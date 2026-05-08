use crate::mutation_executor::GraphMutationExecutor;
use crate::store::{EdgeHandle, GraphStore, GraphStoreError};
use gleaph_gql::Value;
use gleaph_gql::ast::{BinaryOp, CmpOp, Expr, ExprKind, TruthValue, UnaryOp};
use gleaph_gql::numeric_ops::{eval_binary_numeric, eval_unary_numeric};
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_planner::plan::{
    PhysicalPlan, PlanOp, PropertyAssignment, RemovePlanItem, SetPlanItem, Str,
};
use ic_stable_lara::VertexId;
use std::cmp::Ordering;
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
    InvalidExpressionValue { property: String },
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
            Self::InvalidExpressionValue { property } => {
                write!(f, "invalid property expression value for '{property}'")
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
        ExprKind::UnaryOp { op, expr } => eval_unary_expr(
            property,
            *op,
            eval_property_expr(property, expr, parameters)?,
        ),
        ExprKind::BinaryOp { left, op, right } => {
            let left = eval_property_expr(property, left, parameters)?;
            let right = eval_property_expr(property, right, parameters)?;
            eval_binary_expr(property, left, *op, right)
        }
        ExprKind::Not(expr) => {
            eval_not_expr(property, eval_property_expr(property, expr, parameters)?)
        }
        ExprKind::And(left, right) => {
            let left = eval_property_expr(property, left, parameters)?;
            let right = eval_property_expr(property, right, parameters)?;
            eval_and_expr(property, left, right)
        }
        ExprKind::Or(left, right) => {
            let left = eval_property_expr(property, left, parameters)?;
            let right = eval_property_expr(property, right, parameters)?;
            eval_or_expr(property, left, right)
        }
        ExprKind::Xor(left, right) => {
            let left = eval_property_expr(property, left, parameters)?;
            let right = eval_property_expr(property, right, parameters)?;
            eval_xor_expr(property, left, right)
        }
        ExprKind::Compare { left, op, right } => {
            let left = eval_property_expr(property, left, parameters)?;
            let right = eval_property_expr(property, right, parameters)?;
            eval_compare_expr(property, left, *op, right)
        }
        ExprKind::IsNull(expr) => Ok(Value::Bool(
            eval_property_expr(property, expr, parameters)? == Value::Null,
        )),
        ExprKind::IsNotNull(expr) => Ok(Value::Bool(
            eval_property_expr(property, expr, parameters)? != Value::Null,
        )),
        ExprKind::IsTruth {
            expr,
            value,
            negated,
        } => {
            let evaluated = eval_property_expr(property, expr, parameters)?;
            let matched = matches!(
                (evaluated, *value),
                (Value::Bool(true), TruthValue::True)
                    | (Value::Bool(false), TruthValue::False)
                    | (Value::Null, TruthValue::Unknown),
            );
            Ok(Value::Bool(if *negated { !matched } else { matched }))
        }
        ExprKind::Concat(left, right) => {
            let left = eval_property_expr(property, left, parameters)?;
            let right = eval_property_expr(property, right, parameters)?;
            eval_concat_expr(property, left, right)
        }
        ExprKind::Coalesce(exprs) => {
            for expr in exprs {
                let value = eval_property_expr(property, expr, parameters)?;
                if value != Value::Null {
                    return Ok(value);
                }
            }
            Ok(Value::Null)
        }
        ExprKind::NullIf(left, right) => {
            let left = eval_property_expr(property, left, parameters)?;
            let right = eval_property_expr(property, right, parameters)?;
            if left == Value::Null || right == Value::Null {
                return Ok(left);
            }
            if compare_property_values(&left, &right) == Some(Ordering::Equal) {
                Ok(Value::Null)
            } else {
                Ok(left)
            }
        }
        ExprKind::ListLiteral(items) | ExprKind::ListConstructor { items, .. } => items
            .iter()
            .map(|expr| eval_property_expr(property, expr, parameters))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => fields
            .iter()
            .map(|(name, expr)| {
                eval_property_expr(property, expr, parameters).map(|value| (name.clone(), value))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Record),
        _ => Err(PlanMutationError::UnsupportedExpression {
            property: property.to_owned(),
        }),
    }
}

