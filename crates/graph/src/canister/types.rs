//! Candid-shaped types for canister `init` and admin APIs.

use candid::{CandidType, Deserialize, Principal};
use gleaph_graph_kernel::federation::ShardId;

/// Arguments supplied by the registry (or installer) on first `init`.
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct GraphInitArgs {
    pub issuing_principal: Principal,
    #[serde(default)]
    pub initial_admins: Vec<Principal>,
    pub logical_graph_name: Option<String>,
    /// Router canister for federation (required together with `shard_id`).
    #[serde(default)]
    pub router_canister: Option<Principal>,
    #[serde(default)]
    pub shard_id: Option<ShardId>,
}

#[derive(CandidType, Deserialize)]
pub struct GrantRoleArgs {
    pub target: Principal,
    pub role: String,
    pub manager_caps: u64,
}
