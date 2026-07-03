//! PocketIC / `canbench` targets for graph plan execution (`PhysicalPlan` replay).
//!
//! Run from `crates/graph`: `canbench` (see `canbench.yml`).

#[cfg(feature = "canbench_large")]
mod large;
mod stable_layout;

use crate::edge_payload_scalar_codec::encode_edge_payload_scalar;
use crate::facade::GraphStore;
use crate::facade::mutation_executor::GraphMutationExecutor;
use crate::gql_execution_context::GqlExecutionContext;
use crate::gql_run::{GqlCanisterExecutionMode, run_wire_plan_last_read_row_count};
use crate::plan::query::{PlanQueryResult, execute_plan_query, execute_plan_query_bindings};
use canbench_rs::bench;
use gleaph_gql::Value;
use gleaph_gql::ast::{CmpOp, Expr, ExprKind, ObjectName};
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::plan::{
    EdgePayloadPredicate, EdgeVectorMetric, EdgeVectorPredicate, PhysicalPlan, PlanOp,
    ProjectColumn, PropertyAssignment, ScanValue, SearchOutputKind, SearchOutputPlan,
    SearchProviderPlan, ShortestMode, ShortestPathCost, VarLenSpec,
};
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_graph_kernel::entry::{
    ConstraintNameId, EdgeLabelId, EdgePayloadEncoding, EdgePayloadProfile, EdgeWeightProfile,
    PropertyId, Vertex, WeightEncoding,
};
use gleaph_graph_kernel::federation::{ClaimId, EffectId, UniqueEffectOp, UniqueEffectReceipt};
use gleaph_graph_kernel::plan_exec::{ResolvedSearchVertexHitWire, ResolvedSearchWire};
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

fn execute_shortest_plan_with_context(
    store: &GraphStore,
    plan: &PhysicalPlan,
    execution: GqlExecutionContext,
) -> PlanQueryResult {
    pollster::block_on(execute_plan_query(store, plan, &params(), None, execution))
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

fn inline_cost_execution_context(
    label_id: EdgeLabelId,
    property_id: PropertyId,
) -> GqlExecutionContext {
    use gleaph_graph_kernel::plan_exec::{
        ResolvedEdgeLabel, ResolvedLabelTable, ResolvedProperty, ResolvedPropertyTable,
        ResolvedVertexLabel,
    };
    let src_label_id = crate::test_labels::vertex_label_id_for_name("BenchInlineCostSrc");
    let dst_label_id = crate::test_labels::vertex_label_id_for_name("BenchInlineCostDst");
    let labels = ResolvedLabelTable {
        vertex: vec![
            ResolvedVertexLabel {
                name: "BenchInlineCostSrc".to_string(),
                id: src_label_id,
            },
            ResolvedVertexLabel {
                name: "BenchInlineCostDst".to_string(),
                id: dst_label_id,
            },
        ],
        edge: vec![ResolvedEdgeLabel::with_inline_property(
            "BenchInlineCostEdge".to_string(),
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::RawU16,
            },
            Some(property_id),
        )],
    };
    let properties = ResolvedPropertyTable {
        properties: vec![ResolvedProperty {
            name: "distance".to_string(),
            id: property_id,
        }],
    };
    GqlExecutionContext {
        resolved_labels: Some(labels),
        resolved_properties: Some(properties),
        ..GqlExecutionContext::with_host_test_element_id_key()
    }
}

const INLINE_CACHE_PREFIX_COUNT: usize = 48;
const INLINE_CACHE_HUB_OUT_DEGREE: usize = 24;

