//! Tree-mode read dispatch for label buckets (Plan 0318 §Step 5).
//!
//! Implements the read side of the tree-mode bucket backing. The single
//! dispatch point is [`super::traverse::LabeledLaraGraph::visit_edges_for_label_impl`],
//! which routes a tree-mode bucket (`LabelBucket::is_tree_mode() == true`)
//! to [`visit_tree_mode_label_bucket_edges`] instead of the slab path.
//! No branch lives in rope / PMA / placement / leaf-pin code.
//!
//! Storage architecture (per `f6c426d1c` amend and Step 4 commit `44c82d3b2`):
//! - `LabelBucket::edge_start` = LEG slab offset; the slot range
//!   `[edge_start, edge_start + root_len)` holds the **root region**, a
//!   dense `u32` block_id array.
//! - Each `block_id` indexes one 4 KiB block in the LTB store. A block's
//!   payload is `B = 1024` 4-byte rows (edges) in insertion order.
//! - `root_len = ceil(stored_slots / B^depth)` where
//!   `depth = derive_depth(stored_slots)`. The tail block may be
//!   partial (gap-0 invariant): the valid byte count is
//!   `(stored_slots - first_slot) * E::BYTES`.
//! - Logical slot `i` → `(block_root_index = i / B, in_block_offset =
//!   (i % B) * E::BYTES)`. Tombstone filtering is the upper layer's
//!   job; this module yields raw 4-byte targets only.
//!
//! Production read API:
//! - [`super::traverse::LabeledLaraGraph::visit_edges_for_label_impl`]
//!   is the single dispatch point. A tree-mode bucket is walked via
//!   [`visit_tree_mode_label_bucket_edges`]; a slab-mode bucket keeps
//!   its existing path (no mode branch leaks elsewhere).
//! - [`tree_mode_random_ordinal_access`] is the production-side
//!   `random_ordinal_access` (used by `CounterpartScan` and similar
//!   pair-ordinal lookups). Reads exactly 4 bytes via
//!   `LtbRawBlockStore::read_payload_partial`.
//!
//! Prerequisite: `E::BYTES == 4` is asserted at every tree-mode read
//! entry. A typed guard (returning a `LabeledOperationError` instead of
//! panicking in debug) is deferred to Plan 0318 §Step 6 alongside the
//! other tree-mode invariants.

use ic_stable_structures::Memory;

use super::{LabeledLaraGraph, OutEdgeOrder};
use crate::VertexId;
use crate::labeled::graph::error::LabeledOperationError;
use crate::labeled::record::LabelBucket;
use crate::labeled::tree_csr_prototype::{B as BLOCK_B, root_len as derived_root_len};
use crate::lara::operation_error::LaraOperationError;
use crate::traits::CsrEdge;

/// Read the 4-byte target at logical slot `slot` of a tree-mode bucket.
///
/// Returns `Ok(Some(target))` for `slot < stored_slots`, `Ok(None)` for
/// `slot >= stored_slots`, and an error for invariants violated (e.g.
/// `inline_property_byte_width != 0` is currently rejected). This is the
/// production-side `random_ordinal_access` (Plan 0318 §Step 5).
pub(crate) fn tree_mode_random_ordinal_access<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label_id: u16,
    slot: u32,
) -> Result<Option<u32>, LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    debug_assert_eq!(
        E::BYTES,
        4,
        "tree_mode_random_ordinal_access requires E::BYTES == 4"
    );
    let vertex = graph.vertices().get(vid);
    let bucket = match graph.find_bucket(vid, &vertex, super::BucketLabelKey::from_raw(label_id))? {
        super::BucketSearch::Found { bucket, .. } => bucket,
        super::BucketSearch::Missing { .. } => return Ok(None),
    };
    debug_assert!(
        bucket.is_tree_mode(),
        "tree_mode_random_ordinal_access: bucket is not in tree mode (caller must dispatch)"
    );
    if slot >= bucket.stored_slots {
        return Ok(None);
    }
    if bucket.inline_property_byte_width() != 0 {
        return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
            bucket_width: bucket.inline_property_byte_width(),
            edge_inline_property_width: 0,
        });
    }
    // 1. Compute (block_root_index, in_block_offset) from slot.
    let _depth = bucket.tree_mode_physical_depth();
    let root_len = u32::try_from(derived_root_len(bucket.stored_slots))
        .expect("root_len fits u32 (per ADR 0088 §4 R_max = 1024)");
    let block_root_index = slot / (BLOCK_B as u32);
    debug_assert!(block_root_index < root_len);
    let in_block_offset = (slot % (BLOCK_B as u32)) * (E::BYTES as u32);
    // 2. Resolve the leaf block_id via the depth-generic resolver.
    //    For depth 1 this is a single-hop LEG read; for depth 2+ it
    //    descends the interior hop chain.
    let block_id =
        super::tree_write::resolve_leaf_block_id::<E, M>(graph, &bucket, block_root_index)?;
    // 3. Read 4 bytes from the LTB block at `in_block_offset`.
    let mut target_bytes = [0u8; 4];
    graph
        .ltb()
        .read_payload_partial(block_id, in_block_offset as usize, &mut target_bytes)
        .map_err(LabeledOperationError::LtbBlock)?;
    Ok(Some(u32::from_le_bytes(target_bytes)))
}

