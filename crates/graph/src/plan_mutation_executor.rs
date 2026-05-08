use crate::mutation_executor::GraphMutationExecutor;
use crate::store::{EdgeHandle, GraphStore, GraphStoreError};
use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind};
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::plan::{
    PhysicalPlan, PlanOp, PropertyAssignment, RemovePlanItem, SetPlanItem, Str,
};
use ic_stable_lara::VertexId;
use std::collections::BTreeMap;
use std::fmt;

pub trait PlanMutationExecutor {
    fn execute_plan_mutations(
        &self,
        plan: &PhysicalPlan,
    ) -> Result<PlanMutationBindings, PlanMutationError>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanMutationBindings {
    pub vertices: BTreeMap<String, VertexId>,
    pub edges: BTreeMap<String, EdgeHandle>,
}

#[derive(Debug)]
pub enum PlanMutationError {
    Store(GraphStoreError),
    UnsupportedOp(&'static str),
    UnsupportedDirection(EdgeDirection),
    MissingVertexBinding { variable: String },
    MissingElementBinding { variable: String },
    UnsupportedExpression { property: String },
    UnsupportedSetItem(&'static str),
    UnsupportedRemoveItem(&'static str),
    MissingParameter { name: String },
}

impl fmt::Display for PlanMutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(err) => write!(f, "{err}"),
            Self::UnsupportedOp(op) => write!(f, "unsupported plan mutation operator: {op}"),
            Self::UnsupportedDirection(direction) => {
                write!(f, "unsupported insert edge direction: {direction:?}")
            }
            Self::MissingVertexBinding { variable } => {
                write!(f, "missing vertex binding for '{variable}'")
            }
            Self::MissingElementBinding { variable } => {
                write!(f, "missing graph element binding for '{variable}'")
            }
            Self::UnsupportedExpression { property } => {
                write!(f, "unsupported property expression for '{property}'")
            }
            Self::UnsupportedSetItem(item) => write!(f, "unsupported SET item: {item}"),
            Self::UnsupportedRemoveItem(item) => write!(f, "unsupported REMOVE item: {item}"),
            Self::MissingParameter { name } => write!(f, "missing parameter '{name}'"),
        }
    }
}

impl std::error::Error for PlanMutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            _ => None,
        }
    }
}

impl From<GraphStoreError> for PlanMutationError {
    fn from(value: GraphStoreError) -> Self {
        Self::Store(value)
    }
}

impl PlanMutationExecutor for GraphStore {
    fn execute_plan_mutations(
        &self,
        plan: &PhysicalPlan,
    ) -> Result<PlanMutationBindings, PlanMutationError> {
        execute_ops(self, &plan.ops, &BTreeMap::new())
    }
}

pub fn execute_ops(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, Value>,
) -> Result<PlanMutationBindings, PlanMutationError> {
    let mut bindings = PlanMutationBindings::default();
    execute_ops_with_bindings(store, ops, parameters, &mut bindings)?;
    Ok(bindings)
}

fn execute_ops_with_bindings(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, Value>,
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    for op in ops {
        match op {
            PlanOp::InsertVertex {
                variable,
                labels,
                properties,
            } => {
                let properties = resolve_property_assignments(properties, parameters)?;
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
                let properties = resolve_property_assignments(properties, parameters)?;
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
            } => execute_ops_with_bindings(store, sub_plan, parameters, bindings)?,
            PlanOp::SetProperties { items } => {
                for item in items {
                    execute_set_item(store, item, parameters, bindings)?;
                }
            }
            PlanOp::RemoveProperties { items } => {
                for item in items {
                    execute_remove_item(store, item, bindings)?;
                }
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
    parameters: &BTreeMap<String, Value>,
    bindings: &PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    match item {
        SetPlanItem::Property {
            variable,
            property,
            value,
        } => {
            let value = eval_property_expr(property, value, parameters)?;
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

fn resolve_property_assignments<'a>(
    properties: &'a [PropertyAssignment],
    parameters: &BTreeMap<String, Value>,
) -> Result<Vec<(&'a str, Value)>, PlanMutationError> {
    properties
        .iter()
        .map(|assignment| {
            let value = eval_property_expr(&assignment.name, &assignment.value, parameters)?;
            Ok((assignment.name.as_ref(), value))
        })
        .collect()
}

fn eval_property_expr(
    property: &str,
    expr: &Expr,
    parameters: &BTreeMap<String, Value>,
) -> Result<Value, PlanMutationError> {
    match &expr.kind {
        ExprKind::Literal(value) => Ok(value.clone()),
        ExprKind::Paren(inner) => eval_property_expr(property, inner, parameters),
        ExprKind::Parameter(name) => parameters
            .get(name)
            .cloned()
            .ok_or_else(|| PlanMutationError::MissingParameter { name: name.clone() }),
        _ => Err(PlanMutationError::UnsupportedExpression {
            property: property.to_owned(),
        }),
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
    use gleaph_gql::ast::Expr;
    use gleaph_gql_planner::plan::{PlanDiagnostics, PropertyAssignment};

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
            .execute_plan_mutations(&plan)
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

        let bindings = execute_ops(&store, &plan.ops, &parameters).expect("execute with params");
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
            .execute_plan_mutations(&plan)
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
            .execute_plan_mutations(&plan)
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
            .execute_plan_mutations(&plan)
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
            &mut existing_bindings,
        )
        .expect("execute remove label");
        let vertex = store.vertex(vertex_id).expect("read updated vertex");

        assert_eq!(store.vertex_labels(vertex_id, vertex), vec![employee]);
    }
}
