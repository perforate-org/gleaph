//! §14.2 composite set operations (`UNION`, `EXCEPT`, `INTERSECT`, `OTHERWISE`).

use std::collections::BTreeMap;

use gleaph_gql::ast::SetOp;
use gleaph_gql_planner::plan::PhysicalPlan;

use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::PlanBinding;
use super::context::ExecuteCtx;
use super::ops::execute_ops_from;

type RowKey = BTreeMap<String, PlanBinding>;

pub(crate) async fn execute_set_operation(
    ctx: &ExecuteCtx<'_>,
    left: Vec<PlanRow>,
    op: SetOp,
    right: &PhysicalPlan,
    right_input: Vec<PlanRow>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    if matches!(op, SetOp::Otherwise) {
        if left.is_empty() {
            return execute_ops_from(ctx, &right.ops, right_input).await;
        }
        return Ok(left);
    }
    let right_rows = execute_ops_from(ctx, &right.ops, right_input).await?;
    Ok(apply_set_op(op, left, right_rows))
}

fn apply_set_op(op: SetOp, left: Vec<PlanRow>, right: Vec<PlanRow>) -> Vec<PlanRow> {
    match op {
        SetOp::UnionAll => {
            let mut out = left;
            out.extend(right);
            out
        }
        SetOp::Union | SetOp::UnionDistinct => {
            let mut out = left;
            out.extend(right);
            dedup_set_rows(&mut out);
            out
        }
        SetOp::ExceptAll => except_all(left, right),
        SetOp::Except | SetOp::ExceptDistinct => {
            let right_keys = row_keys(&right);
            let mut out: Vec<_> = left
                .into_iter()
                .filter(|row| !contains_row_key(&right_keys, row))
                .collect();
            dedup_set_rows(&mut out);
            out
        }
        SetOp::IntersectAll => intersect_all(left, right),
        SetOp::Intersect | SetOp::IntersectDistinct => {
            let mut out = Vec::new();
            let right_keys = row_keys(&right);
            let mut seen = Vec::<RowKey>::new();
            for row in left {
                let key = row_key(&row);
                if right_keys.iter().any(|r| r == &key) && !seen.iter().any(|r| r == &key) {
                    seen.push(key);
                    out.push(row);
                }
            }
            out
        }
        SetOp::Otherwise => unreachable!("handled in execute_set_operation"),
    }
}

fn row_key(row: &PlanRow) -> RowKey {
    row.iter()
        .map(|(name, binding)| (name.to_string(), binding.clone()))
        .collect()
}

fn row_keys(rows: &[PlanRow]) -> Vec<RowKey> {
    rows.iter().map(row_key).collect()
}

fn contains_row_key(keys: &[RowKey], row: &PlanRow) -> bool {
    let key = row_key(row);
    keys.iter().any(|k| k == &key)
}

fn dedup_set_rows(rows: &mut Vec<PlanRow>) {
    let mut seen = Vec::<RowKey>::new();
    let mut unique = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        let key = row_key(&row);
        if !seen.iter().any(|k| k == &key) {
            seen.push(key);
            unique.push(row);
        }
    }
    *rows = unique;
}

fn count_rows(rows: &[PlanRow]) -> Vec<(RowKey, usize)> {
    let mut counts = Vec::new();
    for row in rows {
        let key = row_key(row);
        if let Some((_, count)) = counts.iter_mut().find(|(r, _)| r == &key) {
            *count += 1;
        } else {
            counts.push((key, 1));
        }
    }
    counts
}

fn count_for_row(counts: &[(RowKey, usize)], row: &PlanRow) -> usize {
    let key = row_key(row);
    counts
        .iter()
        .find(|(r, _)| r == &key)
        .map(|(_, c)| *c)
        .unwrap_or(0)
}

/// Multiset EXCEPT ALL: emit each left row unless matched by a right row (one-for-one).
fn except_all(left: Vec<PlanRow>, right: Vec<PlanRow>) -> Vec<PlanRow> {
    let mut right_counts = count_rows(&right);
    let mut out = Vec::new();
    for row in left {
        let key = row_key(&row);
        if let Some((_, count)) = right_counts.iter_mut().find(|(r, _)| r == &key) {
            if *count > 0 {
                *count -= 1;
                continue;
            }
        }
        out.push(row);
    }
    out
}

/// Multiset INTERSECT ALL: multiplicity = min(left, right) per row; order follows left.
fn intersect_all(left: Vec<PlanRow>, right: Vec<PlanRow>) -> Vec<PlanRow> {
    let left_counts = count_rows(&left);
    let right_counts = count_rows(&right);
    let mut min_counts = Vec::new();
    for (row, lc) in &left_counts {
        let rc = count_for_key(&right_counts, row);
        if rc > 0 {
            min_counts.push((row.clone(), (*lc).min(rc)));
        }
    }
    let mut emitted = Vec::<(RowKey, usize)>::new();
    let mut out = Vec::new();
    for row in left {
        let max = count_for_row(&min_counts, &row);
        if max == 0 {
            continue;
        }
        let key = row_key(&row);
        let emitted_so_far = match emitted.iter_mut().find(|(r, _)| r == &key) {
            Some((_, count)) => {
                *count += 1;
                *count
            }
            None => {
                emitted.push((key, 1));
                1
            }
        };
        if emitted_so_far <= max {
            out.push(row);
        }
    }
    out
}

