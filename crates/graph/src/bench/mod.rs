//! PocketIC / `canbench` targets for weighted shortest-path execution (`PhysicalPlan` replay).
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
    PhysicalPlan, PlanAnnotations, PlanDiagnostics, PlanOp, ProjectColumn, ShortestMode,
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
            mode: ShortestMode::AnyShortest,
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

fn execute_weighted_shortest(store: &GraphStore, plan: &PhysicalPlan) -> PlanQueryResult {
    pollster::block_on(execute_plan_query(
        store,
        plan,
        &params(),
        None,
        GqlExecutionContext::default(),
    ))
    .expect("weighted shortest path plan")
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
        let result = execute_weighted_shortest(black_box(&store), black_box(&plan));
        assert_eq!(
            result.rows.len(),
            1,
            "frontier benchmark should find one path"
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
        let result = execute_weighted_shortest(black_box(&store), black_box(&plan));
        assert_eq!(
            result.rows.len(),
            1,
            "edge-cost cache benchmark should find one path"
        );
        black_box(result.rows.len())
    })
}
