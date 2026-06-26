//! Gleaph-specific interpretation of generic path-pattern extension clauses.

use gleaph_gql::ast::{Expr, ExprKind, ObjectName, ValueType};
use gleaph_gql::value::Value;
use gleaph_gql_planner::collect_expr_variables;
use gleaph_gql_planner::for_each_immediate_child_expr;
use gleaph_gql_planner::{
    PathPatternExtensionContext, PathPatternExtensionHandler, PlannerError, ShortestPathCost,
};
use gleaph_graph_kernel::gql_dialect::GLEAPH_COST;

use super::gleaph_weight::{
    gleaph_weight_arg_edge_var, gleaph_weight_single_arg, is_gleaph_weight_call,
};

pub struct GleaphPathExtensionHandler;

impl PathPatternExtensionHandler for GleaphPathExtensionHandler {
    fn plan_shortest_path_cost(
        &self,
        ctx: &PathPatternExtensionContext<'_>,
    ) -> Result<ShortestPathCost, PlannerError> {
        let Some(_shortest_mode) = ctx.shortest_mode else {
            return Err(PlannerError::UnsupportedExtension(
                "GLEAPH.COST is only supported on shortest-path patterns".into(),
            ));
        };
        if ctx.extensions.len() != 1 {
            return Err(PlannerError::UnsupportedExtension(
                "GLEAPH.COST requires exactly one extension clause".into(),
            ));
        }
        let ext = &ctx.extensions[0];
        if !is_gleaph_cost_extension_name(&ext.name.parts) {
            return Err(PlannerError::UnsupportedExtension(format!(
                "unsupported path pattern extension '{}'",
                ext.name.parts.join(".")
            )));
        }

        let single = ctx.single_edge.as_ref().ok_or_else(|| {
            PlannerError::UnsupportedExtension(
                "GLEAPH.COST requires a single-edge shortest-path pattern".into(),
            )
        })?;
        if single.label.is_none() && single.label_expr.is_none() {
            return Err(PlannerError::UnsupportedExtension(
                "GLEAPH.COST requires a single fixed edge label".into(),
            ));
        }
        let edge_var = single.edge_var.as_deref().ok_or_else(|| {
            PlannerError::UnsupportedExtension(
                "GLEAPH.COST requires the shortest-path edge pattern to declare a variable".into(),
            )
        })?;
        validate_cost_expr_references_edge(&ext.expr, edge_var)?;
        if matches!(&ext.expr.kind, ExprKind::Variable(_)) {
            return Err(PlannerError::UnsupportedExtension(
                "GLEAPH.COST expression must use GLEAPH.WEIGHT(edgeVar) or a numeric expression, not a bare edge variable".into(),
            ));
        }
        validate_cost_expr_shape(&ext.expr)?;
        validate_cost_expr_gleaph_weight_usage(&ext.expr, edge_var)?;

        Ok(ShortestPathCost::EdgeCostExpr {
            edge_var: edge_var.into(),
            expr: ext.expr.clone(),
        })
    }
}

fn is_gleaph_cost_extension_name(parts: &[String]) -> bool {
    GLEAPH_COST.matches_ascii_case_insensitive(parts)
}

fn validate_cost_expr_references_edge(expr: &Expr, edge_var: &str) -> Result<(), PlannerError> {
    let vars = collect_expr_variables(expr);
    if vars.len() != 1 {
        return Err(PlannerError::UnsupportedExtension(format!(
            "GLEAPH.COST expression must reference exactly the shortest-path edge variable '{edge_var}'"
        )));
    }
    if vars[0] != edge_var {
        return Err(PlannerError::UnsupportedExtension(format!(
            "GLEAPH.COST expression must reference exactly the shortest-path edge variable '{edge_var}'"
        )));
    }
    Ok(())
}

fn cost_shape_err(message: impl Into<String>) -> PlannerError {
    PlannerError::UnsupportedExtension(message.into())
}

