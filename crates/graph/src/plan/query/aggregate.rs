//! `PlanOp::Aggregate` execution: grouping, accumulator state, and row slots for
//! post-aggregate `Project` (`ExprKind::Aggregate` resolution).

use super::error::PlanQueryError;
use super::executor::{PlanBinding, PlanRow};
use super::sort_keys::compare_sort_keys;
use crate::plan::expr_evaluator::{eval_binary_expr, truthy};
use gleaph_gql::Value;
use gleaph_gql::ast::{AggregateFunc, BinaryOp, Expr, ExprKind, OrderByClause};
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_planner::plan::AggregateSpec;
use std::cmp::Ordering;

/// Row expression evaluation used while updating aggregate accumulators.
pub(crate) trait PlanRowExprEval {
    fn eval_expr_for_row(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError>;

    /// Sort-key evaluation for aggregate-local `ORDER BY` (mirrors executor sort rows).
    fn eval_sort_key_for_row(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        self.eval_expr_for_row(row, expr)
    }
}

/// Stable row key for a precomputed aggregate value (matches `AggregateSpec` order).
pub(crate) fn aggregate_slot_key(index: usize) -> String {
    format!("__gleaph_agg_{index}")
}

pub(crate) fn validate_aggregate_specs(specs: &[AggregateSpec]) -> Result<(), PlanQueryError> {
    for spec in specs {
        validate_one_aggregate_spec(spec)?;
    }
    Ok(())
}

fn validate_one_aggregate_spec(spec: &AggregateSpec) -> Result<(), PlanQueryError> {
    match spec.func {
        AggregateFunc::CountStar => {
            if spec.expr.is_some() {
                return Err(PlanQueryError::UnsupportedOp(
                    "Aggregate.count_star_with_expr",
                ));
            }
            if spec.expr2.is_some() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.expr2"));
            }
        }
        AggregateFunc::Count => {
            if spec.expr.is_none() {
                return Err(PlanQueryError::UnsupportedOp(
                    "Aggregate.count_without_expr",
                ));
            }
            if spec.expr2.is_some() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.expr2"));
            }
        }
        AggregateFunc::Sum => {
            if spec.expr.is_none() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.sum_without_expr"));
            }
            if spec.expr2.is_some() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.expr2"));
            }
        }
        AggregateFunc::Min => {
            if spec.expr.is_none() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.min_without_expr"));
            }
            if spec.expr2.is_some() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.expr2"));
            }
        }
        AggregateFunc::Max => {
            if spec.expr.is_none() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.max_without_expr"));
            }
            if spec.expr2.is_some() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.expr2"));
            }
        }
        AggregateFunc::Avg => {
            if spec.expr.is_none() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.avg_without_expr"));
            }
            if spec.expr2.is_some() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.expr2"));
            }
        }
        AggregateFunc::Collect => {
            if spec.expr.is_none() {
                return Err(PlanQueryError::UnsupportedOp(
                    "Aggregate.collect_without_expr",
                ));
            }
            if spec.expr2.is_some() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.expr2"));
            }
        }
        AggregateFunc::StddevSamp | AggregateFunc::StddevPop => {
            if spec.expr.is_none() {
                return Err(PlanQueryError::UnsupportedOp(
                    "Aggregate.stddev_without_expr",
                ));
            }
            if spec.expr2.is_some() {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.expr2"));
            }
        }
        AggregateFunc::PercentileCont | AggregateFunc::PercentileDisc => {
            if spec.expr.is_none() || spec.expr2.is_none() {
                return Err(PlanQueryError::UnsupportedOp(
                    "Aggregate.percentile_requires_two_args",
                ));
            }
        }
    }

    if spec.order_by.is_some() {
        match spec.func {
            AggregateFunc::Collect
            | AggregateFunc::PercentileCont
            | AggregateFunc::PercentileDisc => {}
            _ => {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.order_by"));
            }
        }
    }

    Ok(())
}

fn opt_box_expr_eq(opt_box: &Option<Box<Expr>>, opt: &Option<Expr>) -> bool {
    match (opt_box, opt) {
        (None, None) => true,
        (Some(a), Some(b)) => a.as_ref() == b,
        _ => false,
    }
}

pub(crate) fn aggregate_expr_matches_spec(expr: &Expr, spec: &AggregateSpec) -> bool {
    let ExprKind::Aggregate {
        func,
        expr: inner,
        distinct,
        expr2,
        order_by,
        filter,
    } = &expr.kind
    else {
        return false;
    };
    *func == spec.func
        && *distinct == spec.distinct
        && opt_box_expr_eq(inner, &spec.expr)
        && opt_box_expr_eq(expr2, &spec.expr2)
        && opt_box_expr_eq(filter, &spec.filter)
        && order_by == &spec.order_by
}

