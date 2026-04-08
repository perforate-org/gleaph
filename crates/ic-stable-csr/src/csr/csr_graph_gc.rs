//! [`CsrGraph`] plus a seventh [`Memory`] backing a persisted [`StableVecDeque`](crate::StableVecDeque)
//! **segment maintenance** queue (per DGAP leaf: tombstone compaction + PMA rebalance).

use std::collections::BTreeSet;

use ic_stable_structures::Memory;

use crate::StableVecDeque;
use crate::csr::csr_graph::{CsrGraph, CsrGraphError, LogicalNeighborhoodIter};
use crate::csr::gc_work_item::{GC_TAG_SEGMENT_FWD, GC_TAG_SEGMENT_REV, GC_TAG_VERTEX, GcWorkItem};
use crate::csr::{DgapStores, DgapStoresError};
use crate::dgap::{
    RebalanceDecision, SegmentMaintainAction, SegmentMaintainThresholds,
    rebalance_decision_with_reader, segment_maintenance_decision,
};
use crate::layout::dgap::dgap_leaf_segment_id;
use crate::traits::{CsrEdge, CsrEdgeTombstone, CsrEdgeUndirected, CsrVertex, CsrVertexTombstone};

#[inline]
fn vertex_set_dense<V, M>(
    map: &ic_stable_slot_map::SlotMap<V, M>,
    index: usize,
    row: V,
) -> Result<(), CsrGraphError>
where
    V: CsrVertex,
    M: Memory,
{
    map.set_dense(index as u32, &row)
        .map_err(|_| CsrGraphError::LogicalMutation("vertex set_dense failed"))
}

fn sync_pma_meta_for_touched_vertices<V, E, Mvs, M1, M2>(
    stores: &DgapStores<V, E, Mvs, M1, M2>,
    touched: &BTreeSet<usize>,
) -> Result<(), &'static str>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    M1: Memory,
    M2: Memory,
{
    let mut iter = touched.iter().copied();
    let Some(mut range_start) = iter.next() else {
        return Ok(());
    };
    let mut prev = range_start;
    for vid in iter {
        if vid == prev + 1 {
            prev = vid;
            continue;
        }
        stores.sync_pma_meta_for_vertex_range(range_start, prev.saturating_add(1))?;
        range_start = vid;
        prev = vid;
    }
    stores.sync_pma_meta_for_vertex_range(range_start, prev.saturating_add(1))?;
    Ok(())
}

/// Bidirectional CSR with a stable work queue for lazy **per-leaf** physical compaction (tombstones + PMA).
///
/// Single-threaded canister assumption: inline maintenance and the queue are not interleaved concurrently.
pub struct CsrGraphWithGcQueue<V, E, Mvs, F1, F2, R1, R2, QM>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    QM: Memory,
{
    graph: CsrGraph<V, E, Mvs, F1, F2, R1, R2>,
    work_queue: StableVecDeque<GcWorkItem, QM>,
    maintain_thresholds: SegmentMaintainThresholds,
}

impl<V, E, Mvs, F1, F2, R1, R2, QM> CsrGraphWithGcQueue<V, E, Mvs, F1, F2, R1, R2, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    QM: Memory,
{
    pub fn graph(&self) -> &CsrGraph<V, E, Mvs, F1, F2, R1, R2> {
        &self.graph
    }

    pub fn work_queue_len(&self) -> u64 {
        self.work_queue.len()
    }

    pub fn maintain_thresholds(&self) -> SegmentMaintainThresholds {
        self.maintain_thresholds
    }

    fn push_queue(&self, item: GcWorkItem) -> Result<(), CsrGraphError> {
        self.work_queue
            .push_back(&item)
            .map_err(|e| CsrGraphError::GcQueue(e.into()))
    }

    fn vertex_tombstone_fwd(&self, vid: usize) -> bool {
        self.graph
            .forward_dgap()
            .vertices
            .get_dense(vid as u32)
            .map(|v| v.is_tombstone())
            .unwrap_or(true)
    }

    fn ensure_endpoint_live(&self, vid: usize) -> Result<(), CsrGraphError> {
        if self.vertex_tombstone_fwd(vid) {
            return Err(CsrGraphError::EndpointTombstone { vid });
        }
        Ok(())
    }

