//! Fixed-width records for the multi-level labeled CSR layout.

use crate::VertexId;
use crate::labeled::bucket_label_key::{BUCKET_LABEL_INDEX_MASK, BucketLabelKey};
use crate::traits::{CsrEdge, CsrVertex, CsrVertexTombstone};
use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;

/// One LabelBucket descriptor in the intermediate CSR layer.
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
    /// Packed label index + directedness for this contiguous edge range.
    pub bucket_label_key: BucketLabelKey,
    /// Global edge-slot index where this bucket's on-slab edge prefix starts.
    pub edge_start: u64,
    /// Logical live edges for this label.
    pub degree: u32,
    /// Stored edge slots (on-slab prefix plus overflow log chain entries).
    pub stored_slots: u32,
    /// Head index in the per-leaf segment overflow log, or `-1` if slab-only.
    pub overflow_log_head: i16,
}

impl Default for LabelBucket {
    fn default() -> Self {
        Self {
            bucket_label_key: BucketLabelKey::default(),
            edge_start: 0,
            degree: 0,
            stored_slots: 0,
            overflow_log_head: -1,
        }
    }
}

impl LabelBucket {
    /// Fixed byte width of one encoded LabelBucket.
    pub const BYTES: usize = 20;

    /// Encodes this LabelBucket into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        bytes[0..2].copy_from_slice(&self.bucket_label_key.to_le_bytes());
        bytes[2..10].copy_from_slice(&self.edge_start.to_le_bytes());
        bytes[10..14].copy_from_slice(&self.degree.to_le_bytes());
        bytes[14..18].copy_from_slice(&self.stored_slots.to_le_bytes());
        bytes[18..20].copy_from_slice(&self.overflow_log_head.to_le_bytes());
    }

    /// Returns a copy with `edge_start` / [`Self::stored_slots`] updated.
    #[inline]
    pub fn with_edge_range(self, edge_start: u64, stored_slots: u32) -> Self {
        Self {
            edge_start,
            stored_slots,
            ..self
        }
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
        debug_assert!(
            i16::try_from(head).is_ok(),
            "LabelBucket overflow log head must fit in i16"
        );
        Self {
            overflow_log_head: head as i16,
            ..self
        }
    }

    /// Decodes a LabelBucket from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("LabelBucket::read_from expects exactly Self::BYTES bytes");
        Self {
            bucket_label_key: BucketLabelKey::from_le_bytes([chunk[0], chunk[1]]),
            edge_start: u64::from_le_bytes(chunk[2..10].try_into().unwrap()),
            degree: u32::from_le_bytes(chunk[10..14].try_into().unwrap()),
            stored_slots: u32::from_le_bytes(chunk[14..18].try_into().unwrap()),
            overflow_log_head: i16::from_le_bytes(chunk[18..20].try_into().unwrap()),
        }
    }
}

impl CsrVertex for LabelBucket {
    const BYTES: usize = Self::BYTES;

    fn base_slot_start(&self) -> u64 {
        self.edge_start
    }

    fn degree(&self) -> u32 {
        self.degree
    }

    fn stored_degree(&self) -> u32 {
        self.stored_slots
    }

    fn with_base_slot_start(mut self, start: u64) -> Self {
        self.edge_start = start;
        self
    }

    fn with_degree(mut self, degree: u32) -> Self {
        self.degree = degree;
        self
    }

    fn log_head(self) -> i32 {
        i32::from(self.overflow_log_head)
    }

    fn with_log_head(mut self, idx: i32) -> Self {
        debug_assert!(
            i16::try_from(idx).is_ok(),
            "LabelBucket overflow log head must fit in i16"
        );
        self.overflow_log_head = idx as i16;
        self
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

    fn write_to(self, bytes: &mut [u8]) {
        self.write_to(bytes);
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(u32::from(self.bucket_label_key.raw()))
    }

    fn with_neighbor_vid(self, _vid: VertexId) -> Self {
        self
    }
}

/// [`LabeledVertex::metadata`] layout (little-endian bit index):
///
/// ```text
/// bit 0      vertex tombstone (highest-priority scan gate)
/// bit 1      default-label bypass active
/// bit 2      bypass stores undirected homogeneous edges
/// bit 3      reserved
/// bits 4–11  bypass overflow log head (`u8`, `0xFF` = none; max index 169)
/// bits 12–27 LabelBucket descriptor slack beyond [`LabeledVertex::degree`] (`u16`, normal only)
/// bits 28–31 reserved
/// ```
const VERTEX_TOMBSTONE_BIT: u32 = 1;
const DEFAULT_EDGE_LABELED_BIT: u32 = 1 << 1;
const BYPASS_UNDIRECTED_BIT: u32 = 1 << 2;
const BYPASS_LOG_HEAD_SHIFT: u32 = 4;
const BYPASS_LOG_HEAD_MASK: u32 = 0xFF << BYPASS_LOG_HEAD_SHIFT;
const BYPASS_LOG_HEAD_NONE: u8 = 0xFF;
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
        }
    }
}

impl std::error::Error for LabeledVertexFieldError {}

