//! Shared error type for the federation index API.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexError {
    NotAuthorized,
    UnknownShard,
    WrongShardCanister,
    InvalidPrincipalInRegistry,
    /// The configured router principal is the anonymous principal, which can never be the trusted
    /// router. Distinct from shard-attachment principal errors.
    AnonymousRouter,
    /// `shard_id` or principal is already attached to a different counterpart.
    ShardCanisterAlreadyAttached,
    /// The shard cannot be attached while its durable detach session is active.
    DetachInProgress,
    /// The global detach generation cannot advance without wrapping.
    DetachGenerationExhausted,
    /// The bounded active-detach control set is full.
    TooManyActiveDetaches,
    /// A legacy cursor or a cursor for another/completed detach session was supplied.
    LegacyOrStaleDetachCursor,
    GraphOwnershipMismatch,
    InvalidIndexGroupConfig,
    ShardOutOfRangeForGroup,
    IndexValueKeyTooLarge,
    InvalidRangeBounds,
    /// An equality intersection request exceeded the supported number of arms. Callers must enforce
    /// the provider-neutral limit before calling `lookup_intersection_page`.
    TooManyEqualityIntersectionArms,
    /// A range-equality intersection request arrived with no equality sieve arms. Callers that do not
    /// need a sieve must use the ordinary `lookup_range_page` path.
    MissingEqualityIntersectionArms,
    /// An equality-intersection request contained a non-vertex subject (edge or mixed). Only vertex
    /// property equality sieves are supported by the streamed intersection paths.
    InvalidIntersectionSubject,
    InvalidIntersectionCursor,
    /// A batched equality lookup request contained a subject that does not belong to the batch
    /// kind: edge subjects in `lookup_equal_batch`, vertex subjects in `lookup_edge_equal_batch`.
    InvalidBatchSubject,
    InvalidPostingPurgeCursor,
    /// A batched lookup page could not be encoded for response-size measurement.
    BatchEncodeFailed(String),
    IndexBuildAlreadyRegistered,
    UnknownIndexBuild,
    StaleIndexBuildEpoch,
    InvalidIndexBuildScope,
    InvalidIndexBuildTargetShards,
    InvalidIndexBuildTarget,
    IndexBuildRequestTooLarge,
    TooManyIndexBuildRows,
    InvalidIndexBuildCursor,
    IndexBuildAlreadyDone,
    StaleIndexBuildProgress,
    IndexBuildReplayConflict,
    IndexBuildReplayTooOld,
    InvalidIndexBuildSequence,
    IndexBuildSequenceGap,
    InvalidIndexBuildControl,
    IndexBuildNotBuilding,
    IndexBuildNotReadyToSeal,
    InvalidIndexBuildSeal,
    IndexBuildAborted,
    DuplicateIndexBuildSubject,
    IndexBuildProgressOverflow,
    IndexBuildFingerprintFailed(String),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAuthorized => write!(f, "caller is not authorized"),
            Self::UnknownShard => write!(f, "shard is not registered"),
            Self::WrongShardCanister => {
                write!(f, "caller is not the attached canister for this shard")
            }
            Self::InvalidPrincipalInRegistry => write!(f, "invalid principal in shard registry"),
            Self::AnonymousRouter => {
                write!(f, "router principal must not be the anonymous principal")
            }
            Self::ShardCanisterAlreadyAttached => {
                write!(
                    f,
                    "shard/canister attachment already exists with a different counterpart"
                )
            }
            Self::DetachInProgress => {
                write!(f, "shard detach is still in progress")
            }
            Self::DetachGenerationExhausted => {
                write!(f, "shard detach generation is exhausted")
            }
            Self::TooManyActiveDetaches => {
                write!(f, "too many shard detaches are active")
            }
            Self::LegacyOrStaleDetachCursor => {
                write!(f, "shard detach cursor is legacy or stale")
            }
            Self::GraphOwnershipMismatch => {
                write!(
                    f,
                    "index canister is already bound to a different graph/group"
                )
            }
            Self::InvalidIndexGroupConfig => {
                write!(f, "invalid index group configuration")
            }
            Self::ShardOutOfRangeForGroup => {
                write!(f, "shard id is outside the attached index group range")
            }
            Self::IndexValueKeyTooLarge => write!(
                f,
                "index value key exceeds maximum encoded size ({} bytes)",
                gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES
            ),
            Self::InvalidRangeBounds => {
                write!(f, "range bounds are empty, inverted, or otherwise invalid")
            }
            Self::TooManyEqualityIntersectionArms => write!(
                f,
                "equality intersection request has too many arms (max {})",
                gleaph_graph_kernel::index::MAX_EQUALITY_INTERSECTION_ARMS
            ),
            Self::MissingEqualityIntersectionArms => write!(
                f,
                "range-equality intersection request is missing at least one equality sieve arm"
            ),
            Self::InvalidIntersectionSubject => {
                write!(f, "equality intersection subject must be a vertex property")
            }
            Self::InvalidIntersectionCursor => {
                write!(f, "intersection cursor does not match the walk arm")
            }
            Self::InvalidBatchSubject => {
                write!(
                    f,
                    "batched equality lookup subject does not match the batch kind"
                )
            }
            Self::InvalidPostingPurgeCursor => {
                write!(f, "posting purge cursor does not match the requested scope")
            }
            Self::BatchEncodeFailed(detail) => {
                write!(f, "batched lookup page encode failed: {detail}")
            }
            Self::IndexBuildAlreadyRegistered => {
                write!(
                    f,
                    "physical index build is already registered with another scope"
                )
            }
            Self::UnknownIndexBuild => write!(f, "physical index build is not registered"),
            Self::StaleIndexBuildEpoch => write!(f, "physical index build catalog epoch is stale"),
            Self::InvalidIndexBuildScope => write!(f, "physical index build scope is invalid"),
            Self::InvalidIndexBuildTargetShards => {
                write!(f, "physical index build target shard set is invalid")
            }
            Self::InvalidIndexBuildTarget => {
                write!(
                    f,
                    "physical index build fact or subject is outside its target"
                )
            }
            Self::IndexBuildRequestTooLarge => {
                write!(
                    f,
                    "physical index build request exceeds the safe byte limit"
                )
            }
            Self::TooManyIndexBuildRows => {
                write!(f, "physical index build request exceeds the row limit")
            }
            Self::InvalidIndexBuildCursor => {
                write!(f, "physical index build cursor envelope is invalid")
            }
            Self::IndexBuildAlreadyDone => write!(f, "physical index build is already complete"),
            Self::StaleIndexBuildProgress => {
                write!(
                    f,
                    "physical index build page does not match durable progress"
                )
            }
            Self::IndexBuildReplayConflict => {
                write!(
                    f,
                    "physical index build page sequence was reused with another envelope"
                )
            }
            Self::IndexBuildReplayTooOld => {
                write!(
                    f,
                    "physical index build replay is older than the retained receipt"
                )
            }
            Self::InvalidIndexBuildSequence => {
                write!(f, "physical index build shard sequence must start at one")
            }
            Self::IndexBuildSequenceGap => {
                write!(f, "physical index build shard sequence is not contiguous")
            }
            Self::InvalidIndexBuildControl => {
                write!(
                    f,
                    "physical index build control identity does not match registration"
                )
            }
            Self::IndexBuildNotBuilding => {
                write!(f, "physical index build is not accepting base pages")
            }
            Self::IndexBuildNotReadyToSeal => {
                write!(f, "physical index build base scan is not complete")
            }
            Self::InvalidIndexBuildSeal => {
                write!(f, "physical index build seal envelope is invalid")
            }
            Self::IndexBuildAborted => {
                write!(f, "physical index build namespace is aborting or aborted")
            }
            Self::DuplicateIndexBuildSubject => {
                write!(f, "physical index build page contains a duplicate subject")
            }
            Self::IndexBuildProgressOverflow => {
                write!(f, "physical index build progress overflow")
            }
            Self::IndexBuildFingerprintFailed(detail) => {
                write!(f, "physical index build fingerprint failed: {detail}")
            }
        }
    }
}

