//! VCSR / CSR utilities for Internet Computer stable memory with **two logical [`Memory`] regions**:
//! `M_v` (vertex CSR column) and `M_e` (DGAP-style edge region: PMA meta, CSR slab, per-leaf overflow logs).
//!
//! Gleaph-specific types (`VertexEntry`, `EdgeEntry`) should implement [`traits::CsrVertex`] /
//! [`traits::CsrEdgeSlot`] in `graph-pma` (keeps this crate free of `gleaph_graph_kernel`).
//!
//! The [`dgap`] module remains for **append-only stream** tooling on a separate memory (`layout::log_region`);
//! it is not used by [`VcsrStores::insert_edge`].

pub mod csr;
pub mod dgap;
pub mod layout;
pub mod memory_util;
pub mod traits;
pub mod vcsr;

pub use ic_stable_structures::storable::Bound;
pub use ic_stable_structures::vec::Vec as StableVec;
pub use ic_stable_structures::vec_mem::VectorMemory;
pub use ic_stable_structures::{Memory, Storable};

pub use csr::{
    insert_edge_into_slab, insert_edge_into_slab_column, CsrInsertError, CsrVertexColumn, VcsrStores,
    VcsrStoresError,
};
pub use ic_stable_vec_deque::VecDeque as StableVecDeque;
pub use memory_util::{memory_byte_len, safe_write, GrowFailed, WASM_PAGE_SIZE};
pub use traits::{CsrEdgeSlot, CsrVertex};
pub use vcsr::VcsrEdgeStore;
