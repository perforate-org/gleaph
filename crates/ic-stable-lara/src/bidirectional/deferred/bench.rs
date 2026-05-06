use std::hint::black_box;

use canbench_rs::bench;

use crate::{MaintenanceBudget, VertexId, bench as helper, test_support::TestEdge};

/// Measures deferred directed insertion across forward and reverse stores. The
/// benchmark includes dirty/urgent queue marking but excludes later maintenance
/// work, so it isolates write admission cost.
#[bench(raw)]
fn bench_lara_deferred_bidirectional_insert_directed_1024() -> canbench_rs::BenchResult {
    let graph = helper::deferred_bidirectional_graph(256);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_deferred_bidirectional_insert_directed");
        for i in 0..helper::MEDIUM_N {
            let src = (i % 256) as u32;
            let dst = ((i + 1) % 256) as u32;
            graph
                .insert_directed_deferred(VertexId::from(src), VertexId::from(dst), TestEdge(dst))
                .expect("insert directed deferred");
        }
        black_box(graph.forward().maintenance_queue().len());
    })
}

/// Measures deferred undirected insertion across both orientations. This keeps
/// an eye on the most expensive logical insert shape: symmetric adjacency plus
/// deferred maintenance bookkeeping.
#[bench(raw)]
fn bench_lara_deferred_bidirectional_insert_undirected_1024() -> canbench_rs::BenchResult {
    let graph = crate::DeferredBidirectionalLaraGraph::<
        crate::test_support::UndirectedTestEdge,
        crate::Vertex,
        crate::VectorMemory,
    >::new_with_config(
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        crate::test_support::vector_memory(),
        1024,
        16,
        16,
        crate::DeferredConfig {
            leaf_dirty_density: 0.0,
            log_urgent_ratio: 0.80,
        },
    )
    .expect("deferred bidirectional graph");
    for vid in 0..256u32 {
        graph
            .push_vertex(helper::vertex(u64::from(vid) * 16, 0))
            .expect("push vertex");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_deferred_bidirectional_insert_undirected");
        for i in 0..helper::MEDIUM_N {
            let src = (i % 256) as u32;
            let dst = ((i + 1) % 256) as u32;
            graph
                .insert_undirected_deferred(
                    VertexId::from(src),
                    VertexId::from(dst),
                    helper::undirected_edge(dst),
                )
                .expect("insert undirected deferred");
        }
        black_box(graph.forward().maintenance_queue().len());
    })
}

/// Measures combined deferred maintenance choosing and draining work from the
/// forward/reverse queues. The intent is to protect the scheduling loop and one
/// maintenance fold per orientation from unexpected growth.
#[bench(raw)]
fn bench_lara_deferred_bidirectional_maintenance_drain_1() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_deferred_bidirectional_maintenance_drain");
        let graph = helper::deferred_bidirectional_graph(16);
        for i in 0..64 {
            let dst = ((i + 1) % 16) as u32;
            graph
                .insert_directed_deferred(VertexId::from(0), VertexId::from(dst), TestEdge(dst))
                .expect("insert directed deferred");
        }
        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(2),
            })
            .expect("maintenance");
        black_box(report.processed_segments());
    })
}
