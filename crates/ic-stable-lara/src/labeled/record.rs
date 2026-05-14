//! Fixed-width records for the multi-level labeled CSR layout.

use crate::traits::CsrVertex;
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

/// One label bucket row in the intermediate CSR layer.
///
/// `edge_start` and `edge_len` play the same role for an edge range that
/// [`LabeledVertex::base_slot_start`] / [`LabeledVertex::row_count`] play for
/// bucket rows.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabelBucket {
    /// Relationship type for this contiguous edge range.
    pub label_id: LabelId,
    /// Reserved for alignment / future bucket flags.
    pub reserved: u16,
    /// Global edge-slot index where this bucket's clean edge prefix starts.
    pub edge_start: u64,
    /// Number of live edges visible through clean scans of this bucket.
    pub edge_len: u32,
    /// Reserved padding to keep the row width stable.
    pub _pad: u32,
}

impl LabelBucket {
    /// Fixed byte width of one encoded bucket row.
    pub const BYTES: usize = 16;

    /// Encodes this bucket row into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        bytes[0..2].copy_from_slice(&self.label_id.to_le_bytes());
        bytes[2..4].copy_from_slice(&self.reserved.to_le_bytes());
        bytes[4..12].copy_from_slice(&self.edge_start.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.edge_len.to_le_bytes());
    }

    /// Decodes a bucket row from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("LabelBucket::read_from expects exactly 16 bytes");
        Self {
            label_id: LabelId::from_le_bytes([chunk[0], chunk[1]]),
            reserved: u16::from_le_bytes([chunk[2], chunk[3]]),
            edge_start: u64::from_le_bytes(chunk[4..12].try_into().unwrap()),
            edge_len: u32::from_le_bytes(chunk[12..16].try_into().unwrap()),
            _pad: 0,
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
        -1
    }

    fn with_log_head(self, _idx: i32) -> Self {
        self
    }
}

/// Bit 0 of [`LabeledVertex::metadata`]: vertex points directly into the edge CSR.
const DEFAULT_EDGE_LABELED_BIT: u32 = 1;
/// Bit 31 of [`LabeledVertex::metadata`]: logical vertex deletion marker.
const VERTEX_TOMBSTONE_BIT: u32 = 1 << 31;

/// Per-vertex locator for one labeled CSR orientation.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabeledVertex {
    /// Global bucket-slot or edge-slot index where this vertex's clean prefix starts.
    pub base_slot_start: u64,
    /// Number of live buckets or edges visible through clean scans.
    pub row_count: u32,
    /// Packed metadata: default-label bypass and tombstone flags.
    pub metadata: i32,
}

impl LabeledVertex {
    /// Fixed byte width of one encoded vertex row.
    pub const BYTES: usize = 16;

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

    /// Encodes this vertex row into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        bytes[0..8].copy_from_slice(&self.base_slot_start.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.row_count.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.metadata.to_le_bytes());
    }

    /// Decodes a vertex row from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("LabeledVertex::read_from expects exactly 16 bytes");
        Self {
            base_slot_start: u64::from_le_bytes(chunk[0..8].try_into().unwrap()),
            row_count: u32::from_le_bytes(chunk[8..12].try_into().unwrap()),
            metadata: i32::from_le_bytes(chunk[12..16].try_into().unwrap()),
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
            reserved: 0x5678,
            edge_start: 0x1122_3344_5566_7788,
            edge_len: 0xAABB_CCDD,
            _pad: 0,
        };
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        assert_eq!(
            bytes,
            [
                0x34, 0x12, 0x78, 0x56, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xDD, 0xCC,
                0xBB, 0xAA,
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
        }
        .with_default_edge_labeled(true)
        .with_tombstone(true);
        let mut bytes = [0u8; LabeledVertex::BYTES];
        vertex.write_to(&mut bytes);
        let decoded = LabeledVertex::read_from(&bytes);
        assert_eq!(decoded, vertex);
        assert!(decoded.is_default_edge_labeled());
        assert!(decoded.is_tombstone());
    }
}
