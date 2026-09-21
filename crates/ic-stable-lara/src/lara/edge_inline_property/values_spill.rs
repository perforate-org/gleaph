//! Values-plane spill store: the rows a bucket's inline-property bytes own beyond its slab prefix.
//!
//! ADR 0097 §5 gives the values plane the same tier the edge plane got in §3: a **run per bucket**,
//! allocated lazily, with `ArenaRows::VALUES` as the bound, a run id that is the arena row index of
//! the run's header row, and rows addressed by ordinal. The retired per-leaf
//! `InlinePropertyBytesLogStore` reserved `DEFAULT_MAX_LOG_ENTRIES` entries for every leaf whether
//! or not a single row spilled; a run is minted on the first row that does not fit the prefix and
//! grows in power-of-two classes after that.
//!
//! This module is the store itself: it owns the arena and the 8-byte cell encoding, and it is the
//! only place that knows how a value row maps onto a run. Two details are values-specific rather
//! than edge-specific:
//!
//! * **Cells, not edges.** A row is one [`INLINE_PROPERTY_BYTES_LOG_CELL_BYTES`]-byte cell: an
//!   inline value up to the cell's payload, or a tag naming a blob site for a wider one.
//! * **Blob sites move with their run.** A wide value lives in the blob map under `(run, row)`, and
//!   [`SpillRunStore::grow_to`] copies a run that is not at the arena tail to a new id. The store
//!   therefore re-keys the moved rows' blob sites inside the growth step, before it puts the new
//!   row's own site, so a published run id always reads every row it is said to hold.
//!
//! The outer `EdgeInlinePropertyBytesStore` owns the blob map and delegates here, so this module
//! holds only the arena and the row encoding; the two error types are mapped into the ones the
//! callers match on at that boundary.

use super::blob_id::EdgeInlinePropertyBytesBlobId;
use super::blob_store::BlobStoreError;
use super::blobs::EdgeInlinePropertyBytesBlobMap;
use super::cell::{
    INLINE_PROPERTY_BYTES_LOG_CELL_BYTES, InlinePropertyBytesLogCell,
    inline_property_bytes_log_uses_blob,
};
use crate::GrowFailed;
use crate::labeled::spill_run_store::{
    ArenaRows, MAX_ROWS, SpillRunError, SpillRunId, SpillRunStore,
};
use ic_stable_structures::Memory;
use std::fmt;

/// Why a values row could not be read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ValuesSpillReadError {
    /// The caller's output buffer is smaller than the row's width.
    OutputTooSmall {
        /// Expected value byte width.
        width: u16,
        /// Actual output buffer length.
        out_len: usize,
    },
    /// The row is past the run's used length: unused capacity is never exposed as a row.
    RowOutOfRange {
        /// Rejected row index.
        index: u32,
    },
    /// The row is blob-tagged but no blob is stored under its site.
    MissingBlob {
        /// Values run id.
        run: u32,
        /// Row index inside the run.
        row: u32,
    },
    /// The stored blob's length disagrees with the row's width.
    BlobWidthMismatch {
        /// Expected value byte width.
        width: u16,
        /// Actual blob length.
        actual: usize,
    },
    /// The cell's tag cannot represent the expected width.
    InvalidInlineCell {
        /// Expected value byte width.
        width: u16,
    },
}

impl fmt::Display for ValuesSpillReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputTooSmall { width, out_len } => write!(
                f,
                "values row of width {width} does not fit the {out_len}-byte output buffer"
            ),
            Self::RowOutOfRange { index } => {
                write!(f, "values row {index} is past the run's used length")
            }
            Self::MissingBlob { run, row } => {
                write!(f, "missing value blob for values run {run}, row {row}")
            }
            Self::BlobWidthMismatch { width, actual } => {
                write!(f, "value blob length {actual} disagrees with width {width}")
            }
            Self::InvalidInlineCell { width } => {
                write!(f, "invalid inline cell for width {width}")
            }
        }
    }
}

impl std::error::Error for ValuesSpillReadError {}

/// Why a values row could not be written.
#[derive(Debug)]
pub(crate) enum ValuesSpillWriteError {
    /// The backing stable memory could not grow.
    Grow(GrowFailed),
    /// The blob map rejected the value.
    Blob(BlobStoreError),
    /// The run is at its largest class: the caller folds it into the values slab.
    RunFull,
    /// The row is past the run's used length (the overwrite path only claims existing rows).
    RowOutOfRange {
        /// Rejected row index.
        index: u32,
    },
}

