//! Queue-backed CSR wrappers for lazy per-leaf physical compaction.

use std::collections::BTreeSet;

use ic_stable_structures::Memory;

use crate::StableVecDeque;
use crate::csr::csr_graph::{
    CsrGraphBase, CsrGraphDenseDeleted, CsrGraphError, CsrGraphRowTombstone,
    CsrGraphSparseDeleted, DeletedVertexState, DenseDeletedIndex, LogicalNeighborhoodIter,
    RowTombstoneDeleted, SparseDeletedIndex,
};
use crate::csr::gc_work_item::{GC_TAG_SEGMENT_FWD, GC_TAG_SEGMENT_REV, GcWorkItem};
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

fn leaf_segment_for_vid(vid: usize, segment_size: u32) -> usize {
    dgap_leaf_segment_id(vid, segment_size) as usize
}

struct GcGraphCtx<'a, V, E, Mvs, F1, F2, R1, R2, D, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    D: DeletedVertexState<V, Mvs>,
    QM: Memory,
{
    graph: &'a CsrGraphBase<V, E, Mvs, F1, F2, R1, R2, D>,
    work_queue: &'a StableVecDeque<GcWorkItem, QM>,
    maintain_thresholds: SegmentMaintainThresholds,
}

impl<'a, V, E, Mvs, F1, F2, R1, R2, D, QM> GcGraphCtx<'a, V, E, Mvs, F1, F2, R1, R2, D, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    D: DeletedVertexState<V, Mvs>,
    QM: Memory,
{
    fn work_queue_len(&self) -> u64 {
        self.work_queue.len()
    }

    fn push_queue(&self, item: GcWorkItem) -> Result<(), CsrGraphError> {
        self.work_queue
            .push_back(&item)
            .map_err(|e| CsrGraphError::GcQueue(e.into()))
    }

    fn ensure_endpoint_live(&self, vid: usize) -> Result<(), CsrGraphError> {
        if self.graph.vertex_deleted(vid) {
            return Err(CsrGraphError::EndpointTombstone { vid });
        }
        Ok(())
    }

    fn schedule_maintain_for_leaf<M1: Memory, M2: Memory>(
        &self,
        stores: &DgapStores<V, E, Mvs, M1, M2>,
        leaf: usize,
        queue_len: u64,
        forward_column: bool,
    ) -> Result<(), CsrGraphError> {
        let h = stores
            .edges
            .header()
            .ok_or(CsrGraphError::LogicalMutation("no edge header"))?;
        let sc = h.segment_count as usize;
        let n = stores.vertices.len() as usize;
        let pma_idx = leaf + sc;
        let leaf_counts = stores.edges.read_segment_edge_counts(pma_idx);
        let ss = h.segment_size.max(1) as usize;
        let pivot_vid = leaf.saturating_mul(ss).min(n.saturating_sub(1));
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
        let item = if forward_column {
            GcWorkItem::segment_maintain_forward(leaf as u32)
        } else {
            GcWorkItem::segment_maintain_reverse(leaf as u32)
        };
        match action {
            SegmentMaintainAction::Noop => Ok(()),
            SegmentMaintainAction::Enqueue | SegmentMaintainAction::InlineNow => self.push_queue(item),
        }
    }

    fn enqueue_maintain_for_leafs<M1: Memory, M2: Memory>(
        &self,
        stores: &DgapStores<V, E, Mvs, M1, M2>,
        leafs: &BTreeSet<usize>,
        queue_len: u64,
        forward_column: bool,
    ) -> Result<(), CsrGraphError> {
        for &leaf in leafs {
            self.schedule_maintain_for_leaf(stores, leaf, queue_len, forward_column)?;
        }
        Ok(())
    }

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
                stores.edges.resize_double(&stores.vertices).map_err(&map_g)?;
                stores.sync_pma_meta().map_err(&map_g)?;
                h = stores
                    .edges
                    .header()
                    .ok_or(CsrGraphError::LogicalMutation("no edge header"))?;
                continue;
            }
            stores
                .edges
                .maintain_segment_leaf_plan_and_commit(&stores.vertices, left, right, pma_idx, |vid| {
                    self.graph.vertex_deleted(vid)
                })
                .map_err(&map_g)?;
            return Ok(());
        }
        Err(CsrGraphError::LogicalMutation(
            "segment maintain: resize loop limit",
        ))
    }

    fn tombstone_directed_edge_pair(&self, src: usize, dst: usize) -> Result<bool, CsrGraphError> {
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
            return Ok(false);
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
        Ok(true)
    }

    fn delete_vertex_collect_tombstones(
        &self,
        vid: usize,
    ) -> Result<(BTreeSet<usize>, BTreeSet<usize>), CsrGraphError> {
        let fwd = self.graph.forward_dgap();
        let rev = self.graph.reverse_dgap();
        let h = fwd
            .edges
            .header()
            .ok_or(CsrGraphError::LogicalMutation("no edge header"))?;
        let mut forward_touched = BTreeSet::new();
        let mut reverse_touched = BTreeSet::new();

        let out_raw = fwd
            .edges
            .collect_out_edges(&fwd.vertices, vid)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        let removed_out = out_raw.iter().filter(|e| !e.is_tombstone()).count() as i64;
        if removed_out > 0 {
            let fv = fwd.vertices.get_dense(vid as u32).unwrap();
            vertex_set_dense(&fwd.vertices, vid, fv.with_degree(0).with_log_head(-1))?;
            fwd.edges
                .bump_segment_edge_counts_leaf_delta(vid, -removed_out, 0, removed_out)
                .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        }
        forward_touched.insert(leaf_segment_for_vid(vid, h.segment_size));

        let in_raw = rev
            .edges
            .collect_out_edges(&rev.vertices, vid)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        let removed_in = in_raw.iter().filter(|e| !e.is_tombstone()).count() as i64;
        if removed_in > 0 {
            let rv = rev.vertices.get_dense(vid as u32).unwrap();
            vertex_set_dense(&rev.vertices, vid, rv.with_degree(0).with_log_head(-1))?;
            rev.edges
                .bump_segment_edge_counts_leaf_delta(vid, -removed_in, 0, removed_in)
                .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        }
        reverse_touched.insert(leaf_segment_for_vid(vid, h.segment_size));

        self.graph.mark_vertex_deleted(vid)?;
        let fv = fwd.vertices.get_dense(vid as u32).unwrap();
        let rv = rev.vertices.get_dense(vid as u32).unwrap();
        vertex_set_dense(&fwd.vertices, vid, fv.with_tombstone(true))?;
        vertex_set_dense(&rev.vertices, vid, rv.with_tombstone(true))?;
        self.graph.refresh_slab_occupied_tail_meta()?;
        Ok((forward_touched, reverse_touched))
    }

    fn delete_edge_directed(&self, src: usize, dst: usize) -> Result<(), CsrGraphError> {
        self.graph.ensure_vertex(src)?;
        self.graph.ensure_vertex(dst)?;
        let changed = self.tombstone_directed_edge_pair(src, dst)?;
        if !changed {
            return Ok(());
        }
        self.graph.refresh_slab_occupied_tail_meta()?;
        let qlen = self.work_queue_len();
        self.schedule_maintain_for_leaf(
            self.graph.forward_dgap(),
            leaf_segment_for_vid(
                src,
                self.graph
                    .forward_dgap()
                    .edges
                    .header()
                    .ok_or(CsrGraphError::LogicalMutation("no edge header"))?
                    .segment_size,
            ),
            qlen,
            true,
        )?;
        self.schedule_maintain_for_leaf(
            self.graph.reverse_dgap(),
            leaf_segment_for_vid(
                dst,
                self.graph
                    .reverse_dgap()
                    .edges
                    .header()
                    .ok_or(CsrGraphError::LogicalMutation("no edge header"))?
                    .segment_size,
            ),
            qlen,
            false,
        )?;
        Ok(())
    }

    fn delete_edge_undirected(&self, u: usize, v: usize) -> Result<(), CsrGraphError>
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
            let fs = fwd.vertices.get_dense(owner as u32).unwrap();
            let rd = rev.vertices.get_dense(nb as u32).unwrap();
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
            let qlen = self.work_queue_len();
            let h = fwd
                .edges
                .header()
                .ok_or(CsrGraphError::LogicalMutation("no edge header"))?;
            self.schedule_maintain_for_leaf(fwd, leaf_segment_for_vid(u, h.segment_size), qlen, true)?;
            self.schedule_maintain_for_leaf(rev, leaf_segment_for_vid(v, h.segment_size), qlen, false)?;
            self.schedule_maintain_for_leaf(fwd, leaf_segment_for_vid(v, h.segment_size), qlen, true)?;
            self.schedule_maintain_for_leaf(rev, leaf_segment_for_vid(u, h.segment_size), qlen, false)?;
        }
        Ok(())
    }

    fn delete_vertex(&self, vid: usize) -> Result<(), CsrGraphError> {
        self.graph.ensure_vertex(vid)?;
        if self.graph.vertex_deleted(vid) {
            return Ok(());
        }
        let (forward_touched, reverse_touched) = self.delete_vertex_collect_tombstones(vid)?;
        let qlen = self.work_queue_len();
        let fwd = self.graph.forward_dgap();
        let rev = self.graph.reverse_dgap();
        let segs: Vec<_> = forward_touched.iter().copied().collect();
        fwd.edges
            .sync_pma_edge_counts_for_segments(&fwd.vertices, &segs)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        let segs: Vec<_> = reverse_touched.iter().copied().collect();
        rev.edges
            .sync_pma_edge_counts_for_segments(&rev.vertices, &segs)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        self.enqueue_maintain_for_leafs(fwd, &forward_touched, qlen, true)?;
        self.enqueue_maintain_for_leafs(rev, &reverse_touched, qlen, false)?;
        Ok(())
    }

    fn gc_step(&self, budget: usize) -> Result<usize, CsrGraphError> {
        let mut completed = 0usize;
        for _ in 0..budget {
            if self.work_queue.is_empty() {
                break;
            }
            let item = self.work_queue.get(0).expect("len > 0");
            match item.tag() {
                GC_TAG_SEGMENT_FWD => {
                    let leaf = item
                        .leaf_segment_id()
                        .ok_or(CsrGraphError::LogicalMutation("gc: bad segment fwd item"))?;
                    let map_g = |m: &'static str| CsrGraphError::Forward(DgapStoresError::Graph(m));
                    self.run_segment_maintain_for_leaf(self.graph.forward_dgap(), leaf, map_g)?;
                }
                GC_TAG_SEGMENT_REV => {
                    let leaf = item
                        .leaf_segment_id()
                        .ok_or(CsrGraphError::LogicalMutation("gc: bad segment rev item"))?;
                    let map_g = |m: &'static str| CsrGraphError::Reverse(DgapStoresError::Graph(m));
                    self.run_segment_maintain_for_leaf(self.graph.reverse_dgap(), leaf, map_g)?;
                }
                _ => {}
            }
            self.work_queue.pop_front();
            completed += 1;
        }
        Ok(completed)
    }
}

