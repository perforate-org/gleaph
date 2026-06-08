use super::super::test_support::*;
use super::predicates::{PreparedEdgePayloadPredicate, PreparedEdgeVectorThreshold};
use gleaph_gql_planner::plan::{EdgePayloadPredicate, EdgeVectorMetric, EdgeVectorPredicate};
use pollster;

#[test]
fn federated_reverse_expand_from_remote_vertex_binding() {
    let store = GraphStore::new();
    configure_test_federation(&store);
    let source = store.insert_vertex().expect("source");
    let source_logical = store.logical_vertex_id(source).expect("logical");
    let remote_logical = 88_001u64;
    store
        .insert_directed_edge_to_logical(source, remote_logical, None)
        .expect("remote edge");

    let mut seed = PlanRow::new();
    seed.insert("b".to_owned(), PlanBinding::RemoteVertex(remote_logical));

    let parameters = params();
    let ctx = ExecuteCtx::new(
        &store,
        &parameters,
        None,
        GqlExecutionContext::default(),
        None,
    );
    let out = pollster::block_on(execute_expand(
        &ctx,
        vec![seed],
        &"b".into(),
        &"e".into(),
        &"a".into(),
        EdgeDirection::PointingLeft,
        None,
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
    ))
    .expect("federated reverse expand");

    assert_eq!(out.len(), 1);
    assert!(matches!(
        out[0].get("a"),
        Some(PlanBinding::Vertex(v)) if *v == source
    ));
    assert_eq!(
        store.logical_vertex_id(source).expect("source logical"),
        source_logical
    );
}

#[test]
fn executes_planner_one_hop_expand() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(
            ["PlannerQueryExpandSource"],
            [("name", Value::Text("Planner Expand Alice".into()))],
        )
        .expect("insert source");
    let b = store
        .insert_vertex_named(
            ["PlannerQueryExpandTarget"],
            [("name", Value::Text("Planner Expand Bob".into()))],
        )
        .expect("insert target");
    let unrelated = store
        .insert_vertex_named(
            ["PlannerQueryExpandTarget"],
            [("name", Value::Text("Planner Expand Carol".into()))],
        )
        .expect("insert unrelated target");
    store
        .insert_directed_edge_named(
            a,
            b,
            Some("PlannerQueryKnows"),
            [("since", Value::Int64(2026))],
        )
        .expect("insert matching edge");
    store
        .insert_directed_edge_named(
            a,
            unrelated,
            Some("PlannerQueryIgnores"),
            [("since", Value::Int64(2025))],
        )
        .expect("insert non-matching edge");
    let plan = plan_gql(
        "MATCH (a:PlannerQueryExpandSource)-[e:PlannerQueryKnows]->(b:PlannerQueryExpandTarget) \
             RETURN a.name AS a_name, b.name AS b_name, e.since AS since",
    );

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute planned query");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("a_name"),
        Some(&Value::Text("Planner Expand Alice".into()))
    );
    assert_eq!(
        result.rows[0].get("b_name"),
        Some(&Value::Text("Planner Expand Bob".into()))
    );
    assert_eq!(result.rows[0].get("since"), Some(&Value::Int64(2026)));
}

