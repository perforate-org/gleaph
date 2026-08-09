//! Stable Clustered Hashing (Amble & Knuth 1974, "Ordered Hash Tables"): a flattened chained hash
//! table where items of the same bucket are clustered together in the table.
//!
//! Layout (V1, `CHM` magic, 64-byte header prefix like `ic-stable-structures`):
//!
//! ```text
//! ---------------------------------------- <- Address 0
//! Magic `CHM`            ↕ 3 bytes
//! ----------------------------------------
//! Layout version         ↕ 1 byte
//! ----------------------------------------
//! Number of entries = L  ↕ 8 bytes
//! ----------------------------------------
//! log2_buckets = N       ↕ 1 byte
//! ----------------------------------------
//! Key size (K::SIZE)     ↕ 4 bytes
//! ----------------------------------------
//! Value size (V::SIZE)   ↕ 4 bytes
//! ----------------------------------------
//! Reserved space         ↕ 43 bytes
//! ---------------------------------------- <- Address 64
//! Entries: [K + V + distance(u16); capacity]
//!   capacity = 2^N + N   (overflow area = N)
//! ----------------------------------------
//! Unallocated space
//! ```
//!
//! `distance == u16::MAX` marks an empty slot (real distances are bounded by the overflow area `N`,
//! far below `u16::MAX`). Buckets are `lower N bits of (rapidhash(key) * 2^64/phi)` (Fibonacci
//! hashing). The hash is **not stored** (saves 8B/entry); lookup compares keys directly and remap
//! recomputes `rapidhash`.
//!
//! `K` and `V` must be **fixed-size** [`Storable`](ic_stable_structures::Storable). All mutations use
//! `&self` via [`Memory`](ic_stable_structures::Memory).

use crate::header::{self, DATA_OFFSET, InitError};
use crate::iter::Iter;
use crate::memory::{
    grow_memory_to_at_least_bytes, read_u8, read_u32, read_u64, write_u8, write_u64,
};
use ic_stable_structures::{Memory, Storable};
use rapidhash::v1::rapidhash_v1;
use std::borrow::Cow;
use std::marker::PhantomData;

/// Empty marker: a real distance is bounded by the overflow area `N`, so `u16::MAX` is never a real
/// distance.
const EMPTY: u16 = u16::MAX;
/// `2^64 / phi` (golden ratio), the Fibonacci hashing multiplier.
const FIB_CONST: u64 = 11400714819323198485;
/// Initial `log2_buckets`: 2^3 = 8 buckets, capacity = 8 + 3 = 11.
const DEFAULT_LOG2_BUCKETS: u8 = 3;

/// Failure inserting into a [`StableClusteredHashMap`].
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

/// Fibonacci hashing: maps `hash` to the lower `n` bits of `hash * 2^64/phi`.
fn bucket(hash: u64, n: u8) -> u64 {
    if n == 0 {
        return 0;
    }
    let m = 64 - n;
    let fib = hash.wrapping_mul(FIB_CONST);
    (fib << m) >> m
}

/// An in-memory entry used during insert relocation.
struct Entry<K, V> {
    key: K,
    value: V,
    distance: u16,
}

/// Stable clustered hash map over a [`Memory`] region.
///
/// `K`/`V` must be fixed-size [`Storable`](ic_stable_structures::Storable). All mutations use `&self`
/// via [`Memory`](ic_stable_structures::Memory); avoid aliasing the same byte range with another
/// mutating wrapper while an iterator is alive.
pub struct StableClusteredHashMap<K: Storable, V: Storable, M: Memory> {
    memory: M,
    _marker: PhantomData<(K, V)>,
}

