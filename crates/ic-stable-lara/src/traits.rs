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
/// On [`crate::labeled::record::LabeledVertex`], normal (bucket) mode uses [`Self::stored_degree`]
/// as the live label-bucket row count for edge-store geometry; edge bytes are sized separately
/// via [`crate::labeled::record::LabeledVertex::stored_slots`] and per-bucket spans.
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

    /// Updates this row after one on-slab edge was tombstoned in place (no swap).
    fn after_slab_tombstone_delete(self) -> Self;

    /// Grows the packed slab row by one live edge (append path when no tombstoned reuse).
    fn grow_packed_slab_by_one(self) -> Self;

    /// Fallible grow used by [`crate::lara::edge::EdgeStore::insert_edge`]; default delegates
    /// to [`Self::grow_packed_slab_by_one`]. [`crate::labeled::record::LabeledVertex`] checks
    /// overflow in release instead of panicking.
    #[inline]
    fn try_grow_packed_slab_by_one(self) -> Result<Self, ()> {
        Ok(self.grow_packed_slab_by_one())
    }

    /// After writing into the first tombstoned slab slot at `base + degree()` (see
    /// [`Self::after_slab_tombstone_delete`]), adjusts counters so [`Self::degree`] grows by one
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

    /// Minimum exclusive end for the next on-slab append at `base + stored_degree()`.
    ///
    /// Default-label bypass rows return `base + stored_slots + 1` so [`crate::lara::edge::EdgeStore::insert_edge`]
    /// can grow past the PMA leaf `initial_vertex_edge_slots` window without a spurious
    /// overflow-log spill.
    #[inline]
    fn slab_append_exclusive_end(self, base: u64) -> Option<u64> {
        let _ = (self, base);
        None
    }
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
pub trait CsrEdge: Clone {
    /// Fixed byte width of one encoded edge record.
    const BYTES: usize;
    /// Decodes an edge record from exactly [`Self::BYTES`] bytes.
    fn read_from(bytes: &[u8]) -> Self;
    /// Encodes this edge record into exactly [`Self::BYTES`] bytes.
    fn write_to(&self, bytes: &mut [u8]);

    /// Adjacent vertex id for this orientation (out-neighbor in the forward CSR).
    fn neighbor_vid(&self) -> VertexId;
    /// Returns a copy with the adjacent vertex id changed.
    fn with_neighbor_vid(&self, vid: VertexId) -> Self;
    /// Returns a copy annotated with the physical slot index from which it was read.
    #[inline]
    fn with_slot_index(self, _slot_index: u32) -> Self {
        self
    }

    /// Returns a copy annotated with the label-row id from which it was read.
    #[inline]
    fn with_label_id(self, _label_id: u16) -> Self {
        self
    }

    /// Label-row id attached by scanners, when the edge type carries it.
    #[inline]
    fn edge_label_id_raw(&self) -> Option<u16> {
        None
    }

    /// Returns `true` when this slot holds a logical delete marker.
    ///
    /// This is the canonical liveness predicate used by storage read paths, including
    /// paths that only require [`CsrEdge`]. Edge layouts that encode tombstones outside
    /// [`Self::neighbor_vid`] must override this method.
    #[inline]
    fn is_deleted_slot(&self) -> bool {
        self.neighbor_vid().is_edge_tombstone_sentinel()
    }

    /// Physical byte width of the in-memory edge inline value (0 when absent).
    #[inline]
    fn edge_inline_value_byte_width(&self) -> u16 {
        0
    }

    /// In-memory edge inline value bytes; length must match [`Self::edge_inline_value_byte_width`].
    #[inline]
    fn edge_inline_value_bytes(&self) -> &[u8] {
        &[]
    }

    /// Returns a copy with in-memory payload bytes attached (wire row unchanged).
    #[inline]
    fn with_stored_inline_value_bytes(self, _width: u16, _bytes: &[u8]) -> Self {
        self
    }

    /// Logical slot index within the label row (0 when not set by the scanner).
    #[inline]
    fn edge_slot_index_raw(&self) -> u32 {
        0
    }
}

/// Edge records that support **logical slab deletion** without swap-remove.
///
/// New compact edge layouts generally encode this as a tombstone bit in the slot payload. Those
/// layouts must also override [`CsrEdge::is_deleted_slot`] so read paths with only a [`CsrEdge`]
/// bound preserve the same liveness contract. Older test edges may still use
/// [`VertexId::EDGE_TOMBSTONE_SENTINEL`] as their sentinel.
pub trait CsrEdgeTombstone: CsrEdge {
    /// Encoded edge inline value for a tombstoned slab slot (must satisfy [`Self::is_tombstone_edge`]).
    fn tombstone_edge() -> Self;
    /// Returns `true` when this slot holds a logical tombstone.
    #[inline]
    fn is_tombstone_edge(&self) -> bool {
        self.is_deleted_slot()
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