#[test]
fn executes_planner_union_label_expr_expand() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["UnionLabelExprSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    let knows_target = store
        .insert_vertex_named(
            ["UnionLabelExprTarget"],
            [("name", Value::Text("Knows Bob".into()))],
        )
        .expect("insert knows target");
    let likes_target = store
        .insert_vertex_named(
            ["UnionLabelExprTarget"],
            [("name", Value::Text("Likes Carol".into()))],
        )
        .expect("insert likes target");
    let hates_target = store
        .insert_vertex_named(
            ["UnionLabelExprTarget"],
            [("name", Value::Text("Hates Dave".into()))],
        )
        .expect("insert hates target");
    store
        .insert_directed_edge_named(
            a,
            knows_target,
            Some("UnionLabelExprKnows"),
            Vec::<(&str, Value)>::new(),
        )
        .expect("knows edge");
    store
        .insert_directed_edge_named(
            a,
            likes_target,
            Some("UnionLabelExprLikes"),
            Vec::<(&str, Value)>::new(),
        )
        .expect("likes edge");
    store
        .insert_directed_edge_named(
            a,
            hates_target,
            Some("UnionLabelExprHates"),
            Vec::<(&str, Value)>::new(),
        )
        .expect("hates edge");

    let plan = plan_gql(
        "MATCH (a:UnionLabelExprSource)-/UnionLabelExprKnows|UnionLabelExprLikes/->\
             (b:UnionLabelExprTarget) RETURN b.name AS b_name ORDER BY b_name",
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute union label_expr expand");

    assert_eq!(result.rows.len(), 2);
    assert_eq!(
        result.rows[0].get("b_name"),
        Some(&Value::Text("Knows Bob".into()))
    );
    assert_eq!(
        result.rows[1].get("b_name"),
        Some(&Value::Text("Likes Carol".into()))
    );
}

#[test]
fn union_label_expr_edge_payload_predicate_fuses_per_label() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["UnionPayloadFusionA"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    let knows_match = store
        .insert_vertex_named(
            ["UnionPayloadFusionTarget"],
            [("name", Value::Text("Knows match".into()))],
        )
        .expect("knows match");
    let knows_skip = store
        .insert_vertex_named(
            ["UnionPayloadFusionTarget"],
            [("name", Value::Text("Knows skip".into()))],
        )
        .expect("knows skip");
    let likes_match = store
        .insert_vertex_named(
            ["UnionPayloadFusionTarget"],
            [("name", Value::Text("Likes match".into()))],
        )
        .expect("likes match");
    let likes_skip = store
        .insert_vertex_named(
            ["UnionPayloadFusionTarget"],
            [("name", Value::Text("Likes skip".into()))],
        )
        .expect("likes skip");
    let knows_label = crate::test_labels::edge_label_id_for_name("UnionPayloadFusionKnows");
    let likes_label = crate::test_labels::edge_label_id_for_name("UnionPayloadFusionLikes");
    for label_id in [knows_label, likes_label] {
        store
            .install_edge_label_payload_profile_at_init(
                label_id,
                EdgePayloadProfile {
                    byte_width: 2,
                    encoding: EdgePayloadEncoding::WeightRawU16,
                },
            )
            .unwrap();
    }
    store
        .insert_directed_edge_with_payload_bytes(
            a,
            knows_match,
            Some(knows_label),
            &7u16.to_le_bytes(),
        )
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(
            a,
            knows_skip,
            Some(knows_label),
            &9u16.to_le_bytes(),
        )
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(
            a,
            likes_match,
            Some(likes_label),
            &7u16.to_le_bytes(),
        )
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(
            a,
            likes_skip,
            Some(likes_label),
            &9u16.to_le_bytes(),
        )
        .unwrap();

    let label_expr = LabelExpr::Or(
        Box::new(LabelExpr::Name("UnionPayloadFusionKnows".into())),
        Box::new(LabelExpr::Name("UnionPayloadFusionLikes".into())),
    );
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("UnionPayloadFusionA".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: None,
            label_expr: Some(label_expr),
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: Some(EdgePayloadPredicate {
                op: CmpOp::Eq,
                value: ScanValue::Literal(Value::Int64(7)),
            }),
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute union label_expr payload fusion");

    assert_eq!(result.rows.len(), 2);
}

#[test]
fn executes_planner_var_len_expand() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["VarLenSource"], Vec::<(&str, Value)>::new())
        .expect("insert a");
    let b = store
        .insert_vertex_named(["VarLenMid"], Vec::<(&str, Value)>::new())
        .expect("insert b");
    let c = store
        .insert_vertex_named(["VarLenTarget"], Vec::<(&str, Value)>::new())
        .expect("insert c");
    store
        .insert_directed_edge_named(a, b, Some("VarLenRel"), Vec::<(&str, Value)>::new())
        .expect("a->b");
    store
        .insert_directed_edge_named(b, c, Some("VarLenRel"), Vec::<(&str, Value)>::new())
        .expect("b->c");
    store
        .insert_directed_edge_named(a, c, Some("VarLenRel"), Vec::<(&str, Value)>::new())
        .expect("a->c");

    let one_hop = plan_gql("MATCH (a:VarLenSource)-[:VarLenRel]->{1,1}(x) RETURN x");
    let one_hop_rows = store
        .execute_plan_query(&one_hop, &params(), GqlExecutionContext::default())
        .expect("one hop expand")
        .rows;
    assert_eq!(one_hop_rows.len(), 2);

    let two_hop = plan_gql("MATCH (a:VarLenSource)-[:VarLenRel]->{2,2}(x) RETURN x");
    let two_hop_rows = store
        .execute_plan_query(&two_hop, &params(), GqlExecutionContext::default())
        .expect("two hop expand")
        .rows;
    assert_eq!(two_hop_rows.len(), 1);
}

#[test]
fn executes_planner_expand_filter() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(
            ["PlannerQueryExpandFilterSource"],
            [("name", Value::Text("Planner EF A".into()))],
        )
        .expect("insert source");
    let keep = store
        .insert_vertex_named(
            ["PlannerQueryExpandFilterTarget"],
            [
                ("name", Value::Text("Planner EF Keep".into())),
                ("age", Value::Int64(30)),
            ],
        )
        .expect("insert keep target");
    let drop = store
        .insert_vertex_named(
            ["PlannerQueryExpandFilterTarget"],
            [
                ("name", Value::Text("Planner EF Drop".into())),
                ("age", Value::Int64(12)),
            ],
        )
        .expect("insert drop target");
    store
        .insert_directed_edge_named(
            a,
            keep,
            Some("PlannerQueryExpandFilterRel"),
            Vec::<(&str, Value)>::new(),
        )
        .expect("insert keep edge");
    store
        .insert_directed_edge_named(
            a,
            drop,
            Some("PlannerQueryExpandFilterRel"),
            Vec::<(&str, Value)>::new(),
        )
        .expect("insert drop edge");
    let plan = plan_gql(
        "MATCH (a:PlannerQueryExpandFilterSource)-[e:PlannerQueryExpandFilterRel]->\
             (b:PlannerQueryExpandFilterTarget) WHERE b.age > 18 \
             RETURN a.name AS a_name, b.name AS b_name",
    );
    assert!(
        plan.ops
            .iter()
            .any(|op| matches!(op, PlanOp::ExpandFilter { .. }))
    );

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute planned query");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("b_name"),
        Some(&Value::Text("Planner EF Keep".into()))
    );
}