impl std::error::Error for IndexError {}

impl From<IndexError> for gleaph_graph_kernel::index::IndexBuildStoreError {
    fn from(value: IndexError) -> Self {
        use gleaph_graph_kernel::index::IndexBuildStoreError as Wire;

        match value {
            IndexError::NotAuthorized => Wire::NotAuthorized,
            IndexError::UnknownShard => Wire::UnknownShard,
            IndexError::WrongShardCanister => Wire::WrongShardCanister,
            IndexError::IndexValueKeyTooLarge => Wire::IndexValueKeyTooLarge,
            IndexError::IndexBuildAlreadyRegistered => Wire::AlreadyRegistered,
            IndexError::UnknownIndexBuild => Wire::UnknownBuild,
            IndexError::StaleIndexBuildEpoch => Wire::StaleEpoch,
            IndexError::InvalidIndexBuildScope => Wire::InvalidScope,
            IndexError::InvalidIndexBuildTargetShards => Wire::InvalidTargetShards,
            IndexError::InvalidIndexBuildTarget => Wire::InvalidTarget,
            IndexError::IndexBuildRequestTooLarge => Wire::RequestTooLarge,
            IndexError::TooManyIndexBuildRows => Wire::TooManyRows,
            IndexError::InvalidIndexBuildCursor => Wire::InvalidCursor,
            IndexError::IndexBuildAlreadyDone => Wire::AlreadyDone,
            IndexError::StaleIndexBuildProgress => Wire::StaleProgress,
            IndexError::IndexBuildReplayConflict => Wire::ReplayConflict,
            IndexError::IndexBuildReplayTooOld => Wire::ReplayTooOld,
            IndexError::InvalidIndexBuildSequence => Wire::InvalidSequence,
            IndexError::IndexBuildSequenceGap => Wire::SequenceGap,
            IndexError::InvalidIndexBuildControl => Wire::InvalidControl,
            IndexError::IndexBuildNotBuilding => Wire::NotBuilding,
            IndexError::IndexBuildNotReadyToSeal => Wire::NotReadyToSeal,
            IndexError::InvalidIndexBuildSeal => Wire::InvalidSeal,
            IndexError::IndexBuildAborted => Wire::Aborted,
            IndexError::DuplicateIndexBuildSubject => Wire::DuplicateSubject,
            IndexError::IndexBuildProgressOverflow => Wire::ProgressOverflow,
            IndexError::IndexBuildFingerprintFailed(_) => Wire::FingerprintFailed,
            IndexError::InvalidPrincipalInRegistry
            | IndexError::AnonymousRouter
            | IndexError::ShardCanisterAlreadyAttached
            | IndexError::DetachInProgress
            | IndexError::DetachGenerationExhausted
            | IndexError::TooManyActiveDetaches
            | IndexError::LegacyOrStaleDetachCursor
            | IndexError::GraphOwnershipMismatch
            | IndexError::InvalidIndexGroupConfig
            | IndexError::ShardOutOfRangeForGroup
            | IndexError::InvalidRangeBounds
            | IndexError::TooManyEqualityIntersectionArms
            | IndexError::MissingEqualityIntersectionArms
            | IndexError::InvalidIntersectionSubject
            | IndexError::InvalidIntersectionCursor
            | IndexError::InvalidBatchSubject
            | IndexError::InvalidPostingPurgeCursor
            | IndexError::BatchEncodeFailed(_) => Wire::Internal,
        }
    }
}

impl From<IndexError> for gleaph_graph_kernel::index::IndexBuildError {
    fn from(value: IndexError) -> Self {
        Self::Store(value.into())
    }
}
