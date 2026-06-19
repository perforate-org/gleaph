//! PocketIC / `canbench` targets for graph plan execution (`PhysicalPlan` replay).
//!
//! Run from `crates/graph`: `canbench` (see `canbench.yml`).

#[cfg(feature = "canbench_large")]
mod large;
mod stable_layout;

use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::plan::query::{PlanQueryResult, execute_plan_query, execute_plan_query_bindings};
use canbench_rs::bench;
use gleaph_gql::Value;
use gleaph_gql::ast::{CmpOp, Expr, ExprKind, ObjectName};
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::plan::{
    EdgePayloadPredicate, EdgeVectorMetric, EdgeVectorPredicate, PhysicalPlan, PlanOp,
    ProjectColumn, ScanValue, ShortestMode, ShortestPathCost, VarLenSpec,
};
use gleaph_graph_kernel::entry::{
    EdgeLabelId, EdgePayloadEncoding, EdgePayloadProfile, EdgeWeightProfile, Vertex, WeightEncoding,
};
use ic_stable_lara::VertexId;
use std::collections::BTreeMap;
use std::hint::black_box;

// --- benchmark helpers ---

fn params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn project(expr: Expr, alias: &str) -> ProjectColumn {
    ProjectColumn {
        expr,
        alias: Some(alias.into()),
    }
}

fn var(name: &str) -> Expr {
    Expr::new(ExprKind::Variable(name.to_owned()))
}

fn f32_vector_value(values: &[f32]) -> Value {
    Value::List(values.iter().copied().map(Value::Float32).collect())
}

fn f32_vector_bytes(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn plan(ops: Vec<PlanOp>) -> PhysicalPlan {
    PhysicalPlan::from_ops(ops)
}

fn gleaph_weight_call(edge_var: &str) -> Expr {
    Expr::new(ExprKind::FunctionCall {
        name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
        args: vec![Expr::var(edge_var)],
        distinct: false,
    })
}

fn catalog_edge_label(label_name: &str) -> EdgeLabelId {
    crate::test_labels::edge_label_id_for_name(label_name)
}

fn insert_bench_vertex_named(store: &GraphStore, labels: &[&str]) -> VertexId {
    let label_ids = labels
        .iter()
        .map(|label| crate::test_labels::vertex_label_id_for_name(label))
        .collect::<Vec<_>>();
    let vertex_id = store
        .push_unplaced_vertex_row(Vertex::default())
        .expect("vertex row");
    let vertex = store.vertex(vertex_id).expect("new vertex");
    let vertex = store
        .set_vertex_labels(vertex_id, vertex, label_ids)
        .expect("set labels");
    store.set_vertex(vertex_id, vertex).expect("write vertex");
    vertex_id
}

fn weighted_shortest_plan(
    src_label: &str,
    dst_label: &str,
    edge_label: &str,
    cost_expr: Expr,
    max_hops: u64,
) -> PhysicalPlan {
    weighted_shortest_plan_with_mode(
        src_label,
        dst_label,
        edge_label,
        cost_expr,
        max_hops,
        ShortestMode::AnyShortest,
    )
}

fn weighted_shortest_plan_with_mode(
    src_label: &str,
    dst_label: &str,
    edge_label: &str,
    cost_expr: Expr,
    max_hops: u64,
    mode: ShortestMode,
) -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some(src_label.into()),
            property_projection: None,
        },
        PlanOp::NodeScan {
            variable: "c".into(),
            label: Some(dst_label.into()),
            property_projection: None,
        },
        PlanOp::ShortestPath {
            src: "a".into(),
            dst: "c".into(),
            edge: "e".into(),
            path_var: Some("p".into()),
            emit_edge_binding: false,
            emit_path_binding: true,
            mode,
            direction: EdgeDirection::PointingRight,
            label: Some(edge_label.into()),
            label_expr: None,
            var_len: Some(VarLenSpec {
                min: 1,
                max: Some(max_hops),
            }),
            cost: ShortestPathCost::EdgeCostExpr {
                edge_var: "e".into(),
                expr: cost_expr,
            },
        },
        PlanOp::Project {
            columns: vec![project(var("p"), "p")],
            distinct: false,
        },
    ])
}

fn hop_count_shortest_plan(
    src_label: &str,
    dst_label: &str,
    edge_label: &str,
    max_hops: u64,
) -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some(src_label.into()),
            property_projection: None,
        },
        PlanOp::NodeScan {
            variable: "c".into(),
            label: Some(dst_label.into()),
            property_projection: None,
        },
        PlanOp::ShortestPath {
            src: "a".into(),
            dst: "c".into(),
            edge: "e".into(),
            path_var: Some("p".into()),
            emit_edge_binding: false,
            emit_path_binding: true,
            mode: ShortestMode::AnyShortest,
            direction: EdgeDirection::PointingRight,
            label: Some(edge_label.into()),
            label_expr: None,
            var_len: Some(VarLenSpec {
                min: 1,
                max: Some(max_hops),
            }),
            cost: ShortestPathCost::HopCount,
        },
        PlanOp::Project {
            columns: vec![project(var("p"), "p")],
            distinct: false,
        },
    ])
}

fn execute_shortest_plan(store: &GraphStore, plan: &PhysicalPlan) -> PlanQueryResult {
    pollster::block_on(execute_plan_query(
        store,
        plan,
        &params(),
        None,
        GqlExecutionContext::default(),
    ))
    .expect("shortest path plan")
}

const FRONTIER_BRANCH: usize = 4;
const FRONTIER_DEPTH: u64 = 4;

