//! Core planner: converts a GQL AST into a [`PhysicalPlan`].
//!
//! The planner walks a [`LinearQueryStatement`] and emits a sequence of
//! [`PlanOp`] operators, choosing scans, expansions, filters, projections,
//! and aggregations based on the query structure and optional statistics.

use gleaph_gql::ast::*;
use gleaph_gql::type_check::{
    dml_diagnostic_from_warning, infer_linear_query_binding_kinds,
    infer_linear_query_binding_kinds_with_seed, infer_statement_block_binding_kinds,
    type_check_composite_query, type_check_linear_query, type_check_statement,
    type_check_statement_block, type_diagnostic_from_warning, BindingKind,
    DmlDiagnosticSeverity, TypeWarning,
};

use crate::anchor::{self, extract_simple_label};
use crate::cost;
use crate::cse;
use crate::explain::explain_plan;
use crate::join_order;
use crate::plan::*;
use crate::pushdown;
use crate::semantic::{self, SemanticAnalysis, SemanticConstraint};
use crate::stats::GraphStats;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannerError {
    FatalDml(PlannerDiagnostic),
}

impl std::fmt::Display for PlannerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FatalDml(diagnostic) => write!(
                f,
                "fatal DML diagnostic [{}] at {}..{}: {}",
                diagnostic.code,
                diagnostic.span.start,
                diagnostic.span.end,
                diagnostic.message
            ),
        }
    }
}

impl std::error::Error for PlannerError {}

#[derive(Clone, Debug)]
pub struct PlanBuildOutput {
    pub plan: PhysicalPlan,
    pub summary: PlanSummary,
    pub explain: String,
}

impl PlanBuildOutput {
    fn from_plan(plan: PhysicalPlan) -> Self {
        let summary = PlanSummary::from_plan(&plan);
        let explain = explain_plan(&plan);
        Self {
            plan,
            summary,
            explain,
        }
    }
}

/// Build a physical plan from a top-level statement.
///
/// Handles both query statements (`Statement::Query`) and DML statements
/// (`Statement::Insert/Set/Remove/Delete`).
pub fn build_statement_plan(
    stmt: &Statement,
    stats: Option<&dyn GraphStats>,
) -> Result<PhysicalPlan, PlannerError> {
    let mut plan = build_statement_plan_with_binding_kinds(stmt, stats, None);
    apply_type_checker_dml_diagnostics(&mut plan.diagnostics, &type_check_statement(stmt));
    validate_plan(plan)
}

pub fn build_statement_plan_output(
    stmt: &Statement,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_statement_plan(stmt, stats).map(PlanBuildOutput::from_plan)
}

fn build_statement_plan_with_binding_kinds(
    stmt: &Statement,
    stats: Option<&dyn GraphStats>,
    binding_kinds: Option<&std::collections::BTreeMap<String, BindingKind>>,
) -> PhysicalPlan {
    match stmt {
        Statement::Query(composite) => build_composite_plan_with_binding_kinds(composite, stats, binding_kinds),
        Statement::Insert(insert_stmt) => {
            let mut plan = PhysicalPlan::default();
            plan_insert(insert_stmt, &mut plan.ops, &mut plan.annotations);
            plan.annotations.optimizer.estimated_cost = Some(cost::estimate_cost(&plan.ops, stats));
            plan.annotations.optimizer.estimated_rows = Some(cost::estimate_rows(&plan.ops, stats));
            plan
        }
        _ => PhysicalPlan::default(), // TODO: DDL, Session
    }
}

/// Build a physical plan from a full statement block, handling NEXT chains.
///
/// This processes `StatementBlock` which may contain NEXT-chained statements
/// with optional YIELD clauses that act as pipeline boundaries.
pub fn build_block_plan(
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
) -> Result<PhysicalPlan, PlannerError> {
    let binding_kinds = infer_statement_block_binding_kinds(block);

    // Plan the first statement.
    let mut plan = build_statement_plan_with_binding_kinds(&block.first, stats, binding_kinds.first());

    // Process NEXT chains.
    for next in &block.next {
        // Emit Materialize if YIELD is present.
        if let Some(yield_items) = &next.yield_items {
            let columns: Vec<ProjectColumn> = yield_items
                .iter()
                .map(|yi| ProjectColumn {
                    expr: Expr::new(ExprKind::Variable(yi.name.clone())),
                    alias: yi.alias.as_ref().map(|a| Str::from(a.as_str())),
                })
                .collect();
            plan.ops.push(PlanOp::Materialize {
                columns,
                distinct: false,
            });
        }

        // Plan the chained statement and merge its ops.
        let chained = build_statement_plan_with_binding_kinds(
            &next.statement,
            stats,
            binding_kinds.get(index_for_next(&block.next, next)),
        );
        plan.ops.extend(chained.ops);

        // Merge annotations and update cost.
        plan.diagnostics
            .dml_errors
            .extend(chained.diagnostics.dml_errors);
        plan.diagnostics
            .dml_warnings
            .extend(chained.diagnostics.dml_warnings);
        plan.diagnostics
            .type_warnings
            .extend(chained.diagnostics.type_warnings);
    }

    // Re-estimate cost over the full plan.
    plan.annotations.optimizer.estimated_cost = Some(cost::estimate_cost(&plan.ops, stats));
    plan.annotations.optimizer.estimated_rows = Some(cost::estimate_rows(&plan.ops, stats));
    apply_type_checker_dml_diagnostics(&mut plan.diagnostics, &type_check_statement_block(block));
    validate_plan(plan)
}

pub fn build_block_plan_output(
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_block_plan(block, stats).map(PlanBuildOutput::from_plan)
}

/// Build a physical plan from a linear query statement.
pub fn build_plan(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
) -> Result<PhysicalPlan, PlannerError> {
    let mut plan = build_plan_with_binding_kinds(query, stats, None);
    apply_type_checker_dml_diagnostics(&mut plan.diagnostics, &type_check_linear_query(query));
    validate_plan(plan)
}

pub fn build_plan_output(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_plan(query, stats).map(PlanBuildOutput::from_plan)
}

