//! Labeled graph `traverse` implementation.

use crate::{
    VertexId,
    labeled::{
        bucket_label_key::{BucketDirectedness, BucketLabelKey},
        record::{LabelBucket, LabeledVertex},
    },
    lara::{edge::OutEdgeVisitWindow, operation_error::LaraOperationError},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::ops::ControlFlow;

use super::error::LabeledOperationError;
use super::iter::{
    LabeledEdgeInlinePropertyBatch, LabeledEdgeInlinePropertyBatchScratch,
    LabeledInlinePropertyValueBatch, LabeledInlinePropertyValueBatchScratch,
};
use super::{BucketSearch, LabeledLaraGraph, LabeledOutEdgesIter, OutEdgeOrder};

fn emit_inline_property_batch<'a, Visit>(
    scratch: &'a LabeledInlinePropertyValueBatchScratch,
    visit: &mut Visit,
    label_id: BucketLabelKey,
    byte_width: u16,
    order: OutEdgeOrder,
    dense: bool,
) where
    Visit: for<'b> FnMut(LabeledInlinePropertyValueBatch<'b>),
{
    if scratch.slot_indices.is_empty() {
        return;
    }
    visit(LabeledInlinePropertyValueBatch {
        label_id,
        byte_width,
        order,
        slot_indices: &scratch.slot_indices,
        values: &scratch.values,
        dense,
    });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ContiguousBucketRun {
    base: u64,
    total_edges: u32,
}

impl ContiguousBucketRun {
    fn new(base: u64, total_edges: u32) -> Self {
        Self { base, total_edges }
    }

    pub(super) fn base(self) -> u64 {
        self.base
    }

    pub(super) fn total_edges(self) -> u32 {
        self.total_edges
    }

    pub(super) fn byte_len<E: CsrEdge>(self) -> Result<usize, LabeledOperationError> {
        (self.total_edges as usize)
            .checked_mul(E::BYTES)
            .ok_or_else(|| LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn edge_chunk<'a, E: CsrEdge>(
        self,
        raw: &'a [u8],
        bucket: &LabelBucket,
        slot: u32,
    ) -> Result<&'a [u8], LabeledOperationError> {
        let rel = bucket
            .edge_start()
            .saturating_sub(self.base)
            .checked_add(u64::from(slot))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let byte_off = usize::try_from(rel)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let byte_end = byte_off
            .checked_add(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        raw.get(byte_off..byte_end)
            .ok_or_else(|| LaraOperationError::CollectAllocationOverflow.into())
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn try_contiguous_tiled_labeled_out_edges_slice(
        buckets: &[LabelBucket],
        span_end_exclusive: u64,
    ) -> Option<ContiguousBucketRun> {
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
        Some(ContiguousBucketRun::new(base, total_edges))
    }

    pub(super) fn try_contiguous_tiled_labeled_out_edges(
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Option<ContiguousBucketRun> {
        let deg = vertex.degree() as usize;
        if deg == 0 || buckets.len() != deg {
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
        let span_end = crate::labeled::slot_index::checked_add_slot_index(
            base,
            u64::from(vertex.stored_slots),
        )?;
        if pos > span_end {
            return None;
        }
        Some(ContiguousBucketRun::new(base, total_edges))
    }

    /// Visits outgoing edges for one label in descending scan order.
    pub fn for_each_edges_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.for_each_edges_for_label_ordered(src, label_id, OutEdgeOrder::Descending, visit)
    }

    /// Visits outgoing edges for one label in the requested order.
    pub fn for_each_edges_for_label_ordered<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.visit_edges_for_label(src, label_id, order, &mut visit)
    }

    /// Like [`Self::for_each_edges_for_label_ordered`], but skips edge-inline-property-bytes reads.
    pub fn for_each_edges_for_label_topology_ordered<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        let _ = self.visit_edges(src, label_id, order, |_slot, edge| {
            visit(edge.with_label_id(label_id.raw()));
            ControlFlow::<()>::Continue(())
        })?;
        Ok(())
    }
}
impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    /// Visits outgoing edges for one label without checking that `src` is in range.
    pub fn for_each_edges_for_label_unchecked<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_unchecked_bypass_slab");
        self.visit_edges_for_label_unchecked(src, label_id, OutEdgeOrder::Descending, &mut visit)?;
        Ok(())
    }

    pub(super) fn visit_label_out_edges_inner<Match, Visit>(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
        ascending: bool,
        offset: Option<usize>,
        limit: Option<usize>,
        mut raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: &mut Match,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        let mut window = OutEdgeVisitWindow::new(offset, limit);
        let order = if ascending {
            OutEdgeOrder::Ascending
        } else {
            OutEdgeOrder::Descending
        };
        let _ = self.visit_all_labels_with_inline_property(src, order, |edge| {
            let passes = if let Some(raw_m) = raw_matches.as_mut() {
                let mut buf = vec![0u8; E::BYTES];
                edge.write_to(&mut buf);
                raw_m(&buf) && matches(&edge)
            } else {
                matches(&edge)
            };
            if passes && !window.emit_edge(edge, visit) {
                return ControlFlow::<()>::Break(());
            }
            ControlFlow::<()>::Continue(())
        })?;
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
            let label = self.bypass_storage_label_for(&vertex);
            if let Some(directedness) = directedness
                && label.directedness() != directedness
            {
                return Ok(LabeledOutEdgesIter::empty(self, src, order));
            }
            // Bypass vertices use a single synthetic bucket so the iterator delegates
            // to traverse_next on first `next()`.
            let bucket = LabelBucket::from_parts(
                label,
                vertex.base_slot_start(),
                vertex.degree(),
                vertex.stored_degree(),
                -1,
            );
            return Ok(LabeledOutEdgesIter::from_buckets(
                self,
                src,
                order,
                0,
                vec![bucket],
            ));
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
        Ok(LabeledOutEdgesIter::from_buckets(
            self,
            src,
            order,
            base_bucket_index,
            buckets,
        ))
    }

    /// Visits outgoing edges and their inline-property bytes for one label in batches.
    pub fn visit_out_edge_inline_property_batches_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<E>,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledEdgeInlinePropertyBatch<'b, E>),
    {
        let result = self.visit_out_edge_inline_property_batches_for_label_next(
            src,
            label_id,
            order,
            scratch,
            |batch| {
                visit(batch);
                ControlFlow::<()>::Continue(())
            },
        )?;
        assert!(
            result.is_continue(),
            "visit_out_edge_inline_property_batches_for_label closure cannot break"
        );
        Ok(())
    }

    /// Returns whether `(src, label_id)` is eligible for dense inline-property-bytes-only batch traversal.
    ///
    /// Hybrid and sparse overflow buckets return `false`; predicate expand should use the
    /// combined edge + inline property bytes batch path without probing phase 1 first.
    pub fn out_bucket_dense_inline_property_batch_eligible(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<bool, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(false);
        }
        let bucket = match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { bucket, .. } => bucket,
            BucketSearch::Missing { .. } => return Ok(false),
        };
        Ok(super::super::invariants::bucket_dense_inline_property_batch_eligible(&bucket))
    }

    /// Returns whether predicate expand may use phase 1 (inline property values) + phase 2 (topology).
    pub fn out_bucket_inline_property_bytes_first_predicate_eligible(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<bool, LabeledOperationError> {
        if self.out_bucket_dense_inline_property_batch_eligible(src, label_id)? {
            return Ok(true);
        }
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(false);
        }
        let bucket = match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { bucket, .. } => bucket,
            BucketSearch::Missing { .. } => return Ok(false),
        };
        Ok(bucket.degree() > 0
            && bucket.inline_property_byte_width() > 0
            && bucket.overflow_log_head() >= 0)
    }

    /// Visits outgoing inline property bytes for one label as batches without materializing edge rows.
    ///
    /// Dense buckets bulk-read the inline property bytes slab; hybrid buckets combine slab bulk reads with
    /// per-log-entry inline property bytes resolution; sparse buckets walk the span iterator and emit slot
    /// indices with attached inline_property_bytes bytes only.
    pub fn visit_out_inline_property_batches_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledInlinePropertyValueBatch<'b>),
    {
        let result = self.visit_out_inline_property_batches_for_label_next(
            src,
            label_id,
            order,
            scratch,
            |batch| {
                visit(batch);
                ControlFlow::<()>::Continue(())
            },
        )?;
        assert!(
            result.is_continue(),
            "visit_out_inline_property_batches_for_label closure cannot break"
        );
        Ok(())
    }

    /// Visits matching outgoing edges in descending scan order with optional offset and limit.
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
        E: CsrEdgeTombstone,
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

    /// Visits all outgoing edges in descending scan order with optional offset and limit.
    pub fn visit_out_edges_unfiltered<Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.visit_out_edges(src, offset, limit, raw_matches, |_| true, visit)
    }

    /// Visits matching outgoing edges in ascending slot order with optional offset and limit.
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
        E: CsrEdgeTombstone,
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

    /// Visits all outgoing edges in ascending slot order with optional offset and limit.
    pub fn visit_asc_out_edges_unfiltered<Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.visit_asc_out_edges(src, offset, limit, raw_matches, |_| true, visit)
    }

    /// Collects outgoing edges for `src` in descending scan order.
    pub fn out_edges(&self, src: VertexId) -> Result<Vec<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
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

    /// Collects outgoing edges for `src` in ascending slot order.
    pub fn asc_out_edges(&self, src: VertexId) -> Result<Vec<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
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

    /// Returns an iterator over outgoing edges in descending scan order.
    pub fn desc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, OutEdgeOrder::Descending, None)
    }

    /// Returns an iterator over outgoing edges in ascending slot order.
    pub fn asc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, OutEdgeOrder::Ascending, None)
    }

    /// Finds the first outgoing edge matching `pred`, returning its label when available.
    pub fn find_out_edge_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        pred: F,
    ) -> Result<Option<(E, Option<BucketLabelKey>)>, LabeledOperationError>
    where
        F: FnMut(&E) -> bool,
    {
        Ok(self
            .find_nth_edge_with_inline_property_matching(
                src,
                super::traverse_next::EdgeFindScope::AllLabels,
                OutEdgeOrder::Descending,
                0,
                pred,
            )?
            .map(|found| (found.edge, Some(found.label))))
    }

    /// Finds the first outgoing edge matching `pred`, returning its label and bucket slot index.
    pub fn find_out_edge_slot_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        pred: F,
    ) -> Result<Option<(E, BucketLabelKey, u32)>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        Ok(self
            .find_nth_edge_with_inline_property_matching(
                src,
                super::traverse_next::EdgeFindScope::AllLabels,
                OutEdgeOrder::Descending,
                0,
                pred,
            )?
            .map(|found| (found.edge, found.label, found.slot.raw())))
    }

    /// Collects outgoing edges for one label.
    pub fn iter_edges_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<Vec<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.iter_edges_with_inline_property_for_label_next(src, label_id, OutEdgeOrder::Descending)
    }

    /// Returns the bucket-index range that stores edges with `directedness`.
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

    /// Collects outgoing edges whose bucket directedness matches `directedness`.
    pub fn iter_out_edges_by_directedness(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        let mut out = Vec::new();
        let _ = self.visit_out_edges_by_directedness(src, directedness, order, |edge| {
            out.push(edge);
            ControlFlow::<()>::Continue(())
        })?;
        Ok(out)
    }

    /// Returns an iterator over outgoing edges whose bucket directedness matches `directedness`.
    pub fn out_edges_by_directedness_iter(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, order, Some(directedness))
    }

    /// Collects directed outgoing edges.
    pub fn iter_out_edges_directed_only(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        self.iter_out_edges_by_directedness(src, BucketDirectedness::Directed, order)
    }

    /// Collects undirected outgoing edges.
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
    use super::super::{LEAF_VERTEX_EDGE_SEGMENT_DENSITY, *};
    use crate::VertexId;
    use std::num::NonZero;
    use std::ops::ControlFlow;

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
            .collect::<Result<_, _>>()
            .unwrap();
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
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            desc
        );
        assert_eq!(
            graph
                .asc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
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
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(full.len(), 2);

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.try_advance_by(0).unwrap(), Ok(()));
        assert_eq!(it.next().transpose().unwrap(), Some(full[0]));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.try_advance_by(1).unwrap(), Ok(()));
        assert_eq!(it.next().transpose().unwrap(), Some(full[1]));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.try_advance_by(2).unwrap(), Ok(()));
        assert_eq!(it.next().transpose().unwrap(), None);

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.try_advance_by(3).unwrap(), Err(NonZero::new(1).unwrap()));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.next().transpose().unwrap(), Some(full[0]));
        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.nth(1).transpose().unwrap(), Some(full[1]));
        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.nth(2).transpose().unwrap(), None);
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
    fn labeled_dense_leaf_triggers_leaf_rebalance() {
        use super::super::leaf_pin::labeled_leaf_physical_block_len;

        let graph = test_graph();
        let vid = VertexId::from(0);
        graph
            .insert_edge(vid, BucketLabelKey::from_raw(99), TestEdge { target: 999 })
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        let header = graph.edges().header();
        let block_len = labeled_leaf_physical_block_len(header.segment_size);
        for target in 0..block_len {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    TestEdge {
                        target: target as u32,
                    },
                )
                .unwrap();
        }
        graph.rebalance_cascade_after_labeled_mutation(vid).unwrap();
        let counts = graph.leaf_segment_counts_for_vid(vid);
        assert!(counts.total as u64 >= block_len);
        assert!(counts.actual > 0);
        assert!(
            graph.labeled_leaf_pma_density(vid) < LEAF_VERTEX_EDGE_SEGMENT_DENSITY
                || counts.total as u64 > block_len,
            "dense leaf maintenance should slide or grow the pinned PMA block"
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

    #[test]
    fn labeled_scan_never_reads_span_meta() {
        use crate::lara::edge::scan_guard::ScanPathGuard;

        let (graph, hub, _) = build_mixed_label_hub(8, 25);
        let _guard = ScanPathGuard::enter();
        exercise_labeled_hub_scan_paths(&graph, hub);
        assert_eq!(ScanPathGuard::span_meta_reads(), 0);
    }

    #[test]
    fn labeled_scan_never_reads_free_span_store() {
        use crate::lara::edge::scan_guard::ScanPathGuard;

        let (graph, hub, _) = build_mixed_label_hub(8, 25);
        let _guard = ScanPathGuard::enter();
        exercise_labeled_hub_scan_paths(&graph, hub);
        assert_eq!(ScanPathGuard::free_span_reads(), 0);
    }

    #[test]
    fn labeled_hub_materialized_matches_all_scan_iters() {
        let (graph, hub, _) = build_mixed_label_hub(6, 30);
        let materialized = materialized_labeled_edges(&graph, hub);
        exercise_labeled_hub_scan_paths(&graph, hub);
        for (label, expected_targets) in &materialized {
            let asc = graph
                .iter_edges_for_label(hub, *label)
                .unwrap()
                .into_iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>();
            assert_eq!(&asc, expected_targets, "label {label:?}");
        }
        let total: usize = materialized.iter().map(|(_, targets)| targets.len()).sum();
        assert_eq!(graph.asc_out_edges(hub).unwrap().len(), total);
        assert_eq!(
            graph
                .asc_out_edges_iter(hub)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .len(),
            total
        );
    }

    /// A hybrid-overflow replay built for a **different vertex that shares the same inline_property_bytes-log
    /// leaf** (`leaf = src / segment_size`) must be rejected so phase 2 falls back to the sparse
    /// path. Reproduced with two real vertices in one leaf (not by mutating replay fields), since
    /// `leaf` + `label_id` + `slab_slots` alone cannot tell same-leaf vertices apart — only `src`
    /// can. Guards `read_out_edge_slots_for_label_with_replay`.
    #[test]
    fn hybrid_replay_from_other_vertex_in_same_leaf_falls_back_to_sparse() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let a = graph.push_vertex(LabeledVertex::default()).unwrap();
        let b = graph.push_vertex(LabeledVertex::default()).unwrap();
        assert_eq!(
            graph.inline_property_bytes_log_leaf(a),
            graph.inline_property_bytes_log_leaf(b),
            "test requires two vertices sharing one inline-property-bytes-log leaf"
        );
        let road = BucketLabelKey::from_raw(2);
        for v in [a, b] {
            graph
                .ensure_label_bucket_inline_property_byte_width(v, road, 2u16)
                .unwrap();
        }
        // Identically-shaped hybrid overflow buckets, but disjoint target ranges so the two
        // replays decode different overflow-log edges (A: 1..=48, B: 1001..=1048).
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    a,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
            let bt = 1000 + target;
            graph
                .insert_edge_skip_leaf_cascade(
                    b,
                    road,
                    InlinePropertyTestEdge::with_bytes(bt, &(bt as u16).to_le_bytes()),
                )
                .unwrap();
        }

        let bucket_of = |v: VertexId| {
            let vertex = graph.vertices().get(v);
            let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
            graph.buckets().read_label_bucket_slot(slot).unwrap()
        };
        let bucket_a = bucket_of(a);
        let bucket_b = bucket_of(b);
        assert!(bucket_a.overflow_log_head() >= 0 && bucket_b.overflow_log_head() >= 0);
        // Same leaf, same label, same slab split: only `src` distinguishes the two replays, so
        // without the `src` check B's replay would be wrongly adopted for A.
        assert_eq!(
            graph.bucket_slab_prefix_slots(a, &bucket_a),
            graph.bucket_slab_prefix_slots(b, &bucket_b),
        );

        // Phase 1 on A captures A's slot order; phase 1 on B populates a replay owned by B.
        let mut scratch_a = crate::labeled::LabeledInlinePropertyValueBatchScratch::default();
        let mut slots_a = Vec::new();
        graph
            .visit_out_inline_property_batches_for_label(
                a,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch_a,
                |batch| slots_a.extend_from_slice(batch.slot_indices),
            )
            .unwrap();
        let mut scratch_b = crate::labeled::LabeledInlinePropertyValueBatchScratch::default();
        graph
            .visit_out_inline_property_batches_for_label(
                b,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch_b,
                |_| {},
            )
            .unwrap();
        assert!(scratch_b.hybrid_overflow_replay.is_active());
        assert!(
            !scratch_b
                .hybrid_overflow_replay
                .log_indices_by_slot
                .is_empty()
        );

        let read_a = |replay: Option<&crate::labeled::HybridOverflowEdgeReplay>| {
            let mut targets = Vec::new();
            let positions_slots_a: Vec<_> = slots_a
                .iter()
                .copied()
                .map(crate::traverse::BucketEntryPosition::new)
                .collect();
            graph
                .visit_edges_at_with_replay(
                    a,
                    road,
                    &positions_slots_a,
                    OutEdgeOrder::Ascending,
                    replay,
                    |_slot, edge| {
                        targets.push(edge.target);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .unwrap();
            targets
        };
        let expected = read_a(None);
        assert_eq!(expected.len(), 48);
        assert!(
            expected.iter().all(|&t| (1..=48).contains(&t)),
            "A only owns targets 1..=48"
        );

        // A's own replay reproduces the ground truth.
        assert_eq!(read_a(Some(&scratch_a.hybrid_overflow_replay)), expected);
        // B's replay (same leaf/label/slab split, different vertex) must be rejected → sparse path.
        assert_eq!(
            read_a(Some(&scratch_b.hybrid_overflow_replay)),
            expected,
            "a replay owned by another vertex in the same leaf must not be reused"
        );
    }

    #[test]
    fn hybrid_inline_property_bytes_first_keeps_replay_with_tombstone_free_slab_prefix() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, road, 2)
            .unwrap();
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        graph
            .rebalance_edge_log_leaf_for_labeled(src, true, true)
            .unwrap();
        for target in 49..=64u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(src);
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.stored_slots > 0);
        assert!(bucket.overflow_log_head() >= 0);
        assert_eq!(
            graph.bucket_reserved_edge_slots(src, &bucket),
            bucket.degree()
        );

        let mut scratch = crate::labeled::LabeledInlinePropertyValueBatchScratch::default();
        let mut observed = Vec::new();
        graph
            .visit_out_inline_property_batches_for_label(
                src,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    observed.extend(
                        batch
                            .values
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|bytes| u16::from_le_bytes(*bytes)),
                    );
                },
            )
            .unwrap();

        assert_eq!(observed, (1..=64u16).collect::<Vec<_>>());
        assert!(scratch.hybrid_overflow_replay.is_active());
    }

    /// Phase-2 reuse of a matching replay must skip the overflow-log chain rebuild that the sparse
    /// fallback performs. Validates the `overflow_chain_rebuilds` instrumentation (used by the
    /// executor incoming/outgoing replay-reuse tests) distinguishes the two phase-2 paths.
    #[test]
    fn phase2_replay_reuse_avoids_overflow_chain_rebuild() {
        use crate::lara::edge::scan_guard::ScanPathGuard;

        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, road, 2u16)
            .unwrap();
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(src);
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);

        let mut scratch = crate::labeled::LabeledInlinePropertyValueBatchScratch::default();
        let mut slots = Vec::new();
        graph
            .visit_out_inline_property_batches_for_label(
                src,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| slots.extend_from_slice(batch.slot_indices),
            )
            .unwrap();
        assert!(scratch.hybrid_overflow_replay.is_active());

        let read = |replay: Option<&crate::labeled::HybridOverflowEdgeReplay>| {
            let mut targets = Vec::new();
            let positions_slots: Vec<_> = slots
                .iter()
                .copied()
                .map(crate::traverse::BucketEntryPosition::new)
                .collect();
            graph
                .visit_edges_at_with_replay(
                    src,
                    road,
                    &positions_slots,
                    OutEdgeOrder::Ascending,
                    replay,
                    |_slot, edge| {
                        targets.push(edge.target);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .unwrap();
            targets
        };

        let (with_replay, rebuilds_with_replay) = {
            let _guard = ScanPathGuard::enter();
            let targets = read(Some(&scratch.hybrid_overflow_replay));
            (targets, ScanPathGuard::overflow_chain_rebuilds())
        };
        let (without_replay, rebuilds_without_replay) = {
            let _guard = ScanPathGuard::enter();
            let targets = read(None);
            (targets, ScanPathGuard::overflow_chain_rebuilds())
        };

        assert_eq!(with_replay, without_replay);
        assert_eq!(
            rebuilds_with_replay, 0,
            "a reused replay must not rebuild the overflow-log chain"
        );
        assert!(
            rebuilds_without_replay >= 1,
            "the sparse fallback rebuilds the overflow-log chain"
        );
    }

    /// `phase 1 → delete an overflow edge → phase 2` must fall back to sparse. Removing an
    /// overflow-log edge tombstones the log entry in place: `src`, `label_id`, and the slab/log
    /// split are all unchanged, so only the bucket snapshot (`degree`) catches the mutation. Without
    /// it, the stale cached `log_table` would still decode and return the deleted edge.
    #[test]
    fn hybrid_replay_after_overflow_delete_falls_back_to_sparse() {
        use crate::lara::edge::scan_guard::ScanPathGuard;

        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, road, 2u16)
            .unwrap();
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        let bucket_of = |v| {
            let vertex = graph.vertices().get(v);
            let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
            graph.buckets().read_label_bucket_slot(slot).unwrap()
        };
        let bucket = bucket_of(src);
        assert!(bucket.overflow_log_head() >= 0);
        let slab_prefix = graph.bucket_slab_prefix_slots(src, &bucket);

        // Phase 1: capture the replay and the slot order, then take a stale snapshot of the replay.
        let mut scratch = crate::labeled::LabeledInlinePropertyValueBatchScratch::default();
        let mut slots = Vec::new();
        graph
            .visit_out_inline_property_batches_for_label(
                src,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| slots.extend_from_slice(batch.slot_indices),
            )
            .unwrap();
        assert!(scratch.hybrid_overflow_replay.is_active());
        let stale_replay = scratch.hybrid_overflow_replay.clone();

        // Delete one overflow-log edge (first slot past the slab prefix): an in-place tombstone.
        let removed = graph
            .remove_edge_at_slot(src, road, slab_prefix)
            .unwrap()
            .expect("removed an overflow-log edge");
        let deleted_target = removed.target;

        // The slab/log split is unchanged, so the older `src`/`label`/`slab_slots` checks still
        // match — only the `degree` snapshot distinguishes the mutated bucket.
        let bucket_after = bucket_of(src);
        assert_eq!(
            graph.bucket_slab_prefix_slots(src, &bucket_after),
            stale_replay.slab_slots,
            "in-place tombstone delete leaves the slab/log split unchanged"
        );
        assert_ne!(
            bucket_after.degree(),
            stale_replay.degree,
            "the delete decrements degree, which the snapshot detects"
        );

        let read = |replay: Option<&crate::labeled::HybridOverflowEdgeReplay>| {
            let mut targets = Vec::new();
            let positions_slots: Vec<_> = slots
                .iter()
                .copied()
                .map(crate::traverse::BucketEntryPosition::new)
                .collect();
            graph
                .visit_edges_at_with_replay(
                    src,
                    road,
                    &positions_slots,
                    OutEdgeOrder::Ascending,
                    replay,
                    |_slot, edge| {
                        targets.push(edge.target);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .unwrap();
            targets
        };

        // Ground truth: the sparse path resolves canonical state and drops the deleted edge.
        let expected = read(None);
        assert_eq!(expected.len(), 47);
        assert!(!expected.contains(&deleted_target));

        // The stale replay must be rejected (snapshot mismatch) and fall back to sparse: it must not
        // resurrect the deleted edge from its cached log table.
        let (with_stale, rebuilds) = {
            let _guard = ScanPathGuard::enter();
            let targets = read(Some(&stale_replay));
            (targets, ScanPathGuard::overflow_chain_rebuilds())
        };
        assert_eq!(
            with_stale, expected,
            "a replay captured before an overflow delete must not return the deleted edge"
        );
        assert!(
            rebuilds >= 1,
            "snapshot mismatch must take the sparse fallback"
        );
    }
}
