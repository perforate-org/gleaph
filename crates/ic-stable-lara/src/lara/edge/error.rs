//! Edge subsystem init errors.

use super::{
    counts::InitError as CountsInitError, edges::InitError as SlabInitError,
    span_meta::InitError as SpanMetaInitError,
};
use std::fmt;

/// Errors returned when reopening the full edge storage subsystem.
#[derive(Debug)]
pub enum InitError {
    /// The edge subsystem could not allocate its initial metadata.
    OutOfMemory,
    /// The PMA count tree could not be reopened.
    Counts(CountsInitError),
    /// The edge slab could not be reopened.
    Edges(SlabInitError),

    /// Segment span metadata could not be reopened.
    SpanMeta(SpanMetaInitError),
    /// The overflow log was created for a different edge layout.
    LogLayoutMismatch,
    /// Segment span metadata length does not match the edge layout.
    SpanMetaLayoutMismatch,
    /// The backing memories are partially initialized (some regions are empty
    /// while others are populated), so the store must not be reopened or recreated.
    PartialLayout,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfMemory => write!(f, "failed to allocate edge subsystem metadata"),
            Self::Counts(e) => write!(f, "counts init failed: {e}"),
            Self::Edges(e) => write!(f, "edge slab init failed: {e}"),
            Self::SpanMeta(e) => write!(f, "span metadata init failed: {e}"),
            Self::LogLayoutMismatch => {
                write!(f, "overflow log layout does not match the edge layout")
            }
            Self::SpanMetaLayoutMismatch => {
                write!(f, "span metadata length does not match the edge layout")
            }
            Self::PartialLayout => {
                write!(
                    f,
                    "edge memories are partially initialized; refusing to reopen"
                )
            }
        }
    }
}

impl std::error::Error for InitError {}
