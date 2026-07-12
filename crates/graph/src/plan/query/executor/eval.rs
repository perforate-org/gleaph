//! Expression evaluation, projection, and binding materialization.

use std::collections::BTreeMap;

use jiff::Unit;

use crate::edge_inline_value_scalar_codec::decode_edge_inline_value_scalar;
use gleaph_gql::Value;
use gleaph_gql::ast::{
    AggregateFunc, CmpOp, DurationQualifier, Expr, ExprKind, ObjectName, TruthValue,
};
use gleaph_gql::types::LabelExpr;
use gleaph_gql_planner::plan::{ProjectColumn, Str};
use gleaph_graph_kernel::entry::{
    EdgeLabelId, EdgeSlotIndex, PreparedWeightDecoder, PropertyId, Vertex,
};
use gleaph_graph_kernel::federation::ElementIdEncodingKey;
use gleaph_graph_kernel::path::GraphPathVertexId;
use gleaph_graph_kernel::plan_exec::{
    ResolvedInlineSchema, ResolvedInlineStructField, ResolvedLabelTable,
};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;

use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::PlanBinding;
use super::bindings::EdgeBinding;
#[cfg(feature = "cypher")]
use super::bindings::{
    edge_group_element_at_index, path_group_element_at_index, vertex_group_element_at_index,
};
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
use crate::plan::time::{
    current_date_value, current_datetime_value, current_local_datetime_value,
    current_local_time_value, current_time_value,
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
                Some(binding) => binding_to_value(
                    evaluator.store,
                    &evaluator.element_id_key,
                    evaluator.resolved_labels,
                    binding,
                ),
                None => Err(PlanQueryError::MissingBinding {
                    variable: projected_name,
                }),
            }
        }
        Err(err) => Err(err),
    }
}

fn decode_gleaph_weight_for_edge_binding(
    decoder: &PreparedWeightDecoder,
    edge: &EdgeBinding,
) -> Result<f32, PlanQueryError> {
    super::super::gleaph_weight::decode_shortest_hop_cost_from_edge_binding(edge).or_else(|_| {
        super::super::gleaph_weight::decode_traversal_edge_weight_prepared(
            decoder,
            edge.inline_value_len(),
            edge.inline_value_bytes_slice(),
        )
    })
}

#[cfg(feature = "cypher")]
fn eval_list_index_value(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    list: &Expr,
    index: &Expr,
) -> Result<Value, PlanQueryError> {
    if let ExprKind::Variable(name) = &list.kind {
        let index_value = evaluator.eval_expr(row, index)?;
        let idx = list_index_to_i64(&index_value)?;
        return match row.get(name.as_str()) {
            Some(PlanBinding::EdgeGroup(edges)) => edge_group_element_at_index(edges, idx)
                .map(|edge| edge_to_value(evaluator.store, evaluator.resolved_labels, edge.clone()))
                .transpose()?
                .map_or(Ok(Value::Null), Ok),
            Some(PlanBinding::VertexGroup(vertices)) => {
                vertex_group_element_at_index(vertices, idx)
                    .map(|vertex_id| {
                        vertex_to_value(evaluator.store, evaluator.resolved_labels, vertex_id)
                    })
                    .transpose()?
                    .map_or(Ok(Value::Null), Ok)
            }
            Some(PlanBinding::PathGroup(paths)) => match path_group_element_at_index(paths, idx) {
                Some(pb) => Ok(path_binding_to_value(
                    evaluator.store,
                    &evaluator.element_id_key,
                    pb,
                )),
                None => Ok(Value::Null),
            },
            Some(binding) => {
                let value = binding_to_value(
                    evaluator.store,
                    &evaluator.element_id_key,
                    evaluator.resolved_labels,
                    binding,
                )?;
                list_index_into_value(&value, idx)
            }
            None => Err(PlanQueryError::MissingBinding {
                variable: name.clone(),
            }),
        };
    }
    let list_value = evaluator.eval_expr(row, list)?;
    let index_value = evaluator.eval_expr(row, index)?;
    let idx = list_index_to_i64(&index_value)?;
    list_index_into_value(&list_value, idx)
}

fn list_index_to_i64(value: &Value) -> Result<i64, PlanQueryError> {
    match value {
        Value::Int64(v) => Ok(*v),
        Value::Int32(v) => Ok(i64::from(*v)),
        Value::Uint64(v) => i64::try_from(*v).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: format!("list index out of range: {v}"),
        }),
        Value::Null => Err(PlanQueryError::InvalidExpressionValue {
            expression: "list index is null".into(),
        }),
        other => Err(PlanQueryError::InvalidExpressionValue {
            expression: format!("list index must be integral, got {other:?}"),
        }),
    }
}

fn list_index_into_value(list: &Value, index: i64) -> Result<Value, PlanQueryError> {
    let Value::List(items) = list else {
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: format!("list index requires a list, got {list:?}"),
        });
    };
    if items.is_empty() {
        return Ok(Value::Null);
    }
    let len = items.len() as i64;
    let idx = if index < 0 { len + index } else { index };
    Ok(items
        .get(usize::try_from(idx).unwrap_or(items.len()))
        .cloned()
        .unwrap_or(Value::Null))
}

fn try_eval_horizontal_sum_gleaph_weight(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    inner: &Expr,
) -> Result<Option<Value>, PlanQueryError> {
    let ExprKind::FunctionCall {
        name,
        args,
        distinct,
    } = &inner.kind
    else {
        return Ok(None);
    };
    if !super::super::gleaph_weight::is_gleaph_weight_call(name, *distinct)
        || args.len() != 1
        || *distinct
    {
        return Ok(None);
    }
    let Some(super::super::gleaph_weight::GleaphWeightEdgeRef::SingletonVar(group_var)) =
        super::super::gleaph_weight::gleaph_weight_edge_ref(&args[0])
    else {
        return Ok(None);
    };
    let Some(PlanBinding::EdgeGroup(edges)) = row.get(group_var.as_str()) else {
        return Ok(None);
    };
    let decoder = evaluator
        .gleaph_weight_decoders
        .as_ref()
        .and_then(|map| map.get(&group_var))
        .ok_or_else(|| PlanQueryError::GleaphWeight {
            message: format!(
                "SUM(GLEAPH.WEIGHT({group_var})): no prepared decoder for this edge variable"
            ),
        })?;
    let mut sum = 0.0f32;
    for edge in edges.iter() {
        sum += decode_gleaph_weight_for_edge_binding(decoder, edge)?;
    }
    Ok(Some(Value::Float32(sum)))
}

// `evaluator` is only consulted by the cypher list-index (`GroupElement`) arm.
#[cfg_attr(not(feature = "cypher"), allow(unused_variables))]
fn try_eval_gleaph_weight(
    decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    name: &ObjectName,
    args: &[Expr],
    distinct: bool,
    row: &PlanRow,
    evaluator: &QueryExprEvaluator<'_>,
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
    let Some(edge_ref) = super::super::gleaph_weight::gleaph_weight_edge_ref(&args[0]) else {
        return Err(PlanQueryError::GleaphWeight {
            message: "GLEAPH.WEIGHT argument must be an edge variable or indexed group element"
                .into(),
        });
    };
    match edge_ref {
        super::super::gleaph_weight::GleaphWeightEdgeRef::SingletonVar(edge_var) => {
            let decoder = map
                .get(&edge_var)
                .ok_or_else(|| PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH.WEIGHT({edge_var}): no prepared decoder for this edge variable"
                    ),
                })?;
            let binding =
                row.get(edge_var.as_str())
                    .ok_or_else(|| PlanQueryError::MissingBinding {
                        variable: edge_var.clone(),
                    })?;
            match binding {
                PlanBinding::Value(Value::Null) => Ok(Some(Value::Null)),
                PlanBinding::Edge(edge) => {
                    let w = decode_gleaph_weight_for_edge_binding(decoder, edge)?;
                    Ok(Some(Value::Float32(w)))
                }
                PlanBinding::EdgeGroup(_) => Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH.WEIGHT({edge_var}): edge variable is a group; \
                         use an element index such as GLEAPH.WEIGHT({edge_var}[-1]) \
                         or SUM(GLEAPH.WEIGHT({edge_var}))"
                    ),
                }),
                _ => Err(PlanQueryError::GleaphWeight {
                    message: format!("GLEAPH.WEIGHT({edge_var}): binding is not an edge"),
                }),
            }
        }
        #[cfg(feature = "cypher")]
        super::super::gleaph_weight::GleaphWeightEdgeRef::GroupElement { group_var, index } => {
            let decoder = map
                .get(&group_var)
                .ok_or_else(|| PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH.WEIGHT({group_var}[…]): no prepared decoder for this edge variable"
                    ),
                })?;
            let Some(PlanBinding::EdgeGroup(edges)) = row.get(group_var.as_str()) else {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH.WEIGHT({group_var}[…]): binding is not a variable-length edge group"
                    ),
                });
            };
            let index_value = evaluator.eval_expr(row, &index)?;
            let idx = list_index_to_i64(&index_value)?;
            let Some(edge) = edge_group_element_at_index(edges, idx) else {
                return Ok(Some(Value::Null));
            };
            let w = decode_gleaph_weight_for_edge_binding(decoder, edge)?;
            Ok(Some(Value::Float32(w)))
        }
    }
}

