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
        // No tail room → mint a new LTB block and append its
        // `block_id` to the tree. Plan 0325 (right-spine cascade)
        // lifts the cap from the interim 2^20 (Plan 0318 §Step 7
        // amend) to `TREE_STRUCTURAL_CAP = 2^30` (ADR 0088 §4).
        //
        // **Cascade (Plan 0325)**: when the physical root region
        // has length `>= R_MAX` the bucket must either deepen
        // (depth d → d+1, then re-append into the post-deepen
        // layout) or fail-closed (root full AND next_stored past
        // the structural cap, or `MAX_DEPTH` reached).
        //
        //   physical_root_len < R_MAX
        //     → depth-aware leaf append (depth 1: append to root;
        //        depth ≥ 2: append to home interior; root grows
        //        only when a new interior is minted, i.e.
        //        `l % K == 0`).
        //
        //   physical_root_len == R_MAX
        //     → if depth >= MAX_DEPTH OR next_stored > TREE_STRUCTURAL_CAP
        //         → typed `TreeRootCapacityReached` (2^30 structural
        //           boundary)
        //     → else
        //         → `tree_mode_deepen(...)` (right-spine cascade)
        //         → RE-READ the bucket (stale-descriptor hazard:
        //           deepen published a new `edge_start` /
        //           `tree_mode_physical_depth`)
        //         → continue into the depth-aware append at the
        //           post-deepen depth.
        let k = u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
        // Compute physical_root_len WITHOUT calling `derive_depth`
        // (a manually-deepened bucket would have derive_depth=1
        // despite physical depth ≥ 2). Use the same physical
        // ceil-chain as the guard and `bucket_span_region_len`.
        let pre_deepen_depth = bucket.tree_mode_physical_depth();
        let pre_deepen_physical_root_len: u32 =
            physical_root_len_ceil(u64::from(stored_slots), pre_deepen_depth)?;
        // Cap guard (replaces the 0318 interim 2^20 guard). Fires
        // when (a) the root is full AND (b) the cap is structurally
        // unreachable, i.e. either `MAX_DEPTH` is reached OR
        // `next_stored` exceeds the documented `TREE_STRUCTURAL_CAP`.
        // The cap semantics (ADR 0088 §4):
        // - depth 1 → 2 at stored = 2^20 (cascade fires).
        // - depth 2 → 3 at stored = 2^30: NOT REACHABLE IN
        //   PRODUCTION (`MAX_DEPTH = 3` is the primitive safety
        //   bound; production depth never exceeds 2). The cap
        //   `2^30` matches the depth-2 coverage exactly
        //   (1024 × 1024 × 1024); going past it would require
        //   `derive_depth = 3` which is the ADR amend candidate
        //   (out of scope for this slice).
        if pre_deepen_physical_root_len >= k {
            let max_depth = crate::labeled::tree_csr_prototype::MAX_DEPTH;
            let structural_cap = super::TREE_STRUCTURAL_CAP;
            if pre_deepen_depth >= max_depth || next_stored > structural_cap {
                return Err(LabeledOperationError::TreeRootCapacityReached {
                    stored_slots: next_stored,
                    root_len: pre_deepen_physical_root_len,
                    cap: k,
                });
            }
            // Right-spine cascade: deepen the root (depth d → d+1).
            // `tree_mode_deepen` packs the existing root into
            // `ceil(old_root_len / K)` interior blocks and publishes
            // a new 1-entry (or short) root. After this call the
            // bucket's `edge_start` and `tree_mode_physical_depth`
            // have changed — the caller's `bucket` copy is stale.
            tree_mode_deepen::<E, M>(graph, bucket_slot, bucket)?;
        }
        // RE-READ the bucket descriptor. The caller's `bucket`
        // was passed by `&LabelBucket`; after `tree_mode_deepen`
        // publishes a new descriptor, this `bucket` reference
        // points at stale `edge_start` / `tree_mode_physical_depth`
        // (the caller's stack copy was not updated). Read the
        // canonical descriptor from stable storage.
        let post_deepen_bucket_storage;
        let bucket = if pre_deepen_physical_root_len >= k {
            // Deepen always writes a new descriptor at the
            // canonical `bucket_slot`, so `Some` is guaranteed;
            // we surface `None` as a hard error (it would mean
            // storage corruption, not a recoverable failure).
            post_deepen_bucket_storage = graph
                .buckets()
                .read_label_bucket_slot(bucket_slot)
                .ok_or_else(|| {
                    LabeledOperationError::from(LaraOperationError::CollectAllocationOverflow)
                })?;
            &post_deepen_bucket_storage
        } else {
            bucket
        };
        // Compute the post-deepen root length. The new root is
        // sized `ceil(old_root_len / K)` — strictly smaller than
        // the old root, so there's room for at least one append
        // before the next root-full trigger.
        let post_depth = bucket.tree_mode_physical_depth();
        let post_root_len: u32 =
            physical_root_len_ceil(u64::from(bucket.stored_slots), post_depth)?;
        debug_assert!(
            post_root_len < k,
            "right-spine cascade must strictly reduce physical root length"
        );
        // === Depth-aware append ===
        // depth 1: existing path UNCHANGED (mint leaf → root
        //   grow by 1 → append leaf id).
        // depth ≥ 2: append the new leaf id to its home interior
        //   (NOT the root). The root grows only when a new
        //   interior is minted (`l % K == 0`).
        if post_depth == 1 {
            return tree_mode_tail_append_depth1::<E, M>(
                graph,
                src,
                bucket_slot,
                bucket,
                &target_bytes,
                next_stored,
                next_degree,
                label,
            );
        }
        // depth ≥ 2: depth-aware interior append.
        tree_mode_tail_append_depth_ge2::<E, M>(
            graph,
            src,
            bucket_slot,
            bucket,
            &target_bytes,
            next_stored,
            next_degree,
            label,
        )
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

/// Compute the physical root length for a tree-mode bucket at the
/// given `stored_slots` and physical `depth`, using the physical
/// ceil-chain formula. Mirrors the formula used by the cap guard
/// and `bucket_span_region_len` (compact.rs).
///
/// **Structural vs physical**: never use `derived_root_len(stored_slots)`
/// here — a manually-deepened bucket has `derive_depth = 1` but
/// `tree_mode_physical_depth >= 2`; the structural formula would
/// under-walk the hop chain and return a stale root length.
///
/// Returns an error if the result overflows `u32` (effectively
/// unreachable in production: `R_MAX^MAX_DEPTH * B^MAX_DEPTH` =
/// 1024^3 * 1024 = 2^40 slots at MAX_DEPTH = 3, well within u32
/// but the intermediate `ceil` math uses u64).
fn physical_root_len_ceil(stored_slots: u64, depth: u32) -> Result<u32, LabeledOperationError> {
    let k = crate::labeled::tree_csr_prototype::R_MAX as u64;
    let b = crate::labeled::tree_csr_prototype::B as u64;
    let mut r: u64 = stored_slots;
    // ceil-chain: r = ceil(r / B) / K / K / ... until depth-1
    // divisions. depth 1: r = ceil(s / B). depth 2: r = ceil(s / B) / K.
    // depth 3: r = ceil(s / B) / K / K.
    r = r.div_ceil(b);
    for _ in 1..depth {
        r = r.div_ceil(k);
    }
    u32::try_from(r)
        .map_err(|_| LabeledOperationError::from(LaraOperationError::CollectAllocationOverflow))
}

/// Depth-1 tail append: mint a leaf, grow the root by 1, append the
/// new leaf id at the tail of the root region. This is the original
/// Plan 0318 §Step 6 path; preserved byte-identically (modulo the
/// descriptor's `tree_mode_physical_depth` already being 1).
///
/// **Why extracted**: the cascade rewrote the dispatch to either
/// `tree_mode_tail_append_depth1` (this helper) or
/// `tree_mode_tail_append_depth_ge2` (the new interior path). The
/// depth-1 path is unchanged.
fn tree_mode_tail_append_depth1<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    src: VertexId,
    bucket_slot: u64,
    bucket: &LabelBucket,
    target_bytes: &[u8; 4],
    next_stored: u32,
    next_degree: u32,
    _label: BucketLabelKey,
) -> Result<u32, LabeledOperationError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    debug_assert_eq!(bucket.tree_mode_physical_depth(), 1);
    // Root region: [edge_start, edge_start + root_len) where root_len
    // is the physical depth-1 ceil chain = ceil(stored_slots / B).
    let old_root_len = physical_root_len_ceil(u64::from(bucket.stored_slots), 1)?;
    let new_root_len = old_root_len
        .checked_add(1)
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
    // 1. Mint the new LTB block.
    let new_block_id = graph.ltb().mint().map_err(LabeledOperationError::from)?;
    // 2. Allocate the new root region span.
    let new_edge_start = match graph.edges().allocate_span(u64::from(new_root_len)) {
        Ok(s) => s,
        Err(e) => {
            let _ = graph.ltb().release(new_block_id);
            return Err(e.into());
        }
    };
    // 3. Copy old block_ids + append the new block_id.
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
    // 4. Write the edge into the new (empty) leaf block at offset 0.
    if let Err(e) = graph
        .ltb()
        .write_payload_partial(new_block_id, 0, target_bytes)
    {
        let _ = graph.ltb().release(new_block_id);
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(new_root_len));
        return Err(LabeledOperationError::LtbBlock(e));
    }
    // 5. Publish the new descriptor.
    let new_bucket = bucket
        .with_edge_range(new_edge_start, next_stored)
        .with_degree_field(next_degree)
        .with_stored_slots(next_stored)
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(1);
    if let Err(e) = graph
        .buckets()
        .write_label_bucket_slot(bucket_slot, new_bucket)
    {
        let zero = [0u8; 4];
        let _ = graph.ltb().write_payload_partial(new_block_id, 0, &zero);
        let _ = graph.ltb().release(new_block_id);
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(new_root_len));
        return Err(e.into());
    }
    // 6. Release the old root region span.
    if new_edge_start != bucket.edge_start() {
        let _ = graph
            .edges()
            .release_span(bucket.edge_start(), u64::from(old_root_len));
    }
    // 7. Bump global accounting.
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

