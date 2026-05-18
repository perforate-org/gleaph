//! Deferred-maintenance bidirectional labeled LARA graph wrapper.

use crate::{
    VertexCount, VertexId,
    labeled::{
        BucketLabelKey,
        bucket_label_key::BucketDirectedness,
        graph::{InitError, LabeledLaraGraph, LabeledOperationError, OutEdgeOrder},
    },
    lara::maintenance::{
        DeferredConfig, DeferredConfigError, MaintenanceBudget, MaintenanceWorkReport,
    },
    traits::{CsrEdge, CsrEdgeSlabVacancy, CsrVertex},
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
    E: CsrEdge + CsrEdgeSlabVacancy,
    M: Memory,
{
    forward: LabeledLaraGraph<E, M>,
    reverse: LabeledLaraGraph<E, M>,
    maintenance: BidirectionalMaintenanceQueue<M>,
    config: DeferredConfig,
}

impl<E, M> DeferredBidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdge + CsrEdgeSlabVacancy,
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
        default_label: BucketLabelKey,
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
        default_label: BucketLabelKey,
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
        default_label: BucketLabelKey,
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
        default_label: BucketLabelKey,
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
        label_id: BucketLabelKey,
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

    /// Visits forward outgoing edges for one label without materializing the bucket row.
    pub fn for_each_out_edges_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.forward
            .for_each_edges_for_label(src, label_id, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Visits forward outgoing edges for one label in `order`.
    pub fn for_each_out_edges_for_label_ordered<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.forward
            .for_each_edges_for_label_ordered(src, label_id, order, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Like [`LabeledLaraGraph::skip_then_visit_each_out_edge_for_label`] on the forward store.
    pub fn skip_then_visit_each_forward_out_edge_for_label<Visit, Err>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E) -> Result<bool, Err>,
    {
        self.forward
            .skip_then_visit_each_out_edge_for_label(src, label_id, offset_remaining, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Like [`LabeledLaraGraph::skip_then_visit_each_out_edge_by_directedness`] on the forward store.
    pub fn skip_then_visit_each_forward_out_edge_by_directedness<Visit, Err>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E) -> Result<bool, Err>,
    {
        self.forward
            .skip_then_visit_each_out_edge_by_directedness(
                src,
                directedness,
                offset_remaining,
                visit,
            )
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Like [`LabeledLaraGraph::skip_then_visit_each_out_edge_for_label`] on the reverse store.
    pub fn skip_then_visit_each_reverse_out_edge_for_label<Visit, Err>(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E) -> Result<bool, Err>,
    {
        self.reverse
            .skip_then_visit_each_out_edge_for_label(dst, label_id, offset_remaining, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Like [`LabeledLaraGraph::skip_then_visit_each_out_edge_by_directedness`] on the reverse store.
    pub fn skip_then_visit_each_reverse_out_edge_by_directedness<Visit, Err>(
        &self,
        dst: VertexId,
        directedness: BucketDirectedness,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E) -> Result<bool, Err>,
    {
        self.reverse
            .skip_then_visit_each_out_edge_by_directedness(
                dst,
                directedness,
                offset_remaining,
                visit,
            )
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Forward outgoing edges filtered by label-bucket directedness in `order`.
    ///
    /// See [`LabeledLaraGraph::for_each_out_edges_by_directedness`].
    pub fn for_each_out_edges_by_directedness<Visit>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.forward
            .for_each_out_edges_by_directedness(src, directedness, order, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Like [`Self::for_each_out_edges_by_directedness`], but skips forward vertex range validation.
    pub fn for_each_out_edges_by_directedness_unchecked<Visit>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.forward
            .for_each_out_edges_by_directedness_unchecked(src, directedness, order, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Reverse orientation: visits edges at `dst` filtered by directedness (incoming to `dst` forward).
    pub fn for_each_in_edges_by_directedness<Visit>(
        &self,
        dst: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.reverse
            .for_each_out_edges_by_directedness(dst, directedness, order, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Like [`Self::for_each_in_edges_by_directedness`], but skips reverse vertex range validation.
    pub fn for_each_in_edges_by_directedness_unchecked<Visit>(
        &self,
        dst: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.reverse
            .for_each_out_edges_by_directedness_unchecked(dst, directedness, order, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Visits reverse outgoing edges at `dst` (incoming to `dst` in forward orientation).
    pub fn for_each_in_edges_for_label<Visit>(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.reverse
            .for_each_edges_for_label(dst, label_id, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Visits reverse outgoing edges for one label in `order`.
    pub fn for_each_in_edges_for_label_ordered<Visit>(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.reverse
            .for_each_edges_for_label_ordered(dst, label_id, order, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Like [`Self::for_each_out_edges_for_label`], but skips vertex range validation on `src`.
    ///
    /// See [`LabeledLaraGraph::for_each_edges_for_label_unchecked`].
    pub fn for_each_out_edges_for_label_unchecked<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.forward
            .for_each_edges_for_label_unchecked(src, label_id, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Like [`Self::for_each_in_edges_for_label`], but skips vertex range validation on `dst`.
    pub fn for_each_in_edges_for_label_unchecked<Visit>(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.reverse
            .for_each_edges_for_label_unchecked(dst, label_id, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Forward outgoing edges for one label in descending scan order.
    pub fn out_edges_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<Vec<E>, DeferredBidirectionalLabeledError> {
        self.forward
            .iter_edges_for_label(src, label_id)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Incoming edges for one label in descending scan order.
    pub fn in_edges_for_label(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<Vec<E>, DeferredBidirectionalLabeledError> {
        self.reverse
            .iter_edges_for_label(dst, label_id)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Finds which forward label bucket contains `needle` at `src`.
    pub fn find_forward_edge_label(
        &self,
        src: VertexId,
        needle: &E,
    ) -> Result<Option<BucketLabelKey>, DeferredBidirectionalLabeledError>
    where
        E: PartialEq,
    {
        self.forward
            .find_edge_label(src, needle)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Finds the first forward outgoing edge accepted by `pred` in [`Self::asc_out_edges`]
    /// order, together with its label bucket id when applicable.
    pub fn find_forward_out_edge_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        pred: F,
    ) -> Result<Option<(E, Option<BucketLabelKey>)>, DeferredBidirectionalLabeledError>
    where
        F: FnMut(&E) -> bool,
    {
        self.forward
            .find_out_edge_with_label_by_predicate(src, pred)
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

    /// Shared vertex count after validating both orientations match.
    pub fn vertex_count_checked(&self) -> Result<VertexCount, DeferredBidirectionalLabeledError> {
        let forward = self.forward.vertex_count();
        let reverse = self.reverse.vertex_count();
        if forward != reverse {
            return Err(DeferredBidirectionalLabeledError::VertexCountMismatch {
                forward,
                reverse,
            });
        }
        Ok(forward)
    }

    /// Returns the forward vertex column length (mirrors unlabeled deferred graphs).
    ///
    /// Callers that need an integrity check should use [`Self::vertex_count_checked`].
    #[inline]
    pub fn vertex_count(&self) -> VertexCount {
        self.forward.vertex_count()
    }

    /// Appends one synchronized vertex row to both orientations.
    pub fn push_vertex_row(
        &self,
        row: crate::labeled::record::LabeledVertex,
    ) -> Result<VertexId, DeferredBidirectionalLabeledError> {
        let _ = self.vertex_count_checked()?;
        self.forward
            .push_vertex(row)
            .map_err(DeferredBidirectionalLabeledError::Grow)?;
        self.reverse
            .push_vertex(row)
            .map_err(DeferredBidirectionalLabeledError::Grow)?;
        Ok(VertexId::from(
            self.forward.vertex_count().0.saturating_sub(1),
        ))
    }

    /// Reads the forward vertex row for `vid`.
    pub fn vertex_row(
        &self,
        vid: VertexId,
    ) -> Result<crate::labeled::record::LabeledVertex, DeferredBidirectionalLabeledError> {
        let len = self.forward.vertex_count();
        if u32::from(vid) >= len.0 {
            return Err(DeferredBidirectionalLabeledError::VertexOutOfRange { vid, len });
        }
        Ok(self.forward.vertices().get(vid))
    }

    /// Writes the same vertex row into both orientations.
    pub fn set_vertex_row(
        &self,
        vid: VertexId,
        row: &crate::labeled::record::LabeledVertex,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        let len = self.forward.vertex_count();
        if u32::from(vid) >= len.0 {
            return Err(DeferredBidirectionalLabeledError::VertexOutOfRange { vid, len });
        }
        self.forward.vertices().set(vid, row);
        self.reverse.vertices().set(vid, row);
        Ok(())
    }

    /// All forward outgoing edges in default **descending** scan order (see [`LabeledLaraGraph::out_edges`]).
    #[inline]
    pub fn out_edges(&self, src: VertexId) -> Result<Vec<E>, DeferredBidirectionalLabeledError> {
        self.forward
            .out_edges(src)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Incoming edges to `dst` in the forward orientation (reverse CSR at `dst`), descending scan order.
    #[inline]
    pub fn in_edges(&self, dst: VertexId) -> Result<Vec<E>, DeferredBidirectionalLabeledError> {
        self.reverse
            .out_edges(dst)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// All forward outgoing edges in ascending slot/materialization order ([`LabeledLaraGraph::asc_out_edges`]).
    #[inline]
    pub fn asc_out_edges(
        &self,
        src: VertexId,
    ) -> Result<Vec<E>, DeferredBidirectionalLabeledError> {
        self.forward
            .asc_out_edges(src)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Incoming edges in ascending slot/materialization order on the reverse orientation.
    #[inline]
    pub fn asc_in_edges(&self, dst: VertexId) -> Result<Vec<E>, DeferredBidirectionalLabeledError> {
        self.reverse
            .asc_out_edges(dst)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// `true` when `src` has at least one outgoing edge in the forward orientation.
    pub fn has_out_edges(&self, src: VertexId) -> Result<bool, DeferredBidirectionalLabeledError> {
        let len = self.vertex_count_checked()?;
        if u32::from(src) >= len.0 {
            return Err(DeferredBidirectionalLabeledError::VertexOutOfRange { vid: src, len });
        }
        Ok(self.forward.vertices().get(src).degree() > 0)
    }

    /// `true` when `dst` has at least one incoming edge in the forward orientation.
    pub fn has_in_edges(&self, dst: VertexId) -> Result<bool, DeferredBidirectionalLabeledError> {
        let len = self.vertex_count_checked()?;
        if u32::from(dst) >= len.0 {
            return Err(DeferredBidirectionalLabeledError::VertexOutOfRange { vid: dst, len });
        }
        Ok(self.reverse.vertices().get(dst).degree() > 0)
    }

    /// Visits forward outgoing edges with optional `offset` / `limit`, raw-byte prefilter, and match predicate.
    pub fn visit_out_edges<Match, Visit>(
        &self,
        vertex_id: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: Match,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        self.forward
            .visit_out_edges(vertex_id, offset, limit, raw_matches, matches, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Visits incoming edges with optional `offset` / `limit`, raw-byte prefilter, and match predicate.
    pub fn visit_in_edges<Match, Visit>(
        &self,
        vertex_id: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: Match,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        self.reverse
            .visit_out_edges(vertex_id, offset, limit, raw_matches, matches, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Streaming forward out-edge iterator in descending scan order.
    pub fn out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<
        crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>,
        DeferredBidirectionalLabeledError,
    > {
        self.forward
            .desc_out_edges_iter(src)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Streaming incoming-edge iterator in descending scan order.
    pub fn in_edges_iter(
        &self,
        dst: VertexId,
    ) -> Result<
        crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>,
        DeferredBidirectionalLabeledError,
    > {
        self.reverse
            .desc_out_edges_iter(dst)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Explicit descending-scan alias for [`Self::out_edges_iter`].
    pub fn desc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<
        crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>,
        DeferredBidirectionalLabeledError,
    > {
        self.out_edges_iter(src)
    }

    /// Explicit descending-scan alias for [`Self::in_edges_iter`].
    pub fn desc_in_edges_iter(
        &self,
        dst: VertexId,
    ) -> Result<
        crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>,
        DeferredBidirectionalLabeledError,
    > {
        self.in_edges_iter(dst)
    }

    /// Streaming forward out-edge iterator in ascending slot/materialization order.
    pub fn asc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<
        crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>,
        DeferredBidirectionalLabeledError,
    > {
        self.forward
            .asc_out_edges_iter(src)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Streaming incoming-edge iterator in ascending slot/materialization order.
    pub fn asc_in_edges_iter(
        &self,
        dst: VertexId,
    ) -> Result<
        crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>,
        DeferredBidirectionalLabeledError,
    > {
        self.reverse
            .asc_out_edges_iter(dst)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// `true` when `vid` has at least one incident edge in either orientation.
    pub fn has_incident_edges(
        &self,
        vid: VertexId,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        Ok(self.has_out_edges(vid)? || self.has_in_edges(vid)?)
    }

    /// Removes one directed logical edge by scanning every forward label bucket.
    pub fn remove_directed_deferred(
        &self,
        src: VertexId,
        dst: VertexId,
        edge: E,
    ) -> Result<bool, DeferredBidirectionalLabeledError>
    where
        E: PartialEq,
    {
        let labels = self
            .forward
            .out_edge_label_ids(src)
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        for label_id in labels {
            let removed = self
                .forward
                .remove_edge_matching(src, label_id, |cand| {
                    *cand == edge && cand.neighbor_vid() == dst
                })
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
            if let Some(removed) = removed {
                let rev = removed.with_neighbor_vid(src);
                let _ = self
                    .reverse
                    .remove_edge_matching(dst, label_id, |cand| {
                        *cand == rev && cand.neighbor_vid() == src
                    })
                    .map_err(DeferredBidirectionalLabeledError::Reverse)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Removes one undirected logical edge (four physical inserts' inverse).
    pub fn remove_undirected_deferred(
        &self,
        u: VertexId,
        v: VertexId,
        edge_at_u: E,
    ) -> Result<bool, DeferredBidirectionalLabeledError>
    where
        E: PartialEq,
    {
        let ok_uv = self.remove_directed_deferred(u, v, edge_at_u)?;
        let edge_at_v = edge_at_u.with_neighbor_vid(u);
        let ok_vu = self.remove_directed_deferred(v, u, edge_at_v)?;
        Ok(ok_uv || ok_vu)
    }

    /// Queued incremental vertex deletion: removes all incident edges then clears the row.
    pub fn delete_vertex_deferred(
        &self,
        vid: VertexId,
    ) -> Result<bool, DeferredBidirectionalLabeledError>
    where
        E: PartialEq,
    {
        while self.has_incident_edges(vid)? {
            if let Some(edge) = self.asc_out_edges(vid)?.into_iter().next() {
                let dst = edge.neighbor_vid();
                let _ = self.remove_directed_deferred(vid, dst, edge)?;
                continue;
            }
            if let Some(edge) = self.asc_in_edges(vid)?.into_iter().next() {
                let src = edge.neighbor_vid();
                let rev = edge.with_neighbor_vid(vid);
                let _ = self.remove_directed_deferred(src, vid, rev)?;
                continue;
            }
            break;
        }
        let forward_row = self.forward.vertices().get(vid);
        if !forward_row.is_default_edge_labeled() {
            self.forward
                .clear_vertex_label_buckets_for_segment(vid)
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
        }
        let reverse_row = self.reverse.vertices().get(vid);
        if !reverse_row.is_default_edge_labeled() {
            self.reverse
                .clear_vertex_label_buckets_for_segment(vid)
                .map_err(DeferredBidirectionalLabeledError::Reverse)?;
        }
        let len = self.forward.vertex_count();
        if u32::from(vid) < len.0 {
            let cleared = crate::labeled::record::LabeledVertex::default().with_tombstone(true);
            self.set_vertex_row(vid, &cleared)?;
        }
        Ok(true)
    }

    /// Undirected insert matching the four-orientation materialization used by core LARA.
    pub fn insert_undirected_deferred(
        &self,
        u: VertexId,
        v: VertexId,
        label_id: BucketLabelKey,
        edge_uv: E,
        edge_vu: E,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.insert_directed_edge(u, v, label_id, edge_uv, edge_vu)?;
        self.insert_directed_edge(v, u, label_id, edge_vu, edge_uv)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_support::{labeled_lara_memories, vector_memory},
        traits::{CsrEdge, CsrEdgeSlabVacancy},
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

    impl CsrEdgeSlabVacancy for TestEdge {
        fn slab_vacant_edge() -> Self {
            Self(u32::from(crate::VertexId::SLAB_VACANT))
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
            BucketLabelKey::from_raw(1),
        )
        .expect("graph")
    }

    #[test]
    fn delete_vertex_after_mixed_bucket_and_bypass_edge() {
        let (fv, fb, fbfs, fbfsbs, fec, fe, fel, fesm, fefs, fefsbs) = labeled_lara_memories();
        let (rv, rb, rbfs, rbfsbs, rec, re, rel, resm, refs, refsbs) = labeled_lara_memories();
        let graph = DeferredBidirectionalLabeledLaraGraph::new(
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
            BucketLabelKey::UNLABELED_DIRECTED,
        )
        .expect("graph");
        graph.push_vertex().expect("a");
        graph.push_vertex().expect("b");
        graph
            .insert_directed_edge(
                VertexId::from(0),
                VertexId::from(1),
                BucketLabelKey::UNLABELED_DIRECTED,
                TestEdge(1),
                TestEdge(0),
            )
            .expect("edge");
        graph
            .delete_vertex_deferred(VertexId::from(0))
            .expect("delete");
        graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("drain");
        assert!(
            !graph
                .has_incident_edges(VertexId::from(0))
                .expect("incident")
        );
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
    fn visit_out_edges_descending_matches_asc_slot_order_materialization() {
        let graph = graph();
        graph.push_vertex().expect("a");
        graph.push_vertex().expect("b");
        graph.push_vertex().expect("c");
        let label_lo = BucketLabelKey::from_raw(10);
        let label_hi = BucketLabelKey::from_raw(20);
        graph
            .insert_directed_edge(
                VertexId::from(0),
                VertexId::from(1),
                label_lo,
                TestEdge(1),
                TestEdge(0),
            )
            .unwrap();
        graph
            .insert_directed_edge(
                VertexId::from(0),
                VertexId::from(2),
                label_hi,
                TestEdge(2),
                TestEdge(0),
            )
            .unwrap();
        let collected = graph.asc_out_edges(VertexId::from(0)).unwrap();
        let mut streamed = Vec::new();
        graph
            .visit_out_edges(
                VertexId::from(0),
                None,
                None,
                None,
                |_| true,
                |e| streamed.push(e),
            )
            .unwrap();
        assert_eq!(collected, vec![TestEdge(1), TestEdge(2)]);
        assert_eq!(
            streamed,
            graph.forward.out_edges(VertexId::from(0)).unwrap(),
            "visit_out_edges follows descending scan per span"
        );
    }

    #[test]
    fn deferred_bidirectional_propagates_vertex_edge_span_compaction() {
        let graph = graph();
        graph.push_vertex().expect("dst");
        graph.push_vertex().expect("src");
        let hub = VertexId::from(1);
        let dst = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        graph
            .insert_directed_edge(
                hub,
                dst,
                BucketLabelKey::from_raw(99),
                TestEdge(0),
                TestEdge(0),
            )
            .unwrap();
        for _ in 0..80 {
            graph
                .insert_directed_edge(hub, dst, label, TestEdge(1), TestEdge(0))
                .unwrap();
        }
        for _ in 0..72 {
            graph
                .forward()
                .remove_edge_matching(hub, label, |edge| edge.0 == 1)
                .unwrap();
        }
        let before = graph.forward().vertices().get(hub);
        assert!(before.vertex_edge_alloc_slots() > 8);

        graph
            .mark_compact_vertex_edge_span(Orientation::Forward, hub, 0)
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
        let after = graph.forward().vertices().get(hub);
        assert_eq!(after.vertex_edge_alloc_slots(), 9);
    }
}
