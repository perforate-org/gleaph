//! Stable Clustered Hashing (Amble & Knuth 1974, "Ordered Hash Tables"): a flattened chained hash
//! table where items of the same bucket are clustered together in the table.
//!
//! Layout (V1, `CHM` magic, 128-byte metadata prefix):
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
//! Remap boundary         ↕ 8 bytes
//! ----------------------------------------
//! Logical capacity       ↕ 8 bytes
//! ----------------------------------------
//! Resize target capacity ↕ 8 bytes
//! Resize clear cursor    ↕ 8 bytes
//! Resize target N        ↕ 1 byte
//! Resize state           ↕ 1 byte
//! Resize remap start    ↕ 8 bytes
//! ----------------------------------------
//! Header extension      ↕ 64 bytes
//!   future V1 metadata
//! ---------------------------------------- <- Address 128
//! Entries: [K + V + distance(u32); capacity]
//!   capacity >= 2^N      (dynamic cleared relocation tail)
//! ----------------------------------------
//! Unallocated space
//! ```
//!
//! `distance == u32::MAX` marks an empty slot. Real distances are checked at insert and must stay
//! strictly below it; an overflow traps (rolling back the message on the IC). Buckets are `lower N
//! bits of (rapidhash(key) * 2^64/phi)` (Fibonacci hashing). The hash is **not stored** (saves
//! 8B/entry); lookup compares keys directly and remap recomputes `rapidhash`.
//!
//! `K` and `V` must be **fixed-size** [`Storable`](ic_stable_structures::Storable). All mutations use
//! `&self` via [`Memory`](ic_stable_structures::Memory).

use crate::header::{self, DATA_OFFSET, InitError};
use crate::iter::Iter;
use crate::memory::{
    WASM_PAGE_SIZE, grow_memory_to_at_least_bytes, read_u8, read_u32, read_u64, write_u8, write_u64,
};
use ic_stable_structures::{Memory, Storable};
use rapidhash::v3::{DEFAULT_RAPID_SECRETS, rapidhash_v3_inline};
use std::borrow::Cow;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

/// Empty marker: `u32::MAX` is never a real distance (distances are checked at insert).
const EMPTY: u32 = u32::MAX;
/// `2^64 / phi` (golden ratio), the Fibonacci hashing multiplier.
const FIB_CONST: u64 = 11400714819323198485;
/// Initial `log2_buckets`: 2^3 = 8 buckets, capacity = 8 + 3 = 11.
const DEFAULT_LOG2_BUCKETS: u8 = 3;
/// Number of positions the incremental resize remaps per insert/remove step.
const REMAP_BATCH: u64 = 64;
/// Number of new slots initialized during one resize-maintenance step.
const RESIZE_CLEAR_BATCH: u64 = 64;
/// Amortized number of cleared slots added when relocation reaches the logical tail.
const TAIL_GROWTH_CHUNK: u64 = 64;

/// Failure mutating a [`StableClusteredHashMap`].
#[derive(Debug, PartialEq, Eq)]
pub enum InsertError {
    /// Stable memory grow failed while extending the table or relocation tail.
    OutOfMemory,
    /// The requested logical capacity or byte range cannot be represented.
    CapacityOverflow,
}

impl std::fmt::Display for InsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfMemory => write!(f, "out of stable memory while growing the hash map"),
            Self::CapacityOverflow => write!(f, "hash map capacity exceeds the addressable range"),
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

/// Hashes key bytes with rapidhash V3 (deterministic constant seed). Keys are fixed-size, so the
/// `_inline` specialization can constant-fold the length-dependent finalization branches.
fn hash_key(key: &[u8]) -> u64 {
    rapidhash_v3_inline::<true, false, false>(key, &DEFAULT_RAPID_SECRETS)
}

/// Converts a `u64` distance to the stored width, trapping if it would reach the [`EMPTY`] marker
/// (real distances must stay strictly below it). On the IC a trap rolls back the whole message, so
/// the map is never left partially written.
fn checked_distance(distance: u64) -> u32 {
    assert!(
        distance < EMPTY as u64,
        "distance overflow: a bucket's cluster exceeds the maximum representable distance ({})",
        EMPTY - 1
    );
    distance as u32
}

/// An in-memory entry used during insert relocation.
struct Entry<K, V> {
    key: K,
    value: V,
    distance: u32,
}

const TRANSACTION_BLOCK_SIZE: u64 = 1024;

struct UndoBlock {
    index: u64,
    offset: u64,
    bytes: Vec<u8>,
}

/// Direct-write memory that retains the original bytes needed to undo a returned mutation error.
/// Each pre-operation logical block is copied at most once; writes in a newly published tail need
/// no undo record because restoring the original header makes those bytes unreachable.
struct UndoMemory<'a, M: Memory> {
    base: &'a M,
    protected_bytes: u64,
    blocks: Rc<RefCell<Vec<UndoBlock>>>,
}

impl<'a, M: Memory> UndoMemory<'a, M> {
    fn new(base: &'a M, protected_bytes: u64) -> Self {
        Self {
            base,
            protected_bytes,
            blocks: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn snapshot_block(&self, index: u64) {
        if self
            .blocks
            .borrow()
            .binary_search_by_key(&index, |block| block.index)
            .is_ok()
        {
            return;
        }
        let offset = index
            .checked_mul(TRANSACTION_BLOCK_SIZE)
            .expect("transaction block address overflow");
        let mut bytes = vec![0; TRANSACTION_BLOCK_SIZE as usize];
        self.base.read(offset, &mut bytes);
        let mut blocks = self.blocks.borrow_mut();
        let position = blocks
            .binary_search_by_key(&index, |block| block.index)
            .expect_err("transaction block was absent before snapshot");
        blocks.insert(
            position,
            UndoBlock {
                index,
                offset,
                bytes,
            },
        );
    }

    fn rollback(&self) {
        for block in self.blocks.borrow_mut().drain(..) {
            self.base.write(block.offset, &block.bytes);
        }
    }
}

impl<M: Memory> Clone for UndoMemory<'_, M> {
    fn clone(&self) -> Self {
        Self {
            base: self.base,
            protected_bytes: self.protected_bytes,
            blocks: Rc::clone(&self.blocks),
        }
    }
}

impl<M: Memory> Memory for UndoMemory<'_, M> {
    fn size(&self) -> u64 {
        self.base.size()
    }

    fn grow(&self, pages: u64) -> i64 {
        self.base.grow(pages)
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        self.base.read(offset, dst);
    }

    fn write(&self, offset: u64, src: &[u8]) {
        let end = offset
            .checked_add(src.len() as u64)
            .expect("transaction write address overflow");
        let snapshot_end = end.min(self.protected_bytes);
        if offset < snapshot_end {
            let first = offset / TRANSACTION_BLOCK_SIZE;
            let last = (snapshot_end - 1) / TRANSACTION_BLOCK_SIZE;
            for index in first..=last {
                self.snapshot_block(index);
            }
        }
        self.base.write(offset, src);
    }
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
        let buckets = 1u64
            .checked_shl(header.log2_buckets as u32)
            .ok_or(InitError::InvalidLayout)?;
        let stride = (header.key_size as u64)
            .checked_add(header.value_size as u64)
            .and_then(|size| size.checked_add(4))
            .ok_or(InitError::InvalidLayout)?;
        let required_capacity = match header.resize_state {
            header::ResizeState::Clearing => header.capacity,
            header::ResizeState::Settled | header::ResizeState::Publishing => {
                header.capacity.max(header.resize_target_capacity)
            }
        };
        let required = required_capacity
            .checked_mul(stride)
            .and_then(|bytes| DATA_OFFSET.checked_add(bytes))
            .ok_or(InitError::InvalidLayout)?;
        let allocated = memory
            .size()
            .checked_mul(WASM_PAGE_SIZE)
            .ok_or(InitError::InvalidLayout)?;
        let resize_valid = match header.resize_state {
            header::ResizeState::Settled => true,
            header::ResizeState::Clearing => {
                let Some(target_buckets) = 1u64.checked_shl(header.resize_target_log2 as u32)
                else {
                    return Err(InitError::InvalidLayout);
                };
                let Some(old_capacity) = header
                    .resize_target_capacity
                    .checked_sub(target_buckets / 2)
                else {
                    return Err(InitError::InvalidLayout);
                };
                header.resize_target_log2 == header.log2_buckets.saturating_add(1)
                    && header.remap_end == u64::MAX
                    && header.resize_target_capacity > header.capacity
                    && header.resize_cursor == header.capacity
                    && header.capacity >= old_capacity
                    && header.resize_cursor <= header.resize_target_capacity
                    && header.resize_remap_start >= old_capacity
                    && header.resize_remap_start <= header.capacity
            }
            header::ResizeState::Publishing => {
                header.resize_cursor == header.resize_target_capacity
                    && header.capacity <= header.resize_target_capacity
                    && header.resize_target_log2 >= header.log2_buckets
                    && header.resize_target_log2 <= header.log2_buckets.saturating_add(1)
                    && header.resize_remap_start < header.resize_target_capacity
            }
        };
        if header.capacity < buckets
            || header.len > header.capacity
            || (header.remap_end != u64::MAX && header.remap_end >= header.capacity)
            || !resize_valid
            || allocated < required
        {
            return Err(InitError::InvalidLayout);
        }
        let map = Self {
            memory,
            _marker: PhantomData,
        };
        if map.resize_state() == header::ResizeState::Publishing {
            map.publish_resize().map_err(|_| InitError::InvalidLayout)?;
        }
        Ok(map)
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
        let size = DATA_OFFSET + capacity * (key_size as u64 + value_size as u64 + 4);
        grow_memory_to_at_least_bytes(&memory, size).map_err(|_| InitError::OutOfMemory)?;
        header::write_header(&memory, n, key_size, value_size, capacity);
        Self::clear_region(&memory, 0, capacity, key_size, value_size);
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

    /// Returns the persisted logical table capacity, including the cleared relocation tail.
    pub fn capacity(&self) -> u64 {
        read_u64(&self.memory, header::CAPACITY_OFFSET)
    }

    fn resize_state(&self) -> header::ResizeState {
        header::ResizeState::from_u8(read_u8(&self.memory, header::RESIZE_STATE_OFFSET))
            .expect("validated resize state")
    }

    fn set_resize_state(&self, state: header::ResizeState) {
        write_u8(&self.memory, header::RESIZE_STATE_OFFSET, state as u8);
    }

    fn resize_target_capacity(&self) -> u64 {
        read_u64(&self.memory, header::RESIZE_TARGET_CAPACITY_OFFSET)
    }

    fn set_resize_target_capacity(&self, capacity: u64) {
        write_u64(
            &self.memory,
            header::RESIZE_TARGET_CAPACITY_OFFSET,
            capacity,
        );
    }

    fn resize_cursor(&self) -> u64 {
        read_u64(&self.memory, header::RESIZE_CURSOR_OFFSET)
    }

    fn set_resize_cursor(&self, cursor: u64) {
        write_u64(&self.memory, header::RESIZE_CURSOR_OFFSET, cursor);
    }

    fn resize_target_log2(&self) -> u8 {
        read_u8(&self.memory, header::RESIZE_TARGET_LOG2_OFFSET)
    }

    fn set_resize_target_log2(&self, log2_buckets: u8) {
        write_u8(
            &self.memory,
            header::RESIZE_TARGET_LOG2_OFFSET,
            log2_buckets,
        );
    }

    fn resize_remap_start(&self) -> u64 {
        read_u64(&self.memory, header::RESIZE_REMAP_START_OFFSET)
    }

    fn set_resize_remap_start(&self, position: u64) {
        write_u64(&self.memory, header::RESIZE_REMAP_START_OFFSET, position);
    }

    fn clear_resize_state(&self) {
        self.set_resize_state(header::ResizeState::Settled);
        self.set_resize_target_capacity(0);
        self.set_resize_cursor(0);
        self.set_resize_target_log2(0);
        self.set_resize_remap_start(0);
    }

    /// Returns `true` when the map is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` when the next unique-key insert triggers a resize.
    fn is_full(&self) -> bool {
        self.len() >= self.resize_threshold()
    }

