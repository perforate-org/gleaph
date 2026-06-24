//! Candid-shaped init args for the vector index canister.

use candid::{CandidType, Deserialize, Principal};

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct VectorIndexInitArgs {
    /// Router canister allowed to call `admin_attach_shard_canister` / `admin_detach_shard_canister`.
    pub router_canister: Principal,
}
