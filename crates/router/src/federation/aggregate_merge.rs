//! Cross-shard aggregate merge for federated `PlanOp::Aggregate` queries.
//!
//! When each graph shard returns partial aggregate rows, the router merges by GROUP BY key and
//! re-applies commutative aggregate functions (COUNT/COUNT(*)/SUM/MIN/MAX). Non-mergeable
//! aggregates fall back to union row merge.

use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::ast::{AggregateFunc, ExprKind};
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_ic::{IcWirePlanQueryResult, IcWireValue};
use gleaph_gql_planner::plan::{AggregateSpec, PlanOp, ProjectColumn};

/// How partial shard results should be merged on the router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FederatedMergeMode {
    /// Independent fragments: concatenate row batches and sum per-shard row counts.
    UnionRows,
    /// Partial aggregates: group by key columns and merge metric columns.
    Aggregate(FederatedAggregateMerge),
}

/// Column layout for merging partial aggregate rows across shards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FederatedAggregateMerge {
    pub group_key_columns: Vec<String>,
    pub aggregate_columns: Vec<AggregateMergeColumn>,
}

/// One aggregate metric column in the post-aggregate `Project` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregateMergeColumn {
    pub name: String,
    pub func: AggregateFunc,
}

/// Derive merge mode from physical plans (last plan with a mergeable aggregate wins).
pub fn federated_merge_mode_from_plans(
    plans: &[gleaph_gql_planner::plan::PhysicalPlan],
) -> FederatedMergeMode {
    plans
        .iter()
        .rev()
        .find_map(|plan| federated_merge_mode_from_ops(&plan.ops))
        .unwrap_or(FederatedMergeMode::UnionRows)
}

/// Derive merge mode from a single plan's operator list.
pub fn federated_merge_mode_from_ops(ops: &[PlanOp]) -> Option<FederatedMergeMode> {
    for (idx, op) in ops.iter().enumerate() {
        let PlanOp::Aggregate {
            group_by,
            aggregates,
        } = op
        else {
            continue;
        };
        if !aggregates_are_mergeable(aggregates) {
            return None;
        }
        let project_columns = aggregate_sink_project_columns(ops, idx)?;
        let spec = build_aggregate_merge_spec(group_by, project_columns)?;
        return Some(FederatedMergeMode::Aggregate(spec));
    }
    None
}

fn aggregates_are_mergeable(aggregates: &[AggregateSpec]) -> bool {
    aggregates.iter().all(|spec| {
        spec.distinct == false
            && spec.filter.is_none()
            && spec.order_by.is_none()
            && spec.expr2.is_none()
            && matches!(
                spec.func,
                AggregateFunc::CountStar
                    | AggregateFunc::Count
                    | AggregateFunc::Sum
                    | AggregateFunc::Min
                    | AggregateFunc::Max
            )
    })
}

fn aggregate_sink_project_columns(
    ops: &[PlanOp],
    aggregate_idx: usize,
) -> Option<&[ProjectColumn]> {
    for op in ops.get(aggregate_idx + 1..)? {
        match op {
            PlanOp::Filter { .. } => continue,
            PlanOp::Project { columns, .. } => return Some(columns.as_slice()),
            _ => break,
        }
    }
    None
}

fn build_aggregate_merge_spec(
    group_by: &[gleaph_gql::ast::Expr],
    project_columns: &[ProjectColumn],
) -> Option<FederatedAggregateMerge> {
    let mut group_key_columns = Vec::new();
    let mut aggregate_columns = Vec::new();

    for column in project_columns {
        let name = project_column_name(column)?;
        if let ExprKind::Aggregate { func, .. } = &column.expr.kind {
            aggregate_columns.push(AggregateMergeColumn { name, func: *func });
        } else if !group_by.is_empty() {
            group_key_columns.push(name);
        }
    }

    if aggregate_columns.is_empty() {
        return None;
    }
    Some(FederatedAggregateMerge {
        group_key_columns,
        aggregate_columns,
    })
}