fn setup_repeated_inline_cost_cache_graph(store: &GraphStore) -> (VertexId, VertexId) {
    let src = store
        .insert_vertex_named(["BenchInlineCostSrc"], Vec::<(&str, Value)>::new())
        .expect("insert src");
    let hub = store
        .insert_vertex_named(["BenchInlineCostHub"], Vec::<(&str, Value)>::new())
        .expect("insert hub");
    let dst = store
        .insert_vertex_named(["BenchInlineCostDst"], Vec::<(&str, Value)>::new())
        .expect("insert dst");

    let label_id = crate::test_labels::edge_label_id_for_name("BenchInlineCostEdge");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::RawU16,
        },
    );
    crate::test_labels::install_test_edge_inline_property(label_id, PropertyId::from_raw(1));
    let road = catalog_edge_label("BenchInlineCostEdge");

    let mut prefixes = Vec::with_capacity(INLINE_CACHE_PREFIX_COUNT);
    for _ in 0..INLINE_CACHE_PREFIX_COUNT {
        prefixes.push(
            store
                .insert_vertex_named(["BenchInlineCostPrefix"], Vec::<(&str, Value)>::new())
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
    for j in 0..INLINE_CACHE_HUB_OUT_DEGREE {
        let spoke = store
            .insert_vertex_named(["BenchInlineCostSpoke"], Vec::<(&str, Value)>::new())
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
        .expect("finalize bulk ingest");

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

/// Same hub convergence workload as `bench_graph_weighted_shortest_repeated_edge_cost_cache`,
/// but hop costs are sourced through the inline property reader (`COST BY e.distance`) rather
/// than the direct `GLEAPH.WEIGHT(e)` decoder fast path. Measures the resolution/decode overhead
/// of the Slice 23 path.
#[bench(raw)]
fn bench_graph_weighted_shortest_inline_cost_48prefix_24hub_out() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let (_src, _dst) = setup_repeated_inline_cost_cache_graph(&store);
    let edge_label_id = catalog_edge_label("BenchInlineCostEdge");
    let property_id = PropertyId::from_raw(1);
    let execution = inline_cost_execution_context(edge_label_id, property_id);
    let plan = weighted_shortest_plan(
        "BenchInlineCostSrc",
        "BenchInlineCostDst",
        "BenchInlineCostEdge",
        Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::var("e")),
            property: "distance".into(),
        }),
        5,
    );

    // Warm up to ensure any one-time resolution work is outside the measured closure.
    let _ = execute_shortest_plan_with_context(&store, &plan, execution.clone());

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("weighted_shortest_inline_cost");
        let result = execute_shortest_plan_with_context(
            black_box(&store),
            black_box(&plan),
            execution.clone(),
        );
        assert_eq!(
            result.rows.len(),
            1,
            "inline-cost benchmark should find one path"
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

// --- ADR 0034 Slice 5: non-leading SEARCH join benches ---

const SEARCH_JOIN_INPUT_M: usize = 1_000;
const SEARCH_JOIN_INPUT_L: usize = 10_000;
const SEARCH_JOIN_HITS_S: usize = 10;
const SEARCH_JOIN_HITS_M: usize = 100;

fn setup_search_join_graph(store: &GraphStore, vertex_count: usize) -> Vec<VertexId> {
    (0..vertex_count)
        .map(|_| store.insert_vertex().expect("insert search join vertex"))
        .collect()
}

fn search_join_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "d".into(),
            label: None,
            property_projection: None,
        },
        PlanOp::Search {
            binding: "d".into(),
            provider: SearchProviderPlan::VectorIndex {
                index_name: vec!["bench_vec".into()],
                query: Expr::new(ExprKind::Literal(Value::Bytes(vec![0; 12]))),
                limit: Expr::int(SEARCH_JOIN_HITS_M as i64),
                filter: None,
            },
            output: SearchOutputPlan {
                kind: SearchOutputKind::Distance,
                alias: "distance".into(),
            },
        },
        PlanOp::Project {
            columns: vec![],
            distinct: false,
        },
    ])
}

fn build_resolved_search_for_hits(hits: &[VertexId], start_value: f64) -> ResolvedSearchWire {
    ResolvedSearchWire {
        binding: "d".into(),
        output_alias: "distance".into(),
        vertex_hits: hits
            .iter()
            .enumerate()
            .map(|(i, &v)| ResolvedSearchVertexHitWire {
                local_vertex_id: u32::from(v),
                value: start_value + i as f64 * 0.01,
            })
            .collect(),
    }
}

fn bench_search_join(
    input_count: usize,
    hit_count: usize,
    scope: &'static str,
) -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let vertices = setup_search_join_graph(&store, input_count);
    let hits: Vec<_> = vertices.iter().take(hit_count).copied().collect();
    let resolved_search = build_resolved_search_for_hits(&hits, 1.0);
    let plan = search_join_plan();
    let execution = GqlExecutionContext {
        resolved_search: Some(resolved_search),
        ..Default::default()
    };

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let result = pollster::block_on(execute_plan_query(
            black_box(&store),
            black_box(&plan),
            &params(),
            None,
            execution.clone(),
        ))
        .expect("search join plan");
        assert_eq!(result.rows.len(), hit_count);
        black_box(result.rows.len())
    })
}

/// 1_000 input rows, 10 resolved search hits.
#[bench(raw)]
fn bench_graph_search_join_1k_input_10_hits() -> canbench_rs::BenchResult {
    bench_search_join(
        SEARCH_JOIN_INPUT_M,
        SEARCH_JOIN_HITS_S,
        "search_join_1k_input_10_hits",
    )
}

/// 1_000 input rows, 100 resolved search hits.
#[bench(raw)]
fn bench_graph_search_join_1k_input_100_hits() -> canbench_rs::BenchResult {
    bench_search_join(
        SEARCH_JOIN_INPUT_M,
        SEARCH_JOIN_HITS_M,
        "search_join_1k_input_100_hits",
    )
}

/// 10_000 input rows, 10 resolved search hits.
#[bench(raw)]
fn bench_graph_search_join_10k_input_10_hits() -> canbench_rs::BenchResult {
    bench_search_join(
        SEARCH_JOIN_INPUT_L,
        SEARCH_JOIN_HITS_S,
        "search_join_10k_input_10_hits",
    )
}

