use super::super::test_support::*;
use super::execute_var_len_expand;
use super::predicates::{PreparedEdgePayloadPredicate, PreparedEdgeVectorThreshold};
use crate::federation::{TraversalExpandSource, resolve_traversal_expand_source};
use crate::index::placement::native_test_set_active_placement;
use gleaph_gql_planner::plan::{EdgePayloadPredicate, EdgeVectorMetric, EdgeVectorPredicate};
use gleaph_graph_kernel::federation::{
    ElementIdEncodingKey, GlobalVertexId, PhysicalVertexLocation, ShardId,
};
use pollster;

#[test]
fn resolve_traversal_expand_source_uses_peer_expand_for_foreign_authority() {
    let store = GraphStore::new();
    configure_test_federation(&store);
    let vertex = store.insert_vertex().expect("vertex");
    let logical = store.global_vertex_id(vertex).expect("logical");
    native_test_set_active_placement(
        logical,
        PhysicalVertexLocation::new(ShardId::new(1), u32::from(vertex)),
    );

    let source = pollster::block_on(resolve_traversal_expand_source(
        &store,
        Some(&PlanBinding::Vertex(vertex)),
        EdgeDirection::PointingRight,
    ))
    .expect("resolve");

    assert_eq!(source, Some(TraversalExpandSource::PeerExpand(logical)));
}

#[test]
fn resolve_traversal_expand_source_uses_local_csr_for_remote_vertex_on_home_shard() {
    let store = GraphStore::new();
    configure_test_federation(&store);
    let vertex = store.insert_vertex().expect("vertex");
    let logical = store.global_vertex_id(vertex).expect("logical");

    let source = pollster::block_on(resolve_traversal_expand_source(
        &store,
        Some(&PlanBinding::RemoteVertex(logical)),
        EdgeDirection::PointingLeft,
    ))
    .expect("resolve");

    assert_eq!(source, Some(TraversalExpandSource::LocalCsr(vertex)));
}

#[test]
fn federated_reverse_expand_from_remote_vertex_binding() {
    let store = GraphStore::new();
    configure_test_federation(&store);
    let source = store.insert_vertex().expect("source");
    let source_logical = store.global_vertex_id(source).expect("logical");
    let remote_logical = GlobalVertexId::new(ShardId::new(1), 88_001);
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
        None,
    ))
    .expect("federated reverse expand");

    assert_eq!(out.len(), 1);
    assert!(matches!(
        out[0].get("a"),
        Some(PlanBinding::Vertex(v)) if *v == source
    ));
    assert_eq!(
        store.global_vertex_id(source).expect("source logical"),
        source_logical
    );
}

#[test]
fn federated_var_len_one_hop_from_remote_vertex_binding() {
    let store = GraphStore::new();
    configure_test_federation(&store);
    let source = store.insert_vertex().expect("source");
    let remote_logical = GlobalVertexId::new(ShardId::new(1), 88_002);
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
    let out = pollster::block_on(execute_var_len_expand(
        &ctx,
        vec![seed],
        &"b".into(),
        &"e".into(),
        &"a".into(),
        EdgeDirection::PointingLeft,
        None,
        None,
        &ctx.execution,
        &VarLenSpec {
            min: 1,
            max: Some(1),
        },
        &[],
        true,
        None,
        None,
        None,
        None,
        false,
        None,
        None,
        None,
        None,
        None,
    ))
    .expect("federated var_len one hop");

    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].get("a"), Some(PlanBinding::Vertex(v)) if v == &source));
}

