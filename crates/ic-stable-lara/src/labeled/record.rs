//! Fixed-width records for the multi-level labeled CSR layout.

use crate::VertexId;
use crate::labeled::bucket_label_key::{BUCKET_LABEL_INDEX_MASK, BucketLabelKey};
use crate::labeled::slot_index::{
    OVERFLOW_LOG_NONE, bucket_word_has_zero_reserved, checked_add_slot_index,
    decode_bucket_label_key, decode_bucket_overflow_log_head, decode_meta28,
    decode_overflow_log_byte, decode_slot_index, encode_locator_word, encode_overflow_log_byte,
    read_u40, replace_bucket_label_key, replace_bucket_overflow_log_head, slot_index_fits,
    try_encode_bucket_word, try_encode_locator_word, try_encode_overflow_log_byte,
    try_replace_slot_index, write_u40,
};
use crate::slab_index::byte_offset_fits;
use crate::traits::{CsrEdge, CsrVertex, CsrVertexTombstone};
use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;

/// One LabelBucket descriptor in the intermediate CSR layer (24 bytes on wire).
///
/// Physical edge capacity within the containing [`LabeledVertex`] VertexEdgeSpan is
/// [`LabeledVertex::stored_slots`]; this row tracks one label's slab prefix and optional
/// overflow log.
///
/// [`Self::degree`] is the logical live edge count. [`Self::stored_slots`] is the stored
/// width (on-slab cells plus overflow-log entries) and may be larger while tombstoned deletes
/// await compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelBucket {
    word: u64,
    /// Logical live edge count for this label bucket.
    pub degree: u32,
    /// Stored slab/log width (may exceed [`Self::degree`] while tombstones await compaction).
    pub stored_slots: u32,
    /// Byte offset into [`EdgeValueStore`] where this bucket's value span starts.
    value_offset: u64,
    /// Physical byte width per edge value slot (`0` = no values).
    value_byte_width: u16,
    /// Wire byte for per-bucket value overflow log head (`0xFF` = none).
    value_log_byte: u8,
}

impl Default for LabelBucket {
    fn default() -> Self {
        Self::from_parts(BucketLabelKey::default(), 0, 0, 0, -1)
    }
}

impl LabelBucket {
    /// Fixed byte width of one encoded LabelBucket.
    pub const BYTES: usize = 24;

    /// Builds a row from logical fields.
    #[inline]
    pub fn from_parts(
        bucket_label_key: BucketLabelKey,
        edge_start: u64,
        degree: u32,
        stored_slots: u32,
        overflow_log_head: i32,
    ) -> Self {
        Self::try_from_parts(
            bucket_label_key,
            edge_start,
            degree,
            stored_slots,
            overflow_log_head,
            0,
            0,
            -1,
        )
        .expect("LabelBucket::from_parts: invalid fields")
    }

    /// Builds a row with edge-value fields.
    #[inline]
    pub fn from_parts_with_value(
        bucket_label_key: BucketLabelKey,
        edge_start: u64,
        degree: u32,
        stored_slots: u32,
        overflow_log_head: i32,
        value_byte_width: u16,
        value_offset: u64,
        value_log_head: i32,
    ) -> Self {
        Self::try_from_parts(
            bucket_label_key,
            edge_start,
            degree,
            stored_slots,
            overflow_log_head,
            value_byte_width,
            value_offset,
            value_log_head,
        )
        .expect("LabelBucket::from_parts_with_value: invalid fields")
    }

    /// Fallible constructor with release-safe range checks.
    #[inline]
    pub fn try_from_parts(
        bucket_label_key: BucketLabelKey,
        edge_start: u64,
        degree: u32,
        stored_slots: u32,
        overflow_log_head: i32,
        value_byte_width: u16,
        value_offset: u64,
        value_log_head: i32,
    ) -> Result<Self, LabelBucketFieldError> {
        if !slot_index_fits(edge_start) {
            return Err(LabelBucketFieldError::SlotIndexOverflow);
        }
        if !byte_offset_fits(value_offset) {
            return Err(LabelBucketFieldError::ValueOffsetOverflow);
        }
        let word = try_encode_bucket_word(edge_start, bucket_label_key, overflow_log_head)
            .ok_or(LabelBucketFieldError::OverflowLogHeadOutOfRange)?;
        let value_log_byte = try_encode_overflow_log_byte(value_log_head)
            .ok_or(LabelBucketFieldError::ValueLogHeadOutOfRange)?;
        Ok(Self {
            word,
            degree,
            stored_slots,
            value_offset,
            value_byte_width,
            value_log_byte,
        })
    }

    /// Label key for this bucket row (directedness in the MSB).
    #[inline]
    pub fn bucket_label_key(self) -> BucketLabelKey {
        decode_bucket_label_key(self.word)
    }

    /// Global edge-slot index where this bucket's slab prefix starts.
    #[inline]
    pub fn edge_start(self) -> u64 {
        decode_slot_index(self.word)
    }

    /// Per-bucket overflow log head, or `-1` when all neighbors are on the slab.
    #[inline]
    pub fn overflow_log_head(self) -> i32 {
        decode_bucket_overflow_log_head(self.word)
    }

    /// Byte offset into `EdgeValueStore` for this bucket's value span.
    #[inline]
    pub fn value_offset(self) -> u64 {
        self.value_offset
    }

    /// Per-bucket value overflow log head, or `-1` when all values are on the slab.
    #[inline]
    pub fn value_log_head(self) -> i32 {
        decode_overflow_log_byte(self.value_log_byte)
    }

    /// Physical byte width per edge value slot (`0` = no values).
    #[inline]
    pub fn value_byte_width(self) -> u16 {
        self.value_byte_width
    }

    /// Returns `true` when this bucket owns a non-empty value span.
    #[inline]
    pub fn is_value_allocated(self) -> bool {
        self.value_byte_width != 0 && self.degree != 0
    }

    #[inline]
    fn with_word(mut self, word: u64) -> Self {
        self.word = word;
        self
    }