/// 10_000 input rows, 100 resolved search hits.
#[bench(raw)]
fn bench_graph_search_join_10k_input_100_hits() -> canbench_rs::BenchResult {
    bench_search_join(
        SEARCH_JOIN_INPUT_L,
        SEARCH_JOIN_HITS_M,
        "search_join_10k_input_100_hits",
    )
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

// --- ADR 0029 canonical mutation segment benches (wire path: router -> shard) ---
//
// These measure the shard-local canonical critical section
// (`apply_canonical_mutation_segment`, ADR 0029 §1): decode the wire plan bundle, execute the
// DML against `GraphStore`, append the label-stats projection *intent* to the delta log, and
// record the mutation journal — all inside one message segment. `index` is `None` and federation
// routing is unset, so there is no inter-canister projection *delivery* (`flush_pending` is a
// no-op): the benches isolate the canonical write path for the vertex / property / edge mutations
// named in the ADR 0029 Phase 1 roadmap, giving a baseline for future boundary changes.

/// Run a single-DML wire plan through the canonical mutation segment with no index delivery.
fn run_canonical_segment(store: GraphStore, plan: &PhysicalPlan) {
    let blob = encode_block_plans(std::slice::from_ref(plan), true).expect("encode mutation plan");
    pollster::block_on(run_wire_plan_last_read_row_count(
        store,
        &blob,
        &params(),
        GqlCanisterExecutionMode::Update,
        None,
        GqlExecutionContext::default(),
        None,
        Some(1),
    ))
    .expect("canonical mutation segment");
}

fn insert_vertex_mutation_plan(label: &str, properties: Vec<PropertyAssignment>) -> PhysicalPlan {
    plan(vec![PlanOp::InsertVertex {
        variable: Some("n".into()),
        labels: vec![label.into()],
        properties,
    }])
}

fn insert_edge_mutation_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::InsertVertex {
            variable: Some("a".into()),
            labels: vec!["BenchMutEdgeSrc".into()],
            properties: vec![],
        },
        PlanOp::InsertVertex {
            variable: Some("b".into()),
            labels: vec!["BenchMutEdgeDst".into()],
            properties: vec![],
        },
        PlanOp::InsertEdge {
            variable: None,
            src: "a".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            labels: vec!["BenchMutEdge".into()],
            properties: vec![],
        },
    ])
}

/// Canonical segment: insert one labeled vertex (no properties).
#[bench(raw)]
fn bench_graph_canonical_segment_insert_vertex() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let plan = insert_vertex_mutation_plan("BenchMutVertex", vec![]);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("canonical_segment_insert_vertex");
        run_canonical_segment(black_box(store), black_box(&plan));
    })
}

/// Canonical segment: insert one labeled vertex carrying one property (property store write path).
#[bench(raw)]
fn bench_graph_canonical_segment_insert_vertex_with_property() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let plan = insert_vertex_mutation_plan(
        "BenchMutVertexProp",
        vec![PropertyAssignment {
            name: "weight".into(),
            value: Expr::new(ExprKind::Literal(Value::Int64(7))),
        }],
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("canonical_segment_insert_vertex_with_property");
        run_canonical_segment(black_box(store), black_box(&plan));
    })
}

/// Canonical segment: insert two vertices and a directed edge between them in one plan.
#[bench(raw)]
fn bench_graph_canonical_segment_insert_edge() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let plan = insert_edge_mutation_plan();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("canonical_segment_insert_edge");
        run_canonical_segment(black_box(store), black_box(&plan));
    })
}

/// A multi-statement INSERT bundle compiled to a single plan of `n` `InsertVertex` ops. This is the
/// shape contract 1/2 (ADR 0029 §6, Phase 5) executes on one shard: the whole bundle runs under the
/// shard's single canonical critical section. Measures one-shard execution scaling with statement
/// count.
fn insert_bundle_mutation_plan(n: usize) -> PhysicalPlan {
    plan(
        (0..n)
            .map(|_| PlanOp::InsertVertex {
                variable: None,
                labels: vec!["BenchMutBundleVertex".into()],
                properties: vec![],
            })
            .collect(),
    )
}

/// Canonical segment: a 4-statement INSERT bundle on one shard (ADR 0029 Phase 5 one-shard execution).
#[bench(raw)]
fn bench_graph_canonical_segment_insert_bundle_4() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let plan = insert_bundle_mutation_plan(4);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("canonical_segment_insert_bundle_4");
        run_canonical_segment(black_box(store), black_box(&plan));
    })
}

/// Canonical segment: a 16-statement INSERT bundle on one shard (ADR 0029 Phase 5 one-shard execution).
#[bench(raw)]
fn bench_graph_canonical_segment_insert_bundle_16() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let plan = insert_bundle_mutation_plan(16);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("canonical_segment_insert_bundle_16");
        run_canonical_segment(black_box(store), black_box(&plan));
    })
}

// --- ADR 0030 unique-effect outbox benches (graph-shard side of the cross-shard TCC) ---
//
// These complement the Router-side reservation benches (`crates/router/src/bench.rs`) by measuring
// the **graph-shard** legs of the protocol that the Router-only canbench cannot reach: the pinned
// unique-effect outbox the shard appends inside its canonical write segment, the replicated
// `Acquire` proof read the Router's Confirm performs, the per-effect ack (unpin) round, and Driver
// 2's paginated effect enumeration. The 1/16/256 append sweep doubles as the **storage-growth
// baseline** for the outbox (instruction cost of pinning N effects in one mutation). Each bench uses
// a distinct `mutation_id` so the shared thread-local outbox does not collide across benches in the
// same canister instance.