/// Layered directed graph: one source, branching intermediate layers, one destination.
fn setup_frontier_heap_graph(store: &GraphStore) -> (VertexId, VertexId) {
    let src = store
        .insert_vertex_named(["BenchWspSrc"], Vec::<(&str, Value)>::new())
        .expect("insert src");
    let dst = store
        .insert_vertex_named(["BenchWspDst"], Vec::<(&str, Value)>::new())
        .expect("insert dst");

    let mut prev_layer = vec![src];
    for _ in 0..FRONTIER_DEPTH - 1 {
        let mut next_layer = Vec::new();
        for parent in &prev_layer {
            for _ in 0..FRONTIER_BRANCH {
                let mid = store
                    .insert_vertex_named(["BenchWspMid"], Vec::<(&str, Value)>::new())
                    .expect("insert mid");
                store
                    .insert_directed_edge_named(
                        *parent,
                        mid,
                        Some("BenchWspEdge"),
                        Vec::<(&str, Value)>::new(),
                    )
                    .expect("layer edge");
                next_layer.push(mid);
            }
        }
        prev_layer = next_layer;
    }
    for leaf in prev_layer {
        store
            .insert_directed_edge_named(
                leaf,
                dst,
                Some("BenchWspEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("leaf->dst");
    }
    (src, dst)
}

const CACHE_PREFIX_COUNT: usize = 48;
const CACHE_HUB_OUT_DEGREE: usize = 24;

fn setup_repeated_edge_cost_cache_graph(store: &GraphStore) -> (VertexId, VertexId) {
    let src = store
        .insert_vertex_named(["BenchWspCacheSrc"], Vec::<(&str, Value)>::new())
        .expect("insert src");
    let hub = store
        .insert_vertex_named(["BenchWspHub"], Vec::<(&str, Value)>::new())
        .expect("insert hub");
    let dst = store
        .insert_vertex_named(["BenchWspCacheDst"], Vec::<(&str, Value)>::new())
        .expect("insert dst");

    let label_id = crate::test_labels::edge_label_id_for_name("BenchWspWgtEdge");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );
    let road = catalog_edge_label("BenchWspWgtEdge");

    let mut prefixes = Vec::with_capacity(CACHE_PREFIX_COUNT);
    for _ in 0..CACHE_PREFIX_COUNT {
        prefixes.push(
            store
                .insert_vertex_named(["BenchWspPrefix"], Vec::<(&str, Value)>::new())
                .expect("insert prefix"),
        );
    }
    for (i, &prefix) in prefixes.iter().enumerate() {
        store
            .insert_directed_edge_with_payload_bytes(prefix, hub, Some(road), &1u16.to_le_bytes())
            .unwrap_or_else(|e| panic!("prefix->hub i={i}: {e:?}"));
    }
    for (i, &prefix) in prefixes.iter().enumerate() {
        store
            .insert_directed_edge_with_payload_bytes(
                src,
                prefix,
                Some(road),
                &((i % 10) as u16 + 1).to_le_bytes(),
            )
            .unwrap_or_else(|e| panic!("src->prefix i={i}: {e:?}"));
    }
    for j in 0..CACHE_HUB_OUT_DEGREE {
        let spoke = store
            .insert_vertex_named(["BenchWspSpoke"], Vec::<(&str, Value)>::new())
            .expect("insert spoke");
        store
            .insert_directed_edge_with_payload_bytes(
                hub,
                spoke,
                Some(road),
                &((j % 5) as u16 + 1).to_le_bytes(),
            )
            .expect("hub->spoke");
        store
            .insert_directed_edge_with_payload_bytes(spoke, dst, Some(road), &1u16.to_le_bytes())
            .expect("spoke->dst");
    }

    store
        .finalize_bulk_ingest(&crate::facade::BulkIngestFinalizeSpec {
            forward_vertices: vec![src],
            reverse_vertices: vec![],
        })
        .expect("finalize bulk ingest stands in for production post-ingest finalize");

    (src, dst)
}

// --- benchmarks ---

/// Stresses hop-count shortest-path frontier size on a layered branching DAG.
/// Intended to surface regressions in BFS queue management and path-state reconstruction.
#[bench(raw)]
fn bench_graph_hop_count_shortest_frontier_queue_branch4_depth4() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_frontier_heap_graph(&store);
    let plan =
        hop_count_shortest_plan("BenchWspSrc", "BenchWspDst", "BenchWspEdge", FRONTIER_DEPTH);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("hop_count_shortest_frontier_queue");
        let result = execute_shortest_plan(black_box(&store), black_box(&plan));
        assert_eq!(
            result.rows.len(),
            1,
            "hop-count frontier benchmark should find one path"
        );
        black_box(result.rows.len())
    })
}

/// Converging DAG workload where many prefixes reach the same hub. Intended to measure
/// `AnyShortest` duplicate-vertex pruning for hop-count BFS.
#[bench(raw)]
fn bench_graph_hop_count_shortest_converging_hub_48prefix_24hub_out() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_repeated_edge_cost_cache_graph(&store);
    let plan =
        hop_count_shortest_plan("BenchWspCacheSrc", "BenchWspCacheDst", "BenchWspWgtEdge", 5);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("hop_count_shortest_converging_hub");
        let result = execute_shortest_plan(black_box(&store), black_box(&plan));
        assert_eq!(
            result.rows.len(),
            1,
            "hop-count convergence benchmark should find one path"
        );
        black_box(result.rows.len())
    })
}

/// Stresses weighted shortest-path frontier size on a layered branching DAG with uniform
/// per-hop cost. Intended to surface regressions in `BinaryHeap` queue management when many
/// equal-cost partial paths are alive before the destination is reached.
#[bench(raw)]
fn bench_graph_weighted_shortest_frontier_heap_branch4_depth4() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_frontier_heap_graph(&store);
    let plan = weighted_shortest_plan(
        "BenchWspSrc",
        "BenchWspDst",
        "BenchWspEdge",
        Expr::new(ExprKind::Literal(Value::Int32(1))),
        FRONTIER_DEPTH,
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("weighted_shortest_frontier_heap");
        let result = execute_shortest_plan(black_box(&store), black_box(&plan));
        assert_eq!(
            result.rows.len(),
            1,
            "frontier benchmark should find one path"
        );
        black_box(result.rows.len())
    })
}

/// Same layered DAG as the weighted frontier benchmark, but requests all equal-depth shortest
/// paths. With a positive literal cost, weighted `AllShortest` is equivalent to hop-count shortest.
#[bench(raw)]
fn bench_graph_weighted_all_shortest_literal_frontier_branch4_depth4() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_frontier_heap_graph(&store);
    let plan = weighted_shortest_plan_with_mode(
        "BenchWspSrc",
        "BenchWspDst",
        "BenchWspEdge",
        Expr::new(ExprKind::Literal(Value::Int32(1))),
        FRONTIER_DEPTH,
        ShortestMode::AllShortest,
    );
    let expected_paths = FRONTIER_BRANCH.pow((FRONTIER_DEPTH - 1) as u32);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("weighted_all_shortest_literal_frontier");
        let result = execute_shortest_plan(black_box(&store), black_box(&plan));
        assert_eq!(
            result.rows.len(),
            expected_paths,
            "weighted all-shortest frontier benchmark should find every shortest path"
        );
        black_box(result.rows.len())
    })
}

/// Hub convergence workload where many prefixes reach the same vertex and re-expand identical
/// outgoing edges. Intended to measure hop-cost cache reuse for `GLEAPH.WEIGHT` decode plus
/// expression evaluation.
#[bench(raw)]
fn bench_graph_weighted_shortest_repeated_edge_cost_cache_48prefix_24hub_out()
-> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_repeated_edge_cost_cache_graph(&store);
    let plan = weighted_shortest_plan(
        "BenchWspCacheSrc",
        "BenchWspCacheDst",
        "BenchWspWgtEdge",
        gleaph_weight_call("e"),
        5,
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("weighted_shortest_edge_cost_cache");
        let result = execute_shortest_plan(black_box(&store), black_box(&plan));
        assert_eq!(
            result.rows.len(),
            1,
            "edge-cost cache benchmark should find one path"
        );
        black_box(result.rows.len())
    })
}

const EXPAND_HUB_OUT: u32 = 24;
const EXPAND_PREFIXES: u32 = 48;

fn setup_expand_hub_graph(store: &GraphStore) {
    let _src = store
        .insert_vertex_named(["BenchExpandSrc"], Vec::<(&str, Value)>::new())
        .expect("src");
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for i in 0..EXPAND_PREFIXES {
        let prefix = store
            .insert_vertex_named(
                [format!("BenchExpandPrefix{i}")],
                Vec::<(&str, Value)>::new(),
            )
            .expect("prefix");
        store
            .insert_directed_edge_named(
                prefix,
                hub,
                Some("BenchExpandEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .unwrap_or_else(|e| panic!("prefix->hub i={i}: {e:?}"));
    }
    for i in 0..EXPAND_HUB_OUT {
        let dst = store
            .insert_vertex_named([format!("BenchExpandDst{i}")], Vec::<(&str, Value)>::new())
            .expect("dst");
        store
            .insert_directed_edge_named(
                hub,
                dst,
                Some("BenchExpandEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("hub->dst");
    }
}

fn expand_plan(emit_edge_binding: bool) -> PhysicalPlan {
    expand_plan_for_label("BenchExpandEdge", emit_edge_binding, None)
}

fn expand_plan_for_label(
    edge_label: &str,
    emit_edge_binding: bool,
    indexed_edge_equality: Option<(&str, ScanValue)>,
) -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "h".into(),
            label: Some("BenchExpandHub".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "h".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some(edge_label.into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: indexed_edge_equality
                .map(|(property, value)| (property.into(), value)),
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ])
}

fn execute_expand_plan(store: &GraphStore, plan: &PhysicalPlan) -> PlanQueryResult {
    pollster::block_on(execute_plan_query(
        store,
        plan,
        &params(),
        None,
        GqlExecutionContext::default(),
    ))
    .expect("execute expand plan")
}

fn execute_expand_bindings(store: &GraphStore, plan: &PhysicalPlan) -> usize {
    pollster::block_on(execute_plan_query_bindings(
        store,
        plan,
        &params(),
        None,
        GqlExecutionContext::default(),
    ))
    .expect("execute expand bindings")
    .len()
}

/// Hub fanout expand returning only destination vertices; edge binding is pruned.
#[bench(raw)]
fn bench_graph_expand_hub_return_dst_only() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_hub_graph(&store);
    let plan = expand_plan(false);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_hub_return_dst");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(
            result.rows.len(),
            EXPAND_HUB_OUT as usize,
            "expand benchmark should emit one row per hub outgoing edge"
        );
        black_box(result.rows.len())
    })
}

