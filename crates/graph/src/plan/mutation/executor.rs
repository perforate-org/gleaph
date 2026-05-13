use super::error::PlanMutationError;
use super::expr_evaluator::{MutationPropertyExprEvaluation, MutationPropertyExprEvaluator};
use crate::facade::mutation_executor::GraphMutationExecutor;
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};
use crate::gql_execution_context::GqlExecutionContext;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, RemovePlanItem, SetPlanItem, Str};
use ic_stable_lara::VertexId;
use std::collections::BTreeMap;

pub trait PlanMutationExecutor {
    fn execute_plan_mutations(
        &self,
        plan: &PhysicalPlan,
        execution: GqlExecutionContext,
    ) -> Result<PlanMutationBindings, PlanMutationError>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanMutationBindings {
    pub vertices: BTreeMap<String, VertexId>,
    pub edges: BTreeMap<String, EdgeHandle>,
}

impl PlanMutationExecutor for GraphStore {
    fn execute_plan_mutations(
        &self,
        plan: &PhysicalPlan,
        execution: GqlExecutionContext,
    ) -> Result<PlanMutationBindings, PlanMutationError> {
        execute_ops(self, &plan.ops, &BTreeMap::new(), execution)
    }
}

pub fn execute_ops(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, gleaph_gql::Value>,
    execution: GqlExecutionContext,
) -> Result<PlanMutationBindings, PlanMutationError> {
    let mut bindings = PlanMutationBindings::default();
    execute_ops_with_bindings(store, ops, parameters, execution, &mut bindings)?;
    Ok(bindings)
}

fn execute_ops_with_bindings(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, gleaph_gql::Value>,
    execution: GqlExecutionContext,
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    let evaluator = MutationPropertyExprEvaluator::new(parameters, execution.caller);
    for op in ops {
        match op {
            PlanOp::InsertVertex {
                variable,
                labels,
                properties,
            } => {
                let properties = evaluator.resolve_assignments(properties)?;
                let vertex_id =
                    store.insert_vertex_named(labels.iter().map(Str::as_ref), properties)?;
                if let Some(variable) = variable {
                    bindings.vertices.insert(variable.to_string(), vertex_id);
                }
            }
            PlanOp::InsertEdge {
                variable,
                src,
                dst,
                direction,
                labels,
                properties,
            } => {
                let src_id = *bindings.vertices.get(src.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: src.to_string(),
                    }
                })?;
                let dst_id = *bindings.vertices.get(dst.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: dst.to_string(),
                    }
                })?;
                let label = labels.first().map(Str::as_ref);
                let properties = evaluator.resolve_assignments(properties)?;
                let handle = match direction {
                    EdgeDirection::PointingRight => {
                        store.insert_directed_edge_named(src_id, dst_id, label, properties)?
                    }
                    EdgeDirection::PointingLeft => {
                        store.insert_directed_edge_named(dst_id, src_id, label, properties)?
                    }
                    EdgeDirection::Undirected => {
                        store.insert_undirected_edge_named(src_id, dst_id, label, properties)?
                    }
                    other => return Err(PlanMutationError::UnsupportedDirection(*other)),
                };
                if let Some(variable) = variable {
                    bindings.edges.insert(variable.to_string(), handle);
                }
            }
            PlanOp::UseGraph {
                sub_plan: Some(sub_plan),
                ..
            } => execute_ops_with_bindings(store, sub_plan, parameters, execution, bindings)?,
            PlanOp::SetProperties { items } => {
                for item in items {
                    execute_set_item(store, item, &evaluator, bindings)?;
                }
            }
            PlanOp::RemoveProperties { items } => {
                for item in items {
                    execute_remove_item(store, item, bindings)?;
                }
            }
            PlanOp::DeleteVertex { variable } => {
                let vertex_id = *bindings.vertices.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                store.delete_vertex(vertex_id)?;
                bindings.vertices.remove(variable.as_ref());
            }
            PlanOp::DetachDeleteVertex { variable } => {
                let vertex_id = *bindings.vertices.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                store.detach_delete_vertex(vertex_id)?;
                bindings.vertices.remove(variable.as_ref());
            }
            PlanOp::DeleteEdge { variable } => {
                let handle = *bindings.edges.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingElementBinding {
                        variable: variable.to_string(),
                    }
                })?;
                store.delete_edge_by_handle(handle)?;
                bindings.edges.remove(variable.as_ref());
            }
            PlanOp::Materialize { .. } => {}
            other if !is_mutation_op(other) => {}
            other => return Err(PlanMutationError::UnsupportedOp(plan_op_name(other))),
        }
    }
    Ok(())
}

