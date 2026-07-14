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
        edge::{
            OutEdgeSlabIter, OutEdgeVisitWindow, OutEdgesIter, OutOverflowAscParts,
            OutOverflowDescParts,
        },
        operation_error::LaraOperationError,
    },
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::iter::{
    HybridOverflowEdgeReplay, LabeledEdgeInlineValueBatch, LabeledEdgeInlineValueBatchScratch,
    LabeledOutEdgesIterKind, LabeledPayloadValueBatch, LabeledPayloadValueBatchScratch,
};
use super::{BucketSearch, LabeledLaraGraph, LabeledOutEdgesIter, LabeledSpanIter, OutEdgeOrder};

const EDGE_PAYLOAD_BATCH_TARGET_BYTES: usize = 2048;

fn bucket_hybrid_slab_inline_value_batch_eligible<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    src: VertexId,
    bucket: &LabelBucket,
) -> bool
where
    E: CsrEdge,
    M: Memory,
{
    bucket.degree() > 0
        && bucket.inline_value_byte_width() > 0
        && bucket.overflow_log_head() >= 0
        && graph.bucket_slab_prefix_slots(src, bucket) > 0
}

fn slab_slot_deleted(slot: u32, deleted_slab_offsets: &[u32]) -> bool {
    deleted_slab_offsets.binary_search(&slot).is_ok()
}

fn emit_edge_inline_value_batch<'a, E, Visit>(
    scratch: &'a LabeledEdgeInlineValueBatchScratch<E>,
    visit: &mut Visit,
    label_id: BucketLabelKey,
    byte_width: u16,
    order: OutEdgeOrder,
    dense: bool,
) where
    E: CsrEdge,
    Visit: for<'b> FnMut(LabeledEdgeInlineValueBatch<'b, E>),
{
    if scratch.edges.is_empty() {
        return;
    }
    visit(LabeledEdgeInlineValueBatch {
        label_id,
        byte_width,
        order,
        edges: &scratch.edges,
        inline_value_bytes: &scratch.inline_value_bytes,
        dense,
    });
}

fn emit_inline_value_batch<'a, Visit>(
    scratch: &'a LabeledPayloadValueBatchScratch,
    visit: &mut Visit,
    label_id: BucketLabelKey,
    byte_width: u16,
    order: OutEdgeOrder,
    dense: bool,
) where
    Visit: for<'b> FnMut(LabeledPayloadValueBatch<'b>),
{
    if scratch.slot_indices.is_empty() {
        return;
    }
    visit(LabeledPayloadValueBatch {
        label_id,
        byte_width,
        order,
        slot_indices: &scratch.slot_indices,
        values: &scratch.values,
        dense,
    });
}

