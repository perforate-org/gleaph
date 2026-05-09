//! `PlanOp::Aggregate` execution: grouping, accumulator state, and row slots for
//! post-aggregate `Project` (`ExprKind::Aggregate` resolution).

use super::error::PlanQueryError;
use super::executor::{PlanBinding, PlanRow};
use crate::plan::expr_evaluator::eval_binary_expr;
use gleaph_gql::ast::{BinaryOp, Expr, ExprKind};
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql::Value;
use gleaph_gql_planner::plan::AggregateSpec;
use std::cmp::Ordering;

/// Row expression evaluation used while updating aggregate accumulators.
pub(crate) trait PlanRowExprEval {
    fn eval_expr_for_row(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError>;
}

/// Stable row key for a precomputed aggregate value (matches `AggregateSpec` order).
pub(crate) fn aggregate_slot_key(index: usize) -> String {
    format!("__gleaph_agg_{index}")
}

pub(crate) fn validate_aggregate_specs(specs: &[AggregateSpec]) -> Result<(), PlanQueryError> {
    for spec in specs {
        let f = spec.func.as_ref();
        match f {
            "Count" | "CountStar" => {}
            "Sum" | "Min" | "Max" => {
                if spec.expr.is_none() {
                    return Err(PlanQueryError::UnsupportedOp(if f == "Sum" {
                        "Aggregate.sum_without_expr"
                    } else if f == "Min" {
                        "Aggregate.min_without_expr"
                    } else {
                        "Aggregate.max_without_expr"
                    }));
                }
            }
            "Avg" => {
                if spec.expr.is_none() {
                    return Err(PlanQueryError::UnsupportedOp("Aggregate.avg_without_expr"));
                }
            }
            _ => return Err(PlanQueryError::UnsupportedOp("Aggregate.func")),
        }
    }
    Ok(())
}

pub(crate) fn aggregate_expr_matches_spec(expr: &Expr, spec: &AggregateSpec) -> bool {
    let ExprKind::Aggregate {
        func,
        expr: inner,
        distinct,
        expr2,
        order_by,
        filter,
        ..
    } = &expr.kind
    else {
        return false;
    };
    if expr2.is_some() || order_by.is_some() || filter.is_some() {
        return false;
    }
    let func_name = format!("{func:?}");
    if func_name != spec.func.as_ref() {
        return false;
    }
    if *distinct != spec.distinct {
        return false;
    }
    match (inner.as_deref(), spec.expr.as_ref()) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
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
        Some(_) => Err(PlanQueryError::InvalidExpressionValue {
            expression: key,
        }),
        None => Err(PlanQueryError::MissingBinding { variable: key }),
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
}

impl AggState {
    fn new(spec: &AggregateSpec) -> Result<Self, PlanQueryError> {
        Ok(match spec.func.as_ref() {
            "CountStar" => Self::CountStar { n: 0 },
            "Count" => Self::Count {
                distinct: spec.distinct,
                n: 0,
                seen: Vec::new(),
            },
            "Sum" => Self::Sum {
                distinct: spec.distinct,
                acc: None,
                seen: Vec::new(),
            },
            "Min" => Self::Min { best: None },
            "Max" => Self::Max { best: None },
            "Avg" => Self::Avg {
                distinct: spec.distinct,
                sum: None,
                count: 0,
                seen: Vec::new(),
            },
            _ => {
                return Err(PlanQueryError::UnsupportedOp("Aggregate.func"));
            }
        })
    }

