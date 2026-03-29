use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::rc::Rc;

use gleaph_gql::ast::{CmpOp, Expr, ExprKind, NullOrder, OrderByClause, SortDirection};
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql::Value;
use gleaph_gql_planner::plan::{
    AggregateSpec, ConditionalScanCandidate, ProjectColumn, RemovePlanItem, ScanValue,
    SetPlanItem,
};
use gleaph_gql_planner::{PhysicalPlan, PlanOp};
use gleaph_graph_kernel::{EdgeRecord, GraphError, GraphRead, GraphWrite, NodeRecord, PropertyMap};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub enum BindingValue {
    Scalar(Value),
    Node(NodeRecord),
    Edge(EdgeRecord),
}

pub type BindingRow = BTreeMap<Rc<str>, BindingValue>;
pub type OutputRow = BTreeMap<String, Value>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecutionContext {
    pub params: BTreeMap<String, Value>,
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

pub type ExecutionResultExt<T> = Result<T, ExecutionError>;

fn lookup_param(ctx: &ExecutionContext, name: &str) -> Value {
    if let Some(value) = ctx.params.get(name) {
        return value.clone();
    }
    if let Some(stripped) = name.strip_prefix('$')
        && let Some(value) = ctx.params.get(stripped) {
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
    if let Some(error) = plan.diagnostics.dml_errors.first() {
        return Err(ExecutionError::InvalidPlan(format!(
            "[{}] at {}..{}: {}",
            error.code, error.span.start, error.span.end, error.message
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
        .collect();
    let summary = ExecutionSummary::from_result(&rows, &warnings, plan);

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
    execute_ops_from_rows(graph, ops, ctx, vec![BindingRow::new()])
}

fn execute_ops_from_rows<G: GraphRead + GraphWrite>(
    graph: &mut G,
    ops: &[PlanOp],
    ctx: &ExecutionContext,
    initial_rows: Vec<BindingRow>,
) -> ExecutionResultExt<(Vec<BindingRow>, Option<Vec<OutputRow>>)> {
    let mut rows = initial_rows;
    let mut projected = None;

    for op in ops {
        match op {
            PlanOp::NodeScan { variable, label } => {
                rows = exec_node_scan(graph, &rows, variable.as_ref(), label.as_deref())?;
            }
            PlanOp::IndexScan {
                variable,
                property,
                value,
                cmp,
            } => {
                rows = exec_index_scan(
                    graph,
                    &rows,
                    variable.as_ref(),
                    property.as_ref(),
                    value,
                    *cmp,
                    ctx,
                )?;
            }
            PlanOp::EdgeIndexScan {
                variable,
                property,
                value,
            } => {
                rows = exec_edge_index_scan(
                    graph,
                    &rows,
                    variable.as_ref(),
                    property.as_ref(),
                    value,
                    ctx,
                )?;
            }
            PlanOp::ConditionalIndexScan {
                candidates,
                fallback_label,
                fallback_variable,
            } => {
                rows = exec_conditional_index_scan(
                    graph,
                    &rows,
                    candidates,
                    fallback_label.as_deref(),
                    fallback_variable.as_ref(),
                    ctx,
                )?;
            }
            PlanOp::PropertyFilter { predicates, .. } => {
                rows = exec_property_filter(graph, rows, predicates, ctx)?;
            }
            PlanOp::Expand {
                src,
                edge,
                dst,
                direction,
                label,
                var_len,
            } => {
                if var_len.is_some() {
                    return Err(ExecutionError::UnsupportedPlanOp("Expand.var_len"));
                }
                rows = exec_expand(
                    graph,
                    rows,
                    src.as_ref(),
                    edge.as_ref(),
                    dst.as_ref(),
                    *direction,
                    label.as_deref(),
                )?;
            }
            PlanOp::ExpandFilter {
                src,
                edge,
                dst,
                direction,
                label,
                var_len,
                dst_filter,
            } => {
                if var_len.is_some() {
                    return Err(ExecutionError::UnsupportedPlanOp("ExpandFilter.var_len"));
                }
                rows = exec_expand_filter(
                    graph,
                    rows,
                    src.as_ref(),
                    edge.as_ref(),
                    dst.as_ref(),
                    *direction,
                    label.as_deref(),
                    dst_filter,
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
                rows = exec_insert_vertex(graph, rows, variable.as_deref(), labels, properties, ctx)?;
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
                    variable.as_deref(),
                    src.as_ref(),
                    dst.as_ref(),
                    labels,
                    properties,
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
                let right_result = execute_plan_with_context(graph, right, ctx)?;
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
            _ => return Err(unsupported_op_name(op)),
        }
    }

    Ok((rows, projected))
}

fn exec_node_scan<G: GraphRead>(
    graph: &G,
    input: &[BindingRow],
    variable: &str,
    label: Option<&str>,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let nodes = graph.scan_nodes(label)?;
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
        let node = graph.insert_node(
            &labels,
            &property_map,
        )?;
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
    variable: Option<&str>,
    src: &str,
    dst: &str,
    labels: &[Rc<str>],
    properties: &[gleaph_gql_planner::plan::PropertyAssignment],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::with_capacity(input.len());
    for row in input {
        let src_id = match row.get(src) {
            Some(BindingValue::Node(node)) => node.id,
            Some(_) => return Err(ExecutionError::TypeMismatch("insert edge src must be a node")),
            None => return Err(ExecutionError::MissingBinding(src.to_owned())),
        };
        let dst_id = match row.get(dst) {
            Some(BindingValue::Node(node)) => node.id,
            Some(_) => return Err(ExecutionError::TypeMismatch("insert edge dst must be a node")),
            None => return Err(ExecutionError::MissingBinding(dst.to_owned())),
        };
        let property_map = eval_property_assignments(graph, &row, properties, ctx)?;
        let edge_label = labels.first().map(|label| label.as_ref());
        let edge = graph.insert_edge(
            src_id,
            dst_id,
            edge_label,
            &property_map,
        )?;
        let mut next = row;
        if let Some(variable) = variable {
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
                    return Err(ExecutionError::UnsupportedPlanOp("SetProperties.AllProperties"));
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
                RemovePlanItem::Property { variable, property } => match row.get(variable.as_ref()) {
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
                _ => return Err(ExecutionError::TypeMismatch("filter predicate must be boolean")),
            }
        }
        out.push(row);
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
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let value = resolve_scan_value(value, ctx)?;
    let nodes = graph.scan_nodes_by_property(property, &value, cmp)?;
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
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let value = resolve_scan_value(value, ctx)?;
    let edges = graph.scan_edges_by_property(property, &value)?;
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

fn exec_conditional_index_scan<G: GraphRead>(
    graph: &G,
    input: &[BindingRow],
    candidates: &[ConditionalScanCandidate],
    fallback_label: Option<&str>,
    fallback_variable: &str,
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
                    ctx,
                );
            }
        }
    }

    exec_node_scan(graph, input, fallback_variable, fallback_label)
}

fn exec_expand<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    src: &str,
    edge: &str,
    dst: &str,
    direction: gleaph_gql::types::EdgeDirection,
    label: Option<&str>,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut out = Vec::new();
    for row in input {
        let src_node = match row.get(src) {
            Some(BindingValue::Node(node)) => node,
            Some(_) => return Err(ExecutionError::TypeMismatch("expand source must be a node")),
            None => return Err(ExecutionError::MissingBinding(src.to_owned())),
        };

        for expansion in graph.expand(src_node.id, direction, label)? {
            let mut next = row.clone();
            next.insert(Rc::<str>::from(edge), BindingValue::Edge(expansion.edge));
            next.insert(Rc::<str>::from(dst), BindingValue::Node(expansion.node));
            out.push(next);
        }
    }
    Ok(out)
}

fn exec_expand_filter<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    src: &str,
    edge: &str,
    dst: &str,
    direction: gleaph_gql::types::EdgeDirection,
    label: Option<&str>,
    dst_filter: &[Expr],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let expanded = exec_expand(graph, input, src, edge, dst, direction, label)?;
    exec_property_filter(graph, expanded, dst_filter, ctx)
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
        return Err(ExecutionError::UnsupportedPlanOp("HashJoin.projected_subplan"));
    }

    let mut buckets: Vec<(Vec<Value>, Vec<BindingRow>)> = Vec::new();
    for row in left_rows {
        let key = join_key_values(&row, join_keys)?;
        if let Some((_, rows)) = buckets.iter_mut().find(|(bucket_key, _)| *bucket_key == key) {
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
        return Err(ExecutionError::UnsupportedPlanOp("CartesianProduct.projected_subplan"));
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
        let (sub_rows, sub_projected) =
            execute_ops_from_rows(graph, sub_plan, ctx, vec![row.clone()])?;
        if sub_projected.is_some() {
            return Err(ExecutionError::UnsupportedPlanOp("OptionalMatch.projected_subplan"));
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
            if left.is_empty() { right } else { left }
        }
    }
}

fn materialize_output_rows(rows: Vec<OutputRow>, columns: &[ProjectColumn]) -> Vec<BindingRow> {
    rows.into_iter()
        .map(|row| {
            let mut out = BindingRow::new();
            if columns.is_empty() {
                for (key, value) in row {
                    out.insert(Rc::<str>::from(key), BindingValue::Scalar(value));
                }
            } else {
                for (index, column) in columns.iter().enumerate() {
                    let key = materialize_column_name(column, index);
                    let value = row.get(&key).cloned().unwrap_or(Value::Null);
                    out.insert(Rc::<str>::from(key), BindingValue::Scalar(value));
                }
            }
            out
        })
        .collect()
}

fn exec_aggregate<G: GraphRead>(
    graph: &G,
    input: Vec<BindingRow>,
    group_by: &[Expr],
    aggregates: &[AggregateSpec],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let mut groups: Vec<GroupBucket> = Vec::new();

    for row in input {
        let key: Vec<Value> = group_by
            .iter()
            .map(|expr| eval_expr(graph, &row, expr, ctx))
            .collect::<Result<_, _>>()?;

        let idx = groups
            .iter()
            .position(|bucket| bucket.key == key)
            .unwrap_or_else(|| {
                groups.push(GroupBucket::new(key.clone(), row.clone(), aggregates.len()));
                groups.len() - 1
            });

        let bucket = &mut groups[idx];
        for (agg_idx, spec) in aggregates.iter().enumerate() {
            update_aggregate_state(graph, &row, spec, &mut bucket.aggregate_states[agg_idx], ctx)?;
        }
    }

    let mut out = Vec::with_capacity(groups.len());
    for mut bucket in groups {
        for (agg_idx, spec) in aggregates.iter().enumerate() {
            let key = aggregate_binding_name(spec, agg_idx);
            bucket
                .sample_row
                .insert(Rc::<str>::from(key), BindingValue::Scalar(finalize_aggregate_state(&bucket.aggregate_states[agg_idx])));
        }
        out.push(bucket.sample_row);
    }
    Ok(out)
}

fn apply_limit(
    rows: &mut Vec<OutputRow>,
    count: Option<&Expr>,
    offset: Option<&Expr>,
) -> ExecutionResultExt<()> {
    let offset = eval_usize_expr(offset)?.unwrap_or(0);
    let count = eval_usize_expr(count)?;
    let truncated: Vec<_> = rows
        .iter()
        .skip(offset)
        .take(count.unwrap_or(usize::MAX))
        .cloned()
        .collect();
    *rows = truncated;
    Ok(())
}

fn apply_limit_bindings(
    rows: Vec<BindingRow>,
    count: Option<&Expr>,
    offset: Option<&Expr>,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let offset = eval_usize_expr(offset)?.unwrap_or(0);
    let count = eval_usize_expr(count)?;
    Ok(rows
        .into_iter()
        .skip(offset)
        .take(count.unwrap_or(usize::MAX))
        .collect())
}

fn dedup_output_rows(rows: &mut Vec<OutputRow>) {
    let mut deduped = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if !deduped.contains(&row) {
            deduped.push(row);
        }
    }
    *rows = deduped;
}

fn dedup_output_rows_owned(rows: Vec<OutputRow>) -> Vec<OutputRow> {
    let mut rows = rows;
    dedup_output_rows(&mut rows);
    rows
}

fn dedup_binding_rows(rows: &mut Vec<BindingRow>) {
    let mut deduped = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if !deduped.contains(&row) {
            deduped.push(row);
        }
    }
    *rows = deduped;
}

fn normalize_to_output_rows<G: GraphRead>(
    graph: &G,
    rows: Vec<BindingRow>,
    projected: Option<Vec<OutputRow>>,
) -> ExecutionResultExt<Vec<OutputRow>> {
    match projected {
        Some(rows) => Ok(rows),
        None => rows
            .into_iter()
            .map(|row| materialize_row(graph, &row))
            .collect(),
    }
}

fn join_key_values(row: &BindingRow, join_keys: &[Rc<str>]) -> ExecutionResultExt<Vec<Value>> {
    join_keys
        .iter()
        .map(|key| match row.get(key.as_ref()) {
            Some(BindingValue::Scalar(value)) => Ok(value.clone()),
            Some(BindingValue::Node(node)) => Ok(node_to_value(node)),
            Some(BindingValue::Edge(edge)) => Ok(edge_to_value(edge)),
            None => Err(ExecutionError::MissingBinding(key.to_string())),
        })
        .collect()
}

fn merge_rows(left: &BindingRow, right: &BindingRow) -> ExecutionResultExt<BindingRow> {
    let mut merged = left.clone();
    for (key, value) in right {
        if let Some(existing) = merged.get(key.as_ref()) {
            if existing != value {
                return Err(ExecutionError::TypeMismatch("conflicting join bindings"));
            }
            continue;
        }
        merged.insert(key.clone(), value.clone());
    }
    Ok(merged)
}

fn subtract_rows(left: Vec<OutputRow>, right: Vec<OutputRow>, distinct: bool) -> Vec<OutputRow> {
    let mut out = if distinct { dedup_output_rows_owned(left) } else { left };
    let rhs = if distinct { dedup_output_rows_owned(right) } else { right };
    for row in rhs {
        if let Some(pos) = out.iter().position(|candidate| *candidate == row) {
            out.remove(pos);
        }
    }
    out
}

fn intersect_rows(left: Vec<OutputRow>, right: Vec<OutputRow>, distinct: bool) -> Vec<OutputRow> {
    let mut lhs = if distinct { dedup_output_rows_owned(left) } else { left };
    let mut rhs = if distinct { dedup_output_rows_owned(right) } else { right };
    let mut out = Vec::new();

    while let Some(row) = lhs.pop() {
        if let Some(pos) = rhs.iter().position(|candidate| *candidate == row) {
            rhs.remove(pos);
            out.push(row);
        }
    }

    if distinct {
        dedup_output_rows_owned(out)
    } else {
        out
    }
}

fn materialize_column_name(column: &ProjectColumn, index: usize) -> String {
    if let Some(alias) = &column.alias {
        return alias.as_ref().to_owned();
    }
    match &column.expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::PropertyAccess { property, .. } => property.clone(),
        ExprKind::Aggregate { func, expr, .. } => aggregate_expr_binding_name(func, expr.as_deref(), index),
        _ => format!("col_{index}"),
    }
}

fn collect_produced_vars(ops: &[PlanOp]) -> Vec<Rc<str>> {
    let mut vars: Vec<Rc<str>> = Vec::new();
    for op in ops {
        match op {
            PlanOp::NodeScan { variable, .. }
            | PlanOp::IndexScan { variable, .. }
            | PlanOp::EdgeIndexScan { variable, .. } => push_var(&mut vars, variable),
            PlanOp::ConditionalIndexScan {
                fallback_variable, ..
            } => push_var(&mut vars, fallback_variable),
            PlanOp::Expand { src, edge, dst, .. }
            | PlanOp::ExpandFilter { src, edge, dst, .. } => {
                push_var(&mut vars, src);
                push_var(&mut vars, edge);
                push_var(&mut vars, dst);
            }
            PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
                for var in collect_produced_vars(left) {
                    push_var(&mut vars, &var);
                }
                for var in collect_produced_vars(right) {
                    push_var(&mut vars, &var);
                }
            }
            PlanOp::OptionalMatch { sub_plan } => {
                for var in collect_produced_vars(sub_plan) {
                    push_var(&mut vars, &var);
                }
            }
            PlanOp::Materialize { columns, .. } => {
                for (index, column) in columns.iter().enumerate() {
                    let name = Rc::<str>::from(materialize_column_name(column, index));
                    push_var(&mut vars, &name);
                }
            }
            _ => {}
        }
    }
    vars
}

