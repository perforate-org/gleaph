//! VCSR + DGAP overflow edge region (`M_e`): header, PMA meta, CSR slab, per-leaf log pools.
//!
//! Insert path follows [`gleaph-old/reference/DGAP/dgap/src/graph.h`](../../../../gleaph-old/reference/DGAP/dgap/src/graph.h)
//! `do_insertion` / `insert_into_log` / `have_space_onseg`.

use std::marker::PhantomData;

use ic_stable_structures::Memory;

use crate::csr::vertex_column::CsrVertexColumn;
use crate::layout::edge_region::{
    dgap_leaf_segment_id, dgap_log_entry_stride, edge_slot_offset, log_entry_offset, read_actual,
    read_log_segment_idx, read_total as read_total_mem, required_byte_len, write_actual,
    write_log_segment_idx, write_total as write_total_mem, VcsrEdgeHeaderV1,
    DGAP_DEFAULT_MAX_LOG_ENTRIES,
};
use crate::memory_util::{memory_byte_len, read_i32_le, read_u64_le, safe_write, write_i32_le, write_u64_le, GrowFailed};
use crate::traits::{CsrEdgeSlot, CsrVertex};
use crate::vcsr::pma_meta::{rebalance_decision, RebalanceDecision};

/// Owns `M_e` for DGAP layout (v2 header).
pub struct VcsrEdgeStore<E: CsrEdgeSlot, M: Memory> {
    memory: M,
    _marker: PhantomData<E>,
}

impl<E: CsrEdgeSlot, M: Memory> VcsrEdgeStore<E, M> {
    pub fn new(memory: M) -> Self {
        Self {
            memory,
            _marker: PhantomData,
        }
    }

    pub fn memory(&self) -> &M {
        &self.memory
    }

    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Format new region: v2 header, PMA arrays, slab, log idx, log pool (zeroed).
    pub fn format_new(
        &self,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
    ) -> Result<(), GrowFailed> {
        let edge_stride = E::EDGE_BYTES as u32;
        let log_entry_stride = dgap_log_entry_stride(edge_stride);
        let tree_height = crate::vcsr::pma_meta::floor_log2_u32(segment_count.max(1));
        let h = VcsrEdgeHeaderV1 {
            elem_capacity,
            segment_count,
            segment_size,
            tree_height,
            num_edges,
            edge_stride,
            max_log_entries: DGAP_DEFAULT_MAX_LOG_ENTRIES,
            log_entry_stride,
        };
        let need = required_byte_len(&h);
        safe_write(&self.memory, 0, &[0u8; 64])?;
        h.write(&self.memory);
        let meta_end = crate::layout::edge_region::csr_slab_base_offset(segment_count);
        safe_write(
            &self.memory,
            64,
            &vec![0u8; (meta_end - 64) as usize],
        )?;
        safe_write(&self.memory, meta_end, &vec![0u8; (need - meta_end) as usize])?;
        Ok(())
    }

    pub fn header(&self) -> Option<VcsrEdgeHeaderV1> {
        VcsrEdgeHeaderV1::read(&self.memory)
    }

    pub fn read_slot(&self, segment_count: u32, edge_stride: u32, slot: u64) -> E {
        let off = edge_slot_offset(segment_count, edge_stride, slot);
        let mut buf = vec![0u8; edge_stride as usize];
        self.memory.read(off, &mut buf);
        E::read_from(&buf)
    }

    pub fn write_slot(
        &self,
        segment_count: u32,
        edge_stride: u32,
        slot: u64,
        value: E,
    ) -> Result<(), GrowFailed> {
        let off = edge_slot_offset(segment_count, edge_stride, slot);
        let mut buf = vec![0u8; edge_stride as usize];
        value.write_to(&mut buf);
        safe_write(&self.memory, off, &buf)
    }

    pub fn read_actual(&self, segment_count: u32, j: usize) -> i64 {
        read_actual(&self.memory, segment_count, j)
    }

    pub fn read_total(&self, segment_count: u32, j: usize) -> i64 {
        read_total_mem(&self.memory, segment_count, j)
    }

