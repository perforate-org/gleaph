use crate::ast::{
    CmpOp, Direction, Expr, MatchClause, NodePattern, PatternElement, QueryStmt, Statement,
};
use crate::plan::{
    ConditionalCmpOp, ConditionalScanCandidate, ConditionalScanCandidateReason,
    ConditionalScanInfo, ConditionalScanReasoning, PhysicalPlan, PlanAnnotations,
    PlanCardinalityReason, PlanOp,
};
use crate::semantic::{
    NarrowingFact, PropertySchema, SemanticAnalysis, SemanticConstraint,
    analyze_statement_structure,
};
use crate::stats::{
    COST_AGGREGATE_PER_ROW, COST_EXPAND_MULTIPLIER, COST_FILTER_PER_ROW, COST_INDEX_SEEK_FRACTION,
    COST_LIMIT_PER_ROW, COST_PROJECT_PER_ROW, COST_SCAN_PER_ROW, COST_SHORTEST_PER_ROW,
    COST_SORT_NLOGN, CostEstimate, TableStats,
};
use crate::type_check::{WarningKind, type_check_statement_with_schema};
use gleaph_types::GleaphError;
use std::collections::BTreeSet;

/// Converts a validated [`Statement`] into a [`PhysicalPlan`].
///
/// The planner selects an anchor node for the initial scan using a simple
/// cost heuristic (property-equality > label-only > full-scan) and emits the
/// corresponding sequence of [`PlanOp`]s.
///
/// Mutation statements (`CREATE`, `DELETE`) are not yet supported and will
/// return [`GleaphError::UnsupportedFeature`].
pub fn build_plan(stmt: &Statement) -> Result<PhysicalPlan, GleaphError> {
    build_plan_with_stats(stmt, None)
}

/// Builds a physical plan and returns its stable human-readable explanation lines.
pub fn explain_plan(stmt: &Statement) -> Result<Vec<String>, GleaphError> {
    explain_plan_with_stats(stmt, None)
}

/// Like [`explain_plan`] but allows injecting planner statistics for heuristics.
pub fn explain_plan_with_stats(
    stmt: &Statement,
    stats: Option<&TableStats>,
) -> Result<Vec<String>, GleaphError> {
    build_plan_with_stats(stmt, stats).map(|plan| plan.explain_lines())
}

/// Like [`build_plan`] but allows injecting planner statistics for heuristics.
pub fn build_plan_with_stats(
    stmt: &Statement,
    stats: Option<&TableStats>,
) -> Result<PhysicalPlan, GleaphError> {
    build_plan_internal(stmt, stats, None, true)
}

/// Like [`build_plan_with_stats`] but omits explain/debug-only annotations that
/// are not needed by the executor hot path.
pub fn build_runtime_plan_with_stats(
    stmt: &Statement,
    stats: Option<&TableStats>,
) -> Result<PhysicalPlan, GleaphError> {
    build_plan_internal(stmt, stats, None, false)
}

