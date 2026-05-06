//! Persistent maintenance worklist for deferred LARA rebalancing.
//!
//! The deque preserves processing order while the roaring bitmap provides O(1)
//! duplicate suppression for dirty leaf segments.

use crate::{
    GrowFailed as GraphGrowFailed, SegmentId, VertexId,
    lara::{InitError as GraphInitError, LaraGraph, MarkPriority},
    traits::{CsrEdge, LaraVertex},
};
use ic_stable_roaring::StableRoaringBitmap;
use ic_stable_structures::Memory;
use ic_stable_vec_deque::StableVecDeque;
use std::fmt;

/// Errors returned when reopening the persistent maintenance queue.
#[derive(Debug)]
pub enum InitError {
    /// The deque memory could not be reopened.
    Queue(ic_stable_vec_deque::InitError),
    /// The dirty-set bitmap could not be reopened.
    DirtySet(ic_stable_roaring::InitError),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Queue(e) => write!(f, "maintenance queue init failed: {e}"),
            Self::DirtySet(e) => write!(f, "dirty segment set init failed: {e}"),
        }
    }
}

impl std::error::Error for InitError {}

/// Errors returned when maintenance metadata cannot grow.
#[derive(Debug)]
pub enum GrowFailed {
    /// The deque memory could not grow.
    Queue(ic_stable_vec_deque::GrowFailed),
    /// The dirty-set bitmap memory could not grow.
    DirtySet(ic_stable_roaring::GrowFailed),
}

impl fmt::Display for GrowFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Queue(e) => write!(f, "maintenance queue grow failed: {e}"),
            Self::DirtySet(e) => write!(f, "dirty segment set grow failed: {e}"),
        }
    }
}

impl std::error::Error for GrowFailed {}

/// Result of marking one segment for deferred maintenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkResult {
    /// Segment that was marked.
    pub segment: SegmentId,
    /// `true` if this call inserted a new queue entry.
    pub inserted: bool,
}

/// Persistent FIFO worklist with duplicate suppression for dirty segments.
#[derive(Debug)]
pub struct MaintenanceQueue<M: Memory> {
    queue: StableVecDeque<SegmentId, M>,
    dirty: StableRoaringBitmap<M>,
}

impl<M: Memory> MaintenanceQueue<M> {
    /// Creates a fresh empty maintenance queue.
    pub fn new(queue_memory: M, dirty_memory: M) -> Result<Self, GrowFailed> {
        Ok(Self {
            queue: StableVecDeque::new(queue_memory).map_err(GrowFailed::Queue)?,
            dirty: StableRoaringBitmap::new(dirty_memory).map_err(GrowFailed::DirtySet)?,
        })
    }

    /// Reopens an existing maintenance queue.
    pub fn init(queue_memory: M, dirty_memory: M) -> Result<Self, InitError> {
        Ok(Self {
            queue: StableVecDeque::init(queue_memory).map_err(InitError::Queue)?,
            dirty: StableRoaringBitmap::init(dirty_memory).map_err(InitError::DirtySet)?,
        })
    }

    /// Consumes the queue and returns `(queue_memory, dirty_bitmap_memory)`.
    pub fn into_memories(self) -> (M, M) {
        (self.queue.into_memory(), self.dirty.into_memory())
    }

    /// Returns the number of queued segment ids.
    pub fn len(&self) -> u64 {
        self.queue.len()
    }

    /// Returns `true` when no queued segment ids remain.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns whether `segment` is currently marked dirty.
    pub fn is_dirty(&self, segment: SegmentId) -> bool {
        self.dirty.contains(u32::from(segment))
    }

    /// Marks `segment` dirty and appends it to the back of the queue if new.
    pub fn mark_dirty(&self, segment: SegmentId) -> Result<MarkResult, GrowFailed> {
        if self.is_dirty(segment) {
            return Ok(MarkResult {
                segment,
                inserted: false,
            });
        }
        self.dirty
            .insert(u32::from(segment))
            .map_err(GrowFailed::DirtySet)?;
        self.queue.push_back(&segment).map_err(GrowFailed::Queue)?;
        Ok(MarkResult {
            segment,
            inserted: true,
        })
    }