fn execute_set_item(
    store: &GraphStore,
    item: &SetPlanItem,
    evaluator: &impl MutationPropertyExprEvaluation,
    bindings: &PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    match item {
        SetPlanItem::Property {
            variable,
            property,
            value,
        } => {
            let value = evaluator.eval(property.as_ref(), value)?;
            let property_id = store
                .get_or_insert_property_id(property)
                .map_err(GraphStoreError::from)?;

            if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()) {
                store
                    .set_vertex_property(*vertex_id, property_id, value)
                    .map_err(GraphStoreError::from)?;
                return Ok(());
            }

            if let Some(edge) = bindings.edges.get(variable.as_ref()) {
                store
                    .set_edge_property(
                        edge.owner_vertex_id,
                        edge.vertex_edge_id,
                        property_id,
                        value,
                    )
                    .map_err(GraphStoreError::from)?;
                return Ok(());
            }

            Err(PlanMutationError::MissingElementBinding {
                variable: variable.to_string(),
            })
        }
        SetPlanItem::AllProperties { .. } => {
            Err(PlanMutationError::UnsupportedSetItem("AllProperties"))
        }
        SetPlanItem::Label { variable, label } => {
            let label_id = store
                .get_or_insert_label_id(label)
                .map_err(GraphStoreError::from)?;

            if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()) {
                let vertex = store.vertex(*vertex_id).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                let vertex = store
                    .add_vertex_label(*vertex_id, vertex, label_id)
                    .map_err(GraphStoreError::from)?;
                store
                    .set_vertex(*vertex_id, vertex)
                    .map_err(GraphStoreError::from)?;
                return Ok(());
            }

            if bindings.edges.contains_key(variable.as_ref()) {
                return Err(PlanMutationError::UnsupportedSetItem("EdgeLabel"));
            }

            Err(PlanMutationError::MissingElementBinding {
                variable: variable.to_string(),
            })
        }
    }
}

fn execute_remove_item(
    store: &GraphStore,
    item: &RemovePlanItem,
    bindings: &PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    match item {
        RemovePlanItem::Property { variable, property } => {
            let Some(property_id) = store.property_id(property) else {
                return Ok(());
            };

            if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()) {
                store.remove_vertex_property(*vertex_id, property_id);
                return Ok(());
            }

            if let Some(edge) = bindings.edges.get(variable.as_ref()) {
                store.remove_edge_property(edge.owner_vertex_id, edge.vertex_edge_id, property_id);
                return Ok(());
            }

            Err(PlanMutationError::MissingElementBinding {
                variable: variable.to_string(),
            })
        }
        RemovePlanItem::Label { variable, label } => {
            let Some(label_id) = store.label_id(label) else {
                return Ok(());
            };

            if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()) {
                let vertex = store.vertex(*vertex_id).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                let vertex = store.remove_vertex_label(*vertex_id, vertex, label_id);
                store
                    .set_vertex(*vertex_id, vertex)
                    .map_err(GraphStoreError::from)?;
                return Ok(());
            }

            if bindings.edges.contains_key(variable.as_ref()) {
                return Err(PlanMutationError::UnsupportedRemoveItem("EdgeLabel"));
            }

            Err(PlanMutationError::MissingElementBinding {
                variable: variable.to_string(),
            })
        }
    }
}

fn is_mutation_op(op: &PlanOp) -> bool {
    matches!(
        op,
        PlanOp::InsertVertex { .. }
            | PlanOp::InsertEdge { .. }
            | PlanOp::SetProperties { .. }
            | PlanOp::RemoveProperties { .. }
            | PlanOp::DeleteVertex { .. }
            | PlanOp::DetachDeleteVertex { .. }
            | PlanOp::DeleteEdge { .. }
    )
}