/// Visit every logical slot of a tree-mode bucket, yielding
/// `(slot, target)` pairs to `visit` in the requested `order`.
///
/// `out_degree` is the live-edge count (the bucket's `degree()`); the
/// `visit` callback receives the slot index (in iteration order, not
/// physical order) and the 4-byte target. Tombstone filtering is the
/// caller's responsibility; this helper yields the raw targets.
pub(crate) fn visit_tree_mode_label_bucket_edges<E, M, Visit>(
    graph: &LabeledLaraGraph<E, M>,
    label_raw: u16,
    bucket: &LabelBucket,
    out_degree: u32,
    order: OutEdgeOrder,
    mut visit: Visit,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
    Visit: FnMut(u32, E),
{
    debug_assert_eq!(
        E::BYTES,
        4,
        "visit_tree_mode_label_bucket_edges requires E::BYTES == 4"
    );
    debug_assert!(bucket.is_tree_mode());
    if out_degree == 0 {
        return Ok(());
    }
    if bucket.inline_property_byte_width() != 0 {
        // Step 4's promote is fail-closed on inline-property bytes; the
        // tree-mode read path is not wired to handle them yet (Step 6
        // will revisit alongside the tree-mode write dispatch).
        return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
            bucket_width: bucket.inline_property_byte_width(),
            edge_inline_property_width: 0,
        });
    }
    let stored_slots = bucket.stored_slots;
    let _depth = bucket.tree_mode_physical_depth();
    let root_len = u32::try_from(derived_root_len(stored_slots))
        .expect("root_len fits u32 (per ADR 0088 §4 R_max = 1024)");

    // Walk the leaf blocks in the requested order. For each block, yield
    // every valid slot in the requested scan order.
    //
    // Ascending: leaf[0], leaf[1], ..., leaf[leaf_count-1]; within each
    // block: slots 0..block_valid_count.
    //
    // Descending: leaf[leaf_count-1], ..., leaf[0]; within each block:
    // slots block_valid_count-1..0.
    //
    // For depth 1 the leaf array IS the root array, so the walk is
    // identical to the prior single-hop read. For depth 2+ the resolver
    // descends the interior hop chain to find each leaf.
    let leaf_count = u32::try_from(
        (u64::from(stored_slots)).div_ceil(crate::labeled::tree_csr_prototype::B as u64),
    )
    .expect("leaf_count fits u32 for MAX_DEPTH=3");
    debug_assert_eq!(
        leaf_count, root_len,
        "depth 1 leaf_count == root_len; for depth 2+ leaf_count > root_len"
    );
    match order {
        OutEdgeOrder::Ascending => {
            for block_index in 0..leaf_count {
                let block_id =
                    super::tree_write::resolve_leaf_block_id::<E, M>(graph, bucket, block_index)?;
                let block_first_slot = u64::from(block_index) * BLOCK_B as u64;
                let remaining_slots =
                    (u64::from(stored_slots) - block_first_slot).min(BLOCK_B as u64);
                let mut payload = [0u8; ltb_payload_bytes_const()];
                graph
                    .ltb()
                    .read_payload(block_id, &mut payload)
                    .map_err(LabeledOperationError::LtbBlock)?;
                for slot_in_block in 0..remaining_slots as u32 {
                    let byte = (slot_in_block as usize) * E::BYTES;
                    let edge =
                        E::read_from(&payload[byte..byte + E::BYTES]).with_label_id(label_raw);
                    visit(block_first_slot as u32 + slot_in_block, edge);
                }
            }
        }
        OutEdgeOrder::Descending => {
            for block_index in (0..leaf_count).rev() {
                let block_id =
                    super::tree_write::resolve_leaf_block_id::<E, M>(graph, bucket, block_index)?;
                let block_first_slot = u64::from(block_index) * BLOCK_B as u64;
                let remaining_slots =
                    (u64::from(stored_slots) - block_first_slot).min(BLOCK_B as u64);
                let mut payload = [0u8; ltb_payload_bytes_const()];
                graph
                    .ltb()
                    .read_payload(block_id, &mut payload)
                    .map_err(LabeledOperationError::LtbBlock)?;
                for slot_in_block in (0..remaining_slots as u32).rev() {
                    let byte = (slot_in_block as usize) * E::BYTES;
                    let edge =
                        E::read_from(&payload[byte..byte + E::BYTES]).with_label_id(label_raw);
                    visit(block_first_slot as u32 + slot_in_block, edge);
                }
            }
        }
    }
    Ok(())
}

