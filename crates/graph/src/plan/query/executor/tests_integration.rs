//! Cross-cutting planner integration tests.

use super::test_support::*;
use gleaph_gql::Value;
use gleaph_graph_kernel::plan_exec::{ResolvedLabelTable, ResolvedPropertyTable};

#[test]
fn explicit_empty_resolved_table_fails_labeled_node_scan() {
    let store = GraphStore::new();
    let plan = plan_gql("MATCH (n:MissingNodeLabel) RETURN n");
    let err = store
        .execute_plan_query(
            &plan,
            &params(),
            GqlExecutionContext {
                caller: None,
                resolved_labels: Some(ResolvedLabelTable::default()),
                resolved_properties: Some(ResolvedPropertyTable::default()),
                element_id_encoding_key: None,
                unique_claims: Vec::new(),
                constrained_properties: Vec::new(),
            },
        )
        .expect_err("missing resolved node label must fail");
    assert!(matches!(
        err,
        PlanQueryError::MissingResolvedLabel {
            namespace: "node",
            name
        } if name == "MissingNodeLabel"
    ));
}

#[test]
fn explicit_empty_resolved_table_fails_labeled_expand() {
    let store = GraphStore::new();
    let a = store.insert_vertex().expect("insert source");
    let mut row = PlanRow::new();
    row.insert("a".to_owned(), PlanBinding::Vertex(a));
    let parameters = params();
    let ctx = ExecuteCtx::new(
        &store,
        &parameters,
        None,
        GqlExecutionContext {
            caller: None,
            resolved_labels: Some(ResolvedLabelTable::default()),
            resolved_properties: Some(ResolvedPropertyTable::default()),
            element_id_encoding_key: None,
            unique_claims: Vec::new(),
            constrained_properties: Vec::new(),
        },
        None,
    );
    let err = pollster::block_on(execute_expand(
        &ctx,
        vec![row],
        &"a".into(),
        &"e".into(),
        &"b".into(),
        EdgeDirection::PointingRight,
        Some("MissingEdgeLabel"),
        None,
        &ctx.execution,
        EdgeSequenceOrder::Descending,
        &[],
        true,
        None,
        None,
        None,
        None,
        None,
        None,
    ))
    .expect_err("missing resolved edge label must fail");
    assert!(matches!(
        err,
        PlanQueryError::MissingResolvedLabel {
            namespace: "edge",
            name
        } if name == "MissingEdgeLabel"
    ));
}

#[test]
fn mandatory_node_only_match_after_optional_miss_drops_null_rows() {
    let store = GraphStore::new();
    store
        .insert_vertex_named(["OptLabelA"], Vec::<(&str, Value)>::new())
        .expect("insert a");
    store
        .insert_vertex_named(["OptLabelB"], Vec::<(&str, Value)>::new())
        .expect("insert b");
    let gql = "MATCH (a:OptLabelA) OPTIONAL MATCH (a)-[e:OptLabelRel]->(b:OptLabelB) \
               MATCH (b:OptLabelB) RETURN b";
    let plan = plan_gql(gql);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("mandatory node-only match after optional miss");
    assert!(
        result.rows.is_empty(),
        "null optional binding must fail mandatory labeled node match: {:?}",
        result.rows
    );
}

#[test]
fn optional_miss_fails_labeled_node_only_match() {
    let store = GraphStore::new();
    store
        .insert_vertex_named(["OptMissA"], Vec::<(&str, Value)>::new())
        .expect("insert a");
    crate::test_labels::edge_label_id_for_name("OptMissRel");
    let gql = "MATCH (a:OptMissA) OPTIONAL MATCH (a)-[e:OptMissRel]->(b:OptMissB) \
               MATCH (b:OptMissB) RETURN b";
    let plan = plan_gql(gql);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("optional miss labeled match");
    assert!(
        result.rows.is_empty(),
        "optional miss must drop rows on mandatory labeled node-only match: {:?}",
        result.rows
    );
}
