//! Fixed-width edge records used by graph-kernel CSR storage.
//!
//! This module defines [`Edge`], the hot per-adjacency-slot payload stored in
//! LARA edge slabs. An edge slot is deliberately small: it contains the
//! adjacent vertex id, an owner-local [`VertexEdgeId`], a raw [`u16`]
//! [`Edge::inline_value`] for label-defined semantics (for example traversal
//! weights), and the compact [`EdgeMeta`] word for traversal and placement.
//!
//! Layout (little-endian on wire):
//!
//! ```text
//! 11       10 9         8 7          4 3        0
//! +----------+-----------+------------+----------+
//! |   meta   |inline_val | vertex edge|  target  |
//! |  2 bytes |  2 bytes  |  id 4 byte | 4 bytes  |
//! +----------+-----------+------------+----------+
//! ```
//!
//! The encoded fields are:
//!
//! - `target` (`bytes 0..=3`): the adjacent [`VertexId`] for this CSR
//!   orientation.
//! - `vertex_edge_id` (`bytes 4..=7`): the edge id scoped to the canonical
//!   owner vertex.
//! - `inline_value` (`bytes 8..=9`): raw `u16` payload; interpretation is
//!   label-level (for example [`crate::entry::weight::EdgeWeightProfile`]).
//! - `meta` (`bytes 10..=11`): packed [`EdgeMeta`] (inline label id, remote,
//!   undirected).
//!
//! Use the [`CsrEdge`] implementation when crossing a slab storage boundary.
//! It is the compatibility contract that keeps every [`Edge`] exactly twelve
//! bytes and round-trippable through the same little-endian layout.

use ic_stable_lara::{
    VertexId,
    traits::{CsrEdge, CsrEdgeUndirected},
};

mod id;
mod meta;

pub use id::VertexEdgeId;
pub use meta::EdgeMeta;

/// Fixed-size adjacency entry stored in one CSR slab slot.
///
/// Invariant:
/// - one [`Edge`] is always exactly 12 bytes
/// - [`VertexEdgeId`] is unique only within the canonical owner vertex
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Edge {
    /// Adjacent vertex id for this slot orientation.
    pub target: VertexId,
    /// Edge id allocated under the canonical owner vertex.
    pub vertex_edge_id: VertexEdgeId,
    /// Raw `u16` inline payload (label-defined; e.g. encoded traversal weight).
    pub inline_value: u16,
    /// Inline edge label id and CSR placement flags.
    pub meta: EdgeMeta,
}

impl CsrEdge for Edge {
    const BYTES: usize = 12;

    #[inline]
    fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("CsrEdge::read_from expects exactly 12 bytes");

        let mut target = [0; 4];
        target.copy_from_slice(&chunk[0..4]);
        let mut vertex_edge_id = [0; 4];
        vertex_edge_id.copy_from_slice(&chunk[4..8]);
        let mut inline_value = [0; 2];
        inline_value.copy_from_slice(&chunk[8..10]);
        let mut meta = [0; 2];
        meta.copy_from_slice(&chunk[10..12]);

        Edge {
            target: VertexId::from(u32::from_le_bytes(target)),
            vertex_edge_id: VertexEdgeId::from_le_bytes(vertex_edge_id),
            inline_value: u16::from_le_bytes(inline_value),
            meta: EdgeMeta::from_le_bytes(meta),
        }
    }

    #[inline]
    fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(
            bytes.len(),
            Self::BYTES,
            "CsrEdge::write_to expects exactly 12 bytes"
        );
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.vertex_edge_id.to_le_bytes());
        bytes[8..10].copy_from_slice(&self.inline_value.to_le_bytes());
        bytes[10..12].copy_from_slice(&self.meta.to_le_bytes());
    }

    #[inline]
    fn neighbor_vid(&self) -> VertexId {
        self.target
    }

    #[inline]
    fn with_neighbor_vid(self, vid: VertexId) -> Self {
        Self {
            target: vid,
            vertex_edge_id: self.vertex_edge_id,
            inline_value: self.inline_value,
            meta: self.meta,
        }
    }
}

