//! An edge's position within its bucket.

/// The position of an edge half within its `(owner, label, direction)` bucket: stable until the
/// bucket is compacted, which reports every move (ADR 0136, Decision 4). Orientation is carried
/// by the surrounding occurrence.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EdgeSlotIndex(u32);

impl EdgeSlotIndex {
    /// The position `raw`.
    #[inline]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The position `raw`.
    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The position as an index.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Its little-endian bytes, as Graph and federation keys store it.
    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    /// The position from its little-endian bytes.
    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }
}

impl From<u32> for EdgeSlotIndex {
    #[inline]
    fn from(raw: u32) -> Self {
        Self(raw)
    }
}

impl From<EdgeSlotIndex> for u32 {
    #[inline]
    fn from(position: EdgeSlotIndex) -> Self {
        position.0
    }
}

impl From<EdgeSlotIndex> for u64 {
    #[inline]
    fn from(position: EdgeSlotIndex) -> Self {
        u64::from(position.0)
    }
}