#[test]
fn federated_var_len_rejects_peer_expand_source() {
    let store = GraphStore::new();
    configure_test_federation(&store);
    let vertex = store.insert_vertex().expect("vertex");
    let logical = store.global_vertex_id(vertex).expect("logical");
    native_test_set_active_placement(
        logical,
        PhysicalVertexLocation::new(ShardId::new(1), u32::from(vertex)),
    );

    let mut seed = PlanRow::new();
    seed.insert("a".to_owned(), PlanBinding::Vertex(vertex));

    let parameters = params();
    let ctx = ExecuteCtx::new(
        &store,
        &parameters,
        None,
        GqlExecutionContext::default(),
        None,
    );
    let err = pollster::block_on(execute_var_len_expand(
        &ctx,
        vec![seed],
        &"a".into(),
        &"e".into(),
        &"b".into(),
        EdgeDirection::PointingRight,
        None,
        None,
        &ctx.execution,
        &VarLenSpec {
            min: 2,
            max: Some(2),
        },
        &[],
        false,
        None,
        None,
        None,
        None,
        false,
        None,
        None,
        None,
        None,
        None,
    ))
    .expect_err("peer expand var_len source");

    assert!(
        matches!(err, PlanQueryError::UnsupportedOp("Expand.var_len.peer")),
        "unexpected error: {err}"
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
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
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
fn wildcard_label_expr_edge_payload_predicate_fuses_via_catalog_fallback() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["WcPayloadA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let match_b = store
        .insert_vertex_named(["WcPayloadB"], Vec::<(&str, Value)>::new())
        .expect("match");
    let skip_b = store
        .insert_vertex_named(["WcPayloadB"], Vec::<(&str, Value)>::new())
        .expect("skip");
    let road = crate::test_labels::edge_label_id_for_name("WcPayloadRoad");
    let alt = crate::test_labels::edge_label_id_for_name("WcPayloadAlt");
    for label_id in [road, alt] {
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
        .insert_directed_edge_with_payload_bytes(a, match_b, Some(road), &5u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, skip_b, Some(alt), &9u16.to_le_bytes())
        .unwrap();

    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("WcPayloadA".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: None,
            label_expr: Some(LabelExpr::Wildcard),
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: Some(EdgePayloadPredicate {
                op: CmpOp::Eq,
                value: ScanValue::Literal(Value::Int64(5)),
            }),
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
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ]);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("wildcard payload fusion");

    assert_eq!(result.rows.len(), 1);
}

#[test]
fn not_label_expr_edge_payload_predicate_fuses_via_catalog_fallback() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["NotPayloadA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let road_target = store
        .insert_vertex_named(["NotPayloadB"], Vec::<(&str, Value)>::new())
        .expect("road");
    let alt_target = store
        .insert_vertex_named(["NotPayloadB"], Vec::<(&str, Value)>::new())
        .expect("alt");
    let road = crate::test_labels::edge_label_id_for_name("NotPayloadRoad");
    let alt = crate::test_labels::edge_label_id_for_name("NotPayloadAlt");
    for label_id in [road, alt] {
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
        .insert_directed_edge_with_payload_bytes(a, road_target, Some(road), &5u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, alt_target, Some(alt), &5u16.to_le_bytes())
        .unwrap();

    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("NotPayloadA".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: None,
            label_expr: Some(LabelExpr::Not(Box::new(LabelExpr::Name(
                "NotPayloadRoad".into(),
            )))),
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: Some(EdgePayloadPredicate {
                op: CmpOp::Eq,
                value: ScanValue::Literal(Value::Int64(5)),
            }),
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
        PlanOp::Project {
            columns: vec![project(var("b"), "b")],
            distinct: false,
        },
    ]);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("not label_expr payload fusion");

    assert_eq!(result.rows.len(), 1);
    assert!(matches!(result.rows[0].get("b"), Some(Value::Record(_))));
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
fn var_len_edge_property_projection_returns_projected_properties() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["VarLenProjA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["VarLenProjMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["VarLenProjC"], Vec::<(&str, Value)>::new())
        .expect("c");
    // Use the same property value on both hops: reverse-in edge aliases on the
    // intermediate vertex can share canonical property storage with the next forward edge.
    store
        .insert_directed_edge_named(a, b, Some("VarLenProjRel"), [("since", Value::Int64(5))])
        .expect("a->b");
    store
        .insert_directed_edge_named(b, c, Some("VarLenProjRel"), [("since", Value::Int64(5))])
        .expect("b->c");

    let gql_plan = plan_gql(
        "MATCH (a:VarLenProjA)-[e:VarLenProjRel]->{2,2}(c:VarLenProjC) \
         RETURN e.since AS sinces",
    );
    let edge_projection = gql_plan.ops.iter().find_map(|op| match op {
        PlanOp::Expand {
            var_len,
            edge_property_projection,
            ..
        } if var_len.is_some() => edge_property_projection.clone(),
        _ => None,
    });
    assert_eq!(
        edge_projection.as_ref().map(|rc| {
            rc.iter()
                .map(|s| s.as_ref().to_string())
                .collect::<Vec<_>>()
        }),
        Some(vec!["since".to_string()]),
        "planner should infer edge property projection for RETURN e.since: {:?}",
        gql_plan.ops
    );

    let result = store
        .execute_plan_query(&gql_plan, &params(), GqlExecutionContext::default())
        .expect("var_len edge property projection");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("sinces"),
        Some(&Value::List(vec![Value::Int64(5), Value::Int64(5)]))
    );
}

#[test]
fn expand_hop_aux_binding_returns_edge_payload_bytes() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["HopAuxA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["HopAuxB"], Vec::<(&str, Value)>::new())
        .expect("b");
    let label_id = crate::test_labels::edge_label_id_for_name("HopAuxRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::WeightRawU16,
            },
        )
        .unwrap();
    let payload = 7u16.to_le_bytes();
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &payload)
        .unwrap();

    let plan = plan_gql("MATCH (a:HopAuxA)-[e:HopAuxRoad]->(b:HopAuxB) RETURN e__hop_aux AS aux");
    let hop_aux_binding = plan.ops.iter().find_map(|op| match op {
        PlanOp::Expand {
            hop_aux_binding, ..
        } => hop_aux_binding.clone(),
        _ => None,
    });
    assert_eq!(
        hop_aux_binding.as_deref().map(|s| s.as_ref()),
        Some("e__hop_aux")
    );

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("hop_aux expand");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("aux"),
        Some(&Value::Bytes(payload.to_vec()))
    );
}