const UNIQUE_OUTBOX_BENCH_CONSTRAINT: ConstraintNameId = ConstraintNameId::from_raw(1);

fn unique_acquire_receipt(mutation_id: u64, ordinal: u32) -> UniqueEffectReceipt {
    UniqueEffectReceipt {
        effect_id: EffectId::new(mutation_id, ordinal),
        claim_id: Some(ClaimId::new(mutation_id, ordinal)),
        owner_element_id: u64::from(ordinal).to_le_bytes().to_vec(),
        constraint_id: UNIQUE_OUTBOX_BENCH_CONSTRAINT,
        encoded_value: format!("bench-value-{ordinal:08}").into_bytes(),
        op: UniqueEffectOp::Acquire,
    }
}

fn seed_unique_outbox_acquires(store: &GraphStore, mutation_id: u64, count: u32) {
    for ordinal in 0..count {
        store.emit_unique_effect(unique_acquire_receipt(mutation_id, ordinal));
    }
}

/// Append `count` `Acquire` effects into a fresh outbox — the per-effect pin work the shard does
/// inside its canonical write segment, and the outbox storage-growth baseline at 1/16/256.
fn bench_unique_outbox_append(
    mutation_seed: u64,
    count: u32,
    scope: &'static str,
) -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let mutation_id = 8_000_000 + mutation_seed;
    let receipts: Vec<UniqueEffectReceipt> = (0..count)
        .map(|ordinal| unique_acquire_receipt(mutation_id, ordinal))
        .collect();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        for receipt in &receipts {
            store.emit_unique_effect(black_box(receipt.clone()));
        }
    })
}

#[bench(raw)]
fn bench_graph_unique_outbox_append_1() -> canbench_rs::BenchResult {
    bench_unique_outbox_append(1, 1, "unique_outbox_append_1")
}

#[bench(raw)]
fn bench_graph_unique_outbox_append_16() -> canbench_rs::BenchResult {
    bench_unique_outbox_append(16, 16, "unique_outbox_append_16")
}

#[bench(raw)]
fn bench_graph_unique_outbox_append_256() -> canbench_rs::BenchResult {
    bench_unique_outbox_append(256, 256, "unique_outbox_append_256")
}

/// The Router's Confirm proof read: resolve one claim's `Acquire` evidence over a 256-effect outbox.
#[bench(raw)]
fn bench_graph_unique_outbox_acquire_proof_read_256() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let mutation_id = 8_100_001;
    seed_unique_outbox_acquires(&store, mutation_id, 256);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("unique_outbox_acquire_proof_read_256");
        let evidence = store.unique_acquire_evidence(black_box(ClaimId::new(mutation_id, 200)));
        black_box(evidence);
    })
}

/// The post-Confirm ack round: unpin all 256 pinned effects of a mutation (the inter-canister leg
/// the Router drives after a committed canonical write).
#[bench(raw)]
fn bench_graph_unique_outbox_ack_round_256() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let mutation_id = 8_200_001;
    seed_unique_outbox_acquires(&store, mutation_id, 256);
    let effect_ids: Vec<EffectId> = (0..256)
        .map(|ordinal| EffectId::new(mutation_id, ordinal))
        .collect();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("unique_outbox_ack_round_256");
        store.ack_unique_effects(black_box(effect_ids.clone()));
    })
}

/// Driver 2's recovery enumeration: read one 64-effect page from a 256-effect outbox.
#[bench(raw)]
fn bench_graph_unique_outbox_effects_page_256() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let mutation_id = 8_300_001;
    seed_unique_outbox_acquires(&store, mutation_id, 256);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("unique_outbox_effects_page_256");
        let page =
            store.unique_effects_page(black_box(mutation_id), black_box(None), black_box(64));
        black_box(page.len());
    })
}

// --- ADR 0030 slice 10 `ShardLocalGlobal` local-unique-table benches (graph-shard fast path) ---
//
// The `ShardLocalGlobal` fast path enforces a graph-wide UNIQUE constraint entirely inside the one
// owning shard's local table (`GRAPH_LOCAL_UNIQUE_VALUES`): no Router reservation, no pinned
// unique-effect outbox, and no inter-canister ack round. These measure that path's real per-op cost
// so it can be compared against the FederatedTcc legs benchmarked above and in the Router crate
// (Router `try_reserve`/`confirm`/`cancel` in `crates/router/src/bench.rs`; graph `unique_outbox_*`
// here):
//   - acquire: the preflight `contains` (absent) + `insert` the canonical write segment performs
//     all-or-nothing for a single-element INSERT;
//   - duplicate reject: the preflight `contains` hit done before returning `UniquenessViolation`
//     (no canonical write, no Router round-trip);
//   - free: the owner-matched `remove_if_owner` a constrained DELETE performs in its canonical
//     segment (no outbox `Release`, no Router reconcile);
//   - DROP purge: one bounded drain page over a populated table (the slice-9 drain branch the
//     drop-drain recovery lane calls per tick for a `ShardLocalGlobal` constraint).
//
// Each bench uses a distinct `constraint_id` so the shared thread-local table does not collide
// across benches in the same canister instance.

