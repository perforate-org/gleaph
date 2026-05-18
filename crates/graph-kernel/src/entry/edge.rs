//! Fixed-width edge records for labeled CSR storage (6 bytes).
//!
//! Layout (little-endian on wire):
//!
//! ```text
//! 5         4 3        0
//! +-----------+----------+
//! |inline_val |  target  |
//! |  2 bytes  | 4 bytes  |
//! +-----------+----------+
//! ```
//!
//! `target` is a [`VertexRef`] (local id plus optional remote bit). Relationship
//! type and directionality are carried by the labeled bucket layer, not this row.

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

/// Fixed-size adjacency entry stored in one labeled CSR slab slot.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub target: VertexRef,
    pub edge_slot_index: EdgeSlotIndex,
    pub label_id: u16,
    pub inline_value: u16,
}

impl PartialEq for Edge {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target && self.inline_value == other.inline_value
    }
}

impl Eq for Edge {}

impl Hash for Edge {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.target.hash(state);
        self.inline_value.hash(state);
    }
}

impl CsrEdge for Edge {
    const BYTES: usize = 6;

    #[inline]
    fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("CsrEdge::read_from expects exactly 6 bytes");

        Edge {
            target: VertexRef::from_le_bytes(chunk[0..4].try_into().unwrap()),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            inline_value: u16::from_le_bytes(chunk[4..6].try_into().unwrap()),
        }
    }

    #[inline]
    fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(
            bytes.len(),
            Self::BYTES,
            "CsrEdge::write_to expects exactly 6 bytes"
        );
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.inline_value.to_le_bytes());
    }

    #[inline]
    fn neighbor_vid(&self) -> VertexId {
        self.target.local_id()
    }

    #[inline]
    fn with_neighbor_vid(self, vid: VertexId) -> Self {
        let target = if self.target.is_remote() {
            VertexRef::remote(vid)
        } else {
            VertexRef::local(vid)
        };
        Self {
            target,
            edge_slot_index: self.edge_slot_index,
            label_id: self.label_id,
            inline_value: self.inline_value,
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

    fn is_deleted_slot(&self) -> bool {
        self.target.is_tombstone()
    }
}

impl CsrEdgeTombstone for Edge {
    fn tombstone_edge() -> Self {
        Self {
            target: VertexRef::tombstone(),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            inline_value: 0,
        }
    }

    fn is_tombstone_edge(&self) -> bool {
        self.target.is_tombstone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_width_matches_documented_storage_layout() {
        assert_eq!(Edge::BYTES, 6);
        assert!(
            core::mem::size_of::<Edge>() >= Edge::BYTES,
            "Rust layout may include tail padding; wire width is {}",
            Edge::BYTES
        );
    }

    #[test]
    fn write_to_encodes_fields_as_little_endian() {
        let edge = Edge {
            target: VertexRef::remote(VertexId::from(0x1234_5678)),
            edge_slot_index: EdgeSlotIndex::from_raw(0xA1B2_C3D4),
            label_id: 0,
            inline_value: 0x9A8B,
        };
        let mut bytes = [0u8; Edge::BYTES];
        edge.write_to(&mut bytes);
        assert_eq!(bytes, [0x78, 0x56, 0x34, 0x52, 0x8B, 0x9A,]);
    }

    #[test]
    fn read_from_round_trips_all_fields() {
        let bytes = [0x78, 0x56, 0x34, 0x12, 0x8B, 0x9A];
        let edge = Edge::read_from(&bytes);
        assert_eq!(edge.target, VertexRef::local(VertexId::from(0x1234_5678)));
        assert_eq!(edge.edge_slot_index, EdgeSlotIndex::from_raw(0));
        assert_eq!(edge.inline_value, 0x9A8B);

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
}