pub(crate) fn resolve_aggregate_from_row(
    row: &PlanRow,
    expr: &Expr,
    specs: &[AggregateSpec],
) -> Result<Value, PlanQueryError> {
    let idx = specs
        .iter()
        .position(|s| aggregate_expr_matches_spec(expr, s))
        .ok_or_else(|| PlanQueryError::UnsupportedExpression {
            expression: "aggregate expression does not match plan".to_owned(),
        })?;
    let key = aggregate_slot_key(idx);
    match row.get(&key) {
        Some(PlanBinding::Value(v)) => Ok(v.clone()),
        Some(_) => Err(PlanQueryError::InvalidExpressionValue { expression: key }),
        None => Err(PlanQueryError::MissingBinding { variable: key }),
    }
}

const P_FRAC_EPSILON: f64 = 1e-9;

fn passes_aggregate_filter<E: PlanRowExprEval>(
    eval: &E,
    row: &PlanRow,
    spec: &AggregateSpec,
) -> Result<bool, PlanQueryError> {
    if let Some(f) = &spec.filter {
        let v = eval.eval_expr_for_row(row, f)?;
        Ok(truthy(v).map_err(PlanQueryError::from)? == Some(true))
    } else {
        Ok(true)
    }
}

fn numeric_scalar_for_agg(v: &Value) -> Result<f64, PlanQueryError> {
    if v == &Value::Null {
        return Ok(f64::NAN);
    }
    v.as_f64()
        .ok_or_else(|| PlanQueryError::InvalidExpressionValue {
            expression: "aggregate: expected numeric value".to_owned(),
        })
}

fn percentile_fraction_from_value(v: &Value) -> Result<f64, PlanQueryError> {
    let p = numeric_scalar_for_agg(v)?;
    if !p.is_finite() {
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: "aggregate: percentile fraction must be finite".to_owned(),
        });
    }
    if p < 0.0 - P_FRAC_EPSILON || p > 1.0 + P_FRAC_EPSILON {
        return Err(PlanQueryError::InvalidExpressionValue {
            expression: "aggregate: percentile fraction must be in [0, 1]".to_owned(),
        });
    }
    Ok(p.clamp(0.0, 1.0))
}

#[derive(Clone, Debug)]
struct CollectEntry {
    value: Value,
    sort_keys: Option<Vec<Value>>,
}

#[derive(Clone, Debug)]
struct PercentileSample {
    value: Value,
    numeric: f64,
    sort_keys: Option<Vec<Value>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PercentileKind {
    Cont,
    Disc,
}

#[derive(Clone, Debug)]
struct Welford {
    count: i64,
    mean: f64,
    m2: f64,
}

impl Welford {
    fn new() -> Self {
        Self {
            count: 0,
            mean: 0.0,
            m2: 0.0,
        }
    }

    fn merge(&mut self, x: f64) {
        self.count += 1;
        let n = self.count as f64;
        let delta = x - self.mean;
        self.mean += delta / n;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;
    }
}

enum AggState {
    CountStar {
        n: i64,
    },
    Count {
        distinct: bool,
        n: i64,
        seen: Vec<Value>,
    },
    Sum {
        distinct: bool,
        acc: Option<Value>,
        seen: Vec<Value>,
    },
    Min {
        best: Option<Value>,
    },
    Max {
        best: Option<Value>,
    },
    Avg {
        distinct: bool,
        sum: Option<Value>,
        count: i64,
        seen: Vec<Value>,
    },
    Collect {
        distinct: bool,
        entries: Vec<CollectEntry>,
        order_by: Option<OrderByClause>,
        seen: Vec<Value>,
    },
    Stddev {
        sample: bool,
        distinct: bool,
        w: Welford,
        seen: Vec<Value>,
    },
    Percentile {
        kind: PercentileKind,
        distinct: bool,
        samples: Vec<PercentileSample>,
        order_by: Option<OrderByClause>,
        p: Option<f64>,
    },
}

impl AggState {
    fn new(spec: &AggregateSpec) -> Result<Self, PlanQueryError> {
        Ok(match spec.func {
            AggregateFunc::CountStar => Self::CountStar { n: 0 },
            AggregateFunc::Count => Self::Count {
                distinct: spec.distinct,
                n: 0,
                seen: Vec::new(),
            },
            AggregateFunc::Sum => Self::Sum {
                distinct: spec.distinct,
                acc: None,
                seen: Vec::new(),
            },
            AggregateFunc::Min => Self::Min { best: None },
            AggregateFunc::Max => Self::Max { best: None },
            AggregateFunc::Avg => Self::Avg {
                distinct: spec.distinct,
                sum: None,
                count: 0,
                seen: Vec::new(),
            },
            AggregateFunc::Collect => Self::Collect {
                distinct: spec.distinct,
                entries: Vec::new(),
                order_by: spec.order_by.clone(),
                seen: Vec::new(),
            },
            AggregateFunc::StddevPop => Self::Stddev {
                sample: false,
                distinct: spec.distinct,
                w: Welford::new(),
                seen: Vec::new(),
            },
            AggregateFunc::StddevSamp => Self::Stddev {
                sample: true,
                distinct: spec.distinct,
                w: Welford::new(),
                seen: Vec::new(),
            },
            AggregateFunc::PercentileCont => Self::Percentile {
                kind: PercentileKind::Cont,
                distinct: spec.distinct,
                samples: Vec::new(),
                order_by: spec.order_by.clone(),
                p: None,
            },
            AggregateFunc::PercentileDisc => Self::Percentile {
                kind: PercentileKind::Disc,
                distinct: spec.distinct,
                samples: Vec::new(),
                order_by: spec.order_by.clone(),
                p: None,
            },
        })
    }

