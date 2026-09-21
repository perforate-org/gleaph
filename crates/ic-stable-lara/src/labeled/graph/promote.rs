//! Ordinal-preserving slab-bucket promotion to native edge/property trees.
//!
//! Admission uses the original initialized edge prefix plus used spill rows,
//! including tombstones. Target roots and blocks remain unpublished until both
//! streams are transcribed. Source runs and spans are retired only after publication.
//! Bypass conversion and public capacity-signal integration are separate callers.

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
    tree_csr::{B as BLOCK_B, derive_depth, root_len as derived_root_len},
};
use crate::lara::operation_error::LaraOperationError;
use crate::traits::{CsrEdgeTombstone, CsrVertex};

/// Promote an admitted bucket without compacting its source edge or value plane.
/// Native tree tombstones preserve the original high-water slot extent; sidecars
/// are resolved through the source value plane's independent live-order mapping.
/// Recoverable preparation failures release every target block and the root span.
pub(super) fn promote_bucket_to_tree_mode<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label: BucketLabelKey,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    // Plan 0321: see `promote_bucket_to_tree_mode_pub` for the
    // `pub(crate)` test-only wrapper.
    promote_bucket_to_tree_mode_impl(graph, vid, label)
}

/// Copies a bucket's spill run into its slab prefix and releases the run, growing the
/// vertex span once and retrying when the prefix has no room for it yet.
///
/// This is the only shape that moves a run's rows back into the span (ADR 0097 §7): a
/// relocation keeps the run, so every caller that needs the rows in the prefix - the
/// wide-edge carve-out that cannot promote because tree mode stores one 4-byte
/// target per slot - goes through here rather than
/// through a rewrite. The growth step replaces the pre-swap fold's copy: it asks the
/// rebalance for room (`preferred_extra` = the run's rows) while keeping the run where it
/// is, and the retry then performs the explicit compaction that fills it.
pub(super) fn compact_run_into_prefix_with_growth_retry<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    bucket_index: u32,
    bucket: LabelBucket,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    let spill_rows = match graph.edges().spill_mint(bucket.edge_spill_run()) {
        Ok(run) => graph.edges().spill_used_len(run),
        Err(_) => 0,
    };
    match graph.compact_label_bucket_overflow_to_slab(vid, bucket_index, bucket) {
        Ok(_) => Ok(()),
        Err(LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)) => {
            graph.rebalance_vertex_edge_span(vid, Some(bucket_index), spill_rows, true)?;
            let grown_vertex = graph.vertices().get(vid);
            let grown_slot =
                LabeledLaraGraph::<E, M>::labeled_vertex_bucket_slot(&grown_vertex, bucket_index)?;
            let grown = graph
                .buckets()
                .read_label_bucket_slot(grown_slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            graph.compact_label_bucket_overflow_to_slab(vid, bucket_index, grown)?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn promote_bucket_to_tree_mode_impl<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label: BucketLabelKey,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    let vertex = graph.vertices().get(vid);
    let BucketSearch::Found {
        slot: bucket_slot,
        bucket,
    } = graph.find_bucket(vid, &vertex, label)?
    else {
        return Err(LabeledOperationError::BucketNotFound { vid, label });
    };
    if bucket.is_tree_mode() {
        return Ok(());
    }
    transcribe_and_publish_tree(graph, vid, bucket, None, |target| {
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, target)?;
        Ok(())
    })
}

