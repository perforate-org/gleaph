//! Fixed-width edge records for labeled CSR storage (4 bytes).
//!
//! Layout (little-endian on wire):
//!
//! ```text
//! 3         0
//! +----------+
//! |  target  |
//! | 4 bytes  |
//! +----------+
//! ```
//!
//! `target` is a [`VertexRef`] (local id plus optional remote bit). Relationship
//! type, directionality, and per-edge values are carried by the labeled bucket layer
//! and [`EdgeValueStore`], not this row.

use super::edge_value_payload::EdgeValuePayload;
use super::remote_ref::EdgeTarget;
use super::vertex_ref::VertexRef;
use ic_stable_lara::{
    VertexId,
    traits::{CsrEdge, CsrEdgeTombstone},
};
use std::hash::{Hash, Hasher};

mod id;
mod meta;

pub use id::EdgeSlotIndex;
pub use meta::EdgeMeta;

/// Maximum edge-value byte width supported by labeled storage profiles.
pub const MAX_EDGE_VALUE_BYTES: usize = u16::MAX as usize;

/// Fixed-size adjacency entry stored in one labeled CSR slab slot.
#[derive(Clone, Debug)]
pub struct Edge {
    pub target: VertexRef,
    pub edge_slot_index: EdgeSlotIndex,
    pub label_id: u16,
    /// In-memory edge value (not persisted on the 4-byte wire row).
    pub value: EdgeValuePayload,
}

impl PartialEq for Edge {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
            && self.edge_slot_index == other.edge_slot_index
            && self.label_id == other.label_id
            && self.value == other.value
    }
}

impl Eq for Edge {}

impl Hash for Edge {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.target.hash(state);
        self.edge_slot_index.hash(state);
        self.label_id.hash(state);
        self.value.hash(state);
    }
}

impl Edge {
    /// Returns the active value byte slice.
    #[inline]
    pub fn value_bytes(&self) -> &[u8] {
        self.value.as_slice()
    }

    /// Sets the in-memory value bytes (length must be <= [`MAX_EDGE_VALUE_BYTES`] when enforced by callers).
    #[inline]
    pub fn with_value_bytes(&self, bytes: &[u8]) -> Self {
        Self {
            value: EdgeValuePayload::from_slice(bytes),
            ..self.clone()
        }
    }

    /// Legacy u16 weight view when the active value is exactly two bytes (little-endian).
    #[inline]
    pub fn inline_value_u16(&self) -> u16 {
        self.value.inline_u16()
    }

    /// Resolves the stored neighbor as a local or remote [`EdgeTarget`].
    #[inline]
    pub fn edge_target(&self) -> Option<EdgeTarget> {
        self.target.edge_target()
    }
}

impl CsrEdge for Edge {
    const BYTES: usize = 4;

    #[inline]
    fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("CsrEdge::read_from expects exactly 4 bytes");

        Edge {
            target: VertexRef::from_le_bytes(chunk[0..4].try_into().unwrap()),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            value: EdgeValuePayload::EMPTY,
        }
    }

    #[inline]
    fn write_to(&self, bytes: &mut [u8]) {
        debug_assert_eq!(
            bytes.len(),
            Self::BYTES,
            "CsrEdge::write_to expects exactly 4 bytes"
        );
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
    }

    #[inline]
    fn neighbor_vid(&self) -> VertexId {
        self.target.local_id()
    }

    #[inline]
    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        let target = match self.target.edge_target() {
            Some(EdgeTarget::Remote(remote_ref)) => VertexRef::remote_ref(remote_ref),
            Some(EdgeTarget::Local(_)) | None => VertexRef::local(vid),
        };
        Self {
            target,
            edge_slot_index: self.edge_slot_index,
            label_id: self.label_id,
            value: self.value.clone(),
        }
    }

    fn with_slot_index(self, slot_index: u32) -> Self {
        Self {
            edge_slot_index: EdgeSlotIndex::from_raw(slot_index),
            ..self
        }
    }

    fn with_label_id(self, label_id: u16) -> Self {
        Self { label_id, ..self }
    }

    fn edge_label_id_raw(&self) -> Option<u16> {
        Some(self.label_id)
    }

    fn is_deleted_slot(&self) -> bool {
        self.target.is_tombstone()
    }

    fn edge_value_byte_width(&self) -> u16 {
        u16::try_from(self.value.len()).unwrap_or(u16::MAX)
    }

    fn edge_value_bytes(&self) -> &[u8] {
        self.value_bytes()
    }

    fn with_stored_value_bytes(self, _width: u16, bytes: &[u8]) -> Self {
        self.with_value_bytes(bytes)
    }

    fn edge_slot_index_raw(&self) -> u32 {
        self.edge_slot_index.raw()
    }
}

