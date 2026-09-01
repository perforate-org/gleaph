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
        ltb_raw_block_store::LtbRawBlockStore,
        record::{LabelBucket, LabeledVertex},
    },
    lara::{
        edge::EdgeStore, edge_inline_property::EdgeInlinePropertyBytesStore, vertex::VertexStore,
    },
    traits::CsrEdge,
};
use ic_stable_structures::Memory;
use std::{cell::Cell, marker::PhantomData};

const DEFAULT_SEGMENT_SIZE: u32 = 16;
const BULK_BUCKET_SEARCH_MIN_DEGREE: u32 = 16;
const BUCKET_LOOKUP_CACHE_ENTRIES: usize = 64;

/// Same threshold as core LARA leaf density (`actual/total` on one PMA leaf).
const LEAF_VERTEX_EDGE_SEGMENT_DENSITY: f64 = 1.0;

// ---- Plan 0318 §Step 3 cap enforcement constants ---------------------------

/// CSR slab mode cap: `alloc_space = stored_slots + alloc_gap ≤ T_PROMOTE = 4096 slots (16 KiB)`.
///
/// Tree mode cap: `alloc_space = stored_slots ≤ TREE_STRUCTURAL_CAP = 2^30 slots`
/// (the `MAX_DEPTH = 3` fail-closed structural boundary; see ADR 0088 §4).
///
/// The cap is on `alloc_space = stored_slots + alloc_gap`, NOT on `stored_slots` alone
/// (per Plan 0317 amend). LARA rebalance naturally keeps `alloc_space` bounded via
/// weighted gap allocation.
///
/// `R_MAX = 1024` is the **root-array fan-out cap** (number of `u32` block_id
/// entries that fit in one root); the derive-depth formula in
/// `tree_csr_prototype::derive_depth` and the `root_len` per-level cap use it.
/// `root_len > R_MAX` is the **deepen** trigger (Plan 0318 Step 7), not a
/// slot-cap reject.
pub(crate) const T_PROMOTE: u32 = 4096;

/// Root-array fan-out cap (block_id entries per tree root). See ADR 0088 §4.
/// Used by `derive_depth` and by the Step 7 deepen trigger; **not** the
/// logical-slot cap on a tree bucket.
pub(crate) const R_MAX: u32 = 1024;

/// Tree-mode slot cap: `alloc_space = stored_slots ≤ 2^30` (the coverage at
/// `MAX_DEPTH = 3` per ADR 0088 §4: depth 1 ≤ 2²⁰, depth 2 ≤ 2³⁰, depth 3 ≤
/// 2⁴⁰; the fail-closed `MAX_DEPTH` boundary is 2³⁰ distinct edge slots under
/// the current `B = 1024` slots/block, `R_MAX = 1024` root entries/level
/// encoding). Practical depth = 3 is bound by physical stable memory well
/// before the 2⁴⁰ theoretical coverage.
pub(crate) const TREE_STRUCTURAL_CAP: u32 = 1 << 30;

/// Per-vertex edge-label-type limit (per Plan 0317 amend).
///
/// Industry-largest vs DataStax Enterprise Graph's official 200. Typical graph
/// schemas use < 100 labels per vertex; 1024 covers all practical workloads
/// (social, e-commerce, knowledge graphs, IoT, finance) with 10-200× schema
/// evolution headroom.
pub(crate) const MAX_BUCKETS_PER_VERTEX: u32 = 1024;

/// Bucket backing mode: the cap is mode-dependent, so every call site that
/// allocates bucket storage must dispatch through `check_alloc_cap` to get the
/// right cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketMode {
    /// Slab mode (CSR slab, gap may be > 0).
    Slab,
    /// Tree mode (LTB-backed, gap-0 invariant).
    Tree,
}

impl BucketMode {
    #[inline]
    pub fn from_bucket(bucket: &LabelBucket) -> Self {
        if bucket.is_tree_mode() {
            Self::Tree
        } else {
            Self::Slab
        }
    }
}