/// Same hub fanout expand but materializes edge bindings on every output row.
#[bench(raw)]
fn bench_graph_expand_hub_return_edge() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_hub_graph(&store);
    let plan = expand_plan(true);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_hub_return_edge");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), EXPAND_HUB_OUT as usize);
        black_box(result.rows.len())
    })
}

fn expand_filter_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "h".into(),
            label: Some("BenchExpandHub".into()),
            property_projection: None,
        },
        PlanOp::ExpandFilter {
            src: "h".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchExpandEdge".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            dst_filter: vec![Expr::new(ExprKind::Compare {
                left: Box::new(Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable("b".to_owned()))),
                    property: "tag".to_owned(),
                })),
                op: gleaph_gql::ast::CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(1)))),
            })],
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ])
}

fn setup_expand_filter_hub_graph(store: &GraphStore) {
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for i in 0..EXPAND_HUB_OUT {
        let keep = i % 2 == 0;
        let tag = if keep { 1i64 } else { 0i64 };
        let dst = store
            .insert_vertex_named(
                [format!("BenchExpandFilterDst{i}")],
                [("tag", Value::Int64(tag))],
            )
            .expect("dst");
        store
            .insert_directed_edge_named(
                hub,
                dst,
                Some("BenchExpandEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("edge");
    }
}

/// Hub fanout with destination predicate evaluated before row clone.
#[bench(raw)]
fn bench_graph_expand_filter_hub_return_dst_tag_eq() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_filter_hub_graph(&store);
    let plan = expand_filter_plan();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_filter_hub_return_dst");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), EXPAND_HUB_OUT as usize / 2);
        black_box(result.rows.len())
    })
}

fn hop_count_shortest_endpoints_plan(
    src_label: &str,
    dst_label: &str,
    edge_label: &str,
    max_hops: u64,
) -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some(src_label.into()),
            property_projection: None,
        },
        PlanOp::NodeScan {
            variable: "c".into(),
            label: Some(dst_label.into()),
            property_projection: None,
        },
        PlanOp::ShortestPath {
            src: "a".into(),
            dst: "c".into(),
            edge: "e".into(),
            path_var: Some("p".into()),
            emit_edge_binding: false,
            emit_path_binding: false,
            mode: ShortestMode::AnyShortest,
            direction: EdgeDirection::PointingRight,
            label: Some(edge_label.into()),
            label_expr: None,
            var_len: Some(VarLenSpec {
                min: 1,
                max: Some(max_hops),
            }),
            cost: ShortestPathCost::HopCount,
        },
        PlanOp::Project {
            columns: vec![project(var("a"), "a"), project(var("c"), "c")],
            distinct: false,
        },
    ])
}

/// Shortest path returning endpoints only; no edge/path materialization.
#[bench(raw)]
fn bench_graph_hop_shortest_return_endpoints_branch4_depth4() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_frontier_heap_graph(&store);
    let plan = hop_count_shortest_endpoints_plan(
        "BenchWspSrc",
        "BenchWspDst",
        "BenchWspEdge",
        FRONTIER_DEPTH,
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("hop_shortest_return_endpoints");
        let result = execute_shortest_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), 1);
        black_box(result.rows.len())
    })
}

// --- Priority 5 signal benches (paired with existing expand_hub_return_dst_only) ---

const EXPAND_MIXED_LABEL_COUNT: u32 = 10;
const EXPAND_SKEW_NOISE: u32 = 200;
const EXPAND_FILTER_TOTAL: u32 = 240;
const EXPAND_FILTER_PASS: u32 = 24;
const EXPAND_HJ_PREFIXES: u32 = 48;

fn setup_expand_single_label_hub(store: &GraphStore, hub_out: u32, edge_label: &str) {
    let dst = store
        .insert_vertex_named(["BenchScaleDst"], Vec::<(&str, Value)>::new())
        .expect("dst");
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for i in 0..hub_out {
        let _ = i;
        store
            .insert_directed_edge_named(hub, dst, Some(edge_label), Vec::<(&str, Value)>::new())
            .expect("hub->dst");
    }
}

const PAYLOAD_SKEW_EDGE_LABEL: &str = "BenchPayloadSkewRoad";
const PAYLOAD_SKEW_MATCH_WEIGHT: u16 = 7;
const PAYLOAD_SKEW_NOISE_WEIGHT: u16 = 1;
/// Minimum same-label edge count that forces hybrid slab + overflow log on one hub (see ADR 0016).
const PAYLOAD_FIRST_LOG_OVERFLOW_NOISE: u32 = 48;
const PAYLOAD_FIRST_LOG_MATCH_OUT: u32 = 24;

/// Single-label hub with skewed payload values; expand filters by edge payload equality.
fn setup_expand_payload_skewed_graph_scaled(store: &GraphStore, noise: u32, match_out: u32) {
    let label_id = crate::test_labels::edge_label_id_for_name(PAYLOAD_SKEW_EDGE_LABEL);
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::WeightRawU16,
        },
    );
    let noise_dst = store
        .insert_vertex_named(["BenchPayloadSkewNoiseDst"], Vec::<(&str, Value)>::new())
        .expect("noise dst");
    let target_dst = store
        .insert_vertex_named(["BenchPayloadSkewTargetDst"], Vec::<(&str, Value)>::new())
        .expect("target dst");
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for i in 0..noise {
        let _ = i;
        store
            .insert_directed_edge_with_payload_bytes(
                hub,
                noise_dst,
                Some(label_id),
                &PAYLOAD_SKEW_NOISE_WEIGHT.to_le_bytes(),
            )
            .expect("noise edge");
    }
    for i in 0..match_out {
        let _ = i;
        store
            .insert_directed_edge_with_payload_bytes(
                hub,
                target_dst,
                Some(label_id),
                &PAYLOAD_SKEW_MATCH_WEIGHT.to_le_bytes(),
            )
            .expect("target edge");
    }
}

fn expand_payload_predicate_eq_plan(match_weight: u16) -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "h".into(),
            label: Some("BenchExpandHub".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "h".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some(PAYLOAD_SKEW_EDGE_LABEL.into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: Some(EdgePayloadPredicate {
                op: CmpOp::Eq,
                value: ScanValue::Literal(Value::Uint16(match_weight)),
            }),
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ])
}

fn bench_expand_payload_skewed(
    noise: u32,
    match_out: u32,
    scope: &'static str,
) -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_payload_skewed_graph_scaled(&store, noise, match_out);
    let plan = expand_payload_predicate_eq_plan(PAYLOAD_SKEW_MATCH_WEIGHT);
    let expected = match_out as usize;

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), expected);
        black_box(result.rows.len())
    })
}

