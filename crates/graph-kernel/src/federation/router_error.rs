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
    /// GQL program kind (read vs write) does not match the canister entrypoint (query vs update).
    #[error(
        "execution path mismatch on {entrypoint}: {program_kind} program cannot use {call_kind} call; {remedy}"
    )]
    ExecutionPathMismatch {
        entrypoint: String,
        program_kind: String,
        call_kind: String,
        remedy: String,
    },
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
    #[error("vertex is migrating")]
    VertexMigrating,
    #[error("vertex is not migrating")]
    VertexNotMigrating,
    #[error("invalid migration state: {0}")]
    InvalidMigrationState(String),
    #[error("id exhausted: {0}")]
    IdExhausted(String),
    #[error("internal: {0}")]
    Internal(String),
}
