//! Packed small-run store for per-bucket spill (first slice of the design recorded in
//! `design/implementation-gaps.md`, "per-bucket spill + lazy allocation").
//!
//! A spill run is a contiguous block of fixed-width rows owned by a single bucket. Runs are
//! allocated in **power-of-two capacity classes** with a **free list per class** — the shape the
//! reference implementation measured (`bucket-run-allocation`, `bucket-tail-growth`): capacity is
//! the smallest power of two that fits, a retired run is reusable only within its own class, and
//! the first four bytes of a free run hold the next free run's row offset (`u32::MAX` terminates).
//!
//! Why not the LTB store: its payload is a fixed 4096 bytes (1024 rows), so a five-row spill would
//! occupy a 1024-row granule. The smallest class here is [`MIN_ROWS`] rows, measured against the
//! production-shaped hub where the only spilling bucket held 74 rows
//! (`build_mixed_label_hub(20, 500)`: 1 of 20 buckets spilled, 74 of 10 000 rows).
//!
//! Deliberately absent in this slice: coalescing (a free run is reused only within its class) and
//! in-place growth (a spill that outgrows its class takes a new run and the caller copies, so
//! ownership stays with the bucket). Both are follow-ups with their own measurements.

#![allow(
    dead_code,
    reason = "Design slice 1: the store is measured and tested in isolation; the graph wires it in a\n              following slice (per-bucket spill)."
)]

use crate::types::Address;
use crate::{GrowFailed, Memory, read_u32, safe_write};

/// Header size in bytes (magic, version, geometry, per-class free heads).
pub(crate) const HEADER_SIZE: u64 = 64;
/// Magic bytes identifying this store.
pub(crate) const MAGIC: [u8; 3] = *b"SPR";
/// Layout version byte stored immediately after [`MAGIC`].
pub(crate) const LAYOUT_VERSION: u8 = 1;
/// Smallest run class in rows.
pub(crate) const MIN_ROWS: u32 = 8;
/// Largest run class this store serves; larger spills belong to the LTB store.
pub(crate) const MAX_ROWS: u32 = 1024;
/// Number of capacity classes: 8, 16, 32, 64, 128, 256, 512, 1024.
pub(crate) const CLASS_COUNT: usize = 8;
/// Sentinel for "no free run" / "no link".
const NONE: u32 = u32::MAX;
/// Offsets inside the 64-byte header.
const OFFSET_MAGIC: u64 = 0;
const OFFSET_VERSION: u64 = 3;
const OFFSET_ROW_BYTES: u64 = 4;
const OFFSET_ARENA_ROWS: u64 = 8;
const OFFSET_FREE_HEADS: u64 = 16;

