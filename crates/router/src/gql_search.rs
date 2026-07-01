//! Router-side lowering of GQL `SEARCH ... IN (VECTOR INDEX ... FOR ... LIMIT ...)`
//! (ADR 0034 Slices 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15 and 16).
//!
//! This module is the execution boundary between the provider-neutral GQL planner
//! (`PlanOp::Search`) and the Router-owned vector-index catalog / canister dispatch. It supports
//! a narrow leading `NodeScan + Search` shape, one top-level non-leading `SEARCH` after a bound
//! vertex, one to eight `AND`-connected same-binding equality `SEARCH ... WHERE` predicates on
//! distinct properties, one same-binding numeric range `SEARCH ... WHERE` predicate, exactly two
//! same-binding numeric range predicates on the same property forming one lower (`>`/`>=`) and one
//! upper (`<`/`<=`) bound, one equality arm plus one one-sided range arm on distinct properties,
//! one equality arm plus two same-property range arms on a distinct property, two to eight
//! `OR`-connected same-binding same-property equality predicates, or two to eight
//! `OR`-connected same-binding pure equality predicates where property names may repeat or differ
//! (Slices 6, 7, 8, 9, 10, 11, 12, 13, 14, 15 and 16). All unsupported shapes are rejected with
//! explicit `InvalidArgument` errors. For a leading search it dispatches the remaining graph-tail
//! plan from row-shaped vector-search seeds. For a non-leading search it attaches an explicit
//! per-shard resolved-search relation to the normal read dispatch.

use candid::Encode;
use gleaph_graph_kernel::index::{PostingHit, PostingHitPage, PropertyPostingCursor};
use std::collections::BTreeMap;

use crate::planner_stats::RouterGraphStats;
use gleaph_gql::ast::ExprKind;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_planner::plan::{
    NodeLabelRef, PhysicalPlan, PlanOp, SearchOutputKind, SearchOutputPlan, SearchProviderPlan, Str,
};
use gleaph_graph_kernel::entry::{GraphId, PropertyId, VertexLabelId};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    IndexEqualSpec, LookupEqualPageRequest, LookupIntersectionPageRequest,
    LookupRangeIntersectionPageRequest, MAX_INDEX_VALUE_KEY_BYTES,
};
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, GqlExecutionMode, GqlQueryResult, ResolvedSearchVertexHitWire,
    ResolvedSearchWire, SeedBindingsWire, SeedFloat64Binding, SeedRowWire, SeedVertexBinding,
};
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_SEARCH_FILTER_CANDIDATES, MAX_VECTOR_SEARCH_TOP_K, VectorMetric, VectorOutputShape,
    VectorSearchHit, VectorSearchRequest, VectorSubject,
};

use crate::RouterStore;
use crate::facade::stable::{embedding_name_catalog, indexed_catalog, vector_index_catalog};
use crate::federation::{
    empty_execute_plan_result, federated_merge_mode_from_plans, merge_execute_plan_result,
};
use crate::graph_client::execute_plan_on_graph;
use crate::index_client::RouterIndexClient;
use crate::state::RouterError;

/// Execute a GQL plan whose leading ops are a supported `SEARCH` vector-search prefix.
///
/// Returns `Ok(None)` if the plan does not contain `PlanOp::Search` at all, allowing the caller
/// to fall through to the normal dispatch path. Returns `Err` for any `SEARCH` shape that is not
/// accepted by Slice 3.
#[allow(clippy::needless_borrow)]
pub(crate) async fn try_execute_gql_search<V, Vf>(
    plan: &PhysicalPlan,
    graph_id: GraphId,
    params_blob: &[u8],
    mode: GqlExecutionMode,
    stats: &RouterGraphStats,
    store: &RouterStore,
    caller: candid::Principal,
    vector_search: V,
) -> Result<Option<GqlQueryResult>, RouterError>
where
    V: FnOnce(candid::Principal, VectorSearchRequest) -> Vf,
    Vf: std::future::Future<
            Output = Result<gleaph_graph_kernel::vector_index::VectorSearchResult, RouterError>,
        >,
{
    if !gleaph_gql_planner::plan_contains_search(plan) {
        return Ok(None);
    }

    if mode != GqlExecutionMode::Query {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH lowering only supports query mode in this slice".into(),
        ));
    }

    let position = analyze_search_shape(plan, graph_id, store)?;
    let params = gleaph_gql_ic::wire::decode_gql_params_blob(params_blob).map_err(|e| {
        RouterError::InvalidArgument(format!("failed to decode GQL parameters: {e}"))
    })?;

    // DML must not appear in the executable part of the plan. For the leading prefix the search
    // operators are stripped before dispatch; for non-leading the full plan is dispatched.
    let executable_plan = match &position {
        SearchPosition::Leading(_) => {
            strip_search_prefix(plan, search_shape_from_position(&position))?
        }
        SearchPosition::NonLeading(_) => plan.clone(),
    };
    if executable_plan.has_dml() {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH cannot be followed by mutation operators in this slice".into(),
        ));
    }

    let shape = search_shape_from_position(&position);

    let (index_id, def) = resolve_vector_index(graph_id, &shape.index_name)?;
    let query = resolve_query_bytes(&shape.query_expr, &params)?;
    let top_k = resolve_limit_u32(&shape.limit_expr, &params)?;

    if top_k == 0 || top_k > MAX_VECTOR_SEARCH_TOP_K {
        return Err(RouterError::InvalidArgument(format!(
            "SEARCH LIMIT must be in 1..={MAX_VECTOR_SEARCH_TOP_K}"
        )));
    }
    let expected_bytes = def.encoding.stride_bytes(def.dims) as usize;
    if query.len() != expected_bytes {
        return Err(RouterError::InvalidArgument(format!(
            "SEARCH query byte length {} does not match dims*stride {}",
            query.len(),
            expected_bytes
        )));
    }

    // Validate that the requested output shape is honest for the index metric.
    let output_kind = shape.output_kind;
    match (output_kind, def.metric.output_shape()) {
        (SearchOutputKind::Score, VectorOutputShape::Score)
        | (SearchOutputKind::Distance, VectorOutputShape::Distance) => {}
        _ => {
            return Err(RouterError::InvalidArgument(format!(
                "SEARCH output shape {:?} is not supported for metric {:?}",
                output_kind, def.metric
            )));
        }
    }

    // Resolve the vector target and fail closed on the dynamic dispatch gate before any vector
    // canister call (including the empty-candidate early return below). This keeps GQL SEARCH
    // aligned with the public `vector_search` activation contract (ADR 0031 Slice 4).
    let target = def
        .target
        .ok_or_else(|| RouterError::Conflict(format!("vector index {index_id} has no target set")))?
        .canister;
    vector_index_catalog::assert_vector_search_dispatch_ready(graph_id, store, &def)?;

    // ADR 0034 Slice 6/7: a filtered search resolves a bounded label-scoped candidate allowlist
    // from the Property Index before asking Vector Index to rank within that set. The same path
    // is used for leading and non-leading positions once the searched label has been proved.
    let candidate_subjects = match &shape.filter {
        Some(filter) => {
            let label_id = shape.filter_label_id.ok_or_else(|| {
                RouterError::InvalidArgument(
                    "SEARCH ... WHERE requires a statically proved label for the searched binding in this slice".into(),
                )
            })?;
            let candidates = resolve_filtered_candidates(
                graph_id,
                store,
                label_id,
                &shape.binding,
                filter,
                &params,
            )
            .await?;
            if candidates.is_empty() {
                // Empty candidate set: skip the vector canister and dispatch an explicit empty
                // relation so global aggregates still produce one count=0 row. The dispatch gate
                // was already checked above, so this path does not bypass it.
                return dispatch_empty_filtered_search(
                    plan,
                    graph_id,
                    params_blob,
                    &params,
                    mode,
                    stats,
                    store,
                    caller,
                    &position,
                    &shape,
                    def.metric,
                )
                .await;
            }
            Some(candidates)
        }
        None => None,
    };
    let search_req = VectorSearchRequest {
        index_id,
        query,
        encoding: def.encoding,
        dims: def.dims,
        metric: def.metric,
        top_k,
        candidate_subjects: candidate_subjects.clone(),
    };
    let result = vector_search(target, search_req).await?;

    match position {
        SearchPosition::Leading(shape) => {
            let seeds_by_shard = build_search_seeds(
                &shape.binding,
                &shape.output_alias,
                &shape.required_label_ids,
                &result.hits,
                def.metric,
            )?;

            let stripped_plan = strip_search_prefix(plan, &shape)?;
            let stripped_plan_blob = gleaph_gql_planner::wire::encode_block_plans(
                std::slice::from_ref(&stripped_plan),
                false,
            )
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

            dispatch_search_read_plan(
                graph_id,
                plan,
                &stripped_plan_blob,
                &stripped_plan,
                seeds_by_shard,
                params_blob,
                mode,
                stats,
                store,
            )
            .await
            .map(Some)
        }
        SearchPosition::NonLeading(shape) => {
            let resolved_search_by_shard = build_resolved_search_wires(
                &shape.binding,
                &shape.output_alias,
                &result.hits,
                def.metric,
                graph_id,
                store,
            )?;

            let plan_blob =
                gleaph_gql_planner::wire::encode_block_plans(std::slice::from_ref(plan), false)
                    .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

            crate::gql::dispatch_plan_blob_with_search(
                graph_id,
                &plan_blob,
                std::slice::from_ref(plan),
                &params,
                params_blob,
                mode,
                None,
                stats,
                Some(resolved_search_by_shard),
                caller,
            )
            .await
            .map(Some)
        }
    }
}

fn search_shape_from_position(position: &SearchPosition) -> &SearchShape {
    match position {
        SearchPosition::Leading(shape) | SearchPosition::NonLeading(shape) => shape,
    }
}

/// One accepted SEARCH ... WHERE equality predicate on the searched binding.
///
/// The property side is normalized to `(property_name)`. The value side is the literal or
/// parameter expression; it is resolved once against the decoded parameters at execution time.
#[derive(Debug)]
struct SearchFilterArm {
    property_name: String,
    value_expr: gleaph_gql::ast::Expr,
}

/// One accepted SEARCH ... WHERE range predicate on the searched binding.
///
/// The operator is normalized so that the predicate reads `binding.property OP value`.
#[derive(Debug, Clone)]
struct SearchFilterRange {
    property_name: String,
    op: gleaph_gql::ast::CmpOp,
    value_expr: gleaph_gql::ast::Expr,
}

/// ADR 0034 Slice 8/9/10/11/12/14/15/16/17: any bounded number of same-binding equality
/// conjuncts on distinct properties, exactly one numeric range predicate, exactly two
/// same-property range predicates forming one lower and one upper bound, one of those range shapes
/// combined with one or more equality predicates on distinct properties, a same-property pure
/// equality disjunction, a cross-property pure equality disjunction where property names may
/// repeat or differ, or a same-property pure numeric range disjunction. The router enforces the
/// provider-specific eight-arm equality/range limit and distinct-property rules for conjunctions;
/// the planner is provider-neutral.
#[derive(Debug)]
enum SearchFilter {
    Equality(Vec<SearchFilterArm>),
    Range(Vec<SearchFilterRange>),
    Mixed(Vec<SearchFilterArm>, Vec<SearchFilterRange>),
    /// ADR 0034 Slices 15 and 16: pure equality disjunction on the same binding. Each arm owns a
    /// property name and value; property names may repeat or differ. The planner accepts any
    /// number of OR-connected equality arms; the router enforces the execution bound and
    /// deduplicates identical `(property_id, encoded_value)` sources before lookup.
    EqualityDisjunction(Vec<SearchFilterArm>),
    /// ADR 0034 Slice 17: pure same-property numeric range disjunction on the searched binding.
    /// Every arm compares the same property to a literal or parameter using a range operator. The
    /// planner accepts any number of OR-connected range arms; the router resolves each arm to a
    /// finite half-open encoded interval, drops empty intervals, merges overlapping/touching
    /// intervals, and enforces the execution bound before lookup.
    RangeDisjunction(Vec<SearchFilterRange>),
}

/// Maximum number of `AND`-connected equality arms the Property Index intersection path admits.
const MAX_EQUALITY_INTERSECTION_ARMS: usize = 8;

/// Maximum number of `OR`-connected equality arms the Router will execute as a bounded union of
/// `lookup_equal_page` streams. This is a Router-owned fan-out bound, independent from the Property
/// Index intersection limit.
const MAX_EQUALITY_DISJUNCTION_ARMS: usize = 8;

/// Maximum number of `OR`-connected range arms the Router will execute as a bounded union of
/// `lookup_range_page` streams. This is a Router-owned fan-out bound, independent from the equality
/// disjunction limit. Interval merging may reduce the number of distinct range sources actually
/// walked, but the syntactic arm count is bounded here before normalization.
const MAX_RANGE_DISJUNCTION_ARMS: usize = 8;

#[derive(Debug)]
struct SearchShape {
    binding: String,
    index_name: Vec<Str>,
    query_expr: gleaph_gql::ast::Expr,
    limit_expr: gleaph_gql::ast::Expr,
    output_alias: String,
    output_kind: SearchOutputKind,
    required_label_ids: Vec<VertexLabelId>,
    /// Label used for property-index candidate filtering. For a leading filtered search this is
    /// the leading labeled `NodeScan`; for a non-leading filtered search it is derived from the
    /// top-level prefix operators that statically prove exactly one positive label for the
    /// searched binding.
    filter_label_id: Option<VertexLabelId>,
    filter: Option<SearchFilter>,
}

#[derive(Debug)]
enum SearchPosition {
    Leading(SearchShape),
    NonLeading(SearchShape),
}

fn analyze_search_shape(
    plan: &PhysicalPlan,
    graph_id: GraphId,
    store: &RouterStore,
) -> Result<SearchPosition, RouterError> {
    // Classify the single supported SEARCH shape. Leading (Slice 3) is a NodeScan + Search
    // prefix; non-leading (Slice 5) is a single top-level Search with both preceding and
    // following operators. Every other shape is rejected fail-closed.
    if let [first, second, tail @ ..] = plan.ops.as_slice()
        && let (
            PlanOp::NodeScan {
                variable,
                label,
                property_projection: _,
            },
            PlanOp::Search {
                binding,
                provider,
                output,
            },
        ) = (first, second)
        && variable == binding
    {
        if tail.iter().any(op_contains_search) {
            return Err(RouterError::InvalidArgument(
                "GQL SEARCH must appear exactly once as the leading prefix in this slice".into(),
            ));
        }
        if tail.is_empty() {
            return Err(RouterError::InvalidArgument(
                "GQL SEARCH plan has no tail after the vector-search prefix".into(),
            ));
        }
        let filter_label_id = label
            .as_ref()
            .map(|label| resolve_vertex_label_id(graph_id, store, label))
            .transpose()?;
        let shape = extract_shape_from_search_op(
            graph_id,
            store,
            binding.as_ref(),
            provider,
            output,
            label.as_ref(),
            filter_label_id,
        )?;
        if shape.filter.is_some() && shape.filter_label_id.is_none() {
            return Err(RouterError::InvalidArgument(
                "SEARCH ... WHERE requires a labeled leading search in this slice".into(),
            ));
        }
        return Ok(SearchPosition::Leading(shape));
    }

    let top_level_search_count = plan
        .ops
        .iter()
        .filter(|op| matches!(op, PlanOp::Search { .. }))
        .count();
    let has_nested_search = plan
        .ops
        .iter()
        .filter(|op| !matches!(op, PlanOp::Search { .. }))
        .any(op_contains_search);
    if top_level_search_count != 1 || has_nested_search {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH must appear exactly once at the top level and not be nested or repeated in this slice"
                .into(),
        ));
    }

    let search_idx = plan
        .ops
        .iter()
        .position(|op| matches!(op, PlanOp::Search { .. }))
        .expect("exactly one top-level Search");
    if search_idx == 0 || search_idx == plan.ops.len() - 1 {
        return Err(RouterError::InvalidArgument(
            "GQL non-leading SEARCH must have both preceding and following operators".into(),
        ));
    }

    let PlanOp::Search {
        binding,
        provider,
        output,
    } = &plan.ops[search_idx]
    else {
        unreachable!("position returned a Search op");
    };

    let filter_label_id = if provider.filter().is_some() {
        Some(prove_searched_label(
            plan,
            graph_id,
            store,
            binding.as_ref(),
            search_idx,
        )?)
    } else {
        None
    };

    let shape = extract_shape_from_search_op(
        graph_id,
        store,
        binding.as_ref(),
        provider,
        output,
        None,
        filter_label_id,
    )?;
    Ok(SearchPosition::NonLeading(shape))
}

/// Inspect the top-level prefix `plan.ops[..search_idx]` and prove exactly one positive simple
/// label for `binding`. Accepts a labeled `NodeScan` for the binding or a `PropertyFilter` whose
/// predicates contain `IsLabeled(binding, label, negated: false)`. Rejects zero labels, multiple
/// distinct labels, negated labels, dynamic/nested label expressions, or any prefix operator that
/// rebinds the searched variable after the accepted proof.
fn prove_searched_label(
    plan: &PhysicalPlan,
    graph_id: GraphId,
    store: &RouterStore,
    binding: &str,
    search_idx: usize,
) -> Result<VertexLabelId, RouterError> {
    let mut proven_label: Option<VertexLabelId> = None;
    let mut proof_idx: Option<usize> = None;

    for (idx, op) in plan.ops[..search_idx].iter().enumerate() {
        match op {
            PlanOp::NodeScan {
                variable,
                label: Some(label),
                ..
            } if variable.as_ref() == binding => {
                let label_id = resolve_vertex_label_id(graph_id, store, label)?;
                match proven_label {
                    Some(existing) if existing != label_id => {
                        return Err(RouterError::InvalidArgument(
                            "SEARCH ... WHERE prefix proves multiple distinct labels for the searched binding".into(),
                        ));
                    }
                    Some(_) => {}
                    None => {
                        proven_label = Some(label_id);
                        proof_idx = Some(idx);
                    }
                }
            }
            PlanOp::PropertyFilter { predicates, .. }
            | PlanOp::ExpandFilter {
                dst_filter: predicates,
                ..
            } => {
                // PropertyFilter and ExpandFilter both carry conjunctive predicates on bound
                // vertices; either can provide a positive simple label proof for the searched
                // binding.
                for predicate in predicates {
                    match &predicate.kind {
                        ExprKind::IsLabeled {
                            expr,
                            label,
                            negated: false,
                        } if matches!(&expr.kind, ExprKind::Variable(name) if name == binding) => {
                            let label_name = require_simple_label_name(label)?;
                            let label_id = resolve_vertex_label_id(
                                graph_id,
                                store,
                                &NodeLabelRef::new(label_name),
                            )?;
                            match proven_label {
                                Some(existing) if existing != label_id => {
                                    return Err(RouterError::InvalidArgument(
                                        "SEARCH ... WHERE prefix proves multiple distinct labels for the searched binding".into(),
                                    ));
                                }
                                Some(_) => {}
                                None => {
                                    proven_label = Some(label_id);
                                    proof_idx = Some(idx);
                                }
                            }
                        }
                        ExprKind::IsLabeled {
                            expr,
                            negated: true,
                            ..
                        } if matches!(&expr.kind, ExprKind::Variable(name) if name == binding) => {
                            return Err(RouterError::InvalidArgument(
                                "SEARCH ... WHERE label proof must not be negated".into(),
                            ));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    // Fail closed if a later prefix operator rebinds the searched variable after the accepted proof.
    if let Some(proof) = proof_idx {
        for op in &plan.ops[proof + 1..search_idx] {
            if op_writes_variable(op, binding) {
                return Err(RouterError::InvalidArgument(
                    "SEARCH ... WHERE label proof is invalidated by a later prefix operator rebinding the searched binding".into(),
                ));
            }
        }
    }

    match proven_label {
        Some(label_id) => Ok(label_id),
        None => Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE requires a statically proved label for the searched binding in this slice".into(),
        )),
    }
}

/// Return true if `op` writes `variable` at the top level or inside a nested subplan that the
/// router search proof analyzer inspects. Used only for fail-closed rebind detection.
fn op_writes_variable(op: &PlanOp, variable: &str) -> bool {
    fn subplan_writes_variable(ops: &[PlanOp], variable: &str) -> bool {
        ops.iter().any(|op| op_writes_variable(op, variable))
    }

    match op {
        PlanOp::NodeScan { variable: v, .. }
        | PlanOp::IndexScan { variable: v, .. }
        | PlanOp::EdgeIndexScan { variable: v, .. } => v.as_ref() == variable,
        PlanOp::ConditionalIndexScan {
            fallback_variable: v,
            ..
        } => v.as_ref() == variable,
        PlanOp::Expand { edge, dst, .. } | PlanOp::ExpandFilter { edge, dst, .. } => {
            edge.as_ref() == variable || dst.as_ref() == variable
        }
        PlanOp::EdgeBindEndpoints {
            edge, near, far, ..
        } => edge.as_ref() == variable || near.as_ref() == variable || far.as_ref() == variable,
        PlanOp::Let { bindings } => bindings.iter().any(|b| b.variable == variable),
        PlanOp::OptionalMatch { sub_plan }
        | PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => subplan_writes_variable(sub_plan, variable),
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            subplan_writes_variable(&sub_plan.ops, variable)
        }
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right, .. } => {
            subplan_writes_variable(left, variable) || subplan_writes_variable(right, variable)
        }
        PlanOp::SetOperation { right, .. } => subplan_writes_variable(&right.ops, variable),
        _ => false,
    }
}

/// A positive label proof must name exactly one simple label. Wildcard, conjunction, disjunction,
/// and negation are rejected because the Router needs a single router-issued `VertexLabelId` to
/// scope the Property Index candidate lookup.
fn require_simple_label_name(expr: &gleaph_gql::types::LabelExpr) -> Result<&str, RouterError> {
    match expr {
        gleaph_gql::types::LabelExpr::Name(name) => Ok(name.as_str()),
        _ => Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE label proof must be a simple positive label name".into(),
        )),
    }
}

