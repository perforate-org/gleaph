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
//! so there is no level 2 here and no block list to format. The **arena** is bounded too: a run id
//! is a row offset the 29-bit descriptor field must hold, so the arena may hand out at most
//! [`ARENA_ROWS_LIMIT`] rows. Exhaustion is a stated limit of the spill tier, not a memory failure:
//! [`SpillRunStore::allocate`] and [`SpillRunStore::grow_in_place`] return
//! [`SpillRunError::ArenaExhausted`] and the caller promotes that bucket to tree mode, so no insert
//! loses data and no workload reaches an `expect`-driven abort.

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
/// Largest row offset a run id can address, plus one.
///
/// A run id is a row offset that the descriptor's 29-bit field must hold (ADR 0097 §3), so the
/// arena may hand out at most `2^29 = 536_870_912` rows. That is a **stated limit of the spill
/// tier**: when it is reached, [`SpillRunError::ArenaExhausted`] tells the caller to promote the
/// bucket to tree mode (ADR 0097 §6) rather than abort.
pub(crate) const ARENA_ROWS_LIMIT: u32 = 1 << 29;
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

/// Why a spill run could not be allocated or extended.
///
/// The two causes are deliberately distinct. [`Self::ArenaExhausted`] is the spill tier's stated
/// size limit and the caller's **promotion trigger** (ADR 0097 §6); [`Self::Grow`] is a real
/// memory failure the caller reports instead of promoting, because promoting would not help.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SpillRunError {
    /// The arena has no addressable row left for the requested run.
    ArenaExhausted {
        /// Rows the arena has handed out so far.
        arena_rows: u32,
        /// Rows the requested run needs (its class plus the header row).
        requested_rows: u32,
        /// Largest addressable row offset plus one ([`ARENA_ROWS_LIMIT`]).
        arena_row_limit: u32,
    },
    /// The backing stable memory could not grow.
    Grow(GrowFailed),
}

impl From<GrowFailed> for SpillRunError {
    fn from(err: GrowFailed) -> Self {
        Self::Grow(err)
    }
}

/// A store of small spill runs over one stable memory.
pub(crate) struct SpillRunStore<M: Memory> {
    memory: M,
    row_bytes: u32,
    arena_row_limit: u32,
}

impl<M: Memory> SpillRunStore<M> {
    /// Opens the store, initialising the header when the memory is fresh. A header with the wrong
    /// magic or version is an invariant violation and panics (fail-closed, matching the LTB store).
    ///
    /// `arena_row_limit` is the largest addressable row offset plus one; it comes from the
    /// descriptor's run-id width and is not persisted, because it is a property of the packing
    /// rather than of the data (a reopen re-supplies it, and a layout change is caught by the
    /// version and row-width checks).
    pub(crate) fn new(memory: M, row_bytes: u32, arena_row_limit: u32) -> Result<Self, GrowFailed> {
        assert!(
            row_bytes >= 4,
            "a spill row must hold the 4-byte run header word"
        );
        assert!(arena_row_limit > 0, "a spill arena needs at least one row");
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
            return Ok(Self {
                memory,
                row_bytes,
                arena_row_limit,
            });
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
        Ok(Self {
            memory,
            row_bytes,
            arena_row_limit,
        })
    }

    /// Consumes the store and returns its backing memory.
    pub(crate) fn into_memory(self) -> M {
        self.memory
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
    ///
    /// A bump allocation that the arena cannot address returns [`SpillRunError::ArenaExhausted`]
    /// (the caller's promotion trigger, ADR 0097 §6); a backing that cannot grow returns
    /// [`SpillRunError::Grow`]. Reusing a free run never needs the arena, so it never exhausts.
    pub(crate) fn allocate(&self, len: u32) -> Result<RunId, SpillRunError> {
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
            let requested = capacity + RUN_HEADER_ROWS;
            let end = self.reserve_arena_rows(start, requested)?;
            safe_write(&self.memory, OFFSET_ARENA_ROWS, &end.to_le_bytes())?;
            start
        };
        self.set_used_len(run, len)?;
        // Touch the last data row so the backing is grown even before the caller writes it.
        let last = self.data_address(run, capacity - 1);
        safe_write(&self.memory, last, &[0u8; 1])?;
        Ok(run)
    }

