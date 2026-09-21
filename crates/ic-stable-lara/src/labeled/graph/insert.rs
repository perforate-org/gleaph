//! Labeled graph `insert` implementation.

#[cfg(target_family = "wasm")]
fn log_collect_overflow(message: &str) {
    ic_cdk::println!("LARA CollectAllocationOverflow: {}", message);
}

#[cfg(not(target_family = "wasm"))]
fn log_collect_overflow(_message: &str) {}

use crate::{
    VertexId,
    labeled::{
        access::LabelEdgeSpanAccess,
        bucket_label_key::BucketLabelKey,
        record::{LabelBucket, LabeledVertex},
        slot_index::checked_add_slot_index,
    },
    lara::{
        edge::{InsertLocation, segment_tree_leaf_count},
        operation_error::LaraOperationError,
    },
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{BucketMode, BucketSearch, LabeledLaraGraph};

/// Exact logical location produced by a successful scalar write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScalarInsertLocation {
    /// Logical slot within the owning label row.
    pub logical_slot: u32,
    /// Physical storage class selected by the insert.
    pub storage: ScalarInsertStorage,
}

/// Physical storage class for an exact scalar insertion location.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScalarInsertStorage {
    /// The edge was written to the bucket's slab span.
    Slab,
    /// The edge was written to the owning leaf's overflow log.
    OverflowLog,
}

/// Storage-owned placement policy for scalar edge writes on a labeled bucket
/// (ADR 0052 §5/§6).
///
/// The Graph layer maps its resolved ordering policy to this enum at the
/// mutation boundary; LARA never parses GQL or reads Router catalogs and does
/// not own a duplicate schema map (ADR 0052 §4).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EdgePlacementPolicy {
    /// Order is not semantically meaningful: reuse an in-slab tombstone before
    /// appending to the slab tail or the overflow log (ADR 0052 §5).
    #[default]
    Unordered,
    /// Bucket-local live order is the semantic insertion order: append only,
    /// never reuse an interior tombstone (ADR 0052 §6).
    Insertion,
}

#[derive(Clone, Copy)]
enum ScalarLocationCapture {
    Ignore,
    Capture,
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Appends a vertex row and grows segment metadata when a new leaf is needed.
    pub(crate) fn push_vertex(
        &self,
        mut vertex: LabeledVertex,
    ) -> Result<VertexId, LabeledOperationError> {
        vertex.ensure_valid_normal_row()?;
        let id = self.vertices.len();
        if id > 0 {
            let prev_end = self.vertex_bucket_descriptor_row_end(VertexId::from(id - 1))?;
            if vertex.base_slot_start() < prev_end {
                vertex = vertex.with_base_slot_start(prev_end);
            }
        }
        self.vertices
            .push(vertex)
            .map_err(LabeledOperationError::from)?;
        let header = self.edges.header();
        let target = segment_tree_leaf_count(self.vertices.len().into(), header.segment_size);
        if target > header.segment_count {
            self.edges
                .grow_segment_tree_to(target)
                .map_err(LabeledOperationError::from)?;
            self.values
                .grow_segment_count_to(target)
                .map_err(LabeledOperationError::from)?;
        }
        Ok(VertexId::from(id))
    }

    /// Append several vertex rows while growing edge/value segment metadata once for the final
    /// vertex count. Row order and the existing monotonic bucket-base correction are preserved.
    pub(crate) fn push_vertices(
        &self,
        vertices: impl IntoIterator<Item = LabeledVertex>,
    ) -> Result<Vec<VertexId>, LabeledOperationError> {
        let mut vertices: Vec<_> = vertices.into_iter().collect();
        if vertices.is_empty() {
            return Ok(Vec::new());
        }
        let start = self.vertices.len();
        let mut previous_end = if start == 0 {
            None
        } else {
            Some(self.vertex_bucket_descriptor_row_end(VertexId::from(start - 1))?)
        };
        for vertex in &mut vertices {
            vertex.ensure_valid_normal_row()?;
            if let Some(previous_end) = previous_end
                && vertex.base_slot_start() < previous_end
            {
                *vertex = vertex.with_base_slot_start(previous_end);
            }
            previous_end = Some(if vertex.degree() == 0 {
                vertex.base_slot_start()
            } else if vertex.is_default_edge_labeled() {
                crate::labeled::slot_index::checked_add_slot_index(
                    vertex.base_slot_start(),
                    u64::from(vertex.stored_degree()),
                )
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
            } else {
                return Err(LaraOperationError::CollectAllocationOverflow.into());
            });
        }
        self.vertices
            .push_many(vertices)
            .map_err(LabeledOperationError::from)?;
        let header = self.edges.header();
        let target = segment_tree_leaf_count(self.vertices.len().into(), header.segment_size);
        if target > header.segment_count {
            self.edges
                .grow_segment_tree_to(target)
                .map_err(LabeledOperationError::from)?;
            self.values
                .grow_segment_count_to(target)
                .map_err(LabeledOperationError::from)?;
        }
        Ok((start..self.vertices.len()).map(VertexId::from).collect())
    }

    /// Compacts the label-bucket descriptor segment containing `vid`.
    pub(crate) fn compact_label_bucket_vertex_segment(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError> {
        self.ensure_vertex(vid)?;
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_compact_label_bucket_vertex_segment");
        self.buckets
            .compact_vertex_segment_for_vertex(&self.vertices, vid)
            .map_err(LabeledOperationError::from)?;
        self.invalidate_bucket_lookup_caches_for_bucket_segment(vid)?;
        Ok(())
    }

    /// Inserts `edge` into the bucket identified by `label_id` for `src`.
    pub(crate) fn insert_edge(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
        placement: EdgePlacementPolicy,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.insert_edge_skip_leaf_cascade(src, label_id, edge, placement)?;
        if self.labeled_leaf_segment_is_dense(src) {
            self.rebalance_cascade_after_labeled_mutation(src)?;
        }
        #[cfg(debug_assertions)]
        self.assert_no_labeled_leaf_mate_overlap(src);
        Ok(())
    }

    pub(crate) fn insert_edge_skip_leaf_cascade(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
        placement: EdgePlacementPolicy,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.insert_edge_skip_leaf_cascade_impl(
            src,
            label_id,
            edge,
            placement,
            ScalarLocationCapture::Ignore,
        )
        .map(|_| ())
    }

    pub(crate) fn insert_edge_skip_leaf_cascade_with_location(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
        placement: EdgePlacementPolicy,
    ) -> Result<Option<ScalarInsertLocation>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.insert_edge_skip_leaf_cascade_impl(
            src,
            label_id,
            edge,
            placement,
            ScalarLocationCapture::Capture,
        )
    }

