//! Fixed-width records for the multi-level labeled CSR layout.

use crate::VertexId;
use crate::labeled::bucket_label_key::{BUCKET_LABEL_INDEX_MASK, BucketLabelKey};
use crate::lara::edge::DEFAULT_MAX_LOG_ENTRIES;
use crate::traits::{CsrEdge, CsrVertex, CsrVertexTombstone};
use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;

const _: () = assert!(DEFAULT_MAX_LOG_ENTRIES <= u8::MAX as u32);

/// One LabelBucket descriptor in the intermediate CSR layer.
///
/// A bucket describes one label's live edge prefix. It intentionally does **not**
/// store an allocation width. Physical edge capacity is owned by the containing
/// [`LabeledVertex`] through [`LabeledVertex::vertex_edge_alloc_slots`].
///
/// Within one non-default vertex, buckets are stored in strictly ascending
/// [`BucketLabelKey`] order. Edge rows are stored in the same order. A bucket's physical
/// successor boundary is therefore:
///
/// - the next bucket's [`Self::edge_start`], or
/// - for the last bucket, the first bucket's `edge_start` plus the vertex's
///   [`LabeledVertex::vertex_edge_alloc_slots`].
///
/// `edge_start` and stored occupancy [`Self::edge_len`] play the same scan role for
/// one label's edge range that [`LabeledVertex::base_slot_start`] /
/// [`LabeledVertex::row_count`] play for LabelBucket ranges.
///
/// When the slab window up to the next bucket boundary is full, additional live
/// edges for this label are stored in the shared [`crate::lara::edge::EdgeStore`]
/// per-segment overflow log; [`Self::overflow_log_head`] is the head index in
/// that log (same contract as [`crate::lara::vertex::Vertex::log_head`]), or
/// `-1` when every live edge sits contiguously on the slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelBucket {
    /// Packed label index + directedness for this contiguous edge range.
    pub bucket_label_key: BucketLabelKey,
    /// Global edge-slot index where this bucket's on-slab edge prefix starts.
    pub edge_start: u64,
    /// Stored edge slots (on-slab prefix plus overflow log chain entries).
    pub edge_len: u32,
    /// Head index in the per-leaf segment overflow log, or `-1` if slab-only.
    pub overflow_log_head: i32,
    /// Deferred deletes not yet folded; logical live edges are
    /// `edge_len - unmaintained_deletes`.
    ///
    /// This fits in `u8` because deferred placeholders are bounded by the per-segment
    /// overflow log capacity (`DEFAULT_MAX_LOG_ENTRIES`, currently 170), and insertion
    /// reuses or rebalances before a bucket can accumulate more pending holes.
    pub unmaintained_deletes: u8,
}

impl Default for LabelBucket {
    fn default() -> Self {
        Self {
            bucket_label_key: BucketLabelKey::default(),
            edge_start: 0,
            edge_len: 0,
            overflow_log_head: -1,
            unmaintained_deletes: 0,
        }
    }
}

impl LabelBucket {
    /// Fixed byte width of one encoded LabelBucket.
    pub const BYTES: usize = 19;

    /// Encodes this LabelBucket into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        bytes[0..2].copy_from_slice(&self.bucket_label_key.to_le_bytes());
        bytes[2..10].copy_from_slice(&self.edge_start.to_le_bytes());
        bytes[10..14].copy_from_slice(&self.edge_len.to_le_bytes());
        bytes[14..18].copy_from_slice(&self.overflow_log_head.to_le_bytes());
        bytes[18] = self.unmaintained_deletes;
    }

    /// Returns a copy with `edge_start` / `edge_len` updated.
    #[inline]
    pub fn with_edge_range(self, edge_start: u64, edge_len: u32) -> Self {
        Self {
            edge_start,
            edge_len,
            ..self
        }
    }

    /// Returns a copy with [`Self::overflow_log_head`] updated.
    #[inline]
    pub fn with_overflow_log_head(self, head: i32) -> Self {
        Self {
            overflow_log_head: head,
            ..self
        }
    }

    /// Returns a copy with [`Self::unmaintained_deletes`] set to `n`.
    #[inline]
    pub fn with_unmaintained_deletes(self, n: u8) -> Self {
        Self {
            unmaintained_deletes: n,
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
            edge_len: u32::from_le_bytes(chunk[10..14].try_into().unwrap()),
            overflow_log_head: i32::from_le_bytes(chunk[14..18].try_into().unwrap()),
            unmaintained_deletes: chunk[18],
        }
    }
}

