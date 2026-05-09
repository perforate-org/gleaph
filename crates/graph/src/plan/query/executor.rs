use super::error::PlanQueryError;
use crate::facade::{EdgeHandle, GraphStore};
use crate::plan::expr_evaluator::{
    eval_and_expr, eval_binary_expr, eval_compare_expr, eval_concat_expr, eval_not_expr,
    eval_or_expr, eval_unary_expr, eval_xor_expr, truthy,
};
use crate::stable::edge_ids::canonical_undirected_owner;
use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind, TruthValue};
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, ProjectColumn, Str};
use gleaph_graph_kernel::entry::{Edge, LabelId};
use ic_stable_lara::VertexId;
use ic_stable_lara::traits::CsrVertexTombstone;
use std::collections::BTreeMap;

pub trait PlanQueryExecutor {
    fn execute_plan_query(
        &self,
        plan: &PhysicalPlan,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<PlanQueryResult, PlanQueryError>;
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanQueryResult {
    pub rows: Vec<BTreeMap<String, Value>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlanBinding {
    Vertex(VertexId),
    Edge(EdgeHandle),
    Value(Value),
}

type PlanRow = BTreeMap<String, PlanBinding>;

impl PlanQueryExecutor for GraphStore {
    fn execute_plan_query(
        &self,
        plan: &PhysicalPlan,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<PlanQueryResult, PlanQueryError> {
        execute_plan_query(self, plan, parameters)
    }
}

pub fn execute_plan_query(
    store: &GraphStore,
    plan: &PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
) -> Result<PlanQueryResult, PlanQueryError> {
    let rows = execute_ops(store, &plan.ops, parameters)?;
    Ok(PlanQueryResult {
        rows: rows
            .iter()
            .map(|row| value_row(store, row))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn execute_ops(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, Value>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    execute_ops_from(store, ops, parameters, vec![PlanRow::new()])
}

fn execute_ops_from(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, Value>,
    initial_rows: Vec<PlanRow>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut rows = initial_rows;
    let evaluator = QueryExprEvaluator { store, parameters };

    for op in ops {
        rows = match op {
            PlanOp::NodeScan {
                variable,
                label,
                property_projection: _,
            } => execute_node_scan(store, rows, variable, label.as_ref())?,
            PlanOp::PropertyFilter { predicates, .. } => rows
                .into_iter()
                .filter_map(|row| match row_matches_all(&evaluator, &row, predicates) {
                    Ok(true) => Some(Ok(row)),
                    Ok(false) => None,
                    Err(err) => Some(Err(err)),
                })
                .collect::<Result<Vec<_>, _>>()?,
            PlanOp::Expand {
                src,
                edge,
                dst,
                direction,
                label,
                label_expr,
                var_len,
                indexed_edge_equality,
                edge_property_projection: _,
                dst_property_projection: _,
                hop_aux_binding,
            } => {
                ensure_simple_expand(label_expr, var_len, indexed_edge_equality, hop_aux_binding)?;
                execute_expand(
                    store,
                    rows,
                    parameters,
                    src,
                    edge,
                    dst,
                    *direction,
                    label.as_ref(),
                    &[],
                )?
            }
            PlanOp::ExpandFilter {
                src,
                edge,
                dst,
                direction,
                label,
                label_expr,
                var_len,
                indexed_edge_equality,
                dst_filter,
                edge_property_projection: _,
                dst_property_projection: _,
                hop_aux_binding,
            } => {
                ensure_simple_expand(label_expr, var_len, indexed_edge_equality, hop_aux_binding)?;
                execute_expand(
                    store,
                    rows,
                    parameters,
                    src,
                    edge,
                    dst,
                    *direction,
                    label.as_ref(),
                    dst_filter,
                )?
            }
            PlanOp::Project { columns, distinct } => {
                let mut projected = rows
                    .iter()
                    .map(|row| project_row(&evaluator, row, columns))
                    .collect::<Result<Vec<_>, _>>()?;
                if *distinct {
                    dedup_rows(&mut projected);
                }
                projected
            }
            PlanOp::Limit { count, offset } => {
                let offset = match offset {
                    Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
                    None => 0,
                };
                let count = match count {
                    Some(expr) => Some(limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?),
                    None => None,
                };
                rows.into_iter()
                    .skip(offset)
                    .take(count.unwrap_or(usize::MAX))
                    .collect()
            }
            PlanOp::Materialize { columns, distinct } => {
                let mut materialized = rows
                    .iter()
                    .map(|row| project_row(&evaluator, row, columns))
                    .collect::<Result<Vec<_>, _>>()?;
                if *distinct {
                    dedup_rows(&mut materialized);
                }
                materialized
            }
            PlanOp::UseGraph {
                graph_name: _,
                sub_plan: Some(sub_plan),
            } => execute_ops_from(store, sub_plan, parameters, rows)?,
            PlanOp::UseGraph {
                graph_name: _,
                sub_plan: None,
            } => rows,
            other if other.is_dml() => {
                return Err(PlanQueryError::UnsupportedOp(plan_op_name(other)));
            }
            other => return Err(PlanQueryError::UnsupportedOp(plan_op_name(other))),
        };
    }

    Ok(rows)
}

fn execute_node_scan(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    variable: &Str,
    label: Option<&Str>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = label.and_then(|label| store.label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for row in rows {
        for raw in 0..u64::from(store.vertex_count()) {
            let vertex_id = VertexId::from(u32::try_from(raw).expect("vertex count exceeds u32"));
            let Some(vertex) = store.vertex(vertex_id) else {
                continue;
            };
            if vertex.is_tombstone() {
                continue;
            }
            if let Some(label_id) = label_id
                && !store.vertex_labels(vertex_id, vertex).contains(&label_id)
            {
                continue;
            }
            let mut row = row.clone();
            row.insert(variable.to_string(), PlanBinding::Vertex(vertex_id));
            out.push(row);
        }
    }
    Ok(out)
}

fn execute_expand(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    src: &Str,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label: Option<&Str>,
    dst_filter: &[Expr],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = label.and_then(|label| store.label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(Vec::new());
    }

    let evaluator = QueryExprEvaluator { store, parameters };
    let mut out = Vec::new();
    for row in rows {
        let src_id = vertex_binding(&row, src)?;
        let candidates = expand_candidates(store, src_id, direction)?;
        for (dst_id, handle, edge_record) in candidates {
            if let Some(label_id) = label_id
                && edge_record.meta.label_id() != label_id.raw()
            {
                continue;
            }

            let mut expanded = row.clone();
            expanded.insert(edge.to_string(), PlanBinding::Edge(handle));
            expanded.insert(dst.to_string(), PlanBinding::Vertex(dst_id));
            if !row_matches_all(&evaluator, &expanded, dst_filter)? {
                continue;
            }
            out.push(expanded);
        }
    }
    Ok(out)
}

fn expand_candidates(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
) -> Result<Vec<(VertexId, EdgeHandle, Edge)>, PlanQueryError> {
    let mut out = Vec::new();
    match direction {
        EdgeDirection::PointingRight => {
            for edge in store
                .out_edges(src_id)
                .map_err(crate::facade::GraphStoreError::from)?
            {
                if edge.meta.is_undirected() {
                    continue;
                }
                out.push((
                    edge.target,
                    EdgeHandle {
                        owner_vertex_id: src_id,
                        vertex_edge_id: edge.vertex_edge_id,
                    },
                    edge,
                ));
            }
        }
        EdgeDirection::PointingLeft => {
            for edge in store
                .in_edges(src_id)
                .map_err(crate::facade::GraphStoreError::from)?
            {
                if edge.meta.is_undirected() {
                    continue;
                }
                out.push((
                    edge.target,
                    EdgeHandle {
                        owner_vertex_id: edge.target,
                        vertex_edge_id: edge.vertex_edge_id,
                    },
                    edge,
                ));
            }
        }
        EdgeDirection::Undirected => {
            for edge in store
                .out_edges(src_id)
                .map_err(crate::facade::GraphStoreError::from)?
            {
                if !edge.meta.is_undirected() {
                    continue;
                }
                out.push((
                    edge.target,
                    EdgeHandle {
                        owner_vertex_id: canonical_undirected_owner(src_id, edge.target),
                        vertex_edge_id: edge.vertex_edge_id,
                    },
                    edge,
                ));
            }
        }
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    Ok(out)
}

fn ensure_simple_expand(
    label_expr: &Option<LabelExpr>,
    var_len: &Option<gleaph_gql_planner::plan::VarLenSpec>,
    indexed_edge_equality: &Option<(Str, gleaph_gql_planner::plan::ScanValue)>,
    hop_aux_binding: &Option<Str>,
) -> Result<(), PlanQueryError> {
    if label_expr.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.label_expr"));
    }
    if var_len.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.var_len"));
    }
    if indexed_edge_equality.is_some() {
        return Err(PlanQueryError::UnsupportedOp(
            "Expand.indexed_edge_equality",
        ));
    }
    if hop_aux_binding.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.hop_aux_binding"));
    }
    Ok(())
}

fn row_matches_all(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    predicates: &[Expr],
) -> Result<bool, PlanQueryError> {
    for predicate in predicates {
        let value = evaluator.eval_expr(row, predicate)?;
        if truthy(value).map_err(PlanQueryError::from)? != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

struct QueryExprEvaluator<'a> {
    store: &'a GraphStore,
    parameters: &'a BTreeMap<String, Value>,
}

impl QueryExprEvaluator<'_> {
    fn eval_expr(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        match &expr.kind {
            ExprKind::Literal(value) => Ok(value.clone()),
            ExprKind::Paren(inner) => self.eval_expr(row, inner),
            ExprKind::Variable(name) => binding_to_value(
                self.store,
                row.get(name)
                    .ok_or_else(|| PlanQueryError::MissingBinding {
                        variable: name.clone(),
                    })?,
            ),
            ExprKind::Parameter(name) => self
                .parameters
                .get(name)
                .cloned()
                .ok_or_else(|| PlanQueryError::MissingParameter { name: name.clone() }),
            ExprKind::PropertyAccess { expr, property } => self.eval_property(row, expr, property),
            ExprKind::UnaryOp { op, expr } => {
                let value = self.eval_expr(row, expr)?;
                eval_unary_expr(*op, value).map_err(PlanQueryError::from)
            }
            ExprKind::BinaryOp { left, op, right } => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_binary_expr(left, *op, right).map_err(PlanQueryError::from)
            }
            ExprKind::Not(expr) => {
                let value = self.eval_expr(row, expr)?;
                eval_not_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::And(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_and_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Or(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_or_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Xor(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_xor_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Compare { left, op, right } => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_compare_expr(left, *op, right).map_err(PlanQueryError::from)
            }
            ExprKind::IsNull(expr) => Ok(Value::Bool(self.eval_expr(row, expr)? == Value::Null)),
            ExprKind::IsNotNull(expr) => Ok(Value::Bool(self.eval_expr(row, expr)? != Value::Null)),
            ExprKind::IsTruth {
                expr,
                value,
                negated,
            } => {
                let evaluated = self.eval_expr(row, expr)?;
                let matched = matches!(
                    (evaluated, *value),
                    (Value::Bool(true), TruthValue::True)
                        | (Value::Bool(false), TruthValue::False)
                        | (Value::Null, TruthValue::Unknown),
                );
                Ok(Value::Bool(if *negated { !matched } else { matched }))
            }
            ExprKind::Concat(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_concat_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Coalesce(exprs) => {
                for expr in exprs {
                    let value = self.eval_expr(row, expr)?;
                    if value != Value::Null {
                        return Ok(value);
                    }
                }
                Ok(Value::Null)
            }
            ExprKind::NullIf(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                if left == Value::Null || right == Value::Null {
                    return Ok(left);
                }
                let equal = eval_compare_expr(left.clone(), gleaph_gql::ast::CmpOp::Eq, right)
                    .map_err(PlanQueryError::from)?;
                if equal == Value::Bool(true) {
                    Ok(Value::Null)
                } else {
                    Ok(left)
                }
            }
            ExprKind::ListLiteral(items) | ExprKind::ListConstructor { items, .. } => items
                .iter()
                .map(|expr| self.eval_expr(row, expr))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::List),
            ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => fields
                .iter()
                .map(|(name, expr)| self.eval_expr(row, expr).map(|value| (name.clone(), value)))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Record),
            _ => Err(PlanQueryError::UnsupportedExpression {
                expression: format!("{:?}", expr.kind),
            }),
        }
    }

    fn eval_property(
        &self,
        row: &PlanRow,
        expr: &Expr,
        property: &str,
    ) -> Result<Value, PlanQueryError> {
        if let ExprKind::Variable(name) = &expr.kind {
            return match row.get(name) {
                Some(PlanBinding::Vertex(vertex_id)) => self
                    .store
                    .property_id(property)
                    .and_then(|property_id| self.store.vertex_property(*vertex_id, property_id))
                    .map_or(Ok(Value::Null), Ok),
                Some(PlanBinding::Edge(edge)) => self
                    .store
                    .property_id(property)
                    .and_then(|property_id| {
                        self.store.edge_property(
                            edge.owner_vertex_id,
                            edge.vertex_edge_id,
                            property_id,
                        )
                    })
                    .map_or(Ok(Value::Null), Ok),
                Some(PlanBinding::Value(value)) => Ok(record_property(value, property)),
                None => Err(PlanQueryError::MissingBinding {
                    variable: name.clone(),
                }),
            };
        }

        let value = self.eval_expr(row, expr)?;
        Ok(record_property(&value, property))
    }
}

fn project_row(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    columns: &[ProjectColumn],
) -> Result<PlanRow, PlanQueryError> {
    if columns.is_empty() {
        return row
            .iter()
            .map(|(name, binding)| {
                binding_to_value(evaluator.store, binding)
                    .map(|value| (name.clone(), PlanBinding::Value(value)))
            })
            .collect();
    }

    columns
        .iter()
        .map(|column| {
            let name = column
                .alias
                .as_ref()
                .map(Str::to_string)
                .unwrap_or_else(|| expression_name(&column.expr));
            evaluator
                .eval_expr(row, &column.expr)
                .map(|value| (name, PlanBinding::Value(value)))
        })
        .collect()
}

fn expression_name(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::PropertyAccess { expr, property } => {
            format!("{}.{}", expression_name(expr), property)
        }
        _ => "expr".to_owned(),
    }
}

fn value_row(store: &GraphStore, row: &PlanRow) -> Result<BTreeMap<String, Value>, PlanQueryError> {
    row.iter()
        .map(|(name, binding)| binding_to_value(store, binding).map(|value| (name.clone(), value)))
        .collect()
}

fn binding_to_value(store: &GraphStore, binding: &PlanBinding) -> Result<Value, PlanQueryError> {
    match binding {
        PlanBinding::Vertex(vertex_id) => vertex_to_value(store, *vertex_id),
        PlanBinding::Edge(edge) => edge_to_value(store, *edge),
        PlanBinding::Value(value) => Ok(value.clone()),
    }
}

fn vertex_to_value(store: &GraphStore, vertex_id: VertexId) -> Result<Value, PlanQueryError> {
    let vertex = store
        .vertex(vertex_id)
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: format!("vertex {vertex_id:?}"),
        })?;
    Ok(Value::Record(vec![
        ("id".to_owned(), Value::Uint64(u64::from(vertex_id))),
        (
            "labels".to_owned(),
            Value::List(
                store
                    .vertex_labels(vertex_id, vertex)
                    .into_iter()
                    .map(|label| {
                        store
                            .label_name(label)
                            .map(Value::Text)
                            .unwrap_or_else(|| Value::Uint64(u64::from(label.raw())))
                    })
                    .collect(),
            ),
        ),
        (
            "properties".to_owned(),
            properties_to_record(
                store
                    .vertex_properties(vertex_id)
                    .into_iter()
                    .map(|(property, value)| {
                        (store.property_name(property), property.raw(), value)
                    }),
            ),
        ),
    ]))
}

fn edge_to_value(store: &GraphStore, handle: EdgeHandle) -> Result<Value, PlanQueryError> {
    let edge = store
        .out_edges(handle.owner_vertex_id)
        .map_err(crate::facade::GraphStoreError::from)?
        .into_iter()
        .find(|edge| edge.vertex_edge_id == handle.vertex_edge_id)
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: format!("edge {:?}", handle),
        })?;
    let label = LabelId::from_raw(edge.meta.label_id());
    Ok(Value::Record(vec![
        (
            "owner_vertex_id".to_owned(),
            Value::Uint64(u64::from(handle.owner_vertex_id)),
        ),
        (
            "vertex_edge_id".to_owned(),
            Value::Uint64(u64::from(handle.vertex_edge_id.raw())),
        ),
        (
            "label".to_owned(),
            if label.raw() == 0 {
                Value::Null
            } else {
                store
                    .label_name(label)
                    .map(Value::Text)
                    .unwrap_or(Value::Null)
            },
        ),
        (
            "undirected".to_owned(),
            Value::Bool(edge.meta.is_undirected()),
        ),
        (
            "properties".to_owned(),
            properties_to_record(
                store
                    .edge_properties(handle.owner_vertex_id, handle.vertex_edge_id)
                    .into_iter()
                    .map(|(property, value)| {
                        (store.property_name(property), property.raw(), value)
                    }),
            ),
        ),
    ]))
}

fn properties_to_record(
    properties: impl IntoIterator<Item = (Option<String>, u32, Value)>,
) -> Value {
    Value::Record(
        properties
            .into_iter()
            .map(|(name, id, value)| (name.unwrap_or_else(|| id.to_string()), value))
            .collect(),
    )
}

fn record_property(value: &Value, property: &str) -> Value {
    match value {
        Value::Record(fields) => fields
            .iter()
            .find(|(name, _)| name == property)
            .map(|(_, value)| value.clone())
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn vertex_binding(row: &PlanRow, variable: &str) -> Result<VertexId, PlanQueryError> {
    match row.get(variable) {
        Some(PlanBinding::Vertex(vertex_id)) => Ok(*vertex_id),
        Some(_) | None => Err(PlanQueryError::MissingBinding {
            variable: variable.to_owned(),
        }),
    }
}

fn limit_value(value: &Value) -> Result<usize, PlanQueryError> {
    match value {
        Value::Int8(v) if *v >= 0 => Ok(*v as usize),
        Value::Int16(v) if *v >= 0 => Ok(*v as usize),
        Value::Int32(v) if *v >= 0 => Ok(*v as usize),
        Value::Int64(v) if *v >= 0 => {
            usize::try_from(*v).map_err(|_| PlanQueryError::InvalidLimit {
                value: value.clone(),
            })
        }
        Value::Uint8(v) => Ok(*v as usize),
        Value::Uint16(v) => Ok(*v as usize),
        Value::Uint32(v) => Ok(*v as usize),
        Value::Uint64(v) => usize::try_from(*v).map_err(|_| PlanQueryError::InvalidLimit {
            value: value.clone(),
        }),
        _ => Err(PlanQueryError::InvalidLimit {
            value: value.clone(),
        }),
    }
}

fn dedup_rows(rows: &mut Vec<PlanRow>) {
    let mut unique = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if !unique.contains(&row) {
            unique.push(row);
        }
    }
    *rows = unique;
}

fn plan_op_name(op: &PlanOp) -> &'static str {
    match op {
        PlanOp::NodeScan { .. } => "NodeScan",
        PlanOp::IndexScan { .. } => "IndexScan",
        PlanOp::EdgeIndexScan { .. } => "EdgeIndexScan",
        PlanOp::EdgeBindEndpoints { .. } => "EdgeBindEndpoints",
        PlanOp::ConditionalIndexScan { .. } => "ConditionalIndexScan",
        PlanOp::PropertyFilter { .. } => "PropertyFilter",
        PlanOp::Expand { .. } => "Expand",
        PlanOp::ExpandFilter { .. } => "ExpandFilter",
        PlanOp::ShortestPath { .. } => "ShortestPath",
        PlanOp::Let { .. } => "Let",
        PlanOp::For { .. } => "For",
        PlanOp::Filter { .. } => "Filter",
        PlanOp::CallProcedure { .. } => "CallProcedure",
        PlanOp::InlineProcedureCall { .. } => "InlineProcedureCall",
        PlanOp::UseGraph { .. } => "UseGraph",
        PlanOp::HashJoin { .. } => "HashJoin",
        PlanOp::CartesianProduct { .. } => "CartesianProduct",
        PlanOp::Aggregate { .. } => "Aggregate",
        PlanOp::Project { .. } => "Project",
        PlanOp::Sort { .. } => "Sort",
        PlanOp::Limit { .. } => "Limit",
        PlanOp::SetOperation { .. } => "SetOperation",
        PlanOp::OptionalMatch { .. } => "OptionalMatch",
        PlanOp::IndexIntersection { .. } => "IndexIntersection",
        PlanOp::WorstCaseOptimalJoin { .. } => "WorstCaseOptimalJoin",
        PlanOp::TopK { .. } => "TopK",
        PlanOp::Materialize { .. } => "Materialize",
        PlanOp::InsertVertex { .. } => "InsertVertex",
        PlanOp::InsertEdge { .. } => "InsertEdge",
        PlanOp::SetProperties { .. } => "SetProperties",
        PlanOp::RemoveProperties { .. } => "RemoveProperties",
        PlanOp::DeleteVertex { .. } => "DeleteVertex",
        PlanOp::DetachDeleteVertex { .. } => "DetachDeleteVertex",
        PlanOp::DeleteEdge { .. } => "DeleteEdge",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::mutation_executor::GraphMutationExecutor;
    use gleaph_gql::ast::{CmpOp, Expr, ExprKind, Statement};
    use gleaph_gql::parser;
    use gleaph_gql_planner::build_plan;
    use gleaph_gql_planner::plan::{PlanAnnotations, PlanDiagnostics, ScanValue};

    fn plan(ops: Vec<PlanOp>) -> PhysicalPlan {
        PhysicalPlan {
            ops,
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        }
    }

    fn plan_gql(input: &str) -> PhysicalPlan {
        let program = parser::parse(input).unwrap_or_else(|err| panic!("parse error: {err}"));
        let tx = program
            .transaction_activity
            .expect("expected transaction activity");
        let block = tx.body.expect("expected statement block");
        let Statement::Query(composite) = &block.first else {
            panic!("expected query statement");
        };
        build_plan(&composite.left, None).expect("plan should build")
    }

    fn prop(variable: &str, property: &str) -> Expr {
        Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::new(ExprKind::Variable(variable.to_owned()))),
            property: property.to_owned(),
        })
    }

