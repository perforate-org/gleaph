//! Bidirectional CSR: forward out-adjacency + reverse (transpose) in-adjacency.

use std::fmt;
use std::marker::PhantomData;

use crate::csr::{DgapStores, DgapStoresError};
use crate::dgap::{DgapEdgeStore, DgapGraphMemories, NeighborhoodIter};
use crate::memory_util::GrowFailed;
use crate::traits::{CsrEdge, CsrEdgeTombstone, CsrEdgeUndirected, CsrVertex, CsrVertexTombstone};
use ic_stable_slot_map::SlotMap;
use ic_stable_structures::Memory;

// --- specialization: detect undirected flag only when `E: CsrEdgeUndirected` ---

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

/// Failure from [`CsrGraph`] operations.
#[derive(Debug, PartialEq, Eq)]
pub enum CsrGraphError {
    Forward(DgapStoresError),
    Reverse(DgapStoresError),
    /// [`DgapEdgeStore::format_new`] or region grow failed while building the graph.
    Format(GrowFailed),
    VertexCountMismatch {
        forward: u64,
        reverse: u64,
    },
    VertexOutOfRange {
        vid: usize,
        len: u64,
    },
    /// [`CsrGraph::insert_directed`] requires `edge.neighbor_vid() == dst`.
    NeighborMismatch {
        expected: usize,
        actual: usize,
    },
    /// Use [`CsrGraph::insert_undirected`] when the edge is marked undirected.
    UndirectedEdgeInDirectedInsert,
    /// Mutation refused because this vertex row is tombstoned.
    EndpointTombstone {
        vid: usize,
    },
    /// Insert refused: an adjacency slot to that neighbor already exists (including tombstones).
    AdjacencySlotOccupied {
        src: usize,
        dst: usize,
    },
    /// Logical delete could not find the requested neighbor edge.
    EdgeNotFound {
        owner: usize,
        neighbor: usize,
    },
    /// Stable deque backing the GC work queue failed to grow.
    GcQueue(GrowFailed),
    /// Logical delete / GC helper failed inside the edge region.
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
            Self::EndpointTombstone { vid } => {
                write!(f, "vertex {vid} is tombstoned")
            }
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

/// Directed CSR plus transpose CSR, kept in sync on mutation.
///
/// Use [`Self::format_new`] to construct from six [`Memory`](ic_stable_structures::Memory) regions
/// without assembling [`DgapStores`] manually.
///
/// **Iterator limit:** [`Self::out_edges`] / [`Self::in_edges`] require `E::EDGE_BYTES <= 64`
/// (same as [`DgapEdgeStore::try_neighborhood_iter`](crate::dgap::DgapEdgeStore::try_neighborhood_iter)).
pub struct CsrGraph<V, E, Mvs, F1, F2, R1, R2>
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
}

impl<V, E, Mvs, F1, F2, R1, R2> CsrGraph<V, E, Mvs, F1, F2, R1, R2>
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

    /// Refresh persisted slab tail metadata on both DGAP columns (see [`DgapStores::refresh_slab_occupied_tail_meta`]).
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

    /// Raw neighborhood scan (includes tombstones). Used for insert-uniqueness checks.
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
            let e = x.map_err(|m| CsrGraphError::LogicalMutation(m))?;
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

impl<V, E, M> CsrGraph<V, E, M, M, M, M, M>
where
    V: CsrVertex,
    E: CsrEdge,
    M: Memory,
{
    /// Format empty vertex columns and both edge regions from **six** memories (see crate root diagram).
    ///
    /// Order: forward `M_v`, reverse `M_v`, then forward `segment_edge_counts`, `edges_and_log`,
    /// then the same pair for reverse. All edge regions receive the same
    /// `elem_capacity` / `segment_count` / `segment_size` / `num_edges` via [`DgapEdgeStore::format_new`].
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

        let forward = DgapStores::new(vertices_forward, forward_edges);
        let reverse = DgapStores::new(vertices_reverse, reverse_edges);

        Ok(Self { forward, reverse })
    }
}

/// Neighborhood iterator hiding tombstone edges and edges incident to tombstoned vertices.
pub enum LogicalNeighborhoodIter<'a, E, V, Mvs, M1, M2>
where
    E: CsrEdge + CsrEdgeTombstone,
    V: CsrVertex + CsrVertexTombstone,
    Mvs: Memory,
    M1: Memory,
    M2: Memory,
{
    Active {
        inner: NeighborhoodIter<'a, E, M1, M2>,
        verts: &'a SlotMap<V, Mvs>,
        _p: PhantomData<(V, E)>,
    },
    Empty,
}

impl<'a, E, V, Mvs, M1, M2> Iterator for LogicalNeighborhoodIter<'a, E, V, Mvs, M1, M2>
where
    E: CsrEdge + CsrEdgeTombstone,
    V: CsrVertex + CsrVertexTombstone,
    Mvs: Memory,
    M1: Memory,
    M2: Memory,
{
    type Item = Result<E, &'static str>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            LogicalNeighborhoodIter::Empty => None,
            LogicalNeighborhoodIter::Active { inner, verts, .. } => loop {
                let x = inner.next()?;
                match x {
                    Err(e) => return Some(Err(e)),
                    Ok(e) => {
                        if e.is_tombstone() {
                            continue;
                        }
                        let nb = e.neighbor_vid();
                        let dead = verts
                            .get_dense(nb as u32)
                            .map(|v| v.is_tombstone())
                            .unwrap_or(true);
                        if dead {
                            continue;
                        }
                        return Some(Ok(e));
                    }
                }
            },
        }
    }
}

impl<V, E, Mvs, F1, F2, R1, R2> CsrGraph<V, E, Mvs, F1, F2, R1, R2>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Mvs: Memory,
    F1: Memory,
    F2: Memory,
    R1: Memory,
    R2: Memory,
{
    /// Out-neighbors omitting edge tombstones and neighbors whose vertex row is tombstoned.
    pub fn out_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, V, Mvs, F1, F2>, CsrGraphError> {
        self.ensure_vertex(vid)?;
        if self
            .forward
            .vertices
            .get_dense(vid as u32)
            .map(|v| v.is_tombstone())
            == Some(true)
        {
            return Ok(LogicalNeighborhoodIter::Empty);
        }
        let inner = self
            .forward
            .edges
            .try_neighborhood_iter(&self.forward.vertices, vid)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))?;
        Ok(LogicalNeighborhoodIter::Active {
            inner,
            verts: &self.forward.vertices,
            _p: PhantomData,
        })
    }

    /// In-neighbors (transpose) with the same filtering as [`Self::out_edges_logical`].
    pub fn in_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, V, Mvs, R1, R2>, CsrGraphError> {
        self.ensure_vertex(vid)?;
        if self
            .reverse
            .vertices
            .get_dense(vid as u32)
            .map(|v| v.is_tombstone())
            == Some(true)
        {
            return Ok(LogicalNeighborhoodIter::Empty);
        }
        let inner = self
            .reverse
            .edges
            .try_neighborhood_iter(&self.reverse.vertices, vid)
            .map_err(|m| CsrGraphError::Reverse(DgapStoresError::Graph(m)))?;
        Ok(LogicalNeighborhoodIter::Active {
            inner,
            verts: &self.forward.vertices,
            _p: PhantomData,
        })
    }
}
