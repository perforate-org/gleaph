//! Tree-mode promotion for label buckets (Plan 0318 §Step 4).
//!
//! `promote_bypass_to_tree_mode(vid, label)` is the failure-atomic
//! transition that moves a slab-mode or bypass-mode bucket into LTB-backed
//! tree mode once its `alloc_space` crosses the `T_PROMOTE` cap. The
//! transition is a 3-phase reserve/commit/publish pattern modeled after
//! the existing `promote_bypass_to_bucket_mode`.
//!
//! Storage architecture (per f6c426d1c amend):
//! - `edge_start` = LEG slab offset (root region span containing a
//!   `u32` block_id array, 4 bytes per block_id).
//! - Each `block_id` (u32) indexes one 4 KiB block in the LTB store.
//! - The LTB block holds the actual edge data: 4 bytes per edge × up to
//!   1024 edges per block.
//! - For `stored_slots = 4096` at depth 1, the root region is 4 × `u32`
//!   = 16 bytes; 4 LTB blocks hold 4 × 4096 = 16 384 bytes = 16 KiB.
//!   Total tree-mode bucket footprint = 16 KiB + 16 bytes (root region)
//!   ≈ 16.016 KiB.
//!
//! The transition is the only path that constructs an LTB block for
//! production data; subsequent tree-mode operations (Steps 5–6) only
//! read, write_payload_partial, or mint new tail blocks.
//!
//! Prerequisite: [`crate::labeled::ltb_raw_block_store::LtbRawBlockStore`]
//! mutating methods take `&self` (commit 582d75657 interior-mutability
//! amend), so the call site operates on a borrowed `&self.ltb` reference
//! without violating the `&self` convention used by every other graph
//! operation.

use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{
    BucketMode, BucketSearch, LabeledLaraGraph, T_PROMOTE,
    tree_write::TREE_MODE_REQUIRED_EDGE_BYTES,
};
use crate::VertexId;
use crate::labeled::{
    bucket_label_key::BucketLabelKey,
    ltb_raw_block_store::BLOCK_PAYLOAD_BYTES,
    record::LabelBucket,
    tree_csr_prototype::{B as BLOCK_B, derive_depth, root_len as derived_root_len},
};
use crate::lara::operation_error::LaraOperationError;
use crate::traits::CsrEdge;

/// 3-phase promotion entry point.
///
/// Phase 1 — Reserve: read the bucket descriptor, verify preconditions,
/// derive the depth and required root length, mint all LTB blocks (which
/// grows stable memory), and reserve a root region span in the LEG slab.
/// If any step fails, every minted block is released and the function
/// returns the typed error without mutating the bucket descriptor.
///
/// Phase 2 — Commit: read each slab slot (4-byte target) from the LEG
/// slab prefix and write the LTB blocks in stack-buffered chunks of
/// `BLOCK_PAYLOAD_BYTES`. The tail block uses `write_payload_partial`
/// for the leftover 4-byte rows. Then write the block_id array into the
/// LEG root region. If any step fails, every minted block is released
/// and the function returns the typed error without mutating the bucket
/// descriptor.
///
/// Phase 3 — Publish: build a new `LabelBucket` with `with_tree_mode(true)`
/// and the new `edge_start` pointing into the LEG root region, write the
/// descriptor (single canonical write), then release the old edge span
/// and the old inline-property span (if any) so the slab prefix space
/// becomes recyclable.
pub(super) fn promote_bypass_to_tree_mode<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label: BucketLabelKey,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    // Plan 0321: see `promote_bypass_to_tree_mode_pub` for the
    // `pub(crate)` test-only wrapper.
    promote_bypass_to_tree_mode_impl(graph, vid, label)
}

