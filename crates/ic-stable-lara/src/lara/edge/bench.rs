use std::hint::black_box;

use canbench_rs::bench;

use super::{EdgeStore, counts::SegmentEdgeCounts, segment_tree_leaf_count};
use crate::{
    VertexId, bench as helper,
    lara::vertex::{Vertex, VertexStore},
    test_support::TestEdge,
};

/// Matches [`EdgeStore::new`] / [`EdgeStore::grow_segment_tree_to`] in this module.
const BENCH_EDGE_SEGMENT_SIZE: u32 = 16;

fn edge_store_with_vertices(
    vertex_count: u32,
    slot_stride: u32,
) -> (
    VertexStore<Vertex, helper::BenchMemory>,
    EdgeStore<TestEdge, helper::BenchMemory>,
) {
    let mut memories = helper::BenchMemoryFactory::new();
    let vertices = VertexStore::new(memories.memory()).expect("vertices");
    for vid in 0..vertex_count {
        vertices
            .push(Vertex {
                base_slot_start: u64::from(vid) * u64::from(slot_stride),
                live_edges: 0,
                slab_slots: 0,
                log_head: -1,
                deleted: false,
            })
            .expect("push vertex");
    }
    let edges = EdgeStore::new(
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        u64::from(vertex_count) * u64::from(slot_stride),
        BENCH_EDGE_SEGMENT_SIZE,
        0,
    )
    .expect("edge store");
    let seg_count = segment_tree_leaf_count(vertex_count.into(), BENCH_EDGE_SEGMENT_SIZE);
    edges
        .grow_segment_tree_to(seg_count)
        .expect("grow edge segments");
    // With a single vertex row but a wide slab (`slot_stride > 1`), PMA leaf totals stay
    // zero unless a graph initializer runs — `slab_window_exclusive_end` then reports a
    // zero-width CSR window and every insert overflows into the segment log (`SegmentLogFull`).
    // Real graphs set leaf totals via `LaraGraph::update_leaf_count_and_ancestors`; mirror
    // that here for multi-slot single-vertex workloads (log-spill benches keep stride `1`).
    if vertex_count == 1 && slot_stride > 1 {
        let elem_cap = u64::from(vertex_count).saturating_mul(u64::from(slot_stride));
        let total_i64 = i64::try_from(elem_cap).unwrap_or(i64::MAX);
        let idx = u64::from(seg_count);
        edges.counts_store().set(
            idx,
            &SegmentEdgeCounts {
                actual: 0,
                total: total_i64,
            },
        );
    }
    (vertices, edges)
}

/// Measures `EdgeStore::insert_edge` when each insert fits directly in the
/// vertex-owned slab span. This isolates the update-side fast path before log
/// spill or graph-level rebalance is involved.
#[bench(raw)]
fn bench_lara_edge_store_slab_insert_1024() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1024, 4);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_slab_insert");
        for i in 0..helper::MEDIUM_N {
            let i = black_box(i);
            edges
                .insert_edge(&vertices, VertexId::from(i as u32), helper::test_edge(i))
                .expect("insert slab edge");
        }
        black_box(vertices.len());
    })
}

/// Measures overflow-log admission after a tiny owned slab span fills. The
/// workload stays below the per-segment log cap and watches for regressions in
/// log-chain writes and vertex `log_head` updates.
#[bench(raw)]
fn bench_lara_edge_store_log_spill_128() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, 1);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_log_spill");
        for i in 0..128 {
            let i = black_box(i);
            edges
                .insert_edge(
                    &vertices,
                    VertexId::from(black_box(0u32)),
                    helper::test_edge(i),
                )
                .expect("insert log edge");
        }
        black_box(vertices.get(VertexId::from(0)).log_head);
    })
}

/// Measures collecting one large neighborhood from slab storage after setup.
/// This protects the clean scan contract at the `EdgeStore` layer, including
/// decoding fixed-width edge records into a caller-owned vector.
#[bench(raw)]
fn bench_lara_edge_store_collect_out_edges_slot_order_1024() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, helper::MEDIUM_N as u32);
    for i in 0..helper::MEDIUM_N {
        edges
            .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
            .expect("insert edge");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_collect_out_edges_slot_order");
        black_box(
            edges
                .collect_out_edges_slot_order(&vertices, VertexId::from(black_box(0u32)))
                .expect("collect edges"),
        );
    })
}

/// Measures iteration over one large slab-backed neighborhood without
/// materializing the whole row into a vector.
#[bench(raw)]
fn bench_lara_edge_store_iter_out_edges_1024() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, helper::MEDIUM_N as u32);
    for i in 0..helper::MEDIUM_N {
        edges
            .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
            .expect("insert edge");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_iter_out_edges");
        let mut count = 0usize;
        for edge in edges
            .iter_out_edges(&vertices, VertexId::from(black_box(0u32)))
            .expect("iterate edges")
        {
            black_box(edge);
            count += 1;
        }
        black_box(count);
    })
}

/// Measures iteration over a log-backed row. The iterator follows the overflow
/// chain first, then walks the slab tail without allocating the collected edge
/// vector.
#[bench(raw)]
fn bench_lara_edge_store_iter_out_edges_log_backed_128() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, 1);
    for i in 0..128 {
        edges
            .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
            .expect("insert edge");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_iter_out_edges_log_backed");
        let mut count = 0usize;
        for edge in edges
            .iter_out_edges(&vertices, VertexId::from(black_box(0u32)))
            .expect("iterate edges")
        {
            black_box(edge);
            count += 1;
        }
        black_box(count);
    })
}
