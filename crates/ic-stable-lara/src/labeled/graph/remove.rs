//! Labeled graph `remove` implementation.

use crate::{
    VertexId,
    labeled::{
        access::LabelEdgeSpanAccess,
        bucket_label_key::{BucketDirectedness, BucketLabelKey},
        record::LabeledVertex,
        slot_index::checked_add_slot_index,
    },
    lara::{edge::OutEdgeSlabIter, operation_error::LaraOperationError},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::cell::Cell;

use super::error::LabeledOperationError;
use super::{BucketSearch, LabeledLaraGraph, OutEdgeOrder};

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub fn remove_edge_at_slot(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(None);
            }
            return self
                .edges
                .remove_edge_at_slab_slot(&self.vertices, src, slot_index)
                .map_err(Into::into);
        }
        let BucketSearch::Found { slot, mut bucket } = self.find_bucket(src, &vertex, label_id)?
        else {
            return Ok(None);
        };
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        if bucket.overflow_log_head() >= 0 {
            bucket = self.ensure_label_bucket_folded_to_slab(src, bucket_index, slot, bucket)?;
        }
        if slot_index >= bucket.stored_slots {
            return Ok(None);
        }
        let rm_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let removed = self.edges.read_slot(rm_slot);
        if removed.is_tombstone_edge() {
            return Ok(None);
        }
        self.edges
            .write_slot(rm_slot, E::tombstone_edge())
            .map_err(LabeledOperationError::from)?;
        let updated = bucket
            .after_slab_tombstone_delete()
            .with_overflow_log_head(-1);
        let updated = if updated.degree() == 0 {
            self.release_bucket_value_span(src, &bucket)?;
            updated
                .with_value_log_head(-1)
                .with_edge_range(updated.edge_start(), 0)
        } else {
            updated
        };
        self.buckets.write_label_bucket_slot(slot, updated)?;
        let hdr = self.edges.header();
        let next_global = hdr
            .num_edges
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.edges.set_num_edges(next_global);
        self.edges
            .bump_vertex_segment_counts(src, -1, 0)
            .map_err(LabeledOperationError::from)?;
        Ok(Some(removed))
    }
    pub(super) fn for_each_out_edges_by_directedness_impl<Visit>(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        directedness: BucketDirectedness,
        ascending: bool,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        if vertex.is_default_edge_labeled() {
            if self.bypass_storage_label_for(vertex).directedness() != directedness {
                return Ok(());
            }
            match ascending {
                false => {
                    let slab_iter = OutEdgeSlabIter::try_new(
                        &self.edges,
                        vertex.base_slot_start(),
                        vertex.stored_degree(),
                        vertex.degree(),
                    )?;
                    let label = self.bypass_storage_label_for(vertex).raw();
                    for edge in slab_iter {
                        visit(edge.with_label_id(label));
                    }
                }
                true => {
                    let label = self.bypass_storage_label_for(vertex).raw();
                    for edge in self.edges.asc_out_edges(&self.vertices, src)? {
                        visit(edge.with_label_id(label));
                    }
                }
            }
            return Ok(());
        }
        let deg = vertex.degree();
        let strategy = Self::directedness_partition_strategy(directedness, ascending);
        let (lo, hi) = self.buckets.directedness_bucket_index_range(
            vertex.base_slot_start(),
            deg,
            directedness,
            strategy,
        )?;
        if lo >= hi {
            return Ok(());
        }
        let first_global = self
            .buckets
            .read_label_bucket_slot(vertex.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let span_end_exclusive = Self::vertex_label_edge_span_end_exclusive(vertex, &first_global)?;
        let buckets = self.read_vertex_label_buckets_range(vertex, lo, hi)?;
        if let Some((base, total_edges)) =
            Self::try_contiguous_tiled_labeled_out_edges_slice(&buckets, span_end_exclusive)
        {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_out_edges_by_directedness_tiled");
            if total_edges > 0 {
                let nbytes = (total_edges as usize)
                    .checked_mul(E::BYTES)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let mut raw = vec![0u8; nbytes];
                self.edges.read_slots_contiguous(base, &mut raw);
                match ascending {
                    false => {
                        let mut bucket_rev_idx = buckets.len() as isize - 1;
                        let mut slot_rev: Option<u32> = None;
                        while bucket_rev_idx >= 0 {
                            let bidx = bucket_rev_idx as usize;
                            let bucket = &buckets[bidx];
                            if bucket.degree() == 0 {
                                bucket_rev_idx -= 1;
                                slot_rev = None;
                                continue;
                            }
                            let bucket_index = lo + bidx as u32;
                            let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                            let slot = slot_rev.unwrap_or(bucket.degree().saturating_sub(1));
                            let rel = bucket
                                .edge_start()
                                .saturating_sub(base)
                                .checked_add(u64::from(slot))
                                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            let byte_off = usize::try_from(rel)
                                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
                                .checked_mul(E::BYTES)
                                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            let byte_end = byte_off
                                .checked_add(E::BYTES)
                                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            if byte_end > raw.len() {
                                return Err(LaraOperationError::CollectAllocationOverflow.into());
                            }
                            visit(
                                self.labeled_edge_with_value(
                                    src,
                                    vertex,
                                    bucket_index,
                                    *bucket,
                                    slot,
                                    E::read_from(&raw[byte_off..byte_end])
                                        .with_slot_index(slot)
                                        .with_label_id(bucket.bucket_label_key().raw()),
                                    log_chains.as_ref(),
                                ),
                            );
                            if slot == 0 {
                                bucket_rev_idx -= 1;
                                slot_rev = None;
                            } else {
                                slot_rev = Some(slot - 1);
                            }
                        }
                    }
                    true => {
                        for (local, bucket) in buckets.iter().enumerate() {
                            if bucket.degree() == 0 {
                                continue;
                            }
                            let bucket_index = lo + local as u32;
                            let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                            for slot in 0..bucket.degree() {
                                let rel = bucket
                                    .edge_start()
                                    .saturating_sub(base)
                                    .checked_add(u64::from(slot))
                                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                                let byte_off = usize::try_from(rel)
                                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
                                    .checked_mul(E::BYTES)
                                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                                let byte_end = byte_off
                                    .checked_add(E::BYTES)
                                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                                if byte_end > raw.len() {
                                    return Err(
                                        LaraOperationError::CollectAllocationOverflow.into()
                                    );
                                }
                                visit(
                                    self.labeled_edge_with_value(
                                        src,
                                        vertex,
                                        bucket_index,
                                        *bucket,
                                        slot,
                                        E::read_from(&raw[byte_off..byte_end])
                                            .with_slot_index(slot)
                                            .with_label_id(bucket.bucket_label_key().raw()),
                                        log_chains.as_ref(),
                                    ),
                                );
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
        match ascending {
            false => {
                for local_rev in (0..buckets.len()).rev() {
                    let bucket_index = lo + local_rev as u32;
                    let bucket = &buckets[local_rev];
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                    let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
                    let successor =
                        self.bucket_successor_start_after_bucket(vertex, bucket_index, bucket)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    if bucket.overflow_log_head() < 0 {
                        let it = OutEdgeSlabIter::try_new(
                            &self.edges,
                            bucket.edge_start(),
                            bucket.stored_slots,
                            bucket.degree(),
                        )?;
                        for edge in it {
                            let slot_index = edge.edge_slot_index_raw();
                            visit(self.labeled_edge_with_value(
                                src,
                                vertex,
                                bucket_index,
                                *bucket,
                                slot_index,
                                edge.with_label_id(bucket.bucket_label_key().raw()),
                                log_chains.as_ref(),
                            ));
                        }
                    } else {
                        for edge in self.edges.out_edges_iter(&acc, VertexId::from(0))? {
                            let slot_index = edge.edge_slot_index_raw();
                            visit(self.labeled_edge_with_value(
                                src,
                                vertex,
                                bucket_index,
                                *bucket,
                                slot_index,
                                edge.with_label_id(bucket.bucket_label_key().raw()),
                                log_chains.as_ref(),
                            ));
                        }
                    }
                }
            }
            true => {
                for local in 0..buckets.len() {
                    let bucket_index = lo + local as u32;
                    let bucket = &buckets[local];
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                    let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
                    let successor =
                        self.bucket_successor_start_after_bucket(vertex, bucket_index, bucket)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    for edge in self.edges.asc_out_edges(&acc, VertexId::from(0))? {
                        let slot_index = edge.edge_slot_index_raw();
                        visit(self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
    pub fn for_each_out_edges_by_directedness<Visit>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        self.for_each_out_edges_by_directedness_impl(
            src,
            &vertex,
            directedness,
            order.ascending(),
            &mut visit,
        )
    }
    pub fn for_each_out_edges_by_directedness_unchecked<Visit>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        debug_assert!(u32::from(src) < self.vertices.len());
        let vertex = self.vertices.get(src);
        self.for_each_out_edges_by_directedness_impl(
            src,
            &vertex,
            directedness,
            order.ascending(),
            &mut visit,
        )
    }
    pub fn find_edge_label(
        &self,
        src: VertexId,
        needle: &E,
    ) -> Result<Option<BucketLabelKey>, LabeledOperationError>
    where
        E: PartialEq,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(None);
            }
            let label = self.bypass_storage_label_for(&vertex);
            if needle
                .edge_label_id_raw()
                .is_some_and(|needle_label| needle_label != label.raw())
            {
                return Ok(None);
            }
            let mut found = false;
            self.edges.visit_out_edges(
                &self.vertices,
                src,
                None,
                None,
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                |_| true,
                |edge| {
                    let edge = edge.with_label_id(label.raw());
                    if Self::edge_matches_label_lookup(&edge, needle) {
                        found = true;
                    }
                },
            )?;
            return Ok(found.then_some(label));
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(None);
        }
        let mut buckets = Vec::with_capacity(deg as usize);
        for offset in 0..deg {
            let slot = Self::labeled_vertex_bucket_slot(&vertex, offset)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            buckets.push((slot, offset, bucket));
        }
        buckets.sort_by_key(|(_, _, bucket)| bucket.stored_slots);
        for (slot, bucket_index, bucket) in buckets {
            let successor =
                self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?;
            if needle
                .edge_label_id_raw()
                .is_some_and(|needle_label| needle_label != bucket.bucket_label_key().raw())
            {
                continue;
            }
            let log_chains = self.bucket_value_log_chains_opt(src, &bucket);
            let found_label = Cell::new(None);
            self.edges.visit_out_edges(
                &LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src),
                VertexId::from(0),
                None,
                None,
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                |_| found_label.get().is_none(),
                |edge| {
                    let slot_index = edge.edge_slot_index_raw();
                    let edge = self.labeled_edge_with_value(
                        src,
                        &vertex,
                        bucket_index,
                        bucket,
                        slot_index,
                        edge.with_label_id(bucket.bucket_label_key().raw()),
                        log_chains.as_ref(),
                    );
                    if Self::edge_matches_label_lookup(&edge, needle) {
                        found_label.set(Some(bucket.bucket_label_key()));
                    }
                },
            )?;
            if let Some(label_id) = found_label.into_inner() {
                return Ok(Some(label_id));
            }
        }
        Ok(None)
    }
    pub fn out_edge_label_ids(
        &self,
        src: VertexId,
    ) -> Result<Vec<BucketLabelKey>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(Vec::new());
            }
            return Ok(vec![self.bypass_storage_label_for(&vertex)]);
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(deg as usize);
        for offset in 0..deg {
            let slot = Self::labeled_vertex_bucket_slot(&vertex, offset)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            out.push(bucket.bucket_label_key());
        }
        Ok(out)
    }
    pub fn remove_edge_matching<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        matches: F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        let removed = self.remove_edge_matching_skip_leaf_cascade(src, label_id, matches)?;
        if removed.is_some() && self.labeled_leaf_segment_is_dense(src) {
            self.rebalance_cascade_after_labeled_mutation(src)?;
        }
        Ok(removed)
    }
    pub(crate) fn remove_edge_matching_skip_leaf_cascade<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        mut matches: F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(None);
            }
            if vertex.degree() == 0 {
                return Ok(None);
            }
            return self
                .edges
                .remove_edge_slab_tombstone_matching(&self.vertices, src, matches)
                .map_err(Into::into);
        }
        if let BucketSearch::Found { slot, mut bucket } =
            self.find_bucket(src, &vertex, label_id)?
        {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_remove_edge_skip_leaf");
            let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
            if bucket.degree() == 0 {
                return Ok(None);
            }
            if bucket.overflow_log_head() >= 0 {
                bucket =
                    self.ensure_label_bucket_folded_to_slab(src, bucket_index, slot, bucket)?;
                if bucket.degree() == 0 {
                    return Ok(None);
                }
            }
            let log_chains = self.bucket_value_log_chains_opt(src, &bucket);
            let stored = bucket.stored_slots;
            let mut found = None;
            for offset in 0..stored {
                let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(offset))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let edge = self.edges.read_slot(edge_slot);
                if edge.is_tombstone_edge() {
                    continue;
                }
                let edge_with_value = self.labeled_edge_with_value(
                    src,
                    &vertex,
                    bucket_index,
                    bucket,
                    offset,
                    edge,
                    log_chains.as_ref(),
                );
                if matches(&edge_with_value) {
                    found = Some((offset, edge_with_value));
                    break;
                }
            }
            let Some((local_index, removed)) = found else {
                return Ok(None);
            };
            let rm_slot = checked_add_slot_index(bucket.edge_start(), u64::from(local_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.edges
                .write_slot(rm_slot, E::tombstone_edge())
                .map_err(LabeledOperationError::from)?;
            let updated = bucket
                .after_slab_tombstone_delete()
                .with_overflow_log_head(-1);
            let updated = if updated.degree() == 0 {
                self.release_bucket_value_span(src, &bucket)?;
                updated
                    .with_value_log_head(-1)
                    .with_edge_range(updated.edge_start(), 0)
            } else {
                updated
            };
            self.buckets.write_label_bucket_slot(slot, updated)?;
            let hdr = self.edges.header();
            let next_global = hdr
                .num_edges
                .checked_sub(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.edges.set_num_edges(next_global);
            self.edges
                .bump_vertex_segment_counts(src, -1, 0)
                .map_err(LabeledOperationError::from)?;
            return Ok(Some(removed));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use crate::VertexId;

    #[test]
    fn remove_edge_at_slot_uses_edge_tombstone_contract() {
        let graph = flag_tombstone_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, FlagTombstoneEdge::live(10))
            .unwrap();

        let removed = graph
            .remove_edge_matching(VertexId::from(0), road, |edge| {
                edge.neighbor_vid() == VertexId::from(10)
            })
            .unwrap();
        assert_eq!(removed, Some(FlagTombstoneEdge::live(10)));

        let removed_again = graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap();
        assert_eq!(removed_again, None);
    }

    #[test]
    fn remove_edge_leaves_slab_tombstone_until_rebalance() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 12 })
            .unwrap();
        assert!(
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 11)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 12 }, TestEdge { target: 10 }]
        );
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots, 3);
        assert_eq!(bucket.stored_slots.saturating_sub(bucket.degree), 1);
        assert_eq!(bucket.degree(), 2);

        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 13 })
            .unwrap();
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![
                TestEdge { target: 13 },
                TestEdge { target: 12 },
                TestEdge { target: 10 },
            ]
        );
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots, 4);
        assert_eq!(bucket.stored_slots.saturating_sub(bucket.degree), 1);
        assert_eq!(bucket.degree(), 3);
    }
    #[test]
    fn remove_edge_from_one_label_keeps_next_label_isolated() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        let walk = BucketLabelKey::from_raw(3);
        for target in [10, 11] {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        for target in [20, 21] {
            graph
                .insert_edge(VertexId::from(0), walk, TestEdge { target })
                .unwrap();
        }

        graph
            .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 10)
            .unwrap();

        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 11 }]
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), walk).unwrap(),
            vec![TestEdge { target: 21 }, TestEdge { target: 20 }]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }
    #[test]
    fn default_bypass_conversion_clears_vertex_edge_span_allocation() {
        let graph = test_graph();
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                VertexId::from(0),
                LabelBucket::default().with_bucket_label_key(graph.default_label()),
            )
            .unwrap();
        for target in [7u32, 8] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    graph.default_label(),
                    TestEdge { target },
                )
                .unwrap();
        }

        let before = graph.vertices().get(VertexId::from(0));
        assert!(!before.is_default_edge_labeled());
        assert_eq!(before.degree(), 1);

        graph.enable_default_edge_bypass(VertexId::from(0)).unwrap();

        let after = graph.vertices().get(VertexId::from(0));
        assert!(after.is_default_edge_labeled());
        assert_eq!(after.degree(), 2);
        assert_eq!(after.stored_slots, 2);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), graph.default_label())
                .unwrap(),
            vec![TestEdge { target: 8 }, TestEdge { target: 7 }]
        );
    }
    #[test]
    fn releasing_one_bucket_value_span_keeps_other_bucket_value_log() {
        let graph = valued_test_graph_with_capacity(1 << 20);
        for _ in 0..2 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let road = BucketLabelKey::from_raw(2);
        let rail = BucketLabelKey::from_raw(3);
        for (src, label) in [(VertexId::from(0), road), (VertexId::from(1), rail)] {
            graph
                .ensure_label_bucket_value_width(src, label, ValueWidthCode::W2)
                .unwrap();
        }
        for (src, label) in [(VertexId::from(0), road), (VertexId::from(1), rail)] {
            for target in 1..=33u32 {
                let weight = if label == road {
                    target
                } else {
                    target.saturating_mul(10)
                };
                graph
                    .insert_edge_skip_leaf_cascade(
                        src,
                        label,
                        ValuedTestEdge::with_u16(target, u16::try_from(weight).unwrap()),
                    )
                    .unwrap();
            }
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let road_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let road_bucket = graph.buckets().read_label_bucket_slot(road_slot).unwrap();
        let rail_vertex = graph.vertices().get(VertexId::from(1));
        let rail_slot = graph.find_bucket_slot(&rail_vertex, rail).unwrap().unwrap();
        let rail_bucket = graph.buckets().read_label_bucket_slot(rail_slot).unwrap();
        assert!(road_bucket.value_log_head() >= 0);
        assert!(rail_bucket.value_log_head() >= 0);
        let leaf = graph.value_log_leaf(VertexId::from(1));
        let rail_head = u32::try_from(rail_bucket.value_log_head()).unwrap();
        let mut before = [0u8; 2];
        graph.values().read_value_log_entry(
            leaf,
            rail_head,
            rail_bucket.value_width(),
            &mut before,
        );

        let road_bucket_log_only = road_bucket.with_degree_field(0).with_stored_slots(0);
        graph
            .release_bucket_value_span(VertexId::from(0), &road_bucket_log_only)
            .unwrap();

        let mut after = [0u8; 2];
        graph
            .values()
            .read_value_log_entry(leaf, rail_head, rail_bucket.value_width(), &mut after);
        assert_ne!(before, [0, 0]);
        assert_eq!(after, before);
    }
    #[test]
    fn removing_last_valued_edge_clears_vertex_value_allocation() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 42))
            .unwrap();
        assert_eq!(
            graph
                .vertices()
                .get(VertexId::from(0))
                .value_allocated_bytes(),
            2
        );

        graph
            .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 1)
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.value_allocated_bytes(), 0);
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert!(!bucket.is_value_allocated());
        assert_eq!(bucket.value_log_head(), -1);
    }
    #[test]
    fn removing_last_valued_edge_by_slot_clears_vertex_value_allocation() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 42))
            .unwrap();
        assert_eq!(
            graph
                .vertices()
                .get(VertexId::from(0))
                .value_allocated_bytes(),
            2
        );

        let removed = graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed edge");
        assert_eq!(removed.target, 1);

        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.value_allocated_bytes(), 0);
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert!(!bucket.is_value_allocated());
        assert_eq!(bucket.value_log_head(), -1);
    }
}