#[test]
fn directed_expand_projects_endpoint_and_edge_properties() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(
            ["QueryExpandSource"],
            [("name", Value::Text("Expand Alice".into()))],
        )
        .expect("insert source");
    let b = store
        .insert_vertex_named(
            ["QueryExpandTarget"],
            [("name", Value::Text("Expand Bob".into()))],
        )
        .expect("insert target");
    store
        .insert_directed_edge_named(a, b, Some("QueryKnows"), [("since", Value::Int64(2026))])
        .expect("insert edge");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("QueryExpandSource".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("QueryKnows".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
        },
        PlanOp::Project {
            columns: vec![
                project(prop("a", "name"), "a_name"),
                project(prop("b", "name"), "b_name"),
                project(prop("e", "since"), "since"),
            ],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute query");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("a_name"),
        Some(&Value::Text("Expand Alice".into()))
    );
    assert_eq!(
        result.rows[0].get("b_name"),
        Some(&Value::Text("Expand Bob".into()))
    );
    assert_eq!(result.rows[0].get("since"), Some(&Value::Int64(2026)));
}

#[test]
fn reverse_expand_resolves_edge_properties_through_alias() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["QueryReverseSource"], [("name", Value::Text("A".into()))])
        .expect("insert source");
    let b = store
        .insert_vertex_named(["QueryReverseTarget"], [("name", Value::Text("B".into()))])
        .expect("insert target");
    store
        .insert_directed_edge_named(
            a,
            b,
            Some("QueryReverseKnows"),
            [("since", Value::Int64(2027))],
        )
        .expect("insert edge");

    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "b".into(),
            label: Some("QueryReverseTarget".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "b".into(),
            edge: "e".into(),
            dst: "a".into(),
            direction: EdgeDirection::PointingLeft,
            label: Some("QueryReverseKnows".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
        },
        PlanOp::Project {
            columns: vec![project(prop("e", "since"), "since")],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute reverse query");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("since"), Some(&Value::Int64(2027)));
}

#[test]
fn undirected_expand_from_noncanonical_endpoint_resolves_edge_properties_through_alias() {
    let store = GraphStore::new();
    let low = store
        .insert_vertex_named(["QueryUndirLow"], [("name", Value::Text("low".into()))])
        .expect("insert low");
    let high = store
        .insert_vertex_named(["QueryUndirHigh"], [("name", Value::Text("high".into()))])
        .expect("insert high");
    store
        .insert_undirected_edge_named(
            low,
            high,
            Some("QueryUndirKnows"),
            [("since", Value::Int64(2028))],
        )
        .expect("insert edge");

    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("QueryUndirLow".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::Undirected,
            label: Some("QueryUndirKnows".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
        },
        PlanOp::Project {
            columns: vec![project(prop("e", "since"), "since")],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute undirected query");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("since"), Some(&Value::Int64(2028)));
}

fn setup_reused_dst_expand_graph(store: &GraphStore) -> VertexId {
    let a = store
        .insert_vertex_named(["ReuseExpandA"], [("name", Value::Text("anchor".into()))])
        .expect("insert anchor");
    let b = store
        .insert_vertex_named(["ReuseExpandB"], [("name", Value::Text("other".into()))])
        .expect("insert neighbor");
    store
        .insert_directed_edge_named(a, a, Some("ReuseExpandRel"), Vec::<(&str, Value)>::new())
        .expect("self-loop");
    store
        .insert_directed_edge_named(a, b, Some("ReuseExpandRel"), Vec::<(&str, Value)>::new())
        .expect("out-edge");
    a
}

#[test]
fn expand_reused_dst_only_keeps_self_loop_edges() {
    let store = GraphStore::new();
    let anchor = setup_reused_dst_expand_graph(&store);
    let plan =
        plan_gql("MATCH (a:ReuseExpandA)-[:ReuseExpandRel]->(a) RETURN ELEMENT_ID(a) AS a_id");
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("reused dst expand");
    assert_eq!(
        result.rows.len(),
        1,
        "only self-loop may satisfy reused dst: {:?}",
        result.rows
    );
    let Value::Bytes(id_bytes) = result.rows[0].get("a_id").expect("a_id column") else {
        panic!(
            "expected ELEMENT_ID bytes, got {:?}",
            result.rows[0].get("a_id")
        );
    };
    assert_eq!(
        GraphPathVertexId::try_from_slice(id_bytes.as_ref())
            .expect("decode vertex id")
            .logical_vertex_id,
        store.logical_vertex_id(anchor).expect("anchor logical id"),
    );
}

#[test]
fn expand_reused_dst_rejects_neighbor_mismatch() {
    let store = GraphStore::new();
    setup_reused_dst_expand_graph(&store);
    let plan = plan_gql("MATCH (a:ReuseExpandA)-[:ReuseExpandRel]->(a) RETURN a.name AS name");
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("reused dst expand");
    assert!(
        !result
            .rows
            .iter()
            .any(|row| row.get("name") == Some(&Value::Text("other".into()))),
        "reused dst must not adopt neighbor vertex binding: {:?}",
        result.rows
    );
}

#[test]
fn limited_expand_reused_dst_skips_neighbor_mismatch() {
    let store = GraphStore::new();
    setup_reused_dst_expand_graph(&store);
    let plan =
        plan_gql("MATCH (a:ReuseExpandA)-[:ReuseExpandRel]->(a) RETURN a.name AS name LIMIT 1");
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("reused dst expand");
    assert_eq!(text_column(&result, "name"), vec!["anchor"]);
}

fn setup_reused_dst_relabeled_graph(store: &GraphStore) -> VertexId {
    let a = store
        .insert_vertex_named(
            ["ReuseRelabelPerson", "ReuseRelabelUser"],
            [("name", Value::Text("anchor".into()))],
        )
        .expect("insert anchor");
    let b = store
        .insert_vertex_named(
            ["ReuseRelabelPerson"],
            [("name", Value::Text("other".into()))],
        )
        .expect("insert neighbor");
    store
        .insert_directed_edge_named(a, a, Some("ReuseRelabelRel"), Vec::<(&str, Value)>::new())
        .expect("self-loop");
    store
        .insert_directed_edge_named(a, b, Some("ReuseRelabelRel"), Vec::<(&str, Value)>::new())
        .expect("out-edge");
    a
}

#[test]
fn expand_reused_dst_relabeled_endpoints_keep_self_loop() {
    let store = GraphStore::new();
    let anchor = setup_reused_dst_relabeled_graph(&store);
    let plan = plan_gql(
        "MATCH (a:ReuseRelabelPerson)-[:ReuseRelabelRel]->(a:ReuseRelabelUser) RETURN ELEMENT_ID(a) AS a_id",
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("reused relabeled dst expand");
    assert_eq!(
        result.rows.len(),
        1,
        "self-loop with relabeled reuse must keep anchor: {:?}",
        result.rows
    );
    let Value::Bytes(id_bytes) = result.rows[0].get("a_id").expect("a_id column") else {
        panic!(
            "expected ELEMENT_ID bytes, got {:?}",
            result.rows[0].get("a_id")
        );
    };
    assert_eq!(
        GraphPathVertexId::try_from_slice(id_bytes.as_ref())
            .expect("decode vertex id")
            .logical_vertex_id,
        store.logical_vertex_id(anchor).expect("anchor logical id"),
    );
}

#[test]
fn expand_filter_applies_destination_predicate() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["QueryExpandFilterSource"], Vec::<(&str, Value)>::new())
        .expect("insert source");
    let keep = store
        .insert_vertex_named(["QueryExpandFilterTarget"], [("age", Value::Int64(44))])
        .expect("insert keep target");
    let drop = store
        .insert_vertex_named(["QueryExpandFilterTarget"], [("age", Value::Int64(10))])
        .expect("insert drop target");
    store
        .insert_directed_edge_named(
            a,
            keep,
            Some("QueryExpandFilterEdge"),
            Vec::<(&str, Value)>::new(),
        )
        .expect("insert keep edge");
    store
        .insert_directed_edge_named(
            a,
            drop,
            Some("QueryExpandFilterEdge"),
            Vec::<(&str, Value)>::new(),
        )
        .expect("insert drop edge");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("QueryExpandFilterSource".into()),
            property_projection: None,
        },
        PlanOp::ExpandFilter {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("QueryExpandFilterEdge".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            dst_filter: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("b", "age")),
                op: CmpOp::Gt,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
            })],
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
        },
        PlanOp::Project {
            columns: vec![project(prop("b", "age"), "age")],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute query");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("age"), Some(&Value::Int64(44)));
}

