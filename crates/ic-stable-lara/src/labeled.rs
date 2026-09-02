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
pub mod bidirectional;
pub mod bucket_label_key;
mod bucket_store;
#[expect(
    dead_code,
    reason = "labeled graph contains maintenance and diagnostics entry points"
)]
pub(crate) mod graph;
pub use graph::batch_write;
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
/// ADR 0088 §1 LTB block store, raw stable-memory backend (Plan 0315).
///
/// Production module as of Plan 0318 §Step 2: the graph's per-orientation
/// `ltb` field lives here. The old `any(test, feature = "canbench")` gate was
/// removed — a plain production `cargo check` failed with it in place (the
/// gate predated the production wiring and the green-bar matrix never ran a
/// feature-less lib check).
pub(crate) mod ltb_raw_block_store;
/// Plan 0313 amend: cargo-test high-degree sweep (host wall-clock, 1M edges).
#[cfg(test)]
mod tree_csr_high_degree_test {
    // Plan 0324: the `#[ignore]`d prototype host tests in
    // `tree_csr_high_degree_test.rs` were removed (user decision
    // 2026-09-01). The module is kept as an empty stub so the
    // `mod tree_csr_high_degree_test;` declaration compiles; the
    // file is restored from git history if the high-degree host
    // sweep is ever revived outside the canbench path.
}
/// ADR 0088 Tree-CSR prototype helpers (`B`, `derive_depth`, `root_len`) used
/// by the production tree-mode read/write paths (Plan 0318 §Steps 5-7). The
/// `TreeCsrBucket` value type remains evidence-only; the gate was removed when
/// production paths began importing the helpers.
pub(crate) mod tree_csr_prototype;

/// Initial physical capacities for the labeled graph's independent storage slabs.
///
/// These values affect only fresh stores. Reopened stores use the capacities persisted in
/// their own headers. Bucket descriptors, edge slots, and inline property bytes have different
/// growth behavior and therefore must not share one capacity value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitialCapacities {
    /// Initial number of label-bucket descriptor slots per orientation.
    pub bucket_slots: u64,
    /// Initial number of edge slots per orientation.
    pub edge_slots: u64,
    /// Initial byte capacity of the inline-property-bytes slab per orientation.
    pub inline_property_bytes: u64,
}

impl InitialCapacities {
    /// Creates equal capacities for tests and generic labeled-LARA fixtures.
    pub const fn uniform(value: u64) -> Self {
        Self {
            bucket_slots: value,
            edge_slots: value,
            inline_property_bytes: value,
        }
    }
}
pub mod record;
pub mod slot_index;
pub(crate) mod traits;

pub use bidirectional::counterpart::CounterpartLookupError;
pub use bidirectional::{
    CanonicalEdgeOccurrence, DeferredBidirectionalLabeledError,
    DeferredBidirectionalLabeledLaraGraph, DeleteEdgeObserver, DeletedEdge, EdgeSlotMoveObserver,
    LabeledBidirectionalMaintenanceReport, Orientation as LabeledOrientation, ScalarInsertPair,
};
pub use bucket_label_key::{
    BUCKET_LABEL_DIRECTED_BIT, BUCKET_LABEL_INDEX_MASK, BucketDirectedness, BucketLabelKey,
};
pub use bucket_store::InitError as LabelBucketStoreInitError;
pub use graph::BucketEntryPosition;
pub use graph::{EdgePlacementPolicy, ScalarInsertLocation, ScalarInsertStorage};
pub use graph::{EdgeRemoval, EdgeSlotMove};
pub use graph::{
    HybridOverflowEdgeReplay, InitError as LabeledGraphInitError, LabeledEdgeInlinePropertyBatch,
    LabeledEdgeInlinePropertyBatchScratch, LabeledInlinePropertyValueBatch,
    LabeledInlinePropertyValueBatchScratch, LabeledLaraGraph, LabeledOperationError,
    LabeledOutEdgesIter, OutEdgeOrder,
};
pub use graph::{LabelBucketPlacementInfo, LeafBucketPlacementStats};
pub use record::{
    LabelBucket, LabeledVertex, LabeledVertexFieldError, MAX_VERTEX_LABEL_BUCKET_SLACK,
    MAX_VERTEX_LABEL_BUCKETS,
};
pub use traits::LabeledCsrVertex;

/// Convenience alias for the single-orientation labeled LARA graph.
pub type LabeledLara<E, M> = LabeledLaraGraph<E, M>;
/// Convenience alias for the deferred bidirectional labeled LARA graph.
pub type DeferredBidirectionalLabeledLara<E, M> = DeferredBidirectionalLabeledLaraGraph<E, M>;
