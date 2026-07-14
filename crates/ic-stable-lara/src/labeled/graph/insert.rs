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

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Appends a vertex row and grows segment metadata when a new leaf is needed.
    pub fn push_vertex(
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

    /// Compacts the label-bucket descriptor segment containing `vid`.
    pub fn compact_label_bucket_vertex_segment(
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
    pub fn insert_edge(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.insert_edge_skip_leaf_cascade(src, label_id, edge)?;
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
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let mut vertex = self.vertices.get(src);
        let edge_inline_value_width = edge.edge_inline_value_byte_width();
        let has_edge_inline_value = edge_inline_value_width != 0;
        if vertex.is_default_edge_labeled() {
            if !has_edge_inline_value && label_id == self.bypass_storage_label_for(&vertex) {
                return self.insert_homogeneous_bypass_edge(src, label_id, edge);
            }
            if has_edge_inline_value {
                return Err(LabeledOperationError::PayloadByteWidthMismatch {
                    bucket_width: 0,
                    edge_inline_value_width,
                });
            }
            self.promote_bypass_to_bucket_mode(src)?;
            vertex = self.vertices.get(src);
        } else if vertex.degree() == 0
            && self.is_homogeneous_bypass_label(label_id)
            && self.may_use_homogeneous_bypass(src)
            && !has_edge_inline_value
        {
            return self.insert_homogeneous_bypass(src, label_id, edge);
        }

        if edge_inline_value_width != 0
            && let BucketSearch::Missing { .. } = self.find_bucket(src, &vertex, label_id)?
        {
            return Err(LabeledOperationError::PayloadByteWidthMismatch {
                bucket_width: 0,
                edge_inline_value_width,
            });
        }

        let (bucket_slot, mut bucket) = self.find_or_create_bucket(src, &vertex, label_id)?;
        let vertex = self.vertices.get(src);
        if edge_inline_value_width != bucket.inline_value_byte_width() {
            bucket =
                self.ensure_bucket_payload_schema_for_insert(bucket, edge_inline_value_width)?;
            self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
        }
        self.ensure_bucket_slack_insert_when_peers_have_values(src, &vertex)?;
        let vertex = self.vertices.get(src);
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, bucket_slot)?;
        for _attempt in 0..64u32 {
            let attempt_edge = edge.clone();
            let vertex = self.vertices.get(src);
            if has_edge_inline_value
                && bucket.inline_value_log_len() > 0
                && self
                    .values
                    .payload_log_segment_is_full(self.payload_log_leaf(src))
            {
                self.rebalance_payload_log_leaf_for_labeled(src)?;
                let vertex = self.vertices.get(src);
                let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                bucket = self
                    .buckets
                    .read_label_bucket_slot(bucket_slot)
                    .ok_or_else(|| {
                        log_collect_overflow(
                            "insert_edge_skip_leaf_cascade: cannot re-read bucket after payload log rebalance",
                        );
                        LaraOperationError::CollectAllocationOverflow
                    })?;
                continue;
            }
            let successor_start = if vertex.degree() == 1 && !has_edge_inline_value {
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
                let bucket = bucket.grow_packed_slab_by_one();
                let bucket = self.write_edge_inline_value_after_insert(
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
                return Ok(());
            }
            let access = LabelEdgeSpanAccess::with_bucket(
                &self.buckets,
                bucket_slot,
                bucket,
                successor_start,
                src,
            );
            match self
                .edges
                .insert_edge(&access, VertexId::from(0), attempt_edge.clone())
            {
                Ok(InsertLocation::Slab(_)) if !has_edge_inline_value => return Ok(()),
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
                    let bucket = self.write_edge_inline_value_after_insert(
                        src,
                        bucket_slot,
                        bucket,
                        &attempt_edge,
                    )?;
                    self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                    return Ok(());
                }
                Ok(InsertLocation::Log) if !has_edge_inline_value => return Ok(()),
                Ok(InsertLocation::Log) => {
                    bucket = self
                        .buckets
                        .read_label_bucket_slot(bucket_slot)
                        .ok_or_else(|| {
                            log_collect_overflow(
                                "insert_edge_skip_leaf_cascade: cannot re-read bucket after log insert",
                            );
                            LaraOperationError::CollectAllocationOverflow
                        })?;
                    let bucket = self.write_edge_inline_value_after_insert(
                        src,
                        bucket_slot,
                        bucket,
                        &attempt_edge,
                    )?;
                    self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                    return Ok(());
                }
                Err(LaraOperationError::SegmentLogFull) => {
                    let vertex = self.vertices.get(src);
                    if vertex.is_default_edge_labeled()
                        && !has_edge_inline_value
                        && label_id == self.bypass_storage_label_for(&vertex)
                    {
                        return self.insert_homogeneous_bypass_edge(src, label_id, attempt_edge);
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
    /// the bucket needs a free `DEFAULT_SEGMENT_SIZE` span inside `src`'s pinned PMA
    /// leaf block.  If the leaf is already dense or no free span fits, this helper
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

        // New-bucket contract (ADR 0001): the bucket is created with stored_slots=0
        // placed at the successor boundary. No edge-slab bytes are reserved for the
        // bucket itself; the first edges go to the shared leaf overflow log. We only
        // need to guarantee that the target PMA leaf block is pinned, so a zero-length
        // placement is valid and subsequent edge inserts can use the leaf log.
        self.ensure_labeled_leaf_block_pinned(src)?;
        Ok(())
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

        // Pin even the first bucket's leaf before publishing its zero-length position.
        // The pin owns physical capacity and PMA total counts; the bucket itself still
        // reserves no edge slots.
        self.ensure_labeled_leaf_block_pinned(src)?;

        // A new bucket is always inserted with `stored_slots = 0`. Its edge_start is
        // placed at the successor bucket's edge_start (or the vertex span end for the
        // last bucket), giving it a zero-length reservation that is contiguous with its
        // neighbors. Subsequent edge inserts go through the shared leaf overflow log.
        let edge_start = if bucket_index == 0 {
            // A zero-length first bucket still needs a valid physical anchor inside
            // its pinned leaf. A leaf mate may already own more than its fixed quota;
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
        // Do not update vertex.stored_slots here; the new bucket owns zero slots.
        Ok(true)
    }

    /// Converts an eligible vertex row back to default-label bypass storage.
    pub fn enable_default_edge_bypass(&self, src: VertexId) -> Result<(), LabeledOperationError>
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
            .insert_edge(high, BucketLabelKey::from_raw(2), TestEdge { target: 0 })
            .unwrap();
        assert!(graph.edges().header().segment_count >= 2);
    }

    #[test]
    fn labeled_insert_and_iter_by_label() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();
        let walk = BucketLabelKey::from_raw(3);
        graph
            .insert_edge(VertexId::from(0), walk, TestEdge { target: 20 })
            .unwrap();

        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 11 }, TestEdge { target: 10 }]
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![
                TestEdge { target: 20 },
                TestEdge { target: 11 },
                TestEdge { target: 10 },
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
    fn labeled_insert_skip_leaf_cascade_does_not_rebalance() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let before = graph.leaf_segment_counts_for_vid(VertexId::from(0));
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, TestEdge { target: 10 })
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
            .insert_edge(vid, anchor, TestEdge { target: 999 })
            .unwrap();
        let cap_after_pin = graph.edges().header().elem_capacity;
        assert!(graph.labeled_leaf_physical_range(vid).is_some());
        // Growth label must sort after `anchor` so bucket layout stays in pinned-leaf order.
        let road = BucketLabelKey::from_raw(100);
        for target in 0..128u32 {
            graph.insert_edge(vid, road, TestEdge { target }).unwrap();
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
            1 << 20,
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
            )
            .unwrap();
        let rewrites_before = rewrite_vertex_edge_span_calls();
        for target in 0..64u32 {
            graph.insert_edge(vid, road, TestEdge { target }).unwrap();
        }
        assert_eq!(
            rewrite_vertex_edge_span_calls().saturating_sub(rewrites_before),
            0
        );
    }

    #[test]
    fn single_label_log_fold_reserves_edge_only_tail_headroom() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);

        for target in 0..171u32 {
            graph.insert_edge(vid, road, TestEdge { target }).unwrap();
        }

        let vertex = graph.vertices().get(vid);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(bucket.degree(), 171);
        assert_eq!(graph.iter_edges_for_label(vid, road).unwrap().len(), 171);
        assert!(vertex.stored_slots >= DEFAULT_SEGMENT_SIZE);
        assert!(vertex.stored_slots > bucket.stored_slots);
        assert_eq!(bucket.stored_slots, 171);
        assert_eq!(bucket.overflow_log_head(), -1);
        assert_eq!(bucket.inline_value_slab_slots(), 0);
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
                .insert_edge(hub, default, TestEdge { target })
                .unwrap();
        }
        assert_eq!(graph.asc_out_edges(hub).unwrap().len(), 10);
        assert!(graph.vertices().get(hub).is_default_edge_labeled());
    }
}
