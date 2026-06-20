//! Bidirectional LARA graph wrapper.
//!
//! A [`LaraGraph`] stores one oriented adjacency index. This module composes two such indexes:
//! a forward graph for out-neighbors and a reverse graph for the transpose.
//!
//! ## Typed out-adjacency (aligned with labeled bidirectional)
//!
//! Both unlabeled and labeled bidirectional wrappers expose the same surface:
//! `directed_out_edges`, `directed_in_edges`, `undirected_edges`, `for_each_*`, and
//! `*_edges_iter` (with [`OutEdgeOrder`]).
//!
//! **Unlabeled** graphs discriminate directed vs undirected via [`CsrEdgeUndirected`] on the
//! edge payload ([`UndirectedEdgeFlag`]). **Labeled** graphs use [`crate::labeled::BucketLabelKey`]
//! bucket MSB (see [`crate::labeled::bidirectional`]). Gleaph production code uses labeled only.

mod adjacency;
#[cfg(feature = "canbench")]
mod bench;
pub mod deferred;

use crate::{
    GrowFailed, LaraGraph, VertexCount, VertexId,
    lara::{InitError, operation_error::LaraOperationError},
    traits::{CsrEdge, CsrEdgeTombstone, CsrEdgeUndirected, CsrVertex},
};
use adjacency::{OutEdgeDirectednessFilter, filtered_out_edges_iter, for_each_lara_out_filtered};
use ic_stable_structures::Memory;
use std::fmt;

pub use crate::labeled::OutEdgeOrder;
pub use adjacency::FilteredOutEdgesIter;
pub use deferred::{
    BidirectionalMaintenanceReport, DeferredBidirectionalLara, DeferredBidirectionalLaraError,
    DeferredBidirectionalLaraGraph, DeleteEdgeObserver,
};

/// Unlabeled bidirectional graphs only: undirected adjacency via [`CsrEdgeUndirected`].
///
/// Labeled CSR uses bucket wire keys; see [`crate::labeled::bidirectional`].
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
    Forward(LaraOperationError),
    /// Reverse store operation failed.
    Reverse(LaraOperationError),
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
            Self::Forward(e) | Self::Reverse(e) => Some(e),
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
    E: CsrEdge,
    V: CsrVertex,
    M: Memory,
{
    forward: LaraGraph<E, V, M>,
    reverse: LaraGraph<E, V, M>,
}

/// Convenience alias for [`BidirectionalLaraGraph`].
pub type BidirectionalLara<E, V, M> = BidirectionalLaraGraph<E, V, M>;

