//! Ordinal-preserving slab-bucket promotion to native edge/property trees.
//!
//! Admission uses the original initialized edge prefix plus used spill rows,
//! including tombstones. Target roots and blocks remain unpublished until both
//! streams are transcribed. Source runs and spans are retired only after publication.
//! Bypass conversion and public capacity-signal integration are separate callers.

use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{BucketMode, BucketSearch, LabeledLaraGraph, T_PROMOTE};
use crate::VertexId;
use crate::labeled::{
    bucket_label_key::BucketLabelKey,
    bucket_store::{PreparedExistingBucketRow, ReadySegmentRewrite, SegmentRewritePreparation},
    record::{LabelBucket, LabeledVertex},
    tree_csr::{B as BLOCK_B, derive_depth, root_len as derived_root_len},
};
use crate::lara::operation_error::{LaraOperationError, VertexAccess};
use crate::traits::{CsrEdgeTombstone, CsrVertex};

// The existing writers differ in ownership, not in tree source admission. Keep
// their preparations alive until both storage-ready and tree witnesses pass.
enum TreePublication<'a, V, M: Memory> {
    Existing(PreparedExistingBucketRow<'a, M>),
    Segment(SegmentRewritePreparation<'a, V, M>),
}

enum ReadyTreePublication<'a, M: Memory> {
    Existing(&'a PreparedExistingBucketRow<'a, M>),
    Segment(ReadySegmentRewrite<'a, M>),
}

impl<V: VertexAccess<LabeledVertex>, M: Memory> TreePublication<'_, V, M> {
    fn ready(&mut self) -> Result<ReadyTreePublication<'_, M>, LaraOperationError> {
        match self {
            Self::Existing(image) => Ok(ReadyTreePublication::Existing(image)),
            Self::Segment(preparation) => preparation.ready().map(ReadyTreePublication::Segment),
        }
    }
}

impl<M: Memory> ReadyTreePublication<'_, M> {
    fn publish(self) {
        match self {
            Self::Existing(image) => image.write(),
            Self::Segment(ready) => ready.publish(),
        }
    }
}

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
/// relocation keeps the run, so a caller that needs the rows in the prefix goes through
/// here rather than through a rewrite. The growth step asks the rebalance for room
/// (`preferred_extra` = the run's rows) while keeping the run where it is, and the retry
/// then performs the explicit compaction that fills it.
///
/// No production path calls this since every edge width promotes (a tree block holds
/// `EDGES_PER_BLOCK` records of `E::BYTES`): the insert dispatcher's run-full exit is
/// tree promotion, not a fold. The remaining callers materialize a prefix in test and
/// screening fixtures; whether an explicit fold trigger belongs to the spill<->slab
/// contract is settled with that contract, not here.
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
    let BucketSearch::Found { bucket, .. } = graph.find_bucket(vid, &vertex, label)? else {
        return Err(LabeledOperationError::BucketNotFound { vid, label });
    };
    if bucket.is_tree_mode() {
        return Ok(());
    }
    transcribe_and_publish_tree(graph, vid, bucket, None)
}

