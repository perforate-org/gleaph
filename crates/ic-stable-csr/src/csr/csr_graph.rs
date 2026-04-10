//! Bidirectional CSR: forward out-adjacency + reverse (transpose) in-adjacency.

use std::fmt;
use std::marker::PhantomData;

use ic_stable_bitset::{BitSet, ContainsView as BitSetContainsView};
use ic_stable_roaring::{ContainsView as RoaringContainsView, StableRoaringBitMap};
use ic_stable_slot_map::SlotMap;
use ic_stable_structures::Memory;

use crate::csr::{DgapStores, DgapStoresError};
use crate::dgap::{DgapEdgeStore, DgapGraphMemories, NeighborhoodIter};
use crate::memory_util::GrowFailed;
use crate::traits::{CsrEdge, CsrEdgeTombstone, CsrEdgeUndirected, CsrVertex, CsrVertexTombstone};

trait UndirectedEdgeFlag {
    fn marked_undirected(&self) -> bool;
}

impl<E: CsrEdge> UndirectedEdgeFlag for E {
    default fn marked_undirected(&self) -> bool {
        false
    }
}

impl<E: CsrEdge + CsrEdgeUndirected> UndirectedEdgeFlag for E {
    fn marked_undirected(&self) -> bool {
        CsrEdgeUndirected::is_undirected(self)
    }
}

/// Failure from CSR graph operations.
#[derive(Debug, PartialEq, Eq)]
pub enum CsrGraphError {
    Forward(DgapStoresError),
    Reverse(DgapStoresError),
    Format(GrowFailed),
    VertexCountMismatch {
        forward: u64,
        reverse: u64,
    },
    VertexOutOfRange {
        vid: usize,
        len: u64,
    },
    NeighborMismatch {
        expected: usize,
        actual: usize,
    },
    UndirectedEdgeInDirectedInsert,
    EndpointTombstone {
        vid: usize,
    },
    AdjacencySlotOccupied {
        src: usize,
        dst: usize,
    },
    EdgeNotFound {
        owner: usize,
        neighbor: usize,
    },
    GcQueue(GrowFailed),
    LogicalMutation(&'static str),
}

impl fmt::Display for CsrGraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forward(e) => write!(f, "forward store: {e}"),
            Self::Reverse(e) => write!(f, "reverse store: {e}"),
            Self::Format(e) => write!(f, "format / grow: {e}"),
            Self::VertexCountMismatch { forward, reverse } => write!(
                f,
                "vertex column length mismatch: forward={forward} reverse={reverse}"
            ),
            Self::VertexOutOfRange { vid, len } => {
                write!(f, "vertex {vid} out of range (len={len})")
            }
            Self::NeighborMismatch { expected, actual } => write!(
                f,
                "edge neighbor_vid {actual} does not match dst {expected}"
            ),
            Self::UndirectedEdgeInDirectedInsert => write!(
                f,
                "directed insert: edge is marked undirected; use insert_undirected"
            ),
            Self::EndpointTombstone { vid } => write!(f, "vertex {vid} is tombstoned"),
            Self::AdjacencySlotOccupied { src, dst } => write!(
                f,
                "vertex {src} already has an adjacency slot for neighbor {dst}"
            ),
            Self::EdgeNotFound { owner, neighbor } => {
                write!(f, "no edge from owner {owner} to neighbor {neighbor}")
            }
            Self::GcQueue(e) => write!(f, "gc work queue: {e}"),
            Self::LogicalMutation(s) => write!(f, "logical mutation: {s}"),
        }
    }
}

impl std::error::Error for CsrGraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Forward(e) | Self::Reverse(e) => Some(e),
            Self::Format(e) | Self::GcQueue(e) => Some(e),
            _ => None,
        }
    }
}

impl From<GrowFailed> for CsrGraphError {
    fn from(e: GrowFailed) -> Self {
        Self::Format(e)
    }
}

impl From<ic_stable_slot_map::GrowFailed> for CsrGraphError {
    fn from(e: ic_stable_slot_map::GrowFailed) -> Self {
        Self::Format(GrowFailed {
            current_size_pages: e.current_size_pages(),
            delta_pages: e.delta_pages(),
        })
    }
}

fn bitset_grow_failed(e: ic_stable_bitset::GrowFailed) -> CsrGraphError {
    CsrGraphError::Format(GrowFailed {
        current_size_pages: e.current_size_pages(),
        delta_pages: e.delta_pages(),
    })
}