    /// Returns a copy with [`Self::value_byte_width`] updated.
    #[inline]
    pub fn with_value_byte_width(self, value_byte_width: u16) -> Self {
        Self {
            value_byte_width,
            ..self
        }
    }

    /// Returns a copy with [`Self::value_offset`] updated.
    #[inline]
    pub fn with_value_offset(self, value_offset: u64) -> Self {
        Self {
            value_offset,
            ..self
        }
    }

    /// Returns a copy with [`Self::value_log_head`] updated.
    #[inline]
    pub fn try_with_value_log_head(self, head: i32) -> Result<Self, LabelBucketFieldError> {
        let value_log_byte = try_encode_overflow_log_byte(head)
            .ok_or(LabelBucketFieldError::ValueLogHeadOutOfRange)?;
        Ok(Self {
            value_log_byte,
            ..self
        })
    }

    /// Returns a copy with [`Self::value_log_head`] updated.
    #[inline]
    pub fn with_value_log_head(self, head: i32) -> Self {
        self.try_with_value_log_head(head)
            .expect("LabelBucket::with_value_log_head: head out of range")
    }

    /// Encodes this LabelBucket into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        let LabelBucket {
            word,
            degree,
            stored_slots,
            value_offset,
            value_byte_width,
            value_log_byte,
        } = self;
        bytes[0..8].copy_from_slice(&word.to_le_bytes());
        bytes[8..12].copy_from_slice(&degree.to_le_bytes());
        bytes[12..16].copy_from_slice(&stored_slots.to_le_bytes());
        let value_wire: &mut [u8; 5] = (&mut bytes[16..21])
            .try_into()
            .expect("LabelBucket value_offset wire slice must be 5 bytes");
        write_u40(value_offset, value_wire);
        bytes[21..23].copy_from_slice(&value_byte_width.to_le_bytes());
        bytes[23] = value_log_byte;
    }

    /// Returns a copy with `edge_start` / [`Self::stored_slots`] updated.
    #[inline]
    pub fn with_edge_range(self, edge_start: u64, stored_slots: u32) -> Self {
        self.try_with_edge_range(edge_start, stored_slots)
            .expect("LabelBucket::with_edge_range: edge_start out of 36-bit range")
    }

    /// Fallible [`Self::with_edge_range`].
    #[inline]
    pub fn try_with_edge_range(
        self,
        edge_start: u64,
        stored_slots: u32,
    ) -> Result<Self, LabelBucketFieldError> {
        let word = try_replace_slot_index(self.word, edge_start)
            .ok_or(LabelBucketFieldError::SlotIndexOverflow)?;
        Ok(Self {
            word,
            stored_slots,
            ..self
        })
    }

    /// Returns a copy with [`Self::bucket_label_key`] updated.
    #[inline]
    pub fn with_bucket_label_key(self, bucket_label_key: BucketLabelKey) -> Self {
        self.with_word(replace_bucket_label_key(self.word, bucket_label_key))
    }

    /// Returns a copy with [`Self::degree`] updated.
    #[inline]
    pub fn with_degree_field(self, degree: u32) -> Self {
        Self { degree, ..self }
    }

    /// Returns a copy with [`Self::stored_slots`] updated.
    #[inline]
    pub fn with_stored_slots(self, stored_slots: u32) -> Self {
        Self {
            stored_slots,
            ..self
        }
    }

    /// Returns a copy with [`Self::overflow_log_head`] updated.
    #[inline]
    pub fn with_overflow_log_head(self, head: i32) -> Self {
        self.try_with_overflow_log_head(head)
            .expect("LabelBucket::with_overflow_log_head: head out of range")
    }

    /// Fallible [`Self::with_overflow_log_head`].
    #[inline]
    pub fn try_with_overflow_log_head(self, head: i32) -> Result<Self, LabelBucketFieldError> {
        let word = replace_bucket_overflow_log_head(self.word, head)
            .ok_or(LabelBucketFieldError::OverflowLogHeadOutOfRange)?;
        Ok(self.with_word(word))
    }

    /// Decodes a LabelBucket from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        Self::try_read_from(bytes).expect("invalid LabelBucket wire bytes")
    }

    /// Decodes and validates a LabelBucket from exactly [`Self::BYTES`] bytes.
    pub fn try_read_from(bytes: &[u8]) -> Result<Self, LabelBucketFieldError> {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("LabelBucket::try_read_from expects exactly Self::BYTES bytes");
        let word = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
        if !bucket_word_has_zero_reserved(word) {
            return Err(LabelBucketFieldError::ReservedTopBitSet);
        }
        let head_byte = ((word >> 52) & 0xFF) as u8;
        if head_byte != OVERFLOW_LOG_NONE && head_byte >= 170 {
            return Err(LabelBucketFieldError::OverflowLogHeadOutOfRange);
        }
        let value_offset = read_u40(&chunk[16..21].try_into().unwrap());
        if !byte_offset_fits(value_offset) {
            return Err(LabelBucketFieldError::ValueOffsetOverflow);
        }
        let value_byte_width = u16::from_le_bytes(chunk[21..23].try_into().unwrap());
        let value_log_byte = chunk[23];
        if value_log_byte != OVERFLOW_LOG_NONE && value_log_byte >= 170 {
            return Err(LabelBucketFieldError::ValueLogHeadOutOfRange);
        }
        Ok(Self {
            word,
            degree: u32::from_le_bytes(chunk[8..12].try_into().unwrap()),
            stored_slots: u32::from_le_bytes(chunk[12..16].try_into().unwrap()),
            value_offset,
            value_byte_width,
            value_log_byte,
        })
    }
}

/// Invalid [`LabelBucket`] wire or field combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelBucketFieldError {
    /// Bit 63 of the packed word must be zero.
    ReservedTopBitSet,
    /// `edge_start` does not fit in the 36-bit slot index.
    SlotIndexOverflow,
    /// Overflow log head byte is not `0xFF` and not in `0..170`.
    OverflowLogHeadOutOfRange,
    /// `value_offset` does not fit in the 40-bit byte-offset space.
    ValueOffsetOverflow,
    /// Value overflow log head byte is out of range.
    ValueLogHeadOutOfRange,
}

