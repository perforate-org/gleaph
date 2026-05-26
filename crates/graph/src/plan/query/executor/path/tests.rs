mod path_test_helpers {
    use crate::plan::query::executor::test_support::*;
    use gleaph_gql::Value;
    use gleaph_gql::types::PathElement;
    use gleaph_gql_planner::plan::PhysicalPlan;
    use gleaph_graph_kernel::entry::EdgeLabelId;
    use gleaph_graph_kernel::path::{GraphPathEdgeId, GraphPathVertexId};

    pub fn path_column<'a>(result: &'a PlanQueryResult, column: &str) -> &'a [PathElement] {
        match result.rows.first().and_then(|row| row.get(column)) {
            Some(Value::Path(elements)) => elements,
            other => panic!("expected path column {column}, got {other:?}"),
        }
    }

    pub fn vertex_path_id(element: &PathElement) -> GraphPathVertexId {
        match element {
            PathElement::Vertex(id) => {
                GraphPathVertexId::try_from_slice(id.as_ref()).expect("vertex path id")
            }
            other => panic!("expected vertex path element, got {other:?}"),
        }
    }

    pub fn assert_path_vertex_local(store: &GraphStore, element: &PathElement, local: VertexId) {
        assert_eq!(
            vertex_path_id(element).logical_vertex_id,
            store
                .logical_vertex_id(local)
                .expect("logical vertex id for local vertex")
        );
    }

    pub fn edge_path_id(element: &PathElement) -> GraphPathEdgeId {
        match element {
            PathElement::Edge(id) => GraphPathEdgeId::try_from_slice(id.as_ref()).expect("edge id"),
            other => panic!("expected edge path element, got {other:?}"),
        }
    }

    pub fn catalog_edge_label(store: &GraphStore, label_name: &str) -> EdgeLabelId {
        store.edge_label_id(label_name).expect("edge label")
    }

    pub fn gleaph_weight_call(edge_var: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
            args: vec![Expr::var(edge_var)],
            distinct: false,
        })
    }

    pub fn scaled_gleaph_weight_cost(edge_var: &str, scale_param: &str) -> Expr {
        Expr::new(ExprKind::BinaryOp {
            left: Box::new(gleaph_weight_call(edge_var)),
            op: BinaryOp::Mul,
            right: Box::new(Expr::new(ExprKind::Parameter(scale_param.to_owned()))),
        })
    }

    pub fn setup_weighted_road_graph(store: &GraphStore) -> (VertexId, VertexId, VertexId) {
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        let a = store
            .insert_vertex_named(["WgtA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let b = store
            .insert_vertex_named(["WgtB"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        let c = store
            .insert_vertex_named(["WgtC"], Vec::<(&str, Value)>::new())
            .expect("insert c");
        let label_id = store
            .get_or_insert_edge_label_id("WgtRoad")
            .expect("road label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        let road = catalog_edge_label(store, "WgtRoad");
        store
            .insert_directed_edge_with_inline_value(a, b, Some(road), 1)
            .expect("a->b");
        store
            .insert_directed_edge_with_inline_value(b, c, Some(road), 1)
            .expect("b->c");
        store
            .insert_directed_edge_with_inline_value(a, c, Some(road), 100)
            .expect("a->c");
        (a, b, c)
    }

    pub fn weighted_shortest_plan_with_cost(cost: Expr) -> PhysicalPlan {
        weighted_shortest_plan_with_cost_mode(cost, ShortestMode::AnyShortest)
    }

    pub fn weighted_shortest_plan_with_cost_mode(cost: Expr, mode: ShortestMode) -> PhysicalPlan {
        plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: cost,
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ])
    }

    pub fn weighted_2_24_precision_cost_expr() -> Expr {
        Expr::new(ExprKind::CaseSimple {
            operand: Box::new(gleaph_weight_call("e")),
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(1.0))),
                    result: Expr::new(ExprKind::Literal(Value::Float64(8_388_608.0))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(100.0))),
                    result: Expr::new(ExprKind::Literal(Value::Float64(16_777_217.0))),
                },
            ],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Float64(0.0))))),
        })
    }

    pub fn cast_expr_to_float32(expr: Expr) -> Expr {
        Expr::new(ExprKind::Cast {
            expr: Box::new(expr),
            target: gleaph_gql::ast::ValueType::Float32 {
                keyword: gleaph_gql::ast::Keyword::new("FLOAT32"),
            },
        })
    }

    pub fn weighted_2_24_precision_cost_expr_float32() -> Expr {
        Expr::new(ExprKind::CaseSimple {
            operand: Box::new(gleaph_weight_call("e")),
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(1.0))),
                    result: cast_expr_to_float32(Expr::new(ExprKind::Literal(Value::Float64(
                        8_388_608.0,
                    )))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(100.0))),
                    result: cast_expr_to_float32(Expr::new(ExprKind::Literal(Value::Float64(
                        16_777_217.0,
                    )))),
                },
            ],
            else_clause: Some(Box::new(cast_expr_to_float32(Expr::new(
                ExprKind::Literal(Value::Float64(0.0)),
            )))),
        })
    }

    pub fn weighted_decimal_precision_cost_expr() -> Expr {
        use gleaph_gql::types::Decimal;
        Expr::new(ExprKind::CaseSimple {
            operand: Box::new(gleaph_weight_call("e")),
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(1.0))),
                    result: Expr::new(ExprKind::Literal(Value::Decimal(
                        Decimal::parse("0.10").expect("decimal"),
                    ))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(100.0))),
                    result: Expr::new(ExprKind::Literal(Value::Decimal(
                        Decimal::parse("0.21").expect("decimal"),
                    ))),
                },
            ],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Decimal(
                Decimal::from_i64(0),
            ))))),
        })
    }

    pub fn weighted_wide_integer_precision_cost_expr() -> Expr {
        Expr::new(ExprKind::CaseSimple {
            operand: Box::new(gleaph_weight_call("e")),
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(1.0))),
                    result: Expr::new(ExprKind::Literal(Value::Int64(1_000_000))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(100.0))),
                    result: Expr::new(ExprKind::Literal(Value::Int64(2_000_001))),
                },
            ],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int64(0))))),
        })
    }
}

