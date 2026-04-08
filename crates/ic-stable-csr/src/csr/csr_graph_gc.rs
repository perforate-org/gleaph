//! [`CsrGraph`] plus a seventh [`Memory`] backing a persisted [`StableVecDeque`](crate::StableVecDeque)
//! **segment maintenance** queue (per DGAP leaf: tombstone compaction + PMA rebalance).

use ic_stable_structures::Memory;

use crate::StableVecDeque;
use crate::csr::{DgapStores, DgapStoresError};
use crate::csr::csr_graph::{CsrGraph, CsrGraphError, LogicalNeighborhoodIter};
use crate::csr::gc_work_item::{
    GC_TAG_SEGMENT_FWD, GC_TAG_SEGMENT_REV, GC_TAG_VERTEX, GcWorkItem,
};
use crate::dgap::{
    RebalanceDecision, rebalance_decision_with_reader, segment_maintenance_decision,
    SegmentMaintainAction,
    SegmentMaintainThresholds,
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
        let action = segment_maintenance_decision(
            leaf_counts,
            reb,
            queue_len,
            &self.maintain_thresholds,
        );
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
        vertex_set_dense(&fwd.vertices, src, fs.with_degree(fs.degree().saturating_sub(1)))?;
        vertex_set_dense(&rev.vertices, dst, rd.with_degree(rd.degree().saturating_sub(1)))?;
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
            let fs = fwd
                .vertices
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
            vertex_set_dense(&fwd.vertices, owner, fs.with_degree(fs.degree().saturating_sub(1)))?;
            vertex_set_dense(&rev.vertices, nb, rd.with_degree(rd.degree().saturating_sub(1)))?;
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
        let fwd = self.graph.forward_dgap();
        let rev = self.graph.reverse_dgap();
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
            let ru = rev
                .vertices
                .get_dense(u as u32)
                .ok_or(CsrGraphError::VertexOutOfRange {
                    vid: u,
                    len: rev.vertices.len(),
                })?;
            vertex_set_dense(&rev.vertices, u, ru.with_degree(ru.degree().saturating_sub(1)))?;
        }
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
            let fu = fwd
                .vertices
                .get_dense(u as u32)
                .ok_or(CsrGraphError::VertexOutOfRange {
                    vid: u,
                    len: fwd.vertices.len(),
                })?;
            vertex_set_dense(&fwd.vertices, u, fu.with_degree(fu.degree().saturating_sub(1)))?;
        }
        let fv = fwd.vertices.get_dense(vid as u32).unwrap();
        let rv = rev.vertices.get_dense(vid as u32).unwrap();
        vertex_set_dense(&fwd.vertices, vid, fv.with_tombstone(true))?;
        vertex_set_dense(&rev.vertices, vid, rv.with_tombstone(true))?;
        self.graph.refresh_slab_occupied_tail_meta()?;
        // Phase D (partial PMA sync): full dual-column recount is conservative. A tighter approach
        // would union `sync_pma_meta_for_vertex_range` over `vid`, each neighbor’s segment, and
        // any segment touched by the degree bumps above, then prove SEC internal nodes stay
        // consistent (or add targeted `bump_segment_edge_counts_leaf_delta`). Before changing this,
        // add tests that compare partial sync vs `sync_pma_meta()` on random small graphs after
        // `delete_vertex` (forward + reverse SEC, rebalance triggers, tombstone neighbors).
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
                    let leaf = item.leaf_segment_id().ok_or(CsrGraphError::LogicalMutation(
                        "gc: bad segment fwd item",
                    ))?;
                    let map_g =
                        |m: &'static str| CsrGraphError::Forward(DgapStoresError::Graph(m));
                    self.run_segment_maintain_for_leaf(self.graph.forward_dgap(), leaf, map_g)?;
                    true
                }
                GC_TAG_SEGMENT_REV => {
                    let leaf = item.leaf_segment_id().ok_or(CsrGraphError::LogicalMutation(
                        "gc: bad segment rev item",
                    ))?;
                    let map_g =
                        |m: &'static str| CsrGraphError::Reverse(DgapStoresError::Graph(m));
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
