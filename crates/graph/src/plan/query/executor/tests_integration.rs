//! Cross-cutting planner integration tests.

use super::test_support::*;
use gleaph_gql::Value;

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
    store
        .get_or_insert_edge_label_id("OptMissRel")
        .expect("edge label");
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
