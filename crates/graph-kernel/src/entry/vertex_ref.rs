//! Compact vertex reference stored inside labeled edge records.

use super::remote_ref::{EdgeTarget, RemoteRefId};
use ic_stable_lara::VertexId;

const TOMBSTONE_BIT: u32 = 1 << 31;
const REMOTE_BIT: u32 = 1 << 30;
const PAYLOAD_MASK: u32 = (1 << 30) - 1;

/// Adjacent vertex reference with an optional remote-partition flag.
///
/// Local targets store a [`VertexId`]. Remote targets store a shard-local [`RemoteRefId`].
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VertexRef(u32);

impl VertexRef {
    /// Constructs a local vertex reference.
    #[inline]
    pub fn local(vid: VertexId) -> Self {
        Self(u32::from(vid) & PAYLOAD_MASK)
    }

    /// Constructs a remote reference to a logical vertex via a shard-local [`RemoteRefId`].
    #[inline]
    pub fn remote_ref(id: RemoteRefId) -> Self {
        Self(id.raw() | REMOTE_BIT)
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

    /// Returns the local vertex id bits when this is a local target.
    #[inline]
    pub fn local_id(self) -> VertexId {
        debug_assert!(!self.is_remote(), "local_id on remote VertexRef");
        VertexId::from(self.0 & PAYLOAD_MASK)
    }

    /// Returns the shard-local remote ref id when this is a remote target.
    #[inline]
    pub fn remote_ref_id(self) -> RemoteRefId {
        debug_assert!(self.is_remote(), "remote_ref_id on local VertexRef");
        RemoteRefId::from_raw(self.0 & PAYLOAD_MASK)
    }

    /// Decodes this reference as an [`EdgeTarget`].
    #[inline]
    pub fn edge_target(self) -> Option<EdgeTarget> {
        if self.is_tombstone() {
            return None;
        }
        if self.is_remote() {
            Some(EdgeTarget::Remote(self.remote_ref_id()))
        } else {
            Some(EdgeTarget::Local(self.local_id()))
        }
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
    fn remote_ref_preserves_id_bits() {
        let id = RemoteRefId::from_raw(42);
        let remote = VertexRef::remote_ref(id);
        assert!(remote.is_remote());
        assert_eq!(remote.remote_ref_id(), id);
    }

    #[test]
    fn local_and_remote_targets_roundtrip_through_edge_target() {
        let local = VertexRef::local(VertexId::from(7));
        assert_eq!(local.edge_target(), Some(EdgeTarget::Local(VertexId::from(7))));

        let remote = VertexRef::remote_ref(RemoteRefId::from_raw(99));
        assert_eq!(
            remote.edge_target(),
            Some(EdgeTarget::Remote(RemoteRefId::from_raw(99)))
        );
    }

    #[test]
    fn tombstone_has_no_edge_target() {
        assert_eq!(VertexRef::tombstone().edge_target(), None);
    }
}