fn build_plan_with_binding_kinds(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
    seed_binding_kinds: Option<&std::collections::BTreeMap<String, BindingKind>>,
) -> PhysicalPlan {
    let binding_kinds = match seed_binding_kinds {
        Some(seed) => infer_linear_query_binding_kinds_with_seed(query, &gleaph_gql::type_check::NoSchema, seed),
        None => infer_linear_query_binding_kinds(query),
    };

    // Phase 1: Semantic analysis.
    let semantic = semantic::analyze(query);

    // Pre-allocate: each query part typically produces 2-3 ops.
    let mut ops = Vec::with_capacity(query.parts.len() * 3 + 4);
    let mut annotations = PlanAnnotations::default();

    // Populate semantic annotations.
    populate_semantic_annotations(&semantic, &mut annotations, stats);

    // Detect conditional index scan candidates from semantic analysis.
    let conditional_candidates = detect_conditional_candidates(&semantic, stats);

    // Check for independent MATCH groups (bushy join opportunity).
    let groups = detect_independent_match_groups(&query.parts);
    if groups.len() > 1 {
        // Bushy join: build sub-plans for each group, then join.
        plan_bushy_join(
            &groups,
            &query.parts,
            stats,
            &conditional_candidates,
            &binding_kinds,
            &mut ops,
            &mut annotations,
        );
    } else {
        // Sequential: process all parts in order (default behavior).
        for (stage, part) in query.parts.iter().enumerate() {
            plan_simple_statement(
                part,
                stage,
                stats,
                &conditional_candidates,
                &binding_kinds,
                &mut ops,
                &mut annotations,
            );
        }
    }

    // Process the result statement (RETURN / SELECT).
    if let Some(result) = &query.result {
        plan_result_statement(result, &mut ops);
    }

    // Phase 2: Optimizations.
    pushdown::apply_filter_pushdown(&mut ops, &mut annotations);
    pushdown::apply_predicate_reordering(&mut ops, &mut annotations, stats);
    pushdown::apply_ev_fusion(&mut ops, &mut annotations);
    pushdown::apply_late_project(&mut ops, &mut annotations);
    pushdown::apply_limit_pushdown(&mut ops, &mut annotations);
    pushdown::apply_topk_fusion(&mut ops, &mut annotations);
    apply_wcoj_replacement(&mut ops, &mut annotations);

    // Phase 2b: Annotation-only analysis.
    cse::detect_common_subexpressions(&ops, &mut annotations);
    set_reoptimization_hints(&ops, &mut annotations, stats);

    // Phase 3: Cost estimation.
    annotations.optimizer.estimated_cost = Some(cost::estimate_cost(&ops, stats));
    annotations.optimizer.estimated_rows = Some(cost::estimate_rows(&ops, stats));

    PhysicalPlan {
        ops,
        diagnostics: PlanDiagnostics::default(),
        annotations,
    }
}

/// Populate plan annotations from semantic analysis.
fn populate_semantic_annotations(
    semantic: &SemanticAnalysis,
    annotations: &mut PlanAnnotations,
    stats: Option<&dyn GraphStats>,
) {
    // Collect property accesses.
    let mut all_props: Vec<Str> = Vec::new();
    let mut where_props: Vec<Str> = Vec::new();
    let mut indexable_props: Vec<Str> = Vec::new();
    let mut has_aggregate = false;

    for constraint in &semantic.constraints {
        match constraint {
            SemanticConstraint::PropertyAccess {
                var,
                property,
                in_where,
            } => {
                let key: Str = format!("{}.{}", var, property).into();
                all_props.push(key.clone());
                if *in_where {
                    where_props.push(key);
                }
            }
            SemanticConstraint::WhereEqualityPredicate {
                var, property, ..
            } => {
                if let Some(stats) = stats
                    && stats.is_vertex_property_indexed(property) {
                        indexable_props.push(format!("{}.{}", var, property).into());
                    }
            }
            SemanticConstraint::AggregateCall { .. } => {
                has_aggregate = true;
            }
            _ => {}
        }
    }

    annotations.semantic.property_accesses = if all_props.is_empty() {
        None
    } else {
        Some(all_props)
    };
    annotations.semantic.where_property_accesses = if where_props.is_empty() {
        None
    } else {
        Some(where_props)
    };
    annotations.semantic.indexable_properties = if indexable_props.is_empty() {
        None
    } else {
        Some(indexable_props)
    };
    annotations.semantic.has_aggregate = has_aggregate;

    // Copy narrowing facts.
    if !semantic.narrowing_facts.is_empty() {
        annotations.semantic.narrowing_facts = Some(semantic.narrowing_facts.clone());
    }
}

/// Build a plan from a composite query expression (handles set operations).
pub fn build_composite_plan(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
) -> Result<PhysicalPlan, PlannerError> {
    let mut plan = build_composite_plan_with_binding_kinds(composite, stats, None);
    apply_type_checker_dml_diagnostics(&mut plan.diagnostics, &type_check_composite_query(composite));
    validate_plan(plan)
}

pub fn build_composite_plan_output(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_composite_plan(composite, stats).map(PlanBuildOutput::from_plan)
}

fn build_composite_plan_with_binding_kinds(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
    seed_binding_kinds: Option<&std::collections::BTreeMap<String, BindingKind>>,
) -> PhysicalPlan {
    let mut plan = build_plan_with_binding_kinds(&composite.left, stats, seed_binding_kinds);

    // Append set operations (UNION, EXCEPT, INTERSECT, OTHERWISE).
    for (set_op, right_query) in &composite.rest {
        let right_plan = build_plan_with_binding_kinds(right_query, stats, seed_binding_kinds);
        plan.ops.push(PlanOp::SetOperation {
            op: *set_op,
            right: Box::new(right_plan),
        });
    }

    plan
}

fn validate_plan(plan: PhysicalPlan) -> Result<PhysicalPlan, PlannerError> {
    if let Some(error) = plan.diagnostics.dml_errors.first() {
        Err(PlannerError::FatalDml(error.clone()))
    } else {
        Ok(plan)
    }
}

fn apply_type_checker_dml_diagnostics(
    diagnostics: &mut PlanDiagnostics,
    warnings: &[TypeWarning],
) {
    for warning in warnings {
        if let Some(dml) = dml_diagnostic_from_warning(warning) {
            match dml.severity {
                DmlDiagnosticSeverity::Fatal => {
                    diagnostics.dml_errors.push(dml);
                }
                DmlDiagnosticSeverity::Warning => {
                    diagnostics.dml_warnings.push(dml);
                }
            }
        } else {
            diagnostics
                .type_warnings
                .push(type_diagnostic_from_warning(warning));
        }
    }
}

fn index_for_next(nexts: &[NextStatement], current: &NextStatement) -> usize {
    nexts
        .iter()
        .position(|item| std::ptr::eq(item, current))
        .map(|idx| idx + 1)
        .expect("next statement belongs to block")
}

// ════════════════════════════════════════════════════════════════════════════════
// Internal planning functions
// ════════════════════════════════════════════════════════════════════════════════

