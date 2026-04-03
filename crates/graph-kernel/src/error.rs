use std::error::Error;

use thiserror::Error;

use crate::{EdgeId, NodeId};

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("node {0} not found")]
    NodeNotFound(NodeId),
    #[error("edge {0} not found")]
    EdgeNotFound(EdgeId),
    /// Property append-log / stable-store failure (encoding, regions, bucket chain, etc.).
    ///
    /// `message` mirrors `source.to_string()` at construction time; use [`Error::source`] to
    /// downcast the original error type from the graph integration layer.
    #[error("property store: {message}")]
    PropertyStore {
        message: String,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// Property equality index build or sync failure (distinct from append-log payload errors).
    ///
    /// Downcast `source` to the concrete index error type from the graph integration layer.
    #[error("property index: {message}")]
    PropertyIndex {
        message: String,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    #[error("{0}")]
    Message(String),
}

/// Coarse classification for [`GraphError`] without matching on payloads.
///
/// Upper layers (executor, gleaph) can branch on this without depending on every
/// [`GraphError`] variant's fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GraphErrorKind {
    NodeNotFound,
    EdgeNotFound,
    PropertyStore,
    PropertyIndex,
    Message,
}

impl GraphError {
    pub fn kind(&self) -> GraphErrorKind {
        match self {
            GraphError::NodeNotFound(_) => GraphErrorKind::NodeNotFound,
            GraphError::EdgeNotFound(_) => GraphErrorKind::EdgeNotFound,
            GraphError::PropertyStore { .. } => GraphErrorKind::PropertyStore,
            GraphError::PropertyIndex { .. } => GraphErrorKind::PropertyIndex,
            GraphError::Message(_) => GraphErrorKind::Message,
        }
    }

    /// Wraps one property-store error while preserving it as [`Error::source`].
    pub fn property_store(source: impl Error + Send + Sync + 'static) -> Self {
        let message = source.to_string();
        Self::PropertyStore {
            message,
            source: Box::new(source),
        }
    }

    /// Wraps one property-index error while preserving it as [`Error::source`].
    pub fn property_index(source: impl Error + Send + Sync + 'static) -> Self {
        let message = source.to_string();
        Self::PropertyIndex {
            message,
            source: Box::new(source),
        }
    }
}

pub type GraphResult<T> = Result<T, GraphError>;
