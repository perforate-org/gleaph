//! Expression evaluation, projection, and binding materialization.

use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::ast::{CmpOp, Expr, ExprKind, ObjectName, TruthValue};
use gleaph_gql::types::LabelExpr;
use gleaph_gql_planner::plan::{ProjectColumn, Str};
use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeSlotIndex, PreparedWeightDecoder, Vertex};
use gleaph_graph_kernel::path::GraphPathVertexId;
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;

use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::PlanBinding;
use super::bindings::EdgeBinding;
use super::context::QueryExprEvaluator;
use super::path::{
    edge_element_id_bytes, local_shard_id, path_binding_to_value, vertex_element_id_bytes,
};
use crate::facade::GraphStore;
use crate::gql_execution_context::try_eval_runtime_function_call;
use crate::plan::expr_evaluator::{
    SearchedCaseWhenOutcome, eval_abs_expr, eval_acos_expr, eval_and_expr, eval_asin_expr,
    eval_atan_expr, eval_binary_expr, eval_cast_expr, eval_ceil_expr, eval_compare_expr,
    eval_concat_expr, eval_cos_expr, eval_cosh_expr, eval_cot_expr, eval_degrees_expr,
    eval_exp_expr, eval_floor_expr, eval_ln_expr, eval_log_expr, eval_log10_expr, eval_mod_expr,
    eval_not_expr, eval_or_expr, eval_power_expr, eval_radians_expr, eval_sin_expr, eval_sinh_expr,
    eval_sqrt_expr, eval_tan_expr, eval_tanh_expr, eval_unary_expr, eval_xor_expr,
    searched_case_when_outcome,
};

pub(crate) fn eval_sort_expr(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    expr: &Expr,
) -> Result<Value, PlanQueryError> {
    match evaluator.eval_expr(row, expr) {
        Ok(value) => Ok(value),
        Err(PlanQueryError::MissingBinding { .. }) => {
            let projected_name = expression_name(expr);
            match row.get(&projected_name) {
                Some(PlanBinding::Value(value)) => Ok(value.clone()),
                Some(binding) => binding_to_value(evaluator.store, binding),
                None => Err(PlanQueryError::MissingBinding {
                    variable: projected_name,
                }),
            }
        }
        Err(err) => Err(err),
    }
}

fn try_eval_gleaph_weight(
    store: &GraphStore,
    decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    name: &ObjectName,
    args: &[Expr],
    distinct: bool,
    row: &PlanRow,
) -> Result<Option<Value>, PlanQueryError> {
    if !super::super::gleaph_weight::is_gleaph_weight_call(name, distinct) {
        return Ok(None);
    }
    // Inline edge weights decode to FLOAT32; cost expressions may widen via casts or arithmetic.
    if distinct {
        return Err(PlanQueryError::GleaphWeight {
            message: "GLEAPH.WEIGHT does not support DISTINCT".into(),
        });
    }
    let map = decoders.ok_or_else(|| PlanQueryError::GleaphWeight {
        message: "GLEAPH.WEIGHT requires query preparation (no decoder table)".into(),
    })?;
    if args.len() != 1 {
        return Err(PlanQueryError::GleaphWeight {
            message: format!("GLEAPH.WEIGHT expects 1 argument, got {}", args.len()),
        });
    }
    let Some(edge_var) = super::super::gleaph_weight::gleaph_weight_arg_edge_var(&args[0]) else {
        return Err(PlanQueryError::GleaphWeight {
            message: "GLEAPH.WEIGHT argument must be an edge variable".into(),
        });
    };
    let decoder = map
        .get(&edge_var)
        .ok_or_else(|| PlanQueryError::GleaphWeight {
            message: format!(
                "GLEAPH.WEIGHT({edge_var}): no prepared decoder for this edge variable"
            ),
        })?;
    let binding = row
        .get(edge_var.as_str())
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: edge_var.clone(),
        })?;
    match binding {
        PlanBinding::Value(Value::Null) => Ok(Some(Value::Null)),
        PlanBinding::Edge(edge) => {
            let w = super::super::gleaph_weight::decode_traversal_edge_weight(
                store,
                edge.handle,
                edge.value_len(),
                edge.value_bytes_slice(),
                edge.inline_value(),
                Some(decoder),
            )?;
            Ok(Some(Value::Float32(w)))
        }
        _ => Err(PlanQueryError::GleaphWeight {
            message: format!("GLEAPH.WEIGHT({edge_var}): binding is not an edge"),
        }),
    }
}

