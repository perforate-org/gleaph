//! One entry point for a row operation on a labeled bucket.
//!
//! ADR 0100: an operation has a single path, and the storage-class dispatch lives inside it. A caller that
//! decides whether it is looking at a tiny row, a slab prefix, a spill run or a tree has taken over an
//! invariant it does not own - which is how the value plane and the tree write drifted apart.
//!
//! The view borrows the graph; it holds no state and allocates nothing, so it costs nothing at run time and
//! reads as the operation it names.

use crate::{
    VertexId,
    labeled::{
        access::LabelEdgeSpanAccess, bucket_label_key::BucketLabelKey, record::LabelBucket,
        slot_index::checked_add_slot_index,
    },
    lara::{edge::InsertLocation, operation_error::LaraOperationError},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::insert::{
    EdgePlacementPolicy, ScalarInsertLocation, ScalarInsertStorage, ScalarLocationCapture,
    log_collect_overflow,
};
use super::{BucketMode, BucketSearch, EdgeRemoval, LabeledLaraGraph, T_DEMOTE};
use crate::traverse::{BucketEntryPosition, EdgeSlotState};

/// Where the row that was just stored puts its value block.
#[derive(Clone, Copy)]
pub(super) enum StoredRowSlot {
    /// A row inside the slab prefix: its block is at that prefix position.
    Prefix(u32),
    /// A row appended to the values run. `logical` is the row's logical slot when the storage layer
    /// reported one; otherwise the position is read back from the run.
    Run { logical: Option<u32> },
}

/// The row plane of one bucket.
pub(crate) struct BucketRowPlane<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    graph: &'a LabeledLaraGraph<E, M>,
}

