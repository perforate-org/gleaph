use candid::CandidType;
use ic_stable_structures::{Storable, storable::Bound};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// The directed bit of a [`TaggedEdgeLabelId`].
pub const EDGE_LABEL_DIRECTED_BIT: u16 = 0x8000;

/// Maximum catalog edge label id (MSB clear).
pub const EDGE_LABEL_CATALOG_MAX: u16 = 0x7FFF;

/// A catalog edge label with its directedness: bit 15 set for a directed edge, the low 15 bits the
/// catalog [`EdgeLabelId`]. It keys `EDGE_PROPERTIES`, postings, fences, wire formats and
/// `GlobalEdgeId`; the store keeps label and direction apart, and `facade::topology` translates
/// (ADR 0136, Decision 3). [`Ord`] follows the raw `u16`, so undirected keys sort first.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaggedEdgeLabelId(u16);

impl Default for TaggedEdgeLabelId {
    /// The unlabeled directed key.
    #[inline]
    fn default() -> Self {
        Self::UNLABELED_DIRECTED
    }
}

impl TaggedEdgeLabelId {
    /// Unlabeled, directed.
    pub const UNLABELED_DIRECTED: Self = Self(EDGE_LABEL_DIRECTED_BIT);
    /// Unlabeled, undirected.
    pub const UNLABELED_UNDIRECTED: Self = Self(0);

    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// The directed key of catalog label index `label_index` (low 15 bits).
    #[inline]
    pub const fn directed_from_index(label_index: u16) -> Self {
        Self((label_index & EDGE_LABEL_CATALOG_MAX) | EDGE_LABEL_DIRECTED_BIT)
    }

    /// The undirected key of catalog label index `label_index` (low 15 bits).
    #[inline]
    pub const fn undirected_from_index(label_index: u16) -> Self {
        Self(label_index & EDGE_LABEL_CATALOG_MAX)
    }

    /// The key of `label_index` with `directedness`.
    #[inline]
    pub const fn new_from_index(label_index: u16, directedness: EdgeDirectedness) -> Self {
        match directedness {
            EdgeDirectedness::Undirected => Self::undirected_from_index(label_index),
            EdgeDirectedness::Directed => Self::directed_from_index(label_index),
        }
    }

    /// The catalog label index (low 15 bits).
    #[inline]
    pub const fn label_index(self) -> u16 {
        self.0 & EDGE_LABEL_CATALOG_MAX
    }

    #[inline]
    pub const fn is_undirected(self) -> bool {
        self.0 & EDGE_LABEL_DIRECTED_BIT == 0
    }

    #[inline]
    pub const fn is_directed(self) -> bool {
        !self.is_undirected()
    }

    #[inline]
    pub const fn directedness(self) -> EdgeDirectedness {
        if self.is_undirected() {
            EdgeDirectedness::Undirected
        } else {
            EdgeDirectedness::Directed
        }
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

impl fmt::Display for TaggedEdgeLabelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

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
#[serde(transparent)]
pub struct VertexLabelId(u16);

/// Catalog edge label id: lower 15 bits only (MSB must stay clear).
///
/// Stable name maps and weight profiles use this type. Storage / LARA bucket keys use
/// [`TaggedEdgeLabelId`], which adds the directed MSB.
///
/// # Edge posting label identity rule
///
/// This module is the single owner of the catalog ↔ wire translation for edge index
/// postings. Every stored edge posting key carries a wire-tagged label ([`TaggedEdgeLabelId`]
/// space, as produced by [`EdgeLabelId::pack`] and decoded by
/// [`TaggedEdgeLabelId::label_index`]); every logical registration, catalog membership, and
/// lookup sieve request names its label in catalog space ([`EdgeLabelId`]). Boundaries where
/// the two spaces meet (graph-index fact/DML-subject acceptance, graph-index lookup sieves,
/// Graph read-path membership resolution) must convert through these types — no ad-hoc
/// `0x8000` arithmetic elsewhere in edge index paths.
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
#[serde(transparent)]
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
    /// Catalog label index (lower 15 bits; directed MSB cleared).
    #[inline]
    pub const fn label_index(self) -> u16 {
        self.0 & EDGE_LABEL_CATALOG_MAX
    }

    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw & EDGE_LABEL_CATALOG_MAX)
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Packs this catalog id with a directedness bit into a LARA / bucket wire key.
    #[inline]
    pub const fn pack(self, directedness: EdgeDirectedness) -> TaggedEdgeLabelId {
        TaggedEdgeLabelId::new_from_index(self.label_index(), directedness)
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