fn roaring_grow_failed(e: ic_stable_roaring::GrowFailed) -> CsrGraphError {
    CsrGraphError::Format(GrowFailed {
        current_size_pages: e.current_size_pages(),
        delta_pages: e.delta_pages(),
    })
}

#[doc(hidden)]
pub trait DeletedVertexRead {
    type Scan<'a>: DeletedVertexScan
    where
        Self: 'a;

    fn scan_deleted(&self) -> Self::Scan<'_>;
}

#[doc(hidden)]
pub trait DeletedVertexScan {
    fn contains_deleted(&self, vid: usize) -> bool;
}

pub(crate) trait DeletedVertexState<V, Mvs>: Sized
where
    V: CsrVertex + CsrVertexTombstone,
    Mvs: Memory,
{
    fn is_deleted(&self, vertices: &SlotMap<V, Mvs>, vid: usize) -> bool;
    fn mark_deleted(&self, vertices: &SlotMap<V, Mvs>, vid: usize) -> Result<(), CsrGraphError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct RowTombstoneDeleted;

#[doc(hidden)]
#[derive(Debug)]
pub struct DenseDeletedIndex<Dv: Memory> {
    deleted_vertices: BitSet<Dv>,
}

#[doc(hidden)]
#[derive(Debug)]
pub struct SparseDeletedIndex<Dv: Memory> {
    deleted_vertices: StableRoaringBitMap<Dv>,
}

pub struct DenseDeletedScan<'a> {
    view: BitSetContainsView<'a>,
}

pub struct SparseDeletedScan<'a> {
    view: RoaringContainsView<'a>,
}

impl DeletedVertexScan for DenseDeletedScan<'_> {
    #[inline]
    fn contains_deleted(&self, vid: usize) -> bool {
        self.view.contains(vid as u64)
    }
}

impl DeletedVertexScan for SparseDeletedScan<'_> {
    #[inline]
    fn contains_deleted(&self, vid: usize) -> bool {
        self.view.contains(vid as u64)
    }
}

impl<Dv: Memory> DeletedVertexRead for DenseDeletedIndex<Dv> {
    type Scan<'a>
        = DenseDeletedScan<'a>
    where
        Self: 'a;

    #[inline]
    fn scan_deleted(&self) -> Self::Scan<'_> {
        DenseDeletedScan {
            view: self.deleted_vertices.contains_view(),
        }
    }
}

impl<Dv: Memory> DeletedVertexRead for SparseDeletedIndex<Dv> {
    type Scan<'a>
        = SparseDeletedScan<'a>
    where
        Self: 'a;

    #[inline]
    fn scan_deleted(&self) -> Self::Scan<'_> {
        SparseDeletedScan {
            view: self.deleted_vertices.contains_view(),
        }
    }
}

impl<V, Mvs> DeletedVertexState<V, Mvs> for RowTombstoneDeleted
where
    V: CsrVertex + CsrVertexTombstone,
    Mvs: Memory,
{
    fn is_deleted(&self, vertices: &SlotMap<V, Mvs>, vid: usize) -> bool {
        vertices
            .get_dense(vid as u32)
            .map(|row| row.is_tombstone())
            .unwrap_or(false)
    }

    fn mark_deleted(&self, _vertices: &SlotMap<V, Mvs>, _vid: usize) -> Result<(), CsrGraphError> {
        Ok(())
    }
}

impl<V, Mvs, Dv> DeletedVertexState<V, Mvs> for DenseDeletedIndex<Dv>
where
    V: CsrVertex + CsrVertexTombstone,
    Mvs: Memory,
    Dv: Memory,
{
    fn is_deleted(&self, _vertices: &SlotMap<V, Mvs>, vid: usize) -> bool {
        self.deleted_vertices.contains(vid as u64)
    }

    fn mark_deleted(&self, _vertices: &SlotMap<V, Mvs>, vid: usize) -> Result<(), CsrGraphError> {
        self.deleted_vertices
            .insert(vid as u64)
            .map_err(bitset_grow_failed)
    }
}

impl<V, Mvs, Dv> DeletedVertexState<V, Mvs> for SparseDeletedIndex<Dv>
where
    V: CsrVertex + CsrVertexTombstone,
    Mvs: Memory,
    Dv: Memory,
{
    fn is_deleted(&self, _vertices: &SlotMap<V, Mvs>, vid: usize) -> bool {
        self.deleted_vertices.contains(vid as u64)
    }

    fn mark_deleted(&self, _vertices: &SlotMap<V, Mvs>, vid: usize) -> Result<(), CsrGraphError> {
        self.deleted_vertices
            .insert(vid as u64)
            .map_err(roaring_grow_failed)
    }
}