    /// Returns the arena end after handing out `requested` rows at `start`, or
    /// [`SpillRunError::ArenaExhausted`] when the descriptor's run id cannot address them.
    fn reserve_arena_rows(&self, start: u32, requested: u32) -> Result<u32, SpillRunError> {
        start
            .checked_add(requested)
            .filter(|end| *end <= self.arena_row_limit)
            .ok_or(SpillRunError::ArenaExhausted {
                arena_rows: start,
                requested_rows: requested,
                arena_row_limit: self.arena_row_limit,
            })
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
    /// release, and `Err` when the arena cannot address the extension (the promotion trigger) or
    /// the backing cannot grow (a reported failure).
    pub(crate) fn grow_in_place(
        &self,
        run: RunId,
        from_len: u32,
        to_len: u32,
    ) -> Result<Option<RunId>, SpillRunError> {
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
        let end = self.reserve_arena_rows(arena, target - current)?;
        safe_write(&self.memory, OFFSET_ARENA_ROWS, &end.to_le_bytes())?;
        self.set_used_len(run, to_len)?;
        let last = self.data_address(run, target - 1);
        safe_write(&self.memory, last, &[0u8; 1])?;
        Ok(Some(run))
    }

    /// Grows a run to the class that holds `to_len` and returns the run id the caller must publish.
    ///
    /// In place when the run is the arena tail (ADR 0097's requirement), otherwise the next class is
    /// allocated, at most 2x the live rows are copied, and the old run is released. `to_len` must be
    /// within the store's classes; a bucket that needs more promotes to tree mode, which is the
    /// caller's decision (ADR 0097 §6).
    pub(crate) fn grow_to(
        &self,
        run: RunId,
        from_len: u32,
        to_len: u32,
    ) -> Result<RunId, SpillRunError> {
        assert!(
            to_len > from_len && to_len <= MAX_ROWS,
            "spill growth must stay inside the store's classes"
        );
        if let Some(same) = self.grow_in_place(run, from_len, to_len)? {
            return Ok(same);
        }
        let target = self.allocate(to_len)?;
        let mut row = vec![0u8; self.row_bytes as usize];
        for index in 0..from_len {
            self.read_row(run, index, &mut row);
            self.write_row(target, index, &row)?;
        }
        self.release(run)?;
        Ok(target)
    }

    /// Appends `row` at the run's used end and advances the used length.
    ///
    /// Returns `Ok(false)` when the run's class is already full, which is the caller's signal to
    /// grow first (the used length is unchanged). The used length only ever moves through this,
    /// [`Self::allocate`], [`Self::grow_in_place`] and [`Self::release`].
    pub(crate) fn try_append_row(&self, run: RunId, row: &[u8]) -> Result<bool, SpillRunError> {
        assert_eq!(
            row.len(),
            self.row_bytes as usize,
            "spill row width mismatch"
        );
        let used = self.read_used_len(run);
        if used >= self.capacity(run) {
            return Ok(false);
        }
        self.write_row(run, used, row)?;
        self.set_used_len(run, used + 1)?;
        Ok(true)
    }

    /// Returns a live run to its class's free list, turning its header word into the free-list link.
    ///
    /// The class is derived from the **run's own header**, never from a caller-supplied number: the
    /// used length is what `class_rows` maps to the allocated class, and a caller passing a live-row
    /// count instead would free a class-128 run into the 8-row list to be handed out with the wrong
    /// capacity. Deriving it here makes that mistake unrepresentable.
    ///
    /// Ordering: both target ranges are backed before the first write (`new` writes the 64-byte
    /// header, `allocate` touches the run's last row), so the only fallible step is the first write,
    /// which happens before any mutation - a failed release leaves the run live rather than
    /// freed-and-reported. The second write is infallible by construction.
    pub(crate) fn release(&self, run: RunId) -> Result<(), SpillRunError> {
        assert!(run != NONE, "cannot release the sentinel run");
        let class = Self::class_index(Self::class_rows(self.read_used_len(run).max(1)));
        let head = self.free_head(class);
        safe_write(&self.memory, self.header_address(run), &head.to_le_bytes())?;
        // The free-head word lives in the 64-byte store header, which `new` wrote; no growth can be
        // needed and no error can follow the write above.
        self.memory
            .write(OFFSET_FREE_HEADS + (class as u64) * 4, &run.to_le_bytes());
        Ok(())
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
    pub(crate) fn write_row(
        &self,
        run: RunId,
        index: u32,
        row: &[u8],
    ) -> Result<(), SpillRunError> {
        assert_eq!(
            row.len(),
            self.row_bytes as usize,
            "spill row width mismatch"
        );
        assert!(
            index < self.capacity(run),
            "spill write outside the run's class"
        );
        safe_write(&self.memory, self.data_address(run, index), row)?;
        Ok(())
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
        SpillRunStore::new(crate::test_support::vector_memory(), 4, ARENA_ROWS_LIMIT)
            .expect("store")
    }

    /// A store whose arena is one class-8 run plus its header row, so exhaustion is reachable.
    fn tiny_arena_store() -> SpillRunStore<crate::VectorMemory> {
        SpillRunStore::new(
            crate::test_support::vector_memory(),
            4,
            MIN_ROWS + RUN_HEADER_ROWS,
        )
        .expect("store")
    }

    #[test]
    fn a_full_arena_reports_exhaustion_instead_of_aborting() {
        let store = tiny_arena_store();
        // The first run exactly fills the addressable arena.
        let run = store.allocate(5).expect("first run fills the arena");
        assert_eq!(store.arena_rows(), MIN_ROWS + RUN_HEADER_ROWS);
        // The next bump allocation cannot be addressed: an error, not a panic.
        assert_eq!(
            store.allocate(5),
            Err(SpillRunError::ArenaExhausted {
                arena_rows: MIN_ROWS + RUN_HEADER_ROWS,
                requested_rows: MIN_ROWS + RUN_HEADER_ROWS,
                arena_row_limit: MIN_ROWS + RUN_HEADER_ROWS,
            })
        );
        // A free run of the same class still serves the request: reuse needs no arena row.
        store.release(run).expect("release");
        assert_eq!(store.allocate(5).expect("reuse"), run);
    }

    #[test]
    fn growing_a_tail_run_past_the_arena_limit_reports_exhaustion() {
        let store = tiny_arena_store();
        let run = store.allocate(5).expect("run");
        // Growing to class 16 would need 17 rows, but only 9 are addressable.
        assert_eq!(
            store.grow_in_place(run, 5, 9),
            Err(SpillRunError::ArenaExhausted {
                arena_rows: MIN_ROWS + RUN_HEADER_ROWS,
                requested_rows: MIN_ROWS,
                arena_row_limit: MIN_ROWS + RUN_HEADER_ROWS,
            })
        );
        // The run is unchanged: exhaustion is not a partial grow.
        assert_eq!(store.read_used_len(run), 5);
        assert_eq!(store.arena_rows(), MIN_ROWS + RUN_HEADER_ROWS);
    }

    #[test]
    fn a_memory_growth_failure_is_not_arena_exhaustion() {
        // A genuine growth failure must stay distinguishable, because the caller reports it
        // instead of promoting the bucket to tree mode.
        let memory = crate::test_support::FailpointMemory::new();
        let store = SpillRunStore::new(memory.clone(), 4, ARENA_ROWS_LIMIT).expect("store");
        // Fill the first backing page so the next allocation must grow the memory.
        loop {
            let next_end =
                HEADER_SIZE + u64::from(store.arena_rows() + MAX_ROWS + RUN_HEADER_ROWS) * 4;
            if next_end > memory.byte_len() as u64 {
                break;
            }
            store.allocate(MAX_ROWS).expect("fill");
        }
        memory.fail_at_grow(memory.grow_count() + 1);
        let err = store.allocate(MAX_ROWS).expect_err("growth must fail");
        assert!(
            matches!(err, SpillRunError::Grow(_)),
            "a growth failure must not be reported as arena exhaustion: {err:?}"
        );
    }

    #[test]
    fn appending_advances_the_used_length_until_the_class_is_full() {
        let store = store();
        // `allocate(len)` publishes `len` as the used length, so a class-8 run has 7 rows of room.
        let run = store.allocate(1).expect("allocate");
        for index in 1..MIN_ROWS {
            assert!(
                store
                    .try_append_row(run, &index.to_le_bytes())
                    .expect("append"),
                "row {index} fits class {MIN_ROWS}"
            );
        }
        assert_eq!(store.read_used_len(run), MIN_ROWS);
        // The class is full: the caller's signal to grow, and nothing changed.
        assert!(
            !store
                .try_append_row(run, &99u32.to_le_bytes())
                .expect("full")
        );
        assert_eq!(store.read_used_len(run), MIN_ROWS);
        for index in 0..MIN_ROWS {
            let mut buf = [0u8; 4];
            store.read_row(run, index, &mut buf);
            assert_eq!(u32::from_le_bytes(buf), index);
        }
    }

    #[test]
    fn growing_a_tail_run_extends_in_place_and_preserves_rows() {
        let store = store();
        let run = store.allocate(MIN_ROWS).expect("allocate");
        for index in 0..MIN_ROWS {
            store
                .write_row(run, index, &index.to_le_bytes())
                .expect("write");
        }
        assert_eq!(store.arena_rows(), MIN_ROWS + RUN_HEADER_ROWS);
        // The run is the arena tail, so the next class extends it in place.
        let grown = store.grow_to(run, MIN_ROWS, MIN_ROWS + 1).expect("grow");
        assert_eq!(grown, run);
        assert_eq!(store.read_used_len(run), MIN_ROWS + 1);
        assert_eq!(store.arena_rows(), MIN_ROWS * 2 + RUN_HEADER_ROWS);
        for index in 0..MIN_ROWS {
            let mut buf = [0u8; 4];
            store.read_row(run, index, &mut buf);
            assert_eq!(u32::from_le_bytes(buf), index);
        }
    }

    #[test]
    fn growing_a_non_tail_run_moves_it_and_copies_every_row() {
        let store = store();
        let first = store.allocate(MIN_ROWS).expect("first");
        let _tail = store.allocate(MIN_ROWS).expect("tail");
        for index in 0..MIN_ROWS {
            store
                .write_row(first, index, &index.to_le_bytes())
                .expect("write");
        }
        let grown = store.grow_to(first, MIN_ROWS, MIN_ROWS + 1).expect("grow");
        assert_ne!(grown, first, "a run that is not the tail must move");
        assert_eq!(store.read_used_len(grown), MIN_ROWS + 1);
        for index in 0..MIN_ROWS {
            let mut buf = [0u8; 4];
            store.read_row(grown, index, &mut buf);
            assert_eq!(u32::from_le_bytes(buf), index);
        }
        // The old class is free again.
        assert_eq!(store.allocate(MIN_ROWS).expect("reuse"), first);
    }

    #[test]
    fn release_takes_the_class_from_the_run_header() {
        let store = store();
        // A class-128 run (used 65) must come back to the class-128 list, not to class 8.
        let big = store.allocate(65).expect("big");
        assert_eq!(store.arena_rows(), 128 + RUN_HEADER_ROWS);
        store.release(big).expect("release");
        // A class-8 request cannot take it: it bumps a fresh run instead.
        let small = store.allocate(MIN_ROWS).expect("small");
        assert_ne!(small, big);
        // A class-128 request gets exactly it back.
        assert_eq!(store.allocate(65).expect("reuse"), big);
    }

    #[test]
    fn release_needs_no_growth_so_it_cannot_fail_after_freeing() {
        // The class comes from the run's header and both writes target already-backed rows, so a
        // memory that refuses to grow must not change the outcome: release cannot report a failure
        // after it has freed the run.
        let memory = crate::test_support::FailpointMemory::new();
        let store = SpillRunStore::new(memory.clone(), 4, ARENA_ROWS_LIMIT).expect("store");
        let run = store.allocate(5).expect("allocate");
        memory.fail_at_grow(memory.grow_count() + 1);
        store.release(run).expect("release needs no growth");
        assert_eq!(store.allocate(5).expect("reuse"), run);
    }

    #[test]
    fn a_failed_grow_during_grow_to_leaves_the_old_run_allocated() {
        let memory = crate::test_support::FailpointMemory::new();
        let store = SpillRunStore::new(memory.clone(), 4, ARENA_ROWS_LIMIT).expect("store");
        let first = store.allocate(MIN_ROWS).expect("first");
        let _tail = store.allocate(MIN_ROWS).expect("tail");
        for index in 0..MIN_ROWS {
            store
                .write_row(first, index, &index.to_le_bytes())
                .expect("write");
        }
        // Fill the backing so the class-16 allocation that `grow_to` will make must grow the
        // memory. The probe uses that class, not `MAX_ROWS`, because the fill runs are much bigger.
        loop {
            let next_end =
                HEADER_SIZE + u64::from(store.arena_rows() + RUN_HEADER_ROWS + 2 * MIN_ROWS) * 4;
            if next_end > memory.byte_len() as u64 {
                break;
            }
            store.allocate(MAX_ROWS).expect("fill");
        }
        memory.fail_at_grow(memory.grow_count() + 1);
        assert!(matches!(
            store.grow_to(first, MIN_ROWS, MIN_ROWS + 1),
            Err(SpillRunError::Grow(_))
        ));
        // The old run is still allocated, at its own id, with its length and rows intact.
        assert_eq!(store.read_used_len(first), MIN_ROWS);
        for index in 0..MIN_ROWS {
            let mut buf = [0u8; 4];
            store.read_row(first, index, &mut buf);
            assert_eq!(u32::from_le_bytes(buf), index);
        }
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
        store.release(run).expect("release");
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
        store.release(small).expect("release");
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
            let store = SpillRunStore::new(memory.clone(), 4, ARENA_ROWS_LIMIT).expect("store");
            let run = store.allocate(5).expect("allocate");
            store.release(run).expect("release");
            run
        };
        let store = SpillRunStore::new(memory, 4, ARENA_ROWS_LIMIT).expect("reopen");
        assert_eq!(store.arena_rows(), MIN_ROWS + RUN_HEADER_ROWS);
        assert_eq!(store.allocate(8).expect("allocate"), run);
        assert_eq!(store.read_used_len(run), 8);
    }
}
