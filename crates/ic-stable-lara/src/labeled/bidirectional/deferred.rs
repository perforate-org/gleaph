//! Deferred-maintenance bidirectional labeled LARA graph wrapper.
//!
//! Directed vs undirected adjacency is selected by [`BucketLabelKey`] / [`BucketDirectedness`]
//! (bucket MSB), not edge-inline-value flags. Use [`Self::for_each_directed_out_edges`],
//! [`Self::for_each_undirected_edges`], and the matching `*_iter` helpers.

use crate::{
    VertexCount, VertexId,
    labeled::{
        BucketLabelKey, InitialCapacities,
        bucket_label_key::BucketDirectedness,
        graph::batch_write::BatchReservation,
        graph::{
            EdgeRemoval, EdgeSlotMove, InitError, LabeledLaraGraph, LabeledOperationError,
            OutEdgeOrder, ScalarInsertLocation,
        },
    },
    lara::maintenance::{
        DeferredConfig, DeferredConfigError, MaintenanceBudget, MaintenanceWorkReport,
    },
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
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
    /// Instruction-counter delta for this maintenance call (wasm only).
    pub instructions_used: u64,
    /// Whether [`MaintenanceBudget::max_instructions`] stopped the run early.
    pub instruction_budget_exhausted: bool,
}

/// Exact scalar insertion locations for the two directed orientations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScalarInsertPair {
    /// Location in the forward/source row.
    pub forward: Option<ScalarInsertLocation>,
    /// Location in the reverse/target row.
    pub reverse: Option<ScalarInsertLocation>,
}

/// Observer for edge slot relocations produced by labeled row compaction.
pub trait EdgeSlotMoveObserver {
    /// Called after a row compaction has moved one live edge slot.
    fn edge_slot_moved(&mut self, orientation: Orientation, vid: VertexId, moved: EdgeSlotMove);
}

struct NoopEdgeSlotMoveObserver;

impl EdgeSlotMoveObserver for NoopEdgeSlotMoveObserver {
    fn edge_slot_moved(&mut self, _orientation: Orientation, _vid: VertexId, _moved: EdgeSlotMove) {
    }
}

/// Observer for edges removed by resumable [`MaintenanceWorkItem::DeleteVertex`]
/// jobs, and for the completion of a vertex purge (ADR 0021).
pub trait DeleteEdgeObserver<E> {
    /// Called when a delete step removes one outgoing edge of the deleted vertex.
    fn on_delete_outgoing_edge(&mut self, _source: VertexId, _edge: E) {}

    /// Called when a delete step removes one incoming edge of the deleted vertex.
    fn on_delete_incoming_edge(&mut self, _destination: VertexId, _edge: E) {}

    /// Called once after the deleted vertex's rows are cleared and it is tombstoned.
    fn on_vertex_purge_completed(&mut self, _vid: VertexId) {}
}

struct NoopDeleteEdgeObserver;

