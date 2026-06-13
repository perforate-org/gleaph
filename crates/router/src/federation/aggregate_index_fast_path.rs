//! Index posting-bucket fast path for federated `COUNT(*)` `GROUP BY` on one indexed property.

use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::ast::{AggregateFunc, CmpOp, Expr, ExprKind};
use gleaph_gql::index_key_bytes_to_value;
use gleaph_gql::types::LabelExpr;
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql::value_to_index_key_bytes;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_gql_planner::GraphStats;
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp};
use gleaph_graph_kernel::entry::VertexLabelId;
use gleaph_graph_kernel::index::ValuePostingCount;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;

use crate::facade::store::RouterStore;
use crate::planner_stats::RouterGraphStats;
use crate::seed::{IndexAnchor, SeedProbe, resolve_scan_value};

use super::aggregate_merge::{
    FederatedAggregateMerge, FederatedMergeMode, federated_merge_mode_from_plans,
};

/// `MATCH (n:L) RETURN count(*)` answered from router label telemetry (no graph-index call).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelCountTelemetryFastPath {
    pub vertex_label_id: u32,
    pub count_column: String,
    pub min_count: u64,
}

/// Eligible aggregate query answered by scanning index postings for one property bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregateIndexFastPath {
    pub property_id: u32,
    pub group_key_column: String,
    pub count_column: String,
    pub min_count: u64,
    /// When empty, all postings in the property bucket are counted. Otherwise hits from each
    /// anchor are intersected on `(shard_id, vertex_id)` before counting.
    pub index_anchors: Vec<IndexAnchor>,
}

/// Split label membership from property index anchors on a grouped-count fast path.
pub fn split_label_and_property_anchors(
    anchors: &[IndexAnchor],
) -> Result<(Option<u32>, Vec<IndexAnchor>), ()> {
    let mut label_id = None;
    let mut property_anchors = Vec::new();
    for anchor in anchors {
        match anchor {
            IndexAnchor::Label {
                vertex_label_id, ..
            } => {
                if label_id.is_some() {
                    return Err(());
                }
                label_id = Some(*vertex_label_id);
            }
            IndexAnchor::LabelIntersection { .. } => return Err(()),
            other => property_anchors.push(other.clone()),
        }
    }
    Ok((label_id, property_anchors))
}

/// Detect `MATCH (n:L) RETURN count(*)` without `GROUP BY` on an indexed property.
pub fn try_label_count_telemetry_fast_path(
    plans: &[PhysicalPlan],
    stats: &RouterGraphStats,
    store: &RouterStore,
    params: &BTreeMap<String, Value>,
) -> Option<LabelCountTelemetryFastPath> {
    let FederatedMergeMode::Aggregate(spec) = federated_merge_mode_from_plans(plans) else {
        return None;
    };
    if !spec.group_key_columns.is_empty() {
        return None;
    }
    if spec.aggregate_columns.len() != 1
        || spec.aggregate_columns[0].func != AggregateFunc::CountStar
    {
        return None;
    }
    let (aggregate_idx, group_by) = find_aggregate_group_by(plans)?;
    if !group_by.is_empty() {
        return None;
    }
    let ops = plans.last()?.ops.as_slice();
    let prefix = &ops[..aggregate_idx];
    let anchors = match parse_fast_path_prefix(prefix, params, store, stats) {
        Ok(Some(anchors)) if anchors.len() == 1 => anchors,
        _ => return None,
    };
    let IndexAnchor::Label {
        vertex_label_id, ..
    } = anchors[0]
    else {
        return None;
    };
    let min_count = extract_having_min_count(spec.having.as_ref(), &spec.aggregate_columns[0].name)
        .unwrap_or(1);
    Some(LabelCountTelemetryFastPath {
        vertex_label_id,
        count_column: spec.aggregate_columns[0].name.clone(),
        min_count,
    })
}

/// Build a single-row count result from router label telemetry.
pub fn gql_query_result_from_label_live_count(
    fast_path: &LabelCountTelemetryFastPath,
    live_count: u64,
) -> Result<GqlQueryResult, String> {
    if live_count < fast_path.min_count {
        return Ok(GqlQueryResult {
            row_count: 0,
            rows_blob: None,
        });
    }
    let mut row = BTreeMap::new();
    row.insert(
        fast_path.count_column.clone(),
        Value::Int64(
            i64::try_from(live_count)
                .map_err(|_| format!("label live count overflow: {live_count}"))?,
        ),
    );
    let rows_blob = IcWirePlanQueryResult::try_from_value_rows(&[row])
        .map_err(|e| e.to_string())?
        .encode_blob()
        .map_err(|e| e.to_string())?;
    Ok(GqlQueryResult {
        row_count: 1,
        rows_blob: Some(rows_blob),
    })
}

