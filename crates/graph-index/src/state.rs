//! Shared error type for the federation index API.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexError {
    NotAuthorized,
    UnknownShard,
    WrongShardOwner,
    InvalidPrincipalInRegistry,
    /// `shard_id` is already mapped to a different canister principal.
    ShardAlreadyRegistered,
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAuthorized => write!(f, "caller is not an index admin"),
            Self::UnknownShard => write!(f, "shard is not registered"),
            Self::WrongShardOwner => write!(f, "caller does not own this shard"),
            Self::InvalidPrincipalInRegistry => write!(f, "invalid principal in shard registry"),
            Self::ShardAlreadyRegistered => {
                write!(f, "shard_id is already registered to a different principal")
            }
        }
    }
}

impl std::error::Error for IndexError {}
