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

    Ok(PhysicalPlan {
        ops,
        diagnostics: PlanDiagnostics::default(),
        annotations,
        output,
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

fn build_composite_plan_with_binding_kinds_and_options(
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
    } else {
        Ok(plan)
    }
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
                .map(|s| {
                    if *op == CmpOp::Eq {
                        s.is_vertex_property_indexed(property)
                    } else {
                        s.is_vertex_property_range_indexed(property)
                    }
                })
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

#[allow(clippy::too_many_arguments)]
fn plan_simple_statement(
    stmt: &SimpleQueryStatement,
    stage: usize,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    referenced_vars: &BTreeSet<String>,
    schema: &dyn PropertySchema,
    options: PlanBuildOptions<'_>,
    bound_node_vars: &mut BTreeSet<String>,
    optional_node_vars: &mut BTreeSet<String>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    match stmt {
        SimpleQueryStatement::Match(m) => {
            plan_match(
                m,
                stage,
                stats,
                conditional_candidates,
                referenced_vars,
                options,
                bound_node_vars,
                optional_node_vars,
                ops,
                annotations,
            )?;
            Ok(())
        }
        SimpleQueryStatement::Filter(f) => {
            ops.push(PlanOp::Filter {
                condition: f.condition.clone(),
            });
            Ok(())
        }
        SimpleQueryStatement::Let(l) => {
            ops.push(PlanOp::Let {
                bindings: l.bindings.clone(),
            });
            Ok(())
        }
        SimpleQueryStatement::For(f) => {
            ops.push(PlanOp::For {
                variable: f.variable.clone().into(),
                list: f.list.clone(),
                ordinality: f.ordinality.as_ref().map(|o| o.variable.clone().into()),
            });
            Ok(())
        }
        SimpleQueryStatement::OrderBy(o) => {
            ops.push(PlanOp::Sort {
                order_by: o.clone(),
            });
            Ok(())
        }
        SimpleQueryStatement::Limit(l) => {
            ops.push(PlanOp::Limit {
                count: Some(l.count.clone()),
                offset: None,
            });
            Ok(())
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
            Ok(())
        }
        SimpleQueryStatement::Insert(insert_stmt) => {
            plan_insert(insert_stmt, ops, annotations);
            Ok(())
        }
        SimpleQueryStatement::Set(set_stmt) => {
            plan_set(set_stmt, binding_kinds, ops, annotations);
            Ok(())
        }
        SimpleQueryStatement::Remove(remove_stmt) => {
            plan_remove(remove_stmt, binding_kinds, ops, annotations);
            Ok(())
        }
        SimpleQueryStatement::Delete(delete_stmt) => {
            plan_delete(delete_stmt, binding_kinds, ops, annotations);
            Ok(())
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
                name: call
                    .name
                    .parts
                    .iter()
                    .map(|s| Str::from(s.as_str()))
                    .collect(),
                args: call.args.clone(),
                yield_columns,
                optional: call.optional,
            });
            Ok(())
        }
        SimpleQueryStatement::InlineProcedureCall(inline) => {
            let mut sub_plan = build_composite_plan_with_binding_kinds_and_options(
                &inline.body,
                options,
                None,
                schema,
            )?;
            if let Some(graph) = &inline.use_graph {
                let wrapped_ops = std::mem::take(&mut sub_plan.ops);
                sub_plan.ops = vec![PlanOp::UseGraph {
                    graph_name: graph.parts.iter().map(|s| Str::from(s.as_str())).collect(),
                    sub_plan: Some(wrapped_ops),
                }];
            }
            ops.push(PlanOp::InlineProcedureCall {
                sub_plan: Box::new(sub_plan),
                scope_vars: inline
                    .scope_vars
                    .iter()
                    .map(|s| Str::from(s.as_str()))
                    .collect(),
                optional: inline.optional,
            });
            Ok(())
        }
        SimpleQueryStatement::Focused { graph, body } => {
            if let Some(inner) = body {
                let mut sub_ops = Vec::new();
                let mut sub_bound_node_vars = BTreeSet::new();
                let mut sub_optional_node_vars = BTreeSet::new();
                plan_simple_statement(
                    inner,
                    stage,
                    stats,
                    conditional_candidates,
                    binding_kinds,
                    referenced_vars,
                    schema,
                    options,
                    &mut sub_bound_node_vars,
                    &mut sub_optional_node_vars,
                    &mut sub_ops,
                    annotations,
                )?;
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
            Ok(())
        }
    }
}

fn plan_match(
    match_stmt: &MatchStatement,
    stage: usize,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    referenced_vars: &BTreeSet<String>,
    options: PlanBuildOptions<'_>,
    bound_node_vars: &mut BTreeSet<String>,
    optional_node_vars: &mut BTreeSet<String>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    let pattern = &match_stmt.pattern;
    let mut where_conjuncts: Vec<Expr> = pattern
        .where_clause
        .as_ref()
        .map(flatten_conjunction)
        .unwrap_or_default();

    if match_stmt.optional {
        // OPTIONAL MATCH: build sub-plan and wrap in OptionalMatch.
        let mut sub_ops = Vec::new();
        let prior_bound = bound_node_vars.clone();
        let mut sub_bound_node_vars = bound_node_vars.clone();
        let mut sub_optional_node_vars = BTreeSet::new();
        for path_pattern in &pattern.paths {
            plan_path_pattern(
                path_pattern,
                stats,
                conditional_candidates,
                referenced_vars,
                &mut where_conjuncts,
                options,
                &mut sub_bound_node_vars,
                &mut sub_optional_node_vars,
                &mut sub_ops,
                annotations,
            )?;
        }
        if !where_conjuncts.is_empty() {
            sub_ops.push(PlanOp::PropertyFilter {
                predicates: where_conjuncts,
                stage,
            });
        }
        ops.push(PlanOp::OptionalMatch { sub_plan: sub_ops });
        for v in sub_bound_node_vars.difference(&prior_bound) {
            optional_node_vars.insert(v.clone());
        }
        return Ok(());
    }

    // Choose anchor for this match.
    if stage == 0
        && let Some(anchor_info) = anchor::choose_anchor(pattern, stats)
    {
        annotations.optimizer.anchor = Some(anchor_info);
    }

    // Plan each path pattern.
    for path_pattern in &pattern.paths {
        plan_path_pattern(
            path_pattern,
            stats,
            conditional_candidates,
            referenced_vars,
            &mut where_conjuncts,
            options,
            bound_node_vars,
            optional_node_vars,
            ops,
            annotations,
        )?;
    }

    if !where_conjuncts.is_empty() {
        ops.push(PlanOp::PropertyFilter {
            predicates: where_conjuncts,
            stage,
        });
    }
    Ok(())
}

fn plan_path_pattern(
    path: &PathPattern,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    referenced_vars: &BTreeSet<String>,
    where_conjuncts: &mut Vec<Expr>,
    options: PlanBuildOptions<'_>,
    bound_node_vars: &mut BTreeSet<String>,
    optional_node_vars: &mut BTreeSet<String>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
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
    if path.variable.is_some() && shortest_mode.is_none() {
        return Err(PlannerError::UnsupportedPattern(
            "path variables are only supported for shortest-path patterns".into(),
        ));
    }

    let shortest_path_cost = if path.extensions.is_empty() {
        ShortestPathCost::HopCount
    } else {
        let single_edge = match &path.expr {
            PathPatternExpr::Term(term) => extract_single_edge_path_info(term),
            _ => None,
        };
        let ctx = PathPatternExtensionContext {
            prefix: path.prefix.as_ref(),
            extensions: &path.extensions,
            shortest_mode,
            single_edge,
        };
        options.path_extensions.plan_shortest_path_cost(&ctx)?
    };

    // Walk the path expression to emit scan/expand ops.
    plan_path_expr(
        &path.expr,
        shortest_mode,
        shortest_path_cost,
        path.variable.as_deref(),
        stats,
        conditional_candidates,
        referenced_vars,
        where_conjuncts,
        bound_node_vars,
        optional_node_vars,
        ops,
        annotations,
    )?;
    Ok(())
}

fn extract_single_edge_path_info(term: &PathTerm) -> Option<SingleEdgePathInfo> {
    let term = normalize_path_term(term).ok()?;
    if term.factors.len() != 3 {
        return None;
    }
    let PathPrimary::Node(_) = &term.factors[0].primary else {
        return None;
    };
    let PathPrimary::Edge(edge) = &term.factors[1].primary else {
        return None;
    };
    let PathPrimary::Node(_) = &term.factors[2].primary else {
        return None;
    };
    let (label, label_expr) = plan_edge_expand_labels(edge);
    let var_len = term.factors[1]
        .quantifier
        .as_ref()
        .and_then(quantifier_to_var_len);
    Some(SingleEdgePathInfo {
        edge_var: edge.variable.clone(),
        direction: edge.direction,
        label: label.map(|s| s.to_string()),
        label_expr,
        var_len,
    })
}

#[allow(clippy::too_many_arguments)]
fn plan_path_expr(
    expr: &PathPatternExpr,
    shortest_mode: Option<ShortestMode>,
    shortest_path_cost: ShortestPathCost,
    path_var: Option<&str>,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    referenced_vars: &BTreeSet<String>,
    where_conjuncts: &mut Vec<Expr>,
    bound_node_vars: &mut BTreeSet<String>,
    optional_node_vars: &mut BTreeSet<String>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    match expr {
        PathPatternExpr::Term(term) => {
            plan_path_term(
                term,
                shortest_mode,
                shortest_path_cost.clone(),
                path_var,
                stats,
                conditional_candidates,
                referenced_vars,
                where_conjuncts,
                bound_node_vars,
                optional_node_vars,
                ops,
                annotations,
            )?;
        }
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            if let Some(term) = terms.first() {
                plan_path_term(
                    term,
                    shortest_mode,
                    shortest_path_cost.clone(),
                    path_var,
                    stats,
                    conditional_candidates,
                    referenced_vars,
                    where_conjuncts,
                    bound_node_vars,
                    optional_node_vars,
                    ops,
                    annotations,
                )?;
            }
        }
    }
    Ok(())
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
}