mod tests {
    use super::super::{
        ShortestFixedLabelExpand, local_shard_id,
        materialize::materialize_path_from_search_states,
        weighted::{WeightedCost, WeightedCostOrderKey, decode_direct_gleaph_weight_hop_cost},
        weighted_shortest_can_use_hop_count, weighted_shortest_paths_between,
    };
    use super::path_test_helpers::*;
    use crate::plan::query::executor::test_support::*;
    use ic_stable_lara::traits::CsrEdge;

    #[test]
    fn shortest_path_optional_hit_with_dst_label_narrowing() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["OptSpHitA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let b = store
            .insert_vertex_named(["OptSpHitB", "OptSpHitC"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        store
            .insert_directed_edge_named(a, b, Some("OptSpHitRel"), Vec::<(&str, Value)>::new())
            .expect("a->b");
        let gql = "MATCH (a:OptSpHitA) OPTIONAL MATCH (a)-[e:OptSpHitRel]->(b:OptSpHitB) \
                   MATCH ANY SHORTEST (a)-[e2:OptSpHitRel]->(b:OptSpHitC) RETURN a, b";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("shortest path after optional hit with label narrowing");
        assert_eq!(
            result.rows.len(),
            1,
            "optional hit with stricter shortest-path dst label must return one row: {:?}",
            result.rows
        );
    }
    #[test]
    fn shortest_path_after_optional_miss_drops_null_destination_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptSpA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        store
            .insert_vertex_named(["OptSpB"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        store
            .get_or_insert_edge_label_id("OptSpRel")
            .expect("edge label");
        let gql = "MATCH (a:OptSpA) OPTIONAL MATCH (a)-[e:OptSpRel]->(b:OptSpB) \
                   MATCH ANY SHORTEST (a)-[e2:OptSpRel]->(b) RETURN a, b";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("shortest path after optional miss should not error");
        assert!(
            result.rows.is_empty(),
            "optional miss leaves b null; shortest path should drop the row: {:?}",
            result.rows
        );
    }
    #[test]
    fn hop_cost_admits_large_finite_float64() {
        let cost = WeightedCost::from_value(Value::Float64(1e40)).expect("large finite hop cost");
        assert!(matches!(cost.value, Value::Float64(v) if v == 1e40));
    }
    #[test]
    fn hop_cost_rejects_null() {
        let err = WeightedCost::from_value(Value::Null).expect_err("null hop cost");
        assert!(matches!(
            err,
            PlanQueryError::GleaphCost {
                message: msg
            } if msg == "shortest-path edge cost must not be NULL"
        ));
    }
    #[test]
    fn hop_cost_rejects_nan() {
        let err = WeightedCost::from_value(Value::Float64(f64::NAN)).expect_err("nan hop cost");
        assert!(matches!(
            err,
            PlanQueryError::GleaphCost {
                message: msg
            } if msg == "shortest-path edge cost must be finite"
        ));
    }
    #[test]
    fn hop_cost_rejects_negative() {
        let err = WeightedCost::from_value(Value::Int32(-1)).expect_err("negative hop cost");
        assert!(matches!(
            err,
            PlanQueryError::GleaphCost {
                message: msg
            } if msg == "shortest-path edge cost must be non-negative"
        ));
    }
    #[test]
    fn weighted_literal_cost_uses_hop_count_when_equivalent() {
        let positive = Expr::new(ExprKind::Literal(Value::Int32(1)));
        let zero = Expr::new(ExprKind::Literal(Value::Int32(0)));
        let negative = Expr::new(ExprKind::Literal(Value::Int32(-1)));

        assert!(weighted_shortest_can_use_hop_count(
            ShortestMode::AnyShortest,
            &zero
        ));
        assert!(weighted_shortest_can_use_hop_count(
            ShortestMode::AllShortest,
            &positive
        ));
        assert!(!weighted_shortest_can_use_hop_count(
            ShortestMode::AllShortest,
            &zero
        ));
        assert!(!weighted_shortest_can_use_hop_count(
            ShortestMode::AnyShortest,
            &negative
        ));
    }
    #[test]
    fn weighted_cost_add_overflow_errors() {
        let left = WeightedCost::from_value(Value::Float64(f64::MAX)).expect("left");
        let right = WeightedCost::from_value(Value::Float64(f64::MAX)).expect("right");
        let err = left.checked_add(&right).expect_err("overflow add");
        assert!(matches!(
            err,
            PlanQueryError::GleaphCost {
                message: msg
            } if msg == "shortest-path edge cost overflowed or became non-finite"
                || msg == "shortest-path edge cost must be finite"
        ));
    }
    #[test]
    fn element_id_returns_graph_kernel_bytes_for_vertices_and_edges() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let a = store
            .insert_vertex_named(["ElementIdSource"], [("name", Value::Text("a".into()))])
            .expect("insert a");
        let b = store
            .insert_vertex_named(["ElementIdTarget"], [("name", Value::Text("b".into()))])
            .expect("insert b");
        let edge = store
            .insert_directed_edge_named(a, b, Some("ElementIdRel"), Vec::<(&str, Value)>::new())
            .expect("insert edge");
        let plan = plan_gql(
            "MATCH (a:ElementIdSource)-[e:ElementIdRel]->(b:ElementIdTarget) \
             RETURN ELEMENT_ID(a) AS aid, ELEMENT_ID(e) AS eid",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("element ids");

        assert_eq!(result.rows.len(), 1);
        let vertex_id = GraphPathVertexId::try_from_slice(bytes_column(&result, "aid"))
            .expect("vertex element id");
        assert_eq!(
            vertex_id.logical_vertex_id,
            store.logical_vertex_id(a).expect("logical id for a")
        );
        let edge_id =
            GraphPathEdgeId::try_from_slice(bytes_column(&result, "eid")).expect("edge element id");
        assert_eq!(edge_id.shard_id, 7);
        assert_eq!(edge_id.owner_vertex_id, edge.owner_vertex_id);
        assert_eq!(
            edge_id.edge_slot_index,
            EdgeSlotIndex::from_raw(edge.slot_index)
        );
    }
    #[test]
    fn element_id_of_null_optional_binding_returns_null() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["ElementIdOptional"], Vec::<(&str, Value)>::new())
            .expect("insert vertex");
        let plan = plan_gql(
            "MATCH (n:ElementIdOptional) \
             OPTIONAL MATCH (n)-[e:ElementIdMissing]->(m:ElementIdMissingTarget) \
             RETURN ELEMENT_ID(e) AS eid",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("optional element id");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("eid"), Some(&Value::Null));
    }
    #[test]
    fn shortest_path_binds_opaque_path_ids() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let a = store
            .insert_vertex_named(["ShortestPathSource"], [("name", Value::Text("a".into()))])
            .expect("insert a");
        let b = store
            .insert_vertex_named(["ShortestPathMid"], [("name", Value::Text("b".into()))])
            .expect("insert b");
        let c = store
            .insert_vertex_named(["ShortestPathTarget"], [("name", Value::Text("c".into()))])
            .expect("insert c");
        let ab = store
            .insert_directed_edge_named(a, b, Some("ShortestPathRel"), Vec::<(&str, Value)>::new())
            .expect("insert ab");
        let bc = store
            .insert_directed_edge_named(b, c, Some("ShortestPathRel"), Vec::<(&str, Value)>::new())
            .expect("insert bc");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("ShortestPathSource".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("ShortestPathTarget".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("ShortestPathRel".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(3),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute shortest path");

        assert_eq!(result.rows.len(), 1);
        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 5);
        assert_path_vertex_local(&store, &elements[0], a);
        assert_eq!(
            edge_path_id(&elements[1]).owner_vertex_id,
            ab.owner_vertex_id
        );
        assert_eq!(
            edge_path_id(&elements[1]).edge_slot_index,
            EdgeSlotIndex::from_raw(ab.slot_index)
        );
        assert_path_vertex_local(&store, &elements[2], b);
        assert_eq!(
            edge_path_id(&elements[3]).owner_vertex_id,
            bc.owner_vertex_id
        );
        assert_eq!(
            edge_path_id(&elements[3]).edge_slot_index,
            EdgeSlotIndex::from_raw(bc.slot_index)
        );
        assert_path_vertex_local(&store, &elements[4], c);
    }
    #[test]
    fn shortest_path_zero_hop_binds_null_edge_and_single_vertex_path() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["ShortestPathZero"], [("name", Value::Text("a".into()))])
            .expect("insert a");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("ShortestPathZero".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "a".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: None,
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 0,
                    max: Some(3),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p"), project(var("e"), "e")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute zero-hop shortest path");

        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 1);
        assert_path_vertex_local(&store, &elements[0], a);
        assert_eq!(result.rows[0].get("e"), Some(&Value::Null));
    }
    #[test]
    fn all_shortest_path_returns_all_equal_depth_paths() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["AllShortestSource"], [("name", Value::Text("a".into()))])
            .expect("insert a");
        let b1 = store
            .insert_vertex_named(["AllShortestMid"], [("name", Value::Text("b1".into()))])
            .expect("insert b1");
        let b2 = store
            .insert_vertex_named(["AllShortestMid"], [("name", Value::Text("b2".into()))])
            .expect("insert b2");
        let c = store
            .insert_vertex_named(["AllShortestTarget"], [("name", Value::Text("c".into()))])
            .expect("insert c");
        store
            .insert_directed_edge_named(a, b1, Some("AllShortestRel"), Vec::<(&str, Value)>::new())
            .expect("insert a-b1");
        store
            .insert_directed_edge_named(b1, c, Some("AllShortestRel"), Vec::<(&str, Value)>::new())
            .expect("insert b1-c");
        store
            .insert_directed_edge_named(a, b2, Some("AllShortestRel"), Vec::<(&str, Value)>::new())
            .expect("insert a-b2");
        store
            .insert_directed_edge_named(b2, c, Some("AllShortestRel"), Vec::<(&str, Value)>::new())
            .expect("insert b2-c");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("AllShortestSource".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("AllShortestTarget".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AllShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("AllShortestRel".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(3),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute all shortest paths");

        assert_eq!(result.rows.len(), 2);
        let middle_vertices: BTreeSet<gleaph_graph_kernel::federation::LogicalVertexId> = result
            .rows
            .iter()
            .map(|row| match row.get("p") {
                Some(Value::Path(elements)) => vertex_path_id(&elements[2]).logical_vertex_id,
                other => panic!("expected path, got {other:?}"),
            })
            .collect();
        assert_eq!(
            middle_vertices,
            BTreeSet::from([
                store.logical_vertex_id(b1).expect("b1 logical id"),
                store.logical_vertex_id(b2).expect("b2 logical id"),
            ])
        );
    }
    #[test]
    fn shortest_path_rejects_unsupported_mode_and_label_expr() {
        let store = GraphStore::new();
        let k_err = store
            .execute_plan_query(
                &plan(vec![PlanOp::ShortestPath {
                    src: "a".into(),
                    dst: "b".into(),
                    edge: "e".into(),
                    path_var: Some("p".into()),
                    emit_edge_binding: true,
                    emit_path_binding: true,
                    mode: ShortestMode::ShortestK(2),
                    direction: EdgeDirection::PointingRight,
                    label: None,
                    label_expr: None,
                    var_len: None,
                    cost: ShortestPathCost::HopCount,
                }]),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect_err("ShortestK should be unsupported");
        assert!(matches!(
            k_err,
            PlanQueryError::UnsupportedOp("ShortestPath.ShortestK")
        ));

        let label_expr_err = store
            .execute_plan_query(
                &plan(vec![PlanOp::ShortestPath {
                    src: "a".into(),
                    dst: "b".into(),
                    edge: "e".into(),
                    path_var: Some("p".into()),
                    emit_edge_binding: true,
                    emit_path_binding: true,
                    mode: ShortestMode::AnyShortest,
                    direction: EdgeDirection::PointingRight,
                    label: None,
                    label_expr: Some(LabelExpr::Name("Rel".into())),
                    var_len: None,
                    cost: ShortestPathCost::HopCount,
                }]),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect_err("label_expr should be unsupported");
        assert!(matches!(
            label_expr_err,
            PlanQueryError::UnsupportedOp("ShortestPath.label_expr")
        ));
    }
    #[test]
    fn weighted_shortest_path_cost_expr_uses_query_parameters() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: scaled_gleaph_weight_cost("e", "scale"),
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let mut parameters = params();
        parameters.insert("scale".into(), Value::Float32(1.0));
        let result = store
            .execute_plan_query(&plan, &parameters, GqlExecutionContext::default())
            .expect("parameterized weighted shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(
            elements.len(),
            5,
            "GLEAPH.WEIGHT(e) * $scale with scale=1 should match unscaled weighted shortest path"
        );
        assert_path_vertex_local(&store, &elements[4], c);
    }
    #[test]
    fn weighted_shortest_any_prefers_exact_float64_cost_at_2_24() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(weighted_2_24_precision_cost_expr()),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("float64 precision weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_all_shortest_does_not_epsilon_tie_distinct_costs() {
        let store = GraphStore::new();
        setup_weighted_road_graph(&store);
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost_mode(
                    weighted_2_24_precision_cost_expr(),
                    ShortestMode::AllShortest,
                ),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("all-shortest with distinct float64 costs");
        assert_eq!(
            result.rows.len(),
            1,
            "distinct float64 costs must not be epsilon-tied"
        );
        assert_eq!(path_column(&result, "p").len(), 5);
    }
    #[test]
    fn weighted_shortest_all_returns_all_equal_cost_paths() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["WgtAllSrc"], [("name", Value::Text("a".into()))])
            .expect("insert a");
        let b1 = store
            .insert_vertex_named(["WgtAllMid"], [("name", Value::Text("b1".into()))])
            .expect("insert b1");
        let b2 = store
            .insert_vertex_named(["WgtAllMid"], [("name", Value::Text("b2".into()))])
            .expect("insert b2");
        let c = store
            .insert_vertex_named(["WgtAllDst"], [("name", Value::Text("c".into()))])
            .expect("insert c");
        store
            .insert_directed_edge_named(a, b1, Some("WgtAllRel"), Vec::<(&str, Value)>::new())
            .expect("insert a-b1");
        store
            .insert_directed_edge_named(b1, c, Some("WgtAllRel"), Vec::<(&str, Value)>::new())
            .expect("insert b1-c");
        store
            .insert_directed_edge_named(a, b2, Some("WgtAllRel"), Vec::<(&str, Value)>::new())
            .expect("insert a-b2");
        store
            .insert_directed_edge_named(b2, c, Some("WgtAllRel"), Vec::<(&str, Value)>::new())
            .expect("insert b2-c");
        let zero_cost = Expr::new(ExprKind::Literal(Value::Int32(0)));
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtAllSrc".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtAllDst".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AllShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtAllRel".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(3),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: zero_cost,
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("weighted all-shortest with equal zero costs");

        assert_eq!(result.rows.len(), 2);
        let middle_vertices: BTreeSet<gleaph_graph_kernel::federation::LogicalVertexId> = result
            .rows
            .iter()
            .map(|row| match row.get("p") {
                Some(Value::Path(elements)) => vertex_path_id(&elements[2]).logical_vertex_id,
                other => panic!("expected path, got {other:?}"),
            })
            .collect();
        assert_eq!(
            middle_vertices,
            BTreeSet::from([
                store.logical_vertex_id(b1).expect("b1 logical id"),
                store.logical_vertex_id(b2).expect("b2 logical id"),
            ])
        );
    }
    #[test]
    fn weighted_shortest_cast_float32_restores_f32_precision_limits() {
        let store = GraphStore::new();
        setup_weighted_road_graph(&store);
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost_mode(
                    weighted_2_24_precision_cost_expr_float32(),
                    ShortestMode::AllShortest,
                ),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("float32-cast weighted shortest path");
        assert_eq!(
            result.rows.len(),
            2,
            "float32-cast costs should tie at 2^24 precision"
        );
    }
    #[test]
    fn weighted_shortest_decimal_cost_accumulates_exactly() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(weighted_decimal_precision_cost_expr()),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("decimal precision weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_wide_integer_cost_accumulates() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(weighted_wide_integer_precision_cost_expr()),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("wide-integer precision weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_floor_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Floor(Box::new(gleaph_weight_call("e"))));
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("floor-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Float32 {
                keyword: gleaph_gql::ast::Keyword::new("FLOAT32"),
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("cast-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_float128_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Float128,
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("float128-cast weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_float256_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Float256,
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("float256-cast weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_int_precision_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 10,
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("int-precision-cast weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_float_precision_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::FloatPrecision {
                precision: 24,
                scale: None,
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("float-precision-cast weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_int8_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Int8 {
                keyword: gleaph_gql::ast::Keyword::new("INT8"),
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("int8-cast-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_decimal_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: None,
                scale: None,
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("decimal-cast-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_decimal_precision_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: Some(10),
                scale: Some(2),
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("decimal-precision-cast weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_coalesce_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Coalesce(vec![
            gleaph_weight_call("e"),
            Expr::new(ExprKind::Literal(Value::Float32(1.0))),
        ]));
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("coalesce-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_case_wrapped_cost_runs() {
        use gleaph_gql::ast::WhenClause;
        use gleaph_gql::token::Span;
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::CaseSimple {
            operand: Box::new(Expr::var("e")),
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Null)),
                result: gleaph_weight_call("e"),
            }],
            else_clause: Some(Box::new(gleaph_weight_call("e"))),
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("case-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_prefers_lower_total_cost_over_fewer_hops() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: gleaph_weight_call("e"),
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("weighted shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 5, "expected 2-hop weighted shortest path");
        assert_path_vertex_local(&store, &elements[4], c);
    }

    /// Graph where a cheaper arrival at `x` exhausts the hop bound while a higher-cost arrival
    /// can still reach `dst` (s->x cost 2 depth 1, s->a->x cost 1 depth 2, x->dst cost 1, max=2).
    fn setup_hop_bound_cheaper_vertex_unusable_graph(store: &GraphStore) -> (VertexId, VertexId) {
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        let s = store
            .insert_vertex_named(["WgtA"], Vec::<(&str, Value)>::new())
            .expect("insert s");
        let a = store
            .insert_vertex_named(["WgtB"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let x = store
            .insert_vertex_named(["WgtHub"], Vec::<(&str, Value)>::new())
            .expect("insert x");
        let dst = store
            .insert_vertex_named(["WgtC"], Vec::<(&str, Value)>::new())
            .expect("insert dst");
        let label_id = store
            .get_or_insert_edge_label_id("WgtRoad")
            .expect("road label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        let road = catalog_edge_label(store, "WgtRoad");
        store
            .insert_directed_edge_with_inline_value(s, x, Some(road), 2)
            .expect("s->x");
        store
            .insert_directed_edge_with_inline_value(s, a, Some(road), 0)
            .expect("s->a");
        store
            .insert_directed_edge_with_inline_value(a, x, Some(road), 1)
            .expect("a->x");
        store
            .insert_directed_edge_with_inline_value(x, dst, Some(road), 1)
            .expect("x->dst");
        (s, dst)
    }
    #[test]
    fn weighted_shortest_higher_cost_vertex_state_can_still_reach_dst_under_hop_bound() {
        let store = GraphStore::new();
        let (s, dst) = setup_hop_bound_cheaper_vertex_unusable_graph(&store);
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(2),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: gleaph_weight_call("e"),
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("hop-bound weighted shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 5, "expected s->x->dst (2 edges)");
        assert_path_vertex_local(&store, &elements[0], s);
        assert_path_vertex_local(&store, &elements[4], dst);
    }

    /// Graph where a longer prefix reaches `mid` with lower total cost after a stale higher-cost
    /// entry is already in the heap; min-queue ordering and `found_min_cost` skip the stale pop.
    fn setup_stale_mid_diamond_graph(store: &GraphStore) -> (VertexId, VertexId) {
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        let s = store
            .insert_vertex_named(["WgtA"], Vec::<(&str, Value)>::new())
            .expect("insert s");
        let a = store
            .insert_vertex_named(["WgtB"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let mid = store
            .insert_vertex_named(["WgtHub"], Vec::<(&str, Value)>::new())
            .expect("insert mid");
        let dst = store
            .insert_vertex_named(["WgtC"], Vec::<(&str, Value)>::new())
            .expect("insert dst");
        let label_id = store
            .get_or_insert_edge_label_id("WgtRoad")
            .expect("road label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        let road = catalog_edge_label(store, "WgtRoad");
        store
            .insert_directed_edge_with_inline_value(s, mid, Some(road), 10)
            .expect("s->mid");
        store
            .insert_directed_edge_with_inline_value(s, a, Some(road), 5)
            .expect("s->a");
        store
            .insert_directed_edge_with_inline_value(a, mid, Some(road), 1)
            .expect("a->mid");
        store
            .insert_directed_edge_with_inline_value(mid, dst, Some(road), 0)
            .expect("mid->dst");
        (s, dst)
    }
    #[test]
    fn stale_mid_diamond_edge_bindings_carry_expected_weights() {
        use gleaph_gql_planner::plan::{PlanOp, ShortestMode, VarLenSpec};
        let store = GraphStore::new();
        let (s, _dst) = setup_stale_mid_diamond_graph(&store);
        let road = catalog_edge_label(&store, "WgtRoad");
        let cost_expr = gleaph_weight_call("e");
        let decoders = crate::plan::query::gleaph_weight::prepare_gleaph_weight_decoders(
            &store,
            &[PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: None,
                emit_edge_binding: true,
                emit_path_binding: false,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: gleaph_gql_planner::plan::ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: cost_expr.clone(),
                },
            }],
        )
        .expect("decoders")
        .expect("table");
        let decoder = decoders.get("e").expect("edge decoder");
        let mut weights = BTreeMap::new();
        store
            .for_each_directed_out_edges_for_label_unchecked(s, road, |edge| {
                let binding =
                    edge_binding_for_expand(&store, s, EdgeDirection::PointingRight, edge)
                        .expect("binding");
                let w = crate::plan::query::gleaph_weight::decode_traversal_edge_weight(
                    &store,
                    binding.handle,
                    binding.value_len(),
                    binding.value_bytes_slice(),
                    binding.inline_value(),
                    Some(decoder),
                )
                .expect("decode");
                weights.insert(edge.neighbor_vid(), w);
            })
            .expect("for_each");
        let mut sorted: Vec<_> = weights.into_values().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(sorted, vec![5.0, 10.0]);
    }
    #[test]
    fn stale_mid_diamond_shortest_expand_hop_costs_are_5_10_and_1() {
        use gleaph_gql_planner::plan::{PlanOp, ShortestMode, VarLenSpec};
        let store = GraphStore::new();
        let (s, dst) = setup_stale_mid_diamond_graph(&store);
        let road = catalog_edge_label(&store, "WgtRoad");
        let cost_expr = gleaph_weight_call("e");
        let decoders = crate::plan::query::gleaph_weight::prepare_gleaph_weight_decoders(
            &store,
            &[PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: None,
                emit_edge_binding: true,
                emit_path_binding: false,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: gleaph_gql_planner::plan::ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: cost_expr.clone(),
                },
            }],
        )
        .expect("decoders")
        .expect("table");
        let decoder = decoders.get("e").expect("edge decoder");
        let prep = ShortestFixedLabelExpand::new(EdgeDirection::PointingRight, road).expect("prep");
        let mut from_s = Vec::new();
        prep.expand_into(&store, s, &mut from_s).expect("from s");
        let mut hop_costs = Vec::new();
        for (edge_dst, binding) in from_s {
            let hop = decode_direct_gleaph_weight_hop_cost(&store, decoder, binding).expect("hop");
            hop_costs.push((
                u32::from(match edge_dst {
                    ExpandDst::Local(v) => v,
                    ExpandDst::Remote(_) => panic!("remote"),
                }),
                hop.order_key,
            ));
        }
        hop_costs.sort_by_key(|(vid, _)| *vid);
        assert_eq!(hop_costs.len(), 2);
        assert!(
            matches!(hop_costs[0].1, WeightedCostOrderKey::Float64(v) if (v - 5.0).abs() < f64::EPSILON)
        );
        assert!(
            matches!(hop_costs[1].1, WeightedCostOrderKey::Float64(v) if (v - 10.0).abs() < f64::EPSILON)
        );

        let detour = hop_costs[0].0;
        store
            .for_each_directed_out_edges_for_label_unchecked(VertexId::from(detour), road, |edge| {
                assert_eq!(edge.inline_value_u16(), 1, "raw CSR edge value");
                assert_eq!(edge.value_bytes(), &[1, 0]);
                let handle = EdgeHandle {
                    owner_vertex_id: VertexId::from(detour),
                    label_id: ic_stable_lara::BucketLabelKey::from_raw(edge.label_id),
                    slot_index: edge.edge_slot_index.raw(),
                };
                let record = store
                    .find_outgoing_edge_record(handle)
                    .expect("lookup")
                    .expect("record");
                assert_eq!(
                    record.value_bytes(),
                    edge.value_bytes(),
                    "find_outgoing_edge_record must match iterated edge bytes"
                );
            })
            .expect("out from detour");
        let mut from_detour = Vec::new();
        prep.expand_into(&store, VertexId::from(detour), &mut from_detour)
            .expect("from detour");
        assert_eq!(from_detour.len(), 1);
        let binding = from_detour[0].1;
        assert_eq!(
            binding.inline_value(),
            1,
            "binding inline_value for detour->mid"
        );
        assert_eq!(
            binding.value_bytes_slice(),
            &[1, 0],
            "binding value_bytes for detour->mid"
        );
        let hop =
            decode_direct_gleaph_weight_hop_cost(&store, decoder, binding).expect("detour hop");
        assert!(
            matches!(hop.order_key, WeightedCostOrderKey::Float64(v) if (v - 1.0).abs() < f64::EPSILON),
            "detour->mid hop cost, got {:?}",
            hop.order_key
        );
        let _ = dst;
    }
    #[test]
    fn stale_mid_diamond_weighted_search_finds_cheaper_three_hop_path() {
        use gleaph_gql_planner::plan::{PlanOp, ShortestMode, VarLenSpec};
        let store = GraphStore::new();
        let (s, dst) = setup_stale_mid_diamond_graph(&store);
        let road = catalog_edge_label(&store, "WgtRoad");
        let cost_expr = gleaph_weight_call("e");
        let decoders = crate::plan::query::gleaph_weight::prepare_gleaph_weight_decoders(
            &store,
            &[PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: gleaph_gql_planner::plan::ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: cost_expr.clone(),
                },
            }],
        )
        .expect("decoders")
        .expect("decoder table");
        let search = weighted_shortest_paths_between(
            &store,
            s,
            dst,
            EdgeDirection::PointingRight,
            Some(road),
            &Some(VarLenSpec {
                min: 1,
                max: Some(5),
            }),
            "e",
            &cost_expr,
            ShortestMode::AnyShortest,
            &BTreeMap::new(),
            Some(&decoders),
            true,
        )
        .expect("search");
        let path = materialize_path_from_search_states(
            &store,
            local_shard_id(&store),
            &search.states,
            *search.found.first().expect("path"),
        );
        let elements = match path {
            Value::Path(elements) => elements,
            other => panic!("unexpected path value: {other:?}"),
        };
        assert_eq!(
            elements.len(),
            7,
            "expected s->detour->mid->dst; got {elements:?}"
        );
    }
    #[test]
    fn weighted_shortest_skips_stale_higher_cost_vertex_entries() {
        let store = GraphStore::new();
        let (s, dst) = setup_stale_mid_diamond_graph(&store);
        let road = catalog_edge_label(&store, "WgtRoad");
        let mut weights = Vec::new();
        store
            .for_each_directed_out_edges_for_label_unchecked(s, road, |edge| {
                weights.push(edge.inline_value_u16());
            })
            .expect("out edges from s");
        weights.sort_unstable();
        assert_eq!(
            weights,
            vec![5, 10],
            "edge weights from s must be persisted for weighted shortest path"
        );
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: gleaph_weight_call("e"),
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("stale-entry weighted shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 7, "expected s->a->mid->dst (3 edges)");
        assert_path_vertex_local(&store, &elements[6], dst);
        assert_path_vertex_local(&store, &elements[0], s);
    }
    #[test]
    fn weighted_shortest_prefers_zero_weight_detour_over_direct_edge() {
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["WgtA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let c = store
            .insert_vertex_named(["WgtC"], Vec::<(&str, Value)>::new())
            .expect("insert c");
        let d1 = store
            .insert_vertex_named(["WgtD1"], Vec::<(&str, Value)>::new())
            .expect("insert d1");
        let d2 = store
            .insert_vertex_named(["WgtD2"], Vec::<(&str, Value)>::new())
            .expect("insert d2");
        let label_id = store
            .get_or_insert_edge_label_id("WgtRoad")
            .expect("road label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        let road = catalog_edge_label(&store, "WgtRoad");
        store
            .insert_directed_edge_with_inline_value(a, d1, Some(road), 0)
            .expect("a->d1");
        store
            .insert_directed_edge_with_inline_value(a, d2, Some(road), 0)
            .expect("a->d2");
        store
            .insert_directed_edge_with_inline_value(d1, d2, Some(road), 0)
            .expect("d1->d2");
        store
            .insert_directed_edge_with_inline_value(d1, c, Some(road), 0)
            .expect("d1->c");
        store
            .insert_directed_edge_with_inline_value(d2, c, Some(road), 0)
            .expect("d2->c");
        store
            .insert_directed_edge_with_inline_value(a, c, Some(road), 50)
            .expect("a->c direct");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: gleaph_weight_call("e"),
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("zero-weight detour weighted shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(
            elements.len(),
            5,
            "expected 2-hop zero-cost detour a->d1->c, not 1-hop direct edge"
        );
        assert_path_vertex_local(&store, &elements[elements.len() - 1], c);
    }
    #[test]
    fn hop_count_shortest_path_ignores_edge_weights() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("hop-count shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 3, "expected 1-hop unweighted shortest path");
        assert_path_vertex_local(&store, &elements[2], c);
    }
    #[test]
    fn gleaph_weight_in_return_does_not_change_shortest_path_search() {
        let store = GraphStore::new();
        setup_weighted_road_graph(&store);
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![
                    project(var("p"), "p"),
                    project(gleaph_weight_call("e"), "w"),
                ],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("shortest path with gleaph_weight in return");
        let elements = path_column(&result, "p");
        assert_eq!(
            elements.len(),
            3,
            "RETURN GLEAPH.WEIGHT must not affect hop-count search"
        );
        assert!(matches!(result.rows[0].get("w"), Some(Value::Float32(_))));
    }
    #[test]
    fn weighted_shortest_path_literal_overflow_cost_errors() {
        let store = GraphStore::new();
        setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Literal(Value::Float64(f64::NAN)));
        let err = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect_err("non-finite literal cost");
        assert!(matches!(
            err,
            PlanQueryError::GleaphCost {
                message: msg
            } if msg == "shortest-path edge cost must be finite"
        ));
    }
    #[test]
    fn weighted_shortest_path_rejects_missing_weight_profile() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["WgtNoProfileA"], Vec::<(&str, Value)>::new())
            .expect("a");
        let c = store
            .insert_vertex_named(["WgtNoProfileC"], Vec::<(&str, Value)>::new())
            .expect("c");
        store
            .get_or_insert_edge_label_id("WgtNoProfileRoad")
            .expect("road label");
        let road = catalog_edge_label(&store, "WgtNoProfileRoad");
        store.insert_directed_edge(a, c, Some(road)).expect("edge");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtNoProfileA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtNoProfileC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: None,
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtNoProfileRoad".into()),
                label_expr: None,
                var_len: None,
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: gleaph_weight_call("e"),
                },
            },
        ]);
        let err = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect_err("missing profile");
        assert!(matches!(err, PlanQueryError::GleaphWeight { .. }));
    }
}
