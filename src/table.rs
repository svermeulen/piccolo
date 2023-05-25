use std::{
    fmt,
    hash::{Hash, Hasher},
    i64, mem,
};

use gc_arena::{lock::RefLock, Collect, Gc, Mutation};
use hashbrown::raw::RawTable;
use rustc_hash::FxHasher;
use thiserror::Error;

use crate::{IntoValue, Value};

#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Table<'gc>(pub Gc<'gc, RefLock<TableState<'gc>>>);

#[derive(Debug, Copy, Clone, Collect, Error)]
#[collect(require_static)]
pub enum InvalidTableKey {
    #[error("table key is NaN")]
    IsNaN,
    #[error("table key is Nil")]
    IsNil,
}

#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum NextValue<'gc> {
    Found { key: Value<'gc>, value: Value<'gc> },
    Last,
    NotFound,
}

impl<'gc> PartialEq for Table<'gc> {
    fn eq(&self, other: &Table<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl<'gc> Eq for Table<'gc> {}

impl<'gc> Hash for Table<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.as_ptr().hash(state);
    }
}

impl<'gc> Table<'gc> {
    pub fn new(mc: &Mutation<'gc>) -> Table<'gc> {
        Table(Gc::new(mc, RefLock::new(TableState::default())))
    }

    pub fn get<K: IntoValue<'gc>>(&self, mc: &Mutation<'gc>, key: K) -> Value<'gc> {
        self.0.borrow().entries.get(key.into_value(mc))
    }

    pub fn set<K: IntoValue<'gc>, V: IntoValue<'gc>>(
        &self,
        mc: &Mutation<'gc>,
        key: K,
        value: V,
    ) -> Result<Value<'gc>, InvalidTableKey> {
        self.0
            .borrow_mut(mc)
            .entries
            .set(key.into_value(mc), value.into_value(mc))
    }

    pub fn length(&self) -> i64 {
        self.0.borrow().entries.length()
    }

    // Returns the next value after this key in the table order.
    //
    // The table order in the map portion of the table is defined by the incidental order of the
    // internal bucket list. It is unspecified (but safe) to rely on this while inserting into the
    // table.
    //
    // If given Nil, it will return the first pair in the table. If given a key that is present
    // in the table, it will return the next pair in iteration order. If given a key that is not
    // present in the table, the behavior is unspecified.
    pub fn next<K: IntoValue<'gc>>(&self, mc: &Mutation<'gc>, key: K) -> NextValue<'gc> {
        self.0.borrow().entries.next(key.into_value(mc))
    }

    pub fn metatable(&self) -> Option<Table<'gc>> {
        self.0.borrow().metatable
    }

    pub fn set_metatable(
        &self,
        mc: &Mutation<'gc>,
        metatable: Option<Table<'gc>>,
    ) -> Option<Table<'gc>> {
        mem::replace(&mut self.0.borrow_mut(mc).metatable, metatable)
    }
}

#[derive(Debug, Default, Collect)]
#[collect(no_drop)]
pub struct TableState<'gc> {
    pub entries: TableEntries<'gc>,
    pub metatable: Option<Table<'gc>>,
}

#[derive(Default)]
pub struct TableEntries<'gc> {
    array: Vec<Value<'gc>>,
    map: RawTable<(Value<'gc>, Value<'gc>)>,
}

impl<'gc> fmt::Debug for TableEntries<'gc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(
                self.array
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (Value::Integer(i.try_into().unwrap()), *v))
                    .chain(table_iter(&self.map)),
            )
            .finish()
    }
}

unsafe impl<'gc> Collect for TableEntries<'gc> {
    fn trace(&self, cc: &gc_arena::Collection) {
        self.array.trace(cc);
        for (key, value) in table_iter(&self.map) {
            key.trace(cc);
            value.trace(cc);
        }
    }
}