impl<K: Storable + PartialEq, V: Storable, M: Memory> StableClusteredHashMap<K, V, M> {
    /// Opens an existing hash map, failing closed on a corrupt/unsupported layout.
    pub fn init(memory: M) -> Result<Self, InitError> {
        let header = header::read_header(&memory)?;
        if header.key_size != K::BOUND.max_size() || header.value_size != V::BOUND.max_size() {
            return Err(InitError::IncompatibleElementType);
        }
        if !K::BOUND.is_fixed_size() || !V::BOUND.is_fixed_size() {
            return Err(InitError::IncompatibleElementType);
        }
        let capacity = (1u64 << header.log2_buckets) + header.log2_buckets as u64;
        let expected =
            DATA_OFFSET + capacity * (header.key_size as u64 + header.value_size as u64 + 2);
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
        let n = DEFAULT_LOG2_BUCKETS;
        let capacity = (1u64 << n) + n as u64;
        let size = DATA_OFFSET + capacity * (key_size as u64 + value_size as u64 + 2);
        grow_memory_to_at_least_bytes(&memory, size).map_err(|_| InitError::OutOfMemory)?;
        header::write_header(&memory, n, key_size, value_size);
        Self::clear_table(&memory, capacity, key_size, value_size);
        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    /// Returns the number of entries.
    pub fn len(&self) -> u64 {
        read_u64(&self.memory, header::LEN_OFFSET)
    }

    /// Returns the advertised bucket count (`2^N`).
    pub fn buckets(&self) -> u64 {
        1u64 << self.log2_buckets()
    }

    /// Returns the internal table capacity (`2^N + N`, including the overflow area).
    pub fn capacity(&self) -> u64 {
        let n = self.log2_buckets();
        (1u64 << n) + n as u64
    }

    /// Returns `true` when the map is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` when the next unique-key insert triggers a resize.
    fn is_full(&self) -> bool {
        self.len() >= (3 * self.buckets()) / 4
    }

    fn log2_buckets(&self) -> u8 {
        read_u8(&self.memory, header::LOG2_BUCKETS_OFFSET)
    }

    fn key_size(&self) -> u32 {
        read_u32(&self.memory, header::KEY_SIZE_OFFSET)
    }

    fn value_size(&self) -> u32 {
        read_u32(&self.memory, header::VALUE_SIZE_OFFSET)
    }

    fn entry_stride(&self) -> u64 {
        self.key_size() as u64 + self.value_size() as u64 + 2
    }

    fn entry_offset(&self, i: u64) -> u64 {
        DATA_OFFSET + i * self.entry_stride()
    }

    fn key_offset(&self, i: u64) -> u64 {
        self.entry_offset(i)
    }

    fn value_offset(&self, i: u64) -> u64 {
        self.entry_offset(i) + self.key_size() as u64
    }

    fn distance_offset(&self, i: u64) -> u64 {
        self.entry_offset(i) + self.key_size() as u64 + self.value_size() as u64
    }

    fn read_distance(&self, i: u64) -> u16 {
        let mut buf = [0u8; 2];
        self.memory.read(self.distance_offset(i), &mut buf);
        u16::from_le_bytes(buf)
    }

    fn write_distance(&self, i: u64, d: u16) {
        self.memory.write(self.distance_offset(i), &d.to_le_bytes());
    }

    pub(crate) fn read_key(&self, i: u64) -> K {
        let offset = self.key_offset(i);
        let size = self.key_size() as usize;
        if size <= 64 {
            let mut buf = [0u8; 64];
            self.memory.read(offset, &mut buf[..size]);
            K::from_bytes(Cow::Borrowed(&buf[..size]))
        } else {
            let mut buf = vec![0u8; size];
            self.memory.read(offset, &mut buf);
            K::from_bytes(Cow::Owned(buf))
        }
    }

    fn write_key(&self, i: u64, key: &K) {
        self.memory.write(self.key_offset(i), &key.to_bytes());
    }

    pub(crate) fn read_value(&self, i: u64) -> V {
        let offset = self.value_offset(i);
        let size = self.value_size() as usize;
        if size <= 64 {
            let mut buf = [0u8; 64];
            self.memory.read(offset, &mut buf[..size]);
            V::from_bytes(Cow::Borrowed(&buf[..size]))
        } else {
            let mut buf = vec![0u8; size];
            self.memory.read(offset, &mut buf);
            V::from_bytes(Cow::Owned(buf))
        }
    }

    fn write_value(&self, i: u64, value: &V) {
        self.memory.write(self.value_offset(i), &value.to_bytes());
    }

    fn read_entry(&self, i: u64) -> Entry<K, V> {
        Entry {
            key: self.read_key(i),
            value: self.read_value(i),
            distance: self.read_distance(i),
        }
    }

    fn write_entry(&self, i: u64, entry: &Entry<K, V>) {
        self.write_key(i, &entry.key);
        self.write_value(i, &entry.value);
        self.write_distance(i, entry.distance);
    }

    fn set_len(&self, len: u64) {
        write_u64(&self.memory, header::LEN_OFFSET, len);
    }

    /// Returns `true` if the slot at `i` is empty.
    pub(crate) fn is_empty_slot(&self, i: u64) -> bool {
        self.read_distance(i) == EMPTY
    }

    /// The bucket of the entry at `i`, derived from its position and distance.
    fn bucket_by_position(&self, i: u64) -> u64 {
        i - self.read_distance(i) as u64
    }

    /// The end (tail + 1) of the cluster containing `position`.
    fn end_of_cluster_by_position(&self, position: u64) -> u64 {
        let bucket = self.bucket_by_position(position);
        let mut i = position;
        while i < self.capacity() {
            let dist = self.read_distance(i);
            if dist == EMPTY || i - dist as u64 != bucket {
                break;
            }
            i += 1;
        }
        i
    }

    /// The tail of the cluster containing `position`.
    fn tail_of_cluster_by_position(&self, position: u64) -> u64 {
        self.end_of_cluster_by_position(position) - 1
    }

    /// The position where a new entry with `bucket` should be inserted: the end of the cluster of
    /// `bucket` (or the end of the previous cluster if `bucket`'s cluster does not exist).
    fn find_insert_position(&self, bucket: u64) -> u64 {
        let mut i = bucket;
        while i < self.capacity() {
            let dist = self.read_distance(i);
            if dist == EMPTY || i - dist as u64 > bucket {
                break;
            }
            i += 1;
        }
        i
    }

    /// Probes for `key`, returning `(found_index, insert_position, bucket)`. When the key is absent,
    /// `insert_position` is where a new entry with `bucket` should be inserted (the end of the
    /// cluster of `bucket`, or the end of the previous cluster), so `insert` avoids a second search.
    fn lookup_index(&self, key: &K) -> (Option<u64>, u64, u64) {
        let n = self.log2_buckets();
        let hash = rapidhash_v1(&key.to_bytes());
        let b = bucket(hash, n);
        if self.is_empty() {
            // Empty map: the first entry lands at its bucket with distance 0.
            return (None, b, b);
        }
        let mut i = b;
        while i < self.capacity() {
            let dist = self.read_distance(i);
            if dist == EMPTY {
                break;
            }
            let bucket_i = i - dist as u64;
            if bucket_i > b {
                break;
            }
            if bucket_i == b && self.read_key(i) == *key {
                return (Some(i), i, b);
            }
            i += 1;
        }
        (None, i, b)
    }

    /// Returns the value stored by `key`, if any.
    pub fn get(&self, key: &K) -> Option<V> {
        let (idx, _, _) = self.lookup_index(key);
        let idx = idx?;
        Some(self.read_value(idx))
    }

    /// Returns `true` if `key` is present.
    pub fn contains_key(&self, key: &K) -> bool {
        self.lookup_index(key).0.is_some()
    }

    /// Returns an iterator over the map's `(key, value)` entries (unordered slot order).
    pub fn iter(&self) -> Iter<'_, K, V, M> {
        Iter::new(self)
    }