fn extract_shape_from_search_op(
    graph_id: GraphId,
    store: &RouterStore,
    binding: &str,
    provider: &SearchProviderPlan,
    output: &SearchOutputPlan,
    node_scan_label: Option<&NodeLabelRef>,
    filter_label_id: Option<VertexLabelId>,
) -> Result<SearchShape, RouterError> {
    let SearchProviderPlan::VectorIndex {
        index_name,
        query,
        limit,
        filter,
    } = provider;

    let required_label_ids = node_scan_label
        .map(|label| resolve_vertex_label_id(graph_id, store, label))
        .transpose()?
        .into_iter()
        .collect();

    let filter = filter
        .as_ref()
        .map(|expr| extract_search_filter(binding, expr))
        .transpose()?;

    Ok(SearchShape {
        binding: binding.to_string(),
        index_name: index_name.clone(),
        query_expr: query.clone(),
        limit_expr: limit.clone(),
        output_alias: output.alias.to_string(),
        output_kind: output.kind,
        required_label_ids,
        filter_label_id,
        filter,
    })
}

/// Extract an accepted SEARCH ... WHERE predicate from the planner-validated filter
/// expression. The planner already guaranteed exactly one side is `binding.property` and the
/// other is a literal or parameter, so this function only normalizes which side is which and
/// distinguishes equality arms from one or two range arms.
fn extract_search_filter(
    binding: &str,
    expr: &gleaph_gql::ast::Expr,
) -> Result<SearchFilter, RouterError> {
    if let Some(disjunction) = try_extract_equality_disjunction(binding, expr) {
        if disjunction.len() > MAX_EQUALITY_DISJUNCTION_ARMS {
            return Err(RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE equality disjunction supports at most {MAX_EQUALITY_DISJUNCTION_ARMS} OR-connected arms in this slice"
            )));
        }
        return Ok(SearchFilter::EqualityDisjunction(disjunction));
    }

    // First check whether the top-level expression is a range disjunction. If it is not, we
    // fall through to the AND/conjunction path below. The disjunction extractor requires every
    // OR leaf to be a range predicate on the same binding property.
    if let Some(disjunction) = try_extract_range_disjunction(binding, expr) {
        if disjunction.len() > MAX_RANGE_DISJUNCTION_ARMS {
            return Err(RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE range disjunction supports at most {MAX_RANGE_DISJUNCTION_ARMS} OR-connected arms in this slice"
            )));
        }
        return Ok(SearchFilter::RangeDisjunction(disjunction));
    }

    let leaves = collect_and_leaves(expr);
    if leaves.is_empty() {
        return Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE requires at least one predicate".into(),
        ));
    }

    // Detect OR-connected mixed shapes before treating the expression as a conjunction. The
    // equality and range disjunction extractors already rejected anything that was not a pure
    // disjunction of their respective predicate families, so a top-level OR that reaches this
    // point is a mixed or otherwise unsupported disjunction and should report a clear error.
    if matches!(&expr.kind, gleaph_gql::ast::ExprKind::Or(..)) {
        return Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE OR-connected arms must all be equality comparisons or all be range comparisons on the same property in this slice".into(),
        ));
    }

    let mut predicates = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        predicates.push(split_search_predicate(binding, leaf)?);
    }

    let equality_count = predicates
        .iter()
        .filter(|p| matches!(p, SearchPredicate::Equality(..)))
        .count();
    let range_count = predicates
        .iter()
        .filter(|p| matches!(p, SearchPredicate::Range(..)))
        .count();

    if equality_count == predicates.len() {
        let arms: Vec<SearchFilterArm> = predicates
            .into_iter()
            .map(|p| match p {
                SearchPredicate::Equality(property_name, value_expr) => SearchFilterArm {
                    property_name,
                    value_expr,
                },
                _ => unreachable!(),
            })
            .collect();
        validate_pure_equality_arms(&arms)?;
        Ok(SearchFilter::Equality(arms))
    } else if range_count == predicates.len() {
        let ranges: Vec<SearchFilterRange> = predicates
            .into_iter()
            .map(|p| match p {
                SearchPredicate::Range(property_name, op, value_expr) => SearchFilterRange {
                    property_name,
                    op,
                    value_expr,
                },
                _ => unreachable!(),
            })
            .collect();
        validate_search_filter_ranges(&ranges)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        Ok(SearchFilter::Range(ranges))
    } else if equality_count >= 1
        && (1..=2).contains(&range_count)
        && predicates.len() == equality_count + range_count
    {
        // Slice 14: N-way equality (N >= 1) plus one one-sided range or one two-sided range
        // on a different property of the searched binding.
        let mut arms: Vec<SearchFilterArm> = Vec::with_capacity(equality_count);
        let mut ranges: Vec<SearchFilterRange> = Vec::with_capacity(range_count);
        for p in predicates {
            match p {
                SearchPredicate::Equality(property_name, value_expr) => {
                    arms.push(SearchFilterArm {
                        property_name,
                        value_expr,
                    });
                }
                SearchPredicate::Range(property_name, op, value_expr) => {
                    ranges.push(SearchFilterRange {
                        property_name,
                        op,
                        value_expr,
                    });
                }
            }
        }
        validate_pure_equality_arms(&arms)?;
        validate_search_filter_ranges(&ranges)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        let range_property = ranges[0].property_name.clone();
        if arms.iter().any(|arm| arm.property_name == range_property) {
            return Err(RouterError::InvalidArgument(
                "SEARCH ... WHERE mixed equality/range arms must refer to distinct properties"
                    .into(),
            ));
        }
        Ok(SearchFilter::Mixed(arms, ranges))
    } else {
        Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE does not support this equality/range mixture in this slice".into(),
        ))
    }
}

fn validate_pure_equality_arms(arms: &[SearchFilterArm]) -> Result<(), RouterError> {
    let mut seen = std::collections::HashSet::new();
    for arm in arms {
        if !seen.insert(arm.property_name.clone()) {
            return Err(RouterError::InvalidArgument(
                "SEARCH ... WHERE equality conjuncts must refer to distinct properties".into(),
            ));
        }
    }
    if arms.len() > MAX_EQUALITY_INTERSECTION_ARMS {
        return Err(RouterError::InvalidArgument(format!(
            "SEARCH ... WHERE supports at most {MAX_EQUALITY_INTERSECTION_ARMS} equality conjuncts in this slice"
        )));
    }
    Ok(())
}

fn validate_search_filter_ranges(ranges: &[SearchFilterRange]) -> Result<(), String> {
    fn is_lower(op: gleaph_gql::ast::CmpOp) -> bool {
        matches!(op, gleaph_gql::ast::CmpOp::Gt | gleaph_gql::ast::CmpOp::Ge)
    }
    fn is_upper(op: gleaph_gql::ast::CmpOp) -> bool {
        matches!(op, gleaph_gql::ast::CmpOp::Lt | gleaph_gql::ast::CmpOp::Le)
    }

    if ranges.len() == 1 {
        if !is_lower(ranges[0].op) && !is_upper(ranges[0].op) {
            return Err("SEARCH ... WHERE range must be <, <=, >, or >=".into());
        }
        return Ok(());
    }
    if ranges.len() != 2 {
        return Err("SEARCH ... WHERE supports at most two range predicates in this slice".into());
    }
    if ranges[0].property_name != ranges[1].property_name {
        return Err(
            "SEARCH ... WHERE two-sided range requires both predicates to refer to the same property"
                .into(),
        );
    }
    let first_lower = is_lower(ranges[0].op);
    let second_lower = is_lower(ranges[1].op);
    if first_lower == second_lower {
        return Err(
            "SEARCH ... WHERE two-sided range requires one lower bound (> or >=) and one upper bound (< or <=)"
                .into(),
        );
    }
    Ok(())
}

enum SearchPredicate {
    Equality(String, gleaph_gql::ast::Expr),
    Range(String, gleaph_gql::ast::CmpOp, gleaph_gql::ast::Expr),
}

