//! Core planner: converts a GQL AST into a [`PhysicalPlan`].
//!
//! The planner walks a [`LinearQueryStatement`] and emits a sequence of
//! [`PlanOp`] operators, choosing scans, expansions, filters, projections,
//! and aggregations based on the query structure and optional statistics.

use gleaph_gql::ast::*;
use gleaph_gql::type_check::{
    BindingKind, DmlDiagnosticSeverity, NoSchema, PropertySchema, TypeWarning,
    dml_diagnostic_from_warning, infer_composite_query_binding_kinds_and_warnings_with_schema,
    infer_linear_query_binding_kinds_and_warnings_with_schema,
    infer_linear_query_binding_kinds_with_schema, infer_linear_query_binding_kinds_with_seed,
    infer_statement_block_binding_kinds_with_schema, type_check_statement_block_with_schema,
    type_check_statement_with_schema, type_diagnostic_from_warning,
};
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use std::collections::{BTreeMap, BTreeSet};

use crate::anchor::{self, extract_simple_label};
use crate::cost;
use crate::cse;
use crate::explain::explain_plan;
use crate::expr_alias::substitute_return_aliases_in_expr;
use crate::expr_children::for_each_immediate_child_expr;
use crate::join_order;
use crate::path_extensions::{
    PathPatternExtensionContext, PlanBuildOptions, REJECTING_PATH_EXTENSION_HANDLER,
    SingleEdgePathInfo,
};
use crate::plan::*;
use crate::pushdown;
use crate::semantic::{self, SemanticAnalysis, SemanticConstraint};
use crate::stats::GraphStats;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannerError {
    FatalDml(PlannerDiagnostic),
    UnsupportedPattern(String),
    UnsupportedExtension(String),
}

impl std::fmt::Display for PlannerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FatalDml(diagnostic) => write!(
                f,
                "fatal DML diagnostic [{}] at {}..{}: {}",
                diagnostic.code, diagnostic.span.start, diagnostic.span.end, diagnostic.message
            ),
            Self::UnsupportedPattern(msg) => write!(f, "unsupported graph pattern: {msg}"),
            Self::UnsupportedExtension(msg) => {
                write!(f, "unsupported path pattern extension: {msg}")
            }
        }
    }
}

impl std::error::Error for PlannerError {}

#[derive(Clone, Debug)]
pub struct PlanBuildOutput {
    pub plan: PhysicalPlan,
    pub summary: PlanSummary,
    /// Human-readable plan text from [`explain_plan`]. Empty when built via
    /// [`build_plan_output_for_execute`] / [`build_block_plan_output_for_execute`] (execute hot path).
    pub explain: String,
}

impl PlanBuildOutput {
    fn from_plan(plan: PhysicalPlan) -> Self {
        Self::from_plan_with_explain(plan, true)
    }

    fn from_plan_with_explain(plan: PhysicalPlan, include_explain: bool) -> Self {
        let summary = PlanSummary::from_plan(&plan);
        let explain = if include_explain {
            explain_plan(&plan)
        } else {
            String::new()
        };
        Self {
            plan,
            summary,
            explain,
        }
    }
}

/// Build a physical plan from a top-level statement with extension-aware options.
pub fn build_statement_plan_with_options(
    stmt: &Statement,
    options: PlanBuildOptions<'_>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    if let Statement::Query(composite) = stmt {
        return build_composite_plan_with_schema_and_options(composite, options, schema);
    }

    let mut plan =
        build_statement_plan_with_binding_kinds_and_options(stmt, options, None, schema)?;
    apply_type_checker_dml_diagnostics(
        &mut plan.diagnostics,
        &type_check_statement_with_schema(stmt, schema),
    );
    validate_plan(plan)
}

/// Build a physical plan from a top-level statement.
///
/// Handles both query statements (`Statement::Query`) and DML statements
/// (`Statement::Insert/Set/Remove/Delete`).
pub fn build_statement_plan(
    stmt: &Statement,
    stats: Option<&dyn GraphStats>,
) -> Result<PhysicalPlan, PlannerError> {
    build_statement_plan_with_schema(stmt, stats, &NoSchema)
}