pub(crate) struct CsrGraphBase<V, E, Mvs, F1, F2, R1, R2, D>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
{
    forward: DgapStores<V, E, Mvs, F1, F2>,
    reverse: DgapStores<V, E, Mvs, R1, R2>,
    deleted: D,
}

pub struct CsrGraphRowTombstone<V, E, Mvs, F1, F2, R1, R2>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
{
    pub(crate) inner: CsrGraphBase<V, E, Mvs, F1, F2, R1, R2, RowTombstoneDeleted>,
}

pub struct CsrGraphDenseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    Dv: Memory,
{
    pub(crate) inner: CsrGraphBase<V, E, Mvs, F1, F2, R1, R2, DenseDeletedIndex<Dv>>,
}

pub struct CsrGraphSparseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    Dv: Memory,
{
    pub(crate) inner: CsrGraphBase<V, E, Mvs, F1, F2, R1, R2, SparseDeletedIndex<Dv>>,
}

fn format_stores<V, E, Mvs, F1, F2, R1, R2>(
    mem_vertices_forward: Mvs,
    mem_vertices_reverse: Mvs,
    forward_segment_edge_counts: F1,
    forward_edges_and_log: F2,
    reverse_segment_edge_counts: R1,
    reverse_edges_and_log: R2,
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
    num_edges: u64,
) -> Result<(DgapStores<V, E, Mvs, F1, F2>, DgapStores<V, E, Mvs, R1, R2>), CsrGraphError>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
{
    let vertices_forward = SlotMap::new(mem_vertices_forward).map_err(CsrGraphError::from)?;
    let vertices_reverse = SlotMap::new(mem_vertices_reverse).map_err(CsrGraphError::from)?;

    let fwd_mem = DgapGraphMemories::new(forward_segment_edge_counts, forward_edges_and_log);
    let rev_mem = DgapGraphMemories::new(reverse_segment_edge_counts, reverse_edges_and_log);

    let forward_edges = DgapEdgeStore::new(fwd_mem);
    forward_edges
        .format_new(elem_capacity, segment_count, segment_size, num_edges)
        .map_err(CsrGraphError::from)?;
    let reverse_edges = DgapEdgeStore::new(rev_mem);
    reverse_edges
        .format_new(elem_capacity, segment_count, segment_size, num_edges)
        .map_err(CsrGraphError::from)?;

    Ok((
        DgapStores::new(vertices_forward, forward_edges),
        DgapStores::new(vertices_reverse, reverse_edges),
    ))
}

fn open_stores<V, E, Mvs, F1, F2, R1, R2>(
    mem_vertices_forward: Mvs,
    mem_vertices_reverse: Mvs,
    forward_segment_edge_counts: F1,
    forward_edges_and_log: F2,
    reverse_segment_edge_counts: R1,
    reverse_edges_and_log: R2,
) -> Result<(DgapStores<V, E, Mvs, F1, F2>, DgapStores<V, E, Mvs, R1, R2>), CsrGraphError>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
{
    let vertices_forward = SlotMap::init(mem_vertices_forward)
        .map_err(|_| CsrGraphError::LogicalMutation("open_existing: forward vertices init failed"))?;
    let vertices_reverse = SlotMap::init(mem_vertices_reverse)
        .map_err(|_| CsrGraphError::LogicalMutation("open_existing: reverse vertices init failed"))?;
    if vertices_forward.len() != vertices_reverse.len() {
        return Err(CsrGraphError::VertexCountMismatch {
            forward: vertices_forward.len(),
            reverse: vertices_reverse.len(),
        });
    }

    let forward_edges =
        DgapEdgeStore::new(DgapGraphMemories::new(forward_segment_edge_counts, forward_edges_and_log));
    if forward_edges.header().is_none() {
        return Err(CsrGraphError::LogicalMutation(
            "open_existing: missing forward edge header",
        ));
    }
    let reverse_edges =
        DgapEdgeStore::new(DgapGraphMemories::new(reverse_segment_edge_counts, reverse_edges_and_log));
    if reverse_edges.header().is_none() {
        return Err(CsrGraphError::LogicalMutation(
            "open_existing: missing reverse edge header",
        ));
    }

    Ok((
        DgapStores::new(vertices_forward, forward_edges),
        DgapStores::new(vertices_reverse, reverse_edges),
    ))
}