impl<'gc> TableEntries<'gc> {
    pub fn get(&self, key: Value<'gc>) -> Value<'gc> {
        if let Some(index) = to_array_index(key) {
            if index < self.array.len() {
                return self.array[index];
            }
        }

        if let Ok(key) = canonical_key(key) {
            if let Some(&(_, value)) = self.map.get(key_hash(key), |(k, _)| key_eq(key, *k)) {
                value
            } else {
                Value::Nil
            }
        } else {
            Value::Nil
        }
    }

    pub fn set(
        &mut self,
        key: Value<'gc>,
        value: Value<'gc>,
    ) -> Result<Value<'gc>, InvalidTableKey> {
        let index_key = to_array_index(key);
        if let Some(index) = index_key {
            if index < self.array.len() {
                return Ok(mem::replace(&mut self.array[index], value));
            }
        }

        let table_key = canonical_key(key)?;
        let hash = key_hash(table_key);
        if value.is_nil() {
            Ok(table_remove(&mut self.map, hash, table_key).unwrap_or(Value::Nil))
        } else if self.map.len() < self.map.capacity() {
            Ok(table_insert(&mut self.map, hash, table_key, value).unwrap_or(Value::Nil))
        } else {
            // If a new element does not fit in either the array or map part of the table, we need
            // to grow. First, we find the total count of array candidate elements across the array
            // part, the map part, and the newly inserted key.

            const USIZE_BITS: usize = mem::size_of::<usize>() * 8;

            // Count of array-candidate elements based on the highest bit in the index
            let mut array_counts = [0; USIZE_BITS];
            // Total count of all array-candidate elements
            let mut array_total = 0;

            for (i, e) in self.array.iter().enumerate() {
                if !e.is_nil() {
                    array_counts[highest_bit(i)] += 1;
                    array_total += 1;
                }
            }

            for (key, _) in table_iter(&self.map) {
                if let Some(i) = to_array_index(key) {
                    array_counts[highest_bit(i)] += 1;
                    array_total += 1;
                }
            }

            if let Some(i) = index_key {
                array_counts[highest_bit(i)] += 1;
                array_total += 1;
            }

            // Then, we compute the new optimal size for the array by finding the largest array size
            // such that at least half of the elements in the array would be in use.

            let mut optimal_size = 0;
            let mut total = 0;
            for i in 0..USIZE_BITS {
                if (1 << i) / 2 >= array_total {
                    break;
                }

                if array_counts[i] > 0 {
                    total += array_counts[i];
                    if total > (1 << i) / 2 {
                        optimal_size = 1 << i;
                    }
                }
            }

            let old_array_size = self.array.len();
            let old_map_size = self.map.len();
            if optimal_size > old_array_size {
                // If we're growing the array part, we need to grow the array and take any newly
                // valid array keys from the map part.

                self.array.reserve(optimal_size - old_array_size);
                let capacity = self.array.capacity();
                self.array.resize(capacity, Value::Nil);

                let array = &mut self.array;
                table_retain(&mut self.map, |key, value| {
                    if let Some(i) = to_array_index(key) {
                        if i < array.len() {
                            array[i] = value;
                            return false;
                        }
                    }
                    true
                });
            } else {
                // If we aren't growing the array, we're adding a new element to the map that won't
                // fit in the advertised capacity. We explicitly double the map size here.
                self.map.reserve(old_map_size, |(key, _)| key_hash(*key));
            }

            // Now we can insert the new key value pair
            if let Some(index) = index_key {
                if index < self.array.len() {
                    return Ok(mem::replace(&mut self.array[index], value));
                }
            }
            Ok(table_insert(&mut self.map, hash, table_key, value).unwrap_or(Value::Nil))
        }
    }

