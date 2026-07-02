//! Gleaph-specific integration between generic GQL parsing/planning and Graph/Router execution.
//!
//! This crate intentionally lives between `gleaph-gql-planner` and the execution crates so that
//! both the Router (which plans ingress queries) and the Graph shard (which replans or validates)
//! can share the same path-extension classification without leaking Gleaph semantics into the
//! generic GQL crates.

use gleaph_gql::ast::{Expr, ExprKind, ObjectName, ValueType};
use gleaph_gql_planner::{
    PathPatternExtensionContext, PathPatternExtensionHandler, PlannerError, ShortestPathCost,
};
use gleaph_graph_kernel::gql_dialect::{COST, GLEAPH_COST, GLEAPH_WEIGHT};

/// Gleaph-specific interpretation of generic path-pattern extension clauses.
pub struct GleaphPathExtensionHandler;

impl PathPatternExtensionHandler for GleaphPathExtensionHandler {
    fn plan_shortest_path_cost(
        &self,
        ctx: &PathPatternExtensionContext<'_>,
    ) -> Result<ShortestPathCost, PlannerError> {
        let Some(_shortest_mode) = ctx.shortest_mode else {
            return Err(PlannerError::UnsupportedExtension(
                "COST BY is only supported on shortest-path patterns".into(),
            ));
        };
        if ctx.extensions.len() != 1 {
            return Err(PlannerError::UnsupportedExtension(
                "shortest-path cost requires exactly one extension clause".into(),
            ));
        }
        let ext = &ctx.extensions[0];

        let single = ctx.single_edge.as_ref().ok_or_else(|| {
            PlannerError::UnsupportedExtension(
                "shortest-path cost requires a single-edge path pattern".into(),
            )
        })?;
        let edge_var = single.edge_var.as_deref().ok_or_else(|| {
            PlannerError::UnsupportedExtension(
                "shortest-path cost requires the edge pattern to declare a variable".into(),
            )
        })?;

        if is_gleaph_cost_extension_name(&ext.name.parts) {
            return plan_gleaph_cost_shortest_path(single, edge_var, &ext.expr);
        }
        if is_cost_extension_name(&ext.name.parts) {
            return plan_inline_property_cost_shortest_path(single, edge_var, &ext.expr);
        }

        Err(PlannerError::UnsupportedExtension(format!(
            "unsupported path pattern extension '{}'",
            ext.name.parts.join(".")
        )))
    }
}

/// Shared static handler for Router and Graph planning.
pub static GLEAPH_PATH_EXTENSION_HANDLER: GleaphPathExtensionHandler = GleaphPathExtensionHandler;

fn plan_gleaph_cost_shortest_path(
    single: &gleaph_gql_planner::SingleEdgePathInfo,
    edge_var: &str,
    expr: &Expr,
) -> Result<ShortestPathCost, PlannerError> {
    if single.label.is_none() && single.label_expr.is_none() {
        return Err(PlannerError::UnsupportedExtension(
            "GLEAPH.COST requires a single fixed edge label".into(),
        ));
    }
    validate_cost_expr_references_edge(expr, edge_var)?;
    if matches!(&expr.kind, ExprKind::Variable(_)) {
        return Err(PlannerError::UnsupportedExtension(
            "GLEAPH.COST expression must use GLEAPH.WEIGHT(edgeVar) or a numeric expression, not a bare edge variable".into(),
        ));
    }
    validate_cost_expr_shape(expr)?;
    validate_cost_expr_gleaph_weight_usage(expr, edge_var)?;

    Ok(ShortestPathCost::EdgeCostExpr {
        edge_var: edge_var.into(),
        expr: expr.clone(),
    })
}