fn node_emits_unlabeled_full_vertex_scan(
    var: &str,
    label: &Option<String>,
    node: &NodePattern,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    annotations: &PlanAnnotations,
) -> bool {
    if label.is_some() {
        return false;
    }
    if let Some(stats) = stats
        && let Some(where_expr) = &node.where_clause
        && anchor::find_index_intersection(var, where_expr, stats).is_some()
    {
        return false;
    }
    if let Some(anchor) = &annotations.optimizer.anchor
        && &*anchor.variable == var
    {
        match &anchor.source {
            AnchorSource::PropertyEquality { .. }
            | AnchorSource::InlinePropertyEquality { .. }
            | AnchorSource::PropertyRange { .. } => return false,
            AnchorSource::LabelCardinality { .. }
            | AnchorSource::SchemaEndpoint
            | AnchorSource::FullScan => {}
        }
    }
    conditional_candidates
        .iter()
        .filter(|c| &*c.variable == var)
        .count()
        == 0
}

fn edge_has_indexed_scannable_equality(
    edge_var: &str,
    edge: &EdgePattern,
    stats: Option<&dyn GraphStats>,
    where_conjuncts: &[Expr],
) -> bool {
    let Some(stats) = stats else {
        return false;
    };
    for p in &edge.properties {
        if stats.is_edge_property_indexed(&p.name)
            && anchor::scan_value_from_expr(&p.value).is_some()
        {
            return true;
        }
    }
    for c in where_conjuncts {
        if let Some((v, prop, _)) = parse_edge_var_property_equality(c)
            && v == edge_var
            && stats.is_edge_property_indexed(&prop)
        {
            return true;
        }
    }
    if let Some(w) = &edge.where_clause {
        for c in flatten_conjunction(w) {
            if let Some((v, prop, _)) = parse_edge_var_property_equality(&c)
                && v == edge_var
                && stats.is_edge_property_indexed(&prop)
            {
                return true;
            }
        }
    }
    false
}

fn first_hop_supports_leading_edge_index(
    elements: &[PathElement],
    where_conjuncts: &[Expr],
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    annotations: &PlanAnnotations,
    shortest_mode: Option<ShortestMode>,
) -> bool {
    if shortest_mode.is_some() {
        return false;
    }
    if elements.len() < 3 {
        return false;
    }
    let PathElement::Node { var: nv, node } = &elements[0] else {
        return false;
    };
    let PathElement::Edge {
        edge,
        quantifier,
        var: ev,
    } = &elements[1]
    else {
        return false;
    };
    if quantifier
        .as_ref()
        .and_then(quantifier_to_var_len)
        .is_some()
    {
        return false;
    }
    if !matches!(
        edge.direction,
        EdgeDirection::PointingRight | EdgeDirection::PointingLeft
    ) {
        return false;
    }
    let label = extract_simple_label(&node.label);
    if !node_emits_unlabeled_full_vertex_scan(
        nv,
        &label,
        node,
        stats,
        conditional_candidates,
        annotations,
    ) {
        return false;
    }
    edge_has_indexed_scannable_equality(ev, edge, stats, where_conjuncts)
}

/// Split edge pattern label into a cheap single-name [`PlanOp::Expand::label`] plus an optional
/// [`PlanOp::Expand::label_expr`] for unions, negation, `&`, etc.
fn plan_edge_expand_labels(edge: &EdgePattern) -> (Option<Str>, Option<LabelExpr>) {
    match &edge.label {
        None => (None, None),
        Some(LabelExpr::Name(n)) => (Some(Str::from(n.as_str())), None),
        Some(le) => (None, Some(le.clone())),
    }
}