/// Incoming mirror of [`setup_expand_payload_skewed_graph_scaled`]: `noise`/`match_in` edges point
/// **into** the hub (`src -> hub`), so the predicate expand walks the hub's reverse adjacency.
fn setup_expand_payload_skewed_incoming_graph_scaled(
    store: &GraphStore,
    noise: u32,
    match_in: u32,
) {
    let label_id = crate::test_labels::edge_label_id_for_name(PAYLOAD_SKEW_EDGE_LABEL);
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::WeightRawU16,
        },
    );
    let noise_src = store
        .insert_vertex_named(["BenchPayloadSkewNoiseSrc"], Vec::<(&str, Value)>::new())
        .expect("noise src");
    let target_src = store
        .insert_vertex_named(["BenchPayloadSkewTargetSrc"], Vec::<(&str, Value)>::new())
        .expect("target src");
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for i in 0..noise {
        let _ = i;
        store
            .insert_directed_edge_with_payload_bytes(
                noise_src,
                hub,
                Some(label_id),
                &PAYLOAD_SKEW_NOISE_WEIGHT.to_le_bytes(),
            )
            .expect("noise edge");
    }
    for i in 0..match_in {
        let _ = i;
        store
            .insert_directed_edge_with_payload_bytes(
                target_src,
                hub,
                Some(label_id),
                &PAYLOAD_SKEW_MATCH_WEIGHT.to_le_bytes(),
            )
            .expect("target edge");
    }
}

fn expand_payload_predicate_eq_plan_incoming(match_weight: u16) -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "h".into(),
            label: Some("BenchExpandHub".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "h".into(),
            edge: "e".into(),
            dst: "a".into(),
            direction: EdgeDirection::PointingLeft,
            label: Some(PAYLOAD_SKEW_EDGE_LABEL.into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: Some(EdgePayloadPredicate {
                op: CmpOp::Eq,
                value: ScanValue::Literal(Value::Uint16(match_weight)),
            }),
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(var("a"), "a")],
            distinct: false,
        },
    ])
}

fn bench_expand_payload_skewed_incoming(
    noise: u32,
    match_in: u32,
    scope: &'static str,
) -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_payload_skewed_incoming_graph_scaled(&store, noise, match_in);
    let plan = expand_payload_predicate_eq_plan_incoming(PAYLOAD_SKEW_MATCH_WEIGHT);
    let expected = match_in as usize;

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), expected);
        black_box(result.rows.len())
    })
}

/// Parallel edges on a low-vertex skewed hub (shared noise/target dst); paired indexed vs CSR benches.
fn setup_expand_skewed_noise_graph_scaled(store: &GraphStore, noise: u32, match_out: u32) {
    let noise_dst = store
        .insert_vertex_named(["BenchSkewNoiseDst"], Vec::<(&str, Value)>::new())
        .expect("noise dst");
    let target_dst = store
        .insert_vertex_named(["BenchSkewTargetDst"], Vec::<(&str, Value)>::new())
        .expect("target dst");
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for i in 0..noise {
        let _ = i;
        store
            .insert_directed_edge_named(
                hub,
                noise_dst,
                Some("BenchExpandNoise"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("noise edge");
    }
    for i in 0..match_out {
        let _ = i;
        store
            .insert_directed_edge_named(
                hub,
                target_dst,
                Some("BenchExpandTarget"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("target edge");
    }
}

/// Parallel edges per label on a shared dst; paired indexed vs CSR benches.
fn setup_expand_mixed_label_hub_graph_scaled(
    store: &GraphStore,
    label_count: u32,
    edges_per_label: u32,
) -> String {
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    let dst = store
        .insert_vertex_named(["BenchMixedDst"], Vec::<(&str, Value)>::new())
        .expect("dst");
    let target_label = "BenchExpandLbl0".to_owned();
    for label_idx in 0..label_count {
        let label = format!("BenchExpandLbl{label_idx}");
        for i in 0..edges_per_label {
            let _ = i;
            store
                .insert_directed_edge_named(hub, dst, Some(&label), Vec::<(&str, Value)>::new())
                .expect("edge");
        }
    }
    target_label
}

/// Hub with 10 labels × 24 edges; expand one label only (24 rows). Control: `expand_hub_return_dst_only`.
#[bench(raw)]
fn bench_graph_expand_mixed_label_hub_240scan_24match() -> canbench_rs::BenchResult {
    bench_expand_mixed_label_hub(
        EXPAND_MIXED_LABEL_COUNT,
        EXPAND_HUB_OUT,
        "expand_mixed_label_hub_240scan_24match",
    )
}

/// 200 noise edges + 24 target-label edges; expand target label only.
#[bench(raw)]
fn bench_graph_expand_skewed_noise_200a_24b() -> canbench_rs::BenchResult {
    bench_expand_skewed_noise(
        EXPAND_SKEW_NOISE,
        EXPAND_HUB_OUT,
        "expand_skewed_noise_200a_24b",
    )
}

fn setup_expand_deep_row_graph(store: &GraphStore) {
    let src = store
        .insert_vertex_named(["BenchDeepSrc"], Vec::<(&str, Value)>::new())
        .expect("src");
    let mid = store
        .insert_vertex_named(["BenchDeepMid"], Vec::<(&str, Value)>::new())
        .expect("mid");
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    store
        .insert_directed_edge_named(src, mid, Some("BenchDeepEdge"), Vec::<(&str, Value)>::new())
        .expect("src->mid");
    store
        .insert_directed_edge_named(mid, hub, Some("BenchDeepEdge"), Vec::<(&str, Value)>::new())
        .expect("mid->hub");
    for i in 0..EXPAND_HUB_OUT {
        let dst = store
            .insert_vertex_named([format!("BenchDeepDst{i}")], Vec::<(&str, Value)>::new())
            .expect("dst");
        store
            .insert_directed_edge_named(
                hub,
                dst,
                Some("BenchExpandEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("hub->dst");
    }
}

fn expand_deep_row_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("BenchDeepSrc".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e1".into(),
            dst: "mid".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchDeepEdge".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Expand {
            src: "mid".into(),
            edge: "e2".into(),
            dst: "h".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchDeepEdge".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Expand {
            src: "h".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchExpandEdge".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ])
}

/// Two-hop prefix chain before hub fanout; row carries prior bindings at final expand. Control: `expand_hub_return_dst_only`.
#[bench(raw)]
fn bench_graph_expand_deep_row_hub_24out() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_deep_row_graph(&store);
    let plan = expand_deep_row_plan();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_deep_row_hub_24out");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), EXPAND_HUB_OUT as usize);
        black_box(result.rows.len())
    })
}

fn setup_expand_filter_10pct_graph(store: &GraphStore) {
    let mut dsts = Vec::with_capacity(EXPAND_FILTER_TOTAL as usize);
    for i in 0..EXPAND_FILTER_TOTAL {
        let pass = i < EXPAND_FILTER_PASS;
        let tag = if pass { 1i64 } else { 0i64 };
        dsts.push(
            store
                .insert_vertex_named(
                    [format!("BenchFilter10Dst{i}")],
                    [("tag", Value::Int64(tag))],
                )
                .expect("dst"),
        );
    }
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for dst in dsts {
        store
            .insert_directed_edge_named(
                hub,
                dst,
                Some("BenchExpandEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("edge");
    }
}

fn expand_filter_10pct_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "h".into(),
            label: Some("BenchExpandHub".into()),
            property_projection: None,
        },
        PlanOp::ExpandFilter {
            src: "h".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchExpandEdge".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            dst_filter: vec![Expr::new(ExprKind::Compare {
                left: Box::new(Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable("b".to_owned()))),
                    property: "tag".to_owned(),
                })),
                op: gleaph_gql::ast::CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(1)))),
            })],
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ])
}