    /// Inserts `key`/`value`, returning the previous value if the key was present.
    pub fn insert(&self, key: K, value: V) -> Result<Option<V>, InsertError> {
        let (found, insert_position, b) = self.lookup_index(&key);
        if let Some(idx) = found {
            let prev = self.read_value(idx);
            self.write_value(idx, &value);
            return Ok(Some(prev));
        }
        if self.is_full() {
            self.resize()?;
            let n = self.log2_buckets();
            let b = bucket(rapidhash_v1(&key.to_bytes()), n);
            let insert_position = self.find_insert_position(b);
            let entry = Entry {
                key,
                value,
                distance: (insert_position - b) as u16,
            };
            self.insert_and_relocate(entry, insert_position)?;
            self.set_len(self.len() + 1);
            return Ok(None);
        }
        let entry = Entry {
            key,
            value,
            distance: (insert_position - b) as u16,
        };
        self.insert_and_relocate(entry, insert_position)?;
        self.set_len(self.len() + 1);
        Ok(None)
    }

    /// Inserts without a resize trigger (used by the resize rehash).
    fn insert_internal(&self, key: K, value: V) {
        let n = self.log2_buckets();
        let b = bucket(rapidhash_v1(&key.to_bytes()), n);
        let position = self.find_insert_position(b);
        let entry = Entry {
            key,
            value,
            distance: (position - b) as u16,
        };
        // During rehash the new table is 2x, so the overflow area never fills; unwrap is safe.
        self.insert_and_relocate(entry, position)
            .expect("rehash insert cannot overflow");
        self.set_len(self.len() + 1);
    }

