#![allow(dead_code)]
// Helpers are used across different integration test crates; each crate only uses a subset.

//! Shared [`DgapStores`] test vertex/edge types and dense-order `base_slot_start` checks.
//!
//! `remove_slab` uses binary search on the first row with `base > remove_pos`; that is valid only
//! when dense vertex indices have non-decreasing `base_slot_start`.
//!
//! **Rebalance coverage:** `rebalance_weighted` without `resize_double` is covered by the crate-local
//! unit test `rebalance_dense_window::rebalance_weighted_direct_preserves_dense_bases_after_rebalance_window`
//! (`src/rebalance_dense_window.rs`). Normal [`DgapStores::insert_edge`] runs
//! [`ic_stable_csr::dgap::DgapEdgeStore::maintain_rebalance_loop`] and clears a density window before
//! returning, so that test uses [`ic_stable_csr::dgap::DgapEdgeStore::insert_edge_skip_maintain_for_test`]
//! (test-only, `cfg(test)` build) to leave a `RebalanceWindow` visible after `sync_pma_meta` + full SEC
//! recount. Resize-with-rebalance remains exercised in `insert_maintain_triggers_resize_when_slab_full`
//! in `csr_insert_maintain.rs`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use ic_stable_csr::{
    Bound, DgapGraphMemories, DgapStores, Memory, Storable, VectorMemory, VertexId,
    traits::{CsrEdge, CsrEdgeTombstone, CsrEdgeUndirected, CsrVertex, CsrVertexTombstone},
};

pub const DEG_TOMB: u32 = 1u32 << 31;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestVertex {
    pub slot_base: u64,
    pub deg: u32,
    pub log_head: i32,
}

impl CsrVertex for TestVertex {
    fn base_slot_start(&self) -> u64 {
        self.slot_base
    }
    fn degree(&self) -> u32 {
        self.deg & !DEG_TOMB
    }
    fn with_base_slot_start(self, start: u64) -> Self {
        Self {
            slot_base: start,
            ..self
        }
    }
    fn with_degree(self, degree: u32) -> Self {
        Self {
            deg: (self.deg & DEG_TOMB) | (degree & !DEG_TOMB),
            ..self
        }
    }
    fn log_head(self) -> i32 {
        self.log_head
    }
    fn with_log_head(self, idx: i32) -> Self {
        Self {
            log_head: idx,
            ..self
        }
    }
}

impl CsrVertexTombstone for TestVertex {
    fn is_tombstone(&self) -> bool {
        (self.deg & DEG_TOMB) != 0
    }

    fn with_tombstone(self, tombstone: bool) -> Self {
        Self {
            deg: if tombstone {
                self.deg | DEG_TOMB
            } else {
                self.deg & !DEG_TOMB
            },
            ..self
        }
    }
}

impl Storable for TestVertex {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.slot_base.to_le_bytes());
        b[8..12].copy_from_slice(&self.deg.to_le_bytes());
        b[12..16].copy_from_slice(&self.log_head.to_le_bytes());
        Cow::Owned(b.to_vec())
    }
    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }
    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let s = bytes.as_ref();
        Self {
            slot_base: u64::from_le_bytes(s[0..8].try_into().unwrap()),
            deg: u32::from_le_bytes(s[8..12].try_into().unwrap()),
            log_head: i32::from_le_bytes(s[12..16].try_into().unwrap()),
        }
    }
    const BOUND: Bound = Bound::Bounded {
        max_size: 16,
        is_fixed_size: true,
    };
}

/// `[0]` = neighbor vid (u8), `[1]` = undirected flag, `[2]` = tombstone flag (GC tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestEdge(pub [u8; 4]);

impl CsrEdge for TestEdge {
    const EDGE_BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self(bytes.try_into().unwrap())
    }

    fn write_to(self, bytes: &mut [u8]) {
        bytes.copy_from_slice(&self.0);
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId(self.0[0] as u32)
    }

    fn with_neighbor_vid(self, vid: VertexId) -> Self {
        let mut b = self.0;
        b[0] = vid.get() as u8;
        Self(b)
    }
}

impl CsrEdgeTombstone for TestEdge {
    fn is_tombstone(&self) -> bool {
        self.0[2] != 0
    }

    fn with_tombstone(self, tombstone: bool) -> Self {
        let mut b = self.0;
        b[2] = if tombstone { 1 } else { 0 };
        Self(b)
    }
}

impl CsrEdgeUndirected for TestEdge {
    fn is_undirected(&self) -> bool {
        self.0[1] != 0
    }

    fn with_undirected(self, undirected: bool) -> Self {
        let mut b = self.0;
        b[1] = if undirected { 1 } else { 0 };
        Self(b)
    }
}

pub fn vm() -> VectorMemory {
    Rc::new(RefCell::new(Vec::new()))
}

pub fn empty_vertex() -> TestVertex {
    TestVertex {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    }
}

pub fn dual_edge_memories() -> DgapGraphMemories<VectorMemory, VectorMemory> {
    DgapGraphMemories::new(
        Rc::new(RefCell::new(Vec::new())),
        Rc::new(RefCell::new(Vec::new())),
    )
}

pub fn assert_dense_vertex_bases_non_decreasing<V, E, Mvs, M1, M2>(
    stores: &DgapStores<V, E, Mvs, M1, M2>,
) where
    V: CsrVertex,
    E: CsrEdge,
    Mvs: Memory,
    M1: Memory,
    M2: Memory,
{
    let n = stores.vertices.len() as usize;
    if n < 2 {
        return;
    }
    let mut prev = stores.vertices.get(0).unwrap().base_slot_start();
    for j in 1..n {
        let b = stores
            .vertices
            .get(j as u64)
            .unwrap()
            .base_slot_start();
        assert!(
            prev <= b,
            "dense vertex bases must be non-decreasing: base[{}]={} > base[{}]={}",
            j - 1,
            prev,
            j,
            b
        );
        prev = b;
    }
}
