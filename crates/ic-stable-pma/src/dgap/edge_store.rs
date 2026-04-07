//! DGAP edge region (`M_e`): three [`Memory`] regions behind [`DgapGraphMemories`] (CSR slab + per-leaf overflow logs).
//!
//! Persistent layout (per-memory offsets, `ic-stable_structures`-style diagrams): [`crate::layout::dgap`].
//!
//! Insert path follows [`gleaph-old/reference/DGAP/dgap/src/graph.h`](../../../../gleaph-old/reference/DGAP/dgap/src/graph.h)
//! `do_insertion` / `insert_into_log` / `have_space_onseg`.

use std::iter::{ExactSizeIterator, FusedIterator};
use std::marker::PhantomData;

use ic_stable_structures::Memory;

use crate::csr::vertex_column::CsrVertexColumn;
use crate::layout::dgap::{
    dgap_leaf_segment_id, dgap_log_entry_stride, required_edges_and_log_bytes, DgapEdgeHeaderV1,
    DGAP_DEFAULT_MAX_LOG_ENTRIES,
};
use crate::memory_util::GrowFailed;
use crate::traits::{CsrEdgeSlot, CsrVertex};
use crate::dgap::dgap_graph_memories::DgapGraphMemories;
use crate::dgap::pma_meta::{rebalance_decision, RebalanceDecision};

/// Stack / inline scratch cap for edge bytes (slab read, log entry payload, [`NeighborhoodIter`]).
const MAX_INLINE_EDGE: usize = 64;

/// Lazy outgoing neighborhood in DGAP `Neighborhood` order: contiguous **on-segment** slab slots,
/// then overflow **log** chain (same walk as C++ `CSRGraph::Neighborhood` / [`DgapEdgeStore::collect_out_edges`]).
///
/// `start_offset` is applied like C++: `min(requested, degree)` global edges are skipped before the first yield.
///
/// Yields [`Result`] so a truncated log chain surfaces as `Err("log chain short")` (then the iterator fuses).
pub struct NeighborhoodIter<'a, E: CsrEdgeSlot, M1: Memory, M2: Memory, M3: Memory> {
    store: &'a DgapEdgeStore<E, M1, M2, M3>,
    h: DgapEdgeHeaderV1,
    stride: u32,
    base: u64,
    /// Next slab slot index in `0..n_slab` (relative offset added to `base`).
    slab_cursor: usize,
    n_slab: usize,
    leaf: u32,
    log_i: i32,
    log_remaining: usize,
    remaining: usize,
    scratch: [u8; MAX_INLINE_EDGE],
}

impl<'a, E: CsrEdgeSlot, M1: Memory, M2: Memory, M3: Memory> Iterator
    for NeighborhoodIter<'a, E, M1, M2, M3>
{
    type Item = Result<E, &'static str>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        if self.slab_cursor < self.n_slab {
            let stride = self.stride as usize;
            self.store.read_slot_into(
                self.stride,
                self.base + self.slab_cursor as u64,
                &mut self.scratch[..stride],
            );
            let e = E::read_from(&self.scratch[..stride]);
            self.slab_cursor += 1;
            self.remaining -= 1;
            return Some(Ok(e));
        }
        if self.log_remaining == 0 {
            self.remaining = 0;
            return None;
        }
        if self.log_i < 0 {
            self.remaining = 0;
            self.log_remaining = 0;
            return Some(Err("log chain short"));
        }
        let li = self.log_i as u32;
        let eb_len = E::EDGE_BYTES;
        let (prev, _src) = self.store.read_log_entry_into(
            &self.h,
            self.leaf,
            li,
            &mut self.scratch[..eb_len],
        );
        let e = E::read_from(&self.scratch[..eb_len]);
        self.log_i = prev;
        self.log_remaining -= 1;
        self.remaining -= 1;
        Some(Ok(e))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a, E: CsrEdgeSlot, M1: Memory, M2: Memory, M3: Memory> ExactSizeIterator
    for NeighborhoodIter<'a, E, M1, M2, M3>
{
}

impl<'a, E: CsrEdgeSlot, M1: Memory, M2: Memory, M3: Memory> FusedIterator
    for NeighborhoodIter<'a, E, M1, M2, M3>
{
}