/// 240 incident edges, 10% pass dst filter (24 rows). Pair with `expand_filter_hub_return_dst_tag_eq` (50%).
#[bench(raw)]
fn bench_graph_expand_filter_10pct_pass() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_filter_10pct_graph(&store);
    let plan = expand_filter_10pct_plan();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_filter_10pct_pass");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), EXPAND_FILTER_PASS as usize);
        black_box(result.rows.len())
    })
}

fn setup_expand_hash_join_graph(store: &GraphStore) {
    let mut prefixes = Vec::with_capacity(EXPAND_HJ_PREFIXES as usize);
    for _ in 0..EXPAND_HJ_PREFIXES {
        prefixes.push(
            store
                .insert_vertex_named(["BenchHjPrefix"], Vec::<(&str, Value)>::new())
                .expect("prefix"),
        );
    }
    let mut dsts = Vec::with_capacity(EXPAND_HUB_OUT as usize);
    for i in 0..EXPAND_HUB_OUT {
        dsts.push(
            store
                .insert_vertex_named([format!("BenchHjDst{i}")], Vec::<(&str, Value)>::new())
                .expect("dst"),
        );
    }
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for prefix in prefixes {
        store
            .insert_directed_edge_named(
                prefix,
                hub,
                Some("BenchHjToHub"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("prefix->hub");
    }
    for dst in dsts {
        store
            .insert_directed_edge_named(
                hub,
                dst,
                Some("BenchExpandEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("hub->dst");
    }
}

fn expand_hash_join_then_expand_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::HashJoin {
            left: vec![
                PlanOp::NodeScan {
                    variable: "p".into(),
                    label: Some("BenchHjPrefix".into()),
                    property_projection: None,
                },
                PlanOp::Expand {
                    src: "p".into(),
                    edge: "e0".into(),
                    dst: "h".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("BenchHjToHub".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_payload_predicate: None,
                    edge_vector_predicate: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                    hop_aux_binding: None,
                    emit_edge_binding: false,
                    near_group_var: None,
                    far_group_var: None,
                    path_var: None,
                    emit_path_binding: false,
                },
            ],
            right: vec![
                PlanOp::NodeScan {
                    variable: "h".into(),
                    label: Some("BenchExpandHub".into()),
                    property_projection: None,
                },
                PlanOp::Expand {
                    src: "h".into(),
                    edge: "e".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("BenchExpandEdge".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_payload_predicate: None,
                    edge_vector_predicate: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                    hop_aux_binding: None,
                    emit_edge_binding: false,
                    near_group_var: None,
                    far_group_var: None,
                    path_var: None,
                    emit_path_binding: false,
                },
            ],
            join_keys: vec!["h".into()],
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ])
}

/// HashJoin inflates row width before hub fanout expand (48×24 rows).
#[bench(raw)]
fn bench_graph_expand_hash_join_then_expand_48x24() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_hash_join_graph(&store);
    let plan = expand_hash_join_then_expand_plan();
    let expected = (EXPAND_HJ_PREFIXES * EXPAND_HUB_OUT) as usize;

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_hash_join_then_expand_48x24");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), expected);
        black_box(result.rows.len())
    })
}

fn setup_expand_indexed_eq_graph(store: &GraphStore) {
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for i in 0..EXPAND_FILTER_TOTAL {
        let match_weight = i < EXPAND_FILTER_PASS;
        let weight = if match_weight { 5i64 } else { 9i64 };
        let dst = store
            .insert_vertex_named([format!("BenchIdxEqDst{i}")], Vec::<(&str, Value)>::new())
            .expect("dst");
        store
            .insert_directed_edge_named(
                hub,
                dst,
                Some("BenchExpandEdge"),
                [("weight", Value::Int64(weight))],
            )
            .expect("edge");
    }
}

const EXPAND_VECTOR_DIMS: usize = 16;
const EXPAND_VECTOR_TOTAL: u32 = 24;
const EXPAND_VECTOR_PASS: u32 = 8;

#[derive(Clone, Copy)]
struct ExpandVectorGraphScale<'a> {
    hub_label: &'a str,
    edge_label: &'a str,
    total: u32,
    pass: u32,
    dims: usize,
    edges_per_hub: u32,
}

fn setup_expand_vector_graph(store: &GraphStore) {
    setup_expand_vector_graph_scaled(
        store,
        "BenchExpandHub",
        "BenchVectorEdge",
        EXPAND_VECTOR_TOTAL,
        EXPAND_VECTOR_PASS,
    );
}

fn setup_expand_vector_graph_scaled(
    store: &GraphStore,
    hub_label: &str,
    edge_label: &str,
    total: u32,
    pass: u32,
) {
    setup_expand_vector_graph_with_scale(
        store,
        ExpandVectorGraphScale {
            hub_label,
            edge_label,
            total,
            pass,
            dims: EXPAND_VECTOR_DIMS,
            edges_per_hub: total,
        },
    );
}

fn setup_expand_vector_graph_with_scale(store: &GraphStore, scale: ExpandVectorGraphScale<'_>) {
    assert!(scale.edges_per_hub > 0, "edges_per_hub must be non-zero");
    assert!(scale.pass <= scale.total, "pass must not exceed total");
    assert!(scale.dims > 0, "vector dims must be non-zero");
    assert!(
        scale.dims * std::mem::size_of::<f32>() <= 64,
        "W64 vector profile can hold at most 64 bytes"
    );
    let label_id = crate::test_labels::edge_label_id_for_name(scale.edge_label);
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: 64,
            encoding: EdgePayloadEncoding::VectorF32 {
                dims: scale.dims as u16,
            },
        },
    );

    let near = vec![1.0; scale.dims];
    let far = vec![9.0; scale.dims];
    let near_bytes = f32_vector_bytes(&near);
    let far_bytes = f32_vector_bytes(&far);
    let mut hub = insert_bench_vertex_named(store, &[scale.hub_label]);
    for i in 0..scale.total {
        if i > 0 && i % scale.edges_per_hub == 0 {
            hub = insert_bench_vertex_named(store, &[scale.hub_label]);
        }
        let dst = insert_bench_vertex_named(store, &[]);
        let bytes = if i < scale.pass {
            near_bytes.as_slice()
        } else {
            far_bytes.as_slice()
        };
        store
            .insert_directed_edge_with_payload_bytes(hub, dst, Some(label_id), bytes)
            .expect("edge");
    }
}

fn expand_vector_plan(
    hub_label: &str,
    edge_label: &str,
    metric: EdgeVectorMetric,
    op: CmpOp,
    threshold: f32,
    query: &[f32],
) -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "h".into(),
            label: Some(hub_label.into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "h".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some(edge_label.into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: Some(EdgeVectorPredicate {
                metric,
                query: ScanValue::Literal(f32_vector_value(query)),
                op,
                threshold: ScanValue::Literal(Value::Float32(threshold)),
            }),
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ])
}

fn expand_vector_bindings_plan(
    hub_label: &str,
    edge_label: &str,
    metric: EdgeVectorMetric,
    op: CmpOp,
    threshold: f32,
    query: &[f32],
) -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "h".into(),
            label: Some(hub_label.into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "h".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some(edge_label.into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: Some(EdgeVectorPredicate {
                metric,
                query: ScanValue::Literal(f32_vector_value(query)),
                op,
                threshold: ScanValue::Literal(Value::Float32(threshold)),
            }),
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
    ])
}

// --- Large-hub label-adjacency benches (paired single-label controls) ---

const EXPAND_HUB_OUT_M: u32 = 100;
const EXPAND_SKEW_NOISE_M: u32 = 2_000;
const EXPAND_MIXED_LABEL_COUNT_M: u32 = 20;
const EXPAND_EDGES_PER_LABEL_M: u32 = 100;