#[test]
fn var_len_hop_aux_binding_returns_payload_bytes_list() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["HopAuxVarA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["HopAuxVarMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["HopAuxVarC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("HopAuxVarRoad");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::WeightRawU16,
            },
        )
        .unwrap();
    let hop1 = 3u16.to_le_bytes();
    let hop2 = 7u16.to_le_bytes();
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &hop1)
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &hop2)
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:HopAuxVarA)-[e:HopAuxVarRoad]->{2,2}(c:HopAuxVarC) RETURN e__hop_aux AS aux",
    );
    let hop_aux_binding = plan.ops.iter().find_map(|op| match op {
        PlanOp::Expand {
            var_len,
            hop_aux_binding,
            ..
        } if var_len.is_some() => hop_aux_binding.clone(),
        _ => None,
    });
    assert_eq!(
        hop_aux_binding.as_deref().map(|s| s.as_ref()),
        Some("e__hop_aux")
    );

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("var_len hop_aux");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].get("aux"),
        Some(&Value::List(vec![
            Value::Bytes(hop1.to_vec()),
            Value::Bytes(hop2.to_vec()),
        ]))
    );
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
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
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
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
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
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
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
            .decode_global(&ElementIdEncodingKey::standalone()),
        store.global_vertex_id(anchor).expect("anchor global id"),
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
            .decode_global(&ElementIdEncodingKey::standalone()),
        store.global_vertex_id(anchor).expect("anchor global id"),
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
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
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
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
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
fn indexed_edge_equality_expand_return_gleaph_weight() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["IdxEqWgtA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b_match = store
        .insert_vertex_named(["IdxEqWgtB"], Vec::<(&str, Value)>::new())
        .expect("b match");
    let b_miss = store
        .insert_vertex_named(["IdxEqWgtB"], Vec::<(&str, Value)>::new())
        .expect("b miss");
    let label_id = crate::test_labels::edge_label_id_for_name("IdxEqWgtRel");
    store
        .install_edge_label_payload_profile_at_init(
            label_id,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::WeightRawU16,
            },
        )
        .unwrap();
    let weight_prop = crate::test_labels::property_id_for_name("weight");
    let match_edge = store
        .insert_directed_edge_with_payload_bytes(a, b_match, Some(label_id), &5u16.to_le_bytes())
        .expect("match edge");
    store
        .set_edge_property(match_edge, weight_prop, Value::Int64(5))
        .expect("match edge property");
    let miss_edge = store
        .insert_directed_edge_with_payload_bytes(a, b_miss, Some(label_id), &9u16.to_le_bytes())
        .expect("miss edge");
    store
        .set_edge_property(miss_edge, weight_prop, Value::Int64(9))
        .expect("miss edge property");

    let gleaph_weight = Expr::new(ExprKind::FunctionCall {
        name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
        args: vec![var("e")],
        distinct: false,
    });
    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("IdxEqWgtA".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("IdxEqWgtRel".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: Some(("weight".into(), ScanValue::Literal(Value::Int64(5)))),
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
        PlanOp::Project {
            columns: vec![project(gleaph_weight, "w")],
            distinct: false,
        },
    ]);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("indexed expand with gleaph weight return");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("w"), Some(&Value::Float32(5.0)));
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
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
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
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
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
fn gql_union_label_expr_return_gleaph_weight_decodes_per_edge_label() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["ExpandUnionWgtA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b1 = store
        .insert_vertex_named(["ExpandUnionWgtB"], Vec::<(&str, Value)>::new())
        .expect("b1");
    let b2 = store
        .insert_vertex_named(["ExpandUnionWgtB"], Vec::<(&str, Value)>::new())
        .expect("b2");
    let knows = crate::test_labels::edge_label_id_for_name("ExpandUnionWgtKnows");
    let likes = crate::test_labels::edge_label_id_for_name("ExpandUnionWgtLikes");
    for label_id in [knows, likes] {
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
        .insert_directed_edge_with_payload_bytes(a, b1, Some(knows), &5u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, b2, Some(likes), &9u16.to_le_bytes())
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:ExpandUnionWgtA)-[e:ExpandUnionWgtKnows|ExpandUnionWgtLikes]->(b:ExpandUnionWgtB) \
         RETURN GLEAPH.WEIGHT(e) AS w ORDER BY w",
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("union label_expr gleaph weight return");

    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].get("w"), Some(&Value::Float32(5.0)));
    assert_eq!(result.rows[1].get("w"), Some(&Value::Float32(9.0)));
}