    fn sort_keys_for_row<E: PlanRowExprEval>(
        eval: &E,
        row: &PlanRow,
        order_by: &OrderByClause,
    ) -> Result<Vec<Value>, PlanQueryError> {
        order_by
            .items
            .iter()
            .map(|item| eval.eval_sort_key_for_row(row, &item.expr))
            .collect()
    }

    fn update<E: PlanRowExprEval>(
        &mut self,
        row: &PlanRow,
        evaluator: &E,
        spec: &AggregateSpec,
    ) -> Result<(), PlanQueryError> {
        if !passes_aggregate_filter(evaluator, row, spec)? {
            return Ok(());
        }

        match self {
            Self::CountStar { n } => {
                *n += 1;
            }
            Self::Count { distinct, n, seen } => {
                let v = evaluator.eval_expr_for_row(
                    row,
                    spec.expr.as_ref().expect("validated aggregate expr"),
                )?;
                if v == Value::Null {
                    return Ok(());
                }
                if *distinct {
                    if !seen.iter().any(|x| x == &v) {
                        seen.push(v);
                    }
                } else {
                    *n += 1;
                }
            }
            Self::Sum {
                distinct,
                acc,
                seen,
            } => {
                let v = evaluator.eval_expr_for_row(
                    row,
                    spec.expr.as_ref().expect("validated aggregate expr"),
                )?;
                if v == Value::Null {
                    return Ok(());
                }
                if *distinct {
                    if seen.iter().any(|x| x == &v) {
                        return Ok(());
                    }
                    seen.push(v.clone());
                }
                match acc {
                    None => *acc = Some(v),
                    Some(cur) => {
                        *acc = Some(
                            eval_binary_expr(cur.clone(), BinaryOp::Add, v)
                                .map_err(PlanQueryError::from)?,
                        );
                    }
                }
            }
            Self::Min { best } => {
                let v = evaluator.eval_expr_for_row(
                    row,
                    spec.expr.as_ref().expect("validated aggregate expr"),
                )?;
                if v == Value::Null {
                    return Ok(());
                }
                match best {
                    None => *best = Some(v),
                    Some(cur) => {
                        if compare_values(&v, cur) == Some(Ordering::Less) {
                            *best = Some(v);
                        }
                    }
                }
            }
            Self::Max { best } => {
                let v = evaluator.eval_expr_for_row(
                    row,
                    spec.expr.as_ref().expect("validated aggregate expr"),
                )?;
                if v == Value::Null {
                    return Ok(());
                }
                match best {
                    None => *best = Some(v),
                    Some(cur) => {
                        if compare_values(&v, cur) == Some(Ordering::Greater) {
                            *best = Some(v);
                        }
                    }
                }
            }
            Self::Avg {
                distinct,
                sum,
                count,
                seen,
            } => {
                let v = evaluator.eval_expr_for_row(
                    row,
                    spec.expr.as_ref().expect("validated aggregate expr"),
                )?;
                if v == Value::Null {
                    return Ok(());
                }
                if *distinct {
                    if seen.iter().any(|x| x == &v) {
                        return Ok(());
                    }
                    seen.push(v.clone());
                }
                match sum {
                    None => {
                        *sum = Some(v.clone());
                        *count = 1;
                    }
                    Some(cur) => {
                        *sum = Some(
                            eval_binary_expr(cur.clone(), BinaryOp::Add, v)
                                .map_err(PlanQueryError::from)?,
                        );
                        *count += 1;
                    }
                }
            }
            Self::Collect {
                distinct,
                entries,
                order_by,
                seen,
            } => {
                let v = evaluator.eval_expr_for_row(
                    row,
                    spec.expr.as_ref().expect("validated aggregate expr"),
                )?;
                if *distinct {
                    if seen.iter().any(|x| x == &v) {
                        return Ok(());
                    }
                    seen.push(v.clone());
                }
                let sort_keys = if let Some(ob) = order_by {
                    Some(Self::sort_keys_for_row(evaluator, row, ob)?)
                } else {
                    None
                };
                entries.push(CollectEntry {
                    value: v,
                    sort_keys,
                });
            }
            Self::Stddev {
                sample: _,
                distinct,
                w,
                seen,
            } => {
                let v = evaluator.eval_expr_for_row(
                    row,
                    spec.expr.as_ref().expect("validated aggregate expr"),
                )?;
                if v == Value::Null {
                    return Ok(());
                }
                let x = numeric_scalar_for_agg(&v)?;
                if *distinct {
                    if seen.iter().any(|x| x == &v) {
                        return Ok(());
                    }
                    seen.push(v.clone());
                }
                w.merge(x);
            }
            Self::Percentile {
                kind: _,
                distinct,
                samples,
                order_by,
                p,
            } => {
                let v = evaluator.eval_expr_for_row(
                    row,
                    spec.expr.as_ref().expect("validated aggregate expr"),
                )?;
                if v == Value::Null {
                    return Ok(());
                }
                let num = numeric_scalar_for_agg(&v)?;
                let p_row = evaluator.eval_expr_for_row(
                    row,
                    spec.expr2.as_ref().expect("validated percentile expr2"),
                )?;
                let p_val = percentile_fraction_from_value(&p_row)?;
                match p {
                    None => *p = Some(p_val),
                    Some(existing) => {
                        if (*existing - p_val).abs() > P_FRAC_EPSILON {
                            return Err(PlanQueryError::UnsupportedExpression {
                                expression:
                                    "aggregate: percentile fraction must be constant per group"
                                        .to_owned(),
                            });
                        }
                    }
                }
                if *distinct && samples.iter().any(|s| s.value == v) {
                    return Ok(());
                }
                let sort_keys = if let Some(ob) = order_by {
                    Some(Self::sort_keys_for_row(evaluator, row, ob)?)
                } else {
                    None
                };
                samples.push(PercentileSample {
                    value: v,
                    numeric: num,
                    sort_keys,
                });
            }
        }
        Ok(())
    }

