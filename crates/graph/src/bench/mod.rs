//! PocketIC / `canbench` targets for end-to-end GQL query handling (parse, plan, execute).
//!
//! Run from `crates/graph`: `canbench` (see `canbench.yml`).
//!
//! **LabelId pruning checks:** `match_return_noisy` vs `match_return_noisy_multilabel` — same graph
//! size, but multilabel noise forces the label sidecar path on almost every scan.
//! `two_hop_chain` vs `two_hop_chain_mid_junk_out` — same query/plan, but `Mid` has many extra
//! outgoing edges with a different rel label; edge expansion drops them by `LabelId` before row assembly.

use crate::facade::GraphStore;
use crate::facade::mutation_executor::GraphMutationExecutor;
use crate::plan::PlanQueryExecutor;
use canbench_rs::bench;
use gleaph_gql::Value;
use gleaph_gql::ast::Statement;
use gleaph_gql::parser;
use gleaph_gql_planner::build_plan;
use std::collections::BTreeMap;
use std::hint::black_box;

fn empty_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

/// Extra vertices with label `BenchGqlNoiseBlob`: larger [`GraphStore::vertex_count`], so labeled scans sweep more ids.
const BENCH_NOISE_STANDALONE_VERTICES: u32 = 384;

/// For the 2-hop query: extra `BenchGqlTwoHopSource` rows with a `BenchGqlTwoHopRel1` edge into
/// noise only (`BenchGqlNoiseBlob`), so the first expand branches but only the real path matches `Mid`.
const BENCH_TWO_HOP_DECOY_SOURCES: u32 = 96;

/// Outgoing edges from `Mid` with label `BenchGqlMidJunkRel` (not `BenchGqlTwoHopRel2`), to stress
/// expand-side `LabelId` filtering on the second hop.
const BENCH_TWO_HOP_MID_JUNK_OUT_EDGES: u32 = 192;

fn insert_noise_blob_vertices(store: GraphStore, count: u32) {
    for i in 0..count {
        store
            .insert_vertex_named(
                ["BenchGqlNoiseBlob"],
                [("noise_id", Value::Int64(i as i64))],
            )
            .expect("insert noise vertex");
    }
}

/// Same count as [`insert_noise_blob_vertices`], but every noise vertex carries **two** labels so
/// scans use the multi-label sidecar path (no primary-only shortcut).
fn insert_noise_blob_vertices_multilabel(store: GraphStore, count: u32) {
    for i in 0..count {
        store
            .insert_vertex_named(
                ["BenchGqlNoiseBlob", "BenchGqlNoiseExtra"],
                [("noise_id", Value::Int64(i as i64))],
            )
            .expect("insert multilabel noise vertex");
    }
}

fn insert_two_hop_decoys(store: GraphStore, count: u32) {
    for i in 0..count {
        let decoy = store
            .insert_vertex_named(
                ["BenchGqlTwoHopSource"],
                [("name", Value::Text(format!("BenchGqlTwoHopDecoy {i}")))],
            )
            .expect("insert decoy source");
        let sink = store
            .insert_vertex_named(
                ["BenchGqlNoiseBlob"],
                [("decoy_sink", Value::Int64(i as i64))],
            )
            .expect("insert decoy sink");
        store
            .insert_directed_edge_named(
                decoy,
                sink,
                Some("BenchGqlTwoHopRel1"),
                [("hop", Value::Int64(-1))],
            )
            .expect("insert decoy edge");
    }
}

/// Full path used by the canister: parse GQL text, build a physical plan, run it on stable storage.
fn execute_gql_query(
    store: GraphStore,
    gql: &str,
    parameters: &BTreeMap<String, Value>,
) -> crate::plan::PlanQueryResult {
    let program = parser::parse(gql).expect("parse GQL");
    let tx = program.transaction_activity.expect("transaction activity");
    let block = tx.body.expect("statement block");
    let Statement::Query(composite) = &block.first else {
        panic!("expected query statement");
    };
    let plan = build_plan(&composite.left, None).expect("build plan");
    store
        .execute_plan_query(&plan, parameters)
        .expect("execute plan query")
}

/// Simple `MATCH` / `RETURN` (label filter + property read).
#[bench(raw)]
fn bench_graph_gql_parse_plan_execute_match_return() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    store
        .insert_vertex_named(
            ["BenchGqlMatchReturnPerson"],
            [("name", Value::Text("Bench Alice".into()))],
        )
        .expect("insert matching vertex");
    store
        .insert_vertex_named(
            ["BenchGqlMatchReturnOther"],
            [("name", Value::Text("Bench Bob".into()))],
        )
        .expect("insert non-matching vertex");

    let gql = "MATCH (n:BenchGqlMatchReturnPerson) RETURN n.name AS name";
    let params = empty_params();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("gql_parse_plan_execute");
        let result = execute_gql_query(store, black_box(gql), &params);
        black_box(result.rows.len())
    })
}