/// Detect conditional index scan candidates from semantic analysis.
fn detect_conditional_candidates(
    semantic: &SemanticAnalysis,
    stats: Option<&dyn GraphStats>,
) -> Vec<ConditionalScanCandidate> {
    let mut candidates = Vec::new();
    for constraint in &semantic.constraints {
        if let SemanticConstraint::OptionalFilterPredicate {
            param_name,
            var,
            property,
            op,
        } = constraint
        {
            // Only consider if the property is indexed.
            let indexed = stats
                .map(|s| s.is_vertex_property_indexed(property))
                .unwrap_or(false);
            if indexed {
                candidates.push(ConditionalScanCandidate {
                    param_name: param_name.clone().into(),
                    property: property.clone().into(),
                    variable: var.clone().into(),
                    cmp: *op,
                });
            }
        }
    }
    // Sort by selectivity (equality first, then range).
    candidates.sort_by(|a, b| {
        let a_eq = a.cmp == CmpOp::Eq;
        let b_eq = b.cmp == CmpOp::Eq;
        b_eq.cmp(&a_eq)
    });
    candidates
}

fn plan_simple_statement(
    stmt: &SimpleQueryStatement,
    stage: usize,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) {
    match stmt {
        SimpleQueryStatement::Match(m) => plan_match(m, stage, stats, conditional_candidates, ops, annotations),
        SimpleQueryStatement::Filter(f) => {
            ops.push(PlanOp::Filter {
                condition: f.condition.clone(),
            });
        }
        SimpleQueryStatement::Let(l) => {
            ops.push(PlanOp::Let {
                bindings: l.bindings.clone(),
            });
        }
        SimpleQueryStatement::For(f) => {
            ops.push(PlanOp::For {
                variable: f.variable.clone().into(),
                list: f.list.clone(),
                ordinality: f.ordinality.as_ref().map(|o| o.variable.clone().into()),
            });
        }
        SimpleQueryStatement::OrderBy(o) => {
            ops.push(PlanOp::Sort {
                order_by: o.clone(),
            });
        }
        SimpleQueryStatement::Limit(l) => {
            ops.push(PlanOp::Limit {
                count: Some(l.count.clone()),
                offset: None,
            });
        }
        SimpleQueryStatement::Offset(o) => {
            // Merge with preceding Limit if possible, otherwise standalone.
            if let Some(PlanOp::Limit { offset, .. }) = ops.last_mut() {
                *offset = Some(o.count.clone());
            } else {
                ops.push(PlanOp::Limit {
                    count: None,
                    offset: Some(o.count.clone()),
                });
            }
        }
        SimpleQueryStatement::Insert(insert_stmt) => {
            plan_insert(insert_stmt, ops, annotations);
        }
        SimpleQueryStatement::Set(set_stmt) => {
            plan_set(set_stmt, binding_kinds, ops, annotations);
        }
        SimpleQueryStatement::Remove(remove_stmt) => {
            plan_remove(remove_stmt, binding_kinds, ops, annotations);
        }
        SimpleQueryStatement::Delete(delete_stmt) => {
            plan_delete(delete_stmt, binding_kinds, ops, annotations);
        }
        SimpleQueryStatement::CallProcedure(call) => {
            let yield_columns = call.yield_items.as_ref().map(|items| {
                items
                    .iter()
                    .map(|yi| YieldColumn {
                        name: yi.name.clone().into(),
                        alias: yi.alias.as_ref().map(|a| Str::from(a.as_str())),
                    })
                    .collect()
            });
            ops.push(PlanOp::CallProcedure {
                name: call.name.parts.iter().map(|s| Str::from(s.as_str())).collect(),
                args: call.args.clone(),
                yield_columns,
                optional: call.optional,
            });
        }
        SimpleQueryStatement::InlineProcedureCall(inline) => {
            let mut sub_plan = build_composite_plan_with_binding_kinds(&inline.body, stats, None);
            if let Some(graph) = &inline.use_graph {
                let wrapped_ops = std::mem::take(&mut sub_plan.ops);
                sub_plan.ops = vec![PlanOp::UseGraph {
                    graph_name: graph.parts.iter().map(|s| Str::from(s.as_str())).collect(),
                    sub_plan: Some(wrapped_ops),
                }];
            }
            ops.push(PlanOp::InlineProcedureCall {
                sub_plan: Box::new(sub_plan),
                scope_vars: inline.scope_vars.iter().map(|s| Str::from(s.as_str())).collect(),
                optional: inline.optional,
            });
        }
        SimpleQueryStatement::Focused { graph, body } => {
            if let Some(inner) = body {
                let mut sub_ops = Vec::new();
                plan_simple_statement(
                    inner,
                    stage,
                    stats,
                    conditional_candidates,
                    binding_kinds,
                    &mut sub_ops,
                    annotations,
                );
                ops.push(PlanOp::UseGraph {
                    graph_name: graph.parts.iter().map(|s| Str::from(s.as_str())).collect(),
                    sub_plan: Some(sub_ops),
                });
            } else {
                ops.push(PlanOp::UseGraph {
                    graph_name: graph.parts.iter().map(|s| Str::from(s.as_str())).collect(),
                    sub_plan: None,
                });
            }
        }
    }
}

fn plan_match(
    match_stmt: &MatchStatement,
    stage: usize,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) {
    let pattern = &match_stmt.pattern;

    if match_stmt.optional {
        // OPTIONAL MATCH: build sub-plan and wrap in OptionalMatch.
        let mut sub_ops = Vec::new();
        for path_pattern in &pattern.paths {
            plan_path_pattern(path_pattern, stats, conditional_candidates, &mut sub_ops, annotations);
        }
        if let Some(where_expr) = &pattern.where_clause {
            sub_ops.push(PlanOp::PropertyFilter {
                predicates: flatten_conjunction(where_expr),
                stage,
            });
        }
        ops.push(PlanOp::OptionalMatch { sub_plan: sub_ops });
        return;
    }

    // Choose anchor for this match.
    if stage == 0
        && let Some(anchor_info) = anchor::choose_anchor(pattern, stats) {
            annotations.optimizer.anchor = Some(anchor_info);
        }

    // Plan each path pattern.
    for path_pattern in &pattern.paths {
        plan_path_pattern(path_pattern, stats, conditional_candidates, ops, annotations);
    }

    // Plan WHERE clause as a PropertyFilter.
    if let Some(where_expr) = &pattern.where_clause {
        ops.push(PlanOp::PropertyFilter {
            predicates: flatten_conjunction(where_expr),
            stage,
        });
    }
}

