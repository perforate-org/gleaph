//! Fixed-width edge records used by graph-kernel CSR storage.
//!
//! This module defines [`Edge`], the hot per-adjacency-slot payload stored in
//! LARA edge slabs. An edge slot is deliberately small: it contains the
//! adjacent vertex id, an owner-local [`VertexEdgeId`], and the compact
//! [`EdgeMeta`] word needed by traversal and placement logic. Larger attributes
//! and external payloads live outside this record.
//!
//! Layout (little-endian on wire):
//!
//! ```text
//! 11        8 7          4 3        0
//! +----------+------------+----------+
//! |   meta   | vertex edge|  target  |
//! |  4 bytes |  id 4 byte | 4 bytes  |
//! +----------+------------+----------+
//! ```
//!
//! The encoded fields are:
//!
//! - `target` (`bytes 0..=3`): the adjacent [`VertexId`] for this CSR
//!   orientation.
//! - `vertex_edge_id` (`bytes 4..=7`): the edge id scoped to the canonical
//!   owner vertex.
//! - `meta` (`bytes 8..=11`): the packed [`EdgeMeta`] word containing flags,
//!   sidecar hints, and the compact label id.
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
pub use meta::{EdgeFlags, EdgeMeta, SideCarKind};

/// Fixed-size adjacency entry stored in one CSR slab slot.
///
/// This is the LARA-style base entry stored in a surface edge region (CSR slab slot).
/// It intentionally contains only the neighbor vertex ref and edge-local hot
/// metadata.
///
/// Invariant:
/// - one [`Edge`] is always exactly 12 bytes
/// - [`VertexEdgeId`] is unique only within the canonical owner vertex
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Edge {
    /// Adjacent vertex id for this slot orientation.
    ///
    /// In a forward CSR row this is the outgoing neighbor. When LARA builds or
    /// rewrites reverse/transposed storage, [`CsrEdge::with_neighbor_vid`]
    /// replaces this value while preserving [`Self::vertex_edge_id`] and
    /// [`Self::meta`].
    pub target: VertexId,
    /// Edge id allocated under the canonical owner vertex.
    ///
    /// Directed edges use the source vertex as owner. Undirected edges should
    /// use a deterministic endpoint owner, typically `min(endpoint_a,
    /// endpoint_b)`. Reverse and duplicated CSR slots carry the same value so
    /// external payload keys can use `(owner_vertex_id, vertex_edge_id)`.
    pub vertex_edge_id: VertexEdgeId,
    /// Hot edge metadata stored beside the target id.
    ///
    /// This carries traversal-time flags, the inline sidecar byte, and the
    /// compact label id. It is serialized after [`Self::vertex_edge_id`] in the
    /// documented [`EdgeMeta`] little-endian layout.
    pub meta: EdgeMeta,
}

impl CsrEdge for Edge {
    /// Encoded width of one edge record in the CSR slab.
    ///
    /// The layout is little-endian [`VertexId`], [`VertexEdgeId`], then
    /// [`EdgeMeta`], four bytes each.
    const BYTES: usize = 12;

    /// Decodes an edge record from its fixed-width storage representation.
    ///
    /// `bytes` must contain exactly [`Self::BYTES`] bytes. The three fields are
    /// decoded in storage order: [`Self::target`], [`Self::vertex_edge_id`],
    /// then [`Self::meta`].
    #[inline]
    fn read_from(bytes: &[u8]) -> Self {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .expect("CsrEdge::read_from expects exactly 12 bytes");

        let mut target = [0; 4];
        target.copy_from_slice(&chunk[0..4]);
        let mut vertex_edge_id = [0; 4];
        vertex_edge_id.copy_from_slice(&chunk[4..8]);
        let mut meta = [0; 4];
        meta.copy_from_slice(&chunk[8..12]);

        Edge {
            target: VertexId::from(u32::from_le_bytes(target)),
            vertex_edge_id: VertexEdgeId::from_le_bytes(vertex_edge_id),
            meta: EdgeMeta::from_le_bytes(meta),
        }
    }

    /// Encodes this edge record into its fixed-width storage representation.
    ///
    /// The write preserves the same layout accepted by [`Self::read_from`]:
    /// target vertex id first, then owner-local edge id, then metadata, all in
    /// little-endian byte order.
    #[inline]
    fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(
            bytes.len(),
            Self::BYTES,
            "CsrEdge::write_to expects exactly 12 bytes"
        );
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.vertex_edge_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.meta.to_le_bytes());
    }

    /// Returns the adjacent vertex id for this edge orientation.
    #[inline]
    fn neighbor_vid(&self) -> VertexId {
        self.target
    }

    /// Returns a copy with the adjacent vertex id replaced.
    ///
    /// All edge-local metadata is preserved exactly, including reserved flag
    /// bits and the inline sidecar byte.
    #[inline]
    fn with_neighbor_vid(self, vid: VertexId) -> Self {
        Self {
            target: vid,
            vertex_edge_id: self.vertex_edge_id,
            meta: self.meta,
        }
    }
}

