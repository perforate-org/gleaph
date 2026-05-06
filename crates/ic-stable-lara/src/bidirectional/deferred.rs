//! Deferred-maintenance bidirectional LARA graph wrapper.

use crate::{
    GrowFailed, VertexCount, VertexId,
    bidirectional::UndirectedEdgeFlag,
    lara::{
        edge::counts::EdgePmaCountsStride,
        maintenance::{
            DeferredConfig, DeferredError, DeferredInitError, DeferredLaraGraph, MaintenanceBudget,
            MaintenanceReport, MaintenanceWorkReport,
        },
    },
    traits::{CsrEdge, CsrEdgeUndirected, LaraVertex},
};
use ic_stable_structures::Memory;
use std::fmt;

#[cfg(feature = "canbench")]
mod bench;

/// Maintenance report for a deferred bidirectional graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BidirectionalMaintenanceReport {
    /// Work performed on the forward orientation.
    pub forward: MaintenanceWorkReport,
    /// Work performed on the reverse orientation.
    pub reverse: MaintenanceWorkReport,
    /// Instruction-counter value observed at the end of the run.
    pub instructions_used: u64,
    /// Whether the instruction budget stopped the run.
    pub instruction_budget_exhausted: bool,
}

impl BidirectionalMaintenanceReport {
    /// Returns total processed segments across both orientations.
    pub fn processed_segments(self) -> u32 {
        self.forward
            .processed_segments
            .saturating_add(self.reverse.processed_segments)
    }

    /// Returns total remaining queued segments across both orientations.
    pub fn remaining_queue_len(self) -> u64 {
        self.forward
            .remaining_queue_len
            .saturating_add(self.reverse.remaining_queue_len)
    }

    fn add_forward_step(&mut self, step: MaintenanceWorkReport) {
        add_step_report(&mut self.forward, step);
    }

    fn add_reverse_step(&mut self, step: MaintenanceWorkReport) {
        add_step_report(&mut self.reverse, step);
    }
}

fn add_step_report(total: &mut MaintenanceWorkReport, step: MaintenanceWorkReport) {
    total.processed_segments = total
        .processed_segments
        .saturating_add(step.processed_segments);
    total.rebalanced_segments = total
        .rebalanced_segments
        .saturating_add(step.rebalanced_segments);
    total.resized |= step.resized;
    total.remaining_queue_len = step.remaining_queue_len;
}

#[inline]
fn current_instruction_counter() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        ic_cdk::api::instruction_counter()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

/// Errors returned by deferred bidirectional graph operations.
#[derive(Debug)]
pub enum DeferredBidirectionalLaraError {
    /// Forward store operation failed.
    Forward(&'static str),
    /// Reverse store operation failed.
    Reverse(&'static str),
    /// Forward deferred graph operation failed.
    ForwardDeferred(DeferredError),
    /// Reverse deferred graph operation failed.
    ReverseDeferred(DeferredError),
    /// Forward graph initialization failed.
    ForwardInit(DeferredInitError),
    /// Reverse graph initialization failed.
    ReverseInit(DeferredInitError),
    /// Forward vertex append failed.
    ForwardGrow(GrowFailed),
    /// Reverse vertex append failed.
    ReverseGrow(GrowFailed),
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

impl fmt::Display for DeferredBidirectionalLaraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forward(e) => write!(f, "forward store: {e}"),
            Self::Reverse(e) => write!(f, "reverse store: {e}"),
            Self::ForwardDeferred(e) => write!(f, "forward deferred operation failed: {e}"),
            Self::ReverseDeferred(e) => write!(f, "reverse deferred operation failed: {e}"),
            Self::ForwardInit(e) => write!(f, "forward init failed: {e}"),
            Self::ReverseInit(e) => write!(f, "reverse init failed: {e}"),
            Self::ForwardGrow(e) => write!(f, "forward vertex append failed: {e}"),
            Self::ReverseGrow(e) => write!(f, "reverse vertex append failed: {e}"),
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
                "directed insert: edge is marked undirected; use insert_undirected_deferred"
            ),
        }
    }
}