#[test]
fn expand_indexed_edge_equality_filters_candidates() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["IdxEqA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b_match = store
        .insert_vertex_named(["IdxEqB"], Vec::<(&str, Value)>::new())
        .expect("b match");
    let b_miss = store
        .insert_vertex_named(["IdxEqB"], Vec::<(&str, Value)>::new())
        .expect("b miss");
    store
        .insert_directed_edge_named(a, b_match, Some("IdxEqRel"), [("weight", Value::Int64(5))])
        .expect("match edge");
    store
        .insert_directed_edge_named(a, b_miss, Some("IdxEqRel"), [("weight", Value::Int64(9))])
        .expect("miss edge");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("IdxEqA".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("IdxEqRel".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: Some(("weight".into(), ScanValue::Literal(Value::Int64(5)))),
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ]);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("indexed expand");
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn expand_applies_dst_property_projection_for_property_return() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["ProjA"], [("uid", Value::Text("a1".into()))])
        .expect("a");
    let b = store
        .insert_vertex_named(["ProjB"], [("uid", Value::Text("b1".into()))])
        .expect("b");
    store
        .insert_directed_edge_named(a, b, Some("ProjRel"), Vec::<(&str, Value)>::new())
        .expect("edge");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("ProjA".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("ProjRel".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: Some(Rc::from([])),
            dst_property_projection: Some(Rc::from([Str::from("uid")])),
            hop_aux_binding: None,
            emit_edge_binding: false,
        },
        PlanOp::Project {
            columns: vec![
                project(prop("a", "uid"), "a_uid"),
                project(prop("b", "uid"), "b_uid"),
            ],
            distinct: false,
        },
    ]);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("projection expand");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("a_uid"), Some(&Value::Text("a1".into())));
    assert_eq!(result.rows[0].get("b_uid"), Some(&Value::Text("b1".into())));
}