    fn update(
        &mut self,
        row: &PlanRow,
        evaluator: &impl PlanRowExprEval,
        spec: &AggregateSpec,
    ) -> Result<(), PlanQueryError> {
        match self {
            Self::CountStar { n } => {
                *n += 1;
            }
            Self::Count {
                distinct,
                n,
                seen,
            } => {
                let v = evaluator.eval_expr_for_row(row, spec.expr.as_ref().expect("validated"))?;
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
                let v = evaluator.eval_expr_for_row(row, spec.expr.as_ref().expect("validated"))?;
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
                let v = evaluator.eval_expr_for_row(row, spec.expr.as_ref().expect("validated"))?;
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
                let v = evaluator.eval_expr_for_row(row, spec.expr.as_ref().expect("validated"))?;
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
                let v = evaluator.eval_expr_for_row(row, spec.expr.as_ref().expect("validated"))?;
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
        }
        Ok(())
    }

    fn finalize(self, _spec: &AggregateSpec) -> Result<Value, PlanQueryError> {
        match self {
            Self::CountStar { n } => Ok(Value::Int64(n)),
            Self::Count {
                distinct,
                n,
                seen,
            } => {
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
    use gleaph_gql::ast::AggregateFunc;
    use std::cmp::Ordering;
    use std::collections::BTreeMap;
    use std::rc::Rc;

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
                ExprKind::Paren(inner) => self.eval_expr_for_row(row, inner),
                _ => Err(PlanQueryError::UnsupportedExpression {
                    expression: format!("test row eval: {:?}", expr.kind),
                }),
            }
        }
    }

    fn spec(
        func: &'static str,
        expr: Option<Expr>,
        distinct: bool,
    ) -> AggregateSpec {
        AggregateSpec {
            func: Rc::from(func),
            expr,
            distinct,
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
        assert!(validate_aggregate_specs(&[spec("CountStar", None, false)]).is_ok());
    }

    #[test]
    fn validate_aggregate_specs_rejects_sum_without_expr() {
        let err = validate_aggregate_specs(&[spec("Sum", None, false)]).unwrap_err();
        assert!(matches!(
            err,
            PlanQueryError::UnsupportedOp("Aggregate.sum_without_expr")
        ));
    }

    #[test]
    fn validate_aggregate_specs_rejects_unknown_func() {
        let err = validate_aggregate_specs(&[spec("Collect", None, false)]).unwrap_err();
        assert!(matches!(err, PlanQueryError::UnsupportedOp("Aggregate.func")));
    }

    #[test]
    fn aggregate_expr_matches_spec_respects_func_distinct_and_inner_expr() {
        let inner = prop("n", "x");
        let spec = spec("Sum", Some(inner.clone()), true);
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
        let specs = vec![spec("CountStar", None, false)];
        let agg_expr = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::CountStar,
            expr: None,
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        });
        let mut row = PlanRow::new();
        row.insert(
            aggregate_slot_key(0),
            PlanBinding::Value(Value::Int64(7)),
        );
        let v = resolve_aggregate_from_row(&row, &agg_expr, &specs).expect("resolve");
        assert_eq!(v, Value::Int64(7));
    }

    #[test]
    fn resolve_aggregate_from_row_errors_when_slot_missing() {
        let specs = vec![spec("CountStar", None, false)];
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
            &[spec("CountStar", None, false)],
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
            &[spec("CountStar", None, false)],
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
            &[spec("CountStar", None, false)],
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
        r1.insert("dept".to_owned(), PlanBinding::Value(Value::Text("A".into())));
        let mut r2 = row_value(
            "n",
            Value::Record(vec![
                ("v".to_owned(), Value::Int64(20)),
                ("dept".to_owned(), Value::Text("B".into())),
            ]),
        );
        r2.insert("dept".to_owned(), PlanBinding::Value(Value::Text("B".into())));
        let mut r3 = row_value(
            "n",
            Value::Record(vec![
                ("v".to_owned(), Value::Int64(5)),
                ("dept".to_owned(), Value::Text("A".into())),
            ]),
        );
        r3.insert("dept".to_owned(), PlanBinding::Value(Value::Text("A".into())));
        use std::slice::from_ref;

        let out = execute_aggregate(
            vec![r1, r2, r3],
            from_ref(&dept),
            &[
                spec("CountStar", None, false),
                spec("Sum", Some(prop("n", "v")), false),
            ],
            &eval,
        )
        .expect("grouped");
        assert_eq!(out.len(), 2);
        let mut keys: Vec<_> = out
            .iter()
            .map(|row| row.get("dept").and_then(|b| match b {
                PlanBinding::Value(v) => Some(v.clone()),
                _ => None,
            }))
            .collect();
        keys.sort_by(|a, b| match (a, b) {
            (Some(va), Some(vb)) => compare_values(va, vb).unwrap_or(Ordering::Equal),
            _ => Ordering::Equal,
        });
        assert_eq!(
            keys[0].as_ref().unwrap(),
            &Value::Text("A".into())
        );
        assert_eq!(
            keys[1].as_ref().unwrap(),
            &Value::Text("B".into())
        );
        let row_a = out.iter().find(|row| {
            matches!(
                row.get("dept"),
                Some(PlanBinding::Value(Value::Text(s))) if s == "A"
            )
        }).expect("row A");
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
            &[spec("Count", Some(v_expr), true)],
            &eval,
        )
        .expect("count distinct");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].get(&aggregate_slot_key(0)),
            Some(&PlanBinding::Value(Value::Int64(2)))
        );
    }
}
