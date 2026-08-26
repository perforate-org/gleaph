//! Router-side execution of planner-lowered [`PlanOp::TextScan`] seed ops (plan 0297).
//!
//! Division of labor (plan 0297 planner-lowering): `gleaph-gql`/`gql-planner` lower covered
//! `text_score(...)` uses into a leading [`PlanOp::TextScan`] with a structured
//! [`TextScanMode`] — this module is the Router-owned execution authority for that op. It
//! resolves the definition against the TEXT catalog (`Ready` + attached canister only;
//! anything else resolves as "function unknown" fail-closed), dispatches the definition
//! canister's `search` endpoint as a same-subnet composite query, merges hits under the
//! deterministic `(score desc, key asc)` contract, and seeds the plan flow exactly where the
//! op sits. A projected aliased `text_score` call that survived lowering is rewritten to the
//! seeded score binding, so the graph executor never evaluates the function.
//!
//! Both landed modes execute: `TopK { limit }` delivers the top-k ranked prefix; `Threshold
//! { cmp, bound }` keeps the hits whose engine score satisfies the comparison inside the
//! bounded search window (completeness beyond the window is marked truncated).

use std::collections::{BTreeMap, HashSet};

use gleaph_gql::ast::{CmpOp, Expr, ExprKind};
use gleaph_gql_planner::expr_children::for_each_immediate_child_expr;
use gleaph_gql_planner::plan::{
    NodeLabelRef, PhysicalPlan, PlanOp, ProjectColumn, ScanValue, TextScanMode,
};
use gleaph_graph_kernel::entry::{GraphId, VertexLabelId};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::plan_exec::{
    GqlExecutionMode, GqlQueryResult, SeedBindingsWire, SeedFloat64Binding, SeedRowWire,
    SeedVertexBinding,
};

use crate::RouterStore;
use crate::facade::stable::text_index_catalog::{self, TextIndexDefRecord};
use crate::gql_search::cap_result_rows;
use crate::planner_stats::RouterGraphStats;
use crate::state::RouterError;

/// Method name on the text canister (`#[query] fn search(query: String, k: u32)`).
#[cfg(target_family = "wasm")]
const TEXT_SEARCH_METHOD: &str = "search";

/// Mirrors the text canister's `MAX_SEARCH_K` clamp (`crates/text-canister/src/state.rs`);
/// requests above this width stay legal but cannot be satisfied completely.
const MAX_TEXT_SEARCH_K: u32 = 100;

// ════════════════════════════════════════════════════════════════════════════════
// Entry point
// ════════════════════════════════════════════════════════════════════════════════

/// Execute a read plan whose leading operator is a lowered [`PlanOp::TextScan`].
///
/// Returns `Ok(None)` when the plan contains no `TextScan`, letting the caller fall through
/// to normal dispatch. An unlowered residual `text_score` mention (no scan) fails closed
/// here with an explicit unsupported-shape error instead of reaching the graph executor.
pub(crate) async fn try_execute_gql_text_scan(
    plan: &PhysicalPlan,
    graph_id: GraphId,
    params_blob: &[u8],
    mode: GqlExecutionMode,
    stats: &RouterGraphStats,
    store: &RouterStore,
) -> Result<Option<GqlQueryResult>, RouterError> {
    let has_scan = ops_contain_text_scan(&plan.ops);
    if !has_scan && !plan_mentions_text_score(plan) {
        return Ok(None);
    }

    if mode != GqlExecutionMode::Query {
        return Err(RouterError::InvalidArgument(
            "text_score lowering only supports query mode in this slice".into(),
        ));
    }
    if plan.has_dml() {
        return Err(RouterError::InvalidArgument(
            "text_score is not supported in mutation programs in this slice".into(),
        ));
    }
    if !has_scan {
        return Err(RouterError::InvalidArgument(
            "text_score requires a covered TEXT index scan; this shape did not lower into a TextScan and is not supported in this slice".into(),
        ));
    }

    let shape = analyze_text_scan_shape(plan, graph_id, store)?;
    let params = gleaph_gql_ic::wire::decode_gql_params_blob(params_blob).map_err(|e| {
        RouterError::InvalidArgument(format!("failed to decode GQL parameters: {e}"))
    })?;
    let query = resolve_scan_query(&shape.query, &params)?;

    // Planning-visible definitions are always `Ready` with an attached canister; anything
    // else is absent by contract, which resolves as "function unknown" fail-closed.
    let def = resolve_text_index(graph_id, shape.label_id, shape.property_id)?;
    let target = def
        .target
        .expect("planning-visible text definitions always carry a target");

    // One text canister serves exactly its home shard's doc-key space, so multi-shard
    // fan-out stays deferred rather than silently returning partial results.
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    let [shard] = shards.as_slice() else {
        return Err(RouterError::Conflict(format!(
            "text_score requires a single live shard for this graph; {} live shards are not supported until multi-shard text fan-out lands",
            shards.len()
        )));
    };

    // Fetch candidates inside the bounded canister window, then apply the mode.
    let mut hits = text_canister_search(target, query.clone(), MAX_TEXT_SEARCH_K)
        .await
        .map_err(RouterError::Internal)?;
    sort_hits_deterministically(&mut hits);
    let raw_window = hits.len();
    let mut truncated = raw_window == MAX_TEXT_SEARCH_K as usize;
    let requested_top_k: Option<u32> = match &shape.mode {
        TextScanMode::TopK { limit } => {
            let requested_k = resolve_scan_limit(limit, &params)?;
            let request_k = requested_k.min(MAX_TEXT_SEARCH_K);
            hits.truncate(request_k as usize);
            // A LIMIT beyond the canister clamp cannot be satisfied completely.
            truncated |= requested_k > MAX_TEXT_SEARCH_K;
            Some(requested_k)
        }
        TextScanMode::Threshold { cmp, bound } => {
            let bound = resolve_scan_bound(bound, &params)?;
            retain_threshold(&mut hits, *cmp, bound);
            None
        }
    };

    let seeds_by_shard = build_text_scan_seeds(
        &shape.binding,
        shape.alias.as_deref(),
        &[def.label_id],
        shard.shard_id,
        &hits,
    )?;

    let executable_plan =
        rewrite_executable_plan(plan, shape.project_op_index, shape.alias.as_deref())?;
    let executable_plan_blob =
        gleaph_gql_planner::wire::encode_block_plans(std::slice::from_ref(&executable_plan), false)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

    let mut result = crate::gql_search::dispatch_search_read_plan(
        graph_id,
        plan,
        &executable_plan_blob,
        &executable_plan,
        seeds_by_shard,
        params_blob,
        mode,
        stats,
        store,
    )
    .await?;
    if let Some(k) = requested_top_k {
        cap_result_rows(&mut result, k)?;
    }

    Ok(Some(if truncated {
        result.with_truncated(true)
    } else {
        result.with_truncated(false)
    }))
}