pub struct CsrGraphWithGcQueueRowTombstone<V, E, Mvs, F1, F2, R1, R2, QM>
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
    graph: CsrGraphRowTombstone<V, E, Mvs, F1, F2, R1, R2>,
    work_queue: StableVecDeque<GcWorkItem, QM>,
    maintain_thresholds: SegmentMaintainThresholds,
}

pub struct CsrGraphWithGcQueueDenseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv, QM>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    Dv: Memory,
    QM: Memory,
{
    graph: CsrGraphDenseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv>,
    work_queue: StableVecDeque<GcWorkItem, QM>,
    maintain_thresholds: SegmentMaintainThresholds,
}

pub struct CsrGraphWithGcQueueSparseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv, QM>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    Dv: Memory,
    QM: Memory,
{
    graph: CsrGraphSparseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv>,
    work_queue: StableVecDeque<GcWorkItem, QM>,
    maintain_thresholds: SegmentMaintainThresholds,
}

macro_rules! impl_gc_common {
    ($name:ident, $graph_ty:ident, $deleted_ty:ty $(, $dv:ident)?) => {
        impl<V, E, Mvs, F1, F2, R1, R2, QM $(, $dv)?> $name<V, E, Mvs, F1, F2, R1, R2 $(, $dv)?, QM>
        where
            V: CsrVertex + CsrVertexTombstone,
            E: CsrEdge + CsrEdgeTombstone,
            Mvs: Memory,
            F1: Memory,
            F2: Memory,
            R1: Memory,
            R2: Memory,
            QM: Memory,
            $($dv: Memory,)?
        {
            pub fn graph(&self) -> &$graph_ty<V, E, Mvs, F1, F2, R1, R2 $(, $dv)?> {
                &self.graph
            }

            pub fn work_queue_len(&self) -> u64 {
                self.work_queue.len()
            }

            pub fn maintain_thresholds(&self) -> SegmentMaintainThresholds {
                self.maintain_thresholds
            }

            fn ctx(&self) -> GcGraphCtx<'_, V, E, Mvs, F1, F2, R1, R2, $deleted_ty, QM> {
                GcGraphCtx {
                    graph: &self.graph.inner,
                    work_queue: &self.work_queue,
                    maintain_thresholds: self.maintain_thresholds,
                }
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
                self.ctx().ensure_endpoint_live(src)?;
                self.ctx().ensure_endpoint_live(dst)?;
                if self.graph.inner.has_forward_slot_to_neighbor(src, dst)? {
                    return Err(CsrGraphError::AdjacencySlotOccupied { src, dst });
                }
                self.graph.insert_directed(src, dst, edge)
            }

            pub fn insert_undirected(&self, u: usize, v: usize, edge: E) -> Result<(), CsrGraphError>
            where
                E: CsrEdgeUndirected,
            {
                self.ctx().ensure_endpoint_live(u)?;
                self.ctx().ensure_endpoint_live(v)?;
                if u != v {
                    if self.graph.inner.has_forward_slot_to_neighbor(u, v)? {
                        return Err(CsrGraphError::AdjacencySlotOccupied { src: u, dst: v });
                    }
                    if self.graph.inner.has_forward_slot_to_neighbor(v, u)? {
                        return Err(CsrGraphError::AdjacencySlotOccupied { src: v, dst: u });
                    }
                }
                self.graph.insert_undirected(u, v, edge)
            }

            pub fn delete_edge_directed(&self, src: usize, dst: usize) -> Result<(), CsrGraphError> {
                self.ctx().delete_edge_directed(src, dst)
            }

            pub fn delete_edge_undirected(&self, u: usize, v: usize) -> Result<(), CsrGraphError>
            where
                E: CsrEdgeUndirected,
            {
                self.ctx().delete_edge_undirected(u, v)
            }

            pub fn delete_vertex(&self, vid: usize) -> Result<(), CsrGraphError> {
                self.ctx().delete_vertex(vid)
            }

            pub fn gc_step(&self, budget: usize) -> Result<usize, CsrGraphError> {
                self.ctx().gc_step(budget)
            }
        }
    };
}

