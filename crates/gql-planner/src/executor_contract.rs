//! Static contract: which [`PlanOp`] shapes the `gleaph-gql-executor` implements.
//!
//! Keeps `gql-executor` free of IC-specific types while allowing a single early
//! validation path before graph access. Must stay in sync with
//! `gleaph_gql_executor::execute_ops` match arms.

use gleaph_gql::types::EdgeDirection;

use crate::plan::{AggregateSpec, PhysicalPlan, PlanOp, SetPlanItem};

/// Returns the first operator (or nested operator) the executor cannot run, if any.
///
/// This detects missing `match` arms and intentionally unsupported shapes (e.g.
/// variable-length `Expand`) without executing the plan.
pub fn first_executor_unsupported_op(plan: &PhysicalPlan) -> Option<&'static str> {
    first_unsupported_in_ops(&plan.ops)
}

fn first_unsupported_in_ops(ops: &[PlanOp]) -> Option<&'static str> {
    for op in ops {
        if let Some(name) = check_op(op) {
            return Some(name);
        }
    }
    None
}

fn check_op(op: &PlanOp) -> Option<&'static str> {
    match op {
        PlanOp::InlineProcedureCall { sub_plan, .. } => first_executor_unsupported_op(sub_plan),
        PlanOp::UseGraph { sub_plan, .. } => {
            sub_plan.as_ref().and_then(|p| first_unsupported_in_ops(p))
        }
        PlanOp::HashJoin { left, right, .. } => {
            if subplan_may_return_projected(left) || subplan_may_return_projected(right) {
                Some("HashJoin.projected_subplan")
            } else {
                first_unsupported_in_ops(left).or_else(|| first_unsupported_in_ops(right))
            }
        }
        PlanOp::CartesianProduct { left, right } => {
            if subplan_may_return_projected(left) || subplan_may_return_projected(right) {
                Some("CartesianProduct.projected_subplan")
            } else {
                first_unsupported_in_ops(left).or_else(|| first_unsupported_in_ops(right))
            }
        }
        PlanOp::OptionalMatch { sub_plan } => {
            if subplan_may_return_projected(sub_plan) {
                Some("OptionalMatch.projected_subplan")
            } else {
                first_unsupported_in_ops(sub_plan)
            }
        }
        PlanOp::SetOperation { right, .. } => first_executor_unsupported_op(right),
        PlanOp::SetProperties { items } => {
            if items
                .iter()
                .any(|it| matches!(it, SetPlanItem::AllProperties { .. }))
            {
                Some("SetProperties.AllProperties")
            } else {
                None
            }
        }
        PlanOp::EdgeBindEndpoints { direction, .. } => match direction {
            EdgeDirection::PointingRight | EdgeDirection::PointingLeft => None,
            _ => Some("EdgeBindEndpoints.direction"),
        },
        PlanOp::Aggregate { aggregates, .. } => check_aggregate_specs(aggregates),
        _ => None,
    }
}

/// Static check mirroring [`gleaph_gql_executor::update_aggregate_state`].
fn check_aggregate_specs(aggregates: &[AggregateSpec]) -> Option<&'static str> {
    for spec in aggregates {
        let f = spec.func.as_ref();
        match f {
            "Count" | "CountStar" => {}
            "Sum" | "Min" | "Max" => {
                if spec.expr.is_none() {
                    return Some(if f == "Sum" {
                        "Aggregate.sum_without_expr"
                    } else if f == "Min" {
                        "Aggregate.min_without_expr"
                    } else {
                        "Aggregate.max_without_expr"
                    });
                }
            }
            "Avg" => {
                if spec.expr.is_none() {
                    return Some("Aggregate.avg_without_expr");
                }
            }
            _ => return Some("Aggregate.func"),
        }
    }
    None
}