impl<V, E, Mvs, F1, F2, R1, R2, D> CsrGraphBase<V, E, Mvs, F1, F2, R1, R2, D>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
{
    pub(crate) fn new(
        forward: DgapStores<V, E, Mvs, F1, F2>,
        reverse: DgapStores<V, E, Mvs, R1, R2>,
        deleted: D,
    ) -> Self {
        Self {
            forward,
            reverse,
            deleted,
        }
    }

    pub fn vertex_count(&self) -> u64 {
        self.forward.vertices.len()
    }

    pub(crate) fn ensure_vertex(&self, vid: usize) -> Result<u64, CsrGraphError> {
        let n = self.vertex_count();
        if (vid as u64) >= n {
            return Err(CsrGraphError::VertexOutOfRange { vid, len: n });
        }
        Ok(n)
    }

    pub fn sync_pma_meta(&self) -> Result<(), CsrGraphError> {
        self.forward
            .sync_pma_meta()
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        self.reverse
            .sync_pma_meta()
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        Ok(())
    }

    pub fn sync_pma_meta_for_vertex_range(
        &self,
        left: usize,
        right: usize,
    ) -> Result<(), CsrGraphError> {
        self.forward
            .sync_pma_meta_for_vertex_range(left, right)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        self.reverse
            .sync_pma_meta_for_vertex_range(left, right)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        Ok(())
    }

    pub fn refresh_slab_occupied_tail_meta(&self) -> Result<(), CsrGraphError> {
        self.forward
            .refresh_slab_occupied_tail_meta()
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        self.reverse
            .refresh_slab_occupied_tail_meta()
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        Ok(())
    }

    pub fn insert_vertex(&self, row_template: V) -> Result<u64, CsrGraphError> {
        let id = self
            .forward
            .insert_vertex(row_template)
            .map_err(CsrGraphError::Forward)?;
        self.reverse
            .insert_vertex(row_template)
            .map_err(CsrGraphError::Reverse)?;
        debug_assert_eq!(self.forward.vertices.len(), self.reverse.vertices.len());
        Ok(id)
    }

    pub fn insert_vertex_strict(&self, row_template: V) -> Result<u64, CsrGraphError> {
        let id = self
            .forward
            .insert_vertex_strict(row_template)
            .map_err(CsrGraphError::Forward)?;
        self.reverse
            .insert_vertex_strict(row_template)
            .map_err(CsrGraphError::Reverse)?;
        debug_assert_eq!(self.forward.vertices.len(), self.reverse.vertices.len());
        Ok(id)
    }

    #[doc(hidden)]
    pub fn append_empty_vertices_fast_for_fixture(
        &self,
        row_template: V,
        count: usize,
    ) -> Result<(), CsrGraphError> {
        self.forward
            .append_empty_vertices_fast_for_fixture(row_template, count)
            .map_err(CsrGraphError::Forward)?;
        self.reverse
            .append_empty_vertices_fast_for_fixture(row_template, count)
            .map_err(CsrGraphError::Reverse)?;
        debug_assert_eq!(self.forward.vertices.len(), self.reverse.vertices.len());
        Ok(())
    }

    pub fn insert_directed(&self, src: usize, dst: usize, edge: E) -> Result<(), CsrGraphError> {
        self.ensure_vertex(src)?;
        self.ensure_vertex(dst)?;

        if edge.neighbor_vid() != dst {
            return Err(CsrGraphError::NeighborMismatch {
                expected: dst,
                actual: edge.neighbor_vid(),
            });
        }

        if <E as UndirectedEdgeFlag>::marked_undirected(&edge) {
            return Err(CsrGraphError::UndirectedEdgeInDirectedInsert);
        }

        let rev_slot = edge.with_neighbor_vid(src);
        self.forward
            .insert_edge(src, edge)
            .map_err(CsrGraphError::Forward)?;
        self.reverse
            .insert_edge(dst, rev_slot)
            .map_err(CsrGraphError::Reverse)?;
        Ok(())
    }

    pub fn insert_undirected(&self, u: usize, v: usize, edge: E) -> Result<(), CsrGraphError>
    where
        E: CsrEdgeUndirected,
    {
        self.ensure_vertex(u)?;
        self.ensure_vertex(v)?;

        let edge = edge.with_undirected(true);

        if u == v {
            let loop_e = edge.with_neighbor_vid(u);
            self.forward
                .insert_edge(u, loop_e)
                .map_err(CsrGraphError::Forward)?;
            self.reverse
                .insert_edge(u, loop_e)
                .map_err(CsrGraphError::Reverse)?;
            return Ok(());
        }

        self.forward
            .insert_edge(u, edge.with_neighbor_vid(v))
            .map_err(CsrGraphError::Forward)?;
        self.forward
            .insert_edge(v, edge.with_neighbor_vid(u))
            .map_err(CsrGraphError::Forward)?;

        self.reverse
            .insert_edge(v, edge.with_neighbor_vid(u))
            .map_err(CsrGraphError::Reverse)?;
        self.reverse
            .insert_edge(u, edge.with_neighbor_vid(v))
            .map_err(CsrGraphError::Reverse)?;
        Ok(())
    }

