//! Gleaph-specific integration between generic GQL parsing/planning and Graph/Router execution.
//!
//! This crate sits between portable GQL planning and Gleaph execution. It intentionally does not own
//! storage, canister calls, or generic GQL syntax. Its public modules are narrowly scoped:
//!
//! - `path_extension::GLEAPH_PATH_EXTENSION_HANDLER` translates Gleaph path-extension clauses
//!   (e.g. `GLEAPH COST`) into planner concepts consumed by Router and Graph planning;
//! - `weight::GleaphWeightEdgeRef` and `weight::is_gleaph_weight_call` provide pure expression-shape
//!   classification for `GLEAPH WEIGHT(...)` calls consumed by Graph execution.
//!
//! `gleaph_gql` and `gleaph_gql_planner` remain portable and gain no Router/Graph/ICP concepts.

pub mod path_extension;
pub mod weight;

#[cfg(test)]
#[allow(dead_code)]
mod test_support {
    use gleaph_gql::ast::{Expr, ExprKind, ObjectName, PathPatternExtension};
    use gleaph_gql::token::Span;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql_planner::plan::{ShortestMode, VarLenSpec};
    use gleaph_gql_planner::{PathPatternExtensionHandler, SingleEdgePathInfo};

    pub fn gleaph_cost_extension() -> ObjectName {
        ObjectName::qualified(vec!["GLEAPH".into(), "COST".into()])
    }

    pub fn cost_extension() -> ObjectName {
        ObjectName::simple("COST")
    }

    pub fn gleaph_weight(edge_var: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
            args: vec![Expr::var(edge_var)],
            distinct: false,
        })
    }

    pub fn cost_property(edge_var: &str, property: &str) -> Expr {
        Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::var(edge_var)),
            property: property.into(),
        })
    }

    pub fn single_edge() -> SingleEdgePathInfo {
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

    pub fn ctx(
        extensions: &[PathPatternExtension],
    ) -> gleaph_gql_planner::PathPatternExtensionContext<'_> {
        gleaph_gql_planner::PathPatternExtensionContext {
            prefix: None,
            extensions,
            shortest_mode: Some(ShortestMode::AnyShortest),
            single_edge: Some(single_edge()),
        }
    }

    pub fn plan_cost_with_extension(
        expr: Expr,
        extension_name: ObjectName,
    ) -> gleaph_gql_planner::plan::ShortestPathCost {
        let ext = PathPatternExtension {
            span: Span::DUMMY,
            name: extension_name,
            expr,
        };
        crate::path_extension::GLEAPH_PATH_EXTENSION_HANDLER
            .plan_shortest_path_cost(&ctx(&[ext]))
            .expect("plan cost")
    }

    pub fn plan_cost(expr: Expr) -> gleaph_gql_planner::plan::ShortestPathCost {
        plan_cost_with_extension(expr, gleaph_cost_extension())
    }
}
