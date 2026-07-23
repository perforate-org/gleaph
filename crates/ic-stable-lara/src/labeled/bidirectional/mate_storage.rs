//! Dormant, LARA-owned stable storage foundation for ADR 0048 mate blobs.
//!
//! The module is intentionally not wired into graph construction or lookup. It owns only the
//! composite layout, allocation/publication ordering, and reopen validation needed by the later
//! promotion slice.

use super::mate_blob_prototype::MateBlob;
use crate::lara::edge::free_span::FreeSpanStore;
use crate::{CompositeInit, GrowFailed, classify_composite_init, safe_write};
use ic_stable_structures::Memory;
use std::{cell::Cell, fmt};

const LOCATOR_MAGIC: [u8; 3] = *b"MLC";
const BLOB_MAGIC: [u8; 3] = *b"MBB";
const LAYOUT_VERSION: u8 = 1;
const HEADER_BYTES: u64 = 32;
const LOCATOR_ROW_BYTES: u64 = 5;
const LOCATOR_ROW_COUNT_OFFSET: u64 = 4;
const BLOB_TAIL_OFFSET: u64 = 4;
const LOCATOR_MAX_VALUE: u64 = (1 << 40) - 1;

const WASM_PAGE_BYTES: u64 = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MateLocatorState {
    ScanOnly,
    Rebuilding,
    Published { blob_offset: u64 },
}

/// Errors raised while creating, reopening, or mutating mate storage.
#[derive(Debug, PartialEq, Eq)]
pub enum MateStorageInitError {
    /// Forward and reverse PMA geometry cannot share one locator namespace.
    GeometryMismatch {
        /// Forward orientation segment size.
        forward_segment_size: u32,
        /// Reverse orientation segment size.
        reverse_segment_size: u32,
        /// Forward orientation segment count.
        forward_segment_count: u32,
        /// Reverse orientation segment count.
        reverse_segment_count: u32,
    },
    /// The computed shared locator row count overflowed.
    RowCountOverflow,
    /// LARA and mate regions disagree on fresh versus reopen state.
    OwnerLayoutMismatch,
    /// Exactly some, but not all, regions contain data.
    PartialLayout,
    /// Locator header or row area is invalid.
    InvalidLocatorLayout,
    /// Blob header or byte area is invalid.
    InvalidBlobLayout,
    /// The persisted layout version is unsupported.
    IncompatibleVersion(u8),
    /// Persisted locator rows differ from the owner geometry.
    RowCountMismatch {
        /// Expected row count.
        expected: u64,
        /// Persisted row count.
        actual: u64,
    },
    /// Stable memory growth failed.
    Grow(GrowFailed),
    /// Free-span regions are invalid.
    FreeSpan,
    /// A referenced blob is invalid.
    InvalidBlob,
    /// A locator row is outside the persisted range.
    RowOutOfRange,
    /// A published locator offset does not fit the locator encoding.
    LocatorValueOverflow,
    /// A blob length does not fit the blob encoding.
    BlobLengthOverflow,
    /// A free-span operation failed.
    FreeSpanError,
    /// A rebuild is already active for the row.
    RebuildAlreadyActive,
    /// A rebuild token does not match the persisted row state.
    RebuildStateMismatch,
}

impl fmt::Display for MateStorageInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GeometryMismatch {
                forward_segment_size,
                reverse_segment_size,
                forward_segment_count,
                reverse_segment_count,
            } => write!(
                f,
                "mate geometry mismatch: forward size/count={forward_segment_size}/{forward_segment_count}, reverse size/count={reverse_segment_size}/{reverse_segment_count}"
            ),
            Self::RowCountOverflow => write!(f, "mate locator row count overflow"),
            Self::OwnerLayoutMismatch => write!(f, "owner LARA/mate layout state mismatch"),
            Self::PartialLayout => write!(f, "partial mate storage layout"),
            Self::InvalidLocatorLayout => write!(f, "invalid mate locator layout"),
            Self::InvalidBlobLayout => write!(f, "invalid mate blob layout"),
            Self::IncompatibleVersion(version) => {
                write!(f, "unsupported mate storage version {version}")
            }
            Self::RowCountMismatch { expected, actual } => {
                write!(
                    f,
                    "mate locator row count mismatch: expected {expected}, got {actual}"
                )
            }
            Self::Grow(error) => write!(f, "mate storage grow failed: {error}"),
            Self::FreeSpan => write!(f, "mate free-span layout is invalid"),
            Self::InvalidBlob => write!(f, "invalid mate blob"),
            Self::RowOutOfRange => write!(f, "mate locator row is out of range"),
            Self::LocatorValueOverflow => write!(f, "mate locator value exceeds u40"),
            Self::BlobLengthOverflow => write!(f, "mate blob length exceeds u32"),
            Self::FreeSpanError => write!(f, "mate free-span operation failed"),
            Self::RebuildAlreadyActive => write!(f, "mate rebuild is already active"),
            Self::RebuildStateMismatch => write!(f, "mate rebuild token state mismatch"),
        }
    }
}