impl CsrEdgeUndirected for Edge {
    /// Returns whether this slot represents an undirected logical edge.
    ///
    /// The value is delegated to [`EdgeMeta::is_undirected`].
    #[inline]
    fn is_undirected(&self) -> bool {
        self.meta.is_undirected()
    }

    /// Returns a copy with the undirected semantic flag set or cleared.
    ///
    /// The target id and all unrelated metadata fields are preserved.
    #[inline]
    fn with_undirected(self, undirected: bool) -> Self {
        Self {
            target: self.target,
            vertex_edge_id: self.vertex_edge_id,
            meta: self.meta.with_undirected(undirected),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::label::LabelId;

    #[test]
    fn edge_width_matches_documented_storage_layout() {
        assert_eq!(Edge::BYTES, 12);
        assert_eq!(core::mem::size_of::<Edge>(), Edge::BYTES);
    }

    #[test]
    fn write_to_encodes_target_then_metadata_as_little_endian() {
        let mut flags = EdgeFlags::UNDIRECTED | EdgeFlags::REMOTE;
        SideCarKind::QuantizedWeight.apply(&mut flags);
        let edge = Edge {
            target: VertexId::from(0x1234_5678),
            vertex_edge_id: VertexEdgeId::from_raw(0xA1B2_C3D4),
            meta: EdgeMeta::new(flags, 0x9A, LabelId::default()),
        };
        let mut bytes = [0; Edge::BYTES];

        edge.write_to(&mut bytes);

        assert_eq!(
            bytes,
            [
                0x78, 0x56, 0x34, 0x12, 0xD4, 0xC3, 0xB2, 0xA1, 0x00, 0x00, 0x9A, 0x07,
            ]
        );
    }

    #[test]
    fn read_from_decodes_target_and_metadata_and_round_trips() {
        let bytes = [
            0xEF, 0xCD, 0xAB, 0x89, 0x78, 0x56, 0x34, 0x12, 0x34, 0x12, 0x56, 0x8D,
        ];

        let edge = Edge::read_from(&bytes);

        assert_eq!(edge.target, VertexId::from(0x89AB_CDEF));
        assert_eq!(edge.vertex_edge_id, VertexEdgeId::from_raw(0x1234_5678));
        assert_eq!(edge.meta.label_id(), 0x1234);
        assert_eq!(edge.meta.sidecar(), 0x56);
        assert_eq!(edge.meta.flags().bits(), 0x8D);
        assert_eq!(edge.meta.sidecar_kind(), SideCarKind::RecencyBucket);

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
        let meta = EdgeMeta::from_le_bytes([0x34, 0x12, 0x56, 0xF3]);
        let edge = Edge {
            target: VertexId::from(7),
            vertex_edge_id: VertexEdgeId::from_raw(11),
            meta,
        };

        assert_eq!(edge.neighbor_vid(), VertexId::from(7));

        let retargeted = edge.with_neighbor_vid(VertexId::from(99));
        assert_eq!(retargeted.target, VertexId::from(99));
        assert_eq!(retargeted.neighbor_vid(), VertexId::from(99));
        assert_eq!(retargeted.vertex_edge_id, edge.vertex_edge_id);
        assert_eq!(retargeted.meta.raw(), meta.raw());
    }

    #[test]
    fn undirected_accessors_delegate_to_metadata_without_changing_target() {
        let edge = Edge {
            target: VertexId::from(42),
            vertex_edge_id: VertexEdgeId::from_raw(5),
            meta: EdgeMeta::from_le_bytes([0x34, 0x12, 0x56, 0xF2]),
        };

        assert!(!edge.is_undirected());

        let undirected = edge.with_undirected(true);
        assert!(undirected.is_undirected());
        assert_eq!(undirected.target, edge.target);
        assert_eq!(undirected.vertex_edge_id, edge.vertex_edge_id);
        assert_eq!(undirected.meta.raw(), 0xF3_56_12_34);

        let directed = undirected.with_undirected(false);
        assert!(!directed.is_undirected());
        assert_eq!(directed.target, edge.target);
        assert_eq!(directed.meta.raw(), edge.meta.raw());
    }
}
