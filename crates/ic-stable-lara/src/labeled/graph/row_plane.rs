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
        access::LabelEdgeSpanAccess, bucket_label_key::BucketLabelKey,
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

    /// Inserts one row, choosing among the bucket's storage classes.
    ///
    /// ADR 0100: the four classes (tiny, slab, spill run, tree) are four representations of one operation, so
    /// the choice lives here rather than at each call site. The body is the implementation that used to sit on
    /// the graph; this first cut moves it unchanged so the diff reads as a move.
    #[allow(clippy::needless_return)]
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
            BucketMode::Tiny => self.graph.insert_edge_tiny_mode(
                src,
                bucket_slot,
                bucket,
                label_id,
                edge,
                placement,
                location_capture,
            ),
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
                if (super::T_PROMOTE..u32::MAX).contains(&resident) {
                    super::promote::promote_bucket_to_tree_mode(self.graph, src, label_id)?;
                    // Re-read the bucket: after promotion it is tree mode.
                    let vertex = self.graph.vertices.get(src);
                    bucket = match self.graph.find_bucket(src, &vertex, label_id)? {
                        BucketSearch::Found { bucket, .. } => bucket,
                        BucketSearch::Missing { .. } => {
                            return Err(LabeledOperationError::BucketNotFound {
                                vid: src,
                                label: label_id,
                            });
                        }
                    };
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
                // A single pass: the values fold that made this a retry loop is gone (ADR 0099).
                #[allow(clippy::never_loop)]
                for _attempt in 0..1u32 {
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
                        let value_slot = logical_slot;
                        let bucket = bucket.grow_packed_slab_by_one();
                        let bucket = self
                            .graph
                            .value_plane(bucket_slot, bucket)
                            .write_after_insert(src, value_slot, &attempt_edge)?
                            .bucket();
                        self.graph.publish_bucket_descriptor(bucket_slot, bucket)?;
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
                            return Ok(Some(ScalarInsertLocation {
                                logical_slot: written_slot,
                                storage: ScalarInsertStorage::Slab,
                            }));
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
                            if new_stored != bucket.stored_slots() {
                                bucket = bucket.with_stored_slots(new_stored);
                            }
                            let bucket = self
                                .graph
                                .value_plane(bucket_slot, bucket)
                                .write_after_insert(src, written_slot, &attempt_edge)?
                                .bucket();
                            self.graph.publish_bucket_descriptor(bucket_slot, bucket)?;
                            return Ok(Some(ScalarInsertLocation {
                                logical_slot: written_slot,
                                storage: ScalarInsertStorage::Slab,
                            }));
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
                                // A spilled row's slot is `stored_slots + its run index`, and the run
                                // index of the row just appended is `used_len - 1`. Falling back to
                                // `stored_slots` alone would name the run's first row instead of the
                                // new one whenever the run was not empty (ADR 0099 mirror).
                                let value_slot = match logical_slot {
                                    Some(slot) => slot,
                                    None => bucket.stored_slots().saturating_add(
                                        self.graph
                                            .bucket_values_run_rows(&bucket)
                                            .saturating_sub(1),
                                    ),
                                };
                                let bucket = self
                                    .graph
                                    .value_plane(bucket_slot, bucket)
                                    .write_after_insert(src, value_slot, &attempt_edge)?
                                    .bucket();
                                self.graph.publish_bucket_descriptor(bucket_slot, bucket)?;
                            }
                            return Ok(logical_slot.map(|logical_slot| ScalarInsertLocation {
                                logical_slot,
                                storage: ScalarInsertStorage::Spill,
                            }));
                        }
                        Err(LaraOperationError::SpillNeedsPromotion) => {
                            // The bucket's run cannot serve another row (ADR 0097 §6): its class is
                            // full at MAX_ROWS, or the arena cannot address the growth. Tree mode is
                            // the chunked path with no fixed row ceiling, so promote and re-dispatch
                            // through the mode dispatcher - exactly as the tiny->slab promotion
                            // does, because the bucket is no longer slab mode and this loop's
                            // remaining steps do not apply to it. This replaces the retired
                            // log-full leaf rebalance: a per-bucket run has no shared area to empty.
                            super::promote::promote_bucket_to_tree_mode(self.graph, src, label_id)?;
                            return self.graph.insert_edge_skip_leaf_cascade_impl(
                                src,
                                label_id,
                                edge,
                                placement,
                                location_capture,
                            );
                        }
                        Err(e) => return Err(LabeledOperationError::from(e)),
                    }
                }
                // ADR 0099: the values fold that used to retry here is gone, so this block runs
                // once; reaching the end means the bucket could not place the row after promotion
                // last attempt's outcome is reported: the spill still needs promotion.
                Err(LaraOperationError::SpillNeedsPromotion.into())
            }
        }
    }
    /// Removes one row, reporting the bounded survivor slots that moved.
    ///
    /// ADR 0100: the storage-class dispatch is inside the operation, so callers cannot inherit a path
    /// by accident. Moved unchanged from the graph's `remove_edge_at_slot_with_move`.
    pub(super) fn remove_row(
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
                        let _ = super::tree_write::tree_mode_demote_to_slab::<E, M>(
                            self.graph, src, slot, label_id, &updated,
                        );
                    }
                }
                Ok(removed.map(|removed| EdgeRemoval {
                    removed: removed.with_slot_index(slot_index),
                    moves: Vec::new(),
                }))
            }
            BucketMode::Slab => self
                .graph
                .remove_bucket_edge_at_slot(src, &vertex, slot, bucket, slot_index),
        }
    }
}
