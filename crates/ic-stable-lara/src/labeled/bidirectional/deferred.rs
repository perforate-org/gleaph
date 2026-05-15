//! Deferred-maintenance bidirectional labeled LARA graph wrapper.

use crate::{
    VertexCount, VertexId,
    labeled::{
        graph::{InitError, LabeledLaraGraph, LabeledOperationError},
        record::LabelId,
    },
    lara::maintenance::{
        DeferredConfig, DeferredConfigError, MaintenanceBudget, MaintenanceWorkReport,
    },
    traits::CsrEdge,
};
use ic_stable_roaring::{BitmapError, InitError as RoaringInitError, StableRoaringBitmap};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use ic_stable_vec_deque::{
    GrowFailed as QueueGrowFailed, InitError as QueueInitError, StableVecDeque,
};
use std::{borrow::Cow, fmt};

/// Maintenance report for a deferred bidirectional labeled graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BidirectionalMaintenanceReport {
    /// Aggregated work performed by the unified queue.
    pub work: MaintenanceWorkReport,
}

impl BidirectionalMaintenanceReport {
    /// Returns total remaining queued work items.
    pub fn remaining_queue_len(self) -> u64 {
        self.work.remaining_queue_len
    }
}

/// Direction of a queued bidirectional labeled maintenance item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    /// Forward out-adjacency store.
    Forward,
    /// Reverse in-adjacency store.
    Reverse,
}

/// One item in the unified deferred bidirectional labeled maintenance queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceWorkItem {
    /// Compact the LabelBucketStore VertexSegment containing one vertex in one orientation.
    CompactLabelBucketVertexSegment {
        /// Orientation whose LabelBucketStore VertexSegment should be compacted.
        orientation: Orientation,
        /// Vertex inside the segment to compact.
        vid: VertexId,
    },
    /// Compact one VertexEdgeSpan in one orientation.
    CompactVertexEdgeSpan {
        /// Orientation whose VertexEdgeSpan should be compacted.
        orientation: Orientation,
        /// Vertex owning the VertexEdgeSpan.
        vid: VertexId,
        /// Label-bucket index used to validate that the work item is still relevant.
        bucket_index: u32,
    },
    /// Compact the label-bucket vertex segment then the vertex edge span for one orientation.
    CompactDenseLabeledVertexMaintenance {
        /// Orientation whose stores should be compacted.
        orientation: Orientation,
        /// Vertex to compact.
        vid: VertexId,
    },
}

