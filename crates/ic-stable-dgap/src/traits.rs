//! Minimal traits for DGAP CSR edges and vertices without Gleaph-specific types.
//!
//! `graph-store` can implement [`CsrVertex`] / [`CsrEdge`] for `VertexEntry` / `EdgeEntry` in a follow-up
//! (keeps this crate free of `gleaph_graph_kernel`).

use ic_stable_structures::Storable;

use crate::VertexId;

/// One vertex row in the CSR vertex column (`M_v`).
///
/// `log_head` is the DGAP per-segment log array index of the head of this vertex's overflow chain,
/// or `-1` if all neighbors live on the CSR slab (`gleaph-old/reference/DGAP/dgap/src/graph.h` `vertex_element.offset`).
pub trait CsrVertex: Storable + Copy {
    const BYTES: usize;
    /// Global edge-slot index where this vertex's base neighborhood starts (flat slab model).
    fn base_slot_start(&self) -> u64;
    fn degree(&self) -> u32;
    fn with_base_slot_start(self, start: u64) -> Self;
    fn with_degree(self, degree: u32) -> Self;

    fn log_head(self) -> i32;
    fn with_log_head(self, idx: i32) -> Self;
}

pub trait CsrVertexTombstone: CsrVertex {
    fn is_tombstone(&self) -> bool;
    fn with_tombstone(self, tomb: bool) -> Self;
}

/// One fixed-width **edge record** stored in a CSR slab cell (`M_e`).
///
/// In the forward CSR, [`Self::neighbor_vid`](Self::neighbor_vid) is the **other** endpoint (out-neighbor).
/// The CSR graph wrappers build the transpose CSR by storing [`Self::with_neighbor_vid`](Self::with_neighbor_vid)(`src`)
/// at row `dst` in the reverse store.
///
/// Hot paths use a **64-byte stack buffer** when `BYTES <= 64`; larger widths still work via heap.
pub trait CsrEdge: Copy {
    const BYTES: usize;
    fn read_from(bytes: &[u8]) -> Self;
    fn write_to(self, bytes: &mut [u8]);

    /// Adjacent vertex id for this orientation (out-neighbor in the forward CSR).
    fn neighbor_vid(&self) -> VertexId;
    fn with_neighbor_vid(self, vid: VertexId) -> Self;
}

pub trait CsrEdgeTombstone: CsrEdge {
    fn is_tombstone(&self) -> bool;
    fn with_tombstone(self, tomb: bool) -> Self;
}

/// Physical slab tombstone detection for PMA `tombstone` recounts.
///
/// Default: never counts slots as tombstones. Specialized when [`CsrEdgeTombstone`] is implemented.
pub trait CsrEdgeSlotTombstoneScan: CsrEdge {
    fn record_is_physical_tombstone(record: &Self) -> bool;
}

impl<E: CsrEdge> CsrEdgeSlotTombstoneScan for E {
    default fn record_is_physical_tombstone(_: &Self) -> bool {
        false
    }
}

impl<E: CsrEdge + CsrEdgeTombstone> CsrEdgeSlotTombstoneScan for E {
    fn record_is_physical_tombstone(e: &Self) -> bool {
        e.is_tombstone()
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