/// Live vertex count for a label from router telemetry.
pub fn vertex_label_live_count(store: &RouterStore, vertex_label_id: u32) -> u64 {
    store
        .vertex_label_stats(VertexLabelId::from_raw(vertex_label_id as u16))
        .live_count
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
    let index_anchors = match parse_fast_path_prefix(prefix, params, store, stats) {
        Ok(Some(anchors)) => anchors,
        Ok(None) | Err(_) => return None,
    };
    if index_anchors
        .iter()
        .any(|anchor| anchor.variable() != group_var)
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
        index_anchors,
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

/// `Ok(None)` — prefix is not eligible for the fast path.
/// `Ok(Some(anchors))` — vertex filter anchors (empty = unfiltered bucket scan).
/// `Err` — parameter / catalog resolution failed.
fn parse_fast_path_prefix(
    ops: &[PlanOp],
    params: &BTreeMap<String, Value>,
    store: &RouterStore,
    stats: &RouterGraphStats,
) -> Result<Option<Vec<IndexAnchor>>, crate::state::RouterError> {
    if ops.is_empty() {
        return Ok(Some(Vec::new()));
    }
    if ops.len() == 1 {
        return match &ops[0] {
            PlanOp::NodeScan { label: None, .. } => Ok(Some(Vec::new())),
            PlanOp::NodeScan {
                label: Some(label),
                variable,
                ..
            } => Ok(Some(vec![label_anchor(store, label.as_ref(), variable)?])),
            _ => match crate::seed::index_anchor_from_prefix_ops(ops, params, store)? {
                Some(anchor) => Ok(Some(vec![anchor])),
                None => Ok(None),
            },
        };
    }

    let mut anchors = Vec::new();
    let mut bound_var: Option<String> = None;
    for op in ops {
        match op {
            PlanOp::NodeScan {
                label: Some(label),
                variable,
                ..
            } => {
                record_bound_var(&mut bound_var, variable)?;
                anchors.push(label_anchor(store, label.as_ref(), variable)?);
            }
            PlanOp::NodeScan { label: None, .. } => return Ok(None),
            PlanOp::IndexScan {
                variable,
                property,
                value,
                cmp,
                ..
            } if *cmp == CmpOp::Eq && stats.is_vertex_property_indexed(property.as_ref()) => {
                record_bound_var(&mut bound_var, variable)?;
                anchors.push(equal_anchor(
                    store,
                    params,
                    variable,
                    property.as_ref(),
                    value,
                )?);
            }
            PlanOp::IndexScan { .. } | PlanOp::IndexIntersection { .. } => return Ok(None),
            PlanOp::PropertyFilter { predicates, .. } => {
                for predicate in predicates {
                    if let Some(anchor) = anchor_from_property_predicate(
                        predicate,
                        bound_var.as_deref(),
                        params,
                        store,
                        stats,
                    )? {
                        record_bound_var(&mut bound_var, anchor.variable())?;
                        if anchors
                            .iter()
                            .all(|existing| !same_anchor_restriction(existing, &anchor))
                        {
                            anchors.push(anchor);
                        }
                    }
                }
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(anchors))
}

fn record_bound_var(
    bound_var: &mut Option<String>,
    variable: &str,
) -> Result<(), crate::state::RouterError> {
    if let Some(existing) = bound_var {
        if existing != variable {
            return Err(crate::state::RouterError::InvalidArgument(
                "fast path prefix binds multiple variables".into(),
            ));
        }
    } else {
        *bound_var = Some(variable.to_string());
    }
    Ok(())
}

fn label_anchor(
    store: &RouterStore,
    label: &str,
    variable: impl AsRef<str>,
) -> Result<IndexAnchor, crate::state::RouterError> {
    let vertex_label_id = u32::from(
        store
            .lookup_vertex_label_id(label)
            .map_err(|_| crate::state::RouterError::NotFound(format!("label {label}")))?
            .raw(),
    );
    Ok(IndexAnchor::Label {
        variable: variable.as_ref().to_string(),
        vertex_label_id,
    })
}

fn equal_anchor(
    store: &RouterStore,
    params: &BTreeMap<String, Value>,
    variable: impl AsRef<str>,
    property: &str,
    value: &gleaph_gql_planner::plan::ScanValue,
) -> Result<IndexAnchor, crate::state::RouterError> {
    let payload_bytes = resolve_scan_value(value, params).ok_or_else(|| {
        crate::state::RouterError::InvalidArgument("missing fast path parameter".into())
    })?;
    let property_id = store
        .lookup_property_id(property)
        .map_err(|_| crate::state::RouterError::NotFound(format!("property {property}")))?
        .raw();
    Ok(IndexAnchor::Equal(SeedProbe {
        variable: variable.as_ref().to_string(),
        property: property.to_string(),
        property_id,
        payload_bytes,
    }))
}

fn anchor_from_property_predicate(
    predicate: &Expr,
    bound_var: Option<&str>,
    params: &BTreeMap<String, Value>,
    store: &RouterStore,
    stats: &RouterGraphStats,
) -> Result<Option<IndexAnchor>, crate::state::RouterError> {
    match &predicate.kind {
        ExprKind::IsLabeled {
            expr,
            label: LabelExpr::Name(label),
            negated: false,
        } => {
            let Some(variable) = variable_from_expr(expr) else {
                return Ok(None);
            };
            if bound_var.is_some_and(|v| v != variable) {
                return Ok(None);
            }
            Ok(Some(label_anchor(store, label, variable)?))
        }
        ExprKind::Compare {
            left,
            op: CmpOp::Eq,
            right,
        } => {
            let Some((variable, property)) = indexed_property_access(left, stats) else {
                return Ok(None);
            };
            if bound_var.is_some_and(|v| v != variable) {
                return Ok(None);
            }
            let payload_bytes = value_to_index_key_bytes(expr_literal_or_param(right, params)?)
                .map_err(|_| {
                    crate::state::RouterError::InvalidArgument(
                        "fast path filter value is not indexable".into(),
                    )
                })?
                .ok_or_else(|| {
                    crate::state::RouterError::InvalidArgument(
                        "fast path filter rejects null".into(),
                    )
                })?;
            let property_id = store
                .lookup_property_id(&property)
                .map_err(|_| crate::state::RouterError::NotFound(format!("property {property}")))?
                .raw();
            Ok(Some(IndexAnchor::Equal(SeedProbe {
                variable,
                property,
                property_id,
                payload_bytes,
            })))
        }
        _ => Ok(None),
    }
}

fn variable_from_expr(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Variable(name) => Some(name.clone()),
        _ => None,
    }
}

fn indexed_property_access(expr: &Expr, stats: &RouterGraphStats) -> Option<(String, String)> {
    match &expr.kind {
        ExprKind::PropertyAccess { expr, property } => {
            let variable = variable_from_expr(expr)?;
            if stats.is_vertex_property_indexed(property) {
                Some((variable, property.clone()))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn expr_literal_or_param<'a>(
    expr: &'a Expr,
    params: &'a BTreeMap<String, Value>,
) -> Result<&'a Value, crate::state::RouterError> {
    match &expr.kind {
        ExprKind::Literal(value) => Ok(value),
        ExprKind::Parameter(name) => {
            let key = name.strip_prefix('$').unwrap_or(name.as_str());
            params.get(key).ok_or_else(|| {
                crate::state::RouterError::InvalidArgument("missing fast path parameter".into())
            })
        }
        _ => Err(crate::state::RouterError::InvalidArgument(
            "fast path filter expects literal or parameter".into(),
        )),
    }
}

fn same_anchor_restriction(left: &IndexAnchor, right: &IndexAnchor) -> bool {
    match (left, right) {
        (
            IndexAnchor::Label {
                vertex_label_id: l, ..
            },
            IndexAnchor::Label {
                vertex_label_id: r, ..
            },
        ) => l == r,
        (
            IndexAnchor::Equal(SeedProbe {
                property_id: l,
                payload_bytes: lb,
                ..
            }),
            IndexAnchor::Equal(SeedProbe {
                property_id: r,
                payload_bytes: rb,
                ..
            }),
        ) => l == r && lb == rb,
        _ => false,
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
        let stats = RouterGraphStats::test_vertex_indexed(&store, &["country"]);
        let plan = grouped_count_plan("country", None);
        let fast = try_aggregate_index_fast_path(&[plan], &stats, &store, &BTreeMap::new())
            .expect("fast path");
        assert_eq!(fast.group_key_column, "country");
        assert_eq!(fast.count_column, "cnt");
        assert_eq!(fast.min_count, 1);
        assert!(fast.index_anchors.is_empty());
    }

    #[test]
    fn detects_index_scan_prefix_with_seed_anchor() {
        let store = store_with_country_and_region();
        let stats = RouterGraphStats::test_vertex_indexed(&store, &["country"]);
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
        assert!(matches!(
            fast.index_anchors.as_slice(),
            [IndexAnchor::Equal(_)]
        ));
        assert_eq!(
            fast.property_id,
            store.lookup_property_id("country").unwrap().raw()
        );
    }

    #[test]
    fn detects_labeled_node_scan_prefix() {
        let store = store_with_country_and_region();
        let admin = candid::Principal::anonymous();
        store
            .admin_intern_vertex_label(admin, "Person")
            .expect("intern Person");
        let stats = RouterGraphStats::test_vertex_indexed(&store, &["country"]);
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
        let fast = try_aggregate_index_fast_path(&[plan], &stats, &store, &BTreeMap::new())
            .expect("fast path");
        assert!(matches!(
            fast.index_anchors.as_slice(),
            [IndexAnchor::Label { .. }]
        ));
    }

    #[test]
    fn detects_labeled_node_scan_and_index_scan_prefix() {
        let store = store_with_country_and_region();
        let admin = candid::Principal::anonymous();
        store
            .admin_intern_vertex_label(admin, "Person")
            .expect("intern Person");
        let stats = RouterGraphStats::test_vertex_indexed(&store, &["country", "region"]);
        let mut ops = vec![
            PlanOp::NodeScan {
                variable: Rc::from("n"),
                label: Some(gleaph_gql_planner::NodeLabelRef::from("Person")),
                property_projection: None,
            },
            PlanOp::IndexScan {
                variable: Rc::from("n"),
                property: Rc::from("region"),
                value: ScanValue::Literal(Value::Text("US".into())),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
        ];
        ops.extend(grouped_count_tail("country", None));
        let plan = PhysicalPlan::from_ops(ops);
        let fast = try_aggregate_index_fast_path(&[plan], &stats, &store, &BTreeMap::new())
            .expect("fast path");
        assert_eq!(fast.index_anchors.len(), 2);
        assert!(fast.index_anchors.iter().any(|anchor| {
            matches!(
                anchor,
                IndexAnchor::Label {
                    vertex_label_id: 1,
                    ..
                }
            )
        }));
        assert!(
            fast.index_anchors
                .iter()
                .any(|anchor| matches!(anchor, IndexAnchor::Equal(_)))
        );
    }

    #[test]
    fn detects_index_scan_with_is_labeled_property_filter_prefix() {
        let store = store_with_country_and_region();
        let admin = candid::Principal::anonymous();
        store
            .admin_intern_vertex_label(admin, "Person")
            .expect("intern Person");
        let stats = RouterGraphStats::test_vertex_indexed(&store, &["country", "region"]);
        let mut ops = vec![
            PlanOp::IndexScan {
                variable: Rc::from("n"),
                property: Rc::from("region"),
                value: ScanValue::Literal(Value::Text("US".into())),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::IsLabeled {
                    expr: Box::new(Expr::var("n")),
                    label: LabelExpr::Name("Person".into()),
                    negated: false,
                })],
                stage: 0,
            },
        ];
        ops.extend(grouped_count_tail("country", None));
        let plan = PhysicalPlan::from_ops(ops);
        let fast = try_aggregate_index_fast_path(&[plan], &stats, &store, &BTreeMap::new())
            .expect("fast path");
        assert_eq!(fast.index_anchors.len(), 2);
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

    #[test]
    fn detects_label_only_count_star_return() {
        let store = store_with_country_and_region();
        let admin = candid::Principal::anonymous();
        store
            .admin_intern_vertex_label(admin, "Person")
            .expect("intern Person");
        let stats = RouterGraphStats::default();
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("n"),
                label: Some(gleaph_gql_planner::NodeLabelRef::from("Person")),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![],
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
                columns: vec![ProjectColumn {
                    expr: agg_count_star(),
                    alias: Some(Str::from("cnt")),
                }],
                distinct: false,
            },
        ]);
        let fast = try_label_count_telemetry_fast_path(&[plan], &stats, &store, &BTreeMap::new())
            .expect("label count fast path");
        assert_eq!(fast.vertex_label_id, 1);
        assert_eq!(fast.count_column, "cnt");
    }

    #[test]
    fn split_label_and_property_anchors_partitions() {
        let anchors = vec![
            IndexAnchor::Label {
                variable: "n".into(),
                vertex_label_id: 2,
            },
            IndexAnchor::Equal(SeedProbe {
                variable: "n".into(),
                property: "region".into(),
                property_id: 9,
                payload_bytes: vec![1],
            }),
        ];
        let (label, props) = split_label_and_property_anchors(&anchors).expect("split");
        assert_eq!(label, Some(2));
        assert_eq!(props.len(), 1);
    }
}