fn plan_path_pattern(
    path: &PathPattern,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) {
    // Check for shortest-path prefix.
    let shortest_mode = path.prefix.as_ref().and_then(|p| match p {
        PathPatternPrefix::Search(search) => match search {
            SearchPrefix::AnyShortest { .. } => Some(ShortestMode::AnyShortest),
            SearchPrefix::AllShortest { .. } => Some(ShortestMode::AllShortest),
            SearchPrefix::ShortestK { k, .. } => Some(ShortestMode::ShortestK(*k)),
            _ => None,
        },
        _ => None,
    });

    // Walk the path expression to emit scan/expand ops.
    plan_path_expr(&path.expr, shortest_mode, stats, conditional_candidates, ops, annotations);
}

fn plan_path_expr(
    expr: &PathPatternExpr,
    shortest_mode: Option<ShortestMode>,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) {
    match expr {
        PathPatternExpr::Term(term) => {
            plan_path_term(term, shortest_mode, stats, conditional_candidates, ops, annotations);
        }
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            if let Some(term) = terms.first() {
                plan_path_term(term, shortest_mode, stats, conditional_candidates, ops, annotations);
            }
        }
    }
}

fn plan_path_term(
    term: &PathTerm,
    shortest_mode: Option<ShortestMode>,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) {
    // Compute join ordering and detect cyclic patterns.
    let hops = join_order::extract_hops(term);
    if hops.len() > 1 {
        let order = join_order::greedy_join_order(&hops, stats);
        if order != (0..hops.len()).collect::<Vec<_>>() {
            annotations.optimizer.join_order = Some(order);
        }
    }
    if !hops.is_empty() {
        // Determine the first node variable for cycle detection.
        let first_node_var = term
            .factors
            .iter()
            .find_map(|f| match &f.primary {
                PathPrimary::Node(n) => n.variable.clone(),
                _ => None,
            })
            .unwrap_or_default();
        let cycles = join_order::detect_cyclic_patterns(&hops, &first_node_var);
        if !cycles.is_empty() {
            annotations.optimizer.cyclic_patterns = Some(cycles);
        }
    }

    // Pre-extract node/edge elements with their variables for lookahead.
    // A GQL path term alternates: Node, Edge, Node, Edge, Node, ...
    // We collect all elements first so edges can resolve their dst node.
    let elements: Vec<PathElement> = term
        .factors
        .iter()
        .enumerate()
        .map(|(i, factor)| match &factor.primary {
            PathPrimary::Node(node) => {
                let var = node
                    .variable
                    .clone()
                    .unwrap_or_else(|| format!("__anon_n{}", i));
                PathElement::Node {
                    var,
                    node: node.clone(),
                }
            }
            PathPrimary::Edge(edge) => {
                let var = edge
                    .variable
                    .clone()
                    .unwrap_or_else(|| format!("__anon_e{}", i));
                PathElement::Edge {
                    var,
                    edge: edge.clone(),
                    quantifier: factor.quantifier.clone(),
                }
            }
            PathPrimary::Parenthesized { expr, .. } => PathElement::Sub(expr.as_ref().clone()),
            PathPrimary::Simplified(_) => PathElement::Simplified,
        })
        .collect();

    let mut prev_node_var: Option<String> = None;
    // Track nodes whose inline filters were fused into ExpandFilter.
    let mut fused_nodes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for (idx, elem) in elements.iter().enumerate() {
        match elem {
            PathElement::Node { var, node } => {
                let label = extract_simple_label(&node.label);

                // First node: emit a scan.
                if prev_node_var.is_none() && !has_scan(ops) {
                    emit_scan_for_node(var, &label, node, stats, conditional_candidates, ops, annotations);
                }

                // Emit inline filters only if they weren't already fused.
                if !fused_nodes.contains(var) {
                    emit_node_inline_filters(var, node, ops);
                }

                prev_node_var = Some(var.clone());
            }
            PathElement::Edge {
                var: edge_var,
                edge,
                quantifier,
            } => {
                let edge_label = extract_simple_label(&edge.label);

                if let Some(src_var) = &prev_node_var {
                    // Lookahead: find the next node and its variable.
                    let (dst_var, dst_node) = elements[idx + 1..]
                        .iter()
                        .find_map(|e| match e {
                            PathElement::Node { var, node } => Some((var.clone(), Some(node))),
                            _ => None,
                        })
                        .unwrap_or_else(|| (format!("__anon_dst_{}", idx), None));

                    let var_len = quantifier.as_ref().and_then(quantifier_to_var_len);

                    // FilterIntoPattern: collect dst node's inline filters for fusion.
                    let dst_filters = dst_node
                        .map(|n| collect_node_inline_predicates(&dst_var, n))
                        .unwrap_or_default();

                    let src_str: Str = src_var.as_str().into();
                    let edge_str: Str = edge_var.as_str().into();
                    let dst_str: Str = dst_var.as_str().into();
                    let label_str: Option<Str> = edge_label.map(Str::from);

                    if let Some(mode) = shortest_mode {
                        ops.push(PlanOp::ShortestPath {
                            src: src_str,
                            dst: dst_str.clone(),
                            edge: edge_str,
                            path_var: None,
                            mode,
                        });
                        // Cannot fuse filters into shortest path; emit separately.
                        if !dst_filters.is_empty() {
                            ops.push(PlanOp::PropertyFilter {
                                predicates: dst_filters,
                                stage: 0,
                            });
                        }
                    } else if !dst_filters.is_empty() {
                        // Emit ExpandFilter (fused) — FilterIntoPattern optimization.
                        ops.push(PlanOp::ExpandFilter {
                            src: src_str,
                            edge: edge_str,
                            dst: dst_str.clone(),
                            direction: edge.direction,
                            label: label_str,
                            var_len,
                            dst_filter: dst_filters,
                        });
                        fused_nodes.insert(dst_var.clone());
                    } else {
                        ops.push(PlanOp::Expand {
                            src: src_str,
                            edge: edge_str,
                            dst: dst_str,
                            direction: edge.direction,
                            label: label_str,
                            var_len,
                        });
                    }

                    // Emit inline edge filters.
                    emit_edge_inline_filters(edge_var, edge, ops);
                }
                prev_node_var = None;
            }
            PathElement::Sub(expr) => {
                plan_path_expr(expr, shortest_mode, stats, conditional_candidates, ops, annotations);
            }
            PathElement::Simplified => {}
        }
    }
}

/// Pre-extracted path element for lookahead during planning.
enum PathElement {
    Node {
        var: String,
        node: NodePattern,
    },
    Edge {
        var: String,
        edge: EdgePattern,
        quantifier: Option<PathQuantifier>,
    },
    Sub(PathPatternExpr),
    Simplified,
}

