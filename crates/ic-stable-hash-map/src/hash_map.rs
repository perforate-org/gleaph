//! Stable open-addressing hash map (linear probing, eager removes, resizing).
//!
//! Layout (V1, `SHM` magic, 64-byte header prefix like `ic-stable-structures`):
//!
//! ```text
//! ---------------------------------------- <- Address 0
//! Magic `SHM`            ↕ 3 bytes
//! ----------------------------------------
//! Layout version         ↕ 1 byte
//! ----------------------------------------
//! Number of entries = L  ↕ 8 bytes
//! ----------------------------------------
//! Capacity               ↕ 8 bytes
//! ----------------------------------------
//! Key size (K::SIZE)     ↕ 4 bytes
//! ----------------------------------------
//! Value size (V::SIZE)  ↕ 4 bytes
//! ----------------------------------------
//! Reserved space         ↕ 36 bytes
//! ---------------------------------------- <- Address 64
//! KEYS: [flag(1B) + K; CAPACITY]
//! ----------------------------------------
//! VALUES: [V; CAPACITY]
//! ----------------------------------------
//! Unallocated space
//! ```
//!
//! `flag` is `0` (empty) or `255` (occupied). Open addressing with linear probing; deletes use the
//! classic **backward-shift** eager remove (no tombstones, so performance does not degrade under
//! churn). Resize at a 3/4 load factor rehashes all entries into a `2*cap - 1` table (the old table
//! is read into a transient heap buffer, the region is cleared, and entries are rehashed in place).
//!
//! `K` and `V` must be **fixed-size** [`Storable`](ic_stable_structures::Storable) so the slots are
//! fixed-width. Hashing uses `rapidhash(data)` (deterministic constant seed, stable across upgrades).

use crate::header::{self, DATA_OFFSET, InitError};
use crate::iter::Iter;
use crate::memory::{grow_memory_to_at_least_bytes, read_u32, read_u64, write_u64};
use ic_stable_structures::{Memory, Storable};
use rapidhash::v1::rapidhash_v1;
use std::borrow::Cow;
use std::marker::PhantomData;

const EMPTY: u8 = 0;
const OCCUPIED: u8 = 255;
const DEFAULT_CAPACITY: u64 = 7;

/// Failure inserting into a [`StableHashMap`].
#[derive(Debug, PartialEq, Eq)]
pub enum InsertError {
    /// Stable memory grow failed while resizing the table.
    OutOfMemory,
}

impl std::fmt::Display for InsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfMemory => write!(f, "out of stable memory while growing the hash map"),
        }
    }
}

impl std::error::Error for InsertError {}

/// Number of bytes for the KEYS array (flag + key per slot).
fn keys_bytes(capacity: u64, key_size: u32) -> u64 {
    (1 + key_size as u64) * capacity
}

/// Byte offset of the VALUES array relative to [`DATA_OFFSET`].
fn values_offset(capacity: u64, key_size: u32) -> u64 {
    keys_bytes(capacity, key_size)
}

/// Total table size in bytes.
fn table_size(capacity: u64, key_size: u32, value_size: u32) -> u64 {
    values_offset(capacity, key_size) + value_size as u64 * capacity
}

/// Byte offset of a slot's flag byte.
fn key_flag_offset(idx: u64, key_size: u32) -> u64 {
    DATA_OFFSET + (1 + key_size as u64) * idx
}

/// Byte offset of a slot's key data.
fn key_data_offset(idx: u64, key_size: u32) -> u64 {
    key_flag_offset(idx, key_size) + 1
}

/// Byte offset of a slot's value.
fn value_offset(idx: u64, key_size: u32, value_size: u32, capacity: u64) -> u64 {
    DATA_OFFSET + values_offset(capacity, key_size) + value_size as u64 * idx
}

/// Deterministic hash of the key bytes, mapped into `[0, capacity)`.
fn hash(key_bytes: &[u8], capacity: u64) -> u64 {
    rapidhash_v1(key_bytes) % capacity
}

/// Stable open-addressing hash map over a [`Memory`] region.
///
/// `K`/`V` must be fixed-size [`Storable`](ic_stable_structures::Storable). All mutations use `&self`
/// via [`Memory`](ic_stable_structures::Memory); avoid aliasing the same byte range with another
/// mutating wrapper while an iterator is alive.
pub struct StableHashMap<K: Storable, V: Storable, M: Memory> {
    memory: M,
    _marker: PhantomData<(K, V)>,
}

