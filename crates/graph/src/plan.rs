//! Physical plan mutation execution against [`crate::facade::GraphStore`].

mod mutation_error;
mod mutation_executor;
mod property_expr_evaluator;

pub use mutation_error::PlanMutationError;
pub use mutation_executor::{execute_ops, PlanMutationBindings, PlanMutationExecutor};
pub use property_expr_evaluator::{PlanPropertyExprEvaluation, PlanPropertyExprEvaluator};