/// Collect all inline predicates from a node pattern (properties + WHERE clause)
/// without emitting them as PlanOps. Used by FilterIntoPattern to fuse into ExpandFilter.
fn collect_node_inline_predicates(var: &str, node: &NodePattern) -> Vec<Expr> {
    let mut preds = Vec::new();

    for p in &node.properties {
        preds.push(Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::new(ExprKind::Variable(var.to_string()))),
                property: p.name.clone(),
            })),
            op: CmpOp::Eq,
            right: Box::new(p.value.clone()),
        }));
    }

    if let Some(where_expr) = &node.where_clause {
        preds.extend(flatten_conjunction(where_expr));
    }

    preds
}

fn quantifier_to_var_len(q: &PathQuantifier) -> Option<VarLenSpec> {
    match q {
        PathQuantifier::Star => Some(VarLenSpec { min: 0, max: None }),
        PathQuantifier::Plus => Some(VarLenSpec { min: 1, max: None }),
        PathQuantifier::Optional => Some(VarLenSpec {
            min: 0,
            max: Some(1),
        }),
        PathQuantifier::Fixed(n) => Some(VarLenSpec {
            min: *n,
            max: Some(*n),
        }),
        PathQuantifier::Range { lower, upper } => Some(VarLenSpec {
            min: *lower,
            max: *upper,
        }),
    }
}

fn emit_node_inline_filters(var: &str, node: &NodePattern, ops: &mut Vec<PlanOp>) {
    if !node.properties.is_empty() {
        let filter_exprs: Vec<Expr> = node
            .properties
            .iter()
            .map(|p| {
                Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable(var.to_string()))),
                        property: p.name.clone(),
                    })),
                    op: CmpOp::Eq,
                    right: Box::new(p.value.clone()),
                })
            })
            .collect();
        ops.push(PlanOp::PropertyFilter {
            predicates: filter_exprs,
            stage: 0,
        });
    }

    if let Some(where_expr) = &node.where_clause {
        ops.push(PlanOp::PropertyFilter {
            predicates: flatten_conjunction(where_expr),
            stage: 0,
        });
    }
}

fn emit_edge_inline_filters(edge_var: &str, edge: &EdgePattern, ops: &mut Vec<PlanOp>) {
    if !edge.properties.is_empty() {
        let filter_exprs: Vec<Expr> = edge
            .properties
            .iter()
            .map(|p| {
                Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable(edge_var.to_string()))),
                        property: p.name.clone(),
                    })),
                    op: CmpOp::Eq,
                    right: Box::new(p.value.clone()),
                })
            })
            .collect();
        ops.push(PlanOp::PropertyFilter {
            predicates: filter_exprs,
            stage: 0,
        });
    }

    if let Some(where_expr) = &edge.where_clause {
        ops.push(PlanOp::PropertyFilter {
            predicates: flatten_conjunction(where_expr),
            stage: 0,
        });
    }
}

fn emit_scan_for_node(
    var: &str,
    label: &Option<String>,
    node: &NodePattern,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) {
    // Check for index intersection opportunity (multiple indexed predicates).
    if let Some(stats) = stats
        && let Some(where_expr) = &node.where_clause
            && let Some(specs) = anchor::find_index_intersection(var, where_expr, stats) {
                ops.push(PlanOp::IndexIntersection {
                    variable: Str::from(var),
                    scans: specs,
                });
                return;
            }

    // Check if anchor selection found an index scan for this variable.
    if let Some(anchor) = &annotations.optimizer.anchor
        && &*anchor.variable == var {
            match &anchor.source {
                AnchorSource::PropertyEquality { property }
                | AnchorSource::InlinePropertyEquality { property } => {
                    // Find the value from inline properties or inline WHERE.
                    let scan_value = node
                        .properties
                        .iter()
                        .find(|p| p.name == **property)
                        .map(|p| expr_to_scan_value(&p.value))
                        .or_else(|| {
                            // Try inline WHERE: (n WHERE n.prop = value)
                            node.where_clause.as_ref().and_then(|w| {
                                find_equality_value_in_where(var, property, w)
                            })
                        })
                        .unwrap_or(ScanValue::Parameter(format!("${}", property).into()));

                    ops.push(PlanOp::IndexScan {
                        variable: Str::from(var),
                        property: property.clone(),
                        value: scan_value,
                        cmp: CmpOp::Eq,
                    });
                    return;
                }
                AnchorSource::PropertyRange { property } => {
                    ops.push(PlanOp::IndexScan {
                        variable: Str::from(var),
                        property: property.clone(),
                        value: ScanValue::Parameter(format!("${}", property).into()),
                        cmp: CmpOp::Ge,
                    });
                    return;
                }
                _ => {}
            }
        }

    // Check for conditional index scan candidates.
    let var_candidates: Vec<_> = conditional_candidates
        .iter()
        .filter(|c| &*c.variable == var)
        .cloned()
        .collect();
    if !var_candidates.is_empty() {
        ops.push(PlanOp::ConditionalIndexScan {
            candidates: var_candidates,
            fallback_label: label.as_ref().map(|s| Str::from(s.as_str())),
            fallback_variable: Str::from(var),
        });
        return;
    }

    // Default: NodeScan.
    ops.push(PlanOp::NodeScan {
        variable: Str::from(var),
        label: label.as_ref().map(|s| Str::from(s.as_str())),
    });
}

fn expr_to_scan_value(expr: &Expr) -> ScanValue {
    match &expr.kind {
        ExprKind::Literal(v) => ScanValue::Literal(v.clone()),
        ExprKind::Parameter(p) => ScanValue::Parameter(p.clone().into()),
        _ => ScanValue::Parameter(Str::from("?")),
    }
}

/// Find the value for `var.property = <value>` in an inline WHERE clause.
fn find_equality_value_in_where(var: &str, property: &str, where_expr: &Expr) -> Option<ScanValue> {
    let conjuncts = flatten_conjunction(where_expr);
    for conjunct in &conjuncts {
        if let ExprKind::Compare { left, op, right } = &conjunct.kind
            && *op == CmpOp::Eq
                && let ExprKind::PropertyAccess {
                    expr: inner,
                    property: prop,
                } = &left.kind
                    && let ExprKind::Variable(v) = &inner.kind
                        && v == var && prop == property {
                            return Some(expr_to_scan_value(right));
                        }
    }
    None
}