fn eval_not_expr(property: &str, value: Value) -> Result<Value, PlanMutationError> {
    match value {
        Value::Bool(value) => Ok(Value::Bool(!value)),
        Value::Null => Ok(Value::Null),
        _ => invalid_expr_value(property),
    }
}

fn eval_and_expr(property: &str, left: Value, right: Value) -> Result<Value, PlanMutationError> {
    match (truthy(property, left)?, truthy(property, right)?) {
        (Some(false), _) | (_, Some(false)) => Ok(Value::Bool(false)),
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(true), Some(true)) => Ok(Value::Bool(true)),
    }
}

fn eval_or_expr(property: &str, left: Value, right: Value) -> Result<Value, PlanMutationError> {
    match (truthy(property, left)?, truthy(property, right)?) {
        (Some(true), _) | (_, Some(true)) => Ok(Value::Bool(true)),
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(false), Some(false)) => Ok(Value::Bool(false)),
    }
}

fn eval_xor_expr(property: &str, left: Value, right: Value) -> Result<Value, PlanMutationError> {
    match (truthy(property, left)?, truthy(property, right)?) {
        (Some(left), Some(right)) => Ok(Value::Bool(left ^ right)),
        _ => Ok(Value::Null),
    }
}

fn truthy(property: &str, value: Value) -> Result<Option<bool>, PlanMutationError> {
    match value {
        Value::Bool(value) => Ok(Some(value)),
        Value::Null => Ok(None),
        _ => invalid_expr_value(property).map(|_| None),
    }
}

fn eval_compare_expr(
    property: &str,
    left: Value,
    op: CmpOp,
    right: Value,
) -> Result<Value, PlanMutationError> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }

    let Some(ordering) = compare_property_values(&left, &right) else {
        return invalid_expr_value(property);
    };
    let matched = match op {
        CmpOp::Eq => ordering == Ordering::Equal,
        CmpOp::Ne => ordering != Ordering::Equal,
        CmpOp::Lt => ordering == Ordering::Less,
        CmpOp::Le => ordering != Ordering::Greater,
        CmpOp::Gt => ordering == Ordering::Greater,
        CmpOp::Ge => ordering != Ordering::Less,
    };
    Ok(Value::Bool(matched))
}

fn compare_property_values(left: &Value, right: &Value) -> Option<Ordering> {
    compare_values(left, right)
}

fn eval_concat_expr(property: &str, left: Value, right: Value) -> Result<Value, PlanMutationError> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Text(left), Value::Text(right)) => Ok(Value::Text(format!("{left}{right}"))),
        (Value::Bytes(mut left), Value::Bytes(right)) => {
            left.extend_from_slice(&right);
            Ok(Value::Bytes(left))
        }
        _ => invalid_expr_value(property),
    }
}

fn eval_unary_expr(property: &str, op: UnaryOp, value: Value) -> Result<Value, PlanMutationError> {
    eval_unary_numeric(op, value).map_err(|_| invalid_expr_value_err(property))
}

fn eval_binary_expr(
    property: &str,
    left: Value,
    op: BinaryOp,
    right: Value,
) -> Result<Value, PlanMutationError> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }

    if let BinaryOp::Add = op
        && let (Value::Text(left), Value::Text(right)) = (&left, &right)
    {
        return Ok(Value::Text(format!("{left}{right}")));
    }

    eval_binary_numeric(left, op, right).map_err(|_| invalid_expr_value_err(property))
}

fn invalid_expr_value(property: &str) -> Result<Value, PlanMutationError> {
    Err(invalid_expr_value_err(property))
}