#[test]
fn gql_union_label_expr_where_gleaph_weight_equality_fuses_and_filters() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["ExpandUnionWgtEqA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let match_b = store
        .insert_vertex_named(["ExpandUnionWgtEqB"], Vec::<(&str, Value)>::new())
        .expect("match");
    let skip_b = store
        .insert_vertex_named(["ExpandUnionWgtEqB"], Vec::<(&str, Value)>::new())
        .expect("skip");
    let knows = crate::test_labels::edge_label_id_for_name("ExpandUnionWgtEqKnows");
    let likes = crate::test_labels::edge_label_id_for_name("ExpandUnionWgtEqLikes");
    for label_id in [knows, likes] {
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
        .insert_directed_edge_with_payload_bytes(a, match_b, Some(knows), &7u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, skip_b, Some(likes), &9u16.to_le_bytes())
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:ExpandUnionWgtEqA)-[e:ExpandUnionWgtEqKnows|ExpandUnionWgtEqLikes]->(b:ExpandUnionWgtEqB) \
         WHERE GLEAPH.WEIGHT(e) = 7 RETURN b",
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("union label_expr gleaph weight where");

    assert_eq!(result.rows.len(), 1);
}

#[test]
fn gql_var_len_scalar_gleaph_weight_is_rejected() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["VarLenWgtA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["VarLenWgtMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["VarLenWgtC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("VarLenWgtRoad");
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
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &3u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &7u16.to_le_bytes())
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:VarLenWgtA)-[e:VarLenWgtRoad]->{2,2}(c:VarLenWgtC) RETURN GLEAPH.WEIGHT(e) AS w",
    );
    let err = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect_err("scalar gleaph weight on group edge var");
    assert!(
        err.to_string().contains("edge variable is a group")
            || err.to_string().contains("binding is not an edge"),
        "unexpected error: {err}"
    );
}

#[test]
fn gql_var_len_return_gleaph_weight_decodes_indexed_last_hop_edge() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["VarLenWgtA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["VarLenWgtMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["VarLenWgtC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("VarLenWgtRoad");
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
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &3u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &7u16.to_le_bytes())
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:VarLenWgtA)-[e:VarLenWgtRoad]->{2,2}(c:VarLenWgtC) RETURN GLEAPH.WEIGHT(e[-1]) AS w",
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("var_len gleaph weight return");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("w"), Some(&Value::Float32(7.0)));
}