/// Owns the three-`Memory` DGAP edge bundle (`M_e`).
pub struct DgapEdgeStore<E: CsrEdgeSlot, M1, M2, M3> {
    mem: DgapGraphMemories<M1, M2, M3>,
    _marker: PhantomData<E>,
}

impl<E: CsrEdgeSlot, M1: Memory, M2: Memory, M3: Memory> DgapEdgeStore<E, M1, M2, M3> {
    pub fn new(mem: DgapGraphMemories<M1, M2, M3>) -> Self {
        Self {
            mem,
            _marker: PhantomData,
        }
    }

    pub fn memories(&self) -> &DgapGraphMemories<M1, M2, M3> {
        &self.mem
    }

    pub fn into_memories(self) -> DgapGraphMemories<M1, M2, M3> {
        self.mem
    }

    /// Format new regions: grow all three, write PMA mini headers, write `VCE` graph header on edges+log memory.
    pub fn format_new(
        &self,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
    ) -> Result<(), GrowFailed> {
        let edge_stride = E::EDGE_BYTES as u32;
        let log_entry_stride = dgap_log_entry_stride(edge_stride);
        let tree_height = crate::dgap::pma_meta::floor_log2_u32(segment_count.max(1));
        let h = DgapEdgeHeaderV1 {
            elem_capacity,
            segment_count,
            segment_size,
            tree_height,
            num_edges,
            edge_stride,
            max_log_entries: DGAP_DEFAULT_MAX_LOG_ENTRIES,
            log_entry_stride,
        };
        self.mem.grow_all_regions_for_header(&h)?;
        self.mem.write_header(&h);
        Ok(())
    }

    pub fn header(&self) -> Option<DgapEdgeHeaderV1> {
        self.mem.read_header()
    }

    /// Read one slab slot into `out`. Requires `out.len() >= edge_stride`.
    pub fn read_slot_into(&self, edge_stride: u32, slot: u64, out: &mut [u8]) {
        let n = edge_stride as usize;
        debug_assert!(out.len() >= n);
        self.mem.read_edge_slab(edge_stride, slot, &mut out[..n]);
    }

    pub fn read_slot(&self, edge_stride: u32, slot: u64) -> E {
        let n = edge_stride as usize;
        if n <= MAX_INLINE_EDGE {
            let mut buf = [0u8; MAX_INLINE_EDGE];
            self.read_slot_into(edge_stride, slot, &mut buf);
            E::read_from(&buf[..n])
        } else {
            let mut buf = vec![0u8; n];
            self.read_slot_into(edge_stride, slot, &mut buf);
            E::read_from(&buf)
        }
    }

    pub fn write_slot(
        &self,
        edge_stride: u32,
        slot: u64,
        value: E,
    ) -> Result<(), GrowFailed> {
        let mut buf = vec![0u8; edge_stride as usize];
        value.write_to(&mut buf);
        self.mem.write_edge_slab(edge_stride, slot, &buf)
    }

    pub fn read_actual(&self, j: usize) -> i64 {
        self.mem.read_actual(j)
    }

    pub fn read_total(&self, j: usize) -> i64 {
        self.mem.read_total(j)
    }

    pub fn write_actual(&self, j: usize, v: i64) {
        self.mem.write_actual(j, v);
    }

    pub fn write_total(&self, j: usize, v: i64) {
        self.mem.write_total(j, v);
    }

    fn read_log_entry_into(
        &self,
        h: &DgapEdgeHeaderV1,
        leaf_seg: u32,
        idx: u32,
        out: &mut [u8],
    ) -> (i32, i32) {
        debug_assert!(out.len() >= E::EDGE_BYTES);
        self.mem
            .read_log_entry_raw_into(h, leaf_seg, idx, &mut out[..E::EDGE_BYTES])
    }

    fn read_log_entry(&self, h: &DgapEdgeHeaderV1, leaf_seg: u32, idx: u32) -> (i32, i32, E) {
        let eb_len = E::EDGE_BYTES;
        if eb_len <= MAX_INLINE_EDGE {
            let mut eb = [0u8; MAX_INLINE_EDGE];
            let (prev, src) = self.read_log_entry_into(h, leaf_seg, idx, &mut eb);
            (prev, src, E::read_from(&eb[..eb_len]))
        } else {
            let mut eb = vec![0u8; eb_len];
            let (prev, src) = self.read_log_entry_into(h, leaf_seg, idx, &mut eb);
            (prev, src, E::read_from(&eb))
        }
    }