/// All target allocations stay owned here until storage preparation and final tree
/// source admission succeed. The optional edge commits with bypass-to-tree conversion.
fn transcribe_and_publish_tree<E: CsrEdgeTombstone, M: Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    bucket: LabelBucket,
    pending: Option<&E>,
) -> Result<(), LabeledOperationError> {
    use crate::labeled::ltb_raw_block_store::{BlockHeader, BlockKind};
    use crate::traits::NO_SPILL_RUN;
    use std::ops::ControlFlow;
    let label = bucket.bucket_label_key();
    let next_count = graph
        .edges()
        .header()
        .num_edges
        .checked_add(u64::from(pending.is_some()))
        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
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
    if usize::from(width) > crate::labeled::tree_csr::PROPERTY_BLOCK_BYTES {
        return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
            bucket_width: width,
            edge_inline_property_width: 0,
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
        graph.property_slots_per_block(width).ok_or(
            LabeledOperationError::InlinePropertyBytesWidthMismatch {
                bucket_width: width,
                edge_inline_property_width: 0,
            },
        )?
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
    let source = super::compact::PhysicalLayoutSource::capture(graph, vid)?;
    let canonical = super::compact::CanonicalPhysicalVertex::capture(graph, vid)?;
    let old_alloc = if canonical.vertex.is_default_edge_labeled() {
        0
    } else {
        canonical.vertex.stored_slots
    };
    let new_alloc = if canonical.vertex.is_default_edge_labeled() {
        combined_len
    } else {
        old_alloc
    };
    let counts = graph.edges().prepare_vertex_span_counts(
        vid,
        old_alloc,
        new_alloc,
        -i64::from(bucket.degree()),
    )?;
    let sheltered_prefix = super::tree_write::pre_promotion_span_inside_pinned_leaf(
        graph,
        vid,
        bucket.edge_start(),
        prefix,
    )?;
    source.validate()?;
    canonical.validate(graph)?;
    graph.edges().free_span_store().reserve_for_releases(2)?;
    graph.values().reserve_retired_byte_spans(1)?;
    // Promotion is a shrink: the root (`combined_len` slots) never needs more room
    // than the prefix it replaces (`prefix >= combined_len` for every production
    // trigger), so write it at the bucket's own base. That keeps the label-order
    // address monotonicity the successor windows rely on and allocates nothing.
    // A prefix too small for the root falls back to the leaf heap, then to the
    // global allocator (no production path reaches it).
    let in_place = prefix >= combined_len;
    let in_pin_start = if in_place {
        None
    } else {
        // Resident space belongs to the leaf pin: place the root inside it, keeping
        // the source span (still live until the transcription reads it) intact.
        let pin = super::leaf_arena::LeafArena::capture(graph, vid)?;
        let avoid = pin.as_ref().and_then(|pin| pin.vertex_space(vid));
        match (pin.as_ref(), avoid) {
            (Some(pin), Some(avoid)) => pin.place(vid, u64::from(combined_len), avoid),
            _ => None,
        }
    };
    let allocated_in_pin = in_pin_start.is_some();
    // Only a span taken from the global allocator is rolled back on failure.
    let owns_new_span = !in_place && !allocated_in_pin;
    let new_start = if in_place {
        bucket.edge_start()
    } else {
        match in_pin_start {
            Some(base) => base,
            None => graph.edges().allocate_span(u64::from(combined_len))?,
        }
    };
    let allocated_header = graph.edges().header();
    let mut allocated_blocks = Vec::new();
    let prepared = (|| -> Result<(), LabeledOperationError> {
        let source = source.bind_allocated_header(allocated_header)?;
        crate::slab_index::checked_add_slot_exclusive_end(new_start, u64::from(combined_len))
            .filter(|end| *end <= allocated_header.elem_capacity)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
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
        let mut edge_payload = vec![0u8; graph.ltb().payload_bytes()];
        let mut property_payload = vec![0u8; graph.ltb().payload_bytes()];
        let mut dead = 0u16;
        let tombstone = E::tombstone_edge();
        // Source slots are explicit; missing ordinals are emitted as native tree tombstones.
        // The source visitor resolves values through their own live-order prefix/run mapping.
        let mut emit = |slot: u32, edge: &E, value: &[u8]| -> Result<(), LabeledOperationError> {
            let epos = slot as usize % BLOCK_B;
            let eoff = epos * E::BYTES;
            edge.write_to(&mut edge_payload[eoff..eoff + E::BYTES]);
            dead += u16::from(edge.is_deleted_slot());
            if epos + 1 == BLOCK_B || slot + 1 == resident {
                edge_ids.push(mint(
                    BlockHeader {
                        tombstone_count: dead,
                        ..base_header
                    },
                    &edge_payload[..(epos + 1) * E::BYTES],
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
        //
        // One root entry occupies one edge-record slot, so tree mode needs a slot
        // wide enough for a u32 block id; the production edge is exactly 4 bytes
        // and every in-tree record is at least that wide.
        debug_assert!(
            E::BYTES >= 4,
            "tree root entries are u32 block ids, so E::BYTES must be at least 4"
        );
        let mut root = |mut ids: Vec<u32>,
                        depth: u32,
                        kind: BlockKind|
         -> Result<Vec<u8>, LabeledOperationError> {
            for level in 1..depth {
                let mut parents = Vec::new();
                for chunk in ids.chunks(BLOCK_B) {
                    // An interior payload is a slot-strided root image: one u32
                    // block id in each edge-record slot, the same codec the slab
                    // root region uses (so a raw copy still moves a whole level).
                    let mut bytes = vec![0u8; chunk.len() * E::BYTES];
                    for id in chunk {
                        super::tree_write::push_root_entry::<E>(&mut bytes, *id);
                    }
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
            // The root region lives in edge-record slots: each entry is the u32
            // block id in its slot's first four bytes, zero-padded to E::BYTES.
            let mut image = vec![0u8; ids.len() * E::BYTES];
            for (index, id) in ids.iter().enumerate() {
                let offset = index * E::BYTES;
                image[offset..offset + 4].copy_from_slice(&id.to_le_bytes());
            }
            Ok(image)
        };
        let edge_root = root(edge_ids, depth, BlockKind::EdgeInterior)?;
        let property_root = root(
            property_ids,
            property_depth,
            BlockKind::InlinePropertyInterior,
        )?;
        if edge_root.len() != edge_root_len as usize * E::BYTES
            || property_root.len() != property_root_len as usize * E::BYTES
        {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        let target = template.with_base_slot_start(new_start);
        let mut publication = if canonical.vertex.is_default_edge_labeled() {
            let plan = graph.buckets().plan_promote_bypass_to_bucket_mode(
                graph.vertices(),
                vid,
                target,
                new_alloc,
            )?;
            TreePublication::Segment(plan.reserve()?)
        } else {
            let mut desired = graph.read_vertex_label_buckets(&canonical.vertex)?;
            let row = desired
                .iter_mut()
                .find(|row| row.bucket_label_key() == label)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            *row = target;
            TreePublication::Existing(
                graph
                    .buckets()
                    .prepare_existing_row_image(canonical.vertex, &desired)?,
            )
        };
        let ready = publication.ready()?;
        source.validate()?;
        canonical.validate(graph)?;
        if !counts.validate_source() {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        // The root lands in the bucket's own base, so writing it overwrites the
        // head of the source prefix. Every fallible step is therefore done; the
        // write requires no growth (the prefix already backs those slots) and the
        // publish below is infallible.
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
        ready.publish();
        counts.write();
        if canonical.vertex.is_default_edge_labeled() {
            graph
                .invalidate_bucket_lookup_caches_for_bucket_segment(vid)
                .expect("admitted bucket segment");
        }
        if pending.is_some() {
            graph.edges().set_num_edges(next_count);
        }
        Ok(())
    })();
    if let Err(error) = prepared {
        for id in allocated_blocks.into_iter().rev() {
            graph
                .ltb()
                .release(id)
                .expect("unpublished promotion block must remain owned");
        }
        if owns_new_span {
            graph
                .edges()
                .release_span(new_start, u64::from(combined_len))
                .expect("promotion rollback capacity was reserved");
        }
        return Err(error);
    }
    // No recoverable errors after publication. Only old allocations are retired here.
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
    if !sheltered_prefix {
        if in_place {
            // The root occupies the prefix's head; its unused tail becomes free.
            let tail = prefix - combined_len;
            if tail > 0 {
                graph
                    .edges()
                    .release_span(
                        bucket.edge_start() + u64::from(combined_len),
                        u64::from(tail),
                    )
                    .expect("source prefix tail retirement was reserved");
            }
        } else {
            graph
                .edges()
                .release_span(bucket.edge_start(), u64::from(prefix))
                .expect("source prefix retirement was reserved");
        }
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
    transcribe_and_publish_tree(graph, vid, source, Some(edge))
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
        // The raw write bypasses the allocator, so declare the capacity the
        // prefix now occupies: promotion validates that the root fits inside the
        // declared slab capacity and writes it in place.
        let end = edge_start + u64::from(count);
        if graph.edges().header().elem_capacity < end {
            graph
                .edges()
                .set_elem_capacity(end)
                .expect("fixture prefix capacity");
        }
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_rejects_existing_bucket_below_cap() {
        // Plan 0318 §Step 4 amend: under the placeholder
        // `compute_bucket_allocation` reports the slab cap,
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
        let capacity_before = graph.edges().header().elem_capacity;

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

        // The root is written at the bucket's own base (promotion is a shrink),
        // so the address does not move and nothing is allocated.
        assert_eq!(
            new_bucket.edge_start(),
            edge_start,
            "the root stays at the bucket's base"
        );
        assert_eq!(
            graph.edges().header().elem_capacity,
            capacity_before,
            "in-place promotion allocates no span"
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
        // After a successful promotion the descriptor is published in tree mode;
        // only the mode/geometry fields change, not the root address.
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
        assert_ne!(post_bucket, pre_bucket, "the mode fields change");
        assert_eq!(
            post_bucket.edge_start(),
            pre_edge_start,
            "the root address is unchanged"
        );
        assert_eq!(
            post_bucket.degree(),
            pre_bucket.degree(),
            "the logical edge count is unchanged"
        );
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
            let mut buf = vec![0u8; graph.ltb().payload_bytes()];
            graph
                .ltb()
                .read_payload(block_idx, &mut buf)
                .expect("read_payload");
            for in_block_slot in 0..BLOCK_B as u32 {
                let slot_idx = block_idx * BLOCK_B as u32 + in_block_slot;
                let expected = slot_idx.wrapping_add(100);
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
    fn promote_writes_the_root_at_the_bucket_base() {
        // Promotion is a shrink, so the root is written at the bucket's own base
        // and the address does not move. The root region still holds the leaf
        // block ids: entry 0 resolves to a minted edge leaf.
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
        assert_eq!(
            post_bucket.edge_start(),
            pre_edge_start,
            "the root stays at the bucket's base"
        );
        assert!(post_bucket.is_tree_mode());
        let block0 =
            super::super::tree_write::resolve_leaf_block_id::<TestEdge, _>(&graph, &post_bucket, 0)
                .expect("root entry 0 resolves");
        assert_eq!(
            graph.ltb().read_block_header(block0).kind,
            crate::labeled::ltb_raw_block_store::BlockKind::Edge,
            "root entry 0 is a minted edge leaf"
        );
    }

    #[test]
    #[cfg(not(feature = "canbench"))]
    fn promote_returns_the_unused_prefix_tail_to_the_free_store() {
        // The root is written at the bucket's base, so the address does not move;
        // the slots past the root region are no longer resident and must return to
        // the free-span store (a promotion that kept them would leak capacity).
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
        assert_eq!(
            post_edge_start, pre_edge_start,
            "the root stays at the base"
        );
        let combined = crate::labeled::graph::compact::bucket_content_slots(&post_bucket);
        assert!(
            combined < pre_bucket.stored_slots(),
            "promotion shrinks the resident region"
        );
        let tail_start = post_edge_start + u64::from(combined);
        let tail_len = u64::from(pre_bucket.stored_slots()) - u64::from(combined);
        assert!(
            graph.edges().free_byte_spans().iter().any(|span| {
                span.start_slot <= tail_start && span.start_slot + span.len >= tail_start + tail_len
            }),
            "the unused prefix tail [{tail_start}, {}) must be free",
            tail_start + tail_len
        );
    }

    // ================== wide-edge promotion ==================
    // A tree block holds `EDGES_PER_BLOCK` records of `E::BYTES`, so a wide
    // edge type promotes exactly like a 4-byte one; the record width only
    // scales the block. These tests pin that the stored record round-trips.

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
    fn wide_edge_insert_promotes_and_reads_the_record_back() {
        // 4096 inserts fill the bucket to T_PROMOTE; the 4097th crosses the
        // promote trigger. Every width promotes, and the whole record (not
        // just its target) survives the tree round trip.
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
        let BucketSearch::Found { bucket, .. } =
            graph.find_bucket(vid, &vertex, label).expect("find")
        else {
            panic!("bucket missing");
        };
        assert!(
            bucket.is_tree_mode(),
            "wide edge bucket must promote past T_PROMOTE"
        );
        assert_eq!(bucket.degree, 4097);
        assert_eq!(bucket.stored_slots(), 4097);
        let mut read = Vec::new();
        let flow = graph
            .visit_edges_with_inline_property(
                vid,
                label,
                crate::labeled::graph::OutEdgeOrder::Ascending,
                |slot, item| {
                    read.push((slot.raw(), item.edge));
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .expect("visit tree-mode rows");
        assert_eq!(flow, std::ops::ControlFlow::Continue(()));
        assert_eq!(read.len(), 4097);
        for (slot, edge) in read {
            assert_eq!(edge, WideTestEdge::new(slot, 7), "slot {slot}");
        }
    }
}

#[cfg(test)]
mod spill_test;