/// Like [`build_statement_plan`], but uses `schema` for binding inference and DML/type diagnostics.
pub fn build_statement_plan_with_schema(
    stmt: &Statement,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    if let Statement::Query(composite) = stmt {
        return build_composite_plan_with_schema(composite, stats, schema);
    }

    let mut plan = build_statement_plan_with_binding_kinds(stmt, stats, None, schema)?;
    apply_type_checker_dml_diagnostics(
        &mut plan.diagnostics,
        &type_check_statement_with_schema(stmt, schema),
    );
    validate_plan(plan)
}

pub fn build_statement_plan_output(
    stmt: &Statement,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_statement_plan(stmt, stats).map(PlanBuildOutput::from_plan)
}

pub fn build_statement_plan_output_with_schema(
    stmt: &Statement,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PlanBuildOutput, PlannerError> {
    build_statement_plan_with_schema(stmt, stats, schema).map(PlanBuildOutput::from_plan)
}

fn build_statement_plan_with_binding_kinds_and_options(
    stmt: &Statement,
    options: PlanBuildOptions<'_>,
    binding_kinds: Option<&std::collections::BTreeMap<String, BindingKind>>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    match stmt {
        Statement::Query(composite) => build_composite_plan_with_binding_kinds_and_options(
            composite,
            options,
            binding_kinds,
            schema,
        ),
        Statement::Insert(insert_stmt) => {
            let mut plan = PhysicalPlan::default();
            plan_insert(insert_stmt, &mut plan.ops, &mut plan.annotations);
            plan.annotations.optimizer.estimated_cost =
                Some(cost::estimate_cost(&plan.ops, options.stats));
            plan.annotations.optimizer.estimated_rows =
                Some(cost::estimate_rows(&plan.ops, options.stats));
            Ok(plan)
        }
        _ => Ok(PhysicalPlan::default()), // TODO: DDL, Session
    }
}

fn build_statement_plan_with_binding_kinds(
    stmt: &Statement,
    stats: Option<&dyn GraphStats>,
    binding_kinds: Option<&std::collections::BTreeMap<String, BindingKind>>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    match stmt {
        Statement::Query(composite) => {
            build_composite_plan_with_binding_kinds(composite, stats, binding_kinds, schema)
        }
        Statement::Insert(insert_stmt) => {
            let mut plan = PhysicalPlan::default();
            plan_insert(insert_stmt, &mut plan.ops, &mut plan.annotations);
            plan.annotations.optimizer.estimated_cost = Some(cost::estimate_cost(&plan.ops, stats));
            plan.annotations.optimizer.estimated_rows = Some(cost::estimate_rows(&plan.ops, stats));
            Ok(plan)
        }
        _ => Ok(PhysicalPlan::default()), // TODO: DDL, Session
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
    build_block_plan_with_schema(block, stats, &NoSchema)
}

/// Like [`build_block_plan`], but uses `schema` for binding inference and diagnostics.
pub fn build_block_plan_with_schema(
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    let binding_kinds = infer_statement_block_binding_kinds_with_schema(block, schema);

    // Plan the first statement.
    let mut plan = build_statement_plan_with_binding_kinds(
        &block.first,
        stats,
        binding_kinds.first(),
        schema,
    )?;

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
            schema,
        )?;
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
    apply_type_checker_dml_diagnostics(
        &mut plan.diagnostics,
        &type_check_statement_block_with_schema(block, schema),
    );
    validate_plan(plan)
}

pub fn build_block_plan_output(
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_block_plan(block, stats).map(PlanBuildOutput::from_plan)
}

pub fn build_block_plan_output_with_schema(
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PlanBuildOutput, PlannerError> {
    build_block_plan_with_schema(block, stats, schema).map(PlanBuildOutput::from_plan)
}

/// Build a physical plan from a linear query statement.
pub fn build_plan(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
) -> Result<PhysicalPlan, PlannerError> {
    build_plan_with_schema(query, stats, &NoSchema)
}

/// Like [`build_plan`], but uses `schema` for binding inference and DML/type diagnostics.
pub fn build_plan_with_schema_and_options(
    query: &LinearQueryStatement,
    options: PlanBuildOptions<'_>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    let (binding_kinds, type_warnings) =
        infer_linear_query_binding_kinds_and_warnings_with_schema(query, schema);
    let mut plan = build_plan_core(query, &binding_kinds, schema, options)?;
    apply_type_checker_dml_diagnostics(&mut plan.diagnostics, &type_warnings);
    validate_plan(plan)
}

pub fn build_plan_with_schema(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    let (binding_kinds, type_warnings) =
        infer_linear_query_binding_kinds_and_warnings_with_schema(query, schema);
    let mut plan = build_plan_core(
        query,
        &binding_kinds,
        schema,
        PlanBuildOptions {
            stats,
            path_extensions: &REJECTING_PATH_EXTENSION_HANDLER,
        },
    )?;
    apply_type_checker_dml_diagnostics(&mut plan.diagnostics, &type_warnings);
    validate_plan(plan)
}

pub fn build_plan_output(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_plan(query, stats).map(PlanBuildOutput::from_plan)
}

pub fn build_plan_output_with_schema(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PlanBuildOutput, PlannerError> {
    build_plan_with_schema(query, stats, schema).map(PlanBuildOutput::from_plan)
}

/// Like [`build_plan_output`], but leaves [`PlanBuildOutput::explain`] empty so callers avoid
/// [`explain_plan`] formatting on every execute iteration.
pub fn build_plan_output_for_execute(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_plan(query, stats).map(|p| PlanBuildOutput::from_plan_with_explain(p, false))
}

pub fn build_plan_output_for_execute_with_schema(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PlanBuildOutput, PlannerError> {
    build_plan_with_schema(query, stats, schema)
        .map(|p| PlanBuildOutput::from_plan_with_explain(p, false))
}

/// Like [`build_block_plan_output`], but leaves [`PlanBuildOutput::explain`] empty for execute paths.
pub fn build_block_plan_output_for_execute(
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_block_plan(block, stats).map(|p| PlanBuildOutput::from_plan_with_explain(p, false))
}

pub fn build_block_plan_output_for_execute_with_schema(
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PlanBuildOutput, PlannerError> {
    build_block_plan_with_schema(block, stats, schema)
        .map(|p| PlanBuildOutput::from_plan_with_explain(p, false))
}

fn build_plan_with_binding_kinds_and_options(
    query: &LinearQueryStatement,
    options: PlanBuildOptions<'_>,
    seed_binding_kinds: Option<&BTreeMap<String, BindingKind>>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    let binding_kinds = match seed_binding_kinds {
        Some(seed) => infer_linear_query_binding_kinds_with_seed(query, schema, seed),
        None => infer_linear_query_binding_kinds_with_schema(query, schema),
    };
    build_plan_core(query, &binding_kinds, schema, options)
}

fn build_plan_with_binding_kinds(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
    seed_binding_kinds: Option<&BTreeMap<String, BindingKind>>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    let binding_kinds = match seed_binding_kinds {
        Some(seed) => infer_linear_query_binding_kinds_with_seed(query, schema, seed),
        None => infer_linear_query_binding_kinds_with_schema(query, schema),
    };
    build_plan_core(
        query,
        &binding_kinds,
        schema,
        PlanBuildOptions {
            stats,
            path_extensions: &REJECTING_PATH_EXTENSION_HANDLER,
        },
    )
}

fn build_plan_core(
    query: &LinearQueryStatement,
    binding_kinds: &BTreeMap<String, BindingKind>,
    schema: &dyn PropertySchema,
    options: PlanBuildOptions<'_>,
) -> Result<PhysicalPlan, PlannerError> {
    let stats = options.stats;
    // Phase 1: Semantic analysis.
    let semantic = semantic::analyze(query);
    let referenced_vars = crate::variable_refs::linear_query_referenced_variables(query);

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
            binding_kinds,
            &referenced_vars,
            schema,
            options,
            &mut ops,
            &mut annotations,
        )?;
    } else {
        // Sequential: process all parts in order (default behavior).
        let mut bound_node_vars = BTreeSet::new();
        let mut optional_node_vars = BTreeSet::new();
        for (stage, part) in query.parts.iter().enumerate() {
            plan_simple_statement(
                part,
                stage,
                stats,
                &conditional_candidates,
                binding_kinds,
                &referenced_vars,
                schema,
                options,
                &mut bound_node_vars,
                &mut optional_node_vars,
                &mut ops,
                &mut annotations,
            )?;
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
    pushdown::apply_shortest_path_binding_pruning(&mut ops, &mut annotations);
    // Replace simple `Expand` cycles with a single `WorstCaseOptimalJoin` when safe.
    apply_wcoj_replacement(&mut ops, &mut annotations);
    crate::property_projection::apply_node_property_projections(&mut ops);

    // Phase 2b: Annotation-only analysis.
    cse::detect_common_subexpressions(&ops, &mut annotations);
    annotate_use_graph_pushdown(&ops, &mut annotations);
    set_reoptimization_hints(&ops, &mut annotations, stats);

    // Phase 3: Cost estimation.
    annotations.optimizer.estimated_cost = Some(cost::estimate_cost(&ops, stats));
    annotations.optimizer.estimated_rows = Some(cost::estimate_rows(&ops, stats));

    let output = crate::output_schema::derive_output_schema(&ops);
    let binding_layout = crate::binding_layout::derive_binding_layout(&ops);

    Ok(PhysicalPlan {
        ops,
        diagnostics: PlanDiagnostics::default(),
        annotations,
        output,
        binding_layout,
    })
}

fn annotate_use_graph_pushdown(ops: &[PlanOp], annotations: &mut PlanAnnotations) {
    annotations.optimizer.use_graph_pushdown.clear();
    collect_use_graph_pushdown(ops, &mut annotations.optimizer.use_graph_pushdown);
}

fn collect_use_graph_pushdown(ops: &[PlanOp], out: &mut Vec<UseGraphPushdownInfo>) {
    for op in ops {
        match op {
            PlanOp::UseGraph {
                graph_name,
                sub_plan: Some(sub_plan),
            } => {
                let graph_name = graph_name
                    .iter()
                    .map(|part| part.as_ref())
                    .collect::<Vec<_>>()
                    .join(".");
                out.push(analyze_remote_use_graph_pushdown(&graph_name, sub_plan));
                collect_use_graph_pushdown(sub_plan, out);
            }
            PlanOp::UseGraph { sub_plan: None, .. } => {}
            PlanOp::InlineProcedureCall { sub_plan, .. } => {
                collect_use_graph_pushdown(&sub_plan.ops, out);
            }
            PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
                collect_use_graph_pushdown(left, out);
                collect_use_graph_pushdown(right, out);
            }
            PlanOp::SetOperation { right, .. } => collect_use_graph_pushdown(&right.ops, out),
            PlanOp::OptionalMatch { sub_plan } => collect_use_graph_pushdown(sub_plan, out),
            _ => {}
        }
    }
}

pub fn analyze_remote_use_graph_pushdown(
    graph_name: &str,
    sub_plan: &[PlanOp],
) -> UseGraphPushdownInfo {
    match check_remote_use_graph_pushdown(sub_plan) {
        Ok(()) => UseGraphPushdownInfo {
            graph_name: graph_name.to_owned(),
            supported: true,
            reason: None,
        },
        Err(reason) => UseGraphPushdownInfo {
            graph_name: graph_name.to_owned(),
            supported: false,
            reason: Some(reason),
        },
    }
}

fn check_remote_use_graph_pushdown(sub_plan: &[PlanOp]) -> Result<(), String> {
    if sub_plan.is_empty() {
        return Err("empty sub-plan".to_owned());
    }

    let consumed = match &sub_plan[0] {
        PlanOp::CallProcedure { .. } => 1,
        PlanOp::NodeScan { variable, .. } | PlanOp::IndexScan { variable, .. } => {
            consume_simple_expand_chain(sub_plan, 1, variable.as_ref())?
        }
        PlanOp::EdgeIndexScan { .. } => {
            let Some(PlanOp::EdgeBindEndpoints { far, .. }) = sub_plan.get(1) else {
                return Err(
                    "EDGE INDEX SCAN root requires EDGE BIND ENDPOINTS immediately after it"
                        .to_owned(),
                );
            };
            consume_simple_expand_chain(sub_plan, 2, far.as_ref())?
        }
        other => {
            return Err(format!(
                "unsupported remote USE GRAPH root op: {}",
                remote_use_graph_op_name(other)
            ));
        }
    };

    for op in &sub_plan[consumed..] {
        match op {
            PlanOp::Filter { .. }
            | PlanOp::PropertyFilter { .. }
            | PlanOp::Aggregate { .. }
            | PlanOp::Project { .. }
            | PlanOp::Sort { .. }
            | PlanOp::TopK { .. }
            | PlanOp::Limit { .. } => {}
            other => {
                return Err(format!(
                    "unsupported remote USE GRAPH op after root: {}",
                    remote_use_graph_op_name(other)
                ));
            }
        }
    }

    Ok(())
}

fn consume_simple_expand_chain(
    sub_plan: &[PlanOp],
    start_index: usize,
    start_src: &str,
) -> Result<usize, String> {
    let mut index = start_index;
    let mut current_src = start_src.to_owned();
    while let Some(op) = sub_plan.get(index) {
        match op {
            PlanOp::Expand {
                src,
                dst,
                label_expr,
                var_len,
                indexed_edge_equality,
                ..
            }
            | PlanOp::ExpandFilter {
                src,
                dst,
                label_expr,
                var_len,
                indexed_edge_equality,
                ..
            } if src.as_ref() == current_src => {
                if label_expr.is_some() || indexed_edge_equality.is_some() {
                    return Err(
                        "remote USE GRAPH expand chain supports only fixed 1-hop expansions without label expressions or indexed edge equality"
                            .to_owned(),
                    );
                }
                if let Some(vl) = var_len
                    && (vl.min != 1 || vl.max != Some(1))
                {
                    return Err(
                            "remote USE GRAPH expand chain supports only fixed 1-hop expansions without label expressions or indexed edge equality"
                                .to_owned(),
                        );
                }
                current_src = dst.as_ref().to_owned();
                index += 1;
            }
            _ => break,
        }
    }
    Ok(index)
}

fn remote_use_graph_op_name(op: &PlanOp) -> &'static str {
    match op {
        PlanOp::NodeScan { .. } => "NODE SCAN",
        PlanOp::IndexScan { .. } => "INDEX SCAN",
        PlanOp::EdgeIndexScan { .. } => "EDGE INDEX SCAN",
        PlanOp::EdgeBindEndpoints { .. } => "EDGE BIND ENDPOINTS",
        PlanOp::ConditionalIndexScan { .. } => "CONDITIONAL INDEX SCAN",
        PlanOp::PropertyFilter { .. } => "PROPERTY FILTER",
        PlanOp::Expand { .. } => "EXPAND",
        PlanOp::ExpandFilter { .. } => "EXPAND FILTER",
        PlanOp::ShortestPath { .. } => "SHORTEST PATH",
        PlanOp::Let { .. } => "LET",
        PlanOp::For { .. } => "FOR",
        PlanOp::Filter { .. } => "FILTER",
        PlanOp::CallProcedure { .. } => "CALL",
        PlanOp::InlineProcedureCall { .. } => "INLINE CALL",
        PlanOp::UseGraph { .. } => "USE GRAPH",
        PlanOp::HashJoin { .. } => "HASH JOIN",
        PlanOp::CartesianProduct { .. } => "CARTESIAN PRODUCT",
        PlanOp::Aggregate { .. } => "AGGREGATE",
        PlanOp::Project { .. } => "PROJECT",
        PlanOp::Sort { .. } => "SORT",
        PlanOp::Limit { .. } => "LIMIT",
        PlanOp::SetOperation { .. } => "SET OPERATION",
        PlanOp::OptionalMatch { .. } => "OPTIONAL MATCH",
        PlanOp::IndexIntersection { .. } => "INDEX INTERSECTION",
        PlanOp::WorstCaseOptimalJoin { .. } => "WORST-CASE OPTIMAL JOIN",
        PlanOp::TopK { .. } => "TOPK",
        PlanOp::Materialize { .. } => "MATERIALIZE",
        PlanOp::InsertVertex { .. } => "INSERT VERTEX",
        PlanOp::InsertEdge { .. } => "INSERT EDGE",
        PlanOp::SetProperties { .. } => "SET PROPERTIES",
        PlanOp::RemoveProperties { .. } => "REMOVE PROPERTIES",
        PlanOp::DeleteVertex { .. } => "DELETE VERTEX",
        PlanOp::DetachDeleteVertex { .. } => "DETACH DELETE VERTEX",
        PlanOp::DeleteEdge { .. } => "DELETE EDGE",
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
            SemanticConstraint::WhereEqualityPredicate { var, property, .. } => {
                if let Some(stats) = stats
                    && stats.is_vertex_property_indexed(property)
                {
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
    build_composite_plan_with_schema(composite, stats, &NoSchema)
}

pub fn build_composite_plan_with_schema_and_options(
    composite: &CompositeQueryExpr,
    options: PlanBuildOptions<'_>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    if composite.rest.is_empty() {
        return build_plan_with_schema_and_options(&composite.left, options, schema);
    }

    let (branch_kinds, type_warnings) =
        infer_composite_query_binding_kinds_and_warnings_with_schema(composite, schema);
    debug_assert_eq!(branch_kinds.len(), 1 + composite.rest.len());

    let mut plan = build_composite_plan_from_branch_kinds_and_options(
        composite,
        options,
        &branch_kinds,
        schema,
    )?;
    apply_type_checker_dml_diagnostics(&mut plan.diagnostics, &type_warnings);
    validate_plan(plan)
}

pub fn build_composite_plan_with_schema(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    if composite.rest.is_empty() {
        return build_plan_with_schema(&composite.left, stats, schema);
    }

    let (branch_kinds, type_warnings) =
        infer_composite_query_binding_kinds_and_warnings_with_schema(composite, schema);
    debug_assert_eq!(branch_kinds.len(), 1 + composite.rest.len());

    let mut plan = build_composite_plan_from_branch_kinds(composite, stats, &branch_kinds, schema)?;
    apply_type_checker_dml_diagnostics(&mut plan.diagnostics, &type_warnings);
    validate_plan(plan)
}

pub fn build_composite_plan_output(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_composite_plan(composite, stats).map(PlanBuildOutput::from_plan)
}

pub fn build_composite_plan_output_with_schema(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PlanBuildOutput, PlannerError> {
    build_composite_plan_with_schema(composite, stats, schema).map(PlanBuildOutput::from_plan)
}

fn build_composite_plan_from_branch_kinds_and_options(
    composite: &CompositeQueryExpr,
    options: PlanBuildOptions<'_>,
    branch_kinds: &[BTreeMap<String, BindingKind>],
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    debug_assert_eq!(branch_kinds.len(), 1 + composite.rest.len());

    let mut plan = build_plan_core(&composite.left, &branch_kinds[0], schema, options)?;

    for (i, (set_op, right_query)) in composite.rest.iter().enumerate() {
        let right_plan = build_plan_core(right_query, &branch_kinds[1 + i], schema, options)?;
        plan.ops.push(PlanOp::SetOperation {
            op: *set_op,
            right: Box::new(right_plan),
        });
    }

    Ok(plan)
}

fn build_composite_plan_from_branch_kinds(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
    branch_kinds: &[BTreeMap<String, BindingKind>],
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    debug_assert_eq!(branch_kinds.len(), 1 + composite.rest.len());

    let mut plan = build_plan_core(
        &composite.left,
        &branch_kinds[0],
        schema,
        PlanBuildOptions {
            stats,
            path_extensions: &REJECTING_PATH_EXTENSION_HANDLER,
        },
    )?;

    for (i, (set_op, right_query)) in composite.rest.iter().enumerate() {
        let right_plan = build_plan_core(
            right_query,
            &branch_kinds[1 + i],
            schema,
            PlanBuildOptions {
                stats,
                path_extensions: &REJECTING_PATH_EXTENSION_HANDLER,
            },
        )?;
        plan.ops.push(PlanOp::SetOperation {
            op: *set_op,
            right: Box::new(right_plan),
        });
    }

    Ok(plan)
}

pub(super) fn build_composite_plan_with_binding_kinds_and_options(
    composite: &CompositeQueryExpr,
    options: PlanBuildOptions<'_>,
    seed_binding_kinds: Option<&BTreeMap<String, BindingKind>>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    let mut plan = build_plan_with_binding_kinds_and_options(
        &composite.left,
        options,
        seed_binding_kinds,
        schema,
    )?;

    for (set_op, right_query) in &composite.rest {
        let right_plan = build_plan_with_binding_kinds_and_options(
            right_query,
            options,
            seed_binding_kinds,
            schema,
        )?;
        plan.ops.push(PlanOp::SetOperation {
            op: *set_op,
            right: Box::new(right_plan),
        });
    }

    Ok(plan)
}

fn build_composite_plan_with_binding_kinds(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
    seed_binding_kinds: Option<&BTreeMap<String, BindingKind>>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    let mut plan =
        build_plan_with_binding_kinds(&composite.left, stats, seed_binding_kinds, schema)?;

    // Append set operations (UNION, EXCEPT, INTERSECT, OTHERWISE).
    for (set_op, right_query) in &composite.rest {
        let right_plan =
            build_plan_with_binding_kinds(right_query, stats, seed_binding_kinds, schema)?;
        plan.ops.push(PlanOp::SetOperation {
            op: *set_op,
            right: Box::new(right_plan),
        });
    }

    Ok(plan)
}

fn validate_plan(plan: PhysicalPlan) -> Result<PhysicalPlan, PlannerError> {
    if let Some(error) = plan.diagnostics.dml_errors.first() {
        Err(PlannerError::FatalDml(error.clone()))
    } else if let Some(message) = first_unfused_gleaph_vector_expr_in_ops(&plan.ops) {
        Err(PlannerError::UnsupportedPattern(message))
    } else {
        Ok(plan)
    }
}

fn first_unfused_gleaph_vector_expr_in_ops(ops: &[PlanOp]) -> Option<String> {
    for op in ops {
        if let Some(message) = first_unfused_gleaph_vector_expr_in_op(op) {
            return Some(message);
        }
    }
    None
}

fn first_unfused_gleaph_vector_expr_in_op(op: &PlanOp) -> Option<String> {
    match op {
        PlanOp::NodeScan { .. }
        | PlanOp::IndexScan { .. }
        | PlanOp::EdgeIndexScan { .. }
        | PlanOp::EdgeBindEndpoints { .. }
        | PlanOp::ConditionalIndexScan { .. }
        | PlanOp::DeleteVertex { .. }
        | PlanOp::DetachDeleteVertex { .. }
        | PlanOp::DeleteEdge { .. } => None,
        PlanOp::PropertyFilter { predicates, .. } => {
            first_unfused_gleaph_vector_expr_in_exprs(predicates)
        }
        PlanOp::ExpandFilter { dst_filter, .. } => {
            first_unfused_gleaph_vector_expr_in_exprs(dst_filter)
        }
        PlanOp::Expand { .. } => None,
        PlanOp::ShortestPath { cost, .. } => match cost {
            ShortestPathCost::HopCount => None,
            ShortestPathCost::EdgeCostExpr { expr, .. } => {
                first_unfused_gleaph_vector_expr_in_expr(expr)
            }
        },
        PlanOp::Let { bindings } => bindings
            .iter()
            .find_map(|binding| first_unfused_gleaph_vector_expr_in_expr(&binding.value)),
        PlanOp::For { list, .. } => first_unfused_gleaph_vector_expr_in_expr(list),
        PlanOp::Filter { condition } => first_unfused_gleaph_vector_expr_in_expr(condition),
        PlanOp::CallProcedure { args, .. } => first_unfused_gleaph_vector_expr_in_exprs(args),
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            first_unfused_gleaph_vector_expr_in_ops(&sub_plan.ops)
        }
        PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => first_unfused_gleaph_vector_expr_in_ops(sub_plan),
        PlanOp::UseGraph { sub_plan: None, .. } => None,
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            first_unfused_gleaph_vector_expr_in_ops(left)
                .or_else(|| first_unfused_gleaph_vector_expr_in_ops(right))
        }
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => first_unfused_gleaph_vector_expr_in_exprs(group_by).or_else(|| {
            aggregates.iter().find_map(|aggregate| {
                aggregate
                    .expr
                    .as_ref()
                    .and_then(first_unfused_gleaph_vector_expr_in_expr)
                    .or_else(|| {
                        aggregate
                            .expr2
                            .as_ref()
                            .and_then(first_unfused_gleaph_vector_expr_in_expr)
                    })
                    .or_else(|| {
                        aggregate
                            .filter
                            .as_ref()
                            .and_then(first_unfused_gleaph_vector_expr_in_expr)
                    })
                    .or_else(|| {
                        aggregate
                            .order_by
                            .as_ref()
                            .and_then(first_unfused_gleaph_vector_expr_in_order_by)
                    })
            })
        }),
        PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => columns
            .iter()
            .find_map(|column| first_unfused_gleaph_vector_expr_in_expr(&column.expr)),
        PlanOp::Sort { order_by } => first_unfused_gleaph_vector_expr_in_order_by(order_by),
        PlanOp::Limit { count, offset } => count
            .as_ref()
            .and_then(first_unfused_gleaph_vector_expr_in_expr)
            .or_else(|| {
                offset
                    .as_ref()
                    .and_then(first_unfused_gleaph_vector_expr_in_expr)
            }),
        PlanOp::SetOperation { right, .. } => first_unfused_gleaph_vector_expr_in_ops(&right.ops),
        PlanOp::OptionalMatch { sub_plan } => first_unfused_gleaph_vector_expr_in_ops(sub_plan),
        PlanOp::IndexIntersection { .. } => None,
        PlanOp::WorstCaseOptimalJoin { edges, .. } => edges
            .iter()
            .find_map(|edge| first_unfused_gleaph_vector_expr_in_exprs(&edge.dst_filter)),
        PlanOp::TopK {
            order_by,
            k,
            offset,
        } => first_unfused_gleaph_vector_expr_in_order_by(order_by)
            .or_else(|| first_unfused_gleaph_vector_expr_in_expr(k))
            .or_else(|| {
                offset
                    .as_ref()
                    .and_then(first_unfused_gleaph_vector_expr_in_expr)
            }),
        PlanOp::InsertVertex { properties, .. } | PlanOp::InsertEdge { properties, .. } => {
            first_unfused_gleaph_vector_expr_in_property_assignments(properties)
        }
        PlanOp::SetProperties { items } => items.iter().find_map(|item| match item {
            SetPlanItem::Property { value, .. } | SetPlanItem::AllProperties { value, .. } => {
                first_unfused_gleaph_vector_expr_in_expr(value)
            }
            SetPlanItem::Label { .. } => None,
        }),
        PlanOp::RemoveProperties { .. } => None,
    }
}

fn first_unfused_gleaph_vector_expr_in_property_assignments(
    properties: &[PropertyAssignment],
) -> Option<String> {
    properties
        .iter()
        .find_map(|property| first_unfused_gleaph_vector_expr_in_expr(&property.value))
}

fn first_unfused_gleaph_vector_expr_in_order_by(order_by: &OrderByClause) -> Option<String> {
    order_by
        .items
        .iter()
        .find_map(|item| first_unfused_gleaph_vector_expr_in_expr(&item.expr))
}

fn first_unfused_gleaph_vector_expr_in_exprs(exprs: &[Expr]) -> Option<String> {
    exprs
        .iter()
        .find_map(first_unfused_gleaph_vector_expr_in_expr)
}

fn first_unfused_gleaph_vector_expr_in_expr(expr: &Expr) -> Option<String> {
    if is_gleaph_vector_function_call(expr) {
        return Some(
            "GLEAPH.VECTOR.* can only be used as a fused fixed-label edge predicate".into(),
        );
    }
    let mut found = None;
    for_each_immediate_child_expr(expr, |child| {
        if found.is_none() {
            found = first_unfused_gleaph_vector_expr_in_expr(child);
        }
    });
    found
}

fn is_gleaph_vector_function_call(expr: &Expr) -> bool {
    let ExprKind::FunctionCall { name, .. } = &expr.kind else {
        return false;
    };
    name.parts.len() >= 2
        && name.parts[0].eq_ignore_ascii_case("gleaph")
        && name.parts[1].eq_ignore_ascii_case("vector")
}

fn apply_type_checker_dml_diagnostics(diagnostics: &mut PlanDiagnostics, warnings: &[TypeWarning]) {
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


mod dml;
mod join;
mod match_plan;
mod optimize;

use dml::{plan_delete, plan_insert, plan_remove, plan_set};
use join::{detect_independent_match_groups, plan_bushy_join};
use match_plan::{detect_conditional_candidates, plan_result_statement, plan_simple_statement};
use optimize::{apply_wcoj_replacement, set_reoptimization_hints};

#[cfg(test)]
mod tests;