impl core::fmt::Display for LabelBucketFieldError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ReservedTopBitSet => write!(f, "label bucket reserved top bit must be zero"),
            Self::SlotIndexOverflow => {
                write!(f, "label bucket edge_start exceeds 36-bit slot index")
            }
            Self::OverflowLogHeadOutOfRange => {
                write!(f, "label bucket overflow log head out of range")
            }
            Self::ValueOffsetOverflow => {
                write!(f, "label bucket value_offset exceeds 40-bit byte offset")
            }
            Self::ValueLogHeadOutOfRange => {
                write!(f, "label bucket value log head out of range")
            }
        }
    }
}

impl std::error::Error for LabelBucketFieldError {}

impl CsrVertex for LabelBucket {
    const BYTES: usize = Self::BYTES;

    fn base_slot_start(&self) -> u64 {
        self.edge_start()
    }

    fn degree(&self) -> u32 {
        self.degree
    }

    fn stored_degree(&self) -> u32 {
        self.stored_slots
    }

    fn with_base_slot_start(self, start: u64) -> Self {
        self.try_with_edge_range(start, self.stored_slots)
            .expect("LabelBucket::with_base_slot_start: slot index overflow")
    }

    fn with_degree(mut self, degree: u32) -> Self {
        self.degree = degree;
        self
    }

    fn log_head(self) -> i32 {
        self.overflow_log_head()
    }

    fn with_log_head(self, idx: i32) -> Self {
        self.with_overflow_log_head(idx)
    }

    fn after_slab_tombstone_delete(self) -> Self {
        self.with_degree(self.degree.saturating_sub(1))
    }

    fn grow_packed_slab_by_one(self) -> Self {
        self.with_degree(self.degree.saturating_add(1))
            .with_stored_slots(self.stored_slots.saturating_add(1))
    }

    fn after_slab_insert_reuse_tail_tombstone(self) -> Self {
        self.with_degree(self.degree.saturating_add(1))
    }
}

impl CsrEdge for LabelBucket {
    const BYTES: usize = LabelBucket::BYTES;

    fn read_from(bytes: &[u8]) -> Self {
        LabelBucket::read_from(bytes)
    }

    fn write_to(&self, bytes: &mut [u8]) {
        LabelBucket::write_to(*self, bytes);
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(u32::from(self.bucket_label_key().raw()))
    }

    fn with_neighbor_vid(&self, _vid: VertexId) -> Self {
        *self
    }
}

/// [`LabeledVertex`] metadata layout in the upper 28 bits of [`LabeledVertex::locator`]:
///
/// ```text
/// bit 0      vertex tombstone (highest-priority scan gate)
/// bit 1      default-label bypass active
/// bit 2      bypass stores undirected homogeneous edges
/// bit 3      reserved
/// bits 4–11  bypass overflow log head (`u8`, `0xFF` = none; max index 169)
/// bits 12–27 LabelBucket descriptor slack beyond [`LabeledVertex::degree`] (`u16`, normal only)
/// ```
const VERTEX_TOMBSTONE_BIT: u32 = 1;
const DEFAULT_EDGE_LABELED_BIT: u32 = 1 << 1;
const BYPASS_UNDIRECTED_BIT: u32 = 1 << 2;
const METADATA28_RESERVED_BIT: u32 = 1 << 3;
const BYPASS_LOG_HEAD_SHIFT: u32 = 4;
const BYPASS_LOG_HEAD_MASK: u32 = 0xFF << BYPASS_LOG_HEAD_SHIFT;
const BUCKET_SLACK_SHIFT: u32 = 12;
const BUCKET_SLACK_BITS: u32 = 16;
const BUCKET_SLACK_MASK: u32 = ((1 << BUCKET_SLACK_BITS) - 1) << BUCKET_SLACK_SHIFT;

/// Maximum live [`LabelBucket`] rows per vertex (`BucketLabelKey` wire space size).
pub const MAX_VERTEX_LABEL_BUCKETS: u32 = u16::MAX as u32 + 1;

/// Maximum slack slots reserved past [`LabeledVertex::degree`] in metadata.
pub const MAX_VERTEX_LABEL_BUCKET_SLACK: u16 = u16::MAX;

/// Invalid [`LabeledVertex`] field combinations for normal (bucket) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabeledVertexFieldError {
    /// Normal-mode [`LabeledVertex::degree`] exceeds [`MAX_VERTEX_LABEL_BUCKETS`].
    LabelBucketCountOverflow,
    /// Descriptor span `degree + slack` does not fit in `u32`.
    LabelBucketDescriptorSpanOverflow,
    /// `base_slot_start` does not fit in the 36-bit slot index.
    SlotIndexOverflow,
    /// Metadata bit 3 (reserved) must be zero on wire.
    MetadataReservedBitSet,
    /// Bypass overflow log head byte is out of range.
    BypassOverflowLogHeadOutOfRange,
    /// [`Self::value_allocated_bytes`] does not fit in the 40-bit byte-offset space.
    ValueAllocatedBytesOverflow,
}

impl core::fmt::Display for LabeledVertexFieldError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::LabelBucketCountOverflow => write!(
                f,
                "label bucket row count exceeds MAX_VERTEX_LABEL_BUCKETS ({MAX_VERTEX_LABEL_BUCKETS})"
            ),
            Self::LabelBucketDescriptorSpanOverflow => {
                write!(
                    f,
                    "label bucket descriptor span (degree + slack) overflows u32"
                )
            }
            Self::SlotIndexOverflow => {
                write!(f, "vertex base_slot_start exceeds 36-bit slot index")
            }
            Self::MetadataReservedBitSet => {
                write!(f, "vertex metadata reserved bit 3 must be zero")
            }
            Self::BypassOverflowLogHeadOutOfRange => {
                write!(f, "bypass overflow log head out of range")
            }
            Self::ValueAllocatedBytesOverflow => {
                write!(f, "vertex value_allocated_bytes exceeds 40-bit byte offset")
            }
        }
    }
}