/// Like [`bench_graph_gql_parse_plan_execute_match_return`], plus many `BenchGqlNoiseBlob` vertices.
#[bench(raw)]
fn bench_graph_gql_parse_plan_execute_match_return_noisy() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    insert_noise_blob_vertices(store, BENCH_NOISE_STANDALONE_VERTICES);

    store
        .insert_vertex_named(
            ["BenchGqlMatchReturnPerson"],
            [("name", Value::Text("Bench Alice".into()))],
        )
        .expect("insert matching vertex");
    store
        .insert_vertex_named(
            ["BenchGqlMatchReturnOther"],
            [("name", Value::Text("Bench Bob".into()))],
        )
        .expect("insert non-matching vertex");

    let gql = "MATCH (n:BenchGqlMatchReturnPerson) RETURN n.name AS name";
    let params = empty_params();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("gql_parse_plan_execute_noisy_scan");
        let result = execute_gql_query(store, black_box(gql), &params);
        black_box(result.rows.len())
    })
}

/// Like [`bench_graph_gql_parse_plan_execute_match_return_noisy`], but noise vertices use two labels
/// (`BenchGqlNoiseBlob` + `BenchGqlNoiseExtra`) so each hit goes through the label sidecar.
#[bench(raw)]
fn bench_graph_gql_parse_plan_execute_match_return_noisy_multilabel() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    insert_noise_blob_vertices_multilabel(store, BENCH_NOISE_STANDALONE_VERTICES);

    store
        .insert_vertex_named(
            ["BenchGqlMatchReturnPerson"],
            [("name", Value::Text("Bench Alice".into()))],
        )
        .expect("insert matching vertex");
    store
        .insert_vertex_named(
            ["BenchGqlMatchReturnOther"],
            [("name", Value::Text("Bench Bob".into()))],
        )
        .expect("insert non-matching vertex");

    let gql = "MATCH (n:BenchGqlMatchReturnPerson) RETURN n.name AS name";
    let params = empty_params();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("gql_parse_plan_execute_noisy_scan_multilabel");
        let result = execute_gql_query(store, black_box(gql), &params);
        black_box(result.rows.len())
    })
}

/// Same pipeline with a `WHERE` predicate (parser, planner, executor filter path).
#[bench(raw)]
fn bench_graph_gql_parse_plan_execute_where_filter() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    store
        .insert_vertex_named(
            ["BenchGqlWherePerson"],
            [
                ("name", Value::Text("Ada".into())),
                ("age", Value::Int64(37)),
            ],
        )
        .expect("insert matching vertex");
    store
        .insert_vertex_named(
            ["BenchGqlWherePerson"],
            [
                ("name", Value::Text("Bob".into())),
                ("age", Value::Int64(12)),
            ],
        )
        .expect("insert non-matching vertex");

    let gql = "MATCH (n:BenchGqlWherePerson) WHERE n.age > 18 RETURN n.name AS name";
    let params = empty_params();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("gql_parse_plan_execute_where");
        let result = execute_gql_query(store, black_box(gql), &params);
        black_box(result.rows.len())
    })
}

/// Like [`bench_graph_gql_parse_plan_execute_where_filter`], plus many `BenchGqlNoiseBlob` vertices.
#[bench(raw)]
fn bench_graph_gql_parse_plan_execute_where_filter_noisy() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    insert_noise_blob_vertices(store, BENCH_NOISE_STANDALONE_VERTICES);

    store
        .insert_vertex_named(
            ["BenchGqlWherePerson"],
            [
                ("name", Value::Text("Ada".into())),
                ("age", Value::Int64(37)),
            ],
        )
        .expect("insert matching vertex");
    store
        .insert_vertex_named(
            ["BenchGqlWherePerson"],
            [
                ("name", Value::Text("Bob".into())),
                ("age", Value::Int64(12)),
            ],
        )
        .expect("insert non-matching vertex");

    let gql = "MATCH (n:BenchGqlWherePerson) WHERE n.age > 18 RETURN n.name AS name";
    let params = empty_params();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("gql_parse_plan_execute_where_noisy");
        let result = execute_gql_query(store, black_box(gql), &params);
        black_box(result.rows.len())
    })
}

/// Open 2-hop path: `Scan` + `Expand` + `Expand` (fixed labels, no var-length / WCOJ).
#[bench(raw)]
fn bench_graph_gql_parse_plan_execute_two_hop_chain() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(
            ["BenchGqlTwoHopSource"],
            [("name", Value::Text("TwoHop Alice".into()))],
        )
        .expect("insert source");
    let b = store
        .insert_vertex_named(
            ["BenchGqlTwoHopMid"],
            [("name", Value::Text("TwoHop Bob".into()))],
        )
        .expect("insert mid");
    let c = store
        .insert_vertex_named(
            ["BenchGqlTwoHopDest"],
            [("name", Value::Text("TwoHop Carol".into()))],
        )
        .expect("insert dest");
    store
        .insert_directed_edge_named(a, b, Some("BenchGqlTwoHopRel1"), [("hop", Value::Int64(1))])
        .expect("insert first hop edge");
    store
        .insert_directed_edge_named(b, c, Some("BenchGqlTwoHopRel2"), [("hop", Value::Int64(2))])
        .expect("insert second hop edge");

    let gql = "MATCH (src:BenchGqlTwoHopSource)-[e1:BenchGqlTwoHopRel1]->(mid:BenchGqlTwoHopMid)-\
               [e2:BenchGqlTwoHopRel2]->(dst:BenchGqlTwoHopDest) \
               RETURN src.name AS s, mid.name AS m, dst.name AS d, e1.hop AS h1, e2.hop AS h2";
    let params = empty_params();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("gql_parse_plan_execute_two_hop");
        let result = execute_gql_query(store, black_box(gql), &params);
        black_box(result.rows.len())
    })
}