#[inline]
fn encode_bypass_overflow_log_head(head: i32) -> u32 {
    let byte = if head < 0 {
        BYPASS_LOG_HEAD_NONE
    } else {
        debug_assert!(
            head < 170,
            "bypass overflow log head must fit in metadata u8 and be below max_log_entries (170)"
        );
        head as u8
    };
    u32::from(byte) << BYPASS_LOG_HEAD_SHIFT
}

#[inline]
fn decode_bypass_overflow_log_head(raw: u32) -> i32 {
    let byte = ((raw & BYPASS_LOG_HEAD_MASK) >> BYPASS_LOG_HEAD_SHIFT) as u8;
    if byte == BYPASS_LOG_HEAD_NONE {
        -1
    } else {
        i32::from(byte)
    }
}

/// Per-vertex locator for one labeled CSR orientation (20 bytes).
///
/// - **Normal:** [`Self::degree`] is the live [`LabelBucket`] row count (≤ [`MAX_VERTEX_LABEL_BUCKETS`]);
///   metadata bits 12–27 hold [`Self::bucket_slack_slots`] so the physical descriptor span is
///   `degree + slack`; [`Self::stored_slots`] is the separate VertexEdgeSpan width for edge bytes.
/// - **Bypass:** [`Self::degree`] is the logical out-edge count (full `u32`); [`Self::stored_slots`]
///   is the stored slab width (tombstones included). Overflow-log head lives in metadata
///   bits 4–11 ([`CsrVertex::log_head`], `0xFF` = slab-only).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabeledVertex {
    /// Bucket-slot start in normal mode, or edge-slot start in default-bypass mode.
    pub base_slot_start: u64,
    /// LabelBucket count (normal) or logical out-edges (bypass).
    pub degree: u32,
    /// VertexEdgeSpan width (normal) or stored bypass edge slots (physical).
    pub stored_slots: u32,
    /// Packed flags (tombstone, bypass mode, bucket reservation).
    pub metadata: i32,
}

impl LabeledVertex {
    /// Fixed byte width of one encoded vertex row.
    pub const BYTES: usize = 20;

    #[inline]
    fn metadata_word(self) -> u32 {
        self.metadata as u32
    }

    #[inline]
    fn with_metadata_word(mut self, raw: u32) -> Self {
        self.metadata = raw as i32;
        self
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
        if self.is_default_edge_labeled() {
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
        self.try_with_label_bucket_count(label_bucket_count)
            .map(|v| v.with_base_slot_start(base_slot_start))
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
        bytes[0..8].copy_from_slice(&self.base_slot_start.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.degree.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.stored_slots.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.metadata.to_le_bytes());
    }

    /// Decodes a vertex row from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        Self::try_read_from(bytes).expect("invalid LabeledVertex wire bytes")
    }

    /// Decodes and validates a vertex row from exactly [`Self::BYTES`] bytes.
    pub fn try_read_from(bytes: &[u8]) -> Result<Self, LabeledVertexFieldError> {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("LabeledVertex::try_read_from expects exactly 20 bytes");
        let vertex = Self {
            base_slot_start: u64::from_le_bytes(chunk[0..8].try_into().unwrap()),
            degree: u32::from_le_bytes(chunk[8..12].try_into().unwrap()),
            stored_slots: u32::from_le_bytes(chunk[12..16].try_into().unwrap()),
            metadata: i32::from_le_bytes(chunk[16..20].try_into().unwrap()),
        };
        vertex.ensure_valid_normal_row()
    }
}

impl CsrVertex for LabeledVertex {
    const BYTES: usize = Self::BYTES;

    fn base_slot_start(&self) -> u64 {
        self.base_slot_start
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

    fn with_base_slot_start(mut self, start: u64) -> Self {
        self.base_slot_start = start;
        self
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
            Some(
                base.checked_add(u64::from(self.stored_slots))?
                    .checked_add(1)?,
            )
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

    #[test]
    fn label_bucket_round_trips_exact_layout() {
        let bucket = LabelBucket {
            bucket_label_key: BucketLabelKey::from_raw(0x1234),
            edge_start: 0x1122_3344_5566_7788,
            degree: 5,
            stored_slots: 9,
            overflow_log_head: -0x0102,
        };
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(LabelBucket::read_from(&bytes), bucket);
        assert_eq!(bucket.base_slot_start(), bucket.edge_start);
        assert!(bucket.stored_slots >= bucket.degree());
    }

    #[test]
    fn labeled_vertex_round_trips_default_bypass_and_tombstone_bits() {
        let vertex = LabeledVertex {
            base_slot_start: 42,
            degree: 3,
            stored_slots: 9,
            metadata: 0,
        }
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
    fn bucket_slack_metadata_is_u16_wide() {
        let vertex = LabeledVertex::default().with_bucket_slack_slots(u16::MAX);
        assert_eq!(vertex.bucket_slack_slots(), u16::MAX);
        let raw = vertex.metadata_word();
        assert_eq!((raw >> 12) & 0xFFFF, u32::from(u16::MAX));
        assert_eq!((raw >> 28) & 0xF, 0, "top metadata bits stay reserved");
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
        let bucket = LabelBucket {
            degree: 2,
            stored_slots: 5,
            ..LabelBucket::default()
        }
        .after_slab_tombstone_delete();
        assert_eq!(bucket.degree, 1);
        assert_eq!(bucket.stored_slots, 5);
    }
}