    pub fn write_actual(&self, segment_count: u32, j: usize, v: i64) {
        write_actual(&self.memory, segment_count, j, v);
    }

    pub fn write_total(&self, segment_count: u32, j: usize, v: i64) {
        write_total_mem(&self.memory, segment_count, j, v);
    }

    fn read_log_entry(&self, h: &VcsrEdgeHeaderV1, leaf_seg: u32, idx: u32) -> (i32, i32, E) {
        let off = log_entry_offset(
            h.segment_count,
            h.edge_stride,
            h.elem_capacity,
            h.log_entry_stride,
            h.max_log_entries,
            leaf_seg,
            idx,
        );
        let prev = read_i32_le(&self.memory, off);
        let src = read_i32_le(&self.memory, off + 4);
        let mut eb = vec![0u8; E::EDGE_BYTES];
        self.memory.read(off + 8, &mut eb);
        (prev, src, E::read_from(&eb))
    }

    fn write_log_entry(
        &self,
        h: &VcsrEdgeHeaderV1,
        leaf_seg: u32,
        idx: u32,
        prev: i32,
        src_vid: i32,
        edge: E,
    ) -> Result<(), GrowFailed> {
        let off = log_entry_offset(
            h.segment_count,
            h.edge_stride,
            h.elem_capacity,
            h.log_entry_stride,
            h.max_log_entries,
            leaf_seg,
            idx,
        );
        write_i32_le(&self.memory, off, prev);
        write_i32_le(&self.memory, off + 4, src_vid);
        let mut eb = vec![0u8; h.log_entry_stride as usize];
        edge.write_to(&mut eb[..E::EDGE_BYTES]);
        safe_write(&self.memory, off + 8, &eb[..E::EDGE_BYTES])?;
        Ok(())
    }

