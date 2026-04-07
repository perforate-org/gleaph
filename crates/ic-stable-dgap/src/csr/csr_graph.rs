//! Bidirectional CSR: forward out-adjacency + reverse (transpose) in-adjacency.

use std::fmt;
use std::marker::PhantomData;

use ic_stable_structures::Memory;
use ic_stable_structures::vec::Vec as StableVec;

use crate::csr::vertex_column::CsrVertexColumn;
use crate::csr::{DgapStores, DgapStoresError};
use crate::dgap::{DgapEdgeStore, DgapGraphMemories, NeighborhoodIter};
use crate::memory_util::GrowFailed;
use crate::traits::{CsrEdge, CsrEdgeTombstone, CsrEdgeUndirected, CsrVertex, CsrVertexTombstone};

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
    VertexCountMismatch { forward: u64, reverse: u64 },
    VertexOutOfRange { vid: usize, len: u64 },
    /// [`CsrGraph::insert_directed`] requires `edge.neighbor_vid() == dst`.
    NeighborMismatch { expected: usize, actual: usize },
    /// Use [`CsrGraph::insert_undirected`] when the edge is marked undirected.
    UndirectedEdgeInDirectedInsert,
    /// Mutation refused because this vertex row is tombstoned.
    EndpointTombstone { vid: usize },
    /// Insert refused: an adjacency slot to that neighbor already exists (including tombstones).
    AdjacencySlotOccupied { src: usize, dst: usize },
    /// Logical delete could not find the requested neighbor edge.
    EdgeNotFound { owner: usize, neighbor: usize },
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
            Self::EdgeNotFound { owner, neighbor } => write!(
                f,
                "no edge from owner {owner} to neighbor {neighbor}"
            ),
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

/// Directed CSR plus transpose CSR, kept in sync on mutation.
///
/// Use [`Self::format_new`] to construct from eight [`Memory`](ic_stable_structures::Memory) regions
/// without assembling [`DgapStores`] manually.
///
/// **Iterator limit:** [`Self::out_edges`] / [`Self::in_edges`] require `E::EDGE_BYTES <= 64`
/// (same as [`DgapEdgeStore::try_neighborhood_iter`](crate::dgap::DgapEdgeStore::try_neighborhood_iter)).
pub struct CsrGraph<V, E, Vs, F1, F2, F3, R1, R2, R3>
where
    V: CsrVertex,
    E: CsrEdge,
    Vs: CsrVertexColumn<V>,
    F1: Memory,
    F2: Memory,
    F3: Memory,
    R1: Memory,
    R2: Memory,
    R3: Memory,
{
    forward: DgapStores<V, E, Vs, F1, F2, F3>,
    reverse: DgapStores<V, E, Vs, R1, R2, R3>,
}

impl<V, E, Vs, F1, F2, F3, R1, R2, R3> CsrGraph<V, E, Vs, F1, F2, F3, R1, R2, R3>
where
    V: CsrVertex,
    E: CsrEdge,
    Vs: CsrVertexColumn<V>,
    F1: Memory,
    F2: Memory,
    F3: Memory,
    R1: Memory,
    R2: Memory,
    R3: Memory,
{
    /// Build from pre-constructed stores (tests / advanced use).
    #[doc(hidden)]
    pub fn from_stores(
        forward: DgapStores<V, E, Vs, F1, F2, F3>,
        reverse: DgapStores<V, E, Vs, R1, R2, R3>,
    ) -> Result<Self, CsrGraphError> {
        let lf = forward.vertices.col_len();
        let lr = reverse.vertices.col_len();
        if lf != lr {
            return Err(CsrGraphError::VertexCountMismatch {
                forward: lf,
                reverse: lr,
            });
        }
        Ok(Self { forward, reverse })
    }

    pub fn vertex_count(&self) -> u64 {
        self.forward.vertices.col_len()
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

    pub fn insert_vertex(&self, row_template: V) -> Result<u64, CsrGraphError> {
        let id = self
            .forward
            .insert_vertex(row_template)
            .map_err(CsrGraphError::Forward)?;
        self.reverse
            .insert_vertex(row_template)
            .map_err(CsrGraphError::Reverse)?;
        debug_assert_eq!(self.forward.vertices.col_len(), self.reverse.vertices.col_len());
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
        debug_assert_eq!(self.forward.vertices.col_len(), self.reverse.vertices.col_len());
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
    ) -> Result<NeighborhoodIter<'a, E, F1, F2, F3>, CsrGraphError> {
        self.ensure_vertex(vid)?;
        self.forward
            .edges
            .try_neighborhood_iter(&self.forward.vertices, vid)
            .map_err(|m| CsrGraphError::Forward(DgapStoresError::Graph(m)))
    }

    pub fn in_edges<'a>(
        &'a self,
        vid: usize,
    ) -> Result<NeighborhoodIter<'a, E, R1, R2, R3>, CsrGraphError> {
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

    pub fn forward_dgap(&self) -> &DgapStores<V, E, Vs, F1, F2, F3> {
        &self.forward
    }

    pub fn reverse_dgap(&self) -> &DgapStores<V, E, Vs, R1, R2, R3> {
        &self.reverse
    }
}