#[test]
fn return_star_projects_vertex_and_edge_records() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(
            ["QueryReturnStarSource"],
            [("name", Value::Text("Star A".into()))],
        )
        .expect("insert source");
    let b = store
        .insert_vertex_named(
            ["QueryReturnStarTarget"],
            [("name", Value::Text("Star B".into()))],
        )
        .expect("insert target");
    store
        .insert_directed_edge_named(
            a,
            b,
            Some("QueryReturnStarEdge"),
            [("since", Value::Int64(1))],
        )
        .expect("insert edge");
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("QueryReturnStarSource".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("QueryReturnStarEdge".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
        },
        PlanOp::Project {
            columns: vec![],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute query");

    assert_eq!(result.rows.len(), 1);
    assert!(matches!(result.rows[0].get("a"), Some(Value::Record(_))));
    assert!(matches!(result.rows[0].get("b"), Some(Value::Record(_))));
    assert!(matches!(result.rows[0].get("e"), Some(Value::Record(_))));
}

#[test]
fn return_abs_gleaph_weight_does_not_break_decoder_prep() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
    let a = store
        .insert_vertex_named(["AbsWgtA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["AbsWgtB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let label_id = crate::test_labels::edge_label_id_for_name("AbsWgtRoad");
    store
        .install_edge_label_weight_profile_at_init(
            label_id,
            EdgeWeightProfile {
                encoding: WeightEncoding::RawU16,
            },
        )
        .expect("profile");
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &3u16.to_le_bytes())
        .expect("edge");
    let gql = "MATCH (a:AbsWgtA)-[e:AbsWgtRoad]->(b:AbsWgtB) RETURN ABS(GLEAPH.WEIGHT(e)) AS w";
    let plan = plan_gql(gql);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("abs gleaph weight return");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("w"), Some(&Value::Float32(3.0)));
}

#[test]
fn gleaph_weight_accepts_edge_payload_profile_without_legacy_weight_profile() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["PayloadProfileWgtA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["PayloadProfileWgtB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let label_id = crate::test_labels::edge_label_id_for_name("PayloadProfileWgtRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::WeightRawU16,
            },
        )
        .expect("payload profile");
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &[9, 0])
        .expect("edge");

    let gql = "MATCH (a:PayloadProfileWgtA)-[e:PayloadProfileWgtRoad]->(b:PayloadProfileWgtB) RETURN GLEAPH.WEIGHT(e) AS w";
    let plan = plan_gql(gql);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("value-profile-only gleaph weight");
    assert_eq!(result.rows[0].get("w"), Some(&Value::Float32(9.0)));
}

#[test]
fn gql_gleaph_weight_equality_uses_edge_payload_predicate_expand() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["GqlBatchEqualA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["GqlBatchEqualB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["GqlBatchEqualC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("GqlBatchEqualRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::WeightRawU16,
            },
        )
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &7u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, c, Some(label_id), &9u16.to_le_bytes())
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:GqlBatchEqualA)-[e:GqlBatchEqualRoad]->(b) \
             WHERE GLEAPH.WEIGHT(e) = 7 RETURN b",
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute query");

    assert_eq!(result.rows.len(), 1);
}

#[test]
fn gql_gleaph_weight_gt_uses_edge_payload_predicate_expand() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["GqlBatchGtA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["GqlBatchGtB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["GqlBatchGtC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("GqlBatchGtRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::WeightRawU16,
            },
        )
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &7u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, c, Some(label_id), &9u16.to_le_bytes())
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:GqlBatchGtA)-[e:GqlBatchGtRoad]->(b) \
             WHERE GLEAPH.WEIGHT(e) > 7 RETURN b",
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute query");

    assert_eq!(result.rows.len(), 1);
}

