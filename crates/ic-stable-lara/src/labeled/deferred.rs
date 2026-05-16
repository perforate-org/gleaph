//! Deferred-maintenance wrapper for the labeled LARA graph.

use crate::{
    VertexId,
    labeled::graph::{InitError, LabeledLaraGraph, LabeledOperationError},
    lara::{
        maintenance::{MaintenanceBudget, MaintenanceWorkReport},
        vertex::InitError as VertexInitError,
    },
    traits::CsrEdge,
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
    /// Compact the VertexEdgeSpan containing one LabelEdgeSpan.
    CompactVertexEdgeSpan {
        /// Vertex owning the VertexEdgeSpan.
        vid: VertexId,
        /// Label-bucket index used to validate that the work item is still relevant.
        bucket_index: u32,
    },
    /// Compact the label-bucket vertex segment, then the owning vertex edge span
    /// (dense-leaf deferred enqueue).
    CompactDenseLabeledVertexMaintenance {
        /// Vertex to compact in both stores.
        vid: VertexId,
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

        fn write_to(self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.0)
        }

        fn with_neighbor_vid(self, vid: VertexId) -> Self {
            Self(u32::from(vid))
        }
    }

    fn graph() -> DeferredLabeledLaraGraph<TestEdge, crate::VectorMemory> {
        let (v, b, bfs, bfsbs, ec, e, el, esm, efs, efsbs) = labeled_lara_memories();
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
            1024,
            BucketLabelKey::from_raw(1),
        )
        .expect("inner graph");
        DeferredLabeledLaraGraph::new(inner, vector_memory()).expect("deferred graph")
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
        assert!(before.vertex_edge_alloc_slots() > 8);

        graph
            .mark_compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        let report = graph.maintenance(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: None,
        });

        assert_eq!(report.processed_work_items, 1);
        let after = graph.inner().vertices().get(VertexId::from(0));
        assert_eq!(after.vertex_edge_alloc_slots(), 9);
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
        max_size: 12,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut bytes = [0u8; 12];
        match *self {
            Self::CompactLabelBucketVertexSegment { vid } => {
                bytes[0] = 0;
                bytes[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
            }
            Self::CompactVertexEdgeSpan { vid, bucket_index } => {
                bytes[0] = 1;
                bytes[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
                bytes[8..12].copy_from_slice(&bucket_index.to_le_bytes());
            }
            Self::CompactDenseLabeledVertexMaintenance { vid } => {
                bytes[0] = 2;
                bytes[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
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
                bucket_index: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            },
            2 => Self::CompactDenseLabeledVertexMaintenance { vid },
            _ => Self::CompactLabelBucketVertexSegment { vid },
        }
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
    E: CsrEdge,
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

    /// See [`LabeledLaraGraph::out_edges_iter`].
    pub fn out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.inner.out_edges_iter(src)
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
        if self.inner.labeled_leaf_segment_is_dense(src) {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_deferred_dense_enqueue");
            self.mark_compact_dense_labeled_vertex_maintenance(src)?;
        }
        Ok(())
    }

    /// Removes one edge without an immediate leaf cascade; may enqueue compaction when dense.
    pub fn remove_edge_matching<F>(
        &self,
        src: VertexId,
        label_id: crate::labeled::BucketLabelKey,
        matches: F,
    ) -> Result<Option<E>, DeferredError>
    where
        F: FnMut(&E) -> bool,
    {
        let removed = self
            .inner
            .remove_edge_matching_skip_leaf_cascade(src, label_id, matches)
            .map_err(DeferredError::Inner)?;
        if removed.is_some() && self.inner.labeled_leaf_segment_is_dense(src) {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_deferred_remove_dense_enqueue");
            self.mark_compact_dense_labeled_vertex_maintenance(src)?;
        }
        Ok(removed)
    }

    /// Enqueues bucket-segment compaction followed by vertex-edge-span compaction for one vertex.
    pub fn mark_compact_dense_labeled_vertex_maintenance(
        &self,
        vid: VertexId,
    ) -> Result<(), DeferredError> {
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
        self.queue
            .push_back(&MaintenanceWorkItem::CompactVertexEdgeSpan { vid, bucket_index })
            .map_err(DeferredError::QueueGrow)?;
        Ok(())
    }

    /// Processes queued labeled maintenance work up to `budget`.
    pub fn maintenance(&self, budget: MaintenanceBudget) -> MaintenanceWorkReport {
        let mut report = MaintenanceWorkReport::default();
        let max_items = budget.max_work_items.unwrap_or(u32::MAX);
        while report.processed_work_items < max_items {
            let Some(item) = self.queue.pop_front() else {
                break;
            };
            report.processed_work_items = report.processed_work_items.saturating_add(1);
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_deferred_maintenance_item");
            match item {
                MaintenanceWorkItem::CompactLabelBucketVertexSegment { vid } => {
                    if self.inner.compact_label_bucket_vertex_segment(vid).is_ok() {
                        report.rebalanced_segments = report.rebalanced_segments.saturating_add(1);
                    }
                }
                MaintenanceWorkItem::CompactVertexEdgeSpan { vid, bucket_index } => {
                    if self
                        .inner
                        .compact_vertex_edge_span(vid, bucket_index)
                        .is_ok()
                    {
                        report.rebalanced_segments = report.rebalanced_segments.saturating_add(1);
                    }
                }
                MaintenanceWorkItem::CompactDenseLabeledVertexMaintenance { vid } => {
                    if self.inner.compact_label_bucket_vertex_segment(vid).is_ok() {
                        report.rebalanced_segments = report.rebalanced_segments.saturating_add(1);
                    }
                    if self.inner.compact_vertex_edge_span(vid, 0).is_ok() {
                        report.rebalanced_segments = report.rebalanced_segments.saturating_add(1);
                    }
                }
            }
        }
        report.remaining_queue_len = self.queue.len();
        report
    }
}