fn has_scan(ops: &[PlanOp]) -> bool {
    ops.iter().any(|op| {
        matches!(
            op,
            PlanOp::NodeScan { .. }
                | PlanOp::IndexScan { .. }
                | PlanOp::EdgeIndexScan { .. }
                | PlanOp::ConditionalIndexScan { .. }
        )
    })
}

fn plan_result_statement(result: &ResultStatement, ops: &mut Vec<PlanOp>) {
    match result {
        ResultStatement::Return(ret) => plan_return(ret, ops),
        ResultStatement::Select(sel) => plan_select(sel, ops),
        ResultStatement::Finish => {}
    }
}

fn plan_return(ret: &ReturnStatement, ops: &mut Vec<PlanOp>) {
    let distinct = ret.set_quantifier == SetQuantifier::Distinct;
    match &ret.body {
        ReturnBody::Star => {
            ops.push(PlanOp::Project {
                columns: vec![],
                distinct,
            });
        }
        // ReturnBody::NoBindings is cypher-only and not handled here.
        ReturnBody::Items {
            items,
            group_by,
            having: _,
            order_by,
            limit,
            offset,
        } => {
            // Aggregation.
            if let Some(gb) = group_by {
                let (agg_specs, proj_cols) = extract_aggregates(items);
                ops.push(PlanOp::Aggregate {
                    group_by: gb.items.clone(),
                    aggregates: agg_specs,
                });
                ops.push(PlanOp::Project {
                    columns: proj_cols,
                    distinct,
                });
            } else {
                let columns: Vec<ProjectColumn> = items
                    .iter()
                    .map(|item| ProjectColumn {
                        expr: item.expr.clone(),
                        alias: item.alias.as_ref().map(|a| Str::from(a.as_str())),
                    })
                    .collect();
                ops.push(PlanOp::Project { columns, distinct });
            }

            if let Some(ob) = order_by {
                ops.push(PlanOp::Sort {
                    order_by: ob.clone(),
                });
            }

            if limit.is_some() || offset.is_some() {
                ops.push(PlanOp::Limit {
                    count: limit.as_ref().map(|l| l.count.clone()),
                    offset: offset.as_ref().map(|o| o.count.clone()),
                });
            }
        }
    }
}

fn plan_select(sel: &SelectStatement, ops: &mut Vec<PlanOp>) {
    let distinct = sel.set_quantifier == SetQuantifier::Distinct;

    let (items, group_by, order_by, limit, offset) = match &sel.body {
        SelectBody::Star {
            group_by,
            having: _,
            order_by,
            limit,
            offset,
        } => (None, group_by, order_by, limit, offset),
        SelectBody::Items {
            items,
            group_by,
            having: _,
            order_by,
            limit,
            offset,
        } => (Some(items), group_by, order_by, limit, offset),
    };

    if let Some(gb) = group_by {
        if let Some(items) = items {
            let (agg_specs, proj_cols) = extract_aggregates(items);
            ops.push(PlanOp::Aggregate {
                group_by: gb.items.clone(),
                aggregates: agg_specs,
            });
            ops.push(PlanOp::Project {
                columns: proj_cols,
                distinct,
            });
        }
    } else if let Some(items) = items {
        let columns: Vec<ProjectColumn> = items
            .iter()
            .map(|item| ProjectColumn {
                expr: item.expr.clone(),
                alias: item.alias.as_ref().map(|a| Str::from(a.as_str())),
            })
            .collect();
        ops.push(PlanOp::Project { columns, distinct });
    } else {
        ops.push(PlanOp::Project {
            columns: vec![],
            distinct,
        });
    }

    if let Some(ob) = order_by {
        ops.push(PlanOp::Sort {
            order_by: ob.clone(),
        });
    }

    if limit.is_some() || offset.is_some() {
        ops.push(PlanOp::Limit {
            count: limit.as_ref().map(|l| l.count.clone()),
            offset: offset.as_ref().map(|o| o.count.clone()),
        });
    }
}

/// Extract aggregate functions from return items.
fn extract_aggregates(items: &[ReturnItem]) -> (Vec<AggregateSpec>, Vec<ProjectColumn>) {
    let mut agg_specs = Vec::new();
    let mut proj_cols = Vec::new();

    for item in items {
        if let Some(agg) = try_extract_aggregate(&item.expr) {
            agg_specs.push(AggregateSpec {
                func: Str::from(agg.0),
                expr: agg.1,
                distinct: agg.2,
                alias: item.alias.as_ref().map(|a| Str::from(a.as_str())),
            });
        }
        proj_cols.push(ProjectColumn {
            expr: item.expr.clone(),
            alias: item.alias.as_ref().map(|a| Str::from(a.as_str())),
        });
    }

    (agg_specs, proj_cols)
}

/// Try to extract an aggregate function from an expression.
fn try_extract_aggregate(expr: &Expr) -> Option<(String, Option<Expr>, bool)> {
    if let ExprKind::Aggregate {
        func,
        expr: agg_expr,
        distinct,
        ..
    } = &expr.kind
    {
        let func_name = format!("{:?}", func);
        return Some((func_name, agg_expr.as_deref().cloned(), *distinct));
    }
    None
}

/// Flatten AND chains into individual predicates.
fn flatten_conjunction(expr: &Expr) -> Vec<Expr> {
    match &expr.kind {
        ExprKind::And(left, right) => {
            let mut result = flatten_conjunction(left);
            result.extend(flatten_conjunction(right));
            result
        }
        _ => vec![expr.clone()],
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Bushy Join Detection
// ════════════════════════════════════════════════════════════════════════════════

/// Detect independent MATCH groups by analyzing variable flow.
/// Returns groups of part indices. If all parts are dependent, returns a single group.
///
/// Uses a variable→part-index hash map for O(V + N) grouping instead of O(N²)
/// pairwise comparison, where V = total variable count and N = number of parts.
fn detect_independent_match_groups(parts: &[SimpleQueryStatement]) -> Vec<Vec<usize>> {
    let n = parts.len();
    let mut uf = UnionFind::new(n);

    // Map: variable name → first part index that mentions it.
    let mut var_first_part: rapidhash::RapidHashMap<String, usize> =
        rapidhash::RapidHashMap::default();

    for (i, part) in parts.iter().enumerate() {
        let mut vars = std::collections::BTreeSet::new();
        if let SimpleQueryStatement::Match(m) = part {
            collect_pattern_variables(&m.pattern, &mut vars);
        }

        for var in vars {
            match var_first_part.entry(var) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    // Variable seen before: merge groups.
                    uf.union(*e.get(), i);
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(i);
                }
            }
        }

        // Non-MATCH parts are always dependent on the previous MATCH.
        if !matches!(part, SimpleQueryStatement::Match(_)) && i > 0 {
            uf.union(i - 1, i);
        }
    }

    // Collect groups preserving order.
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..n {
        groups.entry(uf.find(i)).or_default().push(i);
    }

    groups.into_values().collect()
}

