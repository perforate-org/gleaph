//! CSR orchestration across `M_v` and three-`Memory` `M_e` ([`DgapGraphMemories`]).
//!
//! ```text
//! [`DgapStores`]
//!   ├─ vertices: one `Memory` — usually [`ic_stable_structures::vec::Vec`] V1 on `M_v`
//!   └─ edges:    [`DgapGraphMemories`] — three `Memory` values for DGAP `M_e` (see crate root diagram)
//! ```

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
use crate::dgap::{DgapGraphMemories, DgapEdgeStore};

/// Failure from [`DgapStores`] graph mutation (CSR vertex column + DGAP PMA / edge region).
#[derive(Debug, PartialEq, Eq)]
pub enum DgapStoresError {
    Graph(&'static str),
}

impl fmt::Display for DgapStoresError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for DgapStoresError {}

/// Two-way split: vertex column (`M_v`) and DGAP edge bundle (`M_e` = three [`Memory`] regions).
///
/// `Vs` is typically [`ic_stable_structures::vec::Vec`] or [`ic_stable_vec_deque::VecDeque`].
///
/// **Vertex capacity:** at most [`DgapEdgeStore::max_vertex_slots`] rows for the edge header’s
/// `segment_count` and `segment_size` (`dgap_leaf_segment_id` must stay in range). Size
/// [`DgapEdgeStore::format_new`](crate::dgap::DgapEdgeStore::format_new) accordingly.
pub struct DgapStores<V, E, Vs, M1, M2, M3>
where
    V: CsrVertex,
    E: CsrEdgeSlot,
    Vs: CsrVertexColumn<V>,
    M1: Memory,
    M2: Memory,
    M3: Memory,
{
    pub vertices: Vs,
    pub edges: DgapEdgeStore<E, M1, M2, M3>,
    _vertex: PhantomData<V>,
}

impl<V, E, Vs, M1, M2, M3> DgapStores<V, E, Vs, M1, M2, M3>
where
    V: CsrVertex,
    E: CsrEdgeSlot,
    Vs: CsrVertexColumn<V>,
    M1: Memory,
    M2: Memory,
    M3: Memory,
{
    pub fn new(vertices: Vs, edges: DgapEdgeStore<E, M1, M2, M3>) -> Self {
        Self {
            vertices,
            edges,
            _vertex: PhantomData,
        }
    }

    /// Borrow the underlying [`DgapGraphMemories`] (e.g. for canister wiring).
    pub fn edge_memories(&self) -> &DgapGraphMemories<M1, M2, M3> {
        self.edges.memories()
    }

    /// Sync PMA `total` and `actual` from the vertex column (no `&[V]` snapshot).
    pub fn sync_pma_meta(&self) -> Result<(), &'static str> {
        self.edges.sync_pma_totals(&self.vertices)?;
        self.edges.sync_pma_actuals(&self.vertices)?;
        Ok(())
    }

    /// Insert one edge for `vid` (CSR slab or DGAP segment log) and run PMA maintenance.
    pub fn insert_edge(&self, vid: usize, edge: E) -> Result<(), DgapStoresError> {
        self.edges
            .insert_edge_and_maintain(&self.vertices, vid, edge)
            .map_err(DgapStoresError::Graph)
    }

    /// Append one vertex row at the end of `M_v`.
    ///
    /// `row.base_slot_start()` must equal [`DgapEdgeStore::slab_append_base_slot`] on [`Self::edges`]
    /// and the column **before** the push (see also [`DgapEdgeStore::slab_occupied_tail`]). If that tail is not
    /// below `elem_capacity`, [`DgapEdgeStore::resize_double`] is run until there is room.
    ///
    /// Returns the new vertex id (`vid`) equal to the previous [`CsrVertexColumn::col_len`].
    pub fn insert_vertex(&self, row: V) -> Result<u64, DgapStoresError> {
        let h = self
            .edges
            .header()
            .ok_or(DgapStoresError::Graph("bad edge header"))?;
        let new_vid = self.vertices.col_len();
        DgapEdgeStore::<E, M1, M2, M3>::check_vertex_append_cap(
            new_vid,
            h.segment_count,
            h.segment_size,
        )
        .map_err(DgapStoresError::Graph)?;

        let expected_base = self
            .edges
            .slab_append_base_slot(&self.vertices)
            .map_err(DgapStoresError::Graph)?;
        if row.base_slot_start() != expected_base {
            return Err(DgapStoresError::Graph(
                "insert_vertex base_slot_start mismatch (use DgapEdgeStore::slab_append_base_slot)",
            ));
        }

        loop {
            let h = self
                .edges
                .header()
                .ok_or(DgapStoresError::Graph("bad edge header"))?;
            if expected_base < h.elem_capacity {
                break;
            }
            self.edges
                .resize_double(&self.vertices)
                .map_err(DgapStoresError::Graph)?;
        }

        self.vertices
            .col_push_back(&row)
            .map_err(|_| DgapStoresError::Graph("vertex column grow failed"))?;

        self.sync_pma_meta().map_err(DgapStoresError::Graph)?;

        Ok(new_vid)
    }
}
