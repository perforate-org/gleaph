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
            let i = black_box(i);
            let src = (i % 256) as u32;
            let dst = ((i + 1) % 256) as u32;
            graph
                .insert_directed_deferred(
                    VertexId::from(black_box(src)),
                    VertexId::from(black_box(dst)),
                    TestEdge(black_box(dst)),
                )
                .expect("insert directed deferred");
        }
        black_box(graph.maintenance_queue_len());
    })
}

/// Measures deferred undirected insertion across both orientations. This keeps
/// an eye on the most expensive logical insert shape: symmetric adjacency plus
/// deferred maintenance bookkeeping.
#[bench(raw)]
fn bench_lara_deferred_bidirectional_insert_undirected_1024() -> canbench_rs::BenchResult {
    let mut memories = helper::BenchMemoryFactory::new();
    let graph = crate::DeferredBidirectionalLaraGraph::<
        crate::test_support::UndirectedTestEdge,
        crate::Vertex,
        helper::BenchMemory,
    >::new_with_config(
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
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
            let i = black_box(i);
            let src = (i % 256) as u32;
            let dst = ((i + 1) % 256) as u32;
            graph
                .insert_undirected_deferred(
                    VertexId::from(black_box(src)),
                    VertexId::from(black_box(dst)),
                    helper::undirected_edge(black_box(dst)),
                )
                .expect("insert undirected deferred");
        }
        black_box(graph.maintenance_queue_len());
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
            let i = black_box(i);
            let dst = ((i + 1) % 16) as u32;
            graph
                .insert_directed_deferred(
                    VertexId::from(black_box(0u32)),
                    VertexId::from(black_box(dst)),
                    TestEdge(black_box(dst)),
                )
                .expect("insert directed deferred");
        }
        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(2),
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_delete_edge_steps: None,
            })
            .expect("maintenance");
        black_box(report.work.processed_segments);
    })
}
