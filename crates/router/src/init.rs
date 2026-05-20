//! Candid-shaped init args for the router canister.

use candid::{CandidType, Deserialize, Principal};

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct RouterInitArgs {
    /// Installer principal; receives [`gleaph_auth::Role::Admin`] in stable auth.
    pub issuing_principal: Principal,
    #[serde(default)]
    pub initial_admins: Vec<Principal>,
    /// Internet Computer controllers (upgrade / control plane, separate from RBAC).
    #[serde(default)]
    pub controllers: Vec<Principal>,
}