fn split_search_predicate(
    binding: &str,
    expr: &gleaph_gql::ast::Expr,
) -> Result<SearchPredicate, RouterError> {
    fn is_bound_property(expr: &gleaph_gql::ast::Expr, binding: &str) -> Option<String> {
        match &expr.kind {
            gleaph_gql::ast::ExprKind::PropertyAccess {
                expr: base,
                property,
            } => {
                if matches!(
                    &base.kind,
                    gleaph_gql::ast::ExprKind::Variable(name) if name == binding
                ) {
                    Some(property.clone())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn is_literal_or_parameter(expr: &gleaph_gql::ast::Expr) -> bool {
        matches!(
            &expr.kind,
            gleaph_gql::ast::ExprKind::Literal(_) | gleaph_gql::ast::ExprKind::Parameter(_)
        )
    }

    let gleaph_gql::ast::ExprKind::Compare { left, op, right } = &expr.kind else {
        return Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE must be a comparison predicate".into(),
        ));
    };

    match op {
        gleaph_gql::ast::CmpOp::Eq => {
            if let Some(property) = is_bound_property(left, binding)
                && is_literal_or_parameter(right)
            {
                return Ok(SearchPredicate::Equality(property, *right.clone()));
            }
            if let Some(property) = is_bound_property(right, binding)
                && is_literal_or_parameter(left)
            {
                return Ok(SearchPredicate::Equality(property, *left.clone()));
            }
        }
        gleaph_gql::ast::CmpOp::Lt
        | gleaph_gql::ast::CmpOp::Le
        | gleaph_gql::ast::CmpOp::Gt
        | gleaph_gql::ast::CmpOp::Ge => {
            if let Some(property) = is_bound_property(left, binding)
                && is_literal_or_parameter(right)
            {
                return Ok(SearchPredicate::Range(property, *op, *right.clone()));
            }
            if let Some(property) = is_bound_property(right, binding)
                && is_literal_or_parameter(left)
            {
                // Normalize reversed operand order by inverting the operator.
                let normalized_op = match op {
                    gleaph_gql::ast::CmpOp::Lt => gleaph_gql::ast::CmpOp::Gt,
                    gleaph_gql::ast::CmpOp::Le => gleaph_gql::ast::CmpOp::Ge,
                    gleaph_gql::ast::CmpOp::Gt => gleaph_gql::ast::CmpOp::Lt,
                    gleaph_gql::ast::CmpOp::Ge => gleaph_gql::ast::CmpOp::Le,
                    _ => unreachable!(),
                };
                return Ok(SearchPredicate::Range(
                    property,
                    normalized_op,
                    *left.clone(),
                ));
            }
        }
        _ => {}
    }

    Err(RouterError::InvalidArgument(
        "SEARCH ... WHERE must compare a property of the searched binding with a literal or parameter".into(),
    ))
}

fn collect_and_leaves(expr: &gleaph_gql::ast::Expr) -> Vec<gleaph_gql::ast::Expr> {
    fn walk(expr: &gleaph_gql::ast::Expr, out: &mut Vec<gleaph_gql::ast::Expr>) {
        match &expr.kind {
            gleaph_gql::ast::ExprKind::And(left, right) => {
                walk(left, out);
                walk(right, out);
            }
            _ => out.push(expr.clone()),
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

fn collect_or_leaves(expr: &gleaph_gql::ast::Expr) -> Vec<gleaph_gql::ast::Expr> {
    fn walk(expr: &gleaph_gql::ast::Expr, out: &mut Vec<gleaph_gql::ast::Expr>) {
        match &expr.kind {
            gleaph_gql::ast::ExprKind::Or(left, right) => {
                walk(left, out);
                walk(right, out);
            }
            _ => out.push(expr.clone()),
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

/// Try to extract a pure equality disjunction: an OR-connected chain where every leaf is an
/// equality comparison between `binding.property` and a literal or parameter, and every leaf refers
/// to the same property. Returns the normalized arms when the shape matches; otherwise returns
/// `None` so the caller can fall back to the conjunction path. A single equality leaf is not
/// classified as a disjunction here.
fn try_extract_equality_disjunction(
    binding: &str,
    expr: &gleaph_gql::ast::Expr,
) -> Option<Vec<SearchFilterArm>> {
    let leaves = collect_or_leaves(expr);
    if leaves.len() < 2 {
        return None;
    }
    let mut arms = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        match split_search_predicate(binding, leaf) {
            Ok(SearchPredicate::Equality(property_name, value_expr)) => {
                arms.push(SearchFilterArm {
                    property_name,
                    value_expr,
                });
            }
            _ => return None,
        }
    }
    Some(arms)
}

/// Try to extract a pure same-property numeric range disjunction: an OR-connected chain where
/// every leaf is a range comparison (`<`, `<=`, `>`, `>=`) between `binding.property` and a
/// literal or parameter, and every leaf refers to the same property. Returns the normalized
/// ranges when the shape matches; otherwise returns `None` so the caller can fall back to the
/// conjunction path. A single range leaf is handled by the conjunction path.
fn try_extract_range_disjunction(
    binding: &str,
    expr: &gleaph_gql::ast::Expr,
) -> Option<Vec<SearchFilterRange>> {
    let leaves = collect_or_leaves(expr);
    if leaves.len() < 2 {
        return None;
    }
    let mut ranges = Vec::with_capacity(leaves.len());
    let mut property_name: Option<String> = None;
    for leaf in &leaves {
        match split_search_predicate(binding, leaf) {
            Ok(SearchPredicate::Range(prop, op, value_expr)) => {
                if let Some(ref existing) = property_name {
                    if existing != &prop {
                        return None;
                    }
                } else {
                    property_name = Some(prop.clone());
                }
                ranges.push(SearchFilterRange {
                    property_name: prop,
                    op,
                    value_expr,
                });
            }
            _ => return None,
        }
    }
    Some(ranges)
}

fn op_contains_search(op: &PlanOp) -> bool {
    match op {
        PlanOp::Search { .. } => true,
        PlanOp::OptionalMatch { sub_plan }
        | PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => sub_plan.iter().any(op_contains_search),
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            left.iter().any(op_contains_search) || right.iter().any(op_contains_search)
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => sub_plan.ops.iter().any(op_contains_search),
        PlanOp::SetOperation { right, .. } => right.ops.iter().any(op_contains_search),
        _ => false,
    }
}

fn resolve_vertex_label_id(
    graph_id: GraphId,
    store: &RouterStore,
    label: &NodeLabelRef,
) -> Result<VertexLabelId, RouterError> {
    store
        .lookup_vertex_label_id(graph_id, label.as_ref())
        .map_err(|e| RouterError::InvalidArgument(format!("SEARCH label {}: {e}", label.as_ref())))
}

fn resolve_vector_index(
    graph_id: GraphId,
    index_name_parts: &[Str],
) -> Result<(u32, vector_index_catalog::VectorIndexDefRecord), RouterError> {
    let name = index_name_parts.join(".");
    let embedding_name_id = embedding_name_catalog::lookup_embedding_name_id(graph_id, &name)
        .ok_or_else(|| RouterError::NotFound(format!("vector index/embedding name {name}")))?;
    let def = vector_index_catalog::list_vector_indexes(graph_id)
        .into_iter()
        .find(|d| d.embedding_name_id == embedding_name_id)
        .ok_or_else(|| RouterError::NotFound(format!("vector index for embedding name {name}")))?;
    Ok((def.index_id, def))
}

fn resolve_query_bytes(
    expr: &gleaph_gql::ast::Expr,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<u8>, RouterError> {
    let value = match &expr.kind {
        ExprKind::Literal(v) => v.clone(),
        ExprKind::Parameter(name) => {
            let key = name.strip_prefix('$').unwrap_or(name.as_str());
            params
                .get(key)
                .ok_or_else(|| RouterError::InvalidArgument(format!("missing parameter ${name}")))?
                .clone()
        }
        _ => {
            return Err(RouterError::InvalidArgument(
                "SEARCH FOR must be a bytes literal or parameter".into(),
            ));
        }
    };
    match value {
        Value::Bytes(b) => Ok(b),
        _ => Err(RouterError::InvalidArgument(
            "SEARCH FOR must evaluate to bytes".into(),
        )),
    }
}

fn resolve_limit_u32(
    expr: &gleaph_gql::ast::Expr,
    params: &BTreeMap<String, Value>,
) -> Result<u32, RouterError> {
    let value = match &expr.kind {
        ExprKind::Literal(v) => v.clone(),
        ExprKind::Parameter(name) => {
            let key = name.strip_prefix('$').unwrap_or(name.as_str());
            params
                .get(key)
                .ok_or_else(|| RouterError::InvalidArgument(format!("missing parameter ${name}")))?
                .clone()
        }
        _ => {
            return Err(RouterError::InvalidArgument(
                "SEARCH LIMIT must be an integer literal or parameter".into(),
            ));
        }
    };
    let n: u64 = match value {
        Value::Int8(v) if v > 0 => v as u64,
        Value::Int16(v) if v > 0 => v as u64,
        Value::Int32(v) if v > 0 => v as u64,
        Value::Int64(v) if v > 0 => v as u64,
        Value::Uint8(v) if v > 0 => v as u64,
        Value::Uint16(v) if v > 0 => v as u64,
        Value::Uint32(v) if v > 0 => v as u64,
        Value::Uint64(v) if v > 0 => v,
        _ => {
            return Err(RouterError::InvalidArgument(
                "SEARCH LIMIT must be a positive integer".into(),
            ));
        }
    };
    if n > u32::MAX as u64 {
        return Err(RouterError::InvalidArgument(
            "SEARCH LIMIT exceeds u32::MAX".into(),
        ));
    }
    Ok(n as u32)
}

fn build_search_seeds(
    binding: &str,
    alias: &str,
    required_label_ids: &[VertexLabelId],
    hits: &[VectorSearchHit],
    metric: VectorMetric,
) -> Result<BTreeMap<ShardId, SeedBindingsWire>, RouterError> {
    let mut by_shard: BTreeMap<ShardId, Vec<SeedRowWire>> = BTreeMap::new();
    let mut seen: std::collections::HashSet<(ShardId, u32)> = std::collections::HashSet::new();
    for hit in hits {
        let VectorSubject::Vertex {
            shard_id,
            vertex_id,
        } = hit.subject;
        // ADR 0034: a single vector search must not return the same subject twice.
        // Fail closed so both the leading seed path and the non-leading resolved path share
        // the same defense contract.
        if !seen.insert((shard_id, vertex_id)) {
            return Err(RouterError::InvalidArgument(format!(
                "duplicate vector search hit for shard {shard_id} vertex {vertex_id}"
            )));
        }
        let value = match metric.output_shape() {
            VectorOutputShape::Distance => {
                metric.to_user_distance(hit.distance).ok_or_else(|| {
                    RouterError::InvalidArgument(
                        "SEARCH distance conversion produced a non-finite value".into(),
                    )
                })?
            }
            VectorOutputShape::Score => metric.to_user_score(hit.distance).ok_or_else(|| {
                RouterError::InvalidArgument(
                    "SEARCH score conversion produced a non-finite value".into(),
                )
            })?,
        };
        let row = SeedRowWire {
            vertex_bindings: vec![SeedVertexBinding {
                variable: binding.to_string(),
                local_vertex_id: vertex_id,
                required_vertex_label_ids: required_label_ids.iter().map(|l| l.raw()).collect(),
            }],
            float64_bindings: vec![SeedFloat64Binding {
                variable: alias.to_string(),
                value: f64::from(value),
            }],
        };
        by_shard.entry(shard_id).or_default().push(row);
    }
    Ok(by_shard
        .into_iter()
        .map(|(shard_id, rows)| {
            let wire = SeedBindingsWire {
                entries: Vec::new(),
                rows,
            };
            (shard_id, wire)
        })
        .collect())
}

fn build_resolved_search_wires(
    binding: &str,
    alias: &str,
    hits: &[VectorSearchHit],
    metric: VectorMetric,
    graph_id: GraphId,
    store: &RouterStore,
) -> Result<BTreeMap<ShardId, ResolvedSearchWire>, RouterError> {
    let live_shards: std::collections::HashSet<ShardId> = store
        .list_live_shards_for_graph_id(graph_id)?
        .into_iter()
        .map(|entry| entry.shard_id)
        .collect();

    let mut by_shard: BTreeMap<ShardId, Vec<ResolvedSearchVertexHitWire>> = BTreeMap::new();
    let mut seen: std::collections::HashSet<(ShardId, u32)> = std::collections::HashSet::new();
    for hit in hits {
        let VectorSubject::Vertex {
            shard_id,
            vertex_id,
        } = hit.subject;
        // ADR 0034 Slice 5: derived-index staleness contract matches the leading path — hits that
        // reference a shard no longer in the live topology are ignored rather than failing the
        // query. The remaining live-shard hits still form the global top-k relation.
        if !live_shards.contains(&shard_id) {
            continue;
        }
        if !seen.insert((shard_id, vertex_id)) {
            return Err(RouterError::InvalidArgument(format!(
                "duplicate vector search hit for shard {shard_id} vertex {vertex_id}"
            )));
        }
        let value = match metric.output_shape() {
            VectorOutputShape::Distance => {
                metric.to_user_distance(hit.distance).ok_or_else(|| {
                    RouterError::InvalidArgument(
                        "SEARCH distance conversion produced a non-finite value".into(),
                    )
                })?
            }
            VectorOutputShape::Score => metric.to_user_score(hit.distance).ok_or_else(|| {
                RouterError::InvalidArgument(
                    "SEARCH score conversion produced a non-finite value".into(),
                )
            })?,
        };
        by_shard
            .entry(shard_id)
            .or_default()
            .push(ResolvedSearchVertexHitWire {
                local_vertex_id: vertex_id,
                value: f64::from(value),
            });
    }

    // Include every live shard, even those with no hits, so each dispatched shard receives an
    // explicit relation (possibly empty) rather than an absent field that would look like a
    // protocol violation to the graph executor.
    let mut out = BTreeMap::new();
    for shard_id in live_shards {
        let vertex_hits = by_shard.remove(&shard_id).unwrap_or_default();
        out.insert(
            shard_id,
            ResolvedSearchWire {
                binding: binding.to_string(),
                output_alias: alias.to_string(),
                vertex_hits,
            },
        );
    }
    Ok(out)
}

fn strip_search_prefix(
    plan: &PhysicalPlan,
    _shape: &SearchShape,
) -> Result<PhysicalPlan, RouterError> {
    let tail = plan.ops[2..].to_vec();
    if tail.is_empty() {
        return Err(RouterError::InvalidArgument(
            "GQL SEARCH plan has no tail after the vector-search prefix".into(),
        ));
    }
    Ok(PhysicalPlan {
        ops: tail,
        diagnostics: plan.diagnostics.clone(),
        annotations: plan.annotations.clone(),
        output: plan.output.clone(),
        binding_layout: plan.binding_layout.clone(),
    })
}

/// Dispatch a filtered SEARCH whose candidate set is empty without calling the vector canister.
/// For a leading search this strips the prefix and sends empty seeds; for a non-leading search
/// it sends an explicit empty resolved-search relation to every live shard. Both paths preserve
/// the global aggregate contract of returning one zero row for `count(*)` over an empty relation.
async fn dispatch_empty_filtered_search(
    plan: &PhysicalPlan,
    graph_id: GraphId,
    params_blob: &[u8],
    params: &BTreeMap<String, Value>,
    mode: GqlExecutionMode,
    stats: &RouterGraphStats,
    store: &RouterStore,
    caller: candid::Principal,
    position: &SearchPosition,
    shape: &SearchShape,
    metric: VectorMetric,
) -> Result<Option<GqlQueryResult>, RouterError> {
    let empty_hits: &[VectorSearchHit] = &[];
    match position {
        SearchPosition::Leading(_) => {
            let stripped_plan = strip_search_prefix(plan, shape)?;
            let stripped_plan_blob = gleaph_gql_planner::wire::encode_block_plans(
                std::slice::from_ref(&stripped_plan),
                false,
            )
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
            dispatch_search_read_plan(
                graph_id,
                plan,
                &stripped_plan_blob,
                &stripped_plan,
                build_search_seeds(
                    &shape.binding,
                    &shape.output_alias,
                    &shape.required_label_ids,
                    empty_hits,
                    metric,
                )?,
                params_blob,
                mode,
                stats,
                store,
            )
            .await
            .map(Some)
        }
        SearchPosition::NonLeading(_) => {
            let resolved_search_by_shard = build_resolved_search_wires(
                &shape.binding,
                &shape.output_alias,
                empty_hits,
                metric,
                graph_id,
                store,
            )?;
            let plan_blob =
                gleaph_gql_planner::wire::encode_block_plans(std::slice::from_ref(plan), false)
                    .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
            crate::gql::dispatch_plan_blob_with_search(
                graph_id,
                &plan_blob,
                std::slice::from_ref(plan),
                params,
                params_blob,
                mode,
                None,
                stats,
                Some(resolved_search_by_shard),
                caller,
            )
            .await
            .map(Some)
        }
    }
}

async fn dispatch_search_read_plan(
    graph_id: GraphId,
    _original_plan: &PhysicalPlan,
    stripped_plan_blob: &[u8],
    stripped_plan: &PhysicalPlan,
    seeds_by_shard: BTreeMap<ShardId, SeedBindingsWire>,
    params_blob: &[u8],
    mode: GqlExecutionMode,
    _stats: &RouterGraphStats,
    store: &RouterStore,
) -> Result<GqlQueryResult, RouterError> {
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    if shards.is_empty() {
        return Err(RouterError::ShardNotRegistered);
    }

    let element_id_encoding_key = store.graph_element_id_encoding_key(graph_id)?.0;
    let resolved_labels =
        store.resolve_plan_labels(graph_id, std::slice::from_ref(stripped_plan))?;
    let resolved_properties =
        store.resolve_plan_properties(graph_id, std::slice::from_ref(stripped_plan))?;
    let indexed_properties =
        crate::index_catalog::graph_stats_for(graph_id).to_indexed_property_catalog();
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    let indexed_embeddings =
        vector_index_catalog::to_indexed_embedding_catalog(graph_id, dispatch_ready);

    let merge_mode = federated_merge_mode_from_plans(std::slice::from_ref(stripped_plan));

    let mut merged = empty_execute_plan_result();
    // ADR 0034: an empty live relation (no hits, or only hits for non-live shards) must still
    // dispatch the stripped tail plan so global aggregates produce one zero row. When live hits
    // exist, keep the historical behavior of dispatching only to shards that own a hit; this avoids
    // shard-count-proportional inter-canister overhead for the common case.
    let has_live_seed = shards
        .iter()
        .any(|shard| seeds_by_shard.contains_key(&shard.shard_id));
    let empty_seed_wire = SeedBindingsWire {
        entries: Vec::new(),
        rows: Vec::new(),
    };
    let mut dispatched_any = false;
    for shard in shards {
        let seed_wire = match seeds_by_shard.get(&shard.shard_id) {
            Some(wire) => wire,
            None if !has_live_seed => &empty_seed_wire,
            None => continue,
        };
        dispatched_any = true;
        let seed_blob = Encode!(seed_wire).expect("encode search seed bindings");
        let result = execute_plan_on_graph(
            shard.graph_canister,
            ExecutePlanArgs {
                target_shard_id: shard.shard_id,
                element_id_encoding_key,
                mutation_id: None,
                plan_blob: stripped_plan_blob.to_vec(),
                params_blob: params_blob.to_vec(),
                mode,
                seed_bindings_blob: Some(seed_blob),
                resolved_labels: Some(resolved_labels.clone()),
                resolved_properties: Some(resolved_properties.clone()),
                indexed_properties: Some(indexed_properties.clone()),
                unique_claims: None,
                constrained_properties: None,
                local_unique_claims: None,
                local_constrained_properties: None,
                indexed_embeddings: Some(indexed_embeddings.clone()),
                resolved_search_blob: None,
            },
        )
        .await
        .map_err(RouterError::InvalidArgument)?;
        merge_execute_plan_result(&mut merged, result, merge_mode.clone())
            .map_err(RouterError::InvalidArgument)?;
    }

    debug_assert!(
        dispatched_any,
        "shards non-empty guarantees at least one dispatch"
    );

    Ok(GqlQueryResult::from_merged(&merged))
}

// ════════════════════════════════════════════════════════════════════════════════
// ADR 0034 Slice 6: filtered SEARCH candidate resolution
// ════════════════════════════════════════════════════════════════════════════════

/// Resolve and collect the candidate set for a filtered SEARCH.
///
/// Steps:
/// 1. Resolve the property name to a property id and verify an active vertex property index for
///    the exact `(label_id, property_id)` tuple.
/// 2. Resolve the literal/parameter value and encode it with the shared property-index key encoder.
/// 3. Validate each encoded key size against `MAX_INDEX_VALUE_KEY_BYTES`.
/// 4. For equality filters, page through the equality bucket or server-side intersection. For a
///    numeric range filter, page through the finite encoded interval. In all cases stop at the
///    4097th distinct label-qualified `(shard_id, vertex_id)` subject and return an explicit error.
async fn resolve_filtered_candidates(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    binding: &str,
    filter: &SearchFilter,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<VectorSubject>, RouterError> {
    match filter {
        SearchFilter::Equality(arms) => {
            resolve_filtered_equality_candidates(graph_id, store, label_id, binding, arms, params)
                .await
        }
        SearchFilter::Range(ranges) => {
            resolve_filtered_range_candidates(graph_id, store, label_id, binding, ranges, params)
                .await
        }
        SearchFilter::Mixed(eq, ranges) => {
            resolve_filtered_mixed_candidates(
                graph_id, store, label_id, binding, eq, ranges, params,
            )
            .await
        }
        SearchFilter::EqualityDisjunction(arms) => {
            resolve_filtered_equality_disjunction_candidates(
                graph_id, store, label_id, binding, arms, params,
            )
            .await
        }
        SearchFilter::RangeDisjunction(ranges) => {
            resolve_filtered_range_disjunction_candidates(
                graph_id, store, label_id, binding, ranges, params,
            )
            .await
        }
    }
}

/// ADR 0034 Slice 14: resolve one to eight equality arms combined with one one- or two-sided
/// numeric range arm on a distinct property. Every equality property and the range property
/// require active vertex property indexes for the same proved label; equality values are encoded
/// with the shared property-index key encoder through `resolve_equality_arms` and the one or two
/// range arms are collapsed into a single finite half-open encoded interval by
/// `resolve_filtered_range_interval`. The Property Index walks the finite range and sieves each
/// page by every equality arm server-side.
async fn resolve_filtered_mixed_candidates(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    binding: &str,
    arms: &[SearchFilterArm],
    ranges: &[SearchFilterRange],
    params: &BTreeMap<String, Value>,
) -> Result<Vec<VectorSubject>, RouterError> {
    let equal_specs = resolve_equality_arms(graph_id, store, label_id, binding, arms, params)?;

    let (range_property_id, low, high) = match resolve_filtered_range_interval(
        graph_id, store, label_id, binding, ranges, params,
    )? {
        Some(interval) => interval,
        None => {
            return Ok(Vec::new());
        }
    };

    collect_bounded_candidates_range_intersection(
        graph_id,
        store,
        label_id,
        range_property_id,
        low,
        high,
        equal_specs,
    )
    .await
}

async fn collect_bounded_candidates_range_intersection(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    range_property_id: PropertyId,
    low: Vec<u8>,
    high: Vec<u8>,
    equal_specs: Vec<IndexEqualSpec>,
) -> Result<Vec<VectorSubject>, RouterError> {
    collect_bounded_candidates(graph_id, store, label_id, |client, after| {
        let equal_specs = equal_specs.clone();
        let req = LookupRangeIntersectionPageRequest {
            range_property_id: range_property_id.raw(),
            low: low.clone(),
            high: high.clone(),
            equal_specs,
            after,
            limit: VECTOR_FILTER_PAGE_LIMIT,
        };
        async move { client.lookup_range_intersection_page(req).await }
    })
    .await
}

async fn resolve_filtered_equality_candidates(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    binding: &str,
    arms: &[SearchFilterArm],
    params: &BTreeMap<String, Value>,
) -> Result<Vec<VectorSubject>, RouterError> {
    let equal_specs = resolve_equality_arms(graph_id, store, label_id, binding, arms, params)?;

    match equal_specs.len() {
        1 => {
            let spec = equal_specs.into_iter().next().unwrap();
            collect_bounded_candidates_equal(
                graph_id,
                store,
                label_id,
                PropertyId::from_raw(spec.property_id),
                spec.value,
            )
            .await
        }
        n if (2..=MAX_EQUALITY_INTERSECTION_ARMS).contains(&n) => {
            collect_bounded_candidates_intersection(graph_id, store, label_id, equal_specs).await
        }
        _ => Err(RouterError::InvalidArgument(format!(
            "SEARCH ... WHERE supports one to {MAX_EQUALITY_INTERSECTION_ARMS} equality conjuncts"
        ))),
    }
}

/// ADR 0034 Slices 15 and 16: resolve a pure equality disjunction on the same binding as a
/// bounded union of `lookup_equal_page` streams. Every arm may target any property of the searched
/// binding; each arm's property must have an active vertex property index for the proved label.
/// The Router enforces the execution arm limit (2..=MAX_EQUALITY_DISJUNCTION_ARMS) and the 4096
/// candidate bound.
async fn resolve_filtered_equality_disjunction_candidates(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    binding: &str,
    arms: &[SearchFilterArm],
    params: &BTreeMap<String, Value>,
) -> Result<Vec<VectorSubject>, RouterError> {
    if arms.is_empty() {
        return Ok(Vec::new());
    }
    if arms.len() > MAX_EQUALITY_DISJUNCTION_ARMS {
        return Err(RouterError::InvalidArgument(format!(
            "SEARCH ... WHERE equality disjunction supports at most {MAX_EQUALITY_DISJUNCTION_ARMS} OR-connected arms in this slice"
        )));
    }

    let property_name = arms[0].property_name.clone();
    let property_id = resolve_search_property_id(graph_id, store, binding, &property_name)?;
    if !indexed_catalog::has_active_vertex_property_index(graph_id, label_id, property_id) {
        return Err(RouterError::InvalidArgument(format!(
            "SEARCH ... WHERE requires an active vertex property index for label {} property {}",
            label_id.raw(),
            property_name
        )));
    }

    let mut sources = Vec::with_capacity(arms.len());
    for arm in arms {
        let property_id = resolve_search_property_id(graph_id, store, binding, &arm.property_name)?;
        if !indexed_catalog::has_active_vertex_property_index(graph_id, label_id, property_id) {
            return Err(RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE requires an active vertex property index for label {} property {}",
                label_id.raw(),
                arm.property_name
            )));
        }
        let value = resolve_filter_value(&arm.value_expr, params)?;
        let encoded = encode_filter_value(&value)?;
        if encoded.len() > MAX_INDEX_VALUE_KEY_BYTES {
            return Err(RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE value exceeds maximum index key size of {MAX_INDEX_VALUE_KEY_BYTES} bytes"
            )));
        }
        sources.push((property_id, encoded));
    }

    // Runtime-equal `(property_id, encoded_value)` pairs share the same posting stream. Dedupe them
    // after the syntactic admission bound has been enforced, preserving the order of first
    // occurrence.
    let mut distinct_sources: Vec<(PropertyId, Vec<u8>)> = Vec::with_capacity(sources.len());
    for (pid, v) in sources {
        if distinct_sources
            .iter()
            .any(|(p, val)| p.raw() == pid.raw() && val == &v)
        {
            continue;
        }
        distinct_sources.push((pid, v));
    }

    collect_bounded_candidates_equal_disjunction_with_store(
        graph_id,
        store,
        label_id,
        distinct_sources,
    )
    .await
}

/// ADR 0034 Slice 17: resolve a pure same-property numeric range disjunction. Each arm is
/// resolved to a finite half-open encoded interval; empty intervals are dropped and the rest are
/// merged into a minimal set of disjoint intervals. The resulting merged intervals are then
/// collected as a bounded union of `lookup_range_page` streams.
async fn resolve_filtered_range_disjunction_candidates(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    binding: &str,
    ranges: &[SearchFilterRange],
    params: &BTreeMap<String, Value>,
) -> Result<Vec<VectorSubject>, RouterError> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    if ranges.len() > MAX_RANGE_DISJUNCTION_ARMS {
        return Err(RouterError::InvalidArgument(format!(
            "SEARCH ... WHERE range disjunction supports at most {MAX_RANGE_DISJUNCTION_ARMS} OR-connected arms in this slice"
        )));
    }

    // All arms share the same property; resolve once and prove a single active index.
    let property_id =
        resolve_search_property_id(graph_id, store, binding, &ranges[0].property_name)?;
    if !indexed_catalog::has_active_vertex_property_index(graph_id, label_id, property_id) {
        return Err(RouterError::InvalidArgument(format!(
            "SEARCH ... WHERE requires an active vertex property index for label {} property {}",
            label_id.raw(),
            ranges[0].property_name
        )));
    }

    let mut intervals: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(ranges.len());
    for range in ranges {
        let value = resolve_filter_value(&range.value_expr, params)?;
        let (low, high) = gleaph_gql::numeric_range_bounds(&value, range.op).map_err(|e| {
            RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE numeric range value is not supported: {e}"
            ))
        })?;
        if low.len() > MAX_INDEX_VALUE_KEY_BYTES || high.len() > MAX_INDEX_VALUE_KEY_BYTES {
            return Err(RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE range bound exceeds maximum index key size of {MAX_INDEX_VALUE_KEY_BYTES} bytes"
            )));
        }
        intervals.push((low, high));
    }

    let merged = merge_encoded_intervals(intervals);
    if merged.is_empty() {
        return Ok(Vec::new());
    }

    collect_bounded_candidates_range_disjunction_with_store(
        graph_id,
        store,
        label_id,
        property_id,
        merged,
    )
    .await
}

/// Normalize a list of finite half-open encoded intervals by dropping empty intervals,
/// sorting canonically, and merging overlapping or touching intervals. This is a pure helper so
/// interval-merge semantics can be unit-tested independently from async index lookups.
fn merge_encoded_intervals(mut intervals: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<(Vec<u8>, Vec<u8>)> {
    intervals.retain(|(low, high)| low.as_slice() < high.as_slice());
    intervals.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut merged: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(intervals.len());
    for (low, high) in intervals {
        match merged.last_mut() {
            Some((_, prev_high)) if low.as_slice() <= prev_high.as_slice() => {
                if high.as_slice() > prev_high.as_slice() {
                    *prev_high = high;
                }
            }
            _ => merged.push((low, high)),
        }
    }
    merged.dedup();
    merged
}

/// Collect at most `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` distinct vertex subjects from the union
/// of one or more finite half-open encoded numeric intervals for `(property_id)`. Sources are
/// normalized before this call, then forwarded to the shared union collector.
async fn collect_bounded_candidates_range_disjunction<L: DisjunctionLookup>(
    clients: impl IntoIterator<Item = L>,
    label_id: VertexLabelId,
    property_id: PropertyId,
    intervals: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<Vec<VectorSubject>, RouterError> {
    let merged = merge_encoded_intervals(intervals);
    let union_sources: Vec<CandidateUnionSource> = merged
        .into_iter()
        .map(|(low, high)| CandidateUnionSource::Range {
            property_id: property_id.raw(),
            low,
            high,
        })
        .collect();
    collect_bounded_candidates_union_inner(clients, label_id, union_sources).await
}

async fn collect_bounded_candidates_range_disjunction_with_store(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    property_id: PropertyId,
    intervals: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<Vec<VectorSubject>, RouterError> {
    let targets = store
        .graph_index_lookup_targets(graph_id)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    collect_bounded_candidates_range_disjunction(
        targets.into_iter().map(RouterIndexClient::new),
        label_id,
        property_id,
        intervals,
    )
    .await
}

/// Shared bounded candidate union collector for equality and numeric range disjunctions.
/// Sources are walked sequentially per index target; label filtering precedes counting, global
/// subject deduplication is applied, and the 4097th distinct label-qualified subject fails explicitly.
async fn collect_bounded_candidates_union_inner<L: DisjunctionLookup>(
    clients: impl IntoIterator<Item = L>,
    label_id: VertexLabelId,
    mut sources: Vec<CandidateUnionSource>,
) -> Result<Vec<VectorSubject>, RouterError> {
    sources.sort();
    sources.dedup();

    let mut seen: std::collections::HashSet<(ShardId, u32)> = std::collections::HashSet::new();

    for client in clients {
        for source in &sources {
            let mut after: Option<PropertyPostingCursor> = None;
            loop {
                let page = match source {
                    CandidateUnionSource::Equal { property_id, value } => {
                        client
                            .lookup_equal_page(
                                *property_id,
                                value.clone(),
                                after,
                                VECTOR_FILTER_PAGE_LIMIT,
                            )
                            .await
                    }
                    CandidateUnionSource::Range {
                        property_id,
                        low,
                        high,
                    } => {
                        let range = gleaph_graph_kernel::index::PostingRangeRequest::Between {
                            low: low.clone(),
                            high: high.clone(),
                        };
                        client
                            .lookup_range_page(*property_id, range, after, VECTOR_FILTER_PAGE_LIMIT)
                            .await
                    }
                }
                .map_err(|e| {
                    RouterError::InvalidArgument(format!("property-index lookup failed: {e}"))
                })?;

                let label_hits = client
                    .filter_hits_by_label(label_id.raw() as u32, page.hits)
                    .await
                    .map_err(|e| {
                        RouterError::InvalidArgument(format!(
                            "property-index label filter failed: {e}"
                        ))
                    })?;

                for hit in label_hits {
                    if !seen.insert((hit.shard_id, hit.vertex_id)) {
                        continue;
                    }
                    if seen.len() > MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
                        return Err(RouterError::InvalidArgument(format!(
                            "SEARCH ... WHERE filter produced more than {MAX_VECTOR_SEARCH_FILTER_CANDIDATES} candidates"
                        )));
                    }
                }

                if page.done {
                    break;
                }
                after = page.next;
            }
        }
    }

    Ok(seen
        .into_iter()
        .map(|(shard_id, vertex_id)| VectorSubject::Vertex {
            shard_id,
            vertex_id,
        })
        .collect())
}

/// Resolve property id, active-index proof, value encoding, and key-size validation for every
/// equality arm. Shared between pure equality and mixed equality-plus-range filter paths so both
/// enforce identical diagnostics.
fn resolve_equality_arms(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    binding: &str,
    arms: &[SearchFilterArm],
    params: &BTreeMap<String, Value>,
) -> Result<Vec<IndexEqualSpec>, RouterError> {
    let mut equal_specs = Vec::with_capacity(arms.len());
    for arm in arms {
        let property_id = resolve_search_property_id(graph_id, store, binding, &arm.property_name)?;
        if !indexed_catalog::has_active_vertex_property_index(graph_id, label_id, property_id) {
            return Err(RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE requires an active vertex property index for label {} property {}",
                label_id.raw(),
                arm.property_name
            )));
        }
        let value = resolve_filter_value(&arm.value_expr, params)?;
        let encoded = encode_filter_value(&value)?;
        if encoded.len() > MAX_INDEX_VALUE_KEY_BYTES {
            return Err(RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE value exceeds maximum index key size of {MAX_INDEX_VALUE_KEY_BYTES} bytes"
            )));
        }
        equal_specs.push(IndexEqualSpec::vertex(property_id.raw(), encoded));
    }
    Ok(equal_specs)
}