    fn resize_threshold(&self) -> u64 {
        (self.buckets() / 4) * 3
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

    /// The incremental-resize mixed-range boundary; `u64::MAX` = no resize in progress.
    fn remap_end(&self) -> u64 {
        read_u64(&self.memory, header::REMAP_END_OFFSET)
    }

    fn set_remap_end(&self, v: u64) {
        write_u64(&self.memory, header::REMAP_END_OFFSET, v);
    }

    fn set_capacity(&self, capacity: u64) {
        write_u64(&self.memory, header::CAPACITY_OFFSET, capacity);
    }

    fn entry_stride(&self) -> u64 {
        self.key_size() as u64 + self.value_size() as u64 + 4
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

    fn read_distance(&self, i: u64) -> u32 {
        let mut buf = [0u8; 4];
        self.memory.read(self.distance_offset(i), &mut buf);
        u32::from_le_bytes(buf)
    }

    fn write_distance(&self, i: u64, d: u32) {
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
    /// During an in-place resize, also checks the previous table size in the mixed range (read-only).
    fn lookup_index(&self, key: &K) -> (Option<u64>, u64, u64) {
        let n = self.log2_buckets();
        let hash = hash_key(&key.to_bytes());
        let b = bucket(hash, n);
        if self.is_empty() {
            // Empty map: the first entry lands at its bucket with distance 0.
            return (None, b, b);
        }
        // Search with the current N.
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
        // If a resize is in progress, check the previous N in the mixed range [0, remap_end].
        let remap_end = self.remap_end();
        if remap_end != u64::MAX && n > 0 {
            let prev_bucket = bucket(hash, n - 1);
            if prev_bucket <= remap_end {
                let mut j = prev_bucket;
                while j <= remap_end {
                    let dist = self.read_distance(j);
                    if dist == EMPTY {
                        break;
                    }
                    let bucket_j = j - dist as u64;
                    if bucket_j > prev_bucket {
                        break;
                    }
                    if bucket_j == prev_bucket && self.read_key(j) == *key {
                        return (Some(j), j, b);
                    }
                    j += 1;
                }
            }
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

    /// Returns an iterator over the map's `(key, value)` entries starting at `slot` (inclusive), in
    /// slot order. Used to resume a bounded scan across steps. The caller must ensure `slot` is a
    /// valid slot index for the map's current capacity; a stale slot after a resize is handled by
    /// the caller restarting the scan.
    pub fn iter_from(&self, slot: u64) -> Iter<'_, K, V, M> {
        Iter::from_slot(self, slot)
    }

    /// Clears the map in place, resetting it to a fresh empty state (used on canister init/reset).
    /// Keeps the current capacity; the table region is cleared and the length/remap state reset.
    pub fn clear_new(&mut self) {
        self.abort_pending_resize();
        self.set_len(0);
        self.set_remap_end(u64::MAX);
        let key_size = self.key_size();
        let value_size = self.value_size();
        let capacity = self.capacity();
        Self::clear_region(&self.memory, 0, capacity, key_size, value_size);
    }

    /// Inserts `key`/`value`, returning the previous value if the key was present.
    ///
    /// If stable-memory growth or capacity arithmetic fails, bounded remap maintenance and the
    /// requested insert leave the operation's logical bytes and header unchanged. Physical pages
    /// grown before a later failure are not rolled back.
    pub fn insert(&self, key: K, value: V) -> Result<Option<V>, InsertError> {
        if self.resize_state() != header::ResizeState::Settled {
            self.advance_resize_initialization()?;
            return self.insert_after_maintenance(key, value);
        }
        if self.remap_end() != u64::MAX {
            if self.active_operation_has_bounded_empty_suffix() {
                self.remap_step(REMAP_BATCH)?;
                return self.insert_after_maintenance(key, value);
            }
            return self.run_transaction(|map| map.insert_active_transactional(key, value));
        }
        if !self.is_full() {
            return self.insert_after_maintenance(key, value);
        }
        // A settled full table must distinguish overwrite from a new-key resize. This is the only
        // path that pays a lookup before maintenance; ordinary inserts retain one lookup.
        if let Some(idx) = self.lookup_index(&key).0 {
            let prev = self.read_value(idx);
            self.write_value(idx, &value);
            return Ok(Some(prev));
        }
        self.run_transaction(|map| map.insert_new_key_after_threshold(key, value))
    }

    fn insert_active_transactional(&self, key: K, value: V) -> Result<Option<V>, InsertError> {
        self.remap_step_transactional(REMAP_BATCH)?;
        self.insert_after_maintenance(key, value)
    }

    fn insert_new_key_after_threshold(&self, key: K, value: V) -> Result<Option<V>, InsertError> {
        self.size_up()?;
        self.insert_after_maintenance(key, value)
    }

    fn insert_after_maintenance(&self, key: K, value: V) -> Result<Option<V>, InsertError> {
        let (found, insert_position, b) = self.lookup_index(&key);
        if let Some(idx) = found {
            let prev = self.read_value(idx);
            self.write_value(idx, &value);
            return Ok(Some(prev));
        }
        let distance = checked_distance(insert_position - b);
        let entry = Entry {
            key,
            value,
            distance,
        };
        let last_affected = self.insert_and_relocate(entry, insert_position)?;
        self.adjust_remap_end(insert_position, last_affected);
        self.adjust_resize_remap_start(last_affected);
        self.set_len(self.len() + 1);
        Ok(None)
    }

    /// Core insert with relocation: append to the cluster tail, swapping the displaced head of the
    /// next cluster and relocating it. Returns the last affected position. The full relocation is
    /// preflighted and a cleared tail chunk is published before the first destructive write.
    fn insert_and_relocate(
        &self,
        mut entry: Entry<K, V>,
        mut position: u64,
    ) -> Result<u64, InsertError> {
        // The common direct-insert path needs no chain preflight. Reserve the larger table only
        // when the destination is occupied or already beyond the logical tail.
        if position < self.capacity() && self.is_empty_slot(position) {
            self.write_entry(position, &entry);
            return Ok(position);
        }
        // A relocation can displace the head of each following cluster until it reaches the
        // overflow boundary. Reserve the larger table before the first write so an OOM does not
        // publish a partial relocation.
        if position >= self.capacity() || self.relocation_reaches_capacity(position) {
            self.extend_tail()?;
        }
        if position < self.capacity() && self.is_empty_slot(position) {
            self.write_entry(position, &entry);
            return Ok(position);
        }

        loop {
            if position >= self.capacity() {
                unreachable!("relocation preflight reserved a cleared tail slot");
            }
            if self.is_empty_slot(position) {
                self.write_entry(position, &entry);
                return Ok(position);
            }
            let t = self.read_entry(position);
            let next = self.end_of_cluster_by_position(position);
            let mut t = t;
            t.distance = checked_distance(t.distance as u64 + (next - position));
            self.write_entry(position, &entry);
            entry = t;
            position = next;
        }
    }

    /// Returns whether relocating from `position` reaches past the current capacity before it
    /// reaches an empty slot. This is read-only preflight for [`Self::insert_and_relocate`].
    fn relocation_reaches_capacity(&self, mut position: u64) -> bool {
        let capacity = self.capacity();
        if position >= capacity {
            return true;
        }
        // Relocation only advances. Fewer occupied entries than suffix slots guarantees an empty
        // destination without traversing the chain.
        if capacity - position > self.len() {
            return false;
        }
        // An empty terminal slot bounds every relocation chain, so avoid a second traversal for
        // the common case where the chain cannot overflow.
        if self.is_empty_slot(capacity - 1) {
            return false;
        }
        while position < capacity {
            if self.is_empty_slot(position) {
                return false;
            }
            position = self.end_of_cluster_by_position(position);
        }
        true
    }

    /// Extends the logical tail in grow -> clear -> publish order.
    fn extend_tail(&self) -> Result<(), InsertError> {
        if self.resize_state() != header::ResizeState::Settled {
            return self.extend_pending_tail();
        }
        let old_capacity = self.capacity();
        let new_capacity = old_capacity
            .checked_add(TAIL_GROWTH_CHUNK)
            .ok_or(InsertError::CapacityOverflow)?;
        let new_size = new_capacity
            .checked_mul(self.entry_stride())
            .and_then(|bytes| DATA_OFFSET.checked_add(bytes))
            .ok_or(InsertError::CapacityOverflow)?;

        grow_memory_to_at_least_bytes(&self.memory, new_size)
            .map_err(|_| InsertError::OutOfMemory)?;
        Self::clear_region(
            &self.memory,
            old_capacity,
            new_capacity,
            self.key_size(),
            self.value_size(),
        );
        self.set_capacity(new_capacity);
        Ok(())
    }

    fn extend_pending_tail(&self) -> Result<(), InsertError> {
        debug_assert_eq!(self.resize_state(), header::ResizeState::Clearing);
        let old_capacity = self.capacity();
        let target_capacity = self.resize_target_capacity();
        let new_capacity = old_capacity
            .checked_add(TAIL_GROWTH_CHUNK)
            .ok_or(InsertError::CapacityOverflow)?;
        if new_capacity >= target_capacity {
            return Err(InsertError::CapacityOverflow);
        }
        let new_size = new_capacity
            .checked_mul(self.entry_stride())
            .and_then(|bytes| DATA_OFFSET.checked_add(bytes))
            .ok_or(InsertError::CapacityOverflow)?;
        grow_memory_to_at_least_bytes(&self.memory, new_size)
            .map_err(|_| InsertError::OutOfMemory)?;
        Self::clear_region(
            &self.memory,
            old_capacity,
            new_capacity,
            self.key_size(),
            self.value_size(),
        );
        self.set_capacity(new_capacity);
        self.set_resize_cursor(new_capacity);
        Ok(())
    }

    /// Removes `key`, returning the previous value if present.
    ///
    /// Bounded remap maintenance and the requested removal are one logical operation. If their
    /// stable-memory growth or capacity arithmetic fails, the exact [`InsertError`] is returned
    /// with logical bytes, header, length, capacity, remap boundary, and key set unchanged.
    /// Physical pages grown before a later failure are not rolled back.
    pub fn remove(&self, key: &K) -> Result<Option<V>, InsertError> {
        if self.resize_state() != header::ResizeState::Settled {
            self.advance_resize_initialization()?;
            return self.remove_after_maintenance(key);
        }
        if self.remap_end() != u64::MAX {
            if self.active_operation_has_bounded_empty_suffix() {
                return self.remove_unplanned(key);
            }
            return self.run_transaction(|map| map.remove_transactional(key));
        }
        self.remove_unplanned(key)
    }

    fn remove_unplanned(&self, key: &K) -> Result<Option<V>, InsertError> {
        if self.resize_state() != header::ResizeState::Settled {
            return self.remove_after_maintenance(key);
        }
        self.remap_step(REMAP_BATCH)?;
        self.remove_after_maintenance(key)
    }

    fn remove_transactional(&self, key: &K) -> Result<Option<V>, InsertError> {
        if self.resize_state() != header::ResizeState::Settled {
            self.advance_resize_initialization()?;
            return self.remove_after_maintenance(key);
        }
        self.remap_step_transactional(REMAP_BATCH)?;
        self.remove_after_maintenance(key)
    }

    fn remove_after_maintenance(&self, key: &K) -> Result<Option<V>, InsertError> {
        let (idx, _, _) = self.lookup_index(key);
        let Some(idx) = idx else {
            return Ok(None);
        };
        let prev = self.read_value(idx);
        self.remove_and_relocate(idx);
        self.set_len(self.len() - 1);
        Ok(Some(prev))
    }

    /// Proves that one bounded maintenance batch plus a requested insert cannot consume every
    /// empty suffix slot. Each remapped entry can fill at most one empty slot and the requested
    /// insert can fill one more; remove is covered conservatively by the same bound.
    fn active_operation_has_bounded_empty_suffix(&self) -> bool {
        const STACK_BYTES: usize = 4096;
        let required = REMAP_BATCH + 1;
        let capacity = self.capacity();
        if capacity < required {
            return false;
        }
        if !self.is_empty_slot(capacity - 1) {
            return false;
        }
        let remaining = required - 1;
        let stride = self.entry_stride();
        let Some(byte_len) = remaining.checked_mul(stride) else {
            return false;
        };
        let Ok(byte_len) = usize::try_from(byte_len) else {
            return false;
        };
        if byte_len > STACK_BYTES {
            return (capacity - required..capacity - 1)
                .all(|position| self.is_empty_slot(position));
        }
        let Some(offset) = (capacity - required)
            .checked_mul(stride)
            .and_then(|bytes| DATA_OFFSET.checked_add(bytes))
        else {
            return false;
        };
        let distance_offset = self.key_size() as usize + self.value_size() as usize;
        let stride = stride as usize;
        let mut bytes = [0; STACK_BYTES];
        self.memory.read(offset, &mut bytes[..byte_len]);
        (0..remaining as usize).all(|index| {
            let start = index * stride + distance_offset;
            u32::from_le_bytes(bytes[start..start + 4].try_into().expect("distance width")) == EMPTY
        })
    }

    /// Runs a growth-capable mutation directly while snapshotting each overwritten logical block
    /// once. A returned error restores the original blocks; pages and bytes beyond the original
    /// logical capacity remain unreachable after the header rollback. A trap relies on the IC's
    /// message-level rollback, as do the map's distance-overflow assertions.
    fn run_transaction<R>(
        &self,
        operation: impl FnOnce(
            &StableClusteredHashMap<K, V, UndoMemory<'_, M>>,
        ) -> Result<R, InsertError>,
    ) -> Result<R, InsertError> {
        let protected_bytes = self
            .capacity()
            .checked_mul(self.entry_stride())
            .and_then(|bytes| DATA_OFFSET.checked_add(bytes))
            .ok_or(InsertError::CapacityOverflow)?;
        let undo_memory = UndoMemory::new(&self.memory, protected_bytes);
        let transaction_map = StableClusteredHashMap {
            memory: undo_memory.clone(),
            _marker: PhantomData,
        };
        match operation(&transaction_map) {
            Ok(result) => Ok(result),
            Err(error) => {
                undo_memory.rollback();
                Err(error)
            }
        }
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
            tail.distance = (tail.distance as u64 - (next - position)) as u32;
            self.write_entry(position, &tail);
            position = next;
        }
    }

    /// Clears one bounded prefix of a pending resize. The logical capacity exposes only slots that
    /// have already been initialized, so old-N lookup and relocation remain valid while clearing
    /// is in progress.
    fn advance_resize_initialization(&self) -> Result<(), InsertError> {
        match self.resize_state() {
            header::ResizeState::Settled => Ok(()),
            header::ResizeState::Publishing => self.publish_resize(),
            header::ResizeState::Clearing => {
                let cursor = self.resize_cursor();
                let target = self.resize_target_capacity();
                if cursor > target || self.capacity() != cursor {
                    return Err(InsertError::CapacityOverflow);
                }
                let remaining = target - cursor;
                if remaining == 0 {
                    return self.publish_resize();
                }
                // Leave at least one tail chunk for a relocation-triggered extension in the same
                // operation. Clearing the final two chunks together is still a fixed bound.
                let amount = if remaining <= RESIZE_CLEAR_BATCH * 2 {
                    remaining
                } else {
                    RESIZE_CLEAR_BATCH
                };
                let next = cursor
                    .checked_add(amount)
                    .ok_or(InsertError::CapacityOverflow)?;
                let next_size = next
                    .checked_mul(self.entry_stride())
                    .and_then(|bytes| DATA_OFFSET.checked_add(bytes))
                    .ok_or(InsertError::CapacityOverflow)?;
                grow_memory_to_at_least_bytes(&self.memory, next_size)
                    .map_err(|_| InsertError::OutOfMemory)?;
                Self::clear_region(
                    &self.memory,
                    cursor,
                    next,
                    self.key_size(),
                    self.value_size(),
                );
                self.set_capacity(next);
                self.set_resize_cursor(next);
                if next == target {
                    self.publish_resize()
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Publishes a fully initialized target table. The publishing marker makes the header
    /// recoverable if a canister is reopened between individual metadata writes.
    fn publish_resize(&self) -> Result<(), InsertError> {
        let target_capacity = self.resize_target_capacity();
        let target_log2 = self.resize_target_log2();
        let target_buckets = 1u64
            .checked_shl(target_log2 as u32)
            .ok_or(InsertError::CapacityOverflow)?;
        if self.capacity() > target_capacity || self.resize_cursor() != target_capacity {
            return Err(InsertError::CapacityOverflow);
        }
        let remap_start = self.resize_remap_start();
        if remap_start >= target_capacity {
            return Err(InsertError::CapacityOverflow);
        }
        if target_buckets == 0 {
            return Err(InsertError::CapacityOverflow);
        }
        self.set_resize_state(header::ResizeState::Publishing);
        self.set_capacity(target_capacity);
        write_u8(&self.memory, header::LOG2_BUCKETS_OFFSET, target_log2);
        self.set_remap_end(remap_start);
        // Settled state is the commit marker. Metadata cleanup is deliberately after it so a
        // reopen can finish an interrupted publication idempotently.
        self.set_resize_state(header::ResizeState::Settled);
        self.set_resize_target_capacity(0);
        self.set_resize_cursor(0);
        self.set_resize_target_log2(0);
        self.set_resize_remap_start(0);
        Ok(())
    }

    /// Aborts a not-yet-published resize for the explicit `clear_new` reset operation.
    fn abort_pending_resize(&self) {
        match self.resize_state() {
            header::ResizeState::Settled => {}
            header::ResizeState::Publishing => {
                self.publish_resize().expect("valid pending resize");
            }
            header::ResizeState::Clearing => {
                let old_buckets = self.buckets();
                let old_capacity = self
                    .resize_target_capacity()
                    .checked_sub(old_buckets)
                    .expect("valid pending resize capacity");
                self.set_capacity(old_capacity);
                self.set_remap_end(u64::MAX);
                self.clear_resize_state();
            }
        }
    }

    #[cfg(any(test, feature = "canbench"))]
    fn finish_resize_initialization_for_setup(&self) -> Result<(), InsertError> {
        if self.resize_state() == header::ResizeState::Clearing {
            let cursor = self.resize_cursor();
            let target = self.resize_target_capacity();
            let target_size = target
                .checked_mul(self.entry_stride())
                .and_then(|bytes| DATA_OFFSET.checked_add(bytes))
                .ok_or(InsertError::CapacityOverflow)?;
            grow_memory_to_at_least_bytes(&self.memory, target_size)
                .map_err(|_| InsertError::OutOfMemory)?;
            Self::clear_region(
                &self.memory,
                cursor,
                target,
                self.key_size(),
                self.value_size(),
            );
            self.set_capacity(target);
            self.set_resize_cursor(target);
        }
        if self.resize_state() != header::ResizeState::Settled {
            self.publish_resize()?;
        }
        Ok(())
    }

    /// Doubles a settled table's buckets, preserving its current cleared tail reserve.
    fn size_up(&self) -> Result<(), InsertError> {
        if self.resize_state() != header::ResizeState::Settled || self.remap_end() != u64::MAX {
            unreachable!("insert pressure prevents bucket growth during an active remap");
        }
        let n = self.log2_buckets();
        let new_n = n.checked_add(1).ok_or(InsertError::CapacityOverflow)?;
        let prev_capacity = self.capacity();
        let tail_reserve = prev_capacity
            .checked_sub(self.buckets())
            .ok_or(InsertError::CapacityOverflow)?;
        let new_buckets = 1u64
            .checked_shl(new_n as u32)
            .ok_or(InsertError::CapacityOverflow)?;
        let new_capacity = new_buckets
            .checked_add(tail_reserve)
            .ok_or(InsertError::CapacityOverflow)?;
        // Persist the target before clearing so a reopen can resume from the cursor. The old
        // mapping and logical capacity remain visible until each cleared prefix is published.
        self.set_resize_target_capacity(new_capacity);
        self.set_resize_cursor(prev_capacity);
        self.set_resize_target_log2(new_n);
        self.set_resize_remap_start(prev_capacity);
        self.set_resize_state(header::ResizeState::Clearing);
        self.advance_resize_initialization()
    }

    /// Remaps up to `max_entries` positions from the bottom of the mixed range, relocating items
    /// whose bucket changed under the new N. Returns `true` when the resize is complete.
    fn remap_step(&self, max_entries: u64) -> Result<bool, InsertError> {
        let mut left = max_entries;
        while self.remap_end() != u64::MAX && left > 0 {
            let position = self.remap_end();
            left -= 1;
            if !self.is_empty_slot(position) {
                let n = self.log2_buckets();
                let key = self.read_key(position);
                let new_bucket = bucket(hash_key(&key.to_bytes()), n);
                let current_bucket = self.bucket_by_position(position);
                if current_bucket != new_bucket {
                    self.remap_position(position, key, new_bucket)?;
                    continue;
                }
            }
            self.set_remap_end(position.wrapping_sub(1));
        }
        Ok(self.remap_end() == u64::MAX)
    }

    /// Transactional counterpart to [`Self::remap_step`]. The undo boundary permits source-gap
    /// filling before the post-removal insertion preflight, so a remapped chain is traversed once
    /// instead of once before and once after the source removal.
    fn remap_step_transactional(&self, max_entries: u64) -> Result<bool, InsertError> {
        let mut left = max_entries;
        while self.remap_end() != u64::MAX && left > 0 {
            let position = self.remap_end();
            left -= 1;
            if !self.is_empty_slot(position) {
                let n = self.log2_buckets();
                let key = self.read_key(position);
                let new_bucket = bucket(hash_key(&key.to_bytes()), n);
                let current_bucket = self.bucket_by_position(position);
                if current_bucket != new_bucket {
                    self.remap_position_transactional(position, key, new_bucket)?;
                    continue;
                }
            }
            self.set_remap_end(position.wrapping_sub(1));
        }
        Ok(self.remap_end() == u64::MAX)
    }

    /// Relocates the item at `position` to `new_bucket`, expanding `remap_end` if the relocation
    /// pushed an item across the boundary. `key` is already read by the caller, so it is not re-read.
    fn remap_position(&self, position: u64, key: K, new_bucket: u64) -> Result<(), InsertError> {
        let insert_position = self.find_insert_position(new_bucket);
        if self.relocation_reaches_capacity(insert_position) {
            self.extend_tail()?;
        }
        let value = self.read_value(position);
        self.remove_and_relocate(position);
        let insert_position = self.find_insert_position(new_bucket);
        let entry = Entry {
            key,
            value,
            distance: checked_distance(insert_position - new_bucket),
        };
        let last_affected = self.insert_and_relocate(entry, insert_position)?;
        self.adjust_remap_end(insert_position, last_affected);
        Ok(())
    }

    /// Transactional remap position. Source removal is intentionally before the only insertion
    /// preflight; [`Self::run_transaction`] restores it if tail growth then returns an error.
    fn remap_position_transactional(
        &self,
        position: u64,
        key: K,
        new_bucket: u64,
    ) -> Result<(), InsertError> {
        let value = self.read_value(position);
        self.remove_and_relocate(position);
        let insert_position = self.find_insert_position(new_bucket);
        let entry = Entry {
            key,
            value,
            distance: checked_distance(insert_position - new_bucket),
        };
        let last_affected = self.insert_and_relocate(entry, insert_position)?;
        self.adjust_remap_end(insert_position, last_affected);
        Ok(())
    }

    fn adjust_remap_end(&self, insert_position: u64, last_affected: u64) {
        let remap_end = self.remap_end();
        if remap_end != u64::MAX && insert_position <= remap_end && remap_end < last_affected {
            self.set_remap_end(last_affected);
        }
    }

    fn adjust_resize_remap_start(&self, last_affected: u64) {
        if self.resize_state() == header::ResizeState::Clearing
            && last_affected >= self.resize_remap_start()
        {
            self.set_resize_remap_start(last_affected);
        }
    }

    /// Sets every slot's distance in `[start, end)` to [`EMPTY`] (the key/value bytes are left as
    /// garbage; they are only read when the slot is occupied). Writes in chunks so the clear is a few
    /// large writes instead of one 4-byte write per slot.
    fn clear_region<M2: Memory>(memory: &M2, start: u64, end: u64, key_size: u32, value_size: u32) {
        const CHUNK: u64 = 64;
        let stride = key_size as u64 + value_size as u64 + 4;
        let dist_offset = key_size as u64 + value_size as u64;
        // Build one chunk of `CHUNK` entries: [0; key] [0; value] [EMPTY] repeated.
        let mut chunk = vec![0u8; (CHUNK * stride) as usize];
        for i in 0..CHUNK {
            let e = (i * stride + dist_offset) as usize;
            chunk[e..e + 4].copy_from_slice(&EMPTY.to_le_bytes());
        }
        let mut i = start;
        while i < end {
            let n = (end - i).min(CHUNK);
            memory.write(DATA_OFFSET + i * stride, &chunk[..(n * stride) as usize]);
            i += n;
        }
    }
}

#[cfg(feature = "canbench")]
pub(crate) mod canbench_fixtures {
    use super::*;
    use ic_stable_structures::DefaultMemoryImpl;

    type BenchMap = StableClusteredHashMap<u64, u64, DefaultMemoryImpl>;

    /// A map state and its complete post-insert contract for one focused canbench case.
    #[derive(Clone, Copy)]
    enum RemapExpectation {
        Exact(u64),
        ActiveBefore(u64),
    }

    pub(crate) struct InsertFixture {
        pub(crate) map: BenchMap,
        pub(crate) target: u64,
        residents: Vec<(u64, u64)>,
        expected_len: u64,
        expected_buckets: u64,
        expected_capacity: u64,
        remap_expectation: RemapExpectation,
        expected_terminal_cluster: Option<(u64, u64)>,
    }

    impl InsertFixture {
        /// Checks every seeded entry and selected structural postconditions after the timed public insert.
        pub(crate) fn assert_postconditions(&self) {
            assert_eq!(self.map.get(&self.target), Some(self.target));
            for (key, value) in &self.residents {
                assert_eq!(self.map.get(key), Some(*value));
            }
            assert_eq!(self.map.len(), self.expected_len);
            assert_eq!(self.map.buckets(), self.expected_buckets);
            assert_eq!(self.map.capacity(), self.expected_capacity);
            match self.remap_expectation {
                RemapExpectation::Exact(expected) => assert_eq!(self.map.remap_end(), expected),
                RemapExpectation::ActiveBefore(before) => {
                    let actual = self.map.remap_end();
                    assert_ne!(actual, u64::MAX);
                    assert!(
                        actual < before,
                        "active remap did not make bounded progress"
                    );
                }
            }
            if let Some((home, entries)) = self.expected_terminal_cluster {
                assert_eq!(self.map.end_of_cluster_by_position(home), home + entries);
                for slot in home..home + entries {
                    assert_eq!(self.map.bucket_by_position(slot), home);
                }
            }
        }
    }

    /// A settled threshold-resize fixture at a selected bucket exponent. Setup writes one valid
    /// resident per old home bucket; the timed operation is the public insert that calls `size_up`.
    pub(crate) struct ScaleResizeFixture {
        pub(crate) map: BenchMap,
        pub(crate) target: u64,
        representatives: [(u64, u64); 4],
        expected_len: u64,
        expected_buckets: u64,
        expected_capacity: u64,
        expected_remap_end: u64,
        expected_resize_state: header::ResizeState,
        expected_target_capacity: u64,
    }

    impl ScaleResizeFixture {
        pub(crate) fn assert_postconditions(&self) {
            assert_eq!(self.map.get(&self.target), Some(self.target));
            for (key, value) in self.representatives {
                assert_eq!(self.map.get(&key), Some(value));
            }
            assert_eq!(self.map.len(), self.expected_len);
            assert_eq!(self.map.buckets(), self.expected_buckets);
            assert_eq!(self.map.capacity(), self.expected_capacity);
            assert_eq!(self.map.resize_cursor(), self.expected_capacity);
            assert_eq!(self.map.remap_end(), self.expected_remap_end);
            assert_eq!(self.map.resize_state(), self.expected_resize_state);
            assert_eq!(
                self.map.resize_target_capacity(),
                self.expected_target_capacity
            );
            for position in self.expected_capacity - RESIZE_CLEAR_BATCH..self.expected_capacity {
                assert!(
                    self.map.is_empty_slot(position),
                    "bounded resize prefix slot {position} was not cleared"
                );
            }
        }
    }

    fn new_map_at_log2_buckets(log2_buckets: u8) -> BenchMap {
        let map = BenchMap::new(DefaultMemoryImpl::default()).expect("create canbench fixture map");
        while map.log2_buckets() < log2_buckets {
            map.size_up().expect("pre-grow canbench fixture map");
            map.finish_resize_initialization_for_setup()
                .expect("finish pre-grow canbench initialization");
            assert!(
                map.remap_step(u64::MAX)
                    .expect("pre-grow canbench fixture remap"),
                "pre-grow canbench fixture remap completes"
            );
        }
        assert_eq!(map.log2_buckets(), log2_buckets);
        assert_eq!(map.remap_end(), u64::MAX);
        map
    }

    fn next_key_for_home(next_key: &mut u64, log2_buckets: u8, home: u64) -> u64 {
        loop {
            let candidate = *next_key;
            *next_key = candidate
                .checked_add(1)
                .expect("canbench fixture key search exhausted u64");
            if bucket(hash_key(&candidate.to_bytes()), log2_buckets) == home {
                return candidate;
            }
        }
    }

    fn next_key_for_old_and_new_home(
        next_key: &mut u64,
        old_log2_buckets: u8,
        old_home: u64,
        new_home: u64,
    ) -> u64 {
        loop {
            let candidate = *next_key;
            *next_key = candidate
                .checked_add(1)
                .expect("canbench fixture key search exhausted u64");
            if bucket(hash_key(&candidate.to_bytes()), old_log2_buckets) == old_home
                && bucket(hash_key(&candidate.to_bytes()), old_log2_buckets + 1) == new_home
            {
                return candidate;
            }
        }
    }

    /// Creates a nonempty settled table whose target has a known empty home slot.
    pub(crate) fn settled_direct_insert() -> InsertFixture {
        let map = new_map_at_log2_buckets(DEFAULT_LOG2_BUCKETS);
        let mut next_key = 0;
        let resident = next_key_for_home(&mut next_key, DEFAULT_LOG2_BUCKETS, 0);
        let target = next_key_for_home(&mut next_key, DEFAULT_LOG2_BUCKETS, 6);
        map.insert(resident, resident)
            .expect("seed direct-insert fixture");

        let (found, insert_position, home) = map.lookup_index(&target);
        assert_eq!(found, None);
        assert_eq!(home, 6);
        assert_eq!(insert_position, home);
        assert!(map.is_empty_slot(insert_position));
        assert!(!map.is_full());

        let expected_len = map.len() + 1;
        let expected_buckets = map.buckets();
        let expected_capacity = map.capacity();
        InsertFixture {
            map,
            target,
            residents: vec![(resident, resident)],
            expected_len,
            expected_buckets,
            expected_capacity,
            remap_expectation: RemapExpectation::Exact(u64::MAX),
            expected_terminal_cluster: None,
        }
    }

    /// Creates five adjacent singleton clusters so the target relocates exactly four occupied slots.
    pub(crate) fn settled_relocation_chain_insert() -> InsertFixture {
        let map = new_map_at_log2_buckets(DEFAULT_LOG2_BUCKETS);
        let mut next_key = 0;
        let mut residents = Vec::with_capacity(5);
        for home in 0..=4 {
            let key = next_key_for_home(&mut next_key, DEFAULT_LOG2_BUCKETS, home);
            map.insert(key, key).expect("seed relocation-chain fixture");
            residents.push((key, key));
        }
        let target = next_key_for_home(&mut next_key, DEFAULT_LOG2_BUCKETS, 0);

        assert_eq!(map.len(), 5);
        assert!(!map.is_full());
        for slot in 0..=4 {
            assert_eq!(map.read_distance(slot), 0);
        }
        assert!(map.is_empty_slot(5));
        let (found, insert_position, home) = map.lookup_index(&target);
        assert_eq!(found, None);
        assert_eq!(home, 0);
        assert_eq!(insert_position, 1);

        let expected_len = map.len() + 1;
        let expected_buckets = map.buckets();
        let expected_capacity = map.capacity();
        InsertFixture {
            map,
            target,
            residents,
            expected_len,
            expected_buckets,
            expected_capacity,
            remap_expectation: RemapExpectation::Exact(u64::MAX),
            expected_terminal_cluster: None,
        }
    }

    /// Creates an N=8 resize whose following insert examines exactly one 64-position remap batch.
    pub(crate) fn active_remap_batch_insert() -> InsertFixture {
        const OLD_LOG2_BUCKETS: u8 = 8;
        const POPULATED_OLD_BUCKETS: u64 = 192;

        let map = new_map_at_log2_buckets(OLD_LOG2_BUCKETS);
        let mut next_key = 0;
        let mut residents = Vec::with_capacity(POPULATED_OLD_BUCKETS as usize + 1);
        for old_home in 0..POPULATED_OLD_BUCKETS {
            let key = next_key_for_old_and_new_home(
                &mut next_key,
                OLD_LOG2_BUCKETS,
                old_home,
                old_home + (1 << OLD_LOG2_BUCKETS),
            );
            map.insert(key, key).expect("seed active-remap fixture");
            residents.push((key, key));
        }
        assert_eq!(map.len(), POPULATED_OLD_BUCKETS);
        assert!(map.is_full());
        for slot in 0..POPULATED_OLD_BUCKETS {
            assert_eq!(map.read_distance(slot), 0);
        }

        let trigger = next_key_for_old_and_new_home(&mut next_key, OLD_LOG2_BUCKETS, 255, 255);
        let target = next_key_for_old_and_new_home(&mut next_key, OLD_LOG2_BUCKETS, 254, 254);
        assert_eq!(map.lookup_index(&trigger).0, None);
        assert_eq!(map.lookup_index(&target).0, None);

        let previous_capacity = map.capacity();
        map.insert(trigger, trigger)
            .expect("start active-remap fixture");
        map.finish_resize_initialization_for_setup()
            .expect("finish active-remap fixture initialization");
        residents.push((trigger, trigger));
        assert_eq!(map.buckets(), 1 << (OLD_LOG2_BUCKETS + 1));
        assert_eq!(map.len(), POPULATED_OLD_BUCKETS + 1);
        assert_eq!(map.remap_end(), previous_capacity);

        let expected_len = map.len() + 1;
        let expected_buckets = map.buckets();
        let expected_capacity = map.capacity();
        let expected_remap_end = previous_capacity - REMAP_BATCH;
        InsertFixture {
            map,
            target,
            residents,
            expected_len,
            expected_buckets,
            expected_capacity,
            remap_expectation: RemapExpectation::Exact(expected_remap_end),
            expected_terminal_cluster: None,
        }
    }

    /// Creates a settled threshold resize at the requested bucket exponent.
    pub(crate) fn threshold_resize_insert_at(log2_buckets: u8) -> ScaleResizeFixture {
        assert!(
            log2_buckets >= DEFAULT_LOG2_BUCKETS,
            "scale fixture must use the map's supported bucket range"
        );
        let map = new_map_at_log2_buckets(log2_buckets);
        let threshold = map.resize_threshold();
        let mut keys_by_bucket = vec![None; threshold as usize + 1];
        let mut missing = keys_by_bucket.len();
        for candidate in 0u64.. {
            let home = bucket(hash_key(&candidate.to_bytes()), log2_buckets);
            if home <= threshold && keys_by_bucket[home as usize].is_none() {
                keys_by_bucket[home as usize] = Some(candidate);
                missing -= 1;
                if missing == 0 {
                    break;
                }
            }
        }

        for (slot, key) in keys_by_bucket[..threshold as usize].iter().enumerate() {
            let key = key.expect("key for each occupied home bucket");
            map.write_entry(
                slot as u64,
                &Entry {
                    key,
                    value: key,
                    distance: 0,
                },
            );
        }
        map.set_len(threshold);
        assert!(map.is_full());
        assert_eq!(map.remap_end(), u64::MAX);

        let target = keys_by_bucket[threshold as usize].expect("threshold trigger key");
        assert_eq!(map.lookup_index(&target).0, None);
        let representatives = [
            keys_by_bucket[0].expect("home zero resident"),
            keys_by_bucket[threshold as usize / 3].expect("first representative resident"),
            keys_by_bucket[threshold as usize * 2 / 3].expect("second representative resident"),
            keys_by_bucket[threshold as usize - 1].expect("last resident"),
        ]
        .map(|key| (key, key));
        let previous_capacity = map.capacity();
        let previous_buckets = map.buckets();
        ScaleResizeFixture {
            map,
            target,
            representatives,
            expected_len: threshold + 1,
            expected_buckets: previous_buckets,
            expected_capacity: previous_capacity + RESIZE_CLEAR_BATCH,
            expected_remap_end: u64::MAX,
            expected_resize_state: header::ResizeState::Clearing,
            expected_target_capacity: previous_capacity + previous_buckets,
        }
    }

    /// Reuses the one-key-per-home N=13 threshold construction from the stride regression.
    pub(crate) fn n13_threshold_resize_insert() -> ScaleResizeFixture {
        threshold_resize_insert_at(13)
    }

    /// Creates an active N=10 remap with a full logical tail. The timed insert extends only that
    /// tail; it must neither double buckets nor drain the remap.
    pub(crate) fn active_remap_tail_extension_insert() -> InsertFixture {
        const OLD_LOG2_BUCKETS: u8 = 9;
        const NEW_LOG2_BUCKETS: u8 = OLD_LOG2_BUCKETS + 1;
        const OLD_HOME: u64 = (1 << OLD_LOG2_BUCKETS) - 1;
        const NEW_HOME: u64 = (1 << NEW_LOG2_BUCKETS) - 1;

        let mut map =
            BenchMap::new(DefaultMemoryImpl::default()).expect("create active-remap fixture");
        let mut next_key = 0u64;
        while map.log2_buckets() < OLD_LOG2_BUCKETS || map.remap_end() != u64::MAX {
            let key = next_key;
            next_key = next_key
                .checked_add(1)
                .expect("active-remap fixture key search exhausted u64");
            map.insert(key, key)
                .expect("grow active-remap fixture publicly");
            if map.resize_state() != header::ResizeState::Settled {
                map.finish_resize_initialization_for_setup()
                    .expect("finish active-remap setup initialization");
            }
        }
        assert_eq!(map.log2_buckets(), OLD_LOG2_BUCKETS);
        assert_eq!(map.remap_end(), u64::MAX);
        map.clear_new();

        let threshold = map.resize_threshold();
        let mut old_residents = Vec::with_capacity(threshold as usize);
        // Leave a deterministic empty band in the old table so the timed remap batch only scans
        // settled empty positions; two one-key-per-home ranges plus three collisions reach the
        // load threshold.
        for home in 0..=250 {
            let key = next_key_for_home(&mut next_key, OLD_LOG2_BUCKETS, home);
            map.insert(key, key).expect("seed old residents publicly");
            old_residents.push((key, key));
        }
        for _ in 0..3 {
            let key = next_key_for_home(&mut next_key, OLD_LOG2_BUCKETS, 0);
            map.insert(key, key).expect("seed old residents publicly");
            old_residents.push((key, key));
        }
        for home in 381..=510 {
            let key = next_key_for_home(&mut next_key, OLD_LOG2_BUCKETS, home);
            map.insert(key, key).expect("seed old residents publicly");
            old_residents.push((key, key));
        }
        assert_eq!(old_residents.len() as u64, threshold);
        assert_eq!(map.len(), threshold);
        assert!(map.is_full());
        assert_eq!(map.remap_end(), u64::MAX);

        let trigger =
            next_key_for_old_and_new_home(&mut next_key, OLD_LOG2_BUCKETS, OLD_HOME, OLD_HOME);
        let previous_capacity = map.capacity();
        map.insert(trigger, trigger)
            .expect("start active-remap fixture publicly");
        map.finish_resize_initialization_for_setup()
            .expect("finish active-remap fixture initialization");
        assert_eq!(map.buckets(), 1 << NEW_LOG2_BUCKETS);
        assert_eq!(map.remap_end(), previous_capacity);

        let old_capacity = map.capacity();
        let tail_entries = old_capacity - NEW_HOME;
        assert_eq!(tail_entries, 4);
        let mut tail_residents = Vec::with_capacity(tail_entries as usize);
        for _ in 0..tail_entries {
            let key =
                next_key_for_old_and_new_home(&mut next_key, OLD_LOG2_BUCKETS, OLD_HOME, NEW_HOME);
            map.insert(key, key).expect("seed tail residents publicly");
            tail_residents.push((key, key));
        }

        let target =
            next_key_for_old_and_new_home(&mut next_key, OLD_LOG2_BUCKETS, OLD_HOME, NEW_HOME);
        assert_eq!(map.len(), threshold + 1 + tail_entries);
        assert_ne!(map.remap_end(), u64::MAX);
        assert_eq!(map.find_insert_position(NEW_HOME), old_capacity);
        for (key, value) in &old_residents {
            assert_eq!(map.get(key), Some(*value));
        }
        assert_eq!(map.get(&trigger), Some(trigger));
        for (key, value) in &tail_residents {
            assert_eq!(map.get(key), Some(*value));
        }
        assert_eq!(map.lookup_index(&target).0, None);

        let mut residents = old_residents;
        residents.push((trigger, trigger));
        residents.extend(tail_residents);

        let expected_len = map.len() + 1;
        let expected_buckets = map.buckets();
        let remap_end_before_timed = map.remap_end();
        assert_ne!(remap_end_before_timed, u64::MAX);
        InsertFixture {
            map,
            target,
            residents,
            expected_len,
            expected_buckets,
            expected_capacity: old_capacity + TAIL_GROWTH_CHUNK,
            remap_expectation: RemapExpectation::ActiveBefore(remap_end_before_timed),
            expected_terminal_cluster: Some((NEW_HOME, tail_entries + 1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::{Memory, VectorMemory};
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct FailNextGrowMemory {
        inner: VectorMemory,
        fail_next_grow: Rc<Cell<bool>>,
    }

    impl FailNextGrowMemory {
        fn new() -> Self {
            Self {
                inner: VectorMemory::default(),
                fail_next_grow: Rc::new(Cell::new(false)),
            }
        }

        fn fail_next_grow(&self) {
            self.fail_next_grow.set(true);
        }

        fn snapshot(&self) -> Vec<u8> {
            let mut bytes = vec![0; (self.size() * crate::memory::WASM_PAGE_SIZE) as usize];
            self.read(0, &mut bytes);
            bytes
        }
    }

    impl Memory for FailNextGrowMemory {
        fn size(&self) -> u64 {
            self.inner.size()
        }

        fn grow(&self, pages: u64) -> i64 {
            if self.fail_next_grow.replace(false) {
                -1
            } else {
                self.inner.grow(pages)
            }
        }

        fn read(&self, offset: u64, dst: &mut [u8]) {
            self.inner.read(offset, dst);
        }

        fn write(&self, offset: u64, src: &[u8]) {
            self.inner.write(offset, src);
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct LargeKey([u8; 1024]);

    impl Storable for LargeKey {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Borrowed(&self.0)
        }

        fn into_bytes(self) -> Vec<u8> {
            self.0.to_vec()
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            let mut value = [0; 1024];
            value.copy_from_slice(bytes.as_ref());
            Self(value)
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 1024,
                is_fixed_size: true,
            };
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct HugeKey([u8; 4096]);

    impl Storable for HugeKey {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Borrowed(&self.0)
        }

        fn into_bytes(self) -> Vec<u8> {
            self.0.to_vec()
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            let mut value = [0; 4096];
            value.copy_from_slice(bytes.as_ref());
            Self(value)
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 4096,
                is_fixed_size: true,
            };
    }

    fn fresh() -> StableClusteredHashMap<u64, u64, VectorMemory> {
        StableClusteredHashMap::new(VectorMemory::default()).expect("new")
    }

    fn next_key_for_home(next_key: &mut u64, log2_buckets: u8, home: u64) -> u64 {
        loop {
            let candidate = *next_key;
            *next_key = next_key
                .checked_add(1)
                .expect("test key search exhausted u64");
            if bucket(hash_key(&candidate.to_bytes()), log2_buckets) == home {
                return candidate;
            }
        }
    }

    fn next_key_for_old_and_new_home(
        next_key: &mut u64,
        old_log2_buckets: u8,
        old_home: u64,
        new_home: u64,
    ) -> u64 {
        loop {
            let candidate = *next_key;
            *next_key = next_key
                .checked_add(1)
                .expect("test key search exhausted u64");
            if bucket(hash_key(&candidate.to_bytes()), old_log2_buckets) == old_home
                && bucket(hash_key(&candidate.to_bytes()), old_log2_buckets + 1) == new_home
            {
                return candidate;
            }
        }
    }

    #[test]
    fn checked_distance_accepts_up_to_empty_minus_one() {
        assert_eq!(checked_distance(0), 0);
        assert_eq!(checked_distance(EMPTY as u64 - 1), EMPTY - 1);
    }

    #[test]
    #[should_panic(expected = "distance overflow")]
    fn checked_distance_panics_at_empty() {
        checked_distance(EMPTY as u64);
    }

    #[test]
    #[should_panic(expected = "distance overflow")]
    fn checked_distance_panics_on_huge() {
        checked_distance(u64::MAX);
    }

    #[test]
    fn insert_get_remove_roundtrip() {
        let map = fresh();
        assert!(map.insert(1, 10).unwrap().is_none());
        assert!(map.insert(2, 20).unwrap().is_none());
        assert_eq!(map.get(&1), Some(10));
        assert_eq!(map.get(&2), Some(20));
        assert_eq!(map.len(), 2);
        assert_eq!(map.remove(&1), Ok(Some(10)));
        assert_eq!(map.get(&1), None);
        assert_eq!(map.len(), 1);
        assert_eq!(map.remove(&2), Ok(Some(20)));
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
        map.remove(&15).expect("remove middle key");
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
    fn inner_resize_recomputes_relocated_entry_distance() {
        let memory = VectorMemory::default();
        let trigger = 0u64;
        let residents = [8u64, 26, 40, 49];

        assert!(
            residents.iter().all(|key| {
                bucket(hash_key(&key.to_bytes()), DEFAULT_LOG2_BUCKETS) == 7
                    && bucket(hash_key(&key.to_bytes()), DEFAULT_LOG2_BUCKETS + 1) == 7
            }),
            "residents occupy the initial table's final cluster"
        );
        assert_eq!(
            bucket(hash_key(&trigger.to_bytes()), DEFAULT_LOG2_BUCKETS),
            7,
            "trigger starts at the residents' cluster"
        );
        assert_eq!(
            bucket(hash_key(&trigger.to_bytes()), DEFAULT_LOG2_BUCKETS + 1),
            15,
            "trigger moves to the grown table's final bucket"
        );

        {
            let map = StableClusteredHashMap::<u64, u64, _>::new(memory.clone()).expect("new");
            for key in residents {
                map.insert(key, key).expect("insert resident");
            }
            map.insert(trigger, trigger)
                .expect("insert trigger through inner resize");

            assert_eq!(map.get(&trigger), Some(trigger));
            assert!(map.contains_key(&trigger));
            assert_eq!(map.len(), residents.len() as u64 + 1);
            assert!(
                map.iter()
                    .any(|(key, value)| key == trigger && value == trigger)
            );
        }

        let map = StableClusteredHashMap::<u64, u64, _>::init(memory).expect("reopen");
        assert_eq!(map.len(), residents.len() as u64 + 1);
        assert_eq!(map.get(&trigger), Some(trigger));
        assert!(map.contains_key(&trigger));
    }

    fn next_large_key_for_home(next_key: &mut u64, home: u64) -> LargeKey {
        loop {
            let candidate = *next_key;
            *next_key = candidate.checked_add(1).expect("large-key search overflow");
            let mut bytes = [0; 1024];
            bytes[..8].copy_from_slice(&candidate.to_le_bytes());
            let key = LargeKey(bytes);
            if bucket(hash_key(&key.to_bytes()), DEFAULT_LOG2_BUCKETS) == home {
                return key;
            }
        }
    }

    fn next_large_key_for_home_at(next_key: &mut u64, log2_buckets: u8, home: u64) -> LargeKey {
        loop {
            let candidate = *next_key;
            *next_key = candidate.checked_add(1).expect("large-key search overflow");
            let mut bytes = [0; 1024];
            bytes[..8].copy_from_slice(&candidate.to_le_bytes());
            let key = LargeKey(bytes);
            if bucket(hash_key(&key.to_bytes()), log2_buckets) == home {
                return key;
            }
        }
    }

    fn next_large_key_for_old_and_new_home(
        next_key: &mut u64,
        old_n: u8,
        old_home: u64,
        new_home: u64,
    ) -> LargeKey {
        loop {
            let candidate = *next_key;
            *next_key = candidate.checked_add(1).expect("large-key search overflow");
            let mut bytes = [0; 1024];
            bytes[..8].copy_from_slice(&candidate.to_le_bytes());
            let key = LargeKey(bytes);
            let hash = hash_key(&key.to_bytes());
            if bucket(hash, old_n) == old_home && bucket(hash, old_n + 1) == new_home {
                return key;
            }
        }
    }

    fn next_huge_key_for_home(next_key: &mut u64, log2_buckets: u8, home: u64) -> HugeKey {
        loop {
            let candidate = *next_key;
            *next_key = candidate.checked_add(1).expect("huge-key search overflow");
            let mut bytes = [0; 4096];
            bytes[..8].copy_from_slice(&candidate.to_le_bytes());
            let key = HugeKey(bytes);
            if bucket(hash_key(&key.to_bytes()), log2_buckets) == home {
                return key;
            }
        }
    }

    #[test]
    fn fresh_and_reopen_use_persisted_capacity() {
        let memory = VectorMemory::default();
        let map = StableClusteredHashMap::<u64, u64, _>::new(memory.clone()).expect("new");
        assert_eq!(map.capacity(), map.buckets() + DEFAULT_LOG2_BUCKETS as u64);
        let old_capacity = map.capacity();
        map.extend_tail().expect("extend fresh tail");
        let capacity = map.capacity();
        for slot in old_capacity..capacity {
            assert!(map.is_empty_slot(slot), "extended slot {slot} is cleared");
        }
        drop(map);

        let reopened = StableClusteredHashMap::<u64, u64, _>::init(memory.clone()).expect("reopen");
        assert_eq!(reopened.capacity(), capacity);
        for slot in old_capacity..capacity {
            assert!(
                reopened.is_empty_slot(slot),
                "extended slot {slot} reopens cleared"
            );
        }
        drop(reopened);

        // A zero persisted capacity is invalid for the current V1 layout.
        write_u64(&memory, header::CAPACITY_OFFSET, 0);
        let mut reserved_capacity = [0; 8];
        memory.read(header::CAPACITY_OFFSET, &mut reserved_capacity);
        assert_eq!(reserved_capacity, [0; 8]);
        assert!(matches!(
            StableClusteredHashMap::<u64, u64, _>::init(memory.clone()),
            Err(InitError::InvalidLayout)
        ));

        write_u64(&memory, header::CAPACITY_OFFSET, 1);
        assert!(matches!(
            StableClusteredHashMap::<u64, u64, _>::init(memory.clone()),
            Err(InitError::InvalidLayout)
        ));
        write_u64(&memory, header::CAPACITY_OFFSET, u64::MAX);
        assert!(matches!(
            StableClusteredHashMap::<u64, u64, _>::init(memory),
            Err(InitError::InvalidLayout)
        ));
    }

    #[test]
    fn current_v1_header_uses_128_byte_data_boundary() {
        let memory = VectorMemory::default();
        let map = StableClusteredHashMap::<u64, u64, _>::new(memory.clone()).expect("new");

        assert_eq!(header::HEADER_SIZE, 128);
        assert_eq!(DATA_OFFSET, 128);
        assert_eq!(map.entry_offset(0), 128);

        let mut extension = [0u8; 64];
        memory.read(64, &mut extension);
        assert!(extension.iter().all(|byte| *byte == 0));

        map.insert(1, 2).expect("insert");
        drop(map);
        let reopened = StableClusteredHashMap::<u64, u64, _>::init(memory).expect("reopen");
        assert_eq!(reopened.get(&1), Some(2));
    }

    #[test]
    fn settled_relocation_extends_tail_without_bucket_growth() {
        let map = fresh();
        let home = map.buckets() - 1;
        let initial_capacity = map.capacity();
        let mut next_key = 0;
        let mut residents = Vec::new();
        for _ in home..initial_capacity {
            let key = next_key_for_home(&mut next_key, DEFAULT_LOG2_BUCKETS, home);
            map.insert(key, key).expect("seed terminal cluster");
            residents.push(key);
        }
        let target = next_key_for_home(&mut next_key, DEFAULT_LOG2_BUCKETS, home);
        map.insert(target, target).expect("extend settled tail");

        assert_eq!(map.buckets(), 1 << DEFAULT_LOG2_BUCKETS);
        assert_eq!(map.capacity(), initial_capacity + TAIL_GROWTH_CHUNK);
        assert_eq!(map.remap_end(), u64::MAX);
        assert_eq!(map.get(&target), Some(target));
        for slot in initial_capacity + 1..map.capacity() {
            assert!(
                map.is_empty_slot(slot),
                "unused tail slot {slot} is cleared"
            );
        }
        for key in residents {
            assert_eq!(map.get(&key), Some(key));
        }
    }

    #[test]
    fn active_remap_tail_extension_preserves_mapping_and_reopens() {
        const OLD_N: u8 = 8;
        const NEW_N: u8 = OLD_N + 1;
        const OLD_HOME: u64 = (1 << OLD_N) - 1;
        const NEW_HOME: u64 = (1 << NEW_N) - 1;

        let memory = VectorMemory::default();
        let map = StableClusteredHashMap::<u64, u64, _>::new(memory.clone()).expect("new");
        while map.log2_buckets() < OLD_N {
            map.size_up().expect("pre-grow");
            map.finish_resize_initialization_for_setup()
                .expect("finish pre-grow initialization");
            assert!(map.remap_step(u64::MAX).expect("settle pre-grow"));
        }
        map.size_up().expect("start active remap");
        map.finish_resize_initialization_for_setup()
            .expect("finish active-remap initialization");
        let initial_capacity = map.capacity();
        let initial_remap_end = map.remap_end();
        let mut next_key = 0;
        let mut residents = Vec::new();
        for distance in 0..initial_capacity - NEW_HOME {
            let key = next_key_for_old_and_new_home(&mut next_key, OLD_N, OLD_HOME, NEW_HOME);
            map.write_entry(
                NEW_HOME + distance,
                &Entry {
                    key,
                    value: key,
                    distance: checked_distance(distance),
                },
            );
            residents.push(key);
        }
        map.set_len(residents.len() as u64);
        let target = next_key_for_old_and_new_home(&mut next_key, OLD_N, OLD_HOME, NEW_HOME);
        map.insert(target, target).expect("extend active tail");

        assert_eq!(map.buckets(), 1 << NEW_N);
        assert_eq!(map.capacity(), initial_capacity + TAIL_GROWTH_CHUNK);
        assert_eq!(map.remap_end(), initial_remap_end - REMAP_BATCH);
        assert_ne!(map.remap_end(), u64::MAX);
        assert_eq!(map.get(&target), Some(target));
        for slot in initial_capacity + 1..map.capacity() {
            assert!(
                map.is_empty_slot(slot),
                "unused tail slot {slot} is cleared"
            );
        }
        for key in &residents {
            assert_eq!(map.get(key), Some(*key));
        }
        drop(map);

        let reopened = StableClusteredHashMap::<u64, u64, _>::init(memory).expect("reopen");
        assert_eq!(reopened.capacity(), initial_capacity + TAIL_GROWTH_CHUNK);
        assert_ne!(reopened.remap_end(), u64::MAX);
        assert_eq!(reopened.get(&target), Some(target));
        for key in residents {
            assert_eq!(reopened.get(&key), Some(key));
        }
        for slot in initial_capacity + 1..reopened.capacity() {
            assert!(
                reopened.is_empty_slot(slot),
                "unused tail slot {slot} reopens cleared"
            );
        }
    }

    #[test]
    fn tail_grow_oom_keeps_header_and_all_bytes_unchanged() {
        let memory = FailNextGrowMemory::new();
        let map = StableClusteredHashMap::<LargeKey, u64, _>::new(memory.clone()).expect("new");
        let home = map.buckets() - 1;
        let mut next_key = 0;
        let mut residents = Vec::new();
        for value in home..map.capacity() {
            let key = next_large_key_for_home(&mut next_key, home);
            map.insert(key.clone(), value)
                .expect("seed large terminal cluster");
            residents.push((key, value));
        }
        let target = next_large_key_for_home(&mut next_key, home);
        let before_capacity = map.capacity();
        let before_bytes = memory.snapshot();
        memory.fail_next_grow();

        assert_eq!(
            map.insert(target.clone(), 99),
            Err(InsertError::OutOfMemory)
        );
        assert_eq!(map.capacity(), before_capacity);
        assert_eq!(memory.snapshot(), before_bytes);
        assert_eq!(map.get(&target), None);
        for (key, value) in &residents {
            assert_eq!(map.get(key), Some(*value));
        }
    }

    #[test]
    fn settled_threshold_grow_oom_keeps_header_bytes_and_entries_unchanged() {
        let memory = FailNextGrowMemory::new();
        let map = StableClusteredHashMap::<HugeKey, u64, _>::new(memory.clone()).expect("new");
        let threshold = map.resize_threshold();
        let mut residents = Vec::new();
        let mut next_key = 0;
        for slot in 0..threshold {
            let key = next_huge_key_for_home(&mut next_key, map.log2_buckets(), slot);
            map.write_entry(
                slot,
                &Entry {
                    key: key.clone(),
                    value: slot,
                    distance: 0,
                },
            );
            residents.push((key, slot));
        }
        map.set_len(threshold);
        let target = next_huge_key_for_home(&mut next_key, map.log2_buckets(), threshold);
        let before_capacity = map.capacity();
        let before_buckets = map.buckets();
        let before_remap_end = map.remap_end();
        let before_bytes = memory.snapshot();
        memory.fail_next_grow();

        assert_eq!(
            map.insert(target.clone(), 99),
            Err(InsertError::OutOfMemory)
        );
        assert_eq!(map.capacity(), before_capacity);
        assert_eq!(map.buckets(), before_buckets);
        assert_eq!(map.remap_end(), before_remap_end);
        assert_eq!(memory.snapshot(), before_bytes);
        for (key, value) in &residents {
            assert_eq!(map.get(key), Some(*value));
        }
        drop(map);

        let reopened = StableClusteredHashMap::<HugeKey, u64, _>::init(memory).expect("reopen");
        assert_eq!(reopened.capacity(), before_capacity);
        assert_eq!(reopened.buckets(), before_buckets);
        assert_eq!(reopened.remap_end(), before_remap_end);
        for (key, value) in residents {
            assert_eq!(reopened.get(&key), Some(value));
        }
    }

    #[test]
    fn size_up_checked_capacity_overflow_returns_before_writes() {
        let memory = VectorMemory::default();
        let map = StableClusteredHashMap::<u64, u64, _>::new(memory.clone()).expect("new");
        write_u64(&memory, header::CAPACITY_OFFSET, u64::MAX);
        let mut before = vec![0; (memory.size() * crate::memory::WASM_PAGE_SIZE) as usize];
        memory.read(0, &mut before);

        assert_eq!(map.size_up(), Err(InsertError::CapacityOverflow));
        let mut after = vec![0; (memory.size() * crate::memory::WASM_PAGE_SIZE) as usize];
        memory.read(0, &mut after);
        assert_eq!(after, before);
    }

    const TAIL_OOM_OLD_N: u8 = 9;
    const TAIL_OOM_OLD_HOME: u64 = (1 << TAIL_OOM_OLD_N) - 1;
    const TAIL_OOM_NEW_HOME: u64 = (1 << (TAIL_OOM_OLD_N + 1)) - 1;

    struct RemapTailOomFixture {
        memory: FailNextGrowMemory,
        map: StableClusteredHashMap<LargeKey, u64, FailNextGrowMemory>,
        source: LargeKey,
        terminal: Vec<(LargeKey, u64)>,
    }

    fn remap_tail_oom_fixture() -> RemapTailOomFixture {
        let memory = FailNextGrowMemory::new();
        let map = StableClusteredHashMap::<LargeKey, u64, _>::new(memory.clone()).expect("new");
        while map.log2_buckets() < TAIL_OOM_OLD_N {
            map.size_up().expect("pre-grow");
            map.finish_resize_initialization_for_setup()
                .expect("finish pre-grow initialization");
            assert!(map.remap_step(u64::MAX).expect("settle pre-grow"));
        }
        map.size_up().expect("start active remap");
        map.finish_resize_initialization_for_setup()
            .expect("finish active-remap initialization");

        let mut next_key = 0;
        let source = next_large_key_for_old_and_new_home(
            &mut next_key,
            TAIL_OOM_OLD_N,
            TAIL_OOM_OLD_HOME,
            TAIL_OOM_NEW_HOME,
        );
        map.write_entry(
            TAIL_OOM_OLD_HOME,
            &Entry {
                key: source.clone(),
                value: 1,
                distance: 0,
            },
        );
        let mut terminal = Vec::new();
        for distance in 0..map.capacity() - TAIL_OOM_NEW_HOME {
            let key = next_large_key_for_old_and_new_home(
                &mut next_key,
                TAIL_OOM_OLD_N,
                TAIL_OOM_OLD_HOME,
                TAIL_OOM_NEW_HOME,
            );
            map.write_entry(
                TAIL_OOM_NEW_HOME + distance,
                &Entry {
                    key: key.clone(),
                    value: distance + 2,
                    distance: checked_distance(distance),
                },
            );
            terminal.push((key, distance + 2));
        }
        map.set_len(terminal.len() as u64 + 1);
        map.set_remap_end(TAIL_OOM_OLD_HOME);

        RemapTailOomFixture {
            memory,
            map,
            source,
            terminal,
        }
    }

    #[test]
    fn remap_tail_grow_oom_happens_before_source_removal() {
        let RemapTailOomFixture {
            memory,
            map,
            source,
            terminal,
        } = remap_tail_oom_fixture();

        let before_capacity = map.capacity();
        let before_bytes = memory.snapshot();
        memory.fail_next_grow();
        assert_eq!(map.remap_step(1), Err(InsertError::OutOfMemory));
        assert_eq!(map.capacity(), before_capacity);
        assert_eq!(memory.snapshot(), before_bytes);
        assert_eq!(map.get(&source), Some(1));
        for (key, value) in &terminal {
            assert_eq!(map.get(key), Some(*value));
        }
        drop(map);

        let reopened = StableClusteredHashMap::<LargeKey, u64, _>::init(memory).expect("reopen");
        assert_eq!(reopened.capacity(), before_capacity);
        assert_eq!(reopened.get(&source), Some(1));
        for (key, value) in terminal {
            assert_eq!(reopened.get(&key), Some(value));
        }
    }

    struct MultiBoundaryRemapOomFixture {
        memory: FailNextGrowMemory,
        map: StableClusteredHashMap<LargeKey, u64, FailNextGrowMemory>,
        source: LargeKey,
        earlier: LargeKey,
        terminal: Vec<(LargeKey, u64)>,
        next_key: u64,
    }

    fn multi_boundary_remap_oom_fixture() -> MultiBoundaryRemapOomFixture {
        let memory = FailNextGrowMemory::new();
        let map = StableClusteredHashMap::<LargeKey, u64, _>::new(memory.clone()).expect("new");
        while map.log2_buckets() < TAIL_OOM_OLD_N {
            map.size_up().expect("pre-grow");
            map.finish_resize_initialization_for_setup()
                .expect("finish pre-grow initialization");
            assert!(map.remap_step(u64::MAX).expect("settle pre-grow"));
        }
        map.size_up().expect("start active remap");
        map.finish_resize_initialization_for_setup()
            .expect("finish active-remap initialization");

        let mut next_key = 0;
        let source = next_large_key_for_old_and_new_home(
            &mut next_key,
            TAIL_OOM_OLD_N,
            TAIL_OOM_OLD_HOME - 1,
            TAIL_OOM_NEW_HOME - 1,
        );
        let earlier = next_large_key_for_old_and_new_home(
            &mut next_key,
            TAIL_OOM_OLD_N,
            TAIL_OOM_OLD_HOME,
            TAIL_OOM_NEW_HOME,
        );
        map.write_entry(
            TAIL_OOM_OLD_HOME - 1,
            &Entry {
                key: source.clone(),
                value: 1,
                distance: 0,
            },
        );
        map.write_entry(
            TAIL_OOM_OLD_HOME,
            &Entry {
                key: earlier.clone(),
                value: 2,
                distance: 0,
            },
        );

        let mut terminal = Vec::new();
        let anchor = next_large_key_for_old_and_new_home(
            &mut next_key,
            TAIL_OOM_OLD_N,
            TAIL_OOM_OLD_HOME - 1,
            TAIL_OOM_NEW_HOME - 1,
        );
        map.write_entry(
            TAIL_OOM_NEW_HOME - 1,
            &Entry {
                key: anchor.clone(),
                value: 3,
                distance: 0,
            },
        );
        terminal.push((anchor, 3));
        for distance in 0..map.capacity() - TAIL_OOM_NEW_HOME - 1 {
            let key = next_large_key_for_old_and_new_home(
                &mut next_key,
                TAIL_OOM_OLD_N,
                TAIL_OOM_OLD_HOME,
                TAIL_OOM_NEW_HOME,
            );
            map.write_entry(
                TAIL_OOM_NEW_HOME + distance,
                &Entry {
                    key: key.clone(),
                    value: distance + 4,
                    distance: checked_distance(distance),
                },
            );
            terminal.push((key, distance + 4));
        }
        map.set_len(terminal.len() as u64 + 2);
        map.set_remap_end(TAIL_OOM_OLD_HOME);

        MultiBoundaryRemapOomFixture {
            memory,
            map,
            source,
            earlier,
            terminal,
            next_key,
        }
    }

    #[test]
    fn active_remap_bounded_empty_suffix_proves_direct_operation_needs_no_growth() {
        let map = fresh();
        while map.capacity() < REMAP_BATCH + 1 {
            if map.remap_end() != u64::MAX {
                assert!(map.remap_step(u64::MAX).expect("settle pre-grow"));
            }
            map.size_up().expect("grow fixture");
            map.finish_resize_initialization_for_setup()
                .expect("finish grow fixture initialization");
        }
        assert_ne!(map.remap_end(), u64::MAX);
        assert!(map.active_operation_has_bounded_empty_suffix());
        let before_capacity = map.capacity();

        assert_eq!(map.insert(1, 1), Ok(None));
        assert_eq!(map.capacity(), before_capacity);
        assert_eq!(map.get(&1), Some(1));
    }

    #[test]
    fn multi_boundary_active_remap_insert_oom_is_operation_atomic_and_retry_succeeds() {
        let MultiBoundaryRemapOomFixture {
            memory,
            map,
            source,
            earlier,
            terminal,
            mut next_key,
        } = multi_boundary_remap_oom_fixture();
        let requested = next_large_key_for_old_and_new_home(&mut next_key, TAIL_OOM_OLD_N, 0, 0);

        let before_capacity = map.capacity();
        let before_len = map.len();
        let before_buckets = map.buckets();
        let before_remap_end = map.remap_end();
        let before_bytes = memory.snapshot();
        assert!(map.is_empty_slot(before_capacity - 1));
        assert!(
            !map.active_operation_has_bounded_empty_suffix(),
            "one empty terminal slot does not cover multiple relocations"
        );
        assert_eq!(map.get(&earlier), Some(2));
        memory.fail_next_grow();

        assert_eq!(
            map.insert(requested.clone(), 99),
            Err(InsertError::OutOfMemory)
        );
        assert_eq!(memory.snapshot(), before_bytes);
        assert_eq!(map.remap_end(), before_remap_end);
        assert_eq!(map.capacity(), before_capacity);
        assert_eq!(map.len(), before_len);
        assert_eq!(map.buckets(), before_buckets);
        assert_eq!(map.get(&requested), None);
        assert_eq!(map.get(&source), Some(1));
        assert_eq!(map.get(&earlier), Some(2));
        for (key, value) in &terminal {
            assert_eq!(map.get(key), Some(*value));
        }
        drop(map);

        let reopened = StableClusteredHashMap::<LargeKey, u64, _>::init(memory.clone())
            .expect("reopen after OOM");
        assert_eq!(reopened.remap_end(), before_remap_end);
        assert_eq!(reopened.capacity(), before_capacity);
        assert_eq!(reopened.len(), before_len);
        assert_eq!(reopened.buckets(), before_buckets);
        assert_eq!(reopened.get(&requested), None);
        assert_eq!(reopened.get(&source), Some(1));
        assert_eq!(reopened.get(&earlier), Some(2));
        for (key, value) in &terminal {
            assert_eq!(reopened.get(key), Some(*value));
        }

        assert_eq!(reopened.insert(requested.clone(), 99), Ok(None));
        assert!(reopened.capacity() > before_capacity);
        assert_eq!(reopened.len(), before_len + 1);
        assert_eq!(reopened.get(&requested), Some(99));
        assert_eq!(reopened.get(&source), Some(1));
        assert_eq!(reopened.get(&earlier), Some(2));
        for (key, value) in terminal {
            assert_eq!(reopened.get(&key), Some(value));
        }
    }

    #[test]
    fn multi_boundary_active_remap_remove_oom_is_operation_atomic_and_retry_succeeds() {
        let MultiBoundaryRemapOomFixture {
            memory,
            map,
            source,
            earlier,
            terminal,
            ..
        } = multi_boundary_remap_oom_fixture();
        let requested = terminal[0].0.clone();
        let requested_value = terminal[0].1;
        let before_len = map.len();
        let before_buckets = map.buckets();
        let before_capacity = map.capacity();
        let before_remap_end = map.remap_end();
        let before_bytes = memory.snapshot();
        memory.fail_next_grow();

        assert_eq!(map.remove(&requested), Err(InsertError::OutOfMemory));
        assert_eq!(map.len(), before_len);
        assert_eq!(map.buckets(), before_buckets);
        assert_eq!(map.capacity(), before_capacity);
        assert_eq!(map.remap_end(), before_remap_end);
        assert_eq!(memory.snapshot(), before_bytes);
        assert_eq!(map.get(&requested), Some(requested_value));
        assert_eq!(map.get(&source), Some(1));
        assert_eq!(map.get(&earlier), Some(2));
        for (key, value) in &terminal {
            assert_eq!(map.get(key), Some(*value));
        }
        drop(map);

        let reopened = StableClusteredHashMap::<LargeKey, u64, _>::init(memory.clone())
            .expect("reopen after remove OOM");
        assert_eq!(reopened.len(), before_len);
        assert_eq!(reopened.buckets(), before_buckets);
        assert_eq!(reopened.capacity(), before_capacity);
        assert_eq!(reopened.remap_end(), before_remap_end);
        assert_eq!(reopened.get(&requested), Some(requested_value));
        assert_eq!(reopened.get(&source), Some(1));
        assert_eq!(reopened.get(&earlier), Some(2));
        for (key, value) in &terminal {
            assert_eq!(reopened.get(key), Some(*value));
        }

        assert_eq!(reopened.remove(&requested), Ok(Some(requested_value)));
        assert_eq!(reopened.len(), before_len - 1);
        assert_eq!(reopened.get(&requested), None);
        assert_eq!(reopened.get(&source), Some(1));
        assert_eq!(reopened.get(&earlier), Some(2));
        for (key, value) in terminal.into_iter().skip(1) {
            assert_eq!(reopened.get(&key), Some(value));
        }
    }

    #[test]
    fn active_threshold_insert_advances_bounded_remap_without_bucket_growth() {
        let map = fresh();
        map.size_up().expect("start remap");
        let n = map.log2_buckets();
        let threshold = map.resize_threshold();
        let mut next_key = 0;
        for home in 0..threshold {
            let key = next_key_for_home(&mut next_key, n, home);
            map.write_entry(
                home,
                &Entry {
                    key,
                    value: key,
                    distance: 0,
                },
            );
        }
        map.set_len(threshold);
        let target = next_key_for_home(&mut next_key, n, threshold);
        let before_remap_end = map.remap_end();
        let before_buckets = map.buckets();
        let before_capacity = map.capacity();

        map.insert(target, target)
            .expect("active threshold insert makes bounded progress");
        assert!(map.remap_end() < before_remap_end || map.remap_end() == u64::MAX);
        assert_eq!(map.buckets(), before_buckets);
        assert_eq!(map.capacity(), before_capacity);
        assert_eq!(map.get(&target), Some(target));
    }

    #[test]
    fn active_insert_crossing_boundary_expands_remap_end_and_reopens() {
        const OLD_N: u8 = 9;
        const NEW_N: u8 = OLD_N + 1;
        const TARGET_HOME: u64 = 450;
        const NEXT_HOME: u64 = TARGET_HOME + 1;

        let memory = VectorMemory::default();
        let map = StableClusteredHashMap::<u64, u64, _>::new(memory.clone()).expect("new");
        while map.log2_buckets() < OLD_N {
            map.size_up().expect("pre-grow");
            map.finish_resize_initialization_for_setup()
                .expect("finish pre-grow initialization");
            assert!(map.remap_step(u64::MAX).expect("settle pre-grow"));
        }
        map.size_up().expect("start active remap");
        map.finish_resize_initialization_for_setup()
            .expect("finish active-remap initialization");

        let mut next_key = 0;
        let resident = next_key_for_home(&mut next_key, NEW_N, TARGET_HOME);
        let next_a = next_key_for_home(&mut next_key, NEW_N, NEXT_HOME);
        let next_b = next_key_for_home(&mut next_key, NEW_N, NEXT_HOME);
        let target = next_key_for_home(&mut next_key, NEW_N, TARGET_HOME);
        map.write_entry(
            TARGET_HOME,
            &Entry {
                key: resident,
                value: resident,
                distance: 0,
            },
        );
        for (slot, key, distance) in [(NEXT_HOME, next_a, 0), (NEXT_HOME + 1, next_b, 1)] {
            map.write_entry(
                slot,
                &Entry {
                    key,
                    value: key,
                    distance,
                },
            );
        }
        map.set_len(3);

        map.insert(target, target)
            .expect("insert relocates across active boundary");
        assert_eq!(map.remap_end(), NEXT_HOME + 2);
        for key in [resident, next_a, next_b, target] {
            assert_eq!(map.get(&key), Some(key));
        }
        drop(map);

        let reopened = StableClusteredHashMap::<u64, u64, _>::init(memory).expect("reopen");
        assert_eq!(reopened.remap_end(), NEXT_HOME + 2);
        for key in [resident, next_a, next_b, target] {
            assert_eq!(reopened.get(&key), Some(key));
        }
    }

    #[test]
    fn load_threshold_resize_allocates_the_canonical_entry_stride() {
        const TARGET_LOG2_BUCKETS: u8 = 13;

        let memory = VectorMemory::default();
        let map = StableClusteredHashMap::<u64, u64, _>::new(memory.clone()).expect("new");
        while map.log2_buckets() < TARGET_LOG2_BUCKETS {
            map.size_up().expect("pre-grow fixture");
            map.finish_resize_initialization_for_setup()
                .expect("finish pre-grow fixture initialization");
            assert!(map.remap_step(u64::MAX).expect("settle pre-grow"));
        }

        let threshold = map.resize_threshold();
        let mut keys_by_bucket = vec![None; threshold as usize + 1];
        let mut missing = keys_by_bucket.len();
        for candidate in 0u64.. {
            let home = bucket(hash_key(&candidate.to_bytes()), TARGET_LOG2_BUCKETS);
            if home <= threshold && keys_by_bucket[home as usize].is_none() {
                keys_by_bucket[home as usize] = Some(candidate);
                missing -= 1;
                if missing == 0 {
                    break;
                }
            }
        }

        for (slot, key) in keys_by_bucket[..threshold as usize].iter().enumerate() {
            let key = key.expect("key for each occupied home bucket");
            map.write_entry(
                slot as u64,
                &Entry {
                    key,
                    value: key,
                    distance: 0,
                },
            );
        }
        map.set_len(threshold);
        assert!(map.is_full(), "fixture reaches the normal load threshold");
        assert_eq!(memory.size(), 3, "fixture has minimal-page backing");
        let tail_reserve = map.capacity() - map.buckets();

        let trigger_key = keys_by_bucket[threshold as usize].expect("next home bucket key");
        map.insert(trigger_key, trigger_key)
            .expect("threshold insert grows the backing before clearing the new region");

        assert_eq!(map.log2_buckets(), TARGET_LOG2_BUCKETS);
        assert_ne!(map.resize_state(), header::ResizeState::Settled);
        map.finish_resize_initialization_for_setup()
            .expect("finish threshold resize");
        let required_bytes = DATA_OFFSET + map.capacity() * map.entry_stride();
        assert_eq!(map.log2_buckets(), TARGET_LOG2_BUCKETS + 1);
        assert_eq!(map.capacity() - map.buckets(), tail_reserve);
        assert_eq!(map.len(), threshold + 1);
        assert!(
            memory.size() * crate::memory::WASM_PAGE_SIZE >= required_bytes,
            "grown backing covers every canonical entry"
        );
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
    fn iter_from_resumes_at_slot() {
        let map = fresh();
        for k in 0..100u64 {
            map.insert(k, k).unwrap();
        }
        // `iter_from(slot)` scans physical slots >= slot, so it yields a suffix of the full
        // slot-ordered iteration (empty slots are skipped in both).
        let full: Vec<(u64, u64)> = map.iter().collect();
        let resume_slot = 40;
        let tail: Vec<(u64, u64)> = map.iter_from(resume_slot).collect();
        assert!(
            full.ends_with(&tail),
            "resumed tail must be a suffix of the full iteration"
        );
        assert!(
            tail.len() < full.len(),
            "resuming mid-table must skip the prefix"
        );
        // Resuming from the end yields nothing.
        assert!(map.iter_from(map.capacity()).next().is_none());
    }

    #[test]
    fn clear_new_resets_to_empty() {
        let mut map = fresh();
        for k in 0..200u64 {
            map.insert(k, k).unwrap();
        }
        assert!(map.buckets() > 8, "resized beyond the initial 8 buckets");
        map.clear_new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
        assert_eq!(map.get(&0), None);
        // The map is usable again after the reset.
        map.insert(1, 10).unwrap();
        assert_eq!(map.get(&1), Some(10));
        assert_eq!(map.len(), 1);
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
                let got = map.remove(&k).expect("remove from clustered map");
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

    #[test]
    fn incremental_resize_preserves_all_entries() {
        let map = fresh();
        for k in 0..5000u64 {
            map.insert(k, k * 7).unwrap();
        }
        assert!(map.buckets() > 8, "resized beyond the initial 8 buckets");
        for k in 0..5000u64 {
            assert_eq!(map.get(&k), Some(k * 7), "key {k} reachable");
        }
        assert_eq!(map.len(), 5000);
        assert_eq!(map.remap_end(), u64::MAX, "resize completed");
    }

    #[test]
    fn mixed_range_lookup_finds_items_during_resize() {
        let map = fresh();
        let mut saw_resize = false;
        for k in 0..5000u64 {
            map.insert(k, k).unwrap();
            if map.remap_end() != u64::MAX {
                saw_resize = true;
                // Lookups must find every entry while the resize is in progress.
                for j in 0..=k {
                    assert_eq!(map.get(&j), Some(j), "key {j} found mid-resize");
                }
                break;
            }
        }
        assert!(saw_resize, "a resize was observed in progress");
    }

    #[test]
    fn upgrade_persistence_reopens_mid_resize() {
        let mm = VectorMemory::default();
        let mut inserted = 0u64;
        {
            let map = StableClusteredHashMap::<u64, u64, _>::new(mm.clone()).expect("new");
            for k in 0..5000u64 {
                map.insert(k, k).unwrap();
                inserted = k + 1;
                if map.remap_end() != u64::MAX {
                    break;
                }
            }
            assert_ne!(map.remap_end(), u64::MAX, "resize in progress");
        }
        let map = StableClusteredHashMap::<u64, u64, _>::init(mm).expect("reopen");
        assert_eq!(map.len(), inserted);
        for k in 0..inserted {
            assert_eq!(
                map.get(&k),
                Some(k),
                "key {k} reachable after reopen mid-resize"
            );
        }
        // Drive the remap to completion.
        for k in inserted..inserted + 200 {
            map.insert(k, k).unwrap();
        }
        assert_eq!(map.remap_end(), u64::MAX, "resize completed after reopen");
        for k in 0..inserted + 200 {
            assert_eq!(map.get(&k), Some(k));
        }
    }

    #[test]
    fn threshold_resize_clears_a_bounded_prefix_and_reopens() {
        const LOG2_BUCKETS: u8 = 8;

        let memory = VectorMemory::default();
        let map = StableClusteredHashMap::<u64, u64, _>::new(memory.clone()).expect("new");
        while map.log2_buckets() < LOG2_BUCKETS {
            map.size_up().expect("pre-grow");
            map.finish_resize_initialization_for_setup()
                .expect("finish pre-grow initialization");
            assert!(map.remap_step(u64::MAX).expect("settle pre-grow"));
        }

        let old_capacity = map.capacity();
        let threshold = map.resize_threshold();
        let mut next_key = 0;
        let mut keys = vec![None; threshold as usize + 1];
        let mut missing = keys.len();
        for candidate in 0u64.. {
            let home = bucket(hash_key(&candidate.to_bytes()), LOG2_BUCKETS);
            if home <= threshold && keys[home as usize].is_none() {
                keys[home as usize] = Some(candidate);
                missing -= 1;
                if missing == 0 {
                    break;
                }
            }
        }
        for (slot, key) in keys[..threshold as usize].iter().enumerate() {
            let key = key.expect("resident key");
            map.write_entry(
                slot as u64,
                &Entry {
                    key,
                    value: key,
                    distance: 0,
                },
            );
        }
        map.set_len(threshold);

        let target = keys[threshold as usize].expect("threshold target");
        map.insert(target, target).expect("start pending resize");
        assert_eq!(map.log2_buckets(), LOG2_BUCKETS);
        assert_eq!(map.remap_end(), u64::MAX);
        assert_eq!(map.resize_state(), header::ResizeState::Clearing);
        assert_eq!(map.capacity(), old_capacity + RESIZE_CLEAR_BATCH);
        assert_eq!(map.resize_cursor(), map.capacity());
        assert_eq!(
            map.resize_target_capacity(),
            old_capacity + (1 << LOG2_BUCKETS)
        );
        for slot in old_capacity..map.capacity() {
            assert!(map.is_empty_slot(slot), "clear prefix slot {slot}");
        }
        drop(map);

        let reopened = StableClusteredHashMap::<u64, u64, _>::init(memory.clone()).expect("reopen");
        assert_eq!(reopened.resize_state(), header::ResizeState::Clearing);
        assert_eq!(reopened.capacity(), old_capacity + RESIZE_CLEAR_BATCH);
        assert_eq!(reopened.get(&target), Some(target));

        let progress_before = reopened.capacity();
        let next = next_key_for_home(&mut next_key, LOG2_BUCKETS, 0);
        reopened.insert(next, next).expect("advance pending resize");
        assert!(reopened.capacity() > progress_before);
        assert_eq!(reopened.get(&target), Some(target));
        assert_eq!(reopened.get(&next), Some(next));
    }

    #[test]
    fn pending_resize_clear_oom_rolls_back_cursor_and_reopens() {
        const LOG2_BUCKETS: u8 = 8;

        let memory = FailNextGrowMemory::new();
        let map = StableClusteredHashMap::<LargeKey, u64, _>::new(memory.clone()).expect("new");
        while map.log2_buckets() < LOG2_BUCKETS {
            map.size_up().expect("pre-grow");
            map.finish_resize_initialization_for_setup()
                .expect("finish pre-grow initialization");
            assert!(map.remap_step(u64::MAX).expect("settle pre-grow"));
        }

        let threshold = map.resize_threshold();
        let mut next_key = 0;
        let mut residents = Vec::with_capacity(threshold as usize);
        for home in 0..threshold {
            let key = next_large_key_for_home_at(&mut next_key, LOG2_BUCKETS, home);
            map.write_entry(
                home,
                &Entry {
                    key: key.clone(),
                    value: home,
                    distance: 0,
                },
            );
            residents.push((key, home));
        }
        map.set_len(threshold);
        let target = next_large_key_for_home_at(&mut next_key, LOG2_BUCKETS, threshold);
        map.insert(target.clone(), 99)
            .expect("start pending resize");
        let before_capacity = map.capacity();
        let before_cursor = map.resize_cursor();
        let before_bytes = memory.snapshot();
        let retry = next_large_key_for_home_at(&mut next_key, LOG2_BUCKETS, 0);
        memory.fail_next_grow();

        assert_eq!(
            map.insert(retry.clone(), 100),
            Err(InsertError::OutOfMemory)
        );
        assert_eq!(map.capacity(), before_capacity);
        assert_eq!(map.resize_cursor(), before_cursor);
        assert_eq!(memory.snapshot(), before_bytes);
        assert_eq!(map.get(&target), Some(99));
        assert_eq!(map.get(&retry), None);
        for (key, value) in &residents {
            assert_eq!(map.get(key), Some(*value));
        }
        drop(map);

        let reopened = StableClusteredHashMap::<LargeKey, u64, _>::init(memory).expect("reopen");
        assert_eq!(reopened.capacity(), before_capacity);
        assert_eq!(reopened.resize_cursor(), before_cursor);
        assert_eq!(reopened.get(&target), Some(99));
        assert_eq!(reopened.get(&retry), None);
    }

    #[test]
    fn publishing_resize_marker_reopens_and_finishes_metadata_commit() {
        const LOG2_BUCKETS: u8 = 8;

        let memory = VectorMemory::default();
        let map = StableClusteredHashMap::<u64, u64, _>::new(memory.clone()).expect("new");
        while map.log2_buckets() < LOG2_BUCKETS {
            map.size_up().expect("pre-grow");
            map.finish_resize_initialization_for_setup()
                .expect("finish pre-grow initialization");
            assert!(map.remap_step(u64::MAX).expect("settle pre-grow"));
        }
        let old_capacity = map.capacity();
        let threshold = map.resize_threshold();
        let mut keys = vec![None; threshold as usize + 1];
        let mut missing = keys.len();
        for candidate in 0u64.. {
            let home = bucket(hash_key(&candidate.to_bytes()), LOG2_BUCKETS);
            if home <= threshold && keys[home as usize].is_none() {
                keys[home as usize] = Some(candidate);
                missing -= 1;
                if missing == 0 {
                    break;
                }
            }
        }
        for (slot, key) in keys[..threshold as usize].iter().enumerate() {
            let key = key.expect("resident key");
            map.write_entry(
                slot as u64,
                &Entry {
                    key,
                    value: key,
                    distance: 0,
                },
            );
        }
        map.set_len(threshold);
        let target = keys[threshold as usize].expect("threshold target");
        map.insert(target, target).expect("start pending resize");
        let target_capacity = map.resize_target_capacity();
        let target_log2 = map.resize_target_log2();
        map.finish_resize_initialization_for_setup()
            .expect("clear target region");

        write_u64(&memory, header::CAPACITY_OFFSET, old_capacity);
        write_u64(
            &memory,
            header::RESIZE_TARGET_CAPACITY_OFFSET,
            target_capacity,
        );
        write_u64(&memory, header::RESIZE_CURSOR_OFFSET, target_capacity);
        write_u8(&memory, header::RESIZE_TARGET_LOG2_OFFSET, target_log2);
        write_u64(&memory, header::RESIZE_REMAP_START_OFFSET, old_capacity);
        write_u8(
            &memory,
            header::RESIZE_STATE_OFFSET,
            header::ResizeState::Publishing as u8,
        );
        write_u8(&memory, header::LOG2_BUCKETS_OFFSET, LOG2_BUCKETS);
        write_u64(&memory, header::REMAP_END_OFFSET, u64::MAX);
        drop(map);

        let reopened = StableClusteredHashMap::<u64, u64, _>::init(memory).expect("reopen");
        assert_eq!(reopened.resize_state(), header::ResizeState::Settled);
        assert_eq!(reopened.log2_buckets(), target_log2);
        assert_eq!(reopened.capacity(), target_capacity);
        assert_eq!(reopened.remap_end(), old_capacity);
        assert_eq!(reopened.get(&target), Some(target));
    }

    #[test]
    fn clear_new_aborts_pending_resize_and_reopens_settled() {
        const LOG2_BUCKETS: u8 = 8;

        let memory = VectorMemory::default();
        let mut map = StableClusteredHashMap::<u64, u64, _>::new(memory.clone()).expect("new");
        while map.log2_buckets() < LOG2_BUCKETS {
            map.size_up().expect("pre-grow");
            map.finish_resize_initialization_for_setup()
                .expect("finish pre-grow initialization");
            assert!(map.remap_step(u64::MAX).expect("settle pre-grow"));
        }
        let old_capacity = map.capacity();
        let threshold = map.resize_threshold();
        let mut keys = vec![None; threshold as usize + 1];
        let mut missing = keys.len();
        for candidate in 0u64.. {
            let home = bucket(hash_key(&candidate.to_bytes()), LOG2_BUCKETS);
            if home <= threshold && keys[home as usize].is_none() {
                keys[home as usize] = Some(candidate);
                missing -= 1;
                if missing == 0 {
                    break;
                }
            }
        }
        for (slot, key) in keys[..threshold as usize].iter().enumerate() {
            let key = key.expect("resident key");
            map.write_entry(
                slot as u64,
                &Entry {
                    key,
                    value: key,
                    distance: 0,
                },
            );
        }
        map.set_len(threshold);
        let target = keys[threshold as usize].expect("threshold target");
        map.insert(target, target).expect("start pending resize");
        assert_ne!(map.resize_state(), header::ResizeState::Settled);

        map.clear_new();
        assert_eq!(map.resize_state(), header::ResizeState::Settled);
        assert_eq!(map.capacity(), old_capacity);
        assert_eq!(map.len(), 0);
        assert_eq!(map.remap_end(), u64::MAX);
        drop(map);

        let reopened = StableClusteredHashMap::<u64, u64, _>::init(memory).expect("reopen");
        assert_eq!(reopened.resize_state(), header::ResizeState::Settled);
        assert_eq!(reopened.capacity(), old_capacity);
        assert!(reopened.is_empty());
    }

    #[test]
    fn fuzz_interleaved_resize_insert_remove() {
        use std::collections::HashMap;
        let map = fresh();
        let mut ref_map: HashMap<u64, u64> = HashMap::new();
        let mut seed = 0x123456789ABCDEF0u64;
        for _ in 0..5000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let k = seed % 2000;
            let v = seed;
            if seed.is_multiple_of(4) {
                let got = map.remove(&k).expect("remove from clustered map");
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
            assert_eq!(map.get(k), Some(*v), "key {k} survives interleaved resize");
        }
    }
}
