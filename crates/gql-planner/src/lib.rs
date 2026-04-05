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
pub mod join_order;
pub mod plan;
pub mod planner;
pub mod property_projection;
pub mod pushdown;
pub mod semantic;
pub mod stats;

mod variable_refs;

// Re-export key types and functions.
pub use executor_contract::first_executor_unsupported_op;
pub use explain::explain_plan;
pub use plan::{
    PhysicalPlan, PlanAnnotations, PlanDiagnostics, PlanOp, PlanSummary, UseGraphPushdownInfo,
};
pub use planner::{
    PlanBuildOutput, PlannerError, analyze_remote_use_graph_pushdown, build_block_plan,
    build_block_plan_output, build_block_plan_output_for_execute,
    build_block_plan_output_for_execute_with_schema, build_block_plan_output_with_schema,
    build_block_plan_with_schema,
    build_composite_plan, build_composite_plan_output, build_composite_plan_output_with_schema,
    build_composite_plan_with_schema, build_plan, build_plan_output, build_plan_output_for_execute,
    build_plan_output_for_execute_with_schema, build_plan_output_with_schema, build_plan_with_schema,
    build_statement_plan, build_statement_plan_output, build_statement_plan_output_with_schema,
    build_statement_plan_with_schema,
};
pub use stats::{GraphStats, NoStats, TableStats};
