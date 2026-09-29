//! Log-backed byte CSR for per-label edge inline property bytes (separate from target rows).

mod blob_id;
mod blob_store;
mod blobs;
mod cell;
mod values_spill;

use crate::labeled::spill_run_store::SpillRunId;
use crate::lara::edge::free_span::{FreeSpan, FreeSpanStore};
use crate::lara::edge_inline_property::blobs::EdgeInlinePropertyBytesBlobMap;
use crate::lara::edge_inline_property::cell::inline_property_bytes_log_uses_blob;
use crate::lara::edge_inline_property::values_spill::{
    ValuesSpillReadError, ValuesSpillStore, ValuesSpillWriteError,
};
use crate::lara::operation_error::LaraOperationError;
use crate::lara::reserved::{region_is_zero, write_zeroes};
use crate::slab_index::{byte_exclusive_end_fits, byte_offset_fits, checked_add_byte_offset};
use crate::{GrowFailed, read_u64, safe_write, types::Address, write_u64};
use ic_stable_structures::Memory;
use std::{cell::Cell, fmt};

pub use blob_id::EdgeInlinePropertyBytesBlobId;
pub use blob_store::{
    BlobStoreError, EdgeInlinePropertyBytesBlobStore, NoopEdgeInlinePropertyBytesBlobStore,
};
pub use cell::InlinePropertyBytesLogCell;

/// Reserve room for a malformed wide blob before invoking the B-tree getter, which
/// decodes its whole body before the width can be checked. A mismatch stops the read.
pub(crate) fn inline_property_blob_read_headroom(width: u16) -> usize {
    if inline_property_bytes_log_uses_blob(width) {
        blobs::MAX_BLOB_BYTES - usize::from(width)
    } else {
        0
    }
}

#[cfg(test)]
thread_local! {
    /// Number of successful inline property bytes slab allocations (`append_byte_span` or
    /// `grow_byte_span_in_place`) to allow before the next allocation returns a
    /// synthetic `GrowFailed`.  `u32::MAX` means no forced error.  Used by the
    /// batch-write reserve rollback tests to prove that a partially-grown tail
    /// is restored.
    static FORCE_PAYLOAD_ALLOC_ERROR_AFTER: Cell<u32> = const { Cell::new(u32::MAX) };
}

#[cfg(test)]
/// Force the inline property bytes allocator to fail after `successful_allocations` successful
/// calls to `append_byte_span` or `grow_byte_span_in_place`.
pub(crate) fn force_inline_property_bytes_allocation_error_after(successful_allocations: u32) {
    FORCE_PAYLOAD_ALLOC_ERROR_AFTER.with(|c| c.set(successful_allocations));
}

#[cfg(test)]
fn take_forced_inline_property_bytes_allocation_error() -> bool {
    FORCE_PAYLOAD_ALLOC_ERROR_AFTER.with(|c| {
        let v = c.get();
        if v == u32::MAX {
            false
        } else if v == 0 {
            c.set(u32::MAX);
            true
        } else {
            c.set(v - 1);
            false
        }
    })
}

/// Magic bytes for the value byte slab.
pub const MAGIC: [u8; 3] = *b"LVG";
/// Current inline property bytes slab layout version.
pub const LAYOUT_VERSION: u8 = 1;
/// Persisted header size in bytes.
pub const HEADER_SIZE: u64 = 64;

const BYTE_CAPACITY_OFFSET: u64 = 4;
const SLAB_OCCUPIED_TAIL_OFFSET: u64 = 36;
/// Declared reserved header bytes between `byte_capacity` and `slab_occupied_tail`.
const RESERVED_LOW_OFFSET: u64 = 12;
const RESERVED_LOW_SIZE: usize = 24;
/// Declared reserved header bytes between `slab_occupied_tail` and [`HEADER_SIZE`].
const RESERVED_HIGH_OFFSET: u64 = 44;
const RESERVED_HIGH_SIZE: usize = 20;

fn byte_offset(addr: u64) -> u64 {
    HEADER_SIZE + addr
}

/// Persisted V1 value-slab header (byte-addressed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    /// Header magic bytes.
    pub magic: [u8; 3],
    /// Payload slab layout version.
    pub version: u8,
    /// Exclusive end of the byte address space (max valid offset + 1).
    pub byte_capacity: u64,
    /// First byte offset after the occupied slab region.
    pub slab_occupied_tail: u64,
}

impl HeaderV1 {
    /// Creates a V1 inline-property-bytes-slab header with the given byte capacity.
    pub fn new(byte_capacity: u64) -> Self {
        Self {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            byte_capacity,
            slab_occupied_tail: 0,
        }
    }
}