fn count_for_key(counts: &[(RowKey, usize)], key: &RowKey) -> usize {
    counts
        .iter()
        .find(|(r, _)| r == key)
        .map(|(_, c)| *c)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;
    use gleaph_gql::ast::{Expr, ExprKind};
    use gleaph_gql_planner::plan::{PlanOp, ProjectColumn, Str};
    use gleaph_gql_planner::{BindingLayout, PhysicalPlan};
    use std::rc::Rc;

    use super::super::PlanBinding;
    use super::super::test_support::params;
    use crate::facade::GraphStore;
    use crate::gql_execution_context::GqlExecutionContext;

    fn scalar_row(name: &str, value: i64) -> PlanRow {
        let mut row = PlanRow::new();
        row.insert(name.to_owned(), PlanBinding::Value(Value::Int64(value)));
        row
    }

    fn indexed_scalar_row(name: &str, value: i64) -> PlanRow {
        PlanRow::with_layout_and_binding(
            Rc::new(BindingLayout::single(name.into())),
            name,
            PlanBinding::Value(Value::Int64(value)),
        )
    }

    fn project_var_as_plan(variable: &str, alias: &str) -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![PlanOp::Project {
            columns: vec![ProjectColumn {
                expr: Expr::new(ExprKind::Variable(variable.to_owned())),
                alias: Some(Str::from(alias)),
            }],
            distinct: false,
        }])
    }

    #[test]
    fn union_all_preserves_duplicates() {
        let left = vec![scalar_row("x", 1), scalar_row("x", 1)];
        let right = vec![scalar_row("x", 2)];
        let out = apply_set_op(SetOp::UnionAll, left, right);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn union_distinct_dedups() {
        let left = vec![scalar_row("x", 1), scalar_row("x", 1)];
        let right = vec![scalar_row("x", 1)];
        let out = apply_set_op(SetOp::UnionDistinct, left, right);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn union_distinct_matches_rows_with_different_storage() {
        let left = vec![indexed_scalar_row("x", 1)];
        let right = vec![scalar_row("x", 1)];
        let out = apply_set_op(SetOp::UnionDistinct, left, right);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn except_distinct_dedups_left_rows() {
        let left = vec![scalar_row("x", 1), scalar_row("x", 1), scalar_row("x", 2)];
        let right = Vec::new();
        let out = apply_set_op(SetOp::ExceptDistinct, left, right);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], scalar_row("x", 1));
        assert_eq!(out[1], scalar_row("x", 2));
    }

    #[test]
    fn except_all_multiset() {
        let left = vec![scalar_row("x", 1), scalar_row("x", 1), scalar_row("x", 2)];
        let right = vec![scalar_row("x", 1)];
        let out = apply_set_op(SetOp::ExceptAll, left, right);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], scalar_row("x", 1));
        assert_eq!(out[1], scalar_row("x", 2));
    }

    #[test]
    fn intersect_all_multiplicity() {
        let left = vec![scalar_row("x", 1), scalar_row("x", 1), scalar_row("x", 2)];
        let right = vec![scalar_row("x", 1), scalar_row("x", 2), scalar_row("x", 2)];
        let out = apply_set_op(SetOp::IntersectAll, left, right);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], scalar_row("x", 1));
        assert_eq!(out[1], scalar_row("x", 2));
    }

    #[test]
    fn otherwise_non_empty_left_skips_right_branch() {
        let store = GraphStore::new();
        let parameters = params();
        let ctx = ExecuteCtx::new(
            &store,
            &parameters,
            None,
            GqlExecutionContext::default(),
            None,
        );
        let right = project_var_as_plan("missing", "x");
        let left = vec![scalar_row("x", 1)];
        let right_input = vec![PlanRow::new()];

        let out = pollster::block_on(execute_set_operation(
            &ctx,
            left.clone(),
            SetOp::Otherwise,
            &right,
            right_input,
        ))
        .expect("otherwise should not evaluate the right branch when left is non-empty");

        assert_eq!(out, left);
    }

    #[test]
    fn otherwise_empty_left_executes_right_with_input() {
        let store = GraphStore::new();
        let parameters = params();
        let ctx = ExecuteCtx::new(
            &store,
            &parameters,
            None,
            GqlExecutionContext::default(),
            None,
        );
        let right = project_var_as_plan("n", "x");
        let right_input = vec![scalar_row("n", 42)];

        let out = pollster::block_on(execute_set_operation(
            &ctx,
            Vec::new(),
            SetOp::Otherwise,
            &right,
            right_input,
        ))
        .expect("otherwise should execute the right branch when left is empty");

        assert_eq!(out.len(), 1);
        assert_eq!(out[0], scalar_row("x", 42));
    }

    #[test]
    fn set_operation_right_branch_reads_input_bindings() {
        let store = GraphStore::new();
        let parameters = params();
        let ctx = ExecuteCtx::new(
            &store,
            &parameters,
            None,
            GqlExecutionContext::default(),
            None,
        );
        let right = project_var_as_plan("n", "x");
        let left = vec![scalar_row("x", 1)];
        let right_input = vec![scalar_row("n", 1)];

        let out = pollster::block_on(execute_set_operation(
            &ctx,
            left,
            SetOp::Union,
            &right,
            right_input,
        ))
        .expect("set operation should read the right branch input row");

        assert_eq!(out.len(), 1);
        assert_eq!(out[0], scalar_row("x", 1));
    }
}