/// All target allocations stay owned here until the caller's prepared publication succeeds.
/// The optional edge is an insertion committed together with bypass-to-tree conversion.
fn transcribe_and_publish_tree<E: CsrEdgeTombstone, M: Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    bucket: LabelBucket,
    pending: Option<&E>,
    publish: impl FnOnce(LabelBucket) -> Result<(), LabeledOperationError>,
) -> Result<(), LabeledOperationError> {
    use crate::labeled::ltb_raw_block_store::{BlockHeader, BlockKind};
    use crate::traits::NO_SPILL_RUN;
    use std::ops::ControlFlow;
    let label = bucket.bucket_label_key();
    let edge_run = if bucket.edge_spill_run() == NO_SPILL_RUN {
        None
    } else {
        Some(graph.edges().spill_mint(bucket.edge_spill_run())?)
    };
    let prefix = graph.edges().initialized_slab_slots(&bucket)?;
    let resident = prefix
        .checked_add(edge_run.map_or(0, |run| graph.edges().spill_used_len(run)))
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
    if resident < T_PROMOTE {
        return Err(LabeledOperationError::AllocSpaceCapReached {
            current_alloc_space: resident,
            cap: T_PROMOTE,
            mode: BucketMode::Slab,
        });
    }
    let source_extent = resident;
    let added = u32::from(pending.is_some());
    let resident = resident
        .checked_add(added)
        .ok_or(LaraOperationError::RowDegreeOverflow)?;
    let degree = bucket
        .degree
        .checked_add(added)
        .ok_or(LaraOperationError::RowDegreeOverflow)?;
    let width = bucket.inline_property_byte_width();
    if usize::from(width) > BLOCK_PAYLOAD_BYTES {
        return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
            bucket_width: width,
            edge_inline_property_width: 0,
        });
    }
    if E::BYTES != TREE_MODE_REQUIRED_EDGE_BYTES {
        return Err(LabeledOperationError::TreeModeEdgeWidthUnsupported {
            actual: E::BYTES,
            expected: TREE_MODE_REQUIRED_EDGE_BYTES,
        });
    }
    let value_run = if bucket.values_spill_run() == NO_SPILL_RUN {
        None
    } else {
        Some(graph.values().spill_mint(bucket.values_spill_run())?)
    };
    let depth = derive_depth(resident);
    let edge_root_len = u32::try_from(derived_root_len(resident))
        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
    let property_slots_per_block = if width == 0 {
        0
    } else {
        BLOCK_PAYLOAD_BYTES as u32 / u32::from(width)
    };
    let property_leaves = if width == 0 {
        0
    } else {
        resident.div_ceil(property_slots_per_block)
    };
    let property_depth = if width == 0 {
        1
    } else {
        super::tree_write::property_depth_for_leaves(
            u64::from(property_leaves),
            property_slots_per_block,
            resident,
        )?
    };
    let property_root_len = u32::try_from(super::tree_write::property_root_entry_count(
        u64::from(property_leaves),
        property_depth,
    ))
    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
    let combined_len = edge_root_len
        .checked_add(property_root_len)
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
    // Construct all descriptor fields before reserving target storage. Only its root address
    // will change after allocation; the old descriptor and both source planes remain owned.
    let template = LabelBucket::try_from_parts(label, 0, degree, resident, width, 0, 0, -1, 0)
        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
        .with_tree_mode(true)
        .with_tree_mode_property_depth(property_depth);
    graph.edges().vertex_segment_counts_index(vid)?;
    graph.edges().free_span_store().reserve_for_releases(2)?;
    graph.values().reserve_retired_byte_spans(1)?;
    let new_start = graph.edges().allocate_span(u64::from(combined_len))?;
    let mut allocated_blocks = Vec::new();
    let prepared = (|| -> Result<(), LabeledOperationError> {
        let base_header = BlockHeader {
            kind: BlockKind::Edge,
            bucket_label_key_wire: 0,
            owner_or_next_free: 0,
            ordinal: 0,
            level: 0,
            tombstone_count: 0,
            reserved: [0; 1],
        };
        let mut mint = |header: BlockHeader, bytes: &[u8]| -> Result<u32, LabeledOperationError> {
            let id = graph.ltb().mint()?;
            allocated_blocks.push(id);
            graph.ltb().write_block_header(id, &header);
            graph.ltb().write_payload_partial(id, 0, bytes)?;
            Ok(id)
        };
        let mut edge_ids = Vec::new();
        let mut property_ids = Vec::new();
        let mut edge_payload = [0u8; BLOCK_PAYLOAD_BYTES];
        let mut property_payload = [0u8; BLOCK_PAYLOAD_BYTES];
        let mut dead = 0u16;
        let tombstone = E::tombstone_edge();
        // Source slots are explicit; missing ordinals are emitted as native tree tombstones.
        // The source visitor resolves values through their own live-order prefix/run mapping.
        let mut emit = |slot: u32, edge: &E, value: &[u8]| -> Result<(), LabeledOperationError> {
            let epos = slot as usize % BLOCK_B;
            edge.write_to(&mut edge_payload[epos * 4..epos * 4 + 4]);
            dead += u16::from(edge.is_deleted_slot());
            if epos + 1 == BLOCK_B || slot + 1 == resident {
                edge_ids.push(mint(
                    BlockHeader {
                        tombstone_count: dead,
                        ..base_header
                    },
                    &edge_payload[..(epos + 1) * 4],
                )?);
                dead = 0;
            }
            if width > 0 {
                let ppos = (slot % property_slots_per_block) as usize * usize::from(width);
                let dst = &mut property_payload[ppos..ppos + usize::from(width)];
                if edge.is_deleted_slot() {
                    dst.fill(0);
                } else {
                    if value.len() != usize::from(width) {
                        return Err(LaraOperationError::CollectAllocationOverflow.into());
                    }
                    dst.copy_from_slice(value);
                }
                if slot % property_slots_per_block + 1 == property_slots_per_block
                    || slot + 1 == resident
                {
                    property_ids.push(mint(
                        BlockHeader {
                            kind: BlockKind::InlineProperty,
                            ..base_header
                        },
                        &property_payload[..ppos + usize::from(width)],
                    )?);
                }
            }
            Ok(())
        };
        let mut next = 0;
        let mut live = 0u32;
        let flow = graph.visit_edges_with_inline_property(
            vid,
            label,
            super::OutEdgeOrder::Ascending,
            |slot, item| {
                let result = (|| -> Result<(), LabeledOperationError> {
                    let slot = slot.raw();
                    if slot < next || slot >= source_extent {
                        return Err(LaraOperationError::CollectAllocationOverflow.into());
                    }
                    while next < slot {
                        emit(next, &tombstone, &[])?;
                        next += 1;
                    }
                    emit(slot, &item.edge, item.inline_property.bytes())?;
                    next += 1;
                    live += 1;
                    Ok(())
                })();
                match result {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(error) => ControlFlow::Break(error),
                }
            },
        )?;
        if let ControlFlow::Break(error) = flow {
            return Err(error);
        }
        if live != bucket.degree {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        while next < source_extent {
            emit(next, &tombstone, &[])?;
            next += 1;
        }

        if let Some(edge) = pending {
            emit(next, edge, &[])?;
        }

        // The same native id-block encoding builds either stream's unpublished interior levels.
        let mut root = |mut ids: Vec<u32>,
                        depth: u32,
                        kind: BlockKind|
         -> Result<Vec<u8>, LabeledOperationError> {
            for level in 1..depth {
                let mut parents = Vec::new();
                for chunk in ids.chunks(BLOCK_B) {
                    let bytes: Vec<_> = chunk.iter().flat_map(|id| id.to_le_bytes()).collect();
                    let header_level = if kind == BlockKind::InlinePropertyInterior {
                        level + 1
                    } else {
                        level
                    };
                    parents.push(mint(
                        BlockHeader {
                            kind,
                            level: header_level as u8,
                            ..base_header
                        },
                        &bytes,
                    )?);
                }
                ids = parents;
            }
            Ok(ids.iter().flat_map(|id| id.to_le_bytes()).collect())
        };
        let edge_root = root(edge_ids, depth, BlockKind::EdgeInterior)?;
        let property_root = root(
            property_ids,
            property_depth,
            BlockKind::InlinePropertyInterior,
        )?;
        if edge_root.len() != edge_root_len as usize * 4
            || property_root.len() != property_root_len as usize * 4
        {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        graph
            .edges()
            .write_slots_contiguous_bytes(new_start, &edge_root)
            .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
        if !property_root.is_empty() {
            graph
                .edges()
                .write_slots_contiguous_bytes(new_start + u64::from(edge_root_len), &property_root)
                .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
        }
        publish(template.with_base_slot_start(new_start))?;
        Ok(())
    })();
    if let Err(error) = prepared {
        for id in allocated_blocks.into_iter().rev() {
            graph
                .ltb()
                .release(id)
                .expect("unpublished promotion block must remain owned");
        }
        graph
            .edges()
            .release_span(new_start, u64::from(combined_len))
            .expect("promotion rollback capacity was reserved");
        return Err(error);
    }
    // No recoverable errors after publication. Only old allocations are retired here.
    graph
        .edges()
        .bump_vertex_segment_counts(vid, -i64::from(bucket.degree), 0)
        .expect("promotion counts were preflighted");
    if let Some(run) = edge_run {
        graph
            .edges()
            .spill_release(run)
            .expect("source edge run was validated");
    }
    if let Some(run) = value_run {
        graph
            .values()
            .spill_release(run)
            .expect("source values run was validated");
    }
    if !super::tree_write::pre_promotion_span_inside_pinned_leaf(
        graph,
        vid,
        bucket.edge_start(),
        prefix,
    ) {
        graph
            .edges()
            .release_span(bucket.edge_start(), u64::from(prefix))
            .expect("source prefix retirement was reserved");
    }
    graph
        .release_bucket_inline_property_bytes_span(vid, &bucket)
        .expect("source value retirement was reserved");
    Ok(())
}

/// Prepare the tree and pending append while the original bypass is canonical.
pub(super) fn promote_bypass_with_edge<E: CsrEdgeTombstone, M: Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    edge: &E,
) -> Result<(), LabeledOperationError> {
    let vertex = graph.vertices().get(vid);
    let source = graph.bypass_bucket_view(&vertex)?;
    let next_count = graph
        .edges()
        .header()
        .num_edges
        .checked_add(1)
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
    transcribe_and_publish_tree(graph, vid, source, Some(edge), |target| {
        let new_alloc = super::compact::combined_span_region_len(&target);
        let mut plan = graph.buckets().plan_promote_bypass_to_bucket_mode(
            graph.vertices(),
            vid,
            target,
            new_alloc,
        )?;
        graph
            .buckets()
            .reserve_promote_bypass_to_bucket_mode(&mut plan)?;
        // Reservation is the last recoverable failure. Publish the final tree directly,
        // never an intermediate bucket descriptor referring to the original run.
        graph
            .buckets()
            .commit_promote_bypass_to_bucket_mode(graph.vertices(), plan)
            .expect("reserved bypass publication");
        graph
            .invalidate_bucket_lookup_caches_for_bucket_segment(vid)
            .expect("preflighted bucket segment");
        graph
            .edges()
            .bump_vertex_segment_counts(vid, 0, i64::from(new_alloc))
            .expect("preflighted counts row");
        graph.edges().set_num_edges(next_count);
        Ok(())
    })
}

