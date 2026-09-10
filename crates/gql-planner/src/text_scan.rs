//! Planner lowering of the scalar `text_score(prop, query)` contract (plan 0297).
//!
//! `text_score(…)` is a relevance score, so it can only be produced by a scan that
//! consulted the TEXT index — never re-evaluated per row. Two shapes lower into
//! [`PlanOp::TextScan`]:
//!
//! - threshold: a WHERE conjunct `text_score(v.prop, Q) > t` (or `>=`, or reversed);
//! - top-k: `ORDER BY text_score(v.prop, Q) DESC LIMIT k`;
//! - compound threshold-top-k (plan 0329): the combined shape above fuses into ONE scan
//!   with [`TextScanMode::ThresholdTopK`] when both halves reference the same
//!   `(variable, property, query)` — the scan retains the threshold on the score-ranked
//!   window, then truncates to the literal limit;
//! - candidate-scoped compound: the same fusion as a ranking barrier AFTER a traversal
//!   prefix (single threshold `PropertyFilter` + `TopK` on one triple), reusing
//!   [`TextScanMode::ThresholdTopK`] with no new wire form.
//!
//! Every other placement fails closed at plan validation: an unfused `text_score`
//! expression rejects the plan instead of falling back to a sequential scan.

use gleaph_gql::ast::{CmpOp, Expr, ExprKind, OrderByClause};
use gleaph_gql::types::LabelExpr;
use std::collections::{BTreeMap, BTreeSet};

use crate::expr_children::for_each_immediate_child_expr;
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
/// ASC is deliberately deferred (least-relevant top-k, no demand); reopen on a concrete use case.
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
    // Compound path (plan 0329), ordered before the NodeScan path: a drained WHERE
    // threshold seed scan (`TextScan { Threshold }`) already sits at ops[0], so the TopK
    // may fuse into it when both halves reference the same (variable, property, query).
    // Coverage was enforced at seed time by `find_text_threshold_seed` — no second stats
    // check; the equality match below already excludes cross-property/cross-query fusion
    // (a mismatch returns false and the TopK mention stays residual → fail closed).
    if let PlanOp::TextScan {
        variable,
        property,
        query,
        mode: mode @ TextScanMode::Threshold { .. },
        ..
    } = &mut ops[0]
    {
        if **variable == *score.variable && **property == *score.property && *query == score.query {
            let TextScanMode::Threshold { cmp, bound } = mode else {
                unreachable!("matched above");
            };
            *mode = TextScanMode::ThresholdTopK {
                cmp: *cmp,
                bound: bound.clone(),
                limit: ScanValue::Literal(gleaph_gql::Value::Int64(k)),
            };
            ops.remove(topk_idx);
            return true;
        }
        // A surviving leading `TextScan { Threshold }` whose halves disagree on any of
        // (variable, property, query) never fuses: the TopK mention stays residual.
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

/// Recursive `text_score(...)` mention check over the canonical immediate-child
/// traversal, so every expression container (including future ones) is covered by
/// construction.
fn expr_mentions_text_score(expr: &Expr) -> bool {
    if is_text_score_call(expr) {
        return true;
    }
    let mut found = false;
    for_each_immediate_child_expr(expr, |child| {
        if !found {
            found = expr_mentions_text_score(child);
        }
    });
    found
}

fn order_mentions_text_score(order_by: &OrderByClause) -> bool {
    order_by
        .items
        .iter()
        .any(|item| expr_mentions_text_score(&item.expr))
}

/// Whether this operator carries any expression-level `text_score` mention.
/// `TextScan` itself is the sanctioned form and is intentionally not a hit.
fn op_mentions_text_score(op: &PlanOp) -> bool {
    match op {
        PlanOp::TextScan { .. } => false,
        PlanOp::PropertyFilter { predicates, .. }
        | PlanOp::ExpandFilter {
            dst_filter: predicates,
            ..
        } => predicates.iter().any(expr_mentions_text_score),
        PlanOp::Let { bindings } => bindings.iter().any(|b| expr_mentions_text_score(&b.value)),
        PlanOp::For { list, .. } => expr_mentions_text_score(list),
        PlanOp::Filter { condition } => expr_mentions_text_score(condition),
        PlanOp::CallProcedure { args, .. } => args.iter().any(expr_mentions_text_score),
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            group_by.iter().any(expr_mentions_text_score)
                || aggregates.iter().any(|spec| {
                    spec.expr.as_ref().is_some_and(expr_mentions_text_score)
                        || spec.expr2.as_ref().is_some_and(expr_mentions_text_score)
                })
        }
        PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => columns
            .iter()
            .any(|col| expr_mentions_text_score(&col.expr)),
        PlanOp::Sort { order_by } => order_mentions_text_score(order_by),
        PlanOp::TopK {
            order_by,
            k,
            offset,
            ..
        } => {
            order_mentions_text_score(order_by)
                || expr_mentions_text_score(k)
                || offset.as_ref().is_some_and(expr_mentions_text_score)
        }
        PlanOp::Limit { count, offset } => {
            count.as_ref().is_some_and(expr_mentions_text_score)
                || offset.as_ref().is_some_and(expr_mentions_text_score)
        }
        PlanOp::ShortestPath { cost, .. } => match cost {
            crate::plan::ShortestPathCost::HopCount => false,
            crate::plan::ShortestPathCost::EdgeCostExpr { expr, .. } => {
                expr_mentions_text_score(expr)
            }
        },
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            left.iter().any(op_mentions_text_score) || right.iter().any(op_mentions_text_score)
        }
        PlanOp::SetOperation { right, .. } => right.ops.iter().any(op_mentions_text_score),
        PlanOp::OptionalMatch { sub_plan } | PlanOp::SemiApply { sub_plan, .. } => {
            sub_plan.iter().any(op_mentions_text_score)
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            sub_plan.ops.iter().any(op_mentions_text_score)
        }
        PlanOp::UseGraph {
            sub_plan: Some(sp), ..
        } => sp.iter().any(op_mentions_text_score),
        _ => false,
    }
}