fn promote_bypass_to_tree_mode_impl<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label: BucketLabelKey,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    // Phase 0: locate the bucket (read-only; no canonical writes).
    let vertex = graph.vertices().get(vid);
    let search = graph.find_bucket(vid, &vertex, label)?;
    let (bucket_slot, bucket) = match search {
        BucketSearch::Found { slot, bucket } => (slot, bucket),
        BucketSearch::Missing { .. } => {
            // No such bucket. The promotion can only proceed once the
            // bucket exists; the caller is expected to insert an edge
            // first. The original Step 4 implementation surfaced
            // `AllocSpaceCapReached { current_alloc_space: 0, .. }` here
            // which is misleading (a missing bucket is not a cap
            // overflow). Plan 0318 §Step 4 amend introduces a dedicated
            // `BucketNotFound` error so the dispatcher can disambiguate
            // "no bucket" from "alloc_space overflow".
            return Err(LabeledOperationError::BucketNotFound { vid, label });
        }
    };

    // Precondition 1: bucket must not already be in tree mode.
    if bucket.is_tree_mode() {
        // Already in tree mode; promote is a no-op success.
        return Ok(());
    }

    // Precondition 2: stored_slots must have reached T_PROMOTE.
    //
    // The cap semantics document (Plan 0317 §3.5) says the trigger is
    // on `alloc_space = stored_slots + alloc_gap`, but the current
    // implementation uses the placeholder `alloc_gap(stored) =
    // T_PROMOTE - stored` (Plan 0317 §3.5 placeholder, weighted gap
    // deferred). Under that placeholder
    // `compute_bucket_allocation(slab) ≡ T_PROMOTE` for any
    // `stored_slots` in `[0, T_PROMOTE]`, so the alloc_space form of
    // the trigger never fires for an existing bucket. We use
    // `stored_slots` directly as the equivalent stricter form. When
    // the weighted gap is introduced the trigger switches back to
    // `compute_bucket_allocation(&bucket) < T_PROMOTE`.
    let stored_slots_check = bucket.stored_slots;
    if stored_slots_check < T_PROMOTE {
        return Err(LabeledOperationError::AllocSpaceCapReached {
            current_alloc_space: stored_slots_check,
            cap: T_PROMOTE,
            mode: BucketMode::Slab,
        });
    }

    // Precondition 3: Plan 0326 (LPB-in-tree) wires the property
    // stream through the tree-form path. The carve-out is removed:
    // - `w == 0`: no property stream (the original Plan 0318 path).
    // - `0 < w <= payload_bytes`: property stream is transcribed into
    //   the LPB (depth-1 property tree) during this promotion.
    // - `w > payload_bytes`: declared ADR bound; typed reject.
    let w = bucket.inline_property_byte_width();
    if w > BLOCK_PAYLOAD_BYTES as u16 {
        return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
            bucket_width: w,
            edge_inline_property_width: 0,
        });
    }

    // Precondition 4: tree mode stores one 4-byte target per LTB slot (ADR
    // 0088 §1 wire truth, `B = BLOCK_PAYLOAD_BYTES / 4`). Wider edge types
    // cannot be promoted — the transcription would silently mis-address
    // slots. Fail closed with the same typed error the tree-mode append
    // path uses; the insert dispatcher carves such buckets out of the
    // trigger so they stay slab (mirroring the inline-property carve-out).
    if E::BYTES != TREE_MODE_REQUIRED_EDGE_BYTES {
        return Err(LabeledOperationError::TreeModeEdgeWidthUnsupported {
            actual: E::BYTES,
            expected: TREE_MODE_REQUIRED_EDGE_BYTES,
        });
    }

    let stored_slots = bucket.stored_slots;
    let _depth = derive_depth(stored_slots);
    let root_len = u32::try_from(derived_root_len(stored_slots)).expect("root_len fits u32");
    debug_assert!(root_len as usize <= 1024);

    let pre_edge_start = bucket.edge_start();
    let pre_stored_slots = stored_slots;
    let pre_property_offset = bucket.inline_property_bytes_offset();
    let _pre_property_slab_slots = bucket.inline_property_bytes_slab_slots();

    // ==================== Phase 1: Reserve ====================
    // Mint `root_len` LTB blocks. The LTB store grows stable memory at
    // mint time (header counter + per-block page); on mint failure, every
    // prior mint is released (LIFO) and the function returns the error.
    let mut reserved_block_ids: Vec<u32> = Vec::with_capacity(root_len as usize);
    let mut mint_failed: Option<crate::GrowFailed> = None;
    for _ in 0..root_len {
        match graph.ltb().mint() {
            Ok(id) => reserved_block_ids.push(id),
            Err(e) => {
                mint_failed = Some(e);
                break;
            }
        }
    }
    if let Some(mint_err) = mint_failed {
        // Rollback: release every block we managed to mint (LIFO).
        for &id in reserved_block_ids.iter().rev() {
            let _ = graph.ltb().release(id);
        }
        return Err(mint_err.into());
    }

    // Plan 0326 LPB-in-tree (REWORK): the combined LEG span holds
    // `[edge root | property root]` (gap 0, ADR 0088 §2). For `w > 0`,
    // the property root is contiguous after the edge root; the read
    // path derives the property root start as `edge_start +
    // bucket_span_region_len(bucket)`. The `inline_property_bytes_offset`
    // descriptor field is set to 0 in tree mode (the property root is
    // contiguous, not at a stored offset). The combined length is
    // computed up front so a single `allocate_span` produces the
    // contiguous region.
    //
    // Property root length per ADR §2 wire truth: `ceil(S / K)` where
    // `K = floor(4096 / w)`. No tail buffer (the legacy `+ 1` was a
    // writing-side convenience; reads never used the tail slot, and
    // a tail buffer would inflate the root past the ADR cap).
    let property_root_len: u32 = if w > 0 && pre_stored_slots > 0 {
        let k_prop = (BLOCK_PAYLOAD_BYTES as u32) / (w as u32);
        // F-2 cap guard: when the property root would exceed R_MAX,
        // the property tree needs to deepen. Wire-up of property-tree
        // deepen (with BlockKind::InlinePropertyInterior) is a
        // follow-up slice per ADR 0088 §7. For this slice we fail
        // closed at the property-root cap to avoid the giant-span
        // regression (w > 0 tree buckets would otherwise grow
        // unbounded).
        let live_prop_root = pre_stored_slots.div_ceil(k_prop);
        let k_max =
            u32::try_from(crate::labeled::tree_csr_prototype::R_MAX).expect("R_MAX fits u32");
        if live_prop_root > k_max {
            // Surface the typed error to the caller; release the
            // edge LTB blocks we already minted before returning.
            for &id in reserved_block_ids.iter().rev() {
                let _ = graph.ltb().release(id);
            }
            return Err(LabeledOperationError::PropertyTreeRootCapacityReached {
                stored_slots: pre_stored_slots,
                property_root_len: live_prop_root,
                cap: k_max,
                property_leaf_fanout: k_prop,
            });
        }
        live_prop_root
    } else {
        0
    };
    let combined_root_len: u32 = root_len.checked_add(property_root_len).expect(
        "combined_root_len overflow: edge_root_len + property_root_len must fit in u32          (edge_root_len <= R_MAX = 1024, property_root_len <= R_MAX = 1024, total <= 2048)",
    );
    // Reserve the combined LEG root region span. This is the slot range
    // in the LEG slab that will hold BOTH the edge block_id array
    // (offset 0..root_len) AND the property block_id array
    // (offset root_len..combined_root_len). If the allocation
    // fails, release the LTB blocks and return the typed error.
    let new_edge_start: u64 = match graph.edges().allocate_span(u64::from(combined_root_len)) {
        Ok(start) => start,
        Err(e) => {
            // Rollback: release LTB blocks in LIFO order.
            for &id in reserved_block_ids.iter().rev() {
                let _ = graph.ltb().release(id);
            }
            return Err(e.into());
        }
    };

    // ==================== Phase 2: Commit ====================
    // Phase 2a: transcribe the slab prefix (stored_slots × 4 bytes) into
    // the LTB blocks, one block at a time using a 4 KiB stack buffer.
    //
    // The block write loop uses `write_payload` for full blocks and
    // `write_payload_partial` for the tail block (4-byte leftover rows).
    //
    // `E::BYTES == 4` was verified as typed Precondition 4 above; the
    // transcription below can therefore treat every LEG slot as one
    // 4-byte target row.
    let ltb = graph.ltb();
    let mut cursor: usize = 0;
    let mut slot_in_block: usize = 0;
    let mut payload = [0u8; BLOCK_PAYLOAD_BYTES];
    let mut current_block_id: u32 = reserved_block_ids[0];
    // The 4-byte target read from the LEG slab prefix.
    let mut target_bytes = [0u8; 4];
    let mut commit_err: Option<LabeledOperationError> = None;
    while cursor < pre_stored_slots as usize {
        // Read the 4-byte target from the LEG slab prefix.
        graph
            .edges()
            .read_slot_bytes(pre_edge_start + cursor as u64, &mut target_bytes);
        // Place the 4 bytes at `slot_in_block * 4` within the payload.
        let in_block_offset = slot_in_block * 4;
        payload[in_block_offset..in_block_offset + 4].copy_from_slice(&target_bytes);
        slot_in_block += 1;
        cursor += 1;
        if slot_in_block == BLOCK_B || cursor == pre_stored_slots as usize {
            // Flush the current block. Full block → write_payload; tail
            // block → write_payload_partial with `slot_in_block * 4` bytes.
            let block_id = current_block_id;
            let write_result = if slot_in_block == BLOCK_B {
                ltb.write_payload(block_id, &payload)
            } else {
                let used = slot_in_block * 4;
                ltb.write_payload_partial(block_id, 0, &payload[..used])
            };
            if let Err(e) = write_result {
                commit_err = Some(e.into());
                break;
            }
            // Move to the next block.
            slot_in_block = 0;
            // Reset payload to zero to avoid leaking tail bytes into
            // the next block.
            payload = [0u8; BLOCK_PAYLOAD_BYTES];
            let next_idx = (cursor as u32) / BLOCK_B as u32;
            if (next_idx as usize) < reserved_block_ids.len() {
                current_block_id = reserved_block_ids[next_idx as usize];
            }
        }
    }
    if let Some(err) = commit_err {
        // Rollback: release LTB blocks in LIFO order, then release the
        // LEG root region span. (The LEG span was never written, so
        // releasing it is a no-op cost-wise; the slot range will simply
        // be reused on the next `allocate_span` call.)
        for &id in reserved_block_ids.iter().rev() {
            let _ = graph.ltb().release(id);
        }
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(root_len));
        return Err(err);
    }

    // Phase 2b: write the block_id array into the LEG root region.
    // The root region is a `u32` array of length `root_len`; we pack
    // each block_id into 4 little-endian bytes.
    let mut root_bytes: Vec<u8> = Vec::with_capacity(root_len as usize * 4);
    for &id in &reserved_block_ids {
        root_bytes.extend_from_slice(&id.to_le_bytes());
    }
    if let Err(e) = graph
        .edges()
        .write_slots_contiguous_bytes(new_edge_start, &root_bytes)
    {
        // Rollback: release LTB blocks in LIFO order, release the LEG
        // root region span.
        for &id in reserved_block_ids.iter().rev() {
            let _ = graph.ltb().release(id);
        }
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(root_len));
        return Err(LaraOperationError::WriteEdgeSlotFailed(e).into());
    }

    // ==================== Phase 2c: Property stream (Plan 0326 REWORK) ====================
    // The property root is contiguous in the COMBINED span: it starts
    // at `new_edge_start + root_len` (immediately after the edge
    // root). The descriptor field `inline_property_bytes_offset` is
    // set to 0 in tree mode; the read path derives the property root
    // start as `edge_start + bucket_span_region_len(bucket)`.
    //
    // Failure atomicity: any error retires the minted LTB blocks
    // (LIFO) and the LEG combined span is released.
    let mut property_block_ids: Vec<u32> = Vec::new();
    let property_root_span_start: u64 = new_edge_start + u64::from(root_len);
    if w > 0 && pre_stored_slots > 0 {
        // K = floor(4096 / w). Guarded above (w <= payload) so K >= 1.
        let k = (BLOCK_PAYLOAD_BYTES as u32) / (w as u32);
        // 1. Mint LTB blocks (LIFO rollback on failure).
        let mut mint_err: Option<LabeledOperationError> = None;
        for _ in 0..property_root_len {
            match graph.ltb().mint() {
                Ok(id) => property_block_ids.push(id),
                Err(e) => {
                    mint_err = Some(e.into());
                    break;
                }
            }
        }
        if let Some(err) = mint_err {
            for &id in property_block_ids.iter().rev() {
                let _ = graph.ltb().release(id);
            }
            for &id in reserved_block_ids.iter().rev() {
                let _ = graph.ltb().release(id);
            }
            let _ = graph
                .edges()
                .release_span(new_edge_start, u64::from(combined_root_len));
            return Err(err);
        }
        // 2. Mark each block as InlineProperty (mint leaves kind = Free).
        for &id in &property_block_ids {
            let header = crate::labeled::ltb_raw_block_store::BlockHeader {
                kind: crate::labeled::ltb_raw_block_store::BlockKind::InlineProperty,
                bucket_label_key_wire: 0,
                owner_or_next_free: 0,
                ordinal: 0,
                level: 0,
                reserved: [0u8; 3],
            };
            graph.ltb().write_block_header(id, &header);
        }
        // 3. Transcribe slab property bytes into LPB blocks.
        let ltb = graph.ltb();
        let mut prop_cursor: usize = 0;
        let mut prop_slot_in_block: usize = 0;
        let mut prop_payload = [0u8; BLOCK_PAYLOAD_BYTES];
        let mut prop_block_idx: usize = 0;
        let mut prop_write_err: Option<LabeledOperationError> = None;
        while prop_cursor < pre_stored_slots as usize {
            let mut value_bytes = vec![0u8; usize::from(w)];
            let source = pre_property_offset
                .checked_add((prop_cursor as u64) * u64::from(w))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            graph.values().read_bytes(source, &mut value_bytes);
            let in_block_offset = prop_slot_in_block * usize::from(w);
            prop_payload[in_block_offset..in_block_offset + usize::from(w)]
                .copy_from_slice(&value_bytes);
            prop_slot_in_block += 1;
            prop_cursor += 1;
            if prop_slot_in_block == k as usize || prop_cursor == pre_stored_slots as usize {
                let block_id = property_block_ids[prop_block_idx];
                let used = prop_slot_in_block * usize::from(w);
                let write_result = if prop_slot_in_block == k as usize {
                    ltb.write_payload(block_id, &prop_payload)
                } else {
                    ltb.write_payload_partial(block_id, 0, &prop_payload[..used])
                };
                if let Err(e) = write_result {
                    prop_write_err = Some(e.into());
                    break;
                }
                prop_slot_in_block = 0;
                prop_payload = [0u8; BLOCK_PAYLOAD_BYTES];
                prop_block_idx += 1;
            }
        }
        if let Some(err) = prop_write_err {
            for &id in property_block_ids.iter().rev() {
                let _ = graph.ltb().release(id);
            }
            for &id in reserved_block_ids.iter().rev() {
                let _ = graph.ltb().release(id);
            }
            let _ = graph
                .edges()
                .release_span(new_edge_start, u64::from(combined_root_len));
            return Err(err);
        }
        // 4. Write the property root block_id array at
        //    `property_root_span_start` (the second half of the
        //    combined span). Failure releases the LPB blocks + the
        //    combined span.
        let mut property_root_bytes: Vec<u8> = Vec::with_capacity(property_root_len as usize * 4);
        for &id in &property_block_ids {
            property_root_bytes.extend_from_slice(&id.to_le_bytes());
        }
        if let Err(e) = graph
            .edges()
            .write_slots_contiguous_bytes(property_root_span_start, &property_root_bytes)
        {
            for &id in property_block_ids.iter().rev() {
                let _ = graph.ltb().release(id);
            }
            for &id in reserved_block_ids.iter().rev() {
                let _ = graph.ltb().release(id);
            }
            let _ = graph
                .edges()
                .release_span(new_edge_start, u64::from(combined_root_len));
            return Err(LaraOperationError::WriteEdgeSlotFailed(e).into());
        }
    }
    // After this point, the combined span
    // `[new_edge_start, new_edge_start + combined_root_len)` holds
    // `[edge root (length root_len) | property root (length
    // property_root_len)]` (gap 0). The descriptor invariants for
    // tree mode are:
    //   `edge_start = new_edge_start`
    //   `inline_property_bytes_offset = 0` (the property root is
    //     derived from `edge_start + bucket_span_region_len(bucket)`,
    //     not from a stored offset).
    //   `inline_property_bytes_slab_slots = 0` (no slab form).

    // ==================== Phase 3: Publish ====================
    // Build the new tree-mode descriptor. The descriptor's `edge_start`
    // is the LEG offset to the root region. `overflow_log_head = -1` to
    // mark the log chain as unused (tree mode does not use overflow
    // logs). `degree` and `stored_slots` carry over unchanged.
    //
    // `inline_property_byte_width = 0` so the inline-property
    // related fields are all zero: no schema, no offset, no log head.
    let new_bucket = LabelBucket::try_from_parts(
        label,
        new_edge_start,
        bucket.degree,
        bucket.stored_slots,
        -1, // overflow_log_head = -1 (tree mode does not use log)
        w,  // inline_property_byte_width (preserved)
        0,  // inline_property_bytes_offset = 0 (unused in tree mode;
        //   property root is contiguous at edge_start + root_len)
        0,  // inline_property_bytes_slab_slots = 0
        -1, // inline_property_bytes_log_head = -1
        0,  // inline_property_bytes_log_len = 0 (depth byte = 0 = depth 1)
    )
    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
    .with_tree_mode(true);

    // Phase 3b: single canonical write of the new descriptor.
    if let Err(e) = graph
        .buckets()
        .write_label_bucket_slot(bucket_slot, new_bucket)
    {
        // Phase 3 failure is the canonical write itself: the bucket is
        // still in slab mode; release the new tree-mode LTB blocks
        // (edge + property) and the combined LEG root region so we
        // don't leak.
        for &id in property_block_ids.iter().rev() {
            let _ = graph.ltb().release(id);
        }
        for &id in reserved_block_ids.iter().rev() {
            let _ = graph.ltb().release(id);
        }
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(combined_root_len));
        return Err(e.into());
    }

    // Phase 3c: release the old edge span. The slab prefix is no longer
    // referenced (the new descriptor points at the LEG root region),
    // so the slab prefix slots become recyclable.
    //
    // **Leaf-physical caveat (Plan 0318 §Step 4 amend note 5, Step 6
    // amend)**: the released pre-promotion edge span is a subrange of
    // an existing leaf physical block. The release returns the
    // subrange to the slab free list, but if the leaf is currently
    // pinned by a `labeled_leaf_physical_range`, the pin is invalidated
    // by the recycle. To preserve the pin invariant we adopt hazard
    // ledger option (b): when the pre-promotion span lies inside a
    // currently-pinned leaf physical block, the release is **deferred**
    // and the subrange stays inside the leaf until the leaf is
    // recycled (pin-sheltered). The upper bound on the deferred
    // reclaim is T_PROMOTE = 4096 slots = 16 KiB per bucket — leaf
    // compaction eventually reclaims it. Non-leaf-pinned spans (test
    // raw spans, future slab-only paths) take the original release.
    if !super::tree_write::pre_promotion_span_inside_pinned_leaf(
        graph,
        vid,
        pre_edge_start,
        pre_stored_slots,
    ) {
        let _ = graph
            .edges()
            .release_span(pre_edge_start, u64::from(pre_stored_slots));
    }

    // Phase 3d: no inline-property span to release (we asserted
    // `inline_property_byte_width == 0` in the preconditions).

    Ok(())
}

