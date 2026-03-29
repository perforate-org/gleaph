//! GQL error types.

/// Unified error type for the GQL crate.
#[derive(Clone, Debug, thiserror::Error)]
pub enum GqlError {
    /// Syntax / parse error.
    #[error("parse error: {0}")]
    Parse(String),

    /// Semantic validation error (e.g. undeclared variable, invalid scope).
    #[error("validation error: {0}")]
    Validation(String),

    /// Static type error detected during analysis.
    #[error("type error: {0}")]
    TypeError(String),
}

/// Convenience alias used throughout the crate.
pub type GqlResult<T> = Result<T, GqlError>;
