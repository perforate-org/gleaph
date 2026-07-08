//! Stable identity for edge inline values stored outside the 8-byte payload overflow log cell.

/// Opaque key for a large edge inline value tied to one payload overflow log slot.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeInlineValueBlobId(u64);

impl EdgeInlineValueBlobId {
    /// Builds the canonical id for `(leaf_segment, entry_idx)`.
    #[inline]
    pub const fn from_log_site(leaf: u32, entry_idx: u32) -> Self {
        debug_assert!(entry_idx < 256);
        Self(((leaf as u64) << 8) | (entry_idx as u64))
    }

    /// Returns the owning edge segment.
    #[inline]
    pub const fn leaf(self) -> u32 {
        (self.0 >> 8) as u32
    }

    /// Returns the overflow-log entry index within the segment.
    #[inline]
    pub const fn entry_idx(self) -> u32 {
        (self.0 & 0xFF) as u32
    }

    /// Returns the packed raw blob id.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Creates a blob id from its packed representation.
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}