impl<'a, E, M> BucketRowPlane<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn new(graph: &'a LabeledLaraGraph<E, M>) -> Self {
        Self { graph }
    }

    /// Finishes a row that was just stored: writes its inline-property block when the bucket carries
    /// values, then publishes the descriptor.
    ///
    /// Two rules live here instead of at each insert branch. The slot: a slab row's block sits at the
    /// prefix position it extended, while a run row's sits at `stored_slots + its run index` - a value the
    /// caller cannot know before the append, which is why the run index is read back rather than passed.
    /// The pairing: the block and the descriptor that admits it are published together, so no caller can
    /// store a row and forget its block - the desync that made a valueless insert and a tree-bucket insert
    /// disagree with the row set.
    pub(super) fn finish_stored_row(
        &self,
        src: VertexId,
        bucket_slot: u64,
        bucket: LabelBucket,
        slot: StoredRowSlot,
        edge: &E,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        // The descriptor is published either way: the row grew the prefix or the run, and that growth is
        // what the publication admits. Only the block is conditional on the bucket carrying values.
        let bucket = if bucket.inline_property_byte_width() == 0 {
            bucket
        } else {
            let value_slot = match slot {
                StoredRowSlot::Prefix(position) => position,
                StoredRowSlot::Run { logical } => logical.unwrap_or_else(|| {
                    bucket.stored_slots().saturating_add(
                        self.graph.bucket_values_run_rows(&bucket).saturating_sub(1),
                    )
                }),
            };
            self.graph
                .value_plane(bucket_slot, bucket)
                .write_after_insert(src, value_slot, edge)?
                .bucket()
        };
        self.graph.publish_bucket_descriptor(bucket_slot, bucket)
    }

    /// Inserts one row, choosing among the bucket's storage classes.
    ///
    /// ADR 0100: the four classes (tiny, slab, spill run, tree) are four representations of one operation, so
    /// the choice lives here rather than at each call site. The body is the implementation that used to sit on
    /// the graph; this first cut moves it unchanged so the diff reads as a move.
    pub(super) fn insert_row(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
        placement: EdgePlacementPolicy,
        location_capture: ScalarLocationCapture,
    ) -> Result<Option<ScalarInsertLocation>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.graph.ensure_vertex(src)?;
        let mut vertex = self.graph.vertices.get(src);
        let edge_inline_property_width = edge.edge_inline_property_byte_width();
        let has_edge_inline_property = edge_inline_property_width != 0;
        if vertex.is_default_edge_labeled() {
            if has_edge_inline_property {
                return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
                    bucket_width: 0,
                    edge_inline_property_width,
                });
            }
            if label_id == self.graph.bypass_storage_label_for(&vertex)
                && self.graph.may_use_homogeneous_bypass(src)
            {
                match self
                    .graph
                    .insert_homogeneous_bypass_edge(src, label_id, edge.clone())
                {
                    Err(LabeledOperationError::Store(LaraOperationError::SpillNeedsPromotion)) => {
                        super::promote::promote_bypass_with_edge(self.graph, src, &edge)?;
                    }
                    result => result?,
                }
                return Ok(None);
            }
            // A same-label insert into a bypass row that stopped being the tail
            // must not extend its slab region: every such insert would rescan
            // and rewrite all later rows' origins. Promote once so the insert —
            // and every future one for this row — takes the bounded bucket path.
            self.graph.promote_bypass_to_bucket_mode(src)?;
            vertex = self.graph.vertices.get(src);
        } else if vertex.degree() == 0
            && self.graph.is_homogeneous_bypass_label(label_id)
            && self.graph.may_use_homogeneous_bypass(src)
            && !has_edge_inline_property
        {
            self.graph.insert_homogeneous_bypass(src, label_id, edge)?;
            return Ok(None);
        }

        if edge_inline_property_width != 0
            && let BucketSearch::Missing { .. } = self.graph.find_bucket(src, &vertex, label_id)?
        {
            // The dispatcher requires the bucket to be pre-declared
            // with the right `inline_property_byte_width` before the
            // first insert (this is a deliberate contract: the
            // caller must establish the schema via
            // `ensure_label_bucket_inline_property_byte_width` or
            // a similar helper, not have the dispatcher infer it).
            return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
                bucket_width: 0,
                edge_inline_property_width,
            });
        }

        // Plan 0318 §Step 3 cap wiring: enforce the per-vertex bucket
        // count cap before the new-bucket create path runs. The
        // pre-existing bucket path is a no-op (count unchanged).
        super::check_vertex_bucket_count_cap(&vertex)?;

        let (bucket_slot, mut bucket) = self.graph.find_or_create_bucket(src, &vertex, label_id)?;
        // ADR 0096 §7: match-first dispatch — mode decides before any
        // storage-class read, so a future mode cannot silently inherit a path.
        // Width is only readable inside the Slab arm below (tiny payload and
        // tree roots never reach a width read).
        match BucketMode::from_bucket(&bucket) {
            // ADR 0096 §5: tiny buckets append inline (no slab/log/counts
            // writes). The arm precedes every width read structurally: width
            // bytes are payload on tiny (see §1 wire table) and must never be
            // read as schema.
            BucketMode::Tiny => {
                self.graph
                    .insert_edge_tiny_mode(crate::labeled::graph::insert::TinyEdgeInsert {
                        src,
                        bucket_slot,
                        bucket,
                        label_id,
                        edge,
                        location_capture,
                        placement,
                    })
            }
            // Plan 0318 §Step 6 single dispatch point: tree-mode buckets
            // bypass the slab-path entirely. Rope / PMA / placement /
            // leaf-pin code does not see this branch.
            BucketMode::Tree => {
                // Width check lives INSIDE the Tree arm (§7.1): the tree callee
                // validators below assume pre-matched widths (Plan 0326
                // LPB-in-tree: schema is declared via the width APIs before
                // inserting); a mismatch fails closed here, never in the Slab
                // width path. This is also the only dispatcher-visible
                // `w > 0 → tree` schema gate (the failing regression pins it).
                if edge_inline_property_width != bucket.inline_property_byte_width() {
                    return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
                        bucket_width: bucket.inline_property_byte_width(),
                        edge_inline_property_width,
                    });
                }
                // Plan 0326 LPB-in-tree: tree mode + `w > 0` is wired
                // through `tree_mode_insert_edge` (which writes the
                // property row via the combined span realloc). The
                // previous carve-out `if has_edge_inline_property` is
                // REMOVED; the dispatcher accepts `w > 0` on a tree
                // bucket as long as the edge's width matches the
                // bucket's width (width mismatch is a typed error above
                // at the `ensure_bucket_inline_property_schema_for_insert_with_materialize`
                // call).
                // Plan 0340 (ADR 0088 follow-up `tree-mode-tombstone-reuse`):
                // Unordered tree buckets reuse an interior tombstone within
                // the fixed tail-first window before tail-appending. Insertion
                // tree buckets never reuse (ADR 0052 §6) — the gate is
                // structural, enforced by the placement policy passed in.
                if let Some(reused_slot) = super::tree_write::tree_mode_reuse_tombstone_slot(
                    self.graph,
                    bucket_slot,
                    &bucket,
                    label_id,
                    &edge,
                    placement,
                )? {
                    return Ok(Some(ScalarInsertLocation {
                        logical_slot: reused_slot,
                        storage: ScalarInsertStorage::Slab,
                    }));
                }
                let logical_slot = super::tree_write::tree_mode_insert_edge(
                    self.graph,
                    src,
                    bucket_slot,
                    &bucket,
                    label_id,
                    &edge,
                )?;
                Ok(Some(ScalarInsertLocation {
                    logical_slot,
                    storage: ScalarInsertStorage::Slab,
                }))
            }
            BucketMode::Slab => {
                // Width check lives INSIDE the Slab arm (§7.1): tiny diverged
                // into its arm and tree returns from its arm, so only slab
                // widths (real schema) ever reach this read. The vertex row is
                // re-read fresh by the slab path below; no stale row crosses
                // the match.
                if edge_inline_property_width != bucket.inline_property_byte_width() {
                    // Plan 0320 §Step 2: width-addition (0→w) wiring.
                    // The new helper handles 0→w on non-empty buckets
                    // via `materialize_inline_property_stream`; other
                    // mismatches stay fail-closed (typed error).
                    bucket = self
                        .graph
                        .ensure_bucket_inline_property_schema_for_insert_with_materialize(
                            src,
                            bucket_slot,
                            bucket,
                            edge_inline_property_width,
                        )?;
                    self.graph.publish_bucket_descriptor(bucket_slot, bucket)?;
                }
                // Admission is based on logical resident width (prefix plus used run
                // rows), not the bytes moved by a run-retaining relocation. A leaf
                // slide must not be required to fold the run for this cap to apply.
                //
                // The additional `< u32::MAX` guard skips promotion for buckets
                // that are already at the degree cap (e.g. `normal_label_bucket_insert_rejects_edge_len_overflow`).
                // Those buckets fail with `RowDegreeOverflow` from the slab path
                // instead of being routed to the tree path, which would otherwise
                // try to mint `resident / B` LTB blocks and fail in a
                // different way.
                //
                // Every edge width promotes: a tree block holds EDGES_PER_BLOCK
                // records of `E::BYTES`, so the width only scales the block, it
                // does not select a different path.
                let resident = self.graph.bucket_resident_rows(&bucket)?;
                // The cap question is asked through the shared predicate; the `u32::MAX` guard stays because
                // a bucket already at the degree cap must fail with `RowDegreeOverflow` from the slab path
                // rather than be routed to the tree path (which would mint `resident / B` LTB blocks).
                if !super::resident_fits_mode(&bucket, resident) && resident < u32::MAX {
                    // The view hands back the descriptor promotion published, so the slab-mode value read
                    // above cannot be published over a tree root.
                    bucket = self
                        .graph
                        .layout(bucket_slot, bucket)
                        .promote_to_tree(src)?
                        .bucket();
                    if bucket.is_tree_mode() {
                        // Plan 0340: reuse applies to a freshly-promoted Unordered
                        // tree bucket too (promotion preserves tombstones).
                        if let Some(reused_slot) =
                            super::tree_write::tree_mode_reuse_tombstone_slot(
                                self.graph,
                                bucket_slot,
                                &bucket,
                                label_id,
                                &edge,
                                placement,
                            )?
                        {
                            return Ok(Some(ScalarInsertLocation {
                                logical_slot: reused_slot,
                                storage: ScalarInsertStorage::Slab,
                            }));
                        }
                        let logical_slot = super::tree_write::tree_mode_insert_edge(
                            self.graph,
                            src,
                            bucket_slot,
                            &bucket,
                            label_id,
                            &edge,
                        )?;
                        return Ok(Some(ScalarInsertLocation {
                            logical_slot,
                            storage: ScalarInsertStorage::Slab,
                        }));
                    }
                }
                self.graph
                    .ensure_bucket_slack_insert_when_peers_have_values(src, &vertex)?;
                let vertex = self.graph.vertices.get(src);
                let bucket_index = LabeledLaraGraph::<E, M>::labeled_bucket_descriptor_index(
                    &vertex,
                    bucket_slot,
                )?;
                // A single pass: the values fold that made this a retry loop is gone (ADR 0099), so this is a block, not a loop.
                {
                    let attempt_edge = edge.clone();
                    let vertex = self.graph.vertices.get(src);
                    // ADR 0099: a full value run is no reason to fold the values plane - the
                    // append grows the run itself, and folding it would leave the edge run's rows
                    // pointing at blocks that moved.
                    // Unordered placement (ADR 0052 §5 step 1): reuse an in-slab
                    // tombstone before appending to the slab tail or the overflow log.
                    // The helper keeps the dense fast path O(1) and falls back to the
                    // ordered path when the inline property bytes are log-backed
                    // (ADR 0052 §9).
                    if placement == EdgePlacementPolicy::Unordered
                        && let Some(location) = self.graph.try_reuse_unordered_slab_tombstone(
                            src,
                            bucket_slot,
                            bucket,
                            &attempt_edge,
                        )?
                    {
                        return Ok(Some(location));
                    }
                    let successor_start = if vertex.degree() == 1 && !has_edge_inline_property {
                        self.graph.bucket_successor_start_after_bucket(
                            &vertex,
                            bucket_index,
                            &bucket,
                        )?
                    } else {
                        self.graph.bucket_slab_window_end_exclusive_after_bucket(
                            &vertex,
                            bucket_index,
                            &bucket,
                        )?
                    };
                    let slack_span = successor_start.saturating_sub(bucket.edge_start());
                    if bucket.edge_run().is_none()
                        && bucket.stored_slots() > 0
                        && slack_span > u64::from(bucket.stored_slots())
                    {
                        let write_slot = checked_add_slot_index(
                            bucket.edge_start(),
                            u64::from(bucket.stored_slots()),
                        )
                        .ok_or(LaraOperationError::overflow())?;
                        debug_assert!(write_slot < successor_start);
                        self.graph
                            .edges
                            .write_slot(write_slot, attempt_edge.clone())?;
                        let logical_slot = bucket.stored_slots();
                        let bucket = bucket.grow_packed_slab_by_one();
                        self.finish_stored_row(
                            src,
                            bucket_slot,
                            bucket,
                            StoredRowSlot::Prefix(logical_slot),
                            &attempt_edge,
                        )?;
                        let hdr = self.graph.edges.header();
                        let next_num_edges = hdr
                            .num_edges
                            .checked_add(1)
                            .ok_or(LaraOperationError::overflow())?;
                        self.graph.edges.set_num_edges(next_num_edges);
                        self.graph
                            .edges
                            .bump_vertex_segment_counts(src, 1, 0)
                            .map_err(LabeledOperationError::from)?;
                        return Ok(Some(ScalarInsertLocation {
                            logical_slot,
                            storage: ScalarInsertStorage::Slab,
                        }));
                    }
                    let access = LabelEdgeSpanAccess::with_bucket(
                        &self.graph.buckets,
                        bucket_slot,
                        bucket,
                        successor_start,
                        src,
                    );
                    let insert_result = match location_capture {
                        ScalarLocationCapture::Ignore => {
                            self.graph.edges.insert_edge_without_logical_slot(
                                &access,
                                VertexId::from(0),
                                attempt_edge.clone(),
                            )
                        }
                        ScalarLocationCapture::Capture => self.graph.edges.insert_edge(
                            &access,
                            VertexId::from(0),
                            attempt_edge.clone(),
                        ),
                    };
                    match insert_result {
                        Ok(InsertLocation::Slab(written_slot)) if !has_edge_inline_property => {
                            Ok(Some(ScalarInsertLocation {
                                logical_slot: written_slot,
                                storage: ScalarInsertStorage::Slab,
                            }))
                        }
                        Ok(InsertLocation::Slab(written_slot)) => {
                            bucket = self.graph
                                .buckets
                                .read_label_bucket_slot(bucket_slot)
                                .ok_or_else(|| {
                                    log_collect_overflow(
                                        "insert_edge_skip_leaf_cascade: cannot re-read bucket after slab insert",
                                    );
                                    LaraOperationError::CollectAllocationOverflow
                                })?;
                            let new_stored =
                                written_slot.saturating_add(1).max(bucket.stored_slots());
                            let bucket = if new_stored != bucket.stored_slots() {
                                bucket.with_stored_slots(new_stored)
                            } else {
                                bucket
                            };
                            self.finish_stored_row(
                                src,
                                bucket_slot,
                                bucket,
                                StoredRowSlot::Prefix(written_slot),
                                &attempt_edge,
                            )?;
                            Ok(Some(ScalarInsertLocation {
                                logical_slot: written_slot,
                                storage: ScalarInsertStorage::Slab,
                            }))
                        }
                        Ok(InsertLocation::Spill { logical_slot, .. }) => {
                            if has_edge_inline_property {
                                bucket = self.graph
                                    .buckets
                                    .read_label_bucket_slot(bucket_slot)
                                    .ok_or_else(|| {
                                        log_collect_overflow(
                                            "insert_edge_skip_leaf_cascade: cannot re-read bucket after spill insert",
                                        );
                                        LaraOperationError::CollectAllocationOverflow
                                    })?;
                                self.finish_stored_row(
                                    src,
                                    bucket_slot,
                                    bucket,
                                    StoredRowSlot::Run {
                                        logical: logical_slot,
                                    },
                                    &attempt_edge,
                                )?;
                            }
                            Ok(logical_slot.map(|logical_slot| ScalarInsertLocation {
                                logical_slot,
                                storage: ScalarInsertStorage::Spill,
                            }))
                        }
                        Err(LaraOperationError::SpillNeedsPromotion) => {
                            // The bucket's run cannot serve another row (ADR 0097 §6): its class is
                            // full at MAX_ROWS, or the arena cannot address the growth. Tree mode is
                            // the chunked path with no fixed row ceiling, so promote and re-dispatch
                            // through the mode dispatcher - exactly as the tiny->slab promotion
                            // does, because the bucket is no longer slab mode and this loop's
                            // remaining steps do not apply to it. This replaces the retired
                            // log-full leaf rebalance: a per-bucket run has no shared area to empty.
                            self.graph
                                .layout(bucket_slot, bucket)
                                .promote_to_tree(src)?;
                            self.insert_row(src, label_id, edge, placement, location_capture)
                        }
                        Err(e) => Err(LabeledOperationError::from(e)),
                    }
                }
            }
        }
    }
    /// Removes one row, reporting the bounded survivor slots that moved.
    ///
    /// ADR 0100: the storage-class dispatch is inside the operation, so callers cannot inherit a path
    /// by accident. Moved unchanged from the graph's `remove_edge_at_slot_with_move`.
    pub(crate) fn remove_row(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
    ) -> Result<Option<EdgeRemoval<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.graph.ensure_vertex(src)?;
        let vertex = self.graph.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return self
                .graph
                .remove_default_bypass_edge_at_slot(src, label_id, vertex, slot_index);
        }
        let BucketSearch::Found { slot, bucket } =
            self.graph.find_bucket(src, &vertex, label_id)?
        else {
            return Ok(None);
        };
        // ADR 0096 §7: match-first dispatch — mode decides before any
        // storage-class read, so a future mode cannot silently inherit a path.
        match BucketMode::from_bucket(&bucket) {
            // ADR 0096 §5: tiny buckets graduate to slab before deleting
            // (promote-then-tombstone): deletes never move survivors inline in
            // any mode or policy — positional stability across deletes is
            // load-bearing (slot-keyed counterpart occurrences; moves-dropping
            // remove APIs). Tiny has no tombstone representation, so the
            // bucket takes the normal slab tombstone path. Moves are always
            // empty (nothing moved), matching slab-delete reporting.
            BucketMode::Tiny => self
                .graph
                .remove_tiny_edge_at_slot(src, slot, &bucket, slot_index),
            // Plan 0318 §Step 6 single dispatch point: tree-mode buckets
            // take the tombstone-rewrite path; slab buckets keep the
            // existing path. No other module under `graph/` branches on
            // `bucket.is_tree_mode()`.
            BucketMode::Tree => {
                let mut demote_moves: Option<Vec<super::EdgeSlotMove>> = None;
                let removed = super::tree_write::tree_mode_remove_edge_at_slot(
                    self.graph, slot, &bucket, slot_index,
                )?;
                // Plan 0319 §Step 2: after a successful tree-mode removal,
                // check the degree-hysteresis trigger. If the updated
                // `degree <= T_DEMOTE`, rebuild the bucket as a fresh
                // slab. The trigger is best-effort: a successful removal
                // must not be turned into an error, so a mid-demote
                // failure is contained (`let _ =`). The next removal
                // retries the trigger; until then the bucket stays in
                // tree mode with the same `degree`.
                if let Some(ref _removed_edge) = removed {
                    let updated = self
                        .graph
                        .buckets()
                        .read_label_bucket_slot(slot)
                        .ok_or(LaraOperationError::overflow())?;
                    if updated.is_tree_mode() && updated.degree <= T_DEMOTE {
                        // Best-effort: a successful removal is never turned into an error, so a mid-demote failure
                        // reports no moves and the next removal retries the trigger.
                        demote_moves = super::tree_write::tree_mode_demote_to_slab::<E, M>(
                            self.graph, src, slot, label_id, &updated,
                        )
                        .ok();
                    }
                }
                // A demotion repacks the survivors, and the caller hears about it: dropping those moves left an
                // observer pointing at slots that no longer hold those rows.
                let moves = demote_moves.unwrap_or_default();
                Ok(removed.map(|removed| EdgeRemoval {
                    removed: removed.with_slot_index(slot_index),
                    moves,
                }))
            }
            BucketMode::Slab => self
                .graph
                .remove_bucket_edge_at_slot(src, &vertex, slot, bucket, slot_index),
        }
    }
    pub(crate) fn read_row_state(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slot: BucketEntryPosition,
    ) -> Result<EdgeSlotState<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.graph.ensure_vertex(owner)?;
        let vertex = self.graph.vertices.get(owner);
        let bucket = if vertex.is_default_edge_labeled() {
            if label != self.graph.bypass_storage_label_for(&vertex) {
                return Ok(EdgeSlotState::Missing);
            }
            self.graph.bypass_bucket_view(&vertex)?
        } else {
            let BucketSearch::Found { bucket, .. } =
                self.graph.find_bucket(owner, &vertex, label)?
            else {
                return Ok(EdgeSlotState::Missing);
            };
            bucket
        };
        self.graph.read_edge_state_at_slot(&bucket, slot.raw())
    }

    /// The live row at `slot`, or `None` for a tombstone or a missing position.
    ///
    /// The state form above is the contract; this is the shape most callers want.
    pub(crate) fn read_row(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slot: BucketEntryPosition,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        Ok(match self.read_row_state(owner, label, slot)? {
            EdgeSlotState::Live(edge) => Some(edge),
            EdgeSlotState::Missing | EdgeSlotState::Tombstone => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::graph::test_support::{TestEdge, test_graph_with_segment_size};
    use crate::labeled::record::LabeledVertex;

    type Graph = LabeledLaraGraph<TestEdge, VectorMemory>;

    fn graph() -> Graph {
        test_graph_with_segment_size(16, BucketLabelKey::directed_from_index(1))
    }

    fn label() -> BucketLabelKey {
        BucketLabelKey::directed_from_index(2)
    }

    fn owner(graph: &Graph) -> VertexId {
        graph.push_vertex(LabeledVertex::default()).expect("push")
    }

    fn insert(graph: &Graph, vid: VertexId, target: u32) {
        graph
            .row_plane()
            .insert_row(
                vid,
                label(),
                TestEdge { target },
                EdgePlacementPolicy::Insertion,
                ScalarLocationCapture::Ignore,
            )
            .expect("insert a row");
    }

    /// A row inserted through the plane reads back through the same plane at the position it was inserted at,
    /// and a position beyond what the bucket holds answers `Missing` rather than a stale neighbour.
    #[test]
    fn a_row_reads_back_at_its_own_position() {
        let graph = graph();
        let vid = owner(&graph);
        for target in 7..10u32 {
            insert(&graph, vid, target);
        }
        for (slot, target) in (7..10u32).enumerate() {
            let read = graph
                .row_plane()
                .read_row(vid, label(), BucketEntryPosition::new(slot as u32))
                .expect("read")
                .expect("live row");
            assert_eq!(read.target, target, "slot {slot} keeps its own edge");
        }
        assert!(
            matches!(
                graph
                    .row_plane()
                    .read_row_state(vid, label(), BucketEntryPosition::new(3))
                    .expect("state"),
                EdgeSlotState::Missing
            ),
            "a position past the row is missing, not a neighbour"
        );
    }

    /// A removed row becomes a tombstone: still a position, no longer a live row, and the reading entry points
    /// distinguish the two so a caller does not mistake a hole for the end of the row.
    #[test]
    fn a_removed_row_is_a_tombstone_not_a_missing_position() {
        let graph = graph();
        let vid = owner(&graph);
        for target in 7..11u32 {
            insert(&graph, vid, target);
        }
        let removed = graph
            .row_plane()
            .remove_row(vid, label(), 1)
            .expect("remove")
            .expect("the row existed");
        assert_eq!(
            removed.removed.target, 8,
            "the removal reports the edge it took out"
        );
        assert!(matches!(
            graph
                .row_plane()
                .read_row_state(vid, label(), BucketEntryPosition::new(1))
                .expect("state"),
            EdgeSlotState::Tombstone
        ));
        assert_eq!(
            graph
                .row_plane()
                .read_row(vid, label(), BucketEntryPosition::new(1))
                .expect("read"),
            None,
            "the shape most callers want answers None for a tombstone"
        );
        // The neighbour is untouched: a tombstone is a position, not a compaction.
        assert_eq!(
            graph
                .row_plane()
                .read_row(vid, label(), BucketEntryPosition::new(2))
                .expect("read")
                .expect("live")
                .target,
            9
        );
    }

    /// Reading a bucket the vertex does not have answers `Missing`; reading a vertex that does not exist is an
    /// error, because "no such vertex" is not "no such row".
    #[test]
    fn a_missing_bucket_answers_missing_and_a_missing_vertex_errors() {
        let graph = graph();
        let vid = owner(&graph);
        assert!(
            matches!(
                graph
                    .row_plane()
                    .read_row_state(vid, label(), BucketEntryPosition::new(0))
                    .expect("state"),
                EdgeSlotState::Missing
            ),
            "no bucket means no position"
        );
        assert!(
            graph
                .row_plane()
                .read_row_state(VertexId::from(99), label(), BucketEntryPosition::new(0))
                .is_err(),
            "a vertex that does not exist is an error, not an empty row"
        );
    }
}
