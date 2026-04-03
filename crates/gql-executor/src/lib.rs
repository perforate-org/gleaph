//! Executes compiled GQL physical plans against the graph kernel traits.
//!
//! **Property projection:** the planner attaches `property_projection` to vertex scan operators when
//! safe; the executor hydrates partial `NodeRecord.properties` and falls back to
//! `GraphRead::get_node_property_value` / `get_edge_property_value` on cache miss.

mod aggregates;
mod exec_utils;
mod graph_ops;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use futures::executor::block_on;
use gleaph_gql::Value;
use gleaph_gql::ast::{CmpOp, Expr, ExprKind, LetBinding, NullOrder, OrderByClause, SortDirection};
use gleaph_gql::types::{LabelExpr, matches_edge_label};
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_planner::plan::{
    ConditionalScanCandidate, IndexScanSpec, ProjectColumn, RemovePlanItem, ScanValue, SetPlanItem,
    ShortestMode, VarLenSpec, WcojEdge,
};
use gleaph_gql_planner::{PhysicalPlan, PlanOp};
use gleaph_graph_kernel::{
    EdgeLabelFilter, EdgeRecord, GraphError, GraphErrorKind, GraphRead, GraphWrite, NodeId,
    NodeRecord, PropertyMap,
};
use thiserror::Error;

use aggregates::{aggregate_expr_binding_name, exec_aggregate};
use exec_utils::{
    apply_limit, apply_limit_bindings, collect_produced_vars, column_name, dedup_binding_rows,
    dedup_output_rows, dedup_output_rows_owned, eval_expr, eval_materialize_expr,
    eval_project_expr, eval_property_assignments, intersect_rows, join_key_values,
    materialize_column_name, materialize_output_rows, materialize_row, merge_rows,
    normalize_to_output_rows, resolve_scan_value, sort_binding_rows, sort_output_rows,
    subtract_rows,
};
use graph_ops::{
    exec_expand, exec_expand_edge_index, exec_expand_filter, exec_expand_filter_var_len,
    exec_expand_var_len, exec_shortest_path, exec_worst_case_optimal_join,
};

/// Fills `out` with cloned leaf names when `expr` is only `Name` and `Or`.
fn try_pure_disjunction_owned(expr: &LabelExpr, out: &mut Vec<String>) -> bool {
    fn go(expr: &LabelExpr, out: &mut Vec<String>) -> bool {
        match expr {
            LabelExpr::Name(n) => {
                out.push(n.clone());
                true
            }
            LabelExpr::Or(a, b) => go(a, out) && go(b, out),
            _ => false,
        }
    }
    out.clear();
    if go(expr, out) {
        true
    } else {
        out.clear();
        false
    }
}

/// Maps plan label fields to [`EdgeLabelFilter`] for [`GraphRead::expand`].
///
/// When the second return value is true, [`edge_satisfies_expand_labels`] must still be applied
/// (general `label_expr`, or inconsistent plan fields).
fn edge_label_filter_for_expand<'a, 'b>(
    label: Option<&'a str>,
    label_expr: Option<&LabelExpr>,
    name_scratch: &'b mut Vec<String>,
) -> (EdgeLabelFilter<'a, 'b>, bool) {
    match (label, label_expr) {
        (None, None) => (EdgeLabelFilter::All, false),
        (Some(s), None) => (EdgeLabelFilter::Single(s), false),
        (None, Some(expr)) => {
            if try_pure_disjunction_owned(expr, name_scratch) {
                (EdgeLabelFilter::AnyOf(name_scratch.as_slice()), false)
            } else {
                (EdgeLabelFilter::All, true)
            }
        }
        (Some(_), Some(_)) => (EdgeLabelFilter::All, true),
    }
}

fn edge_satisfies_expand_labels(
    edge_label: Option<&str>,
    label: Option<&str>,
    label_expr: Option<&LabelExpr>,
) -> bool {
    if let Some(le) = label_expr {
        matches_edge_label(le, edge_label)
    } else {
        label.is_none_or(|l| edge_label == Some(l))
    }
}

fn wcoj_edge_satisfies_labels(spec: &WcojEdge, edge_label: Option<&str>) -> bool {
    edge_satisfies_expand_labels(edge_label, spec.label.as_deref(), spec.label_expr.as_ref())
}

#[derive(Clone, Debug, PartialEq)]
pub enum BindingValue {
    Scalar(Value),
    Node(NodeRecord),
    Edge(EdgeRecord),
}

pub type BindingRow = BTreeMap<Rc<str>, BindingValue>;
pub type OutputRow = BTreeMap<String, Value>;
type ShortestPathPredecessors = HashMap<NodeId, Vec<(NodeId, EdgeRecord)>>;
type ShortestPathBfsState = (HashMap<NodeId, u32>, ShortestPathPredecessors);

#[derive(Clone, Debug)]
pub struct ProcedureInvocation {
    pub name: Vec<String>,
    pub args: Vec<Value>,
    pub selected_graph: Option<String>,
    pub caller: Option<Value>,
}

pub trait ProcedureRegistry: Send + Sync {
    fn call(
        &self,
        graph: &dyn GraphRead,
        invocation: &ProcedureInvocation,
    ) -> ExecutionResultExt<Vec<OutputRow>>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphResolution {
    pub graph_name: String,
    pub canister_id: Option<String>,
}

pub trait GraphRegistryResolver: Send + Sync {
    fn resolve(
        &self,
        requested_graph: &str,
        caller: Option<&Value>,
    ) -> ExecutionResultExt<GraphResolution>;
}

#[async_trait::async_trait(?Send)]
pub trait UseGraphRouter: Send + Sync {
    async fn execute_remote_subplan(
        &self,
        target: &GraphResolution,
        sub_plan: &[PlanOp],
        ctx: &ExecutionContext,
        input_rows: Vec<BindingRow>,
    ) -> ExecutionResultExt<(Vec<BindingRow>, Option<Vec<OutputRow>>)>;
}

struct InsertEdgeSpec<'a> {
    variable: Option<&'a str>,
    src: &'a str,
    dst: &'a str,
    labels: &'a [Rc<str>],
    properties: &'a [gleaph_gql_planner::plan::PropertyAssignment],
}

#[derive(Clone)]
struct ExpandSpec<'a> {
    src: &'a str,
    edge: &'a str,
    dst: &'a str,
    direction: gleaph_gql::types::EdgeDirection,
    label: Option<&'a str>,
    label_expr: Option<&'a LabelExpr>,
    edge_property_names: Option<Vec<String>>,
    dst_property_names: Option<Vec<String>>,
}

fn projection_name_vec(names: Option<&[Rc<str>]>) -> Option<Vec<String>> {
    names.map(|s| s.iter().map(|x| x.as_ref().to_owned()).collect())
}

struct ExpandIndexSpec<'a> {
    expand: ExpandSpec<'a>,
    property: &'a str,
    value: &'a ScanValue,
}

struct ShortestBfsSpec<'a, 'b> {
    start: NodeId,
    direction: gleaph_gql::types::EdgeDirection,
    filter: EdgeLabelFilter<'a, 'b>,
    post_filter: bool,
    label: Option<&'a str>,
    label_expr: Option<&'a LabelExpr>,
    max_depth: u32,
}

struct ShortestPathSpec<'a> {
    src: &'a str,
    dst: &'a str,
    edge_var: &'a str,
    path_var: Option<&'a str>,
    mode: ShortestMode,
    direction: gleaph_gql::types::EdgeDirection,
    label: Option<&'a str>,
    label_expr: Option<&'a LabelExpr>,
    var_len: Option<&'a VarLenSpec>,
}

struct WcojDfsState<'a> {
    nodes: &'a mut HashMap<Rc<str>, NodeId>,
    edgs: &'a mut HashMap<Rc<str>, Option<EdgeRecord>>,
    out: &'a mut Vec<BindingRow>,
    budget: &'a mut usize,
}

struct WcojDfsSpec<'a> {
    base_row: &'a BindingRow,
    vars: &'a [Rc<str>],
    edges: &'a [WcojEdge],
    n: usize,
    ctx: &'a ExecutionContext,
}

#[derive(Clone, Default)]
pub struct ExecutionContext {
    pub params: BTreeMap<String, Value>,
    pub caller: Option<Value>,
    /// Optional currently selected graph name.
    pub selected_graph: Option<String>,
    /// Optional default graph name (used by CURRENT_GRAPH aliases).
    pub default_graph: Option<String>,
    /// Optional home graph name (used by HOME_GRAPH aliases).
    pub home_graph: Option<String>,
    /// Optional allow-list for explicit graph names in USE.
    pub available_graphs: BTreeSet<String>,
    /// Optional external procedure registry.
    pub procedure_registry: Option<Arc<dyn ProcedureRegistry>>,
    /// Optional graph registry resolver (`graph_name` -> execution target).
    pub graph_registry_resolver: Option<Arc<dyn GraphRegistryResolver>>,
    /// Optional router for remote `USE GRAPH` delegation.
    pub use_graph_router: Option<Arc<dyn UseGraphRouter>>,
}

