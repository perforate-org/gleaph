//! Gleaph-specific path-extension planning integration.
//!
//! Translates generic GQL path-pattern extension clauses (e.g. `GLEAPH.COST`) into planner
//! concepts consumed by both Router and Graph planning.

use gleaph_gql::ast::{Expr, ExprKind};
use gleaph_gql_planner::{
    PathPatternExtensionContext, PathPatternExtensionHandler, PlannerError, ShortestPathCost,
};
use gleaph_graph_kernel::gql_dialect::{COST, GLEAPH_COST};

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
    // GLEAPH.COST is now an alias for the standard COST BY e.property path.
    // The legacy GLEAPH.WEIGHT(e) compatibility surface has been removed.
    plan_inline_property_cost_shortest_path(single, edge_var, expr)
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

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{
        ExprKind, ObjectName, PathPatternExtension, PathPatternPrefix, SearchPrefix,
    };
    use gleaph_gql::token::Span;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql::types::LabelExpr;
    use gleaph_gql_planner::plan::{ShortestMode, VarLenSpec};
    use gleaph_gql_planner::{PathPatternExtensionHandler, SingleEdgePathInfo};

    fn gleaph_cost_extension() -> ObjectName {
        ObjectName::qualified(vec!["GLEAPH".into(), "COST".into()])
    }

    fn cost_extension() -> ObjectName {
        ObjectName::simple("COST")
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
        assert!(err.to_string().contains("bare variable"), "{err}");
    }
}
