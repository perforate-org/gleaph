//! Index posting-bucket fast path for federated `COUNT(*)` `GROUP BY` on one indexed property.

use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::ast::{AggregateFunc, CmpOp, Expr, ExprKind};
use gleaph_gql::index_key_bytes_to_value;
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_gql_planner::GraphStats;
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp};
use gleaph_graph_kernel::index::ValuePostingCount;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;

use crate::facade::store::RouterStore;
use crate::planner_stats::RouterGraphStats;
use crate::seed::IndexAnchor;

use super::aggregate_merge::{
    FederatedAggregateMerge, FederatedMergeMode, federated_merge_mode_from_plans,
};

/// Eligible aggregate query answered by scanning index postings for one property bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregateIndexFastPath {
    pub property_id: u32,
    pub group_key_column: String,
    pub count_column: String,
    pub min_count: u64,
    /// When set, posting counts are restricted to vertices from this index anchor.
    pub index_anchor: Option<IndexAnchor>,
}

/// Detect whether `plans` match the index posting-count fast path.
pub fn try_aggregate_index_fast_path(
    plans: &[PhysicalPlan],
    stats: &RouterGraphStats,
    store: &RouterStore,
    params: &BTreeMap<String, Value>,
) -> Option<AggregateIndexFastPath> {
    let FederatedMergeMode::Aggregate(spec) = federated_merge_mode_from_plans(plans) else {
        return None;
    };
    if !fast_path_aggregate_spec_matches(&spec) {
        return None;
    }
    let (aggregate_idx, group_by) = find_aggregate_group_by(plans)?;
    if group_by.len() != 1 {
        return None;
    }
    let group_var = group_by_variable(&group_by[0])?;
    let property = group_by_indexed_property(&group_by[0])?;
    if !stats.is_vertex_property_indexed(property) {
        return None;
    }
    let property_id = store.lookup_property_id(property).ok()?.raw();
    let ops = plans.last()?.ops.as_slice();
    let prefix = &ops[..aggregate_idx];
    let index_anchor = match parse_fast_path_prefix(prefix, params, store) {
        Ok(Some(anchor)) => anchor,
        Ok(None) | Err(_) => return None,
    };
    if let Some(anchor) = &index_anchor
        && anchor.variable() != group_var
    {
        return None;
    }
    let min_count = extract_having_min_count(spec.having.as_ref(), &spec.aggregate_columns[0].name)
        .unwrap_or(1);
    Some(AggregateIndexFastPath {
        property_id,
        group_key_column: spec.group_key_columns[0].clone(),
        count_column: spec.aggregate_columns[0].name.clone(),
        min_count,
        index_anchor,
    })
}

/// Build a [`GqlQueryResult`] from index posting bucket counts.
pub fn gql_query_result_from_posting_counts(
    fast_path: &AggregateIndexFastPath,
    counts: Vec<ValuePostingCount>,
) -> Result<GqlQueryResult, String> {
    let mut rows: Vec<BTreeMap<String, Value>> = counts
        .into_iter()
        .map(|entry| {
            let group_value = index_key_bytes_to_value(&entry.encoded_value)
                .unwrap_or(Value::Bytes(entry.encoded_value));
            let mut row = BTreeMap::new();
            row.insert(fast_path.group_key_column.clone(), group_value);
            row.insert(
                fast_path.count_column.clone(),
                Value::Int64(
                    i64::try_from(entry.count)
                        .map_err(|_| format!("posting count overflow: {}", entry.count))?,
                ),
            );
            Ok(row)
        })
        .collect::<Result<Vec<_>, String>>()?;
    rows.sort_by(|left, right| compare_group_key_rows(left, right, &fast_path.group_key_column));
    let row_count = rows.len() as u64;
    let rows_blob = if rows.is_empty() {
        None
    } else {
        Some(
            IcWirePlanQueryResult::try_from_value_rows(&rows)
                .map_err(|e| e.to_string())?
                .encode_blob()
                .map_err(|e| e.to_string())?,
        )
    };
    Ok(GqlQueryResult {
        row_count,
        rows_blob,
    })
}

fn fast_path_aggregate_spec_matches(spec: &FederatedAggregateMerge) -> bool {
    spec.group_key_columns.len() == 1
        && spec.aggregate_columns.len() == 1
        && spec.aggregate_columns[0].func == AggregateFunc::CountStar
}

fn find_aggregate_group_by(plans: &[PhysicalPlan]) -> Option<(usize, Vec<Expr>)> {
    let ops = plans.last()?.ops.as_slice();
    for (idx, op) in ops.iter().enumerate() {
        if let PlanOp::Aggregate { group_by, .. } = op {
            return Some((idx, group_by.clone()));
        }
    }
    None
}