    pub fn out_edges<'a>(
        &'a self,
        vid: usize,
    ) -> Result<NeighborhoodIter<'a, E, F1, F2>, CsrGraphError> {
        self.ensure_vertex(vid)?;
        self.forward
            .edges
            .try_neighborhood_iter(&self.forward.vertices, vid)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))
    }

    pub fn in_edges<'a>(
        &'a self,
        vid: usize,
    ) -> Result<NeighborhoodIter<'a, E, R1, R2>, CsrGraphError> {
        self.ensure_vertex(vid)?;
        self.reverse
            .edges
            .try_neighborhood_iter(&self.reverse.vertices, vid)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))
    }

    pub(crate) fn has_forward_slot_to_neighbor(
        &self,
        src: usize,
        dst: usize,
    ) -> Result<bool, CsrGraphError> {
        self.ensure_vertex(src)?;
        let it = self
            .forward
            .edges
            .try_neighborhood_iter(&self.forward.vertices, src)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        for x in it {
            let e = x.map_err(CsrGraphError::LogicalMutation)?;
            if e.neighbor_vid() == dst {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn forward_dgap(&self) -> &DgapStores<V, E, Mvs, F1, F2> {
        &self.forward
    }

    pub fn reverse_dgap(&self) -> &DgapStores<V, E, Mvs, R1, R2> {
        &self.reverse
    }
}

impl<V, E, Mvs, F1, F2, R1, R2, D> CsrGraphBase<V, E, Mvs, F1, F2, R1, R2, D>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    D: DeletedVertexState<V, Mvs>,
{
    pub(crate) fn vertex_deleted(&self, vid: usize) -> bool {
        self.deleted.is_deleted(&self.forward.vertices, vid)
    }

    pub(crate) fn mark_vertex_deleted(&self, vid: usize) -> Result<(), CsrGraphError> {
        self.deleted.mark_deleted(&self.forward.vertices, vid)
    }
}

/// Neighborhood iterator hiding tombstone edges and edges incident to deleted vertices.
pub enum LogicalNeighborhoodIter<'a, E, D, M1, M2>
where
    E: CsrEdge + CsrEdgeTombstone,
    D: DeletedVertexRead,
    M1: Memory,
    M2: Memory,
{
    Active {
        inner: NeighborhoodIter<'a, E, M1, M2>,
        deleted_vertices: D::Scan<'a>,
        _marker: PhantomData<&'a D>,
    },
    Empty,
}

impl<'a, E, D, M1, M2> Iterator for LogicalNeighborhoodIter<'a, E, D, M1, M2>
where
    E: CsrEdge + CsrEdgeTombstone,
    D: DeletedVertexRead,
    M1: Memory,
    M2: Memory,
{
    type Item = Result<E, &'static str>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            LogicalNeighborhoodIter::Empty => None,
            LogicalNeighborhoodIter::Active {
                inner,
                deleted_vertices,
                ..
            } => loop {
                let x = inner.next()?;
                match x {
                    Err(e) => return Some(Err(e)),
                    Ok(e) => {
                        if e.is_tombstone() {
                            continue;
                        }
                        if deleted_vertices.contains_deleted(e.neighbor_vid()) {
                            continue;
                        }
                        return Some(Ok(e));
                    }
                }
            },
        }
    }
}

impl<V, E, Mvs, F1, F2, R1, R2, D> CsrGraphBase<V, E, Mvs, F1, F2, R1, R2, D>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    D: DeletedVertexState<V, Mvs> + DeletedVertexRead,
{
    pub fn out_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, D, F1, F2>, CsrGraphError> {
        self.ensure_vertex(vid)?;
        if self.vertex_deleted(vid) {
            return Ok(LogicalNeighborhoodIter::Empty);
        }
        let inner = self
            .forward
            .edges
            .try_neighborhood_iter(&self.forward.vertices, vid)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        Ok(LogicalNeighborhoodIter::Active {
            inner,
            deleted_vertices: self.deleted.scan_deleted(),
            _marker: PhantomData,
        })
    }

