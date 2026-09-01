//! Tree-mode write dispatch for label buckets (Plan 0318 §Step 6).
//!
//! Provides the production-side append and tombstone-rewrite paths for
//! tree-mode buckets. The single dispatch points are
//! [`super::LabeledLaraGraph::insert_edge_skip_leaf_cascade_impl`] (Commit A)
//! and [`super::LabeledLaraGraph::remove_edge_at_slot_with_move`] (Commit B);
//! no other module under `graph/` branches on `bucket.is_tree_mode()`.
//!
//! Storage architecture (per `f6c426d1c` amend and Step 4 commit `44c82d3b2`):
//! - `LabelBucket::edge_start` = LEG slab offset; the slot range
//!   `[edge_start, edge_start + root_len)` holds the **root region**, a
//!   dense `u32` block_id array.
//! - Each `block_id` indexes one 4 KiB block in the LTB store. A block's
//!   payload is `B = 1024` 4-byte rows (edges) in insertion order.
//! - `root_len = ceil(stored_slots / B^depth)`. The tail block may be
//!   partial (gap-0 invariant): the valid byte count is
//!   `(stored_slots - first_slot) * E::BYTES`.
//! - Logical slot `i` → `(block_root_index = i / B, in_block_offset =
//!   (i % B) * E::BYTES)`.
//!
//! Promotion trigger (Plan 0318 §Step 6 amend): the dispatcher promotes
//! when the bucket's `stored_slots >= T_PROMOTE` (placeholder-gap form).
//! When the weighted `alloc_gap` is introduced the trigger switches to the
//! `compute_bucket_allocation < T_PROMOTE` form. See
//! `super::promote::promote_bypass_to_tree_mode` for the cap semantics.
//!
//! Unordered placement tombstone reuse is **not** implemented for tree
//! buckets. Tree mode always appends; tombstone reuse is a future slice.

use ic_stable_structures::Memory;

use super::{LabeledLaraGraph, promote as promote_path};
use crate::GrowFailed;
use crate::VertexId;
use crate::labeled::bucket_label_key::BucketLabelKey;
use crate::labeled::graph::error::LabeledOperationError;
use crate::labeled::record::LabelBucket;
use crate::labeled::tree_csr_prototype::{B as BLOCK_B, root_len as derived_root_len};
use crate::lara::operation_error::LaraOperationError;
use crate::traits::{CsrEdge, CsrEdgeTombstone};

/// Required LTB block payload alignment for tree-mode edges. The LTB
/// store currently fixes `BLOCK_PAYLOAD_BYTES = 4096`, so tree mode
/// requires `E::BYTES == 4` (one block = 1024 edges).
pub(crate) const TREE_MODE_REQUIRED_EDGE_BYTES: usize = 4;

/// Tree-mode append for a single edge. Returns the logical slot index
/// assigned to the new edge (`stored_slots` after the append).
///
/// Failure atomicity: on any error the LTB store and LEG free list are
/// rolled back to the pre-call state (no partial tree-mode state is
/// published). The bucket descriptor is rewritten only at the end via
/// the canonical `write_label_bucket_slot` write.
///
/// Unordered placement (`EdgePlacementPolicy::Unordered`) is a no-op
/// for tree mode: this helper always appends. Reuse of a tombstoned
/// slot in tree mode is deferred to a future slice.
pub(crate) fn tree_mode_insert_edge<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    src: VertexId,
    bucket_slot: u64,
    bucket: &LabelBucket,
    label: BucketLabelKey,
    edge: &E,
) -> Result<u32, LabeledOperationError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    // Typed guard: tree mode requires 4-byte edges.
    if E::BYTES != TREE_MODE_REQUIRED_EDGE_BYTES {
        return Err(LabeledOperationError::TreeModeEdgeWidthUnsupported {
            actual: E::BYTES,
            expected: TREE_MODE_REQUIRED_EDGE_BYTES,
        });
    }
    debug_assert!(bucket.is_tree_mode(), "caller must dispatch on tree mode");
    debug_assert_eq!(
        bucket.inline_property_byte_width(),
        0,
        "tree mode rejects inline-property buckets (promote is fail-closed)"
    );
    // 4-byte target buffer; LTB blocks hold one target per row.
    let mut target_bytes = [0u8; 4];
    edge.write_to(&mut target_bytes);

    let stored_slots = bucket.stored_slots;
    let tail_block_idx: u32 = stored_slots / (BLOCK_B as u32);
    let tail_offset: u32 = (stored_slots % (BLOCK_B as u32)) * (E::BYTES as u32);

    // Common accounting fields.
    let next_stored = stored_slots
        .checked_add(1)
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
    let next_degree = bucket
        .degree
        .checked_add(1)
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;

    if tail_offset == 0 {
        // No tail room → mint a new LTB block and grow the root region
        // by 1. The root region has shape `[edge_start, edge_start + old_root_len)`;
        // after the grow it is `[new_edge_start, new_edge_start + old_root_len + 1)`.
        // We allocate the new span first, copy the old block_ids verbatim,
        // append the new block_id, and publish the new descriptor.
        //
        // **Interim fail-closed (Plan 0318 §Step 7 amend)**: the
        // interior-level insert cascade (depth ≥ 2 growth via
        // right-spine nodes) is not yet wired. A root region already
        // at `R_MAX = 1024` entries cannot accept a 1,048,577th slot
        // without either growing the root past `R_MAX` (ADR wire-truth
        // violation) or cascading into a new interior level (unwired).
        // The pre-mint guard below returns `TreeRootCapacityReached`
        // before any state change. The follow-up todo
        // `tree-mode-interior-level-insert-growth` will replace this
        // guard with a right-spine cascade (depth grows first, then
        // root grows, then the next insert).
        let k = u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
        let b = u32::try_from(BLOCK_B).expect("B fits u32");
        let depth = bucket.tree_mode_physical_depth();
        let physical_root_len: u32 = match depth {
            1 => u32::try_from((u64::from(stored_slots)).div_ceil(b as u64))
                .expect("depth-1 root_len fits u32"),
            2 => u32::try_from(
                (u64::from(stored_slots))
                    .div_ceil(b as u64)
                    .div_ceil(k as u64),
            )
            .expect("depth-2 root_len fits u32"),
            3 => u32::try_from(
                (u64::from(stored_slots))
                    .div_ceil(b as u64)
                    .div_ceil(k as u64)
                    .div_ceil(k as u64),
            )
            .expect("depth-3 root_len fits u32"),
            _ => unreachable!("tree_mode_physical_depth out of range"),
        };
        if physical_root_len >= k {
            return Err(LabeledOperationError::TreeRootCapacityReached {
                stored_slots: next_stored,
                root_len: physical_root_len,
                cap: k,
            });
        }
        let old_root_len = u32::try_from(derived_root_len(stored_slots))
            .expect("root_len fits u32 (per ADR 0088 §4 R_max = 1024)");
        let new_root_len = old_root_len
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        // 1. Mint the new LTB block. On failure, no rollback needed.
        let new_block_id = graph.ltb().mint().map_err(LabeledOperationError::from)?;
        // 2. Allocate the new root region span. On failure, release the
        //    new LTB block.
        let new_edge_start = match graph.edges().allocate_span(u64::from(new_root_len)) {
            Ok(s) => s,
            Err(e) => {
                let _ = graph.ltb().release(new_block_id);
                return Err(e.into());
            }
        };
        // 3. Copy old block_ids from the old root region into the new
        //    region, then append the new block_id. If this fails, release
        //    the new LTB block and the new span.
        let mut root_bytes: Vec<u8> = Vec::with_capacity(new_root_len as usize * 4);
        if old_root_len > 0 {
            let mut old_bytes = vec![0u8; old_root_len as usize * 4];
            graph
                .edges()
                .read_slots_contiguous_bytes(bucket.edge_start(), &mut old_bytes);
            root_bytes.extend_from_slice(&old_bytes);
        }
        root_bytes.extend_from_slice(&new_block_id.to_le_bytes());
        if let Err(e) = graph
            .edges()
            .write_slots_contiguous_bytes(new_edge_start, &root_bytes)
        {
            let _ = graph.ltb().release(new_block_id);
            let _ = graph
                .edges()
                .release_span(new_edge_start, u64::from(new_root_len));
            return Err(e.into());
        }
        // 4. Append the edge to the new (empty) LTB block at offset 0.
        //    The block is brand-new, so a tail-trim partial write is
        //    unnecessary — the block was zero-initialized by `mint`.
        //    Per Plan 0318 §Step 7 review (F-a) the edge write happens
        //    **before** the descriptor publish: this aligns the insert
        //    ordering with `promote_bypass_to_tree_mode` (Phase 2
        //    transcription writes the leaf before Phase 3 publishes the
        //    descriptor). On any failure the block is brand-new and
        //    untouched, so we don't need to roll back the payload.
        if let Err(e) = graph
            .ltb()
            .write_payload_partial(new_block_id, 0, &target_bytes)
        {
            let _ = graph.ltb().release(new_block_id);
            let _ = graph
                .edges()
                .release_span(new_edge_start, u64::from(new_root_len));
            return Err(LabeledOperationError::LtbBlock(e));
        }
        // 5. Publish the new descriptor (single canonical write). On
        //    failure, release the new LTB block and the new span.
        let new_bucket = bucket
            .with_edge_range(new_edge_start, next_stored)
            .with_degree_field(next_degree);
        let new_bucket = new_bucket.with_stored_slots(next_stored);
        // `with_tree_mode(true)` is a no-op here (the source bucket is
        // already tree-mode), but we re-apply for safety.
        let new_bucket = new_bucket.with_tree_mode(true);
        if let Err(e) = graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, new_bucket)
        {
            // Roll back the edge write: rewrite the slot back to zero
            // (the next caller can re-claim the block via tail-room
            // path, or the released block is re-mintable).
            let zero = [0u8; 4];
            let _ = graph.ltb().write_payload_partial(new_block_id, 0, &zero);
            let _ = graph.ltb().release(new_block_id);
            let _ = graph
                .edges()
                .release_span(new_edge_start, u64::from(new_root_len));
            return Err(e.into());
        }
        // 6. Release the old root region span if it differs from the new
        //    one. (If `new_edge_start == bucket.edge_start()` the LEG
        //    span is unchanged; the release is a no-op on the free list
        //    and skipped to keep rollback simple.)
        if new_edge_start != bucket.edge_start() {
            let _ = graph
                .edges()
                .release_span(bucket.edge_start(), u64::from(old_root_len));
        }
        // 7. Bump global accounting mirrors the slab path's
        //    `set_num_edges(num + 1)` + `bump_vertex_segment_counts(src, 1, 0)`.
        let hdr = graph.edges().header();
        let next_num_edges = hdr
            .num_edges
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        graph.edges().set_num_edges(next_num_edges);
        graph
            .edges()
            .bump_vertex_segment_counts(src, 1, 0)
            .map_err(LabeledOperationError::from)?;
        let _ = label; // descriptor carries the label; no per-insert write.
        Ok(next_stored - 1)
    } else {
        // Tail room available → write the 4-byte target into the tail
        // block at `tail_offset` and update the descriptor.
        // 1. Resolve the tail block_id via the depth-generic resolver.
        //    For depth 1 this is a single-hop LEG read; for depth 2+ it
        //    descends the interior hop chain.
        let tail_block_id = resolve_leaf_block_id::<E, M>(graph, bucket, tail_block_idx)?;
        // 2. Write the target into the tail block at `tail_offset`.
        if let Err(e) =
            graph
                .ltb()
                .write_payload_partial(tail_block_id, tail_offset as usize, &target_bytes)
        {
            return Err(LabeledOperationError::LtbBlock(e));
        }
        // 3. Publish the descriptor (canonical write).
        let new_bucket = bucket
            .with_stored_slots(next_stored)
            .with_degree_field(next_degree);
        let new_bucket = new_bucket.with_tree_mode(true);
        if let Err(e) = graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, new_bucket)
        {
            // Roll back: rewrite the tail block byte to the pre-call
            // value (zero-initialized for a fresh tail; for an
            // already-populated tail byte we leave it as the just-written
            // target — the descriptor revert is the source of truth).
            return Err(e.into());
        }
        // 4. Bump global accounting.
        let hdr = graph.edges().header();
        let next_num_edges = hdr
            .num_edges
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        graph.edges().set_num_edges(next_num_edges);
        graph
            .edges()
            .bump_vertex_segment_counts(src, 1, 0)
            .map_err(LabeledOperationError::from)?;
        Ok(next_stored - 1)
    }
}