// ════════════════════════════════════════════════════════════════════════════════
// Detection
// ════════════════════════════════════════════════════════════════════════════════

/// Total over every read-path operator: direct containers plus nested sub-plans. DML
/// operators are unreachable (the caller rejects mutating plans before analysis).
fn ops_contain_text_scan(ops: &[PlanOp]) -> bool {
    ops.iter().any(|op| match op {
        PlanOp::TextScan { .. } => true,
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            ops_contain_text_scan(left) || ops_contain_text_scan(right)
        }
        PlanOp::SetOperation { right, .. } => ops_contain_text_scan(&right.ops),
        PlanOp::OptionalMatch { sub_plan } | PlanOp::SemiApply { sub_plan, .. } => {
            ops_contain_text_scan(sub_plan)
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => ops_contain_text_scan(&sub_plan.ops),
        PlanOp::UseGraph {
            sub_plan: Some(sp), ..
        } => ops_contain_text_scan(sp),
        _ => false,
    })
}

/// True iff `expr` is syntactically a `text_score(...)` call (unqualified name).
fn is_text_score_call(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::FunctionCall { name, .. }
        if name.parts.len() == 1
            && name.parts[0].eq_ignore_ascii_case("text_score"))
}

/// Recursive detection over the canonical immediate-child traversal, so every expression
/// container (including future ones) is covered by construction.
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

fn sort_items_mention(order_by: &gleaph_gql::ast::OrderByClause) -> bool {
    order_by
        .items
        .iter()
        .any(|item| expr_mentions_text_score(&item.expr))
}

/// Whether this specific operator carries any expression-level `text_score` mention
/// (`TextScan` itself is the sanctioned form and is intentionally not an expression hit).
fn op_mention_text_score(op: &PlanOp) -> bool {
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
        PlanOp::Sort { order_by } => sort_items_mention(order_by),
        PlanOp::TopK {
            order_by,
            k,
            offset,
        } => {
            sort_items_mention(order_by)
                || expr_mentions_text_score(k)
                || offset.as_ref().is_some_and(expr_mentions_text_score)
        }
        PlanOp::Limit { count, offset } => {
            count.as_ref().is_some_and(expr_mentions_text_score)
                || offset.as_ref().is_some_and(expr_mentions_text_score)
        }
        PlanOp::ShortestPath { cost, .. } => match cost {
            gleaph_gql_planner::plan::ShortestPathCost::HopCount => false,
            gleaph_gql_planner::plan::ShortestPathCost::EdgeCostExpr { expr, .. } => {
                expr_mentions_text_score(expr)
            }
        },
        PlanOp::SemiApply {
            terminal_predicates,
            sub_plan,
            ..
        } => {
            terminal_predicates.iter().any(expr_mentions_text_score)
                || ops_contain_text_scan(sub_plan)
                || sub_plan.iter().any(op_mention_text_score)
        }
        _ => false,
    }
}

pub(crate) fn plan_mentions_text_score(plan: &PhysicalPlan) -> bool {
    plan.ops.iter().any(op_mention_text_score)
}

// ════════════════════════════════════════════════════════════════════════════════
// Shape analysis
// ════════════════════════════════════════════════════════════════════════════════

/// One accepted shape: a single LEADING `TextScan` (the only position the planner emits)
/// plus at most one residual projected aliased call that this layer rewrites onto the
/// seeded score binding. Everything else fails closed with its container named.
#[derive(Debug)]
struct TextScanShape {
    /// Leading `TextScan` variable whose vertices the seed binds.
    binding: String,
    /// Resolved id of the scan label (seed requirement + definition matching).
    label_id: VertexLabelId,
    /// Indexed property named by the scan.
    property_id: gleaph_graph_kernel::entry::PropertyId,
    /// Structured scan mode from the planner (`TopK` / `Threshold`).
    mode: TextScanMode,
    /// Scan query: a TEXT literal or `$param` reference.
    query: ScanValue,
    /// User-visible alias of a residual projected call, bound by the seed.
    alias: Option<String>,
    /// Index (in `plan.ops`) of the `Project` carrying the residual call, if any.
    project_op_index: Option<usize>,
}

