//! Physical plan mutation execution against [`crate::facade::GraphStore`].

mod expr_evaluator;
mod ic_wire;
pub mod mutation;
pub mod query;

pub use ic_wire::{
    IcWirePlanQueryResult, IcWirePlanQueryRow, ic_wire_from_plan_query_result,
    plan_query_result_from_ic_wire,
};
pub use mutation::{
    MutationPropertyExprEvaluation, MutationPropertyExprEvaluator, PlanMutationBindings,
    PlanMutationError, PlanMutationExecutor, execute_ops,
};
pub use query::PlanQueryExecutor;
pub use query::{
    EdgeBinding, PathBinding, PlanBinding, PlanQueryBindings, PlanQueryError, PlanQueryResult,
    PlanQueryRow, empty_row_for_plan, execute_plan_query, execute_plan_query_bindings,
    execute_plan_query_bindings_with_initial_rows, hydrate_plan_rows, materialize_plan_rows,
    materialize_plan_rows_for_schema,
};
