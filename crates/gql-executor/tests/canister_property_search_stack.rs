//! Stack contract for **indexed property reads** on a canister-shaped backend:
//!
//! - `IndexScan` / `IndexIntersection`: `TableStats` → `build_plan` → `execute_plan` →
//!   `GraphRead::scan_nodes_by_property`
//! - **Indexed edge equality on `Expand` / `ExpandFilter`**: when the source node is already
//!   constrained by a useful scan (e.g. labeled `NodeScan` / `IndexScan`) and stats mark the edge
//!   property, the planner sets `indexed_edge_equality` and the executor calls
//!   `GraphRead::scan_edges_by_property` while preserving source-node bindings.
//! - **Leading `EdgeIndexScan` + `EdgeBindEndpoints`**: when the pattern starts with an unlabeled
//!   node that would only warrant a full vertex scan, the planner may instead open with
//!   `EdgeIndexScan` then `EdgeBindEndpoints` (see `gql-planner` + design doc).
//! - **`EdgeIndexScan` alone**: also covered by a **manual** `PhysicalPlan` for callers that emit
//!   it without endpoint binding.
//! - **`WorstCaseOptimalJoin` + `e1__hop_aux`**: cyclic patterns may fuse to WCOJ; when `RETURN`
//!   references `{edge}__hop_aux`, the plan carries `hop_aux_binding` on the corresponding
//!   `WcojEdge` and the executor binds scalars via `hop_aux_bytes_for_edge`.
//!
//! Larger design text: `docs/graph-store-target-design.md` (`Internet Computer: full property search stack`).

use std::collections::BTreeMap;
use std::rc::Rc;

use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind, Statement};
use gleaph_gql::parser;
use gleaph_gql_executor::{ExecutionContext, execute_plan, execute_plan_with_context};
use gleaph_gql_planner::plan::{
    PhysicalPlan, PlanAnnotations, PlanDiagnostics, ProjectColumn, ScanValue,
};
use gleaph_gql_planner::stats::TableStats;
use gleaph_gql_planner::{PlanOp, build_plan};
use gleaph_graph_kernel::{GraphWrite, NodeId, PropertyMap};
use gleaph_graph_store::low_level::BucketSizeInPages;
use gleaph_graph_store::{GraphStore, GraphStoreVecMemory};

fn linear_query_from_str(input: &str) -> gleaph_gql::ast::LinearQueryStatement {
    let program = parser::parse(input).expect("parse");
    let tx = program.transaction_activity.expect("transaction_activity");
    let block = tx.body.expect("block body");
    match &block.first {
        Statement::Query(composite) => composite.left.clone(),
        other => panic!("expected query statement, got {other:?}"),
    }
}

fn exec_ctx_bind_planner_uid_param(value: impl Into<String>) -> ExecutionContext {
    // `IndexScan` lowers equality literals on indexed properties to parameters (see planner).
    // Canister / Gleaph callers must supply the same binding when executing.
    let mut ctx = ExecutionContext::default();
    ctx.params
        .insert("uid".to_owned(), Value::Text(value.into()));
    ctx.caller = None;
    ctx
}

fn stats_indexed_uid_person() -> TableStats {
    let mut stats = TableStats::default();
    stats.indexed_vertex_properties.insert("uid".to_string());
    stats.property_selectivity.insert("uid".to_string(), 0.001);
    stats
        .label_cardinality
        .insert("Person".to_string(), 100_000);
    stats
}

fn stats_user_with_indexed_edge_weight() -> TableStats {
    let mut stats = TableStats::default();
    stats.label_cardinality.insert("User".to_string(), 10_000);
    stats.indexed_edge_properties.insert("weight".to_string());
    stats
        .property_selectivity
        .insert("weight".to_string(), 0.05);
    stats.avg_degree = 8.0;
    stats
}

