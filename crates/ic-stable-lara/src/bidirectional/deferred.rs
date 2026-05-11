//! Deferred-maintenance bidirectional LARA graph wrapper.

use crate::{
    GrowFailed, SegmentId, VertexCount, VertexId,
    bidirectional::UndirectedEdgeFlag,
    lara::{
        InitError, LaraGraph, MarkPriority,
        edge::OutEdgesIter,
        maintenance::{DeferredConfig, DeferredError, MaintenanceBudget, MaintenanceWorkReport},
        operation_error::LaraOperationError,
    },
    traits::{CsrEdge, CsrEdgeUndirected, CsrVertex},
};
use ic_stable_roaring::{BitmapError, InitError as RoaringInitError, StableRoaringBitmap};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use ic_stable_vec_deque::{
    GrowFailed as QueueGrowFailed, InitError as QueueInitError, StableVecDeque,
};
use std::{borrow::Cow, fmt};

#[cfg(feature = "canbench")]
mod bench;

/// Maintenance report for a deferred bidirectional graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BidirectionalMaintenanceReport {
    /// Aggregated work performed by the unified queue.
    pub work: MaintenanceWorkReport,
    /// Instruction-counter value observed at the end of the run.
    pub instructions_used: u64,
    /// Whether the instruction budget stopped the run.
    pub instruction_budget_exhausted: bool,
}

impl BidirectionalMaintenanceReport {
    /// Returns total remaining queued work items.
    pub fn remaining_queue_len(self) -> u64 {
        self.work.remaining_queue_len
    }
}

/// Observer for edge records removed by incremental vertex-delete maintenance.
pub trait DeleteEdgeObserver<E> {
    /// Called when deleting a vertex removes one outgoing edge from the forward row.
    fn on_delete_outgoing_edge(&mut self, _source: VertexId, _edge: E) {}

    /// Called when deleting a vertex removes one incoming edge from the reverse row.
    fn on_delete_incoming_edge(&mut self, _destination: VertexId, _edge: E) {}
}

#[derive(Default)]
struct NoopDeleteEdgeObserver;

impl<E> DeleteEdgeObserver<E> for NoopDeleteEdgeObserver {}

fn add_step_report(total: &mut MaintenanceWorkReport, step: MaintenanceWorkReport) {
    total.processed_work_items = total
        .processed_work_items
        .saturating_add(step.processed_work_items);
    total.processed_segments = total
        .processed_segments
        .saturating_add(step.processed_segments);
    total.rebalanced_segments = total
        .rebalanced_segments
        .saturating_add(step.rebalanced_segments);
    total.resized |= step.resized;
    total.processed_delete_edge_steps = total
        .processed_delete_edge_steps
        .saturating_add(step.processed_delete_edge_steps);
    total.completed_vertex_deletes = total
        .completed_vertex_deletes
        .saturating_add(step.completed_vertex_deletes);
    total.remaining_queue_len = step.remaining_queue_len;
}

/// Direction of a queued bidirectional maintenance item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    /// Forward out-adjacency store.
    Forward,
    /// Reverse in-adjacency store.
    Reverse,
}

/// Phase of an incremental vertex delete job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeletePhase {
    /// Remove reverse counterparts for outgoing forward edges.
    RemoveOutgoing,
    /// Clear the deleted vertex's forward row.
    ClearForwardRow,
    /// Remove forward counterparts for incoming reverse edges.
    RemoveIncoming,
    /// Clear the deleted vertex's reverse row.
    ClearReverseRow,
}

/// One item in the unified deferred bidirectional maintenance queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceWorkItem {
    /// Rebalance one segment in one orientation.
    Rebalance {
        /// Store orientation.
        orientation: Orientation,
        /// Segment to inspect/rebalance.
        segment: SegmentId,
    },
    /// Incrementally remove all incident edges for a deleted vertex.
    DeleteVertex {
        /// Deleted vertex id.
        vid: VertexId,
        /// Current delete phase.
        phase: DeletePhase,
        /// Edge cursor within the source row for the current phase.
        cursor: u32,
        /// Number of incident edge steps already processed.
        removed_edges: u64,
    },
}

