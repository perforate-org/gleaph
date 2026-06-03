use crate::facade::GraphStoreError;
use crate::gql_execution_context::RuntimeFunctionError;
use crate::plan::expr_evaluator::ExprEvaluationError;
use gleaph_gql::types::EdgeDirection;
use std::fmt;

#[derive(Debug)]
pub enum PlanQueryError {
    Store(GraphStoreError),
    /// Inter-canister call to `gleaph-graph-index` failed (includes `detail` for operators).
    FederatedIndexCall {
        op: &'static str,
        detail: String,
    },
    UnsupportedOp(&'static str),
    UnsupportedDirection(EdgeDirection),
    UnsupportedExpression {
        expression: String,
    },
    MissingBinding {
        variable: String,
    },
    MissingParameter {
        name: String,
    },
    MissingResolvedLabel {
        namespace: &'static str,
        name: String,
    },
    InvalidExpressionValue {
        expression: String,
    },
    ExpressionDivisionByZero {
        expression: String,
    },
    ExpressionNumericOverflow {
        expression: String,
    },
    ExpressionNumericPrecisionOverflow {
        expression: String,
    },
    ExpressionNonFiniteNumeric {
        expression: String,
    },
    ExpressionIncomparableValues {
        expression: String,
    },
    ExpressionUnsupportedNumericConversion {
        expression: String,
    },
    InvalidLimit {
        value: gleaph_gql::Value,
    },
    IncomparableSortValues {
        left: gleaph_gql::Value,
        right: gleaph_gql::Value,
    },
    RuntimeFunction(RuntimeFunctionError),
    /// `GLEAPH.WEIGHT` preparation or evaluation failed (message is user-facing).
    GleaphWeight {
        message: String,
    },
    /// `GLEAPH.COST` shortest-path edge cost evaluation failed (message is user-facing).
    GleaphCost {
        message: String,
    },
    /// `GLEAPH.SEQUENCE` order preparation failed (message is user-facing).
    GleaphSequence {
        message: String,
    },
}

impl fmt::Display for PlanQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(err) => write!(f, "{err}"),
            Self::FederatedIndexCall { op, detail } => {
                write!(f, "federated index {op} failed: {detail}")
            }
            Self::UnsupportedOp(op) => write!(f, "unsupported plan query operator: {op}"),
            Self::UnsupportedDirection(direction) => {
                write!(f, "unsupported query edge direction: {direction:?}")
            }
            Self::UnsupportedExpression { expression } => {
                write!(f, "unsupported query expression: {expression}")
            }
            Self::MissingBinding { variable } => write!(f, "missing binding for '{variable}'"),
            Self::MissingParameter { name } => write!(f, "missing parameter '{name}'"),
            Self::MissingResolvedLabel { namespace, name } => {
                write!(f, "missing router-resolved {namespace} label '{name}'")
            }
            Self::InvalidExpressionValue { expression } => {
                write!(f, "invalid query expression value for '{expression}'")
            }
            Self::ExpressionDivisionByZero { expression } => {
                write!(f, "division by zero in query expression for '{expression}'")
            }
            Self::ExpressionNumericOverflow { expression } => {
                write!(f, "numeric overflow in query expression for '{expression}'")
            }
            Self::ExpressionNumericPrecisionOverflow { expression } => {
                write!(
                    f,
                    "numeric precision overflow in query expression for '{expression}'"
                )
            }
            Self::ExpressionNonFiniteNumeric { expression } => {
                write!(
                    f,
                    "non-finite float result in query expression for '{expression}'"
                )
            }
            Self::ExpressionIncomparableValues { expression } => {
                write!(
                    f,
                    "incomparable values in query expression for '{expression}'"
                )
            }
            Self::ExpressionUnsupportedNumericConversion { expression } => {
                write!(
                    f,
                    "unsupported numeric conversion in query expression for '{expression}'"
                )
            }
            Self::InvalidLimit { value } => write!(f, "invalid LIMIT/OFFSET value: {value:?}"),
            Self::IncomparableSortValues { left, right } => {
                write!(f, "incomparable ORDER BY values: {left:?} and {right:?}")
            }
            Self::RuntimeFunction(err) => write!(f, "{err}"),
            Self::GleaphWeight { message } => write!(f, "{message}"),
            Self::GleaphCost { message } => write!(f, "{message}"),
            Self::GleaphSequence { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for PlanQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            _ => None,
        }
    }
}

impl From<GraphStoreError> for PlanQueryError {
    fn from(value: GraphStoreError) -> Self {
        Self::Store(value)
    }
}

impl From<RuntimeFunctionError> for PlanQueryError {
    fn from(value: RuntimeFunctionError) -> Self {
        Self::RuntimeFunction(value)
    }
}

impl From<ExprEvaluationError> for PlanQueryError {
    fn from(value: ExprEvaluationError) -> Self {
        match value {
            ExprEvaluationError::InvalidValue => Self::InvalidExpressionValue {
                expression: "query".to_owned(),
            },
            ExprEvaluationError::DivisionByZero => Self::ExpressionDivisionByZero {
                expression: "query".to_owned(),
            },
            ExprEvaluationError::NumericOverflow => Self::ExpressionNumericOverflow {
                expression: "query".to_owned(),
            },
            ExprEvaluationError::NumericPrecisionOverflow => {
                Self::ExpressionNumericPrecisionOverflow {
                    expression: "query".to_owned(),
                }
            }
            ExprEvaluationError::NonFiniteNumeric => Self::ExpressionNonFiniteNumeric {
                expression: "query".to_owned(),
            },
            ExprEvaluationError::IncomparableValues => Self::ExpressionIncomparableValues {
                expression: "query".to_owned(),
            },
            ExprEvaluationError::UnsupportedNumericConversion => {
                Self::ExpressionUnsupportedNumericConversion {
                    expression: "query".to_owned(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::expr_evaluator::ExprEvaluationError;

    #[test]
    fn expr_precision_overflow_maps_to_plan_query_error() {
        let err: PlanQueryError = ExprEvaluationError::NumericPrecisionOverflow.into();
        assert!(matches!(
            err,
            PlanQueryError::ExpressionNumericPrecisionOverflow { .. }
        ));
    }

    #[test]
    fn expr_numeric_overflow_maps_to_plan_query_error() {
        let err: PlanQueryError = ExprEvaluationError::NumericOverflow.into();
        assert!(matches!(
            err,
            PlanQueryError::ExpressionNumericOverflow { .. }
        ));
    }
}