/// Collect the single statically proven label for `variable` from its bindings in the
/// prefix: a labeled `NodeScan`, or an `ExpandFilter` destination guarded by a
/// non-negated `IS LABELED <simple name>` conjunct. Returns `None` when the variable
/// is unbound, ambiguously labeled, or only guarded by a compound label expression.
///
/// Shared with the Router candidate barrier (`gql_text_scan`): the second variable of
/// a two-variable dual-score shape never lowers to a `TextScan`, so the Router
/// re-derives its label from the barrier-free prefix with this same helper.
/// Generic plan analysis — no execution or storage assumptions.
pub fn proven_prefix_label(prefix: &[PlanOp], variable: &str) -> Option<String> {
    fn simple_label_expr(label: &LabelExpr) -> Option<String> {
        match label {
            LabelExpr::Name(name) => Some(name.clone()),
            _ => None,
        }
    }
    fn is_labeled_name(expr: &Expr, variable: &str) -> Option<String> {
        match &expr.kind {
            ExprKind::IsLabeled {
                expr,
                label,
                negated,
            } if !negated => match &expr.kind {
                ExprKind::Variable(v) if v == variable => simple_label_expr(label),
                _ => None,
            },
            _ => None,
        }
    }
    let mut labels: BTreeSet<String> = BTreeSet::new();
    let mut bound = false;
    for op in prefix {
        match op {
            PlanOp::NodeScan {
                variable: v, label, ..
            } if v.as_ref() == variable => {
                bound = true;
                let Some(l) = label else {
                    return None;
                };
                labels.insert(l.to_string());
            }
            PlanOp::ExpandFilter {
                dst, dst_filter, ..
            } if dst.as_ref() == variable => {
                bound = true;
                let mut guarded = false;
                for predicate in dst_filter {
                    if let Some(name) = is_labeled_name(predicate, variable) {
                        labels.insert(name);
                        guarded = true;
                    }
                }
                if !guarded {
                    return None;
                }
            }
            _ => {}
        }
    }
    if !bound || labels.len() != 1 {
        return None;
    }
    labels.into_iter().next()
}

/// Candidate-scoped top-k lowering (plan 0344): rewrite
/// `[candidate prefix binding v, …, TopK { text_score(v.prop, Q) DESC, k }, Project]`
/// into `[candidate prefix binding v, …, TextScan { mode: TopK, … }, Project]`.
///
/// The `TextScan` sits AFTER the traversal prefix as a ranking barrier: the Router
/// executes the fully authorized prefix first, scores only those candidates in TEXT,
/// then applies the row LIMIT. Unlike the leading lowering, intervening traversal ops
/// (`ExpandFilter`, …) are the point — but `Limit`/`TopK` inside the prefix, a second
/// `TopK`, a non-`Project` tail, an unproved label, or missing TEXT coverage
/// all refuse to lower so the residual mention fails closed downstream.
///
/// A fused OFFSET rides in the consumed `TopK`: the barrier scan carries the
/// inflated `k + skip` ranking window and a pure trailing skip-`Limit` parks after
/// the late-projected RETURN, so the Router skips after ranking (never before).
///
/// The trailing `TopK` is consumed into the scan (no `Limit` survives in the plan), so
/// limit pushdown — which runs before this pass and bails on `Sort` — cannot narrow the
/// candidate prefix through the barrier.
/// Trailing skip for a fused OFFSET: a pure `Limit { count: None, offset }` parked
/// after the late-projected RETURN. The Router applies the skip after ranking; the
/// barrier scan carries the inflated `k + skip` window so ranking stays exact.
/// Emitted only for a positive skip — `OFFSET 0` lowers to today's offset-free shape.
fn skip_limit_op(skip: i64) -> PlanOp {
    PlanOp::Limit {
        count: None,
        offset: Some(Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(skip)))),
    }
}