impl QueryExprEvaluator<'_> {
    pub(crate) fn eval_expr(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
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
            ExprKind::ElementId(expr) => self.eval_element_id(row, expr),
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
            ExprKind::IsLabeled {
                expr,
                label,
                negated,
            } => {
                let matched = self.eval_is_labeled(row, expr, label)?;
                Ok(Value::Bool(if *negated { !matched } else { matched }))
            }
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
            ExprKind::Abs(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_abs_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Floor(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_floor_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Ceil(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_ceil_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Sqrt(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_sqrt_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Exp(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_exp_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Ln(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_ln_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Log10(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_log10_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Sin(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_sin_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Cos(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_cos_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Tan(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_tan_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Asin(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_asin_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Acos(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_acos_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Atan(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_atan_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Degrees(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_degrees_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Radians(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_radians_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Cot(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_cot_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Sinh(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_sinh_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Cosh(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_cosh_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Tanh(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_tanh_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Cast { expr, target } => {
                let value = self.eval_expr(row, expr)?;
                eval_cast_expr(value, target).map_err(PlanQueryError::from)
            }
            ExprKind::Mod(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_mod_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Log(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_log_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Power(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_power_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::CaseSimple {
                operand,
                when_clauses,
                else_clause,
            } => {
                let operand = self.eval_expr(row, operand)?;
                for clause in when_clauses {
                    let condition = self.eval_expr(row, &clause.condition)?;
                    if operand == Value::Null || condition == Value::Null {
                        continue;
                    }
                    if eval_compare_expr(operand.clone(), CmpOp::Eq, condition).ok()
                        == Some(Value::Bool(true))
                    {
                        return self.eval_expr(row, &clause.result);
                    }
                }
                match else_clause {
                    Some(expr) => self.eval_expr(row, expr),
                    None => Ok(Value::Null),
                }
            }
            ExprKind::CaseSearched {
                when_clauses,
                else_clause,
            } => {
                for clause in when_clauses {
                    let condition = self.eval_expr(row, &clause.condition)?;
                    if searched_case_when_outcome(condition).map_err(PlanQueryError::from)?
                        == SearchedCaseWhenOutcome::Match
                    {
                        return self.eval_expr(row, &clause.result);
                    }
                }
                match else_clause {
                    Some(expr) => self.eval_expr(row, expr),
                    None => Ok(Value::Null),
                }
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
            ExprKind::Aggregate { .. } => {
                let Some(specs) = self.aggregate_specs else {
                    return Err(PlanQueryError::UnsupportedExpression {
                        expression: "aggregate".to_owned(),
                    });
                };
                super::super::aggregate::resolve_aggregate_from_row(row, expr, specs)
            }
            ExprKind::FunctionCall {
                name,
                args,
                distinct,
            } => {
                if let Some(v) = try_eval_gleaph_weight(
                    self.store,
                    self.gleaph_weight_decoders,
                    name,
                    args,
                    *distinct,
                    row,
                )? {
                    return Ok(v);
                }
                match try_eval_runtime_function_call(self.caller, name, args, *distinct) {
                    Ok(Some(value)) => Ok(value),
                    Ok(None) => Err(PlanQueryError::UnsupportedExpression {
                        expression: format!("{:?}", expr.kind),
                    }),
                    Err(e) => Err(e.into()),
                }
            }
            _ => Err(PlanQueryError::UnsupportedExpression {
                expression: format!("{:?}", expr.kind),
            }),
        }
    }

    fn eval_is_labeled(
        &self,
        row: &PlanRow,
        expr: &Expr,
        label: &LabelExpr,
    ) -> Result<bool, PlanQueryError> {
        let ExprKind::Variable(name) = &expr.kind else {
            return Err(PlanQueryError::UnsupportedExpression {
                expression: format!(
                    "IS LABELED requires a variable expression, got {:?}",
                    expr.kind
                ),
            });
        };
        match row.get(name.as_str()) {
            Some(PlanBinding::Vertex(vertex_id)) => {
                let Some(vertex) = self.store.vertex(*vertex_id) else {
                    return Ok(false);
                };
                Ok(vertex_matches_label_expr(
                    self.store, *vertex_id, vertex, label,
                ))
            }
            Some(PlanBinding::Value(Value::Null)) => Ok(false),
            Some(
                PlanBinding::Value(_)
                | PlanBinding::Edge(_)
                | PlanBinding::Path(_)
                | PlanBinding::RemoteVertex(_),
            ) => Ok(false),
            None => Err(PlanQueryError::MissingBinding {
                variable: name.clone(),
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
                    .and_then(|property_id| self.store.edge_property(edge.handle, property_id))
                    .map_or(Ok(Value::Null), Ok),
                Some(PlanBinding::Value(value)) => Ok(record_property(value, property)),
                Some(PlanBinding::Path(pb)) => Ok(record_property(
                    &path_binding_to_value(self.store, pb),
                    property,
                )),
                Some(PlanBinding::RemoteVertex(_)) => Ok(Value::Null),
                None => Err(PlanQueryError::MissingBinding {
                    variable: name.clone(),
                }),
            };
        }

        let value = self.eval_expr(row, expr)?;
        Ok(record_property(&value, property))
    }

    fn eval_element_id(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        if let ExprKind::Variable(name) = &expr.kind {
            return match row.get(name) {
                Some(PlanBinding::Vertex(vertex_id)) => Ok(Value::Bytes(vertex_element_id_bytes(
                    self.store, *vertex_id,
                )?)),
                Some(PlanBinding::RemoteVertex(logical_vertex_id)) => Ok(Value::Bytes(
                    GraphPathVertexId::new(*logical_vertex_id)
                        .to_bytes()
                        .to_vec(),
                )),
                Some(PlanBinding::Edge(edge)) => Ok(Value::Bytes(edge_element_id_bytes(
                    local_shard_id(self.store),
                    edge.handle.owner_vertex_id,
                    EdgeSlotIndex::from_raw(edge.handle.slot_index),
                ))),
                Some(PlanBinding::Value(Value::Null)) => Ok(Value::Null),
                Some(binding) => Err(PlanQueryError::InvalidExpressionValue {
                    expression: format!("ELEMENT_ID({name}) for {binding:?}"),
                }),
                None => Err(PlanQueryError::MissingBinding {
                    variable: name.clone(),
                }),
            };
        }

        let value = self.eval_expr(row, expr)?;
        if value == Value::Null {
            Ok(Value::Null)
        } else {
            Err(PlanQueryError::InvalidExpressionValue {
                expression: format!("ELEMENT_ID({:?})", expr.kind),
            })
        }
    }
}

impl super::super::aggregate::PlanRowExprEval for QueryExprEvaluator<'_> {
    fn eval_expr_for_row(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        QueryExprEvaluator::eval_expr(self, row, expr)
    }

    fn eval_sort_key_for_row(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        eval_sort_expr(self, row, expr)
    }
}

pub(crate) fn project_row(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    columns: &[ProjectColumn],
) -> Result<PlanRow, PlanQueryError> {
    if columns.is_empty() {
        let mut out = PlanRow::new();
        for (name, binding) in row.iter() {
            let value = binding_to_value(evaluator.store, binding)?;
            out.insert(name.to_string(), PlanBinding::Value(value));
        }
        return Ok(out);
    }

    // Fast path: `RETURN v` / `RETURN v AS alias` — keep graph bindings so later
    // `value_row` does a single `binding_to_value` (avoids materializing a large
    // `Value::Record` in Project then cloning it again in `execute_plan_query`).
    if columns.len() == 1 {
        let column = &columns[0];
        if let ExprKind::Variable(var_name) = &column.expr.kind {
            let binding =
                row.get(var_name.as_str())
                    .ok_or_else(|| PlanQueryError::MissingBinding {
                        variable: var_name.clone(),
                    })?;
            let name = column
                .alias
                .as_ref()
                .map(Str::to_string)
                .unwrap_or_else(|| var_name.clone());
            if column.alias.is_none() && row.is_singleton_binding(var_name.as_str())
                && let Some(layout) = row.shared_layout() {
                    return Ok(PlanRow::with_layout_and_binding(
                        layout,
                        var_name.as_str(),
                        binding.clone(),
                    ));
                }
            let mut out = PlanRow::new();
            out.insert(name, binding.clone());
            return Ok(out);
        }
    }

    let mut out = PlanRow::new();
    for column in columns {
        let name = column
            .alias
            .as_ref()
            .map(Str::to_string)
            .unwrap_or_else(|| expression_name(&column.expr));
        let value = evaluator.eval_expr(row, &column.expr)?;
        out.insert(name, PlanBinding::Value(value));
    }
    Ok(out)
}

pub(crate) fn expression_name(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::PropertyAccess { expr, property } => {
            format!("{}.{}", expression_name(expr), property)
        }
        _ => "expr".to_owned(),
    }
}

pub(crate) fn value_row(
    store: &GraphStore,
    row: &PlanRow,
) -> Result<BTreeMap<String, Value>, PlanQueryError> {
    if row.len() == 1 {
        let (name, binding) = row.iter().next().expect("len==1 guarantees one entry");
        let value = binding_to_value(store, binding)?;
        let mut out = BTreeMap::new();
        out.insert(name.to_string(), value);
        return Ok(out);
    }
    row.iter()
        .map(|(name, binding)| {
            binding_to_value(store, binding).map(|value| (name.to_string(), value))
        })
        .collect()
}

pub(crate) fn binding_to_value(
    store: &GraphStore,
    binding: &PlanBinding,
) -> Result<Value, PlanQueryError> {
    match binding {
        PlanBinding::Vertex(vertex_id) => vertex_to_value(store, *vertex_id),
        PlanBinding::RemoteVertex(logical_vertex_id) => Ok(Value::Record(vec![
            ("id".to_owned(), Value::Uint64(*logical_vertex_id)),
            ("remote".to_owned(), Value::Bool(true)),
        ])),
        PlanBinding::Edge(edge) => edge_to_value(store, *edge),
        PlanBinding::Value(value) => Ok(value.clone()),
        PlanBinding::Path(pb) => Ok(path_binding_to_value(store, pb)),
    }
}

fn vertex_to_value(store: &GraphStore, vertex_id: VertexId) -> Result<Value, PlanQueryError> {
    let vertex = store
        .vertex(vertex_id)
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: format!("vertex {vertex_id:?}"),
        })?;

    let labels = store.vertex_label_gql_list(vertex_id, vertex);

    let properties_value = store.vertex_properties_gql_record(vertex_id);

    Ok(Value::Record(vec![
        ("id".to_owned(), Value::Uint64(u64::from(vertex_id))),
        ("labels".to_owned(), Value::List(labels)),
        ("properties".to_owned(), properties_value),
    ]))
}

fn edge_to_value(store: &GraphStore, binding: EdgeBinding) -> Result<Value, PlanQueryError> {
    let handle = binding.handle;
    let (_edge, bucket_label) = store
        .find_outgoing_edge_with_bucket_label(handle)?
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: format!("edge {:?}", handle),
        })?;
    let storage = LaraLabelId::from_raw(bucket_label.raw());
    let catalog_id = EdgeLabelId::from_raw(storage.label_index());
    Ok(Value::Record(vec![
        (
            "owner_vertex_id".to_owned(),
            Value::Uint64(u64::from(handle.owner_vertex_id)),
        ),
        (
            "edge_slot_index".to_owned(),
            Value::Uint64(u64::from(handle.slot_index)),
        ),
        (
            "inline_value".to_owned(),
            Value::Uint64(u64::from(binding.inline_value())),
        ),
        (
            "label".to_owned(),
            if catalog_id.raw() == 0 {
                Value::Null
            } else {
                store
                    .edge_label_name(catalog_id)
                    .map(Value::Text)
                    .unwrap_or(Value::Null)
            },
        ),
        (
            "undirected".to_owned(),
            Value::Bool(storage.is_undirected()),
        ),
        ("properties".to_owned(), {
            store.edge_properties_gql_record(handle)
        }),
    ]))
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

fn vertex_matches_label_expr(
    store: &GraphStore,
    vertex_id: VertexId,
    vertex: Vertex,
    expr: &LabelExpr,
) -> bool {
    match expr {
        LabelExpr::Name(name) => store
            .vertex_label_id(name)
            .is_some_and(|label_id| store.vertex_has_label(vertex_id, vertex, label_id)),
        LabelExpr::Wildcard => store.vertex_has_any_label(vertex_id, vertex),
        LabelExpr::And(left, right) => {
            vertex_matches_label_expr(store, vertex_id, vertex, left)
                && vertex_matches_label_expr(store, vertex_id, vertex, right)
        }
        LabelExpr::Or(left, right) => {
            vertex_matches_label_expr(store, vertex_id, vertex, left)
                || vertex_matches_label_expr(store, vertex_id, vertex, right)
        }
        LabelExpr::Not(inner) => !vertex_matches_label_expr(store, vertex_id, vertex, inner),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use pollster;
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
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
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
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner Filter Ada".into()))
        );
    }
    #[test]
    fn executes_planner_let_binding() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerQueryLetAge"], [("age", Value::Int64(36))])
            .expect("insert vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryLetAge) LET x = n.age + 1 RETURN x");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("x"), Some(&Value::Int64(37)));
    }
    #[test]
    fn executes_planner_let_binding_dependency_order() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerQueryLetChain"], [("k", Value::Int64(10))])
            .expect("insert vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryLetChain) LET x = n.k + 1, y = x * 2 RETURN y");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("y"), Some(&Value::Int64(22)));
    }
    #[test]
    fn executes_planner_standalone_filter() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryStandaloneFilter"],
                [
                    ("name", Value::Text("Active Ada".into())),
                    ("active", Value::Bool(true)),
                ],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["PlannerQueryStandaloneFilter"],
                [
                    ("name", Value::Text("Inactive Bob".into())),
                    ("active", Value::Bool(false)),
                ],
            )
            .expect("insert non-matching vertex");
        let plan = plan_gql(
            "MATCH (n:PlannerQueryStandaloneFilter) FILTER n.active RETURN n.name AS name",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Active Ada".into()))
        );
    }
    #[test]
    fn executes_planner_order_by() {
        let store = GraphStore::new();
        for name in ["Planner Sort C", "Planner Sort A", "Planner Sort B"] {
            store
                .insert_vertex_named(["PlannerQuerySort"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan_gql("MATCH (n:PlannerQuerySort) RETURN n.name ORDER BY n.name");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(
            text_column(&result, "n.name"),
            vec!["Planner Sort A", "Planner Sort B", "Planner Sort C"]
        );
    }
    #[test]
    fn executes_planner_order_by_limit_topk() {
        let store = GraphStore::new();
        for name in [
            "Planner TopK D",
            "Planner TopK A",
            "Planner TopK C",
            "Planner TopK B",
        ] {
            store
                .insert_vertex_named(["PlannerQueryTopK"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan_gql("MATCH (n:PlannerQueryTopK) RETURN n.name ORDER BY n.name LIMIT 2");
        assert!(plan.ops.iter().any(|op| matches!(op, PlanOp::TopK { .. })));

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(
            text_column(&result, "n.name"),
            vec!["Planner TopK A", "Planner TopK B"]
        );
    }
    #[test]
    fn executes_planner_order_by_record_value() {
        let store = GraphStore::new();
        for (name, rank) in [("Planner Record B", 2), ("Planner Record A", 1)] {
            store
                .insert_vertex_named(
                    ["PlannerQueryRecordSort"],
                    [
                        ("name", Value::Text(name.into())),
                        ("rank", Value::Int64(rank)),
                    ],
                )
                .expect("insert vertex");
        }
        let plan = plan_gql(
            "MATCH (n:PlannerQueryRecordSort) RETURN n.name AS name, {rank: n.rank} AS sort_key ORDER BY sort_key",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(
            text_column(&result, "name"),
            vec!["Planner Record A", "Planner Record B"]
        );
    }
    #[test]
    fn executes_planner_record_equality_independent_of_field_order() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryRecordEq"],
                [("a", Value::Int64(1)), ("b", Value::Int64(2))],
            )
            .expect("insert vertex");
        let plan = plan_gql(
            "MATCH (n:PlannerQueryRecordEq) \
             RETURN {b: n.b, a: n.a} = {a: n.a, b: n.b} AS same",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("same"), Some(&Value::Bool(true)));
    }
    #[test]
    fn executes_planner_return_star() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryReturnStar"],
                [("name", Value::Text("Planner Star".into()))],
            )
            .expect("insert vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryReturnStar) RETURN *");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert!(matches!(result.rows[0].get("n"), Some(Value::Record(_))));
    }
    #[test]
    fn executes_planner_limit() {
        let store = GraphStore::new();
        for name in ["Planner Limit A", "Planner Limit B"] {
            store
                .insert_vertex_named(["PlannerQueryLimit"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan_gql("MATCH (n:PlannerQueryLimit) RETURN n.name LIMIT 1");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
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
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Filter Ada".into()))
        );
    }
    #[test]
    fn sort_orders_projected_scalars_ascending_and_descending() {
        let store = GraphStore::new();
        for name in ["Sort Scalar C", "Sort Scalar A", "Sort Scalar B"] {
            store
                .insert_vertex_named(["QuerySortScalar"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let scan_project = || {
            vec![
                PlanOp::NodeScan {
                    variable: "n".into(),
                    label: Some("QuerySortScalar".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![project(prop("n", "name"), "name")],
                    distinct: false,
                },
            ]
        };
        let asc = plan(
            scan_project()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(var("name"), None, None)]),
                }])
                .collect(),
        );
        let desc = plan(
            scan_project()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(
                        var("name"),
                        Some(SortDirection::Desc),
                        None,
                    )]),
                }])
                .collect(),
        );

        let asc_result = store
            .execute_plan_query(&asc, &params(), GqlExecutionContext::default())
            .expect("execute ascending sort");
        let desc_result = store
            .execute_plan_query(&desc, &params(), GqlExecutionContext::default())
            .expect("execute descending sort");

        assert_eq!(
            text_column(&asc_result, "name"),
            vec!["Sort Scalar A", "Sort Scalar B", "Sort Scalar C"]
        );
        assert_eq!(
            text_column(&desc_result, "name"),
            vec!["Sort Scalar C", "Sort Scalar B", "Sort Scalar A"]
        );
    }
    #[test]
    fn sort_orders_multiple_keys() {
        let store = GraphStore::new();
        for (group, name) in [
            (Value::Int64(2), "Multi B"),
            (Value::Int64(1), "Multi B"),
            (Value::Int64(1), "Multi A"),
            (Value::Int64(2), "Multi A"),
        ] {
            store
                .insert_vertex_named(
                    ["QuerySortMulti"],
                    [("group", group), ("name", Value::Text(name.into()))],
                )
                .expect("insert vertex");
        }
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QuerySortMulti".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("n", "group"), "group"),
                    project(prop("n", "name"), "name"),
                ],
                distinct: false,
            },
            PlanOp::Sort {
                order_by: order_by(vec![
                    sort_item(var("group"), None, None),
                    sort_item(var("name"), Some(SortDirection::Desc), None),
                ]),
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute multi-key sort");

        assert_eq!(
            text_column(&result, "name"),
            vec!["Multi B", "Multi A", "Multi B", "Multi A"]
        );
    }
    #[test]
    fn sort_honors_explicit_null_ordering() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["QuerySortNulls"], Vec::<(&str, Value)>::new())
            .expect("insert null vertex");
        for name in ["Null Ada", "Null Bob"] {
            store
                .insert_vertex_named(["QuerySortNulls"], [("name", Value::Text(name.into()))])
                .expect("insert named vertex");
        }
        let base_ops = || {
            vec![
                PlanOp::NodeScan {
                    variable: "n".into(),
                    label: Some("QuerySortNulls".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![project(prop("n", "name"), "name")],
                    distinct: false,
                },
            ]
        };
        let nulls_first = plan(
            base_ops()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(var("name"), None, Some(NullOrder::First))]),
                }])
                .collect(),
        );
        let nulls_last = plan(
            base_ops()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(var("name"), None, Some(NullOrder::Last))]),
                }])
                .collect(),
        );

        let first = store
            .execute_plan_query(&nulls_first, &params(), GqlExecutionContext::default())
            .expect("execute nulls first sort");
        let last = store
            .execute_plan_query(&nulls_last, &params(), GqlExecutionContext::default())
            .expect("execute nulls last sort");

        assert_eq!(first.rows[0].get("name"), Some(&Value::Null));
        assert_eq!(last.rows[2].get("name"), Some(&Value::Null));
    }
    #[test]
    fn sort_rejects_incomparable_keys() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QuerySortIncomparable"],
                [("key", Value::Text("x".into()))],
            )
            .expect("insert text vertex");
        store
            .insert_vertex_named(["QuerySortIncomparable"], [("key", Value::Int64(1))])
            .expect("insert int vertex");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QuerySortIncomparable".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "key"), "key")],
                distinct: false,
            },
            PlanOp::Sort {
                order_by: order_by(vec![sort_item(var("key"), None, None)]),
            },
        ]);

        let err = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect_err("incomparable keys should fail");

        assert!(matches!(err, PlanQueryError::IncomparableSortValues { .. }));
    }
    #[test]
    fn topk_sorts_then_applies_offset_and_k() {
        let store = GraphStore::new();
        for name in ["TopK D", "TopK A", "TopK C", "TopK B"] {
            store
                .insert_vertex_named(["QueryTopK"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryTopK".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
            PlanOp::TopK {
                order_by: order_by(vec![sort_item(var("name"), None, None)]),
                k: Expr::new(ExprKind::Literal(Value::Int64(2))),
                offset: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute topk");

        assert_eq!(text_column(&result, "name"), vec!["TopK B", "TopK C"]);
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
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Limit A".into()))
        );
    }
    #[test]
    fn case_searched_skips_untaken_invalid_result() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSearched {
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Bool(false))),
                result: Expr::new(ExprKind::Sqrt(Box::new(Expr::new(ExprKind::Literal(
                    Value::Float32(-1.0),
                ))))),
            }],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Float32(1.0))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Float32(1.0));
    }
    #[test]
    fn case_searched_unknown_skips_invalid_then() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSearched {
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Null)),
                    result: Expr::new(ExprKind::Sqrt(Box::new(Expr::new(ExprKind::Literal(
                        Value::Float32(-1.0),
                    ))))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Bool(true))),
                    result: Expr::new(ExprKind::Literal(Value::Int32(2))),
                },
            ],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int32(3))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Int32(2));
    }
    #[test]
    fn case_simple_skips_untaken_invalid_result() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSimple {
            operand: Box::new(Expr::new(ExprKind::Literal(Value::Int32(0)))),
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Int32(1))),
                result: Expr::new(ExprKind::Sqrt(Box::new(Expr::new(ExprKind::Literal(
                    Value::Float32(-1.0),
                ))))),
            }],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int32(2))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Int32(2));
    }
    #[test]
    fn case_searched_unknown_condition_falls_through() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSearched {
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Null)),
                    result: Expr::new(ExprKind::Literal(Value::Int32(1))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Bool(true))),
                    result: Expr::new(ExprKind::Literal(Value::Int32(2))),
                },
            ],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int32(3))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Int32(2));
    }
    #[test]
    fn case_searched_all_unknown_uses_else() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSearched {
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Null)),
                result: Expr::new(ExprKind::Literal(Value::Int32(1))),
            }],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int32(3))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Int32(3));
    }
    #[test]
    fn case_simple_skips_incomparable_when_and_uses_else() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSimple {
            operand: Box::new(Expr::new(ExprKind::Literal(Value::Int32(1)))),
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Text("a".into()))),
                result: Expr::new(ExprKind::Literal(Value::Int32(99))),
            }],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int32(3))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Int32(3));
    }

    fn agg_sum_expr(inner: Expr, distinct: bool) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Sum,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct,
            order_by: None,
            filter: None,
        })
    }

    fn agg_min_expr(inner: Expr) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Min,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    fn agg_max_expr(inner: Expr) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Max,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    fn agg_avg_expr(inner: Expr) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Avg,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }
    #[test]
    fn aggregate_count_star_empty_graph_after_scan() {
        let store = GraphStore::new();
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("NoVerticesForAgg".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("cnt"))],
            },
            PlanOp::Project {
                columns: vec![project(agg_count_star(), "cnt")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("global aggregate on empty match");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(0)));
    }
    #[test]
    fn aggregate_groups_by_property_and_counts_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggGrpLbl"], [("dept", Value::Text("S".into()))])
            .expect("a");
        store
            .insert_vertex_named(["AggGrpLbl"], [("dept", Value::Text("S".into()))])
            .expect("b");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggGrpLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![prop("n", "dept")],
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("c"))],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("n", "dept"), "d"),
                    project(agg_count_star(), "c"),
                ],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("grouped");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("d"), Some(&Value::Text("S".into())));
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(2)));
    }
    #[test]
    fn aggregate_sum_min_max_avg_numeric_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggNumLbl"], [("v", Value::Int64(10))])
            .expect("a");
        store
            .insert_vertex_named(["AggNumLbl"], [("v", Value::Int64(20))])
            .expect("b");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggNumLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![
                    agg_spec(AggregateFunc::Sum, Some(prop("n", "v")), false, Some("s")),
                    agg_spec(AggregateFunc::Min, Some(prop("n", "v")), false, Some("mn")),
                    agg_spec(AggregateFunc::Max, Some(prop("n", "v")), false, Some("mx")),
                    agg_spec(AggregateFunc::Avg, Some(prop("n", "v")), false, Some("a")),
                ],
            },
            PlanOp::Project {
                columns: vec![
                    project(agg_sum_expr(prop("n", "v"), false), "s"),
                    project(agg_min_expr(prop("n", "v")), "mn"),
                    project(agg_max_expr(prop("n", "v")), "mx"),
                    project(agg_avg_expr(prop("n", "v")), "a"),
                ],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("agg");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("s"), Some(&Value::Int64(30)));
        assert_eq!(result.rows[0].get("mn"), Some(&Value::Int64(10)));
        assert_eq!(result.rows[0].get("mx"), Some(&Value::Int64(20)));
        assert_eq!(result.rows[0].get("a"), Some(&Value::Int64(15)));
    }
    #[test]
    fn aggregate_count_distinct_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggDistLbl"], [("k", Value::Int64(1))])
            .expect("a");
        store
            .insert_vertex_named(["AggDistLbl"], [("k", Value::Int64(1))])
            .expect("b");
        let count_distinct = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Count,
            expr: Some(Box::new(prop("n", "k"))),
            expr2: None,
            distinct: true,
            order_by: None,
            filter: None,
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggDistLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![agg_spec(
                    AggregateFunc::Count,
                    Some(prop("n", "k")),
                    true,
                    Some("c"),
                )],
            },
            PlanOp::Project {
                columns: vec![project(count_distinct, "c")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("distinct");
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(1)));
    }
    #[test]
    fn aggregate_grouped_empty_input_yields_no_rows() {
        let store = GraphStore::new();
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("NoSuchAggLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![prop("n", "dept")],
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("c"))],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("n", "dept"), "d"),
                    project(agg_count_star(), "c"),
                ],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("empty groups");
        assert!(result.rows.is_empty());
    }
    #[test]
    fn aggregate_count_star_with_filter_manual_plan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["FiltAggLbl"], [("ok", Value::Bool(false))])
            .expect("v0");
        store
            .insert_vertex_named(["FiltAggLbl"], [("ok", Value::Bool(true))])
            .expect("v1");
        let filter = Expr::new(ExprKind::Compare {
            left: Box::new(prop("n", "ok")),
            op: CmpOp::Eq,
            right: Box::new(Expr::new(ExprKind::Literal(Value::Bool(true)))),
        });
        let count_star_filtered = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::CountStar,
            expr: None,
            expr2: None,
            distinct: false,
            order_by: None,
            filter: Some(Box::new(filter.clone())),
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("FiltAggLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::CountStar,
                    expr: None,
                    expr2: None,
                    distinct: false,
                    filter: Some(filter),
                    order_by: None,
                    alias: Some("c".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![project(count_star_filtered, "c")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("filtered");
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(1)));
    }
    #[test]
    fn aggregate_collect_list_manual_plan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["CollLbl"], [("v", Value::Int64(3))])
            .expect("a");
        store
            .insert_vertex_named(["CollLbl"], [("v", Value::Int64(1))])
            .expect("b");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("CollLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::Collect,
                    expr: Some(prop("n", "v")),
                    expr2: None,
                    distinct: false,
                    filter: None,
                    order_by: None,
                    alias: Some("xs".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![project(
                    Expr::new(ExprKind::Aggregate {
                        func: AggregateFunc::Collect,
                        expr: Some(Box::new(prop("n", "v"))),
                        expr2: None,
                        distinct: false,
                        order_by: None,
                        filter: None,
                    }),
                    "xs",
                )],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("collect");
        match result.rows[0].get("xs") {
            Some(Value::List(xs)) => {
                assert_eq!(xs.len(), 2);
            }
            other => panic!("expected list: {other:?}"),
        }
    }
    #[test]
    fn aggregate_percentile_cont_manual_plan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PctLbl"], [("v", Value::Int64(10))])
            .expect("a");
        store
            .insert_vertex_named(["PctLbl"], [("v", Value::Int64(30))])
            .expect("b");
        let p = Expr::new(ExprKind::Literal(Value::Float64(0.5)));
        let agg = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::PercentileCont,
            expr: Some(Box::new(prop("n", "v"))),
            expr2: Some(Box::new(p.clone())),
            distinct: false,
            order_by: None,
            filter: None,
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("PctLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::PercentileCont,
                    expr: Some(prop("n", "v")),
                    expr2: Some(p),
                    distinct: false,
                    filter: None,
                    order_by: None,
                    alias: Some("m".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![project(agg, "m")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("pct");
        match result.rows[0].get("m") {
            Some(Value::Float64(f)) => assert!((f - 20.0).abs() < 1e-9),
            other => panic!("expected float median: {other:?}"),
        }
    }
    #[test]
    fn aggregate_sum_with_expr2_is_rejected() {
        let store = GraphStore::new();
        let plan = plan(vec![PlanOp::Aggregate {
            group_by: Vec::new(),
            aggregates: vec![AggregateSpec {
                func: AggregateFunc::Sum,
                expr: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
                expr2: Some(Expr::new(ExprKind::Literal(Value::Int64(2)))),
                distinct: false,
                filter: None,
                order_by: None,
                alias: None,
            }],
        }]);
        let err = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect_err("sum with expr2");
        assert!(
            matches!(err, PlanQueryError::UnsupportedOp(name) if name == "Aggregate.expr2"),
            "{err:?}"
        );
    }
    #[test]
    fn executes_planner_match_return_count_star() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerAggCntLbl"], Vec::<(&str, Value)>::new())
            .expect("vertex");
        let plan = plan_gql("MATCH (n:PlannerAggCntLbl) RETURN count(*) AS c");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("planner aggregate");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(1)));
    }
    #[test]
    fn executes_planner_match_return_count_star_plus_literal() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerAggPlus"], Vec::<(&str, Value)>::new())
            .expect("v1");
        store
            .insert_vertex_named(["PlannerAggPlus"], Vec::<(&str, Value)>::new())
            .expect("v2");
        let plan = plan_gql("MATCH (n:PlannerAggPlus) RETURN count(*) + 1 AS c");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("nested aggregate expr");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(3)));
    }
    #[test]
    fn executes_planner_avg_nested_in_arithmetic() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerAggAvgArith"], [("x", Value::Int64(10))]);
        let _ = store.insert_vertex_named(["PlannerAggAvgArith"], [("x", Value::Int64(30))]);
        let plan = plan_gql("MATCH (n:PlannerAggAvgArith) RETURN avg(n.x) * 2 AS doubled");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("avg * 2");
        assert_eq!(result.rows.len(), 1);
        match result.rows[0].get("doubled") {
            Some(Value::Float64(f)) => assert!((f - 40.0).abs() < 1e-6),
            Some(Value::Int64(i)) => assert_eq!(*i, 40),
            other => panic!("expected numeric doubled: {other:?}"),
        }
    }
    #[test]
    fn executes_planner_group_by_having_count_filter() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(2))]);
        let plan = plan_gql(
            "MATCH (n:PlannerHavingK) RETURN n.k, count(*) AS cnt GROUP BY n.k HAVING count(*) > 1",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("having");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("n.k"), Some(&Value::Int64(1)));
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
    }
    #[test]
    fn executes_planner_group_by_having_count_return_alias() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(2))]);
        let plan = plan_gql(
            "MATCH (n:PlannerHavingK) RETURN n.k, count(*) AS cnt GROUP BY n.k HAVING cnt > 1",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("having with return alias");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("n.k"), Some(&Value::Int64(1)));
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
    }
    #[test]
    fn executes_planner_collect_list_names() {
        let store = GraphStore::new();
        let _ =
            store.insert_vertex_named(["PlannerAggCollect"], [("name", Value::Text("a".into()))]);
        let _ =
            store.insert_vertex_named(["PlannerAggCollect"], [("name", Value::Text("b".into()))]);
        let plan = plan_gql("MATCH (n:PlannerAggCollect) RETURN COLLECT_LIST(n.name) AS names");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("collect_list");
        assert_eq!(result.rows.len(), 1);
        let list = result.rows[0].get("names").expect("names column");
        let Value::List(items) = list else {
            panic!("expected list, got {list:?}");
        };
        assert_eq!(items.len(), 2);
        let mut texts: Vec<String> = items
            .iter()
            .map(|v| match v {
                Value::Text(t) => t.clone(),
                _ => panic!("expected text in list: {v:?}"),
            })
            .collect();
        texts.sort();
        assert_eq!(texts, vec!["a".to_string(), "b".to_string()]);
    }
    #[test]
    fn executes_planner_stddev_pop_two_values() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerAggStd"], [("v", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerAggStd"], [("v", Value::Int64(3))]);
        let plan = plan_gql("MATCH (n:PlannerAggStd) RETURN STDDEV_POP(n.v) AS s");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("stddev_pop");
        assert_eq!(result.rows.len(), 1);
        match result.rows[0].get("s") {
            Some(Value::Float64(f)) => assert!((f - 1.0).abs() < 1e-6),
            other => panic!("expected float stddev: {other:?}"),
        }
    }
    #[test]
    fn executes_planner_percentile_cont_planned() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerAggPct"], [("v", Value::Int64(10))]);
        let _ = store.insert_vertex_named(["PlannerAggPct"], [("v", Value::Int64(20))]);
        let _ = store.insert_vertex_named(["PlannerAggPct"], [("v", Value::Int64(30))]);
        let plan = plan_gql("MATCH (n:PlannerAggPct) RETURN PERCENTILE_CONT(n.v, 0.5) AS m");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("percentile");
        assert_eq!(result.rows.len(), 1);
        match result.rows[0].get("m") {
            Some(Value::Float64(f)) => assert!((f - 20.0).abs() < 1e-6),
            other => panic!("expected float median: {other:?}"),
        }
    }
}