fn local_unique_value(ordinal: u32) -> Vec<u8> {
    format!("bench-local-value-{ordinal:08}").into_bytes()
}

fn local_unique_owner(ordinal: u32) -> Vec<u8> {
    u64::from(ordinal).to_le_bytes().to_vec()
}

/// `ShardLocalGlobal` acquire for one claim: preflight `contains` (absent) + `insert`.
#[bench(raw)]
fn bench_graph_local_unique_acquire_1() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let constraint = ConstraintNameId::from_raw(11);
    let value = local_unique_value(0);
    let owner = local_unique_owner(0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("local_unique_acquire_1");
        let present = store.local_unique_contains(black_box(constraint), black_box(&value));
        assert!(
            !present,
            "acquire bench preflight must see the value absent"
        );
        store.local_unique_insert(
            black_box(constraint),
            black_box(value.clone()),
            black_box(owner.clone()),
        );
    })
}

/// `ShardLocalGlobal` duplicate rejection: the preflight `contains` hit on an already-claimed value.
#[bench(raw)]
fn bench_graph_local_unique_duplicate_reject_1() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let constraint = ConstraintNameId::from_raw(12);
    let value = local_unique_value(0);
    store.local_unique_insert(constraint, value.clone(), local_unique_owner(0));
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("local_unique_duplicate_reject_1");
        let present = store.local_unique_contains(black_box(constraint), black_box(&value));
        assert!(
            present,
            "duplicate-reject bench preflight must see the value present"
        );
        black_box(present);
    })
}

/// `ShardLocalGlobal` free: the owner-matched `remove_if_owner` a constrained DELETE performs.
#[bench(raw)]
fn bench_graph_local_unique_release_owner_match_1() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let constraint = ConstraintNameId::from_raw(13);
    let value = local_unique_value(0);
    let owner = local_unique_owner(0);
    store.local_unique_insert(constraint, value.clone(), owner.clone());
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("local_unique_release_owner_match_1");
        let removed = store.local_unique_remove_if_owner(
            black_box(constraint),
            black_box(&value),
            black_box(&owner),
        );
        assert!(removed, "release bench must free the owner-matched value");
        black_box(removed);
    })
}

/// `ShardLocalGlobal` DROP drain: one bounded purge page over a 256-entry local table.
#[bench(raw)]
fn bench_graph_local_unique_purge_page_256() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let constraint = ConstraintNameId::from_raw(14);
    for ordinal in 0..256u32 {
        store.local_unique_insert(
            constraint,
            local_unique_value(ordinal),
            local_unique_owner(ordinal),
        );
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("local_unique_purge_page_256");
        let progress = store.local_unique_purge(black_box(constraint), black_box(256));
        assert_eq!(
            progress.removed, 256,
            "the purge page drains all seeded entries"
        );
        black_box(progress.done);
    })
}

// --- ADR 0034 Slice 21 inline edge scalar read access benches ---

const INLINE_EDGE_COUNT: u32 = 1_000;

fn setup_inline_scalar_edges(
    store: &GraphStore,
) -> (VertexId, gleaph_graph_kernel::entry::EdgeLabelId) {
    let label_id = crate::test_labels::edge_label_id_for_name("BenchInlineRoad");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::RawU16,
        },
    );
    let src = store
        .insert_vertex_named(["BenchInlineSrc"], Vec::<(&str, Value)>::new())
        .expect("src");
    for i in 0..INLINE_EDGE_COUNT {
        let dst = store
            .insert_vertex_named(["BenchInlineDst"], Vec::<(&str, Value)>::new())
            .expect("dst");
        store
            .insert_directed_edge_with_payload_bytes(
                src,
                dst,
                Some(label_id),
                &((i % 100) as u16).to_le_bytes(),
            )
            .expect("edge");
    }
    (src, label_id)
}

fn inline_read_execution_context(
    label_id: EdgeLabelId,
    property_id: PropertyId,
) -> GqlExecutionContext {
    use gleaph_graph_kernel::plan_exec::{
        ResolvedEdgeLabel, ResolvedLabelTable, ResolvedProperty, ResolvedPropertyTable,
        ResolvedVertexLabel,
    };
    let src_label_id = crate::test_labels::vertex_label_id_for_name("BenchInlineSrc");
    let dst_label_id = crate::test_labels::vertex_label_id_for_name("BenchInlineDst");
    let labels = ResolvedLabelTable {
        vertex: vec![
            ResolvedVertexLabel {
                name: "BenchInlineSrc".to_string(),
                id: src_label_id,
            },
            ResolvedVertexLabel {
                name: "BenchInlineDst".to_string(),
                id: dst_label_id,
            },
        ],
        edge: vec![ResolvedEdgeLabel::with_inline_property(
            "BenchInlineRoad".to_string(),
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::RawU16,
            },
            Some(property_id),
        )],
    };
    let properties = ResolvedPropertyTable {
        properties: vec![ResolvedProperty {
            name: "distance".to_string(),
            id: property_id,
        }],
    };
    GqlExecutionContext {
        resolved_labels: Some(labels),
        resolved_properties: Some(properties),
        ..GqlExecutionContext::with_host_test_element_id_key()
    }
}

