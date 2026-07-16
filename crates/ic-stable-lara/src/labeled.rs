//! Multi-level labeled CSR variant of LARA.
//!
//! This module keeps the same scan/update split as [`crate::LaraGraph`], but
//! inserts LabelBuckets between vertices and edge rows:
//!
//! - **Scan contract:** read one [`LabeledVertex`]. If it uses the default-label
//!   bypass, scan the edge row directly. Otherwise read LabelBucket slots
//!   `[base_slot_start, base_slot_start + degree)`, choose the matching
//!   [`LabelBucket`], then scan that bucket's LabelEdgeSpan. Scan code never reads the
//!   LabelBucketStore free-span index. For a LabelEdgeSpan it only needs the bucket
//!   itself plus the next bucket's `edge_start`; for the last bucket, the
//!   successor boundary is the VertexEdgeSpan end.
//! - **Update contract:** LabelBucket inserts and maintenance rewrite the
//!   owning LabelBucketStore VertexSegment, updating each affected
//!   [`LabeledVertex::base_slot_start`] and [`LabeledVertex::degree`]. Edge
//!   inserts under a label append through [`crate::lara::edge::EdgeStore`] using
//!   the same slab vs per-segment overflow log split as core LARA rows; when the
//!   slab window up to the next bucket boundary is full, new edges for that label
//!   go to the segment log (see [`crate::labeled::record::LabelBucket::overflow_log_head`]).
//!   A full segment log triggers a VertexEdgeSpan rewrite that folds overflow back
//!   onto the slab and may widen the [`LabeledVertex::stored_slots`] reservation.
//!   The edge bytes still live in the
//!   regular [`crate::lara::edge::EdgeStore`] slab, so allocation and free-span
//!   reuse stay centralized in the existing LARA implementation.
//!
//! The LabelBucketStore deliberately does **not** have its own overflow log or PMA segment
//! metadata. A physical VertexSegment's length is exactly the number of live
//! LabelBuckets in that 32-vertex segment; old spans are released only after the
//! rewritten vertex rows point at the new committed layout.
//!
//! The edge layer has one additional labeled-specific rule. For a non-default
//! vertex, [`LabeledVertex::stored_slots`] reserves one contiguous
//! physical VertexEdgeSpan for all labels on that vertex. The [`LabelBucket`] rows are
//! kept strictly sorted by [`BucketLabelKey`], and their LabelEdgeSpans are laid out in
//! that same order inside the VertexEdgeSpan:
//!
//! ```text
//! LabeledVertex (normal mode)
//!   base_slot_start     -> LabelBucket descriptor span in LabelBucketStore
//!   degree              -> live LabelBucket rows (≤ MAX_VERTEX_LABEL_BUCKETS = 65536)
//!   bucket_slack_slots  -> extra descriptor slots in metadata (physical span = degree + slack)
//!   stored_slots        -> VertexEdgeSpan width for edge bytes (all labels)
//!
//! EdgeStore slab
//!   LabelEdgeSpan(label=2) | slack | LabelEdgeSpan(label=7) | slack | ...
//! ```
//!
//! ### Labeled edge physical footprint (implemented)
//!
//! Labeled edge bytes are pinned to PMA leaf blocks (`span_meta.physical_start`). Growth
//! on the hot path uses in-leaf weighted slide and leaf-block relocate (`release_span` once
//! per relocate). [`rewrite_vertex_edge_span`] resolves new bases via leaf relocate, not
//! per-vertex tail append at `elem_capacity`.
//!
//! [`rebalance_vertex_edge_span`] resolves new bases via in-leaf weighted slide and
//! leaf-block relocate when pinned; after relocate, rebalance updates `stored_slots`
//! metadata only (slide already committed bucket layout). Tail append remains only for
//! unpinned rows and relocate-internal escape hatches. Default-label bypass rows still
//! use the core vertex relocate semantics.
//!
//! Default-label **bypass** rows skip LabelBuckets: [`LabeledVertex::degree`] / [`LabeledVertex::stored_slots`]
//! track logical and physical edge slots directly, and overflow uses metadata bits 4–11 as
//! [`LabeledVertex::bypass_overflow_log_head`].
//!
//! ## API note
//!
//! [`LabeledLaraGraph::push_vertex`] now returns [`LabeledOperationError`] (not [`crate::GrowFailed`])
//! and rejects normal-mode rows whose label-bucket count exceeds [`MAX_VERTEX_LABEL_BUCKETS`].
//!
//! This keeps the bucket descriptors exact-fit while allowing edge-heavy labels
//! to grow without adding allocation fields to every [`LabelBucket`].
//! Normal labeled rows use [`crate::lara::edge::EdgeStore`] as a slab allocator
//! and byte store, but they do not participate in the core LARA PMA leaf-density
//! accounting; their density/rewrite unit is the VertexEdgeSpan.
//! Default-label bypass rows still use the regular edge row path.

