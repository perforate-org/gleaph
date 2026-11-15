//! Labeled `NodeScan` cost on a graph where the scanned label is rare (16 of 10,000 vertices).
//!
//! The executor visits every vertex id and checks its labels; these benches split that per-vertex
//! cost into the vertex-row read and the label-membership check so the dominant step is visible.

use super::*;
use canbench_rs::bench;

const NODE_SCAN_TOTAL: u32 = 10_000;
const NODE_SCAN_RARE: u32 = 16;

/// `NODE_SCAN_RARE` `BenchRareLabel` vertices and the rest `BenchCommonLabel`, interleaved so the
/// rare label is spread across the id space like real seed data.
fn setup_rare_label_graph(store: &GraphStore) {
    let stride = NODE_SCAN_TOTAL / NODE_SCAN_RARE;
    for i in 0..NODE_SCAN_TOTAL {
        let label = if i % stride == 0 {
            "BenchRareLabel"
        } else {
            "BenchCommonLabel"
        };
        store
            .insert_vertex_named([label], [("name", Value::Text(format!("v{i}")))])
            .expect("insert node-scan bench vertex");
    }
}

/// Full executor path: `NodeScan(label=BenchRareLabel)` over 10,000 vertices.
#[bench(raw)]
fn bench_graph_node_scan_rare_label_10k() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_rare_label_graph(&store);
    let plan = plan(vec![PlanOp::NodeScan {
        variable: "g".into(),
        label: Some("BenchRareLabel".into()),
        property_projection: None,
    }]);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("node_scan_rare_label_10k");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), NODE_SCAN_RARE as usize);
        black_box(result.rows.len())
    })
}

/// The two per-vertex steps of the scan loop, each in its own scope.
#[bench(raw)]
fn bench_graph_node_scan_steps_10k() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_rare_label_graph(&store);
    let label = crate::test_labels::vertex_label_id_for_name("BenchRareLabel");

    canbench_rs::bench_fn(|| {
        let rows = {
            let _scope = canbench_rs::bench_scope("vertex_row_reads");
            (0..NODE_SCAN_TOTAL)
                .filter_map(|raw| {
                    let id = VertexId::from(raw);
                    store.vertex(id).map(|vertex| (id, vertex))
                })
                .collect::<Vec<_>>()
        };
        let matched = {
            let _scope = canbench_rs::bench_scope("label_checks");
            rows.iter()
                .filter(|(id, vertex)| store.vertex_has_label(*id, *vertex, label))
                .count()
        };
        assert_eq!(matched, NODE_SCAN_RARE as usize);
        black_box(matched)
    })
}

/// Plan one GQL DML statement the way the Router does before shipping it to the shard.
fn dml_plan(gql: &str) -> PhysicalPlan {
    let program = gleaph_gql::parser::parse(gql).expect("parse node-scan bench DML");
    let block = program
        .transaction_activity
        .and_then(|tx| tx.body)
        .expect("node-scan bench DML has a statement block");
    gleaph_gql_planner::build_statement_plan(&block.first, None).expect("plan node-scan bench DML")
}

/// One Router-shaped mutation through the shard's canonical segment, in its own scope.
fn bench_match_insert(gql: &'static str, scope: &'static str) -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_rare_label_graph(&store);
    let plan = dml_plan(gql);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        run_canonical_segment(store.clone(), black_box(&plan));
    })
}

/// `MATCH` one rare-label vertex by an unindexed property, then insert one vertex.
#[bench(raw)]
fn bench_graph_match1_insert_10k() -> canbench_rs::BenchResult {
    bench_match_insert(
        "MATCH (g0:BenchRareLabel {name: 'v0'}) INSERT (f:BenchMutFilm {t: 'x'})",
        "match1_insert_10k",
    )
}

/// The same with two disconnected patterns (a leading `CartesianProduct`).
#[bench(raw)]
fn bench_graph_match2_insert_10k() -> canbench_rs::BenchResult {
    bench_match_insert(
        "MATCH (g0:BenchRareLabel {name: 'v0'}), (g1:BenchRareLabel {name: 'v625'}) \
         INSERT (f:BenchMutFilm {t: 'x'})",
        "match2_insert_10k",
    )
}
