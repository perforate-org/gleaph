//! **DGAP**-aligned graph edges on Internet Computer stable memory, plus a CSR vertex table **`M_v`**
//! ([`StableVec`] V1, magic **`SVC`**).
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
//! | [`StableVec`] V1 header + items (`SVC` magic)                                      |
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
//! **Segment maintenance queue:** use one of
//! [`csr::CsrGraphWithGcQueueRowTombstone::format_new_with_gc_queue`],
//! [`csr::CsrGraphWithGcQueueSparseDeleted::format_new_with_gc_queue`], or
//! [`csr::CsrGraphWithGcQueueDenseDeleted::format_new_with_gc_queue`]
//! takes one more region for [`StableVecDeque`](crate::StableVecDeque)`<`[`csr::GcWorkItem`](crate::csr::gc_work_item::GcWorkItem)`>`
//! (per-leaf tombstone compaction + PMA sync; not an audit log). Optional [`SegmentMaintainThresholds`]
//! combines PMA density hints with tombstone ratio + size-corrected score (and queue depth).
//! ```
//!
//! Gleaph-specific types (`VertexEntry`, `EdgeEntry`) should implement [`traits::CsrVertex`] /
//! [`traits::CsrEdge`] in `graph-store` (keeps this crate free of `gleaph_graph_kernel`).
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
//! **Bidirectional CSR:** choose one of [`csr::CsrGraphRowTombstone`], [`csr::CsrGraphSparseDeleted`],
//! or [`csr::CsrGraphDenseDeleted`] depending on deleted-vertex tracking strategy.

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

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct VertexId(pub u32);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SegmentId(pub u32);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct VertexCount(pub u64);

pub type SlotIndex = u64;

impl std::fmt::Display for VertexId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for SegmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for VertexCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl VertexId {
    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl SegmentId {
    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl VertexCount {
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn saturating_add(self, rhs: u64) -> Self {
        Self(self.0.saturating_add(rhs))
    }
}

impl From<u32> for VertexId {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<u32> for SegmentId {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<u64> for VertexCount {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<VertexId> for u32 {
    fn from(value: VertexId) -> Self {
        value.0
    }
}

impl From<VertexId> for u64 {
    fn from(value: VertexId) -> Self {
        u64::from(value.0)
    }
}

impl From<VertexId> for usize {
    fn from(value: VertexId) -> Self {
        value.0 as usize
    }
}

impl From<SegmentId> for u32 {
    fn from(value: SegmentId) -> Self {
        value.0
    }
}

impl From<SegmentId> for u64 {
    fn from(value: SegmentId) -> Self {
        u64::from(value.0)
    }
}

impl From<SegmentId> for usize {
    fn from(value: SegmentId) -> Self {
        value.0 as usize
    }
}

impl From<VertexCount> for u64 {
    fn from(value: VertexCount) -> Self {
        value.0
    }
}

impl From<usize> for VertexCount {
    fn from(value: usize) -> Self {
        Self(value as u64)
    }
}

impl TryFrom<usize> for VertexId {
    type Error = std::num::TryFromIntError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        u32::try_from(value).map(Self)
    }
}

impl TryFrom<u64> for VertexId {
    type Error = std::num::TryFromIntError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        u32::try_from(value).map(Self)
    }
}

impl TryFrom<usize> for SegmentId {
    type Error = std::num::TryFromIntError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        u32::try_from(value).map(Self)
    }
}

impl TryFrom<VertexId> for i32 {
    type Error = std::num::TryFromIntError;

    fn try_from(value: VertexId) -> Result<Self, Self::Error> {
        i32::try_from(value.0)
    }
}

impl TryFrom<VertexCount> for usize {
    type Error = std::num::TryFromIntError;

    fn try_from(value: VertexCount) -> Result<Self, Self::Error> {
        usize::try_from(value.0)
    }
}

pub use ic_stable_structures::storable::Bound;
pub use ic_stable_structures::vec::Vec as StableVec;
pub use ic_stable_structures::vec_mem::VectorMemory;
pub use ic_stable_structures::{Memory, Storable};

pub use csr::{
    CsrGraphDenseDeleted, CsrGraphError, CsrGraphRowTombstone, CsrGraphSparseDeleted,
    CsrGraphWithGcQueueDenseDeleted, CsrGraphWithGcQueueRowTombstone,
    CsrGraphWithGcQueueSparseDeleted, CsrInsertError, DgapStores, DgapStoresError, GcWorkItem,
    LogicalNeighborhoodIter, SegmentMaintainAction, SegmentMaintainThresholds,
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
