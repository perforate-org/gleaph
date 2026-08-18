//! Router-side row merge for multi-graph `USE GRAPH` (wire blobs).

use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::ast::ExprKind;
use gleaph_gql::hash_value_for_join;
use gleaph_gql::value::cmp::compare_values;
use gleaph_gql_ic::{GqlWireRow, GqlWireRows, GqlWireValue};
use gleaph_gql_planner::plan::{PlanOp, ProjectColumn, Str};
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

use crate::state::RouterError;

fn row_column_map(row: &GqlWireRow) -> BTreeMap<&str, &GqlWireValue> {
    row.columns.iter().map(|(k, v)| (k.as_str(), v)).collect()
}

fn wire_value_to_gql(value: &GqlWireValue) -> Result<Value, RouterError> {
    value
        .try_into_value()
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))
}

fn join_key_values(row: &GqlWireRow, join_keys: &[Str]) -> Result<Vec<Value>, RouterError> {
    let cols = row_column_map(row);
    join_keys
        .iter()
        .map(|key| {
            cols.get(key.as_ref())
                .ok_or_else(|| {
                    RouterError::InvalidArgument(format!(
                        "join key column `{}` missing from wire row",
                        key.as_ref()
                    ))
                })
                .and_then(|wire| wire_value_to_gql(wire))
        })
        .collect()
}

fn hash_join_key(values: &[Value]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for value in values {
        hash_value_for_join(value, &mut hasher);
    }
    hasher.finish()
}

fn merge_wire_rows_skip_keys(
    left: &GqlWireRow,
    right: &GqlWireRow,
    skip_keys: &[Str],
) -> Result<GqlWireRow, RouterError> {
    let mut columns = left.columns.clone();
    for (key, value) in &right.columns {
        if skip_keys.iter().any(|skip| skip.as_ref() == key) {
            continue;
        }
        if columns.iter().any(|(existing, _)| existing == key) {
            return Err(RouterError::InvalidArgument(format!(
                "multi-graph row merge column conflict: {key}"
            )));
        }
        columns.push((key.clone(), value.clone()));
    }
    Ok(GqlWireRow { columns })
}

fn merge_wire_rows(left: &GqlWireRow, right: &GqlWireRow) -> Result<GqlWireRow, RouterError> {
    merge_wire_rows_skip_keys(left, right, &[])
}

pub fn cartesian_merge_wire_results(
    left: &GqlWireRows,
    right: &GqlWireRows,
) -> Result<GqlWireRows, RouterError> {
    let mut rows = Vec::new();
    for left_row in &left.rows {
        for right_row in &right.rows {
            rows.push(merge_wire_rows(left_row, right_row)?);
        }
    }
    Ok(GqlWireRows { rows })
}

pub fn hash_join_wire_results(
    left: &GqlWireRows,
    right: &GqlWireRows,
    join_keys: &[Str],
) -> Result<GqlWireRows, RouterError> {
    if join_keys.is_empty() {
        return Err(RouterError::InvalidArgument(
            "HashJoin(empty join_keys) on router".into(),
        ));
    }

    let mut right_buckets: BTreeMap<u64, Vec<GqlWireRow>> = BTreeMap::new();
    for row in &right.rows {
        let key_values = join_key_values(row, join_keys)?;
        right_buckets
            .entry(hash_join_key(&key_values))
            .or_default()
            .push(row.clone());
    }

    let mut rows = Vec::new();
    'outer: for left_row in &left.rows {
        let left_key = join_key_values(left_row, join_keys)?;
        let left_hash = hash_join_key(&left_key);
        let Some(candidates) = right_buckets.get(&left_hash) else {
            continue;
        };
        for right_row in candidates {
            let right_key = join_key_values(right_row, join_keys)?;
            if join_keys
                .iter()
                .zip(left_key.iter().zip(right_key.iter()))
                .any(|(_name, (lv, rv))| compare_values(lv, rv) != Some(std::cmp::Ordering::Equal))
            {
                continue;
            }
            rows.push(merge_wire_rows_skip_keys(left_row, right_row, join_keys)?);
            continue 'outer;
        }
    }
    Ok(GqlWireRows { rows })
}

pub fn apply_project_wire(
    input: &GqlWireRows,
    columns: &[ProjectColumn],
) -> Result<GqlWireRows, RouterError> {
    let mut rows = Vec::with_capacity(input.rows.len());
    for row in &input.rows {
        let cols = row_column_map(row);
        let mut projected = Vec::with_capacity(columns.len());
        for column in columns {
            let ExprKind::Variable(name) = &column.expr.kind else {
                return Err(RouterError::InvalidArgument(
                    "multi-graph tail Project supports variables only".into(),
                ));
            };
            let wire = cols.get(name.as_str()).ok_or_else(|| {
                RouterError::InvalidArgument(format!(
                    "Project variable `{name}` missing from merged wire row"
                ))
            })?;
            let alias = column
                .alias
                .as_ref()
                .map(|a| a.to_string())
                .unwrap_or_else(|| name.to_string());
            projected.push((alias, (*wire).clone()));
        }
        rows.push(GqlWireRow { columns: projected });
    }
    Ok(GqlWireRows { rows })
}

pub fn apply_tail_ops_wire(
    input: &GqlWireRows,
    tail_ops: &[PlanOp],
) -> Result<GqlWireRows, RouterError> {
    let mut current = input.clone();
    for op in tail_ops {
        current = match op {
            PlanOp::Project { columns, .. } => apply_project_wire(&current, columns)?,
            other => {
                return Err(RouterError::InvalidArgument(format!(
                    "unsupported multi-graph tail op: {other:?}"
                )));
            }
        };
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(col: &str, value: i64) -> GqlWireRow {
        GqlWireRow {
            columns: vec![(col.into(), GqlWireValue::Int64(value))],
        }
    }

    #[test]
    fn cartesian_merge_wire_results_multiplies_rows() {
        let left = GqlWireRows {
            rows: vec![row("a", 1), row("a", 2)],
        };
        let right = GqlWireRows {
            rows: vec![row("b", 10)],
        };
        let merged = cartesian_merge_wire_results(&left, &right).expect("merge");
        assert_eq!(merged.rows.len(), 2);
    }

    #[test]
    fn hash_join_wire_results_matches_on_key() {
        let left = GqlWireRows {
            rows: vec![GqlWireRow {
                columns: vec![
                    ("id".into(), GqlWireValue::Int64(1)),
                    ("a".into(), GqlWireValue::Int64(11)),
                ],
            }],
        };
        let right = GqlWireRows {
            rows: vec![GqlWireRow {
                columns: vec![
                    ("id".into(), GqlWireValue::Int64(1)),
                    ("b".into(), GqlWireValue::Int64(22)),
                ],
            }],
        };
        let merged =
            hash_join_wire_results(&left, &right, &[gleaph_gql_planner::plan::Str::from("id")])
                .expect("join");
        assert_eq!(merged.rows.len(), 1);
        assert_eq!(merged.rows[0].columns.len(), 3);
    }
}