    #[allow(clippy::needless_return)]
    fn insert_edge_skip_leaf_cascade_impl(
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
        self.ensure_vertex(src)?;
        let mut vertex = self.vertices.get(src);
        let edge_inline_property_width = edge.edge_inline_property_byte_width();
        let has_edge_inline_property = edge_inline_property_width != 0;
        if vertex.is_default_edge_labeled() {
            if has_edge_inline_property {
                return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
                    bucket_width: 0,
                    edge_inline_property_width,
                });
            }
            if label_id == self.bypass_storage_label_for(&vertex)
                && self.may_use_homogeneous_bypass(src)
            {
                self.insert_homogeneous_bypass_edge(src, label_id, edge)?;
                return Ok(None);
            }
            // A same-label insert into a bypass row that stopped being the tail
            // must not extend its slab region: every such insert would rescan
            // and rewrite all later rows' origins. Promote once so the insert —
            // and every future one for this row — takes the bounded bucket path.
            self.promote_bypass_to_bucket_mode(src)?;
            vertex = self.vertices.get(src);
        } else if vertex.degree() == 0
            && self.is_homogeneous_bypass_label(label_id)
            && self.may_use_homogeneous_bypass(src)
            && !has_edge_inline_property
        {
            self.insert_homogeneous_bypass(src, label_id, edge)?;
            return Ok(None);
        }

        if edge_inline_property_width != 0
            && let BucketSearch::Missing { .. } = self.find_bucket(src, &vertex, label_id)?
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

        let (bucket_slot, mut bucket) = self.find_or_create_bucket(src, &vertex, label_id)?;
        // ADR 0096 §7: match-first dispatch — mode decides before any
        // storage-class read, so a future mode cannot silently inherit a path.
        // Width is only readable inside the Slab arm below (tiny payload and
        // tree roots never reach a width read).
        match BucketMode::from_bucket(&bucket) {
            // ADR 0096 §5: tiny buckets append inline (no slab/log/counts
            // writes). The arm precedes every width read structurally: width
            // bytes are payload on tiny (see §1 wire table) and must never be
            // read as schema.
            BucketMode::Tiny => self.insert_edge_tiny_mode(
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
                    self,
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
                    self,
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
                        .ensure_bucket_inline_property_schema_for_insert_with_materialize(
                            src,
                            bucket_slot,
                            bucket,
                            edge_inline_property_width,
                        )?;
                    self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                }
                // Plan 0318 §Step 6 promote trigger: if the slab bucket has
                // reached T_PROMOTE, promote it to tree mode and recurse through
                // the new descriptor. The trigger is the placeholder-gap form
                // (`stored_slots >= T_PROMOTE`); the `compute_bucket_allocation`
                // form is used when the weighted gap is introduced.
                //
                // The additional `< u32::MAX` guard skips promotion for buckets
                // that are already at the degree cap (e.g. `normal_label_bucket_insert_rejects_edge_len_overflow`).
                // Those buckets fail with `RowDegreeOverflow` from the slab path
                // instead of being routed to the tree path, which would otherwise
                // try to mint `stored_slots / B` LTB blocks and fail in a
                // different way.
                //
                // The `E::BYTES == 4` carve-out keeps wide edge types on the slab
                // path: tree mode stores one 4-byte target per LTB slot (ADR 0088
                // §1), so a wider `E` can never be promoted. Mirrors the
                // inline-property carve-out — such buckets stay slab and keep the
                // pre-Plan-0318 growth behavior. (Post-merge fix: the canbench
                // `bench_l_s2_det_sat_4096` bench drives a 10-byte edge type and
                // trapped on the tree-append typed guard after the promotion had
                // already mis-transcribed — promote now rejects before minting.)
                if bucket.stored_slots() >= super::T_PROMOTE
                    && bucket.stored_slots() < u32::MAX
                    && E::BYTES == super::tree_write::TREE_MODE_REQUIRED_EDGE_BYTES
                {
                    super::tree_write::promote_bucket_if_needed(self, src, label_id)?;
                    // Re-read the bucket: after promotion it is tree mode.
                    let vertex = self.vertices.get(src);
                    bucket = match self.find_bucket(src, &vertex, label_id)? {
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
                                self,
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
                            self,
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
                self.ensure_bucket_slack_insert_when_peers_have_values(src, &vertex)?;
                let vertex = self.vertices.get(src);
                let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, bucket_slot)?;
                for _attempt in 0..64u32 {
                    let attempt_edge = edge.clone();
                    let vertex = self.vertices.get(src);
                    if has_edge_inline_property
                        && bucket.inline_property_bytes_log_len() > 0
                        && self.values.inline_property_bytes_log_segment_is_full(
                            self.inline_property_bytes_log_leaf(src),
                        )
                    {
                        self.rebalance_inline_property_bytes_log_leaf_for_labeled(src)?;
                        let vertex = self.vertices.get(src);
                        let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                        bucket = self
                            .buckets
                            .read_label_bucket_slot(bucket_slot)
                            .ok_or_else(|| {
                                log_collect_overflow(
                                    "insert_edge_skip_leaf_cascade: cannot re-read bucket after inline property bytes log rebalance",
                                );
                                LaraOperationError::CollectAllocationOverflow
                            })?;
                        continue;
                    }
                    // Unordered placement (ADR 0052 §5 step 1): reuse an in-slab
                    // tombstone before appending to the slab tail or the overflow log.
                    // The helper keeps the dense fast path O(1) and falls back to the
                    // ordered path when the inline property bytes are log-backed
                    // (ADR 0052 §9).
                    if placement == EdgePlacementPolicy::Unordered
                        && let Some(location) = self.try_reuse_unordered_slab_tombstone(
                            src,
                            bucket_slot,
                            bucket,
                            &attempt_edge,
                        )?
                    {
                        return Ok(Some(location));
                    }
                    let successor_start = if vertex.degree() == 1 && !has_edge_inline_property {
                        self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?
                    } else {
                        self.bucket_slab_window_end_exclusive_after_bucket(
                            &vertex,
                            bucket_index,
                            &bucket,
                        )?
                    };
                    let slack_span = successor_start.saturating_sub(bucket.edge_start());
                    if bucket.overflow_log_head() < 0
                        && bucket.stored_slots() > 0
                        && slack_span > u64::from(bucket.stored_slots())
                    {
                        let write_slot = checked_add_slot_index(
                            bucket.edge_start(),
                            u64::from(bucket.stored_slots()),
                        )
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        debug_assert!(write_slot < successor_start);
                        self.edges.write_slot(write_slot, attempt_edge.clone())?;
                        let logical_slot = bucket.stored_slots();
                        let bucket = bucket.grow_packed_slab_by_one();
                        let bucket = self.write_edge_inline_property_after_insert(
                            src,
                            bucket_slot,
                            bucket,
                            &attempt_edge,
                        )?;
                        self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                        let hdr = self.edges.header();
                        let next_num_edges = hdr
                            .num_edges
                            .checked_add(1)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        self.edges.set_num_edges(next_num_edges);
                        self.edges
                            .bump_vertex_segment_counts(src, 1, 0)
                            .map_err(LabeledOperationError::from)?;
                        return Ok(Some(ScalarInsertLocation {
                            logical_slot,
                            storage: ScalarInsertStorage::Slab,
                        }));
                    }
                    let access = LabelEdgeSpanAccess::with_bucket(
                        &self.buckets,
                        bucket_slot,
                        bucket,
                        successor_start,
                        src,
                    );
                    let insert_result = match location_capture {
                        ScalarLocationCapture::Ignore => {
                            self.edges.insert_edge_without_logical_slot(
                                &access,
                                VertexId::from(0),
                                attempt_edge.clone(),
                            )
                        }
                        ScalarLocationCapture::Capture => {
                            self.edges
                                .insert_edge(&access, VertexId::from(0), attempt_edge.clone())
                        }
                    };
                    match insert_result {
                        Ok(InsertLocation::Slab(written_slot)) if !has_edge_inline_property => {
                            return Ok(Some(ScalarInsertLocation {
                                logical_slot: written_slot,
                                storage: ScalarInsertStorage::Slab,
                            }));
                        }
                        Ok(InsertLocation::Slab(written_slot)) => {
                            bucket = self
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
                            let bucket = self.write_edge_inline_property_after_insert(
                                src,
                                bucket_slot,
                                bucket,
                                &attempt_edge,
                            )?;
                            self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                            return Ok(Some(ScalarInsertLocation {
                                logical_slot: written_slot,
                                storage: ScalarInsertStorage::Slab,
                            }));
                        }
                        Ok(InsertLocation::Log { logical_slot, .. })
                            if !has_edge_inline_property =>
                        {
                            return Ok(Some(ScalarInsertLocation {
                                logical_slot,
                                storage: ScalarInsertStorage::OverflowLog,
                            }));
                        }
                        Ok(InsertLocation::Log { logical_slot, .. }) => {
                            bucket = self
                                .buckets
                                .read_label_bucket_slot(bucket_slot)
                                .ok_or_else(|| {
                                    log_collect_overflow(
                                        "insert_edge_skip_leaf_cascade: cannot re-read bucket after log insert",
                                    );
                                    LaraOperationError::CollectAllocationOverflow
                                })?;
                            let bucket = self.write_edge_inline_property_after_insert(
                                src,
                                bucket_slot,
                                bucket,
                                &attempt_edge,
                            )?;
                            self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                            return Ok(Some(ScalarInsertLocation {
                                logical_slot,
                                storage: ScalarInsertStorage::OverflowLog,
                            }));
                        }
                        Ok(InsertLocation::LogOnly { .. }) => {
                            if has_edge_inline_property {
                                bucket = self
                                    .buckets
                                    .read_label_bucket_slot(bucket_slot)
                                    .ok_or_else(|| {
                                        log_collect_overflow(
                                            "insert_edge_skip_leaf_cascade: cannot re-read bucket after log insert",
                                        );
                                        LaraOperationError::CollectAllocationOverflow
                                    })?;
                                let bucket = self.write_edge_inline_property_after_insert(
                                    src,
                                    bucket_slot,
                                    bucket,
                                    &attempt_edge,
                                )?;
                                self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                            }
                            return Ok(None);
                        }
                        Err(LaraOperationError::SegmentLogFull) => {
                            let vertex = self.vertices.get(src);
                            if vertex.is_default_edge_labeled()
                                && !has_edge_inline_property
                                && label_id == self.bypass_storage_label_for(&vertex)
                            {
                                self.insert_homogeneous_bypass_edge(src, label_id, attempt_edge)?;
                                return Ok(None);
                            }
                            self.rebalance_edge_log_leaf_for_labeled(src, true, true)?;
                            let vertex = self.vertices.get(src);
                            let bucket_slot =
                                Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                            bucket = self
                                .buckets
                                .read_label_bucket_slot(bucket_slot)
                                .ok_or_else(|| {
                                    log_collect_overflow(
                                        "insert_edge_skip_leaf_cascade: cannot re-read bucket after log rebalance",
                                    );
                                    LaraOperationError::CollectAllocationOverflow
                                })?;
                        }
                        Err(e) => return Err(LabeledOperationError::from(e)),
                    }
                }
                Err(LabeledOperationError::from(
                    LaraOperationError::SegmentLogFull,
                ))
            }
        }
    }

    /// Storage-owned pre-insert capacity preparation for a new label bucket.
    ///
    /// For wider-than-4-byte edge types (slab birth): when the next ordinary
    /// insert will create a new bucket for `(src, label_id)`, the bucket needs
    /// a free configured per-vertex quota span inside `src`'s pinned PMA leaf
    /// block. If the leaf is already dense or no free span fits, this helper
    /// rebalances / relocates the leaf *before* any canonical edge write,
    /// keeping the subsequent `find_or_create_bucket` path fail-closed.
    ///
    /// For 4-byte edge types (tiny birth, ADR 0096 §4) this is vertex validation
    /// only: spanless buckets need no quota, pin, or pre-rebalance. Pin defers
    /// to promotion and slab growth paths, which pin on demand.
    ///
    /// The operation is idempotent; any error leaves canonical edge state untouched.
    /// Pinning a previously unpinned leaf is non-canonical physical preallocation and is
    /// safe to retain after a rejected mutation.
    pub(crate) fn prepare_labeled_edge_capacity_for_insert(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        if vertex.degree() > 0
            && matches!(
                self.find_bucket(src, &vertex, label_id)?,
                BucketSearch::Found { .. }
            )
        {
            return Ok(());
        }

        // New-bucket contract (ADR 0001): later buckets are created with
        // stored_slots=0 at the successor boundary. The first bucket on a vertex
        // receives the configured initial quota so a one-edge vertex stays on slab
        // instead of immediately entering the shared leaf overflow log.
        // ADR 0096 §4 (birth flip): 4-byte-edge buckets are born tiny and need
        // no leaf pin, quota, or pre-rebalance — pin defers to promotion and
        // slab growth paths, which pin on demand. Wider edge types keep the
        // anticipatory pin below.
        if E::BYTES != 4 {
            self.ensure_labeled_leaf_block_pinned(src)?;
        }
        Ok(())
    }

    pub(crate) fn insert_edge_skip_leaf_cascade_deferred_inline_property(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
        placement: EdgePlacementPolicy,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let was_deferred = self.inline_property_bytes_compaction_deferred.replace(true);
        let result = self.insert_edge_skip_leaf_cascade_impl(
            src,
            label_id,
            edge,
            placement,
            ScalarLocationCapture::Ignore,
        );
        self.inline_property_bytes_compaction_deferred
            .set(was_deferred);
        result.map(|_| ())
    }

    pub(crate) fn insert_edge_skip_leaf_cascade_deferred_inline_property_with_location(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
        placement: EdgePlacementPolicy,
    ) -> Result<Option<ScalarInsertLocation>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let was_deferred = self.inline_property_bytes_compaction_deferred.replace(true);
        let result =
            self.insert_edge_skip_leaf_cascade_with_location(src, label_id, edge, placement);
        self.inline_property_bytes_compaction_deferred
            .set(was_deferred);
        result
    }

    /// Attempts an Unordered in-slab tombstone reuse before the tail/log append
    /// paths (ADR 0052 §5 step 1).
    ///
    /// Returns `None` without touching state when the bucket is dense (O(1) fast
    /// path), when its inline property bytes are log-backed (ADR 0052 §9: the
    /// reused middle ordinal cannot be synchronized with a bytes log), or when
    /// the slab prefix holds no tombstone. On success the edge is written at the
    /// reused physical slot, the bucket degree grows by one with `stored_slots`
    /// unchanged, and slab-backed inline property bytes are inserted at the
    /// reused slot's live ordinal with later bytes shifted up so their values
    /// are preserved (ADR 0052 §9).
    fn try_reuse_unordered_slab_tombstone(
        &self,
        src: VertexId,
        bucket_slot: u64,
        mut bucket: LabelBucket,
        edge: &E,
    ) -> Result<Option<ScalarInsertLocation>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        // O(1) fast path: a dense bucket has no slab tombstone to reuse. For a
        // slab-only bucket, `stored_slots > degree` is exactly "a slab tombstone
        // exists". For a log-backed bucket it is a sufficient condition (slab
        // tombstones outnumber live overflow-log edges); the conservative miss
        // (`log_live >= tombs`) defers those tombstones to fold/compaction,
        // which is the pre-slice behavior and avoids an O(log-chain) walk on
        // every insert (ADR 0052 §5, Slice 3 implementation note).
        if bucket.stored_slots() <= bucket.degree() {
            return Ok(None);
        }
        if bucket.inline_property_bytes_log_head() >= 0 {
            return Ok(None);
        }
        let mut ordinal_before = 0u32;
        let mut reused_slot = None;
        for slot_index in 0..bucket.stored_slots() {
            let physical_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if self.edges.read_slot(physical_slot).is_tombstone_edge() {
                reused_slot = Some(slot_index);
                break;
            }
            ordinal_before += 1;
        }
        let Some(reused_slot) = reused_slot else {
            return Ok(None);
        };
        let width = bucket.inline_property_byte_width();
        let has_inline_property = width != 0 && edge.edge_inline_property_byte_width() != 0;
        if has_inline_property {
            let old_offset = bucket.inline_property_bytes_offset();
            let trailing_slots = bucket.degree().saturating_sub(ordinal_before);
            let trailing_len = u64::from(trailing_slots)
                .checked_mul(u64::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut trailing = vec![
                0u8;
                usize::try_from(trailing_len).map_err(|_| {
                    LaraOperationError::CollectAllocationOverflow
                })?
            ];
            if trailing_len > 0 {
                let source = old_offset
                    .checked_add(u64::from(ordinal_before) * u64::from(width))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                self.values.read_bytes(source, &mut trailing);
            }
            bucket = bucket.after_slab_insert_reuse_tail_tombstone();
            let previous_slab_slots = bucket.inline_property_bytes_slab_slots();
            bucket = self.ensure_bucket_inline_property_bytes_span(
                src,
                bucket_slot,
                bucket,
                previous_slab_slots,
            )?;
            if trailing_len > 0 {
                let destination = (u64::from(ordinal_before) + 1)
                    .checked_mul(u64::from(width))
                    .and_then(|offset| bucket.inline_property_bytes_offset().checked_add(offset))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                self.values
                    .write_bytes(destination, &trailing)
                    .map_err(LabeledOperationError::from)?;
            }
            self.write_edge_inline_property_at_slot(&bucket, ordinal_before, edge)?;
        } else {
            bucket = bucket.after_slab_insert_reuse_tail_tombstone();
        }
        let physical_slot = checked_add_slot_index(bucket.edge_start(), u64::from(reused_slot))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.edges.write_slot(physical_slot, edge.clone())?;
        self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
        let hdr = self.edges.header();
        let next_num_edges = hdr
            .num_edges
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.edges.set_num_edges(next_num_edges);
        self.edges
            .bump_vertex_segment_counts(src, 1, 0)
            .map_err(LabeledOperationError::from)?;
        Ok(Some(ScalarInsertLocation {
            logical_slot: reused_slot,
            storage: ScalarInsertStorage::Slab,
        }))
    }

    pub(super) fn ensure_labeled_bucket_edge_span_room(
        &self,
        src: VertexId,
        bucket_index: u32,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(src);
        let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
        if self.try_place_new_bucket_edge_span(src, &vertex, slot, bucket_index)? {
            return Ok(());
        }
        log_collect_overflow(
            "ensure_labeled_bucket_edge_span_room: new bucket span placement failed",
        );
        Err(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn find_or_create_bucket(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        label_id: BucketLabelKey,
    ) -> Result<(u64, LabelBucket), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let insert_index = match self.find_bucket(src, vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => return Ok((slot, bucket)),
            BucketSearch::Missing { insert_index } => insert_index,
        };
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_insert_new_label_bucket");
        let (slot, rewrote_bucket_segment) = self
            .buckets
            .insert_label_bucket_at(
                &self.vertices,
                src,
                LabelBucket::default().with_bucket_label_key(label_id),
                insert_index,
            )
            .map_err(LabeledOperationError::from)?;
        if rewrote_bucket_segment {
            self.invalidate_bucket_lookup_caches_for_bucket_segment(src)?;
        }
        self.ensure_vertex_bucket_row_origin(src)?;
        let vertex = self.vertices.get(src);
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        // ADR 0096 §4 (birth flip): 4-byte-edge buckets are born tiny
        // (spanless, pinless, quotaless). Wider edge types keep the slab birth
        // path below: tiny stores 4-byte targets and transcription would be
        // lossy (mirrors the tree carve-out at the insert dispatcher). The
        // E::BYTES gate monomorphizes away per instantiation.
        if E::BYTES == 4 {
            let anchor =
                self.bucket_successor_start_after_bucket_for_new_bucket(&vertex, bucket_index)?;
            let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(bucket_slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            // Fresh descriptor (degree 0, no log, no values) always satisfies
            // the tiny pre-checks; failure here means the creation path itself
            // changed shape and must be re-examined, never ignored.
            let bucket = bucket
                .try_enable_tiny_mode()
                .map_err(LaraOperationError::from)?;
            let bucket = bucket.with_edge_range(anchor, 0);
            self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
            self.cache_bucket_lookup(src, label_id, &vertex, bucket_slot);
            return Ok((bucket_slot, bucket));
        }
        self.ensure_labeled_bucket_edge_span_room(src, bucket_index)?;
        let vertex = self.vertices.get(src);
        let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
        let bucket = self
            .buckets
            .read_label_bucket_slot(bucket_slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.cache_bucket_lookup(src, label_id, &vertex, bucket_slot);
        Ok((bucket_slot, bucket))
    }

    pub(super) fn try_place_new_bucket_edge_span(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        slot: u64,
        bucket_index: u32,
    ) -> Result<bool, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(false);
        }

        // Pin even the first bucket's leaf before publishing its initial position. The
        // pin owns physical capacity and PMA total counts; the first bucket receives
        // only the configured per-vertex initial quota from that block.
        self.ensure_labeled_leaf_block_pinned(src)?;

        // Later buckets are inserted with `stored_slots = 0`. Their edge_start is
        // placed at the successor bucket's edge_start (or the preceding bucket's end
        // for the last bucket), giving them zero-length reservations. The first bucket
        // receives the initial quota and can accept its first edge directly on slab.
        let edge_start = if bucket_index == 0 {
            // The first bucket needs a valid physical anchor inside its pinned leaf. A
            // leaf mate may already own more than its fixed quota;
            // once this descriptor exists, relocate the whole leaf so the new active
            // vertex participates in the weighted layout before retrying the anchor.
            // Later buckets inherit an existing bucket boundary.
            match self.ensure_labeled_leaf_edge_physical_pin(src) {
                Ok(base) => base,
                Err(LabeledOperationError::Store(
                    LaraOperationError::CollectAllocationOverflow,
                )) => {
                    self.relocate_labeled_leaf_physical_block(src)?;
                    self.labeled_edge_base_from_first_bucket(src)?
                }
                Err(error) => return Err(error),
            }
        } else {
            self.bucket_successor_start_after_bucket_for_new_bucket(vertex, bucket_index)?
        };
        let initial_slots = if bucket_index == 0 && vertex.degree() == 1 {
            crate::labeled::graph::leaf_pin::labeled_leaf_initial_bucket_quota(
                self.edges.header().segment_size,
            )
        } else {
            0
        };
        let bucket = self.buckets.read_label_bucket_slot(slot).ok_or_else(|| {
            log_collect_overflow("try_place_new_bucket_edge_span: cannot read new bucket slot");
            LaraOperationError::CollectAllocationOverflow
        })?;
        // ADR 0096 §4b capacity contract: span placement never serves tiny
        // buckets (R2b creation branches before this fn via exhaustive match).
        // The sole allocator for a tiny-bit bucket is the tiny→slab promotion
        // reserve, which does not route through here.
        debug_assert!(
            !bucket.is_tiny_mode(),
            "tiny bucket must not reach span placement"
        );
        let bucket = bucket
            .with_edge_range(edge_start, 0)
            .with_overflow_log_head(-1);
        self.buckets.write_label_bucket_slot(slot, bucket)?;
        if initial_slots > 0 {
            self.vertices
                .set(src, &vertex.with_stored_slots(initial_slots));
        }
        Ok(true)
    }

    /// Inserts one edge into a tiny-mode bucket (ADR 0096 §5 + delete redesign).
    ///
    /// No slab write, no log admission, no successor read, no leaf `actual`
    /// bump (tiny edges occupy no leaf slots). The global `num_edges` census
    /// still counts the live edge (mirrors the slab arm). Placement parity
    /// with slab (ADR 0052 §6): `Unordered` fills the first tombstone hole;
    /// `Insertion` dense-appends (promoting when the prefix is full).
    /// Survivors keep slots either way. Width-carrying edges and hole-less
    /// full prefixes promote first (ADR 0096 §4); the pending edge then flows
    /// through the normal slab path via one recursion (depth 1: the
    /// post-promotion bucket is always slab).
    fn insert_edge_tiny_mode(
        &self,
        src: VertexId,
        bucket_slot: u64,
        bucket: LabelBucket,
        label_id: BucketLabelKey,
        edge: E,
        _placement: EdgePlacementPolicy,
        location_capture: ScalarLocationCapture,
    ) -> Result<Option<ScalarInsertLocation>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        // Width bytes are payload on tiny: a width-carrying edge can never be
        // stored inline. Promote first (transcribes live targets only —
        // tombstone holes excluded; tiny buckets hold no values), then let the
        // normal schema path handle the width.
        // Promotion triggers: width-carrying edge, or prefix full with no
        // hole to fill (`stored >= TINY_MAX_DEGREE` and every slot live).
        // Holed prefixes hole-fill below (no promotion). A leaf too tight for
        // the pending span relocates and retries, exactly like slab growth: one
        // relocate can leave the block one growth quantum short (K=4 promotes a
        // full inline prefix, so promotions land later and collide more often),
        // so the retry budget matches the slab append loop's and persistent
        // failure still propagates.
        // Hole scan (Unordered only — Insertion appends per ADR 0052 §6,
        // uniformly across modes; survivors keep slots either way).
        let mut hole: Option<u32> = None;
        if _placement == EdgePlacementPolicy::Unordered {
            for i in 0..bucket.tiny_used_width() {
                // Layout-native liveness (same predicate as slab/tree read paths).
                if E::read_from(&bucket.tiny_target(i).to_le_bytes()).is_deleted_slot() {
                    hole = Some(i);
                    break;
                }
            }
        }
        if edge.edge_inline_property_byte_width() != 0
            || (hole.is_none() && bucket.tiny_used_width() >= LabelBucket::TINY_MAX_DEGREE)
        {
            self.promote_tiny_to_slab_with_growth(src, bucket_slot, &bucket)?;
            return self.insert_edge_skip_leaf_cascade_impl(
                src,
                label_id,
                edge,
                _placement,
                location_capture,
            );
        }
        debug_assert!(
            E::BYTES == 4,
            "tiny buckets require 4-byte edges (birth gate)"
        );
        // Unordered hole-fill (found above) or dense append at the used width
        // (Insertion's tail ordinal); the used width grows only on append while
        // `degree` (live) always +1.
        let logical_slot = hole.unwrap_or(bucket.tiny_used_width());
        let grown = bucket.with_degree_field(
            bucket
                .degree()
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?,
        );
        let grown = if hole.is_some() {
            grown
        } else {
            grown.with_tiny_used_width(
                bucket
                    .tiny_used_width()
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?,
            )
        };
        let grown = grown.with_tiny_target(logical_slot, u32::from(edge.neighbor_vid()));
        let grown = self.write_edge_inline_property_after_insert(src, bucket_slot, grown, &edge)?;
        self.buckets.write_label_bucket_slot(bucket_slot, grown)?;
        let hdr = self.edges.header();
        let next_num_edges = hdr
            .num_edges
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.edges.set_num_edges(next_num_edges);
        Ok(Some(ScalarInsertLocation {
            logical_slot,
            // Storage class reuses Slab for "not-overflow-log" (tree-append
            // precedent at the tree-mode branch); the ordinal is the slot.
            storage: ScalarInsertStorage::Slab,
        }))
    }

    /// Promotes a tiny bucket to the slab from the insert trigger, growing the
    /// leaf block when the pending span does not fit.
    ///
    /// One relocate can leave the block a growth quantum short, and K=4
    /// promotes a *full* inline prefix, so promotions land later in the leaf's
    /// life and collide far more often than at K=3. The retry budget therefore
    /// mirrors the slab append loop's; persistent failure still propagates
    /// (fail-closed) and the caller treats it as the leaf-pressure signal.
    ///
    /// Scoped to the insert trigger (the dense-promotion path). The inline
    /// property width-declaration path promotes single-shot: it declares schema
    /// on a bucket with no pending edge and surfaces the typed pressure error
    /// to its caller instead (ADR 0096 §4b).
    pub(super) fn promote_tiny_to_slab_with_growth(
        &self,
        src: VertexId,
        bucket_slot: u64,
        bucket: &LabelBucket,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        for _attempt in 0..64u32 {
            match self.promote_tiny_to_slab(src, bucket_slot, bucket) {
                Ok(()) => return Ok(()),
                Err(LabeledOperationError::Store(
                    LaraOperationError::CollectAllocationOverflow,
                )) => self.relocate_labeled_leaf_physical_block(src)?,
                Err(other) => return Err(other),
            }
        }
        Err(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Promotes a tiny-mode bucket to slab (ADR 0096 §4): reserve, transcribe,
    /// publish. The ONLY span admission reachable with the tiny bit set.
    ///
    /// Reserve (all fallible grows complete here): extend the vertex cover
    /// contiguously (pin + first-fit for the first span), reserving a
    /// `(degree + 1)`-slot run for the transcribed edges plus the pending edge.
    /// Full blocks fail closed here (slab-growth error family); the
    /// insert/remove dispatchers retry once after relocate.
    /// Commit: transcribe targets as raw 4-byte slots, publish one fresh slab
    /// descriptor, extend the vertex span, bump leaf `actual` by the transcribed
    /// count (never counted before). Global `num_edges` is untouched by the
    /// transcription (live count unchanged); the pending edge bumps it via the
    /// fall-through slab insert.
    pub(super) fn promote_tiny_to_slab(
        &self,
        src: VertexId,
        bucket_slot: u64,
        bucket: &LabelBucket,
    ) -> Result<(), LabeledOperationError> {
        debug_assert!(
            bucket.is_tiny_mode(),
            "tiny promotion requires a tiny-mode bucket"
        );
        debug_assert!(
            E::BYTES == 4,
            "tiny buckets require 4-byte edges (birth gate)"
        );
        let degree = bucket.degree();
        let span_len = degree
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        // --- Reserve + commit, fail-closed (ADR 0096 §4, R2b) ---
        // Vertex-span tiling is load-bearing (occupied spans, audit, release
        // all read vertex spans as contiguous cover): spans extend the cover
        // contiguously, never find_free-anywhere (dangling spans misfire
        // downstream release math). Placement starts at the CONTENT end (max
        // non-tiny edge end), not the accounting end: slide-granted tile slack
        // must be consumed in place, not extended past (extending past tile
        // slack chases the block end forever). Cover width is monotone
        // (max(old, content+len-base)): fitting inside keeps tile slack for
        // growth; reaching the end extends it. PMA total bumps the delta
        // unless the pinned-block floor covers the span (audit max() rule).
        // Full blocks fail closed (slab-growth error family); the remove path
        // retries once after relocate (it owns a Tombstone bound — relocate
        // needs one and promote stays CsrEdge-clean for the values callers).
        let vertex = self.vertices.get(src);
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        // Cover base (first non-tiny edge_start; tiny anchors are
        // placeholders) and content end (max non-tiny edge end).
        let mut cover_base: Option<u64> = None;
        let mut content_end: Option<u64> = None;
        for bucket in buckets.iter() {
            if bucket.is_tiny_mode() {
                continue;
            }
            if cover_base.is_none() {
                cover_base = Some(bucket.edge_start());
            }
            let end = bucket
                .edge_start()
                .checked_add(u64::from(bucket.stored_slots_raw()))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            content_end = Some(content_end.map_or(end, |prev| prev.max(end)));
        }
        let (span_base, new_stored, total_delta) = match (cover_base, content_end) {
            (Some(base), Some(content)) => {
                let span_end = content
                    .checked_add(u64::from(span_len))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                // Freeness: mates-disjoint (tiny-skipped occupancy).
                let mates_free = self
                    .labeled_leaf_occupied_spans(src, src)
                    .iter()
                    .all(|(s, e)| span_end <= *s || content >= *e);
                // Pinned leaves contain their spans (floor covers); unpinned
                // leaves tail-grow like slab appends.
                let floor_covers = match self.labeled_leaf_physical_range(src) {
                    Some((block_start, block_len)) => {
                        let block_end = block_start
                            .checked_add(block_len)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        content >= block_start && span_end <= block_end
                    }
                    None => false,
                };
                let pinned = self.labeled_leaf_physical_range(src).is_some();
                if !mates_free || (pinned && !floor_covers) {
                    return Err(LaraOperationError::CollectAllocationOverflow.into());
                }
                if span_end > self.edges.header().elem_capacity {
                    self.edges.set_elem_capacity(span_end)?;
                }
                // Monotone cover: keep tile slack when fitting inside, extend
                // exactly when reaching the end.
                let grown = span_end
                    .checked_sub(base)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let grown = u32::try_from(grown)
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                let new_stored = vertex.stored_slots.max(grown);
                let delta = i64::from(new_stored) - i64::from(vertex.stored_slots);
                (content, new_stored, if floor_covers { 0 } else { delta })
            }
            _ => {
                // No cover yet: pin the leaf (block-aligned growth — per-vertex
                // tail would violate the leaf-allocation discipline) and take
                // the first-fit free run. Tiling-safe: nothing pre-exists;
                // publishing edge_start below sets the exact cover. The pin
                // owns PMA total (no bump here).
                let (leaf_start, leaf_len) = self.ensure_labeled_leaf_block_pinned(src)?;
                if let Some(base) = self.find_free_labeled_leaf_edge_base(
                    src,
                    leaf_start,
                    leaf_len,
                    u64::from(span_len),
                ) {
                    (base, vertex.stored_slots.max(span_len), 0)
                } else {
                    // Block full of live tile slack (slide grants whole-block
                    // tiles; all-tiny mates own no resident to share by): steal
                    // the first mate slack that fits (split its tile at content
                    // end). Total-neutral (shrink+grow net ≤ 0, floor covers);
                    // tiling stays exact (mate [M, C) + own [C, C+len)). The
                    // mate's next growth collides with own span and relocates
                    // (correct pressure response). Genuinely full blocks (no
                    // mate slack) fail closed; the dispatcher relocates.
                    let header = self.edges.header();
                    let seg = header.segment_size.max(1);
                    let leaf = Self::leaf_index_for_vid(src, header.segment_size);
                    let start_vid = leaf.saturating_mul(seg);
                    let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
                    let mut stolen: Option<(u64, u32)> = None;
                    for vid_u in start_vid..end_vid {
                        let mate = VertexId::from(vid_u);
                        if mate == src {
                            continue;
                        }
                        let mvertex = self.vertices.get(mate);
                        if mvertex.is_default_edge_labeled() || mvertex.stored_slots == 0 {
                            continue;
                        }
                        let mbuckets = self.read_vertex_label_buckets(&mvertex)?;
                        let mut mbase: Option<u64> = None;
                        let mut mcontent: Option<u64> = None;
                        for mbucket in mbuckets.iter() {
                            if mbucket.is_tiny_mode() {
                                continue;
                            }
                            if mbase.is_none() {
                                mbase = Some(mbucket.edge_start());
                            }
                            let end = mbucket
                                .edge_start()
                                .checked_add(u64::from(mbucket.stored_slots_raw()))
                                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            mcontent = Some(mcontent.map_or(end, |prev| prev.max(end)));
                        }
                        let (Some(mb), Some(mc)) = (mbase, mcontent) else {
                            continue;
                        };
                        let slack = mb
                            .checked_add(u64::from(mvertex.stored_slots))
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?
                            .saturating_sub(mc);
                        if slack < u64::from(span_len) {
                            continue;
                        }
                        let new_mate_stored = u32::try_from(
                            mc.checked_sub(mb)
                                .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                        )
                        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                        self.vertices
                            .set(mate, &mvertex.with_stored_slots(new_mate_stored));
                        stolen = Some((mc, vertex.stored_slots.max(span_len)));
                        break;
                    }
                    let (base, new_stored) =
                        stolen.ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    // Defense in depth: the stolen range must be mates-disjoint
                    // post-shrink (tiling guarantees it; fail closed if drifted).
                    let span_end = base
                        .checked_add(u64::from(span_len))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let still_free = self
                        .labeled_leaf_occupied_spans(src, src)
                        .iter()
                        .all(|(s, e)| span_end <= *s || base >= *e);
                    if !still_free {
                        return Err(LaraOperationError::CollectAllocationOverflow.into());
                    }
                    (base, new_stored, 0)
                }
            }
        };
        // --- Commit: transcribe, publish, extend span, account ---
        // Transcribe LIVE targets only (skip tombstone holes — the slab form
        // is dense; holes do not survive promotion).
        let mut transcribed = 0u32;
        for i in 0..bucket.tiny_used_width() {
            let target = bucket.tiny_target(i);
            if E::read_from(&target.to_le_bytes()).is_deleted_slot() {
                continue;
            }
            let slot = span_base
                .checked_add(u64::from(transcribed))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.edges
                .write_slot_bytes(slot, &target.to_le_bytes())
                .map_err(LabeledOperationError::from)?;
            transcribed = transcribed
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        debug_assert_eq!(
            transcribed, degree,
            "transcribed live count must match bucket degree"
        );
        let fresh =
            LabelBucket::from_parts(bucket.bucket_label_key(), span_base, degree, degree, -1);
        self.buckets.write_label_bucket_slot(bucket_slot, fresh)?;
        let vertex = self.vertices.get(src);
        self.vertices
            .set(src, &vertex.with_stored_slots(new_stored));
        self.edges
            .bump_vertex_segment_counts(src, i64::from(degree), 0)
            .map_err(LabeledOperationError::from)?;
        if total_delta != 0 {
            self.edges
                .bump_vertex_segment_counts(src, 0, total_delta)
                .map_err(LabeledOperationError::from)?;
        }
        // Bucket lookup caches key on (vertex, slot); the slot is stable
        // across promotion, so no invalidation is needed (same reasoning as
        // slab appends, which publish descriptor rewrites in place).
        Ok(())
    }

    /// Converts an eligible vertex row back to default-label bypass storage.
    pub(crate) fn enable_default_edge_bypass(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        if vertex.degree() > 1 {
            return Err(LabeledOperationError::InvalidDefaultBypass);
        }
        if vertex.degree() == 1 {
            let mut bucket = self
                .buckets
                .read_label_bucket_slot(vertex.base_slot_start())
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.overflow_log_head() >= 0 {
                bucket = self.ensure_label_bucket_folded_to_slab(
                    src,
                    0,
                    vertex.base_slot_start(),
                    bucket,
                )?;
            }
            let old_alloc = vertex.stored_slots;
            let updated = vertex
                .with_default_edge_labeled(true)
                .with_bypass_undirected(bucket.bucket_label_key().is_undirected())
                .with_base_slot_start(bucket.edge_start())
                .with_degree(bucket.degree)
                .with_stored_slots(bucket.stored_slots());
            self.clear_vertex_label_buckets_for_segment(src)?;
            self.set_labeled_vertex(src, updated)?;
            self.edges
                .bump_vertex_segment_counts(src, 0, -i64::from(old_alloc))?;
        } else {
            self.set_labeled_vertex(
                src,
                vertex.with_homogeneous_bypass_label(self.default_label),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use crate::VertexId;

    #[test]
    fn push_vertex_grows_pma_segment_tree_before_high_leaf_edge_insert() {
        let graph = test_graph_with_default(BucketLabelKey::from_raw(1));
        for _ in 1..33 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let high = VertexId::from(32);
        graph
            .insert_edge(
                high,
                BucketLabelKey::from_raw(2),
                TestEdge { target: 0 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        assert!(graph.edges().header().segment_count >= 2);
    }

    #[test]
    fn labeled_insert_and_iter_by_label() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 10 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 11 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let walk = BucketLabelKey::from_raw(3);
        graph
            .insert_edge(
                VertexId::from(0),
                walk,
                TestEdge { target: 20 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 11 }, TestEdge { target: 10 }]
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![
                TestEdge { target: 10 },
                TestEdge { target: 11 },
                TestEdge { target: 20 },
            ]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn first_label_bucket_born_tiny_promotes_to_span() {
        // ADR 0096 §4/§3b: new buckets are born tiny (no quota span, no pin).
        // The initial quota span materializes at promotion (K+1 = 5th edge).
        let graph = test_graph();
        let first_label = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                first_label,
                TestEdge { target: 10 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let first = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(vertex.degree(), 1);
        assert_eq!(vertex.stored_slots, 0, "tiny birth consumes no span");
        assert!(first.is_tiny_mode());
        assert_eq!((first.degree(), first.tiny_used_width()), (1, 1));
        assert!(
            graph
                .labeled_leaf_physical_range(VertexId::from(0))
                .is_none()
        );
        // K=4: a completely full inline bucket still owns no leaf span.
        for target in [11u32, 12, 13] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    first_label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let first = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert!(first.is_tiny_mode(), "4 live edges still fit inline at K=4");
        assert_eq!((first.degree(), first.tiny_used_width()), (4, 4));
        assert_eq!(vertex.stored_slots, 0, "a full tiny bucket owns no span");
        assert!(
            graph
                .labeled_leaf_physical_range(VertexId::from(0))
                .is_none()
        );
        // The 5th edge is the one that promotes.
        graph
            .insert_edge(
                VertexId::from(0),
                first_label,
                TestEdge { target: 14 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(0));
        let first = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert!(!first.is_tiny_mode(), "5th edge must promote");
        assert_eq!((first.degree(), first.stored_slots_raw()), (5, 5));
        assert!(vertex.stored_slots >= 5);
        // ADR 0096 §5: promotion reserves a span (edge_start + stored cohere
        // with the vertex cover); pinning is maintenance's job, not the
        // insert path's (tiny birth pins nothing, and promotion need not
        // either — the allocator/tail covers it).
        assert!(
            first
                .edge_start()
                .checked_add(u64::from(first.stored_slots_raw()))
                .is_some()
        );
        assert!(vertex.stored_slots >= first.stored_slots_raw());
    }

    #[test]
    fn labeled_insert_skip_leaf_cascade_does_not_rebalance() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        // ADR 0096 §4/§3b: fresh buckets are born tiny (no leaf accounting), so
        // seed past promotion to exercise the slab accounting path this test
        // pins (accounting without rebalance).
        for target in [10u32, 11, 12, 13, 14] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let before = graph.leaf_segment_counts_for_vid(VertexId::from(0));
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                TestEdge { target: 15 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let after = graph.leaf_segment_counts_for_vid(VertexId::from(0));
        assert_eq!(after.actual, before.actual + 1);
        assert_eq!(after.total, before.total);
    }

    #[test]
    fn insert_beyond_initial_label_edge_span_capacity_relocates_labeled_leaf() {
        use super::super::leaf_pin::labeled_leaf_physical_block_len;
        let graph = test_graph();
        let vid = VertexId::from(0);
        let cap_before = graph.edges().header().elem_capacity;
        let anchor = BucketLabelKey::from_raw(99);
        graph
            .insert_edge(
                vid,
                anchor,
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let cap_after_tiny_anchor = graph.edges().header().elem_capacity;
        // ADR 0096 §4: the anchor insert births a tiny bucket (no pin, no span).
        // Pinning defers to promotion (K+1 = 5th edge of a bucket) and slab growth.
        assert!(graph.labeled_leaf_physical_range(vid).is_none());
        // Growth label must sort after `anchor` so bucket layout stays in pinned-leaf order.
        let road = BucketLabelKey::from_raw(100);
        for target in 0..5u32 {
            graph
                .insert_edge(
                    vid,
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert!(graph.labeled_leaf_physical_range(vid).is_some());
        for target in 5..128u32 {
            graph
                .insert_edge(
                    vid,
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let cap_after = graph.edges().header().elem_capacity;
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        if cap_after > cap_after_tiny_anchor {
            let delta = cap_after.saturating_sub(cap_after_tiny_anchor);
            assert_eq!(
                delta % block_len,
                0,
                "elem_capacity should grow only via block-aligned leaf allocation, not per-vertex tail (delta={delta}, block_len={block_len})"
            );
        }
        assert!(cap_after >= cap_before);
        graph
            .assert_labeled_buckets_within_leaf_physical(vid)
            .unwrap();
        let edges = graph.iter_edges_for_label(vid, road).unwrap();
        assert_eq!(edges.len(), 128);
        assert_eq!(edges[0], TestEdge { target: 127 });
        assert_eq!(edges[127], TestEdge { target: 0 });
    }

    #[test]
    fn labeled_insert_does_not_grow_elem_capacity_for_hub_growth() {
        use super::super::leaf_pin::labeled_leaf_physical_block_len;
        let graph = LabeledLaraGraph::new(
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
            crate::labeled::InitialCapacities::uniform(1 << 20),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                BucketLabelKey::from_raw(10_000),
                TestEdge {
                    target: u32::from(dst),
                },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let cap_after_pin = graph.edges().header().elem_capacity;
        for label_idx in 0..33u16 {
            let label = BucketLabelKey::from_raw(10_000 + label_idx);
            for edge_i in 0..50u32 {
                graph
                    .insert_edge(
                        hub,
                        label,
                        TestEdge {
                            target: u32::from(dst),
                        },
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap_or_else(|e| panic!("label_idx={label_idx} edge_i={edge_i}: {e:?}"));
            }
        }
        let cap_final = graph.edges().header().elem_capacity;
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        if cap_final > cap_after_pin {
            let delta = cap_final.saturating_sub(cap_after_pin);
            assert_eq!(
                delta % block_len,
                0,
                "hub growth must not tail-append; elem_capacity delta must be block-aligned (delta={delta}, block_len={block_len})"
            );
        }
        graph
            .assert_labeled_buckets_within_leaf_physical(hub)
            .unwrap();
    }

    #[test]
    fn labeled_no_vertex_edge_span_rewrite_on_routine_insert() {
        use super::super::compact::{
            reset_rewrite_vertex_edge_span_test_metrics, rewrite_vertex_edge_span_calls,
        };

        reset_rewrite_vertex_edge_span_test_metrics();
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let rewrites_before = rewrite_vertex_edge_span_calls();
        for target in 0..64u32 {
            graph
                .insert_edge(
                    vid,
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert_eq!(
            rewrite_vertex_edge_span_calls().saturating_sub(rewrites_before),
            0
        );
    }

    #[test]
    fn single_label_log_fold_reserves_edge_only_tail_headroom() {
        use super::super::leaf_pin::labeled_leaf_physical_block_len;

        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        let edge_count = labeled_leaf_physical_block_len(graph.edges().header().segment_size)
            .saturating_add(1) as u32;

        for target in 0..edge_count {
            graph
                .insert_edge(
                    vid,
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(vid);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(bucket.degree(), edge_count);
        assert_eq!(
            graph.iter_edges_for_label(vid, road).unwrap().len(),
            edge_count as usize
        );
        assert!(vertex.stored_slots >= graph.edges().header().segment_size);
        assert!(vertex.stored_slots > bucket.stored_slots());
        assert_eq!(bucket.stored_slots(), edge_count);
        assert_eq!(bucket.overflow_log_head(), -1);
        assert_eq!(bucket.inline_property_bytes_slab_slots(), 0);
    }

    #[test]
    fn labeled_bypass_still_uses_core_vertex_path() {
        let default = BucketLabelKey::from_raw(7);
        let graph = test_graph_with_default(default);
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .enable_default_edge_bypass(hub)
            .expect("single-label row can enter bypass mode");
        for target in 10..20u32 {
            graph
                .insert_edge(
                    hub,
                    default,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert_eq!(graph.out_edges(hub).unwrap().len(), 10);
        assert!(graph.vertices().get(hub).is_default_edge_labeled());
    }

    #[test]
    fn unordered_scalar_insert_reuses_interior_slab_tombstone() {
        let graph = test_graph();
        let src = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        let insertion = crate::labeled::graph::EdgePlacementPolicy::Insertion;
        let unordered = crate::labeled::graph::EdgePlacementPolicy::Unordered;
        for target in [10u32, 20, 30, 40, 50] {
            graph
                .insert_edge(src, label, TestEdge { target }, insertion)
                .unwrap();
        }
        // Fold the overflow log into the slab so the bucket is slab-backed
        // (stored_slots == degree), matching a production bucket after
        // maintenance.
        graph.compact_vertex_edge_span(src, 0).unwrap();

        // Delete the middle edge: slot 1 becomes a tombstone while stored_slots stays 5.
        let removed = graph.remove_edge_at_slot(src, label, 1).unwrap().unwrap();
        assert_eq!(removed.target, 20);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(
                graph
                    .find_bucket_slot(&graph.vertices.get(src), label)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        assert!(!bucket.is_tiny_mode(), "5 seeds promote to the slab");
        assert_eq!(bucket.stored_slots_raw(), 5);
        assert_eq!(bucket.degree(), 4);

        // Unordered placement reuses the tombstone before appending.
        let location = graph
            .insert_edge_skip_leaf_cascade_with_location(
                src,
                label,
                TestEdge { target: 21 },
                unordered,
            )
            .unwrap()
            .unwrap();
        assert_eq!(location.logical_slot, 1);
        assert_eq!(
            location.storage,
            crate::labeled::graph::ScalarInsertStorage::Slab
        );

        let bucket = graph
            .buckets()
            .read_label_bucket_slot(
                graph
                    .find_bucket_slot(&graph.vertices.get(src), label)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(bucket.stored_slots(), 5);
        assert_eq!(bucket.degree(), 5);
        assert_eq!(
            graph
                .out_edges(src)
                .unwrap()
                .iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>(),
            vec![10, 21, 30, 40, 50]
        );
    }

    #[test]
    fn unordered_reuse_writes_inline_property_bytes_at_live_ordinal() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let src = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, label, 2)
            .unwrap();
        let insertion = crate::labeled::graph::EdgePlacementPolicy::Insertion;
        let unordered = crate::labeled::graph::EdgePlacementPolicy::Unordered;
        for target in [10u32, 20, 30] {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                    insertion,
                )
                .unwrap();
        }
        graph.compact_vertex_edge_span(src, 0).unwrap();
        // Delete the middle edge: edge slot 1 is tombstoned and the inline
        // property bytes sequence compacts to [10, 30].
        let removed = graph.remove_edge_at_slot(src, label, 1).unwrap().unwrap();
        assert_eq!(removed.target, 20);

        // Unordered reuse writes the new bytes at the reused slot's live ordinal
        // (1) and shifts the trailing bytes (30) up so they keep their value.
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyTestEdge::with_bytes(21, &21u16.to_le_bytes()),
                unordered,
            )
            .unwrap();

        let mut rows = Vec::new();
        let _ = graph
            .visit_edges_with_inline_property::<()>(
                src,
                label,
                OutEdgeOrder::Ascending,
                |slot, item| {
                    rows.push((
                        slot.raw(),
                        item.edge.target,
                        item.inline_property.bytes().to_vec(),
                    ));
                    std::ops::ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (0, 10, 10u16.to_le_bytes().to_vec()),
                (1, 21, 21u16.to_le_bytes().to_vec()),
                (2, 30, 30u16.to_le_bytes().to_vec()),
            ]
        );
    }

    #[test]
    fn unordered_insert_appends_when_bucket_is_dense() {
        let graph = test_graph();
        let src = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        let insertion = crate::labeled::graph::EdgePlacementPolicy::Insertion;
        let unordered = crate::labeled::graph::EdgePlacementPolicy::Unordered;
        graph
            .insert_edge(src, label, TestEdge { target: 10 }, insertion)
            .unwrap();
        graph
            .insert_edge(src, label, TestEdge { target: 20 }, insertion)
            .unwrap();
        // No tombstone: the dense fast path keeps the append order.
        let location = graph
            .insert_edge_skip_leaf_cascade_with_location(
                src,
                label,
                TestEdge { target: 30 },
                unordered,
            )
            .unwrap()
            .unwrap();
        assert_eq!(location.logical_slot, 2);
        assert_eq!(
            graph
                .out_edges(src)
                .unwrap()
                .iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
    }

    #[test]
    fn insertion_placement_never_reuses_interior_tombstone() {
        let graph = test_graph();
        let src = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        let insertion = crate::labeled::graph::EdgePlacementPolicy::Insertion;
        for target in [10u32, 20, 30] {
            graph
                .insert_edge(src, label, TestEdge { target }, insertion)
                .unwrap();
        }
        graph.compact_vertex_edge_span(src, 0).unwrap();
        graph.remove_edge_at_slot(src, label, 1).unwrap().unwrap();

        // Insertion placement never fills the interior tombstone (ADR 0052 §6).
        // Tiny buckets are born tiny (not slab): the live prefix does not
        // promote yet (K=4 holds 4 inline slots, and only 3 are used), so the
        // pending edge appends at the used tail (slot 3) and the interior hole
        // at slot 1 is retained as a layout-native tombstone. The §6 intent
        // holds: no hole was filled (slot != 1).
        let location = graph
            .insert_edge_skip_leaf_cascade_with_location(
                src,
                label,
                TestEdge { target: 40 },
                insertion,
            )
            .unwrap()
            .unwrap();
        assert_ne!(location.logical_slot, 1, "Insertion must not fill the hole");
        assert_eq!(location.logical_slot, 3);
        assert_eq!(
            graph
                .out_edges(src)
                .unwrap()
                .iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>(),
            vec![10, 30, 40]
        );
    }

    #[test]
    fn unordered_reuse_skips_log_backed_inline_property_bytes_bucket() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let src = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, label, 2)
            .unwrap();
        let insertion = crate::labeled::graph::EdgePlacementPolicy::Insertion;
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyTestEdge::with_bytes(1, &1u16.to_le_bytes()),
                insertion,
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyTestEdge::with_bytes(2, &2u16.to_le_bytes()),
                insertion,
            )
            .unwrap();
        graph.compact_vertex_edge_span(src, 0).unwrap();
        // Create a slab tombstone at slot 0, then craft the ADR 0052 §9
        // fallback state: the bucket's inline property bytes are log-backed.
        graph.remove_edge_at_slot(src, label, 0).unwrap().unwrap();
        let bucket_slot = graph
            .find_bucket_slot(&graph.vertices.get(src), label)
            .unwrap()
            .unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        let crafted = bucket.try_with_inline_property_bytes_log(1, 1).unwrap();
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, crafted)
            .unwrap();

        // The reuse helper must refuse without touching state.
        let result = graph
            .try_reuse_unordered_slab_tombstone(
                src,
                bucket_slot,
                crafted,
                &InlinePropertyTestEdge::with_bytes(3, &3u16.to_le_bytes()),
            )
            .unwrap();
        assert!(result.is_none());
        let after = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert_eq!(after, crafted);
        assert_eq!(after.degree(), 1);
        assert_eq!(after.stored_slots(), 2);
    }

    #[test]
    fn unordered_delete_insert_cycles_keep_stored_slots_bounded() {
        let graph = test_graph();
        let src = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        let insertion = crate::labeled::graph::EdgePlacementPolicy::Insertion;
        let unordered = crate::labeled::graph::EdgePlacementPolicy::Unordered;
        for target in 0..8u32 {
            graph
                .insert_edge(src, label, TestEdge { target }, insertion)
                .unwrap();
        }
        graph.compact_vertex_edge_span(src, 0).unwrap();
        // Interleave delete + unordered re-insert: every re-insert fills the
        // tombstone it just created, so stored_slots never grows past 8.
        for i in 0..8u32 {
            graph.remove_edge_at_slot(src, label, i).unwrap().unwrap();
            graph
                .insert_edge(src, label, TestEdge { target: 100 + i }, unordered)
                .unwrap();
            let bucket = graph
                .buckets()
                .read_label_bucket_slot(
                    graph
                        .find_bucket_slot(&graph.vertices.get(src), label)
                        .unwrap()
                        .unwrap(),
                )
                .unwrap();
            assert_eq!(bucket.stored_slots(), 8);
            assert_eq!(bucket.degree(), 8);
        }
    }

    // ADR 0096 §5 (R2b): tiny insert/promote behavior. Buckets are hand-built
    // via `force_tiny_bucket` (birth-tiny flips separately); all assertions
    // hold through the full `insert_edge` path including the cascade check.
    use std::ops::ControlFlow;

    fn tiny_test_bucket(
        graph: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        targets: &[u32],
    ) -> (VertexId, BucketLabelKey) {
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        force_tiny_bucket(graph, vid, label, targets);
        (vid, label)
    }

    fn read_tiny_bucket(
        graph: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        vid: VertexId,
        label: BucketLabelKey,
    ) -> LabelBucket {
        let vertex = graph.vertices().get(vid);
        match graph.find_bucket(vid, &vertex, label).expect("find") {
            BucketSearch::Found { bucket, .. } => bucket,
            BucketSearch::Missing { .. } => panic!("tiny bucket missing"),
        }
    }

    #[test]
    fn tiny_live_slot_count_invariant_catches_degree_drift() {
        // ADR 0096 §3b invariant: the live slots inside the byte-28 used width
        // are exactly `degree` (holes are tombstones). Injected drift — here a
        // degree field claiming a slot that is actually a dead hole — must fail
        // the layout audit rather than pass as a valid tiny row.
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        force_tiny_bucket(&graph, vid, label, &[10, 11, 12]);
        graph.remove_edge_at_slot(vid, label, 1).unwrap().unwrap();
        let slot = graph
            .find_bucket_slot(&graph.vertices().get(vid), label)
            .unwrap()
            .unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!((bucket.degree(), bucket.tiny_used_width()), (2, 3));
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );

        graph
            .buckets()
            .write_label_bucket_slot(slot, bucket.with_degree_field(3))
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::labeled::invariants::assert_labeled_layout_invariants(
                graph.vertices(),
                graph.buckets(),
                graph.edges(),
            );
        }));
        assert!(
            result.is_err(),
            "audit must reject a tiny degree that is not backed by live slots"
        );
    }

    #[test]
    fn tiny_append_grows_inline_without_leaf_accounting() {
        let graph = test_graph();
        let (vid, label) = tiny_test_bucket(&graph, &[]);
        let actual_before = graph.leaf_segment_counts_for_vid(vid).actual;
        for target in [10u32, 11, 12] {
            graph
                .insert_edge(
                    vid,
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let bucket = read_tiny_bucket(&graph, vid, label);
        assert!(bucket.is_tiny_mode());
        assert_eq!((bucket.degree(), bucket.tiny_used_width()), (3, 3));
        assert_eq!(
            graph.leaf_segment_counts_for_vid(vid).actual,
            actual_before,
            "tiny appends must not bump leaf actual"
        );
        assert_eq!(graph.edges().header().num_edges, 3);
        let mut seen = Vec::new();
        graph
            .visit_edges(
                vid,
                label,
                crate::labeled::OutEdgeOrder::Ascending,
                |_, edge| {
                    seen.push(u32::from(edge.neighbor_vid()));
                    ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        assert_eq!(seen, vec![10, 11, 12]);
    }

    #[test]
    fn tiny_fourth_insert_stays_inline_fifth_promotes_to_slab() {
        let graph = test_graph();
        let (vid, label) = tiny_test_bucket(&graph, &[10, 11, 12]);
        let actual_before = graph.leaf_segment_counts_for_vid(vid).actual;
        // K=4: the 4th edge still lands inline (no span, no leaf accounting).
        graph
            .insert_edge(
                vid,
                label,
                TestEdge { target: 13 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let bucket = read_tiny_bucket(&graph, vid, label);
        assert!(bucket.is_tiny_mode(), "4th edge still fits inline at K=4");
        assert_eq!((bucket.degree(), bucket.tiny_used_width()), (4, 4));
        assert_eq!(bucket.overflow_log_head(), -1);
        assert_eq!(
            graph.leaf_segment_counts_for_vid(vid).actual,
            actual_before,
            "a full inline bucket banks nothing in the leaf"
        );
        assert_eq!(graph.edges().header().num_edges, 4);
        // The 5th edge promotes and transcribes all 4 inline edges.
        graph
            .insert_edge(
                vid,
                label,
                TestEdge { target: 14 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let bucket = read_tiny_bucket(&graph, vid, label);
        assert!(!bucket.is_tiny_mode(), "5th edge must promote");
        assert!(!bucket.is_tree_mode());
        assert_eq!((bucket.degree(), bucket.stored_slots_raw()), (5, 5));
        assert_eq!(bucket.overflow_log_head(), -1);
        // Transcribed 4 never counted before (+4), appended 5th bumps (+1).
        assert_eq!(
            graph.leaf_segment_counts_for_vid(vid).actual,
            actual_before + 5
        );
        assert_eq!(graph.edges().header().num_edges, 5);
        let mut seen = Vec::new();
        graph
            .visit_edges(
                vid,
                label,
                crate::labeled::OutEdgeOrder::Ascending,
                |_, edge| {
                    seen.push(u32::from(edge.neighbor_vid()));
                    ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        assert_eq!(seen, vec![10, 11, 12, 13, 14]);
    }
}

#[cfg(test)]
mod g6_zero_read_tests {
    use super::super::test_support::*;
    use super::*;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::{VertexId, traits::CsrEdge};

    /// G6 insert arm: a tiny append performs zero edge-slab / edge-log /
    /// inline-property / span reads or writes. Differential proof: the same op
    /// on a slab control bucket reads the slab; on the tiny bucket only the
    /// descriptor row moves. Counters are graph-wide (all 16 memories share
    /// one pair), so the bound is total stable bytes, not per-store.
    ///
    /// Wrong-implementation probe: routing the tiny append through the slab
    /// span path (e.g. deleting the Tiny dispatcher arm) reads slab bytes and
    /// fails the zero-delta assert.
    #[test]
    fn g6_tiny_insert_reads_no_edge_bytes() {
        let (graph, reads, writes) = counting_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        force_tiny_bucket(&graph, vid, label, &[10, 11]);
        // Quiesce: drain setup reads/writes from the counters.
        reads.set(0);
        writes.set(0);
        graph
            .insert_edge(
                vid,
                label,
                TestEdge { target: 12 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        // One descriptor-row read (find) + one descriptor-row write (publish)
        // plus the vertex row: bounded small. The edge slab, edge log, value
        // slab/log, span meta, and free-span stores move zero bytes — proven
        // differentially below against the slab control (which reads its span).
        let r = reads.get();
        let w = writes.get();
        // Sanity first (pre-promotion): the edge landed inline.
        let vertex = graph.vertices().get(vid);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find") {
            BucketSearch::Found { bucket, .. } => bucket,
            BucketSearch::Missing { .. } => panic!("bucket missing"),
        };
        assert!(bucket.is_tiny_mode());
        assert_eq!(bucket.degree(), 3);
        assert_eq!(TestEdge::BYTES, 4);
        // Absolute bound: descriptor row (29B) + vertex row + lookup reads only.
        // No edge-slab/log/ipb/span bytes can hide in 135R/37W: a single slab
        // span read of the 2-edge prefix alone would cost 8B+ and the control
        // below shows the slab path's floor.
        assert_eq!((r, w), (135, 37), "tiny insert byte shape changed");
        // Differential control: promote to slab (5th edge), then measure a slab
        // append (6th edge) — it must read its slab span, strictly more than tiny.
        graph
            .insert_edge(
                vid,
                label,
                TestEdge { target: 13 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let vertex = graph.vertices().get(vid);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find") {
            BucketSearch::Found { bucket, .. } => bucket,
            BucketSearch::Missing { .. } => panic!("bucket missing"),
        };
        assert!(bucket.is_tiny_mode(), "4th insert still fits inline at K=4");
        graph
            .insert_edge(
                vid,
                label,
                TestEdge { target: 14 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let vertex = graph.vertices().get(vid);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find") {
            BucketSearch::Found { bucket, .. } => bucket,
            BucketSearch::Missing { .. } => panic!("bucket missing"),
        };
        assert!(!bucket.is_tiny_mode(), "5th insert promotes to slab");
        reads.set(0);
        writes.set(0);
        graph
            .insert_edge(
                vid,
                label,
                TestEdge { target: 15 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let sr = reads.get();
        let sw = writes.get();
        assert_eq!((sr, sw), (372, 65), "slab control byte shape changed");
        assert!(
            sr > r && sw > w,
            "slab append must move strictly more bytes than tiny: tiny ({r},{w}) vs slab ({sr},{sw})"
        );
    }
}