const EXPAND_HUB_OUT_L: u32 = 500;
const EXPAND_SKEW_NOISE_L: u32 = 9_500;
const EXPAND_MIXED_LABEL_COUNT_L: u32 = 20;
const EXPAND_EDGES_PER_LABEL_L: u32 = 500;

const EXPAND_HUB_OUT_XL: u32 = 1_000;
const EXPAND_SKEW_NOISE_XL: u32 = 49_000;
const EXPAND_MIXED_LABEL_COUNT_XL: u32 = 50;
const EXPAND_EDGES_PER_LABEL_XL: u32 = 1_000;

fn bench_expand_hub_control(hub_out: u32, scope: &'static str) -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_single_label_hub(&store, hub_out, "BenchExpandEdge");
    let plan = expand_plan(false);
    let expected = hub_out as usize;

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), expected);
        black_box(result.rows.len())
    })
}

fn bench_expand_skewed_noise(
    noise: u32,
    match_out: u32,
    scope: &'static str,
) -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_skewed_noise_graph_scaled(&store, noise, match_out);
    let plan = expand_plan_for_label("BenchExpandTarget", false, None);
    let expected = match_out as usize;

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), expected);
        black_box(result.rows.len())
    })
}

fn bench_expand_mixed_label_hub(
    label_count: u32,
    edges_per_label: u32,
    scope: &'static str,
) -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let target_label =
        setup_expand_mixed_label_hub_graph_scaled(&store, label_count, edges_per_label);
    let plan = expand_plan_for_label(&target_label, false, None);
    let expected = edges_per_label as usize;

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), expected);
        black_box(result.rows.len())
    })
}

/// Single-label hub, 100 out-edges (control for scaled 5a benches).
#[bench(raw)]
fn bench_graph_expand_hub_return_dst_100_only() -> canbench_rs::BenchResult {
    bench_expand_hub_control(EXPAND_HUB_OUT_M, "expand_hub_return_dst_100")
}

/// 2_000 noise + 100 target-label edges; expand target label only.
#[bench(raw)]
fn bench_graph_expand_skewed_noise_2k_a_100b() -> canbench_rs::BenchResult {
    bench_expand_skewed_noise(
        EXPAND_SKEW_NOISE_M,
        EXPAND_HUB_OUT_M,
        "expand_skewed_noise_2k_a_100b",
    )
}

/// 20 labels × 100 edges; expand one label (100 rows). Control: `expand_hub_return_dst_100_only`.
#[bench(raw)]
fn bench_graph_expand_mixed_label_hub_2kscan_100match() -> canbench_rs::BenchResult {
    bench_expand_mixed_label_hub(
        EXPAND_MIXED_LABEL_COUNT_M,
        EXPAND_EDGES_PER_LABEL_M,
        "expand_mixed_label_hub_2kscan_100match",
    )
}

/// Single-label hub, 500 out-edges (control for scaled 5a benches).
#[bench(raw)]
fn bench_graph_expand_hub_return_dst_500_only() -> canbench_rs::BenchResult {
    bench_expand_hub_control(EXPAND_HUB_OUT_L, "expand_hub_return_dst_500")
}

/// 9_500 noise + 500 target-label edges (10_500 incident); expand target label only.
#[bench(raw)]
fn bench_graph_expand_skewed_noise_10k_a_500b() -> canbench_rs::BenchResult {
    bench_expand_skewed_noise(
        EXPAND_SKEW_NOISE_L,
        EXPAND_HUB_OUT_L,
        "expand_skewed_noise_10k_a_500b",
    )
}

/// 20 labels × 500 edges (10_000 incident); expand one label (500 rows).
#[bench(raw)]
fn bench_graph_expand_mixed_label_hub_10kscan_500match() -> canbench_rs::BenchResult {
    bench_expand_mixed_label_hub(
        EXPAND_MIXED_LABEL_COUNT_L,
        EXPAND_EDGES_PER_LABEL_L,
        "expand_mixed_label_hub_10kscan_500match",
    )
}

/// Single-label hub, 1_000 out-edges (control for 50kscan scaled 5a benches).
#[bench(raw)]
fn bench_graph_expand_hub_return_dst_1k_only() -> canbench_rs::BenchResult {
    bench_expand_hub_control(EXPAND_HUB_OUT_XL, "expand_hub_return_dst_1k")
}

/// 49_000 noise + 1_000 target-label edges (50_000 incident); expand target label only.
#[bench(raw)]
fn bench_graph_expand_skewed_noise_50k_a_1k_b() -> canbench_rs::BenchResult {
    bench_expand_skewed_noise(
        EXPAND_SKEW_NOISE_XL,
        EXPAND_HUB_OUT_XL,
        "expand_skewed_noise_50k_a_1k_b",
    )
}

/// 200 noise + 24 matching payload edges on one label; edge payload `Eq` predicate expand.
#[bench(raw)]
fn bench_graph_expand_payload_skewed_200a_24b() -> canbench_rs::BenchResult {
    bench_expand_payload_skewed(
        EXPAND_SKEW_NOISE,
        EXPAND_HUB_OUT,
        "expand_payload_skewed_200a_24b",
    )
}

/// 2_000 noise + 100 matching payload edges; edge payload `Eq` predicate expand.
#[bench(raw)]
fn bench_graph_expand_payload_skewed_2k_a_100b() -> canbench_rs::BenchResult {
    bench_expand_payload_skewed(
        EXPAND_SKEW_NOISE_M,
        EXPAND_HUB_OUT_M,
        "expand_payload_skewed_2k_a_100b",
    )
}

/// ADR 0016 / M6: 48 noise + 24 payload matches on one overflow-log hub; payload-first selective expand.
#[bench(raw)]
fn bench_graph_payload_first_log_backed_selective_match() -> canbench_rs::BenchResult {
    bench_expand_payload_skewed(
        PAYLOAD_FIRST_LOG_OVERFLOW_NOISE,
        PAYLOAD_FIRST_LOG_MATCH_OUT,
        "payload_first_log_backed_selective_match",
    )
}

/// Incoming mirror of `payload_first_log_backed_selective_match`: 48 noise + 24 payload matches
/// point into one overflow-log hub; `PointingLeft` payload-first expand reuses the reverse phase-1
/// hybrid replay for phase-2 slot reads (symmetry with the outgoing path).
#[bench(raw)]
fn bench_graph_payload_first_incoming_log_backed_selective_match() -> canbench_rs::BenchResult {
    bench_expand_payload_skewed_incoming(
        PAYLOAD_FIRST_LOG_OVERFLOW_NOISE,
        PAYLOAD_FIRST_LOG_MATCH_OUT,
        "payload_first_incoming_log_backed_selective_match",
    )
}

/// Profiles 50kscan graph construction only (scope breakdown; no query execution).
#[bench(raw)]
fn bench_graph_profile_setup_50kscan() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("graph_setup_50kscan_total");
        std::hint::black_box(setup_expand_mixed_label_hub_graph_scaled(
            &store,
            EXPAND_MIXED_LABEL_COUNT_XL,
            EXPAND_EDGES_PER_LABEL_XL,
        ));
    })
}

/// 50 labels × 1_000 edges (50_000 incident); expand one label (1_000 rows).
#[bench(raw)]
fn bench_graph_expand_mixed_label_hub_50kscan_1kmatch() -> canbench_rs::BenchResult {
    bench_expand_mixed_label_hub(
        EXPAND_MIXED_LABEL_COUNT_XL,
        EXPAND_EDGES_PER_LABEL_XL,
        "expand_mixed_label_hub_50kscan_1kmatch",
    )
}