impl std::error::Error for DeferredBidirectionalLaraError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ForwardDeferred(e) | Self::ReverseDeferred(e) => Some(e),
            Self::ForwardInit(e) | Self::ReverseInit(e) => Some(e),
            Self::ForwardGrow(e) | Self::ReverseGrow(e) => Some(e),
            _ => None,
        }
    }
}

/// Bidirectional LARA graph whose two orientations use deferred maintenance.
pub struct DeferredBidirectionalLaraGraph<E, V, M>
where
    E: CsrEdge + EdgePmaCountsStride,
    V: LaraVertex,
    M: Memory,
{
    forward: DeferredLaraGraph<E, V, M>,
    reverse: DeferredLaraGraph<E, V, M>,
}

/// Convenience alias for [`DeferredBidirectionalLaraGraph`].
pub type DeferredBidirectionalLara<E, V, M> = DeferredBidirectionalLaraGraph<E, V, M>;

impl<E, V, M> DeferredBidirectionalLaraGraph<E, V, M>
where
    E: CsrEdge + EdgePmaCountsStride,
    V: LaraVertex,
    M: Memory,
{
    /// Creates fresh forward and reverse deferred LARA stores.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        forward_vertices: M,
        forward_counts: M,
        forward_edges: M,
        forward_log: M,
        forward_span_meta: M,
        forward_free_spans: M,
        forward_free_span_by_start: M,
        forward_maintenance_queue: M,
        forward_dirty_segments: M,
        reverse_vertices: M,
        reverse_counts: M,
        reverse_edges: M,
        reverse_log: M,
        reverse_span_meta: M,
        reverse_free_spans: M,
        reverse_free_span_by_start: M,
        reverse_maintenance_queue: M,
        reverse_dirty_segments: M,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
    ) -> Result<Self, DeferredBidirectionalLaraError> {
        Self::new_with_config(
            forward_vertices,
            forward_counts,
            forward_edges,
            forward_log,
            forward_span_meta,
            forward_free_spans,
            forward_free_span_by_start,
            forward_maintenance_queue,
            forward_dirty_segments,
            reverse_vertices,
            reverse_counts,
            reverse_edges,
            reverse_log,
            reverse_span_meta,
            reverse_free_spans,
            reverse_free_span_by_start,
            reverse_maintenance_queue,
            reverse_dirty_segments,
            elem_capacity,
            segment_count,
            segment_size,
            DeferredConfig::default(),
        )
    }

    /// Creates fresh forward and reverse stores with custom maintenance thresholds.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_config(
        forward_vertices: M,
        forward_counts: M,
        forward_edges: M,
        forward_log: M,
        forward_span_meta: M,
        forward_free_spans: M,
        forward_free_span_by_start: M,
        forward_maintenance_queue: M,
        forward_dirty_segments: M,
        reverse_vertices: M,
        reverse_counts: M,
        reverse_edges: M,
        reverse_log: M,
        reverse_span_meta: M,
        reverse_free_spans: M,
        reverse_free_span_by_start: M,
        reverse_maintenance_queue: M,
        reverse_dirty_segments: M,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        config: DeferredConfig,
    ) -> Result<Self, DeferredBidirectionalLaraError> {
        let forward = DeferredLaraGraph::new_with_config(
            forward_vertices,
            forward_counts,
            forward_edges,
            forward_log,
            forward_span_meta,
            forward_free_spans,
            forward_free_span_by_start,
            forward_maintenance_queue,
            forward_dirty_segments,
            elem_capacity,
            segment_count,
            segment_size,
            config,
        )
        .map_err(DeferredBidirectionalLaraError::ForwardDeferred)?;
        let reverse = DeferredLaraGraph::new_with_config(
            reverse_vertices,
            reverse_counts,
            reverse_edges,
            reverse_log,
            reverse_span_meta,
            reverse_free_spans,
            reverse_free_span_by_start,
            reverse_maintenance_queue,
            reverse_dirty_segments,
            elem_capacity,
            segment_count,
            segment_size,
            config,
        )
        .map_err(DeferredBidirectionalLaraError::ReverseDeferred)?;
        Ok(Self { forward, reverse })
    }

    /// Reopens forward and reverse deferred LARA stores.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        forward_vertices: M,
        forward_counts: M,
        forward_edges: M,
        forward_log: M,
        forward_span_meta: M,
        forward_free_spans: M,
        forward_free_span_by_start: M,
        forward_maintenance_queue: M,
        forward_dirty_segments: M,
        reverse_vertices: M,
        reverse_counts: M,
        reverse_edges: M,
        reverse_log: M,
        reverse_span_meta: M,
        reverse_free_spans: M,
        reverse_free_span_by_start: M,
        reverse_maintenance_queue: M,
        reverse_dirty_segments: M,
    ) -> Result<Self, DeferredBidirectionalLaraError> {
        Self::init_with_config(
            forward_vertices,
            forward_counts,
            forward_edges,
            forward_log,
            forward_span_meta,
            forward_free_spans,
            forward_free_span_by_start,
            forward_maintenance_queue,
            forward_dirty_segments,
            reverse_vertices,
            reverse_counts,
            reverse_edges,
            reverse_log,
            reverse_span_meta,
            reverse_free_spans,
            reverse_free_span_by_start,
            reverse_maintenance_queue,
            reverse_dirty_segments,
            DeferredConfig::default(),
        )
    }

    /// Reopens forward and reverse stores with custom maintenance thresholds.
    #[allow(clippy::too_many_arguments)]
    pub fn init_with_config(
        forward_vertices: M,
        forward_counts: M,
        forward_edges: M,
        forward_log: M,
        forward_span_meta: M,
        forward_free_spans: M,
        forward_free_span_by_start: M,
        forward_maintenance_queue: M,
        forward_dirty_segments: M,
        reverse_vertices: M,
        reverse_counts: M,
        reverse_edges: M,
        reverse_log: M,
        reverse_span_meta: M,
        reverse_free_spans: M,
        reverse_free_span_by_start: M,
        reverse_maintenance_queue: M,
        reverse_dirty_segments: M,
        config: DeferredConfig,
    ) -> Result<Self, DeferredBidirectionalLaraError> {
        let forward = DeferredLaraGraph::init_with_config(
            forward_vertices,
            forward_counts,
            forward_edges,
            forward_log,
            forward_span_meta,
            forward_free_spans,
            forward_free_span_by_start,
            forward_maintenance_queue,
            forward_dirty_segments,
            config,
        )
        .map_err(DeferredBidirectionalLaraError::ForwardInit)?;
        let reverse = DeferredLaraGraph::init_with_config(
            reverse_vertices,
            reverse_counts,
            reverse_edges,
            reverse_log,
            reverse_span_meta,
            reverse_free_spans,
            reverse_free_span_by_start,
            reverse_maintenance_queue,
            reverse_dirty_segments,
            config,
        )
        .map_err(DeferredBidirectionalLaraError::ReverseInit)?;
        let graph = Self { forward, reverse };
        graph.ensure_matching_vertex_counts()?;
        Ok(graph)
    }

    /// Returns the forward out-adjacency graph.
    pub fn forward(&self) -> &DeferredLaraGraph<E, V, M> {
        &self.forward
    }

    /// Returns the reverse in-adjacency graph.
    pub fn reverse(&self) -> &DeferredLaraGraph<E, V, M> {
        &self.reverse
    }

    /// Consumes the wrapper and returns all forward memories followed by all reverse memories.
    #[allow(clippy::type_complexity)]
    pub fn into_memories(self) -> (M, M, M, M, M, M, M, M, M, M, M, M, M, M, M, M, M, M) {
        let (fv, fc, fe, fl, fs, ff, ffs, fq, fd) = self.forward.into_memories();
        let (rv, rc, re, rl, rs, rf, rfs, rq, rd) = self.reverse.into_memories();
        (
            fv, fc, fe, fl, fs, ff, ffs, fq, fd, rv, rc, re, rl, rs, rf, rfs, rq, rd,
        )
    }

    /// Returns the number of vertices in both orientations.
    pub fn vertex_count(&self) -> VertexCount {
        VertexCount(self.forward.graph().vertices().len())
    }

    /// Appends the same vertex row to the forward and reverse stores.
    pub fn push_vertex(&self, vertex: V) -> Result<VertexId, DeferredBidirectionalLaraError> {
        let id = self
            .forward
            .push_vertex(vertex)
            .map_err(DeferredBidirectionalLaraError::ForwardGrow)?;
        self.reverse
            .push_vertex(vertex)
            .map_err(DeferredBidirectionalLaraError::ReverseGrow)?;
        self.ensure_matching_vertex_counts()?;
        Ok(id)
    }

    /// Collects outgoing edges from the forward store.
    pub fn out_edges(&self, src: VertexId) -> Result<Vec<E>, DeferredBidirectionalLaraError> {
        self.ensure_vertex(src)?;
        self.forward
            .collect_out_edges(src)
            .map_err(DeferredBidirectionalLaraError::Forward)
    }

    /// Collects incoming edges from the reverse store.
    pub fn in_edges(&self, dst: VertexId) -> Result<Vec<E>, DeferredBidirectionalLaraError> {
        self.ensure_vertex(dst)?;
        self.reverse
            .collect_out_edges(dst)
            .map_err(DeferredBidirectionalLaraError::Reverse)
    }

    /// Inserts a directed edge and defers maintenance in each orientation.
    pub fn insert_directed_deferred(
        &self,
        src: VertexId,
        dst: VertexId,
        edge: E,
    ) -> Result<(), DeferredBidirectionalLaraError> {
        self.ensure_vertex(src)?;
        self.ensure_vertex(dst)?;
        if edge.neighbor_vid() != dst {
            return Err(DeferredBidirectionalLaraError::NeighborMismatch {
                expected: dst,
                actual: edge.neighbor_vid(),
            });
        }
        if <E as UndirectedEdgeFlag>::marked_undirected(&edge) {
            return Err(DeferredBidirectionalLaraError::UndirectedEdgeInDirectedInsert);
        }

        self.forward
            .insert_edge_deferred(src, edge)
            .map_err(DeferredBidirectionalLaraError::ForwardDeferred)?;
        self.reverse
            .insert_edge_deferred(dst, edge.with_neighbor_vid(src))
            .map_err(DeferredBidirectionalLaraError::ReverseDeferred)?;
        Ok(())
    }

    /// Removes one directed edge record without preserving adjacency order.
    ///
    /// `edge.neighbor_vid()` must equal `dst`. When parallel edges connect the
    /// same vertices, the full edge record selects which one is removed. Both
    /// orientations are updated.
    pub fn remove_directed_deferred(
        &self,
        src: VertexId,
        dst: VertexId,
        edge: E,
    ) -> Result<bool, DeferredBidirectionalLaraError>
    where
        E: PartialEq,
    {
        self.ensure_vertex(src)?;
        self.ensure_vertex(dst)?;
        if edge.neighbor_vid() != dst {
            return Err(DeferredBidirectionalLaraError::NeighborMismatch {
                expected: dst,
                actual: edge.neighbor_vid(),
            });
        }
        if <E as UndirectedEdgeFlag>::marked_undirected(&edge) {
            return Err(DeferredBidirectionalLaraError::UndirectedEdgeInDirectedInsert);
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
    pub fn remove_directed_matching_deferred<F>(
        &self,
        src: VertexId,
        dst: VertexId,
        matches: F,
    ) -> Result<Option<E>, DeferredBidirectionalLaraError>
    where
        E: PartialEq,
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
    ) -> Result<Option<E>, DeferredBidirectionalLaraError>
    where
        E: PartialEq,
    {
        self.remove_directed_matching_unchecked(src, dst, |candidate| *candidate == edge)
    }

    fn remove_directed_matching_unchecked<F>(
        &self,
        src: VertexId,
        dst: VertexId,
        mut matches: F,
    ) -> Result<Option<E>, DeferredBidirectionalLaraError>
    where
        E: PartialEq,
        F: FnMut(&E) -> bool,
    {
        let removed_forward = self
            .forward
            .remove_edge_matching_deferred(src, |edge| edge.neighbor_vid() == dst && matches(edge))
            .map_err(DeferredBidirectionalLaraError::ForwardDeferred)?;
        let Some(edge) = removed_forward else {
            return Ok(None);
        };
        let removed_reverse = self
            .reverse
            .remove_edge_deferred(dst, edge.with_neighbor_vid(src))
            .map_err(DeferredBidirectionalLaraError::ReverseDeferred)?;
        if !removed_reverse {
            return Err(DeferredBidirectionalLaraError::Reverse(
                "directed remove orientation mismatch",
            ));
        }
        Ok(Some(edge))
    }

    /// Inserts an undirected edge and defers maintenance in each orientation.
    pub fn insert_undirected_deferred(
        &self,
        u: VertexId,
        v: VertexId,
        edge: E,
    ) -> Result<(), DeferredBidirectionalLaraError>
    where
        E: CsrEdgeUndirected,
    {
        self.ensure_vertex(u)?;
        self.ensure_vertex(v)?;
        let edge = edge.with_undirected(true);

        if u == v {
            let loop_edge = edge.with_neighbor_vid(u);
            self.forward
                .insert_edge_deferred(u, loop_edge)
                .map_err(DeferredBidirectionalLaraError::ForwardDeferred)?;
            self.reverse
                .insert_edge_deferred(u, loop_edge)
                .map_err(DeferredBidirectionalLaraError::ReverseDeferred)?;
            return Ok(());
        }

        self.forward
            .insert_edge_deferred(u, edge.with_neighbor_vid(v))
            .map_err(DeferredBidirectionalLaraError::ForwardDeferred)?;
        self.forward
            .insert_edge_deferred(v, edge.with_neighbor_vid(u))
            .map_err(DeferredBidirectionalLaraError::ForwardDeferred)?;
        self.reverse
            .insert_edge_deferred(v, edge.with_neighbor_vid(u))
            .map_err(DeferredBidirectionalLaraError::ReverseDeferred)?;
        self.reverse
            .insert_edge_deferred(u, edge.with_neighbor_vid(v))
            .map_err(DeferredBidirectionalLaraError::ReverseDeferred)?;
        Ok(())
    }

    /// Removes an undirected edge without preserving adjacency order.
    ///
    /// Returns `true` when at least one materialized direction was present.
    pub fn remove_undirected_deferred(
        &self,
        u: VertexId,
        v: VertexId,
        edge: E,
    ) -> Result<bool, DeferredBidirectionalLaraError>
    where
        E: CsrEdgeUndirected + PartialEq,
    {
        self.ensure_vertex(u)?;
        self.ensure_vertex(v)?;
        let edge = edge.with_undirected(true);

        if u == v {
            return Ok(self
                .remove_directed_record_unchecked(u, u, edge.with_neighbor_vid(u))?
                .is_some());
        }

        let uv = self.remove_directed_record_unchecked(u, v, edge.with_neighbor_vid(v))?;
        let vu = self.remove_directed_record_unchecked(v, u, edge.with_neighbor_vid(u))?;
        Ok(uv.is_some() || vu.is_some())
    }

    /// Removes the first undirected edge accepted by `matches`.
    ///
    /// The predicate is evaluated against the `u -> v` forward record after the
    /// undirected flag and neighbor id are checked.
    pub fn remove_undirected_matching_deferred<F>(
        &self,
        u: VertexId,
        v: VertexId,
        mut matches: F,
    ) -> Result<Option<E>, DeferredBidirectionalLaraError>
    where
        E: CsrEdgeUndirected + PartialEq,
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(u)?;
        self.ensure_vertex(v)?;

        let removed = self.remove_directed_matching_unchecked(u, v, |edge| {
            edge.neighbor_vid() == v
                && <E as UndirectedEdgeFlag>::marked_undirected(edge)
                && matches(edge)
        })?;
        let Some(edge) = removed else {
            return Ok(None);
        };

        if u != v {
            let opposite =
                self.remove_directed_record_unchecked(v, u, edge.with_neighbor_vid(u))?;
            if opposite.is_none() {
                return Err(DeferredBidirectionalLaraError::Forward(
                    "undirected remove orientation mismatch",
                ));
            }
        }
        Ok(Some(edge))
    }

    /// Runs maintenance only for the forward orientation.
    pub fn maintenance_forward(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, DeferredBidirectionalLaraError> {
        self.forward
            .maintenance(budget)
            .map_err(DeferredBidirectionalLaraError::ForwardDeferred)
    }

    /// Runs maintenance only for the reverse orientation.
    pub fn maintenance_reverse(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, DeferredBidirectionalLaraError> {
        self.reverse
            .maintenance(budget)
            .map_err(DeferredBidirectionalLaraError::ReverseDeferred)
    }

    /// Runs budgeted maintenance across both orientations.
    pub fn maintenance(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<BidirectionalMaintenanceReport, DeferredBidirectionalLaraError> {
        let mut report = BidirectionalMaintenanceReport::default();
        let mut forward_len = self.forward.maintenance_queue().len();
        let mut reverse_len = self.reverse.maintenance_queue().len();

        while budget
            .max_segments
            .is_none_or(|max_segments| report.processed_segments() < max_segments)
        {
            let instructions_used = current_instruction_counter();
            if budget.max_instructions > 0 && instructions_used >= budget.max_instructions {
                report.instruction_budget_exhausted = true;
                break;
            }

            if forward_len == 0 && reverse_len == 0 {
                break;
            }

            if forward_len >= reverse_len {
                if let Some(step) = self
                    .forward
                    .maintenance_step()
                    .map_err(DeferredBidirectionalLaraError::ForwardDeferred)?
                {
                    report.add_forward_step(step);
                    forward_len = forward_len.saturating_sub(1);
                }
            } else if let Some(step) = self
                .reverse
                .maintenance_step()
                .map_err(DeferredBidirectionalLaraError::ReverseDeferred)?
            {
                report.add_reverse_step(step);
                reverse_len = reverse_len.saturating_sub(1);
            }
        }

        let instructions_used = current_instruction_counter();
        let exhausted = budget.max_instructions > 0 && instructions_used >= budget.max_instructions;
        report.instructions_used = instructions_used;
        report.instruction_budget_exhausted |= exhausted;
        report.forward.remaining_queue_len = forward_len;
        report.reverse.remaining_queue_len = reverse_len;
        Ok(report)
    }

    fn ensure_matching_vertex_counts(&self) -> Result<(), DeferredBidirectionalLaraError> {
        let forward = VertexCount(self.forward.graph().vertices().len());
        let reverse = VertexCount(self.reverse.graph().vertices().len());
        if forward != reverse {
            return Err(DeferredBidirectionalLaraError::VertexCountMismatch { forward, reverse });
        }
        Ok(())
    }

    fn ensure_vertex(&self, vid: VertexId) -> Result<(), DeferredBidirectionalLaraError> {
        let len = self.vertex_count();
        if u64::from(u32::from(vid)) >= u64::from(len) {
            return Err(DeferredBidirectionalLaraError::VertexOutOfRange { vid, len });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::CsrEdgeUndirected;
    use crate::{
        SegmentId, Vertex,
        test_support::{
            TestEdge, UndirectedTestEdge, deferred_bidirectional_test_graph, vector_memory,
        },
    };

    #[test]
    fn deferred_directed_insert_updates_forward_and_reverse() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(8, 2, 2, &[0, 2, 4]);

        graph
            .insert_directed_deferred(VertexId::from(0), VertexId::from(2), TestEdge(2))
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
    fn deferred_directed_insert_rejects_neighbor_mismatch_before_writes() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(8, 2, 2, &[0, 2]);

        let err = graph
            .insert_directed_deferred(VertexId::from(0), VertexId::from(1), TestEdge(0))
            .unwrap_err();

        assert!(matches!(
            err,
            DeferredBidirectionalLaraError::NeighborMismatch {
                expected,
                actual
            } if expected == VertexId::from(1) && actual == VertexId::from(0)
        ));
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap(), Vec::new());
        assert_eq!(graph.in_edges(VertexId::from(1)).unwrap(), Vec::new());
    }

    #[test]
    fn deferred_directed_insert_rejects_undirected_edge() {
        let graph = deferred_bidirectional_test_graph::<UndirectedTestEdge>(8, 2, 2, &[0, 2]);
        let edge = UndirectedTestEdge::new(1).with_undirected(true);

        let err = graph
            .insert_directed_deferred(VertexId::from(0), VertexId::from(1), edge)
            .unwrap_err();

        assert!(matches!(
            err,
            DeferredBidirectionalLaraError::UndirectedEdgeInDirectedInsert
        ));
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap(), Vec::new());
        assert_eq!(graph.in_edges(VertexId::from(1)).unwrap(), Vec::new());
    }

    #[test]
    fn deferred_undirected_insert_materializes_symmetric_adjacency() {
        let graph = deferred_bidirectional_test_graph::<UndirectedTestEdge>(8, 2, 2, &[0, 2, 4]);

        graph
            .insert_undirected_deferred(
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
    fn deferred_undirected_self_loop_stores_one_loop_per_orientation() {
        let graph = deferred_bidirectional_test_graph::<UndirectedTestEdge>(8, 2, 2, &[0, 2]);

        graph
            .insert_undirected_deferred(
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
    fn deferred_bidirectional_reopen_preserves_stores_and_queues() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(8, 2, 2, &[0, 2, 4]);
        for _ in 0..3 {
            graph
                .insert_directed_deferred(VertexId::from(0), VertexId::from(2), TestEdge(2))
                .unwrap();
        }

        assert!(
            graph
                .forward()
                .maintenance_queue()
                .is_dirty(SegmentId::from(0))
        );
        assert_eq!(graph.reverse().maintenance_queue().len(), 1);

        let memories = graph.into_memories();
        let reopened = DeferredBidirectionalLaraGraph::<TestEdge, Vertex, _>::init(
            memories.0,
            memories.1,
            memories.2,
            memories.3,
            memories.4,
            memories.5,
            memories.6,
            memories.7,
            memories.8,
            memories.9,
            memories.10,
            memories.11,
            memories.12,
            memories.13,
            memories.14,
            memories.15,
            memories.16,
            memories.17,
        )
        .unwrap();

        assert_eq!(
            reopened.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(2), TestEdge(2), TestEdge(2)]
        );
        assert_eq!(
            reopened.in_edges(VertexId::from(2)).unwrap(),
            vec![TestEdge(0), TestEdge(0), TestEdge(0)]
        );
        assert!(
            reopened
                .forward()
                .maintenance_queue()
                .is_dirty(SegmentId::from(0))
        );
        assert_eq!(reopened.reverse().maintenance_queue().len(), 1);
    }

    #[test]
    fn deferred_bidirectional_maintenance_drains_orientations_independently() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(8, 2, 2, &[0, 2, 4]);
        for _ in 0..3 {
            graph
                .insert_directed_deferred(VertexId::from(0), VertexId::from(2), TestEdge(2))
                .unwrap();
        }

        let forward = graph
            .maintenance_forward(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
            })
            .unwrap();

        assert_eq!(forward.work.processed_segments, 1);
        assert!(
            !graph
                .forward()
                .maintenance_queue()
                .is_dirty(SegmentId::from(0))
        );
        assert_eq!(graph.reverse().maintenance_queue().len(), 1);

        let reverse = graph
            .maintenance_reverse(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
            })
            .unwrap();

        assert_eq!(reverse.work.processed_segments, 1);
        assert_eq!(graph.reverse().maintenance_queue().len(), 0);
    }

    #[test]
    fn deferred_bidirectional_combined_maintenance_respects_segment_cap() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(8, 2, 2, &[0, 2, 4]);
        for _ in 0..3 {
            graph
                .insert_directed_deferred(VertexId::from(0), VertexId::from(2), TestEdge(2))
                .unwrap();
        }

        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
            })
            .unwrap();

        assert_eq!(report.processed_segments(), 1);
        assert_eq!(report.forward.processed_segments, 1);
        assert_eq!(report.reverse.processed_segments, 0);
        assert_eq!(graph.reverse().maintenance_queue().len(), 1);
    }

    #[test]
    fn deferred_bidirectional_combined_maintenance_chooses_reverse_when_longer() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(16, 4, 2, &[0, 2, 4, 6, 8, 10]);
        graph
            .forward()
            .maintenance_queue()
            .mark_dirty(SegmentId::from(0))
            .unwrap();
        graph
            .reverse()
            .maintenance_queue()
            .mark_dirty(SegmentId::from(1))
            .unwrap();
        graph
            .reverse()
            .maintenance_queue()
            .mark_dirty(SegmentId::from(2))
            .unwrap();

        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
            })
            .unwrap();

        assert_eq!(report.forward.processed_segments, 0);
        assert_eq!(report.reverse.processed_segments, 1);
        assert_eq!(graph.forward().maintenance_queue().len(), 1);
        assert_eq!(graph.reverse().maintenance_queue().len(), 1);
    }

    #[test]
    fn deferred_bidirectional_combined_maintenance_chooses_forward_on_tie() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(16, 4, 2, &[0, 2, 4, 6]);
        graph
            .forward()
            .maintenance_queue()
            .mark_dirty(SegmentId::from(0))
            .unwrap();
        graph
            .reverse()
            .maintenance_queue()
            .mark_dirty(SegmentId::from(1))
            .unwrap();

        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
            })
            .unwrap();

        assert_eq!(report.forward.processed_segments, 1);
        assert_eq!(report.reverse.processed_segments, 0);
        assert_eq!(graph.forward().maintenance_queue().len(), 0);
        assert_eq!(graph.reverse().maintenance_queue().len(), 1);
    }

    #[test]
    fn deferred_bidirectional_combined_maintenance_rechecks_lengths_after_each_step() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(16, 4, 2, &[0, 2, 4, 6, 8, 10]);
        graph
            .forward()
            .maintenance_queue()
            .mark_dirty(SegmentId::from(0))
            .unwrap();
        graph
            .forward()
            .maintenance_queue()
            .mark_dirty(SegmentId::from(1))
            .unwrap();
        graph
            .reverse()
            .maintenance_queue()
            .mark_dirty(SegmentId::from(0))
            .unwrap();
        graph
            .reverse()
            .maintenance_queue()
            .mark_dirty(SegmentId::from(2))
            .unwrap();

        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(2),
            })
            .unwrap();

        assert_eq!(report.forward.processed_segments, 1);
        assert_eq!(report.reverse.processed_segments, 1);
        assert_eq!(graph.forward().maintenance_queue().len(), 1);
        assert_eq!(graph.reverse().maintenance_queue().len(), 1);
    }

    #[test]
    fn deferred_bidirectional_init_rejects_vertex_count_mismatch() {
        let forward = crate::DeferredLaraGraph::<TestEdge, Vertex, _>::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            8,
            2,
            2,
        )
        .unwrap();
        let reverse = crate::DeferredLaraGraph::<TestEdge, Vertex, _>::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            8,
            2,
            2,
        )
        .unwrap();
        forward
            .push_vertex(Vertex {
                base_slot_start: 0,
                degree: 0,
                capacity: 0,
                log_head: -1,
            })
            .unwrap();

        let (fv, fc, fe, fl, fs, ff, ffs, fq, fd) = forward.into_memories();
        let (rv, rc, re, rl, rs, rf, rfs, rq, rd) = reverse.into_memories();
        let err = match DeferredBidirectionalLaraGraph::<TestEdge, Vertex, _>::init(
            fv, fc, fe, fl, fs, ff, ffs, fq, fd, rv, rc, re, rl, rs, rf, rfs, rq, rd,
        ) {
            Ok(_) => panic!("vertex count mismatch was accepted"),
            Err(err) => err,
        };

        assert!(matches!(
            err,
            DeferredBidirectionalLaraError::VertexCountMismatch { .. }
        ));
    }
}