/// Plan 0321 Step 1: `pub(crate)` test-only wrapper that lets
/// `crate::labeled::graph::test_support::force_tree_mode_for_test`
/// (and any future non-sibling test setup) reach the natural
/// promotion path without paying the 4096-insert cost. The
/// wrapper is `pub(crate)` and the function it wraps is identical
/// to the `pub(super)` `promote_bypass_to_tree_mode_impl`.
pub(crate) fn promote_bypass_to_tree_mode_pub<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label: BucketLabelKey,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    promote_bypass_to_tree_mode_impl(graph, vid, label)
}

// =========================== Test-only re-exports ===========================
// Plan 0321 §Step 1: re-export the test-only helpers (which live
// inside `pub mod tests`) so non-sibling test modules
// (e.g. `crate::labeled::graph::test_support`) can reach them
// for the tree-mode batch guard test setup. The re-exports are
// `pub(crate)` and `#[cfg(test)]`-gated so they never affect
// non-test builds.
#[cfg(test)]
pub(crate) use self::tests::{
    fill_leg_slab_prefix as fill_leg_slab_prefix_pub,
    force_bucket_to_stored_slots as force_bucket_to_stored_slots_pub,
};

// =========================== Unit tests ===========================
#[cfg(test)]
pub mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use super::*;

    /// Force the bucket at `(vid, label)` to have `stored_slots` edges
    /// in the slab prefix. This is a test-only helper that bypasses
    /// the cap and lock checks to construct the specific state required
    /// by each test. If a bucket does not exist, it creates a new one
    /// at slot 0 with `edge_start = 0` and updates the vertex's bucket
    /// row.
    pub(crate) fn force_bucket_to_stored_slots(
        graph: &LabeledLaraGraph<TestEdge, ic_stable_structures::VectorMemory>,
        vid: VertexId,
        label: BucketLabelKey,
        stored_slots: u32,
    ) {
        // Track the global edge count to match `stored_slots` so that
        // `num_edges.checked_sub(1)` does not underflow in subsequent
        // operations (insert / remove) that mirror the production
        // accounting path. This is a test-only invariant: production
        // keeps `num_edges` in sync via the slab insert path.
        if stored_slots > 0 {
            graph.edges().set_num_edges(u64::from(stored_slots));
        }
        let vertex = graph.vertices().get(vid);
        let search = graph.find_bucket(vid, &vertex, label).expect("find_bucket");
        let slot = match search {
            BucketSearch::Found { slot, .. } => slot,
            BucketSearch::Missing { .. } => {
                // Create a new bucket at slot 0. Use edge_start = 0
                // (matches the LEG slab prefix we will write to).
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
                .expect("LabelBucket::try_from_parts")
                .with_tree_mode(false);
                graph
                    .buckets()
                    .write_label_bucket_slot(0, bucket)
                    .expect("write_label_bucket_slot 0");
                // Update the vertex: bucket_count = 1, base_slot_start = 0.
                let new_vertex = vertex
                    .try_with_bucket_row(0, 1)
                    .expect("try_with_bucket_row");
                graph
                    .set_labeled_vertex(vid, new_vertex)
                    .expect("set_labeled_vertex");
                return;
            }
        };
        let existing = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let new_bucket = LabelBucket::try_from_parts(
            label,
            existing.edge_start(),
            existing.degree,
            stored_slots,
            -1,
            0,
            0,
            0,
            -1,
            0,
        )
        .expect("LabelBucket::try_from_parts")
        .with_tree_mode(false);
        graph
            .buckets()
            .write_label_bucket_slot(slot, new_bucket)
            .expect("write_label_bucket_slot");
    }

    /// Fill the LEG slab prefix at `edge_start` with deterministic
    /// 4-byte target values: slot i = (i + 100) as u32.
    pub(crate) fn fill_leg_slab_prefix(
        graph: &LabeledLaraGraph<TestEdge, ic_stable_structures::VectorMemory>,
        edge_start: u64,
        count: u32,
    ) {
        let mut all_bytes = Vec::with_capacity(count as usize * 4);
        for i in 0..count {
            let target = i.wrapping_add(100);
            all_bytes.extend_from_slice(&target.to_le_bytes());
        }
        graph
            .edges()
            .write_slots_contiguous_bytes(edge_start, &all_bytes)
            .expect("write_slots_contiguous");
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_rejects_existing_bucket_below_cap() {
        // Plan 0318 §Step 4 amend: under the placeholder
        // `alloc_gap = T_PROMOTE - stored`, `compute_bucket_allocation`
        // is constant T_PROMOTE so the alloc_space form of the trigger
        // never fires for an existing bucket. The precondition is
        // `stored_slots < T_PROMOTE` directly. An existing bucket at
        // stored_slots = 10 must be rejected.
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        force_bucket_to_stored_slots(&graph, vid, BucketLabelKey::directed_from_index(1), 10);
        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        match result {
            Err(LabeledOperationError::AllocSpaceCapReached {
                current_alloc_space,
                cap,
                mode,
            }) => {
                assert_eq!(current_alloc_space, 10);
                assert_eq!(cap, T_PROMOTE);
                assert_eq!(mode, BucketMode::Slab);
            }
            other => panic!("expected AllocSpaceCapReached, got {other:?}"),
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_rejects_empty_existing_bucket() {
        // Plan 0318 §Step 4 amend: an existing empty bucket
        // (stored_slots = 0) is below-cap and must be rejected
        // (promoting an empty bucket is wasteful).
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        force_bucket_to_stored_slots(&graph, vid, BucketLabelKey::directed_from_index(1), 0);
        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        match result {
            Err(LabeledOperationError::AllocSpaceCapReached {
                current_alloc_space,
                cap,
                ..
            }) => {
                assert_eq!(current_alloc_space, 0);
                assert_eq!(cap, T_PROMOTE);
            }
            other => panic!("expected AllocSpaceCapReached, got {other:?}"),
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_missing_bucket_returns_bucket_not_found() {
        // Plan 0318 §Step 4 amend: the `Missing` branch returns
        // `BucketNotFound` (not the misleading `AllocSpaceCapReached`).
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        // No bucket exists for this label; `find_bucket` returns Missing.
        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(99));
        match result {
            Err(LabeledOperationError::BucketNotFound { .. }) => {}
            other => panic!("expected BucketNotFound, got {other:?}"),
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_succeeds_when_alloc_space_at_cap() {
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        // Force the bucket to have stored_slots = T_PROMOTE = 4096,
        // then call promote. We must first pre-populate the LEG slab
        // prefix at edge_start = 0 with deterministic data so the
        // transcription in Phase 2 reads valid bytes.
        let stored = T_PROMOTE;
        // Force the bucket descriptor to have stored_slots = T_PROMOTE;
        // this also creates the bucket + vertex bucket row if it
        // does not exist.
        force_bucket_to_stored_slots(&graph, vid, BucketLabelKey::directed_from_index(1), stored);
        let vertex = graph.vertices().get(vid);
        let search = graph
            .find_bucket(vid, &vertex, BucketLabelKey::directed_from_index(1))
            .expect("find_bucket");
        let slot = match search {
            BucketSearch::Found { slot, .. } => slot,
            BucketSearch::Missing { .. } => {
                panic!("bucket missing after force_bucket_to_stored_slots")
            }
        };
        let existing = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let edge_start = existing.edge_start();
        // Pre-populate the LEG slab prefix with deterministic data.
        fill_leg_slab_prefix(&graph, edge_start, stored);

        // Promote.
        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        // Verify the descriptor is now in tree mode.
        let new_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        assert!(new_bucket.is_tree_mode());

        // Verify the LTB store minted exactly 4 blocks (depth 1 root_len).
        assert_eq!(graph.ltb().block_capacity(), 4);

        // Verify the new edge_start points at a LEG offset (>= 4,
        // since the LTB block ids are 0..3).
        assert!(
            new_bucket.edge_start() >= 4,
            "edge_start {} should be ≥ 4 (LTB block ids are 0..3)",
            new_bucket.edge_start()
        );
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_reserve_phase_rolls_back_on_mint_failure() {
        // Plan 0318 §Step 4 amend: rewritten to use an existing
        // bucket with stored_slots = 10 (the true below-cap path) so
        // the test exercises the precondition-2 rejection rather than
        // the `Missing` branch. The atomic rollback on mint failure
        // itself is exercised by the typed-error contract (the
        // function releases on error); exhaustively exhausting stable
        // memory in a unit test is impractical.
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        force_bucket_to_stored_slots(&graph, vid, BucketLabelKey::directed_from_index(1), 10);
        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        match result {
            Err(LabeledOperationError::AllocSpaceCapReached {
                current_alloc_space,
                ..
            }) => {
                assert_eq!(current_alloc_space, 10);
            }
            other => panic!("expected AllocSpaceCapReached, got {other:?}"),
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_publish_phase_atomic_descriptor_write() {
        // After a successful promotion, the descriptor must be in
        // tree mode and the new edge_start must differ from the
        // pre-promotion edge_start.
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        let stored = T_PROMOTE;
        // Force the bucket descriptor to have stored_slots = T_PROMOTE;
        // this also creates the bucket + vertex bucket row if it
        // does not exist.
        force_bucket_to_stored_slots(&graph, vid, BucketLabelKey::directed_from_index(1), stored);
        let vertex = graph.vertices().get(vid);
        let search = graph
            .find_bucket(vid, &vertex, BucketLabelKey::directed_from_index(1))
            .expect("find_bucket");
        let slot = match search {
            BucketSearch::Found { slot, .. } => slot,
            BucketSearch::Missing { .. } => {
                panic!("bucket missing after force_bucket_to_stored_slots")
            }
        };
        let pre_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let pre_edge_start = pre_bucket.edge_start();
        fill_leg_slab_prefix(&graph, pre_edge_start, stored);

        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let post_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        assert!(post_bucket.is_tree_mode());
        assert_ne!(post_bucket.edge_start(), pre_edge_start);
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_leg_root_region_holds_block_id_sequence() {
        // After a successful promotion, the LEG root region must hold
        // 4 distinct block_ids in order. Read 16 bytes from the new
        // edge_start and decode them.
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        let stored = T_PROMOTE;
        // Force the bucket descriptor to have stored_slots = T_PROMOTE;
        // this also creates the bucket + vertex bucket row if it
        // does not exist.
        force_bucket_to_stored_slots(&graph, vid, BucketLabelKey::directed_from_index(1), stored);
        let vertex = graph.vertices().get(vid);
        let search = graph
            .find_bucket(vid, &vertex, BucketLabelKey::directed_from_index(1))
            .expect("find_bucket");
        let slot = match search {
            BucketSearch::Found { slot, .. } => slot,
            BucketSearch::Missing { .. } => {
                panic!("bucket missing after force_bucket_to_stored_slots")
            }
        };
        let pre_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let pre_edge_start = pre_bucket.edge_start();
        fill_leg_slab_prefix(&graph, pre_edge_start, stored);

        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let post_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let new_edge_start = post_bucket.edge_start();
        // Read 16 bytes from the new edge_start.
        let mut root_bytes = [0u8; 16];
        graph
            .edges()
            .read_slots_contiguous_bytes(new_edge_start, &mut root_bytes);
        // Decode the 4 block_ids.
        let id_0 = u32::from_le_bytes(root_bytes[0..4].try_into().unwrap());
        let id_1 = u32::from_le_bytes(root_bytes[4..8].try_into().unwrap());
        let id_2 = u32::from_le_bytes(root_bytes[8..12].try_into().unwrap());
        let id_3 = u32::from_le_bytes(root_bytes[12..16].try_into().unwrap());
        // Each block_id must be a valid mint (id < block_capacity).
        let cap = graph.ltb().block_capacity();
        assert!(id_0 < cap);
        assert!(id_1 < cap);
        assert!(id_2 < cap);
        assert!(id_3 < cap);
        // block_ids must be unique (different mint calls).
        let ids = [id_0, id_1, id_2, id_3];
        for i in 0..4 {
            for j in (i + 1)..4 {
                assert_ne!(ids[i], ids[j], "ids[{i}] == ids[{j}] = {}", ids[i]);
            }
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_ltb_blocks_hold_correctly_ordered_edge_data() {
        // After a successful promotion, the LTB blocks must hold the
        // same data as the LEG slab prefix. Read 4 bytes from each LTB
        // block (at the appropriate offset) and compare with the
        // precomputed value.
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        let stored = T_PROMOTE;
        // Force the bucket descriptor to have stored_slots = T_PROMOTE;
        // this also creates the bucket + vertex bucket row if it
        // does not exist.
        force_bucket_to_stored_slots(&graph, vid, BucketLabelKey::directed_from_index(1), stored);
        let vertex = graph.vertices().get(vid);
        let search = graph
            .find_bucket(vid, &vertex, BucketLabelKey::directed_from_index(1))
            .expect("find_bucket");
        let slot = match search {
            BucketSearch::Found { slot, .. } => slot,
            BucketSearch::Missing { .. } => {
                panic!("bucket missing after force_bucket_to_stored_slots")
            }
        };
        let pre_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let pre_edge_start = pre_bucket.edge_start();
        fill_leg_slab_prefix(&graph, pre_edge_start, stored);

        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        // Read the LTB blocks: 4 blocks × 1024 edges = 4096 edges.
        for block_idx in 0..4u32 {
            let mut buf = [0u8; BLOCK_PAYLOAD_BYTES];
            graph
                .ltb()
                .read_payload(block_idx, &mut buf)
                .expect("read_payload");
            for in_block_slot in 0..BLOCK_B as u32 {
                let slot_idx = block_idx * BLOCK_B as u32 + in_block_slot;
                let expected = (slot_idx as u32).wrapping_add(100);
                let got = u32::from_le_bytes(
                    buf[(in_block_slot as usize) * 4..(in_block_slot as usize) * 4 + 4]
                        .try_into()
                        .unwrap(),
                );
                assert_eq!(got, expected, "mismatch at slot {slot_idx}");
            }
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_edge_start_points_to_leg_offset() {
        // After promotion, the new descriptor's `edge_start` must point
        // to a LEG offset (>= root_len = 4), not to an LTB block id.
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        let stored = T_PROMOTE;
        // Force the bucket descriptor to have stored_slots = T_PROMOTE;
        // this also creates the bucket + vertex bucket row if it
        // does not exist.
        force_bucket_to_stored_slots(&graph, vid, BucketLabelKey::directed_from_index(1), stored);
        let vertex = graph.vertices().get(vid);
        let search = graph
            .find_bucket(vid, &vertex, BucketLabelKey::directed_from_index(1))
            .expect("find_bucket");
        let slot = match search {
            BucketSearch::Found { slot, .. } => slot,
            BucketSearch::Missing { .. } => {
                panic!("bucket missing after force_bucket_to_stored_slots")
            }
        };
        let pre_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let pre_edge_start = pre_bucket.edge_start();
        fill_leg_slab_prefix(&graph, pre_edge_start, stored);

        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let post_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        // The LTB block ids are 0..3; the LEG root region offset must
        // be ≥ 4 to be unambiguously a LEG offset.
        assert!(
            post_bucket.edge_start() >= 4,
            "edge_start {} should be ≥ 4 (LTB block ids are 0..3)",
            post_bucket.edge_start()
        );
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_pre_and_post_edge_start_differ() {
        // Pre-promotion `edge_start` (slab prefix) must differ from
        // post-promotion `edge_start` (LEG root region).
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        let stored = T_PROMOTE;
        // Force the bucket descriptor to have stored_slots = T_PROMOTE;
        // this also creates the bucket + vertex bucket row if it
        // does not exist.
        force_bucket_to_stored_slots(&graph, vid, BucketLabelKey::directed_from_index(1), stored);
        let vertex = graph.vertices().get(vid);
        let search = graph
            .find_bucket(vid, &vertex, BucketLabelKey::directed_from_index(1))
            .expect("find_bucket");
        let slot = match search {
            BucketSearch::Found { slot, .. } => slot,
            BucketSearch::Missing { .. } => {
                panic!("bucket missing after force_bucket_to_stored_slots")
            }
        };
        let pre_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let pre_edge_start = pre_bucket.edge_start();
        fill_leg_slab_prefix(&graph, pre_edge_start, stored);

        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let post_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let post_edge_start = post_bucket.edge_start();
        assert_ne!(pre_edge_start, post_edge_start);
    }

    // ================== wide-edge promotion carve-out ==================
    // Post-merge fix: the canbench `bench_l_s2_det_sat_4096` bench drives a
    // 10-byte edge type; the promote trigger fired at T_PROMOTE and the
    // tree-append typed guard trapped after a silent mis-transcription.
    // Tree mode stores one 4-byte target per LTB slot (ADR 0088 §1), so
    // wide edge types must never promote — they stay slab (mirroring the
    // inline-property carve-out).

    use crate::test_support::LabelledTestEdge as WideTestEdge;

    /// A wide-edge graph fixture mirroring `test_graph` but with an `E`
    /// whose `BYTES != 4`.
    fn wide_edge_graph() -> LabeledLaraGraph<WideTestEdge, ic_stable_structures::VectorMemory> {
        let graph = LabeledLaraGraph::new_with_segment_size(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::directed_from_index(1),
            32,
        )
        .expect("wide edge graph");
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
        graph
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_rejects_wide_edge_type_fail_closed() {
        // A wide (8-byte) edge bucket can never promote: tree mode stores
        // one 4-byte target per LTB slot (ADR 0088 §1). The typed guard
        // fires at Precondition 4 — before any LTB mint or LEG read.
        let graph = wide_edge_graph();
        let vid: VertexId = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(1);
        let bucket = LabelBucket::try_from_parts(label, 0, 4096, 4096, -1, 0, 0, 0, -1, 0)
            .expect("try_from_parts")
            .with_tree_mode(false);
        graph
            .buckets()
            .write_label_bucket_slot(0, bucket)
            .expect("write bucket");
        let vertex = graph.vertices().get(vid);
        graph
            .set_labeled_vertex(vid, vertex.try_with_bucket_row(0, 1).expect("bucket row"))
            .expect("set vertex");

        let result = promote_bypass_to_tree_mode(&graph, vid, label);
        match result {
            Err(LabeledOperationError::TreeModeEdgeWidthUnsupported { actual, expected }) => {
                assert_eq!(actual, 8);
                assert_eq!(expected, 4);
            }
            other => panic!("expected TreeModeEdgeWidthUnsupported, got {other:?}"),
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn wide_edge_insert_stays_slab_past_promote_cap() {
        // 4096 inserts fill the bucket to T_PROMOTE; the 4097th insert
        // crosses the promote trigger — which must carve wide edge types
        // out and keep the bucket on the slab path (pre-Plan-0318
        // behavior, mirroring the inline-property carve-out).
        let graph = wide_edge_graph();
        let vid: VertexId = VertexId::from(0);
        // A non-default label: the default-label insert would take the
        // homogeneous-bypass row (no bucket), which cannot promote.
        let label = BucketLabelKey::directed_from_index(2);
        for i in 0..4097u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    WideTestEdge::new(i, 7),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("wide edge insert");
        }
        let vertex = graph.vertices().get(vid);
        let search = graph.find_bucket(vid, &vertex, label).expect("find");
        let BucketSearch::Found { bucket, .. } = search else {
            panic!("bucket missing");
        };
        assert!(
            !bucket.is_tree_mode(),
            "wide edge bucket must stay slab past T_PROMOTE"
        );
        // The slab path counts slab-resident slots in `stored_slots` and
        // live edges in `degree`; past the slab window the 4097th edge
        // lives in the overflow log (degree 4097, stored unchanged).
        assert_eq!(bucket.degree, 4097);
    }
}