fn analyze_text_scan_shape(
    plan: &PhysicalPlan,
    graph_id: GraphId,
    store: &RouterStore,
) -> Result<TextScanShape, RouterError> {
    let Some((scan @ PlanOp::TextScan { .. }, tail)) = plan.ops.split_first() else {
        return Err(RouterError::InvalidArgument(
            "TextScan must be the leading scan operator in this slice".into(),
        ));
    };
    let PlanOp::TextScan {
        variable,
        label,
        property,
        query,
        mode,
        ..
    } = scan
    else {
        unreachable!("split_first matched TextScan");
    };
    if tail.iter().any(|op| matches!(op, PlanOp::TextScan { .. })) {
        return Err(RouterError::InvalidArgument(
            "TextScan must appear exactly once in this slice".into(),
        ));
    }

    let label_id = resolve_vertex_label_id(graph_id, store, label)?;
    let property_id = store
        .lookup_property_id(graph_id, property)
        .map_err(|e| RouterError::NotFound(format!("text_score property {property}: {e}")))?;

    // Residual mentions after the scan: exactly one projected aliased call referencing the
    // scanned (variable, property) may survive; every other placement fails closed.
    let mut alias: Option<String> = None;
    let mut project_op_index: Option<usize> = None;
    for (idx, op) in tail.iter().enumerate() {
        if nested_subplan_mentions(op) {
            return Err(RouterError::InvalidArgument(
                "text_score inside sub-plans is not supported in this slice".into(),
            ));
        }
        if !op_mention_text_score(op) {
            continue;
        }
        let PlanOp::Project { columns, .. } = op else {
            return Err(RouterError::InvalidArgument(
                "residual text_score references are only supported as projected expressions in this slice".into(),
            ));
        };
        for col in columns {
            if !expr_mentions_text_score(&col.expr) {
                continue;
            }
            if !is_text_score_call(&col.expr) {
                return Err(RouterError::InvalidArgument(
                    "residual text_score must be the entire projected expression in this slice"
                        .into(),
                ));
            }
            if alias.is_some() {
                return Err(RouterError::InvalidArgument(
                    "text_score must appear exactly once; reference the existing alias instead"
                        .into(),
                ));
            }
            let col_alias = col.alias.as_ref().ok_or_else(|| {
                RouterError::InvalidArgument(
                    "residual text_score projection requires an alias (`... AS score`)".into(),
                )
            })?;
            let (call_var, call_property) = extract_call_target(&col.expr)?;
            if call_var != variable.as_ref() || call_property != property.as_ref() {
                return Err(RouterError::InvalidArgument(format!(
                    "residual text_score must reference the scanned `{variable}.{property}`"
                )));
            }
            alias = Some(col_alias.to_string());
            project_op_index = Some(idx + 1);
        }
    }

    Ok(TextScanShape {
        binding: variable.to_string(),
        label_id,
        property_id,
        mode: mode.clone(),
        query: query.clone(),
        alias,
        project_op_index,
    })
}

fn nested_subplan_mentions(op: &PlanOp) -> bool {
    match op {
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            ops_contain_text_scan(left)
                || ops_contain_text_scan(right)
                || left.iter().any(op_mention_text_score)
                || right.iter().any(op_mention_text_score)
        }
        PlanOp::SetOperation { right, .. } => {
            ops_contain_text_scan(&right.ops) || right.ops.iter().any(op_mention_text_score)
        }
        PlanOp::OptionalMatch { sub_plan } | PlanOp::SemiApply { sub_plan, .. } => {
            ops_contain_text_scan(sub_plan) || sub_plan.iter().any(op_mention_text_score)
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            ops_contain_text_scan(&sub_plan.ops) || sub_plan.ops.iter().any(op_mention_text_score)
        }
        PlanOp::UseGraph {
            sub_plan: Some(sp), ..
        } => ops_contain_text_scan(sp) || sp.iter().any(op_mention_text_score),
        _ => false,
    }
}

/// Extracts `(variable, property)` from a detected `text_score(v.prop, ...)` call.
fn extract_call_target(expr: &Expr) -> Result<(String, String), RouterError> {
    let ExprKind::FunctionCall { args, distinct, .. } = &expr.kind else {
        unreachable!("extract_call_target is only called on detected calls");
    };
    if *distinct {
        return Err(RouterError::InvalidArgument(
            "text_score does not accept DISTINCT".into(),
        ));
    }
    let [prop_arg, query_arg] = args.as_slice() else {
        return Err(RouterError::InvalidArgument(format!(
            "text_score expects exactly 2 arguments (property access, query); got {}",
            args.len()
        )));
    };
    let ExprKind::PropertyAccess {
        expr: target,
        property,
    } = &prop_arg.kind
    else {
        return Err(RouterError::InvalidArgument(
            "text_score first argument must be `<variable>.<property>`".into(),
        ));
    };
    let ExprKind::Variable(variable) = &target.kind else {
        return Err(RouterError::InvalidArgument(
            "text_score first argument must be a property access rooted at a variable".into(),
        ));
    };
    // The query argument must stay a literal-or-parameter shape even though the scan owns
    // the authoritative copy, so a rewritten column cannot drift from its scan.
    if !matches!(
        query_arg.kind,
        ExprKind::Literal(_) | ExprKind::Parameter(_)
    ) {
        return Err(RouterError::InvalidArgument(
            "text_score query must be a string literal or parameter".into(),
        ));
    }
    Ok((variable.clone(), property.clone()))
}

