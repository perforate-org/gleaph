//! Fixed-width records for the multi-level labeled CSR layout.

use crate::VertexId;
use crate::traits::{CsrEdge, CsrVertex, CsrVertexTombstone};
use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;

/// Edge-label identifier used by the labeled CSR layer.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LabelId(u16);

impl LabelId {
    /// Constructs a label id from its raw numeric value.
    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric value.
    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Returns the little-endian wire encoding.
    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 2] {
        self.0.to_le_bytes()
    }

    /// Decodes a little-endian wire value.
    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 2]) -> Self {
        Self(u16::from_le_bytes(bytes))
    }
}

/// One LabelBucket descriptor in the intermediate CSR layer.
///
/// A bucket describes one label's live edge prefix. It intentionally does **not**
/// store an allocation width. Physical edge capacity is owned by the containing
/// [`LabeledVertex`] through [`LabeledVertex::vertex_edge_alloc_slots`].
///
/// Within one non-default vertex, buckets are stored in strictly ascending
/// [`LabelId`] order. Edge rows are stored in the same order. A bucket's physical
/// successor boundary is therefore:
///
/// - the next bucket's [`Self::edge_start`], or
/// - for the last bucket, the first bucket's `edge_start` plus the vertex's
///   [`LabeledVertex::vertex_edge_alloc_slots`].
///
/// `edge_start` and `edge_len` play the same scan role for one label's edge
/// range that [`LabeledVertex::base_slot_start`] / [`LabeledVertex::row_count`]
/// play for LabelBucket ranges.
///
/// When the slab window up to the next bucket boundary is full, additional live
/// edges for this label are stored in the shared [`crate::lara::edge::EdgeStore`]
/// per-segment overflow log; [`Self::overflow_log_head`] is the head index in
/// that log (same contract as [`crate::lara::vertex::Vertex::log_head`]), or
/// `-1` when every live edge sits contiguously on the slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelBucket {
    /// Relationship type for this contiguous edge range.
    pub label_id: LabelId,
    /// Global edge-slot index where this bucket's on-slab edge prefix starts.
    pub edge_start: u64,
    /// Number of live edges (on-slab prefix plus overflow log chain).
    pub edge_len: u32,
    /// Head index in the per-leaf segment overflow log, or `-1` if slab-only.
    pub overflow_log_head: i32,
}

impl Default for LabelBucket {
    fn default() -> Self {
        Self {
            label_id: LabelId::default(),
            edge_start: 0,
            edge_len: 0,
            overflow_log_head: -1,
        }
    }
}

impl LabelBucket {
    /// Fixed byte width of one encoded LabelBucket.
    pub const BYTES: usize = 18;

    /// Encodes this LabelBucket into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        bytes[0..2].copy_from_slice(&self.label_id.to_le_bytes());
        bytes[2..10].copy_from_slice(&self.edge_start.to_le_bytes());
        bytes[10..14].copy_from_slice(&self.edge_len.to_le_bytes());
        bytes[14..18].copy_from_slice(&self.overflow_log_head.to_le_bytes());
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

    /// Decodes a LabelBucket from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("LabelBucket::read_from expects exactly Self::BYTES bytes");
        Self {
            label_id: LabelId::from_le_bytes([chunk[0], chunk[1]]),
            edge_start: u64::from_le_bytes(chunk[2..10].try_into().unwrap()),
            edge_len: u32::from_le_bytes(chunk[10..14].try_into().unwrap()),
            overflow_log_head: i32::from_le_bytes(chunk[14..18].try_into().unwrap()),
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
        VertexId::from(u32::from(self.label_id.raw()))
    }

    fn with_neighbor_vid(self, _vid: VertexId) -> Self {
        self
    }
}

/// Bit 0 of [`LabeledVertex::metadata`]: vertex points directly into the edge CSR.
const DEFAULT_EDGE_LABELED_BIT: u32 = 1;
/// Bit 31 of [`LabeledVertex::metadata`]: logical vertex deletion marker.
const VERTEX_TOMBSTONE_BIT: u32 = 1 << 31;

/// Per-vertex locator for one labeled CSR orientation.
///
/// The locator has two modes:
///
/// - **Default-label bypass:** [`Self::is_default_edge_labeled`] is true.
///   [`Self::base_slot_start`] points directly into [`crate::lara::edge::EdgeStore`],
///   and [`Self::row_count`] is the number of default-label edges. No LabelBucket rows
///   are read and [`Self::vertex_edge_alloc_slots`] is ignored.
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
    /// LabelBucket count in normal mode, or edge count in default-bypass mode.
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
        } else {
            raw &= !DEFAULT_EDGE_LABELED_BIT;
        }
        self.with_metadata_word(raw)
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
            label_id: LabelId::from_raw(0x1234),
            edge_start: 0x1122_3344_5566_7788,
            edge_len: 0xAABB_CCDD,
            overflow_log_head: -0x0102_0304,
        };
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(
            bytes,
            [
                0x34, 0x12, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xDD, 0xCC, 0xBB, 0xAA,
                0xFC, 0xFC, 0xFD, 0xFE,
            ]
        );
        assert_eq!(LabelBucket::read_from(&bytes), bucket);
        assert_eq!(bucket.base_slot_start(), bucket.edge_start);
        assert_eq!(bucket.degree(), bucket.edge_len);
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
        .with_tombstone(true)
        .with_vertex_edge_alloc_slots(0x1234);
        let mut bytes = [0u8; LabeledVertex::BYTES];
        vertex.write_to(&mut bytes);
        let decoded = LabeledVertex::read_from(&bytes);
        assert_eq!(decoded, vertex);
        assert!(decoded.is_default_edge_labeled());
        assert!(decoded.is_tombstone());
        assert_eq!(decoded.vertex_edge_alloc_slots(), 0x1234);
    }
}