/// Find the compound partner for a candidate TopK: exactly one single-predicate
/// `PropertyFilter` in the prefix whose predicate extracts as a threshold on the
/// same (variable, property, query) triple as the TopK score. Returns the filter
/// index, comparison, and bound. Zero partners, two partners, a mismatched triple,
/// or a multi-predicate filter yields `None`, so both halves stay residual and
/// fail closed at validation.
fn find_compound_threshold_filter(
    prefix: &[PlanOp],
    score: &TextScoreRef,
) -> Option<(usize, CmpOp, ScanValue)> {
    let mut found = None;
    for (idx, op) in prefix.iter().enumerate() {
        let PlanOp::PropertyFilter { predicates, .. } = op else {
            continue;
        };
        if predicates.len() != 1 {
            continue;
        }
        let Some((candidate, cmp, bound)) = extract_threshold_predicate(&predicates[0]) else {
            continue;
        };
        if candidate.variable != score.variable
            || candidate.property != score.property
            || candidate.query != score.query
        {
            return None;
        }
        if found.is_some() {
            return None;
        }
        found = Some((idx, cmp, bound));
    }
    found
}

pub(crate) fn apply_candidate_text_topk_lowering(
    ops: &mut Vec<PlanOp>,
    stats: Option<&dyn GraphStats>,
) -> bool {
    // `Vec` (not a slice): the compound path removes the fused threshold filter.
    let Some(stats) = stats else {
        return false;
    };
    let topk_positions: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| matches!(op, PlanOp::TopK { .. }))
        .map(|(idx, _)| idx)
        .collect();
    if topk_positions.len() != 1 {
        return false;
    }
    let topk_idx = topk_positions[0];
    if topk_idx == 0 {
        return false;
    }
    let (score, window, skip) = {
        let PlanOp::TopK {
            order_by,
            k,
            offset,
            ..
        } = &ops[topk_idx]
        else {
            return false;
        };
        let Some(k_value) = const_int64(k) else {
            return false;
        };
        if k_value <= 0 {
            return false;
        }
        // A fused OFFSET is literal-only (parity with the limit): negative,
        // non-literal, or overflowing `k + skip` refuses to lower so the
        // residual mention fails closed downstream. The 1024-row admission cap
        // stays owned by the Router gate — no cap constant is copied here.
        let skip_value = match offset {
            None => 0,
            Some(expr) => match const_int64(expr) {
                Some(n) if n >= 0 => n,
                _ => return false,
            },
        };
        let Some(window) = k_value.checked_add(skip_value) else {
            return false;
        };
        match extract_topk_order(order_by) {
            Some(score) => (score, window, skip_value),
            None => return false,
        }
    };
    // The tail after the TopK must be exactly the late-projected RETURN.
    if ops.len() != topk_idx + 2 || !matches!(ops[topk_idx + 1], PlanOp::Project { .. }) {
        return false;
    }
    // Compound path (candidate-scoped plan 0329): a single threshold
    // `PropertyFilter` on the same (variable, property, query) triple fuses with
    // the TopK into ONE barrier. Ordered before the mention-free check below:
    // any other score mention in the prefix stays residual and fails closed.
    if let Some((filter_idx, cmp, bound)) = find_compound_threshold_filter(&ops[..topk_idx], &score)
    {
        let prefix_clean = ops[..topk_idx].iter().enumerate().all(|(idx, op)| {
            idx == filter_idx
                || !(matches!(
                    op,
                    PlanOp::TextScan { .. } | PlanOp::Limit { .. } | PlanOp::TopK { .. }
                ) || op_mentions_text_score(op))
        });
        if !prefix_clean {
            return false;
        }
        let prefix = &ops[..topk_idx];
        let Some(label) = proven_prefix_label(prefix, &score.variable) else {
            return false;
        };
        if !stats.is_vertex_property_text_indexed_for(Some(&label), &score.property) {
            return false;
        }
        let text_scan = PlanOp::TextScan {
            variable: score.variable.as_str().into(),
            label: label.as_str().into(),
            property: score.property.as_str().into(),
            query: score.query.clone(),
            mode: TextScanMode::ThresholdTopK {
                cmp,
                bound,
                limit: ScanValue::Literal(gleaph_gql::Value::Int64(window)),
            },
            property_projection: None,
        };
        ops.remove(filter_idx);
        // The filter sat strictly before the TopK, so the barrier slides one slot
        // and the RETURN Project now sits at `topk_idx`; the skip parks after it.
        ops[topk_idx - 1] = text_scan;
        if skip > 0 {
            ops.insert(topk_idx + 1, skip_limit_op(skip));
        }
        return true;
    }
    let prefix = &ops[..topk_idx];
    // The prefix must not contain a scan, a row cap, or any other score mention:
    // candidate membership is the complete authorized prefix, never a window.
    if prefix.iter().any(|op| {
        matches!(
            op,
            PlanOp::TextScan { .. } | PlanOp::Limit { .. } | PlanOp::TopK { .. }
        ) || op_mentions_text_score(op)
    }) {
        return false;
    }
    let Some(label) = proven_prefix_label(prefix, &score.variable) else {
        return false;
    };
    if !stats.is_vertex_property_text_indexed_for(Some(&label), &score.property) {
        return false;
    }
    let text_scan = PlanOp::TextScan {
        variable: score.variable.as_str().into(),
        label: label.as_str().into(),
        property: score.property.as_str().into(),
        query: score.query.clone(),
        mode: TextScanMode::TopK {
            limit: ScanValue::Literal(gleaph_gql::Value::Int64(window)),
        },
        property_projection: None,
    };
    ops[topk_idx] = text_scan;
    if skip > 0 {
        ops.insert(topk_idx + 2, skip_limit_op(skip));
    }
    true
}

