//! Packed small-run store for per-bucket spill (ADR 0097, slices 1–2).
//!
//! A spill run is a contiguous block of fixed-width rows owned by a single bucket. Runs are
//! allocated in **power-of-two capacity classes** with a **free list per class** — the shape the
//! reference implementation measured (`bucket-run-allocation`, `bucket-tail-growth`): capacity is
//! the smallest power of two that fits, a retired run is reusable only within its own class, and
//! the first four bytes of a free run hold the next free run's id (`u32::MAX` terminates).
//!
//! # Run header (ADR 0097 §3)
//!
//! A run occupies **one header row plus its class of data rows**: the run id addresses the header,
//! and data row `i` lives at arena offset `run + 1 + i`. Classes count *data* rows, so a 74-row
//! spill still takes class 128. The header word carries the **used length** while the run is live
//! and the free-list link while it is free — one word, two meanings, exactly one of them true at a
//! time. That is why the descriptor stores only the run id: the length is the store's, and because
//! a spill is append-only with in-place tombstones its used length is monotone, so `class_rows(len)`
//! always names the class the run was allocated with.
//!
//! Why not the LTB store: its payload is a fixed 4096 bytes (1024 rows), so a five-row spill would
//! occupy a 1024-row granule. The smallest class here is [`MIN_ROWS`] rows, measured against the
//! production-shaped hub where the only spilling bucket held 74 rows
//! (`build_mixed_label_hub(20, 500)`: 1 of 20 buckets spilled, 74 of 10 000 rows).
//!
//! The spill tops out at [`MAX_ROWS`]; a bucket that needs more promotes to tree mode (ADR 0097 §6),
//! so there is no level 2 here and no block list to format.

#![allow(
    dead_code,
    reason = "ADR 0097 slices 1-2: the store is measured and tested in isolation; the graph wires\n              its consumers in the following slices (descriptor reinterpretation, insert producer)."
)]

use crate::types::Address;
use crate::{GrowFailed, Memory, read_u32, safe_write};

/// Header size in bytes (magic, version, geometry, per-class free heads).
pub(crate) const HEADER_SIZE: u64 = 64;
/// Magic bytes identifying this store.
pub(crate) const MAGIC: [u8; 3] = *b"SPR";
/// Layout version byte stored immediately after [`MAGIC`].
///
/// Bumped to 2 by ADR 0097 §3: every run now carries a header row, so row addressing and the
/// class unit changed. Pre-production, so there is no reader for version 1 (fresh state).
pub(crate) const LAYOUT_VERSION: u8 = 2;
/// Header rows reserved at the start of every run.
pub(crate) const RUN_HEADER_ROWS: u32 = 1;
/// Smallest run class in data rows.
pub(crate) const MIN_ROWS: u32 = 8;
/// Largest run class; a bucket that needs more data rows promotes to tree mode (ADR 0097 §6).
pub(crate) const MAX_ROWS: u32 = 1024;
/// Number of capacity classes: 8, 16, 32, 64, 128, 256, 512, 1024.
pub(crate) const CLASS_COUNT: usize = 8;
/// Sentinel for "no free run" / "no link".
const NONE: u32 = u32::MAX;
/// Offsets inside the 64-byte store header.
const OFFSET_MAGIC: u64 = 0;
const OFFSET_VERSION: u64 = 3;
const OFFSET_ROW_BYTES: u64 = 4;
const OFFSET_ARENA_ROWS: u64 = 8;
const OFFSET_FREE_HEADS: u64 = 16;

/// Row offset of a run's header inside the arena. The header's data rows follow it.
pub(crate) type RunId = u32;

/// A store of small spill runs over one stable memory.
pub(crate) struct SpillRunStore<M: Memory> {
    memory: M,
    row_bytes: u32,
}