    fn write_log_entry(
        &self,
        h: &DgapEdgeHeaderV1,
        leaf_seg: u32,
        idx: u32,
        prev: i32,
        src_vid: i32,
        edge: E,
    ) -> Result<(), GrowFailed> {
        let n = E::EDGE_BYTES;
        if n <= MAX_INLINE_EDGE {
            let mut eb = [0u8; MAX_INLINE_EDGE];
            edge.write_to(&mut eb[..n]);
            self.mem
                .write_log_entry_raw(h, leaf_seg, idx, prev, src_vid, &eb[..n])
        } else {
            let mut eb = vec![0u8; n];
            edge.write_to(&mut eb);
            self.mem
                .write_log_entry_raw(h, leaf_seg, idx, prev, src_vid, &eb)
        }
    }

    fn release_log_segment(&self, h: &DgapEdgeHeaderV1, leaf_seg: u32) -> Result<(), GrowFailed> {
        let cur = self.mem.read_log_idx(h, leaf_seg);
        if cur <= 0 {
            return Ok(());
        }
        for i in 0..(cur as u32) {
            self.mem.zero_log_entry_slot(h, leaf_seg, i)?;
        }
        self.mem.write_log_idx(h, leaf_seg, 0);
        Ok(())
    }

    /// DGAP `out_neigh` slab span count (`onseg_edges`).
    fn onseg_edges<V, C>(
        col: &C,
        vid: usize,
        n: usize,
        elem_capacity: u64,
        v: &V,
    ) -> u32
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let deg = v.degree();
        if v.log_head() < 0 {
            return deg;
        }
        let next_start = if vid + 1 < n {
            col.col_get((vid + 1) as u64)
                .map(|x| x.base_slot_start())
                .unwrap_or(elem_capacity)
        } else {
            elem_capacity
        };
        let next_boundary = next_start.saturating_sub(1);
        let span = next_boundary
            .saturating_sub(v.base_slot_start())
            .saturating_add(1);
        u32::try_from(span.min(u64::from(u32::MAX))).unwrap_or(u32::MAX)
    }

    /// Outgoing edges in DGAP `Neighborhood` order (contiguous slab prefix, then overflow log chain).
    pub fn neighborhood_edges<V, C>(&self, col: &C, vid: usize) -> Result<Vec<E>, &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        self.collect_out_edges(col, vid)
    }

    /// Lazy [`NeighborhoodIter`] with `start_offset == 0`. See [`Self::try_neighborhood_iter_from`].
    pub fn try_neighborhood_iter<V, C>(
        &self,
        col: &C,
        vid: usize,
    ) -> Result<NeighborhoodIter<'_, E, M1, M2, M3>, &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        self.try_neighborhood_iter_from(col, vid, 0)
    }

    /// Lazy [`NeighborhoodIter`] (no up-front `Vec`); C++ `Neighborhood`-style traversal.
    ///
    /// `start_offset` is clamped to `degree` (same as `std::min(start_offset_, src_v->degree)` in `graph.h`).
    pub fn try_neighborhood_iter_from<V, C>(
        &self,
        col: &C,
        vid: usize,
        start_offset: usize,
    ) -> Result<NeighborhoodIter<'_, E, M1, M2, M3>, &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let h = self.header().ok_or("bad edge header")?;
        if h.edge_stride as usize > MAX_INLINE_EDGE || E::EDGE_BYTES > MAX_INLINE_EDGE {
            return Err("neighborhood iter: edge stride too large for inline buffer");
        }
        let n = col.col_len() as usize;
        if vid >= n {
            return Err("vertex out of range");
        }
        let v = col.col_get(vid as u64).ok_or("missing vertex")?;
        let d = v.degree() as usize;
        let skip = start_offset.min(d);
        let ons = Self::onseg_edges(col, vid, n, h.elem_capacity, &v) as usize;
        let n_slab = d.min(ons);
        let leaf = dgap_leaf_segment_id(vid, h.segment_size);
        let slab_start = skip.min(n_slab);
        let log_skip = skip.saturating_sub(n_slab);
        let log_edges_total = d.saturating_sub(n_slab);

        let mut log_i = v.log_head();
        for _ in 0..log_skip {
            if log_i < 0 {
                return Err("log chain short");
            }
            let li = log_i as u32;
            let (prev, _src, _e) = self.read_log_entry(&h, leaf, li);
            log_i = prev;
        }

        let log_remaining = log_edges_total.saturating_sub(log_skip);
        let remaining = d.saturating_sub(skip);

        Ok(NeighborhoodIter {
            store: self,
            h,
            stride: h.edge_stride,
            base: v.base_slot_start(),
            slab_cursor: slab_start,
            n_slab,
            leaf,
            log_i,
            log_remaining,
            remaining,
            scratch: [0u8; MAX_INLINE_EDGE],
        })
    }

    /// Alias for [`Self::neighborhood_edges`].
    pub fn collect_out_edges<V, C>(&self, col: &C, vid: usize) -> Result<Vec<E>, &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let it = self.try_neighborhood_iter(col, vid)?;
        let mut out = Vec::with_capacity(it.remaining);
        for x in it {
            out.push(x?);
        }
        Ok(out)
    }

    fn have_space_onseg<V, C>(
        col: &C,
        vid: usize,
        loc: u64,
        elem_capacity: u64,
    ) -> bool
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let n = col.col_len() as usize;
        if vid == n.saturating_sub(1) {
            elem_capacity > loc
        } else if vid + 1 < n {
            col.col_get((vid + 1) as u64)
                .map(|nv| nv.base_slot_start() > loc)
                .unwrap_or(false)
        } else {
            false
        }
    }

    /// Merge overflow logs into the CSR slab for vertices `[start_vertex, end_vertex)` and clear segment logs
    /// (DGAP `release_log` semantics). Used by [`Self::rebalance_weighted`].
    pub fn merge_logs_into_slab_for_window<V, C>(
        &self,
        col: &C,
        start_vertex: usize,
        end_vertex: usize,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let h = self.header().ok_or("bad edge header")?;
        let n = col.col_len() as usize;
        if start_vertex >= end_vertex || end_vertex > n {
            return Err("bad merge window");
        }
        let mut cur = col
            .col_get(start_vertex as u64)
            .ok_or("missing vertex")?
            .base_slot_start();
        let window_right_base = if end_vertex < n {
            col.col_get(end_vertex as u64)
                .ok_or("missing boundary vertex")?
                .base_slot_start()
        } else {
            h.elem_capacity
        };
        let mut packed: Vec<Vec<E>> = Vec::with_capacity(end_vertex - start_vertex);
        for vid in start_vertex..end_vertex {
            packed.push(self.collect_out_edges(col, vid)?);
        }
        for (k, vid) in (start_vertex..end_vertex).enumerate() {
            let edges = &packed[k];
            let row = col.col_get(vid as u64).ok_or("missing vertex")?;
            let d = edges.len();
            for (i, e) in edges.iter().enumerate() {
                self.write_slot(h.edge_stride, cur + i as u64, *e)
                    .map_err(|_| "write slot")?;
            }
            col.col_set(
                vid as u64,
                row.with_base_slot_start(cur)
                    .with_degree(d as u32)
                    .with_log_head(-1),
            );
            cur = cur.saturating_add(d as u64);
        }
        if cur > window_right_base {
            return Err("merge packed past window boundary");
        }
        let first_leaf = dgap_leaf_segment_id(start_vertex, h.segment_size);
        let last_leaf = dgap_leaf_segment_id(end_vertex.saturating_sub(1), h.segment_size);
        for ls in first_leaf..=last_leaf {
            self.release_log_segment(&h, ls).map_err(|_| "release log")?;
        }
        Ok(())
    }

    pub fn rebalance_weighted<V, C>(
        &self,
        col: &C,
        start_vertex: usize,
        end_vertex: usize,
        pma_idx: usize,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        self.merge_logs_into_slab_for_window(col, start_vertex, end_vertex)?;
        let h = self.header().ok_or("bad edge header")?;
        if h.edge_stride as usize != E::EDGE_BYTES {
            return Err("edge stride mismatch");
        }
        let n = col.col_len() as usize;
        if start_vertex >= n || end_vertex > n || end_vertex <= start_vertex {
            return Err("bad vertex window");
        }
        let mut win: Vec<V> = Vec::with_capacity(end_vertex - start_vertex);
        for i in start_vertex..end_vertex {
            win.push(
                col.col_get(i as u64)
                    .ok_or("missing vertex row for rebalance")?,
            );
        }
        let next_base = if end_vertex >= n {
            h.elem_capacity
        } else {
            col.col_get(end_vertex as u64)
                .ok_or("missing boundary vertex")?
                .base_slot_start()
        };
        let to = next_base;
        let from = win[0].base_slot_start();
        if to <= from {
            return Err("empty range");
        }
        let cap = (to - from) as i64;
        let total_at = self.mem.read_total(pma_idx);
        let actual_at = self.mem.read_actual(pma_idx);
        if total_at != cap {
            return Err("segment total mismatch");
        }
        let mut edges_full: Vec<E> = (0..h.elem_capacity)
            .map(|s| self.read_slot(h.edge_stride, s))
            .collect();
        crate::dgap::pma_meta::rebalance_weighted_window(
            &mut win,
            next_base,
            &mut edges_full,
            h.elem_capacity,
            total_at,
            actual_at,
        );
        for s in 0..h.elem_capacity {
            self.write_slot(h.edge_stride, s, edges_full[s as usize])
                .map_err(|_| "grow failed")?;
        }
        for li in 0..win.len() {
            col.col_set((start_vertex + li) as u64, win[li]);
        }
        Ok(())
    }

    pub fn set_num_edges_header(&self, n: u64) {
        self.mem.set_num_edges(n);
    }

    pub fn resize_double<V, C>(&self, col: &C) -> Result<(), &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let h = self.header().ok_or("bad edge header")?;
        let old_cap = h.elem_capacity;
        let new_cap = old_cap.checked_mul(2).ok_or("capacity overflow")?;
        let stride = h.edge_stride;
        let n = col.col_len() as usize;
        if n > 0 {
            self.merge_logs_into_slab_for_window(col, 0, n)?;
        }
        let h = self.header().ok_or("bad header")?;
        let mut edges: Vec<E> = (0..old_cap)
            .map(|s| self.read_slot(stride, s))
            .collect();
        let z = vec![0u8; stride as usize];
        while (edges.len() as u64) < new_cap {
            edges.push(E::read_from(&z));
        }
        let h2 = DgapEdgeHeaderV1 {
            elem_capacity: new_cap,
            ..h
        };
        let need = required_edges_and_log_bytes(&h2);
        self.mem
            .grow_edges_and_log_to(need)
            .map_err(|_| "grow failed")?;
        if n == 0 {
            self.mem.write_header(&h2);
            for s in 0..new_cap {
                self.write_slot(stride, s, edges[s as usize])
                    .map_err(|_| "write slot")?;
            }
            self.sync_pma_totals(col)?;
            self.sync_pma_actuals(col)?;
            return Ok(());
        }
        let mut vertices: Vec<V> = (0..n)
            .map(|i| {
                col.col_get(i as u64)
                    .ok_or("missing vertex during resize")
            })
            .collect::<Result<_, _>>()?;
        let from = vertices[0].base_slot_start();
        if from >= new_cap {
            return Err("invalid base under new capacity");
        }
        let cap_span = (new_cap - from) as i64;
        let sum_d: i64 = vertices.iter().map(|v| v.degree() as i64).sum();
        let nv = vertices.len();
        crate::dgap::pma_meta::rebalance_weighted(
            &mut vertices,
            &mut edges,
            0,
            nv,
            new_cap,
            cap_span,
            sum_d,
        );
        self.mem.write_header(&h2);
        for s in 0..new_cap {
            self.write_slot(stride, s, edges[s as usize])
                .map_err(|_| "write slot")?;
        }
        for i in 0..n {
            col.col_set(i as u64, vertices[i]);
        }
        self.mem
            .zero_log_partition(&h2)
            .map_err(|_| "zero log partition")?;
        self.sync_pma_totals(col)?;
        self.sync_pma_actuals(col)?;
        Ok(())
    }

    fn try_insert_into_log<V, C>(
        &self,
        col: &C,
        vid: usize,
        edge: E,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let h = self.header().ok_or("bad edge header")?;
        let leaf = dgap_leaf_segment_id(vid, h.segment_size);
        let idx = self.mem.read_log_idx(&h, leaf);
        if idx >= h.max_log_entries as i32 {
            return Err("log full");
        }
        let v = col.col_get(vid as u64).ok_or("missing vertex")?;
        let prev = v.log_head();
        let entry_idx = idx as u32;
        self.write_log_entry(&h, leaf, entry_idx, prev, vid as i32, edge)
            .map_err(|_| "write log")?;
        self.mem.write_log_idx(&h, leaf, idx + 1);
        col.col_set(
            vid as u64,
            v.with_log_head(idx)
                .with_degree(v.degree() + 1),
        );
        let ne = self.mem.read_num_edges();
        self.mem.set_num_edges(ne.saturating_add(1));
        Ok(())
    }

    fn dgap_insert_once<V, C>(&self, col: &C, vid: usize, edge: E) -> Result<(), &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let h = self.header().ok_or("bad edge header")?;
        let n = col.col_len() as usize;
        if vid >= n {
            return Err("vertex out of range");
        }
        let v = col.col_get(vid as u64).ok_or("missing vertex")?;
        let loc = v.base_slot_start().saturating_add(v.degree() as u64);
        if Self::have_space_onseg(col, vid, loc, h.elem_capacity) {
            self.write_slot(h.edge_stride, loc, edge)
                .map_err(|_| "write slot")?;
            col.col_set(vid as u64, v.with_degree(v.degree() + 1));
            let ne = self.mem.read_num_edges();
            self.mem.set_num_edges(ne.saturating_add(1));
            return Ok(());
        }
        let leaf = dgap_leaf_segment_id(vid, h.segment_size);
        let idx = self.mem.read_log_idx(&h, leaf);
        if idx >= h.max_log_entries as i32 {
            return Err("log full");
        }
        self.try_insert_into_log(col, vid, edge)
    }

    /// Insert one edge (slab or DGAP log), sync PMA totals/actuals, rebalance/resize until noop.
    pub fn insert_edge_and_maintain<V, C>(
        &self,
        col: &C,
        vid: usize,
        edge: E,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        const MAX_TRIES: usize = 64;
        for _try in 0..MAX_TRIES {
            let h = self.header().ok_or("bad edge header")?;
            let n = col.col_len() as usize;
            if vid >= n {
                return Err("vertex out of range");
            }
            let leaf = dgap_leaf_segment_id(vid, h.segment_size);
            let idx = self.mem.read_log_idx(&h, leaf);
            if idx >= h.max_log_entries as i32 {
                let left_index = (vid / h.segment_size as usize) * h.segment_size as usize;
                let right_index = ((left_index + h.segment_size as usize).min(n)).min(n);
                self.merge_logs_into_slab_for_window(col, left_index, right_index)?;
                self.sync_pma_totals(col)?;
                self.sync_pma_actuals(col)?;
                continue;
            }
            match self.dgap_insert_once(col, vid, edge) {
                Ok(()) => {
                    self.sync_pma_totals(col).map_err(|_| "sync totals")?;
                    self.sync_pma_actuals(col).map_err(|_| "sync actuals")?;
                    self.maintain_rebalance_loop(col, vid)?;
                    return Ok(());
                }
                Err("log full") => {
                    let left_index = (vid / h.segment_size as usize) * h.segment_size as usize;
                    let right_index = (left_index + h.segment_size as usize).min(n);
                    self.merge_logs_into_slab_for_window(col, left_index, right_index)?;
                    self.sync_pma_totals(col)?;
                    self.sync_pma_actuals(col)?;
                }
                Err(e) if e == "vertex out of range" || e == "missing vertex" => {
                    return Err(e);
                }
                Err("write slot") => {
                    self.resize_double(col)?;
                }
                Err(e) => return Err(e),
            }
        }
        Err("insert maintenance tries exhausted")
    }

    pub(crate) fn maintain_rebalance_loop<V, C>(&self, col: &C, vid: usize) -> Result<(), &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        for _ in 0..32 {
            let h = self.header().ok_or("bad header")?;
            let sc = h.segment_count as usize;
            let len = sc * 2;
            let mut actual = vec![0i64; len];
            let mut total = vec![0i64; len];
            for j in 0..len {
                actual[j] = self.read_actual(j);
                total[j] = self.read_total(j);
            }
            let num_v = col.col_len() as usize;
            match rebalance_decision(
                vid as u32,
                h.segment_size,
                h.segment_count,
                num_v,
                h.tree_height,
                &actual,
                &total,
            ) {
                RebalanceDecision::Noop => return Ok(()),
                RebalanceDecision::RebalanceWindow {
                    left_vertex,
                    right_vertex,
                    pma_idx,
                } => {
                    self.rebalance_weighted(col, left_vertex, right_vertex, pma_idx)?;
                    self.sync_pma_totals(col)?;
                    self.sync_pma_actuals(col)?;
                }
                RebalanceDecision::ResizeNeeded => {
                    self.resize_double(col)?;
                }
            }
        }
        Err("rebalance maintenance limit")
    }

    /// Maximum vertex rows such that every `vid` satisfies `dgap_leaf_segment_id(vid, segment_size) < segment_count`.
    #[inline]
    pub fn max_vertex_slots(segment_count: u32, segment_size: u32) -> u64 {
        let ss = segment_size.max(1) as u64;
        (segment_count as u64).saturating_mul(ss)
    }

    /// `current_len` is [`CsrVertexColumn::col_len`] **before** push; the new row will get `vid == current_len`.
    #[inline]
    pub fn check_vertex_append_cap(
        current_len: u64,
        segment_count: u32,
        segment_size: u32,
    ) -> Result<(), &'static str> {
        let cap = Self::max_vertex_slots(segment_count, segment_size);
        if current_len >= cap {
            return Err("vertex column cap exceeded (segment_count * segment_size)");
        }
        Ok(())
    }

    /// `max_v (base + degree)` over existing rows.
    pub fn slab_occupied_tail<V, C>(&self, col: &C) -> Result<u64, &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let n = col.col_len() as usize;
        let mut end = 0u64;
        for i in 0..n {
            let v = col.col_get(i as u64).ok_or("missing vertex row")?;
            let b = v.base_slot_start();
            let d = v.degree() as u64;
            end = end.max(b.saturating_add(d));
        }
        Ok(end)
    }

    /// Next `base_slot_start` for a **new tail** vertex row (before `col_push_back`).
    ///
    /// Uses the occupied slab tail plus a one-slot bump when the current last vertex has `degree == 0`
    /// and would otherwise share the same insertion cursor as another empty tail (see `dgap_insert_once`).
    pub fn slab_append_base_slot<V, C>(&self, col: &C) -> Result<u64, &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let n = col.col_len() as usize;
        if n == 0 {
            return Ok(0);
        }
        let mut expected = self.slab_occupied_tail(col)?;
        let last = col
            .col_get((n - 1) as u64)
            .ok_or("missing last vertex row")?;
        if last.degree() == 0 && expected == last.base_slot_start() {
            expected = expected.saturating_add(1);
        }
        Ok(expected)
    }

    pub fn sync_pma_totals<V, C>(&self, col: &C) -> Result<(), &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let h = self.header().ok_or("bad header")?;
        let sc = h.segment_count as usize;
        let mut total = vec![0i64; sc * 2];
        crate::dgap::pma_meta::recount_segment_total_column(
            col,
            col.col_len(),
            h.segment_count,
            h.segment_size,
            h.elem_capacity,
            &mut total,
        );
        for j in 0..total.len() {
            self.write_total(j, total[j]);
        }
        Ok(())
    }

    pub fn sync_pma_actuals<V, C>(&self, col: &C) -> Result<(), &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let h = self.header().ok_or("bad header")?;
        let sc = h.segment_count as usize;
        let mut actual = vec![0i64; sc * 2];
        crate::dgap::pma_meta::recount_segment_actual_column(
            col,
            col.col_len(),
            h.segment_count,
            h.segment_size,
            &mut actual,
        );
        for j in 0..actual.len() {
            self.write_actual(j, actual[j]);
        }
        Ok(())
    }
}
