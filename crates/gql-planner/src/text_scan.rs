//! Planner lowering of the scalar `text_score(prop, query)` contract (plan 0297).
//!
//! `text_score(…)` is a relevance score, so it can only be produced by a scan that
//! consulted the TEXT index — never re-evaluated per row. Two shapes lower into
//! [`PlanOp::TextScan`]:
//!
//! - threshold: a WHERE conjunct `text_score(v.prop, Q) > t` (or `>=`, or reversed);
//! - top-k: `ORDER BY text_score(v.prop, Q) DESC LIMIT k`.
//!
//! Every other placement fails closed at plan validation: an unfused `text_score`
//! expression rejects the plan instead of falling back to a sequential scan.

use gleaph_gql::ast::{CmpOp, Expr, ExprKind, OrderByClause};
use std::collections::{BTreeMap, BTreeSet};

use crate::plan::{PlanOp, ScanValue, TextScanMode, TextSeedInfo};
use crate::stats::GraphStats;

/// One resolved `text_score(variable.property, query)` reference.
#[derive(Clone, Debug)]
pub(crate) struct TextScoreRef {
    pub variable: String,
    pub property: String,
    /// The query expression: Text literal or parameter.
    pub query: ScanValue,
}

/// Whether this expression is exactly `text_score(...)` (case-insensitive, unqualified).
pub(crate) fn is_text_score_call(expr: &Expr) -> bool {
    let ExprKind::FunctionCall { name, .. } = &expr.kind else {
        return false;
    };
    name.parts.len() == 1 && name.parts[0].eq_ignore_ascii_case("text_score")
}

/// Resolve `text_score(v.prop, Q)` into its variable/property/query parts.
///
/// The first argument must be a property access rooted at a plain variable; the second
/// must bind a query at plan time (Text literal or parameter). Anything else returns
/// `None` and stays unfused.
fn resolve_text_score_call(expr: &Expr) -> Option<TextScoreRef> {
    let ExprKind::FunctionCall { args, .. } = &expr.kind else {
        return None;
    };
    if !is_text_score_call(expr) || args.len() != 2 {
        return None;
    }
    let (variable, property) = crate::anchor::extract_property_access(&args[0])?;
    let query = match &args[1].kind {
        ExprKind::Literal(gleaph_gql::Value::Text(_)) => anchor_scan_value(&args[1])?,
        ExprKind::Parameter(_) => anchor_scan_value(&args[1])?,
        _ => return None,
    };
    Some(TextScoreRef {
        variable,
        property,
        query,
    })
}

fn anchor_scan_value(expr: &Expr) -> Option<ScanValue> {
    crate::anchor::scan_value_from_expr(expr)
}

/// Extract a threshold predicate: `text_score(v.prop, Q) cmp bound` with `cmp ∈ {>, >=}`
/// (or the reversed operand order with `<`, `<=`). Any other comparison shape keeps the
/// conjunct unfused so validation rejects it fail-closed.
pub(crate) fn extract_threshold_predicate(expr: &Expr) -> Option<(TextScoreRef, CmpOp, ScanValue)> {
    let ExprKind::Compare { left, op, right } = &expr.kind else {
        return None;
    };
    let score_side = |side: &Expr| resolve_text_score_call(side);
    let bound_side = |side: &Expr| {
        matches!(side.kind, ExprKind::Literal(_) | ExprKind::Parameter(_))
            .then(|| anchor_scan_value(side))
            .flatten()
    };
    if matches!(op, CmpOp::Gt | CmpOp::Ge)
        && let Some(score) = score_side(left)
        && let Some(bound) = bound_side(right)
    {
        return Some((score, *op, bound));
    }
    if matches!(op, CmpOp::Lt | CmpOp::Le)
        && let Some(bound) = bound_side(left)
        && let Some(score) = score_side(right)
    {
        return Some((score, reverse_threshold_cmp(*op), bound));
    }
    None
}

fn reverse_threshold_cmp(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        other => other,
    }
}

/// Match an ORDER BY clause whose leading key is exactly `text_score(v.prop, Q) DESC`.
///
/// The scan itself delivers the decided `(score DESC, element-key ASC)` determinism
/// contract. An explicitly written second key is accepted only when it is the scanned
/// variable itself (a no-op restatement of the implicit tie-break); any other secondary
/// key keeps the pipeline unfused and fails closed at validation.
pub(crate) fn extract_topk_order(order_by: &OrderByClause) -> Option<TextScoreRef> {
    let items = &order_by.items;
    if items.is_empty()
        || !matches!(
            items[0].direction,
            Some(gleaph_gql::ast::SortDirection::Desc)
        )
    {
        return None;
    }
    let score = resolve_text_score_call(&items[0].expr)?;
    if items.len() == 2 {
        let key_is_variable =
            matches!(&items[1].expr.kind, ExprKind::Variable(v) if *v == score.variable);
        if !key_is_variable {
            return None;
        }
    } else if items.len() > 2 {
        return None;
    }
    Some(score)
}