/// Reads an inline edge property if `(label_id, property_id)` matches the Router-resolved
/// inline schema for this concrete edge label. Returns `Ok(None)` when the property is not the
/// inline slot, allowing the caller to fall back to the sidecar property store. Returns an error
/// when the inline slot matches but the payload/schema is malformed, missing, or unsupported.
pub(crate) fn try_read_inline_edge_property(
    edge: &EdgeBinding,
    property_id: PropertyId,
    resolved_labels: Option<&ResolvedLabelTable>,
) -> Result<Option<Value>, PlanQueryError> {
    let Some(label) = crate::edge_inline_value_schema::resolved_edge_label_with(
        resolved_labels,
        EdgeLabelId::from_raw(edge.handle.label_id.raw()),
    ) else {
        return Ok(None);
    };
    let Some(inline_schema) = label.inline_schema() else {
        return Ok(None);
    };
    if inline_schema.property_id() != property_id {
        return Ok(None);
    }

    let bytes = edge.inline_value_bytes_slice();
    let required_width = usize::from(label.inline_value_profile.required_byte_width());
    if required_width == 0 {
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: format!(
                "inline property read for label {} requires a non-zero payload width",
                edge.handle.label_id.raw()
            ),
        });
    }
    if bytes.len() != required_width {
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: format!(
                "inline payload width mismatch for label {}: expected {} bytes, got {}",
                edge.handle.label_id.raw(),
                required_width,
                bytes.len()
            ),
        });
    }

    match inline_schema {
        ResolvedInlineSchema::Scalar { .. } => {
            decode_edge_inline_value_scalar(&label.inline_value_profile, bytes)
                .map(Some)
                .map_err(|err| PlanQueryError::InvalidExpressionValue {
                    expression: format!(
                        "inline payload decode error for label {}: {err}",
                        edge.handle.label_id.raw()
                    ),
                })
        }
        ResolvedInlineSchema::Struct { fields, .. } => {
            validate_and_decode_inline_struct(edge.handle.label_id.raw(), bytes, fields).map(Some)
        }
    }
}

fn validate_and_decode_inline_struct(
    label_id_raw: u16,
    payload: &[u8],
    fields: &[ResolvedInlineStructField],
) -> Result<Value, PlanQueryError> {
    if fields.is_empty() {
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: format!("inline struct schema for label {label_id_raw} has no fields"),
        });
    }

    let mut field_width_sum: u16 = 0;
    let mut previous_end: u16 = 0;
    let mut seen_names = std::collections::HashSet::new();
    for (idx, field) in fields.iter().enumerate() {
        if !seen_names.insert(field.name.clone()) {
            return Err(PlanQueryError::InvalidExpressionValue {
                expression: format!(
                    "inline struct field name `{}` is duplicated for label {label_id_raw}",
                    field.name
                ),
            });
        }
        let width = field.profile.required_byte_width();
        if width == 0 {
            return Err(PlanQueryError::InvalidExpressionValue {
                expression: format!(
                    "inline struct field {} for label {label_id_raw} has zero byte width",
                    field.name
                ),
            });
        }
        field_width_sum = field_width_sum.checked_add(width).ok_or_else(|| {
            PlanQueryError::InvalidExpressionValue {
                expression: format!(
                    "inline struct field width sum for label {label_id_raw} overflows"
                ),
            }
        })?;

        if idx > 0 && field.byte_offset < previous_end {
            return Err(PlanQueryError::InvalidExpressionValue {
                expression: format!(
                    "inline struct field {} for label {label_id_raw} overlaps previous field (offset {} < end {})",
                    field.name, field.byte_offset, previous_end
                ),
            });
        }
        previous_end = field.byte_offset.checked_add(width).ok_or_else(|| {
            PlanQueryError::InvalidExpressionValue {
                expression: format!(
                    "inline struct field {} for label {label_id_raw} extends past u16 bounds",
                    field.name
                ),
            }
        })?;
    }

    if usize::from(field_width_sum) != payload.len() {
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: format!(
                "inline struct payload width mismatch for label {label_id_raw}: expected {field_width_sum} bytes, got {}",
                payload.len()
            ),
        });
    }

    let mut record_fields = Vec::with_capacity(fields.len());
    for field in fields {
        let start = usize::from(field.byte_offset);
        let end = start + usize::from(field.profile.required_byte_width());
        if end > payload.len() {
            return Err(PlanQueryError::InvalidExpressionValue {
                expression: format!(
                    "inline struct field {} for label {label_id_raw} exceeds payload bounds (offset {} + width {} > {})",
                    field.name,
                    field.byte_offset,
                    field.profile.required_byte_width(),
                    payload.len()
                ),
            });
        }
        let value = decode_edge_inline_value_scalar(&field.profile, &payload[start..end]).map_err(
            |err| PlanQueryError::InvalidExpressionValue {
                expression: format!(
                    "inline struct field {} decode error for label {label_id_raw}: {err}",
                    field.name
                ),
            },
        )?;
        record_fields.push((field.name.clone(), value));
    }

    Ok(Value::Record(record_fields))
}