impl std::error::Error for MateStorageInitError {}

pub(crate) struct MateLocatorStore<M: Memory> {
    memory: M,
    row_count: Cell<u64>,
}

impl<M: Memory> MateLocatorStore<M> {
    fn new(memory: M, row_count: u64) -> Result<Self, MateStorageInitError> {
        let bytes = HEADER_BYTES
            .checked_add(
                row_count
                    .checked_mul(LOCATOR_ROW_BYTES)
                    .ok_or(MateStorageInitError::InvalidLocatorLayout)?,
            )
            .ok_or(MateStorageInitError::InvalidLocatorLayout)?;
        let store = Self {
            memory,
            row_count: Cell::new(row_count),
        };
        store.grow(bytes)?;
        let mut header = [0u8; HEADER_BYTES as usize];
        header[..3].copy_from_slice(&LOCATOR_MAGIC);
        header[3] = LAYOUT_VERSION;
        header[4..12].copy_from_slice(&row_count.to_be_bytes());
        safe_write(&store.memory, 0, &header).map_err(MateStorageInitError::Grow)?;
        Ok(store)
    }

    fn init(memory: M, expected_rows: u64) -> Result<Self, MateStorageInitError> {
        if memory.size() == 0 {
            return Self::new(memory, expected_rows);
        }
        if memory.size() * WASM_PAGE_BYTES < HEADER_BYTES {
            return Err(MateStorageInitError::InvalidLocatorLayout);
        }
        let mut header = [0u8; HEADER_BYTES as usize];
        memory.read(0, &mut header);
        if header[..3] != LOCATOR_MAGIC {
            return Err(MateStorageInitError::InvalidLocatorLayout);
        }
        if header[3] != LAYOUT_VERSION {
            return Err(MateStorageInitError::IncompatibleVersion(header[3]));
        }
        let actual = u64::from_be_bytes(header[4..12].try_into().expect("header row count"));
        if actual != expected_rows {
            return Err(MateStorageInitError::RowCountMismatch {
                expected: expected_rows,
                actual,
            });
        }
        let required_bytes = HEADER_BYTES
            .checked_add(
                actual
                    .checked_mul(LOCATOR_ROW_BYTES)
                    .ok_or(MateStorageInitError::InvalidLocatorLayout)?,
            )
            .ok_or(MateStorageInitError::InvalidLocatorLayout)?;
        if memory.size() * WASM_PAGE_BYTES < required_bytes {
            return Err(MateStorageInitError::InvalidLocatorLayout);
        }
        Ok(Self {
            memory,
            row_count: Cell::new(actual),
        })
    }

    fn grow_rows(&self, new_row_count: u64) -> Result<(), MateStorageInitError> {
        let current = self.row_count.get();
        if new_row_count <= current {
            return Ok(());
        }
        let bytes = HEADER_BYTES
            .checked_add(
                new_row_count
                    .checked_mul(LOCATOR_ROW_BYTES)
                    .ok_or(MateStorageInitError::InvalidLocatorLayout)?,
            )
            .ok_or(MateStorageInitError::InvalidLocatorLayout)?;
        self.grow(bytes)?;
        safe_write(
            &self.memory,
            LOCATOR_ROW_COUNT_OFFSET,
            &new_row_count.to_be_bytes(),
        )
        .map_err(MateStorageInitError::Grow)?;
        self.row_count.set(new_row_count);
        Ok(())
    }

    fn grow(&self, bytes: u64) -> Result<(), MateStorageInitError> {
        if self.memory.size() * WASM_PAGE_BYTES >= bytes {
            return Ok(());
        }
        let delta = (bytes - self.memory.size() * WASM_PAGE_BYTES).div_ceil(WASM_PAGE_BYTES);
        if self.memory.grow(delta) == -1 {
            return Err(MateStorageInitError::Grow(GrowFailed {
                current_size: self.memory.size(),
                delta,
            }));
        }
        Ok(())
    }

    fn row_offset(&self, row: u64) -> Result<u64, MateStorageInitError> {
        if row >= self.row_count.get() {
            return Err(MateStorageInitError::RowOutOfRange);
        }
        HEADER_BYTES
            .checked_add(
                row.checked_mul(LOCATOR_ROW_BYTES)
                    .ok_or(MateStorageInitError::RowOutOfRange)?,
            )
            .ok_or(MateStorageInitError::RowOutOfRange)
    }