fn inline_projection_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("BenchInlineSrc".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchInlineRoad".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: Some(vec!["distance".into()].into()),
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(
                Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(var("e")),
                    property: "distance".into(),
                }),
                "d",
            )],
            distinct: false,
        },
    ])
}

fn inline_filter_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("BenchInlineSrc".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchInlineRoad".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Filter {
            condition: Expr::new(ExprKind::Compare {
                left: Box::new(Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(var("e")),
                    property: "distance".into(),
                })),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(7)))),
            }),
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ])
}

/// Inline scalar edge-property projection over a fixed out-edge set.
#[bench(raw)]
fn bench_graph_inline_scalar_projection_1k() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let (_src, label_id) = setup_inline_scalar_edges(&store);
    let property_id = PropertyId::from_raw(1);
    let ctx = inline_read_execution_context(label_id, property_id);
    let plan = inline_projection_plan();

    // Verify fixture membership once, outside the measured closure.
    let probe = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        None,
        ctx.clone(),
    ))
    .expect("inline projection probe");
    assert_eq!(
        probe.rows.len(),
        INLINE_EDGE_COUNT as usize,
        "projection benchmark should emit one row per edge"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("inline_scalar_projection_1k");
        let result = pollster::block_on(execute_plan_query(
            black_box(&store),
            black_box(&plan),
            &params(),
            None,
            black_box(ctx.clone()),
        ))
        .expect("inline projection plan");
        black_box(result.rows.len())
    })
}

/// Inline scalar edge-property equality filter over a fixed out-edge set.
#[bench(raw)]
fn bench_graph_inline_scalar_filter_1k() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let (_src, label_id) = setup_inline_scalar_edges(&store);
    let property_id = PropertyId::from_raw(1);
    let ctx = inline_read_execution_context(label_id, property_id);
    let plan = inline_filter_plan();

    // Verify fixture membership once, outside the measured closure.
    let probe = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        None,
        ctx.clone(),
    ))
    .expect("inline filter probe");
    // Fixture uses (i % 100) for 0..1000, so value 7 appears exactly 10 times.
    assert_eq!(
        probe.rows.len(),
        10,
        "filter benchmark should match exactly 10 rows for distance = 7"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("inline_scalar_filter_1k");
        let result = pollster::block_on(execute_plan_query(
            black_box(&store),
            black_box(&plan),
            &params(),
            None,
            black_box(ctx.clone()),
        ))
        .expect("inline filter plan");
        black_box(result.rows.len())
    })
}

// --- ADR 0034 Slice 25 inline edge STRUCT read access benches ---

fn setup_inline_struct_edges(
    store: &GraphStore,
) -> (VertexId, gleaph_graph_kernel::entry::EdgeLabelId) {
    let label_id = crate::test_labels::edge_label_id_for_name("BenchInlineStructRoad");
    let total_width: u16 = 16;
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: total_width,
            encoding: EdgePayloadEncoding::RawBytes,
        },
    );
    let property_id = PropertyId::from_raw(2);
    crate::test_labels::install_test_edge_inline_struct_property(
        label_id,
        property_id,
        vec![
            (
                "score".to_string(),
                0,
                EdgePayloadProfile {
                    byte_width: 4,
                    encoding: EdgePayloadEncoding::F32,
                },
            ),
            (
                "confidence".to_string(),
                4,
                EdgePayloadProfile {
                    byte_width: 4,
                    encoding: EdgePayloadEncoding::F32,
                },
            ),
            (
                "updated_at".to_string(),
                8,
                EdgePayloadProfile {
                    byte_width: 8,
                    encoding: EdgePayloadEncoding::RawU64,
                },
            ),
        ],
    );
    let src = store
        .insert_vertex_named(["BenchInlineStructSrc"], Vec::<(&str, Value)>::new())
        .expect("src");
    for i in 0..INLINE_EDGE_COUNT {
        let dst = store
            .insert_vertex_named(["BenchInlineStructDst"], Vec::<(&str, Value)>::new())
            .expect("dst");
        let score = ((i % 100) as f32) / 10.0;
        let mut payload = Vec::with_capacity(usize::from(total_width));
        payload.extend_from_slice(&score.to_le_bytes());
        payload.extend_from_slice(&0.5f32.to_le_bytes());
        payload.extend_from_slice(&((i % 100) as u64).to_le_bytes());
        store
            .insert_directed_edge_with_payload_bytes(src, dst, Some(label_id), &payload)
            .expect("edge");
    }
    (src, label_id)
}

