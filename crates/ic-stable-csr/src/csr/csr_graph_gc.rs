//! [`CsrGraph`] plus a ninth [`Memory`] backing a persisted [`StableVecDeque`](crate::StableVecDeque) GC work queue.

use ic_stable_structures::Memory;

use crate::StableVecDeque;
use crate::csr::DgapStoresError;
use crate::csr::csr_graph::{CsrGraph, CsrGraphError, LogicalNeighborhoodIter};
use crate::csr::gc_work_item::{
    GC_TAG_EDGE_DIRECTED, GC_TAG_EDGE_UNDIRECTED, GC_TAG_VERTEX, GcWorkItem,
};
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

/// Bidirectional CSR with a stable work queue for lazy physical compaction.
pub struct CsrGraphWithGcQueue<V, E, Mvs, F1, F2, F3, R1, R2, R3, QM>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    F3: Memory,
    R1: Memory,
    R2: Memory,
    R3: Memory,
    QM: Memory,
{
    graph: CsrGraph<V, E, Mvs, F1, F2, F3, R1, R2, R3>,
    work_queue: StableVecDeque<GcWorkItem, QM>,
}

impl<V, E, Mvs, F1, F2, F3, R1, R2, R3, QM> CsrGraphWithGcQueue<V, E, Mvs, F1, F2, F3, R1, R2, R3, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    F3: Memory,
    R1: Memory,
    R2: Memory,
    R3: Memory,
    QM: Memory,
{
    pub fn graph(&self) -> &CsrGraph<V, E, Mvs, F1, F2, F3, R1, R2, R3> {
        &self.graph
    }

    pub fn work_queue_len(&self) -> u64 {
        self.work_queue.len()
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

    /// Logical delete: tombstones both orientations, decrements stored degrees, enqueues physical GC.
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
        self.graph.sync_pma_meta()?;
        self.push_queue(GcWorkItem::edge_directed(src as u32, dst as u32))?;
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
        }
        self.graph.sync_pma_meta()?;
        if changed {
            self.push_queue(GcWorkItem::edge_undirected(u as u32, v as u32))?;
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
        self.graph.sync_pma_meta()?;
        self.push_queue(GcWorkItem::vertex_delete(vid as u64))?;
        Ok(())
    }

    pub fn out_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, V, Mvs, F1, F2, F3>, CsrGraphError> {
        self.graph.out_edges_logical(vid)
    }

    pub fn in_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, V, Mvs, R1, R2, R3>, CsrGraphError> {
        self.graph.in_edges_logical(vid)
    }

    /// Drain at most `budget` queue entries (each entry may perform several stable writes).
    pub fn gc_step(&self, budget: usize) -> Result<usize, CsrGraphError> {
        let mut completed = 0usize;
        for _ in 0..budget {
            if self.work_queue.is_empty() {
                break;
            }
            let item = self.work_queue.get(0).expect("len > 0");
            let done = match item.tag() {
                GC_TAG_EDGE_DIRECTED => self.gc_one_edge_directed(item)?,
                GC_TAG_EDGE_UNDIRECTED => self.gc_one_edge_undirected(item)?,
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
        self.graph.sync_pma_meta()?;
        Ok(completed)
    }

    fn gc_one_edge_directed(&self, item: GcWorkItem) -> Result<bool, CsrGraphError> {
        let (src, dst) = item
            .edge_endpoints()
            .ok_or(CsrGraphError::LogicalMutation("gc: bad edge work item"))?;
        let src = src as usize;
        let dst = dst as usize;
        let g = &self.graph;
        let fwd = g.forward_dgap();
        let rev = g.reverse_dgap();
        let fi = fwd
            .edges
            .neighbor_local_index::<V, Mvs>(&fwd.vertices, src, dst)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        if fi.is_none() {
            return Ok(true);
        }
        let i = fi.unwrap();
        let h = fwd
            .edges
            .header()
            .ok_or(CsrGraphError::LogicalMutation("gc: no edge header"))?;
        let stride = h.edge_stride;
        let base = fwd
            .vertices
            .get_dense(src as u32)
            .ok_or(CsrGraphError::VertexOutOfRange {
                vid: src,
                len: fwd.vertices.len(),
            })?
            .base_slot_start();
        let e = fwd.edges.read_slot(stride, base.saturating_add(i as u64));
        if !e.is_tombstone() {
            return Err(CsrGraphError::LogicalMutation(
                "gc: expected tombstone edge for directed gc",
            ));
        }
        fwd.edges
            .remove_slab_edge_at_local_index_physically::<V, Mvs>(&fwd.vertices, src, i)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        let j = rev
            .edges
            .neighbor_local_index::<V, Mvs>(&rev.vertices, dst, src)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?
            .ok_or(CsrGraphError::LogicalMutation(
                "gc: reverse edge missing after forward remove",
            ))?;
        rev.edges
            .remove_slab_edge_at_local_index_physically::<V, Mvs>(&rev.vertices, dst, j)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        Ok(true)
    }

    fn gc_one_edge_undirected(&self, item: GcWorkItem) -> Result<bool, CsrGraphError> {
        let (u, v) = item.edge_endpoints().ok_or(CsrGraphError::LogicalMutation(
            "gc: bad undirected work item",
        ))?;
        let u = u as usize;
        let v = v as usize;
        if u == v {
            return self.gc_one_edge_directed(GcWorkItem::edge_directed(u as u32, u as u32));
        }
        for (a, b) in [(u, v), (v, u)] {
            if self.gc_try_remove_one_tombstone_directed(a, b)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Returns `true` if one directed tombstone pair `(src → dst)` was physically removed.
    fn gc_try_remove_one_tombstone_directed(
        &self,
        src: usize,
        dst: usize,
    ) -> Result<bool, CsrGraphError> {
        let g = &self.graph;
        let fwd = g.forward_dgap();
        let rev = g.reverse_dgap();
        let fi = fwd
            .edges
            .neighbor_local_index::<V, Mvs>(&fwd.vertices, src, dst)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        let Some(i) = fi else {
            return Ok(false);
        };
        let h = fwd
            .edges
            .header()
            .ok_or(CsrGraphError::LogicalMutation("gc: no edge header"))?;
        let stride = h.edge_stride;
        let base = fwd
            .vertices
            .get_dense(src as u32)
            .ok_or(CsrGraphError::VertexOutOfRange {
                vid: src,
                len: fwd.vertices.len(),
            })?
            .base_slot_start();
        let e = fwd.edges.read_slot(stride, base.saturating_add(i as u64));
        if !e.is_tombstone() {
            return Ok(false);
        }
        fwd.edges
            .remove_slab_edge_at_local_index_physically::<V, Mvs>(&fwd.vertices, src, i)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        let j = rev
            .edges
            .neighbor_local_index::<V, Mvs>(&rev.vertices, dst, src)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?
            .ok_or(CsrGraphError::LogicalMutation(
                "gc: reverse edge missing after forward remove",
            ))?;
        rev.edges
            .remove_slab_edge_at_local_index_physically::<V, Mvs>(&rev.vertices, dst, j)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        Ok(true)
    }

    fn gc_one_vertex(&self, item: GcWorkItem) -> Result<bool, CsrGraphError> {
        let vid = item
            .vertex_id()
            .ok_or(CsrGraphError::LogicalMutation("gc: bad vertex work item"))?
            as usize;
        let g = &self.graph;
        let fwd = g.forward_dgap();
        let rev = g.reverse_dgap();
        let fd = fwd.vertices.get_dense(vid as u32).unwrap().degree();
        if fd > 0 {
            let edges = fwd
                .edges
                .collect_out_edges(&fwd.vertices, vid)
                .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
            let dst = edges[0].neighbor_vid();
            fwd.edges
                .remove_slab_edge_at_local_index_physically::<V, Mvs>(&fwd.vertices, vid, 0)
                .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
            if let Some(j) = rev
                .edges
                .neighbor_local_index::<V, Mvs>(&rev.vertices, dst, vid)
                .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?
            {
                rev.edges
                    .remove_slab_edge_at_local_index_physically::<V, Mvs>(&rev.vertices, dst, j)
                    .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
            }
            return Ok(false);
        }
        let rd = rev.vertices.get_dense(vid as u32).unwrap().degree();
        if rd > 0 {
            let edges = rev
                .edges
                .collect_out_edges(&rev.vertices, vid)
                .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
            let src = edges[0].neighbor_vid();
            rev.edges
                .remove_slab_edge_at_local_index_physically::<V, Mvs>(&rev.vertices, vid, 0)
                .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
            if let Some(j) = fwd
                .edges
                .neighbor_local_index::<V, Mvs>(&fwd.vertices, src, vid)
                .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?
            {
                fwd.edges
                    .remove_slab_edge_at_local_index_physically::<V, Mvs>(&fwd.vertices, src, j)
                    .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
            }
            return Ok(false);
        }
        Ok(true)
    }
}

impl<V, E, M, QM> CsrGraphWithGcQueue<V, E, M, M, M, M, M, M, M, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    M: Memory,
    QM: Memory,
{
    /// Like [`CsrGraph::format_new`] plus a **ninth** `Memory` for [`StableVecDeque`](crate::StableVecDeque)`<`[`GcWorkItem`](crate::csr::gc_work_item::GcWorkItem)`>`.
    #[allow(clippy::too_many_arguments)]
    pub fn format_new_with_gc_queue(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edges_actual: M,
        forward_segment_edges_total: M,
        forward_edges_and_log: M,
        reverse_segment_edges_actual: M,
        reverse_segment_edges_total: M,
        reverse_edges_and_log: M,
        mem_gc_work_queue: QM,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
    ) -> Result<Self, CsrGraphError> {
        let graph = CsrGraph::format_new(
            mem_vertices_forward,
            mem_vertices_reverse,
            forward_segment_edges_actual,
            forward_segment_edges_total,
            forward_edges_and_log,
            reverse_segment_edges_actual,
            reverse_segment_edges_total,
            reverse_edges_and_log,
            elem_capacity,
            segment_count,
            segment_size,
            num_edges,
        )?;
        let work_queue =
            StableVecDeque::new(mem_gc_work_queue).map_err(|e| CsrGraphError::GcQueue(e.into()))?;
        Ok(Self { graph, work_queue })
    }
}
