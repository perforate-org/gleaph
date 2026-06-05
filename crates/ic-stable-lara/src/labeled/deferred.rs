//! Deferred-maintenance wrapper for the labeled LARA graph.

use crate::{
    VertexId,
    labeled::graph::{InitError, LabeledLaraGraph, LabeledOperationError},
    lara::{
        maintenance::{MaintenanceBudget, MaintenanceWorkReport},
        vertex::InitError as VertexInitError,
    },
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use ic_stable_vec_deque::{
    GrowFailed as QueueGrowFailed, InitError as QueueInitError, StableVecDeque,
};
use std::{borrow::Cow, fmt};

#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;

/// One deferred maintenance item for a labeled graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceWorkItem {
    /// Compact the LabelBucketStore VertexSegment containing one vertex.
    CompactLabelBucketVertexSegment {
        /// Vertex whose LabelBucketStore VertexSegment should be compacted.
        vid: VertexId,
    },
    /// Compact the VertexEdgeSpan containing one LabelEdgeSpan (one edge step per queue pop).
    CompactVertexEdgeSpan {
        /// Vertex owning the VertexEdgeSpan.
        vid: VertexId,
        /// Label-bucket index used to validate that the work item is still relevant.
        anchor_bucket_index: u32,
        /// Next label-bucket index to compact.
        resume_bucket_index: u32,
    },
    /// Compact the label-bucket vertex segment, then the owning vertex edge span
    /// (dense-leaf deferred enqueue).
    CompactDenseLabeledVertexMaintenance {
        /// Vertex to compact in both stores.
        vid: VertexId,
    },
    /// Compact only the vertex value byte span (allocation/log cleanup).
    CompactVertexValueSpan {
        /// Vertex owning the value span.
        vid: VertexId,
    },
    /// Compact edge and payload spans together (preferred for value-bearing labels).
    CompactVertexEdgeAndValueSpan {
        /// Vertex owning both spans.
        vid: VertexId,
        /// Label-bucket index used to validate relevance.
        anchor_bucket_index: u32,
        /// Next label-bucket index to compact.
        resume_bucket_index: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        labeled::{BucketLabelKey, graph::LabeledLaraGraph},
        test_support::{labeled_lara_memories, vector_memory},
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestEdge(u32);

    impl CsrEdge for TestEdge {
        const BYTES: usize = 4;

        fn read_from(bytes: &[u8]) -> Self {
            Self(u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
        }

        fn write_to(&self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.0)
        }

        fn with_neighbor_vid(&self, vid: VertexId) -> Self {
            Self(u32::from(vid))
        }
    }

    impl CsrEdgeTombstone for TestEdge {
        fn tombstone_edge() -> Self {
            Self(u32::from(crate::VertexId::EDGE_TOMBSTONE_SENTINEL))
        }
    }

    fn graph() -> DeferredLabeledLaraGraph<TestEdge, crate::VectorMemory> {
        let (v, b, bfs, bfsbs, ec, e, el, esm, efs, efsbs, vs, vffs, vffsbs, vlog, vblobs) =
            labeled_lara_memories();
        let inner = LabeledLaraGraph::new(
            v,
            b,
            bfs,
            bfsbs,
            ec,
            e,
            el,
            esm,
            efs,
            efsbs,
            vs,
            vffs,
            vffsbs,
            vlog,
            vblobs,
            1024,
            BucketLabelKey::from_raw(1),
        )
        .expect("inner graph");
        DeferredLabeledLaraGraph::new(inner, vector_memory()).expect("deferred graph")
    }

    fn drain_vertex_edge_span_compact_queue(
        graph: &DeferredLabeledLaraGraph<TestEdge, crate::VectorMemory>,
    ) {
        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        while graph.maintenance_queue_len() > 0 {
            graph.maintenance(budget);
        }
    }

    #[test]
    fn dense_maintenance_enqueue_deduplicates_per_vertex() {
        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        for target in 0..64u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge(target))
                .unwrap();
        }
        let before = graph.maintenance_queue_len();
        for target in 64..128u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge(target))
                .unwrap();
        }
        assert_eq!(graph.maintenance_queue_len(), before);
    }

    #[test]
    fn maintenance_vertex_edge_span_compact_one_work_item_per_step() {
        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge(999),
            )
            .unwrap();
        for target in 0..30u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge(target))
                .unwrap();
        }
        for target in 0..25u32 {
            graph
                .remove_edge_matching(VertexId::from(0), label, |edge| edge.0 == target)
                .unwrap();
        }

        graph
            .mark_compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        let budget_one = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: None,
        };

        let mut steps = 0u32;
        while graph.maintenance_queue_len() > 0 {
            let report = graph.maintenance(budget_one);
            assert_eq!(
                report.processed_work_items, 1,
                "each maintenance call should advance exactly one compaction step"
            );
            steps = steps.saturating_add(1);
            assert!(
                steps < 512,
                "incremental compaction should finish within a bounded number of steps"
            );
        }
        assert!(
            steps > 1,
            "tombstone-heavy bucket should need multiple steps"
        );

        let inner = graph.inner();
        assert_eq!(
            inner
                .iter_edges_for_label(VertexId::from(0), label)
                .unwrap(),
            vec![
                TestEdge(29),
                TestEdge(28),
                TestEdge(27),
                TestEdge(26),
                TestEdge(25)
            ]
        );
    }

    #[test]
    fn maintenance_vertex_edge_span_compact_clears_many_slab_tombstones() {
        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        for target in 1..=120u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge(target))
                .unwrap();
        }
        for target in 1..=115u32 {
            graph
                .remove_edge_matching(VertexId::from(0), label, |edge| edge.0 == target)
                .unwrap();
        }

        let inner = graph.inner();
        assert_eq!(
            inner
                .iter_edges_for_label(VertexId::from(0), label)
                .unwrap()
                .len(),
            5
        );

        graph
            .mark_compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        drain_vertex_edge_span_compact_queue(&graph);

        assert_eq!(
            inner
                .iter_edges_for_label(VertexId::from(0), label)
                .unwrap(),
            vec![
                TestEdge(120),
                TestEdge(119),
                TestEdge(118),
                TestEdge(117),
                TestEdge(116)
            ]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            inner.vertices(),
            inner.buckets(),
            inner.edges(),
        );
    }

    #[test]
    fn maintenance_compacts_vertex_edge_span() {
        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge(999),
            )
            .unwrap();
        for target in 0..80u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge(target))
                .unwrap();
        }
        for target in 0..72u32 {
            graph
                .remove_edge_matching(VertexId::from(0), label, |edge| edge.0 == target)
                .unwrap();
        }

        let before = graph.inner().vertices().get(VertexId::from(0));
        assert!(before.stored_slots > 8);

        graph
            .mark_compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        drain_vertex_edge_span_compact_queue(&graph);

        let after = graph.inner().vertices().get(VertexId::from(0));
        assert_eq!(after.stored_slots, 9);
    }

    #[test]
    fn deferred_insert_enqueues_maintenance_when_leaf_dense() {
        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge(999),
            )
            .unwrap();
        for target in 0..130u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge(target))
                .unwrap();
        }
        assert!(
            graph.maintenance_queue_len() > 0,
            "expected deferred wrapper to enqueue compaction when PMA leaf is dense"
        );
    }
}

