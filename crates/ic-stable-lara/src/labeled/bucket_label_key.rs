//! Wire key for labeled CSR bucket rows and homogeneous bypass.
//!
//! Lower 15 bits are the **label index** (caller-defined; often a catalog id). Bit 15
//! ([`BUCKET_LABEL_DIRECTED_BIT`]) selects the **directed** bucket when set.
//! [`Ord`] follows raw `u16`, so every undirected key (`0x0000..=0x7FFF`) sorts before
//! every directed key (`0x8000..=0xFFFF`) at the same label-index rank.

use std::fmt;

/// MSB on [`BucketLabelKey`]: directed bucket / default directed bypass wire value.
pub const BUCKET_LABEL_DIRECTED_BIT: u16 = 0x8000;

/// Mask for the low 15 label-index bits.
pub const BUCKET_LABEL_INDEX_MASK: u16 = 0x7FFF;

/// Directed vs undirected interpretation when packing a label index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BucketDirectedness {
    /// MSB clear.
    Undirected,
    /// MSB set ([`BUCKET_LABEL_DIRECTED_BIT`]).
    Directed,
}

/// Packed bucket / bypass label wire value (`u16`).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BucketLabelKey(u16);

impl Default for BucketLabelKey {
    /// Directed unlabeled bucket key (`0x8000`): smallest directed wire in sort order.
    #[inline]
    fn default() -> Self {
        Self::UNLABELED_DIRECTED
    }
}

#[allow(missing_docs)]
impl BucketLabelKey {
    /// Homogeneous bypass / unlabeled directed bucket key.
    pub const UNLABELED_DIRECTED: Self = Self(BUCKET_LABEL_DIRECTED_BIT);
    /// Homogeneous bypass / unlabeled undirected bucket key.
    pub const UNLABELED_UNDIRECTED: Self = Self(0);

    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    #[inline]
    pub const fn directed_from_index(label_index: u16) -> Self {
        Self((label_index & BUCKET_LABEL_INDEX_MASK) | BUCKET_LABEL_DIRECTED_BIT)
    }

    #[inline]
    pub const fn undirected_from_index(label_index: u16) -> Self {
        Self(label_index & BUCKET_LABEL_INDEX_MASK)
    }

    #[inline]
    pub const fn new_from_index(label_index: u16, directedness: BucketDirectedness) -> Self {
        match directedness {
            BucketDirectedness::Undirected => Self::undirected_from_index(label_index),
            BucketDirectedness::Directed => Self::directed_from_index(label_index),
        }
    }

    /// Low 15 bits: label index (`0` = unlabeled in typical bypass encodings).
    #[inline]
    pub const fn label_index(self) -> u16 {
        self.0 & BUCKET_LABEL_INDEX_MASK
    }

    #[inline]
    pub const fn is_undirected(self) -> bool {
        self.0 & BUCKET_LABEL_DIRECTED_BIT == 0
    }

    #[inline]
    pub const fn is_directed(self) -> bool {
        !self.is_undirected()
    }

    #[inline]
    pub const fn directedness(self) -> BucketDirectedness {
        if self.is_undirected() {
            BucketDirectedness::Undirected
        } else {
            BucketDirectedness::Directed
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

impl fmt::Display for BucketLabelKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msb_encodes_directed() {
        let d = BucketLabelKey::directed_from_index(5);
        let u = BucketLabelKey::undirected_from_index(5);
        assert!(d.is_directed());
        assert!(u.is_undirected());
        assert_eq!(d.raw(), 0x8005);
        assert_eq!(u.raw(), 5);
    }

    #[test]
    fn default_is_unlabeled_directed_wire() {
        assert_eq!(BucketLabelKey::default().raw(), BUCKET_LABEL_DIRECTED_BIT);
        assert_eq!(
            BucketLabelKey::default(),
            BucketLabelKey::UNLABELED_DIRECTED
        );
    }

    #[test]
    fn ord_groups_undirected_before_directed() {
        let hi = BucketLabelKey::directed_from_index(1);
        let lo = BucketLabelKey::undirected_from_index(BUCKET_LABEL_INDEX_MASK);
        assert!(lo < hi);
    }
}