#[test]
fn gql_gleaph_vector_l2_uses_edge_vector_predicate_expand() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["GqlVectorA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let near = store
        .insert_vertex_named(["GqlVectorB"], [("name", Value::Text("near".into()))])
        .expect("near");
    let far = store
        .insert_vertex_named(["GqlVectorB"], [("name", Value::Text("far".into()))])
        .expect("far");
    let label_id = crate::test_labels::edge_label_id_for_name("GqlVectorRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 16,
                encoding: EdgePayloadEncoding::VectorF32 { dims: 4 },
            },
        )
        .unwrap();
    let near_bytes = f32_vector_bytes(&[1.0, 1.0, 1.0, 1.0]);
    let far_bytes = f32_vector_bytes(&[9.0, 9.0, 9.0, 9.0]);
    store
        .insert_directed_edge_with_payload_bytes(a, near, Some(label_id), &near_bytes)
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, far, Some(label_id), &far_bytes)
        .unwrap();

    let mut parameters = params();
    parameters.insert(
        "$q".into(),
        Value::List(vec![
            Value::Float32(1.0),
            Value::Float32(1.0),
            Value::Float32(1.0),
            Value::Float32(1.0),
        ]),
    );
    let plan = plan_gql(
        "MATCH (a:GqlVectorA)-[e:GqlVectorRoad]->(b:GqlVectorB) \
             WHERE GLEAPH.VECTOR.L2_SQUARED(e, $q) <= 4.0 RETURN b.name AS name",
    );
    let result = store
        .execute_plan_query(&plan, &parameters, GqlExecutionContext::default())
        .expect("execute query");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("name"),
        Some(&Value::Text("near".into()))
    );
}

#[test]
fn gql_gleaph_vector_dot_uses_edge_vector_predicate_expand() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["GqlVectorDotA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let high = store
        .insert_vertex_named(["GqlVectorDotB"], [("name", Value::Text("high".into()))])
        .expect("high");
    let low = store
        .insert_vertex_named(["GqlVectorDotB"], [("name", Value::Text("low".into()))])
        .expect("low");
    let label_id = crate::test_labels::edge_label_id_for_name("GqlVectorDotRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 16,
                encoding: EdgePayloadEncoding::VectorF32 { dims: 4 },
            },
        )
        .unwrap();
    let high_bytes = f32_vector_bytes(&[2.0, 2.0, 2.0, 2.0]);
    let low_bytes = f32_vector_bytes(&[0.1, 0.1, 0.1, 0.1]);
    store
        .insert_directed_edge_with_payload_bytes(a, high, Some(label_id), &high_bytes)
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, low, Some(label_id), &low_bytes)
        .unwrap();

    let mut parameters = params();
    parameters.insert(
        "$q".into(),
        Value::List(vec![
            Value::Float32(1.0),
            Value::Float32(1.0),
            Value::Float32(1.0),
            Value::Float32(1.0),
        ]),
    );
    let plan = plan_gql(
        "MATCH (a:GqlVectorDotA)-[e:GqlVectorDotRoad]->(b:GqlVectorDotB) \
             WHERE GLEAPH.VECTOR.DOT(e, $q) >= 4.0 RETURN b.name AS name",
    );
    let result = store
        .execute_plan_query(&plan, &parameters, GqlExecutionContext::default())
        .expect("execute query");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("name"),
        Some(&Value::Text("high".into()))
    );
}

#[test]
fn vector_dst_only_expand_filter_keeps_projection_fast_path_semantics() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["VectorDstOnlyFilterA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let keep = store
        .insert_vertex_named(
            ["VectorDstOnlyFilterB"],
            [
                ("age", Value::Int64(44)),
                ("name", Value::Text("keep".into())),
            ],
        )
        .expect("keep");
    let drop = store
        .insert_vertex_named(
            ["VectorDstOnlyFilterB"],
            [
                ("age", Value::Int64(10)),
                ("name", Value::Text("drop".into())),
            ],
        )
        .expect("drop");
    let label_id = crate::test_labels::edge_label_id_for_name("VectorDstOnlyFilterRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 16,
                encoding: EdgePayloadEncoding::VectorF32 { dims: 4 },
            },
        )
        .unwrap();
    let near_bytes = f32_vector_bytes(&[1.0, 1.0, 1.0, 1.0]);
    store
        .insert_directed_edge_with_payload_bytes(a, keep, Some(label_id), &near_bytes)
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, drop, Some(label_id), &near_bytes)
        .unwrap();

    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("VectorDstOnlyFilterA".into()),
            property_projection: None,
        },
        PlanOp::ExpandFilter {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("VectorDstOnlyFilterRoad".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: Some(EdgeVectorPredicate {
                metric: EdgeVectorMetric::L2Squared,
                query: ScanValue::Literal(Value::List(vec![
                    Value::Float32(1.0),
                    Value::Float32(1.0),
                    Value::Float32(1.0),
                    Value::Float32(1.0),
                ])),
                op: CmpOp::Le,
                threshold: ScanValue::Literal(Value::Float32(4.0)),
            }),
            dst_filter: vec![Expr::new(ExprKind::Compare {
                left: Box::new(prop("b", "age")),
                op: CmpOp::Gt,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
            })],
            edge_property_projection: None,
            dst_property_projection: Some(vec!["name".into()].into()),
            hop_aux_binding: None,
            emit_edge_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(prop("b", "name"), "name")],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute query");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("name"),
        Some(&Value::Text("keep".into()))
    );
}

#[test]
fn ascending_forward_fixed_label_candidates_use_batched_edge_payloads() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["BatchExpandA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["BatchExpandB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["BatchExpandC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("BatchExpandRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::RawU16,
            },
        )
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &[1, 0])
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, c, Some(label_id), &[2, 0])
        .unwrap();

    let mut out = Vec::new();
    super::expand_candidates_into(
        &store,
        a,
        EdgeDirection::PointingRight,
        Some(label_id),
        EdgeSequenceOrder::Ascending,
        None,
        None,
        None,
        &params(),
        &mut out,
    )
    .expect("expand candidates");

    assert_eq!(out.len(), 2);
    assert_eq!(out[0].1.payload_bytes_slice(), &[1, 0]);
    assert_eq!(out[1].1.payload_bytes_slice(), &[2, 0]);
}

