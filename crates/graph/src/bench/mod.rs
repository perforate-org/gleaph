//! PocketIC / `canbench` targets for graph plan execution (`PhysicalPlan` replay).
//!
//! Run from `crates/graph`: `canbench` (see `canbench.yml`).
//!
//! Priority-5 signal benches (compare paired workloads, not absolute instruction counts):
//!
//! - **5a label scan**: `expand_mixed_label_hub_240scan_24match` vs `expand_hub_return_dst_only`;
//!   `expand_skewed_noise_200a_24b` for skewed noise.
//! - **5b row clone**: `expand_deep_row_hub_24out` vs `expand_hub_return_dst_only`;
//!   `expand_hash_join_then_expand_48x24` for join-inflated rows.
//! - **P1 / P4 cross-check**: `expand_filter_10pct_pass` (reject clone); `expand_indexed_eq_selective_24match`
//!   vs mixed-label scan.

use crate::facade::GraphStore;
use crate::facade::mutation_executor::GraphMutationExecutor;
use crate::gql_execution_context::GqlExecutionContext;
use crate::plan::query::{PlanQueryResult, execute_plan_query};
use canbench_rs::bench;
use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind, ObjectName};
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::plan::{
    PhysicalPlan, PlanAnnotations, PlanDiagnostics, PlanOp, ProjectColumn, ScanValue, ShortestMode,
    ShortestPathCost, VarLenSpec,
};
use gleaph_graph_kernel::entry::{EdgeMeta, EdgeWeightProfile, InlineEdgeLabelId, WeightEncoding};
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

fn plan(ops: Vec<PlanOp>) -> PhysicalPlan {
    PhysicalPlan {
        ops,
        diagnostics: PlanDiagnostics::default(),
        annotations: PlanAnnotations::default(),
    }
}

fn gleaph_weight_call(edge_var: &str) -> Expr {
    Expr::new(ExprKind::FunctionCall {
        name: ObjectName::simple("GLEAPH_WEIGHT"),
        args: vec![Expr::var(edge_var)],
        distinct: false,
    })
}

fn edge_meta_for_label(store: &GraphStore, label_name: &str) -> EdgeMeta {
    let lid = store.label_id(label_name).expect("label");
    let inline = InlineEdgeLabelId::from_label_id(lid).expect("inline edge label");
    EdgeMeta::new(false, false, Some(inline))
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

/// Many prefixes converge on one hub, then repeatedly expand the same hub outgoing edges.
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

    let label_id = store
        .get_or_insert_edge_label_id("BenchWspWgtEdge")
        .expect("edge label");
    store
        .set_edge_label_weight_profile(
            label_id,
            EdgeWeightProfile {
                encoding: WeightEncoding::RawU16,
            },
        )
        .expect("weight profile");
    let meta = edge_meta_for_label(store, "BenchWspWgtEdge");

    for i in 0..CACHE_PREFIX_COUNT {
        let prefix = store
            .insert_vertex_named(["BenchWspPrefix"], Vec::<(&str, Value)>::new())
            .expect("insert prefix");
        store
            .insert_directed_edge_with_inline_value(src, prefix, meta, (i % 10) as u16 + 1)
            .expect("src->prefix");
        store
            .insert_directed_edge_with_inline_value(prefix, hub, meta, 1)
            .expect("prefix->hub");
    }

    for j in 0..CACHE_HUB_OUT_DEGREE {
        let spoke = store
            .insert_vertex_named(["BenchWspSpoke"], Vec::<(&str, Value)>::new())
            .expect("insert spoke");
        store
            .insert_directed_edge_with_inline_value(hub, spoke, meta, (j % 5) as u16 + 1)
            .expect("hub->spoke");
        store
            .insert_directed_edge_with_inline_value(spoke, dst, meta, 1)
            .expect("spoke->dst");
    }

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
/// outgoing edges. Intended to measure hop-cost cache reuse for `GLEAPH_WEIGHT` decode plus
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
            .expect("prefix->hub");
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
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding,
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

fn setup_expand_mixed_label_hub_graph(store: &GraphStore) -> String {
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    let target_label = "BenchExpandLbl0".to_owned();
    for label_idx in 0..EXPAND_MIXED_LABEL_COUNT {
        let label = format!("BenchExpandLbl{label_idx}");
        for i in 0..EXPAND_HUB_OUT {
            let dst = store
                .insert_vertex_named(
                    [format!("BenchMixedDst{label_idx}_{i}")],
                    Vec::<(&str, Value)>::new(),
                )
                .expect("dst");
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
    let store = GraphStore::new();
    let target_label = setup_expand_mixed_label_hub_graph(&store);
    let plan = expand_plan_for_label(&target_label, false, None);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_mixed_label_hub_240scan_24match");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), EXPAND_HUB_OUT as usize);
        black_box(result.rows.len())
    })
}

fn setup_expand_skewed_noise_graph(store: &GraphStore) {
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for i in 0..EXPAND_SKEW_NOISE {
        let dst = store
            .insert_vertex_named(
                [format!("BenchSkewNoiseDst{i}")],
                Vec::<(&str, Value)>::new(),
            )
            .expect("noise dst");
        store
            .insert_directed_edge_named(
                hub,
                dst,
                Some("BenchExpandNoise"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("noise edge");
    }
    for i in 0..EXPAND_HUB_OUT {
        let dst = store
            .insert_vertex_named(
                [format!("BenchSkewTargetDst{i}")],
                Vec::<(&str, Value)>::new(),
            )
            .expect("target dst");
        store
            .insert_directed_edge_named(
                hub,
                dst,
                Some("BenchExpandTarget"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("target edge");
    }
}

/// 200 noise edges + 24 target-label edges; expand target label only.
#[bench(raw)]
fn bench_graph_expand_skewed_noise_200a_24b() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_skewed_noise_graph(&store);
    let plan = expand_plan_for_label("BenchExpandTarget", false, None);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("expand_skewed_noise_200a_24b");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), EXPAND_HUB_OUT as usize);
        black_box(result.rows.len())
    })
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
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
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
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
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
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
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
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for i in 0..EXPAND_FILTER_TOTAL {
        let pass = i < EXPAND_FILTER_PASS;
        let tag = if pass { 1i64 } else { 0i64 };
        let dst = store
            .insert_vertex_named(
                [format!("BenchFilter10Dst{i}")],
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
    let hub = store
        .insert_vertex_named(["BenchExpandHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    for i in 0..EXPAND_HJ_PREFIXES {
        let prefix = store
            .insert_vertex_named(["BenchHjPrefix"], Vec::<(&str, Value)>::new())
            .expect("prefix");
        store
            .insert_directed_edge_named(
                prefix,
                hub,
                Some("BenchHjToHub"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("prefix->hub");
    }
    for i in 0..EXPAND_HUB_OUT {
        let dst = store
            .insert_vertex_named([format!("BenchHjDst{i}")], Vec::<(&str, Value)>::new())
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
                    edge_property_projection: None,
                    dst_property_projection: None,
                    hop_aux_binding: None,
                    emit_edge_binding: false,
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
                    edge_property_projection: None,
                    dst_property_projection: None,
                    hop_aux_binding: None,
                    emit_edge_binding: false,
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
