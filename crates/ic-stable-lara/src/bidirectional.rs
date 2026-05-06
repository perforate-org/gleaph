//! Bidirectional LARA graph wrapper.
//!
//! A [`LaraGraph`] stores one oriented adjacency index. This
//! module composes two such indexes: a forward graph for out-neighbors and a
//! reverse graph for the transpose.

#[cfg(feature = "canbench")]
mod bench;
pub mod deferred;

use crate::{
    GrowFailed, LaraGraph, VertexCount, VertexId,
    lara::{InitError, edge::counts::EdgePmaCountsStride},
    traits::{CsrEdge, CsrEdgeUndirected, LaraVertex},
};
use ic_stable_structures::Memory;
use std::fmt;

pub use deferred::{
    BidirectionalMaintenanceReport, DeferredBidirectionalLara, DeferredBidirectionalLaraError,
    DeferredBidirectionalLaraGraph,
};

pub(crate) trait UndirectedEdgeFlag {
    fn marked_undirected(&self) -> bool;
}

impl<E: CsrEdge> UndirectedEdgeFlag for E {
    default fn marked_undirected(&self) -> bool {
        false
    }
}

impl<E: CsrEdge + CsrEdgeUndirected> UndirectedEdgeFlag for E {
    fn marked_undirected(&self) -> bool {
        CsrEdgeUndirected::is_undirected(self)
    }
}

/// Failure from bidirectional LARA graph operations.
#[derive(Debug)]
pub enum BidirectionalLaraError {
    /// Forward store operation failed.
    Forward(&'static str),
    /// Reverse store operation failed.
    Reverse(&'static str),
    /// Forward store initialization failed.
    ForwardInit(InitError),
    /// Reverse store initialization failed.
    ReverseInit(InitError),
    /// A stable memory grow operation failed.
    Grow(GrowFailed),
    /// Forward and reverse vertex columns have different lengths.
    VertexCountMismatch {
        /// Forward vertex count.
        forward: VertexCount,
        /// Reverse vertex count.
        reverse: VertexCount,
    },
    /// A requested vertex id is outside the graph.
    VertexOutOfRange {
        /// Out-of-range vertex id.
        vid: VertexId,
        /// Current graph vertex count.
        len: VertexCount,
    },
    /// The edge payload neighbor does not match the destination argument.
    NeighborMismatch {
        /// Destination vertex expected by the API call.
        expected: VertexId,
        /// Neighbor id carried by the edge payload.
        actual: VertexId,
    },
    /// A directed insert received an edge payload marked as undirected.
    UndirectedEdgeInDirectedInsert,
}

impl fmt::Display for BidirectionalLaraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forward(e) => write!(f, "forward store: {e}"),
            Self::Reverse(e) => write!(f, "reverse store: {e}"),
            Self::ForwardInit(e) => write!(f, "forward init failed: {e}"),
            Self::ReverseInit(e) => write!(f, "reverse init failed: {e}"),
            Self::Grow(e) => write!(f, "format / grow: {e}"),
            Self::VertexCountMismatch { forward, reverse } => write!(
                f,
                "vertex column length mismatch: forward={forward} reverse={reverse}"
            ),
            Self::VertexOutOfRange { vid, len } => {
                write!(f, "vertex {vid} out of range (len={len})")
            }
            Self::NeighborMismatch { expected, actual } => write!(
                f,
                "edge neighbor_vid {actual} does not match dst {expected}"
            ),
            Self::UndirectedEdgeInDirectedInsert => write!(
                f,
                "directed insert: edge is marked undirected; use insert_undirected"
            ),
        }
    }
}

impl std::error::Error for BidirectionalLaraError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ForwardInit(e) | Self::ReverseInit(e) => Some(e),
            Self::Grow(e) => Some(e),
            _ => None,
        }
    }
}

impl From<GrowFailed> for BidirectionalLaraError {
    fn from(e: GrowFailed) -> Self {
        Self::Grow(e)
    }
}

/// Two synchronized LARA adjacency stores: forward out-adjacency and reverse in-adjacency.
pub struct BidirectionalLaraGraph<E, V, M>
where
    E: CsrEdge + EdgePmaCountsStride,
    V: LaraVertex,
    M: Memory,
{
    forward: LaraGraph<E, V, M>,
    reverse: LaraGraph<E, V, M>,
}

/// Convenience alias for [`BidirectionalLaraGraph`].
pub type BidirectionalLara<E, V, M> = BidirectionalLaraGraph<E, V, M>;