#[test]
fn ascending_reverse_fixed_label_candidates_use_batched_edge_payloads() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["BatchReverseExpandA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["BatchReverseExpandB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["BatchReverseExpandC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("BatchReverseExpandRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::RawU16,
            },
        )
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, c, Some(label_id), &[1, 0])
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &[2, 0])
        .unwrap();

    let mut out = Vec::new();
    super::expand_candidates_into(
        &store,
        c,
        EdgeDirection::PointingLeft,
        Some(label_id),
        EdgeSequenceOrder::Ascending,
        None,
        None,
        None,
        &params(),
        &mut out,
    )
    .expect("expand candidates");

    assert_eq!(out.len(), 2);
    let mut values = out
        .iter()
        .map(|(_, binding)| binding.payload_bytes_slice().to_vec())
        .collect::<Vec<_>>();
    values.sort();
    assert_eq!(values, vec![vec![1, 0], vec![2, 0]]);
}

#[test]
fn forward_fixed_label_edge_payload_predicate_uses_batch_kernel() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["BatchEqualA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["BatchEqualB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["BatchEqualC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("BatchEqualRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::RawU16,
            },
        )
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &7u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, c, Some(label_id), &9u16.to_le_bytes())
        .unwrap();

    let equality = PreparedEdgePayloadPredicate::prepare(
        &store,
        label_id,
        &EdgePayloadPredicate {
            op: CmpOp::Eq,
            value: ScanValue::Literal(Value::Uint16(7)),
        },
        &params(),
    )
    .expect("prepare")
    .expect("equality");
    let mut out = Vec::new();
    super::candidates::expand_candidates_matching_edge_payload_into(
        &store,
        a,
        EdgeDirection::PointingRight,
        label_id,
        EdgeSequenceOrder::Ascending,
        &equality,
        &mut out,
    )
    .expect("expand candidates");

    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].0, ExpandDst::Local(dst) if dst == b));
    assert_eq!(out[0].1.payload_bytes_slice(), &7u16.to_le_bytes());
}

#[test]
fn expand_plan_edge_payload_predicate_filters_candidates() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["PlanBatchEqualA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["PlanBatchEqualB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["PlanBatchEqualC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("PlanBatchEqualRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::RawU16,
            },
        )
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &7u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, c, Some(label_id), &9u16.to_le_bytes())
        .unwrap();

    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("PlanBatchEqualA".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("PlanBatchEqualRoad".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: Some(EdgePayloadPredicate {
                op: CmpOp::Eq,
                value: ScanValue::Literal(Value::Uint16(7)),
            }),
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
        },
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ]);

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("execute query");

    assert_eq!(result.rows.len(), 1);
    assert!(matches!(result.rows[0].get("b"), Some(Value::Record(_))));
}

#[test]
fn reverse_fixed_label_edge_payload_predicate_uses_batch_kernel() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["BatchReverseEqualA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["BatchReverseEqualB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["BatchReverseEqualC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("BatchReverseEqualRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::RawU16,
            },
        )
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, c, Some(label_id), &7u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &9u16.to_le_bytes())
        .unwrap();

    let equality = PreparedEdgePayloadPredicate::prepare(
        &store,
        label_id,
        &EdgePayloadPredicate {
            op: CmpOp::Eq,
            value: ScanValue::Literal(Value::Uint16(7)),
        },
        &params(),
    )
    .expect("prepare")
    .expect("equality");
    let mut out = Vec::new();
    super::candidates::expand_candidates_matching_edge_payload_into(
        &store,
        c,
        EdgeDirection::PointingLeft,
        label_id,
        EdgeSequenceOrder::Ascending,
        &equality,
        &mut out,
    )
    .expect("expand candidates");

    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].0, ExpandDst::Local(dst) if dst == a));
    assert_eq!(out[0].1.payload_bytes_slice(), &7u16.to_le_bytes());
}