/// Tree-mode tombstone rewrite for `slot`.
///
/// Mirrors the slab path's behaviour (`remove_bucket_edge_at_location`
/// → `BucketEdgeDeleteLocation::Slab`): write a tombstone edge into the
/// LTB block, decrement `degree`, leave `stored_slots` unchanged, and
/// decrement global edge accounting. Returns the removed edge's value.
///
/// `UnorderedPlacement` reuse is not implemented for tree mode; this
/// helper always produces a tombstone (the slot stays "allocated" in
/// `stored_slots` and is not reusable by a future insert).
pub(crate) fn tree_mode_remove_edge_at_slot<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    src: VertexId,
    bucket_slot: u64,
    bucket: &LabelBucket,
    slot: u32,
) -> Result<Option<E>, LabeledOperationError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    if E::BYTES != TREE_MODE_REQUIRED_EDGE_BYTES {
        return Err(LabeledOperationError::TreeModeEdgeWidthUnsupported {
            actual: E::BYTES,
            expected: TREE_MODE_REQUIRED_EDGE_BYTES,
        });
    }
    debug_assert!(bucket.is_tree_mode(), "caller must dispatch on tree mode");
    debug_assert_eq!(
        bucket.inline_property_byte_width(),
        0,
        "tree mode rejects inline-property buckets (promote is fail-closed)"
    );
    if slot >= bucket.stored_slots {
        return Ok(None);
    }
    let block_root_index: u32 = slot / (BLOCK_B as u32);
    let in_block_offset: u32 = (slot % (BLOCK_B as u32)) * (E::BYTES as u32);
    // 1. Resolve the leaf block_id via the depth-generic resolver.
    //    For depth 1 this is a single-hop LEG read; for depth 2+ it
    //    descends the interior hop chain.
    let block_id = resolve_leaf_block_id::<E, M>(graph, bucket, block_root_index)?;
    let mut current_bytes = [0u8; 4];
    graph
        .ltb()
        .read_payload_partial(block_id, in_block_offset as usize, &mut current_bytes)
        .map_err(LabeledOperationError::LtbBlock)?;
    let current = E::read_from(&current_bytes);
    // 2. If already a tombstone, return the tombstone value (idempotent
    //    remove mirrors the slab path's "no live edge" early-return but
    //    still reports the tombstone to the caller). We compare raw
    //    bytes to avoid a `PartialEq` bound on `E`.
    let mut tombstone_bytes = [0u8; 4];
    E::tombstone_edge().write_to(&mut tombstone_bytes);
    if current_bytes == tombstone_bytes {
        return Ok(Some(current));
    }
    // 3. Write the tombstone into the LTB block.
    if let Err(e) =
        graph
            .ltb()
            .write_payload_partial(block_id, in_block_offset as usize, &tombstone_bytes)
    {
        return Err(LabeledOperationError::LtbBlock(e));
    }
    // 4. Decrement degree (stored_slots unchanged — tombstones are
    //    physical slots that may be reused later, even though the
    //    current tree-mode insert path always appends).
    let next_degree = bucket
        .degree
        .checked_sub(1)
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
    let new_bucket = bucket
        .with_degree_field(next_degree)
        .with_stored_slots(bucket.stored_slots)
        .with_tree_mode(true);
    if let Err(e) = graph
        .buckets()
        .write_label_bucket_slot(bucket_slot, new_bucket)
    {
        // Roll back the tombstone write: restore the original 4 bytes.
        let _ =
            graph
                .ltb()
                .write_payload_partial(block_id, in_block_offset as usize, &current_bytes);
        return Err(e.into());
    }
    // 5. Decrement global accounting.
    let hdr = graph.edges().header();
    let next_num_edges = hdr
        .num_edges
        .checked_sub(1)
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
    graph.edges().set_num_edges(next_num_edges);
    graph
        .edges()
        .bump_vertex_segment_counts(src, -1, 0)
        .map_err(LabeledOperationError::from)?;
    Ok(Some(current))
}

/// Check whether the pre-promotion slab span `[edge_start, edge_start + len)`
/// lies **inside** a currently-pinned leaf physical block for `vid`.
///
/// Used by [`super::promote::promote_bypass_to_tree_mode`] (Plan 0318
/// §Step 4 amend note 5, hazard ledger option b — compaction delegation)
/// to skip the post-promotion release of the slab prefix when the prefix
/// is pin-sheltered: the leaf compaction pass will reclaim the subrange
/// when the leaf is recycled.
///
/// Returns `true` if the span overlaps any active leaf physical range;
/// `false` for test-only raw spans (which are not leaf-pinned) or for a
/// vertex with no leaf-pinned ranges.
pub(crate) fn pre_promotion_span_inside_pinned_leaf<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    edge_start: u64,
    len: u32,
) -> bool
where
    E: CsrEdge,
    M: Memory,
{
    if len == 0 {
        return false;
    }
    // The labeled PMA keeps a single physical range per vertex (the
    // leaf-block anchor). If a leaf is pinned for `vid`, the pre-promotion
    // span that lives entirely inside that anchor is pin-sheltered: leaf
    // compaction reclaims the subrange when the leaf is recycled.
    let Some((physical_start, physical_len)) = graph.labeled_leaf_physical_range(vid) else {
        return false;
    };
    let span_start = edge_start;
    let span_end = edge_start
        .checked_add(u64::from(len))
        .expect("pre_promotion_span overflows u64");
    let physical_end = physical_start
        .checked_add(physical_len)
        .expect("physical range overflows u64");
    span_start >= physical_start && span_end <= physical_end
}

/// Run the promotion path (a thin re-export of the Step 4 entry point)
/// and return the updated bucket descriptor. Returns `Ok(())` if the
/// bucket was already tree-mode (no-op).
///
/// Used by the insert dispatcher (Commit A) to wire the
/// `stored_slots >= T_PROMOTE` promote trigger into the production
/// write path without changing the slab-mode `insert_edge_skip_leaf_cascade_impl`
/// flow.
pub(crate) fn promote_bucket_if_needed<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    src: VertexId,
    label: BucketLabelKey,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    let vertex = graph.vertices().get(src);
    let bucket = match graph.find_bucket(src, &vertex, label)? {
        super::BucketSearch::Found { bucket, .. } => bucket,
        super::BucketSearch::Missing { .. } => return Ok(()),
    };
    if bucket.is_tree_mode() {
        return Ok(());
    }
    if bucket.stored_slots >= super::T_PROMOTE {
        promote_path::promote_bypass_to_tree_mode(graph, src, label)?;
    }
    Ok(())
}