fn plan_inline_property_cost_shortest_path(
    single: &gleaph_gql_planner::SingleEdgePathInfo,
    edge_var: &str,
    expr: &Expr,
) -> Result<ShortestPathCost, PlannerError> {
    if single.label_expr.is_some() {
        return Err(PlannerError::UnsupportedExtension(
            "COST BY e.property requires a single concrete edge label, not a label expression"
                .into(),
        ));
    }
    if single.label.is_none() {
        return Err(PlannerError::UnsupportedExtension(
            "COST BY e.property requires a single concrete edge label".into(),
        ));
    }

    let normalized = normalize_for_cost_property_access(expr);
    let property_name = match &normalized.kind {
        ExprKind::PropertyAccess {
            expr: base,
            property,
        } => {
            if let ExprKind::Variable(v) = &base.kind {
                if v != edge_var {
                    return Err(PlannerError::UnsupportedExtension(format!(
                        "COST BY e.property must reference the shortest-path edge variable '{edge_var}'"
                    )));
                }
                property.as_str()
            } else {
                return Err(PlannerError::UnsupportedExtension(
                    "COST BY e.property requires a direct property access on the edge variable"
                        .into(),
                ));
            }
        }
        ExprKind::Variable(_) => {
            return Err(PlannerError::UnsupportedExtension(
                "COST BY e.property requires a direct property access, not a bare variable".into(),
            ));
        }
        _ => {
            return Err(PlannerError::UnsupportedExtension(
                "COST BY e.property supports only a direct edge property access".into(),
            ));
        }
    };

    if property_name.is_empty() {
        return Err(PlannerError::UnsupportedExtension(
            "COST BY e.property requires a non-empty property name".into(),
        ));
    }

    Ok(ShortestPathCost::EdgeCostExpr {
        edge_var: edge_var.into(),
        expr: normalized,
    })
}

/// Strip harmless grouping around a candidate `COST BY` expression.
fn normalize_for_cost_property_access(expr: &Expr) -> Expr {
    let mut current = expr;
    while let ExprKind::Paren(inner) = &current.kind {
        current = inner;
    }
    current.clone()
}

fn is_gleaph_cost_extension_name(parts: &[String]) -> bool {
    GLEAPH_COST.matches_ascii_case_insensitive(parts)
}

fn is_cost_extension_name(parts: &[String]) -> bool {
    COST.matches_ascii_case_insensitive(parts)
}