#[test]
fn indexed_person_uid_planner_emits_index_scan_and_executor_finds_node_via_graph_pma() {
    let mem_rc = Rc::new(GraphStoreVecMemory::default());
    let mut facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
        BucketSizeInPages::DEFAULT,
        Rc::clone(&mem_rc),
    )
    .expect("bootstrap");
    let mut graph = facade.bind_kernel_overlay(mem_rc.as_ref());

    let labels = vec!["Person".to_owned()];
    let mut props: PropertyMap = BTreeMap::new();
    props.insert("uid".to_owned(), Value::Text("alice".into()));
    let alice = graph
        .insert_node(&labels, &props)
        .expect("insert node with uid");

    let q = linear_query_from_str("MATCH (n:Person) WHERE n.uid = 'alice' RETURN n");
    let stats = stats_indexed_uid_person();
    let plan = build_plan(&q, Some(&stats)).expect("build_plan");

    assert!(
        plan.ops
            .iter()
            .any(|op| matches!(op, PlanOp::IndexScan { .. })),
        "with indexed `uid` in stats, expected IndexScan in {:?}",
        plan.ops
    );

    let ctx = exec_ctx_bind_planner_uid_param("alice");
    let out = execute_plan_with_context(&mut graph, &plan, &ctx).expect("execute_plan");
    assert_eq!(out.rows.len(), 1, "single equality match");

    let n = out.rows[0]
        .get("n")
        .expect("RETURN n should produce column n");
    let Value::Record(fields) = n else {
        panic!("expected Record for node projection, got {n:?}");
    };
    let id_val = fields
        .iter()
        .find(|(k, _)| k == "id")
        .map(|(_, v)| v)
        .expect("id field");
    let Value::Uint64(id) = id_val else {
        panic!("expected Uint64 id, got {id_val:?}");
    };
    assert_eq!(NodeId::try_from(*id).expect("id fits"), alice.id);
    let uid_val = fields
        .iter()
        .find(|(k, _)| k == "uid")
        .map(|(_, v)| v)
        .expect("uid field");
    assert_eq!(uid_val, &Value::Text("alice".into()));
}

#[test]
fn index_scan_without_index_in_stats_falls_back_to_node_scan_but_results_match() {
    let mem_rc = Rc::new(GraphStoreVecMemory::default());
    let mut facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
        BucketSizeInPages::DEFAULT,
        Rc::clone(&mem_rc),
    )
    .expect("bootstrap");
    let mut graph = facade.bind_kernel_overlay(mem_rc.as_ref());

    let labels = vec!["Person".to_owned()];
    let mut props: PropertyMap = BTreeMap::new();
    props.insert("uid".to_owned(), Value::Text("bob".into()));
    let bob = graph.insert_node(&labels, &props).expect("insert node");

    let q = linear_query_from_str("MATCH (n:Person) WHERE n.uid = 'bob' RETURN n");

    let stats_empty = TableStats::default();
    let plan_no_index = build_plan(&q, Some(&stats_empty)).expect("plan");
    assert!(
        plan_no_index
            .ops
            .iter()
            .any(|op| matches!(op, PlanOp::NodeScan { .. })),
        "without indexed stats, expected NodeScan, got {:?}",
        plan_no_index.ops
    );
    let out_scan = execute_plan(&mut graph, &plan_no_index).expect("execute");

    let stats_uid = stats_indexed_uid_person();
    let plan_index = build_plan(&q, Some(&stats_uid)).expect("plan indexed");
    assert!(
        plan_index
            .ops
            .iter()
            .any(|op| matches!(op, PlanOp::IndexScan { .. })),
        "with stats, expected IndexScan, got {:?}",
        plan_index.ops
    );
    let ctx = exec_ctx_bind_planner_uid_param("bob");
    let out_index =
        execute_plan_with_context(&mut graph, &plan_index, &ctx).expect("execute index path");

    assert_eq!(out_scan.rows, out_index.rows, "index vs scan must agree");
    assert_eq!(out_scan.rows.len(), 1);
    let n = &out_index.rows[0]["n"];
    let Value::Record(fields) = n else {
        panic!("expected Record, got {n:?}");
    };
    let Value::Uint64(id) = fields
        .iter()
        .find(|(k, _)| k == "id")
        .map(|(_, v)| v)
        .expect("id")
    else {
        panic!("expected id");
    };
    assert_eq!(NodeId::try_from(*id).expect("id"), bob.id);
}

