//! Labeled graph `insert` implementation.

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
use super::{BucketSearch, DEFAULT_SEGMENT_SIZE, LabeledLaraGraph};

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub fn push_vertex(
        &self,
        mut vertex: LabeledVertex,
    ) -> Result<VertexId, LabeledOperationError> {
        vertex.ensure_valid_normal_row()?;
        let id = self.vertices.len();
        if id > 0 {
            let prev_end = self
                .vertex_bucket_descriptor_row_end(VertexId::from(id as u32 - 1))
                .map_err(LabeledOperationError::from)?;
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
        let has_edge_value = edge.edge_value_byte_width() != 0;
        if vertex.is_default_edge_labeled() {
            if !has_edge_value && label_id == self.bypass_storage_label_for(&vertex) {
                return self.insert_homogeneous_bypass_edge(src, label_id, edge);
            }
            self.promote_bypass_to_bucket_mode(src)?;
            vertex = self.vertices.get(src);
        } else if vertex.degree() == 0
            && self.is_homogeneous_bypass_label(label_id)
            && self.may_use_homogeneous_bypass(src)
            && !has_edge_value
        {
            return self.insert_homogeneous_bypass(src, label_id, edge);
        }

        let (bucket_slot, mut bucket) = self.find_or_create_bucket(src, &vertex, label_id)?;
        let vertex = self.vertices.get(src);
        let value_width = edge.edge_value_byte_width();
        if value_width != 0 && value_width != bucket.value_byte_width() {
            bucket =
                self.ensure_bucket_value_byte_width_on_slot(src, bucket_slot, bucket, value_width)?;
            self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
        }
        self.ensure_bucket_slack_insert_when_peers_have_values(src, &vertex)?;
        let vertex = self.vertices.get(src);
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, bucket_slot)?;
        for _attempt in 0..64u32 {
            let attempt_edge = edge.clone();
            let vertex = self.vertices.get(src);
            let successor_start =
                self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?;
            let slack_span = successor_start.saturating_sub(bucket.edge_start());
            if bucket.overflow_log_head() < 0 && slack_span > u64::from(bucket.stored_slots) {
                let write_slot =
                    checked_add_slot_index(bucket.edge_start(), u64::from(bucket.stored_slots))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                debug_assert!(write_slot < successor_start);
                self.edges.write_slot(write_slot, attempt_edge.clone())?;
                let prev_stored_slots = bucket.stored_slots;
                let bucket = bucket.grow_packed_slab_by_one();
                let bucket =
                    self.ensure_bucket_value_span(src, bucket_slot, bucket, prev_stored_slots)?;
                let slot_index = bucket.stored_slots.saturating_sub(1);
                self.write_edge_value_at_slot(&bucket, slot_index, &attempt_edge)?;
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
                self.invalidate_bucket_lookup_for_label(src, label_id);
                return Ok(());
            }
            let access = LabelEdgeSpanAccess::new(&self.buckets, bucket_slot, successor_start, src);
            match self
                .edges
                .insert_edge(&access, VertexId::from(0), attempt_edge.clone())
            {
                Ok(InsertLocation::Slab(written_slot)) => {
                    bucket = self
                        .buckets
                        .read_label_bucket_slot(bucket_slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let prev_stored_slots = bucket.stored_slots;
                    let new_stored = written_slot.saturating_add(1).max(bucket.stored_slots);
                    if new_stored != bucket.stored_slots {
                        bucket = bucket.with_stored_slots(new_stored);
                    }
                    let bucket =
                        self.ensure_bucket_value_span(src, bucket_slot, bucket, prev_stored_slots)?;
                    self.write_edge_value_at_slot(&bucket, written_slot, &attempt_edge)?;
                    self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                    self.invalidate_bucket_lookup_for_label(src, label_id);
                    return Ok(());
                }
                Ok(InsertLocation::Log) => {
                    bucket = self
                        .buckets
                        .read_label_bucket_slot(bucket_slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let prev_stored_slots = bucket.stored_slots;
                    let mut bucket =
                        self.ensure_bucket_value_span(src, bucket_slot, bucket, prev_stored_slots)?;
                    let entry_idx = bucket.overflow_log_head();
                    if bucket.is_value_allocated() && entry_idx >= 0 {
                        bucket =
                            self.write_edge_value_to_log(src, &bucket, entry_idx, &attempt_edge)?;
                    }
                    self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                    self.invalidate_bucket_lookup_for_label(src, label_id);
                    return Ok(());
                }
                Err(LaraOperationError::SegmentLogFull) => {
                    let vertex = self.vertices.get(src);
                    if vertex.is_default_edge_labeled()
                        && !has_edge_value
                        && label_id == self.bypass_storage_label_for(&vertex)
                    {
                        return self.insert_homogeneous_bypass_edge(src, label_id, attempt_edge);
                    }
                    self.reclaim_edge_log_leaf_for_labeled(src)?;
                    let vertex = self.vertices.get(src);
                    let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                    bucket = self
                        .buckets
                        .read_label_bucket_slot(bucket_slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                }
                Err(e) => return Err(LabeledOperationError::from(e)),
            }
        }
        Err(LabeledOperationError::from(
            LaraOperationError::SegmentLogFull,
        ))
    }
    pub(super) fn find_or_create_bucket(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        label_id: BucketLabelKey,
    ) -> Result<(u64, LabelBucket), LabeledOperationError> {
        let insert_index = match self.find_bucket(src, vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => return Ok((slot, bucket)),
            BucketSearch::Missing { insert_index } => insert_index,
        };
        if insert_index > 0 && self.vertex_label_buckets_have_overflow(vertex)? {
            self.reclaim_vertex_overflow_buckets(src)?;
        }
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
        if !self.try_place_new_bucket_edge_span(src, &vertex, slot, bucket_index)? {
            let vertex = self.vertices.get(src);
            if self.vertex_label_buckets_have_overflow(&vertex)? {
                self.reclaim_vertex_overflow_buckets(src)?;
            }
            let vertex = self.vertices.get(src);
            if !self.try_place_new_bucket_edge_span(src, &vertex, slot, bucket_index)? {
                self.rewrite_vertex_edge_span(src, Some(bucket_index), 1, false, false)?;
            }
        }
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
    ) -> Result<bool, LabeledOperationError> {
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(false);
        }
        if vertex.degree() == 1 {
            let new_alloc = DEFAULT_SEGMENT_SIZE;
            let edge_start = self.edges.allocate_span(u64::from(new_alloc))?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .with_edge_range(edge_start, 0)
                .with_overflow_log_head(-1);
            self.buckets.write_label_bucket_slot(slot, bucket)?;
            self.vertices.set(src, &vertex.with_stored_slots(new_alloc));
            self.edges
                .bump_vertex_segment_counts(src, 0, i64::from(new_alloc))
                .map_err(LabeledOperationError::from)?;
            return Ok(true);
        }

        if bucket_index + 1 != vertex.degree() {
            return Ok(false);
        }
        let prev_slot = slot
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let prev = self
            .buckets
            .read_label_bucket_slot(prev_slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if prev.overflow_log_head() >= 0 {
            return Ok(false);
        }
        if prev.stored_slots > DEFAULT_SEGMENT_SIZE {
            return Ok(false);
        }
        let first = self
            .buckets
            .read_label_bucket_slot(vertex.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let span_end = checked_add_slot_index(first.edge_start(), u64::from(vertex.stored_slots))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let edge_start = checked_add_slot_index(prev.edge_start(), u64::from(prev.stored_slots))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let gap = span_end.saturating_sub(edge_start);
        if gap == 0 {
            return Ok(false);
        }
        let bucket = self
            .buckets
            .read_label_bucket_slot(slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?
            .with_edge_range(edge_start, 0)
            .with_overflow_log_head(-1);
        self.buckets.write_label_bucket_slot(slot, bucket)?;
        Ok(true)
    }
    pub fn enable_default_edge_bypass(&self, src: VertexId) -> Result<(), LabeledOperationError> {
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
    fn insert_beyond_initial_label_edge_span_capacity_relocates_vertex_edge_span() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let road = BucketLabelKey::from_raw(2);
        for target in 0..128u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        let edges = graph.iter_edges_for_label(VertexId::from(0), road).unwrap();
        assert_eq!(edges.len(), 128);
        assert_eq!(edges[0], TestEdge { target: 127 });
        assert_eq!(edges[127], TestEdge { target: 0 });
        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(bucket.stored_slots, 128);
        assert!(vertex.stored_slots >= 128);
    }
}
