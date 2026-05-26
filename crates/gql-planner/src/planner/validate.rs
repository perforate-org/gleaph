use gleaph_gql::ast::{Expr, ExprKind, OrderByClause};
use gleaph_gql::type_check::{
    DmlDiagnosticSeverity, TypeWarning, dml_diagnostic_from_warning, type_diagnostic_from_warning,
};

use super::PlannerError;
use crate::expr_children::for_each_immediate_child_expr;
use crate::plan::{
    PhysicalPlan, PlanDiagnostics, PlanOp, PropertyAssignment, SetPlanItem, ShortestPathCost,
};
pub(crate) fn validate_plan(plan: PhysicalPlan) -> Result<PhysicalPlan, PlannerError> {
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

pub(crate) fn apply_type_checker_dml_diagnostics(
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
