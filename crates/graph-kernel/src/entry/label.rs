use candid::CandidType;
use ic_stable_lara::labeled::{BUCKET_LABEL_DIRECTED_BIT, BucketDirectedness, BucketLabelKey};
use ic_stable_structures::{Storable, storable::Bound};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Same value as [`BUCKET_LABEL_DIRECTED_BIT`]: directed bit on LARA bucket wire keys.
pub const EDGE_LABEL_DIRECTED_BIT: u16 = BUCKET_LABEL_DIRECTED_BIT;

/// Maximum catalog edge label id (MSB clear).
pub const EDGE_LABEL_CATALOG_MAX: u16 = 0x7FFF;

/// LARA labeled CSR bucket wire key (re-exported from `ic-stable-lara`).
pub type TaggedEdgeLabelId = BucketLabelKey;

/// Vertex label identifier (`0` reserved; catalog allocates `1..=0xFFFF`).
#[repr(transparent)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    CandidType,
    Serialize,
    Deserialize,
)]
pub struct VertexLabelId(u16);

/// Catalog edge label id: lower 15 bits only (MSB must stay clear).
///
/// Stable name maps and weight profiles use this type. Storage / LARA bucket keys use
/// [`TaggedEdgeLabelId`], which adds the directed MSB.
#[repr(transparent)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    CandidType,
    Serialize,
    Deserialize,
)]
pub struct EdgeLabelId(u16);

/// Directed vs undirected interpretation for [`EdgeLabelId::pack`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeDirectedness {
    Directed,
    Undirected,
}

impl VertexLabelId {
    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    #[inline]
    pub const fn is_reserved(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 2] {
        self.0.to_le_bytes()
    }

    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 2]) -> Self {
        Self(u16::from_le_bytes(bytes))
    }
}

impl EdgeLabelId {
    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Packs this catalog id with a directedness bit into a LARA / bucket wire key.
    #[inline]
    pub const fn pack(self, directedness: EdgeDirectedness) -> TaggedEdgeLabelId {
        TaggedEdgeLabelId::new_from_index(
            self.raw(),
            match directedness {
                EdgeDirectedness::Directed => BucketDirectedness::Directed,
                EdgeDirectedness::Undirected => BucketDirectedness::Undirected,
            },
        )
    }

    /// `true` when this id may be stored in the edge label catalog (MSB clear, non-zero).
    #[inline]
    pub const fn is_catalog_allocatable(self) -> bool {
        self.0 != 0 && (self.0 & EDGE_LABEL_DIRECTED_BIT) == 0 && self.0 <= EDGE_LABEL_CATALOG_MAX
    }

    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 2] {
        self.0.to_le_bytes()
    }

    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 2]) -> Self {
        Self(u16::from_le_bytes(bytes))
    }
}

impl Storable for VertexLabelId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 2,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_le_bytes()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_le_bytes())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut out = [0u8; 2];
        out.copy_from_slice(bytes.as_ref());
        Self::from_le_bytes(out)
    }
}

impl Storable for EdgeLabelId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 2,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_le_bytes()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_le_bytes())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut out = [0u8; 2];
        out.copy_from_slice(bytes.as_ref());
        Self::from_le_bytes(out)
    }
}

impl fmt::Display for VertexLabelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for EdgeLabelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_label_msb_encodes_directed() {
        let catalog = EdgeLabelId::from_raw(5);
        let d = catalog.pack(EdgeDirectedness::Directed);
        let u = catalog.pack(EdgeDirectedness::Undirected);
        assert!(d.is_directed());
        assert!(u.is_undirected());
        assert_eq!(d.raw(), 0x8005);
        assert_eq!(u.raw(), 5);
    }

    #[test]
    fn unlabeled_storage_ids() {
        assert_eq!(TaggedEdgeLabelId::UNLABELED_DIRECTED.raw(), 0x8000);
        assert_eq!(TaggedEdgeLabelId::UNLABELED_UNDIRECTED.raw(), 0);
    }

    #[test]
    fn ord_groups_undirected_before_directed() {
        let hi = EdgeLabelId::from_raw(1).pack(EdgeDirectedness::Directed);
        let lo = EdgeLabelId::from_raw(EDGE_LABEL_CATALOG_MAX).pack(EdgeDirectedness::Undirected);
        assert!(lo < hi);
    }
}
