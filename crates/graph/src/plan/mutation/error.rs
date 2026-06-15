use crate::facade::GraphStoreError;
use crate::gql_execution_context::RuntimeFunctionError;
use gleaph_gql::types::EdgeDirection;
use std::fmt;

#[derive(Debug)]
pub enum PlanMutationError {
    Store(GraphStoreError),
    UnsupportedOp(&'static str),
    UnsupportedDirection(EdgeDirection),
    MissingVertexBinding {
        variable: String,
    },
    MissingElementBinding {
        variable: String,
    },
    UnsupportedExpression {
        property: String,
    },
    /// Operand type or shape is invalid for the expression (e.g. `NOT` on a string).
    InvalidExpressionValue {
        property: String,
    },
    ExpressionDivisionByZero {
        property: String,
    },
    ExpressionNumericOverflow {
        property: String,
    },
    ExpressionNumericPrecisionOverflow {
        property: String,
    },
    ExpressionNonFiniteNumeric {
        property: String,
    },
    ExpressionIncomparableValues {
        property: String,
    },
    ExpressionUnsupportedNumericConversion {
        property: String,
    },
    InvalidPropertyReplacement {
        variable: String,
    },
    UnsupportedSetItem(&'static str),
    UnsupportedRemoveItem(&'static str),
    MissingParameter {
        name: String,
    },
    MissingResolvedLabel {
        namespace: &'static str,
        name: String,
    },
    MissingResolvedProperty {
        name: String,
    },
    RuntimeFunction(RuntimeFunctionError),
    UnknownGleaphProcedure {
        name: String,
    },
    InvalidFinalizeProcedureArgs {
        procedure: &'static str,
        expected: &'static str,
    },
    InvalidFinalizeVertexListArg,
    TooManyFinalizeVertices {
        count: usize,
        max: usize,
    },
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
                write!(
                    f,
                    "division by zero in property expression for '{property}'"
                )
            }
            Self::ExpressionNumericOverflow { property } => {
                write!(
                    f,
                    "numeric overflow in property expression for '{property}'"
                )
            }
            Self::ExpressionNumericPrecisionOverflow { property } => {
                write!(
                    f,
                    "numeric precision overflow in property expression for '{property}'"
                )
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
            Self::InvalidPropertyReplacement { variable } => {
                write!(f, "SET {variable} = ... requires a record value")
            }
            Self::UnsupportedSetItem(item) => write!(f, "unsupported SET item: {item}"),
            Self::UnsupportedRemoveItem(item) => write!(f, "unsupported REMOVE item: {item}"),
            Self::MissingParameter { name } => write!(f, "missing parameter '{name}'"),
            Self::MissingResolvedLabel { namespace, name } => {
                write!(f, "missing router-resolved {namespace} label '{name}'")
            }
            Self::MissingResolvedProperty { name } => {
                write!(f, "missing router-resolved property '{name}'")
            }
            Self::RuntimeFunction(err) => write!(f, "{err}"),
            Self::UnknownGleaphProcedure { name } => {
                write!(f, "unknown Gleaph procedure: {name}")
            }
            Self::InvalidFinalizeProcedureArgs {
                procedure,
                expected,
            } => {
                write!(f, "{procedure} expects {expected}")
            }
            Self::InvalidFinalizeVertexListArg => {
                write!(f, "GLEAPH.VERTEX_LIST expects bound node variables")
            }
            Self::TooManyFinalizeVertices { count, max } => {
                write!(f, "finalize vertex list too long: {count} > {max}")
            }
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

impl From<RuntimeFunctionError> for PlanMutationError {
    fn from(value: RuntimeFunctionError) -> Self {
        Self::RuntimeFunction(value)
    }
}

impl From<GraphStoreError> for PlanMutationError {
    fn from(value: GraphStoreError) -> Self {
        Self::Store(value)
    }
}