    pub fn sync_pma_meta(&self) -> Result<(), CsrGraphError> {
        self.graph.sync_pma_meta()
    }

    pub fn sync_pma_meta_for_vertex_range(
        &self,
        left: usize,
        right: usize,
    ) -> Result<(), CsrGraphError> {
        self.graph.sync_pma_meta_for_vertex_range(left, right)
    }

    pub fn insert_vertex(&self, row_template: V) -> Result<u64, CsrGraphError> {
        self.graph.insert_vertex(row_template)
    }

    pub fn insert_vertex_strict(&self, row_template: V) -> Result<u64, CsrGraphError> {
        self.graph.insert_vertex_strict(row_template)
    }

    pub fn insert_directed(&self, src: usize, dst: usize, edge: E) -> Result<(), CsrGraphError> {
        self.ensure_endpoint_live(src)?;
        self.ensure_endpoint_live(dst)?;
        if self.graph.has_forward_slot_to_neighbor(src, dst)? {
            return Err(CsrGraphError::AdjacencySlotOccupied { src, dst });
        }
        self.graph.insert_directed(src, dst, edge)
    }

    pub fn insert_undirected(&self, u: usize, v: usize, edge: E) -> Result<(), CsrGraphError>
    where
        E: CsrEdgeUndirected,
    {
        self.ensure_endpoint_live(u)?;
        self.ensure_endpoint_live(v)?;
        if u != v {
            if self.graph.has_forward_slot_to_neighbor(u, v)? {
                return Err(CsrGraphError::AdjacencySlotOccupied { src: u, dst: v });
            }
            if self.graph.has_forward_slot_to_neighbor(v, u)? {
                return Err(CsrGraphError::AdjacencySlotOccupied { src: v, dst: u });
            }
        }
        self.graph.insert_undirected(u, v, edge)
    }

