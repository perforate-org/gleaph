//! Router-driven sibling graph canister ACL for `federated_expand`.

use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

/// Router → new graph shard: register all existing sibling graph principals.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct BootstrapGraphPeersArgs {
    pub peers: Vec<Principal>,
}

/// Router → existing graph shard: allow one new sibling principal.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct AddGraphPeerArgs {
    pub peer: Principal,
}

/// Router → graph shard: drop one sibling principal from the peer ACL.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct RemoveGraphPeerArgs {
    pub peer: Principal,
}
