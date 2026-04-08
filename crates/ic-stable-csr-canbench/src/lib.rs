//! PocketIC / `canbench` harness for [`ic_stable_csr::dgap::DgapEdgeStore::remove_slab_edge_at_local_index_physically`].
//!
//! See the crate `README.md` for build and run commands.

#![cfg_attr(target_arch = "wasm32", no_main)]

use std::borrow::Cow;

use ic_cdk::export_candid;
use ic_cdk_macros::{init, post_upgrade, pre_upgrade};
use ic_stable_csr::{
    Bound, DgapEdgeStore, DgapGraphMemories, DgapStores, Storable,
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_slot_map::SlotMap;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::DefaultMemoryImpl;

mod wipe;

type BenchMemory = VirtualMemory<DefaultMemoryImpl>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BenchVertex {
    slot_base: u64,
    deg: u32,
    log_head: i32,
}

impl CsrVertex for BenchVertex {
    fn base_slot_start(&self) -> u64 {
        self.slot_base
    }
    fn degree(&self) -> u32 {
        self.deg
    }
    fn with_base_slot_start(self, start: u64) -> Self {
        Self {
            slot_base: start,
            ..self
        }
    }
    fn with_degree(self, degree: u32) -> Self {
        Self {
            deg: degree,
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

impl Storable for BenchVertex {
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BenchEdge([u8; 4]);

impl CsrEdge for BenchEdge {
    const EDGE_BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self(bytes.try_into().unwrap())
    }

    fn write_to(self, bytes: &mut [u8]) {
        bytes.copy_from_slice(&self.0);
    }

    fn neighbor_vid(&self) -> usize {
        u32::from_le_bytes(self.0) as usize
    }

    fn with_neighbor_vid(self, vid: usize) -> Self {
        Self((vid as u32).to_le_bytes())
    }
}

type BenchEdgeStore = DgapEdgeStore<BenchEdge, BenchMemory, BenchMemory>;
type BenchStores = DgapStores<BenchVertex, BenchEdge, BenchMemory, BenchMemory, BenchMemory>;

/// Chain `0→1→…→(n-1)` on the slab so vertex `0` has one out-edge at local index `0` and
/// `remove_pos == 0` while every other row has `base > 0`, maximizing work in the
/// `dgap_remove_slab_base_decrement` scan.
fn build_chain_stores(n: usize) -> BenchStores {
    assert!(n >= 2);
    const ELEM_CAP: u64 = 65_536;
    const SEGMENT_COUNT: u32 = 32;
    const SEGMENT_SIZE: u32 = 128;
    assert!(
        (SEGMENT_COUNT as u64) * (SEGMENT_SIZE as u64) >= n as u64,
        "segment_count * segment_size must cover n vertices"
    );

    let mgr = MemoryManager::init(DefaultMemoryImpl::default());
    let m_v = mgr.get(MemoryId::new(0));
    let m_sec = mgr.get(MemoryId::new(1));
    let m_edges = mgr.get(MemoryId::new(2));

    let vertices = SlotMap::new(m_v).expect("vertex SlotMap");
    let edges = BenchEdgeStore::new(DgapGraphMemories::new(m_sec, m_edges));
    edges
        .format_new(ELEM_CAP, SEGMENT_COUNT, SEGMENT_SIZE, 0)
        .expect("format_new edge region");

    let stores = DgapStores::new(vertices, edges);
    let template = BenchVertex {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    };
    for _ in 0..n {
        stores.insert_vertex(template).expect("insert_vertex");
    }
    for i in 0..n.saturating_sub(1) {
        stores
            .insert_edge(i, BenchEdge::default().with_neighbor_vid(i + 1))
            .expect("insert_edge");
    }
    stores
        .refresh_slab_occupied_tail_meta()
        .expect("refresh_slab_occupied_tail_meta");
    stores.sync_pma_meta().expect("sync_pma_meta");
    stores
}

mod canbench_benches {
    use canbench_rs::bench;

    use super::*;

    #[bench(raw)]
    fn bench_remove_slab_physically_chain_32() -> canbench_rs::BenchResult {
        crate::wipe::wipe_stable_memory();
        let stores = build_chain_stores(32);
        canbench_rs::bench_fn(|| {
            stores
                .edges
                .remove_slab_edge_at_local_index_physically(&stores.vertices, 0, 0)
                .expect("remove_slab");
        })
    }

    #[bench(raw)]
    fn bench_remove_slab_physically_chain_1024() -> canbench_rs::BenchResult {
        crate::wipe::wipe_stable_memory();
        let stores = build_chain_stores(1024);
        canbench_rs::bench_fn(|| {
            stores
                .edges
                .remove_slab_edge_at_local_index_physically(&stores.vertices, 0, 0)
                .expect("remove_slab");
        })
    }
}

#[init]
fn init() {}

#[pre_upgrade]
fn pre_upgrade() {}

#[post_upgrade]
fn post_upgrade() {}

export_candid!();
