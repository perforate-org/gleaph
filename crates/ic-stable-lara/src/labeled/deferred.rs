//! Deferred-maintenance wrapper for the labeled LARA graph.

use crate::{
    VertexId,
    labeled::graph::{InitError, LabeledLaraGraph, LabeledOperationError},
    lara::maintenance::{MaintenanceBudget, MaintenanceWorkReport},
    traits::CsrEdge,
};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use ic_stable_vec_deque::{
    GrowFailed as QueueGrowFailed, InitError as QueueInitError, StableVecDeque,
};
use std::{borrow::Cow, fmt};

/// One deferred maintenance item for a labeled graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceWorkItem {
    /// Rebalance one vertex bucket range.
    RebalanceVertexBuckets {
        /// Vertex whose bucket range should be compacted.
        vid: VertexId,
    },
    /// Rebalance one label bucket edge range.
    RebalanceLabelBucket {
        /// Vertex owning the bucket.
        vid: VertexId,
        /// Bucket index inside the vertex bucket range.
        bucket_index: u32,
    },
}

impl Storable for MaintenanceWorkItem {
    const BOUND: Bound = Bound::Bounded {
        max_size: 12,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut bytes = [0u8; 12];
        match *self {
            Self::RebalanceVertexBuckets { vid } => {
                bytes[0] = 0;
                bytes[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
            }
            Self::RebalanceLabelBucket { vid, bucket_index } => {
                bytes[0] = 1;
                bytes[4..8].copy_from_slice(&u32::from(vid).to_le_bytes());
                bytes[8..12].copy_from_slice(&bucket_index.to_le_bytes());
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
            1 => Self::RebalanceLabelBucket {
                vid,
                bucket_index: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            },
            _ => Self::RebalanceVertexBuckets { vid },
        }
    }
}

/// Errors returned by deferred labeled graph operations.
#[derive(Debug)]
pub enum DeferredError {
    Inner(LabeledOperationError),
    Queue(QueueInitError),
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

impl std::error::Error for DeferredError {}

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
    pub fn new(inner: LabeledLaraGraph<E, M>, queue_memory: M) -> Result<Self, DeferredError> {
        Ok(Self {
            inner,
            queue: StableVecDeque::init(queue_memory).map_err(DeferredError::Queue)?,
        })
    }

    pub fn init(
        vertices: M,
        buckets: M,
        edges: M,
        queue_memory: M,
        elem_capacity: u64,
        default_label: crate::labeled::record::LabelId,
    ) -> Result<Self, InitError> {
        let inner = LabeledLaraGraph::init(vertices, buckets, edges, elem_capacity, default_label)?;
        let queue = StableVecDeque::init(queue_memory)
            .map_err(|_| InitError::Vertices(crate::labeled::row_store::InitError::OutOfMemory))?;
        Ok(Self { inner, queue })
    }

    pub fn inner(&self) -> &LabeledLaraGraph<E, M> {
        &self.inner
    }

    pub fn mark_rebalance_vertex(&self, vid: VertexId) -> Result<(), DeferredError> {
        self.queue
            .push_back(&MaintenanceWorkItem::RebalanceVertexBuckets { vid })
            .map_err(DeferredError::QueueGrow)?;
        Ok(())
    }

    pub fn maintenance(&self, budget: MaintenanceBudget) -> MaintenanceWorkReport {
        let mut report = MaintenanceWorkReport::default();
        let max_items = budget.max_work_items.unwrap_or(u32::MAX);
        while report.processed_work_items < max_items {
            let Some(item) = self.queue.pop_front() else {
                break;
            };
            report.processed_work_items = report.processed_work_items.saturating_add(1);
            match item {
                MaintenanceWorkItem::RebalanceVertexBuckets { .. }
                | MaintenanceWorkItem::RebalanceLabelBucket { .. } => {
                    report.rebalanced_segments = report.rebalanced_segments.saturating_add(1);
                }
            }
        }
        report.remaining_queue_len = self.queue.len();
        report
    }
}
