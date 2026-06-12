//! PocketIC / `canbench` targets for Phase 8 stable-memory layout (ADR 0007 §6).
//!
//! Run from `crates/graph-kernel`: `canbench` (see `canbench.yml`).

use crate::bidirectional_catalog::{
    BidirectionalCatalog, DenseEdgeLabelPolicy, DenseMaxPlusOnePolicy,
};
use crate::entry::{EdgeLabelId, PropertyId, VertexLabelId};
use crate::stable_layout::{GRAPH_STABLE_LAYOUT, INDEX_STABLE_LAYOUT, ROUTER_STABLE_LAYOUT};
use canbench_rs::bench;
use ic_stable_structures::{
    BTreeMap, DefaultMemoryImpl,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
};
use std::hint::black_box;

type Mem = VirtualMemory<DefaultMemoryImpl>;

fn touch_n_empty_maps(n: u8) {
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let mut maps = Vec::with_capacity(n as usize);
    for i in 0..n {
        let mem = manager.get(MemoryId::new(i));
        maps.push(BTreeMap::<u64, u64, Mem>::init(mem));
    }
    for (idx, map) in maps.iter_mut().enumerate() {
        map.insert(black_box(idx as u64), black_box(idx as u64 + 1));
    }
    black_box(maps.len());
}

fn router_three_catalog_intern_round() {
    let manager = MemoryManager::init(DefaultMemoryImpl::default());
    let mut vertex = BidirectionalCatalog::<VertexLabelId, Mem, Mem, DenseMaxPlusOnePolicy>::init(
        manager.get(MemoryId::new(0)),
        manager.get(MemoryId::new(1)),
    );
    let mut edge = BidirectionalCatalog::<EdgeLabelId, Mem, Mem, DenseEdgeLabelPolicy>::init(
        manager.get(MemoryId::new(2)),
        manager.get(MemoryId::new(3)),
    );
    let mut property = BidirectionalCatalog::<PropertyId, Mem, Mem, DenseMaxPlusOnePolicy>::init(
        manager.get(MemoryId::new(4)),
        manager.get(MemoryId::new(5)),
    );

    for i in 0..32u32 {
        let name = format!("name_{i}");
        black_box(vertex.get_or_insert(&format!("v_{name}")).expect("vertex"));
        black_box(edge.get_or_insert(&format!("e_{name}")).expect("edge"));
        black_box(
            property
                .get_or_insert(&format!("p_{name}"))
                .expect("property"),
        );
    }
}

/// Cold `MemoryManager` + one `BTreeMap` init+insert per region (graph-index baseline: 5).
#[bench(raw)]
fn bench_layout_memory_manager_cold_touch_5() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("layout_mm_touch");
        touch_n_empty_maps(INDEX_STABLE_LAYOUT.region_count() as u8);
    })
}

/// Same as [`bench_layout_memory_manager_cold_touch_5`] for router layout (21 regions).
#[bench(raw)]
fn bench_layout_memory_manager_cold_touch_21() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("layout_mm_touch");
        touch_n_empty_maps(ROUTER_STABLE_LAYOUT.region_count() as u8);
    })
}

/// Same as [`bench_layout_memory_manager_cold_touch_5`] for graph layout (42 regions).
#[bench(raw)]
fn bench_layout_memory_manager_cold_touch_42() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("layout_mm_touch");
        touch_n_empty_maps(GRAPH_STABLE_LAYOUT.region_count() as u8);
    })
}

/// Router resolution path: three `BidirectionalCatalog` pairs (six `VirtualMemory` regions).
#[bench(raw)]
fn bench_layout_router_three_catalog_intern_6vm() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("layout_router_catalog_intern");
        router_three_catalog_intern_round();
    })
}