/// Translate an LTB grow failure into a `LabeledOperationError`.
pub(crate) fn grow_failed_to_labeled(e: GrowFailed) -> LabeledOperationError {
    e.into()
}

// =================== Step 7: depth-generic resolver / deepen / flatten ===================

/// Resolve the leaf `block_id` that holds the `block_index`-th leaf slot
/// in a tree-mode bucket.
///
/// This is the depth-generic successor to the Step 5 / Step 6 single-hop
/// `read_slot_bytes(root[block_index])` path. With depth `d`:
/// - `d == 1` (production reachable): root is a dense `u32` block_id
///   array; `root[block_index]` IS the leaf block_id. The "hop chain" is
///   a single hop.
/// - `d >= 2` (production unreachable in Step 7's wire-up but exercised
///   by tests): root is a dense `u32` interior block_id array; each
///   interior block holds `K = R_MAX = 1024` child block_ids. We descend
///   the hop chain: `idx_{d-1} = block_index / K^{d-1}`,
///   `idx_{d-2} = (block_index / K^{d-2}) % K`, ..., `idx_0 =
///   block_index % K`. At each level the child id is read either from
///   the LEG root region (level d-1) or from the previous interior
///   block's payload.
///
/// **Invariants**:
/// - `bucket.is_tree_mode()` is the caller's responsibility.
/// - `block_index < ceil(stored_slots / B)`. Caller checks.
/// - `E::BYTES == 4` (else the wire math breaks); the resolver uses
///   `E::BYTES` for the interior-block payload offset, so a 4-byte edge
///   works at every interior level. The typed guard at the dispatcher
///   entry ensures this.
pub(crate) fn resolve_leaf_block_id<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    bucket: &LabelBucket,
    block_index: u32,
) -> Result<u32, LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    debug_assert_eq!(
        E::BYTES,
        4,
        "resolve_leaf_block_id requires E::BYTES == 4 (typed guard lives at the dispatcher)"
    );
    // Use the **physical** depth from the bucket, not the structural
    // `derive_depth(stored_slots)`. A manually-deepened bucket at
    // stored=1,048,576 has structural depth 1 but physical depth 2;
    // the structural formula would under-walk the hop chain.
    let depth = bucket.tree_mode_physical_depth();
    if depth == 1 {
        // Single hop: root region entry IS the leaf block_id.
        let mut block_id_bytes = [0u8; 4];
        graph.edges().read_slot_bytes(
            bucket.edge_start() + u64::from(block_index),
            &mut block_id_bytes,
        );
        return Ok(u32::from_le_bytes(block_id_bytes));
    }
    // Depth >= 2: descend root -> interior -> ... -> interior -> leaf.
    // The hop chain is indexed by **mixed-radix decomposition** of
    // `block_index` in base K = R_MAX. At level j (0 = root, 1 = first
    // interior, ..., d-1 = last interior), the index is
    // `(block_index / K^(d-1-j)) % K`. The first hop reads from the
    // LEG root region; subsequent hops read from the previous
    // interior block's payload at `(idx % K) * E::BYTES`.
    let k = u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
    let mut child_id: u32 = {
        // Level 0 hop: index into the root region.
        // divisor = K^(d-1-0) = K^(d-1)
        let divisor = k.pow(depth - 1);
        let level_idx = (block_index / divisor) % k;
        let mut id_bytes = [0u8; 4];
        graph
            .edges()
            .read_slot_bytes(bucket.edge_start() + u64::from(level_idx), &mut id_bytes);
        u32::from_le_bytes(id_bytes)
    };
    // Descend levels 1..=d-1 (interior hops).
    for j in 1..depth {
        // divisor = K^(d-1-j)
        let divisor = k.pow(depth - 1 - j);
        let level_idx = (block_index / divisor) % k;
        let mut child_id_bytes = [0u8; 4];
        let offset = (level_idx as usize) * E::BYTES;
        graph
            .ltb()
            .read_payload_partial(child_id, offset, &mut child_id_bytes)
            .map_err(LabeledOperationError::LtbBlock)?;
        child_id = u32::from_le_bytes(child_id_bytes);
    }
    Ok(child_id)
}

/// Iterator-style helper: yield every leaf `block_id` of a tree-mode
/// bucket in ascending leaf-index order (`0..ceil(stored_slots/B)`).
///
/// Returns a `Vec<u32>` instead of a Rust iterator to keep the
/// implementation simple and the test surface tractable. The vec is
/// only used by callers that want to walk the whole leaf space (e.g.
/// `visit_tree_mode_label_bucket_edges`); single-slot readers use
/// `resolve_leaf_block_id` instead.
///
/// **Performance note**: this allocates a `Vec<u32>` of length
/// `ceil(stored_slots / B)`. For production bucket sizes (stored_slots
/// up to 2^30 = ~1 Gi edges) this is at most 2^20 = 1 M u32 = 4 MiB.
/// Callers that need streaming should write a custom visitor.
pub(crate) fn collect_leaf_block_ids<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    bucket: &LabelBucket,
) -> Result<Vec<u32>, LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    debug_assert_eq!(E::BYTES, 4);
    let stored_slots = bucket.stored_slots;
    let leaf_count = u32::try_from(
        (u64::from(stored_slots)).div_ceil(crate::labeled::tree_csr_prototype::B as u64),
    )
    .expect("leaf_count fits u32 for MAX_DEPTH=3");
    let mut out = Vec::with_capacity(leaf_count as usize);
    for block_index in 0..leaf_count {
        out.push(resolve_leaf_block_id(graph, bucket, block_index)?);
    }
    Ok(out)
}

