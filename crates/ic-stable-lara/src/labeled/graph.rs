//! Single-orientation labeled LARA graph orchestration.
//!
//! [`LabeledLaraGraph`] mirrors [`crate::LaraGraph`]: it owns the vertex column
//! plus the storage layers required to mutate one CSR orientation. The extra
//! bucket layer is kept small and relocatable. Normal labeled edge bytes live in
//! the regular [`EdgeStore`] slab/free-span store and participate in the same
//! PMA segment [`crate::lara::edge::counts::SegmentEdgeCounts`] accounting as
//! core LARA: each [`LabeledVertex`]'s [`LabeledVertex::stored_slots`]
//! contributes `total` while live edges contribute `actual`. A **cascade** from
//! per-label edge span grow/shrink propagates through the owning **VertexEdgeSpan**
//! into per-leaf density checks (compaction then optional slack growth).

use crate::{
    VertexId,
    labeled::{
        bucket_label_key::BucketLabelKey,
        bucket_store::LabelBucketStore,
        record::{LabelBucket, LabeledVertex},
    },
    lara::{edge::EdgeStore, edge_payload::EdgePayloadStore, vertex::VertexStore},
    traits::CsrEdge,
};
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::{cell::Cell, marker::PhantomData};

const DEFAULT_SEGMENT_SIZE: u32 = 32;
const BULK_BUCKET_SEARCH_MIN_DEGREE: u32 = 16;
const BUCKET_LOOKUP_CACHE_ENTRIES: usize = 64;

/// Same threshold as core LARA leaf density (`actual/total` on one PMA leaf).
const LEAF_VERTEX_EDGE_SEGMENT_DENSITY: f64 = 1.0;

pub(crate) enum BucketSearch {
    Found { slot: u64, bucket: LabelBucket },
    Missing { insert_index: u32 },
}

#[derive(Clone, Copy)]
struct BucketLookupCache {
    vid: VertexId,
    bucket_key: BucketLabelKey,
    base_slot_start: u64,
    degree: u32,
    slot: u64,
}

mod bucket;
mod bypass;
mod compact;
mod error;
mod init;
mod insert;
mod iter;
mod remove;
#[cfg(test)]
mod test_support;
mod traverse;
mod values;

pub use error::{InitError, LabeledOperationError, OutEdgeOrder};
pub use iter::LabeledOutEdgesIter;
pub use iter::LabeledSpanIter;
pub use iter::{LabeledEdgePayloadBatch, LabeledEdgePayloadBatchScratch};

/// Single-orientation multi-level labeled CSR graph.
pub struct LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    vertices: VertexStore<LabeledVertex, M>,
    buckets: LabelBucketStore<M>,
    edges: EdgeStore<E, M>,
    values: EdgePayloadStore<M>,
    default_label: BucketLabelKey,
    last_bucket_lookup: Cell<Option<BucketLookupCache>>,
    bucket_lookup_cache: [Cell<Option<BucketLookupCache>>; BUCKET_LOOKUP_CACHE_ENTRIES],
    _marker: PhantomData<E>,
}

/// Slot relocation produced while compacting one labeled adjacency row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeSlotMove {
    /// Label row whose local slot changed.
    pub label_id: BucketLabelKey,
    /// Old slot index inside the label row.
    pub old_slot_index: u32,
    /// New slot index inside the label row.
    pub new_slot_index: u32,
}

/// Result of one incremental [`LabeledLaraGraph::compact_vertex_edge_span_one_step`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexEdgeSpanCompactOneStep {
    /// One live edge was relocated inside its label bucket; sidecars should follow `move`.
    EdgeMoved(EdgeSlotMove),
    /// The current label bucket is packed; continue from the next bucket index.
    AdvanceBucket(u32),
    /// Overflow-log buckets required a full span rewrite; `moves` lists slot rewrites.
    OverflowRewrite(Vec<EdgeSlotMove>),
    /// The vertex span is fully compacted.
    Finished,
}