/// LARA rebalance weighted gap allocation (slab mode only; tree mode is gap-0).
///
/// `alloc_gap(stored_slots)` returns the gap slots the LARA rebalance would
/// reserve on top of `stored_slots` for the current degree. It is a pure
/// function of `stored_slots` (no graph state). Plan 0317 §Step 3 used the
/// simple `T_PROMOTE - stored_slots` form; we keep that here as the canonical
/// implementation. The real LARA rebalance path may use a more sophisticated
/// weighted allocation in a future slice.
#[inline]
pub(crate) fn alloc_gap(stored_slots: u32) -> u32 {
    T_PROMOTE.saturating_sub(stored_slots)
}

/// Compute `alloc_space` for a bucket. Single source of truth used by every
/// cap-enforcement call site.
///
/// CSR slab mode: `alloc_space = stored_slots + alloc_gap(stored_slots)`.
/// Tree mode: `alloc_space = stored_slots` (gap-0 invariant).
#[inline]
pub(crate) fn compute_bucket_allocation(bucket: &LabelBucket) -> u32 {
    if bucket.is_tree_mode() {
        bucket.stored_slots
    } else {
        bucket
            .stored_slots
            .saturating_add(alloc_gap(bucket.stored_slots))
    }
}

/// Cap for the bucket's current mode.
///
/// - Slab: `T_PROMOTE = 4096` slots.
/// - Tree: `TREE_STRUCTURAL_CAP = 2^30` slots (the `MAX_DEPTH = 3` fail-closed
///   boundary). `R_MAX = 1024` is the **root-array fan-out cap** governing
///   `deepen` (Step 7), not a slot cap.
#[inline]
pub(crate) fn cap_for_mode(bucket: &LabelBucket) -> u32 {
    if bucket.is_tree_mode() {
        TREE_STRUCTURAL_CAP
    } else {
        T_PROMOTE
    }
}

/// Check the `alloc_space` cap before any allocation. Returns a typed error when
/// `current_alloc_space + increment > cap(bucket.mode)`.
pub(crate) fn check_alloc_cap(
    bucket: &LabelBucket,
    increment: u32,
) -> Result<(), LabeledOperationError> {
    let cap = cap_for_mode(bucket);
    let current = compute_bucket_allocation(bucket);
    if current.saturating_add(increment) > cap {
        return Err(LabeledOperationError::AllocSpaceCapReached {
            current_alloc_space: current,
            cap,
            mode: BucketMode::from_bucket(bucket),
        });
    }
    Ok(())
}

/// Check the per-vertex bucket count cap. Returns a typed error when the
/// vertex already holds `MAX_BUCKETS_PER_VERTEX` buckets.
pub(crate) fn check_vertex_bucket_count_cap(
    vertex: &LabeledVertex,
) -> Result<(), LabeledOperationError> {
    let count = vertex.degree;
    if count >= MAX_BUCKETS_PER_VERTEX {
        return Err(LabeledOperationError::VertexBucketCountCapReached {
            current_count: count,
            cap: MAX_BUCKETS_PER_VERTEX,
        });
    }
    Ok(())
}

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
pub(crate) use values::force_next_inline_property_bytes_compaction_error;
pub mod batch_write;
#[cfg(test)]
mod batch_write_test;
mod error;
mod init;
mod insert;
mod iter;
pub(crate) mod leaf_pin;
mod promote;
mod remove;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod traverse;
mod tree_read;

pub use traverse::BucketEntryPosition;
mod values;