#[test]
fn gql_var_len_return_sum_gleaph_weight_over_edge_group_via_let() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["VarLenSumA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["VarLenSumMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["VarLenSumC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("VarLenSumRoad");
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
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &3u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &7u16.to_le_bytes())
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:VarLenSumA)-[e:VarLenSumRoad]->{2,2}(c:VarLenSumC) \
         LET total = SUM(GLEAPH.WEIGHT(e)) RETURN total, CARDINALITY(e) AS hops",
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("var_len sum gleaph weight");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("total"), Some(&Value::Float32(10.0)));
    assert_eq!(result.rows[0].get("hops"), Some(&Value::Int64(2)));
}

#[test]
fn gql_var_len_return_sum_gleaph_weight_implicit_aggregate() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["VarLenSumRetA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["VarLenSumRetMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["VarLenSumRetC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("VarLenSumRetRoad");
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
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &3u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &7u16.to_le_bytes())
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:VarLenSumRetA)-[e:VarLenSumRetRoad]->{2,2}(c:VarLenSumRetC) \
         RETURN SUM(GLEAPH.WEIGHT(e)) AS total, CARDINALITY(e) AS hops",
    );
    assert!(
        plan.ops
            .iter()
            .any(|op| matches!(op, PlanOp::Aggregate { .. })),
        "planner should emit implicit aggregate for RETURN SUM(...): {:?}",
        plan.ops
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("var_len RETURN SUM gleaph weight");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("total"), Some(&Value::Float32(10.0)));
    assert_eq!(result.rows[0].get("hops"), Some(&Value::Int64(2)));
}

#[test]
fn gql_quantified_subpath_binds_node_and_edge_groups() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["QtyHopA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["QtyHopMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["QtyHopC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let dead_end = store
        .insert_vertex_named(["QtyHopDead"], Vec::<(&str, Value)>::new())
        .expect("dead");
    store
        .insert_directed_edge_named(a, b, Some("QtyHopRoad"), Vec::<(&str, Value)>::new())
        .expect("a->b");
    store
        .insert_directed_edge_named(b, c, Some("QtyHopRoad"), Vec::<(&str, Value)>::new())
        .expect("b->c");
    store
        .insert_directed_edge_named(a, dead_end, Some("QtyHopRoad"), Vec::<(&str, Value)>::new())
        .expect("a->dead");

    let plan = plan_gql(
        "MATCH (a:QtyHopA)((u)-[e:QtyHopRoad]->(v)){2,2}(c:QtyHopC) \
         RETURN CARDINALITY(u) AS u_hops, CARDINALITY(v) AS v_hops, CARDINALITY(e) AS e_hops",
    );
    let group_expand = plan.ops.iter().find_map(|op| match op {
        PlanOp::Expand {
            near_group_var,
            far_group_var,
            var_len,
            ..
        } if var_len.is_some() => Some((near_group_var.clone(), far_group_var.clone())),
        _ => None,
    });
    assert_eq!(
        group_expand,
        Some((Some("u".into()), Some("v".into()))),
        "quantified subpath should plan var_len expand with node group vars: {:?}",
        plan.ops
    );

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("quantified subpath node groups");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("u_hops"), Some(&Value::Int64(2)));
    assert_eq!(result.rows[0].get("v_hops"), Some(&Value::Int64(2)));
    assert_eq!(result.rows[0].get("e_hops"), Some(&Value::Int64(2)));
}

#[test]
fn gql_var_len_path_var_binds_traversed_path() {
    let store = GraphStore::new();
    let a = store
        .insert_vertex_named(["PathVarA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["PathVarMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["PathVarC"], Vec::<(&str, Value)>::new())
        .expect("c");
    store
        .insert_directed_edge_named(a, b, Some("PathVarRoad"), Vec::<(&str, Value)>::new())
        .expect("a->b");
    store
        .insert_directed_edge_named(b, c, Some("PathVarRoad"), Vec::<(&str, Value)>::new())
        .expect("b->c");

    let plan = plan_gql(
        "MATCH p = (a:PathVarA)-[e:PathVarRoad]->{2,2}(c:PathVarC) \
         RETURN CARDINALITY(e) AS e_hops",
    );
    let path_expand = plan.ops.iter().find_map(|op| match op {
        PlanOp::Expand {
            path_var,
            emit_path_binding,
            var_len,
            ..
        } if var_len.is_some() => Some((path_var.clone(), *emit_path_binding)),
        _ => None,
    });
    assert_eq!(
        path_expand,
        Some((Some("p".into()), false)),
        "edge group referenced in RETURN keeps path binding pruned when unused: {:?}",
        plan.ops
    );

    let plan_return_p = plan_gql(
        "MATCH p = (a:PathVarA)-[e:PathVarRoad]->{2,2}(c:PathVarC) RETURN p, CARDINALITY(e) AS e_hops",
    );
    let result = store
        .execute_plan_query(&plan_return_p, &params(), GqlExecutionContext::default())
        .expect("var_len path variable");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("e_hops"), Some(&Value::Int64(2)));
    let Some(Value::Path(elements)) = result.rows[0].get("p") else {
        panic!(
            "expected path value for p, got {:?}",
            result.rows[0].get("p")
        );
    };
    assert_eq!(
        elements.len(),
        5,
        "path should alternate vertex/edge for 2 hops"
    );
}