impl<K: Storable + PartialEq, V: Storable, M: Memory> StableHashMap<K, V, M> {
    /// Opens an existing hash map, failing closed on a corrupt/unsupported layout.
    pub fn init(memory: M) -> Result<Self, InitError> {
        let header = header::read_header(&memory)?;
        if header.key_size != K::BOUND.max_size() || header.value_size != V::BOUND.max_size() {
            return Err(InitError::IncompatibleElementType);
        }
        if !K::BOUND.is_fixed_size() || !V::BOUND.is_fixed_size() {
            return Err(InitError::IncompatibleElementType);
        }
        let expected =
            DATA_OFFSET + table_size(header.capacity, header.key_size, header.value_size);
        if memory.size() * 65536 < expected {
            return Err(InitError::InvalidLayout);
        }
        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    /// Creates a fresh hash map, overwriting any existing layout in the region.
    pub fn new(memory: M) -> Result<Self, InitError> {
        let key_size = K::BOUND.max_size();
        let value_size = V::BOUND.max_size();
        if !K::BOUND.is_fixed_size() || !V::BOUND.is_fixed_size() {
            return Err(InitError::IncompatibleElementType);
        }
        // Grow first so the header and table writes are in bounds.
        let size = DATA_OFFSET + table_size(DEFAULT_CAPACITY, key_size, value_size);
        grow_memory_to_at_least_bytes(&memory, size).map_err(|_| InitError::OutOfMemory)?;
        header::write_header(&memory, DEFAULT_CAPACITY, key_size, value_size);
        let zeroed = vec![0u8; (size - DATA_OFFSET) as usize];
        memory.write(DATA_OFFSET, &zeroed);
        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    /// Returns the number of entries.
    pub fn len(&self) -> u64 {
        read_u64(&self.memory, header::LEN_OFFSET)
    }

    /// Returns the table capacity.
    pub fn capacity(&self) -> u64 {
        read_u64(&self.memory, header::CAP_OFFSET)
    }

    /// Returns `true` when the map is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` when the next unique-key insert triggers a resize.
    fn is_full(&self) -> bool {
        self.len() == (self.capacity() >> 2) * 3
    }

    fn key_size(&self) -> u32 {
        read_u32(&self.memory, header::KEY_SIZE_OFFSET)
    }

    fn value_size(&self) -> u32 {
        read_u32(&self.memory, header::VALUE_SIZE_OFFSET)
    }

    fn read_flag(&self, idx: u64) -> u8 {
        let mut buf = [0u8; 1];
        self.memory
            .read(key_flag_offset(idx, self.key_size()), &mut buf);
        buf[0]
    }

    fn write_flag(&self, idx: u64, flag: u8) {
        self.memory
            .write(key_flag_offset(idx, self.key_size()), &[flag]);
    }

    pub(crate) fn read_key(&self, idx: u64) -> K {
        let offset = key_data_offset(idx, self.key_size());
        let mut buf = vec![0u8; self.key_size() as usize];
        self.memory.read(offset, &mut buf);
        K::from_bytes(Cow::Owned(buf))
    }

    fn write_key(&self, idx: u64, key: &K) {
        let offset = key_data_offset(idx, self.key_size());
        self.memory.write(offset, &key.to_bytes());
    }

    pub(crate) fn read_value(&self, idx: u64) -> V {
        let offset = value_offset(idx, self.key_size(), self.value_size(), self.capacity());
        let mut buf = vec![0u8; self.value_size() as usize];
        self.memory.read(offset, &mut buf);
        V::from_bytes(Cow::Owned(buf))
    }

    fn write_value(&self, idx: u64, value: &V) {
        let offset = value_offset(idx, self.key_size(), self.value_size(), self.capacity());
        self.memory.write(offset, &value.to_bytes());
    }

    fn set_len(&self, len: u64) {
        write_u64(&self.memory, header::LEN_OFFSET, len);
    }

    /// Returns `true` if the slot at `idx` is occupied.
    pub(crate) fn is_occupied(&self, idx: u64) -> bool {
        self.read_flag(idx) == OCCUPIED
    }

    /// Returns the value stored by `key`, if any.
    pub fn get(&self, key: &K) -> Option<V> {
        let idx = self.find_inner_idx(key)?;
        Some(self.read_value(idx))
    }

    /// Returns `true` if `key` is present.
    pub fn contains_key(&self, key: &K) -> bool {
        self.find_inner_idx(key).is_some()
    }

    /// Returns an iterator over the map's `(key, value)` entries (unordered slot order).
    pub fn iter(&self) -> Iter<'_, K, V, M> {
        Iter::new(self)
    }

    /// Inserts `key`/`value`, returning the previous value if the key was present.
    ///
    /// A single linear probe handles both cases: an existing key is replaced in place (no resize), and
    /// a new key is inserted at the first empty slot (resizing first if the map is full).
    pub fn insert(&self, key: K, value: V) -> Result<Option<V>, InsertError> {
        let capacity = self.capacity();
        let key_bytes = key.to_bytes();
        let mut i = hash(&key_bytes, capacity);
        loop {
            match self.read_flag(i) {
                OCCUPIED => {
                    if self.read_key(i) == key {
                        let prev = self.read_value(i);
                        self.write_value(i, &value);
                        return Ok(Some(prev));
                    }
                    i = (i + 1) % capacity;
                }
                EMPTY => {
                    if self.is_full() {
                        self.resize()?;
                        return Ok(self.insert_internal(key, value));
                    }
                    self.write_key(i, &key);
                    self.write_value(i, &value);
                    self.write_flag(i, OCCUPIED);
                    self.set_len(self.len() + 1);
                    return Ok(None);
                }
                _ => unreachable!("invalid slot flag"),
            }
        }
    }

    /// Removes `key`, returning the previous value if present.
    pub fn remove(&self, key: &K) -> Option<V> {
        let idx = self.find_inner_idx(key)?;
        let prev = self.read_value(idx);
        self.remove_by_idx(idx);
        Some(prev)
    }

    /// Probes for `key`, returning its slot index if present.
    fn find_inner_idx(&self, key: &K) -> Option<u64> {
        if self.is_empty() {
            return None;
        }
        let capacity = self.capacity();
        let key_bytes = key.to_bytes();
        let mut i = hash(&key_bytes, capacity);
        loop {
            match self.read_flag(i) {
                EMPTY => return None,
                OCCUPIED => {
                    if self.read_key(i) == *key {
                        return Some(i);
                    }
                    i = (i + 1) % capacity;
                }
                _ => unreachable!("invalid slot flag"),
            }
        }
    }

    /// Inserts without a resize check (used by `insert` and the resize rehash).
    fn insert_internal(&self, key: K, value: V) -> Option<V> {
        let capacity = self.capacity();
        let key_bytes = key.to_bytes();
        let mut i = hash(&key_bytes, capacity);
        loop {
            match self.read_flag(i) {
                OCCUPIED => {
                    if self.read_key(i) == key {
                        let prev = self.read_value(i);
                        self.write_value(i, &value);
                        return Some(prev);
                    }
                    i = (i + 1) % capacity;
                }
                EMPTY => {
                    self.write_key(i, &key);
                    self.write_value(i, &value);
                    self.write_flag(i, OCCUPIED);
                    self.set_len(self.len() + 1);
                    return None;
                }
                _ => unreachable!("invalid slot flag"),
            }
        }
    }

    /// Eager remove: backward-shift subsequent keys to fill the hole (no tombstones).
    fn remove_by_idx(&self, idx: u64) {
        let capacity = self.capacity();
        let mut i = idx;
        let mut j = idx;
        loop {
            j = (j + 1) % capacity;
            if j == idx {
                break;
            }
            if self.read_flag(j) == OCCUPIED {
                let next_key = self.read_key(j);
                let k = hash(&next_key.to_bytes(), capacity);
                if (j < i) ^ (k <= i) ^ (k > j) {
                    self.write_key(i, &next_key);
                    self.write_value(i, &self.read_value(j));
                    i = j;
                }
                continue;
            }
            break;
        }
        self.write_flag(i, EMPTY);
        self.set_len(self.len() - 1);
    }

    /// Grows the table to `2*cap - 1` and rehashes all entries.
    fn resize(&self) -> Result<(), InsertError> {
        let capacity = self.capacity();
        let key_size = self.key_size();
        let value_size = self.value_size();
        let new_capacity = capacity * 2 - 1;

        // Read all entries into a transient heap buffer (the new table overlaps the old).
        let mut entries: Vec<(K, V)> = Vec::with_capacity(self.len() as usize);
        for i in 0..capacity {
            if self.read_flag(i) == OCCUPIED {
                entries.push((self.read_key(i), self.read_value(i)));
            }
        }

        let new_size = DATA_OFFSET + table_size(new_capacity, key_size, value_size);
        grow_memory_to_at_least_bytes(&self.memory, new_size)
            .map_err(|_| InsertError::OutOfMemory)?;

        // Clear the whole table region, then rehash.
        let zeroed = vec![0u8; (new_size - DATA_OFFSET) as usize];
        self.memory.write(DATA_OFFSET, &zeroed);
        write_u64(&self.memory, header::CAP_OFFSET, new_capacity);
        write_u64(&self.memory, header::LEN_OFFSET, 0);
        for (k, v) in entries {
            self.insert_internal(k, v);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;
    use std::collections::HashMap;

    fn fresh() -> StableHashMap<u64, u64, VectorMemory> {
        StableHashMap::new(VectorMemory::default()).expect("new")
    }

    #[test]
    fn insert_get_remove_roundtrip() {
        let map = fresh();
        assert!(map.is_empty());
        assert_eq!(map.insert(1, 10).unwrap(), None);
        assert_eq!(map.insert(2, 20).unwrap(), None);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&1), Some(10));
        assert_eq!(map.get(&2), Some(20));
        assert_eq!(map.get(&3), None);
        // Replace returns the previous value.
        assert_eq!(map.insert(1, 11).unwrap(), Some(10));
        assert_eq!(map.get(&1), Some(11));
        // Remove returns the previous value.
        assert_eq!(map.remove(&1), Some(11));
        assert_eq!(map.get(&1), None);
        assert_eq!(map.len(), 1);
        assert_eq!(map.remove(&1), None);
    }

    #[test]
    fn collision_handling() {
        let map = fresh();
        // Keys that collide under the hash (same low bits) still resolve correctly via probing.
        for k in 0..50u64 {
            map.insert(k, k * 100).unwrap();
        }
        for k in 0..50u64 {
            assert_eq!(map.get(&k), Some(k * 100));
        }
        for k in 0..50u64 {
            assert_eq!(map.remove(&k), Some(k * 100));
        }
        assert!(map.is_empty());
    }

    #[test]
    fn eager_remove_keeps_all_remaining_entries() {
        let map = fresh();
        for k in 0..200u64 {
            map.insert(k, k).unwrap();
        }
        // Remove every other key; the backward-shift must not lose any remaining entry.
        for k in (0..200u64).step_by(2) {
            assert_eq!(map.remove(&k), Some(k));
        }
        for k in 0..200u64 {
            if k % 2 == 0 {
                assert_eq!(map.get(&k), None);
            } else {
                assert_eq!(map.get(&k), Some(k));
            }
        }
    }

    #[test]
    fn resize_rehash_preserves_all_entries() {
        let map = fresh();
        // Insert enough to trigger several resizes (default capacity 7, 3/4 load factor).
        for k in 0..1000u64 {
            map.insert(k, k * 7).unwrap();
        }
        assert!(map.capacity() > 7);
        for k in 0..1000u64 {
            assert_eq!(map.get(&k), Some(k * 7));
        }
        assert_eq!(map.len(), 1000);
    }

    #[test]
    fn iter_yields_all_entries() {
        let map = fresh();
        for k in 0..100u64 {
            map.insert(k, k * 3).unwrap();
        }
        let mut collected: Vec<(u64, u64)> = map.iter().collect();
        collected.sort();
        assert_eq!(collected.len(), 100);
        for (i, (k, v)) in collected.iter().enumerate() {
            assert_eq!(*k, i as u64);
            assert_eq!(*v, (i as u64) * 3);
        }
    }

    #[test]
    fn upgrade_persistence_reopens() {
        let memory = VectorMemory::default();
        {
            let map = StableHashMap::<u64, u64, _>::new(memory.clone()).expect("new");
            for k in 0..100u64 {
                map.insert(k, k + 1).unwrap();
            }
        }
        // Reopen (simulating a canister upgrade): the entries survive.
        let map = StableHashMap::<u64, u64, _>::init(memory).expect("init");
        assert_eq!(map.len(), 100);
        for k in 0..100u64 {
            assert_eq!(map.get(&k), Some(k + 1));
        }
    }

    #[test]
    fn fuzz_insert_remove_matches_reference() {
        let map = fresh();
        let mut reference: HashMap<u64, u64> = HashMap::new();
        let mut rng = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            rng >> 33
        };
        for _ in 0..5000 {
            let key = next() % 500;
            if next() % 2 == 0 {
                let value = next();
                let prev = map.insert(key, value).unwrap();
                assert_eq!(prev, reference.insert(key, value));
            } else {
                let prev = map.remove(&key);
                assert_eq!(prev, reference.remove(&key));
            }
        }
        assert_eq!(map.len(), reference.len() as u64);
        for (k, v) in &reference {
            assert_eq!(map.get(k), Some(*v));
        }
    }
}
