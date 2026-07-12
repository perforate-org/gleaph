use crate::plan::{PlanAnnotations, PlanOp};

use super::vars::all_variables_eq;

pub fn apply_ev_fusion(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    let mut i = 0;
    while i + 1 < ops.len() {
        let can_fuse = match (&ops[i], &ops[i + 1]) {
            (PlanOp::Expand { dst, .. }, PlanOp::PropertyFilter { predicates, .. }) => {
                // Check all predicates reference only the dst variable (zero-copy).
                predicates.iter().all(|pred| all_variables_eq(pred, dst))
            }
            _ => false,
        };

        if can_fuse {
            // Extract both ops.
            let filter_op = ops.remove(i + 1);
            let expand_op = ops.remove(i);

            if let (
                PlanOp::Expand {
                    src,
                    edge,
                    dst,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    indexed_edge_equality,
                    edge_inline_value_predicate,
                    edge_inline_vector_predicate,
                    edge_property_projection,
                    dst_property_projection,
                    hop_aux_binding,
                    emit_edge_binding,
                    near_group_var: _,
                    far_group_var: _,
                    path_var: _,
                    emit_path_binding: _,
                },
                PlanOp::PropertyFilter { predicates, .. },
            ) = (expand_op, filter_op)
            {
                ops.insert(
                    i,
                    PlanOp::ExpandFilter {
                        src,
                        edge,
                        dst,
                        direction,
                        label,
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        edge_inline_value_predicate,
                        edge_inline_vector_predicate,
                        dst_filter: predicates,
                        edge_property_projection,
                        dst_property_projection,
                        hop_aux_binding,
                        emit_edge_binding,
                        near_group_var: None,
                        far_group_var: None,
                        path_var: None,
                        emit_path_binding: false,
                    },
                );
                annotations.optimizer.ev_fusion_applied = true;
            }
            // Don't increment i — check the new ExpandFilter against next op.
        } else {
            i += 1;
        }
    }
}

/// LateProject: ensure Project appears after all Filter/ExpandFilter/Sort/TopK ops.
/// If Project is found before any of those ops, move it after the last one.
/// This keeps sort keys (e.g. `ORDER BY p.order_index`) resolvable on graph
/// bindings rather than projected values that may have been dropped.
pub fn apply_late_project(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    let project_idx = ops
        .iter()
        .position(|op| matches!(op, PlanOp::Project { .. }));
    let last_blocking_idx = ops.iter().rposition(|op| {
        matches!(
            op,
            PlanOp::PropertyFilter { .. }
                | PlanOp::Filter { .. }
                | PlanOp::ExpandFilter { .. }
                | PlanOp::Expand { .. }
                | PlanOp::Sort { .. }
                | PlanOp::TopK { .. }
        )
    });

    if let (Some(pi), Some(fi)) = (project_idx, last_blocking_idx) {
        if pi < fi {
            let project_op = ops.remove(pi);
            // fi shifted by -1 since we removed an element before it.
            ops.insert(fi, project_op);
            annotations.optimizer.late_project_applied = true;
        } else {
            annotations.optimizer.late_project_applied = true; // Already in the right place.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanOp, ProjectColumn};
    use gleaph_gql::ast::{Expr, ExprKind, ObjectName, OrderByClause, SortItem};
    use gleaph_gql::types::EdgeDirection;

    fn node_scan(variable: &str, label: &str) -> PlanOp {
        PlanOp::NodeScan {
            variable: variable.into(),
            label: Some(label.into()),
            property_projection: None,
        }
    }

    fn expand(src: &str, edge: &str, dst: &str, label: &str) -> PlanOp {
        PlanOp::Expand {
            src: src.into(),
            edge: edge.into(),
            dst: dst.into(),
            direction: EdgeDirection::PointingLeft,
            label: Some(label.into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_inline_value_predicate: None,
            edge_inline_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        }
    }

    fn project(columns: Vec<ProjectColumn>) -> PlanOp {
        PlanOp::Project {
            columns,
            distinct: false,
        }
    }

    fn sort_by(expr: Expr) -> PlanOp {
        PlanOp::Sort {
            order_by: OrderByClause {
                span: gleaph_gql::token::Span::default(),
                items: vec![SortItem {
                    span: gleaph_gql::token::Span::default(),
                    expr,
                    direction: None,
                    null_order: None,
                }],
            },
        }
    }

    fn topk(expr: Expr) -> PlanOp {
        PlanOp::TopK {
            order_by: OrderByClause {
                span: gleaph_gql::token::Span::default(),
                items: vec![SortItem {
                    span: gleaph_gql::token::Span::default(),
                    expr,
                    direction: None,
                    null_order: None,
                }],
            },
            k: Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(10))),
            offset: None,
        }
    }

    fn sequence_expr(edge_var: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: ObjectName::qualified(vec!["GLEAPH".into(), "SEQUENCE".into()]),
            args: vec![Expr::new(ExprKind::Variable(edge_var.into()))],
            distinct: false,
        })
    }

    #[test]
    fn late_project_moves_project_after_sort() {
        let mut ops = vec![
            node_scan("feed", "Feed"),
            expand("feed", "e", "p", "IN_PUBLIC_FEED"),
            project(vec![ProjectColumn {
                expr: Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable("p".into()))),
                    property: "demo_id".into(),
                }),
                alias: Some("post_id".into()),
            }]),
            sort_by(sequence_expr("e")),
        ];
        let mut annotations = PlanAnnotations::default();
        apply_late_project(&mut ops, &mut annotations);
        assert!(annotations.optimizer.late_project_applied);
        assert!(
            matches!(&ops[2], PlanOp::Sort { .. }),
            "Sort should stay before Project"
        );
        assert!(
            matches!(&ops[3], PlanOp::Project { .. }),
            "Project should be moved after Sort"
        );
    }

    #[test]
    fn late_project_moves_project_after_topk() {
        let mut ops = vec![
            node_scan("feed", "Feed"),
            expand("feed", "e", "p", "IN_PUBLIC_FEED"),
            project(vec![ProjectColumn {
                expr: Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable("p".into()))),
                    property: "demo_id".into(),
                }),
                alias: Some("post_id".into()),
            }]),
            topk(sequence_expr("e")),
        ];
        let mut annotations = PlanAnnotations::default();
        apply_late_project(&mut ops, &mut annotations);
        assert!(annotations.optimizer.late_project_applied);
        assert!(
            matches!(&ops[2], PlanOp::TopK { .. }),
            "TopK should stay before Project"
        );
        assert!(
            matches!(&ops[3], PlanOp::Project { .. }),
            "Project should be moved after TopK"
        );
    }

    #[test]
    fn late_project_keeps_project_after_last_blocking_op() {
        let mut ops = vec![
            node_scan("feed", "Feed"),
            expand("feed", "e", "p", "IN_PUBLIC_FEED"),
            sort_by(sequence_expr("e")),
            project(vec![ProjectColumn {
                expr: Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable("p".into()))),
                    property: "demo_id".into(),
                }),
                alias: Some("post_id".into()),
            }]),
        ];
        let mut annotations = PlanAnnotations::default();
        apply_late_project(&mut ops, &mut annotations);
        assert!(annotations.optimizer.late_project_applied);
        assert!(
            matches!(&ops[2], PlanOp::Sort { .. }),
            "Sort should stay before Project"
        );
        assert!(
            matches!(&ops[3], PlanOp::Project { .. }),
            "Project already after Sort should not move"
        );
    }
}