    fn get_state(&self, row: u64) -> Result<MateLocatorState, MateStorageInitError> {
        let offset = self.row_offset(row)?;
        let mut bytes = [0u8; LOCATOR_ROW_BYTES as usize];
        self.memory.read(offset, &mut bytes);
        let value = u64::from_be_bytes([0, 0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]]);
        Ok(match value {
            0 => MateLocatorState::ScanOnly,
            1 => MateLocatorState::Rebuilding,
            encoded => MateLocatorState::Published {
                blob_offset: encoded - 2,
            },
        })
    }

    fn published_offset(&self, row: u64) -> Result<Option<u64>, MateStorageInitError> {
        Ok(match self.get_state(row)? {
            MateLocatorState::Published { blob_offset } => Some(blob_offset),
            MateLocatorState::ScanOnly | MateLocatorState::Rebuilding => None,
        })
    }

    fn publish_rebuilding(&self, row: u64) -> Result<(), MateStorageInitError> {
        let offset = self.row_offset(row)?;
        safe_write(&self.memory, offset, &[0, 0, 0, 0, 1]).map_err(MateStorageInitError::Grow)
    }

    fn publish_scan_only(&self, row: u64) -> Result<(), MateStorageInitError> {
        let offset = self.row_offset(row)?;
        safe_write(&self.memory, offset, &[0, 0, 0, 0, 0]).map_err(MateStorageInitError::Grow)
    }

    fn publish(&self, row: u64, blob_offset: u64) -> Result<(), MateStorageInitError> {
        let encoded = blob_offset
            .checked_add(2)
            .ok_or(MateStorageInitError::LocatorValueOverflow)?;
        if encoded > LOCATOR_MAX_VALUE {
            return Err(MateStorageInitError::LocatorValueOverflow);
        }
        let offset = self.row_offset(row)?;
        let bytes = encoded.to_be_bytes();
        safe_write(&self.memory, offset, &bytes[3..]).map_err(MateStorageInitError::Grow)
    }

    fn into_memory(self) -> M {
        self.memory
    }
}

pub(crate) struct MateBlobByteStore<M: Memory> {
    memory: M,
}

#[derive(Clone, Copy)]
struct Allocation {
    start: u64,
    len: u64,
    previous_tail: u64,
    from_free: bool,
}

impl<M: Memory> MateBlobByteStore<M> {
    fn new(memory: M) -> Result<Self, MateStorageInitError> {
        let store = Self { memory };
        store.grow(HEADER_BYTES)?;
        let mut header = [0u8; HEADER_BYTES as usize];
        header[..3].copy_from_slice(&BLOB_MAGIC);
        header[3] = LAYOUT_VERSION;
        safe_write(&store.memory, 0, &header).map_err(MateStorageInitError::Grow)?;
        Ok(store)
    }

    fn init(memory: M) -> Result<Self, MateStorageInitError> {
        if memory.size() == 0 {
            return Self::new(memory);
        }
        if memory.size() * WASM_PAGE_BYTES < HEADER_BYTES {
            return Err(MateStorageInitError::InvalidBlobLayout);
        }
        let mut header = [0u8; HEADER_BYTES as usize];
        memory.read(0, &mut header);
        if header[..3] != BLOB_MAGIC {
            return Err(MateStorageInitError::InvalidBlobLayout);
        }
        if header[3] != LAYOUT_VERSION {
            return Err(MateStorageInitError::IncompatibleVersion(header[3]));
        }
        let tail = u64::from_be_bytes(header[4..12].try_into().expect("blob tail"));
        let capacity = memory.size() * WASM_PAGE_BYTES;
        if tail > capacity.saturating_sub(HEADER_BYTES) {
            return Err(MateStorageInitError::InvalidBlobLayout);
        }
        Ok(Self { memory })
    }

    fn tail(&self) -> u64 {
        let mut bytes = [0u8; 8];
        self.memory.read(BLOB_TAIL_OFFSET, &mut bytes);
        u64::from_be_bytes(bytes)
    }

    fn set_tail(&self, tail: u64) -> Result<(), MateStorageInitError> {
        safe_write(&self.memory, BLOB_TAIL_OFFSET, &tail.to_be_bytes())
            .map_err(MateStorageInitError::Grow)
    }

    fn grow(&self, bytes: u64) -> Result<(), MateStorageInitError> {
        if self.memory.size() * WASM_PAGE_BYTES >= bytes {
            return Ok(());
        }
        let delta = (bytes - self.memory.size() * WASM_PAGE_BYTES).div_ceil(WASM_PAGE_BYTES);
        if self.memory.grow(delta) == -1 {
            return Err(MateStorageInitError::Grow(GrowFailed {
                current_size: self.memory.size(),
                delta,
            }));
        }
        Ok(())
    }