/// Expand §16.10 simplified path factors into ordinary `Edge` / `Node` factors so existing
/// join-order, cycle detection, and `Expand` planning apply.
fn normalize_path_term(term: &PathTerm) -> Result<PathTerm, PlannerError> {
    let mut out_factors = Vec::with_capacity(term.factors.len().saturating_mul(2));
    for (i, factor) in term.factors.iter().enumerate() {
        match &factor.primary {
            PathPrimary::Simplified(sp) => {
                let n_el = sp.elements.len();
                if n_el == 0 {
                    return Err(PlannerError::UnsupportedPattern(
                        "empty simplified path segment".into(),
                    ));
                }
                let mut eid = 0usize;
                let mut chunks: Vec<Vec<(EdgePattern, Option<PathQuantifier>)>> =
                    Vec::with_capacity(n_el);
                for elem in &sp.elements {
                    chunks.push(lower_simplified_element_edges(elem, i, &mut eid)?);
                }
                let total_edges: usize = chunks.iter().map(|c| c.len()).sum();
                if factor.quantifier.is_some() && total_edges != 1 {
                    return Err(PlannerError::UnsupportedPattern(
                        "path quantifier on multi-segment simplified edge is not supported".into(),
                    ));
                }
                for (j, chunk) in chunks.into_iter().enumerate() {
                    let n_in_chunk = chunk.len();
                    for (k, (edge_pat, inner_q)) in chunk.into_iter().enumerate() {
                        let quantifier = if total_edges == 1 {
                            match (inner_q, factor.quantifier.clone()) {
                                (Some(q), _) => Some(q),
                                (None, outer) => outer,
                            }
                        } else {
                            inner_q
                        };
                        out_factors.push(PathFactor {
                            span: edge_pat.span,
                            primary: PathPrimary::Edge(edge_pat),
                            quantifier,
                        });
                        if k + 1 < n_in_chunk {
                            let mid_var = format!("__simpl_mid_in_{i}_{j}_{k}");
                            out_factors.push(PathFactor {
                                span: factor.span,
                                primary: PathPrimary::Node(NodePattern {
                                    span: factor.span,
                                    variable: Some(mid_var),
                                    is_or_colon: None,
                                    label: None,
                                    properties: vec![],
                                    where_clause: None,
                                }),
                                quantifier: None,
                            });
                        }
                    }
                    if j + 1 < n_el {
                        let mid_var = format!("__simpl_mid_el_{i}_{j}");
                        out_factors.push(PathFactor {
                            span: factor.span,
                            primary: PathPrimary::Node(NodePattern {
                                span: factor.span,
                                variable: Some(mid_var),
                                is_or_colon: None,
                                label: None,
                                properties: vec![],
                                where_clause: None,
                            }),
                            quantifier: None,
                        });
                    }
                }
            }
            _ => out_factors.push(factor.clone()),
        }
    }
    Ok(PathTerm {
        span: term.span,
        factors: out_factors,
    })
}

fn peel_all_groups(mut c: &SimplifiedContents) -> &SimplifiedContents {
    while let SimplifiedContents::Group(inner) = c {
        c = inner.as_ref();
    }
    c
}

/// True when `c` contains `Concatenation` (juxtaposition of factorLows inside §16.12).
fn has_concatenation(c: &SimplifiedContents) -> bool {
    match c {
        SimplifiedContents::Concatenation(_, _) => true,
        SimplifiedContents::Group(inner)
        | SimplifiedContents::Negation(inner)
        | SimplifiedContents::Quantified(inner, _) => has_concatenation(inner),
        SimplifiedContents::DirectionOverride(_, inner) => has_concatenation(inner),
        SimplifiedContents::Conjunction(a, b)
        | SimplifiedContents::Union(a, b)
        | SimplifiedContents::MultisetAlternation(a, b) => {
            has_concatenation(a) || has_concatenation(b)
        }
        SimplifiedContents::Label(_) => false,
    }
}

fn flatten_alt_branches(c: &SimplifiedContents) -> Vec<&SimplifiedContents> {
    match c {
        SimplifiedContents::Union(a, b) | SimplifiedContents::MultisetAlternation(a, b) => {
            let mut v = flatten_alt_branches(a);
            v.extend(flatten_alt_branches(b));
            v
        }
        _ => vec![c],
    }
}

fn flatten_concat_branches(c: &SimplifiedContents) -> Vec<&SimplifiedContents> {
    match c {
        SimplifiedContents::Concatenation(a, b) => {
            let mut v = flatten_concat_branches(a);
            v.extend(flatten_concat_branches(b));
            v
        }
        _ => vec![c],
    }
}

/// One slash-delimited simplified element (`-/ ... /->` etc.) → 1+ edge tuples.
fn lower_simplified_element_edges(
    elem: &SimplifiedElement,
    factor_idx: usize,
    eid: &mut usize,
) -> Result<Vec<(EdgePattern, Option<PathQuantifier>)>, PlannerError> {
    let c = peel_all_groups(&elem.contents);
    match c {
        SimplifiedContents::Union(_, _) | SimplifiedContents::MultisetAlternation(_, _) => {
            let branches = flatten_alt_branches(c);
            if branches.is_empty() {
                return Err(PlannerError::UnsupportedPattern(
                    "empty simplified path alternative".into(),
                ));
            }
            let mut merged_dir: Option<EdgeDirection> = None;
            let mut label_acc: Option<LabelExpr> = None;
            for b in &branches {
                if has_concatenation(b) {
                    return Err(PlannerError::UnsupportedPattern(
                        "union or |+| combined with concatenated simplified hops is not supported by the planner".into(),
                    ));
                }
                let b = peel_all_groups(b);
                let (branch_q, after_q) = peel_simplified_quantifier(b);
                if branch_q.is_some() {
                    return Err(PlannerError::UnsupportedPattern(
                        "quantified alternatives in a simplified path are not supported by the planner".into(),
                    ));
                }
                let (dir, rest) = peel_simplified_direction_overrides(elem.direction, after_q)?;
                let lbl = simplified_contents_to_label_expr(rest)?;
                match merged_dir {
                    None => merged_dir = Some(dir),
                    Some(d) if d == dir => {}
                    _ => {
                        return Err(PlannerError::UnsupportedPattern(
                            "simplified path alternatives with different directions are not supported by the planner".into(),
                        ));
                    }
                }
                label_acc = Some(match label_acc {
                    None => lbl,
                    Some(prev) => LabelExpr::Or(Box::new(prev), Box::new(lbl)),
                });
            }
            let j = *eid;
            *eid += 1;
            Ok(vec![(
                EdgePattern {
                    span: elem.span,
                    direction: merged_dir.expect("non-empty branches"),
                    variable: Some(format!("__simpl_e{factor_idx}_{j}")),
                    is_or_colon: None,
                    label: Some(label_acc.expect("non-empty branches")),
                    properties: vec![],
                    where_clause: None,
                },
                None,
            )])
        }
        _ => {
            let parts = flatten_concat_branches(c);
            let mut out = Vec::new();
            for p in parts {
                out.append(&mut lower_factor_low_maybe_multi(elem, p, factor_idx, eid)?);
            }
            Ok(out)
        }
    }
}

fn lower_factor_low_maybe_multi(
    elem: &SimplifiedElement,
    factor_low: &SimplifiedContents,
    factor_idx: usize,
    eid: &mut usize,
) -> Result<Vec<(EdgePattern, Option<PathQuantifier>)>, PlannerError> {
    let (quant, after_q) = peel_simplified_quantifier(factor_low);
    let after_q = peel_all_groups(after_q);
    if let SimplifiedContents::Concatenation(_, _) = after_q {
        if quant.is_some() {
            return Err(PlannerError::UnsupportedPattern(
                "quantifier on a concatenated simplified path group is not supported".into(),
            ));
        }
        let mut v = Vec::new();
        for p in flatten_concat_branches(after_q) {
            v.push(lower_one_simplified_edge_piece(
                elem, p, factor_idx, *eid, None,
            )?);
            *eid += 1;
        }
        Ok(v)
    } else {
        let e = lower_one_simplified_edge_piece(elem, after_q, factor_idx, *eid, quant)?;
        *eid += 1;
        Ok(vec![e])
    }
}