    /// Returns a 'border' for this table.
    ///
    /// A 'border' for a table is any i >= 0 where:
    /// `(i == 0 or table[i] ~= nil) and table[i + 1] == nil`
    ///
    /// If a table has exactly one border, it is called a 'sequence', and this border is the table's
    /// length.
    pub fn length(&self) -> i64 {
        // Binary search for a border. Entry at max must be Nil, min must be 0 or entry at min must
        // be != Nil.
        fn binary_search<F: Fn(i64) -> bool>(mut min: i64, mut max: i64, is_nil: F) -> i64 {
            while max - min > 1 {
                let mid = min + (max - min) / 2;
                if is_nil(mid) {
                    max = mid;
                } else {
                    min = mid;
                }
            }
            min
        }

        let array_len: i64 = self.array.len().try_into().unwrap();

        if !self.array.is_empty() && self.array[array_len as usize - 1].is_nil() {
            // If the array part ends in a Nil, there must be a border inside it
            binary_search(0, array_len, |i| self.array[i as usize - 1].is_nil())
        } else if self.map.is_empty() {
            // If there is no border in the array but the map part is empty, then the array length
            // is a border
            array_len
        } else {
            // Otherwise, we must check the map part for a border. We need to find some nil value in
            // the map part as the max for a binary search.
            let min = array_len;
            let mut max = array_len.checked_add(1).unwrap();
            while self
                .map
                .find(key_hash(max.into()), |(k, _)| key_eq(max.into(), *k))
                .is_some()
            {
                if max == i64::MAX {
                    // If we can't find a nil entry by doubling, then the table is pathalogical. We
                    // return the favor with a pathalogical answer: i64::MAX + 1 can't exist in the
                    // table, therefore it is Nil, so since the table contains i64::MAX, i64::MAX is
                    // a border.
                    return i64::MAX;
                } else if let Some(double_max) = max.checked_mul(2) {
                    max = double_max;
                } else {
                    max = i64::MAX;
                }
            }

            // We have found a max where table[max] == nil, so we can now binary search
            binary_search(min, max, |i| {
                self.map
                    .find(key_hash(i.into()), |(k, _)| key_eq(i.into(), *k))
                    .is_none()
            })
        }
    }

    pub fn next(&self, key: Value<'gc>) -> NextValue<'gc> {
        let array_result = if let Some(index_key) = to_array_index(key) {
            if index_key < self.array.len() {
                Some((index_key + 1, self.array[index_key].is_nil()))
            } else {
                None
            }
        } else if key.is_nil() {
            // Nil is never considered missing, it is the "key" before the first key.
            Some((0, false))
        } else {
            None
        };

        if let Some((start_index, is_missing)) = array_result {
            for i in start_index..self.array.len() {
                if !self.array[i].is_nil() {
                    return NextValue::Found {
                        key: Value::Integer((i + 1).try_into().unwrap()),
                        value: self.array[i],
                    };
                }
            }

            if is_missing {
                return NextValue::NotFound;
            }

            unsafe {
                for bucket_index in 0..self.map.buckets() {
                    if self.map.is_bucket_full(bucket_index) {
                        let (key, value) = *self.map.bucket(bucket_index).as_ref();
                        return NextValue::Found { key, value };
                    }
                }
            }

            return NextValue::Last;
        }

        if let Ok(table_key) = canonical_key(key) {
            if let Some(bucket) = self
                .map
                .find(key_hash(table_key), |(k, _)| key_eq(*k, table_key))
            {
                unsafe {
                    let bucket_index = self.map.bucket_index(&bucket);
                    for i in bucket_index + 1..self.map.buckets() {
                        if self.map.is_bucket_full(i) {
                            let (key, value) = *self.map.bucket(i).as_ref();
                            return NextValue::Found { key, value };
                        }
                    }
                }
                return NextValue::Last;
            }
        }

        NextValue::NotFound
    }
}

fn table_iter<'a, 'gc>(
    table: &'a RawTable<(Value<'gc>, Value<'gc>)>,
) -> impl Iterator<Item = (Value<'gc>, Value<'gc>)> + 'a {
    unsafe {
        table.iter().map(|bucket| {
            let &(key, value) = bucket.as_ref();
            (key, value)
        })
    }
}

fn table_insert<'gc>(
    table: &mut RawTable<(Value<'gc>, Value<'gc>)>,
    hash: u64,
    key: Value<'gc>,
    value: Value<'gc>,
) -> Option<Value<'gc>> {
    if let Some(bucket) = table.find(hash, |(k, _)| key_eq(*k, key)) {
        Some(mem::replace(unsafe { &mut bucket.as_mut().1 }, value))
    } else {
        table.insert(hash, (key, value), |(k, _)| key_hash(*k));
        None
    }
}

fn table_remove<'gc>(
    table: &mut RawTable<(Value<'gc>, Value<'gc>)>,
    hash: u64,
    key: Value<'gc>,
) -> Option<Value<'gc>> {
    if let Some(bucket) = table.find(hash, |(k, _)| key_eq(*k, key)) {
        unsafe { Some(table.remove(bucket).1) }
    } else {
        None
    }
}

