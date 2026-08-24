//! Shard-local handles for cross-shard CSR edge endpoints.
//!
//! # Payload ceiling invariant
//!
//! Remote handles occupy the same 30-bit payload domain as local vertex ids in
//! an edge row ([`super::vertex_ref::MAX_PAYLOAD_VERTEX_ID`]). [`RemoteVertexId::from_raw`]
//! rejects higher values fail-closed; it is also the decode path for persisted
//! handle keys, so corrupt high-bit bytes trap instead of silently masking.

use ic_stable_lara::VertexId;
use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;

/// Dense shard-local handle stored in remote [`super::vertex_ref::VertexRef`] slots.
///
/// Many edges may share one `RemoteVertexId` for the same global target vertex.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteVertexId(u32);

const REMOTE_VERTEX_ID_MASK: u32 = (1 << 30) - 1;

/// Highest shard-local remote handle representable in the 30-bit edge-row
/// payload domain (`2^30 - 1`). Blessed per-shard capacity bound shared with
/// local ids; see [`super::vertex_ref::MAX_PAYLOAD_VERTEX_ID`] and
/// `design/storage/lara.md`.
pub const MAX_REMOTE_VERTEX_ID: u32 = REMOTE_VERTEX_ID_MASK;

impl RemoteVertexId {
    /// Constructs a remote handle from a raw shard-local value.
    ///
    /// Fail-closed at the payload-ceiling boundary: panics when `raw` exceeds
    /// [`MAX_REMOTE_VERTEX_ID`] instead of masking. This is also the decode
    /// path for persisted handle keys, so a corrupt high-bit key traps rather
    /// than aliasing to a different handle.
    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        if raw > REMOTE_VERTEX_ID_MASK {
            panic!("remote vertex id exceeds the 30-bit edge-row payload ceiling");
        }
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self::from_raw(u32::from_le_bytes(bytes))
    }
}

impl Storable for RemoteVertexId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 4,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_le_bytes()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_le_bytes())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0; 4];
        raw.copy_from_slice(bytes.as_ref());
        Self::from_le_bytes(raw)
    }
}

/// Resolved edge endpoint on a graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeTarget {
    Local(VertexId),
    Remote(RemoteVertexId),
}

impl EdgeTarget {
    #[inline]
    pub const fn is_remote(self) -> bool {
        matches!(self, Self::Remote(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_payload_ceiling_accepts_highest_remote_handle() {
        let handle = RemoteVertexId::from_raw(MAX_REMOTE_VERTEX_ID);
        assert_eq!(handle.raw(), MAX_REMOTE_VERTEX_ID);
        assert_eq!(RemoteVertexId::from_le_bytes(handle.to_le_bytes()), handle);
        // Storable wire round trip preserves the boundary payload exactly.
        assert_eq!(RemoteVertexId::from_bytes(handle.to_bytes()), handle);
    }

    #[test]
    #[should_panic(expected = "exceeds the 30-bit edge-row payload ceiling")]
    fn edge_payload_ceiling_rejects_first_handle_past_bound() {
        let _ = RemoteVertexId::from_raw(MAX_REMOTE_VERTEX_ID + 1);
    }

    #[test]
    #[should_panic(expected = "exceeds the 30-bit edge-row payload ceiling")]
    fn storable_decode_rejects_corrupt_reserved_bit_handle() {
        // A persisted handle key with the reserved bit set is corruption:
        // fail closed instead of masking it to a different handle.
        let _ = RemoteVertexId::from_bytes(Cow::Owned(vec![0x00, 0x00, 0x00, 0x40]));
    }
}