fn resolve_vertex_label_id(
    graph_id: GraphId,
    store: &RouterStore,
    label: &NodeLabelRef,
) -> Result<VertexLabelId, RouterError> {
    store
        .lookup_vertex_label_id(graph_id, label.as_ref())
        .map_err(|e| RouterError::InvalidArgument(format!("label {}: {e}", label.as_ref())))
}

// ════════════════════════════════════════════════════════════════════════════════
// ScanValue resolution (query / limit / threshold bound)
// ════════════════════════════════════════════════════════════════════════════════

fn scan_value_param_name(value: &ScanValue) -> Option<&str> {
    match value {
        ScanValue::Parameter(name) => Some(name.strip_prefix('$').unwrap_or(name.as_ref())),
        _ => None,
    }
}

fn resolve_scan_query(
    value: &ScanValue,
    params: &BTreeMap<String, gleaph_gql::Value>,
) -> Result<String, RouterError> {
    let text = match value {
        ScanValue::Literal(gleaph_gql::Value::Text(text)) => text.clone(),
        ScanValue::Parameter(_) => {
            let key = scan_value_param_name(value).expect("parameter checked above");
            match params.get(key) {
                Some(gleaph_gql::Value::Text(text)) => text.clone(),
                Some(_) => {
                    return Err(RouterError::InvalidArgument(format!(
                        "text_score query parameter ${key} must evaluate to a string"
                    )));
                }
                None => {
                    return Err(RouterError::InvalidArgument(format!(
                        "missing parameter ${key}"
                    )));
                }
            }
        }
        _ => {
            return Err(RouterError::InvalidArgument(
                "TextScan query must be a TEXT literal or parameter".into(),
            ));
        }
    };
    Ok(text)
}

fn positive_u64(value: &gleaph_gql::Value) -> Option<u64> {
    match value {
        gleaph_gql::Value::Int8(x) if *x > 0 => Some(*x as u64),
        gleaph_gql::Value::Int16(x) if *x > 0 => Some(*x as u64),
        gleaph_gql::Value::Int32(x) if *x > 0 => Some(*x as u64),
        gleaph_gql::Value::Int64(x) if *x > 0 => Some(*x as u64),
        gleaph_gql::Value::Uint8(x) => Some(u64::from(*x)).filter(|v| *v > 0),
        gleaph_gql::Value::Uint16(x) => Some(u64::from(*x)),
        gleaph_gql::Value::Uint32(x) => Some(u64::from(*x)),
        gleaph_gql::Value::Uint64(x) if *x > 0 => Some(*x),
        _ => None,
    }
}

fn resolve_scan_limit(
    value: &ScanValue,
    params: &BTreeMap<String, gleaph_gql::Value>,
) -> Result<u32, RouterError> {
    let n: u64 = match value {
        ScanValue::Literal(v) => positive_u64(v).ok_or_else(|| {
            RouterError::InvalidArgument("TextScan top-k limit must be a positive integer".into())
        })?,
        ScanValue::Parameter(_) => {
            let key = scan_value_param_name(value).expect("parameter checked above");
            let resolved = params
                .get(key)
                .ok_or_else(|| RouterError::InvalidArgument(format!("missing parameter ${key}")))?;
            positive_u64(resolved).ok_or_else(|| {
                RouterError::InvalidArgument(format!(
                    "top-k limit parameter ${key} must be a positive integer"
                ))
            })?
        }
        _ => {
            return Err(RouterError::InvalidArgument(
                "TextScan top-k limit must be an integer literal or parameter".into(),
            ));
        }
    };
    u32::try_from(n)
        .map_err(|_| RouterError::InvalidArgument("TextScan top-k limit exceeds u32::MAX".into()))
}

/// Resolves the threshold bound to f64 for comparison against engine scores.
fn resolve_scan_bound(
    value: &ScanValue,
    params: &BTreeMap<String, gleaph_gql::Value>,
) -> Result<f64, RouterError> {
    fn numeric_f64(value: &gleaph_gql::Value) -> Option<f64> {
        match value {
            gleaph_gql::Value::Int8(x) => Some(f64::from(*x)),
            gleaph_gql::Value::Int16(x) => Some(f64::from(*x)),
            gleaph_gql::Value::Int32(x) => Some(f64::from(*x)),
            gleaph_gql::Value::Int64(x) => Some(*x as f64),
            gleaph_gql::Value::Uint8(x) => Some(f64::from(*x)),
            gleaph_gql::Value::Uint16(x) => Some(f64::from(*x)),
            gleaph_gql::Value::Uint32(x) => Some(f64::from(*x)),
            gleaph_gql::Value::Uint64(x) => Some(*x as f64),
            gleaph_gql::Value::Float32(x) => Some(f64::from(*x)),
            gleaph_gql::Value::Float64(x) => Some(*x),
            _ => None,
        }
    }
    match value {
        ScanValue::Literal(v) => numeric_f64(v).ok_or_else(|| {
            RouterError::InvalidArgument(
                "TextScan threshold bound must be a numeric literal or parameter".into(),
            )
        }),
        ScanValue::Parameter(_) => {
            let key = scan_value_param_name(value).expect("parameter checked above");
            let resolved = params
                .get(key)
                .ok_or_else(|| RouterError::InvalidArgument(format!("missing parameter ${key}")))?;
            numeric_f64(resolved).ok_or_else(|| {
                RouterError::InvalidArgument(format!(
                    "threshold bound parameter ${key} must be numeric"
                ))
            })
        }
        _ => Err(RouterError::InvalidArgument(
            "TextScan threshold bound must be a numeric literal or parameter".into(),
        )),
    }
}