fn project_column_name(column: &ProjectColumn) -> Option<String> {
    column
        .alias
        .as_ref()
        .map(|alias| alias.to_string())
        .or_else(|| match &column.expr.kind {
            ExprKind::Variable(var) => Some(var.clone()),
            _ => None,
        })
}

/// Merge two optional row batches using aggregate semantics.
pub fn merge_optional_aggregate_blobs(
    acc: Option<Vec<u8>>,
    next: Option<Vec<u8>>,
    spec: &FederatedAggregateMerge,
) -> Result<Option<Vec<u8>>, String> {
    match (acc, next) {
        (None, None) => Ok(None),
        (Some(blob), None) | (None, Some(blob)) => Ok(Some(blob)),
        (Some(left), Some(right)) => Ok(Some(merge_aggregate_blobs(&left, &right, spec)?)),
    }
}

/// Merge two row batches by GROUP BY key and aggregate metric columns.
pub fn merge_aggregate_blobs(
    left: &[u8],
    right: &[u8],
    spec: &FederatedAggregateMerge,
) -> Result<Vec<u8>, String> {
    let left_rows = IcWirePlanQueryResult::decode_blob(left)
        .map_err(|e| e.to_string())?
        .try_into_value_rows()
        .map_err(|e| e.to_string())?;
    let right_rows = IcWirePlanQueryResult::decode_blob(right)
        .map_err(|e| e.to_string())?
        .try_into_value_rows()
        .map_err(|e| e.to_string())?;
    let merged = merge_aggregate_value_rows(&left_rows, &right_rows, spec)?;
    IcWirePlanQueryResult::try_from_value_rows(&merged)
        .map_err(|e| e.to_string())?
        .encode_blob()
        .map_err(|e| e.to_string())
}

fn merge_aggregate_value_rows(
    left: &[BTreeMap<String, Value>],
    right: &[BTreeMap<String, Value>],
    spec: &FederatedAggregateMerge,
) -> Result<Vec<BTreeMap<String, Value>>, String> {
    let mut groups: BTreeMap<Vec<u8>, MergedAggregateGroup> = BTreeMap::new();
    for row in left.iter().chain(right.iter()) {
        ingest_aggregate_row(&mut groups, row, spec)?;
    }
    let mut out: Vec<BTreeMap<String, Value>> = groups
        .into_values()
        .map(|group| group.into_output_row(spec))
        .collect();
    out.sort_by(|left, right| compare_output_rows(left, right, spec));
    Ok(out)
}

#[derive(Clone, Debug)]
struct MergedAggregateGroup {
    group_values: Vec<Value>,
    metrics: Vec<Option<Value>>,
}