#[test]
fn gql_var_len_where_gleaph_weight_filters_on_last_hop_edge() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["VarLenWgtEqA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["VarLenWgtEqMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["VarLenWgtEqC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("VarLenWgtEqRoad");
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
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &3u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &9u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(a, c, Some(label_id), &5u16.to_le_bytes())
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:VarLenWgtEqA)-[e:VarLenWgtEqRoad]->{1,2}(c:VarLenWgtEqC) \
         WHERE GLEAPH.WEIGHT(e) = 5 RETURN GLEAPH.WEIGHT(e[-1]) AS w",
    );
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("var_len gleaph weight where");

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].get("w"), Some(&Value::Float32(5.0)));
}

#[test]
fn var_len_edge_payload_predicate_fuses_at_each_hop() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["VarLenFusA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["VarLenFusMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["VarLenFusC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("VarLenFusRoad");
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
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &5u16.to_le_bytes())
        .unwrap();

    let plan = plan(vec![
        PlanOp::NodeScan {
            variable: "a".into(),
            label: Some("VarLenFusA".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "a".into(),
            edge: "e".into(),
            dst: "c".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("VarLenFusRoad".into()),
            label_expr: None,
            var_len: Some(VarLenSpec {
                min: 2,
                max: Some(2),
            }),
            indexed_edge_equality: None,
            edge_payload_predicate: Some(EdgePayloadPredicate {
                op: CmpOp::Eq,
                value: ScanValue::Literal(Value::Int64(5)),
            }),
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
        PlanOp::Project {
            columns: vec![project(var("c"), "c")],
            distinct: false,
        },
    ]);
    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("var_len payload fusion");

    assert_eq!(
        result.rows.len(),
        0,
        "first hop weight 7 must be pruned by per-hop payload predicate"
    );
}

#[test]
fn gql_var_len_where_gleaph_weight_fuses_payload_predicate_per_hop() {
    let store = GraphStore::new();
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};
    let a = store
        .insert_vertex_named(["VarLenFusGqlA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["VarLenFusGqlMid"], Vec::<(&str, Value)>::new())
        .expect("b");
    let c = store
        .insert_vertex_named(["VarLenFusGqlC"], Vec::<(&str, Value)>::new())
        .expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("VarLenFusGqlRoad");
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
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &5u16.to_le_bytes())
        .unwrap();
    store
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &5u16.to_le_bytes())
        .unwrap();

    let plan = plan_gql(
        "MATCH (a:VarLenFusGqlA)-[e:VarLenFusGqlRoad]->{2,2}(c:VarLenFusGqlC) \
         WHERE GLEAPH.WEIGHT(e) = 5 RETURN c",
    );
    let has_payload_fusion = plan.ops.iter().any(|op| {
        matches!(
            op,
            PlanOp::Expand {
                edge_payload_predicate: Some(_),
                var_len: Some(_),
                ..
            } | PlanOp::ExpandFilter {
                edge_payload_predicate: Some(_),
                var_len: Some(_),
                ..
            }
        )
    });
    assert!(
        has_payload_fusion,
        "planner should fuse GLEAPH.WEIGHT into var_len expand: {:?}",
        plan.ops
    );

    let result = store
        .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
        .expect("var_len gql payload fusion");

    assert_eq!(result.rows.len(), 1);
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
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
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
        &GqlExecutionContext::default(),
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
        &GqlExecutionContext::default(),
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
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
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
        &GqlExecutionContext::default(),
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
        shard_id: ShardId::new(99),
        neighbor_vertex_id: GlobalVertexId::new(ShardId::new(0), 1),
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
