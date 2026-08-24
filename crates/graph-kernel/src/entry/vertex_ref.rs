//! Compact vertex reference stored inside labeled edge records.
//!
//! # Payload ceiling invariant
//!
//! An edge row is 4 bytes: bits 0–29 carry the vertex payload, bit 30 is the
//! remote flag, and bit 31 is the tombstone flag. The payload domain is the
//! blessed per-shard capacity bound ([`MAX_PAYLOAD_VERTEX_ID`]); minting
//! constructors reject higher ids fail-closed instead of masking them into
//! aliased neighbors. See `design/storage/lara.md` (shard capacity bounds).

use super::remote_vertex_id::{EdgeTarget, RemoteVertexId};
use ic_stable_lara::VertexId;

const TOMBSTONE_BIT: u32 = 1 << 31;
const REMOTE_BIT: u32 = 1 << 30;
const PAYLOAD_MASK: u32 = (1 << 30) - 1;

/// Highest id representable in the 30-bit edge-row payload domain
/// (`2^30 - 1`, ~1.07e9 local vertex references per shard orientation).
///
/// This is the blessed per-shard capacity bound, not an incidental side effect
/// of bit packing: [`VertexRef::local`] and [`RemoteVertexId::from_raw`] reject
/// larger ids fail-closed rather than silently masking high bits. Widening past
/// 30 bits cannot stay inside a 4-byte edge row — it means 8-byte rows and a new
/// layout ADR. Widen trigger: a product requirement for more than 2^30 local
/// vertex references in one shard orientation. See `design/storage/lara.md`.
pub const MAX_PAYLOAD_VERTEX_ID: u32 = PAYLOAD_MASK;

/// Adjacent vertex reference with an optional remote-partition flag.
///
/// Local targets store a [`VertexId`]. Remote targets store a shard-local [`RemoteVertexId`].
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VertexRef(u32);

impl VertexRef {
    /// Constructs a local vertex reference.
    ///
    /// Fail-closed at the payload-ceiling boundary: panics when `vid` exceeds
    /// [`MAX_PAYLOAD_VERTEX_ID`] instead of masking the high bits (which would
    /// silently alias the neighbor to a different vertex). Called before any
    /// canonical mutation on every edge-insert path, so the trap leaves stored
    /// state untouched.
    #[inline]
    pub fn local(vid: VertexId) -> Self {
        let raw = u32::from(vid);
        assert!(
            raw <= PAYLOAD_MASK,
            "vertex id {raw} exceeds the 30-bit edge-row payload ceiling \
             ({MAX_PAYLOAD_VERTEX_ID}); shard capacity bound violated"
        );
        Self(raw)
    }

    /// Constructs a remote CSR endpoint via a shard-local [`RemoteVertexId`].
    #[inline]
    pub fn remote_vertex(id: RemoteVertexId) -> Self {
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

    /// Returns the shard-local remote vertex id when this is a remote target.
    #[inline]
    pub fn remote_vertex_id(self) -> RemoteVertexId {
        debug_assert!(self.is_remote(), "remote_vertex_id on local VertexRef");
        RemoteVertexId::from_raw(self.0 & PAYLOAD_MASK)
    }

    /// Decodes this reference as an [`EdgeTarget`].
    #[inline]
    pub fn edge_target(self) -> Option<EdgeTarget> {
        if self.is_tombstone() {
            return None;
        }
        if self.is_remote() {
            Some(EdgeTarget::Remote(self.remote_vertex_id()))
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
    ///
    /// Decode-side counterpart of [`Self::raw`]: accepts any stored `u32`
    /// (including flag bits) so persisted rows round-trip exactly. Minting
    /// ids into the payload domain goes through [`Self::local`] /
    /// [`Self::remote_vertex`], which enforce the ceiling.
    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the little-endian wire encoding.
    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    /// Decodes a little-endian wire value (decode-side; accepts any stored bytes).
    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_vertex_preserves_id_bits() {
        let id = RemoteVertexId::from_raw(42);
        let remote = VertexRef::remote_vertex(id);
        assert!(remote.is_remote());
        assert_eq!(remote.remote_vertex_id(), id);
    }

    #[test]
    fn local_and_remote_targets_roundtrip_through_edge_target() {
        let local = VertexRef::local(VertexId::from(7));
        assert_eq!(
            local.edge_target(),
            Some(EdgeTarget::Local(VertexId::from(7)))
        );

        let remote = VertexRef::remote_vertex(RemoteVertexId::from_raw(99));
        assert_eq!(
            remote.edge_target(),
            Some(EdgeTarget::Remote(RemoteVertexId::from_raw(99)))
        );
    }

    #[test]
    fn tombstone_has_no_edge_target() {
        assert_eq!(VertexRef::tombstone().edge_target(), None);
    }

    #[test]
    fn edge_payload_ceiling_accepts_highest_local_id_without_masking() {
        let max = VertexId::from(MAX_PAYLOAD_VERTEX_ID);
        let reference = VertexRef::local(max);
        assert_eq!(reference.raw(), MAX_PAYLOAD_VERTEX_ID);
        assert!(!reference.is_remote());
        assert!(!reference.is_tombstone());
        assert_eq!(reference.local_id(), max);
        assert_eq!(reference.edge_target(), Some(EdgeTarget::Local(max)));
    }

    #[test]
    #[should_panic(expected = "exceeds the 30-bit edge-row payload ceiling")]
    fn edge_payload_ceiling_rejects_first_id_past_bound() {
        let _ = VertexRef::local(VertexId::from(MAX_PAYLOAD_VERTEX_ID + 1));
    }

    #[test]
    #[should_panic(expected = "exceeds the 30-bit edge-row payload ceiling")]
    fn edge_payload_ceiling_rejects_tombstone_sentinel_as_neighbor() {
        // u32::MAX is the reserved EDGE_TOMBSTONE_SENTINEL; it must never be
        // minted into a payload slot (it would alias to 2^30 - 1 under masking).
        let _ = VertexRef::local(VertexId::EDGE_TOMBSTONE_SENTINEL);
    }

    #[test]
    fn edge_payload_round_trip_preserves_boundary_payload_and_flag_bits() {
        // Local boundary value: all 30 payload bits intact, no flags set.
        let local_max = VertexRef::local(VertexId::from(MAX_PAYLOAD_VERTEX_ID));
        let decoded_local = VertexRef::from_le_bytes(local_max.to_le_bytes());
        assert_eq!(decoded_local, local_max);
        assert_eq!(
            decoded_local.edge_target(),
            Some(EdgeTarget::Local(VertexId::from(MAX_PAYLOAD_VERTEX_ID)))
        );

        // Remote boundary value: payload bits plus REMOTE_BIT preserved.
        let handle = RemoteVertexId::from_raw(MAX_PAYLOAD_VERTEX_ID);
        let remote = VertexRef::remote_vertex(handle);
        assert_eq!(remote.raw(), REMOTE_BIT | MAX_PAYLOAD_VERTEX_ID);
        assert!(remote.is_remote());
        assert!(!remote.is_tombstone());
        let decoded_remote = VertexRef::from_le_bytes(remote.to_le_bytes());
        assert_eq!(decoded_remote.remote_vertex_id(), handle);
        assert_eq!(
            decoded_remote.edge_target(),
            Some(EdgeTarget::Remote(handle))
        );

        // Tombstone flag survives encode/decode untouched.
        let tombstone = VertexRef::tombstone();
        assert_eq!(VertexRef::from_le_bytes(tombstone.to_le_bytes()), tombstone);
        assert_eq!(tombstone.raw(), TOMBSTONE_BIT);
    }
}