fn inline_struct_read_execution_context(
    label_id: EdgeLabelId,
    property_id: PropertyId,
) -> GqlExecutionContext {
    use gleaph_graph_kernel::plan_exec::{
        ResolvedEdgeLabel, ResolvedInlineSchema, ResolvedInlineStructField, ResolvedLabelTable,
        ResolvedProperty, ResolvedPropertyTable, ResolvedVertexLabel,
    };
    let src_label_id = crate::test_labels::vertex_label_id_for_name("BenchInlineStructSrc");
    let dst_label_id = crate::test_labels::vertex_label_id_for_name("BenchInlineStructDst");
    let total_width: u16 = 16;
    let labels = ResolvedLabelTable {
        vertex: vec![
            ResolvedVertexLabel {
                name: "BenchInlineStructSrc".to_string(),
                id: src_label_id,
            },
            ResolvedVertexLabel {
                name: "BenchInlineStructDst".to_string(),
                id: dst_label_id,
            },
        ],
        edge: vec![ResolvedEdgeLabel::with_inline_schema(
            "BenchInlineStructRoad".to_string(),
            label_id,
            EdgePayloadProfile {
                byte_width: total_width,
                encoding: EdgePayloadEncoding::RawBytes,
            },
            Some(ResolvedInlineSchema::Struct {
                property_id,
                fields: vec![
                    ResolvedInlineStructField {
                        name: "score".to_string(),
                        byte_offset: 0,
                        profile: EdgePayloadProfile {
                            byte_width: 4,
                            encoding: EdgePayloadEncoding::F32,
                        },
                    },
                    ResolvedInlineStructField {
                        name: "confidence".to_string(),
                        byte_offset: 4,
                        profile: EdgePayloadProfile {
                            byte_width: 4,
                            encoding: EdgePayloadEncoding::F32,
                        },
                    },
                    ResolvedInlineStructField {
                        name: "updated_at".to_string(),
                        byte_offset: 8,
                        profile: EdgePayloadProfile {
                            byte_width: 8,
                            encoding: EdgePayloadEncoding::RawU64,
                        },
                    },
                ],
            }),
        )],
    };
    let properties = ResolvedPropertyTable {
        properties: vec![ResolvedProperty {
            name: "stats".to_string(),
            id: property_id,
        }],
    };
    GqlExecutionContext {
        resolved_labels: Some(labels),
        resolved_properties: Some(properties),
        ..GqlExecutionContext::with_host_test_element_id_key()
    }
}

fn inline_struct_projection_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("BenchInlineStructSrc".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchInlineStructRoad".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: Some(vec!["stats".into()].into()),
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(
                Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(var("e")),
                        property: "stats".into(),
                    })),
                    property: "score".into(),
                }),
                "s",
            )],
            distinct: false,
        },
    ])
}

fn inline_struct_filter_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("BenchInlineStructSrc".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchInlineStructRoad".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Filter {
            condition: Expr::new(ExprKind::Compare {
                left: Box::new(Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(var("e")),
                        property: "stats".into(),
                    })),
                    property: "updated_at".into(),
                })),
                op: CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Uint64(7)))),
            }),
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ])
}

/// Inline STRUCT edge-property projection over a fixed out-edge set.
#[bench(raw)]
fn bench_graph_inline_struct_projection_1k() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let (_src, label_id) = setup_inline_struct_edges(&store);
    let property_id = PropertyId::from_raw(2);
    let ctx = inline_struct_read_execution_context(label_id, property_id);
    let plan = inline_struct_projection_plan();

    let probe = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        None,
        ctx.clone(),
    ))
    .expect("inline struct projection probe");
    assert_eq!(
        probe.rows.len(),
        INLINE_EDGE_COUNT as usize,
        "struct projection benchmark should emit one row per edge"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("inline_struct_projection_1k");
        let result = pollster::block_on(execute_plan_query(
            black_box(&store),
            black_box(&plan),
            &params(),
            None,
            black_box(ctx.clone()),
        ))
        .expect("inline struct projection plan");
        black_box(result.rows.len())
    })
}