impl<E, V, M> BidirectionalLaraGraph<E, V, M>
where
    E: CsrEdge + EdgePmaCountsStride,
    V: LaraVertex,
    M: Memory,
{
    /// Creates fresh forward and reverse LARA stores.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        forward_vertices: M,
        forward_counts: M,
        forward_edges: M,
        forward_log: M,
        forward_span_meta: M,
        forward_free_spans: M,
        forward_free_span_by_start: M,
        reverse_vertices: M,
        reverse_counts: M,
        reverse_edges: M,
        reverse_log: M,
        reverse_span_meta: M,
        reverse_free_spans: M,
        reverse_free_span_by_start: M,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
    ) -> Result<Self, BidirectionalLaraError> {
        let forward = LaraGraph::new(
            forward_vertices,
            forward_counts,
            forward_edges,
            forward_log,
            forward_span_meta,
            forward_free_spans,
            forward_free_span_by_start,
            elem_capacity,
            segment_count,
            segment_size,
        )?;
        let reverse = LaraGraph::new(
            reverse_vertices,
            reverse_counts,
            reverse_edges,
            reverse_log,
            reverse_span_meta,
            reverse_free_spans,
            reverse_free_span_by_start,
            elem_capacity,
            segment_count,
            segment_size,
        )?;
        Ok(Self { forward, reverse })
    }

    /// Reopens forward and reverse LARA stores from stable memories.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        forward_vertices: M,
        forward_counts: M,
        forward_edges: M,
        forward_log: M,
        forward_span_meta: M,
        forward_free_spans: M,
        forward_free_span_by_start: M,
        reverse_vertices: M,
        reverse_counts: M,
        reverse_edges: M,
        reverse_log: M,
        reverse_span_meta: M,
        reverse_free_spans: M,
        reverse_free_span_by_start: M,
    ) -> Result<Self, BidirectionalLaraError> {
        let forward = LaraGraph::init(
            forward_vertices,
            forward_counts,
            forward_edges,
            forward_log,
            forward_span_meta,
            forward_free_spans,
            forward_free_span_by_start,
        )
        .map_err(BidirectionalLaraError::ForwardInit)?;
        let reverse = LaraGraph::init(
            reverse_vertices,
            reverse_counts,
            reverse_edges,
            reverse_log,
            reverse_span_meta,
            reverse_free_spans,
            reverse_free_span_by_start,
        )
        .map_err(BidirectionalLaraError::ReverseInit)?;
        let graph = Self { forward, reverse };
        graph.ensure_matching_vertex_counts()?;
        Ok(graph)
    }

    /// Returns the forward out-adjacency graph.
    pub fn forward(&self) -> &LaraGraph<E, V, M> {
        &self.forward
    }

    /// Returns the reverse in-adjacency graph.
    pub fn reverse(&self) -> &LaraGraph<E, V, M> {
        &self.reverse
    }

    /// Consumes the wrapper and returns all forward memories followed by all reverse memories.
    #[allow(clippy::type_complexity)]
    pub fn into_memories(self) -> (M, M, M, M, M, M, M, M, M, M, M, M, M, M) {
        let (fv, fc, fe, fl, fs, ff, ffs) = self.forward.into_memories();
        let (rv, rc, re, rl, rs, rf, rfs) = self.reverse.into_memories();
        (fv, fc, fe, fl, fs, ff, ffs, rv, rc, re, rl, rs, rf, rfs)
    }

    /// Returns the number of vertices in both orientations.
    pub fn vertex_count(&self) -> VertexCount {
        VertexCount(self.forward.vertices().len())
    }

    /// Appends the same vertex row to the forward and reverse stores.
    pub fn push_vertex(&self, vertex: V) -> Result<VertexId, BidirectionalLaraError> {
        let id = self.forward.push_vertex(vertex)?;
        self.reverse.push_vertex(vertex)?;
        self.ensure_matching_vertex_counts()?;
        Ok(id)
    }

    /// Collects outgoing edges from the forward store.
    pub fn out_edges(&self, src: VertexId) -> Result<Vec<E>, BidirectionalLaraError> {
        self.ensure_vertex(src)?;
        self.forward
            .collect_out_edges(src)
            .map_err(BidirectionalLaraError::Forward)
    }

    /// Collects incoming edges from the reverse store.
    pub fn in_edges(&self, dst: VertexId) -> Result<Vec<E>, BidirectionalLaraError> {
        self.ensure_vertex(dst)?;
        self.reverse
            .collect_out_edges(dst)
            .map_err(BidirectionalLaraError::Reverse)
    }

    /// Inserts a directed edge from `src` to `dst`.
    ///
    /// `edge.neighbor_vid()` must equal `dst`; the reverse orientation stores a
    /// copy whose neighbor id is rewritten to `src`.
    pub fn insert_directed(
        &self,
        src: VertexId,
        dst: VertexId,
        edge: E,
    ) -> Result<(), BidirectionalLaraError> {
        self.ensure_vertex(src)?;
        self.ensure_vertex(dst)?;
        if edge.neighbor_vid() != dst {
            return Err(BidirectionalLaraError::NeighborMismatch {
                expected: dst,
                actual: edge.neighbor_vid(),
            });
        }
        if <E as UndirectedEdgeFlag>::marked_undirected(&edge) {
            return Err(BidirectionalLaraError::UndirectedEdgeInDirectedInsert);
        }

        self.forward
            .insert_edge(src, edge)
            .map_err(BidirectionalLaraError::Forward)?;
        self.reverse
            .insert_edge(dst, edge.with_neighbor_vid(src))
            .map_err(BidirectionalLaraError::Reverse)?;
        Ok(())
    }

    /// Inserts an undirected edge by materializing both directions in both orientations.
    pub fn insert_undirected(
        &self,
        u: VertexId,
        v: VertexId,
        edge: E,
    ) -> Result<(), BidirectionalLaraError>
    where
        E: CsrEdgeUndirected,
    {
        self.ensure_vertex(u)?;
        self.ensure_vertex(v)?;
        let edge = edge.with_undirected(true);

        if u == v {
            let loop_edge = edge.with_neighbor_vid(u);
            self.forward
                .insert_edge(u, loop_edge)
                .map_err(BidirectionalLaraError::Forward)?;
            self.reverse
                .insert_edge(u, loop_edge)
                .map_err(BidirectionalLaraError::Reverse)?;
            return Ok(());
        }

        self.forward
            .insert_edge(u, edge.with_neighbor_vid(v))
            .map_err(BidirectionalLaraError::Forward)?;
        self.forward
            .insert_edge(v, edge.with_neighbor_vid(u))
            .map_err(BidirectionalLaraError::Forward)?;
        self.reverse
            .insert_edge(v, edge.with_neighbor_vid(u))
            .map_err(BidirectionalLaraError::Reverse)?;
        self.reverse
            .insert_edge(u, edge.with_neighbor_vid(v))
            .map_err(BidirectionalLaraError::Reverse)?;
        Ok(())
    }

    fn ensure_matching_vertex_counts(&self) -> Result<(), BidirectionalLaraError> {
        let forward = VertexCount(self.forward.vertices().len());
        let reverse = VertexCount(self.reverse.vertices().len());
        if forward != reverse {
            return Err(BidirectionalLaraError::VertexCountMismatch { forward, reverse });
        }
        Ok(())
    }

    fn ensure_vertex(&self, vid: VertexId) -> Result<(), BidirectionalLaraError> {
        let len = self.vertex_count();
        if u64::from(u32::from(vid)) >= u64::from(len) {
            return Err(BidirectionalLaraError::VertexOutOfRange { vid, len });
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::CsrEdgeUndirected;
    use crate::{
        Vertex,
        test_support::{TestEdge, UndirectedTestEdge, bidirectional_test_graph},
    };

    #[test]
    fn bidirectional_directed_insert_updates_forward_and_reverse() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4, 8]);

        graph
            .insert_directed(VertexId::from(0), VertexId::from(2), TestEdge(2))
            .unwrap();

        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(2)]
        );
        assert_eq!(graph.out_edges(VertexId::from(2)).unwrap(), Vec::new());
        assert_eq!(
            graph.in_edges(VertexId::from(2)).unwrap(),
            vec![TestEdge(0)]
        );
        assert_eq!(graph.in_edges(VertexId::from(0)).unwrap(), Vec::new());
    }

    #[test]
    fn bidirectional_directed_insert_rejects_neighbor_mismatch() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4]);

        let err = graph
            .insert_directed(VertexId::from(0), VertexId::from(1), TestEdge(0))
            .unwrap_err();

        assert!(matches!(
            err,
            BidirectionalLaraError::NeighborMismatch {
                expected,
                actual
            } if expected == VertexId::from(1) && actual == VertexId::from(0)
        ));
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap(), Vec::new());
        assert_eq!(graph.in_edges(VertexId::from(1)).unwrap(), Vec::new());
    }

    #[test]
    fn bidirectional_directed_insert_rejects_undirected_edge() {
        let graph = bidirectional_test_graph::<UndirectedTestEdge>(&[0, 4]);
        let edge = UndirectedTestEdge::new(1).with_undirected(true);

        let err = graph
            .insert_directed(VertexId::from(0), VertexId::from(1), edge)
            .unwrap_err();

        assert!(matches!(
            err,
            BidirectionalLaraError::UndirectedEdgeInDirectedInsert
        ));
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap(), Vec::new());
        assert_eq!(graph.in_edges(VertexId::from(1)).unwrap(), Vec::new());
    }

    #[test]
    fn bidirectional_undirected_insert_materializes_symmetric_adjacency() {
        let graph = bidirectional_test_graph::<UndirectedTestEdge>(&[0, 4, 8]);

        graph
            .insert_undirected(
                VertexId::from(0),
                VertexId::from(2),
                UndirectedTestEdge::new(2),
            )
            .unwrap();

        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 2,
                undirected: true
            }]
        );
        assert_eq!(
            graph.out_edges(VertexId::from(2)).unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 0,
                undirected: true
            }]
        );
        assert_eq!(
            graph.in_edges(VertexId::from(0)).unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 2,
                undirected: true
            }]
        );
        assert_eq!(
            graph.in_edges(VertexId::from(2)).unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 0,
                undirected: true
            }]
        );
    }

    #[test]
    fn bidirectional_undirected_self_loop_stores_one_loop_per_orientation() {
        let graph = bidirectional_test_graph::<UndirectedTestEdge>(&[0, 4]);

        graph
            .insert_undirected(
                VertexId::from(1),
                VertexId::from(1),
                UndirectedTestEdge::new(1),
            )
            .unwrap();

        let loop_edge = UndirectedTestEdge {
            neighbor: 1,
            undirected: true,
        };
        assert_eq!(graph.out_edges(VertexId::from(1)).unwrap(), vec![loop_edge]);
        assert_eq!(graph.in_edges(VertexId::from(1)).unwrap(), vec![loop_edge]);
    }

    #[test]
    fn bidirectional_reopen_preserves_forward_and_reverse_stores() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4, 8]);
        graph
            .insert_directed(VertexId::from(0), VertexId::from(2), TestEdge(2))
            .unwrap();

        let (fv, fc, fe, fl, fs, ff, ffs, rv, rc, re, rl, rs, rf, rfs) = graph.into_memories();
        let reopened = BidirectionalLaraGraph::<TestEdge, Vertex, _>::init(
            fv, fc, fe, fl, fs, ff, ffs, rv, rc, re, rl, rs, rf, rfs,
        )
        .unwrap();

        assert_eq!(
            reopened.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(2)]
        );
        assert_eq!(
            reopened.in_edges(VertexId::from(2)).unwrap(),
            vec![TestEdge(0)]
        );
    }

    #[test]
    fn insert_directed_rejects_out_of_range_src() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4]);

        let err = graph
            .insert_directed(VertexId::from(2), VertexId::from(1), TestEdge(1))
            .unwrap_err();

        assert!(matches!(
            err,
            BidirectionalLaraError::VertexOutOfRange { vid, len }
                if vid == VertexId::from(2) && len == VertexCount::from(2)
        ));
    }

    #[test]
    fn insert_directed_rejects_out_of_range_dst() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4]);

        let err = graph
            .insert_directed(VertexId::from(0), VertexId::from(2), TestEdge(2))
            .unwrap_err();

        assert!(matches!(
            err,
            BidirectionalLaraError::VertexOutOfRange { vid, len }
                if vid == VertexId::from(2) && len == VertexCount::from(2)
        ));
    }

    #[test]
    fn display_formats_validation_errors() {
        assert_eq!(
            BidirectionalLaraError::NeighborMismatch {
                expected: VertexId::from(1),
                actual: VertexId::from(0),
            }
            .to_string(),
            "edge neighbor_vid 0 does not match dst 1"
        );
        assert_eq!(
            BidirectionalLaraError::UndirectedEdgeInDirectedInsert.to_string(),
            "directed insert: edge is marked undirected; use insert_undirected"
        );
        assert_eq!(
            (BidirectionalLaraError::VertexCountMismatch {
                forward: VertexCount::from(1),
                reverse: VertexCount::from(2),
            })
            .to_string(),
            "vertex column length mismatch: forward=1 reverse=2"
        );
    }
}