impl CsrEdgeUndirected for Edge {
    #[inline]
    fn is_undirected(&self) -> bool {
        self.meta.is_undirected()
    }

    #[inline]
    fn with_undirected(self, undirected: bool) -> Self {
        Self {
            target: self.target,
            vertex_edge_id: self.vertex_edge_id,
            inline_value: self.inline_value,
            meta: self.meta.with_undirected(undirected),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::label::InlineEdgeLabelId;

    #[test]
    fn edge_width_matches_documented_storage_layout() {
        assert_eq!(Edge::BYTES, 12);
        assert_eq!(core::mem::size_of::<Edge>(), Edge::BYTES);
    }

    #[test]
    fn write_to_encodes_fields_as_little_endian() {
        let label = InlineEdgeLabelId::try_from_raw(0x123).unwrap();
        let edge = Edge {
            target: VertexId::from(0x1234_5678),
            vertex_edge_id: VertexEdgeId::from_raw(0xA1B2_C3D4),
            inline_value: 0x9A8B,
            meta: EdgeMeta::new(true, false, Some(label)),
        };
        let mut bytes = [0; Edge::BYTES];
        edge.write_to(&mut bytes);
        assert_eq!(
            bytes,
            [
                0x78, 0x56, 0x34, 0x12, 0xD4, 0xC3, 0xB2, 0xA1, 0x8B, 0x9A, 0x23, 0x41,
            ]
        );
    }

    #[test]
    fn read_from_round_trips_all_fields() {
        let bytes = [
            0xEF, 0xCD, 0xAB, 0x89, 0x78, 0x56, 0x34, 0x12, 0x10, 0x20, 0x34, 0x80,
        ];
        let edge = Edge::read_from(&bytes);
        assert_eq!(edge.target, VertexId::from(0x89AB_CDEF));
        assert_eq!(edge.vertex_edge_id, VertexEdgeId::from_raw(0x1234_5678));
        assert_eq!(edge.inline_value, 0x2010);
        assert_eq!(edge.meta.inline_label_bits(), 0x34);
        assert!(edge.meta.is_undirected());
        assert!(!edge.meta.is_remote());

        let mut round_trip = [0; Edge::BYTES];
        edge.write_to(&mut round_trip);
        assert_eq!(round_trip, bytes);
    }

    #[test]
    fn read_from_requires_exactly_one_edge_record() {
        let too_short = [0; Edge::BYTES - 1];
        let too_long = [0; Edge::BYTES + 1];
        assert!(std::panic::catch_unwind(|| Edge::read_from(&too_short)).is_err());
        assert!(std::panic::catch_unwind(|| Edge::read_from(&too_long)).is_err());
    }

    #[test]
    fn neighbor_vid_accessors_only_touch_target() {
        let meta = EdgeMeta::from_le_bytes([0x05, 0x80]);
        let edge = Edge {
            target: VertexId::from(7),
            vertex_edge_id: VertexEdgeId::from_raw(11),
            inline_value: 99,
            meta,
        };
        assert_eq!(edge.neighbor_vid(), VertexId::from(7));
        let retargeted = edge.with_neighbor_vid(VertexId::from(99));
        assert_eq!(retargeted.target, VertexId::from(99));
        assert_eq!(retargeted.vertex_edge_id, edge.vertex_edge_id);
        assert_eq!(retargeted.inline_value, edge.inline_value);
        assert_eq!(retargeted.meta.raw(), meta.raw());
    }

    #[test]
    fn undirected_accessors_delegate_to_metadata() {
        let edge = Edge {
            target: VertexId::from(42),
            vertex_edge_id: VertexEdgeId::from_raw(5),
            inline_value: 0,
            meta: EdgeMeta::from_le_bytes([0x01, 0x40]),
        };
        assert!(!edge.is_undirected());
        let undirected = edge.with_undirected(true);
        assert!(undirected.is_undirected());
        assert_eq!(undirected.inline_value, edge.inline_value);
        let directed = undirected.with_undirected(false);
        assert!(!directed.is_undirected());
        assert_eq!(directed.meta.raw(), edge.meta.raw());
    }
}