impl<E, V, M> BidirectionalLaraGraph<E, V, M>
where
    E: CsrEdge,
    V: CsrVertex,
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
        segment_size: u32,
        initial_vertex_edge_slots: u32,
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
            segment_size,
            initial_vertex_edge_slots,
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
            segment_size,
            initial_vertex_edge_slots,
        )?;
        Ok(Self { forward, reverse })
    }

    /// Opens forward and reverse LARA stores from stable memories, creating them when empty.
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
        elem_capacity: u64,
        segment_size: u32,
        initial_vertex_edge_slots: u32,
    ) -> Result<Self, BidirectionalLaraError> {
        let forward = LaraGraph::init(
            forward_vertices,
            forward_counts,
            forward_edges,
            forward_log,
            forward_span_meta,
            forward_free_spans,
            forward_free_span_by_start,
            elem_capacity,
            segment_size,
            initial_vertex_edge_slots,
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
            elem_capacity,
            segment_size,
            initial_vertex_edge_slots,
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

    /// Copies the vertex row from the forward store.
    ///
    /// Forward and reverse vertex tables stay aligned for all supported mutation
    /// paths; callers must update both together via [`Self::set_vertex_row`].
    pub fn vertex_row(&self, vid: VertexId) -> Result<V, BidirectionalLaraError> {
        self.ensure_vertex(vid)?;
        Ok(self.forward.vertices().get(vid))
    }

    /// Overwrites the vertex payload in **both** forward and reverse stores.
    ///
    /// This keeps the invariant established by [`Self::push_vertex`].
    pub fn set_vertex_row(&self, vid: VertexId, row: &V) -> Result<(), BidirectionalLaraError> {
        self.ensure_vertex(vid)?;
        self.forward.vertices().set(vid, row);
        self.reverse.vertices().set(vid, row);
        Ok(())
    }

    /// All directed outgoing edges at `src` in ascending slot order.
    pub fn directed_out_edges(&self, src: VertexId) -> Result<Vec<E>, BidirectionalLaraError> {
        let mut edges = Vec::new();
        self.for_each_directed_out_edges(src, OutEdgeOrder::Ascending, |edge| edges.push(edge))?;
        Ok(edges)
    }

    /// All directed incoming edges at `dst` in ascending slot order.
    pub fn directed_in_edges(&self, dst: VertexId) -> Result<Vec<E>, BidirectionalLaraError> {
        let mut edges = Vec::new();
        self.for_each_directed_in_edges(dst, OutEdgeOrder::Ascending, |edge| edges.push(edge))?;
        Ok(edges)
    }

    /// Undirected outgoing edges at `src` in ascending slot order (forward store only).
    pub fn undirected_edges(&self, src: VertexId) -> Result<Vec<E>, BidirectionalLaraError> {
        let mut edges = Vec::new();
        self.for_each_undirected_edges(src, OutEdgeOrder::Ascending, |edge| edges.push(edge))?;
        Ok(edges)
    }

    /// Visits directed forward outgoing edges in `order`.
    pub fn for_each_directed_out_edges<Visit>(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), BidirectionalLaraError>
    where
        Visit: FnMut(E),
    {
        self.ensure_vertex(src)?;
        for_each_lara_out_filtered(
            &self.forward,
            src,
            OutEdgeDirectednessFilter::DirectedOnly,
            order,
            &mut visit,
        )
        .map_err(BidirectionalLaraError::Forward)
    }

    /// Visits undirected forward outgoing edges in `order`.
    pub fn for_each_undirected_edges<Visit>(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), BidirectionalLaraError>
    where
        Visit: FnMut(E),
    {
        self.ensure_vertex(src)?;
        for_each_lara_out_filtered(
            &self.forward,
            src,
            OutEdgeDirectednessFilter::UndirectedOnly,
            order,
            &mut visit,
        )
        .map_err(BidirectionalLaraError::Forward)
    }

    /// Visits directed incoming edges at `dst` in `order` (reverse store).
    pub fn for_each_directed_in_edges<Visit>(
        &self,
        dst: VertexId,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), BidirectionalLaraError>
    where
        Visit: FnMut(E),
    {
        self.ensure_vertex(dst)?;
        for_each_lara_out_filtered(
            &self.reverse,
            dst,
            OutEdgeDirectednessFilter::DirectedOnly,
            order,
            &mut visit,
        )
        .map_err(BidirectionalLaraError::Reverse)
    }

    /// Streaming directed forward out-edges filtered by edge payload in `order`.
    pub fn directed_out_edges_iter(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<FilteredOutEdgesIter<'_, E, M>, BidirectionalLaraError> {
        self.ensure_vertex(src)?;
        filtered_out_edges_iter(
            &self.forward,
            src,
            OutEdgeDirectednessFilter::DirectedOnly,
            order,
        )
        .map_err(BidirectionalLaraError::Forward)
    }

    /// Streaming undirected forward out-edges filtered by edge payload in `order`.
    pub fn undirected_edges_iter(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<FilteredOutEdgesIter<'_, E, M>, BidirectionalLaraError> {
        self.ensure_vertex(src)?;
        filtered_out_edges_iter(
            &self.forward,
            src,
            OutEdgeDirectednessFilter::UndirectedOnly,
            order,
        )
        .map_err(BidirectionalLaraError::Forward)
    }

    /// Streaming directed incoming edges filtered by edge payload in `order`.
    pub fn directed_in_edges_iter(
        &self,
        dst: VertexId,
        order: OutEdgeOrder,
    ) -> Result<FilteredOutEdgesIter<'_, E, M>, BidirectionalLaraError> {
        self.ensure_vertex(dst)?;
        filtered_out_edges_iter(
            &self.reverse,
            dst,
            OutEdgeDirectednessFilter::DirectedOnly,
            order,
        )
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

        let reverse = edge.with_neighbor_vid(src);
        self.forward
            .insert_edge(src, edge)
            .map_err(BidirectionalLaraError::Forward)?;
        self.reverse
            .insert_edge(dst, reverse)
            .map_err(BidirectionalLaraError::Reverse)?;
        Ok(())
    }

    /// Removes one directed edge record from `src` to `dst` without preserving adjacency order.
    ///
    /// `edge.neighbor_vid()` must equal `dst`. When parallel edges connect the
    /// same vertices, the full edge record selects which one is removed. Both
    /// forward and reverse orientations are updated; a mismatch is reported as
    /// a storage error.
    pub fn remove_directed(
        &self,
        src: VertexId,
        dst: VertexId,
        edge: E,
    ) -> Result<bool, BidirectionalLaraError>
    where
        E: PartialEq + CsrEdgeTombstone,
    {
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
        Ok(self
            .remove_directed_record_unchecked(src, dst, edge)?
            .is_some())
    }

    /// Removes the first directed edge accepted by `matches`.
    ///
    /// The predicate is evaluated against the forward `src -> dst` record after
    /// filtering by `dst`. The returned edge is the forward record that was
    /// removed.
    pub fn remove_directed_matching<F>(
        &self,
        src: VertexId,
        dst: VertexId,
        matches: F,
    ) -> Result<Option<E>, BidirectionalLaraError>
    where
        E: PartialEq + CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(src)?;
        self.ensure_vertex(dst)?;
        self.remove_directed_matching_unchecked(src, dst, matches)
    }

    fn remove_directed_record_unchecked(
        &self,
        src: VertexId,
        dst: VertexId,
        edge: E,
    ) -> Result<Option<E>, BidirectionalLaraError>
    where
        E: PartialEq + CsrEdgeTombstone,
    {
        self.remove_directed_matching_unchecked(src, dst, |candidate| *candidate == edge)
    }

    fn remove_directed_matching_unchecked<F>(
        &self,
        src: VertexId,
        dst: VertexId,
        mut matches: F,
    ) -> Result<Option<E>, BidirectionalLaraError>
    where
        E: PartialEq + CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        let removed_forward = self
            .forward
            .remove_edge_matching(src, |edge| edge.neighbor_vid() == dst && matches(edge))
            .map_err(BidirectionalLaraError::Forward)?;
        let Some(edge) = removed_forward else {
            return Ok(None);
        };
        let removed_reverse = self
            .reverse
            .remove_edge(dst, edge.with_neighbor_vid(src))
            .map_err(BidirectionalLaraError::Reverse)?;
        if !removed_reverse {
            return Err(BidirectionalLaraError::Reverse(
                LaraOperationError::DirectedRemoveOrientationMismatch,
            ));
        }
        Ok(Some(edge))
    }

    /// Inserts an undirected edge on forward out-adjacency only (`u → v` and `v → u`).
    ///
    /// Reverse orientation stores no undirected records; use [`Self::undirected_edges`]
    /// at each endpoint. Directed edges still use forward + reverse.
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
            self.forward
                .insert_edge(u, edge.with_neighbor_vid(u))
                .map_err(BidirectionalLaraError::Forward)?;
        } else {
            self.forward
                .insert_edge(u, edge.with_neighbor_vid(v))
                .map_err(BidirectionalLaraError::Forward)?;
            self.forward
                .insert_edge(v, edge.with_neighbor_vid(u))
                .map_err(BidirectionalLaraError::Forward)?;
        }
        Ok(())
    }

    /// Removes one undirected edge record without preserving adjacency order.
    ///
    /// Returns `true` when at least one materialized direction was present.
    pub fn remove_undirected(
        &self,
        u: VertexId,
        v: VertexId,
        edge: E,
    ) -> Result<bool, BidirectionalLaraError>
    where
        E: CsrEdgeUndirected + PartialEq + CsrEdgeTombstone,
    {
        self.ensure_vertex(u)?;
        self.ensure_vertex(v)?;
        let edge = edge.with_undirected(true);

        if u == v {
            return Ok(self
                .remove_forward_record_unchecked(u, u, edge.with_neighbor_vid(u))?
                .is_some());
        }

        let uv = self.remove_forward_record_unchecked(u, v, edge.with_neighbor_vid(v))?;
        let vu = self.remove_forward_record_unchecked(v, u, edge.with_neighbor_vid(u))?;
        Ok(uv.is_some() || vu.is_some())
    }

    /// Removes the first undirected edge accepted by `matches`.
    ///
    /// The predicate is evaluated against the `u -> v` forward record after the
    /// undirected flag and neighbor id are checked.
    pub fn remove_undirected_matching<F>(
        &self,
        u: VertexId,
        v: VertexId,
        mut matches: F,
    ) -> Result<Option<E>, BidirectionalLaraError>
    where
        E: CsrEdgeUndirected + PartialEq + CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(u)?;
        self.ensure_vertex(v)?;

        let removed = self.remove_forward_matching_unchecked(u, v, |edge| {
            edge.neighbor_vid() == v
                && <E as UndirectedEdgeFlag>::marked_undirected(edge)
                && matches(edge)
        })?;
        let Some(edge) = removed else {
            return Ok(None);
        };

        if u != v {
            let opposite = self.remove_forward_record_unchecked(v, u, edge.with_neighbor_vid(u))?;
            if opposite.is_none() {
                return Err(BidirectionalLaraError::Forward(
                    LaraOperationError::UndirectedRemoveOrientationMismatch,
                ));
            }
        }
        Ok(Some(edge))
    }

    fn remove_forward_record_unchecked(
        &self,
        src: VertexId,
        dst: VertexId,
        edge: E,
    ) -> Result<Option<E>, BidirectionalLaraError>
    where
        E: PartialEq + CsrEdgeTombstone,
    {
        self.remove_forward_matching_unchecked(src, dst, |candidate| *candidate == edge)
    }

    fn remove_forward_matching_unchecked<F>(
        &self,
        src: VertexId,
        dst: VertexId,
        mut matches: F,
    ) -> Result<Option<E>, BidirectionalLaraError>
    where
        E: PartialEq + CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        self.forward
            .remove_edge_matching(src, |edge| edge.neighbor_vid() == dst && matches(edge))
            .map_err(BidirectionalLaraError::Forward)
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
        let len = self.forward.vertices().len();
        if u32::from(vid) >= len {
            return Err(BidirectionalLaraError::VertexOutOfRange {
                vid,
                len: VertexCount(len),
            });
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
        test_support::{LabelledTestEdge, TestEdge, UndirectedTestEdge, bidirectional_test_graph},
    };

    #[test]
    fn bidirectional_directed_insert_updates_forward_and_reverse() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4, 8]);

        graph
            .insert_directed(VertexId::from(0), VertexId::from(2), TestEdge(2))
            .unwrap();

        assert_eq!(
            graph.directed_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(2)]
        );
        assert_eq!(
            graph.directed_out_edges(VertexId::from(2)).unwrap(),
            Vec::new()
        );
        assert_eq!(
            graph.directed_in_edges(VertexId::from(2)).unwrap(),
            vec![TestEdge(0)]
        );
        assert_eq!(
            graph.directed_in_edges(VertexId::from(0)).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn bidirectional_init_creates_empty_graph_when_memory_is_empty() {
        let graph = BidirectionalLaraGraph::<TestEdge, Vertex, _>::init(
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            8,
            2,
            0,
        )
        .unwrap();

        assert_eq!(graph.vertex_count(), VertexCount(0));
        assert_eq!(graph.forward().edges().header().elem_capacity, 8);
        assert_eq!(graph.reverse().edges().header().segment_size, 2);
    }

    #[test]
    fn bidirectional_directed_remove_updates_forward_and_reverse() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4, 8]);

        graph
            .insert_directed(VertexId::from(0), VertexId::from(2), TestEdge(2))
            .unwrap();

        assert!(
            graph
                .remove_directed(VertexId::from(0), VertexId::from(2), TestEdge(2))
                .unwrap()
        );

        assert_eq!(
            graph.directed_out_edges(VertexId::from(0)).unwrap(),
            Vec::new()
        );
        assert_eq!(
            graph.directed_in_edges(VertexId::from(2)).unwrap(),
            Vec::new()
        );
        assert!(
            !graph
                .remove_directed(VertexId::from(0), VertexId::from(2), TestEdge(2))
                .unwrap()
        );
    }

    #[test]
    fn bidirectional_directed_parallel_edges_remove_by_full_record() {
        let graph = bidirectional_test_graph::<LabelledTestEdge>(&[0, 4]);
        let red = LabelledTestEdge::new(1, 10);
        let blue = LabelledTestEdge::new(1, 20);

        graph
            .insert_directed(VertexId::from(0), VertexId::from(1), red)
            .unwrap();
        graph
            .insert_directed(VertexId::from(0), VertexId::from(1), blue)
            .unwrap();

        assert!(
            graph
                .remove_directed(VertexId::from(0), VertexId::from(1), blue)
                .unwrap()
        );
        assert_eq!(
            graph.directed_out_edges(VertexId::from(0)).unwrap(),
            vec![red]
        );
        assert_eq!(
            graph.directed_in_edges(VertexId::from(1)).unwrap(),
            vec![red.with_neighbor_vid(VertexId::from(0))]
        );
    }

    #[test]
    fn bidirectional_directed_parallel_edges_remove_by_predicate() {
        let graph = bidirectional_test_graph::<LabelledTestEdge>(&[0, 4]);
        let red = LabelledTestEdge::new(1, 10);
        let blue = LabelledTestEdge::new(1, 20);

        graph
            .insert_directed(VertexId::from(0), VertexId::from(1), red)
            .unwrap();
        graph
            .insert_directed(VertexId::from(0), VertexId::from(1), blue)
            .unwrap();

        let removed = graph
            .remove_directed_matching(VertexId::from(0), VertexId::from(1), |edge| {
                edge.label == 10
            })
            .unwrap();
        assert_eq!(removed, Some(red));
        assert_eq!(
            graph.directed_out_edges(VertexId::from(0)).unwrap(),
            vec![blue]
        );
        assert_eq!(
            graph.directed_in_edges(VertexId::from(1)).unwrap(),
            vec![blue.with_neighbor_vid(VertexId::from(0))]
        );
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
        assert_eq!(
            graph.directed_out_edges(VertexId::from(0)).unwrap(),
            Vec::new()
        );
        assert_eq!(
            graph.directed_in_edges(VertexId::from(1)).unwrap(),
            Vec::new()
        );
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
        assert_eq!(
            graph.directed_out_edges(VertexId::from(0)).unwrap(),
            Vec::new()
        );
        assert_eq!(
            graph.directed_in_edges(VertexId::from(1)).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn bidirectional_undirected_insert_materializes_forward_out_at_both_endpoints() {
        let graph = bidirectional_test_graph::<UndirectedTestEdge>(&[0, 4, 8]);

        graph
            .insert_undirected(
                VertexId::from(0),
                VertexId::from(2),
                UndirectedTestEdge::new(2),
            )
            .unwrap();

        let uv = UndirectedTestEdge {
            neighbor: 2,
            undirected: true,
        };
        let vu = UndirectedTestEdge {
            neighbor: 0,
            undirected: true,
        };
        assert_eq!(graph.undirected_edges(VertexId::from(0)).unwrap(), vec![uv]);
        assert_eq!(graph.undirected_edges(VertexId::from(2)).unwrap(), vec![vu]);
        assert_eq!(
            graph.directed_in_edges(VertexId::from(0)).unwrap(),
            Vec::new()
        );
        assert_eq!(
            graph.directed_in_edges(VertexId::from(2)).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn bidirectional_undirected_self_loop_stores_one_forward_out_record() {
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
        assert_eq!(
            graph.undirected_edges(VertexId::from(1)).unwrap(),
            vec![loop_edge]
        );
        assert_eq!(
            graph.directed_in_edges(VertexId::from(1)).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn bidirectional_reopen_preserves_forward_and_reverse_stores() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4, 8]);
        graph
            .insert_directed(VertexId::from(0), VertexId::from(2), TestEdge(2))
            .unwrap();

        let (fv, fc, fe, fl, fs, ff, ffs, rv, rc, re, rl, rs, rf, rfs) = graph.into_memories();
        let reopened = BidirectionalLaraGraph::<TestEdge, Vertex, _>::init(
            fv, fc, fe, fl, fs, ff, ffs, rv, rc, re, rl, rs, rf, rfs, 16, 2, 0,
        )
        .unwrap();

        assert_eq!(
            reopened.directed_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(2)]
        );
        assert_eq!(
            reopened.directed_in_edges(VertexId::from(2)).unwrap(),
            vec![TestEdge(0)]
        );
    }

    /// Upgrade-boundary corruption guard for the forward+reverse store pair:
    /// build a dense directed graph (parallel labelled edges per pair) that forces
    /// relocation in both orientations, delete a subset to exercise reverse-sync
    /// under tombstones, reopen the stable memories, and require the forward
    /// out-adjacency and reverse in-adjacency to stay mutually consistent and
    /// match the model across the boundary and through further mutation.
    #[test]
    fn bidirectional_heavy_relocation_survives_reopen_consistently() {
        use std::collections::BTreeSet;

        const VERTICES: u32 = 4;
        const PARALLEL: u32 = 12;
        const CAP: u64 = 128;
        const SEG: u32 = 32;

        fn build(
            mems: &[crate::VectorMemory; 14],
        ) -> crate::test_support::TestBidirectionalLaraGraph<LabelledTestEdge> {
            BidirectionalLaraGraph::new(
                mems[0].clone(),
                mems[1].clone(),
                mems[2].clone(),
                mems[3].clone(),
                mems[4].clone(),
                mems[5].clone(),
                mems[6].clone(),
                mems[7].clone(),
                mems[8].clone(),
                mems[9].clone(),
                mems[10].clone(),
                mems[11].clone(),
                mems[12].clone(),
                mems[13].clone(),
                CAP,
                SEG,
                0,
            )
            .unwrap()
        }
        fn reopen(
            mems: &[crate::VectorMemory; 14],
        ) -> crate::test_support::TestBidirectionalLaraGraph<LabelledTestEdge> {
            BidirectionalLaraGraph::init(
                mems[0].clone(),
                mems[1].clone(),
                mems[2].clone(),
                mems[3].clone(),
                mems[4].clone(),
                mems[5].clone(),
                mems[6].clone(),
                mems[7].clone(),
                mems[8].clone(),
                mems[9].clone(),
                mems[10].clone(),
                mems[11].clone(),
                mems[12].clone(),
                mems[13].clone(),
                CAP,
                SEG,
                0,
            )
            .unwrap()
        }

        let mems: [crate::VectorMemory; 14] =
            std::array::from_fn(|_| crate::test_support::vector_memory());
        let graph = build(&mems);
        for _ in 0..VERTICES {
            graph
                .push_vertex(Vertex::from_parts(0, 0, 0, -1, false))
                .unwrap();
        }

        // Model: (neighbor, label) sets per vertex for both orientations.
        let mut out: Vec<BTreeSet<(u32, u32)>> = vec![BTreeSet::new(); VERTICES as usize];
        let mut inc: Vec<BTreeSet<(u32, u32)>> = vec![BTreeSet::new(); VERTICES as usize];
        for src in 0..VERTICES {
            for dst in 0..VERTICES {
                if src == dst {
                    continue;
                }
                for l in 0..PARALLEL {
                    let label = dst * 100 + l;
                    graph
                        .insert_directed(
                            VertexId::from(src),
                            VertexId::from(dst),
                            LabelledTestEdge::new(dst, label),
                        )
                        .unwrap();
                    out[src as usize].insert((dst, label));
                    inc[dst as usize].insert((src, label));
                }
            }
        }

        // Delete the highest label of each ordered pair: exercises tombstones and
        // forward/reverse co-deletion before the boundary.
        for src in 0..VERTICES {
            for dst in 0..VERTICES {
                if src == dst {
                    continue;
                }
                let label = dst * 100 + (PARALLEL - 1);
                assert!(
                    graph
                        .remove_directed(
                            VertexId::from(src),
                            VertexId::from(dst),
                            LabelledTestEdge::new(dst, label),
                        )
                        .unwrap()
                );
                out[src as usize].remove(&(dst, label));
                inc[dst as usize].remove(&(src, label));
            }
        }

        let check = |g: &crate::test_support::TestBidirectionalLaraGraph<LabelledTestEdge>,
                     out: &[BTreeSet<(u32, u32)>],
                     inc: &[BTreeSet<(u32, u32)>],
                     phase: &str| {
            for v in 0..VERTICES {
                let got_out: BTreeSet<(u32, u32)> = g
                    .directed_out_edges(VertexId::from(v))
                    .unwrap()
                    .into_iter()
                    .map(|e| (e.neighbor, e.label))
                    .collect();
                assert_eq!(
                    got_out, out[v as usize],
                    "{phase}: vertex {v} out-adjacency diverged"
                );
                let got_in: BTreeSet<(u32, u32)> = g
                    .directed_in_edges(VertexId::from(v))
                    .unwrap()
                    .into_iter()
                    .map(|e| (e.neighbor, e.label))
                    .collect();
                assert_eq!(
                    got_in, inc[v as usize],
                    "{phase}: vertex {v} in-adjacency diverged"
                );
            }
            // Forward/reverse must describe exactly the same edge multiset.
            let fwd: BTreeSet<(u32, u32, u32)> = (0..VERTICES)
                .flat_map(|s| {
                    g.directed_out_edges(VertexId::from(s))
                        .unwrap()
                        .into_iter()
                        .map(move |e| (s, e.neighbor, e.label))
                })
                .collect();
            let rev: BTreeSet<(u32, u32, u32)> = (0..VERTICES)
                .flat_map(|d| {
                    g.directed_in_edges(VertexId::from(d))
                        .unwrap()
                        .into_iter()
                        .map(move |e| (e.neighbor, d, e.label))
                })
                .collect();
            assert_eq!(fwd, rev, "{phase}: forward and reverse stores disagree");
        };
        check(&graph, &out, &inc, "pre-reopen");

        drop(graph);
        let reopened = reopen(&mems);
        check(&reopened, &out, &inc, "post-reopen");

        // Re-insert the deleted labels after reopen: must reuse freed space and
        // keep both orientations consistent.
        for src in 0..VERTICES {
            for dst in 0..VERTICES {
                if src == dst {
                    continue;
                }
                let label = dst * 100 + (PARALLEL - 1);
                reopened
                    .insert_directed(
                        VertexId::from(src),
                        VertexId::from(dst),
                        LabelledTestEdge::new(dst, label),
                    )
                    .unwrap();
                out[src as usize].insert((dst, label));
                inc[dst as usize].insert((src, label));
            }
        }
        check(&reopened, &out, &inc, "post-reopen-mutation");
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