fn table_retain<'gc>(
    table: &mut RawTable<(Value<'gc>, Value<'gc>)>,
    mut f: impl FnMut(Value<'gc>, Value<'gc>) -> bool,
) {
    unsafe {
        for bucket in table.iter() {
            let &(key, value) = bucket.as_ref();
            if !f(key, value) {
                // SAFETY: We do not need to call Rawiter::reflect_remove because we are
                // removing an item that the iterator already yielded.
                table.erase(bucket);
            }
        }
    }
}

fn canonical_key<'gc>(value: Value<'gc>) -> Result<Value<'gc>, InvalidTableKey> {
    match value {
        Value::Nil => Err(InvalidTableKey::IsNil),
        Value::Number(n) => {
            // NaN keys are disallowed, f64 keys where their closest i64 representation is equal
            // to themselves when cast back to f64 are considered integer keys.
            if n.is_nan() {
                Err(InvalidTableKey::IsNaN)
            } else if let Some(i) = f64_to_i64(n) {
                Ok(Value::Integer(i))
            } else {
                Ok(Value::Number(n))
            }
        }
        v => Ok(v),
    }
}

fn key_eq<'gc>(a: Value<'gc>, b: Value<'gc>) -> bool {
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Boolean(a), Value::Boolean(b)) => a == b,
        (Value::Integer(a), Value::Integer(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Table(a), Value::Table(b)) => a == b,
        (Value::Function(a), Value::Function(b)) => a == b,
        (Value::Thread(a), Value::Thread(b)) => a == b,
        (Value::UserData(a), Value::UserData(b)) => a == b,
        _ => false,
    }
}

fn key_hash<'gc>(value: Value<'gc>) -> u64 {
    let mut state = FxHasher::default();
    match value {
        Value::Nil => Hash::hash(&0, &mut state),
        Value::Boolean(b) => {
            Hash::hash(&1, &mut state);
            b.hash(&mut state);
        }
        Value::Integer(i) => {
            Hash::hash(&2, &mut state);
            i.hash(&mut state);
        }
        Value::Number(n) => {
            Hash::hash(&3, &mut state);
            canonical_float_bytes(n).hash(&mut state);
        }
        Value::String(s) => {
            Hash::hash(&4, &mut state);
            s.hash(&mut state);
        }
        Value::Table(t) => {
            Hash::hash(&5, &mut state);
            t.hash(&mut state);
        }
        Value::Function(c) => {
            Hash::hash(&6, &mut state);
            c.hash(&mut state);
        }
        Value::Thread(t) => {
            Hash::hash(&7, &mut state);
            t.hash(&mut state);
        }
        Value::UserData(u) => {
            Hash::hash(&8, &mut state);
            u.hash(&mut state);
        }
    }
    state.finish()
}

// Returns the closest i64 to a given f64 such that casting the i64 back to an f64 results in an
// equal value, if such an integer exists.
fn f64_to_i64(n: f64) -> Option<i64> {
    let i = n as i64;
    if i as f64 == n {
        Some(i)
    } else {
        None
    }
}

// Parameter must not be NaN, should return a bit-pattern which is always equal when the
// corresponding f64s are equal (-0.0 and 0.0 return the same bit pattern).
fn canonical_float_bytes(f: f64) -> u64 {
    assert!(!f.is_nan());
    if f == 0.0 {
        0.0f64.to_bits()
    } else {
        f.to_bits()
    }
}

// If the given key can live in the array part of the table (integral value between 1 and
// usize::MAX), returns the associated array index.
fn to_array_index<'gc>(key: Value<'gc>) -> Option<usize> {
    let i = match key {
        Value::Integer(i) => i,
        Value::Number(f) => f64_to_i64(f)?,
        _ => return None,
    };

    if i > 0 {
        Some(usize::try_from(i).ok()? - 1)
    } else {
        None
    }
}

// Returns the place of the highest set bit in the given i, i = 0 returns 0, i = 1 returns 1, i = 2
// returns 2, i = 3 returns 2, and so on.
fn highest_bit(mut i: usize) -> usize {
    const LOG_2: [u8; 256] = [
        0, 1, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
        5, 5, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
        6, 6, 6, 6, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
        7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
        7, 7, 7, 7, 7, 7, 7, 7, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
        8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
        8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
        8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
        8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
    ];

    let mut hb = 0;
    while i >= 256 {
        hb += 8;
        i >>= 8;
    }

    hb + LOG_2[i] as usize
}