pub(crate) use crate::traverse::EdgeSlotState;
pub use bucket::{LabelBucketPlacementInfo, LeafBucketPlacementStats};
pub use error::{InitError, LabeledOperationError, OutEdgeOrder};
pub use insert::{EdgePlacementPolicy, ScalarInsertLocation, ScalarInsertStorage};
pub use iter::LabeledOutEdgesIter;
pub use iter::{
    HybridOverflowEdgeReplay, LabeledEdgeInlinePropertyBatch,
    LabeledEdgeInlinePropertyBatchScratch, LabeledInlinePropertyValueBatch,
    LabeledInlinePropertyValueBatchScratch,
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
    values: EdgeInlinePropertyBytesStore<M>,
    /// Tree-mode LTB block store. Per Plan 0318 §Step 2, one store per orientation
    /// (forward + reverse), so the bidirectional graph holds two of these.
    /// Lazily created (the LTB region stays physically unallocated until the
    /// first promotion) per ADR 0088 §1.
    ltb: LtbRawBlockStore<M>,
    default_label: BucketLabelKey,
    last_bucket_lookup: Cell<Option<BucketLookupCache>>,
    inline_property_bytes_compaction_deferred: Cell<bool>,
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

/// Aggregated inline property bytes storage accounting for one labeled graph orientation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabeledInlinePropertyBytesStorageStats {
    /// Bytes required by live inline property values in bucket-local order.
    pub live_bytes: u64,
    /// Bytes reserved by inline property bytes slab spans owned by labeled vertices.
    pub allocated_bytes: u64,
    /// InlinePropertyBytes slab backing capacity in bytes.
    pub byte_capacity: u64,
    /// Exclusive end of the append-only occupied inline property bytes slab prefix.
    pub slab_occupied_tail: u64,
    /// Bytes available in retired inline_property_bytes free spans.
    pub free_bytes: u64,
    /// Largest retired inline property bytes free span.
    pub largest_free_span: u64,
    /// Number of retired inline property bytes free spans.
    pub free_span_count: u64,
}

/// Result of a inline-property-bytes-only slab compaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabeledInlinePropertyBytesCompactionResult {
    /// Number of inline property bytes slab spans moved.
    pub moved_spans: u32,
    /// Total inline property bytes copied into earlier free spans.
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

// ---- Plan 0318 §Step 3 cap enforcement tests -------------------------------

#[cfg(test)]
mod cap_enforcement_tests {
    use super::*;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::record::LabeledVertex;

    #[test]
    fn compute_bucket_allocation_returns_stored_slots_for_slab_mode_without_gap() {
        // For stored_slots = 0 in slab mode: alloc_space = 0 + (T_PROMOTE - 0) = T_PROMOTE.
        // The helper returns the cap-bounded allocation: min(T_PROMOTE, T_PROMOTE) = T_PROMOTE.
        let bucket = LabelBucket::from_parts(BucketLabelKey::default(), 0, 0, 0, -1);
        assert_eq!(compute_bucket_allocation(&bucket), T_PROMOTE);
        assert_eq!(cap_for_mode(&bucket), T_PROMOTE);
        assert_eq!(BucketMode::from_bucket(&bucket), BucketMode::Slab);
    }

    #[test]
    fn compute_bucket_allocation_returns_stored_slots_for_tree_mode() {
        // Tree mode is gap-0: alloc_space = stored_slots. For stored_slots = 50,
        // alloc_space = 50 and cap = TREE_STRUCTURAL_CAP = 2^30 (the
        // `MAX_DEPTH = 3` fail-closed boundary per ADR 0088 §4).
        // `R_MAX` is the root-array fan-out cap (deepen trigger), not the slot cap.
        let bucket =
            LabelBucket::from_parts(BucketLabelKey::default(), 0, 5, 50, -1).with_tree_mode(true);
        assert_eq!(compute_bucket_allocation(&bucket), 50);
        assert_eq!(cap_for_mode(&bucket), TREE_STRUCTURAL_CAP);
        assert_eq!(BucketMode::from_bucket(&bucket), BucketMode::Tree);
    }

