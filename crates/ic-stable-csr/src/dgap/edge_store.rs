//! DGAP edge region (`M_e`): two [`Memory`] regions behind [`DgapGraphMemories`] (CSR slab + per-leaf overflow logs).
//!
//! Persistent layout (per-memory offsets, `ic-stable_structures`-style diagrams): [`crate::layout::dgap`].
//!
//! **Phase C profiling:** enable this crate’s `canbench-rs` feature so [`DgapEdgeStore::remove_slab_edge_at_local_index_physically`]
//! records `canbench_rs::bench_scope` splits (`dgap_remove_slab_*`). The base pass uses a binary-search split when dense
//! `base_slot_start` is non-decreasing (see tests in `csr_insert_maintain.rs`). PocketIC + `canbench` live in
//! `crates/ic-stable-csr-canbench` (`README.md`, `canbench_results.yml`).
//!
//! Insert path follows [`gleaph-old/reference/DGAP/dgap/src/graph.h`](../../../../gleaph-old/reference/DGAP/dgap/src/graph.h)
//! `do_insertion` / `insert_into_log` / `have_space_onseg`.

use std::collections::BTreeSet;
use std::iter::{ExactSizeIterator, FusedIterator};
use std::marker::PhantomData;

use ic_stable_slot_map::SlotMap;
use ic_stable_structures::Memory;

#[inline]
fn vtx32(i: usize) -> Result<u32, &'static str> {
    u32::try_from(i).map_err(|_| "vertex id out of u32 range")
}

#[inline]
fn col_get<V, M>(col: &SlotMap<V, M>, i: usize) -> Result<V, &'static str>
where
    V: CsrVertex,
    M: Memory,
{
    col.get_dense(vtx32(i)?).ok_or("missing vertex")
}

#[inline]
fn col_set<V, M>(col: &SlotMap<V, M>, i: usize, v: V) -> Result<(), &'static str>
where
    V: CsrVertex,
    M: Memory,
{
    col.set_dense(vtx32(i)?, &v)
        .map_err(|_| "vertex column set failed")
}

#[inline]
fn push_remove_slab_pma_dirty_segments(segs: &mut BTreeSet<usize>, idx: usize, ss: usize, sc: usize) {
    let seg = idx / ss;
    if seg < sc {
        segs.insert(seg);
    }
    if idx > 0 && idx % ss == 0 {
        let prev = seg.saturating_sub(1);
        if prev < sc {
            segs.insert(prev);
        }
    }
}

