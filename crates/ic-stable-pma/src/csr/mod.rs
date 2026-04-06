//! CSR orchestration across `M_v` and `M_e` (DGAP overflow logs live inside `M_e`).

use std::fmt;
use std::marker::PhantomData;

pub mod insert;
pub mod vertex_column;

pub use insert::{
    insert_edge_into_slab, insert_edge_into_slab_column, CsrInsertError,
};
pub use vertex_column::CsrVertexColumn;

use ic_stable_structures::Memory;

use crate::traits::{CsrEdgeSlot, CsrVertex};
use crate::vcsr::VcsrEdgeStore;

/// Failure from [`VcsrStores`] graph mutation (CSR / PMA).
#[derive(Debug, PartialEq, Eq)]
pub enum VcsrStoresError {
    Graph(&'static str),
}

impl fmt::Display for VcsrStoresError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for VcsrStoresError {}

/// Two-way split: vertex column (`M_v`) and DGAP edge region (`M_e` includes per-segment overflow logs).
///
/// `Vs` is typically [`ic_stable_structures::vec::Vec`] or [`ic_stable_vec_deque::VecDeque`].
pub struct VcsrStores<V, E, Vs, Me>
where
    V: CsrVertex,
    E: CsrEdgeSlot,
    Vs: CsrVertexColumn<V>,
    Me: Memory,
{
    pub vertices: Vs,
    pub edges: VcsrEdgeStore<E, Me>,
    _vertex: PhantomData<V>,
}

impl<V, E, Vs, Me> VcsrStores<V, E, Vs, Me>
where
    V: CsrVertex,
    E: CsrEdgeSlot,
    Vs: CsrVertexColumn<V>,
    Me: Memory,
{
    pub fn new(vertices: Vs, edges: VcsrEdgeStore<E, Me>) -> Self {
        Self {
            vertices,
            edges,
            _vertex: PhantomData,
        }
    }

    /// Sync PMA `total` and `actual` from the vertex column (no `&[V]` snapshot).
    pub fn sync_pma_meta(&self) -> Result<(), &'static str> {
        self.edges.sync_pma_totals(&self.vertices)?;
        self.edges.sync_pma_actuals(&self.vertices)?;
        Ok(())
    }

    /// Insert one edge for `vid` (CSR slab or DGAP segment log) and run PMA maintenance.
    pub fn insert_edge(&self, vid: usize, edge: E) -> Result<(), VcsrStoresError>
    where
        E: Clone + Copy,
        V: Copy,
    {
        self.edges
            .insert_edge_and_maintain(&self.vertices, vid, edge)
            .map_err(VcsrStoresError::Graph)
    }
}