impl fmt::Display for ValuesSpillWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grow(err) => write!(f, "values spill write failed: {err}"),
            Self::Blob(err) => write!(f, "value blob write failed: {err}"),
            Self::RunFull => write!(f, "values run is at its largest class"),
            Self::RowOutOfRange { index } => write!(f, "values row {index} is out of range"),
        }
    }
}

impl std::error::Error for ValuesSpillWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Grow(err) => Some(err),
            Self::Blob(err) => Some(err),
            Self::RunFull | Self::RowOutOfRange { .. } => None,
        }
    }
}

/// The arena's refusals all mean the same thing to a caller: this run cannot take another row here,
/// so fold it into the values slab (ADR 0097 §5).
fn map_write_error(err: SpillRunError) -> ValuesSpillWriteError {
    match err {
        SpillRunError::Grow(err) => ValuesSpillWriteError::Grow(err),
        SpillRunError::ArenaExhausted { .. } | SpillRunError::RunIdOutOfRange { .. } => {
            ValuesSpillWriteError::RunFull
        }
    }
}

/// A store of values spill runs over one stable memory.
pub(crate) struct ValuesSpillStore<M: Memory> {
    arena: SpillRunStore<M>,
}

impl<M: Memory> ValuesSpillStore<M> {
    /// Opens (or creates) the values arena. The store validates its own header, version and row
    /// width and fails closed, exactly like the edge plane's.
    pub(crate) fn new(memory: M) -> Result<Self, GrowFailed> {
        Ok(Self {
            arena: SpillRunStore::new(
                memory,
                INLINE_PROPERTY_BYTES_LOG_CELL_BYTES as u32,
                ArenaRows::VALUES,
            )?,
        })
    }

    /// Consumes the store and returns its backing memory.
    pub(crate) fn into_memory(self) -> M {
        self.arena.into_memory()
    }

    /// Mints a descriptor's raw values id against the values arena's capacity. The values id is a
    /// full `u32` (ADR 0097 §3), so the bound is [`ArenaRows::VALUES`] and the `u32::MAX` sentinel
    /// is rejected rather than masked.
    pub(crate) fn mint(&self, run: u32) -> Result<SpillRunId, SpillRunError> {
        SpillRunId::mint(run, ArenaRows::VALUES)
    }

    /// Used length of a run: the rows `0..used_len` that exist.
    #[inline]
    pub(crate) fn used_len(&self, run: SpillRunId) -> u32 {
        self.arena.read_used_len(run)
    }

    /// True when `run` is at its largest class and cannot take another row.
    pub(crate) fn run_is_full(&self, run: SpillRunId) -> bool {
        let used = self.used_len(run);
        used >= MAX_ROWS && used >= SpillRunStore::<M>::class_rows(used.max(1))
    }

    /// The run's row indices in ordinal order (`0..used_len`): a run has no chain to walk.
    pub(crate) fn asc_indices(&self, run: SpillRunId) -> Vec<u32> {
        (0..self.used_len(run)).collect()
    }

    /// Reads row `index` of `run` into `out`, fail-closed past the used length.
    pub(crate) fn read(
        &self,
        blobs: &EdgeInlinePropertyBytesBlobMap<M>,
        run: SpillRunId,
        index: u32,
        width: u16,
        out: &mut [u8],
    ) -> Result<(), ValuesSpillReadError> {
        let w = usize::from(width);
        if w == 0 || out.len() < w {
            return Err(ValuesSpillReadError::OutputTooSmall {
                width,
                out_len: out.len(),
            });
        }
        if index >= self.used_len(run) {
            return Err(ValuesSpillReadError::RowOutOfRange { index });
        }
        if inline_property_bytes_log_uses_blob(width) {
            let mut buf = Vec::with_capacity(w);
            if !blobs.get_blob(
                EdgeInlinePropertyBytesBlobId::from_run_row(run.get(), index),
                &mut buf,
            ) {
                return Err(ValuesSpillReadError::MissingBlob {
                    run: run.get(),
                    row: index,
                });
            }
            if buf.len() != w {
                return Err(ValuesSpillReadError::BlobWidthMismatch {
                    width,
                    actual: buf.len(),
                });
            }
            out[..w].copy_from_slice(&buf);
            return Ok(());
        }
        let cell = self.read_cell(run, index);
        if cell.decode_inline(width, out).is_some() {
            return Ok(());
        }
        Err(ValuesSpillReadError::InvalidInlineCell { width })
    }