fn maintenance_work_item_bytes(item: &MaintenanceWorkItem) -> [u8; 16] {
    let mut b = [0u8; 16];
    match *item {
        MaintenanceWorkItem::CompactLabelBucketVertexSegment { orientation, vid } => {
            b[0] = 0;
            b[1] = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            b[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
        }
        MaintenanceWorkItem::CompactVertexEdgeSpan {
            orientation,
            vid,
            bucket_index,
        } => {
            b[0] = 1;
            b[1] = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            b[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
            b[8..12].copy_from_slice(&bucket_index.to_le_bytes());
        }
        MaintenanceWorkItem::CompactDenseLabeledVertexMaintenance { orientation, vid } => {
            b[0] = 2;
            b[1] = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            b[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
        }
    }
    b
}

impl Storable for MaintenanceWorkItem {
    const BOUND: Bound = Bound::Bounded {
        max_size: 16,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(maintenance_work_item_bytes(self)))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(maintenance_work_item_bytes(&self))
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let b = bytes.as_ref();
        let vid = VertexId::from(u32::from_le_bytes(b[4..8].try_into().unwrap()));
        let orientation = if b[1] == 1 {
            Orientation::Reverse
        } else {
            Orientation::Forward
        };
        match b[0] {
            1 => Self::CompactVertexEdgeSpan {
                orientation,
                vid,
                bucket_index: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            },
            2 => Self::CompactDenseLabeledVertexMaintenance { orientation, vid },
            _ => Self::CompactLabelBucketVertexSegment { orientation, vid },
        }
    }
}

#[derive(Debug)]
struct BidirectionalMaintenanceQueue<M: Memory> {
    queue: StableVecDeque<MaintenanceWorkItem, M>,
    dirty: StableRoaringBitmap<M>,
}

impl<M: Memory> BidirectionalMaintenanceQueue<M> {
    fn new(queue_memory: M, dirty_memory: M) -> Result<Self, DeferredBidirectionalLabeledError> {
        Ok(Self {
            queue: StableVecDeque::new(queue_memory)
                .map_err(DeferredBidirectionalLabeledError::MaintenanceQueue)?,
            dirty: StableRoaringBitmap::new(dirty_memory)
                .map_err(DeferredBidirectionalLabeledError::MaintenanceDirtyBitmap)?,
        })
    }

    fn init(queue_memory: M, dirty_memory: M) -> Result<Self, DeferredBidirectionalLabeledError> {
        Ok(Self {
            queue: StableVecDeque::init(queue_memory)
                .map_err(DeferredBidirectionalLabeledError::MaintenanceQueueInit)?,
            dirty: StableRoaringBitmap::init(dirty_memory)
                .map_err(DeferredBidirectionalLabeledError::MaintenanceDirtyInit)?,
        })
    }

    fn len(&self) -> u64 {
        self.queue.len()
    }

    fn mark_dirty(
        &self,
        item: MaintenanceWorkItem,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        let key = work_item_key(item);
        if self.dirty.contains(key) {
            return Ok(false);
        }
        self.dirty
            .insert(key)
            .map_err(DeferredBidirectionalLabeledError::MaintenanceDirtyBitmap)?;
        self.queue
            .push_back(&item)
            .map_err(DeferredBidirectionalLabeledError::MaintenanceQueue)?;
        Ok(true)
    }

    fn pop_next(&self) -> Result<Option<MaintenanceWorkItem>, DeferredBidirectionalLabeledError> {
        while let Some(item) = self.queue.pop_front() {
            if self.dirty.contains(work_item_key(item)) {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    fn complete(&self, item: MaintenanceWorkItem) -> Result<(), DeferredBidirectionalLabeledError> {
        self.dirty
            .clear(work_item_key(item))
            .map_err(DeferredBidirectionalLabeledError::MaintenanceDirtyBitmap)
    }
}

fn work_item_key(item: MaintenanceWorkItem) -> u32 {
    match item {
        MaintenanceWorkItem::CompactLabelBucketVertexSegment { orientation, vid } => {
            let orient = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            u32::from(vid).saturating_mul(2).saturating_add(orient)
        }
        MaintenanceWorkItem::CompactVertexEdgeSpan {
            orientation,
            vid,
            bucket_index,
        } => {
            let orient = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            0x4000_0000 | bucket_index ^ (u32::from(vid) << 1) ^ orient
        }
        MaintenanceWorkItem::CompactDenseLabeledVertexMaintenance { orientation, vid } => {
            let orient = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            0xC000_0000u32 ^ u32::from(vid).wrapping_mul(2_654_435_761) ^ orient
        }
    }
}

/// Errors returned by deferred bidirectional labeled graph operations.
#[derive(Debug)]
pub enum DeferredBidirectionalLabeledError {
    /// Forward orientation failed.
    Forward(LabeledOperationError),
    /// Reverse orientation failed.
    Reverse(LabeledOperationError),
    /// Forward orientation could not be initialized.
    ForwardInit(InitError),
    /// Reverse orientation could not be initialized.
    ReverseInit(InitError),
    /// Stable memory grow or format initialization failed.
    Grow(crate::GrowFailed),
    /// Maintenance queue could not grow.
    MaintenanceQueue(QueueGrowFailed),
    /// Maintenance queue could not be reopened.
    MaintenanceQueueInit(QueueInitError),
    /// Maintenance dirty bitmap could not be reopened.
    MaintenanceDirtyInit(RoaringInitError),
    /// Maintenance dirty bitmap operation failed.
    MaintenanceDirtyBitmap(BitmapError),
    /// Deferred maintenance configuration is invalid.
    InvalidConfig(DeferredConfigError),
    /// The two orientations do not contain the same number of vertex rows.
    VertexCountMismatch {
        /// Forward vertex count.
        forward: VertexCount,
        /// Reverse vertex count.
        reverse: VertexCount,
    },
    /// Addressing a vertex outside `0..vertex_count`.
    VertexOutOfRange {
        /// Requested vertex id.
        vid: VertexId,
        /// Current vertex column length.
        len: VertexCount,
    },
}

impl fmt::Display for DeferredBidirectionalLabeledError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forward(err) => write!(f, "forward store: {err}"),
            Self::Reverse(err) => write!(f, "reverse store: {err}"),
            Self::ForwardInit(err) => write!(f, "forward init failed: {err}"),
            Self::ReverseInit(err) => write!(f, "reverse init failed: {err}"),
            Self::Grow(err) => write!(f, "format / grow: {err}"),
            Self::MaintenanceQueue(err) => write!(f, "maintenance queue failed: {err}"),
            Self::MaintenanceQueueInit(err) => write!(f, "maintenance queue init failed: {err}"),
            Self::MaintenanceDirtyInit(err) => {
                write!(f, "maintenance dirty bitmap init failed: {err}")
            }
            Self::MaintenanceDirtyBitmap(err) => {
                write!(f, "maintenance dirty bitmap failed: {err}")
            }
            Self::InvalidConfig(err) => write!(f, "invalid deferred config: {err}"),
            Self::VertexCountMismatch { forward, reverse } => write!(
                f,
                "vertex column length mismatch: forward={forward} reverse={reverse}"
            ),
            Self::VertexOutOfRange { vid, len } => {
                write!(f, "vertex {vid} out of range (len={len})")
            }
        }
    }
}

impl std::error::Error for DeferredBidirectionalLabeledError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Forward(err) | Self::Reverse(err) => Some(err),
            Self::ForwardInit(err) | Self::ReverseInit(err) => Some(err),
            Self::Grow(err) => Some(err),
            Self::MaintenanceQueue(err) => Some(err),
            Self::MaintenanceQueueInit(err) => Some(err),
            Self::MaintenanceDirtyInit(err) => Some(err),
            Self::MaintenanceDirtyBitmap(err) => Some(err),
            Self::InvalidConfig(err) => Some(err),
            Self::VertexCountMismatch { .. } | Self::VertexOutOfRange { .. } => None,
        }
    }
}