/// Post-pass candidate threshold lowering: rewrite
/// `[prefix binding v, …, PropertyFilter { [text_score(v.prop, Q) cmp bound] }, Project]`
/// into `[prefix, TextScan { mode: Threshold, … }, Project]`.
///
/// The symmetric counterpart of [`apply_candidate_text_topk_lowering`] for the
/// non-leading `WHERE text_score(…) > t` shape: the barrier sits AFTER the
/// traversal prefix, so TEXT scores only graph-qualified candidates. The single
/// threshold predicate is consumed; compound filters, offsets, and score-ordered
/// tails stay unsupported and fail closed. The trailing `Project` need not
/// project the score call — a threshold-only `RETURN` keeps every surviving row
/// without a residual score column.
///
/// The predicate arrives as a late-stage `PropertyFilter` (filter pushdown only
/// absorbs `IsLabeled` into `ExpandFilter` destinations), so the barrier takes
/// exactly one single-predicate `PropertyFilter` whose predicate extracts.
pub(crate) fn apply_candidate_text_threshold_lowering(
    ops: &mut [PlanOp],
    stats: Option<&dyn GraphStats>,
) -> bool {
    let Some(stats) = stats else {
        return false;
    };
    // The barrier candidate is a single-predicate `PropertyFilter` whose predicate
    // extracts as a threshold (other single-predicate filters, such as the anchor
    // equality, stay in the prefix). Exactly one must exist.
    let filter_positions: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| match op {
            PlanOp::PropertyFilter { predicates, .. } if predicates.len() == 1 => {
                extract_threshold_predicate(&predicates[0]).is_some()
            }
            _ => false,
        })
        .map(|(idx, _)| idx)
        .collect();
    if filter_positions.len() != 1 {
        return false;
    }
    let filter_idx = filter_positions[0];
    if filter_idx == 0 {
        return false;
    }
    let (score, cmp, bound) = {
        let PlanOp::PropertyFilter { predicates, .. } = &ops[filter_idx] else {
            unreachable!("checked above");
        };
        match extract_threshold_predicate(&predicates[0]) {
            Some(seed) => seed,
            None => return false,
        }
    };
    // The tail after the barrier must be exactly the late-projected RETURN.
    if ops.len() != filter_idx + 2 || !matches!(ops[filter_idx + 1], PlanOp::Project { .. }) {
        return false;
    }
    let prefix = &ops[..filter_idx];
    // The prefix must not contain a scan, a row cap, or any other score mention:
    // candidate membership is the complete authorized prefix, never a window.
    if prefix.iter().any(|op| {
        matches!(
            op,
            PlanOp::TextScan { .. } | PlanOp::Limit { .. } | PlanOp::TopK { .. }
        ) || op_mentions_text_score(op)
    }) {
        return false;
    }
    let Some(label) = proven_prefix_label(prefix, &score.variable) else {
        return false;
    };
    if !stats.is_vertex_property_text_indexed_for(Some(&label), &score.property) {
        return false;
    }
    let text_scan = PlanOp::TextScan {
        variable: score.variable.as_str().into(),
        label: label.as_str().into(),
        property: score.property.as_str().into(),
        query: score.query.clone(),
        mode: TextScanMode::Threshold { cmp, bound },
        property_projection: None,
    };
    ops[filter_idx] = text_scan;
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