    fn finalize(self, _spec: &AggregateSpec) -> Result<Value, PlanQueryError> {
        match self {
            Self::CountStar { n } => Ok(Value::Int64(n)),
            Self::Count { distinct, n, seen } => {
                if distinct {
                    Ok(Value::Int64(i64::try_from(seen.len()).map_err(|_| {
                        PlanQueryError::ExpressionNumericOverflow {
                            expression: "COUNT(DISTINCT)".to_owned(),
                        }
                    })?))
                } else {
                    Ok(Value::Int64(n))
                }
            }
            Self::Sum { acc, .. } => Ok(acc.unwrap_or(Value::Null)),
            Self::Min { best } => Ok(best.unwrap_or(Value::Null)),
            Self::Max { best } => Ok(best.unwrap_or(Value::Null)),
            Self::Avg {
                distinct,
                sum,
                count,
                seen,
            } => {
                if distinct {
                    let len = seen.len();
                    if len == 0 {
                        return Ok(Value::Null);
                    }
                    let total = sum.expect("non-empty distinct implies sum");
                    let denom = Value::Int64(i64::try_from(len).map_err(|_| {
                        PlanQueryError::ExpressionNumericOverflow {
                            expression: "AVG(DISTINCT)".to_owned(),
                        }
                    })?);
                    eval_binary_expr(total, BinaryOp::Div, denom).map_err(PlanQueryError::from)
                } else if count == 0 {
                    Ok(Value::Null)
                } else {
                    let sum = sum.expect("count > 0 implies sum");
                    let denom = Value::Int64(count);
                    eval_binary_expr(sum, BinaryOp::Div, denom).map_err(PlanQueryError::from)
                }
            }
            Self::Collect {
                distinct: _,
                mut entries,
                order_by,
                seen: _,
            } => {
                if let Some(ob) = order_by.as_ref() {
                    for i in 0..entries.len() {
                        for j in (i + 1)..entries.len() {
                            let a = entries[i].sort_keys.as_ref().expect("order_by keys");
                            let b = entries[j].sort_keys.as_ref().expect("order_by keys");
                            compare_sort_keys(a, b, ob)?;
                        }
                    }
                    entries.sort_by(|a, b| {
                        compare_sort_keys(
                            a.sort_keys.as_ref().expect("keys"),
                            b.sort_keys.as_ref().expect("keys"),
                            ob,
                        )
                        .expect("prevalidated")
                    });
                }
                let values: Vec<Value> = entries.into_iter().map(|e| e.value).collect();
                Ok(Value::List(values))
            }
            Self::Stddev {
                sample,
                distinct: _,
                w,
                seen: _,
            } => {
                let n = w.count;
                if n == 0 {
                    return Ok(Value::Null);
                }
                if sample && n < 2 {
                    return Ok(Value::Null);
                }
                let var = if sample {
                    w.m2 / (n - 1) as f64
                } else {
                    w.m2 / n as f64
                };
                if var < 0.0 || !var.is_finite() {
                    return Ok(Value::Null);
                }
                Ok(Value::Float64(var.sqrt()))
            }
            Self::Percentile {
                kind,
                distinct: _,
                mut samples,
                order_by,
                p,
            } => {
                if samples.is_empty() {
                    return Ok(Value::Null);
                }
                let p = p.ok_or_else(|| PlanQueryError::UnsupportedExpression {
                    expression: "aggregate: percentile fraction missing".to_owned(),
                })?;
                if let Some(ob) = order_by.as_ref() {
                    for i in 0..samples.len() {
                        for j in (i + 1)..samples.len() {
                            let a = samples[i].sort_keys.as_ref().expect("order_by keys");
                            let b = samples[j].sort_keys.as_ref().expect("order_by keys");
                            compare_sort_keys(a, b, ob)?;
                        }
                    }
                    samples.sort_by(|a, b| {
                        compare_sort_keys(
                            a.sort_keys.as_ref().expect("keys"),
                            b.sort_keys.as_ref().expect("keys"),
                            ob,
                        )
                        .expect("prevalidated")
                    });
                } else {
                    samples.sort_by(|a, b| {
                        a.numeric.partial_cmp(&b.numeric).unwrap_or(Ordering::Equal)
                    });
                }
                let nums: Vec<f64> = samples.iter().map(|s| s.numeric).collect();
                let values: Vec<Value> = samples.iter().map(|s| s.value.clone()).collect();
                let n = nums.len();
                match kind {
                    PercentileKind::Disc => {
                        let mut idx = (p * n as f64).ceil() as i64 - 1;
                        idx = idx.clamp(0, n as i64 - 1);
                        Ok(values[idx as usize].clone())
                    }
                    PercentileKind::Cont => {
                        if n == 1 {
                            return Ok(Value::Float64(nums[0]));
                        }
                        let x = p * (n - 1) as f64;
                        let i = x.floor() as usize;
                        let j = x.ceil() as usize;
                        let lo = nums[i];
                        let hi = nums[j];
                        let t = x - i as f64;
                        Ok(Value::Float64(lo + t * (hi - lo)))
                    }
                }
            }
        }
    }
}

struct GroupBucket {
    key: Vec<Value>,
    rep_row: PlanRow,
    states: Vec<AggState>,
}

pub(crate) fn execute_aggregate<E: PlanRowExprEval>(
    rows: Vec<PlanRow>,
    group_by: &[Expr],
    aggregates: &[AggregateSpec],
    eval: &E,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    validate_aggregate_specs(aggregates)?;

    if group_by.is_empty() && rows.is_empty() {
        return execute_aggregate_empty_global(aggregates);
    }

    let mut groups: Vec<GroupBucket> = Vec::new();

    for row in rows {
        let key: Vec<Value> = group_by
            .iter()
            .map(|e| eval.eval_expr_for_row(&row, e))
            .collect::<Result<Vec<_>, _>>()?;

        if let Some(idx) = groups.iter().position(|g| g.key == key) {
            for (j, spec) in aggregates.iter().enumerate() {
                groups[idx].states[j].update(&row, eval, spec)?;
            }
        } else {
            let mut states: Vec<AggState> = aggregates
                .iter()
                .map(AggState::new)
                .collect::<Result<Vec<_>, _>>()?;
            for (j, spec) in aggregates.iter().enumerate() {
                states[j].update(&row, eval, spec)?;
            }
            groups.push(GroupBucket {
                key,
                rep_row: row,
                states,
            });
        }
    }

    groups
        .into_iter()
        .map(|g| finalize_aggregate_row(g, aggregates))
        .collect()
}

fn execute_aggregate_empty_global(
    aggregates: &[AggregateSpec],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    validate_aggregate_specs(aggregates)?;
    let states: Vec<AggState> = aggregates
        .iter()
        .map(AggState::new)
        .collect::<Result<Vec<_>, _>>()?;

    let mut out_row = PlanRow::new();
    for (i, (state, spec)) in states.into_iter().zip(aggregates.iter()).enumerate() {
        let v = state.finalize(spec)?;
        out_row.insert(aggregate_slot_key(i), PlanBinding::Value(v));
    }
    Ok(vec![out_row])
}

fn finalize_aggregate_row(
    bucket: GroupBucket,
    specs: &[AggregateSpec],
) -> Result<PlanRow, PlanQueryError> {
    let GroupBucket {
        mut rep_row,
        states,
        ..
    } = bucket;

    for (i, (state, spec)) in states.into_iter().zip(specs.iter()).enumerate() {
        let v = state.finalize(spec)?;
        rep_row.insert(aggregate_slot_key(i), PlanBinding::Value(v));
    }
    Ok(rep_row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::expr_evaluator::eval_compare_expr;
    use gleaph_gql::ast::{AggregateFunc, CmpOp, SortDirection};
    use gleaph_gql::token::Span;
    use std::collections::BTreeMap;

    /// Minimal row evaluator for unit tests: `Variable` and `PropertyAccess` on
    /// `Variable` where the binding is [`PlanBinding::Value`] (including `Value::Record`).
    struct TestRowEval;

    fn record_field(value: &Value, property: &str) -> Value {
        match value {
            Value::Record(fields) => fields
                .iter()
                .find(|(name, _)| name == property)
                .map(|(_, value)| value.clone())
                .unwrap_or(Value::Null),
            _ => Value::Null,
        }
    }

    impl PlanRowExprEval for TestRowEval {
        fn eval_expr_for_row(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
            match &expr.kind {
                ExprKind::Variable(name) => match row.get(name) {
                    Some(PlanBinding::Value(v)) => Ok(v.clone()),
                    Some(_) => Err(PlanQueryError::UnsupportedExpression {
                        expression: "test row eval: non-value binding".to_owned(),
                    }),
                    None => Err(PlanQueryError::MissingBinding {
                        variable: name.clone(),
                    }),
                },
                ExprKind::PropertyAccess { expr, property } => {
                    if let ExprKind::Variable(name) = &expr.kind {
                        let base = match row.get(name) {
                            Some(PlanBinding::Value(v)) => v,
                            Some(_) => {
                                return Err(PlanQueryError::UnsupportedExpression {
                                    expression: "test row eval: property base".to_owned(),
                                });
                            }
                            None => {
                                return Err(PlanQueryError::MissingBinding {
                                    variable: name.clone(),
                                });
                            }
                        };
                        Ok(record_field(base, property))
                    } else {
                        let base = self.eval_expr_for_row(row, expr)?;
                        Ok(record_field(&base, property))
                    }
                }
                ExprKind::Literal(v) => Ok(v.clone()),
                ExprKind::Compare { left, op, right } => {
                    let l = self.eval_expr_for_row(row, left)?;
                    let r = self.eval_expr_for_row(row, right)?;
                    eval_compare_expr(l, *op, r).map_err(PlanQueryError::from)
                }
                ExprKind::Paren(inner) => self.eval_expr_for_row(row, inner),
                _ => Err(PlanQueryError::UnsupportedExpression {
                    expression: format!("test row eval: {:?}", expr.kind),
                }),
            }
        }
    }

    fn spec(
        func: AggregateFunc,
        expr: Option<Expr>,
        expr2: Option<Expr>,
        distinct: bool,
        filter: Option<Expr>,
        order_by: Option<OrderByClause>,
    ) -> AggregateSpec {
        AggregateSpec {
            func,
            expr,
            expr2,
            distinct,
            filter,
            order_by,
            alias: None,
        }
    }

    fn prop(var: &str, field: &str) -> Expr {
        Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::new(ExprKind::Variable(var.to_owned()))),
            property: field.to_owned(),
        })
    }

    fn row_value(name: &str, v: Value) -> PlanRow {
        BTreeMap::from([(name.to_owned(), PlanBinding::Value(v))])
    }

    #[test]
    fn aggregate_slot_key_is_stable() {
        assert_eq!(aggregate_slot_key(0), "__gleaph_agg_0");
        assert_eq!(aggregate_slot_key(2), "__gleaph_agg_2");
    }

    #[test]
    fn validate_aggregate_specs_accepts_count_star_without_expr() {
        assert!(
            validate_aggregate_specs(&[spec(
                AggregateFunc::CountStar,
                None,
                None,
                false,
                None,
                None
            )])
            .is_ok()
        );
    }

    #[test]
    fn validate_aggregate_specs_rejects_sum_without_expr() {
        let err =
            validate_aggregate_specs(&[spec(AggregateFunc::Sum, None, None, false, None, None)])
                .unwrap_err();
        assert!(matches!(
            err,
            PlanQueryError::UnsupportedOp("Aggregate.sum_without_expr")
        ));
    }

    #[test]
    fn validate_aggregate_specs_rejects_unknown_order_by_on_sum() {
        let ob = OrderByClause {
            span: Span::DUMMY,
            items: vec![gleaph_gql::ast::SortItem {
                span: Span::DUMMY,
                expr: Expr::new(ExprKind::Variable("x".into())),
                direction: None,
                null_order: None,
            }],
        };
        let err = validate_aggregate_specs(&[spec(
            AggregateFunc::Sum,
            Some(prop("n", "v")),
            None,
            false,
            None,
            Some(ob),
        )])
        .unwrap_err();
        assert!(matches!(
            err,
            PlanQueryError::UnsupportedOp("Aggregate.order_by")
        ));
    }

    #[test]
    fn aggregate_expr_matches_spec_respects_fields() {
        let inner = prop("n", "x");
        let spec = spec(
            AggregateFunc::Sum,
            Some(inner.clone()),
            None,
            true,
            None,
            None,
        );
        let matching = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Sum,
            expr: Some(Box::new(inner.clone())),
            expr2: None,
            distinct: true,
            order_by: None,
            filter: None,
        });
        assert!(aggregate_expr_matches_spec(&matching, &spec));

        let wrong_distinct = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Sum,
            expr: Some(Box::new(inner.clone())),
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        });
        assert!(!aggregate_expr_matches_spec(&wrong_distinct, &spec));
    }

    #[test]
    fn resolve_aggregate_from_row_reads_slot() {
        let specs = vec![spec(
            AggregateFunc::CountStar,
            None,
            None,
            false,
            None,
            None,
        )];
        let agg_expr = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::CountStar,
            expr: None,
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        });
        let mut row = PlanRow::new();
        row.insert(aggregate_slot_key(0), PlanBinding::Value(Value::Int64(7)));
        let v = resolve_aggregate_from_row(&row, &agg_expr, &specs).expect("resolve");
        assert_eq!(v, Value::Int64(7));
    }

    #[test]
    fn resolve_aggregate_from_row_errors_when_slot_missing() {
        let specs = vec![spec(
            AggregateFunc::CountStar,
            None,
            None,
            false,
            None,
            None,
        )];
        let agg_expr = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::CountStar,
            expr: None,
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        });
        let row = PlanRow::new();
        let err = resolve_aggregate_from_row(&row, &agg_expr, &specs).unwrap_err();
        assert!(matches!(err, PlanQueryError::MissingBinding { .. }));
    }

    #[test]
    fn execute_aggregate_global_count_star_over_rows() {
        let eval = TestRowEval;
        let rows = vec![row_value("n", Value::Null), row_value("n", Value::Null)];
        let out = execute_aggregate(
            rows,
            &[],
            &[spec(
                AggregateFunc::CountStar,
                None,
                None,
                false,
                None,
                None,
            )],
            &eval,
        )
        .expect("agg");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].get(&aggregate_slot_key(0)),
            Some(&PlanBinding::Value(Value::Int64(2)))
        );
    }

    #[test]
    fn execute_aggregate_empty_global_emits_one_row_with_zero_count_star() {
        let eval = TestRowEval;
        let out = execute_aggregate(
            vec![],
            &[],
            &[spec(
                AggregateFunc::CountStar,
                None,
                None,
                false,
                None,
                None,
            )],
            &eval,
        )
        .expect("empty global");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].get(&aggregate_slot_key(0)),
            Some(&PlanBinding::Value(Value::Int64(0)))
        );
    }

    #[test]
    fn execute_aggregate_grouped_empty_input_yields_no_groups() {
        let eval = TestRowEval;
        let g = Expr::new(ExprKind::Variable("g".to_owned()));
        let out = execute_aggregate(
            vec![],
            &[g],
            &[spec(
                AggregateFunc::CountStar,
                None,
                None,
                false,
                None,
                None,
            )],
            &eval,
        )
        .expect("no rows");
        assert!(out.is_empty());
    }

    #[test]
    fn execute_aggregate_groups_by_variable_and_sums_property() {
        let eval = TestRowEval;
        let dept = Expr::new(ExprKind::Variable("dept".to_owned()));
        let mut r1 = row_value(
            "n",
            Value::Record(vec![
                ("v".to_owned(), Value::Int64(10)),
                ("dept".to_owned(), Value::Text("A".into())),
            ]),
        );
        r1.insert(
            "dept".to_owned(),
            PlanBinding::Value(Value::Text("A".into())),
        );
        let mut r2 = row_value(
            "n",
            Value::Record(vec![
                ("v".to_owned(), Value::Int64(20)),
                ("dept".to_owned(), Value::Text("B".into())),
            ]),
        );
        r2.insert(
            "dept".to_owned(),
            PlanBinding::Value(Value::Text("B".into())),
        );
        let mut r3 = row_value(
            "n",
            Value::Record(vec![
                ("v".to_owned(), Value::Int64(5)),
                ("dept".to_owned(), Value::Text("A".into())),
            ]),
        );
        r3.insert(
            "dept".to_owned(),
            PlanBinding::Value(Value::Text("A".into())),
        );
        use std::slice::from_ref;

        let out = execute_aggregate(
            vec![r1, r2, r3],
            from_ref(&dept),
            &[
                spec(AggregateFunc::CountStar, None, None, false, None, None),
                spec(
                    AggregateFunc::Sum,
                    Some(prop("n", "v")),
                    None,
                    false,
                    None,
                    None,
                ),
            ],
            &eval,
        )
        .expect("grouped");
        assert_eq!(out.len(), 2);
        let mut keys: Vec<_> = out
            .iter()
            .map(|row| {
                row.get("dept").and_then(|b| match b {
                    PlanBinding::Value(v) => Some(v.clone()),
                    _ => None,
                })
            })
            .collect();
        keys.sort_by(|a, b| match (a, b) {
            (Some(va), Some(vb)) => compare_values(va, vb).unwrap_or(Ordering::Equal),
            _ => Ordering::Equal,
        });
        assert_eq!(keys[0].as_ref().unwrap(), &Value::Text("A".into()));
        assert_eq!(keys[1].as_ref().unwrap(), &Value::Text("B".into()));
        let row_a = out
            .iter()
            .find(|row| {
                matches!(
                    row.get("dept"),
                    Some(PlanBinding::Value(Value::Text(s))) if s == "A"
                )
            })
            .expect("row A");
        assert_eq!(
            row_a.get(&aggregate_slot_key(0)),
            Some(&PlanBinding::Value(Value::Int64(2)))
        );
        assert_eq!(
            row_a.get(&aggregate_slot_key(1)),
            Some(&PlanBinding::Value(Value::Int64(15)))
        );
        let row_b = out
            .iter()
            .find(|row| {
                matches!(
                    row.get("dept"),
                    Some(PlanBinding::Value(Value::Text(s))) if s == "B"
                )
            })
            .expect("row B");
        assert_eq!(
            row_b.get(&aggregate_slot_key(0)),
            Some(&PlanBinding::Value(Value::Int64(1)))
        );
        assert_eq!(
            row_b.get(&aggregate_slot_key(1)),
            Some(&PlanBinding::Value(Value::Int64(20)))
        );
    }

    #[test]
    fn execute_aggregate_count_distinct_on_property() {
        let eval = TestRowEval;
        let v_expr = prop("n", "k");
        let r1 = row_value("n", Value::Record(vec![("k".to_owned(), Value::Int64(1))]));
        let r2 = row_value("n", Value::Record(vec![("k".to_owned(), Value::Int64(1))]));
        let r3 = row_value("n", Value::Record(vec![("k".to_owned(), Value::Int64(2))]));
        let out = execute_aggregate(
            vec![r1, r2, r3],
            &[],
            &[spec(
                AggregateFunc::Count,
                Some(v_expr),
                None,
                true,
                None,
                None,
            )],
            &eval,
        )
        .expect("count distinct");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].get(&aggregate_slot_key(0)),
            Some(&PlanBinding::Value(Value::Int64(2)))
        );
    }

    #[test]
    fn collect_includes_null_and_respects_order_by() {
        let eval = TestRowEval;
        let ob = OrderByClause {
            span: Span::DUMMY,
            items: vec![gleaph_gql::ast::SortItem {
                span: Span::DUMMY,
                expr: Expr::new(ExprKind::Variable("ord".into())),
                direction: Some(SortDirection::Asc),
                null_order: None,
            }],
        };
        let mut a = row_value("v", Value::Int64(2));
        a.insert("ord".into(), PlanBinding::Value(Value::Int64(2)));
        let mut b = row_value("v", Value::Null);
        b.insert("ord".into(), PlanBinding::Value(Value::Int64(1)));
        let out = execute_aggregate(
            vec![a, b],
            &[],
            &[spec(
                AggregateFunc::Collect,
                Some(Expr::new(ExprKind::Variable("v".into()))),
                None,
                false,
                None,
                Some(ob),
            )],
            &eval,
        )
        .expect("collect");
        match out[0].get(&aggregate_slot_key(0)).unwrap() {
            PlanBinding::Value(Value::List(items)) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Value::Null);
                assert_eq!(items[1], Value::Int64(2));
            }
            other => panic!("expected list: {other:?}"),
        }
    }

    #[test]
    fn count_star_with_filter_skips_rows() {
        let eval = TestRowEval;
        let filter = Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::Variable("flag".into()))),
            op: CmpOp::Eq,
            right: Box::new(Expr::new(ExprKind::Literal(Value::Bool(true)))),
        });
        let mut r1 = row_value("flag", Value::Bool(false));
        let mut r2 = row_value("flag", Value::Bool(true));
        r1.insert("x".into(), PlanBinding::Value(Value::Int64(1)));
        r2.insert("x".into(), PlanBinding::Value(Value::Int64(2)));
        let out = execute_aggregate(
            vec![r1, r2],
            &[],
            &[spec(
                AggregateFunc::CountStar,
                None,
                None,
                false,
                Some(filter),
                None,
            )],
            &eval,
        )
        .expect("filtered count");
        assert_eq!(
            out[0].get(&aggregate_slot_key(0)),
            Some(&PlanBinding::Value(Value::Int64(1)))
        );
    }

    #[test]
    fn stddev_pop_two_values() {
        let eval = TestRowEval;
        let out = execute_aggregate(
            vec![
                row_value("x", Value::Int64(10)),
                row_value("x", Value::Int64(30)),
            ],
            &[],
            &[spec(
                AggregateFunc::StddevPop,
                Some(Expr::new(ExprKind::Variable("x".into()))),
                None,
                false,
                None,
                None,
            )],
            &eval,
        )
        .expect("stddev");
        let v = out[0].get(&aggregate_slot_key(0)).unwrap();
        if let PlanBinding::Value(Value::Float64(f)) = v {
            assert!((f - 10.0).abs() < 1e-9);
        } else {
            panic!("expected float: {v:?}");
        }
    }

    #[test]
    fn percentile_cont_median() {
        let eval = TestRowEval;
        let p = Expr::new(ExprKind::Literal(Value::Float64(0.5)));
        let out = execute_aggregate(
            vec![
                row_value("v", Value::Int64(10)),
                row_value("v", Value::Int64(30)),
            ],
            &[],
            &[spec(
                AggregateFunc::PercentileCont,
                Some(Expr::new(ExprKind::Variable("v".into()))),
                Some(p),
                false,
                None,
                None,
            )],
            &eval,
        )
        .expect("pct");
        let v = out[0].get(&aggregate_slot_key(0)).unwrap();
        if let PlanBinding::Value(Value::Float64(f)) = v {
            assert!((f - 20.0).abs() < 1e-9);
        } else {
            panic!("expected float: {v:?}");
        }
    }
}
