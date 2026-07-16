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
    lara::{edge::EdgeStore, edge_inline_value::EdgeInlineValueStore, vertex::VertexStore},
    traits::CsrEdge,
};
use ic_stable_structures::Memory;
use std::{cell::Cell, marker::PhantomData};

const DEFAULT_SEGMENT_SIZE: u32 = 16;
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
#[cfg(test)]
pub(crate) use compact::force_next_compact_vertex_edge_span_step_error;
#[cfg(test)]
pub(crate) use values::force_next_payload_compaction_error;
mod error;
mod init;
mod insert;
mod iter;
pub(crate) mod leaf_pin;
mod remove;
#[cfg(test)]
mod test_support;
mod traverse;
mod values;

pub use error::{InitError, LabeledOperationError, OutEdgeOrder};
pub use iter::LabeledOutEdgesIter;
pub use iter::LabeledSpanIter;
pub use iter::{
    HybridOverflowEdgeReplay, LabeledEdgeInlineValueBatch, LabeledEdgeInlineValueBatchScratch,
    LabeledPayloadValueBatch, LabeledPayloadValueBatchScratch,
};

/// Single-orientation multi-level labeled CSR graph.
pub struct LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    vertices: VertexStore<LabeledVertex, M>,
    buckets: LabelBucketStore<M>,
    edges: EdgeStore<E, M>,
    values: EdgeInlineValueStore<M>,
    default_label: BucketLabelKey,
    last_bucket_lookup: Cell<Option<BucketLookupCache>>,
    payload_compaction_deferred: Cell<bool>,
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

/// One removed edge plus the only surviving slot relocation that deletion may produce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeRemoval<E> {
    /// Edge selected by the caller.
    pub removed: E,
    /// Surviving overflow edges whose scan-ordinal slots shifted down by one.
    pub moves: Vec<EdgeSlotMove>,
}

/// Aggregated payload storage accounting for one labeled graph orientation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabeledPayloadStorageStats {
    /// Bytes required by live payload values in bucket-local order.
    pub live_bytes: u64,
    /// Bytes reserved by payload slab spans owned by labeled vertices.
    pub allocated_bytes: u64,
    /// Payload slab backing capacity in bytes.
    pub byte_capacity: u64,
    /// Exclusive end of the append-only occupied payload slab prefix.
    pub slab_occupied_tail: u64,
    /// Bytes available in retired payload free spans.
    pub free_bytes: u64,
    /// Largest retired payload free span.
    pub largest_free_span: u64,
    /// Number of retired payload free spans.
    pub free_span_count: u64,
}

/// Result of a payload-only slab compaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabeledPayloadCompactionResult {
    /// Number of payload slab spans moved.
    pub moved_spans: u32,
    /// Total payload bytes copied into earlier free spans.
    pub moved_bytes: u64,
}

/// Result of one incremental [`LabeledLaraGraph::compact_vertex_edge_span_one_step`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexEdgeSpanCompactOneStep {
    /// One live edge was relocated inside its label bucket; sidecars should follow `move`.
    EdgeMoved(EdgeSlotMove),
    /// The current label bucket is packed; continue from the next bucket index.
    AdvanceBucket(u32),
    /// One overflow suffix was folded; `moves` lists legacy-tombstone slot rewrites. Resume at bucket 0.
    OverflowRewrite(Vec<EdgeSlotMove>),
    /// The vertex span is fully compacted.
    Finished,
}
