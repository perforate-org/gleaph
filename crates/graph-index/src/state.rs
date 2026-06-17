//! Shared error type for the federation index API.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexError {
    NotAuthorized,
    UnknownShard,
    WrongShardCanister,
    InvalidPrincipalInRegistry,
    /// `shard_id` or principal is already attached to a different counterpart.
    ShardCanisterAlreadyAttached,
    GraphOwnershipMismatch,
    InvalidIndexGroupConfig,
    ShardOutOfRangeForGroup,
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
        }
    }
}

impl std::error::Error for IndexError {}