impl QueryExprEvaluator<'_> {
    pub(crate) fn eval_expr(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        match &expr.kind {
            ExprKind::Literal(value) => Ok(value.clone()),
            ExprKind::Paren(inner) => self.eval_expr(row, inner),
            ExprKind::Variable(name) => binding_to_value(
                self.store,
                &self.element_id_key,
                self.resolved_labels,
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
            ExprKind::Aggregate {
                func,
                expr: inner,
                distinct,
                filter,
                ..
            } => {
                if !*distinct
                    && filter.is_none()
                    && *func == AggregateFunc::Sum
                    && let Some(inner) = inner
                    && let Some(value) = try_eval_horizontal_sum_gleaph_weight(self, row, inner)?
                {
                    return Ok(value);
                }
                let Some(specs) = self.aggregate_specs else {
                    return Err(PlanQueryError::UnsupportedExpression {
                        expression: "aggregate".to_owned(),
                    });
                };
                super::super::aggregate::resolve_aggregate_from_row(row, expr, specs)
            }
            #[cfg(feature = "cypher")]
            ExprKind::ListIndex { list, index } => eval_list_index_value(self, row, list, index),
            ExprKind::Cardinality { expr, .. } => {
                if let ExprKind::Variable(name) = &expr.kind {
                    match row.get(name.as_str()) {
                        Some(PlanBinding::EdgeGroup(edges)) => {
                            return Ok(Value::Int64(edges.len() as i64));
                        }
                        Some(PlanBinding::VertexGroup(vertices)) => {
                            return Ok(Value::Int64(vertices.len() as i64));
                        }
                        Some(PlanBinding::PathGroup(paths)) => {
                            return Ok(Value::Int64(paths.len() as i64));
                        }
                        Some(binding) => {
                            let value = binding_to_value(
                                self.store,
                                &self.element_id_key,
                                self.resolved_labels,
                                binding,
                            )?;
                            if let Value::List(items) = value {
                                return Ok(Value::Int64(items.len() as i64));
                            }
                        }
                        None => {
                            return Err(PlanQueryError::MissingBinding {
                                variable: name.clone(),
                            });
                        }
                    }
                }
                let value = self.eval_expr(row, expr)?;
                match value {
                    Value::List(items) => Ok(Value::Int64(items.len() as i64)),
                    Value::Null => Ok(Value::Null),
                    other => Err(PlanQueryError::InvalidExpressionValue {
                        expression: format!("CARDINALITY expects a list, got {other:?}"),
                    }),
                }
            }
            ExprKind::FunctionCall {
                name,
                args,
                distinct,
            } => {
                if let Some(v) = try_eval_gleaph_weight(
                    self.gleaph_weight_decoders,
                    name,
                    args,
                    *distinct,
                    row,
                    self,
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
            ExprKind::CurrentTimestamp => Ok(current_datetime_value()),
            ExprKind::CurrentLocalTimestamp => Ok(current_local_datetime_value()),
            ExprKind::CurrentDate => Ok(current_date_value()),
            ExprKind::CurrentTime => Ok(current_time_value()),
            ExprKind::CurrentLocalTime => Ok(current_local_time_value()),
            ExprKind::DateLiteral(args) | ExprKind::DateFunction(args) => self
                .eval_temporal_constructor(row, args, "DATE", |s| {
                    gleaph_gql::temporal::parse_date(s).map(Value::Date)
                }),
            ExprKind::TimeLiteral(args) | ExprKind::TimeFunction(args) => self
                .eval_temporal_constructor(row, args, "TIME", |s| {
                    gleaph_gql::temporal::parse_time(s).map(Value::LocalTime)
                }),
            ExprKind::LocalTimeFunction(args) => {
                self.eval_temporal_constructor(row, args, "LOCAL_TIME", |s| {
                    gleaph_gql::temporal::parse_local_time(s).map(Value::LocalTime)
                })
            }
            ExprKind::ZonedTimeFunction(args) => {
                self.eval_temporal_constructor(row, args, "ZONED_TIME", |s| {
                    gleaph_gql::temporal::parse_zoned_time(s)
                        .map(|(nanos, tz)| Value::ZonedTime(nanos, tz))
                })
            }
            ExprKind::DatetimeLiteral(args) => {
                self.eval_temporal_constructor(row, args, "DATETIME", |s| {
                    gleaph_gql::temporal::parse_datetime(s)
                        .map(|(secs, nanos)| Value::DateTime(secs, nanos))
                })
            }
            ExprKind::LocalDatetimeFunction(args) => {
                self.eval_temporal_constructor(row, args, "LOCAL_DATETIME", |s| {
                    gleaph_gql::temporal::parse_local_datetime(s)
                        .map(|(secs, nanos)| Value::LocalDateTime(secs, nanos))
                })
            }
            ExprKind::ZonedDatetimeFunction(args) => {
                self.eval_temporal_constructor(row, args, "ZONED_DATETIME", |s| {
                    gleaph_gql::temporal::parse_zoned_datetime(s)
                        .map(|(secs, nanos, tz)| Value::ZonedDateTime(secs, nanos, tz))
                })
            }
            ExprKind::TimestampLiteral(args) => {
                self.eval_temporal_constructor(row, args, "TIMESTAMP", |s| {
                    gleaph_gql::temporal::parse_local_datetime(s)
                        .map(|(secs, nanos)| Value::LocalDateTime(secs, nanos))
                })
            }
            ExprKind::DurationLiteral(args) | ExprKind::DurationFunction(args) => self
                .eval_temporal_constructor(row, args, "DURATION", |s| {
                    gleaph_gql::temporal::parse_duration(s)
                        .map(|(months, nanos)| Value::Duration(months, nanos))
                }),
            ExprKind::DurationBetween {
                left,
                right,
                qualifier,
            } => self.eval_duration_between(row, left, right, qualifier.as_ref()),
            _ => Err(PlanQueryError::UnsupportedExpression {
                expression: format!("{:?}", expr.kind),
            }),
        }
    }

    /// Evaluate a temporal constructor expression that expects exactly one string
    /// argument (possibly wrapped in `ExprKind::Literal(Value::Text(...))`).
    fn eval_temporal_constructor(
        &self,
        row: &PlanRow,
        args: &[Expr],
        name: &str,
        parse: impl FnOnce(&str) -> Option<Value>,
    ) -> Result<Value, PlanQueryError> {
        if args.len() != 1 {
            return Err(PlanQueryError::InvalidExpressionValue {
                expression: format!("{name} expects exactly one string argument"),
            });
        }
        let value = self.eval_expr(row, &args[0])?;
        let Value::Text(s) = value else {
            return Err(PlanQueryError::InvalidExpressionValue {
                expression: format!("{name} expects a string argument, got {value:?}"),
            });
        };
        parse(&s).ok_or_else(|| PlanQueryError::InvalidExpressionValue {
            expression: format!("{name} could not parse '{s}'"),
        })
    }

    fn eval_duration_between(
        &self,
        row: &PlanRow,
        left: &Expr,
        right: &Expr,
        qualifier: Option<&DurationQualifier>,
    ) -> Result<Value, PlanQueryError> {
        let left = self.eval_expr(row, left)?;
        let right = self.eval_expr(row, right)?;

        match qualifier {
            Some(DurationQualifier::YearToMonth) => {
                let months = temporal_year_month_span(&left, &right)?;
                Ok(Value::Duration(months, 0))
            }
            Some(DurationQualifier::DayToSecond) | None => {
                // ISO/IEC 39075 defaults an omitted qualifier to DAY TO SECOND.
                let nanos = temporal_nanos_between(&left, &right)?;
                Ok(Value::Duration(0, nanos))
            }
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
                    self.store,
                    self.resolved_labels,
                    *vertex_id,
                    vertex,
                    label,
                ))
            }
            Some(PlanBinding::Value(Value::Null)) => Ok(false),
            Some(
                PlanBinding::Value(_)
                | PlanBinding::Edge(_)
                | PlanBinding::EdgeGroup(_)
                | PlanBinding::VertexGroup(_)
                | PlanBinding::Path(_)
                | PlanBinding::PathGroup(_)
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
                    .resolved_property_id(property)
                    .and_then(|property_id| self.store.vertex_property(*vertex_id, property_id))
                    .map_or(Ok(Value::Null), Ok),
                Some(PlanBinding::Edge(edge)) => {
                    let property_id = self.resolved_property_id(property);
                    if let Some(property_id) = property_id
                        && let Some(value) =
                            try_read_inline_edge_property(edge, property_id, self.resolved_labels)?
                    {
                        return Ok(value);
                    }
                    property_id
                        .and_then(|property_id| self.store.edge_property(edge.handle, property_id))
                        .map_or(Ok(Value::Null), Ok)
                }
                Some(PlanBinding::EdgeGroup(_)) => Err(PlanQueryError::InvalidExpressionValue {
                    expression: format!(
                        "property access on group edge variable '{name}.{property}' requires element indexing"
                    ),
                }),
                Some(PlanBinding::VertexGroup(_)) => Err(PlanQueryError::InvalidExpressionValue {
                    expression: format!(
                        "property access on group node variable '{name}.{property}' requires element indexing"
                    ),
                }),
                Some(PlanBinding::Value(value)) => Ok(property_from_value(value, property)),
                Some(PlanBinding::Path(pb)) => Ok(record_property(
                    &path_binding_to_value(self.store, &self.element_id_key, pb),
                    property,
                )),
                Some(PlanBinding::PathGroup(_)) => Err(PlanQueryError::InvalidExpressionValue {
                    expression: format!(
                        "property access on group path variable '{name}.{property}' requires element indexing"
                    ),
                }),
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
                    self.store,
                    &self.element_id_key,
                    *vertex_id,
                )?)),
                Some(PlanBinding::RemoteVertex(vertex_id)) => Ok(Value::Bytes(
                    GraphPathVertexId::from_global(&self.element_id_key, *vertex_id)
                        .to_bytes()
                        .to_vec(),
                )),
                Some(PlanBinding::Edge(edge)) => Ok(Value::Bytes(edge_element_id_bytes(
                    &self.element_id_key,
                    local_shard_id(self.store),
                    edge.handle.owner_vertex_id,
                    EdgeSlotIndex::from_raw(edge.handle.slot_index),
                )?)),
                Some(PlanBinding::EdgeGroup(_)) => Err(PlanQueryError::InvalidExpressionValue {
                    expression: format!(
                        "ELEMENT_ID({name}) on a group edge variable requires element indexing"
                    ),
                }),
                Some(PlanBinding::VertexGroup(_)) => Err(PlanQueryError::InvalidExpressionValue {
                    expression: format!(
                        "ELEMENT_ID({name}) on a group node variable requires element indexing"
                    ),
                }),
                Some(PlanBinding::PathGroup(_)) => Err(PlanQueryError::InvalidExpressionValue {
                    expression: format!(
                        "ELEMENT_ID({name}) on a group path variable requires element indexing"
                    ),
                }),
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

    fn try_eval_horizontal_sum_operand(
        &self,
        row: &PlanRow,
        expr: &Expr,
    ) -> Result<Option<Value>, PlanQueryError> {
        try_eval_horizontal_sum_gleaph_weight(self, row, expr)
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
            let value = binding_to_value(
                evaluator.store,
                &evaluator.element_id_key,
                evaluator.resolved_labels,
                binding,
            )?;
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
            if column.alias.is_none()
                && row.is_singleton_binding(var_name.as_str())
                && let Some(layout) = row.shared_layout()
            {
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
        // Preserve typed graph bindings when the column is a plain variable
        // reference.  Converting every projected column to Value would lose
        // vertex/edge identity, which breaks chained DML statements that reuse
        // matched elements (e.g. RETURN a NEXT INSERT (a)-[:L]->(b)).
        let binding = if let ExprKind::Variable(var_name) = &column.expr.kind {
            row.get(var_name.as_str())
                .ok_or_else(|| PlanQueryError::MissingBinding {
                    variable: var_name.clone(),
                })?
                .clone()
        } else {
            PlanBinding::Value(evaluator.eval_expr(row, &column.expr)?)
        };
        out.insert(name, binding);
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
    key: &ElementIdEncodingKey,
    row: &PlanRow,
) -> Result<BTreeMap<String, Value>, PlanQueryError> {
    if row.len() == 1 {
        let (name, binding) = row.iter().next().expect("len==1 guarantees one entry");
        let value = binding_to_value(store, key, None, binding)?;
        let mut out = BTreeMap::new();
        out.insert(name.to_string(), value);
        return Ok(out);
    }
    row.iter()
        .map(|(name, binding)| {
            binding_to_value(store, key, None, binding).map(|value| (name.to_string(), value))
        })
        .collect()
}

pub(crate) fn binding_to_value(
    store: &GraphStore,
    key: &ElementIdEncodingKey,
    resolved_labels: Option<&ResolvedLabelTable>,
    binding: &PlanBinding,
) -> Result<Value, PlanQueryError> {
    match binding {
        PlanBinding::Vertex(vertex_id) => vertex_to_value(store, resolved_labels, *vertex_id),
        PlanBinding::RemoteVertex(vertex_id) => Ok(Value::Record(vec![
            (
                "id".to_owned(),
                Value::Bytes(
                    GraphPathVertexId::from_global(key, *vertex_id)
                        .to_bytes()
                        .to_vec(),
                ),
            ),
            ("remote".to_owned(), Value::Bool(true)),
        ])),
        PlanBinding::Edge(edge) => edge_to_value(store, resolved_labels, edge.clone()),
        PlanBinding::EdgeGroup(edges) => Ok(Value::List(
            edges
                .iter()
                .cloned()
                .map(|edge| edge_to_value(store, resolved_labels, edge))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        PlanBinding::VertexGroup(vertices) => Ok(Value::List(
            vertices
                .iter()
                .copied()
                .map(|vertex_id| vertex_to_value(store, resolved_labels, vertex_id))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        PlanBinding::Value(value) => Ok(value.clone()),
        PlanBinding::Path(pb) => Ok(path_binding_to_value(store, key, pb)),
        PlanBinding::PathGroup(paths) => Ok(Value::List(
            paths
                .iter()
                .map(|pb| path_binding_to_value(store, key, pb))
                .collect(),
        )),
    }
}

fn vertex_to_value(
    store: &GraphStore,
    resolved_labels: Option<&ResolvedLabelTable>,
    vertex_id: VertexId,
) -> Result<Value, PlanQueryError> {
    let vertex = store
        .vertex(vertex_id)
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: format!("vertex {vertex_id:?}"),
        })?;

    let labels = store.vertex_label_gql_list(vertex_id, vertex, resolved_labels);

    let properties_value = store.vertex_properties_gql_record(vertex_id);

    Ok(Value::Record(vec![
        ("id".to_owned(), Value::Uint64(u64::from(vertex_id))),
        ("labels".to_owned(), Value::List(labels)),
        ("properties".to_owned(), properties_value),
    ]))
}

fn edge_to_value(
    store: &GraphStore,
    resolved_labels: Option<&ResolvedLabelTable>,
    binding: EdgeBinding,
) -> Result<Value, PlanQueryError> {
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
            "payload".to_owned(),
            Value::Bytes(binding.inline_value_bytes_slice().to_vec()),
        ),
        (
            "label".to_owned(),
            if catalog_id.raw() == 0 {
                Value::Null
            } else {
                resolved_labels
                    .and_then(|labels| {
                        labels
                            .edge
                            .iter()
                            .find(|entry| entry.id == catalog_id)
                            .map(|entry| Value::Text(entry.name.clone()))
                    })
                    .unwrap_or_else(|| Value::Uint64(u64::from(catalog_id.raw())))
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

fn property_from_value(value: &Value, property: &str) -> Value {
    match value {
        Value::List(items)
            if !items.is_empty() && items.iter().all(|v| matches!(v, Value::Record(_))) =>
        {
            Value::List(
                items
                    .iter()
                    .map(|item| record_property(item, property))
                    .collect(),
            )
        }
        other => record_property(other, property),
    }
}

/// Compute the calendar-aware span between two matching temporal instant values.
///
/// Operands must be the same kind (`Date`/`Date`, `LocalDateTime`/`LocalDateTime`,
/// `ZonedDateTime`/`ZonedDateTime`). Time-only values and mixed types are rejected.
fn temporal_year_month_span(left: &Value, right: &Value) -> Result<i32, PlanQueryError> {
    use gleaph_gql::temporal::{date_to_jiff, datetime_to_jiff, zoned_datetime_to_jiff};
    let invalid = || PlanQueryError::InvalidExpressionValue {
        expression: "DURATION_BETWEEN operands must be matching temporal instant types".to_owned(),
    };
    let span = match (left, right) {
        (Value::Date(d1), Value::Date(d2)) => {
            let date1 = date_to_jiff(*d1).ok_or_else(invalid)?;
            let date2 = date_to_jiff(*d2).ok_or_else(invalid)?;
            date1
                .until(
                    jiff::civil::DateDifference::new(date2)
                        .largest(Unit::Year)
                        .smallest(Unit::Month),
                )
                .map_err(|e| PlanQueryError::InvalidExpressionValue {
                    expression: format!("DURATION_BETWEEN date year-month span error: {e}"),
                })?
        }
        (Value::LocalDateTime(s1, n1), Value::LocalDateTime(s2, n2)) => {
            let dt1 = datetime_to_jiff(*s1, *n1).ok_or_else(invalid)?;
            let dt2 = datetime_to_jiff(*s2, *n2).ok_or_else(invalid)?;
            dt1.until(
                jiff::civil::DateTimeDifference::new(dt2)
                    .largest(Unit::Year)
                    .smallest(Unit::Month),
            )
            .map_err(|e| PlanQueryError::InvalidExpressionValue {
                expression: format!("DURATION_BETWEEN local datetime year-month span error: {e}"),
            })?
        }
        (Value::ZonedDateTime(s1, n1, tz1), Value::ZonedDateTime(s2, n2, tz2)) => {
            // Normalize both to UTC fixed-offset so span arithmetic is unambiguous.
            let zdt1 = zoned_datetime_to_jiff(*s1, *n1, *tz1)
                .map(|zdt| {
                    let ts = zdt.timestamp();
                    zoned_datetime_to_jiff(ts.as_second(), ts.subsec_nanosecond() as u32, 0)
                        .expect("UTC zoned datetime is valid")
                })
                .ok_or_else(invalid)?;
            let zdt2 = zoned_datetime_to_jiff(*s2, *n2, *tz2)
                .map(|zdt| {
                    let ts = zdt.timestamp();
                    zoned_datetime_to_jiff(ts.as_second(), ts.subsec_nanosecond() as u32, 0)
                        .expect("UTC zoned datetime is valid")
                })
                .ok_or_else(invalid)?;
            zdt1.until(
                jiff::ZonedDifference::new(&zdt2)
                    .largest(Unit::Year)
                    .smallest(Unit::Month),
            )
            .map_err(|e| PlanQueryError::InvalidExpressionValue {
                expression: format!("DURATION_BETWEEN zoned datetime year-month span error: {e}"),
            })?
        }
        _ => return Err(invalid()),
    };
    Ok(span.get_years() as i32 * 12 + span.get_months())
}

fn temporal_nanos_between(left: &Value, right: &Value) -> Result<i64, PlanQueryError> {
    use gleaph_gql::temporal::{date_to_jiff, datetime_to_jiff, zoned_datetime_to_jiff};
    let invalid = || PlanQueryError::InvalidExpressionValue {
        expression: "DURATION_BETWEEN operands must be matching temporal instant types".to_owned(),
    };
    let duration = match (left, right) {
        (Value::Date(d1), Value::Date(d2)) => {
            let date1 = date_to_jiff(*d1).ok_or_else(invalid)?;
            let date2 = date_to_jiff(*d2).ok_or_else(invalid)?;
            date1.duration_until(date2)
        }
        (Value::LocalDateTime(s1, n1), Value::LocalDateTime(s2, n2)) => {
            let dt1 = datetime_to_jiff(*s1, *n1).ok_or_else(invalid)?;
            let dt2 = datetime_to_jiff(*s2, *n2).ok_or_else(invalid)?;
            dt1.duration_until(dt2)
        }
        (Value::ZonedDateTime(s1, n1, tz1), Value::ZonedDateTime(s2, n2, tz2)) => {
            // Normalize both to UTC fixed-offset so duration arithmetic is unambiguous.
            let zdt1 = zoned_datetime_to_jiff(*s1, *n1, *tz1)
                .map(|zdt| {
                    let ts = zdt.timestamp();
                    zoned_datetime_to_jiff(ts.as_second(), ts.subsec_nanosecond() as u32, 0)
                        .expect("UTC zoned datetime is valid")
                })
                .ok_or_else(invalid)?;
            let zdt2 = zoned_datetime_to_jiff(*s2, *n2, *tz2)
                .map(|zdt| {
                    let ts = zdt.timestamp();
                    zoned_datetime_to_jiff(ts.as_second(), ts.subsec_nanosecond() as u32, 0)
                        .expect("UTC zoned datetime is valid")
                })
                .ok_or_else(invalid)?;
            zdt1.duration_until(&zdt2)
        }
        _ => return Err(invalid()),
    };
    Ok(duration.as_secs() * 1_000_000_000 + duration.subsec_nanos() as i64)
}

fn vertex_matches_label_expr(
    store: &GraphStore,
    resolved_labels: Option<&ResolvedLabelTable>,
    vertex_id: VertexId,
    vertex: Vertex,
    expr: &LabelExpr,
) -> bool {
    match expr {
        LabelExpr::Name(name) => resolved_labels
            .and_then(|labels| {
                labels
                    .vertex
                    .iter()
                    .find(|entry| entry.name == name.as_ref())
                    .map(|entry| entry.id)
            })
            .or({
                #[cfg(any(test, feature = "canbench"))]
                {
                    Some(crate::test_labels::vertex_label_id_for_name(name.as_ref()))
                }
                #[cfg(not(any(test, feature = "canbench")))]
                {
                    None
                }
            })
            .is_some_and(|label_id| store.vertex_has_label(vertex_id, vertex, label_id)),
        LabelExpr::Wildcard => store.vertex_has_any_label(vertex_id, vertex),
        LabelExpr::And(left, right) => {
            vertex_matches_label_expr(store, resolved_labels, vertex_id, vertex, left)
                && vertex_matches_label_expr(store, resolved_labels, vertex_id, vertex, right)
        }
        LabelExpr::Or(left, right) => {
            vertex_matches_label_expr(store, resolved_labels, vertex_id, vertex, left)
                || vertex_matches_label_expr(store, resolved_labels, vertex_id, vertex, right)
        }
        LabelExpr::Not(inner) => {
            !vertex_matches_label_expr(store, resolved_labels, vertex_id, vertex, inner)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::try_read_inline_edge_property;
    use crate::plan::query::executor::eval::record_property;
    use gleaph_graph_kernel::entry::{
        Edge, EdgeInlineValue, EdgeInlineValueEncoding, EdgeInlineValueProfile, EdgeLabelId,
        EdgeSlotIndex, PropertyId,
    };
    use gleaph_graph_kernel::plan_exec::{
        ResolvedEdgeLabel, ResolvedInlineSchema, ResolvedInlineStructField, ResolvedLabelTable,
    };
    use half::f16;

    #[test]
    fn executes_planner_current_timestamp() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["CurrentTimestampProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql("MATCH (n:CurrentTimestampProbe) RETURN CURRENT_TIMESTAMP AS now");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute current timestamp");
        assert_eq!(result.rows.len(), 1);
        let now = result.rows[0].get("now").expect("now column");
        assert!(
            matches!(now, Value::DateTime(_, _)),
            "expected DateTime, got {now:?}"
        );
    }

    #[test]
    fn executes_planner_current_date() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["CurrentDateProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql("MATCH (n:CurrentDateProbe) RETURN CURRENT_DATE AS today");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute current date");
        assert_eq!(result.rows.len(), 1);
        let today = result.rows[0].get("today").expect("today column");
        assert!(
            matches!(today, Value::Date(_)),
            "expected Date, got {today:?}"
        );
    }

    #[test]
    fn executes_planner_current_time() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["CurrentTimeProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql("MATCH (n:CurrentTimeProbe) RETURN CURRENT_TIME AS t");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute current time");
        assert_eq!(result.rows.len(), 1);
        let t = result.rows[0].get("t").expect("t column");
        assert!(
            matches!(t, Value::ZonedTime(_, _)),
            "expected ZonedTime, got {t:?}"
        );
    }

    #[test]
    fn executes_planner_local_timestamp() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["LocalTimestampProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql("MATCH (n:LocalTimestampProbe) RETURN LOCAL_TIMESTAMP AS now");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute local timestamp");
        assert_eq!(result.rows.len(), 1);
        let now = result.rows[0].get("now").expect("now column");
        assert!(
            matches!(now, Value::LocalDateTime(_, _)),
            "expected LocalDateTime, got {now:?}"
        );
    }

    #[test]
    fn executes_planner_local_time() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["LocalTimeProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql("MATCH (n:LocalTimeProbe) RETURN LOCAL_TIME AS t");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute local time");
        assert_eq!(result.rows.len(), 1);
        let t = result.rows[0].get("t").expect("t column");
        assert!(
            matches!(t, Value::LocalTime(_)),
            "expected LocalTime, got {t:?}"
        );
    }

    #[test]
    fn executes_planner_date_literal() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["DateLiteralProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql("MATCH (n:DateLiteralProbe) RETURN DATE '2025-07-13' AS d");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute date literal");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("d"),
            Some(&Value::Date(
                gleaph_gql::temporal::parse_date("2025-07-13").unwrap()
            ))
        );
    }

    #[test]
    fn executes_planner_time_constructor() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["TimeConstructorProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql("MATCH (n:TimeConstructorProbe) RETURN TIME '14:30:00.123456789' AS t");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute time constructor");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("t"),
            Some(&Value::LocalTime(
                gleaph_gql::temporal::parse_time("14:30:00.123456789").unwrap()
            ))
        );
    }

    #[test]
    fn executes_planner_time_function_alias() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["TimeFunctionProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql("MATCH (n:TimeFunctionProbe) RETURN TIME('14:30:00') AS t");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute time function alias");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("t"),
            Some(&Value::LocalTime(
                gleaph_gql::temporal::parse_time("14:30:00").unwrap()
            ))
        );
    }

    #[test]
    fn executes_planner_local_time_constructor() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["LocalTimeConstructorProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan =
            plan_gql("MATCH (n:LocalTimeConstructorProbe) RETURN LOCAL_TIME('14:30:00') AS t");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute local time constructor");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("t"),
            Some(&Value::LocalTime(
                gleaph_gql::temporal::parse_local_time("14:30:00").unwrap()
            ))
        );
    }

    #[test]
    fn executes_planner_zoned_time_constructor() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["ZonedTimeConstructorProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql(
            "MATCH (n:ZonedTimeConstructorProbe) RETURN ZONED_TIME('14:30:00+09:00') AS t",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute zoned time constructor");
        assert_eq!(result.rows.len(), 1);
        let (nanos, tz) = gleaph_gql::temporal::parse_zoned_time("14:30:00+09:00").unwrap();
        assert_eq!(result.rows[0].get("t"), Some(&Value::ZonedTime(nanos, tz)));
    }

    #[test]
    fn executes_planner_datetime_literal() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["DatetimeLiteralProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan =
            plan_gql("MATCH (n:DatetimeLiteralProbe) RETURN DATETIME '2025-07-13T14:30:00' AS dt");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute datetime literal");
        assert_eq!(result.rows.len(), 1);
        let (secs, nanos) = gleaph_gql::temporal::parse_datetime("2025-07-13T14:30:00").unwrap();
        assert_eq!(
            result.rows[0].get("dt"),
            Some(&Value::DateTime(secs, nanos))
        );
    }

    #[test]
    fn executes_planner_local_datetime_constructor() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["LocalDatetimeConstructorProbe"],
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert probe vertex");
        let plan = plan_gql(
            "MATCH (n:LocalDatetimeConstructorProbe) RETURN LOCAL_DATETIME('2025-07-13T14:30:00') AS dt",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute local datetime constructor");
        assert_eq!(result.rows.len(), 1);
        let (secs, nanos) =
            gleaph_gql::temporal::parse_local_datetime("2025-07-13T14:30:00").unwrap();
        assert_eq!(
            result.rows[0].get("dt"),
            Some(&Value::LocalDateTime(secs, nanos))
        );
    }

    #[test]
    fn executes_planner_zoned_datetime_constructor() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["ZonedDatetimeConstructorProbe"],
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert probe vertex");
        let plan = plan_gql(
            "MATCH (n:ZonedDatetimeConstructorProbe) RETURN ZONED_DATETIME('2025-07-13T14:30:00+09:00') AS dt",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute zoned datetime constructor");
        assert_eq!(result.rows.len(), 1);
        let (secs, nanos, tz) =
            gleaph_gql::temporal::parse_zoned_datetime("2025-07-13T14:30:00+09:00").unwrap();
        assert_eq!(
            result.rows[0].get("dt"),
            Some(&Value::ZonedDateTime(secs, nanos, tz))
        );
    }

    #[test]
    fn executes_planner_timestamp_literal() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["TimestampLiteralProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql(
            "MATCH (n:TimestampLiteralProbe) RETURN TIMESTAMP '2025-07-13T14:30:00' AS ts",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute timestamp literal");
        assert_eq!(result.rows.len(), 1);
        let (secs, nanos) =
            gleaph_gql::temporal::parse_local_datetime("2025-07-13T14:30:00").unwrap();
        assert_eq!(
            result.rows[0].get("ts"),
            Some(&Value::LocalDateTime(secs, nanos))
        );
    }

    #[test]
    fn executes_planner_duration_constructor() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["DurationConstructorProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan =
            plan_gql("MATCH (n:DurationConstructorProbe) RETURN DURATION 'P1DT2H3M4.5S' AS dur");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute duration constructor");
        assert_eq!(result.rows.len(), 1);
        let (months, nanos) = gleaph_gql::temporal::parse_duration("P1DT2H3M4.5S").unwrap();
        assert_eq!(
            result.rows[0].get("dur"),
            Some(&Value::Duration(months, nanos))
        );
    }

    #[test]
    fn executes_planner_duration_between_dates() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["DurationBetweenProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql(
            "MATCH (n:DurationBetweenProbe) RETURN DURATION_BETWEEN(DATE '2025-01-01', DATE '2025-02-15') AS dur",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute duration between");
        assert_eq!(result.rows.len(), 1);
        // With the default (omitted) qualifier, GQL treats DURATION_BETWEEN as
        // DAY TO SECOND. 2025-01-01 -> 2025-02-15 is 45 days, so months=0 and
        // nanos holds exactly 45 days.
        assert_eq!(
            result.rows[0].get("dur"),
            Some(&Value::Duration(0, 45 * 86_400_000_000_000i64))
        );
    }

    #[test]
    fn executes_planner_duration_between_year_to_month() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["DurationBetweenYearToMonthProbe"],
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert probe vertex");
        let plan = plan_gql(
            "MATCH (n:DurationBetweenYearToMonthProbe) RETURN DURATION_BETWEEN(DATE '2024-01-01', DATE '2025-07-13') YEAR TO MONTH AS dur",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute duration between year to month");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("dur"), Some(&Value::Duration(18, 0)));
    }

    #[test]
    fn executes_planner_duration_between_day_to_second() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["DurationBetweenDayToSecondProbe"],
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert probe vertex");
        let plan = plan_gql(
            "MATCH (n:DurationBetweenDayToSecondProbe) RETURN DURATION_BETWEEN(DATE '2025-01-01', DATE '2025-01-03') DAY TO SECOND AS dur",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute duration between day to second");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("dur"),
            Some(&Value::Duration(0, 2 * 86_400_000_000_000i64))
        );
    }

    #[test]
    fn executes_planner_duration_between_calendar_month() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["DurationBetweenCalendarMonthProbe"],
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert probe vertex");
        let plan = plan_gql(
            "MATCH (n:DurationBetweenCalendarMonthProbe) RETURN DURATION_BETWEEN(DATE '2025-01-31', DATE '2025-02-28') YEAR TO MONTH AS dur",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute duration between calendar month");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("dur"), Some(&Value::Duration(1, 0)));
    }

    #[test]
    fn executes_planner_duration_between_local_datetime_day_to_second() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["DurationBetweenLocalDatetimeProbe"],
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert probe vertex");
        let plan = plan_gql(
            "MATCH (n:DurationBetweenLocalDatetimeProbe) RETURN DURATION_BETWEEN(LOCAL_DATETIME('2025-01-01T10:00:00'), LOCAL_DATETIME('2025-01-02T12:30:45.5')) AS dur",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute duration between local datetime");
        assert_eq!(result.rows.len(), 1);
        // 1 day + 2h 30m 45.5s = 95,445.5 seconds
        assert_eq!(
            result.rows[0].get("dur"),
            Some(&Value::Duration(0, 95_445_500_000_000i64,))
        );
    }

    #[test]
    fn executes_planner_duration_between_rejects_mixed_types() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["DurationBetweenMixedProbe"], Vec::<(&str, Value)>::new())
            .expect("insert probe vertex");
        let plan = plan_gql(
            "MATCH (n:DurationBetweenMixedProbe) RETURN DURATION_BETWEEN(DATE '2025-01-01', DATETIME '2025-01-02T00:00:00') AS dur",
        );
        let err = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect_err("mixed-type DURATION_BETWEEN should fail");
        assert!(
            matches!(err, PlanQueryError::InvalidExpressionValue { .. }),
            "expected InvalidExpressionValue, got {err:?}"
        );
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
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner Alice".into()))
        );
    }

    #[test]
    fn element_id_encoding_uses_per_evaluator_key_not_ambient_state() {
        // ADR 0030 P0 regression. The element-id encoding key (ADR 0019) is owned per execution by
        // `QueryExprEvaluator`, never parked in ambient thread-local state across an `await`. A
        // graph canister can host shards of different logical graphs, so concurrent messages can
        // carry different keys, and on the IC another message runs during any inter-canister
        // `await`. Here we interleave a key-B evaluation between two key-A evaluations: key A's
        // result must be unaffected, and each evaluator must encode with its own key.
        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(["KeyIsoOwner"], [("k", Value::Int64(1))])
            .expect("insert vertex");
        let parameters = params();
        let mut row = super::PlanRow::new();
        row.insert("n".to_owned(), super::PlanBinding::Vertex(vid));
        let element_id_expr = gleaph_gql::ast::Expr::new(gleaph_gql::ast::ExprKind::ElementId(
            Box::new(gleaph_gql::ast::Expr::var("n")),
        ));

        let make = |key: gleaph_graph_kernel::federation::ElementIdEncodingKey| {
            crate::plan::query::executor::context::QueryExprEvaluator {
                store: &store,
                parameters: &parameters,
                aggregate_specs: None,
                caller: None,
                resolved_labels: None,
                resolved_properties: None,
                gleaph_weight_decoders: None,
                element_id_key: key,
            }
        };
        let key_a = gleaph_graph_kernel::federation::ElementIdEncodingKey(*b"graph-a-key-0001");
        let key_b = gleaph_graph_kernel::federation::ElementIdEncodingKey(*b"graph-b-key-0002");

        let a1 = make(key_a)
            .eval_expr(&row, &element_id_expr)
            .expect("eval element id under key A");
        let b = make(key_b)
            .eval_expr(&row, &element_id_expr)
            .expect("eval element id under key B");
        let a2 = make(key_a)
            .eval_expr(&row, &element_id_expr)
            .expect("re-eval element id under key A");

        assert_ne!(a1, b, "distinct keys must produce distinct element ids");
        assert_eq!(
            a1, a2,
            "key A result must be stable across an interleaved key B evaluation"
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
    fn inline_edge_binding(payload: &[u8]) -> EdgeBinding {
        let handle = EdgeHandle {
            owner_vertex_id: VertexId::from(1u32),
            label_id: ic_stable_lara::labeled::BucketLabelKey::from_raw(7),
            slot_index: 0,
        };
        let edge = Edge {
            target: gleaph_graph_kernel::entry::VertexRef::local(VertexId::from(2u32)),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 7,
            inline_value: EdgeInlineValue::from_slice(payload),
        };
        EdgeBinding::from_edge(handle, edge)
    }

    fn resolved_label_table_with_inline(
        label_id: u16,
        property_id: u16,
        profile: EdgeInlineValueProfile,
    ) -> ResolvedLabelTable {
        ResolvedLabelTable {
            vertex: Vec::new(),
            edge: vec![ResolvedEdgeLabel::with_inline_property(
                "Road".to_string(),
                EdgeLabelId::from_raw(label_id),
                profile,
                Some(PropertyId::from_raw(u32::from(property_id))),
            )],
        }
    }

    #[test]
    fn inline_edge_property_read_decodes_f32_payload() {
        let binding = inline_edge_binding(&f32::to_le_bytes(3.5));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 4,
                encoding: EdgeInlineValueEncoding::F32,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Float32(3.5));
        // Scalar path still produces the exact value, not a single-field record.
        assert!(!matches!(value, Value::Record(_)));
    }

    #[test]
    fn inline_edge_property_read_returns_none_for_non_inline_property() {
        let binding = inline_edge_binding(&f32::to_le_bytes(3.5));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 4,
                encoding: EdgeInlineValueEncoding::F32,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(99), Some(&table))
            .expect("decode");
        assert_eq!(value, None);
    }

    #[test]
    fn inline_edge_property_read_fails_on_width_mismatch() {
        let binding = inline_edge_binding(&[0u8; 2]);
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 4,
                encoding: EdgeInlineValueEncoding::F32,
            },
        );
        let err = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect_err("width mismatch");
        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }

    fn resolved_label_table_with_inline_struct(
        label_id: u16,
        property_id: u16,
        inline_value_width: u16,
        fields: Vec<(String, u16, EdgeInlineValueProfile)>,
    ) -> ResolvedLabelTable {
        let schema = ResolvedInlineSchema::Struct {
            property_id: PropertyId::from_raw(u32::from(property_id)),
            fields: fields
                .into_iter()
                .map(|(name, offset, profile)| ResolvedInlineStructField {
                    name,
                    byte_offset: offset,
                    profile,
                })
                .collect(),
        };
        ResolvedLabelTable {
            vertex: Vec::new(),
            edge: vec![ResolvedEdgeLabel::with_inline_schema(
                "Affinity".to_string(),
                EdgeLabelId::from_raw(label_id),
                EdgeInlineValueProfile {
                    byte_width: inline_value_width,
                    encoding: EdgeInlineValueEncoding::RawBytes,
                },
                Some(schema),
            )],
        }
    }

    #[test]
    fn inline_struct_edge_property_read_decodes_record() {
        let score = 3.5f32.to_le_bytes();
        let confidence = 0.75f32.to_le_bytes();
        let updated_at = 1_700_000_000u64.to_le_bytes();
        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&score);
        payload.extend_from_slice(&confidence);
        payload.extend_from_slice(&updated_at);
        let binding = inline_edge_binding(&payload);
        let table = resolved_label_table_with_inline_struct(
            7,
            42,
            16,
            vec![
                (
                    "score".to_string(),
                    0,
                    EdgeInlineValueProfile {
                        byte_width: 4,
                        encoding: EdgeInlineValueEncoding::F32,
                    },
                ),
                (
                    "confidence".to_string(),
                    4,
                    EdgeInlineValueProfile {
                        byte_width: 4,
                        encoding: EdgeInlineValueEncoding::F32,
                    },
                ),
                (
                    "updated_at".to_string(),
                    8,
                    EdgeInlineValueProfile {
                        byte_width: 8,
                        encoding: EdgeInlineValueEncoding::RawU64,
                    },
                ),
            ],
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode struct")
            .expect("inline value");
        let Value::Record(fields) = value else {
            panic!("expected Value::Record, got {value:?}");
        };
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0], ("score".to_string(), Value::Float32(3.5)));
        assert_eq!(fields[1], ("confidence".to_string(), Value::Float32(0.75)));
        assert_eq!(
            fields[2],
            ("updated_at".to_string(), Value::Uint64(1_700_000_000))
        );
    }

    #[test]
    fn inline_struct_edge_property_read_unknown_field_becomes_null() {
        let payload = Vec::from(7.0f32.to_le_bytes());
        let binding = inline_edge_binding(&payload);
        let table = resolved_label_table_with_inline_struct(
            7,
            42,
            4,
            vec![(
                "score".to_string(),
                0,
                EdgeInlineValueProfile {
                    byte_width: 4,
                    encoding: EdgeInlineValueEncoding::F32,
                },
            )],
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode struct")
            .expect("inline value");
        let Value::Record(ref fields) = value else {
            panic!("expected Value::Record, got {value:?}");
        };
        assert_eq!(fields, &vec![("score".to_string(), Value::Float32(7.0))]);
        assert_eq!(record_property(&value, "unknown"), Value::Null);
    }

    #[test]
    fn inline_struct_edge_property_read_fails_on_malformed_projection() {
        let payload = Vec::from(7.0f32.to_le_bytes());
        let binding = inline_edge_binding(&payload);
        let table = ResolvedLabelTable {
            vertex: Vec::new(),
            edge: vec![ResolvedEdgeLabel::with_inline_schema(
                "Affinity".to_string(),
                EdgeLabelId::from_raw(7),
                EdgeInlineValueProfile {
                    byte_width: 4,
                    encoding: EdgeInlineValueEncoding::RawBytes,
                },
                Some(ResolvedInlineSchema::Struct {
                    property_id: PropertyId::from_raw(42),
                    fields: Vec::new(),
                }),
            )],
        };
        let err = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect_err("empty struct projection must fail closed");
        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }

    #[test]
    fn inline_struct_edge_property_read_fails_on_overlapping_fields() {
        let payload = [0u8; 8];
        let binding = inline_edge_binding(&payload);
        let table = resolved_label_table_with_inline_struct(
            7,
            42,
            8,
            vec![
                (
                    "a".to_string(),
                    0,
                    EdgeInlineValueProfile {
                        byte_width: 4,
                        encoding: EdgeInlineValueEncoding::F32,
                    },
                ),
                (
                    "b".to_string(),
                    2,
                    EdgeInlineValueProfile {
                        byte_width: 4,
                        encoding: EdgeInlineValueEncoding::F32,
                    },
                ),
            ],
        );
        let err = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect_err("overlapping fields must fail closed");
        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }

    #[test]
    fn inline_struct_edge_property_read_fails_on_inline_value_width_mismatch() {
        let payload = [0u8; 12];
        let binding = inline_edge_binding(&payload);
        let table = resolved_label_table_with_inline_struct(
            7,
            42,
            16,
            vec![
                (
                    "a".to_string(),
                    0,
                    EdgeInlineValueProfile {
                        byte_width: 8,
                        encoding: EdgeInlineValueEncoding::RawU64,
                    },
                ),
                (
                    "b".to_string(),
                    8,
                    EdgeInlineValueProfile {
                        byte_width: 8,
                        encoding: EdgeInlineValueEncoding::RawU64,
                    },
                ),
            ],
        );
        let err = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect_err("payload width mismatch must fail closed");
        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }

    #[test]
    fn inline_struct_edge_property_read_fails_on_duplicate_field_names() {
        let payload = [0u8; 8];
        let binding = inline_edge_binding(&payload);
        let table = resolved_label_table_with_inline_struct(
            7,
            42,
            8,
            vec![
                (
                    "a".to_string(),
                    0,
                    EdgeInlineValueProfile {
                        byte_width: 4,
                        encoding: EdgeInlineValueEncoding::F32,
                    },
                ),
                (
                    "a".to_string(),
                    4,
                    EdgeInlineValueProfile {
                        byte_width: 4,
                        encoding: EdgeInlineValueEncoding::F32,
                    },
                ),
            ],
        );
        let err = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect_err("duplicate field names must fail closed");
        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }

    #[test]
    fn inline_struct_edge_property_read_decodes_mixed_scalar_types() {
        let mut payload = Vec::with_capacity(40);
        payload.extend_from_slice(&f16::from_f32(1.5).to_le_bytes()); // 0
        payload.extend_from_slice(&2.5f32.to_le_bytes()); // 2
        payload.extend_from_slice(&3.5f64.to_le_bytes()); // 6
        payload.extend_from_slice(&u128::MAX.to_le_bytes()); // 14
        payload.extend_from_slice(&i128::MIN.to_le_bytes()); // 30
        payload.extend_from_slice(&[0xef; 32]); // 46..78 FIXED32 (placeholder; we will use 32 bytes)
        assert_eq!(payload.len(), 78);
        let binding = inline_edge_binding(&payload);
        let table = resolved_label_table_with_inline_struct(
            7,
            42,
            78,
            vec![
                (
                    "f16_col".to_string(),
                    0,
                    EdgeInlineValueProfile {
                        byte_width: 2,
                        encoding: EdgeInlineValueEncoding::F16,
                    },
                ),
                (
                    "f32_col".to_string(),
                    2,
                    EdgeInlineValueProfile {
                        byte_width: 4,
                        encoding: EdgeInlineValueEncoding::F32,
                    },
                ),
                (
                    "f64_col".to_string(),
                    6,
                    EdgeInlineValueProfile {
                        byte_width: 8,
                        encoding: EdgeInlineValueEncoding::F64,
                    },
                ),
                (
                    "u128_col".to_string(),
                    14,
                    EdgeInlineValueProfile {
                        byte_width: 16,
                        encoding: EdgeInlineValueEncoding::RawU128,
                    },
                ),
                (
                    "i128_col".to_string(),
                    30,
                    EdgeInlineValueProfile {
                        byte_width: 16,
                        encoding: EdgeInlineValueEncoding::RawI128,
                    },
                ),
                (
                    "fixed_col".to_string(),
                    46,
                    EdgeInlineValueProfile {
                        byte_width: 32,
                        encoding: EdgeInlineValueEncoding::RawFixed32,
                    },
                ),
            ],
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode mixed struct")
            .expect("inline value");
        let Value::Record(fields) = value else {
            panic!("expected Value::Record, got {value:?}");
        };
        assert_eq!(
            fields[0],
            ("f16_col".to_string(), Value::Float16(f16::from_f32(1.5)))
        );
        assert_eq!(fields[1], ("f32_col".to_string(), Value::Float32(2.5)));
        assert_eq!(fields[2], ("f64_col".to_string(), Value::Float64(3.5)));
        assert_eq!(
            fields[3],
            ("u128_col".to_string(), Value::Uint128(u128::MAX))
        );
        assert_eq!(
            fields[4],
            ("i128_col".to_string(), Value::Int128(i128::MIN))
        );
        assert_eq!(
            fields[5],
            ("fixed_col".to_string(), Value::Bytes(vec![0xef; 32]))
        );
    }

    #[test]
    fn inline_edge_property_read_preserves_f64_width() {
        let binding = inline_edge_binding(&f64::to_le_bytes(1.23456789));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 8,
                encoding: EdgeInlineValueEncoding::F64,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Float64(1.23456789));
    }

    #[test]
    fn inline_edge_property_read_preserves_signed_integer() {
        let binding = inline_edge_binding(&i64::to_le_bytes(-7));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 8,
                encoding: EdgeInlineValueEncoding::RawI64,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Int64(-7));
    }

    #[test]
    fn inline_edge_property_read_returns_none_for_unnamed_profile() {
        let binding = inline_edge_binding(&[0u8; 4]);
        let table = ResolvedLabelTable {
            vertex: Vec::new(),
            edge: vec![ResolvedEdgeLabel::new(
                "Road".to_string(),
                EdgeLabelId::from_raw(7),
                EdgeInlineValueProfile {
                    byte_width: 4,
                    encoding: EdgeInlineValueEncoding::RawBytes,
                },
            )],
        };
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode");
        assert_eq!(value, None);
    }

    #[test]
    fn inline_edge_property_read_decodes_f16_payload() {
        let binding = inline_edge_binding(&f16::from_f32(1.5).to_le_bytes());
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 2,
                encoding: EdgeInlineValueEncoding::F16,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Float16(f16::from_f32(1.5)));
    }

    #[test]
    fn inline_edge_property_read_decodes_u8_payload() {
        let binding = inline_edge_binding(&[42u8]);
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 1,
                encoding: EdgeInlineValueEncoding::RawU8,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Uint8(42));
    }

    #[test]
    fn inline_edge_property_read_decodes_u16_payload() {
        let binding = inline_edge_binding(&u16::to_le_bytes(1000));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 2,
                encoding: EdgeInlineValueEncoding::RawU16,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Uint16(1000));
    }

    #[test]
    fn inline_edge_property_read_decodes_u32_payload() {
        let binding = inline_edge_binding(&u32::to_le_bytes(123_456_789));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 4,
                encoding: EdgeInlineValueEncoding::RawU32,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Uint32(123_456_789));
    }

    #[test]
    fn inline_edge_property_read_decodes_u64_payload() {
        let binding = inline_edge_binding(&u64::to_le_bytes(u64::MAX));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 8,
                encoding: EdgeInlineValueEncoding::RawU64,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Uint64(u64::MAX));
    }

    #[test]
    fn inline_edge_property_read_decodes_u128_max() {
        let binding = inline_edge_binding(&u128::to_le_bytes(u128::MAX));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 16,
                encoding: EdgeInlineValueEncoding::RawU128,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Uint128(u128::MAX));
    }

    #[test]
    fn inline_edge_property_read_decodes_i8_min() {
        let binding = inline_edge_binding(&i8::to_le_bytes(i8::MIN));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 1,
                encoding: EdgeInlineValueEncoding::RawI8,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Int8(i8::MIN));
    }

    #[test]
    fn inline_edge_property_read_decodes_i16_min() {
        let binding = inline_edge_binding(&i16::to_le_bytes(i16::MIN));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 2,
                encoding: EdgeInlineValueEncoding::RawI16,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Int16(i16::MIN));
    }

    #[test]
    fn inline_edge_property_read_decodes_i32_min() {
        let binding = inline_edge_binding(&i32::to_le_bytes(i32::MIN));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 4,
                encoding: EdgeInlineValueEncoding::RawI32,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Int32(i32::MIN));
    }

    #[test]
    fn inline_edge_property_read_decodes_i64_min() {
        let binding = inline_edge_binding(&i64::to_le_bytes(i64::MIN));
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 8,
                encoding: EdgeInlineValueEncoding::RawI64,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Int64(i64::MIN));
    }

    #[test]
    fn inline_edge_property_read_decodes_i128_boundaries() {
        for (raw, expected) in [
            (i128::MIN, Value::Int128(i128::MIN)),
            (i128::MAX, Value::Int128(i128::MAX)),
        ] {
            let binding = inline_edge_binding(&i128::to_le_bytes(raw));
            let table = resolved_label_table_with_inline(
                7,
                42,
                EdgeInlineValueProfile {
                    byte_width: 16,
                    encoding: EdgeInlineValueEncoding::RawI128,
                },
            );
            let value =
                try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
                    .expect("decode")
                    .expect("inline value");
            assert_eq!(value, expected);
        }
    }

    #[test]
    fn inline_edge_property_read_decodes_fixed32_payload() {
        let payload = [0xabu8; 32];
        let binding = inline_edge_binding(&payload);
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 32,
                encoding: EdgeInlineValueEncoding::RawFixed32,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Bytes(payload.to_vec()));
    }

    #[test]
    fn inline_edge_property_read_decodes_fixed64_payload() {
        let payload = [0xcd; 64];
        let binding = inline_edge_binding(&payload);
        let table = resolved_label_table_with_inline(
            7,
            42,
            EdgeInlineValueProfile {
                byte_width: 64,
                encoding: EdgeInlineValueEncoding::RawFixed64,
            },
        );
        let value = try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
            .expect("decode")
            .expect("inline value");
        assert_eq!(value, Value::Bytes(payload.to_vec()));
    }

    #[test]
    fn inline_edge_property_read_rejects_width_mismatch_for_integer_encodings() {
        let cases: &[(EdgeInlineValueEncoding, u16, &[u8])] = &[
            (EdgeInlineValueEncoding::RawU8, 1, &[0u8; 2]),
            (EdgeInlineValueEncoding::RawU16, 2, &[0u8; 1]),
            (EdgeInlineValueEncoding::RawU32, 4, &[0u8; 2]),
            (EdgeInlineValueEncoding::RawU64, 8, &[0u8; 4]),
            (EdgeInlineValueEncoding::RawI8, 1, &[0u8; 2]),
            (EdgeInlineValueEncoding::RawI16, 2, &[0u8; 1]),
            (EdgeInlineValueEncoding::RawI32, 4, &[0u8; 2]),
            (EdgeInlineValueEncoding::RawI64, 8, &[0u8; 4]),
        ];
        for (encoding, width, payload) in cases {
            let binding = inline_edge_binding(payload);
            let table = resolved_label_table_with_inline(
                7,
                42,
                EdgeInlineValueProfile {
                    byte_width: *width,
                    encoding: encoding.clone(),
                },
            );
            let err =
                try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
                    .expect_err("width mismatch");
            assert!(
                matches!(err, PlanQueryError::InvalidExpressionValue { .. }),
                "encoding {encoding:?} should fail with width mismatch: {err:?}"
            );
        }
    }

    #[test]
    fn inline_edge_property_read_rejects_width_mismatch_for_128bit_encodings() {
        let cases: &[(EdgeInlineValueEncoding, &[u8])] = &[
            (EdgeInlineValueEncoding::RawU128, &[0u8; 8]),
            (EdgeInlineValueEncoding::RawI128, &[0u8; 8]),
        ];
        for (encoding, payload) in cases {
            let binding = inline_edge_binding(payload);
            let table = resolved_label_table_with_inline(
                7,
                42,
                EdgeInlineValueProfile {
                    byte_width: 16,
                    encoding: encoding.clone(),
                },
            );
            let err =
                try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
                    .expect_err("width mismatch");
            assert!(
                matches!(err, PlanQueryError::InvalidExpressionValue { .. }),
                "encoding {encoding:?} should fail with width mismatch: {err:?}"
            );
        }
    }

    #[test]
    fn inline_edge_property_read_rejects_width_mismatch_for_float_encodings() {
        let cases: &[(EdgeInlineValueEncoding, u16, &[u8])] = &[
            (EdgeInlineValueEncoding::F16, 2, &[0u8; 1]),
            (EdgeInlineValueEncoding::F32, 4, &[0u8; 2]),
            (EdgeInlineValueEncoding::F64, 8, &[0u8; 4]),
        ];
        for (encoding, width, payload) in cases {
            let binding = inline_edge_binding(payload);
            let table = resolved_label_table_with_inline(
                7,
                42,
                EdgeInlineValueProfile {
                    byte_width: *width,
                    encoding: encoding.clone(),
                },
            );
            let err =
                try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
                    .expect_err("width mismatch");
            assert!(
                matches!(err, PlanQueryError::InvalidExpressionValue { .. }),
                "encoding {encoding:?} should fail with width mismatch: {err:?}"
            );
        }
    }

    #[test]
    fn inline_edge_property_read_rejects_width_mismatch_for_fixed_encodings() {
        let cases: &[(EdgeInlineValueEncoding, u16, &[u8])] = &[
            (EdgeInlineValueEncoding::RawFixed32, 32, &[0u8; 16]),
            (EdgeInlineValueEncoding::RawFixed64, 64, &[0u8; 32]),
        ];
        for (encoding, width, payload) in cases {
            let binding = inline_edge_binding(payload);
            let table = resolved_label_table_with_inline(
                7,
                42,
                EdgeInlineValueProfile {
                    byte_width: *width,
                    encoding: encoding.clone(),
                },
            );
            let err =
                try_read_inline_edge_property(&binding, PropertyId::from_raw(42), Some(&table))
                    .expect_err("width mismatch");
            assert!(
                matches!(err, PlanQueryError::InvalidExpressionValue { .. }),
                "encoding {encoding:?} should fail with width mismatch: {err:?}"
            );
        }
    }
}