pub(crate) mod access;
#[cfg(feature = "canbench")]
mod bench;
#[expect(
    dead_code,
    reason = "bidirectional labeled maintenance exposes staged helpers"
)]
pub(crate) mod bidirectional;
pub mod bucket_label_key;
mod bucket_store;
pub(crate) mod deferred;
#[expect(
    dead_code,
    reason = "labeled graph contains maintenance and diagnostics entry points"
)]
pub(crate) mod graph;
/// ADR 0022 Stage 2b prototype (evidence-only; not wired into the graph).
#[cfg(any(test, feature = "canbench"))]
pub(crate) mod hub_tree_prototype;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "layout invariant checks are compiled for targeted diagnostics"
    )
)]
pub(crate) mod invariants;

/// Initial physical capacities for the labeled graph's independent storage slabs.
///
/// These values affect only fresh stores. Reopened stores use the capacities persisted in
/// their own headers. Bucket descriptors, edge slots, and inline payload bytes have different
/// growth behavior and therefore must not share one capacity value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitialCapacities {
    /// Initial number of label-bucket descriptor slots per orientation.
    pub bucket_slots: u64,
    /// Initial number of edge slots per orientation.
    pub edge_slots: u64,
    /// Initial byte capacity of the inline-payload slab per orientation.
    pub payload_bytes: u64,
}

impl InitialCapacities {
    /// Creates equal capacities for tests and generic labeled-LARA fixtures.
    pub const fn uniform(value: u64) -> Self {
        Self {
            bucket_slots: value,
            edge_slots: value,
            payload_bytes: value,
        }
    }
}
pub mod record;
pub mod slot_index;
pub(crate) mod traits;

pub use bidirectional::{
    DeferredBidirectionalLabeledError, DeferredBidirectionalLabeledLaraGraph, DeleteEdgeObserver,
    EdgeSlotMoveObserver, LabeledBidirectionalMaintenanceReport, Orientation as LabeledOrientation,
};
pub use bucket_label_key::{
    BUCKET_LABEL_DIRECTED_BIT, BUCKET_LABEL_INDEX_MASK, BucketDirectedness, BucketLabelKey,
};
pub use bucket_store::InitError as LabelBucketStoreInitError;
pub use deferred::{DeferredError, DeferredLabeledLaraGraph, MaintenanceWorkItem};
pub use graph::{EdgeRemoval, EdgeSlotMove};
pub use graph::{
    HybridOverflowEdgeReplay, InitError as LabeledGraphInitError, LabeledEdgeInlineValueBatch,
    LabeledEdgeInlineValueBatchScratch, LabeledLaraGraph, LabeledOperationError,
    LabeledOutEdgesIter, LabeledPayloadValueBatch, LabeledPayloadValueBatchScratch, OutEdgeOrder,
};
pub use record::{
    LabelBucket, LabeledVertex, LabeledVertexFieldError, MAX_VERTEX_LABEL_BUCKET_SLACK,
    MAX_VERTEX_LABEL_BUCKETS,
};
pub use traits::LabeledCsrVertex;

/// Convenience alias for the single-orientation labeled LARA graph.
pub type LabeledLara<E, M> = LabeledLaraGraph<E, M>;
/// Convenience alias for the deferred-maintenance labeled LARA graph.
pub type DeferredLabeledLara<E, M> = DeferredLabeledLaraGraph<E, M>;
/// Convenience alias for the deferred bidirectional labeled LARA graph.
pub type DeferredBidirectionalLabeledLara<E, M> = DeferredBidirectionalLabeledLaraGraph<E, M>;