    fn allocate(
        &self,
        free_spans: &FreeSpanStore<M>,
        len: u64,
    ) -> Result<Allocation, MateStorageInitError> {
        if len == 0 {
            return Err(MateStorageInitError::InvalidBlob);
        }
        if let Some(span) = free_spans
            .take_best_fit(len)
            .map_err(|_| MateStorageInitError::FreeSpanError)?
        {
            return Ok(Allocation {
                start: span.start_slot,
                len,
                previous_tail: self.tail(),
                from_free: true,
            });
        }
        let previous_tail = self.tail();
        let end = previous_tail
            .checked_add(len)
            .ok_or(MateStorageInitError::BlobLengthOverflow)?;
        self.grow(
            HEADER_BYTES
                .checked_add(end)
                .ok_or(MateStorageInitError::BlobLengthOverflow)?,
        )?;
        self.set_tail(end)?;
        Ok(Allocation {
            start: previous_tail,
            len,
            previous_tail,
            from_free: false,
        })
    }

    fn rollback(
        &self,
        free_spans: &FreeSpanStore<M>,
        allocation: Allocation,
    ) -> Result<(), MateStorageInitError> {
        if allocation.from_free {
            free_spans
                .release_span(allocation.start, allocation.len)
                .map_err(|_| MateStorageInitError::FreeSpanError)
        } else {
            self.set_tail(allocation.previous_tail)
        }
    }

    fn write(&self, allocation: Allocation, bytes: &[u8]) -> Result<(), MateStorageInitError> {
        if u64::try_from(bytes.len()).map_err(|_| MateStorageInitError::BlobLengthOverflow)?
            != allocation.len
        {
            return Err(MateStorageInitError::InvalidBlob);
        }
        let offset = HEADER_BYTES
            .checked_add(allocation.start)
            .ok_or(MateStorageInitError::BlobLengthOverflow)?;
        safe_write(&self.memory, offset, bytes).map_err(MateStorageInitError::Grow)
    }

    fn read(&self, start: u64) -> Result<Vec<u8>, MateStorageInitError> {
        let tail = self.tail();
        if start >= tail || tail - start < 24 {
            return Err(MateStorageInitError::InvalidBlobLayout);
        }
        let offset = HEADER_BYTES
            .checked_add(start)
            .ok_or(MateStorageInitError::BlobLengthOverflow)?;
        let mut header = [0u8; 24];
        self.memory.read(offset, &mut header);
        let len = u32::from_be_bytes(header[20..24].try_into().expect("blob length")) as u64;
        if len < 24 || len > tail - start {
            return Err(MateStorageInitError::InvalidBlobLayout);
        }
        let mut bytes =
            vec![0u8; usize::try_from(len).map_err(|_| MateStorageInitError::BlobLengthOverflow)?];
        self.memory.read(offset, &mut bytes);
        Ok(bytes)
    }

    fn into_memory(self) -> M {
        self.memory
    }
}

pub(crate) struct MateStorage<M: Memory> {
    locators: MateLocatorStore<M>,
    blobs: MateBlobByteStore<M>,
    free_spans: FreeSpanStore<M>,
}

/// Caller-assigned stable regions owned jointly by both LARA orientations.
pub struct MateStorageMemories<M: Memory> {
    /// Fixed-row locator memory shared by both orientations.
    pub locator: M,
    /// Versioned mate blob byte memory.
    pub blobs: M,
    /// Retired blob free-span records.
    pub free_spans: M,
    /// Free-span coalescing index keyed by start offset.
    pub free_span_by_start: M,
}

