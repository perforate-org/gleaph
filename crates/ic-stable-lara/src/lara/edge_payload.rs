//! Log-backed byte CSR for per-label edge payloads (separate from target rows).

mod blob_id;
mod blob_store;
mod blobs;
mod cell;
mod log;

use crate::lara::edge::{LOG_SRC_DEAD, free_span::FreeSpanStore};
use crate::lara::edge_payload::blobs::EdgePayloadBlobMap;
use crate::lara::edge_payload::cell::MAX_PAYLOAD_LOG_INLINE_WIDTH;
use crate::lara::edge_payload::log::{
    HeaderV1 as PayloadLogHeaderV1, PAYLOAD_BYTES, PayloadLogStore,
};
use crate::slab_index::{byte_exclusive_end_fits, byte_offset_fits, checked_add_byte_offset};
use crate::{GrowFailed, read_u64, safe_write, types::Address, write_u64};
use ic_stable_structures::Memory;
use std::{cell::Cell, fmt};

pub use blob_id::EdgePayloadBlobId;
pub use blob_store::{BlobStoreError, EdgePayloadBlobStore, NoopEdgePayloadBlobStore};
pub use cell::PayloadLogCell;
pub use log::{InitError as PayloadLogInitError, PayloadLogStore as ValueOverflowLogStore};

/// Magic bytes for the value byte slab.
pub const MAGIC: [u8; 3] = *b"LVG";
/// Current payload slab layout version.
pub const LAYOUT_VERSION: u8 = 1;
/// Persisted header size in bytes.
pub const HEADER_SIZE: u64 = 64;

const BYTE_CAPACITY_OFFSET: u64 = 4;
const SLAB_OCCUPIED_TAIL_OFFSET: u64 = 36;

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
    /// Creates a V1 payload-slab header with the given byte capacity.
    pub fn new(byte_capacity: u64) -> Self {
        Self {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            byte_capacity,
            slab_occupied_tail: 0,
        }
    }
}

/// Errors when reopening a payload slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The payload slab header had unexpected magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The payload slab layout version is not supported.
    IncompatibleVersion(u8),
    /// The payload slab header or backing memory layout is invalid.
    InvalidLayout,
    /// The configured byte capacity exceeds the supported address space.
    ByteCapacityOverflow,
    /// The payload free-span index could not be reopened.
    FreeSpansInvalid,
    /// The payload overflow log could not be reopened.
    PayloadLog(log::InitError),
    /// The payload overflow-log segment count does not match the edge store.
    PayloadLogLayoutMismatch,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => write!(f, "bad payload slab magic {actual:?}"),
            Self::IncompatibleVersion(v) => write!(f, "unsupported payload slab version {v}"),
            Self::InvalidLayout => write!(f, "invalid payload slab layout"),
            Self::ByteCapacityOverflow => write!(f, "value byte_capacity exceeds 40-bit space"),
            Self::FreeSpansInvalid => write!(f, "value free-span init failed"),
            Self::PayloadLog(e) => write!(f, "payload log init failed: {e}"),
            Self::PayloadLogLayoutMismatch => {
                write!(f, "payload log segment_count does not match edge store")
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Errors returned while writing one payload overflow-log entry.
#[derive(Debug, PartialEq, Eq)]
pub enum PayloadLogWriteError {
    /// Stable memory could not grow or write the value-log entry.
    Grow(GrowFailed),
    /// External blob storage rejected the payload.
    Blob(BlobStoreError),
    /// The payload overflow-log segment is full.
    SegmentLogFull,
}

impl fmt::Display for PayloadLogWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grow(err) => write!(f, "payload log write failed: {err}"),
            Self::Blob(err) => write!(f, "value blob write failed: {err}"),
            Self::SegmentLogFull => write!(f, "payload log segment is full"),
        }
    }
}

impl std::error::Error for PayloadLogWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Grow(err) => Some(err),
            Self::Blob(err) => Some(err),
            Self::SegmentLogFull => None,
        }
    }
}

impl From<GrowFailed> for PayloadLogWriteError {
    fn from(value: GrowFailed) -> Self {
        Self::Grow(value)
    }
}

impl From<BlobStoreError> for PayloadLogWriteError {
    fn from(value: BlobStoreError) -> Self {
        Self::Blob(value)
    }
}

