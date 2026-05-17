//! Ten-byte compact edge records for the labeled multi-level CSR layout.

use super::{VertexEdgeId, vertex_ref::VertexRef};
use ic_stable_lara::{
    VertexId,
    traits::{CsrEdge, CsrEdgeSlabVacancy},
};

/// Fixed-size adjacency entry stored in one labeled edge CSR slot.
///
/// Layout (little-endian on wire):
///
/// ```text
/// 9         8 7          4 3        0
/// +-----------+------------+----------+
/// |inline_val | vertex_edge|  target  |
/// |  2 bytes  |  id 4 byte | 4 bytes  |
/// +-----------+------------+----------+
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CompactEdge {
    /// Adjacent vertex reference for this slot orientation.
    pub target: VertexRef,
    /// Edge id allocated under the canonical owner vertex.
    pub vertex_edge_id: VertexEdgeId,
    /// Raw `u16` inline payload; interpretation is label-defined.
    pub inline_value: u16,
}

impl CompactEdge {
    /// Fixed byte width of one encoded edge record.
    pub const BYTES: usize = 10;

    #[inline]
    pub const fn new(target: VertexRef, vertex_edge_id: VertexEdgeId, inline_value: u16) -> Self {
        Self {
            target,
            vertex_edge_id,
            inline_value,
        }
    }

    #[inline]
    fn write_to_bytes(self, bytes: &mut [u8; Self::BYTES]) {
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.vertex_edge_id.to_le_bytes());
        bytes[8..10].copy_from_slice(&self.inline_value.to_le_bytes());
    }
}

impl CsrEdge for CompactEdge {
    const BYTES: usize = Self::BYTES;

    fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("CompactEdge::read_from expects exactly 10 bytes");
        Self {
            target: VertexRef::from_le_bytes(chunk[0..4].try_into().unwrap()),
            vertex_edge_id: VertexEdgeId::from_le_bytes(chunk[4..8].try_into().unwrap()),
            inline_value: u16::from_le_bytes(chunk[8..10].try_into().unwrap()),
        }
    }

    fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        let mut chunk = [0u8; Self::BYTES];
        self.write_to_bytes(&mut chunk);
        bytes.copy_from_slice(&chunk);
    }

    fn neighbor_vid(&self) -> VertexId {
        self.target.local_id()
    }

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

impl CsrEdgeSlabVacancy for CompactEdge {
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
    fn compact_edge_round_trips_exact_layout() {
        let edge = CompactEdge::new(
            VertexRef::remote(VertexId::from(0x1234_5678)),
            VertexEdgeId::from_raw(0xA1B2_C3D4),
            0x9A8B,
        );
        let mut bytes = [0u8; CompactEdge::BYTES];
        edge.write_to(&mut bytes);
        assert_eq!(
            bytes,
            [0x78, 0x56, 0x34, 0x92, 0xD4, 0xC3, 0xB2, 0xA1, 0x8B, 0x9A,]
        );
        assert_eq!(CompactEdge::read_from(&bytes), edge);
    }
}