/// Smallest dense vertex index `j` with `base_slot_start(j) > remove_pos`, or `n` if none.
///
/// This matches a linear scan only when `base_slot_start` is **non-decreasing** along dense
/// order `0..n-1` (integration tests: `tests/common/mod.rs`, `csr_insert_maintain.rs`). The remove-slab path uses it
/// to skip `col_set` base decrements on the prefix `[0, L)` where every base is `<= remove_pos`.
#[inline]
fn first_dense_vertex_base_gt<V, M>(
    col: &SlotMap<V, M>,
    n: usize,
    remove_pos: u64,
) -> Result<usize, &'static str>
where
    V: CsrVertex,
    M: Memory,
{
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let b = col_get(col, mid)?.base_slot_start();
        if b > remove_pos {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(lo)
}

/// When feature `strict-dgap-invariants` is enabled, [`DgapEdgeStore::remove_slab_edge_at_local_index_physically`]
/// runs this before [`first_dense_vertex_base_gt`] (full column read, `O(n)`).
#[cfg(feature = "strict-dgap-invariants")]
fn strict_check_dense_bases_monotone_remove_slab<V, M>(
    col: &SlotMap<V, M>,
    n: usize,
) -> Result<(), &'static str>
where
    V: CsrVertex,
    M: Memory,
{
    if n < 2 {
        return Ok(());
    }
    let mut prev = col_get(col, 0)?.base_slot_start();
    for j in 1..n {
        let b = col_get(col, j)?.base_slot_start();
        if prev > b {
            return Err("strict-dgap-invariants: dense base_slot_start not non-decreasing (remove_slab binary search invalid)");
        }
        prev = b;
    }
    Ok(())
}

use crate::dgap::dgap_graph_memories::DgapGraphMemories;
use crate::dgap::edge_pma_stride::EdgePmaCountsStride;
use crate::dgap::pma_meta::{
    RebalanceDecision, rebalance_decision_with_reader, rebalance_weighted_window_rel,
};
use crate::layout::dgap::SegmentEdgeCounts;
use crate::layout::dgap::{
    DGAP_DEFAULT_MAX_LOG_ENTRIES, DgapEdgeHeaderV1, dgap_leaf_segment_id, dgap_log_entry_stride,
    required_edges_and_log_bytes,
};
use crate::memory_util::GrowFailed;
use crate::traits::{CsrEdge, CsrEdgeSlotTombstoneScan, CsrEdgeTombstone, CsrVertex};

/// Stack / inline scratch cap for edge bytes (slab read, log entry payload, [`NeighborhoodIter`]).
const MAX_INLINE_EDGE: usize = 64;

/// Lazy outgoing neighborhood in DGAP `Neighborhood` order: contiguous **on-segment** slab slots,
/// then overflow **log** chain (same walk as C++ `CSRGraph::Neighborhood` / [`DgapEdgeStore::collect_out_edges`]).
///
/// `start_offset` is applied like C++: `min(requested, degree)` global edges are skipped before the first yield.
///
/// Yields [`Result`] so a truncated log chain surfaces as `Err("log chain short")` (then the iterator fuses).
pub struct NeighborhoodIter<'a, E: CsrEdge, M1: Memory, M2: Memory> {
    store: &'a DgapEdgeStore<E, M1, M2>,
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

impl<'a, E: CsrEdge, M1: Memory, M2: Memory> Iterator
    for NeighborhoodIter<'a, E, M1, M2>
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
        let (prev, _src) =
            self.store
                .read_log_entry_into(&self.h, self.leaf, li, &mut self.scratch[..eb_len]);
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

impl<'a, E: CsrEdge, M1: Memory, M2: Memory> ExactSizeIterator
    for NeighborhoodIter<'a, E, M1, M2>
{
}

impl<'a, E: CsrEdge, M1: Memory, M2: Memory> FusedIterator
    for NeighborhoodIter<'a, E, M1, M2>
{
}

/// Owns the two-`Memory` DGAP edge bundle (`M_e`).
pub struct DgapEdgeStore<E: CsrEdge, M1, M2> {
    mem: DgapGraphMemories<M1, M2>,
    _marker: PhantomData<E>,
}

impl<E: CsrEdge, M1: Memory, M2: Memory> DgapEdgeStore<E, M1, M2> {
    pub fn new(mem: DgapGraphMemories<M1, M2>) -> Self {
        Self {
            mem,
            _marker: PhantomData,
        }
    }

    pub fn memories(&self) -> &DgapGraphMemories<M1, M2> {
        &self.mem
    }

    pub fn into_memories(self) -> DgapGraphMemories<M1, M2> {
        self.mem
    }

    #[inline]
    fn pma_stride(&self) -> u64 {
        E::pma_counts_stride_bytes()
    }

    /// Format new regions: grow PMA + edges+log, write `SEC` / `VCE` headers.
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
            slab_occupied_tail: 0,
        };
        self.mem
            .grow_all_regions_for_header(&h, E::pma_counts_stride_bytes())?;
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

    pub fn write_slot(&self, edge_stride: u32, slot: u64, value: E) -> Result<(), GrowFailed> {
        let mut buf = vec![0u8; edge_stride as usize];
        value.write_to(&mut buf);
        self.mem.write_edge_slab(edge_stride, slot, &buf)
    }

    /// One PMA tree node: `actual` / `total` / `tombstone` (tombstone is 0 when `stride` is 16).
    pub fn read_segment_edge_counts(&self, j: usize) -> SegmentEdgeCounts {
        self.mem.read_segment_edge_counts(j, self.pma_stride())
    }

    /// Incremental SEC update for `owner_vid`'s DGAP leaf: apply deltas at the leaf, then re-aggregate ancestors.
    ///
    /// Stride-16 stores ignore `d_tombstone` on disk (see [`crate::dgap::pma_meta::propagate_segment_edge_counts_leaf_delta`]).
    pub fn bump_segment_edge_counts_leaf_delta(
        &self,
        owner_vid: usize,
        d_actual: i64,
        d_total: i64,
        d_tombstone: i64,
    ) -> Result<(), &'static str> {
        let h = self.header().ok_or("bad header")?;
        let seg_id = dgap_leaf_segment_id(owner_vid, h.segment_size) as usize;
        crate::dgap::pma_meta::propagate_segment_edge_counts_leaf_delta(
            &self.mem.segment_edge_counts,
            self.pma_stride(),
            h.segment_count,
            seg_id,
            d_actual,
            d_total,
            d_tombstone,
        )
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
    fn onseg_edges<V, Mvs>(col: &SlotMap<V, Mvs>, vid: usize, n: usize, elem_capacity: u64, v: &V) -> u32
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let deg = v.degree();
        if v.log_head() < 0 {
            return deg;
        }
        let next_start = if vid + 1 < n {
            col_get(col, vid + 1)
                .ok()
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
    pub fn neighborhood_edges<V, Mvs>(&self, col: &SlotMap<V, Mvs>, vid: usize) -> Result<Vec<E>, &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        self.collect_out_edges(col, vid)
    }

    /// Lazy [`NeighborhoodIter`] with `start_offset == 0`. See [`Self::try_neighborhood_iter_from`].
    pub fn try_neighborhood_iter<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        vid: usize,
    ) -> Result<NeighborhoodIter<'_, E, M1, M2>, &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        self.try_neighborhood_iter_from(col, vid, 0)
    }

    /// Lazy [`NeighborhoodIter`] (no up-front `Vec`); C++ `Neighborhood`-style traversal.
    ///
    /// `start_offset` is clamped to `degree` (same as `std::min(start_offset_, src_v->degree)` in `graph.h`).
    pub fn try_neighborhood_iter_from<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        vid: usize,
        start_offset: usize,
    ) -> Result<NeighborhoodIter<'_, E, M1, M2>, &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let h = self.header().ok_or("bad edge header")?;
        if h.edge_stride as usize > MAX_INLINE_EDGE || E::EDGE_BYTES > MAX_INLINE_EDGE {
            return Err("neighborhood iter: edge stride too large for inline buffer");
        }
        let n = col.len() as usize;
        if vid >= n {
            return Err("vertex out of range");
        }
        let v = col_get(col, vid)?;
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
    pub fn collect_out_edges<V, Mvs>(&self, col: &SlotMap<V, Mvs>, vid: usize) -> Result<Vec<E>, &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let it = self.try_neighborhood_iter(col, vid)?;
        let mut out = Vec::with_capacity(it.remaining);
        for x in it {
            out.push(x?);
        }
        Ok(out)
    }

    fn have_space_onseg<V, Mvs>(col: &SlotMap<V, Mvs>, vid: usize, loc: u64, elem_capacity: u64) -> bool
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let n = col.len() as usize;
        if vid == n.saturating_sub(1) {
            elem_capacity > loc
        } else if vid + 1 < n {
            col_get(col, vid + 1)
                .ok()
                .map(|nv| nv.base_slot_start() > loc)
                .unwrap_or(false)
        } else {
            false
        }
    }

    /// Merge overflow logs into the CSR slab for vertices `[start_vertex, end_vertex)` and clear segment logs
    /// (DGAP `release_log` semantics). Used by [`Self::rebalance_weighted`].
    pub fn merge_logs_into_slab_for_window<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        start_vertex: usize,
        end_vertex: usize,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let h = self.header().ok_or("bad edge header")?;
        let n = col.len() as usize;
        if start_vertex >= end_vertex || end_vertex > n {
            return Err("bad merge window");
        }
        let mut cur = col_get(col, start_vertex)?.base_slot_start();
        let window_right_base = if end_vertex < n {
            col_get(col, end_vertex)?.base_slot_start()
        } else {
            h.elem_capacity
        };
        let mut packed: Vec<Vec<E>> = Vec::with_capacity(end_vertex - start_vertex);
        for vid in start_vertex..end_vertex {
            packed.push(self.collect_out_edges(col, vid)?);
        }
        for (k, vid) in (start_vertex..end_vertex).enumerate() {
            let edges = &packed[k];
            let row = col_get(col, vid)?;
            let d = edges.len();
            for (i, e) in edges.iter().enumerate() {
                self.write_slot(h.edge_stride, cur + i as u64, *e)
                    .map_err(|_| "write slot")?;
            }
            col_set(
                col,
                vid,
                row.with_base_slot_start(cur)
                    .with_degree(d as u32)
                    .with_log_head(-1),
            )?;
            cur = cur.saturating_add(d as u64);
        }
        if cur > window_right_base {
            return Err("merge packed past window boundary");
        }
        let first_leaf = dgap_leaf_segment_id(start_vertex, h.segment_size);
        let last_leaf = dgap_leaf_segment_id(end_vertex.saturating_sub(1), h.segment_size);
        for ls in first_leaf..=last_leaf {
            self.release_log_segment(&h, ls)
                .map_err(|_| "release log")?;
        }
        self.refresh_slab_occupied_tail_from_column(col)?;
        Ok(())
    }

    pub fn rebalance_weighted<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        start_vertex: usize,
        end_vertex: usize,
        pma_idx: usize,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        self.merge_logs_into_slab_for_window(col, start_vertex, end_vertex)?;
        let h = self.header().ok_or("bad edge header")?;
        if h.edge_stride as usize != E::EDGE_BYTES {
            return Err("edge stride mismatch");
        }
        let n = col.len() as usize;
        if start_vertex >= n || end_vertex > n || end_vertex <= start_vertex {
            return Err("bad vertex window");
        }
        let mut win: Vec<V> = Vec::with_capacity(end_vertex - start_vertex);
        for i in start_vertex..end_vertex {
            win.push(col_get(col, i).map_err(|_| "missing vertex row for rebalance")?);
        }
        let next_base = if end_vertex >= n {
            h.elem_capacity
        } else {
            col_get(col, end_vertex).map_err(|_| "missing boundary vertex")?
                .base_slot_start()
        };
        let to = next_base;
        let from = win[0].base_slot_start();
        if to <= from {
            return Err("empty range");
        }
        let cap = (to - from) as i64;
        let c = self.mem.read_segment_edge_counts(pma_idx, self.pma_stride());
        let total_at = c.total;
        let actual_at = c.actual;
        if total_at != cap {
            return Err("segment total mismatch");
        }
        let mut edges_local: Vec<E> = (from..to)
            .map(|s| self.read_slot(h.edge_stride, s))
            .collect();
        rebalance_weighted_window_rel(
            &mut win,
            from,
            next_base,
            &mut edges_local,
            total_at,
            actual_at,
        );
        for (i, slot) in (from..to).enumerate() {
            self.write_slot(h.edge_stride, slot, edges_local[i])
                .map_err(|_| "grow failed")?;
        }
        for li in 0..win.len() {
            col_set(col, start_vertex + li, win[li])?;
        }
        self.refresh_slab_occupied_tail_from_column(col)?;
        Ok(())
    }

    pub fn set_num_edges_header(&self, n: u64) {
        self.mem.set_num_edges(n);
    }

    pub fn resize_double<V, Mvs>(&self, col: &SlotMap<V, Mvs>) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let h = self.header().ok_or("bad edge header")?;
        let old_cap = h.elem_capacity;
        let new_cap = old_cap.checked_mul(2).ok_or("capacity overflow")?;
        let stride = h.edge_stride;
        let n = col.len() as usize;
        if n > 0 {
            self.merge_logs_into_slab_for_window(col, 0, n)?;
        }
        let h = self.header().ok_or("bad header")?;
        let mut edges: Vec<E> = (0..old_cap).map(|s| self.read_slot(stride, s)).collect();
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
            self.sync_pma_edge_counts(col)?;
            self.refresh_slab_occupied_tail_from_column(col)?;
            return Ok(());
        }
        let mut vertices: Vec<V> = (0..n)
            .map(|i| col_get(col, i).map_err(|_| "missing vertex during resize"))
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
            col_set(col, i, vertices[i])?;
        }
        self.mem
            .zero_log_partition(&h2)
            .map_err(|_| "zero log partition")?;
        self.sync_pma_edge_counts(col)?;
        self.refresh_slab_occupied_tail_from_column(col)?;
        Ok(())
    }

    fn try_insert_into_log<V, Mvs>(&self, col: &SlotMap<V, Mvs>, vid: usize, edge: E) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let h = self.header().ok_or("bad edge header")?;
        let leaf = dgap_leaf_segment_id(vid, h.segment_size);
        let idx = self.mem.read_log_idx(&h, leaf);
        if idx >= h.max_log_entries as i32 {
            return Err("log full");
        }
        let v = col_get(col, vid)?;
        let prev = v.log_head();
        let entry_idx = idx as u32;
        self.write_log_entry(&h, leaf, entry_idx, prev, vid as i32, edge)
            .map_err(|_| "write log")?;
        self.mem.write_log_idx(&h, leaf, idx + 1);
        col_set(col, vid, v.with_log_head(idx).with_degree(v.degree() + 1))?;
        let ne = self.mem.read_num_edges();
        self.mem.set_num_edges(ne.saturating_add(1));
        Ok(())
    }

    fn dgap_insert_once<V, Mvs>(&self, col: &SlotMap<V, Mvs>, vid: usize, edge: E) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let h = self.header().ok_or("bad edge header")?;
        let n = col.len() as usize;
        if vid >= n {
            return Err("vertex out of range");
        }
        let v = col_get(col, vid)?;
        let loc = v.base_slot_start().saturating_add(v.degree() as u64);
        if Self::have_space_onseg(col, vid, loc, h.elem_capacity) {
            self.write_slot(h.edge_stride, loc, edge)
                .map_err(|_| "write slot")?;
            col_set(col, vid, v.with_degree(v.degree() + 1))?;
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
    pub fn insert_edge_and_maintain<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        vid: usize,
        edge: E,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        const MAX_TRIES: usize = 64;
        for _try in 0..MAX_TRIES {
            let h = self.header().ok_or("bad edge header")?;
            let n = col.len() as usize;
            if vid >= n {
                return Err("vertex out of range");
            }
            let leaf = dgap_leaf_segment_id(vid, h.segment_size);
            let idx = self.mem.read_log_idx(&h, leaf);
            if idx >= h.max_log_entries as i32 {
                let left_index = (vid / h.segment_size as usize) * h.segment_size as usize;
                let right_index = ((left_index + h.segment_size as usize).min(n)).min(n);
                self.merge_logs_into_slab_for_window(col, left_index, right_index)?;
                self.sync_pma_edge_counts_for_vertex_range(col, left_index, right_index)?;
                continue;
            }
            match self.dgap_insert_once(col, vid, edge) {
                Ok(()) => {
                    self.bump_segment_edge_counts_leaf_delta(vid, 1, 0, 0)
                        .map_err(|_| "sync edge counts")?;
                    self.maintain_rebalance_loop(col, vid)?;
                    self.refresh_slab_occupied_tail_from_column(col)?;
                    return Ok(());
                }
                Err("log full") => {
                    let left_index = (vid / h.segment_size as usize) * h.segment_size as usize;
                    let right_index = (left_index + h.segment_size as usize).min(n);
                    self.merge_logs_into_slab_for_window(col, left_index, right_index)?;
                    self.sync_pma_edge_counts_for_vertex_range(col, left_index, right_index)?;
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

    /// Insert many `(source_vid, edge)` pairs in iterator order. Consecutive edges with the same `vid` are
    /// batched (preflight, one slab [`DgapGraphMemories::write_edge_slab_span`] when possible, then log
    /// rows via [`write_log_entry_raw`]), with `sync_pma_*` and [`Self::maintain_rebalance_loop`] once per
    /// same-`vid` run. On failure, earlier successful inserts remain (same partial-commit semantics as
    /// repeated [`Self::insert_edge_and_maintain`]).
    pub fn insert_edges_and_maintain<V, Mvs, I>(&self, col: &SlotMap<V, Mvs>, edges: I) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
        I: IntoIterator<Item = (usize, E)>,
    {
        let mut iter = edges.into_iter();
        let mut cur_vid: Option<usize> = None;
        let mut run: Vec<E> = Vec::new();

        while let Some((vid, e)) = iter.next() {
            match cur_vid {
                None => {
                    cur_vid = Some(vid);
                    run.push(e);
                }
                Some(cv) if cv == vid => run.push(e),
                Some(cv) => {
                    self.flush_insert_run(col, cv, &run)?;
                    run.clear();
                    cur_vid = Some(vid);
                    run.push(e);
                }
            }
        }
        if let Some(cv) = cur_vid {
            self.flush_insert_run(col, cv, &run)?;
        }
        Ok(())
    }

    fn flush_insert_run<V, Mvs>(&self, col: &SlotMap<V, Mvs>, vid: usize, run: &[E]) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        if run.is_empty() {
            return Ok(());
        }
        if run.len() == 1 {
            self.insert_edge_and_maintain(col, vid, run[0])
        } else {
            self.insert_same_vid_run_batch(col, vid, run)
        }
    }

    /// Preflight (merge / resize as needed), write up to `k` new edges for `vid` with at most one slab span
    /// write and per-log-row writes, then sync + PMA maintain once.
    fn insert_same_vid_run_batch<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        vid: usize,
        edges: &[E],
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let k = edges.len();
        if k == 0 {
            return Ok(());
        }

        const MAX_TRIES: usize = 64;
        for _try in 0..MAX_TRIES {
            let h = self.header().ok_or("bad edge header")?;
            let n = col.len() as usize;
            if vid >= n {
                return Err("vertex out of range");
            }
            let leaf = dgap_leaf_segment_id(vid, h.segment_size);
            let idx = self.mem.read_log_idx(&h, leaf);
            if idx >= h.max_log_entries as i32 {
                let left_index = (vid / h.segment_size as usize) * h.segment_size as usize;
                let right_index = (left_index + h.segment_size as usize).min(n);
                self.merge_logs_into_slab_for_window(col, left_index, right_index)?;
                self.sync_pma_edge_counts_for_vertex_range(col, left_index, right_index)?;
                continue;
            }

            let v = col_get(col, vid)?;
            let stride = h.edge_stride as usize;
            let loc0 = v.base_slot_start().saturating_add(v.degree() as u64);

            let mut k_slab = 0usize;
            let mut test_loc = loc0;
            while k_slab < k && Self::have_space_onseg(col, vid, test_loc, h.elem_capacity) {
                k_slab += 1;
                test_loc = test_loc.saturating_add(1);
            }

            let log_free = (h.max_log_entries as i32 - idx).max(0) as usize;
            let k_log_needed = k.saturating_sub(k_slab);
            if k_log_needed > log_free {
                let left_index = (vid / h.segment_size as usize) * h.segment_size as usize;
                let right_index = (left_index + h.segment_size as usize).min(n);
                self.merge_logs_into_slab_for_window(col, left_index, right_index)?;
                self.sync_pma_edge_counts_for_vertex_range(col, left_index, right_index)?;
                continue;
            }

            if k_slab > 0 && loc0.saturating_add(k_slab as u64) > h.elem_capacity {
                self.resize_double(col)?;
                continue;
            }

            if k_slab > 0 {
                let mut payload = vec![0u8; k_slab * stride];
                for i in 0..k_slab {
                    edges[i].write_to(&mut payload[i * stride..(i + 1) * stride]);
                }
                if self
                    .mem
                    .write_edge_slab_span(h.edge_stride, loc0, &payload)
                    .is_err()
                {
                    self.resize_double(col)?;
                    continue;
                }
                let deg_new = v.degree().saturating_add(k_slab as u32);
                col_set(col, vid, v.with_degree(deg_new))?;
                let ne = self.mem.read_num_edges();
                self.mem.set_num_edges(ne.saturating_add(k_slab as u64));
            }

            for i in k_slab..k {
                self.try_insert_into_log(col, vid, edges[i])?;
            }

            self.bump_segment_edge_counts_leaf_delta(vid, k as i64, 0, 0)
                .map_err(|_| "sync edge counts")?;
            self.maintain_rebalance_loop(col, vid)?;
            self.refresh_slab_occupied_tail_from_column(col)?;
            return Ok(());
        }

        Err("same-vid batch insert tries exhausted")
    }

    pub(crate) fn maintain_rebalance_loop<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        vid: usize,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        for _ in 0..32 {
            let h = self.header().ok_or("bad header")?;
            let st = self.pma_stride();
            let num_v = col.len() as usize;
            match rebalance_decision_with_reader(
                vid as u32,
                h.segment_size,
                h.segment_count,
                num_v,
                h.tree_height,
                |j| {
                    let c = self.mem.read_segment_edge_counts(j, st);
                    (c.actual, c.total)
                },
            ) {
                RebalanceDecision::Noop => return Ok(()),
                RebalanceDecision::RebalanceWindow {
                    left_vertex,
                    right_vertex,
                    pma_idx,
                } => {
                    self.rebalance_weighted(col, left_vertex, right_vertex, pma_idx)?;
                    self.sync_pma_edge_counts_for_vertex_range(col, left_vertex, right_vertex)?;
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

    /// `current_len` is [`SlotMap::len`](ic_stable_slot_map::SlotMap::len) on the vertex table **before** push;
    /// the new row will get `vid == current_len`.
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

    /// Returns [`DgapEdgeHeaderV1::slab_occupied_tail`], maintained to equal `max_i (base_i + degree_i)`.
    /// Use [`Self::refresh_slab_occupied_tail_from_column`] after any mutation that changes vertex
    /// `base_slot_start` or `degree` without going through this store’s normal paths.
    pub fn slab_occupied_tail<V, Mvs>(&self, col: &SlotMap<V, Mvs>) -> Result<u64, &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let h = self.header().ok_or("bad header")?;
        #[cfg(debug_assertions)]
        {
            let mut truth = 0u64;
            let n = col.len() as usize;
            for i in 0..n {
                let v = col_get(col, i).map_err(|_| "missing vertex row")?;
                truth = truth.max(v.base_slot_start().saturating_add(v.degree() as u64));
            }
            debug_assert_eq!(
                h.slab_occupied_tail, truth,
                "slab_occupied_tail header out of sync with vertex column"
            );
        }
        #[cfg(not(debug_assertions))]
        let _ = col;
        Ok(h.slab_occupied_tail)
    }

    /// Recompute `max_i (base_i + degree_i)` from `col` and persist it in the VCE header.
    pub fn refresh_slab_occupied_tail_from_column<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let n = col.len() as usize;
        let mut end = 0u64;
        for i in 0..n {
            let v = col_get(col, i).map_err(|_| "missing vertex row")?;
            let b = v.base_slot_start();
            let d = v.degree() as u64;
            end = end.max(b.saturating_add(d));
        }
        self.mem.set_slab_occupied_tail(end);
        Ok(())
    }

    /// Next `base_slot_start` for a **new tail** vertex row (before [`SlotMap::insert`](ic_stable_slot_map::SlotMap::insert)).
    ///
    /// Uses the occupied slab tail plus a one-slot bump when the current last vertex has `degree == 0`
    /// and would otherwise share the same insertion cursor as another empty tail (see `dgap_insert_once`).
    pub fn slab_append_base_slot<V, Mvs>(&self, col: &SlotMap<V, Mvs>) -> Result<u64, &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let n = col.len() as usize;
        if n == 0 {
            return Ok(0);
        }
        let mut expected = self.slab_occupied_tail(col)?;
        let last = col_get(col, n - 1).map_err(|_| "missing last vertex row")?;
        if last.degree() == 0 && expected == last.base_slot_start() {
            expected = expected.saturating_add(1);
        }
        Ok(expected)
    }

    pub fn sync_pma_edge_counts<V, Mvs>(&self, col: &SlotMap<V, Mvs>) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let h = self.header().ok_or("bad header")?;
        let sc = h.segment_count as usize;
        let mut buf = vec![
            SegmentEdgeCounts {
                actual: 0,
                total: 0,
                tombstone: 0,
            };
            sc * 2
        ];
        let stride = h.edge_stride;
        crate::dgap::pma_meta::recount_segment_edge_counts_column(
            col,
            col.len(),
            h.segment_count,
            h.segment_size,
            h.elem_capacity,
            |slot| {
                let e = self.read_slot(stride, slot);
                <E as CsrEdgeSlotTombstoneScan>::record_is_physical_tombstone(&e)
            },
            &mut buf,
        );
        let st = self.pma_stride();
        for j in 0..buf.len() {
            self.mem.write_segment_edge_counts(j, st, buf[j]);
        }
        Ok(())
    }

    /// Recompute PMA leaves for the given DGAP segment indices only, then propagate ancestors (see
    /// [`crate::dgap::pma_meta::refresh_segment_edge_counts_leaves`]). No-op if `segment_indices` is empty.
    pub fn sync_pma_edge_counts_for_segments<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        segment_indices: &[usize],
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        if segment_indices.is_empty() {
            return Ok(());
        }
        let h = self.header().ok_or("bad header")?;
        let stride = h.edge_stride;
        let st = self.pma_stride();
        crate::dgap::pma_meta::refresh_segment_edge_counts_leaves(
            col,
            col.len(),
            h.segment_count,
            h.segment_size,
            h.elem_capacity,
            &self.mem.segment_edge_counts,
            st,
            segment_indices,
            |slot| {
                let e = self.read_slot(stride, slot);
                <E as CsrEdgeSlotTombstoneScan>::record_is_physical_tombstone(&e)
            },
        )
    }

    /// Recompute SEC leaves for DGAP segments touched by vertices `[left, right)` (`right` exclusive),
    /// including the predecessor segment when `left` is a segment boundary (see
    /// [`crate::dgap::pma_meta::segments_for_vertex_range`]). No-op if `left >= right`.
    pub fn sync_pma_edge_counts_for_vertex_range<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        left: usize,
        right: usize,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        if left >= right {
            return Ok(());
        }
        let h = self.header().ok_or("bad header")?;
        let segs = crate::dgap::pma_meta::segments_for_vertex_range(
            left,
            right,
            h.segment_size,
            h.segment_count,
        );
        self.sync_pma_edge_counts_for_segments(col, &segs)
    }

    /// Merge overflow logs into the slab for every vertex in the same DGAP leaf segment as `vid`.
    pub fn merge_logs_for_vertex_segment<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        vid: usize,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        let h = self.header().ok_or("bad edge header")?;
        let n = col.len() as usize;
        let ss = h.segment_size as usize;
        let left = (vid / ss) * ss;
        let right = (left + ss).min(n);
        if left < right {
            self.merge_logs_into_slab_for_window(col, left, right)?;
        }
        Ok(())
    }

    /// After merging logs, set the slot for `owner_vid → neighbor_vid` to a tombstone (`E: CsrEdgeTombstone`).
    pub fn tombstone_edge_with_neighbor<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        owner_vid: usize,
        neighbor_vid: usize,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
        E: CsrEdge + CsrEdgeTombstone,
    {
        self.merge_logs_for_vertex_segment(col, owner_vid)?;
        let h = self.header().ok_or("bad edge header")?;
        let edges = self.collect_out_edges(col, owner_vid)?;
        let row = col_get(col, owner_vid)?;
        let base = row.base_slot_start();
        for (i, e) in edges.iter().enumerate() {
            if e.neighbor_vid() == neighbor_vid {
                let tomb = (*e).with_tombstone(true);
                let slot = base.saturating_add(i as u64);
                self.write_slot(h.edge_stride, slot, tomb)
                    .map_err(|_| "tombstone write slot")?;
                return Ok(());
            }
        }
        Err("tombstone: neighbor not found")
    }

    /// Physically remove one edge at `local_index` in `vid`'s neighborhood (slab compaction + PMA sync).
    pub fn remove_slab_edge_at_local_index_physically<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        vid: usize,
        local_index: usize,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        self.merge_logs_for_vertex_segment(col, vid)?;
        let h = self.header().ok_or("bad edge header")?;
        let n = col.len() as usize;
        if vid >= n {
            return Err("vertex out of range");
        }
        let row = col_get(col, vid)?;
        let deg = row.degree() as usize;
        if local_index >= deg {
            return Err("local edge index out of range");
        }
        let base = row.base_slot_start();
        let remove_pos = base.saturating_add(local_index as u64);
        let occ_end = self.slab_occupied_tail(col)?;
        if remove_pos >= occ_end {
            return Err("remove position past occupied tail");
        }
        let stride = h.edge_stride;
        let end = occ_end as usize;
        let rp = remove_pos as usize;
        let slide_lo = remove_pos;
        let slide_hi = occ_end;
        {
            let _s = crate::canbench_scope::scope("dgap_remove_slab_slide");
            const MAX_SLIDE_CHUNK_BYTES: usize = 8192;
            let st = stride as usize;
            let num_slots = end.saturating_sub(1).saturating_sub(rp);
            if num_slots > 0 && st > 0 {
                let max_chunk_slots = (MAX_SLIDE_CHUNK_BYTES / st).max(1);
                let cap_bytes = max_chunk_slots.saturating_mul(st);
                let mut buf = vec![0u8; cap_bytes];
                let mut offset = 0usize;
                while offset < num_slots {
                    let chunk_slots = (num_slots - offset).min(max_chunk_slots);
                    let byte_len = chunk_slots.saturating_mul(st);
                    let src_slot = (rp + 1 + offset) as u64;
                    let dst_slot = (rp + offset) as u64;
                    self.mem.read_edge_slab_span(stride, src_slot, &mut buf[..byte_len]);
                    self.mem
                        .write_edge_slab_span(stride, dst_slot, &buf[..byte_len])
                        .map_err(|_| "slide write")?;
                    offset += chunk_slots;
                }
            }
        }
        let v_after = col_get(col, vid)?;
        col_set(col, vid, v_after.with_degree((deg.saturating_sub(1)) as u32))?;
        let ne = self.mem.read_num_edges();
        self.mem.set_num_edges(ne.saturating_sub(1));
        let ss = h.segment_size.max(1) as usize;
        let sc = h.segment_count as usize;
        let mut segs = BTreeSet::new();
        {
            let _s = crate::canbench_scope::scope("dgap_remove_slab_base_decrement");
            // `L`: first row whose slab base is strictly past `remove_pos` (suffix needs `--`).
            // Prefix `[0, L)` only contributes PMA dirty segments (no base writes). The pair
            // `(L-1, L)` is the first where the lagged neighbor uses `row L` *after* decrement.
            #[cfg(feature = "strict-dgap-invariants")]
            strict_check_dense_bases_monotone_remove_slab(col, n)?;
            let l_suffix = first_dense_vertex_base_gt(col, n, remove_pos)?;
            let mut prev_final: Option<V> = None;
            let mut prev_had_base_dec = false;

            for j in 0..l_suffix {
                let cur = col_get(col, j)?;
                if j > 0 {
                    let idx = j - 1;
                    let prev = prev_final.ok_or("remove_slab: column scan invariant")?;
                    let b_prev = prev.base_slot_start();
                    let nxt_prev = cur.base_slot_start();
                    let dirty = idx == vid || (b_prev < slide_hi && nxt_prev > slide_lo);
                    if dirty {
                        push_remove_slab_pma_dirty_segments(&mut segs, idx, ss, sc);
                    }
                }
                prev_final = Some(cur);
            }

            for j in l_suffix..n {
                let vr = col_get(col, j)?;
                let b = vr.base_slot_start();
                let had_dec = b > remove_pos;
                let cur_final = if had_dec {
                    let nf = vr.with_base_slot_start(b.saturating_sub(1));
                    col_set(col, j, nf)?;
                    nf
                } else {
                    vr
                };

                if j > 0 {
                    let idx = j - 1;
                    let prev = prev_final.ok_or("remove_slab: column scan invariant")?;
                    let b_prev = prev.base_slot_start();
                    let nxt_prev = cur_final.base_slot_start();
                    let dirty = idx == vid
                        || prev_had_base_dec
                        || (b_prev < slide_hi && nxt_prev > slide_lo);
                    if dirty {
                        push_remove_slab_pma_dirty_segments(&mut segs, idx, ss, sc);
                    }
                }

                prev_final = Some(cur_final);
                prev_had_base_dec = had_dec;
            }
            if n > 0 {
                let prev = prev_final.ok_or("remove_slab: column scan invariant")?;
                let b_prev = prev.base_slot_start();
                let nxt_prev = h.elem_capacity;
                let last = n - 1;
                let dirty = last == vid
                    || prev_had_base_dec
                    || (b_prev < slide_hi && nxt_prev > slide_lo);
                if dirty {
                    push_remove_slab_pma_dirty_segments(&mut segs, last, ss, sc);
                }
            }
        }
        {
            let _s = crate::canbench_scope::scope("dgap_remove_slab_sync_pma_full");
            let idx: Vec<usize> = segs.into_iter().collect();
            if idx.is_empty() {
                self.sync_pma_edge_counts(col)?;
            } else {
                self.sync_pma_edge_counts_for_segments(col, &idx)?;
            }
        }
        {
            let _s = crate::canbench_scope::scope("dgap_remove_slab_maintain");
            self.maintain_rebalance_loop(col, vid)?;
        }
        {
            let _s = crate::canbench_scope::scope("dgap_remove_slab_refresh_tail");
            self.refresh_slab_occupied_tail_from_column(col)?;
        }
        Ok(())
    }

    /// After merging logs, returns the neighborhood index of `neighbor_vid` at `owner_vid`, if present.
    pub fn neighbor_local_index<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        owner_vid: usize,
        neighbor_vid: usize,
    ) -> Result<Option<usize>, &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        self.merge_logs_for_vertex_segment(col, owner_vid)?;
        let edges = self.collect_out_edges(col, owner_vid)?;
        Ok(edges.iter().position(|e| e.neighbor_vid() == neighbor_vid))
    }
}