/// Errors when reopening an inline property bytes slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The inline property bytes slab header had unexpected magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The inline property bytes slab layout version is not supported.
    IncompatibleVersion(u8),
    /// The inline property bytes slab header or backing memory layout is invalid.
    InvalidLayout,
    /// The configured byte capacity exceeds the supported address space.
    ByteCapacityOverflow,
    /// The inline property bytes slab free-span index could not be reopened.
    FreeSpansInvalid,
    /// The values spill store could not be reopened: its header, version or row width disagrees
    /// with this build (it fails closed rather than reading rows at the wrong width).
    PayloadLog,
    /// The backing memories are partially initialized (some regions are empty
    /// while others are populated), so the store must not be reopened or recreated.
    PartialLayout,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(f, "bad inline property bytes slab magic {actual:?}")
            }
            Self::IncompatibleVersion(v) => {
                write!(f, "unsupported inline property bytes slab version {v}")
            }
            Self::InvalidLayout => write!(f, "invalid inline property bytes slab layout"),
            Self::ByteCapacityOverflow => write!(f, "value byte_capacity exceeds 40-bit space"),
            Self::FreeSpansInvalid => write!(f, "value free-span init failed"),
            Self::PayloadLog => write!(f, "values spill store init failed"),
            Self::PartialLayout => {
                write!(
                    f,
                    "inline property bytes store memories are partially initialized; refusing to reopen"
                )
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Errors returned while writing one inline property bytes overflow-log entry.
#[derive(Debug, PartialEq, Eq)]
pub enum InlinePropertyBytesLogWriteError {
    /// Stable memory could not grow or write the value-log entry.
    Grow(GrowFailed),
    /// External blob storage rejected the inline property bytes.
    Blob(BlobStoreError),
    /// The values run is at its largest class: the caller folds it into the values slab.
    ValuesRunFull,
    /// The row is past the run's used length.
    RowOutOfRange {
        /// Rejected row index.
        index: u32,
    },
}

impl fmt::Display for InlinePropertyBytesLogWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grow(err) => write!(f, "inline property bytes log write failed: {err}"),
            Self::Blob(err) => write!(f, "value blob write failed: {err}"),
            Self::ValuesRunFull => write!(f, "values run is full"),
            Self::RowOutOfRange { index } => write!(f, "values run row {index} is out of range"),
        }
    }
}

/// The values arena's refusals all mean the same thing to a caller: this run cannot take another
/// row here, so fold it into the values slab (ADR 0097 §5).
fn map_values_spill_write_error(err: ValuesSpillWriteError) -> InlinePropertyBytesLogWriteError {
    match err {
        ValuesSpillWriteError::Grow(err) => InlinePropertyBytesLogWriteError::Grow(err),
        ValuesSpillWriteError::Blob(err) => InlinePropertyBytesLogWriteError::Blob(err),
        ValuesSpillWriteError::RunFull => InlinePropertyBytesLogWriteError::ValuesRunFull,
        ValuesSpillWriteError::RowOutOfRange { index } => {
            InlinePropertyBytesLogWriteError::RowOutOfRange { index }
        }
    }
}

fn map_values_spill_read_error(err: ValuesSpillReadError) -> InlinePropertyBytesLogReadError {
    match err {
        ValuesSpillReadError::OutputTooSmall { width, out_len } => {
            InlinePropertyBytesLogReadError::OutputTooSmall { width, out_len }
        }
        ValuesSpillReadError::RowOutOfRange { index } => {
            InlinePropertyBytesLogReadError::MissingAscLogIndex {
                asc_log_index: index,
            }
        }
        ValuesSpillReadError::MissingBlob { run, row } => {
            InlinePropertyBytesLogReadError::MissingBlob { run, row }
        }
        ValuesSpillReadError::BlobWidthMismatch { width, actual } => {
            InlinePropertyBytesLogReadError::BlobWidthMismatch {
                expected: width,
                actual,
            }
        }
        ValuesSpillReadError::InvalidInlineCell { width } => {
            InlinePropertyBytesLogReadError::InvalidInlineCell { width }
        }
    }
}

impl std::error::Error for InlinePropertyBytesLogWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Grow(err) => Some(err),
            Self::Blob(err) => Some(err),
            Self::ValuesRunFull | Self::RowOutOfRange { .. } => None,
        }
    }
}

impl From<GrowFailed> for InlinePropertyBytesLogWriteError {
    fn from(value: GrowFailed) -> Self {
        Self::Grow(value)
    }
}

impl From<BlobStoreError> for InlinePropertyBytesLogWriteError {
    fn from(value: BlobStoreError) -> Self {
        Self::Blob(value)
    }
}

/// Errors returned while reading one inline property bytes overflow-log entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlinePropertyBytesLogReadError {
    /// Caller-provided output buffer is smaller than the expected edge-inline-property-bytes width.
    OutputTooSmall {
        /// Expected edge-inline-property-bytes byte width.
        width: u16,
        /// Actual output buffer length.
        out_len: usize,
    },
    /// Inline cell tag cannot represent the expected width.
    InvalidInlineCell {
        /// Expected edge-inline-property-bytes byte width.
        width: u16,
    },
    /// Blob-tagged values row has no corresponding inline property bytes blob.
    MissingBlob {
        /// Values run id.
        run: u32,
        /// Row index inside the run.
        row: u32,
    },
    /// The row is past the run's used length.
    RowOutOfRange {
        /// Rejected row index.
        index: u32,
    },
    /// Inline property bytes blob length does not match the expected edge-inline-property-bytes width.
    BlobWidthMismatch {
        /// Expected edge-inline-property-bytes byte width.
        expected: u16,
        /// Actual inline property bytes blob length.
        actual: usize,
    },
    /// Requested ascending log-chain index is outside the materialized chain.
    MissingAscLogIndex {
        /// Oldest-to-newest value-log index requested by the caller.
        asc_log_index: u32,
    },
}

impl fmt::Display for InlinePropertyBytesLogReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputTooSmall { width, out_len } => write!(
                f,
                "inline property bytes log read output too small: width {width}, output length {out_len}"
            ),
            Self::InvalidInlineCell { width } => {
                write!(
                    f,
                    "invalid inline property bytes log cell for width {width}"
                )
            }
            Self::MissingBlob { run, row } => {
                write!(f, "missing value blob for values run {run}, row {row}")
            }
            Self::RowOutOfRange { index } => {
                write!(f, "values run row {index} is out of range")
            }
            Self::BlobWidthMismatch { expected, actual } => write!(
                f,
                "value blob width mismatch: expected {expected} bytes, got {actual}"
            ),
            Self::MissingAscLogIndex { asc_log_index } => {
                write!(
                    f,
                    "inline property bytes log ascending index {asc_log_index} is missing"
                )
            }
        }
    }
}

impl std::error::Error for InlinePropertyBytesLogReadError {}

