//! PocketIC / `canbench` harness for [`ic_stable_csr::dgap::DgapEdgeStore::remove_slab_edge_at_local_index_physically`].
//!
//! See the crate `README.md` for build and run commands.

#![cfg_attr(target_arch = "wasm32", no_main)]

use std::borrow::Cow;
use std::hint::black_box;

use ic_cdk::export_candid;
use ic_cdk_macros::{init, post_upgrade, pre_upgrade};
use ic_stable_csr::{
    Bound, CsrGraphWithGcQueue, DgapEdgeStore, DgapGraphMemories, DgapStores, SegmentEdgeCounts,
    Storable,
    dgap::{RebalanceDecision, SegmentMaintainAction, SegmentMaintainThresholds, segment_maintenance_decision},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex, CsrVertexTombstone},
};
use ic_stable_slot_map::SlotMap;
use ic_stable_structures::DefaultMemoryImpl;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};

mod wipe;

type BenchMemory = VirtualMemory<DefaultMemoryImpl>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BenchVertex {
    slot_base: u64,
    deg: u32,
    log_head: i32,
}

const DEG_TOMB: u32 = 1u32 << 31;

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

impl CsrVertexTombstone for BenchVertex {
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

impl CsrEdgeTombstone for BenchEdge {
    fn is_tombstone(&self) -> bool {
        self.0[2] != 0
    }