fn plan_op_name(op: &PlanOp) -> &'static str {
    match op {
        PlanOp::SetProperties { .. } => "SetProperties",
        PlanOp::RemoveProperties { .. } => "RemoveProperties",
        PlanOp::DeleteVertex { .. } => "DeleteVertex",
        PlanOp::DetachDeleteVertex { .. } => "DetachDeleteVertex",
        PlanOp::DeleteEdge { .. } => "DeleteEdge",
        _ => "PlanOp",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::canonical_undirected_owner;
    use crate::gql_execution_context::GqlExecutionContext;
    use gleaph_gql::Value;
    use gleaph_gql::ast::{BinaryOp, CmpOp, Expr, ExprKind, TruthValue, UnaryOp};
    use gleaph_gql::types::Decimal;
    use gleaph_gql_planner::plan::{PlanDiagnostics, PropertyAssignment};
    use gleaph_graph_kernel::entry::VertexEdgeId;
    use ic_stable_lara::bidirectional::DeferredBidirectionalLaraError;

    #[test]
    fn executes_insert_vertex_and_edge_ops() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec!["Person".into()],
                    properties: vec![PropertyAssignment {
                        name: "name".into(),
                        value: Expr::new(ExprKind::Literal(Value::Text("Alice".into()))),
                    }],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec!["Person".into()],
                    properties: vec![PropertyAssignment {
                        name: "name".into(),
                        value: Expr::new(ExprKind::Literal(Value::Text("Bob".into()))),
                    }],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["KNOWS".into()],
                    properties: vec![PropertyAssignment {
                        name: "since".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(2026))),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute plan mutations");

        let a = bindings.vertices["a"];
        let b = bindings.vertices["b"];
        let edge = bindings.edges["e"];
        let name = store.property_id("name").expect("name property");
        let since = store.property_id("since").expect("since property");

        assert_eq!(
            store.vertex_property(a, name),
            Some(Value::Text("Alice".into()))
        );
        assert_eq!(edge.owner_vertex_id, a);
        assert_eq!(
            store.edge_property(edge.owner_vertex_id, edge.vertex_edge_id, since),
            Some(Value::Int64(2026))
        );
        assert!(
            store
                .out_edges(a)
                .unwrap()
                .iter()
                .any(|candidate| candidate.target == b
                    && candidate.vertex_edge_id == edge.vertex_edge_id)
        );
    }

    #[test]
    fn parameters_can_supply_property_values() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec!["Person".into()],
                properties: vec![PropertyAssignment {
                    name: "name".into(),
                    value: Expr::new(ExprKind::Parameter("name".into())),
                }],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };
        let mut parameters = BTreeMap::new();
        parameters.insert("name".to_owned(), Value::Text("Ada".into()));

        let bindings = execute_ops(
            &store,
            &plan.ops,
            &parameters,
            GqlExecutionContext::default(),
        )
        .expect("execute with params");
        let property = store.property_id("name").expect("name property");

        assert_eq!(
            store.vertex_property(bindings.vertices["n"], property),
            Some(Value::Text("Ada".into()))
        );
    }

    #[test]
    fn set_properties_updates_vertex_and_edge_bindings() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::SetProperties {
                    items: vec![
                        SetPlanItem::Property {
                            variable: "a".into(),
                            property: "name".into(),
                            value: Expr::new(ExprKind::Literal(Value::Text("Alice".into()))),
                        },
                        SetPlanItem::Property {
                            variable: "e".into(),
                            property: "weight".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                        },
                    ],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute set properties");
        let name = store.property_id("name").expect("name property");
        let weight = store.property_id("weight").expect("weight property");
        let edge = bindings.edges["e"];

        assert_eq!(
            store.vertex_property(bindings.vertices["a"], name),
            Some(Value::Text("Alice".into()))
        );
        assert_eq!(
            store.edge_property(edge.owner_vertex_id, edge.vertex_edge_id, weight),
            Some(Value::Int64(7))
        );
    }

    #[test]
    fn remove_properties_removes_vertex_and_edge_properties() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "name".into(),
                        value: Expr::new(ExprKind::Literal(Value::Text("Alice".into()))),
                    }],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "weight".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                    }],
                },
                PlanOp::RemoveProperties {
                    items: vec![
                        RemovePlanItem::Property {
                            variable: "a".into(),
                            property: "name".into(),
                        },
                        RemovePlanItem::Property {
                            variable: "e".into(),
                            property: "weight".into(),
                        },
                        RemovePlanItem::Property {
                            variable: "a".into(),
                            property: "missing_property_is_noop".into(),
                        },
                    ],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute remove properties");
        let name = store.property_id("name").expect("name property");
        let weight = store.property_id("weight").expect("weight property");
        let edge = bindings.edges["e"];

        assert_eq!(store.vertex_property(bindings.vertices["a"], name), None);
        assert_eq!(
            store.edge_property(edge.owner_vertex_id, edge.vertex_edge_id, weight),
            None
        );
    }

    #[test]
    fn set_and_remove_vertex_labels() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec!["Person".into()],
                    properties: vec![],
                },
                PlanOp::SetProperties {
                    items: vec![SetPlanItem::Label {
                        variable: "a".into(),
                        label: "Employee".into(),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute set label");
        let person = store.label_id("Person").expect("person label");
        let employee = store.label_id("Employee").expect("employee label");
        let vertex_id = bindings.vertices["a"];
        let vertex = store.vertex(vertex_id).expect("read vertex");

        assert_eq!(
            store.vertex_labels(vertex_id, vertex),
            vec![person, employee]
        );

        let remove = PhysicalPlan {
            ops: vec![PlanOp::RemoveProperties {
                items: vec![
                    RemovePlanItem::Label {
                        variable: "a".into(),
                        label: "Person".into(),
                    },
                    RemovePlanItem::Label {
                        variable: "a".into(),
                        label: "MissingLabelIsNoop".into(),
                    },
                ],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };
        let mut existing_bindings = bindings;
        execute_ops_with_bindings(
            &store,
            &remove.ops,
            &BTreeMap::new(),
            GqlExecutionContext::default(),
            &mut existing_bindings,
        )
        .expect("execute remove label");
        let vertex = store.vertex(vertex_id).expect("read updated vertex");

        assert_eq!(store.vertex_labels(vertex_id, vertex), vec![employee]);
    }

    #[test]
    fn evaluates_simple_arithmetic_property_expressions() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec![],
                properties: vec![
                    PropertyAssignment {
                        name: "score".into(),
                        value: Expr::new(ExprKind::BinaryOp {
                            left: Box::new(Expr::new(ExprKind::Literal(Value::Int64(4)))),
                            op: BinaryOp::Mul,
                            right: Box::new(Expr::new(ExprKind::BinaryOp {
                                left: Box::new(Expr::new(ExprKind::Parameter("base".into()))),
                                op: BinaryOp::Add,
                                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(3)))),
                            })),
                        }),
                    },
                    PropertyAssignment {
                        name: "name".into(),
                        value: Expr::new(ExprKind::BinaryOp {
                            left: Box::new(Expr::new(ExprKind::Literal(Value::Text("Ada".into())))),
                            op: BinaryOp::Add,
                            right: Box::new(Expr::new(ExprKind::Literal(Value::Text(
                                " Lovelace".into(),
                            )))),
                        }),
                    },
                    PropertyAssignment {
                        name: "negative".into(),
                        value: Expr::new(ExprKind::UnaryOp {
                            op: UnaryOp::Neg,
                            expr: Box::new(Expr::new(ExprKind::Literal(Value::Int64(9)))),
                        }),
                    },
                ],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };
        let mut parameters = BTreeMap::new();
        parameters.insert("base".to_owned(), Value::Int64(2));

        let bindings = execute_ops(
            &store,
            &plan.ops,
            &parameters,
            GqlExecutionContext::default(),
        )
        .expect("execute arithmetic");
        let vertex = bindings.vertices["n"];
        let score = store.property_id("score").expect("score property");
        let name = store.property_id("name").expect("name property");
        let negative = store.property_id("negative").expect("negative property");

        assert_eq!(store.vertex_property(vertex, score), Some(Value::Int64(20)));
        assert_eq!(
            store.vertex_property(vertex, name),
            Some(Value::Text("Ada Lovelace".into()))
        );
        assert_eq!(
            store.vertex_property(vertex, negative),
            Some(Value::Int64(-9))
        );
    }

    #[test]
    fn preserves_decimal_arithmetic() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec![],
                properties: vec![
                    PropertyAssignment {
                        name: "price".into(),
                        value: Expr::new(ExprKind::BinaryOp {
                            left: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(
                                Decimal::parse("10.50").expect("decimal"),
                            )))),
                            op: BinaryOp::Add,
                            right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(2)))),
                        }),
                    },
                    PropertyAssignment {
                        name: "ratio".into(),
                        value: Expr::new(ExprKind::BinaryOp {
                            left: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(
                                Decimal::parse("1.00").expect("decimal"),
                            )))),
                            op: BinaryOp::Div,
                            right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(4)))),
                        }),
                    },
                    PropertyAssignment {
                        name: "negative".into(),
                        value: Expr::new(ExprKind::UnaryOp {
                            op: UnaryOp::Neg,
                            expr: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(
                                Decimal::parse("3.25").expect("decimal"),
                            )))),
                        }),
                    },
                ],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute decimal arithmetic");
        let vertex = bindings.vertices["n"];
        let price = store.property_id("price").expect("price property");
        let ratio = store.property_id("ratio").expect("ratio property");
        let negative = store.property_id("negative").expect("negative property");

        assert_eq!(
            store.vertex_property(vertex, price),
            Some(Value::Decimal(Decimal::parse("12.5").expect("decimal")))
        );
        assert_eq!(
            store.vertex_property(vertex, ratio),
            Some(Value::Decimal(Decimal::parse("0.25").expect("decimal")))
        );
        assert_eq!(
            store.vertex_property(vertex, negative),
            Some(Value::Decimal(Decimal::parse("-3.25").expect("decimal")))
        );
    }

    #[test]
    fn evaluates_boolean_comparison_and_constructed_values() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec![],
                properties: vec![
                    PropertyAssignment {
                        name: "logic".into(),
                        value: Expr::new(ExprKind::Or(
                            Box::new(Expr::new(ExprKind::And(
                                Box::new(Expr::new(ExprKind::Literal(Value::Bool(true)))),
                                Box::new(Expr::new(ExprKind::Literal(Value::Null))),
                            ))),
                            Box::new(Expr::new(ExprKind::Compare {
                                left: Box::new(Expr::new(ExprKind::Literal(Value::Int64(3)))),
                                op: CmpOp::Lt,
                                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(4)))),
                            })),
                        )),
                    },
                    PropertyAssignment {
                        name: "is_unknown".into(),
                        value: Expr::new(ExprKind::IsTruth {
                            expr: Box::new(Expr::new(ExprKind::Literal(Value::Null))),
                            value: TruthValue::Unknown,
                            negated: false,
                        }),
                    },
                    PropertyAssignment {
                        name: "nickname".into(),
                        value: Expr::new(ExprKind::Coalesce(vec![
                            Expr::new(ExprKind::Literal(Value::Null)),
                            Expr::new(ExprKind::Literal(Value::Text("Ada".into()))),
                        ])),
                    },
                    PropertyAssignment {
                        name: "list".into(),
                        value: Expr::new(ExprKind::ListLiteral(vec![
                            Expr::new(ExprKind::Literal(Value::Int64(1))),
                            Expr::new(ExprKind::Literal(Value::Text("two".into()))),
                        ])),
                    },
                    PropertyAssignment {
                        name: "record".into(),
                        value: Expr::new(ExprKind::RecordLiteral(vec![(
                            "ok".into(),
                            Expr::new(ExprKind::Literal(Value::Bool(true))),
                        )])),
                    },
                    PropertyAssignment {
                        name: "bytes".into(),
                        value: Expr::new(ExprKind::Concat(
                            Box::new(Expr::new(ExprKind::Literal(Value::Bytes(vec![1, 2])))),
                            Box::new(Expr::new(ExprKind::Literal(Value::Bytes(vec![3])))),
                        )),
                    },
                ],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute constructed values");
        let vertex = bindings.vertices["n"];
        let logic = store.property_id("logic").expect("logic property");
        let is_unknown = store.property_id("is_unknown").expect("truth property");
        let nickname = store.property_id("nickname").expect("nickname property");
        let list = store.property_id("list").expect("list property");
        let record = store.property_id("record").expect("record property");
        let bytes = store.property_id("bytes").expect("bytes property");

        assert_eq!(
            store.vertex_property(vertex, logic),
            Some(Value::Bool(true))
        );
        assert_eq!(
            store.vertex_property(vertex, is_unknown),
            Some(Value::Bool(true))
        );
        assert_eq!(
            store.vertex_property(vertex, nickname),
            Some(Value::Text("Ada".into()))
        );
        assert_eq!(
            store.vertex_property(vertex, list),
            Some(Value::List(vec![
                Value::Int64(1),
                Value::Text("two".into())
            ]))
        );
        assert_eq!(
            store.vertex_property(vertex, record),
            Some(Value::Record(vec![("ok".into(), Value::Bool(true))]))
        );
        assert_eq!(
            store.vertex_property(vertex, bytes),
            Some(Value::Bytes(vec![1, 2, 3]))
        );
    }

    #[test]
    fn delete_vertex_fails_when_vertex_has_incident_edges() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: None,
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::DeleteVertex {
                    variable: "a".into(),
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        let err = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect_err("delete vertex with edges should fail");
        assert!(matches!(
            err,
            PlanMutationError::Store(GraphStoreError::VertexNotDetached { .. })
        ));
    }

    #[test]
    fn delete_vertex_succeeds_for_isolated_vertex() {
        let store = GraphStore::new();
        let before = store.vertex_count();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "k".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(1))),
                    }],
                },
                PlanOp::DeleteVertex {
                    variable: "a".into(),
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("delete isolated vertex");
        let before_u32: u32 = before.into();
        let vid = VertexId::from(before_u32);
        let k = store.property_id("k").expect("k property");
        assert_eq!(store.vertex_property(vid, k), None);
    }

    #[test]
    fn detach_delete_vertex_clears_incident_edge_sidecars() {
        let store = GraphStore::new();
        let before_a = store.vertex_count();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "w".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(2))),
                    }],
                },
                PlanOp::DetachDeleteVertex {
                    variable: "a".into(),
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("detach delete vertex");
        let b = bindings.vertices["b"];
        let w = store.property_id("w").expect("w property");
        let e = bindings.edges["e"];

        assert_eq!(
            store.edge_property(e.owner_vertex_id, e.vertex_edge_id, w),
            None
        );
        assert!(store.in_edges(b).expect("in edges").is_empty());
        assert!(store.out_edges(b).expect("out edges").is_empty());

        let before_a_u32: u32 = before_a.into();
        let deleted = VertexId::from(before_a_u32);
        assert!(
            matches!(
                store.out_edges(deleted),
                Err(DeferredBidirectionalLaraError::VertexDeleted { .. })
            ) || store.out_edges(deleted).unwrap().is_empty(),
            "deleted vertex should not expose outgoing edges"
        );
    }

    #[test]
    fn delete_edge_removes_directed_edge_and_properties() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "w".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(5))),
                    }],
                },
                PlanOp::DeleteEdge {
                    variable: "e".into(),
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("delete directed edge");
        let a = bindings.vertices["a"];
        let w = store.property_id("w").expect("w property");
        assert!(!bindings.edges.contains_key("e"));

        assert!(
            store
                .out_edges(a)
                .expect("out edges after delete")
                .is_empty()
        );
        assert_eq!(store.edge_property(a, VertexEdgeId::from_raw(1), w), None);
    }

    #[test]
    fn delete_edge_removes_undirected_edge_and_properties() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::Undirected,
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "w".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(9))),
                    }],
                },
                PlanOp::DeleteEdge {
                    variable: "e".into(),
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("delete undirected edge");
        let low = bindings.vertices["a"];
        let high = bindings.vertices["b"];
        let w = store.property_id("w").expect("w property");
        let owner = canonical_undirected_owner(low, high);
        let edge_id = VertexEdgeId::from_raw(1);

        assert!(store.out_edges(low).unwrap().is_empty());
        assert!(store.out_edges(high).unwrap().is_empty());
        assert_eq!(store.edge_property(owner, edge_id, w), None);
    }
}