/// Errors returned while reading one payload overflow-log entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadLogReadError {
    /// Caller-provided output buffer is smaller than the expected edge-payload width.
    OutputTooSmall {
        /// Expected edge-payload byte width.
        width: u16,
        /// Actual output buffer length.
        out_len: usize,
    },
    /// Inline cell tag/payload cannot represent the expected width.
    InvalidInlineCell {
        /// Expected edge-payload byte width.
        width: u16,
    },
    /// Blob-tagged value-log entry has no corresponding blob payload.
    MissingBlob {
        /// Value-log leaf segment.
        leaf_segment: u32,
        /// Value-log entry index inside the segment.
        entry_idx: u32,
    },
    /// Blob payload length does not match the expected edge-payload width.
    BlobWidthMismatch {
        /// Expected edge-payload byte width.
        expected: u16,
        /// Actual blob payload length.
        actual: usize,
    },
    /// Requested ascending log-chain index is outside the materialized chain.
    MissingAscLogIndex {
        /// Oldest-to-newest value-log index requested by the caller.
        asc_log_index: u32,
    },
}

impl fmt::Display for PayloadLogReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputTooSmall { width, out_len } => write!(
                f,
                "payload log read output too small: width {width}, output length {out_len}"
            ),
            Self::InvalidInlineCell { width } => {
                write!(f, "invalid inline payload log cell for width {width}")
            }
            Self::MissingBlob {
                leaf_segment,
                entry_idx,
            } => write!(
                f,
                "missing value blob for leaf segment {leaf_segment}, entry {entry_idx}"
            ),
            Self::BlobWidthMismatch { expected, actual } => write!(
                f,
                "value blob width mismatch: expected {expected} bytes, got {actual}"
            ),
            Self::MissingAscLogIndex { asc_log_index } => {
                write!(f, "payload log ascending index {asc_log_index} is missing")
            }
        }
    }
}

impl std::error::Error for PayloadLogReadError {}

/// Stable byte slab for edge payloads.
#[derive(Clone, Debug)]
pub struct PayloadByteSlabStore<M: Memory> {
    memory: M,
}

impl<M: Memory> PayloadByteSlabStore<M> {
    /// Creates a new payload byte slab with `header`.
    pub fn new(memory: M, header: HeaderV1) -> Result<Self, GrowFailed> {
        let store = Self { memory };
        store.grow_for_header(&header)?;
        store.write_header(&header);
        Ok(store)
    }