/// Stable byte slab for edge inline property bytes.
#[derive(Clone, Debug)]
pub struct PayloadByteSlabStore<M: Memory> {
    memory: M,
}

impl<M: Memory> PayloadByteSlabStore<M> {
    /// Creates a new inline property bytes slab with `header`.
    pub fn new(memory: M, header: HeaderV1) -> Result<Self, GrowFailed> {
        let store = Self { memory };
        store.grow_for_header(&header)?;
        store.write_header(&header);
        Ok(store)
    }

    /// Reopens an existing inline property bytes slab.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Err(InitError::InvalidLayout);
        }
        let store = Self { memory };
        let header = store.read_header()?;
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        if !byte_exclusive_end_fits(header.byte_capacity) {
            return Err(InitError::ByteCapacityOverflow);
        }
        // Reserved-region guards: nonzero reserved bytes mean a foreign or corrupt
        // slab layout that must not open.
        if !region_is_zero(&store.memory, RESERVED_LOW_OFFSET, RESERVED_LOW_SIZE) {
            return Err(InitError::InvalidLayout);
        }
        if !region_is_zero(&store.memory, RESERVED_HIGH_OFFSET, RESERVED_HIGH_SIZE) {
            return Err(InitError::InvalidLayout);
        }
        Ok(store)
    }

    /// Returns the backing memory.
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Reads and validates the persisted inline-property-bytes-slab header.
    pub fn header(&self) -> Result<HeaderV1, InitError> {
        self.read_header()
    }

    /// Persists `h` as the inline-property-bytes-slab header.
    pub(crate) fn write_header(&self, h: &HeaderV1) {
        self.memory.write(0, &h.magic);
        self.memory.write(3, &[h.version]);
        write_u64(
            &self.memory,
            Address::from(BYTE_CAPACITY_OFFSET),
            h.byte_capacity,
        );
        write_u64(
            &self.memory,
            Address::from(SLAB_OCCUPIED_TAIL_OFFSET),
            h.slab_occupied_tail,
        );
        // Both reserved regions lie inside the header page that every caller has
        // already grown before rewriting the header, so these writes cannot fail.
        write_zeroes(&self.memory, RESERVED_LOW_OFFSET, RESERVED_LOW_SIZE)
            .expect("LVG slab low reserved region lies within grown memory");
        write_zeroes(&self.memory, RESERVED_HIGH_OFFSET, RESERVED_HIGH_SIZE)
            .expect("LVG slab high reserved region lies within grown memory");
    }

    /// Grows the byte slab capacity to `n`.
    pub(crate) fn set_byte_capacity(&self, n: u64) -> Result<(), GrowFailed> {
        if !byte_exclusive_end_fits(n) {
            return Err(GrowFailed {
                current_size: self.memory.size(),
                delta: 0,
            });
        }
        let mut h = self.header().map_err(|_| GrowFailed {
            current_size: self.memory.size(),
            delta: 0,
        })?;
        h.byte_capacity = n;
        self.grow_for_header(&h)?;
        write_u64(&self.memory, Address::from(BYTE_CAPACITY_OFFSET), n);
        Ok(())
    }

    /// Reads bytes from the inline property bytes slab at `offset`.
    pub fn read_bytes(&self, offset: u64, out: &mut [u8]) {
        self.memory.read(byte_offset(offset), out);
    }

    /// Writes bytes to the inline property bytes slab at `offset`, growing stable memory if needed.
    pub(crate) fn write_bytes(&self, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
        safe_write(&self.memory, byte_offset(offset), bytes)
    }

    fn read_header(&self) -> Result<HeaderV1, InitError> {
        let mut magic = [0u8; 3];
        self.memory.read(0, &mut magic);
        if magic != MAGIC {
            return Err(InitError::BadMagic { actual: magic });
        }
        let mut version = [0u8; 1];
        self.memory.read(3, &mut version);
        Ok(HeaderV1 {
            magic,
            version: version[0],
            byte_capacity: read_u64(&self.memory, Address::from(BYTE_CAPACITY_OFFSET)),
            slab_occupied_tail: read_u64(&self.memory, Address::from(SLAB_OCCUPIED_TAIL_OFFSET)),
        })
    }

    fn grow_for_header(&self, h: &HeaderV1) -> Result<(), GrowFailed> {
        let need = HEADER_SIZE.saturating_add(h.byte_capacity);
        if need == 0 {
            return Ok(());
        }
        safe_write(&self.memory, need - 1, &[0])
    }
}

/// Combined stable edge-inline-property-bytes storage for labeled graphs.
pub struct EdgeInlinePropertyBytesStore<M: Memory> {
    slab: PayloadByteSlabStore<M>,
    values_spill: ValuesSpillStore<M>,
    blobs: EdgeInlinePropertyBytesBlobMap<M>,
    free_spans: FreeSpanStore<M>,
    header: Cell<HeaderV1>,
}

/// Inline-property-bytes-slab accounting derived from the inline property bytes allocator owner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlinePropertyBytesAllocatorStats {
    /// Backing inline-property-bytes-slab capacity in bytes.
    pub byte_capacity: u64,
    /// Exclusive end of the append-only occupied slab prefix.
    pub slab_occupied_tail: u64,
    /// Total bytes represented by retired free spans.
    pub free_bytes: u64,
    /// Largest individual retired free span.
    pub largest_free_span: u64,
    /// Number of retired free spans.
    pub free_span_count: u64,
}