impl CsrEdgeTombstone for Edge {
    fn tombstone_edge() -> Self {
        Self {
            target: VertexRef::tombstone(),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            value: EdgeValuePayload::EMPTY,
        }
    }

    fn is_tombstone_edge(&self) -> bool {
        self.target.is_tombstone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::RemoteRefId;
    use std::collections::hash_map::DefaultHasher;

    fn test_edge(slot: u32, label_id: u16) -> Edge {
        Edge {
            target: VertexRef::local(VertexId::from(1)),
            edge_slot_index: EdgeSlotIndex::from_raw(slot),
            label_id,
            value: EdgeValuePayload::from_inline_u16(7),
        }
    }

    fn hash(edge: &Edge) -> u64 {
        let mut hasher = DefaultHasher::new();
        edge.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn edge_width_matches_documented_storage_layout() {
        assert_eq!(Edge::BYTES, 4);
        assert!(
            core::mem::size_of::<Edge>() >= Edge::BYTES,
            "Rust layout may include tail padding; wire width is {}",
            Edge::BYTES
        );
    }

    #[test]
    fn edge_identity_includes_label_id_and_slot_index() {
        let base = test_edge(1, 2);
        let different_slot = test_edge(2, 2);
        let different_label = test_edge(1, 3);

        assert_ne!(base, different_slot);
        assert_ne!(base, different_label);
        assert_ne!(hash(&base), hash(&different_slot));
        assert_ne!(hash(&base), hash(&different_label));
    }

    #[test]
    fn write_to_encodes_target_only() {
        let edge = Edge {
            target: VertexRef::remote_ref(RemoteRefId::from_raw(0x1234_5678)),
            edge_slot_index: EdgeSlotIndex::from_raw(0xA1B2_C3D4),
            label_id: 0,
            value: EdgeValuePayload::from_slice(&0x9A8Bu16.to_le_bytes()),
        };
        let mut bytes = [0u8; Edge::BYTES];
        edge.write_to(&mut bytes);
        assert_eq!(bytes, [0x78, 0x56, 0x34, 0x52]);
    }

    #[test]
    fn read_from_round_trips_target() {
        let bytes = [0x78, 0x56, 0x34, 0x12];
        let edge = Edge::read_from(&bytes);
        assert_eq!(edge.target, VertexRef::local(VertexId::from(0x1234_5678)));
        assert!(edge.value.is_empty());

        let mut round_trip = [0u8; Edge::BYTES];
        edge.write_to(&mut round_trip);
        assert_eq!(round_trip, bytes);
    }

    #[test]
    fn tombstone_edge_uses_tombstone_bit() {
        let edge = Edge::tombstone_edge();
        assert!(edge.is_tombstone_edge());
        assert!(edge.target.is_tombstone());
    }

    #[test]
    fn with_value_bytes_round_trips() {
        let edge = Edge::tombstone_edge().with_value_bytes(&[1, 2, 3, 4]);
        assert_eq!(edge.value_bytes(), &[1, 2, 3, 4]);
        assert_eq!(edge.inline_value_u16(), 0);
        let u16_edge = Edge::tombstone_edge().with_value_bytes(&0x9A8Bu16.to_le_bytes());
        assert_eq!(u16_edge.inline_value_u16(), 0x9A8B);
    }
}
