//! Variables read from [`super::executor::PlanRow`] bindings by subsequent plan ops.

use std::collections::BTreeSet;

use gleaph_gql_planner::collect_expr_variables;
use gleaph_gql_planner::plan::PlanOp;

/// Names of plan variables whose bindings must remain on each row for `ops`.
pub fn variables_read_by_ops(ops: &[PlanOp]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for op in ops {
        variables_read_by_op(op, &mut out);
    }
    out
}

fn variables_read_by_op(op: &PlanOp, out: &mut BTreeSet<String>) {
    match op {
        PlanOp::PropertyFilter { predicates, .. } => {
            for predicate in predicates {
                collect_expr_vars(predicate, out);
            }
        }
        PlanOp::Filter { condition } => collect_expr_vars(condition, out),
        PlanOp::Let { bindings } => {
            for binding in bindings {
                collect_expr_vars(&binding.value, out);
            }
        }
        PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
            for column in columns {
                collect_expr_vars(&column.expr, out);
            }
        }
        PlanOp::Sort { order_by } => {
            for item in &order_by.items {
                collect_expr_vars(&item.expr, out);
            }
        }
        PlanOp::Limit { count, offset } => {
            if let Some(expr) = count {
                collect_expr_vars(expr, out);
            }
            if let Some(expr) = offset {
                collect_expr_vars(expr, out);
            }
        }
        PlanOp::TopK {
            order_by,
            k,
            offset,
            ..
        } => {
            for item in &order_by.items {
                collect_expr_vars(&item.expr, out);
            }
            collect_expr_vars(k, out);
            if let Some(expr) = offset {
                collect_expr_vars(expr, out);
            }
        }
        PlanOp::Expand { src, .. } => {
            out.insert(src.to_string());
        }
        PlanOp::ExpandFilter {
            src, dst_filter, ..
        } => {
            out.insert(src.to_string());
            for predicate in dst_filter {
                collect_expr_vars(predicate, out);
            }
        }
        PlanOp::ShortestPath { src, dst, .. } => {
            out.insert(src.to_string());
            out.insert(dst.to_string());
        }
        PlanOp::HashJoin { join_keys, .. } => {
            for key in join_keys {
                out.insert(key.to_string());
            }
        }
        PlanOp::Aggregate {
            group_by,
            aggregates,
            ..
        } => {
            for key in group_by {
                collect_expr_vars(key, out);
            }
            for aggregate in aggregates {
                if let Some(expr) = &aggregate.expr {
                    collect_expr_vars(expr, out);
                }
                if let Some(expr) = &aggregate.expr2 {
                    collect_expr_vars(expr, out);
                }
                if let Some(filter) = &aggregate.filter {
                    collect_expr_vars(filter, out);
                }
            }
        }
        PlanOp::OptionalMatch { sub_plan } => out.extend(variables_read_by_ops(sub_plan)),
        PlanOp::SetOperation { right, .. } => out.extend(variables_read_by_ops(&right.ops)),
        PlanOp::For { list, .. } => collect_expr_vars(list, out),
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            out.extend(variables_read_by_ops(&sub_plan.ops));
        }
        PlanOp::CallProcedure { args, .. } => {
            for arg in args {
                collect_expr_vars(arg, out);
            }
        }
        PlanOp::SetProperties { items } => {
            for item in items {
                match item {
                    gleaph_gql_planner::plan::SetPlanItem::Property {
                        variable, value, ..
                    }
                    | gleaph_gql_planner::plan::SetPlanItem::AllProperties { variable, value } => {
                        out.insert(variable.to_string());
                        collect_expr_vars(value, out);
                    }
                    gleaph_gql_planner::plan::SetPlanItem::Label { variable, .. } => {
                        out.insert(variable.to_string());
                    }
                }
            }
        }
        PlanOp::RemoveProperties { items } => {
            for item in items {
                match item {
                    gleaph_gql_planner::plan::RemovePlanItem::Property { variable, .. }
                    | gleaph_gql_planner::plan::RemovePlanItem::Label { variable, .. } => {
                        out.insert(variable.to_string());
                    }
                }
            }
        }
        PlanOp::DeleteVertex { variable }
        | PlanOp::DetachDeleteVertex { variable }
        | PlanOp::DeleteEdge { variable } => {
            out.insert(variable.to_string());
        }
        PlanOp::EdgeBindEndpoints { edge, .. } => {
            out.insert(edge.to_string());
        }
        PlanOp::WorstCaseOptimalJoin { edges, .. } => {
            for edge in edges {
                out.insert(edge.src.to_string());
                out.insert(edge.variable.to_string());
            }
        }
        PlanOp::NodeScan { .. }
        | PlanOp::IndexScan { .. }
        | PlanOp::EdgeIndexScan { .. }
        | PlanOp::ConditionalIndexScan { .. }
        | PlanOp::IndexIntersection { .. }
        | PlanOp::InsertVertex { .. }
        | PlanOp::InsertEdge { .. }
        | PlanOp::UseGraph { .. }
        | PlanOp::CartesianProduct { .. } => {}
    }
}

fn collect_expr_vars(expr: &gleaph_gql::ast::Expr, out: &mut BTreeSet<String>) {
    for name in collect_expr_variables(expr) {
        out.insert(name);
    }
}

/// When true, [`execute_shortest_path`] may emit rows that only carry the path binding.
pub fn shortest_path_may_emit_path_only_rows(
    remaining_ops: &[PlanOp],
    path_var: &str,
    src: &str,
    dst: &str,
    edge: &str,
    emit_edge_binding: bool,
) -> bool {
    if emit_edge_binding {
        return false;
    }
    let needed = variables_read_by_ops(remaining_ops);
    if !needed.contains(path_var) {
        return false;
    }
    ![src, dst, edge].iter().any(|name| needed.contains(*name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{Expr, ExprKind};
    use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, ProjectColumn, ShortestMode};

    #[test]
    fn all_shortest_bench_plan_allows_path_only_rows() {
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("Src".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("Dst".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: false,
                emit_path_binding: true,
                mode: ShortestMode::AllShortest,
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: None,
                label_expr: None,
                var_len: None,
                cost: gleaph_gql_planner::plan::ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("p".into())),
                    alias: Some("p".into()),
                }],
                distinct: false,
            },
        ]);
        let remaining = &plan.ops[3..];
        assert!(shortest_path_may_emit_path_only_rows(
            remaining, "p", "a", "c", "e", false
        ));
    }

    #[test]
    fn filter_on_src_disallows_path_only_rows() {
        let remaining = [PlanOp::Filter {
            condition: Expr::new(ExprKind::Variable("a".into())),
        }];
        assert!(!shortest_path_may_emit_path_only_rows(
            &remaining, "p", "a", "c", "e", false
        ));
    }
}
