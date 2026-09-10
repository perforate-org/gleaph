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
//! bounded search window (completeness beyond the window is marked truncated). The compound
//! `ThresholdTopK { cmp, bound, limit }` mode (plan 0329) retains the threshold on the
//! score-ranked window FIRST, then truncates to the limit — the top-k of the
//! threshold-filtered set, since `search` returns the score-ranked top-N window.

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

use candid::Encode;

use crate::RouterStore;
use crate::facade::stable::text_index_catalog::{self, TextIndexDefRecord};
use crate::gql_search::cap_result_rows;
use crate::planner_stats::RouterGraphStats;
use crate::state::RouterError;

/// Method name on the text canister (`#[query] fn search(query: String, k: u32)`).
#[cfg(target_family = "wasm")]
const TEXT_SEARCH_METHOD: &str = "search";

/// Method name on the text canister (`#[query] fn search_candidates(query, keys)`).
/// Controller-guarded on wasm; the native stub fails closed (plan 0344).
#[cfg(target_family = "wasm")]
const TEXT_SEARCH_CANDIDATES_METHOD: &str = "search_candidates";

/// Admission cap on authorized prefix rows admitted into candidate scoring (R=1024;
/// the probe row 1025 is rejected). Distinct from the TEXT-side candidate key cap.
const MAX_CANDIDATE_PREFIX_ROWS: usize = 1024;
/// Admission cap on the encoded prefix payload admitted into candidate scoring.
const MAX_CANDIDATE_PREFIX_BYTES: usize = 1024 * 1024;
/// Admission cap on user (non-identity) columns in the retained prefix projection.
const MAX_CANDIDATE_USER_COLUMNS: usize = 16;
/// Admission cap on distinct TEXT candidate keys per call (mirrors the canister).
const MAX_CANDIDATE_KEYS: usize = 256;
/// Admission cap on the encoded `search_candidates` argument payload.
const MAX_CANDIDATE_CALL_BYTES: usize = 32 * 1024;
/// Internal prefix-projection alias carrying `ELEMENT_ID(scored variable)`.
const CANDIDATE_ID_ALIAS: &str = "__gleaph_cid";
/// Internal prefix-projection alias carrying `ELEMENT_ID(second variable)` for the
/// two-variable dual-score slice. Projected only then; the same-variable slice
/// reuses the single identity column.
const CANDIDATE_ID_ALIAS_2: &str = "__gleaph_cid2";
/// Internal prefix-projection alias prefix for retained user columns.
const CANDIDATE_COLUMN_ALIAS_PREFIX: &str = "__gleaph_c";

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
    // A TextScan AFTER a traversal prefix is the plan 0344 candidate barrier: the
    // fully authorized prefix runs first and only its rows are scored.
    if !matches!(plan.ops.first(), Some(PlanOp::TextScan { .. })) {
        return try_execute_candidate_text_scan(plan, graph_id, params_blob, mode, stats, store)
            .await;
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
        TextScanMode::ThresholdTopK { cmp, bound, limit } => {
            // Order is the correctness core: retain the threshold on the score-ranked
            // window FIRST, then truncate to the limit, so the survivors are exactly the
            // top-k of the threshold-filtered set.
            let bound = resolve_scan_bound(bound, &params)?;
            retain_threshold(&mut hits, *cmp, bound);
            let requested_k = resolve_scan_limit(limit, &params)?;
            let request_k = requested_k.min(MAX_TEXT_SEARCH_K);
            hits.truncate(request_k as usize);
            // A LIMIT beyond the canister clamp cannot be satisfied completely.
            truncated |= requested_k > MAX_TEXT_SEARCH_K;
            Some(requested_k)
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
// Candidate-scoped non-leading text_score (plan 0344)
// ════════════════════════════════════════════════════════════════════════════════

/// Analyzed candidate barrier: everything the Router needs without catalog access.
/// Barrier ranking mode: row-capped top-k, or uncapped threshold filtering.
#[derive(Clone, Copy)]
enum CandidateBarrierMode {
    /// Deliver the `limit` highest-scoring rows (`TopK` scan mode).
    TopK { limit: u32 },
    /// Keep every row whose candidate score satisfies `cmp bound` (`Threshold`
    /// scan mode). Candidate scoring is all-match, so filtering is complete and
    /// the result is never truncated.
    Threshold { cmp: CmpOp, bound: f64 },
    /// Keep the `limit` highest-scoring rows of the threshold-filtered set
    /// (`ThresholdTopK` scan mode fused after a prefix). The threshold applies
    /// first on the complete candidate hit set, then ranking truncates — the
    /// candidate-scoped plan 0329 order — so the result is exact and never
    /// truncated.
    Compound { cmp: CmpOp, bound: f64, limit: u32 },
}

/// Second residual score on a different (variable, property, label) triple:
/// slice 1 scores a distinct property of the scanned variable (same document
/// key, no second identity column); slice 2 scores a second prefix-bound
/// variable (own identity column, own label resolved from the prefix). Both
/// scores resolve their queries independently — no same-query requirement.
struct SecondBarrierScore {
    /// Second scored variable (equals the barrier variable in slice 1).
    variable: String,
    /// Second scored label name (equals the barrier label in slice 1).
    label: String,
    /// Second scored property name (resolved to an id by the caller).
    property: String,
    /// Second scored query (literal or `$param`, resolved per call).
    query: ScanValue,
    /// Trailing-`Project` column index holding the second residual call.
    score_col_idx: usize,
}

struct CandidateBarrierShape {
    /// Index of the barrier `TextScan` in `plan.ops` (always > 0).
    scan_idx: usize,
    /// Scored variable bound by the prefix.
    variable: String,
    /// Scan label / property names (resolved to ids by the caller).
    label: String,
    property: String,
    /// Scan query (literal or `$param`).
    query: ScanValue,
    /// Ranking mode: row-capped top-k, or uncapped threshold filtering.
    mode: CandidateBarrierMode,
    /// Rows to skip after ranking (a fused OFFSET parked as a trailing pure
    /// skip-`Limit` by the planner). Zero when the query carries no offset.
    skip: u32,
    /// The trailing `Project` carried `DISTINCT`: the execution dedups the fully
    /// projected rows (first occurrence wins, rank order preserved) before
    /// skip/take. False for the plain `RETURN` shape.
    distinct: bool,
    /// Trailing-`Project` column index holding the residual score call, when the
    /// `RETURN` projects one. A threshold-only `RETURN` carries no score column.
    score_col_idx: Option<usize>,
    /// Optional second residual score: same-variable distinct-property (slice 1)
    /// or second-variable (slice 2, own label and identity column).
    second: Option<SecondBarrierScore>,
    /// Count of retained user columns (excludes the score column).
    user_col_count: usize,
}

/// Pure shape analysis for the candidate barrier (no catalog, no I/O): exactly one
/// non-leading `TextScan` in `TopK` or `Threshold` mode, a non-empty cap-free
/// mention-free prefix, and exactly one trailing `Project` whose optional single
/// residual `text_score` call names the scanned (variable, property, query).
/// Anything else fails closed.
fn analyze_candidate_barrier_shape(
    plan: &PhysicalPlan,
    params: &BTreeMap<String, gleaph_gql::Value>,
) -> Result<CandidateBarrierShape, RouterError> {
    let unsupported = |detail: &str| {
        RouterError::InvalidArgument(format!("candidate text_score unsupported: {detail}"))
    };
    let scan_positions: Vec<usize> = plan
        .ops
        .iter()
        .enumerate()
        .filter(|(_, op)| matches!(op, PlanOp::TextScan { .. }))
        .map(|(idx, _)| idx)
        .collect();
    let [scan_idx] = scan_positions.as_slice() else {
        return Err(unsupported("exactly one TextScan is required"));
    };
    let scan_idx = *scan_idx;
    if scan_idx == 0 {
        return Err(unsupported("leading TextScan takes the seed path"));
    }
    let PlanOp::TextScan {
        variable,
        label,
        property,
        query,
        mode,
        ..
    } = &plan.ops[scan_idx]
    else {
        unreachable!("position matched TextScan");
    };
    let mode = match mode {
        TextScanMode::TopK { limit } => {
            let limit = resolve_scan_limit(limit, params)?;
            if limit == 0 || limit as usize > MAX_CANDIDATE_PREFIX_ROWS {
                return Err(unsupported(
                    "row LIMIT must be within 1..=1024 in this slice",
                ));
            }
            CandidateBarrierMode::TopK { limit }
        }
        TextScanMode::Threshold { cmp, bound } => {
            let bound = resolve_scan_bound(bound, params)?;
            CandidateBarrierMode::Threshold { cmp: *cmp, bound }
        }
        TextScanMode::ThresholdTopK { cmp, bound, limit } => {
            let bound = resolve_scan_bound(bound, params)?;
            let limit = resolve_scan_limit(limit, params)?;
            if limit == 0 || limit as usize > MAX_CANDIDATE_PREFIX_ROWS {
                return Err(unsupported(
                    "row LIMIT must be within 1..=1024 in this slice",
                ));
            }
            CandidateBarrierMode::Compound {
                cmp: *cmp,
                bound,
                limit,
            }
        }
    };
    let prefix = &plan.ops[..scan_idx];
    if prefix.iter().any(|op| {
        matches!(
            op,
            PlanOp::TextScan { .. } | PlanOp::Limit { .. } | PlanOp::TopK { .. }
        ) || op_mention_text_score(op)
            || nested_subplan_mentions(op)
    }) {
        return Err(unsupported(
            "the candidate prefix must be cap-free and mention-free",
        ));
    }
    // The tail must be the late-projected RETURN, optionally followed by the
    // planner-parked pure skip-`Limit` of a fused OFFSET. A `DISTINCT` tail is
    // accepted and carried as a flag for the execution dedup stage. Anything
    // else — a row count, a non-literal offset, a parameter — fails closed.
    let (columns, skip, distinct) = match &plan.ops[scan_idx + 1..] {
        [PlanOp::Project { columns, distinct }] => (columns, 0, *distinct),
        [
            PlanOp::Project { columns, distinct },
            PlanOp::Limit {
                count: None,
                offset: Some(offset),
            },
        ] => (columns, resolve_skip_offset(offset)?, *distinct),
        _ => {
            return Err(unsupported(
                "only a single trailing Project, optionally followed by a pure skip Limit, is supported",
            ));
        }
    };
    // A skip after a threshold barrier stays unsupported in this slice: the
    // threshold lowering carries no TopK to fuse an OFFSET through, so no
    // planner path produces this shape — only a hand-built plan could.
    if skip > 0 && matches!(mode, CandidateBarrierMode::Threshold { .. }) {
        return Err(unsupported(
            "offset after a threshold barrier stays unsupported in this slice",
        ));
    }
    let mut score_col_idx = None;
    let mut second: Option<SecondBarrierScore> = None;
    for (idx, col) in columns.iter().enumerate() {
        if !expr_mentions_text_score(&col.expr) {
            continue;
        }
        if residual_call_matches_scan(&col.expr, variable, property, query) {
            if score_col_idx.is_some() {
                return Err(unsupported(
                    "the trailing Project carries the scanned call at most once",
                ));
            }
            score_col_idx = Some(idx);
            continue;
        }
        // Slice 1: a second bare call on the same variable with a distinct
        // property. Slice 2: a bare call on a second prefix-bound variable with
        // a label proven by the prefix. Anything else — a wrapped call, a third
        // call — fails closed.
        let Some((var2, prop2, query2)) = resolve_residual_call(&col.expr) else {
            return Err(unsupported(
                "the trailing Project carries at most the scanned call plus one bare second call",
            ));
        };
        if second.is_some() {
            return Err(unsupported(
                "the second residual call is accepted at most once",
            ));
        }
        // The second join is TopK-only in both slices: threshold and compound
        // barriers plus DISTINCT tails stay single-score and fail closed.
        if !matches!(mode, CandidateBarrierMode::TopK { .. }) || distinct {
            return Err(unsupported(
                "a second text_score call is supported in the top-k form only, without DISTINCT",
            ));
        }
        if var2 == variable.to_string() {
            if prop2 == property.to_string() {
                return Err(unsupported(
                    "the second residual call must score a distinct property of the scanned variable",
                ));
            }
            second = Some(SecondBarrierScore {
                variable: var2,
                label: label.to_string(),
                property: prop2,
                query: query2,
                score_col_idx: idx,
            });
            continue;
        }
        // Slice 2: the second variable's label is proven from the barrier-free
        // prefix with the same helper the planner lowering uses. An unbound or
        // ambiguously labeled variable fails closed — the Router cannot key a
        // TEXT triple without an exact (label, property) index.
        let prefix = &plan.ops[..scan_idx];
        let Some(label2) = gleaph_gql_planner::text_scan::proven_prefix_label(prefix, &var2) else {
            return Err(unsupported(
                "the second scored variable needs a single proven label in the prefix",
            ));
        };
        second = Some(SecondBarrierScore {
            variable: var2,
            label: label2,
            property: prop2,
            query: query2,
            score_col_idx: idx,
        });
    }
    let score_cols = usize::from(score_col_idx.is_some()) + usize::from(second.is_some());
    let user_col_count = columns.len() - score_cols;
    if user_col_count > MAX_CANDIDATE_USER_COLUMNS {
        return Err(unsupported("at most 16 retained user columns"));
    }
    Ok(CandidateBarrierShape {
        scan_idx,
        variable: variable.to_string(),
        label: label.to_string(),
        property: property.to_string(),
        query: query.clone(),
        mode,
        skip,
        distinct,
        score_col_idx,
        second,
        user_col_count,
    })
}

/// The residual call must be the entire projected expression over the scanned
/// (variable, property) with a query matching the scan (literal-for-literal,
/// parameter-for-parameter).
/// Resolve a bare `text_score(v.prop, Q)` column expression into its triple.
/// Returns `None` for wrapped or malformed calls (rejected by the shape gate).
/// Pure: unit-tested without I/O.
fn resolve_residual_call(expr: &Expr) -> Option<(String, String, ScanValue)> {
    let ExprKind::FunctionCall {
        name,
        args,
        distinct,
    } = &expr.kind
    else {
        return None;
    };
    if *distinct || name.parts.len() != 1 || !name.parts[0].eq_ignore_ascii_case("text_score") {
        return None;
    }
    let [target, query_arg] = args.as_slice() else {
        return None;
    };
    let ExprKind::PropertyAccess { expr, property } = &target.kind else {
        return None;
    };
    let ExprKind::Variable(variable) = &expr.kind else {
        return None;
    };
    let query = match &query_arg.kind {
        ExprKind::Literal(gleaph_gql::Value::Text(text)) => {
            ScanValue::Literal(gleaph_gql::Value::Text(text.clone()))
        }
        ExprKind::Parameter(name) => ScanValue::Parameter(name.as_str().into()),
        _ => return None,
    };
    Some((variable.clone(), property.clone(), query))
}

fn residual_call_matches_scan(
    expr: &Expr,
    variable: &str,
    property: &str,
    query: &ScanValue,
) -> bool {
    let ExprKind::FunctionCall { name, args, .. } = &expr.kind else {
        return false;
    };
    if name.parts.len() != 1 || !name.parts[0].eq_ignore_ascii_case("text_score") {
        return false;
    }
    let [target, query_arg] = args.as_slice() else {
        return false;
    };
    let ExprKind::PropertyAccess {
        expr,
        property: prop,
    } = &target.kind
    else {
        return false;
    };
    if prop != property || !matches!(&expr.kind, ExprKind::Variable(v) if v == variable) {
        return false;
    }
    match (query, &query_arg.kind) {
        (ScanValue::Literal(want), ExprKind::Literal(got)) => want == got,
        (ScanValue::Parameter(want), ExprKind::Parameter(got)) => {
            want.strip_prefix('$').unwrap_or(want.as_ref())
                == got.strip_prefix('$').unwrap_or(got.as_ref())
        }
        _ => false,
    }
}

/// One authorized prefix row: the TEXT candidate key, the optional second-variable
/// key (slice 2 only), plus retained user values.
struct CandidatePrefixRow {
    key: u64,
    key2: Option<u64>,
    values: Vec<gleaph_gql_ic::GqlWireValue>,
}

/// Join key for the second round trip (pure: unit-tested without I/O). The
/// same-variable slice reuses the shared document key; the two-variable slice
/// keys off the second variable's element id. `None` (a slice-2 row without a
/// second identity) never joins — the caller treats it as a scoreless drop.
fn second_join_key(row: &CandidatePrefixRow, two_variable: bool) -> Option<u64> {
    if two_variable {
        row.key2
    } else {
        Some(row.key)
    }
}

/// Row window and post-dedup take for the barrier execution. A `DISTINCT` tail
/// bypasses the `k + skip` window (dedup only shrinks rows, so a pre-dedup
/// window is never exact) and takes after dedup → skip instead. The take is
/// the fused window minus the parked skip (the planner fuses `LIMIT k OFFSET
/// n` into a `k + n` window with a pure skip-`Limit`, so `window - skip` is
/// exactly `k`; saturating for hand-built shapes). Pure: unit-tested without I/O.
fn barrier_row_window(mode: &CandidateBarrierMode, distinct: bool, skip: u32) -> (usize, usize) {
    let window = match mode {
        CandidateBarrierMode::TopK { limit } | CandidateBarrierMode::Compound { limit, .. } => {
            *limit as usize
        }
        // Threshold keeps every surviving row: filtering already happened above.
        CandidateBarrierMode::Threshold { .. } => usize::MAX,
    };
    let take = window.saturating_sub(skip as usize);
    let row_cap = if distinct { usize::MAX } else { window };
    (row_cap, take)
}

/// First-occurrence dedup over fully projected wire rows, mirroring Graph
/// `dedup_rows`: whole-row equality, rank order preserved. O(n²) over at most
/// the 1024-row prefix cap, so no hash index. Pure: unit-tested without I/O.
fn dedup_wire_rows(rows: &mut Vec<gleaph_gql_ic::GqlWireRow>) {
    let mut unique = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if !unique.contains(&row) {
            unique.push(row);
        }
    }
    *rows = unique;
}
/// Join TEXT scores onto prefix rows and rank: keep rows whose key scored, order
/// `(score desc, key asc)` (stable within identical pairs, preserving prefix order),
/// truncate to the row limit. Pure: unit-tested without I/O.
fn rank_candidate_rows(
    rows: Vec<CandidatePrefixRow>,
    scores: &BTreeMap<u64, u32>,
    limit: usize,
) -> Vec<(u32, CandidatePrefixRow)> {
    let mut ranked: Vec<(u32, usize, CandidatePrefixRow)> = Vec::new();
    for (order, row) in rows.into_iter().enumerate() {
        if let Some(score) = scores.get(&row.key) {
            ranked.push((*score, order, row));
        }
    }
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.2.key.cmp(&b.2.key))
            .then_with(|| a.1.cmp(&b.1))
    });
    ranked.truncate(limit);
    ranked
        .into_iter()
        .map(|(score, _, row)| (score, row))
        .collect()
}

