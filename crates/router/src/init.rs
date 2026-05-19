//! Candid-shaped init args for the router canister.

use candid::{CandidType, Deserialize, Principal};

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct RouterInitArgs {
    #[serde(default)]
    pub controllers: Vec<Principal>,
}
