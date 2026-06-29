//! Router-side lowering of GQL `SEARCH ... IN (VECTOR INDEX ... FOR ... LIMIT ...)`
//! (ADR 0034 Slices 3, 5, 6 and 7).
//!
//! This module is the execution boundary between the provider-neutral GQL planner
//! (`PlanOp::Search`) and the Router-owned vector-index catalog / canister dispatch. It supports
//! a narrow leading `NodeScan + Search` shape, one top-level non-leading `SEARCH` after a bound
//! vertex, and one or two `AND`-connected same-binding equality `SEARCH ... WHERE` predicates
//! for both positions (Slices 6, 7 and 8). All unsupported shapes are rejected with explicit
//! `InvalidArgument` errors. For a leading search it dispatches the remaining graph-tail plan
//! from row-shaped vector-search seeds; for a non-leading search it attaches an explicit
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
    IndexEqualSpec, LookupIntersectionPageRequest, MAX_INDEX_VALUE_KEY_BYTES,
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

/// ADR 0034 Slice 8: one or two same-binding equality conjuncts.
#[derive(Debug)]
struct SearchFilter {
    arms: Vec<SearchFilterArm>,
}

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

/// Extract an accepted equality predicate from the planner-validated filter expression.
/// The planner already guaranteed exactly one side is `binding.property` and the other is a
/// literal or parameter, so this function only normalizes which side is which.
fn extract_search_filter(
    binding: &str,
    expr: &gleaph_gql::ast::Expr,
) -> Result<SearchFilter, RouterError> {
    let arms = flatten_search_filter_arms(binding, expr)?;
    if arms.is_empty() || arms.len() > 2 {
        return Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE supports one or two equality conjuncts in this slice".into(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for (prop, _) in &arms {
        if !seen.insert(prop.clone()) {
            return Err(RouterError::InvalidArgument(
                "SEARCH ... WHERE equality conjuncts must refer to distinct properties".into(),
            ));
        }
    }
    Ok(SearchFilter {
        arms: arms
            .into_iter()
            .map(|(property_name, value_expr)| SearchFilterArm {
                property_name,
                value_expr,
            })
            .collect(),
    })
}

fn flatten_search_filter_arms(
    binding: &str,
    expr: &gleaph_gql::ast::Expr,
) -> Result<Vec<(String, gleaph_gql::ast::Expr)>, RouterError> {
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

    fn split_eq(
        expr: &gleaph_gql::ast::Expr,
        binding: &str,
    ) -> Result<(String, gleaph_gql::ast::Expr), RouterError> {
        let gleaph_gql::ast::ExprKind::Compare { left, op, right } = &expr.kind else {
            return Err(RouterError::InvalidArgument(
                "SEARCH ... WHERE must be an equality comparison".into(),
            ));
        };
        if *op != gleaph_gql::ast::CmpOp::Eq {
            return Err(RouterError::InvalidArgument(
                "SEARCH ... WHERE only supports equality (=)".into(),
            ));
        }
        if let Some(property) = is_bound_property(left, binding)
            && is_literal_or_parameter(right)
        {
            return Ok((property, *right.clone()));
        }
        if let Some(property) = is_bound_property(right, binding)
            && is_literal_or_parameter(left)
        {
            return Ok((property, *left.clone()));
        }
        Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE must compare a property of the searched binding with a literal or parameter".into(),
        ))
    }

    fn collect_and_leaves(expr: &gleaph_gql::ast::Expr, out: &mut Vec<gleaph_gql::ast::Expr>) {
        match &expr.kind {
            gleaph_gql::ast::ExprKind::And(left, right) => {
                collect_and_leaves(left, out);
                collect_and_leaves(right, out);
            }
            _ => out.push(expr.clone()),
        }
    }

    let mut leaves = Vec::new();
    collect_and_leaves(expr, &mut leaves);
    let mut out = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        out.push(split_eq(leaf, binding)?);
    }
    Ok(out)
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