#[test]
fn index_intersection_planner_and_executor_agree_on_pma_overlay() {
    let mem_rc = Rc::new(GraphStoreVecMemory::default());
    let mut facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
        BucketSizeInPages::DEFAULT,
        Rc::clone(&mem_rc),
    )
    .expect("bootstrap");
    let mut graph = facade.bind_kernel_overlay(mem_rc.as_ref());

    let labels = vec!["User".to_owned()];

    let mut full = PropertyMap::new();
    full.insert("uid".to_owned(), Value::Text("alice".into()));
    full.insert("email".to_owned(), Value::Text("alice@example.com".into()));
    let alice = graph.insert_node(&labels, &full).expect("insert alice");

    let mut partial = PropertyMap::new();
    partial.insert("uid".to_owned(), Value::Text("alice".into()));
    partial.insert("email".to_owned(), Value::Text("other@example.com".into()));
    graph.insert_node(&labels, &partial).expect("insert decoy");

    let mut stats = TableStats::default();
    stats.label_cardinality.insert("User".to_string(), 10_000);
    stats.indexed_vertex_properties.insert("uid".to_string());
    stats.indexed_vertex_properties.insert("email".to_string());

    let q = linear_query_from_str(
        "MATCH (n:User WHERE n.uid = 'alice' AND n.email = 'alice@example.com') RETURN n",
    );
    let plan = build_plan(&q, Some(&stats)).expect("plan");
    assert!(
        plan.ops
            .iter()
            .any(|op| matches!(op, PlanOp::IndexIntersection { .. })),
        "expected IndexIntersection in {:?}",
        plan.ops
    );

    let out = execute_plan(&mut graph, &plan).expect("execute");
    assert_eq!(out.rows.len(), 1);
    let Value::Record(fields) = &out.rows[0]["n"] else {
        panic!("expected node record");
    };
    let Value::Uint64(id) = fields
        .iter()
        .find(|(k, _)| k == "id")
        .map(|(_, v)| v)
        .expect("id")
    else {
        panic!("id field");
    };
    assert_eq!(NodeId::try_from(*id).expect("id"), alice.id);
}

#[test]
fn indexed_edge_expand_planner_path_runs_on_pma_overlay() {
    let mem_rc = Rc::new(GraphStoreVecMemory::default());
    let mut facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
        BucketSizeInPages::DEFAULT,
        Rc::clone(&mem_rc),
    )
    .expect("bootstrap");
    let mut graph = facade.bind_kernel_overlay(mem_rc.as_ref());

    let labels = vec!["User".to_owned()];
    let empty = PropertyMap::new();
    let a = graph.insert_node(&labels, &empty).expect("a");
    let b = graph.insert_node(&labels, &empty).expect("b");
    let mut eprops = PropertyMap::new();
    eprops.insert("weight".to_owned(), Value::Int64(7));
    graph
        .insert_edge(a.id, b.id, Some("KNOWS"), &eprops, false)
        .expect("edge");

    let q = linear_query_from_str("MATCH (a:User)-[e:KNOWS {weight: 7}]->(b:User) RETURN e");
    let stats = stats_user_with_indexed_edge_weight();
    let plan = build_plan(&q, Some(&stats)).expect("build_plan");

    assert!(
        plan.ops.iter().any(|op| {
            matches!(
                op,
                PlanOp::Expand {
                    indexed_edge_equality: Some(_),
                    ..
                } | PlanOp::ExpandFilter {
                    indexed_edge_equality: Some(_),
                    ..
                }
            )
        }),
        "expected Expand/ExpandFilter with indexed_edge_equality, got {:?}",
        plan.ops
    );

    let out = execute_plan(&mut graph, &plan).expect("execute indexed edge expand");
    assert_eq!(out.rows.len(), 1, "single edge match");
    let e = out.rows[0].get("e").expect("RETURN e");
    let Value::Record(fields) = e else {
        panic!("expected edge record, got {e:?}");
    };
    assert_eq!(
        fields.iter().find(|(k, _)| k == "weight").map(|(_, v)| v),
        Some(&Value::Int64(7))
    );
}

