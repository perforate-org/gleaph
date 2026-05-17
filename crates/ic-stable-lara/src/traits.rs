//! Minimal traits for LARA CSR edges and vertices without Gleaph-specific types.
//!
//! `graph-store` can implement [`CsrVertex`] / [`CsrEdge`] for `VertexEntry` / `EdgeEntry` in a follow-up
//! (keeps this crate free of `gleaph_graph_kernel`).

use ic_stable_structures::Storable;

use crate::VertexId;

/// One vertex row in the CSR vertex column (`M_v`).
///
/// `log_head` is the LARA per-segment log array index of the head of this vertex's overflow chain,
/// or `-1` if all neighbors live on the CSR slab.
///
/// [`Self::degree`] is the **logical** neighborhood width (what callers should treat as
/// out-degree). [`Self::stored_degree`] is the backing width used for slab / overflow layout
/// and may be larger while deferred deletes have not been folded.
///
/// Owned slab spans for inserts and relocation follow CSR geometry:
/// adjacent [`Self::base_slot_start`] values and PMA leaf totals (plus `elem_capacity`).
pub trait CsrVertex: Storable + Copy {
    /// Fixed byte width of one encoded vertex row.
    const BYTES: usize;
    /// Global edge-slot index where this vertex's base neighborhood starts (flat slab model).
    fn base_slot_start(&self) -> u64;
    /// Logical out-degree (or label-bucket row count) visible through graph APIs.
    fn degree(&self) -> u32;
    /// Physical width backing this row in slab / log storage (never less than [`Self::degree`]).
    fn stored_degree(&self) -> u32 {
        self.degree()
    }
    /// Returns a copy with a new slab base slot.
    fn with_base_slot_start(self, start: u64) -> Self;
    /// Returns a copy after updating the **stored** neighborhood width used for layout.
    fn with_degree(self, degree: u32) -> Self;

    /// Updates this row after [`crate::lara::edge::EdgeStore::remove_edge_slab_placeholder_matching`]
    /// logically removed one on-slab edge by writing a vacant placeholder (no swap).
    fn after_slab_placeholder_delete(self) -> Self;

    /// Grows the packed slab row by one live edge (append path when no vacant reuse).
    fn grow_packed_slab_by_one(self) -> Self;

    /// After writing into the first vacant slab slot at `base + degree()` (see
    /// [`Self::after_slab_placeholder_delete`]), adjusts counters so [`Self::degree`] grows by one
    /// without growing [`Self::stored_degree`].
    ///
    /// Default: no-op (rows without deferred tail tombstones).
    #[inline]
    fn after_slab_insert_reuse_tail_tombstone(self) -> Self {
        self
    }

    /// Head index of this vertex's overflow log chain, or `-1` when absent.
    fn log_head(self) -> i32;
    /// Returns a copy with a new overflow log head.
    fn with_log_head(self, idx: i32) -> Self;
}

/// Optional marker support for vertex rows that can represent deleted vertices.
pub trait CsrVertexTombstone: CsrVertex {
    /// Returns `true` when the vertex row is a tombstone.
    fn is_tombstone(&self) -> bool;
    /// Returns a copy with the tombstone flag changed.
    fn with_tombstone(self, tomb: bool) -> Self;
}

/// Tombstone behavior used by generic graph code.
pub trait CsrVertexTombstoneScan: CsrVertex {
    /// Returns whether this vertex is logically deleted.
    fn record_is_vertex_tombstone(vertex: &Self) -> bool;
    /// Returns a copy with the logical deletion marker changed.
    fn record_with_vertex_tombstone(vertex: Self, tomb: bool) -> Self;
}

impl<V: CsrVertex> CsrVertexTombstoneScan for V {
    default fn record_is_vertex_tombstone(_: &Self) -> bool {
        false
    }

    default fn record_with_vertex_tombstone(vertex: Self, _: bool) -> Self {
        vertex
    }
}

impl<V: CsrVertex + CsrVertexTombstone> CsrVertexTombstoneScan for V {
    fn record_is_vertex_tombstone(vertex: &Self) -> bool {
        vertex.is_tombstone()
    }

    fn record_with_vertex_tombstone(vertex: Self, tomb: bool) -> Self {
        vertex.with_tombstone(tomb)
    }
}

/// One fixed-width **edge record** stored in a CSR slab cell (`M_e`).
///
/// In the forward CSR, [`Self::neighbor_vid`](Self::neighbor_vid) is the **other** endpoint (out-neighbor).
/// The CSR graph wrappers build the transpose CSR by storing [`Self::with_neighbor_vid`](Self::with_neighbor_vid)(`src`)
/// at row `dst` in the reverse store.
///
/// Hot paths use a **64-byte stack buffer** when `BYTES <= 64`; larger widths still work via heap.
pub trait CsrEdge: Copy {
    /// Fixed byte width of one encoded edge record.
    const BYTES: usize;
    /// Decodes an edge record from exactly [`Self::BYTES`] bytes.
    fn read_from(bytes: &[u8]) -> Self;
    /// Encodes this edge record into exactly [`Self::BYTES`] bytes.
    fn write_to(self, bytes: &mut [u8]);

    /// Adjacent vertex id for this orientation (out-neighbor in the forward CSR).
    fn neighbor_vid(&self) -> VertexId;
    /// Returns a copy with the adjacent vertex id changed.
    fn with_neighbor_vid(self, vid: VertexId) -> Self;
}

/// Edge records that support **logical slab deletion** without swap-remove:
/// [`VertexId::SLAB_VACANT`] marks a cell deleted until a leaf rebalance packs the row.
pub trait CsrEdgeSlabVacancy: CsrEdge {
    /// Encoded edge payload for a vacant slab slot (must satisfy [`Self::is_slab_vacant_edge`]).
    fn slab_vacant_edge() -> Self;
    /// Returns `true` when this slot holds a logical-delete placeholder.
    #[inline]
    fn is_slab_vacant_edge(&self) -> bool {
        self.neighbor_vid().is_slab_vacant_neighbor()
    }
}

/// Extension of [`CsrEdge`] for edges that carry an **undirected** semantic flag in the slot payload.
///
/// Storage remains directed CSR (forward + reverse in the CSR graph wrappers); this bit records that
/// the logical relationship is undirected so APIs can reject `insert_directed`
/// when inappropriate and route to `insert_undirected`.
///
/// Implementations should keep the flag consistent with any other packed metadata when rewriting the edge
/// (for example the undirected bit in `graph-store`’s `EdgeMeta`).
pub trait CsrEdgeUndirected: CsrEdge {
    /// `true` if this slot represents an undirected logical edge (caller-defined; typically mirrored in both directions).
    fn is_undirected(&self) -> bool;
    /// Returns a copy with only the undirected flag changed; other fields (including [`CsrEdge::neighbor_vid`]) unchanged unless the type fuses them.
    fn with_undirected(self, undirected: bool) -> Self;
}