impl<M: Memory> EdgeInlinePropertyBytesStore<M> {
    /// Creates a new edge-inline-property-bytes store over empty stable memories.
    pub fn new(
        slab_memory: M,
        values_spill_memory: M,
        value_blobs: M,
        free_spans: M,
        free_span_by_start: M,
        byte_capacity: u64,
    ) -> Result<Self, GrowFailed> {
        if !byte_exclusive_end_fits(byte_capacity) {
            return Err(GrowFailed {
                current_size: slab_memory.size(),
                delta: 0,
            });
        }
        let header = HeaderV1::new(byte_capacity);
        let slab = PayloadByteSlabStore::new(slab_memory, header)?;
        // ADR 0097 §5: the values plane's rows live in a spill arena exactly like the edge plane's.
        let values_spill = ValuesSpillStore::new(values_spill_memory)?;
        let blobs = EdgeInlinePropertyBytesBlobMap::init(value_blobs);
        let free_spans = FreeSpanStore::new(free_spans, free_span_by_start)?;
        Ok(Self {
            slab,
            values_spill,
            blobs,
            free_spans,
            header: Cell::new(header),
        })
    }

    /// Reopens an edge-inline-property-bytes store, initializing empty inline property bytes memories when needed.
    pub fn init(
        slab_memory: M,
        values_spill_memory: M,
        value_blobs: M,
        free_spans: M,
        free_span_by_start: M,
        byte_capacity: u64,
    ) -> Result<Self, InitError> {
        // The inline property bytes slab, overflow log, and free-span pair must move together.
        // `value_blobs` is asymmetric: on reopen it may be empty (a populated
        // store with no wide-inline-property-bytes blobs) or populated, but on a fresh create
        // it must be empty. A populated blob region alongside empty required
        // regions is partial loss.
        match crate::classify_composite_init([
            slab_memory.size(),
            values_spill_memory.size(),
            free_spans.size(),
            free_span_by_start.size(),
        ]) {
            crate::CompositeInit::Partial => return Err(InitError::PartialLayout),
            crate::CompositeInit::Fresh => {
                if value_blobs.size() != 0 {
                    return Err(InitError::PartialLayout);
                }
            }
            crate::CompositeInit::Reopen => {}
        }
        let slab = if slab_memory.size() == 0 {
            PayloadByteSlabStore::new(slab_memory, HeaderV1::new(byte_capacity))
                .map_err(|_| InitError::InvalidLayout)?
        } else {
            PayloadByteSlabStore::init(slab_memory)?
        };
        // The values spill store validates its own header, version and row width and fails closed.
        let values_spill =
            ValuesSpillStore::new(values_spill_memory).map_err(|_| InitError::PayloadLog)?;
        let free_spans = if free_spans.size() == 0 {
            FreeSpanStore::new(free_spans, free_span_by_start)
                .map_err(|_| InitError::InvalidLayout)?
        } else {
            FreeSpanStore::init(free_spans, free_span_by_start)
                .map_err(|_| InitError::FreeSpansInvalid)?
        };
        let blobs = EdgeInlinePropertyBytesBlobMap::init(value_blobs);
        let header = slab.header()?;
        Ok(Self {
            slab,
            values_spill,
            blobs,
            free_spans,
            header: Cell::new(header),
        })
    }

    // ---------------------------------------------------------------- values run tier (ADR 0097 §5)

    /// Mints a descriptor's raw values id against the values arena's capacity.
    pub(crate) fn spill_mint(&self, run: u32) -> Result<SpillRunId, LaraOperationError> {
        self.values_spill
            .mint(run)
            .map_err(LaraOperationError::from)
    }

    /// Used length of a values run: the rows `0..used_len` that exist.
    pub(crate) fn spill_used_len(&self, run: SpillRunId) -> u32 {
        self.values_spill.used_len(run)
    }

    /// True when `run` is at its largest class and cannot take another row: the caller's trigger to
    /// fold the run into the values slab.
    pub(crate) fn spill_run_is_full(&self, run: SpillRunId) -> bool {
        self.values_spill.run_is_full(run)
    }

    /// The run's row indices in ordinal order (`0..used_len`): a values run has no chain.
    pub(crate) fn spill_asc_indices(&self, run: SpillRunId) -> Vec<u32> {
        self.values_spill.asc_indices(run)
    }

    /// Reads row `row` of `run` into `out`. Fail-closed past the used length.
    pub(crate) fn spill_read(
        &self,
        run: SpillRunId,
        row: u32,
        width: u16,
        out: &mut [u8],
    ) -> Result<(), InlinePropertyBytesLogReadError> {
        self.values_spill
            .read(&self.blobs, run, row, width, out)
            .map_err(map_values_spill_read_error)
    }

    /// Reads a contiguous range of value ordinals, packing exact-width values into `out`.
    pub(crate) fn spill_read_range(
        &self,
        run: SpillRunId,
        rows: std::ops::Range<u32>,
        width: u16,
        out: &mut [u8],
    ) -> Result<(), InlinePropertyBytesLogReadError> {
        self.values_spill
            .read_range(&self.blobs, run, rows, width, out)
            .map_err(map_values_spill_read_error)
    }

    /// Writes row `row` of `run`, which must already exist: the compaction path claims a row it has
    /// just folded.
    pub(crate) fn spill_write(
        &self,
        run: SpillRunId,
        row: u32,
        width: u16,
        bytes: &[u8],
    ) -> Result<(), InlinePropertyBytesLogWriteError> {
        self.values_spill
            .write(&self.blobs, run, row, width, bytes)
            .map_err(map_values_spill_write_error)
    }

    /// Appends `bytes` as the run's next row, allocating the run when `run` is `None` and growing
    /// its class when that class is full. Returns the run the caller must publish and the row it
    /// wrote, so a descriptor is never pointed at a freed run.
    pub(crate) fn spill_append(
        &self,
        run: Option<SpillRunId>,
        width: u16,
        bytes: &[u8],
    ) -> Result<(SpillRunId, u32), InlinePropertyBytesLogWriteError> {
        self.values_spill
            .append(&self.blobs, run, width, bytes)
            .map_err(map_values_spill_write_error)
    }

