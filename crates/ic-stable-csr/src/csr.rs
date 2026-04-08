//! CSR orchestration across `M_v` and two-`Memory` `M_e` ([`DgapGraphMemories`]).
//!
//! ```text
//! [`DgapStores`]
//!   ├─ vertices: [`ic_stable_slot_map::SlotMap`] V1 on `M_v` (`SSM` magic)
//!   └─ edges:    [`DgapGraphMemories`] — two `Memory` values for DGAP `M_e` (see crate root diagram)
//! ```

use std::fmt;
use std::marker::PhantomData;

use ic_stable_slot_map::SlotMap;
use ic_stable_structures::Memory;

pub mod csr_graph;
pub mod csr_graph_gc;
pub mod gc_work_item;
pub mod insert;

pub use csr_graph::{CsrGraph, CsrGraphError, LogicalNeighborhoodIter};
pub use csr_graph_gc::CsrGraphWithGcQueue;
pub use gc_work_item::{GC_TAG_SEGMENT_FWD, GC_TAG_SEGMENT_REV, GC_TAG_VERTEX, GcWorkItem};
pub use crate::dgap::{SegmentMaintainAction, SegmentMaintainThresholds};
pub use insert::{CsrInsertError, insert_edge_into_slab, insert_edge_into_slab_column};

use crate::dgap::{DgapEdgeStore, DgapGraphMemories};
use crate::traits::{CsrEdge, CsrVertex};

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

/// Two-way split: vertex table (`M_v`) and DGAP edge bundle (`M_e` = two [`Memory`] regions).
///
/// Vertices live in [`SlotMap`] (`SSM`); use append-only [`SlotMap::insert`] via [`Self::insert_vertex`].
///
/// **Vertex capacity:** at most [`DgapEdgeStore::max_vertex_slots`] rows for the edge header’s
/// `segment_count` and `segment_size` (`dgap_leaf_segment_id` must stay in range). Size
/// [`DgapEdgeStore::format_new`](crate::dgap::DgapEdgeStore::format_new) accordingly.
pub struct DgapStores<V, E, Mvs, M1, M2>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    M1: Memory,
    M2: Memory,
{
    pub vertices: SlotMap<V, Mvs>,
    pub edges: DgapEdgeStore<E, M1, M2>,
    _vertex: PhantomData<V>,
}

impl<V, E, Mvs, M1, M2> DgapStores<V, E, Mvs, M1, M2>
where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    M1: Memory,
    M2: Memory,
{
    #[doc(hidden)]
    pub fn new(vertices: SlotMap<V, Mvs>, edges: DgapEdgeStore<E, M1, M2>) -> Self {
        Self {
            vertices,
            edges,
            _vertex: PhantomData,
        }
    }

    /// Borrow the underlying [`DgapGraphMemories`] (e.g. for canister wiring).
    pub fn edge_memories(&self) -> &DgapGraphMemories<M1, M2> {
        self.edges.memories()
    }

    /// Sync PMA segment edge counts from the vertex column (no `&[V]` snapshot).
    pub fn sync_pma_meta(&self) -> Result<(), &'static str> {
        self.edges.sync_pma_edge_counts(&self.vertices)
    }

    /// Recompute SEC only for DGAP segments touched by vertices `[left, right)` (`right` exclusive).
    pub fn sync_pma_meta_for_vertex_range(
        &self,
        left: usize,
        right: usize,
    ) -> Result<(), &'static str> {
        self.edges
            .sync_pma_edge_counts_for_vertex_range(&self.vertices, left, right)
    }

    /// Recompute and persist [`crate::layout::dgap::DgapEdgeHeaderV1::slab_occupied_tail`] from `vertices`.
    pub fn refresh_slab_occupied_tail_meta(&self) -> Result<(), &'static str> {
        self.edges
            .refresh_slab_occupied_tail_from_column(&self.vertices)
    }

    /// Insert one edge for `vid` (CSR slab or DGAP segment log) and run PMA maintenance.
    pub fn insert_edge(&self, vid: usize, edge: E) -> Result<(), DgapStoresError> {
        self.edges
            .insert_edge_and_maintain(&self.vertices, vid, edge)
            .map_err(DgapStoresError::Graph)
    }

    /// Insert many edges in iterator order. Consecutive same-`vid` runs use the batched edge-store path;
    /// see [`DgapEdgeStore::insert_edges_and_maintain`].
    pub fn insert_edges<I>(&self, edges: I) -> Result<(), DgapStoresError>
    where
        I: IntoIterator<Item = (usize, E)>,
    {
        self.edges
            .insert_edges_and_maintain(&self.vertices, edges)
            .map_err(DgapStoresError::Graph)
    }

    /// Append one vertex row at the end of `M_v`.
    ///
    /// Sets `row.base_slot_start()` to [`DgapEdgeStore::slab_append_base_slot`] on [`Self::edges`]
    /// for the column **before** the push (other fields are taken from `row`). If that tail is not
    /// below `elem_capacity`, [`DgapEdgeStore::resize_double`] is run until there is room.
    ///
    /// Returns the new vertex id (`vid`) equal to the previous [`SlotMap::len`](ic_stable_slot_map::SlotMap::len).
    ///
    /// To require a caller-supplied base that already matches the append cursor, use
    /// [`Self::insert_vertex_strict`].
    pub fn insert_vertex(&self, row: V) -> Result<u64, DgapStoresError> {
        let h = self
            .edges
            .header()
            .ok_or(DgapStoresError::Graph("bad edge header"))?;
        let new_vid = self.vertices.len();
        DgapEdgeStore::<E, M1, M2>::check_vertex_append_cap(
            new_vid,
            h.segment_count,
            h.segment_size,
        )
        .map_err(DgapStoresError::Graph)?;

        let expected_base = self
            .edges
            .slab_append_base_slot(&self.vertices)
            .map_err(DgapStoresError::Graph)?;
        let row = row.with_base_slot_start(expected_base);
        self.append_vertex_row_after_base_checks(new_vid, row, expected_base)
    }

    /// Like [`Self::insert_vertex`], but **`row.base_slot_start()` must already equal**
    /// [`DgapEdgeStore::slab_append_base_slot`] before the push. Use this when you want an explicit
    /// error instead of silently coercing the base field.
    pub fn insert_vertex_strict(&self, row: V) -> Result<u64, DgapStoresError> {
        let h = self
            .edges
            .header()
            .ok_or(DgapStoresError::Graph("bad edge header"))?;
        let new_vid = self.vertices.len();
        DgapEdgeStore::<E, M1, M2>::check_vertex_append_cap(
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
                "insert_vertex_strict: base_slot_start mismatch (expected slab_append_base_slot)",
            ));
        }

        self.append_vertex_row_after_base_checks(new_vid, row, expected_base)
    }

    fn append_vertex_row_after_base_checks(
        &self,
        new_vid: u64,
        row: V,
        expected_base: u64,
    ) -> Result<u64, DgapStoresError> {
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
            .insert(&row)
            .map_err(|_| DgapStoresError::Graph("vertex column grow failed"))?;

        self.edges
            .refresh_slab_occupied_tail_from_column(&self.vertices)
            .map_err(DgapStoresError::Graph)?;

        let idx = new_vid as usize;
        self.sync_pma_meta_for_vertex_range(idx, idx.saturating_add(1))
            .map_err(DgapStoresError::Graph)?;

        Ok(new_vid)
    }
}
