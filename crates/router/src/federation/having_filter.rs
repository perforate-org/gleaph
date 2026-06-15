//! Post-merge `HAVING` evaluation on federated aggregate rows.
//!
//! Shard-local `PlanOp::Filter` between `Aggregate` and `Project` is stripped for multi-shard
//! dispatch; merged rows are filtered here using projected column names.

use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::ast::{BinaryOp, CmpOp, Expr, ExprKind};
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::plan_exec::ExecutePlanResult;

use super::aggregate_merge::FederatedAggregateMerge;

/// Evaluate whether a merged value row satisfies a post-aggregate `HAVING` predicate.
pub fn row_passes_having(
    row: &BTreeMap<String, Value>,
    condition: &Expr,
    spec: &FederatedAggregateMerge,
    params: &BTreeMap<String, Value>,
) -> Result<bool, String> {
    let value = eval_having_expr(row, condition, spec, params)?;
    Ok(truthy(value)? == Some(true))
}

/// Apply `HAVING` to a merged federated aggregate result, updating `rows_blob` and `row_count`.
pub fn apply_federated_aggregate_having(
    result: &mut ExecutePlanResult,
    spec: &FederatedAggregateMerge,
    params: &BTreeMap<String, Value>,
) -> Result<(), String> {
    let Some(condition) = spec.having.as_ref() else {
        return Ok(());
    };
    let Some(blob) = result.rows_blob.as_ref() else {
        result.row_count = 0;
        return Ok(());
    };
    let rows = IcWirePlanQueryResult::decode_blob(blob)
        .map_err(|e| e.to_string())?
        .try_into_value_rows()
        .map_err(|e| e.to_string())?;
    let mut filtered = Vec::new();
    for row in rows {
        if row_passes_having(&row, condition, spec, params)? {
            filtered.push(row);
        }
    }
    result.row_count = filtered.len() as u64;
    result.rows_blob = if filtered.is_empty() {
        None
    } else {
        Some(
            IcWirePlanQueryResult::try_from_value_rows(&filtered)
                .map_err(|e| e.to_string())?
                .encode_blob()
                .map_err(|e| e.to_string())?,
        )
    };
    Ok(())
}

fn eval_having_expr(
    row: &BTreeMap<String, Value>,
    expr: &Expr,
    spec: &FederatedAggregateMerge,
    params: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    match &expr.kind {
        ExprKind::Paren(inner) => eval_having_expr(row, inner, spec, params),
        ExprKind::Literal(value) => Ok(value.clone()),
        ExprKind::Parameter(name) => params
            .get(name)
            .cloned()
            .ok_or_else(|| format!("missing query parameter `{name}`")),
        ExprKind::Variable(name) => row
            .get(name)
            .cloned()
            .ok_or_else(|| format!("missing HAVING column `{name}`")),
        ExprKind::Aggregate { .. } => resolve_aggregate_column(row, expr, spec),
        ExprKind::Compare { left, op, right } => {
            let left = eval_having_expr(row, left, spec, params)?;
            let right = eval_having_expr(row, right, spec, params)?;
            eval_compare(left, *op, right)
        }
        ExprKind::And(left, right) => {
            let left = eval_having_expr(row, left, spec, params)?;
            let right = eval_having_expr(row, right, spec, params)?;
            eval_and(left, right)
        }
        ExprKind::Or(left, right) => {
            let left = eval_having_expr(row, left, spec, params)?;
            let right = eval_having_expr(row, right, spec, params)?;
            eval_or(left, right)
        }
        ExprKind::Not(inner) => eval_not(eval_having_expr(row, inner, spec, params)?),
        ExprKind::IsNull(inner) => {
            let value = eval_having_expr(row, inner, spec, params)?;
            Ok(Value::Bool(value == Value::Null))
        }
        ExprKind::IsNotNull(inner) => {
            let value = eval_having_expr(row, inner, spec, params)?;
            Ok(Value::Bool(value != Value::Null))
        }
        ExprKind::BinaryOp { left, op, right } if matches!(op, BinaryOp::Add | BinaryOp::Sub) => {
            let left = eval_having_expr(row, left, spec, params)?;
            let right = eval_having_expr(row, right, spec, params)?;
            eval_numeric_binary(*op, left, right)
        }
        other => Err(format!("unsupported HAVING expression: {other:?}")),
    }
}

fn resolve_aggregate_column(
    row: &BTreeMap<String, Value>,
    expr: &Expr,
    spec: &FederatedAggregateMerge,
) -> Result<Value, String> {
    let ExprKind::Aggregate { func, .. } = &expr.kind else {
        return Err("expected aggregate expression".into());
    };
    let column = spec
        .aggregate_columns
        .iter()
        .find(|column| column.func == *func)
        .ok_or_else(|| format!("no merged column for aggregate func {func:?}"))?;
    row.get(&column.name)
        .cloned()
        .ok_or_else(|| format!("missing merged aggregate column `{}`", column.name))
}

fn truthy(value: Value) -> Result<Option<bool>, String> {
    match value {
        Value::Bool(value) => Ok(Some(value)),
        Value::Null => Ok(None),
        _ => Err(format!("expected boolean HAVING result, got {value:?}")),
    }
}

fn eval_not(value: Value) -> Result<Value, String> {
    match value {
        Value::Bool(value) => Ok(Value::Bool(!value)),
        Value::Null => Ok(Value::Null),
        other => Err(format!("expected boolean for NOT, got {other:?}")),
    }
}