    /// Releases `run` to its class's free list, dropping the blob sites it owned.
    pub(crate) fn spill_release(&self, run: SpillRunId) -> Result<(), LaraOperationError> {
        self.values_spill
            .release(&self.blobs, run)
            .map_err(LaraOperationError::from)
    }

    /// Drops one row's blob site, for tests that need a missing blob.
    #[cfg(test)]
    pub(crate) fn drop_inline_property_bytes_blob_for_test(&self, run: u32, row: u32) {
        self.blobs
            .drop_blob(EdgeInlinePropertyBytesBlobId::from_run_row(run, row));
    }

    /// Returns the cached inline-property-bytes-slab header.
    pub fn header(&self) -> HeaderV1 {
        self.header.get()
    }

    /// Returns the current inline property bytes capacity.
    pub fn byte_capacity(&self) -> u64 {
        self.header().byte_capacity
    }

    /// Returns allocator-owned capacity and retired-span accounting.
    pub fn allocator_stats(&self) -> InlinePropertyBytesAllocatorStats {
        let spans = self.free_spans.allocator_stats();
        InlinePropertyBytesAllocatorStats {
            byte_capacity: self.byte_capacity(),
            slab_occupied_tail: self.header().slab_occupied_tail,
            free_bytes: spans.free_bytes,
            largest_free_span: spans.largest_free_span,
            free_span_count: spans.free_span_count,
        }
    }

    pub(crate) fn free_byte_spans(&self) -> Vec<FreeSpan> {
        self.free_spans.spans()
    }

    /// Sets the inline property bytes capacity to `end`.
    pub(crate) fn set_byte_capacity(&self, end: u64) -> Result<(), GrowFailed> {
        self.slab.set_byte_capacity(end)?;
        let mut h = self.header();
        h.byte_capacity = end;
        self.header.set(h);
        Ok(())
    }

    /// Resets the occupied tail to `tail` and persists the header.
    ///
    /// Used to roll back unpublished tail allocations. The rewound range is
    /// owned by the implicit tail and must NOT also be registered as free:
    /// append/in-place growth can claim it without consulting the free list.
    pub(crate) fn reset_slab_occupied_tail(&self, tail: u64) {
        let mut h = self.header();
        h.slab_occupied_tail = tail;
        self.slab.write_header(&h);
        self.header.set(h);
    }

    /// Reads bytes from the inline property bytes slab.
    pub fn read_bytes(&self, offset: u64, out: &mut [u8]) {
        debug_assert!(byte_offset_fits(offset));
        self.slab.read_bytes(offset, out);
    }

