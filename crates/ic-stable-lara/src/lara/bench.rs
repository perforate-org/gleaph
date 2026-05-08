use std::hint::black_box;

use canbench_rs::bench;

use crate::{LaraGraph, VertexId, bench as helper, lara::vertex::Vertex, test_support::TestEdge};

/// Measures repeated public `LaraGraph::insert_edge` calls when most inserts can
/// append into existing vertex spans. This is the broad update-path smoke signal
/// for regressions in insert accounting, segment counts, and light rebalancing.
#[bench(raw)]
fn bench_lara_graph_insert_append_heavy_1024() -> canbench_rs::BenchResult {
    let graph = helper::lara_graph(4096, 16, 256);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_graph_insert_append_heavy");
        for i in 0..helper::MEDIUM_N {
            let i = black_box(i);
            graph
                .insert_edge(VertexId::from((i % 256) as u32), helper::test_edge(i))
                .expect("insert edge");
        }
        black_box(graph.vertices().len());
    })
}

/// Measures slot-order adjacency scans over already materialized slab edges. The
/// goal is to keep read-only traversal independent of capacity, span metadata,
/// free spans, and maintenance structures.
#[bench(raw)]
fn bench_lara_graph_clean_scan_slot_order_1024() -> canbench_rs::BenchResult {
    let graph = helper::populated_lara_graph(256, 4);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_graph_clean_scan_slot_order");
        let mut len = 0usize;
        for src in 0..256 {
            len += graph
                .collect_out_edges_slot_order(VertexId::from(black_box(src as u32)))
                .expect("collect out edges")
                .len();
        }
        black_box(len);
    })
}

/// Measures public iteration over already materialized slab edges. This is the
/// streaming counterpart to `lara_graph_clean_scan_slot_order`.
#[bench(raw)]
fn bench_lara_graph_clean_scan_iter_1024() -> canbench_rs::BenchResult {
    let graph = helper::populated_lara_graph(256, 4);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_graph_clean_scan_iter");
        let mut len = 0usize;
        for src in 0..256 {
            for edge in graph
                .iter_out_edges(VertexId::from(black_box(src as u32)))
                .expect("iterate out edges")
            {
                black_box(edge);
                len += 1;
            }
        }
        black_box(len);
    })
}

/// Like [`bench_lara_graph_clean_scan_iter_1024`], but all edges live on one
/// vertex. This stresses `iter_out_edges` and slab decoding without repeatedly
/// paying per-vertex iterator setup across many small rows.
#[bench(raw)]
fn bench_lara_graph_clean_scan_iter_single_row_1024() -> canbench_rs::BenchResult {
    let graph = helper::populated_lara_graph(1, helper::MEDIUM_N as u32);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_graph_clean_scan_iter_single_row");
        let mut sum = 0u32;
        for edge in graph
            .iter_out_edges(VertexId::from(black_box(0u32)))
            .expect("iterate out edges")
        {
            sum ^= black_box(edge).0;
        }
        black_box(sum);
    })
}

/// Slot-order materialization for a single large slab-backed row (graph API).
/// Pairs with [`bench_lara_graph_clean_scan_iter_single_row_1024`] to separate
/// vector growth from streaming iteration.
#[bench(raw)]
fn bench_lara_graph_clean_scan_slot_order_single_row_1024() -> canbench_rs::BenchResult {
    let graph = helper::populated_lara_graph(1, helper::MEDIUM_N as u32);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_graph_clean_scan_slot_order_single_row");
        black_box(
            graph
                .collect_out_edges_slot_order(VertexId::from(black_box(0u32)))
                .expect("collect out edges"),
        );
    })
}

/// Measures the root-saturation path that relocates a hot segment to fresh tail
/// space. This protects the expensive resize/relocation boundary where span
/// metadata, counts, and free-span release are all updated together.
#[bench(raw)]
fn bench_lara_graph_root_relocation_1() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_graph_root_relocation");
        let graph = helper::lara_graph(4, 1, 2);
        for dst in 10..14 {
            graph
                .insert_edge(VertexId::from(black_box(0u32)), TestEdge(black_box(dst)))
                .expect("insert edge");
        }
        black_box(graph.edges().span_meta_store().get(0).physical_start);
    })
}

/// Measures local relocation that reuses an earlier retired physical span. This
/// is the key LARA locality case: move one dense segment without forcing a full
/// graph resize.
#[bench(raw)]
fn bench_lara_graph_local_relocation_1() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_graph_local_relocation");
        let graph = helper::lara_graph(12, 1, 2);
        for dst in 10..20 {
            graph
                .insert_edge(VertexId::from(black_box(0u32)), TestEdge(black_box(dst)))
                .expect("insert hot vertex");
        }
        for dst in 20..25 {
            graph
                .insert_edge(VertexId::from(black_box(1u32)), TestEdge(black_box(dst)))
                .expect("insert relocated vertex");
        }
        black_box(graph.edges().free_span_store().len());
    })
}

/// Measures reopening a graph after relocation metadata has been persisted. The
/// target is a small, stable cost for validating headers and resuming clean
/// scans from the stored layout.
#[bench(raw)]
fn bench_lara_graph_reopen_after_relocation_1() -> canbench_rs::BenchResult {
    let graph = helper::lara_graph(4, 1, 2);
    for dst in 10..14 {
        graph
            .insert_edge(VertexId::from(black_box(0u32)), TestEdge(black_box(dst)))
            .expect("insert edge");
    }
    let memories = graph.into_memories();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_graph_reopen_after_relocation");
        let reopened = LaraGraph::<TestEdge, Vertex, _>::init(
            memories.0.clone(),
            memories.1.clone(),
            memories.2.clone(),
            memories.3.clone(),
            memories.4.clone(),
            memories.5.clone(),
            memories.6.clone(),
        )
        .expect("reopen graph");
        black_box(
            reopened
                .collect_out_edges_slot_order(VertexId::from(black_box(0u32)))
                .expect("scan"),
        );
    })
}
