//! PocketIC / `canbench` targets for graph plan execution (`PhysicalPlan` replay).
//!
//! Run from `crates/graph`: `canbench` (see `canbench.yml`).

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
use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeWeightProfile, WeightEncoding};
use ic_stable_lara::{MaintenanceBudget, VertexId};
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
        name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
        args: vec![Expr::var(edge_var)],
        distinct: false,
    })
}

fn catalog_edge_label(store: &GraphStore, label_name: &str) -> EdgeLabelId {
    store.edge_label_id(label_name).expect("edge label")
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
    let road = catalog_edge_label(store, "BenchWspWgtEdge");

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
            .insert_directed_edge_with_inline_value(prefix, hub, Some(road), 1)
            .unwrap_or_else(|e| panic!("prefix->hub i={i}: {e:?}"));
    }
    for (i, &prefix) in prefixes.iter().enumerate() {
        store
            .insert_directed_edge_with_inline_value(src, prefix, Some(road), (i % 10) as u16 + 1)
            .unwrap_or_else(|e| panic!("src->prefix i={i}: {e:?}"));
    }
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("drain maintenance after prefix setup");

    for j in 0..CACHE_HUB_OUT_DEGREE {
        let spoke = store
            .insert_vertex_named(["BenchWspSpoke"], Vec::<(&str, Value)>::new())
            .expect("insert spoke");
        store
            .insert_directed_edge_with_inline_value(hub, spoke, Some(road), (j % 5) as u16 + 1)
            .expect("hub->spoke");
        store
            .insert_directed_edge_with_inline_value(spoke, dst, Some(road), 1)
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

/// Single-label hub, 1_000 out-edges (control for scaled 5a benches).
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

#[cfg(test)]
mod bench_setup_tests {
    use super::*;
    use ic_stable_lara::CsrEdge;

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
    fn repeated_edge_cost_cache_setup_preserves_full_topology() {
        let store = GraphStore::new();
        let start = u32::from(store.vertex_count());
        let (src, dst) = setup_repeated_edge_cost_cache_graph(&store);
        let end = u32::from(store.vertex_count());

        assert_eq!(u32::from(src), start);
        assert_eq!(u32::from(dst), start + 2);

        let prefix_label = store
            .vertex_label_id("BenchWspPrefix")
            .expect("prefix label");
        let hub_label = store.vertex_label_id("BenchWspHub").expect("hub label");
        let spoke_label = store.vertex_label_id("BenchWspSpoke").expect("spoke label");

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
            store.out_edges(src).expect("src out").len(),
            CACHE_PREFIX_COUNT
        );
        assert_eq!(
            store.out_edges(hub).expect("hub out").len(),
            CACHE_HUB_OUT_DEGREE
        );
        for prefix in prefixes {
            assert_eq!(store.out_edges(prefix).expect("prefix out").len(), 1);
        }
        for spoke in spokes {
            let out = store.out_edges(spoke).expect("spoke out");
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
        store
            .get_or_insert_edge_label_id("BenchWspWgtEdge")
            .expect("label");
        let road = catalog_edge_label(&store, "BenchWspWgtEdge");
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
                .insert_directed_edge_with_inline_value(prefix, hub, Some(road), 1)
                .expect("prefix->hub");
        }
        for (i, &prefix) in prefixes.iter().enumerate() {
            store
                .insert_directed_edge_with_inline_value(
                    src,
                    prefix,
                    Some(road),
                    (i % 10) as u16 + 1,
                )
                .expect("src->prefix");
        }
        let spoke = store
            .insert_vertex_named(["BenchWspSpoke"], Vec::<(&str, Value)>::new())
            .expect("spoke");
        store
            .insert_directed_edge_with_inline_value(hub, spoke, Some(road), 1)
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
        store
            .get_or_insert_edge_label_id("BenchWspWgtEdge")
            .expect("edge label");
        let road = catalog_edge_label(&store, "BenchWspWgtEdge");
        for i in 0..3usize {
            let prefix = store
                .insert_vertex_named(["BenchWspPrefix"], Vec::<(&str, Value)>::new())
                .expect("prefix");
            store
                .insert_directed_edge_with_inline_value(
                    src,
                    prefix,
                    Some(road),
                    (i % 10) as u16 + 1,
                )
                .unwrap_or_else(|e| panic!("src->prefix i={i}: {e:?}"));
            store
                .insert_directed_edge_with_inline_value(prefix, hub, Some(road), 1)
                .unwrap_or_else(|e| panic!("prefix->hub i={i}: {e:?}"));
        }
    }

    #[test]
    fn converging_hub_src_prefix_only_three() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["BenchWspCacheSrc"], Vec::<(&str, Value)>::new())
            .expect("src");
        store
            .get_or_insert_edge_label_id("BenchWspWgtEdge")
            .expect("edge label");
        let road = catalog_edge_label(&store, "BenchWspWgtEdge");
        for i in 0..3usize {
            let prefix = store
                .insert_vertex_named(["BenchWspPrefix"], Vec::<(&str, Value)>::new())
                .expect("prefix");
            store
                .insert_directed_edge_with_inline_value(
                    src,
                    prefix,
                    Some(road),
                    (i % 10) as u16 + 1,
                )
                .unwrap_or_else(|e| panic!("src->prefix i={i}: {e:?}"));
        }
    }

    #[test]
    fn expand_single_label_hub_1k_setup_and_execute() {
        let store = GraphStore::new();
        setup_expand_single_label_hub(&store, EXPAND_HUB_OUT_XL, "BenchExpandEdge");
        let result = execute_expand_plan(
            &store,
            &expand_plan_for_label("BenchExpandEdge", false, None),
        );
        assert_eq!(result.rows.len(), EXPAND_HUB_OUT_XL as usize);
    }
}