#[inline]
fn zero_edge_record<E: CsrEdge>() -> E {
    if E::EDGE_BYTES <= MAX_INLINE_EDGE {
        let z = [0u8; MAX_INLINE_EDGE];
        E::read_from(&z[..E::EDGE_BYTES])
    } else {
        E::read_from(&vec![0u8; E::EDGE_BYTES])
    }
}

/// Slide live edges within each vertex span inside `[left, right)`, drop tombstones, zero freed tail slots.
/// `edges_local` indexes global slot `range_from + i`.
fn slide_remove_tombstones_in_window_edges<V, Mvs, E>(
    col: &SlotMap<V, Mvs>,
    edges_local: &mut [E],
    range_from: u64,
    left: usize,
    right: usize,
    nv: usize,
    elem_cap: u64,
) -> Result<u64, &'static str>
where
    V: CsrVertex,
    Mvs: Memory,
    E: CsrEdge + CsrEdgeTombstone,
{
    let zero = zero_edge_record::<E>();
    let mut removed = 0u64;
    for v in left..right {
        let row = col_get(col, v)?;
        let b = row.base_slot_start();
        let end = if v + 1 < nv {
            col_get(col, v + 1)?.base_slot_start()
        } else {
            elem_cap
        };
        if b < range_from || end < b || end > range_from.saturating_add(edges_local.len() as u64) {
            return Err("vertex span outside local edge buffer");
        }
        let mut write = b;
        for s in b..end {
            let idx = (s - range_from) as usize;
            let e = edges_local[idx];
            if e.is_tombstone() {
                removed = removed.saturating_add(1);
            } else {
                if write != s {
                    let wi = (write - range_from) as usize;
                    edges_local[wi] = e;
                }
                write = write.saturating_add(1);
            }
        }
        let mut t = write;
        while t < end {
            edges_local[(t - range_from) as usize] = zero;
            t = t.saturating_add(1);
        }
    }
    Ok(removed)
}