    fn with_tombstone(self, tombstone: bool) -> Self {
        let mut b = self.0;
        b[2] = if tombstone { 1 } else { 0 };
        Self(b)
    }
}

type BenchEdgeStore = DgapEdgeStore<BenchEdge, BenchMemory, BenchMemory>;
type BenchStores = DgapStores<BenchVertex, BenchEdge, BenchMemory, BenchMemory, BenchMemory>;
type BenchGcGraph = CsrGraphWithGcQueue<
    BenchVertex,
    BenchEdge,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
    BenchMemory,
>;

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

fn build_star_gc_graph(n: usize) -> BenchGcGraph {
    assert!(n >= 2);
    const ELEM_CAP: u64 = 65_536;
    const SEGMENT_COUNT: u32 = 32;
    const SEGMENT_SIZE: u32 = 128;
    assert!(
        (SEGMENT_COUNT as u64) * (SEGMENT_SIZE as u64) >= n as u64,
        "segment_count * segment_size must cover n vertices"
    );

    let mgr = MemoryManager::init(DefaultMemoryImpl::default());
    let m_v_f = mgr.get(MemoryId::new(0));
    let m_v_r = mgr.get(MemoryId::new(1));
    let m_f_sec = mgr.get(MemoryId::new(2));
    let m_f_edges = mgr.get(MemoryId::new(3));
    let m_r_sec = mgr.get(MemoryId::new(4));
    let m_r_edges = mgr.get(MemoryId::new(5));
    let m_q = mgr.get(MemoryId::new(6));

    let graph = BenchGcGraph::format_new_with_gc_queue(
        m_v_f, m_v_r, m_f_sec, m_f_edges, m_r_sec, m_r_edges, m_q, ELEM_CAP, SEGMENT_COUNT,
        SEGMENT_SIZE, 0, None,
    )
    .expect("format_new_with_gc_queue");

    let template = BenchVertex {
        slot_base: 0,
        deg: 0,
        log_head: -1,
    };
    for _ in 0..n {
        graph.insert_vertex(template).expect("insert_vertex");
    }
    for i in 1..n {
        graph
            .insert_directed(0, i, BenchEdge::default().with_neighbor_vid(i))
            .expect("insert_directed hub out");
        graph
            .insert_directed(i, 0, BenchEdge::default().with_neighbor_vid(0))
            .expect("insert_directed hub in");
    }
    graph.sync_pma_meta().expect("sync_pma_meta");
    graph
}

#[derive(Clone, Copy)]
struct SegmentMaintainBenchCase {
    leaf: SegmentEdgeCounts,
    rebalance: RebalanceDecision,
    queue_len: u64,
    thresholds: SegmentMaintainThresholds,
    expected: SegmentMaintainAction,
}

fn segment_maintain_thresholds() -> SegmentMaintainThresholds {
    SegmentMaintainThresholds::default()
}

fn segment_maintain_case(
    leaf: SegmentEdgeCounts,
    rebalance: RebalanceDecision,
    queue_len: u64,
    expected: SegmentMaintainAction,
) -> SegmentMaintainBenchCase {
    SegmentMaintainBenchCase {
        leaf,
        rebalance,
        queue_len,
        thresholds: segment_maintain_thresholds(),
        expected,
    }
}

fn bench_segment_maintain(case: SegmentMaintainBenchCase) -> canbench_rs::BenchResult {
    crate::wipe::wipe_stable_memory();
    canbench_rs::bench_fn(move || {
        let action = segment_maintenance_decision(
            case.leaf,
            case.rebalance,
            case.queue_len,
            &case.thresholds,
        );
        assert_eq!(action, case.expected, "segment maintenance decision changed");
        black_box(action);
    })
}

use canbench_rs::bench;

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

/// Remove the last chain edge on vertex `n-2` (the tail of the slab): large `remove_pos`, so path A skips almost all base `--`.
#[bench(raw)]
fn bench_remove_slab_physically_tail_vertex_chain_1024() -> canbench_rs::BenchResult {
    crate::wipe::wipe_stable_memory();
    let n = 1024usize;
    let stores = build_chain_stores(n);
    let vid = n - 2;
    canbench_rs::bench_fn(|| {
        stores
            .edges
            .remove_slab_edge_at_local_index_physically(&stores.vertices, vid, 0)
            .expect("remove_slab tail");
    })
}

/// Hub-and-spoke graph where deleting the center touches many live neighbors and exercises the
/// partial forward/reverse PMA resync path added in Phase D.
#[bench(raw)]
fn bench_delete_vertex_hub_star_1024() -> canbench_rs::BenchResult {
    crate::wipe::wipe_stable_memory();
    let graph = build_star_gc_graph(1024);
    canbench_rs::bench_fn(|| {
        graph.delete_vertex(0).expect("delete_vertex");
    })
}

/// Small leaf with a single tombstone stays below the score gate and should not enqueue work.
#[bench(raw)]
fn bench_segment_maintain_small_noop() -> canbench_rs::BenchResult {
    bench_segment_maintain(
        segment_maintain_case(
            SegmentEdgeCounts {
                actual: 10,
                total: 100,
                tombstone: 1,
            },
            RebalanceDecision::Noop,
            0,
            SegmentMaintainAction::Noop,
        ),
    )
}

/// Small leaf crosses the soft ratio threshold and should enqueue.
#[bench(raw)]
fn bench_segment_maintain_small_enqueue() -> canbench_rs::BenchResult {
    bench_segment_maintain(
        segment_maintain_case(
            SegmentEdgeCounts {
                actual: 95,
                total: 100,
                tombstone: 5,
            },
            RebalanceDecision::Noop,
            0,
            SegmentMaintainAction::Enqueue,
        ),
    )
}

/// Same ratio as the small case, but the larger span raises the score enough to enqueue.
#[bench(raw)]
fn bench_segment_maintain_large_enqueue_by_score() -> canbench_rs::BenchResult {
    bench_segment_maintain(
        segment_maintain_case(
            SegmentEdgeCounts {
                actual: 2900,
                total: 3000,
                tombstone: 100,
            },
            RebalanceDecision::Noop,
            0,
            SegmentMaintainAction::Enqueue,
        ),
    )
}

/// Tombstone ratio above the strict gate should inline immediately.
#[bench(raw)]
fn bench_segment_maintain_strict_inline() -> canbench_rs::BenchResult {
    bench_segment_maintain(
        segment_maintain_case(
            SegmentEdgeCounts {
                actual: 7,
                total: 10,
                tombstone: 3,
            },
            RebalanceDecision::Noop,
            0,
            SegmentMaintainAction::InlineNow,
        ),
    )
}

/// Queue pressure only promotes significant tombstone garbage to inline work.
#[bench(raw)]
fn bench_segment_maintain_queue_pressure_inline() -> canbench_rs::BenchResult {
    bench_segment_maintain(
        segment_maintain_case(
            SegmentEdgeCounts {
                actual: 95,
                total: 100,
                tombstone: 5,
            },
            RebalanceDecision::Noop,
            64,
            SegmentMaintainAction::InlineNow,
        ),
    )
}

/// Rebalance hints should enqueue even when tombstones are absent.
#[bench(raw)]
fn bench_segment_maintain_rebalance_window_enqueue() -> canbench_rs::BenchResult {
    bench_segment_maintain(
        segment_maintain_case(
            SegmentEdgeCounts {
                actual: 10,
                total: 100,
                tombstone: 0,
            },
            RebalanceDecision::RebalanceWindow {
                left_vertex: 0,
                right_vertex: 32,
                pma_idx: 16,
            },
            0,
            SegmentMaintainAction::Enqueue,
        ),
    )
}

#[init]
fn init() {}

#[pre_upgrade]
fn pre_upgrade() {}

#[post_upgrade]
fn post_upgrade() {}

export_candid!();