impl MergedAggregateGroup {
    fn new(spec: &FederatedAggregateMerge, row: &BTreeMap<String, Value>) -> Result<Self, String> {
        let group_values = spec
            .group_key_columns
            .iter()
            .map(|column| {
                row.get(column)
                    .cloned()
                    .ok_or_else(|| format!("missing group key column `{column}`"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            group_values,
            metrics: vec![None; spec.aggregate_columns.len()],
        })
    }

    fn into_output_row(self, spec: &FederatedAggregateMerge) -> BTreeMap<String, Value> {
        let mut row = BTreeMap::new();
        for (column, value) in spec.group_key_columns.iter().zip(self.group_values) {
            row.insert(column.clone(), value);
        }
        for (column, value) in spec.aggregate_columns.iter().zip(self.metrics) {
            row.insert(column.name.clone(), value.unwrap_or(Value::Null));
        }
        row
    }
}

fn ingest_aggregate_row(
    groups: &mut BTreeMap<Vec<u8>, MergedAggregateGroup>,
    row: &BTreeMap<String, Value>,
    spec: &FederatedAggregateMerge,
) -> Result<(), String> {
    let key = encode_group_key(row, spec)?;
    if !groups.contains_key(&key) {
        groups.insert(key.clone(), MergedAggregateGroup::new(spec, row)?);
    }
    let entry = groups.get_mut(&key).expect("group key inserted above");
    for (idx, column) in spec.aggregate_columns.iter().enumerate() {
        let value = row
            .get(&column.name)
            .ok_or_else(|| format!("missing aggregate column `{}`", column.name))?;
        entry.metrics[idx] = Some(merge_metric_value(
            entry.metrics[idx].as_ref(),
            value,
            column.func,
        )?);
    }
    Ok(())
}

fn encode_group_key(
    row: &BTreeMap<String, Value>,
    spec: &FederatedAggregateMerge,
) -> Result<Vec<u8>, String> {
    let key_values = spec
        .group_key_columns
        .iter()
        .map(|column| {
            row.get(column)
                .cloned()
                .ok_or_else(|| format!("missing group key column `{column}`"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let wire_values = key_values
        .iter()
        .map(IcWireValue::try_from_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    candid::encode_one(wire_values).map_err(|e| e.to_string())
}

fn merge_metric_value(
    existing: Option<&Value>,
    incoming: &Value,
    func: AggregateFunc,
) -> Result<Value, String> {
    match func {
        AggregateFunc::CountStar | AggregateFunc::Count | AggregateFunc::Sum => {
            merge_add_values(existing, incoming)
        }
        AggregateFunc::Min => merge_extreme_value(existing, incoming, true),
        AggregateFunc::Max => merge_extreme_value(existing, incoming, false),
        other => Err(format!(
            "unsupported federated aggregate merge func: {other:?}"
        )),
    }
}

fn merge_add_values(existing: Option<&Value>, incoming: &Value) -> Result<Value, String> {
    if incoming == &Value::Null {
        return existing
            .cloned()
            .ok_or_else(|| "aggregate merge on null".to_string());
    }
    let incoming_numeric = incoming
        .as_f64()
        .ok_or_else(|| format!("expected numeric aggregate value, got {incoming:?}"))?;
    match existing {
        None => Ok(clone_numeric_as_value(incoming, incoming_numeric)),
        Some(left) if left == &Value::Null => {
            Ok(clone_numeric_as_value(incoming, incoming_numeric))
        }
        Some(left) => {
            let left_numeric = left
                .as_f64()
                .ok_or_else(|| format!("expected numeric aggregate value, got {left:?}"))?;
            let sum = left_numeric + incoming_numeric;
            Ok(clone_numeric_as_value(left, sum))
        }
    }
}

fn clone_numeric_as_value(template: &Value, numeric: f64) -> Value {
    if numeric.fract() == 0.0 && template.is_integer_like() {
        Value::Int64(numeric as i64)
    } else {
        Value::Float64(numeric)
    }
}

fn merge_extreme_value(
    existing: Option<&Value>,
    incoming: &Value,
    pick_min: bool,
) -> Result<Value, String> {
    if incoming == &Value::Null {
        return existing
            .cloned()
            .ok_or_else(|| "aggregate merge on null".to_string());
    }
    let Some(left) = existing else {
        return Ok(incoming.clone());
    };
    if left == &Value::Null {
        return Ok(incoming.clone());
    }
    let ord = compare_values(left, incoming)
        .ok_or_else(|| format!("incomparable aggregate values: {left:?} vs {incoming:?}"))?;
    let pick_incoming = if pick_min {
        ord == std::cmp::Ordering::Greater
    } else {
        ord == std::cmp::Ordering::Less
    };
    Ok(if pick_incoming {
        incoming.clone()
    } else {
        left.clone()
    })
}

fn compare_output_rows(
    left: &BTreeMap<String, Value>,
    right: &BTreeMap<String, Value>,
    spec: &FederatedAggregateMerge,
) -> std::cmp::Ordering {
    for column in &spec.group_key_columns {
        let l = left.get(column);
        let r = right.get(column);
        match (l, r) {
            (Some(lv), Some(rv)) => {
                if let Some(ord) = compare_values(lv, rv)
                    && ord != std::cmp::Ordering::Equal
                {
                    return ord;
                }
            }
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (None, None) => {}
        }
    }
    std::cmp::Ordering::Equal
}

trait IntegerLikeValue {
    fn is_integer_like(&self) -> bool;
}

impl IntegerLikeValue for Value {
    fn is_integer_like(&self) -> bool {
        matches!(
            self,
            Value::Int8(_)
                | Value::Int16(_)
                | Value::Int32(_)
                | Value::Int64(_)
                | Value::Int128(_)
                | Value::Uint8(_)
                | Value::Uint16(_)
                | Value::Uint32(_)
                | Value::Uint64(_)
                | Value::Uint128(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use gleaph_gql::ast::{AggregateFunc, Expr, ExprKind};
    use gleaph_gql_planner::plan::{AggregateSpec, PhysicalPlan, PlanOp, ProjectColumn, Str};

    use super::*;

    fn agg_count_star() -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::CountStar,
            expr: None,
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    fn project_agg(expr: Expr, alias: &str) -> ProjectColumn {
        ProjectColumn {
            expr,
            alias: Some(Str::from(alias)),
        }
    }

    fn sample_aggregate_plan(group_by: Vec<Expr>) -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::Aggregate {
                group_by: group_by.clone(),
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::CountStar,
                    expr: None,
                    expr2: None,
                    distinct: false,
                    filter: None,
                    order_by: None,
                    alias: None,
                }],
            },
            PlanOp::Project {
                columns: if group_by.is_empty() {
                    vec![project_agg(agg_count_star(), "cnt")]
                } else {
                    vec![
                        ProjectColumn {
                            expr: group_by[0].clone(),
                            alias: Some(Str::from("country")),
                        },
                        project_agg(agg_count_star(), "cnt"),
                    ]
                },
                distinct: false,
            },
        ])
    }

    fn rows_blob(rows: Vec<BTreeMap<String, Value>>) -> Vec<u8> {
        IcWirePlanQueryResult::try_from_value_rows(&rows)
            .expect("wire rows")
            .encode_blob()
            .expect("encode")
    }

    fn int_row(columns: &[(&str, i64)]) -> BTreeMap<String, Value> {
        columns
            .iter()
            .map(|(k, v)| (k.to_string(), Value::Int64(*v)))
            .collect()
    }

    fn text_row(columns: &[(&str, &str)], metrics: &[(&str, i64)]) -> BTreeMap<String, Value> {
        let mut row = BTreeMap::new();
        for (k, v) in columns {
            row.insert(k.to_string(), Value::Text((*v).into()));
        }
        for (k, v) in metrics {
            row.insert(k.to_string(), Value::Int64(*v));
        }
        row
    }

    #[test]
    fn federated_merge_mode_detects_global_count_star() {
        let mode = federated_merge_mode_from_plans(&[sample_aggregate_plan(vec![])]);
        assert_eq!(
            mode,
            FederatedMergeMode::Aggregate(FederatedAggregateMerge {
                group_key_columns: vec![],
                aggregate_columns: vec![AggregateMergeColumn {
                    name: "cnt".into(),
                    func: AggregateFunc::CountStar,
                }],
            })
        );
    }

    #[test]
    fn federated_merge_mode_detects_grouped_count_star() {
        let country = Expr::var("n");
        let mode = federated_merge_mode_from_plans(&[sample_aggregate_plan(vec![country])]);
        assert_eq!(
            mode,
            FederatedMergeMode::Aggregate(FederatedAggregateMerge {
                group_key_columns: vec!["country".into()],
                aggregate_columns: vec![AggregateMergeColumn {
                    name: "cnt".into(),
                    func: AggregateFunc::CountStar,
                }],
            })
        );
    }

    #[test]
    fn merge_aggregate_blobs_sums_global_count_star() {
        let spec = FederatedAggregateMerge {
            group_key_columns: vec![],
            aggregate_columns: vec![AggregateMergeColumn {
                name: "cnt".into(),
                func: AggregateFunc::CountStar,
            }],
        };
        let left = rows_blob(vec![int_row(&[("cnt", 5)])]);
        let right = rows_blob(vec![int_row(&[("cnt", 3)])]);
        let merged = merge_aggregate_blobs(&left, &right, &spec).expect("merge");
        let decoded = IcWirePlanQueryResult::decode_blob(&merged)
            .expect("decode")
            .try_into_value_rows()
            .expect("values");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].get("cnt"), Some(&Value::Int64(8)));
    }

    #[test]
    fn merge_aggregate_blobs_merges_grouped_count_star() {
        let spec = FederatedAggregateMerge {
            group_key_columns: vec!["country".into()],
            aggregate_columns: vec![AggregateMergeColumn {
                name: "cnt".into(),
                func: AggregateFunc::CountStar,
            }],
        };
        let left = rows_blob(vec![
            text_row(&[("country", "US")], &[("cnt", 2)]),
            text_row(&[("country", "UK")], &[("cnt", 1)]),
        ]);
        let right = rows_blob(vec![text_row(&[("country", "US")], &[("cnt", 1)])]);
        let merged = merge_aggregate_blobs(&left, &right, &spec).expect("merge");
        let decoded = IcWirePlanQueryResult::decode_blob(&merged)
            .expect("decode")
            .try_into_value_rows()
            .expect("values");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].get("country"), Some(&Value::Text("UK".into())));
        assert_eq!(decoded[0].get("cnt"), Some(&Value::Int64(1)));
        assert_eq!(decoded[1].get("country"), Some(&Value::Text("US".into())));
        assert_eq!(decoded[1].get("cnt"), Some(&Value::Int64(3)));
    }

    #[test]
    fn merge_aggregate_blobs_merges_min_and_max() {
        let spec = FederatedAggregateMerge {
            group_key_columns: vec![],
            aggregate_columns: vec![
                AggregateMergeColumn {
                    name: "min_v".into(),
                    func: AggregateFunc::Min,
                },
                AggregateMergeColumn {
                    name: "max_v".into(),
                    func: AggregateFunc::Max,
                },
            ],
        };
        let left = rows_blob(vec![int_row(&[("min_v", 4), ("max_v", 9)])]);
        let right = rows_blob(vec![int_row(&[("min_v", 2), ("max_v", 11)])]);
        let merged = merge_aggregate_blobs(&left, &right, &spec).expect("merge");
        let decoded = IcWirePlanQueryResult::decode_blob(&merged)
            .expect("decode")
            .try_into_value_rows()
            .expect("values");
        assert_eq!(decoded[0].get("min_v"), Some(&Value::Int64(2)));
        assert_eq!(decoded[0].get("max_v"), Some(&Value::Int64(11)));
    }

    #[test]
    fn avg_aggregate_falls_back_to_union_mode() {
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::Avg,
                    expr: Some(Expr::var("n")),
                    expr2: None,
                    distinct: false,
                    filter: None,
                    order_by: None,
                    alias: None,
                }],
            },
            PlanOp::Project {
                columns: vec![project_agg(
                    Expr::new(ExprKind::Aggregate {
                        func: AggregateFunc::Avg,
                        expr: Some(Box::new(Expr::var("n"))),
                        expr2: None,
                        distinct: false,
                        order_by: None,
                        filter: None,
                    }),
                    "avg_v",
                )],
                distinct: false,
            },
        ]);
        assert_eq!(
            federated_merge_mode_from_plans(&[plan]),
            FederatedMergeMode::UnionRows
        );
    }
}