    pub fn in_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, D, R1, R2>, CsrGraphError> {
        self.ensure_vertex(vid)?;
        if self.vertex_deleted(vid) {
            return Ok(LogicalNeighborhoodIter::Empty);
        }
        let inner = self
            .reverse
            .edges
            .try_neighborhood_iter(&self.reverse.vertices, vid)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        Ok(LogicalNeighborhoodIter::Active {
            inner,
            deleted_vertices: self.deleted.scan_deleted(),
            _marker: PhantomData,
        })
    }
}

impl<V, E, Mvs, F1, F2, R1, R2> CsrGraphRowTombstone<V, E, Mvs, F1, F2, R1, R2>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
{
    pub fn vertex_count(&self) -> u64 {
        self.inner.vertex_count()
    }

    pub fn sync_pma_meta(&self) -> Result<(), CsrGraphError> {
        self.inner.sync_pma_meta()
    }

    pub fn sync_pma_meta_for_vertex_range(
        &self,
        left: usize,
        right: usize,
    ) -> Result<(), CsrGraphError> {
        self.inner.sync_pma_meta_for_vertex_range(left, right)
    }

    pub fn refresh_slab_occupied_tail_meta(&self) -> Result<(), CsrGraphError> {
        self.inner.refresh_slab_occupied_tail_meta()
    }

    pub fn insert_vertex(&self, row_template: V) -> Result<u64, CsrGraphError> {
        self.inner.insert_vertex(row_template)
    }

    pub fn insert_vertex_strict(&self, row_template: V) -> Result<u64, CsrGraphError> {
        self.inner.insert_vertex_strict(row_template)
    }

    #[doc(hidden)]
    pub fn append_empty_vertices_fast_for_fixture(
        &self,
        row_template: V,
        count: usize,
    ) -> Result<(), CsrGraphError> {
        self.inner
            .append_empty_vertices_fast_for_fixture(row_template, count)
    }

    pub fn insert_directed(&self, src: usize, dst: usize, edge: E) -> Result<(), CsrGraphError> {
        self.inner.insert_directed(src, dst, edge)
    }

    pub fn insert_undirected(&self, u: usize, v: usize, edge: E) -> Result<(), CsrGraphError>
    where
        E: CsrEdgeUndirected,
    {
        self.inner.insert_undirected(u, v, edge)
    }

    pub fn out_edges<'a>(
        &'a self,
        vid: usize,
    ) -> Result<NeighborhoodIter<'a, E, F1, F2>, CsrGraphError> {
        self.inner.out_edges(vid)
    }

    pub fn in_edges<'a>(
        &'a self,
        vid: usize,
    ) -> Result<NeighborhoodIter<'a, E, R1, R2>, CsrGraphError> {
        self.inner.in_edges(vid)
    }

    pub fn forward_dgap(&self) -> &DgapStores<V, E, Mvs, F1, F2> {
        self.inner.forward_dgap()
    }

    pub fn reverse_dgap(&self) -> &DgapStores<V, E, Mvs, R1, R2> {
        self.inner.reverse_dgap()
    }
}

impl<V, E, Mvs, F1, F2, R1, R2, Dv> CsrGraphDenseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    Dv: Memory,
{
    pub fn vertex_count(&self) -> u64 {
        self.inner.vertex_count()
    }

    pub fn sync_pma_meta(&self) -> Result<(), CsrGraphError> {
        self.inner.sync_pma_meta()
    }

    pub fn sync_pma_meta_for_vertex_range(
        &self,
        left: usize,
        right: usize,
    ) -> Result<(), CsrGraphError> {
        self.inner.sync_pma_meta_for_vertex_range(left, right)
    }

    pub fn refresh_slab_occupied_tail_meta(&self) -> Result<(), CsrGraphError> {
        self.inner.refresh_slab_occupied_tail_meta()
    }

    pub fn insert_vertex(&self, row_template: V) -> Result<u64, CsrGraphError> {
        self.inner.insert_vertex(row_template)
    }

    pub fn insert_vertex_strict(&self, row_template: V) -> Result<u64, CsrGraphError> {
        self.inner.insert_vertex_strict(row_template)
    }

    #[doc(hidden)]
    pub fn append_empty_vertices_fast_for_fixture(
        &self,
        row_template: V,
        count: usize,
    ) -> Result<(), CsrGraphError> {
        self.inner
            .append_empty_vertices_fast_for_fixture(row_template, count)
    }

    pub fn insert_directed(&self, src: usize, dst: usize, edge: E) -> Result<(), CsrGraphError> {
        self.inner.insert_directed(src, dst, edge)
    }