impl_gc_common!(
    CsrGraphWithGcQueueRowTombstone,
    CsrGraphRowTombstone,
    RowTombstoneDeleted
);
impl_gc_common!(
    CsrGraphWithGcQueueDenseDeleted,
    CsrGraphDenseDeleted,
    DenseDeletedIndex<Dv>,
    Dv
);
impl_gc_common!(
    CsrGraphWithGcQueueSparseDeleted,
    CsrGraphSparseDeleted,
    SparseDeletedIndex<Dv>,
    Dv
);

impl<V, E, Mvs, F1, F2, R1, R2, Dv, QM>
    CsrGraphWithGcQueueDenseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    Dv: Memory,
    QM: Memory,
{
    pub fn out_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, DenseDeletedIndex<Dv>, F1, F2>, CsrGraphError>
    {
        self.graph.out_edges_logical(vid)
    }

    pub fn in_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, DenseDeletedIndex<Dv>, R1, R2>, CsrGraphError>
    {
        self.graph.in_edges_logical(vid)
    }
}

impl<V, E, Mvs, F1, F2, R1, R2, Dv, QM>
    CsrGraphWithGcQueueSparseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    Dv: Memory,
    QM: Memory,
{
    pub fn out_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, SparseDeletedIndex<Dv>, F1, F2>, CsrGraphError>
    {
        self.graph.out_edges_logical(vid)
    }

    pub fn in_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, SparseDeletedIndex<Dv>, R1, R2>, CsrGraphError>
    {
        self.graph.in_edges_logical(vid)
    }
}

