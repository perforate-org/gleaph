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
    #[error(
        "graph context mismatch: API `{api_graph}` does not match GQL-resolved graph `{resolved_graph}`"
    )]
    GraphContextMismatch {
        api_graph: String,
        resolved_graph: String,
    },
    #[error("shard not registered")]
    ShardNotRegistered,
    /// A `ReadMode::AtLeast(token)` read could not be served because a shard projection
    /// has not yet reached the token's watermark (ADR 0029 §5, Phase 3). This is
    /// **retryable**: the caller should retry after the projection drains. No stale state
    /// is served.
    #[error(
        "projection lag on shard {shard_id} ({watermark}): required {required}, current {current}; retry"
    )]
    ProjectionLag {
        shard_id: u32,
        watermark: String,
        required: u64,
        current: u64,
    },
    /// A federated update bundle contained more than one top-level DML statement. Federated
    /// multi-DML bundles have no defined cross-shard partial-application contract yet
    /// (ADR 0029 Phase 5), so the bundle is rejected before any shard dispatch — no canonical
    /// or projection state changed. This is **not** retryable as written: split the bundle into
    /// single-statement mutations, or target a single-shard graph. Resubmitting the same bundle
    /// is rejected identically.
    #[error(
        "unsupported federated multi-DML bundle: {dml_statements} top-level DML statements across \
         {shard_count} shards; no cross-shard bundle contract is implemented (ADR 0029 Phase 5). \
         Split into single-statement mutations or target a single-shard graph"
    )]
    UnsupportedMultiDmlBundle {
        dml_statements: u32,
        shard_count: u32,
    },
    #[error("shard already registered")]
    ShardAlreadyRegistered,
    #[error("id exhausted: {0}")]
    IdExhausted(String),
    /// A uniqueness constraint would be violated (ADR 0030). **Not retryable**: the value is already
    /// committed, or the same mutation claims one value twice. Resubmitting is rejected identically.
    #[error("uniqueness violation: {0}")]
    UniquenessViolation(String),
    /// A uniqueness value is claimed by an in-flight or reclaiming mutation (ADR 0030).
    /// **Retryable**: retry after the holding saga resolves (it then either frees the value or this
    /// retry gets `UniquenessViolation`).
    #[error("uniqueness reservation in flight: {0}")]
    UniquenessReservationInFlight(String),
    /// A recognized operation whose implementation is intentionally inactive (e.g. a feature
    /// landed in slices and not yet end-to-end). Distinct from `InvalidArgument`: the request is
    /// well-formed, but the capability is not yet published. Not retryable.
    #[error("not implemented: {0}")]
    NotImplemented(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// Wire-error prefix a graph shard's `ShardLocalGlobal` uniqueness violation carries back to the
/// Router across the `Result<_, String>` execute-plan boundary (ADR 0030 slice 10). Unlike the
/// `FederatedTcc` path — where the Router detects duplicates at its own reservation Try — a
/// `ShardLocalGlobal` duplicate is detected on the owning shard, so the Router recognizes this prefix
/// to re-type the returned string as [`RouterError::UniquenessViolation`] (non-retryable) instead of
/// a generic `InvalidArgument`. Kept in lockstep with the `UniquenessViolation` `Display` by
/// `uniqueness_violation_prefix_matches_display`.
pub const UNIQUENESS_VIOLATION_WIRE_PREFIX: &str = "uniqueness violation: ";

#[cfg(test)]
mod tests {
    use super::{RouterError, UNIQUENESS_VIOLATION_WIRE_PREFIX};

    #[test]
    fn uniqueness_violation_prefix_matches_display() {
        let rendered = RouterError::UniquenessViolation("x".to_string()).to_string();
        assert!(
            rendered.starts_with(UNIQUENESS_VIOLATION_WIRE_PREFIX),
            "the wire prefix must match the RouterError Display, got {rendered:?}"
        );
    }
}
