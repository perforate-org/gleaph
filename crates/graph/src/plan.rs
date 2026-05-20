//! Physical plan mutation execution against [`crate::facade::GraphStore`].

mod expr_evaluator;
mod ic_wire;
pub mod mutation;
pub mod query;

pub use ic_wire::{IcWirePlanQueryResult, IcWirePlanQueryRow};
pub use mutation::{
    MutationPropertyExprEvaluation, MutationPropertyExprEvaluator, PlanMutationBindings,
    PlanMutationError, PlanMutationExecutor, execute_ops,
};
pub use query::PlanQueryExecutor;
pub use query::{
    EdgeBinding, PathBinding, PlanBinding, PlanQueryBindings, PlanQueryError, PlanQueryResult,
    PlanQueryRow, execute_plan_query, execute_plan_query_bindings,
    execute_plan_query_bindings_with_initial_rows, hydrate_plan_rows, materialize_plan_rows,
    materialize_plan_rows_for_schema,
};
