use crate::facade::GraphStoreError;
use gleaph_gql::types::EdgeDirection;
use std::fmt;

#[derive(Debug)]
pub enum PlanMutationError {
    Store(GraphStoreError),
    UnsupportedOp(&'static str),
    UnsupportedDirection(EdgeDirection),
    MissingVertexBinding { variable: String },
    MissingElementBinding { variable: String },
    UnsupportedExpression { property: String },
    /// Operand type or shape is invalid for the expression (e.g. `NOT` on a string).
    InvalidExpressionValue { property: String },
    ExpressionDivisionByZero { property: String },
    ExpressionNumericOverflow { property: String },
    ExpressionNonFiniteNumeric { property: String },
    ExpressionIncomparableValues { property: String },
    ExpressionUnsupportedNumericConversion { property: String },
    UnsupportedSetItem(&'static str),
    UnsupportedRemoveItem(&'static str),
    MissingParameter { name: String },
}

impl fmt::Display for PlanMutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(err) => write!(f, "{err}"),
            Self::UnsupportedOp(op) => write!(f, "unsupported plan mutation operator: {op}"),
            Self::UnsupportedDirection(direction) => {
                write!(f, "unsupported insert edge direction: {direction:?}")
            }
            Self::MissingVertexBinding { variable } => {
                write!(f, "missing vertex binding for '{variable}'")
            }
            Self::MissingElementBinding { variable } => {
                write!(f, "missing graph element binding for '{variable}'")
            }
            Self::UnsupportedExpression { property } => {
                write!(f, "unsupported property expression for '{property}'")
            }
            Self::InvalidExpressionValue { property } => {
                write!(f, "invalid property expression value for '{property}'")
            }
            Self::ExpressionDivisionByZero { property } => {
                write!(f, "division by zero in property expression for '{property}'")
            }
            Self::ExpressionNumericOverflow { property } => {
                write!(f, "numeric overflow in property expression for '{property}'")
            }
            Self::ExpressionNonFiniteNumeric { property } => {
                write!(
                    f,
                    "non-finite float result in property expression for '{property}'"
                )
            }
            Self::ExpressionIncomparableValues { property } => {
                write!(
                    f,
                    "incomparable values in property expression comparison for '{property}'"
                )
            }
            Self::ExpressionUnsupportedNumericConversion { property } => {
                write!(
                    f,
                    "unsupported numeric conversion in property expression for '{property}'"
                )
            }
            Self::UnsupportedSetItem(item) => write!(f, "unsupported SET item: {item}"),
            Self::UnsupportedRemoveItem(item) => write!(f, "unsupported REMOVE item: {item}"),
            Self::MissingParameter { name } => write!(f, "missing parameter '{name}'"),
        }
    }
}

impl std::error::Error for PlanMutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            _ => None,
        }
    }
}

impl From<GraphStoreError> for PlanMutationError {
    fn from(value: GraphStoreError) -> Self {
        Self::Store(value)
    }
}
