use std::error::Error;
use std::fmt;

use super::*;

/// Facade-level error type for the rewrite entrypoint.
///
/// This keeps the higher-level facade ergonomic without erasing the low-level
/// failure modes that still matter during the rewrite phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RewriteGraphPmaError {
    /// Stable-memory hydration failed.
    Hydration(HydrationError),
    /// Stable-memory writeback failed.
    Writeback(WritebackError),
    /// Property-store hydration or writeback failed.
    PropertyStore(PropertyStoreError),
    /// Property-index hydration or writeback failed.
    PropertyIndex(PropertyIndexError),
    /// Caller-supplied semantic edge ids did not match the current forward-side layout.
    InvalidLocatorInputs,
}

/// Facade-level result alias for the rewrite entrypoint.
pub type RewriteGraphPmaResult<T> = Result<T, RewriteGraphPmaError>;

impl fmt::Display for RewriteGraphPmaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hydration(err) => write!(f, "rewrite graph-pma hydration failed: {err}"),
            Self::Writeback(err) => write!(f, "rewrite graph-pma writeback failed: {err}"),
            Self::PropertyStore(err) => write!(f, "rewrite property-store operation failed: {err}"),
            Self::PropertyIndex(err) => write!(f, "rewrite property-index operation failed: {err}"),
            Self::InvalidLocatorInputs => {
                write!(f, "invalid locator rebuild inputs for forward surface")
            }
        }
    }
}

impl Error for RewriteGraphPmaError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Hydration(err) => Some(err),
            Self::Writeback(err) => Some(err),
            Self::PropertyStore(err) => Some(err),
            Self::PropertyIndex(err) => Some(err),
            Self::InvalidLocatorInputs => None,
        }
    }
}

impl From<HydrationError> for RewriteGraphPmaError {
    fn from(value: HydrationError) -> Self {
        Self::Hydration(value)
    }
}

impl From<WritebackError> for RewriteGraphPmaError {
    fn from(value: WritebackError) -> Self {
        Self::Writeback(value)
    }
}

impl From<PropertyStoreError> for RewriteGraphPmaError {
    fn from(value: PropertyStoreError) -> Self {
        Self::PropertyStore(value)
    }
}

impl From<PropertyIndexError> for RewriteGraphPmaError {
    fn from(value: PropertyIndexError) -> Self {
        Self::PropertyIndex(value)
    }
}
