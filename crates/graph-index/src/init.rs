//! Candid-shaped init args for the index canister.

use candid::{CandidType, Deserialize, Principal};

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct IndexInitArgs {
    /// Principals allowed to call `admin_register_shard`.
    #[serde(default)]
    pub controllers: Vec<Principal>,
}
