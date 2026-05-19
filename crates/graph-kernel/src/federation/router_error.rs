//! Router control-plane error type (Candid wire).

use candid::CandidType;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize, thiserror::Error)]
pub enum RouterError {
    #[error("not authorized")]
    NotAuthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("graph unavailable")]
    GraphUnavailable,
    #[error("shard not registered")]
    ShardNotRegistered,
    #[error("shard already registered")]
    ShardAlreadyRegistered,
    #[error("vertex not found")]
    VertexNotFound,
    #[error("placement already committed")]
    PlacementAlreadyCommitted,
    #[error("unallocated logical vertex")]
    UnallocatedLogicalVertex,
    #[error("internal: {0}")]
    Internal(String),
}