/// Execute the candidate barrier: run the fully authorized prefix on the graph,
/// score only its document keys in TEXT, rank rows, and project the user columns
/// plus the score — no second graph round-trip (retained projection).
async fn try_execute_candidate_text_scan(
    plan: &PhysicalPlan,
    graph_id: GraphId,
    params_blob: &[u8],
    mode: GqlExecutionMode,
    stats: &RouterGraphStats,
    store: &RouterStore,
) -> Result<Option<GqlQueryResult>, RouterError> {
    let params = gleaph_gql_ic::wire::decode_gql_params_blob(params_blob).map_err(|e| {
        RouterError::InvalidArgument(format!("failed to decode GQL parameters: {e}"))
    })?;
    let barrier = analyze_candidate_barrier_shape(plan, &params)?;
    let query = resolve_scan_query(&barrier.query, &params)?;

    let label_id = store
        .lookup_vertex_label_id(graph_id, &barrier.label)
        .map_err(|e| {
            RouterError::NotFound(format!("candidate text label {}: {e}", barrier.label))
        })?;
    let property_id = store
        .lookup_property_id(graph_id, &barrier.property)
        .map_err(|e| {
            RouterError::NotFound(format!("candidate text property {}: {e}", barrier.property))
        })?;
    // Planning-visible definitions are always `Ready` with an attached canister;
    // anything else resolves as "function unknown" fail-closed.
    let def = resolve_text_index(graph_id, label_id, property_id)?;
    let target = def
        .target
        .expect("planning-visible text definitions always carry a target");
    // Second triple: the second variable's own label (slice 2) or the barrier
    // label (slice 1), distinct property. Unready resolves fail-closed before
    // any I/O, exactly like the primary triple.
    let second_target = if let Some(second) = &barrier.second {
        let second_label_id = store
            .lookup_vertex_label_id(graph_id, &second.label)
            .map_err(|e| {
                RouterError::NotFound(format!("candidate text label {}: {e}", second.label))
            })?;
        let second_property_id = store
            .lookup_property_id(graph_id, &second.property)
            .map_err(|e| {
                RouterError::NotFound(format!("candidate text property {}: {e}", second.property))
            })?;
        let second_def = resolve_text_index(graph_id, second_label_id, second_property_id)?;
        Some(
            second_def
                .target
                .expect("planning-visible text definitions always carry a target"),
        )
    } else {
        None
    };
    // One text canister serves exactly its home shard's doc-key space.
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    let [shard] = shards.as_slice() else {
        return Err(RouterError::Conflict(format!(
            "candidate text_score requires a single live shard for this graph; {} live shards are not supported until multi-shard text fan-out lands",
            shards.len()
        )));
    };
    let element_id_key = store.graph_element_id_encoding_key(graph_id)?;

    // Prefix executable plan: the barrier-free prefix plus a terminal projection
    // carrying ELEMENT_ID(scored variable) and the retained user columns.
    let PlanOp::Project { columns, .. } = &plan.ops[barrier.scan_idx + 1] else {
        return Err(RouterError::InvalidArgument(
            "candidate text_score unsupported: trailing Project vanished".into(),
        ));
    };
    let mut prefix_ops: Vec<PlanOp> = plan.ops[..barrier.scan_idx].to_vec();
    let mut internal_cols = Vec::with_capacity(barrier.user_col_count + 1);
    internal_cols.push(ProjectColumn {
        expr: Expr::new(ExprKind::ElementId(Box::new(Expr::new(
            ExprKind::Variable(barrier.variable.clone()),
        )))),
        alias: Some(CANDIDATE_ID_ALIAS.into()),
    });
    // Slice 2 carries the second variable's element id alongside: the s2 call
    // never reaches the graph (both score columns stay excluded below), but the
    // barrier needs the second key to address the second TEXT triple.
    let second_variable = barrier
        .second
        .as_ref()
        .filter(|second| second.variable != barrier.variable)
        .map(|second| second.variable.clone());
    if let Some(variable) = &second_variable {
        internal_cols.push(ProjectColumn {
            expr: Expr::new(ExprKind::ElementId(Box::new(Expr::new(
                ExprKind::Variable(variable.clone()),
            )))),
            alias: Some(CANDIDATE_ID_ALIAS_2.into()),
        });
    }
    let mut user_positions: Vec<usize> = Vec::with_capacity(barrier.user_col_count);
    for (idx, col) in columns.iter().enumerate() {
        if Some(idx) == barrier.score_col_idx
            || barrier
                .second
                .as_ref()
                .is_some_and(|second| idx == second.score_col_idx)
        {
            continue;
        }
        user_positions.push(idx);
        internal_cols.push(ProjectColumn {
            expr: col.expr.clone(),
            alias: Some(
                format!(
                    "{CANDIDATE_COLUMN_ALIAS_PREFIX}{}",
                    user_positions.len() - 1
                )
                .into(),
            ),
        });
    }
    prefix_ops.push(PlanOp::Project {
        columns: internal_cols,
        distinct: false,
    });
    // Output/binding_layout are re-derived over the barrier-free prefix ops: the
    // appended terminal projection is Router-internal and never re-planned.
    let prefix_plan = PhysicalPlan {
        output: gleaph_gql_planner::output_schema::derive_output_schema(&prefix_ops),
        binding_layout: gleaph_gql_planner::binding_layout::derive_binding_layout(&prefix_ops),
        ops: prefix_ops,
        diagnostics: plan.diagnostics.clone(),
        annotations: plan.annotations.clone(),
    };
    let prefix_blob =
        gleaph_gql_planner::wire::encode_block_plans(std::slice::from_ref(&prefix_plan), false)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    if prefix_blob.len() > MAX_CANDIDATE_PREFIX_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "candidate prefix plan of {} bytes exceeds the 1MiB admission cap",
            prefix_blob.len()
        )));
    }

    // The prefix runs through the canonical single-graph read pipeline (seed-anchor
    // resolution, policy lowering, sharded dispatch) with the barrier-free plan: the
    // Router never re-implements index-anchor dispatch for candidates.
    let prefix_result = crate::gql::dispatch_plan_blob(
        graph_id,
        &prefix_blob,
        std::slice::from_ref(&prefix_plan),
        &params,
        params_blob,
        mode,
        None,
        stats,
    )
    .await?;
    let Some(rows_blob) = prefix_result.rows_blob.as_ref() else {
        return Err(RouterError::Internal(
            "candidate prefix returned no rows payload".into(),
        ));
    };
    if rows_blob.len() > MAX_CANDIDATE_PREFIX_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "candidate prefix payload of {} bytes exceeds the 1MiB admission cap",
            rows_blob.len()
        )));
    }
    let wire_rows = gleaph_gql_ic::GqlWireRows::decode_blob(rows_blob).map_err(|e| {
        RouterError::InvalidArgument(format!("candidate prefix decode failed: {e}"))
    })?;
    if wire_rows.rows.len() > MAX_CANDIDATE_PREFIX_ROWS {
        return Err(RouterError::InvalidArgument(format!(
            "candidate prefix of {} rows exceeds the 1024-row admission cap; narrow the graph pattern before ranking",
            wire_rows.rows.len()
        )));
    }
    let mut prefix_rows = Vec::with_capacity(wire_rows.rows.len());
    for row in &wire_rows.rows {
        let identity = row
            .columns
            .iter()
            .find(|(name, _)| name == CANDIDATE_ID_ALIAS)
            .map(|(_, value)| value)
            .ok_or_else(|| {
                RouterError::Internal("candidate prefix row lacks the identity column".into())
            })?;
        let gleaph_gql_ic::GqlWireValue::Bytes(id_bytes) = identity else {
            return Err(RouterError::Internal(
                "candidate identity column is not an element id".into(),
            ));
        };
        let encoded: [u8; 8] = id_bytes.as_slice().try_into().map_err(|_| {
            RouterError::InvalidArgument("candidate element id must be 8 bytes".into())
        })?;
        let global = gleaph_graph_kernel::federation::decode_global_vertex_id(
            &element_id_key,
            gleaph_graph_kernel::federation::EncodedVertexId(encoded),
        );
        if global.shard_id != shard.shard_id {
            return Err(RouterError::InvalidArgument(
                "candidate vertex shard does not match the live shard".into(),
            ));
        }
        let mut values = Vec::with_capacity(user_positions.len());
        for position in 0..user_positions.len() {
            let want = format!("{CANDIDATE_COLUMN_ALIAS_PREFIX}{position}");
            let value = row
                .columns
                .iter()
                .find(|(name, _)| *name == want)
                .map(|(_, value)| value.clone())
                .ok_or_else(|| {
                    RouterError::Internal(format!(
                        "candidate prefix row lacks retained column {want}"
                    ))
                })?;
            values.push(value);
        }
        // Slice 2 second key: same element-id decode and shard check as the
        // primary identity. Required exactly when the barrier carries a second
        // variable; absent otherwise (the same-variable slice never projects it).
        let key2 = if second_variable.is_some() {
            let identity2 = row
                .columns
                .iter()
                .find(|(name, _)| name == CANDIDATE_ID_ALIAS_2)
                .map(|(_, value)| value)
                .ok_or_else(|| {
                    RouterError::Internal(
                        "candidate prefix row lacks the second identity column".into(),
                    )
                })?;
            let gleaph_gql_ic::GqlWireValue::Bytes(id2_bytes) = identity2 else {
                return Err(RouterError::Internal(
                    "candidate second identity column is not an element id".into(),
                ));
            };
            let encoded2: [u8; 8] = id2_bytes.as_slice().try_into().map_err(|_| {
                RouterError::InvalidArgument("candidate element id must be 8 bytes".into())
            })?;
            let global2 = gleaph_graph_kernel::federation::decode_global_vertex_id(
                &element_id_key,
                gleaph_graph_kernel::federation::EncodedVertexId(encoded2),
            );
            if global2.shard_id != shard.shard_id {
                return Err(RouterError::InvalidArgument(
                    "candidate second vertex shard does not match the live shard".into(),
                ));
            }
            Some(u64::from(global2.local_vertex_id))
        } else {
            None
        };
        prefix_rows.push(CandidatePrefixRow {
            key: u64::from(global.local_vertex_id),
            key2,
            values,
        });
    }

    // Deduplicate keys into canonical ascending order for the TEXT call. No hit
    // carries row multiplicity; multiplicity is restored at join time.
    let mut keys: Vec<u64> = prefix_rows.iter().map(|row| row.key).collect();
    keys.sort_unstable();
    keys.dedup();
    if keys.len() > MAX_CANDIDATE_KEYS {
        return Err(RouterError::InvalidArgument(format!(
            "{} distinct candidate keys exceed the 256-key admission cap",
            keys.len()
        )));
    }
    let mut scores: BTreeMap<u64, u32> = BTreeMap::new();
    if !keys.is_empty() {
        let args = Encode!(&(&query, &keys)).map_err(|e| {
            RouterError::Internal(format!("candidate text call encode failed: {e}"))
        })?;
        if args.len() > MAX_CANDIDATE_CALL_BYTES {
            return Err(RouterError::InvalidArgument(format!(
                "candidate text call of {} bytes exceeds the 32KiB admission cap",
                args.len()
            )));
        }
        let mut hits = text_canister_search_candidates(target, query, keys.clone())
            .await
            .map_err(RouterError::Internal)?;
        // Threshold and compound modes filter here, on the complete candidate hit
        // set: TEXT returns every candidate match, so retention is exact (never
        // a window). Compound truncates after ranking below.
        if let CandidateBarrierMode::Threshold { cmp, bound }
        | CandidateBarrierMode::Compound { cmp, bound, .. } = barrier.mode
        {
            retain_threshold(&mut hits, cmp, bound);
        }
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for hit in hits {
            if !seen.insert(hit.key) {
                return Err(RouterError::InvalidArgument(format!(
                    "duplicate candidate text hit for document key {}",
                    hit.key
                )));
            }
            if keys.binary_search(&hit.key).is_err() {
                return Err(RouterError::InvalidArgument(format!(
                    "candidate text hit for unrequested document key {}",
                    hit.key
                )));
            }
            let vertex_id = u32::try_from(hit.key).map_err(|_| {
                RouterError::InvalidArgument(format!(
                    "candidate text document key {} does not fit a local vertex id",
                    hit.key
                ))
            })?;
            let _ = vertex_id;
            scores.insert(hit.key, hit.score);
        }
    }

    // Second round trip: slice 1 reuses the candidate keys against the second
    // triple's index; slice 2 keys off the second variable's element ids.
    // Calls are sequential; the join below is symmetric (a row survives only
    // with both scores), so ranking afterwards stays exact. Caps apply per
    // call, independently.
    let mut second_scores: BTreeMap<u64, u32> = BTreeMap::new();
    if let (Some(second), Some(second_target)) = (&barrier.second, second_target) {
        let two_variable = second.variable != barrier.variable;
        let mut second_keys: Vec<u64> = prefix_rows
            .iter()
            .filter_map(|row| second_join_key(row, two_variable))
            .collect();
        second_keys.sort_unstable();
        second_keys.dedup();
        if second_keys.len() > MAX_CANDIDATE_KEYS {
            return Err(RouterError::InvalidArgument(format!(
                "{} distinct candidate second keys exceed the 256-key admission cap",
                second_keys.len()
            )));
        }
        let second_query = resolve_scan_query(&second.query, &params)?;
        let second_args = Encode!(&(&second_query, &second_keys)).map_err(|e| {
            RouterError::Internal(format!("candidate second text call encode failed: {e}"))
        })?;
        if second_args.len() > MAX_CANDIDATE_CALL_BYTES {
            return Err(RouterError::InvalidArgument(format!(
                "candidate second text call of {} bytes exceeds the 32KiB admission cap",
                second_args.len()
            )));
        }
        let second_hits =
            text_canister_search_candidates(second_target, second_query, second_keys.clone())
                .await
                .map_err(RouterError::Internal)?;
        let mut second_seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for hit in second_hits {
            if !second_seen.insert(hit.key) {
                return Err(RouterError::InvalidArgument(format!(
                    "duplicate candidate second text hit for document key {}",
                    hit.key
                )));
            }
            if second_keys.binary_search(&hit.key).is_err() {
                return Err(RouterError::InvalidArgument(format!(
                    "candidate second text hit for unrequested document key {}",
                    hit.key
                )));
            }
            second_scores.insert(hit.key, hit.score);
        }
        // Symmetric scoreless drop: rows missing either score leave before rank.
        // Slice 2 joins on the second key — a row whose summary never scored
        // drops even when its document scored.
        prefix_rows.retain(|row| {
            second_join_key(row, two_variable).is_some_and(|key| second_scores.contains_key(&key))
        });
    }

    let (row_cap, take) = barrier_row_window(&barrier.mode, barrier.distinct, barrier.skip);
    // The skip applies AFTER ranking on the fully ordered rows (never before):
    // the barrier scan already carries the inflated `k + skip` window, and scoring
    // is all-match, so skipping here yields exactly rows `skip..skip + k`. A
    // DISTINCT tail skips nothing here: dedup runs before skip/take below.
    let ranked: Vec<(u32, CandidatePrefixRow)> = rank_candidate_rows(prefix_rows, &scores, row_cap)
        .into_iter()
        .skip(if barrier.distinct {
            0
        } else {
            barrier.skip as usize
        })
        .collect();
    // Output names follow the original trailing Project order (score included).
    if plan.output.columns.len() != columns.len() {
        return Err(RouterError::Internal(
            "candidate trailing Project and output schema disagree".into(),
        ));
    }
    let mut out_rows = Vec::with_capacity(ranked.len());
    for (score, row) in ranked {
        let mut out_cols = Vec::with_capacity(columns.len());
        // Resolve the second join key before `values` is moved into the user
        // iterator below.
        let second_lookup = barrier
            .second
            .as_ref()
            .and_then(|second| second_join_key(&row, second.variable != barrier.variable));
        let mut user_iter = row.values.into_iter();
        for (idx, out_col) in plan.output.columns.iter().enumerate() {
            if Some(idx) == barrier.score_col_idx {
                out_cols.push((
                    out_col.name.to_string(),
                    gleaph_gql_ic::GqlWireValue::Float64(f64::from(score)),
                ));
            } else if barrier
                .second
                .as_ref()
                .is_some_and(|second| idx == second.score_col_idx)
            {
                // Slice 2 reads the second score by the second key; slice 1 by
                // the shared document key. The symmetric retain above guarantees
                // presence — a miss is an internal invariant break, never silent.
                let lookup = second_lookup.ok_or_else(|| {
                    RouterError::Internal("candidate second key missing after join".into())
                })?;
                let second_score = second_scores.get(&lookup).ok_or_else(|| {
                    RouterError::Internal("candidate second score missing after join".into())
                })?;
                out_cols.push((
                    out_col.name.to_string(),
                    gleaph_gql_ic::GqlWireValue::Float64(f64::from(*second_score)),
                ));
            } else {
                let value = user_iter.next().ok_or_else(|| {
                    RouterError::Internal("candidate retained values underflow".into())
                })?;
                out_cols.push((out_col.name.to_string(), value));
            }
        }
        out_rows.push(gleaph_gql_ic::GqlWireRow { columns: out_cols });
    }
    if barrier.distinct {
        // DISTINCT is a set op on the RETURN rows: dedup the fully ordered
        // projection (first occurrence wins), then skip/take. The rank window
        // was bypassed above, so this take is exact, never a pre-dedup cut.
        dedup_wire_rows(&mut out_rows);
        out_rows = out_rows
            .into_iter()
            .skip(barrier.skip as usize)
            .take(take)
            .collect();
    }
    let row_count = out_rows.len() as u64;
    let out_blob = gleaph_gql_ic::GqlWireRows { rows: out_rows }
        .encode_blob()
        .map_err(|e| RouterError::Internal(format!("candidate result encode failed: {e}")))?;
    Ok(Some(GqlQueryResult {
        row_count,
        rows_blob: Some(out_blob),
        phase: None,
        token: None,
        truncated: Some(false),
        search_chain_receipt: None,
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
    /// Structured scan mode from the planner (`TopK` / `Threshold` / `ThresholdTopK`).
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

/// Resolve a trailing skip-`Limit` offset: a non-negative integer literal only.
/// Parameters, negative values, and non-literals fail closed (parity with the
/// planner, which lowers literal offsets and leaves anything else residual).
fn resolve_skip_offset(offset: &Expr) -> Result<u32, RouterError> {
    let unsupported = |detail: &str| {
        RouterError::InvalidArgument(format!("candidate text_score unsupported: {detail}"))
    };
    let ExprKind::Literal(gleaph_gql::Value::Int64(n)) = &offset.kind else {
        return Err(unsupported(
            "skip Limit offset must be a non-negative integer literal",
        ));
    };
    u32::try_from(*n)
        .map_err(|_| unsupported("skip Limit offset must be a non-negative integer literal"))
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

/// Candidate-scoped retrieval over the text canister's `search_candidates` endpoint.
/// The argument payload is pre-measured so the 32KiB admission cap holds on every
/// transport; every hit is validated against the requested key set by the caller.
#[cfg(target_family = "wasm")]
async fn text_canister_search_candidates(
    target: candid::Principal,
    query: String,
    keys: Vec<u64>,
) -> Result<Vec<TextHitWire>, String> {
    use ic_cdk::call::Call;

    #[derive(candid::CandidType, serde::Deserialize)]
    struct WireTextHit {
        key: u64,
        docid: u32,
        score: u32,
    }

    Call::bounded_wait(target, TEXT_SEARCH_CANDIDATES_METHOD)
        .with_args(&(&query, &keys))
        .await
        .map_err(|e| format!("text {TEXT_SEARCH_CANDIDATES_METHOD} call failed: {e}"))?
        .candid::<Result<Vec<WireTextHit>, String>>()
        .map_err(|_| format!("text {TEXT_SEARCH_CANDIDATES_METHOD} decode failed"))?
        .map(|hits| {
            hits.into_iter()
                .map(|hit| TextHitWire {
                    key: hit.key,
                    docid: hit.docid,
                    score: hit.score,
                })
                .collect()
        })
        .map_err(|detail| format!("text {TEXT_SEARCH_CANDIDATES_METHOD} rejected: {detail}"))
}

/// Native transports cannot reach a text canister: fail closed instead of scoring
/// zero rows and masquerading as an exact top-k.
#[cfg(not(target_family = "wasm"))]
async fn text_canister_search_candidates(
    _target: candid::Principal,
    _query: String,
    _keys: Vec<u64>,
) -> Result<Vec<TextHitWire>, String> {
    Err("candidate text search requires the wasm transport".to_string())
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
        // Non-leading scans route to the candidate barrier first. This bare shape
        // (no trailing Project) still fails there even though a well-formed
        // compound barrier is accepted since the compound slice.
        assert!(
            matches!(position_err, RouterError::InvalidArgument(ref msg) if msg.contains("candidate text_score unsupported")),
            "unexpected error: {position_err:?}"
        );
    }

    fn compound_text_scan_op() -> PlanOp {
        PlanOp::TextScan {
            variable: "n".into(),
            label: NodeLabelRef::from("Document"),
            property: "body".into(),
            query: ScanValue::Literal(gleaph_gql::Value::Text("index".into())),
            mode: TextScanMode::ThresholdTopK {
                cmp: CmpOp::Gt,
                bound: ScanValue::Literal(gleaph_gql::Value::Float64(0.5)),
                limit: ScanValue::Literal(gleaph_gql::Value::Int64(2)),
            },
            property_projection: None,
        }
    }

    /// Store fixture with one graph whose (`Document`, `body`) pair is interned, so
    /// `analyze_text_scan_shape` can resolve the scan's label and property.
    fn analyzed_store() -> (crate::RouterStore, GraphId) {
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
            "tenant.text.compound",
        );
        let graph_id = store
            .resolve_graph_id("tenant.text.compound")
            .expect("graph");
        store
            .admin_intern_vertex_label(admin, "tenant.text.compound", "Document")
            .expect("label");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            "tenant.text.compound",
            "body",
        );
        (store, graph_id)
    }

    #[test]
    fn shape_analysis_accepts_compound_mode_and_carries_it() {
        let (store, graph_id) = analyzed_store();
        let plan = PhysicalPlan::from_ops(vec![compound_text_scan_op(), project_call_op("score")]);
        let shape = analyze_text_scan_shape(&plan, graph_id, &store).expect("compound shape");
        assert!(matches!(
            shape.mode,
            TextScanMode::ThresholdTopK { cmp: CmpOp::Gt, .. }
        ));
        assert_eq!(shape.alias.as_deref(), Some("score"));
    }

    #[test]
    fn compound_execution_retains_threshold_then_truncates_in_ranked_order() {
        // Score-ranked window (as `sort_hits_deterministically` produced): threshold
        // excludes some of the window, truncate to k < filtered len.
        let mut window = hits(&[(9, 20), (2, 70), (5, 50), (7, 50), (3, 90)]);
        sort_hits_deterministically(&mut window);
        retain_threshold(&mut window, CmpOp::Gt, 40.0);
        window.truncate(2);
        assert_eq!(
            window.iter().map(|hit| hit.key).collect::<Vec<_>>(),
            vec![3, 2],
            "top-k of the filtered set in (score DESC, key ASC) order"
        );
    }

    #[test]
    fn compound_limit_clamps_at_max_text_search_k() {
        let mut params = BTreeMap::new();
        params.insert("k".to_string(), gleaph_gql::Value::Int64(500));
        let limit = ScanValue::Parameter("$k".into());
        let requested_k = resolve_scan_limit(&limit, &params).expect("param limit");
        let request_k = requested_k.min(MAX_TEXT_SEARCH_K);
        assert_eq!(request_k, MAX_TEXT_SEARCH_K, "clamped at the window width");
        assert!(requested_k > MAX_TEXT_SEARCH_K, "truncation flag set");
    }

    #[test]
    fn compound_empty_post_threshold_set_yields_empty_seeds_without_error() {
        let mut window = hits(&[(1, 30), (2, 20)]);
        retain_threshold(&mut window, CmpOp::Gt, 90.0);
        assert!(window.is_empty());
        window.truncate(5);
        let seeded = build_text_scan_seeds("n", Some("score"), &[], ShardId::new(0), &window)
            .expect("empty hits seed cleanly");
        assert!(seeded[&ShardId::new(0)].rows.is_empty());
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
            vec![],
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

#[cfg(test)]
mod candidate_barrier_tests {
    use super::super::gql_text_scan::*;
    use gleaph_gql::ast::{Expr, ExprKind};
    use gleaph_gql_planner::output_schema::{OutputBindingKind, OutputColumn, OutputSchema};
    use gleaph_gql_planner::plan::{
        NodeLabelRef, PhysicalPlan, PlanAnnotations, PlanDiagnostics, PlanOp, ProjectColumn,
        ScanValue, TextScanMode,
    };
    use std::collections::BTreeMap;

    fn test_plan(ops: Vec<PlanOp>) -> PhysicalPlan {
        PhysicalPlan {
            ops,
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
            output: OutputSchema {
                columns: vec![
                    OutputColumn {
                        name: "d.title".into(),
                        kind: OutputBindingKind::Scalar,
                        source_var: None,
                    },
                    OutputColumn {
                        name: "score".into(),
                        kind: OutputBindingKind::Scalar,
                        source_var: None,
                    },
                ],
            },
            binding_layout: gleaph_gql_planner::binding_layout::derive_binding_layout(&[]),
        }
    }

    fn node_scan_d() -> PlanOp {
        PlanOp::NodeScan {
            variable: "d".into(),
            label: Some(NodeLabelRef::from("Document")),
            property_projection: None,
        }
    }

    fn barrier_scan(limit: i64) -> PlanOp {
        PlanOp::TextScan {
            variable: "d".into(),
            label: NodeLabelRef::from("Document"),
            property: "body".into(),
            query: ScanValue::Literal(gleaph_gql::Value::Text("hello".into())),
            mode: TextScanMode::TopK {
                limit: ScanValue::Literal(gleaph_gql::Value::Int64(limit)),
            },
            property_projection: None,
        }
    }

    fn score_call_expr() -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: gleaph_gql::ast::ObjectName::simple("text_score"),
            args: vec![
                Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable("d".into()))),
                    property: "body".into(),
                }),
                Expr::new(ExprKind::Literal(gleaph_gql::Value::Text("hello".into()))),
            ],
            distinct: false,
        })
    }

    fn barrier_scan_threshold() -> PlanOp {
        PlanOp::TextScan {
            variable: "d".into(),
            label: NodeLabelRef::from("Document"),
            property: "body".into(),
            query: ScanValue::Literal(gleaph_gql::Value::Text("hello".into())),
            mode: TextScanMode::Threshold {
                cmp: CmpOp::Gt,
                bound: ScanValue::Literal(gleaph_gql::Value::Float64(0.5)),
            },
            property_projection: None,
        }
    }

    fn tail_project_no_score() -> PlanOp {
        PlanOp::Project {
            columns: vec![ProjectColumn {
                expr: Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable("d".into()))),
                    property: "title".into(),
                }),
                alias: None,
            }],
            distinct: false,
        }
    }

    fn tail_project() -> PlanOp {
        PlanOp::Project {
            columns: vec![
                ProjectColumn {
                    expr: Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("d".into()))),
                        property: "title".into(),
                    }),
                    alias: None,
                },
                ProjectColumn {
                    expr: score_call_expr(),
                    alias: Some("score".into()),
                },
            ],
            distinct: false,
        }
    }

    fn tail_project_distinct() -> PlanOp {
        let PlanOp::Project { columns, .. } = tail_project() else {
            panic!("tail");
        };
        PlanOp::Project {
            columns,
            distinct: true,
        }
    }

    fn params() -> BTreeMap<String, gleaph_gql::Value> {
        BTreeMap::new()
    }

    #[test]
    fn barrier_shape_accepts_target_layout() {
        let plan = test_plan(vec![node_scan_d(), barrier_scan(20), tail_project()]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert_eq!(shape.scan_idx, 1);
        assert_eq!(shape.variable, "d");
        assert!(matches!(
            shape.mode,
            CandidateBarrierMode::TopK { limit: 20 }
        ));
        assert_eq!(shape.score_col_idx, Some(1));
        assert_eq!(shape.user_col_count, 1);
    }

    #[test]
    fn barrier_shape_rejects_leading_scan_limit_prefix_and_over_limit() {
        // Leading scan takes the seed path.
        let plan = test_plan(vec![barrier_scan(20), tail_project()]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        // A Limit inside the prefix narrows candidates before scoring.
        let plan = test_plan(vec![
            node_scan_d(),
            PlanOp::Limit {
                count: None,
                offset: None,
            },
            barrier_scan(20),
            tail_project(),
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        // LIMIT beyond the admission cap fails closed.
        let plan = test_plan(vec![node_scan_d(), barrier_scan(2048), tail_project()]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
    }

    #[test]
    fn barrier_shape_rejects_score_mismatch_and_wrapped_score() {
        // No score mention at all: accepted with no score column (threshold-only
        // RETURN shape; the same layout holds for TopK).
        let mut project = tail_project();
        let PlanOp::Project { columns, .. } = &mut project else {
            panic!("project");
        };
        columns[1].expr = Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(1)));
        let plan = test_plan(vec![node_scan_d(), barrier_scan(20), project]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert_eq!(shape.score_col_idx, None);
        assert_eq!(shape.user_col_count, 2);
        // Score call wrapped in arithmetic is not the entire expression.
        let mut project = tail_project();
        let PlanOp::Project { columns, .. } = &mut project else {
            panic!("project");
        };
        columns[1].expr = Expr::new(ExprKind::BinaryOp {
            left: Box::new(score_call_expr()),
            op: gleaph_gql::ast::BinaryOp::Add,
            right: Box::new(Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(1)))),
        });
        let plan = test_plan(vec![node_scan_d(), barrier_scan(20), project]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
    }

    #[test]
    fn barrier_shape_accepts_threshold_with_and_without_score_column() {
        // Threshold-only RETURN: no residual score column.
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan_threshold(),
            tail_project_no_score(),
        ]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(matches!(
            shape.mode,
            CandidateBarrierMode::Threshold { bound, .. } if bound == 0.5
        ));
        assert_eq!(shape.score_col_idx, None);
        assert_eq!(shape.user_col_count, 1);
        // Projected score alongside the threshold still resolves to the same barrier.
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan_threshold(),
            tail_project(),
        ]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(matches!(shape.mode, CandidateBarrierMode::Threshold { .. }));
        assert_eq!(shape.score_col_idx, Some(1));
        assert_eq!(shape.user_col_count, 1);
    }

    #[test]
    // Intentional contract change: the candidate compound barrier
    // (`ThresholdTopK` after a prefix) is now accepted — the threshold applies on
    // the complete candidate hit set and ranking truncates after, so the result
    // stays exact. The former rejection was the pre-compound slice boundary.
    fn barrier_shape_accepts_compound_after_prefix() {
        let mut scan = barrier_scan_threshold();
        let PlanOp::TextScan { mode, .. } = &mut scan else {
            panic!("scan");
        };
        *mode = TextScanMode::ThresholdTopK {
            cmp: CmpOp::Gt,
            bound: ScanValue::Literal(gleaph_gql::Value::Float64(0.5)),
            limit: ScanValue::Literal(gleaph_gql::Value::Int64(10)),
        };
        let plan = test_plan(vec![node_scan_d(), scan, tail_project_no_score()]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(matches!(
            shape.mode,
            CandidateBarrierMode::Compound { bound, limit: 10, .. } if bound == 0.5
        ));
        // The row LIMIT cap is reapplied to the compound limit: 0 and >1024
        // fail closed, exactly like the TopK admission gate.
        for bad_limit in [0, 1025] {
            let mut bad_scan = barrier_scan_threshold();
            let PlanOp::TextScan { mode, .. } = &mut bad_scan else {
                panic!("scan");
            };
            *mode = TextScanMode::ThresholdTopK {
                cmp: CmpOp::Gt,
                bound: ScanValue::Literal(gleaph_gql::Value::Float64(0.5)),
                limit: ScanValue::Literal(gleaph_gql::Value::Int64(bad_limit)),
            };
            let bad_plan = test_plan(vec![node_scan_d(), bad_scan, tail_project_no_score()]);
            assert!(
                analyze_candidate_barrier_shape(&bad_plan, &params()).is_err(),
                "compound limit {bad_limit} must fail closed"
            );
        }
    }

    fn skip_limit_op(skip: i64) -> PlanOp {
        PlanOp::Limit {
            count: None,
            offset: Some(Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(skip)))),
        }
    }

    #[test]
    fn barrier_shape_accepts_trailing_skip_and_rejects_non_skip_limits() {
        // A pure skip-Limit parks after the RETURN: the shape carries the skip.
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan(15),
            tail_project(),
            skip_limit_op(5),
        ]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert_eq!(shape.skip, 5);
        assert!(matches!(
            shape.mode,
            CandidateBarrierMode::TopK { limit: 15 }
        ));
        // A row count is a second cap, not a skip.
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan(15),
            tail_project(),
            PlanOp::Limit {
                count: Some(Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(5)))),
                offset: Some(Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(5)))),
            },
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        // A parameterized offset fails closed (parity with the planner).
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan(15),
            tail_project(),
            PlanOp::Limit {
                count: None,
                offset: Some(Expr::new(ExprKind::Parameter("$skip".into()))),
            },
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        // A negative offset fails closed.
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan(15),
            tail_project(),
            skip_limit_op(-1),
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        // A skip after a threshold barrier stays unsupported: the threshold
        // lowering carries no TopK to fuse an OFFSET through.
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan_threshold(),
            tail_project_no_score(),
            skip_limit_op(5),
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
    }

    #[test]
    fn barrier_shape_carries_distinct_across_modes() {
        // A DISTINCT tail is accepted and carried as a flag; the plain tail
        // stays flag-free.
        let plan = test_plan(vec![node_scan_d(), barrier_scan(20), tail_project()]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(!shape.distinct);
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan(20),
            tail_project_distinct(),
        ]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(shape.distinct);
        assert!(matches!(
            shape.mode,
            CandidateBarrierMode::TopK { limit: 20 }
        ));
        // Threshold + DISTINCT opens together: the dedup stage is mode-agnostic.
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan_threshold(),
            tail_project_no_score(),
        ]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(!shape.distinct);
        let mut scan = barrier_scan_threshold();
        let PlanOp::Project { columns, .. } = tail_project_no_score() else {
            panic!("tail");
        };
        let distinct_tail = PlanOp::Project {
            columns,
            distinct: true,
        };
        let plan = test_plan(vec![node_scan_d(), scan.clone(), distinct_tail]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(shape.distinct);
        assert!(matches!(shape.mode, CandidateBarrierMode::Threshold { .. }));
        // Compound + DISTINCT opens through the same flag.
        let PlanOp::TextScan { mode, .. } = &mut scan else {
            panic!("scan");
        };
        *mode = TextScanMode::ThresholdTopK {
            cmp: CmpOp::Gt,
            bound: ScanValue::Literal(gleaph_gql::Value::Float64(0.5)),
            limit: ScanValue::Literal(gleaph_gql::Value::Int64(10)),
        };
        let PlanOp::Project { columns, .. } = tail_project_no_score() else {
            panic!("tail");
        };
        let plan = test_plan(vec![
            node_scan_d(),
            scan,
            PlanOp::Project {
                columns,
                distinct: true,
            },
        ]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(shape.distinct);
        assert!(matches!(
            shape.mode,
            CandidateBarrierMode::Compound { limit: 10, .. }
        ));
        // DISTINCT + skip is accepted (dedup runs before skip); threshold +
        // skip stays rejected with or without DISTINCT.
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan(15),
            tail_project_distinct(),
            skip_limit_op(5),
        ]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(shape.distinct);
        assert_eq!(shape.skip, 5);
        let PlanOp::Project { columns, .. } = tail_project_no_score() else {
            panic!("tail");
        };
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan_threshold(),
            PlanOp::Project {
                columns,
                distinct: true,
            },
            skip_limit_op(5),
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
    }

    #[test]
    fn barrier_row_window_bypasses_cap_for_distinct() {
        // Plain tails keep the `k + skip` window as the row cap.
        assert_eq!(
            barrier_row_window(&CandidateBarrierMode::TopK { limit: 10 }, false, 0),
            (10, 10)
        );
        // A DISTINCT tail ranks the full candidate set and takes after dedup.
        assert_eq!(
            barrier_row_window(&CandidateBarrierMode::TopK { limit: 10 }, true, 0),
            (usize::MAX, 10)
        );
        // With a parked skip the take is the fused window minus the skip
        // (LIMIT k OFFSET n fuses to a k + n window), never the window.
        assert_eq!(
            barrier_row_window(&CandidateBarrierMode::TopK { limit: 15 }, true, 5),
            (usize::MAX, 10)
        );
        assert_eq!(
            barrier_row_window(
                &CandidateBarrierMode::Compound {
                    cmp: CmpOp::Gt,
                    bound: 0.5,
                    limit: 15
                },
                true,
                5
            ),
            (usize::MAX, 10)
        );
        // Threshold never windows, with or without DISTINCT.
        for distinct in [false, true] {
            assert_eq!(
                barrier_row_window(
                    &CandidateBarrierMode::Threshold {
                        cmp: CmpOp::Gt,
                        bound: 0.5
                    },
                    distinct,
                    0
                ),
                (usize::MAX, usize::MAX)
            );
        }
    }

    #[test]
    fn dedup_wire_rows_keeps_first_occurrence_on_whole_rows() {
        use gleaph_gql_ic::{GqlWireRow, GqlWireValue};
        let row = |title: &str, score: f64| GqlWireRow {
            columns: vec![
                ("title".to_string(), GqlWireValue::Text(title.into())),
                ("s".to_string(), GqlWireValue::Float64(score)),
            ],
        };
        let mut rows = vec![
            row("a", 3.0),
            row("b", 2.0),
            row("a", 3.0),
            // Same title, different score: a whole-row key keeps both.
            // A document-identity dedup would wrongly collapse these.
            row("a", 1.0),
            row("b", 2.0),
        ];
        dedup_wire_rows(&mut rows);
        assert_eq!(rows, vec![row("a", 3.0), row("b", 2.0), row("a", 1.0)]);
        // NULL titles dedup together, matching the Graph whole-row contract.
        let mut nulls = vec![
            GqlWireRow {
                columns: vec![("title".to_string(), GqlWireValue::Null)],
            },
            GqlWireRow {
                columns: vec![("title".to_string(), GqlWireValue::Null)],
            },
        ];
        dedup_wire_rows(&mut nulls);
        assert_eq!(nulls.len(), 1);
    }

    fn score_call_on(property: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: gleaph_gql::ast::ObjectName::simple("text_score"),
            args: vec![
                Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable("d".into()))),
                    property: property.into(),
                }),
                Expr::new(ExprKind::Literal(gleaph_gql::Value::Text("hello".into()))),
            ],
            distinct: false,
        })
    }

    fn tail_project_dual() -> PlanOp {
        PlanOp::Project {
            columns: vec![
                ProjectColumn {
                    expr: Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("d".into()))),
                        property: "title".into(),
                    }),
                    alias: None,
                },
                ProjectColumn {
                    expr: score_call_expr(),
                    alias: Some("s1".into()),
                },
                ProjectColumn {
                    expr: score_call_on("blurb"),
                    alias: Some("s2".into()),
                },
            ],
            distinct: false,
        }
    }

    #[test]
    fn barrier_shape_accepts_second_same_variable_call() {
        let plan = test_plan(vec![node_scan_d(), barrier_scan(20), tail_project_dual()]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert_eq!(shape.score_col_idx, Some(1));
        let second = shape.second.expect("second score recorded");
        assert_eq!(second.property, "blurb");
        assert_eq!(second.score_col_idx, 2);
        // The second join composes with a parked skip.
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan(15),
            tail_project_dual(),
            skip_limit_op(5),
        ]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(shape.second.is_some());
        assert_eq!(shape.skip, 5);
    }

    #[test]
    fn barrier_shape_rejects_second_call_violations() {
        // The scanned call twice is not a dual score.
        let PlanOp::Project { columns, .. } = tail_project() else {
            panic!("tail");
        };
        let mut dup = columns.clone();
        dup.push(ProjectColumn {
            expr: score_call_expr(),
            alias: Some("s1b".into()),
        });
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan(20),
            PlanOp::Project {
                columns: dup,
                distinct: false,
            },
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        let _ = plan;
        // A second variable with no prefix binding stays out of slice 2: the
        // label cannot be proven.
        let other_var = Expr::new(ExprKind::FunctionCall {
            name: gleaph_gql::ast::ObjectName::simple("text_score"),
            args: vec![
                Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable("s".into()))),
                    property: "text".into(),
                }),
                Expr::new(ExprKind::Literal(gleaph_gql::Value::Text("hello".into()))),
            ],
            distinct: false,
        });
        let PlanOp::Project { columns, .. } = tail_project() else {
            panic!("tail");
        };
        let mut mixed = columns.clone();
        mixed.push(ProjectColumn {
            expr: other_var,
            alias: Some("s2".into()),
        });
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan(20),
            PlanOp::Project {
                columns: mixed,
                distinct: false,
            },
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        // Threshold and compound barriers stay single-score.
        for scan in [barrier_scan_threshold(), {
            let mut scan = barrier_scan_threshold();
            let PlanOp::TextScan { mode, .. } = &mut scan else {
                panic!("scan");
            };
            *mode = TextScanMode::ThresholdTopK {
                cmp: CmpOp::Gt,
                bound: ScanValue::Literal(gleaph_gql::Value::Float64(0.5)),
                limit: ScanValue::Literal(gleaph_gql::Value::Int64(10)),
            };
            scan
        }] {
            let plan = test_plan(vec![node_scan_d(), scan, tail_project_dual()]);
            assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        }
        // DISTINCT plus a second call stays out of both slices.
        let PlanOp::Project { columns, .. } = tail_project_dual() else {
            panic!("tail");
        };
        let plan = test_plan(vec![
            node_scan_d(),
            barrier_scan(20),
            PlanOp::Project {
                columns,
                distinct: true,
            },
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
    }

    fn node_scan_s() -> PlanOp {
        PlanOp::NodeScan {
            variable: "s".into(),
            label: Some(NodeLabelRef::from("Summary")),
            property_projection: None,
        }
    }

    fn score_call_on_var(variable: &str, property: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: gleaph_gql::ast::ObjectName::simple("text_score"),
            args: vec![
                Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable(variable.into()))),
                    property: property.into(),
                }),
                Expr::new(ExprKind::Literal(gleaph_gql::Value::Text("hello".into()))),
            ],
            distinct: false,
        })
    }

    fn tail_project_dual_var() -> PlanOp {
        PlanOp::Project {
            columns: vec![
                ProjectColumn {
                    expr: Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("d".into()))),
                        property: "title".into(),
                    }),
                    alias: None,
                },
                ProjectColumn {
                    expr: score_call_expr(),
                    alias: Some("s1".into()),
                },
                ProjectColumn {
                    expr: score_call_on_var("s", "text"),
                    alias: Some("s2".into()),
                },
            ],
            distinct: false,
        }
    }

    #[test]
    fn barrier_shape_accepts_second_variable_call() {
        let plan = test_plan(vec![
            node_scan_d(),
            node_scan_s(),
            barrier_scan(20),
            tail_project_dual_var(),
        ]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert_eq!(shape.score_col_idx, Some(1));
        let second = shape.second.expect("second score recorded");
        assert_eq!(second.variable, "s");
        assert_eq!(second.label, "Summary");
        assert_eq!(second.property, "text");
        assert_eq!(second.score_col_idx, 2);
        // The second join composes with a parked skip.
        let plan = test_plan(vec![
            node_scan_d(),
            node_scan_s(),
            barrier_scan(15),
            tail_project_dual_var(),
            skip_limit_op(5),
        ]);
        let shape = analyze_candidate_barrier_shape(&plan, &params()).expect("shape");
        assert!(shape.second.is_some());
        assert_eq!(shape.skip, 5);
    }

    #[test]
    fn barrier_shape_rejects_second_variable_violations() {
        // Ambiguously labeled second variable: the TEXT triple is unprovable.
        let ambiguous = PlanOp::NodeScan {
            variable: "s".into(),
            label: Some(NodeLabelRef::from("Other")),
            property_projection: None,
        };
        let plan = test_plan(vec![
            node_scan_d(),
            node_scan_s(),
            ambiguous,
            barrier_scan(20),
            tail_project_dual_var(),
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        // A third call stays out of both slices.
        let PlanOp::Project { columns, .. } = tail_project_dual_var() else {
            panic!("tail");
        };
        let mut triple = columns.clone();
        triple.push(ProjectColumn {
            expr: score_call_on("blurb"),
            alias: Some("s3".into()),
        });
        let plan = test_plan(vec![
            node_scan_d(),
            node_scan_s(),
            barrier_scan(20),
            PlanOp::Project {
                columns: triple,
                distinct: false,
            },
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        // Threshold and compound barriers stay single-score for slice 2 too.
        for scan in [barrier_scan_threshold(), {
            let mut scan = barrier_scan_threshold();
            let PlanOp::TextScan { mode, .. } = &mut scan else {
                panic!("scan");
            };
            *mode = TextScanMode::ThresholdTopK {
                cmp: CmpOp::Gt,
                bound: ScanValue::Literal(gleaph_gql::Value::Float64(0.5)),
                limit: ScanValue::Literal(gleaph_gql::Value::Int64(10)),
            };
            scan
        }] {
            let plan = test_plan(vec![
                node_scan_d(),
                node_scan_s(),
                scan,
                tail_project_dual_var(),
            ]);
            assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
        }
        // DISTINCT plus a slice-2 second call stays rejected.
        let PlanOp::Project { columns, .. } = tail_project_dual_var() else {
            panic!("tail");
        };
        let plan = test_plan(vec![
            node_scan_d(),
            node_scan_s(),
            barrier_scan(20),
            PlanOp::Project {
                columns,
                distinct: true,
            },
        ]);
        assert!(analyze_candidate_barrier_shape(&plan, &params()).is_err());
    }

    #[test]
    fn second_join_key_selects_identity_per_slice() {
        use gleaph_gql_ic::GqlWireValue;
        let row = CandidatePrefixRow {
            key: 11,
            key2: Some(77),
            values: vec![GqlWireValue::Null],
        };
        // Slice 1 reuses the shared document key.
        assert_eq!(second_join_key(&row, false), Some(11));
        // Slice 2 keys off the second variable's element id.
        assert_eq!(second_join_key(&row, true), Some(77));
        // A slice-2 row without a second identity never joins: the retain
        // below drops it exactly like a scoreless row.
        let mut rows = vec![
            row,
            CandidatePrefixRow {
                key: 12,
                key2: None,
                values: vec![GqlWireValue::Null],
            },
        ];
        let second_scores: BTreeMap<u64, u32> = [(77, 5)].into_iter().collect();
        rows.retain(|row| {
            second_join_key(row, true).is_some_and(|key| second_scores.contains_key(&key))
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, 11);
        // Ranking stays on the first score: s2 never reorders.
        let scores: BTreeMap<u64, u32> = [(11, 30), (12, 90)].into_iter().collect();
        let ranked = rank_candidate_rows(rows, &scores, 10);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, 30);
    }

    #[test]
    fn resolve_residual_call_accepts_bare_calls_only() {
        let (var, prop, query) =
            resolve_residual_call(&score_call_on("blurb")).expect("bare call resolves");
        assert_eq!(var, "d");
        assert_eq!(prop, "blurb");
        assert!(matches!(
            query,
            ScanValue::Literal(gleaph_gql::Value::Text(_))
        ));
        // A wrapped call (score arithmetic) is not a joinable residual.
        let wrapped = Expr::new(ExprKind::BinaryOp {
            op: gleaph_gql::ast::BinaryOp::Add,
            left: Box::new(score_call_expr()),
            right: Box::new(score_call_on("blurb")),
        });
        assert!(resolve_residual_call(&wrapped).is_none());
        assert!(resolve_residual_call(&Expr::new(ExprKind::Variable("d".into()))).is_none());
    }

    #[test]
    fn rank_preserves_multiplicity_drops_unmatched_and_truncates() {
        use gleaph_gql_ic::GqlWireValue;
        let rows = vec![
            CandidatePrefixRow {
                key: 3,
                key2: None,
                values: vec![GqlWireValue::Int64(3)],
            },
            CandidatePrefixRow {
                key: 1,
                key2: None,
                values: vec![GqlWireValue::Int64(1)],
            },
            CandidatePrefixRow {
                key: 3,
                key2: None,
                values: vec![GqlWireValue::Int64(33)],
            },
            CandidatePrefixRow {
                key: 9,
                key2: None,
                values: vec![GqlWireValue::Int64(9)],
            },
        ];
        let scores: BTreeMap<u64, u32> = [(1, 10), (3, 30)].into_iter().collect();
        let ranked = rank_candidate_rows(rows, &scores, 20);
        // Key 9 unmatched (dropped); key 3 twice (both prefix paths survive).
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0].1.key, 3);
        assert_eq!(ranked[1].1.key, 3);
        assert_eq!(ranked[2].1.key, 1);
        // Stable within identical (score, key): first prefix occurrence first.
        assert!(matches!(ranked[0].1.values[0], GqlWireValue::Int64(3)));
        assert!(matches!(ranked[1].1.values[0], GqlWireValue::Int64(33)));
        // Truncation keeps the head of the ranked order.
        let rows = vec![
            CandidatePrefixRow {
                key: 1,
                key2: None,
                values: vec![],
            },
            CandidatePrefixRow {
                key: 3,
                key2: None,
                values: vec![],
            },
        ];
        let ranked = rank_candidate_rows(rows, &scores, 1);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, 30);
    }

    #[test]
    fn rank_breaks_score_ties_by_key_ascending() {
        use gleaph_gql_ic::GqlWireValue;
        let rows = vec![
            CandidatePrefixRow {
                key: 7,
                key2: None,
                values: vec![GqlWireValue::Null],
            },
            CandidatePrefixRow {
                key: 2,
                key2: None,
                values: vec![GqlWireValue::Null],
            },
        ];
        let scores: BTreeMap<u64, u32> = [(7, 5), (2, 5)].into_iter().collect();
        let ranked = rank_candidate_rows(rows, &scores, 10);
        assert_eq!(ranked[0].1.key, 2);
        assert_eq!(ranked[1].1.key, 7);
    }
}
