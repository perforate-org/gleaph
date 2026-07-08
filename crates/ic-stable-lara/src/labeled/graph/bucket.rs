//! Labeled graph `bucket` implementation.

use crate::{
    SegmentId, VertexId,
    labeled::{
        bucket_label_key::BucketLabelKey,
        record::{LabelBucket, LabeledVertex},
        slot_index::checked_add_slot_index,
    },
    lara::operation_error::LaraOperationError,
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::cmp::Ordering;

use super::error::LabeledOperationError;
use super::{
    BUCKET_LOOKUP_CACHE_ENTRIES, BULK_BUCKET_SEARCH_MIN_DEGREE, BucketLookupCache, BucketSearch,
    LEAF_VERTEX_EDGE_SEGMENT_DENSITY, LabeledLaraGraph,
};

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(crate) fn labeled_leaf_segment_is_dense(&self, vid: VertexId) -> bool {
        self.labeled_leaf_pma_density(vid) >= LEAF_VERTEX_EDGE_SEGMENT_DENSITY
    }

    fn leaf_has_other_labeled_vertices_with_edges(
        &self,
        leaf: u32,
        seg: u32,
        except: VertexId,
    ) -> bool {
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            if vid == except {
                continue;
            }
            let v = self.vertices.get(vid);
            if !v.is_default_edge_labeled() && v.degree() > 0 {
                return true;
            }
        }
        false
    }

    pub(super) fn rebalance_cascade_after_labeled_mutation(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(src, header.segment_size);
        let idx_u32 = leaf
            .checked_add(header.segment_count)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let _idx = u64::from(idx_u32);
        if self.labeled_leaf_pma_density(src) < LEAF_VERTEX_EDGE_SEGMENT_DENSITY {
            return Ok(());
        }

        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_rebalance_leaf_cascade");

        let sole_active_labeled_vertex =
            !self.leaf_has_other_labeled_vertices_with_edges(leaf, seg, src);

        if self.labeled_leaf_physical_range(src).is_some() {
            let counts = self.leaf_segment_counts_for_vid(src);
            if counts.total > 0 && counts.actual >= counts.total {
                self.relocate_labeled_leaf_physical_block(src)?;
            } else {
                self.rebalance_labeled_leaf_weighted_slide(src)?;
                if self.labeled_leaf_pma_density(src) >= LEAF_VERTEX_EDGE_SEGMENT_DENSITY {
                    self.relocate_labeled_leaf_physical_block(src)?;
                }
            }
            return Ok(());
        }

        if self.edges.overflow_log_segment_high_water(leaf) > 0 {
            if sole_active_labeled_vertex {
                self.rebalance_edge_log_vertex_for_labeled(src, true, false)?;
                self.edges
                    .release_log_segment(SegmentId::from(leaf))
                    .map_err(LabeledOperationError::from)?;
            } else {
                self.rebalance_edge_log_leaf_for_labeled(src, true, false)?;
            }
            if self.labeled_leaf_pma_density(src) < LEAF_VERTEX_EDGE_SEGMENT_DENSITY {
                return Ok(());
            }
        }

        let src_vertex = self.vertices.get(src);
        if !src_vertex.is_default_edge_labeled() && src_vertex.degree() > 0 {
            self.rewrite_vertex_edge_span(src, None, 0, false, true, None)?;
            if self.labeled_leaf_pma_density(src) < LEAF_VERTEX_EDGE_SEGMENT_DENSITY {
                return Ok(());
            }
        }

        if sole_active_labeled_vertex {
            return Ok(());
        }

        let start_vid = leaf
            .checked_mul(seg)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let end_vid = start_vid
            .checked_add(seg)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?
            .min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            if vid == src {
                continue;
            }
            let v = self.vertices.get(vid);
            if v.is_default_edge_labeled() {
                continue;
            }
            if v.degree() > 0 {
                self.rewrite_vertex_edge_span(vid, None, 0, true, false, None)?;
            }
        }

        if self.labeled_leaf_pma_density(src) < LEAF_VERTEX_EDGE_SEGMENT_DENSITY {
            return Ok(());
        }

        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            if vid == src {
                continue;
            }
            let v = self.vertices.get(vid);
            if v.is_default_edge_labeled() {
                continue;
            }
            if v.degree() > 0 {
                self.rewrite_vertex_edge_span(vid, None, 0, false, true, None)?;
            }
        }
        Ok(())
    }

    pub(super) fn find_bucket(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        label_id: BucketLabelKey,
    ) -> Result<BucketSearch, LabeledOperationError> {
        if vertex.is_default_edge_labeled() {
            return Ok(BucketSearch::Missing { insert_index: 0 });
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(BucketSearch::Missing { insert_index: 0 });
        }
        let start = vertex.base_slot_start();
        let range_end = start
            .checked_add(u64::from(deg))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;

        if let Some(cache) = self.last_bucket_lookup.get()
            && cache.vid == src
            && cache.base_slot_start == start
            && cache.degree == deg
        {
            if cache.bucket_key == label_id && (start..range_end).contains(&cache.slot) {
                let bucket = self
                    .buckets
                    .read_label_bucket_slot(cache.slot)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                if bucket.bucket_label_key() == label_id {
                    return Ok(BucketSearch::Found {
                        slot: cache.slot,
                        bucket,
                    });
                }
            }
            if let Some(slot_after_cache) = cache.slot.checked_add(1)
                && slot_after_cache == range_end
                && cache.bucket_key < label_id
            {
                let bucket = self
                    .buckets
                    .read_label_bucket_slot(cache.slot)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                if bucket.bucket_label_key() == cache.bucket_key {
                    return Ok(BucketSearch::Missing { insert_index: deg });
                }
            }
            if label_id > cache.bucket_key {
                if let Some(next_slot) = cache.slot.checked_add(1)
                    && next_slot < range_end
                {
                    let bucket = self
                        .buckets
                        .read_label_bucket_slot(next_slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    if bucket.bucket_label_key() == label_id {
                        self.cache_bucket_lookup(src, label_id, vertex, next_slot);
                        return Ok(BucketSearch::Found {
                            slot: next_slot,
                            bucket,
                        });
                    }
                }
            } else if label_id < cache.bucket_key && cache.slot > start {
                let prev_slot = cache
                    .slot
                    .checked_sub(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let bucket = self
                    .buckets
                    .read_label_bucket_slot(prev_slot)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                if bucket.bucket_label_key() == label_id {
                    self.cache_bucket_lookup(src, label_id, vertex, prev_slot);
                    return Ok(BucketSearch::Found {
                        slot: prev_slot,
                        bucket,
                    });
                }
            }
        }
        let cache_index = Self::bucket_lookup_cache_index(src, label_id);
        if let Some(cache) = self.bucket_lookup_cache[cache_index].get()
            && cache.vid == src
            && cache.bucket_key == label_id
            && cache.base_slot_start == start
            && cache.degree == deg
            && (start..range_end).contains(&cache.slot)
        {
            let bucket = self
                .buckets
                .read_label_bucket_slot(cache.slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.bucket_label_key() == label_id {
                self.last_bucket_lookup.set(Some(cache));
                return Ok(BucketSearch::Found {
                    slot: cache.slot,
                    bucket,
                });
            }
        }
        // Fast paths: avoid binary search + canbench scope overhead on tiny degree.
        if deg == 1 {
            let bucket = self
                .buckets
                .read_label_bucket_slot(start)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            return Ok(match label_id.cmp(&bucket.bucket_label_key()) {
                Ordering::Less => BucketSearch::Missing { insert_index: 0 },
                Ordering::Equal => {
                    self.cache_bucket_lookup(src, label_id, vertex, start);
                    BucketSearch::Found {
                        slot: start,
                        bucket,
                    }
                }
                Ordering::Greater => BucketSearch::Missing { insert_index: 1 },
            });
        }
        if deg == 2 {
            let b0 = self
                .buckets
                .read_label_bucket_slot(start)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            match label_id.cmp(&b0.bucket_label_key()) {
                Ordering::Less => return Ok(BucketSearch::Missing { insert_index: 0 }),
                Ordering::Equal => {
                    self.cache_bucket_lookup(src, label_id, vertex, start);
                    return Ok(BucketSearch::Found {
                        slot: start,
                        bucket: b0,
                    });
                }
                Ordering::Greater => {
                    let slot1 = start
                        .checked_add(1)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let b1 = self
                        .buckets
                        .read_label_bucket_slot(slot1)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    return Ok(match label_id.cmp(&b1.bucket_label_key()) {
                        Ordering::Less => BucketSearch::Missing { insert_index: 1 },
                        Ordering::Equal => {
                            self.cache_bucket_lookup(src, label_id, vertex, slot1);
                            BucketSearch::Found {
                                slot: slot1,
                                bucket: b1,
                            }
                        }
                        Ordering::Greater => BucketSearch::Missing { insert_index: 2 },
                    });
                }
            }
        }

        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_find_bucket_slot");

        if deg >= BULK_BUCKET_SEARCH_MIN_DEGREE {
            let buckets = self
                .buckets
                .read_label_bucket_slots_contiguous(start, deg)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            return Ok(
                match buckets.binary_search_by_key(&label_id, |bucket| bucket.bucket_label_key()) {
                    Ok(index) => {
                        let slot = start
                            .checked_add(index as u64)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        let bucket = buckets[index];
                        self.cache_bucket_lookup(src, label_id, vertex, slot);
                        BucketSearch::Found { slot, bucket }
                    }
                    Err(index) => BucketSearch::Missing {
                        insert_index: index as u32,
                    },
                },
            );
        }

        let mut lo = 0u32;
        let mut hi = deg;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let slot = start
                .checked_add(u64::from(mid))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.bucket_label_key() == label_id {
                self.cache_bucket_lookup(src, label_id, vertex, slot);
                return Ok(BucketSearch::Found { slot, bucket });
            }
            if bucket.bucket_label_key() < label_id {
                lo = mid
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            } else {
                hi = mid;
            }
        }
        Ok(BucketSearch::Missing { insert_index: lo })
    }

    pub(crate) fn invalidate_bucket_lookup_caches_for_bucket_segment(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let (start, end) = self.buckets.segment_vertex_bounds(&self.vertices, vid)?;
        if let Some(cache) = self.last_bucket_lookup.get() {
            let v_ord = u32::from(cache.vid);
            if (start..end).contains(&v_ord) {
                self.last_bucket_lookup.set(None);
            }
        }
        for cell in &self.bucket_lookup_cache {
            if let Some(cache) = cell.get() {
                let v_ord = u32::from(cache.vid);
                if (start..end).contains(&v_ord) {
                    cell.set(None);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn clear_vertex_label_buckets_for_segment(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError> {
        self.buckets
            .clear_vertex_label_buckets(&self.vertices, vid)?;
        self.invalidate_bucket_lookup_caches_for_bucket_segment(vid)?;
        Ok(())
    }

    pub(super) fn invalidate_bucket_lookup_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) {
        self.last_bucket_lookup.set(None);
        self.bucket_lookup_cache[Self::bucket_lookup_cache_index(src, label_id)].set(None);
    }

    pub(super) fn cache_bucket_lookup(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        vertex: &LabeledVertex,
        slot: u64,
    ) {
        let cache = BucketLookupCache {
            vid: src,
            bucket_key: label_id,
            base_slot_start: vertex.base_slot_start(),
            degree: vertex.degree(),
            slot,
        };
        self.last_bucket_lookup.set(Some(cache));
        self.bucket_lookup_cache[Self::bucket_lookup_cache_index(src, label_id)].set(Some(cache));
    }

    pub(super) fn bucket_lookup_cache_index(src: VertexId, label_id: BucketLabelKey) -> usize {
        let mixed = u32::from(src)
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add(u32::from(label_id.raw()));
        (mixed as usize) & (BUCKET_LOOKUP_CACHE_ENTRIES - 1)
    }

    pub(super) fn labeled_bucket_descriptor_index(
        vertex: &LabeledVertex,
        bucket_slot: u64,
    ) -> Result<u32, LabeledOperationError> {
        let d = bucket_slot
            .checked_sub(vertex.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        u32::try_from(d).map_err(|_| LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn labeled_vertex_bucket_slot(
        vertex: &LabeledVertex,
        bucket_index: u32,
    ) -> Result<u64, LabeledOperationError> {
        vertex
            .base_slot_start()
            .checked_add(u64::from(bucket_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn find_bucket_slot(
        &self,
        vertex: &LabeledVertex,
        label_id: BucketLabelKey,
    ) -> Result<Option<u64>, LabeledOperationError> {
        Ok(
            match self.find_bucket(VertexId::from(0), vertex, label_id)? {
                BucketSearch::Found { slot, .. } => Some(slot),
                BucketSearch::Missing { .. } => None,
            },
        )
    }

    pub(super) fn read_vertex_label_buckets(
        &self,
        vertex: &LabeledVertex,
    ) -> Result<Vec<LabelBucket>, LabeledOperationError> {
        if vertex.is_default_edge_labeled() {
            return Ok(Vec::new());
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(Vec::new());
        }
        let start = vertex.base_slot_start();
        Ok(self
            .buckets
            .read_label_bucket_slots_contiguous(start, deg)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?)
    }

    pub(super) fn read_vertex_label_buckets_range(
        &self,
        vertex: &LabeledVertex,
        lo: u32,
        hi: u32,
    ) -> Result<Vec<LabelBucket>, LabeledOperationError> {
        if lo >= hi {
            return Ok(Vec::new());
        }
        let deg = vertex.degree();
        if hi > deg {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        let n = hi - lo;
        let start = vertex
            .base_slot_start()
            .checked_add(u64::from(lo))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.buckets
            .read_label_bucket_slots_contiguous(start, n)
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn vertex_label_buckets_have_overflow(
        &self,
        vertex: &LabeledVertex,
    ) -> Result<bool, LabeledOperationError> {
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(false);
        }
        let buckets = self.read_vertex_label_buckets(vertex)?;
        Ok(buckets.iter().any(|b| b.overflow_log_head() >= 0))
    }

    pub(super) fn bucket_successor_start(
        &self,
        vertex: &LabeledVertex,
        bucket_index: u32,
    ) -> Result<u64, LabeledOperationError> {
        let cur_slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
        let cur = self
            .buckets
            .read_label_bucket_slot(cur_slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.bucket_successor_start_after_bucket(vertex, bucket_index, &cur)
    }

    pub(super) fn bucket_successor_start_after_bucket(
        &self,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<u64, LabeledOperationError> {
        if bucket_index + 1 < vertex.degree() {
            let next_ix = bucket_index
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let next_slot = Self::labeled_vertex_bucket_slot(vertex, next_ix)?;
            let next = self
                .buckets
                .read_label_bucket_slot(next_slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            // Proportional slack placement does not guarantee strictly increasing
            // `edge_start` across bucket slots; CSR slab-window geometry requires a
            // non-decreasing neighbor base, so clamp the successor boundary.
            return Ok(next.edge_start().max(bucket.edge_start()));
        }

        if vertex.degree() == 0 {
            return Ok(0);
        }

        let first_edge_start = if bucket_index == 0 {
            bucket.edge_start()
        } else {
            self.buckets
                .read_label_bucket_slot(vertex.base_slot_start())
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .edge_start()
        };
        crate::labeled::slot_index::checked_add_slot_index(
            first_edge_start,
            u64::from(vertex.stored_slots),
        )
        .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn label_buckets_allow_contiguous_slab_copy(
        &self,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Result<bool, LabeledOperationError> {
        if buckets.iter().any(|b| b.overflow_log_head() >= 0) {
            return Ok(false);
        }
        if buckets.iter().any(|b| b.stored_slots != b.degree()) {
            return Ok(false);
        }
        for (index, bucket) in buckets.iter().enumerate() {
            let successor = match index.checked_add(1).and_then(|next_i| buckets.get(next_i)) {
                Some(next) => next.edge_start().max(bucket.edge_start()),
                None => {
                    checked_add_slot_index(buckets[0].edge_start(), u64::from(vertex.stored_slots))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?
                }
            };
            let span_width = successor.saturating_sub(bucket.edge_start());
            if span_width < u64::from(bucket.stored_slots) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn vertex_bucket_descriptor_row_end(
        &self,
        vid: VertexId,
    ) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() {
            return self.vertex_prefix_end(vid);
        }
        let span = vertex
            .label_bucket_descriptor_span()
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        vertex
            .base_slot_start()
            .checked_add(u64::from(span))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn ensure_vertex_bucket_row_origin(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() || vertex.degree() > 0 {
            return Ok(());
        }
        let row_base = if u32::from(src) == 0 {
            0
        } else {
            let pred = VertexId::from(u32::from(src).saturating_sub(1));
            self.vertex_bucket_descriptor_row_end(pred)?
        };
        if vertex.base_slot_start() != row_base {
            self.set_labeled_vertex(src, vertex.with_base_slot_start(row_base))?;
        }
        Ok(())
    }

    pub(super) fn ensure_label_bucket_folded_to_slab(
        &self,
        src: VertexId,
        bucket_index: u32,
        bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Result<LabelBucket, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if bucket.overflow_log_head() < 0 {
            return Ok(bucket);
        }
        let vertex = self.vertices.get(src);
        match self.fold_label_bucket_to_slab(src, &vertex, bucket_index, bucket_slot, bucket) {
            Ok(folded) => {
                self.buckets.write_label_bucket_slot(bucket_slot, folded)?;
                Ok(folded)
            }
            Err(_) => {
                self.rewrite_vertex_edge_span(src, Some(bucket_index), 0, true, false, None)?;
                let vertex = self.vertices.get(src);
                let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                self.buckets
                    .read_label_bucket_slot(slot)
                    .ok_or(LaraOperationError::CollectAllocationOverflow.into())
            }
        }
    }

    pub(super) fn bucket_log_chains(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> (Vec<u32>, Vec<u32>) {
        let leaf = self.payload_log_leaf(src);
        let edge_chain = self
            .edges
            .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
        let payload_chain = self
            .values
            .payload_log_chain_asc_indices(leaf, bucket.inline_value_log_head());
        (edge_chain, payload_chain)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{LEAF_VERTEX_EDGE_SEGMENT_DENSITY, *};
    use crate::VertexId;

    #[test]
    fn visit_out_edges_with_raw_still_applies_matches_on_log_backed_bucket() {
        let graph = inline_value_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=33u32 {
            let weight = u16::try_from(target).expect("weight fits u16");
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &weight.to_le_bytes()),
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);

        let mut visited = Vec::new();
        let mut raw_all = |_bytes: &[u8]| true;
        graph
            .visit_out_edges(
                VertexId::from(0),
                None,
                Some(2),
                Some(&mut raw_all),
                |edge| edge.target < 100 && edge.target % 2 == 0,
                |edge| visited.push(edge),
            )
            .unwrap();

        assert_eq!(
            visited.iter().map(|e| e.target).collect::<Vec<_>>(),
            vec![32, 30]
        );
        for edge in &visited {
            assert_eq!(edge.inline_value_len, 2);
            let b = edge.edge_inline_value_bytes();
            assert_eq!(
                u16::from_le_bytes([b[0], b[1]]),
                u16::try_from(edge.target).unwrap()
            );
        }
    }

    #[test]
    fn out_edge_bucket_index_range_agrees_with_slice_partition() {
        use crate::labeled::BucketDirectedness;

        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::undirected_from_index(3),
                TestEdge { target: 30 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(2),
                TestEdge { target: 10 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(4),
                TestEdge { target: 40 },
            )
            .unwrap();

        let v = graph.vertices().get(VertexId::from(0));
        let base = v.base_slot_start();
        let deg = v.degree();
        let full = graph
            .buckets()
            .read_label_bucket_slots_contiguous(base, deg)
            .unwrap();
        let p = full.partition_point(|b| b.bucket_label_key().is_undirected());

        assert_eq!(
            graph
                .out_edge_bucket_index_range_for_directedness(
                    VertexId::from(0),
                    BucketDirectedness::Undirected,
                    OutEdgeOrder::Ascending,
                )
                .unwrap(),
            (0, p as u32)
        );
        assert_eq!(
            graph
                .out_edge_bucket_index_range_for_directedness(
                    VertexId::from(0),
                    BucketDirectedness::Directed,
                    OutEdgeOrder::Descending,
                )
                .unwrap(),
            (p as u32, deg)
        );

        assert_eq!(
            graph
                .buckets()
                .partition_first_directed_linear_from_start(base, deg)
                .unwrap(),
            p as u32
        );
        assert_eq!(
            graph
                .buckets()
                .partition_first_directed_linear_from_end(base, deg)
                .unwrap(),
            p as u32
        );
        assert_eq!(
            graph
                .buckets()
                .partition_first_directed_hybrid(base, deg)
                .unwrap(),
            p as u32
        );
    }

    #[test]
    fn label_buckets_and_edges_follow_label_order() {
        let graph = test_graph();
        for (label, target) in [(10, 100), (2, 20), (7, 70), (2, 21)] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(label),
                    TestEdge { target },
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let labels: Vec<_> = (0..vertex.degree())
            .map(|offset| {
                graph
                    .buckets()
                    .read_label_bucket_slot(vertex.base_slot_start() + u64::from(offset))
                    .unwrap()
                    .bucket_label_key()
                    .raw()
            })
            .collect();
        assert_eq!(labels, vec![2, 7, 10]);
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![
                TestEdge { target: 100 },
                TestEdge { target: 70 },
                TestEdge { target: 21 },
                TestEdge { target: 20 },
            ]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn bucket_tail_missing_cache_revalidates_cached_slot_label() {
        let graph = test_graph();
        let low = BucketLabelKey::from_raw(10);
        let old_tail = BucketLabelKey::from_raw(20);
        let inserted = BucketLabelKey::from_raw(30);
        let new_tail = BucketLabelKey::from_raw(40);

        graph
            .insert_edge(VertexId::from(0), low, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), old_tail, TestEdge { target: 20 })
            .unwrap();

        // Populate `last_bucket_lookup` with the old tail label.
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), old_tail)
                .unwrap(),
            vec![TestEdge { target: 20 }]
        );

        let vertex = graph.vertices().get(VertexId::from(0));
        let tail_slot = graph.find_bucket_slot(&vertex, old_tail).unwrap().unwrap();
        let tail_bucket = graph.buckets().read_label_bucket_slot(tail_slot).unwrap();
        graph
            .buckets()
            .write_label_bucket_slot(tail_slot, tail_bucket.with_bucket_label_key(new_tail))
            .unwrap();

        graph
            .insert_edge(VertexId::from(0), inserted, TestEdge { target: 30 })
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let buckets = graph.read_vertex_label_buckets(&vertex).unwrap();
        let labels: Vec<_> = buckets
            .iter()
            .map(|bucket| bucket.bucket_label_key())
            .collect();
        assert_eq!(labels, vec![low, inserted, new_tail]);
    }

    #[test]
    fn empty_middle_label_bucket_does_not_expose_neighbor_edges() {
        let graph = test_graph();
        let low = BucketLabelKey::from_raw(2);
        let middle = BucketLabelKey::from_raw(3);
        let high = BucketLabelKey::from_raw(4);

        graph
            .insert_edge(VertexId::from(0), low, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), middle, TestEdge { target: 20 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), high, TestEdge { target: 30 })
            .unwrap();

        assert!(
            graph
                .remove_edge_matching(VertexId::from(0), middle, |edge| edge.target == 20)
                .unwrap()
                .is_some()
        );

        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), middle)
                .unwrap(),
            Vec::<TestEdge>::new()
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge { target: 30 }, TestEdge { target: 10 }]
        );

        let mut raw_scanned = Vec::new();
        graph
            .visit_out_edges(
                VertexId::from(0),
                None,
                None,
                None,
                |_| true,
                |edge| raw_scanned.push(edge),
            )
            .unwrap();
        assert_eq!(
            raw_scanned,
            vec![TestEdge { target: 30 }, TestEdge { target: 10 }]
        );

        let vertex = graph.vertices().get(VertexId::from(0));
        let middle_slot = graph.find_bucket_slot(&vertex, middle).unwrap().unwrap();
        let middle_bucket = graph.buckets().read_label_bucket_slot(middle_slot).unwrap();
        let middle_index = middle_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let successor = graph.bucket_successor_start(&vertex, middle_index).unwrap();
        assert_eq!(middle_bucket.stored_slots, 0);
        assert!(successor >= middle_bucket.edge_start());

        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn insert_beyond_initial_label_bucket_store_vertex_segment_relocates_buckets() {
        let graph = test_graph();
        for label in 1..=33u16 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(label),
                    TestEdge {
                        target: label as u32,
                    },
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.degree(), 33);
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap().len(), 33);
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn labeled_leaf_segment_is_dense_uses_pma_not_geometry_when_pinned() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        graph
            .insert_edge(vid, BucketLabelKey::from_raw(99), TestEdge { target: 999 })
            .unwrap();
        assert!(graph.labeled_leaf_physical_range(vid).is_some());
        let pma = graph.labeled_leaf_pma_density(vid);
        let geometry = graph.labeled_leaf_geometry_density(vid);
        assert!(
            geometry > pma,
            "geometry density inflates before PMA leaf fills"
        );
        assert_eq!(
            graph.labeled_leaf_segment_is_dense(vid),
            pma >= LEAF_VERTEX_EDGE_SEGMENT_DENSITY
        );
        assert!(!graph.labeled_leaf_segment_is_dense(vid));
    }

    #[test]
    fn labeled_leaf_rebalance_preserves_scan() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let anchor = BucketLabelKey::from_raw(99);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(vid, anchor, TestEdge { target: 999 })
            .unwrap();
        for target in 0..64u32 {
            graph.insert_edge(vid, road, TestEdge { target }).unwrap();
        }
        let materialized = |label: BucketLabelKey| {
            graph
                .iter_edges_for_label(vid, label)
                .unwrap()
                .into_iter()
                .map(|e| e.target)
                .collect::<Vec<_>>()
        };
        let before_anchor = materialized(anchor);
        let before_road = materialized(road);
        graph.rebalance_cascade_after_labeled_mutation(vid).unwrap();
        assert_eq!(materialized(anchor), before_anchor);
        assert_eq!(materialized(road), before_road);
    }

    #[test]
    fn labeled_leaf_density_triggers_rebalance_not_vertex_span_alone() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        graph
            .insert_edge(vid, BucketLabelKey::from_raw(99), TestEdge { target: 999 })
            .unwrap();
        let road = BucketLabelKey::from_raw(2);
        let pma_before = graph.labeled_leaf_pma_density(vid);
        graph
            .insert_edge_skip_leaf_cascade(vid, road, TestEdge { target: 1 })
            .unwrap();
        let pma_after = graph.labeled_leaf_pma_density(vid);
        assert!(
            pma_after > pma_before,
            "PMA actual/total should rise with live edges"
        );
        assert!(
            graph.labeled_leaf_geometry_density(vid) > pma_after,
            "maintenance must not use geometry density while leaf block is mostly empty"
        );
    }
}
