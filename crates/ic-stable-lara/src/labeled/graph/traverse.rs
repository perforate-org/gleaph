//! Labeled graph `traverse` implementation.

use crate::{
    VertexId,
    labeled::{
        access::LabelEdgeSpanAccess,
        bucket_label_key::{BucketDirectedness, BucketLabelKey},
        record::{LabelBucket, LabeledVertex},
        slot_index::checked_add_slot_index,
    },
    lara::{
        edge::{OutEdgeSlabIter, OutEdgeVisitWindow, OutEdgesIter},
        operation_error::LaraOperationError,
    },
    traits::{CsrEdge, CsrVertex},
};
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::iter::{LabeledEdgeValueBatch, LabeledEdgeValueBatchScratch, LabeledOutEdgesIterKind};
use super::{BucketSearch, LabeledLaraGraph, LabeledOutEdgesIter, LabeledSpanIter, OutEdgeOrder};

const EDGE_VALUE_BATCH_TARGET_BYTES: usize = 2048;

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn try_contiguous_tiled_labeled_out_edges_slice(
        buckets: &[LabelBucket],
        span_end_exclusive: u64,
    ) -> Option<(u64, u32)> {
        if buckets.is_empty() {
            return None;
        }
        if buckets.iter().any(|b| b.overflow_log_head() >= 0) {
            return None;
        }
        if buckets.iter().any(|b| b.stored_slots != b.degree()) {
            return None;
        }
        let base = buckets.first()?.edge_start();
        let mut pos = base;
        let mut total_edges: u32 = 0;
        for b in buckets {
            if b.edge_start() != pos {
                return None;
            }
            total_edges = total_edges.checked_add(b.stored_slots)?;
            pos =
                crate::labeled::slot_index::checked_add_slot_index(pos, u64::from(b.stored_slots))?;
        }
        if pos > span_end_exclusive {
            return None;
        }
        Some((base, total_edges))
    }
    pub(super) fn try_contiguous_tiled_labeled_out_edges(
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Option<(u64, u32)> {
        let deg = vertex.degree() as usize;
        if deg == 0 || buckets.len() != deg {
            return None;
        }
        if buckets.iter().any(|b| b.overflow_log_head() >= 0) {
            return None;
        }
        let base = buckets.first()?.edge_start();
        let mut pos = base;
        let mut total_edges: u32 = 0;
        for b in buckets {
            if b.edge_start() != pos {
                return None;
            }
            total_edges = total_edges.checked_add(b.stored_slots)?;
            pos =
                crate::labeled::slot_index::checked_add_slot_index(pos, u64::from(b.stored_slots))?;
        }
        let span_end = crate::labeled::slot_index::checked_add_slot_index(
            base,
            u64::from(vertex.stored_slots),
        )?;
        if pos > span_end {
            return None;
        }
        Some((base, total_edges))
    }
    pub fn for_each_edges_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        let mut visit = visit;
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(());
            }
            return self
                .edges
                .visit_out_edges(
                    &self.vertices,
                    src,
                    None,
                    None,
                    None::<&mut dyn FnMut(&[u8]) -> bool>,
                    |_| true,
                    |edge| visit(edge.with_label_id(label_id.raw())),
                )
                .map_err(Into::into);
        }
        for edge in
            self.out_edges_iter_for_label_ordered(src, label_id, OutEdgeOrder::Descending)?
        {
            visit(edge.with_label_id(label_id.raw()));
        }
        Ok(())
    }
    pub fn for_each_edges_for_label_ordered<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        for edge in self.out_edges_iter_for_label_ordered(src, label_id, order)? {
            visit(edge.with_label_id(label_id.raw()));
        }
        Ok(())
    }
    pub(super) fn out_edges_iter_for_label_ordered(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
    ) -> Result<LabeledSpanIter<'_, E, M>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(LabeledSpanIter::Empty);
            }
            return match order {
                OutEdgeOrder::Descending => Ok(LabeledSpanIter::Desc {
                    graph: self,
                    src,
                    vertex,
                    bucket_index: 0,
                    bucket: LabelBucket::default(),
                    label_id,
                    log_chains: None,
                    iter: self.edges.out_edges_iter(&self.vertices, src)?,
                }),
                OutEdgeOrder::Ascending => Ok(LabeledSpanIter::Asc {
                    graph: self,
                    src,
                    vertex,
                    bucket_index: 0,
                    bucket: LabelBucket::default(),
                    label_id,
                    log_chains: None,
                    iter: self.edges.asc_out_edges_iter(&self.vertices, src)?,
                }),
            };
        }
        match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => {
                let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
                self.labeled_bucket_span_iter(src, order, &vertex, &[bucket], 0, bucket_index)
            }
            BucketSearch::Missing { .. } => Ok(LabeledSpanIter::Empty),
        }
    }
    pub(crate) fn out_edges_iter_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<OutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(OutEdgesIter::empty(&self.edges));
            }
            return self
                .edges
                .out_edges_iter(&self.vertices, src)
                .map_err(LabeledOperationError::Store);
        }
        match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => {
                let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
                let successor_start =
                    self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?;
                self.edges
                    .out_edges_iter(
                        &LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src),
                        VertexId::from(0),
                    )
                    .map_err(LabeledOperationError::Store)
            }
            BucketSearch::Missing { .. } => Ok(OutEdgesIter::empty(&self.edges)),
        }
    }
    pub(crate) fn skip_then_visit_each_out_edge_for_label<Visit, Err>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        offset_remaining: &mut usize,
        mut visit: Visit,
    ) -> Result<Result<bool, Err>, LabeledOperationError>
    where
        Visit: FnMut(E) -> Result<bool, Err>,
    {
        let skip = *offset_remaining;
        let mut it =
            self.out_edges_iter_for_label_ordered(src, label_id, OutEdgeOrder::Descending)?;
        match Iterator::advance_by(&mut it, skip) {
            Ok(()) => {
                *offset_remaining = 0;
            }
            Err(nz) => {
                *offset_remaining = nz.get();
                return Ok(Ok(false));
            }
        }
        loop {
            let Some(edge) = it.next() else {
                return Ok(Ok(false));
            };
            match visit(edge) {
                Ok(false) => continue,
                Ok(true) => return Ok(Ok(true)),
                Err(e) => return Ok(Err(e)),
            }
        }
    }
    pub(crate) fn skip_then_visit_each_out_edge_by_directedness<Visit, Err>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        offset_remaining: &mut usize,
        mut visit: Visit,
    ) -> Result<Result<bool, Err>, LabeledOperationError>
    where
        Visit: FnMut(E) -> Result<bool, Err>,
    {
        let skip = *offset_remaining;
        let mut it =
            self.out_edges_by_directedness_iter(src, directedness, OutEdgeOrder::Descending)?;
        match Iterator::advance_by(&mut it, skip) {
            Ok(()) => {
                *offset_remaining = 0;
            }
            Err(nz) => {
                *offset_remaining = nz.get();
                return Ok(Ok(false));
            }
        }
        loop {
            let Some(edge) = it.next() else {
                return Ok(Ok(false));
            };
            match visit(edge) {
                Ok(false) => continue,
                Ok(true) => return Ok(Ok(true)),
                Err(e) => return Ok(Err(e)),
            }
        }
    }
    pub fn for_each_edges_for_label_unchecked<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        debug_assert!(u32::from(src) < self.vertices.len());
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(());
            }
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_unchecked_bypass_slab");
            return self
                .edges
                .visit_out_edges(
                    &self.vertices,
                    src,
                    None,
                    None,
                    None::<&mut dyn FnMut(&[u8]) -> bool>,
                    |_| true,
                    |edge| visit(edge.with_label_id(label_id.raw())),
                )
                .map_err(Into::into);
        }
        for edge in
            self.out_edges_iter_for_label_ordered(src, label_id, OutEdgeOrder::Descending)?
        {
            visit(edge.with_label_id(label_id.raw()));
        }
        Ok(())
    }
    pub(super) fn visit_label_out_edges_inner<Match, Visit>(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        ascending: bool,
        offset: Option<usize>,
        limit: Option<usize>,
        mut raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: &mut Match,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        let mut window = OutEdgeVisitWindow::new(offset, limit);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(());
            }
            let label = self.bypass_storage_label_for(vertex).raw();
            if !ascending {
                let mut it = OutEdgeSlabIter::try_new(
                    &self.edges,
                    vertex.base_slot_start(),
                    vertex.stored_degree(),
                    vertex.degree(),
                )?;
                let has_raw = raw_matches.is_some();
                while let Some(edge) = it.next_live_edge_filtered(&mut raw_matches) {
                    let edge = edge.with_label_id(label);
                    if has_raw {
                        if matches(&edge) && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    } else if matches(&edge) && !window.emit_edge(edge, visit) {
                        return Ok(());
                    }
                }
                return Ok(());
            }
            for edge in self.edges.asc_out_edges(&self.vertices, src)? {
                let edge = edge.with_label_id(label);
                let passes = if let Some(raw_m) = raw_matches.as_mut() {
                    let mut buf = vec![0u8; E::BYTES];
                    edge.write_to(&mut buf);
                    raw_m(&buf) && matches(&edge)
                } else {
                    matches(&edge)
                };
                if passes && !window.emit_edge(edge, visit) {
                    return Ok(());
                }
            }
            return Ok(());
        }

        let buckets = self.read_vertex_label_buckets(&vertex)?;
        if let Some((base, total_edges)) =
            Self::try_contiguous_tiled_labeled_out_edges(&vertex, &buckets)
        {
            if total_edges == 0 {
                return Ok(());
            }
            let nbytes = (total_edges as usize)
                .checked_mul(E::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut raw = vec![0u8; nbytes];
            self.edges.read_slots_contiguous(base, &mut raw);
            if !ascending {
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
                    let bucket_index = bidx as u32;
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
                    let chunk = &raw[byte_off..byte_end];
                    let cont = if let Some(raw_m) = raw_matches.as_mut() {
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot,
                            E::read_from(chunk)
                                .with_slot_index(slot)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        if raw_m(chunk) {
                            if matches(&edge) {
                                window.emit_edge(edge, visit)
                            } else {
                                true
                            }
                        } else {
                            true
                        }
                    } else {
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot,
                            E::read_from(chunk)
                                .with_slot_index(slot)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        if matches(&edge) {
                            window.emit_edge(edge, visit)
                        } else {
                            true
                        }
                    };
                    if !cont {
                        return Ok(());
                    }
                    if slot == 0 {
                        bucket_rev_idx -= 1;
                        slot_rev = None;
                    } else {
                        slot_rev = Some(slot - 1);
                    }
                }
            } else {
                for (bucket_index, bucket) in buckets.iter().enumerate() {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let bucket_index = bucket_index as u32;
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
                            return Err(LaraOperationError::CollectAllocationOverflow.into());
                        }
                        let chunk = &raw[byte_off..byte_end];
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot,
                            E::read_from(chunk)
                                .with_slot_index(slot)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        let passes = if let Some(raw_m) = raw_matches.as_mut() {
                            raw_m(chunk) && matches(&edge)
                        } else {
                            matches(&edge)
                        };
                        if passes && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    }
                }
            }
            return Ok(());
        }

        if !ascending {
            for bucket_index in (0..buckets.len()).rev() {
                let bucket_index = bucket_index as u32;
                let bucket = &buckets[bucket_index as usize];
                if bucket.degree() == 0 {
                    continue;
                }
                let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                let successor_start =
                    self.bucket_successor_start_after_bucket(&vertex, bucket_index, bucket)?;
                let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src);
                if bucket.overflow_log_head() < 0 {
                    let mut it = OutEdgeSlabIter::try_new(
                        &self.edges,
                        bucket.edge_start(),
                        bucket.stored_slots,
                        bucket.degree(),
                    )?;
                    let has_raw = raw_matches.is_some();
                    while let Some(edge) = it.next_live_edge_filtered(&mut raw_matches) {
                        let slot_index = edge.edge_slot_index_raw();
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        if has_raw {
                            if matches(&edge) && !window.emit_edge(edge, visit) {
                                return Ok(());
                            }
                        } else if matches(&edge) && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    }
                } else {
                    for edge in self.edges.out_edges_iter(&acc, VertexId::from(0))? {
                        let slot_index = edge.edge_slot_index_raw();
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        let passes = if let Some(raw_m) = raw_matches.as_mut() {
                            let mut buf = vec![0u8; E::BYTES];
                            edge.write_to(&mut buf);
                            raw_m(&buf) && matches(&edge)
                        } else {
                            matches(&edge)
                        };
                        if passes && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    }
                }
            }
        } else {
            for bucket_index in 0..buckets.len() as u32 {
                let bucket = &buckets[bucket_index as usize];
                if bucket.degree() == 0 {
                    continue;
                }
                let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                let successor_start =
                    self.bucket_successor_start_after_bucket(&vertex, bucket_index, bucket)?;
                let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src);
                if bucket.overflow_log_head() < 0 {
                    for slot_idx in 0..bucket.degree() {
                        let at = checked_add_slot_index(bucket.edge_start(), u64::from(slot_idx))
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        if self.edges.read_slot(at).is_deleted_slot() {
                            continue;
                        }
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_idx,
                            self.edges
                                .read_slot(at)
                                .with_slot_index(slot_idx)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        let passes = if let Some(raw_m) = raw_matches.as_mut() {
                            let mut buf = vec![0u8; E::BYTES];
                            edge.write_to(&mut buf);
                            raw_m(&buf) && matches(&edge)
                        } else {
                            matches(&edge)
                        };
                        if passes && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    }
                } else {
                    for edge in self.edges.asc_out_edges(&acc, VertexId::from(0))? {
                        let slot_index = edge.edge_slot_index_raw();
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        let passes = if let Some(raw_m) = raw_matches.as_mut() {
                            let mut buf = vec![0u8; E::BYTES];
                            edge.write_to(&mut buf);
                            raw_m(&buf) && matches(&edge)
                        } else {
                            matches(&edge)
                        };
                        if passes && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn labeled_out_edges_iter(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
        directedness: Option<BucketDirectedness>,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.degree() == 0 {
            return Ok(LabeledOutEdgesIter::empty(self, src, order));
        }
        if vertex.is_default_edge_labeled() {
            if let Some(directedness) = directedness {
                if self.bypass_storage_label_for(&vertex).directedness() != directedness {
                    return Ok(LabeledOutEdgesIter::empty(self, src, order));
                }
            }
            return match order {
                OutEdgeOrder::Descending => Ok(LabeledOutEdgesIter {
                    graph: self,
                    src,
                    order,
                    kind: LabeledOutEdgesIterKind::BypassDesc {
                        label_id: self.bypass_storage_label_for(&vertex),
                        iter: OutEdgeSlabIter::try_new(
                            &self.edges,
                            vertex.base_slot_start(),
                            vertex.stored_degree(),
                            vertex.degree(),
                        )?,
                    },
                }),
                OutEdgeOrder::Ascending => Ok(LabeledOutEdgesIter {
                    graph: self,
                    src,
                    order,
                    kind: LabeledOutEdgesIterKind::BypassAsc {
                        label_id: self.bypass_storage_label_for(&vertex),
                        iter: self.edges.asc_out_edges_iter(&self.vertices, src)?,
                    },
                }),
            };
        }

        let (base_bucket_index, buckets) = if let Some(directedness) = directedness {
            let strategy = Self::directedness_partition_strategy(directedness, order.ascending());
            let (lo, hi) = self.buckets.directedness_bucket_index_range(
                vertex.base_slot_start(),
                vertex.degree(),
                directedness,
                strategy,
            )?;
            if lo >= hi {
                return Ok(LabeledOutEdgesIter::empty(self, src, order));
            }
            (lo, self.read_vertex_label_buckets_range(&vertex, lo, hi)?)
        } else {
            (0, self.read_vertex_label_buckets(&vertex)?)
        };
        let next_bucket = match order {
            OutEdgeOrder::Descending => buckets.len().checked_sub(1),
            OutEdgeOrder::Ascending => (!buckets.is_empty()).then_some(0),
        };
        Ok(LabeledOutEdgesIter {
            graph: self,
            src,
            order,
            kind: LabeledOutEdgesIterKind::Buckets {
                vertex,
                buckets,
                base_bucket_index,
                next_bucket,
                current: LabeledSpanIter::Empty,
            },
        })
    }
    pub(super) fn labeled_bucket_span_iter<'a>(
        &'a self,
        src: VertexId,
        order: OutEdgeOrder,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
        local_bucket_index: usize,
        bucket_index: u32,
    ) -> Result<LabeledSpanIter<'a, E, M>, LabeledOperationError> {
        let bucket = buckets[local_bucket_index];
        if bucket.degree() == 0 {
            return Ok(LabeledSpanIter::Empty);
        }
        let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
        let successor_start =
            self.bucket_successor_start_after_bucket(vertex, bucket_index, &bucket)?;
        let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src);
        let log_chains = self.bucket_value_log_chains_opt(src, &bucket);
        match order {
            OutEdgeOrder::Descending => Ok(LabeledSpanIter::Desc {
                graph: self,
                src,
                vertex: *vertex,
                bucket_index,
                bucket,
                label_id: bucket.bucket_label_key(),
                log_chains,
                iter: self.edges.out_edges_iter(&acc, VertexId::from(0))?,
            }),
            OutEdgeOrder::Ascending => Ok(LabeledSpanIter::Asc {
                graph: self,
                src,
                vertex: *vertex,
                bucket_index,
                bucket,
                label_id: bucket.bucket_label_key(),
                log_chains,
                iter: self.edges.asc_out_edges_iter(&acc, VertexId::from(0))?,
            }),
        }
    }

    /// Visits outgoing edges for one label as batches with parallel flattened value bytes.
    pub fn visit_out_edge_value_batches_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeValueBatchScratch<E>,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: for<'b> FnMut(LabeledEdgeValueBatch<'b, E>),
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        let (slot, bucket) = match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => (slot, bucket),
            BucketSearch::Missing { .. } => return Ok(()),
        };
        if bucket.degree() == 0 || bucket.value_byte_width() == 0 {
            return Ok(());
        }
        let dense = bucket.value_log_head() < 0
            && bucket.overflow_log_head() < 0
            && bucket.stored_slots == bucket.degree();
        if dense {
            return self
                .visit_dense_out_edge_value_batches_for_bucket(bucket, order, scratch, &mut visit);
        }

        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        let mut iter =
            self.labeled_bucket_span_iter(src, order, &vertex, &[bucket], 0, bucket_index)?;
        let width = usize::from(bucket.value_byte_width());
        let batch_edges = (EDGE_VALUE_BATCH_TARGET_BYTES / width).max(1);
        loop {
            scratch.clear();
            scratch.edges.reserve(batch_edges);
            scratch.value_bytes.reserve(batch_edges * width);
            for _ in 0..batch_edges {
                let Some(edge) = iter.next() else {
                    break;
                };
                scratch
                    .value_bytes
                    .extend_from_slice(edge.edge_value_bytes());
                scratch.edges.push(edge);
            }
            if scratch.edges.is_empty() {
                return Ok(());
            }
            visit(LabeledEdgeValueBatch {
                label_id,
                byte_width: bucket.value_byte_width(),
                order,
                edges: &scratch.edges,
                value_bytes: &scratch.value_bytes,
                dense: false,
            });
        }
    }

    fn visit_dense_out_edge_value_batches_for_bucket<Visit>(
        &self,
        bucket: LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeValueBatchScratch<E>,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: for<'b> FnMut(LabeledEdgeValueBatch<'b, E>),
    {
        let width = usize::from(bucket.value_byte_width());
        let batch_edges = (EDGE_VALUE_BATCH_TARGET_BYTES / width).max(1);
        let degree = bucket.degree();
        let mut remaining = degree;
        while remaining > 0 {
            let take = remaining.min(batch_edges as u32);
            let first_slot = match order {
                OutEdgeOrder::Descending => remaining - take,
                OutEdgeOrder::Ascending => degree - remaining,
            };
            scratch.clear();
            scratch.edges.reserve(take as usize);
            scratch.value_bytes.reserve(take as usize * width);

            let mut raw_edges = vec![0u8; take as usize * E::BYTES];
            self.edges
                .read_slots_contiguous(bucket.edge_start() + u64::from(first_slot), &mut raw_edges);
            let value_offset = bucket
                .value_offset()
                .checked_add(u64::from(first_slot) * u64::from(bucket.value_byte_width()))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut raw_values = vec![0u8; take as usize * width];
            self.values.read_bytes(value_offset, &mut raw_values);

            match order {
                OutEdgeOrder::Ascending => {
                    for i in 0..take as usize {
                        let slot = first_slot + i as u32;
                        let edge_off = i * E::BYTES;
                        let value_off = i * width;
                        let edge = E::read_from(&raw_edges[edge_off..edge_off + E::BYTES])
                            .with_slot_index(slot)
                            .with_label_id(bucket.bucket_label_key().raw());
                        if edge.is_deleted_slot() {
                            continue;
                        }
                        scratch.edges.push(edge);
                        scratch
                            .value_bytes
                            .extend_from_slice(&raw_values[value_off..value_off + width]);
                    }
                }
                OutEdgeOrder::Descending => {
                    for i in (0..take as usize).rev() {
                        let slot = first_slot + i as u32;
                        let edge_off = i * E::BYTES;
                        let value_off = i * width;
                        let edge = E::read_from(&raw_edges[edge_off..edge_off + E::BYTES])
                            .with_slot_index(slot)
                            .with_label_id(bucket.bucket_label_key().raw());
                        if edge.is_deleted_slot() {
                            continue;
                        }
                        scratch.edges.push(edge);
                        scratch
                            .value_bytes
                            .extend_from_slice(&raw_values[value_off..value_off + width]);
                    }
                }
            }

            if !scratch.edges.is_empty() {
                visit(LabeledEdgeValueBatch {
                    label_id: bucket.bucket_label_key(),
                    byte_width: bucket.value_byte_width(),
                    order,
                    edges: &scratch.edges,
                    value_bytes: &scratch.value_bytes,
                    dense: true,
                });
            }
            remaining -= take;
        }
        Ok(())
    }
    pub fn visit_out_edges<Match, Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        mut matches: Match,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        self.visit_label_out_edges_inner(
            src,
            &vertex,
            false,
            offset,
            limit,
            raw_matches,
            &mut matches,
            &mut visit,
        )
    }
    pub fn visit_out_edges_unfiltered<Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        self.visit_out_edges(src, offset, limit, raw_matches, |_| true, visit)
    }
    pub fn visit_asc_out_edges<Match, Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        mut matches: Match,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        self.visit_label_out_edges_inner(
            src,
            &vertex,
            true,
            offset,
            limit,
            raw_matches,
            &mut matches,
            &mut visit,
        )
    }
    pub fn visit_asc_out_edges_unfiltered<Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        self.visit_asc_out_edges(src, offset, limit, raw_matches, |_| true, visit)
    }
    pub fn out_edges(&self, src: VertexId) -> Result<Vec<E>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        let mut out = Vec::new();
        self.visit_label_out_edges_inner(
            src,
            &vertex,
            false,
            None,
            None,
            None,
            &mut |_| true,
            &mut |e| out.push(e),
        )?;
        Ok(out)
    }
    pub fn asc_out_edges(&self, src: VertexId) -> Result<Vec<E>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        let mut out = Vec::new();
        self.visit_label_out_edges_inner(
            src,
            &vertex,
            true,
            None,
            None,
            None,
            &mut |_| true,
            &mut |e| out.push(e),
        )?;
        Ok(out)
    }
    pub fn desc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, OutEdgeOrder::Descending, None)
    }
    pub fn asc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, OutEdgeOrder::Ascending, None)
    }
    pub fn find_out_edge_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        mut pred: F,
    ) -> Result<Option<(E, Option<BucketLabelKey>)>, LabeledOperationError>
    where
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(None);
            }
            let label = self.bypass_storage_label_for(&vertex);
            let found = self.edges.find_first_out_edge_matching(
                &self.vertices,
                src,
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                &mut pred,
            )?;
            return Ok(found.map(|e| (e.with_label_id(label.raw()), Some(label))));
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        if let Some((base, total_edges)) =
            Self::try_contiguous_tiled_labeled_out_edges(&vertex, &buckets)
        {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_find_out_edge_with_label_tiled");
            if total_edges == 0 {
                return Ok(None);
            }
            let degree = total_edges as usize;
            let nbytes = degree
                .checked_mul(E::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut raw = vec![0u8; nbytes];
            self.edges.read_slots_contiguous(base, &mut raw);
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
                let edge = E::read_from(&raw[byte_off..byte_end]).with_slot_index(slot);
                if pred(&edge) {
                    return Ok(Some((
                        edge.with_slot_index(slot)
                            .with_label_id(bucket.bucket_label_key().raw()),
                        Some(bucket.bucket_label_key()),
                    )));
                }
                if slot == 0 {
                    bucket_rev_idx -= 1;
                    slot_rev = None;
                } else {
                    slot_rev = Some(slot - 1);
                }
            }
            return Ok(None);
        }
        for bucket_index in (0..buckets.len()).rev() {
            let bucket_index = bucket_index as u32;
            let bucket = &buckets[bucket_index as usize];
            let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
            let successor_start =
                self.bucket_successor_start_after_bucket(&vertex, bucket_index, bucket)?;
            let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src);
            if let Some(edge) = self.edges.find_first_out_edge_matching(
                &acc,
                VertexId::from(0),
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                &mut pred,
            )? {
                return Ok(Some((
                    edge.with_label_id(bucket.bucket_label_key().raw()),
                    Some(bucket.bucket_label_key()),
                )));
            }
        }
        Ok(None)
    }
    pub fn find_out_edge_slot_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        mut pred: F,
    ) -> Result<Option<(E, BucketLabelKey, u32)>, LabeledOperationError>
    where
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(None);
            }
            let label = self.bypass_storage_label_for(&vertex);
            let found =
                self.edges
                    .find_first_out_edge_slot_matching(&self.vertices, src, |edge| pred(edge))?;
            return Ok(found.map(|(slot, edge)| (edge.with_label_id(label.raw()), label, slot)));
        }

        let buckets = self.read_vertex_label_buckets(&vertex)?;
        for bucket_index in (0..buckets.len()).rev() {
            let bucket_index = bucket_index as u32;
            let mut bucket = buckets[bucket_index as usize];
            if bucket.degree() == 0 {
                continue;
            }
            if bucket.overflow_log_head() >= 0 {
                let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                bucket = self.ensure_label_bucket_folded_to_slab(
                    src,
                    bucket_index,
                    bucket_slot,
                    bucket,
                )?;
            }
            for slot_index in (0..bucket.stored_slots).rev() {
                let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let edge = self.edges.read_slot(edge_slot).with_slot_index(slot_index);
                if edge.is_deleted_slot() {
                    continue;
                }
                if pred(&edge) {
                    return Ok(Some((
                        edge.with_label_id(bucket.bucket_label_key().raw()),
                        bucket.bucket_label_key(),
                        slot_index,
                    )));
                }
            }
        }
        Ok(None)
    }
    pub fn iter_edges_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<Vec<E>, LabeledOperationError> {
        let mut out = Vec::new();
        self.for_each_edges_for_label(src, label_id, |edge| out.push(edge))?;
        Ok(out)
    }
    pub fn out_edge_bucket_index_range_for_directedness(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
    ) -> Result<(u32, u32), LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok((0, 0));
        }
        let deg = vertex.degree();
        let strategy = Self::directedness_partition_strategy(directedness, order.ascending());
        Ok(self.buckets.directedness_bucket_index_range(
            vertex.base_slot_start(),
            deg,
            directedness,
            strategy,
        )?)
    }
    pub fn iter_out_edges_by_directedness(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        self.out_edges_by_directedness_iter(src, directedness, order)
            .map(|iter| iter.collect())
    }
    pub fn out_edges_by_directedness_iter(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, order, Some(directedness))
    }
    pub fn iter_out_edges_directed_only(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        self.iter_out_edges_by_directedness(src, BucketDirectedness::Directed, order)
    }
    pub fn iter_out_edges_undirected_only(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        self.iter_out_edges_by_directedness(src, BucketDirectedness::Undirected, order)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use crate::VertexId;
    use std::num::NonZero;

    #[test]
    fn out_edges_iterator_streams_desc_order() {
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

        let expected = graph.out_edges(VertexId::from(0)).unwrap();
        let lazy: Vec<_> = graph
            .desc_out_edges_iter(VertexId::from(0))
            .unwrap()
            .collect();
        assert_eq!(lazy, expected);
    }
    #[test]
    fn labeled_desc_and_asc_out_edges_iters_match_materialized_rows() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();

        let desc = graph.out_edges(VertexId::from(0)).unwrap();
        let asc = graph.asc_out_edges(VertexId::from(0)).unwrap();
        assert_eq!(
            graph
                .desc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            desc
        );
        assert_eq!(
            graph
                .asc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            asc
        );
    }
    #[test]
    fn labeled_out_edges_iter_advance_by_and_nth_match_scan() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();

        let full: Vec<_> = graph
            .desc_out_edges_iter(VertexId::from(0))
            .unwrap()
            .collect();
        assert_eq!(full.len(), 2);

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.advance_by(0), Ok(()));
        assert_eq!(it.next(), Some(full[0]));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.advance_by(1), Ok(()));
        assert_eq!(it.next(), Some(full[1]));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.advance_by(2), Ok(()));
        assert_eq!(it.next(), None);

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.advance_by(3), Err(NonZero::new(1).unwrap()));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.nth(0), Some(full[0]));
        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.nth(1), Some(full[1]));
        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.nth(2), None);
    }
    #[test]
    fn out_edges_by_directedness_filters_and_orders() {
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

        assert_eq!(
            graph
                .iter_out_edges_by_directedness(
                    VertexId::from(0),
                    BucketDirectedness::Directed,
                    OutEdgeOrder::Descending,
                )
                .unwrap(),
            vec![TestEdge { target: 40 }, TestEdge { target: 10 }]
        );
        assert_eq!(
            graph
                .iter_out_edges_by_directedness(
                    VertexId::from(0),
                    BucketDirectedness::Directed,
                    OutEdgeOrder::Ascending,
                )
                .unwrap(),
            vec![TestEdge { target: 10 }, TestEdge { target: 40 }]
        );
        assert_eq!(
            graph
                .iter_out_edges_undirected_only(VertexId::from(0), OutEdgeOrder::Descending)
                .unwrap(),
            vec![TestEdge { target: 30 }]
        );
        assert_eq!(
            graph
                .iter_out_edges_undirected_only(VertexId::from(0), OutEdgeOrder::Ascending)
                .unwrap(),
            vec![TestEdge { target: 30 }]
        );
    }
    #[test]
    fn normal_labeled_edges_update_pma_leaf_segment_counts() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(2),
                TestEdge { target: 10 },
            )
            .unwrap();

        let header = graph.edges().header();
        let first_leaf = graph
            .edges()
            .counts_store()
            .get(u64::from(header.segment_count));
        assert_eq!(first_leaf.actual, 1);
        assert!(first_leaf.total > 0);
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }
    #[test]
    fn labeled_dense_leaf_triggers_slack_growth_cascade() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        let mut alloc = graph.vertices().get(VertexId::from(0)).stored_slots;
        let mut grew = false;
        for target in 0..512u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge { target })
                .unwrap();
            let next = graph.vertices().get(VertexId::from(0)).stored_slots;
            if next > alloc {
                grew = true;
                break;
            }
            alloc = next;
        }
        assert!(
            grew,
            "expected dense-leaf cascade to expand VertexEdgeSpan reservation"
        );
    }
    #[test]
    fn unchecked_label_iteration_matches_checked_for_valid_vertices() {
        let graph = test_graph();
        let bypass_tail = graph.push_vertex(LabeledVertex::default()).unwrap();
        let catalog_tail = graph.push_vertex(LabeledVertex::default()).unwrap();

        let road = BucketLabelKey::from_raw(2);
        let walk = BucketLabelKey::from_raw(3);
        for target in [10, 11] {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        graph
            .insert_edge(VertexId::from(0), walk, TestEdge { target: 20 })
            .unwrap();

        for target in [100, 101] {
            graph
                .insert_edge(bypass_tail, graph.default_label(), TestEdge { target })
                .unwrap();
        }

        let catalog = BucketLabelKey::from_raw(42);
        for target in [200, 201] {
            graph
                .insert_edge(catalog_tail, catalog, TestEdge { target })
                .unwrap();
        }

        for (src, label) in [
            (VertexId::from(0), road),
            (VertexId::from(0), walk),
            (VertexId::from(0), BucketLabelKey::from_raw(999)),
            (bypass_tail, graph.default_label()),
            (bypass_tail, road),
            (catalog_tail, catalog),
            (catalog_tail, graph.default_label()),
        ] {
            let mut checked = Vec::new();
            graph
                .for_each_edges_for_label(src, label, |edge| checked.push(edge))
                .unwrap();

            let mut unchecked = Vec::new();
            graph
                .for_each_edges_for_label_unchecked(src, label, |edge| unchecked.push(edge))
                .unwrap();

            assert_eq!(unchecked, checked, "src={src:?} label={label:?}");
        }
    }
}