    /// Marks `segment` urgent and pushes it to the front of the queue if new.
    pub fn mark_urgent(&self, segment: SegmentId) -> Result<MarkResult, GrowFailed> {
        if self.is_dirty(segment) {
            return Ok(MarkResult {
                segment,
                inserted: false,
            });
        }
        self.dirty
            .insert(u32::from(segment))
            .map_err(GrowFailed::DirtySet)?;
        self.queue.push_front(&segment).map_err(GrowFailed::Queue)?;
        Ok(MarkResult {
            segment,
            inserted: true,
        })
    }

    /// Pops the next still-dirty segment and clears its dirty bit.
    pub fn pop_next(&self) -> Result<Option<SegmentId>, GrowFailed> {
        while let Some(segment) = self.queue.pop_front() {
            if !self.is_dirty(segment) {
                continue;
            }
            self.dirty
                .clear(u32::from(segment))
                .map_err(GrowFailed::DirtySet)?;
            return Ok(Some(segment));
        }
        Ok(None)
    }

    /// Clears the dirty bit for `segment` without removing queued duplicates.
    pub fn clear_dirty(&self, segment: SegmentId) -> Result<(), GrowFailed> {
        self.dirty
            .clear(u32::from(segment))
            .map_err(GrowFailed::DirtySet)
    }
}

/// Errors returned when reopening a deferred-maintenance graph.
#[derive(Debug)]
pub enum DeferredInitError {
    /// The underlying LARA graph could not be reopened.
    Graph(GraphInitError),
    /// Maintenance metadata could not be reopened.
    Maintenance(InitError),
    /// The supplied deferred-maintenance configuration is invalid.
    InvalidConfig(DeferredConfigError),
}

impl fmt::Display for DeferredInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(e) => write!(f, "LARA init failed: {e}"),
            Self::Maintenance(e) => write!(f, "maintenance init failed: {e}"),
            Self::InvalidConfig(e) => write!(f, "invalid deferred config: {e}"),
        }
    }
}

impl std::error::Error for DeferredInitError {}

/// Errors returned by deferred graph operations.
#[derive(Debug)]
pub enum DeferredError {
    /// The underlying LARA graph operation failed.
    Graph(&'static str),
    /// The underlying LARA graph could not grow memory.
    Grow(GraphGrowFailed),
    /// Maintenance metadata operation failed.
    Maintenance(GrowFailed),
    /// The supplied deferred-maintenance configuration is invalid.
    InvalidConfig(DeferredConfigError),
}

impl fmt::Display for DeferredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(e) => write!(f, "LARA operation failed: {e}"),
            Self::Grow(e) => write!(f, "LARA memory grow failed: {e}"),
            Self::Maintenance(e) => write!(f, "maintenance operation failed: {e}"),
            Self::InvalidConfig(e) => write!(f, "invalid deferred config: {e}"),
        }
    }
}

impl std::error::Error for DeferredError {}

/// Thresholds that control when deferred inserts enqueue maintenance work.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeferredConfig {
    /// Leaf density at or above which a segment is marked dirty after insert.
    pub leaf_dirty_density: f64,
    /// Per-segment log fill ratio at or above which a segment is marked urgent.
    pub log_urgent_ratio: f64,
}

impl Default for DeferredConfig {
    fn default() -> Self {
        Self {
            leaf_dirty_density: 0.85,
            log_urgent_ratio: 0.80,
        }
    }
}

impl DeferredConfig {
    fn validate(self) -> Result<Self, DeferredConfigError> {
        validate_ratio("leaf_dirty_density", self.leaf_dirty_density)?;
        validate_ratio("log_urgent_ratio", self.log_urgent_ratio)?;
        Ok(self)
    }
}

/// Invalid deferred-maintenance configuration value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeferredConfigError {
    field: &'static str,
    value: f64,
}