fn f32_vector_bytes(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

#[test]
fn forward_fixed_label_edge_vector_threshold_uses_batch_kernel() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["BatchVectorA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let near = store
        .insert_vertex_named(["BatchVectorNear"], Vec::<(&str, Value)>::new())
        .expect("near");
    let far = store
        .insert_vertex_named(["BatchVectorFar"], Vec::<(&str, Value)>::new())
        .expect("far");
    let label_id = crate::test_labels::edge_label_id_for_name("BatchVectorRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 16,
                encoding: EdgePayloadEncoding::VectorF32 { dims: 4 },
            },
        )
        .unwrap();
    let near_bytes = f32_vector_bytes(&[1.0, 1.0, 1.0, 1.0]);
    let far_bytes = f32_vector_bytes(&[9.0, 9.0, 9.0, 9.0]);
    store
        .insert_directed_edge_with_payload_bytes(a, near, Some(label_id), &near_bytes)
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, far, Some(label_id), &far_bytes)
        .unwrap();

    let predicate = PreparedEdgeVectorThreshold::prepare(
        &store,
        label_id,
        &EdgeVectorPredicate {
            metric: EdgeVectorMetric::L2Squared,
            query: ScanValue::Literal(Value::List(vec![
                Value::Float32(1.0),
                Value::Float32(1.0),
                Value::Float32(1.0),
                Value::Float32(1.0),
            ])),
            op: CmpOp::Le,
            threshold: ScanValue::Literal(Value::Float32(4.0)),
        },
        &params(),
    )
    .expect("prepare")
    .expect("predicate");
    let mut out = Vec::new();
    super::candidates::expand_candidates_matching_edge_vector_threshold_into(
        &store,
        a,
        EdgeDirection::PointingRight,
        label_id,
        EdgeSequenceOrder::Ascending,
        &predicate,
        &mut out,
    )
    .expect("expand candidates");

    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].0, ExpandDst::Local(dst) if dst == near));
    assert_eq!(out[0].1.handle.owner_vertex_id, a);
    assert_eq!(out[0].1.payload_bytes_slice(), near_bytes.as_slice());
}

#[test]
fn reverse_fixed_label_edge_vector_threshold_uses_batch_kernel() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let near = store
        .insert_vertex_named(["BatchVectorReverseNear"], Vec::<(&str, Value)>::new())
        .expect("near");
    let far = store
        .insert_vertex_named(["BatchVectorReverseFar"], Vec::<(&str, Value)>::new())
        .expect("far");
    let c = store
        .insert_vertex_named(["BatchVectorReverseC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("BatchVectorReverseRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 16,
                encoding: EdgePayloadEncoding::VectorF32 { dims: 4 },
            },
        )
        .unwrap();
    let near_bytes = f32_vector_bytes(&[1.0, 1.0, 1.0, 1.0]);
    let far_bytes = f32_vector_bytes(&[9.0, 9.0, 9.0, 9.0]);
    store
        .insert_directed_edge_with_payload_bytes(near, c, Some(label_id), &near_bytes)
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(far, c, Some(label_id), &far_bytes)
        .unwrap();

    let predicate = PreparedEdgeVectorThreshold::prepare(
        &store,
        label_id,
        &EdgeVectorPredicate {
            metric: EdgeVectorMetric::L2Squared,
            query: ScanValue::Literal(Value::List(vec![
                Value::Float32(1.0),
                Value::Float32(1.0),
                Value::Float32(1.0),
                Value::Float32(1.0),
            ])),
            op: CmpOp::Le,
            threshold: ScanValue::Literal(Value::Float32(4.0)),
        },
        &params(),
    )
    .expect("prepare")
    .expect("predicate");
    let mut out = Vec::new();
    super::candidates::expand_candidates_matching_edge_vector_threshold_into(
        &store,
        c,
        EdgeDirection::PointingLeft,
        label_id,
        EdgeSequenceOrder::Ascending,
        &predicate,
        &mut out,
    )
    .expect("expand candidates");

    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].0, ExpandDst::Local(dst) if dst == near));
    assert_eq!(out[0].1.handle.owner_vertex_id, near);
    assert_eq!(out[0].1.payload_bytes_slice(), near_bytes.as_slice());
}

#[test]
fn ascending_forward_fixed_label_without_edge_payloads_keeps_scalar_scan() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["ScalarExpandA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["ScalarExpandB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let label_id = crate::test_labels::edge_label_id_for_name("ScalarExpandRoad");
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &[])
        .unwrap();

    let mut out = Vec::new();
    super::expand_candidates_into(
        &store,
        a,
        EdgeDirection::PointingRight,
        Some(label_id),
        EdgeSequenceOrder::Ascending,
        None,
        None,
        None,
        &params(),
        &mut out,
    )
    .expect("expand candidates");

    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].0, ExpandDst::Local(dst) if dst == b));
    assert!(out[0].1.payload_bytes_slice().is_empty());
}

#[test]
fn gleaph_weight_rejects_edge_payload_width_mismatch() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["MissingValueWgtA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["MissingValueWgtB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let label_id = crate::test_labels::edge_label_id_for_name("MissingValueWgtRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::WeightRawU16,
            },
        )
        .expect("payload profile");
    let err = store
        .insert_directed_edge(a, b, Some(label_id))
        .expect_err("edge without value bytes must fail at insert");
    assert!(
        err.to_string().contains("expects 2 value bytes, got 0"),
        "unexpected error: {err}"
    );
}

#[test]
fn federated_neighbor_hit_preserves_remote_payload_bytes() {
    let hit = FederatedExpandNeighbor {
        shard_id: 99,
        neighbor_logical_vertex_id: 1,
        neighbor_local_vertex_id: 2,
        anchor_local_vertex_id: 3,
        label_id_raw: 0,
        slot_index: 4,
        payload_bytes: vec![42, 0],
    };
    let binding = EdgeBinding::from_federated_neighbor_hit(&hit);
    assert_eq!(binding.payload_len(), 2);
    assert_eq!(binding.payload_bytes_slice(), &[42, 0]);
}
