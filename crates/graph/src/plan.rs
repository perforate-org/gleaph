//! Physical plan mutation execution against [`crate::facade::GraphStore`].

mod expr_evaluator;
pub mod mutation;
pub mod query;

pub use mutation::{
    MutationPropertyExprEvaluation, MutationPropertyExprEvaluator, PlanMutationBindings,
    PlanMutationError, PlanMutationExecutor, execute_ops,
};
#[cfg(not(target_family = "wasm"))]
pub use query::PlanQueryExecutor;
pub use query::{EdgeBinding, PlanBinding, PlanQueryError, PlanQueryResult, execute_plan_query};