impl<M: Memory> SpillRunStore<M> {
    /// Opens the store, initialising the header when the memory is fresh. A header with the wrong
    /// magic or version is an invariant violation and panics (fail-closed, matching the LTB store).
    pub(crate) fn new(memory: M, row_bytes: u32) -> Result<Self, GrowFailed> {
        assert!(
            row_bytes >= 4,
            "a spill row must hold the 4-byte run header word"
        );
        if memory.size() == 0 {
            let mut header = [0u8; HEADER_SIZE as usize];
            header[..3].copy_from_slice(&MAGIC);
            header[OFFSET_VERSION as usize] = LAYOUT_VERSION;
            header[OFFSET_ROW_BYTES as usize..OFFSET_ROW_BYTES as usize + 4]
                .copy_from_slice(&row_bytes.to_le_bytes());
            for i in 0..CLASS_COUNT {
                let at = OFFSET_FREE_HEADS as usize + i * 4;
                header[at..at + 4].copy_from_slice(&NONE.to_le_bytes());
            }
            safe_write(&memory, 0, &header)?;
            return Ok(Self { memory, row_bytes });
        }
        let header = read_header(&memory);
        assert_eq!(&header[..3], &MAGIC, "spill store magic mismatch");
        assert_eq!(
            header[OFFSET_VERSION as usize], LAYOUT_VERSION,
            "spill store version mismatch"
        );
        let stored = u32::from_le_bytes(
            header[OFFSET_ROW_BYTES as usize..OFFSET_ROW_BYTES as usize + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(stored, row_bytes, "spill store row width mismatch");
        Ok(Self { memory, row_bytes })
    }

    /// Capacity class for a run that must hold `len` data rows: the smallest power of two in
    /// `[MIN_ROWS, MAX_ROWS]`.
    pub(crate) fn class_rows(len: u32) -> u32 {
        let mut rows = MIN_ROWS;
        while rows < len && rows < MAX_ROWS {
            rows *= 2;
        }
        rows
    }

    fn class_index(capacity_rows: u32) -> usize {
        debug_assert!(capacity_rows.is_power_of_two());
        (capacity_rows.trailing_zeros() - MIN_ROWS.trailing_zeros()) as usize
    }

    /// Address of a run's header row.
    fn header_address(&self, run: RunId) -> u64 {
        HEADER_SIZE + u64::from(run) * u64::from(self.row_bytes)
    }

    /// Address of data row `index` of `run`.
    fn data_address(&self, run: RunId, index: u32) -> u64 {
        HEADER_SIZE
            + (u64::from(run) + u64::from(RUN_HEADER_ROWS) + u64::from(index))
                * u64::from(self.row_bytes)
    }

    /// Number of arena rows handed out so far, header rows included.
    pub(crate) fn arena_rows(&self) -> u32 {
        read_u32(&self.memory, Address::from(OFFSET_ARENA_ROWS))
    }

    fn free_head(&self, class: usize) -> RunId {
        read_u32(
            &self.memory,
            Address::from(OFFSET_FREE_HEADS + (class as u64) * 4),
        )
    }

    fn set_free_head(&self, class: usize, head: RunId) -> Result<(), GrowFailed> {
        safe_write(
            &self.memory,
            OFFSET_FREE_HEADS + (class as u64) * 4,
            &head.to_le_bytes(),
        )
    }

    /// Allocates a run of the class that holds `len` data rows and records `len` as its used
    /// length, reusing a free run of the same class when one exists and bump-allocating from the
    /// arena otherwise.
    pub(crate) fn allocate(&self, len: u32) -> Result<RunId, GrowFailed> {
        assert!(
            len > 0 && len <= MAX_ROWS,
            "spill length {len} outside the store's classes"
        );
        let capacity = Self::class_rows(len);
        let class = Self::class_index(capacity);
        let head = self.free_head(class);
        let run = if head != NONE {
            // A free run's header word is its free-list link, not a length.
            let next = read_u32(&self.memory, Address::from(self.header_address(head)));
            self.set_free_head(class, next)?;
            head
        } else {
            let start = self.arena_rows();
            let end = start
                .checked_add(capacity + RUN_HEADER_ROWS)
                .expect("spill arena overflow");
            safe_write(&self.memory, OFFSET_ARENA_ROWS, &end.to_le_bytes())?;
            start
        };
        self.set_used_len(run, len)?;
        // Touch the last data row so the backing is grown even before the caller writes it.
        let last = self.data_address(run, capacity - 1);
        safe_write(&self.memory, last, &[0u8; 1])?;
        Ok(run)
    }

    /// Used length of a live run, read from its header row.
    pub(crate) fn read_used_len(&self, run: RunId) -> u32 {
        read_u32(&self.memory, Address::from(self.header_address(run)))
    }

    /// Writes the header's used-length word. Private to the store because the used length may only
    /// move through the operations that keep `class_rows(len)` equal to the allocated class.
    fn set_used_len(&self, run: RunId, len: u32) -> Result<(), GrowFailed> {
        safe_write(&self.memory, self.header_address(run), &len.to_le_bytes())
    }

    /// Data rows a live run's class can hold, derived from its used length.
    fn capacity(&self, run: RunId) -> u32 {
        Self::class_rows(self.read_used_len(run).max(1))
    }

    /// Grows a run in place when it is the arena tail. ADR 0097 makes this a requirement rather
    /// than an optimisation: without it every class change copies up to 2x the live rows, which an
    /// append-only log never had to do.
    ///
    /// `from_len` is the run's current used length (its class follows from it, because a spill's
    /// used length is monotone) and `to_len` the length it must now hold. Returns `Ok(Some(run))`
    /// when the run was extended in place, `Ok(None)` when the caller must allocate, copy and
    /// release, and `Err` when the backing could not grow.
    pub(crate) fn grow_in_place(
        &self,
        run: RunId,
        from_len: u32,
        to_len: u32,
    ) -> Result<Option<RunId>, GrowFailed> {
        assert!(run != NONE, "cannot grow the sentinel run");
        let current = Self::class_rows(from_len.max(1));
        let target = Self::class_rows(to_len.max(1));
        if target <= current || target > MAX_ROWS {
            // Nothing to grow into, or the store tops out: a larger bucket is a tree bucket.
            return Ok(None);
        }
        let arena = self.arena_rows();
        if run.checked_add(RUN_HEADER_ROWS + current) != Some(arena) {
            // Not the tail: another run follows, so growing here would collide with it.
            return Ok(None);
        }
        let end = arena
            .checked_add(target - current)
            .expect("spill arena overflow");
        safe_write(&self.memory, OFFSET_ARENA_ROWS, &end.to_le_bytes())?;
        self.set_used_len(run, to_len)?;
        let last = self.data_address(run, target - 1);
        safe_write(&self.memory, last, &[0u8; 1])?;
        Ok(Some(run))
    }

    /// Returns a run to its class's free list, turning its header word into the free-list link.
    /// `len` is the used length; the class follows from it, which is why the descriptor only needs
    /// the run id.
    pub(crate) fn release(&self, run: RunId, len: u32) -> Result<(), GrowFailed> {
        assert!(run != NONE, "cannot release the sentinel run");
        let capacity = Self::class_rows(len.max(1));
        let class = Self::class_index(capacity);
        let head = self.free_head(class);
        safe_write(&self.memory, self.header_address(run), &head.to_le_bytes())?;
        self.set_free_head(class, run)
    }

    /// Reads data row `index` of `run` into `row`.
    pub(crate) fn read_row(&self, run: RunId, index: u32, row: &mut [u8]) {
        assert_eq!(
            row.len(),
            self.row_bytes as usize,
            "spill row width mismatch"
        );
        assert!(
            index < self.capacity(run),
            "spill read outside the run's class"
        );
        self.memory.read(self.data_address(run, index), row);
    }

    /// Writes `row` as data row `index` of `run`, overwriting in place (a tombstone hole or a slot
    /// the caller just claimed). The used length does not change.
    pub(crate) fn write_row(&self, run: RunId, index: u32, row: &[u8]) -> Result<(), GrowFailed> {
        assert_eq!(
            row.len(),
            self.row_bytes as usize,
            "spill row width mismatch"
        );
        assert!(
            index < self.capacity(run),
            "spill write outside the run's class"
        );
        safe_write(&self.memory, self.data_address(run, index), row)
    }
}

fn read_header<M: Memory>(memory: &M) -> [u8; HEADER_SIZE as usize] {
    let mut header = [0u8; HEADER_SIZE as usize];
    memory.read(0, &mut header);
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SpillRunStore<crate::VectorMemory> {
        SpillRunStore::new(crate::test_support::vector_memory(), 4).expect("store")
    }

    #[test]
    fn classes_are_powers_of_two_bounded_by_the_two_ends() {
        assert_eq!(
            SpillRunStore::<crate::VectorMemory>::class_rows(1),
            MIN_ROWS
        );
        assert_eq!(SpillRunStore::<crate::VectorMemory>::class_rows(5), 8);
        assert_eq!(SpillRunStore::<crate::VectorMemory>::class_rows(74), 128);
        assert_eq!(SpillRunStore::<crate::VectorMemory>::class_rows(1024), 1024);
        assert_eq!(
            SpillRunStore::<crate::VectorMemory>::class_rows(4096),
            MAX_ROWS
        );
    }

    #[test]
    fn rows_round_trip_within_a_run() {
        let store = store();
        let run = store.allocate(74).expect("allocate");
        assert_eq!(store.read_used_len(run), 74);
        for i in 0..74u32 {
            store.write_row(run, i, &i.to_le_bytes()).expect("write");
        }
        for i in 0..74u32 {
            let mut buf = [0u8; 4];
            store.read_row(run, i, &mut buf);
            assert_eq!(u32::from_le_bytes(buf), i);
        }
    }

    #[test]
    fn a_run_owns_one_header_row_plus_its_class() {
        let store = store();
        // Class counts data rows, but the arena also pays one header row per run.
        let first = store.allocate(74).expect("first");
        assert_eq!(first, 0);
        assert_eq!(store.arena_rows(), 128 + RUN_HEADER_ROWS);
        // The next run starts after the first run's header and all 128 data rows.
        let second = store.allocate(8).expect("second");
        assert_eq!(second, 128 + RUN_HEADER_ROWS);
        assert_eq!(
            store.arena_rows(),
            128 + RUN_HEADER_ROWS + 8 + RUN_HEADER_ROWS
        );
    }

    #[test]
    fn the_header_word_is_a_length_while_live_and_a_link_while_free() {
        let store = store();
        let run = store.allocate(5).expect("allocate");
        assert_eq!(store.read_used_len(run), 5);
        store.write_row(run, 0, &7u32.to_le_bytes()).expect("write");
        store.release(run, 5).expect("release");
        // A released run's header is the free-list terminator, not a length.
        assert_eq!(store.free_head(0), run);
        assert_eq!(store.read_used_len(run), NONE);
        // Reuse rewrites it as the new run's used length.
        let again = store.allocate(3).expect("reuse");
        assert_eq!(again, run);
        assert_eq!(store.read_used_len(again), 3);
    }

    #[test]
    fn a_released_run_is_reused_by_its_own_class_only() {
        let store = store();
        let small = store.allocate(5).expect("small");
        let big = store.allocate(200).expect("big");
        assert_ne!(small, big);
        store.release(small, 5).expect("release");
        // A run of another class does not take the freed eight-row run.
        let other = store.allocate(300).expect("other");
        assert_ne!(other, small);
        // The freed class gets it back exactly.
        let again = store.allocate(6).expect("again");
        assert_eq!(again, small);
        let _ = big;
    }

    #[test]
    fn the_arena_grows_by_the_class_capacity_plus_the_header() {
        let store = store();
        let before = store.arena_rows();
        let _ = store.allocate(74).expect("allocate");
        assert_eq!(store.arena_rows(), before + 128 + RUN_HEADER_ROWS);
        let _ = store.allocate(9).expect("allocate");
        assert_eq!(
            store.arena_rows(),
            before + 128 + RUN_HEADER_ROWS + 16 + RUN_HEADER_ROWS
        );
    }

    #[test]
    fn a_tail_run_grows_in_place_and_others_do_not() {
        let store = store();
        let tail = store.allocate(5).expect("tail");
        assert_eq!(store.arena_rows(), MIN_ROWS + RUN_HEADER_ROWS);
        // The tail run takes the next class in place: same id, used length published, arena
        // extended by the class difference.
        assert_eq!(store.grow_in_place(tail, 5, 9).expect("grow"), Some(tail));
        assert_eq!(store.arena_rows(), MIN_ROWS * 2 + RUN_HEADER_ROWS);
        assert_eq!(store.read_used_len(tail), 9);
        // With a run that follows, the earlier one is no longer the tail and must be moved.
        let following = store.allocate(5).expect("following");
        assert_eq!(store.grow_in_place(tail, 9, 20).expect("grow"), None);
        // The following run is the tail, so it may grow in place.
        assert_eq!(
            store.grow_in_place(following, 5, 9).expect("grow"),
            Some(following)
        );
        assert_eq!(store.read_used_len(following), 9);
    }

    #[test]
    fn a_run_at_the_largest_class_reports_no_in_place_growth() {
        let store = store();
        let run = store.allocate(1024).expect("largest class");
        assert_eq!(store.grow_in_place(run, 1024, 1024).expect("grow"), None);
        assert_eq!(store.arena_rows(), MAX_ROWS + RUN_HEADER_ROWS);
    }

    #[test]
    fn reopening_the_same_memory_keeps_the_arena_and_free_lists() {
        let memory = crate::test_support::vector_memory();
        let run = {
            let store = SpillRunStore::new(memory.clone(), 4).expect("store");
            let run = store.allocate(5).expect("allocate");
            store.release(run, 5).expect("release");
            run
        };
        let store = SpillRunStore::new(memory, 4).expect("reopen");
        assert_eq!(store.arena_rows(), MIN_ROWS + RUN_HEADER_ROWS);
        assert_eq!(store.allocate(8).expect("allocate"), run);
        assert_eq!(store.read_used_len(run), 8);
    }
}
