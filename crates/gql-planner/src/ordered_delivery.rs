//! Index-ordered ORDER BY delivery (ADR 0081, Slice A).
//!
//! When a query sorts by the exact property its driving index anchor scans
//! (`MATCH (v:X) WHERE v.p >= 10 RETURN v ORDER BY v.p`), the posting stream already
//! yields rows in ascending encoded-key order and no `Sort` work is needed. This module
//! owns the single eligibility proof for that delivery contract:
//!
//! - [`index_ordered_delivery_candidate`] is the planner-side shape proof run once at
//!   each Sort emission site; on success the planner records the intent on the leading
//!   [`PlanOp::IndexScan`] (and elides or keeps the `Sort` op).
//! - [`index_ordered_delivery_active`] is the executor-side gate: the same proof plus a
//!   check that the recorded intent actually survived planning and wire round-trip.
//! - [`op_preserves_leading_row_order`] classifies operators that cannot reorder rows
//!   bound before them; both proofs share this one classification.
//!
//! Eligibility contract (ADR 0081 §Decision 1): exactly one sort item whose expression
//! is a bare `PropertyAccess(var, p)` with ASC direction; a driving vertex anchor on the
//! same `var`+`p` among equality | IN | range | prefix bounds; `var` bound by the leading
//! scan; and only order-preserving operators between that scan and the sort site.
//! `null_order` annotations are accepted and vacuous: the sparse-posting invariant means
//! every delivered row carries `p`. DESC, multi-key sorts, and edge-side sorts are out of
//! scope (Slice B / future slices) and keep their `Sort`/`TopK`.

use gleaph_gql::ast::{CmpOp, OrderByClause, SortDirection};

use crate::anchor::extract_property_access;
use crate::plan::{PlanOp, ScanValue, Str};

/// True when executing `op` cannot reorder rows bound by the operators before it:
/// row-local reshaping only. Any operator not classified here must fail the ordered-
/// delivery proof, so newly added variants are conservatively excluded.
pub fn op_preserves_leading_row_order(op: &PlanOp) -> bool {
    matches!(
        op,
        PlanOp::PropertyFilter { .. }
            | PlanOp::Filter { .. }
            | PlanOp::Let { .. }
            | PlanOp::Project {
                distinct: false,
                ..
            }
    )
}

/// The single ascending bare-property key of `order_by`, if the clause has exactly one
/// item shaped `(var).(property)` with ASC direction. `null_order` is accepted as vacuous.
fn ascending_single_property_key(order_by: &OrderByClause) -> Option<(String, String)> {
    let [item] = order_by.items.as_slice() else {
        return None;
    };
    match item.direction {
        None | Some(SortDirection::Asc | SortDirection::Ascending) => {}
        Some(SortDirection::Desc | SortDirection::Descending) => return None,
    }
    extract_property_access(&item.expr)
}

/// Whether an `IndexScan` bound delivers rows in ascending encoded-key order of the
/// scanned property. Equality probes deliver one tie group; IN-list unions concatenate
/// per-element blocks in encoded payload byte order (ADR 0081 §4, executor contract);
/// prefix anchors lower into contiguous encoded intervals like ranges.
fn scan_bound_is_ordered(cmp: CmpOp, value: &ScanValue) -> bool {
    match cmp {
        CmpOp::Eq => true,
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
            matches!(value, ScanValue::Literal(_) | ScanValue::Parameter(_))
        }
        CmpOp::Ne => false,
    }
}

/// Planner-side proof that the pipeline prefix `ops` (everything before the sort site)
/// delivers rows globally ascending by `order_by`'s single key. Returns the proven
/// `(variable, property)` so the caller can record the intent on the leading scan.
pub fn index_ordered_delivery_candidate(
    ops: &[PlanOp],
    order_by: &OrderByClause,
) -> Option<(Str, Str)> {
    let (var, prop) = ascending_single_property_key(order_by)?;
    let (first, rest) = ops.split_first()?;
    let PlanOp::IndexScan {
        variable,
        property,
        value,
        cmp,
        ..
    } = first
    else {
        return None;
    };
    if variable.as_ref() != var.as_str() || property.as_ref() != prop.as_str() {
        return None;
    }
    if !scan_bound_is_ordered(*cmp, value) {
        return None;
    }
    if !rest.iter().all(op_preserves_leading_row_order) {
        return None;
    }
    Some((variable.clone(), property.clone()))
}