impl<V, E, M> CsrGraph<V, E, StableVec<V, M>, M, M, M, M, M, M>
where
    V: CsrVertex,
    E: CsrEdge,
    M: Memory,
{
    /// Format empty vertex columns and both edge regions from **eight** memories (see crate root diagram).
    ///
    /// Order: forward `M_v`, reverse `M_v`, then forward `segment_edges_actual`, `segment_edges_total`,
    /// `edges_and_log`, then the same three for reverse. All edge regions receive the same
    /// `elem_capacity` / `segment_count` / `segment_size` / `num_edges` via [`DgapEdgeStore::format_new`].
    pub fn format_new(
        mem_vertices_forward: M,
        mem_vertices_reverse: M,
        forward_segment_edges_actual: M,
        forward_segment_edges_total: M,
        forward_edges_and_log: M,
        reverse_segment_edges_actual: M,
        reverse_segment_edges_total: M,
        reverse_edges_and_log: M,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        num_edges: u64,
    ) -> Result<Self, CsrGraphError> {
        let vertices_forward = StableVec::new(mem_vertices_forward);
        let vertices_reverse = StableVec::new(mem_vertices_reverse);

        let fwd_mem = DgapGraphMemories::new(
            forward_segment_edges_actual,
            forward_segment_edges_total,
            forward_edges_and_log,
        );
        let rev_mem = DgapGraphMemories::new(
            reverse_segment_edges_actual,
            reverse_segment_edges_total,
            reverse_edges_and_log,
        );

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
pub enum LogicalNeighborhoodIter<'a, E, Vs, V, M1, M2, M3>
where
    E: CsrEdge + CsrEdgeTombstone,
    V: CsrVertex + CsrVertexTombstone,
    Vs: CsrVertexColumn<V>,
    M1: Memory,
    M2: Memory,
    M3: Memory,
{
    Active {
        inner: NeighborhoodIter<'a, E, M1, M2, M3>,
        verts: &'a Vs,
        _p: PhantomData<(V, E)>,
    },
    Empty,
}

impl<'a, E, Vs, V, M1, M2, M3> Iterator for LogicalNeighborhoodIter<'a, E, Vs, V, M1, M2, M3>
where
    E: CsrEdge + CsrEdgeTombstone,
    V: CsrVertex + CsrVertexTombstone,
    Vs: CsrVertexColumn<V>,
    M1: Memory,
    M2: Memory,
    M3: Memory,
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
                            .col_get(nb as u64)
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

impl<V, E, Vs, F1, F2, F3, R1, R2, R3> CsrGraph<V, E, Vs, F1, F2, F3, R1, R2, R3>
where
    V: CsrVertex + CsrVertexTombstone,
    E: CsrEdge + CsrEdgeTombstone,
    Vs: CsrVertexColumn<V>,
    F1: Memory,
    F2: Memory,
    F3: Memory,
    R1: Memory,
    R2: Memory,
    R3: Memory,
{
    /// Out-neighbors omitting edge tombstones and neighbors whose vertex row is tombstoned.
    pub fn out_edges_logical<'a>(
        &'a self,
        vid: usize,
    ) -> Result<LogicalNeighborhoodIter<'a, E, Vs, V, F1, F2, F3>, CsrGraphError> {
        self.ensure_vertex(vid)?;
        if self
            .forward
            .vertices
            .col_get(vid as u64)
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
    ) -> Result<LogicalNeighborhoodIter<'a, E, Vs, V, R1, R2, R3>, CsrGraphError> {
        self.ensure_vertex(vid)?;
        if self
            .reverse
            .vertices
            .col_get(vid as u64)
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
