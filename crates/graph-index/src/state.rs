//! Shared error type for the federation index API.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        }
    }
}

impl std::error::Error for IndexError {}