    pub fn insert_undirected(&self, u: usize, v: usize, edge: E) -> Result<(), CsrGraphError>
    where
        E: CsrEdgeUndirected,
    {
        self.inner.insert_undirected(u, v, edge)
    }

    pub fn out_edges<'a>(
        &'a self,
        vid: usize,
    ) -> Result<NeighborhoodIter<'a, E, F1, F2>, CsrGraphError> {
        self.inner.out_edges(vid)
    }

    pub fn in_edges<'a>(
        &'a self,
        vid: usize,
    ) -> Result<NeighborhoodIter<'a, E, R1, R2>, CsrGraphError> {
        self.inner.in_edges(vid)
    }

    pub fn forward_dgap(&self) -> &DgapStores<V, E, Mvs, F1, F2> {
        self.inner.forward_dgap()
    }

    pub fn reverse_dgap(&self) -> &DgapStores<V, E, Mvs, R1, R2> {
        self.inner.reverse_dgap()
    }
}

impl<V, E, Mvs, F1, F2, R1, R2, Dv> CsrGraphSparseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    Dv: Memory,
{
    pub fn vertex_count(&self) -> u64 {
        self.inner.vertex_count()
    }

    pub fn sync_pma_meta(&self) -> Result<(), CsrGraphError> {
        self.inner.sync_pma_meta()
    }

    pub fn sync_pma_meta_for_vertex_range(
        &self,
        left: usize,
        right: usize,
    ) -> Result<(), CsrGraphError> {
        self.inner.sync_pma_meta_for_vertex_range(left, right)
    }

    pub fn refresh_slab_occupied_tail_meta(&self) -> Result<(), CsrGraphError> {
        self.inner.refresh_slab_occupied_tail_meta()
    }

    pub fn insert_vertex(&self, row_template: V) -> Result<u64, CsrGraphError> {
        self.inner.insert_vertex(row_template)
    }

    pub fn insert_vertex_strict(&self, row_template: V) -> Result<u64, CsrGraphError> {
        self.inner.insert_vertex_strict(row_template)
    }

    #[doc(hidden)]
    pub fn append_empty_vertices_fast_for_fixture(
        &self,
        row_template: V,
        count: usize,
    ) -> Result<(), CsrGraphError> {
        self.inner
            .append_empty_vertices_fast_for_fixture(row_template, count)
    }

    pub fn insert_directed(&self, src: usize, dst: usize, edge: E) -> Result<(), CsrGraphError> {
        self.inner.insert_directed(src, dst, edge)
    }

    pub fn insert_undirected(&self, u: usize, v: usize, edge: E) -> Result<(), CsrGraphError>
    where
        E: CsrEdgeUndirected,
    {
        self.inner.insert_undirected(u, v, edge)
    }

    pub fn out_edges<'a>(
        &'a self,
        vid: usize,
    ) -> Result<NeighborhoodIter<'a, E, F1, F2>, CsrGraphError> {
        self.inner.out_edges(vid)
    }

    pub fn in_edges<'a>(
        &'a self,
        vid: usize,
    ) -> Result<NeighborhoodIter<'a, E, R1, R2>, CsrGraphError> {
        self.inner.in_edges(vid)
    }

    pub fn forward_dgap(&self) -> &DgapStores<V, E, Mvs, F1, F2> {
        self.inner.forward_dgap()
    }

    pub fn reverse_dgap(&self) -> &DgapStores<V, E, Mvs, R1, R2> {
        self.inner.reverse_dgap()
    }
}

impl<V, E, Mvs, F1, F2, R1, R2, Dv> CsrGraphDenseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    Dv: Memory,
{
    pub fn out_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, DenseDeletedIndex<Dv>, F1, F2>, CsrGraphError>
    {
        self.inner.out_edges_logical(vid)
    }

    pub fn in_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, DenseDeletedIndex<Dv>, R1, R2>, CsrGraphError>
    {
        self.inner.in_edges_logical(vid)
    }
}

impl<V, E, Mvs, F1, F2, R1, R2, Dv> CsrGraphSparseDeleted<V, E, Mvs, F1, F2, R1, R2, Dv>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
    Dv: Memory,
{
    pub fn out_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, SparseDeletedIndex<Dv>, F1, F2>, CsrGraphError>
    {
        self.inner.out_edges_logical(vid)
    }

    pub fn in_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, SparseDeletedIndex<Dv>, R1, R2>, CsrGraphError>
    {
        self.inner.in_edges_logical(vid)
    }
}