    /// Writes row `index` of `run`, which must already exist: the compaction path claims a row it
    /// has just folded, and the used length does not change.
    pub(crate) fn write(
        &self,
        blobs: &EdgeInlinePropertyBytesBlobMap<M>,
        run: SpillRunId,
        index: u32,
        width: u16,
        bytes: &[u8],
    ) -> Result<(), ValuesSpillWriteError> {
        if index >= self.used_len(run) {
            return Err(ValuesSpillWriteError::RowOutOfRange { index });
        }
        let cell = self.cell_for_site(blobs, run.get(), index, width, bytes)?;
        self.arena
            .write_row(run, index, cell.as_bytes())
            .map_err(map_write_error)
    }

    /// Appends `bytes` as the run's next row, allocating the run when `run` is `None` and growing
    /// its class when that class is full. Returns the run the caller must publish and the row it
    /// wrote, so a descriptor is never pointed at a freed run.
    pub(crate) fn append(
        &self,
        blobs: &EdgeInlinePropertyBytesBlobMap<M>,
        run: Option<SpillRunId>,
        width: u16,
        bytes: &[u8],
    ) -> Result<(SpillRunId, u32), ValuesSpillWriteError> {
        let run = match run {
            Some(run) => run,
            None => {
                // `allocate(1)` already records row 0 as the run's used length, so the fresh case
                // writes that row rather than appending a second one.
                let run = self.arena.allocate(1).map_err(map_write_error)?;
                let cell = self.cell_for_site(blobs, run.get(), 0, width, bytes)?;
                self.arena
                    .write_row(run, 0, cell.as_bytes())
                    .map_err(map_write_error)?;
                return Ok((run, 0));
            }
        };
        let used = self.used_len(run);
        let class = SpillRunStore::<M>::class_rows(used.max(1));
        if used >= class {
            if class >= MAX_ROWS {
                return Err(ValuesSpillWriteError::RunFull);
            }
            // `grow_to` records the new used length itself, so the row at `used` is written after
            // it rather than appended. A run that grows outside the arena tail is copied to a new
            // id, and a wide value's blob is keyed by its site, so the moved rows' blobs move too.
            let grown = self
                .arena
                .grow_to(run, used, used + 1)
                .map_err(map_write_error)?;
            self.rekey_blob_sites(blobs, run, grown, used, width);
            let cell = self.cell_for_site(blobs, grown.get(), used, width, bytes)?;
            self.arena
                .write_row(grown, used, cell.as_bytes())
                .map_err(map_write_error)?;
            return Ok((grown, used));
        }
        let cell = self.cell_for_site(blobs, run.get(), used, width, bytes)?;
        let appended = self
            .arena
            .try_append_row(run, cell.as_bytes())
            .map_err(map_write_error)?;
        if !appended {
            return Err(ValuesSpillWriteError::RunFull);
        }
        Ok((run, used))
    }

    /// Releases `run` to its class's free list, dropping the blob sites it owned.
    pub(crate) fn release(
        &self,
        blobs: &EdgeInlinePropertyBytesBlobMap<M>,
        run: SpillRunId,
    ) -> Result<(), SpillRunError> {
        for row in 0..self.used_len(run) {
            blobs.drop_blob(EdgeInlinePropertyBytesBlobId::from_run_row(run.get(), row));
        }
        self.arena.release(run)
    }

    fn read_cell(&self, run: SpillRunId, index: u32) -> InlinePropertyBytesLogCell {
        let mut bytes = [0u8; INLINE_PROPERTY_BYTES_LOG_CELL_BYTES];
        self.arena.read_row(run, index, &mut bytes);
        InlinePropertyBytesLogCell::from_bytes(bytes)
    }

    /// The cell for `(run, row)`: a wide value lives in the blob map under that site, an inline one
    /// in the cell itself.
    fn cell_for_site(
        &self,
        blobs: &EdgeInlinePropertyBytesBlobMap<M>,
        run: u32,
        row: u32,
        width: u16,
        bytes: &[u8],
    ) -> Result<InlinePropertyBytesLogCell, ValuesSpillWriteError> {
        if inline_property_bytes_log_uses_blob(width) {
            blobs
                .put_blob(EdgeInlinePropertyBytesBlobId::from_run_row(run, row), bytes)
                .map_err(ValuesSpillWriteError::Blob)?;
            Ok(InlinePropertyBytesLogCell::EMPTY)
        } else {
            debug_assert_eq!(bytes.len(), usize::from(width));
            Ok(InlinePropertyBytesLogCell::inline(width, bytes))
        }
    }