fn lower_one_simplified_edge_piece(
    elem: &SimplifiedElement,
    piece: &SimplifiedContents,
    factor_idx: usize,
    j: usize,
    forced_quant: Option<PathQuantifier>,
) -> Result<(EdgePattern, Option<PathQuantifier>), PlannerError> {
    let (inner_q, after_q) = peel_simplified_quantifier(piece);
    if inner_q.is_some() && forced_quant.is_some() {
        return Err(PlannerError::UnsupportedPattern(
            "conflicting quantifiers on simplified path piece".into(),
        ));
    }
    let quant = inner_q.or(forced_quant);
    let (direction, rest) = peel_simplified_direction_overrides(elem.direction, after_q)?;
    if has_concatenation(rest) {
        return Err(PlannerError::UnsupportedPattern(
            "nested simplified path concatenation is not supported by the planner".into(),
        ));
    }
    let label = simplified_contents_to_label_expr(rest)?;
    Ok((
        EdgePattern {
            span: elem.span,
            direction,
            variable: Some(format!("__simpl_e{factor_idx}_{j}")),
            is_or_colon: None,
            label: Some(label),
            properties: vec![],
            where_clause: None,
        },
        quant,
    ))
}

fn peel_simplified_quantifier(
    c: &SimplifiedContents,
) -> (Option<PathQuantifier>, &SimplifiedContents) {
    match c {
        SimplifiedContents::Quantified(inner, q) => (Some(q.clone()), inner.as_ref()),
        SimplifiedContents::Group(inner) => peel_simplified_quantifier(inner),
        _ => (None, c),
    }
}

fn peel_simplified_direction_overrides(
    mut dir: EdgeDirection,
    mut c: &SimplifiedContents,
) -> Result<(EdgeDirection, &SimplifiedContents), PlannerError> {
    loop {
        match c {
            SimplifiedContents::Group(inner) => c = inner,
            SimplifiedContents::DirectionOverride(d, inner) => {
                dir = *d;
                c = inner;
            }
            SimplifiedContents::Quantified(_, _) => {
                return Err(PlannerError::UnsupportedPattern(
                    "mis-ordered quantifier inside simplified path".into(),
                ));
            }
            _ => return Ok((dir, c)),
        }
    }
}

fn simplified_contents_to_label_expr(c: &SimplifiedContents) -> Result<LabelExpr, PlannerError> {
    match c {
        SimplifiedContents::Label(le) => Ok(le.clone()),
        SimplifiedContents::Group(inner) => simplified_contents_to_label_expr(inner),
        SimplifiedContents::Conjunction(a, b) => Ok(LabelExpr::And(
            Box::new(simplified_contents_to_label_expr(a)?),
            Box::new(simplified_contents_to_label_expr(b)?),
        )),
        SimplifiedContents::Union(a, b) => Ok(LabelExpr::Or(
            Box::new(simplified_contents_to_label_expr(a)?),
            Box::new(simplified_contents_to_label_expr(b)?),
        )),
        // Multiset alternation (|+|): planner treats like set union for edge typing; multiplicity is not modeled.
        SimplifiedContents::MultisetAlternation(a, b) => Ok(LabelExpr::Or(
            Box::new(simplified_contents_to_label_expr(a)?),
            Box::new(simplified_contents_to_label_expr(b)?),
        )),
        SimplifiedContents::Negation(inner) => Ok(LabelExpr::Not(Box::new(
            simplified_contents_to_label_expr(inner)?,
        ))),
        SimplifiedContents::Concatenation(_, _) => Err(PlannerError::UnsupportedPattern(
            "concatenated simplified path should be lowered before label conversion".into(),
        )),
        SimplifiedContents::Quantified(_, _) => Err(PlannerError::UnsupportedPattern(
            "unexpected quantifier while lowering simplified path".into(),
        )),
        SimplifiedContents::DirectionOverride(_, _) => Err(PlannerError::UnsupportedPattern(
            "unexpected direction override while lowering simplified path".into(),
        )),
    }
}

/// Per-hop auxiliary binding for [`PlanOp::Expand`] / [`PlanOp::ExpandFilter`] / [`PlanOp::EdgeBindEndpoints`]
/// when the linear query references `{edge_var}__hop_aux`.
fn hop_aux_binding_for_edge_if_referenced(
    edge_var: &str,
    referenced: &BTreeSet<String>,
) -> Option<Str> {
    let name = format!("{edge_var}__hop_aux");
    referenced.contains(&name).then_some(name.into())
}