impl std::fmt::Debug for ExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionContext")
            .field("params", &self.params)
            .field("caller", &self.caller)
            .field("selected_graph", &self.selected_graph)
            .field("default_graph", &self.default_graph)
            .field("home_graph", &self.home_graph)
            .field("available_graphs", &self.available_graphs)
            .field(
                "procedure_registry",
                &self.procedure_registry.as_ref().map(|_| "<custom>"),
            )
            .field(
                "graph_registry_resolver",
                &self.graph_registry_resolver.as_ref().map(|_| "<custom>"),
            )
            .field(
                "use_graph_router",
                &self.use_graph_router.as_ref().map(|_| "<custom>"),
            )
            .finish()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecutionResult {
    pub rows: Vec<OutputRow>,
    pub warnings: Vec<String>,
    pub summary: ExecutionSummary,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecutionSummary {
    pub row_count: usize,
    pub warning_count: usize,
    pub had_dml: bool,
}

impl ExecutionSummary {
    fn from_result(rows: &[OutputRow], warnings: &[String], plan: &PhysicalPlan) -> Self {
        Self {
            row_count: rows.len(),
            warning_count: warnings.len(),
            had_dml: plan.has_dml(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("invalid plan: {0}")]
    InvalidPlan(String),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("unsupported plan op: {0}")]
    UnsupportedPlanOp(&'static str),
    #[error("unsupported expression: {0}")]
    UnsupportedExpr(&'static str),
    #[error("missing binding for variable `{0}`")]
    MissingBinding(String),
    #[error("type mismatch: {0}")]
    TypeMismatch(&'static str),
    #[error("invalid limit/offset expression")]
    InvalidLimit,
}

impl ExecutionError {
    /// Returns the wrapped [`GraphError`] when this error is [`ExecutionError::Graph`].
    pub fn as_graph_error(&self) -> Option<&GraphError> {
        match self {
            ExecutionError::Graph(e) => Some(e),
            _ => None,
        }
    }

    /// [`GraphError::kind`] when this error is [`ExecutionError::Graph`].
    pub fn graph_error_kind(&self) -> Option<GraphErrorKind> {
        self.as_graph_error().map(GraphError::kind)
    }
}

pub type ExecutionResultExt<T> = Result<T, ExecutionError>;

fn use_graph_pushdown_warning(info: &gleaph_gql_planner::UseGraphPushdownInfo) -> Option<String> {
    if info.supported {
        return None;
    }
    let reason = info
        .reason
        .as_deref()
        .unwrap_or("unsupported remote USE GRAPH shape");
    Some(format!(
        "remote USE GRAPH pushdown unavailable for {}: {}",
        info.graph_name, reason
    ))
}

fn lookup_param(ctx: &ExecutionContext, name: &str) -> Value {
    if let Some(value) = ctx.params.get(name) {
        return value.clone();
    }
    if let Some(stripped) = name.strip_prefix('$')
        && let Some(value) = ctx.params.get(stripped)
    {
        return value.clone();
    }
    if !name.starts_with('$') {
        let prefixed = format!("${name}");
        if let Some(value) = ctx.params.get(&prefixed) {
            return value.clone();
        }
    }
    Value::Null
}

fn active_graph_from_context(ctx: &ExecutionContext) -> Option<GraphResolution> {
    ctx.selected_graph.as_ref().map(|name| GraphResolution {
        graph_name: name.clone(),
        canister_id: None,
    })
}

fn resolve_use_graph_target(
    graph_name: &[Rc<str>],
    ctx: &ExecutionContext,
    active_graph: Option<&GraphResolution>,
) -> ExecutionResultExt<GraphResolution> {
    let requested = graph_name
        .iter()
        .map(|part| part.as_ref())
        .collect::<Vec<_>>()
        .join(".");
    let upper = requested.to_ascii_uppercase();
    let resolved_name = match upper.as_str() {
        "CURRENT_GRAPH" | "CURRENT_PROPERTY_GRAPH" => active_graph
            .map(|g| g.graph_name.clone())
            .or_else(|| ctx.selected_graph.clone())
            .or_else(|| ctx.default_graph.clone())
            .or_else(|| ctx.home_graph.clone()),
        "HOME_GRAPH" | "HOME_PROPERTY_GRAPH" => {
            ctx.home_graph.clone().or_else(|| ctx.default_graph.clone())
        }
        _ => Some(requested),
    }
    .ok_or_else(|| ExecutionError::InvalidPlan("unable to resolve USE graph target".to_owned()))?;

    if !ctx.available_graphs.is_empty() && !ctx.available_graphs.contains(&resolved_name) {
        return Err(ExecutionError::InvalidPlan(format!(
            "unknown graph in USE: {resolved_name}"
        )));
    }

    if let Some(resolver) = &ctx.graph_registry_resolver {
        return resolver.resolve(&resolved_name, ctx.caller.as_ref());
    }

    Ok(GraphResolution {
        graph_name: resolved_name,
        canister_id: None,
    })
}

pub fn execute_plan<G: GraphRead + GraphWrite>(
    graph: &mut G,
    plan: &PhysicalPlan,
) -> ExecutionResultExt<ExecutionResult> {
    execute_plan_with_context(graph, plan, &ExecutionContext::default())
}

pub fn execute_plan_with_context<G: GraphRead + GraphWrite>(
    graph: &mut G,
    plan: &PhysicalPlan,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<ExecutionResult> {
    execute_plan_with_context_maybe_flush(graph, plan, ctx, true)
}

/// Like [`execute_plan_with_context`], but `flush_at_end` controls the terminal [`GraphWrite::flush`].
///
/// Nested plans (e.g. [`PlanOp::SetOperation`] RHS) pass `false` so the outer plan performs a single
/// stable flush, avoiding duplicate PMA refresh / PIDX work.
fn execute_plan_with_context_maybe_flush<G: GraphRead + GraphWrite>(
    graph: &mut G,
    plan: &PhysicalPlan,
    ctx: &ExecutionContext,
    flush_at_end: bool,
) -> ExecutionResultExt<ExecutionResult> {
    if let Some(error) = plan.diagnostics.dml_errors.first() {
        return Err(ExecutionError::InvalidPlan(format!(
            "[{}] at {}..{}: {}",
            error.code, error.span.start, error.span.end, error.message
        )));
    }

    if let Some(op_name) = gleaph_gql_planner::first_executor_unsupported_op(plan) {
        return Err(ExecutionError::InvalidPlan(format!(
            "plan contains operator not supported by executor: {op_name}"
        )));
    }

    let (rows, projected) = execute_ops(graph, &plan.ops, ctx)?;

    let rows = match projected {
        Some(rows) => rows,
        None => rows
            .into_iter()
            .map(|row| materialize_row(graph, &row))
            .collect::<Result<_, _>>()?,
    };
    let warnings: Vec<String> = plan
        .diagnostics
        .dml_warnings
        .iter()
        .map(|warning| {
            format!(
                "[{}] at {}..{}: {}",
                warning.code, warning.span.start, warning.span.end, warning.message
            )
        })
        .chain(plan.diagnostics.type_warnings.iter().map(|warning| {
            format!(
                "[{}] at {}..{}: {}",
                warning.code.unwrap_or("TYPE"),
                warning.span.start,
                warning.span.end,
                warning.message
            )
        }))
        .chain(
            plan.use_graph_pushdown()
                .iter()
                .filter_map(use_graph_pushdown_warning),
        )
        .collect();
    let summary = ExecutionSummary::from_result(&rows, &warnings, plan);

    if flush_at_end {
        #[cfg(feature = "canbench-rs")]
        let _flush_scope = canbench_rs::bench_scope("gql_exec_plan_flush");
        graph.flush()?;
    }

    Ok(ExecutionResult {
        rows,
        warnings,
        summary,
    })
}

fn execute_ops<G: GraphRead + GraphWrite>(
    graph: &mut G,
    ops: &[PlanOp],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<(Vec<BindingRow>, Option<Vec<OutputRow>>)> {
    execute_ops_from_rows(
        graph,
        ops,
        ctx,
        vec![BindingRow::new()],
        active_graph_from_context(ctx),
    )
}

fn execute_ops_from_rows<G: GraphRead + GraphWrite>(
    graph: &mut G,
    ops: &[PlanOp],
    ctx: &ExecutionContext,
    initial_rows: Vec<BindingRow>,
    active_graph: Option<GraphResolution>,
) -> ExecutionResultExt<(Vec<BindingRow>, Option<Vec<OutputRow>>)> {
    let mut rows = initial_rows;
    let mut projected = None;
    let mut active_graph = active_graph;

    for op in ops {
        match op {
            PlanOp::NodeScan {
                variable,
                label,
                property_projection,
            } => {
                rows = exec_node_scan(
                    graph,
                    &rows,
                    variable.as_ref(),
                    label.as_deref(),
                    property_projection.as_deref(),
                )?;
            }
            PlanOp::IndexScan {
                variable,
                property,
                value,
                cmp,
                property_projection,
            } => {
                rows = exec_index_scan(
                    graph,
                    &rows,
                    variable.as_ref(),
                    property.as_ref(),
                    value,
                    *cmp,
                    property_projection.as_deref(),
                    ctx,
                )?;
            }
            PlanOp::IndexIntersection {
                variable,
                scans,
                property_projection,
            } => {
                rows = exec_index_intersection(
                    graph,
                    &rows,
                    variable.as_ref(),
                    scans.as_slice(),
                    property_projection.as_deref(),
                    ctx,
                )?;
            }
            PlanOp::EdgeIndexScan {
                variable,
                property,
                value,
                property_projection,
            } => {
                rows = exec_edge_index_scan(
                    graph,
                    &rows,
                    variable.as_ref(),
                    property.as_ref(),
                    value,
                    property_projection.as_deref(),
                    ctx,
                )?;
            }
            PlanOp::EdgeBindEndpoints {
                edge,
                near,
                far,
                direction,
                label,
                near_property_projection,
                far_property_projection,
            } => {
                rows = exec_edge_bind_endpoints(
                    graph,
                    rows,
                    edge.as_ref(),
                    near.as_ref(),
                    far.as_ref(),
                    *direction,
                    label.as_deref(),
                    near_property_projection.as_deref(),
                    far_property_projection.as_deref(),
                )?;
            }
            PlanOp::ConditionalIndexScan {
                candidates,
                fallback_label,
                fallback_variable,
                property_projection,
            } => {
                rows = exec_conditional_index_scan(
                    graph,
                    &rows,
                    candidates,
                    fallback_label.as_deref(),
                    fallback_variable.as_ref(),
                    property_projection.as_deref(),
                    ctx,
                )?;
            }
            PlanOp::PropertyFilter { predicates, .. } => {
                rows = exec_property_filter(graph, rows, predicates, ctx)?;
            }
            PlanOp::Let { bindings } => {
                rows = exec_let(graph, rows, bindings, ctx)?;
                projected = None;
            }
            PlanOp::For {
                variable,
                list,
                ordinality,
            } => {
                rows = exec_for(
                    graph,
                    rows,
                    variable.as_ref(),
                    list,
                    ordinality.as_deref(),
                    ctx,
                )?;
                projected = None;
            }
            PlanOp::Filter { condition } => {
                rows = exec_filter(graph, rows, condition, ctx)?;
                projected = None;
            }
            PlanOp::CallProcedure {
                name,
                args,
                yield_columns,
                optional,
            } => {
                rows = exec_call_procedure(
                    graph,
                    rows,
                    name,
                    args,
                    yield_columns.as_ref(),
                    *optional,
                    ctx,
                )?;
                projected = None;
            }
            PlanOp::InlineProcedureCall {
                sub_plan,
                scope_vars,
                optional,
            } => {
                rows =
                    exec_inline_procedure_call(graph, rows, sub_plan, scope_vars, *optional, ctx)?;
                projected = None;
            }
            PlanOp::UseGraph {
                graph_name,
                sub_plan,
            } => {
                let resolved = resolve_use_graph_target(graph_name, ctx, active_graph.as_ref())?;
                if let Some(sub_plan) = sub_plan {
                    let mut sub_ctx = ctx.clone();
                    sub_ctx.selected_graph = Some(resolved.graph_name.clone());
                    let (sub_rows, sub_projected) = if resolved.canister_id.is_some() {
                        let pushdown = gleaph_gql_planner::analyze_remote_use_graph_pushdown(
                            &resolved.graph_name,
                            sub_plan,
                        );
                        if !pushdown.supported {
                            let reason = pushdown
                                .reason
                                .unwrap_or_else(|| "unsupported remote USE GRAPH shape".to_owned());
                            return Err(ExecutionError::InvalidPlan(format!(
                                "remote USE GRAPH pushdown unavailable for {}: {}",
                                resolved.graph_name, reason
                            )));
                        }
                        let router = sub_ctx.use_graph_router.as_ref().ok_or_else(|| {
                            ExecutionError::InvalidPlan(format!(
                                "remote USE graph requires router: {}",
                                resolved.graph_name
                            ))
                        })?;
                        block_on(
                            router.execute_remote_subplan(&resolved, sub_plan, &sub_ctx, rows),
                        )?
                    } else {
                        execute_ops_from_rows(
                            graph,
                            sub_plan,
                            &sub_ctx,
                            rows,
                            Some(resolved.clone()),
                        )?
                    };
                    rows = sub_rows;
                    projected = sub_projected;
                } else {
                    active_graph = Some(resolved);
                }
            }
            PlanOp::Expand {
                src,
                edge,
                dst,
                direction,
                label,
                label_expr,
                var_len,
                indexed_edge_equality,
                edge_property_projection,
                dst_property_projection,
            } => {
                let expand_spec = ExpandSpec {
                    src: src.as_ref(),
                    edge: edge.as_ref(),
                    dst: dst.as_ref(),
                    direction: *direction,
                    label: label.as_deref(),
                    label_expr: label_expr.as_ref(),
                    edge_property_names: projection_name_vec(edge_property_projection.as_deref()),
                    dst_property_names: projection_name_vec(dst_property_projection.as_deref()),
                };
                if let Some(var_len) = var_len.as_ref() {
                    rows = exec_expand_var_len(
                        graph,
                        rows,
                        expand_spec,
                        var_len,
                        indexed_edge_equality
                            .as_ref()
                            .map(|(prop, value)| (prop.as_ref(), value)),
                        ctx,
                    )?;
                } else {
                    rows = if let Some((prop, value)) = indexed_edge_equality.as_ref() {
                        exec_expand_edge_index(
                            graph,
                            rows,
                            ExpandIndexSpec {
                                expand: expand_spec,
                                property: prop.as_ref(),
                                value,
                            },
                            ctx,
                        )?
                    } else {
                        exec_expand(graph, rows, expand_spec)?
                    };
                }
            }
            PlanOp::ExpandFilter {
                src,
                edge,
                dst,
                direction,
                label,
                label_expr,
                var_len,
                indexed_edge_equality,
                dst_filter,
                edge_property_projection,
                dst_property_projection,
            } => {
                let expand_spec = ExpandSpec {
                    src: src.as_ref(),
                    edge: edge.as_ref(),
                    dst: dst.as_ref(),
                    direction: *direction,
                    label: label.as_deref(),
                    label_expr: label_expr.as_ref(),
                    edge_property_names: projection_name_vec(edge_property_projection.as_deref()),
                    dst_property_names: projection_name_vec(dst_property_projection.as_deref()),
                };
                if let Some(var_len) = var_len.as_ref() {
                    rows = exec_expand_filter_var_len(
                        graph,
                        rows,
                        expand_spec,
                        var_len,
                        dst_filter,
                        indexed_edge_equality
                            .as_ref()
                            .map(|(prop, value)| (prop.as_ref(), value)),
                        ctx,
                    )?;
                } else {
                    rows = if let Some((prop, value)) = indexed_edge_equality.as_ref() {
                        let expanded = exec_expand_edge_index(
                            graph,
                            rows,
                            ExpandIndexSpec {
                                expand: expand_spec,
                                property: prop.as_ref(),
                                value,
                            },
                            ctx,
                        )?;
                        exec_property_filter(graph, expanded, dst_filter, ctx)?
                    } else {
                        exec_expand_filter(graph, rows, expand_spec, dst_filter, ctx)?
                    };
                }
            }
            PlanOp::ShortestPath {
                src,
                dst,
                edge,
                path_var,
                mode,
                direction,
                label,
                label_expr,
                var_len,
            } => {
                rows = exec_shortest_path(
                    graph,
                    rows,
                    ShortestPathSpec {
                        src: src.as_ref(),
                        dst: dst.as_ref(),
                        edge_var: edge.as_ref(),
                        path_var: path_var.as_deref(),
                        mode: *mode,
                        direction: *direction,
                        label: label.as_deref(),
                        label_expr: label_expr.as_ref(),
                        var_len: var_len.as_ref(),
                    },
                )?;
            }
            PlanOp::WorstCaseOptimalJoin { variables, edges } => {
                rows = exec_worst_case_optimal_join(
                    graph,
                    rows,
                    variables.as_slice(),
                    edges.as_slice(),
                    ctx,
                )?;
            }
            PlanOp::Aggregate {
                group_by,
                aggregates,
            } => {
                rows = exec_aggregate(graph, rows, group_by, aggregates, ctx)?;
            }
            PlanOp::HashJoin {
                left,
                right,
                join_keys,
            } => {
                rows = exec_hash_join(graph, left, right, join_keys, ctx)?;
                projected = None;
            }
            PlanOp::CartesianProduct { left, right } => {
                rows = exec_cartesian_product(graph, left, right, ctx)?;
                projected = None;
            }
            PlanOp::OptionalMatch { sub_plan } => {
                rows = exec_optional_match(graph, rows, sub_plan, ctx)?;
                projected = None;
            }
            PlanOp::InsertVertex {
                variable,
                labels,
                properties,
            } => {
                rows =
                    exec_insert_vertex(graph, rows, variable.as_deref(), labels, properties, ctx)?;
                projected = None;
            }
            PlanOp::InsertEdge {
                variable,
                src,
                dst,
                labels,
                properties,
                ..
            } => {
                rows = exec_insert_edge(
                    graph,
                    rows,
                    InsertEdgeSpec {
                        variable: variable.as_deref(),
                        src: src.as_ref(),
                        dst: dst.as_ref(),
                        labels,
                        properties,
                    },
                    ctx,
                )?;
                projected = None;
            }
            PlanOp::SetProperties { items } => {
                rows = exec_set_properties(graph, rows, items, ctx)?;
                projected = None;
            }
            PlanOp::RemoveProperties { items } => {
                rows = exec_remove_properties(graph, rows, items)?;
                projected = None;
            }
            PlanOp::DeleteVertex { variable } => {
                rows = exec_delete_vertex(graph, rows, variable.as_ref(), false)?;
                projected = None;
            }
            PlanOp::DetachDeleteVertex { variable } => {
                rows = exec_delete_vertex(graph, rows, variable.as_ref(), true)?;
                projected = None;
            }
            PlanOp::DeleteEdge { variable } => {
                rows = exec_delete_edge(graph, rows, variable.as_ref())?;
                projected = None;
            }
            PlanOp::Project { columns, .. } => {
                let mut output = exec_project(graph, &rows, columns, ctx)?;
                if matches!(op, PlanOp::Project { distinct: true, .. }) {
                    dedup_output_rows(&mut output);
                }
                projected = Some(output);
            }
            PlanOp::SetOperation { op, right } => {
                let left_rows = normalize_to_output_rows(graph, rows, projected.take())?;
                let right_result = execute_plan_with_context_maybe_flush(graph, right, ctx, false)?;
                let output = exec_set_operation(*op, left_rows, right_result.rows);
                rows = Vec::new();
                projected = Some(output);
            }
            PlanOp::Sort { order_by } => {
                if let Some(output) = projected.as_mut() {
                    sort_output_rows(output, order_by)?;
                } else {
                    sort_binding_rows(graph, &mut rows, order_by, ctx)?;
                }
            }
            PlanOp::Limit { count, offset } => {
                if let Some(output) = projected.as_mut() {
                    apply_limit(output, count.as_ref(), offset.as_ref())?;
                } else {
                    rows = apply_limit_bindings(rows, count.as_ref(), offset.as_ref())?;
                }
            }
            PlanOp::TopK {
                order_by,
                k,
                offset,
            } => {
                if let Some(output) = projected.as_mut() {
                    sort_output_rows(output, order_by)?;
                    apply_limit(output, Some(k), offset.as_ref())?;
                } else {
                    sort_binding_rows(graph, &mut rows, order_by, ctx)?;
                    rows = apply_limit_bindings(rows, Some(k), offset.as_ref())?;
                }
            }
            PlanOp::Materialize { columns, distinct } => {
                rows = exec_materialize(graph, rows, projected.take(), columns, *distinct, ctx)?;
                projected = None;
            }
        }
    }

    Ok((rows, projected))
}

fn exec_node_scan<G: GraphRead>(
    graph: &G,
    input: &[BindingRow],
    variable: &str,
    label: Option<&str>,
    property_projection: Option<&[Rc<str>]>,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let nodes = match property_projection {
        None => graph.scan_nodes(label)?,
        Some(names) => {
            let v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            graph.scan_nodes_projected(label, &v)?
        }
    };
    let mut next = Vec::new();
    for row in input {
        for node in &nodes {
            let mut out = row.clone();
            out.insert(Rc::<str>::from(variable), BindingValue::Node(node.clone()));
            next.push(out);
        }
    }
    Ok(next)
}

fn exec_insert_vertex<G: GraphRead + GraphWrite>(
    graph: &mut G,
    input: Vec<BindingRow>,
    variable: Option<&str>,
    labels: &[Rc<str>],
    properties: &[gleaph_gql_planner::plan::PropertyAssignment],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::with_capacity(input.len());
    for row in input {
        let property_map = eval_property_assignments(graph, &row, properties, ctx)?;
        let labels: Vec<String> = labels.iter().map(|label| label.to_string()).collect();
        let node = graph.insert_node(&labels, &property_map)?;
        let mut next = row;
        if let Some(variable) = variable {
            next.insert(Rc::<str>::from(variable), BindingValue::Node(node));
        }
        out.push(next);
    }
    Ok(out)
}

fn exec_insert_edge<G: GraphRead + GraphWrite>(
    graph: &mut G,
    input: Vec<BindingRow>,
    spec: InsertEdgeSpec<'_>,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::with_capacity(input.len());
    for row in input {
        let src_id = match row.get(spec.src) {
            Some(BindingValue::Node(node)) => node.id,
            Some(_) => {
                return Err(ExecutionError::TypeMismatch(
                    "insert edge src must be a node",
                ));
            }
            None => return Err(ExecutionError::MissingBinding(spec.src.to_owned())),
        };
        let dst_id = match row.get(spec.dst) {
            Some(BindingValue::Node(node)) => node.id,
            Some(_) => {
                return Err(ExecutionError::TypeMismatch(
                    "insert edge dst must be a node",
                ));
            }
            None => return Err(ExecutionError::MissingBinding(spec.dst.to_owned())),
        };
        let property_map = eval_property_assignments(graph, &row, spec.properties, ctx)?;
        let edge_label = spec.labels.first().map(|label| label.as_ref());
        let edge = graph.insert_edge(src_id, dst_id, edge_label, &property_map)?;
        let mut next = row;
        if let Some(variable) = spec.variable {
            next.insert(Rc::<str>::from(variable), BindingValue::Edge(edge));
        }
        out.push(next);
    }
    Ok(out)
}

fn exec_set_properties<G: GraphRead + GraphWrite>(
    graph: &mut G,
    input: Vec<BindingRow>,
    items: &[SetPlanItem],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::with_capacity(input.len());
    for mut row in input {
        for item in items {
            match item {
                SetPlanItem::Property {
                    variable,
                    property,
                    value,
                } => {
                    #[cfg(feature = "canbench-rs")]
                    let _set_item_scope = canbench_rs::bench_scope("gql_exec_set_property_item");
                    let evaluated = eval_expr(graph, &row, value, ctx)?;
                    match row.get(variable.as_ref()) {
                        Some(BindingValue::Node(node)) => {
                            let updated = graph.set_node_property(node.id, property, &evaluated)?;
                            row.insert(variable.clone(), BindingValue::Node(updated));
                        }
                        Some(BindingValue::Edge(edge)) => {
                            let updated = graph.set_edge_property(edge.id, property, &evaluated)?;
                            row.insert(variable.clone(), BindingValue::Edge(updated));
                        }
                        Some(_) => {
                            return Err(ExecutionError::TypeMismatch(
                                "set property target must be node or edge",
                            ));
                        }
                        None => return Err(ExecutionError::MissingBinding(variable.to_string())),
                    }
                }
                SetPlanItem::Label { variable, label } => match row.get(variable.as_ref()) {
                    Some(BindingValue::Node(node)) => {
                        let updated = graph.add_node_label(node.id, label)?;
                        row.insert(variable.clone(), BindingValue::Node(updated));
                    }
                    Some(BindingValue::Edge(edge)) => {
                        let updated = graph.set_edge_label(edge.id, Some(label))?;
                        row.insert(variable.clone(), BindingValue::Edge(updated));
                    }
                    Some(_) => {
                        return Err(ExecutionError::TypeMismatch(
                            "set label target must be node or edge",
                        ));
                    }
                    None => return Err(ExecutionError::MissingBinding(variable.to_string())),
                },
                SetPlanItem::AllProperties { .. } => {
                    return Err(ExecutionError::UnsupportedPlanOp(
                        "SetProperties.AllProperties",
                    ));
                }
            }
        }
        out.push(row);
    }
    Ok(out)
}

fn exec_remove_properties<G: GraphRead + GraphWrite>(
    graph: &mut G,
    input: Vec<BindingRow>,
    items: &[RemovePlanItem],
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::with_capacity(input.len());
    for mut row in input {
        for item in items {
            match item {
                RemovePlanItem::Property { variable, property } => match row.get(variable.as_ref())
                {
                    Some(BindingValue::Node(node)) => {
                        let updated = graph.remove_node_property(node.id, property)?;
                        row.insert(variable.clone(), BindingValue::Node(updated));
                    }
                    Some(BindingValue::Edge(edge)) => {
                        let updated = graph.remove_edge_property(edge.id, property)?;
                        row.insert(variable.clone(), BindingValue::Edge(updated));
                    }
                    Some(_) => {
                        return Err(ExecutionError::TypeMismatch(
                            "remove property target must be node or edge",
                        ));
                    }
                    None => return Err(ExecutionError::MissingBinding(variable.to_string())),
                },
                RemovePlanItem::Label { variable, label } => match row.get(variable.as_ref()) {
                    Some(BindingValue::Node(node)) => {
                        let updated = graph.remove_node_label(node.id, label)?;
                        row.insert(variable.clone(), BindingValue::Node(updated));
                    }
                    Some(BindingValue::Edge(edge)) => {
                        let updated = graph.set_edge_label(edge.id, None)?;
                        row.insert(variable.clone(), BindingValue::Edge(updated));
                    }
                    Some(_) => {
                        return Err(ExecutionError::TypeMismatch(
                            "remove label target must be node or edge",
                        ));
                    }
                    None => return Err(ExecutionError::MissingBinding(variable.to_string())),
                },
            }
        }
        out.push(row);
    }
    Ok(out)
}

fn exec_delete_vertex<G: GraphRead + GraphWrite>(
    graph: &mut G,
    input: Vec<BindingRow>,
    variable: &str,
    detach: bool,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::with_capacity(input.len());
    for mut row in input {
        let node_id = match row.get(variable) {
            Some(BindingValue::Node(node)) => node.id,
            Some(_) => return Err(ExecutionError::TypeMismatch("delete target must be node")),
            None => return Err(ExecutionError::MissingBinding(variable.to_owned())),
        };
        graph.delete_node(node_id, detach)?;
        row.remove(variable);
        out.push(row);
    }
    Ok(out)
}

fn exec_delete_edge<G: GraphRead + GraphWrite>(
    graph: &mut G,
    input: Vec<BindingRow>,
    variable: &str,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::with_capacity(input.len());
    for mut row in input {
        let edge_id = match row.get(variable) {
            Some(BindingValue::Edge(edge)) => edge.id,
            Some(_) => return Err(ExecutionError::TypeMismatch("delete target must be edge")),
            None => return Err(ExecutionError::MissingBinding(variable.to_owned())),
        };
        graph.delete_edge(edge_id)?;
        row.remove(variable);
        out.push(row);
    }
    Ok(out)
}

fn exec_property_filter<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    predicates: &[Expr],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::new();
    'rows: for row in input {
        for predicate in predicates {
            match eval_expr(graph, &row, predicate, ctx)? {
                Value::Bool(true) => {}
                Value::Bool(false) | Value::Null => continue 'rows,
                _ => {
                    return Err(ExecutionError::TypeMismatch(
                        "filter predicate must be boolean",
                    ));
                }
            }
        }
        out.push(row);
    }
    Ok(out)
}

fn value_to_binding_value(value: Value) -> ExecutionResultExt<BindingValue> {
    // Convert the executor's internal record representation back into graph
    // bindings when it looks like a node/edge value.
    match value {
        Value::Record(fields) => {
            // Build small lookup helpers for the few keys we care about.
            let get = |key: &str| -> Option<Value> {
                fields
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
            };

            fn parse_u64(v: Value) -> Option<u64> {
                match v {
                    Value::Uint64(u) => Some(u),
                    Value::Int64(i) if i >= 0 => Some(i as u64),
                    Value::Int32(i) if i >= 0 => Some(i as u64),
                    Value::Uint32(u) => Some(u as u64),
                    _ => None,
                }
            }

            let id = get("id").and_then(parse_u64);

            // Node shape: { id, labels, ...properties }
            let labels = get("labels").and_then(|v| match v {
                Value::List(items) => Some(
                    items
                        .into_iter()
                        .filter_map(|x| match x {
                            Value::Text(s) => Some(s),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            });

            let src = get("src").and_then(parse_u64);
            let dst = get("dst").and_then(parse_u64);

            match (id, labels, src, dst) {
                (Some(id), Some(labels), None, None) => {
                    let id = NodeId::try_from(id)
                        .map_err(|_| ExecutionError::TypeMismatch("node id overflow"))?;
                    let mut properties: PropertyMap = PropertyMap::new();
                    for (k, v) in fields {
                        if k != "id" && k != "labels" {
                            properties.insert(k, v);
                        }
                    }
                    Ok(BindingValue::Node(NodeRecord {
                        id,
                        labels,
                        properties,
                    }))
                }
                (Some(edge_id), _, Some(src), Some(dst)) => {
                    let src = NodeId::try_from(src)
                        .map_err(|_| ExecutionError::TypeMismatch("edge src id overflow"))?;
                    let dst = NodeId::try_from(dst)
                        .map_err(|_| ExecutionError::TypeMismatch("edge dst id overflow"))?;
                    let label = get("label").and_then(|v| match v {
                        Value::Text(s) => Some(s),
                        _ => None,
                    });

                    let mut properties: PropertyMap = PropertyMap::new();
                    for (k, v) in fields {
                        if k != "id" && k != "src" && k != "dst" && k != "label" {
                            properties.insert(k, v);
                        }
                    }
                    Ok(BindingValue::Edge(EdgeRecord {
                        id: edge_id,
                        src,
                        dst,
                        label,
                        properties,
                    }))
                }
                _ => Ok(BindingValue::Scalar(Value::Record(fields))),
            }
        }
        other => Ok(BindingValue::Scalar(other)),
    }
}

fn exec_let<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    bindings: &[LetBinding],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::with_capacity(input.len());
    for mut row in input {
        for binding in bindings {
            let value = eval_expr(graph, &row, &binding.value, ctx)?;
            let binding_value = value_to_binding_value(value)?;
            row.insert(Rc::<str>::from(binding.variable.as_str()), binding_value);
        }
        out.push(row);
    }
    Ok(out)
}

fn exec_for<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    variable: &str,
    list: &Expr,
    ordinality: Option<&str>,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::new();
    for row in input {
        let list_value = eval_expr(graph, &row, list, ctx)?;
        let items = match list_value {
            Value::List(items) => items,
            Value::Null => Vec::new(),
            _ => return Err(ExecutionError::TypeMismatch("FOR expects list expression")),
        };

        for (i0, item) in items.into_iter().enumerate() {
            let mut next = row.clone();
            let binding_value = value_to_binding_value(item)?;
            next.insert(Rc::<str>::from(variable), binding_value);

            if let Some(ord) = ordinality {
                // SQL-standard: ORDINALITY is 1-based.
                next.insert(
                    Rc::<str>::from(ord),
                    BindingValue::Scalar(Value::Int64((i0 + 1) as i64)),
                );
            }
            out.push(next);
        }
    }
    Ok(out)
}

fn exec_filter<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    condition: &Expr,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::new();
    for row in input {
        let keep = match eval_expr(graph, &row, condition, ctx)? {
            Value::Bool(true) => true,
            Value::Bool(false) | Value::Null => false,
            _ => {
                return Err(ExecutionError::TypeMismatch(
                    "FILTER condition must be boolean",
                ));
            }
        };
        if keep {
            out.push(row);
        }
    }
    Ok(out)
}

fn exec_index_scan<G: GraphRead>(
    graph: &G,
    input: &[BindingRow],
    variable: &str,
    property: &str,
    value: &ScanValue,
    cmp: CmpOp,
    property_projection: Option<&[Rc<str>]>,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let value = resolve_scan_value(value, ctx)?;
    let nodes = match property_projection {
        None => graph.scan_nodes_by_property(property, &value, cmp)?,
        Some(names) => {
            let v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            graph.scan_nodes_by_property_projected(property, &value, cmp, &v)?
        }
    };
    let mut next = Vec::new();
    for row in input {
        for node in &nodes {
            let mut out = row.clone();
            out.insert(Rc::<str>::from(variable), BindingValue::Node(node.clone()));
            next.push(out);
        }
    }
    Ok(next)
}

fn exec_index_intersection<G: GraphRead>(
    graph: &G,
    input: &[BindingRow],
    variable: &str,
    scans: &[IndexScanSpec],
    property_projection: Option<&[Rc<str>]>,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    if scans.len() < 2 {
        return Err(ExecutionError::InvalidPlan(
            "IndexIntersection requires at least two index scans".into(),
        ));
    }

    let mut id_sets: Vec<BTreeSet<NodeId>> = Vec::with_capacity(scans.len());
    for spec in scans {
        let value = resolve_scan_value(&spec.value, ctx)?;
        let nodes = graph.scan_nodes_by_property(spec.property.as_ref(), &value, spec.cmp)?;
        id_sets.push(nodes.iter().map(|n| n.id).collect());
    }

    let mut intersection: BTreeSet<NodeId> = id_sets[0].clone();
    for set in &id_sets[1..] {
        intersection = intersection.intersection(set).copied().collect();
    }

    let names_vec: Option<Vec<String>> =
        property_projection.map(|names| names.iter().map(|s| s.to_string()).collect());

    let mut nodes = Vec::new();
    for id in intersection {
        let node = match names_vec.as_ref() {
            None => graph.get_node(id)?,
            Some(v) => graph.get_node_projected(id, v)?,
        };
        if let Some(node) = node {
            nodes.push(node);
        }
    }

    let mut next = Vec::new();
    for row in input {
        for node in &nodes {
            let mut out = row.clone();
            out.insert(Rc::<str>::from(variable), BindingValue::Node(node.clone()));
            next.push(out);
        }
    }
    Ok(next)
}

fn exec_edge_index_scan<G: GraphRead>(
    graph: &G,
    input: &[BindingRow],
    variable: &str,
    property: &str,
    value: &ScanValue,
    property_projection: Option<&[Rc<str>]>,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let value = resolve_scan_value(value, ctx)?;
    let edges = match property_projection {
        None => graph.scan_edges_by_property(property, &value)?,
        Some(names) => {
            let v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            graph.scan_edges_by_property_projected(property, &value, &v)?
        }
    };
    let mut next = Vec::new();
    for row in input {
        for edge in &edges {
            let mut out = row.clone();
            out.insert(Rc::<str>::from(variable), BindingValue::Edge(edge.clone()));
            next.push(out);
        }
    }
    Ok(next)
}

fn exec_edge_bind_endpoints<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    edge_var: &str,
    near_var: &str,
    far_var: &str,
    direction: gleaph_gql::types::EdgeDirection,
    label: Option<&str>,
    near_property_projection: Option<&[Rc<str>]>,
    far_property_projection: Option<&[Rc<str>]>,
) -> ExecutionResultExt<Vec<BindingRow>> {
    use gleaph_gql::types::EdgeDirection;
    let near_names: Option<Vec<String>> =
        near_property_projection.map(|n| n.iter().map(|s| s.to_string()).collect());
    let far_names: Option<Vec<String>> =
        far_property_projection.map(|n| n.iter().map(|s| s.to_string()).collect());
    let mut out = Vec::new();
    for row in input {
        let edge_rec = match row.get(edge_var) {
            Some(BindingValue::Edge(e)) => e,
            Some(_) => {
                return Err(ExecutionError::TypeMismatch(
                    "EdgeBindEndpoints requires edge binding",
                ));
            }
            None => return Err(ExecutionError::MissingBinding(edge_var.to_owned())),
        };
        if label.is_some_and(|l| edge_rec.label.as_deref() != Some(l)) {
            continue;
        }
        let (near_id, far_id) = match direction {
            EdgeDirection::PointingRight => (edge_rec.src, edge_rec.dst),
            EdgeDirection::PointingLeft => (edge_rec.dst, edge_rec.src),
            EdgeDirection::LeftOrRight
            | EdgeDirection::Undirected
            | EdgeDirection::LeftOrUndirected
            | EdgeDirection::UndirectedOrRight
            | EdgeDirection::AnyDirection => {
                return Err(ExecutionError::UnsupportedPlanOp(
                    "EdgeBindEndpoints.direction",
                ));
            }
        };
        let Some(near_node) = (match near_names.as_ref() {
            None => graph.get_node(near_id)?,
            Some(v) => graph.get_node_projected(near_id, v)?,
        }) else {
            continue;
        };
        let Some(far_node) = (match far_names.as_ref() {
            None => graph.get_node(far_id)?,
            Some(v) => graph.get_node_projected(far_id, v)?,
        }) else {
            continue;
        };
        let mut next = row.clone();
        next.insert(Rc::<str>::from(near_var), BindingValue::Node(near_node));
        next.insert(Rc::<str>::from(far_var), BindingValue::Node(far_node));
        out.push(next);
    }
    Ok(out)
}

fn exec_conditional_index_scan<G: GraphRead>(
    graph: &G,
    input: &[BindingRow],
    candidates: &[ConditionalScanCandidate],
    fallback_label: Option<&str>,
    fallback_variable: &str,
    property_projection: Option<&[Rc<str>]>,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    for candidate in candidates {
        match lookup_param(ctx, candidate.param_name.as_ref()) {
            Value::Null => continue,
            _ => {
                let scan = ScanValue::Parameter(candidate.param_name.clone());
                return exec_index_scan(
                    graph,
                    input,
                    candidate.variable.as_ref(),
                    candidate.property.as_ref(),
                    &scan,
                    candidate.cmp,
                    property_projection,
                    ctx,
                );
            }
        }
    }

    exec_node_scan(
        graph,
        input,
        fallback_variable,
        fallback_label,
        property_projection,
    )
}

fn exec_project<G: GraphRead>(
    graph: &G,
    rows: &[BindingRow],
    columns: &[ProjectColumn],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<OutputRow>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut output = OutputRow::new();
        for (index, column) in columns.iter().enumerate() {
            let key = column_name(column);
            let value = eval_project_expr(graph, row, column, index, ctx)?;
            output.insert(key, value);
        }
        out.push(output);
    }
    Ok(out)
}

fn exec_hash_join<G: GraphRead + GraphWrite>(
    graph: &mut G,
    left: &[PlanOp],
    right: &[PlanOp],
    join_keys: &[Rc<str>],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let (left_rows, left_projected) = execute_ops(graph, left, ctx)?;
    let (right_rows, right_projected) = execute_ops(graph, right, ctx)?;
    if left_projected.is_some() || right_projected.is_some() {
        return Err(ExecutionError::UnsupportedPlanOp(
            "HashJoin.projected_subplan",
        ));
    }

    let mut buckets: Vec<(Vec<Value>, Vec<BindingRow>)> = Vec::new();
    for row in left_rows {
        let key = join_key_values(&row, join_keys)?;
        if let Some((_, rows)) = buckets
            .iter_mut()
            .find(|(bucket_key, _)| *bucket_key == key)
        {
            rows.push(row);
        } else {
            buckets.push((key, vec![row]));
        }
    }

    let mut out = Vec::new();
    for right_row in right_rows {
        let key = join_key_values(&right_row, join_keys)?;
        if let Some((_, left_bucket)) = buckets.iter().find(|(bucket_key, _)| *bucket_key == key) {
            for left_row in left_bucket {
                out.push(merge_rows(left_row, &right_row)?);
            }
        }
    }
    Ok(out)
}

fn exec_cartesian_product<G: GraphRead + GraphWrite>(
    graph: &mut G,
    left: &[PlanOp],
    right: &[PlanOp],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let (left_rows, left_projected) = execute_ops(graph, left, ctx)?;
    let (right_rows, right_projected) = execute_ops(graph, right, ctx)?;
    if left_projected.is_some() || right_projected.is_some() {
        return Err(ExecutionError::UnsupportedPlanOp(
            "CartesianProduct.projected_subplan",
        ));
    }

    let mut out = Vec::new();
    for left_row in &left_rows {
        for right_row in &right_rows {
            out.push(merge_rows(left_row, right_row)?);
        }
    }
    Ok(out)
}

fn exec_optional_match<G: GraphRead + GraphWrite>(
    graph: &mut G,
    input: Vec<BindingRow>,
    sub_plan: &[PlanOp],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let produced = collect_produced_vars(sub_plan);
    let mut out = Vec::new();

    for row in input {
        let (sub_rows, sub_projected) = execute_ops_from_rows(
            graph,
            sub_plan,
            ctx,
            vec![row.clone()],
            active_graph_from_context(ctx),
        )?;
        if sub_projected.is_some() {
            return Err(ExecutionError::UnsupportedPlanOp(
                "OptionalMatch.projected_subplan",
            ));
        }
        if sub_rows.is_empty() {
            let mut padded = row.clone();
            for var in &produced {
                padded
                    .entry(var.clone())
                    .or_insert_with(|| BindingValue::Scalar(Value::Null));
            }
            out.push(padded);
        } else {
            out.extend(sub_rows);
        }
    }

    Ok(out)
}

fn output_row_to_binding_row(output_row: OutputRow) -> ExecutionResultExt<BindingRow> {
    let mut out = BindingRow::new();
    for (key, value) in output_row {
        out.insert(Rc::<str>::from(key), value_to_binding_value(value)?);
    }
    Ok(out)
}

fn projected_column_keys_from_ops(ops: &[PlanOp]) -> Option<Vec<Rc<str>>> {
    for op in ops.iter().rev() {
        if let PlanOp::Project { columns, .. } = op {
            return Some(
                columns
                    .iter()
                    .map(|c| Rc::<str>::from(column_name(c)))
                    .collect(),
            );
        }
    }
    None
}

fn exec_call_procedure<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    name: &[Rc<str>],
    args: &[Expr],
    yield_columns: Option<&Vec<gleaph_gql_planner::plan::YieldColumn>>,
    optional: bool,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::new();

    for row in input {
        let evaluated_args = args
            .iter()
            .map(|expr| eval_expr(graph, &row, expr, ctx))
            .collect::<ExecutionResultExt<Vec<_>>>()?;
        let invocation = ProcedureInvocation {
            name: name.iter().map(|s| s.to_string()).collect(),
            args: evaluated_args,
            selected_graph: ctx.selected_graph.clone(),
            caller: ctx.caller.clone(),
        };
        let proc_rows = if let Some(registry) = &ctx.procedure_registry {
            registry.call(graph, &invocation)?
        } else {
            call_builtin_procedure(graph, &invocation)?
        };

        let Some(yield_columns) = yield_columns else {
            // Procedure is still executed (for side effects); without YIELD we preserve rows.
            out.push(row);
            continue;
        };

        if proc_rows.is_empty() {
            if optional {
                let mut padded = row;
                for yc in yield_columns {
                    let key = yc.alias.as_ref().unwrap_or(&yc.name).clone();
                    padded.insert(key, BindingValue::Scalar(Value::Null));
                }
                out.push(padded);
            }
            continue;
        }

        for proc_row in proc_rows {
            let mut next = row.clone();
            for yc in yield_columns {
                let key = yc.alias.as_ref().unwrap_or(&yc.name).clone();
                let value = proc_row
                    .get(yc.name.as_ref())
                    .cloned()
                    .unwrap_or(Value::Null);
                next.insert(key, value_to_binding_value(value)?);
            }
            out.push(next);
        }
    }

    Ok(out)
}

fn call_builtin_procedure<G: GraphRead>(
    graph: &G,
    invocation: &ProcedureInvocation,
) -> ExecutionResultExt<Vec<OutputRow>> {
    let is_db_labels =
        invocation.name.len() == 2 && invocation.name[0] == "db" && invocation.name[1] == "labels";
    if !is_db_labels {
        return Err(ExecutionError::UnsupportedPlanOp(
            "CallProcedure.unknown_procedure",
        ));
    }

    let mut label_set: BTreeSet<String> = BTreeSet::new();
    for node in graph.scan_nodes(None)? {
        for label in node.labels {
            label_set.insert(label);
        }
    }

    let mut out = Vec::new();
    for label in label_set {
        out.push(
            [
                ("label".to_owned(), Value::Text(label.clone())),
                ("lbl".to_owned(), Value::Text(label)),
            ]
            .into_iter()
            .collect(),
        );
    }
    Ok(out)
}

fn exec_inline_procedure_call<G: GraphRead + GraphWrite>(
    graph: &mut G,
    input: Vec<BindingRow>,
    sub_plan: &PhysicalPlan,
    scope_vars: &[Rc<str>],
    optional: bool,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let projected_keys = projected_column_keys_from_ops(&sub_plan.ops);
    let produced_vars = collect_produced_vars(&sub_plan.ops);

    let mut out = Vec::new();

    for row in input {
        // Build the initial scope row expected by the sub-plan.
        let mut scope_row = BindingRow::new();
        for var in scope_vars {
            let value = row
                .get(var.as_ref())
                .cloned()
                .ok_or_else(|| ExecutionError::MissingBinding(var.to_string()))?;
            scope_row.insert(var.clone(), value);
        }

        let (_sub_rows, sub_projected) = execute_ops_from_rows(
            graph,
            &sub_plan.ops,
            ctx,
            vec![scope_row],
            active_graph_from_context(ctx),
        )?;

        if let Some(output_rows) = sub_projected {
            if output_rows.is_empty() {
                if optional {
                    let mut padded = row;
                    let keys_to_pad = projected_keys.as_ref().unwrap_or(&produced_vars);
                    for key in keys_to_pad {
                        padded
                            .entry(key.clone())
                            .or_insert_with(|| BindingValue::Scalar(Value::Null));
                    }
                    out.push(padded);
                }
                continue;
            }

            for output_row in output_rows {
                let sub_binding = output_row_to_binding_row(output_row)?;
                let merged = merge_rows(&row, &sub_binding)?;
                out.push(merged);
            }
        } else if !_sub_rows.is_empty() {
            for sub_row in _sub_rows {
                let merged = merge_rows(&row, &sub_row)?;
                out.push(merged);
            }
        } else if optional {
            let mut padded = row;
            for var in &produced_vars {
                padded
                    .entry(var.clone())
                    .or_insert_with(|| BindingValue::Scalar(Value::Null));
            }
            out.push(padded);
        }
    }

    Ok(out)
}

fn exec_materialize<G: GraphRead>(
    graph: &G,
    rows: Vec<BindingRow>,
    projected: Option<Vec<OutputRow>>,
    columns: &[ProjectColumn],
    distinct: bool,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut materialized = if columns.is_empty() {
        if let Some(output_rows) = projected {
            materialize_output_rows(output_rows, columns)
        } else {
            rows
        }
    } else {
        rows.into_iter()
            .map(|row| {
                let mut out = BindingRow::new();
                for (index, column) in columns.iter().enumerate() {
                    let key = materialize_column_name(column, index);
                    let value = eval_materialize_expr(graph, &row, column, index, ctx)?;
                    out.insert(Rc::<str>::from(key), value);
                }
                Ok(out)
            })
            .collect::<ExecutionResultExt<Vec<_>>>()?
    };

    if distinct {
        dedup_binding_rows(&mut materialized);
    }
    Ok(materialized)
}

fn exec_set_operation(
    op: gleaph_gql::ast::SetOp,
    left: Vec<OutputRow>,
    right: Vec<OutputRow>,
) -> Vec<OutputRow> {
    use gleaph_gql::ast::SetOp;

    match op {
        SetOp::UnionAll => {
            let mut out = left;
            out.extend(right);
            out
        }
        SetOp::Union | SetOp::UnionDistinct => {
            let mut out = left;
            out.extend(right);
            dedup_output_rows_owned(out)
        }
        SetOp::ExceptAll => subtract_rows(left, right, false),
        SetOp::Except | SetOp::ExceptDistinct => subtract_rows(left, right, true),
        SetOp::IntersectAll => intersect_rows(left, right, false),
        SetOp::Intersect | SetOp::IntersectDistinct => intersect_rows(left, right, true),
        SetOp::Otherwise => {
            if left.is_empty() {
                right
            } else {
                left
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use gleaph_gql::Value;
    use gleaph_gql::ast::{
        CmpOp, Expr, ExprKind, NullOrder, OrderByClause, SortDirection, SortItem,
    };
    use gleaph_gql::token::Span;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql::types::PathElement;
    use gleaph_gql_planner::plan::{
        AggregateSpec, ConditionalScanCandidate, ProjectColumn, RemovePlanItem, ScanValue,
        SetPlanItem, ShortestMode, VarLenSpec,
    };
    use gleaph_gql_planner::{PhysicalPlan, PlanAnnotations, PlanOp};
    use gleaph_graph_kernel::{
        EdgeId, EdgeLabelFilter, EdgeRecord, Expansion, GraphRead, GraphResult, GraphWrite, NodeId,
        NodeRecord, PropertyMap,
    };
    use gleaph_graph_pma::facade::RewriteWriteEventProjection;
    use gleaph_graph_pma::integration::{RewriteOverlayEdgeMutationKind, RewriteOverlayWriteEvent};
    use gleaph_graph_pma::low_level::GraphMutationPath;
    use gleaph_graph_pma::observability::project_overlay_write_event;
    use gleaph_graph_pma::property_index::PropertyIndexNodeStoreMutationKind;
    use gleaph_graph_pma::{GraphPma, VecMemory};

    use self::backend_debug_helpers::{expect_graph_execution, expect_rewrite_overlay_execution};
    use self::overlay_test_helpers::{
        assert_last_projected_event, bootstrap_empty_rewrite_harness,
        bootstrap_rewrite_overlay_authored_and_liked_posts,
        bootstrap_rewrite_overlay_user_post_authored, bootstrap_rewrite_overlay_user_uid,
        projected_history,
    };
    use self::seed_helpers::{
        rewrite_seed_authored_and_liked_posts, rewrite_seed_user_post_authored,
        rewrite_seed_user_uid,
    };
    use super::{
        BindingRow, ExecutionContext, ExecutionError, ExecutionResultExt, GraphRegistryResolver,
        GraphResolution, OutputRow, ProcedureInvocation, ProcedureRegistry, UseGraphRouter,
        exec_set_operation, execute_plan, execute_plan_with_context,
    };
    use gleaph_graph_kernel::{GraphError, GraphErrorKind};
    use std::cell::RefCell;

    /// Test-only graph adapter that preserves the old `InMemoryGraph` helper API
    /// while running on top of graph-pma.
    struct InMemoryGraph {
        facade: RefCell<GraphPma>,
        memory: VecMemory,
    }

    impl InMemoryGraph {
        fn new() -> Self {
            let memory = VecMemory::default();
            let facade = GraphPma::bootstrap_empty(&memory).expect("bootstrap graph-pma");
            Self {
                facade: RefCell::new(facade),
                memory,
            }
        }

        fn with_overlay<R>(&self, f: impl FnOnce(&dyn GraphRead) -> R) -> R {
            let mut facade = self.facade.borrow_mut();
            let overlay = facade.bind_kernel_overlay(&self.memory);
            f(&overlay)
        }

        fn with_overlay_mut<R>(&self, f: impl FnOnce(&mut dyn GraphWrite) -> R) -> R {
            let mut facade = self.facade.borrow_mut();
            let mut overlay = facade.bind_kernel_overlay(&self.memory);
            let out = f(&mut overlay);
            let _ = facade.try_refresh_and_write_dirty_to_stable_memory(&self.memory);
            out
        }

        fn insert_node(
            &self,
            labels: impl IntoIterator<Item = &'static str>,
            properties: impl IntoIterator<Item = (&'static str, Value)>,
        ) -> NodeId {
            let labels: Vec<String> = labels.into_iter().map(str::to_owned).collect();
            let mut props = PropertyMap::new();
            for (k, v) in properties {
                props.insert(k.to_owned(), v);
            }
            self.with_overlay_mut(|g| g.insert_node(&labels, &props).expect("insert node").id)
        }

        fn insert_edge(
            &self,
            src: NodeId,
            dst: NodeId,
            label: Option<&str>,
            properties: impl IntoIterator<Item = (&'static str, Value)>,
        ) -> EdgeId {
            let mut props = PropertyMap::new();
            for (k, v) in properties {
                props.insert(k.to_owned(), v);
            }
            self.with_overlay_mut(|g| {
                g.insert_edge(src, dst, label, &props)
                    .expect("insert edge")
                    .id
            })
        }
    }

    impl GraphRead for InMemoryGraph {
        fn scan_nodes(&self, label: Option<&str>) -> GraphResult<Vec<NodeRecord>> {
            self.with_overlay(|g| g.scan_nodes(label))
        }
        fn scan_nodes_projected(
            &self,
            label: Option<&str>,
            property_names: &[String],
        ) -> GraphResult<Vec<NodeRecord>> {
            self.with_overlay(|g| g.scan_nodes_projected(label, property_names))
        }
        fn scan_nodes_by_property(
            &self,
            property: &str,
            value: &Value,
            cmp: CmpOp,
        ) -> GraphResult<Vec<NodeRecord>> {
            self.with_overlay(|g| g.scan_nodes_by_property(property, value, cmp))
        }
        fn scan_nodes_by_property_projected(
            &self,
            property: &str,
            value: &Value,
            cmp: CmpOp,
            property_names: &[String],
        ) -> GraphResult<Vec<NodeRecord>> {
            self.with_overlay(|g| {
                g.scan_nodes_by_property_projected(property, value, cmp, property_names)
            })
        }
        fn scan_edges_by_property(
            &self,
            property: &str,
            value: &Value,
        ) -> GraphResult<Vec<EdgeRecord>> {
            self.with_overlay(|g| g.scan_edges_by_property(property, value))
        }
        fn scan_edges_by_property_projected(
            &self,
            property: &str,
            value: &Value,
            property_names: &[String],
        ) -> GraphResult<Vec<EdgeRecord>> {
            self.with_overlay(|g| {
                g.scan_edges_by_property_projected(property, value, property_names)
            })
        }
        fn expand(
            &self,
            from: NodeId,
            direction: EdgeDirection,
            filter: EdgeLabelFilter<'_, '_>,
        ) -> GraphResult<Vec<Expansion>> {
            self.with_overlay(|g| g.expand(from, direction, filter))
        }
        fn expand_projected(
            &self,
            from: NodeId,
            direction: EdgeDirection,
            filter: EdgeLabelFilter<'_, '_>,
            edge_property_names: Option<&[String]>,
            dst_property_names: Option<&[String]>,
        ) -> GraphResult<Vec<Expansion>> {
            self.with_overlay(|g| {
                g.expand_projected(
                    from,
                    direction,
                    filter,
                    edge_property_names,
                    dst_property_names,
                )
            })
        }
        fn scan_all_edges(&self) -> GraphResult<Vec<EdgeRecord>> {
            self.with_overlay(|g| g.scan_all_edges())
        }
        fn get_node(&self, id: NodeId) -> GraphResult<Option<NodeRecord>> {
            self.with_overlay(|g| g.get_node(id))
        }
        fn get_node_projected(
            &self,
            id: NodeId,
            property_names: &[String],
        ) -> GraphResult<Option<NodeRecord>> {
            self.with_overlay(|g| g.get_node_projected(id, property_names))
        }
        fn get_edge_projected(
            &self,
            edge_id: EdgeId,
            property_names: &[String],
        ) -> GraphResult<Option<EdgeRecord>> {
            self.with_overlay(|g| g.get_edge_projected(edge_id, property_names))
        }
        fn all_property_key_names(&self) -> GraphResult<std::collections::BTreeSet<String>> {
            self.with_overlay(|g| g.all_property_key_names())
        }
        fn get_node_property_value(
            &self,
            node_id: NodeId,
            property: &str,
        ) -> GraphResult<Option<Value>> {
            self.with_overlay(|g| g.get_node_property_value(node_id, property))
        }
        fn get_edge_property_value(
            &self,
            edge_id: EdgeId,
            property: &str,
        ) -> GraphResult<Option<Value>> {
            self.with_overlay(|g| g.get_edge_property_value(edge_id, property))
        }
    }

    impl GraphWrite for InMemoryGraph {
        fn insert_node(
            &mut self,
            labels: &[String],
            properties: &PropertyMap,
        ) -> GraphResult<NodeRecord> {
            self.with_overlay_mut(|g| g.insert_node(labels, properties))
        }
        fn insert_edge(
            &mut self,
            src: NodeId,
            dst: NodeId,
            label: Option<&str>,
            properties: &PropertyMap,
        ) -> GraphResult<EdgeRecord> {
            self.with_overlay_mut(|g| g.insert_edge(src, dst, label, properties))
        }
        fn set_node_property(
            &mut self,
            node_id: NodeId,
            property: &str,
            value: &Value,
        ) -> GraphResult<NodeRecord> {
            self.with_overlay_mut(|g| g.set_node_property(node_id, property, value))
        }
        fn remove_node_property(
            &mut self,
            node_id: NodeId,
            property: &str,
        ) -> GraphResult<NodeRecord> {
            self.with_overlay_mut(|g| g.remove_node_property(node_id, property))
        }
        fn add_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord> {
            self.with_overlay_mut(|g| g.add_node_label(node_id, label))
        }
        fn remove_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord> {
            self.with_overlay_mut(|g| g.remove_node_label(node_id, label))
        }
        fn set_edge_property(
            &mut self,
            edge_id: EdgeId,
            property: &str,
            value: &Value,
        ) -> GraphResult<EdgeRecord> {
            self.with_overlay_mut(|g| g.set_edge_property(edge_id, property, value))
        }
        fn remove_edge_property(
            &mut self,
            edge_id: EdgeId,
            property: &str,
        ) -> GraphResult<EdgeRecord> {
            self.with_overlay_mut(|g| g.remove_edge_property(edge_id, property))
        }
        fn set_edge_label(
            &mut self,
            edge_id: EdgeId,
            label: Option<&str>,
        ) -> GraphResult<EdgeRecord> {
            self.with_overlay_mut(|g| g.set_edge_label(edge_id, label))
        }
        fn delete_edge(&mut self, edge_id: EdgeId) -> GraphResult<()> {
            self.with_overlay_mut(|g| g.delete_edge(edge_id))
        }
        fn delete_node(&mut self, node_id: NodeId, detach: bool) -> GraphResult<()> {
            self.with_overlay_mut(|g| g.delete_node(node_id, detach))
        }
    }

    #[test]
    fn execution_error_exposes_graph_error_kind() {
        let graph_err =
            ExecutionError::from(GraphError::property_index(std::io::Error::other("idx")));
        assert_eq!(
            graph_err.graph_error_kind(),
            Some(GraphErrorKind::PropertyIndex)
        );
        assert!(graph_err.as_graph_error().is_some());

        let other = ExecutionError::InvalidPlan("x".into());
        assert_eq!(other.graph_error_kind(), None);
        assert!(other.as_graph_error().is_none());
    }

    const DEBUG_NODE_PROPERTY_KEYS: &[&str] = &["uid", "title", "name"];
    const DEBUG_EDGE_PROPERTY_KEYS: &[&str] = &["weight", "score"];

    mod overlay_test_helpers {
        use gleaph_graph_pma::RewriteVecMemory;
        use gleaph_graph_pma::facade::RewriteWriteEventProjection;
        use gleaph_graph_pma::integration::{
            KernelBootstrapGraphSpec, RewriteGraphPmaKernelHarness, RewriteGraphPmaKernelOverlay,
            RewriteOverlayWriteEvent,
        };
        use gleaph_graph_pma::observability::{
            RewriteDiagnosticsView, last_projected_overlay_event, project_overlay_write_history,
        };

        pub(super) fn bootstrap_empty_rewrite_harness()
        -> RewriteGraphPmaKernelHarness<RewriteVecMemory> {
            RewriteGraphPmaKernelHarness::bootstrap_empty(RewriteVecMemory::default())
                .expect("bootstrap rewrite")
        }

        pub(super) fn projected_history(
            graph: &RewriteGraphPmaKernelOverlay<'_, RewriteVecMemory>,
        ) -> Vec<RewriteWriteEventProjection> {
            project_overlay_write_history(graph.write_history())
        }

        pub(super) fn assert_last_projected_event(
            graph: &RewriteGraphPmaKernelOverlay<'_, RewriteVecMemory>,
            expected: RewriteWriteEventProjection,
        ) {
            assert_eq!(
                last_projected_overlay_event(graph.write_history()).as_ref(),
                Some(&expected)
            );
        }

        pub(super) fn bootstrap_rewrite_overlay<'a>(
            harness: &'a mut RewriteGraphPmaKernelHarness<RewriteVecMemory>,
            spec: &KernelBootstrapGraphSpec,
        ) -> (
            RewriteGraphPmaKernelOverlay<'a, RewriteVecMemory>,
            gleaph_graph_pma::integration::KernelBootstrapGraphSummary,
        ) {
            let (graph, summary) = harness
                .bind_overlay_with_graph(spec)
                .expect("seed rewrite overlay graph");
            assert_overlay_bootstrap_projection_matches_event(&graph, &summary);
            (graph, summary)
        }

        pub(super) fn bootstrap_rewrite_overlay_user_uid<'a>(
            harness: &'a mut RewriteGraphPmaKernelHarness<RewriteVecMemory>,
            uid: &str,
        ) -> (
            RewriteGraphPmaKernelOverlay<'a, RewriteVecMemory>,
            gleaph_graph_pma::integration::KernelBootstrapGraphSummary,
        ) {
            let spec = super::rewrite_seed_user_uid(uid);
            bootstrap_rewrite_overlay(harness, &spec)
        }

        pub(super) fn bootstrap_rewrite_overlay_user_post_authored<'a>(
            harness: &'a mut RewriteGraphPmaKernelHarness<RewriteVecMemory>,
            uid: &str,
            title: &str,
            weight: i64,
        ) -> (
            RewriteGraphPmaKernelOverlay<'a, RewriteVecMemory>,
            gleaph_graph_pma::integration::KernelBootstrapGraphSummary,
        ) {
            let spec = super::rewrite_seed_user_post_authored(uid, title, weight);
            bootstrap_rewrite_overlay(harness, &spec)
        }

        pub(super) fn bootstrap_rewrite_overlay_authored_and_liked_posts<'a>(
            harness: &'a mut RewriteGraphPmaKernelHarness<RewriteVecMemory>,
        ) -> (
            RewriteGraphPmaKernelOverlay<'a, RewriteVecMemory>,
            gleaph_graph_pma::integration::KernelBootstrapGraphSummary,
        ) {
            let spec = super::rewrite_seed_authored_and_liked_posts();
            bootstrap_rewrite_overlay(harness, &spec)
        }

        fn assert_overlay_bootstrap_projection_matches_event(
            graph: &RewriteGraphPmaKernelOverlay<'_, RewriteVecMemory>,
            summary: &gleaph_graph_pma::integration::KernelBootstrapGraphSummary,
        ) {
            let event_projection = match graph.write_history().last() {
                Some(RewriteOverlayWriteEvent::BootstrapGraph(event_summary)) => {
                    event_summary.projection()
                }
                other => panic!("expected aggregate bootstrap event, got {other:?}"),
            };
            assert_eq!(summary.projection(), event_projection);
            let expected = format!(
                "bootstrap-graph vertices={} edges={} refreshed=({},{}) fwd={} rev={}",
                summary.vertex_ordinals.len(),
                summary.locators.len(),
                summary.refreshed.forward.len(),
                summary.refreshed.reverse.len(),
                super::backend_debug_helpers::format_usize_list(&summary.refreshed.forward),
                super::backend_debug_helpers::format_usize_list(&summary.refreshed.reverse),
            );
            assert_eq!(
                RewriteDiagnosticsView::formatted_last_write_event(graph),
                Some(expected.clone())
            );
            assert_eq!(
                RewriteDiagnosticsView::debug_report(graph)
                    .lines()
                    .last()
                    .map(str::to_owned),
                Some(expected)
            );
        }
    }

    mod backend_debug_helpers {
        use std::collections::BTreeSet;

        use gleaph_gql::types::EdgeDirection;
        use gleaph_graph_kernel::{EdgeRecord, GraphRead, GraphWrite, PropertyMap};
        use gleaph_graph_pma::RewriteVecMemory;
        use gleaph_graph_pma::integration::RewriteGraphPmaKernelOverlay;
        use gleaph_graph_pma::observability::RewriteDiagnosticsView;

        use super::{DEBUG_EDGE_PROPERTY_KEYS, DEBUG_NODE_PROPERTY_KEYS, ExecutionError};

        pub(super) fn expect_rewrite_overlay_execution<T>(
            graph: &RewriteGraphPmaKernelOverlay<'_, RewriteVecMemory>,
            result: super::super::ExecutionResultExt<T>,
            context: &str,
        ) -> T {
            expect_execution_with_report(
                result,
                context,
                "rewrite diagnostics",
                rewrite_overlay_debug_report(graph),
            )
        }

        pub(super) fn expect_graph_execution<G: GraphRead + GraphWrite, T>(
            graph: &G,
            result: super::super::ExecutionResultExt<T>,
            context: &str,
        ) -> T {
            expect_execution_with_report(
                result,
                context,
                "graph snapshot",
                persistent_graph_debug_report(graph),
            )
        }

        pub(super) fn format_usize_list(values: &[usize]) -> String {
            if values.is_empty() {
                return "[]".to_owned();
            }
            let joined = values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{joined}]")
        }

        pub(super) fn rewrite_overlay_debug_report(
            graph: &RewriteGraphPmaKernelOverlay<'_, RewriteVecMemory>,
        ) -> String {
            RewriteDiagnosticsView::debug_report(graph)
        }

        fn panic_with_debug_report(
            context: &str,
            error: &ExecutionError,
            label: &str,
            report: String,
        ) -> ! {
            panic!("{context}: {error}\n{label}:\n{report}")
        }

        fn expect_execution_with_report<T>(
            result: super::super::ExecutionResultExt<T>,
            context: &str,
            label: &str,
            report: String,
        ) -> T {
            match result {
                Ok(value) => value,
                Err(error) => panic_with_debug_report(context, &error, label, report),
            }
        }

        pub(super) fn persistent_graph_debug_report<G: GraphRead>(graph: &G) -> String {
            graph_debug_snapshot(graph)
        }

        // Persistent backend snapshot formatting helpers.
        fn graph_debug_snapshot<G: GraphRead>(graph: &G) -> String {
            match graph.scan_nodes(None) {
                Ok(nodes) => {
                    let (edge_ids, node_summaries) =
                        collect_graph_debug_snapshot_details(graph, &nodes);
                    format!(
                        "nodes={} edges={} node_ids={:?}\n{}",
                        nodes.len(),
                        edge_ids.len(),
                        nodes.iter().map(|node| node.id).collect::<Vec<_>>(),
                        node_summaries.join("\n")
                    )
                }
                Err(error) => format!("snapshot-error: {error}"),
            }
        }

        fn collect_graph_debug_snapshot_details<G: GraphRead>(
            graph: &G,
            nodes: &[gleaph_graph_kernel::NodeRecord],
        ) -> (BTreeSet<u64>, Vec<String>) {
            let mut edge_ids = BTreeSet::new();
            let mut node_summaries = Vec::new();
            for node in nodes {
                let outgoing = graph.expand(
                    node.id,
                    EdgeDirection::PointingRight,
                    gleaph_graph_kernel::EdgeLabelFilter::All,
                );
                let incoming = graph.expand(
                    node.id,
                    EdgeDirection::PointingLeft,
                    gleaph_graph_kernel::EdgeLabelFilter::All,
                );
                if let (Ok(outgoing_expansions), Ok(incoming_expansions)) = (outgoing, incoming) {
                    for expansion in outgoing_expansions.iter() {
                        edge_ids.insert(expansion.edge.id);
                    }
                    node_summaries.push(format_debug_node_summary(
                        node,
                        &incoming_expansions,
                        &outgoing_expansions,
                    ));
                } else {
                    node_summaries.push(format!(
                        "node {} labels={:?} props=[] degree=<expand-error>",
                        node.id, node.labels
                    ));
                }
            }
            (edge_ids, node_summaries)
        }

        fn format_debug_node_summary(
            node: &gleaph_graph_kernel::NodeRecord,
            incoming_expansions: &[gleaph_graph_kernel::Expansion],
            outgoing_expansions: &[gleaph_graph_kernel::Expansion],
        ) -> String {
            format!(
                "node {} labels={:?} props=[{}] degree=(in:{}, out:{}) in={:?} out={:?}",
                node.id,
                node.labels,
                format_debug_properties(&node.properties, DEBUG_NODE_PROPERTY_KEYS),
                incoming_expansions.len(),
                outgoing_expansions.len(),
                collect_debug_edge_labels(incoming_expansions),
                collect_debug_edge_labels(outgoing_expansions)
            )
        }

        fn collect_debug_edge_labels(expansions: &[gleaph_graph_kernel::Expansion]) -> Vec<String> {
            expansions
                .iter()
                .map(|expansion| format_debug_edge(&expansion.edge))
                .collect()
        }

        fn format_debug_edge(edge: &EdgeRecord) -> String {
            let label = edge.label.clone().unwrap_or_else(|| "_".to_owned());
            let suffix = format_debug_properties(&edge.properties, DEBUG_EDGE_PROPERTY_KEYS);
            if suffix.is_empty() {
                label
            } else {
                format!("{label}({suffix})")
            }
        }

        fn format_debug_properties(properties: &PropertyMap, keys: &[&str]) -> String {
            keys.iter()
                .filter_map(|key| properties.get(*key).map(|value| format!("{key}={value}")))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    mod seed_helpers {
        use gleaph_gql::Value;
        use gleaph_graph_kernel::PropertyMap;
        use gleaph_graph_pma::integration::{
            KernelBootstrapEdgeSpec, KernelBootstrapGraphSpec, KernelBootstrapNodeSpec,
        };

        pub(super) fn rewrite_seed_user_uid(uid: &str) -> KernelBootstrapGraphSpec {
            let properties: PropertyMap = [("uid".to_owned(), Value::Text(uid.to_owned()))]
                .into_iter()
                .collect();
            KernelBootstrapGraphSpec::empty()
                .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &properties))
        }

        pub(super) fn rewrite_seed_user_post_authored(
            uid: &str,
            title: &str,
            weight: i64,
        ) -> KernelBootstrapGraphSpec {
            let user_properties: PropertyMap = [("uid".to_owned(), Value::Text(uid.to_owned()))]
                .into_iter()
                .collect();
            let post_properties: PropertyMap =
                [("title".to_owned(), Value::Text(title.to_owned()))]
                    .into_iter()
                    .collect();
            let edge_properties: PropertyMap = [("weight".to_owned(), Value::Int64(weight))]
                .into_iter()
                .collect();
            KernelBootstrapGraphSpec::empty()
                .with_node(KernelBootstrapNodeSpec::from_parts(
                    &["User"],
                    &user_properties,
                ))
                .with_node(KernelBootstrapNodeSpec::from_parts(
                    &["Post"],
                    &post_properties,
                ))
                .with_edge(KernelBootstrapEdgeSpec::from_parts(
                    0,
                    1,
                    Some("AUTHORED"),
                    &edge_properties,
                ))
        }

        pub(super) fn rewrite_seed_authored_and_liked_posts() -> KernelBootstrapGraphSpec {
            KernelBootstrapGraphSpec::empty()
                .with_node(KernelBootstrapNodeSpec::labeled(
                    "User",
                    [("name".to_owned(), Value::Text("Alice".to_owned()))]
                        .into_iter()
                        .collect(),
                ))
                .with_node(KernelBootstrapNodeSpec::labeled(
                    "User",
                    [("name".to_owned(), Value::Text("Bob".to_owned()))]
                        .into_iter()
                        .collect(),
                ))
                .with_node(KernelBootstrapNodeSpec::labeled(
                    "Post",
                    [("title".to_owned(), Value::Text("Hello".to_owned()))]
                        .into_iter()
                        .collect(),
                ))
                .with_edge(KernelBootstrapEdgeSpec::new(
                    0,
                    2,
                    Some("AUTHORED".to_owned()),
                    [("since".to_owned(), Value::Int64(2024))]
                        .into_iter()
                        .collect(),
                ))
                .with_edge(KernelBootstrapEdgeSpec::new(
                    1,
                    2,
                    Some("LIKED".to_owned()),
                    [("since".to_owned(), Value::Int64(2025))]
                        .into_iter()
                        .collect(),
                ))
        }
    }

    #[test]
    fn executes_scan_filter_expand_project_limit_pipeline() {
        let mut harness = bootstrap_empty_rewrite_harness();
        let (mut graph, _) = bootstrap_rewrite_overlay_authored_and_liked_posts(&mut harness);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "u".into(),
                    label: Some("User".into()),
                },
                PlanOp::PropertyFilter {
                    predicates: vec![Expr::new(ExprKind::Compare {
                        left: Box::new(Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("u".to_owned()))),
                            property: "name".to_owned(),
                        })),
                        op: CmpOp::Eq,
                        right: Box::new(Expr::new(ExprKind::Literal(Value::Text(
                            "Alice".to_owned(),
                        )))),
                    })],
                    stage: 0,
                },
                PlanOp::Expand {
                    src: "u".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("u".to_owned()))),
                                property: "name".to_owned(),
                            }),
                            alias: Some("author".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                                property: "title".to_owned(),
                            }),
                            alias: Some("title".into()),
                        },
                    ],
                    distinct: false,
                },
                PlanOp::Limit {
                    count: Some(Expr::new(ExprKind::Literal(Value::Uint64(1)))),
                    offset: None,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("plan should execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("author"),
            Some(&Value::Text("Alice".to_owned()))
        );
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("Hello".to_owned()))
        );
    }

    #[test]
    fn executes_expand_with_indexed_edge_property_equality() {
        let mut harness = bootstrap_empty_rewrite_harness();
        let mut graph = harness.bind_overlay();
        let a = graph
            .insert_node(&["Person".to_owned()], &PropertyMap::new())
            .expect("insert a")
            .id;
        let b1 = graph
            .insert_node(&["Person".to_owned()], &PropertyMap::new())
            .expect("insert b1")
            .id;
        let b2 = graph
            .insert_node(&["Person".to_owned()], &PropertyMap::new())
            .expect("insert b2")
            .id;
        let mut p1 = PropertyMap::new();
        p1.insert("weight".to_owned(), Value::Int64(5));
        let _ = graph
            .insert_edge(a, b1, Some("REL"), &p1)
            .expect("insert e1");
        let mut p2 = PropertyMap::new();
        p2.insert("weight".to_owned(), Value::Int64(6));
        let _ = graph
            .insert_edge(a, b2, Some("REL"), &p2)
            .expect("insert e2");

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "a".into(),
                    label: Some("Person".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("REL".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: Some((
                        "weight".into(),
                        ScanValue::Literal(Value::Int64(5)),
                    )),
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("indexed expand should execute");
        assert_eq!(result.rows.len(), 1);
    }

    // Rewrite overlay read-path coverage.
    #[test]
    fn executes_scan_filter_expand_project_limit_pipeline_on_rewrite_overlay() {
        let mut harness = bootstrap_empty_rewrite_harness();
        let (mut graph, _) = bootstrap_rewrite_overlay_authored_and_liked_posts(&mut harness);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "u".into(),
                    label: Some("User".into()),
                },
                PlanOp::PropertyFilter {
                    predicates: vec![Expr::new(ExprKind::Compare {
                        left: Box::new(Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("u".to_owned()))),
                            property: "name".to_owned(),
                        })),
                        op: CmpOp::Eq,
                        right: Box::new(Expr::new(ExprKind::Literal(Value::Text(
                            "Alice".to_owned(),
                        )))),
                    })],
                    stage: 0,
                },
                PlanOp::Expand {
                    src: "u".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("u".to_owned()))),
                                property: "name".to_owned(),
                            }),
                            alias: Some("author".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                                property: "title".to_owned(),
                            }),
                            alias: Some("title".into()),
                        },
                    ],
                    distinct: false,
                },
                PlanOp::Limit {
                    count: Some(Expr::new(ExprKind::Literal(Value::Uint64(1)))),
                    offset: None,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let plan_result = execute_plan(&mut graph, &plan);
        let result = expect_rewrite_overlay_execution(
            &graph,
            plan_result,
            "rewrite overlay plan should execute",
        );
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("author"),
            Some(&Value::Text("Alice".to_owned()))
        );
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("Hello".to_owned()))
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_index_scan_and_conditional_fallback() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        graph.insert_node(["User"], [("uid", Value::Text("u2".to_owned()))]);

        let indexed = PhysicalPlan {
            ops: vec![
                PlanOp::IndexScan {
                    property_projection: None,
                    variable: "u".into(),
                    property: "uid".into(),
                    value: ScanValue::Parameter("uid".into()),
                    cmp: CmpOp::Eq,
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("u".to_owned()))),
                            property: "uid".to_owned(),
                        }),
                        alias: Some("uid".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan_with_context(
            &mut graph,
            &indexed,
            &ExecutionContext {
                params: [("uid".to_owned(), Value::Text("u2".to_owned()))]
                    .into_iter()
                    .collect(),
                caller: None,
                ..ExecutionContext::default()
            },
        )
        .expect("index scan should execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("uid"),
            Some(&Value::Text("u2".to_owned()))
        );

        let result = execute_plan_with_context(
            &mut graph,
            &indexed,
            &ExecutionContext {
                params: [("$uid".to_owned(), Value::Text("u1".to_owned()))]
                    .into_iter()
                    .collect(),
                caller: None,
                ..ExecutionContext::default()
            },
        )
        .expect("index scan should accept prefixed param key");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("uid"),
            Some(&Value::Text("u1".to_owned()))
        );

        let intersection = PhysicalPlan {
            ops: vec![
                PlanOp::IndexIntersection {
                    property_projection: None,
                    variable: "n".into(),
                    scans: vec![
                        gleaph_gql_planner::plan::IndexScanSpec {
                            property: "uid".into(),
                            value: ScanValue::Literal(Value::Text("a".into())),
                            cmp: CmpOp::Eq,
                        },
                        gleaph_gql_planner::plan::IndexScanSpec {
                            property: "email".into(),
                            value: ScanValue::Literal(Value::Text("a@x".into())),
                            cmp: CmpOp::Eq,
                        },
                    ],
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("n".to_owned()))),
                            property: "uid".to_owned(),
                        }),
                        alias: Some("uid".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let mut graph2 = InMemoryGraph::new();
        graph2.insert_node(
            ["User"],
            [
                ("uid", Value::Text("a".into())),
                ("email", Value::Text("a@x".into())),
            ],
        );
        graph2.insert_node(
            ["User"],
            [
                ("uid", Value::Text("a".into())),
                ("email", Value::Text("b@x".into())),
            ],
        );
        let result = execute_plan(&mut graph2, &intersection).expect("index intersection");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("uid"), Some(&Value::Text("a".into())));

        let conditional = PhysicalPlan {
            ops: vec![
                PlanOp::ConditionalIndexScan {
                    property_projection: None,
                    candidates: vec![ConditionalScanCandidate {
                        param_name: "uid".into(),
                        property: "uid".into(),
                        variable: "u".into(),
                        cmp: CmpOp::Eq,
                    }],
                    fallback_label: Some("User".into()),
                    fallback_variable: "u".into(),
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("u".to_owned()))),
                            property: "uid".to_owned(),
                        }),
                        alias: Some("uid".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result =
            execute_plan_with_context(&mut graph, &conditional, &ExecutionContext::default())
                .expect("conditional scan fallback should execute");
        assert_eq!(result.rows.len(), 2);

        let result = execute_plan_with_context(
            &mut graph,
            &conditional,
            &ExecutionContext {
                params: [("$uid".to_owned(), Value::Text("u2".to_owned()))]
                    .into_iter()
                    .collect(),
                caller: None,
                ..ExecutionContext::default()
            },
        )
        .expect("conditional scan should accept prefixed param key");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("uid"),
            Some(&Value::Text("u2".to_owned()))
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_expand_filter_and_topk() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);
        let bob = graph.insert_node(["User"], [("name", Value::Text("Bob".to_owned()))]);
        let p1 = graph.insert_node(
            ["Post"],
            [
                ("title", Value::Text("A".to_owned())),
                ("score", Value::Int64(10)),
            ],
        );
        let p2 = graph.insert_node(
            ["Post"],
            [
                ("title", Value::Text("B".to_owned())),
                ("score", Value::Int64(30)),
            ],
        );
        let p3 = graph.insert_node(
            ["Post"],
            [
                ("title", Value::Text("C".to_owned())),
                ("score", Value::Int64(20)),
            ],
        );
        graph.insert_edge(
            alice,
            p1,
            Some("AUTHORED"),
            std::iter::empty::<(&str, Value)>(),
        );
        graph.insert_edge(
            alice,
            p2,
            Some("AUTHORED"),
            std::iter::empty::<(&str, Value)>(),
        );
        graph.insert_edge(
            bob,
            p3,
            Some("AUTHORED"),
            std::iter::empty::<(&str, Value)>(),
        );

        let order_by = OrderByClause {
            span: Span::DUMMY,
            items: vec![SortItem {
                span: Span::DUMMY,
                expr: Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                    property: "score".to_owned(),
                }),
                direction: Some(SortDirection::Desc),
                null_order: Some(NullOrder::Last),
            }],
        };

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "u".into(),
                    label: Some("User".into()),
                },
                PlanOp::ExpandFilter {
                    src: "u".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    dst_filter: vec![Expr::new(ExprKind::Compare {
                        left: Box::new(Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                            property: "score".to_owned(),
                        })),
                        op: CmpOp::Ge,
                        right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(15)))),
                    })],
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::TopK {
                    order_by,
                    k: Expr::new(ExprKind::Literal(Value::Uint64(1))),
                    offset: None,
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                            property: "title".to_owned(),
                        }),
                        alias: Some("title".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("expand filter + topk should execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("B".to_owned()))
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_grouped_count_aggregate() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);
        let bob = graph.insert_node(["User"], [("name", Value::Text("Bob".to_owned()))]);
        let p1 = graph.insert_node(["Post"], [("title", Value::Text("P1".to_owned()))]);
        let p2 = graph.insert_node(["Post"], [("title", Value::Text("P2".to_owned()))]);
        let p3 = graph.insert_node(["Post"], [("title", Value::Text("P3".to_owned()))]);
        graph.insert_edge(
            alice,
            p1,
            Some("KNOWS"),
            std::iter::empty::<(&str, Value)>(),
        );
        graph.insert_edge(
            alice,
            p2,
            Some("KNOWS"),
            std::iter::empty::<(&str, Value)>(),
        );
        graph.insert_edge(bob, p3, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("KNOWS".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::Aggregate {
                    group_by: vec![Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("a".to_owned()))),
                        property: "name".to_owned(),
                    })],
                    aggregates: vec![AggregateSpec {
                        func: "CountStar".into(),
                        expr: None,
                        distinct: false,
                        alias: Some("cnt".into()),
                    }],
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("a".to_owned()))),
                                property: "name".to_owned(),
                            }),
                            alias: Some("name".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::Aggregate {
                                func: gleaph_gql::ast::AggregateFunc::CountStar,
                                expr: None,
                                expr2: None,
                                distinct: false,
                                order_by: None,
                                filter: None,
                            }),
                            alias: Some("cnt".into()),
                        },
                    ],
                    distinct: false,
                },
                PlanOp::Sort {
                    order_by: OrderByClause {
                        span: Span::DUMMY,
                        items: vec![SortItem {
                            span: Span::DUMMY,
                            expr: Expr::new(ExprKind::Variable("cnt".to_owned())),
                            direction: Some(SortDirection::Desc),
                            null_order: Some(NullOrder::Last),
                        }],
                    },
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("aggregate should execute");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Alice".to_owned()))
        );
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
        assert_eq!(
            result.rows[1].get("name"),
            Some(&Value::Text("Bob".to_owned()))
        );
        assert_eq!(result.rows[1].get("cnt"), Some(&Value::Int64(1)));
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_sum_min_max_aggregate() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);
        let bob = graph.insert_node(["User"], [("name", Value::Text("Bob".to_owned()))]);
        let p1 = graph.insert_node(["Post"], [("score", Value::Int64(10))]);
        let p2 = graph.insert_node(["Post"], [("score", Value::Int64(30))]);
        let p3 = graph.insert_node(["Post"], [("score", Value::Int64(20))]);
        graph.insert_edge(
            alice,
            p1,
            Some("KNOWS"),
            std::iter::empty::<(&str, Value)>(),
        );
        graph.insert_edge(
            alice,
            p2,
            Some("KNOWS"),
            std::iter::empty::<(&str, Value)>(),
        );
        graph.insert_edge(bob, p3, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("KNOWS".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::Aggregate {
                    group_by: vec![Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("a".to_owned()))),
                        property: "name".to_owned(),
                    })],
                    aggregates: vec![
                        AggregateSpec {
                            func: "Sum".into(),
                            expr: Some(Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("b".to_owned()))),
                                property: "score".to_owned(),
                            })),
                            distinct: false,
                            alias: Some("sum_score".into()),
                        },
                        AggregateSpec {
                            func: "Min".into(),
                            expr: Some(Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("b".to_owned()))),
                                property: "score".to_owned(),
                            })),
                            distinct: false,
                            alias: Some("min_score".into()),
                        },
                        AggregateSpec {
                            func: "Max".into(),
                            expr: Some(Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("b".to_owned()))),
                                property: "score".to_owned(),
                            })),
                            distinct: false,
                            alias: Some("max_score".into()),
                        },
                    ],
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("a".to_owned()))),
                                property: "name".to_owned(),
                            }),
                            alias: Some("name".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::Aggregate {
                                func: gleaph_gql::ast::AggregateFunc::Sum,
                                expr: Some(Box::new(Expr::new(ExprKind::PropertyAccess {
                                    expr: Box::new(Expr::new(ExprKind::Variable("b".to_owned()))),
                                    property: "score".to_owned(),
                                }))),
                                expr2: None,
                                distinct: false,
                                order_by: None,
                                filter: None,
                            }),
                            alias: Some("sum_score".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::Aggregate {
                                func: gleaph_gql::ast::AggregateFunc::Min,
                                expr: Some(Box::new(Expr::new(ExprKind::PropertyAccess {
                                    expr: Box::new(Expr::new(ExprKind::Variable("b".to_owned()))),
                                    property: "score".to_owned(),
                                }))),
                                expr2: None,
                                distinct: false,
                                order_by: None,
                                filter: None,
                            }),
                            alias: Some("min_score".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::Aggregate {
                                func: gleaph_gql::ast::AggregateFunc::Max,
                                expr: Some(Box::new(Expr::new(ExprKind::PropertyAccess {
                                    expr: Box::new(Expr::new(ExprKind::Variable("b".to_owned()))),
                                    property: "score".to_owned(),
                                }))),
                                expr2: None,
                                distinct: false,
                                order_by: None,
                                filter: None,
                            }),
                            alias: Some("max_score".into()),
                        },
                    ],
                    distinct: false,
                },
                PlanOp::Sort {
                    order_by: OrderByClause {
                        span: Span::DUMMY,
                        items: vec![SortItem {
                            span: Span::DUMMY,
                            expr: Expr::new(ExprKind::Variable("name".to_owned())),
                            direction: Some(SortDirection::Asc),
                            null_order: Some(NullOrder::Last),
                        }],
                    },
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("sum/min/max aggregate should execute");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Alice".to_owned()))
        );
        assert_eq!(result.rows[0].get("sum_score"), Some(&Value::Int64(40)));
        assert_eq!(result.rows[0].get("min_score"), Some(&Value::Int64(10)));
        assert_eq!(result.rows[0].get("max_score"), Some(&Value::Int64(30)));
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_avg_aggregate() {
        let mut graph = InMemoryGraph::new();
        let n1 = graph.insert_node(["N"], [("v", Value::Int64(10))]);
        let n2 = graph.insert_node(["N"], [("v", Value::Int64(30))]);
        let n3 = graph.insert_node(["N"], [("v", Value::Int64(20))]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "n".into(),
                    label: Some("N".into()),
                },
                PlanOp::Aggregate {
                    group_by: vec![],
                    aggregates: vec![AggregateSpec {
                        func: "Avg".into(),
                        expr: Some(Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("n".to_owned()))),
                            property: "v".to_owned(),
                        })),
                        distinct: false,
                        alias: Some("m".into()),
                    }],
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("m".to_owned())),
                        alias: Some("m".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("avg");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("m"),
            Some(&Value::Float64(20.0)),
            "mean of 10,30,20"
        );

        // distinct AVG
        let plan_distinct = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "n".into(),
                    label: Some("N".into()),
                },
                PlanOp::Aggregate {
                    group_by: vec![],
                    aggregates: vec![AggregateSpec {
                        func: "Avg".into(),
                        expr: Some(Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("n".to_owned()))),
                            property: "v".to_owned(),
                        })),
                        distinct: true,
                        alias: Some("m".into()),
                    }],
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("m".to_owned())),
                        alias: Some("m".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        let result_d = execute_plan(&mut graph, &plan_distinct).expect("avg distinct");
        assert_eq!(
            result_d.rows[0].get("m"),
            Some(&Value::Float64(20.0)),
            "distinct values 10,20,30"
        );
        let _ = (n1, n2, n3);
    }

    #[test]
    fn executes_plan_against_graph_pma_backend() {
        let mut harness =
            gleaph_graph_pma::integration::RewriteGraphPmaKernelHarness::bootstrap_empty(
                VecMemory::new(),
            )
            .expect("bootstrap");
        let mut graph = harness.bind_overlay();
        let alice = graph
            .insert_node(
                &["User".to_owned()],
                &std::collections::BTreeMap::from([(
                    "uid".to_owned(),
                    Value::Text("u1".to_owned()),
                )]),
            )
            .expect("insert alice")
            .id;
        let post = graph
            .insert_node(
                &["Post".to_owned()],
                &std::collections::BTreeMap::from([(
                    "title".to_owned(),
                    Value::Text("Hello".to_owned()),
                )]),
            )
            .expect("insert post")
            .id;
        graph
            .insert_edge(
                alice,
                post,
                Some("AUTHORED"),
                &std::collections::BTreeMap::from([("weight".to_owned(), Value::Int64(10))]),
            )
            .expect("insert authored edge");

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::IndexScan {
                    property_projection: None,
                    variable: "u".into(),
                    property: "uid".into(),
                    value: ScanValue::Literal(Value::Text("u1".to_owned())),
                    cmp: CmpOp::Eq,
                },
                PlanOp::Expand {
                    src: "u".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("u".to_owned()))),
                                property: "uid".to_owned(),
                            }),
                            alias: Some("uid".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                                property: "title".to_owned(),
                            }),
                            alias: Some("title".into()),
                        },
                    ],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let plan_result = execute_plan(&mut graph, &plan);
        let result =
            expect_graph_execution(&graph, plan_result, "plan should execute on graph-pma");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("uid"),
            Some(&Value::Text("u1".to_owned()))
        );
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("Hello".to_owned()))
        );
    }

    #[test]
    fn persistent_graph_debug_report_formats_representative_graph_shape() {
        let mut harness =
            gleaph_graph_pma::integration::RewriteGraphPmaKernelHarness::bootstrap_empty(
                VecMemory::new(),
            )
            .expect("bootstrap");
        let mut graph = harness.bind_overlay();
        let alice = graph
            .insert_node(
                &["User".to_owned()],
                &std::collections::BTreeMap::from([
                    ("uid".to_owned(), Value::Text("u1".to_owned())),
                    ("name".to_owned(), Value::Text("Alice".to_owned())),
                ]),
            )
            .expect("insert alice")
            .id;
        let post = graph
            .insert_node(
                &["Post".to_owned()],
                &std::collections::BTreeMap::from([(
                    "title".to_owned(),
                    Value::Text("Hello".to_owned()),
                )]),
            )
            .expect("insert post")
            .id;
        graph
            .insert_edge(
                alice,
                post,
                Some("AUTHORED"),
                &std::collections::BTreeMap::from([("weight".to_owned(), Value::Int64(10))]),
            )
            .expect("insert authored edge");

        let report = backend_debug_helpers::persistent_graph_debug_report(&graph);
        let lines: Vec<_> = report.lines().collect();

        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("nodes=2 edges=1 node_ids="));
        assert_eq!(
            lines[1],
            format!(
                "node {} labels=[\"User\"] props=[uid=u1, name=Alice] degree=(in:0, out:1) in=[] out=[\"AUTHORED(weight=10)\"]",
                alice
            )
        );
        assert_eq!(
            lines[2],
            format!(
                "node {} labels=[\"Post\"] props=[title=Hello] degree=(in:1, out:0) in=[\"AUTHORED(weight=10)\"] out=[]",
                post
            )
        );
    }

    #[test]
    fn executes_dml_against_graph_pma_backend_and_reopens() {
        let mut harness =
            gleaph_graph_pma::integration::RewriteGraphPmaKernelHarness::bootstrap_empty(
                VecMemory::new(),
            )
            .expect("bootstrap");
        let mut graph = harness.bind_overlay();
        let alice = graph
            .insert_node(
                &["User".to_owned()],
                &std::collections::BTreeMap::from([(
                    "uid".to_owned(),
                    Value::Text("u1".to_owned()),
                )]),
            )
            .expect("insert alice")
            .id;

        let insert_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::InsertVertex {
                    variable: Some("p".into()),
                    labels: vec!["Post".into()],
                    properties: vec![gleaph_gql_planner::plan::PropertyAssignment {
                        name: "title".into(),
                        value: Expr::new(ExprKind::Literal(Value::Text("draft".to_owned()))),
                    }],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["AUTHORED".into()],
                    properties: vec![gleaph_gql_planner::plan::PropertyAssignment {
                        name: "weight".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(1))),
                    }],
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                                property: "title".to_owned(),
                            }),
                            alias: Some("title".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("e".to_owned()))),
                                property: "weight".to_owned(),
                            }),
                            alias: Some("weight".into()),
                        },
                    ],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        let insert_result = execute_plan(&mut graph, &insert_plan);
        let result = expect_graph_execution(&graph, insert_result, "insert dml should execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("draft".to_owned()))
        );
        assert_eq!(result.rows[0].get("weight"), Some(&Value::Int64(1)));

        let update_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::SetProperties {
                    items: vec![
                        SetPlanItem::Property {
                            variable: "p".into(),
                            property: "title".into(),
                            value: Expr::new(ExprKind::Literal(Value::Text(
                                "published".to_owned(),
                            ))),
                        },
                        SetPlanItem::Property {
                            variable: "e".into(),
                            property: "score".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(9))),
                        },
                        SetPlanItem::Label {
                            variable: "a".into(),
                            label: "Author".into(),
                        },
                        SetPlanItem::Label {
                            variable: "e".into(),
                            label: "LIKES".into(),
                        },
                    ],
                },
                PlanOp::RemoveProperties {
                    items: vec![
                        RemovePlanItem::Property {
                            variable: "e".into(),
                            property: "weight".into(),
                        },
                        RemovePlanItem::Label {
                            variable: "a".into(),
                            label: "Author".into(),
                        },
                        RemovePlanItem::Label {
                            variable: "e".into(),
                            label: "LIKES".into(),
                        },
                    ],
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                                property: "title".to_owned(),
                            }),
                            alias: Some("title".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("e".to_owned()))),
                                property: "score".to_owned(),
                            }),
                            alias: Some("score".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("e".to_owned()))),
                                property: "weight".to_owned(),
                            }),
                            alias: Some("weight".into()),
                        },
                    ],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        let update_result = execute_plan(&mut graph, &update_plan);
        let result = expect_graph_execution(&graph, update_result, "update dml should execute");
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("published".to_owned()))
        );
        assert_eq!(result.rows[0].get("score"), Some(&Value::Int64(9)));
        assert_eq!(result.rows[0].get("weight"), Some(&Value::Null));

        let post_id = graph
            .expand(
                alice,
                EdgeDirection::PointingRight,
                gleaph_graph_kernel::EdgeLabelFilter::All,
            )
            .expect("expand")
            .into_iter()
            .next()
            .expect("edge exists")
            .node
            .id;

        let detach_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "p".into(),
                    label: Some("Post".into()),
                },
                PlanOp::DetachDeleteVertex {
                    variable: "p".into(),
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        let detach_result = execute_plan(&mut graph, &detach_plan);
        expect_graph_execution(&graph, detach_result, "detach delete should execute");
        assert!(graph.get_node(post_id).expect("get node").is_none());
        assert!(
            graph
                .expand(
                    alice,
                    EdgeDirection::PointingRight,
                    gleaph_graph_kernel::EdgeLabelFilter::Single("AUTHORED")
                )
                .expect("expand")
                .is_empty()
        );

        let memory = harness.memory().clone();
        harness
            .facade_mut()
            .try_write_all_to_stable_memory(&memory)
            .expect("flush before reopen");
        let mut reopened_facade = GraphPma::hydrate_from_stable_memory(
            harness.facade().manager().clone(),
            harness.memory(),
        )
        .expect("graph should reopen");
        let reopened = reopened_facade.bind_kernel_overlay(harness.memory());
        assert!(reopened.get_node(post_id).expect("get node").is_none());
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_cartesian_product_subplans() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        graph.insert_node(["Post"], [("title", Value::Text("p1".to_owned()))]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::CartesianProduct {
                    left: vec![PlanOp::NodeScan {
                        property_projection: None,
                        variable: "u".into(),
                        label: Some("User".into()),
                    }],
                    right: vec![PlanOp::NodeScan {
                        property_projection: None,
                        variable: "p".into(),
                        label: Some("Post".into()),
                    }],
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("u".to_owned()))),
                                property: "uid".to_owned(),
                            }),
                            alias: Some("uid".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                                property: "title".to_owned(),
                            }),
                            alias: Some("title".into()),
                        },
                    ],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("cartesian product should execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("uid"),
            Some(&Value::Text("u1".to_owned()))
        );
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("p1".to_owned()))
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_hash_join_subplans() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("hello".to_owned()))]);
        graph.insert_edge(
            alice,
            post,
            Some("AUTHORED"),
            std::iter::empty::<(&str, Value)>(),
        );

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::HashJoin {
                    left: vec![PlanOp::NodeScan {
                        property_projection: None,
                        variable: "a".into(),
                        label: Some("User".into()),
                    }],
                    right: vec![
                        PlanOp::NodeScan {
                            property_projection: None,
                            variable: "a".into(),
                            label: Some("User".into()),
                        },
                        PlanOp::Expand {
                            src: "a".into(),
                            edge: "e".into(),
                            dst: "p".into(),
                            direction: EdgeDirection::PointingRight,
                            label: Some("AUTHORED".into()),
                            label_expr: None,
                            var_len: None,
                            indexed_edge_equality: None,
                            edge_property_projection: None,
                            dst_property_projection: None,
                        },
                    ],
                    join_keys: vec!["a".into()],
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("a".to_owned()))),
                                property: "uid".to_owned(),
                            }),
                            alias: Some("uid".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                                property: "title".to_owned(),
                            }),
                            alias: Some("title".into()),
                        },
                    ],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("hash join should execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("uid"),
            Some(&Value::Text("u1".to_owned()))
        );
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("hello".to_owned()))
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_materialize_between_pipeline_stages() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("hello".to_owned()))]);
        graph.insert_edge(
            alice,
            post,
            Some("AUTHORED"),
            std::iter::empty::<(&str, Value)>(),
        );

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "n".into(),
                    label: Some("User".into()),
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("n".to_owned())),
                        alias: Some("n".into()),
                    }],
                    distinct: false,
                },
                PlanOp::Materialize {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("n".to_owned())),
                        alias: Some("n".into()),
                    }],
                    distinct: false,
                },
                PlanOp::Expand {
                    src: "n".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                            property: "title".to_owned(),
                        }),
                        alias: Some("title".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("materialize should execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("hello".to_owned()))
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_optional_match_with_null_padding() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let bob = graph.insert_node(["User"], [("uid", Value::Text("u2".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("hello".to_owned()))]);
        graph.insert_edge(
            alice,
            post,
            Some("AUTHORED"),
            std::iter::empty::<(&str, Value)>(),
        );

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "n".into(),
                    label: Some("User".into()),
                },
                PlanOp::OptionalMatch {
                    sub_plan: vec![PlanOp::Expand {
                        src: "n".into(),
                        edge: "e".into(),
                        dst: "m".into(),
                        direction: EdgeDirection::PointingRight,
                        label: Some("AUTHORED".into()),
                        label_expr: None,
                        var_len: None,
                        indexed_edge_equality: None,
                        edge_property_projection: None,
                        dst_property_projection: None,
                    }],
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("n".to_owned()))),
                                property: "uid".to_owned(),
                            }),
                            alias: Some("uid".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("m".to_owned()))),
                                property: "title".to_owned(),
                            }),
                            alias: Some("title".into()),
                        },
                    ],
                    distinct: false,
                },
                PlanOp::Sort {
                    order_by: OrderByClause {
                        span: Span::DUMMY,
                        items: vec![SortItem {
                            span: Span::DUMMY,
                            expr: Expr::new(ExprKind::Variable("uid".to_owned())),
                            direction: Some(SortDirection::Asc),
                            null_order: Some(NullOrder::Last),
                        }],
                    },
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("optional match should execute");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(
            result.rows[0].get("uid"),
            Some(&Value::Text("u1".to_owned()))
        );
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("hello".to_owned()))
        );
        assert_eq!(
            result.rows[1].get("uid"),
            Some(&Value::Text("u2".to_owned()))
        );
        assert_eq!(result.rows[1].get("title"), Some(&Value::Null));
        let _ = bob;
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_union_all_set_operation() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("alice".to_owned()))]);
        graph.insert_node(["Admin"], [("name", Value::Text("root".to_owned()))]);

        let right = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "m".into(),
                    label: Some("Admin".into()),
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("m".to_owned()))),
                            property: "name".to_owned(),
                        }),
                        alias: Some("name".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "n".into(),
                    label: Some("User".into()),
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("n".to_owned()))),
                            property: "name".to_owned(),
                        }),
                        alias: Some("name".into()),
                    }],
                    distinct: false,
                },
                PlanOp::SetOperation {
                    op: gleaph_gql::ast::SetOp::UnionAll,
                    right: Box::new(right),
                },
                PlanOp::Sort {
                    order_by: OrderByClause {
                        span: Span::DUMMY,
                        items: vec![SortItem {
                            span: Span::DUMMY,
                            expr: Expr::new(ExprKind::Variable("name".to_owned())),
                            direction: Some(SortDirection::Asc),
                            null_order: Some(NullOrder::Last),
                        }],
                    },
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("union all should execute");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("alice".to_owned()))
        );
        assert_eq!(
            result.rows[1].get("name"),
            Some(&Value::Text("root".to_owned()))
        );
    }

    // Rewrite overlay insert-path coverage.
    #[test]
    fn executes_insert_vertex_and_insert_edge_on_rewrite_overlay() {
        let mut harness = bootstrap_empty_rewrite_harness();
        let (mut graph, summary) = bootstrap_rewrite_overlay_user_uid(&mut harness, "u1");
        let alice = summary.nodes[0].id;

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::InsertVertex {
                    variable: Some("p".into()),
                    labels: vec!["Post".into()],
                    properties: vec![gleaph_gql_planner::plan::PropertyAssignment {
                        name: "title".into(),
                        value: Expr::new(ExprKind::Literal(Value::Text("hello".to_owned()))),
                    }],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["AUTHORED".into()],
                    properties: vec![gleaph_gql_planner::plan::PropertyAssignment {
                        name: "weight".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(10))),
                    }],
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                                property: "title".to_owned(),
                            }),
                            alias: Some("title".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("e".to_owned()))),
                                property: "weight".to_owned(),
                            }),
                            alias: Some("weight".into()),
                        },
                    ],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let plan_result = execute_plan(&mut graph, &plan);
        let result = expect_rewrite_overlay_execution(
            &graph,
            plan_result,
            "rewrite overlay insert dml should execute",
        );
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("hello".to_owned()))
        );
        assert_eq!(result.rows[0].get("weight"), Some(&Value::Int64(10)));
        let insert_summary = graph
            .last_insert_edge_summary()
            .expect("rewrite overlay insert summary");
        assert!(insert_summary.inserted);
        assert_eq!(graph.insert_edge_history().len(), 1);
        assert_eq!(insert_summary.ensure_capacity_projection(), None);
        assert!(matches!(
            graph.write_history().last(),
            Some(RewriteOverlayWriteEvent::InsertEdge(_))
        ));
        assert_eq!(
            gleaph_graph_pma::observability::project_overlay_write_event(
                graph.write_history().last().expect("last overlay event")
            ),
            vec![RewriteWriteEventProjection::InsertEdge(
                insert_summary.projection()
            )]
        );
        let insert_report = backend_debug_helpers::rewrite_overlay_debug_report(&graph);
        assert!(insert_report.contains("insert-edge inserted=true"));
        assert!(!insert_report.contains("ensure-capacity rebalanced=true"));
        assert_eq!(
            graph
                .expand(
                    alice,
                    EdgeDirection::PointingRight,
                    gleaph_graph_kernel::EdgeLabelFilter::Single("AUTHORED")
                )
                .expect("expand")
                .len(),
            1
        );
    }

    // Rewrite overlay property and node-delete DML coverage.
    #[test]
    fn executes_set_remove_and_delete_dml_on_rewrite_overlay() {
        let mut harness = bootstrap_empty_rewrite_harness();
        let (mut graph, summary) =
            bootstrap_rewrite_overlay_user_post_authored(&mut harness, "u1", "hello", 1);
        let alice = summary.nodes[0].id;
        let post = summary.nodes[1].id;
        let edge_id = summary.edges[0].id;

        let set_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::SetProperties {
                    items: vec![
                        SetPlanItem::Property {
                            variable: "p".into(),
                            property: "title".into(),
                            value: Expr::new(ExprKind::Literal(Value::Text("updated".to_owned()))),
                        },
                        SetPlanItem::Property {
                            variable: "e".into(),
                            property: "weight".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(5))),
                        },
                        SetPlanItem::Label {
                            variable: "a".into(),
                            label: "Author".into(),
                        },
                    ],
                },
                PlanOp::RemoveProperties {
                    items: vec![
                        RemovePlanItem::Property {
                            variable: "e".into(),
                            property: "weight".into(),
                        },
                        RemovePlanItem::Label {
                            variable: "a".into(),
                            label: "Author".into(),
                        },
                    ],
                },
                PlanOp::Project {
                    columns: vec![
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                                property: "title".to_owned(),
                            }),
                            alias: Some("title".into()),
                        },
                        ProjectColumn {
                            expr: Expr::new(ExprKind::PropertyAccess {
                                expr: Box::new(Expr::new(ExprKind::Variable("e".to_owned()))),
                                property: "weight".to_owned(),
                            }),
                            alias: Some("weight".into()),
                        },
                    ],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let plan_result = execute_plan(&mut graph, &set_plan);
        let result = expect_rewrite_overlay_execution(
            &graph,
            plan_result,
            "rewrite overlay set/remove should execute",
        );
        assert_eq!(
            result.rows[0].get("title"),
            Some(&Value::Text("updated".to_owned()))
        );
        assert_eq!(result.rows[0].get("weight"), Some(&Value::Null));
        assert_eq!(
            graph.get_node(alice).expect("node").unwrap().labels,
            vec!["User".to_owned()]
        );
        let property_summary = graph
            .last_property_write_summary()
            .expect("rewrite overlay property write summary");
        assert!(property_summary.flushed_sections.property_store);
        assert!(property_summary.flushed_sections.logical_index);
        assert_eq!(
            property_summary.mutation.node_store_operations,
            vec![PropertyIndexNodeStoreMutationKind::Collapse]
        );
        assert!(!property_summary.mutation.touched_node_ids.is_empty());
        assert_eq!(graph.property_write_history().len(), 3);
        let property_events: Vec<_> = projected_history(&graph)
            .into_iter()
            .filter_map(|event| match event {
                RewriteWriteEventProjection::Property(summary) => Some(summary),
                _ => None,
            })
            .collect();
        assert_eq!(property_events.len(), 3);
        assert_eq!(property_events.last(), Some(&property_summary.projection()));
        assert_eq!(
            project_overlay_write_event(graph.write_history().last().expect("last overlay event")),
            vec![RewriteWriteEventProjection::Property(
                property_summary.projection()
            )]
        );
        let property_report = backend_debug_helpers::rewrite_overlay_debug_report(&graph);
        assert!(property_report.contains("ops=collapse"));
        assert!(property_report.contains("touched:"));
        assert!(property_report.contains("flushed=(true,true,true)"));
        assert!(matches!(
            graph.write_history(),
            [
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapEdge(_),
                RewriteOverlayWriteEvent::BootstrapGraph(_),
                RewriteOverlayWriteEvent::Property(_),
                RewriteOverlayWriteEvent::Property(_),
                RewriteOverlayWriteEvent::Property(_)
            ]
        ));
        let _ = edge_id;
        let property_event_count_before_delete = graph.property_write_history().len();

        let delete_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "p".into(),
                    label: Some("Post".into()),
                },
                PlanOp::DeleteVertex {
                    variable: "p".into(),
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        let delete_err =
            execute_plan(&mut graph, &delete_plan).expect_err("delete without detach should fail");
        assert!(matches!(delete_err, ExecutionError::Graph(_)));

        let detach_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "p".into(),
                    label: Some("Post".into()),
                },
                PlanOp::DetachDeleteVertex {
                    variable: "p".into(),
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        let detach_result = execute_plan(&mut graph, &detach_plan);
        expect_rewrite_overlay_execution(
            &graph,
            detach_result,
            "rewrite overlay detach delete should succeed",
        );
        let delete_summary = graph
            .last_node_delete_summary()
            .expect("rewrite overlay node delete summary");
        assert!(delete_summary.detached);
        assert_eq!(delete_summary.deleted_edge_ids.len(), 1);
        assert_eq!(
            delete_summary.edge_writes[0].operation,
            RewriteOverlayEdgeMutationKind::Delete
        );
        let delete_event_projection = graph
            .write_history()
            .iter()
            .find_map(RewriteOverlayWriteEvent::node_delete_projection)
            .expect("rewrite overlay node delete event projection");
        assert_eq!(delete_summary.projection(), delete_event_projection);
        assert_eq!(
            project_overlay_write_event(graph.write_history().last().expect("last overlay event")),
            vec![RewriteWriteEventProjection::NodeDelete(
                delete_summary.projection()
            )]
        );
        assert_last_projected_event(
            &graph,
            gleaph_graph_pma::facade::RewriteWriteEventProjection::NodeDelete(
                delete_summary.projection(),
            ),
        );
        assert_eq!(graph.node_delete_history().len(), 1);
        assert!(
            graph.property_write_history().len() > property_event_count_before_delete,
            "expected detach delete to emit property cleanup events, got {:?}",
            graph.formatted_write_history()
        );
        let history = graph.write_history();
        assert!(
            history[..history.len() - 1]
                .iter()
                .any(|event| matches!(event, RewriteOverlayWriteEvent::Property(_)))
        );
        assert!(
            history[..history.len() - 1]
                .iter()
                .any(|event| matches!(event, RewriteOverlayWriteEvent::Edge(_)))
        );
        assert!(matches!(
            history.last(),
            Some(RewriteOverlayWriteEvent::NodeDelete(_))
        ));
        assert!(graph.get_node(post).expect("node").is_none());
        assert_eq!(
            graph
                .expand(
                    alice,
                    EdgeDirection::PointingRight,
                    gleaph_graph_kernel::EdgeLabelFilter::Single("AUTHORED")
                )
                .expect("expand")
                .len(),
            0
        );
    }

    // Rewrite overlay edge delete coverage.
    #[test]
    fn executes_delete_edge_dml_on_rewrite_overlay() {
        let mut harness = bootstrap_empty_rewrite_harness();
        let (mut graph, summary) =
            bootstrap_rewrite_overlay_user_post_authored(&mut harness, "u1", "hello", 1);
        let alice = summary.nodes[0].id;
        let post = summary.nodes[1].id;

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::DeleteEdge {
                    variable: "e".into(),
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let plan_result = execute_plan(&mut graph, &plan);
        expect_rewrite_overlay_execution(
            &graph,
            plan_result,
            "rewrite overlay delete edge should execute",
        );
        assert!(
            !graph.property_write_history().is_empty(),
            "expected edge delete to emit property cleanup events, got {:?}",
            graph.formatted_write_history()
        );
        let summary = graph
            .last_edge_write_summary()
            .expect("rewrite overlay edge delete summary");
        assert_eq!(summary.operation, RewriteOverlayEdgeMutationKind::Delete);
        assert_eq!(summary.path, GraphMutationPath::Base);
        assert!(summary.refreshed.forward.contains(&0));
        assert_eq!(
            project_overlay_write_event(graph.write_history().last().expect("last overlay event")),
            vec![RewriteWriteEventProjection::Edge(summary.projection())]
        );
        assert_eq!(graph.edge_write_history().len(), 1);
        let history = graph.write_history();
        assert!(matches!(
            history.first(),
            Some(RewriteOverlayWriteEvent::BootstrapNode(_))
        ));
        assert!(matches!(
            history.get(1),
            Some(RewriteOverlayWriteEvent::BootstrapNode(_))
        ));
        assert!(matches!(
            history.get(2),
            Some(RewriteOverlayWriteEvent::BootstrapEdge(_))
        ));
        assert!(matches!(
            history.get(3),
            Some(RewriteOverlayWriteEvent::BootstrapGraph(_))
        ));
        assert!(
            history[..history.len() - 1]
                .iter()
                .any(|event| matches!(event, RewriteOverlayWriteEvent::Property(_)))
        );
        assert!(matches!(
            history.last(),
            Some(RewriteOverlayWriteEvent::Edge(_))
        ));
        assert_eq!(
            graph
                .expand(
                    alice,
                    EdgeDirection::PointingRight,
                    gleaph_graph_kernel::EdgeLabelFilter::Single("AUTHORED")
                )
                .expect("expand")
                .len(),
            0
        );
        assert!(graph.get_node(post).expect("get node").is_some());
    }

    // Rewrite overlay edge label DML coverage.
    #[test]
    fn executes_set_and_remove_edge_label_on_rewrite_overlay() {
        let mut harness = bootstrap_empty_rewrite_harness();
        let (mut graph, summary) =
            bootstrap_rewrite_overlay_user_post_authored(&mut harness, "u1", "hello", 1);
        let alice = summary.nodes[0].id;

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    label_expr: None,
                    var_len: None,
                    indexed_edge_equality: None,
                    edge_property_projection: None,
                    dst_property_projection: None,
                },
                PlanOp::SetProperties {
                    items: vec![SetPlanItem::Label {
                        variable: "e".into(),
                        label: "LIKES".into(),
                    }],
                },
                PlanOp::RemoveProperties {
                    items: vec![RemovePlanItem::Label {
                        variable: "e".into(),
                        label: "LIKES".into(),
                    }],
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let plan_result = execute_plan(&mut graph, &plan);
        expect_rewrite_overlay_execution(
            &graph,
            plan_result,
            "rewrite overlay edge-label dml should execute",
        );
        let summary = graph
            .last_edge_write_summary()
            .expect("rewrite overlay edge label summary");
        assert_eq!(
            summary.operation,
            RewriteOverlayEdgeMutationKind::ReplaceLabel
        );
        assert_eq!(summary.path, GraphMutationPath::Base);
        assert!(summary.refreshed.forward.contains(&0));
        assert_eq!(
            project_overlay_write_event(graph.write_history().last().expect("last overlay event")),
            vec![RewriteWriteEventProjection::Edge(summary.projection())]
        );
        assert_eq!(graph.edge_write_history().len(), 2);
        assert!(matches!(
            graph.write_history(),
            [
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapEdge(_),
                RewriteOverlayWriteEvent::BootstrapGraph(_),
                RewriteOverlayWriteEvent::Edge(_),
                RewriteOverlayWriteEvent::Edge(_)
            ]
        ));
        assert_eq!(
            graph
                .expand(
                    alice,
                    EdgeDirection::PointingRight,
                    gleaph_graph_kernel::EdgeLabelFilter::Single("AUTHORED")
                )
                .expect("expand")
                .len(),
            0
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn rejects_plan_with_fatal_dml_errors_before_execution() {
        let mut graph = InMemoryGraph::new();
        let plan = PhysicalPlan {
            ops: vec![],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics {
                dml_errors: vec![gleaph_gql_planner::plan::PlannerDiagnostic {
                    code: "DML002",
                    message: gleaph_gql::type_check::dml_target_value_message("DELETE", Some("x")),
                    span: Span::DUMMY,
                    severity: gleaph_gql::type_check::DmlDiagnosticSeverity::Fatal,
                }],
                ..gleaph_gql_planner::plan::PlanDiagnostics::default()
            },
            annotations: PlanAnnotations::default(),
        };

        let err = execute_plan(&mut graph, &plan).expect_err("fatal dml error should fail");
        assert!(matches!(err, ExecutionError::InvalidPlan(_)));
    }

    #[test]
    fn shortest_path_plan_anchors_inline_where_with_index_scan() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program = parser::parse(
            "MATCH ANY SHORTEST (a:U WHERE a.name = 'a')-[:KNOWS]->{1,3}(b:U) RETURN a, b",
        )
        .expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        assert!(
            matches!(
                plan.ops.first(),
                Some(PlanOp::IndexScan {
                    variable,
                    property,
                    cmp: CmpOp::Eq,
                    ..
                }) if &**variable == "a" && &**property == "name"
            ),
            "expected first op to be name equality index scan, got {:?}",
            plan.ops
        );
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::ShortestPath { src, .. } if &**src == "a")),
            "expected shortest path anchored from `a`, got {:?}",
            plan.ops
        );
    }

    #[test]
    fn bench_profile_smoke_shortest_path_source_binding() {
        if std::env::var_os("GLEAPH_BENCH_PROFILE").is_none() {
            return;
        }

        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;
        use std::time::Instant;

        let program = parser::parse(
            "MATCH ANY SHORTEST (a:U WHERE a.name = 'a')-[:KNOWS]->{1,3}(b:U) RETURN a, b",
        )
        .expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");
        let index_scan = plan
            .ops
            .iter()
            .find_map(|op| match op {
                PlanOp::IndexScan {
                    variable,
                    property,
                    value,
                    cmp,
                    property_projection,
                } => Some((
                    variable.clone(),
                    property.clone(),
                    value.clone(),
                    *cmp,
                    property_projection.clone(),
                )),
                _ => None,
            })
            .expect("index scan");
        let shortest = plan
            .ops
            .iter()
            .find_map(|op| match op {
                PlanOp::ShortestPath {
                    src,
                    dst,
                    edge,
                    path_var,
                    mode,
                    direction,
                    label,
                    label_expr,
                    var_len,
                } => Some((
                    src.clone(),
                    dst.clone(),
                    edge.clone(),
                    path_var.clone(),
                    *mode,
                    *direction,
                    label.clone(),
                    label_expr.clone(),
                    var_len.clone(),
                )),
                _ => None,
            })
            .expect("shortest path");

        let spec = gleaph_graph_pma::integration::KernelBootstrapGraphSpec::empty()
            .with_node(gleaph_graph_pma::integration::KernelBootstrapNodeSpec::from_parts(
                &["U"],
                &[("name".to_owned(), Value::Text("a".into()))]
                    .into_iter()
                    .collect(),
            ))
            .with_node(gleaph_graph_pma::integration::KernelBootstrapNodeSpec::from_parts(
                &["U"],
                &[("name".to_owned(), Value::Text("b".into()))]
                    .into_iter()
                    .collect(),
            ))
            .with_node(gleaph_graph_pma::integration::KernelBootstrapNodeSpec::from_parts(
                &["U"],
                &[("name".to_owned(), Value::Text("c".into()))]
                    .into_iter()
                    .collect(),
            ))
            .with_edge(gleaph_graph_pma::integration::KernelBootstrapEdgeSpec::from_parts(
                0,
                1,
                Some("KNOWS"),
                &PropertyMap::new(),
            ))
            .with_edge(gleaph_graph_pma::integration::KernelBootstrapEdgeSpec::from_parts(
                1,
                2,
                Some("KNOWS"),
                &PropertyMap::new(),
            ));
        let mut harness = bootstrap_empty_rewrite_harness();
        let (graph, _) = harness.bind_overlay_with_graph(&spec).expect("seed");

        let iterations = 2_000usize;
        let ctx = ExecutionContext::default();

        let t_scan = Instant::now();
        let mut scanned_rows = Vec::new();
        for _ in 0..iterations {
            scanned_rows = super::exec_index_scan(
                &graph,
                &[BindingRow::new()],
                index_scan.0.as_ref(),
                index_scan.1.as_ref(),
                &index_scan.2,
                index_scan.3,
                index_scan.4.as_deref(),
                &ctx,
            )
            .expect("exec index scan");
        }
        let scan_elapsed = t_scan.elapsed();
        assert_eq!(scanned_rows.len(), 1, "expected single anchor row");

        let t_shortest = Instant::now();
        let mut shortest_rows = Vec::new();
        for _ in 0..iterations {
            shortest_rows = super::exec_shortest_path(
                &graph,
                scanned_rows.clone(),
                super::ShortestPathSpec {
                    src: shortest.0.as_ref(),
                    dst: shortest.1.as_ref(),
                    edge_var: shortest.2.as_ref(),
                    path_var: shortest.3.as_deref(),
                    mode: shortest.4,
                    direction: shortest.5,
                    label: shortest.6.as_deref(),
                    label_expr: shortest.7.as_ref(),
                    var_len: shortest.8.as_ref(),
                },
            )
            .expect("exec shortest path");
        }
        let shortest_elapsed = t_shortest.elapsed();
        assert_eq!(shortest_rows.len(), 2, "expected two reachable destinations");

        eprintln!(
            "GLEAPH_BENCH_PROFILE shortest_path_source_binding scan={scan_elapsed:?} shortest={shortest_elapsed:?} rows={} iters={iterations}",
            shortest_rows.len()
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_any_shortest_path_bounded_hops() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program = parser::parse(
            "MATCH ANY SHORTEST (a:U WHERE a.name = 'a')-[:KNOWS]->{1,3}(b:U) RETURN a, b",
        )
        .expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::ShortestPath { .. })),
            "expected ShortestPath in {:?}",
            plan.ops
        );

        let mut graph = InMemoryGraph::new();
        let n1 = graph.insert_node(["U"], [("name", Value::Text("a".into()))]);
        let n2 = graph.insert_node(["U"], [("name", Value::Text("b".into()))]);
        let n3 = graph.insert_node(["U"], [("name", Value::Text("c".into()))]);
        graph.insert_edge(n1, n2, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(n2, n3, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let result = execute_plan(&mut graph, &plan).expect("shortest path should run");
        assert_eq!(
            result.rows.len(),
            2,
            "from anchor `a` only: b at 1 hop and c at 2 hops; got {:?}",
            result.rows
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_any_shortest_path_union_edge_labels() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program = parser::parse(
            "MATCH ANY SHORTEST (a:U WHERE a.name = 'a')-/KNOWS|OTHER/->{1,3}(b:U) RETURN a, b",
        )
        .expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");
        assert!(
            plan.ops.iter().any(|op| matches!(
                op,
                PlanOp::ShortestPath {
                    label_expr: Some(_),
                    ..
                }
            )),
            "expected ShortestPath with compound label_expr: {:?}",
            plan.ops
        );

        let mut graph = InMemoryGraph::new();
        let n1 = graph.insert_node(["U"], [("name", Value::Text("a".into()))]);
        let n2 = graph.insert_node(["U"], [("name", Value::Text("b".into()))]);
        let n3 = graph.insert_node(["U"], [("name", Value::Text("c".into()))]);
        let n4 = graph.insert_node(["U"], [("name", Value::Text("d".into()))]);
        graph.insert_edge(n1, n2, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(n2, n3, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(n1, n4, Some("OTHER"), std::iter::empty::<(&str, Value)>());

        let result = execute_plan(&mut graph, &plan).expect("shortest path with union labels");
        assert_eq!(
            result.rows.len(),
            3,
            "expect ←1 KNOWS→ b, ←2 KNOWS→ c, ←1 OTHER→ d: {:?}",
            result.rows
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn expand_three_way_label_disjunction_reaches_matching_edge() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program =
            parser::parse("MATCH (a:U WHERE a.name = 'a')-[:KNOWS|LIKES|WORKS]->(b:U) RETURN b")
                .expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        let mut graph = InMemoryGraph::new();
        let n1 = graph.insert_node(["U"], [("name", Value::Text("a".into()))]);
        let n2 = graph.insert_node(["U"], [("name", Value::Text("x".into()))]);
        graph.insert_edge(n1, n2, Some("WORKS"), std::iter::empty::<(&str, Value)>());

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(
            result.rows.len(),
            1,
            "WORKS is one of three OR labels: {:?}",
            result.rows
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_wcoj_triangle_cycle() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program = parser::parse(
            "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) RETURN a",
        )
        .expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::WorstCaseOptimalJoin { .. })),
            "expected WorstCaseOptimalJoin in {:?}",
            plan.ops
        );

        let mut graph = InMemoryGraph::new();
        let n1 = graph.insert_node(["Person"], std::iter::empty::<(&str, Value)>());
        let n2 = graph.insert_node(["Person"], std::iter::empty::<(&str, Value)>());
        let n3 = graph.insert_node(["Person"], std::iter::empty::<(&str, Value)>());
        graph.insert_edge(n1, n2, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(n2, n3, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(n3, n1, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let result = execute_plan(&mut graph, &plan).expect("wcoj executes");
        assert_eq!(
            result.rows.len(),
            3,
            "each triangle corner can anchor `a`: {:?}",
            result.rows
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn returns_non_dml_type_warnings_in_execution_result() {
        let mut graph = InMemoryGraph::new();
        let plan = PhysicalPlan {
            ops: vec![],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics {
                type_warnings: vec![gleaph_gql::type_check::TypeDiagnostic {
                    code: None,
                    message: "LIMIT expects a numeric expression, got String".into(),
                    span: Span::DUMMY,
                    severity: gleaph_gql::type_check::DiagnosticSeverity::Warning,
                }],
                ..gleaph_gql_planner::plan::PlanDiagnostics::default()
            },
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("non-fatal warnings should succeed");
        assert_eq!(
            result.warnings,
            vec!["[TYPE] at 0..0: LIMIT expects a numeric expression, got String".to_owned()]
        );
        assert_eq!(result.summary.row_count, 1);
        assert_eq!(result.summary.warning_count, 1);
        assert!(!result.summary.had_dml);
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn execution_summary_marks_dml_and_row_count() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    property_projection: None,
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::InsertVertex {
                    variable: Some("p".into()),
                    labels: vec!["Post".into()],
                    properties: vec![],
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("p".to_owned())),
                        alias: Some("p".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("dml plan should execute");
        assert_eq!(result.summary.row_count, result.rows.len());
        assert_eq!(result.summary.warning_count, 0);
        assert!(result.summary.had_dml);
    }

    #[test]
    fn executes_except_distinct_set_operation() {
        let left = vec![
            [("name".to_owned(), Value::Text("alice".to_owned()))]
                .into_iter()
                .collect::<OutputRow>(),
            [("name".to_owned(), Value::Text("bob".to_owned()))]
                .into_iter()
                .collect::<OutputRow>(),
        ];
        let right = vec![
            [("name".to_owned(), Value::Text("alice".to_owned()))]
                .into_iter()
                .collect::<OutputRow>(),
        ];

        let result = exec_set_operation(gleaph_gql::ast::SetOp::ExceptDistinct, left, right);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].get("name"), Some(&Value::Text("bob".to_owned())));
    }

    #[test]
    fn executes_otherwise_set_operation() {
        let left = Vec::<OutputRow>::new();
        let right = vec![
            [("name".to_owned(), Value::Text("fallback".to_owned()))]
                .into_iter()
                .collect::<OutputRow>(),
        ];

        let result = exec_set_operation(gleaph_gql::ast::SetOp::Otherwise, left, right);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].get("name"),
            Some(&Value::Text("fallback".to_owned()))
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_filter_statement() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program =
            parser::parse("MATCH (n:User) FILTER n.age = 30 RETURN n.name").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::Filter { .. })),
            "expected PlanOp::Filter, got: {:?}",
            plan.ops
        );

        let mut graph = InMemoryGraph::new();
        graph.insert_node(
            ["User"],
            [
                ("name", Value::Text("alice".to_owned())),
                ("age", Value::Int64(30)),
            ],
        );
        graph.insert_node(
            ["User"],
            [
                ("name", Value::Text("bob".to_owned())),
                ("age", Value::Int64(31)),
            ],
        );

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("alice".to_owned()))
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_let_statement() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program = parser::parse("MATCH (n:User) LET x = n.name RETURN x").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        assert!(
            plan.ops.iter().any(|op| matches!(op, PlanOp::Let { .. })),
            "expected PlanOp::Let, got: {:?}",
            plan.ops
        );

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("alice".to_owned()))]);
        graph.insert_node(["User"], [("name", Value::Text("bob".to_owned()))]);

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(result.rows.len(), 2);
        let got: std::collections::BTreeSet<_> = result
            .rows
            .iter()
            .filter_map(|r| match r.get("x") {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            std::collections::BTreeSet::from([String::from("alice"), String::from("bob")])
        );
    }

    #[test]
    fn executes_for_statement_with_ordinality() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program =
            parser::parse("FOR x IN [1, 2, 3] WITH ORDINALITY i RETURN x, i").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        assert!(
            plan.ops.iter().any(|op| matches!(op, PlanOp::For { .. })),
            "expected PlanOp::For, got: {:?}",
            plan.ops
        );

        let mut graph = InMemoryGraph::new();
        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(result.rows.len(), 3);

        let mut pairs = result
            .rows
            .iter()
            .filter_map(|r| {
                fn as_i64(v: &Value) -> Option<i64> {
                    match v {
                        Value::Int64(i) => Some(*i),
                        Value::Int32(i) => Some(*i as i64),
                        Value::Int16(i) => Some(*i as i64),
                        Value::Int8(i) => Some(*i as i64),
                        Value::Uint64(u) => Some(*u as i64),
                        Value::Uint32(u) => Some(*u as i64),
                        Value::Uint16(u) => Some(*u as i64),
                        Value::Uint8(u) => Some(*u as i64),
                        _ => None,
                    }
                }

                match (r.get("x"), r.get("i")) {
                    (Some(x), Some(i)) => Some((as_i64(x)?, as_i64(i)?)),
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        pairs.sort();
        assert_eq!(pairs, vec![(1, 1), (2, 2), (3, 3)]);
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_call_procedure_db_labels() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program = parser::parse("CALL db.labels() YIELD lbl RETURN lbl").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);
        graph.insert_node(["Post"], [("title", Value::Text("Hello".to_owned()))]);

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert!(!result.rows.is_empty());

        let got: std::collections::BTreeSet<_> = result
            .rows
            .iter()
            .filter_map(|r| match r.get("lbl") {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(
            got,
            std::collections::BTreeSet::from([String::from("Post"), String::from("User")])
        );
    }

    #[test]
    fn executes_call_procedure_optional_empty_graph() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program =
            parser::parse("OPTIONAL CALL db.labels() YIELD lbl RETURN lbl").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        let mut graph = InMemoryGraph::new();
        let result = execute_plan(&mut graph, &plan).expect("execute");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("lbl"), Some(&Value::Null));
    }

    #[test]
    fn executes_call_procedure_via_custom_registry() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        struct EchoRegistry;
        impl ProcedureRegistry for EchoRegistry {
            fn call(
                &self,
                _graph: &dyn GraphRead,
                invocation: &ProcedureInvocation,
            ) -> ExecutionResultExt<Vec<OutputRow>> {
                if invocation.name != vec!["app".to_owned(), "echo".to_owned()] {
                    return Err(ExecutionError::UnsupportedPlanOp(
                        "CallProcedure.unknown_procedure",
                    ));
                }
                let value = invocation.args.first().cloned().unwrap_or(Value::Null);
                Ok(vec![
                    [("value".to_owned(), value)]
                        .into_iter()
                        .collect::<OutputRow>(),
                ])
            }
        }

        let program = parser::parse("CALL app.echo(42) YIELD value RETURN value").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        let mut graph = InMemoryGraph::new();
        let ctx = ExecutionContext {
            procedure_registry: Some(std::sync::Arc::new(EchoRegistry)),
            ..ExecutionContext::default()
        };
        let result = execute_plan_with_context(&mut graph, &plan, &ctx).expect("execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("value"), Some(&Value::Int64(42)));
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_inline_procedure_call_returns_nodes() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program = parser::parse("CALL { MATCH (n:User) RETURN n } RETURN n").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);
        graph.insert_node(["User"], [("name", Value::Text("Bob".to_owned()))]);
        graph.insert_node(["Post"], [("title", Value::Text("Hello".to_owned()))]);

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(result.rows.len(), 2);

        let mut got = std::collections::BTreeSet::new();
        for r in &result.rows {
            let v = r.get("n").expect("n");
            match v {
                Value::Record(fields) => {
                    let labels = fields
                        .iter()
                        .find(|(k, _)| k == "labels")
                        .map(|(_, v)| v)
                        .expect("labels");
                    if let Value::List(items) = labels {
                        for item in items {
                            if let Value::Text(s) = item {
                                got.insert(s.clone());
                            }
                        }
                    }
                }
                other => panic!("expected record, got {other:?}"),
            }
        }

        assert!(got.contains("User"));
    }

    #[test]
    fn executes_inline_procedure_call_optional_padding() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program =
            parser::parse("OPTIONAL CALL { MATCH (n:User) RETURN n } RETURN n").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["Post"], [("title", Value::Text("Hello".to_owned()))]);

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("n"), Some(&Value::Null));
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_use_graph_scoped_call_procedure() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program =
            parser::parse("USE myGraph CALL db.labels() YIELD lbl RETURN lbl").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);

        let result = execute_plan(&mut graph, &plan).expect("execute");

        let got: std::collections::BTreeSet<_> = result
            .rows
            .iter()
            .filter_map(|r| match r.get("lbl") {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        assert_eq!(
            got,
            std::collections::BTreeSet::from([String::from("User")])
        );
    }

    #[test]
    fn rejects_use_graph_when_not_in_available_graphs() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program =
            parser::parse("USE myGraph CALL db.labels() YIELD lbl RETURN lbl").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);

        let ctx = ExecutionContext {
            available_graphs: std::collections::BTreeSet::from([String::from("otherGraph")]),
            ..ExecutionContext::default()
        };
        let err = execute_plan_with_context(&mut graph, &plan, &ctx).expect_err("should fail");
        assert!(
            matches!(err, ExecutionError::InvalidPlan(message) if message.contains("unknown graph"))
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn resolves_use_current_graph_from_context() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program = parser::parse("USE CURRENT_GRAPH CALL db.labels() YIELD lbl RETURN lbl")
            .expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);

        let ctx = ExecutionContext {
            default_graph: Some("myGraph".to_owned()),
            available_graphs: std::collections::BTreeSet::from([String::from("myGraph")]),
            ..ExecutionContext::default()
        };
        let result = execute_plan_with_context(&mut graph, &plan, &ctx).expect("execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("lbl"),
            Some(&Value::Text("User".to_owned()))
        );
    }

    #[test]
    fn use_graph_uses_registry_resolver_for_named_graph() {
        struct StaticResolver;
        impl GraphRegistryResolver for StaticResolver {
            fn resolve(
                &self,
                requested_graph: &str,
                _caller: Option<&Value>,
            ) -> ExecutionResultExt<GraphResolution> {
                if requested_graph == "tenant.main" {
                    return Ok(GraphResolution {
                        graph_name: "tenant.main".to_owned(),
                        canister_id: None,
                    });
                }
                Err(ExecutionError::InvalidPlan("graph not found".to_owned()))
            }
        }

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::UseGraph {
                    graph_name: vec!["tenant".into(), "main".into()],
                    sub_plan: None,
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::new(ExprKind::Literal(Value::Int64(1))),
                        alias: Some("ok".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let mut graph = InMemoryGraph::new();
        let ctx = ExecutionContext {
            graph_registry_resolver: Some(std::sync::Arc::new(StaticResolver)),
            ..ExecutionContext::default()
        };
        let result = execute_plan_with_context(&mut graph, &plan, &ctx).expect("execute");
        assert_eq!(result.rows[0].get("ok"), Some(&Value::Int64(1)));
    }

    #[test]
    fn use_graph_remote_requires_router_and_can_delegate() {
        struct RemoteResolver;
        impl GraphRegistryResolver for RemoteResolver {
            fn resolve(
                &self,
                requested_graph: &str,
                _caller: Option<&Value>,
            ) -> ExecutionResultExt<GraphResolution> {
                Ok(GraphResolution {
                    graph_name: requested_graph.to_owned(),
                    canister_id: Some("rrkah-fqaaa-aaaaa-aaaaq-cai".to_owned()),
                })
            }
        }

        struct MockRouter;
        #[async_trait::async_trait(?Send)]
        impl UseGraphRouter for MockRouter {
            async fn execute_remote_subplan(
                &self,
                _target: &GraphResolution,
                _sub_plan: &[PlanOp],
                _ctx: &ExecutionContext,
                _input_rows: Vec<BindingRow>,
            ) -> ExecutionResultExt<(Vec<BindingRow>, Option<Vec<OutputRow>>)> {
                Ok((
                    Vec::new(),
                    Some(vec![
                        [("lbl".to_owned(), Value::Text("remote".to_owned()))]
                            .into_iter()
                            .collect::<OutputRow>(),
                    ]),
                ))
            }
        }

        let plan = PhysicalPlan {
            ops: vec![PlanOp::UseGraph {
                graph_name: vec!["remote".into()],
                sub_plan: Some(vec![PlanOp::CallProcedure {
                    name: vec!["db".into(), "labels".into()],
                    args: vec![],
                    yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                        name: "lbl".into(),
                        alias: None,
                    }]),
                    optional: false,
                }]),
            }],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let mut graph = InMemoryGraph::new();
        let no_router_ctx = ExecutionContext {
            graph_registry_resolver: Some(std::sync::Arc::new(RemoteResolver)),
            ..ExecutionContext::default()
        };
        let err = execute_plan_with_context(&mut graph, &plan, &no_router_ctx)
            .expect_err("remote graph without router should fail");
        assert!(
            matches!(err, ExecutionError::InvalidPlan(message) if message.contains("requires router"))
        );

        let with_router_ctx = ExecutionContext {
            graph_registry_resolver: Some(std::sync::Arc::new(RemoteResolver)),
            use_graph_router: Some(std::sync::Arc::new(MockRouter)),
            ..ExecutionContext::default()
        };
        let delegated = execute_plan_with_context(&mut graph, &plan, &with_router_ctx)
            .expect("remote delegated execution");
        assert_eq!(
            delegated.rows.first().and_then(|row| row.get("lbl")),
            Some(&Value::Text("remote".to_owned()))
        );
    }

    #[test]
    fn use_graph_remote_rejects_unsupported_pushdown_shape_before_routing() {
        struct RemoteResolver;
        impl GraphRegistryResolver for RemoteResolver {
            fn resolve(
                &self,
                requested_graph: &str,
                _caller: Option<&Value>,
            ) -> ExecutionResultExt<GraphResolution> {
                Ok(GraphResolution {
                    graph_name: requested_graph.to_owned(),
                    canister_id: Some("rrkah-fqaaa-aaaaa-aaaaq-cai".to_owned()),
                })
            }
        }

        struct PanicRouter;
        #[async_trait::async_trait(?Send)]
        impl UseGraphRouter for PanicRouter {
            async fn execute_remote_subplan(
                &self,
                _target: &GraphResolution,
                _sub_plan: &[PlanOp],
                _ctx: &ExecutionContext,
                _input_rows: Vec<BindingRow>,
            ) -> ExecutionResultExt<(Vec<BindingRow>, Option<Vec<OutputRow>>)> {
                panic!("router should not be called for unsupported pushdown");
            }
        }

        let plan = PhysicalPlan {
            ops: vec![PlanOp::UseGraph {
                graph_name: vec!["remote".into()],
                sub_plan: Some(vec![PlanOp::ShortestPath {
                    src: "a".into(),
                    dst: "b".into(),
                    edge: "e".into(),
                    path_var: None,
                    mode: ShortestMode::AnyShortest,
                    direction: gleaph_gql::types::EdgeDirection::PointingRight,
                    label: Some("KNOWS".into()),
                    label_expr: None,
                    var_len: Some(VarLenSpec {
                        min: 1,
                        max: Some(3),
                    }),
                }]),
            }],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let mut graph = InMemoryGraph::new();
        let ctx = ExecutionContext {
            graph_registry_resolver: Some(std::sync::Arc::new(RemoteResolver)),
            use_graph_router: Some(std::sync::Arc::new(PanicRouter)),
            ..ExecutionContext::default()
        };
        let err = execute_plan_with_context(&mut graph, &plan, &ctx)
            .expect_err("unsupported remote pushdown should fail before routing");
        assert!(matches!(
            err,
            ExecutionError::InvalidPlan(message)
                if message.contains("remote USE GRAPH pushdown unavailable")
                    && message.contains("remote")
        ));
    }

    #[test]
    fn local_use_graph_with_unsupported_remote_pushdown_shape_surfaces_warning() {
        let plan = PhysicalPlan {
            ops: vec![PlanOp::UseGraph {
                graph_name: vec!["localGraph".into()],
                sub_plan: Some(vec![
                    PlanOp::NodeScan {
                        variable: "a".into(),
                        label: Some("User".into()),
                        property_projection: None,
                    },
                    PlanOp::NodeScan {
                        variable: "b".into(),
                        label: Some("User".into()),
                        property_projection: None,
                    },
                ]),
            }],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations {
                optimizer: gleaph_gql_planner::plan::OptimizerPlanAnnotations {
                    use_graph_pushdown: vec![gleaph_gql_planner::UseGraphPushdownInfo {
                        graph_name: "localGraph".to_owned(),
                        supported: false,
                        reason: Some(
                            "unsupported remote USE GRAPH op after root: NODE SCAN".to_owned(),
                        ),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        };

        let mut graph = InMemoryGraph::new();
        let result = execute_plan_with_context(&mut graph, &plan, &ExecutionContext::default())
            .expect("local execution should still succeed");
        assert_eq!(result.summary.warning_count, 1);
        assert!(result.warnings[0].contains("remote USE GRAPH pushdown unavailable"));
        assert!(result.warnings[0].contains("localGraph"));
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_expand_var_len_bounded_hops() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program =
            parser::parse("MATCH (a:U WHERE a.name = 'a')-/KNOWS{1,2}/->(b:U) RETURN b.name")
                .expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        assert!(
            plan.ops.iter().any(|op| matches!(
                op,
                PlanOp::Expand {
                    var_len: Some(_),
                    ..
                }
            )),
            "expected Expand.var_len, got: {:?}",
            plan.ops
        );

        let mut graph = InMemoryGraph::new();
        let a = graph.insert_node(["U"], [("name", Value::Text("a".into()))]);
        let b = graph.insert_node(["U"], [("name", Value::Text("b".into()))]);
        let c = graph.insert_node(["U"], [("name", Value::Text("c".into()))]);
        graph.insert_edge(a, b, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(b, c, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(result.rows.len(), 2, "result={:?}", result.rows);

        let got: std::collections::BTreeSet<_> = result
            .rows
            .iter()
            .filter_map(|r| match r.get("name") {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            std::collections::BTreeSet::from([String::from("b"), String::from("c")])
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn executes_expand_filter_var_len_dst_filter() {
        use gleaph_gql::ast::Statement;
        use gleaph_gql::parser;

        let program = parser::parse(
            "MATCH (a:U WHERE a.name = 'a')-/KNOWS{1,2}/->(b:U WHERE b.name = 'c') RETURN b.name",
        )
        .expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let q = match &block.first {
            Statement::Query(c) => c.left.clone(),
            other => panic!("expected query, got {other:?}"),
        };
        let plan = gleaph_gql_planner::build_plan(&q, None).expect("build_plan");

        assert!(
            plan.ops.iter().any(|op| matches!(
                op,
                PlanOp::ExpandFilter {
                    var_len: Some(_),
                    ..
                }
            )),
            "expected ExpandFilter.var_len, got: {:?}",
            plan.ops
        );

        let mut graph = InMemoryGraph::new();
        let a = graph.insert_node(["U"], [("name", Value::Text("a".into()))]);
        let b = graph.insert_node(["U"], [("name", Value::Text("b".into()))]);
        let c = graph.insert_node(["U"], [("name", Value::Text("c".into()))]);
        graph.insert_edge(a, b, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(b, c, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(result.rows.len(), 1, "result={:?}", result.rows);

        assert_eq!(result.rows[0].get("name"), Some(&Value::Text("c".into())));
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn shortest_path_path_var_any_shortest() {
        use gleaph_gql_planner::plan::{ShortestMode, VarLenSpec};

        let mut graph = InMemoryGraph::new();
        let a = graph.insert_node(["U"], [("name", Value::Text("a".into()))]);
        let b1 = graph.insert_node(["U"], [("name", Value::Text("b1".into()))]);
        let b2 = graph.insert_node(["U"], [("name", Value::Text("b2".into()))]);
        let d = graph.insert_node(["U"], [("name", Value::Text("d".into()))]);

        graph.insert_edge(a, b1, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(a, b2, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(b1, d, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(b2, d, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let expected_b1 = Value::Path(vec![
            PathElement::Vertex(a.into()),
            PathElement::Edge {
                src: a.into(),
                dst: b1.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(b1.into()),
            PathElement::Edge {
                src: b1.into(),
                dst: d.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(d.into()),
        ]);
        let expected_b2 = Value::Path(vec![
            PathElement::Vertex(a.into()),
            PathElement::Edge {
                src: a.into(),
                dst: b2.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(b2.into()),
            PathElement::Edge {
                src: b2.into(),
                dst: d.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(d.into()),
        ]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::IndexScan {
                    property_projection: None,
                    variable: "a".into(),
                    property: "name".into(),
                    value: ScanValue::Literal(Value::Text("a".into())),
                    cmp: CmpOp::Eq,
                },
                PlanOp::ShortestPath {
                    src: "a".into(),
                    dst: "d".into(),
                    edge: "e".into(),
                    path_var: Some("p".into()),
                    mode: ShortestMode::AnyShortest,
                    direction: EdgeDirection::PointingRight,
                    label: Some("KNOWS".into()),
                    label_expr: None,
                    var_len: Some(VarLenSpec {
                        min: 2,
                        max: Some(2),
                    }),
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(result.rows.len(), 1, "result={:?}", result.rows);
        let got = result
            .rows
            .first()
            .and_then(|r| r.get("p"))
            .cloned()
            .expect("path var present");
        assert!(
            got == expected_b1 || got == expected_b2,
            "expected one shortest path, got={got:?}"
        );
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn shortest_path_path_var_all_shortest() {
        use gleaph_gql_planner::plan::{ShortestMode, VarLenSpec};

        let mut graph = InMemoryGraph::new();
        let a = graph.insert_node(["U"], [("name", Value::Text("a".into()))]);
        let b1 = graph.insert_node(["U"], [("name", Value::Text("b1".into()))]);
        let b2 = graph.insert_node(["U"], [("name", Value::Text("b2".into()))]);
        let d = graph.insert_node(["U"], [("name", Value::Text("d".into()))]);

        graph.insert_edge(a, b1, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(a, b2, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(b1, d, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(b2, d, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let expected_b1 = Value::Path(vec![
            PathElement::Vertex(a.into()),
            PathElement::Edge {
                src: a.into(),
                dst: b1.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(b1.into()),
            PathElement::Edge {
                src: b1.into(),
                dst: d.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(d.into()),
        ]);
        let expected_b2 = Value::Path(vec![
            PathElement::Vertex(a.into()),
            PathElement::Edge {
                src: a.into(),
                dst: b2.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(b2.into()),
            PathElement::Edge {
                src: b2.into(),
                dst: d.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(d.into()),
        ]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::IndexScan {
                    property_projection: None,
                    variable: "a".into(),
                    property: "name".into(),
                    value: ScanValue::Literal(Value::Text("a".into())),
                    cmp: CmpOp::Eq,
                },
                PlanOp::ShortestPath {
                    src: "a".into(),
                    dst: "d".into(),
                    edge: "e".into(),
                    path_var: Some("p".into()),
                    mode: ShortestMode::AllShortest,
                    direction: EdgeDirection::PointingRight,
                    label: Some("KNOWS".into()),
                    label_expr: None,
                    var_len: Some(VarLenSpec {
                        min: 2,
                        max: Some(2),
                    }),
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(result.rows.len(), 2, "result={:?}", result.rows);
        let got_paths = result
            .rows
            .iter()
            .filter_map(|r| r.get("p").cloned())
            .collect::<Vec<_>>();
        assert!(got_paths.contains(&expected_b1), "missing b1 path");
        assert!(got_paths.contains(&expected_b2), "missing b2 path");
    }

    #[test]
    #[ignore = "legacy in-memory backend test; covered by rewrite overlay path"]
    fn shortest_path_path_var_shortest_k() {
        use gleaph_gql_planner::plan::{ShortestMode, VarLenSpec};

        let mut graph = InMemoryGraph::new();
        let a = graph.insert_node(["U"], [("name", Value::Text("a".into()))]);
        let b1 = graph.insert_node(["U"], [("name", Value::Text("b1".into()))]);
        let b2 = graph.insert_node(["U"], [("name", Value::Text("b2".into()))]);
        let d = graph.insert_node(["U"], [("name", Value::Text("d".into()))]);

        graph.insert_edge(a, b1, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(a, b2, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(b1, d, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(b2, d, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let expected_b1 = Value::Path(vec![
            PathElement::Vertex(a.into()),
            PathElement::Edge {
                src: a.into(),
                dst: b1.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(b1.into()),
            PathElement::Edge {
                src: b1.into(),
                dst: d.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(d.into()),
        ]);
        let expected_b2 = Value::Path(vec![
            PathElement::Vertex(a.into()),
            PathElement::Edge {
                src: a.into(),
                dst: b2.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(b2.into()),
            PathElement::Edge {
                src: b2.into(),
                dst: d.into(),
                label: Some("KNOWS".into()),
            },
            PathElement::Vertex(d.into()),
        ]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::IndexScan {
                    property_projection: None,
                    variable: "a".into(),
                    property: "name".into(),
                    value: ScanValue::Literal(Value::Text("a".into())),
                    cmp: CmpOp::Eq,
                },
                PlanOp::ShortestPath {
                    src: "a".into(),
                    dst: "d".into(),
                    edge: "e".into(),
                    path_var: Some("p".into()),
                    mode: ShortestMode::ShortestK(1),
                    direction: EdgeDirection::PointingRight,
                    label: Some("KNOWS".into()),
                    label_expr: None,
                    var_len: Some(VarLenSpec {
                        min: 2,
                        max: Some(2),
                    }),
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        let result = execute_plan(&mut graph, &plan).expect("execute");
        assert_eq!(result.rows.len(), 1, "result={:?}", result.rows);
        let got = result
            .rows
            .first()
            .and_then(|r| r.get("p"))
            .cloned()
            .expect("path var present");
        assert!(
            got == expected_b1 || got == expected_b2,
            "expected one shortest path, got={got:?}"
        );
    }
}