impl<M: Memory> MateStorageMemories<M> {
    /// Creates a caller-assigned shared mate storage bundle.
    pub fn new(locator: M, blobs: M, free_spans: M, free_span_by_start: M) -> Self {
        Self {
            locator,
            blobs,
            free_spans,
            free_span_by_start,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MateRebuildToken {
    row: u64,
    previous: MateLocatorState,
    previous_blob_len: Option<u64>,
}

impl<M: Memory> MateStorage<M> {
    /// Returns the persisted locator row count without mutating any region.
    pub(crate) fn preflight_locator_rows(memory: &M) -> Result<Option<u64>, MateStorageInitError> {
        if memory.size() == 0 {
            return Ok(None);
        }
        if memory.size() * WASM_PAGE_BYTES < HEADER_BYTES {
            return Err(MateStorageInitError::InvalidLocatorLayout);
        }
        let mut header = [0u8; HEADER_BYTES as usize];
        memory.read(0, &mut header);
        if header[..3] != LOCATOR_MAGIC {
            return Err(MateStorageInitError::InvalidLocatorLayout);
        }
        if header[3] != LAYOUT_VERSION {
            return Err(MateStorageInitError::IncompatibleVersion(header[3]));
        }
        Ok(Some(u64::from_be_bytes(
            header[4..12].try_into().expect("locator row count"),
        )))
    }

    fn validate_references(&self) -> Result<(), MateStorageInitError> {
        for row in 0..self.locators.row_count.get() {
            if let MateLocatorState::Published { blob_offset: start } =
                self.locators.get_state(row)?
            {
                let bytes = self.blobs.read(start)?;
                MateBlob::decode(&bytes).map_err(|_| MateStorageInitError::InvalidBlob)?;
            }
        }
        Ok(())
    }

    pub(crate) fn init(
        locator: M,
        blobs: M,
        free_spans: M,
        free_span_by_start: M,
        locator_rows: u64,
    ) -> Result<Self, MateStorageInitError> {
        let storage = match classify_composite_init([
            locator.size(),
            blobs.size(),
            free_spans.size(),
            free_span_by_start.size(),
        ]) {
            CompositeInit::Partial => return Err(MateStorageInitError::PartialLayout),
            CompositeInit::Fresh => Ok(Self {
                locators: MateLocatorStore::new(locator, locator_rows)?,
                blobs: MateBlobByteStore::new(blobs)?,
                free_spans: FreeSpanStore::new(free_spans, free_span_by_start)
                    .map_err(|_| MateStorageInitError::FreeSpan)?,
            }),
            CompositeInit::Reopen => Ok(Self {
                locators: MateLocatorStore::init(locator, locator_rows)?,
                blobs: MateBlobByteStore::init(blobs)?,
                free_spans: FreeSpanStore::init(free_spans, free_span_by_start)
                    .map_err(|_| MateStorageInitError::FreeSpan)?,
            }),
        }?;
        storage.validate_references()?;
        Ok(storage)
    }

    pub(crate) fn replace(
        &self,
        row: u64,
        encoded_blob: &[u8],
    ) -> Result<(), MateStorageInitError> {
        MateBlob::decode(encoded_blob).map_err(|_| MateStorageInitError::InvalidBlob)?;
        let old = self.locators.published_offset(row)?;
        let old_bytes = old.map(|start| self.blobs.read(start)).transpose()?;
        let allocation = self.blobs.allocate(
            &self.free_spans,
            u64::try_from(encoded_blob.len())
                .map_err(|_| MateStorageInitError::BlobLengthOverflow)?,
        )?;
        if let Err(error) = self.blobs.write(allocation, encoded_blob) {
            self.blobs.rollback(&self.free_spans, allocation)?;
            return Err(error);
        }
        if let Err(error) = self.locators.publish(row, allocation.start) {
            self.blobs.rollback(&self.free_spans, allocation)?;
            return Err(error);
        }
        if let (Some(old_start), Some(old_bytes)) = (old, old_bytes) {
            self.free_spans
                .release_span(
                    old_start,
                    u64::try_from(old_bytes.len()).expect("validated blob length"),
                )
                .expect("published locator must not leave an unretirable old blob");
        }
        Ok(())
    }

    pub(crate) fn published_blob_offset(
        &self,
        row: u64,
    ) -> Result<Option<u64>, MateStorageInitError> {
        self.locators.published_offset(row)
    }

    pub(crate) fn ensure_locator_rows(&self, row_count: u64) -> Result<(), MateStorageInitError> {
        self.locators.grow_rows(row_count)
    }

    pub(crate) fn locator_state(&self, row: u64) -> Result<MateLocatorState, MateStorageInitError> {
        self.locators.get_state(row)
    }

    pub(crate) fn locator_row_count(&self) -> u64 {
        self.locators.row_count.get()
    }

    #[cfg(test)]
    pub(crate) fn test_locator_row_count(&self) -> u64 {
        self.locator_row_count()
    }

    #[cfg(test)]
    pub(crate) fn test_publish_rebuilding(&self, row: u64) -> Result<(), MateStorageInitError> {
        self.locators.publish_rebuilding(row)
    }

    pub(crate) fn begin_rebuild(&self, row: u64) -> Result<MateRebuildToken, MateStorageInitError> {
        let previous = self.locators.get_state(row)?;
        if matches!(previous, MateLocatorState::Rebuilding) {
            return Err(MateStorageInitError::RebuildAlreadyActive);
        }
        let previous_blob_len = match previous {
            MateLocatorState::Published { blob_offset } => {
                let bytes = self.blobs.read(blob_offset)?;
                MateBlob::decode(&bytes).map_err(|_| MateStorageInitError::InvalidBlob)?;
                Some(u64::try_from(bytes.len()).expect("validated blob length"))
            }
            MateLocatorState::ScanOnly | MateLocatorState::Rebuilding => None,
        };
        self.locators.publish_rebuilding(row)?;
        Ok(MateRebuildToken {
            row,
            previous,
            previous_blob_len,
        })
    }

    fn restore_rebuild_token(&self, token: MateRebuildToken) -> Result<(), MateStorageInitError> {
        if !matches!(
            self.locators.get_state(token.row)?,
            MateLocatorState::Rebuilding
        ) {
            return Err(MateStorageInitError::RebuildStateMismatch);
        }
        match token.previous {
            MateLocatorState::ScanOnly => self.locators.publish_scan_only(token.row),
            MateLocatorState::Published { blob_offset } => {
                self.locators.publish(token.row, blob_offset)
            }
            MateLocatorState::Rebuilding => Err(MateStorageInitError::RebuildStateMismatch),
        }
    }

    pub(crate) fn abort_rebuild(
        &self,
        token: MateRebuildToken,
    ) -> Result<(), MateStorageInitError> {
        self.restore_rebuild_token(token)
    }

    pub(crate) fn publish_rebuild(
        &self,
        token: MateRebuildToken,
        encoded_blob: &[u8],
    ) -> Result<(), MateStorageInitError> {
        if MateBlob::decode(encoded_blob).is_err() {
            self.restore_rebuild_token(token)?;
            return Err(MateStorageInitError::InvalidBlob);
        }
        if !matches!(
            self.locators.get_state(token.row)?,
            MateLocatorState::Rebuilding
        ) {
            return Err(MateStorageInitError::RebuildStateMismatch);
        }
        let encoded_len = match u64::try_from(encoded_blob.len()) {
            Ok(encoded_len) => encoded_len,
            Err(_) => {
                self.restore_rebuild_token(token)?;
                return Err(MateStorageInitError::BlobLengthOverflow);
            }
        };
        let allocation = match self.blobs.allocate(&self.free_spans, encoded_len) {
            Ok(allocation) => allocation,
            Err(error) => {
                self.restore_rebuild_token(token)?;
                return Err(error);
            }
        };
        if let Err(error) = self.blobs.write(allocation, encoded_blob) {
            self.blobs.rollback(&self.free_spans, allocation)?;
            self.restore_rebuild_token(token)?;
            return Err(error);
        }
        if let Err(error) = self.locators.publish(token.row, allocation.start) {
            self.blobs.rollback(&self.free_spans, allocation)?;
            self.restore_rebuild_token(token)?;
            return Err(error);
        }
        if let (MateLocatorState::Published { blob_offset }, Some(previous_blob_len)) =
            (token.previous, token.previous_blob_len)
        {
            self.free_spans
                .release_span(blob_offset, previous_blob_len)
                .expect("published rebuild must retire the previous blob");
        }
        Ok(())
    }

    pub(crate) fn into_memories(self) -> (M, M, M, M) {
        let (free_spans, free_span_by_start) = self.free_spans.into_memories();
        (
            self.locators.into_memory(),
            self.blobs.into_memory(),
            free_spans,
            free_span_by_start,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labeled::bidirectional::mate_blob_prototype::{Bucket, Mode};
    use crate::test_support::FailpointMemory;
    use ic_stable_structures::{
        DefaultMemoryImpl,
        memory_manager::{MemoryId, MemoryManager, VirtualMemory},
    };

    type TestMemory = VirtualMemory<DefaultMemoryImpl>;

    fn memories() -> [TestMemory; 4] {
        let manager = MemoryManager::init(DefaultMemoryImpl::default());
        [
            manager.get(MemoryId::new(0)),
            manager.get(MemoryId::new(1)),
            manager.get(MemoryId::new(2)),
            manager.get(MemoryId::new(3)),
        ]
    }

    fn failpoint_memories() -> [FailpointMemory; 4] {
        std::array::from_fn(|_| FailpointMemory::new())
    }

    fn blob() -> Vec<u8> {
        MateBlob {
            buckets: vec![Bucket {
                owner_vertex_id: 2,
                bucket_label_key: 7,
                entries: 1,
                mode: Mode::Packed { width_bytes: 1 },
                mapping: vec![1, 2],
            }],
        }
        .encode()
        .expect("blob")
    }

    #[test]
    fn fresh_reopen_and_replace_retire_old_span() {
        let [locator, blobs, free_spans, free_span_by_start] = memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        let first = blob();
        storage.replace(0, &first).expect("first blob");
        let old = storage
            .published_blob_offset(0)
            .expect("locator")
            .expect("published");
        storage.replace(0, &first).expect("replacement");
        assert_eq!(
            storage.published_blob_offset(0).expect("locator"),
            Some(old + first.len() as u64)
        );
        storage.replace(0, &first).expect("reuse retired span");
        assert_eq!(
            storage.published_blob_offset(0).expect("locator"),
            Some(old)
        );
        let memories = storage.into_memories();
        let reopened = MateStorage::init(memories.0, memories.1, memories.2, memories.3, 4)
            .expect("reopen storage");
        assert!(
            reopened
                .published_blob_offset(0)
                .expect("locator")
                .is_some()
        );
    }

    #[test]
    fn locator_tagged_states_round_trip_and_rebuilding_skips_blob_reads() {
        let [locator, blobs, free_spans, free_span_by_start] = memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        assert_eq!(
            storage.locator_state(0).expect("state"),
            MateLocatorState::ScanOnly
        );
        let _token = storage.begin_rebuild(0).expect("rebuilding");
        assert_eq!(
            storage.locator_state(0).expect("state"),
            MateLocatorState::Rebuilding
        );
        let memories = storage.into_memories();
        let reopened = MateStorage::init(memories.0, memories.1, memories.2, memories.3, 4)
            .expect("reopen rebuilding storage");
        assert_eq!(
            reopened.locator_state(0).expect("state"),
            MateLocatorState::Rebuilding
        );
    }

    #[test]
    fn rebuilding_cannot_be_started_twice_for_one_locator() {
        let [locator, blobs, free_spans, free_span_by_start] = memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        let _token = storage.begin_rebuild(0).expect("begin rebuild");
        assert_eq!(
            storage.begin_rebuild(0),
            Err(MateStorageInitError::RebuildAlreadyActive)
        );
    }

    #[test]
    fn failed_rebuild_restores_previous_published_locator() {
        let [locator, blobs, free_spans, free_span_by_start] = failpoint_memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        let first = blob();
        storage.replace(0, &first).expect("initial publish");
        let old_start = storage
            .published_blob_offset(0)
            .expect("locator")
            .expect("published");
        let tail = WASM_PAGE_BYTES - HEADER_BYTES;
        storage.blobs.set_tail(tail).expect("seed tail");
        let token = storage.begin_rebuild(0).expect("begin rebuild");
        assert_eq!(
            storage.locator_state(0).expect("state"),
            MateLocatorState::Rebuilding
        );
        storage
            .blobs
            .memory
            .fail_at_grow(storage.blobs.memory.grow_count() + 1);
        assert!(matches!(
            storage.publish_rebuild(token, &first),
            Err(MateStorageInitError::Grow(_))
        ));
        assert_eq!(
            storage.locator_state(0).expect("state"),
            MateLocatorState::Published {
                blob_offset: old_start
            }
        );
        assert_eq!(storage.blobs.tail(), tail);
    }

    #[test]
    fn invalid_rebuild_blob_restores_previous_published_locator() {
        let [locator, blobs, free_spans, free_span_by_start] = memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        let first = blob();
        storage.replace(0, &first).expect("initial publish");
        let old_start = storage
            .published_blob_offset(0)
            .expect("locator")
            .expect("published");
        let token = storage.begin_rebuild(0).expect("begin rebuild");
        assert_eq!(
            storage.publish_rebuild(token, b"invalid-mate-blob"),
            Err(MateStorageInitError::InvalidBlob)
        );
        assert_eq!(
            storage.locator_state(0).expect("state"),
            MateLocatorState::Published {
                blob_offset: old_start
            }
        );
    }

    #[test]
    fn successful_rebuild_publishes_new_blob_and_retires_old_span() {
        let [locator, blobs, free_spans, free_span_by_start] = memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        let first = blob();
        storage.replace(0, &first).expect("initial publish");
        let old_start = storage
            .published_blob_offset(0)
            .expect("locator")
            .expect("published");
        let token = storage.begin_rebuild(0).expect("begin rebuild");
        storage
            .publish_rebuild(token, &first)
            .expect("publish rebuild");
        assert!(matches!(
            storage.locator_state(0).expect("state"),
            MateLocatorState::Published { .. }
        ));
        storage.replace(0, &first).expect("reuse retired span");
        assert_eq!(
            storage.published_blob_offset(0).expect("locator"),
            Some(old_start)
        );
    }

    #[test]
    fn partial_layout_and_malformed_blob_fail_before_publication() {
        let [locator, blobs, free_spans, free_span_by_start] = memories();
        safe_write(&locator, 0, b"bad").expect("seed partial memory");
        assert!(matches!(
            MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4),
            Err(MateStorageInitError::PartialLayout)
        ));

        let memories = memories();
        let storage = MateStorage::init(
            memories[0].clone(),
            memories[1].clone(),
            memories[2].clone(),
            memories[3].clone(),
            4,
        )
        .expect("fresh storage");
        assert_eq!(
            storage.replace(0, b"not-a-mate-blob"),
            Err(MateStorageInitError::InvalidBlob)
        );
        assert_eq!(storage.published_blob_offset(0).expect("locator"), None);
    }

    #[test]
    fn reopen_rejects_corrupt_published_blob() {
        let [locator, blobs, free_spans, free_span_by_start] = memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        let encoded = blob();
        storage.replace(0, &encoded).expect("publish");
        let (locator, blobs, free_spans, free_span_by_start) = storage.into_memories();
        let mut magic = [0u8; 3];
        blobs.read(HEADER_BYTES, &mut magic);
        magic[0] ^= 0xff;
        blobs.write(HEADER_BYTES, &magic);
        assert!(matches!(
            MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4),
            Err(MateStorageInitError::InvalidBlob)
        ));
    }

    #[test]
    fn reopen_rejects_locator_rows_that_do_not_fit_backing_memory() {
        let [locator, blobs, free_spans, free_span_by_start] = memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        let (locator, blobs, free_spans, free_span_by_start) = storage.into_memories();
        let mut row_count = [0u8; 8];
        row_count[0..8].copy_from_slice(&1_000_000u64.to_be_bytes());
        locator.write(LOCATOR_ROW_COUNT_OFFSET, &row_count);
        assert!(matches!(
            MateStorage::init(locator, blobs, free_spans, free_span_by_start, 1_000_000),
            Err(MateStorageInitError::InvalidLocatorLayout)
        ));
    }

    #[test]
    fn allocation_failure_preserves_locator_tail_and_free_spans() {
        let [locator, blobs, free_spans, free_span_by_start] = failpoint_memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        let tail = WASM_PAGE_BYTES - HEADER_BYTES;
        storage.blobs.set_tail(tail).expect("seed tail");
        let before = storage.free_spans.allocator_stats();
        storage
            .blobs
            .memory
            .fail_at_grow(storage.blobs.memory.grow_count() + 1);
        assert!(matches!(
            storage.replace(0, &blob()),
            Err(MateStorageInitError::Grow(_))
        ));
        assert_eq!(storage.published_blob_offset(0).expect("locator"), None);
        assert_eq!(storage.blobs.tail(), tail);
        assert_eq!(storage.free_spans.allocator_stats(), before);
    }

    #[test]
    fn replacement_allocation_failure_preserves_existing_locator_and_blob() {
        let [locator, blobs, free_spans, free_span_by_start] = failpoint_memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        let first = blob();
        storage.replace(0, &first).expect("initial publish");
        let old_start = storage
            .published_blob_offset(0)
            .expect("locator")
            .expect("published");
        let old_bytes = storage.blobs.read(old_start).expect("old blob");
        let tail = WASM_PAGE_BYTES - HEADER_BYTES;
        storage.blobs.set_tail(tail).expect("seed tail");
        let before = storage.free_spans.allocator_stats();
        storage
            .blobs
            .memory
            .fail_at_grow(storage.blobs.memory.grow_count() + 1);

        assert!(matches!(
            storage.replace(0, &first),
            Err(MateStorageInitError::Grow(_))
        ));
        assert_eq!(
            storage.published_blob_offset(0).expect("locator"),
            Some(old_start),
            "failed replacement must preserve the old locator"
        );
        assert_eq!(storage.blobs.read(old_start).expect("old blob"), old_bytes);
        assert_eq!(storage.blobs.tail(), tail);
        assert_eq!(storage.free_spans.allocator_stats(), before);
    }

    #[test]
    fn four_regions_have_independent_headers() {
        let [locator, blobs, free_spans, free_span_by_start] = memories();
        let storage = MateStorage::init(locator, blobs, free_spans, free_span_by_start, 4)
            .expect("fresh storage");
        let (locator, blobs, free_spans, free_span_by_start) = storage.into_memories();
        let mut locator_magic = [0u8; 3];
        let mut blob_magic = [0u8; 3];
        locator.read(0, &mut locator_magic);
        blobs.read(0, &mut blob_magic);
        assert_eq!(locator_magic, LOCATOR_MAGIC);
        assert_eq!(blob_magic, BLOB_MAGIC);
        assert!(free_spans.size() > 0);
        assert!(free_span_by_start.size() > 0);
    }
}