impl<V, E, M> CsrGraphRowTombstone<V, E, M, M, M, M, M>
where
    V: CsrVertex,
    E: CsrEdge,
    M: Memory,
{
    #[allow(clippy::too_many_arguments)]
    pub fn format_new(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edge_counts: M,
        forward_edges_and_log: M,
        reverse_segment_edge_counts: M,
        reverse_edges_and_log: M,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
    ) -> Result<Self, CsrGraphError> {
        let (forward, reverse) = format_stores(
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
        Ok(Self {
            inner: CsrGraphBase::new(forward, reverse, RowTombstoneDeleted),
        })
    }

    pub fn open_existing(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edge_counts: M,
        forward_edges_and_log: M,
        reverse_segment_edge_counts: M,
        reverse_edges_and_log: M,
    ) -> Result<Self, CsrGraphError> {
        let (forward, reverse) = open_stores(
            mem_vertices_forward,
            mem_vertices_reverse,
            forward_segment_edge_counts,
            forward_edges_and_log,
            reverse_segment_edge_counts,
            reverse_edges_and_log,
        )?;
        Ok(Self {
            inner: CsrGraphBase::new(forward, reverse, RowTombstoneDeleted),
        })
    }
}

impl<V, E, M, Dv> CsrGraphDenseDeleted<V, E, M, M, M, M, M, Dv>
where
    V: CsrVertex,
    E: CsrEdge,
    M: Memory,
    Dv: Memory,
{
    #[allow(clippy::too_many_arguments)]
    pub fn format_new(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edge_counts: M,
        forward_edges_and_log: M,
        reverse_segment_edge_counts: M,
        reverse_edges_and_log: M,
        mem_deleted_vertices: Dv,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
    ) -> Result<Self, CsrGraphError> {
        let (forward, reverse) = format_stores(
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
        let deleted = DenseDeletedIndex {
            deleted_vertices: BitSet::new(mem_deleted_vertices).map_err(bitset_grow_failed)?,
        };
        Ok(Self {
            inner: CsrGraphBase::new(forward, reverse, deleted),
        })
    }

    pub fn open_existing(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edge_counts: M,
        forward_edges_and_log: M,
        reverse_segment_edge_counts: M,
        reverse_edges_and_log: M,
        mem_deleted_vertices: Dv,
    ) -> Result<Self, CsrGraphError> {
        let (forward, reverse) = open_stores(
            mem_vertices_forward,
            mem_vertices_reverse,
            forward_segment_edge_counts,
            forward_edges_and_log,
            reverse_segment_edge_counts,
            reverse_edges_and_log,
        )?;
        let deleted = DenseDeletedIndex {
            deleted_vertices: BitSet::init(mem_deleted_vertices)
                .map_err(|_| CsrGraphError::LogicalMutation("open_existing: dense deleted init failed"))?,
        };
        Ok(Self {
            inner: CsrGraphBase::new(forward, reverse, deleted),
        })
    }
}

impl<V, E, M, Dv> CsrGraphSparseDeleted<V, E, M, M, M, M, M, Dv>
where
    V: CsrVertex,
    E: CsrEdge,
    M: Memory,
    Dv: Memory,
{
    #[allow(clippy::too_many_arguments)]
    pub fn format_new(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edge_counts: M,
        forward_edges_and_log: M,
        reverse_segment_edge_counts: M,
        reverse_edges_and_log: M,
        mem_deleted_vertices: Dv,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
    ) -> Result<Self, CsrGraphError> {
        let (forward, reverse) = format_stores(
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
        let deleted = SparseDeletedIndex {
            deleted_vertices: StableRoaringBitMap::new(mem_deleted_vertices)
                .map_err(roaring_grow_failed)?,
        };
        Ok(Self {
            inner: CsrGraphBase::new(forward, reverse, deleted),
        })
    }

    pub fn open_existing(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edge_counts: M,
        forward_edges_and_log: M,
        reverse_segment_edge_counts: M,
        reverse_edges_and_log: M,
        mem_deleted_vertices: Dv,
    ) -> Result<Self, CsrGraphError> {
        let (forward, reverse) = open_stores(
            mem_vertices_forward,
            mem_vertices_reverse,
            forward_segment_edge_counts,
            forward_edges_and_log,
            reverse_segment_edge_counts,
            reverse_edges_and_log,
        )?;
        let deleted = SparseDeletedIndex {
            deleted_vertices: StableRoaringBitMap::init(mem_deleted_vertices).map_err(|_| {
                CsrGraphError::LogicalMutation("open_existing: sparse deleted init failed")
            })?,
        };
        Ok(Self {
            inner: CsrGraphBase::new(forward, reverse, deleted),
        })
    }
}