#[test]
fn indexed_edge_expand_matches_scan_filter_semantics() {
    let mem_rc = Rc::new(GraphStoreVecMemory::default());
    let mut facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
        BucketSizeInPages::DEFAULT,
        Rc::clone(&mem_rc),
    )
    .expect("bootstrap");
    let mut graph = facade.bind_kernel_overlay(mem_rc.as_ref());

    let labels = vec!["User".to_owned()];
    let empty = PropertyMap::new();
    let a = graph.insert_node(&labels, &empty).expect("a");
    let b = graph.insert_node(&labels, &empty).expect("b");
    let c = graph.insert_node(&labels, &empty).expect("c");

    let mut w7 = PropertyMap::new();
    w7.insert("weight".to_owned(), Value::Int64(7));
    graph
        .insert_edge(a.id, b.id, Some("KNOWS"), &w7, false)
        .expect("edge 7");

    let mut w3 = PropertyMap::new();
    w3.insert("weight".to_owned(), Value::Int64(3));
    graph
        .insert_edge(a.id, c.id, Some("KNOWS"), &w3, false)
        .expect("edge 3");

    let q = linear_query_from_str("MATCH (a:User)-[e:KNOWS {weight: 7}]->(b:User) RETURN e, b");

    let indexed_stats = stats_user_with_indexed_edge_weight();
    let indexed_plan = build_plan(&q, Some(&indexed_stats)).expect("build indexed plan");
    let indexed_out = execute_plan(&mut graph, &indexed_plan).expect("execute indexed");

    let no_index_stats = TableStats::default();
    let scan_plan = build_plan(&q, Some(&no_index_stats)).expect("build scan plan");
    let scan_out = execute_plan(&mut graph, &scan_plan).expect("execute scan");

    assert_eq!(
        indexed_out.rows, scan_out.rows,
        "indexed-edge-equality path must match scan+filter semantics"
    );
}

#[test]
fn leading_edge_index_scan_planner_path_runs_on_pma_overlay() {
    let mem_rc = Rc::new(GraphStoreVecMemory::default());
    let mut facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
        BucketSizeInPages::DEFAULT,
        Rc::clone(&mem_rc),
    )
    .expect("bootstrap");
    let mut graph = facade.bind_kernel_overlay(mem_rc.as_ref());

    let empty = PropertyMap::new();
    let user_labels = vec!["User".to_owned()];
    let a = graph
        .insert_node(&[], &empty)
        .expect("unlabeled start node");
    let b = graph.insert_node(&user_labels, &empty).expect("b:User");
    let mut eprops = PropertyMap::new();
    eprops.insert("weight".to_owned(), Value::Int64(7));
    graph
        .insert_edge(a.id, b.id, Some("KNOWS"), &eprops, false)
        .expect("edge");

    let q = linear_query_from_str("MATCH ()-[e:KNOWS {weight: 7}]->(b:User) RETURN e, b");
    let stats = stats_user_with_indexed_edge_weight();
    let plan = build_plan(&q, Some(&stats)).expect("build_plan");

    assert!(
        matches!(
            plan.ops.first(),
            Some(PlanOp::EdgeIndexScan {
                property,
                value,
                ..
            }) if &**property == "weight" && matches!(value, ScanValue::Literal(Value::Int64(7)))
        ),
        "expected leading EdgeIndexScan(weight=7), got {:?}",
        plan.ops
    );
    assert!(
        matches!(plan.ops.get(1), Some(PlanOp::EdgeBindEndpoints { .. })),
        "expected EdgeBindEndpoints after EdgeIndexScan, got {:?}",
        plan.ops
    );

    let out = execute_plan(&mut graph, &plan).expect("execute leading edge index plan");
    assert_eq!(out.rows.len(), 1, "single path match");

    let e = out.rows[0].get("e").expect("RETURN e");
    let Value::Record(efields) = e else {
        panic!("expected edge record, got {e:?}");
    };
    assert_eq!(
        efields.iter().find(|(k, _)| k == "weight").map(|(_, v)| v),
        Some(&Value::Int64(7))
    );

    let bval = out.rows[0].get("b").expect("RETURN b");
    let Value::Record(bfields) = bval else {
        panic!("expected node record, got {bval:?}");
    };
    let Value::List(label_vals) = bfields
        .iter()
        .find(|(k, _)| k == "labels")
        .map(|(_, v)| v)
        .expect("labels field")
    else {
        panic!("expected labels list on b");
    };
    assert!(
        label_vals
            .iter()
            .any(|v| *v == Value::Text("User".to_owned())),
        "b should be :User, labels={label_vals:?}"
    );
    let Value::Uint64(id) = bfields
        .iter()
        .find(|(k, _)| k == "id")
        .map(|(_, v)| v)
        .expect("id")
    else {
        panic!("id field");
    };
    assert_eq!(NodeId::try_from(*id).expect("id"), b.id);
}

