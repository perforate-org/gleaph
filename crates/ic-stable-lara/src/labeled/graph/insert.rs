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
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{BucketSearch, LabeledLaraGraph};

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
        #[cfg(feature = "canbench")]
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
            return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
                bucket_width: 0,
                edge_inline_property_width,
            });
        }

        let (bucket_slot, mut bucket) = self.find_or_create_bucket(src, &vertex, label_id)?;
        let vertex = self.vertices.get(src);
        if edge_inline_property_width != bucket.inline_property_byte_width() {
            bucket = self.ensure_bucket_inline_property_schema_for_insert(
                bucket,
                edge_inline_property_width,
            )?;
            self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
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
                self.bucket_slab_window_end_exclusive_after_bucket(&vertex, bucket_index, &bucket)?
            };
            let slack_span = successor_start.saturating_sub(bucket.edge_start());
            if bucket.overflow_log_head() < 0
                && bucket.stored_slots > 0
                && slack_span > u64::from(bucket.stored_slots)
            {
                let write_slot =
                    checked_add_slot_index(bucket.edge_start(), u64::from(bucket.stored_slots))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                debug_assert!(write_slot < successor_start);
                self.edges.write_slot(write_slot, attempt_edge.clone())?;
                let logical_slot = bucket.stored_slots;
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
                ScalarLocationCapture::Ignore => self.edges.insert_edge_without_logical_slot(
                    &access,
                    VertexId::from(0),
                    attempt_edge.clone(),
                ),
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
                    let new_stored = written_slot.saturating_add(1).max(bucket.stored_slots);
                    if new_stored != bucket.stored_slots {
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
                Ok(InsertLocation::Log { logical_slot, .. }) if !has_edge_inline_property => {
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
                    let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
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

    /// Storage-owned pre-insert capacity preparation for a new label bucket.
    ///
    /// When the next ordinary insert will create a new bucket for `(src, label_id)`,
    /// the bucket needs a free configured per-vertex quota span inside `src`'s pinned
    /// PMA leaf block.  If the leaf is already dense or no free span fits, this helper
    /// rebalances / relocates the leaf *before* any canonical edge write, keeping the
    /// subsequent `find_or_create_bucket` path fail-closed.
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
        self.ensure_labeled_leaf_block_pinned(src)?;
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
        if bucket.stored_slots <= bucket.degree() {
            return Ok(None);
        }
        if bucket.inline_property_bytes_log_head() >= 0 {
            return Ok(None);
        }
        let mut ordinal_before = 0u32;
        let mut reused_slot = None;
        for slot_index in 0..bucket.stored_slots {
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
        #[cfg(feature = "canbench")]
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
        let bucket = self
            .buckets
            .read_label_bucket_slot(slot)
            .ok_or_else(|| {
                log_collect_overflow("try_place_new_bucket_edge_span: cannot read new bucket slot");
                LaraOperationError::CollectAllocationOverflow
            })?
            .with_edge_range(edge_start, 0)
            .with_overflow_log_head(-1);
        self.buckets.write_label_bucket_slot(slot, bucket)?;
        if initial_slots > 0 {
            self.vertices
                .set(src, &vertex.with_stored_slots(initial_slots));
        }
        Ok(true)
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
                .with_stored_slots(bucket.stored_slots);
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
    fn first_label_bucket_reserves_initial_edge_quota() {
        let graph = test_graph();
        let first_label = BucketLabelKey::from_raw(2);
        let quota = super::super::leaf_pin::labeled_leaf_initial_bucket_quota(
            graph.edges().header().segment_size,
        );

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
        assert_eq!(vertex.stored_slots, quota);
        assert_eq!(first.degree(), 1);
        assert_eq!(first.stored_slots, quota.min(1));
        if quota == 0 {
            assert!(first.overflow_log_head() >= 0);
        } else {
            assert_eq!(first.overflow_log_head(), -1);
        }
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
        let before = graph.leaf_segment_counts_for_vid(VertexId::from(0));
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                TestEdge { target: 10 },
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
        let cap_after_pin = graph.edges().header().elem_capacity;
        assert!(graph.labeled_leaf_physical_range(vid).is_some());
        // Growth label must sort after `anchor` so bucket layout stays in pinned-leaf order.
        let road = BucketLabelKey::from_raw(100);
        for target in 0..128u32 {
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
        if cap_after > cap_after_pin {
            let delta = cap_after.saturating_sub(cap_after_pin);
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
        assert!(vertex.stored_slots > bucket.stored_slots);
        assert_eq!(bucket.stored_slots, edge_count);
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
        for target in [10u32, 20, 30] {
            graph
                .insert_edge(src, label, TestEdge { target }, insertion)
                .unwrap();
        }
        // Fold the overflow log into the slab so the bucket is slab-backed
        // (stored_slots == degree), matching a production bucket after
        // maintenance.
        graph.compact_vertex_edge_span(src, 0).unwrap();

        // Delete the middle edge: slot 1 becomes a tombstone while stored_slots stays 3.
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
        assert_eq!(bucket.stored_slots, 3);
        assert_eq!(bucket.degree(), 2);

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
        assert_eq!(bucket.stored_slots, 3);
        assert_eq!(bucket.degree(), 3);
        assert_eq!(
            graph
                .out_edges(src)
                .unwrap()
                .iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>(),
            vec![10, 21, 30]
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

        // Insertion placement appends after the surviving suffix and never fills
        // the interior tombstone (ADR 0052 §6).
        let location = graph
            .insert_edge_skip_leaf_cascade_with_location(
                src,
                label,
                TestEdge { target: 40 },
                insertion,
            )
            .unwrap()
            .unwrap();
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
        assert_eq!(after.stored_slots, 2);
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
            assert_eq!(bucket.stored_slots, 8);
            assert_eq!(bucket.degree(), 8);
        }
    }
}