fn maintenance_work_item_bytes(item: &MaintenanceWorkItem) -> [u8; 24] {
    let mut b = [0u8; 24];
    match *item {
        MaintenanceWorkItem::Rebalance {
            orientation,
            segment,
        } => {
            b[0] = 0;
            b[1] = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            b[4..8].copy_from_slice(&u32::from(segment).to_le_bytes());
        }
        MaintenanceWorkItem::DeleteVertex {
            vid,
            phase,
            cursor,
            removed_edges,
        } => {
            b[0] = 1;
            b[2] = match phase {
                DeletePhase::RemoveOutgoing => 0,
                DeletePhase::ClearForwardRow => 1,
                DeletePhase::RemoveIncoming => 2,
                DeletePhase::ClearReverseRow => 3,
            };
            b[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
            b[8..12].copy_from_slice(&cursor.to_le_bytes());
            b[16..24].copy_from_slice(&removed_edges.to_le_bytes());
        }
    }
    b
}

impl Storable for MaintenanceWorkItem {
    const BOUND: Bound = Bound::Bounded {
        max_size: 24,
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
        let read_u32 = |start: usize| {
            let mut out = [0u8; 4];
            out.copy_from_slice(&b[start..start + 4]);
            u32::from_le_bytes(out)
        };
        let read_u64 = |start: usize| {
            let mut out = [0u8; 8];
            out.copy_from_slice(&b[start..start + 8]);
            u64::from_le_bytes(out)
        };
        match b[0] {
            0 => Self::Rebalance {
                orientation: if b[1] == 1 {
                    Orientation::Reverse
                } else {
                    Orientation::Forward
                },
                segment: SegmentId::from(read_u32(4)),
            },
            _ => Self::DeleteVertex {
                vid: VertexId::from(read_u32(4)),
                phase: match b[2] {
                    1 => DeletePhase::ClearForwardRow,
                    2 => DeletePhase::RemoveIncoming,
                    3 => DeletePhase::ClearReverseRow,
                    _ => DeletePhase::RemoveOutgoing,
                },
                cursor: read_u32(8),
                removed_edges: read_u64(16),
            },
        }
    }
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

#[derive(Debug)]
struct BidirectionalMaintenanceQueue<M: Memory> {
    queue: StableVecDeque<MaintenanceWorkItem, M>,
    dirty: StableRoaringBitmap<M>,
}

impl<M: Memory> BidirectionalMaintenanceQueue<M> {
    fn new(queue_memory: M, dirty_memory: M) -> Result<Self, DeferredBidirectionalLaraError> {
        Ok(Self {
            queue: StableVecDeque::new(queue_memory)
                .map_err(DeferredBidirectionalLaraError::MaintenanceQueue)?,
            dirty: StableRoaringBitmap::new(dirty_memory)
                .map_err(DeferredBidirectionalLaraError::MaintenanceDirtyBitmap)?,
        })
    }

    fn init(queue_memory: M, dirty_memory: M) -> Result<Self, DeferredBidirectionalLaraError> {
        Ok(Self {
            queue: StableVecDeque::init(queue_memory)
                .map_err(DeferredBidirectionalLaraError::MaintenanceQueueInit)?,
            dirty: StableRoaringBitmap::init(dirty_memory)
                .map_err(DeferredBidirectionalLaraError::MaintenanceDirtyInit)?,
        })
    }

    fn into_memories(self) -> (M, M) {
        (self.queue.into_memory(), self.dirty.into_memory())
    }

    fn len(&self) -> u64 {
        self.queue.len()
    }

    fn mark_dirty(
        &self,
        item: MaintenanceWorkItem,
    ) -> Result<bool, DeferredBidirectionalLaraError> {
        let key = work_item_key(item);
        if self.dirty.contains(key) {
            return Ok(false);
        }
        self.dirty
            .insert(key)
            .map_err(DeferredBidirectionalLaraError::MaintenanceDirtyBitmap)?;
        self.queue
            .push_back(&item)
            .map_err(DeferredBidirectionalLaraError::MaintenanceQueue)?;
        Ok(true)
    }

    fn mark_urgent(
        &self,
        item: MaintenanceWorkItem,
    ) -> Result<bool, DeferredBidirectionalLaraError> {
        let key = work_item_key(item);
        if self.dirty.contains(key) {
            return Ok(false);
        }
        self.dirty
            .insert(key)
            .map_err(DeferredBidirectionalLaraError::MaintenanceDirtyBitmap)?;
        self.queue
            .push_front(&item)
            .map_err(DeferredBidirectionalLaraError::MaintenanceQueue)?;
        Ok(true)
    }

    fn pop_next(&self) -> Result<Option<MaintenanceWorkItem>, DeferredBidirectionalLaraError> {
        while let Some(item) = self.queue.pop_front() {
            if self.dirty.contains(work_item_key(item)) {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    fn complete(&self, item: MaintenanceWorkItem) -> Result<(), DeferredBidirectionalLaraError> {
        self.dirty
            .clear(work_item_key(item))
            .map_err(DeferredBidirectionalLaraError::MaintenanceDirtyBitmap)
    }

    fn requeue_front(
        &self,
        item: MaintenanceWorkItem,
    ) -> Result<(), DeferredBidirectionalLaraError> {
        self.queue
            .push_front(&item)
            .map_err(DeferredBidirectionalLaraError::MaintenanceQueue)
    }
}

fn work_item_key(item: MaintenanceWorkItem) -> u32 {
    match item {
        MaintenanceWorkItem::Rebalance {
            orientation,
            segment,
        } => {
            let orient = match orientation {
                Orientation::Forward => 0,
                Orientation::Reverse => 1,
            };
            u32::from(segment).saturating_mul(2).saturating_add(orient)
        }
        MaintenanceWorkItem::DeleteVertex { vid, .. } => 0x8000_0000 | u32::from(vid),
    }
}

/// Errors returned by deferred bidirectional graph operations.
#[derive(Debug)]
pub enum DeferredBidirectionalLaraError {
    /// Forward store operation failed.
    Forward(LaraOperationError),
    /// Reverse store operation failed.
    Reverse(LaraOperationError),
    /// Forward deferred graph operation failed.
    ForwardDeferred(DeferredError),
    /// Reverse deferred graph operation failed.
    ReverseDeferred(DeferredError),
    /// Unified bidirectional maintenance deque grow or enqueue failed.
    MaintenanceQueue(QueueGrowFailed),
    /// Unified bidirectional maintenance deque could not be reopened.
    MaintenanceQueueInit(QueueInitError),
    /// Dirty-work-item roaring bitmap reopen failed.
    MaintenanceDirtyInit(RoaringInitError),
    /// Dirty-work-item roaring bitmap operation failed (`new`, mutations, checkpoints).
    MaintenanceDirtyBitmap(BitmapError),
    /// Forward graph initialization failed.
    ForwardInit(InitError),
    /// Reverse graph initialization failed.
    ReverseInit(InitError),
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
    /// A requested vertex has been logically deleted.
    VertexDeleted {
        /// Deleted vertex id.
        vid: VertexId,
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
    /// The supplied deferred-maintenance configuration is invalid.
    InvalidConfig(crate::lara::maintenance::DeferredConfigError),
}

impl fmt::Display for DeferredBidirectionalLaraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forward(e) => write!(f, "forward store: {e}"),
            Self::Reverse(e) => write!(f, "reverse store: {e}"),
            Self::ForwardDeferred(e) => write!(f, "forward deferred operation failed: {e}"),
            Self::ReverseDeferred(e) => write!(f, "reverse deferred operation failed: {e}"),
            Self::MaintenanceQueue(e) => {
                write!(f, "bidirectional maintenance queue failed: {e}")
            }
            Self::MaintenanceQueueInit(e) => {
                write!(f, "bidirectional maintenance queue init failed: {e}")
            }
            Self::MaintenanceDirtyInit(e) => {
                write!(f, "bidirectional maintenance dirty bitmap init failed: {e}")
            }
            Self::MaintenanceDirtyBitmap(e) => {
                write!(f, "bidirectional maintenance dirty bitmap failed: {e}")
            }
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
            Self::VertexDeleted { vid } => write!(f, "vertex {vid} is deleted"),
            Self::NeighborMismatch { expected, actual } => write!(
                f,
                "edge neighbor_vid {actual} does not match dst {expected}"
            ),
            Self::UndirectedEdgeInDirectedInsert => write!(
                f,
                "directed insert: edge is marked undirected; use insert_undirected_deferred"
            ),
            Self::InvalidConfig(e) => write!(f, "invalid deferred config: {e}"),
        }
    }
}

impl std::error::Error for DeferredBidirectionalLaraError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ForwardDeferred(e) | Self::ReverseDeferred(e) => Some(e),
            Self::MaintenanceQueue(e) => Some(e),
            Self::MaintenanceQueueInit(e) => Some(e),
            Self::MaintenanceDirtyInit(e) => Some(e),
            Self::MaintenanceDirtyBitmap(e) => Some(e),
            Self::ForwardInit(e) | Self::ReverseInit(e) => Some(e),
            Self::ForwardGrow(e) | Self::ReverseGrow(e) => Some(e),
            Self::Forward(e) | Self::Reverse(e) => Some(e),
            Self::InvalidConfig(e) => Some(e),
            _ => None,
        }
    }
}

