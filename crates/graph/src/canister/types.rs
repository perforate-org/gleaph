//! Candid-shaped types for canister `init` and admin APIs.

use candid::{CandidType, Deserialize, Principal};

/// Arguments supplied by the registry (or installer) on first `init`.
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct GraphInitArgs {
    pub issuing_principal: Principal,
    #[serde(default)]
    pub initial_admins: Vec<Principal>,
    pub logical_graph_name: Option<String>,
    /// Optional `gleaph-graph-index` canister principal for federated property indexing.
    #[serde(default)]
    pub index_canister: Option<Principal>,
    /// Shard id registered on the index canister for this graph replica (required together with `index_canister`).
    #[serde(default)]
    pub graph_shard_id: Option<u64>,
}

#[derive(CandidType, Deserialize)]
pub struct GrantRoleArgs {
    pub target: Principal,
    pub role: String,
    pub manager_caps: u64,
}
