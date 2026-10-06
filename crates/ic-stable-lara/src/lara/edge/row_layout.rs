//! EdgeStore `row_layout` implementation.

use crate::lara::operation_error::{LaraOperationError, VertexAccess};
use crate::{
    GrowFailed, SegmentId, VertexId,
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

use super::iter::leaf_segment;
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
    ) -> Result<u64, LaraOperationError>
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
            // Synthetic accessors (label buckets) use the sorted neighbor boundary as the
            // authoritative slab-window end; consulting PMA span metadata would make clean
            // query scans depend on maintenance state.
            if v.trusts_neighbor_boundary() {
                return Ok(Self::max_slab_window_for_vertex(v, base, next_base));
            }
            if base == next_base {
                return Ok(Self::max_slab_window_for_vertex(v, base, next_base));
            }
            let Some(range) =
                self.assigned_physical_range_with_layout(VertexId::from(v_ord), edge_layout)?
            else {
                return Ok(Self::max_slab_window_for_vertex(v, base, next_base));
            };
            let cap = range.start_slot + range.len;
            // The leaf physical-block cap only bounds rows whose edges actually live
            // inside this leaf's block. A synthetic single-bucket access
            // (`LabelEdgeSpanAccess`) exposes `v_ord == 0` while its edge bytes may sit
            // in a *different* physical leaf; reading leaf-0's cap then yields
            // `cap < base`, and `next_base.min(cap)` would place the window end before
            // `base` (underflowing the on-slab span). When `base` is past the cap, the
            // next-bucket boundary is authoritative and the leaf cap must not apply.
            let end = if base < cap {
                next_base.min(cap)
            } else {
                next_base
            };
            return Ok(Self::max_slab_window_for_vertex(v, base, end));
        }

        let w = edge_layout.initial_vertex_edge_slots;
        if w > 0 && v_ord.saturating_add(1) < leaf_logical_end_exclusive {
            let tail = base.saturating_add(u64::from(w));
            let end = self
                .assigned_physical_range_with_layout(VertexId::from(v_ord), edge_layout)?
                .map_or(tail, |range| tail.min(range.start_slot + range.len));
            return Ok(Self::max_slab_window_for_vertex(v, base, end));
        }

        if v_ord.saturating_add(1) < len {
            let next_base = vertices
                .get(VertexId::from(v_ord.saturating_add(1)))
                .base_slot_start();
            if next_base >= base {
                if let Some(range) =
                    self.assigned_physical_range_with_layout(VertexId::from(v_ord), edge_layout)?
                {
                    let cap = range.start_slot + range.len;
                    return Ok(Self::max_slab_window_for_vertex(
                        v,
                        base,
                        next_base.min(cap),
                    ));
                }
                if leaf < edge_layout.segment_count {
                    let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
                    let total_u = c.total.max(0) as u64;
                    if total_u > 0 {
                        let head = vertices.get(VertexId::from(leaf_start)).base_slot_start();
                        let cap = head.saturating_add(total_u);
                        return Ok(Self::max_slab_window_for_vertex(
                            v,
                            base,
                            next_base.min(cap),
                        ));
                    }
                }
                return Ok(Self::max_slab_window_for_vertex(v, base, next_base));
            }
        }

        if let Some(range) =
            self.assigned_physical_range_with_layout(VertexId::from(v_ord), edge_layout)?
        {
            return Ok(Self::max_slab_window_for_vertex(
                v,
                base,
                range.start_slot + range.len,
            ));
        }

        let end = if leaf < edge_layout.segment_count {
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            base.saturating_add(c.total.max(0) as u64)
        } else {
            edge_layout.elem_capacity
        };
        Ok(Self::max_slab_window_for_vertex(v, base, end))
    }

    pub(crate) fn assigned_range_from_source(
        pin: SegmentSpanMeta,
        counts: super::counts::SegmentEdgeCounts,
        capacity: u64,
    ) -> Result<Option<super::free_span::FreeSpan>, LaraOperationError> {
        if pin.physical_start == SPAN_PHYSICAL_UNASSIGNED {
            return Ok(None);
        }
        let len = u64::try_from(counts.total)
            .ok()
            .filter(|len| *len > 0)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let end = crate::slab_index::checked_add_slot_exclusive_end(pin.physical_start, len)
            .filter(|end| *end <= capacity)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        Ok(Some(super::free_span::FreeSpan {
            start_slot: pin.physical_start,
            len: end - pin.physical_start,
        }))
    }

    pub(crate) fn assigned_physical_range(
        &self,
        vid: VertexId,
    ) -> Result<Option<super::free_span::FreeSpan>, LaraOperationError> {
        self.assigned_physical_range_with_layout(vid, &self.edge_layout())
    }

    fn assigned_physical_range_with_layout(
        &self,
        vid: VertexId,
        layout: &EdgeLayout,
    ) -> Result<Option<super::free_span::FreeSpan>, LaraOperationError> {
        let leaf = u64::from(leaf_segment(vid, layout.segment_size));
        let pin = self.span_meta.get(leaf);
        if pin.physical_start == SPAN_PHYSICAL_UNASSIGNED {
            return Ok(None);
        }
        let counts = self
            .counts
            .checked_get(self.counts_leaf_index(layout, vid)?)?;
        Self::assigned_range_from_source(pin, counts, layout.elem_capacity)
    }

    pub(crate) fn prepare_segment_physical_start(
        &self,
        segment: SegmentId,
        physical_start: u64,
    ) -> Result<super::span_meta::PreparedSegmentPhysicalStart<'_, M>, LaraOperationError> {
        self.span_meta
            .prepare_physical_start(u64::from(segment), physical_start)
    }

    pub(crate) fn prepare_physical_counts(
        &self,
        vid: VertexId,
        projection: super::counts::PhysicalCountProjection,
    ) -> Result<super::counts::PreparedPhysicalCounts<'_, E, M>, LaraOperationError> {
        match projection {
            super::counts::PhysicalCountProjection::VertexSpan { old, new, actual } => {
                self.prepare_vertex_span_counts(vid, old, new, actual)
            }
            super::counts::PhysicalCountProjection::LeafBlock(_)
            | super::counts::PhysicalCountProjection::RetainAssignment { .. } => self
                .counts
                .prepare_physical_counts(self.vertex_segment_counts_index(vid)?, projection),
        }
    }

    pub(crate) fn prepare_vertex_span_counts(
        &self,
        vid: VertexId,
        old: u32,
        new: u32,
        actual: i64,
    ) -> Result<super::counts::PreparedPhysicalCounts<'_, E, M>, LaraOperationError> {
        let projection = if self.assigned_physical_range(vid)?.is_some() {
            super::counts::PhysicalCountProjection::RetainAssignment { actual }
        } else {
            super::counts::PhysicalCountProjection::VertexSpan { old, new, actual }
        };
        self.counts
            .prepare_physical_counts(self.vertex_segment_counts_index(vid)?, projection)
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

    /// Initialized rows, independent of the surrounding allocation window. The row type owns
    /// whether its stored width includes spill; the store owns reading and validating the run.
    pub(crate) fn initialized_slab_slots<V: CsrVertex>(
        &self,
        v: &V,
    ) -> Result<u32, LaraOperationError> {
        self.initialized_row_slots(v).map(|(prefix, _)| prefix)
    }

    /// Checked initialized widths of the slab prefix and spill suffix. Allocation slack is
    /// never a logical slot, and the sum is validated here for every reader.
    pub(crate) fn initialized_row_slots<V: CsrVertex>(
        &self,
        v: &V,
    ) -> Result<(u32, u32), LaraOperationError> {
        let spill_rows = if v.spill_run() == crate::traits::NO_SPILL_RUN {
            0
        } else {
            let used = self.spill_used_len(self.spill_mint(v.spill_run())?);
            if used > crate::labeled::spill_run_store::MAX_ROWS {
                return Err(LaraOperationError::SpillRowOutOfRange);
            }
            used
        };
        let prefix = v
            .initialized_slab_slots(spill_rows)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        prefix
            .checked_add(spill_rows)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        Ok((prefix, spill_rows))
    }

    pub(super) fn have_space_on_slab<V, A>(
        &self,
        vertices: &A,
        v_ord: u32,
        v: &V,
        loc: u64,
        edge_layout: &EdgeLayout,
    ) -> Result<bool, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        Ok(loc < self.slab_window_exclusive_end(edge_layout, vertices, v_ord, v)?)
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

    pub(crate) fn vertex_segment_counts_index(
        &self,
        vid: VertexId,
    ) -> Result<u64, LaraOperationError> {
        self.counts_leaf_index(&self.edge_layout(), vid)
    }

    fn counts_leaf_index(
        &self,
        layout: &EdgeLayout,
        vid: VertexId,
    ) -> Result<u64, LaraOperationError> {
        let idx =
            u64::from(leaf_segment(vid, layout.segment_size)) + u64::from(layout.segment_count);
        if idx >= self.counts.len() {
            return Err(LaraOperationError::SegmentCountsTreeTooSmall);
        }
        Ok(idx)
    }

    pub(super) fn bump_counts_leaf_with_layout(
        &self,
        edge_layout: &EdgeLayout,
        vid: VertexId,
        d_actual: i64,
        d_total: i64,
    ) -> Result<(), LaraOperationError> {
        let idx = self.counts_leaf_index(edge_layout, vid)?;
        // **Leaf rows are canonical; internal rows are derived** (GAP-2026-09-20-004).
        // Every runtime consumer reads the leaf row: the density decision
        // (`leaf_segment_counts_for_vid`), the layout audit, and the tests. The
        // internal path was maintained eagerly with one read + write per level
        // (`~2.3 K instructions per insert`, 56 % of the attributed labeled append
        // cost in the S1 probe) and nothing read it, so the delta is applied to the
        // leaf only; `LaraGraph::rebuild_counts_internal_nodes` restores the
        // internal rows from the leaves where an aggregate is needed (segment
        // growth, audits, or a future reader).
        let c = self.counts.get(idx).checked_delta(d_actual, d_total)?;
        self.counts.set(idx, &c);
        Ok(())
    }
}