impl<E> DeleteEdgeObserver<E> for NoopDeleteEdgeObserver {}

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
    /// Compact one VertexEdgeSpan in one orientation (incremental; one edge step per queue pop).
    CompactVertexEdgeSpan {
        /// Orientation whose VertexEdgeSpan should be compacted.
        orientation: Orientation,
        /// Vertex owning the VertexEdgeSpan.
        vid: VertexId,
        /// Label-bucket index used to validate that the work item is still relevant.
        anchor_bucket_index: u32,
        /// Next label-bucket index to compact (0 at enqueue time).
        resume_bucket_index: u32,
    },
    /// Compact the label-bucket vertex segment then the vertex edge span for one orientation.
    CompactDenseLabeledVertexMaintenance {
        /// Orientation whose stores should be compacted.
        orientation: Orientation,
        /// Vertex to compact.
        vid: VertexId,
    },
    /// Reserved stable tag for independently scheduled value-span maintenance.
    CompactVertexValueSpan {
        orientation: Orientation,
        vid: VertexId,
    },
    /// Legacy stable tag. It now advances edge compaction only; value maintenance is independent.
    CompactVertexEdgeAndValueSpan {
        orientation: Orientation,
        vid: VertexId,
        anchor_bucket_index: u32,
        resume_bucket_index: u32,
    },
    /// Compact the payload slab when aggregate free space is fragmented.
    CompactPayloadSlab {
        /// Orientation whose payload slab should be compacted.
        orientation: Orientation,
    },
    /// Incrementally remove all incident edges of a deleted vertex, one edge per
    /// step, then clear its rows. Resumable across maintenance calls (ADR 0021).
    DeleteVertex {
        /// Deleted vertex id.
        vid: VertexId,
        /// Incident edges already removed (informational; threaded across steps).
        removed_edges: u32,
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
            anchor_bucket_index,
            resume_bucket_index,
        } => {
            b[0] = 1;
            b[1] = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            b[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
            b[8..12].copy_from_slice(&anchor_bucket_index.to_le_bytes());
            b[12..16].copy_from_slice(&resume_bucket_index.to_le_bytes());
        }
        MaintenanceWorkItem::CompactDenseLabeledVertexMaintenance { orientation, vid } => {
            b[0] = 2;
            b[1] = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            b[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
        }
        MaintenanceWorkItem::CompactVertexValueSpan { orientation, vid } => {
            b[0] = 3;
            b[1] = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            b[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
        }
        MaintenanceWorkItem::CompactVertexEdgeAndValueSpan {
            orientation,
            vid,
            anchor_bucket_index,
            resume_bucket_index,
        } => {
            b[0] = 4;
            b[1] = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            b[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
            b[8..12].copy_from_slice(&anchor_bucket_index.to_le_bytes());
            b[12..16].copy_from_slice(&resume_bucket_index.to_le_bytes());
        }
        MaintenanceWorkItem::DeleteVertex { vid, removed_edges } => {
            b[0] = 5;
            b[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
            b[8..12].copy_from_slice(&removed_edges.to_le_bytes());
        }
        MaintenanceWorkItem::CompactPayloadSlab { orientation } => {
            b[0] = 6;
            b[1] = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
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
                anchor_bucket_index: u32::from_le_bytes(b[8..12].try_into().unwrap()),
                resume_bucket_index: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            },
            2 => Self::CompactDenseLabeledVertexMaintenance { orientation, vid },
            3 => Self::CompactVertexValueSpan { orientation, vid },
            4 => Self::CompactVertexEdgeAndValueSpan {
                orientation,
                vid,
                anchor_bucket_index: u32::from_le_bytes(b[8..12].try_into().unwrap()),
                resume_bucket_index: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            },
            5 => Self::DeleteVertex {
                vid,
                removed_edges: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            },
            6 => Self::CompactPayloadSlab { orientation },
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

    /// Enqueues a `DeleteVertex` job. Delete jobs bypass the dirty gate (see
    /// [`Self::pop_next`]), so this pushes directly without a dirty key.
    fn enqueue_delete_vertex(
        &self,
        item: MaintenanceWorkItem,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.queue
            .push_back(&item)
            .map_err(DeferredBidirectionalLabeledError::MaintenanceQueue)
    }

    fn pop_next(&self) -> Result<Option<MaintenanceWorkItem>, DeferredBidirectionalLabeledError> {
        while let Some(item) = self.queue.pop_front() {
            // DeleteVertex bypasses the dirty gate: its `work_item_key` would land
            // in the same high-bit ranges as the compaction keys, so a colliding
            // compaction `complete` could clear it and silently drop a delete
            // mid-job. Delete jobs are never deduped via the dirty bitmap.
            if matches!(item, MaintenanceWorkItem::DeleteVertex { .. })
                || self.dirty.contains(work_item_key(item))
            {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    fn complete(&self, item: MaintenanceWorkItem) -> Result<(), DeferredBidirectionalLabeledError> {
        if matches!(item, MaintenanceWorkItem::DeleteVertex { .. }) {
            return Ok(());
        }
        self.dirty
            .clear(work_item_key(item))
            .map_err(DeferredBidirectionalLabeledError::MaintenanceDirtyBitmap)
    }

    fn requeue_front(
        &self,
        item: MaintenanceWorkItem,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.queue
            .push_front(&item)
            .map_err(DeferredBidirectionalLabeledError::MaintenanceQueue)
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
            anchor_bucket_index,
            resume_bucket_index: _,
        } => {
            let orient = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            0x4000_0000 | anchor_bucket_index ^ (u32::from(vid) << 1) ^ orient
        }
        MaintenanceWorkItem::CompactDenseLabeledVertexMaintenance { orientation, vid } => {
            let orient = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            0xC000_0000u32 ^ u32::from(vid).wrapping_mul(2_654_435_761) ^ orient
        }
        MaintenanceWorkItem::CompactVertexValueSpan { orientation, vid } => {
            let orient = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            0x8000_0000 | u32::from(vid).wrapping_mul(2) ^ orient
        }
        MaintenanceWorkItem::CompactVertexEdgeAndValueSpan {
            orientation,
            vid,
            anchor_bucket_index,
            resume_bucket_index: _,
        } => {
            let orient = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            0xA000_0000 | anchor_bucket_index ^ (u32::from(vid) << 1) ^ orient
        }
        MaintenanceWorkItem::CompactPayloadSlab { orientation } => match orientation {
            Orientation::Forward => 0x6000_0000,
            Orientation::Reverse => 0x6000_0001,
        },
        // DeleteVertex bypasses the dirty gate, so this key is never inserted,
        // checked, or cleared; it exists only to keep the match exhaustive.
        MaintenanceWorkItem::DeleteVertex { vid, .. } => 0xE000_0000 | u32::from(vid),
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
    E: CsrEdge + CsrEdgeTombstone,
    M: Memory,
{
    forward: LabeledLaraGraph<E, M>,
    reverse: LabeledLaraGraph<E, M>,
    maintenance: BidirectionalMaintenanceQueue<M>,
    config: DeferredConfig,
}

fn edge_matches_remove_target<E>(candidate: &E, expected: &E, neighbor: VertexId) -> bool
where
    E: CsrEdge + PartialEq,
{
    if candidate.neighbor_vid() != neighbor {
        return false;
    }
    let width = expected.edge_inline_value_byte_width();
    if width != 0 {
        return candidate.edge_inline_value_byte_width() == width
            && candidate.edge_inline_value_bytes() == expected.edge_inline_value_bytes();
    }
    candidate.edge_slot_index_raw() == expected.edge_slot_index_raw()
}

impl<E, M> DeferredBidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdge + CsrEdgeTombstone,
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
        forward_inline_value_slab: M,
        forward_payload_free_spans: M,
        forward_payload_free_span_by_start: M,
        forward_payload_log: M,
        forward_payload_blobs: M,
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
        reverse_inline_value_slab: M,
        reverse_payload_free_spans: M,
        reverse_payload_free_span_by_start: M,
        reverse_payload_log: M,
        reverse_payload_blobs: M,
        maintenance_queue: M,
        dirty_work_items: M,
        capacities: InitialCapacities,
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
            forward_inline_value_slab,
            forward_payload_free_spans,
            forward_payload_free_span_by_start,
            forward_payload_log,
            forward_payload_blobs,
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
            reverse_inline_value_slab,
            reverse_payload_free_spans,
            reverse_payload_free_span_by_start,
            reverse_payload_log,
            reverse_payload_blobs,
            maintenance_queue,
            dirty_work_items,
            capacities,
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
        forward_inline_value_slab: M,
        forward_payload_free_spans: M,
        forward_payload_free_span_by_start: M,
        forward_payload_log: M,
        forward_payload_blobs: M,
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
        reverse_inline_value_slab: M,
        reverse_payload_free_spans: M,
        reverse_payload_free_span_by_start: M,
        reverse_payload_log: M,
        reverse_payload_blobs: M,
        maintenance_queue: M,
        dirty_work_items: M,
        capacities: InitialCapacities,
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
            forward_inline_value_slab,
            forward_payload_free_spans,
            forward_payload_free_span_by_start,
            forward_payload_log,
            forward_payload_blobs,
            capacities,
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
            reverse_inline_value_slab,
            reverse_payload_free_spans,
            reverse_payload_free_span_by_start,
            reverse_payload_log,
            reverse_payload_blobs,
            capacities,
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
        forward_inline_value_slab: M,
        forward_payload_free_spans: M,
        forward_payload_free_span_by_start: M,
        forward_payload_log: M,
        forward_payload_blobs: M,
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
        reverse_inline_value_slab: M,
        reverse_payload_free_spans: M,
        reverse_payload_free_span_by_start: M,
        reverse_payload_log: M,
        reverse_payload_blobs: M,
        maintenance_queue: M,
        dirty_work_items: M,
        capacities: InitialCapacities,
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
            forward_inline_value_slab,
            forward_payload_free_spans,
            forward_payload_free_span_by_start,
            forward_payload_log,
            forward_payload_blobs,
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
            reverse_inline_value_slab,
            reverse_payload_free_spans,
            reverse_payload_free_span_by_start,
            reverse_payload_log,
            reverse_payload_blobs,
            maintenance_queue,
            dirty_work_items,
            capacities,
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
        forward_inline_value_slab: M,
        forward_payload_free_spans: M,
        forward_payload_free_span_by_start: M,
        forward_payload_log: M,
        forward_payload_blobs: M,
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
        reverse_inline_value_slab: M,
        reverse_payload_free_spans: M,
        reverse_payload_free_span_by_start: M,
        reverse_payload_log: M,
        reverse_payload_blobs: M,
        maintenance_queue: M,
        dirty_work_items: M,
        capacities: InitialCapacities,
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
            forward_inline_value_slab,
            forward_payload_free_spans,
            forward_payload_free_span_by_start,
            forward_payload_log,
            forward_payload_blobs,
            capacities,
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
            reverse_inline_value_slab,
            reverse_payload_free_spans,
            reverse_payload_free_span_by_start,
            reverse_payload_log,
            reverse_payload_blobs,
            capacities,
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

    /// Read-only placement metadata for an existing forward label bucket.
    ///
    /// See [`crate::labeled::graph::LabelBucketPlacementInfo`].
    pub fn read_forward_bucket_placement_info(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<
        Option<crate::labeled::graph::LabelBucketPlacementInfo>,
        DeferredBidirectionalLabeledError,
    > {
        self.forward
            .read_label_bucket_placement_info(src, label_id)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Read-only placement metadata for an existing reverse label bucket.
    pub fn read_reverse_bucket_placement_info(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<
        Option<crate::labeled::graph::LabelBucketPlacementInfo>,
        DeferredBidirectionalLabeledError,
    > {
        self.reverse
            .read_label_bucket_placement_info(src, label_id)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }
    /// Read-only aggregate placement metadata for every bucket on one forward PMA leaf.
    pub fn read_forward_leaf_placement_stats(
        &self,
        leaf: u32,
    ) -> Result<crate::labeled::graph::LeafBucketPlacementStats, DeferredBidirectionalLabeledError>
    {
        self.forward
            .read_leaf_placement_stats(leaf)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Read-only aggregate placement metadata for every bucket on one reverse PMA leaf.
    pub fn read_reverse_leaf_placement_stats(
        &self,
        leaf: u32,
    ) -> Result<crate::labeled::graph::LeafBucketPlacementStats, DeferredBidirectionalLabeledError>
    {
        self.reverse
            .read_leaf_placement_stats(leaf)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
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
                anchor_bucket_index: bucket_index,
                resume_bucket_index: 0,
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

    /// Enqueues payload-only compaction for one orientation.
    pub fn mark_compact_payload_slab(
        &self,
        orientation: Orientation,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.maintenance
            .mark_dirty(MaintenanceWorkItem::CompactPayloadSlab { orientation })
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
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        self.reverse
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .map_err(DeferredBidirectionalLabeledError::Reverse)?;
        Ok(VertexId::from(
            self.forward.vertex_count().0.saturating_sub(1),
        ))
    }

    /// Inserts one outgoing edge on the forward store only (remote / external targets).
    pub fn insert_forward_out_edge(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        forward_edge: E,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        let payload_compaction_needed = forward_edge.edge_inline_value_byte_width() != 0
            && self
                .forward
                .payload_compaction_needed(u64::from(forward_edge.edge_inline_value_byte_width()))
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
        self.forward
            .insert_edge_skip_leaf_cascade_deferred_payload(src, label_id, forward_edge)
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        if payload_compaction_needed {
            self.mark_compact_payload_slab(Orientation::Forward)?;
        }
        Ok(())
    }

    /// Ensures forward/reverse label buckets declare `inline_value_byte_width` for a directed insert.
    pub fn ensure_directed_edge_inline_value_width(
        &self,
        src: VertexId,
        dst: VertexId,
        label_id: BucketLabelKey,
        inline_value_byte_width: u16,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.forward
            .ensure_label_bucket_inline_value_byte_width(src, label_id, inline_value_byte_width)
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        self.reverse
            .ensure_label_bucket_inline_value_byte_width(dst, label_id, inline_value_byte_width)
            .map_err(DeferredBidirectionalLabeledError::Reverse)?;
        Ok(())
    }

    /// Ensures the forward out-adjacency label bucket declares `inline_value_byte_width`.
    pub fn ensure_forward_edge_inline_value_width(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        inline_value_byte_width: u16,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.forward
            .ensure_label_bucket_inline_value_byte_width(src, label_id, inline_value_byte_width)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Ensures both undirected forward-store endpoint buckets declare `inline_value_byte_width`.
    pub fn ensure_undirected_edge_inline_value_width(
        &self,
        u: VertexId,
        v: VertexId,
        label_id: BucketLabelKey,
        inline_value_byte_width: u16,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.forward
            .ensure_label_bucket_inline_value_byte_width(u, label_id, inline_value_byte_width)
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        if u != v {
            self.forward
                .ensure_label_bucket_inline_value_byte_width(v, label_id, inline_value_byte_width)
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
        }
        Ok(())
    }

    /// Roll back a one-orientation batch reservation without committing it.
    ///
    /// This consumes the reservation token, delegates to the forward or reverse
    /// labeled graph, and restores the edge-store logical capacity and payload
    /// occupied tail captured at reserve time.  Any payload bytes that were
    /// already appended are retired to the payload free-list as reusable slack;
    /// the underlying stable-memory pages are not shrunk.  Canonical adjacency
    /// and bucket metadata are untouched.
    pub fn rollback_batch_reservation(
        &self,
        orientation: Orientation,
        reservation: BatchReservation<E>,
    ) {
        match orientation {
            Orientation::Forward => reservation.rollback(&self.forward),
            Orientation::Reverse => reservation.rollback(&self.reverse),
        }
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
        self.insert_directed_edge_with_locations(src, dst, label_id, forward_edge, reverse_edge)
            .map(|_| ())
    }

    /// Inserts one directed edge and returns the exact locations captured by both writes.
    pub fn insert_directed_edge_with_locations(
        &self,
        src: VertexId,
        dst: VertexId,
        label_id: BucketLabelKey,
        forward_edge: E,
        reverse_edge: E,
    ) -> Result<ScalarInsertPair, DeferredBidirectionalLabeledError> {
        // Storage-owned capacity preparation: before any canonical edge write, make
        // sure both orientations have room for a new label bucket.  This keeps
        // ordinary writes writable when deferred leaf maintenance has not yet drained.
        self.forward
            .prepare_labeled_edge_capacity_for_insert(src, label_id)
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        self.reverse
            .prepare_labeled_edge_capacity_for_insert(dst, label_id)
            .map_err(DeferredBidirectionalLabeledError::Reverse)?;
        let forward_payload_compaction_needed = forward_edge.edge_inline_value_byte_width() != 0
            && self
                .forward
                .payload_compaction_needed(u64::from(forward_edge.edge_inline_value_byte_width()))
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
        let reverse_payload_compaction_needed = reverse_edge.edge_inline_value_byte_width() != 0
            && self
                .reverse
                .payload_compaction_needed(u64::from(reverse_edge.edge_inline_value_byte_width()))
                .map_err(DeferredBidirectionalLabeledError::Reverse)?;
        let forward_location = self
            .forward
            .insert_edge_skip_leaf_cascade_deferred_payload_with_location(
                src,
                label_id,
                forward_edge,
            )
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        let reverse_location = self
            .reverse
            .insert_edge_skip_leaf_cascade_deferred_payload_with_location(
                dst,
                label_id,
                reverse_edge,
            )
            .map_err(DeferredBidirectionalLabeledError::Reverse)?;
        if forward_payload_compaction_needed {
            self.mark_compact_payload_slab(Orientation::Forward)?;
        }
        if reverse_payload_compaction_needed {
            self.mark_compact_payload_slab(Orientation::Reverse)?;
        }
        if self.forward.labeled_leaf_segment_is_dense(src) {
            self.mark_compact_dense_labeled_vertex_maintenance(Orientation::Forward, src)?;
        }
        if self.reverse.labeled_leaf_segment_is_dense(dst) {
            self.mark_compact_dense_labeled_vertex_maintenance(Orientation::Reverse, dst)?;
        }
        Ok(ScalarInsertPair {
            forward: forward_location,
            reverse: reverse_location,
        })
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

    /// Like [`Self::for_each_out_edges_for_label_ordered`], but skips edge-inline-value reads.
    pub fn for_each_out_edges_for_label_topology_ordered<Visit>(
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
            .for_each_edges_for_label_topology_ordered(src, label_id, order, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Like [`Self::for_each_out_edges_for_label_topology_ordered`], but skips vertex checks.
    pub fn for_each_out_edges_for_label_topology_unchecked<Visit>(
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
            .for_each_edges_for_label_topology_unchecked(src, label_id, order, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Returns whether forward `(src, label_id)` supports dense payload-only phase 1.
    pub fn out_bucket_dense_inline_value_batch_eligible(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        self.forward
            .out_bucket_dense_inline_value_batch_eligible(src, label_id)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Returns whether forward predicate expand may use payload-first phase 1 + phase 2.
    pub fn out_bucket_inline_value_first_predicate_eligible(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        self.forward
            .out_bucket_inline_value_first_predicate_eligible(src, label_id)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Returns whether reverse `(dst, label_id)` supports dense payload-only phase 1.
    pub fn in_bucket_dense_inline_value_batch_eligible(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        self.reverse
            .out_bucket_dense_inline_value_batch_eligible(dst, label_id)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Returns whether reverse predicate expand may use payload-first phase 1 + phase 2.
    pub fn in_bucket_inline_value_first_predicate_eligible(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        self.reverse
            .out_bucket_inline_value_first_predicate_eligible(dst, label_id)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Visits forward outgoing payload bytes for one label in `order` (dense, hybrid, and sparse).
    pub fn visit_out_inline_value_batches_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut crate::labeled::LabeledPayloadValueBatchScratch,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(crate::labeled::LabeledPayloadValueBatch<'b>),
    {
        self.forward
            .visit_out_inline_value_batches_for_label(src, label_id, order, scratch, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Reads forward outgoing edge rows for the requested slot indices (topology only).
    pub fn read_out_edge_slots_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slots: &[u32],
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.forward
            .read_out_edge_slots_for_label(src, label_id, slots, order, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Like [`Self::read_out_edge_slots_for_label`], reusing hybrid overflow replay from phase 1.
    pub fn read_out_edge_slots_for_label_with_replay<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slots: &[u32],
        order: OutEdgeOrder,
        replay: Option<&crate::labeled::HybridOverflowEdgeReplay>,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.forward
            .read_out_edge_slots_for_label_with_replay(src, label_id, slots, order, replay, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Visits forward outgoing edges and parallel value bytes for one label in `order`.
    pub fn visit_out_edge_inline_value_batches_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut crate::labeled::LabeledEdgeInlineValueBatchScratch<E>,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(crate::labeled::LabeledEdgeInlineValueBatch<'b, E>),
    {
        self.forward
            .visit_out_edge_inline_value_batches_for_label(src, label_id, order, scratch, visit)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Visits reverse outgoing payload bytes for one label in `order` (dense, hybrid, and sparse).
    pub fn visit_in_inline_value_batches_for_label<Visit>(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut crate::labeled::LabeledPayloadValueBatchScratch,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(crate::labeled::LabeledPayloadValueBatch<'b>),
    {
        self.reverse
            .visit_out_inline_value_batches_for_label(dst, label_id, order, scratch, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Reads reverse outgoing edge rows for the requested slot indices (topology only).
    pub fn read_in_edge_slots_for_label<Visit>(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        slots: &[u32],
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.reverse
            .read_out_edge_slots_for_label(dst, label_id, slots, order, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Like [`Self::read_in_edge_slots_for_label`], reusing hybrid overflow replay from the reverse
    /// phase-1 scan (`visit_in_inline_value_batches_for_label`). Mirrors the forward
    /// [`Self::read_out_edge_slots_for_label_with_replay`] contract on reverse orientation.
    pub fn read_in_edge_slots_for_label_with_replay<Visit>(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        slots: &[u32],
        order: OutEdgeOrder,
        replay: Option<&crate::labeled::HybridOverflowEdgeReplay>,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.reverse
            .read_out_edge_slots_for_label_with_replay(dst, label_id, slots, order, replay, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Visits reverse outgoing edges (incoming edges in the public graph view) and parallel value
    /// bytes for one label in `order`.
    pub fn visit_in_edge_inline_value_batches_for_label<Visit>(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut crate::labeled::LabeledEdgeInlineValueBatchScratch<E>,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(crate::labeled::LabeledEdgeInlineValueBatch<'b, E>),
    {
        self.reverse
            .visit_out_edge_inline_value_batches_for_label(dst, label_id, order, scratch, visit)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
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
    pub(crate) fn for_each_out_edges_by_directedness<Visit>(
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
    pub(crate) fn for_each_out_edges_by_directedness_unchecked<Visit>(
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
    pub(crate) fn for_each_in_edges_by_directedness<Visit>(
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
    pub(crate) fn for_each_in_edges_by_directedness_unchecked<Visit>(
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

    /// Like [`Self::for_each_in_edges_for_label_ordered`], but skips edge-inline-value reads.
    pub fn for_each_in_edges_for_label_topology_ordered<Visit>(
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
            .for_each_edges_for_label_topology_ordered(dst, label_id, order, visit)
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

    /// Finds the first forward outgoing edge accepted by `pred` in default descending order,
    /// returning the edge, label id, and physical slot index inside that label row.
    pub fn find_forward_out_edge_slot_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        pred: F,
    ) -> Result<Option<(E, BucketLabelKey, u32)>, DeferredBidirectionalLabeledError>
    where
        F: FnMut(&E) -> bool,
    {
        self.forward
            .find_out_edge_slot_with_label_by_predicate(src, pred)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Finds the first reverse-store outgoing edge accepted by `pred` in default descending order,
    /// returning the edge, label id, and physical slot index inside that reverse label row.
    pub fn find_reverse_out_edge_slot_with_label_by_predicate<F>(
        &self,
        dst: VertexId,
        pred: F,
    ) -> Result<Option<(E, BucketLabelKey, u32)>, DeferredBidirectionalLabeledError>
    where
        F: FnMut(&E) -> bool,
    {
        self.reverse
            .find_out_edge_slot_with_label_by_predicate(dst, pred)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Removes one forward edge at the given label-row slot.
    pub fn remove_forward_edge_at_slot(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
    ) -> Result<Option<E>, DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
    {
        self.forward
            .remove_edge_at_slot(src, label_id, slot_index)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Removes one forward edge and reports the bounded slot shifts from overflow unlink.
    pub fn remove_forward_edge_at_slot_with_move(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
    ) -> Result<Option<EdgeRemoval<E>>, DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
    {
        self.forward
            .remove_edge_at_slot_with_move(src, label_id, slot_index)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Updates the edge-inline-value payload for one forward-out edge at `slot_index`.
    pub fn update_forward_edge_inline_value_at_slot(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
        edge: E,
    ) -> Result<bool, DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
    {
        self.forward
            .update_edge_inline_value_at_slot(src, label_id, slot_index, edge)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Updates the edge-inline-value payload for one reverse-store out edge at `slot_index`.
    pub fn update_reverse_edge_inline_value_at_slot(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
        edge: E,
    ) -> Result<bool, DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
    {
        self.reverse
            .update_edge_inline_value_at_slot(dst, label_id, slot_index, edge)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Removes one reverse-store edge at the given label-row slot.
    pub fn remove_reverse_edge_at_slot(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
    ) -> Result<Option<E>, DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
    {
        self.reverse
            .remove_edge_at_slot(dst, label_id, slot_index)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Removes one reverse edge and reports the bounded slot shifts from overflow unlink.
    pub fn remove_reverse_edge_at_slot_with_move(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
    ) -> Result<Option<EdgeRemoval<E>>, DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
    {
        self.reverse
            .remove_edge_at_slot_with_move(dst, label_id, slot_index)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Removes one reverse-store edge from `dst` under `label_id`.
    pub fn remove_reverse_edge_matching<F>(
        &self,
        dst: VertexId,
        label_id: BucketLabelKey,
        matches: F,
    ) -> Result<Option<E>, DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        self.reverse
            .remove_edge_matching(dst, label_id, matches)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// Removes one matching forward edge and reports the bounded slot shifts from overflow unlink.
    pub fn remove_forward_edge_matching_with_move<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        matches: F,
    ) -> Result<Option<EdgeRemoval<E>>, DeferredBidirectionalLabeledError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        self.forward
            .remove_edge_matching_with_move(src, label_id, matches)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Processes queued maintenance work up to `budget`.
    pub fn maintenance(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<BidirectionalMaintenanceReport, DeferredBidirectionalLabeledError>
    where
        E: PartialEq,
    {
        self.maintenance_with_observers(
            budget,
            &mut NoopEdgeSlotMoveObserver,
            &mut NoopDeleteEdgeObserver,
        )
    }

    /// Processes queued maintenance work and reports edge slot relocations to `observer`.
    pub fn maintenance_with_edge_slot_move_observer<O>(
        &self,
        budget: MaintenanceBudget,
        observer: &mut O,
    ) -> Result<BidirectionalMaintenanceReport, DeferredBidirectionalLabeledError>
    where
        O: EdgeSlotMoveObserver,
        E: CsrEdgeTombstone + PartialEq,
    {
        self.maintenance_with_observers(budget, observer, &mut NoopDeleteEdgeObserver)
    }

    /// Processes queued maintenance work and reports removed edges of resumable
    /// vertex-delete jobs to `delete_observer`.
    pub fn maintenance_with_delete_observer<D>(
        &self,
        budget: MaintenanceBudget,
        delete_observer: &mut D,
    ) -> Result<BidirectionalMaintenanceReport, DeferredBidirectionalLabeledError>
    where
        D: DeleteEdgeObserver<E>,
        E: CsrEdgeTombstone + PartialEq,
    {
        self.maintenance_with_observers(budget, &mut NoopEdgeSlotMoveObserver, delete_observer)
    }

    /// Processes queued maintenance work, threading both the compaction edge-slot
    /// move observer and the resumable vertex-delete edge observer.
    pub fn maintenance_with_observers<O, D>(
        &self,
        budget: MaintenanceBudget,
        observer: &mut O,
        delete_observer: &mut D,
    ) -> Result<BidirectionalMaintenanceReport, DeferredBidirectionalLabeledError>
    where
        O: EdgeSlotMoveObserver,
        D: DeleteEdgeObserver<E>,
        E: CsrEdgeTombstone + PartialEq,
    {
        use crate::labeled::graph::VertexEdgeSpanCompactOneStep;

        let mut report = BidirectionalMaintenanceReport::default();
        let baseline = current_instruction_counter();
        let max_items = budget.max_work_items.unwrap_or(u32::MAX);
        let mut checkpoint_tick = 0u32;

        while report.work.processed_work_items < max_items {
            if budget
                .max_delete_edge_steps
                .is_some_and(|max| report.work.processed_delete_edge_steps >= max)
            {
                break;
            }
            checkpoint_tick = checkpoint_tick.wrapping_add(1);
            let should_check = budget.checkpoint_every <= 1
                || checkpoint_tick.is_multiple_of(budget.checkpoint_every);
            report.instructions_used = current_instruction_counter().saturating_sub(baseline);
            if should_check
                && budget.max_instructions > 0
                && report
                    .instructions_used
                    .saturating_add(budget.reserve_instructions)
                    >= budget.max_instructions
            {
                report.instruction_budget_exhausted = true;
                break;
            }

            let Some(item) = self.maintenance.pop_next()? else {
                break;
            };
            report.work.processed_work_items = report.work.processed_work_items.saturating_add(1);
            // Set when a compaction step fails. A failed step may have partially mutated the
            // slab, so the item must be retried rather than marked complete. We requeue it and
            // stop the pass to avoid hot-looping a deterministic failure within a single tick;
            // the next maintenance tick retries it with a fresh instruction budget.
            let mut stalled = false;
            let requeue = match item {
                MaintenanceWorkItem::CompactLabelBucketVertexSegment { orientation, vid } => {
                    let graph = match orientation {
                        Orientation::Forward => &self.forward,
                        Orientation::Reverse => &self.reverse,
                    };
                    if graph.compact_label_bucket_vertex_segment(vid).is_ok() {
                        report.work.rebalanced_segments =
                            report.work.rebalanced_segments.saturating_add(1);
                    }
                    None
                }
                MaintenanceWorkItem::CompactVertexEdgeSpan {
                    orientation,
                    vid,
                    anchor_bucket_index,
                    resume_bucket_index,
                } => {
                    let graph = match orientation {
                        Orientation::Forward => &self.forward,
                        Orientation::Reverse => &self.reverse,
                    };
                    let vertex = graph.vertices().get(vid);
                    if anchor_bucket_index >= vertex.degree() {
                        None
                    } else {
                        match graph.compact_vertex_edge_span_one_step(vid, resume_bucket_index) {
                            Ok(VertexEdgeSpanCompactOneStep::EdgeMoved(moved)) => {
                                observer.edge_slot_moved(orientation, vid, moved);
                                Some(MaintenanceWorkItem::CompactVertexEdgeSpan {
                                    orientation,
                                    vid,
                                    anchor_bucket_index,
                                    resume_bucket_index,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(next)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeSpan {
                                    orientation,
                                    vid,
                                    anchor_bucket_index,
                                    resume_bucket_index: next,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::OverflowRewrite(moves)) => {
                                for moved in moves {
                                    observer.edge_slot_moved(orientation, vid, moved);
                                }
                                Some(MaintenanceWorkItem::CompactVertexEdgeSpan {
                                    orientation,
                                    vid,
                                    anchor_bucket_index,
                                    resume_bucket_index: 0,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::Finished) => {
                                report.work.rebalanced_segments =
                                    report.work.rebalanced_segments.saturating_add(1);
                                None
                            }
                            Err(_) => {
                                stalled = true;
                                None
                            }
                        }
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
                    Some(MaintenanceWorkItem::CompactVertexEdgeSpan {
                        orientation,
                        vid,
                        anchor_bucket_index: 0,
                        resume_bucket_index: 0,
                    })
                }
                MaintenanceWorkItem::CompactVertexValueSpan { .. } => None,
                MaintenanceWorkItem::CompactPayloadSlab { orientation } => {
                    let graph = match orientation {
                        Orientation::Forward => &self.forward,
                        Orientation::Reverse => &self.reverse,
                    };
                    match graph.compact_payload_slab() {
                        Ok(result) => {
                            if result.moved_spans > 0 {
                                report.work.rebalanced_segments =
                                    report.work.rebalanced_segments.saturating_add(1);
                            }
                            None
                        }
                        Err(_) => {
                            stalled = true;
                            Some(MaintenanceWorkItem::CompactPayloadSlab { orientation })
                        }
                    }
                }
                MaintenanceWorkItem::CompactVertexEdgeAndValueSpan {
                    orientation,
                    vid,
                    anchor_bucket_index,
                    resume_bucket_index,
                } => {
                    let graph = match orientation {
                        Orientation::Forward => &self.forward,
                        Orientation::Reverse => &self.reverse,
                    };
                    let vertex = graph.vertices().get(vid);
                    if anchor_bucket_index >= vertex.degree() {
                        None
                    } else {
                        match graph.compact_vertex_edge_span_one_step(vid, resume_bucket_index) {
                            Ok(VertexEdgeSpanCompactOneStep::EdgeMoved(_)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeAndValueSpan {
                                    orientation,
                                    vid,
                                    anchor_bucket_index,
                                    resume_bucket_index,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(next)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeAndValueSpan {
                                    orientation,
                                    vid,
                                    anchor_bucket_index,
                                    resume_bucket_index: next,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::OverflowRewrite(_)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeAndValueSpan {
                                    orientation,
                                    vid,
                                    anchor_bucket_index,
                                    resume_bucket_index: 0,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::Finished) => {
                                report.work.rebalanced_segments =
                                    report.work.rebalanced_segments.saturating_add(1);
                                None
                            }
                            Err(_) => {
                                stalled = true;
                                None
                            }
                        }
                    }
                }
                MaintenanceWorkItem::DeleteVertex { vid, removed_edges } => {
                    let (next, did_step, completed) = self.process_delete_vertex_step(
                        vid,
                        removed_edges,
                        observer,
                        delete_observer,
                    )?;
                    if did_step {
                        report.work.processed_delete_edge_steps =
                            report.work.processed_delete_edge_steps.saturating_add(1);
                    }
                    if completed {
                        report.work.completed_vertex_deletes =
                            report.work.completed_vertex_deletes.saturating_add(1);
                    }
                    next
                }
            };
            if stalled {
                // Keep the failed item queued (its dirty bit is still set) and stop the pass.
                self.maintenance.requeue_front(item)?;
                break;
            }
            if let Some(next) = requeue {
                self.maintenance.requeue_front(next)?;
            } else {
                self.maintenance.complete(item)?;
            }
        }

        report.instructions_used = current_instruction_counter().saturating_sub(baseline);
        report.instruction_budget_exhausted |= budget.max_instructions > 0
            && report
                .instructions_used
                .saturating_add(budget.reserve_instructions)
                >= budget.max_instructions;
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
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        self.reverse
            .push_vertex(row)
            .map_err(DeferredBidirectionalLabeledError::Reverse)?;
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

    /// Directed outgoing edges at `src` in ascending slot order.
    pub fn directed_out_edges(
        &self,
        src: VertexId,
    ) -> Result<Vec<E>, DeferredBidirectionalLabeledError> {
        let mut edges = Vec::new();
        self.for_each_directed_out_edges(src, OutEdgeOrder::Ascending, |edge| edges.push(edge))?;
        Ok(edges)
    }

    /// Directed incoming edges at `dst` in ascending slot order.
    pub fn directed_in_edges(
        &self,
        dst: VertexId,
    ) -> Result<Vec<E>, DeferredBidirectionalLabeledError> {
        let mut edges = Vec::new();
        self.for_each_directed_in_edges(dst, OutEdgeOrder::Ascending, |edge| edges.push(edge))?;
        Ok(edges)
    }

    /// Undirected edges at `src` in ascending slot order (forward store only).
    pub fn undirected_edges(
        &self,
        src: VertexId,
    ) -> Result<Vec<E>, DeferredBidirectionalLabeledError> {
        let mut edges = Vec::new();
        self.for_each_undirected_edges(src, OutEdgeOrder::Ascending, |edge| edges.push(edge))?;
        Ok(edges)
    }

    /// Streaming directed forward out-edge iterator in `order`.
    pub fn directed_out_edges_iter(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<
        crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>,
        DeferredBidirectionalLabeledError,
    > {
        self.forward
            .out_edges_by_directedness_iter(src, BucketDirectedness::Directed, order)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Streaming undirected forward out-edge iterator in `order`.
    pub fn undirected_edges_iter(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<
        crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>,
        DeferredBidirectionalLabeledError,
    > {
        self.forward
            .out_edges_by_directedness_iter(src, BucketDirectedness::Undirected, order)
            .map_err(DeferredBidirectionalLabeledError::Forward)
    }

    /// Streaming directed incoming-edge iterator in `order`.
    pub fn directed_in_edges_iter(
        &self,
        dst: VertexId,
        order: OutEdgeOrder,
    ) -> Result<
        crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>,
        DeferredBidirectionalLabeledError,
    > {
        self.reverse
            .out_edges_by_directedness_iter(dst, BucketDirectedness::Directed, order)
            .map_err(DeferredBidirectionalLabeledError::Reverse)
    }

    /// `true` when `vid` has at least one incident edge in either orientation.
    pub fn has_incident_edges(
        &self,
        vid: VertexId,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        let len = self.vertex_count_checked()?;
        if u32::from(vid) >= len.0 {
            return Err(DeferredBidirectionalLabeledError::VertexOutOfRange { vid, len });
        }
        Ok(self.forward.vertices().get(vid).degree() > 0
            || self.reverse.vertices().get(vid).degree() > 0)
    }

    /// Total incident logical degree of `vid` (forward out-adjacency + reverse
    /// in-adjacency). This is the synchronous work proxy for a detach-delete: each
    /// incident edge removal also touches its neighbor's counterpart row.
    pub fn incident_degree(&self, vid: VertexId) -> Result<u64, DeferredBidirectionalLabeledError> {
        let len = self.vertex_count_checked()?;
        if u32::from(vid) >= len.0 {
            return Err(DeferredBidirectionalLabeledError::VertexOutOfRange { vid, len });
        }
        let forward = self
            .forward
            .vertex_live_edge_count(vid)
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        let reverse = self
            .reverse
            .vertex_live_edge_count(vid)
            .map_err(DeferredBidirectionalLabeledError::Reverse)?;
        Ok(forward.saturating_add(reverse))
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
                    edge_matches_remove_target(cand, &edge, dst)
                })
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
            if let Some(removed) = removed {
                let reverse_edge = removed.with_neighbor_vid(src);
                let reverse_removed = self
                    .reverse
                    .remove_edge_matching(dst, label_id, |cand| {
                        *cand == reverse_edge && cand.neighbor_vid() == src
                    })
                    .map_err(DeferredBidirectionalLabeledError::Reverse)?;
                if reverse_removed.is_none() {
                    let _ = self
                        .reverse
                        .remove_edge_matching(dst, label_id, |cand| cand.neighbor_vid() == src)
                        .map_err(DeferredBidirectionalLabeledError::Reverse)?;
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Removes one undirected logical edge (forward out records at both endpoints).
    pub fn remove_undirected_deferred(
        &self,
        u: VertexId,
        v: VertexId,
        edge_at_u: E,
    ) -> Result<bool, DeferredBidirectionalLabeledError>
    where
        E: PartialEq,
    {
        let edge_at_v = edge_at_u.with_neighbor_vid(u);
        let ok_uv = self.remove_forward_half_undirected(u, v, edge_at_u)?;
        let ok_vu = if u == v {
            ok_uv
        } else {
            self.remove_forward_half_undirected(v, u, edge_at_v)?
        };
        Ok(ok_uv || ok_vu)
    }

    fn remove_forward_half_undirected(
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
            if label_id.is_directed() {
                continue;
            }
            let removed = self
                .forward
                .remove_edge_matching(src, label_id, |cand| {
                    edge_matches_remove_target(cand, &edge, dst)
                })
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
            if removed.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Drains one directed in-edge `src -> dst` while `dst` is being deleted:
    /// removes one forward record at `src` and one reverse record at `dst`, matched
    /// by neighbor identity across directed label buckets.
    ///
    /// A reverse-store record only carries `dst`'s reverse slot, which does not
    /// match `src`'s forward slot, so a payload-free directed edge cannot be located
    /// by the `edge`-identity removal used elsewhere (it would match by slot and
    /// silently fail, spinning the purge). Because `dst` is being deleted every
    /// `src -> dst` edge is removed eventually, so draining an arbitrary parallel
    /// per call keeps forward/reverse counts balanced (ADR 0021). Removing the
    /// reverse record independently guarantees forward progress even if the forward
    /// record is already gone.
    /// Queued incremental vertex deletion: removes all incident edges then clears the row.
    pub fn delete_vertex_deferred(
        &self,
        vid: VertexId,
    ) -> Result<bool, DeferredBidirectionalLabeledError>
    where
        E: PartialEq + CsrEdgeTombstone,
    {
        // Drain `vid`'s own rows in O(degree) (descending-slot removal, no per-edge
        // predicate re-scan), then remove only the counterpart row at each neighbour.
        // The owner side never re-scans, so the cost is O(degree + sum of neighbour
        // degrees) instead of the prior O(degree^2) owner predicate re-scan.
        let forward_labels = self
            .forward
            .out_edge_label_ids(vid)
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        for label_id in forward_labels {
            let mut neighbors: Vec<VertexId> = Vec::new();
            self.forward
                .drain_out_edges_for_label(vid, label_id, |edge| {
                    neighbors.push(edge.neighbor_vid())
                })
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
            for neighbor in neighbors {
                if neighbor == vid {
                    continue;
                }
                if label_id.is_undirected() {
                    self.forward
                        .remove_edge_matching(neighbor, label_id, |cand| cand.neighbor_vid() == vid)
                        .map_err(DeferredBidirectionalLabeledError::Forward)?;
                } else {
                    self.reverse
                        .remove_edge_matching(neighbor, label_id, |cand| cand.neighbor_vid() == vid)
                        .map_err(DeferredBidirectionalLabeledError::Reverse)?;
                }
            }
        }
        // Reverse-store out-edges are directed in-edges `src -> vid`; drain them and
        // remove the surviving forward record at each `src`.
        let reverse_labels = self
            .reverse
            .out_edge_label_ids(vid)
            .map_err(DeferredBidirectionalLabeledError::Reverse)?;
        for label_id in reverse_labels {
            let mut sources: Vec<VertexId> = Vec::new();
            self.reverse
                .drain_out_edges_for_label(vid, label_id, |edge| sources.push(edge.neighbor_vid()))
                .map_err(DeferredBidirectionalLabeledError::Reverse)?;
            for src in sources {
                if src == vid {
                    continue;
                }
                self.forward
                    .remove_edge_matching(src, label_id, |cand| cand.neighbor_vid() == vid)
                    .map_err(DeferredBidirectionalLabeledError::Forward)?;
            }
        }
        self.finalize_vertex_delete(vid)?;
        Ok(true)
    }

    /// Clears both orientation rows of `vid` and tombstones the shared vertex row.
    ///
    /// Shared by the synchronous [`Self::delete_vertex_deferred`] and the resumable
    /// [`MaintenanceWorkItem::DeleteVertex`] finalize step (ADR 0021).
    fn finalize_vertex_delete(
        &self,
        vid: VertexId,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
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
        Ok(())
    }

    /// Enqueues a resumable [`MaintenanceWorkItem::DeleteVertex`] purge of `vid`.
    ///
    /// The job removes incident edges one per maintenance step and tombstones the
    /// vertex when drained. Stage 1 (ADR 0021) ships the machinery; the production
    /// delete path stays synchronous until Stage 2.
    pub fn enqueue_vertex_delete(
        &self,
        vid: VertexId,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        let len = self.vertex_count_checked()?;
        if u32::from(vid) >= len.0 {
            return Err(DeferredBidirectionalLabeledError::VertexOutOfRange { vid, len });
        }
        self.maintenance
            .enqueue_delete_vertex(MaintenanceWorkItem::DeleteVertex {
                vid,
                removed_edges: 0,
            })
    }

    /// Tombstone-first start of a resumable vertex delete (ADR 0021 Stage 2).
    ///
    /// Sets the tombstone bit on both orientation rows **in place** so the vertex
    /// is immediately invisible to node scans, while preserving each side's label
    /// buckets so the deferred [`MaintenanceWorkItem::DeleteVertex`] purge can still
    /// iterate and drain the incident edges. The dangling back-edges that survive
    /// at neighbours until the purge completes are hidden by the graph-facade read
    /// gate, preserving the refined "tombstoned ⇒ no *visible* incident edges"
    /// invariant.
    ///
    /// Unlike [`Self::delete_vertex_deferred`] (synchronous, O(degree) in one
    /// message) this does only O(1) work before returning, then enqueues the purge.
    pub fn begin_vertex_delete_deferred(
        &self,
        vid: VertexId,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        let len = self.vertex_count_checked()?;
        if u32::from(vid) >= len.0 {
            return Err(DeferredBidirectionalLabeledError::VertexOutOfRange { vid, len });
        }
        let forward_row = self.forward.vertices().get(vid);
        if !forward_row.is_tombstone() {
            self.forward
                .vertices()
                .set(vid, &forward_row.with_tombstone(true));
        }
        let reverse_row = self.reverse.vertices().get(vid);
        if !reverse_row.is_tombstone() {
            self.reverse
                .vertices()
                .set(vid, &reverse_row.with_tombstone(true));
        }
        self.maintenance
            .enqueue_delete_vertex(MaintenanceWorkItem::DeleteVertex {
                vid,
                removed_edges: 0,
            })
    }

    /// Performs one step of a resumable vertex-delete job: removes a single
    /// incident edge (and its counterpart) if any remain, else finalizes.
    ///
    /// Returns `(next_work_item, did_edge_step, completed)`.
    fn process_delete_vertex_step<O, D>(
        &self,
        vid: VertexId,
        removed_edges: u32,
        move_observer: &mut O,
        delete_observer: &mut D,
    ) -> Result<(Option<MaintenanceWorkItem>, bool, bool), DeferredBidirectionalLabeledError>
    where
        O: EdgeSlotMoveObserver,
        D: DeleteEdgeObserver<E>,
        E: PartialEq + CsrEdgeTombstone,
    {
        let next_item = |removed: u32| {
            Some(MaintenanceWorkItem::DeleteVertex {
                vid,
                removed_edges: removed.saturating_add(1),
            })
        };

        // First step: compact both orientation rows once so each later removal hits a
        // front-packed slab slot in O(1) (folds overflow logs, drops tombstone gaps).
        // This converts the owner-side drain from O(degree^2) (the prior per-step
        // `asc_out_edges` re-scan that skipped a growing tombstone prefix, plus the
        // per-edge predicate re-find) to O(degree). Bypass rows are left as-is and
        // drained by a bounded descending scan.
        if removed_edges == 0 {
            for moved in self
                .forward
                .compact_vertex_edge_span_with_moves(vid, 0)
                .map_err(DeferredBidirectionalLabeledError::Forward)?
            {
                move_observer.edge_slot_moved(Orientation::Forward, vid, moved);
            }
            for moved in self
                .reverse
                .compact_vertex_edge_span_with_moves(vid, 0)
                .map_err(DeferredBidirectionalLabeledError::Reverse)?
            {
                move_observer.edge_slot_moved(Orientation::Reverse, vid, moved);
            }
        }

        // Drain one owner out-edge (forward) and remove only its counterpart row at the
        // neighbour, mirroring the synchronous `delete_vertex_deferred` per edge.
        if let Some((edge, label)) = self
            .forward
            .remove_top_out_edge(vid)
            .map_err(DeferredBidirectionalLabeledError::Forward)?
        {
            let dst = edge.neighbor_vid();
            // Clear the removed canonical sidecars before any survivor is shifted into its slot.
            delete_observer.on_delete_outgoing_edge(vid, edge.clone());
            if dst != vid {
                if label.is_undirected() {
                    if let Some(removal) = self
                        .forward
                        .remove_edge_matching_with_move(dst, label, |cand| {
                            cand.neighbor_vid() == vid
                        })
                        .map_err(DeferredBidirectionalLabeledError::Forward)?
                    {
                        for moved in removal.moves {
                            move_observer.edge_slot_moved(Orientation::Forward, dst, moved);
                        }
                    }
                } else {
                    if let Some(removal) = self
                        .reverse
                        .remove_edge_matching_with_move(dst, label, |cand| {
                            cand.neighbor_vid() == vid
                        })
                        .map_err(DeferredBidirectionalLabeledError::Reverse)?
                    {
                        for moved in removal.moves {
                            move_observer.edge_slot_moved(Orientation::Reverse, dst, moved);
                        }
                    }
                }
            }
            return Ok((next_item(removed_edges), true, false));
        }

        // Then one directed in-edge: the owner record lives in the reverse store; remove
        // the surviving forward record at the source.
        if let Some((edge, label)) = self
            .reverse
            .remove_top_out_edge(vid)
            .map_err(DeferredBidirectionalLabeledError::Reverse)?
        {
            let src = edge.neighbor_vid();
            // The counterpart forward unlink may shift a survivor into the removed canonical slot.
            delete_observer.on_delete_incoming_edge(vid, edge.clone());
            if src != vid
                && let Some(removal) = self
                    .forward
                    .remove_edge_matching_with_move(src, label, |cand| cand.neighbor_vid() == vid)
                    .map_err(DeferredBidirectionalLabeledError::Forward)?
            {
                for moved in removal.moves {
                    move_observer.edge_slot_moved(Orientation::Forward, src, moved);
                }
            }
            return Ok((next_item(removed_edges), true, false));
        }

        self.finalize_vertex_delete(vid)?;
        delete_observer.on_vertex_purge_completed(vid);
        Ok((None, false, true))
    }

    /// Inserts an undirected edge on forward out-adjacency at both endpoints (no reverse rows).
    pub fn insert_undirected_deferred(
        &self,
        u: VertexId,
        v: VertexId,
        label_id: BucketLabelKey,
        edge_uv: E,
        edge_vu: E,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        self.insert_undirected_deferred_with_locations(u, v, label_id, edge_uv, edge_vu)
            .map(|_| ())
    }

    /// Inserts both undirected endpoint rows and returns their exact locations.
    pub fn insert_undirected_deferred_with_locations(
        &self,
        u: VertexId,
        v: VertexId,
        label_id: BucketLabelKey,
        edge_uv: E,
        edge_vu: E,
    ) -> Result<ScalarInsertPair, DeferredBidirectionalLabeledError> {
        debug_assert!(
            label_id.is_undirected(),
            "insert_undirected_deferred requires an undirected bucket label"
        );
        self.forward
            .prepare_labeled_edge_capacity_for_insert(u, label_id)
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        if u != v {
            self.forward
                .prepare_labeled_edge_capacity_for_insert(v, label_id)
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
        }
        let u_payload_compaction_needed = edge_uv.edge_inline_value_byte_width() != 0
            && self
                .forward
                .payload_compaction_needed(u64::from(edge_uv.edge_inline_value_byte_width()))
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
        let v_payload_compaction_needed = edge_vu.edge_inline_value_byte_width() != 0
            && self
                .forward
                .payload_compaction_needed(u64::from(edge_vu.edge_inline_value_byte_width()))
                .map_err(DeferredBidirectionalLabeledError::Forward)?;
        let forward_location = self
            .forward
            .insert_edge_skip_leaf_cascade_deferred_payload_with_location(u, label_id, edge_uv)
            .map_err(DeferredBidirectionalLabeledError::Forward)?;
        let reverse_location = if u != v {
            Some(
                self.forward
                    .insert_edge_skip_leaf_cascade_deferred_payload_with_location(
                        v, label_id, edge_vu,
                    )
                    .map_err(DeferredBidirectionalLabeledError::Forward)?,
            )
        } else {
            None
        };
        if u_payload_compaction_needed || (u != v && v_payload_compaction_needed) {
            self.mark_compact_payload_slab(Orientation::Forward)?;
        }
        if self.forward.labeled_leaf_segment_is_dense(u) {
            self.mark_compact_dense_labeled_vertex_maintenance(Orientation::Forward, u)?;
        }
        if u != v && self.forward.labeled_leaf_segment_is_dense(v) {
            self.mark_compact_dense_labeled_vertex_maintenance(Orientation::Forward, v)?;
        }
        Ok(ScalarInsertPair {
            forward: forward_location,
            reverse: reverse_location.flatten(),
        })
    }

    /// Visits undirected outgoing edges at `vertex_id` (forward store, undirected buckets only).
    pub fn for_each_undirected_edges<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.for_each_out_edges_by_directedness(
            vertex_id,
            BucketDirectedness::Undirected,
            order,
            visit,
        )
    }

    /// Like [`Self::for_each_undirected_edges`], but skips `ensure_vertex`.
    pub fn for_each_undirected_edges_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.for_each_out_edges_by_directedness_unchecked(
            vertex_id,
            BucketDirectedness::Undirected,
            order,
            visit,
        )
    }

    /// Visits directed outgoing edges at `vertex_id` (forward store, directed buckets only).
    pub fn for_each_directed_out_edges<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.for_each_out_edges_by_directedness(
            vertex_id,
            BucketDirectedness::Directed,
            order,
            visit,
        )
    }

    /// Visits directed incoming edges at `vertex_id` (reverse store, directed buckets only).
    pub fn for_each_directed_in_edges<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(E),
    {
        self.for_each_in_edges_by_directedness(
            vertex_id,
            BucketDirectedness::Directed,
            order,
            visit,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        labeled::PhysicalEdgeRef,
        test_support::{labeled_lara_memories, vector_memory},
        traits::{CsrEdge, CsrEdgeTombstone},
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

    use crate::VectorMemory;

    fn graph() -> DeferredBidirectionalLabeledLaraGraph<TestEdge, VectorMemory> {
        sized_graph(128)
    }

    fn sized_graph(
        elem_capacity: u64,
    ) -> DeferredBidirectionalLabeledLaraGraph<TestEdge, VectorMemory> {
        let (
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
            fvs,
            fvffs,
            fvffsbs,
            fvlog,
            fvblobs,
        ) = labeled_lara_memories();
        let (
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
            rvs,
            rvffs,
            rvffsbs,
            rvlog,
            rvblobs,
        ) = labeled_lara_memories();
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
            fvs,
            fvffs,
            fvffsbs,
            fvlog,
            fvblobs,
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
            rvs,
            rvffs,
            rvffsbs,
            rvlog,
            rvblobs,
            vector_memory(),
            vector_memory(),
            crate::labeled::InitialCapacities::uniform(elem_capacity),
            BucketLabelKey::from_raw(1),
        )
        .expect("graph")
    }

    fn unbounded_budget() -> MaintenanceBudget {
        MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        }
    }

    #[test]
    fn failed_compaction_step_is_requeued_not_silently_completed() {
        // A compaction step can fail after partially mutating the slab (e.g. a transient
        // grow failure). The maintenance loop must keep the work item queued for retry
        // instead of marking it complete and dropping it, which would leave the span
        // half-compacted with no path back to consistency.
        let graph = graph();
        let src = graph.push_vertex().expect("src");
        let dst = graph.push_vertex().expect("dst");
        let label = BucketLabelKey::directed_from_index(3);
        graph
            .insert_directed_edge(
                src,
                dst,
                label,
                TestEdge(u32::from(dst)),
                TestEdge(u32::from(src)),
            )
            .expect("src -> dst");
        graph
            .maintenance(unbounded_budget())
            .expect("settle insert");
        assert_eq!(
            graph.maintenance_queue_len(),
            0,
            "insert maintenance should drain to a clean queue"
        );

        // Enqueue a forward vertex-edge-span compaction (src has degree > 0, so the loop
        // reaches the fallible one-step call) and force that step to fail.
        graph
            .mark_compact_vertex_edge_span(Orientation::Forward, src, 0)
            .expect("enqueue compaction");
        assert_eq!(graph.maintenance_queue_len(), 1);

        crate::labeled::graph::force_next_compact_vertex_edge_span_step_error();
        let report = graph
            .maintenance(unbounded_budget())
            .expect("maintenance pass");

        assert_eq!(
            graph.maintenance_queue_len(),
            1,
            "a failed compaction step must be requeued, not silently completed"
        );
        assert_eq!(report.work.processed_work_items, 1);
        assert_eq!(
            report.work.rebalanced_segments, 0,
            "a failed step must not be counted as rebalanced"
        );

        // With the fault cleared, the retained item drains cleanly, proving the retry path.
        let retry = graph.maintenance(unbounded_budget()).expect("retry pass");
        assert_eq!(
            graph.maintenance_queue_len(),
            0,
            "the requeued item should drain once the fault clears"
        );
        assert!(retry.work.processed_work_items >= 1);
    }

    #[test]
    fn new_label_in_middle_of_oversized_leaf_fits_without_drain() {
        // Regression for the social-demo Post shape: a vertex already has label
        // buckets that sort *around* the new label, and at least one bucket has
        // stored_slots > DEFAULT_SEGMENT_SIZE.  The new bucket must be inserted in
        // the middle of the row, not appended, so try_place_new_bucket_edge_span
        // returns false and the fallback must rebalance the pinned leaf instead of
        // tail-appending.
        let graph = sized_graph(1 << 16);
        let src = graph.push_vertex().expect("src");

        // Fill the leaf with mates so src does not get an oversized quota for free.
        let mut mates = Vec::new();
        for _ in 0..31 {
            mates.push(graph.push_vertex().expect("mate"));
        }
        let mate_label = BucketLabelKey::directed_from_index(2);
        for mate in &mates {
            let dst = graph.push_vertex().expect("dst for mate");
            graph
                .insert_directed_edge(
                    *mate,
                    dst,
                    mate_label,
                    TestEdge(u32::from(dst)),
                    TestEdge(u32::from(*mate)),
                )
                .expect("mate edge");
        }

        // Existing buckets at indices 3 and 7 bracket the new label at index 5.
        let before_label = BucketLabelKey::directed_from_index(3);
        let after_label = BucketLabelKey::directed_from_index(7);
        let before_dst = graph.push_vertex().expect("dst for before label");
        graph
            .insert_directed_edge(
                src,
                before_dst,
                before_label,
                TestEdge(u32::from(before_dst)),
                TestEdge(u32::from(src)),
            )
            .expect("before label edge");

        // Make the "before" bucket oversized so there is no slack to insert in the
        // middle without a leaf-level rebalance.
        for _ in 0..64 {
            let dst = graph.push_vertex().expect("dst for before label growth");
            graph
                .insert_directed_edge(
                    src,
                    dst,
                    before_label,
                    TestEdge(u32::from(dst)),
                    TestEdge(u32::from(src)),
                )
                .expect("before label growth edge");
        }

        let after_dst = graph.push_vertex().expect("dst for after label");
        graph
            .insert_directed_edge(
                src,
                after_dst,
                after_label,
                TestEdge(u32::from(after_dst)),
                TestEdge(u32::from(src)),
            )
            .expect("after label edge");

        // Insert a new label that sorts between before_label and after_label.
        let new_label = BucketLabelKey::directed_from_index(5);
        let dst = graph.push_vertex().expect("dst for new label");
        graph
            .insert_directed_edge(
                src,
                dst,
                new_label,
                TestEdge(u32::from(dst)),
                TestEdge(u32::from(src)),
            )
            .expect("new label in middle");

        graph
            .maintenance(unbounded_budget())
            .expect("drain before verification");
        let mut forward = Vec::new();
        graph
            .for_each_out_edges_for_label(src, new_label, |edge| forward.push(edge))
            .expect("read new label forward");
        assert_eq!(forward.len(), 1);
    }

    #[test]
    fn new_label_after_oversized_bucket_fits_without_drain() {
        // Regression for the social-demo Post shape: a vertex has a label bucket
        // whose stored_slots already exceeds DEFAULT_SEGMENT_SIZE, then a new label
        // bucket is added without draining deferred maintenance.  The append path
        // in try_place_new_bucket_edge_span refuses when prev.stored_slots > 32,
        // so the preflight / ensure_labeled_bucket_edge_span_room path must rebalance
        // or relocate the pinned leaf instead of falling back to tail append.
        let graph = sized_graph(1 << 16);
        let src = graph.push_vertex().expect("src");

        // Fill the leaf with mates so src does not get an oversized quota for free.
        let mut mates = Vec::new();
        for _ in 0..31 {
            mates.push(graph.push_vertex().expect("mate"));
        }
        let mate_label = BucketLabelKey::directed_from_index(2);
        for mate in &mates {
            let dst = graph.push_vertex().expect("dst for mate");
            graph
                .insert_directed_edge(
                    *mate,
                    dst,
                    mate_label,
                    TestEdge(u32::from(dst)),
                    TestEdge(u32::from(*mate)),
                )
                .expect("mate edge");
        }

        // Give src one oversized bucket with more than DEFAULT_SEGMENT_SIZE edges.
        let big_label = BucketLabelKey::directed_from_index(3);
        for _ in 0..64 {
            let dst = graph.push_vertex().expect("dst for big label");
            graph
                .insert_directed_edge(
                    src,
                    dst,
                    big_label,
                    TestEdge(u32::from(dst)),
                    TestEdge(u32::from(src)),
                )
                .expect("big label edge");
        }

        // Now add a new label that sorts after the big bucket.  Without robust
        // leaf-level capacity preparation this traps on CollectAllocationOverflow.
        let new_label = BucketLabelKey::directed_from_index(4);
        let dst = graph.push_vertex().expect("dst for new label");
        graph
            .insert_directed_edge(
                src,
                dst,
                new_label,
                TestEdge(u32::from(dst)),
                TestEdge(u32::from(src)),
            )
            .expect("new label after oversized bucket");

        graph
            .maintenance(unbounded_budget())
            .expect("drain before verification");
        let mut forward = Vec::new();
        graph
            .for_each_out_edges_for_label(src, new_label, |edge| forward.push(edge))
            .expect("read new label forward");
        assert_eq!(forward.len(), 1);
    }

    #[test]
    fn repeated_multi_label_inserts_without_drain_remain_writable_simple() {
        // Minimal version: just src collecting labels, no leaf-mate pressure.
        let graph = sized_graph(1024);
        let src = graph.push_vertex().expect("src");
        const EXTRA_LABELS: u32 = 40;
        for i in 0..EXTRA_LABELS {
            let dst = graph.push_vertex().expect("dst for src label");
            let label = BucketLabelKey::directed_from_index(u16::try_from(3 + i).unwrap());
            graph
                .insert_directed_edge(
                    src,
                    dst,
                    label,
                    TestEdge(u32::from(dst)),
                    TestEdge(u32::from(src)),
                )
                .unwrap_or_else(|_| panic!("src label {i}"));
        }
        graph
            .maintenance(unbounded_budget())
            .expect("drain before verification");
        for i in 0..EXTRA_LABELS {
            let label = BucketLabelKey::directed_from_index(u16::try_from(3 + i).unwrap());
            let mut forward = Vec::new();
            graph
                .for_each_out_edges_for_label(src, label, |edge| forward.push(edge))
                .expect("read forward");
            assert_eq!(forward.len(), 1, "label {i} forward degree");
        }
    }

    #[test]
    fn repeated_multi_label_inserts_without_drain_remain_writable() {
        // Regression for plan 0077: a vertex accumulates many new labels while
        // deferred leaf maintenance is *not* drained.  Without storage-owned
        // pre-insert capacity preparation, the pinned PMA leaf block eventually has
        // no free span large enough for a new bucket and forward insertion traps
        // with CollectAllocationOverflow.  With preflight, ordinary inserts stay
        // writable and both orientations remain complete.
        let graph = sized_graph(1 << 16);
        let src = graph.push_vertex().expect("src");

        // Fill the same PMA leaf (segment_size == DEFAULT_SEGMENT_SIZE == 16) with
        // mates that pin the leaf block.  Each mate reserves its fixed per-vertex
        // quota, leaving src only its own quota for new buckets until the leaf
        // becomes dense and must be rebalanced / relocated.
        let mut mates = Vec::new();
        for _ in 0..31 {
            mates.push(graph.push_vertex().expect("mate"));
        }
        let mate_label = BucketLabelKey::directed_from_index(2);
        for mate in &mates {
            let dst = graph.push_vertex().expect("dst for mate");
            graph
                .insert_directed_edge(
                    *mate,
                    dst,
                    mate_label,
                    TestEdge(u32::from(dst)),
                    TestEdge(u32::from(*mate)),
                )
                .expect("mate edge");
        }

        // Add many new labels to src without draining deferred maintenance.  The
        // 33rd+ label exceeds src's fixed quota and forces preflight rebalance.
        const EXTRA_LABELS: u32 = 40;
        for i in 0..EXTRA_LABELS {
            let dst = graph.push_vertex().expect("dst for src label");
            let label = BucketLabelKey::directed_from_index(u16::try_from(3 + i).unwrap());
            graph
                .insert_directed_edge(
                    src,
                    dst,
                    label,
                    TestEdge(u32::from(dst)),
                    TestEdge(u32::from(src)),
                )
                .unwrap_or_else(|_| panic!("src label {i}"));
        }

        // Deferred queue is allowed to be non-empty (post-insert dense marks).
        // What matters is that every src label is readable in both orientations.
        // Drain any deferred compaction before reading so the regression isolates
        // the *insert* path, not the scan path.
        graph
            .maintenance(unbounded_budget())
            .expect("drain before verification");
        for i in 0..EXTRA_LABELS {
            let label = BucketLabelKey::directed_from_index(u16::try_from(3 + i).unwrap());
            let mut forward = Vec::new();
            graph
                .for_each_out_edges_for_label(src, label, |edge| forward.push(edge))
                .expect("read forward");
            assert_eq!(forward.len(), 1, "label {i} forward degree");
        }
    }

    #[test]
    fn sync_delete_vertex_drains_hub_and_preserves_neighbor_edges() {
        // Directed hub 0 -> neighbors 1..=DEG (exercises slab growth + overflow),
        // plus a directed in-edge, a self-loop, and a neighbour that keeps an edge to
        // a survivor vertex. The drain rewrite must remove every owner row and every
        // mirror at neighbours, while leaving unrelated edges (and counts) intact.
        const DEG: u32 = 300;
        let graph = sized_graph(1 << 16);
        let hub = graph.push_vertex().expect("hub"); // 0
        let label = BucketLabelKey::directed_from_index(7);
        for _ in 0..DEG {
            let neighbor = graph.push_vertex().expect("neighbor");
            graph
                .insert_directed_edge(
                    hub,
                    neighbor,
                    label,
                    TestEdge(u32::from(neighbor)),
                    TestEdge(u32::from(hub)),
                )
                .expect("hub -> neighbor");
        }
        // Survivor vertex with an edge from a neighbour that must outlive the hub delete.
        // Use a neighbour in a far leaf (not a leaf-mate of the hub) so its insert does
        // not rebalance the hub's own leaf.
        let survivor = graph.push_vertex().expect("survivor");
        let keeper = VertexId::from(250);
        graph
            .insert_directed_edge(
                survivor,
                hub,
                label,
                TestEdge(u32::from(hub)),
                TestEdge(u32::from(survivor)),
            )
            .expect("survivor -> hub");
        graph
            .insert_directed_edge(
                keeper,
                survivor,
                label,
                TestEdge(u32::from(survivor)),
                TestEdge(u32::from(keeper)),
            )
            .expect("keeper -> survivor");
        graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("settle inserts");

        // Sanity: before deletion the hub stores a forward out-edge to every neighbour.
        let pre_out = graph.directed_out_edges(hub).expect("hub out pre-delete");
        let mut pre_targets: Vec<u32> = pre_out
            .iter()
            .map(|e| u32::from(e.neighbor_vid()))
            .collect();
        pre_targets.sort_unstable();
        let missing: Vec<u32> = (1..=DEG).filter(|n| !pre_targets.contains(n)).collect();
        assert!(
            missing.is_empty(),
            "hub missing forward edges pre-delete: {missing:?}"
        );

        graph.delete_vertex_deferred(hub).expect("detach delete");
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

        // Hub fully drained.
        assert!(!graph.has_incident_edges(hub).expect("hub incident"));
        // Every neighbour lost its back-edge from the hub...
        let mut still_linked: Vec<u32> = Vec::new();
        for n in 1..=DEG {
            let nbr = VertexId::from(n);
            let in_edges = graph.directed_in_edges(nbr).expect("neighbor in-edges");
            if in_edges.iter().any(|e| e.neighbor_vid() == hub) {
                still_linked.push(n);
            }
        }
        assert!(
            still_linked.is_empty(),
            "neighbors still linked to hub: {still_linked:?}"
        );
        // ...but the keeper retains its edge to the survivor, and the survivor keeps it.
        assert_eq!(
            graph.incident_degree(keeper).expect("keeper degree"),
            1,
            "keeper should retain exactly its survivor edge"
        );
        let survivor_in = graph.directed_in_edges(survivor).expect("survivor in");
        assert!(
            survivor_in.iter().any(|e| e.neighbor_vid() == keeper),
            "survivor lost keeper edge"
        );
        // Survivor's out-edge to the hub was a mirror that must be gone.
        let survivor_out = graph.directed_out_edges(survivor).expect("survivor out");
        assert!(
            survivor_out.iter().all(|e| e.neighbor_vid() != hub),
            "survivor still points at deleted hub"
        );
    }

    #[test]
    fn stepped_delete_vertex_drains_hub_one_edge_per_step() {
        // Same supernode shape as the sync drain, but driven through the resumable
        // `MaintenanceWorkItem::DeleteVertex` step path with a one-edge-per-step budget.
        // Each maintenance call must remove exactly one incident edge (owner + mirror)
        // until the row is empty, then finalize once.
        const DEG: u32 = 300;
        let graph = sized_graph(1 << 16);
        let hub = graph.push_vertex().expect("hub"); // 0
        let label = BucketLabelKey::directed_from_index(7);
        for _ in 0..DEG {
            let neighbor = graph.push_vertex().expect("neighbor");
            graph
                .insert_directed_edge(
                    hub,
                    neighbor,
                    label,
                    TestEdge(u32::from(neighbor)),
                    TestEdge(u32::from(hub)),
                )
                .expect("hub -> neighbor");
        }
        let survivor = graph.push_vertex().expect("survivor");
        let keeper = VertexId::from(250);
        graph
            .insert_directed_edge(
                survivor,
                hub,
                label,
                TestEdge(u32::from(hub)),
                TestEdge(u32::from(survivor)),
            )
            .expect("survivor -> hub");
        graph
            .insert_directed_edge(
                keeper,
                survivor,
                label,
                TestEdge(u32::from(survivor)),
                TestEdge(u32::from(keeper)),
            )
            .expect("keeper -> survivor");
        let full = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        graph.maintenance(full).expect("settle inserts");

        let incident_before = graph.incident_degree(hub).expect("incident before");
        assert_eq!(
            incident_before,
            u64::from(DEG + 1),
            "hub owns DEG out + 1 in"
        );

        graph.enqueue_vertex_delete(hub).expect("enqueue");
        let one_step = MaintenanceBudget {
            max_delete_edge_steps: Some(1),
            ..full
        };
        let mut edge_steps = 0u32;
        let mut completed = 0u32;
        for _ in 0..(DEG + 8) {
            let report = graph.maintenance(one_step).expect("step");
            assert!(
                report.work.processed_delete_edge_steps <= 1,
                "step budget exceeded"
            );
            edge_steps = edge_steps.saturating_add(report.work.processed_delete_edge_steps);
            completed = completed.saturating_add(report.work.completed_vertex_deletes);
            if report.remaining_queue_len() == 0 {
                break;
            }
        }
        assert_eq!(
            u64::from(edge_steps),
            incident_before,
            "one removal per incident edge"
        );
        assert_eq!(completed, 1, "vertex purge completes exactly once");
        assert!(!graph.has_incident_edges(hub).expect("hub incident"));

        let mut still_linked: Vec<u32> = Vec::new();
        for n in 1..=DEG {
            let nbr = VertexId::from(n);
            let in_edges = graph.directed_in_edges(nbr).expect("neighbor in-edges");
            if in_edges.iter().any(|e| e.neighbor_vid() == hub) {
                still_linked.push(n);
            }
        }
        assert!(
            still_linked.is_empty(),
            "neighbors still linked to hub: {still_linked:?}"
        );
        assert_eq!(
            graph.incident_degree(keeper).expect("keeper degree"),
            1,
            "keeper should retain exactly its survivor edge"
        );
        let survivor_out = graph.directed_out_edges(survivor).expect("survivor out");
        assert!(
            survivor_out.iter().all(|e| e.neighbor_vid() != hub),
            "survivor still points at deleted hub"
        );
    }

    #[test]
    fn stepped_delete_bypass_vertex_with_interior_tombstone() {
        // A small (bypass-stored) vertex with an interior tombstone: removing a middle
        // edge first leaves a gap that the bypass row does not compact. The stepped
        // drain's top-slot primitive must still drain every live edge via its
        // descending fallback scan and clean up each neighbour.
        let graph = graph();
        let center = graph.push_vertex().expect("center"); // 0
        let label = BucketLabelKey::directed_from_index(3);
        let mut neighbors = Vec::new();
        for _ in 0..5u32 {
            neighbors.push(graph.push_vertex().expect("neighbor"));
        }
        for &n in &neighbors {
            graph
                .insert_directed_edge(
                    center,
                    n,
                    label,
                    TestEdge(u32::from(n)),
                    TestEdge(u32::from(center)),
                )
                .expect("center -> neighbor");
        }
        let full = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        graph.maintenance(full).expect("settle");

        // Tombstone the middle edge (center -> neighbors[2]) before deleting the vertex.
        let gap = neighbors[2];
        assert!(
            graph
                .remove_directed_deferred(center, gap, TestEdge(u32::from(gap)))
                .expect("remove middle edge")
        );

        graph.enqueue_vertex_delete(center).expect("enqueue");
        graph.maintenance(full).expect("drain");

        assert!(!graph.has_incident_edges(center).expect("center incident"));
        for &n in &neighbors {
            let in_edges = graph.directed_in_edges(n).expect("neighbor in");
            assert!(
                in_edges.iter().all(|e| e.neighbor_vid() != center),
                "neighbor {n:?} still linked to deleted center"
            );
        }
    }

    #[test]
    fn delete_vertex_after_mixed_bucket_and_bypass_edge() {
        let (
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
            fvs,
            fvffs,
            fvffsbs,
            fvlog,
            fvblobs,
        ) = labeled_lara_memories();
        let (
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
            rvs,
            rvffs,
            rvffsbs,
            rvlog,
            rvblobs,
        ) = labeled_lara_memories();
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
            fvs,
            fvffs,
            fvffsbs,
            fvlog,
            fvblobs,
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
            rvs,
            rvffs,
            rvffsbs,
            rvlog,
            rvblobs,
            vector_memory(),
            vector_memory(),
            crate::labeled::InitialCapacities::uniform(128),
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
    fn directed_out_edges_iter_matches_asc_materialization() {
        let graph = graph();
        graph.push_vertex().expect("a");
        graph.push_vertex().expect("b");
        graph.push_vertex().expect("c");
        let label_lo = BucketLabelKey::directed_from_index(10);
        let label_hi = BucketLabelKey::directed_from_index(20);
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
        let asc = graph.directed_out_edges(VertexId::from(0)).unwrap();
        let desc: Vec<_> = graph
            .directed_out_edges_iter(VertexId::from(0), OutEdgeOrder::Descending)
            .expect("iter")
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(asc, vec![TestEdge(1), TestEdge(2)]);
        assert_eq!(desc, vec![TestEdge(2), TestEdge(1)]);
    }

    #[test]
    fn incident_degree_counts_forward_and_reverse() {
        let graph = graph();
        graph.push_vertex().expect("a");
        graph.push_vertex().expect("b");
        graph.push_vertex().expect("c");
        let label = BucketLabelKey::UNLABELED_DIRECTED;
        // a(0) -> b(1), a(0) -> c(2): two forward out-edges at a.
        graph
            .insert_directed_edge(
                VertexId::from(0),
                VertexId::from(1),
                label,
                TestEdge(1),
                TestEdge(0),
            )
            .expect("a->b");
        graph
            .insert_directed_edge(
                VertexId::from(0),
                VertexId::from(2),
                label,
                TestEdge(2),
                TestEdge(0),
            )
            .expect("a->c");
        // c(2) -> a(0): one reverse in-edge at a.
        graph
            .insert_directed_edge(
                VertexId::from(2),
                VertexId::from(0),
                label,
                TestEdge(0),
                TestEdge(2),
            )
            .expect("c->a");
        // Vertex-row degree (read by both `has_incident_edges` and `incident_degree`)
        // is materialized during maintenance; drain it as the facade path does.
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
        assert_eq!(graph.incident_degree(VertexId::from(0)).expect("a"), 3);
        assert_eq!(graph.incident_degree(VertexId::from(1)).expect("b"), 1);
        assert_eq!(graph.incident_degree(VertexId::from(2)).expect("c"), 2);
        assert!(graph.incident_degree(VertexId::from(99)).is_err());
    }

    #[test]
    fn delete_vertex_job_purges_incident_edges_phased() {
        let graph = graph();
        for _ in 0..4 {
            graph.push_vertex().expect("vertex");
        }
        let hub = VertexId::from(0);
        let label = BucketLabelKey::UNLABELED_DIRECTED;
        graph
            .insert_directed_edge(hub, VertexId::from(1), label, TestEdge(1), TestEdge(0))
            .expect("hub->1");
        graph
            .insert_directed_edge(hub, VertexId::from(2), label, TestEdge(2), TestEdge(0))
            .expect("hub->2");
        graph
            .insert_directed_edge(VertexId::from(3), hub, label, TestEdge(0), TestEdge(3))
            .expect("3->hub");

        let full = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        // Drain pre-existing compaction work so the phased drain only sees the job.
        graph.maintenance(full).expect("pre-drain");
        assert_eq!(graph.incident_degree(hub).expect("degree"), 3);

        graph.enqueue_vertex_delete(hub).expect("enqueue");

        let one_step = MaintenanceBudget {
            max_delete_edge_steps: Some(1),
            ..full
        };
        let mut edge_steps = 0u32;
        let mut completed = 0u32;
        let mut calls = 0u32;
        for _ in 0..16 {
            let report = graph.maintenance(one_step).expect("step");
            edge_steps = edge_steps.saturating_add(report.work.processed_delete_edge_steps);
            completed = completed.saturating_add(report.work.completed_vertex_deletes);
            calls += 1;
            if report.remaining_queue_len() == 0 {
                break;
            }
        }
        // 3 incident edges removed one per step, plus a final finalize step.
        assert_eq!(edge_steps, 3, "one removal per incident edge");
        assert_eq!(completed, 1, "vertex purge completes exactly once");
        assert_eq!(
            calls, 4,
            "3 edge steps + 1 finalize, one delete step per call"
        );
        assert_eq!(graph.incident_degree(hub).expect("degree after"), 0);
        assert!(!graph.has_incident_edges(hub).expect("incident after"));
        assert!(
            graph
                .directed_in_edges(VertexId::from(1))
                .expect("in 1")
                .is_empty(),
            "neighbor 1 keeps no dangling in-edge"
        );
        assert!(
            graph
                .directed_in_edges(VertexId::from(2))
                .expect("in 2")
                .is_empty()
        );
        assert!(
            graph
                .directed_out_edges(VertexId::from(3))
                .expect("out 3")
                .is_empty(),
            "neighbor 3 keeps no dangling out-edge"
        );
    }

    #[test]
    fn delete_vertex_work_item_round_trips() {
        use ic_stable_structures::Storable;
        let item = MaintenanceWorkItem::DeleteVertex {
            vid: VertexId::from(7),
            removed_edges: 12_345,
        };
        let bytes = item.to_bytes();
        assert_eq!(
            bytes.len(),
            16,
            "delete work item fits the fixed 16-byte format"
        );
        assert_eq!(MaintenanceWorkItem::from_bytes(bytes), item);
    }

    #[test]
    fn payload_compaction_work_item_round_trips_and_runs_once() {
        use ic_stable_structures::Storable;
        let item = MaintenanceWorkItem::CompactPayloadSlab {
            orientation: Orientation::Reverse,
        };
        assert_eq!(MaintenanceWorkItem::from_bytes(item.to_bytes()), item);

        let graph = graph();
        graph
            .mark_compact_payload_slab(Orientation::Forward)
            .unwrap();
        graph
            .mark_compact_payload_slab(Orientation::Forward)
            .unwrap();
        assert_eq!(graph.maintenance_queue_len(), 1);
        graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: Some(1),
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .unwrap();
        assert_eq!(graph.maintenance_queue_len(), 0);
    }

    #[test]
    fn delete_vertex_job_is_idempotent() {
        let graph = graph();
        for _ in 0..2 {
            graph.push_vertex().expect("vertex");
        }
        let hub = VertexId::from(0);
        let label = BucketLabelKey::UNLABELED_DIRECTED;
        graph
            .insert_directed_edge(hub, VertexId::from(1), label, TestEdge(1), TestEdge(0))
            .expect("hub->1");
        let full = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };

        graph.enqueue_vertex_delete(hub).expect("enqueue");
        let first = graph.maintenance(full).expect("drain 1");
        assert_eq!(first.work.completed_vertex_deletes, 1);
        assert!(!graph.has_incident_edges(hub).expect("incident"));

        // Re-enqueuing an already-purged vertex finalizes safely with no edge work.
        graph.enqueue_vertex_delete(hub).expect("enqueue again");
        let second = graph.maintenance(full).expect("drain 2");
        assert_eq!(second.work.completed_vertex_deletes, 1);
        assert_eq!(second.work.processed_delete_edge_steps, 0);
        assert!(!graph.has_incident_edges(hub).expect("incident again"));
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
        graph.forward().compact_vertex_edge_span(hub, 0).unwrap();
        for _ in 0..72 {
            graph
                .forward()
                .remove_edge_matching(hub, label, |edge| edge.0 == 1)
                .unwrap();
        }
        let before = graph.forward().vertices().get(hub);
        assert!(before.stored_slots > 8);

        graph
            .mark_compact_vertex_edge_span(Orientation::Forward, hub, 0)
            .expect("mark");
        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        while graph.maintenance_queue_len() > 0 {
            graph.maintenance(budget).expect("maintenance");
        }

        let after = graph.forward().vertices().get(hub);
        assert_eq!(after.stored_slots, 9);
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct PayloadTestEdge {
        target: u32,
        slot_index: u32,
        value: [u8; 8],
        inline_value_len: u16,
    }

    impl PayloadTestEdge {
        fn with_bytes(target: u32, bytes: &[u8]) -> Self {
            let mut value = [0u8; 8];
            let len = bytes.len().min(8);
            value[..len].copy_from_slice(&bytes[..len]);
            Self {
                target,
                slot_index: 0,
                value,
                inline_value_len: u16::try_from(len).expect("test payload fits u16 width"),
            }
        }
    }

    impl CsrEdge for PayloadTestEdge {
        const BYTES: usize = 4;

        fn read_from(bytes: &[u8]) -> Self {
            Self {
                target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
                slot_index: 0,
                value: [0u8; 8],
                inline_value_len: 0,
            }
        }

        fn write_to(&self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.target)
        }

        fn with_neighbor_vid(&self, vid: VertexId) -> Self {
            Self {
                target: u32::from(vid),
                ..*self
            }
        }

        fn with_slot_index(self, slot_index: u32) -> Self {
            Self { slot_index, ..self }
        }

        fn edge_inline_value_byte_width(&self) -> u16 {
            self.inline_value_len
        }

        fn edge_inline_value_bytes(&self) -> &[u8] {
            &self.value[..usize::from(self.inline_value_len)]
        }

        fn with_stored_inline_value_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
            self.value = [0u8; 8];
            let len = usize::from(width).min(bytes.len()).min(8);
            self.value[..len].copy_from_slice(&bytes[..len]);
            self.inline_value_len = width;
            self
        }

        fn edge_slot_index_raw(&self) -> u32 {
            self.slot_index
        }
    }

    impl CsrEdgeTombstone for PayloadTestEdge {
        fn tombstone_edge() -> Self {
            Self {
                target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
                slot_index: 0,
                value: [0u8; 8],
                inline_value_len: 0,
            }
        }
    }

    fn valued_bidirectional_graph()
    -> DeferredBidirectionalLabeledLaraGraph<PayloadTestEdge, VectorMemory> {
        let (
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
            fvs,
            fvffs,
            fvffsbs,
            fvlog,
            fvblobs,
        ) = labeled_lara_memories();
        let (
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
            rvs,
            rvffs,
            rvffsbs,
            rvlog,
            rvblobs,
        ) = labeled_lara_memories();
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
            fvs,
            fvffs,
            fvffsbs,
            fvlog,
            fvblobs,
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
            rvs,
            rvffs,
            rvffsbs,
            rvlog,
            rvblobs,
            vector_memory(),
            vector_memory(),
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::UNLABELED_DIRECTED,
        )
        .expect("graph")
    }

    #[test]
    fn bidirectional_parallel_edge_inline_values_survive_diamond_insert() {
        let graph = valued_bidirectional_graph();
        for _ in 0..4 {
            graph.push_vertex().unwrap();
        }
        let road = BucketLabelKey::directed_from_index(2);
        let rev = |src: u32| PayloadTestEdge::with_bytes(src, &0u16.to_le_bytes());
        graph
            .ensure_directed_edge_inline_value_width(
                VertexId::from(0),
                VertexId::from(2),
                road,
                2u16,
            )
            .unwrap();
        graph
            .ensure_directed_edge_inline_value_width(
                VertexId::from(0),
                VertexId::from(1),
                road,
                2u16,
            )
            .unwrap();
        graph
            .ensure_directed_edge_inline_value_width(
                VertexId::from(1),
                VertexId::from(2),
                road,
                2u16,
            )
            .unwrap();
        graph
            .insert_directed_edge(
                VertexId::from(0),
                VertexId::from(2),
                road,
                PayloadTestEdge::with_bytes(2, &10u16.to_le_bytes()),
                rev(0),
            )
            .unwrap();
        graph
            .insert_directed_edge(
                VertexId::from(0),
                VertexId::from(1),
                road,
                PayloadTestEdge::with_bytes(1, &5u16.to_le_bytes()),
                rev(0),
            )
            .unwrap();
        graph
            .insert_directed_edge(
                VertexId::from(1),
                VertexId::from(2),
                road,
                PayloadTestEdge::with_bytes(2, &1u16.to_le_bytes()),
                rev(1),
            )
            .unwrap();
        let mut weights = Vec::new();
        graph
            .for_each_out_edges_for_label_unchecked(VertexId::from(0), road, |edge| {
                if edge.inline_value_len == 2 {
                    let b = edge.edge_inline_value_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        weights.sort_unstable();
        assert_eq!(weights, vec![5, 10]);
    }

    #[test]
    fn remove_directed_deferred_uses_edge_inline_value_to_select_parallel_edge() {
        let graph = valued_bidirectional_graph();
        for _ in 0..2 {
            graph.push_vertex().unwrap();
        }
        let road = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_directed_edge_inline_value_width(
                VertexId::from(0),
                VertexId::from(1),
                road,
                2u16,
            )
            .unwrap();
        let rev = |src: u32| PayloadTestEdge::with_bytes(src, &0u16.to_le_bytes());
        graph
            .insert_directed_edge(
                VertexId::from(0),
                VertexId::from(1),
                road,
                PayloadTestEdge::with_bytes(1, &10u16.to_le_bytes()),
                rev(0),
            )
            .unwrap();
        graph
            .insert_directed_edge(
                VertexId::from(0),
                VertexId::from(1),
                road,
                PayloadTestEdge::with_bytes(1, &20u16.to_le_bytes()),
                rev(0),
            )
            .unwrap();

        assert!(
            graph
                .remove_directed_deferred(
                    VertexId::from(0),
                    VertexId::from(1),
                    PayloadTestEdge::with_bytes(1, &20u16.to_le_bytes()),
                )
                .unwrap()
        );

        let mut weights = Vec::new();
        graph
            .for_each_out_edges_for_label_unchecked(VertexId::from(0), road, |edge| {
                if edge.inline_value_len == 2 {
                    let b = edge.edge_inline_value_bytes();
                    weights.push(u16::from_le_bytes([b[0], b[1]]));
                }
            })
            .unwrap();
        assert_eq!(weights, vec![10]);
        assert_eq!(
            graph
                .in_edges_for_label(VertexId::from(1), road)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn remove_undirected_deferred_uses_edge_inline_value_to_select_parallel_edge() {
        let graph = valued_bidirectional_graph();
        for _ in 0..2 {
            graph.push_vertex().unwrap();
        }
        let road = BucketLabelKey::undirected_from_index(2);
        graph
            .forward()
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .forward()
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(1), road, 2u16)
            .unwrap();
        graph
            .insert_undirected_deferred(
                VertexId::from(0),
                VertexId::from(1),
                road,
                PayloadTestEdge::with_bytes(1, &10u16.to_le_bytes()),
                PayloadTestEdge::with_bytes(0, &10u16.to_le_bytes()),
            )
            .unwrap();
        graph
            .insert_undirected_deferred(
                VertexId::from(0),
                VertexId::from(1),
                road,
                PayloadTestEdge::with_bytes(1, &20u16.to_le_bytes()),
                PayloadTestEdge::with_bytes(0, &20u16.to_le_bytes()),
            )
            .unwrap();

        assert!(
            graph
                .remove_undirected_deferred(
                    VertexId::from(0),
                    VertexId::from(1),
                    PayloadTestEdge::with_bytes(1, &20u16.to_le_bytes()),
                )
                .unwrap()
        );

        let weights_from = |vertex| {
            let mut weights = Vec::new();
            graph
                .for_each_undirected_edges(vertex, OutEdgeOrder::Ascending, |edge| {
                    if edge.inline_value_len == 2 {
                        let b = edge.edge_inline_value_bytes();
                        weights.push(u16::from_le_bytes([b[0], b[1]]));
                    }
                })
                .unwrap();
            weights
        };
        assert_eq!(weights_from(VertexId::from(0)), vec![10]);
        assert_eq!(weights_from(VertexId::from(1)), vec![10]);
    }

    /// Proves the reverse payload-first phase-2 read actually reuses the phase-1 replay: many edges
    /// point into one hub, so the hub's reverse bucket is an overflow-log hybrid. Reading the
    /// in-edge slots with the captured replay must avoid the overflow-log chain rebuild (0), while a
    /// no-replay read takes the sparse fallback (>= 1) — both returning the same incoming edges. This
    /// guards `read_in_edge_slots_for_label_with_replay`, which the incoming payload-first expand
    /// executor depends on.
    #[test]
    fn read_in_edge_slots_for_label_with_replay_reuses_reverse_replay() {
        use crate::lara::edge::scan_guard::ScanPathGuard;

        let graph = valued_bidirectional_graph();
        const SOURCES: u32 = 40;
        for _ in 0..=SOURCES {
            graph.push_vertex().unwrap();
        }
        let hub = VertexId::from(0);
        let road = BucketLabelKey::directed_from_index(2);
        for src in 1..=SOURCES {
            let bytes = (src as u16).to_le_bytes();
            graph
                .ensure_directed_edge_inline_value_width(VertexId::from(src), hub, road, 2u16)
                .unwrap();
            graph
                .insert_directed_edge(
                    VertexId::from(src),
                    hub,
                    road,
                    PayloadTestEdge::with_bytes(u32::from(hub), &bytes),
                    PayloadTestEdge::with_bytes(src, &bytes),
                )
                .unwrap();
        }

        // Phase 1: capture the reverse hybrid replay and the slot order it emits for the hub.
        let mut scratch = crate::labeled::LabeledPayloadValueBatchScratch::default();
        let mut slots = Vec::new();
        graph
            .visit_in_inline_value_batches_for_label(
                hub,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| slots.extend_from_slice(batch.slot_indices),
            )
            .unwrap();
        assert!(scratch.hybrid_overflow_replay.is_active());

        let read_in = |replay: Option<&crate::labeled::HybridOverflowEdgeReplay>| {
            let mut sources = Vec::new();
            graph
                .read_in_edge_slots_for_label_with_replay(
                    hub,
                    road,
                    &slots,
                    OutEdgeOrder::Ascending,
                    replay,
                    |edge| sources.push(u32::from(edge.neighbor_vid())),
                )
                .unwrap();
            sources.sort_unstable();
            sources
        };

        let (with_replay, rebuilds_with_replay) = {
            let _guard = ScanPathGuard::enter();
            let sources = read_in(Some(&scratch.hybrid_overflow_replay));
            (sources, ScanPathGuard::overflow_chain_rebuilds())
        };
        let (without_replay, rebuilds_without_replay) = {
            let _guard = ScanPathGuard::enter();
            let sources = read_in(None);
            (sources, ScanPathGuard::overflow_chain_rebuilds())
        };

        assert_eq!(with_replay, (1..=SOURCES).collect::<Vec<_>>());
        assert_eq!(with_replay, without_replay);
        assert_eq!(
            rebuilds_with_replay, 0,
            "reverse phase-2 must reuse the reverse phase-1 replay, not rebuild the overflow chain"
        );
        assert!(
            rebuilds_without_replay >= 1,
            "no-replay reverse read takes the sparse fallback that rebuilds the overflow chain"
        );
    }

    #[test]
    fn directed_inline_value_adjacent_reverse_hub_stays_writable_after_skew() {
        let graph = valued_bidirectional_graph();
        for _ in 0..3 {
            graph.push_vertex().unwrap();
        }
        let noise_dst = VertexId::from(0);
        let target_dst = VertexId::from(1);
        let hub = VertexId::from(2);
        let road = BucketLabelKey::directed_from_index(2);
        for edge_index in 0..2_000u32 {
            let bytes = 1u16.to_le_bytes();
            graph
                .ensure_directed_edge_inline_value_width(hub, noise_dst, road, 2)
                .unwrap_or_else(|error| panic!("noise payload schema {edge_index}: {error:?}"));
            graph
                .insert_directed_edge(
                    hub,
                    noise_dst,
                    road,
                    PayloadTestEdge::with_bytes(u32::from(noise_dst), &bytes),
                    PayloadTestEdge::with_bytes(u32::from(hub), &bytes),
                )
                .unwrap_or_else(|error| panic!("noise edge {edge_index}: {error:?}"));
        }

        for edge_index in 0..100u32 {
            let bytes = 7u16.to_le_bytes();
            graph
                .ensure_directed_edge_inline_value_width(hub, target_dst, road, 2)
                .unwrap_or_else(|error| panic!("target payload schema {edge_index}: {error:?}"));
            graph
                .insert_directed_edge(
                    hub,
                    target_dst,
                    road,
                    PayloadTestEdge::with_bytes(u32::from(target_dst), &bytes),
                    PayloadTestEdge::with_bytes(u32::from(hub), &bytes),
                )
                .unwrap_or_else(|error| panic!("target edge {edge_index}: {error:?}"));
        }

        assert_eq!(
            graph.in_edges_for_label(noise_dst, road).unwrap().len(),
            2_000
        );
        assert_eq!(
            graph.in_edges_for_label(target_dst, road).unwrap().len(),
            100
        );
    }

    #[test]
    fn scan_only_mate_resolves_parallel_directed_edges_and_canonical_handle() {
        let graph = valued_bidirectional_graph();
        for _ in 0..3 {
            graph.push_vertex().unwrap();
        }
        let source = VertexId::from(0);
        let target = VertexId::from(1);
        let label = BucketLabelKey::directed_from_index(7);
        graph
            .ensure_directed_edge_inline_value_width(source, target, label, 1)
            .unwrap();
        for value in 1..=3u8 {
            graph
                .insert_directed_edge(
                    source,
                    target,
                    label,
                    PayloadTestEdge::with_bytes(u32::from(target), &[value]),
                    PayloadTestEdge::with_bytes(u32::from(source), &[value]),
                )
                .unwrap();
        }
        graph.forward().compact_vertex_edge_span(source, 0).unwrap();
        graph.reverse().compact_vertex_edge_span(target, 0).unwrap();
        let mut source_slots = Vec::new();
        graph
            .forward()
            .for_each_live_edge_slot_for_label(source, label, |slot, _| source_slots.push(slot))
            .unwrap();
        let source_slot = source_slots[1];
        let source_ref = PhysicalEdgeRef {
            orientation: Orientation::Forward,
            owner_vertex_id: source,
            label_id: label,
            slot_index: source_slot,
        };
        let mate = graph.mate_of(source_ref).unwrap();
        assert_eq!(mate.orientation, Orientation::Reverse);
        assert_eq!(mate.owner_vertex_id, target);
        assert_eq!(graph.canonical_handle(mate).unwrap(), source_ref);
    }

    #[test]
    fn scalar_insert_returns_exact_forward_and_reverse_locations() {
        let graph = graph();
        graph.push_vertex().unwrap();
        graph.push_vertex().unwrap();
        let label = BucketLabelKey::directed_from_index(11);
        let locations = graph
            .insert_directed_edge_with_locations(
                VertexId::from(0),
                VertexId::from(1),
                label,
                TestEdge(1),
                TestEdge(0),
            )
            .unwrap();
        let forward = locations.forward.expect("named forward bucket location");
        let reverse = locations.reverse.expect("named reverse bucket location");
        let mut forward_slots = Vec::new();
        graph
            .forward()
            .for_each_live_edge_slot_for_label(VertexId::from(0), label, |slot, edge| {
                forward_slots.push((slot, edge.neighbor_vid()));
            })
            .unwrap();
        let mut reverse_slots = Vec::new();
        graph
            .reverse()
            .for_each_live_edge_slot_for_label(VertexId::from(1), label, |slot, edge| {
                reverse_slots.push((slot, edge.neighbor_vid()));
            })
            .unwrap();
        assert_eq!(
            forward_slots,
            vec![(forward.logical_slot, VertexId::from(1))]
        );
        assert_eq!(
            reverse_slots,
            vec![(reverse.logical_slot, VertexId::from(0))]
        );
    }

    #[test]
    fn scalar_undirected_location_pair_preserves_owner_and_self_loop_shape() {
        let graph = graph();
        for _ in 0..3 {
            graph.push_vertex().unwrap();
        }
        let label = BucketLabelKey::undirected_from_index(12);
        let pair = graph
            .insert_undirected_deferred_with_locations(
                VertexId::from(0),
                VertexId::from(2),
                label,
                TestEdge(2),
                TestEdge(0),
            )
            .unwrap();
        assert!(pair.forward.is_some());
        assert!(pair.reverse.is_some());

        let self_pair = graph
            .insert_undirected_deferred_with_locations(
                VertexId::from(1),
                VertexId::from(1),
                label,
                TestEdge(1),
                TestEdge(1),
            )
            .unwrap();
        assert!(self_pair.forward.is_some());
        assert!(self_pair.reverse.is_none());
    }

    #[test]
    fn scan_only_mate_resolves_undirected_pairs_and_self_loops() {
        let graph = graph();
        for _ in 0..3 {
            graph.push_vertex().unwrap();
        }
        let low = VertexId::from(0);
        let high = VertexId::from(1);
        let label = BucketLabelKey::undirected_from_index(9);
        graph
            .insert_undirected_deferred(
                low,
                high,
                label,
                TestEdge(u32::from(high)),
                TestEdge(u32::from(low)),
            )
            .unwrap();
        let mut low_slots = Vec::new();
        graph
            .forward()
            .for_each_live_edge_slot_for_label(low, label, |slot, _| low_slots.push(slot))
            .unwrap();
        let low_slot = low_slots[0];
        let low_ref = PhysicalEdgeRef {
            orientation: Orientation::Forward,
            owner_vertex_id: low,
            label_id: label,
            slot_index: low_slot,
        };
        let high_ref = graph.mate_of(low_ref).unwrap();
        assert_eq!(high_ref.owner_vertex_id, high);
        assert_eq!(graph.canonical_handle(low_ref).unwrap(), high_ref);

        let self_loop = VertexId::from(2);
        graph
            .insert_undirected_deferred(
                self_loop,
                self_loop,
                label,
                TestEdge(u32::from(self_loop)),
                TestEdge(u32::from(self_loop)),
            )
            .unwrap();
        let mut self_slots = Vec::new();
        graph
            .forward()
            .for_each_live_edge_slot_for_label(self_loop, label, |slot, _| self_slots.push(slot))
            .unwrap();
        let self_slot = self_slots[0];
        let self_ref = PhysicalEdgeRef {
            orientation: Orientation::Forward,
            owner_vertex_id: self_loop,
            label_id: label,
            slot_index: self_slot,
        };
        assert_eq!(graph.mate_of(self_ref).unwrap(), self_ref);
    }

    #[test]
    fn scan_only_mate_fails_closed_for_missing_source_and_invalid_orientation() {
        let graph = graph();
        for _ in 0..2 {
            graph.push_vertex().unwrap();
        }
        let label = BucketLabelKey::undirected_from_index(11);
        graph
            .insert_undirected_deferred(
                VertexId::from(0),
                VertexId::from(1),
                label,
                TestEdge(1),
                TestEdge(0),
            )
            .unwrap();
        let missing = PhysicalEdgeRef {
            orientation: Orientation::Forward,
            owner_vertex_id: VertexId::from(0),
            label_id: label,
            slot_index: u32::MAX,
        };
        assert!(matches!(
            graph.mate_of(missing),
            Err(crate::labeled::MateLookupError::SourceNotFound(_))
        ));
        let invalid = PhysicalEdgeRef {
            orientation: Orientation::Reverse,
            ..missing
        };
        assert!(matches!(
            graph.mate_of(invalid),
            Err(crate::labeled::MateLookupError::InvalidOrientation(_))
        ));
    }
}