#[allow(clippy::too_many_arguments)]
fn plan_path_term(
    term: &PathTerm,
    shortest_mode: Option<ShortestMode>,
    shortest_path_cost: ShortestPathCost,
    path_var: Option<&str>,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    referenced_vars: &BTreeSet<String>,
    where_conjuncts: &mut Vec<Expr>,
    bound_node_vars: &mut BTreeSet<String>,
    optional_node_vars: &mut BTreeSet<String>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    let term = normalize_path_term(term)?;
    // Compute join ordering and detect cyclic patterns.
    let hops = join_order::extract_hops(&term);
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
            PathPrimary::Simplified(_) => {
                unreachable!("normalize_path_term should remove simplified primaries")
            }
        })
        .collect();

    let leading_first_hop_eligible = first_hop_supports_leading_edge_index(
        &elements,
        where_conjuncts.as_slice(),
        stats,
        conditional_candidates,
        annotations,
        shortest_mode,
    );

    let entry_bound_node_vars = bound_node_vars.clone();
    let entry_optional_node_vars = optional_node_vars.clone();

    let mut prev_node_var: Option<String> = None;
    let mut pending_deferred_first_scan: Option<(String, Option<String>, NodePattern)> = None;
    // Track nodes whose inline filters were fused into ExpandFilter.
    let mut fused_nodes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut enforced_reuse_nodes: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut seen_path_node_vars: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut path_bound_node_vars: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();

    for (idx, elem) in elements.iter().enumerate() {
        match elem {
            PathElement::Node { var, node } => {
                let label = extract_simple_label(&node.label);

                let reuse_from_prior_match =
                    entry_bound_node_vars.contains(var) || entry_optional_node_vars.contains(var);
                let reuse_within_path = seen_path_node_vars.contains(var);
                seen_path_node_vars.insert(var.clone());

                if reuse_from_prior_match || reuse_within_path {
                    if !enforced_reuse_nodes.contains(var) {
                        emit_bound_node_pattern_checks(
                            var,
                            node,
                            optional_node_vars.contains(var),
                            ops,
                        );
                        enforced_reuse_nodes.insert(var.clone());
                    }
                } else if prev_node_var.is_none() && !path_bound_node_vars.contains(var) {
                    if leading_first_hop_eligible {
                        pending_deferred_first_scan =
                            Some((var.clone(), label.clone(), node.clone()));
                    } else {
                        emit_scan_for_node(
                            var,
                            &label,
                            node,
                            stats,
                            conditional_candidates,
                            ops,
                            annotations,
                        );
                        bound_node_vars.insert(var.clone());
                        path_bound_node_vars.insert(var.clone());
                    }
                }

                if !fused_nodes.contains(var) {
                    let defer_near = leading_first_hop_eligible
                        && idx == 0
                        && pending_deferred_first_scan.is_some();
                    if !defer_near {
                        emit_node_inline_filters(var, node, ops);
                    }
                }

                prev_node_var = Some(var.clone());
            }
            PathElement::Edge {
                var: edge_var,
                edge,
                quantifier,
            } => {
                let (label_str, label_expr) = plan_edge_expand_labels(edge);

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

                    let try_leading = idx == 1
                        && shortest_mode.is_none()
                        && pending_deferred_first_scan
                            .as_ref()
                            .is_some_and(|p| p.0 == *src_var);
                    let mut wc_clone = where_conjuncts.clone();
                    let fusion_on_clone =
                        plan_edge_filter_fusion(edge_var, edge, stats, &mut wc_clone);
                    let use_leading = try_leading
                        && fusion_on_clone.indexed_equality.is_some()
                        && var_len.is_none()
                        && label_expr.is_none();

                    if use_leading {
                        *where_conjuncts = wc_clone;
                        let (_, _, near_node) = pending_deferred_first_scan.take().unwrap();
                        let (prop, scan_val) = fusion_on_clone.indexed_equality.as_ref().unwrap();
                        ops.push(PlanOp::EdgeIndexScan {
                            variable: edge_str.clone(),
                            property: prop.clone(),
                            value: scan_val.clone(),
                            property_projection: None,
                        });
                        ops.push(PlanOp::EdgeBindEndpoints {
                            edge: edge_str.clone(),
                            near: src_str.clone(),
                            far: dst_str.clone(),
                            direction: edge.direction,
                            label: label_str.clone(),
                            near_property_projection: None,
                            far_property_projection: None,
                            hop_aux_binding: hop_aux_binding_for_edge_if_referenced(
                                edge_var,
                                referenced_vars,
                            ),
                        });
                        emit_edge_inline_filters(edge_var, edge, &fusion_on_clone, ops);
                        bound_node_vars.insert(src_var.clone());
                        bound_node_vars.insert(dst_var.clone());
                        path_bound_node_vars.insert(src_var.clone());
                        path_bound_node_vars.insert(dst_var.clone());
                        if !dst_filters.is_empty() {
                            ops.push(PlanOp::PropertyFilter {
                                predicates: dst_filters,
                                stage: 0,
                            });
                            fused_nodes.insert(dst_var.clone());
                        }
                        emit_node_inline_filters(src_var, &near_node, ops);
                    } else {
                        if let Some((v, lbl, n)) = pending_deferred_first_scan.take() {
                            emit_scan_for_node(
                                &v,
                                &lbl,
                                &n,
                                stats,
                                conditional_candidates,
                                ops,
                                annotations,
                            );
                            bound_node_vars.insert(v.clone());
                            path_bound_node_vars.insert(v);
                        }

                        let edge_fusion = if shortest_mode.is_some() {
                            EdgeFilterFusion::default()
                        } else {
                            plan_edge_filter_fusion(edge_var, edge, stats, where_conjuncts)
                        };
                        let indexed_edge_equality = edge_fusion.indexed_equality.clone();

                        if let Some(mode) = shortest_mode {
                            if let Some(dst_node) = dst_node.as_ref() {
                                if !entry_bound_node_vars.contains(&dst_var)
                                    && !entry_optional_node_vars.contains(&dst_var)
                                    && !bound_node_vars.contains(&dst_var)
                                    && !optional_node_vars.contains(&dst_var)
                                {
                                    let dst_label = extract_simple_label(&dst_node.label);
                                    emit_scan_for_node(
                                        &dst_var,
                                        &dst_label,
                                        dst_node,
                                        stats,
                                        conditional_candidates,
                                        ops,
                                        annotations,
                                    );
                                    bound_node_vars.insert(dst_var.clone());
                                    path_bound_node_vars.insert(dst_var.clone());
                                } else if !enforced_reuse_nodes.contains(&dst_var) {
                                    emit_bound_node_pattern_checks(
                                        &dst_var,
                                        dst_node,
                                        optional_node_vars.contains(&dst_var),
                                        ops,
                                    );
                                    enforced_reuse_nodes.insert(dst_var.clone());
                                }
                            }
                            ops.push(PlanOp::ShortestPath {
                                src: src_str,
                                dst: dst_str.clone(),
                                edge: edge_str,
                                path_var: path_var.map(Into::into),
                                emit_edge_binding: true,
                                emit_path_binding: true,
                                mode,
                                direction: edge.direction,
                                label: label_str.clone(),
                                label_expr,
                                var_len,
                                cost: shortest_path_cost.clone(),
                            });
                            if !dst_filters.is_empty() {
                                ops.push(PlanOp::PropertyFilter {
                                    predicates: dst_filters,
                                    stage: 0,
                                });
                            }
                        } else if !dst_filters.is_empty() {
                            ops.push(PlanOp::ExpandFilter {
                                src: src_str,
                                edge: edge_str,
                                dst: dst_str.clone(),
                                direction: edge.direction,
                                label: label_str,
                                label_expr,
                                var_len,
                                indexed_edge_equality,
                                dst_filter: dst_filters,
                                edge_property_projection: None,
                                dst_property_projection: None,
                                hop_aux_binding: hop_aux_binding_for_edge_if_referenced(
                                    edge_var,
                                    referenced_vars,
                                ),
                                emit_edge_binding: true,
                            });
                            bound_node_vars.insert(dst_var.clone());
                            path_bound_node_vars.insert(dst_var.clone());
                            fused_nodes.insert(dst_var.clone());
                        } else {
                            ops.push(PlanOp::Expand {
                                src: src_str,
                                edge: edge_str,
                                dst: dst_str,
                                direction: edge.direction,
                                label: label_str,
                                label_expr,
                                var_len,
                                indexed_edge_equality,
                                edge_property_projection: None,
                                dst_property_projection: None,
                                hop_aux_binding: hop_aux_binding_for_edge_if_referenced(
                                    edge_var,
                                    referenced_vars,
                                ),
                                emit_edge_binding: true,
                            });
                            bound_node_vars.insert(dst_var.clone());
                            path_bound_node_vars.insert(dst_var.clone());
                        }

                        emit_edge_inline_filters(edge_var, edge, &edge_fusion, ops);
                    }
                }
                prev_node_var = None;
            }
            PathElement::Sub(expr) => {
                plan_path_expr(
                    expr,
                    shortest_mode,
                    shortest_path_cost.clone(),
                    path_var,
                    stats,
                    conditional_candidates,
                    referenced_vars,
                    where_conjuncts,
                    bound_node_vars,
                    optional_node_vars,
                    ops,
                    annotations,
                )?;
            }
        }
    }
    Ok(())
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