/// Internal planning entry point that optionally takes a schema for
/// endpoint-driven anchor selection.
fn build_plan_internal(
    stmt: &Statement,
    stats: Option<&TableStats>,
    schema: Option<&dyn PropertySchema>,
    include_debug_annotations: bool,
) -> Result<PhysicalPlan, GleaphError> {
    match stmt {
        Statement::Query(q) => {
            let semantic = analyze_statement_structure(stmt);
            let semantic_where_property_accesses =
                semantic_where_property_accesses(&semantic.constraints);
            let semantic_indexable_vertex_properties = stats.and_then(|s| {
                semantic_indexable_properties(
                    semantic_where_property_accesses.as_deref(),
                    &s.indexed_vertex_properties,
                )
            });
            let semantic_range_indexable_vertex_properties = stats.and_then(|s| {
                semantic_indexable_properties(
                    semantic_where_property_accesses.as_deref(),
                    &s.range_indexed_vertex_properties,
                )
            });
            let semantic_indexable_edge_properties = stats.and_then(|s| {
                semantic_indexable_properties(
                    semantic_where_property_accesses.as_deref(),
                    &s.indexed_edge_properties,
                )
            });
            let semantic_scan_reason_value = include_debug_annotations
                .then(|| {
                    semantic_scan_reason(
                        semantic_where_property_accesses.as_deref(),
                        semantic_indexable_vertex_properties.as_deref(),
                        semantic_range_indexable_vertex_properties.as_deref(),
                        semantic_indexable_edge_properties.as_deref(),
                    )
                })
                .flatten();
            let semantic_aggregates = semantic_aggregates(&semantic.constraints);
            let has_aggregate = semantic_aggregates.is_some();
            let first_match =
                q.match_clauses.first().map(|m| &m.pattern).ok_or_else(|| {
                    GleaphError::ValidationError("MATCH clause is required".into())
                })?;
            let (chosen_anchor, anchor_source) =
                choose_anchor(q, stats, &semantic.narrowing_facts, &semantic, schema);
            let first_entry = q.match_clauses.first().expect("validated non-empty match");
            let join_order = include_debug_annotations
                .then(|| greedy_left_deep_join_order(first_match, stats, schema));
            let filter_stages = include_debug_annotations
                .then(|| plan_filter_pushdown_stages(q.where_clause.as_ref(), first_match));
            let filter_stages_vec = filter_stages.clone().unwrap_or_default();
            // Detect IS NULL OR patterns first to collect parameter names that should
            // NOT be used for direct IndexScan (they need ConditionalIndexScan instead).
            // Prefer semantic facts; fall back to AST walk for uncaptured patterns.
            let optional_filters =
                optional_filters_from_semantic_or_ast(&semantic, q.where_clause.as_ref());
            let conditional_param_names: std::collections::HashSet<&str> = optional_filters
                .iter()
                .map(|of| of.param_name.as_str())
                .collect();

            let index_scan_result = should_use_index_scan(
                q,
                &chosen_anchor,
                stats,
                &conditional_param_names,
                semantic_where_property_accesses.as_deref(),
            );
            let use_index_scan = index_scan_result.is_some();
            let range_index_scan_op = index_scan_result.and_then(|op| {
                if op == ConditionalCmpOp::Eq {
                    None
                } else {
                    Some(op)
                }
            });
            let use_edge_index_scan = !use_index_scan
                && should_use_edge_index_scan(
                    q,
                    stats,
                    semantic_where_property_accesses.as_deref(),
                );
            let conditional_scan_candidates =
                if !use_index_scan && !use_edge_index_scan && stats.is_some() {
                    let st = stats.unwrap();
                    let mut candidates: Vec<_> = optional_filters
                        .into_iter()
                        .filter(|of| {
                            let var_in_pattern = q.match_clauses.first().is_some_and(|m| {
                                m.pattern.start.var.as_deref() == Some(of.variable.as_str())
                                    || m.pattern.hops().any(|c| {
                                        c.node.var.as_deref() == Some(of.variable.as_str())
                                    })
                            });
                            if !var_in_pattern {
                                return false;
                            }
                            match of.cmp_op {
                                ConditionalCmpOp::Eq => {
                                    st.indexed_vertex_properties.contains(&of.property)
                                }
                                _ => st.range_indexed_vertex_properties.contains(&of.property),
                            }
                        })
                        .collect();
                    // Phase 11: Sort candidates by selectivity (lowest first).
                    // Range operators get a slight penalty (×2) since they typically
                    // match more rows than equality.
                    candidates.sort_by(|a, b| {
                        let sel = |of: &OptionalFilter| -> f64 {
                            let base = st
                                .property_selectivity
                                .get(&format!("vertex:{}", of.property))
                                .copied()
                                .unwrap_or(0.5);
                            if matches!(of.cmp_op, ConditionalCmpOp::Eq) {
                                base
                            } else {
                                base * 2.0 // range penalty
                            }
                        };
                        sel(a)
                            .partial_cmp(&sel(b))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    candidates
                } else {
                    Vec::new()
                };
            let semantic_conditional_scan_properties = (include_debug_annotations
                && !conditional_scan_candidates.is_empty())
            .then(|| {
                let props: BTreeSet<String> = conditional_scan_candidates
                    .iter()
                    .map(|c| c.property.clone())
                    .collect();
                props.into_iter().collect()
            });
            let conditional_scan_reasoning =
                (include_debug_annotations && !conditional_scan_candidates.is_empty()).then(|| {
                    ConditionalScanReasoning {
                    semantic_properties: semantic_conditional_scan_properties
                        .clone()
                        .unwrap_or_default(),
                    candidate_reasons: conditional_scan_candidates
                        .iter()
                        .map(|candidate| ConditionalScanCandidateReason {
                            property: candidate.property.clone(),
                            variable: candidate.variable.clone(),
                            cmp_op: candidate.cmp_op,
                            selectivity_hint: stats.and_then(|s| {
                                s.property_selectivity
                                    .get(&format!("vertex:{}", candidate.property))
                                    .copied()
                            }),
                        })
                        .collect(),
                }
                });
            let mut ops = vec![if use_index_scan {
                PlanOp::IndexScan
            } else if !conditional_scan_candidates.is_empty() {
                PlanOp::ConditionalIndexScan
            } else if use_edge_index_scan {
                PlanOp::EdgeIndexScan
            } else {
                PlanOp::NodeScan
            }];
            for _ in 0..filter_stages_vec.iter().filter(|&&s| s == 0).count() {
                ops.push(PlanOp::PropertyFilter);
            }
            for chain in first_match.hops() {
                if first_entry.shortest {
                    ops.push(PlanOp::ShortestPath);
                } else {
                    ops.push(PlanOp::Expand);
                    if chain_has_literal_edge_props(chain) {
                        ops.push(PlanOp::FilterEdge);
                    }
                }
                let stage = ops
                    .iter()
                    .filter(|op| matches!(op, PlanOp::Expand | PlanOp::ShortestPath))
                    .count();
                for _ in 0..filter_stages_vec.iter().filter(|&&s| s == stage).count() {
                    ops.push(PlanOp::PropertyFilter);
                }
            }
            for _ in 0..filter_stages_vec
                .iter()
                .filter(|&&s| s > first_match.elements.len())
                .count()
            {
                ops.push(PlanOp::PropertyFilter);
            }
            if has_aggregate {
                ops.push(PlanOp::Aggregate);
            }
            // LIMIT pushdown is safe when there is no ORDER BY, no aggregation, no DISTINCT,
            // and all WITH clauses are pure projections (no WHERE, DISTINCT, ORDER BY, MATCH,
            // aggregation, or inner LIMIT/OFFSET that would change cardinality).
            let limit_pushdown = q.limit.is_some()
                && q.order_by.is_none()
                && !has_aggregate
                && !q.return_clause.distinct
                && (q.with_clauses.is_empty()
                    || with_clauses_are_pure_projections(&q.with_clauses));
            if limit_pushdown {
                ops.push(PlanOp::Limit);
            }
            ops.push(PlanOp::Project);
            if q.order_by.is_some() {
                ops.push(PlanOp::Sort);
            }
            if q.limit.is_some() && !limit_pushdown {
                ops.push(PlanOp::Limit);
            }
            let cost = estimate_cost(&ops, q, stats, semantic_where_property_accesses.as_deref());
            let estimated_cardinality_source = include_debug_annotations.then(|| {
                if use_index_scan {
                    index_scan_source(q, &chosen_anchor).unwrap_or_else(|| anchor_source.clone())
                } else if use_edge_index_scan {
                    edge_index_scan_source(q).unwrap_or_else(|| anchor_source.clone())
                } else {
                    anchor_source.clone()
                }
            });
            let estimated_cardinality_reason = include_debug_annotations.then(|| {
                estimated_cardinality_reason(
                    q,
                    use_index_scan,
                    use_edge_index_scan,
                    range_index_scan_op,
                    &anchor_source,
                )
            }).flatten();
            let shortest_reverse_anchor = (include_debug_annotations && first_entry.shortest)
                .then(|| detect_shortest_reverse_anchor(first_match, stats))
                .flatten();
            let match_clause_order = if q.match_clauses.len() > 1 {
                Some(multi_match_clause_order(&q.match_clauses, stats))
            } else {
                None
            };
            Ok(PhysicalPlan {
                ops,
                annotations: PlanAnnotations {
                    chosen_anchor,
                    estimated_cardinality_source,
                    estimated_cardinality_reason,
                    estimated_rows: Some(cost.estimated_rows),
                    estimated_instructions: Some(cost.estimated_instructions),
                    join_order,
                    filter_pushdown_stages: filter_stages.filter(|stages| !stages.is_empty()),
                    limit_pushdown_applied: include_debug_annotations && limit_pushdown,
                    shortest_reverse_anchor,
                    match_clause_order,
                    conditional_scan: if conditional_scan_candidates.is_empty() {
                        None
                    } else {
                        Some(ConditionalScanInfo {
                            candidates: conditional_scan_candidates
                                .into_iter()
                                .map(|of| ConditionalScanCandidate {
                                    param_name: of.param_name,
                                    property: of.property,
                                    variable: of.variable,
                                    cmp_op: of.cmp_op,
                                })
                                .collect(),
                            reasoning: conditional_scan_reasoning,
                        })
                    },
                    index_scan_cmp_op: range_index_scan_op,
                    semantic_property_accesses: include_debug_annotations
                        .then(|| semantic_property_accesses(&semantic.constraints))
                        .flatten(),
                    semantic_where_property_accesses: include_debug_annotations
                        .then_some(semantic_where_property_accesses.clone())
                        .flatten(),
                    semantic_indexable_vertex_properties: include_debug_annotations
                        .then_some(semantic_indexable_vertex_properties.clone())
                        .flatten(),
                    semantic_range_indexable_vertex_properties: include_debug_annotations
                        .then_some(semantic_range_indexable_vertex_properties.clone())
                        .flatten(),
                    semantic_indexable_edge_properties: include_debug_annotations
                        .then_some(semantic_indexable_edge_properties.clone())
                        .flatten(),
                    semantic_scan_reason: semantic_scan_reason_value,
                    semantic_conditional_scan_properties,
                    semantic_aggregates: include_debug_annotations
                        .then_some(semantic_aggregates)
                        .flatten(),
                    narrowing_facts: (include_debug_annotations && !semantic.narrowing_facts.is_empty())
                        .then(|| semantic.narrowing_facts.clone()),
                    type_diagnostics: None,
                    statically_contradictory: false,
                },
                query: Some(q.clone()),
            })
        }
        Statement::Compound { .. } => Err(GleaphError::UnsupportedFeature(
            "planner currently does not support compound query statements".into(),
        )),
        _ => Err(GleaphError::UnsupportedFeature(
            "planner currently supports query statements only".into(),
        )),
    }
}

/// Like [`build_plan_with_stats`] but also runs constraint-based type checking
/// and attaches type diagnostics and contradiction status to the plan.
///
/// This is the preferred entry point when a [`PropertySchema`] is available
/// (e.g. from active graph type metadata).
pub fn build_plan_with_schema_and_stats(
    stmt: &Statement,
    stats: Option<&TableStats>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, GleaphError> {
    let mut plan = build_plan_internal(stmt, stats, Some(schema), true)?;
    let warnings = type_check_statement_with_schema(stmt, schema);
    let contradictory = warnings
        .iter()
        .any(|w| w.kind == WarningKind::ImpossiblePattern);
    plan.annotations.statically_contradictory = contradictory;
    plan.annotations.type_diagnostics = if warnings.is_empty() {
        None
    } else {
        Some(warnings)
    };
    Ok(plan)
}

fn semantic_property_accesses(constraints: &[SemanticConstraint]) -> Option<Vec<String>> {
    let props: BTreeSet<String> = constraints
        .iter()
        .filter_map(|c| match c {
            SemanticConstraint::PropertyAccess { property, .. } => Some(property.clone()),
            _ => None,
        })
        .collect();
    (!props.is_empty()).then(|| props.into_iter().collect())
}

fn semantic_where_property_accesses(constraints: &[SemanticConstraint]) -> Option<Vec<String>> {
    let mut props = BTreeSet::new();
    for constraint in constraints {
        if let SemanticConstraint::BooleanContext { expr } = constraint {
            collect_property_names(expr, &mut props);
        }
    }
    (!props.is_empty()).then(|| props.into_iter().collect())
}

fn semantic_aggregates(constraints: &[SemanticConstraint]) -> Option<Vec<crate::ast::AggFunc>> {
    let mut aggs = Vec::new();
    for constraint in constraints {
        if let SemanticConstraint::AggregateCall { func, .. } = constraint
            && !aggs.contains(func)
        {
            aggs.push(*func);
        }
    }
    (!aggs.is_empty()).then_some(aggs)
}

fn semantic_indexable_properties(
    semantic_where_properties: Option<&[String]>,
    indexed_properties: &BTreeSet<String>,
) -> Option<Vec<String>> {
    let props: BTreeSet<String> = semantic_where_properties
        .into_iter()
        .flatten()
        .filter(|prop| indexed_properties.contains(*prop))
        .cloned()
        .collect();
    (!props.is_empty()).then(|| props.into_iter().collect())
}

fn semantic_scan_reason(
    where_props: Option<&[String]>,
    eq_props: Option<&[String]>,
    range_props: Option<&[String]>,
    edge_props: Option<&[String]>,
) -> Option<String> {
    let where_count = where_props.map_or(0, |p| p.len());
    let eq_count = eq_props.map_or(0, |p| p.len());
    let range_count = range_props.map_or(0, |p| p.len());
    let edge_count = edge_props.map_or(0, |p| p.len());
    if where_count == 0 && eq_count == 0 && range_count == 0 && edge_count == 0 {
        return None;
    }
    Some(format!(
        "semantic-where-props={where_count}; vertex-eq-indexable={eq_count}; vertex-range-indexable={range_count}; edge-indexable={edge_count}"
    ))
}

fn estimated_cardinality_reason(
    q: &QueryStmt,
    use_index_scan: bool,
    use_edge_index_scan: bool,
    range_index_scan_op: Option<ConditionalCmpOp>,
    anchor_source: &str,
) -> Option<PlanCardinalityReason> {
    if use_index_scan {
        if let Some((_, prop)) = equality_property_predicate(q.where_clause.as_ref())
            && !prop.eq_ignore_ascii_case("id")
        {
            return Some(PlanCardinalityReason::PropertyIndex {
                property: prop,
                comparison: None,
            });
        }
        if let Some((_, prop, cmp_op)) = range_property_predicate(q.where_clause.as_ref())
            && !prop.eq_ignore_ascii_case("id")
        {
            return Some(PlanCardinalityReason::PropertyIndex {
                property: prop,
                comparison: Some(cmp_op),
            });
        }
        if let Some((_, prop)) = inline_props_hint_predicate(q) {
            return Some(PlanCardinalityReason::InlinePropertyIndex { property: prop });
        }
        if let Some((_, prop)) = inline_where_equality_predicate(q) {
            return Some(PlanCardinalityReason::InlineWhereIndex { property: prop });
        }
        if let Some(cmp) = range_index_scan_op {
            return Some(PlanCardinalityReason::PropertyIndex {
                property: "unknown".into(),
                comparison: Some(cmp),
            });
        }
    }
    if use_edge_index_scan && let Some(prop) = edge_index_property_name(q) {
        return Some(PlanCardinalityReason::EdgePropertyIndex { property: prop });
    }
    Some(PlanCardinalityReason::AnchorHeuristic {
        kind: anchor_source.to_string(),
    })
}

fn collect_property_names(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::PropertyAccess { target, property } => {
            out.insert(property.clone());
            collect_property_names(target, out);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Not(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::PathLength(expr)
        | Expr::Cast { expr, .. }
        | Expr::IsTruth { expr, .. }
        | Expr::IsLabeled { expr, .. }
        | Expr::IsDirected { expr, .. }
        | Expr::IsType { expr, .. } => collect_property_names(expr, out),
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::Concat(left, right)
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right)
        | Expr::ListIndex {
            list: left,
            index: right,
        }
        | Expr::IsSourceOf {
            node: left,
            edge: right,
            ..
        }
        | Expr::IsDestOf {
            node: left,
            edge: right,
            ..
        } => {
            collect_property_names(left, out);
            collect_property_names(right, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_property_names(expr, out);
            for item in list {
                collect_property_names(item, out);
            }
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            collect_property_names(expr, out);
            collect_property_names(pattern, out);
        }
        Expr::Case(c) => {
            if let Some(operand) = &c.operand {
                collect_property_names(operand, out);
            }
            for wt in &c.when_then {
                collect_property_names(&wt.when, out);
                collect_property_names(&wt.then, out);
            }
            if let Some(expr) = &c.else_expr {
                collect_property_names(expr, out);
            }
        }
        Expr::Coalesce(items)
        | Expr::ListLiteral(items)
        | Expr::AllDifferent(items)
        | Expr::Same(items)
        | Expr::PathConstructor(items) => {
            for item in items {
                collect_property_names(item, out);
            }
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_property_names(arg, out);
            }
        }
        Expr::Aggregate(a) => {
            if let Some(expr) = &a.expr {
                collect_property_names(expr, out);
            }
            if let Some(expr) = &a.separator {
                collect_property_names(expr, out);
            }
        }
        Expr::PropertyExists { target, property } => {
            out.insert(property.clone());
            collect_property_names(target, out);
        }
        Expr::RecordLiteral(pairs) => {
            for (_, expr) in pairs {
                collect_property_names(expr, out);
            }
        }
        Expr::LetIn { bindings, body } => {
            for (_, expr) in bindings {
                collect_property_names(expr, out);
            }
            collect_property_names(body, out);
        }
        Expr::Exists(_)
        | Expr::ValueSubquery(_)
        | Expr::Literal(_)
        | Expr::Variable(_)
        | Expr::PathVar(_)
        | Expr::Parameter { .. } => {}
    }
}

/// Selects the best anchor variable and returns it together with a description
/// of the heuristic that was used.
///
/// Priority:
/// 1. A node variable that appears in a property-equality predicate (`=`).
/// 2. The first node variable that carries a label constraint.
/// 3. The start-node variable (full scan, possibly bounded by LIMIT).
fn choose_anchor(
    q: &QueryStmt,
    stats: Option<&TableStats>,
    narrowing_facts: &[NarrowingFact],
    semantic: &SemanticAnalysis,
    schema: Option<&dyn PropertySchema>,
) -> (Option<String>, String) {
    // Check if a variable is in the first MATCH pattern.
    let var_in_pattern = |var: &str| -> bool {
        q.match_clauses.first().is_some_and(|m| {
            m.pattern.start.var.as_deref() == Some(var)
                || m.pattern.hops().any(|c| c.node.var.as_deref() == Some(var))
        })
    };
    // Fall back to legacy AST walk for equality predicates not captured semantically
    // (e.g. nested inside complex OR expressions).
    if let Some((var, _prop)) =
        best_equality_predicate_anchor(q.where_clause.as_ref(), stats, var_in_pattern)
    {
        return (Some(var), "property-equality".into());
    }
    // Legacy fallbacks for inline props/WHERE.
    if let Some((var, _prop)) = inline_props_hint_predicate(q) {
        return (Some(var), "inline-property-equality".into());
    }
    if let Some((var, _prop)) = inline_where_equality_predicate(q) {
        return (Some(var), "inline-where-equality".into());
    }
    // Prefer semantic equality predicates from WHERE for anchor selection when
    // the direct AST walk did not find a candidate.
    let mut best_semantic_eq: Option<(&str, f64)> = None;
    for constraint in &semantic.constraints {
        let SemanticConstraint::WhereEqualityPredicate { var, property } = constraint else {
            continue;
        };
        if !var_in_pattern(var) {
            continue;
        }
        let score = stats
            .and_then(|s| s.property_selectivity.get(&format!("vertex:{property}")).copied())
            .unwrap_or(0.5);
        if best_semantic_eq.as_ref().is_none_or(|(_, best)| score < *best) {
            best_semantic_eq = Some((var.as_str(), score));
        }
    }
    if let Some((var, _)) = best_semantic_eq {
        return (Some(var.to_string()), "property-equality".into());
    }
    // Semantic inline node properties (from pattern).
    if let Some(var) = semantic.constraints.iter().find_map(|c| match c {
        SemanticConstraint::InlineNodeProperty { var, .. } if var_in_pattern(var) => {
            Some(var.as_str())
        }
        _ => None,
    }) {
        return (Some(var.to_string()), "inline-property-equality".into());
    }
    // Semantic inline node WHERE predicates.
    if let Some(var) = semantic.constraints.iter().find_map(|c| match c {
        SemanticConstraint::InlineNodeWherePredicate { var, .. } if var_in_pattern(var) => {
            Some(var.as_str())
        }
        _ => None,
    }) {
        return (Some(var.to_string()), "inline-where-equality".into());
    }
    let first_match = match q.match_clauses.first() {
        Some(m) => &m.pattern,
        None => return (None, "missing-match".into()),
    };
    if let Some((var, source)) = lowest_label_cardinality_anchor(first_match, stats) {
        return (Some(var), source);
    }
    if let Some(var) = first_labeled_node_var(first_match) {
        return (Some(var), "label-only".into());
    }
    // Use WHERE-narrowed labels (e.g. `MATCH (n) WHERE n IS LABELED :Person`)
    // as a fallback for anchor selection when no pattern labels exist.
    if let Some((var, source)) = narrowing_label_anchor(first_match, stats, narrowing_facts) {
        return (Some(var), source);
    }
    // Schema-endpoint-driven: if an edge label is known, infer endpoint labels
    // from schema and prefer the endpoint with lowest label cardinality.
    if let Some(schema) = schema {
        if let Some((var, source)) = schema_endpoint_anchor(first_match, stats, schema) {
            return (Some(var), source);
        }
    }
    (
        first_match.start.var.clone(),
        if q.limit.is_some() {
            "full-scan-bounded-by-limit".into()
        } else {
            "full-scan".into()
        },
    )
}

/// Check if any node variable in the pattern has a WHERE-narrowed label from
/// flow-sensitive analysis. Use cardinality stats if available.
fn narrowing_label_anchor(
    m: &MatchClause,
    stats: Option<&TableStats>,
    narrowing_facts: &[NarrowingFact],
) -> Option<(String, String)> {
    // Build map of var -> narrowed labels from narrowing facts.
    let mut narrowed: rapidhash::fast::RapidHashMap<&str, Vec<&str>> =
        rapidhash::fast::RapidHashMap::default();
    for fact in narrowing_facts {
        if let NarrowingFact::LabelNarrowed { var, label } = fact {
            narrowed
                .entry(var.as_str())
                .or_default()
                .push(label.as_str());
        }
    }
    if narrowed.is_empty() {
        return None;
    }

    // Try to find the lowest-cardinality narrowed label with stats.
    let mut best: Option<(String, u64, String)> = None;
    let mut check_var = |var: &Option<String>| {
        let var_name = var.as_deref()?;
        let labels = narrowed.get(var_name)?;
        for &label in labels {
            if let Some(stats) = stats
                && let Some(&card) = stats.label_cardinality.get(label)
            {
                if best.as_ref().is_none_or(|(_, b, _)| card < *b) {
                    best = Some((
                        var_name.to_string(),
                        card,
                        format!("narrowing-label-cardinality({label}={card})"),
                    ));
                }
            } else if best.is_none() {
                best = Some((
                    var_name.to_string(),
                    1000,
                    format!("narrowing-label({label})"),
                ));
            }
        }
        None::<()>
    };

    check_var(&m.start.var);
    for chain in m.hops() {
        check_var(&chain.node.var);
    }
    best.map(|(var, _, source)| (var, source))
}

/// Schema-endpoint-driven anchor selection: when an edge label is known in the
/// pattern, use `edge_endpoint_types` to infer node labels from the schema.
/// Prefer the endpoint with the lowest label cardinality.
fn schema_endpoint_anchor(
    m: &MatchClause,
    stats: Option<&TableStats>,
    schema: &dyn PropertySchema,
) -> Option<(String, String)> {
    let mut candidates: Vec<(String, String, u64)> = Vec::new();

    // Walk each hop: prev_node -[edge]-> chain.node
    let mut prev_node = &m.start;
    for chain in m.hops() {
        let edge_label = match chain.edge.label.as_deref() {
            Some(l) => l,
            None => {
                prev_node = &chain.node;
                continue;
            }
        };
        let endpoints = schema.edge_endpoint_types(edge_label);
        if endpoints.is_empty() {
            prev_node = &chain.node;
            continue;
        }

        // Determine source/dest node vars based on direction.
        let (src_var, dst_var) = match chain.edge.direction {
            Direction::Outgoing => (prev_node.var.as_deref(), chain.node.var.as_deref()),
            Direction::Incoming => (chain.node.var.as_deref(), prev_node.var.as_deref()),
            Direction::Either => {
                prev_node = &chain.node;
                continue;
            }
        };

        for (from_labels, to_labels) in &endpoints {
            if let Some(var) = src_var {
                for label in from_labels {
                    let card = stats
                        .and_then(|s| s.label_cardinality.get(label.as_str()).copied())
                        .unwrap_or(1000);
                    candidates.push((var.to_string(), label.clone(), card));
                }
            }
            if let Some(var) = dst_var {
                for label in to_labels {
                    let card = stats
                        .and_then(|s| s.label_cardinality.get(label.as_str()).copied())
                        .unwrap_or(1000);
                    candidates.push((var.to_string(), label.clone(), card));
                }
            }
        }

        prev_node = &chain.node;
    }

    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by_key(|(_, _, card)| *card);
    let (var, label, card) = &candidates[0];
    if stats.is_some() {
        Some((
            var.clone(),
            format!("schema-endpoint-cardinality({label}={card})"),
        ))
    } else {
        Some((var.clone(), format!("schema-endpoint({label})")))
    }
}

/// For SHORTEST single-chain outgoing patterns, detect when the end-node has
/// lower estimated cardinality than the start-node. Returns the end-node variable
/// name if reverse-anchor BFS would be beneficial.
fn detect_shortest_reverse_anchor(m: &MatchClause, stats: Option<&TableStats>) -> Option<String> {
    if m.elements.len() != 1 {
        return None;
    }
    let chain = m.chain(0);
    if !matches!(chain.edge.direction, Direction::Outgoing) {
        return None;
    }
    let end_var = chain.node.var.as_ref()?;
    let start_card = estimate_node_cardinality(&m.start, stats);
    let end_card = estimate_node_cardinality(&chain.node, stats);
    if end_card < start_card {
        Some(end_var.clone())
    } else {
        None
    }
}

/// Estimate the number of candidate vertices for a node pattern.
///
/// Priority: inline property constraints → label cardinality from stats → label
/// presence (arbitrary large estimate) → no constraint (very large estimate).
fn estimate_node_cardinality(node: &NodePattern, stats: Option<&TableStats>) -> u64 {
    // Inline property constraints typically resolve to very few vertices.
    if !node.props_hint.is_empty() {
        return 1;
    }
    // Inline WHERE constraint reduces cardinality significantly.
    if node.where_clause.is_some() {
        return 100;
    }
    // Use label cardinality from stats when available.
    if let Some(stats) = stats
        && let Some(label) = node.labels.first()
        && let Some(&card) = stats.label_cardinality.get(label)
    {
        return card;
    }
    // Label present but no stats → moderate estimate.
    if !node.labels.is_empty() {
        return 1000;
    }
    // No constraints at all → large estimate.
    10_000
}

fn lowest_label_cardinality_anchor(
    m: &MatchClause,
    stats: Option<&TableStats>,
) -> Option<(String, String)> {
    let stats = stats?;
    let mut best: Option<(String, u64)> = None;

    let mut consider = |var: &Option<String>, labels: &[String]| {
        let Some(var) = var else { return };
        let Some(label) = labels.first() else { return };
        let Some(card) = stats.label_cardinality.get(label).copied() else {
            return;
        };
        if best.as_ref().is_none_or(|(_, b)| card < *b) {
            best = Some((var.clone(), card));
        }
    };

    consider(&m.start.var, &m.start.labels);
    for c in m.hops() {
        consider(&c.node.var, &c.node.labels);
    }
    best.map(|(v, card)| (v, format!("label-cardinality-stats({card})")))
}

/// Collects ALL equality property predicates (`var.prop = literal/param`) from
/// a WHERE clause, instead of returning only the first one.
fn all_equality_property_predicates(where_clause: Option<&Expr>) -> Vec<(String, String)> {
    fn is_value_source(e: &Expr) -> bool {
        matches!(e, Expr::Literal(_) | Expr::Parameter { .. })
    }
    fn walk_all(expr: &Expr, out: &mut Vec<(String, String)>) {
        match expr {
            Expr::Compare { left, op, right } if *op == CmpOp::Eq => {
                match (left.as_ref(), right.as_ref()) {
                    (Expr::PropertyAccess { target, property }, rhs) if is_value_source(rhs) => {
                        if let Expr::Variable(var) = target.as_ref() {
                            out.push((var.clone(), property.clone()));
                        }
                    }
                    (lhs, Expr::PropertyAccess { target, property }) if is_value_source(lhs) => {
                        if let Expr::Variable(var) = target.as_ref() {
                            out.push((var.clone(), property.clone()));
                        }
                    }
                    _ => {}
                }
            }
            Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) => {
                walk_all(l, out);
                walk_all(r, out);
            }
            Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => walk_all(e, out),
            _ => {}
        }
    }
    let mut results = Vec::new();
    if let Some(expr) = where_clause {
        walk_all(expr, &mut results);
    }
    results
}

/// Selects the best equality predicate anchor by comparing selectivity.
/// When stats are available, picks the predicate with lowest estimated matching rows.
/// Falls back to first-found when no selectivity data is available.
fn best_equality_predicate_anchor(
    where_clause: Option<&Expr>,
    stats: Option<&TableStats>,
    pattern_var_check: impl Fn(&str) -> bool,
) -> Option<(String, String)> {
    let all = all_equality_property_predicates(where_clause);
    if all.is_empty() {
        return None;
    }
    // Filter to only variables in the MATCH pattern.
    let candidates: Vec<_> = all
        .into_iter()
        .filter(|(var, _)| pattern_var_check(var))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates.into_iter().next().unwrap());
    }
    // With stats, pick lowest selectivity. Without stats, prefer indexed properties, then first.
    let Some(stats) = stats else {
        return Some(candidates.into_iter().next().unwrap());
    };
    candidates.into_iter().min_by(|(_, prop_a), (_, prop_b)| {
        let sel_a = stats
            .property_selectivity
            .get(&format!("vertex:{prop_a}"))
            .copied()
            .unwrap_or(0.5);
        let sel_b = stats
            .property_selectivity
            .get(&format!("vertex:{prop_b}"))
            .copied()
            .unwrap_or(0.5);
        sel_a
            .partial_cmp(&sel_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn equality_property_predicate(where_clause: Option<&Expr>) -> Option<(String, String)> {
    /// Returns true if the expression is a literal or parameter (i.e., a known value source).
    fn is_value_source(e: &Expr) -> bool {
        matches!(e, Expr::Literal(_) | Expr::Parameter { .. })
    }
    fn walk(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Compare { left, op, right } if *op == CmpOp::Eq => {
                match (left.as_ref(), right.as_ref()) {
                    (Expr::PropertyAccess { target, .. }, rhs) if is_value_source(rhs) => {
                        match target.as_ref() {
                            Expr::Variable(var) => Some(var.clone()),
                            _ => None,
                        }
                    }
                    (lhs, Expr::PropertyAccess { target, .. }) if is_value_source(lhs) => {
                        match target.as_ref() {
                            Expr::Variable(var) => Some(var.clone()),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
            Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) => walk(l).or_else(|| walk(r)),
            Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => walk(e),
            _ => None,
        }
    }
    fn walk_pair(expr: &Expr) -> Option<(String, String)> {
        match expr {
            Expr::Compare { left, op, right } if *op == CmpOp::Eq => {
                match (left.as_ref(), right.as_ref()) {
                    (Expr::PropertyAccess { target, property }, rhs) if is_value_source(rhs) => {
                        match target.as_ref() {
                            Expr::Variable(var) => Some((var.clone(), property.clone())),
                            _ => None,
                        }
                    }
                    (lhs, Expr::PropertyAccess { target, property }) if is_value_source(lhs) => {
                        match target.as_ref() {
                            Expr::Variable(var) => Some((var.clone(), property.clone())),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
            Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) => {
                walk_pair(l).or_else(|| walk_pair(r))
            }
            Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => walk_pair(e),
            _ => None,
        }
    }
    let _ = walk; // keep local shape stable during refactor
    where_clause.and_then(walk_pair)
}

/// Extracts the first range comparison predicate (`var.prop >= literal`, etc.)
/// from a WHERE clause. Returns `(variable, property, cmp_op)`.
fn range_property_predicate(
    where_clause: Option<&Expr>,
) -> Option<(String, String, ConditionalCmpOp)> {
    fn is_value_source(e: &Expr) -> bool {
        matches!(e, Expr::Literal(_) | Expr::Parameter { .. })
    }
    fn walk(expr: &Expr) -> Option<(String, String, ConditionalCmpOp)> {
        match expr {
            Expr::Compare { left, op, right }
                if matches!(op, CmpOp::Ge | CmpOp::Gt | CmpOp::Le | CmpOp::Lt) =>
            {
                let (var, prop, reversed) = match (left.as_ref(), right.as_ref()) {
                    (Expr::PropertyAccess { target, property }, rhs) if is_value_source(rhs) => {
                        match target.as_ref() {
                            Expr::Variable(v) => (v.clone(), property.clone(), false),
                            _ => return None,
                        }
                    }
                    (lhs, Expr::PropertyAccess { target, property }) if is_value_source(lhs) => {
                        match target.as_ref() {
                            Expr::Variable(v) => (v.clone(), property.clone(), true),
                            _ => return None,
                        }
                    }
                    _ => return None,
                };
                let cmp_op = match (op, reversed) {
                    (CmpOp::Ge, false) | (CmpOp::Le, true) => ConditionalCmpOp::Ge,
                    (CmpOp::Gt, false) | (CmpOp::Lt, true) => ConditionalCmpOp::Gt,
                    (CmpOp::Le, false) | (CmpOp::Ge, true) => ConditionalCmpOp::Le,
                    (CmpOp::Lt, false) | (CmpOp::Gt, true) => ConditionalCmpOp::Lt,
                    _ => return None,
                };
                Some((var, prop, cmp_op))
            }
            Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) => walk(l).or_else(|| walk(r)),
            Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => walk(e),
            _ => None,
        }
    }
    where_clause.and_then(walk)
}

/// Result of detecting a `$param IS NULL OR var.prop = $param` pattern.
struct OptionalFilter {
    param_name: String,
    variable: String,
    property: String,
    cmp_op: ConditionalCmpOp,
}

/// Detects all `$param IS NULL OR var.prop <op> $param` patterns in a WHERE clause.
///
/// Supports equality (`=`) and range (`>=`, `>`, `<=`, `<`) operators.
/// Returns all optional filters found. Both operand orders within the OR
/// and within the comparison are accepted. The same parameter name
/// must appear in both the IS NULL check and the comparison.
fn detect_optional_filters(where_clause: Option<&Expr>) -> Vec<OptionalFilter> {
    fn ast_cmp_to_conditional(op: &CmpOp, reversed: bool) -> Option<ConditionalCmpOp> {
        match op {
            CmpOp::Eq => Some(ConditionalCmpOp::Eq),
            CmpOp::Ge if !reversed => Some(ConditionalCmpOp::Ge),
            CmpOp::Ge if reversed => Some(ConditionalCmpOp::Le),
            CmpOp::Gt if !reversed => Some(ConditionalCmpOp::Gt),
            CmpOp::Gt if reversed => Some(ConditionalCmpOp::Lt),
            CmpOp::Le if !reversed => Some(ConditionalCmpOp::Le),
            CmpOp::Le if reversed => Some(ConditionalCmpOp::Ge),
            CmpOp::Lt if !reversed => Some(ConditionalCmpOp::Lt),
            CmpOp::Lt if reversed => Some(ConditionalCmpOp::Gt),
            _ => None,
        }
    }

    fn try_extract(null_side: &Expr, cmp_side: &Expr) -> Option<OptionalFilter> {
        let param_name = match null_side {
            Expr::IsNull(inner) => match inner.as_ref() {
                Expr::Parameter { name, .. } => name.clone(),
                _ => return None,
            },
            _ => return None,
        };
        match cmp_side {
            Expr::Compare { left, op, right } => {
                let cmp_op = match op {
                    CmpOp::Eq | CmpOp::Ge | CmpOp::Gt | CmpOp::Le | CmpOp::Lt => *op,
                    _ => return None,
                };
                match (left.as_ref(), right.as_ref()) {
                    (Expr::PropertyAccess { target, property }, Expr::Parameter { name, .. })
                        if *name == param_name =>
                    {
                        let cond_op = ast_cmp_to_conditional(&cmp_op, false)?;
                        if let Expr::Variable(var) = target.as_ref() {
                            Some(OptionalFilter {
                                param_name,
                                variable: var.clone(),
                                property: property.clone(),
                                cmp_op: cond_op,
                            })
                        } else {
                            None
                        }
                    }
                    (Expr::Parameter { name, .. }, Expr::PropertyAccess { target, property })
                        if *name == param_name =>
                    {
                        // Reversed: $param >= var.prop → var.prop <= $param
                        let cond_op = ast_cmp_to_conditional(&cmp_op, true)?;
                        if let Expr::Variable(var) = target.as_ref() {
                            Some(OptionalFilter {
                                param_name,
                                variable: var.clone(),
                                property: property.clone(),
                                cmp_op: cond_op,
                            })
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn walk(expr: &Expr, out: &mut Vec<OptionalFilter>) {
        match expr {
            Expr::Or(left, right) => {
                if let Some(of) = try_extract(left, right).or_else(|| try_extract(right, left)) {
                    out.push(of);
                }
            }
            Expr::And(l, r) => {
                walk(l, out);
                walk(r, out);
            }
            _ => {}
        }
    }

    let mut results = Vec::new();
    if let Some(expr) = where_clause {
        walk(expr, &mut results);
    }
    results
}

/// Build optional filters from semantic analysis when available, falling back to AST walk.
fn optional_filters_from_semantic_or_ast(
    semantic: &SemanticAnalysis,
    where_clause: Option<&Expr>,
) -> Vec<OptionalFilter> {
    if where_clause.is_none() {
        return Vec::new();
    }
    let sem_preds: Vec<_> = semantic
        .constraints
        .iter()
        .filter_map(|c| match c {
            SemanticConstraint::OptionalFilterPredicate {
                param_name,
                var,
                property,
                op,
            } => Some((param_name, var, property, *op)),
            _ => None,
        })
        .collect();
    if !sem_preds.is_empty() {
        return sem_preds
            .into_iter()
            .filter_map(|(param_name, var, property, op)| {
                let cmp_op = ast_cmp_to_conditional_cmp(op)?;
                Some(OptionalFilter {
                    param_name: param_name.clone(),
                    variable: var.clone(),
                    property: property.clone(),
                    cmp_op,
                })
            })
            .collect();
    }
    // Fall back to AST walk for patterns not captured semantically.
    detect_optional_filters(where_clause)
}

/// Convert AST `CmpOp` to planner `ConditionalCmpOp`.
fn ast_cmp_to_conditional_cmp(op: CmpOp) -> Option<ConditionalCmpOp> {
    match op {
        CmpOp::Eq => Some(ConditionalCmpOp::Eq),
        CmpOp::Ge => Some(ConditionalCmpOp::Ge),
        CmpOp::Gt => Some(ConditionalCmpOp::Gt),
        CmpOp::Le => Some(ConditionalCmpOp::Le),
        CmpOp::Lt => Some(ConditionalCmpOp::Lt),
        _ => None,
    }
}

/// Scans the first MATCH clause's node patterns for an inline `props_hint` with a
/// literal value. Returns the first `(variable, property)` pair found.
///
/// This complements `equality_property_predicate` which only looks at WHERE clauses.
fn inline_props_hint_predicate(q: &QueryStmt) -> Option<(String, String)> {
    let first_match = q.match_clauses.first()?;
    let m = &first_match.pattern;
    if let Some((prop, _)) = m.start.props_hint.first() {
        let var = m
            .start
            .var
            .clone()
            .unwrap_or_else(|| "__anon_start__".to_string());
        return Some((var, prop.clone()));
    }
    for (i, elem) in m.elements.iter().enumerate() {
        let PatternElement::Hop(chain) = elem else {
            continue;
        };
        if let Some((prop, _)) = chain.node.props_hint.first() {
            let var = chain
                .node
                .var
                .clone()
                .unwrap_or_else(|| format!("__anon_chain_{i}__"));
            return Some((var, prop.clone()));
        }
    }
    None
}

/// Scans inline WHERE clauses on node patterns for `var.prop = literal` equality.
/// Returns the first `(variable, property)` pair found.
fn inline_where_equality_predicate(q: &QueryStmt) -> Option<(String, String)> {
    fn extract_eq(expr: &Expr) -> Option<(String, String)> {
        match expr {
            Expr::Compare { left, op, right } if *op == CmpOp::Eq => {
                match (left.as_ref(), right.as_ref()) {
                    (Expr::PropertyAccess { target, property }, Expr::Literal(_))
                    | (Expr::Literal(_), Expr::PropertyAccess { target, property }) => {
                        if let Expr::Variable(var) = target.as_ref() {
                            return Some((var.clone(), property.clone()));
                        }
                        None
                    }
                    _ => None,
                }
            }
            Expr::And(l, r) => extract_eq(l).or_else(|| extract_eq(r)),
            _ => None,
        }
    }

    let first_match = q.match_clauses.first()?;
    let m = &first_match.pattern;
    if let Some(w) = m.start.where_clause.as_deref()
        && let Some(pair) = extract_eq(w)
    {
        return Some(pair);
    }
    for chain in m.hops() {
        if let Some(w) = chain.node.where_clause.as_deref()
            && let Some(pair) = extract_eq(w)
        {
            return Some(pair);
        }
    }
    None
}

/// Returns the variable name of the first node in the MATCH pattern that has
/// at least one label constraint, or `None` if no such node exists.
fn first_labeled_node_var(m: &MatchClause) -> Option<String> {
    if !m.start.labels.is_empty() {
        return m.start.var.clone();
    }
    for chain in m.hops() {
        if !chain.node.labels.is_empty() {
            return chain.node.var.clone();
        }
    }
    None
}

/// Returns `true` when every WITH clause in the list is a pure 1:1 projection:
/// no DISTINCT, no WHERE, no ORDER BY, no MATCH continuation, no inner LIMIT/OFFSET,
/// and no aggregate functions in items. Under these conditions the overall row count
/// is not changed by the WITH pipeline, so LIMIT pushdown to the MATCH scan is safe.
fn with_clauses_are_pure_projections(with_clauses: &[crate::ast::WithClause]) -> bool {
    with_clauses.iter().all(|w| {
        !w.distinct
            && w.where_clause.is_none()
            && w.order_by.is_none()
            && w.limit.is_none()
            && w.offset.is_none()
            && w.match_clauses.is_empty()
            && w.post_match_where.is_none()
            && !w.items.iter().any(|i| expr_has_aggregate(&i.expr))
    })
}

fn greedy_left_deep_join_order(
    m: &MatchClause,
    stats: Option<&TableStats>,
    schema: Option<&dyn PropertySchema>,
) -> Vec<usize> {
    let mut remaining: Vec<usize> = (0..m.elements.len()).collect();
    let mut order = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let (best_pos, _) = remaining
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                greedy_chain_score(m.chain(**a), stats, schema)
                    .partial_cmp(&greedy_chain_score(m.chain(**b), stats, schema))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("non-empty");
        order.push(remaining.remove(best_pos));
    }
    order
}

fn greedy_chain_score(
    chain: &crate::ast::MatchChain,
    stats: Option<&TableStats>,
    schema: Option<&dyn PropertySchema>,
) -> f64 {
    let explicit_card = chain
        .node
        .labels
        .first()
        .and_then(|l| stats.and_then(|s| s.label_cardinality.get(l).copied()));
    // When the node has no explicit label but the edge has a known label,
    // use schema endpoint metadata to infer the destination node's likely
    // label cardinality. This gives multi-hop join ordering a better cost
    // estimate for unlabeled nodes.
    let label_card = explicit_card
        .or_else(|| {
            if chain.node.labels.is_empty() {
                let edge_label = chain.edge.label.as_deref()?;
                let endpoints = schema?.edge_endpoint_types(edge_label);
                // Pick the minimum cardinality across all possible destination labels.
                let dst_labels = match chain.edge.direction {
                    crate::ast::Direction::Outgoing => endpoints
                        .iter()
                        .flat_map(|(_, to)| to.iter())
                        .collect::<Vec<_>>(),
                    crate::ast::Direction::Incoming => endpoints
                        .iter()
                        .flat_map(|(from, _)| from.iter())
                        .collect::<Vec<_>>(),
                    crate::ast::Direction::Either => endpoints
                        .iter()
                        .flat_map(|(f, t)| f.iter().chain(t.iter()))
                        .collect::<Vec<_>>(),
                };
                let min_card = dst_labels
                    .iter()
                    .filter_map(|l| {
                        stats.and_then(|s| s.label_cardinality.get(l.as_str()).copied())
                    })
                    .min();
                min_card
            } else {
                None
            }
        })
        .unwrap_or(u64::MAX / 4) as f64;
    let len_penalty = match chain.edge.length {
        crate::ast::PathLength::Fixed(n) => n as f64,
        crate::ast::PathLength::Range { min, max } => (min + max).max(1) as f64 * 2.0,
    };
    let dir_penalty = match chain.edge.direction {
        crate::ast::Direction::Outgoing => 1.0,
        crate::ast::Direction::Incoming => 1.05,
        crate::ast::Direction::Either => 1.1,
    };
    // Edge property constraints increase hop selectivity → prefer this chain earlier.
    // Use measured selectivity when an edge property index exists, otherwise a fixed 0.5 bonus.
    let prop_bonus = if chain_has_literal_edge_props(chain) {
        if let Some(s) = stats {
            chain
                .edge
                .properties
                .iter()
                .filter_map(|(k, expr)| {
                    if matches!(expr, crate::ast::Expr::Literal(_)) {
                        s.property_selectivity.get(&format!("edge:{k}")).copied()
                    } else {
                        None
                    }
                })
                .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or(0.5)
        } else {
            0.5
        }
    } else {
        1.0
    };
    label_card * len_penalty * dir_penalty * prop_bonus
}

fn chain_has_literal_edge_props(chain: &crate::ast::MatchChain) -> bool {
    chain.edge.where_clause.is_some()
        || chain
            .edge
            .properties
            .iter()
            .any(|(_, expr)| matches!(expr, crate::ast::Expr::Literal(_)))
}

/// Compute execution order for multiple MATCH clauses.
/// Index 0 is always kept first (anchor selection targets it). Among indices 1..n,
/// reorder by selectivity: prefer smaller start-node cardinality, non-optional, fewer chains.
fn multi_match_clause_order(
    clauses: &[crate::ast::MatchEntry],
    stats: Option<&TableStats>,
) -> Vec<usize> {
    let mut order = vec![0usize]; // first clause always stays first
    let mut remaining: Vec<usize> = (1..clauses.len()).collect();
    while !remaining.is_empty() {
        let (best_pos, _) = remaining
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                match_clause_score(&clauses[**a], stats)
                    .partial_cmp(&match_clause_score(&clauses[**b], stats))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("non-empty");
        order.push(remaining.remove(best_pos));
    }
    order
}

/// Score a MatchEntry for clause reordering. Lower = execute earlier.
fn match_clause_score(entry: &crate::ast::MatchEntry, stats: Option<&TableStats>) -> f64 {
    // OPTIONAL clauses should run last — they always produce output.
    let optional_penalty = if entry.optional { 1e12 } else { 1.0 };
    // Start node label cardinality.
    let start_card = entry
        .pattern
        .start
        .labels
        .first()
        .and_then(|l| stats.and_then(|s| s.label_cardinality.get(l).copied()))
        .unwrap_or(u64::MAX / 4) as f64;
    // Minimum label cardinality across all chain nodes (destination nodes).
    let min_chain_card = entry
        .pattern
        .hops()
        .filter_map(|c| {
            c.node
                .labels
                .first()
                .and_then(|l| stats.and_then(|s| s.label_cardinality.get(l).copied()))
        })
        .min()
        .unwrap_or(u64::MAX / 4) as f64;
    // Use the minimum of start and chain cardinalities for selectivity estimate.
    let card = start_card.min(min_chain_card);
    // More chains → more work.
    let chain_penalty = (entry.pattern.elements.len() as f64 + 1.0).max(1.0);
    optional_penalty * card * chain_penalty
}

fn plan_filter_pushdown_stages(where_clause: Option<&Expr>, m: &MatchClause) -> Vec<usize> {
    let Some(where_clause) = where_clause else {
        return Vec::new();
    };
    let conjuncts = split_conjuncts(where_clause);
    let produced = produced_vars_by_stage(m);
    let final_stage = m.elements.len();
    let mut stages = Vec::new();
    for c in conjuncts {
        let vars = expr_referenced_vars(c);
        let stage = produced
            .iter()
            .enumerate()
            .find(|(_, s)| vars.is_subset(s))
            .map(|(i, _)| i)
            .unwrap_or(final_stage + 1);
        stages.push(stage);
    }
    // High selectivity predicates are planned first within the same stage.
    // PlanOp has no payload, so we only preserve stage placement here.
    stages.sort_unstable();
    stages
}

fn split_conjuncts(expr: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    fn walk<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
        match e {
            Expr::And(l, r) => {
                walk(l, out);
                walk(r, out);
            }
            _ => out.push(e),
        }
    }
    walk(expr, &mut out);
    out
}

fn produced_vars_by_stage(m: &MatchClause) -> Vec<BTreeSet<String>> {
    let mut stages = Vec::with_capacity(m.elements.len() + 1);
    let mut set = BTreeSet::new();
    if let Some(v) = &m.start.var {
        set.insert(v.clone());
    }
    stages.push(set.clone());
    for chain in m.hops() {
        if let Some(v) = &chain.edge.var {
            set.insert(v.clone());
        }
        if let Some(v) = &chain.node.var {
            set.insert(v.clone());
        }
        stages.push(set.clone());
    }
    stages
}

fn expr_referenced_vars(expr: &Expr) -> BTreeSet<String> {
    fn walk(e: &Expr, out: &mut BTreeSet<String>) {
        match e {
            Expr::Variable(v) | Expr::PathVar(v) => {
                out.insert(v.clone());
            }
            Expr::Parameter { .. } => {}
            Expr::PropertyAccess { target, .. } => walk(target, out),
            Expr::UnaryOp { expr, .. }
            | Expr::Not(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr) => walk(expr, out),
            Expr::BinaryOp { left, right, .. }
            | Expr::Compare { left, right, .. }
            | Expr::NullIf { left, right }
            | Expr::ListIndex {
                list: left,
                index: right,
            }
            | Expr::Concat(left, right)
            | Expr::And(left, right)
            | Expr::Or(left, right)
            | Expr::Xor(left, right) => {
                walk(left, out);
                walk(right, out);
            }
            Expr::InList { expr, list, .. } => {
                walk(expr, out);
                for item in list {
                    walk(item, out);
                }
            }
            Expr::StringPredicate { expr, pattern, .. } => {
                walk(expr, out);
                walk(pattern, out);
            }
            Expr::Case(c) => {
                if let Some(op) = &c.operand {
                    walk(op, out);
                }
                for wt in &c.when_then {
                    walk(&wt.when, out);
                    walk(&wt.then, out);
                }
                if let Some(e) = &c.else_expr {
                    walk(e, out);
                }
            }
            Expr::Coalesce(items) | Expr::ListLiteral(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            Expr::FunctionCall { args, .. } => {
                for arg in args {
                    walk(arg, out);
                }
            }
            Expr::Aggregate(a) => {
                if let Some(e) = &a.expr {
                    walk(e, out);
                }
            }
            Expr::PathLength(e) => walk(e, out),
            Expr::Cast { expr, .. }
            | Expr::IsTruth { expr, .. }
            | Expr::IsLabeled { expr, .. }
            | Expr::IsDirected { expr, .. } => {
                walk(expr, out);
            }
            Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
                walk(node, out);
                walk(edge, out);
            }
            Expr::AllDifferent(exprs) | Expr::Same(exprs) => {
                for e in exprs {
                    walk(e, out);
                }
            }
            Expr::PropertyExists { target, .. } => walk(target, out),
            Expr::RecordLiteral(pairs) => {
                for (_, e) in pairs {
                    walk(e, out);
                }
            }
            Expr::Exists(_) | Expr::Literal(_) | Expr::ValueSubquery(_) => {}
            Expr::IsType { expr, .. } => walk(expr, out),
            Expr::LetIn { bindings, body } => {
                for (_, e) in bindings {
                    walk(e, out);
                }
                walk(body, out);
            }
            Expr::PathConstructor(elems) => {
                for e in elems {
                    walk(e, out);
                }
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(expr, &mut out);
    out
}

fn estimate_cost(
    ops: &[PlanOp],
    q: &QueryStmt,
    stats: Option<&TableStats>,
    semantic_where_properties: Option<&[String]>,
) -> CostEstimate {
    let stats = stats.cloned().unwrap_or_default();
    let base_rows = if let Some(first) = q.match_clauses.first() {
        let mut label_rows = None::<u64>;
        if let Some(label) = first.pattern.start.labels.first() {
            label_rows = stats.label_cardinality.get(label).copied();
        }
        label_rows.unwrap_or(stats.vertex_count.max(1)).max(1) as f64
    } else {
        1.0
    };

    // Cost constants derived from canbench IC instruction measurements (see stats.rs).
    let c_scan = COST_SCAN_PER_ROW;
    let c_index_seek = COST_INDEX_SEEK_FRACTION;
    let c_expand = stats.avg_degree.max(1.0) * COST_EXPAND_MULTIPLIER;
    let c_filter = COST_FILTER_PER_ROW;
    let c_agg = COST_AGGREGATE_PER_ROW;
    let c_sort = COST_SORT_NLOGN;
    let c_limit = COST_LIMIT_PER_ROW;
    let c_project = COST_PROJECT_PER_ROW;
    let c_shortest = COST_SHORTEST_PER_ROW;

    // Look up filter selectivity from stats if available, otherwise default to 0.25.
    let filter_sel = if semantic_where_properties.is_some() {
        filter_selectivity_from_stats(q, &stats)
    } else {
        DEFAULT_FILTER_SELECTIVITY
    };

    let mut rows = base_rows;
    let mut instr = 0.0;
    for op in ops {
        match op {
            PlanOp::IndexScan => {
                let cardinality_ratio = q
                    .where_clause
                    .as_ref()
                    .and_then(|w| {
                        equality_property_predicate(Some(w)).and_then(|(_, p)| {
                            stats
                                .property_selectivity
                                .get(&format!("vertex:{p}"))
                                .copied()
                        })
                    })
                    .unwrap_or(0.1)
                    .clamp(0.0001, 1.0);
                // Convert cardinality ratio to expected matching rows.
                let q_sel = query_selectivity(cardinality_ratio, rows);
                let index_result_rows = (rows * q_sel).max(1.0);
                instr += index_result_rows * c_index_seek + c_index_seek * 2.0;
                rows = index_result_rows;
            }
            PlanOp::EdgeIndexScan => {
                // Edge index scan: seed from edge property index instead of vertex scan.
                // Cost similar to vertex index scan but seeds edge pairs.
                let edge_rows = stats.edge_count.max(1) as f64;
                let cardinality_ratio = edge_index_property_name(q)
                    .and_then(|p| {
                        stats
                            .property_selectivity
                            .get(&format!("edge:{p}"))
                            .copied()
                    })
                    .unwrap_or(0.1)
                    .clamp(0.0001, 1.0);
                let q_sel = query_selectivity(cardinality_ratio, edge_rows);
                let index_result_rows = (edge_rows * q_sel).max(1.0);
                instr += index_result_rows * c_index_seek + c_index_seek * 2.0;
                rows = index_result_rows;
            }
            PlanOp::NodeScan | PlanOp::ConditionalIndexScan => instr += rows * c_scan,
            PlanOp::PropertyFilter => {
                instr += rows * c_filter;
                rows *= filter_sel;
            }
            PlanOp::Expand => {
                instr += rows * c_expand;
                rows *= stats.avg_degree.clamp(1.0, 8.0);
            }
            PlanOp::FilterEdge => {
                instr += rows * c_filter;
                rows *= filter_sel;
            }
            PlanOp::ShortestPath => {
                instr += rows * c_shortest;
                rows *= 0.5;
            }
            PlanOp::Aggregate => {
                instr += rows * c_agg;
                rows = rows.clamp(1.0, 10_000.0);
            }
            PlanOp::Project => instr += rows * c_project,
            PlanOp::Sort => {
                // When LIMIT is present the executor uses a top-k heap at O(n log k)
                // instead of a full sort at O(n log n).
                let log_factor = if let Some(limit) = q.limit {
                    let k = (limit.0 as f64).min(rows).max(1.0);
                    k.log2().max(1.0)
                } else {
                    rows.max(1.0).log2().max(1.0)
                };
                instr += rows * log_factor * c_sort;
            }
            PlanOp::Limit => {
                instr += rows * c_limit;
                if let Some(limit) = q.limit {
                    rows = rows.min(limit.0 as f64);
                }
            }
        }
    }
    CostEstimate {
        estimated_rows: rows.max(0.0),
        estimated_instructions: instr.max(0.0),
    }
}

/// Looks up filter selectivity from `TableStats.property_selectivity` for the
/// first equality predicate in the WHERE clause. Falls back to `DEFAULT_FILTER_SELECTIVITY`
/// (0.25) when no measured selectivity is available.
const DEFAULT_FILTER_SELECTIVITY: f64 = 0.25;

/// Convert stored cardinality ratio (`distinct/total`) to query selectivity
/// (expected fraction of rows matching one specific equality value).
///
/// Stored: `sel = distinct_values / total_entities` (high = many distinct values).
/// Query:  For one equality lookup, expected matches = `total / distinct = 1/sel`.
/// Fraction of rows matching = `(1/sel) / base_rows`.
fn query_selectivity(cardinality_ratio: f64, base_rows: f64) -> f64 {
    let distinct = (base_rows * cardinality_ratio).max(1.0);
    (base_rows / distinct / base_rows).clamp(0.0001, 1.0)
}

fn filter_selectivity_from_stats(q: &QueryStmt, stats: &TableStats) -> f64 {
    if let Some((_, prop)) = equality_property_predicate(q.where_clause.as_ref())
        && let Some(&sel) = stats.property_selectivity.get(&format!("vertex:{prop}"))
    {
        // sel is cardinality ratio (distinct/total). Convert to query selectivity.
        // Without base_rows, approximate: 1/distinct ≈ 1/(1000*sel) for moderate graphs.
        // Clamp to safe range.
        let approx_base = stats.vertex_count.max(100) as f64;
        return query_selectivity(sel, approx_base).clamp(0.001, 0.5);
    }
    DEFAULT_FILTER_SELECTIVITY
}

/// Returns `Some(ConditionalCmpOp)` when an index scan should be used.
/// `Some(Eq)` for equality index, `Some(Ge/Gt/Le/Lt)` for range index.
///
/// `conditional_params` contains parameter names involved in `$param IS NULL OR ...`
/// patterns — these must NOT trigger a direct IndexScan (they need ConditionalIndexScan).
fn should_use_index_scan(
    q: &QueryStmt,
    _chosen_anchor: &Option<String>,
    stats: Option<&TableStats>,
    conditional_params: &std::collections::HashSet<&str>,
    semantic_where_properties: Option<&[String]>,
) -> Option<ConditionalCmpOp> {
    let Some(stats) = stats else { return None };

    // Helper: cost-based check for a predicate on (var, prop) with given selectivity default.
    let cost_check = |var: &str, prop: &str| -> bool {
        let var_in_pattern = q.match_clauses.first().is_some_and(|m| {
            m.pattern.start.var.as_deref() == Some(var)
                || m.pattern.hops().any(|c| c.node.var.as_deref() == Some(var))
        });
        if !var_in_pattern {
            return false;
        }
        let cardinality_ratio = stats
            .property_selectivity
            .get(&format!("vertex:{prop}"))
            .copied()
            .unwrap_or(0.1)
            .clamp(0.0001, 1.0);
        let base_rows = q
            .match_clauses
            .first()
            .and_then(|m| m.pattern.start.labels.first())
            .and_then(|l| stats.label_cardinality.get(l).copied())
            .unwrap_or(stats.vertex_count.max(1)) as f64;
        let is_non_start = q
            .match_clauses
            .first()
            .is_some_and(|m| m.pattern.start.var.as_deref() != Some(var));
        let effective_rows = if is_non_start {
            base_rows * stats.avg_degree.max(1.0)
        } else {
            base_rows
        };
        let q_sel = query_selectivity(cardinality_ratio, effective_rows);
        let index_result_rows = (effective_rows * q_sel).max(1.0);
        let scan_filter_cost = effective_rows * COST_SCAN_PER_ROW
            + if is_non_start {
                base_rows * COST_EXPAND_MULTIPLIER
            } else {
                0.0
            }
            + (effective_rows * q_sel) * COST_FILTER_PER_ROW;
        let index_cost =
            index_result_rows * COST_INDEX_SEEK_FRACTION + COST_INDEX_SEEK_FRACTION * 2.0;
        index_cost < scan_filter_cost
    };

    // Helper: check if the predicate uses a parameter that is part of a
    // conditional pattern ($param IS NULL OR ...). Such predicates must use
    // ConditionalIndexScan, not direct IndexScan.
    let uses_conditional_param = |where_clause: Option<&Expr>, var: &str, prop: &str| -> bool {
        fn find_param_in_predicate(expr: &Expr, var: &str, prop: &str) -> Option<String> {
            match expr {
                Expr::Compare { left, op, right }
                    if matches!(
                        op,
                        CmpOp::Eq | CmpOp::Ge | CmpOp::Gt | CmpOp::Le | CmpOp::Lt
                    ) =>
                {
                    match (left.as_ref(), right.as_ref()) {
                        (
                            Expr::PropertyAccess { target, property },
                            Expr::Parameter { name, .. },
                        ) if property == prop => {
                            if let Expr::Variable(v) = target.as_ref() {
                                if v == var {
                                    return Some(name.clone());
                                }
                            }
                            None
                        }
                        (
                            Expr::Parameter { name, .. },
                            Expr::PropertyAccess { target, property },
                        ) if property == prop => {
                            if let Expr::Variable(v) = target.as_ref() {
                                if v == var {
                                    return Some(name.clone());
                                }
                            }
                            None
                        }
                        _ => None,
                    }
                }
                Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) => {
                    find_param_in_predicate(l, var, prop)
                        .or_else(|| find_param_in_predicate(r, var, prop))
                }
                Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => {
                    find_param_in_predicate(e, var, prop)
                }
                _ => None,
            }
        }
        if let Some(wc) = where_clause {
            if let Some(param_name) = find_param_in_predicate(wc, var, prop) {
                return conditional_params.contains(param_name.as_str());
            }
        }
        false
    };

    // Try WHERE-based equality predicates. Collect all and pick the best indexed one
    // (lowest selectivity) instead of using only the first match.
    // Prefer semantic facts when available, fall back to AST walk.
    if semantic_where_properties.is_some() {
        let all_eq = all_equality_property_predicates(q.where_clause.as_ref()); // legacy fallback
        // Filter to indexed, non-id, non-conditional, cost-viable candidates.
        let mut viable: Vec<(String, String, f64)> = all_eq
            .into_iter()
            .filter(|(var, prop)| {
                !prop.eq_ignore_ascii_case("id")
                    && stats.indexed_vertex_properties.contains(prop)
                    && !uses_conditional_param(q.where_clause.as_ref(), var, prop)
                    && cost_check(var, prop)
            })
            .map(|(var, prop)| {
                let sel = stats
                    .property_selectivity
                    .get(&format!("vertex:{}", prop))
                    .copied()
                    .unwrap_or(0.5);
                (var, prop, sel)
            })
            .collect();
        viable.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        if !viable.is_empty() {
            return Some(ConditionalCmpOp::Eq);
        }
    }

    // Try WHERE-based range predicate (>= / > / <= / <).
    if semantic_where_properties.is_some()
        && let Some((var, prop, cmp_op)) = range_property_predicate(q.where_clause.as_ref())
        && !prop.eq_ignore_ascii_case("id")
        && stats.range_indexed_vertex_properties.contains(&prop)
        && !uses_conditional_param(q.where_clause.as_ref(), &var, &prop)
        && cost_check(&var, &prop)
    {
        return Some(cmp_op);
    }

    // Fallback: try inline props_hint (e.g. `(p:Product {id: 5})`).
    // Note: "id" is NOT skipped for inline props — it refers to a stored property, not vertex_id.
    if let Some((var, prop)) = inline_props_hint_predicate(q)
        && stats.indexed_vertex_properties.contains(&prop)
    {
        let var_in_pattern = q.match_clauses.first().is_some_and(|m| {
            m.pattern.start.var.as_deref() == Some(&var)
                || (m.pattern.start.var.is_none() && var == "__anon_start__")
                || m.pattern.elements.iter().enumerate().any(|(i, elem)| {
                    let PatternElement::Hop(c) = elem else {
                        return false;
                    };
                    c.node.var.as_deref() == Some(&var)
                        || (c.node.var.is_none() && var == format!("__anon_chain_{i}__"))
                })
        });
        if var_in_pattern {
            return Some(ConditionalCmpOp::Eq);
        }
    }

    // Fallback: try inline WHERE equality predicate (e.g. `(n:User WHERE n.name = 'Alice')`).
    if let Some((var, prop)) = inline_where_equality_predicate(q)
        && stats.indexed_vertex_properties.contains(&prop)
    {
        let var_in_pattern = q.match_clauses.first().is_some_and(|m| {
            m.pattern.start.var.as_deref() == Some(&var)
                || m.pattern
                    .hops()
                    .any(|c| c.node.var.as_deref() == Some(&var))
        });
        if var_in_pattern {
            return Some(ConditionalCmpOp::Eq);
        }
    }

    None
}

fn index_scan_source(q: &QueryStmt, _chosen_anchor: &Option<String>) -> Option<String> {
    // Try WHERE-based equality predicate.
    if let Some((var, prop)) = equality_property_predicate(q.where_clause.as_ref())
        && !prop.eq_ignore_ascii_case("id")
    {
        let var_in_pattern = q.match_clauses.first().is_some_and(|m| {
            m.pattern.start.var.as_deref() == Some(&var)
                || m.pattern
                    .hops()
                    .any(|c| c.node.var.as_deref() == Some(&var))
        });
        if var_in_pattern {
            return Some(format!("property-index(vertex:{prop})"));
        }
    }
    // Try WHERE-based range predicate.
    if let Some((var, prop, _cmp_op)) = range_property_predicate(q.where_clause.as_ref())
        && !prop.eq_ignore_ascii_case("id")
    {
        let var_in_pattern = q.match_clauses.first().is_some_and(|m| {
            m.pattern.start.var.as_deref() == Some(&var)
                || m.pattern
                    .hops()
                    .any(|c| c.node.var.as_deref() == Some(&var))
        });
        if var_in_pattern {
            return Some(format!("range-index(vertex:{prop})"));
        }
    }
    // Fallback: inline props_hint. "id" is NOT skipped (stored property).
    if let Some((_var, prop)) = inline_props_hint_predicate(q) {
        return Some(format!("inline-property-index(vertex:{prop})"));
    }
    // Fallback: inline WHERE equality predicate.
    if let Some((_var, prop)) = inline_where_equality_predicate(q) {
        return Some(format!("inline-where-index(vertex:{prop})"));
    }
    None
}

fn expr_has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate(_) => true,
        Expr::PropertyAccess { target, .. }
        | Expr::UnaryOp { expr: target, .. }
        | Expr::Not(target)
        | Expr::IsNull(target)
        | Expr::IsNotNull(target)
        | Expr::PathLength(target) => expr_has_aggregate(target),
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::ListIndex {
            list: left,
            index: right,
        }
        | Expr::Concat(left, right)
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right) => expr_has_aggregate(left) || expr_has_aggregate(right),
        Expr::InList { expr, list, .. } => {
            expr_has_aggregate(expr) || list.iter().any(expr_has_aggregate)
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            expr_has_aggregate(expr) || expr_has_aggregate(pattern)
        }
        Expr::Case(c) => {
            c.operand.as_ref().is_some_and(|e| expr_has_aggregate(e))
                || c.when_then
                    .iter()
                    .any(|wt| expr_has_aggregate(&wt.when) || expr_has_aggregate(&wt.then))
                || c.else_expr.as_ref().is_some_and(|e| expr_has_aggregate(e))
        }
        Expr::Coalesce(items) | Expr::ListLiteral(items) => items.iter().any(expr_has_aggregate),
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_aggregate),
        Expr::Exists(_)
        | Expr::Literal(_)
        | Expr::Variable(_)
        | Expr::PathVar(_)
        | Expr::Parameter { .. } => false,
        Expr::Cast { expr, .. }
        | Expr::IsTruth { expr, .. }
        | Expr::IsLabeled { expr, .. }
        | Expr::IsDirected { expr, .. } => expr_has_aggregate(expr),
        Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
            expr_has_aggregate(node) || expr_has_aggregate(edge)
        }
        Expr::AllDifferent(exprs) | Expr::Same(exprs) => exprs.iter().any(expr_has_aggregate),
        Expr::PropertyExists { target, .. } => expr_has_aggregate(target),
        Expr::RecordLiteral(pairs) => pairs.iter().any(|(_, e)| expr_has_aggregate(e)),
        Expr::IsType { expr, .. } => expr_has_aggregate(expr),
        Expr::ValueSubquery(_) => false,
        Expr::LetIn { bindings, body } => {
            bindings.iter().any(|(_, e)| expr_has_aggregate(e)) || expr_has_aggregate(body)
        }
        Expr::PathConstructor(elems) => elems.iter().any(expr_has_aggregate),
    }
}

/// Extracts the edge property name from a query's first chain that has an
/// edge property equality predicate (inline or WHERE-based).
fn edge_index_property_name(q: &QueryStmt) -> Option<String> {
    let first_match = q.match_clauses.first()?;
    let m = &first_match.pattern;
    // Check inline edge property hints first.
    for chain in m.hops() {
        for (prop, expr) in &chain.edge.properties {
            if matches!(expr, Expr::Literal(_)) {
                return Some(prop.clone());
            }
        }
    }
    // Check WHERE clause for edge variable property equality.
    if let Some(where_clause) = &q.where_clause
        && let Some((var, prop)) = equality_property_predicate(Some(where_clause))
    {
        // Check if var matches an edge variable in the first chain.
        for chain in m.hops() {
            if chain.edge.var.as_deref() == Some(&var) {
                return Some(prop);
            }
        }
    }
    None
}

/// Detects whether an edge-index-seeded scan should be used.
/// Returns true when:
/// 1. The first chain has an edge with a property equality predicate (inline or WHERE)
/// 2. The property has a registered edge index in stats
/// 3. The cost is favorable compared to a full vertex scan
fn should_use_edge_index_scan(
    q: &QueryStmt,
    stats: Option<&TableStats>,
    semantic_where_properties: Option<&[String]>,
) -> bool {
    let Some(stats) = stats else { return false };
    let first_match = q.match_clauses.first();
    let first_match = match first_match {
        Some(m) => m,
        None => return false,
    };
    // Don't use edge index for OPTIONAL or SHORTEST patterns.
    if first_match.optional || first_match.shortest {
        return false;
    }
    let m = &first_match.pattern;
    if m.elements.is_empty() {
        return false;
    }

    // Try inline edge property hints on first chain.
    let chain = m.chain(0);
    for (prop, expr) in &chain.edge.properties {
        if matches!(expr, Expr::Literal(_)) && stats.indexed_edge_properties.contains(prop) {
            return true;
        }
    }
    // Try WHERE clause: e.prop = literal where e is the first chain's edge variable.
    if semantic_where_properties.is_some()
        && let Some(where_clause) = &q.where_clause
        && let Some((var, prop)) = equality_property_predicate(Some(where_clause))
        && chain.edge.var.as_deref() == Some(&var)
        && stats.indexed_edge_properties.contains(&prop)
    {
        return true;
    }
    false
}

fn edge_index_scan_source(q: &QueryStmt) -> Option<String> {
    let prop = edge_index_property_name(q)?;
    Some(format!("edge-property-index(edge:{prop})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::AggFunc;
    use crate::parser::parse_statement;
    use crate::plan::PlanOp;
    use crate::validate::validate_statement;

    #[test]
    fn plan_contains_expected_ops_for_query_shape() {
        let stmt = parse_statement(
            "MATCH (a:User)-[:KNOWS]->(b) WHERE a.id = 1 RETURN b.name ORDER BY b.name LIMIT 5",
        )
        .unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert_eq!(
            plan.ops,
            vec![
                PlanOp::NodeScan,
                PlanOp::PropertyFilter,
                PlanOp::Expand,
                PlanOp::Project,
                PlanOp::Sort,
                PlanOp::Limit
            ]
        );
        assert_eq!(plan.annotations.chosen_anchor.as_deref(), Some("a"));
    }

    #[test]
    fn planner_rejects_mutations_for_now() {
        let stmt = parse_statement("INSERT (:User)").unwrap();
        assert!(matches!(
            build_plan(&stmt).unwrap_err(),
            GleaphError::UnsupportedFeature(_)
        ));
    }

    #[test]
    fn planner_prefers_property_equality_anchor() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b:User) WHERE b.id = 1 RETURN a, b LIMIT 10")
            .unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert_eq!(plan.annotations.chosen_anchor.as_deref(), Some("b"));
        assert_eq!(
            plan.annotations.estimated_cardinality_source.as_deref(),
            Some("property-equality")
        );
    }

    #[test]
    fn plan_annotations_include_semantic_facts() {
        let stmt = parse_statement(
            "MATCH (n:User) WHERE n.age > 18 RETURN n.name, COUNT(n) AS c ORDER BY n.name",
        )
        .unwrap();
        let stats = TableStats {
            indexed_vertex_properties: ["age".into()].into_iter().collect(),
            range_indexed_vertex_properties: ["age".into()].into_iter().collect(),
            ..TableStats::default()
        };
        validate_statement(&stmt).unwrap();
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

        assert_eq!(
            plan.annotations.semantic_property_accesses,
            Some(vec!["age".into(), "name".into()])
        );
        assert_eq!(
            plan.annotations.semantic_where_property_accesses,
            Some(vec!["age".into()])
        );
        assert_eq!(
            plan.annotations.semantic_indexable_vertex_properties,
            Some(vec!["age".into()])
        );
        assert_eq!(
            plan.annotations.semantic_range_indexable_vertex_properties,
            Some(vec!["age".into()])
        );
        assert_eq!(plan.annotations.semantic_indexable_edge_properties, None);
        assert_eq!(
            plan.annotations.semantic_scan_reason.as_deref(),
            Some(
                "semantic-where-props=1; vertex-eq-indexable=1; vertex-range-indexable=1; edge-indexable=0"
            )
        );
        assert_eq!(plan.annotations.semantic_conditional_scan_properties, None);
        assert_eq!(
            plan.annotations.semantic_aggregates,
            Some(vec![AggFunc::Count])
        );
    }

    #[test]
    fn conditional_scan_annotations_include_semantic_properties() {
        let stmt =
            parse_statement("MATCH (u:User) WHERE $name IS NULL OR u.name = $name RETURN u.name")
                .unwrap();
        let stats = TableStats {
            indexed_vertex_properties: ["name".into()].into_iter().collect(),
            ..TableStats::default()
        };
        validate_statement(&stmt).unwrap();
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

        assert_eq!(
            plan.annotations.semantic_conditional_scan_properties,
            Some(vec!["name".into()])
        );
        let explain = plan.explain_lines();
        assert!(
            explain
                .iter()
                .any(|line| { line == "ops=ConditionalIndexScan,PropertyFilter,Project" })
        );
        assert!(
            explain
                .iter()
                .any(|line| line == "conditional-scan-semantic-properties=name")
        );
        assert!(
            explain
                .iter()
                .any(|line| { line.starts_with("conditional-scan-candidate=u.name:eq") })
        );
    }

    #[test]
    fn estimated_cardinality_reason_is_structured() {
        let stmt = parse_statement("MATCH (n:User) WHERE n.age >= 18 RETURN n.name").unwrap();
        let mut stats = TableStats {
            range_indexed_vertex_properties: ["age".into()].into_iter().collect(),
            vertex_count: 10_000,
            ..TableStats::default()
        };
        stats.property_selectivity.insert("vertex:age".into(), 0.01);
        validate_statement(&stmt).unwrap();
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

        assert!(matches!(
            plan.annotations.estimated_cardinality_reason,
            Some(PlanCardinalityReason::PropertyIndex {
                ref property,
                comparison: Some(ConditionalCmpOp::Ge)
            }) if property == "age"
        ));
        let explain = plan.annotations.explain_lines();
        assert!(
            explain
                .iter()
                .any(|line| { line == "estimated-cardinality-reason=property-index(age, ge)" })
        );
    }

    #[test]
    fn explain_lines_include_semantic_and_conditional_reasoning() {
        let stmt = parse_statement(
            "MATCH (u:User) WHERE ($name IS NULL OR u.name = $name) AND ($city IS NULL OR u.city = $city) RETURN COUNT(u)",
        )
        .unwrap();
        let mut stats = TableStats {
            indexed_vertex_properties: ["name".into(), "city".into()].into_iter().collect(),
            vertex_count: 10_000,
            ..TableStats::default()
        };
        stats
            .property_selectivity
            .insert("vertex:name".into(), 0.02);
        stats
            .property_selectivity
            .insert("vertex:city".into(), 0.10);
        validate_statement(&stmt).unwrap();
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();

        let explain = plan.explain_lines();
        assert!(explain.iter().any(|line| {
            line == "semantic-scan-reason=semantic-where-props=2; vertex-eq-indexable=2; vertex-range-indexable=0; edge-indexable=0"
        }));
        assert!(
            explain
                .iter()
                .any(|line| line == "semantic-where-properties=city,name")
        );
        assert!(
            explain
                .iter()
                .any(|line| line == "semantic-indexable-vertex-properties=city,name")
        );
        assert!(
            explain
                .iter()
                .any(|line| line == "semantic-aggregates=count")
        );
        assert!(explain.iter().any(|line| {
            line == "conditional-scan-candidates=u.name:eq:$name, u.city:eq:$city"
        }));
        assert!(
            explain
                .iter()
                .any(|line| { line == "conditional-scan-candidate=u.name:eq (selectivity=0.020)" })
        );
        assert!(
            explain
                .iter()
                .any(|line| { line == "conditional-scan-candidate=u.city:eq (selectivity=0.100)" })
        );
    }

    #[test]
    fn explain_plan_with_stats_exposes_structured_reasons() {
        let stmt = parse_statement(
            "MATCH (u:User) WHERE ($name IS NULL OR u.name = $name) AND ($city IS NULL OR u.city = $city) RETURN COUNT(u)",
        )
        .unwrap();
        let mut stats = TableStats {
            indexed_vertex_properties: ["name".into(), "city".into()].into_iter().collect(),
            vertex_count: 10_000,
            ..TableStats::default()
        };
        stats
            .property_selectivity
            .insert("vertex:name".into(), 0.02);
        stats
            .property_selectivity
            .insert("vertex:city".into(), 0.10);
        validate_statement(&stmt).unwrap();

        let explain = explain_plan_with_stats(&stmt, Some(&stats)).unwrap();

        assert!(explain.iter().any(|line| {
            line.starts_with("ops=ConditionalIndexScan")
                && line.contains("Aggregate")
                && line.ends_with("Project")
        }));
        assert!(explain.iter().any(|line| {
            line == "semantic-scan-reason=semantic-where-props=2; vertex-eq-indexable=2; vertex-range-indexable=0; edge-indexable=0"
        }));
        assert!(explain.iter().any(|line| {
            line == "conditional-scan-candidates=u.name:eq:$name, u.city:eq:$city"
        }));
        assert!(
            explain
                .iter()
                .any(|line| line == "semantic-aggregates=count")
        );
    }

    #[test]
    fn planner_falls_back_to_label_anchor_then_full_scan() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b:User) RETURN a").unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert_eq!(
            plan.annotations.estimated_cardinality_source.as_deref(),
            Some("label-only")
        );

        let stmt = parse_statement("MATCH (a)-[:X]->(b) RETURN a LIMIT 2").unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert_eq!(
            plan.annotations.estimated_cardinality_source.as_deref(),
            Some("full-scan-bounded-by-limit")
        );
    }

    #[test]
    fn planner_emits_shortest_path_operator_for_shortest_match() {
        let stmt = parse_statement("MATCH SHORTEST p = (a)-[:KNOWS*1..3]->(b) RETURN p, length(p)")
            .unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert!(plan.ops.iter().any(|op| matches!(op, PlanOp::ShortestPath)));
    }

    #[test]
    fn planner_emits_aggregate_operator_for_aggregate_query() {
        let stmt = parse_statement("MATCH (a)-[:KNOWS]->(b) RETURN a.name, COUNT(*)").unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert!(plan.ops.iter().any(|op| matches!(op, PlanOp::Aggregate)));
    }

    #[test]
    fn planner_can_use_label_cardinality_stats_for_anchor_selection() {
        let stmt = parse_statement("MATCH (a:Hot)-[:X]->(b:Rare) RETURN a, b").unwrap();
        let mut stats = TableStats::default();
        stats.label_cardinality.insert("Hot".into(), 10_000);
        stats.label_cardinality.insert("Rare".into(), 3);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert_eq!(plan.annotations.chosen_anchor.as_deref(), Some("b"));
        assert!(
            plan.annotations
                .estimated_cardinality_source
                .as_deref()
                .is_some_and(|s| s.starts_with("label-cardinality-stats"))
        );
    }

    #[test]
    fn planner_populates_cost_estimate_annotations() {
        let stmt = parse_statement("MATCH (a:User)-[:KNOWS]->(b) RETURN b LIMIT 5").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 3.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 100);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(plan.annotations.estimated_rows.is_some());
        assert!(plan.annotations.estimated_instructions.is_some());
        assert!(plan.annotations.estimated_instructions.unwrap() > 0.0);
    }

    #[test]
    fn planner_pushes_filters_to_earliest_producing_stage() {
        let stmt = parse_statement(
            "MATCH (a)-[e:X]->(b)-[:Y]->(c) WHERE a.id = 1 AND gleaph_timestamp(e) > 10 AND c.name = 'x' RETURN c",
        )
        .unwrap();
        let plan = build_plan(&stmt).unwrap();
        // one filter after scan, one after first expand, one after second expand
        assert_eq!(
            plan.ops,
            vec![
                PlanOp::NodeScan,
                PlanOp::PropertyFilter,
                PlanOp::Expand,
                PlanOp::PropertyFilter,
                PlanOp::Expand,
                PlanOp::PropertyFilter,
                PlanOp::Project,
            ]
        );
        assert_eq!(plan.annotations.filter_pushdown_stages, Some(vec![0, 1, 2]));
    }

    #[test]
    fn planner_pushes_limit_before_project_when_safe() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) RETURN b LIMIT 3").unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert_eq!(
            plan.ops,
            vec![
                PlanOp::NodeScan,
                PlanOp::Expand,
                PlanOp::Limit,
                PlanOp::Project
            ]
        );
        assert!(plan.annotations.limit_pushdown_applied);

        let stmt = parse_statement("MATCH (a)-[:X]->(b) RETURN b ORDER BY b LIMIT 3").unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert!(!plan.annotations.limit_pushdown_applied);
        assert_eq!(
            plan.ops,
            vec![
                PlanOp::NodeScan,
                PlanOp::Expand,
                PlanOp::Project,
                PlanOp::Sort,
                PlanOp::Limit
            ]
        );
    }

    #[test]
    fn planner_emits_greedy_join_order_annotation_for_multi_hop_match() {
        let stmt =
            parse_statement("MATCH (a)-[:X]->(b:Hot)-[:Y]->(c:Rare)-[:Z]->(d:Warm) RETURN d")
                .unwrap();
        let mut stats = TableStats::default();
        stats.label_cardinality.insert("Hot".into(), 10_000);
        stats.label_cardinality.insert("Rare".into(), 5);
        stats.label_cardinality.insert("Warm".into(), 100);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert_eq!(plan.annotations.join_order, Some(vec![1, 2, 0]));
    }

    #[test]
    fn planner_can_choose_index_scan_when_property_selectivity_is_low() {
        let stmt = parse_statement("MATCH (a:User)-[:X]->(b) WHERE a.uid = 42 RETURN a").unwrap();
        let mut stats = TableStats {
            vertex_count: 100_000,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 50_000);
        stats
            .property_selectivity
            .insert("vertex:uid".into(), 0.0001);
        stats.indexed_vertex_properties.insert("uid".into());
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(matches!(plan.ops.first(), Some(PlanOp::IndexScan)));
        assert_eq!(
            plan.annotations.estimated_cardinality_source.as_deref(),
            Some("property-index(vertex:uid)")
        );
    }

    #[test]
    fn planner_keeps_label_scan_when_property_selectivity_is_not_available() {
        let stmt = parse_statement("MATCH (a:User)-[:X]->(b) WHERE a.uid = 42 RETURN a").unwrap();
        let mut stats = TableStats {
            vertex_count: 100_000,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 50_000);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(matches!(plan.ops.first(), Some(PlanOp::NodeScan)));
    }

    // ── Cost estimation value tests ──────────────────────────────────────────

    #[test]
    fn cost_estimate_simple_scan_expand_project() {
        // NodeScan(100) → Expand(avg_degree=3) → Project
        // scan instr = 100 * COST_SCAN_PER_ROW = 100 * 1.0 = 100
        // expand instr = 100 * (3.0 * COST_EXPAND_MULTIPLIER) = 100 * 18.57 = 1857, rows = 300
        // project instr = 300 * COST_PROJECT_PER_ROW = 300 * 0.089 = 26.7
        // total instr = 1983.7, rows = 300
        let stmt = parse_statement("MATCH (a:User)-[:X]->(b) RETURN b").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 3.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 100);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        let est_rows = plan.annotations.estimated_rows.unwrap();
        let est_instr = plan.annotations.estimated_instructions.unwrap();
        // rows should reflect 100 * min(3.0, 8.0) = 300
        assert!((est_rows - 300.0).abs() < 1.0, "got rows={est_rows}");
        // 100 + 100*(3.0*6.19) + 300*0.089 = 100 + 1857 + 26.7 = 1983.7
        assert!((est_instr - 1983.7).abs() < 1.0, "got instr={est_instr}");
    }

    #[test]
    fn cost_estimate_filter_reduces_rows_by_quarter() {
        // NodeScan(100) → PropertyFilter → Expand(avg_degree=2) → Project
        // scan: instr += 100 * 1.0 = 100, rows = 100
        // filter: instr += 100 * COST_FILTER_PER_ROW = 40.7, rows = 25
        // expand: instr += 25 * (2 * COST_EXPAND_MULTIPLIER) = 25 * 12.38 = 309.5, rows = 50
        // project: instr += 50 * 0.089 = 4.45
        // total instr ≈ 454.65, rows = 50
        let stmt = parse_statement("MATCH (a:User)-[:X]->(b) WHERE a.id = 1 RETURN b").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 2.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 100);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        let est_rows = plan.annotations.estimated_rows.unwrap();
        let est_instr = plan.annotations.estimated_instructions.unwrap();
        assert!((est_rows - 50.0).abs() < 1.0, "got rows={est_rows}");
        // 100 + 100*0.407 + 25*(2*6.19) + 50*0.089 = 100 + 40.7 + 309.5 + 4.45 = 454.65
        assert!((est_instr - 454.65).abs() < 1.0, "got instr={est_instr}");
    }

    #[test]
    fn cost_estimate_sort_has_nlogn_component() {
        let stmt =
            parse_statement("MATCH (a:User)-[:X]->(b) RETURN b ORDER BY b LIMIT 10").unwrap();
        let mut stats = TableStats {
            vertex_count: 10_000,
            avg_degree: 2.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 1_000);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        let est_instr = plan.annotations.estimated_instructions.unwrap();
        // NodeScan(1000) + Expand(c_expand=2×3=6, rows→2000) + Project + Sort + Limit
        // scan=1000, expand=1000×6=6000, project=2000×0.5=1000,
        // sort=2000×log2(2000)×COST_SORT_NLOGN ≈ 2000×10.97×0.06 ≈ 1316, limit=200
        // total ≈ 9516 → sort adds a non-trivial n·log₂(n) term on top of the base cost
        assert!(
            est_instr > 5_000.0,
            "sort should add a meaningful term; got instr={est_instr}"
        );
    }

    #[test]
    fn cost_estimate_sort_limit_uses_top_k_cost() {
        // ORDER BY + LIMIT should use O(n log k) cost, not O(n log n).
        let stmt_limit =
            parse_statement("MATCH (a:User)-[:X]->(b) RETURN b ORDER BY b LIMIT 5").unwrap();
        let stmt_no_limit =
            parse_statement("MATCH (a:User)-[:X]->(b) RETURN b ORDER BY b").unwrap();
        let mut stats = TableStats {
            vertex_count: 10_000,
            avg_degree: 2.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 1_000);
        let plan_limit = build_plan_with_stats(&stmt_limit, Some(&stats)).unwrap();
        let plan_no_limit = build_plan_with_stats(&stmt_no_limit, Some(&stats)).unwrap();
        let instr_limit = plan_limit.annotations.estimated_instructions.unwrap();
        let instr_no_limit = plan_no_limit.annotations.estimated_instructions.unwrap();
        // Sort with LIMIT 5 uses log2(5)≈2.32 vs log2(2000)≈10.97 → ~4.7× cheaper sort
        assert!(
            instr_limit < instr_no_limit,
            "ORDER BY + LIMIT should be cheaper than ORDER BY alone: with={instr_limit}, without={instr_no_limit}"
        );
    }

    #[test]
    fn cost_estimate_limit_pushdown_caps_rows() {
        let stmt = parse_statement("MATCH (a:User)-[:X]->(b) RETURN b LIMIT 5").unwrap();
        let mut stats = TableStats {
            vertex_count: 10_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 1_000);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        // With limit pushdown: Limit placed before Project caps rows to 5
        let est_rows = plan.annotations.estimated_rows.unwrap();
        assert!(est_rows <= 5.0, "got rows={est_rows}");
        assert!(plan.annotations.limit_pushdown_applied);
    }

    #[test]
    fn cost_estimate_aggregate_caps_output_rows() {
        let stmt = parse_statement("MATCH (a:User)-[:KNOWS]->(b) RETURN a.name, COUNT(*)").unwrap();
        let mut stats = TableStats {
            vertex_count: 100_000,
            avg_degree: 5.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 100_000);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        let est_rows = plan.annotations.estimated_rows.unwrap();
        // Aggregate caps rows to min(input_rows, 10_000)
        assert!(est_rows <= 10_000.0, "got rows={est_rows}");
    }

    #[test]
    fn cost_estimate_avg_degree_affects_expand_cost() {
        let stmt = parse_statement("MATCH (a:User)-[:X]->(b) RETURN b").unwrap();

        // Low degree
        let mut stats_low = TableStats {
            vertex_count: 100,
            avg_degree: 1.0,
            ..Default::default()
        };
        stats_low.label_cardinality.insert("User".into(), 100);
        let plan_low = build_plan_with_stats(&stmt, Some(&stats_low)).unwrap();

        // High degree
        let mut stats_high = TableStats {
            vertex_count: 100,
            avg_degree: 8.0,
            ..Default::default()
        };
        stats_high.label_cardinality.insert("User".into(), 100);
        let plan_high = build_plan_with_stats(&stmt, Some(&stats_high)).unwrap();

        let instr_low = plan_low.annotations.estimated_instructions.unwrap();
        let instr_high = plan_high.annotations.estimated_instructions.unwrap();
        assert!(
            instr_high > instr_low * 2.0,
            "high degree should cost significantly more: low={instr_low}, high={instr_high}"
        );
    }

    #[test]
    fn cost_estimate_index_scan_cheaper_than_full_scan_on_selective_property() {
        let stmt = parse_statement("MATCH (a:User)-[:X]->(b) WHERE a.uid = 42 RETURN a").unwrap();
        let mut stats = TableStats {
            vertex_count: 100_000,
            avg_degree: 3.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 50_000);
        stats.property_selectivity.insert("vertex:uid".into(), 1.0); // cardinality ratio 1.0 = all unique
        stats.indexed_vertex_properties.insert("uid".into());

        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        let est_instr = plan.annotations.estimated_instructions.unwrap();
        // IndexScan path should be much cheaper than full scan + filter.
        // With cardinality_ratio 1.0 (all unique), index returns ~1 row → cheap.
        assert!(
            est_instr < 500.0,
            "index scan should be cheap: got instr={est_instr}"
        );
    }

    #[test]
    fn cost_estimate_multi_hop_chain_multiplies_rows() {
        // Two expansions: rows multiply by avg_degree each time
        let stmt = parse_statement("MATCH (a:User)-[:X]->(b)-[:Y]->(c) RETURN c").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 100);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        let est_rows = plan.annotations.estimated_rows.unwrap();
        // After scan: 100; after expand1: 100*4=400; after expand2: 400*4=1600
        assert!((est_rows - 1600.0).abs() < 10.0, "got rows={est_rows}");
    }

    #[test]
    fn cost_estimate_with_no_stats_uses_defaults() {
        // Without stats, should still produce valid estimates (using defaults)
        let stmt = parse_statement("MATCH (a:User)-[:X]->(b) RETURN b").unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert!(plan.annotations.estimated_rows.is_some());
        assert!(plan.annotations.estimated_instructions.is_some());
        // Should be positive
        let est_instr = plan.annotations.estimated_instructions.unwrap();
        assert!(est_instr > 0.0, "got instr={est_instr}");
    }

    #[test]
    fn planner_pushes_edge_property_into_filter() {
        // An edge with inline literal property hint `{weight: 5}` should produce
        // a FilterEdge op immediately after the Expand.
        let stmt = parse_statement("MATCH (a)-[e:KNOWS {weight: 5}]->(b) RETURN a, b").unwrap();
        let plan = build_plan(&stmt).unwrap();
        // Expected: NodeScan → Expand → FilterEdge → Project
        assert_eq!(
            plan.ops,
            vec![
                PlanOp::NodeScan,
                PlanOp::Expand,
                PlanOp::FilterEdge,
                PlanOp::Project,
            ],
            "ops were: {:?}",
            plan.ops
        );
    }

    #[test]
    fn match_clause_order_reorders_optional_by_selectivity() {
        // MATCH + two OPTIONAL MATCHes with different label selectivity.
        // Rare OPTIONAL should come before Common OPTIONAL.
        let stmt = parse_statement(
            "MATCH (a:User)-[:X]->(b) OPTIONAL { MATCH (b)-[:Y]->(c:Common) } OPTIONAL { MATCH (b)-[:Z]->(d:Rare) } RETURN a, c, d",
        )
        .unwrap();
        let mut stats = TableStats {
            vertex_count: 20_000,
            avg_degree: 3.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.label_cardinality.insert("Common".into(), 10_000);
        stats.label_cardinality.insert("Rare".into(), 10);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        let order = plan.annotations.match_clause_order.unwrap();
        // First clause stays 0; both OPTIONAL have equal penalty (1e12),
        // but among them Rare (idx 2) scores lower than Common (idx 1).
        assert_eq!(order, vec![0, 2, 1]);
    }

    #[test]
    fn match_clause_order_with_optional() {
        // Single OPTIONAL MATCH after initial MATCH.
        let stmt = parse_statement(
            "MATCH (a:User)-[:X]->(b) OPTIONAL { MATCH (b)-[:Y]->(c) } RETURN a, c",
        )
        .unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 2.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 100);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        let order = plan.annotations.match_clause_order.unwrap();
        assert_eq!(order, vec![0, 1]);
    }

    #[test]
    fn match_clause_order_none_for_single_clause() {
        let stmt = parse_statement("MATCH (a:User)-[:X]->(b) RETURN b").unwrap();
        let plan = build_plan(&stmt).unwrap();
        assert!(plan.annotations.match_clause_order.is_none());
    }

    #[test]
    fn edge_property_filter_reduces_scan() {
        // With a FilterEdge op in the plan, the cost model should produce
        // fewer estimated output rows than without one.
        let stmt_with_prop =
            parse_statement("MATCH (a:User)-[e:X {weight: 5}]->(b) RETURN b").unwrap();
        let stmt_no_prop = parse_statement("MATCH (a:User)-[e:X]->(b) RETURN b").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 100);
        let plan_with = build_plan_with_stats(&stmt_with_prop, Some(&stats)).unwrap();
        let plan_no = build_plan_with_stats(&stmt_no_prop, Some(&stats)).unwrap();
        let rows_with = plan_with.annotations.estimated_rows.unwrap();
        let rows_no = plan_no.annotations.estimated_rows.unwrap();
        // FilterEdge multiplies rows by 0.25, so the filtered plan should have fewer rows.
        assert!(
            rows_with < rows_no,
            "FilterEdge should reduce rows: with={rows_with}, without={rows_no}"
        );
    }

    // ── Conditional index scan tests ──

    #[test]
    fn detect_optional_filter_basic() {
        let stmt = parse_statement("MATCH (u:User) WHERE $name IS NULL OR u.name = $name RETURN u")
            .unwrap();
        let Statement::Query(q) = &stmt else { panic!() };
        let filters = detect_optional_filters(q.where_clause.as_ref());
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].param_name, "name");
        assert_eq!(filters[0].variable, "u");
        assert_eq!(filters[0].property, "name");
        assert_eq!(filters[0].cmp_op, ConditionalCmpOp::Eq);
    }

    #[test]
    fn detect_optional_filter_reversed_or() {
        let stmt = parse_statement("MATCH (u:User) WHERE u.name = $name OR $name IS NULL RETURN u")
            .unwrap();
        let Statement::Query(q) = &stmt else { panic!() };
        let filters = detect_optional_filters(q.where_clause.as_ref());
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].param_name, "name");
    }

    #[test]
    fn detect_optional_filter_reversed_eq() {
        let stmt = parse_statement("MATCH (u:User) WHERE $name IS NULL OR $name = u.name RETURN u")
            .unwrap();
        let Statement::Query(q) = &stmt else { panic!() };
        let filters = detect_optional_filters(q.where_clause.as_ref());
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].param_name, "name");
    }

    #[test]
    fn detect_optional_filter_nested_and() {
        let stmt = parse_statement(
            "MATCH (u:User) WHERE ($name IS NULL OR u.name = $name) AND ($age IS NULL OR u.age = $age) RETURN u",
        )
        .unwrap();
        let Statement::Query(q) = &stmt else { panic!() };
        let filters = detect_optional_filters(q.where_clause.as_ref());
        assert_eq!(filters.len(), 2);
        let names: Vec<_> = filters.iter().map(|f| f.param_name.as_str()).collect();
        assert!(names.contains(&"name"));
        assert!(names.contains(&"age"));
    }

    #[test]
    fn detect_optional_filter_no_match_literal() {
        let stmt = parse_statement("MATCH (u:User) WHERE u.name = 'Alice' RETURN u").unwrap();
        let Statement::Query(q) = &stmt else { panic!() };
        assert!(detect_optional_filters(q.where_clause.as_ref()).is_empty());
    }

    #[test]
    fn detect_optional_filter_no_match_param_mismatch() {
        let stmt =
            parse_statement("MATCH (u:User) WHERE $x IS NULL OR u.name = $y RETURN u").unwrap();
        let Statement::Query(q) = &stmt else { panic!() };
        assert!(detect_optional_filters(q.where_clause.as_ref()).is_empty());
    }

    #[test]
    fn plan_emits_conditional_index_scan() {
        let stmt = parse_statement("MATCH (u:User) WHERE $name IS NULL OR u.name = $name RETURN u")
            .unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.indexed_vertex_properties.insert("name".into());
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::ConditionalIndexScan)),
            "expected ConditionalIndexScan, got {:?}",
            plan.ops.first()
        );
        let cond = plan.annotations.conditional_scan.as_ref().unwrap();
        assert_eq!(cond.candidates.len(), 1);
        assert_eq!(cond.candidates[0].param_name, "name");
        assert_eq!(cond.candidates[0].property, "name");
        assert_eq!(cond.candidates[0].variable, "u");
    }

    #[test]
    fn plan_emits_multi_candidate_conditional_scan() {
        let stmt = parse_statement(
            "MATCH (u:User) WHERE ($name IS NULL OR u.name = $name) AND ($city IS NULL OR u.city = $city) RETURN u",
        )
        .unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.indexed_vertex_properties.insert("name".into());
        stats.indexed_vertex_properties.insert("city".into());
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::ConditionalIndexScan)),
            "expected ConditionalIndexScan, got {:?}",
            plan.ops.first()
        );
        let cond = plan.annotations.conditional_scan.as_ref().unwrap();
        assert_eq!(cond.candidates.len(), 2);
    }

    #[test]
    fn plan_prefers_literal_index_over_conditional() {
        let stmt = parse_statement(
            "MATCH (u:User) WHERE u.name = 'Alice' AND ($age IS NULL OR u.age = $age) RETURN u",
        )
        .unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.indexed_vertex_properties.insert("name".into());
        stats.indexed_vertex_properties.insert("age".into());
        stats
            .property_selectivity
            .insert("vertex:name".into(), 0.01);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::IndexScan)),
            "literal IndexScan should take priority, got {:?}",
            plan.ops.first()
        );
        assert!(plan.annotations.conditional_scan.is_none());
    }

    #[test]
    fn plan_no_conditional_scan_without_index() {
        let stmt = parse_statement("MATCH (u:User) WHERE $name IS NULL OR u.name = $name RETURN u")
            .unwrap();
        let stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        // No indexed properties.
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::NodeScan)),
            "expected NodeScan without index, got {:?}",
            plan.ops.first()
        );
        assert!(plan.annotations.conditional_scan.is_none());
    }

    #[test]
    fn detect_optional_filter_range_ge() {
        let stmt =
            parse_statement("MATCH (u:User) WHERE $min_age IS NULL OR u.age >= $min_age RETURN u")
                .unwrap();
        let Statement::Query(q) = &stmt else { panic!() };
        let filters = detect_optional_filters(q.where_clause.as_ref());
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].param_name, "min_age");
        assert_eq!(filters[0].property, "age");
        assert_eq!(filters[0].cmp_op, ConditionalCmpOp::Ge);
    }

    #[test]
    fn detect_optional_filter_range_lt() {
        let stmt =
            parse_statement("MATCH (u:User) WHERE $max_age IS NULL OR u.age < $max_age RETURN u")
                .unwrap();
        let Statement::Query(q) = &stmt else { panic!() };
        let filters = detect_optional_filters(q.where_clause.as_ref());
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].param_name, "max_age");
        assert_eq!(filters[0].property, "age");
        assert_eq!(filters[0].cmp_op, ConditionalCmpOp::Lt);
    }

    #[test]
    fn detect_optional_filter_range_reversed() {
        // $min_age >= u.age → u.age <= $min_age
        let stmt = parse_statement("MATCH (u:User) WHERE $val IS NULL OR $val >= u.score RETURN u")
            .unwrap();
        let Statement::Query(q) = &stmt else { panic!() };
        let filters = detect_optional_filters(q.where_clause.as_ref());
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].cmp_op, ConditionalCmpOp::Le);
    }

    #[test]
    fn plan_emits_range_conditional_scan() {
        let stmt =
            parse_statement("MATCH (u:User) WHERE $min_age IS NULL OR u.age >= $min_age RETURN u")
                .unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.range_indexed_vertex_properties.insert("age".into());
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(matches!(
            plan.ops.first(),
            Some(PlanOp::ConditionalIndexScan)
        ));
        let cond = plan.annotations.conditional_scan.as_ref().unwrap();
        assert_eq!(cond.candidates.len(), 1);
        assert_eq!(cond.candidates[0].cmp_op, ConditionalCmpOp::Ge);
    }

    #[test]
    fn plan_no_range_scan_without_range_index() {
        // Range pattern but only equality index → no conditional scan.
        let stmt =
            parse_statement("MATCH (u:User) WHERE $min_age IS NULL OR u.age >= $min_age RETURN u")
                .unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.indexed_vertex_properties.insert("age".into()); // equality only
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::NodeScan)),
            "range pattern should not use equality index"
        );
    }

    // ── Phase 4: Direct Range IndexScan tests ──

    #[test]
    fn plan_emits_range_index_scan_ge() {
        let stmt = parse_statement("MATCH (u:User) WHERE u.age >= 30 RETURN u").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.range_indexed_vertex_properties.insert("age".into());
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::IndexScan)),
            "range literal predicate should emit IndexScan, got {:?}",
            plan.ops.first()
        );
        assert_eq!(
            plan.annotations.index_scan_cmp_op,
            Some(ConditionalCmpOp::Ge)
        );
        assert!(
            plan.annotations
                .estimated_cardinality_source
                .as_ref()
                .unwrap()
                .contains("range-index")
        );
    }

    #[test]
    fn plan_emits_range_index_scan_lt() {
        let stmt = parse_statement("MATCH (u:User) WHERE u.age < 18 RETURN u").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.range_indexed_vertex_properties.insert("age".into());
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(matches!(plan.ops.first(), Some(PlanOp::IndexScan)));
        assert_eq!(
            plan.annotations.index_scan_cmp_op,
            Some(ConditionalCmpOp::Lt)
        );
    }

    #[test]
    fn plan_range_index_scan_reversed_operands() {
        // `30 <= u.age` is equivalent to `u.age >= 30`
        let stmt = parse_statement("MATCH (u:User) WHERE 30 <= u.age RETURN u").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.range_indexed_vertex_properties.insert("age".into());
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(matches!(plan.ops.first(), Some(PlanOp::IndexScan)));
        assert_eq!(
            plan.annotations.index_scan_cmp_op,
            Some(ConditionalCmpOp::Ge)
        );
    }

    #[test]
    fn plan_no_range_index_scan_without_range_index() {
        // Range literal predicate but no range index → NodeScan
        let stmt = parse_statement("MATCH (u:User) WHERE u.age >= 30 RETURN u").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.indexed_vertex_properties.insert("age".into()); // equality only
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::NodeScan)),
            "no range index → should fall back to NodeScan"
        );
        assert_eq!(plan.annotations.index_scan_cmp_op, None);
    }

    #[test]
    fn plan_equality_index_scan_has_none_cmp_op() {
        let stmt = parse_statement("MATCH (u:User) WHERE u.name = 'Alice' RETURN u").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.indexed_vertex_properties.insert("name".into());
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(matches!(plan.ops.first(), Some(PlanOp::IndexScan)));
        assert_eq!(plan.annotations.index_scan_cmp_op, None);
    }

    // ── Phase 5: Parameter-Based IndexScan tests ──

    #[test]
    fn plan_emits_index_scan_for_parameter_equality() {
        let stmt = parse_statement("MATCH (u:User) WHERE u.name = $name RETURN u").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.indexed_vertex_properties.insert("name".into());
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::IndexScan)),
            "parameter equality should emit IndexScan, got {:?}",
            plan.ops.first()
        );
        assert_eq!(plan.annotations.index_scan_cmp_op, None);
    }

    #[test]
    fn plan_emits_range_index_scan_for_parameter_range() {
        let stmt = parse_statement("MATCH (u:User) WHERE u.age >= $min RETURN u").unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.range_indexed_vertex_properties.insert("age".into());
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::IndexScan)),
            "parameter range should emit IndexScan, got {:?}",
            plan.ops.first()
        );
        assert_eq!(
            plan.annotations.index_scan_cmp_op,
            Some(ConditionalCmpOp::Ge)
        );
    }

    #[test]
    fn plan_multi_pred_picks_most_selective_anchor() {
        // Two equality predicates: country (low selectivity 0.3) and email (high selectivity 0.001).
        // The planner should pick email as anchor regardless of AST order.
        let stmt = parse_statement(
            "MATCH (n:User) WHERE n.country = 'JP' AND n.email = 'alice@test.com' RETURN n",
        )
        .unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats
            .property_selectivity
            .insert("vertex:country".into(), 0.3);
        stats
            .property_selectivity
            .insert("vertex:email".into(), 0.001);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert_eq!(
            plan.annotations.chosen_anchor.as_deref(),
            Some("n"),
            "should pick n as anchor"
        );
        assert_eq!(
            plan.annotations.estimated_cardinality_source.as_deref(),
            Some("property-equality"),
        );
    }

    #[test]
    fn plan_multi_pred_picks_most_selective_for_index_scan() {
        // Two indexed predicates: country (selectivity 0.3) and email (selectivity 0.001).
        // The planner should use IndexScan with the most selective property.
        let stmt = parse_statement(
            "MATCH (n:User) WHERE n.country = 'JP' AND n.email = 'alice@test.com' RETURN n",
        )
        .unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.indexed_vertex_properties.insert("country".into());
        stats.indexed_vertex_properties.insert("email".into());
        stats
            .property_selectivity
            .insert("vertex:country".into(), 0.3);
        stats
            .property_selectivity
            .insert("vertex:email".into(), 0.001);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::IndexScan)),
            "should emit IndexScan, got {:?}",
            plan.ops.first()
        );
    }

    #[test]
    fn plan_multi_pred_reversed_order_same_result() {
        // Same as above but email appears first in the AST. Result should be identical.
        let stmt = parse_statement(
            "MATCH (n:User) WHERE n.email = 'alice@test.com' AND n.country = 'JP' RETURN n",
        )
        .unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.indexed_vertex_properties.insert("country".into());
        stats.indexed_vertex_properties.insert("email".into());
        stats
            .property_selectivity
            .insert("vertex:country".into(), 0.3);
        stats
            .property_selectivity
            .insert("vertex:email".into(), 0.001);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(
            matches!(plan.ops.first(), Some(PlanOp::IndexScan)),
            "should emit IndexScan regardless of AST order, got {:?}",
            plan.ops.first()
        );
    }

    #[test]
    fn plan_conditional_candidates_sorted_by_selectivity() {
        // Two conditional candidates: country (sel 0.3) first in AST, email (sel 0.001) second.
        // Phase 11 should reorder so email comes first.
        let stmt = parse_statement(
            "MATCH (n:User) WHERE ($country IS NULL OR n.country = $country) AND ($email IS NULL OR n.email = $email) RETURN n",
        )
        .unwrap();
        let mut stats = TableStats {
            vertex_count: 1_000,
            avg_degree: 4.0,
            ..Default::default()
        };
        stats.label_cardinality.insert("User".into(), 500);
        stats.indexed_vertex_properties.insert("country".into());
        stats.indexed_vertex_properties.insert("email".into());
        stats
            .property_selectivity
            .insert("vertex:country".into(), 0.3);
        stats
            .property_selectivity
            .insert("vertex:email".into(), 0.001);
        let plan = build_plan_with_stats(&stmt, Some(&stats)).unwrap();
        assert!(matches!(
            plan.ops.first(),
            Some(PlanOp::ConditionalIndexScan)
        ));
        let cond = plan.annotations.conditional_scan.as_ref().unwrap();
        assert_eq!(cond.candidates.len(), 2);
        // email should come first (lower selectivity).
        assert_eq!(
            cond.candidates[0].property, "email",
            "most selective candidate should be first"
        );
        assert_eq!(cond.candidates[1].property, "country");
    }
}