/// Resolve the property, coverage, and encoded half-open interval for a range SEARCH filter.
/// Returns `Ok(None)` when the interval is empty or contradictory, so the caller can apply the
/// empty-candidate dispatch contract without touching the Property Index. Returns `Ok(Some)` with
/// the validated bounds when the interval is non-empty.
fn resolve_filtered_range_interval(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    binding: &str,
    ranges: &[SearchFilterRange],
    params: &BTreeMap<String, Value>,
) -> Result<Option<(PropertyId, Vec<u8>, Vec<u8>)>, RouterError> {
    if ranges.is_empty() {
        return Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE range filter is empty".into(),
        ));
    }
    // All range arms share the same property in accepted shapes (two-sided) or a single arm.
    let property_id =
        resolve_search_property_id(graph_id, store, binding, &ranges[0].property_name)?;
    if !indexed_catalog::has_active_vertex_property_index(graph_id, label_id, property_id) {
        return Err(RouterError::InvalidArgument(format!(
            "SEARCH ... WHERE requires an active vertex property index for label {} property {}",
            label_id.raw(),
            ranges[0].property_name
        )));
    }

    let mut final_low: Option<Vec<u8>> = None;
    let mut final_high: Option<Vec<u8>> = None;
    for range in ranges {
        let value = resolve_filter_value(&range.value_expr, params)?;
        let (low, high) = gleaph_gql::numeric_range_bounds(&value, range.op).map_err(|e| {
            RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE numeric range value is not supported: {e}"
            ))
        })?;
        if low.len() > MAX_INDEX_VALUE_KEY_BYTES || high.len() > MAX_INDEX_VALUE_KEY_BYTES {
            return Err(RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE range bound exceeds maximum index key size of {MAX_INDEX_VALUE_KEY_BYTES} bytes"
            )));
        }
        final_low = Some(match final_low {
            Some(prev) => std::cmp::max(prev, low),
            None => low,
        });
        final_high = Some(match final_high {
            Some(prev) => std::cmp::min(prev, high),
            None => high,
        });
    }

    let low = final_low.unwrap();
    let high = final_high.unwrap();
    if low >= high {
        Ok(None)
    } else {
        Ok(Some((property_id, low, high)))
    }
}

async fn resolve_filtered_range_candidates(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    binding: &str,
    ranges: &[SearchFilterRange],
    params: &BTreeMap<String, Value>,
) -> Result<Vec<VectorSubject>, RouterError> {
    match resolve_filtered_range_interval(graph_id, store, label_id, binding, ranges, params)? {
        Some((property_id, low, high)) => {
            collect_bounded_candidates_range(graph_id, store, label_id, property_id, low, high)
                .await
        }
        None => {
            // Contradictory or empty numeric interval: the candidate set is empty without touching
            // the Property Index, but the dispatch contract still runs the prefix/aggregate path.
            Ok(Vec::new())
        }
    }
}

fn resolve_search_property_id(
    graph_id: GraphId,
    store: &RouterStore,
    binding: &str,
    property_name: &str,
) -> Result<PropertyId, RouterError> {
    store
        .lookup_property_id(graph_id, property_name)
        .map_err(|e| {
            RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE binding `{binding}` property `{property_name}`: {e}"
            ))
        })
}

fn resolve_filter_value(
    expr: &gleaph_gql::ast::Expr,
    params: &BTreeMap<String, Value>,
) -> Result<Value, RouterError> {
    match &expr.kind {
        ExprKind::Literal(v) => Ok(v.clone()),
        ExprKind::Parameter(name) => {
            let key = name.strip_prefix('$').unwrap_or(name.as_str());
            params
                .get(key)
                .ok_or_else(|| RouterError::InvalidArgument(format!("missing parameter ${name}")))
                .cloned()
        }
        _ => Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE value must be a literal or parameter".into(),
        )),
    }
}

fn encode_filter_value(value: &Value) -> Result<Vec<u8>, RouterError> {
    if matches!(value, Value::Null) {
        return Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE value must not be NULL".into(),
        ));
    }
    let Some(bytes) = value_to_index_key_bytes(value).map_err(|e| {
        RouterError::InvalidArgument(format!(
            "SEARCH ... WHERE value is not supported by property index keys: {e}"
        ))
    })?
    else {
        return Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE value must not be NULL".into(),
        ));
    };
    Ok(bytes)
}

const VECTOR_FILTER_PAGE_LIMIT: u32 = 10_000;

/// Collect at most `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` distinct vertex subjects from the Property
/// Index equality bucket for `(property_id, encoded_value)`. Stops at the first page that would
/// exceed the bound and returns an explicit `InvalidArgument` error without materializing the
/// remaining postings.
async fn collect_bounded_candidates_equal(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    property_id: PropertyId,
    encoded_value: Vec<u8>,
) -> Result<Vec<VectorSubject>, RouterError> {
    collect_bounded_candidates(graph_id, store, label_id, |client, after| {
        let value = encoded_value.clone();
        async move {
            client
                .lookup_equal_page(gleaph_graph_kernel::index::LookupEqualPageRequest {
                    property_id: property_id.raw(),
                    value: value.clone(),
                    after,
                    limit: VECTOR_FILTER_PAGE_LIMIT,
                })
                .await
        }
    })
    .await
}

/// Collect at most `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` distinct vertex subjects from the
/// server-side equality intersection of two `(property_id, encoded_value)` arms. Stops at the
/// first page that would exceed the bound and returns an explicit error.
async fn collect_bounded_candidates_intersection(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    specs: Vec<IndexEqualSpec>,
) -> Result<Vec<VectorSubject>, RouterError> {
    collect_bounded_candidates(graph_id, store, label_id, |client, after| {
        let value = specs.clone();
        async move {
            client
                .lookup_intersection_page(LookupIntersectionPageRequest {
                    specs: value.clone(),
                    after,
                    limit: VECTOR_FILTER_PAGE_LIMIT,
                })
                .await
        }
    })
    .await
}

/// Collect at most `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` distinct vertex subjects from a finite
/// half-open encoded numeric range for `(property_id, low, high)`. Stops at the first page that
/// would exceed the bound and returns an explicit error.
async fn collect_bounded_candidates_range(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    property_id: PropertyId,
    low: Vec<u8>,
    high: Vec<u8>,
) -> Result<Vec<VectorSubject>, RouterError> {
    collect_bounded_candidates(graph_id, store, label_id, |client, after| {
        let range = gleaph_graph_kernel::index::PostingRangeRequest::Between {
            low: low.clone(),
            high: high.clone(),
        };
        async move {
            client
                .lookup_range_page(property_id.raw(), range, after, VECTOR_FILTER_PAGE_LIMIT)
                .await
        }
    })
    .await
}

/// Trait for the index operations used by the equality-disjunction and range-disjunction
/// collectors. Production uses `RouterIndexClient`; tests inject a mock to verify paging,
/// deduplication, merging, and bounds without canister calls.
trait DisjunctionLookup: Clone {
    async fn lookup_equal_page(
        &self,
        property_id: u32,
        value: Vec<u8>,
        after: Option<PropertyPostingCursor>,
        limit: u32,
    ) -> Result<PostingHitPage, String>;

    async fn lookup_range_page(
        &self,
        property_id: u32,
        range: gleaph_graph_kernel::index::PostingRangeRequest,
        after: Option<PropertyPostingCursor>,
        limit: u32,
    ) -> Result<PostingHitPage, String>;

    async fn filter_hits_by_label(
        &self,
        vertex_label_id: u32,
        hits: Vec<PostingHit>,
    ) -> Result<Vec<PostingHit>, String>;
}

impl DisjunctionLookup for RouterIndexClient {
    async fn lookup_equal_page(
        &self,
        property_id: u32,
        value: Vec<u8>,
        after: Option<PropertyPostingCursor>,
        limit: u32,
    ) -> Result<PostingHitPage, String> {
        self.lookup_equal_page(LookupEqualPageRequest {
            property_id,
            value,
            after,
            limit,
        })
        .await
    }

    async fn lookup_range_page(
        &self,
        property_id: u32,
        range: gleaph_graph_kernel::index::PostingRangeRequest,
        after: Option<PropertyPostingCursor>,
        limit: u32,
    ) -> Result<PostingHitPage, String> {
        self.lookup_range_page(property_id, range, after, limit)
            .await
    }

    async fn filter_hits_by_label(
        &self,
        vertex_label_id: u32,
        hits: Vec<PostingHit>,
    ) -> Result<Vec<PostingHit>, String> {
        self.filter_hits_by_label(vertex_label_id, hits).await
    }
}

/// One source in a bounded union of Property Index postings for `SEARCH ... WHERE` disjunctions.
/// The Router owns this enum so equality and range disjunctions share a single candidate
/// accumulation loop, preserving identical paging, label filtering, deduplication, and candidate-cap
/// behavior.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CandidateUnionSource {
    Equal {
        property_id: u32,
        value: Vec<u8>,
    },
    Range {
        property_id: u32,
        low: Vec<u8>,
        high: Vec<u8>,
    },
}

/// Collect at most `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` distinct vertex subjects from the union of
/// several Property Index sources. Each source is either an equality bucket or a finite half-open
/// encoded range interval; sources are walked sequentially per index target, so each page fetch,
/// label-filter, and merge step preserves the existing per-page work bound and stops as soon as the
/// 4097th distinct label-qualified subject is observed. Sources are normalized before lookup so
/// identical equality values and merged range intervals are walked once.
async fn collect_bounded_candidates_equal_disjunction<L: DisjunctionLookup>(
    clients: impl IntoIterator<Item = L>,
    label_id: VertexLabelId,
    sources: Vec<(PropertyId, Vec<u8>)>,
) -> Result<Vec<VectorSubject>, RouterError> {
    let union_sources: Vec<CandidateUnionSource> = sources
        .into_iter()
        .map(|(property_id, value)| CandidateUnionSource::Equal {
            property_id: property_id.raw(),
            value,
        })
        .collect();
    collect_bounded_candidates_union_inner(clients, label_id, union_sources).await
}

async fn collect_bounded_candidates_equal_disjunction_with_store(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    sources: Vec<(PropertyId, Vec<u8>)>,
) -> Result<Vec<VectorSubject>, RouterError> {
    let targets = store
        .graph_index_lookup_targets(graph_id)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    collect_bounded_candidates_equal_disjunction(
        targets.into_iter().map(RouterIndexClient::new),
        label_id,
        sources,
    )
    .await
}

/// Generic bounded candidate collector used by both single-arm equality and two-arm intersection.
/// Calls `fetch_page` for each index-canister target, label-filters the survivors, deduplicates
/// globally, and fails on the 4097th distinct label-qualified subject.
async fn collect_bounded_candidates<F, Fut>(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    fetch_page: F,
) -> Result<Vec<VectorSubject>, RouterError>
where
    F: FnMut(RouterIndexClient, Option<gleaph_graph_kernel::index::PropertyPostingCursor>) -> Fut,
    Fut: std::future::Future<Output = Result<PostingHitPage, String>>,
{
    collect_bounded_candidates_inner(
        graph_id,
        store,
        label_id,
        fetch_page,
        |client, label_id, hits| async move { client.filter_hits_by_label(label_id, hits).await },
    )
    .await
}