impl Storable for MaintenanceWorkItem {
    const BOUND: Bound = Bound::Bounded {
        max_size: 16,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut bytes = [0u8; 16];
        match *self {
            Self::CompactLabelBucketVertexSegment { vid } => {
                bytes[0] = 0;
                bytes[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
            }
            Self::CompactVertexEdgeSpan {
                vid,
                anchor_bucket_index,
                resume_bucket_index,
            } => {
                bytes[0] = 1;
                bytes[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
                bytes[8..12].copy_from_slice(&anchor_bucket_index.to_le_bytes());
                bytes[12..16].copy_from_slice(&resume_bucket_index.to_le_bytes());
            }
            Self::CompactDenseLabeledVertexMaintenance { vid } => {
                bytes[0] = 2;
                bytes[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
            }
            Self::CompactVertexValueSpan { vid } => {
                bytes[0] = 3;
                bytes[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
            }
            Self::CompactVertexEdgeAndValueSpan {
                vid,
                anchor_bucket_index,
                resume_bucket_index,
            } => {
                bytes[0] = 4;
                bytes[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
                bytes[8..12].copy_from_slice(&anchor_bucket_index.to_le_bytes());
                bytes[12..16].copy_from_slice(&resume_bucket_index.to_le_bytes());
            }
        }
        Cow::Owned(Vec::from(bytes))
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let b = bytes.as_ref();
        let vid = VertexId::from(u32::from_le_bytes(b[4..8].try_into().unwrap()));
        match b[0] {
            1 => Self::CompactVertexEdgeSpan {
                vid,
                anchor_bucket_index: u32::from_le_bytes(b[8..12].try_into().unwrap()),
                resume_bucket_index: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            },
            2 => Self::CompactDenseLabeledVertexMaintenance { vid },
            3 => Self::CompactVertexValueSpan { vid },
            4 => Self::CompactVertexEdgeAndValueSpan {
                vid,
                anchor_bucket_index: u32::from_le_bytes(b[8..12].try_into().unwrap()),
                resume_bucket_index: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            },
            _ => Self::CompactLabelBucketVertexSegment { vid },
        }
    }
}

#[inline]
fn current_instruction_counter() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::instruction_counter()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}

/// Errors returned by deferred labeled graph operations.
#[derive(Debug)]
pub enum DeferredError {
    /// Inner graph operation failed.
    Inner(LabeledOperationError),
    /// Maintenance queue could not be initialized.
    Queue(QueueInitError),
    /// Maintenance queue could not grow.
    QueueGrow(QueueGrowFailed),
}

impl fmt::Display for DeferredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inner(err) => write!(f, "{err}"),
            Self::Queue(err) => write!(f, "queue init failed: {err}"),
            Self::QueueGrow(err) => write!(f, "queue grow failed: {err}"),
        }
    }
}

impl std::error::Error for DeferredError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Inner(err) => Some(err),
            Self::Queue(err) => Some(err),
            Self::QueueGrow(err) => Some(err),
        }
    }
}