/// Tree-mode out-edge collector: materializes every live slot of a
/// tree-mode bucket into a `Vec<E>` in the requested order. This is the
/// production counterpart of the prototype's
/// `for_each_ascending` / `for_each_descending`. The result includes
/// the `with_label_id` adapter so callers can dispatch without
/// re-applying it.
pub(crate) fn tree_mode_out_edges_collect<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    label_raw: u16,
    bucket: &LabelBucket,
    out_degree: u32,
    order: OutEdgeOrder,
) -> Result<Vec<E>, LabeledOperationError>
where
    E: CsrEdge,
    M: Memory,
{
    let mut out: Vec<E> = Vec::new();
    out.try_reserve_exact(out_degree as usize)
        .map_err(|_| LabeledOperationError::from(LaraOperationError::CollectAllocationOverflow))?;
    visit_tree_mode_label_bucket_edges(
        graph,
        label_raw,
        bucket,
        out_degree,
        order,
        |_slot, edge| {
            out.push(edge);
        },
    )?;
    Ok(out)
}

/// Constant-time accessor for the LTB payload byte count, used to
/// size the stack `payload` buffer in [`visit_tree_mode_label_bucket_edges`].
const fn ltb_payload_bytes_const() -> usize {
    BLOCK_B * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::graph::test_support::TestEdge;
    use ic_stable_structures::VectorMemory;

    fn make_test_graph() -> LabeledLaraGraph<TestEdge, VectorMemory> {
        crate::labeled::graph::test_support::test_graph()
    }

    fn promote_bucket(
        graph: &LabeledLaraGraph<TestEdge, VectorMemory>,
        vid: VertexId,
        stored: u32,
    ) {
        // Pre-populate the LEG slab prefix with deterministic 4-byte targets
        // (slot i = i + 100) and the bucket descriptor (stored_slots =
        // `stored`). Then call the production promote path.
        use crate::labeled::graph::promote;
        let label = BucketLabelKey::directed_from_index(1);
        let vertex = graph.vertices().get(vid);
        let search = graph.find_bucket(vid, &vertex, label).expect("find_bucket");
        let slot = match search {
            super::super::BucketSearch::Found { slot, .. } => slot,
            super::super::BucketSearch::Missing { .. } => {
                // The Missing path also exercises BucketNotFound; for the
                // happy-path tests below we use `force_bucket_to_stored_slots`
                // (defined in `super::promote::tests`). Reach into the test
                // helper via the public crate.
                promote::tests::force_bucket_to_stored_slots(graph, vid, label, stored);
                let vertex = graph.vertices().get(vid);
                let search = graph
                    .find_bucket(vid, &vertex, label)
                    .expect("find_bucket 2");
                let super::super::BucketSearch::Found { slot, .. } = search else {
                    panic!("bucket missing after force_bucket_to_stored_slots");
                };
                slot
            }
        };
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let edge_start = bucket.edge_start();
        promote::tests::fill_leg_slab_prefix(graph, edge_start, stored);
        promote::tests::force_bucket_to_stored_slots(graph, vid, label, stored);
        // Promotion itself.
        promote::promote_bypass_to_tree_mode(graph, vid, label).expect("promote");
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_read_random_slot_access_matches() {
        let graph = make_test_graph();
        let vid = VertexId::from(0);
        let stored: u32 = 4096;
        promote_bucket(&graph, vid, stored);
        // Read 0, 1, 1023, 1024, 4095 (block-boundary crossing).
        let label_raw = BucketLabelKey::directed_from_index(1).raw();
        for &slot in &[0u32, 1, 1023, 1024, 4095] {
            let v = tree_mode_random_ordinal_access(&graph, vid, label_raw, slot)
                .expect("random_ordinal_access")
                .expect("slot in range");
            assert_eq!(v, slot + 100, "slot {slot} mismatch");
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_read_out_of_range_returns_none() {
        let graph = make_test_graph();
        let vid = VertexId::from(0);
        let stored: u32 = 4096;
        promote_bucket(&graph, vid, stored);
        let label_raw = BucketLabelKey::directed_from_index(1).raw();
        let v = tree_mode_random_ordinal_access(&graph, vid, label_raw, stored).expect("err");
        assert!(v.is_none());
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_read_collect_matches_promoted_data() {
        let graph = make_test_graph();
        let vid = VertexId::from(0);
        let stored: u32 = 4096;
        promote_bucket(&graph, vid, stored);
        let vertex = graph.vertices().get(vid);
        let label = BucketLabelKey::directed_from_index(1);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
            super::super::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket not found"),
        };
        let collected: Vec<TestEdge> = tree_mode_out_edges_collect(
            &graph,
            label.raw(),
            &bucket,
            bucket.degree,
            OutEdgeOrder::Ascending,
        )
        .expect("collect");
        assert_eq!(collected.len(), 4096);
        for (i, edge) in collected.iter().enumerate() {
            assert_eq!(edge.target, (i as u32) + 100, "slot {i} mismatch");
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_read_tail_block_trimmed_at_stored_slots() {
        let graph = make_test_graph();
        let vid = VertexId::from(0);
        // 4596 % 1024 = 520 tail slots.
        let stored: u32 = 4596;
        promote_bucket(&graph, vid, stored);
        let vertex = graph.vertices().get(vid);
        let label = BucketLabelKey::directed_from_index(1);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
            super::super::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket not found"),
        };
        let collected = tree_mode_out_edges_collect(
            &graph,
            label.raw(),
            &bucket,
            bucket.degree,
            OutEdgeOrder::Ascending,
        )
        .expect("collect");
        assert_eq!(collected.len(), 4596);
        for (i, edge) in collected.iter().enumerate() {
            assert_eq!(edge.target, (i as u32) + 100, "slot {i} mismatch");
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_visit_edges_descending_order() {
        let graph = make_test_graph();
        let vid = VertexId::from(0);
        let stored: u32 = 4096;
        promote_bucket(&graph, vid, stored);
        let vertex = graph.vertices().get(vid);
        let label = BucketLabelKey::directed_from_index(1);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
            super::super::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket not found"),
        };
        let collected = tree_mode_out_edges_collect(
            &graph,
            label.raw(),
            &bucket,
            bucket.degree,
            OutEdgeOrder::Descending,
        )
        .expect("collect descending");
        assert_eq!(collected.len(), 4096);
        for (i, edge) in collected.iter().enumerate() {
            let expected_slot = 4096 - 1 - i as u32;
            assert_eq!(edge.target, expected_slot + 100, "desc slot {i} mismatch");
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_read_after_promote_preserves_bucket_count_and_degree() {
        let graph = make_test_graph();
        let vid = VertexId::from(0);
        let stored: u32 = 4096;
        promote_bucket(&graph, vid, stored);
        let vertex = graph.vertices().get(vid);
        let label = BucketLabelKey::directed_from_index(1);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
            super::super::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket not found"),
        };
        // After promote: tree mode, stored_slots = 4096, bucket.degree
        // remains the pre-promotion degree (promote doesn't change the
        // logical edge count, only the storage backing).
        assert!(bucket.is_tree_mode());
        assert_eq!(bucket.stored_slots, 4096);
        let collected = tree_mode_out_edges_collect(
            &graph,
            label.raw(),
            &bucket,
            bucket.degree,
            OutEdgeOrder::Ascending,
        )
        .expect("collect");
        assert_eq!(collected.len() as u32, bucket.degree);
    }

    /// Regression test for GAP-2026-09-02-001: `graph.visit_edges` on a
    /// tree-mode bucket must NOT use the dense slab bulk-read path
    /// (which would read from the LEG root region instead of the LTB
    /// payload blocks and trap with `VectorMemory::read: out of bounds`).
    /// The dispatch at `traverse.rs:786` routes tree-mode visits through
    /// `visit_tree_mode_label_bucket_edges` (the LTB walker). This test
    /// exercises that path end-to-end through the public `visit_edges` API.
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tree_visit_edges_via_public_api_works_on_dense_tree_bucket() {
        use crate::OutEdgeOrder;
        use std::ops::ControlFlow;

        let graph = make_test_graph();
        let vid = VertexId::from(0);
        let stored: u32 = 4096;
        promote_bucket(&graph, vid, stored);
        let vertex = graph.vertices().get(vid);
        let label = BucketLabelKey::directed_from_index(1);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find_bucket") {
            super::super::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket not found"),
        };
        // dense tree bucket precondition: degree == stored_slots, no log.
        assert!(bucket.is_tree_mode());
        assert_eq!(bucket.degree, stored);

        // Descending via public `visit_edges` (regression: this used to trap).
        // In descending order the FIRST visit is the highest slot (target = 100 + stored - 1)
        // and the LAST visit is the lowest slot (target = 100).
        let mut count_desc: u32 = 0;
        let mut first_target_desc: u32 = 0;
        let mut last_target_desc: u32 = 0;
        let _ = graph
            .visit_edges(vid, label, OutEdgeOrder::Descending, |_slot, edge| {
                if count_desc == 0 {
                    first_target_desc = edge.target;
                }
                last_target_desc = edge.target;
                count_desc = count_desc.saturating_add(1);
                ControlFlow::<()>::Continue(())
            })
            .expect("visit_edges descending");
        assert_eq!(count_desc, stored, "descending must visit all stored edges");
        assert_eq!(
            first_target_desc,
            100 + stored - 1,
            "first visit in desc is highest slot"
        );
        assert_eq!(last_target_desc, 100, "last visit in desc is lowest slot");

        // Ascending via public `visit_edges` (regression: same trap).
        let mut count_asc: u32 = 0;
        let mut first_target_asc: u32 = 0;
        let mut last_target_asc: u32 = 0;
        let _ = graph
            .visit_edges(vid, label, OutEdgeOrder::Ascending, |_slot, edge| {
                if count_asc == 0 {
                    first_target_asc = edge.target;
                }
                last_target_asc = edge.target;
                count_asc = count_asc.saturating_add(1);
                ControlFlow::<()>::Continue(())
            })
            .expect("visit_edges ascending");
        assert_eq!(count_asc, stored, "ascending must visit all stored edges");
        assert_eq!(first_target_asc, 100, "first visit in asc is lowest slot");
        assert_eq!(
            last_target_asc,
            100 + stored - 1,
            "last visit in asc is highest slot"
        );
    }

    /// Regression test for the tree-mode position contract (ADR 0088 §2):
    /// `BucketEntryPosition` is a tombstone-inclusive bucket-local slot.
    /// The previous `visit_edges` fix used `tree_mode_out_edges_collect` +
    /// `enumerate()`, which yields **live-ordinal** positions (compressed
    /// to 0..degree), violating the contract for tombstoned tree buckets.
    /// This test promotes a 4096-slot tree bucket, tombstones slot 100,
    /// then verifies that `visit_edges`:
    ///   1. yields positions in the **tombstone-inclusive** range [0, 4096)
    ///      (i.e. position 100 is yielded even though it holds a tombstone),
    ///   2. visits 4096 slots (not 4095) because the visit must include the
    ///      tombstoned slot,
    ///   3. the slot 100 visitor sees a tombstone edge (target =
    ///      `EDGE_TOMBSTONE_SENTINEL`).
    /// For tombstoned tree buckets, the live-only fix would have
    /// position 100 mapped to 100 in the live-ordinal space (skipping
    /// over the tombstone in the wrong direction).
    #[test]
    #[cfg(not(feature = "canbench"))]
    fn tombstoned_tree_bucket_visit_preserves_logical_positions() {
        use crate::OutEdgeOrder;
        use crate::VertexId;
        use crate::traits::CsrEdgeTombstone;
        use std::ops::ControlFlow;

        let graph = make_test_graph();
        let vid = VertexId::from(0);
        let stored: u32 = 4096;
        promote_bucket(&graph, vid, stored);
        let vertex = graph.vertices().get(vid);
        let label = BucketLabelKey::directed_from_index(1);
        let (slot_idx, bucket) = match graph.find_bucket(vid, &vertex, label).expect("find") {
            super::super::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("bucket not found"),
        };
        assert!(bucket.is_tree_mode());
        assert_eq!(bucket.degree, stored);
        assert_eq!(bucket.stored_slots, stored);

        // Tombstone slot 100 via the production tree-mode remove path.
        super::super::tree_write::tree_mode_remove_edge_at_slot(
            &graph, vid, slot_idx, &bucket, 100,
        )
        .expect("remove")
        .expect("slot 100 in range");
        // Re-read the bucket: degree decreased, stored_slots unchanged.
        let vertex = graph.vertices().get(vid);
        let bucket_after = match graph.find_bucket(vid, &vertex, label).expect("find after") {
            super::super::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing after remove"),
        };
        assert_eq!(bucket_after.degree, stored - 1, "degree drops by 1");
        assert_eq!(
            bucket_after.stored_slots, stored,
            "stored_slots is tombstone-inclusive (unchanged by tombstone)"
        );

        // Visit ascending. Expect 4096 visits (not 4095): slot 100 must
        // be included even though it holds a tombstone, AND the position
        // yielded must be 100 (tombstone-inclusive), not a live-ordinal
        // offset that would skip past it.
        let mut visited_positions: Vec<u32> = Vec::new();
        let mut tombstone_seen_at: Option<u32> = None;
        let _ = graph
            .visit_edges(vid, label, OutEdgeOrder::Ascending, |slot, edge| {
                if edge.is_tombstone_edge() && tombstone_seen_at.is_none() {
                    tombstone_seen_at = Some(slot.raw());
                }
                visited_positions.push(slot.raw());
                ControlFlow::<()>::Continue(())
            })
            .expect("visit_edges ascending");
        assert_eq!(
            visited_positions.len() as u32,
            stored,
            "ascending must visit all stored slots (tombstone-inclusive)"
        );
        assert_eq!(
            tombstone_seen_at,
            Some(100),
            "tombstone at slot 100 must be visited at position 100 (tombstone-inclusive contract)"
        );
        // All positions 0..stored appear exactly once.
        let mut sorted: Vec<u32> = visited_positions.clone();
        sorted.sort_unstable();
        let expected: Vec<u32> = (0..stored).collect();
        assert_eq!(
            sorted, expected,
            "positions must cover 0..stored exactly once"
        );

        // Visit descending. Expect 4096 visits, tombstone at position 100.
        let mut visited_positions_desc: Vec<u32> = Vec::new();
        let mut tombstone_seen_desc: Option<u32> = None;
        let _ = graph
            .visit_edges(vid, label, OutEdgeOrder::Descending, |slot, edge| {
                if edge.is_tombstone_edge() && tombstone_seen_desc.is_none() {
                    tombstone_seen_desc = Some(slot.raw());
                }
                visited_positions_desc.push(slot.raw());
                ControlFlow::<()>::Continue(())
            })
            .expect("visit_edges descending");
        assert_eq!(
            visited_positions_desc.len() as u32,
            stored,
            "descending must visit all stored slots (tombstone-inclusive)"
        );
        assert_eq!(
            tombstone_seen_desc,
            Some(100),
            "tombstone at slot 100 must be visited at position 100 in descending order too"
        );
    }
}