impl std::error::Error for LabeledVertexFieldError {}

#[inline]
fn encode_bypass_overflow_log_head(head: i32) -> u32 {
    let byte = encode_overflow_log_byte(head);
    u32::from(byte) << BYPASS_LOG_HEAD_SHIFT
}

#[inline]
fn decode_bypass_overflow_log_head(raw: u32) -> i32 {
    let byte = ((raw & BYPASS_LOG_HEAD_MASK) >> BYPASS_LOG_HEAD_SHIFT) as u8;
    decode_overflow_log_byte(byte)
}

/// Per-vertex locator for one labeled CSR orientation (21 bytes).
///
/// - **Normal:** [`Self::degree`] is the live [`LabelBucket`] row count (≤ [`MAX_VERTEX_LABEL_BUCKETS`]);
///   locator bits 36–63 hold [`Self::bucket_slack_slots`] so the physical descriptor span is
///   `degree + slack`; [`Self::stored_slots`] is the separate VertexEdgeSpan width for edge bytes.
/// - **Bypass:** [`Self::degree`] is the logical out-edge count (full `u32`); [`Self::stored_slots`]
///   is the stored slab width (tombstones included). Overflow-log head lives in metadata28
///   bits 4–11 ([`CsrVertex::log_head`], wire byte `0xFF` = slab-only).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabeledVertex {
    locator: u64,
    /// Logical out-degree or live label-bucket row count (mode-dependent).
    pub degree: u32,
    /// Stored edge-slab width for this vertex's VertexEdgeSpan (tombstones included).
    pub stored_slots: u32,
    /// Physical byte width of this vertex's value span in `EdgeValueStore` (slack included).
    value_allocated_bytes: u64,
}

impl LabeledVertex {
    /// Fixed byte width of one encoded vertex row.
    pub const BYTES: usize = 21;

    /// Builds a row from logical fields.
    #[inline]
    pub fn from_parts(
        base_slot_start: u64,
        degree: u32,
        stored_slots: u32,
        metadata28: u32,
    ) -> Self {
        Self::try_from_parts(base_slot_start, degree, stored_slots, 0, metadata28)
            .expect("LabeledVertex::from_parts: invalid fields")
    }

    /// Fallible constructor with release-safe range checks.
    #[inline]
    pub fn try_from_parts(
        base_slot_start: u64,
        degree: u32,
        stored_slots: u32,
        value_allocated_bytes: u64,
        metadata28: u32,
    ) -> Result<Self, LabeledVertexFieldError> {
        if !slot_index_fits(base_slot_start) {
            return Err(LabeledVertexFieldError::SlotIndexOverflow);
        }
        if !byte_offset_fits(value_allocated_bytes) {
            return Err(LabeledVertexFieldError::ValueAllocatedBytesOverflow);
        }
        if metadata28 & METADATA28_RESERVED_BIT != 0 {
            return Err(LabeledVertexFieldError::MetadataReservedBitSet);
        }
        let locator = try_encode_locator_word(base_slot_start, metadata28)
            .ok_or(LabeledVertexFieldError::SlotIndexOverflow)?;
        Ok(Self {
            locator,
            degree,
            stored_slots,
            value_allocated_bytes,
        })
    }

    /// Physical byte width reserved for edge values on this vertex.
    #[inline]
    pub fn value_allocated_bytes(self) -> u64 {
        self.value_allocated_bytes
    }

    /// Returns a copy with [`Self::value_allocated_bytes`] updated.
    #[inline]
    pub fn with_value_allocated_bytes(self, bytes: u64) -> Self {
        Self {
            value_allocated_bytes: bytes,
            ..self
        }
    }

    /// Returns a copy with [`Self::value_allocated_bytes`] updated, or an error if it does not fit.
    #[inline]
    pub fn try_with_value_allocated_bytes(
        self,
        bytes: u64,
    ) -> Result<Self, LabeledVertexFieldError> {
        if !byte_offset_fits(bytes) {
            return Err(LabeledVertexFieldError::ValueAllocatedBytesOverflow);
        }
        Ok(self.with_value_allocated_bytes(bytes))
    }

    /// Global label-bucket descriptor base (normal mode) or edge-slab base (bypass mode).
    #[inline]
    pub fn base_slot_start(self) -> u64 {
        decode_slot_index(self.locator)
    }

    /// Raw 28-bit metadata word in the locator (mode flags, slack, bypass log head, …).
    #[inline]
    pub fn metadata28(self) -> u32 {
        decode_meta28(self.locator)
    }

    #[inline]
    fn metadata_word(self) -> u32 {
        self.metadata28()
    }

    #[inline]
    fn with_locator(mut self, locator: u64) -> Self {
        self.locator = locator;
        self
    }

    #[inline]
    fn with_metadata_word(self, raw: u32) -> Self {
        self.with_locator(encode_locator_word(self.base_slot_start(), raw))
    }

    /// Returns `true` when this vertex points directly into the edge CSR.
    #[inline]
    pub fn is_default_edge_labeled(self) -> bool {
        (self.metadata_word() & DEFAULT_EDGE_LABELED_BIT) != 0
    }

    /// Returns a copy with the default-label bypass flag changed.
    #[inline]
    pub fn with_default_edge_labeled(self, enabled: bool) -> Self {
        let mut raw = self.metadata_word();
        if enabled {
            raw |= DEFAULT_EDGE_LABELED_BIT;
            raw &= !BUCKET_SLACK_MASK;
            raw &= !BYPASS_LOG_HEAD_MASK;
            raw |= encode_bypass_overflow_log_head(-1);
            self.with_metadata_word(raw)
                .with_degree(0)
                .with_stored_slots(0)
        } else {
            raw &= !DEFAULT_EDGE_LABELED_BIT;
            raw &= !BYPASS_UNDIRECTED_BIT;
            raw &= !BYPASS_LOG_HEAD_MASK;
            self.with_metadata_word(raw)
        }
    }