fn emit_bound_node_pattern_checks(
    var: &str,
    node: &NodePattern,
    require_non_null: bool,
    ops: &mut Vec<PlanOp>,
) {
    let mut predicates = Vec::new();
    if require_non_null {
        predicates.push(Expr::new(ExprKind::IsNotNull(Box::new(Expr::var(var)))));
    }
    if let Some(label) = &node.label {
        predicates.push(Expr::new(ExprKind::IsLabeled {
            expr: Box::new(Expr::var(var)),
            label: label.clone(),
            negated: false,
        }));
    }
    if !predicates.is_empty() {
        ops.push(PlanOp::PropertyFilter {
            predicates,
            stage: 0,
        });
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

/// Planner-only: indexed edge equality plus residual edge filters.
#[derive(Default, Clone)]
struct EdgeFilterFusion {
    indexed_equality: Option<(Str, ScanValue)>,
    skip_inline_prop: Option<String>,
    /// `None`: emit full `edge.where_clause`. `Some(predicates)` emits only these (empty = omit).
    edge_where_override: Option<Vec<Expr>>,
}

fn plan_edge_filter_fusion(
    edge_var: &str,
    edge: &EdgePattern,
    stats: Option<&dyn GraphStats>,
    where_conjuncts: &mut Vec<Expr>,
) -> EdgeFilterFusion {
    let mut out = EdgeFilterFusion::default();
    let Some(stats) = stats else {
        return out;
    };

    for p in &edge.properties {
        if stats.is_edge_property_indexed(&p.name)
            && let Some(sv) = anchor::scan_value_from_expr(&p.value)
        {
            out.indexed_equality = Some((p.name.clone().into(), sv));
            out.skip_inline_prop = Some(p.name.clone());
            strip_edge_var_prop_eq_from_where(where_conjuncts, edge_var, &p.name);
            out.edge_where_override = edge_where_after_fusing_prop(edge, edge_var, &p.name);
            return out;
        }
    }

    if let Some((idx, prop, sv)) =
        find_first_indexed_edge_eq_in_conjunctions(where_conjuncts, edge_var, stats)
    {
        where_conjuncts.remove(idx);
        out.indexed_equality = Some((prop.into(), sv));
        return out;
    }

    if let Some(where_clause) = edge.where_clause.as_ref() {
        let mut conj = flatten_conjunction(where_clause);
        if let Some((idx, prop, sv)) =
            find_first_indexed_edge_eq_in_conjunctions(&conj, edge_var, stats)
        {
            conj.remove(idx);
            out.indexed_equality = Some((prop.into(), sv));
            out.edge_where_override = Some(conj);
        }
    }

    out
}

fn find_first_indexed_edge_eq_in_conjunctions(
    conjuncts: &[Expr],
    edge_var: &str,
    stats: &dyn GraphStats,
) -> Option<(usize, String, ScanValue)> {
    for (i, c) in conjuncts.iter().enumerate() {
        if let Some((v, p, sv)) = parse_edge_var_property_equality(c)
            && v == edge_var
            && stats.is_edge_property_indexed(&p)
        {
            return Some((i, p, sv));
        }
    }
    None
}

fn parse_edge_var_property_equality(expr: &Expr) -> Option<(String, String, ScanValue)> {
    if let ExprKind::Compare { left, op, right } = &expr.kind
        && *op == CmpOp::Eq
        && let ExprKind::PropertyAccess {
            expr: inner,
            property,
        } = &left.kind
        && let ExprKind::Variable(v) = &inner.kind
    {
        return anchor::scan_value_from_expr(right).map(|sv| (v.clone(), property.clone(), sv));
    }
    None
}

fn strip_edge_var_prop_eq_from_where(where_conjuncts: &mut Vec<Expr>, edge_var: &str, prop: &str) {
    where_conjuncts.retain(|c| {
        !parse_edge_var_property_equality(c).is_some_and(|(v, p, _)| v == edge_var && p == prop)
    });
}

fn edge_where_after_fusing_prop(
    edge: &EdgePattern,
    edge_var: &str,
    fused_prop: &str,
) -> Option<Vec<Expr>> {
    edge.where_clause.as_ref()?;
    let mut conj = flatten_conjunction(edge.where_clause.as_ref().unwrap());
    let orig_len = conj.len();
    conj.retain(|c| {
        !parse_edge_var_property_equality(c)
            .is_some_and(|(v, p, _)| v == edge_var && p == fused_prop)
    });
    if conj.len() == orig_len {
        None
    } else {
        Some(conj)
    }
}

fn emit_edge_inline_filters(
    edge_var: &str,
    edge: &EdgePattern,
    fusion: &EdgeFilterFusion,
    ops: &mut Vec<PlanOp>,
) {
    let filter_exprs: Vec<Expr> = edge
        .properties
        .iter()
        .filter(|p| fusion.skip_inline_prop.as_deref() != Some(p.name.as_str()))
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
    if !filter_exprs.is_empty() {
        ops.push(PlanOp::PropertyFilter {
            predicates: filter_exprs,
            stage: 0,
        });
    }

    match &fusion.edge_where_override {
        None => {
            if let Some(where_expr) = &edge.where_clause {
                ops.push(PlanOp::PropertyFilter {
                    predicates: flatten_conjunction(where_expr),
                    stage: 0,
                });
            }
        }
        Some(preds) if !preds.is_empty() => {
            ops.push(PlanOp::PropertyFilter {
                predicates: preds.clone(),
                stage: 0,
            });
        }
        Some(_) => {}
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
        && let Some(specs) = anchor::find_index_intersection(var, where_expr, stats)
    {
        ops.push(PlanOp::IndexIntersection {
            variable: Str::from(var),
            scans: specs,
            property_projection: None,
        });
        return;
    }

    // Check if anchor selection found an index scan for this variable.
    if let Some(anchor) = &annotations.optimizer.anchor
        && &*anchor.variable == var
    {
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
                        node.where_clause
                            .as_ref()
                            .and_then(|w| find_equality_value_in_where(var, property, w))
                    })
                    .unwrap_or(ScanValue::Parameter(format!("${}", property).into()));

                ops.push(PlanOp::IndexScan {
                    variable: Str::from(var),
                    property: property.clone(),
                    value: scan_value,
                    cmp: CmpOp::Eq,
                    property_projection: None,
                });
                return;
            }
            AnchorSource::PropertyRange {
                property,
                value,
                cmp,
            } => {
                ops.push(PlanOp::IndexScan {
                    variable: Str::from(var),
                    property: property.clone(),
                    value: value.clone(),
                    cmp: *cmp,
                    property_projection: None,
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
            property_projection: None,
        });
        return;
    }

    // Default: NodeScan.
    ops.push(PlanOp::NodeScan {
        variable: Str::from(var),
        label: label.as_ref().map(|s| Str::from(s.as_str())),
        property_projection: None,
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
            && v == var
            && prop == property
        {
            return Some(expr_to_scan_value(right));
        }
    }
    None
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
        #[cfg(feature = "cypher")]
        ReturnBody::NoBindings => {
            // Cypher extension: explicit empty return bindings.
            // Produces an output row set with zero projected columns.
            ops.push(PlanOp::Project {
                columns: vec![],
                distinct,
            });
        }
        ReturnBody::Items {
            items,
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => {
            let having_rw = rewrite_having_with_return_aliases(items, having.as_ref());
            // Aggregation.
            if let Some(gb) = group_by {
                let (agg_specs, proj_cols) = extract_aggregates(items, having_rw.as_ref());
                ops.push(PlanOp::Aggregate {
                    group_by: gb.items.clone(),
                    aggregates: agg_specs,
                });
                if let Some(h) = having_rw {
                    ops.push(PlanOp::Filter { condition: h });
                }
                ops.push(PlanOp::Project {
                    columns: proj_cols,
                    distinct,
                });
            } else if items.iter().any(|item| expr_contains_aggregate(&item.expr))
                || having_rw.as_ref().is_some_and(expr_contains_aggregate)
            {
                // Implicit whole-result aggregation (no GROUP BY): executor needs `Aggregate`
                // before `Project`; bare `Aggregate` exprs in `Project` are not evaluable.
                let (agg_specs, proj_cols) = extract_aggregates(items, having_rw.as_ref());
                ops.push(PlanOp::Aggregate {
                    group_by: Vec::new(),
                    aggregates: agg_specs,
                });
                if let Some(h) = having_rw {
                    ops.push(PlanOp::Filter { condition: h });
                }
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

    let (items, group_by, having, order_by, limit, offset) = match &sel.body {
        SelectBody::Star {
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => (None, group_by, having, order_by, limit, offset),
        SelectBody::Items {
            items,
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => (Some(items), group_by, having, order_by, limit, offset),
    };

    if let Some(gb) = group_by {
        if let Some(items) = items {
            let having_rw = rewrite_having_with_return_aliases(items, having.as_ref());
            let (agg_specs, proj_cols) = extract_aggregates(items, having_rw.as_ref());
            ops.push(PlanOp::Aggregate {
                group_by: gb.items.clone(),
                aggregates: agg_specs,
            });
            if let Some(h) = having_rw {
                ops.push(PlanOp::Filter { condition: h });
            }
            ops.push(PlanOp::Project {
                columns: proj_cols,
                distinct,
            });
        }
    } else if let Some(items) = items {
        let having_rw = rewrite_having_with_return_aliases(items, having.as_ref());
        if items.iter().any(|item| expr_contains_aggregate(&item.expr))
            || having_rw.as_ref().is_some_and(expr_contains_aggregate)
        {
            let (agg_specs, proj_cols) = extract_aggregates(items, having_rw.as_ref());
            ops.push(PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: agg_specs,
            });
            if let Some(h) = having_rw {
                ops.push(PlanOp::Filter { condition: h });
            }
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

/// Expand `RETURN`/`SELECT` column aliases inside `HAVING` so post-aggregate filtering runs on
/// expressions that are actually bound on aggregate rows (aggregates and grouping keys).
fn rewrite_having_with_return_aliases(items: &[ReturnItem], having: Option<&Expr>) -> Option<Expr> {
    let aliases: BTreeMap<String, Expr> = items
        .iter()
        .filter_map(|item| {
            let alias = item.alias.as_ref()?;
            Some((alias.clone(), item.expr.clone()))
        })
        .collect();
    having.map(|h| substitute_return_aliases_in_expr(h, &aliases))
}

/// True when `expr` contains any [`ExprKind::Aggregate`] (including nested).
fn expr_contains_aggregate(expr: &Expr) -> bool {
    if matches!(&expr.kind, ExprKind::Aggregate { .. }) {
        return true;
    }
    let mut found = false;
    for_each_immediate_child_expr(expr, |child| {
        found |= expr_contains_aggregate(child);
    });
    found
}

/// Compare aggregate specs ignoring output alias (used for deduplication).
fn aggregate_spec_body_eq(a: &AggregateSpec, b: &AggregateSpec) -> bool {
    a.func == b.func
        && a.distinct == b.distinct
        && a.expr == b.expr
        && a.expr2 == b.expr2
        && a.filter == b.filter
        && a.order_by == b.order_by
}

/// DFS collect unique [`AggregateSpec`] bodies from `expr` in stable pre-order.
fn collect_unique_aggregate_specs_from_expr(expr: &Expr, out: &mut Vec<AggregateSpec>) {
    if let ExprKind::Aggregate { .. } = &expr.kind
        && let Some(spec) = try_extract_aggregate(expr)
        && !out.iter().any(|s| aggregate_spec_body_eq(s, &spec))
    {
        out.push(spec);
    }
    for_each_immediate_child_expr(expr, |child| {
        collect_unique_aggregate_specs_from_expr(child, out);
    });
}

/// Extract aggregate functions from return items and optional `HAVING`.
fn extract_aggregates(
    items: &[ReturnItem],
    having: Option<&Expr>,
) -> (Vec<AggregateSpec>, Vec<ProjectColumn>) {
    let mut agg_specs = Vec::new();
    for item in items {
        collect_unique_aggregate_specs_from_expr(&item.expr, &mut agg_specs);
    }
    if let Some(h) = having {
        collect_unique_aggregate_specs_from_expr(h, &mut agg_specs);
    }
    let proj_cols = items
        .iter()
        .map(|item| ProjectColumn {
            expr: item.expr.clone(),
            alias: item.alias.as_ref().map(|a| Str::from(a.as_str())),
        })
        .collect();

    (agg_specs, proj_cols)
}

/// Try to extract an aggregate function from an expression.
fn try_extract_aggregate(expr: &Expr) -> Option<AggregateSpec> {
    let ExprKind::Aggregate {
        func,
        expr: agg_expr,
        expr2,
        distinct,
        order_by,
        filter,
    } = &expr.kind
    else {
        return None;
    };
    Some(AggregateSpec {
        func: *func,
        expr: agg_expr.as_deref().cloned(),
        expr2: expr2.as_deref().cloned(),
        distinct: *distinct,
        filter: filter.as_deref().cloned(),
        order_by: order_by.clone(),
        alias: None,
    })
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
#[allow(clippy::too_many_arguments)]
fn plan_bushy_join(
    groups: &[Vec<usize>],
    parts: &[SimpleQueryStatement],
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    referenced_vars: &BTreeSet<String>,
    schema: &dyn PropertySchema,
    options: PlanBuildOptions<'_>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    let mut sub_plans: Vec<Vec<PlanOp>> = Vec::new();
    let mut sub_vars: Vec<std::collections::BTreeSet<String>> = Vec::new();

    for group in groups {
        let mut group_ops = Vec::new();
        let mut group_vars = std::collections::BTreeSet::new();
        let mut bound_node_vars = BTreeSet::new();
        let mut optional_node_vars = BTreeSet::new();

        for &idx in group {
            plan_simple_statement(
                &parts[idx],
                idx,
                stats,
                conditional_candidates,
                binding_kinds,
                referenced_vars,
                schema,
                options,
                &mut bound_node_vars,
                &mut optional_node_vars,
                &mut group_ops,
                annotations,
            )?;
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

    for (plan, vars) in sub_plans.into_iter().zip(sub_vars) {
        let shared: Vec<String> = result_vars.intersection(&vars).cloned().collect();

        let left = std::mem::take(&mut result_ops);
        if shared.is_empty() {
            result_ops = vec![PlanOp::CartesianProduct { left, right: plan }];
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
    Ok(())
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
        && rows > 100_000.0
        && annotations.optimizer.reoptimize_after_rows.is_none()
    {
        annotations.optimizer.reoptimize_after_rows = Some(10_000);
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// WCOJ Replacement
// ════════════════════════════════════════════════════════════════════════════════

/// Replace `Expand` / `ExpandFilter` chains that close a detected cycle with
/// [`PlanOp::WorstCaseOptimalJoin`] when every hop can be represented on the edge ring.
///
/// Skips fusion when any hop combines **`indexed_edge_equality`** with **`var_len`** (executor uses
/// plain expansion for variable-length segments).
fn apply_wcoj_replacement(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    let cycles = match &annotations.optimizer.cyclic_patterns {
        Some(c) if !c.is_empty() => c.clone(),
        _ => return,
    };

    for cycle in &cycles {
        let uniq = normalize_cycle_variables(&cycle.variables);
        if uniq.len() < 3 {
            continue;
        }
        let n = uniq.len();

        let mut expand_hops: Vec<CollectedWcojHop> = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            match op {
                PlanOp::Expand {
                    src,
                    dst,
                    edge,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    indexed_edge_equality,
                    hop_aux_binding,
                    ..
                } => {
                    expand_hops.push(CollectedWcojHop {
                        op_idx: i,
                        src: src.clone(),
                        dst: dst.clone(),
                        edge: edge.clone(),
                        label: label.clone(),
                        label_expr: label_expr.clone(),
                        direction: *direction,
                        var_len: *var_len,
                        indexed_edge_equality: indexed_edge_equality.clone(),
                        dst_filter: Vec::new(),
                        hop_aux_binding: hop_aux_binding.clone(),
                    });
                }
                PlanOp::ExpandFilter {
                    src,
                    dst,
                    edge,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    indexed_edge_equality,
                    dst_filter,
                    hop_aux_binding,
                    ..
                } => {
                    expand_hops.push(CollectedWcojHop {
                        op_idx: i,
                        src: src.clone(),
                        dst: dst.clone(),
                        edge: edge.clone(),
                        label: label.clone(),
                        label_expr: label_expr.clone(),
                        direction: *direction,
                        var_len: *var_len,
                        indexed_edge_equality: indexed_edge_equality.clone(),
                        dst_filter: dst_filter.clone(),
                        hop_aux_binding: hop_aux_binding.clone(),
                    });
                }
                _ => {}
            }
        }

        if expand_hops
            .iter()
            .any(|h| h.var_len.is_some() && h.indexed_edge_equality.is_some())
        {
            continue;
        }

        let Some((ordered_edges, mut remove_indices)) =
            order_wcoj_edges_for_cycle(&uniq, &expand_hops)
        else {
            continue;
        };
        if ordered_edges.len() != n || remove_indices.len() != n {
            continue;
        }
        remove_indices.sort_unstable();
        for &idx in remove_indices.iter().rev() {
            ops.remove(idx);
        }
        let insert_pos = remove_indices[0].min(ops.len());
        ops.insert(
            insert_pos,
            PlanOp::WorstCaseOptimalJoin {
                variables: uniq,
                edges: ordered_edges,
            },
        );
        break;
    }
}

fn normalize_cycle_variables(variables: &[Str]) -> Vec<Str> {
    if variables.len() >= 2 && variables.first() == variables.last() {
        variables[..variables.len() - 1].to_vec()
    } else {
        variables.to_vec()
    }
}

#[derive(Clone, Debug)]
struct CollectedWcojHop {
    op_idx: usize,
    src: Str,
    dst: Str,
    edge: Str,
    label: Option<Str>,
    label_expr: Option<LabelExpr>,
    direction: EdgeDirection,
    var_len: Option<VarLenSpec>,
    indexed_edge_equality: Option<(Str, ScanValue)>,
    dst_filter: Vec<Expr>,
    hop_aux_binding: Option<Str>,
}

fn order_wcoj_edges_for_cycle(
    uniq: &[Str],
    expands: &[CollectedWcojHop],
) -> Option<(Vec<WcojEdge>, Vec<usize>)> {
    let n = uniq.len();
    let mut used = vec![false; expands.len()];
    let mut out_edges = Vec::with_capacity(n);
    let mut remove_indices = Vec::with_capacity(n);

    for i in 0..n {
        let src = &uniq[i];
        let dst = &uniq[(i + 1) % n];
        let mut found = None;
        for (j, hop) in expands.iter().enumerate() {
            if used[j] {
                continue;
            }
            if hop.src == *src && hop.dst == *dst {
                found = Some((
                    j,
                    WcojEdge {
                        src: src.clone(),
                        dst: dst.clone(),
                        variable: hop.edge.clone(),
                        label: hop.label.clone(),
                        label_expr: hop.label_expr.clone(),
                        direction: hop.direction,
                        var_len: hop.var_len,
                        indexed_edge_equality: hop.indexed_edge_equality.clone(),
                        dst_filter: hop.dst_filter.clone(),
                        hop_aux_binding: hop.hop_aux_binding.clone(),
                    },
                ));
                break;
            }
        }
        let (j, w) = found?;
        used[j] = true;
        remove_indices.push(expands[j].op_idx);
        out_edges.push(w);
    }

    Some((out_edges, remove_indices))
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
                            InsertElement::Node(n) => Some(
                                n.variable
                                    .clone()
                                    .unwrap_or_else(|| format!("__insert_n{}", i + 1)),
                            ),
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
            } => SetPlanItem::Property {
                variable: variable.clone().into(),
                property: property.clone().into(),
                value: value.clone(),
            },
            SetItem::AllProperties {
                span: _,
                variable,
                value,
            } => SetPlanItem::AllProperties {
                variable: variable.clone().into(),
                value: value.clone(),
            },
            SetItem::Label {
                span: _,
                variable,
                label,
                ..
            } => SetPlanItem::Label {
                variable: variable.clone().into(),
                label: label.clone().into(),
            },
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
            } => RemovePlanItem::Property {
                variable: variable.clone().into(),
                property: property.clone().into(),
            },
            RemoveItem::Label {
                span: _,
                variable,
                label,
                ..
            } => RemovePlanItem::Label {
                variable: variable.clone().into(),
                label: label.clone().into(),
            },
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
            BindingKind::Node | BindingKind::Unknown | BindingKind::Path | BindingKind::Value => {
                match delete_stmt.detach {
                    DeleteDetach::Detach => {
                        ops.push(PlanOp::DetachDeleteVertex { variable });
                    }
                    DeleteDetach::NoDetach | DeleteDetach::Unspecified => {
                        ops.push(PlanOp::DeleteVertex { variable });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use gleaph_gql::parser;

    use super::{PlanOp, build_block_plan};

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
        assert!(
            plan.ops.iter().any(
                |op| matches!(op, PlanOp::DeleteEdge { variable } if variable.as_ref() == "e")
            )
        );
    }

    #[test]
    fn keeps_delete_vertex_for_node_binding() {
        let block = parse_block("MATCH (a:User) DELETE a");
        let plan = build_block_plan(&block, None).expect("plan should build");
        assert!(
            plan.ops.iter().any(
                |op| matches!(op, PlanOp::DeleteVertex { variable } if variable.as_ref() == "a")
            )
        );
    }
}
