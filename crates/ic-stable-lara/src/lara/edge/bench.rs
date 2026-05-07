use std::hint::black_box;

use canbench_rs::bench;

use super::EdgeStore;
use crate::{
    VertexId, bench as helper,
    lara::vertex::{Vertex, VertexStore},
    test_support::TestEdge,
};

fn edge_store_with_vertices(
    vertex_count: u32,
    capacity: u32,
) -> (
    VertexStore<Vertex, helper::BenchMemory>,
    EdgeStore<TestEdge, helper::BenchMemory>,
) {
    let mut memories = helper::BenchMemoryFactory::new();
    let vertices = VertexStore::new(memories.memory()).expect("vertices");
    for vid in 0..vertex_count {
        vertices
            .push(Vertex {
                base_slot_start: u64::from(vid) * u64::from(capacity),
                degree: 0,
                capacity,
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
        u64::from(vertex_count) * u64::from(capacity),
        vertex_count.div_ceil(16).max(1),
        16,
    )
    .expect("edge store");
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
            edges
                .insert_edge(&vertices, VertexId::from(i as u32), helper::test_edge(i))
                .expect("insert slab edge");
        }
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
            edges
                .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
                .expect("insert log edge");
        }
        black_box(vertices.get(VertexId::from(0)).log_head);
    })
}

/// Measures collecting one large neighborhood from slab storage after setup.
/// This protects the clean scan contract at the `EdgeStore` layer, including
/// decoding fixed-width edge records into a caller-owned vector.
#[bench(raw)]
fn bench_lara_edge_store_collect_out_edges_1024() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, helper::MEDIUM_N as u32);
    for i in 0..helper::MEDIUM_N {
        edges
            .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
            .expect("insert edge");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_collect_out_edges");
        black_box(
            edges
                .collect_out_edges(&vertices, VertexId::from(0))
                .expect("collect edges"),
        );
    })
}

/// Measures reverse iteration over one large slab-backed neighborhood without
/// materializing the whole row into a vector.
#[bench(raw)]
fn bench_lara_edge_store_iter_out_edges_rev_1024() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, helper::MEDIUM_N as u32);
    for i in 0..helper::MEDIUM_N {
        edges
            .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
            .expect("insert edge");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_iter_out_edges_rev");
        let mut count = 0usize;
        for edge in edges
            .iter_out_edges_rev(&vertices, VertexId::from(0))
            .expect("iterate edges")
        {
            black_box(edge);
            count += 1;
        }
        black_box(count);
    })
}

/// Measures reverse iteration over a log-backed row. The iterator follows the
/// overflow chain first, then walks the slab tail, matching the reverse of
/// `collect_out_edges` without allocating the collected edge vector.
#[bench(raw)]
fn bench_lara_edge_store_iter_out_edges_rev_log_backed_128() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, 1);
    for i in 0..128 {
        edges
            .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
            .expect("insert edge");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_iter_out_edges_rev_log_backed");
        let mut count = 0usize;
        for edge in edges
            .iter_out_edges_rev(&vertices, VertexId::from(0))
            .expect("iterate edges")
        {
            black_box(edge);
            count += 1;
        }
        black_box(count);
    })
}