/// Plan 0321 Step 1: `pub(crate)` test-only wrapper that lets
/// `crate::labeled::graph::test_support::force_tree_mode_for_test`
/// (and any future non-sibling test setup) reach the natural
/// promotion path without paying the 4096-insert cost. The
/// wrapper is `pub(crate)` and the function it wraps is identical
/// to the `pub(super)` `promote_bucket_to_tree_mode_impl`.
pub(crate) fn promote_bucket_to_tree_mode_pub<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label: BucketLabelKey,
) -> Result<(), LabeledOperationError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    promote_bucket_to_tree_mode_impl(graph, vid, label)
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
                // Leaf PMA `actual` counts live slab edges (ADR 0096 §5), so the
                // fabricated descriptor must be visible there too — otherwise no
                // test using this helper could audit (or detect) the promote /
                // demote leaf-count arithmetic.
                graph
                    .edges()
                    .bump_vertex_segment_counts(vid, i64::from(stored_slots), 0)
                    .expect("fixture leaf counts");
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
        // The descriptor keeps `existing.degree` but is forced back to slab /
        // non-tiny mode: tiny and tree contributions are excluded from leaf
        // `actual` (ADR 0096 §5), so switching them to slab re-admits exactly
        // that degree.
        if existing.is_tiny_mode() || existing.is_tree_mode() {
            graph
                .edges()
                .bump_vertex_segment_counts(vid, i64::from(existing.degree), 0)
                .expect("fixture leaf counts");
        }
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
            promote_bucket_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
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
            promote_bucket_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
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
            promote_bucket_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(99));
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
            promote_bucket_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        // Verify the descriptor is now in tree mode.
        let new_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        assert!(new_bucket.is_tree_mode());

        // Verify the LTB store minted exactly the depth-1 root length
        // (`ceil(T_PROMOTE / B)` leaf blocks).
        let root_len = T_PROMOTE.div_ceil(BLOCK_B as u32);
        assert_eq!(graph.ltb().block_capacity(), root_len);

        // Verify the new edge_start points at a LEG offset at or above the
        // minted block ids (root entries 0..root_len).
        assert!(
            new_bucket.edge_start() >= u64::from(root_len),
            "edge_start {} should be ≥ {root_len} (LTB block ids are 0..{root_len})",
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
            promote_bucket_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
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
            promote_bucket_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
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
        // `ceil(T_PROMOTE / B)` distinct block_ids in order. Read the root
        // region bytes from the new edge_start and decode them.
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
            promote_bucket_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let post_bucket = graph
            .buckets()
            .read_label_bucket_slot(slot)
            .expect("read_label_bucket_slot");
        let new_edge_start = post_bucket.edge_start();
        let root_len = T_PROMOTE.div_ceil(BLOCK_B as u32);
        let mut root_bytes = vec![0u8; root_len as usize * 4];
        graph
            .edges()
            .read_slots_contiguous_bytes(new_edge_start, &mut root_bytes);
        // Each block_id must be a valid mint (id < block_capacity).
        let cap = graph.ltb().block_capacity();
        let mut ids = Vec::with_capacity(root_len as usize);
        for i in 0..root_len {
            let at = i as usize * 4;
            let id = u32::from_le_bytes(root_bytes[at..at + 4].try_into().unwrap());
            assert!(
                id < cap,
                "root entry {i}: id {id} must be a valid mint (< {cap})"
            );
            ids.push(id);
        }
        // block_ids must be unique (different mint calls).
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
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
            promote_bucket_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        // Read the LTB blocks: `root_len` blocks × B edges == `stored` edges.
        let root_len = T_PROMOTE.div_ceil(BLOCK_B as u32);
        for block_idx in 0..root_len {
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
            promote_bucket_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
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
            promote_bucket_to_tree_mode(&graph, vid, BucketLabelKey::directed_from_index(1));
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
        let bucket = LabelBucket::try_from_parts(label, 0, 4096, 4096, 0, 0, 0, -1, 0)
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

        let result = promote_bucket_to_tree_mode(&graph, vid, label);
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

#[cfg(test)]
mod spill_test;