#[test]
fn leading_edge_bind_hop_aux_is_demand_driven_on_pma_overlay() {
    let mem_rc = Rc::new(GraphStoreVecMemory::default());
    let mut facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
        BucketSizeInPages::DEFAULT,
        Rc::clone(&mem_rc),
    )
    .expect("bootstrap");
    let mut graph = facade.bind_kernel_overlay(mem_rc.as_ref());

    let empty = PropertyMap::new();
    let user_labels = vec!["User".to_owned()];
    let a = graph
        .insert_node(&[], &empty)
        .expect("unlabeled start node");
    let b = graph.insert_node(&user_labels, &empty).expect("b:User");
    let mut eprops = PropertyMap::new();
    eprops.insert("weight".to_owned(), Value::Int64(7));
    graph
        .insert_edge(a.id, b.id, Some("KNOWS"), &eprops, false)
        .expect("edge");

    let stats = stats_user_with_indexed_edge_weight();

    let q_no_aux = linear_query_from_str("MATCH ()-[e:KNOWS {weight: 7}]->(b:User) RETURN e, b");
    let plan_no = build_plan(&q_no_aux, Some(&stats)).expect("build_plan");
    let Some(PlanOp::EdgeBindEndpoints {
        hop_aux_binding: hop_none,
        ..
    }) = plan_no.ops.get(1)
    else {
        panic!("expected EdgeBindEndpoints, ops={:?}", plan_no.ops);
    };
    assert!(
        hop_none.is_none(),
        "plan should omit hop_aux when not referenced, got {hop_none:?}"
    );
    let out_no = execute_plan(&mut graph, &plan_no).expect("execute");
    assert_eq!(out_no.rows.len(), 1);
    assert!(
        !out_no.rows[0].contains_key("e__hop_aux"),
        "output should not include e__hop_aux when not projected"
    );

    let q_aux =
        linear_query_from_str("MATCH ()-[e:KNOWS {weight: 7}]->(b:User) RETURN e, e__hop_aux, b");
    let plan_aux = build_plan(&q_aux, Some(&stats)).expect("build_plan");
    let Some(PlanOp::EdgeBindEndpoints {
        hop_aux_binding: hop_some,
        ..
    }) = plan_aux.ops.get(1)
    else {
        panic!("expected EdgeBindEndpoints, ops={:?}", plan_aux.ops);
    };
    assert_eq!(
        hop_some.as_deref(),
        Some("e__hop_aux"),
        "plan should bind hop_aux when referenced, got {hop_some:?}"
    );
    let out_aux = execute_plan(&mut graph, &plan_aux).expect("execute with hop_aux");
    assert_eq!(out_aux.rows.len(), 1);
    let aux_val = out_aux.rows[0]
        .get("e__hop_aux")
        .expect("RETURN e__hop_aux should appear in output");
    assert!(
        matches!(aux_val, Value::Null | Value::Bytes(_)),
        "local PMA overlay has no shard principal; expect Null or Bytes, got {aux_val:?}"
    );
}