impl fmt::Display for DeferredConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} must be in 0.0..=1.0, got {}", self.field, self.value)
    }
}

impl std::error::Error for DeferredConfigError {}

fn validate_ratio(field: &'static str, value: f64) -> Result<(), DeferredConfigError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(DeferredConfigError { field, value })
    }
}

/// Budget for one deferred maintenance call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceBudget {
    /// Upper instruction-counter value allowed for the current call.
    ///
    /// On IC wasm builds this is compared with
    /// `ic_cdk::api::instruction_counter()`, which is scoped to the current
    /// message execution. Use `0` to disable instruction-based termination.
    pub max_instructions: u64,
    /// Optional hard cap on the number of segments processed in one call.
    ///
    /// This is useful for deterministic tests or fairness tuning. `None` means
    /// instruction budget and queue exhaustion are the only maintenance caps.
    pub max_segments: Option<u32>,
}

/// Work performed by one or more deferred maintenance steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceWorkReport {
    /// Number of queue entries consumed.
    pub processed_segments: u32,
    /// Number of segments that actually needed rebalancing.
    pub rebalanced_segments: u32,
    /// Whether any step expanded the edge slab.
    pub resized: bool,
    /// Queue length after the reported work.
    pub remaining_queue_len: u64,
}

/// Result of a budgeted deferred maintenance run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// Segment work performed by the run.
    pub work: MaintenanceWorkReport,
    /// Instruction-counter value observed at the end of the run.
    pub instructions_used: u64,
    /// Whether the instruction budget stopped the run.
    pub instruction_budget_exhausted: bool,
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

/// Single-orientation LARA graph with deferred maintenance.
pub struct DeferredLaraGraph<E, V, M>
where
    E: CsrEdge,
    V: LaraVertex,
    M: Memory,
{
    graph: LaraGraph<E, V, M>,
    maintenance: MaintenanceQueue<M>,
    config: DeferredConfig,
}

