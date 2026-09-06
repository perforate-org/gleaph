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
    /// Installer principal; receives the full administrative capability set in stable auth.
    pub issuing_principal: Principal,
    /// Additional principals seeded with the full administrative capability set at init.
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

/// Candid init args for a Text canister issued by Provision (plan 0297; analyzer id per
/// plan 0331). The Candid shape mirrors `text_canister::TextCanisterInitArgs` exactly
/// (`controller`, `analyzer_id`, `dict_relay_caller`); this module only fixes the wire shape
/// so Router-built install args decode in the text canister's init handler.
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct TextCanisterInitArgs {
    /// Controller allowed to call the text canister's admin endpoints (`admin_flush`,
    /// `admin_merge_step`, and the later backfill/dictionary steps). Wired to the issuing
    /// Router so Router-driven backfill/flush work is authorized from day one.
    pub controller: Option<Principal>,
    /// Pinned analyzer id (plan 0331): 1 = unicode-bigram, 2 = vibrato + ipadic lemma
    /// pipeline. The text canister validates ∈ {1, 2} fail-closed at the open.
    pub analyzer_id: Option<u32>,
    /// Principal allowed to call the dictionary-relay endpoints (`admin_upload_dict_chunk` /
    /// `admin_finalize_dict_upload`) in addition to the stored controller. Wired to the
    /// Provision canister principal by the Router at build time. `None` (or anonymous) is
    /// fail-closed on the text side: no relay caller is authorized.
    pub dict_relay_caller: Option<Principal>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{decode_args, encode_args};

    #[test]
    fn text_canister_init_args_candid_roundtrip() {
        for dict_relay_caller in [None, Some(Principal::from_slice(&[0x42; 29]))] {
            let args = TextCanisterInitArgs {
                controller: Some(Principal::from_slice(&[0x41; 29])),
                analyzer_id: Some(2),
                dict_relay_caller,
            };
            let bytes = encode_args((args.clone(),)).unwrap();
            let decoded: (TextCanisterInitArgs,) = decode_args(&bytes).unwrap();
            assert_eq!(decoded.0.controller, args.controller);
            assert_eq!(decoded.0.analyzer_id, args.analyzer_id);
            assert_eq!(decoded.0.dict_relay_caller, args.dict_relay_caller);
        }
    }

    #[test]
    fn old_two_field_init_args_decodes_with_none_relay_caller() {
        // A pre-0335 sender that only encodes `(controller, analyzer_id)` must decode under
        // the new receiver as `dict_relay_caller = None` (Candid fills the missing optional).
        #[derive(CandidType, Deserialize, Clone, Debug)]
        struct OldTextCanisterInitArgs {
            controller: Option<Principal>,
            analyzer_id: Option<u32>,
        }
        let old = OldTextCanisterInitArgs {
            controller: Some(Principal::from_slice(&[0x41; 29])),
            analyzer_id: Some(0),
        };
        let bytes = encode_args((old,)).unwrap();
        let decoded: (TextCanisterInitArgs,) = decode_args(&bytes).unwrap();
        assert_eq!(
            decoded.0.controller,
            Some(Principal::from_slice(&[0x41; 29]))
        );
        assert_eq!(decoded.0.analyzer_id, Some(0));
        assert_eq!(decoded.0.dict_relay_caller, None);
    }
}