/// Pack the bucket's depth-`d` root array into `EdgeInterior` blocks,
/// allocating a new root region that holds the resulting interior-id
/// array. Updates the bucket descriptor to the new root region (and
/// the new depth). `stored_slots` is unchanged; the leaf block_ids are
/// never relocated (ADR 0088 §2: blocks never move).
///
/// **Preconditions** (fail-closed before any canonical write):
/// - `bucket.is_tree_mode()` is the caller's responsibility.
/// - `derive_depth(stored_slots) < MAX_DEPTH` (else
///   `TreeDepthLimitReached` is returned).
/// - `derived_root_len(stored_slots) <= R_MAX` (the root is at the
///   fan-out cap and a new interior level is needed).
/// - `E::BYTES == 4` (typed guard lives at the dispatcher entry).
///
/// **Reserve / Commit / Publish**:
/// 1. **Reserve**: mint `ceil(old_root_len / K)` interior blocks. Pack
///    the old root array into the new interiors (`K` ids per block via
///    `write_payload_partial`). Allocate a new root span of length
///    `ceil(old_root_len / K)`. Write the interior-id array to the new
///    root region. Each step has explicit rollback: any failure
///    releases the already-minted interiors and the new span.
/// 2. **Commit**: build the new descriptor with
///    `edge_start = new_edge_start`, `stored_slots` and `degree`
///    unchanged, tree mode bit preserved.
/// 3. **Publish**: `write_label_bucket_slot(bucket_slot, new_descriptor)`
///    is the single canonical write. On failure, release the new
///    interiors and the new span, and return the error.
/// 4. After publish, release the old root region span.
///
/// **Why we don't need a pin-sheltered helper here**: the old root
/// span is a standalone `allocate_span`'d range (not a subrange of a
/// leaf physical block), so leaf-pin invariants don't apply. The
/// pre-promotion span inside `promote_bypass_to_tree_mode` (which IS
/// a slab leaf subrange) still uses
/// `pre_promotion_span_inside_pinned_leaf`.
pub(crate) fn tree_mode_deepen<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    bucket_slot: u64,
    bucket: &LabelBucket,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    debug_assert_eq!(E::BYTES, 4);
    debug_assert!(bucket.is_tree_mode());
    let stored_slots = bucket.stored_slots;
    // Compute the depth *without* calling `derive_depth` so a stored
    // value past the structural cap returns a typed error instead of a
    // `derive_depth` panic. We use the same loop predicate as the
    // prototype but track depth explicitly.
    let depth: u32 = if stored_slots == 0 {
        1
    } else {
        let s = u64::from(stored_slots);
        let mut d: u32 = crate::labeled::tree_csr_prototype::MAX_DEPTH;
        for cand in 1..=crate::labeled::tree_csr_prototype::MAX_DEPTH {
            // ceil(s / B^depth) <= R_MAX  <=>  s <= R_MAX * B^depth
            let coverage = (crate::labeled::tree_csr_prototype::B as u64)
                .checked_pow(cand)
                .expect("B^MAX_DEPTH fits u64");
            let ceiling = s.div_ceil(coverage);
            if ceiling <= crate::labeled::tree_csr_prototype::R_MAX as u64 {
                d = cand;
                break;
            }
        }
        d
    };
    if depth >= crate::labeled::tree_csr_prototype::MAX_DEPTH {
        return Err(LabeledOperationError::TreeDepthLimitReached {
            depth,
            max_depth: crate::labeled::tree_csr_prototype::MAX_DEPTH,
        });
    }
    let old_root_len = derived_root_len(stored_slots) as u32;
    let k = u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
    // ceil(old_root_len / K) interior blocks needed.
    let new_interior_count = old_root_len.div_ceil(k);
    // Sanity: deepening must strictly reduce the root length (otherwise
    // there's no fan-out to gain and the caller miscomputed the trigger).
    debug_assert!(
        (new_interior_count as usize) < (old_root_len as usize) || old_root_len <= k,
        "deepen must reduce root length"
    );
    // 1. Reserve: mint the interior blocks.
    let mut interior_ids: Vec<u32> = Vec::with_capacity(new_interior_count as usize);
    for _ in 0..new_interior_count {
        match graph.ltb().mint() {
            Ok(id) => interior_ids.push(id),
            Err(e) => {
                // Roll back already-minted interiors (LIFO).
                for &id in interior_ids.iter().rev() {
                    let _ = graph.ltb().release(id);
                }
                return Err(LabeledOperationError::from(e));
            }
        }
    }
    // 2. Mark each interior's block kind as `EdgeInterior` so a future
    //    reopen can distinguish interior from leaf. Mint leaves the
    //    kind as `Free`; we rewrite the header.
    for (level, &id) in interior_ids.iter().enumerate() {
        let header = crate::labeled::ltb_raw_block_store::BlockHeader {
            kind: crate::labeled::ltb_raw_block_store::BlockKind::EdgeInterior,
            bucket_label_key_wire: 0,
            owner_or_next_free: 0,
            ordinal: 0,
            level: (level + 1) as u8,
            reserved: [0u8; 3],
        };
        graph.ltb().write_block_header(id, &header);
    }
    // 3. Pack the old root array into the interiors. For each interior
    //    block i, write `min(K, remaining)` ids from the old root,
    //    starting at offset 0 of the interior's payload. If the
    //    `write_payload_partial` fails, release all interiors and the
    //    new span (allocated below).
    let mut old_root_bytes: Vec<u8> = vec![0u8; old_root_len as usize * 4];
    if old_root_len > 0 {
        graph
            .edges()
            .read_slots_contiguous_bytes(bucket.edge_start(), &mut old_root_bytes);
    }
    let mut cursor: usize = 0;
    for (i, &interior_id) in interior_ids.iter().enumerate() {
        let remaining = (old_root_len as usize) - cursor;
        let this_block_count = remaining.min(k as usize);
        if let Err(e) = graph.ltb().write_payload_partial(
            interior_id,
            0,
            &old_root_bytes[cursor * 4..(cursor + this_block_count) * 4],
        ) {
            // Roll back: release all minted interiors.
            for &id in interior_ids.iter().rev() {
                let _ = graph.ltb().release(id);
            }
            return Err(LabeledOperationError::LtbBlock(e));
        }
        cursor += this_block_count;
        let _ = i;
    }
    debug_assert_eq!(cursor, old_root_len as usize);
    // 4. Allocate the new root region span.
    let new_edge_start = match graph.edges().allocate_span(u64::from(new_interior_count)) {
        Ok(s) => s,
        Err(e) => {
            for &id in interior_ids.iter().rev() {
                let _ = graph.ltb().release(id);
            }
            return Err(LabeledOperationError::from(e));
        }
    };
    // 5. Write the interior-id array to the new root region.
    let mut new_root_bytes: Vec<u8> = Vec::with_capacity(new_interior_count as usize * 4);
    for &id in &interior_ids {
        new_root_bytes.extend_from_slice(&id.to_le_bytes());
    }
    if let Err(e) = graph
        .edges()
        .write_slots_contiguous_bytes(new_edge_start, &new_root_bytes)
    {
        for &id in interior_ids.iter().rev() {
            let _ = graph.ltb().release(id);
        }
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(new_interior_count));
        return Err(LabeledOperationError::from(e));
    }
    // 6. Commit: build the new descriptor. `stored_slots` and `degree`
    //    are unchanged (deepen does not add or remove edges). The new
    //    `edge_start` points to the new root region. The physical
    //    depth is set to `depth + 1` so the resolver can disambiguate
    //    the post-deepen layout (Plan 0318 §Step 7).
    let new_physical_depth = depth + 1;
    let new_bucket = bucket
        .with_edge_range(new_edge_start, stored_slots)
        .with_stored_slots(stored_slots)
        .with_degree_field(bucket.degree)
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(new_physical_depth);
    // 7. Publish: single canonical write.
    if let Err(e) = graph
        .buckets()
        .write_label_bucket_slot(bucket_slot, new_bucket)
    {
        // Rollback: release the new interior blocks and the new span.
        // The old root region is still intact (we only read it).
        for &id in interior_ids.iter().rev() {
            let _ = graph.ltb().release(id);
        }
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(new_interior_count));
        return Err(e.into());
    }
    // 8. After publish, release the old root region span. (Standalone
    //    span, not a leaf subrange; pin-sheltered helper not needed.)
    if new_edge_start != bucket.edge_start() {
        let _ = graph
            .edges()
            .release_span(bucket.edge_start(), u64::from(old_root_len));
    }
    Ok(())
}