/// Executor-side gate for consuming ordered delivery from `ops` (the executed slice up
/// to a `Sort`/`TopK` site): the candidate proof plus the recorded intent on the leading
/// scan. Router-seeded executions skip the annotated anchor op entirely, so this gate
/// fails closed there and consumers fall back to full sorting regardless of seed order.
pub fn index_ordered_delivery_active(
    ops: &[PlanOp],
    order_by: &OrderByClause,
) -> Option<(Str, Str)> {
    let (var, prop) = index_ordered_delivery_candidate(ops, order_by)?;
    match ops.first() {
        Some(PlanOp::IndexScan {
            ordered_by_sort: Some(delivered),
            ..
        }) if delivered.as_ref() == prop.as_ref() => Some((var, prop)),
        _ => None,
    }
}

/// Record ordered-delivery intent on the leading [`PlanOp::IndexScan`] after
/// [`index_ordered_delivery_candidate`] proved it. Returns false when the leading op is
/// not an `IndexScan` (caller must then treat the plan as ineligible).
pub fn mark_leading_index_scan_ordered(ops: &mut [PlanOp], property: Str) -> bool {
    let Some(PlanOp::IndexScan {
        ordered_by_sort, ..
    }) = ops.first_mut()
    else {
        return false;
    };
    *ordered_by_sort = Some(property);
    true
}