async fn collect_bounded_candidates_inner<F, Fut, L, Lfut>(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    mut fetch_page: F,
    mut filter_hits: L,
) -> Result<Vec<VectorSubject>, RouterError>
where
    F: FnMut(RouterIndexClient, Option<PropertyPostingCursor>) -> Fut,
    Fut: std::future::Future<Output = Result<PostingHitPage, String>>,
    L: FnMut(RouterIndexClient, u32, Vec<PostingHit>) -> Lfut,
    Lfut: std::future::Future<Output = Result<Vec<PostingHit>, String>>,
{
    let targets = store
        .graph_index_lookup_targets(graph_id)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    if targets.is_empty() {
        return Err(RouterError::InvalidArgument(
            "no index canister registered for logical graph".into(),
        ));
    }

    let mut seen: std::collections::HashSet<(ShardId, u32)> = std::collections::HashSet::new();
    let mut after: Option<PropertyPostingCursor> = None;
    let mut target_idx = 0usize;

    while target_idx < targets.len() {
        let principal = targets[target_idx];
        let client = RouterIndexClient::new(principal);
        let page = fetch_page(client.clone(), after).await.map_err(|e| {
            RouterError::InvalidArgument(format!("property-index lookup failed: {e}"))
        })?;

        let label_hits = filter_hits(client, label_id.raw() as u32, page.hits)
            .await
            .map_err(|e| {
                RouterError::InvalidArgument(format!("property-index label filter failed: {e}"))
            })?;

        for hit in label_hits {
            if !seen.insert((hit.shard_id, hit.vertex_id)) {
                continue;
            }
            if seen.len() > MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
                return Err(RouterError::InvalidArgument(format!(
                    "SEARCH ... WHERE candidate set exceeds maximum of {MAX_VECTOR_SEARCH_FILTER_CANDIDATES}"
                )));
            }
        }

        if page.done {
            target_idx += 1;
            after = None;
        } else {
            after = page.next;
        }
    }

    Ok(seen
        .into_iter()
        .map(|(shard_id, vertex_id)| VectorSubject::Vertex {
            shard_id,
            vertex_id,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::{embedding_name_catalog, vector_index_catalog};
    use crate::facade::store::catalog_test_support;
    use candid::Principal;
    use gleaph_gql::Value;
    use gleaph_gql::ast::{Expr, ExprKind};
    use gleaph_gql_planner::plan::{SearchOutputKind, SearchOutputPlan, SearchProviderPlan};
    use gleaph_graph_kernel::entry::{GraphId, VertexLabelId};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{PostingHit, PostingHitPage, PropertyPostingCursor};
    use gleaph_graph_kernel::vector_index::MAX_VECTOR_SEARCH_FILTER_CANDIDATES;
    use gleaph_graph_kernel::vector_index::{
        VectorEncoding, VectorIndexKind, VectorMetric, VectorSearchHit, VectorSearchRequest,
        VectorSearchResult, VectorSubject,
    };
    use std::collections::BTreeMap;

    fn vector_search_unreachable() -> impl FnOnce(
        candid::Principal,
        VectorSearchRequest,
    ) -> std::future::Ready<
        Result<VectorSearchResult, RouterError>,
    > {
        |_target, _req| {
            std::future::ready(Err(RouterError::Internal(
                "vector_search should not be called in this test".into(),
            )))
        }
    }

    fn vector_search_counter(
        hits: Vec<VectorSearchHit>,
    ) -> (
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        impl FnOnce(
            candid::Principal,
            VectorSearchRequest,
        ) -> std::future::Ready<Result<VectorSearchResult, RouterError>>,
    ) {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = count.clone();
        let mock = move |_target, _req: VectorSearchRequest| {
            count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok(VectorSearchResult { hits }))
        };
        (count, mock)
    }

    fn non_leading_search_plan_with_distance() -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("Author".into()),
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ])
    }

    fn non_leading_search_plan_with_score() -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("Author".into()),
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Score,
                    alias: "similarity".into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ])
    }

    fn bytes_expr(b: Vec<u8>) -> Expr {
        Expr::new(ExprKind::Literal(Value::Bytes(b)))
    }

    fn param_expr(name: &str) -> Expr {
        Expr::new(ExprKind::Parameter(name.to_owned()))
    }

    fn search_plan_with_distance() -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3]),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ])
    }

    #[test]
    fn resolve_query_bytes_strips_dollar_prefix_for_router_params() {
        let params = BTreeMap::from([("query".to_string(), Value::Bytes(vec![4, 5, 6]))]);
        assert_eq!(
            resolve_query_bytes(&param_expr("$query"), &params).unwrap(),
            vec![4, 5, 6]
        );
    }

    #[test]
    fn resolve_limit_u32_strips_dollar_prefix_for_router_params() {
        let params = BTreeMap::from([("k".to_string(), Value::Int64(42))]);
        assert_eq!(resolve_limit_u32(&param_expr("$k"), &params).unwrap(), 42);
    }

    #[test]
    fn try_execute_gql_search_rejects_non_query_mode() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3]),
                    limit: Expr::int(1),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            GraphId::from_raw(0),
            &[],
            GqlExecutionMode::Update,
            &RouterGraphStats::from_catalog(
                GraphId::from_raw(0),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("only supports query mode"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_rejects_dml_tail() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3]),
                    limit: Expr::int(1),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec!["Doc".into()],
                properties: vec![],
            },
        ]);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            GraphId::from_raw(0),
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                GraphId::from_raw(0),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot be followed by mutation operators"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_query_bytes_accepts_literal_and_parameter() {
        let params = BTreeMap::from([("q".to_string(), Value::Bytes(vec![4, 5, 6]))]);
        assert_eq!(
            resolve_query_bytes(&bytes_expr(vec![1, 2, 3]), &params).unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(
            resolve_query_bytes(&param_expr("q"), &params).unwrap(),
            vec![4, 5, 6]
        );
    }

    #[test]
    fn resolve_query_bytes_rejects_non_bytes() {
        let params = BTreeMap::from([("q".to_string(), Value::Int64(42))]);
        let err = resolve_query_bytes(&param_expr("q"), &params).unwrap_err();
        assert!(err.to_string().contains("must evaluate to bytes"));
    }

    #[test]
    fn resolve_limit_u32_accepts_literal_and_parameter() {
        let params = BTreeMap::from([("k".to_string(), Value::Int64(25))]);
        assert_eq!(resolve_limit_u32(&Expr::int(10), &params).unwrap(), 10);
        assert_eq!(resolve_limit_u32(&param_expr("k"), &params).unwrap(), 25);
    }

    #[test]
    fn resolve_limit_u32_rejects_non_positive_and_overflow() {
        let params = BTreeMap::new();
        assert!(resolve_limit_u32(&Expr::int(0), &params).is_err());
        assert!(resolve_limit_u32(&Expr::int(-1), &params).is_err());
        assert!(resolve_limit_u32(&bytes_expr(vec![]), &params).is_err());
    }

    #[test]
    fn build_search_seeds_groups_hits_by_shard_and_carries_distance_alias() {
        let hits = vec![
            VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                },
                distance: 1.25f32,
                embedding_incarnation: 0,
                embedding_version: 0,
            },
            VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(1),
                    vertex_id: 9,
                },
                distance: 2.5f32,
                embedding_incarnation: 0,
                embedding_version: 0,
            },
        ];
        let by_shard = build_search_seeds("d", "distance", &[], &hits, VectorMetric::L2Squared)
            .expect("build seeds");
        assert_eq!(by_shard.len(), 2);

        let shard0 = by_shard.get(&ShardId::new(0)).unwrap();
        assert_eq!(shard0.rows.len(), 1);
        assert_eq!(shard0.rows[0].vertex_bindings[0].local_vertex_id, 7);
        assert_eq!(shard0.rows[0].float64_bindings[0].value, 1.25f64);

        let shard1 = by_shard.get(&ShardId::new(1)).unwrap();
        assert_eq!(shard1.rows[0].vertex_bindings[0].local_vertex_id, 9);
        assert_eq!(shard1.rows[0].float64_bindings[0].value, 2.5f64);
    }

    #[test]
    fn build_search_seeds_includes_required_label_ids() {
        let hits = vec![VectorSearchHit {
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 5,
            },
            distance: 0.0f32,
            embedding_incarnation: 0,
            embedding_version: 0,
        }];
        let by_shard = build_search_seeds(
            "d",
            "distance",
            &[VertexLabelId::from_raw(3)],
            &hits,
            VectorMetric::L2Squared,
        )
        .expect("build seeds");
        assert_eq!(
            by_shard[&ShardId::new(0)].rows[0].vertex_bindings[0].required_vertex_label_ids,
            vec![3]
        );
    }

    #[test]
    fn build_search_seeds_rejects_duplicate_subject() {
        let hits = vec![
            VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                },
                distance: 1.0f32,
                embedding_incarnation: 0,
                embedding_version: 0,
            },
            VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                },
                distance: 2.0f32,
                embedding_incarnation: 0,
                embedding_version: 0,
            },
        ];
        let err = build_search_seeds("d", "distance", &[], &hits, VectorMetric::L2Squared)
            .expect_err("duplicate hit");
        assert!(
            err.to_string().contains("duplicate vector search hit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn strip_search_prefix_removes_node_scan_and_search() {
        let plan = search_plan_with_distance();
        let shape = SearchShape {
            binding: "d".into(),
            index_name: vec!["doc_vec".into()],
            query_expr: bytes_expr(vec![]),
            limit_expr: Expr::int(1),
            output_alias: "distance".into(),
            output_kind: SearchOutputKind::Distance,
            required_label_ids: vec![],
            filter_label_id: None,
            filter: None,
        };
        let stripped = strip_search_prefix(&plan, &shape).unwrap();
        assert_eq!(stripped.ops.len(), 1);
        assert!(matches!(stripped.ops[0], PlanOp::Project { .. }));
    }

    #[test]
    fn strip_search_prefix_rejects_empty_tail() {
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![]),
                    limit: Expr::int(1),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
        ]);
        let shape = SearchShape {
            binding: "d".into(),
            index_name: vec!["doc_vec".into()],
            query_expr: bytes_expr(vec![]),
            limit_expr: Expr::int(1),
            output_alias: "distance".into(),
            output_kind: SearchOutputKind::Distance,
            required_label_ids: vec![],
            filter_label_id: None,
            filter: None,
        };
        let err = strip_search_prefix(&plan, &shape).unwrap_err();
        assert!(err.to_string().contains("no tail"));
    }

    #[test]
    fn op_contains_search_detects_nested_search() {
        let inner_search = PhysicalPlan::from_ops(vec![PlanOp::Search {
            binding: "d".into(),
            provider: SearchProviderPlan::VectorIndex {
                index_name: vec!["doc_vec".into()],
                query: bytes_expr(vec![]),
                limit: Expr::int(1),
                filter: None,
            },
            output: SearchOutputPlan {
                kind: SearchOutputKind::Distance,
                alias: "distance".into(),
            },
        }]);
        assert!(op_contains_search(&PlanOp::OptionalMatch {
            sub_plan: inner_search.ops.clone(),
        }));
        assert!(op_contains_search(&PlanOp::UseGraph {
            graph_name: vec!["g".into()],
            sub_plan: Some(inner_search.ops),
        }));
        assert!(!op_contains_search(&PlanOp::NodeScan {
            variable: "n".into(),
            label: None,
            property_projection: None,
        }));
    }

    // --- ADR 0034 Slice 4: metric-aware SEARCH shape and seed conversion tests ---

    fn search_plan_with_output(kind: SearchOutputKind, alias: &str) -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind,
                    alias: alias.into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ])
    }

    fn register_vector_index_for_test(
        _store: &RouterStore,
        graph_id: GraphId,
        metric: VectorMetric,
    ) {
        let name_id = embedding_name_catalog::intern_embedding_name(graph_id, "doc_vec").unwrap();
        vector_index_catalog::register_vector_index(
            graph_id,
            1,
            name_id,
            VectorIndexKind::IvfFlat,
            metric,
            VectorEncoding::F32,
            3,
            None,
            false,
        )
        .unwrap();
        vector_index_catalog::set_vector_index_target(
            graph_id,
            1,
            vector_index_catalog::VectorIndexTarget {
                canister: candid::Principal::from_slice(&[9]),
            },
        )
        .unwrap();
        // Test-only: bypass the dynamic dispatch gate so unit tests can focus on search shape
        // validation and seed wiring without requiring a fully vector-attached shard fleet.
        vector_index_catalog::set_vector_index_activation_state_for_test(
            graph_id,
            1,
            vector_index_catalog::VectorIndexActivationState::Registered,
        )
        .unwrap();
    }

    #[test]
    fn try_execute_gql_search_rejects_score_on_l2_index() {
        let (store, _admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        let plan = search_plan_with_output(SearchOutputKind::Score, "score");
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("SCORE AS on L2Squared must fail");
        assert!(
            err.to_string().contains("not supported for metric"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_rejects_distance_on_cosine_index() {
        let (store, _admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::Cosine);
        let plan = search_plan_with_output(SearchOutputKind::Distance, "distance");
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("DISTANCE AS on Cosine must fail");
        assert!(
            err.to_string().contains("not supported for metric"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_leading_empty_hits_reaches_graph_dispatch() {
        let (store, _admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        let (count, mock) = vector_search_counter(vec![]);
        let plan = search_plan_with_output(SearchOutputKind::Distance, "distance");
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            mock,
        ));
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "vector_search must be invoked exactly once"
        );
        // With an empty hit set the Router must still dispatch the stripped tail to the live
        // shard (so a global aggregate can produce one zero row). The test catalog has no real
        // graph canister, so dispatch fails; the important contract is that it is attempted rather
        // than short-circuited to row_count_only(0).
        assert!(
            result.is_err(),
            "leading SEARCH with empty hits must reach graph dispatch: {result:?}"
        );
    }

    #[test]
    fn try_execute_gql_search_leading_stale_only_hits_reaches_graph_dispatch() {
        let (store, _admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        let stale_hit = VectorSearchHit {
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(42),
                vertex_id: 7,
            },
            distance: 1.0,
            embedding_incarnation: 0,
            embedding_version: 0,
        };
        let (count, mock) = vector_search_counter(vec![stale_hit]);
        let plan = search_plan_with_output(SearchOutputKind::Distance, "distance");
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            mock,
        ));
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "vector_search must be invoked exactly once"
        );
        // A hit for a shard outside the live topology is an empty live relation. The Router must
        // still dispatch an empty seed to shard 0 so an aggregate tail can run. This fixture has no
        // real graph canister, so reaching dispatch is observed as an error rather than a result.
        assert!(
            result.is_err(),
            "leading SEARCH with only stale hits must reach graph dispatch: {result:?}"
        );
    }

    #[test]
    fn build_search_seeds_converts_cosine_raw_to_score() {
        let hits = vec![VectorSearchHit {
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 7,
            },
            distance: 0.25f32,
            embedding_incarnation: 0,
            embedding_version: 0,
        }];
        let by_shard = build_search_seeds("d", "similarity", &[], &hits, VectorMetric::Cosine)
            .expect("build seeds");
        let row = &by_shard[&ShardId::new(0)].rows[0];
        assert_eq!(row.float64_bindings[0].value, 0.75f64, "score = 1 - raw");
        assert_eq!(row.float64_bindings[0].variable, "similarity");
    }

    #[test]
    fn build_search_seeds_rejects_non_finite_distance_for_l2() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let hits = vec![VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                },
                distance: bad,
                embedding_incarnation: 0,
                embedding_version: 0,
            }];
            let err = build_search_seeds("d", "distance", &[], &hits, VectorMetric::L2Squared)
                .expect_err("non-finite distance must fail");
            assert!(
                err.to_string()
                    .contains("distance conversion produced a non-finite value"),
                "unexpected error for {bad}: {err}"
            );
        }
    }

    #[test]
    fn build_search_seeds_rejects_non_finite_score_for_cosine() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let hits = vec![VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                },
                distance: bad,
                embedding_incarnation: 0,
                embedding_version: 0,
            }];
            let err = build_search_seeds("d", "similarity", &[], &hits, VectorMetric::Cosine)
                .expect_err("non-finite score must fail");
            assert!(
                err.to_string()
                    .contains("score conversion produced a non-finite value"),
                "unexpected error for {bad}: {err}"
            );
        }
    }

    // --- ADR 0034 Slice 5: non-leading SEARCH classification and resolved relation tests ---

    #[test]
    fn analyze_search_shape_classifies_leading_prefix() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        let plan = search_plan_with_distance();
        let position =
            analyze_search_shape(&plan, GraphId::from_raw(0), &store).expect("leading shape");
        assert!(matches!(position, SearchPosition::Leading(_)));
    }

    #[test]
    fn analyze_search_shape_classifies_non_leading_position() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        let plan = non_leading_search_plan_with_distance();
        let position =
            analyze_search_shape(&plan, GraphId::from_raw(0), &store).expect("non-leading shape");
        assert!(matches!(position, SearchPosition::NonLeading(_)));
    }

    #[test]
    fn analyze_search_shape_rejects_search_without_preceding_or_following_ops() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        // Search is the only op.
        let plan = PhysicalPlan::from_ops(vec![PlanOp::Search {
            binding: "d".into(),
            provider: SearchProviderPlan::VectorIndex {
                index_name: vec!["doc_vec".into()],
                query: bytes_expr(vec![1, 2, 3]),
                limit: Expr::int(10),
                filter: None,
            },
            output: SearchOutputPlan {
                kind: SearchOutputKind::Distance,
                alias: "distance".into(),
            },
        }]);
        let err =
            analyze_search_shape(&plan, GraphId::from_raw(0), &store).expect_err("lone Search");
        assert!(
            err.to_string()
                .contains("both preceding and following operators"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn analyze_search_shape_rejects_multiple_top_level_searches() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3]),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Search {
                binding: "d2".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3]),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance2".into(),
                },
            },
        ]);
        let err = analyze_search_shape(&plan, GraphId::from_raw(0), &store)
            .expect_err("multiple searches");
        assert!(
            err.to_string().contains("not be nested or repeated"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn analyze_search_shape_rejects_nested_search() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        let inner = PhysicalPlan::from_ops(vec![PlanOp::Search {
            binding: "d".into(),
            provider: SearchProviderPlan::VectorIndex {
                index_name: vec!["doc_vec".into()],
                query: bytes_expr(vec![1, 2, 3]),
                limit: Expr::int(10),
                filter: None,
            },
            output: SearchOutputPlan {
                kind: SearchOutputKind::Distance,
                alias: "distance".into(),
            },
        }]);
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("Author".into()),
                property_projection: None,
            },
            PlanOp::OptionalMatch {
                sub_plan: inner.ops,
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);
        let err =
            analyze_search_shape(&plan, GraphId::from_raw(0), &store).expect_err("nested search");
        assert!(
            err.to_string().contains("not be nested or repeated"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_resolved_search_wires_skips_non_live_shard_hits() {
        let hits = vec![
            VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                },
                distance: 1.25f32,
                embedding_incarnation: 0,
                embedding_version: 0,
            },
            VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(42),
                    vertex_id: 8,
                },
                distance: 2.25f32,
                embedding_incarnation: 0,
                embedding_version: 0,
            },
        ];
        let (store, _admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        let by_shard = build_resolved_search_wires(
            "d",
            "distance",
            &hits,
            VectorMetric::L2Squared,
            graph_id,
            &store,
        )
        .expect("build resolved wires");
        assert_eq!(
            by_shard.len(),
            1,
            "only live shards are included; non-live hits are ignored"
        );
        let wire = by_shard.get(&ShardId::new(0)).expect("shard 0 wire");
        assert_eq!(wire.vertex_hits.len(), 1);
        assert_eq!(wire.vertex_hits[0].local_vertex_id, 7);
    }

    #[test]
    fn build_resolved_search_wires_groups_hits_by_shard() {
        let hits = vec![VectorSearchHit {
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 7,
            },
            distance: 1.25f32,
            embedding_incarnation: 0,
            embedding_version: 0,
        }];
        let (store, _admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        let by_shard = build_resolved_search_wires(
            "d",
            "distance",
            &hits,
            VectorMetric::L2Squared,
            graph_id,
            &store,
        )
        .expect("build resolved wires");
        assert_eq!(
            by_shard.len(),
            1,
            "only the registered live shard is included"
        );
        let wire = by_shard.get(&ShardId::new(0)).expect("shard 0 wire");
        assert_eq!(wire.binding, "d");
        assert_eq!(wire.output_alias, "distance");
        assert_eq!(wire.vertex_hits.len(), 1);
        assert_eq!(wire.vertex_hits[0].local_vertex_id, 7);
        assert_eq!(wire.vertex_hits[0].value, 1.25f64);
    }

    #[test]
    fn build_resolved_search_wires_rejects_duplicate_subject() {
        let hits = vec![
            VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                },
                distance: 1.0f32,
                embedding_incarnation: 0,
                embedding_version: 0,
            },
            VectorSearchHit {
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                },
                distance: 2.0f32,
                embedding_incarnation: 0,
                embedding_version: 0,
            },
        ];
        let (store, _admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        let err = build_resolved_search_wires(
            "d",
            "distance",
            &hits,
            VectorMetric::L2Squared,
            graph_id,
            &store,
        )
        .expect_err("duplicate hit");
        assert!(
            err.to_string().contains("duplicate vector search hit"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_resolved_search_wires_rejects_non_finite_value() {
        let hits = vec![VectorSearchHit {
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 7,
            },
            distance: f32::NAN,
            embedding_incarnation: 0,
            embedding_version: 0,
        }];
        let (store, _admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        let err = build_resolved_search_wires(
            "d",
            "distance",
            &hits,
            VectorMetric::L2Squared,
            graph_id,
            &store,
        )
        .expect_err("non-finite value");
        assert!(
            err.to_string().contains("non-finite value"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_invokes_vector_search_once_for_non_leading() {
        let (store, _admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        let plan = non_leading_search_plan_with_distance();
        let hits = vec![VectorSearchHit {
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 7,
            },
            distance: 1.25f32,
            embedding_incarnation: 0,
            embedding_version: 0,
        }];
        let (count, mock) = vector_search_counter(hits);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            mock,
        ));
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "vector_search must be invoked exactly once"
        );
        // Dispatch fails because the test catalog has no registered graph canister, but the
        // one-call invariant is the contract under test here.
        assert!(
            result.is_err(),
            "non-leading SEARCH must reach dispatch: {result:?}"
        );
    }

    #[test]
    fn try_execute_gql_search_non_leading_rejects_dml_tail() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        let mut plan = non_leading_search_plan_with_distance();
        plan.ops.push(PlanOp::InsertVertex {
            variable: Some("n".into()),
            labels: vec!["Doc".into()],
            properties: vec![],
        });
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            GraphId::from_raw(0),
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                GraphId::from_raw(0),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("DML tail must fail");
        assert!(
            err.to_string()
                .contains("cannot be followed by mutation operators"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_non_leading_rejects_score_on_l2() {
        let (store, _admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        let plan = non_leading_search_plan_with_score();
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("SCORE AS on L2Squared must fail");
        assert!(
            err.to_string().contains("not supported for metric"),
            "unexpected error: {err}"
        );
    }

    // --- ADR 0034 Slice 6: filtered SEARCH classification and validation tests ---

    fn filter_eq_expr(property: &str, value: Value) -> Expr {
        Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::new(ExprKind::Variable("d".to_string()))),
                property: property.to_string(),
            })),
            op: gleaph_gql::ast::CmpOp::Eq,
            right: Box::new(Expr::new(ExprKind::Literal(value))),
        })
    }
    fn filter_and_expr(left: Expr, right: Expr) -> Expr {
        Expr::new(ExprKind::And(Box::new(left), Box::new(right)))
    }
    fn filter_range_expr(property: &str, op: gleaph_gql::ast::CmpOp, value: Value) -> Expr {
        Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::new(ExprKind::Variable("d".to_string()))),
                property: property.to_string(),
            })),
            op,
            right: Box::new(Expr::new(ExprKind::Literal(value))),
        })
    }

    fn search_plan_with_filter(filter: Expr) -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: Some("Document".into()),
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
                    limit: Expr::int(10),
                    filter: Some(filter),
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ])
    }

    fn filter_or_expr(left: Expr, right: Expr) -> Expr {
        Expr::new(ExprKind::Or(Box::new(left), Box::new(right)))
    }
    fn filter_or_n(exprs: Vec<Expr>) -> Expr {
        assert!(exprs.len() >= 2);
        let mut expr = filter_or_expr(exprs[0].clone(), exprs[1].clone());
        for e in exprs.into_iter().skip(2) {
            expr = filter_or_expr(expr, e);
        }
        expr
    }

    #[test]
    fn extract_search_filter_classifies_equality_disjunction() {
        let filter = filter_or_expr(
            filter_eq_expr("category", Value::Int64(1)),
            filter_eq_expr("category", Value::Int64(2)),
        );
        let f = extract_search_filter("d", &filter).expect("equality disjunction");
        match f {
            SearchFilter::EqualityDisjunction(arms) => {
                assert_eq!(arms.len(), 2);
                assert_eq!(arms[0].property_name, "category");
                assert_eq!(arms[1].property_name, "category");
            }
            _ => panic!("expected EqualityDisjunction"),
        }
    }

    #[test]
    fn extract_search_filter_accepts_disjunction_across_properties() {
        let filter = filter_or_expr(
            filter_eq_expr("category", Value::Int64(1)),
            filter_eq_expr("tenant", Value::Int64(2)),
        );
        match extract_search_filter("d", &filter).expect("cross-property OR must be accepted") {
            SearchFilter::EqualityDisjunction(arms) => {
                assert_eq!(arms.len(), 2);
                assert_eq!(arms[0].property_name, "category");
                assert_eq!(arms[1].property_name, "tenant");
            }
            other => panic!("expected EqualityDisjunction, got {other:?}"),
        }
    }

    #[test]
    fn extract_search_filter_rejects_mixed_range_disjunction() {
        let filter = filter_or_expr(
            filter_eq_expr("category", Value::Int64(1)),
            filter_range_expr("category", gleaph_gql::ast::CmpOp::Ge, Value::Int64(10)),
        );
        let f = extract_search_filter("d", &filter);
        let err = f.expect_err("range inside OR must fail");
        assert!(
            err.to_string()
                .contains("does not support this equality/range mixture")
                || err.to_string().contains("must be a comparison predicate")
                || err.to_string().contains("OR-connected arms must all be"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_single_equality_remains_conjunction() {
        // A single equality leaf is not classified as a disjunction.
        let filter = filter_eq_expr("category", Value::Int64(1));
        let f = extract_search_filter("d", &filter).expect("single equality");
        assert!(
            matches!(f, SearchFilter::Equality(_)),
            "single equality must remain SearchFilter::Equality"
        );
    }

    #[test]
    fn extract_search_filter_classifies_same_property_range_disjunction() {
        let filter = filter_or_expr(
            filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10)),
            filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(50)),
        );
        let f = extract_search_filter("d", &filter).expect("range disjunction");
        match f {
            SearchFilter::RangeDisjunction(ranges) => {
                assert_eq!(ranges.len(), 2);
                assert_eq!(ranges[0].property_name, "price");
                assert_eq!(ranges[1].property_name, "price");
            }
            _ => panic!("expected RangeDisjunction"),
        }
    }

    #[test]
    fn extract_search_filter_rejects_range_disjunction_across_properties() {
        let filter = filter_or_expr(
            filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10)),
            filter_range_expr("rating", gleaph_gql::ast::CmpOp::Ge, Value::Int64(4)),
        );
        let err = extract_search_filter("d", &filter)
            .expect_err("range disjunction across properties must fail");
        assert!(
            err.to_string()
                .contains("does not support this equality/range mixture")
                || err.to_string().contains("must be a comparison predicate")
                || err.to_string().contains("OR-connected arms must all be"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_single_range_remains_conjunction() {
        let filter = filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10));
        let f = extract_search_filter("d", &filter).expect("single range");
        assert!(
            matches!(f, SearchFilter::Range(_)),
            "single range must remain SearchFilter::Range"
        );
    }

    #[test]
    fn analyze_search_shape_rejects_range_disjunction_too_many_arms() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        let arms: Vec<Expr> = (0..=MAX_RANGE_DISJUNCTION_ARMS)
            .map(|i| filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(i as i64)))
            .collect();
        let plan = search_plan_with_filter(filter_or_n(arms));
        let err = analyze_search_shape(&plan, graph_id, &store)
            .expect_err("range disjunction with >MAX arms must fail");
        assert!(
            err.to_string()
                .contains("range disjunction supports at most")
                || err
                    .to_string()
                    .contains("at most {MAX_RANGE_DISJUNCTION_ARMS}"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn analyze_search_shape_rejects_equality_disjunction_too_many_arms() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        let arms: Vec<Expr> = (0..=MAX_EQUALITY_DISJUNCTION_ARMS)
            .map(|i| filter_eq_expr("category", Value::Int64(i as i64)))
            .collect();
        let plan = search_plan_with_filter(filter_or_n(arms));
        let err = analyze_search_shape(&plan, graph_id, &store)
            .expect_err("disjunction with >MAX arms must fail");
        assert!(
            err.to_string()
                .contains("at most {MAX_EQUALITY_DISJUNCTION_ARMS}")
                || err
                    .to_string()
                    .contains("equality disjunction supports at most"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_normalizes_reversed_operands() {
        let filter = Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::Literal(Value::Text("doc".into())))),
            op: gleaph_gql::ast::CmpOp::Eq,
            right: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::new(ExprKind::Variable("d".to_string()))),
                property: "category".to_string(),
            })),
        });
        let f = extract_search_filter("d", &filter).expect("reversed operands");
        let arms = match f {
            SearchFilter::Equality(arms) => arms,
            _ => panic!("expected equality filter"),
        };
        assert_eq!(arms.len(), 1);
        assert_eq!(arms[0].property_name, "category");
        assert!(matches!(arms[0].value_expr.kind, ExprKind::Literal(_)));
    }

    #[test]
    fn analyze_search_shape_rejects_filtered_leading_without_label() {
        let store = crate::facade::store::RouterStore::new();
        store.init_from_args(&crate::init::RouterInitArgs {
            issuing_principal: candid::Principal::anonymous(),
            initial_admins: vec![],
        });
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: None,
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
                    limit: Expr::int(10),
                    filter: Some(filter_eq_expr("category", Value::Text("doc".into()))),
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);
        let err = analyze_search_shape(&plan, GraphId::from_raw(0), &store)
            .expect_err("filtered search without label");
        assert!(
            err.to_string()
                .contains("requires a labeled leading search"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_rejects_filtered_non_leading_without_label() {
        let (store, _admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        let mut plan = non_leading_search_plan_with_distance();
        if let PlanOp::Search { provider, .. } = &mut plan.ops[1] {
            let SearchProviderPlan::VectorIndex { filter, .. } = provider;
            *filter = Some(filter_eq_expr("category", Value::Text("doc".into())));
        }
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("filtered non-leading without label proof must fail");
        assert!(
            err.to_string()
                .contains("requires a statically proved label"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn analyze_search_shape_accepts_non_leading_range_disjunction_with_label_proof() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern Document");
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Author")
            .expect("intern Author");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern price");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create range index");

        let filter = filter_or_expr(
            filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10)),
            filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(50)),
        );
        let plan = non_leading_search_plan_with_node_scan_label_proof(filter);

        let position = analyze_search_shape(&plan, graph_id, &store)
            .expect("non-leading range disjunction accepted");
        assert!(
            matches!(
                position,
                SearchPosition::NonLeading(SearchShape {
                    filter: Some(SearchFilter::RangeDisjunction(_)),
                    filter_label_id: Some(_),
                    ..
                })
            ),
            "expected filtered non-leading range disjunction, got {position:?}"
        );
    }

    #[test]
    fn analyze_search_shape_rejects_non_leading_range_disjunction_without_label_proof() {
        let (store, _admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        let filter = filter_or_expr(
            filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10)),
            filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(50)),
        );
        let mut plan = non_leading_search_plan_with_distance();
        // Remove the leading Author label proof by clearing it.
        if let PlanOp::NodeScan { label, .. } = &mut plan.ops[0] {
            *label = None;
        }
        if let PlanOp::Search { provider, .. } = &mut plan.ops[1] {
            let SearchProviderPlan::VectorIndex { filter: f, .. } = provider;
            *f = Some(filter);
        }

        let err = analyze_search_shape(&plan, graph_id, &store)
            .expect_err("non-leading range disjunction without label proof must fail");
        assert!(
            err.to_string()
                .contains("requires a statically proved label"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_rejects_dispatch_blocked() {
        let (store, _admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        // Restore the production fail-closed stored state so the dynamic gate is exercised.
        vector_index_catalog::set_vector_index_activation_state_for_test(
            graph_id,
            1,
            vector_index_catalog::VectorIndexActivationState::DispatchBlocked,
        )
        .unwrap();
        let plan = search_plan_with_output(SearchOutputKind::Distance, "distance");
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("DispatchBlocked must fail");
        assert!(
            err.to_string()
                .contains("vector dispatch activation blocked"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_filtered_gate_checked_before_candidate_lookup() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        // setup_with_shard creates the tenant.main graph with one shard; intern the Document label
        // and category property, plus the exact index, so the only remaining failure path is the
        // dispatch gate.
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern property");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create exact index");
        // Force the activation state back to DispatchBlocked. The target is set, so this is the
        // production fail-closed state; the gate must fire before the candidate lookup.
        vector_index_catalog::set_vector_index_activation_state_for_test(
            graph_id,
            1,
            vector_index_catalog::VectorIndexActivationState::DispatchBlocked,
        )
        .unwrap();
        let plan = search_plan_with_filter(filter_eq_expr("category", Value::Text("doc".into())));
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("DispatchBlocked must fail even with empty candidates");
        assert!(
            err.to_string()
                .contains("vector dispatch activation blocked"),
            "expected activation blocked, got {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_filtered_rejects_missing_exact_index() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern property");
        // Property "category" is registered, but there is no active vertex equality index for
        // (Document, category).
        let plan = search_plan_with_filter(filter_eq_expr("category", Value::Text("doc".into())));
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("missing property must fail");
        assert!(
            err.to_string()
                .contains("requires an active vertex property index"),
            "unexpected error: {err}"
        );
    }

    // --- ADR 0034 Slice 7: non-leading filtered SEARCH label-proof tests ---

    fn is_labeled_expr(var: &str, label: &str, negated: bool) -> Expr {
        Expr::new(ExprKind::IsLabeled {
            expr: Box::new(Expr::new(ExprKind::Variable(var.to_string()))),
            label: gleaph_gql::types::LabelExpr::Name(label.to_string()),
            negated,
        })
    }

    fn non_leading_search_plan_with_property_filter_proof(filter: Expr) -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("Author".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "d".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("WROTE".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_payload_predicate: None,
                edge_vector_predicate: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
                near_group_var: None,
                far_group_var: None,
                path_var: None,
                emit_path_binding: true,
            },
            PlanOp::PropertyFilter {
                predicates: vec![is_labeled_expr("d", "Document", false)],
                stage: 0,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
                    limit: Expr::int(10),
                    filter: Some(filter),
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ])
    }

    fn non_leading_search_plan_with_node_scan_label_proof(filter: Expr) -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("Author".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "d".into(),
                label: Some("Document".into()),
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: bytes_expr(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
                    limit: Expr::int(10),
                    filter: Some(filter),
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ])
    }

    #[test]
    fn analyze_search_shape_accepts_non_leading_filtered_property_filter_proof() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern Document");
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Author")
            .expect("intern Author");
        let plan = non_leading_search_plan_with_property_filter_proof(filter_eq_expr(
            "category",
            Value::Text("doc".into()),
        ));
        let position = analyze_search_shape(&plan, graph_id, &store)
            .expect("non-leading filtered shape with property-filter proof");
        assert!(matches!(position, SearchPosition::NonLeading(_)));
    }

    #[test]
    fn analyze_search_shape_accepts_non_leading_filtered_node_scan_proof() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern Document");
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Author")
            .expect("intern Author");
        let plan = non_leading_search_plan_with_node_scan_label_proof(filter_eq_expr(
            "category",
            Value::Text("doc".into()),
        ));
        let position = analyze_search_shape(&plan, graph_id, &store)
            .expect("non-leading filtered shape with node-scan proof");
        assert!(matches!(position, SearchPosition::NonLeading(_)));
    }

    #[test]
    fn analyze_search_shape_accepts_repeated_same_label_proof() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern Document");
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Author")
            .expect("intern Author");
        let mut plan = non_leading_search_plan_with_node_scan_label_proof(filter_eq_expr(
            "category",
            Value::Text("doc".into()),
        ));
        // Add a redundant PropertyFilter IsLabeled(d, Document) before Search.
        plan.ops.insert(
            2,
            PlanOp::PropertyFilter {
                predicates: vec![is_labeled_expr("d", "Document", false)],
                stage: 0,
            },
        );
        let position = analyze_search_shape(&plan, graph_id, &store)
            .expect("repeated same-label proof should be accepted");
        assert!(matches!(position, SearchPosition::NonLeading(_)));
    }

    #[test]
    fn analyze_search_shape_rejects_non_leading_filtered_missing_label() {
        let (store, _admin, graph_id) = catalog_test_support::setup();
        let mut plan = non_leading_search_plan_with_distance();
        if let PlanOp::Search { provider, .. } = &mut plan.ops[1] {
            let SearchProviderPlan::VectorIndex { filter, .. } = provider;
            *filter = Some(filter_eq_expr("category", Value::Text("doc".into())));
        }
        let err = analyze_search_shape(&plan, graph_id, &store)
            .expect_err("non-leading filtered search without label proof");
        assert!(
            err.to_string()
                .contains("requires a statically proved label"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn analyze_search_shape_rejects_non_leading_filtered_two_labels() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern Document");
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Other")
            .expect("intern Other");
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Author")
            .expect("intern Author");
        let mut plan = non_leading_search_plan_with_node_scan_label_proof(filter_eq_expr(
            "category",
            Value::Text("doc".into()),
        ));
        // Add a contradictory PropertyFilter IsLabeled(d, Other) before Search.
        plan.ops.insert(
            2,
            PlanOp::PropertyFilter {
                predicates: vec![is_labeled_expr("d", "Other", false)],
                stage: 0,
            },
        );
        let err =
            analyze_search_shape(&plan, graph_id, &store).expect_err("two distinct label proofs");
        assert!(
            err.to_string().contains("multiple distinct labels"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn analyze_search_shape_rejects_non_leading_filtered_negated_label() {
        let (store, _admin, graph_id) = catalog_test_support::setup();
        let mut plan = non_leading_search_plan_with_distance();
        plan.ops.insert(
            1,
            PlanOp::PropertyFilter {
                predicates: vec![is_labeled_expr("d", "Document", true)],
                stage: 0,
            },
        );
        if let PlanOp::Search { provider, .. } = &mut plan.ops[2] {
            let SearchProviderPlan::VectorIndex { filter, .. } = provider;
            *filter = Some(filter_eq_expr("category", Value::Text("doc".into())));
        }
        let err = analyze_search_shape(&plan, graph_id, &store).expect_err("negated label proof");
        assert!(
            err.to_string().contains("must not be negated"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn analyze_search_shape_rejects_non_leading_filtered_label_after_search() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern Document");
        let mut plan = non_leading_search_plan_with_distance();
        if let PlanOp::Search { provider, .. } = &mut plan.ops[1] {
            let SearchProviderPlan::VectorIndex { filter, .. } = provider;
            *filter = Some(filter_eq_expr("category", Value::Text("doc".into())));
        }
        // Append the label proof after SEARCH so it is not in the prefix.
        plan.ops.insert(
            2,
            PlanOp::PropertyFilter {
                predicates: vec![is_labeled_expr("d", "Document", false)],
                stage: 0,
            },
        );
        let err =
            analyze_search_shape(&plan, graph_id, &store).expect_err("label proof after search");
        assert!(
            err.to_string()
                .contains("requires a statically proved label"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn analyze_search_shape_rejects_non_leading_filtered_rebound_variable() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern Document");
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Author")
            .expect("intern Author");
        let mut plan = non_leading_search_plan_with_node_scan_label_proof(filter_eq_expr(
            "category",
            Value::Text("doc".into()),
        ));
        // Insert a Let that rebinds d between the proof and the Search.
        plan.ops.insert(
            2,
            PlanOp::Let {
                bindings: vec![gleaph_gql::ast::LetBinding {
                    span: gleaph_gql::token::Span::DUMMY,
                    variable: "d".to_string(),
                    value: Expr::new(ExprKind::Variable("a".to_string())),
                }],
            },
        );
        let err =
            analyze_search_shape(&plan, graph_id, &store).expect_err("rebound searched variable");
        assert!(
            err.to_string()
                .contains("invalidated by a later prefix operator"),
            "unexpected error: {err}"
        );
    }
    #[test]
    fn analyze_search_shape_accepts_non_leading_filtered_search_with_src_expand() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern Document");
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Author")
            .expect("intern Author");
        // d is introduced by a NodeScan proof, then used as the src of a later Expand.
        // Reading d as an expand source must not invalidate the earlier label proof.
        let mut plan = non_leading_search_plan_with_node_scan_label_proof(filter_eq_expr(
            "category",
            Value::Text("doc".into()),
        ));
        plan.ops.insert(
            2,
            PlanOp::Expand {
                src: "d".into(),
                edge: "e".into(),
                dst: "x".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("CITES".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_payload_predicate: None,
                edge_vector_predicate: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
                near_group_var: None,
                far_group_var: None,
                path_var: None,
                emit_path_binding: true,
            },
        );
        let position = analyze_search_shape(&plan, graph_id, &store)
            .expect("expand source read should not invalidate label proof");
        assert!(matches!(position, SearchPosition::NonLeading(_)));
    }

    #[test]
    fn try_execute_gql_search_non_leading_filtered_rejects_missing_exact_index() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern property");
        // Property "category" is registered but there is no active vertex equality index for
        // (Document, category).
        let plan = non_leading_search_plan_with_property_filter_proof(filter_eq_expr(
            "category",
            Value::Text("doc".into()),
        ));
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("missing exact index must fail");
        assert!(
            err.to_string()
                .contains("requires an active vertex property index"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_non_leading_filtered_gate_checked_before_candidate_lookup() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern property");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create exact index");
        vector_index_catalog::set_vector_index_activation_state_for_test(
            graph_id,
            1,
            vector_index_catalog::VectorIndexActivationState::DispatchBlocked,
        )
        .unwrap();
        let plan = non_leading_search_plan_with_property_filter_proof(filter_eq_expr(
            "category",
            Value::Text("doc".into()),
        ));
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("DispatchBlocked must fail even with empty candidates");
        assert!(
            err.to_string()
                .contains("vector dispatch activation blocked"),
            "expected activation blocked, got {err}"
        );
    }

    #[test]
    fn extract_search_filter_accepts_eight_arm_conjunction() {
        let mut arms: Vec<Expr> = (1..=8)
            .map(|i| filter_eq_expr(format!("p{i}").as_str(), Value::Int64(i)))
            .collect();
        // Build a left-deep AND tree; order is preserved for client-side extraction.
        let mut filter = arms.remove(0);
        for arm in arms {
            filter = filter_and_expr(filter, arm);
        }
        let f = extract_search_filter("d", &filter).expect("eight equality arms");
        let extracted = match f {
            SearchFilter::Equality(arms) => arms,
            _ => panic!("expected equality filter"),
        };
        assert_eq!(extracted.len(), 8);
        let props: Vec<_> = extracted.iter().map(|a| a.property_name.as_str()).collect();
        assert!(props.contains(&"p1"));
        assert!(props.contains(&"p8"));
    }

    #[test]
    fn extract_search_filter_rejects_nine_arm_conjunction() {
        let mut arms: Vec<Expr> = (1..=9)
            .map(|i| filter_eq_expr(format!("p{i}").as_str(), Value::Int64(i)))
            .collect();
        let mut filter = arms.remove(0);
        for arm in arms {
            filter = filter_and_expr(filter, arm);
        }
        let err = extract_search_filter("d", &filter).expect_err("nine arms must fail");
        assert!(
            err.to_string().contains("at most 8 equality conjuncts"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_rejects_duplicate_property_in_eight_arm_conjunction() {
        let arms: Vec<Expr> = (1..=7)
            .map(|i| filter_eq_expr(format!("p{i}").as_str(), Value::Int64(i)))
            .chain(std::iter::once(filter_eq_expr("p3", Value::Int64(99))))
            .collect();
        let mut filter = arms[0].clone();
        for arm in &arms[1..] {
            filter = filter_and_expr(filter, arm.clone());
        }
        let err = extract_search_filter("d", &filter).expect_err("duplicate property must fail");
        assert!(
            err.to_string().contains("distinct properties"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_accepts_two_arm_conjunction() {
        let left = filter_eq_expr("category", Value::Text("doc".into()));
        let right = filter_eq_expr("tenant_id", Value::Int64(7));
        let f = extract_search_filter("d", &filter_and_expr(left, right)).expect("two arms");
        let arms = match f {
            SearchFilter::Equality(arms) => arms,
            _ => panic!("expected equality filter"),
        };
        assert_eq!(arms.len(), 2);
        let props: Vec<_> = arms.iter().map(|a| a.property_name.as_str()).collect();
        assert!(props.contains(&"category"));
        assert!(props.contains(&"tenant_id"));
    }

    #[test]
    fn extract_search_filter_rejects_duplicate_property_in_conjunction() {
        let left = filter_eq_expr("category", Value::Text("doc".into()));
        let right = filter_eq_expr("category", Value::Int64(7));
        let err = extract_search_filter("d", &filter_and_expr(left, right))
            .expect_err("duplicate property must fail");
        assert!(
            err.to_string().contains("distinct properties"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_non_leading_conjunction_rejects_missing_second_exact_index() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern category");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "tenant_id")
            .expect("intern tenant_id");
        // Only category has an active vertex equality index; tenant_id is covered but no index.
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create category index");
        let filter = filter_and_expr(
            filter_eq_expr("category", Value::Text("doc".into())),
            filter_eq_expr("tenant_id", Value::Int64(7)),
        );
        let plan = non_leading_search_plan_with_property_filter_proof(filter);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("missing second exact index must fail");
        assert!(
            err.to_string()
                .contains("requires an active vertex property index"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_accepts_numeric_range_operators() {
        for op in [
            gleaph_gql::ast::CmpOp::Ge,
            gleaph_gql::ast::CmpOp::Gt,
            gleaph_gql::ast::CmpOp::Le,
            gleaph_gql::ast::CmpOp::Lt,
        ] {
            let f = extract_search_filter("d", &filter_range_expr("price", op, Value::Int64(5)))
                .expect("range predicate must be accepted");
            match f {
                SearchFilter::Range(ranges) => {
                    assert_eq!(ranges.len(), 1);
                    assert_eq!(ranges[0].property_name, "price");
                    assert_eq!(ranges[0].op, op);
                }
                _ => panic!("expected range filter for {op:?}"),
            }
        }
    }

    #[test]
    fn extract_search_filter_normalizes_reversed_range_operands() {
        let filter = Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::Literal(Value::Int64(5)))),
            op: gleaph_gql::ast::CmpOp::Lt,
            right: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::new(ExprKind::Variable("d".to_string()))),
                property: "price".to_string(),
            })),
        });
        let f = extract_search_filter("d", &filter).expect("reversed range operands");
        match f {
            SearchFilter::Range(ranges) => {
                assert_eq!(ranges.len(), 1);
                assert_eq!(ranges[0].property_name, "price");
                // 5 < d.price normalizes to d.price > 5.
                assert_eq!(ranges[0].op, gleaph_gql::ast::CmpOp::Gt);
            }
            _ => panic!("expected range filter"),
        }
    }

    #[test]
    fn extract_search_filter_accepts_two_sided_range() {
        let lower = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let upper = filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10));
        let f = extract_search_filter("d", &filter_and_expr(lower, upper))
            .expect("two-sided range must be accepted");
        match f {
            SearchFilter::Range(ranges) => {
                assert_eq!(ranges.len(), 2);
                assert!(ranges.iter().all(|r| r.property_name == "price"));
            }
            _ => panic!("expected range filter"),
        }
    }

    #[test]
    fn extract_search_filter_accepts_two_sided_range_reversed_order() {
        let upper = filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10));
        let lower = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let f = extract_search_filter("d", &filter_and_expr(upper, lower))
            .expect("two-sided range in any order must be accepted");
        match f {
            SearchFilter::Range(ranges) => assert_eq!(ranges.len(), 2),
            _ => panic!("expected range filter"),
        }
    }

    #[test]
    fn extract_search_filter_rejects_two_lower_bounds() {
        let a = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let b = filter_range_expr("price", gleaph_gql::ast::CmpOp::Gt, Value::Int64(2));
        let err = extract_search_filter("d", &filter_and_expr(a, b))
            .expect_err("two lower bounds must fail");
        assert!(
            err.to_string().contains("lower bound"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_rejects_two_upper_bounds() {
        let a = filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10));
        let b = filter_range_expr("price", gleaph_gql::ast::CmpOp::Le, Value::Int64(8));
        let err = extract_search_filter("d", &filter_and_expr(a, b))
            .expect_err("two upper bounds must fail");
        assert!(
            err.to_string().contains("upper bound"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_rejects_two_sided_range_different_properties() {
        let lower = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let upper = filter_range_expr("score", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10));
        let err = extract_search_filter("d", &filter_and_expr(lower, upper))
            .expect_err("different-property two-sided range must fail");
        assert!(
            err.to_string().contains("same property"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_accepts_mixed_equality_and_range_distinct_properties() {
        let eq = filter_eq_expr("category", Value::Text("doc".into()));
        let range = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let f = extract_search_filter("d", &filter_and_expr(eq, range))
            .expect("mixed equality/range on distinct properties must be accepted");
        match f {
            SearchFilter::Mixed(arms, ranges) => {
                assert_eq!(arms.len(), 1);
                assert_eq!(arms[0].property_name, "category");
                assert_eq!(ranges.len(), 1);
                assert_eq!(ranges[0].property_name, "price");
                assert_eq!(ranges[0].op, gleaph_gql::ast::CmpOp::Ge);
            }
            _ => panic!("expected Mixed filter"),
        }
    }

    #[test]
    fn extract_search_filter_accepts_mixed_equality_plus_two_sided_range() {
        let eq = filter_eq_expr("category", Value::Text("doc".into()));
        let lower = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let upper = filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10));
        let inner = filter_and_expr(lower, upper);
        let f = extract_search_filter("d", &filter_and_expr(eq, inner))
            .expect("mixed equality plus two-sided range must be accepted");
        match f {
            SearchFilter::Mixed(arms, ranges) => {
                assert_eq!(arms.len(), 1);
                assert_eq!(arms[0].property_name, "category");
                assert_eq!(ranges.len(), 2);
                assert!(ranges.iter().all(|r| r.property_name == "price"));
            }
            _ => panic!("expected Mixed filter"),
        }
    }

    #[test]
    fn extract_search_filter_accepts_mixed_equality_plus_two_sided_range_reversed_order() {
        let eq = filter_eq_expr("category", Value::Text("doc".into()));
        let lower = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let upper = filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10));
        // Equality between the two range arms.
        let f = extract_search_filter("d", &filter_and_expr(filter_and_expr(lower, eq), upper))
            .expect("equality in the middle must be accepted");
        assert!(
            matches!(f, SearchFilter::Mixed(..)),
            "expected Mixed filter"
        );
    }

    #[test]
    fn extract_search_filter_accepts_mixed_two_equalities_plus_range() {
        let a = filter_eq_expr("category", Value::Text("doc".into()));
        let b = filter_eq_expr("tenant", Value::Int64(7));
        let range = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let inner = filter_and_expr(a, range);
        let f = extract_search_filter("d", &filter_and_expr(b, inner))
            .expect("two equalities plus range must be accepted");
        assert!(
            matches!(f, SearchFilter::Mixed(ref arms, ref ranges) if arms.len() == 2 && ranges.len() == 1),
            "expected Mixed with two equality arms and one range: {f:?}"
        );
    }

    #[test]
    fn extract_search_filter_rejects_mixed_equality_plus_two_sided_range_same_property() {
        let eq = filter_eq_expr("price", Value::Int64(5));
        let lower = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let upper = filter_range_expr("price", gleaph_gql::ast::CmpOp::Lt, Value::Int64(10));
        let inner = filter_and_expr(lower, upper);
        let err = extract_search_filter("d", &filter_and_expr(eq, inner))
            .expect_err("same-property equality plus two-sided range must fail");
        assert!(
            err.to_string().contains("distinct properties"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_accepts_mixed_equality_and_range_reversed_conjunct_order() {
        let eq = filter_eq_expr("category", Value::Text("doc".into()));
        let range = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let f = extract_search_filter("d", &filter_and_expr(range, eq))
            .expect("range-then-equality order must be accepted");
        assert!(
            matches!(f, SearchFilter::Mixed(..)),
            "expected Mixed filter"
        );
    }

    #[test]
    fn extract_search_filter_rejects_mixed_equality_and_range_same_property() {
        let eq = filter_eq_expr("price", Value::Int64(50));
        let range = filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(5));
        let err = extract_search_filter("d", &filter_and_expr(eq, range))
            .expect_err("same-property mixed equality/range must fail");
        assert!(
            err.to_string().contains("distinct properties"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extract_search_filter_rejects_mixed_nine_equality_arms_plus_range() {
        let mut conjunction =
            filter_range_expr("price", gleaph_gql::ast::CmpOp::Ge, Value::Int64(0));
        for i in 0..9 {
            let arm = filter_eq_expr(&format!("prop_{i}"), Value::Text(format!("v{i}")));
            conjunction = filter_and_expr(conjunction, arm);
        }
        let err = extract_search_filter("d", &conjunction)
            .expect_err("nine equality arms plus range must fail");
        assert!(
            err.to_string().contains("at most 8 equality conjuncts"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_filtered_range_candidates_short_circuits_empty_intersection() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        let label_id = store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern property");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create price index");

        // d.price > 10 AND d.price <= 10: the intersection is empty. The Router must short-circuit
        // before issuing a Property Index request and return an empty candidate vector.
        let filter = SearchFilter::Range(vec![
            SearchFilterRange {
                property_name: "price".into(),
                op: gleaph_gql::ast::CmpOp::Gt,
                value_expr: Expr::new(ExprKind::Literal(Value::Int64(10))),
            },
            SearchFilterRange {
                property_name: "price".into(),
                op: gleaph_gql::ast::CmpOp::Le,
                value_expr: Expr::new(ExprKind::Literal(Value::Int64(10))),
            },
        ]);
        let params = BTreeMap::new();
        let candidates = pollster::block_on(resolve_filtered_candidates(
            graph_id, &store, label_id, "d", &filter, &params,
        ))
        .expect("resolve filtered candidates");
        assert!(
            candidates.is_empty(),
            "empty intersection must yield no candidates"
        );
    }

    #[test]
    fn resolve_filtered_range_interval_intersects_mixed_numeric_widths() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        let label_id = store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern property");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create price index");

        // d.price >= 5 (Int64) AND d.price < 10.0 (Float64) must produce a non-empty encoded
        // interval, proving mixed-width values share the same canonical numeric domain.
        let ranges = vec![
            SearchFilterRange {
                property_name: "price".into(),
                op: gleaph_gql::ast::CmpOp::Ge,
                value_expr: Expr::new(ExprKind::Literal(Value::Int64(5))),
            },
            SearchFilterRange {
                property_name: "price".into(),
                op: gleaph_gql::ast::CmpOp::Lt,
                value_expr: Expr::new(ExprKind::Literal(Value::Float64(10.0))),
            },
        ];
        let params = BTreeMap::new();
        let interval =
            resolve_filtered_range_interval(graph_id, &store, label_id, "d", &ranges, &params)
                .expect("resolve interval");
        let (_property_id, low, high) = interval.expect("interval must be non-empty");
        assert!(
            low < high,
            "mixed-width intersection must produce a valid half-open interval"
        );
    }

    #[test]
    fn try_execute_gql_search_non_leading_range_rejects_text_value() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .unwrap();
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .unwrap();
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create exact index");

        let plan = non_leading_search_plan_with_property_filter_proof(filter_range_expr(
            "category",
            gleaph_gql::ast::CmpOp::Ge,
            Value::Text("doc".into()),
        ));
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("text range value must fail");
        assert!(
            err.to_string()
                .contains("numeric range value is not supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_mixed_rejects_missing_range_index() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern category");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern price");
        // Only the equality index exists; the range index is missing.
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create category index");

        let filter = filter_mixed_expr(
            "category",
            Value::Text("doc".into()),
            "price",
            gleaph_gql::ast::CmpOp::Ge,
            Value::Int64(5),
        );
        let plan = search_plan_with_filter(filter);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("missing range index must fail");
        assert!(
            err.to_string()
                .contains("requires an active vertex property index"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_mixed_rejects_missing_equality_index() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern category");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern price");
        // Only the range index exists; the equality index is missing.
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create price index");

        let filter = filter_mixed_expr(
            "category",
            Value::Text("doc".into()),
            "price",
            gleaph_gql::ast::CmpOp::Ge,
            Value::Int64(5),
        );
        let plan = search_plan_with_filter(filter);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("missing equality index must fail");
        assert!(
            err.to_string()
                .contains("requires an active vertex property index"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_mixed_rejects_text_range_value() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .unwrap();
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .unwrap();
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .unwrap();
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create category index");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create price index");

        let filter = filter_mixed_expr(
            "category",
            Value::Text("doc".into()),
            "price",
            gleaph_gql::ast::CmpOp::Ge,
            Value::Text("cheap".into()),
        );
        let plan = non_leading_search_plan_with_property_filter_proof(filter);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("text range value must fail");
        assert!(
            err.to_string()
                .contains("numeric range value is not supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_mixed_gate_checked_before_candidate_lookup() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern category");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern price");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create category index");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create price index");
        vector_index_catalog::set_vector_index_activation_state_for_test(
            graph_id,
            1,
            vector_index_catalog::VectorIndexActivationState::DispatchBlocked,
        )
        .unwrap();

        let filter = filter_mixed_expr(
            "category",
            Value::Text("doc".into()),
            "price",
            gleaph_gql::ast::CmpOp::Ge,
            Value::Int64(5),
        );
        let plan = search_plan_with_filter(filter);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("DispatchBlocked must fail before candidate lookup");
        assert!(
            err.to_string()
                .contains("vector dispatch activation blocked"),
            "expected activation blocked, got {err}"
        );
    }

    #[test]
    fn resolve_filtered_mixed_candidates_short_circuits_empty_two_sided_interval() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        let label_id = store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern category");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern price");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create category index");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create price index");

        let filter = SearchFilter::Mixed(
            vec![SearchFilterArm {
                property_name: "category".into(),
                value_expr: Expr::new(ExprKind::Literal(Value::Text("doc".into()))),
            }],
            vec![
                SearchFilterRange {
                    property_name: "price".into(),
                    op: gleaph_gql::ast::CmpOp::Gt,
                    value_expr: Expr::new(ExprKind::Literal(Value::Int64(10))),
                },
                SearchFilterRange {
                    property_name: "price".into(),
                    op: gleaph_gql::ast::CmpOp::Le,
                    value_expr: Expr::new(ExprKind::Literal(Value::Int64(10))),
                },
            ],
        );
        let params = BTreeMap::new();
        let candidates = pollster::block_on(resolve_filtered_candidates(
            graph_id, &store, label_id, "d", &filter, &params,
        ))
        .expect("resolve filtered candidates");
        assert!(
            candidates.is_empty(),
            "empty two-sided interval must yield no candidates"
        );
    }

    #[test]
    fn resolve_filtered_mixed_candidates_equal_inclusive_endpoint_is_non_empty() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        let label_id = store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern category");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern price");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create category index");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create price index");

        let filter = SearchFilter::Mixed(
            vec![SearchFilterArm {
                property_name: "category".into(),
                value_expr: Expr::new(ExprKind::Literal(Value::Text("doc".into()))),
            }],
            vec![
                SearchFilterRange {
                    property_name: "price".into(),
                    op: gleaph_gql::ast::CmpOp::Ge,
                    value_expr: Expr::new(ExprKind::Literal(Value::Int64(5))),
                },
                SearchFilterRange {
                    property_name: "price".into(),
                    op: gleaph_gql::ast::CmpOp::Le,
                    value_expr: Expr::new(ExprKind::Literal(Value::Int64(5))),
                },
            ],
        );
        let params = BTreeMap::new();
        let ranges = match &filter {
            SearchFilter::Mixed(_, ranges) => ranges,
            _ => panic!("expected Mixed filter"),
        };
        let interval =
            resolve_filtered_range_interval(graph_id, &store, label_id, "d", ranges, &params)
                .expect("resolve interval");
        let (_property_id, low, high) =
            interval.expect("equal inclusive endpoints must produce an interval");
        assert!(
            low < high,
            "[5, 5] inclusive endpoint must produce a non-empty half-open interval"
        );
    }

    #[test]
    fn resolve_filtered_mixed_candidates_intersects_mixed_numeric_widths() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        let label_id = store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern category");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern price");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create category index");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create price index");

        let filter = SearchFilter::Mixed(
            vec![SearchFilterArm {
                property_name: "category".into(),
                value_expr: Expr::new(ExprKind::Literal(Value::Text("doc".into()))),
            }],
            vec![
                SearchFilterRange {
                    property_name: "price".into(),
                    op: gleaph_gql::ast::CmpOp::Ge,
                    value_expr: Expr::new(ExprKind::Literal(Value::Int64(5))),
                },
                SearchFilterRange {
                    property_name: "price".into(),
                    op: gleaph_gql::ast::CmpOp::Lt,
                    value_expr: Expr::new(ExprKind::Literal(Value::Float64(10.0))),
                },
            ],
        );
        let params = BTreeMap::new();
        let ranges = match &filter {
            SearchFilter::Mixed(_, ranges) => ranges,
            _ => panic!("expected Mixed filter"),
        };
        let interval =
            resolve_filtered_range_interval(graph_id, &store, label_id, "d", ranges, &params)
                .expect("resolve interval");
        let (_property_id, low, high) = interval.expect("mixed widths must intersect");
        assert!(
            low < high,
            "mixed-width intersection for Slice 12 must be valid"
        );
    }

    #[test]
    fn try_execute_gql_search_mixed_two_sided_gate_checked_before_candidate_lookup() {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern category");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern price");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create category index");
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create price index");
        vector_index_catalog::set_vector_index_activation_state_for_test(
            graph_id,
            1,
            vector_index_catalog::VectorIndexActivationState::DispatchBlocked,
        )
        .unwrap();

        let filter = filter_mixed_two_range_expr(
            "category",
            Value::Text("doc".into()),
            "price",
            gleaph_gql::ast::CmpOp::Ge,
            Value::Int64(5),
            gleaph_gql::ast::CmpOp::Lt,
            Value::Int64(10),
        );
        let plan = search_plan_with_filter(filter);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("DispatchBlocked must fail before candidate lookup");
        assert!(
            err.to_string()
                .contains("vector dispatch activation blocked"),
            "expected activation blocked, got {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_mixed_two_sided_rejects_missing_equality_index() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern category");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern price");
        // Only the range index exists; category equality index is missing.
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "price".into(),
                edge_direction: None,
            },
        ))
        .expect("create price index");

        let filter = filter_mixed_two_range_expr(
            "category",
            Value::Text("doc".into()),
            "price",
            gleaph_gql::ast::CmpOp::Ge,
            Value::Int64(5),
            gleaph_gql::ast::CmpOp::Lt,
            Value::Int64(10),
        );
        let plan = search_plan_with_filter(filter);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("missing equality index must fail");
        assert!(
            err.to_string()
                .contains("requires an active vertex property index"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn try_execute_gql_search_mixed_two_sided_rejects_missing_range_index() {
        let (store, admin, graph_id) = catalog_test_support::setup();
        register_vector_index_for_test(&store, graph_id, VectorMetric::L2Squared);
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .expect("intern label");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .expect("intern category");
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "price")
            .expect("intern price");
        // Only the equality index exists; price range index is missing.
        futures::executor::block_on(crate::index_catalog::create_admin_compat_property_index(
            graph_id,
            crate::index_ddl::IndexTarget {
                kind: gleaph_graph_kernel::index::IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "category".into(),
                edge_direction: None,
            },
        ))
        .expect("create category index");

        let filter = filter_mixed_two_range_expr(
            "category",
            Value::Text("doc".into()),
            "price",
            gleaph_gql::ast::CmpOp::Ge,
            Value::Int64(5),
            gleaph_gql::ast::CmpOp::Lt,
            Value::Int64(10),
        );
        let plan = search_plan_with_filter(filter);
        let result = pollster::block_on(try_execute_gql_search(
            &plan,
            graph_id,
            &[],
            GqlExecutionMode::Query,
            &RouterGraphStats::from_catalog(
                graph_id,
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
                std::collections::BTreeSet::new(),
            ),
            &store,
            candid::Principal::anonymous(),
            vector_search_unreachable(),
        ));
        let err = result.expect_err("missing range index must fail");
        assert!(
            err.to_string()
                .contains("requires an active vertex property index"),
            "unexpected error: {err}"
        );
    }

    fn filter_mixed_expr(
        eq_property: &str,
        eq_value: Value,
        range_property: &str,
        range_op: gleaph_gql::ast::CmpOp,
        range_value: Value,
    ) -> Expr {
        filter_and_expr(
            filter_eq_expr(eq_property, eq_value),
            filter_range_expr(range_property, range_op, range_value),
        )
    }

    fn filter_mixed_two_range_expr(
        eq_property: &str,
        eq_value: Value,
        range_property: &str,
        lower_op: gleaph_gql::ast::CmpOp,
        lower_value: Value,
        upper_op: gleaph_gql::ast::CmpOp,
        upper_value: Value,
    ) -> Expr {
        filter_and_expr(
            filter_eq_expr(eq_property, eq_value),
            filter_and_expr(
                filter_range_expr(range_property, lower_op, lower_value),
                filter_range_expr(range_property, upper_op, upper_value),
            ),
        )
    }

    fn collect_candidates_with_pages(
        graph_id: GraphId,
        store: &RouterStore,
        label_id: VertexLabelId,
        pages: Vec<Vec<Vec<PostingHit>>>,
        filtered_hits: Vec<Vec<Vec<PostingHit>>>,
    ) -> Result<Vec<VectorSubject>, RouterError> {
        // `pages` and `filtered_hits` are modeled as target-then-page sequences. The helper
        // must track the current target and advance only after that target's iterator is empty,
        // because a target may contribute more than one page.
        let target_idx = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut page_iters: Vec<_> = pages.into_iter().map(|p| p.into_iter()).collect();
        let mut filter_iters: Vec<_> = filtered_hits.into_iter().map(|p| p.into_iter()).collect();
        pollster::block_on(collect_bounded_candidates_inner(
            graph_id,
            store,
            label_id,
            |_client, _after| {
                let idx = target_idx.load(std::sync::atomic::Ordering::SeqCst);
                let hits = page_iters[idx].next().unwrap_or_default();
                let done = page_iters[idx].len() == 0;
                std::future::ready(Ok(PostingHitPage {
                    hits,
                    next: None,
                    done,
                }))
            },
            |_client, _label_id, _hits| {
                let idx = target_idx.load(std::sync::atomic::Ordering::SeqCst);
                let filtered = filter_iters[idx].next().unwrap_or_default();
                if filter_iters[idx].len() == 0 {
                    target_idx.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                std::future::ready(Ok(filtered))
            },
        ))
    }

    fn store_with_one_index_canister() -> (RouterStore, Principal, GraphId) {
        let (store, admin, graph_id) = catalog_test_support::setup_with_shard(ShardId::new(0));
        store
            .admin_intern_vertex_label(admin, catalog_test_support::GRAPH, "Document")
            .unwrap();
        store
            .admin_intern_property(admin, catalog_test_support::GRAPH, "category")
            .unwrap();
        (store, admin, graph_id)
    }

    #[test]
    fn collect_bounded_candidates_rejects_4097th_subject() {
        let (store, _admin, graph_id) = store_with_one_index_canister();
        let label_id = VertexLabelId::from_raw(1);
        let mut page = Vec::new();
        for i in 0..=MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
            page.push(PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: i as u32,
            });
        }
        let err = collect_candidates_with_pages(
            graph_id,
            &store,
            label_id,
            vec![vec![page.clone()]],
            vec![vec![page]],
        )
        .expect_err("4097th distinct subject must fail");
        assert!(
            err.to_string().contains("candidate set exceeds maximum"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn collect_bounded_candidates_allows_exactly_4096_subjects() {
        let (store, _admin, graph_id) = store_with_one_index_canister();
        let label_id = VertexLabelId::from_raw(1);
        let mut page = Vec::new();
        for i in 0..MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
            page.push(PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: i as u32,
            });
        }
        let candidates = collect_candidates_with_pages(
            graph_id,
            &store,
            label_id,
            vec![vec![page.clone()]],
            vec![vec![page]],
        )
        .expect("4096 distinct subjects must succeed");
        assert_eq!(candidates.len(), MAX_VECTOR_SEARCH_FILTER_CANDIDATES);
    }

    #[test]
    fn collect_bounded_candidates_label_filter_happens_before_counting() {
        let (store, _admin, graph_id) = store_with_one_index_canister();
        let label_id = VertexLabelId::from_raw(1);
        // Raw page carries 4097 hits, but all but 4096 are filtered out by label filtering
        // before counting.
        let mut page = Vec::new();
        for i in 0..=MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
            page.push(PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: i as u32,
            });
        }
        let filtered: Vec<PostingHit> = page
            .iter()
            .take(MAX_VECTOR_SEARCH_FILTER_CANDIDATES)
            .copied()
            .collect();
        let candidates = collect_candidates_with_pages(
            graph_id,
            &store,
            label_id,
            vec![vec![page]],
            vec![vec![filtered]],
        )
        .expect("label-filtered survivors must fit the bound");
        assert_eq!(candidates.len(), MAX_VECTOR_SEARCH_FILTER_CANDIDATES);
    }

    fn store_with_two_index_canisters() -> (RouterStore, Principal, GraphId) {
        let (store, admin, graph_id) = catalog_test_support::setup();
        let make_principal = |seed: u8| {
            let mut bytes = [seed; 29];
            bytes[0] = seed + 10;
            Principal::from_slice(&bytes)
        };
        futures::executor::block_on(store.admin_register_shard(
            admin,
            crate::types::AdminRegisterShardArgs {
                shard_id: ShardId::new(0),
                graph_canister: make_principal(1),
                index_canister: make_principal(2),
                logical_graph_name: catalog_test_support::GRAPH.into(),
            },
        ))
        .unwrap();
        futures::executor::block_on(store.admin_register_shard(
            admin,
            crate::types::AdminRegisterShardArgs {
                shard_id: ShardId::new(1),
                graph_canister: make_principal(3),
                index_canister: make_principal(4),
                logical_graph_name: catalog_test_support::GRAPH.into(),
            },
        ))
        .unwrap();
        (store, admin, graph_id)
    }

    #[test]
    fn collect_bounded_candidates_resets_cursor_across_targets() {
        let (store, _admin, graph_id) = store_with_two_index_canisters();
        let label_id = VertexLabelId::from_raw(1);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let result = pollster::block_on(collect_bounded_candidates_inner(
            graph_id,
            &store,
            label_id,
            |_client, after| {
                let calls = calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert!(
                    after.is_none(),
                    "each target must start from a fresh cursor, got {after:?}"
                );
                std::future::ready(Ok(PostingHitPage {
                    hits: vec![PostingHit {
                        shard_id: ShardId::new(0),
                        vertex_id: calls as u32 + 1,
                    }],
                    next: Some(PropertyPostingCursor {
                        value: vec![1],
                        shard_id: ShardId::new(0),
                        vertex_id: 1,
                    }),
                    done: true,
                }))
            },
            |_client, _label_id, hits| std::future::ready(Ok(hits)),
        ));
        assert_eq!(result.unwrap().len(), 2);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "cursor must reset so each target gets its own call"
        );
    }

    #[test]
    fn collect_bounded_candidates_dedups_across_targets() {
        let (store, _admin, graph_id) = store_with_two_index_canisters();
        let label_id = VertexLabelId::from_raw(1);
        let hits = vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 7,
        }];
        let candidates = collect_candidates_with_pages(
            graph_id,
            &store,
            label_id,
            vec![vec![hits.clone()], vec![hits.clone()]],
            vec![vec![hits.clone()], vec![hits.clone()]],
        )
        .expect("dedup across targets");
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn collect_bounded_candidates_continues_past_first_page() {
        let (store, _admin, graph_id) = store_with_one_index_canister();
        let label_id = VertexLabelId::from_raw(1);
        let cursor = PropertyPostingCursor {
            value: vec![1],
            shard_id: ShardId::new(0),
            vertex_id: 0,
        };
        let mut saw_after = false;
        let result = pollster::block_on(collect_bounded_candidates_inner(
            graph_id,
            &store,
            label_id,
            |_client, after| {
                if after.is_some() {
                    saw_after = true;
                    return std::future::ready(Ok(PostingHitPage {
                        hits: vec![PostingHit {
                            shard_id: ShardId::new(0),
                            vertex_id: 2,
                        }],
                        next: None,
                        done: true,
                    }));
                }
                std::future::ready(Ok(PostingHitPage {
                    hits: vec![PostingHit {
                        shard_id: ShardId::new(0),
                        vertex_id: 1,
                    }],
                    next: Some(cursor.clone()),
                    done: false,
                }))
            },
            |_client, _label_id, hits| std::future::ready(Ok(hits)),
        ))
        .expect("multi-page continuation");
        assert!(
            saw_after,
            "second call must receive the cursor from the first page"
        );
        assert_eq!(result.len(), 2);
    }

    // --- ADR 0034 Slices 15 and 16: equality-disjunction collector contract tests ---

    #[derive(Clone)]
    struct MockDisjunctionClient {
        calls:
            std::sync::Arc<std::sync::Mutex<Vec<(EqualOrRangeKey, Option<PropertyPostingCursor>)>>>,
        pages_by_value: std::collections::BTreeMap<Vec<u8>, Vec<PostingHitPage>>,
        pages_by_interval: std::collections::BTreeMap<(Vec<u8>, Vec<u8>), Vec<PostingHitPage>>,
        next_index:
            std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<EqualOrRangeKey, usize>>>,
        label_filter: fn(Vec<PostingHit>) -> Vec<PostingHit>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum EqualOrRangeKey {
        Equal(Vec<u8>),
        Range(Vec<u8>, Vec<u8>),
    }

    impl Default for MockDisjunctionClient {
        fn default() -> Self {
            Self {
                calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                pages_by_value: std::collections::BTreeMap::new(),
                pages_by_interval: std::collections::BTreeMap::new(),
                next_index: std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::BTreeMap::new(),
                )),
                label_filter: |hits| hits,
            }
        }
    }

    impl MockDisjunctionClient {
        fn with_pages(
            pages_by_value: std::collections::BTreeMap<Vec<u8>, Vec<PostingHitPage>>,
        ) -> Self {
            Self {
                calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                pages_by_value,
                pages_by_interval: std::collections::BTreeMap::new(),
                next_index: std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::BTreeMap::new(),
                )),
                label_filter: |hits| hits,
            }
        }

        fn with_interval_pages(
            pages_by_interval: std::collections::BTreeMap<(Vec<u8>, Vec<u8>), Vec<PostingHitPage>>,
        ) -> Self {
            Self {
                calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                pages_by_value: std::collections::BTreeMap::new(),
                pages_by_interval,
                next_index: std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::BTreeMap::new(),
                )),
                label_filter: |hits| hits,
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl DisjunctionLookup for MockDisjunctionClient {
        async fn lookup_equal_page(
            &self,
            _property_id: u32,
            value: Vec<u8>,
            after: Option<PropertyPostingCursor>,
            _limit: u32,
        ) -> Result<PostingHitPage, String> {
            let key = EqualOrRangeKey::Equal(value.clone());
            self.calls.lock().unwrap().push((key.clone(), after));
            let idx = {
                let mut map = self.next_index.lock().unwrap();
                let entry = map.entry(key).or_insert(0);
                let i = *entry;
                *entry += 1;
                i
            };
            let page = self
                .pages_by_value
                .get(&value)
                .and_then(|pages| pages.get(idx))
                .cloned()
                .unwrap_or(PostingHitPage {
                    hits: vec![],
                    next: None,
                    done: true,
                });
            Ok(page)
        }

        async fn lookup_range_page(
            &self,
            _property_id: u32,
            range: gleaph_graph_kernel::index::PostingRangeRequest,
            after: Option<PropertyPostingCursor>,
            _limit: u32,
        ) -> Result<PostingHitPage, String> {
            let key = match range {
                gleaph_graph_kernel::index::PostingRangeRequest::Between { low, high } => {
                    EqualOrRangeKey::Range(low.clone(), high.clone())
                }
                _ => return Err("mock only supports Between range requests".into()),
            };
            self.calls.lock().unwrap().push((key.clone(), after));
            let idx = {
                let mut map = self.next_index.lock().unwrap();
                let entry = map.entry(key.clone()).or_insert(0);
                let i = *entry;
                *entry += 1;
                i
            };
            let page = match &key {
                EqualOrRangeKey::Range(low, high) => self
                    .pages_by_interval
                    .get(&(low.clone(), high.clone()))
                    .and_then(|pages| pages.get(idx))
                    .cloned(),
                EqualOrRangeKey::Equal(_) => None,
            }
            .unwrap_or(PostingHitPage {
                hits: vec![],
                next: None,
                done: true,
            });
            Ok(page)
        }

        async fn filter_hits_by_label(
            &self,
            _vertex_label_id: u32,
            hits: Vec<PostingHit>,
        ) -> Result<Vec<PostingHit>, String> {
            Ok((self.label_filter)(hits))
        }
    }

    #[test]
    fn collect_range_disjunction_unions_intervals_and_dedupes() {
        let interval_a = (vec![10u8], vec![20u8]);
        let interval_b = (vec![15u8], vec![25u8]);
        let merged = (vec![10u8], vec![25u8]);
        let shared = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 100,
        };
        let only_a = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 101,
        };
        let only_b = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 102,
        };

        // The mock is keyed on the merged interval: the collector must normalize overlapping
        // inputs into one lookup.
        let client =
            MockDisjunctionClient::with_interval_pages(std::collections::BTreeMap::from([(
                merged.clone(),
                vec![PostingHitPage {
                    hits: vec![shared, only_a, only_b],
                    next: None,
                    done: true,
                }],
            )]));

        let result = pollster::block_on(collect_bounded_candidates_range_disjunction(
            vec![client.clone()],
            VertexLabelId::from_raw(1),
            PropertyId::from_raw(7),
            vec![interval_a.clone(), interval_b.clone()],
        ))
        .expect("range disjunction union with dedup");

        let ids: Vec<u32> = result
            .into_iter()
            .map(|s| match s {
                VectorSubject::Vertex { vertex_id, .. } => vertex_id,
            })
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&100));
        assert!(ids.contains(&101));
        assert!(ids.contains(&102));
        assert_eq!(
            client.call_count(),
            1,
            "overlapping intervals must be merged into one lookup"
        );
    }

    #[test]
    fn merge_encoded_intervals_drops_empty_and_sorts_unordered() {
        let intervals = vec![
            (vec![20u8], vec![10u8]), // empty, dropped
            (vec![30u8], vec![40u8]),
            (vec![5u8], vec![15u8]),
        ];
        assert_eq!(
            merge_encoded_intervals(intervals),
            vec![(vec![5u8], vec![15u8]), (vec![30u8], vec![40u8])]
        );
    }

    #[test]
    fn merge_encoded_intervals_merges_overlapping_touching_and_contained() {
        let intervals = vec![
            (vec![10u8], vec![20u8]),
            (vec![15u8], vec![25u8]), // overlaps
            (vec![22u8], vec![24u8]), // contained
            (vec![25u8], vec![30u8]), // touches at boundary and merges
            (vec![50u8], vec![60u8]), // disjoint
        ];
        assert_eq!(
            merge_encoded_intervals(intervals),
            vec![(vec![10u8], vec![30u8]), (vec![50u8], vec![60u8]),]
        );
    }

    #[test]
    fn merge_encoded_intervals_dedupes_duplicates() {
        let intervals = vec![
            (vec![1u8], vec![2u8]),
            (vec![1u8], vec![2u8]),
            (vec![3u8], vec![4u8]),
        ];
        assert_eq!(
            merge_encoded_intervals(intervals),
            vec![(vec![1u8], vec![2u8]), (vec![3u8], vec![4u8])]
        );
    }

    #[test]
    fn merge_encoded_intervals_returns_empty_when_all_empty() {
        assert!(
            merge_encoded_intervals(vec![(vec![5u8], vec![5u8]), (vec![10u8], vec![10u8]),])
                .is_empty()
        );
    }

    #[test]
    fn collect_range_disjunction_rejects_4097th_subject() {
        let mut page = Vec::new();
        for i in 0..=MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
            page.push(PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: i as u32,
            });
        }
        let interval = (vec![1u8], vec![2u8]);
        let client =
            MockDisjunctionClient::with_interval_pages(std::collections::BTreeMap::from([(
                interval.clone(),
                vec![PostingHitPage {
                    hits: page,
                    next: None,
                    done: true,
                }],
            )]));

        let err = pollster::block_on(collect_bounded_candidates_range_disjunction(
            vec![client],
            VertexLabelId::from_raw(1),
            PropertyId::from_raw(7),
            vec![interval],
        ))
        .expect_err("4097th distinct subject must fail");
        assert!(
            err.to_string().contains("produced more than"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn collect_disjunction_unions_values_and_dedupes_across_arms() {
        let value_a = vec![1u8];
        let value_b = vec![2u8];
        let shared = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 100,
        };
        let only_a = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 101,
        };
        let only_b = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 102,
        };
        let client = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([
            (
                value_a.clone(),
                vec![PostingHitPage {
                    hits: vec![shared, only_a],
                    next: None,
                    done: true,
                }],
            ),
            (
                value_b.clone(),
                vec![PostingHitPage {
                    hits: vec![shared, only_b],
                    next: None,
                    done: true,
                }],
            ),
        ]));

        let result = pollster::block_on(collect_bounded_candidates_equal_disjunction(
            vec![client],
            VertexLabelId::from_raw(1),
            vec![
                (PropertyId::from_raw(7), value_a),
                (PropertyId::from_raw(7), value_b),
            ],
        ))
        .expect("union with dedup");

        let ids: Vec<u32> = result
            .into_iter()
            .map(|s| match s {
                VectorSubject::Vertex { vertex_id, .. } => vertex_id,
            })
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&100));
        assert!(ids.contains(&101));
        assert!(ids.contains(&102));
    }

    #[test]
    fn collect_disjunction_continues_past_empty_non_terminal_page() {
        let value = vec![1u8];
        let cursor = PropertyPostingCursor {
            value: value.clone(),
            shard_id: ShardId::new(0),
            vertex_id: 0,
        };
        let client = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([(
            value.clone(),
            vec![
                PostingHitPage {
                    hits: vec![],
                    next: Some(cursor.clone()),
                    done: false,
                },
                PostingHitPage {
                    hits: vec![PostingHit {
                        shard_id: ShardId::new(0),
                        vertex_id: 42,
                    }],
                    next: None,
                    done: true,
                },
            ],
        )]));

        let result = pollster::block_on(collect_bounded_candidates_equal_disjunction(
            vec![client],
            VertexLabelId::from_raw(1),
            vec![(PropertyId::from_raw(7), value)],
        ))
        .expect("empty non-terminal page must not stop the scan");

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0],
            VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 42,
            }
        );
    }

    #[test]
    fn collect_disjunction_rejects_4097th_distinct_subject() {
        let value = vec![1u8];
        let mut hits = Vec::new();
        for i in 0..=MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
            hits.push(PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: i as u32,
            });
        }
        let client = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([(
            value.clone(),
            vec![PostingHitPage {
                hits,
                next: None,
                done: true,
            }],
        )]));

        let err = pollster::block_on(collect_bounded_candidates_equal_disjunction(
            vec![client],
            VertexLabelId::from_raw(1),
            vec![(PropertyId::from_raw(7), value)],
        ))
        .expect_err("4097th distinct subject must fail");
        assert!(
            err.to_string()
                .contains("produced more than 4096 candidates"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn collect_disjunction_label_filter_happens_before_counting() {
        let value = vec![1u8];
        let mut hits = Vec::new();
        for i in 0..=MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
            hits.push(PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: i as u32,
            });
        }

        fn keep_first_4096(hits: Vec<PostingHit>) -> Vec<PostingHit> {
            hits.into_iter()
                .take(MAX_VECTOR_SEARCH_FILTER_CANDIDATES)
                .collect()
        }

        let client = MockDisjunctionClient {
            calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            pages_by_value: std::collections::BTreeMap::from([(
                value.clone(),
                vec![PostingHitPage {
                    hits,
                    next: None,
                    done: true,
                }],
            )]),
            pages_by_interval: std::collections::BTreeMap::new(),
            next_index: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::BTreeMap::new(),
            )),
            label_filter: keep_first_4096,
        };

        let result = pollster::block_on(collect_bounded_candidates_equal_disjunction(
            vec![client],
            VertexLabelId::from_raw(1),
            vec![(PropertyId::from_raw(7), value)],
        ))
        .expect("label filtering must keep the candidate count at 4096");
        assert_eq!(result.len(), MAX_VECTOR_SEARCH_FILTER_CANDIDATES);
    }

    #[test]
    fn collect_disjunction_deduplicates_runtime_equal_values_before_lookup() {
        let value = vec![1u8];
        let hit = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 55,
        };
        let client = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([(
            value.clone(),
            vec![PostingHitPage {
                hits: vec![hit],
                next: None,
                done: true,
            }],
        )]));

        let result = pollster::block_on(collect_bounded_candidates_equal_disjunction(
            vec![client.clone()],
            VertexLabelId::from_raw(1),
            vec![
                (PropertyId::from_raw(7), value.clone()),
                (PropertyId::from_raw(7), value.clone()),
                (PropertyId::from_raw(7), value.clone()),
            ],
        ))
        .expect("duplicate runtime values must be unioned once");

        assert_eq!(result.len(), 1);
        assert_eq!(
            client.call_count(),
            1,
            "only one lookup per distinct encoded value"
        );
    }

    #[test]
    fn collect_disjunction_resets_cursor_per_source_for_same_value() {
        let value = vec![7u8];

        // Source A exposes one subject.
        let client_a = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([(
            value.clone(),
            vec![PostingHitPage {
                hits: vec![PostingHit {
                    shard_id: ShardId::new(0),
                    vertex_id: 10,
                }],
                next: None,
                done: true,
            }],
        )]));

        // Source B exposes a different subject, and starts its own walk from `None` even though
        // the same encoded value is being queried.
        let client_b = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([(
            value.clone(),
            vec![PostingHitPage {
                hits: vec![PostingHit {
                    shard_id: ShardId::new(1),
                    vertex_id: 20,
                }],
                next: None,
                done: true,
            }],
        )]));

        let result = pollster::block_on(collect_bounded_candidates_equal_disjunction(
            vec![client_a.clone(), client_b.clone()],
            VertexLabelId::from_raw(1),
            vec![(PropertyId::from_raw(7), value)],
        ))
        .expect("per-source cursor must be independent");

        assert_eq!(result.len(), 2);
        let ids: std::collections::HashSet<_> = result
            .iter()
            .map(|s| match s {
                VectorSubject::Vertex { vertex_id, .. } => *vertex_id,
            })
            .collect();
        assert!(ids.contains(&10));
        assert!(ids.contains(&20));

        // Both clients should have received exactly one call, and the second call must start
        // from `None` even though the first source already completed the same value.
        let a_calls = client_a.calls.lock().unwrap();
        let b_calls = client_b.calls.lock().unwrap();
        assert_eq!(a_calls.len(), 1, "source A sees one lookup");
        assert_eq!(b_calls.len(), 1, "source B sees one lookup");
        assert_eq!(a_calls[0].1, None, "source A starts from None");
        assert_eq!(
            b_calls[0].1, None,
            "source B starts from None independently"
        );
    }

    #[test]
    fn collect_disjunction_source_order_independence() {
        let value_a = vec![1u8];
        let value_b = vec![2u8];
        let hit_a = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 10,
        };
        let hit_b = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 20,
        };

        let client_a = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([(
            value_a.clone(),
            vec![PostingHitPage {
                hits: vec![hit_a],
                next: None,
                done: true,
            }],
        )]));
        let client_b = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([(
            value_b.clone(),
            vec![PostingHitPage {
                hits: vec![hit_b],
                next: None,
                done: true,
            }],
        )]));

        let result_ab = pollster::block_on(collect_bounded_candidates_equal_disjunction(
            vec![client_a.clone(), client_b.clone()],
            VertexLabelId::from_raw(1),
            vec![
                (PropertyId::from_raw(7), value_a.clone()),
                (PropertyId::from_raw(7), value_b.clone()),
            ],
        ))
        .expect("order AB");

        // Use fresh mock clients for the second run so that per-source paging state is not
        // carried over from the first run.
        let client_c = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([(
            value_a.clone(),
            vec![PostingHitPage {
                hits: vec![hit_a],
                next: None,
                done: true,
            }],
        )]));
        let client_d = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([(
            value_b.clone(),
            vec![PostingHitPage {
                hits: vec![hit_b],
                next: None,
                done: true,
            }],
        )]));

        let result_ba = pollster::block_on(collect_bounded_candidates_equal_disjunction(
            vec![client_d, client_c],
            VertexLabelId::from_raw(1),
            vec![
                (PropertyId::from_raw(7), value_b.clone()),
                (PropertyId::from_raw(7), value_a.clone()),
            ],
        ))
        .expect("order BA");

        let set_ab: std::collections::HashSet<_> = result_ab.into_iter().collect();
        let set_ba: std::collections::HashSet<_> = result_ba.into_iter().collect();
        assert_eq!(
            set_ab, set_ba,
            "union result set must be independent of source order"
        );
    }

    #[test]
    fn collect_disjunction_same_value_different_property_generates_two_sources() {
        let value = vec![1u8];
        let hit = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 10,
        };

        let client = MockDisjunctionClient::with_pages(std::collections::BTreeMap::from([(
            value.clone(),
            vec![PostingHitPage {
                hits: vec![hit],
                next: None,
                done: true,
            }],
        )]));

        let result = pollster::block_on(collect_bounded_candidates_equal_disjunction(
            vec![client.clone()],
            VertexLabelId::from_raw(1),
            vec![
                (PropertyId::from_raw(7), value.clone()),
                (PropertyId::from_raw(8), value.clone()),
            ],
        ))
        .expect("same value across two properties");

        assert_eq!(
            result.len(),
            1,
            "identical hit from two properties dedupes to one subject"
        );
        // The single index target must be queried once for each distinct `(property_id, encoded_value)`
        // source, even though the encoded value is identical.
        assert_eq!(
            client.call_count(),
            2,
            "two distinct sources must each be queried once"
        );
    }

    #[test]
    fn collect_range_disjunction_label_filter_happens_before_counting() {
        let interval = (vec![1u8], vec![2u8]);
        let included = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 1,
        };
        let excluded = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 2,
        };
        let mut client =
            MockDisjunctionClient::with_interval_pages(std::collections::BTreeMap::from([(
                interval.clone(),
                vec![PostingHitPage {
                    hits: vec![included, excluded],
                    next: None,
                    done: true,
                }],
            )]));
        client.label_filter = |hits| hits.into_iter().filter(|h| h.vertex_id != 2).collect();

        let result = pollster::block_on(collect_bounded_candidates_range_disjunction(
            vec![client],
            VertexLabelId::from_raw(1),
            PropertyId::from_raw(7),
            vec![interval],
        ))
        .expect("label filter before count");

        assert_eq!(result.len(), 1);
        assert!(matches!(
            result[0],
            VectorSubject::Vertex { vertex_id: 1, .. }
        ));
    }

    #[test]
    fn collect_range_disjunction_endpoint_strictness_merges_touching_intervals_to_one_lookup() {
        // [0,10) and [10,20) are adjacent in encoded half-open space. They merge to [0,20),
        // proving the boundary is preserved while the lookup count drops to one.
        let interval_a = (vec![0u8], vec![10u8]);
        let interval_b = (vec![10u8], vec![20u8]);
        let merged = (vec![0u8], vec![20u8]);
        let hit_a = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 1,
        };
        let hit_b = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 2,
        };
        let client =
            MockDisjunctionClient::with_interval_pages(std::collections::BTreeMap::from([(
                merged.clone(),
                vec![PostingHitPage {
                    hits: vec![hit_a, hit_b],
                    next: None,
                    done: true,
                }],
            )]));

        let result = pollster::block_on(collect_bounded_candidates_range_disjunction(
            vec![client.clone()],
            VertexLabelId::from_raw(1),
            PropertyId::from_raw(7),
            vec![interval_a.clone(), interval_b.clone()],
        ))
        .expect("touching interval merge");

        assert_eq!(result.len(), 2);
        assert_eq!(
            client.call_count(),
            1,
            "touching half-open intervals must merge into a single lookup"
        );
    }

    #[test]
    fn collect_range_disjunction_mixed_width_intervals_normalize_correctly() {
        // Canonical numeric keys are normalized decimal encodings, not raw width-prefixed values.
        // Intervals from different scalar magnitudes can have different byte lengths and remain
        // disjoint in encoded order; the merge helper must operate on those canonical encoded bounds.
        let int32_le_negative = gleaph_gql::value_index_key::numeric_range_bounds(
            &Value::Int32(-1),
            gleaph_gql::ast::CmpOp::Le,
        )
        .expect("Int32 negative upper bound");
        let int64_ge_zero = gleaph_gql::value_index_key::numeric_range_bounds(
            &Value::Int64(0),
            gleaph_gql::ast::CmpOp::Ge,
        )
        .expect("Int64 zero lower bound");

        // The negative interval ends strictly before the non-negative interval starts.
        assert!(
            int32_le_negative.1.as_slice() < int64_ge_zero.0.as_slice(),
            "negative encoded upper bound must precede non-negative encoded lower bound"
        );

        let hit_negative = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 1,
        };
        let hit_non_negative = PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 2,
        };
        let client =
            MockDisjunctionClient::with_interval_pages(std::collections::BTreeMap::from([
                (
                    int32_le_negative.clone(),
                    vec![PostingHitPage {
                        hits: vec![hit_negative],
                        next: None,
                        done: true,
                    }],
                ),
                (
                    int64_ge_zero.clone(),
                    vec![PostingHitPage {
                        hits: vec![hit_non_negative],
                        next: None,
                        done: true,
                    }],
                ),
            ]));

        let result = pollster::block_on(collect_bounded_candidates_range_disjunction(
            vec![client.clone()],
            VertexLabelId::from_raw(1),
            PropertyId::from_raw(7),
            vec![int32_le_negative, int64_ge_zero],
        ))
        .expect("mixed-width interval normalization");

        assert_eq!(result.len(), 2);
        assert_eq!(
            client.call_count(),
            2,
            "canonical numeric intervals of different encoded lengths must not merge across a gap"
        );
    }

    #[test]
    fn collect_range_disjunction_accepts_exactly_4096_subjects() {
        let mut hits = Vec::with_capacity(MAX_VECTOR_SEARCH_FILTER_CANDIDATES);
        for i in 0..MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
            hits.push(PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: i as u32,
            });
        }
        let interval = (vec![1u8], vec![2u8]);
        let client =
            MockDisjunctionClient::with_interval_pages(std::collections::BTreeMap::from([(
                interval.clone(),
                vec![PostingHitPage {
                    hits,
                    next: None,
                    done: true,
                }],
            )]));

        let result = pollster::block_on(collect_bounded_candidates_range_disjunction(
            vec![client],
            VertexLabelId::from_raw(1),
            PropertyId::from_raw(7),
            vec![interval],
        ))
        .expect("exactly 4096 subjects must be accepted");
        assert_eq!(result.len(), MAX_VECTOR_SEARCH_FILTER_CANDIDATES);
    }
}