/// Keeps hits whose engine score satisfies the normalized (`Gt` / `Ge`) comparison.
fn retain_threshold(hits: &mut Vec<TextHitWire>, cmp: CmpOp, bound: f64) {
    hits.retain(|hit| {
        let score = f64::from(hit.score);
        match cmp {
            CmpOp::Gt => score > bound,
            CmpOp::Ge => score >= bound,
            // The planner normalizes reversed forms to `Gt`/`Ge`; any other operator never
            // lowers, so this arm is defensive only.
            _ => false,
        }
    });
}
// ════════════════════════════════════════════════════════════════════════════════
// Catalog resolution (fail-closed absent semantics)
// ════════════════════════════════════════════════════════════════════════════════

fn resolve_text_index(
    graph_id: GraphId,
    label_id: VertexLabelId,
    property_id: gleaph_graph_kernel::entry::PropertyId,
) -> Result<TextIndexDefRecord, RouterError> {
    // Planning-visible = `Ready` + attached canister; everything else is absent by contract,
    // so an unmatched (label, property) pair reports the function itself as unknown instead
    // of leaking catalog lifecycle states to callers.
    let mut candidates: Vec<TextIndexDefRecord> =
        text_index_catalog::planning_visible_text_indexes(graph_id)
            .into_iter()
            .filter(|def| def.label_id == label_id && def.property_id == property_id)
            .collect();
    candidates.sort_by_key(|def| def.text_index_id);
    candidates.into_iter().next().ok_or_else(|| {
        RouterError::NotFound(format!(
            "no ready TEXT index covers text_score over label id {} property id {} for this graph",
            label_id.raw(),
            property_id.raw()
        ))
    })
}

// ════════════════════════════════════════════════════════════════════════════════
// Transport (same-subnet composite query to the definition canister)
// ════════════════════════════════════════════════════════════════════════════════

/// Wire mirror of `text_canister::TextHit` (the router does not depend on the canister crate).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextHitWire {
    pub key: u64,
    /// Internal canister docid; carried for wire fidelity, never read by the merge.
    #[allow(dead_code)]
    pub docid: u32,
    pub score: u32,
}

#[cfg(target_family = "wasm")]
async fn text_canister_search(
    target: candid::Principal,
    query: String,
    k: u32,
) -> Result<Vec<TextHitWire>, String> {
    use ic_cdk::call::Call;

    #[derive(candid::CandidType, serde::Deserialize)]
    struct WireTextHit {
        key: u64,
        docid: u32,
        score: u32,
    }

    Call::bounded_wait(target, TEXT_SEARCH_METHOD)
        .with_args(&(query, k))
        .await
        .map_err(|e| format!("text {TEXT_SEARCH_METHOD} call failed: {e}"))?
        .candid::<Result<Vec<WireTextHit>, String>>()
        .map_err(|_| format!("text {TEXT_SEARCH_METHOD} decode failed"))?
        .map(|hits| {
            hits.into_iter()
                .map(|hit| TextHitWire {
                    key: hit.key,
                    docid: hit.docid,
                    score: hit.score,
                })
                .collect()
        })
        .map_err(|detail| format!("text {TEXT_SEARCH_METHOD} rejected: {detail}"))
}

#[cfg(not(target_family = "wasm"))]
async fn text_canister_search(
    _target: candid::Principal,
    _query: String,
    _k: u32,
) -> Result<Vec<TextHitWire>, String> {
    Ok(Vec::new())
}

// ════════════════════════════════════════════════════════════════════════════════
// Deterministic merge + seed emission
// ════════════════════════════════════════════════════════════════════════════════

/// Sort hits into the documented deterministic contract (score descending, key ascending
/// among equal scores). The canister already returns this order; re-sorting keeps the merge
/// deterministic regardless of wire-level tie presentation.
fn sort_hits_deterministically(hits: &mut [TextHitWire]) {
    hits.sort_unstable_by(|a, b| b.score.cmp(&a.score).then(a.key.cmp(&b.key)));
}

