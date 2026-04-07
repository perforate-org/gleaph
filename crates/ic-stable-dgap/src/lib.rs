//! **DGAP**-aligned graph edges on Internet Computer stable memory, plus a plain CSR vertex column **`M_v`**.
//!
//! This crate uses unstable **`specialization`** internally (for optional [`traits::CsrEdgeUndirected`]
//! checks on directed inserts). Dependent crates do not need to enable the feature.
//! Edge state uses **`M_e`** as **three** [`Memory`] regions ([`DgapGraphMemories`]: PMA actual / PMA total /
//! edges+log with [`DgapEdgeHeaderV1`](crate::layout::dgap::DgapEdgeHeaderV1) at offset 0 on the latter). **v2** single-`Memory` `M_e` layouts are not read or migrated.
//!
//! # Logical memories (`ic_stable_structures::Memory`)
//!
//! Each named region below is a **separate** [`Memory`] (for example three [`VirtualMemory`](ic_stable_structures::memory_manager::VirtualMemory)
//! instances from [`MemoryManager::get`](ic_stable_structures::memory_manager::MemoryManager::get), plus one for vertices).
//! Layout diagrams in this crate use the same ASCII style as `ic-stable_structures` (`memory_manager`, `base_vec`):
//! horizontal rules, byte sizes (`↕`), and `<- Address 0` on the right.
//!
//! ```text
//! -------------------------------------------------- <- M_v (vertex CSR column)
//! | [`ic_stable_structures::vec::Vec`] V1 header + element slots (`SVC` magic)      |
//! --------------------------------------------------
//!
//! -------------------------------------------------- <- M_e memory 1 (`segment_edges_actual`)
//! | V1 mini header `VCA` + `segment_edges_actual` PMA `i64` tree array              |
//! --------------------------------------------------
//!
//! -------------------------------------------------- <- M_e memory 2 (`segment_edges_total`)
//! | V1 mini header `VCT` + `segment_edges_total` PMA `i64` tree array                 |
//! --------------------------------------------------
//!
//! -------------------------------------------------- <- M_e memory 3 (`edges_and_log_segment`)
//! | [`DgapEdgeHeaderV1`] (`VCE`) + CSR slab + log idx[] + per-leaf log entry pool     |
//! --------------------------------------------------
//!
//! (optional) -------------------------------------------------- <- M_l stream journal
//! | [`layout::log_region`] V1 header `DGL` + append-only records                      |
//! --------------------------------------------------
//!
//! **GC queue (ninth `Memory`):** [`csr::CsrGraphWithGcQueue::format_new_with_gc_queue`] takes one extra
//! region for [`StableVecDeque`](crate::StableVecDeque)`<`[`csr::GcWorkItem`](crate::csr::gc_work_item::GcWorkItem)`>` (not an audit log).
//! ```
//!
//! Gleaph-specific types (`VertexEntry`, `EdgeEntry`) should implement [`traits::CsrVertex`] /
//! [`traits::CsrEdge`] in `graph-pma` (keeps this crate free of `gleaph_graph_kernel`).
//!
//! Optional **append-only stream** helpers live in [`layout::log_region`] (re-exported from [`layout`]);
//! they are not used by [`DgapStores::insert_edge`] / [`DgapStores::insert_edges`].
//!
//! **Vertices:** use [`DgapStores::insert_vertex`] to append rows to `M_v` (it sets `base_slot_start`
//! from the edge store’s append cursor; use [`DgapStores::insert_vertex_strict`] to require a matching
//! caller-supplied base). Subject to [`DgapEdgeStore::max_vertex_slots`] for the formatted
//! `segment_count` / `segment_size`.
//!
//! **New `M_e` layout:** [`layout::suggested_format`] / [`layout::DgapSuggestedFormat`] propose
//! `elem_capacity`, `segment_count`, and `segment_size` for [`DgapEdgeStore::format_new`] (heuristic;
//! see that type’s docs for `tree_height` and log-pool limits not included).
//!
//! **Bidirectional CSR:** [`csr::CsrGraph`] keeps forward and transpose [`DgapStores`] in sync; prefer
//! [`CsrGraph::format_new`](csr::CsrGraph::format_new) over assembling [`DgapStores`] by hand.

#![feature(specialization)]

pub mod csr;
pub mod dgap;
pub mod layout;
pub mod memory_util;
pub mod traits;

pub use ic_stable_structures::storable::Bound;
pub use ic_stable_structures::vec::Vec as StableVec;
pub use ic_stable_structures::vec_mem::VectorMemory;
pub use ic_stable_structures::{Memory, Storable};

pub use csr::{
    CsrGraph, CsrGraphError, CsrGraphWithGcQueue, CsrInsertError, CsrVertexColumn, DgapStores,
    DgapStoresError, GcWorkItem, LogicalNeighborhoodIter, insert_edge_into_slab,
    insert_edge_into_slab_column,
};
pub use dgap::{DgapEdgeStore, DgapGraphMemories, NeighborhoodIter};
pub use ic_stable_vec_deque::VecDeque as StableVecDeque;
pub use memory_util::{GrowFailed, WASM_PAGE_SIZE, memory_byte_len, safe_write};
pub use traits::{
    CsrEdge, CsrEdgeTombstone, CsrEdgeUndirected, CsrVertex, CsrVertexTombstone,
};