impl From<crate::GrowFailed> for DeferredBidirectionalLabeledError {
    fn from(value: crate::GrowFailed) -> Self {
        Self::Grow(value)
    }
}

/// Bidirectional labeled LARA graph whose two orientations share one deferred queue.
pub struct DeferredBidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    forward: LabeledLaraGraph<E, M>,
    reverse: LabeledLaraGraph<E, M>,
    maintenance: BidirectionalMaintenanceQueue<M>,
    config: DeferredConfig,
}

impl<E, M> DeferredBidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Creates fresh bidirectional labeled stores with a shared deferred queue.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        forward_vertices: M,
        forward_buckets: M,
        forward_bucket_free_spans: M,
        forward_bucket_free_span_by_start: M,
        forward_edge_counts: M,
        forward_edges: M,
        forward_edge_log: M,
        forward_edge_span_meta: M,
        forward_edge_free_spans: M,
        forward_edge_free_span_by_start: M,
        reverse_vertices: M,
        reverse_buckets: M,
        reverse_bucket_free_spans: M,
        reverse_bucket_free_span_by_start: M,
        reverse_edge_counts: M,
        reverse_edges: M,
        reverse_edge_log: M,
        reverse_edge_span_meta: M,
        reverse_edge_free_spans: M,
        reverse_edge_free_span_by_start: M,
        maintenance_queue: M,
        dirty_work_items: M,
        elem_capacity: u64,
        default_label: LabelId,
    ) -> Result<Self, DeferredBidirectionalLabeledError> {
        Self::new_with_config(
            forward_vertices,
            forward_buckets,
            forward_bucket_free_spans,
            forward_bucket_free_span_by_start,
            forward_edge_counts,
            forward_edges,
            forward_edge_log,
            forward_edge_span_meta,
            forward_edge_free_spans,
            forward_edge_free_span_by_start,
            reverse_vertices,
            reverse_buckets,
            reverse_bucket_free_spans,
            reverse_bucket_free_span_by_start,
            reverse_edge_counts,
            reverse_edges,
            reverse_edge_log,
            reverse_edge_span_meta,
            reverse_edge_free_spans,
            reverse_edge_free_span_by_start,
            maintenance_queue,
            dirty_work_items,
            elem_capacity,
            default_label,
            DeferredConfig::default(),
        )
    }

    /// Creates fresh bidirectional labeled stores with explicit deferred thresholds.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_config(
        forward_vertices: M,
        forward_buckets: M,
        forward_bucket_free_spans: M,
        forward_bucket_free_span_by_start: M,
        forward_edge_counts: M,
        forward_edges: M,
        forward_edge_log: M,
        forward_edge_span_meta: M,
        forward_edge_free_spans: M,
        forward_edge_free_span_by_start: M,
        reverse_vertices: M,
        reverse_buckets: M,
        reverse_bucket_free_spans: M,
        reverse_bucket_free_span_by_start: M,
        reverse_edge_counts: M,
        reverse_edges: M,
        reverse_edge_log: M,
        reverse_edge_span_meta: M,
        reverse_edge_free_spans: M,
        reverse_edge_free_span_by_start: M,
        maintenance_queue: M,
        dirty_work_items: M,
        elem_capacity: u64,
        default_label: LabelId,
        config: DeferredConfig,
    ) -> Result<Self, DeferredBidirectionalLabeledError> {
        let config = config
            .validate()
            .map_err(DeferredBidirectionalLabeledError::InvalidConfig)?;
        let forward = LabeledLaraGraph::new(
            forward_vertices,
            forward_buckets,
            forward_bucket_free_spans,
            forward_bucket_free_span_by_start,
            forward_edge_counts,
            forward_edges,
            forward_edge_log,
            forward_edge_span_meta,
            forward_edge_free_spans,
            forward_edge_free_span_by_start,
            elem_capacity,
            default_label,
        )?;
        let reverse = LabeledLaraGraph::new(
            reverse_vertices,
            reverse_buckets,
            reverse_bucket_free_spans,
            reverse_bucket_free_span_by_start,
            reverse_edge_counts,
            reverse_edges,
            reverse_edge_log,
            reverse_edge_span_meta,
            reverse_edge_free_spans,
            reverse_edge_free_span_by_start,
            elem_capacity,
            default_label,
        )?;
        let maintenance = BidirectionalMaintenanceQueue::new(maintenance_queue, dirty_work_items)?;
        Ok(Self {
            forward,
            reverse,
            maintenance,
            config,
        })
    }

    /// Opens bidirectional labeled stores and the shared deferred queue.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        forward_vertices: M,
        forward_buckets: M,
        forward_bucket_free_spans: M,
        forward_bucket_free_span_by_start: M,
        forward_edge_counts: M,
        forward_edges: M,
        forward_edge_log: M,
        forward_edge_span_meta: M,
        forward_edge_free_spans: M,
        forward_edge_free_span_by_start: M,
        reverse_vertices: M,
        reverse_buckets: M,
        reverse_bucket_free_spans: M,
        reverse_bucket_free_span_by_start: M,
        reverse_edge_counts: M,
        reverse_edges: M,
        reverse_edge_log: M,
        reverse_edge_span_meta: M,
        reverse_edge_free_spans: M,
        reverse_edge_free_span_by_start: M,
        maintenance_queue: M,
        dirty_work_items: M,
        elem_capacity: u64,
        default_label: LabelId,
    ) -> Result<Self, DeferredBidirectionalLabeledError> {
        Self::init_with_config(
            forward_vertices,
            forward_buckets,
            forward_bucket_free_spans,
            forward_bucket_free_span_by_start,
            forward_edge_counts,
            forward_edges,
            forward_edge_log,
            forward_edge_span_meta,
            forward_edge_free_spans,
            forward_edge_free_span_by_start,
            reverse_vertices,
            reverse_buckets,
            reverse_bucket_free_spans,
            reverse_bucket_free_span_by_start,
            reverse_edge_counts,
            reverse_edges,
            reverse_edge_log,
            reverse_edge_span_meta,
            reverse_edge_free_spans,
            reverse_edge_free_span_by_start,
            maintenance_queue,
            dirty_work_items,
            elem_capacity,
            default_label,
            DeferredConfig::default(),
        )
    }

    /// Opens bidirectional labeled stores with explicit deferred thresholds.
    #[allow(clippy::too_many_arguments)]
    pub fn init_with_config(
        forward_vertices: M,
        forward_buckets: M,
        forward_bucket_free_spans: M,
        forward_bucket_free_span_by_start: M,
        forward_edge_counts: M,
        forward_edges: M,
        forward_edge_log: M,
        forward_edge_span_meta: M,
        forward_edge_free_spans: M,
        forward_edge_free_span_by_start: M,
        reverse_vertices: M,
        reverse_buckets: M,
        reverse_bucket_free_spans: M,
        reverse_bucket_free_span_by_start: M,
        reverse_edge_counts: M,
        reverse_edges: M,
        reverse_edge_log: M,
        reverse_edge_span_meta: M,
        reverse_edge_free_spans: M,
        reverse_edge_free_span_by_start: M,
        maintenance_queue: M,
        dirty_work_items: M,
        elem_capacity: u64,
        default_label: LabelId,
        config: DeferredConfig,
    ) -> Result<Self, DeferredBidirectionalLabeledError> {
        let config = config
            .validate()
            .map_err(DeferredBidirectionalLabeledError::InvalidConfig)?;
        let forward = LabeledLaraGraph::init(
            forward_vertices,
            forward_buckets,
            forward_bucket_free_spans,
            forward_bucket_free_span_by_start,
            forward_edge_counts,
            forward_edges,
            forward_edge_log,
            forward_edge_span_meta,
            forward_edge_free_spans,
            forward_edge_free_span_by_start,
            elem_capacity,
            default_label,
        )
        .map_err(DeferredBidirectionalLabeledError::ForwardInit)?;
        let reverse = LabeledLaraGraph::init(
            reverse_vertices,
            reverse_buckets,
            reverse_bucket_free_spans,
            reverse_bucket_free_span_by_start,
            reverse_edge_counts,
            reverse_edges,
            reverse_edge_log,
            reverse_edge_span_meta,
            reverse_edge_free_spans,
            reverse_edge_free_span_by_start,
            elem_capacity,
            default_label,
        )
        .map_err(DeferredBidirectionalLabeledError::ReverseInit)?;
        let maintenance = BidirectionalMaintenanceQueue::init(maintenance_queue, dirty_work_items)?;
        Ok(Self {
            forward,
            reverse,
            maintenance,
            config,
        })
    }

    /// Returns the forward out-adjacency orientation.
    pub fn forward(&self) -> &LabeledLaraGraph<E, M> {
        &self.forward
    }

    /// Returns the reverse in-adjacency orientation.
    pub fn reverse(&self) -> &LabeledLaraGraph<E, M> {
        &self.reverse
    }

    /// Returns the validated deferred maintenance configuration.
    pub fn config(&self) -> DeferredConfig {
        self.config
    }

    /// Returns the number of queued maintenance items.
    pub fn maintenance_queue_len(&self) -> u64 {
        self.maintenance.len()
    }

    /// Enqueues exact-fit compaction of a LabelBucketStore VertexSegment.
    pub fn mark_compact_label_bucket_vertex_segment(
        &self,
        orientation: Orientation,
        vid: VertexId,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.maintenance
            .mark_dirty(MaintenanceWorkItem::CompactLabelBucketVertexSegment { orientation, vid })
            .map(|_| ())
    }

    /// Enqueues compaction of one VertexEdgeSpan.
    pub fn mark_compact_vertex_edge_span(
        &self,
        orientation: Orientation,
        vid: VertexId,
        bucket_index: u32,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.maintenance
            .mark_dirty(MaintenanceWorkItem::CompactVertexEdgeSpan {
                orientation,
                vid,
                bucket_index,
            })
            .map(|_| ())
    }

    /// Enqueues label-bucket vertex-segment compaction then vertex-edge-span compaction.
    pub fn mark_compact_dense_labeled_vertex_maintenance(
        &self,
        orientation: Orientation,
        vid: VertexId,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.maintenance
            .mark_dirty(MaintenanceWorkItem::CompactDenseLabeledVertexMaintenance {
                orientation,
                vid,
            })
            .map(|_| ())
    }

    /// Appends one vertex row to both orientations.
    pub fn push_vertex(&self) -> Result<VertexId, DeferredBidirectionalLabeledError> {
        let forward_count = self.forward.vertex_count();
        let reverse_count = self.reverse.vertex_count();
        if forward_count != reverse_count {
            return Err(DeferredBidirectionalLabeledError::VertexCountMismatch {
                forward: forward_count,
                reverse: reverse_count,
            });
        }
        self.forward
            .push_vertex(crate::labeled::record::LabeledVertex::default())?;
        self.reverse
            .push_vertex(crate::labeled::record::LabeledVertex::default())?;
        Ok(VertexId::from(
            self.forward.vertex_count().0.saturating_sub(1),
        ))
    }

    /// Inserts one directed edge into forward and reverse orientations.
    pub fn insert_directed_edge(
        &self,
        src: VertexId,
        dst: VertexId,
        label_id: LabelId,
        forward_edge: E,
        reverse_edge: E,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.forward
            .insert_edge_skip_leaf_cascade(src, label_id, forward_edge)
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        self.reverse
            .insert_edge_skip_leaf_cascade(dst, label_id, reverse_edge)
            .map_err(DeferredBidirectionalLabeledError::Reverse)?;
        if self.forward.labeled_leaf_segment_is_dense(src) {
            self.mark_compact_dense_labeled_vertex_maintenance(Orientation::Forward, src)?;
        }
        if self.reverse.labeled_leaf_segment_is_dense(dst) {
            self.mark_compact_dense_labeled_vertex_maintenance(Orientation::Reverse, dst)?;
        }
        Ok(())
    }

    /// Iterates forward outgoing edges for one label.
    pub fn iter_out_edges_for_label(
        &self,
        src: VertexId,
        label_id: LabelId,
    ) -> Result<Vec<E>, DeferredBidirectionalLabeledError> {
        self.forward
            .iter_edges_for_label(src, label_id)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Processes queued maintenance work up to `budget`.
    pub fn maintenance(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<BidirectionalMaintenanceReport, DeferredBidirectionalLabeledError> {
        let mut report = BidirectionalMaintenanceReport::default();
        let max_items = budget.max_work_items.unwrap_or(u32::MAX);
        while report.work.processed_work_items < max_items {
            let Some(item) = self.maintenance.pop_next()? else {
                break;
            };
            report.work.processed_work_items = report.work.processed_work_items.saturating_add(1);
            match item {
                MaintenanceWorkItem::CompactLabelBucketVertexSegment { orientation, vid } => {
                    let graph = match orientation {
                        Orientation::Forward => &self.forward,
                        Orientation::Reverse => &self.reverse,
                    };
                    if graph.compact_label_bucket_vertex_segment(vid).is_ok() {
                        report.work.rebalanced_segments =
                            report.work.rebalanced_segments.saturating_add(1);
                    }
                }
                MaintenanceWorkItem::CompactVertexEdgeSpan {
                    orientation,
                    vid,
                    bucket_index,
                } => {
                    let graph = match orientation {
                        Orientation::Forward => &self.forward,
                        Orientation::Reverse => &self.reverse,
                    };
                    if graph.compact_vertex_edge_span(vid, bucket_index).is_ok() {
                        report.work.rebalanced_segments =
                            report.work.rebalanced_segments.saturating_add(1);
                    }
                }
                MaintenanceWorkItem::CompactDenseLabeledVertexMaintenance { orientation, vid } => {
                    let graph = match orientation {
                        Orientation::Forward => &self.forward,
                        Orientation::Reverse => &self.reverse,
                    };
                    if graph.compact_label_bucket_vertex_segment(vid).is_ok() {
                        report.work.rebalanced_segments =
                            report.work.rebalanced_segments.saturating_add(1);
                    }
                    if graph.compact_vertex_edge_span(vid, 0).is_ok() {
                        report.work.rebalanced_segments =
                            report.work.rebalanced_segments.saturating_add(1);
                    }
                }
            }
            self.maintenance.complete(item)?;
        }
        report.work.remaining_queue_len = self.maintenance.len();
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_support::{labeled_lara_memories, vector_memory},
        traits::CsrEdge,
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

    use crate::VectorMemory;

    fn graph() -> DeferredBidirectionalLabeledLaraGraph<TestEdge, VectorMemory> {
        let (fv, fb, fbfs, fbfsbs, fec, fe, fel, fesm, fefs, fefsbs) = labeled_lara_memories();
        let (rv, rb, rbfs, rbfsbs, rec, re, rel, resm, refs, refsbs) = labeled_lara_memories();
        DeferredBidirectionalLabeledLaraGraph::new(
            fv,
            fb,
            fbfs,
            fbfsbs,
            fec,
            fe,
            fel,
            fesm,
            fefs,
            fefsbs,
            rv,
            rb,
            rbfs,
            rbfsbs,
            rec,
            re,
            rel,
            resm,
            refs,
            refsbs,
            vector_memory(),
            vector_memory(),
            128,
            LabelId::from_raw(1),
        )
        .expect("graph")
    }

    #[test]
    fn deferred_bidirectional_uses_one_shared_queue() {
        let graph = graph();
        graph.push_vertex().expect("vertex");
        graph
            .mark_compact_label_bucket_vertex_segment(Orientation::Forward, VertexId::from(0))
            .expect("mark");
        graph
            .mark_compact_label_bucket_vertex_segment(Orientation::Reverse, VertexId::from(0))
            .expect("mark");
        assert_eq!(graph.maintenance_queue_len(), 2);
        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: Some(1),
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("maintenance");
        assert_eq!(report.work.processed_work_items, 1);
        assert_eq!(report.remaining_queue_len(), 1);
    }

    #[test]
    fn deferred_bidirectional_propagates_vertex_edge_span_compaction() {
        let graph = graph();
        graph.push_vertex().expect("src");
        graph.push_vertex().expect("dst");
        let label = LabelId::from_raw(2);
        for _ in 0..80 {
            graph
                .insert_directed_edge(
                    VertexId::from(0),
                    VertexId::from(1),
                    label,
                    TestEdge(1),
                    TestEdge(0),
                )
                .unwrap();
        }
        for _ in 0..72 {
            graph
                .forward()
                .remove_edge_matching(VertexId::from(0), label, |edge| edge.0 == 1)
                .unwrap();
        }
        let before = graph.forward().vertices().get(VertexId::from(0));
        assert!(before.vertex_edge_alloc_slots() > 8);

        graph
            .mark_compact_vertex_edge_span(Orientation::Forward, VertexId::from(0), 0)
            .expect("mark");
        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: Some(1),
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("maintenance");

        assert_eq!(report.work.processed_work_items, 1);
        let after = graph.forward().vertices().get(VertexId::from(0));
        assert_eq!(after.vertex_edge_alloc_slots(), 8);
    }
}