impl CsrVertex for LabelBucket {
    const BYTES: usize = Self::BYTES;

    fn base_slot_start(&self) -> u64 {
        self.edge_start
    }

    fn degree(&self) -> u32 {
        self.edge_len
            .saturating_sub(u32::from(self.unmaintained_deletes))
    }

    fn stored_degree(&self) -> u32 {
        self.edge_len
    }

    fn with_base_slot_start(mut self, start: u64) -> Self {
        self.edge_start = start;
        self
    }

    fn with_degree(mut self, degree: u32) -> Self {
        self.edge_len = degree;
        self
    }

    fn log_head(self) -> i32 {
        self.overflow_log_head
    }

    fn with_log_head(mut self, idx: i32) -> Self {
        self.overflow_log_head = idx;
        self
    }

    fn after_slab_placeholder_delete(self) -> Self {
        debug_assert!(self.unmaintained_deletes < u8::MAX);
        self.with_unmaintained_deletes(self.unmaintained_deletes.saturating_add(1))
    }

    fn grow_packed_slab_by_one(self) -> Self {
        self.with_degree(self.edge_len.saturating_add(1))
    }

    fn after_slab_insert_reuse_tail_tombstone(self) -> Self {
        self.with_unmaintained_deletes(self.unmaintained_deletes.saturating_sub(1))
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

/// Bit 0 of [`LabeledVertex::metadata`]: vertex points directly into the edge CSR.
const DEFAULT_EDGE_LABELED_BIT: u32 = 1;
/// Bit 1 while bypass is active: homogeneous edges use undirected wire
/// (`default_label` with the directed MSB cleared).
///
/// In normal labeled mode this bit is the LSB of the packed bucket-row reservation
/// ([`LabeledVertex::bucket_alloc_slots`]); it is not interpreted as an undirected flag.
const BYPASS_UNDIRECTED_BIT: u32 = 1 << 1;
const BUCKET_ALLOC_SHIFT: u32 = 1;
const BUCKET_ALLOC_BITS: u32 = 15;
const BUCKET_ALLOC_MASK: u32 = ((1 << BUCKET_ALLOC_BITS) - 1) << BUCKET_ALLOC_SHIFT;
/// Bit 31 of [`LabeledVertex::metadata`]: logical vertex deletion marker.
const VERTEX_TOMBSTONE_BIT: u32 = 1 << 31;
/// Bits 16–23 of [`LabeledVertex::metadata`]: unmaintained logical deletes on a bypass row.
const UNMAINTAINED_BYPASS_DELETE_SHIFT: u32 = 16;
const UNMAINTAINED_BYPASS_DELETE_MASK: u32 = 0xFF << UNMAINTAINED_BYPASS_DELETE_SHIFT;

/// Per-vertex locator for one labeled CSR orientation.
///
/// The locator has two modes:
///
/// - **Homogeneous / default-label bypass:** [`Self::is_default_edge_labeled`] is true.
///   [`Self::base_slot_start`] points directly into [`crate::lara::edge::EdgeStore`],
///   and [`Self::row_count`] is the **stored** out-edge slot count. The logical out-degree
///   is [`CsrVertex::degree`]. No LabelBucket rows are read.
///   The storage label is always the graph's `default_label` plus [`Self::is_bypass_undirected`].
/// - **Normal labeled row:** [`Self::is_default_edge_labeled`] is false.
///   [`Self::base_slot_start`] points into [`crate::labeled::LabelBucketStore`],
///   [`Self::row_count`] is the number of [`LabelBucket`] rows, and
///   [`Self::vertex_edge_alloc_slots`] is the physical width of this vertex's
///   contiguous VertexEdgeSpan.
///
/// Keeping `row_count` as `u32` preserves the default-bypass fast path for high
/// degree vertices. Keeping `metadata` as a full word leaves room for future
/// flags while the edge-span allocation gets its own `u32`, making the encoded
/// row 20 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabeledVertex {
    /// Bucket-slot start in normal mode, or edge-slot start in default-bypass mode.
    pub base_slot_start: u64,
    /// LabelBucket count in normal mode, or stored out-edge slot count in default-bypass mode.
    pub row_count: u32,
    /// Packed metadata: default-label bypass and tombstone bits.
    pub metadata: i32,
    /// Physical edge slots reserved for this vertex's normal-mode VertexEdgeSpan.
    pub vertex_edge_alloc_slots: u32,
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
            raw &= !UNMAINTAINED_BYPASS_DELETE_MASK;
        } else {
            raw &= !DEFAULT_EDGE_LABELED_BIT;
            raw &= !BYPASS_UNDIRECTED_BIT;
            raw &= !UNMAINTAINED_BYPASS_DELETE_MASK;
        }
        self.with_metadata_word(raw)
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

