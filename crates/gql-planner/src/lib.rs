//! GQL query planner for the Gleaph graph database.
//!
//! This crate converts parsed GQL ASTs (from `gleaph-gql`) into physical
//! execution plans. It performs cost-based anchor selection, filter/limit
//! pushdown, and produces human-readable explain output.
//!
//! # Usage
//!
//! ```rust,no_run
//! use gleaph_gql::parser;
//! use gleaph_gql_planner::{build_plan, explain_plan};
//!
//! let program = parser::parse("MATCH (n:User) RETURN n.name").unwrap();
//! // Extract the query from the program...
//! ```
//!
//! The executor (which actually runs plans against a graph) lives in a
//! separate crate.

pub mod anchor;
pub mod cost;
pub mod cse;
pub mod executor_contract;
pub mod explain;
mod expr_alias;
pub mod expr_children;
pub mod join_order;
pub mod path_extensions;
pub mod plan;
pub mod planner;
pub mod property_projection;
pub mod pushdown;
pub mod semantic;
pub mod stats;

#[cfg(feature = "plan-wire")]
pub mod wire;

mod variable_refs;

// Re-export key types and functions.
pub use executor_contract::first_executor_unsupported_op;
pub use explain::explain_plan;
pub use expr_children::for_each_immediate_child_expr;
pub use path_extensions::{
    PathPatternExtensionContext, PathPatternExtensionHandler, PlanBuildOptions,
    RejectingPathExtensionHandler, SingleEdgePathInfo,
};
pub use plan::{
    PhysicalPlan, PlanAnnotations, PlanDiagnostics, PlanOp, PlanSummary, ShortestPathCost,
    UseGraphPushdownInfo,
};
pub use planner::{
    PlanBuildOutput, PlannerError, analyze_remote_use_graph_pushdown, build_block_plan,
    build_block_plan_output, build_block_plan_output_for_execute,
    build_block_plan_output_for_execute_with_schema, build_block_plan_output_with_schema,
    build_block_plan_with_schema, build_composite_plan, build_composite_plan_output,
    build_composite_plan_output_with_schema, build_composite_plan_with_schema, build_plan,
    build_plan_output, build_plan_output_for_execute, build_plan_output_for_execute_with_schema,
    build_plan_output_with_schema, build_plan_with_schema, build_plan_with_schema_and_options,
    build_statement_plan, build_statement_plan_output, build_statement_plan_output_with_schema,
    build_statement_plan_with_options, build_statement_plan_with_schema,
};
pub use pushdown::collect_variables as collect_expr_variables;
pub use stats::{GraphStats, NoStats, TableStats};