fn subplan_may_return_projected(ops: &[PlanOp]) -> bool {
    let mut projected = false;
    for op in ops {
        match op {
            PlanOp::Project { .. } | PlanOp::SetOperation { .. } => projected = true,
            PlanOp::Materialize { .. } => projected = false,
            PlanOp::Sort { .. } | PlanOp::Limit { .. } | PlanOp::TopK { .. } => {}
            _ => projected = false,
        }
    }
    projected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanAnnotations, PlanDiagnostics};

    #[test]
    fn shortest_path_supported_without_path_var() {
        use crate::plan::{ShortestMode, VarLenSpec};
        use gleaph_gql::types::EdgeDirection;

        let plan = PhysicalPlan {
            ops: vec![PlanOp::ShortestPath {
                src: "a".into(),
                dst: "b".into(),
                edge: "e".into(),
                path_var: None,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(3),
                }),
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        assert_eq!(first_executor_unsupported_op(&plan), None);
    }

    #[test]
    fn shortest_path_with_path_var_is_supported() {
        let plan = PhysicalPlan {
            ops: vec![PlanOp::ShortestPath {
                src: "a".into(),
                dst: "b".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                mode: crate::plan::ShortestMode::AnyShortest,
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: None,
                label_expr: None,
                var_len: None,
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        assert_eq!(first_executor_unsupported_op(&plan), None);
    }

    #[test]
    fn set_all_properties_reported_unsupported() {
        use crate::plan::SetPlanItem;
        let plan = PhysicalPlan {
            ops: vec![PlanOp::SetProperties {
                items: vec![SetPlanItem::AllProperties {
                    variable: "n".into(),
                    value: gleaph_gql::ast::Expr::new(gleaph_gql::ast::ExprKind::Literal(
                        gleaph_gql::Value::Null,
                    )),
                }],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        assert_eq!(
            first_executor_unsupported_op(&plan),
            Some("SetProperties.AllProperties")
        );
    }

    #[test]
    fn avg_aggregate_with_expr_is_supported_in_contract() {
        use crate::plan::AggregateSpec;
        let plan = PhysicalPlan {
            ops: vec![PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: "Avg".into(),
                    expr: Some(gleaph_gql::ast::Expr::new(
                        gleaph_gql::ast::ExprKind::Literal(gleaph_gql::Value::Int64(1)),
                    )),
                    distinct: false,
                    alias: Some("m".into()),
                }],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        assert_eq!(first_executor_unsupported_op(&plan), None);
    }

    #[test]
    fn collect_aggregate_reported_unsupported() {
        use crate::plan::AggregateSpec;
        let plan = PhysicalPlan {
            ops: vec![PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: "Collect".into(),
                    expr: Some(gleaph_gql::ast::Expr::new(
                        gleaph_gql::ast::ExprKind::Literal(gleaph_gql::Value::Int64(1)),
                    )),
                    distinct: false,
                    alias: None,
                }],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        assert_eq!(first_executor_unsupported_op(&plan), Some("Aggregate.func"));
    }

    #[test]
    fn nested_subplan_checked() {
        use crate::plan::{ShortestMode, VarLenSpec};
        use gleaph_gql::types::EdgeDirection;

        let plan = PhysicalPlan {
            ops: vec![PlanOp::OptionalMatch {
                sub_plan: vec![PlanOp::ShortestPath {
                    src: "a".into(),
                    dst: "b".into(),
                    edge: "e".into(),
                    path_var: None,
                    mode: ShortestMode::AnyShortest,
                    direction: EdgeDirection::PointingRight,
                    label: None,
                    label_expr: None,
                    var_len: Some(VarLenSpec {
                        min: 1,
                        max: Some(1),
                    }),
                }],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        assert_eq!(first_executor_unsupported_op(&plan), None);
    }

    #[test]
    fn call_procedure_supported() {
        use crate::plan::YieldColumn;

        let plan = PhysicalPlan {
            ops: vec![PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![YieldColumn {
                    name: "lbl".into(),
                    alias: None,
                }]),
                optional: false,
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };

        assert_eq!(first_executor_unsupported_op(&plan), None);
    }

    #[test]
    fn hash_join_projected_subplan_reported_unsupported() {
        use crate::plan::ProjectColumn;
        use gleaph_gql::ast::{Expr, ExprKind};
        let projected_sub = vec![PlanOp::Project {
            columns: vec![ProjectColumn {
                expr: Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(1))),
                alias: Some("x".into()),
            }],
            distinct: false,
        }];
        let plan = PhysicalPlan {
            ops: vec![PlanOp::HashJoin {
                left: projected_sub,
                right: vec![],
                join_keys: vec![],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        assert_eq!(
            first_executor_unsupported_op(&plan),
            Some("HashJoin.projected_subplan")
        );
    }

    #[test]
    fn optional_match_materialized_subplan_supported() {
        use crate::plan::{ProjectColumn, Str};
        use gleaph_gql::ast::{Expr, ExprKind};
        let sub_plan = vec![
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Literal(gleaph_gql::Value::Int64(1))),
                    alias: Some("x".into()),
                }],
                distinct: false,
            },
            PlanOp::Materialize {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("x".to_owned())),
                    alias: Some(Str::from("x")),
                }],
                distinct: false,
            },
        ];
        let plan = PhysicalPlan {
            ops: vec![PlanOp::OptionalMatch { sub_plan }],
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        assert_eq!(first_executor_unsupported_op(&plan), None);
    }

    #[test]
    fn worst_case_optimal_join_is_supported() {
        use crate::plan::{Str, WcojEdge};
        use gleaph_gql::types::EdgeDirection;

        let plan = PhysicalPlan {
            ops: vec![PlanOp::WorstCaseOptimalJoin {
                variables: vec![Str::from("a"), Str::from("b"), Str::from("c")],
                edges: vec![
                    WcojEdge {
                        src: "a".into(),
                        dst: "b".into(),
                        variable: "e1".into(),
                        label: Some("KNOWS".into()),
                        label_expr: None,
                        direction: EdgeDirection::PointingRight,
                        var_len: None,
                        indexed_edge_equality: None,
                        dst_filter: vec![],
                        hop_aux_binding: None,
                    },
                    WcojEdge {
                        src: "b".into(),
                        dst: "c".into(),
                        variable: "e2".into(),
                        label: Some("KNOWS".into()),
                        label_expr: None,
                        direction: EdgeDirection::PointingRight,
                        var_len: None,
                        indexed_edge_equality: None,
                        dst_filter: vec![],
                        hop_aux_binding: None,
                    },
                    WcojEdge {
                        src: "c".into(),
                        dst: "a".into(),
                        variable: "e3".into(),
                        label: Some("KNOWS".into()),
                        label_expr: None,
                        direction: EdgeDirection::PointingRight,
                        var_len: None,
                        indexed_edge_equality: None,
                        dst_filter: vec![],
                        hop_aux_binding: None,
                    },
                ],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        };
        assert_eq!(first_executor_unsupported_op(&plan), None);
    }
}