    /// Unmaintained logical deletes on a default-label bypass row (metadata bits 16–23).
    #[inline]
    pub fn unmaintained_bypass_delete_count(self) -> u8 {
        ((self.metadata_word() & UNMAINTAINED_BYPASS_DELETE_MASK)
            >> UNMAINTAINED_BYPASS_DELETE_SHIFT) as u8
    }

    /// Returns a copy with the bypass unmaintained-delete counter set to `n`.
    #[inline]
    pub fn with_unmaintained_bypass_delete_count(self, n: u8) -> Self {
        let mut raw = self.metadata_word();
        raw &= !UNMAINTAINED_BYPASS_DELETE_MASK;
        raw |= u32::from(n) << UNMAINTAINED_BYPASS_DELETE_SHIFT;
        self.with_metadata_word(raw)
    }

    /// Returns the storage label id for a homogeneous bypass row.
    ///
    /// Uses `default_label` plus [`Self::is_bypass_undirected`].
    #[inline]
    pub fn bypass_storage_label(self, default_label: BucketLabelKey) -> BucketLabelKey {
        debug_assert!(self.is_default_edge_labeled());
        if self.is_bypass_undirected() {
            BucketLabelKey::from_raw(default_label.raw() & BUCKET_LABEL_INDEX_MASK)
        } else {
            default_label
        }
    }

    /// Returns a copy configured for homogeneous default-label bypass; `label_key`
    /// sets [`Self::with_bypass_undirected`].
    #[inline]
    pub fn with_homogeneous_bypass_label(self, label_key: BucketLabelKey) -> Self {
        self.with_default_edge_labeled(true)
            .with_bypass_undirected(label_key.is_undirected())
            .with_vertex_edge_alloc_slots(0)
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

    /// Returns the physical LabelBucket slots reserved for this vertex row.
    ///
    /// This may be larger than [`Self::row_count`] so inserting new labels can
    /// shift bucket descriptors in place instead of relocating the whole
    /// LabelBucketStore vertex segment on every label.
    #[inline]
    pub fn bucket_alloc_slots(self) -> u32 {
        (self.metadata_word() & BUCKET_ALLOC_MASK) >> BUCKET_ALLOC_SHIFT
    }

    /// Returns a copy with the LabelBucket row reservation width changed.
    #[inline]
    pub fn with_bucket_alloc_slots(self, slots: u32) -> Self {
        let clamped = slots.min((1 << BUCKET_ALLOC_BITS) - 1);
        let mut raw = self.metadata_word() & !BUCKET_ALLOC_MASK;
        raw |= clamped << BUCKET_ALLOC_SHIFT;
        self.with_metadata_word(raw)
    }

    /// Returns the physical width of this vertex's VertexEdgeSpan.
    ///
    /// This value is meaningful only in normal labeled mode. It is not the live
    /// edge count; the live edge count is the sum of all bucket `edge_len`
    /// values. The difference is slack distributed between LabelEdgeSpans.
    #[inline]
    pub fn vertex_edge_alloc_slots(self) -> u32 {
        self.vertex_edge_alloc_slots
    }

    /// Returns a copy with the VertexEdgeSpan reservation width changed.
    #[inline]
    pub fn with_vertex_edge_alloc_slots(mut self, slots: u32) -> Self {
        self.vertex_edge_alloc_slots = slots;
        self
    }

    /// Returns a copy with bucket locator fields updated together.
    #[inline]
    pub fn with_bucket_row(self, base_slot_start: u64, row_count: u32) -> Self {
        self.with_base_slot_start(base_slot_start)
            .with_degree(row_count)
    }

    /// Returns a copy with bucket locator and reservation fields updated together.
    #[inline]
    pub fn with_bucket_row_and_alloc(
        self,
        base_slot_start: u64,
        row_count: u32,
        bucket_alloc_slots: u32,
    ) -> Self {
        self.with_bucket_row(base_slot_start, row_count)
            .with_bucket_alloc_slots(bucket_alloc_slots)
    }

    /// Encodes this vertex row into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        bytes[0..8].copy_from_slice(&self.base_slot_start.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.row_count.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.metadata.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.vertex_edge_alloc_slots.to_le_bytes());
    }