/// 240 incident edges; indexed equality selects 24. Compare against mixed-label scan for P4 vs 5a priority.
#[bench(raw)]
fn bench_graph_expand_indexed_eq_selective_24match() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_indexed_eq_graph(&store);
    let plan = expand_plan_for_label(
        "BenchExpandEdge",
        false,
        Some(("weight", ScanValue::Literal(Value::Int64(5)))),
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_indexed_eq_selective_24match");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), EXPAND_FILTER_PASS as usize);
        black_box(result.rows.len())
    })
}

/// 24 fixed-label vector edge payloads; L2 threshold selects 8 rows via SIMD vector scoring.
#[bench(raw)]
fn bench_graph_expand_vector_l2_24scan_8match() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_vector_graph(&store);
    let query = vec![1.0; EXPAND_VECTOR_DIMS];
    let plan = expand_vector_plan(
        "BenchExpandHub",
        "BenchVectorEdge",
        EdgeVectorMetric::L2Squared,
        CmpOp::Le,
        4.0,
        &query,
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_vector_l2_24scan_8match");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), EXPAND_VECTOR_PASS as usize);
        black_box(result.rows.len())
    })
}

/// 24 fixed-label vector edge payloads; DOT threshold selects 8 rows via SIMD vector scoring.
#[bench(raw)]
fn bench_graph_expand_vector_dot_24scan_8match() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_vector_graph(&store);
    let query = vec![-1.0; EXPAND_VECTOR_DIMS];
    let threshold = -(EXPAND_VECTOR_DIMS as f32) - 4.0;
    let plan = expand_vector_plan(
        "BenchExpandHub",
        "BenchVectorEdge",
        EdgeVectorMetric::Dot,
        CmpOp::Ge,
        threshold,
        &query,
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_vector_dot_24scan_8match");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), EXPAND_VECTOR_PASS as usize);
        black_box(result.rows.len())
    })
}

/// Exercises [`gleaph_gql_ic::decode_gql_params_blob`] (graph canister param path) for canbench.
#[bench(raw)]
fn bench_graph_gql_ic_params_blob_decode() -> canbench_rs::BenchResult {
    use candid::Principal;
    use gleaph_gql_ic::{encode_gql_params_blob, principal_to_value};

    let p = Principal::from_text("aaaaa-aa").expect("management id");
    let blob = encode_gql_params_blob(vec![
        ("limit".into(), Value::Int64(100)),
        ("name".into(), Value::Text("neo".into())),
        ("who".into(), principal_to_value(p)),
        (
            "meta".into(),
            Value::Record(vec![("k".into(), Value::Null)]),
        ),
    ])
    .expect("encode");

    canbench_rs::bench_fn(|| {
        let pmap = crate::canister::handlers::decode_gql_param_map(black_box(blob.clone()))
            .expect("decode");
        black_box(pmap.len())
    })
}

// --- Delete benches (ADR 0021 resumable tombstone-first DETACH DELETE) ---

const DELETE_HUB_DEGREE: u32 = 24;

/// Hub with `in_degree` payload-free directed in-edges (`n -> hub`) from distinct
/// sources. This is the reverse-adjacency purge path fixed in ADR 0021.
fn setup_delete_hub_in_edges(store: &GraphStore, in_degree: u32) -> VertexId {
    let hub = store.insert_vertex().expect("hub");
    for _ in 0..in_degree {
        let n = store.insert_vertex().expect("in neighbor");
        store.insert_directed_edge(n, hub, None).expect("n->hub");
    }
    hub
}

/// Hub with `out_degree` payload-free directed out-edges (`hub -> n`) to distinct
/// destinations (forward-adjacency purge path).
fn setup_delete_hub_out_edges(store: &GraphStore, out_degree: u32) -> VertexId {
    let hub = store.insert_vertex().expect("hub");
    for _ in 0..out_degree {
        let n = store.insert_vertex().expect("out neighbor");
        store.insert_directed_edge(hub, n, None).expect("hub->n");
    }
    hub
}

/// Detach-delete of a hub fed by 24 payload-free in-edges; the measured call
/// tombstones the row and drains the incident-edge purge (ADR 0021 Stage 2).
#[bench(raw)]
fn bench_graph_detach_delete_hub_in_edges_24() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let hub = setup_delete_hub_in_edges(&store, DELETE_HUB_DEGREE);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("detach_delete_hub_in_edges_24");
        store
            .detach_delete_vertex(black_box(hub))
            .expect("detach delete hub");
        assert!(!store.is_vertex_live(hub), "hub tombstoned after detach");
    })
}

/// Detach-delete of a hub with 24 out-edges (forward-adjacency purge path).
#[bench(raw)]
fn bench_graph_detach_delete_hub_out_edges_24() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let hub = setup_delete_hub_out_edges(&store, DELETE_HUB_DEGREE);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("detach_delete_hub_out_edges_24");
        store
            .detach_delete_vertex(black_box(hub))
            .expect("detach delete hub");
        assert!(!store.is_vertex_live(hub), "hub tombstoned after detach");
    })
}

/// Baseline: delete a detached vertex (no incident edges); isolates the row-removal
/// and sidecar-clear cost without any edge-purge work.
#[bench(raw)]
fn bench_graph_delete_detached_vertex() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let vertex = store.insert_vertex().expect("vertex");

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("delete_detached_vertex");
        store
            .delete_vertex(black_box(vertex))
            .expect("delete detached vertex");
        assert!(
            !store.is_vertex_live(vertex),
            "vertex tombstoned after delete"
        );
    })
}

#[cfg(test)]
mod bench_setup_tests {
    use super::*;
    use ic_stable_lara::CsrEdge;

    /// Installs the `BenchWspWgtEdge` weight payload profile (`RawU16`, 2 bytes) in
    /// the process-global test registry. The probe tests below build partial
    /// converging-hub fixtures directly, so they cannot rely on
    /// [`setup_repeated_edge_cost_cache_graph`] having installed it first.
    fn install_bench_wsp_wgt_profile() {
        let label_id = crate::test_labels::edge_label_id_for_name("BenchWspWgtEdge");
        crate::test_labels::install_test_edge_payload_profile(
            label_id,
            EdgePayloadProfile::from(EdgeWeightProfile {
                encoding: WeightEncoding::RawU16,
            }),
        );
    }

    #[test]
    fn expand_hub_graph_two_prefix_named_edges_native() {
        let store = GraphStore::new();
        let hub = store
            .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
            .expect("hub");
        for i in 0..2u32 {
            let prefix = store
                .insert_vertex_named(
                    [format!("BenchExpandPrefix{i}")],
                    Vec::<(&str, Value)>::new(),
                )
                .expect("prefix");
            store
                .insert_directed_edge_named(
                    prefix,
                    hub,
                    Some("BenchExpandEdge"),
                    Vec::<(&str, Value)>::new(),
                )
                .unwrap_or_else(|e| panic!("prefix->hub i={i}: {e:?}"));
        }
    }

    #[test]
    fn expand_filter_10pct_pass_setup_and_execute() {
        let store = GraphStore::new();
        setup_expand_filter_10pct_graph(&store);
        let result = execute_expand_plan(&store, &expand_filter_10pct_plan());
        assert_eq!(result.rows.len(), EXPAND_FILTER_PASS as usize);
    }

    #[test]
    fn expand_hash_join_then_expand_48x24_setup_and_execute() {
        let store = GraphStore::new();
        setup_expand_hash_join_graph(&store);
        let expected = (EXPAND_HJ_PREFIXES * EXPAND_HUB_OUT) as usize;
        let result = execute_expand_plan(&store, &expand_hash_join_then_expand_plan());
        assert_eq!(result.rows.len(), expected);
    }

    #[test]
    fn expand_mixed_label_hub_10kscan_500match_setup() {
        setup_expand_mixed_label_hub_graph_scaled(
            &GraphStore::new(),
            EXPAND_MIXED_LABEL_COUNT_L,
            EXPAND_EDGES_PER_LABEL_L,
        );
    }

