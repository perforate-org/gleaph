//! Stable identity for edge payloads stored outside the 8-byte payload overflow log cell.

/// Opaque key for a large edge payload tied to one payload overflow log slot.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgePayloadBlobId(u64);

impl EdgePayloadBlobId {
    /// Builds the canonical id for `(leaf_segment, entry_idx)`.
    #[inline]
    pub const fn from_log_site(leaf: u32, entry_idx: u32) -> Self {
        debug_assert!(entry_idx < 256);
        Self(((leaf as u64) << 8) | (entry_idx as u64))
    }

    #[inline]
    pub const fn leaf(self) -> u32 {
        (self.0 >> 8) as u32
    }

    #[inline]
    pub const fn entry_idx(self) -> u32 {
        (self.0 & 0xFF) as u32
    }

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}