    fn release_log_segment(&self, h: &VcsrEdgeHeaderV1, leaf_seg: u32) -> Result<(), GrowFailed> {
        let cur = read_log_segment_idx(&self.memory, h, leaf_seg);
        if cur <= 0 {
            return Ok(());
        }
        let z = vec![0u8; h.log_entry_stride as usize];
        for i in 0..(cur as u32) {
            let off = log_entry_offset(
                h.segment_count,
                h.edge_stride,
                h.elem_capacity,
                h.log_entry_stride,
                h.max_log_entries,
                leaf_seg,
                i,
            );
            safe_write(&self.memory, off, &z)?;
        }
        write_log_segment_idx(&self.memory, h, leaf_seg, 0);
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
        E: Clone,
    {
        self.collect_out_edges(col, vid)
    }

    /// Alias for [`Self::neighborhood_edges`].
    pub fn collect_out_edges<V, C>(&self, col: &C, vid: usize) -> Result<Vec<E>, &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
        E: Clone,
    {
        let h = self.header().ok_or("bad edge header")?;
        let n = col.col_len() as usize;
        if vid >= n {
            return Err("vertex out of range");
        }
        let v = col.col_get(vid as u64).ok_or("missing vertex")?;
        let d = v.degree() as usize;
        let ons = Self::onseg_edges(col, vid, n, h.elem_capacity, &v) as usize;
        let n_slab = d.min(ons);
        let mut out = Vec::with_capacity(d);
        let base = v.base_slot_start();
        for i in 0..n_slab {
            out.push(self.read_slot(h.segment_count, h.edge_stride, base + i as u64));
        }
        let mut remaining = d - n_slab;
        let h_copy = h.clone();
        let mut log_i = v.log_head();
        let leaf = dgap_leaf_segment_id(vid, h.segment_size);
        while remaining > 0 {
            if log_i < 0 {
                return Err("log chain short");
            }
            let li = log_i as u32;
            let (prev, _src, e) = self.read_log_entry(&h_copy, leaf, li);
            out.push(e);
            log_i = prev;
            remaining -= 1;
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
        V: CsrVertex + Copy,
        C: CsrVertexColumn<V>,
        E: Clone + Copy,
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
                self.write_slot(
                    h.segment_count,
                    h.edge_stride,
                    cur + i as u64,
                    *e,
                )
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
        V: CsrVertex + Copy,
        C: CsrVertexColumn<V>,
        E: CsrEdgeSlot + Clone + Copy,
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
        let total_at = read_total_mem(&self.memory, h.segment_count, pma_idx);
        let actual_at = read_actual(&self.memory, h.segment_count, pma_idx);
        if total_at != cap {
            return Err("segment total mismatch");
        }
        let mut edges_full: Vec<E> = (0..h.elem_capacity)
            .map(|s| self.read_slot(h.segment_count, h.edge_stride, s))
            .collect();
        crate::vcsr::pma_meta::rebalance_weighted_window(
            &mut win,
            next_base,
            &mut edges_full,
            h.elem_capacity,
            total_at,
            actual_at,
        );
        for s in 0..h.elem_capacity {
            self.write_slot(h.segment_count, h.edge_stride, s, edges_full[s as usize])
                .map_err(|_| "grow failed")?;
        }
        for li in 0..win.len() {
            col.col_set((start_vertex + li) as u64, win[li]);
        }
        Ok(())
    }

    pub fn set_num_edges_header(&self, n: u64) {
        write_u64_le(&self.memory, 32, n);
    }

    pub fn resize_double<V, C>(&self, col: &C) -> Result<(), &'static str>
    where
        V: CsrVertex + Copy,
        C: CsrVertexColumn<V>,
        E: Clone + Copy,
    {
        let h = self.header().ok_or("bad edge header")?;
        let old_cap = h.elem_capacity;
        let new_cap = old_cap.checked_mul(2).ok_or("capacity overflow")?;
        let sc = h.segment_count;
        let stride = h.edge_stride;
        let n = col.col_len() as usize;
        if n > 0 {
            self.merge_logs_into_slab_for_window(col, 0, n)?;
        }
        let h = self.header().ok_or("bad header")?;
        let mut edges: Vec<E> = (0..old_cap)
            .map(|s| self.read_slot(sc, stride, s))
            .collect();
        let z = vec![0u8; stride as usize];
        while (edges.len() as u64) < new_cap {
            edges.push(E::read_from(&z));
        }
        let h2 = VcsrEdgeHeaderV1 {
            elem_capacity: new_cap,
            ..h.clone()
        };
        let need = required_byte_len(&h2);
        let cur = memory_byte_len(&self.memory);
        if need > cur {
            safe_write(
                &self.memory,
                cur,
                &vec![0u8; (need - cur) as usize],
            )
            .map_err(|_| "grow failed")?;
        }
        if n == 0 {
            h2.write(&self.memory);
            for s in 0..new_cap {
                self.write_slot(sc, stride, s, edges[s as usize])
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
        crate::vcsr::pma_meta::rebalance_weighted(
            &mut vertices,
            &mut edges,
            0,
            nv,
            new_cap,
            cap_span,
            sum_d,
        );
        h2.write(&self.memory);
        for s in 0..new_cap {
            self.write_slot(sc, stride, s, edges[s as usize])
                .map_err(|_| "write slot")?;
        }
        for i in 0..n {
            col.col_set(i as u64, vertices[i]);
        }
        let idx_base = crate::layout::edge_region::dgap_log_idx_base_offset(
            h2.segment_count,
            h2.edge_stride,
            h2.elem_capacity,
        );
        safe_write(
            &self.memory,
            idx_base,
            &vec![0u8; (h2.segment_count as usize).saturating_mul(4)],
        )
        .map_err(|_| "zero log idx")?;
        let pool_base = crate::layout::edge_region::dgap_log_pool_base_offset(
            h2.segment_count,
            h2.edge_stride,
            h2.elem_capacity,
        );
        let pool_bytes = need.saturating_sub(pool_base);
        safe_write(&self.memory, pool_base, &vec![0u8; pool_bytes as usize])
            .map_err(|_| "zero log pool")?;
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
        V: CsrVertex + Copy,
        C: CsrVertexColumn<V>,
        E: Copy,
    {
        let h = self.header().ok_or("bad edge header")?;
        let leaf = dgap_leaf_segment_id(vid, h.segment_size);
        let idx = read_log_segment_idx(&self.memory, &h, leaf);
        if idx >= h.max_log_entries as i32 {
            return Err("log full");
        }
        let v = col.col_get(vid as u64).ok_or("missing vertex")?;
        let prev = v.log_head();
        let entry_idx = idx as u32;
        self.write_log_entry(&h, leaf, entry_idx, prev, vid as i32, edge)
            .map_err(|_| "write log")?;
        write_log_segment_idx(&self.memory, &h, leaf, idx + 1);
        col.col_set(
            vid as u64,
            v.with_log_head(idx)
                .with_degree(v.degree() + 1),
        );
        let ne = read_u64_le(&self.memory, 32);
        write_u64_le(&self.memory, 32, ne.saturating_add(1));
        Ok(())
    }

    fn dgap_insert_once<V, C>(&self, col: &C, vid: usize, edge: E) -> Result<(), &'static str>
    where
        V: CsrVertex + Copy,
        C: CsrVertexColumn<V>,
        E: Copy,
    {
        let h = self.header().ok_or("bad edge header")?;
        let n = col.col_len() as usize;
        if vid >= n {
            return Err("vertex out of range");
        }
        let v = col.col_get(vid as u64).ok_or("missing vertex")?;
        let loc = v.base_slot_start().saturating_add(v.degree() as u64);
        if Self::have_space_onseg(col, vid, loc, h.elem_capacity) {
            self.write_slot(h.segment_count, h.edge_stride, loc, edge)
                .map_err(|_| "write slot")?;
            col.col_set(vid as u64, v.with_degree(v.degree() + 1));
            let ne = read_u64_le(&self.memory, 32);
            write_u64_le(&self.memory, 32, ne.saturating_add(1));
            return Ok(());
        }
        let leaf = dgap_leaf_segment_id(vid, h.segment_size);
        let idx = read_log_segment_idx(&self.memory, &h, leaf);
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
        V: CsrVertex + Copy,
        C: CsrVertexColumn<V>,
        E: Clone + Copy,
    {
        const MAX_TRIES: usize = 64;
        for _try in 0..MAX_TRIES {
            let h = self.header().ok_or("bad edge header")?;
            let n = col.col_len() as usize;
            if vid >= n {
                return Err("vertex out of range");
            }
            let leaf = dgap_leaf_segment_id(vid, h.segment_size);
            let idx = read_log_segment_idx(&self.memory, &h, leaf);
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
        V: CsrVertex + Copy,
        C: CsrVertexColumn<V>,
        E: Clone + Copy,
    {
        for _ in 0..32 {
            let h = self.header().ok_or("bad header")?;
            let sc = h.segment_count as usize;
            let len = sc * 2;
            let mut actual = vec![0i64; len];
            let mut total = vec![0i64; len];
            for j in 0..len {
                actual[j] = self.read_actual(h.segment_count, j);
                total[j] = self.read_total(h.segment_count, j);
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
}

impl<E: CsrEdgeSlot, M: Memory> VcsrEdgeStore<E, M> {
    pub fn sync_pma_totals<V, C>(&self, col: &C) -> Result<(), &'static str>
    where
        V: CsrVertex,
        C: CsrVertexColumn<V>,
    {
        let h = self.header().ok_or("bad header")?;
        let sc = h.segment_count as usize;
        let mut total = vec![0i64; sc * 2];
        crate::vcsr::pma_meta::recount_segment_total_column(
            col,
            col.col_len(),
            h.segment_count,
            h.segment_size,
            h.elem_capacity,
            &mut total,
        );
        for j in 0..total.len() {
            self.write_total(h.segment_count, j, total[j]);
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
        crate::vcsr::pma_meta::recount_segment_actual_column(
            col,
            col.col_len(),
            h.segment_count,
            h.segment_size,
            &mut actual,
        );
        for j in 0..actual.len() {
            self.write_actual(h.segment_count, j, actual[j]);
        }
        Ok(())
    }
}