    /// Core insert with relocation: append to the cluster tail, swapping the displaced head of the
    /// next cluster and relocating it. Resizes (full rehash) if the overflow area fills.
    fn insert_and_relocate(
        &self,
        mut entry: Entry<K, V>,
        mut position: u64,
    ) -> Result<(), InsertError> {
        loop {
            if position >= self.capacity() {
                self.resize()?;
                let n = self.log2_buckets();
                let b = bucket(rapidhash_v1(&entry.key.to_bytes()), n);
                position = self.find_insert_position(b);
                continue;
            }
            if self.is_empty_slot(position) {
                self.write_entry(position, &entry);
                return Ok(());
            }
            let t = self.read_entry(position);
            let next = self.end_of_cluster_by_position(position);
            let mut t = t;
            t.distance = (t.distance as u64 + (next - position)) as u16;
            self.write_entry(position, &entry);
            entry = t;
            position = next;
        }
    }

    /// Removes `key`, returning the previous value if present.
    pub fn remove(&self, key: &K) -> Option<V> {
        let (idx, _, _) = self.lookup_index(key);
        let idx = idx?;
        let prev = self.read_value(idx);
        self.remove_and_relocate(idx);
        self.set_len(self.len() - 1);
        Some(prev)
    }

    /// Eager remove: empty the slot and fill the gap by moving the tail of the next cluster up
    /// (no tombstones, so performance does not degrade under churn).
    fn remove_and_relocate(&self, mut position: u64) {
        loop {
            if position == self.capacity() - 1 {
                self.write_distance(position, EMPTY);
                return;
            }
            let next_dist = self.read_distance(position + 1);
            if next_dist == EMPTY || next_dist == 0 {
                self.write_distance(position, EMPTY);
                return;
            }
            let next = self.tail_of_cluster_by_position(position + 1);
            let mut tail = self.read_entry(next);
            tail.distance = (tail.distance as u64 - (next - position)) as u16;
            self.write_entry(position, &tail);
            position = next;
        }
    }

    /// Grows the table to `2^(N+1) + (N+1)` and rehashes all entries (full rehash; the in-place
    /// incremental resize is a later slice).
    fn resize(&self) -> Result<(), InsertError> {
        let n = self.log2_buckets();
        let new_n = n + 1;
        let new_capacity = (1u64 << new_n) + new_n as u64;
        let key_size = self.key_size();
        let value_size = self.value_size();

        // Read all entries into a transient heap buffer (the new table overlaps the old).
        let mut entries: Vec<(K, V)> = Vec::with_capacity(self.len() as usize);
        for i in 0..self.capacity() {
            if !self.is_empty_slot(i) {
                entries.push((self.read_key(i), self.read_value(i)));
            }
        }

        let new_size = DATA_OFFSET + new_capacity * (key_size as u64 + value_size as u64 + 2);
        grow_memory_to_at_least_bytes(&self.memory, new_size)
            .map_err(|_| InsertError::OutOfMemory)?;

        Self::clear_table(&self.memory, new_capacity, key_size, value_size);
        write_u8(&self.memory, header::LOG2_BUCKETS_OFFSET, new_n);
        write_u64(&self.memory, header::LEN_OFFSET, 0);
        for (k, v) in entries {
            self.insert_internal(k, v);
        }
        Ok(())
    }