    /// Writes bytes to the inline property bytes slab.
    pub(crate) fn write_bytes(&self, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
        if bytes.is_empty() {
            return Ok(());
        }
        let end = checked_add_byte_offset(offset, bytes.len() as u64).ok_or(GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })?;
        if end > self.byte_capacity() {
            self.set_byte_capacity(end.max(self.byte_capacity()))?;
        }
        self.slab.write_bytes(offset, bytes)?;
        let mut h = self.header();
        if end > h.slab_occupied_tail {
            h.slab_occupied_tail = end;
            self.slab.write_header(&h);
            self.header.set(h);
        }
        Ok(())
    }

    /// Reads one fixed-width inline property bytes slot.
    pub fn read_value_slot(&self, offset: u64, width: u16, out: &mut [u8]) {
        debug_assert_eq!(out.len(), usize::from(width));
        if width == 0 {
            return;
        }
        self.read_bytes(offset, out);
    }

    /// Writes one fixed-width inline property bytes slot.
    pub(crate) fn write_inline_property_bytes_slot(
        &self,
        offset: u64,
        width: u16,
        bytes: &[u8],
    ) -> Result<(), GrowFailed> {
        debug_assert_eq!(bytes.len(), usize::from(width));
        if width == 0 {
            return Ok(());
        }
        self.write_bytes(offset, bytes)
    }

    /// Writes an arbitrary byte range to the inline property bytes slab.
    pub(crate) fn write_range(&self, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
        self.write_bytes(offset, bytes)
    }

    /// Allocates a byte span, preferring the free list then bumping the occupied tail.
    pub(crate) fn allocate_byte_span(&self, len: u64) -> Result<u64, GrowFailed> {
        Ok(self.allocate_byte_span_with_origin(len)?.0)
    }

    /// Allocates a byte span and reports the free-list prefix when one was consumed.
    ///
    /// Keeping selection and removal in one operation avoids a second best-fit scan on
    /// reservation paths that need to retain enough information for rollback.
    pub(crate) fn allocate_byte_span_with_origin(
        &self,
        len: u64,
    ) -> Result<(u64, Option<FreeSpan>), GrowFailed> {
        if len == 0 {
            return Ok((0, None));
        }
        // The normal append path has an empty free-list. Avoid scanning every size
        // class when there is no reusable span to consider.
        if self.free_spans.is_empty() {
            return Ok((self.append_byte_span(len)?, None));
        }
        if let Some(span) = self.free_spans.take_best_fit(len).map_err(|_| GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })? {
            return Ok((span.start_slot, Some(span)));
        }
        Ok((self.append_byte_span(len)?, None))
    }

    /// Returns the best-fit retired byte span without removing it.
    pub(crate) fn peek_free_byte_span(&self, len: u64) -> Option<FreeSpan> {
        self.free_spans.peek_best_fit(len)
    }

    /// Restores a byte-span prefix previously removed by [`Self::allocate_byte_span`].
    pub(crate) fn restore_allocated_byte_span(&self, span: FreeSpan) -> Result<(), GrowFailed> {
        self.free_spans
            .restore_allocated_prefix(span)
            .map_err(|_| GrowFailed {
                current_size: self.byte_capacity(),
                delta: 0,
            })
    }

    /// Takes an inline property bytes free-span prefix at an exact byte offset.
    pub(crate) fn allocate_byte_span_at(&self, offset: u64, len: u64) -> Result<bool, GrowFailed> {
        if len == 0 {
            return Ok(true);
        }
        self.free_spans
            .take_prefix_at(offset, len)
            .map(|span| span.is_some())
            .map_err(|_| GrowFailed {
                current_size: self.byte_capacity(),
                delta: 0,
            })
    }

    /// Grows a span when it ends at the occupied tail (no free-list churn).
    pub(crate) fn grow_byte_span_in_place(
        &self,
        offset: u64,
        old_len: u64,
        new_len: u64,
    ) -> Result<bool, GrowFailed> {
        #[cfg(test)]
        if take_forced_inline_property_bytes_allocation_error() {
            return Err(GrowFailed {
                current_size: self.byte_capacity(),
                delta: 0,
            });
        }
        if new_len <= old_len {
            return Ok(true);
        }
        let tail = self.header().slab_occupied_tail;
        if offset.saturating_add(old_len) != tail {
            return Ok(false);
        }
        let pad_start = offset.checked_add(old_len).ok_or(GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })?;
        let pad_len = new_len.saturating_sub(old_len);
        if pad_len > 0 {
            self.write_bytes(
                pad_start,
                &vec![
                    0u8;
                    usize::try_from(pad_len).map_err(|_| GrowFailed {
                        current_size: self.byte_capacity(),
                        delta: 0,
                    })?
                ],
            )?;
        }
        let mut h = self.header();
        h.slab_occupied_tail = offset.checked_add(new_len).ok_or(GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })?;
        self.slab.write_header(&h);
        self.header.set(h);
        Ok(true)
    }

    /// Returns a retired byte range to the free list regardless of occupied tail.
    pub(crate) fn retire_byte_span(&self, offset: u64, len: u64) -> Result<(), GrowFailed> {
        if len == 0 {
            return Ok(());
        }
        self.free_spans
            .release_span(offset, len)
            .map_err(|_| GrowFailed {
                current_size: self.byte_capacity(),
                delta: 0,
            })
    }

    pub(crate) fn reserve_retired_byte_spans(&self, additional: u64) -> Result<(), GrowFailed> {
        self.free_spans.reserve_for_releases(additional)
    }

    /// Returns retired byte ranges to the free list.
    ///
    /// Spans still covered by [`HeaderV1::slab_occupied_tail`] are ignored so a
    /// failed in-place grow cannot recycle bytes that remain live at the tail.
    pub(crate) fn release_byte_span(&self, offset: u64, len: u64) -> Result<(), GrowFailed> {
        if len == 0 {
            return Ok(());
        }
        let tail = self.header().slab_occupied_tail;
        let end = offset.checked_add(len).ok_or(GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })?;
        if end <= tail {
            return Ok(());
        }
        self.free_spans
            .release_span(offset, len)
            .map_err(|_| GrowFailed {
                current_size: self.byte_capacity(),
                delta: 0,
            })
    }

    /// Allocates at the occupied tail without consulting the free list.
    pub(crate) fn append_byte_span(&self, len: u64) -> Result<u64, GrowFailed> {
        #[cfg(test)]
        if take_forced_inline_property_bytes_allocation_error() {
            return Err(GrowFailed {
                current_size: self.byte_capacity(),
                delta: 0,
            });
        }
        let start = self.header().slab_occupied_tail;
        let end = checked_add_byte_offset(start, len).ok_or(GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })?;
        if end > self.byte_capacity() {
            self.set_byte_capacity(end.max(self.byte_capacity()))?;
        }
        let mut h = self.header();
        h.slab_occupied_tail = end;
        self.slab.write_header(&h);
        self.header.set(h);
        Ok(start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;
    use std::{cell::RefCell, rc::Rc};

    fn mem() -> VectorMemory {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn test_store() -> EdgeInlinePropertyBytesStore<VectorMemory> {
        EdgeInlinePropertyBytesStore::new(mem(), mem(), mem(), mem(), mem(), 1024).expect("store")
    }

    #[test]
    fn slab_header_reserved_regions_are_zeroed_and_reopen() {
        let memory = mem();
        let store =
            PayloadByteSlabStore::new(memory.clone(), HeaderV1::new(1024)).expect("seed slab");
        drop(store);

        let mut low = [0u8; RESERVED_LOW_SIZE];
        memory.read(RESERVED_LOW_OFFSET, &mut low);
        assert!(low.iter().all(|&byte| byte == 0));
        let mut high = [0u8; RESERVED_HIGH_SIZE];
        memory.read(RESERVED_HIGH_OFFSET, &mut high);
        assert!(high.iter().all(|&byte| byte == 0));

        let reopened = PayloadByteSlabStore::init(memory).expect("reopen");
        assert_eq!(reopened.header().expect("header").byte_capacity, 1024);
    }
    #[test]
    fn slab_init_rejects_nonzero_reserved_byte_in_each_region() {
        for (offset, size) in [
            (RESERVED_LOW_OFFSET, RESERVED_LOW_SIZE),
            (RESERVED_HIGH_OFFSET, RESERVED_HIGH_SIZE),
        ] {
            let memory = mem();
            let store =
                PayloadByteSlabStore::new(memory.clone(), HeaderV1::new(1024)).expect("seed slab");
            drop(store);

            memory.write(offset + (size - 1) as u64, &[1]);
            assert!(
                matches!(
                    PayloadByteSlabStore::init(memory.clone()),
                    Err(InitError::InvalidLayout)
                ),
                "reserved byte at {offset}.. must be rejected"
            );
        }
    }
    #[test]
    fn allocator_stats_reports_retired_inline_property_spans() {
        let store = test_store();
        let first = store.append_byte_span(10).unwrap();
        let second = store.append_byte_span(20).unwrap();
        store.retire_byte_span(first, 10).unwrap();

        assert_eq!(
            store.allocator_stats(),
            InlinePropertyBytesAllocatorStats {
                byte_capacity: 1024,
                slab_occupied_tail: 30,
                free_bytes: 10,
                largest_free_span: 10,
                free_span_count: 1,
            }
        );
        assert_eq!(second, 10);
    }
    #[test]
    fn inline_property_bytes_slab_round_trips_bytes() {
        let store = test_store();
        store.write_bytes(10, &[1, 2, 3, 4]).expect("write");
        let mut buf = [0u8; 4];
        store.read_bytes(10, &mut buf);
        assert_eq!(buf, [1, 2, 3, 4]);
    }
    #[test]
    fn retire_byte_span_reuses_via_free_list() {
        let store = test_store();
        let a = store.allocate_byte_span(4).expect("a");
        store.write_bytes(a, &[1, 2, 3, 4]).expect("write a");
        store.retire_byte_span(a, 4).expect("retire a");
        let b = store.allocate_byte_span(4).expect("b");
        assert_eq!(b, a);
        let mut buf = [0u8; 4];
        store.read_bytes(b, &mut buf);
        assert_eq!(buf, [1, 2, 3, 4]);
    }
    #[test]
    fn release_byte_span_at_tail_does_not_recycle() {
        let store = test_store();
        let a = store.allocate_byte_span(4).expect("a");
        store.release_byte_span(a, 4).expect("release at tail");
        let b = store.allocate_byte_span(4).expect("b");
        assert_eq!(b, 4, "tail bump alloc after release at occupied tail");
    }
    #[test]
    fn grow_byte_span_in_place_extends_tail_span() {
        let store = test_store();
        let offset = store.allocate_byte_span(4).expect("allocate");
        store.write_bytes(offset, &[1, 2, 3, 4]).expect("write");
        assert!(
            store
                .grow_byte_span_in_place(offset, 4, 8)
                .expect("grow in place")
        );
        let mut buf = [0u8; 8];
        store.read_bytes(offset, &mut buf);
        assert_eq!(buf, [1, 2, 3, 4, 0, 0, 0, 0]);
    }
    #[test]
    fn init_rejects_partial_layout_when_log_wiped() {
        let slab = mem();
        let log = mem();
        let blobs = mem();
        let free_spans = mem();
        let by_start = mem();
        EdgeInlinePropertyBytesStore::new(
            slab.clone(),
            log.clone(),
            blobs.clone(),
            free_spans.clone(),
            by_start.clone(),
            1024,
        )
        .expect("store");
        // Slab, free-span pair populated, inline property bytes log wiped (miswired region).
        assert!(matches!(
            EdgeInlinePropertyBytesStore::init(slab, mem(), blobs, free_spans, by_start, 1024),
            Err(InitError::PartialLayout)
        ));
    }
    #[test]
    fn init_reopens_fully_populated_layout() {
        let slab = mem();
        let log = mem();
        let blobs = mem();
        let free_spans = mem();
        let by_start = mem();
        EdgeInlinePropertyBytesStore::new(
            slab.clone(),
            log.clone(),
            blobs.clone(),
            free_spans.clone(),
            by_start.clone(),
            1024,
        )
        .expect("store");
        assert!(
            EdgeInlinePropertyBytesStore::init(slab, log, blobs, free_spans, by_start, 1024)
                .is_ok()
        );
    }
    #[test]
    fn init_rejects_fresh_when_only_blob_region_populated() {
        let blobs = mem();
        // Required regions empty, but a wide-inline-property-bytes blob region survived: this
        // is partial loss, not a fresh create.
        crate::safe_write(&blobs, 0, &[1]).expect("populate blob region");
        assert!(matches!(
            EdgeInlinePropertyBytesStore::init(mem(), mem(), blobs, mem(), mem(), 1024),
            Err(InitError::PartialLayout)
        ));
    }
    #[test]
    fn init_reopens_when_blob_region_is_empty() {
        let slab = mem();
        let log = mem();
        let free_spans = mem();
        let by_start = mem();
        // Populate every required region; leave the blob region untouched.
        EdgeInlinePropertyBytesStore::new(
            slab.clone(),
            log.clone(),
            mem(),
            free_spans.clone(),
            by_start.clone(),
            1024,
        )
        .expect("store");
        // Reopen with an empty blob region: valid for a store that never wrote a
        // wide-inline-property-bytes blob.
        assert!(
            EdgeInlinePropertyBytesStore::init(slab, log, mem(), free_spans, by_start, 1024)
                .is_ok()
        );
    }
    /// ADR 0097 §5: the values plane's rows live in the bucket's own run, addressed by ordinal.
    #[test]
    fn a_values_run_round_trips_inline_property_bytes() {
        let store = test_store();
        let (run, row) = store.spill_append(None, 2, &[1, 2]).expect("append");
        assert_eq!(row, 0);
        let mut out = [0u8; 2];
        store.spill_read(run, 0, 2, &mut out).expect("read");
        assert_eq!(out, [1, 2]);
        // An overwrite claims a row that already exists; the used length does not change.
        store.spill_write(run, 0, 2, &[9, 9]).expect("write");
        store.spill_read(run, 0, 2, &mut out).expect("read");
        assert_eq!(out, [9, 9]);
        assert_eq!(store.spill_used_len(run), 1);
    }

    #[test]
    fn a_values_run_round_trips_wide_inline_property_bytes_through_the_blob_map() {
        let store = test_store();
        let wide = [0xA5u8; 24];
        let (run, _) = store.spill_append(None, 24, &wide).expect("append");
        let mut out = [0u8; 24];
        store.spill_read(run, 0, 24, &mut out).expect("read");
        assert_eq!(out, wide);
    }

    #[test]
    fn values_range_reads_exact_widths_and_rejects_slack() {
        for width in [1u16, 2, 8, 9, 33] {
            let store = test_store();
            let mut run = None;
            let value = |row: u8| {
                (0..width)
                    .map(|i| row.wrapping_mul(7).wrapping_add(i as u8))
                    .collect::<Vec<_>>()
            };
            for row in 0..35 {
                run = Some(store.spill_append(run, width, &value(row)).unwrap().0);
            }
            let run = run.unwrap();
            for rows in [0..35, 1..4, 31..35, 35..35] {
                let mut out = vec![0xA5; (rows.end - rows.start) as usize * usize::from(width)];
                store
                    .spill_read_range(run, rows.clone(), width, &mut out)
                    .unwrap();
                assert_eq!(
                    out,
                    rows.flat_map(|row| value(row as u8)).collect::<Vec<_>>()
                );
            }
            let mut out = vec![0xA5; 2 * usize::from(width)];
            assert_eq!(
                store.spill_read_range(run, 34..36, width, &mut out),
                Err(InlinePropertyBytesLogReadError::MissingAscLogIndex { asc_log_index: 35 })
            );
            assert_eq!(out, vec![0xA5; 2 * usize::from(width)]);
            assert_eq!(
                store.spill_read_range(run, 0..2, width, &mut out[..usize::from(width)]),
                Err(InlinePropertyBytesLogReadError::OutputTooSmall {
                    width,
                    out_len: usize::from(width)
                })
            );
            if width > 8 {
                store.drop_inline_property_bytes_blob_for_test(run.get(), 33);
                assert_eq!(
                    store.spill_read_range(
                        run,
                        32..35,
                        width,
                        &mut vec![0; 3 * usize::from(width)]
                    ),
                    Err(InlinePropertyBytesLogReadError::MissingBlob {
                        run: run.get(),
                        row: 33
                    })
                );
                store
                    .blobs
                    .put_blob(
                        EdgeInlinePropertyBytesBlobId::from_run_row(run.get(), 33),
                        &[7; 8],
                    )
                    .unwrap();
                assert_eq!(
                    store.spill_read_range(
                        run,
                        32..35,
                        width,
                        &mut vec![0; 3 * usize::from(width)]
                    ),
                    Err(InlinePropertyBytesLogReadError::BlobWidthMismatch {
                        expected: width,
                        actual: 8
                    })
                );
            }
        }
    }

    #[test]
    fn values_read_rejects_undersized_output_buffer() {
        let store = test_store();
        let (run, _) = store.spill_append(None, 4, &[1, 2, 3, 4]).expect("append");
        let mut out = [0u8; 2];
        assert!(matches!(
            store.spill_read(run, 0, 4, &mut out),
            Err(InlinePropertyBytesLogReadError::OutputTooSmall { .. })
        ));
    }

    #[test]
    fn values_read_rejects_missing_blob() {
        let store = test_store();
        let (run, _) = store.spill_append(None, 24, &[7u8; 24]).expect("append");
        store.drop_inline_property_bytes_blob_for_test(run.get(), 0);
        let mut out = [0u8; 24];
        assert!(matches!(
            store.spill_read(run, 0, 24, &mut out),
            Err(InlinePropertyBytesLogReadError::MissingBlob { .. })
        ));
    }

    #[test]
    fn values_read_rejects_blob_width_mismatch() {
        let store = test_store();
        let (run, _) = store.spill_append(None, 24, &[7u8; 24]).expect("append");
        // A body shorter than the row's width must fail closed, not be zero-padded.
        store
            .blobs
            .put_blob(
                EdgeInlinePropertyBytesBlobId::from_run_row(run.get(), 0),
                &[7u8; 8],
            )
            .expect("blob");
        let mut out = [0u8; 24];
        assert!(matches!(
            store.spill_read(run, 0, 24, &mut out),
            Err(InlinePropertyBytesLogReadError::BlobWidthMismatch { .. })
        ));
    }

    /// ADR 0097 §5: a values run is ordinal, so ascending index order is append order.
    #[test]
    fn a_values_run_reads_oldest_to_newest_by_ordinal() {
        let store = test_store();
        let mut run = None;
        for value in 1..=4u8 {
            let (next, _) = store.spill_append(run, 1, &[value]).expect("append");
            run = Some(next);
        }
        let run = run.expect("run");
        assert_eq!(store.spill_asc_indices(run), vec![0, 1, 2, 3]);
        let mut out = [0u8; 1];
        for (row, expected) in (1..=4u8).enumerate() {
            store
                .spill_read(run, row as u32, 1, &mut out)
                .expect("read");
            assert_eq!(out, [expected]);
        }
    }

    /// Releasing a run drops the rows: the descriptor must not keep pointing at it.
    #[test]
    fn releasing_a_values_run_frees_it_for_reuse() {
        let store = test_store();
        let (run, _) = store.spill_append(None, 1, &[7]).expect("append");
        store.spill_release(run).expect("release");
        let (reused, _) = store.spill_append(None, 1, &[8]).expect("append");
        assert_eq!(reused, run);
        let mut out = [0u8; 1];
        store.spill_read(reused, 0, 1, &mut out).expect("read");
        assert_eq!(out, [8]);
    }

    #[test]
    fn values_read_rejects_a_row_past_the_used_length() {
        let store = test_store();
        let (run, _) = store.spill_append(None, 2, &[1, 2]).expect("append");
        let mut out = [0u8; 2];
        assert!(matches!(
            store.spill_read(run, 1, 2, &mut out),
            Err(InlinePropertyBytesLogReadError::MissingAscLogIndex { asc_log_index: 1 })
        ));
    }
}