/// Same query as [`bench_graph_gql_parse_plan_execute_two_hop_chain`], but `Mid` has many
/// `BenchGqlMidJunkRel` edges into noise — only `BenchGqlTwoHopRel2` to `Dest` should survive the
/// expand’s label filter.
#[bench(raw)]
fn bench_graph_gql_parse_plan_execute_two_hop_chain_mid_junk_out() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(
            ["BenchGqlTwoHopSource"],
            [("name", Value::Text("TwoHop Alice".into()))],
        )
        .expect("insert source");
    let b = store
        .insert_vertex_named(
            ["BenchGqlTwoHopMid"],
            [("name", Value::Text("TwoHop Bob".into()))],
        )
        .expect("insert mid");
    let c = store
        .insert_vertex_named(
            ["BenchGqlTwoHopDest"],
            [("name", Value::Text("TwoHop Carol".into()))],
        )
        .expect("insert dest");
    store
        .insert_directed_edge_named(a, b, Some("BenchGqlTwoHopRel1"), [("hop", Value::Int64(1))])
        .expect("insert first hop edge");
    store
        .insert_directed_edge_named(b, c, Some("BenchGqlTwoHopRel2"), [("hop", Value::Int64(2))])
        .expect("insert second hop edge");

    for i in 0..BENCH_TWO_HOP_MID_JUNK_OUT_EDGES {
        let junk = store
            .insert_vertex_named(["BenchGqlNoiseBlob"], [("junk", Value::Int64(i as i64))])
            .expect("junk target");
        store
            .insert_directed_edge_named(
                b,
                junk,
                Some("BenchGqlMidJunkRel"),
                [("hop", Value::Int64(999))],
            )
            .expect("mid junk out-edge");
    }

    let gql = "MATCH (src:BenchGqlTwoHopSource)-[e1:BenchGqlTwoHopRel1]->(mid:BenchGqlTwoHopMid)-\
               [e2:BenchGqlTwoHopRel2]->(dst:BenchGqlTwoHopDest) \
               RETURN src.name AS s, mid.name AS m, dst.name AS d, e1.hop AS h1, e2.hop AS h2";
    let params = empty_params();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("gql_parse_plan_execute_two_hop_mid_junk_out");
        let result = execute_gql_query(store, black_box(gql), &params);
        black_box(result.rows.len())
    })
}

/// Like [`bench_graph_gql_parse_plan_execute_two_hop_chain`], plus noise vertices, decoy sources,
/// and dead-end `Rel1` edges (still exactly one full `Source→Mid→Dest` chain).
#[bench(raw)]
fn bench_graph_gql_parse_plan_execute_two_hop_chain_noisy() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    insert_noise_blob_vertices(store, BENCH_NOISE_STANDALONE_VERTICES);
    insert_two_hop_decoys(store, BENCH_TWO_HOP_DECOY_SOURCES);

    let a = store
        .insert_vertex_named(
            ["BenchGqlTwoHopSource"],
            [("name", Value::Text("TwoHop Alice".into()))],
        )
        .expect("insert source");
    let b = store
        .insert_vertex_named(
            ["BenchGqlTwoHopMid"],
            [("name", Value::Text("TwoHop Bob".into()))],
        )
        .expect("insert mid");
    let c = store
        .insert_vertex_named(
            ["BenchGqlTwoHopDest"],
            [("name", Value::Text("TwoHop Carol".into()))],
        )
        .expect("insert dest");
    store
        .insert_directed_edge_named(a, b, Some("BenchGqlTwoHopRel1"), [("hop", Value::Int64(1))])
        .expect("insert first hop edge");
    store
        .insert_directed_edge_named(b, c, Some("BenchGqlTwoHopRel2"), [("hop", Value::Int64(2))])
        .expect("insert second hop edge");

    let gql = "MATCH (src:BenchGqlTwoHopSource)-[e1:BenchGqlTwoHopRel1]->(mid:BenchGqlTwoHopMid)-\
               [e2:BenchGqlTwoHopRel2]->(dst:BenchGqlTwoHopDest) \
               RETURN src.name AS s, mid.name AS m, dst.name AS d, e1.hop AS h1, e2.hop AS h2";
    let params = empty_params();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("gql_parse_plan_execute_two_hop_noisy");
        let result = execute_gql_query(store, black_box(gql), &params);
        black_box(result.rows.len())
    })
}