impl<V, E, M, QM> CsrGraphWithGcQueueRowTombstone<V, E, M, M, M, M, M, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    M: Memory,
    QM: Memory,
{
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
        let graph = CsrGraphRowTombstone::format_new(
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

impl<V, E, M, Dv, QM> CsrGraphWithGcQueueDenseDeleted<V, E, M, M, M, M, M, Dv, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    M: Memory,
    Dv: Memory,
    QM: Memory,
{
    #[allow(clippy::too_many_arguments)]
    pub fn format_new_with_gc_queue(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edge_counts: M,
        forward_edges_and_log: M,
        reverse_segment_edge_counts: M,
        reverse_edges_and_log: M,
        mem_deleted_vertices: Dv,
        mem_gc_work_queue: QM,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
        maintain_thresholds: Option<SegmentMaintainThresholds>,
    ) -> Result<Self, CsrGraphError> {
        let graph = CsrGraphDenseDeleted::format_new(
            mem_vertices_forward,
            mem_vertices_reverse,
            forward_segment_edge_counts,
            forward_edges_and_log,
            reverse_segment_edge_counts,
            reverse_edges_and_log,
            mem_deleted_vertices,
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

impl<V, E, M, Dv, QM> CsrGraphWithGcQueueSparseDeleted<V, E, M, M, M, M, M, Dv, QM>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    M: Memory,
    Dv: Memory,
    QM: Memory,
{
    #[allow(clippy::too_many_arguments)]
    pub fn format_new_with_gc_queue(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edge_counts: M,
        forward_edges_and_log: M,
        reverse_segment_edge_counts: M,
        reverse_edges_and_log: M,
        mem_deleted_vertices: Dv,
        mem_gc_work_queue: QM,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
        maintain_thresholds: Option<SegmentMaintainThresholds>,
    ) -> Result<Self, CsrGraphError> {
        let graph = CsrGraphSparseDeleted::format_new(
            mem_vertices_forward,
            mem_vertices_reverse,
            forward_segment_edge_counts,
            forward_edges_and_log,
            reverse_segment_edge_counts,
            reverse_edges_and_log,
            mem_deleted_vertices,
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