/// Post-pass top-k lowering: rewrite
/// `[seed(v), …PropertyFilters…, TopK { order_by: [text_score DESC, …], k }]`
/// into `[TextScan { mode: TopK, … }, …PropertyFilters…]`.
///
/// Returns whether a rewrite happened. Coverage and label rules are enforced here:
/// no covering text index for `(label, property)` means no lowering, and the plan then
/// fails closed in validation because the score expression stays unfused.
pub(crate) fn apply_text_topk_lowering(
    ops: &mut Vec<PlanOp>,
    stats: Option<&dyn GraphStats>,
) -> bool {
    let Some(stats) = stats else {
        return false;
    };
    let Some(topk_idx) = ops
        .iter()
        .position(|op| matches!(op, PlanOp::TopK { offset: None, .. }))
    else {
        return false;
    };
    let (score, k) = {
        let PlanOp::TopK {
            order_by,
            k,
            offset: None,
            ..
        } = &ops[topk_idx]
        else {
            unreachable!("checked above");
        };
        let Some(k_value) = const_int64(k) else {
            return false;
        };
        match extract_topk_order(order_by) {
            Some(score) => (score, k_value),
            None => return false,
        }
    };

    // Everything between the seed and the TopK must be row filters; projections or
    // joins would break the "scan delivers scored rows" contract.
    if !ops[..topk_idx]
        .iter()
        .skip(1)
        .all(|op| matches!(op, PlanOp::PropertyFilter { .. } | PlanOp::Filter { .. }))
    {
        return false;
    }
    let seed_label = match &ops[0] {
        PlanOp::NodeScan {
            variable,
            label: Some(label),
            ..
        } if **variable == *score.variable => label.clone(),
        _ => return false,
    };
    if !stats.is_vertex_property_text_indexed_for(Some(&seed_label), &score.property) {
        return false;
    }

    let text_scan = PlanOp::TextScan {
        variable: score.variable.as_str().into(),
        label: seed_label,
        property: score.property.as_str().into(),
        query: score.query.clone(),
        mode: TextScanMode::TopK {
            limit: ScanValue::Literal(gleaph_gql::Value::Int64(k)),
        },
        property_projection: None,
    };
    ops[0] = text_scan;
    // Drop the TopK op; the TextScan delivers deterministic (score DESC, key ASC) order.
    ops.remove(topk_idx);
    true
}

fn const_int64(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Literal(gleaph_gql::Value::Int64(value)) => Some(*value),
        _ => None,
    }
}

/// Find the first WHERE conjunct that lowers into a full-text threshold seed for a
/// newly-bound, labeled node of this pattern with stats-confirmed TEXT coverage.
///
/// Returns the conjunct's position together with the resolved seed. The caller removes
/// the conjunct and stashes the seed in the plan annotations; if the seed is never
/// consumed by a seed scan, the conjunct must be restored so the predicate cannot be
/// silently dropped.
pub(crate) fn find_text_threshold_seed(
    where_conjuncts: &[Expr],
    pattern: &gleaph_gql::ast::GraphPattern,
    already_bound: &BTreeSet<String>,
    stats: Option<&dyn GraphStats>,
) -> Option<(usize, TextSeedInfo)> {
    let labeled_nodes = collect_labeled_nodes(pattern);
    for (idx, conjunct) in where_conjuncts.iter().enumerate() {
        let Some((score, cmp, bound)) = extract_threshold_predicate(conjunct) else {
            continue;
        };
        if already_bound.contains(&score.variable) {
            continue;
        }
        let Some(label) = labeled_nodes.get(&score.variable) else {
            continue;
        };
        if !stats?.is_vertex_property_text_indexed_for(Some(label), &score.property) {
            continue;
        }
        return Some((
            idx,
            TextSeedInfo {
                variable: score.variable.as_str().into(),
                label: crate::plan::NodeLabelRef::from(label.as_str()),
                property: score.property.as_str().into(),
                query: score.query,
                cmp,
                bound,
            },
        ));
    }
    None
}

/// Collect `variable → simple label` for every labeled node pattern variable.
fn collect_labeled_nodes(pattern: &gleaph_gql::ast::GraphPattern) -> BTreeMap<String, String> {
    fn walk_expr(expr: &gleaph_gql::ast::PathPatternExpr, out: &mut BTreeMap<String, String>) {
        match expr {
            gleaph_gql::ast::PathPatternExpr::Term(term) => walk_term(term, out),
            gleaph_gql::ast::PathPatternExpr::MultisetAlternation(terms)
            | gleaph_gql::ast::PathPatternExpr::PatternUnion(terms) => {
                for term in terms {
                    walk_term(term, out);
                }
            }
        }
    }
    fn walk_term(term: &gleaph_gql::ast::PathTerm, out: &mut BTreeMap<String, String>) {
        for factor in &term.factors {
            match &factor.primary {
                gleaph_gql::ast::PathPrimary::Node(node) => {
                    if let (Some(var), Some(label)) = (
                        &node.variable,
                        crate::anchor::extract_simple_label(&node.label),
                    ) {
                        out.insert(var.clone(), label);
                    }
                }
                gleaph_gql::ast::PathPrimary::Parenthesized { expr, .. } => walk_expr(expr, out),
                gleaph_gql::ast::PathPrimary::Edge(_)
                | gleaph_gql::ast::PathPrimary::Simplified(_) => {}
            }
        }
    }
    let mut out = BTreeMap::new();
    for path in &pattern.paths {
        walk_expr(&path.expr, &mut out);
    }
    out
}