impl<E, V, M> DeferredLaraGraph<E, V, M>
where
    E: CsrEdge,
    V: LaraVertex,
    M: Memory,
{
    /// Creates a fresh deferred graph with default maintenance thresholds.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vertices: M,
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
        maintenance_queue: M,
        dirty_segments: M,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
    ) -> Result<Self, DeferredError> {
        Self::new_with_config(
            vertices,
            counts,
            edges,
            log,
            span_meta,
            free_spans,
            free_span_by_start,
            maintenance_queue,
            dirty_segments,
            elem_capacity,
            segment_count,
            segment_size,
            DeferredConfig::default(),
        )
    }

    /// Creates a fresh deferred graph with custom maintenance thresholds.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_config(
        vertices: M,
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
        maintenance_queue: M,
        dirty_segments: M,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        config: DeferredConfig,
    ) -> Result<Self, DeferredError> {
        let config = config.validate().map_err(DeferredError::InvalidConfig)?;
        Ok(Self {
            graph: LaraGraph::new(
                vertices,
                counts,
                edges,
                log,
                span_meta,
                free_spans,
                free_span_by_start,
                elem_capacity,
                segment_count,
                segment_size,
            )
            .map_err(DeferredError::Grow)?,
            maintenance: MaintenanceQueue::new(maintenance_queue, dirty_segments)
                .map_err(DeferredError::Maintenance)?,
            config,
        })
    }

    /// Reopens a deferred graph with default maintenance thresholds.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        vertices: M,
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
        maintenance_queue: M,
        dirty_segments: M,
    ) -> Result<Self, DeferredInitError> {
        Self::init_with_config(
            vertices,
            counts,
            edges,
            log,
            span_meta,
            free_spans,
            free_span_by_start,
            maintenance_queue,
            dirty_segments,
            DeferredConfig::default(),
        )
    }

    /// Reopens a deferred graph with custom maintenance thresholds.
    #[allow(clippy::too_many_arguments)]
    pub fn init_with_config(
        vertices: M,
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
        maintenance_queue: M,
        dirty_segments: M,
        config: DeferredConfig,
    ) -> Result<Self, DeferredInitError> {
        let config = config
            .validate()
            .map_err(DeferredInitError::InvalidConfig)?;
        Ok(Self {
            graph: LaraGraph::init(
                vertices,
                counts,
                edges,
                log,
                span_meta,
                free_spans,
                free_span_by_start,
            )
            .map_err(DeferredInitError::Graph)?,
            maintenance: MaintenanceQueue::init(maintenance_queue, dirty_segments)
                .map_err(DeferredInitError::Maintenance)?,
            config,
        })
    }

    /// Returns the underlying LARA graph.
    pub fn graph(&self) -> &LaraGraph<E, V, M> {
        &self.graph
    }

    /// Returns the persistent maintenance queue.
    pub fn maintenance_queue(&self) -> &MaintenanceQueue<M> {
        &self.maintenance
    }

    /// Returns the active deferred-maintenance configuration.
    pub fn config(&self) -> DeferredConfig {
        self.config
    }

    /// Consumes the graph and returns graph memories followed by maintenance memories.
    pub fn into_memories(self) -> (M, M, M, M, M, M, M, M, M) {
        let (vertices, counts, edges, log, span_meta, free_spans, free_span_by_start) =
            self.graph.into_memories();
        let (queue, dirty) = self.maintenance.into_memories();
        (
            vertices,
            counts,
            edges,
            log,
            span_meta,
            free_spans,
            free_span_by_start,
            queue,
            dirty,
        )
    }

    /// Appends a vertex row to the underlying graph.
    pub fn push_vertex(&self, vertex: V) -> Result<VertexId, GraphGrowFailed> {
        self.graph.push_vertex(vertex)
    }

    /// Collects outgoing edges, including entries still waiting in overflow logs.
    pub fn collect_out_edges(&self, src: VertexId) -> Result<Vec<E>, &'static str> {
        self.graph.collect_out_edges(src)
    }

    /// Inserts an edge without immediate rebalancing, enqueueing maintenance if needed.
    pub fn insert_edge_deferred(&self, src: VertexId, edge: E) -> Result<(), DeferredError> {
        let outcome = self
            .graph
            .insert_edge_raw(src, edge)
            .map_err(DeferredError::Graph)?;
        match self.graph.deferred_mark_priority(
            outcome.segment,
            outcome.inserted_into_log,
            self.config.leaf_dirty_density,
            self.config.log_urgent_ratio,
        ) {
            MarkPriority::Clean => {}
            MarkPriority::Dirty(segment) => {
                self.maintenance
                    .mark_dirty(segment)
                    .map_err(DeferredError::Maintenance)?;
            }
            MarkPriority::Urgent(segment) => {
                self.maintenance
                    .mark_urgent(segment)
                    .map_err(DeferredError::Maintenance)?;
            }
        }
        Ok(())
    }

    /// Removes one outgoing edge without preserving adjacency order.
    pub fn remove_edge_deferred(&self, src: VertexId, edge: E) -> Result<bool, DeferredError>
    where
        E: PartialEq,
    {
        Ok(self
            .remove_edge_matching_deferred(src, |candidate| *candidate == edge)?
            .is_some())
    }

    /// Removes the first outgoing edge accepted by `matches`.
    pub fn remove_edge_matching_deferred<F>(
        &self,
        src: VertexId,
        matches: F,
    ) -> Result<Option<E>, DeferredError>
    where
        F: FnMut(&E) -> bool,
    {
        self.graph
            .remove_edge_matching(src, matches)
            .map_err(DeferredError::Graph)
    }

    /// Processes at most one queued dirty segment.
    pub fn maintenance_step(&self) -> Result<Option<MaintenanceWorkReport>, DeferredError> {
        let Some(segment) = self
            .maintenance
            .pop_next()
            .map_err(DeferredError::Maintenance)?
        else {
            return Ok(None);
        };

        let mut report = MaintenanceWorkReport {
            processed_segments: 1,
            ..MaintenanceWorkReport::default()
        };
        let before_capacity = self.graph.edges.header().elem_capacity;
        if self.graph.rebalance_maintenance_segment(segment) {
            self.graph
                .rebalance_dirty_segment(segment)
                .map_err(DeferredError::Grow)?;
            report.rebalanced_segments = 1;
            report.resized = self.graph.edges.header().elem_capacity != before_capacity;
        }
        report.remaining_queue_len = self.maintenance.len();
        Ok(Some(report))
    }

    /// Processes queued dirty segments until the budget or queue is exhausted.
    pub fn maintenance(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, DeferredError> {
        let mut report = MaintenanceReport::default();

        while budget
            .max_segments
            .is_none_or(|max_segments| report.work.processed_segments < max_segments)
        {
            report.instructions_used = current_instruction_counter();
            if budget.max_instructions > 0 && report.instructions_used >= budget.max_instructions {
                report.instruction_budget_exhausted = true;
                break;
            }

            let Some(step) = self.maintenance_step()? else {
                break;
            };
            report.work.processed_segments += step.processed_segments;
            report.work.rebalanced_segments += step.rebalanced_segments;
            report.work.resized |= step.resized;
        }

        report.instructions_used = current_instruction_counter();
        report.instruction_budget_exhausted =
            budget.max_instructions > 0 && report.instructions_used >= budget.max_instructions;
        report.work.remaining_queue_len = self.maintenance.len();
        Ok(report)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lara::vertex::Vertex;
    use crate::test_support::{TestEdge, deferred_test_graph, vector_memory};
    use crate::{SegmentId, VertexId};

    #[test]
    fn maintenance_queue_deduplicates_and_prioritizes_urgent_segments() {
        let mq = MaintenanceQueue::new(vector_memory(), vector_memory()).unwrap();

        assert!(mq.mark_dirty(SegmentId::from(2)).unwrap().inserted);
        assert!(!mq.mark_dirty(SegmentId::from(2)).unwrap().inserted);
        assert!(mq.mark_urgent(SegmentId::from(7)).unwrap().inserted);

        assert_eq!(mq.len(), 2);
        assert_eq!(mq.pop_next().unwrap(), Some(SegmentId::from(7)));
        assert_eq!(mq.pop_next().unwrap(), Some(SegmentId::from(2)));
        assert_eq!(mq.pop_next().unwrap(), None);
    }

    #[test]
    fn maintenance_queue_reopens_dirty_membership_and_order() {
        let mq = MaintenanceQueue::new(vector_memory(), vector_memory()).unwrap();
        mq.mark_dirty(SegmentId::from(1)).unwrap();
        mq.mark_dirty(SegmentId::from(3)).unwrap();

        let memories = mq.into_memories();
        let reopened = MaintenanceQueue::init(memories.0, memories.1).unwrap();

        assert!(reopened.is_dirty(SegmentId::from(1)));
        assert!(reopened.is_dirty(SegmentId::from(3)));
        assert_eq!(reopened.pop_next().unwrap(), Some(SegmentId::from(1)));
        assert!(!reopened.is_dirty(SegmentId::from(1)));
        assert_eq!(reopened.pop_next().unwrap(), Some(SegmentId::from(3)));
        assert_eq!(reopened.pop_next().unwrap(), None);
    }

    #[test]
    fn maintenance_step_returns_none_on_empty_queue() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);

        assert_eq!(graph.maintenance_step().unwrap(), None);
    }

    #[test]
    fn maintenance_step_consumes_exactly_one_queue_item() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);
        graph
            .maintenance_queue()
            .mark_dirty(SegmentId::from(0))
            .unwrap();
        graph
            .maintenance_queue()
            .mark_dirty(SegmentId::from(1))
            .unwrap();

        let report = graph.maintenance_step().unwrap().unwrap();

        assert_eq!(report.processed_segments, 1);
        assert_eq!(report.remaining_queue_len, 1);
        assert_eq!(graph.maintenance_queue().len(), 1);
        assert!(!graph.maintenance_queue().is_dirty(SegmentId::from(0)));
        assert!(graph.maintenance_queue().is_dirty(SegmentId::from(1)));
    }

    #[test]
    fn maintenance_loop_preserves_existing_segment_cap_behavior() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);
        graph
            .maintenance_queue()
            .mark_dirty(SegmentId::from(0))
            .unwrap();
        graph
            .maintenance_queue()
            .mark_dirty(SegmentId::from(1))
            .unwrap();

        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
            })
            .unwrap();

        assert_eq!(report.work.processed_segments, 1);
        assert_eq!(report.work.remaining_queue_len, 1);
        assert_eq!(graph.maintenance_queue().len(), 1);
    }

    #[test]
    fn deferred_insert_keeps_reads_correct_until_maintenance_folds_log() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);

        for dst in 10..13 {
            graph
                .insert_edge_deferred(VertexId::from(0), TestEdge(dst))
                .unwrap();
        }

        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12)]
        );
        assert!(graph.graph().vertices().get(0).log_head >= 0);
        assert!(graph.maintenance_queue().is_dirty(SegmentId::from(0)));

        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
            })
            .unwrap();

        assert_eq!(report.work.processed_segments, 1);
        assert_eq!(report.work.rebalanced_segments, 1);
        assert_eq!(graph.graph().vertices().get(0).log_head, -1);
        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12)]
        );
    }

    #[test]
    fn deferred_remove_folds_log_and_removes_edge() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);

        for dst in 10..13 {
            graph
                .insert_edge_deferred(VertexId::from(0), TestEdge(dst))
                .unwrap();
        }
        assert!(graph.graph().vertices().get(0).log_head >= 0);

        assert!(
            graph
                .remove_edge_deferred(VertexId::from(0), TestEdge(11))
                .unwrap()
        );

        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(12)]
        );
        assert_eq!(graph.graph().vertices().get(0).degree, 2);
        assert_eq!(graph.graph().vertices().get(0).log_head, -1);
    }

    #[test]
    fn deferred_maintenance_segment_cap_leaves_unprocessed_segments_queued() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);

        for dst in 10..13 {
            graph
                .insert_edge_deferred(VertexId::from(0), TestEdge(dst))
                .unwrap();
        }
        graph
            .maintenance_queue()
            .mark_dirty(SegmentId::from(1))
            .unwrap();

        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
            })
            .unwrap();

        assert_eq!(report.work.processed_segments, 1);
        assert_eq!(report.work.rebalanced_segments, 1);
        assert!(!graph.maintenance_queue().is_dirty(SegmentId::from(0)));
        assert!(graph.maintenance_queue().is_dirty(SegmentId::from(1)));
        assert_eq!(graph.maintenance_queue().len(), 1);
    }

    #[test]
    fn deferred_lara_graph_reopens_maintenance_state() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);
        for dst in 10..13 {
            graph
                .insert_edge_deferred(VertexId::from(0), TestEdge(dst))
                .unwrap();
        }

        let memories = graph.into_memories();
        let reopened = DeferredLaraGraph::<TestEdge, Vertex, _>::init(
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5, memories.6,
            memories.7, memories.8,
        )
        .unwrap();

        assert!(reopened.maintenance_queue().is_dirty(SegmentId::from(0)));
        assert_eq!(
            reopened.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12)]
        );
    }

    #[test]
    fn deferred_insert_skips_dirty_when_slab_insert_is_below_soft_threshold() {
        let graph = deferred_test_graph(16, 2, 4, &[0, 4, 8, 12]);

        graph
            .insert_edge_deferred(VertexId::from(0), TestEdge(10))
            .unwrap();

        assert!(!graph.maintenance_queue().is_dirty(SegmentId::from(0)));
        assert_eq!(graph.maintenance_queue().len(), 0);
        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10)]
        );
    }

    #[test]
    fn deferred_config_controls_dirty_threshold() {
        let graph = DeferredLaraGraph::<TestEdge, Vertex, _>::new_with_config(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            16,
            2,
            4,
            DeferredConfig {
                leaf_dirty_density: 0.05,
                log_urgent_ratio: 0.80,
            },
        )
        .unwrap();
        for slot in [0, 4, 8, 12] {
            graph
                .push_vertex(Vertex {
                    base_slot_start: slot,
                    degree: 0,
                    capacity: 0,
                    log_head: -1,
                })
                .unwrap();
        }

        graph
            .insert_edge_deferred(VertexId::from(0), TestEdge(10))
            .unwrap();

        assert_eq!(graph.config().leaf_dirty_density, 0.05);
        assert!(graph.maintenance_queue().is_dirty(SegmentId::from(0)));
    }

    #[test]
    fn deferred_config_rejects_invalid_thresholds() {
        let err = match DeferredLaraGraph::<TestEdge, Vertex, _>::new_with_config(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            16,
            2,
            4,
            DeferredConfig {
                leaf_dirty_density: f64::NAN,
                log_urgent_ratio: 0.80,
            },
        ) {
            Ok(_) => panic!("invalid deferred config was accepted"),
            Err(err) => err,
        };

        assert!(matches!(err, DeferredError::InvalidConfig(_)));
    }
}

