//! Compact vertex reference stored inside labeled edge records.

use ic_stable_lara::VertexId;

const TOMBSTONE_BIT: u32 = 1 << 31;
const REMOTE_BIT: u32 = 1 << 30;
const LOCAL_ID_MASK: u32 = (1 << 30) - 1;

/// Adjacent vertex reference with an optional remote-partition flag.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VertexRef(u32);

impl VertexRef {
    /// Constructs a local vertex reference.
    #[inline]
    pub fn local(vid: VertexId) -> Self {
        Self(u32::from(vid) & LOCAL_ID_MASK)
    }

    /// Constructs a remote vertex reference.
    #[inline]
    pub fn remote(vid: VertexId) -> Self {
        Self((u32::from(vid) & LOCAL_ID_MASK) | REMOTE_BIT)
    }

    /// Constructs a tombstone reference. Tombstones do not identify a live neighbor.
    #[inline]
    pub const fn tombstone() -> Self {
        Self(TOMBSTONE_BIT)
    }

    /// Returns `true` when this slot has been logically deleted.
    #[inline]
    pub const fn is_tombstone(self) -> bool {
        self.0 & TOMBSTONE_BIT != 0
    }

    /// Returns `true` when the target lives outside the local partition.
    #[inline]
    pub const fn is_remote(self) -> bool {
        self.0 & REMOTE_BIT != 0
    }

    /// Returns the local vertex id bits.
    #[inline]
    pub fn local_id(self) -> VertexId {
        VertexId::from(self.0 & LOCAL_ID_MASK)
    }

    /// Returns the raw encoded value.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Decodes a raw encoded value.
    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the little-endian wire encoding.
    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    /// Decodes a little-endian wire value.
    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_flag_preserves_local_id_bits() {
        let local = VertexRef::local(VertexId::from(42));
        let remote = VertexRef::remote(VertexId::from(42));
        assert!(!local.is_remote());
        assert!(remote.is_remote());
        assert_eq!(local.local_id(), remote.local_id());
    }

    #[test]
    fn tombstone_bit_is_independent_from_remote_and_id_bits() {
        let tomb = VertexRef::tombstone();
        assert!(tomb.is_tombstone());
        assert!(!tomb.is_remote());
        assert_eq!(tomb.local_id(), VertexId::from(0));

        let remote = VertexRef::remote(VertexId::from((1 << 30) - 1));
        assert!(!remote.is_tombstone());
        assert!(remote.is_remote());
        assert_eq!(remote.local_id(), VertexId::from((1 << 30) - 1));
    }
}