/// Bidirectional LARA graph whose two orientations use deferred maintenance.
pub struct DeferredBidirectionalLaraGraph<E, V, M>
where
    E: CsrEdge,
    V: CsrVertex,
    M: Memory,
{
    forward: LaraGraph<E, V, M>,
    reverse: LaraGraph<E, V, M>,
    maintenance: BidirectionalMaintenanceQueue<M>,
    config: DeferredConfig,
}

/// Convenience alias for [`DeferredBidirectionalLaraGraph`].
pub type DeferredBidirectionalLara<E, V, M> = DeferredBidirectionalLaraGraph<E, V, M>;

impl<E, V, M> DeferredBidirectionalLaraGraph<E, V, M>
where
    E: CsrEdge,
    V: CsrVertex,
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
        reverse_vertices: M,
        reverse_counts: M,
        reverse_edges: M,
        reverse_log: M,
        reverse_span_meta: M,
        reverse_free_spans: M,
        reverse_free_span_by_start: M,
        maintenance_queue: M,
        dirty_work_items: M,
        elem_capacity: u64,
        segment_size: u32,
        initial_vertex_edge_slots: u32,
    ) -> Result<Self, DeferredBidirectionalLaraError> {
        Self::new_with_config(
            forward_vertices,
            forward_counts,
            forward_edges,
            forward_log,
            forward_span_meta,
            forward_free_spans,
            forward_free_span_by_start,
            reverse_vertices,
            reverse_counts,
            reverse_edges,
            reverse_log,
            reverse_span_meta,
            reverse_free_spans,
            reverse_free_span_by_start,
            maintenance_queue,
            dirty_work_items,
            elem_capacity,
            segment_size,
            initial_vertex_edge_slots,
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
        reverse_vertices: M,
        reverse_counts: M,
        reverse_edges: M,
        reverse_log: M,
        reverse_span_meta: M,
        reverse_free_spans: M,
        reverse_free_span_by_start: M,
        maintenance_queue: M,
        dirty_work_items: M,
        elem_capacity: u64,
        segment_size: u32,
        initial_vertex_edge_slots: u32,
        config: DeferredConfig,
    ) -> Result<Self, DeferredBidirectionalLaraError> {
        let config = config
            .validate()
            .map_err(DeferredBidirectionalLaraError::InvalidConfig)?;
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
        )
        .map_err(DeferredBidirectionalLaraError::ForwardGrow)?;
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
        )
        .map_err(DeferredBidirectionalLaraError::ReverseGrow)?;
        let maintenance = BidirectionalMaintenanceQueue::new(maintenance_queue, dirty_work_items)?;
        Ok(Self {
            forward,
            reverse,
            maintenance,
            config,
        })
    }

    /// Opens forward and reverse deferred LARA stores, creating them when empty.
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
        maintenance_queue: M,
        dirty_work_items: M,
        elem_capacity: u64,
        segment_size: u32,
        initial_vertex_edge_slots: u32,
    ) -> Result<Self, DeferredBidirectionalLaraError> {
        Self::init_with_config(
            forward_vertices,
            forward_counts,
            forward_edges,
            forward_log,
            forward_span_meta,
            forward_free_spans,
            forward_free_span_by_start,
            reverse_vertices,
            reverse_counts,
            reverse_edges,
            reverse_log,
            reverse_span_meta,
            reverse_free_spans,
            reverse_free_span_by_start,
            maintenance_queue,
            dirty_work_items,
            elem_capacity,
            segment_size,
            initial_vertex_edge_slots,
            DeferredConfig::default(),
        )
    }

    /// Opens forward and reverse stores with custom maintenance thresholds, creating them when empty.
    #[allow(clippy::too_many_arguments)]
    pub fn init_with_config(
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
        maintenance_queue: M,
        dirty_work_items: M,
        elem_capacity: u64,
        segment_size: u32,
        initial_vertex_edge_slots: u32,
        config: DeferredConfig,
    ) -> Result<Self, DeferredBidirectionalLaraError> {
        let config = config
            .validate()
            .map_err(DeferredBidirectionalLaraError::InvalidConfig)?;
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
        .map_err(DeferredBidirectionalLaraError::ForwardInit)?;
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
        .map_err(DeferredBidirectionalLaraError::ReverseInit)?;
        let maintenance = BidirectionalMaintenanceQueue::init(maintenance_queue, dirty_work_items)?;
        let graph = Self {
            forward,
            reverse,
            maintenance,
            config,
        };
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
    pub fn into_memories(self) -> (M, M, M, M, M, M, M, M, M, M, M, M, M, M, M, M) {
        let (fv, fc, fe, fl, fs, ff, ffs) = self.forward.into_memories();
        let (rv, rc, re, rl, rs, rf, rfs) = self.reverse.into_memories();
        let (mq, md) = self.maintenance.into_memories();
        (
            fv, fc, fe, fl, fs, ff, ffs, rv, rc, re, rl, rs, rf, rfs, mq, md,
        )
    }

    /// Returns the number of vertices in both orientations.
    pub fn vertex_count(&self) -> VertexCount {
        VertexCount(self.forward.vertices().len())
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

    /// Copies the vertex row from the forward store.
    ///
    /// Forward and reverse vertex tables stay aligned for all supported mutation
    /// paths; callers must update both together via [`Self::set_vertex_row`].
    pub fn vertex_row(&self, vid: VertexId) -> Result<V, DeferredBidirectionalLaraError> {
        self.ensure_vertex_in_range(vid)?;
        Ok(self.forward.vertices().get(vid))
    }

    /// Overwrites the vertex payload in **both** forward and reverse stores.
    ///
    /// This keeps the invariant established by [`Self::push_vertex`].
    pub fn set_vertex_row(
        &self,
        vid: VertexId,
        row: &V,
    ) -> Result<(), DeferredBidirectionalLaraError> {
        self.ensure_vertex_in_range(vid)?;
        self.forward.vertices().set(vid, row);
        self.reverse.vertices().set(vid, row);
        Ok(())
    }

    /// Returns `true` if `vid` has any incident edge (forward out-adjacency or reverse out-adjacency).
    ///
    /// Equivalent to treating [`Self::collect_out_edges_slot_order`] and
    /// [`Self::collect_in_edges_slot_order`] as non-empty OR, without allocating edge vectors.
    pub fn has_incident_edges(
        &self,
        vid: VertexId,
    ) -> Result<bool, DeferredBidirectionalLaraError> {
        self.ensure_vertex(vid)?;
        if self
            .forward
            .has_out_edges(vid)
            .map_err(DeferredBidirectionalLaraError::Forward)?
        {
            return Ok(true);
        }
        self.reverse
            .has_out_edges(vid)
            .map_err(DeferredBidirectionalLaraError::Reverse)
    }

    /// Collects outgoing edges from the forward store in slab slot order.
    pub fn collect_out_edges_slot_order(
        &self,
        src: VertexId,
    ) -> Result<Vec<E>, DeferredBidirectionalLaraError> {
        self.ensure_vertex(src)?;
        self.forward
            .collect_out_edges_slot_order(src)
            .map_err(DeferredBidirectionalLaraError::Forward)
    }

    /// Collects incoming edges from the reverse store in slab slot order.
    pub fn collect_in_edges_slot_order(
        &self,
        dst: VertexId,
    ) -> Result<Vec<E>, DeferredBidirectionalLaraError> {
        self.ensure_vertex(dst)?;
        self.reverse
            .collect_out_edges_slot_order(dst)
            .map_err(DeferredBidirectionalLaraError::Reverse)
    }

    /// Iterates outgoing edges from the forward store in standard scan order.
    pub fn iter_out_edges(
        &self,
        src: VertexId,
    ) -> Result<OutEdgesIter<'_, E, M>, DeferredBidirectionalLaraError> {
        self.ensure_vertex(src)?;
        self.forward
            .iter_out_edges(src)
            .map_err(DeferredBidirectionalLaraError::Forward)
    }

    /// Iterates incoming edges from the reverse store in standard scan order.
    pub fn iter_in_edges(
        &self,
        dst: VertexId,
    ) -> Result<OutEdgesIter<'_, E, M>, DeferredBidirectionalLaraError> {
        self.ensure_vertex(dst)?;
        self.reverse
            .iter_out_edges(dst)
            .map_err(DeferredBidirectionalLaraError::Reverse)
    }

    /// Logically deletes a vertex and queues incremental incident-edge cleanup.
    pub fn delete_vertex_deferred(
        &self,
        vid: VertexId,
    ) -> Result<bool, DeferredBidirectionalLaraError> {
        self.ensure_vertex_in_range(vid)?;
        if self
            .forward
            .vertex_is_deleted(vid)
            .map_err(DeferredBidirectionalLaraError::Forward)?
        {
            return Ok(false);
        }
        self.forward
            .set_vertex_deleted(vid, true)
            .map_err(DeferredBidirectionalLaraError::Forward)?;
        self.reverse
            .set_vertex_deleted(vid, true)
            .map_err(DeferredBidirectionalLaraError::Reverse)?;
        self.maintenance
            .mark_urgent(MaintenanceWorkItem::DeleteVertex {
                vid,
                phase: DeletePhase::RemoveOutgoing,
                cursor: 0,
                removed_edges: 0,
            })?;
        Ok(true)
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

        self.insert_oriented_deferred(Orientation::Forward, src, edge)?;
        self.insert_oriented_deferred(Orientation::Reverse, dst, edge.with_neighbor_vid(src))?;
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
            .remove_edge_matching(src, |edge| edge.neighbor_vid() == dst && matches(edge))
            .map_err(DeferredBidirectionalLaraError::Forward)?;
        let Some(edge) = removed_forward else {
            return Ok(None);
        };
        let removed_reverse = self
            .reverse
            .remove_edge(dst, edge.with_neighbor_vid(src))
            .map_err(DeferredBidirectionalLaraError::Reverse)?;
        if !removed_reverse {
            return Err(DeferredBidirectionalLaraError::Reverse(
                LaraOperationError::DirectedRemoveOrientationMismatch,
            ));
        }
        Ok(Some(edge))
    }

    fn insert_oriented_deferred(
        &self,
        orientation: Orientation,
        src: VertexId,
        edge: E,
    ) -> Result<(), DeferredBidirectionalLaraError> {
        let graph = match orientation {
            Orientation::Forward => &self.forward,
            Orientation::Reverse => &self.reverse,
        };
        let outcome = graph
            .insert_edge_raw(src, edge)
            .map_err(|e| match orientation {
                Orientation::Forward => DeferredBidirectionalLaraError::Forward(e),
                Orientation::Reverse => DeferredBidirectionalLaraError::Reverse(e),
            })?;
        let priority = graph.deferred_mark_priority(
            outcome.segment,
            outcome.inserted_into_log,
            self.config.leaf_dirty_density,
            self.config.log_urgent_ratio,
        );
        self.enqueue_rebalance_priority(orientation, priority)
    }

    fn enqueue_rebalance_priority(
        &self,
        orientation: Orientation,
        priority: MarkPriority,
    ) -> Result<(), DeferredBidirectionalLaraError> {
        match priority {
            MarkPriority::Clean => {}
            MarkPriority::Dirty(segment) => {
                self.maintenance
                    .mark_dirty(MaintenanceWorkItem::Rebalance {
                        orientation,
                        segment,
                    })?;
            }
            MarkPriority::Urgent(segment) => {
                self.maintenance
                    .mark_urgent(MaintenanceWorkItem::Rebalance {
                        orientation,
                        segment,
                    })?;
            }
        }
        Ok(())
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
            self.insert_oriented_deferred(Orientation::Forward, u, loop_edge)?;
            self.insert_oriented_deferred(Orientation::Reverse, u, loop_edge)?;
            return Ok(());
        }

        self.insert_oriented_deferred(Orientation::Forward, u, edge.with_neighbor_vid(v))?;
        self.insert_oriented_deferred(Orientation::Forward, v, edge.with_neighbor_vid(u))?;
        self.insert_oriented_deferred(Orientation::Reverse, v, edge.with_neighbor_vid(u))?;
        self.insert_oriented_deferred(Orientation::Reverse, u, edge.with_neighbor_vid(v))?;
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
                    LaraOperationError::UndirectedRemoveOrientationMismatch,
                ));
            }
        }
        Ok(Some(edge))
    }

    /// Returns the unified bidirectional maintenance queue length.
    pub fn maintenance_queue_len(&self) -> u64 {
        self.maintenance.len()
    }

    /// Runs budgeted maintenance across both orientations and vertex-delete jobs.
    pub fn maintenance(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<BidirectionalMaintenanceReport, DeferredBidirectionalLaraError> {
        self.maintenance_with_delete_observer(budget, &mut NoopDeleteEdgeObserver)
    }

    /// Runs budgeted maintenance and notifies `observer` as vertex-delete work removes edges.
    pub fn maintenance_with_delete_observer<O>(
        &self,
        budget: MaintenanceBudget,
        observer: &mut O,
    ) -> Result<BidirectionalMaintenanceReport, DeferredBidirectionalLaraError>
    where
        O: DeleteEdgeObserver<E>,
    {
        let mut report = BidirectionalMaintenanceReport::default();
        let baseline = current_instruction_counter();
        let mut checkpoint_tick = 0u32;

        loop {
            if budget
                .max_work_items
                .is_some_and(|max| report.work.processed_work_items >= max)
                || budget
                    .max_segments
                    .is_some_and(|max| report.work.processed_segments >= max)
                || budget
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

            let Some(step) = self.maintenance_step_with_delete_observer(observer)? else {
                break;
            };
            add_step_report(&mut report.work, step);
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

    fn maintenance_step_with_delete_observer<O>(
        &self,
        observer: &mut O,
    ) -> Result<Option<MaintenanceWorkReport>, DeferredBidirectionalLaraError>
    where
        O: DeleteEdgeObserver<E>,
    {
        let Some(item) = self.maintenance.pop_next()? else {
            return Ok(None);
        };
        let (step, next) = self.process_work_item_with_delete_observer(item, observer)?;
        if let Some(next) = next {
            self.maintenance.requeue_front(next)?;
        } else {
            self.maintenance.complete(item)?;
        }
        Ok(Some(step))
    }

    fn process_work_item_with_delete_observer<O>(
        &self,
        item: MaintenanceWorkItem,
        observer: &mut O,
    ) -> Result<(MaintenanceWorkReport, Option<MaintenanceWorkItem>), DeferredBidirectionalLaraError>
    where
        O: DeleteEdgeObserver<E>,
    {
        match item {
            MaintenanceWorkItem::Rebalance {
                orientation,
                segment,
            } => {
                let graph = match orientation {
                    Orientation::Forward => &self.forward,
                    Orientation::Reverse => &self.reverse,
                };
                let mut report = MaintenanceWorkReport {
                    processed_work_items: 1,
                    processed_segments: 1,
                    ..MaintenanceWorkReport::default()
                };
                let before_capacity = graph.edges().header().elem_capacity;
                if graph.rebalance_maintenance_segment(segment) {
                    graph
                        .rebalance_dirty_segment(segment)
                        .map_err(|e| match orientation {
                            Orientation::Forward => DeferredBidirectionalLaraError::ForwardGrow(e),
                            Orientation::Reverse => DeferredBidirectionalLaraError::ReverseGrow(e),
                        })?;
                    report.rebalanced_segments = 1;
                    report.resized = graph.edges().header().elem_capacity != before_capacity;
                }
                report.remaining_queue_len = self.maintenance.len();
                Ok((report, None))
            }
            MaintenanceWorkItem::DeleteVertex {
                vid,
                phase,
                cursor,
                removed_edges,
            } => self.process_delete_vertex(vid, phase, cursor, removed_edges, observer),
        }
    }

    fn process_delete_vertex<O>(
        &self,
        vid: VertexId,
        phase: DeletePhase,
        cursor: u32,
        removed_edges: u64,
        observer: &mut O,
    ) -> Result<(MaintenanceWorkReport, Option<MaintenanceWorkItem>), DeferredBidirectionalLaraError>
    where
        O: DeleteEdgeObserver<E>,
    {
        let mut report = MaintenanceWorkReport {
            processed_work_items: 1,
            ..MaintenanceWorkReport::default()
        };
        let next = match phase {
            DeletePhase::RemoveOutgoing => {
                if let Some(edge) = self
                    .forward
                    .row_edge_at_after_rebalance(vid, cursor)
                    .map_err(DeferredBidirectionalLaraError::Forward)?
                {
                    let dst = edge.neighbor_vid();
                    let _ = self
                        .reverse
                        .remove_edge_matching_idempotent(dst, |candidate| {
                            candidate.neighbor_vid() == vid
                        })
                        .map_err(DeferredBidirectionalLaraError::Reverse)?;
                    observer.on_delete_outgoing_edge(vid, edge);
                    report.processed_delete_edge_steps = 1;
                    Some(MaintenanceWorkItem::DeleteVertex {
                        vid,
                        phase,
                        cursor: cursor.saturating_add(1),
                        removed_edges: removed_edges.saturating_add(1),
                    })
                } else {
                    Some(MaintenanceWorkItem::DeleteVertex {
                        vid,
                        phase: DeletePhase::ClearForwardRow,
                        cursor: 0,
                        removed_edges,
                    })
                }
            }
            DeletePhase::ClearForwardRow => {
                self.forward
                    .clear_row_after_rebalance(vid)
                    .map_err(DeferredBidirectionalLaraError::Forward)?;
                Some(MaintenanceWorkItem::DeleteVertex {
                    vid,
                    phase: DeletePhase::RemoveIncoming,
                    cursor: 0,
                    removed_edges,
                })
            }
            DeletePhase::RemoveIncoming => {
                if let Some(edge) = self
                    .reverse
                    .row_edge_at_after_rebalance(vid, cursor)
                    .map_err(DeferredBidirectionalLaraError::Reverse)?
                {
                    let src = edge.neighbor_vid();
                    let _ = self
                        .forward
                        .remove_edge_matching_idempotent(src, |candidate| {
                            candidate.neighbor_vid() == vid
                        })
                        .map_err(DeferredBidirectionalLaraError::Forward)?;
                    observer.on_delete_incoming_edge(vid, edge);
                    report.processed_delete_edge_steps = 1;
                    Some(MaintenanceWorkItem::DeleteVertex {
                        vid,
                        phase,
                        cursor: cursor.saturating_add(1),
                        removed_edges: removed_edges.saturating_add(1),
                    })
                } else {
                    Some(MaintenanceWorkItem::DeleteVertex {
                        vid,
                        phase: DeletePhase::ClearReverseRow,
                        cursor: 0,
                        removed_edges,
                    })
                }
            }
            DeletePhase::ClearReverseRow => {
                self.reverse
                    .clear_row_after_rebalance(vid)
                    .map_err(DeferredBidirectionalLaraError::Reverse)?;
                report.completed_vertex_deletes = 1;
                None
            }
        };
        report.remaining_queue_len = self.maintenance.len();
        Ok((report, next))
    }

    fn ensure_matching_vertex_counts(&self) -> Result<(), DeferredBidirectionalLaraError> {
        let forward = VertexCount(self.forward.vertices().len());
        let reverse = VertexCount(self.reverse.vertices().len());
        if forward != reverse {
            return Err(DeferredBidirectionalLaraError::VertexCountMismatch { forward, reverse });
        }
        Ok(())
    }

    fn ensure_vertex(&self, vid: VertexId) -> Result<(), DeferredBidirectionalLaraError> {
        self.ensure_vertex_in_range(vid)?;
        if self
            .forward
            .vertex_is_deleted(vid)
            .map_err(DeferredBidirectionalLaraError::Forward)?
        {
            return Err(DeferredBidirectionalLaraError::VertexDeleted { vid });
        }
        Ok(())
    }

    fn ensure_vertex_in_range(&self, vid: VertexId) -> Result<(), DeferredBidirectionalLaraError> {
        let len = self.forward.vertices().len();
        if u32::from(vid) >= len {
            return Err(DeferredBidirectionalLaraError::VertexOutOfRange {
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
        test_support::{
            TestEdge, UndirectedTestEdge, deferred_bidirectional_test_graph, vector_memory,
        },
    };

    #[test]
    fn deferred_directed_insert_updates_forward_and_reverse() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(8, 2, &[0, 2, 4]);

        graph
            .insert_directed_deferred(VertexId::from(0), VertexId::from(2), TestEdge(2))
            .unwrap();

        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(2)]
        );
        assert_eq!(
            graph
                .iter_out_edges(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(2)]
        );
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(2))
                .unwrap(),
            Vec::new()
        );
        assert_eq!(
            graph
                .collect_in_edges_slot_order(VertexId::from(2))
                .unwrap(),
            vec![TestEdge(0)]
        );
        assert_eq!(
            graph
                .iter_in_edges(VertexId::from(2))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(0)]
        );
        assert_eq!(
            graph
                .collect_in_edges_slot_order(VertexId::from(0))
                .unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn has_incident_edges_matches_collect_emptiness() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(8, 2, &[0, 2, 4]);
        graph
            .insert_directed_deferred(VertexId::from(0), VertexId::from(2), TestEdge(2))
            .unwrap();

        let expect = |vid: u32| {
            let vid = VertexId::from(vid);
            let legacy = !graph.collect_out_edges_slot_order(vid).unwrap().is_empty()
                || !graph.collect_in_edges_slot_order(vid).unwrap().is_empty();
            assert_eq!(graph.has_incident_edges(vid).unwrap(), legacy);
        };
        expect(0);
        expect(2);
        expect(1);
    }

    #[test]
    fn deferred_bidirectional_init_creates_empty_graph_when_memory_is_empty() {
        let graph = DeferredBidirectionalLaraGraph::<TestEdge, Vertex, _>::init(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
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
            0,
        )
        .unwrap();

        assert_eq!(graph.vertex_count(), VertexCount(0));
        assert_eq!(graph.forward().edges().header().elem_capacity, 8);
        assert_eq!(graph.reverse().edges().header().segment_size, 2);
        assert_eq!(graph.maintenance_queue_len(), 0);
    }

    #[test]
    fn deferred_directed_insert_rejects_neighbor_mismatch_before_writes() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(8, 2, &[0, 2]);

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
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            Vec::new()
        );
        assert_eq!(
            graph
                .collect_in_edges_slot_order(VertexId::from(1))
                .unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn deferred_directed_insert_rejects_undirected_edge() {
        let graph = deferred_bidirectional_test_graph::<UndirectedTestEdge>(8, 2, &[0, 2]);
        let edge = UndirectedTestEdge::new(1).with_undirected(true);

        let err = graph
            .insert_directed_deferred(VertexId::from(0), VertexId::from(1), edge)
            .unwrap_err();

        assert!(matches!(
            err,
            DeferredBidirectionalLaraError::UndirectedEdgeInDirectedInsert
        ));
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            Vec::new()
        );
        assert_eq!(
            graph
                .collect_in_edges_slot_order(VertexId::from(1))
                .unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn deferred_undirected_insert_materializes_symmetric_adjacency() {
        let graph = deferred_bidirectional_test_graph::<UndirectedTestEdge>(8, 2, &[0, 2, 4]);

        graph
            .insert_undirected_deferred(
                VertexId::from(0),
                VertexId::from(2),
                UndirectedTestEdge::new(2),
            )
            .unwrap();

        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 2,
                undirected: true
            }]
        );
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(2))
                .unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 0,
                undirected: true
            }]
        );
        assert_eq!(
            graph
                .collect_in_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 2,
                undirected: true
            }]
        );
        assert_eq!(
            graph
                .collect_in_edges_slot_order(VertexId::from(2))
                .unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 0,
                undirected: true
            }]
        );
    }

    #[test]
    fn deferred_undirected_self_loop_stores_one_loop_per_orientation() {
        let graph = deferred_bidirectional_test_graph::<UndirectedTestEdge>(8, 2, &[0, 2]);

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
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(1))
                .unwrap(),
            vec![loop_edge]
        );
        assert_eq!(
            graph
                .collect_in_edges_slot_order(VertexId::from(1))
                .unwrap(),
            vec![loop_edge]
        );
    }

    #[test]
    fn deferred_bidirectional_reopen_preserves_unified_queue() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(8, 2, &[0, 2, 4]);
        for _ in 0..3 {
            graph
                .insert_directed_deferred(VertexId::from(0), VertexId::from(2), TestEdge(2))
                .unwrap();
        }
        assert_eq!(graph.maintenance_queue_len(), 2);

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
            16,
            2,
            0,
        )
        .unwrap();

        assert_eq!(
            reopened
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(2), TestEdge(2), TestEdge(2)]
        );
        assert_eq!(reopened.maintenance_queue_len(), 2);
    }

    #[test]
    fn deferred_bidirectional_unified_maintenance_respects_segment_cap() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(8, 2, &[0, 2, 4]);
        for _ in 0..3 {
            graph
                .insert_directed_deferred(VertexId::from(0), VertexId::from(2), TestEdge(2))
                .unwrap();
        }

        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_delete_edge_steps: None,
            })
            .unwrap();

        assert_eq!(report.work.processed_segments, 1);
        assert_eq!(graph.maintenance_queue_len(), 1);
    }

    #[test]
    fn deferred_vertex_delete_is_incremental_and_removes_incident_edges() {
        let graph = deferred_bidirectional_test_graph::<TestEdge>(16, 4, &[0, 2, 4, 6]);
        graph
            .insert_directed_deferred(VertexId::from(0), VertexId::from(1), TestEdge(1))
            .unwrap();
        graph
            .insert_directed_deferred(VertexId::from(2), VertexId::from(1), TestEdge(1))
            .unwrap();
        graph
            .insert_directed_deferred(VertexId::from(1), VertexId::from(3), TestEdge(3))
            .unwrap();

        assert!(graph.delete_vertex_deferred(VertexId::from(1)).unwrap());
        assert!(matches!(
            graph.collect_out_edges_slot_order(VertexId::from(1)),
            Err(DeferredBidirectionalLaraError::VertexDeleted { .. })
        ));

        while graph.maintenance_queue_len() > 0 {
            graph
                .maintenance(MaintenanceBudget {
                    max_instructions: 0,
                    max_segments: None,
                    reserve_instructions: 0,
                    checkpoint_every: 1,
                    max_work_items: Some(1),
                    max_delete_edge_steps: Some(1),
                })
                .unwrap();
        }

        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            Vec::new()
        );
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(2))
                .unwrap(),
            Vec::new()
        );
        assert_eq!(
            graph
                .collect_in_edges_slot_order(VertexId::from(3))
                .unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn vertex_delete_observer_sees_removed_edges() {
        #[derive(Default)]
        struct Observer {
            outgoing: Vec<(VertexId, TestEdge)>,
            incoming: Vec<(VertexId, TestEdge)>,
        }

        impl DeleteEdgeObserver<TestEdge> for Observer {
            fn on_delete_outgoing_edge(&mut self, source: VertexId, edge: TestEdge) {
                self.outgoing.push((source, edge));
            }

            fn on_delete_incoming_edge(&mut self, destination: VertexId, edge: TestEdge) {
                self.incoming.push((destination, edge));
            }
        }

        let graph = deferred_bidirectional_test_graph::<TestEdge>(16, 4, &[0, 2, 4, 6]);
        graph
            .insert_directed_deferred(VertexId::from(0), VertexId::from(1), TestEdge(1))
            .unwrap();
        graph
            .insert_directed_deferred(VertexId::from(2), VertexId::from(1), TestEdge(1))
            .unwrap();
        graph
            .insert_directed_deferred(VertexId::from(1), VertexId::from(3), TestEdge(3))
            .unwrap();
        assert!(graph.delete_vertex_deferred(VertexId::from(1)).unwrap());

        let mut observer = Observer::default();
        while graph.maintenance_queue_len() > 0 {
            graph
                .maintenance_with_delete_observer(
                    MaintenanceBudget {
                        max_instructions: 0,
                        max_segments: None,
                        reserve_instructions: 0,
                        checkpoint_every: 1,
                        max_work_items: Some(1),
                        max_delete_edge_steps: Some(1),
                    },
                    &mut observer,
                )
                .unwrap();
        }

        assert_eq!(observer.outgoing, vec![(VertexId::from(1), TestEdge(3))]);
        observer.incoming.sort_by_key(|(_, edge)| edge.0);
        assert_eq!(
            observer.incoming,
            vec![
                (VertexId::from(1), TestEdge(0)),
                (VertexId::from(1), TestEdge(2))
            ]
        );
    }

    #[test]
    fn deferred_bidirectional_init_rejects_vertex_count_mismatch() {
        let forward = LaraGraph::<TestEdge, Vertex, _>::new(
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
        let reverse = LaraGraph::<TestEdge, Vertex, _>::new(
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
                log_head: -1,
                deleted: false,
            })
            .unwrap();

        let (fv, fc, fe, fl, fs, ff, ffs) = forward.into_memories();
        let (rv, rc, re, rl, rs, rf, rfs) = reverse.into_memories();
        let err = match DeferredBidirectionalLaraGraph::<TestEdge, Vertex, _>::init(
            fv,
            fc,
            fe,
            fl,
            fs,
            ff,
            ffs,
            rv,
            rc,
            re,
            rl,
            rs,
            rf,
            rfs,
            vector_memory(),
            vector_memory(),
            8,
            2,
            0,
        ) {
            Ok(_) => panic!("vertex count mismatch was accepted"),
            Err(err) => err,
        };

        assert!(matches!(
            err,
            DeferredBidirectionalLaraError::VertexCountMismatch { .. }
        ));
    }

    /// Regression for tombstoned rows that still hold slab/log material during
    /// incremental `DeleteVertex`: leaf rebalance must enumerate those edges.
    /// Uses the same `segment_size` / `initial_vertex_edge_slots` defaults as
    /// `gleaph-graph` stable init (`32` / `0`).
    #[test]
    fn deferred_vertex_delete_wide_segment_rebalance_drains_without_panic() {
        let graph = crate::DeferredBidirectionalLaraGraph::<TestEdge, Vertex, _>::new_with_config(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            0,
            32,
            0,
            crate::DeferredConfig {
                leaf_dirty_density: 0.0,
                log_urgent_ratio: 0.80,
            },
        )
        .unwrap();

        graph.push_vertex(Vertex::default()).unwrap();
        graph.push_vertex(Vertex::default()).unwrap();

        graph
            .insert_directed_deferred(VertexId::from(0), VertexId::from(1), TestEdge(1))
            .unwrap();

        assert!(graph.delete_vertex_deferred(VertexId::from(0)).unwrap());

        let budget = crate::MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        while graph.maintenance_queue_len() > 0 {
            graph.maintenance(budget).unwrap();
        }

        assert!(
            graph
                .collect_out_edges_slot_order(VertexId::from(1))
                .unwrap()
                .is_empty()
        );
    }
}
