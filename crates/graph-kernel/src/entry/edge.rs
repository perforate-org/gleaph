//! Fixed-width edge records for labeled CSR storage (10 bytes).
//!
//! Layout (little-endian on wire):
//!
//! ```text
//! 9         8 7          4 3        0
//! +-----------+------------+----------+
//! |inline_val | vertex_edge|  target  |
//! |  2 bytes  |  id 4 byte | 4 bytes  |
//! +-----------+------------+----------+
//! ```
//!
//! `target` is a [`VertexRef`] (local id plus optional remote bit). Relationship
//! type and directionality are carried by the labeled bucket layer, not this row.

use super::vertex_ref::VertexRef;
use ic_stable_lara::{
    VertexId,
    traits::{CsrEdge, CsrEdgeSlabVacancy},
};

mod id;
mod meta;

pub use id::VertexEdgeId;
pub use meta::EdgeMeta;

/// Fixed-size adjacency entry stored in one labeled CSR slab slot.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Edge {
    pub target: VertexRef,
    pub vertex_edge_id: VertexEdgeId,
    pub inline_value: u16,
}

impl CsrEdge for Edge {
    const BYTES: usize = 10;

    #[inline]
    fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("CsrEdge::read_from expects exactly 10 bytes");

        Edge {
            target: VertexRef::from_le_bytes(chunk[0..4].try_into().unwrap()),
            vertex_edge_id: VertexEdgeId::from_le_bytes(chunk[4..8].try_into().unwrap()),
            inline_value: u16::from_le_bytes(chunk[8..10].try_into().unwrap()),
        }
    }

    #[inline]
    fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(
            bytes.len(),
            Self::BYTES,
            "CsrEdge::write_to expects exactly 10 bytes"
        );
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.vertex_edge_id.to_le_bytes());
        bytes[8..10].copy_from_slice(&self.inline_value.to_le_bytes());
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
            vertex_edge_id: self.vertex_edge_id,
            inline_value: self.inline_value,
        }
    }
}

impl CsrEdgeSlabVacancy for Edge {
    fn slab_vacant_edge() -> Self {
        Self {
            target: VertexRef::local(VertexId::SLAB_VACANT),
            vertex_edge_id: VertexEdgeId::from_raw(0),
            inline_value: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_width_matches_documented_storage_layout() {
        assert_eq!(Edge::BYTES, 10);
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
            vertex_edge_id: VertexEdgeId::from_raw(0xA1B2_C3D4),
            inline_value: 0x9A8B,
        };
        let mut bytes = [0u8; Edge::BYTES];
        edge.write_to(&mut bytes);
        assert_eq!(
            bytes,
            [0x78, 0x56, 0x34, 0x92, 0xD4, 0xC3, 0xB2, 0xA1, 0x8B, 0x9A,]
        );
    }

    #[test]
    fn read_from_round_trips_all_fields() {
        let bytes = [0x78, 0x56, 0x34, 0x12, 0xD4, 0xC3, 0xB2, 0xA1, 0x8B, 0x9A];
        let edge = Edge::read_from(&bytes);
        assert_eq!(edge.target, VertexRef::local(VertexId::from(0x1234_5678)));
        assert_eq!(edge.vertex_edge_id, VertexEdgeId::from_raw(0xA1B2_C3D4));
        assert_eq!(edge.inline_value, 0x9A8B);

        let mut round_trip = [0u8; Edge::BYTES];
        edge.write_to(&mut round_trip);
        assert_eq!(round_trip, bytes);
    }
}