/// Build the single-shard seed relation. Each hit binds the scanned variable to its vertex;
/// when a residual projected call exists, the alias binds the score so ordinary downstream
/// machinery projects it without ever evaluating the function.
fn build_text_scan_seeds(
    binding: &str,
    alias: Option<&str>,
    required_label_ids: &[VertexLabelId],
    shard_id: ShardId,
    hits: &[TextHitWire],
) -> Result<BTreeMap<ShardId, SeedBindingsWire>, RouterError> {
    let mut seen: HashSet<u64> = HashSet::new();
    let mut rows = Vec::with_capacity(hits.len());
    for hit in hits {
        // One search must not rank the same document twice.
        if !seen.insert(hit.key) {
            return Err(RouterError::InvalidArgument(format!(
                "duplicate text search hit for document key {}",
                hit.key
            )));
        }
        let vertex_id = u32::try_from(hit.key).map_err(|_| {
            RouterError::InvalidArgument(format!(
                "text search document key {} does not fit a local vertex id",
                hit.key
            ))
        })?;
        rows.push(SeedRowWire {
            vertex_bindings: vec![SeedVertexBinding {
                variable: binding.to_string(),
                local_vertex_id: vertex_id,
                required_vertex_label_ids: required_label_ids.iter().map(|l| l.raw()).collect(),
            }],
            float64_bindings: alias
                .map(|name| {
                    vec![SeedFloat64Binding {
                        variable: name.to_string(),
                        value: f64::from(hit.score),
                    }]
                })
                .unwrap_or_default(),
        });
    }
    let mut by_shard = BTreeMap::new();
    by_shard.insert(
        shard_id,
        SeedBindingsWire {
            entries: Vec::new(),
            rows,
            complete_prefix_rows: false,
        },
    );
    Ok(by_shard)
}

// ════════════════════════════════════════════════════════════════════════════════
// Plan rewrite
// ════════════════════════════════════════════════════════════════════════════════

/// Strip the leading `TextScan` (seeds replace the anchor scan) and swap the residual
/// projected call for a plain reference to the seeded alias variable.
fn rewrite_executable_plan(
    plan: &PhysicalPlan,
    project_op_index: Option<usize>,
    alias: Option<&str>,
) -> Result<PhysicalPlan, RouterError> {
    let rewrite_at = match (project_op_index, alias) {
        (Some(index), Some(name)) => Some((index, name)),
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            return Err(RouterError::InvalidArgument(
                "internal: residual call and alias must agree".into(),
            ));
        }
    };
    let tail: Vec<PlanOp> = plan.ops[1..]
        .iter()
        .enumerate()
        .map(|(idx, op)| match rewrite_at {
            Some((index, name)) if idx + 1 == index => rewrite_project_op(op, name),
            _ => op.clone(),
        })
        .collect();
    Ok(PhysicalPlan {
        ops: tail,
        diagnostics: plan.diagnostics.clone(),
        annotations: plan.annotations.clone(),
        output: plan.output.clone(),
        binding_layout: plan.binding_layout.clone(),
    })
}