    /// Decodes a vertex row from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("LabeledVertex::read_from expects exactly 20 bytes");
        Self {
            base_slot_start: u64::from_le_bytes(chunk[0..8].try_into().unwrap()),
            row_count: u32::from_le_bytes(chunk[8..12].try_into().unwrap()),
            metadata: i32::from_le_bytes(chunk[12..16].try_into().unwrap()),
            vertex_edge_alloc_slots: u32::from_le_bytes(chunk[16..20].try_into().unwrap()),
        }
    }
}

impl CsrVertex for LabeledVertex {
    const BYTES: usize = Self::BYTES;

    fn base_slot_start(&self) -> u64 {
        self.base_slot_start
    }

    fn degree(&self) -> u32 {
        if self.is_default_edge_labeled() {
            self.row_count
                .saturating_sub(u32::from(self.unmaintained_bypass_delete_count()))
        } else {
            self.row_count
        }
    }

    fn stored_degree(&self) -> u32 {
        self.row_count
    }

    fn with_base_slot_start(mut self, start: u64) -> Self {
        self.base_slot_start = start;
        self
    }

    fn with_degree(mut self, degree: u32) -> Self {
        self.row_count = degree;
        self
    }

    fn log_head(self) -> i32 {
        -1
    }

    fn with_log_head(self, _idx: i32) -> Self {
        self
    }

    fn after_slab_placeholder_delete(self) -> Self {
        if self.is_default_edge_labeled() {
            debug_assert!(self.unmaintained_bypass_delete_count() < u8::MAX);
            return self.with_unmaintained_bypass_delete_count(
                self.unmaintained_bypass_delete_count().saturating_add(1),
            );
        }
        self.with_degree(self.row_count.saturating_sub(1))
    }

    fn grow_packed_slab_by_one(self) -> Self {
        self.with_degree(self.row_count.saturating_add(1))
    }

    fn after_slab_insert_reuse_tail_tombstone(self) -> Self {
        if self.is_default_edge_labeled() {
            return self.with_unmaintained_bypass_delete_count(
                self.unmaintained_bypass_delete_count().saturating_sub(1),
            );
        }
        self
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
        Self::read_from(bytes.as_ref())
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
            edge_len: 0xAABB_CCDD,
            overflow_log_head: -0x0102_0304,
            unmaintained_deletes: 0,
        };
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(
            bytes,
            [
                0x34, 0x12, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xDD, 0xCC, 0xBB, 0xAA,
                0xFC, 0xFC, 0xFD, 0xFE, 0x00,
            ]
        );
        assert_eq!(LabelBucket::read_from(&bytes), bucket);
        assert_eq!(bucket.base_slot_start(), bucket.edge_start);
        assert_eq!(
            bucket.degree(),
            bucket
                .edge_len
                .saturating_sub(u32::from(bucket.unmaintained_deletes))
        );
    }

    #[test]
    fn labeled_vertex_round_trips_default_bypass_and_tombstone_bits() {
        let vertex = LabeledVertex {
            base_slot_start: 42,
            row_count: 3,
            metadata: 0,
            vertex_edge_alloc_slots: 0,
        }
        .with_default_edge_labeled(true)
        .with_bypass_undirected(true)
        .with_tombstone(true)
        .with_vertex_edge_alloc_slots(0x1234);
        let mut bytes = [0u8; LabeledVertex::BYTES];
        vertex.write_to(&mut bytes);
        let decoded = LabeledVertex::read_from(&bytes);
        assert_eq!(decoded, vertex);
        assert!(decoded.is_default_edge_labeled());
        assert!(decoded.is_bypass_undirected());
        assert_eq!(
            decoded.bypass_storage_label(BucketLabelKey::UNLABELED_DIRECTED),
            BucketLabelKey::UNLABELED_UNDIRECTED
        );
        assert!(decoded.is_tombstone());
        assert_eq!(decoded.vertex_edge_alloc_slots(), 0x1234);
        let with_bucket_alloc = decoded.with_bucket_alloc_slots(37);
        assert_eq!(with_bucket_alloc.bucket_alloc_slots(), 37);
        assert!(with_bucket_alloc.is_default_edge_labeled());
        assert!(with_bucket_alloc.is_tombstone());
    }
}