#[test]
fn wcoj_triangle_hop_aux_on_pma_overlay() {
    let mem_rc = Rc::new(GraphStoreVecMemory::default());
    let mut facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
        BucketSizeInPages::DEFAULT,
        Rc::clone(&mem_rc),
    )
    .expect("bootstrap");
    let mut graph = facade.bind_kernel_overlay(mem_rc.as_ref());

    let labels = vec!["Person".to_owned()];
    let empty = PropertyMap::new();
    let n1 = graph.insert_node(&labels, &empty).expect("n1");
    let n2 = graph.insert_node(&labels, &empty).expect("n2");
    let n3 = graph.insert_node(&labels, &empty).expect("n3");
    graph
        .insert_edge(n1.id, n2.id, Some("KNOWS"), &empty, false)
        .expect("e12");
    graph
        .insert_edge(n2.id, n3.id, Some("KNOWS"), &empty, false)
        .expect("e23");
    graph
        .insert_edge(n3.id, n1.id, Some("KNOWS"), &empty, false)
        .expect("e31");

    let q = linear_query_from_str(
        "MATCH (a:Person)-[e1:KNOWS]->(b:Person)-[e2:KNOWS]->(c:Person)-[e3:KNOWS]->(a) RETURN a, e1__hop_aux",
    );
    let plan = build_plan(&q, None).expect("build_plan");
    assert!(
        plan.ops
            .iter()
            .any(|op| matches!(op, PlanOp::WorstCaseOptimalJoin { .. })),
        "expected WorstCaseOptimalJoin, ops={:?}",
        plan.ops
    );
    let wcoj_edges = plan.ops.iter().find_map(|op| match op {
        PlanOp::WorstCaseOptimalJoin { edges, .. } => Some(edges.as_slice()),
        _ => None,
    });
    let e1 = wcoj_edges
        .expect("wcoj edges")
        .iter()
        .find(|e| &*e.variable == "e1")
        .expect("e1");
    assert_eq!(e1.hop_aux_binding.as_deref(), Some("e1__hop_aux"));

    let out = execute_plan(&mut graph, &plan).expect("execute wcoj triangle");
    assert!(
        !out.rows.is_empty(),
        "triangle should produce at least one row, got {:?}",
        out.rows
    );
    for row in &out.rows {
        let aux = row
            .get("e1__hop_aux")
            .expect("RETURN e1__hop_aux should appear in output");
        assert!(
            matches!(aux, Value::Null | Value::Bytes(_)),
            "unexpected hop_aux value {aux:?}"
        );
    }
}

#[test]
fn edge_index_scan_manual_plan_runs_on_pma_overlay() {
    let mem_rc = Rc::new(GraphStoreVecMemory::default());
    let mut facade = GraphStore::bootstrap_empty_with_bucket_size_using_memory_rc(
        BucketSizeInPages::DEFAULT,
        Rc::clone(&mem_rc),
    )
    .expect("bootstrap");
    let mut graph = facade.bind_kernel_overlay(mem_rc.as_ref());

    let labels = vec!["User".to_owned()];
    let empty = PropertyMap::new();
    let a = graph.insert_node(&labels, &empty).expect("a");
    let b = graph.insert_node(&labels, &empty).expect("b");
    let mut eprops = PropertyMap::new();
    eprops.insert("weight".to_owned(), Value::Int64(7));
    graph
        .insert_edge(a.id, b.id, Some("KNOWS"), &eprops, false)
        .expect("edge");

    let plan = PhysicalPlan {
        ops: vec![
            PlanOp::EdgeIndexScan {
                variable: "e".into(),
                property: "weight".into(),
                value: ScanValue::Literal(Value::Int64(7)),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("e".to_string())),
                    alias: None,
                }],
                distinct: false,
            },
        ],
        diagnostics: PlanDiagnostics::default(),
        annotations: PlanAnnotations::default(),
    };

    let out = execute_plan(&mut graph, &plan).expect("execute edge index scan");
    assert_eq!(out.rows.len(), 1);
    let e = &out.rows[0]["e"];
    let Value::Record(fields) = e else {
        panic!("expected edge record, got {e:?}");
    };
    assert_eq!(
        fields.iter().find(|(k, _)| k == "weight").map(|(_, v)| v),
        Some(&Value::Int64(7))
    );
}
