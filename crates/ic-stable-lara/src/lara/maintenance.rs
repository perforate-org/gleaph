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

#[derive(Debug)]
pub enum InitError {
    Queue(ic_stable_vec_deque::InitError),
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

#[derive(Debug)]
pub enum GrowFailed {
    Queue(ic_stable_vec_deque::GrowFailed),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkResult {
    pub segment: SegmentId,
    pub inserted: bool,
}

#[derive(Debug)]
pub struct MaintenanceQueue<MQ: Memory, MD: Memory> {
    queue: StableVecDeque<SegmentId, MQ>,
    dirty: StableRoaringBitmap<MD>,
}

impl<MQ: Memory, MD: Memory> MaintenanceQueue<MQ, MD> {
    pub fn new(queue_memory: MQ, dirty_memory: MD) -> Result<Self, GrowFailed> {
        Ok(Self {
            queue: StableVecDeque::new(queue_memory).map_err(GrowFailed::Queue)?,
            dirty: StableRoaringBitmap::new(dirty_memory).map_err(GrowFailed::DirtySet)?,
        })
    }

    pub fn init(queue_memory: MQ, dirty_memory: MD) -> Result<Self, InitError> {
        Ok(Self {
            queue: StableVecDeque::init(queue_memory).map_err(InitError::Queue)?,
            dirty: StableRoaringBitmap::init(dirty_memory).map_err(InitError::DirtySet)?,
        })
    }

    pub fn into_memories(self) -> (MQ, MD) {
        (self.queue.into_memory(), self.dirty.into_memory())
    }

    pub fn len(&self) -> u64 {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn is_dirty(&self, segment: SegmentId) -> bool {
        self.dirty.contains(u32::from(segment))
    }

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

    pub fn clear_dirty(&self, segment: SegmentId) -> Result<(), GrowFailed> {
        self.dirty
            .clear(u32::from(segment))
            .map_err(GrowFailed::DirtySet)
    }
}

#[derive(Debug)]
pub enum DeferredInitError {
    Graph(GraphInitError),
    Maintenance(InitError),
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

#[derive(Debug)]
pub enum DeferredError {
    Graph(&'static str),
    Grow(GraphGrowFailed),
    Maintenance(GrowFailed),
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeferredConfig {
    pub leaf_dirty_density: f64,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceWorkReport {
    pub processed_segments: u32,
    pub rebalanced_segments: u32,
    pub resized: bool,
    pub remaining_queue_len: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub work: MaintenanceWorkReport,
    pub instructions_used: u64,
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

pub struct DeferredLaraGraph<E, V, MV, MC, ME, ML, MS, MF, MMQ, MDS>
where
    E: CsrEdge,
    V: LaraVertex,
    MV: Memory,
    MC: Memory,
    ME: Memory,
    ML: Memory,
    MS: Memory,
    MF: Memory,
    MMQ: Memory,
    MDS: Memory,
{
    graph: LaraGraph<E, V, MV, MC, ME, ML, MS, MF>,
    maintenance: MaintenanceQueue<MMQ, MDS>,
    config: DeferredConfig,
}

impl<E, V, MV, MC, ME, ML, MS, MF, MMQ, MDS>
    DeferredLaraGraph<E, V, MV, MC, ME, ML, MS, MF, MMQ, MDS>
where
    E: CsrEdge,
    V: LaraVertex,
    MV: Memory,
    MC: Memory,
    ME: Memory,
    ML: Memory,
    MS: Memory,
    MF: Memory,
    MMQ: Memory,
    MDS: Memory,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vertices: MV,
        counts: MC,
        edges: ME,
        log: ML,
        span_meta: MS,
        free_spans: MF,
        maintenance_queue: MMQ,
        dirty_segments: MDS,
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
            maintenance_queue,
            dirty_segments,
            elem_capacity,
            segment_count,
            segment_size,
            DeferredConfig::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_config(
        vertices: MV,
        counts: MC,
        edges: ME,
        log: ML,
        span_meta: MS,
        free_spans: MF,
        maintenance_queue: MMQ,
        dirty_segments: MDS,
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

    #[allow(clippy::too_many_arguments)]
    pub fn init(
        vertices: MV,
        counts: MC,
        edges: ME,
        log: ML,
        span_meta: MS,
        free_spans: MF,
        maintenance_queue: MMQ,
        dirty_segments: MDS,
    ) -> Result<Self, DeferredInitError> {
        Self::init_with_config(
            vertices,
            counts,
            edges,
            log,
            span_meta,
            free_spans,
            maintenance_queue,
            dirty_segments,
            DeferredConfig::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn init_with_config(
        vertices: MV,
        counts: MC,
        edges: ME,
        log: ML,
        span_meta: MS,
        free_spans: MF,
        maintenance_queue: MMQ,
        dirty_segments: MDS,
        config: DeferredConfig,
    ) -> Result<Self, DeferredInitError> {
        let config = config
            .validate()
            .map_err(DeferredInitError::InvalidConfig)?;
        Ok(Self {
            graph: LaraGraph::init(vertices, counts, edges, log, span_meta, free_spans)
                .map_err(DeferredInitError::Graph)?,
            maintenance: MaintenanceQueue::init(maintenance_queue, dirty_segments)
                .map_err(DeferredInitError::Maintenance)?,
            config,
        })
    }

    pub fn graph(&self) -> &LaraGraph<E, V, MV, MC, ME, ML, MS, MF> {
        &self.graph
    }

    pub fn maintenance_queue(&self) -> &MaintenanceQueue<MMQ, MDS> {
        &self.maintenance
    }

    pub fn config(&self) -> DeferredConfig {
        self.config
    }

    pub fn into_memories(self) -> (MV, MC, ME, ML, MS, MF, MMQ, MDS) {
        let (vertices, counts, edges, log, span_meta, free_spans) = self.graph.into_memories();
        let (queue, dirty) = self.maintenance.into_memories();
        (
            vertices, counts, edges, log, span_meta, free_spans, queue, dirty,
        )
    }

    pub fn push_vertex(&self, vertex: V) -> Result<VertexId, GraphGrowFailed> {
        self.graph.push_vertex(vertex)
    }

    pub fn collect_out_edges(&self, src: VertexId) -> Result<Vec<E>, &'static str> {
        self.graph.collect_out_edges(src)
    }

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
        let reopened = DeferredLaraGraph::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::init(
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5, memories.6,
            memories.7,
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
        let graph = DeferredLaraGraph::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::new_with_config(
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
        let err =
            match DeferredLaraGraph::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::new_with_config(
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
