//! Shared canister init args for resources issued by Provision.
//!
//! These types live here so the issuing canisters (Router, Account) and Provision agree on a
//! single wire shape for `ProvisionRequest.install_args` without depending on each other. The
//! Router still owns the logical values (principals, shard ids); this module only fixes the
//! Candid shape.

use candid::{CandidType, Deserialize, Principal};

use crate::federation::ShardId;

/// Candid init args for a Router canister issued by Provision.
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct RouterInitArgs {
    /// Installer principal; receives `Admin` role in stable auth.
    pub issuing_principal: Principal,
    /// Additional principals seeded as `Admin` at init.
    #[serde(default)]
    pub initial_admins: Vec<Principal>,
    /// Optional provision-canister principal for ADR 0035 Slice 5.
    #[serde(default)]
    pub provision_canister: Option<Principal>,
}

/// Candid init args for a Graph shard canister issued by Provision.
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct GraphInitArgs {
    pub logical_graph_name: Option<String>,
    /// Router canister for federation (required together with `shard_id`).
    #[serde(default)]
    pub router_canister: Option<Principal>,
    #[serde(default)]
    pub shard_id: Option<ShardId>,
    /// Index canister for install-time federation wiring.
    ///
    /// Canister init cannot perform inter-canister calls, so deployments pass this after the
    /// Router registry has been configured.
    #[serde(default)]
    pub index_canister: Option<Principal>,
}

/// Candid init args for a Property Index canister issued by Provision.
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct IndexInitArgs {
    /// Router canister allowed to call `admin_attach_shard_canister` / `admin_detach_shard_canister`.
    pub router_canister: Principal,
}

/// Deterministic trusted seed used by local fixtures and deploy tooling. Production callers still
/// pass the value explicitly so the persisted header records the install-time trust decision.
pub const DEFAULT_DEFINITION_MAP_SEED: u64 = 0x6a09_e667_f3bc_c909;
pub const DEFAULT_SUBJECT_MAP_SEED: u64 = 0xbb67_ae85_84ca_a73b;

/// Candid init args for a Vector canister issued by Provision.
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct VectorCanisterInitArgs {
    /// Router canister allowed to call `admin_attach_shard_canister` / `admin_detach_shard_canister`.
    pub router_canister: Principal,
    /// Trusted hash seed persisted by the strict fresh-install definition-map create operation.
    pub definition_map_seed: u64,
    /// Trusted hash seed persisted by the strict fresh-install subject-map create operation.
    pub subject_map_seed: u64,
}
