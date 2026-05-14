//! Compact vertex reference stored inside labeled edge records.

use ic_stable_lara::VertexId;

const REMOTE_BIT: u32 = 1 << 31;
const LOCAL_ID_MASK: u32 = !REMOTE_BIT;

/// Adjacent vertex reference with an optional remote-partition flag.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VertexRef(u32);

impl VertexRef {
    /// Constructs a local vertex reference.
    #[inline]
    pub fn local(vid: VertexId) -> Self {
        Self(u32::from(vid))
    }

    /// Constructs a remote vertex reference.
    #[inline]
    pub fn remote(vid: VertexId) -> Self {
        Self(u32::from(vid) | REMOTE_BIT)
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
}
