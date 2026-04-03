use std::cmp::Ordering;
use std::rc::Rc;

use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_planner::plan::AggregateSpec;
use gleaph_graph_kernel::GraphRead;

use crate::{
    BindingRow, BindingValue, ExecutionContext, ExecutionError, ExecutionResultExt, eval_expr,
};

#[derive(Clone, Debug)]
struct GroupBucket {
    key: Vec<Value>,
    sample_row: BindingRow,
    aggregate_states: Vec<AggregateState>,
}

impl GroupBucket {
    fn new(key: Vec<Value>, sample_row: BindingRow, aggregate_len: usize) -> Self {
        Self {
            key,
            sample_row,
            aggregate_states: vec![AggregateState::default(); aggregate_len],
        }
    }
}

#[derive(Clone, Debug, Default)]
struct AggregateState {
    count: i64,
    distinct_values: Vec<Value>,
    sum: Option<Value>,
    extremum: Option<Value>,
    avg: Option<(Value, i64)>,
}

pub(crate) fn exec_aggregate<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    group_by: &[Expr],
    aggregates: &[AggregateSpec],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut groups: Vec<GroupBucket> = Vec::new();

    for row in input {
        let key: Vec<Value> = group_by
            .iter()
            .map(|expr| eval_expr(graph, &row, expr, ctx))
            .collect::<Result<_, _>>()?;

        let idx = groups
            .iter()
            .position(|bucket| bucket.key == key)
            .unwrap_or_else(|| {
                groups.push(GroupBucket::new(key.clone(), row.clone(), aggregates.len()));
                groups.len() - 1
            });

        let bucket = &mut groups[idx];
        for (agg_idx, spec) in aggregates.iter().enumerate() {
            update_aggregate_state(
                graph,
                &row,
                spec,
                &mut bucket.aggregate_states[agg_idx],
                ctx,
            )?;
        }
    }

    let mut out = Vec::with_capacity(groups.len());
    for mut bucket in groups {
        for (agg_idx, spec) in aggregates.iter().enumerate() {
            let key = aggregate_binding_name(spec, agg_idx);
            bucket.sample_row.insert(
                Rc::<str>::from(key),
                BindingValue::Scalar(finalize_aggregate_state(&bucket.aggregate_states[agg_idx])),
            );
        }
        out.push(bucket.sample_row);
    }
    Ok(out)
}

pub(crate) fn aggregate_binding_name(spec: &AggregateSpec, index: usize) -> String {
    if let Some(alias) = &spec.alias {
        alias.as_ref().to_owned()
    } else {
        aggregate_expr_binding_name_from_spec(spec, index)
    }
}

fn aggregate_expr_binding_name_from_spec(spec: &AggregateSpec, index: usize) -> String {
    let suffix = match &spec.expr {
        Some(expr) => format!(":{expr:?}"),
        None => ":*".to_owned(),
    };
    format!("__agg_{index}_{}{}", spec.func, suffix)
}

pub(crate) fn aggregate_expr_binding_name(
    func: &gleaph_gql::ast::AggregateFunc,
    expr: Option<&Expr>,
    index: usize,
) -> String {
    let suffix = match expr {
        Some(expr) => format!(":{expr:?}"),
        None => ":*".to_owned(),
    };
    format!("__agg_{index}_{func:?}{suffix}")
}

fn update_aggregate_state<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    spec: &AggregateSpec,
    state: &mut AggregateState,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<()> {
    let func = spec.func.as_ref();
    match func {
        "Count" | "CountStar" => {
            let value = match &spec.expr {
                Some(expr) => eval_expr(graph, row, expr, ctx)?,
                None => Value::Int64(1),
            };
            if spec.expr.is_some() && matches!(value, Value::Null) {
                return Ok(());
            }
            if spec.distinct {
                if state.distinct_values.contains(&value) {
                    return Ok(());
                }
                state.distinct_values.push(value);
            }
            state.count += 1;
            Ok(())
        }
        "Sum" => {
            let value = match &spec.expr {
                Some(expr) => eval_expr(graph, row, expr, ctx)?,
                None => {
                    return Err(ExecutionError::UnsupportedPlanOp(
                        "Aggregate.sum_without_expr",
                    ));
                }
            };
            if matches!(value, Value::Null) {
                return Ok(());
            }
            if spec.distinct {
                if state.distinct_values.contains(&value) {
                    return Ok(());
                }
                state.distinct_values.push(value.clone());
            }
            accumulate_sum(&mut state.sum, value)?;
            Ok(())
        }
        "Avg" => {
            let value = match &spec.expr {
                Some(expr) => eval_expr(graph, row, expr, ctx)?,
                None => {
                    return Err(ExecutionError::UnsupportedPlanOp(
                        "Aggregate.avg_without_expr",
                    ));
                }
            };
            if matches!(value, Value::Null) {
                return Ok(());
            }
            if spec.distinct {
                if state.distinct_values.contains(&value) {
                    return Ok(());
                }
                state.distinct_values.push(value.clone());
            }
            match &mut state.avg {
                None => state.avg = Some((value, 1)),
                Some((acc, c)) => {
                    *acc = sum_values(acc.clone(), value)?;
                    *c += 1;
                }
            }
            Ok(())
        }
        "Min" => {
            let value = match &spec.expr {
                Some(expr) => eval_expr(graph, row, expr, ctx)?,
                None => {
                    return Err(ExecutionError::UnsupportedPlanOp(
                        "Aggregate.min_without_expr",
                    ));
                }
            };
            if matches!(value, Value::Null) {
                return Ok(());
            }
            if spec.distinct {
                if state.distinct_values.contains(&value) {
                    return Ok(());
                }
                state.distinct_values.push(value.clone());
            }
            update_extremum(&mut state.extremum, value, true);
            Ok(())
        }
        "Max" => {
            let value = match &spec.expr {
                Some(expr) => eval_expr(graph, row, expr, ctx)?,
                None => {
                    return Err(ExecutionError::UnsupportedPlanOp(
                        "Aggregate.max_without_expr",
                    ));
                }
            };
            if matches!(value, Value::Null) {
                return Ok(());
            }
            if spec.distinct {
                if state.distinct_values.contains(&value) {
                    return Ok(());
                }
                state.distinct_values.push(value.clone());
            }
            update_extremum(&mut state.extremum, value, false);
            Ok(())
        }
        _ => Err(ExecutionError::UnsupportedPlanOp("Aggregate.func")),
    }
}

