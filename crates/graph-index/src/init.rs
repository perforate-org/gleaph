//! Candid-shaped init args for the index canister.

use candid::{CandidType, Deserialize, Principal};

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct IndexInitArgs {
    /// Principals allowed to call index admin APIs other than router-driven shard owner updates.
    #[serde(default)]
    pub controllers: Vec<Principal>,
    /// Router canister allowed to call `admin_set_shard_owner` / `admin_clear_shard_owner`.
    pub router_canister: Principal,
}
