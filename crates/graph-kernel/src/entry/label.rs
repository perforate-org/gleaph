use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;
use std::fmt;

/// Bit 15 of [`EdgeLabelId`]: undirected bucket / storage key.
pub const EDGE_LABEL_UNDIRECTED_BIT: u16 = 0x8000;

/// Maximum catalog edge label id (MSB clear).
pub const EDGE_LABEL_CATALOG_MAX: u16 = 0x7FFF;

/// Vertex label identifier (`0` reserved; catalog allocates `1..=0xFFFF`).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct VertexLabelId(u16);

/// Edge label identifier for Lara buckets (`bit15` = undirected).
///
/// Catalog names allocate in the directed half (`0x0001..=0x7FFF`, MSB clear).
/// Storage keys set bit15 for undirected edges (`0x8000..=0xFFFF`).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EdgeLabelId(u16);

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
    pub const UNLABELED_DIRECTED: Self = Self(0);
    pub const UNLABELED_UNDIRECTED: Self = Self(EDGE_LABEL_UNDIRECTED_BIT);

    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    #[inline]
    pub const fn is_undirected(self) -> bool {
        self.0 & EDGE_LABEL_UNDIRECTED_BIT != 0
    }

    /// Lower 15 bits: catalog id (`0` = unlabeled).
    #[inline]
    pub const fn catalog_id(self) -> u16 {
        self.0 & EDGE_LABEL_CATALOG_MAX
    }

    /// Builds a storage/bucket key from a catalog id (MSB clear) and direction.
    #[inline]
    pub const fn from_catalog(catalog: Self, undirected: bool) -> Self {
        let id = catalog.0 & EDGE_LABEL_CATALOG_MAX;
        if undirected {
            Self(id | EDGE_LABEL_UNDIRECTED_BIT)
        } else {
            Self(id)
        }
    }

    /// `true` when this id may be stored in the edge label catalog (MSB clear, non-zero).
    #[inline]
    pub const fn is_catalog_allocatable(self) -> bool {
        !self.is_undirected() && self.0 != 0 && self.0 <= EDGE_LABEL_CATALOG_MAX
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
    fn edge_label_msb_encodes_undirected() {
        let catalog = EdgeLabelId::from_raw(5);
        assert!(!EdgeLabelId::from_catalog(catalog, false).is_undirected());
        assert!(EdgeLabelId::from_catalog(catalog, true).is_undirected());
        assert_eq!(EdgeLabelId::from_catalog(catalog, true).raw(), 0x8005);
    }

    #[test]
    fn unlabeled_storage_ids() {
        assert_eq!(EdgeLabelId::UNLABELED_DIRECTED.raw(), 0);
        assert_eq!(EdgeLabelId::UNLABELED_UNDIRECTED.raw(), 0x8000);
    }
}