    /// Sets every slot's distance to [`EMPTY`] (the key/value bytes are left as garbage; they are
    /// only read when the slot is occupied). Writes the table in chunks so the resize clear is a few
    /// large writes instead of one 2-byte write per slot.
    fn clear_table<M2: Memory>(memory: &M2, capacity: u64, key_size: u32, value_size: u32) {
        const CHUNK: u64 = 64;
        let stride = key_size as u64 + value_size as u64 + 2;
        let dist_offset = key_size as u64 + value_size as u64;
        // Build one chunk of `CHUNK` entries: [0; key] [0; value] [EMPTY] repeated.
        let mut chunk = vec![0u8; (CHUNK * stride) as usize];
        for i in 0..CHUNK {
            let e = (i * stride + dist_offset) as usize;
            chunk[e..e + 2].copy_from_slice(&EMPTY.to_le_bytes());
        }
        let mut i = 0;
        while i < capacity {
            let n = (capacity - i).min(CHUNK);
            memory.write(DATA_OFFSET + i * stride, &chunk[..(n * stride) as usize]);
            i += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    fn fresh() -> StableClusteredHashMap<u64, u64, VectorMemory> {
        StableClusteredHashMap::new(VectorMemory::default()).expect("new")
    }

    #[test]
    fn insert_get_remove_roundtrip() {
        let map = fresh();
        assert!(map.insert(1, 10).unwrap().is_none());
        assert!(map.insert(2, 20).unwrap().is_none());
        assert_eq!(map.get(&1), Some(10));
        assert_eq!(map.get(&2), Some(20));
        assert_eq!(map.len(), 2);
        assert_eq!(map.remove(&1), Some(10));
        assert_eq!(map.get(&1), None);
        assert_eq!(map.len(), 1);
        assert_eq!(map.remove(&2), Some(20));
        assert!(map.is_empty());
    }

    #[test]
    fn insert_overwrites_existing() {
        let map = fresh();
        map.insert(7, 1).unwrap();
        assert_eq!(map.insert(7, 2).unwrap(), Some(1));
        assert_eq!(map.get(&7), Some(2));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn collision_cluster_handling() {
        // Force many keys into the same bucket by using a tiny table and keys that collide.
        let map = fresh();
        for k in 0..50u64 {
            map.insert(k, k * 10).unwrap();
        }
        for k in 0..50u64 {
            assert_eq!(map.get(&k), Some(k * 10), "key {k} survives clustering");
        }
        assert_eq!(map.len(), 50);
    }

    #[test]
    fn remove_fills_gap_and_preserves_others() {
        let map = fresh();
        for k in 0..30u64 {
            map.insert(k, k).unwrap();
        }
        // Remove a middle key; all others must remain reachable.
        map.remove(&15);
        assert_eq!(map.get(&15), None);
        for k in 0..30u64 {
            if k != 15 {
                assert_eq!(map.get(&k), Some(k), "key {k} survives a middle remove");
            }
        }
        assert_eq!(map.len(), 29);
    }

    #[test]
    fn resize_preserves_all_entries() {
        let map = fresh();
        for k in 0..200u64 {
            map.insert(k, k * 3).unwrap();
        }
        assert!(map.buckets() > 8, "resized beyond the initial 8 buckets");
        for k in 0..200u64 {
            assert_eq!(map.get(&k), Some(k * 3), "key {k} survives resize");
        }
        assert_eq!(map.len(), 200);
    }

    #[test]
    fn iter_yields_all_entries() {
        let map = fresh();
        for k in 0..100u64 {
            map.insert(k, k).unwrap();
        }
        let mut seen: Vec<(u64, u64)> = map.iter().collect();
        seen.sort();
        let expected: Vec<(u64, u64)> = (0..100).map(|k| (k, k)).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn upgrade_persistence_reopens() {
        let mm = VectorMemory::default();
        {
            let map = StableClusteredHashMap::<u64, u64, _>::new(mm.clone()).expect("new");
            for k in 0..50u64 {
                map.insert(k, k).unwrap();
            }
        }
        let map = StableClusteredHashMap::<u64, u64, _>::init(mm).expect("reopen");
        assert_eq!(map.len(), 50);
        for k in 0..50u64 {
            assert_eq!(map.get(&k), Some(k));
        }
    }

    #[test]
    fn fuzz_insert_remove_matches_reference() {
        use std::collections::HashMap;
        let map = fresh();
        let mut ref_map: HashMap<u64, u64> = HashMap::new();
        let mut seed = 0x9E3779B97F4A7C15u64;
        for _ in 0..2000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let k = seed % 500;
            let v = seed;
            if seed.is_multiple_of(3) {
                let got = map.remove(&k);
                let expected = ref_map.remove(&k);
                assert_eq!(got, expected, "remove {k}");
            } else {
                let got = map.insert(k, v).unwrap();
                let expected = ref_map.insert(k, v);
                assert_eq!(got, expected, "insert {k}");
            }
        }
        assert_eq!(map.len(), ref_map.len() as u64);
        for (k, v) in &ref_map {
            assert_eq!(map.get(k), Some(*v));
        }
    }
}