/// Row offset of a run inside the arena, or one of the sentinels.
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
        assert!(row_bytes > 0, "spill rows must have a width");
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

    /// Capacity class for a run that must hold `len` rows: the smallest power of two in
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

    fn row_address(&self, row: RunId, index: u32) -> u64 {
        HEADER_SIZE + (u64::from(row) + u64::from(index)) * u64::from(self.row_bytes)
    }

    fn arena_rows(&self) -> u32 {
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

    /// Allocates a run able to hold `len` rows, reusing a free run of the same class when one
    /// exists and bump-allocating from the arena otherwise.
    pub(crate) fn allocate(&self, len: u32) -> Result<RunId, GrowFailed> {
        assert!(
            len > 0 && len <= MAX_ROWS,
            "spill length {len} outside the store's classes"
        );
        let capacity = Self::class_rows(len);
        let class = Self::class_index(capacity);
        let head = self.free_head(class);
        if head != NONE {
            let next = read_u32(&self.memory, Address::from(self.row_address(head, 0)));
            self.set_free_head(class, next)?;
            return Ok(head);
        }
        let start = self.arena_rows();
        let end = start.checked_add(capacity).expect("spill arena overflow");
        safe_write(&self.memory, OFFSET_ARENA_ROWS, &end.to_le_bytes())?;
        // Touch the last row so the backing is grown even before the caller writes it.
        let last = self.row_address(start, capacity - 1);
        safe_write(&self.memory, last, &[0u8; 1])?;
        Ok(start)
    }

    /// Grows a run in place when it is the arena tail. ADR 0097 makes this a requirement rather
    /// than an optimisation: without it every class change copies up to 2x the live rows, which an
    /// append-only log never had to do.
    ///
    /// `from_len` is the run's current length (its class follows from it, because a spill's used
    /// length is monotone) and `to_len` the length it must now hold. Returns `Ok(Some(run))` when
    /// the run was extended in place, `Ok(None)` when the caller must allocate, copy and release,
    /// and `Err` when the backing could not grow.
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
            // Nothing to grow into, or the arena tops out: longer spills continue at level 2.
            return Ok(None);
        }
        let arena = self.arena_rows();
        if run.checked_add(current) != Some(arena) {
            // Not the tail: another run follows, so growing here would collide with it.
            return Ok(None);
        }
        let end = arena
            .checked_add(target - current)
            .expect("spill arena overflow");
        safe_write(&self.memory, OFFSET_ARENA_ROWS, &end.to_le_bytes())?;
        let last = self.row_address(run, target - 1);
        safe_write(&self.memory, last, &[0u8; 1])?;
        Ok(Some(run))
    }

    /// Returns a run to its class's free list. `len` is the live length; the class follows from it,
    /// which is why the descriptor only needs the run id and the length.
    pub(crate) fn release(&self, run: RunId, len: u32) -> Result<(), GrowFailed> {
        assert!(run != NONE, "cannot release the sentinel run");
        let capacity = Self::class_rows(len.max(1));
        let class = Self::class_index(capacity);
        let head = self.free_head(class);
        safe_write(&self.memory, self.row_address(run, 0), &head.to_le_bytes())?;
        self.set_free_head(class, run)
    }

    /// Reads row `index` of `run` into `row`.
    pub(crate) fn read_row(&self, run: RunId, index: u32, row: &mut [u8]) {
        assert_eq!(
            row.len(),
            self.row_bytes as usize,
            "spill row width mismatch"
        );
        self.memory.read(self.row_address(run, index), row);
    }

    /// Writes `row` as row `index` of `run`.
    pub(crate) fn write_row(&self, run: RunId, index: u32, row: &[u8]) -> Result<(), GrowFailed> {
        assert_eq!(
            row.len(),
            self.row_bytes as usize,
            "spill row width mismatch"
        );
        safe_write(&self.memory, self.row_address(run, index), row)
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
    fn the_arena_grows_by_the_class_capacity() {
        let store = store();
        let before = store.arena_rows();
        let _ = store.allocate(74).expect("allocate");
        assert_eq!(store.arena_rows(), before + 128);
        let _ = store.allocate(9).expect("allocate");
        assert_eq!(store.arena_rows(), before + 128 + 16);
    }

    #[test]
    fn a_tail_run_grows_in_place_and_others_do_not() {
        let store = store();
        let tail = store.allocate(5).expect("tail");
        assert_eq!(store.arena_rows(), MIN_ROWS);
        // The tail run takes the next class in place: same id, arena extended by the difference.
        assert_eq!(store.grow_in_place(tail, 5, 9).expect("grow"), Some(tail));
        assert_eq!(store.arena_rows(), MIN_ROWS * 2);
        // With a run that follows, the earlier one is no longer the tail and must be moved.
        let following = store.allocate(5).expect("following");
        assert_eq!(store.grow_in_place(tail, 9, 20).expect("grow"), None);
        // The following run is the tail, so it may grow in place.
        assert_eq!(
            store.grow_in_place(following, 5, 9).expect("grow"),
            Some(following)
        );
    }

    #[test]
    fn a_run_at_the_largest_class_reports_no_in_place_growth() {
        let store = store();
        let run = store.allocate(1024).expect("largest class");
        assert_eq!(store.grow_in_place(run, 1024, 1024).expect("grow"), None);
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
        assert_eq!(store.arena_rows(), MIN_ROWS);
        assert_eq!(store.allocate(8).expect("allocate"), run);
    }
}