    /// Moves a grown run's blob sites from the old id to the new one.
    fn rekey_blob_sites(
        &self,
        blobs: &EdgeInlinePropertyBytesBlobMap<M>,
        from: SpillRunId,
        to: SpillRunId,
        rows: u32,
        width: u16,
    ) {
        if from == to || !inline_property_bytes_log_uses_blob(width) {
            return;
        }
        let mut buf = Vec::new();
        for row in 0..rows {
            let from_id = EdgeInlinePropertyBytesBlobId::from_run_row(from.get(), row);
            buf.clear();
            if blobs.get_blob(from_id, &mut buf) {
                let _ = blobs.put_blob(
                    EdgeInlinePropertyBytesBlobId::from_run_row(to.get(), row),
                    &buf,
                );
                blobs.drop_blob(from_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::vector_memory;

    /// A value wider than the cell's payload is blob-backed; anything else is inline.
    const WIDE: u16 = 24;
    const INLINE: u16 = 4;

    fn store() -> (
        ValuesSpillStore<crate::VectorMemory>,
        EdgeInlinePropertyBytesBlobMap<crate::VectorMemory>,
    ) {
        (
            ValuesSpillStore::new(vector_memory()).expect("values spill store"),
            EdgeInlinePropertyBytesBlobMap::init(vector_memory()),
        )
    }

    #[test]
    fn a_values_run_round_trips_inline_cells_in_ordinal_order() {
        let (store, blobs) = store();
        let (run, first) = store
            .append(&blobs, None, INLINE, &[1, 2, 3, 4])
            .expect("append");
        assert_eq!(first, 0);
        let (run, second) = store
            .append(&blobs, Some(run), INLINE, &[9, 9, 9, 9])
            .expect("append");
        assert_eq!(second, 1);
        assert_eq!(store.used_len(run), 2);
        assert_eq!(store.asc_indices(run), vec![0, 1]);
        let mut out = [0u8; 4];
        store.read(&blobs, run, 1, INLINE, &mut out).expect("read");
        assert_eq!(out, [9, 9, 9, 9]);
        assert_eq!(
            store.read(&blobs, run, 2, INLINE, &mut out),
            Err(ValuesSpillReadError::RowOutOfRange { index: 2 })
        );
    }

    #[test]
    fn a_wide_value_lives_in_the_blob_map_under_its_run_row() {
        let (store, blobs) = store();
        let wide = [7u8; WIDE as usize];
        let (run, row) = store.append(&blobs, None, WIDE, &wide).expect("append");
        assert_eq!(row, 0);
        let mut out = [0u8; WIDE as usize];
        store.read(&blobs, run, row, WIDE, &mut out).expect("read");
        assert_eq!(out, wide);
        // Dropping the site fails the read closed rather than returning a stale value.
        blobs.drop_blob(EdgeInlinePropertyBytesBlobId::from_run_row(run.get(), row));
        assert_eq!(
            store.read(&blobs, run, row, WIDE, &mut out),
            Err(ValuesSpillReadError::MissingBlob {
                run: run.get(),
                row
            })
        );
    }

    /// A run that grows outside the arena tail is copied to a new id; its blob sites move with it.
    #[test]
    fn growing_a_moved_run_rekeys_its_blob_sites() {
        let (store, blobs) = store();
        let wide = [3u8; WIDE as usize];
        let (mut run, _) = store.append(&blobs, None, WIDE, &wide).expect("append");
        // A second run keeps the first off the arena tail, so growing it must move it.
        let (tail, _) = store.append(&blobs, None, WIDE, &wide).expect("append");
        assert_ne!(run, tail);
        for _ in 1..MAX_ROWS {
            let (next, _) = store
                .append(&blobs, Some(run), WIDE, &wide)
                .expect("append");
            run = next;
        }
        assert_eq!(store.used_len(run), MAX_ROWS);
        assert!(store.run_is_full(run));
        let mut out = [0u8; WIDE as usize];
        store.read(&blobs, run, 0, WIDE, &mut out).expect("read");
        assert_eq!(out, wide);
        assert!(matches!(
            store.append(&blobs, Some(run), WIDE, &wide),
            Err(ValuesSpillWriteError::RunFull)
        ));
    }

    #[test]
    fn releasing_a_run_drops_its_blob_sites() {
        let (store, blobs) = store();
        let (run, _) = store
            .append(&blobs, None, WIDE, &[5u8; WIDE as usize])
            .expect("append");
        store.release(&blobs, run).expect("release");
        let mut buf = Vec::new();
        assert!(!blobs.get_blob(
            EdgeInlinePropertyBytesBlobId::from_run_row(run.get(), 0),
            &mut buf
        ));
    }

    #[test]
    fn minting_rejects_the_no_run_sentinel_and_the_arena_limit() {
        let (store, _) = store();
        assert!(store.mint(0).is_ok());
        assert!(store.mint(crate::traits::NO_SPILL_RUN).is_err());
    }
}
