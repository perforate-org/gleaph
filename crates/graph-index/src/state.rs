//! Shared error type for the federation index API.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexError {
    NotAuthorized,
    UnknownShard,
    WrongShardCanister,
    InvalidPrincipalInRegistry,
    /// `shard_id` or principal is already attached to a different counterpart.
    ShardCanisterAlreadyAttached,
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
        }
    }
}

impl std::error::Error for IndexError {}
