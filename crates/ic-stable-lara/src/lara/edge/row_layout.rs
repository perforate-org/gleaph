//! EdgeStore `row_layout` implementation.

use crate::lara::operation_error::{LaraOperationError, VertexAccess};
use crate::{
    GrowFailed, SegmentId, VertexId,
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

use super::counts::SegmentEdgeCounts;
use super::scan_iter::leaf_segment;
use super::span_meta::{SPAN_PHYSICAL_UNASSIGNED, SegmentSpanMeta};
use super::{EdgeLayout, EdgeStore};

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    pub(super) fn max_slab_window_for_vertex<V: CsrVertex>(v: &V, base: u64, end: u64) -> u64 {
        v.slab_append_exclusive_end(base)
            .map(|bypass_end| end.max(bypass_end))
            .unwrap_or(end)
    }
    pub(crate) fn slab_window_exclusive_end<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        v_ord: u32,
        v: &V,
    ) -> u64
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let len = vertices.len();
        let base = v.base_slot_start();
        let seg = edge_layout.segment_size.max(1);
        let leaf = v_ord / seg;
        let leaf_start = leaf.saturating_mul(seg);
        let leaf_logical_end_exclusive = leaf_start.saturating_add(seg);
        let occupied_leaf_end_exclusive = leaf_logical_end_exclusive.min(len);

        // Hot path for inserts: successive vertex ids inside one PMA leaf. Only touch
        // span metadata (+ counts when the leaf is PMA-pinned) after the CSR neighbor read.
        if v_ord.saturating_add(1) < occupied_leaf_end_exclusive {
            let next_base = vertices
                .get(VertexId::from(v_ord.saturating_add(1)))
                .base_slot_start();
            debug_assert!(
                next_base >= base,
                "LARA CSR invariant: base_slot_start must be non-decreasing in VertexId order"
            );
            let span_rec = self.span_meta_store().get(u64::from(leaf));
            if span_rec.physical_start == SPAN_PHYSICAL_UNASSIGNED {
                return Self::max_slab_window_for_vertex(v, base, next_base);
            }
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            let cap = span_rec
                .physical_start
                .saturating_add(c.total.max(0) as u64);
            return Self::max_slab_window_for_vertex(v, base, next_base.min(cap));
        }

        let w = edge_layout.initial_vertex_edge_slots;
        if w > 0 && v_ord.saturating_add(1) < leaf_logical_end_exclusive {
            let tail = base.saturating_add(u64::from(w));
            let span_rec = self.span_meta_store().get(u64::from(leaf));
            if span_rec.physical_start == SPAN_PHYSICAL_UNASSIGNED {
                return Self::max_slab_window_for_vertex(v, base, tail);
            }
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            let cap = span_rec
                .physical_start
                .saturating_add(c.total.max(0) as u64);
            return Self::max_slab_window_for_vertex(v, base, tail.min(cap));
        }

        if v_ord.saturating_add(1) < len {
            let next_base = vertices
                .get(VertexId::from(v_ord.saturating_add(1)))
                .base_slot_start();
            if next_base >= base {
                let span_rec = self.span_meta_store().get(u64::from(leaf));
                if span_rec.physical_start != SPAN_PHYSICAL_UNASSIGNED {
                    let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
                    let cap = span_rec
                        .physical_start
                        .saturating_add(c.total.max(0) as u64);
                    return Self::max_slab_window_for_vertex(v, base, next_base.min(cap));
                }
                if leaf < edge_layout.segment_count {
                    let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
                    let total_u = c.total.max(0) as u64;
                    if total_u > 0 {
                        let head = vertices.get(VertexId::from(leaf_start)).base_slot_start();
                        let cap = head.saturating_add(total_u);
                        return Self::max_slab_window_for_vertex(v, base, next_base.min(cap));
                    }
                }
                return Self::max_slab_window_for_vertex(v, base, next_base);
            }
        }

        let span_rec = self.span_meta_store().get(u64::from(leaf));
        if span_rec.physical_start != SPAN_PHYSICAL_UNASSIGNED {
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            let end = span_rec
                .physical_start
                .saturating_add(c.total.max(0) as u64);
            return Self::max_slab_window_for_vertex(v, base, end);
        }

        let end = if leaf < edge_layout.segment_count {
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            base.saturating_add(c.total.max(0) as u64)
        } else {
            edge_layout.elem_capacity
        };
        Self::max_slab_window_for_vertex(v, base, end)
    }
    pub(crate) fn set_segment_physical_start(
        &self,
        segment: SegmentId,
        physical_start: u64,
    ) -> Result<(), GrowFailed> {
        let idx = u64::from(segment);
        if idx < self.span_meta.len() {
            self.span_meta.set(idx, &SegmentSpanMeta { physical_start });
        } else {
            while self.span_meta.len() < idx {
                self.span_meta.push(SegmentSpanMeta::default())?;
            }
            self.span_meta.push(SegmentSpanMeta { physical_start })?;
        }
        Ok(())
    }
    pub(super) fn edge_layout(&self) -> EdgeLayout {
        self.header().into()
    }
    pub(crate) fn on_slab_degree_for_vertex_access<V, A>(
        &self,
        vertices: &A,
        v_ord: u32,
        v: &V,
    ) -> Result<u32, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        self.on_slab_edges_with_layout(&self.edge_layout(), vertices, v_ord, v)
    }
    pub(super) fn on_slab_edges_with_layout<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        v_ord: u32,
        v: &V,
    ) -> Result<u32, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        if v.log_head() < 0 {
            return Ok(v.stored_degree());
        }
        let next_exclusive = self.slab_window_exclusive_end(edge_layout, vertices, v_ord, v);
        let span_slots = next_exclusive
            .checked_sub(v.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let span_u32 = span_slots.min(u64::from(u32::MAX)) as u32;
        // Once the overflow log is active, the slab prefix is at most the CSR window
        // width; additional live edges are chained through `log_head`.
        Ok(if v.stored_degree() > span_u32 {
            span_u32
        } else {
            v.stored_degree()
        })
    }
    pub(super) fn have_space_on_slab<V, A>(
        &self,
        vertices: &A,
        v_ord: u32,
        v: &V,
        loc: u64,
        edge_layout: &EdgeLayout,
    ) -> bool
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        loc < self.slab_window_exclusive_end(edge_layout, vertices, v_ord, v)
    }
    pub(crate) fn bump_vertex_segment_counts(
        &self,
        vid: VertexId,
        d_actual: i64,
        d_total: i64,
    ) -> Result<(), LaraOperationError> {
        let edge_layout = self.edge_layout();
        self.bump_counts_leaf_with_layout(&edge_layout, vid, d_actual, d_total)
    }
    pub(super) fn bump_counts_leaf_with_layout(
        &self,
        edge_layout: &EdgeLayout,
        vid: VertexId,
        d_actual: i64,
        d_total: i64,
    ) -> Result<(), LaraOperationError> {
        let mut idx =
            (leaf_segment(vid, edge_layout.segment_size) + edge_layout.segment_count) as usize;
        if idx as u64 >= self.counts.len() {
            return Err(LaraOperationError::SegmentCountsTreeTooSmall);
        }
        // Inserts/removes only ever adjust `actual` (live edge records). `total` is owned by
        // explicit recount/rebalance paths (`LaraGraph::update_leaf_count_and_ancestors`).
        // Propagate the same delta up the tree with one read + write per level instead of
        // re-summing both children at every internal node (two reads + write per level).
        if d_total == 0 {
            loop {
                let mut c = self.counts.get(idx as u64);
                c.actual += d_actual;
                self.counts.set(idx as u64, &c);
                if idx == 1 {
                    break;
                }
                idx /= 2;
            }
            return Ok(());
        }
        loop {
            let mut c = self.counts.get(idx as u64);
            if idx >= edge_layout.segment_count as usize {
                c.actual += d_actual;
                c.total += d_total;
            } else {
                let left = self.counts.get((idx * 2) as u64);
                let right = self.counts.get((idx * 2 + 1) as u64);
                c = SegmentEdgeCounts {
                    actual: left.actual + right.actual,
                    total: left.total + right.total,
                };
            }
            self.counts.set(idx as u64, &c);
            if idx == 1 {
                break;
            }
            idx /= 2;
        }
        Ok(())
    }
}