    /// Reopens an existing payload byte slab.
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
        Ok(store)
    }

    /// Returns the backing memory.
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Reads and validates the persisted payload-slab header.
    pub fn header(&self) -> Result<HeaderV1, InitError> {
        self.read_header()
    }

    /// Persists `h` as the payload-slab header.
    pub fn write_header(&self, h: &HeaderV1) {
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
    }

    /// Grows the byte slab capacity to `n`.
    pub fn set_byte_capacity(&self, n: u64) -> Result<(), GrowFailed> {
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

    /// Reads bytes from the payload slab at `offset`.
    pub fn read_bytes(&self, offset: u64, out: &mut [u8]) {
        self.memory.read(byte_offset(offset), out);
    }

    /// Writes bytes to the payload slab at `offset`, growing stable memory if needed.
    pub fn write_bytes(&self, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
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

/// Combined stable edge-payload storage for labeled graphs.
pub struct EdgePayloadStore<M: Memory> {
    slab: PayloadByteSlabStore<M>,
    log: PayloadLogStore<M>,
    blobs: EdgePayloadBlobMap<M>,
    free_spans: FreeSpanStore<M>,
    header: Cell<HeaderV1>,
}

impl<M: Memory> EdgePayloadStore<M> {
    /// Creates a new edge-payload store over empty stable memories.
    pub fn new(
        slab_memory: M,
        payload_log: M,
        value_blobs: M,
        free_spans: M,
        free_span_by_start: M,
        byte_capacity: u64,
        segment_count: u32,
    ) -> Result<Self, GrowFailed> {
        if !byte_exclusive_end_fits(byte_capacity) {
            return Err(GrowFailed {
                current_size: slab_memory.size(),
                delta: 0,
            });
        }
        let header = HeaderV1::new(byte_capacity);
        let slab = PayloadByteSlabStore::new(slab_memory, header)?;
        let log = PayloadLogStore::new(payload_log, PayloadLogHeaderV1::new(segment_count))?;
        let blobs = EdgePayloadBlobMap::init(value_blobs);
        let free_spans = FreeSpanStore::new(free_spans, free_span_by_start)?;
        Ok(Self {
            slab,
            log,
            blobs,
            free_spans,
            header: Cell::new(header),
        })
    }

    /// Reopens an edge-payload store, initializing empty payload memories when needed.
    pub fn init(
        slab_memory: M,
        payload_log: M,
        value_blobs: M,
        free_spans: M,
        free_span_by_start: M,
        byte_capacity: u64,
        edge_segment_count: u32,
    ) -> Result<Self, InitError> {
        let slab = if slab_memory.size() == 0 {
            PayloadByteSlabStore::new(slab_memory, HeaderV1::new(byte_capacity))
                .map_err(|_| InitError::InvalidLayout)?
        } else {
            PayloadByteSlabStore::init(slab_memory)?
        };
        let log = if payload_log.size() == 0 {
            PayloadLogStore::new(payload_log, PayloadLogHeaderV1::new(edge_segment_count))
                .map_err(|_| InitError::InvalidLayout)?
        } else {
            PayloadLogStore::init(payload_log).map_err(InitError::PayloadLog)?
        };
        if log.header().segment_count != edge_segment_count {
            return Err(InitError::PayloadLogLayoutMismatch);
        }
        let free_spans = if free_spans.size() == 0 {
            FreeSpanStore::new(free_spans, free_span_by_start)
                .map_err(|_| InitError::InvalidLayout)?
        } else {
            FreeSpanStore::init(free_spans, free_span_by_start)
                .map_err(|_| InitError::FreeSpansInvalid)?
        };
        let blobs = EdgePayloadBlobMap::init(value_blobs);
        let header = slab.header()?;
        Ok(Self {
            slab,
            log,
            blobs,
            free_spans,
            header: Cell::new(header),
        })
    }

    pub(crate) fn grow_segment_count_to(&self, new_count: u32) -> Result<(), GrowFailed> {
        self.log.grow_segment_count_to(new_count)
    }

    pub(crate) fn release_payload_log_segment(&self, leaf_segment: u32) -> Result<(), GrowFailed> {
        let high_water = self.payload_log_segment_high_water(leaf_segment);
        self.blobs.drain_leaf_segment(leaf_segment, high_water);
        self.log.release_segment(leaf_segment)
    }

    pub(crate) fn payload_log_segment_high_water(&self, leaf_segment: u32) -> u32 {
        let h = self.log.header();
        self.log.read_idx_with_header(&h, leaf_segment).max(0) as u32
    }

    pub(crate) fn payload_log_segment_is_full(&self, leaf_segment: u32) -> bool {
        let h = self.log.header();
        self.payload_log_segment_high_water(leaf_segment) >= h.max_log_entries
    }

    pub(crate) fn sweep_payload_log_chain(&self, leaf_segment: u32, payload_chain: &[u32]) {
        for &entry_idx in payload_chain {
            self.blobs.drop_log_site(leaf_segment, entry_idx);
            self.clear_payload_log_cell(leaf_segment, entry_idx);
        }
    }

    fn read_payload_log_cell(&self, leaf_segment: u32, entry_idx: u32) -> PayloadLogCell {
        let mut payload = [0u8; PAYLOAD_BYTES];
        let h = self.log.header();
        self.log
            .read_entry_with_header(&h, leaf_segment, entry_idx, &mut payload);
        PayloadLogCell::from_bytes(payload)
    }

    pub(crate) fn clear_payload_log_cell(&self, leaf_segment: u32, entry_idx: u32) {
        let h = self.log.header();
        let zeros = [0u8; PAYLOAD_BYTES];
        let _ = self
            .log
            .write_entry_with_header(&h, leaf_segment, entry_idx, -1, -1, &zeros);
    }

    pub(crate) fn write_payload_log_entry(
        &self,
        leaf_segment: u32,
        entry_idx: u32,
        prev_head: i32,
        src: i32,
        width: u16,
        payload_bytes: &[u8],
    ) -> Result<(), PayloadLogWriteError> {
        self.blobs.drop_log_site(leaf_segment, entry_idx);
        let cell = if usize::from(width) <= MAX_PAYLOAD_LOG_INLINE_WIDTH {
            let w = usize::from(width);
            debug_assert_eq!(payload_bytes.len(), w);
            PayloadLogCell::inline(width, payload_bytes)
        } else {
            let id = EdgePayloadBlobId::from_log_site(leaf_segment, entry_idx);
            self.blobs.put_blob(id, payload_bytes)?;
            PayloadLogCell::blob(width)
        };
        let h = self.log.header();
        self.log.write_entry_with_header(
            &h,
            leaf_segment,
            entry_idx,
            prev_head,
            src,
            cell.as_bytes(),
        )?;
        self.log.write_idx_at_least(
            leaf_segment,
            i32::try_from(entry_idx).unwrap_or(i32::MAX) + 1,
        );
        Ok(())
    }

    pub(crate) fn append_payload_log_entry(
        &self,
        leaf_segment: u32,
        prev_head: i32,
        src: i32,
        width: u16,
        payload_bytes: &[u8],
    ) -> Result<u32, PayloadLogWriteError> {
        let h = self.log.header();
        let idx = self.log.read_idx_with_header(&h, leaf_segment);
        if idx < 0 || idx >= h.max_log_entries as i32 {
            return Err(PayloadLogWriteError::SegmentLogFull);
        }
        let entry_idx = u32::try_from(idx).map_err(|_| PayloadLogWriteError::SegmentLogFull)?;
        self.write_payload_log_entry(
            leaf_segment,
            entry_idx,
            prev_head,
            src,
            width,
            payload_bytes,
        )?;
        Ok(entry_idx)
    }

    pub(crate) fn read_payload_log_entry(
        &self,
        leaf_segment: u32,
        entry_idx: u32,
        width: u16,
        out: &mut [u8],
    ) -> Result<(), PayloadLogReadError> {
        let w = usize::from(width);
        if w == 0 || out.len() < w {
            return Err(PayloadLogReadError::OutputTooSmall {
                width,
                out_len: out.len(),
            });
        }
        let cell = self.read_payload_log_cell(leaf_segment, entry_idx);
        if cell.is_inline() {
            if cell.decode_inline(width, out).is_some() {
                return Ok(());
            }
            return Err(PayloadLogReadError::InvalidInlineCell { width });
        } else if cell.is_blob() {
            let id = EdgePayloadBlobId::from_log_site(leaf_segment, entry_idx);
            let mut buf = Vec::with_capacity(w);
            if !self.blobs.get_blob(id, &mut buf) {
                return Err(PayloadLogReadError::MissingBlob {
                    leaf_segment,
                    entry_idx,
                });
            }
            if buf.len() != w {
                return Err(PayloadLogReadError::BlobWidthMismatch {
                    expected: width,
                    actual: buf.len(),
                });
            }
            out[..w].copy_from_slice(&buf);
            return Ok(());
        }
        Err(PayloadLogReadError::InvalidInlineCell { width })
    }

    pub(crate) fn mark_payload_log_entry_dead(
        &self,
        leaf_segment: u32,
        entry_idx: u32,
    ) -> Result<(), PayloadLogWriteError> {
        self.blobs.drop_log_site(leaf_segment, entry_idx);
        let mut payload = [0u8; PAYLOAD_BYTES];
        let h = self.log.header();
        let (prev, _) = self
            .log
            .read_entry_with_header(&h, leaf_segment, entry_idx, &mut payload);
        let zeros = [0u8; PAYLOAD_BYTES];
        self.log
            .write_entry_with_header(&h, leaf_segment, entry_idx, prev, LOG_SRC_DEAD, &zeros)
            .map_err(|_| PayloadLogWriteError::SegmentLogFull)
    }

    #[cfg(test)]
    pub(crate) fn drop_payload_blob_for_test(&self, leaf_segment: u32, entry_idx: u32) {
        self.blobs.drop_log_site(leaf_segment, entry_idx);
    }

    /// Returns value-log entry indices from oldest to newest by walking `log_head`.
    pub(crate) fn payload_log_chain_asc_indices(
        &self,
        leaf_segment: u32,
        log_head: i32,
    ) -> Vec<u32> {
        if log_head < 0 {
            return Vec::new();
        }
        let mut chain = Vec::new();
        let mut cur = log_head;
        while cur >= 0 {
            chain.push(cur as u32);
            let mut scratch = [0u8; PAYLOAD_BYTES];
            let h = self.log.header();
            let (prev, _) =
                self.log
                    .read_entry_with_header(&h, leaf_segment, cur as u32, &mut scratch);
            cur = prev;
        }
        chain.reverse();
        chain
    }

    /// Reads the value stored at `asc_log_index` in oldest-to-newest log order.
    pub(crate) fn read_payload_log_asc_index(
        &self,
        leaf_segment: u32,
        log_head: i32,
        asc_log_index: u32,
        width: u16,
        out: &mut [u8],
    ) -> Result<(), PayloadLogReadError> {
        if log_head < 0 || width == 0 {
            return Err(PayloadLogReadError::MissingAscLogIndex { asc_log_index });
        }
        let mut chain = Vec::new();
        let mut cur = log_head;
        while cur >= 0 {
            chain.push(cur as u32);
            let mut payload = [0u8; PAYLOAD_BYTES];
            let h = self.log.header();
            let (prev, _) =
                self.log
                    .read_entry_with_header(&h, leaf_segment, cur as u32, &mut payload);
            cur = prev;
        }
        chain.reverse();
        if let Some(&idx) = chain.get(asc_log_index as usize) {
            let mut payload = [0u8; PAYLOAD_BYTES];
            let h = self.log.header();
            let (_, src) = self
                .log
                .read_entry_with_header(&h, leaf_segment, idx, &mut payload);
            if src == LOG_SRC_DEAD {
                return Err(PayloadLogReadError::MissingAscLogIndex { asc_log_index });
            }
            return self.read_payload_log_entry(leaf_segment, idx, width, out);
        }
        Err(PayloadLogReadError::MissingAscLogIndex { asc_log_index })
    }

    /// Returns the cached payload-slab header.
    pub fn header(&self) -> HeaderV1 {
        self.header.get()
    }

    /// Returns the current payload byte capacity.
    pub fn byte_capacity(&self) -> u64 {
        self.header().byte_capacity
    }

    /// Sets the payload byte capacity to `end`.
    pub fn set_byte_capacity(&self, end: u64) -> Result<(), GrowFailed> {
        self.slab.set_byte_capacity(end)?;
        let mut h = self.header();
        h.byte_capacity = end;
        self.header.set(h);
        Ok(())
    }

    /// Reads bytes from the payload slab.
    pub fn read_bytes(&self, offset: u64, out: &mut [u8]) {
        debug_assert!(byte_offset_fits(offset));
        self.slab.read_bytes(offset, out);
    }

    /// Writes bytes to the payload slab.
    pub fn write_bytes(&self, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
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

    /// Reads one fixed-width payload slot.
    pub fn read_value_slot(&self, offset: u64, width: u16, out: &mut [u8]) {
        debug_assert_eq!(out.len(), usize::from(width));
        if width == 0 {
            return;
        }
        self.read_bytes(offset, out);
    }

    /// Writes one fixed-width payload slot.
    pub fn write_payload_slot(
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

    /// Writes an arbitrary byte range to the payload slab.
    pub fn write_range(&self, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
        self.write_bytes(offset, bytes)
    }

    /// Allocates a byte span, preferring the free list then bumping the occupied tail.
    pub fn allocate_byte_span(&self, len: u64) -> Result<u64, GrowFailed> {
        if len == 0 {
            return Ok(0);
        }
        if let Some(span) = self.free_spans.take_best_fit(len).map_err(|_| GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })? {
            return Ok(span.start_slot);
        }
        self.append_byte_span(len)
    }

    /// Grows a span when it ends at the occupied tail (no free-list churn).
    pub fn grow_byte_span_in_place(
        &self,
        offset: u64,
        old_len: u64,
        new_len: u64,
    ) -> Result<bool, GrowFailed> {
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
    pub fn retire_byte_span(&self, offset: u64, len: u64) -> Result<(), GrowFailed> {
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

    /// Returns retired byte ranges to the free list.
    ///
    /// Spans still covered by [`HeaderV1::slab_occupied_tail`] are ignored so a
    /// failed in-place grow cannot recycle bytes that remain live at the tail.
    pub fn release_byte_span(&self, offset: u64, len: u64) -> Result<(), GrowFailed> {
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
    pub fn append_byte_span(&self, len: u64) -> Result<u64, GrowFailed> {
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

    fn test_store() -> EdgePayloadStore<VectorMemory> {
        EdgePayloadStore::new(mem(), mem(), mem(), mem(), mem(), 1024, 1).expect("store")
    }

    #[test]
    fn payload_log_blob_round_trips_wide_payload() {
        let store = test_store();
        let payload = vec![0xABu8; 100];
        store
            .write_payload_log_entry(0, 0, -1, 0, 100, &payload)
            .expect("write");
        let mut out = vec![0u8; 100];
        store
            .read_payload_log_entry(0, 0, 100, &mut out)
            .expect("read");
        assert_eq!(out, payload);
    }

    #[test]
    fn payload_log_round_trips_payload() {
        let store = test_store();
        store
            .write_payload_log_entry(0, 0, -1, 0, 4, &[1, 2, 3, 4])
            .expect("write");
        let mut out = [0u8; 4];
        store
            .read_payload_log_entry(0, 0, 4, &mut out)
            .expect("read");
        assert_eq!(out, [1, 2, 3, 4]);
        store
            .read_payload_log_asc_index(0, 0, 0, 4, &mut out)
            .expect("read asc");
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn payload_log_read_rejects_undersized_output_buffer() {
        let store = test_store();
        store
            .write_payload_log_entry(0, 0, -1, 0, 4, &[1, 2, 3, 4])
            .expect("write");
        let mut out = [0u8; 3];
        assert_eq!(
            store.read_payload_log_entry(0, 0, 4, &mut out),
            Err(PayloadLogReadError::OutputTooSmall {
                width: 4,
                out_len: 3
            })
        );
    }

    #[test]
    fn payload_log_read_rejects_inline_width_above_inline_limit() {
        let store = test_store();
        store
            .write_payload_log_entry(0, 0, -1, 0, 8, &[1, 2, 3, 4, 5, 6, 7, 8])
            .expect("write");
        let mut out = [0u8; 9];
        assert_eq!(
            store.read_payload_log_entry(0, 0, 9, &mut out),
            Err(PayloadLogReadError::InvalidInlineCell { width: 9 })
        );
    }

    #[test]
    fn payload_log_read_rejects_missing_blob() {
        let store = test_store();
        store
            .write_payload_log_entry(0, 0, -1, 0, 9, &[1, 2, 3, 4, 5, 6, 7, 8, 9])
            .expect("write");
        store.blobs.drop_log_site(0, 0);
        let mut out = [0u8; 9];
        assert_eq!(
            store.read_payload_log_entry(0, 0, 9, &mut out),
            Err(PayloadLogReadError::MissingBlob {
                leaf_segment: 0,
                entry_idx: 0
            })
        );
    }

    #[test]
    fn payload_log_read_rejects_blob_width_mismatch() {
        let store = test_store();
        store
            .write_payload_log_entry(0, 0, -1, 0, 9, &[1, 2, 3, 4, 5, 6, 7, 8, 9])
            .expect("write");
        store
            .blobs
            .put_blob(EdgePayloadBlobId::from_log_site(0, 0), &[1, 2, 3, 4, 5])
            .expect("overwrite blob");
        let mut out = [0u8; 9];
        assert_eq!(
            store.read_payload_log_entry(0, 0, 9, &mut out),
            Err(PayloadLogReadError::BlobWidthMismatch {
                expected: 9,
                actual: 5
            })
        );
    }

    #[test]
    fn payload_slab_round_trips_bytes() {
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
    fn payload_log_asc_index_follows_oldest_to_newest() {
        let store = test_store();
        store
            .write_payload_log_entry(0, 0, -1, 0, 2, &[10, 0])
            .expect("entry 0");
        store
            .write_payload_log_entry(0, 1, 0, 0, 2, &[20, 0])
            .expect("entry 1");
        let mut out = [0u8; 2];
        store
            .read_payload_log_asc_index(0, 1, 0, 2, &mut out)
            .expect("read oldest");
        assert_eq!(out, [10, 0]);
        store
            .read_payload_log_asc_index(0, 1, 1, 2, &mut out)
            .expect("read newest");
        assert_eq!(out, [20, 0]);
    }

    #[test]
    fn payload_log_grows_segment_count() {
        let store = test_store();
        assert_eq!(store.log.header().segment_count, 1);
        store.grow_segment_count_to(4).expect("grow");
        assert_eq!(store.log.header().segment_count, 4);
        store
            .write_payload_log_entry(3, 0, -1, 0, 1, &[7])
            .expect("leaf 3");
        let mut out = [0u8; 1];
        store
            .read_payload_log_entry(3, 0, 1, &mut out)
            .expect("read leaf 3");
        assert_eq!(out, [7]);
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
    fn init_rejects_mismatched_log_segment_count() {
        use super::log::{HeaderV1 as LogHeader, PayloadLogStore};

        let edge_segments = 3u32;
        let log_mem = mem();
        let log = PayloadLogStore::new(log_mem, LogHeader::new(2))
            .expect("log with fewer segments than edge store");
        let log_mem = log.into_memory();
        assert!(matches!(
            EdgePayloadStore::init(mem(), log_mem, mem(), mem(), mem(), 1024, edge_segments),
            Err(InitError::PayloadLogLayoutMismatch)
        ));
    }
}