fn group_by_variable(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::PropertyAccess { expr, .. } => match &expr.kind {
            ExprKind::Variable(name) => Some(name.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn group_by_indexed_property(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::PropertyAccess { property, .. } => Some(property.as_str()),
        _ => None,
    }
}

/// `Ok(None)` — prefix is unfiltered (empty or unlabeled `NodeScan`).
/// `Ok(Some(anchor))` — prefix is a single index anchor op.
/// `Err` — parameter / catalog resolution failed.
fn parse_fast_path_prefix(
    ops: &[PlanOp],
    params: &BTreeMap<String, Value>,
    store: &RouterStore,
) -> Result<Option<Option<IndexAnchor>>, crate::state::RouterError> {
    match ops {
        [] => Ok(Some(None)),
        [PlanOp::NodeScan { label: None, .. }] => Ok(Some(None)),
        [PlanOp::NodeScan { label: Some(_), .. }] => Ok(None),
        [op] => crate::seed::index_anchor_from_prefix_ops(std::slice::from_ref(op), params, store)
            .map(|anchor| anchor.map(Some)),
        _ => Ok(None),
    }
}

fn extract_having_min_count(having: Option<&Expr>, count_column: &str) -> Option<u64> {
    let having = having?;
    let ExprKind::Compare { left, op, right } = &having.kind else {
        return None;
    };
    let threshold = match &right.kind {
        ExprKind::Literal(Value::Int64(n)) if *n >= 0 => *n as u64,
        _ => return None,
    };
    let is_count_ref = match &left.kind {
        ExprKind::Aggregate {
            func: AggregateFunc::CountStar,
            ..
        } => true,
        ExprKind::Variable(name) => name == count_column,
        _ => false,
    };
    if !is_count_ref {
        return None;
    }
    match op {
        CmpOp::Gt => Some(threshold.saturating_add(1)),
        CmpOp::Ge => Some(threshold),
        _ => None,
    }
}

fn compare_group_key_rows(
    left: &BTreeMap<String, Value>,
    right: &BTreeMap<String, Value>,
    column: &str,
) -> std::cmp::Ordering {
    match (left.get(column), right.get(column)) {
        (Some(lv), Some(rv)) => compare_values(lv, rv).unwrap_or(std::cmp::Ordering::Equal),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use gleaph_gql::ast::{AggregateFunc, Expr, ExprKind};
    use gleaph_gql_planner::plan::{
        AggregateSpec, PhysicalPlan, PlanOp, ProjectColumn, ScanValue, Str,
    };

    use super::*;
    use crate::facade::store::RouterStore;
    use crate::init::RouterInitArgs;

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

    fn grouped_count_tail(property: &str, having: Option<Expr>) -> Vec<PlanOp> {
        let group = Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::var("n")),
            property: property.into(),
        });
        let mut ops = vec![PlanOp::Aggregate {
            group_by: vec![group.clone()],
            aggregates: vec![AggregateSpec {
                func: AggregateFunc::CountStar,
                expr: None,
                expr2: None,
                distinct: false,
                filter: None,
                order_by: None,
                alias: None,
            }],
        }];
        if let Some(h) = having {
            ops.push(PlanOp::Filter { condition: h });
        }
        ops.push(PlanOp::Project {
            columns: vec![
                ProjectColumn {
                    expr: group,
                    alias: Some(Str::from("country")),
                },
                ProjectColumn {
                    expr: agg_count_star(),
                    alias: Some(Str::from("cnt")),
                },
            ],
            distinct: false,
        });
        ops
    }

    fn grouped_count_plan(property: &str, having: Option<Expr>) -> PhysicalPlan {
        PhysicalPlan::from_ops(grouped_count_tail(property, having))
    }

    fn store_with_country_and_region() -> RouterStore {
        let store = RouterStore::new();
        let admin = candid::Principal::anonymous();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![admin],
            controllers: vec![],
        });
        store.bootstrap_controllers(&[admin]);
        store
            .admin_intern_property(admin, "country")
            .expect("intern country");
        store
            .admin_intern_property(admin, "region")
            .expect("intern region");
        store
    }

    #[test]
    fn detects_grouped_count_star_on_indexed_property() {
        let store = store_with_country_and_region();
        let stats = RouterGraphStats::default().with_indexed_vertex_property("country");
        let plan = grouped_count_plan("country", None);
        let fast = try_aggregate_index_fast_path(&[plan], &stats, &store, &BTreeMap::new())
            .expect("fast path");
        assert_eq!(fast.group_key_column, "country");
        assert_eq!(fast.count_column, "cnt");
        assert_eq!(fast.min_count, 1);
        assert!(fast.index_anchor.is_none());
    }

    #[test]
    fn detects_index_scan_prefix_with_seed_anchor() {
        let store = store_with_country_and_region();
        let stats = RouterGraphStats::default().with_indexed_vertex_property("country");
        let mut ops = vec![PlanOp::IndexScan {
            variable: Rc::from("n"),
            property: Rc::from("region"),
            value: ScanValue::Literal(Value::Text("US".into())),
            cmp: CmpOp::Eq,
            property_projection: None,
        }];
        ops.extend(grouped_count_tail("country", None));
        let plan = PhysicalPlan::from_ops(ops);
        let fast = try_aggregate_index_fast_path(&[plan], &stats, &store, &BTreeMap::new())
            .expect("fast path");
        assert!(matches!(fast.index_anchor, Some(IndexAnchor::Equal(_))));
        assert_eq!(
            fast.property_id,
            store.lookup_property_id("country").unwrap().raw()
        );
    }

    #[test]
    fn rejects_labeled_node_scan_prefix() {
        let store = store_with_country_and_region();
        let stats = RouterGraphStats::default().with_indexed_vertex_property("country");
        let group = Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::var("n")),
            property: "country".into(),
        });
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("n"),
                label: Some(gleaph_gql_planner::NodeLabelRef::from("Person")),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![group.clone()],
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
                columns: vec![
                    ProjectColumn {
                        expr: group,
                        alias: Some(Str::from("country")),
                    },
                    ProjectColumn {
                        expr: agg_count_star(),
                        alias: Some(Str::from("cnt")),
                    },
                ],
                distinct: false,
            },
        ]);
        assert!(try_aggregate_index_fast_path(&[plan], &stats, &store, &BTreeMap::new()).is_none());
    }

    #[test]
    fn extract_having_min_count_from_count_star_gt() {
        let having = Expr::new(ExprKind::Compare {
            left: Box::new(agg_count_star()),
            op: CmpOp::Gt,
            right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(5)))),
        });
        assert_eq!(extract_having_min_count(Some(&having), "cnt"), Some(6));
    }
}