/// Inline STRUCT edge-property field equality filter over a fixed out-edge set.
#[bench(raw)]
fn bench_graph_inline_struct_filter_1k() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let (_src, label_id) = setup_inline_struct_edges(&store);
    let property_id = PropertyId::from_raw(2);
    let ctx = inline_struct_read_execution_context(label_id, property_id);
    let plan = inline_struct_filter_plan();

    let probe = pollster::block_on(execute_plan_query(
        &store,
        &plan,
        &params(),
        None,
        ctx.clone(),
    ))
    .expect("inline struct filter probe");
    // Fixture uses (i % 100) for 0..999 as a u64 field, so 7 appears for i = 7, 107, ..., 907 -> 10 rows.
    assert_eq!(
        probe.rows.len(),
        10,
        "struct filter benchmark should match exactly 10 rows for updated_at % 100 = 7"
    );

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("inline_struct_filter_1k");
        let result = pollster::block_on(execute_plan_query(
            black_box(&store),
            black_box(&plan),
            &params(),
            None,
            black_box(ctx.clone()),
        ))
        .expect("inline struct filter plan");
        black_box(result.rows.len())
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

    /// The canonical-segment mutation benches must execute and persist real
    /// canonical state, so the benchmark measures the write path rather than an
    /// early error return.
    #[test]
    fn canonical_segment_insert_vertex_bench_persists() {
        let store = GraphStore::new();
        run_canonical_segment(
            store,
            &insert_vertex_mutation_plan("BenchMutVertex", vec![]),
        );
        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan(vec![
                PlanOp::NodeScan {
                    variable: "n".into(),
                    label: Some("BenchMutVertex".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![project(var("n"), "n")],
                    distinct: false,
                },
            ]),
            &params(),
            None,
            GqlExecutionContext::default(),
        ))
        .expect("read back inserted vertex");
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn canonical_segment_insert_vertex_with_property_bench_persists() {
        let store = GraphStore::new();
        run_canonical_segment(
            store,
            &insert_vertex_mutation_plan(
                "BenchMutVertexProp",
                vec![PropertyAssignment {
                    name: "weight".into(),
                    value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                }],
            ),
        );
        assert_eq!(u32::from(store.vertex_count()), 1);
    }

    #[test]
    fn canonical_segment_insert_bundle_bench_persists() {
        let store = GraphStore::new();
        run_canonical_segment(store, &insert_bundle_mutation_plan(4));
        assert_eq!(
            u32::from(store.vertex_count()),
            4,
            "the 4-statement bundle persists all four inserts in one canonical segment"
        );
    }

    #[test]
    fn canonical_segment_insert_edge_bench_persists() {
        let store = GraphStore::new();
        run_canonical_segment(store, &insert_edge_mutation_plan());
        assert_eq!(u32::from(store.vertex_count()), 2);
        let src_label = crate::test_labels::vertex_label_id_for_name("BenchMutEdgeSrc");
        let mut src = None;
        for raw in 0..u32::from(store.vertex_count()) {
            let vid = VertexId::from(raw);
            let vertex = store.vertex(vid).expect("vertex");
            if store.vertex_has_label(vid, vertex, src_label) {
                src = Some(vid);
            }
        }
        let src = src.expect("edge source vertex");
        assert_eq!(
            store.directed_out_edges(src).expect("src out edges").len(),
            1
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

    /// The local-unique benches must measure real table work, not an early return: the acquire
    /// preflight sees the value absent then claims it, and the owner-matched release frees it.
    #[test]
    fn local_unique_acquire_release_bench_fixture_mutates_table() {
        let store = GraphStore::new();
        let constraint = ConstraintNameId::from_raw(15);
        let value = local_unique_value(0);
        let owner = local_unique_owner(0);
        assert!(!store.local_unique_contains(constraint, &value));
        store.local_unique_insert(constraint, value.clone(), owner.clone());
        assert!(store.local_unique_contains(constraint, &value));
        assert!(store.local_unique_remove_if_owner(constraint, &value, &owner));
        assert!(!store.local_unique_contains(constraint, &value));
    }

    #[test]
    fn local_unique_purge_bench_fixture_drains_full_page() {
        let store = GraphStore::new();
        let constraint = ConstraintNameId::from_raw(16);
        for ordinal in 0..256u32 {
            store.local_unique_insert(
                constraint,
                local_unique_value(ordinal),
                local_unique_owner(ordinal),
            );
        }
        let progress = store.local_unique_purge(constraint, 256);
        assert_eq!(progress.removed, 256);
        assert!(progress.done);
        assert!(store.local_unique_is_empty(constraint));
    }
}

// --- ADR 0034 Slice 22 inline scalar mutation benchmarks ---

fn install_bench_inline_road(label_name: &str) -> (EdgeLabelId, PropertyId) {
    let label = crate::test_labels::edge_label_id_for_name(label_name);
    let property = crate::test_labels::property_id_for_name("distance");
    crate::test_labels::install_test_edge_payload_profile(
        label,
        EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::RawU16,
        },
    );
    crate::test_labels::install_test_edge_inline_property(label, property);
    (label, property)
}

#[bench(raw)]
fn bench_inline_scalar_pack_batch_u16() -> canbench_rs::BenchResult {
    let values: Vec<Value> = (0..256).map(Value::Int64).collect();
    let profile = EdgePayloadProfile {
        byte_width: 2,
        encoding: EdgePayloadEncoding::RawU16,
    };

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("inline_scalar_pack_batch_u16");
        let mut total = 0usize;
        for value in &values {
            let bytes = encode_edge_payload_scalar(&profile, value).expect("encode");
            total = total.wrapping_add(bytes.len());
        }
        black_box(total)
    })
}

#[bench(raw)]
fn bench_inline_scalar_set_payload_fixed_edges() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let (label, _property) = install_bench_inline_road("BenchInlineRoad");

    let mut vertices = Vec::new();
    let mut edges = Vec::new();
    for _ in 0..100 {
        let src = insert_bench_vertex_named(&store, &["BenchInlineCity"]);
        let dst = insert_bench_vertex_named(&store, &["BenchInlineCity"]);
        let handle = GraphMutationExecutor::insert_directed_edge_with_payload_bytes(
            &store,
            src,
            dst,
            Some(label),
            &1u16.to_le_bytes(),
            Vec::new(),
        )
        .expect("insert bench edge");
        vertices.push((src, dst));
        edges.push(handle);
    }

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("inline_scalar_set_payload_fixed_edges");
        let mut i = 0u16;
        for handle in &edges {
            let bytes = (i.wrapping_add(1)).to_le_bytes();
            store
                .update_edge_payload_at_handle(*handle, &bytes)
                .expect("update payload");
            i = i.wrapping_add(1);
        }
        black_box(i)
    })
}