fn order_slot_indices(slots: &[u32], order: OutEdgeOrder) -> Vec<u32> {
    let mut ordered = slots.to_vec();
    match order {
        OutEdgeOrder::Ascending => ordered.sort_unstable(),
        OutEdgeOrder::Descending => ordered.sort_unstable_by(|a, b| b.cmp(a)),
    }
    ordered.dedup();
    ordered
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
        self.for_each_edges_for_label_ordered(src, label_id, OutEdgeOrder::Descending, visit)
    }

    /// Visits outgoing edges for one label in the requested order.
    pub fn for_each_edges_for_label_ordered<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        let mut visit = visit;
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(());
            }
            return match order {
                OutEdgeOrder::Descending => self
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
                    .map_err(Into::into),
                OutEdgeOrder::Ascending => {
                    for edge in self.edges.asc_out_edges(&self.vertices, src)? {
                        visit(edge.with_label_id(label_id.raw()));
                    }
                    Ok(())
                }
            };
        }
        let BucketSearch::Found { slot, bucket } = self.find_bucket(src, &vertex, label_id)? else {
            return Ok(());
        };
        if bucket.degree() == 0 {
            return Ok(());
        }
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_for_each_edges_for_label");
        for edge in
            self.labeled_bucket_span_iter(src, order, &vertex, &[bucket], 0, bucket_index, true)?
        {
            visit(edge?);
        }
        Ok(())
    }

    /// Like [`Self::for_each_edges_for_label_ordered`], but skips edge-inline-value reads.
    pub fn for_each_edges_for_label_topology_ordered<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        let mut visit = visit;
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(());
            }
            return match order {
                OutEdgeOrder::Descending => self
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
                    .map_err(Into::into),
                OutEdgeOrder::Ascending => {
                    for edge in self.edges.asc_out_edges(&self.vertices, src)? {
                        visit(edge.with_label_id(label_id.raw()));
                    }
                    Ok(())
                }
            };
        }
        let BucketSearch::Found { slot, bucket } = self.find_bucket(src, &vertex, label_id)? else {
            return Ok(());
        };
        if bucket.degree() == 0 {
            return Ok(());
        }
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_for_each_edges_for_label_topology");
        for edge in
            self.labeled_bucket_span_iter(src, order, &vertex, &[bucket], 0, bucket_index, false)?
        {
            visit(edge?);
        }
        Ok(())
    }

    /// Like [`Self::for_each_edges_for_label_topology_ordered`], but skips vertex range checks.
    pub fn for_each_edges_for_label_topology_unchecked<Visit>(
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
        debug_assert!(u32::from(src) < self.vertices.len());
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(());
            }
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_unchecked_bypass_slab");
            return match order {
                OutEdgeOrder::Descending => self
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
                    .map_err(Into::into),
                OutEdgeOrder::Ascending => {
                    for edge in self.edges.asc_out_edges(&self.vertices, src)? {
                        visit(edge.with_label_id(label_id.raw()));
                    }
                    Ok(())
                }
            };
        }
        let BucketSearch::Found { slot, bucket } = self.find_bucket(src, &vertex, label_id)? else {
            return Ok(());
        };
        if bucket.degree() == 0 {
            return Ok(());
        }
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_for_each_edges_for_label_topology");
        for edge in
            self.labeled_bucket_span_iter(src, order, &vertex, &[bucket], 0, bucket_index, false)?
        {
            visit(edge?);
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
                OutEdgeOrder::Descending => Ok(LabeledSpanIter::desc(
                    self,
                    src,
                    vertex,
                    0,
                    LabelBucket::default(),
                    label_id,
                    None,
                    true,
                    self.edges.out_edges_iter(&self.vertices, src)?,
                )),
                OutEdgeOrder::Ascending => Ok(LabeledSpanIter::asc(
                    self,
                    src,
                    vertex,
                    0,
                    LabelBucket::default(),
                    label_id,
                    None,
                    true,
                    self.edges.asc_out_edges_iter(&self.vertices, src)?,
                )),
            };
        }
        match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => {
                let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
                self.labeled_bucket_span_iter(src, order, &vertex, &[bucket], 0, bucket_index, true)
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
                let successor_start = self.bucket_slab_window_end_exclusive_after_bucket(
                    &vertex,
                    bucket_index,
                    &bucket,
                )?;
                self.edges
                    .out_edges_iter(
                        &LabelEdgeSpanAccess::with_bucket(
                            &self.buckets,
                            slot,
                            bucket,
                            successor_start,
                            src,
                        ),
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
        match it.try_advance_by(skip) {
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
            let edge = edge?;
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
        match it.try_advance_by(skip)? {
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
            let edge = edge?;
            match visit(edge) {
                Ok(false) => continue,
                Ok(true) => return Ok(Ok(true)),
                Err(e) => return Ok(Err(e)),
            }
        }
    }

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
            visit(edge?.with_label_id(label_id.raw()));
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
        E: CsrEdgeTombstone,
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

        let buckets = self.read_vertex_label_buckets(vertex)?;
        if let Some(run) = Self::try_contiguous_tiled_labeled_out_edges(vertex, &buckets) {
            if run.total_edges() == 0 {
                return Ok(());
            }
            let nbytes = run.byte_len::<E>()?;
            let mut raw = vec![0u8; nbytes];
            self.edges.read_slots_contiguous(run.base(), &mut raw);
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
                    let log_chains = self.bucket_payload_log_chain_opt(src, bucket);
                    let slot = slot_rev.unwrap_or(bucket.degree().saturating_sub(1));
                    let chunk = run.edge_chunk::<E>(&raw, bucket, slot)?;
                    let cont = if let Some(raw_m) = raw_matches.as_mut() {
                        let edge = self.attach_edge_inline_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot,
                            E::read_from(chunk)
                                .with_slot_index(slot)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        )?;
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
                        let edge = self.attach_edge_inline_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot,
                            E::read_from(chunk)
                                .with_slot_index(slot)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        )?;
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
                    let log_chains = self.bucket_payload_log_chain_opt(src, bucket);
                    for slot in 0..bucket.degree() {
                        let chunk = run.edge_chunk::<E>(&raw, bucket, slot)?;
                        let edge = self.attach_edge_inline_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot,
                            E::read_from(chunk)
                                .with_slot_index(slot)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        )?;
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
                let log_chains = self.bucket_payload_log_chain_opt(src, bucket);
                let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
                let successor_start = self.bucket_slab_window_end_exclusive_after_bucket(
                    vertex,
                    bucket_index,
                    bucket,
                )?;
                let acc = LabelEdgeSpanAccess::with_bucket(
                    &self.buckets,
                    slot,
                    *bucket,
                    successor_start,
                    src,
                );
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
                        let edge = self.attach_edge_inline_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        )?;
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
                        let edge = self.attach_edge_inline_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        )?;
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
                let log_chains = self.bucket_payload_log_chain_opt(src, bucket);
                let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
                let successor_start = self.bucket_slab_window_end_exclusive_after_bucket(
                    vertex,
                    bucket_index,
                    bucket,
                )?;
                let acc = LabelEdgeSpanAccess::with_bucket(
                    &self.buckets,
                    slot,
                    *bucket,
                    successor_start,
                    src,
                );
                if bucket.overflow_log_head() < 0 {
                    for slot_idx in 0..bucket.stored_slots {
                        let at = checked_add_slot_index(bucket.edge_start(), u64::from(slot_idx))
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        let edge = self.edges.read_slot(at);
                        if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                            continue;
                        }
                        let edge = self.attach_edge_inline_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_idx,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        )?;
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
                        let edge = self.attach_edge_inline_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        )?;
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
            if let Some(directedness) = directedness
                && self.bypass_storage_label_for(&vertex).directedness() != directedness
            {
                return Ok(LabeledOutEdgesIter::empty(self, src, order));
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
        attach_payload: bool,
    ) -> Result<LabeledSpanIter<'a, E, M>, LabeledOperationError> {
        let bucket = buckets[local_bucket_index];
        if bucket.degree() == 0 {
            return Ok(LabeledSpanIter::Empty);
        }
        let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
        let successor_start =
            self.bucket_slab_window_end_exclusive_after_bucket(vertex, bucket_index, &bucket)?;
        let acc =
            LabelEdgeSpanAccess::with_bucket(&self.buckets, slot, bucket, successor_start, src);
        let log_chains = if attach_payload {
            self.bucket_payload_log_chain_opt(src, &bucket)
        } else {
            None
        };
        match order {
            OutEdgeOrder::Descending => Ok(LabeledSpanIter::desc(
                self,
                src,
                *vertex,
                bucket_index,
                bucket,
                bucket.bucket_label_key(),
                log_chains,
                attach_payload,
                self.edges.out_edges_iter(&acc, VertexId::from(0))?,
            )),
            OutEdgeOrder::Ascending => Ok(LabeledSpanIter::asc(
                self,
                src,
                *vertex,
                bucket_index,
                bucket,
                bucket.bucket_label_key(),
                log_chains,
                attach_payload,
                self.edges.asc_out_edges_iter(&acc, VertexId::from(0))?,
            )),
        }
    }

    /// Visits outgoing edges for one label as batches with parallel flattened value bytes.
    pub fn visit_out_edge_inline_value_batches_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlineValueBatchScratch<E>,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledEdgeInlineValueBatch<'b, E>),
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
        if bucket.degree() == 0 || bucket.inline_value_byte_width() == 0 {
            return Ok(());
        }
        if super::super::invariants::bucket_dense_inline_value_batch_eligible(&bucket) {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_visit_dense_out_edge_inline_value_batches");
            return self.visit_dense_out_edge_inline_value_batches_for_bucket(
                bucket, order, scratch, &mut visit,
            );
        }
        if bucket_hybrid_slab_inline_value_batch_eligible(self, src, &bucket) {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_visit_hybrid_out_edge_inline_value_batches");
            let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
            return self.visit_hybrid_out_edge_inline_value_batches_for_bucket(
                src,
                &vertex,
                bucket_index,
                slot,
                bucket,
                order,
                scratch,
                &mut visit,
            );
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_visit_sparse_out_edge_inline_value_batches");
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        let mut iter =
            self.labeled_bucket_span_iter(src, order, &vertex, &[bucket], 0, bucket_index, true)?;
        let width = usize::from(bucket.inline_value_byte_width());
        let batch_edges = (EDGE_PAYLOAD_BATCH_TARGET_BYTES / width).max(1);
        loop {
            scratch.clear();
            scratch.edges.reserve(batch_edges);
            scratch.inline_value_bytes.reserve(batch_edges * width);
            for _ in 0..batch_edges {
                let Some(edge) = iter.next() else {
                    break;
                };
                let edge = edge?;
                scratch
                    .inline_value_bytes
                    .extend_from_slice(edge.edge_inline_value_bytes());
                scratch.edges.push(edge);
            }
            if scratch.edges.is_empty() {
                return Ok(());
            }
            visit(LabeledEdgeInlineValueBatch {
                label_id,
                byte_width: bucket.inline_value_byte_width(),
                order,
                edges: &scratch.edges,
                inline_value_bytes: &scratch.inline_value_bytes,
                dense: false,
            });
        }
    }

    /// Returns whether `(src, label_id)` is eligible for dense payload-only batch traversal.
    ///
    /// Hybrid and sparse overflow buckets return `false`; predicate expand should use the
    /// combined edge+payload batch path without probing phase 1 first.
    pub fn out_bucket_dense_inline_value_batch_eligible(
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
        Ok(super::super::invariants::bucket_dense_inline_value_batch_eligible(&bucket))
    }

    /// Returns whether predicate expand may use phase 1 (payload values) + phase 2 (topology).
    pub fn out_bucket_inline_value_first_predicate_eligible(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<bool, LabeledOperationError> {
        if self.out_bucket_dense_inline_value_batch_eligible(src, label_id)? {
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
            && bucket.inline_value_byte_width() > 0
            && bucket.overflow_log_head() >= 0)
    }

    /// Visits outgoing payload bytes for one label as batches without materializing edge rows.
    ///
    /// Dense buckets bulk-read the payload slab; hybrid buckets combine slab bulk reads with
    /// per-log-entry payload resolution; sparse buckets walk the span iterator and emit slot
    /// indices with attached payload bytes only.
    pub fn visit_out_inline_value_batches_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut LabeledPayloadValueBatchScratch,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledPayloadValueBatch<'b>),
    {
        // Output contract: a phase-1 call always resets the replay first, so a stale replay from a
        // previous `(src, label)` can never survive an early return (default vertex, missing bucket,
        // zero degree / payload width) and be misused by a phase-2 read.
        scratch.hybrid_overflow_replay.clear();
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        let (slot, bucket) = match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => (slot, bucket),
            BucketSearch::Missing { .. } => return Ok(()),
        };
        if bucket.degree() == 0 || bucket.inline_value_byte_width() == 0 {
            return Ok(());
        }
        if super::super::invariants::bucket_dense_inline_value_batch_eligible(&bucket) {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_visit_dense_out_inline_value_batches");
            return self.visit_dense_out_inline_value_batches_for_bucket(
                bucket, order, scratch, &mut visit,
            );
        }
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        if bucket.overflow_log_head() >= 0 {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_visit_hybrid_out_inline_value_batches");
            return self.visit_hybrid_out_inline_value_batches_for_bucket(
                src,
                &vertex,
                bucket_index,
                slot,
                bucket,
                order,
                scratch,
                &mut visit,
            );
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_visit_sparse_out_inline_value_batches");
        self.visit_sparse_out_inline_value_batches_for_bucket(
            src,
            &vertex,
            bucket_index,
            bucket,
            order,
            scratch,
            &mut visit,
        )
    }

    fn visit_dense_out_inline_value_batches_for_bucket<Visit>(
        &self,
        bucket: LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledPayloadValueBatchScratch,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: for<'b> FnMut(LabeledPayloadValueBatch<'b>),
    {
        let width = usize::from(bucket.inline_value_byte_width());
        let batch_edges = (EDGE_PAYLOAD_BATCH_TARGET_BYTES / width).max(1);
        let degree = bucket.degree();
        let label_id = bucket.bucket_label_key();
        let mut remaining = degree;
        while remaining > 0 {
            let take = remaining.min(batch_edges as u32);
            let first_slot = match order {
                OutEdgeOrder::Descending => remaining - take,
                OutEdgeOrder::Ascending => degree - remaining,
            };
            scratch.clear();
            scratch.slot_indices.reserve(take as usize);
            scratch.values.reserve(take as usize * width);

            let inline_value_offset =
                super::super::invariants::inline_value_byte_offset_at_slot(&bucket, first_slot)?;
            let mut raw_values = vec![0u8; take as usize * width];
            self.values.read_bytes(inline_value_offset, &mut raw_values);

            // Dense-eligible buckets satisfy `stored_slots == degree` with no overflow logs.
            // Tombstone deletes decrement `degree` without shrinking `stored_slots`, so they
            // fall off the dense path; phase 2 still skips deleted slots if invariants drift.
            match order {
                OutEdgeOrder::Ascending => {
                    for i in 0..take as usize {
                        let slot = first_slot + i as u32;
                        let value_off = i * width;
                        scratch.slot_indices.push(slot);
                        scratch
                            .values
                            .extend_from_slice(&raw_values[value_off..value_off + width]);
                    }
                }
                OutEdgeOrder::Descending => {
                    for i in (0..take as usize).rev() {
                        let slot = first_slot + i as u32;
                        let value_off = i * width;
                        scratch.slot_indices.push(slot);
                        scratch
                            .values
                            .extend_from_slice(&raw_values[value_off..value_off + width]);
                    }
                }
            }

            if !scratch.slot_indices.is_empty() {
                visit(LabeledPayloadValueBatch {
                    label_id,
                    byte_width: bucket.inline_value_byte_width(),
                    order,
                    slot_indices: &scratch.slot_indices,
                    values: &scratch.values,
                    dense: true,
                });
            }
            remaining -= take;
        }
        Ok(())
    }

    fn visit_dense_out_edge_inline_value_batches_for_bucket<Visit>(
        &self,
        bucket: LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlineValueBatchScratch<E>,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledEdgeInlineValueBatch<'b, E>),
    {
        self.visit_dense_out_edge_inline_value_batches_for_slab_prefix(
            bucket,
            bucket.degree(),
            &[],
            order,
            scratch,
            visit,
            true,
        )
    }

    /// Reads topology-only outgoing edge rows for the requested bucket slot indices.
    ///
    /// Dense buckets bulk-read contiguous slab spans; sparse and log-backed buckets resolve each
    /// slot individually. Deleted and out-of-range slots are skipped without error.
    pub fn read_out_edge_slots_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slots: &[u32],
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.read_out_edge_slots_for_label_with_replay(src, label_id, slots, order, None, visit)
    }

    /// Like [`Self::read_out_edge_slots_for_label`], but may reuse hybrid overflow replay cached
    /// on the payload phase-1 scratch to avoid rebuilding the overflow log chain.
    pub fn read_out_edge_slots_for_label_with_replay<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slots: &[u32],
        order: OutEdgeOrder,
        replay: Option<&HybridOverflowEdgeReplay>,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        if slots.is_empty() {
            return Ok(());
        }
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        let BucketSearch::Found { slot, bucket } = self.find_bucket(src, &vertex, label_id)? else {
            return Ok(());
        };
        if bucket.degree() == 0 {
            return Ok(());
        }
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        let visit_order = order_slot_indices(slots, order);
        if super::super::invariants::bucket_dense_inline_value_batch_eligible(&bucket) {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_read_dense_out_edge_slots");
            let mut loaded =
                self.read_dense_out_edge_topology_slots(&bucket, label_id, &visit_order)?;
            loaded.sort_unstable_by_key(|(slot, _)| *slot);
            for slot in visit_order {
                if let Ok(idx) = loaded.binary_search_by_key(&slot, |(s, _)| *s) {
                    visit(loaded[idx].1.clone());
                }
            }
            return Ok(());
        }

        // Reuse the cached phase-1 replay only when it provably belongs to this exact
        // `(src, bucket)` and the bucket has not mutated since phase 1. `src` is the owner identity:
        // `leaf = src / segment_size` is shared by many vertices, so matching on `leaf` alone would
        // wrongly adopt a replay built for a different vertex in the same leaf. `label_id` +
        // `slab_slots` guard the slab/log split, and the `(degree, stored_slots, overflow_log_head,
        // edge_start)` snapshot guards against an intervening mutation that keeps the split intact —
        // notably an in-place overflow-log tombstone delete, which only decrements `degree` while
        // the cached `log_table` would still decode the now-removed edge. On any mismatch we fall
        // through to the sparse path, which resolves each slot from canonical state.
        if let Some(replay) = replay
            && replay.is_active()
            && bucket.overflow_log_head() >= 0
            && replay.src == src
            && replay.label_id == label_id
            && replay.slab_slots == self.bucket_slab_prefix_slots(src, &bucket)
            && replay.degree == bucket.degree()
            && replay.stored_slots == bucket.stored_slots
            && replay.overflow_log_head == bucket.overflow_log_head()
            && replay.edge_start == bucket.edge_start()
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_read_hybrid_replay_out_edge_slots");
            return self.read_out_edge_slots_with_hybrid_replay(
                &bucket,
                label_id,
                &visit_order,
                replay,
                &mut visit,
            );
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_read_sparse_out_edge_slots");
        let overflow_chain = (bucket.overflow_log_head() >= 0).then(|| {
            #[cfg(test)]
            crate::lara::edge::scan_guard::record_overflow_chain_rebuild();
            self.edges.overflow_log_chain_asc_indices(
                self.payload_log_leaf(src),
                bucket.overflow_log_head(),
            )
        });
        for slot_index in visit_order {
            if let Some(edge) = self.read_out_edge_topology_at_slot(
                src,
                &vertex,
                bucket_index,
                &bucket,
                slot_index,
                label_id,
                overflow_chain.as_deref(),
            )? {
                visit(edge);
            }
        }
        Ok(())
    }

    fn read_out_edge_slots_with_hybrid_replay<Visit>(
        &self,
        bucket: &LabelBucket,
        label_id: BucketLabelKey,
        visit_order: &[u32],
        replay: &HybridOverflowEdgeReplay,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        let log_table = (!replay.log_table.is_empty()).then_some(replay.log_table.as_slice());
        for &slot_index in visit_order {
            if slot_index < replay.slab_slots {
                if slab_slot_deleted(slot_index, &replay.deleted_slab_offsets) {
                    continue;
                }
                let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let edge = self
                    .edges
                    .read_slot(edge_slot)
                    .with_slot_index(slot_index)
                    .with_label_id(label_id.raw());
                if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                    continue;
                }
                visit(edge);
                continue;
            }
            let Some(log_slot) = slot_index.checked_sub(replay.slab_slots) else {
                continue;
            };
            let Some(Some(log_idx)) = replay.log_indices_by_slot.get(log_slot as usize) else {
                continue;
            };
            let edge = self
                .edges
                .decode_overflow_log_edge_from_table(replay.leaf, *log_idx, log_table)
                .with_slot_index(slot_index)
                .with_label_id(label_id.raw());
            if edge.is_tombstone_edge() {
                continue;
            }
            visit(edge);
        }
        Ok(())
    }

    fn read_dense_out_edge_topology_slots(
        &self,
        bucket: &LabelBucket,
        label_id: BucketLabelKey,
        slots: &[u32],
    ) -> Result<Vec<(u32, E)>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let mut asc = slots.to_vec();
        asc.sort_unstable();
        asc.dedup();
        let mut loaded = Vec::with_capacity(asc.len());
        for (first_slot, count) in super::super::invariants::ascending_contiguous_u32_runs(&asc) {
            if first_slot >= bucket.stored_slots {
                continue;
            }
            let take = count.min(bucket.stored_slots.saturating_sub(first_slot));
            if take == 0 {
                continue;
            }
            let mut raw_edges = vec![0u8; take as usize * E::BYTES];
            self.edges
                .read_slots_contiguous(bucket.edge_start() + u64::from(first_slot), &mut raw_edges);
            for i in 0..take as usize {
                let slot = first_slot + i as u32;
                let edge_off = i * E::BYTES;
                let edge = E::read_from(&raw_edges[edge_off..edge_off + E::BYTES])
                    .with_slot_index(slot)
                    .with_label_id(label_id.raw());
                if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                    continue;
                }
                loaded.push((slot, edge));
            }
        }
        Ok(loaded)
    }

    fn read_out_edge_topology_at_slot(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
        _bucket_index: u32,
        bucket: &LabelBucket,
        slot_index: u32,
        label_id: BucketLabelKey,
        overflow_chain: Option<&[u32]>,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if slot_index >= self.bucket_reserved_edge_slots(src, bucket) {
            return Ok(None);
        }
        if bucket.overflow_log_head() < 0 {
            if slot_index >= bucket.stored_slots {
                return Ok(None);
            }
            let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self
                .edges
                .read_slot(edge_slot)
                .with_slot_index(slot_index)
                .with_label_id(label_id.raw());
            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                return Ok(None);
            }
            return Ok(Some(edge));
        }

        let slab_prefix = self.bucket_slab_prefix_slots(src, bucket);
        if slot_index < slab_prefix {
            let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self
                .edges
                .read_slot(edge_slot)
                .with_slot_index(slot_index)
                .with_label_id(label_id.raw());
            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                return Ok(None);
            }
            return Ok(Some(edge));
        }

        let log_ordinal = slot_index
            .checked_sub(slab_prefix)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let leaf = self.payload_log_leaf(src);
        let chain_storage;
        let chain = match overflow_chain {
            Some(chain) => chain,
            None => {
                chain_storage = self
                    .edges
                    .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
                &chain_storage
            }
        };
        let Some(&entry_idx) = chain.get(log_ordinal as usize) else {
            return Ok(None);
        };
        let (_, edge) = self.edges.read_overflow_log_entry(leaf, entry_idx);
        if edge.is_tombstone_edge() {
            return Ok(None);
        }
        Ok(Some(
            edge.with_slot_index(slot_index)
                .with_label_id(label_id.raw()),
        ))
    }

    fn visit_dense_out_edge_inline_value_batches_for_slab_prefix<Visit>(
        &self,
        bucket: LabelBucket,
        scan_slots: u32,
        deleted_slab_offsets: &[u32],
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlineValueBatchScratch<E>,
        visit: &mut Visit,
        dense: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledEdgeInlineValueBatch<'b, E>),
    {
        if scan_slots == 0 {
            return Ok(());
        }
        let width = usize::from(bucket.inline_value_byte_width());
        let batch_edges = (EDGE_PAYLOAD_BATCH_TARGET_BYTES / width).max(1);
        let label_id = bucket.bucket_label_key();
        let mut remaining = scan_slots;
        while remaining > 0 {
            let take = remaining.min(batch_edges as u32);
            let first_slot = match order {
                OutEdgeOrder::Descending => remaining - take,
                OutEdgeOrder::Ascending => scan_slots - remaining,
            };
            scratch.clear();
            scratch.edges.reserve(take as usize);
            scratch.inline_value_bytes.reserve(take as usize * width);

            let edge_bytes = take as usize * E::BYTES;
            let inline_value_bytes = take as usize * width;
            {
                let raw_edges = scratch.io_edge_slice_mut(edge_bytes);
                self.edges
                    .read_slots_contiguous(bucket.edge_start() + u64::from(first_slot), raw_edges);
            }
            let inline_value_offset =
                super::super::invariants::inline_value_byte_offset_at_slot(&bucket, first_slot)?;
            {
                let raw_values = scratch.io_payload_slice_mut(inline_value_bytes);
                self.values.read_bytes(inline_value_offset, raw_values);
            }
            let raw_edges = &scratch.io_edge_bytes[..edge_bytes];
            let raw_values = &scratch.io_inline_value_bytes[..inline_value_bytes];

            match order {
                OutEdgeOrder::Ascending => {
                    for i in 0..take as usize {
                        let slot = first_slot + i as u32;
                        if slab_slot_deleted(slot, deleted_slab_offsets) {
                            continue;
                        }
                        let edge_off = i * E::BYTES;
                        let value_off = i * width;
                        let edge = E::read_from(&raw_edges[edge_off..edge_off + E::BYTES])
                            .with_slot_index(slot)
                            .with_label_id(label_id.raw());
                        if edge.is_deleted_slot() {
                            continue;
                        }
                        scratch.edges.push(edge);
                        scratch
                            .inline_value_bytes
                            .extend_from_slice(&raw_values[value_off..value_off + width]);
                    }
                }
                OutEdgeOrder::Descending => {
                    for i in (0..take as usize).rev() {
                        let slot = first_slot + i as u32;
                        if slab_slot_deleted(slot, deleted_slab_offsets) {
                            continue;
                        }
                        let edge_off = i * E::BYTES;
                        let value_off = i * width;
                        let edge = E::read_from(&raw_edges[edge_off..edge_off + E::BYTES])
                            .with_slot_index(slot)
                            .with_label_id(label_id.raw());
                        if edge.is_deleted_slot() {
                            continue;
                        }
                        scratch.edges.push(edge);
                        scratch
                            .inline_value_bytes
                            .extend_from_slice(&raw_values[value_off..value_off + width]);
                    }
                }
            }

            emit_edge_inline_value_batch(
                scratch,
                visit,
                label_id,
                bucket.inline_value_byte_width(),
                order,
                dense,
            );
            remaining -= take;
        }
        Ok(())
    }

    fn visit_hybrid_out_edge_inline_value_batches_for_bucket<Visit>(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket_slot: u64,
        bucket: LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlineValueBatchScratch<E>,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledEdgeInlineValueBatch<'b, E>),
    {
        // The paired bulk path may index edge and payload bytes with the same local offset only
        // when both independent stores currently expose the same slab prefix. Otherwise use the
        // live-ordinal-aware streaming batch path; physical split points are not an association
        // contract.
        if bucket.inline_value_slab_slots() != bucket.stored_slots {
            return self.visit_sparse_out_edge_inline_value_batches_for_bucket(
                src,
                vertex,
                bucket_index,
                bucket,
                order,
                scratch,
                visit,
            );
        }
        let successor_start =
            self.bucket_slab_window_end_exclusive_after_bucket(vertex, bucket_index, &bucket)?;
        let acc = LabelEdgeSpanAccess::with_bucket(
            &self.buckets,
            bucket_slot,
            bucket,
            successor_start,
            src,
        );
        let label_id = bucket.bucket_label_key();
        let width = usize::from(bucket.inline_value_byte_width());
        let batch_edges = (EDGE_PAYLOAD_BATCH_TARGET_BYTES / width).max(1);
        let log_chains = self.bucket_payload_log_chain_opt(src, &bucket);

        match order {
            OutEdgeOrder::Descending => {
                let OutOverflowDescParts {
                    log_entries,
                    deleted_slab_offsets,
                    slab_slots,
                    mut next_log_slot,
                } = self
                    .edges
                    .out_edges_iter(&acc, VertexId::from(0))?
                    .into_overflow_desc_parts();
                scratch.clear();
                for edge_opt in log_entries {
                    if scratch.edges.len() >= batch_edges {
                        emit_edge_inline_value_batch(
                            scratch,
                            visit,
                            label_id,
                            bucket.inline_value_byte_width(),
                            order,
                            false,
                        );
                        scratch.clear();
                    }
                    let Some(edge) = edge_opt else {
                        next_log_slot = next_log_slot.saturating_sub(1);
                        continue;
                    };
                    let slot = next_log_slot;
                    next_log_slot = next_log_slot.saturating_sub(1);
                    let edge = edge.with_slot_index(slot).with_label_id(label_id.raw());
                    let edge = self.attach_edge_inline_value(
                        src,
                        vertex,
                        bucket_index,
                        bucket,
                        slot,
                        edge,
                        log_chains.as_ref(),
                    )?;
                    scratch
                        .inline_value_bytes
                        .extend_from_slice(edge.edge_inline_value_bytes());
                    scratch.edges.push(edge);
                }
                emit_edge_inline_value_batch(
                    scratch,
                    visit,
                    label_id,
                    bucket.inline_value_byte_width(),
                    order,
                    false,
                );
                self.visit_dense_out_edge_inline_value_batches_for_slab_prefix(
                    bucket,
                    slab_slots,
                    &deleted_slab_offsets,
                    order,
                    scratch,
                    visit,
                    true,
                )
            }
            OutEdgeOrder::Ascending => {
                let OutOverflowAscParts {
                    inserted_log_entries,
                    deleted_slab_offsets,
                    slab_slots,
                    mut next_inserted_log_slot,
                } = self
                    .edges
                    .asc_out_edges_iter(&acc, VertexId::from(0))?
                    .into_overflow_asc_parts();
                self.visit_dense_out_edge_inline_value_batches_for_slab_prefix(
                    bucket,
                    slab_slots,
                    &deleted_slab_offsets,
                    order,
                    scratch,
                    visit,
                    true,
                )?;
                scratch.clear();
                for edge_opt in inserted_log_entries {
                    if scratch.edges.len() >= batch_edges {
                        emit_edge_inline_value_batch(
                            scratch,
                            visit,
                            label_id,
                            bucket.inline_value_byte_width(),
                            order,
                            false,
                        );
                        scratch.clear();
                    }
                    let Some(edge) = edge_opt else {
                        next_inserted_log_slot = next_inserted_log_slot.saturating_add(1);
                        continue;
                    };
                    let slot = next_inserted_log_slot;
                    next_inserted_log_slot = next_inserted_log_slot.saturating_add(1);
                    let edge = edge.with_slot_index(slot).with_label_id(label_id.raw());
                    let edge = self.attach_edge_inline_value(
                        src,
                        vertex,
                        bucket_index,
                        bucket,
                        slot,
                        edge,
                        log_chains.as_ref(),
                    )?;
                    scratch
                        .inline_value_bytes
                        .extend_from_slice(edge.edge_inline_value_bytes());
                    scratch.edges.push(edge);
                }
                emit_edge_inline_value_batch(
                    scratch,
                    visit,
                    label_id,
                    bucket.inline_value_byte_width(),
                    order,
                    false,
                );
                Ok(())
            }
        }
    }

    fn visit_sparse_out_edge_inline_value_batches_for_bucket<Visit>(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlineValueBatchScratch<E>,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledEdgeInlineValueBatch<'b, E>),
    {
        let label_id = bucket.bucket_label_key();
        let width = usize::from(bucket.inline_value_byte_width());
        let batch_edges = (EDGE_PAYLOAD_BATCH_TARGET_BYTES / width).max(1);
        let mut iter =
            self.labeled_bucket_span_iter(src, order, vertex, &[bucket], 0, bucket_index, true)?;
        loop {
            scratch.clear();
            scratch.edges.reserve(batch_edges);
            scratch.inline_value_bytes.reserve(batch_edges * width);
            for _ in 0..batch_edges {
                let Some(edge) = iter.next() else {
                    break;
                };
                let edge = edge?;
                scratch
                    .inline_value_bytes
                    .extend_from_slice(edge.edge_inline_value_bytes());
                scratch.edges.push(edge);
            }
            if scratch.edges.is_empty() {
                return Ok(());
            }
            emit_edge_inline_value_batch(
                scratch,
                visit,
                label_id,
                bucket.inline_value_byte_width(),
                order,
                false,
            );
        }
    }

    fn visit_dense_out_inline_value_batches_for_slab_prefix<Visit>(
        &self,
        bucket: LabelBucket,
        scan_slots: u32,
        deleted_slab_offsets: &[u32],
        order: OutEdgeOrder,
        scratch: &mut LabeledPayloadValueBatchScratch,
        visit: &mut Visit,
        dense: bool,
        omit_edge_slab_reads: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledPayloadValueBatch<'b>),
    {
        if scan_slots == 0 {
            return Ok(());
        }
        let width = usize::from(bucket.inline_value_byte_width());
        let batch_edges = (EDGE_PAYLOAD_BATCH_TARGET_BYTES / width).max(1);
        let label_id = bucket.bucket_label_key();
        if !omit_edge_slab_reads || !deleted_slab_offsets.is_empty() {
            let mut ordered: Vec<(u32, u32)> = (0..scan_slots)
                .filter(|slot| {
                    if slab_slot_deleted(*slot, deleted_slab_offsets) {
                        return false;
                    }
                    if omit_edge_slab_reads {
                        return true;
                    }
                    let edge = self.edges.read_slot(bucket.edge_start() + u64::from(*slot));
                    !edge.is_deleted_slot() && !edge.is_tombstone_edge()
                })
                .enumerate()
                .map(|(ordinal, slot)| {
                    u32::try_from(ordinal)
                        .map(|ordinal| (slot, ordinal))
                        .map_err(|_| LaraOperationError::CollectAllocationOverflow)
                })
                .collect::<Result<_, _>>()?;
            if matches!(order, OutEdgeOrder::Descending) {
                ordered.reverse();
            }
            for chunk in ordered.chunks(batch_edges) {
                scratch.clear();
                for &(slot, ordinal) in chunk {
                    let offset = super::super::invariants::inline_value_byte_offset_at_slot(
                        &bucket, ordinal,
                    )?;
                    let mut value = vec![0u8; width];
                    self.values.read_bytes(offset, &mut value);
                    scratch.slot_indices.push(slot);
                    scratch.values.extend_from_slice(&value);
                }
                emit_inline_value_batch(
                    scratch,
                    visit,
                    label_id,
                    bucket.inline_value_byte_width(),
                    order,
                    false,
                );
            }
            return Ok(());
        }
        let mut remaining = scan_slots;
        while remaining > 0 {
            let take = remaining.min(batch_edges as u32);
            let first_slot = match order {
                OutEdgeOrder::Descending => remaining - take,
                OutEdgeOrder::Ascending => scan_slots - remaining,
            };
            scratch.clear();
            scratch.slot_indices.reserve(take as usize);
            scratch.values.reserve(take as usize * width);

            let inline_value_bytes = take as usize * width;
            let inline_value_offset =
                super::super::invariants::inline_value_byte_offset_at_slot(&bucket, first_slot)?;
            let mut raw_values = vec![0u8; inline_value_bytes];
            self.values.read_bytes(inline_value_offset, &mut raw_values);

            let mut raw_edges = Vec::new();
            if !omit_edge_slab_reads {
                let edge_bytes = take as usize * E::BYTES;
                raw_edges = vec![0u8; edge_bytes];
                self.edges.read_slots_contiguous(
                    bucket.edge_start() + u64::from(first_slot),
                    &mut raw_edges,
                );
            }

            match order {
                OutEdgeOrder::Ascending => {
                    for i in 0..take as usize {
                        let slot = first_slot + i as u32;
                        if slab_slot_deleted(slot, deleted_slab_offsets) {
                            continue;
                        }
                        if !omit_edge_slab_reads {
                            let edge_off = i * E::BYTES;
                            let edge = E::read_from(&raw_edges[edge_off..edge_off + E::BYTES])
                                .with_slot_index(slot)
                                .with_label_id(label_id.raw());
                            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                                continue;
                            }
                        }
                        let value_off = i * width;
                        scratch.slot_indices.push(slot);
                        scratch
                            .values
                            .extend_from_slice(&raw_values[value_off..value_off + width]);
                    }
                }
                OutEdgeOrder::Descending => {
                    for i in (0..take as usize).rev() {
                        let slot = first_slot + i as u32;
                        if slab_slot_deleted(slot, deleted_slab_offsets) {
                            continue;
                        }
                        if !omit_edge_slab_reads {
                            let edge_off = i * E::BYTES;
                            let edge = E::read_from(&raw_edges[edge_off..edge_off + E::BYTES])
                                .with_slot_index(slot)
                                .with_label_id(label_id.raw());
                            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                                continue;
                            }
                        }
                        let value_off = i * width;
                        scratch.slot_indices.push(slot);
                        scratch
                            .values
                            .extend_from_slice(&raw_values[value_off..value_off + width]);
                    }
                }
            }

            emit_inline_value_batch(
                scratch,
                visit,
                label_id,
                bucket.inline_value_byte_width(),
                order,
                dense,
            );
            remaining -= take;
        }
        Ok(())
    }

    fn emit_hybrid_overflow_log_inline_values_desc<Visit>(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        prefetched: (Vec<Option<u32>>, Vec<u32>, Vec<u8>),
        order: OutEdgeOrder,
        scratch: &mut LabeledPayloadValueBatchScratch,
        visit: &mut Visit,
        log_chains: Option<&Vec<u32>>,
    ) -> Result<(u32, Vec<u32>), LabeledOperationError>
    where
        Visit: for<'b> FnMut(LabeledPayloadValueBatch<'b>),
    {
        let leaf = self.payload_log_leaf(src);
        let (mut replay_entries, mut deleted_slab_offsets, log_table) = prefetched;
        deleted_slab_offsets.sort_unstable();
        let slab_slots = self.bucket_slab_prefix_slots(src, bucket);
        let label_id = bucket.bucket_label_key();
        let width = usize::from(bucket.inline_value_byte_width());
        let batch_edges = (EDGE_PAYLOAD_BATCH_TARGET_BYTES / width).max(1);
        let reserved_log_slots = u32::try_from(replay_entries.len())
            .map_err(|_| LaraOperationError::RowDegreeOverflow)?;
        let mut next_log_slot = slab_slots
            .saturating_add(reserved_log_slots)
            .saturating_sub(1);
        let mut next_payload_ordinal = bucket.degree().saturating_sub(1);

        let mut live_slots = Vec::with_capacity(replay_entries.len());
        for &log_entry in &replay_entries {
            if log_entry.is_none() {
                next_log_slot = next_log_slot.saturating_sub(1);
                continue;
            }
            let slot = next_log_slot;
            next_log_slot = next_log_slot.saturating_sub(1);
            live_slots.push((slot, next_payload_ordinal));
            next_payload_ordinal = next_payload_ordinal.saturating_sub(1);
        }
        for chunk in live_slots.chunks(batch_edges) {
            scratch.clear();
            self.append_ordered_payload_ordinals(src, bucket, chunk, log_chains, scratch)?;
            emit_inline_value_batch(
                scratch,
                visit,
                label_id,
                bucket.inline_value_byte_width(),
                order,
                false,
            );
        }
        let replay = &mut scratch.hybrid_overflow_replay;
        replay.clear();
        replay.src = src;
        replay.leaf = leaf;
        replay.label_id = label_id;
        replay.slab_slots = slab_slots;
        replay.degree = bucket.degree();
        replay.stored_slots = bucket.stored_slots;
        replay.overflow_log_head = bucket.overflow_log_head();
        replay.edge_start = bucket.edge_start();
        replay.deleted_slab_offsets = deleted_slab_offsets.clone();
        replay.log_table = log_table;
        replay_entries.reverse();
        replay.log_indices_by_slot = replay_entries;
        Ok((slab_slots, deleted_slab_offsets))
    }

    fn emit_hybrid_overflow_log_inline_values_asc<Visit>(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        prefetched: (Vec<Option<u32>>, Vec<u32>, Vec<u8>),
        order: OutEdgeOrder,
        scratch: &mut LabeledPayloadValueBatchScratch,
        visit: &mut Visit,
        log_chains: Option<&Vec<u32>>,
    ) -> Result<(u32, Vec<u32>), LabeledOperationError>
    where
        Visit: for<'b> FnMut(LabeledPayloadValueBatch<'b>),
    {
        let leaf = self.payload_log_leaf(src);
        let (inserted_entries, deleted_slab_offsets, log_table) = prefetched;
        let slab_slots = self.bucket_slab_prefix_slots(src, bucket);
        let label_id = bucket.bucket_label_key();
        let width = usize::from(bucket.inline_value_byte_width());
        let batch_edges = (EDGE_PAYLOAD_BATCH_TARGET_BYTES / width).max(1);
        let mut next_inserted_log_slot = slab_slots;
        let mut next_payload_ordinal = slab_slots
            .saturating_sub(u32::try_from(deleted_slab_offsets.len()).unwrap_or(u32::MAX));

        let mut live_slots = Vec::with_capacity(inserted_entries.len());
        for &log_entry in &inserted_entries {
            if log_entry.is_none() {
                next_inserted_log_slot = next_inserted_log_slot.saturating_add(1);
                continue;
            }
            let slot = next_inserted_log_slot;
            next_inserted_log_slot = next_inserted_log_slot.saturating_add(1);
            live_slots.push((slot, next_payload_ordinal));
            next_payload_ordinal = next_payload_ordinal.saturating_add(1);
        }
        for chunk in live_slots.chunks(batch_edges) {
            scratch.clear();
            self.append_ordered_payload_ordinals(src, bucket, chunk, log_chains, scratch)?;
            emit_inline_value_batch(
                scratch,
                visit,
                label_id,
                bucket.inline_value_byte_width(),
                order,
                false,
            );
        }
        let replay = &mut scratch.hybrid_overflow_replay;
        replay.clear();
        replay.src = src;
        replay.leaf = leaf;
        replay.label_id = label_id;
        replay.slab_slots = slab_slots;
        replay.degree = bucket.degree();
        replay.stored_slots = bucket.stored_slots;
        replay.overflow_log_head = bucket.overflow_log_head();
        replay.edge_start = bucket.edge_start();
        replay.deleted_slab_offsets = deleted_slab_offsets.clone();
        replay.log_table = log_table;
        replay.log_indices_by_slot = inserted_entries;
        Ok((slab_slots, deleted_slab_offsets))
    }

    fn append_ordered_payload_ordinals(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        slots_and_ordinals: &[(u32, u32)],
        log_chain: Option<&Vec<u32>>,
        scratch: &mut LabeledPayloadValueBatchScratch,
    ) -> Result<(), LabeledOperationError> {
        let width = usize::from(bucket.inline_value_byte_width());
        let slab_slots = bucket.inline_value_slab_slots();
        let mut i = 0usize;
        while i < slots_and_ordinals.len() {
            let (_, ordinal) = slots_and_ordinals[i];
            if ordinal >= slab_slots {
                let payload = self.read_bucket_payload_for_slot(src, bucket, ordinal, log_chain)?;
                scratch.slot_indices.push(slots_and_ordinals[i].0);
                scratch.values.extend_from_slice(&payload);
                i += 1;
                continue;
            }

            let mut end = i + 1;
            while end < slots_and_ordinals.len() {
                let previous = slots_and_ordinals[end - 1].1;
                let next = slots_and_ordinals[end].1;
                if next >= slab_slots || previous.abs_diff(next) != 1 {
                    break;
                }
                end += 1;
            }
            let first = slots_and_ordinals[i].1;
            let last = slots_and_ordinals[end - 1].1;
            let low = first.min(last);
            let count = end - i;
            let byte_len = count
                .checked_mul(width)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let offset = super::super::invariants::inline_value_byte_offset_at_slot(bucket, low)?;
            let mut bytes = vec![0u8; byte_len];
            self.values.read_bytes(offset, &mut bytes);
            for &(slot, ordinal) in &slots_and_ordinals[i..end] {
                let value_index = usize::try_from(ordinal.saturating_sub(low))
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                let start = value_index
                    .checked_mul(width)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                scratch.slot_indices.push(slot);
                scratch
                    .values
                    .extend_from_slice(&bytes[start..start + width]);
            }
            i = end;
        }
        Ok(())
    }

    fn visit_payload_log_ordinals_in_edge_slab<Visit>(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        start_ordinal: u32,
        end_ordinal: u32,
        order: OutEdgeOrder,
        scratch: &mut LabeledPayloadValueBatchScratch,
        visit: &mut Visit,
        log_chain: Option<&Vec<u32>>,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: for<'b> FnMut(LabeledPayloadValueBatch<'b>),
    {
        if start_ordinal >= end_ordinal {
            return Ok(());
        }
        let width = usize::from(bucket.inline_value_byte_width());
        let batch_edges = (EDGE_PAYLOAD_BATCH_TARGET_BYTES / width).max(1);
        let mut remaining = end_ordinal - start_ordinal;
        while remaining > 0 {
            let take = remaining.min(batch_edges as u32);
            scratch.clear();
            match order {
                OutEdgeOrder::Ascending => {
                    let first = end_ordinal - remaining;
                    for ordinal in first..first + take {
                        let payload =
                            self.read_bucket_payload_for_slot(src, bucket, ordinal, log_chain)?;
                        scratch.slot_indices.push(ordinal);
                        scratch.values.extend_from_slice(&payload);
                    }
                }
                OutEdgeOrder::Descending => {
                    let high = start_ordinal + remaining;
                    for ordinal in (high - take..high).rev() {
                        let payload =
                            self.read_bucket_payload_for_slot(src, bucket, ordinal, log_chain)?;
                        scratch.slot_indices.push(ordinal);
                        scratch.values.extend_from_slice(&payload);
                    }
                }
            }
            emit_inline_value_batch(
                scratch,
                visit,
                bucket.bucket_label_key(),
                bucket.inline_value_byte_width(),
                order,
                false,
            );
            remaining -= take;
        }
        Ok(())
    }

    fn visit_hybrid_out_inline_value_batches_for_bucket<Visit>(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
        _bucket_index: u32,
        _bucket_slot: u64,
        bucket: LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledPayloadValueBatchScratch,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledPayloadValueBatch<'b>),
    {
        let log_chains = self.bucket_payload_log_chain_opt(src, &bucket);
        match order {
            OutEdgeOrder::Descending => {
                let prefetched = self.edges.prefetch_overflow_log_replay_desc(
                    self.payload_log_leaf(src),
                    bucket.overflow_log_head(),
                )?;
                let reserved = bucket.stored_slots.saturating_add(
                    u32::try_from(prefetched.0.len())
                        .map_err(|_| LaraOperationError::RowDegreeOverflow)?,
                );
                // Equality proves there are no edge tombstones: physical edge slots and
                // bucket-local live ordinals are then identical even when the independent
                // payload slab/log split differs from the edge slab/log split.
                if reserved != bucket.degree() {
                    return self.visit_sparse_out_inline_value_batches_for_bucket(
                        src,
                        _vertex,
                        _bucket_index,
                        bucket,
                        order,
                        scratch,
                        visit,
                    );
                }
                let (slab_slots, deleted_slab_offsets) = self
                    .emit_hybrid_overflow_log_inline_values_desc(
                        src,
                        &bucket,
                        prefetched,
                        order,
                        scratch,
                        visit,
                        log_chains.as_ref(),
                    )?;
                let payload_slab_slots = slab_slots.min(bucket.inline_value_slab_slots());
                self.visit_payload_log_ordinals_in_edge_slab(
                    src,
                    &bucket,
                    payload_slab_slots,
                    slab_slots,
                    order,
                    scratch,
                    visit,
                    log_chains.as_ref(),
                )?;
                self.visit_dense_out_inline_value_batches_for_slab_prefix(
                    bucket,
                    payload_slab_slots,
                    &deleted_slab_offsets,
                    order,
                    scratch,
                    visit,
                    true,
                    true,
                )
            }
            OutEdgeOrder::Ascending => {
                let slab_slots = self.bucket_slab_prefix_slots(src, &bucket);
                let prefetched = self.edges.prefetch_overflow_log_inserted_tags_asc(
                    self.payload_log_leaf(src),
                    bucket.overflow_log_head(),
                )?;
                let reserved = bucket.stored_slots.saturating_add(
                    u32::try_from(prefetched.0.len())
                        .map_err(|_| LaraOperationError::RowDegreeOverflow)?,
                );
                if reserved != bucket.degree() {
                    return self.visit_sparse_out_inline_value_batches_for_bucket(
                        src,
                        _vertex,
                        _bucket_index,
                        bucket,
                        order,
                        scratch,
                        visit,
                    );
                }
                let deleted_slab_offsets = &prefetched.1;
                let payload_slab_slots = slab_slots.min(bucket.inline_value_slab_slots());
                self.visit_dense_out_inline_value_batches_for_slab_prefix(
                    bucket,
                    payload_slab_slots,
                    deleted_slab_offsets,
                    order,
                    scratch,
                    visit,
                    true,
                    true,
                )?;
                self.visit_payload_log_ordinals_in_edge_slab(
                    src,
                    &bucket,
                    payload_slab_slots,
                    slab_slots,
                    order,
                    scratch,
                    visit,
                    log_chains.as_ref(),
                )?;
                let (slab_slots, _deleted_slab_offsets) = self
                    .emit_hybrid_overflow_log_inline_values_asc(
                        src,
                        &bucket,
                        prefetched,
                        order,
                        scratch,
                        visit,
                        log_chains.as_ref(),
                    )?;
                debug_assert_eq!(slab_slots, self.bucket_slab_prefix_slots(src, &bucket));
                Ok(())
            }
        }
    }

    fn visit_sparse_out_inline_value_batches_for_bucket<Visit>(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledPayloadValueBatchScratch,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledPayloadValueBatch<'b>),
    {
        let label_id = bucket.bucket_label_key();
        let width = usize::from(bucket.inline_value_byte_width());
        let batch_edges = (EDGE_PAYLOAD_BATCH_TARGET_BYTES / width).max(1);
        let mut iter =
            self.labeled_bucket_span_iter(src, order, vertex, &[bucket], 0, bucket_index, true)?;
        loop {
            scratch.clear();
            scratch.slot_indices.reserve(batch_edges);
            scratch.values.reserve(batch_edges * width);
            for _ in 0..batch_edges {
                let Some(edge) = iter.next() else {
                    break;
                };
                let edge = edge?;
                scratch.slot_indices.push(edge.edge_slot_index_raw());
                scratch
                    .values
                    .extend_from_slice(edge.edge_inline_value_bytes());
            }
            if scratch.slot_indices.is_empty() {
                return Ok(());
            }
            emit_inline_value_batch(
                scratch,
                visit,
                label_id,
                bucket.inline_value_byte_width(),
                order,
                false,
            );
        }
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
        if let Some(run) = Self::try_contiguous_tiled_labeled_out_edges(&vertex, &buckets) {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_find_out_edge_with_label_tiled");
            if run.total_edges() == 0 {
                return Ok(None);
            }
            let nbytes = run.byte_len::<E>()?;
            let mut raw = vec![0u8; nbytes];
            self.edges.read_slots_contiguous(run.base(), &mut raw);
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
                let chunk = run.edge_chunk::<E>(&raw, bucket, slot)?;
                let log_chains = self.bucket_payload_log_chain_opt(src, bucket);
                let edge = self.attach_edge_inline_value(
                    src,
                    &vertex,
                    bidx as u32,
                    *bucket,
                    slot,
                    E::read_from(chunk).with_slot_index(slot),
                    log_chains.as_ref(),
                )?;
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
                self.bucket_slab_window_end_exclusive_after_bucket(&vertex, bucket_index, bucket)?;
            let acc = LabelEdgeSpanAccess::with_bucket(
                &self.buckets,
                slot,
                *bucket,
                successor_start,
                src,
            );
            let log_chains = self.bucket_payload_log_chain_opt(src, bucket);
            for edge in self.edges.out_edges_iter(&acc, VertexId::from(0))? {
                let slot_index = edge.edge_slot_index_raw();
                let edge = self.attach_edge_inline_value(
                    src,
                    &vertex,
                    bucket_index,
                    *bucket,
                    slot_index,
                    edge,
                    log_chains.as_ref(),
                )?;
                if pred(&edge) {
                    return Ok(Some((
                        edge.with_label_id(bucket.bucket_label_key().raw()),
                        Some(bucket.bucket_label_key()),
                    )));
                }
            }
        }
        Ok(None)
    }

    /// Finds the first outgoing edge matching `pred`, returning its label and bucket slot index.
    pub fn find_out_edge_slot_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        mut pred: F,
    ) -> Result<Option<(E, BucketLabelKey, u32)>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
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
            let log_chains = self.bucket_payload_log_chain_opt(src, &bucket);
            for slot_index in (0..bucket.stored_slots).rev() {
                let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let edge = self.edges.read_slot(edge_slot).with_slot_index(slot_index);
                if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                    continue;
                }
                let edge = self.attach_edge_inline_value(
                    src,
                    &vertex,
                    bucket_index,
                    bucket,
                    slot_index,
                    edge,
                    log_chains.as_ref(),
                )?;
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

    /// Collects outgoing edges for one label.
    pub fn iter_edges_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<Vec<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let mut out = Vec::new();
        self.for_each_edges_for_label(src, label_id, |edge| out.push(edge))?;
        Ok(out)
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
        self.out_edges_by_directedness_iter(src, directedness, order)
            .and_then(|iter| iter.collect())
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

    /// A hybrid-overflow replay built for a **different vertex that shares the same payload-log
    /// leaf** (`leaf = src / segment_size`) must be rejected so phase 2 falls back to the sparse
    /// path. Reproduced with two real vertices in one leaf (not by mutating replay fields), since
    /// `leaf` + `label_id` + `slab_slots` alone cannot tell same-leaf vertices apart — only `src`
    /// can. Guards `read_out_edge_slots_for_label_with_replay`.
    #[test]
    fn hybrid_replay_from_other_vertex_in_same_leaf_falls_back_to_sparse() {
        let graph = inline_value_test_graph_with_capacity(1 << 16);
        let a = graph.push_vertex(LabeledVertex::default()).unwrap();
        let b = graph.push_vertex(LabeledVertex::default()).unwrap();
        assert_eq!(
            graph.payload_log_leaf(a),
            graph.payload_log_leaf(b),
            "test requires two vertices sharing one payload-log leaf"
        );
        let road = BucketLabelKey::from_raw(2);
        for v in [a, b] {
            graph
                .ensure_label_bucket_inline_value_byte_width(v, road, 2u16)
                .unwrap();
        }
        // Identically-shaped hybrid overflow buckets, but disjoint target ranges so the two
        // replays decode different overflow-log edges (A: 1..=48, B: 1001..=1048).
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    a,
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
            let bt = 1000 + target;
            graph
                .insert_edge_skip_leaf_cascade(
                    b,
                    road,
                    PayloadTestEdge::with_bytes(bt, &(bt as u16).to_le_bytes()),
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
        let mut scratch_a = crate::labeled::LabeledPayloadValueBatchScratch::default();
        let mut slots_a = Vec::new();
        graph
            .visit_out_inline_value_batches_for_label(
                a,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch_a,
                |batch| slots_a.extend_from_slice(batch.slot_indices),
            )
            .unwrap();
        let mut scratch_b = crate::labeled::LabeledPayloadValueBatchScratch::default();
        graph
            .visit_out_inline_value_batches_for_label(
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
            graph
                .read_out_edge_slots_for_label_with_replay(
                    a,
                    road,
                    &slots_a,
                    OutEdgeOrder::Ascending,
                    replay,
                    |edge| targets.push(edge.target),
                )
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
    fn hybrid_payload_first_keeps_replay_with_tombstone_free_slab_prefix() {
        let graph = inline_value_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(src, road, 2)
            .unwrap();
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
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
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
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

        let mut scratch = crate::labeled::LabeledPayloadValueBatchScratch::default();
        let mut observed = Vec::new();
        graph
            .visit_out_inline_value_batches_for_label(
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

        let graph = inline_value_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(src, road, 2u16)
            .unwrap();
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(src);
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);

        let mut scratch = crate::labeled::LabeledPayloadValueBatchScratch::default();
        let mut slots = Vec::new();
        graph
            .visit_out_inline_value_batches_for_label(
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
            graph
                .read_out_edge_slots_for_label_with_replay(
                    src,
                    road,
                    &slots,
                    OutEdgeOrder::Ascending,
                    replay,
                    |edge| targets.push(edge.target),
                )
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

        let graph = inline_value_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(src, road, 2u16)
            .unwrap();
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
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
        let mut scratch = crate::labeled::LabeledPayloadValueBatchScratch::default();
        let mut slots = Vec::new();
        graph
            .visit_out_inline_value_batches_for_label(
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
            graph
                .read_out_edge_slots_for_label_with_replay(
                    src,
                    road,
                    &slots,
                    OutEdgeOrder::Ascending,
                    replay,
                    |edge| targets.push(edge.target),
                )
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