fn push_var(vars: &mut Vec<Rc<str>>, var: &Rc<str>) {
    if !vars.iter().any(|existing| existing == var) {
        vars.push(var.clone());
    }
}

fn sort_binding_rows<G: GraphRead>(
    graph: &G,
    rows: &mut [BindingRow],
    order_by: &OrderByClause,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<()> {
    rows.sort_by(|left, right| compare_binding_rows(graph, left, right, order_by, ctx));
    Ok(())
}

fn sort_output_rows(
    rows: &mut [OutputRow],
    order_by: &OrderByClause,
) -> ExecutionResultExt<()> {
    rows.sort_by(|left, right| compare_output_rows(left, right, order_by));
    Ok(())
}

fn eval_usize_expr(expr: Option<&Expr>) -> ExecutionResultExt<Option<usize>> {
    match expr {
        None => Ok(None),
        Some(Expr {
            kind: ExprKind::Literal(value),
            ..
        }) => match value {
            Value::Int8(v) if *v >= 0 => Ok(Some(*v as usize)),
            Value::Int16(v) if *v >= 0 => Ok(Some(*v as usize)),
            Value::Int32(v) if *v >= 0 => Ok(Some(*v as usize)),
            Value::Int64(v) if *v >= 0 => Ok(Some(*v as usize)),
            Value::Uint8(v) => Ok(Some(*v as usize)),
            Value::Uint16(v) => Ok(Some(*v as usize)),
            Value::Uint32(v) => Ok(Some(*v as usize)),
            Value::Uint64(v) => usize::try_from(*v)
                .map(Some)
                .map_err(|_| ExecutionError::InvalidLimit),
            _ => Err(ExecutionError::InvalidLimit),
        },
        Some(_) => Err(ExecutionError::InvalidLimit),
    }
}

fn eval_expr<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    expr: &Expr,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Value> {
    match &expr.kind {
        ExprKind::Literal(value) => Ok(value.clone()),
        ExprKind::Parameter(name) => Ok(lookup_param(ctx, name)),
        ExprKind::Variable(name) => binding_to_value(graph, row, name),
        ExprKind::PropertyAccess { expr, property } => {
            eval_property_access(graph, row, expr, property, ctx)
        }
        ExprKind::Compare { left, op, right } => {
            let left = eval_expr(graph, row, left, ctx)?;
            let right = eval_expr(graph, row, right, ctx)?;
            let ord = compare_values(&left, &right);
            Ok(Value::Bool(apply_cmp(*op, ord)))
        }
        ExprKind::And(left, right) => {
            let left = eval_expr(graph, row, left, ctx)?;
            let right = eval_expr(graph, row, right, ctx)?;
            Ok(Value::Bool(expect_bool(left)? && expect_bool(right)?))
        }
        ExprKind::Or(left, right) => {
            let left = eval_expr(graph, row, left, ctx)?;
            let right = eval_expr(graph, row, right, ctx)?;
            Ok(Value::Bool(expect_bool(left)? || expect_bool(right)?))
        }
        ExprKind::Not(expr) => {
            let value = eval_expr(graph, row, expr, ctx)?;
            Ok(Value::Bool(!expect_bool(value)?))
        }
        ExprKind::IsNull(expr) => Ok(Value::Bool(matches!(
            eval_expr(graph, row, expr, ctx)?,
            Value::Null
        ))),
        ExprKind::IsNotNull(expr) => {
            Ok(Value::Bool(!matches!(
                eval_expr(graph, row, expr, ctx)?,
                Value::Null
            )))
        }
        ExprKind::Paren(expr) => eval_expr(graph, row, expr, ctx),
        _ => Err(ExecutionError::UnsupportedExpr("expression kind")),
    }
}

fn eval_project_expr<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    column: &ProjectColumn,
    index: usize,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Value> {
    if let ExprKind::Aggregate { func, expr, .. } = &column.expr.kind {
        if let Some(alias) = &column.alias
            && let Some(BindingValue::Scalar(value)) = row.get(alias.as_ref())
        {
            return Ok(value.clone());
        }
        let key = aggregate_expr_binding_name(func, expr.as_deref(), index);
        if let Some(BindingValue::Scalar(value)) = row.get(key.as_str()) {
            return Ok(value.clone());
        }
    }
    eval_expr(graph, row, &column.expr, ctx)
}

fn eval_property_assignments<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    properties: &[gleaph_gql_planner::plan::PropertyAssignment],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<PropertyMap> {
    let mut map = PropertyMap::new();
    for assignment in properties {
        let value = eval_expr(graph, row, &assignment.value, ctx)?;
        map.insert(assignment.name.to_string(), value);
    }
    Ok(map)
}

fn eval_materialize_expr<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    column: &ProjectColumn,
    index: usize,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<BindingValue> {
    match &column.expr.kind {
        ExprKind::Variable(name) => row
            .get(name.as_str())
            .cloned()
            .ok_or_else(|| ExecutionError::MissingBinding(name.clone())),
        ExprKind::Aggregate { func, expr, .. } => {
            if let Some(alias) = &column.alias
                && let Some(value) = row.get(alias.as_ref()).cloned()
            {
                return Ok(value);
            }
            let key = aggregate_expr_binding_name(func, expr.as_deref(), index);
            row.get(key.as_str())
                .cloned()
                .ok_or_else(|| ExecutionError::MissingBinding(key))
        }
        _ => Ok(BindingValue::Scalar(eval_project_expr(
            graph, row, column, index, ctx,
        )?)),
    }
}

fn eval_output_expr(row: &OutputRow, expr: &Expr) -> ExecutionResultExt<Value> {
    match &expr.kind {
        ExprKind::Literal(value) => Ok(value.clone()),
        ExprKind::Variable(name) => Ok(row.get(name).cloned().unwrap_or(Value::Null)),
        ExprKind::Aggregate { func, expr, .. } => {
            let key = aggregate_expr_binding_name(func, expr.as_deref(), 0);
            Ok(row.get(&key).cloned().unwrap_or(Value::Null))
        }
        ExprKind::Paren(expr) => eval_output_expr(row, expr),
        _ => Err(ExecutionError::UnsupportedExpr(
            "output sort expression kind",
        )),
    }
}

fn eval_property_access<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    expr: &Expr,
    property: &str,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Value> {
    match &expr.kind {
        ExprKind::Variable(name) => match row.get(name.as_str()) {
            Some(BindingValue::Node(node)) => Ok(node
                .properties
                .get(property)
                .cloned()
                .unwrap_or(Value::Null)),
            Some(BindingValue::Edge(edge)) => Ok(edge
                .properties
                .get(property)
                .cloned()
                .unwrap_or(Value::Null)),
            Some(BindingValue::Scalar(Value::Null)) => Ok(Value::Null),
            Some(BindingValue::Scalar(Value::Record(fields))) => Ok(fields
                .iter()
                .find(|(name, _)| name == property)
                .map(|(_, value)| value.clone())
                .unwrap_or(Value::Null)),
            Some(BindingValue::Scalar(_)) => Err(ExecutionError::TypeMismatch(
                "property access requires node, edge, or record",
            )),
            None => Err(ExecutionError::MissingBinding(name.clone())),
        },
        _ => {
            let base = eval_expr(graph, row, expr, ctx)?;
            match base {
                Value::Record(fields) => Ok(fields
                    .into_iter()
                    .find(|(name, _)| name == property)
                    .map(|(_, value)| value)
                    .unwrap_or(Value::Null)),
                _ => Err(ExecutionError::TypeMismatch(
                    "property access requires node, edge, or record",
                )),
            }
        }
    }
}

fn resolve_scan_value(
    value: &ScanValue,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Value> {
    match value {
        ScanValue::Literal(value) => Ok(value.clone()),
        ScanValue::Parameter(name) => Ok(lookup_param(ctx, name.as_ref())),
    }
}

fn compare_binding_rows<G: GraphRead>(
    graph: &G,
    left: &BindingRow,
    right: &BindingRow,
    order_by: &OrderByClause,
    ctx: &ExecutionContext,
) -> Ordering {
    for item in &order_by.items {
        let left_value = match eval_expr(graph, left, &item.expr, ctx) {
            Ok(value) => value,
            Err(_) => return Ordering::Equal,
        };
        let right_value = match eval_expr(graph, right, &item.expr, ctx) {
            Ok(value) => value,
            Err(_) => return Ordering::Equal,
        };
        let ord = compare_sort_values(
            &left_value,
            &right_value,
            item.direction,
            item.null_order,
        );
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn compare_output_rows(
    left: &OutputRow,
    right: &OutputRow,
    order_by: &OrderByClause,
) -> Ordering {
    for item in &order_by.items {
        let left_value = match eval_output_expr(left, &item.expr) {
            Ok(value) => value,
            Err(_) => return Ordering::Equal,
        };
        let right_value = match eval_output_expr(right, &item.expr) {
            Ok(value) => value,
            Err(_) => return Ordering::Equal,
        };
        let ord = compare_sort_values(
            &left_value,
            &right_value,
            item.direction,
            item.null_order,
        );
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn compare_sort_values(
    left: &Value,
    right: &Value,
    direction: Option<SortDirection>,
    null_order: Option<NullOrder>,
) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => return Ordering::Equal,
        (Value::Null, _) => {
            return match null_order.unwrap_or(NullOrder::Last) {
                NullOrder::First => Ordering::Less,
                NullOrder::Last => Ordering::Greater,
            };
        }
        (_, Value::Null) => {
            return match null_order.unwrap_or(NullOrder::Last) {
                NullOrder::First => Ordering::Greater,
                NullOrder::Last => Ordering::Less,
            };
        }
        _ => {}
    }

    let ord = compare_values(left, right).unwrap_or(Ordering::Equal);
    match direction.unwrap_or(SortDirection::Asc) {
        SortDirection::Asc | SortDirection::Ascending => ord,
        SortDirection::Desc | SortDirection::Descending => ord.reverse(),
    }
}

fn binding_to_value<G: GraphRead>(
    _graph: &G,
    row: &BindingRow,
    name: &str,
) -> ExecutionResultExt<Value> {
    match row.get(name) {
        Some(BindingValue::Scalar(value)) => Ok(value.clone()),
        Some(BindingValue::Node(node)) => Ok(node_to_value(node)),
        Some(BindingValue::Edge(edge)) => Ok(edge_to_value(edge)),
        None => Err(ExecutionError::MissingBinding(name.to_owned())),
    }
}

fn aggregate_binding_name(spec: &AggregateSpec, index: usize) -> String {
    if let Some(alias) = &spec.alias {
        alias.as_ref().to_owned()
    } else {
        aggregate_expr_binding_name_from_spec(spec, index)
    }
}

fn aggregate_expr_binding_name_from_spec(spec: &AggregateSpec, index: usize) -> String {
    let suffix = match &spec.expr {
        Some(expr) => format!(":{expr:?}"),
        None => ":*".to_owned(),
    };
    format!("__agg_{index}_{}{}", spec.func, suffix)
}

fn aggregate_expr_binding_name(
    func: &gleaph_gql::ast::AggregateFunc,
    expr: Option<&Expr>,
    index: usize,
) -> String {
    let suffix = match expr {
        Some(expr) => format!(":{expr:?}"),
        None => ":*".to_owned(),
    };
    format!("__agg_{index}_{func:?}{suffix}")
}

fn materialize_row<G: GraphRead>(_graph: &G, row: &BindingRow) -> ExecutionResultExt<OutputRow> {
    row.iter()
        .map(|(key, value)| {
            let value = match value {
                BindingValue::Scalar(value) => value.clone(),
                BindingValue::Node(node) => node_to_value(node),
                BindingValue::Edge(edge) => edge_to_value(edge),
            };
            Ok((key.to_string(), value))
        })
        .collect()
}

fn node_to_value(node: &NodeRecord) -> Value {
    let mut fields = Vec::with_capacity(node.properties.len() + 3);
    fields.push(("id".to_owned(), Value::Uint64(node.id.into())));
    fields.push((
        "labels".to_owned(),
        Value::List(node.labels.iter().cloned().map(Value::Text).collect()),
    ));
    for (key, value) in &node.properties {
        fields.push((key.clone(), value.clone()));
    }
    Value::Record(fields)
}

fn edge_to_value(edge: &EdgeRecord) -> Value {
    let mut fields = Vec::with_capacity(edge.properties.len() + 4);
    fields.push(("id".to_owned(), Value::Uint64(edge.id)));
    fields.push(("src".to_owned(), Value::Uint64(edge.src.into())));
    fields.push(("dst".to_owned(), Value::Uint64(edge.dst.into())));
    if let Some(label) = &edge.label {
        fields.push(("label".to_owned(), Value::Text(label.clone())));
    }
    for (key, value) in &edge.properties {
        fields.push((key.clone(), value.clone()));
    }
    Value::Record(fields)
}

fn apply_cmp(op: CmpOp, ordering: Option<Ordering>) -> bool {
    match op {
        CmpOp::Eq => ordering == Some(Ordering::Equal),
        CmpOp::Ne => ordering != Some(Ordering::Equal),
        CmpOp::Lt => ordering == Some(Ordering::Less),
        CmpOp::Le => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
        CmpOp::Gt => ordering == Some(Ordering::Greater),
        CmpOp::Ge => matches!(ordering, Some(Ordering::Greater | Ordering::Equal)),
    }
}

fn expect_bool(value: Value) -> ExecutionResultExt<bool> {
    match value {
        Value::Bool(value) => Ok(value),
        _ => Err(ExecutionError::TypeMismatch("expected boolean")),
    }
}

fn column_name(column: &ProjectColumn) -> String {
    if let Some(alias) = &column.alias {
        return alias.as_ref().to_owned();
    }
    match &column.expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::PropertyAccess { property, .. } => property.clone(),
        _ => "expr".to_owned(),
    }
}

fn unsupported_op_name(op: &PlanOp) -> ExecutionError {
    let name = match op {
        PlanOp::IndexScan { .. }
        | PlanOp::EdgeIndexScan { .. }
        | PlanOp::ConditionalIndexScan { .. } => "reachable-supported-op",
        PlanOp::ExpandFilter { .. } => "reachable-supported-op",
        PlanOp::ShortestPath { .. } => "ShortestPath",
        PlanOp::Let { .. } => "Let",
        PlanOp::For { .. } => "For",
        PlanOp::Filter { .. } => "Filter",
        PlanOp::CallProcedure { .. } => "CallProcedure",
        PlanOp::InlineProcedureCall { .. } => "InlineProcedureCall",
        PlanOp::UseGraph { .. } => "UseGraph",
        PlanOp::HashJoin { .. } | PlanOp::CartesianProduct { .. } => "reachable-supported-op",
        PlanOp::Aggregate { .. } => "reachable-supported-op",
        PlanOp::Sort { .. } => "reachable-supported-op",
        PlanOp::SetOperation { .. } => "reachable-supported-op",
        PlanOp::OptionalMatch { .. } => "reachable-supported-op",
        PlanOp::IndexIntersection { .. } => "IndexIntersection",
        PlanOp::WorstCaseOptimalJoin { .. } => "WorstCaseOptimalJoin",
        PlanOp::TopK { .. } => "reachable-supported-op",
        PlanOp::Materialize { .. } => "reachable-supported-op",
        PlanOp::InsertVertex { .. } => "InsertVertex",
        PlanOp::InsertEdge { .. } => "InsertEdge",
        PlanOp::SetProperties { .. } => "SetProperties",
        PlanOp::RemoveProperties { .. } => "RemoveProperties",
        PlanOp::DeleteVertex { .. } => "DeleteVertex",
        PlanOp::DetachDeleteVertex { .. } => "DetachDeleteVertex",
        PlanOp::DeleteEdge { .. } => "DeleteEdge",
        PlanOp::NodeScan { .. }
        | PlanOp::PropertyFilter { .. }
        | PlanOp::Expand { .. }
        | PlanOp::Project { .. }
        | PlanOp::Limit { .. } => "reachable-supported-op",
    };
    ExecutionError::UnsupportedPlanOp(name)
}

#[cfg(test)]
mod tests {
    use gleaph_gql::ast::{
        CmpOp, Expr, ExprKind, NullOrder, OrderByClause, SortDirection, SortItem,
    };
    use gleaph_gql::token::Span;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql::Value;
    use gleaph_gql_planner::plan::{
        AggregateSpec, ConditionalScanCandidate, ProjectColumn, RemovePlanItem, ScanValue,
        SetPlanItem,
    };
    use gleaph_gql_planner::{PhysicalPlan, PlanAnnotations, PlanOp};
    use gleaph_graph_kernel::GraphRead;
    use gleaph_graph_mem::InMemoryGraph;
    use gleaph_graph_pma::{
        GraphMutationPath, GraphPma, project_overlay_write_event,
        PropertyIndexNodeStoreMutationKind, RewriteOverlayEdgeMutationKind,
        RewriteOverlayWriteEvent, RewriteWriteEventProjection, VecMemory,
    };

    use super::{
        ExecutionContext, ExecutionError, OutputRow, exec_set_operation, execute_plan,
        execute_plan_with_context,
    };
    use self::backend_debug_helpers::{expect_graph_execution, expect_rewrite_overlay_execution};
    use self::overlay_test_helpers::{
        assert_last_projected_event, bootstrap_empty_rewrite_harness,
        bootstrap_rewrite_overlay_authored_and_liked_posts, bootstrap_rewrite_overlay_user_post_authored,
        bootstrap_rewrite_overlay_user_uid, projected_history,
    };
    use self::seed_helpers::{
        rewrite_seed_authored_and_liked_posts, rewrite_seed_user_post_authored,
        rewrite_seed_user_uid,
    };

    const DEBUG_NODE_PROPERTY_KEYS: &[&str] = &["uid", "title", "name"];
    const DEBUG_EDGE_PROPERTY_KEYS: &[&str] = &["weight", "score"];

    mod overlay_test_helpers {
        use gleaph_graph_pma::{
            KernelBootstrapGraphSpec, RewriteDiagnosticsView, RewriteGraphPmaKernelHarness,
            RewriteGraphPmaKernelOverlay, RewriteOverlayWriteEvent, RewriteVecMemory,
            RewriteWriteEventProjection, last_projected_overlay_event,
            project_overlay_write_history,
        };

        pub(super) fn bootstrap_empty_rewrite_harness(
        ) -> RewriteGraphPmaKernelHarness<RewriteVecMemory> {
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
            gleaph_graph_pma::KernelBootstrapGraphSummary,
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
            gleaph_graph_pma::KernelBootstrapGraphSummary,
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
            gleaph_graph_pma::KernelBootstrapGraphSummary,
        ) {
            let spec = super::rewrite_seed_user_post_authored(uid, title, weight);
            bootstrap_rewrite_overlay(harness, &spec)
        }

        pub(super) fn bootstrap_rewrite_overlay_authored_and_liked_posts<'a>(
            harness: &'a mut RewriteGraphPmaKernelHarness<RewriteVecMemory>,
        ) -> (
            RewriteGraphPmaKernelOverlay<'a, RewriteVecMemory>,
            gleaph_graph_pma::KernelBootstrapGraphSummary,
        ) {
            let spec = super::rewrite_seed_authored_and_liked_posts();
            bootstrap_rewrite_overlay(harness, &spec)
        }

        fn assert_overlay_bootstrap_projection_matches_event(
            graph: &RewriteGraphPmaKernelOverlay<'_, RewriteVecMemory>,
            summary: &gleaph_graph_pma::KernelBootstrapGraphSummary,
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
            assert_eq!(RewriteDiagnosticsView::formatted_last_write_event(graph), Some(expected.clone()));
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
        use gleaph_graph_pma::{RewriteDiagnosticsView, RewriteGraphPmaKernelOverlay, RewriteVecMemory};

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
                let outgoing = graph.expand(node.id, EdgeDirection::PointingRight, None);
                let incoming = graph.expand(node.id, EdgeDirection::PointingLeft, None);
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

        fn collect_debug_edge_labels(
            expansions: &[gleaph_graph_kernel::Expansion],
        ) -> Vec<String> {
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
        use gleaph_graph_pma::{
            KernelBootstrapEdgeSpec, KernelBootstrapGraphSpec, KernelBootstrapNodeSpec,
        };

        pub(super) fn rewrite_seed_user_uid(uid: &str) -> KernelBootstrapGraphSpec {
            let properties: PropertyMap =
                [("uid".to_owned(), Value::Text(uid.to_owned()))].into_iter().collect();
            KernelBootstrapGraphSpec::empty()
                .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &properties))
        }

        pub(super) fn rewrite_seed_user_post_authored(
            uid: &str,
            title: &str,
            weight: i64,
        ) -> KernelBootstrapGraphSpec {
            let user_properties: PropertyMap =
                [("uid".to_owned(), Value::Text(uid.to_owned()))].into_iter().collect();
            let post_properties: PropertyMap =
                [("title".to_owned(), Value::Text(title.to_owned()))]
                    .into_iter()
                    .collect();
            let edge_properties: PropertyMap =
                [("weight".to_owned(), Value::Int64(weight))].into_iter().collect();
            KernelBootstrapGraphSpec::empty()
                .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &user_properties))
                .with_node(KernelBootstrapNodeSpec::from_parts(&["Post"], &post_properties))
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
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(
            ["User"],
            [("name", Value::Text("Alice".to_owned()))],
        );
        let bob = graph.insert_node(
            ["User"],
            [("name", Value::Text("Bob".to_owned()))],
        );
        let post = graph.insert_node(
            ["Post"],
            [("title", Value::Text("Hello".to_owned()))],
        );
        graph.insert_edge(
            alice,
            post,
            Some("AUTHORED"),
            [("since", Value::Int64(2024))],
        );
        graph.insert_edge(
            bob,
            post,
            Some("LIKED"),
            [("since", Value::Int64(2025))],
        );

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
                    var_len: None,
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

    // Rewrite overlay read-path coverage.
    #[test]
    fn executes_scan_filter_expand_project_limit_pipeline_on_rewrite_overlay() {
        let mut harness = bootstrap_empty_rewrite_harness();
        let (mut graph, _) = bootstrap_rewrite_overlay_authored_and_liked_posts(&mut harness);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
                    var_len: None,
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
    fn executes_index_scan_and_conditional_fallback() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        graph.insert_node(["User"], [("uid", Value::Text("u2".to_owned()))]);

        let indexed = PhysicalPlan {
            ops: vec![
                PlanOp::IndexScan {
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
            },
        )
        .expect("index scan should execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("uid"), Some(&Value::Text("u2".to_owned())));

        let result = execute_plan_with_context(
            &mut graph,
            &indexed,
            &ExecutionContext {
                params: [("$uid".to_owned(), Value::Text("u1".to_owned()))]
                    .into_iter()
                    .collect(),
            },
        )
        .expect("index scan should accept prefixed param key");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("uid"), Some(&Value::Text("u1".to_owned())));

        let conditional = PhysicalPlan {
            ops: vec![
                PlanOp::ConditionalIndexScan {
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

        let result = execute_plan_with_context(
            &mut graph,
            &conditional,
            &ExecutionContext::default(),
        )
        .expect("conditional scan fallback should execute");
        assert_eq!(result.rows.len(), 2);

        let result = execute_plan_with_context(
            &mut graph,
            &conditional,
            &ExecutionContext {
                params: [("$uid".to_owned(), Value::Text("u2".to_owned()))]
                    .into_iter()
                    .collect(),
            },
        )
        .expect("conditional scan should accept prefixed param key");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("uid"), Some(&Value::Text("u2".to_owned())));
    }

    #[test]
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
        graph.insert_edge(alice, p1, Some("AUTHORED"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(alice, p2, Some("AUTHORED"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(bob, p3, Some("AUTHORED"), std::iter::empty::<(&str, Value)>());

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
                    variable: "u".into(),
                    label: Some("User".into()),
                },
                PlanOp::ExpandFilter {
                    src: "u".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    var_len: None,
                    dst_filter: vec![Expr::new(ExprKind::Compare {
                        left: Box::new(Expr::new(ExprKind::PropertyAccess {
                            expr: Box::new(Expr::new(ExprKind::Variable("p".to_owned()))),
                            property: "score".to_owned(),
                        })),
                        op: CmpOp::Ge,
                        right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(15)))),
                    })],
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
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("B".to_owned())));
    }

    #[test]
    fn executes_grouped_count_aggregate() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);
        let bob = graph.insert_node(["User"], [("name", Value::Text("Bob".to_owned()))]);
        let p1 = graph.insert_node(["Post"], [("title", Value::Text("P1".to_owned()))]);
        let p2 = graph.insert_node(["Post"], [("title", Value::Text("P2".to_owned()))]);
        let p3 = graph.insert_node(["Post"], [("title", Value::Text("P3".to_owned()))]);
        graph.insert_edge(alice, p1, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(alice, p2, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(bob, p3, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("KNOWS".into()),
                    var_len: None,
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
        assert_eq!(result.rows[0].get("name"), Some(&Value::Text("Alice".to_owned())));
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
        assert_eq!(result.rows[1].get("name"), Some(&Value::Text("Bob".to_owned())));
        assert_eq!(result.rows[1].get("cnt"), Some(&Value::Int64(1)));
    }

    #[test]
    fn executes_sum_min_max_aggregate() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);
        let bob = graph.insert_node(["User"], [("name", Value::Text("Bob".to_owned()))]);
        let p1 = graph.insert_node(["Post"], [("score", Value::Int64(10))]);
        let p2 = graph.insert_node(["Post"], [("score", Value::Int64(30))]);
        let p3 = graph.insert_node(["Post"], [("score", Value::Int64(20))]);
        graph.insert_edge(alice, p1, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(alice, p2, Some("KNOWS"), std::iter::empty::<(&str, Value)>());
        graph.insert_edge(bob, p3, Some("KNOWS"), std::iter::empty::<(&str, Value)>());

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("KNOWS".into()),
                    var_len: None,
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
        assert_eq!(result.rows[0].get("name"), Some(&Value::Text("Alice".to_owned())));
        assert_eq!(result.rows[0].get("sum_score"), Some(&Value::Int64(40)));
        assert_eq!(result.rows[0].get("min_score"), Some(&Value::Int64(10)));
        assert_eq!(result.rows[0].get("max_score"), Some(&Value::Int64(30)));
    }

    #[test]
    fn executes_plan_against_graph_pma_backend() {
        let mut graph = GraphPma::init(VecMemory::new());
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("Hello".to_owned()))]);
        graph.insert_edge(alice, post, Some("AUTHORED"), [("weight", Value::Int64(10))]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::IndexScan {
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
                    var_len: None,
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
        let result = expect_graph_execution(
            &graph,
            plan_result,
            "plan should execute on graph-pma",
        );
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("uid"), Some(&Value::Text("u1".to_owned())));
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("Hello".to_owned())));
    }

    #[test]
    fn persistent_graph_debug_report_formats_representative_graph_shape() {
        let mut graph = GraphPma::init(VecMemory::new());
        let alice = graph.insert_node(
            ["User"],
            [
                ("uid", Value::Text("u1".to_owned())),
                ("name", Value::Text("Alice".to_owned())),
            ],
        );
        let post = graph.insert_node(["Post"], [("title", Value::Text("Hello".to_owned()))]);
        graph.insert_edge(alice, post, Some("AUTHORED"), [("weight", Value::Int64(10))]);

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
        let mut graph = GraphPma::init(VecMemory::new());
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);

        let insert_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("draft".to_owned())));
        assert_eq!(result.rows[0].get("weight"), Some(&Value::Int64(1)));

        let update_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    var_len: None,
                },
                PlanOp::SetProperties {
                    items: vec![
                        SetPlanItem::Property {
                            variable: "p".into(),
                            property: "title".into(),
                            value: Expr::new(ExprKind::Literal(Value::Text("published".to_owned()))),
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
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("published".to_owned())));
        assert_eq!(result.rows[0].get("score"), Some(&Value::Int64(9)));
        assert_eq!(result.rows[0].get("weight"), Some(&Value::Null));

        let post_id = graph
            .expand(alice, EdgeDirection::PointingRight, Some("AUTHORED"))
            .expect("expand")
            .into_iter()
            .next()
            .expect("edge exists")
            .node
            .id;

        let detach_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
            graph.expand(alice, EdgeDirection::PointingRight, Some("AUTHORED"))
                .expect("expand")
                .is_empty()
        );

        let reopened = GraphPma::open(graph.memory().clone()).expect("graph should reopen");
        let alice = reopened.get_node(alice).expect("get node").expect("alice exists");
        assert_eq!(alice.labels, vec!["User".to_owned()]);
        assert!(reopened.get_node(post_id).expect("get node").is_none());
        assert!(
            reopened
                .expand(alice.id, EdgeDirection::PointingRight, Some("AUTHORED"))
                .expect("expand")
                .is_empty()
        );
    }

    #[test]
    fn executes_cartesian_product_subplans() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        graph.insert_node(["Post"], [("title", Value::Text("p1".to_owned()))]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::CartesianProduct {
                    left: vec![PlanOp::NodeScan {
                        variable: "u".into(),
                        label: Some("User".into()),
                    }],
                    right: vec![PlanOp::NodeScan {
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
        assert_eq!(result.rows[0].get("uid"), Some(&Value::Text("u1".to_owned())));
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("p1".to_owned())));
    }

    #[test]
    fn executes_hash_join_subplans() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("hello".to_owned()))]);
        graph.insert_edge(alice, post, Some("AUTHORED"), std::iter::empty::<(&str, Value)>());

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::HashJoin {
                    left: vec![PlanOp::NodeScan {
                        variable: "a".into(),
                        label: Some("User".into()),
                    }],
                    right: vec![
                        PlanOp::NodeScan {
                            variable: "a".into(),
                            label: Some("User".into()),
                        },
                        PlanOp::Expand {
                            src: "a".into(),
                            edge: "e".into(),
                            dst: "p".into(),
                            direction: EdgeDirection::PointingRight,
                            label: Some("AUTHORED".into()),
                            var_len: None,
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
        assert_eq!(result.rows[0].get("uid"), Some(&Value::Text("u1".to_owned())));
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("hello".to_owned())));
    }

    #[test]
    fn executes_materialize_between_pipeline_stages() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("hello".to_owned()))]);
        graph.insert_edge(alice, post, Some("AUTHORED"), std::iter::empty::<(&str, Value)>());

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
                    var_len: None,
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
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("hello".to_owned())));
    }

    #[test]
    fn executes_optional_match_with_null_padding() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let bob = graph.insert_node(["User"], [("uid", Value::Text("u2".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("hello".to_owned()))]);
        graph.insert_edge(alice, post, Some("AUTHORED"), std::iter::empty::<(&str, Value)>());

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
                        var_len: None,
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
        assert_eq!(result.rows[0].get("uid"), Some(&Value::Text("u1".to_owned())));
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("hello".to_owned())));
        assert_eq!(result.rows[1].get("uid"), Some(&Value::Text("u2".to_owned())));
        assert_eq!(result.rows[1].get("title"), Some(&Value::Null));
        let _ = bob;
    }

    #[test]
    fn executes_union_all_set_operation() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("alice".to_owned()))]);
        graph.insert_node(["Admin"], [("name", Value::Text("root".to_owned()))]);

        let right = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
        assert_eq!(result.rows[0].get("name"), Some(&Value::Text("alice".to_owned())));
        assert_eq!(result.rows[1].get("name"), Some(&Value::Text("root".to_owned())));
    }

    #[test]
    fn executes_insert_vertex_and_insert_edge() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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

        let result = execute_plan(&mut graph, &plan).expect("insert dml should execute");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("hello".to_owned())));
        assert_eq!(result.rows[0].get("weight"), Some(&Value::Int64(10)));
        assert_eq!(graph.expand(alice, EdgeDirection::PointingRight, Some("AUTHORED")).expect("expand").len(), 1);
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
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("hello".to_owned())));
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
            gleaph_graph_pma::project_overlay_write_event(
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
            graph.expand(alice, EdgeDirection::PointingRight, Some("AUTHORED")).expect("expand").len(),
            1
        );
    }

    #[test]
    fn executes_set_remove_and_delete_dml() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("hello".to_owned()))]);
        let edge_id = graph.insert_edge(alice, post, Some("AUTHORED"), [("weight", Value::Int64(1))]);

        let set_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    var_len: None,
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

        let result = execute_plan(&mut graph, &set_plan).expect("set/remove should execute");
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("updated".to_owned())));
        assert_eq!(result.rows[0].get("weight"), Some(&Value::Null));
        assert_eq!(graph.get_node(alice).expect("node").unwrap().labels, vec!["User".to_owned()]);
        let _ = edge_id;

        let delete_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
        let delete_err = execute_plan(&mut graph, &delete_plan).expect_err("delete without detach should fail");
        assert!(matches!(delete_err, ExecutionError::Graph(_)));

        let detach_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
        execute_plan(&mut graph, &detach_plan).expect("detach delete should succeed");
        assert!(graph.get_node(post).expect("node").is_none());
        assert_eq!(graph.expand(alice, EdgeDirection::PointingRight, Some("AUTHORED")).expect("expand").len(), 0);
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
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    var_len: None,
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
        assert_eq!(result.rows[0].get("title"), Some(&Value::Text("updated".to_owned())));
        assert_eq!(result.rows[0].get("weight"), Some(&Value::Null));
        assert_eq!(graph.get_node(alice).expect("node").unwrap().labels, vec!["User".to_owned()]);
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
        assert_eq!(
            property_events.last(),
            Some(&property_summary.projection())
        );
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

        let delete_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
        let delete_err = execute_plan(&mut graph, &delete_plan).expect_err("delete without detach should fail");
        assert!(matches!(delete_err, ExecutionError::Graph(_)));

        let detach_plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
            gleaph_graph_pma::RewriteWriteEventProjection::NodeDelete(
                delete_summary.projection(),
            ),
        );
        assert_eq!(graph.node_delete_history().len(), 1);
        assert!(matches!(
            graph.write_history(),
            [
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapEdge(_),
                RewriteOverlayWriteEvent::BootstrapGraph(_),
                RewriteOverlayWriteEvent::Property(_),
                RewriteOverlayWriteEvent::Property(_),
                RewriteOverlayWriteEvent::Property(_),
                RewriteOverlayWriteEvent::Edge(_),
                RewriteOverlayWriteEvent::NodeDelete(_)
            ]
        ));
        assert!(graph.get_node(post).expect("node").is_none());
        assert_eq!(
            graph.expand(alice, EdgeDirection::PointingRight, Some("AUTHORED"))
                .expect("expand")
                .len(),
            0
        );
    }

    #[test]
    fn executes_delete_edge_dml() {
        let mut graph = InMemoryGraph::new();
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("hello".to_owned()))]);
        graph.insert_edge(alice, post, Some("AUTHORED"), [("weight", Value::Int64(1))]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    var_len: None,
                },
                PlanOp::DeleteEdge {
                    variable: "e".into(),
                },
            ],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        execute_plan(&mut graph, &plan).expect("delete edge should execute");
        assert_eq!(
            graph.expand(alice, EdgeDirection::PointingRight, Some("AUTHORED"))
                .expect("expand")
                .len(),
            0
        );
        assert!(graph.get_node(post).expect("get node").is_some());
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
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    var_len: None,
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
        assert!(matches!(
            graph.write_history(),
            [
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapEdge(_),
                RewriteOverlayWriteEvent::BootstrapGraph(_),
                RewriteOverlayWriteEvent::Edge(_)
            ]
        ));
        assert_eq!(
            graph.expand(alice, EdgeDirection::PointingRight, Some("AUTHORED"))
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
                    variable: "a".into(),
                    label: Some("User".into()),
                },
                PlanOp::Expand {
                    src: "a".into(),
                    edge: "e".into(),
                    dst: "p".into(),
                    direction: EdgeDirection::PointingRight,
                    label: Some("AUTHORED".into()),
                    var_len: None,
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
        assert_eq!(summary.operation, RewriteOverlayEdgeMutationKind::ReplaceLabel);
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
            graph.expand(alice, EdgeDirection::PointingRight, Some("AUTHORED"))
                .expect("expand")
                .len(),
            0
        );
    }

    #[test]
    fn rejects_plan_with_fatal_dml_errors_before_execution() {
        let mut graph = InMemoryGraph::new();
        let plan = PhysicalPlan {
            ops: vec![],
            diagnostics: gleaph_gql_planner::plan::PlanDiagnostics {
                dml_errors: vec![gleaph_gql_planner::plan::PlannerDiagnostic {
                    code: "DML002".into(),
                    message: gleaph_gql::type_check::dml_target_value_message("DELETE", Some("x")).into(),
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
    fn execution_summary_marks_dml_and_row_count() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);

        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::NodeScan {
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
        assert_eq!(result[0].get("name"), Some(&Value::Text("fallback".to_owned())));
    }
}

#[derive(Clone, Debug)]
struct GroupBucket {
    key: Vec<Value>,
    sample_row: BindingRow,
    aggregate_states: Vec<AggregateState>,
}

impl GroupBucket {
    fn new(key: Vec<Value>, sample_row: BindingRow, aggregate_len: usize) -> Self {
        Self {
            key,
            sample_row,
            aggregate_states: vec![AggregateState::default(); aggregate_len],
        }
    }
}

#[derive(Clone, Debug, Default)]
struct AggregateState {
    count: i64,
    distinct_values: Vec<Value>,
    sum: Option<Value>,
    extremum: Option<Value>,
}

fn update_aggregate_state<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    spec: &AggregateSpec,
    state: &mut AggregateState,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<()> {
    let func = spec.func.as_ref();
    match func {
        "Count" | "CountStar" => {
            let value = match &spec.expr {
                Some(expr) => eval_expr(graph, row, expr, ctx)?,
                None => Value::Int64(1),
            };
            if spec.expr.is_some() && matches!(value, Value::Null) {
                return Ok(());
            }
            if spec.distinct {
                if state.distinct_values.contains(&value) {
                    return Ok(());
                }
                state.distinct_values.push(value);
            }
            state.count += 1;
            Ok(())
        }
        "Sum" => {
            let value = match &spec.expr {
                Some(expr) => eval_expr(graph, row, expr, ctx)?,
                None => return Err(ExecutionError::UnsupportedPlanOp("Aggregate.sum_without_expr")),
            };
            if matches!(value, Value::Null) {
                return Ok(());
            }
            if spec.distinct {
                if state.distinct_values.contains(&value) {
                    return Ok(());
                }
                state.distinct_values.push(value.clone());
            }
            accumulate_sum(&mut state.sum, value)?;
            Ok(())
        }
        "Min" => {
            let value = match &spec.expr {
                Some(expr) => eval_expr(graph, row, expr, ctx)?,
                None => return Err(ExecutionError::UnsupportedPlanOp("Aggregate.min_without_expr")),
            };
            if matches!(value, Value::Null) {
                return Ok(());
            }
            if spec.distinct {
                if state.distinct_values.contains(&value) {
                    return Ok(());
                }
                state.distinct_values.push(value.clone());
            }
            update_extremum(&mut state.extremum, value, true);
            Ok(())
        }
        "Max" => {
            let value = match &spec.expr {
                Some(expr) => eval_expr(graph, row, expr, ctx)?,
                None => return Err(ExecutionError::UnsupportedPlanOp("Aggregate.max_without_expr")),
            };
            if matches!(value, Value::Null) {
                return Ok(());
            }
            if spec.distinct {
                if state.distinct_values.contains(&value) {
                    return Ok(());
                }
                state.distinct_values.push(value.clone());
            }
            update_extremum(&mut state.extremum, value, false);
            Ok(())
        }
        _ => Err(ExecutionError::UnsupportedPlanOp("Aggregate.func")),
    }
}

fn finalize_aggregate_state(state: &AggregateState) -> Value {
    if let Some(value) = &state.sum {
        return value.clone();
    }
    if let Some(value) = &state.extremum {
        return value.clone();
    }
    Value::Int64(state.count)
}

fn accumulate_sum(current: &mut Option<Value>, value: Value) -> ExecutionResultExt<()> {
    match current.take() {
        None => {
            *current = Some(value);
            Ok(())
        }
        Some(acc) => {
            *current = Some(sum_values(acc, value)?);
            Ok(())
        }
    }
}

fn sum_values(left: Value, right: Value) -> ExecutionResultExt<Value> {
    match (left, right) {
        (Value::Int8(a), Value::Int8(b)) => Ok(Value::Int8(a.saturating_add(b))),
        (Value::Int16(a), Value::Int16(b)) => Ok(Value::Int16(a.saturating_add(b))),
        (Value::Int32(a), Value::Int32(b)) => Ok(Value::Int32(a.saturating_add(b))),
        (Value::Int64(a), Value::Int64(b)) => Ok(Value::Int64(a.saturating_add(b))),
        (Value::Int128(a), Value::Int128(b)) => Ok(Value::Int128(a.saturating_add(b))),
        (Value::Uint8(a), Value::Uint8(b)) => Ok(Value::Uint8(a.saturating_add(b))),
        (Value::Uint16(a), Value::Uint16(b)) => Ok(Value::Uint16(a.saturating_add(b))),
        (Value::Uint32(a), Value::Uint32(b)) => Ok(Value::Uint32(a.saturating_add(b))),
        (Value::Uint64(a), Value::Uint64(b)) => Ok(Value::Uint64(a.saturating_add(b))),
        (Value::Uint128(a), Value::Uint128(b)) => Ok(Value::Uint128(a.saturating_add(b))),
        _ => Err(ExecutionError::UnsupportedPlanOp("Aggregate.sum_value_type")),
    }
}

fn update_extremum(slot: &mut Option<Value>, candidate: Value, is_min: bool) {
    match slot {
        None => *slot = Some(candidate),
        Some(current) => {
            let ord = compare_values(&candidate, current).unwrap_or(Ordering::Equal);
            let replace = if is_min {
                ord == Ordering::Less
            } else {
                ord == Ordering::Greater
            };
            if replace {
                *current = candidate;
            }
        }
    }
}