/// Deferred-maintenance labeled LARA graph wrapper.
pub struct DeferredLabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    inner: LabeledLaraGraph<E, M>,
    queue: StableVecDeque<MaintenanceWorkItem, M>,
}

impl<E, M> DeferredLabeledLaraGraph<E, M>
where
    E: CsrEdge + CsrEdgeTombstone,
    M: Memory,
{
    /// Wraps an existing labeled graph with a deferred maintenance queue.
    pub fn new(inner: LabeledLaraGraph<E, M>, queue_memory: M) -> Result<Self, DeferredError> {
        Ok(Self {
            inner,
            queue: StableVecDeque::init(queue_memory).map_err(DeferredError::Queue)?,
        })
    }

    /// Opens a deferred labeled graph from stable memories.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        vertices: M,
        buckets: M,
        bucket_free_spans: M,
        bucket_free_span_by_start: M,
        edge_counts: M,
        edges: M,
        edge_log: M,
        edge_span_meta: M,
        edge_free_spans: M,
        edge_free_span_by_start: M,
        payload_slab: M,
        value_free_spans: M,
        value_free_span_by_start: M,
        payload_log: M,
        value_blobs: M,
        queue_memory: M,
        elem_capacity: u64,
        default_label: crate::labeled::BucketLabelKey,
    ) -> Result<Self, InitError> {
        let inner = LabeledLaraGraph::init(
            vertices,
            buckets,
            bucket_free_spans,
            bucket_free_span_by_start,
            edge_counts,
            edges,
            edge_log,
            edge_span_meta,
            edge_free_spans,
            edge_free_span_by_start,
            payload_slab,
            value_free_spans,
            value_free_span_by_start,
            payload_log,
            value_blobs,
            elem_capacity,
            default_label,
        )?;
        let queue = StableVecDeque::init(queue_memory)
            .map_err(|_| InitError::Vertices(VertexInitError::OutOfMemory))?;
        Ok(Self { inner, queue })
    }

    /// Returns the inner single-orientation labeled graph.
    pub fn inner(&self) -> &LabeledLaraGraph<E, M> {
        &self.inner
    }

    /// Returns the number of pending deferred maintenance items.
    pub fn maintenance_queue_len(&self) -> u64 {
        self.queue.len()
    }

    /// Inserts one labeled edge without an immediate leaf cascade; enqueues compaction
    /// when the owning PMA leaf is still dense afterward.
    pub fn insert_edge(
        &self,
        src: VertexId,
        label_id: crate::labeled::BucketLabelKey,
        edge: E,
    ) -> Result<(), DeferredError> {
        self.inner
            .insert_edge_skip_leaf_cascade(src, label_id, edge)
            .map_err(DeferredError::Inner)?;
        self.maybe_enqueue_dense_vertex_maintenance(src)
    }

    /// Removes one edge without an immediate leaf cascade; may enqueue compaction when dense.
    pub fn remove_edge_matching<F>(
        &self,
        src: VertexId,
        label_id: crate::labeled::BucketLabelKey,
        matches: F,
    ) -> Result<Option<E>, DeferredError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        let removed = self
            .inner
            .remove_edge_matching_skip_leaf_cascade(src, label_id, matches)
            .map_err(DeferredError::Inner)?;
        if removed.is_some() {
            self.maybe_enqueue_tombstone_vertex_edge_span_maintenance(src)?;
            if self.inner.labeled_leaf_segment_is_dense(src) {
                #[cfg(feature = "canbench")]
                let _bench_scope = bench_scope("labeled_deferred_remove_dense_enqueue");
                self.maybe_enqueue_dense_vertex_maintenance(src)?;
            }
        }
        Ok(removed)
    }

    fn vertex_edge_span_maintenance_pending(&self, vid: VertexId) -> bool {
        self.queue.iter().any(|item| match item {
            MaintenanceWorkItem::CompactDenseLabeledVertexMaintenance { vid: queued } => {
                queued == vid
            }
            MaintenanceWorkItem::CompactVertexEdgeSpan { vid: queued, .. } => queued == vid,
            MaintenanceWorkItem::CompactVertexEdgeAndValueSpan { vid: queued, .. } => queued == vid,
            MaintenanceWorkItem::CompactLabelBucketVertexSegment { .. }
            | MaintenanceWorkItem::CompactVertexValueSpan { .. } => false,
        })
    }

    fn maybe_enqueue_dense_vertex_maintenance(&self, vid: VertexId) -> Result<(), DeferredError> {
        if !self.inner.labeled_leaf_segment_is_dense(vid) {
            return Ok(());
        }
        if self.vertex_edge_span_maintenance_pending(vid) {
            return Ok(());
        }
        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_deferred_dense_enqueue");
        self.mark_compact_dense_labeled_vertex_maintenance(vid)
    }

    fn maybe_enqueue_tombstone_vertex_edge_span_maintenance(
        &self,
        vid: VertexId,
    ) -> Result<(), DeferredError> {
        if self.vertex_edge_span_maintenance_pending(vid) {
            return Ok(());
        }
        if !self
            .inner
            .vertex_has_slab_tombstone_slack_pressure(vid)
            .map_err(DeferredError::Inner)?
        {
            return Ok(());
        }
        self.mark_compact_vertex_edge_span(vid, 0)
    }

    /// Enqueues bucket-segment compaction followed by vertex-edge-span compaction for one vertex.
    pub fn mark_compact_dense_labeled_vertex_maintenance(
        &self,
        vid: VertexId,
    ) -> Result<(), DeferredError> {
        if self.vertex_edge_span_maintenance_pending(vid) {
            return Ok(());
        }
        self.queue
            .push_back(&MaintenanceWorkItem::CompactDenseLabeledVertexMaintenance { vid })
            .map_err(DeferredError::QueueGrow)?;
        Ok(())
    }

    /// Enqueues exact-fit compaction of the owning LabelBucketStore VertexSegment.
    pub fn mark_compact_label_bucket_vertex_segment(
        &self,
        vid: VertexId,
    ) -> Result<(), DeferredError> {
        self.queue
            .push_back(&MaintenanceWorkItem::CompactLabelBucketVertexSegment { vid })
            .map_err(DeferredError::QueueGrow)?;
        Ok(())
    }

    /// Enqueues compaction of one VertexEdgeSpan.
    pub fn mark_compact_vertex_edge_span(
        &self,
        vid: VertexId,
        bucket_index: u32,
    ) -> Result<(), DeferredError> {
        if self.vertex_edge_span_maintenance_pending(vid) {
            return Ok(());
        }
        self.queue
            .push_back(&MaintenanceWorkItem::CompactVertexEdgeSpan {
                vid,
                anchor_bucket_index: bucket_index,
                resume_bucket_index: 0,
            })
            .map_err(DeferredError::QueueGrow)?;
        Ok(())
    }

    /// Processes queued labeled maintenance work up to `budget`.
    pub fn maintenance(&self, budget: MaintenanceBudget) -> MaintenanceWorkReport
    where
        E: CsrEdgeTombstone,
    {
        use crate::labeled::graph::VertexEdgeSpanCompactOneStep;

        let mut report = MaintenanceWorkReport::default();
        let baseline = current_instruction_counter();
        let max_items = budget.max_work_items.unwrap_or(u32::MAX);
        let mut checkpoint_tick = 0u32;

        while report.processed_work_items < max_items {
            checkpoint_tick = checkpoint_tick.wrapping_add(1);
            let should_check = budget.checkpoint_every <= 1
                || checkpoint_tick.is_multiple_of(budget.checkpoint_every);
            if should_check
                && budget.max_instructions > 0
                && current_instruction_counter()
                    .saturating_sub(baseline)
                    .saturating_add(budget.reserve_instructions)
                    >= budget.max_instructions
            {
                break;
            }

            let Some(item) = self.queue.pop_front() else {
                break;
            };
            report.processed_work_items = report.processed_work_items.saturating_add(1);
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_deferred_maintenance_item");
            let requeue = match item {
                MaintenanceWorkItem::CompactLabelBucketVertexSegment { vid } => {
                    if self.inner.compact_label_bucket_vertex_segment(vid).is_ok() {
                        report.rebalanced_segments = report.rebalanced_segments.saturating_add(1);
                    }
                    None
                }
                MaintenanceWorkItem::CompactVertexEdgeSpan {
                    vid,
                    anchor_bucket_index,
                    resume_bucket_index,
                } => {
                    let vertex = self.inner.vertices().get(vid);
                    if anchor_bucket_index >= vertex.degree() {
                        None
                    } else {
                        match self
                            .inner
                            .compact_vertex_edge_span_one_step(vid, resume_bucket_index)
                        {
                            Ok(VertexEdgeSpanCompactOneStep::EdgeMoved(_)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeSpan {
                                    vid,
                                    anchor_bucket_index,
                                    resume_bucket_index,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(next)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeSpan {
                                    vid,
                                    anchor_bucket_index,
                                    resume_bucket_index: next,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::OverflowRewrite(_))
                            | Ok(VertexEdgeSpanCompactOneStep::Finished) => {
                                report.rebalanced_segments =
                                    report.rebalanced_segments.saturating_add(1);
                                None
                            }
                            Err(_) => None,
                        }
                    }
                }
                MaintenanceWorkItem::CompactDenseLabeledVertexMaintenance { vid } => {
                    if self.inner.compact_label_bucket_vertex_segment(vid).is_ok() {
                        report.rebalanced_segments = report.rebalanced_segments.saturating_add(1);
                    }
                    Some(MaintenanceWorkItem::CompactVertexEdgeSpan {
                        vid,
                        anchor_bucket_index: 0,
                        resume_bucket_index: 0,
                    })
                }
                MaintenanceWorkItem::CompactVertexValueSpan { .. } => None,
                MaintenanceWorkItem::CompactVertexEdgeAndValueSpan {
                    vid,
                    anchor_bucket_index,
                    resume_bucket_index,
                } => {
                    let vertex = self.inner.vertices().get(vid);
                    if anchor_bucket_index >= vertex.degree() {
                        None
                    } else {
                        match self
                            .inner
                            .compact_vertex_edge_span_one_step(vid, resume_bucket_index)
                        {
                            Ok(VertexEdgeSpanCompactOneStep::EdgeMoved(_)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeAndValueSpan {
                                    vid,
                                    anchor_bucket_index,
                                    resume_bucket_index,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(next)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeAndValueSpan {
                                    vid,
                                    anchor_bucket_index,
                                    resume_bucket_index: next,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::OverflowRewrite(_))
                            | Ok(VertexEdgeSpanCompactOneStep::Finished) => {
                                report.rebalanced_segments =
                                    report.rebalanced_segments.saturating_add(1);
                                None
                            }
                            Err(_) => None,
                        }
                    }
                }
            };
            if let Some(next) = requeue {
                let _ = self.queue.push_front(&next);
            }
        }
        report.remaining_queue_len = self.queue.len();
        report
    }
}
