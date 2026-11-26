//! Labeled `NodeScan` cost on a graph where the scanned label is rare (16 of 10,000 vertices).
//!
//! A labeled scan of an unbound variable reads the label's postings (ADR 0123) and then each hit's
//! vertex row; these benches measure the full executor path for a rare and a common label, and
//! split out the posting read beside the per-vertex row read and label probe that a scan over every
//! vertex id would pay.

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

/// Full executor path for a label carried by almost every vertex: the posting read must not cost
/// more than the per-vertex scan it replaced.
#[bench(raw)]
fn bench_graph_node_scan_common_label_10k() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_rare_label_graph(&store);
    let plan = plan(vec![PlanOp::NodeScan {
        variable: "g".into(),
        label: Some("BenchCommonLabel".into()),
        property_projection: None,
    }]);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("node_scan_common_label_10k");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(
            result.rows.len(),
            (NODE_SCAN_TOTAL - NODE_SCAN_RARE) as usize
        );
        black_box(result.rows.len())
    })
}

/// The scan's steps, each in its own scope: the rare label's posting read (what a labeled scan
/// does), and the per-vertex row read and label-set probe over every id (what it no longer does;
/// the probe remains the check for a scan of an already-bound variable).
#[bench(raw)]
fn bench_graph_node_scan_steps_10k() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_rare_label_graph(&store);
    let label = crate::test_labels::vertex_label_id_for_name("BenchRareLabel");

    canbench_rs::bench_fn(|| {
        let posted = {
            let _scope = canbench_rs::bench_scope("label_postings");
            store.vertices_with_label(label).count()
        };
        assert_eq!(posted, NODE_SCAN_RARE as usize);
        let rows = {
            let _scope = canbench_rs::bench_scope("vertex_row_reads");
            (0..NODE_SCAN_TOTAL)
                .map(VertexId::from)
                .filter(|&id| store.vertex_exists(id))
                .collect::<Vec<_>>()
        };
        let matched = {
            let _scope = canbench_rs::bench_scope("label_checks");
            rows.iter()
                .filter(|&&id| store.vertex_has_label(id, label))
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