/// Resolve a bounded set of vector-search candidate subjects for one or two same-binding
/// property equality predicates.
///
/// Steps:
/// 1. Resolve every property name to a property id and verify an active vertex equality index for
///    the exact `(label_id, property_id)` tuple.
/// 2. Resolve every literal/parameter value and encode it with the shared property-index key encoder.
/// 3. Validate each encoded key size against `MAX_INDEX_VALUE_KEY_BYTES`.
/// 4. For one arm, page through the matching Property Index equality bucket. For two arms, page
///    through the server-side equality intersection. In both cases stop at the 4097th distinct
///    label-qualified `(shard_id, vertex_id)` subject and return an explicit error.
async fn resolve_filtered_candidates(
    graph_id: GraphId,
    store: &RouterStore,
    label_id: VertexLabelId,
    binding: &str,
    filter: &SearchFilter,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<VectorSubject>, RouterError> {
    let mut resolved_arms = Vec::with_capacity(filter.arms.len());
    for arm in &filter.arms {
        let property_id = resolve_search_property_id(graph_id, store, binding, &arm.property_name)?;
        if !indexed_catalog::has_exact_vertex_index(graph_id, label_id, property_id) {
            return Err(RouterError::InvalidArgument(format!(
                "SEARCH ... WHERE requires an active vertex equality index for label {} property {}",
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
        resolved_arms.push((property_id, encoded));
    }

    match resolved_arms.len() {
        1 => {
            let (property_id, encoded) = resolved_arms.into_iter().next().unwrap();
            collect_bounded_candidates_equal(graph_id, store, label_id, property_id, encoded).await
        }
        2 => {
            let specs = resolved_arms
                .into_iter()
                .map(|(property_id, value)| IndexEqualSpec::vertex(property_id.raw(), value))
                .collect();
            collect_bounded_candidates_intersection(graph_id, store, label_id, specs).await
        }
        _ => Err(RouterError::InvalidArgument(
            "SEARCH ... WHERE supports one or two equality conjuncts in this slice".into(),
        )),
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
        assert_eq!(f.arms.len(), 1);
        assert_eq!(f.arms[0].property_name, "category");
        assert!(matches!(f.arms[0].value_expr.kind, ExprKind::Literal(_)));
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
                .contains("requires an active vertex equality index"),
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
                .contains("requires an active vertex equality index"),
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
    fn extract_search_filter_accepts_two_arm_conjunction() {
        let left = filter_eq_expr("category", Value::Text("doc".into()));
        let right = filter_eq_expr("tenant_id", Value::Int64(7));
        let f = extract_search_filter("d", &filter_and_expr(left, right)).expect("two arms");
        assert_eq!(f.arms.len(), 2);
        let props: Vec<_> = f.arms.iter().map(|a| a.property_name.as_str()).collect();
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
                .contains("requires an active vertex equality index"),
            "unexpected error: {err}"
        );
    }

    // --- ADR 0034 Slice 8: bounded candidate collector regression tests ---

    fn collect_candidates_with_pages(
        graph_id: GraphId,
        store: &RouterStore,
        label_id: VertexLabelId,
        pages: Vec<Vec<Vec<PostingHit>>>,
        filtered_hits: Vec<Vec<Vec<PostingHit>>>,
    ) -> Result<Vec<VectorSubject>, RouterError> {
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let filter_count = call_count.clone();
        let mut page_iters: Vec<_> = pages.into_iter().map(|p| p.into_iter()).collect();
        let mut filter_iters: Vec<_> = filtered_hits.into_iter().map(|p| p.into_iter()).collect();
        pollster::block_on(collect_bounded_candidates_inner(
            graph_id,
            store,
            label_id,
            |_client, _after| {
                let idx = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let hits = page_iters[idx].next().unwrap_or_default();
                let done = page_iters[idx].len() == 0;
                std::future::ready(Ok(PostingHitPage {
                    hits,
                    next: None,
                    done,
                }))
            },
            |_client, _label_id, _hits| {
                let idx = filter_count.load(std::sync::atomic::Ordering::SeqCst) - 1;
                let filtered = filter_iters[idx].next().unwrap_or_default();
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
}
