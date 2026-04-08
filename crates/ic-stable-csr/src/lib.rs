//! **DGAP**-aligned graph edges on Internet Computer stable memory, plus a CSR vertex table **`M_v`**
//! ([`ic_stable_slot_map::SlotMap`] V1, magic **`SSM`**).
//!
//! This crate uses unstable **`specialization`** internally (for optional [`traits::CsrEdgeUndirected`]
//! checks on directed inserts, and PMA node **stride** when [`traits::CsrEdgeTombstone`] is implemented).
//! Dependent crates do not need to enable the feature.
//!
//! Optional Cargo feature **`strict-dgap-invariants`**: before each physical `remove_slab`, performs a
//! full dense-order scan to ensure `base_slot_start` is non-decreasing (validates the binary-search
//! split on `L`; costs `O(n)` vertex reads per remove).
//! Edge state uses **`M_e`** as **two** [`Memory`] regions ([`DgapGraphMemories`]: unified PMA `segment_edge_counts` /
//! edges+log with [`DgapEdgeHeaderV1`](crate::layout::dgap::DgapEdgeHeaderV1) at offset 0 on the latter).
//!
//! # Logical memories (`ic_stable_structures::Memory`)
//!
//! Each named region below is a **separate** [`Memory`] (for example two [`VirtualMemory`](ic_stable_structures::memory_manager::VirtualMemory)
//! instances from [`MemoryManager::get`](ic_stable_structures::memory_manager::MemoryManager::get), plus one for vertices).
//! Layout diagrams in this crate use the same ASCII style as `ic-stable_structures` (`memory_manager`, `base_vec`):
//! horizontal rules, byte sizes (`↕`), and `<- Address 0` on the right.
//!
//! ```text
//! ---------------------------------------- <- M_v (vertex CSR table)
//! | [`ic_stable_slot_map::SlotMap`] V1 header + slot cells (`SSM` magic)              |
//! ----------------------------------------
//!
//! ---------------------------------------- <- M_e memory 1 (`segment_edge_counts`)
//! | V1 mini header `SEC` + packed [`dgap::SegmentEdgeCounts`] PMA tree (16 or 24 B/node) |
//! ----------------------------------------
//!
//! ---------------------------------------- <- M_e memory 2 (`edges_and_log_segment`)
//! | [`DgapEdgeHeaderV1`] (`VCE`) + CSR slab + log idx[] + per-leaf log entry pool     |
//! ----------------------------------------
//!
//! (optional) ----------------------------- <- M_l stream journal
//! | [`layout::log_region`] V1 header `DGL` + append-only records                      |
//! ----------------------------------------
//!
//! **Segment maintenance queue (seventh `Memory`):** [`csr::CsrGraphWithGcQueue::format_new_with_gc_queue`]
//! takes one more region for [`StableVecDeque`](crate::StableVecDeque)`<`[`csr::GcWorkItem`](crate::csr::gc_work_item::GcWorkItem)`>`
//! (per-leaf tombstone compaction + PMA sync; not an audit log). Optional [`SegmentMaintainThresholds`]
//! combines PMA density hints with tombstone soft ratio / minimum tombstone count (and queue depth).
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

// `tests/common/mod.rs` (included by `rebalance_dense_window`) uses `ic_stable_csr::…` paths.
#[cfg(test)]
extern crate self as ic_stable_csr;

mod canbench_scope;
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
    CsrGraph, CsrGraphError, CsrGraphWithGcQueue, CsrInsertError, DgapStores, DgapStoresError,
    GcWorkItem, LogicalNeighborhoodIter, SegmentMaintainAction, SegmentMaintainThresholds,
    insert_edge_into_slab, insert_edge_into_slab_column,
};
pub use dgap::{DgapEdgeStore, DgapGraphMemories, NeighborhoodIter, SegmentEdgeCounts};
pub use ic_stable_vec_deque::VecDeque as StableVecDeque;
pub use memory_util::{GrowFailed, WASM_PAGE_SIZE, memory_byte_len, safe_write};
pub use traits::{
    CsrEdge, CsrEdgeSlotTombstoneScan, CsrEdgeTombstone, CsrEdgeUndirected, CsrVertex,
    CsrVertexTombstone,
};

#[cfg(test)]
mod rebalance_dense_window;