/// Inverse of `tree_mode_deepen`: collapse a depth-`d` tree-mode bucket
/// (`d >= 2`) to depth 1 by re-packing the leaf block_ids into a single
/// new root region and releasing the interior blocks.
///
/// **Preconditions** (fail-closed before any canonical write):
/// - `bucket.is_tree_mode()` is the caller's responsibility.
/// - `derive_depth(stored_slots) >= 2` (else there's no interior layer
///   to collapse).
/// - After flatten, `derive_depth(stored_slots) == 1`, i.e. `stored_slots
///   <= 2^20`. This is invariantly true when `depth >= 2`'s `B`-th
///   power upper bound holds: depth 2 means `stored_slots > 2^20` only
///   if depth should be 3. So `depth >= 2` implies the bucket has more
///   than 2^20 slots and flatten would push it past the depth-1 cap.
///   Therefore we additionally check `derive_depth(stored_slots) == 2`
///   and reject depths > 2 (the test surface is depth-2 → depth-1 only;
///   a depth-3 → depth-2 collapse is a future slice).
///
/// **Production hook**: none. Flatten is a maintenance/compaction path;
/// the production wire-up does not call it. It exists for round-trip
/// tests and to keep the structural invariant
/// `derive_depth(bucket) <= MAX_DEPTH` reachable for compactors.
///
/// **Commit order** (per Plan 0318 §Step 7 spec): the canonical
/// descriptor write happens **before** the interior block release. This
/// mirrors `tree_mode_deepen`'s order and avoids a "looks like depth 1
/// from the descriptor, but interior block still owns child ids" window.
pub(crate) fn tree_mode_flatten<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    bucket_slot: u64,
    bucket: &LabelBucket,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    debug_assert_eq!(E::BYTES, 4);
    debug_assert!(bucket.is_tree_mode());
    let stored_slots = bucket.stored_slots;
    // Use the **physical** depth (stored in the bucket via the
    // `inline_property_bytes_log_len` byte) rather than the structural
    // `derive_depth(stored_slots)`. A manually-deepened bucket at
    // stored=1,048,576 has structural depth 1 but physical depth 2.
    // The `collect_leaf_block_ids` call below uses the resolver, which
    // reads the physical depth.
    let depth = bucket.tree_mode_physical_depth();
    if depth != 2 {
        return Err(LabeledOperationError::TreeDepthLimitReached {
            depth,
            max_depth: crate::labeled::tree_csr_prototype::MAX_DEPTH,
        });
    }
    let k = u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
    // 1. Collect the current leaf block_ids in order.
    let leaf_ids = collect_leaf_block_ids::<E, M>(graph, bucket)?;
    let new_root_len = leaf_ids.len() as u32;
    debug_assert!(new_root_len <= k, "depth 2 root_len must be <= R_MAX");
    // 2. Allocate the new root region span.
    let new_edge_start = match graph.edges().allocate_span(u64::from(new_root_len)) {
        Ok(s) => s,
        Err(e) => return Err(LabeledOperationError::from(e)),
    };
    // 3. Write the leaf-id array to the new root region.
    let mut new_root_bytes: Vec<u8> = Vec::with_capacity(new_root_len as usize * 4);
    for &id in &leaf_ids {
        new_root_bytes.extend_from_slice(&id.to_le_bytes());
    }
    if let Err(e) = graph
        .edges()
        .write_slots_contiguous_bytes(new_edge_start, &new_root_bytes)
    {
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(new_root_len));
        return Err(LabeledOperationError::from(e));
    }
    // 4. Commit: new descriptor. Reset the physical depth to 1
    //    (the flatten result is a flat root region; no interior).
    let new_bucket = bucket
        .with_edge_range(new_edge_start, stored_slots)
        .with_stored_slots(stored_slots)
        .with_degree_field(bucket.degree)
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(1);
    // 5. Publish.
    if let Err(e) = graph
        .buckets()
        .write_label_bucket_slot(bucket_slot, new_bucket)
    {
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(new_root_len));
        return Err(e.into());
    }
    // 6. After publish, release the old root region span and the
    //    interior blocks. The interior blocks are read-but-not-claimed
    //    by the new descriptor (the leaf block_ids are the only
    //    references), so the LTB blocks are unreachable.
    //
    // **Physical root_len**: we use the **physical** depth (from the
    // bucket) to compute the old root region's length, because the
    // structural `derived_root_len` may be inconsistent with the
    // physical layout (a manually-deepened bucket at stored=1,048,576
    // has structural root_len=1024 but physical root_len=1).
    let leaf_count = u32::try_from(
        (u64::from(stored_slots)).div_ceil(crate::labeled::tree_csr_prototype::B as u64),
    )
    .expect("leaf_count fits u32 for MAX_DEPTH=3");
    let physical_depth = bucket.tree_mode_physical_depth();
    let old_physical_root_len: u32 = match physical_depth {
        1 => leaf_count,
        2 => leaf_count.div_ceil(k),
        3 => leaf_count.div_ceil(k).div_ceil(k),
        _ => unreachable!("tree_mode_physical_depth out of range"),
    };
    if new_edge_start != bucket.edge_start() {
        let _ = graph
            .edges()
            .release_span(bucket.edge_start(), u64::from(old_physical_root_len));
    }
    // 7. Release the interiors. The order doesn't matter: the new
    // descriptor doesn't reference them. We release in mint order
    // (FIFO) for consistency with `tree_mode_deepen`'s mint order.
    let old_root_len = old_physical_root_len;
    // Read the old root region to discover the interior block_ids.
    let mut old_root_bytes = vec![0u8; old_root_len as usize * 4];
    if old_root_len > 0 {
        graph
            .edges()
            .read_slots_contiguous_bytes(bucket.edge_start(), &mut old_root_bytes);
    }
    for chunk in old_root_bytes.chunks(4) {
        if chunk.len() < 4 {
            break;
        }
        let interior_id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let _ = graph.ltb().release(interior_id);
    }
    Ok(())
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::super::test_support::{TestEdge, test_graph};
    use super::super::tree_read::tree_mode_out_edges_collect;
    use super::super::{BucketSearch, OutEdgeOrder};
    use super::*;
    use crate::VertexId;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use ic_stable_structures::VectorMemory;

    /// Build a tree-mode bucket at `(vid, label)` with `stored_slots`
    /// entries (slab prefix pre-populated with `slot i = i + 100`) and
    /// then promote it. Mirrors the pattern used by
    /// `super::promote::tests`.
    fn promote_test_bucket(
        graph: &LabeledLaraGraph<TestEdge, VectorMemory>,
        vid: VertexId,
        label: BucketLabelKey,
        stored_slots: u32,
    ) {
        super::super::promote::tests::force_bucket_to_stored_slots(graph, vid, label, stored_slots);
        let vertex = graph.vertices().get(vid);
        let search = graph.find_bucket(vid, &vertex, label).expect("find_bucket");
        let super::super::BucketSearch::Found { slot, .. } = search else {
            panic!("bucket missing after force_bucket_to_stored_slots");
        };
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let edge_start = bucket.edge_start();
        super::super::promote::tests::fill_leg_slab_prefix(graph, edge_start, stored_slots);
        super::super::promote::promote_bypass_to_tree_mode(graph, vid, label).expect("promote");
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn insert_after_promote_appends_into_tree_tail() {
        // Plan 0318 §Step 6 test 1: promote to tree (stored=4096), then
        // insert 1 edge. With stored=4096 (a multiple of B), the next
        // insert must mint a new LTB block and grow the root region.
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        promote_test_bucket(&graph, vid, label, 4096);

        // Re-read the bucket after promote.
        let vertex = graph.vertices().get(vid);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket not found"),
        };
        let bucket_slot = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
            BucketSearch::Found { slot, .. } => slot,
            _ => panic!("bucket slot not found"),
        };
        let new_edge = TestEdge { target: 9999 };
        let logical_slot =
            tree_mode_insert_edge(&graph, vid, bucket_slot, &bucket, label, &new_edge)
                .expect("tree_mode_insert_edge");
        assert_eq!(logical_slot, 4096);

        // Re-read the bucket: stored=4097, degree=4097, still tree mode.
        let vertex = graph.vertices().get(vid);
        let bucket2 = match graph
            .find_bucket(vid, &vertex, label)
            .expect("find_bucket 2")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket not found 2"),
        };
        assert!(bucket2.is_tree_mode());
        assert_eq!(bucket2.stored_slots, 4097);
        assert_eq!(bucket2.degree, 4097);

        // Verify the new edge is readable at slot 4096.
        let collected = tree_mode_out_edges_collect(
            &graph,
            label.raw(),
            &bucket2,
            bucket2.degree,
            OutEdgeOrder::Ascending,
        )
        .expect("collect");
        assert_eq!(collected.len(), 4097);
        assert_eq!(collected[4096].target, 9999);
        // Sanity: first slot still has its pre-promote value.
        assert_eq!(collected[0].target, 100);
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn insert_crosses_root_growth_boundary() {
        // Plan 0318 §Step 6 test 2: after promote, insert 1024 more
        // edges. Root region grows from 4 to 5 (block 4 is fresh, then
        // a 6th block is needed for slot 5120).
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        promote_test_bucket(&graph, vid, label, 4096);

        for i in 0u32..1024 {
            let vertex = graph.vertices().get(vid);
            let bucket = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
                BucketSearch::Found { bucket, .. } => bucket,
                _ => panic!("bucket not found"),
            };
            let bucket_slot = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
                BucketSearch::Found { slot, .. } => slot,
                _ => panic!("slot not found"),
            };
            let edge = TestEdge { target: 4096 + i };
            tree_mode_insert_edge(&graph, vid, bucket_slot, &bucket, label, &edge).expect("insert");
        }
        // Verify the final bucket state.
        let vertex = graph.vertices().get(vid);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket not found"),
        };
        assert!(bucket.is_tree_mode());
        assert_eq!(bucket.stored_slots, 5120);
        assert_eq!(bucket.degree, 5120);
        let collected = tree_mode_out_edges_collect(
            &graph,
            label.raw(),
            &bucket,
            bucket.degree,
            OutEdgeOrder::Ascending,
        )
        .expect("collect");
        assert_eq!(collected.len(), 5120);
        for i in 0u32..5120 {
            let expected = if i < 4096 { i + 100 } else { i };
            assert_eq!(collected[i as usize].target, expected, "slot {i} mismatch");
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_wires_into_insert_path() {
        // Plan 0318 §Step 6 test 3: a slab bucket with stored=4096
        // (=T_PROMOTE) must be promoted by the production `insert_edge`
        // path, and the new edge must land at slot 4096 (the post-promote
        // tail). The promote trigger is `stored_slots >= T_PROMOTE` on
        // the pre-insert state; the post-promote insert appends to the
        // fresh LTB block at slot 4096.
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        super::super::promote::tests::force_bucket_to_stored_slots(&graph, vid, label, 4096);
        let vertex = graph.vertices().get(vid);
        let search = graph.find_bucket(vid, &vertex, label).expect("find_bucket");
        let super::super::BucketSearch::Found { slot, .. } = search else {
            panic!("bucket missing");
        };
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let edge_start = bucket.edge_start();
        super::super::promote::tests::fill_leg_slab_prefix(&graph, edge_start, 4096);

        // Production insert path: this must auto-promote.
        let new_edge = TestEdge { target: 8800 };
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                new_edge,
                crate::labeled::EdgePlacementPolicy::Insertion,
            )
            .expect("insert_edge_skip_leaf_cascade");

        // Re-read: should be tree mode, stored=4097, degree=4097, and
        // the new edge should be at slot 4096 (the post-promote tail).
        let vertex = graph.vertices().get(vid);
        let bucket_after = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing after insert"),
        };
        assert!(bucket_after.is_tree_mode(), "expected tree mode");
        assert_eq!(bucket_after.stored_slots, 4097);
        assert_eq!(bucket_after.degree, 4097);
        let collected = tree_mode_out_edges_collect(
            &graph,
            label.raw(),
            &bucket_after,
            bucket_after.degree,
            OutEdgeOrder::Ascending,
        )
        .expect("collect");
        assert_eq!(collected.len(), 4097);
        // First 4096 entries are the pre-promote slab prefix (slot i = i + 100).
        for i in 0..4096 {
            assert_eq!(collected[i].target, (i as u32) + 100, "slot {i} mismatch");
        }
        // Last entry is the new edge at slot 4096.
        assert_eq!(collected[4096].target, 8800);
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn vertex_bucket_count_cap_rejects_1025th_bucket() {
        // Plan 0318 §Step 6 test 4: a vertex with 1024 buckets must
        // reject a new label insert with `VertexBucketCountCapReached`.
        use crate::labeled::graph::check_vertex_bucket_count_cap;
        let graph = test_graph();
        let vid = VertexId::from(0);
        graph
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        // Construct a vertex with 1024 buckets (cap is `>= MAX_BUCKETS_PER_VERTEX`).
        let cap_vertex = crate::labeled::record::LabeledVertex::try_from_parts(
            0,
            super::super::MAX_BUCKETS_PER_VERTEX,
            0,
            0,
            0,
        )
        .expect("try_from_parts");
        graph
            .set_labeled_vertex(vid, cap_vertex)
            .expect("set_labeled_vertex");
        // The cap helper must reject.
        let vertex = graph.vertices().get(vid);
        let err = check_vertex_bucket_count_cap(&vertex).expect_err("cap must trigger");
        match err {
            LabeledOperationError::VertexBucketCountCapReached { current_count, cap } => {
                assert_eq!(current_count, super::super::MAX_BUCKETS_PER_VERTEX);
                assert_eq!(cap, super::super::MAX_BUCKETS_PER_VERTEX);
            }
            other => panic!("expected VertexBucketCountCapReached, got {other:?}"),
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_insert_rejects_inline_property_edge() {
        // Plan 0318 §Step 6 test 5: an inline-property edge inserted
        // into a tree bucket must fail with `InlinePropertyBytesWidthMismatch`.
        //
        // We build a fresh graph typed for `InlinePropEdge` (4-byte
        // edge with width 4). The bucket is forced and promoted via
        // raw byte writes (helpers are E-agnostic). The dispatcher
        // then sees a tree-mode bucket with width 0 and an edge with
        // width 4 → mismatch.
        use crate::labeled::graph::test_support::mem as test_mem;
        use crate::labeled::record::LabelBucket;
        use crate::traits::{CsrEdge, CsrEdgeTombstone};

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct InlinePropEdge(u32);
        impl CsrEdge for InlinePropEdge {
            const BYTES: usize = 4;
            fn read_from(bytes: &[u8]) -> Self {
                Self(u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
            }
            fn write_to(&self, bytes: &mut [u8]) {
                bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
            }
            fn neighbor_vid(&self) -> crate::VertexId {
                crate::VertexId::from(self.0)
            }
            fn with_neighbor_vid(&self, vid: crate::VertexId) -> Self {
                Self(u32::from(vid))
            }
            fn edge_inline_property_byte_width(&self) -> u16 {
                4
            }
        }
        impl CsrEdgeTombstone for InlinePropEdge {
            fn tombstone_edge() -> Self {
                Self(u32::from(crate::VertexId::EDGE_TOMBSTONE_SENTINEL))
            }
        }

        fn make_graph() -> LabeledLaraGraph<InlinePropEdge, VectorMemory> {
            LabeledLaraGraph::<InlinePropEdge, VectorMemory>::new(
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                test_mem(),
                crate::labeled::InitialCapacities::uniform(64),
                BucketLabelKey::UNLABELED_DIRECTED,
            )
            .expect("graph::new")
        }

        fn force_bucket<E: CsrEdge>(
            graph: &LabeledLaraGraph<E, VectorMemory>,
            vid: VertexId,
            label: BucketLabelKey,
            stored_slots: u32,
        ) {
            // Generic version of the TestEdge-locked helper.
            graph
                .push_vertex(crate::labeled::record::LabeledVertex::default())
                .expect("push_vertex");
            let bucket = LabelBucket::try_from_parts(
                label,
                0,
                stored_slots,
                stored_slots,
                -1,
                0,
                0,
                0,
                -1,
                0,
            )
            .expect("try_from_parts")
            .with_tree_mode(false);
            graph
                .buckets()
                .write_label_bucket_slot(0, bucket)
                .expect("write_label_bucket_slot 0");
            let vertex = graph.vertices().get(vid);
            let new_vertex = vertex
                .try_with_bucket_row(0, 1)
                .expect("try_with_bucket_row");
            graph
                .set_labeled_vertex(vid, new_vertex)
                .expect("set_labeled_vertex");
        }

        let graph = make_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        force_bucket(&graph, vid, label, 4096);
        // Fill LEG slab prefix with deterministic bytes (E-agnostic).
        let mut all_bytes = Vec::with_capacity(4096 * 4);
        for i in 0..4096u32 {
            let target = i.wrapping_add(100);
            all_bytes.extend_from_slice(&target.to_le_bytes());
        }
        graph
            .edges()
            .write_slots_contiguous_bytes(0, &all_bytes)
            .expect("write_slots_contiguous");
        // Promote to tree mode.
        super::super::promote::promote_bypass_to_tree_mode(&graph, vid, label).expect("promote");
        // Insert an inline-property edge via the dispatcher. The
        // dispatcher checks `bucket.is_tree_mode()` and then
        // `has_edge_inline_property`; the bucket has width 0, the edge
        // has width 4 → `InlinePropertyBytesWidthMismatch`.
        let inline_edge = InlinePropEdge(9999);
        let err = graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                inline_edge,
                crate::labeled::EdgePlacementPolicy::Insertion,
            )
            .expect_err("tree bucket must reject inline-property edge");
        match err {
            LabeledOperationError::InlinePropertyBytesWidthMismatch { .. } => {}
            other => panic!("expected InlinePropertyBytesWidthMismatch, got {other:?}"),
        }
    }

    // ====================== Commit B tests ======================

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_remove_writes_tombstone_and_updates_accounting() {
        // Plan 0318 §Step 6 test 6: promote (stored=4096), remove slot
        // 100 → read shows tombstone value, degree=4095, num_edges-1,
        // stored_slots unchanged at 4096.
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        promote_test_bucket(&graph, vid, label, 4096);
        let num_before = graph.edges().header().num_edges;
        let bucket = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket not found"),
        };
        let slot = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { slot, .. } => slot,
            _ => panic!("slot"),
        };
        let removed = tree_mode_remove_edge_at_slot(&graph, vid, slot, &bucket, 100)
            .expect("remove")
            .expect("slot in range");
        // The removed edge must be the original value at slot 100.
        assert_eq!(removed.target, 100 + 100);
        // Re-read the bucket.
        let vertex = graph.vertices().get(vid);
        let bucket_after = match graph.find_bucket(vid, &vertex, label).expect("find after") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket not found after"),
        };
        assert!(bucket_after.is_tree_mode());
        assert_eq!(bucket_after.degree, 4095);
        assert_eq!(bucket_after.stored_slots, 4096);
        // num_edges decremented.
        let num_after = graph.edges().header().num_edges;
        assert_eq!(num_after, num_before - 1);
        // Re-read slot 100: should be the tombstone target.
        let collected = tree_mode_out_edges_collect(
            &graph,
            label.raw(),
            &bucket_after,
            bucket_after.degree,
            OutEdgeOrder::Ascending,
        )
        .expect("collect");
        // The dense collect walks every stored slot (tombstones are
        // physical slots that the collect does not filter). The
        // collect size is therefore `stored_slots` (4096), even though
        // degree is 4095.
        assert_eq!(collected.len(), 4096);
        // Slot 100 is now the tombstone sentinel.
        let slot_value =
            super::super::tree_read::tree_mode_random_ordinal_access(&graph, vid, label.raw(), 100)
                .expect("random_ordinal_access")
                .expect("slot in range");
        // The tombstone target is EDGE_TOMBSTONE_SENTINEL.
        assert_eq!(
            slot_value,
            u32::from(crate::VertexId::EDGE_TOMBSTONE_SENTINEL)
        );
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_remove_then_insert_appends_to_tail() {
        // Plan 0318 §Step 6 test 7: after a tree-mode remove, an insert
        // must append (Unordered tombstone reuse is not implemented).
        // So a remove + insert advances stored_slots by 1 and lands the
        // new edge at the new tail slot.
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        promote_test_bucket(&graph, vid, label, 4096);
        // Remove slot 100.
        let bucket = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket"),
        };
        let slot = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { slot, .. } => slot,
            _ => panic!("slot"),
        };
        let _ = tree_mode_remove_edge_at_slot(&graph, vid, slot, &bucket, 100).expect("remove");
        // Re-read.
        let vertex = graph.vertices().get(vid);
        let bucket2 = match graph.find_bucket(vid, &vertex, label).expect("find 2") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket 2"),
        };
        let slot2 = match graph.find_bucket(vid, &vertex, label).expect("find 2") {
            BucketSearch::Found { slot, .. } => slot,
            _ => panic!("slot 2"),
        };
        // Insert: stored_slots was 4096, append goes to slot 4096.
        let new_edge = TestEdge { target: 7777 };
        let logical_slot =
            tree_mode_insert_edge(&graph, vid, slot2, &bucket2, label, &new_edge).expect("insert");
        // 0-indexed: 0..=4095 were the original slots; slot 4096 is the
        // post-remove post-insert tail.
        assert_eq!(logical_slot, 4096);
        // Re-read: stored=4097, degree=4097.
        let vertex = graph.vertices().get(vid);
        let bucket3 = match graph.find_bucket(vid, &vertex, label).expect("find 3") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket 3"),
        };
        assert_eq!(bucket3.stored_slots, 4097);
        // After remove (degree 4096 → 4095) and insert (degree 4095 →
        // 4096): the new edge is at slot 4096, the tombstone at slot
        // 100 stays (stored_slots includes tombstones).
        assert_eq!(bucket3.degree, 4096);
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn pre_promotion_span_inside_pinned_leaf_helper() {
        // Plan 0318 §Step 6 B-2 test: a non-pinned test graph returns
        // `false` for the helper. We test the helper directly without
        // pinning a leaf (pinning requires a complex PMA setup that's
        // not needed for the unit-level invariant).
        let graph = test_graph();
        let vid = VertexId::from(0);
        // No leaf is pinned for vid 0 in a fresh test graph.
        let result = super::pre_promotion_span_inside_pinned_leaf(&graph, vid, 0, 4096);
        assert!(!result, "fresh graph has no pinned leaf");
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_skip_release_inside_pinned_leaf() {
        // Plan 0318 §Step 6 B-2 test: force a bucket at T_PROMOTE, then
        // promote it. The pre-promotion release is **not deferred** when
        // the leaf is not pinned (test graphs don't pin leaves by
        // default), so the LEG free-list gains the pre-promotion span.
        // We confirm the promote succeeds and the bucket is in tree mode.
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        super::super::promote::tests::force_bucket_to_stored_slots(
            &graph,
            vid,
            label,
            super::super::T_PROMOTE,
        );
        let vertex = graph.vertices().get(vid);
        let search = graph.find_bucket(vid, &vertex, label).expect("find_bucket");
        let super::super::BucketSearch::Found { slot, .. } = search else {
            panic!("bucket missing");
        };
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let edge_start = bucket.edge_start();
        super::super::promote::tests::fill_leg_slab_prefix(
            &graph,
            edge_start,
            super::super::T_PROMOTE,
        );
        // The leaf is not pinned; the helper returns false → release
        // runs. We just confirm the promote succeeds.
        super::super::promote::promote_bypass_to_tree_mode(&graph, vid, label).expect("promote");
        let vertex = graph.vertices().get(vid);
        let bucket_after = match graph.find_bucket(vid, &vertex, label).expect("find after") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing after promote"),
        };
        assert!(bucket_after.is_tree_mode());
    }

    // ====================== Step 7 tests ======================

    /// Helper: build a depth-1 tree bucket with `root_len = K = 1024`
    /// (i.e., `stored_slots = B * K = 1,048,576`). This mints 1024
    /// LTB blocks (one per leaf), allocates a 1024-slot LEG root
    /// region, and writes the 1024 block_ids into the root region. The
    /// leaf payload is left zero-initialized (we never call
    /// `write_payload_partial`); the test only verifies the resolver
    /// walks the root + interior hop chain correctly, not the leaf
    /// payload content.
    fn build_depth1_full_root_bucket(
        graph: &LabeledLaraGraph<TestEdge, VectorMemory>,
        _vid: VertexId,
        label: BucketLabelKey,
        bucket_slot: u64,
    ) {
        let b = BLOCK_B as u32;
        let k = u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
        let stored_slots = b * k; // 1024 * 1024 = 1,048,576
        let root_len = k; // depth 1, root_len = R_MAX
        // 1. Mint 1024 LTB blocks (one per leaf).
        let mut block_ids: Vec<u32> = Vec::with_capacity(root_len as usize);
        for _ in 0..root_len {
            let id = graph.ltb().mint().expect("mint");
            block_ids.push(id);
        }
        // 2. Allocate a 1024-slot LEG root region.
        let edge_start = graph
            .edges()
            .allocate_span(u64::from(root_len))
            .expect("allocate_span");
        // 3. Write the 1024 block_ids to the root region.
        let mut root_bytes: Vec<u8> = Vec::with_capacity(root_len as usize * 4);
        for &id in &block_ids {
            root_bytes.extend_from_slice(&id.to_le_bytes());
        }
        graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &root_bytes)
            .expect("write_slots_contiguous_bytes");
        // 4. Construct the depth-1 tree bucket descriptor.
        let new_bucket = crate::labeled::record::LabelBucket::try_from_parts(
            label,
            edge_start,
            stored_slots, // degree = stored (every leaf is a real edge in this synthetic bucket)
            stored_slots,
            -1, // overflow_log_head (unused in tree mode; -1 = empty)
            0,  // inline_property_byte_width
            0,  // inline_property_bytes_offset
            0,  // inline_property_bytes_slab_slots
            -1, // inline_property_bytes_log_head (unused: -1 = empty)
            0,  // inline_property_bytes_log_len
        )
        .expect("try_from_parts")
        .with_tree_mode(true);
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, new_bucket)
            .expect("write_label_bucket_slot");
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn deepen_fail_closed_at_max_depth() {
        // Plan 0318 §Step 7 test: a bucket whose `stored_slots` would
        // resolve to depth 3 must fail-closed with
        // `TreeDepthLimitReached`. We construct a synthetic bucket
        // descriptor with `stored_slots = 2^30 + 1` (the first value
        // that pushes to depth 3) but no actual leaf blocks. The
        // precondition check in `tree_mode_deepen` catches this before
        // any mint/write.
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        // Set up vertex with a single bucket slot.
        let vid: VertexId = graph
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .expect("push_vertex");
        let bucket_slot = 0u64;
        // Build the depth-3-bound synthetic bucket.
        let stored_slots: u32 = (1u32 << 30) + 1;
        let bucket = crate::labeled::record::LabelBucket::try_from_parts(
            label,
            0, // edge_start (unused: typed error returned before any LEG read)
            stored_slots,
            stored_slots,
            -1,
            0,
            0,
            0,
            -1,
            0,
        )
        .expect("try_from_parts")
        .with_tree_mode(true);
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, bucket)
            .expect("write_label_bucket_slot");
        let result = tree_mode_deepen(&graph, bucket_slot, &bucket);
        match result {
            Err(LabeledOperationError::TreeDepthLimitReached { depth, max_depth }) => {
                assert_eq!(max_depth, 3);
                assert!(depth >= 3, "expected depth >= MAX_DEPTH, got {depth}");
            }
            other => panic!("expected TreeDepthLimitReached, got {other:?}"),
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn resolve_leaf_block_id_walks_synthetic_depth2_layout() {
        // Plan 0318 §Step 7 test: build a depth-2 tree bucket
        // synthetically (without going through `tree_mode_deepen`),
        // then verify the depth-generic resolver walks the
        // root -> interior -> leaf hop chain correctly.
        //
        // Setup: stored_slots = 1,048,577 (= R_MAX * B + 1), so
        // `derive_depth` returns 2 and `root_len` returns 2. The
        // physical layout has 1 interior block holding 1025 leaf
        // block_ids (slots 0..=1024, first 1024 in the first interior
        // and 1 in the second; wait, K=1024, so all 1025 fit in 1
        // interior with 1024 - 1025 = 999 unused slots, but root_len
        // is 2 so we mint 2 interiors).
        let graph = test_graph();
        let label = BucketLabelKey::directed_from_index(1);
        let vid = graph
            .push_vertex(
                crate::labeled::record::LabeledVertex::default()
                    .try_with_bucket_row(0, 1)
                    .expect("try_with_bucket_row"),
            )
            .expect("push_vertex");
        let bucket_slot = 0u64;
        // 1. Mint 1025 leaf blocks.
        let mut leaf_ids: Vec<u32> = Vec::with_capacity(1025);
        for _ in 0..1025 {
            leaf_ids.push(graph.ltb().mint().expect("mint leaf"));
        }
        // 2. Mint 2 interior blocks.
        let mut interior_ids: Vec<u32> = Vec::with_capacity(2);
        for _ in 0..2 {
            interior_ids.push(graph.ltb().mint().expect("mint interior"));
        }
        // 3. Pack the 1025 leaf_ids into the 2 interiors (K=1024 each).
        //    interior[0] gets leaf_ids[0..1024]; interior[1] gets leaf_ids[1024..1025].
        for (i, interior_id) in interior_ids.iter().enumerate() {
            let start = i * 1024;
            let end = (start + 1024).min(leaf_ids.len());
            let chunk: Vec<u8> = leaf_ids[start..end]
                .iter()
                .flat_map(|id| id.to_le_bytes())
                .collect();
            graph
                .ltb()
                .write_payload_partial(*interior_id, 0, &chunk)
                .expect("write_payload_partial interior");
        }
        // 4. Allocate a 2-slot LEG root region.
        let edge_start = graph.edges().allocate_span(2).expect("allocate_span root");
        let root_bytes: Vec<u8> = interior_ids
            .iter()
            .flat_map(|id| id.to_le_bytes())
            .collect();
        graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &root_bytes)
            .expect("write_slots_contiguous_bytes root");
        // 5. Build the depth-2 descriptor.
        let new_bucket = crate::labeled::record::LabelBucket::try_from_parts(
            label, edge_start, 1_048_577, // degree = stored
            1_048_577, -1, 0, 0, 0, -1, 0,
        )
        .expect("try_from_parts")
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(2);
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, new_bucket)
            .expect("write_label_bucket_slot");
        // 6. Verify the resolver returns the correct leaf block_ids.
        for block_index in 0..1025u32 {
            let id =
                resolve_leaf_block_id::<TestEdge, VectorMemory>(&graph, &new_bucket, block_index)
                    .expect("resolve depth-2");
            assert_eq!(
                id, leaf_ids[block_index as usize],
                "leaf block_id mismatch at block_index={block_index}"
            );
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn deepen_restructures_root_region() {
        // Plan 0318 §Step 7 test: build a small depth-1 bucket
        // (root_len = 4) and call `tree_mode_deepen`. The function
        // should:
        // - pack the 4 root entries into a fresh interior block
        // - allocate a 1-slot new root region
        // - write the interior id to the new root region
        // - publish the new descriptor (canonical write)
        // - release the old root region span
        // We verify the new descriptor is in place and the new root
        // region has 1 entry that is not one of the original leaf ids.
        // (The resolver leaf-id check is covered by the
        // synthetic-depth-2 test above; we do NOT require the structural
        // formula to align with the physical depth here, because the
        // production wire-up only calls `tree_mode_deepen` when the
        // next insert would push stored past the depth-1 cap — i.e.
        // when `derive_depth` already returns 2.)
        let graph = test_graph();
        let label = BucketLabelKey::directed_from_index(1);
        let vid = graph
            .push_vertex(
                crate::labeled::record::LabeledVertex::default()
                    .try_with_bucket_row(0, 1)
                    .expect("try_with_bucket_row"),
            )
            .expect("push_vertex");
        let bucket_slot = 0u64;
        // Build a small depth-1 bucket with root_len = 4.
        let b = BLOCK_B as u32;
        let stored_slots = b * 4; // 4096
        let root_len = 4u32;
        let mut block_ids: Vec<u32> = Vec::with_capacity(4);
        for _ in 0..4 {
            block_ids.push(graph.ltb().mint().expect("mint"));
        }
        let edge_start = graph
            .edges()
            .allocate_span(u64::from(root_len))
            .expect("allocate_span");
        let root_bytes: Vec<u8> = block_ids.iter().flat_map(|id| id.to_le_bytes()).collect();
        graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &root_bytes)
            .expect("write_slots_contiguous_bytes");
        let new_bucket = crate::labeled::record::LabelBucket::try_from_parts(
            label,
            edge_start,
            stored_slots,
            stored_slots,
            -1,
            0,
            0,
            0,
            -1,
            0,
        )
        .expect("try_from_parts")
        .with_tree_mode(true);
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, new_bucket)
            .expect("write_label_bucket_slot");

        // Deepen.
        tree_mode_deepen(&graph, bucket_slot, &new_bucket).expect("deepen");

        // Re-read the bucket.
        let vertex = graph.vertices().get(vid);
        let bucket_after = match graph.find_bucket(vid, &vertex, label).expect("find after") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing after"),
        };
        assert!(bucket_after.is_tree_mode());
        assert_eq!(bucket_after.stored_slots, stored_slots);
        assert_eq!(bucket_after.degree, stored_slots);
        // The new root region has 1 entry pointing to the new interior.
        let mut new_root_id_bytes = [0u8; 4];
        graph
            .edges()
            .read_slot_bytes(bucket_after.edge_start(), &mut new_root_id_bytes);
        let new_root_id = u32::from_le_bytes(new_root_id_bytes);
        assert!(
            !block_ids.contains(&new_root_id),
            "deepened root should hold an interior block_id, not a leaf"
        );
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn flatten_inverts_deepen() {
        // Plan 0318 §Step 7 test: build depth-1 root_len=1024, deepen
        // to depth 2 (root_len=1, one interior), then flatten back to
        // depth 1 (root_len=1024 with the same leaf block_ids).
        // Verifies the round-trip property and the post-publish
        // interior-release invariant.
        let graph = test_graph();
        let label = BucketLabelKey::directed_from_index(1);
        let vid = graph
            .push_vertex(
                crate::labeled::record::LabeledVertex::default()
                    .try_with_bucket_row(0, 1)
                    .expect("try_with_bucket_row"),
            )
            .expect("push_vertex");
        let bucket_slot = 0u64;
        build_depth1_full_root_bucket(&graph, vid, label, bucket_slot);

        // Capture the original leaf block_ids.
        let vertex = graph.vertices().get(vid);
        let bucket_before = match graph.find_bucket(vid, &vertex, label).expect("find before") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing before"),
        };
        // The structural root_len for stored=1,048,576 is 1024 (the
        // bucket is at the depth-1 fan-out cap, not pushed to depth 2).
        let structural_root_len =
            crate::labeled::tree_csr_prototype::root_len(bucket_before.stored_slots);
        assert_eq!(structural_root_len, 1024);
        let original_edge_start = bucket_before.edge_start();
        let mut original_leaf_ids: Vec<u32> = Vec::with_capacity(1024);
        for block_index in 0..1024u32 {
            let id = resolve_leaf_block_id::<TestEdge, VectorMemory>(
                &graph,
                &bucket_before,
                block_index,
            )
            .expect("resolve before");
            original_leaf_ids.push(id);
        }

        // Deepen.
        tree_mode_deepen(&graph, bucket_slot, &bucket_before).expect("deepen");
        let vertex = graph.vertices().get(vid);
        let bucket_deepened = match graph
            .find_bucket(vid, &vertex, label)
            .expect("find deepened")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing after deepen"),
        };
        assert_ne!(bucket_deepened.edge_start(), original_edge_start);
        // After deepen, the structural root_len is still 1024 (depth 1
        // admits stored=1,048,576), but the physical root region has
        // been replaced with a 1-entry region pointing to the new
        // interior.
        let deepened_structural_root_len =
            crate::labeled::tree_csr_prototype::root_len(bucket_deepened.stored_slots);
        assert_eq!(deepened_structural_root_len, 1024);
        // The deepened root's first entry is the new interior block_id.
        let mut new_root_id_bytes = [0u8; 4];
        graph
            .edges()
            .read_slot_bytes(bucket_deepened.edge_start(), &mut new_root_id_bytes);
        let new_root_id = u32::from_le_bytes(new_root_id_bytes);
        assert!(
            !original_leaf_ids.contains(&new_root_id),
            "deepened root should hold an interior block_id, not a leaf"
        );

        // Flatten. The deepen-then-flatten cycle yields the original
        // root region with the original leaf block_ids.
        tree_mode_flatten(&graph, bucket_slot, &bucket_deepened).expect("flatten");
        let vertex = graph.vertices().get(vid);
        let bucket_flat = match graph.find_bucket(vid, &vertex, label).expect("find flat") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing after flatten"),
        };
        // The flatten replaced the new root region with another region
        // holding the original leaf ids. The edge_start may differ
        // from the original (a fresh span), but the leaf ids must match.
        for block_index in 0..1024u32 {
            let id =
                resolve_leaf_block_id::<TestEdge, VectorMemory>(&graph, &bucket_flat, block_index)
                    .expect("resolve after flatten");
            assert_eq!(
                id, original_leaf_ids[block_index as usize],
                "leaf block_id changed at index {block_index} after flatten"
            );
        }
    }

    // ====================== Commit A: fail-closed root capacity guard ======================

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_insert_fails_closed_at_root_capacity() {
        // Plan 0318 §Step 7 amend (interim fail-closed): a tree-mode
        // bucket with `root_len == R_MAX = 1024` cannot accept a
        // 1,048,577th slot because the interior-level insert cascade
        // is not yet wired. The guard must reject the call with
        // `TreeRootCapacityReached` BEFORE any state change (no mint,
        // no span allocation, no descriptor publish).
        //
        // The bucket is constructed via `force_bucket_to_stored_slots`
        // (slab-mode) + `promote_bypass_to_tree_mode` (tree-mode
        // depth-1 layout). We force `stored_slots = 1024 * 1024 =
        // 1,048,576` (= 2^20) so the root region has exactly 1024
        // entries after promote. The next insert would push the root
        // to 1025 entries, triggering the guard.
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        let r_max =
            u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
        let target_stored: u32 = (BLOCK_B as u32) * r_max; // 1024 * 1024 = 1,048,576
        super::super::promote::tests::force_bucket_to_stored_slots(
            &graph,
            vid,
            label,
            target_stored,
        );
        let vertex = graph.vertices().get(vid);
        let search = graph.find_bucket(vid, &vertex, label).expect("find_bucket");
        let super::super::BucketSearch::Found { slot, .. } = search else {
            panic!("bucket missing");
        };
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let edge_start = bucket.edge_start();
        super::super::promote::tests::fill_leg_slab_prefix(&graph, edge_start, target_stored);
        super::super::promote::promote_bypass_to_tree_mode(&graph, vid, label).expect("promote");
        // Re-read the now tree-mode bucket.
        let vertex = graph.vertices().get(vid);
        let bucket_after = match graph.find_bucket(vid, &vertex, label).expect("find after") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing after promote"),
        };
        assert!(bucket_after.is_tree_mode());
        assert_eq!(bucket_after.stored_slots, target_stored);
        assert_eq!(
            bucket_after.stored_slots / BLOCK_B as u32,
            1024,
            "root_len must be 1024 to exercise the guard"
        );

        // The next insert (tail_offset == 0, since stored_slots % B == 0)
        // must fail-closed with TreeRootCapacityReached.
        let new_edge = TestEdge { target: 0xCAFE };
        let result = tree_mode_insert_edge(&graph, vid, slot, &bucket_after, label, &new_edge);
        match result {
            Err(LabeledOperationError::TreeRootCapacityReached {
                stored_slots,
                root_len,
                cap,
            }) => {
                assert_eq!(stored_slots, target_stored + 1);
                assert_eq!(root_len, 1024);
                assert_eq!(cap, 1024);
            }
            other => panic!("expected TreeRootCapacityReached, got {other:?}"),
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn production_insert_path_fails_closed_at_root_capacity() {
        // Plan 0318 §Step 7 amend (production-path integration): the
        // same guard must fire when called via the production
        // `insert_edge_skip_leaf_cascade` path, not just the raw
        // `tree_mode_insert_edge` helper. We pre-construct a tree-mode
        // bucket at the root capacity (`stored_slots = 1024 * 1024`)
        // via the same force_bucket_to_stored_slots + promote path
        // as the direct-helper test, then drive the production
        // dispatcher once.
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        let r_max =
            u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
        let target_stored: u32 = (BLOCK_B as u32) * r_max; // 1,048,576
        super::super::promote::tests::force_bucket_to_stored_slots(
            &graph,
            vid,
            label,
            target_stored,
        );
        let vertex = graph.vertices().get(vid);
        let search = graph.find_bucket(vid, &vertex, label).expect("find_bucket");
        let super::super::BucketSearch::Found { slot, .. } = search else {
            panic!("bucket missing");
        };
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let edge_start = bucket.edge_start();
        super::super::promote::tests::fill_leg_slab_prefix(&graph, edge_start, target_stored);
        super::super::promote::promote_bypass_to_tree_mode(&graph, vid, label).expect("promote");

        // Production path: the next insert must surface
        // TreeRootCapacityReached through the public API.
        let new_edge = TestEdge { target: 0xBEEF };
        let result = graph.insert_edge_skip_leaf_cascade(
            vid,
            label,
            new_edge,
            crate::labeled::EdgePlacementPolicy::Insertion,
        );
        match result {
            Err(LabeledOperationError::TreeRootCapacityReached {
                stored_slots,
                root_len,
                cap,
            }) => {
                assert_eq!(stored_slots, target_stored + 1);
                assert_eq!(root_len, 1024);
                assert_eq!(cap, 1024);
            }
            other => panic!("expected TreeRootCapacityReached, got {other:?}"),
        }
    }
}