#[cfg(feature = "canbench")]
mod bench {
    use std::hint::black_box;

    use canbench_rs::bench;

    use super::{MaintenanceBudget, MaintenanceQueue};
    use crate::{SegmentId, VertexId, bench as helper};

    /// Measures persistent maintenance queue admission, duplicate suppression,
    /// urgent priority insertion, and draining. This isolates queue/bitmap cost
    /// from graph rebalancing work.
    #[bench(raw)]
    fn bench_lara_maintenance_queue_mark_pop_1024() -> canbench_rs::BenchResult {
        let mut memories = helper::BenchMemoryFactory::new();
        let queue = MaintenanceQueue::new(memories.memory(), memories.memory()).expect("queue");
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_maintenance_queue_mark_pop");
            for i in 0..helper::MEDIUM_N {
                let segment = SegmentId::from((i % 256) as u32);
                if i % 8 == 0 {
                    queue.mark_urgent(segment).expect("mark urgent");
                } else {
                    queue.mark_dirty(segment).expect("mark dirty");
                }
            }
            let mut count = 0u64;
            while queue.pop_next().expect("pop").is_some() {
                count += 1;
            }
            black_box(count);
        })
    }

    /// Measures deferred edge insertion up to dirty/urgent marking. It excludes
    /// maintenance folding so changes here indicate admission-path or queue
    /// bookkeeping regressions.
    #[bench(raw)]
    fn bench_lara_deferred_insert_dirty_1024() -> canbench_rs::BenchResult {
        let graph = helper::deferred_graph(256);
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_deferred_insert_dirty");
            for i in 0..helper::MEDIUM_N {
                graph
                    .insert_edge_deferred(VertexId::from((i % 256) as u32), helper::test_edge(i))
                    .expect("insert deferred edge");
            }
            black_box(graph.maintenance_queue().len());
        })
    }

    /// Measures one deferred maintenance fold for a dirty segment. The target
    /// is bounded cost for turning log-backed adjacency back into clean slab
    /// layout under a segment budget.
    #[bench(raw)]
    fn bench_lara_deferred_maintenance_fold_1() -> canbench_rs::BenchResult {
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_deferred_maintenance_fold");
            let graph = helper::deferred_graph(16);
            for i in 0..64 {
                graph
                    .insert_edge_deferred(VertexId::from(0), helper::test_edge(i))
                    .expect("insert deferred edge");
            }
            let report = graph
                .maintenance(MaintenanceBudget {
                    max_instructions: 0,
                    max_segments: Some(1),
                })
                .expect("maintenance");
            black_box(report.work.rebalanced_segments);
        })
    }
}