    fn project(expr: Expr, alias: &str) -> ProjectColumn {
        ProjectColumn {
            expr,
            alias: Some(alias.into()),
        }
    }

    fn params() -> BTreeMap<String, Value> {
        BTreeMap::new()
    }

    #[test]
    fn executes_planner_match_return_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryPersonReturn"],
                [("name", Value::Text("Planner Alice".into()))],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["PlannerQueryOtherReturn"],
                [("name", Value::Text("Planner Bob".into()))],
            )
            .expect("insert non-matching vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryPersonReturn) RETURN n.name AS name");

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner Alice".into()))
        );
    }

    #[test]
    fn executes_planner_property_filter() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryPersonFilter"],
                [
                    ("name", Value::Text("Planner Filter Ada".into())),
                    ("age", Value::Int64(37)),
                ],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["PlannerQueryPersonFilter"],
                [
                    ("name", Value::Text("Planner Filter Bob".into())),
                    ("age", Value::Int64(12)),
                ],
            )
            .expect("insert non-matching vertex");
        let plan =
            plan_gql("MATCH (n:PlannerQueryPersonFilter) WHERE n.age > 18 RETURN n.name AS name");

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner Filter Ada".into()))
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
            .execute_plan_query(&plan, &params())
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
    fn node_scan_projects_vertex_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryPersonNodeScan"],
                [("name", Value::Text("Node Alice".into()))],
            )
            .expect("insert vertex");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryPersonNodeScan".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Node Alice".into()))
        );
    }

    #[test]
    fn property_filter_keeps_matching_vertices() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryPersonFilter"],
                [
                    ("name", Value::Text("Filter Ada".into())),
                    ("age", Value::Int64(37)),
                ],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["QueryPersonFilter"],
                [
                    ("name", Value::Text("Filter Bob".into())),
                    ("age", Value::Int64(12)),
                ],
            )
            .expect("insert non-matching vertex");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryPersonFilter".into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "age")),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
                })],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Filter Ada".into()))
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
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
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
            .execute_plan_query(&plan, &params())
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
                dst_filter: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("b", "age")),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
                })],
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("b", "age"), "age")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("age"), Some(&Value::Int64(44)));
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
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert!(matches!(result.rows[0].get("a"), Some(Value::Record(_))));
        assert!(matches!(result.rows[0].get("b"), Some(Value::Record(_))));
        assert!(matches!(result.rows[0].get("e"), Some(Value::Record(_))));
    }

    #[test]
    fn materialize_and_limit_shape_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryLimitPerson"],
                [("name", Value::Text("Limit A".into()))],
            )
            .expect("insert first");
        store
            .insert_vertex_named(
                ["QueryLimitPerson"],
                [("name", Value::Text("Limit B".into()))],
            )
            .expect("insert second");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryLimitPerson".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
            PlanOp::Materialize {
                columns: vec![],
                distinct: false,
            },
            PlanOp::Limit {
                count: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
                offset: None,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Limit A".into()))
        );
    }

    #[test]
    fn unsupported_operator_returns_stable_error() {
        let store = GraphStore::new();
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "uid".into(),
            value: ScanValue::Literal(Value::Text("alice".into())),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let err = store
            .execute_plan_query(&plan, &params())
            .expect_err("index scan unsupported in v1");

        assert!(matches!(err, PlanQueryError::UnsupportedOp("IndexScan")));
    }
}