    /// Returns `true` when bypass mode stores undirected homogeneous edges (`label | 0x8000`).
    #[inline]
    pub fn is_bypass_undirected(self) -> bool {
        self.is_default_edge_labeled() && (self.metadata_word() & BYPASS_UNDIRECTED_BIT) != 0
    }

    /// Returns a copy with the bypass undirected flag changed (only meaningful in bypass mode).
    #[inline]
    pub fn with_bypass_undirected(self, undirected: bool) -> Self {
        let mut raw = self.metadata_word();
        if undirected {
            raw |= BYPASS_UNDIRECTED_BIT;
        } else {
            raw &= !BYPASS_UNDIRECTED_BIT;
        }
        self.with_metadata_word(raw)
    }

    /// Returns the storage label id for a homogeneous bypass row.
    #[inline]
    pub fn bypass_storage_label(self, default_label: BucketLabelKey) -> BucketLabelKey {
        debug_assert!(self.is_default_edge_labeled());
        if self.is_bypass_undirected() {
            BucketLabelKey::from_raw(default_label.raw() & BUCKET_LABEL_INDEX_MASK)
        } else {
            default_label
        }
    }

    /// Returns a copy configured for homogeneous default-label bypass.
    #[inline]
    pub fn with_homogeneous_bypass_label(self, label_key: BucketLabelKey) -> Self {
        self.with_default_edge_labeled(true)
            .with_bypass_undirected(label_key.is_undirected())
    }

    /// Returns `true` when the vertex row is a tombstone.
    #[inline]
    pub fn is_tombstone(self) -> bool {
        (self.metadata_word() & VERTEX_TOMBSTONE_BIT) != 0
    }

    /// Returns a copy with the tombstone flag changed.
    #[inline]
    pub fn with_tombstone(self, tomb: bool) -> Self {
        let mut raw = self.metadata_word();
        if tomb {
            raw |= VERTEX_TOMBSTONE_BIT;
        } else {
            raw &= !VERTEX_TOMBSTONE_BIT;
        }
        self.with_metadata_word(raw)
    }

    /// Extra LabelBucket descriptor slots reserved past live [`Self::degree`] (normal mode).
    #[inline]
    pub fn bucket_slack_slots(self) -> u16 {
        ((self.metadata_word() & BUCKET_SLACK_MASK) >> BUCKET_SLACK_SHIFT) as u16
    }

    /// Returns a copy with LabelBucket descriptor slack changed (normal mode only).
    #[inline]
    pub fn with_bucket_slack_slots(self, slack: u16) -> Self {
        let clamped = u32::from(slack.min(MAX_VERTEX_LABEL_BUCKET_SLACK));
        let mut raw = self.metadata_word() & !BUCKET_SLACK_MASK;
        raw |= clamped << BUCKET_SLACK_SHIFT;
        self.with_metadata_word(raw)
    }

    /// Physical LabelBucket descriptor span: [`Self::degree`] + [`Self::bucket_slack_slots`].
    #[inline]
    pub fn label_bucket_descriptor_span(self) -> Option<u32> {
        if self.is_default_edge_labeled() {
            return None;
        }
        self.degree()
            .checked_add(u32::from(self.bucket_slack_slots()))
    }

    /// Returns `true` when `count` is a valid normal-mode LabelBucket row count.
    #[inline]
    pub fn label_bucket_count_fits(count: u32) -> bool {
        count <= MAX_VERTEX_LABEL_BUCKETS
    }

    /// Slack for a physical descriptor span and live row count.
    #[inline]
    pub fn bucket_slack_for_descriptor_span(live_rows: u32, physical_span: u32) -> Option<u16> {
        let slack = physical_span.checked_sub(live_rows)?;
        u16::try_from(slack).ok()
    }

    /// Bypass-only overflow log head from metadata bits 4–11 (`-1` when absent).
    #[inline]
    pub fn bypass_overflow_log_head(self) -> i32 {
        if self.is_default_edge_labeled() {
            decode_bypass_overflow_log_head(self.metadata_word())
        } else {
            -1
        }
    }

    /// Returns a copy with the bypass overflow log head changed (bypass mode only).
    #[inline]
    pub fn with_bypass_overflow_log_head(self, head: i32) -> Self {
        debug_assert!(self.is_default_edge_labeled());
        let mut raw = self.metadata_word() & !BYPASS_LOG_HEAD_MASK;
        raw |= encode_bypass_overflow_log_head(head);
        self.with_metadata_word(raw)
    }

    /// Validates normal-mode label-bucket row count and descriptor span.
    #[inline]
    pub fn ensure_valid_normal_row(self) -> Result<Self, LabeledVertexFieldError> {
        if self.metadata_word() & METADATA28_RESERVED_BIT != 0 {
            return Err(LabeledVertexFieldError::MetadataReservedBitSet);
        }
        if self.is_default_edge_labeled() {
            let head_byte =
                ((self.metadata_word() & BYPASS_LOG_HEAD_MASK) >> BYPASS_LOG_HEAD_SHIFT) as u8;
            if head_byte != OVERFLOW_LOG_NONE && head_byte >= 170 {
                return Err(LabeledVertexFieldError::BypassOverflowLogHeadOutOfRange);
            }
            return Ok(self);
        }
        if !Self::label_bucket_count_fits(self.degree) {
            return Err(LabeledVertexFieldError::LabelBucketCountOverflow);
        }
        if self.label_bucket_descriptor_span().is_none() {
            return Err(LabeledVertexFieldError::LabelBucketDescriptorSpanOverflow);
        }
        Ok(self)
    }