fn invalid_expr_value_err(property: &str) -> PlanMutationError {
    PlanMutationError::InvalidExpressionValue {
        property: property.to_owned(),
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
    use gleaph_gql::types::{Decimal, Int256, Uint256};
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

        let bindings = execute_ops(&store, &plan.ops, &parameters).expect("execute arithmetic");
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
            .execute_plan_mutations(&plan)
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
    fn preserves_numeric_precision_when_evaluating_arithmetic() {
        assert_eq!(
            eval_binary_expr("p", Value::Int8(120), BinaryOp::Add, Value::Int8(10))
                .expect("signed widening"),
            Value::Int16(130)
        );
        assert_eq!(
            eval_binary_expr("p", Value::Uint8(250), BinaryOp::Add, Value::Uint8(10))
                .expect("unsigned widening"),
            Value::Uint16(260)
        );
        assert_eq!(
            eval_binary_expr("p", Value::Int64(1), BinaryOp::Div, Value::Int64(4))
                .expect("integer division decimal"),
            Value::Decimal(Decimal::parse("0.25").expect("decimal"))
        );
        assert_eq!(
            eval_binary_expr("p", Value::Uint8(2), BinaryOp::Sub, Value::Uint8(5))
                .expect("unsigned subtraction below zero"),
            Value::Int128(-3)
        );

        let large = Value::Int256(Int256::new(ethnum::I256::from(i128::MAX)));
        assert_eq!(
            eval_binary_expr("p", large, BinaryOp::Add, Value::Int64(1)).expect("i256 add"),
            Value::Int256(Int256::new(ethnum::I256::from(i128::MAX) + 1))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Int128(i128::MAX),
                BinaryOp::Add,
                Value::Int64(1),
            )
            .expect("i128 overflow widens"),
            Value::Int256(Int256::new(ethnum::I256::from(i128::MAX) + 1))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Uint128(u128::MAX),
                BinaryOp::Add,
                Value::Uint8(1),
            )
            .expect("u128 overflow widens"),
            Value::Uint256(Uint256::new(ethnum::U256::from(u128::MAX) + 1))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Uint128(u128::MAX),
                BinaryOp::Sub,
                Value::Uint256(Uint256::new(ethnum::U256::from(u128::MAX) + 1)),
            )
            .expect("large unsigned subtraction below zero"),
            Value::Int256(Int256::new(ethnum::I256::from(-1)))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Int256(Int256::new(ethnum::I256::from(1))),
                BinaryOp::Div,
                Value::Int256(Int256::new(ethnum::I256::from(4))),
            )
            .expect("i256 fractional division"),
            Value::Float256("0.25".parse::<f256::f256>().expect("f256"))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Uint256(Uint256::new(ethnum::U256::from(1u8))),
                BinaryOp::Div,
                Value::Uint256(Uint256::new(ethnum::U256::from(4u8))),
            )
            .expect("u256 fractional division"),
            Value::Float256("0.25".parse::<f256::f256>().expect("f256"))
        );
    }

    #[test]
    fn preserves_float_width_when_evaluating_arithmetic() {
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Float16(half::f16::from_f32(1.5)),
                BinaryOp::Add,
                Value::Float16(half::f16::from_f32(2.0)),
            )
            .expect("f16 add"),
            Value::Float16(half::f16::from_f32(3.5))
        );
        assert_eq!(
            eval_binary_expr("p", Value::Float32(1.5), BinaryOp::Add, Value::Int64(2))
                .expect("f32 plus int"),
            Value::Float32(3.5)
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Float128(1.5f128),
                BinaryOp::Add,
                Value::Float128(2.25f128),
            )
            .expect("f128 add"),
            Value::Float128(3.75f128)
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Float256("1.5".parse::<f256::f256>().expect("f256")),
                BinaryOp::Add,
                Value::Int64(2),
            )
            .expect("f256 plus int"),
            Value::Float256("3.5".parse::<f256::f256>().expect("f256"))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Decimal(Decimal::parse("1.25").expect("decimal")),
                BinaryOp::Add,
                Value::Float32(2.25),
            )
            .expect("decimal plus f32"),
            Value::Float64(3.5)
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Decimal(Decimal::parse("1.25").expect("decimal")),
                BinaryOp::Add,
                Value::Float256("2.25".parse::<f256::f256>().expect("f256")),
            )
            .expect("decimal plus f256"),
            Value::Float256("3.5".parse::<f256::f256>().expect("f256"))
        );
    }

    #[test]
    fn preserves_numeric_precision_when_comparing_values() {
        assert_eq!(
            compare_property_values(
                &Value::Float128(1.0f128 + f128::EPSILON),
                &Value::Float128(1.0f128),
            ),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_property_values(
                &Value::Float256("1.0000000000000000000000000000000000001".parse().unwrap()),
                &Value::Float256("1.0000000000000000000000000000000000000".parse().unwrap()),
            ),
            Some(Ordering::Greater)
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
            .execute_plan_mutations(&plan)
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
}