impl<E: CsrEdge + CsrEdgeTombstone, M1: Memory, M2: Memory> DgapEdgeStore<E, M1, M2> {
    /// Merge logs, strip physical tombstones in the vertex window, rebalance in RAM, write slab + vertices,
    /// sync PMA, then [`Self::maintain_rebalance_loop`]. Returns tombstone slots physically removed.
    pub fn maintain_segment_leaf_plan_and_commit<V, Mvs>(
        &self,
        col: &SlotMap<V, Mvs>,
        left: usize,
        right: usize,
        pma_idx: usize,
    ) -> Result<u64, &'static str>
    where
        V: CsrVertex,
        Mvs: Memory,
    {
        self.merge_logs_into_slab_for_window(col, left, right)?;
        let h = self.header().ok_or("bad header")?;
        let stride = h.edge_stride;
        if stride as usize != E::EDGE_BYTES {
            return Err("edge stride mismatch");
        }
        let n = col.len() as usize;
        if left >= n || right > n || right <= left {
            return Err("bad leaf bounds");
        }
        let from = col_get(col, left)?.base_slot_start();
        let to = if right < n {
            col_get(col, right)?.base_slot_start()
        } else {
            h.elem_capacity
        };
        if to <= from {
            return Err("empty slab range");
        }
        let mut edges_local: Vec<E> = (from..to)
            .map(|s| self.read_slot(stride, s))
            .collect();
        let removed = slide_remove_tombstones_in_window_edges(
            col,
            &mut edges_local,
            from,
            left,
            right,
            n,
            h.elem_capacity,
        )?;
        let cap = (to - from) as i64;
        let c = self.mem.read_segment_edge_counts(pma_idx, self.pma_stride());
        let total_at = c.total;
        let actual_at = c.actual;
        if total_at != cap {
            return Err("segment total mismatch");
        }
        let mut win: Vec<V> = Vec::with_capacity(right - left);
        for i in left..right {
            win.push(col_get(col, i)?);
        }
        rebalance_weighted_window_rel(
            &mut win,
            from,
            to,
            &mut edges_local,
            total_at,
            actual_at,
        );
        for (i, slot) in (from..to).enumerate() {
            self.write_slot(stride, slot, edges_local[i])
                .map_err(|_| "maintain commit write")?;
        }
        for li in 0..win.len() {
            col_set(col, left + li, win[li])?;
        }
        if removed > 0 {
            let ne = self.mem.read_num_edges();
            self.mem.set_num_edges(ne.saturating_sub(removed));
        }
        self.sync_pma_edge_counts_for_vertex_range(col, left, right)?;
        self.maintain_rebalance_loop(col, left)?;
        self.refresh_slab_occupied_tail_from_column(col)?;
        Ok(removed)
    }
}