fn finalize_aggregate_state(state: &AggregateState) -> Value {
    if let Some((sum, c)) = &state.avg {
        if *c == 0 {
            return Value::Null;
        }
        return avg_finalize_sum(sum.clone(), *c);
    }
    if let Some(value) = &state.sum {
        return value.clone();
    }
    if let Some(value) = &state.extremum {
        return value.clone();
    }
    Value::Int64(state.count)
}

fn avg_finalize_sum(sum: Value, count: i64) -> Value {
    let c = count as f64;
    match sum {
        Value::Int8(v) => Value::Float64(f64::from(v) / c),
        Value::Int16(v) => Value::Float64(f64::from(v) / c),
        Value::Int32(v) => Value::Float64(f64::from(v) / c),
        Value::Int64(v) => Value::Float64(v as f64 / c),
        Value::Int128(v) => Value::Float64((v as f64) / c),
        Value::Uint8(v) => Value::Float64(f64::from(v) / c),
        Value::Uint16(v) => Value::Float64(f64::from(v) / c),
        Value::Uint32(v) => Value::Float64(f64::from(v) / c),
        Value::Uint64(v) => Value::Float64(v as f64 / c),
        Value::Uint128(v) => Value::Float64((v as f64) / c),
        Value::Float32(v) => Value::Float64(f64::from(v) / c),
        Value::Float64(v) => Value::Float64(v / c),
        other => other,
    }
}

fn accumulate_sum(current: &mut Option<Value>, value: Value) -> ExecutionResultExt<()> {
    match current.take() {
        None => {
            *current = Some(value);
            Ok(())
        }
        Some(acc) => {
            *current = Some(sum_values(acc, value)?);
            Ok(())
        }
    }
}

fn sum_values(left: Value, right: Value) -> ExecutionResultExt<Value> {
    match (left, right) {
        (Value::Int8(a), Value::Int8(b)) => Ok(Value::Int8(a.saturating_add(b))),
        (Value::Int16(a), Value::Int16(b)) => Ok(Value::Int16(a.saturating_add(b))),
        (Value::Int32(a), Value::Int32(b)) => Ok(Value::Int32(a.saturating_add(b))),
        (Value::Int64(a), Value::Int64(b)) => Ok(Value::Int64(a.saturating_add(b))),
        (Value::Int128(a), Value::Int128(b)) => Ok(Value::Int128(a.saturating_add(b))),
        (Value::Uint8(a), Value::Uint8(b)) => Ok(Value::Uint8(a.saturating_add(b))),
        (Value::Uint16(a), Value::Uint16(b)) => Ok(Value::Uint16(a.saturating_add(b))),
        (Value::Uint32(a), Value::Uint32(b)) => Ok(Value::Uint32(a.saturating_add(b))),
        (Value::Uint64(a), Value::Uint64(b)) => Ok(Value::Uint64(a.saturating_add(b))),
        (Value::Uint128(a), Value::Uint128(b)) => Ok(Value::Uint128(a.saturating_add(b))),
        _ => Err(ExecutionError::UnsupportedPlanOp(
            "Aggregate.sum_value_type",
        )),
    }
}

fn update_extremum(slot: &mut Option<Value>, candidate: Value, is_min: bool) {
    match slot {
        None => *slot = Some(candidate),
        Some(current) => {
            let ord = compare_values(&candidate, current).unwrap_or(Ordering::Equal);
            let replace = if is_min {
                ord == Ordering::Less
            } else {
                ord == Ordering::Greater
            };
            if replace {
                *current = candidate;
            }
        }
    }
}