fn rewrite_project_op(op: &PlanOp, alias: &str) -> PlanOp {
    let PlanOp::Project { columns, distinct } = op else {
        return op.clone();
    };
    let columns: Vec<ProjectColumn> = columns
        .iter()
        .map(|col| {
            if is_text_score_call(&col.expr) {
                ProjectColumn {
                    expr: Expr::new(ExprKind::Variable(alias.to_string())),
                    alias: col.alias.clone(),
                }
            } else {
                col.clone()
            }
        })
        .collect();
    PlanOp::Project {
        columns,
        distinct: *distinct,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql_planner::plan::{NodeLabelRef, PhysicalPlan};

    fn hits(keys_scores: &[(u64, u32)]) -> Vec<TextHitWire> {
        keys_scores
            .iter()
            .map(|&(key, score)| TextHitWire {
                key,
                docid: 0,
                score,
            })
            .collect()
    }

    fn text_scan_op() -> PlanOp {
        PlanOp::TextScan {
            variable: "n".into(),
            label: NodeLabelRef::from("Document"),
            property: "body".into(),
            query: ScanValue::Literal(gleaph_gql::Value::Text("index".into())),
            mode: TextScanMode::Threshold {
                cmp: CmpOp::Gt,
                bound: ScanValue::Literal(gleaph_gql::Value::Float64(0.5)),
            },
            property_projection: None,
        }
    }

    fn project_call_op(alias: &str) -> PlanOp {
        PlanOp::Project {
            columns: vec![ProjectColumn {
                expr: Expr::new(ExprKind::FunctionCall {
                    name: gleaph_gql::ast::ObjectName::simple("text_score"),
                    args: vec![
                        Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("n".into()))),
                            property: "body".into(),
                        }),
                        Expr::new(ExprKind::Literal(gleaph_gql::Value::Text("index".into()))),
                    ],
                    distinct: false,
                }),
                alias: Some(alias.into()),
            }],
            distinct: false,
        }
    }

    #[test]
    fn sort_orders_by_score_desc_then_key_asc() {
        let mut sorted = hits(&[(7, 9), (2, 12), (5, 9)]);
        sort_hits_deterministically(&mut sorted);
        assert_eq!(
            sorted.iter().map(|hit| hit.key).collect::<Vec<_>>(),
            vec![2, 5, 7],
            "highest score first; among equal scores the smaller key wins"
        );
    }

    #[test]
    fn seeds_bind_vertex_label_and_optional_score_alias() {
        let shard_id = ShardId::new(0);
        let by_shard = build_text_scan_seeds(
            "n",
            Some("score"),
            &[VertexLabelId::from_raw(4)],
            shard_id,
            &hits(&[(11, 30), (3, 20)]),
        )
        .expect("seeds");
        let rows = &by_shard[&shard_id].rows;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].vertex_bindings[0].variable, "n");
        assert_eq!(rows[0].vertex_bindings[0].local_vertex_id, 11);
        assert_eq!(
            rows[0].vertex_bindings[0].required_vertex_label_ids,
            vec![4]
        );
        assert_eq!(rows[0].float64_bindings[0].variable, "score");
        assert_eq!(rows[0].float64_bindings[0].value, 30.0);
        assert_eq!(rows[1].float64_bindings[0].value, 20.0);
    }

    #[test]
    fn seeds_without_alias_bind_no_score_columns() {
        let shard_id = ShardId::new(0);
        let by_shard =
            build_text_scan_seeds("n", None, &[], shard_id, &hits(&[(11, 30)])).expect("seeds");
        assert!(by_shard[&shard_id].rows[0].float64_bindings.is_empty());
    }

    #[test]
    fn duplicate_hit_keys_fail_closed() {
        let seeded = build_text_scan_seeds(
            "n",
            None,
            &[],
            ShardId::new(0),
            &hits(&[(11, 30), (11, 25)]),
        );
        assert!(
            matches!(seeded, Err(RouterError::InvalidArgument(ref msg)) if msg.contains("duplicate")),
            "unexpected result: {seeded:?}"
        );
    }

    #[test]
    fn oversized_document_keys_fail_closed() {
        let seeded = build_text_scan_seeds(
            "n",
            None,
            &[],
            ShardId::new(0),
            &hits(&[(u64::from(u32::MAX) + 1, 30)]),
        );
        assert!(matches!(seeded, Err(RouterError::InvalidArgument(_))));
    }

    #[test]
    fn threshold_retains_strict_and_inclusive_bounds_only() {
        let mut strict = hits(&[(1, 60), (2, 50), (3, 40)]);
        retain_threshold(&mut strict, CmpOp::Gt, 50.0);
        assert_eq!(strict.len(), 1, "`>` excludes equal scores");

        let mut inclusive = hits(&[(1, 60), (2, 50), (3, 40)]);
        retain_threshold(&mut inclusive, CmpOp::Ge, 50.0);
        assert_eq!(inclusive.len(), 2, "`>=` includes equal scores");

        // Defensive arm: un-lowerable operators keep nothing rather than guessing.
        let mut eq = hits(&[(1, 60)]);
        retain_threshold(&mut eq, CmpOp::Eq, 60.0);
        assert!(eq.is_empty());
    }

    #[test]
    fn scan_queries_resolve_literals_and_parameters() {
        let mut params = BTreeMap::new();
        params.insert("q".to_string(), gleaph_gql::Value::Text("needle".into()));

        let literal = ScanValue::Literal(gleaph_gql::Value::Text("literal".into()));
        assert_eq!(
            resolve_scan_query(&literal, &params).expect("literal"),
            "literal"
        );
        let parameter = ScanValue::Parameter("$q".into());
        assert_eq!(
            resolve_scan_query(&parameter, &params).expect("parameter"),
            "needle"
        );

        let mut wrong_type = BTreeMap::new();
        wrong_type.insert("q".to_string(), gleaph_gql::Value::Int64(7));
        assert!(resolve_scan_query(&parameter, &wrong_type).is_err());
        assert!(resolve_scan_query(&parameter, &BTreeMap::new()).is_err());
        let numeric = ScanValue::Literal(gleaph_gql::Value::Int64(7));
        assert!(resolve_scan_query(&numeric, &params).is_err());
    }

    #[test]
    fn topk_limits_resolve_positive_integers_with_clamp_headroom() {
        let mut params = BTreeMap::new();
        params.insert("k".to_string(), gleaph_gql::Value::Int64(9));

        let literal = ScanValue::Literal(gleaph_gql::Value::Int64(5));
        assert_eq!(resolve_scan_limit(&literal, &params).expect("literal"), 5);
        let parameter = ScanValue::Parameter("$k".into());
        assert_eq!(resolve_scan_limit(&parameter, &params).expect("param"), 9);

        let zero = ScanValue::Literal(gleaph_gql::Value::Int64(0));
        assert!(resolve_scan_limit(&zero, &params).is_err());
        let negative = ScanValue::Literal(gleaph_gql::Value::Int64(-3));
        assert!(resolve_scan_limit(&negative, &params).is_err());
        let overflowing = ScanValue::Literal(gleaph_gql::Value::Uint64(u64::MAX));
        assert!(resolve_scan_limit(&overflowing, &params).is_err());
    }

    #[test]
    fn threshold_bounds_resolve_numeric_values_only() {
        let float = ScanValue::Literal(gleaph_gql::Value::Float64(0.25));
        assert_eq!(
            resolve_scan_bound(&float, &BTreeMap::new()).expect("float"),
            0.25
        );
        let int = ScanValue::Literal(gleaph_gql::Value::Int64(1));
        assert_eq!(
            resolve_scan_bound(&int, &BTreeMap::new()).expect("int"),
            1.0
        );

        let mut params = BTreeMap::new();
        params.insert("t".to_string(), gleaph_gql::Value::Float64(0.75));
        let parameter = ScanValue::Parameter("$t".into());
        assert_eq!(
            resolve_scan_bound(&parameter, &params).expect("param"),
            0.75
        );

        let text = ScanValue::Literal(gleaph_gql::Value::Text("high".into()));
        assert!(resolve_scan_bound(&text, &params).is_err());
    }

    #[test]
    fn detection_separates_scans_from_residual_mentions() {
        let scan_plan = PhysicalPlan::from_ops(vec![text_scan_op()]);
        assert!(ops_contain_text_scan(&scan_plan.ops));
        assert!(!plan_mentions_text_score(&scan_plan));

        let mention_plan = PhysicalPlan::from_ops(vec![project_call_op("score")]);
        assert!(!ops_contain_text_scan(&mention_plan.ops));
        assert!(plan_mentions_text_score(&mention_plan));

        let neutral = PhysicalPlan::from_ops(vec![PlanOp::NodeScan {
            variable: "n".into(),
            label: None,
            property_projection: None,
        }]);
        assert!(!ops_contain_text_scan(&neutral.ops));
        assert!(!plan_mentions_text_score(&neutral));
    }

    #[test]
    fn entry_returns_none_for_unrelated_plans_and_rejects_unlowered_mentions() {
        let store = crate::RouterStore::new();
        let graph_id = GraphId::from_raw(1);
        let stats = crate::planner_stats::RouterGraphStats::from_property_ids(
            graph_id,
            Default::default(),
            Default::default(),
        );

        let neutral = PhysicalPlan::from_ops(vec![PlanOp::NodeScan {
            variable: "n".into(),
            label: None,
            property_projection: None,
        }]);
        let result = futures::executor::block_on(try_execute_gql_text_scan(
            &neutral,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &stats,
            &store,
        ))
        .expect("no error");
        assert!(result.is_none(), "unrelated plans must fall through");

        let unlowered = PhysicalPlan::from_ops(vec![project_call_op("score")]);
        let err = futures::executor::block_on(try_execute_gql_text_scan(
            &unlowered,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &stats,
            &store,
        ))
        .expect_err("residual mention without a scan must fail closed");
        assert!(
            matches!(err, RouterError::InvalidArgument(ref msg) if msg.contains("did not lower")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn entry_enforces_query_mode_and_leading_scan_position() {
        let store = crate::RouterStore::new();
        let graph_id = GraphId::from_raw(1);
        let stats = crate::planner_stats::RouterGraphStats::from_property_ids(
            graph_id,
            Default::default(),
            Default::default(),
        );

        let update_err = futures::executor::block_on(try_execute_gql_text_scan(
            &PhysicalPlan::from_ops(vec![text_scan_op()]),
            graph_id,
            &[],
            GqlExecutionMode::Update,
            &stats,
            &store,
        ))
        .expect_err("mutation programs are rejected in this slice");
        assert!(matches!(update_err, RouterError::InvalidArgument(_)));

        let trailing = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: None,
                property_projection: None,
            },
            text_scan_op(),
        ]);
        let position_err = futures::executor::block_on(try_execute_gql_text_scan(
            &trailing,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &stats,
            &store,
        ))
        .expect_err("non-leading TextScan must be rejected");
        assert!(
            matches!(position_err, RouterError::InvalidArgument(ref msg) if msg.contains("leading")),
            "unexpected error: {position_err:?}"
        );
    }

    #[test]
    fn resolver_matrix_is_visible_only_for_ready_targeted_definitions() {
        use crate::facade::auth;
        use crate::init::RouterInitArgs;

        let store = crate::RouterStore::new();
        let admin = candid::Principal::from_slice(&[1; 29]);
        store.init_from_args(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: None,
        });
        auth::grant_admins(&[admin]);
        crate::facade::store::catalog_test_support::register_graph(
            &store,
            admin,
            "tenant.text.exec",
        );
        let graph_id = store.resolve_graph_id("tenant.text.exec").expect("graph");
        store
            .admin_intern_vertex_label(admin, "tenant.text.exec", "Document")
            .expect("label");
        let label_id = store
            .lookup_vertex_label_id(graph_id, "Document")
            .expect("label id");
        let property_id = crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            "tenant.text.exec",
            "body",
        );
        let index_name_id = crate::facade::stable::index_name_catalog::intern_index_name(
            graph_id,
            "doc_body_text_idx",
        )
        .expect("intern name");
        let raw_id = text_index_catalog::allocate_text_index_id().expect("allocate");

        // Absent: nothing registered yet.
        assert!(resolve_text_index(graph_id, label_id, property_id).is_err());

        // Provisioned definitions are born `Backfilling`: planner-INVISIBLE even though
        // a row exists, closing the empty-but-visible false-negative window.
        text_index_catalog::register_text_index(
            graph_id,
            raw_id,
            index_name_id,
            label_id,
            property_id,
            crate::index_catalog::TEXT_INDEX_ANALYZER_V0,
            Some(candid::Principal::management_canister()),
            false,
        )
        .expect("register provisioned definition");
        assert!(
            resolve_text_index(graph_id, label_id, property_id).is_err(),
            "Backfilling definitions must stay invisible to text execution"
        );

        // Convergence flips `Backfilling -> Ready`: only now does the resolver bind.
        text_index_catalog::complete_text_backfill(graph_id, raw_id).expect("converge backfill");
        let resolved =
            resolve_text_index(graph_id, label_id, property_id).expect("Ready definition resolves");
        assert_eq!(resolved.text_index_id, raw_id);
    }
}