    /// LSM-style trigger composition: PMA density ([`rebalance_decision`]) + tombstone ratio + queue depth.
    fn schedule_maintain_for_vertex_column<M1: Memory, M2: Memory>(
        &self,
        stores: &DgapStores<V, E, Mvs, M1, M2>,
        pivot_vid: usize,
        queue_len: u64,
        forward_column: bool,
    ) -> Result<(), CsrGraphError> {
        let h = stores
            .edges
            .header()
            .ok_or(CsrGraphError::LogicalMutation("no edge header"))?;
        let sc = h.segment_count as usize;
        let n = stores.vertices.len() as usize;
        let leaf = dgap_leaf_segment_id(pivot_vid, h.segment_size);
        let pma_idx = leaf as usize + sc;
        let leaf_counts = stores.edges.read_segment_edge_counts(pma_idx);
        let reb = rebalance_decision_with_reader(
            pivot_vid as u32,
            h.segment_size,
            h.segment_count,
            n,
            h.tree_height,
            |j| {
                let c = stores.edges.read_segment_edge_counts(j);
                (c.actual, c.total)
            },
        );
        let action =
            segment_maintenance_decision(leaf_counts, reb, queue_len, &self.maintain_thresholds);
        let map_g = |m: &'static str| {
            if forward_column {
                CsrGraphError::Forward(DgapStoresError::Graph(m))
            } else {
                CsrGraphError::Reverse(DgapStoresError::Graph(m))
            }
        };
        match action {
            SegmentMaintainAction::Noop => Ok(()),
            SegmentMaintainAction::Enqueue => {
                let item = if forward_column {
                    GcWorkItem::segment_maintain_forward(leaf)
                } else {
                    GcWorkItem::segment_maintain_reverse(leaf)
                };
                self.push_queue(item)
            }
            SegmentMaintainAction::InlineNow => {
                self.run_segment_maintain_for_leaf(stores, leaf, map_g)
            }
        }
    }

    /// Resize slab if PMA demands it, then [`DgapEdgeStore::maintain_segment_leaf_plan_and_commit`].
    fn run_segment_maintain_for_leaf<M1: Memory, M2: Memory>(
        &self,
        stores: &DgapStores<V, E, Mvs, M1, M2>,
        leaf: u32,
        map_g: impl Fn(&'static str) -> CsrGraphError,
    ) -> Result<(), CsrGraphError> {
        let mut h = stores
            .edges
            .header()
            .ok_or(CsrGraphError::LogicalMutation("no edge header"))?;
        let sc = h.segment_count as usize;
        let n = stores.vertices.len() as usize;
        let ss = h.segment_size.max(1) as usize;
        let left = (leaf as usize).saturating_mul(ss).min(n);
        let right = (left + ss).min(n);
        let pivot_vid = left.min(n.saturating_sub(1));
        let pma_idx = leaf as usize + sc;
        for _ in 0..16 {
            let reb = rebalance_decision_with_reader(
                pivot_vid as u32,
                h.segment_size,
                h.segment_count,
                n,
                h.tree_height,
                |j| {
                    let c = stores.edges.read_segment_edge_counts(j);
                    (c.actual, c.total)
                },
            );
            if matches!(reb, RebalanceDecision::ResizeNeeded) {
                stores
                    .edges
                    .resize_double(&stores.vertices)
                    .map_err(|m| map_g(m))?;
                stores.sync_pma_meta().map_err(|m| map_g(m))?;
                h = stores
                    .edges
                    .header()
                    .ok_or(CsrGraphError::LogicalMutation("no edge header"))?;
                continue;
            }
            stores
                .edges
                .maintain_segment_leaf_plan_and_commit(&stores.vertices, left, right, pma_idx)
                .map_err(|m| map_g(m))?;
            return Ok(());
        }
        Err(CsrGraphError::LogicalMutation(
            "segment maintain: resize loop limit",
        ))
    }

    pub fn delete_edge_directed(&self, src: usize, dst: usize) -> Result<(), CsrGraphError> {
        self.graph.ensure_vertex(src)?;
        self.graph.ensure_vertex(dst)?;
        let fwd = self.graph.forward_dgap();
        let rev = self.graph.reverse_dgap();
        let edges_f = fwd
            .edges
            .collect_out_edges(&fwd.vertices, src)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        let found = edges_f
            .iter()
            .find(|e| e.neighbor_vid() == dst)
            .copied()
            .ok_or(CsrGraphError::EdgeNotFound {
                owner: src,
                neighbor: dst,
            })?;
        if found.is_tombstone() {
            return Ok(());
        }
        fwd.edges
            .tombstone_edge_with_neighbor::<V, Mvs>(&fwd.vertices, src, dst)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        rev.edges
            .tombstone_edge_with_neighbor::<V, Mvs>(&rev.vertices, dst, src)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        let fs = fwd
            .vertices
            .get_dense(src as u32)
            .ok_or(CsrGraphError::VertexOutOfRange {
                vid: src,
                len: fwd.vertices.len(),
            })?;
        let rd = rev
            .vertices
            .get_dense(dst as u32)
            .ok_or(CsrGraphError::VertexOutOfRange {
                vid: dst,
                len: rev.vertices.len(),
            })?;
        vertex_set_dense(
            &fwd.vertices,
            src,
            fs.with_degree(fs.degree().saturating_sub(1)),
        )?;
        vertex_set_dense(
            &rev.vertices,
            dst,
            rd.with_degree(rd.degree().saturating_sub(1)),
        )?;
        fwd.edges
            .bump_segment_edge_counts_leaf_delta(src, -1, 0, 1)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        rev.edges
            .bump_segment_edge_counts_leaf_delta(dst, -1, 0, 1)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        self.graph.refresh_slab_occupied_tail_meta()?;
        let qlen = self.work_queue.len();
        self.schedule_maintain_for_vertex_column(fwd, src, qlen, true)?;
        self.schedule_maintain_for_vertex_column(rev, dst, qlen, false)?;
        Ok(())
    }

    pub fn delete_edge_undirected(&self, u: usize, v: usize) -> Result<(), CsrGraphError>
    where
        E: CsrEdgeUndirected,
    {
        if u == v {
            return self.delete_edge_directed(u, u);
        }
        self.graph.ensure_vertex(u)?;
        self.graph.ensure_vertex(v)?;
        let fwd = self.graph.forward_dgap();
        let rev = self.graph.reverse_dgap();
        let mut changed = false;
        for (owner, nb) in [(u, v), (v, u)] {
            let edges_f = fwd
                .edges
                .collect_out_edges(&fwd.vertices, owner)
                .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
            let found = edges_f
                .iter()
                .find(|e| e.neighbor_vid() == nb)
                .copied()
                .ok_or(CsrGraphError::EdgeNotFound {
                    owner,
                    neighbor: nb,
                })?;
            if found.is_tombstone() {
                continue;
            }
            changed = true;
            fwd.edges
                .tombstone_edge_with_neighbor::<V, Mvs>(&fwd.vertices, owner, nb)
                .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
            rev.edges
                .tombstone_edge_with_neighbor::<V, Mvs>(&rev.vertices, nb, owner)
                .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
            let fs =
                fwd.vertices
                    .get_dense(owner as u32)
                    .ok_or(CsrGraphError::VertexOutOfRange {
                        vid: owner,
                        len: fwd.vertices.len(),
                    })?;
            let rd = rev
                .vertices
                .get_dense(nb as u32)
                .ok_or(CsrGraphError::VertexOutOfRange {
                    vid: nb,
                    len: rev.vertices.len(),
                })?;
            vertex_set_dense(
                &fwd.vertices,
                owner,
                fs.with_degree(fs.degree().saturating_sub(1)),
            )?;
            vertex_set_dense(
                &rev.vertices,
                nb,
                rd.with_degree(rd.degree().saturating_sub(1)),
            )?;
            fwd.edges
                .bump_segment_edge_counts_leaf_delta(owner, -1, 0, 1)
                .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
            rev.edges
                .bump_segment_edge_counts_leaf_delta(nb, -1, 0, 1)
                .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        }
        if changed {
            self.graph.refresh_slab_occupied_tail_meta()?;
            let qlen = self.work_queue.len();
            self.schedule_maintain_for_vertex_column(fwd, u, qlen, true)?;
            self.schedule_maintain_for_vertex_column(rev, v, qlen, false)?;
            self.schedule_maintain_for_vertex_column(fwd, v, qlen, true)?;
            self.schedule_maintain_for_vertex_column(rev, u, qlen, false)?;
        }
        Ok(())
    }

    pub fn delete_vertex(&self, vid: usize) -> Result<(), CsrGraphError> {
        self.graph.ensure_vertex(vid)?;
        if self.vertex_tombstone_fwd(vid) {
            return Ok(());
        }
        let (forward_touched, reverse_touched) = {
            let _s = crate::canbench_scope::scope("dgap_delete_vertex_collect_touched");
            self.delete_vertex_collect_touched(vid)?
        };
        {
            let _s = crate::canbench_scope::scope("dgap_delete_vertex_sync_pma_forward");
            sync_pma_meta_for_touched_vertices(self.graph.forward_dgap(), &forward_touched)
                .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        }
        {
            let _s = crate::canbench_scope::scope("dgap_delete_vertex_sync_pma_reverse");
            sync_pma_meta_for_touched_vertices(self.graph.reverse_dgap(), &reverse_touched)
                .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        }
        {
            let _s = crate::canbench_scope::scope("dgap_delete_vertex_push_queue");
            self.push_queue(GcWorkItem::vertex_delete(vid as u64))?;
        }
        Ok(())
    }

    fn delete_vertex_collect_touched(
        &self,
        vid: usize,
    ) -> Result<(BTreeSet<usize>, BTreeSet<usize>), CsrGraphError> {
        let fwd = self.graph.forward_dgap();
        let rev = self.graph.reverse_dgap();
        let mut forward_touched = BTreeSet::new();
        let mut reverse_touched = BTreeSet::new();
        forward_touched.insert(vid);
        reverse_touched.insert(vid);
        {
            let _s = crate::canbench_scope::scope("dgap_delete_vertex_out_neighbors");
            let out_raw = fwd
                .edges
                .collect_out_edges(&fwd.vertices, vid)
                .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
            for e in &out_raw {
                if e.is_tombstone() {
                    continue;
                }
                let u = e.neighbor_vid();
                if self.vertex_tombstone_fwd(u) {
                    continue;
                }
                reverse_touched.insert(u);
                let ru = rev
                    .vertices
                    .get_dense(u as u32)
                    .ok_or(CsrGraphError::VertexOutOfRange {
                        vid: u,
                        len: rev.vertices.len(),
                    })?;
                vertex_set_dense(
                    &rev.vertices,
                    u,
                    ru.with_degree(ru.degree().saturating_sub(1)),
                )?;
            }
        }
        {
            let _s = crate::canbench_scope::scope("dgap_delete_vertex_in_neighbors");
            let in_raw = rev
                .edges
                .collect_out_edges(&rev.vertices, vid)
                .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
            for e in &in_raw {
                if e.is_tombstone() {
                    continue;
                }
                let u = e.neighbor_vid();
                if self.vertex_tombstone_fwd(u) {
                    continue;
                }
                forward_touched.insert(u);
                let fu = fwd
                    .vertices
                    .get_dense(u as u32)
                    .ok_or(CsrGraphError::VertexOutOfRange {
                        vid: u,
                        len: fwd.vertices.len(),
                    })?;
                vertex_set_dense(
                    &fwd.vertices,
                    u,
                    fu.with_degree(fu.degree().saturating_sub(1)),
                )?;
            }
        }
        let _s = crate::canbench_scope::scope("dgap_delete_vertex_refresh_tail");
        let fv = fwd.vertices.get_dense(vid as u32).unwrap();
        let rv = rev.vertices.get_dense(vid as u32).unwrap();
        vertex_set_dense(&fwd.vertices, vid, fv.with_tombstone(true))?;
        vertex_set_dense(&rev.vertices, vid, rv.with_tombstone(true))?;
        self.graph.refresh_slab_occupied_tail_meta()?;
        Ok((forward_touched, reverse_touched))
    }

    #[cfg(test)]
    pub(crate) fn delete_vertex_full_sync_for_test(&self, vid: usize) -> Result<(), CsrGraphError> {
        self.graph.ensure_vertex(vid)?;
        if self.vertex_tombstone_fwd(vid) {
            return Ok(());
        }
        let _ = self.delete_vertex_collect_touched(vid)?;
        self.graph.sync_pma_meta()?;
        self.push_queue(GcWorkItem::vertex_delete(vid as u64))?;
        Ok(())
    }

    pub fn out_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, V, Mvs, F1, F2>, CsrGraphError> {
        self.graph.out_edges_logical(vid)
    }

    pub fn in_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, V, Mvs, R1, R2>, CsrGraphError> {
        self.graph.in_edges_logical(vid)
    }

    pub fn gc_step(&self, budget: usize) -> Result<usize, CsrGraphError> {
        let mut completed = 0usize;
        for _ in 0..budget {
            if self.work_queue.is_empty() {
                break;
            }
            let item = self.work_queue.get(0).expect("len > 0");
            let done = match item.tag() {
                GC_TAG_SEGMENT_FWD => {
                    let leaf = item
                        .leaf_segment_id()
                        .ok_or(CsrGraphError::LogicalMutation("gc: bad segment fwd item"))?;
                    let map_g = |m: &'static str| CsrGraphError::Forward(DgapStoresError::Graph(m));
                    self.run_segment_maintain_for_leaf(self.graph.forward_dgap(), leaf, map_g)?;
                    true
                }
                GC_TAG_SEGMENT_REV => {
                    let leaf = item
                        .leaf_segment_id()
                        .ok_or(CsrGraphError::LogicalMutation("gc: bad segment rev item"))?;
                    let map_g = |m: &'static str| CsrGraphError::Reverse(DgapStoresError::Graph(m));
                    self.run_segment_maintain_for_leaf(self.graph.reverse_dgap(), leaf, map_g)?;
                    true
                }
                GC_TAG_VERTEX => self.gc_one_vertex(item)?,
                _ => {
                    self.work_queue.pop_front();
                    true
                }
            };
            if done {
                self.work_queue.pop_front();
                completed += 1;
            } else {
                break;
            }
        }
        Ok(completed)
    }

    fn gc_one_vertex(&self, item: GcWorkItem) -> Result<bool, CsrGraphError> {
        let vid = item
            .vertex_id()
            .ok_or(CsrGraphError::LogicalMutation("gc: bad vertex work item"))?
            as usize;
        let fwd = self.graph.forward_dgap();
        let h = fwd
            .edges
            .header()
            .ok_or(CsrGraphError::LogicalMutation("no edge header"))?;
        let leaf = dgap_leaf_segment_id(vid, h.segment_size);
        let map_fwd = |m: &'static str| CsrGraphError::Forward(DgapStoresError::Graph(m));
        let map_rev = |m: &'static str| CsrGraphError::Reverse(DgapStoresError::Graph(m));
        self.run_segment_maintain_for_leaf(fwd, leaf, map_fwd)?;
        self.run_segment_maintain_for_leaf(self.graph.reverse_dgap(), leaf, map_rev)?;
        let fd = fwd.vertices.get_dense(vid as u32).unwrap().degree();
        let rd = self
            .graph
            .reverse_dgap()
            .vertices
            .get_dense(vid as u32)
            .unwrap()
            .degree();
        Ok(fd == 0 && rd == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::borrow::Cow;
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::dgap::recount_segment_edge_counts_column;
    use crate::traits::{
        CsrEdge, CsrEdgeSlotTombstoneScan, CsrEdgeTombstone, CsrEdgeUndirected, CsrVertex,
        CsrVertexTombstone,
    };
    use crate::{
        Bound, DgapStores, Memory, SegmentEdgeCounts, Storable, VectorMemory,
    };

    const DEG_TOMB: u32 = 1u32 << 31;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestVertex {
        slot_base: u64,
        deg: u32,
        log_head: i32,
    }

    impl CsrVertex for TestVertex {
        fn base_slot_start(&self) -> u64 {
            self.slot_base
        }
        fn degree(&self) -> u32 {
            self.deg & !DEG_TOMB
        }
        fn with_base_slot_start(self, start: u64) -> Self {
            Self {
                slot_base: start,
                ..self
            }
        }
        fn with_degree(self, degree: u32) -> Self {
            Self {
                deg: (self.deg & DEG_TOMB) | (degree & !DEG_TOMB),
                ..self
            }
        }
        fn log_head(self) -> i32 {
            self.log_head
        }
        fn with_log_head(self, idx: i32) -> Self {
            Self {
                log_head: idx,
                ..self
            }
        }
    }

    impl CsrVertexTombstone for TestVertex {
        fn is_tombstone(&self) -> bool {
            (self.deg & DEG_TOMB) != 0
        }

        fn with_tombstone(self, tombstone: bool) -> Self {
            Self {
                deg: if tombstone {
                    self.deg | DEG_TOMB
                } else {
                    self.deg & !DEG_TOMB
                },
                ..self
            }
        }
    }

    impl Storable for TestVertex {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            let mut b = [0u8; 16];
            b[0..8].copy_from_slice(&self.slot_base.to_le_bytes());
            b[8..12].copy_from_slice(&self.deg.to_le_bytes());
            b[12..16].copy_from_slice(&self.log_head.to_le_bytes());
            Cow::Owned(b.to_vec())
        }
        fn into_bytes(self) -> Vec<u8> {
            self.to_bytes().into_owned()
        }
        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            let s = bytes.as_ref();
            Self {
                slot_base: u64::from_le_bytes(s[0..8].try_into().unwrap()),
                deg: u32::from_le_bytes(s[8..12].try_into().unwrap()),
                log_head: i32::from_le_bytes(s[12..16].try_into().unwrap()),
            }
        }
        const BOUND: Bound = Bound::Bounded {
            max_size: 16,
            is_fixed_size: true,
        };
    }

    /// `[0]` = neighbor vid, `[1]` = undirected flag, `[2]` = tombstone flag.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestEdge([u8; 4]);

    impl CsrEdge for TestEdge {
        const EDGE_BYTES: usize = 4;

        fn read_from(bytes: &[u8]) -> Self {
            Self(bytes.try_into().unwrap())
        }

        fn write_to(self, bytes: &mut [u8]) {
            bytes.copy_from_slice(&self.0);
        }

        fn neighbor_vid(&self) -> usize {
            self.0[0] as usize
        }

        fn with_neighbor_vid(self, vid: usize) -> Self {
            let mut b = self.0;
            b[0] = vid as u8;
            Self(b)
        }
    }

    impl CsrEdgeTombstone for TestEdge {
        fn is_tombstone(&self) -> bool {
            self.0[2] != 0
        }

        fn with_tombstone(self, tombstone: bool) -> Self {
            let mut b = self.0;
            b[2] = if tombstone { 1 } else { 0 };
            Self(b)
        }
    }

    impl CsrEdgeUndirected for TestEdge {
        fn is_undirected(&self) -> bool {
            self.0[1] != 0
        }

        fn with_undirected(self, undirected: bool) -> Self {
            let mut b = self.0;
            b[1] = if undirected { 1 } else { 0 };
            Self(b)
        }
    }

    pub(crate) fn vm() -> VectorMemory {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn empty_vertex() -> TestVertex {
        TestVertex {
            slot_base: 0,
            deg: 0,
            log_head: -1,
        }
    }

    fn assert_dense_vertex_bases_non_decreasing<V, E, Mvs, M1, M2>(
        stores: &DgapStores<V, E, Mvs, M1, M2>,
    ) where
        V: CsrVertex,
        E: CsrEdge,
        Mvs: Memory,
        M1: Memory,
        M2: Memory,
    {
        let n = stores.vertices.len() as usize;
        if n < 2 {
            return;
        }
        let mut prev = stores.vertices.get_dense(0).unwrap().base_slot_start();
        for j in 1..n {
            let b = stores
                .vertices
                .get_dense(j as u32)
                .unwrap()
                .base_slot_start();
            assert!(
                prev <= b,
                "dense vertex bases must be non-decreasing: base[{}]={} > base[{}]={}",
                j - 1,
                prev,
                j,
                b
            );
            prev = b;
        }
    }

    type TestGcGraph = CsrGraphWithGcQueue<
        TestVertex,
        TestEdge,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
    >;

    fn new_graph() -> TestGcGraph {
        CsrGraphWithGcQueue::format_new_with_gc_queue(
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            64,
            4,
            2,
            0,
            None,
        )
        .expect("format")
    }

    fn populate_graph(g: &TestGcGraph) {
        for _ in 0..4 {
            g.insert_vertex(empty_vertex()).unwrap();
        }
        g.sync_pma_meta().unwrap();
        g.insert_directed(0, 1, TestEdge([1, 0, 0, 0])).unwrap();
        g.insert_directed(1, 0, TestEdge([0, 0, 0, 0])).unwrap();
        g.insert_directed(0, 2, TestEdge([2, 0, 0, 0])).unwrap();
        g.insert_directed(3, 0, TestEdge([0, 0, 0, 0])).unwrap();
        assert_dense_vertex_bases_non_decreasing(g.graph().forward_dgap());
        assert_dense_vertex_bases_non_decreasing(g.graph().reverse_dgap());
    }

    fn assert_sec_matches_full_recount_te(
        stores: &DgapStores<TestVertex, TestEdge, VectorMemory, VectorMemory, VectorMemory>,
    ) {
        let h = stores.edges.header().unwrap();
        let sc = h.segment_count as usize;
        let len = sc * 2;
        let mut buf = vec![
            SegmentEdgeCounts {
                actual: 0,
                total: 0,
                tombstone: 0,
            };
            len
        ];
        let es = h.edge_stride;
        recount_segment_edge_counts_column(
            &stores.vertices,
            stores.vertices.len(),
            h.segment_count,
            h.segment_size,
            h.elem_capacity,
            |slot| {
                let e = stores.edges.read_slot(es, slot);
                TestEdge::record_is_physical_tombstone(&e)
            },
            &mut buf,
        );
        for j in 0..len {
            assert_eq!(
                stores.edges.read_segment_edge_counts(j),
                buf[j],
                "SEC node {j} diverges from full recount"
            );
        }
    }

    fn logical_neighbors_out(g: &TestGcGraph, vid: usize) -> Vec<usize> {
        g.out_edges_logical(vid)
            .unwrap()
            .map(|r| r.unwrap().neighbor_vid())
            .collect()
    }

    fn logical_neighbors_in(g: &TestGcGraph, vid: usize) -> Vec<usize> {
        g.in_edges_logical(vid)
            .unwrap()
            .map(|r| r.unwrap().neighbor_vid())
            .collect()
    }

    fn assert_graph_equiv(left: &TestGcGraph, right: &TestGcGraph) {
        assert_eq!(left.work_queue_len(), right.work_queue_len());
        assert_dense_vertex_bases_non_decreasing(left.graph().forward_dgap());
        assert_dense_vertex_bases_non_decreasing(left.graph().reverse_dgap());
        assert_dense_vertex_bases_non_decreasing(right.graph().forward_dgap());
        assert_dense_vertex_bases_non_decreasing(right.graph().reverse_dgap());
        assert_sec_matches_full_recount_te(left.graph().forward_dgap());
        assert_sec_matches_full_recount_te(left.graph().reverse_dgap());
        assert_sec_matches_full_recount_te(right.graph().forward_dgap());
        assert_sec_matches_full_recount_te(right.graph().reverse_dgap());

        let n = left.graph().forward_dgap().vertices.len() as usize;
        assert_eq!(n, right.graph().forward_dgap().vertices.len() as usize);
        for vid in 0..n {
            let lf = left
                .graph()
                .forward_dgap()
                .vertices
                .get_dense(vid as u32)
                .unwrap();
            let rf = right
                .graph()
                .forward_dgap()
                .vertices
                .get_dense(vid as u32)
                .unwrap();
            let lr = left
                .graph()
                .reverse_dgap()
                .vertices
                .get_dense(vid as u32)
                .unwrap();
            let rr = right
                .graph()
                .reverse_dgap()
                .vertices
                .get_dense(vid as u32)
                .unwrap();
            assert_eq!(lf.degree(), rf.degree(), "forward degree mismatch at {vid}");
            assert_eq!(
                lf.is_tombstone(),
                rf.is_tombstone(),
                "forward tombstone mismatch at {vid}"
            );
            assert_eq!(lr.degree(), rr.degree(), "reverse degree mismatch at {vid}");
            assert_eq!(
                lr.is_tombstone(),
                rr.is_tombstone(),
                "reverse tombstone mismatch at {vid}"
            );
            assert_eq!(
                logical_neighbors_out(left, vid),
                logical_neighbors_out(right, vid),
                "logical out neighborhood mismatch at {vid}"
            );
            assert_eq!(
                logical_neighbors_in(left, vid),
                logical_neighbors_in(right, vid),
                "logical in neighborhood mismatch at {vid}"
            );
        }
    }

    #[test]
    fn delete_vertex_partial_sync_matches_full_sync() {
        let partial = new_graph();
        let full = new_graph();
        populate_graph(&partial);
        populate_graph(&full);

        partial.delete_vertex(1).unwrap();
        full.delete_vertex_full_sync_for_test(1).unwrap();
        assert_graph_equiv(&partial, &full);

        partial.delete_vertex(0).unwrap();
        full.delete_vertex_full_sync_for_test(0).unwrap();
        assert_graph_equiv(&partial, &full);

        let _ = partial.gc_step(32).unwrap();
        let _ = full.gc_step(32).unwrap();
        assert_graph_equiv(&partial, &full);
    }
}

impl<V, E, M, QM> CsrGraphWithGcQueue<V, E, M, M, M, M, M, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    M: Memory,
    QM: Memory,
{
    /// Like [`CsrGraph::format_new`] plus a **seventh** `Memory` for [`StableVecDeque`](crate::StableVecDeque)`<`[`GcWorkItem`](crate::csr::gc_work_item::GcWorkItem)`>`.
    #[allow(clippy::too_many_arguments)]
    pub fn format_new_with_gc_queue(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edge_counts: M,
        forward_edges_and_log: M,
        reverse_segment_edge_counts: M,
        reverse_edges_and_log: M,
        mem_gc_work_queue: QM,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
        maintain_thresholds: Option<SegmentMaintainThresholds>,
    ) -> Result<Self, CsrGraphError> {
        let graph = CsrGraph::format_new(
            mem_vertices_forward,
            mem_vertices_reverse,
            forward_segment_edge_counts,
            forward_edges_and_log,
            reverse_segment_edge_counts,
            reverse_edges_and_log,
            elem_capacity,
            segment_count,
            segment_size,
            num_edges,
        )?;
        let work_queue =
            StableVecDeque::new(mem_gc_work_queue).map_err(|e| CsrGraphError::GcQueue(e.into()))?;
        Ok(Self {
            graph,
            work_queue,
            maintain_thresholds: maintain_thresholds.unwrap_or_default(),
        })
    }
}