    /// Returns a copy with normal-mode label-bucket row count, or an error if it does not fit.
    #[inline]
    pub fn try_with_label_bucket_count(
        self,
        label_bucket_count: u32,
    ) -> Result<Self, LabeledVertexFieldError> {
        if self.is_default_edge_labeled() {
            return Ok(self.with_degree(label_bucket_count));
        }
        if !Self::label_bucket_count_fits(label_bucket_count) {
            return Err(LabeledVertexFieldError::LabelBucketCountOverflow);
        }
        Ok(self.with_degree(label_bucket_count))
    }

    /// Returns a copy with bucket locator fields updated together.
    #[inline]
    pub fn try_with_bucket_row(
        self,
        base_slot_start: u64,
        label_bucket_count: u32,
    ) -> Result<Self, LabeledVertexFieldError> {
        self.try_with_label_bucket_count(label_bucket_count)?
            .try_with_base_slot_start(base_slot_start)
    }

    /// Returns a copy with [`Self::base_slot_start`] updated, or an error if it does not fit.
    #[inline]
    pub fn try_with_base_slot_start(
        self,
        base_slot_start: u64,
    ) -> Result<Self, LabeledVertexFieldError> {
        let locator = try_replace_slot_index(self.locator, base_slot_start)
            .ok_or(LabeledVertexFieldError::SlotIndexOverflow)?;
        Ok(self.with_locator(locator))
    }

    /// Returns a copy with bucket locator, live count, and descriptor slack updated together.
    #[inline]
    pub fn try_with_bucket_row_and_slack(
        self,
        base_slot_start: u64,
        label_bucket_count: u32,
        bucket_slack_slots: u16,
    ) -> Result<Self, LabeledVertexFieldError> {
        self.try_with_bucket_row(base_slot_start, label_bucket_count)
            .map(|v| v.with_bucket_slack_slots(bucket_slack_slots))
            .and_then(LabeledVertex::ensure_valid_normal_row)
    }

    /// Returns a copy with bucket locator fields updated together.
    ///
    /// Panics in debug builds when `label_bucket_count` does not fit; use
    /// [`Self::try_with_bucket_row`] in release-safe paths.
    #[inline]
    pub fn with_bucket_row(self, base_slot_start: u64, label_bucket_count: u32) -> Self {
        self.try_with_bucket_row(base_slot_start, label_bucket_count)
            .expect("label bucket count overflow")
    }

    /// Returns a copy with bucket locator, live count, and descriptor slack updated together.
    ///
    /// Panics in debug builds on overflow; use [`Self::try_with_bucket_row_and_slack`] otherwise.
    #[inline]
    pub fn with_bucket_row_and_slack(
        self,
        base_slot_start: u64,
        label_bucket_count: u32,
        bucket_slack_slots: u16,
    ) -> Self {
        self.try_with_bucket_row_and_slack(base_slot_start, label_bucket_count, bucket_slack_slots)
            .expect("label bucket row overflow")
    }

    /// Returns a copy with [`Self::stored_slots`] updated.
    #[inline]
    pub fn with_stored_slots(mut self, slots: u32) -> Self {
        self.stored_slots = slots;
        self
    }

    /// Encodes this vertex row into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        bytes[0..8].copy_from_slice(&self.locator.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.degree.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.stored_slots.to_le_bytes());
        let value_alloc_wire: &mut [u8; 5] = (&mut bytes[16..21])
            .try_into()
            .expect("LabeledVertex value_allocated_bytes wire slice must be 5 bytes");
        write_u40(self.value_allocated_bytes, value_alloc_wire);
    }

    /// Decodes a vertex row from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        Self::try_read_from(bytes).expect("invalid LabeledVertex wire bytes")
    }

    /// Decodes and validates a vertex row from exactly [`Self::BYTES`] bytes.
    pub fn try_read_from(bytes: &[u8]) -> Result<Self, LabeledVertexFieldError> {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("LabeledVertex::try_read_from expects exactly Self::BYTES bytes");
        let value_allocated_bytes = read_u40(&chunk[16..21].try_into().unwrap());
        if !byte_offset_fits(value_allocated_bytes) {
            return Err(LabeledVertexFieldError::ValueAllocatedBytesOverflow);
        }
        let vertex = Self {
            locator: u64::from_le_bytes(chunk[0..8].try_into().unwrap()),
            degree: u32::from_le_bytes(chunk[8..12].try_into().unwrap()),
            stored_slots: u32::from_le_bytes(chunk[12..16].try_into().unwrap()),
            value_allocated_bytes,
        };
        vertex.ensure_valid_normal_row()
    }
}

impl CsrVertex for LabeledVertex {
    const BYTES: usize = Self::BYTES;

    fn base_slot_start(&self) -> u64 {
        decode_slot_index(self.locator)
    }

    fn degree(&self) -> u32 {
        self.degree
    }

    /// Layout width for [`crate::lara::edge::EdgeStore::insert_edge`]:
    /// bypass physical slab slots ([`Self::stored_slots`]), or normal live bucket rows ([`Self::degree`]).
    ///
    /// Normal-mode edge bytes are *not* sized by this value; they use [`Self::stored_slots`] on the
    /// vertex row and per-[`LabelBucket`] spans instead.
    fn stored_degree(&self) -> u32 {
        if self.is_default_edge_labeled() {
            self.stored_slots
        } else {
            self.degree
        }
    }

    fn with_base_slot_start(self, start: u64) -> Self {
        self.try_with_base_slot_start(start)
            .expect("LabeledVertex::with_base_slot_start: slot index overflow")
    }

    fn with_degree(mut self, degree: u32) -> Self {
        self.degree = degree;
        self
    }

    fn log_head(self) -> i32 {
        self.bypass_overflow_log_head()
    }

    fn with_log_head(self, idx: i32) -> Self {
        if self.is_default_edge_labeled() {
            LabeledVertex::with_bypass_overflow_log_head(self, idx)
        } else {
            self
        }
    }

    fn slab_append_exclusive_end(self, base: u64) -> Option<u64> {
        if self.is_default_edge_labeled() {
            let end = checked_add_slot_index(base, u64::from(self.stored_slots))?;
            checked_add_slot_index(end, 1)
        } else {
            None
        }
    }

