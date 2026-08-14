//! Candid-shaped init args for the vector index canister.

use candid::{CandidType, Deserialize, Principal};

/// Deterministic trusted seed used by local fixtures and deploy tooling.  Production callers still
/// pass the value explicitly so the persisted header records the install-time trust decision.
pub const DEFAULT_DEFINITION_MAP_SEED: u64 = 0x6a09_e667_f3bc_c909;
pub const DEFAULT_SUBJECT_MAP_SEED: u64 = 0xbb67_ae85_84ca_a73b;

#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct VectorCanisterInitArgs {
    /// Router canister allowed to call `admin_attach_shard_canister` / `admin_detach_shard_canister`.
    pub router_canister: Principal,
    /// Trusted hash seed persisted by the strict fresh-install definition-map create operation.
    pub definition_map_seed: u64,
    /// Trusted hash seed persisted by the strict fresh-install subject-map create operation.
    pub subject_map_seed: u64,
}