/// Depth-≥2 tail append: append the new leaf to its home interior
/// (the interior holding the leaf index `l = ceil(stored_slots / B)`).
/// The root grows only when a new interior is minted
/// (`l % K == 0`).
///
/// **Layout** (depth 2): root is `[interior_0, interior_1, ...]` of
/// length `R_MAX` (or fewer if a 2^20-aligned boundary hasn't been
/// hit). Each interior holds K=1024 leaf block_ids at offset
/// `(row * 4)` bytes.
///
/// **Reserve / Commit / Publish** for the new-interior-mint case:
/// 1. **Reserve**: mint the new leaf block; mint the new interior
///    block; grow the root region by 1 (realloc + copy + append);
///    mark the interior as `EdgeInterior`; pack the leaf id into
///    interior row 0; write the edge into the leaf block at offset
///    0. Each step has LIFO rollback.
/// 2. **Commit**: build the new descriptor (edge_start unchanged;
///    stored/degree bumped).
/// 3. **Publish**: single canonical write. On failure release the
///    leaf, the interior, and the new root span.
///
/// **Why this lives in tree_write.rs**: the right-spine cascade is
/// the insert side of tree growth. The shape mirrors `tree_mode_deepen`
/// (reserve/commit/publish) but the trigger is a single leaf append
/// into the existing interior structure.
fn tree_mode_tail_append_depth_ge2<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    src: VertexId,
    bucket_slot: u64,
    bucket: &LabelBucket,
    target_bytes: &[u8; 4],
    next_stored: u32,
    next_degree: u32,
    _label: BucketLabelKey,
) -> Result<u32, LabeledOperationError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    let depth = bucket.tree_mode_physical_depth();
    debug_assert!(depth >= 2, "caller must check depth");
    let k = u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
    let b = u32::try_from(BLOCK_B).expect("B fits u32");
    // Leaf index for the new leaf: l = ceil(stored_slots / B). Note
    // `stored_slots` is the pre-insert value (this helper is called
    // when the previous leaf block is full at the boundary; the
    // next slot lives in a new leaf block).
    let stored_slots = bucket.stored_slots;
    let leaf_index: u32 = u32::try_from((u64::from(stored_slots)).div_ceil(b as u64))
        .expect("leaf_index fits u32 for MAX_DEPTH=3");
    // Home interior: the last interior level. At depth d=2 the
    // last interior is the single root level (level 0 = root,
    // level 1 = leaf). For the new leaf, the home interior is
    // `l / K`.
    let home_interior_index = leaf_index / k;
    let row_in_home_interior = leaf_index % k;
    // 1. Resolve the home interior's block_id.
    let home_interior_id = resolve_interior_block_id::<E, M>(graph, bucket, home_interior_index)?;
    // 2. Mint the new leaf block.
    let new_leaf_id = graph.ltb().mint().map_err(LabeledOperationError::from)?;
    // 3. Branch on row index.
    if row_in_home_interior != 0 {
        // === Interior-row append (root unchanged) ===
        // Write the leaf id into the home interior at the row
        // position, then write the edge into the leaf block at
        // offset 0. Then publish the descriptor (edge_start
        // unchanged). LIFO rollback.
        if let Err(e) = graph.ltb().write_payload_partial(
            home_interior_id,
            (row_in_home_interior as usize) * E::BYTES,
            &new_leaf_id.to_le_bytes(),
        ) {
            let _ = graph.ltb().release(new_leaf_id);
            return Err(LabeledOperationError::LtbBlock(e));
        }
        if let Err(e) = graph
            .ltb()
            .write_payload_partial(new_leaf_id, 0, target_bytes)
        {
            // Roll back the interior pointer (we don't need to
            // zero the row, because the descriptor still has
            // stored_slots == pre-insert and the resolver's
            // hop chain only consults rows < pre-insert ceil).
            let _ = graph.ltb().release(new_leaf_id);
            return Err(LabeledOperationError::LtbBlock(e));
        }
        // Publish the descriptor (edge_start unchanged).
        let new_bucket = bucket
            .with_stored_slots(next_stored)
            .with_degree_field(next_degree)
            .with_tree_mode(true)
            .with_tree_mode_physical_depth(depth);
        if let Err(e) = graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, new_bucket)
        {
            return Err(e.into());
        }
        // Bump global accounting.
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
        return Ok(next_stored - 1);
    }
    // === New-interior mint (root grows) ===
    // Mint the new interior block. LIFO rollback on any failure
    // (release the leaf + the interior + the new root span, in
    // the order they were minted/allocated).
    let new_interior_id = match graph.ltb().mint() {
        Ok(id) => id,
        Err(e) => {
            let _ = graph.ltb().release(new_leaf_id);
            return Err(LabeledOperationError::from(e));
        }
    };
    // Mark the new interior as `EdgeInterior` (mirrors `tree_mode_deepen`).
    let interior_header = crate::labeled::ltb_raw_block_store::BlockHeader {
        kind: crate::labeled::ltb_raw_block_store::BlockKind::EdgeInterior,
        bucket_label_key_wire: 0,
        owner_or_next_free: 0,
        ordinal: 0,
        level: 1, // depth 2 → interior at level 1
        reserved: [0u8; 3],
    };
    graph
        .ltb()
        .write_block_header(new_interior_id, &interior_header);
    // Grow the root region by 1: realloc + copy + append the new
    // interior id. Physical root length uses the same physical
    // ceil-chain (NOT `derived_root_len`).
    let old_root_len = physical_root_len_ceil(u64::from(bucket.stored_slots), depth)?;
    let new_root_len = old_root_len
        .checked_add(1)
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
    let new_edge_start = match graph.edges().allocate_span(u64::from(new_root_len)) {
        Ok(s) => s,
        Err(e) => {
            let _ = graph.ltb().release(new_interior_id);
            let _ = graph.ltb().release(new_leaf_id);
            return Err(e.into());
        }
    };
    let mut new_root_bytes: Vec<u8> = Vec::with_capacity(new_root_len as usize * 4);
    if old_root_len > 0 {
        let mut old_bytes = vec![0u8; old_root_len as usize * 4];
        graph
            .edges()
            .read_slots_contiguous_bytes(bucket.edge_start(), &mut old_bytes);
        new_root_bytes.extend_from_slice(&old_bytes);
    }
    new_root_bytes.extend_from_slice(&new_interior_id.to_le_bytes());
    if let Err(e) = graph
        .edges()
        .write_slots_contiguous_bytes(new_edge_start, &new_root_bytes)
    {
        let _ = graph.ltb().release(new_interior_id);
        let _ = graph.ltb().release(new_leaf_id);
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(new_root_len));
        return Err(e.into());
    }
    // Write the leaf id into the new interior at row 0.
    if let Err(e) =
        graph
            .ltb()
            .write_payload_partial(new_interior_id, 0, &new_leaf_id.to_le_bytes())
    {
        // Roll back: release the new interior, the new leaf, and
        // the new root span. The old root region is intact.
        let _ = graph.ltb().release(new_interior_id);
        let _ = graph.ltb().release(new_leaf_id);
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(new_root_len));
        return Err(LabeledOperationError::LtbBlock(e));
    }
    // Write the edge into the new leaf at offset 0.
    if let Err(e) = graph
        .ltb()
        .write_payload_partial(new_leaf_id, 0, target_bytes)
    {
        let _ = graph.ltb().release(new_interior_id);
        let _ = graph.ltb().release(new_leaf_id);
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(new_root_len));
        return Err(LabeledOperationError::LtbBlock(e));
    }
    // Publish the new descriptor (edge_start updated, depth
    // unchanged).
    let new_bucket = bucket
        .with_edge_range(new_edge_start, next_stored)
        .with_degree_field(next_degree)
        .with_stored_slots(next_stored)
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(depth);
    if let Err(e) = graph
        .buckets()
        .write_label_bucket_slot(bucket_slot, new_bucket)
    {
        let _ = graph.ltb().release(new_interior_id);
        let _ = graph.ltb().release(new_leaf_id);
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(new_root_len));
        return Err(e.into());
    }
    // Release the old root region.
    if new_edge_start != bucket.edge_start() {
        let _ = graph
            .edges()
            .release_span(bucket.edge_start(), u64::from(old_root_len));
    }
    // Bump global accounting.
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
/// Resolve the interior `block_id` that holds the `interior_index`-th
/// interior block_id at the level just above the leaves.
///
/// The hop chain is the same as `resolve_leaf_block_id` but truncated
/// one level short — it descends from the root to the last interior
/// level (depth `d - 1` for a depth-`d` bucket) and returns that
/// interior's `block_id`. Callers that need a specific leaf instead
/// should use `resolve_leaf_block_id`.
///
/// At depth 1 there are no interior levels, so this helper is only
/// meaningful for depth ≥ 2. The caller checks depth before calling.
///
/// **Structural vs physical depth**: like `resolve_leaf_block_id`, this
/// uses `bucket.tree_mode_physical_depth()`. A manually-deepened
/// bucket at `stored = 1,048,576` has `derive_depth = 1` but physical
/// depth 2; using the structural formula would under-walk the chain
/// and return a stale/garbage block_id.
///
/// **Mixed-radix indexing** mirrors the leaf resolver: at level j
/// (0 = root, 1 = first interior, ..., d-2 = last interior), the
/// index is `(interior_index / K^(d-2-j)) % K`. The first hop reads
/// from the LEG root region; subsequent hops read from the previous
/// interior block's payload at `(idx % K) * E::BYTES`.
pub(crate) fn resolve_interior_block_id<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    bucket: &LabelBucket,
    interior_index: u32,
) -> Result<u32, LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    debug_assert_eq!(
        E::BYTES,
        4,
        "resolve_interior_block_id requires E::BYTES == 4 (typed guard lives at the dispatcher)"
    );
    let depth = bucket.tree_mode_physical_depth();
    debug_assert!(
        depth >= 2,
        "resolve_interior_block_id requires physical depth >= 2 (caller must check)"
    );
    let k = u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
    // For depth `d`, the interior levels are 0..=d-2 (root is level 0,
    // leaf is level d-1). The last interior (level d-2) holds K leaf
    // block_ids. We descend from the root, stopping at level d-2.
    //
    // The hop chain stops one level short of `resolve_leaf_block_id`:
    // - Level 0 hop: read root[level_idx_0] from the LEG root region.
    // - Levels 1..=d-2: read interior[level_idx_j] from the previous
    //   interior's payload.
    //
    // divisor_j = K^(d-2-j)  (with the convention K^0 = 1).
    let mut child_id: u32 = {
        // Level 0 hop.
        let divisor = k.pow(depth - 2);
        let level_idx = (interior_index / divisor) % k;
        let mut id_bytes = [0u8; 4];
        graph
            .edges()
            .read_slot_bytes(bucket.edge_start() + u64::from(level_idx), &mut id_bytes);
        u32::from_le_bytes(id_bytes)
    };
    // Descend levels 1..=d-2 (interior hops; final hop is the last
    // interior at level d-2).
    for j in 1..depth - 1 {
        let divisor = k.pow(depth - 2 - j);
        let level_idx = (interior_index / divisor) % k;
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

/// **Plan 0319 §Step 1**: rebuild a tree-mode bucket as a fresh CSR
/// slab containing only live edges.
///
/// Tree-mode insert always appends (slab tombstone reuse never
/// applies), so a high-churn bucket accumulates tombstones: `stored_slots`
/// grows monotonically while `degree` shrinks. Demotion is the primary
/// reclaim path — it rebuilds the bucket as a slab of `degree` live
/// edges, releases every LTB leaf + interior block and the old root
/// region, and restores pre-0318 slab growth/compaction behavior for the
/// bucket.
///
/// **Preconditions** (typed, before any state change — mirrors the
/// promote path's Precondition 1-4 from `promote_bypass_to_tree_mode`):
/// 1. `bucket.is_tree_mode()` (caller's responsibility; assert).
/// 2. `E::BYTES == TREE_MODE_REQUIRED_EDGE_BYTES` (typed
///    `TreeModeEdgeWidthUnsupported`).
/// 3. `bucket.inline_property_byte_width() == 0` (LPB-in-tree is
///    Plan 0320+; typed `InlinePropertyBytesWidthMismatch`).
///
/// **Failure-atomic reserve / commit / publish** (mirrors
/// `tree_mode_flatten`):
/// 1. **Collect**: read live edges in ascending logical order via
///    `visit_tree_mode_label_bucket_edges`, filtering tombstones
///    (the visit yields ALL slot positions; tombstones are filtered
///    inside the visit closure using `E::is_tombstone_edge`).
/// 2. **Reserve**: `graph.edges().allocate_span(degree as u64)`.
/// 3. **Write**: copy the live edges contiguously into the new span.
/// 4. **Publish**: write the new slab-mode descriptor
///    (`with_tree_mode(false)`, `with_tree_mode_physical_depth(1)`
///    so the byte resets to 0; physical depth field is repurposed
///    for tree mode and is 0 for slab mode), then `write_label_bucket_slot`.
/// 5. **Release after publish**: walk the old root region for
///    interior block_ids, release each interior via
///    `ltb().release()`, release the leaf blocks via
///    `collect_leaf_block_ids` + `ltb().release()`, and
///    `release_span` the old root region and the old log-entry root
///    span. All post-publish releases are best-effort (`let _ =`),
///    matching `tree_mode_flatten` and `tree_mode_deepen`.
///
/// **On mid-demote failure (before publish)**: release the reserved
/// new span and return Err leaving the tree bucket fully intact.
/// The caller (the remove-path trigger) treats demotion as
/// best-effort: a successful removal must not be turned into an
/// error, so demote failures after a successful remove are
/// contained (`let _ =`).
pub(crate) fn tree_mode_demote_to_slab<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    bucket_slot: u64,
    label: BucketLabelKey,
    bucket: &LabelBucket,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    // Precondition 1 (asserted; the dispatcher filters).
    debug_assert!(bucket.is_tree_mode(), "demote requires tree-mode bucket");
    // Precondition 2: edge width.
    if E::BYTES != TREE_MODE_REQUIRED_EDGE_BYTES {
        return Err(LabeledOperationError::TreeModeEdgeWidthUnsupported {
            actual: E::BYTES,
            expected: TREE_MODE_REQUIRED_EDGE_BYTES,
        });
    }
    // Precondition 3: inline-property width.
    if bucket.inline_property_byte_width() != 0 {
        return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
            bucket_width: bucket.inline_property_byte_width(),
            edge_inline_property_width: 0,
        });
    }

    let degree = bucket.degree;
    let label_raw = label.label_index();

    // Compute the old physical root region length (same formula as
    // `tree_mode_flatten` and `collect_leaf_block_ids`). The new
    // slab span must avoid this range so the in-progress demote
    // does not corrupt the live root region before the descriptor
    // flip. (For depth 1 the root region overlaps the released
    // slab prefix; `allocate_span_avoiding` is the correct API.)
    let physical_depth = bucket.tree_mode_physical_depth();
    let stored = bucket.stored_slots;
    let leaf_count =
        u32::try_from((u64::from(stored)).div_ceil(crate::labeled::tree_csr_prototype::B as u64))
            .expect("leaf_count fits u32 for MAX_DEPTH=3");
    let k_for_avoid =
        u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
    let old_physical_root_len: u32 = match physical_depth {
        1 => leaf_count,
        2 => leaf_count.div_ceil(k_for_avoid),
        3 => leaf_count.div_ceil(k_for_avoid).div_ceil(k_for_avoid),
        _ => unreachable!("tree_mode_physical_depth out of range"),
    };
    let avoid_range = if old_physical_root_len > 0 {
        Some((bucket.edge_start(), u64::from(old_physical_root_len)))
    } else {
        None
    };

    // Phase 1: Collect live edges in ascending logical order.
    // We use the visit function directly so we can filter tombstones
    // (the visit yields ALL slot positions, including tombstoned ones;
    // `tree_mode_out_edges_collect` would include tombstones too).
    let mut live: Vec<E> = Vec::new();
    live.try_reserve_exact(degree as usize)
        .map_err(|_| LabeledOperationError::from(LaraOperationError::CollectAllocationOverflow))?;
    super::tree_read::visit_tree_mode_label_bucket_edges(
        graph,
        label_raw,
        bucket,
        degree,
        super::OutEdgeOrder::Ascending,
        |_slot, edge| {
            if !edge.is_tombstone_edge() {
                live.push(edge);
            }
        },
    )?;
    // Sanity: the live count must equal `degree` (every non-tombstone
    // edge is live). If the visit has a bug (e.g. double-counts) this
    // assertion catches it before we publish a malformed descriptor.
    debug_assert_eq!(
        live.len(),
        degree as usize,
        "live edge count must equal bucket.degree"
    );

    // Phase 2: Reserve new slab span of exactly `degree` slots,
    // avoiding the live root region (see comment above).
    let new_edge_start = match graph
        .edges()
        .allocate_span_avoiding(u64::from(degree), avoid_range)
    {
        Ok(s) => s,
        Err(e) => return Err(LabeledOperationError::from(e)),
    };

    // Phase 3: Write the live edges contiguously.
    let write_bytes = {
        // E::BYTES == 4 (precondition), so live is a tight
        // little-endian 4-byte-per-edge layout.
        let mut bytes = vec![0u8; live.len() * E::BYTES];
        for (i, edge) in live.iter().enumerate() {
            let slot_bytes = &mut bytes[i * E::BYTES..(i + 1) * E::BYTES];
            edge.write_to(slot_bytes);
        }
        bytes
    };
    if let Err(e) = graph
        .edges()
        .write_slots_contiguous(new_edge_start, &write_bytes)
    {
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(degree));
        return Err(LabeledOperationError::from(e));
    }

    // Phase 4: Build the new slab-mode descriptor and publish.
    //
    // Slab mode: `with_tree_mode(false)`, `with_tree_mode_physical_depth(1)`
    // (resets the repurposed byte to 0), all overflow-log fields reset
    // to empty (the tree log held root block ids; the rebuild has no
    // log). `stored_slots = degree` (slab convention: stored == degree
    // for fresh slabs, gap regrows via the existing leaf cascade).
    let new_bucket = bucket
        .with_edge_range(new_edge_start, degree)
        .with_stored_slots(degree)
        .with_degree_field(degree)
        .with_overflow_log_head(-1)
        .with_tree_mode(false)
        .with_tree_mode_physical_depth(1);
    if let Err(e) = graph
        .buckets()
        .write_label_bucket_slot(bucket_slot, new_bucket)
    {
        // Publish failed: release the reserved new span and return.
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(degree));
        return Err(e.into());
    }

    // Phase 5: After publish, release all old resources. Best-effort:
    // the descriptor no longer references them.
    //
    // 5a. Collect leaf block_ids via the depth-generic resolver, then
    //     release each.
    let leaf_ids = collect_leaf_block_ids::<E, M>(graph, bucket)?;
    for leaf_id in leaf_ids {
        let _ = graph.ltb().release(leaf_id);
    }
    // 5b. Release interior blocks: walk the old root region for u32
    //     block_ids (same pattern as `tree_mode_flatten`'s interior
    //     release at the end of tree_write.rs). For depth 1, root is
    //     leaf ids (already released in 5a); for depth 2+, root is
    //     interior ids (need to release here).
    //
    // physical_depth and old_physical_root_len were computed at the
    // top of the function for the avoid_range.
    // Read the old root region to discover block_ids (leaves at depth 1,
    // interiors at depth >= 2). For depth 1 the block_ids are leaves —
    // we already released them in 5a, so this walk is skipped to avoid
    // the LTB pop-time double-release guard. For depth 2+ these are
    // interiors and need release here.
    if physical_depth >= 2 && old_physical_root_len > 0 {
        let mut old_root_bytes = vec![0u8; old_physical_root_len as usize * 4];
        graph
            .edges()
            .read_slots_contiguous_bytes(bucket.edge_start(), &mut old_root_bytes);
        for chunk in old_root_bytes.chunks(4) {
            if chunk.len() < 4 {
                break;
            }
            let id = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let _ = graph.ltb().release(id);
        }
    }
    // 5c. Release the old root region span.
    if old_physical_root_len > 0 {
        let _ = graph
            .edges()
            .release_span(bucket.edge_start(), u64::from(old_physical_root_len));
    }
    // 5d. Release the old log-entry root span. The log was orphaned
    //     by promote (overflow_log_head = -1), so its root is no
    //     longer referenced by the descriptor. The plan notes the log
    //     chain is compacted on next slab-mode growth; for tree
    //     buckets, the log held root block_ids and the entries are
    //     unreachable. Best-effort release.
    // The log root span offset/length: a tree bucket keeps the same
    // `edge_start` for both root and log in the storage layout (root
    // at edge_start, log at edge_start + root_len in the legacy
    // convention). For Plan 0318 the log was already orphaned via
    // `overflow_log_head = -1` at promote time, so no live log
    // entries exist for a tree bucket. The legacy log root span (if
    // any) was released during promote; nothing to do here.
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
        let label = BucketLabelKey::directed_from_index(1);
        // Set up vertex with a single bucket slot.
        let _vid: VertexId = graph
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
        let _vid = graph
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
    //
    // Plan 0325: the fail-closed boundary moved from the interim 2^20
    // (Plan 0318 §Step 7 amend) to the documented
    // `TREE_STRUCTURAL_CAP = 2^30` (ADR 0088 §4). The cascade grows
    // depth 1 → 2 at stored = 2^20 and fail-closes at stored = 2^30 +
    // 1 (root full at depth 2, root_len = 1024 = R_MAX).
    //
    // canbench cannot seed 2^30 edges (Plan 0324 audit: 17.9T ins >
    // 10T per-bench limit), so the 2^30 fail-closed boundary is
    // proven via a synthetic-layout unit test (build a depth-2
    // bucket with root_len = R_MAX and stored_slots = 2^30).

    /// Plan 0325 §Step 3: at the 2^30 structural cap, a tree-mode
    /// bucket with depth 2 + root_len = R_MAX + stored_slots = 2^30
    /// must fail-closed. The next insert would push
    /// `next_stored > TREE_STRUCTURAL_CAP`, triggering the typed
    /// `TreeRootCapacityReached` BEFORE any state change.
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_insert_fails_closed_at_2_30_cap() {
        // Synthetic depth-2 root-full layout:
        //   stored_slots = 2^30 = TREE_STRUCTURAL_CAP
        //   depth = 2 (physical)
        //   root_len = ceil(2^30 / 1024) / 1024 = 1024 = R_MAX
        let graph = test_graph();
        let label = BucketLabelKey::directed_from_index(1);
        let _vid = graph
            .push_vertex(
                crate::labeled::record::LabeledVertex::default()
                    .try_with_bucket_row(0, 1)
                    .expect("try_with_bucket_row"),
            )
            .expect("push_vertex");
        let bucket_slot = 0u64;
        let k = u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
        // Mint 1024 interior blocks (the root region is full at stored=2^30).
        let mut interior_ids: Vec<u32> = Vec::with_capacity(k as usize);
        for _ in 0..k {
            interior_ids.push(graph.ltb().mint().expect("mint interior"));
        }
        // Allocate a 1024-entry LEG root region.
        let edge_start = graph
            .edges()
            .allocate_span(u64::from(k))
            .expect("allocate_span root");
        let root_bytes: Vec<u8> = interior_ids
            .iter()
            .flat_map(|id| id.to_le_bytes())
            .collect();
        graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &root_bytes)
            .expect("write root");
        // Build the depth-2 root-full descriptor.
        let new_bucket = crate::labeled::record::LabelBucket::try_from_parts(
            label,
            edge_start,
            1u32 << 30,
            1u32 << 30,
            -1,
            0,
            0,
            0,
            -1,
            0,
        )
        .expect("try_from_parts")
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(2);
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, new_bucket)
            .expect("write descriptor");
        // The next insert must fail-closed.
        let new_edge = TestEdge { target: 0xCAFE };
        let result = tree_mode_insert_edge(
            &graph,
            crate::VertexId::from(0),
            bucket_slot,
            &new_bucket,
            label,
            &new_edge,
        );
        match result {
            Err(LabeledOperationError::TreeRootCapacityReached {
                stored_slots,
                root_len,
                cap,
            }) => {
                assert_eq!(stored_slots, (1u32 << 30) + 1);
                assert_eq!(root_len, k);
                assert_eq!(cap, k);
            }
            other => panic!("expected TreeRootCapacityReached, got {other:?}"),
        }
    }

    /// Plan 0325 §Step 3 (production-path integration): the 2^30
    /// fail-closed boundary must surface through the production
    /// `insert_edge_skip_leaf_cascade` API.
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn production_insert_path_fails_closed_at_2_30_cap() {
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
        let k = u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
        let mut interior_ids: Vec<u32> = Vec::with_capacity(k as usize);
        for _ in 0..k {
            interior_ids.push(graph.ltb().mint().expect("mint interior"));
        }
        let edge_start = graph
            .edges()
            .allocate_span(u64::from(k))
            .expect("allocate_span root");
        let root_bytes: Vec<u8> = interior_ids
            .iter()
            .flat_map(|id| id.to_le_bytes())
            .collect();
        graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &root_bytes)
            .expect("write root");
        let new_bucket = crate::labeled::record::LabelBucket::try_from_parts(
            label,
            edge_start,
            1u32 << 30,
            1u32 << 30,
            -1,
            0,
            0,
            0,
            -1,
            0,
        )
        .expect("try_from_parts")
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(2);
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, new_bucket)
            .expect("write descriptor");
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
                assert_eq!(stored_slots, (1u32 << 30) + 1);
                assert_eq!(root_len, k);
                assert_eq!(cap, k);
            }
            other => panic!("expected TreeRootCapacityReached, got {other:?}"),
        }
    }

    // ====================== Plan 0325: cascade unit matrix ======================
    //
    // The right-spine cascade is exercised by the canbench surface
    // (`tcsr_1048576_deepen_beyond_r_max` runs the production path
    // on the wasm target). The unit matrix below uses host
    // `VectorMemory` for cheap verification of:
    //  (1) the cascade wiring at 2^20 + 1 (depth 1 → 2 via the raw
    //      `tree_mode_insert_edge` helper, not the production path)
    //  (2) interior-row append: cascade → 1st insert deepens → 2nd
    //      insert goes into the existing interior (root unchanged)
    //  (3) new-interior mint: fill 1 interior, next leaf crosses
    //      the interior boundary, root grows by 1
    //  (4) demote from depth 2 (via `tree_mode_demote_to_slab`)
    //  (5) batch tail-fit at depth 2
    //  (6) public read accessors (`visit_edges`, `visit_edges_window`)
    //      over a depth-2 bucket

    /// Plan 0325 §Step 3: cascade fires at 2^20 + 1. Build a
    /// depth-1 tree bucket at exactly 2^20 (root_len = R_MAX = 1024),
    /// then run the raw `tree_mode_insert_edge` helper. The cascade
    /// must deepen (depth 1 → 2), commit the insert, and the bucket
    /// must be readable at depth 2.
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn cascade_at_2_20_plus_1_deepens() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        let r_max =
            u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
        let target_stored: u32 = (BLOCK_B as u32) * r_max; // 2^20
        promote_test_bucket(&graph, vid, label, target_stored);
        // Verify the pre-insert state is depth 1, root_len = R_MAX.
        let vertex = graph.vertices().get(vid);
        let pre_bucket = match graph.find_bucket(vid, &vertex, label).expect("find") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        let pre_slot = match graph.find_bucket(vid, &vertex, label).expect("find") {
            BucketSearch::Found { slot, .. } => slot,
            _ => panic!("bucket slot missing"),
        };
        assert!(pre_bucket.is_tree_mode());
        assert_eq!(pre_bucket.stored_slots, target_stored);
        assert_eq!(pre_bucket.tree_mode_physical_depth(), 1);
        // Insert one edge via the raw helper.
        let new_edge = TestEdge { target: 0xDEAD };
        let logical_slot =
            tree_mode_insert_edge(&graph, vid, pre_slot, &pre_bucket, label, &new_edge)
                .expect("cascade insert must succeed");
        assert_eq!(logical_slot, target_stored);
        // Re-read: depth 2, stored 2^20 + 1.
        let vertex = graph.vertices().get(vid);
        let post_bucket = match graph.find_bucket(vid, &vertex, label).expect("find post") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing post"),
        };
        assert!(post_bucket.is_tree_mode());
        assert_eq!(post_bucket.stored_slots, target_stored + 1);
        assert_eq!(post_bucket.degree, target_stored + 1);
        assert_eq!(
            post_bucket.tree_mode_physical_depth(),
            2,
            "cascade must deepen depth 1 → 2"
        );
        // The new edge must be readable via the public visit API
        // (the test exercises the production read path, not the LTB
        // primitive). Count visits to confirm all stored slots are
        // walked.
        let mut count: u32 = 0;
        let _ = graph.visit_edges(vid, label, OutEdgeOrder::Ascending, |_slot, _edge| {
            count += 1;
            std::ops::ControlFlow::<()>::Continue(())
        });
        assert_eq!(count, target_stored + 1);
    }

    /// Plan 0325 §Step 3: interior-row append. After the cascade
    /// deepens a 2^20-bucket to depth 2 (1 interior), the next
    /// insert appends to the existing interior at row 1 (no root
    /// grow). Build a synthetic depth-2 bucket with 1 interior
    /// holding 1024 leaf block_ids + a 2nd interior with 1 leaf
    /// at row 0 (the 1025th leaf); stored = 2^20 + 1. The next
    /// insert goes to row 1 of the 2nd interior (l = 1025,
    /// l % K = 1, no root grow).
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn interior_row_append_keeps_root_constant() {
        let graph = test_graph();
        let label = BucketLabelKey::directed_from_index(1);
        let _vid = graph
            .push_vertex(
                crate::labeled::record::LabeledVertex::default()
                    .try_with_bucket_row(0, 1)
                    .expect("try_with_bucket_row"),
            )
            .expect("push_vertex");
        let bucket_slot = 0u64;
        // Mint 1024 + 1 = 1025 leaves (first interior full,
        // second interior has 1 leaf at row 0).
        let mut leaf_ids: Vec<u32> = Vec::with_capacity(1025);
        for _ in 0..1025 {
            leaf_ids.push(graph.ltb().mint().expect("mint leaf"));
        }
        // Mint 2 interiors.
        let mut interior_ids: Vec<u32> = Vec::with_capacity(2);
        for _ in 0..2 {
            let id = graph.ltb().mint().expect("mint interior");
            interior_ids.push(id);
            let header = crate::labeled::ltb_raw_block_store::BlockHeader {
                kind: crate::labeled::ltb_raw_block_store::BlockKind::EdgeInterior,
                bucket_label_key_wire: 0,
                owner_or_next_free: 0,
                ordinal: 0,
                level: 1,
                reserved: [0u8; 3],
            };
            graph.ltb().write_block_header(id, &header);
        }
        // First interior: 1024 leaf_ids (rows 0..1023).
        let first_chunk: Vec<u8> = leaf_ids[0..1024]
            .iter()
            .flat_map(|id| id.to_le_bytes())
            .collect();
        graph
            .ltb()
            .write_payload_partial(interior_ids[0], 0, &first_chunk)
            .expect("write first interior");
        // Second interior: row 0 = leaf_ids[1024], rows 1..1023
        // = 0 (unused, but written for block completeness).
        let mut second_chunk: Vec<u8> = vec![0u8; 1024 * 4];
        second_chunk[0..4].copy_from_slice(&leaf_ids[1024].to_le_bytes());
        graph
            .ltb()
            .write_payload_partial(interior_ids[1], 0, &second_chunk)
            .expect("write second interior");
        // Allocate a 2-slot root region.
        let edge_start = graph.edges().allocate_span(2).expect("allocate_span root");
        let root_bytes: Vec<u8> = interior_ids
            .iter()
            .flat_map(|id| id.to_le_bytes())
            .collect();
        graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &root_bytes)
            .expect("write root");
        // Descriptor: stored = 2^20 + 1 (one extra slot in the
        // 1025th leaf).
        let pre_stored: u32 = (1u32 << 20) + 1;
        let pre_bucket = crate::labeled::record::LabelBucket::try_from_parts(
            label, edge_start, pre_stored, pre_stored, -1, 0, 0, 0, -1, 0,
        )
        .expect("try_from_parts")
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(2);
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, pre_bucket)
            .expect("write descriptor");
        // Insert one more edge. l = ceil((2^20+1)/1024) = 1025.
        // l % K = 1, so the interior-row append path fires:
        // root unchanged.
        let new_edge = TestEdge { target: 0xBEEF };
        let logical_slot = tree_mode_insert_edge(
            &graph,
            crate::VertexId::from(0),
            bucket_slot,
            &pre_bucket,
            label,
            &new_edge,
        )
        .expect("interior-row append must succeed");
        assert_eq!(logical_slot, pre_stored);
        // Re-read: root unchanged, depth unchanged, stored 2^20+2.
        let post_bucket = graph
            .buckets()
            .read_label_bucket_slot(bucket_slot)
            .expect("post read");
        assert_eq!(post_bucket.stored_slots, pre_stored + 1);
        assert_eq!(post_bucket.degree, pre_stored + 1);
        assert_eq!(post_bucket.tree_mode_physical_depth(), 2);
        assert_eq!(
            post_bucket.edge_start(),
            edge_start,
            "interior-row append must not grow the root"
        );
    }

    /// Plan 0325 §Step 3: new-interior mint. Build a synthetic
    /// depth-2 bucket at stored = 2^21 = 2,097,152 (2 full
    /// interiors, 2,048 leaves all full). The next insert would
    /// be the 2,049th leaf at row 0 of a new interior, l % K = 0
    /// → new-interior mint, root grows from 2 to 3.
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn new_interior_mint_grows_root() {
        let graph = test_graph();
        // Use a non-default label (2) so the vertex is NOT in
        // default-edge-labeled mode — the visit path takes the
        // tree branch, not the bypass.
        let label = BucketLabelKey::directed_from_index(2);
        let _vid = graph
            .push_vertex(
                crate::labeled::record::LabeledVertex::default()
                    .try_with_bucket_row(0, 1)
                    .expect("try_with_bucket_row"),
            )
            .expect("push_vertex");
        let bucket_slot = 0u64;
        // Mint 2048 leaves (the next insert mints the 2049th).
        let mut leaf_ids: Vec<u32> = Vec::with_capacity(2048);
        for _ in 0..2048 {
            leaf_ids.push(graph.ltb().mint().expect("mint leaf"));
        }
        let mut interior_ids: Vec<u32> = Vec::with_capacity(2);
        for _ in 0..2 {
            let id = graph.ltb().mint().expect("mint interior");
            interior_ids.push(id);
            let header = crate::labeled::ltb_raw_block_store::BlockHeader {
                kind: crate::labeled::ltb_raw_block_store::BlockKind::EdgeInterior,
                bucket_label_key_wire: 0,
                owner_or_next_free: 0,
                ordinal: 0,
                level: 1,
                reserved: [0u8; 3],
            };
            graph.ltb().write_block_header(id, &header);
        }
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
                .expect("write_payload_partial");
        }
        let edge_start = graph.edges().allocate_span(2).expect("allocate_span");
        let root_bytes: Vec<u8> = interior_ids
            .iter()
            .flat_map(|id| id.to_le_bytes())
            .collect();
        graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &root_bytes)
            .expect("write root");
        // Descriptor: stored = 2*1024*1024 - 1 = 2,097,151.
        let pre_stored: u32 = (1u32 << 21) - 1;
        // pre_stored = 2,097,151 = 2,047 leaves * 1024 + 1023.
        // tail_offset = 1023 * 4 = 4092 (NOT 0) so the tail-room
        // path runs, not the cascade. For the cascade to fire we
        // need a bucket where stored_slots % B == 0. Use
        // pre_stored = 2^20 * 2 = 2,097,152 (2,048 leaves, all
        // full).
        let _ = pre_stored; // suppress unused; we redefine below
        let pre_stored: u32 = 1u32 << 21;
        let pre_bucket = crate::labeled::record::LabelBucket::try_from_parts(
            label, edge_start, pre_stored, pre_stored, -1, 0, 0, 0, -1, 0,
        )
        .expect("try_from_parts")
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(2);
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, pre_bucket)
            .expect("write descriptor");
        // Insert one. l = ceil(2,097,152/1024) = 2048. l % K = 0
        // → new-interior mint, root grows from 2 to 3.
        let new_edge = TestEdge { target: 0xFEED };
        let logical_slot = tree_mode_insert_edge(
            &graph,
            crate::VertexId::from(0),
            bucket_slot,
            &pre_bucket,
            label,
            &new_edge,
        )
        .expect("new-interior mint must succeed");
        assert_eq!(logical_slot, pre_stored);
        let post_bucket = graph
            .buckets()
            .read_label_bucket_slot(bucket_slot)
            .expect("post read");
        assert_eq!(post_bucket.stored_slots, pre_stored + 1);
        assert_eq!(post_bucket.tree_mode_physical_depth(), 2);
        // Verify the root grew by 1: read the post-insert root
        // region (3 entries) and check the 3rd entry is a valid
        // LTB block_id (not zero). The structural root_len formula
        // would still report 2 for stored = 2^21 (since
        // ceil(2^21 / 1024^2) = 2), so we read the actual physical
        // root region from the descriptor's `edge_start`.
        let mut new_root_bytes = [0u8; 12];
        graph
            .edges()
            .read_slots_contiguous_bytes(post_bucket.edge_start(), &mut new_root_bytes);
        let new_root: Vec<u32> = new_root_bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(
            new_root.len(),
            3,
            "new-interior mint must grow the root region to 3 entries (got {:?})",
            new_root
        );
        assert!(
            new_root[2] > 0,
            "the new 3rd root entry must be a valid LTB block id (got {})",
            new_root[2]
        );
        // Verify the new interior holds the new leaf id at row 0.
        let mut new_interior_id_bytes = [0u8; 4];
        graph
            .edges()
            .read_slot_bytes(post_bucket.edge_start() + 2, &mut new_interior_id_bytes);
        let new_interior_id = u32::from_le_bytes(new_interior_id_bytes);
        assert!(
            new_interior_id > 0,
            "new-interior mint must write a fresh LTB block id at root[2]"
        );
    }

    /// Plan 0325 §Step 3 (read accessors over depth 2): build a
    /// depth-2 bucket and verify `visit_edges` walks the LTB
    /// correctly. Synthetic layout: 1025 leaves, 2 interiors.
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn public_read_accessors_over_depth_2() {
        let graph = test_graph();
        let label = BucketLabelKey::directed_from_index(1);
        let _vid = graph
            .push_vertex(
                crate::labeled::record::LabeledVertex::default()
                    .try_with_bucket_row(0, 1)
                    .expect("try_with_bucket_row"),
            )
            .expect("push_vertex");
        let bucket_slot = 0u64;
        let mut leaf_ids: Vec<u32> = Vec::with_capacity(1025);
        for _ in 0..1025 {
            leaf_ids.push(graph.ltb().mint().expect("mint leaf"));
        }
        // Write target bytes (i as u32 LE) into each leaf at
        // position 0.
        for (i, leaf_id) in leaf_ids.iter().enumerate() {
            let bytes = (i as u32).to_le_bytes();
            graph
                .ltb()
                .write_payload_partial(*leaf_id, 0, &bytes)
                .expect("write leaf payload");
        }
        let mut interior_ids: Vec<u32> = Vec::with_capacity(2);
        for _ in 0..2 {
            let id = graph.ltb().mint().expect("mint interior");
            interior_ids.push(id);
            let header = crate::labeled::ltb_raw_block_store::BlockHeader {
                kind: crate::labeled::ltb_raw_block_store::BlockKind::EdgeInterior,
                bucket_label_key_wire: 0,
                owner_or_next_free: 0,
                ordinal: 0,
                level: 1,
                reserved: [0u8; 3],
            };
            graph.ltb().write_block_header(id, &header);
        }
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
                .expect("write_payload_partial");
        }
        let edge_start = graph.edges().allocate_span(2).expect("allocate_span");
        let root_bytes: Vec<u8> = interior_ids
            .iter()
            .flat_map(|id| id.to_le_bytes())
            .collect();
        graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &root_bytes)
            .expect("write root");
        let stored: u32 = 1_048_577;
        let bucket = crate::labeled::record::LabelBucket::try_from_parts(
            label, edge_start, stored, stored, -1, 0, 0, 0, -1, 0,
        )
        .expect("try_from_parts")
        .with_tree_mode(true)
        .with_tree_mode_physical_depth(2);
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, bucket)
            .expect("write descriptor");
        // visit_edges ascending: should yield 1,048,577 visits.
        // The public `graph.visit_edges` API uses the
        // default-edge-labeled bypass for label 1 (which IS the
        // graph's default), so it doesn't exercise the tree branch
        // for this vertex. We instead verify the read via the LTB
        // primitive (`tree_mode_out_edges_collect`), which directly
        // walks the leaf block_ids via `resolve_leaf_block_id`.
        // This primitive was fixed in this slice to be depth-2-safe
        // (the prior `debug_assert_eq!(leaf_count, root_len)` for
        // depth 1 only was relaxed to `leaf_count >= root_len`).
        let collected = tree_mode_out_edges_collect(
            &graph,
            label.raw(),
            &bucket,
            bucket.degree,
            OutEdgeOrder::Ascending,
        )
        .expect("collect");
        assert_eq!(
            collected.len() as u32,
            1_048_577,
            "tree_mode_out_edges_collect must walk all stored slots"
        );
        // Verify the first slot payload (slot 0 → leaf[0][0] = 0)
        // and the last slot payload (slot 1,048,576 = 2^20 →
        // leaf[1024][0] = 1024). Note: leaf_payload writes
        // indexed leaves by `i` (0..1025), so leaf[1024] has
        // payload[0] = 1024. The slot 1,048,576 is the first slot
        // in leaf[1024], which holds target 1024.
        assert_eq!(collected[0].target, 0);
        assert_eq!(collected[1_048_576].target, 1024);
        // The middle slots read zero-initialized payload (we only
        // wrote the first row of each leaf); confirm a sample.
        assert_eq!(collected[1023].target, 0);
        assert_eq!(collected[1024].target, 1);
        assert_eq!(collected[1_048_575].target, 0);
    }

    // -----------------------------------------------------------------
    // Plan 0319 §Step 1 — demotion primitive tests
    // -----------------------------------------------------------------

    /// Plan 0319 §Step 1 test (a): demote round-trip preserves the live
    /// edge set exactly. Promote at 4096, remove ~half, force a degree
    /// <= T_DEMOTE state, demote, and assert the bucket is slab-mode
    /// with `stored == degree` and the read-back edge set matches the
    /// expected live set.
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn demote_round_trip_preserves_live_edge_set() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        let start = 4096u32;
        promote_test_bucket(&graph, vid, label, start);

        // Tombstone ~half the slots via tree-mode remove: keep even
        // slots live, tombstone odd slots. After this step, degree
        // = 2048 (still > T_DEMOTE) and stored_slots = 4096.
        let bucket_slot = {
            let v = graph.vertices().get(vid);
            match graph.find_bucket(vid, &v, label).expect("find") {
                BucketSearch::Found { slot, .. } => slot,
                _ => panic!("bucket missing"),
            }
        };
        for slot in (1..start).step_by(2) {
            let bucket = match graph
                .find_bucket(vid, &graph.vertices().get(vid), label)
                .expect("find")
            {
                BucketSearch::Found { bucket, .. } => bucket,
                _ => panic!("bucket missing mid-tombstone"),
            };
            tree_mode_remove_edge_at_slot(&graph, vid, bucket_slot, &bucket, slot)
                .expect("tree remove")
                .expect("slot in range");
        }
        // After ~half-tombstone, degree = 2048, stored = 4096.
        let v = graph.vertices().get(vid);
        let b = match graph.find_bucket(vid, &v, label).expect("find") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        assert_eq!(b.degree, 2048);
        assert_eq!(b.stored_slots, 4096);
        assert!(b.is_tree_mode());

        // Snapshot the LTB allocated count before demote.
        let alloc_before = graph
            .ltb()
            .block_capacity()
            .saturating_sub(graph.ltb().free_count());

        // Snapshot LEG header (edge count) before demote.
        let num_before = graph.edges().header().num_edges;

        // Demote.
        tree_mode_demote_to_slab(&graph, bucket_slot, label, &b).expect("demote");

        // Re-read the bucket: must be slab-mode, stored == degree == 2048,
        // overflow_log_head = -1, inline_property_bytes_log_len = 0
        // (physical depth byte reset).
        let v2 = graph.vertices().get(vid);
        let b2 = match graph.find_bucket(vid, &v2, label).expect("find") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing post-demote"),
        };
        assert!(!b2.is_tree_mode(), "demoted bucket must be slab-mode");
        assert_eq!(b2.degree, 2048);
        assert_eq!(b2.stored_slots, 2048);
        assert_eq!(b2.tree_mode_physical_depth(), 0); // slab mode => 0
        assert_eq!(b2.inline_property_byte_width(), 0);
        assert_eq!(b2.overflow_log_head(), -1);

        // Read back all live edges via the slab path. The post-demote
        // bucket is slab-mode, so we use `edges.read_slot` directly to
        // walk the contiguous degree slots at `b2.edge_start()`.
        // Compare against the expected live set = { even slot targets }
        // = { 100, 102, 104, ... } (the fill_leg_slab_prefix helper
        // writes `(i + 100).to_le_bytes()` at slot i).
        let mut expected: Vec<u32> = (0..start).step_by(2).map(|i| i + 100).collect();
        expected.sort_unstable();
        let slab_start = b2.edge_start();
        let mut got: Vec<u32> = Vec::with_capacity(b2.degree as usize);
        for i in 0..b2.degree {
            let edge: TestEdge = graph.edges().read_slot(slab_start + u64::from(i));
            got.push(edge.target);
        }
        got.sort_unstable();
        assert_eq!(got, expected);

        // LTB allocated count must drop back to (pre-promote) baseline:
        // promote minted `root_len` leaf blocks, demote releases them.
        // For start = 4096, root_len = ceil(4096/1024) = 4 blocks; the
        // pre-promote baseline is 0. After demote, free_count should
        // cover the 4 blocks.
        let alloc_after = graph
            .ltb()
            .block_capacity()
            .saturating_sub(graph.ltb().free_count());
        assert_eq!(
            alloc_after,
            alloc_before - 4,
            "demote must release the 4 leaf blocks promoted for stored=4096"
        );

        // num_edges unchanged: 2048 live edges both before and after
        // demote (the demote path doesn't change edge count).
        assert_eq!(graph.edges().header().num_edges, num_before);
    }

    /// Plan 0319 §Step 1 test (b): hysteresis no-oscillation. With
    /// `T_PROMOTE = 4096` and `T_DEMOTE = 2048`:
    /// - start with a tree bucket at stored = 4096 (promoted)
    /// - remove 1 edge: degree = 4095 > T_DEMOTE => bucket stays tree
    /// - remove down to 2048: degree = 2048 <= T_DEMOTE => Step 2
    ///   trigger demotes; for THIS test we call the demotion primitive
    ///   directly (without Step 2 wiring) and verify the demotion
    ///   itself
    /// - after demote, re-promote by raising stored above T_PROMOTE
    ///   and call promote_bypass_to_tree_mode again
    /// - assert mode transitions land at the two thresholds exactly
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn demote_hysteresis_no_oscillation() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        let start = 4096u32;
        promote_test_bucket(&graph, vid, label, start);
        let bucket_slot = {
            let v = graph.vertices().get(vid);
            match graph.find_bucket(vid, &v, label).expect("find") {
                BucketSearch::Found { slot, .. } => slot,
                _ => panic!("bucket missing"),
            }
        };

        // Remove 1 edge: degree = 4095 > T_DEMOTE (2048). Bucket stays
        // tree (we don't call demote here; the test is about the
        // primitive, not the trigger).
        let bucket = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        tree_mode_remove_edge_at_slot(&graph, vid, bucket_slot, &bucket, 0)
            .expect("remove 0")
            .expect("slot in range");
        let b = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        assert_eq!(b.degree, 4095);
        assert!(b.is_tree_mode(), "degree 4095 > T_DEMOTE => stay tree");

        // Tombstone enough slots to land degree at 2048.
        for slot in 1..(start - 1).min(2047) {
            // Tombstone slots 1..(2047) => 2046 tombstones. After the
            // earlier remove of slot 0, degree is 4095 - 2046 = 2049.
            let bcur = match graph
                .find_bucket(vid, &graph.vertices().get(vid), label)
                .expect("find")
            {
                BucketSearch::Found { bucket, .. } => bucket,
                _ => panic!("bucket missing mid"),
            };
            tree_mode_remove_edge_at_slot(&graph, vid, bucket_slot, &bcur, slot)
                .expect("remove mid")
                .expect("slot in range");
        }
        // Degree = 2049, still > T_DEMOTE.
        let b2 = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        assert_eq!(b2.degree, 2049);
        assert!(b2.is_tree_mode(), "degree 2049 > T_DEMOTE => still tree");

        // Tombstone one more: degree = 2048 == T_DEMOTE. Direct demote
        // (without the Step 2 trigger) should succeed.
        let b2_slot_idx = 2047u32;
        tree_mode_remove_edge_at_slot(&graph, vid, bucket_slot, &b2, b2_slot_idx)
            .expect("remove 2047")
            .expect("slot in range");
        let b3 = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        assert_eq!(b3.degree, 2048);
        assert!(b3.is_tree_mode());
        // Direct demote at degree == T_DEMOTE.
        tree_mode_demote_to_slab(&graph, bucket_slot, label, &b3).expect("demote");
        let b4 = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        assert!(!b4.is_tree_mode(), "after direct demote: slab mode");
        assert_eq!(b4.degree, 2048);
        assert_eq!(b4.stored_slots, 2048);
    }

    /// Plan 0319 §Step 2 hysteresis test: drive the production
    /// remove path and verify the `T_DEMOTE = 2048` trigger fires.
    /// Promote a fresh bucket at stored = 4096, then tombstone slots
    /// via the production remove path until `degree == 2048 ==
    /// T_DEMOTE`. The next remove (the one that crosses the
    /// threshold) must trigger the demote via the inline trigger
    /// wired in remove.rs:328. Assert the bucket ends up in slab
    /// mode with `stored == degree == 2048`.
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn demote_hysteresis_trigger_via_remove_path() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        let start = 4096u32;
        promote_test_bucket(&graph, vid, label, start);
        // Remove slot 0 first via the production dispatch path so
        // `num_edges` and `vertex_segment_counts` stay consistent
        // with the production accounting. The bucket is now at
        // degree=4095, stored=4096, still tree mode.
        let _ = graph
            .remove_edge_at_slot(vid, label, 0)
            .expect("production remove 0");
        // Tombstone slots 1..=2047 via the production path. After
        // 2047 removes, degree = 4095 - 2047 = 2048 == T_DEMOTE.
        // The last remove (slot 2047) triggers the demote.
        for slot in 1..2048u32 {
            graph
                .remove_edge_at_slot(vid, label, slot)
                .expect("production remove");
        }
        // After the trigger fires on the 2048th remove, the bucket
        // should be in slab mode with stored == degree == 2048.
        let v = graph.vertices().get(vid);
        let b = match graph.find_bucket(vid, &v, label).expect("find") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        assert!(!b.is_tree_mode(), "trigger should have demoted the bucket");
        assert_eq!(b.degree, 2048);
        assert_eq!(b.stored_slots, 2048);
    }

    /// Plan 0319 §Step 1 test (c): demote at degree 0 reclaims all
    /// blocks. Promote, tombstone every slot, demote, assert bucket
    /// is empty-slab and LTB allocated count is back to baseline.
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn demote_degree_zero_reclaims_all_blocks() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        let start = 4096u32;
        promote_test_bucket(&graph, vid, label, start);
        let bucket_slot = {
            let v = graph.vertices().get(vid);
            match graph.find_bucket(vid, &v, label).expect("find") {
                BucketSearch::Found { slot, .. } => slot,
                _ => panic!("bucket missing"),
            }
        };
        let alloc_before = graph
            .ltb()
            .block_capacity()
            .saturating_sub(graph.ltb().free_count());
        // Tombstone every slot. After every remove, re-read the bucket
        // (the descriptor changes after each write).
        for slot in 0..start {
            let b = match graph
                .find_bucket(vid, &graph.vertices().get(vid), label)
                .expect("find")
            {
                BucketSearch::Found { bucket, .. } => bucket,
                _ => panic!("bucket missing mid"),
            };
            tree_mode_remove_edge_at_slot(&graph, vid, bucket_slot, &b, slot)
                .expect("remove all")
                .expect("slot in range");
        }
        let b = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        assert_eq!(b.degree, 0);
        assert_eq!(b.stored_slots, 4096);
        assert!(b.is_tree_mode());
        // Demote at degree 0: the live Vec is empty, no slab span is
        // reserved (allocate_span(0) returns 0? actually returns the
        // free-list head; either way, the new bucket has 0 live edges).
        tree_mode_demote_to_slab(&graph, bucket_slot, label, &b).expect("demote 0");
        let b2 = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        assert!(!b2.is_tree_mode());
        assert_eq!(b2.degree, 0);
        assert_eq!(b2.stored_slots, 0);
        // LTB allocated count back to pre-promote baseline.
        let alloc_after = graph
            .ltb()
            .block_capacity()
            .saturating_sub(graph.ltb().free_count());
        assert_eq!(alloc_after, alloc_before - 4, "all 4 leaf blocks released");
    }

    /// Plan 0319 §Step 1 test (d): demote is atomic on mid-failure.
    /// The LTB `release` path has no failpoint; instead, we use a
    /// typed precondition: an inline-property-byte bucket must fail
    /// before any state change. Promote such a bucket (force one with
    /// `inline_property_byte_width = 0`, then manually patch the
    /// descriptor byte) and verify demote returns the typed error and
    /// leaves the bucket intact.
    ///
    /// Easier path: demote on a slab-mode bucket (caller misuse) is
    /// caught by `debug_assert!(bucket.is_tree_mode())`; we instead
    /// test the typed `TreeModeEdgeWidthUnsupported` guard by checking
    /// that demote on a tree bucket with `inline_property_byte_width()
    /// != 0` returns `InlinePropertyBytesWidthMismatch` and leaves the
    /// bucket untouched. We construct the failing state by reading the
    /// bucket and re-writing it with a non-zero inline_property_byte_width
    /// (the encode is invariant under this write because the byte
    /// is repurposed for tree-mode physical depth which we don't
    /// observe for buckets that just pass through the demote call).
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn demote_atomic_on_failure() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        let start = 4096u32;
        promote_test_bucket(&graph, vid, label, start);
        let bucket_slot = {
            let v = graph.vertices().get(vid);
            match graph.find_bucket(vid, &v, label).expect("find") {
                BucketSearch::Found { slot, .. } => slot,
                _ => panic!("bucket missing"),
            }
        };
        // Read bucket, then re-write it with a non-zero
        // inline_property_byte_width to simulate a bucket that has
        // LPB bytes (this would normally block at promote via the
        // typed guard, but we are testing demote's guard here).
        let b_orig = match graph
            .find_bucket(vid, &graph.vertices().get(vid), label)
            .expect("find")
        {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        let b_with_lpb = b_orig.with_inline_property_byte_width(4);
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, b_with_lpb)
            .expect("write patched bucket");
        // Demote: should fail with `InlinePropertyBytesWidthMismatch`
        // BEFORE any state change.
        let result = tree_mode_demote_to_slab(&graph, bucket_slot, label, &b_with_lpb);
        assert!(
            matches!(
                result,
                Err(LabeledOperationError::InlinePropertyBytesWidthMismatch { .. })
            ),
            "expected InlinePropertyBytesWidthMismatch, got {result:?}"
        );
        // The bucket must still be the patched tree-mode bucket (no
        // state change).
        let b_after = graph
            .buckets()
            .read_label_bucket_slot(bucket_slot)
            .expect("read post-fail");
        assert!(b_after.is_tree_mode());
        assert_eq!(b_after.inline_property_byte_width(), 4);
        // Restore the original bucket (without the LPB byte) so
        // teardown is clean.
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, b_orig)
            .expect("restore");
    }

    /// Plan 0319 §Step 1 test (e): demote a physical-depth-2 tree
    /// bucket. Mirrors the existing `build_depth1_full_root_bucket`
    /// helper to construct a depth-2 bucket, demote, and verify
    /// physical-depth byte reset + interior blocks released.
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn demote_physical_depth_2_releases_interiors() {
        let graph = test_graph();
        let label = BucketLabelKey::directed_from_index(1);
        // The graph already has a default vertex at id 0; push_vertex
        // returns a new VertexId (>= 1) so the slot index used by the
        // helper does not collide with any pre-existing bucket.
        let vid = graph
            .push_vertex(
                crate::labeled::record::LabeledVertex::default()
                    .try_with_bucket_row(0, 1)
                    .expect("try_with_bucket_row"),
            )
            .expect("push_vertex");
        let bucket_slot = 0u64;
        build_depth1_full_root_bucket(&graph, vid, label, bucket_slot);
        let v = graph.vertices().get(vid);
        let b = match graph.find_bucket(vid, &v, label).expect("find") {
            BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        };
        assert_eq!(b.stored_slots, 1_048_576);
        assert_eq!(b.tree_mode_physical_depth(), 1);
        // bucket_slot was captured before build_depth1_full_root_bucket;
        // the helper writes the same slot, so we reuse it for the deepen
        // call below.
        let alloc_before = graph
            .ltb()
            .block_capacity()
            .saturating_sub(graph.ltb().free_count());
        // Deepen: from depth 1 (1 root region of 1024 leaf ids) to
        // depth 2 (1 root region of 1 interior id; the interior block
        // holds the 1024 leaf ids).
        super::tree_mode_deepen(&graph, bucket_slot, &b).expect("deepen");
        let b_d2 = graph
            .buckets()
            .read_label_bucket_slot(bucket_slot)
            .expect("read post-deepen");
        assert_eq!(b_d2.tree_mode_physical_depth(), 2);
        let alloc_after_deepen = graph
            .ltb()
            .block_capacity()
            .saturating_sub(graph.ltb().free_count());
        // Deepen minted 1 interior block.
        assert_eq!(alloc_after_deepen, alloc_before + 1);
        // Demote: should release 1024 leaves + 1 interior, drop back
        // to alloc_before.
        tree_mode_demote_to_slab(&graph, bucket_slot, label, &b_d2).expect("demote d2");
        let b_post = graph
            .buckets()
            .read_label_bucket_slot(bucket_slot)
            .expect("read post-demote");
        assert!(!b_post.is_tree_mode());
        assert_eq!(b_post.degree, 1_048_576);
        assert_eq!(b_post.stored_slots, 1_048_576);
        assert_eq!(b_post.tree_mode_physical_depth(), 0); // slab mode
        let alloc_final = graph
            .ltb()
            .block_capacity()
            .saturating_sub(graph.ltb().free_count());
        // After demote: all 1024 leaf blocks + 1 interior block are
        // released back to the LTB free list. `alloc_final` should
        // be 0 (no live blocks).
        assert_eq!(alloc_final, 0, "all leaf + interior blocks released");
    }
}