    fn after_slab_tombstone_delete(self) -> Self {
        if self.is_default_edge_labeled() {
            self.with_degree(self.degree.saturating_sub(1))
        } else {
            self
        }
    }

    fn try_grow_packed_slab_by_one(self) -> Result<Self, ()> {
        if self.is_default_edge_labeled() {
            let next_degree = self.degree.checked_add(1).ok_or(())?;
            let next_stored = self.stored_slots.checked_add(1).ok_or(())?;
            Ok(self.with_degree(next_degree).with_stored_slots(next_stored))
        } else {
            let next_degree = self.degree.checked_add(1).ok_or(())?;
            if !Self::label_bucket_count_fits(next_degree) {
                return Err(());
            }
            Ok(self.with_degree(next_degree))
        }
    }

    fn grow_packed_slab_by_one(self) -> Self {
        match self.try_grow_packed_slab_by_one() {
            Ok(grown) => grown,
            Err(()) => {
                debug_assert!(
                    false,
                    "grow_packed_slab_by_one: overflow (insert_edge should reject first)"
                );
                self
            }
        }
    }

    fn after_slab_insert_reuse_tail_tombstone(self) -> Self {
        if self.is_default_edge_labeled() {
            self.with_degree(self.degree.saturating_add(1))
        } else {
            self
        }
    }
}

impl CsrVertexTombstone for LabeledVertex {
    fn is_tombstone(&self) -> bool {
        (*self).is_tombstone()
    }

    fn with_tombstone(self, tomb: bool) -> Self {
        LabeledVertex::with_tombstone(self, tomb)
    }
}

impl Storable for LabeledVertex {
    const BOUND: Bound = Bound::Bounded {
        max_size: LabeledVertex::BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut bytes = [0u8; LabeledVertex::BYTES];
        self.write_to(&mut bytes);
        Cow::Owned(Vec::from(bytes))
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut bytes = [0u8; LabeledVertex::BYTES];
        self.write_to(&mut bytes);
        Vec::from(bytes)
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        Self::try_read_from(bytes.as_ref())
            .expect("LabeledVertex stable bytes failed normal-row validation")
    }
}