fn validate_cost_expr_references_edge(expr: &Expr, edge_var: &str) -> Result<(), PlannerError> {
    let vars = gleaph_gql_planner::collect_expr_variables(expr);
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

fn is_numeric_cast_target(target: &gleaph_gql::ast::ValueType) -> bool {
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
    gleaph_gql_planner::for_each_immediate_child_expr(expr, |child| {
        if err.is_ok() {
            err = visit(child);
        }
    });
    err
}

/// Per-node rules for `GLEAPH.COST` expression shapes (child recursion is handled separately).
fn validate_cost_expr_node(expr: &Expr) -> Result<(), PlannerError> {
    use gleaph_gql::value::Value;
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

/// How [`GLEAPH.WEIGHT`] names an edge in an expression.
#[derive(Clone, Debug, PartialEq)]
pub enum GleaphWeightEdgeRef {
    /// Single-hop expand or shortest-path relax step.
    SingletonVar(String),
    /// Indexed element of a variable-length edge group (`e[-1]`, `e[0]`, …).
    /// Reachable only through the cypher list-index expression.
    #[cfg(feature = "cypher")]
    GroupElement { group_var: String, index: Box<Expr> },
}

/// True when `name` is an unqualified `GLEAPH.WEIGHT` function reference.
pub fn is_gleaph_weight_call(name: &ObjectName, distinct: bool) -> bool {
    !distinct && GLEAPH_WEIGHT.matches_ascii_case_insensitive(&name.parts)
}

/// Returns the single argument of a `GLEAPH.WEIGHT` call, if exactly one is present.
pub fn gleaph_weight_single_arg(args: &[Expr]) -> Option<&Expr> {
    if args.len() == 1 {
        Some(&args[0])
    } else {
        None
    }
}

/// Resolves the edge variable referenced by a `GLEAPH.WEIGHT` argument expression.
pub fn gleaph_weight_edge_ref(expr: &Expr) -> Option<GleaphWeightEdgeRef> {
    match &expr.kind {
        ExprKind::Paren(inner) => gleaph_weight_edge_ref(inner),
        ExprKind::Variable(v) => Some(GleaphWeightEdgeRef::SingletonVar(v.clone())),
        #[cfg(feature = "cypher")]
        ExprKind::ListIndex { list, index } => {
            let ExprKind::Variable(v) = &list.kind else {
                return None;
            };
            Some(GleaphWeightEdgeRef::GroupElement {
                group_var: v.clone(),
                index: index.clone(),
            })
        }
        _ => None,
    }
}

/// Returns the edge variable name referenced by a `GLEAPH.WEIGHT` argument, if any.
pub fn gleaph_weight_arg_edge_var(expr: &Expr) -> Option<String> {
    match gleaph_weight_edge_ref(expr)? {
        GleaphWeightEdgeRef::SingletonVar(v) => Some(v),
        #[cfg(feature = "cypher")]
        GleaphWeightEdgeRef::GroupElement { group_var, .. } => Some(group_var),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{
        BinaryOp, ExprKind, ObjectName, PathPatternExtension, PathPatternPrefix, SearchPrefix,
        ValueType,
    };
    use gleaph_gql::token::Span;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql::types::LabelExpr;
    use gleaph_gql_planner::SingleEdgePathInfo;
    use gleaph_gql_planner::plan::{ShortestMode, VarLenSpec};

    fn gleaph_cost_extension() -> ObjectName {
        ObjectName::qualified(vec!["GLEAPH".into(), "COST".into()])
    }

    fn cost_extension() -> ObjectName {
        ObjectName::simple("COST")
    }

    fn gleaph_weight(edge_var: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
            args: vec![Expr::var(edge_var)],
            distinct: false,
        })
    }

    fn cost_property(edge_var: &str, property: &str) -> Expr {
        Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::var(edge_var)),
            property: property.into(),
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
        GLEAPH_PATH_EXTENSION_HANDLER
            .plan_shortest_path_cost(&ctx(
                &[ext],
                Some(ShortestMode::AnyShortest),
                Some(single_edge()),
            ))
            .expect("should plan")
    }

    fn plan_cost(expr: Expr) -> ShortestPathCost {
        plan_cost_with_extension(expr, gleaph_cost_extension())
    }

    fn plan_cost_err(expr: Expr) -> PlannerError {
        let ext = PathPatternExtension {
            span: Span::DUMMY,
            name: gleaph_cost_extension(),
            expr,
        };
        GLEAPH_PATH_EXTENSION_HANDLER
            .plan_shortest_path_cost(&ctx(
                &[ext],
                Some(ShortestMode::AnyShortest),
                Some(single_edge()),
            ))
            .expect_err("should be rejected")
    }

    fn plan_inline_cost(expr: Expr) -> ShortestPathCost {
        plan_cost_with_extension(expr, cost_extension())
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
    fn gleaph_cost_accepts_gleaph_weight() {
        assert!(matches!(
            plan_cost(gleaph_weight("e")),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn inline_cost_accepts_direct_edge_property() {
        assert!(matches!(
            plan_inline_cost(cost_property("e", "distance")),
            ShortestPathCost::EdgeCostExpr { edge_var, .. } if &*edge_var == "e"
        ));
    }

    #[test]
    fn inline_cost_rejects_label_expression() {
        let mut edge = single_edge();
        edge.label = None;
        edge.label_expr = Some(LabelExpr::Or(
            Box::new(LabelExpr::Name("KNOWS".into())),
            Box::new(LabelExpr::Name("LIKES".into())),
        ));
        let ext = PathPatternExtension {
            span: Span::DUMMY,
            name: cost_extension(),
            expr: cost_property("e", "distance"),
        };
        let err = GLEAPH_PATH_EXTENSION_HANDLER
            .plan_shortest_path_cost(&ctx(&[ext], Some(ShortestMode::AnyShortest), Some(edge)))
            .expect_err("should be rejected");
        assert!(err.to_string().contains("label expression"), "{err}");
    }

    #[test]
    fn gleaph_cost_rejects_bare_edge_variable() {
        let err = plan_cost_err(Expr::var("e"));
        assert!(err.to_string().contains("bare edge variable"), "{err}");
    }

    #[test]
    fn cost_expr_shape_rejects_non_numeric_literal() {
        let expr = Expr::new(ExprKind::BinaryOp {
            left: Box::new(gleaph_weight("e")),
            op: BinaryOp::Add,
            right: Box::new(Expr::new(ExprKind::Literal(
                gleaph_gql::value::Value::Text("x".into()),
            ))),
        });
        let err = plan_cost_err(expr);
        assert!(err.to_string().contains("numeric"), "{err}");
    }

    #[test]
    fn cost_expr_shape_accepts_cast_wrapped_weight() {
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
    fn is_gleaph_weight_call_recognizes_weight() {
        let name = ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]);
        assert!(is_gleaph_weight_call(&name, false));
        assert!(!is_gleaph_weight_call(&name, true));
    }

    #[cfg(feature = "cypher")]
    #[test]
    fn gleaph_weight_edge_ref_recognizes_group_element() {
        use gleaph_gql::value::Value;
        let list = Expr::var("e");
        let index = Expr::new(ExprKind::Literal(Value::Int64(-1)));
        let expr = Expr::new(ExprKind::ListIndex {
            list: Box::new(list),
            index: Box::new(index),
        });
        assert!(
            matches!(
                gleaph_weight_edge_ref(&expr),
                Some(GleaphWeightEdgeRef::GroupElement { group_var, .. })
                if group_var == "e"
            ),
            "expected e[-1] to resolve to group element edge ref"
        );
    }
}
