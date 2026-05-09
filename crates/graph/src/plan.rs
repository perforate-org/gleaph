//! Physical plan mutation execution against [`crate::facade::GraphStore`].

mod expr_evaluator;
pub mod mutation;
pub mod query;

pub use mutation::{
    MutationPropertyExprEvaluation, MutationPropertyExprEvaluator, PlanMutationBindings,
    PlanMutationError, PlanMutationExecutor, execute_ops,
};
pub use query::{
    PlanBinding, PlanQueryError, PlanQueryExecutor, PlanQueryResult, execute_plan_query,
};