fn is_numeric_cast_target(target: &ValueType) -> bool {
    matches!(
        target,
        ValueType::Int8 { .. }
            | ValueType::Int16 { .. }
            | ValueType::Int32 { .. }
            | ValueType::Int64 { .. }
            | ValueType::IntPrecision { .. }
            | ValueType::Int128 { .. }
            | ValueType::Int256 { .. }
            | ValueType::Uint8 { .. }
            | ValueType::Uint16 { .. }
            | ValueType::Uint32 { .. }
            | ValueType::Uint64 { .. }
            | ValueType::UintPrecision { .. }
            | ValueType::Uint128 { .. }
            | ValueType::Uint256 { .. }
            | ValueType::Float16 { .. }
            | ValueType::Float32 { .. }
            | ValueType::Float64 { .. }
            | ValueType::Float128
            | ValueType::Float256
            | ValueType::FloatPrecision { .. }
            | ValueType::Decimal { .. }
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GleaphWeightVarContext {
    Normal,
    GleaphWeightArg,
}

fn validate_cost_expr_gleaph_weight_usage(expr: &Expr, edge_var: &str) -> Result<(), PlannerError> {
    validate_cost_expr_gleaph_weight_usage_inner(expr, edge_var, GleaphWeightVarContext::Normal)
}

fn validate_cost_expr_gleaph_weight_usage_inner(
    expr: &Expr,
    edge_var: &str,
    context: GleaphWeightVarContext,
) -> Result<(), PlannerError> {
    if let ExprKind::Variable(v) = &expr.kind
        && context == GleaphWeightVarContext::Normal
    {
        return if v == edge_var {
            Err(cost_shape_err(
                "GLEAPH.COST expression may only reference the edge variable inside GLEAPH.WEIGHT(edgeVar)",
            ))
        } else {
            Ok(())
        };
    }
    if context == GleaphWeightVarContext::GleaphWeightArg {
        return match gleaph_weight_arg_edge_var(expr) {
            Some(v) if v == edge_var => Ok(()),
            Some(_) | None => Err(cost_shape_err(format!(
                "GLEAPH.WEIGHT argument must be the shortest-path edge variable '{edge_var}'"
            ))),
        };
    }
    if let ExprKind::FunctionCall {
        name,
        args,
        distinct,
    } = &expr.kind
        && is_gleaph_weight_call(name, *distinct)
    {
        let arg = gleaph_weight_single_arg(args).ok_or_else(|| {
            cost_shape_err(format!(
                "GLEAPH.WEIGHT expects 1 argument in GLEAPH.COST expression, got {}",
                args.len()
            ))
        })?;
        return validate_cost_expr_gleaph_weight_usage_inner(
            arg,
            edge_var,
            GleaphWeightVarContext::GleaphWeightArg,
        );
    }
    try_for_each_immediate_child_expr(expr, |child| {
        validate_cost_expr_gleaph_weight_usage_inner(
            child,
            edge_var,
            GleaphWeightVarContext::Normal,
        )
    })
}

fn validate_function_call_cost_shape(
    name: &ObjectName,
    args: &[Expr],
    distinct: bool,
) -> Result<(), PlannerError> {
    if distinct {
        return Err(cost_shape_err(
            "GLEAPH.COST expression does not support DISTINCT function calls",
        ));
    }
    if is_gleaph_weight_call(name, distinct) {
        gleaph_weight_single_arg(args).ok_or_else(|| {
            cost_shape_err(format!(
                "GLEAPH.WEIGHT expects 1 argument in GLEAPH.COST expression, got {}",
                args.len()
            ))
        })?;
        return Ok(());
    }
    let Some(last) = name.parts.last().map(|s| s.as_str()) else {
        return Err(cost_shape_err(
            "GLEAPH.COST expression has an empty function name",
        ));
    };
    if name.parts.len() != 1 {
        return Err(cost_shape_err(format!(
            "GLEAPH.COST expression does not support qualified function '{last}'"
        )));
    }
    Err(cost_shape_err(format!(
        "GLEAPH.COST expression does not support function '{last}'"
    )))
}

fn try_for_each_immediate_child_expr(
    expr: &Expr,
    mut visit: impl FnMut(&Expr) -> Result<(), PlannerError>,
) -> Result<(), PlannerError> {
    let mut err = Ok(());
    for_each_immediate_child_expr(expr, |child| {
        if err.is_ok() {
            err = visit(child);
        }
    });
    err
}

/// Per-node rules for `GLEAPH.COST` expression shapes (child recursion is handled separately).
fn validate_cost_expr_node(expr: &Expr) -> Result<(), PlannerError> {
    match &expr.kind {
        ExprKind::Literal(value) => {
            if value.is_numeric() || *value == Value::Null {
                Ok(())
            } else {
                Err(cost_shape_err(
                    "GLEAPH.COST expression literals must be numeric",
                ))
            }
        }
        ExprKind::Parameter(_) | ExprKind::Variable(_) => Ok(()),
        ExprKind::Paren(_)
        | ExprKind::UnaryOp { .. }
        | ExprKind::BinaryOp { .. }
        | ExprKind::Mod(..)
        | ExprKind::Log(..)
        | ExprKind::Power(..)
        | ExprKind::Coalesce(_)
        | ExprKind::NullIf(..)
        | ExprKind::Abs(_)
        | ExprKind::Floor(_)
        | ExprKind::Ceil(_)
        | ExprKind::Sqrt(_)
        | ExprKind::Exp(_)
        | ExprKind::Ln(_)
        | ExprKind::Log10(_)
        | ExprKind::Sin(_)
        | ExprKind::Cos(_)
        | ExprKind::Tan(_)
        | ExprKind::Asin(_)
        | ExprKind::Acos(_)
        | ExprKind::Atan(_)
        | ExprKind::Degrees(_)
        | ExprKind::Radians(_)
        | ExprKind::Cot(_)
        | ExprKind::Sinh(_)
        | ExprKind::Cosh(_)
        | ExprKind::Tanh(_)
        | ExprKind::CaseSimple { .. }
        | ExprKind::CaseSearched { .. } => Ok(()),
        ExprKind::Cast { target, .. } => {
            if is_numeric_cast_target(target) {
                Ok(())
            } else {
                Err(cost_shape_err(
                    "GLEAPH.COST CAST target must be a numeric type",
                ))
            }
        }
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
        } => validate_function_call_cost_shape(name, args, *distinct),
        _ => Err(cost_shape_err(format!(
            "GLEAPH.COST expression does not support expression kind {:?}",
            expr.kind
        ))),
    }
}

/// Ensures a `GLEAPH.COST` expression only uses numeric-cost shapes supported at execution time.
fn validate_cost_expr_shape(expr: &Expr) -> Result<(), PlannerError> {
    validate_cost_expr_node(expr)?;
    try_for_each_immediate_child_expr(expr, validate_cost_expr_shape)
}

pub(crate) static GLEAPH_PATH_EXTENSION_HANDLER: GleaphPathExtensionHandler =
    GleaphPathExtensionHandler;

// Full GLEAPH.COST coverage including sql-compat builtins:
// cargo test -p gleaph-graph --features sql-compat gleaph_cost
#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{
        BinaryOp, ExprKind, ObjectName, PathPatternExtension, PathPatternPrefix, SearchPrefix,
        ValueType, WhenClause,
    };
    use gleaph_gql::token::Span;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql_planner::SingleEdgePathInfo;
    use gleaph_gql_planner::plan::{ShortestMode, VarLenSpec};

    fn gleaph_cost_extension() -> ObjectName {
        ObjectName::qualified(vec!["GLEAPH".into(), "COST".into()])
    }

    fn gleaph_weight(edge_var: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
            args: vec![Expr::var(edge_var)],
            distinct: false,
        })
    }

    fn single_edge() -> SingleEdgePathInfo {
        SingleEdgePathInfo {
            edge_var: Some("e".into()),
            direction: EdgeDirection::PointingRight,
            label: Some("ROAD".into()),
            label_expr: None,
            var_len: Some(VarLenSpec {
                min: 1,
                max: Some(5),
            }),
        }
    }

    fn plan_cost_with_extension(expr: Expr, extension_name: ObjectName) -> ShortestPathCost {
        let ext = PathPatternExtension {
            span: Span::DUMMY,
            name: extension_name,
            expr,
        };
        GleaphPathExtensionHandler
            .plan_shortest_path_cost(&ctx(
                &[ext],
                Some(ShortestMode::AnyShortest),
                Some(single_edge()),
            ))
            .expect("GLEAPH.COST should plan")
    }

    fn plan_cost(expr: Expr) -> ShortestPathCost {
        plan_cost_with_extension(expr, gleaph_cost_extension())
    }

    fn plan_cost_with_extension_err(expr: Expr, extension_name: ObjectName) -> PlannerError {
        let ext = PathPatternExtension {
            span: Span::DUMMY,
            name: extension_name,
            expr,
        };
        GleaphPathExtensionHandler
            .plan_shortest_path_cost(&ctx(
                &[ext],
                Some(ShortestMode::AnyShortest),
                Some(single_edge()),
            ))
            .expect_err("GLEAPH.COST should be rejected")
    }

    fn plan_cost_err(expr: Expr) -> PlannerError {
        plan_cost_with_extension_err(expr, gleaph_cost_extension())
    }

    fn ctx<'a>(
        extensions: &'a [PathPatternExtension],
        shortest_mode: Option<ShortestMode>,
        single_edge: Option<SingleEdgePathInfo>,
    ) -> PathPatternExtensionContext<'a> {
        PathPatternExtensionContext {
            prefix: Some(&PathPatternPrefix::Search(SearchPrefix::AnyShortest {
                mode: None,
                path_keyword: None,
            })),
            extensions,
            shortest_mode,
            single_edge,
        }
    }

    #[test]
    fn gleaph_cost_rejects_underscored_cost_extension_name() {
        let err =
            plan_cost_with_extension_err(gleaph_weight("e"), ObjectName::simple("GLEAPH_COST"));
        assert!(
            err.to_string()
                .contains("unsupported path pattern extension"),
            "{err}"
        );
    }

    #[test]
    fn gleaph_cost_rejects_bare_edge_variable() {
        let err = plan_cost_err(Expr::var("e"));
        assert!(err.to_string().contains("bare edge variable"), "{err}");
    }

    #[test]
    fn gleaph_cost_rejects_literal_without_edge_reference() {
        let expr = Expr::new(ExprKind::Literal(gleaph_gql::value::Value::Float32(1.0)));
        let err = plan_cost_err(expr);
        assert!(
            err.to_string().contains("shortest-path edge variable"),
            "{err}"
        );
    }

    #[test]
    fn gleaph_cost_accepts_repeated_gleaph_weight_same_edge_var() {
        let expr = Expr::new(ExprKind::BinaryOp {
            left: Box::new(gleaph_weight("e")),
            op: BinaryOp::Add,
            right: Box::new(gleaph_weight("e")),
        });
        assert!(matches!(
            plan_cost(expr),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_accepts_gleaph_weight() {
        assert!(matches!(
            plan_cost(gleaph_weight("e")),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_accepts_union_label_expr_on_shortest_path() {
        let mut edge = single_edge();
        edge.label = None;
        edge.label_expr = Some(gleaph_gql::types::LabelExpr::Or(
            Box::new(gleaph_gql::types::LabelExpr::Name("KNOWS".into())),
            Box::new(gleaph_gql::types::LabelExpr::Name("LIKES".into())),
        ));
        let ext = PathPatternExtension {
            span: Span::DUMMY,
            name: gleaph_cost_extension(),
            expr: gleaph_weight("e"),
        };
        assert!(matches!(
            GleaphPathExtensionHandler.plan_shortest_path_cost(&ctx(
                &[ext],
                Some(ShortestMode::AnyShortest),
                Some(edge),
            ))
            .expect("union label_expr with GLEAPH.COST should plan"),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_accepts_gleaph_weight_parenthesized_arg() {
        let expr = Expr::new(ExprKind::FunctionCall {
            name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
            args: vec![Expr::new(ExprKind::Paren(Box::new(Expr::var("e"))))],
            distinct: false,
        });
        assert!(matches!(
            plan_cost(expr),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_accepts_gleaph_weight_triple_parenthesized_arg() {
        fn paren(expr: Expr) -> Expr {
            Expr::new(ExprKind::Paren(Box::new(expr)))
        }
        let expr = Expr::new(ExprKind::FunctionCall {
            name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
            args: vec![paren(paren(paren(Expr::var("e"))))],
            distinct: false,
        });
        assert!(matches!(
            plan_cost(expr),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_rejects_function_call_abs_wrapped_weight() {
        let expr = Expr::new(ExprKind::FunctionCall {
            name: ObjectName::simple("ABS"),
            args: vec![gleaph_weight("e")],
            distinct: false,
        });
        let err = plan_cost_err(expr);
        assert!(
            err.to_string().contains("does not support function 'ABS'"),
            "{err}"
        );
    }

    #[test]
    fn gleaph_cost_rejects_function_call_mod_wrapped_weight() {
        let expr = Expr::new(ExprKind::FunctionCall {
            name: ObjectName::simple("MOD"),
            args: vec![
                gleaph_weight("e"),
                Expr::new(ExprKind::Literal(gleaph_gql::value::Value::Float32(2.0))),
            ],
            distinct: false,
        });
        let err = plan_cost_err(expr);
        assert!(
            err.to_string().contains("does not support function 'MOD'"),
            "{err}"
        );
    }

    #[test]
    fn gleaph_cost_accepts_builtin_abs_wrapped_weight() {
        let expr = Expr::new(ExprKind::Abs(Box::new(gleaph_weight("e"))));
        assert!(matches!(
            plan_cost(expr),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_accepts_cast_wrapped_weight() {
        let expr = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight("e")),
            target: ValueType::Float32 {
                keyword: gleaph_gql::ast::Keyword::new("FLOAT32"),
            },
        });
        assert!(matches!(
            plan_cost(expr),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_accepts_cast_wrapped_weight_float128() {
        let expr = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight("e")),
            target: ValueType::Float128,
        });
        assert!(matches!(
            plan_cost(expr),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_accepts_cast_wrapped_weight_float256() {
        let expr = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight("e")),
            target: ValueType::Float256,
        });
        assert!(matches!(
            plan_cost(expr),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_accepts_coalesce_wrapped_weight() {
        let expr = Expr::new(ExprKind::Coalesce(vec![
            gleaph_weight("e"),
            Expr::new(ExprKind::Literal(gleaph_gql::value::Value::Float32(1.0))),
        ]));
        assert!(matches!(
            plan_cost(expr),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_accepts_case_wrapped_weight() {
        let expr = Expr::new(ExprKind::CaseSimple {
            operand: Box::new(Expr::new(ExprKind::Literal(gleaph_gql::value::Value::Null))),
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(gleaph_gql::value::Value::Null)),
                result: gleaph_weight("e"),
            }],
            else_clause: Some(Box::new(gleaph_weight("e"))),
        });
        assert!(matches!(
            plan_cost(expr),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_rejects_binary_edge_var_misuse() {
        let expr = Expr::new(ExprKind::BinaryOp {
            left: Box::new(Expr::var("e")),
            op: BinaryOp::Mul,
            right: Box::new(Expr::new(ExprKind::Literal(
                gleaph_gql::value::Value::Float32(2.0),
            ))),
        });
        let err = plan_cost_err(expr);
        assert!(err.to_string().contains("inside GLEAPH.WEIGHT"), "{err}");
    }

    #[test]
    fn gleaph_cost_rejects_case_operand_edge_var_misuse() {
        let expr = Expr::new(ExprKind::CaseSimple {
            operand: Box::new(Expr::var("e")),
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(gleaph_gql::value::Value::Null)),
                result: gleaph_weight("e"),
            }],
            else_clause: Some(Box::new(gleaph_weight("e"))),
        });
        let err = plan_cost_err(expr);
        assert!(err.to_string().contains("inside GLEAPH.WEIGHT"), "{err}");
    }

    #[test]
    fn gleaph_cost_rejects_case_when_condition_edge_var_misuse() {
        let expr = Expr::new(ExprKind::CaseSimple {
            operand: Box::new(Expr::new(ExprKind::Literal(gleaph_gql::value::Value::Null))),
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::var("e"),
                result: gleaph_weight("e"),
            }],
            else_clause: Some(Box::new(gleaph_weight("e"))),
        });
        let err = plan_cost_err(expr);
        assert!(err.to_string().contains("inside GLEAPH.WEIGHT"), "{err}");
    }

    #[test]
    fn gleaph_cost_rejects_gleaph_weight_non_variable_arg() {
        let expr = Expr::new(ExprKind::BinaryOp {
            left: Box::new(gleaph_weight("e")),
            op: BinaryOp::Add,
            right: Box::new(Expr::new(ExprKind::FunctionCall {
                name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
                args: vec![Expr::new(ExprKind::Literal(
                    gleaph_gql::value::Value::Float32(1.0),
                ))],
                distinct: false,
            })),
        });
        let err = plan_cost_err(expr);
        assert!(
            err.to_string()
                .contains("argument must be the shortest-path edge variable"),
            "{err}"
        );
    }

    #[test]
    fn gleaph_cost_rejects_gleaph_weight_expression_arg() {
        let expr = Expr::new(ExprKind::FunctionCall {
            name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
            args: vec![Expr::new(ExprKind::BinaryOp {
                left: Box::new(Expr::var("e")),
                op: BinaryOp::Add,
                right: Box::new(Expr::new(ExprKind::Literal(
                    gleaph_gql::value::Value::Float32(0.0),
                ))),
            })],
            distinct: false,
        });
        let err = plan_cost_err(expr);
        assert!(
            err.to_string()
                .contains("argument must be the shortest-path edge variable"),
            "{err}"
        );
    }

    #[test]
    fn gleaph_cost_accepts_floor_wrapped_weight() {
        let expr = Expr::new(ExprKind::Floor(Box::new(gleaph_weight("e"))));
        assert!(matches!(
            plan_cost(expr),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn gleaph_cost_rejects_upper_wrapped_weight() {
        let expr = Expr::new(ExprKind::Upper(Box::new(gleaph_weight("e"))));
        let err = plan_cost_err(expr);
        assert!(
            err.to_string().to_ascii_lowercase().contains("upper")
                || err.to_string().contains("does not support"),
            "{err}"
        );
    }

    #[test]
    #[cfg(feature = "sql-compat")]
    fn gleaph_cost_rejects_atan2_wrapped_weight() {
        let expr = Expr::new(ExprKind::Atan2(
            Box::new(gleaph_weight("e")),
            Box::new(Expr::new(ExprKind::Literal(
                gleaph_gql::value::Value::Float32(1.0),
            ))),
        ));
        let err = plan_cost_err(expr);
        assert!(
            err.to_string().contains("Atan2") || err.to_string().contains("does not support"),
            "{err}"
        );
    }
}