impl Storable for LabelBucket {
    const BOUND: Bound = Bound::Bounded {
        max_size: LabelBucket::BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut bytes = [0u8; LabelBucket::BYTES];
        self.write_to(&mut bytes);
        Cow::Owned(Vec::from(bytes))
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut bytes = [0u8; LabelBucket::BYTES];
        self.write_to(&mut bytes);
        Vec::from(bytes)
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        Self::read_from(bytes.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labeled::slot_index::SLOT_INDEX_MASK;
    use core::mem;

    #[test]
    fn wire_rows_match_documented_layout() {
        assert_eq!(LabeledVertex::BYTES, 21);
        assert_eq!(LabelBucket::BYTES, 24);
        assert!(mem::size_of::<LabeledVertex>() >= LabeledVertex::BYTES);
        assert!(mem::size_of::<LabelBucket>() >= LabelBucket::BYTES);
    }

    #[test]
    fn labeled_vertex_wire_bytes_golden() {
        let vertex = LabeledVertex::from_parts(42, 3, 9, 0);
        let mut bytes = [0u8; LabeledVertex::BYTES];
        vertex.write_to(&mut bytes);
        let mut expected = [0u8; LabeledVertex::BYTES];
        expected[0..8].copy_from_slice(&42u64.to_le_bytes());
        expected[8..12].copy_from_slice(&3u32.to_le_bytes());
        expected[12..16].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(bytes, expected);
    }

    #[test]
    fn label_bucket_wire_bytes_golden() {
        let bucket =
            LabelBucket::from_parts(BucketLabelKey::from_raw(0x1234), 0x0F_FFFF_FFFE, 5, 9, 42);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(bucket.edge_start(), 0x0F_FFFF_FFFE);
        assert_eq!(bucket.bucket_label_key().raw(), 0x1234);
        assert_eq!(bucket.overflow_log_head(), 42);
        assert_eq!(bytes[8..12], 5u32.to_le_bytes());
        assert_eq!(bytes[12..16], 9u32.to_le_bytes());
        assert_eq!(LabelBucket::read_from(&bytes), bucket);
    }

    #[test]
    fn try_from_parts_rejects_out_of_range_fields() {
        assert_eq!(
            LabelBucket::try_from_parts(
                BucketLabelKey::default(),
                SLOT_INDEX_MASK + 1,
                0,
                0,
                -1,
                0u16,
                0,
                -1,
            ),
            Err(LabelBucketFieldError::SlotIndexOverflow)
        );
        assert_eq!(
            LabelBucket::try_from_parts(BucketLabelKey::default(), 0, 0, 0, 170, 0u16, 0, -1,),
            Err(LabelBucketFieldError::OverflowLogHeadOutOfRange)
        );
        assert_eq!(
            LabeledVertex::try_from_parts(SLOT_INDEX_MASK + 1, 0, 0, 0, 0),
            Err(LabeledVertexFieldError::SlotIndexOverflow)
        );
        assert_eq!(
            LabeledVertex::try_from_parts(0, 0, 0, 0, METADATA28_RESERVED_BIT),
            Err(LabeledVertexFieldError::MetadataReservedBitSet)
        );
    }

    #[test]
    fn try_with_base_slot_start_rejects_slot_overflow() {
        let vertex = LabeledVertex::default();
        let err = vertex
            .try_with_base_slot_start(SLOT_INDEX_MASK + 1)
            .expect_err("slot overflow");
        assert_eq!(err, LabeledVertexFieldError::SlotIndexOverflow);
    }

    #[test]
    fn label_bucket_round_trips_exact_layout() {
        let bucket =
            LabelBucket::from_parts(BucketLabelKey::from_raw(0x1234), 0x0F_FFFF_FFFE, 5, 9, 42);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(LabelBucket::read_from(&bytes), bucket);
        assert_eq!(bucket.base_slot_start(), bucket.edge_start());
        assert!(bucket.stored_slots >= bucket.degree());
    }

    #[test]
    fn label_bucket_rejects_nonzero_reserved_top_bit() {
        let bucket = LabelBucket::from_parts(BucketLabelKey::default(), 0, 0, 0, -1);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        bytes[7] |= 0x80;
        let err = LabelBucket::try_read_from(&bytes).expect_err("reserved top bit");
        assert_eq!(err, LabelBucketFieldError::ReservedTopBitSet);
    }

    #[test]
    fn label_bucket_round_trips_w64_value_byte_width() {
        let bucket = LabelBucket::from_parts_with_value(
            BucketLabelKey::default(),
            0,
            0,
            0,
            -1,
            64u16,
            0,
            -1,
        );
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        let decoded = LabelBucket::try_read_from(&bytes).expect("decode");
        assert_eq!(decoded.value_byte_width(), 64u16);
        assert_eq!(decoded.value_byte_width(), 64);
    }

    #[test]
    fn labeled_vertex_round_trips_default_bypass_and_tombstone_bits() {
        let vertex = LabeledVertex::from_parts(42, 3, 9, 0)
            .with_default_edge_labeled(true)
            .with_bypass_undirected(true)
            .with_tombstone(true);
        let mut bytes = [0u8; LabeledVertex::BYTES];
        vertex.write_to(&mut bytes);
        let decoded = LabeledVertex::read_from(&bytes);
        assert_eq!(decoded.degree, 0);
        assert_eq!(decoded.stored_slots, 0);
        assert!(decoded.is_default_edge_labeled());
        assert!(decoded.is_bypass_undirected());
        assert_eq!(
            decoded.bypass_storage_label(BucketLabelKey::UNLABELED_DIRECTED),
            BucketLabelKey::UNLABELED_UNDIRECTED
        );
        assert!(decoded.is_tombstone());
        assert_eq!(decoded.bypass_overflow_log_head(), -1);
        let with_log = decoded.with_bypass_overflow_log_head(42);
        assert_eq!(with_log.bypass_overflow_log_head(), 42);
        let log_cleared = with_log.with_bypass_overflow_log_head(-1);
        assert_eq!(log_cleared.bypass_overflow_log_head(), -1);
        let normal = decoded
            .with_default_edge_labeled(false)
            .with_degree(2)
            .with_stored_slots(0x1234);
        assert_eq!(normal.degree, 2);
        assert_eq!(normal.stored_slots, 0x1234);
        let with_slack = normal.with_bucket_slack_slots(37);
        assert_eq!(with_slack.bucket_slack_slots(), 37);
        assert_eq!(with_slack.label_bucket_descriptor_span(), Some(39));
        assert!(!with_slack.is_default_edge_labeled());
    }

    #[test]
    fn label_bucket_value_offset_round_trips_on_wire() {
        let bucket = LabelBucket::from_parts_with_value(
            BucketLabelKey::from_raw(2),
            10,
            2,
            2,
            -1,
            2u16,
            4,
            -1,
        );
        assert_eq!(bucket.value_offset(), 4);
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(bytes[16], 4);
        let decoded = LabelBucket::read_from(&bytes);
        assert_eq!(decoded.value_offset(), 4);
    }

    #[test]
    fn bucket_slack_metadata_is_u16_wide() {
        let vertex = LabeledVertex::default().with_bucket_slack_slots(u16::MAX);
        assert_eq!(vertex.bucket_slack_slots(), u16::MAX);
        let raw = vertex.metadata28();
        assert_eq!((raw >> 12) & 0xFFFF, u32::from(u16::MAX));
    }

    #[test]
    fn label_bucket_descriptor_span_is_degree_plus_slack() {
        let vertex = LabeledVertex::default()
            .with_degree(5)
            .with_bucket_slack_slots(10);
        assert_eq!(vertex.label_bucket_descriptor_span(), Some(15));
    }

    #[test]
    fn label_bucket_count_fits_matches_wire_space() {
        assert!(LabeledVertex::label_bucket_count_fits(
            MAX_VERTEX_LABEL_BUCKETS
        ));
        assert!(!LabeledVertex::label_bucket_count_fits(
            MAX_VERTEX_LABEL_BUCKETS + 1
        ));
    }

    #[test]
    fn try_with_label_bucket_count_rejects_overflow_in_release() {
        let vertex = LabeledVertex::default();
        let err = vertex
            .try_with_label_bucket_count(MAX_VERTEX_LABEL_BUCKETS + 1)
            .expect_err("overflow must be rejected");
        assert_eq!(err, LabeledVertexFieldError::LabelBucketCountOverflow);
    }

    #[test]
    fn try_read_from_rejects_normal_row_with_overflow_degree() {
        let vertex = LabeledVertex::default().with_degree(MAX_VERTEX_LABEL_BUCKETS + 1);
        let mut bytes = [0u8; LabeledVertex::BYTES];
        vertex.write_to(&mut bytes);
        let err = LabeledVertex::try_read_from(&bytes).expect_err("wire row must be rejected");
        assert_eq!(err, LabeledVertexFieldError::LabelBucketCountOverflow);
    }

    #[test]
    fn bypass_overflow_log_head_respects_max_log_entries() {
        let vertex =
            LabeledVertex::default().with_homogeneous_bypass_label(BucketLabelKey::from_raw(1));
        let at_max = vertex.with_bypass_overflow_log_head(169);
        assert_eq!(at_max.bypass_overflow_log_head(), 169);
    }

    #[test]
    fn label_bucket_tombstone_delete_keeps_physical_width() {
        let bucket = LabelBucket::from_parts(BucketLabelKey::default(), 0, 2, 5, -1)
            .after_slab_tombstone_delete();
        assert_eq!(bucket.degree, 1);
        assert_eq!(bucket.stored_slots, 5);
    }
}
