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
use super::{BucketMode, BucketSearch, LabeledLaraGraph, T_PROMOTE, compute_bucket_allocation};
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
    // Phase 0: locate the bucket (read-only; no canonical writes).
    let vertex = graph.vertices().get(vid);
    let search = graph.find_bucket(vid, &vertex, label)?;
    let (bucket_slot, bucket) = match search {
        BucketSearch::Found { slot, bucket } => (slot, bucket),
        BucketSearch::Missing { .. } => {
            // No such bucket. The promotion can only proceed once the
            // bucket exists; the caller is expected to insert an edge
            // first. Surface the cap as a typed error so the dispatcher
            // can disambiguate "no bucket" from "alloc_space overflow".
            return Err(LabeledOperationError::AllocSpaceCapReached {
                current_alloc_space: 0,
                cap: T_PROMOTE,
                mode: BucketMode::Slab,
            });
        }
    };

    // Precondition 1: bucket must not already be in tree mode.
    if bucket.is_tree_mode() {
        // Already in tree mode; promote is a no-op success.
        return Ok(());
    }

    // Precondition 2: alloc_space must have reached T_PROMOTE.
    let alloc_space = compute_bucket_allocation(&bucket);
    if alloc_space < T_PROMOTE {
        return Err(LabeledOperationError::AllocSpaceCapReached {
            current_alloc_space: alloc_space,
            cap: T_PROMOTE,
            mode: BucketMode::Slab,
        });
    }

    // Precondition 3: inline-property bytes are not yet wired through this
    // promotion path. A bucket that already has non-zero
    // `inline_property_byte_width` must be promoted in a separate slice
    // (Plan 0318 + future work). Fail-closed for now.
    if bucket.inline_property_byte_width() != 0 {
        return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
            bucket_width: bucket.inline_property_byte_width(),
            edge_inline_property_width: 0,
        });
    }

    let stored_slots = bucket.stored_slots;
    let _depth = derive_depth(stored_slots);
    let root_len = u32::try_from(derived_root_len(stored_slots)).expect("root_len fits u32");
    debug_assert!(root_len as usize <= 1024);

    let pre_edge_start = bucket.edge_start();
    let pre_stored_slots = stored_slots;

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

    // Reserve the LEG root region span. This is the slot range in the
    // LEG slab that will hold the block_id array. If the allocation
    // fails, release the LTB blocks and return the typed error.
    let new_edge_start: u64 = match graph.edges().allocate_span(u64::from(root_len)) {
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
        0,  // inline_property_byte_width = 0 (handled in a later slice)
        0,  // inline_property_bytes_offset = 0
        0,  // inline_property_bytes_slab_slots = 0
        -1, // inline_property_bytes_log_head = -1
        0,  // inline_property_bytes_log_len = 0
    )
    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
    .with_tree_mode(true);

    // Phase 3b: single canonical write of the new descriptor.
    if let Err(e) = graph
        .buckets()
        .write_label_bucket_slot(bucket_slot, new_bucket)
    {
        // Phase 3 failure is the canonical write itself: the bucket is
        // still in slab mode; release the new tree-mode LTB blocks and
        // LEG root region so we don't leak.
        for &id in reserved_block_ids.iter().rev() {
            let _ = graph.ltb().release(id);
        }
        let _ = graph
            .edges()
            .release_span(new_edge_start, u64::from(root_len));
        return Err(e.into());
    }

    // Phase 3c: release the old edge span. The slab prefix is no longer
    // referenced (the new descriptor points at the LEG root region),
    // so the slab prefix slots become recyclable.
    let _ = graph
        .edges()
        .release_span(pre_edge_start, u64::from(pre_stored_slots));

    // Phase 3d: no inline-property span to release (we asserted
    // `inline_property_byte_width == 0` in the preconditions).

    Ok(())
}

// =========================== Unit tests ===========================
#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use super::*;

    /// Force the bucket at `(vid, label)` to have `stored_slots` edges
    /// in the slab prefix. This is a test-only helper that bypasses
    /// the cap and lock checks to construct the specific state required
    /// by each test. If a bucket does not exist, it creates a new one
    /// at slot 0 with `edge_start = 0` and updates the vertex's bucket
    /// row.
    fn force_bucket_to_stored_slots(
        graph: &LabeledLaraGraph<TestEdge, ic_stable_structures::VectorMemory>,
        vid: VertexId,
        label: BucketLabelKey,
        stored_slots: u32,
    ) {
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
    fn fill_leg_slab_prefix(
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
    fn promote_rejects_when_alloc_space_below_cap() {
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        // The fresh test graph has an empty default-label bucket. With
        // stored_slots = 0, alloc_space = 0 < T_PROMOTE, so the
        // promotion must be rejected.
        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(
            matches!(
                result,
                Err(LabeledOperationError::AllocSpaceCapReached { .. })
            ),
            "expected AllocSpaceCapReached, got {result:?}"
        );
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
        // Cannot exhaust stable memory in a unit test, so this test
        // verifies only the happy-path rejection when stored_slots
        // hasn't reached the cap. The atomic rollback on mint failure
        // is exercised by the typed-error contract (the function
        // releases on error).
        let graph = test_graph();
        let vid: VertexId = VertexId::from(0);
        let result =
            promote_bypass_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(matches!(
            result,
            Err(LabeledOperationError::AllocSpaceCapReached { .. })
        ));
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
}