/// Emit the ORDER BY site for a pipeline whose prefix `ops` may already deliver rows in
/// ascending key order (ADR 0081 Slice A).
///
/// Eligible pipelines record the intent on the leading scan. When a `LIMIT`/`OFFSET`
/// follows (`limited`), the `Sort` op is still pushed so the existing Sort+Limit → TopK
/// fusion forms and execution can stop at its tie-group boundary; without a limit the
/// `Sort` op is elided outright because the rows already arrive sorted. Ineligible
/// pipelines keep the unconditional `Sort` and no intent is recorded, so they compile
/// exactly as before.
///
/// Standalone `ORDER BY` statements pass `limited = false`: a following `LIMIT`
/// statement truncates the ordered stream directly, which yields the same row set as
/// `TopK` under GQL's unspecified tie order.
pub fn emit_sort_or_mark_ordered_delivery(
    ops: &mut Vec<PlanOp>,
    order_by: OrderByClause,
    limited: bool,
) {
    if let Some((_, property)) = index_ordered_delivery_candidate(ops, &order_by)
        && mark_leading_index_scan_ordered(ops, property)
    {
        if limited {
            ops.push(PlanOp::Sort { order_by });
        }
        return;
    }
    ops.push(PlanOp::Sort { order_by });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;
    use gleaph_gql::ast::ExprKind;
    use gleaph_gql::token::Span;
    use std::rc::Rc;

    fn order_by(expr: gleaph_gql::ast::Expr, direction: Option<SortDirection>) -> OrderByClause {
        OrderByClause {
            span: Span::DUMMY,
            items: vec![gleaph_gql::ast::SortItem {
                span: Span::DUMMY,
                expr,
                direction,
                null_order: None,
            }],
        }
    }

    fn prop_access(var: &str, property: &str) -> gleaph_gql::ast::Expr {
        gleaph_gql::ast::Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(gleaph_gql::ast::Expr::var(var)),
            property: property.into(),
        })
    }

    fn index_scan(var: &str, property: &str, cmp: CmpOp) -> PlanOp {
        PlanOp::IndexScan {
            variable: Str::from(var),
            property: Str::from(property),
            value: ScanValue::Literal(Value::Int64(5)),
            cmp,
            property_projection: None,
            ordered_by_sort: None,
        }
    }

    fn filter() -> PlanOp {
        PlanOp::Filter {
            condition: gleaph_gql::ast::Expr::var("v"),
        }
    }

    #[test]
    fn candidate_accepts_ascending_range_anchor_with_residual_filter() {
        let ops = vec![index_scan("v", "p", CmpOp::Ge), filter()];
        assert_eq!(
            index_ordered_delivery_candidate(&ops, &order_by(prop_access("v", "p"), None)),
            Some((Str::from("v"), Str::from("p")))
        );
    }

    #[test]
    fn candidate_rejects_desc_direction() {
        let ops = vec![index_scan("v", "p", CmpOp::Ge)];
        assert_eq!(
            index_ordered_delivery_candidate(
                &ops,
                &order_by(prop_access("v", "p"), Some(SortDirection::Desc))
            ),
            None
        );
    }

    #[test]
    fn candidate_rejects_multiple_sort_items() {
        let mut ob = order_by(prop_access("v", "p"), None);
        ob.items.push(gleaph_gql::ast::SortItem {
            span: Span::DUMMY,
            expr: prop_access("v", "q"),
            direction: None,
            null_order: None,
        });
        let ops = vec![index_scan("v", "p", CmpOp::Ge)];
        assert_eq!(index_ordered_delivery_candidate(&ops, &ob), None);
    }

    #[test]
    fn candidate_rejects_expression_sort_and_other_property_sort() {
        let ops = vec![index_scan("v", "p", CmpOp::Ge)];
        let expr_sort = order_by(
            gleaph_gql::ast::Expr::new(ExprKind::BinaryOp {
                left: Box::new(prop_access("v", "p")),
                op: gleaph_gql::ast::BinaryOp::Add,
                right: Box::new(gleaph_gql::ast::Expr::new(ExprKind::Literal(Value::Int64(
                    1,
                )))),
            }),
            None,
        );
        assert_eq!(index_ordered_delivery_candidate(&ops, &expr_sort), None);
        assert_eq!(
            index_ordered_delivery_candidate(&ops, &order_by(prop_access("v", "q"), None)),
            None
        );
    }

    #[test]
    fn candidate_rejects_non_index_leading_op_and_distinct_project() {
        let node_first = vec![
            PlanOp::NodeScan {
                variable: Str::from("v"),
                label: None,
                property_projection: None,
            },
            filter(),
        ];
        assert_eq!(
            index_ordered_delivery_candidate(&node_first, &order_by(prop_access("v", "p"), None)),
            None
        );
        let distinct_project = vec![
            index_scan("v", "p", CmpOp::Ge),
            PlanOp::Project {
                columns: vec![],
                distinct: true,
            },
        ];
        assert_eq!(
            index_ordered_delivery_candidate(
                &distinct_project,
                &order_by(prop_access("v", "p"), None)
            ),
            None
        );
    }

    #[test]
    fn candidate_rejects_ne_bound_and_inlist_range_bound() {
        let ne = vec![index_scan("v", "p", CmpOp::Ne)];
        assert_eq!(
            index_ordered_delivery_candidate(&ne, &order_by(prop_access("v", "p"), None)),
            None
        );
        let inlist_range = vec![PlanOp::IndexScan {
            variable: Str::from("v"),
            property: Str::from("p"),
            value: ScanValue::InList(vec![ScanValue::Literal(Value::Int64(1))]),
            cmp: CmpOp::Lt,
            property_projection: None,
            ordered_by_sort: None,
        }];
        assert_eq!(
            index_ordered_delivery_candidate(&inlist_range, &order_by(prop_access("v", "p"), None)),
            None
        );
    }

    #[test]
    fn active_requires_recorded_intent_on_leading_scan() {
        let mut ops = vec![index_scan("v", "p", CmpOp::Ge), filter()];
        assert_eq!(
            index_ordered_delivery_active(&ops, &order_by(prop_access("v", "p"), None)),
            None
        );
        assert!(mark_leading_index_scan_ordered(&mut ops, Rc::from("p")));
        assert_eq!(
            index_ordered_delivery_active(&ops, &order_by(prop_access("v", "p"), None)),
            Some((Str::from("v"), Str::from("p")))
        );
    }

    #[test]
    fn mark_fails_closed_without_leading_index_scan() {
        let mut ops = vec![filter()];
        assert!(!mark_leading_index_scan_ordered(&mut ops, Rc::from("p")));
    }
}