    #[test]
    fn check_alloc_cap_tree_mode_promoted_bucket_not_rejected() {
        // Regression for the F1 unit-confusion fix: a promoted tree bucket
        // with stored_slots = T_PROMOTE = 4096 (the post-promotion
        // invariant) must NOT be rejected by `check_alloc_cap` because the
        // tree-mode cap is `TREE_STRUCTURAL_CAP = 2^30`, not `R_MAX = 1024`.
        let bucket = LabelBucket::from_parts(BucketLabelKey::default(), 0, 4096, T_PROMOTE, -1)
            .with_tree_mode(true);
        assert_eq!(cap_for_mode(&bucket), TREE_STRUCTURAL_CAP);
        check_alloc_cap(&bucket, 1).expect("promoted bucket must not be rejected");
    }

    #[test]
    fn check_alloc_cap_tree_mode_rejects_beyond_structural_coverage() {
        // The MAX_DEPTH = 3 fail-closed boundary (2^30 slots) is
        // effectively equivalent to the u32 slot cap (2^32 - 1) for any
        // practical stored_slots; we test the boundary by constructing a
        // bucket at the cap and asserting that even increment = 0
        // succeeds (sanity), then a pathologically-large increment is
        // rejected with the cap error.
        let bucket = LabelBucket::from_parts(
            BucketLabelKey::default(),
            0,
            TREE_STRUCTURAL_CAP,
            TREE_STRUCTURAL_CAP,
            -1,
        )
        .with_tree_mode(true);
        check_alloc_cap(&bucket, 0).expect("zero increment must always succeed");
        // `u32::MAX - TREE_STRUCTURAL_CAP` is a saturating_add that
        // wraps to u32::MAX, which is > TREE_STRUCTURAL_CAP.
        let err = check_alloc_cap(&bucket, u32::MAX).expect_err("must reject at u32::MAX");
        assert!(
            matches!(err, LabeledOperationError::AllocSpaceCapReached { .. }),
            "expected AllocSpaceCapReached, got {err:?}"
        );
    }

    #[test]
    fn check_alloc_cap_slab_mode_triggers_at_t_promote() {
        // Slab mode: stored_slots = 4090, alloc_gap = T_PROMOTE - 4090 = 6.
        // alloc_space = 4090 + 6 = 4096 = T_PROMOTE. Any positive increment triggers the cap.
        let bucket = LabelBucket::from_parts(BucketLabelKey::default(), 0, 0, 4090, -1);
        let current = compute_bucket_allocation(&bucket);
        assert_eq!(current, T_PROMOTE, "alloc_space at cap");
        let err = check_alloc_cap(&bucket, 1).expect_err("must reject at cap");
        match err {
            LabeledOperationError::AllocSpaceCapReached {
                current_alloc_space,
                cap,
                mode,
            } => {
                assert_eq!(current_alloc_space, T_PROMOTE);
                assert_eq!(cap, T_PROMOTE);
                assert_eq!(mode, BucketMode::Slab);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn check_vertex_bucket_count_cap_triggers_at_max_buckets_per_vertex() {
        // Vertex with degree = MAX_BUCKETS_PER_VERTEX (1024) is at-cap; any
        // attempt to add another bucket must reject.
        let vertex = LabeledVertex::try_from_parts(0, MAX_BUCKETS_PER_VERTEX, 0, 0, 0).unwrap();
        let err = check_vertex_bucket_count_cap(&vertex)
            .expect_err("vertex at MAX_BUCKETS_PER_VERTEX must reject");
        match err {
            LabeledOperationError::VertexBucketCountCapReached { current_count, cap } => {
                assert_eq!(current_count, MAX_BUCKETS_PER_VERTEX);
                assert_eq!(cap, MAX_BUCKETS_PER_VERTEX);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        // A vertex with degree < MAX passes:
        let under_cap = LabeledVertex::try_from_parts(0, 100, 0, 0, 0).unwrap();
        assert!(check_vertex_bucket_count_cap(&under_cap).is_ok());
    }
}