/// Lightweight union-find with path compression and union by rank.
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        // Path splitting (iterative path compression).
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        // Union by rank.
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}

/// Collect all pattern variables from a GraphPattern.
fn collect_pattern_variables(
    pattern: &GraphPattern,
    vars: &mut std::collections::BTreeSet<String>,
) {
    for path in &pattern.paths {
        collect_path_expr_variables(&path.expr, vars);
        if let Some(v) = &path.variable {
            vars.insert(v.clone());
        }
    }
    // WHERE clause references.
    if let Some(where_expr) = &pattern.where_clause {
        for v in pushdown::collect_variables(where_expr) {
            vars.insert(v);
        }
    }
}

fn collect_path_expr_variables(
    expr: &PathPatternExpr,
    vars: &mut std::collections::BTreeSet<String>,
) {
    match expr {
        PathPatternExpr::Term(term) => {
            for factor in &term.factors {
                match &factor.primary {
                    PathPrimary::Node(node) => {
                        if let Some(v) = &node.variable {
                            vars.insert(v.clone());
                        }
                    }
                    PathPrimary::Edge(edge) => {
                        if let Some(v) = &edge.variable {
                            vars.insert(v.clone());
                        }
                    }
                    PathPrimary::Parenthesized { expr, .. } => {
                        collect_path_expr_variables(expr, vars);
                    }
                    _ => {}
                }
            }
        }
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                for factor in &term.factors {
                    match &factor.primary {
                        PathPrimary::Node(node) => {
                            if let Some(v) = &node.variable {
                                vars.insert(v.clone());
                            }
                        }
                        PathPrimary::Edge(edge) => {
                            if let Some(v) = &edge.variable {
                                vars.insert(v.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Build bushy join plan for independent MATCH groups.
fn plan_bushy_join(
    groups: &[Vec<usize>],
    parts: &[SimpleQueryStatement],
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) {
    let mut sub_plans: Vec<Vec<PlanOp>> = Vec::new();
    let mut sub_vars: Vec<std::collections::BTreeSet<String>> = Vec::new();

    for group in groups {
        let mut group_ops = Vec::new();
        let mut group_vars = std::collections::BTreeSet::new();

        for &idx in group {
            plan_simple_statement(
                &parts[idx],
                idx,
                stats,
                conditional_candidates,
                binding_kinds,
                &mut group_ops,
                annotations,
            );
            if let SimpleQueryStatement::Match(m) = &parts[idx] {
                collect_pattern_variables(&m.pattern, &mut group_vars);
            }
        }

        sub_vars.push(group_vars);
        sub_plans.push(group_ops);
    }

    // Join sub-plans pairwise.
    let mut result_ops = sub_plans.remove(0);
    let mut result_vars = sub_vars.remove(0);

    for (plan, vars) in sub_plans.into_iter().zip(sub_vars.into_iter()) {
        let shared: Vec<String> = result_vars
            .intersection(&vars)
            .cloned()
            .collect();

        let left = std::mem::take(&mut result_ops);
        if shared.is_empty() {
            result_ops = vec![PlanOp::CartesianProduct {
                left,
                right: plan,
            }];
        } else {
            let join_keys: Vec<Str> = shared.iter().map(|s| Str::from(s.as_str())).collect();
            result_ops = vec![PlanOp::HashJoin {
                left,
                right: plan,
                join_keys,
            }];
        }

        result_vars.extend(vars);
    }

    ops.extend(result_ops);
}

// ════════════════════════════════════════════════════════════════════════════════
// Adaptive Reoptimization Hints
// ════════════════════════════════════════════════════════════════════════════════

/// Set reoptimization hints for the executor based on plan uncertainty.
fn set_reoptimization_hints(
    ops: &[PlanOp],
    annotations: &mut PlanAnnotations,
    stats: Option<&dyn GraphStats>,
) {
    let has_stats = stats.and_then(|s| s.avg_degree()).is_some();

    for (i, op) in ops.iter().enumerate() {
        match op {
            // Expand without stats: cardinality is uncertain.
            PlanOp::Expand { .. } | PlanOp::ExpandFilter { .. } if !has_stats => {
                annotations.optimizer.cardinality_check_points.push(i);
                if annotations.optimizer.reoptimize_after_rows.is_none() {
                    annotations.optimizer.reoptimize_after_rows = Some(1000);
                }
            }
            // Procedure calls are opaque: always a check point.
            PlanOp::CallProcedure { .. } => {
                annotations.optimizer.cardinality_check_points.push(i);
            }
            _ => {}
        }
    }

    // Large plans: set reoptimization threshold.
    if let Some(rows) = annotations.optimizer.estimated_rows
        && rows > 100_000.0 && annotations.optimizer.reoptimize_after_rows.is_none() {
            annotations.optimizer.reoptimize_after_rows = Some(10_000);
        }
}

// ════════════════════════════════════════════════════════════════════════════════
// WCOJ Replacement
// ════════════════════════════════════════════════════════════════════════════════

/// Replace Expand chains that form a cycle with a single WorstCaseOptimalJoin op.
fn apply_wcoj_replacement(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    let cycles = match &annotations.optimizer.cyclic_patterns {
        Some(c) if !c.is_empty() => c.clone(),
        _ => return,
    };

    for cycle in &cycles {
        if cycle.variables.len() < 3 {
            continue; // Need at least a triangle.
        }

        // Find the Expand (or ExpandFilter) ops that form this cycle.
        let cycle_vars: std::collections::BTreeSet<&str> =
            cycle.variables.iter().map(|s| &**s).collect();

        let mut cycle_expand_indices = Vec::new();
        let mut wcoj_edges = Vec::new();

        for (i, op) in ops.iter().enumerate() {
            match op {
                PlanOp::Expand {
                    src, dst, edge, direction, label, ..
                }
                | PlanOp::ExpandFilter {
                    src, dst, edge, direction, label, ..
                } if cycle_vars.contains(&**src) && cycle_vars.contains(&**dst) => {
                    cycle_expand_indices.push(i);
                    wcoj_edges.push(WcojEdge {
                        variable: edge.clone(),
                        label: label.clone(),
                        direction: *direction,
                    });
                }
                _ => {}
            }
        }

        // Only replace if we found enough edges for the cycle.
        if wcoj_edges.len() >= cycle.variables.len() - 1 {
            // Remove the cycle Expand ops (in reverse to preserve indices).
            for &idx in cycle_expand_indices.iter().rev() {
                ops.remove(idx);
            }

            // Insert WCOJ at the position of the first removed Expand.
            let insert_pos = cycle_expand_indices[0].min(ops.len());
            ops.insert(
                insert_pos,
                PlanOp::WorstCaseOptimalJoin {
                    variables: cycle.variables.clone(),
                    edges: wcoj_edges,
                },
            );
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// DML Planning
// ════════════════════════════════════════════════════════════════════════════════

fn plan_insert(
    insert_stmt: &InsertStatement,
    ops: &mut Vec<PlanOp>,
    _annotations: &mut PlanAnnotations,
) {
    for pattern in &insert_stmt.patterns {
        let mut prev_node_var: Option<String> = None;

        for (i, element) in pattern.elements.iter().enumerate() {
            match element {
                InsertElement::Node(node) => {
                    let var = node
                        .variable
                        .clone()
                        .unwrap_or_else(|| format!("__insert_n{}", i));
                    let props: Vec<PropertyAssignment> = node
                        .properties
                        .iter()
                        .map(|p| PropertyAssignment {
                            name: p.name.clone().into(),
                            value: p.value.clone(),
                        })
                        .collect();
                    ops.push(PlanOp::InsertVertex {
                        variable: Some(Str::from(var.as_str())),
                        labels: node.labels.iter().map(|s| Str::from(s.as_str())).collect(),
                        properties: props,
                    });
                    prev_node_var = Some(var);
                }
                InsertElement::Edge(edge) => {
                    let var = edge
                        .variable
                        .clone()
                        .unwrap_or_else(|| format!("__insert_e{}", i));
                    let src = prev_node_var.clone().unwrap_or_default();
                    // Lookahead for destination node.
                    let dst = pattern.elements[i + 1..]
                        .iter()
                        .find_map(|e| match e {
                            InsertElement::Node(n) => {
                                Some(
                                    n.variable
                                        .clone()
                                        .unwrap_or_else(|| format!("__insert_n{}", i + 1)),
                                )
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| format!("__insert_dst_{}", i));
                    let props: Vec<PropertyAssignment> = edge
                        .properties
                        .iter()
                        .map(|p| PropertyAssignment {
                            name: p.name.clone().into(),
                            value: p.value.clone(),
                        })
                        .collect();
                    ops.push(PlanOp::InsertEdge {
                        variable: Some(Str::from(var.as_str())),
                        src: Str::from(src.as_str()),
                        dst: Str::from(dst.as_str()),
                        direction: edge.direction,
                        labels: edge.labels.iter().map(|s| Str::from(s.as_str())).collect(),
                        properties: props,
                    });
                    prev_node_var = None;
                }
            }
        }
    }
}

fn plan_set(
    set_stmt: &SetStatement,
    _binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    ops: &mut Vec<PlanOp>,
    _annotations: &mut PlanAnnotations,
) {
    let items: Vec<SetPlanItem> = set_stmt
        .items
        .iter()
        .map(|item| match item {
            SetItem::Property {
                span: _,
                variable,
                property,
                value,
            } => {
                SetPlanItem::Property {
                    variable: variable.clone().into(),
                    property: property.clone().into(),
                    value: value.clone(),
                }
            }
            SetItem::AllProperties {
                span: _,
                variable,
                value,
            } => {
                SetPlanItem::AllProperties {
                    variable: variable.clone().into(),
                    value: value.clone(),
                }
            }
            SetItem::Label {
                span: _,
                variable, label, ..
            } => {
                SetPlanItem::Label {
                    variable: variable.clone().into(),
                    label: label.clone().into(),
                }
            }
        })
        .collect();

    ops.push(PlanOp::SetProperties { items });
}

fn plan_remove(
    remove_stmt: &RemoveStatement,
    _binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    ops: &mut Vec<PlanOp>,
    _annotations: &mut PlanAnnotations,
) {
    let items: Vec<RemovePlanItem> = remove_stmt
        .items
        .iter()
        .map(|item| match item {
            RemoveItem::Property {
                span: _,
                variable,
                property,
            } => {
                RemovePlanItem::Property {
                    variable: variable.clone().into(),
                    property: property.clone().into(),
                }
            }
            RemoveItem::Label {
                span: _,
                variable, label, ..
            } => {
                RemovePlanItem::Label {
                    variable: variable.clone().into(),
                    label: label.clone().into(),
                }
            }
        })
        .collect();

    ops.push(PlanOp::RemoveProperties { items });
}

fn plan_delete(
    delete_stmt: &DeleteStatement,
    binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    ops: &mut Vec<PlanOp>,
    _annotations: &mut PlanAnnotations,
) {
    for item in &delete_stmt.items {
        let variable: Str = match &item.kind {
            ExprKind::Variable(v) => Str::from(v.as_str()),
            _ => Str::from(format!("{:?}", item.kind).as_str()),
        };

        match binding_kinds
            .get(variable.as_ref())
            .copied()
            .unwrap_or(BindingKind::Unknown)
        {
            BindingKind::Edge => {
                ops.push(PlanOp::DeleteEdge { variable });
            }
            BindingKind::Node
            | BindingKind::Unknown
            | BindingKind::Path
            | BindingKind::Value => match delete_stmt.detach {
                DeleteDetach::Detach => {
                    ops.push(PlanOp::DetachDeleteVertex { variable });
                }
                DeleteDetach::NoDetach | DeleteDetach::Unspecified => {
                    ops.push(PlanOp::DeleteVertex { variable });
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use gleaph_gql::parser;

    use super::{build_block_plan, PlanOp};

    fn parse_block(input: &str) -> gleaph_gql::ast::StatementBlock {
        let program = parser::parse(input).expect("query should parse");
        program
            .transaction_activity
            .expect("transaction activity")
            .body
            .expect("statement block")
    }

    #[test]
    fn plans_delete_edge_when_binding_was_introduced_by_expand() {
        let block = parse_block("MATCH (a)-[e]->(b) DELETE e");
        let plan = build_block_plan(&block, None).expect("plan should build");
        assert!(plan.ops.iter().any(|op| matches!(op, PlanOp::DeleteEdge { variable } if variable.as_ref() == "e")));
    }

    #[test]
    fn keeps_delete_vertex_for_node_binding() {
        let block = parse_block("MATCH (a:User) DELETE a");
        let plan = build_block_plan(&block, None).expect("plan should build");
        assert!(plan.ops.iter().any(|op| matches!(op, PlanOp::DeleteVertex { variable } if variable.as_ref() == "a")));
    }
}
