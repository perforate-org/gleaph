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

use super::error::LabeledOperationError;
use super::{BucketSearch, LabeledLaraGraph, OutEdgeOrder};

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    fn decrement_edge_counts_after_remove(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let hdr = self.edges.header();
        let next_global = hdr
            .num_edges
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.edges.set_num_edges(next_global);
        self.edges
            .bump_vertex_segment_counts(src, -1, 0)
            .map_err(LabeledOperationError::from)
    }

    fn default_bypass_edge_log_slots(&self, src: VertexId, vertex: &LabeledVertex) -> u32 {
        let head = vertex.bypass_overflow_log_head();
        if head < 0 {
            return 0;
        }
        let leaf = self.payload_log_leaf(src);
        self.edges.overflow_log_chain_asc_indices(leaf, head).len() as u32
    }

    fn default_bypass_slab_prefix_slots(&self, src: VertexId, vertex: &LabeledVertex) -> u32 {
        vertex
            .stored_degree()
            .saturating_sub(self.default_bypass_edge_log_slots(src, vertex))
    }

    fn remove_default_bypass_edge_at_slot(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        vertex: LabeledVertex,
        slot_index: u32,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if label_id != self.bypass_storage_label_for(&vertex) {
            return Ok(None);
        }
        if slot_index >= vertex.stored_degree() {
            return Ok(None);
        }
        let slab_prefix_slots = self.default_bypass_slab_prefix_slots(src, &vertex);
        if slot_index >= slab_prefix_slots {
            let leaf = self.payload_log_leaf(src);
            let log_ordinal = slot_index
                .checked_sub(slab_prefix_slots)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let chain = self
                .edges
                .overflow_log_chain_asc_indices(leaf, vertex.bypass_overflow_log_head());
            let Some(&entry_idx) = chain.get(log_ordinal as usize) else {
                return Ok(None);
            };
            let (_, src_tag, removed) = self.edges.read_overflow_log_entry(leaf, entry_idx);
            if src_tag < 0 || removed.is_tombstone_edge() {
                return Ok(None);
            }
            self.edges.mark_overflow_log_entry_dead(leaf, entry_idx)?;
            self.vertices
                .set(src, &vertex.after_slab_tombstone_delete());
            self.decrement_edge_counts_after_remove(src)?;
            return Ok(Some(
                removed
                    .with_slot_index(slot_index)
                    .with_label_id(label_id.raw()),
            ));
        }

        let rm_slot = checked_add_slot_index(vertex.base_slot_start(), u64::from(slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let removed = self.edges.read_slot(rm_slot);
        if removed.is_deleted_slot() || removed.is_tombstone_edge() {
            return Ok(None);
        }
        self.edges
            .write_slot(rm_slot, E::tombstone_edge())
            .map_err(LabeledOperationError::from)?;
        self.vertices
            .set(src, &vertex.after_slab_tombstone_delete());
        self.decrement_edge_counts_after_remove(src)?;
        Ok(Some(
            removed
                .with_slot_index(slot_index)
                .with_label_id(label_id.raw()),
        ))
    }

    fn remove_default_bypass_edge_matching<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        vertex: LabeledVertex,
        mut matches: F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        if label_id != self.bypass_storage_label_for(&vertex) {
            return Ok(None);
        }
        if vertex.degree() == 0 {
            return Ok(None);
        }

        let slab_prefix_slots = self.default_bypass_slab_prefix_slots(src, &vertex);
        for slot_index in 0..slab_prefix_slots {
            let edge_slot = checked_add_slot_index(vertex.base_slot_start(), u64::from(slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.edges.read_slot(edge_slot);
            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                continue;
            }
            let edge = edge
                .with_slot_index(slot_index)
                .with_label_id(label_id.raw());
            if matches(&edge) {
                return self.remove_default_bypass_edge_at_slot(src, label_id, vertex, slot_index);
            }
        }

        if vertex.bypass_overflow_log_head() < 0 {
            return Ok(None);
        }
        let leaf = self.payload_log_leaf(src);
        let chain = self
            .edges
            .overflow_log_chain_asc_indices(leaf, vertex.bypass_overflow_log_head());
        for (ordinal, entry_idx) in chain.into_iter().enumerate() {
            let (_, src_tag, edge) = self.edges.read_overflow_log_entry(leaf, entry_idx);
            if src_tag < 0 || edge.is_tombstone_edge() {
                continue;
            }
            let slot_index = slab_prefix_slots
                .checked_add(
                    u32::try_from(ordinal)
                        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?,
                )
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = edge
                .with_slot_index(slot_index)
                .with_label_id(label_id.raw());
            if matches(&edge) {
                return self.remove_default_bypass_edge_at_slot(src, label_id, vertex, slot_index);
            }
        }
        Ok(None)
    }

    /// Removes the edge stored at `slot_index` in the bucket identified by `label_id`.
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
            return self.remove_default_bypass_edge_at_slot(src, label_id, vertex, slot_index);
        }
        let BucketSearch::Found { slot, bucket } = self.find_bucket(src, &vertex, label_id)? else {
            return Ok(None);
        };
        if slot_index >= self.bucket_reserved_edge_slots(src, &bucket) {
            return Ok(None);
        }
        let slab_prefix_slots = self.bucket_slab_prefix_slots(src, &bucket);
        if slot_index >= slab_prefix_slots {
            let leaf = self.payload_log_leaf(src);
            let log_ordinal = slot_index
                .checked_sub(slab_prefix_slots)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let chain = self
                .edges
                .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
            let Some(&entry_idx) = chain.get(log_ordinal as usize) else {
                return Ok(None);
            };
            let (_, src_tag, removed) = self.edges.read_overflow_log_entry(leaf, entry_idx);
            if src_tag < 0 || removed.is_tombstone_edge() {
                return Ok(None);
            }
            self.edges.mark_overflow_log_entry_dead(leaf, entry_idx)?;
            self.mark_payload_log_slot_dead(src, &bucket, slot_index)?;
            let updated = bucket.after_slab_tombstone_delete();
            self.buckets.write_label_bucket_slot(slot, updated)?;
            self.decrement_edge_counts_after_remove(src)?;
            self.invalidate_bucket_lookup_for_label(src, label_id);
            return Ok(Some(removed.with_slot_index(slot_index)));
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
        let updated = bucket.after_slab_tombstone_delete();
        let updated = if updated.degree() == 0 && updated.overflow_log_head() < 0 {
            self.release_bucket_payload_span(src, &bucket)?;
            updated
                .with_payload_log_head(-1)
                .with_edge_range(updated.edge_start(), 0)
        } else {
            updated
        };
        self.buckets.write_label_bucket_slot(slot, updated)?;
        self.decrement_edge_counts_after_remove(src)?;
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
        if let Some(run) =
            Self::try_contiguous_tiled_labeled_out_edges_slice(&buckets, span_end_exclusive)
        {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_out_edges_by_directedness_tiled");
            if run.total_edges() > 0 {
                let nbytes = run.byte_len::<E>()?;
                let mut raw = vec![0u8; nbytes];
                self.edges.read_slots_contiguous(run.base(), &mut raw);
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
                            let log_chains = self.bucket_payload_log_chains_opt(src, bucket);
                            let slot = slot_rev.unwrap_or(bucket.degree().saturating_sub(1));
                            let chunk = run.edge_chunk::<E>(&raw, bucket, slot)?;
                            visit(
                                self.attach_edge_payload(
                                    src,
                                    vertex,
                                    bucket_index,
                                    *bucket,
                                    slot,
                                    E::read_from(chunk)
                                        .with_slot_index(slot)
                                        .with_label_id(bucket.bucket_label_key().raw()),
                                    log_chains.as_ref(),
                                )?,
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
                            let log_chains = self.bucket_payload_log_chains_opt(src, bucket);
                            for slot in 0..bucket.degree() {
                                let chunk = run.edge_chunk::<E>(&raw, bucket, slot)?;
                                visit(
                                    self.attach_edge_payload(
                                        src,
                                        vertex,
                                        bucket_index,
                                        *bucket,
                                        slot,
                                        E::read_from(chunk)
                                            .with_slot_index(slot)
                                            .with_label_id(bucket.bucket_label_key().raw()),
                                        log_chains.as_ref(),
                                    )?,
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
                    let log_chains = self.bucket_payload_log_chains_opt(src, bucket);
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
                            visit(self.attach_edge_payload(
                                src,
                                vertex,
                                bucket_index,
                                *bucket,
                                slot_index,
                                edge.with_label_id(bucket.bucket_label_key().raw()),
                                log_chains.as_ref(),
                            )?);
                        }
                    } else {
                        for edge in self.edges.out_edges_iter(&acc, VertexId::from(0))? {
                            let slot_index = edge.edge_slot_index_raw();
                            visit(self.attach_edge_payload(
                                src,
                                vertex,
                                bucket_index,
                                *bucket,
                                slot_index,
                                edge.with_label_id(bucket.bucket_label_key().raw()),
                                log_chains.as_ref(),
                            )?);
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
                    let log_chains = self.bucket_payload_log_chains_opt(src, bucket);
                    let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
                    let successor =
                        self.bucket_successor_start_after_bucket(vertex, bucket_index, bucket)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    for edge in self.edges.asc_out_edges(&acc, VertexId::from(0))? {
                        let slot_index = edge.edge_slot_index_raw();
                        visit(self.attach_edge_payload(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        )?);
                    }
                }
            }
        }
        Ok(())
    }

    /// Visits outgoing edges whose bucket directedness matches `directedness`.
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

    /// Visits outgoing edges by directedness without checking that `src` is in range.
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

    /// Finds the label bucket containing `needle` on `src`.
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
            let log_chains = self.bucket_payload_log_chains_opt(src, &bucket);
            let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
            for edge in self.edges.out_edges_iter(&acc, VertexId::from(0))? {
                let slot_index = edge.edge_slot_index_raw();
                let edge = self.attach_edge_payload(
                    src,
                    &vertex,
                    bucket_index,
                    bucket,
                    slot_index,
                    edge.with_label_id(bucket.bucket_label_key().raw()),
                    log_chains.as_ref(),
                )?;
                if Self::edge_matches_label_lookup(&edge, needle) {
                    return Ok(Some(bucket.bucket_label_key()));
                }
            }
        }
        Ok(None)
    }

    /// Returns the labels that currently have outgoing edges for `src`.
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

    /// Removes the first edge in `label_id` for `src` that satisfies `matches`.
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
        self.remove_edge_matching_skip_leaf_cascade(src, label_id, matches)
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
            return self.remove_default_bypass_edge_matching(src, label_id, vertex, matches);
        }
        if let BucketSearch::Found { slot, bucket } = self.find_bucket(src, &vertex, label_id)? {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_remove_edge_skip_leaf");
            let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
            if bucket.degree() == 0 {
                return Ok(None);
            }
            if bucket.overflow_log_head() >= 0 {
                let log_chains = self.bucket_payload_log_chains_opt(src, &bucket);
                let slab_prefix_slots = self.bucket_slab_prefix_slots(src, &bucket);
                for slot_index in 0..slab_prefix_slots {
                    let edge_slot =
                        checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let edge = self.edges.read_slot(edge_slot);
                    if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                        continue;
                    }
                    let edge_with_value = self.attach_edge_payload(
                        src,
                        &vertex,
                        bucket_index,
                        bucket,
                        slot_index,
                        edge.with_label_id(bucket.bucket_label_key().raw()),
                        log_chains.as_ref(),
                    )?;
                    if matches(&edge_with_value) {
                        if self
                            .remove_edge_at_slot(src, label_id, slot_index)?
                            .is_some()
                        {
                            return Ok(Some(edge_with_value));
                        }
                    }
                }
                let leaf = self.payload_log_leaf(src);
                let chain = self
                    .edges
                    .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
                for (ordinal, entry_idx) in chain.into_iter().enumerate() {
                    let (_, src_tag, edge) = self.edges.read_overflow_log_entry(leaf, entry_idx);
                    if src_tag < 0 || edge.is_tombstone_edge() {
                        continue;
                    }
                    let slot_index = bucket
                        .stored_slots
                        .saturating_sub(self.bucket_edge_log_slots(src, &bucket))
                        .checked_add(
                            u32::try_from(ordinal)
                                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?,
                        )
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let edge_with_value = self.attach_edge_payload(
                        src,
                        &vertex,
                        bucket_index,
                        bucket,
                        slot_index,
                        edge.with_label_id(bucket.bucket_label_key().raw()),
                        log_chains.as_ref(),
                    )?;
                    if matches(&edge_with_value) {
                        if self
                            .remove_edge_at_slot(src, label_id, slot_index)?
                            .is_some()
                        {
                            return Ok(Some(edge_with_value));
                        }
                    }
                }
                return Ok(None);
            }
            let stored = bucket.stored_slots;
            let mut found = None;
            if bucket.is_payload_allocated() {
                let log_chains = self.bucket_payload_log_chains_opt(src, &bucket);
                for offset in 0..stored {
                    let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(offset))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let edge = self.edges.read_slot(edge_slot);
                    if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                        continue;
                    }
                    let edge_with_value = self.attach_edge_payload(
                        src,
                        &vertex,
                        bucket_index,
                        bucket,
                        offset,
                        edge,
                        log_chains.as_ref(),
                    )?;
                    if matches(&edge_with_value) {
                        found = Some((offset, edge_with_value));
                        break;
                    }
                }
            } else {
                for offset in 0..stored {
                    let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(offset))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let edge = self.edges.read_slot(edge_slot);
                    if edge.is_tombstone_edge() {
                        continue;
                    }
                    if matches(&edge) {
                        found = Some((offset, edge));
                        break;
                    }
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
            let updated = bucket.after_slab_tombstone_delete();
            let updated = if updated.degree() == 0 && updated.overflow_log_head() < 0 {
                self.release_bucket_payload_span(src, &bucket)?;
                updated
                    .with_payload_log_head(-1)
                    .with_edge_range(updated.edge_start(), 0)
            } else {
                updated
            };
            self.buckets.write_label_bucket_slot(slot, updated)?;
            self.decrement_edge_counts_after_remove(src)?;
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
        assert_eq!(
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge { target: 10 }, TestEdge { target: 12 }]
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
        assert_eq!(
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![
                TestEdge { target: 10 },
                TestEdge { target: 12 },
                TestEdge { target: 13 },
            ]
        );
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots, 4);
        assert_eq!(bucket.stored_slots.saturating_sub(bucket.degree), 1);
        assert_eq!(bucket.degree(), 3);
    }

    #[test]
    fn removing_log_edge_dead_marks_entry_and_preserves_later_slot() {
        let graph = payload_test_graph_with_capacity(1 << 20);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=35u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }

        let slot_of = |target| {
            let mut slot = None;
            graph
                .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                    if edge.target == target {
                        slot = Some(edge.slot_index);
                    }
                })
                .unwrap();
            slot.expect("target edge exists")
        };
        let later_slot_before = slot_of(34);
        let removed_slot = slot_of(33);

        let removed = graph
            .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 33)
            .unwrap()
            .expect("log edge removed");
        assert_eq!(removed.target, 33);
        assert_eq!(slot_of(34), later_slot_before);
        assert!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .iter()
                .all(|edge| edge.target != 33)
        );

        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        let slab_prefix = graph.bucket_slab_prefix_slots(VertexId::from(0), &bucket);
        let log_ordinal = removed_slot
            .checked_sub(slab_prefix)
            .expect("removed edge was log-backed");
        let leaf = graph.payload_log_leaf(VertexId::from(0));
        let chain = graph
            .edges()
            .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
        let entry_idx = chain[log_ordinal as usize];
        let (_, src_tag, _) = graph.edges().read_overflow_log_entry(leaf, entry_idx);
        assert_eq!(src_tag, crate::lara::edge::LOG_SRC_DEAD);
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
    fn releasing_one_bucket_payload_span_keeps_other_bucket_payload_log() {
        let graph = payload_test_graph_with_capacity(1 << 20);
        for _ in 0..2 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let road = BucketLabelKey::from_raw(2);
        let rail = BucketLabelKey::from_raw(3);
        for (src, label) in [(VertexId::from(0), road), (VertexId::from(1), rail)] {
            graph
                .ensure_label_bucket_payload_byte_width(src, label, 2u16)
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
                        PayloadTestEdge::with_bytes(
                            target,
                            &u16::try_from(weight).unwrap().to_le_bytes(),
                        ),
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
        assert!(road_bucket.payload_log_head() >= 0);
        assert!(rail_bucket.payload_log_head() >= 0);
        let leaf = graph.payload_log_leaf(VertexId::from(1));
        let rail_head = u32::try_from(rail_bucket.payload_log_head()).unwrap();
        let mut before = [0u8; 2];
        graph
            .values()
            .read_payload_log_entry(
                leaf,
                rail_head,
                rail_bucket.payload_byte_width(),
                &mut before,
            )
            .expect("read before");

        let road_bucket_log_only = road_bucket.with_degree_field(0).with_stored_slots(0);
        graph
            .release_bucket_payload_span(VertexId::from(0), &road_bucket_log_only)
            .unwrap();

        let mut after = [0u8; 2];
        graph
            .values()
            .read_payload_log_entry(
                leaf,
                rail_head,
                rail_bucket.payload_byte_width(),
                &mut after,
            )
            .expect("read after");
        assert_ne!(before, [0, 0]);
        assert_eq!(after, before);
    }

    #[test]
    fn removing_last_payloaded_edge_clears_vertex_payload_allocation() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(1, &42u16.to_le_bytes()),
            )
            .unwrap();
        assert_eq!(
            graph
                .vertices()
                .get(VertexId::from(0))
                .payload_allocated_bytes(),
            2
        );

        graph
            .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 1)
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.payload_allocated_bytes(), 0);
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert!(!bucket.is_payload_allocated());
        assert_eq!(bucket.payload_log_head(), -1);
    }

    #[test]
    fn removing_last_payloaded_edge_by_slot_clears_vertex_payload_allocation() {
        let graph = payload_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_payload_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                PayloadTestEdge::with_bytes(1, &42u16.to_le_bytes()),
            )
            .unwrap();
        assert_eq!(
            graph
                .vertices()
                .get(VertexId::from(0))
                .payload_allocated_bytes(),
            2
        );

        let removed = graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed edge");
        assert_eq!(removed.target, 1);

        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.payload_allocated_bytes(), 0);
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert!(!bucket.is_payload_allocated());
        assert_eq!(bucket.payload_log_head(), -1);
    }
}