    #[test]
    fn hop_count_shortest_converging_hub_setup() {
        setup_repeated_edge_cost_cache_graph(&GraphStore::new());
    }

    #[test]
    fn repeated_edge_cost_cache_payload_batch_path_on_hot_vertices() {
        use ic_stable_lara::{OutEdgeOrder, labeled::LabeledEdgePayloadBatchScratch};

        let store = GraphStore::new();
        let start = u32::from(store.vertex_count());
        let (src, _dst) = setup_repeated_edge_cost_cache_graph(&store);
        let end = u32::from(store.vertex_count());
        let road = catalog_edge_label("BenchWspWgtEdge");
        let hub_label = crate::test_labels::vertex_label_id_for_name("BenchWspHub");

        let mut hub = None;
        for raw in start..end {
            let vid = VertexId::from(raw);
            let vertex = store.vertex(vid).expect("vertex");
            if store.vertex_has_label(vid, vertex, hub_label) {
                hub = Some(vid);
            }
        }
        let hub = hub.expect("hub");

        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        let mut hub_dense = None;
        store
            .visit_directed_out_edge_payload_batches_for_label(
                hub,
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| hub_dense = Some(batch.dense),
            )
            .expect("hub payload batches");

        let mut src_dense = None;
        store
            .visit_directed_out_edge_payload_batches_for_label(
                src,
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| src_dense = Some(batch.dense),
            )
            .expect("src payload batches");

        assert_eq!(
            hub_dense,
            Some(true),
            "hub bucket (24 live edges) should stay dense-eligible"
        );
        assert_eq!(
            src_dense,
            Some(true),
            "src bucket should be dense-eligible after setup-time overflow reclaim"
        );
    }

    #[test]
    fn repeated_edge_cost_cache_setup_preserves_full_topology() {
        let store = GraphStore::new();
        let start = u32::from(store.vertex_count());
        let (src, dst) = setup_repeated_edge_cost_cache_graph(&store);
        let end = u32::from(store.vertex_count());

        assert_eq!(u32::from(src), start);
        assert_eq!(u32::from(dst), start + 2);

        let prefix_label = crate::test_labels::vertex_label_id_for_name("BenchWspPrefix");
        let hub_label = crate::test_labels::vertex_label_id_for_name("BenchWspHub");
        let spoke_label = crate::test_labels::vertex_label_id_for_name("BenchWspSpoke");

        let mut hub = None;
        let mut prefixes = Vec::new();
        let mut spokes = Vec::new();
        for raw in start..end {
            let vid = VertexId::from(raw);
            let vertex = store.vertex(vid).expect("vertex");
            if store.vertex_has_label(vid, vertex, prefix_label) {
                prefixes.push(vid);
            } else if store.vertex_has_label(vid, vertex, hub_label) {
                hub = Some(vid);
            } else if store.vertex_has_label(vid, vertex, spoke_label) {
                spokes.push(vid);
            }
        }
        let hub = hub.expect("hub");

        assert_eq!(prefixes.len(), CACHE_PREFIX_COUNT);
        assert_eq!(spokes.len(), CACHE_HUB_OUT_DEGREE);
        assert_eq!(
            store.directed_out_edges(src).expect("src out").len(),
            CACHE_PREFIX_COUNT
        );
        assert_eq!(
            store.directed_out_edges(hub).expect("hub out").len(),
            CACHE_HUB_OUT_DEGREE
        );
        for prefix in prefixes {
            assert_eq!(
                store.directed_out_edges(prefix).expect("prefix out").len(),
                1
            );
        }
        for spoke in spokes {
            let out = store.directed_out_edges(spoke).expect("spoke out");
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].neighbor_vid(), dst);
        }
    }

    #[test]
    fn converging_hub_prefixes_then_one_spoke() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["BenchWspCacheSrc"], Vec::<(&str, Value)>::new())
            .expect("src");
        let hub = store
            .insert_vertex_named(["BenchWspHub"], Vec::<(&str, Value)>::new())
            .expect("hub");
        let dst = store
            .insert_vertex_named(["BenchWspCacheDst"], Vec::<(&str, Value)>::new())
            .expect("dst");
        install_bench_wsp_wgt_profile();
        let road = catalog_edge_label("BenchWspWgtEdge");
        let mut prefixes = Vec::new();
        for _ in 0..CACHE_PREFIX_COUNT {
            prefixes.push(
                store
                    .insert_vertex_named(["BenchWspPrefix"], Vec::<(&str, Value)>::new())
                    .expect("prefix"),
            );
        }
        for &prefix in &prefixes {
            store
                .insert_directed_edge_with_payload_bytes(
                    prefix,
                    hub,
                    Some(road),
                    &1u16.to_le_bytes(),
                )
                .expect("prefix->hub");
        }
        for (i, &prefix) in prefixes.iter().enumerate() {
            store
                .insert_directed_edge_with_payload_bytes(
                    src,
                    prefix,
                    Some(road),
                    &((i % 10) as u16 + 1).to_le_bytes(),
                )
                .expect("src->prefix");
        }
        let spoke = store
            .insert_vertex_named(["BenchWspSpoke"], Vec::<(&str, Value)>::new())
            .expect("spoke");
        store
            .insert_directed_edge_with_payload_bytes(hub, spoke, Some(road), &1u16.to_le_bytes())
            .expect("hub->spoke");
        let _ = dst;
    }

    #[test]
    fn converging_hub_three_prefix_with_hub_edges() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["BenchWspCacheSrc"], Vec::<(&str, Value)>::new())
            .expect("src");
        let hub = store
            .insert_vertex_named(["BenchWspHub"], Vec::<(&str, Value)>::new())
            .expect("hub");
        install_bench_wsp_wgt_profile();
        let road = catalog_edge_label("BenchWspWgtEdge");
        for i in 0..3usize {
            let prefix = store
                .insert_vertex_named(["BenchWspPrefix"], Vec::<(&str, Value)>::new())
                .expect("prefix");
            store
                .insert_directed_edge_with_payload_bytes(
                    src,
                    prefix,
                    Some(road),
                    &((i % 10) as u16 + 1).to_le_bytes(),
                )
                .unwrap_or_else(|e| panic!("src->prefix i={i}: {e:?}"));
            store
                .insert_directed_edge_with_payload_bytes(
                    prefix,
                    hub,
                    Some(road),
                    &1u16.to_le_bytes(),
                )
                .unwrap_or_else(|e| panic!("prefix->hub i={i}: {e:?}"));
        }
    }

    #[test]
    fn converging_hub_src_prefix_only_three() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["BenchWspCacheSrc"], Vec::<(&str, Value)>::new())
            .expect("src");
        install_bench_wsp_wgt_profile();
        let road = catalog_edge_label("BenchWspWgtEdge");
        for i in 0..3usize {
            let prefix = store
                .insert_vertex_named(["BenchWspPrefix"], Vec::<(&str, Value)>::new())
                .expect("prefix");
            store
                .insert_directed_edge_with_payload_bytes(
                    src,
                    prefix,
                    Some(road),
                    &((i % 10) as u16 + 1).to_le_bytes(),
                )
                .unwrap_or_else(|e| panic!("src->prefix i={i}: {e:?}"));
        }
    }

    #[test]
    fn expand_single_label_hub_1k_setup_and_execute() {
        const HUB_OUT: u32 = 1_000;
        let store = GraphStore::new();
        setup_expand_single_label_hub(&store, HUB_OUT, "BenchExpandEdge");
        let result = execute_expand_plan(
            &store,
            &expand_plan_for_label("BenchExpandEdge", false, None),
        );
        assert_eq!(result.rows.len(), HUB_OUT as usize);
    }
}