fn eval_and(left: Value, right: Value) -> Result<Value, String> {
    match (truthy(left)?, truthy(right)?) {
        (Some(false), _) | (_, Some(false)) => Ok(Value::Bool(false)),
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(true), Some(true)) => Ok(Value::Bool(true)),
    }
}

fn eval_or(left: Value, right: Value) -> Result<Value, String> {
    match (truthy(left)?, truthy(right)?) {
        (Some(true), _) | (_, Some(true)) => Ok(Value::Bool(true)),
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(false), Some(false)) => Ok(Value::Bool(false)),
    }
}

fn eval_compare(left: Value, op: CmpOp, right: Value) -> Result<Value, String> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let Some(ordering) = compare_values(&left, &right) else {
        return Err(format!("incomparable HAVING values: {left:?} vs {right:?}"));
    };
    let matched = match op {
        CmpOp::Eq => ordering == std::cmp::Ordering::Equal,
        CmpOp::Ne => ordering != std::cmp::Ordering::Equal,
        CmpOp::Lt => ordering == std::cmp::Ordering::Less,
        CmpOp::Le => ordering != std::cmp::Ordering::Greater,
        CmpOp::Gt => ordering == std::cmp::Ordering::Greater,
        CmpOp::Ge => ordering != std::cmp::Ordering::Less,
    };
    Ok(Value::Bool(matched))
}

fn eval_numeric_binary(op: BinaryOp, left: Value, right: Value) -> Result<Value, String> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let left = left
        .as_f64()
        .ok_or_else(|| format!("expected numeric HAVING operand, got {left:?}"))?;
    let right = right
        .as_f64()
        .ok_or_else(|| format!("expected numeric HAVING operand, got {right:?}"))?;
    let out = match op {
        BinaryOp::Add => left + right,
        BinaryOp::Sub => left - right,
        other => return Err(format!("unsupported numeric HAVING op: {other:?}")),
    };
    Ok(if out.fract() == 0.0 {
        Value::Int64(out as i64)
    } else {
        Value::Float64(out)
    })
}

#[cfg(test)]
mod tests {
    use gleaph_gql::ast::{AggregateFunc, Expr, ExprKind};
    use gleaph_gql_ic::{IcWirePlanQueryResult, IcWireValue};

    use super::*;
    use crate::federation::aggregate_merge::AggregateMergeColumn;

    fn count_star_gt(n: i64) -> Expr {
        Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::Aggregate {
                func: AggregateFunc::CountStar,
                expr: None,
                expr2: None,
                distinct: false,
                order_by: None,
                filter: None,
            })),
            op: CmpOp::Gt,
            right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(n)))),
        })
    }

    fn spec_with_having(having: Expr) -> FederatedAggregateMerge {
        FederatedAggregateMerge {
            group_key_columns: vec!["country".into()],
            aggregate_columns: vec![AggregateMergeColumn {
                name: "cnt".into(),
                func: AggregateFunc::CountStar,
            }],
            having: Some(having),
        }
    }

    fn text_row(country: &str, cnt: i64) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("country".into(), Value::Text(country.into())),
            ("cnt".into(), Value::Int64(cnt)),
        ])
    }

    #[test]
    fn row_passes_having_count_star_gt() {
        let spec = spec_with_having(count_star_gt(1));
        assert!(
            row_passes_having(
                &text_row("US", 2),
                &count_star_gt(1),
                &spec,
                &BTreeMap::new()
            )
            .expect("pass")
        );
        assert!(
            !row_passes_having(
                &text_row("UK", 1),
                &count_star_gt(1),
                &spec,
                &BTreeMap::new()
            )
            .expect("fail")
        );
    }

    #[test]
    fn row_passes_having_return_alias_variable() {
        let having = Expr::new(ExprKind::Compare {
            left: Box::new(Expr::var("cnt")),
            op: CmpOp::Gt,
            right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(1)))),
        });
        let spec = spec_with_having(having.clone());
        assert!(
            row_passes_having(&text_row("US", 2), &having, &spec, &BTreeMap::new()).expect("pass")
        );
        assert!(
            !row_passes_having(&text_row("UK", 1), &having, &spec, &BTreeMap::new()).expect("fail")
        );
    }

    #[test]
    fn apply_federated_aggregate_having_updates_blob_and_count() {
        let spec = spec_with_having(count_star_gt(1));
        let rows_blob = IcWirePlanQueryResult {
            rows: vec![
                gleaph_gql_ic::IcWirePlanQueryRow {
                    columns: vec![
                        ("country".into(), IcWireValue::Text("US".into())),
                        ("cnt".into(), IcWireValue::Int64(2)),
                    ],
                },
                gleaph_gql_ic::IcWirePlanQueryRow {
                    columns: vec![
                        ("country".into(), IcWireValue::Text("UK".into())),
                        ("cnt".into(), IcWireValue::Int64(1)),
                    ],
                },
            ],
        }
        .encode_blob()
        .expect("encode");
        let mut result = ExecutePlanResult {
            row_count: 2,
            rows_blob: Some(rows_blob),
        };
        apply_federated_aggregate_having(&mut result, &spec, &BTreeMap::new()).expect("apply");
        assert_eq!(result.row_count, 1);
        let values = IcWirePlanQueryResult::decode_blob(result.rows_blob.as_ref().unwrap())
            .expect("decode")
            .try_into_value_rows()
            .expect("values");
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].get("country"), Some(&Value::Text("US".into())));
        assert_eq!(values[0].get("cnt"), Some(&Value::Int64(2)));
    }
}
