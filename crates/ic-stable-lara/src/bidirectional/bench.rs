use std::hint::black_box;

use canbench_rs::bench;

use crate::{
    VertexId, bench as helper,
    test_support::{TestEdge, UndirectedTestEdge},
};

/// Measures directed insertion through the bidirectional wrapper. One logical
/// edge writes both forward and reverse LARA stores, so this tracks validation
/// plus the two oriented insert paths.
#[bench(raw)]
fn bench_lara_bidirectional_insert_directed_1024() -> canbench_rs::BenchResult {
    let graph = helper::bidirectional_graph::<TestEdge>(256);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_bidirectional_insert_directed");
        for i in 0..helper::MEDIUM_N {
            let i = black_box(i);
            let src = (i % 256) as u32;
            let dst = ((i + 1) % 256) as u32;
            graph
                .insert_directed(
                    VertexId::from(black_box(src)),
                    VertexId::from(black_box(dst)),
                    TestEdge(black_box(dst)),
                )
                .expect("insert directed");
        }
        black_box(graph.vertex_count());
    })
}

/// Measures undirected insertion through the bidirectional wrapper. This is
/// intentionally heavier than directed insert because one logical edge
/// materializes symmetric adjacency in both forward and reverse stores.
#[bench(raw)]
fn bench_lara_bidirectional_insert_undirected_1024() -> canbench_rs::BenchResult {
    let graph = helper::bidirectional_graph::<UndirectedTestEdge>(256);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_bidirectional_insert_undirected");
        for i in 0..helper::MEDIUM_N {
            let i = black_box(i);
            let src = (i % 256) as u32;
            let dst = ((i + 1) % 256) as u32;
            graph
                .insert_undirected(
                    VertexId::from(black_box(src)),
                    VertexId::from(black_box(dst)),
                    helper::undirected_edge(black_box(dst)),
                )
                .expect("insert undirected");
        }
        black_box(graph.vertex_count());
    })
}

/// Measures read-side fan-out over both orientations after setup. The goal is
/// to catch regressions where bidirectional APIs add overhead on top of the
/// underlying clean LARA scans.
#[bench(raw)]
fn bench_lara_bidirectional_scan_in_out_1024() -> canbench_rs::BenchResult {
    let graph = helper::bidirectional_graph::<TestEdge>(256);
    for i in 0..helper::MEDIUM_N {
        let src = (i % 256) as u32;
        let dst = ((i + 1) % 256) as u32;
        graph
            .insert_directed(VertexId::from(src), VertexId::from(dst), TestEdge(dst))
            .expect("insert directed");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_bidirectional_scan_in_out");
        let mut len = 0usize;
        for vid in 0..256 {
            let vid_u32 = black_box(vid as u32);
            len += graph
                .collect_out_edges_slot_order(VertexId::from(vid_u32))
                .expect("out")
                .len();
            len += graph
                .collect_in_edges_slot_order(VertexId::from(vid_u32))
                .expect("in")
                .len();
        }
        black_box(len);
    })
}
