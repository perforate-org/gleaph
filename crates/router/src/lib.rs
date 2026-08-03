//! Gleaph router canister — federation control plane (graph registry, shard registry).
//!
//! This crate root keeps only canister bootstrap (`init` / `post_upgrade`), the module
//! declarations, and the Candid export. The public surface lives in the [`api`] layer modules
//! (`client` / `control` / `federation`) per ADR 0056 §1.

#[cfg(feature = "canbench")]
mod bench;

#[cfg(feature = "pocket-ic-e2e")]
mod test_fault;

mod api;
mod batch_wave;
mod bulk_ingest_finalize;
mod bulk_load;
mod constraint_ddl;
mod constraint_drop;
mod edge_backfill;
mod edge_index_direction;
mod effect_recovery;
mod execution_path;
pub mod facade;
mod federation;
mod gql;
mod gql_search;
mod graph_client;
mod graph_context;
mod index_catalog;
#[cfg_attr(
    not(target_family = "wasm"),
    expect(dead_code, reason = "index client issues IC calls only on wasm")
)]
mod index_client;
mod index_ddl;
mod index_lookup;
mod index_route;
mod index_sync;
pub mod init;
#[cfg(feature = "batch-instr-log")]
pub(crate) mod instr_log;
#[cfg(not(feature = "batch-instr-log"))]
mod instr_log;
mod label_backfill;
mod label_stats_projection;
#[cfg_attr(
    not(target_family = "wasm"),
    expect(
        dead_code,
        reason = "peer sync hooks run on wasm registry lifecycle paths"
    )
)]
mod peer_sync;
mod planner_stats;
mod prepared;
mod prepared_documentation;
mod provisioning;
mod rbac;
mod reclaim;
mod recovery;
mod seed;
pub mod state;
pub mod types;
mod use_graph;
mod use_graph_wire;
mod vector_sync;
mod vertex_property_backfill;

pub use facade::store::RouterStore;
pub use init::{RouterInitArgs, RouterUpgradeArgs};
pub use state::RouterError;

#[cfg(test)]
use candid::Decode;
use candid::Principal;
use ic_cdk_macros::{init, post_upgrade};

use crate::facade::auth;

#[cfg(feature = "batch-instr-log")]
fn current_instruction_counter() -> u64 {
    gleaph_instruction_budget::call_context_instruction_counter()
}

#[init]
fn init(args: RouterInitArgs) {
    // Preflight: reject invalid bootstrap principals before clearing/writing any Router stable
    // state, so a failed init never mutates state and never depends on IC trap rollback.
    if let Err(e) =
        auth::validate_bootstrap_principals(args.issuing_principal, &args.initial_admins)
    {
        ic_cdk::trap(e.to_string());
    }
    RouterStore::new().init_from_args(&args);
    if let Err(e) = auth::bootstrap_canister_auth(args.issuing_principal, &args.initial_admins) {
        ic_cdk::trap(e.to_string());
    }
    if let Err(e) = crate::init::validate_provision_principal(&args.provision_canister) {
        ic_cdk::trap(format!("init: {e}"));
    }
    crate::provisioning::config::set(args.provision_canister);
    crate::facade::stable::provision_config::save_provision_runtime_config(
        &crate::provisioning::config::ProvisionRuntimeConfig {
            provision_canister: args.provision_canister,
        },
    );
    // ADR 0029 Phase 4: arm the autonomous saga recovery driver (no-op until there is work).
    crate::recovery::arm_if_needed();
}

#[post_upgrade]
fn post_upgrade(args: Option<RouterUpgradeArgs>) {
    let args = args.unwrap_or_default();
    let durable = crate::facade::stable::provision_config::load_provision_runtime_config();
    let provision_canister =
        match resolve_provision_canister_for_upgrade(args.provision_canister, &durable) {
            Ok(p) => p,
            Err(e) => ic_cdk::trap(format!("post_upgrade: {e}")),
        };
    crate::provisioning::config::set(provision_canister);

    // Timers do not survive an upgrade; re-arm the recovery driver so non-terminal sagas
    // persisted across the upgrade still converge (ADR 0029 Phase 4).
    crate::recovery::arm_if_needed();
    facade::stable::graph_type_catalog::rebuild_caches_after_upgrade();
    prepared::rebuild_prepared_caches_after_upgrade();
}

/// Decode Router upgrade args from Candid bytes.
///
/// Empty arg data is accepted as the stable "preserve durable configuration" form.
/// A non-empty payload must decode as [`RouterUpgradeArgs`]; anything else traps so
/// an operator cannot accidentally feed init args into an upgrade (ADR 0039).
#[cfg(test)]
pub(crate) fn decode_upgrade_args(arg_data: &[u8]) -> Option<RouterUpgradeArgs> {
    if arg_data.is_empty() {
        return None;
    }
    match candid::Decode!(arg_data, RouterUpgradeArgs) {
        Ok(args) => Some(args),
        Err(_) => ic_cdk::trap("post_upgrade: invalid upgrade args"),
    }
}

pub(crate) fn resolve_provision_canister_for_upgrade(
    override_arg: Option<Principal>,
    durable: &crate::provisioning::config::ProvisionRuntimeConfig,
) -> Result<Option<Principal>, &'static str> {
    // The durable ROUTER_PROVISION_CONFIG stable region is the SSOT for the provision-canister
    // binding. Upgrade args with `provision_canister: Some(p)` are an explicit operator override;
    // `None` means "preserve the durable binding". An invalid override is rejected with an
    // error and the durable binding is preserved.
    match override_arg {
        Some(p) => {
            crate::init::validate_provision_principal(&Some(p))?;
            crate::facade::stable::provision_config::save_provision_runtime_config(
                &crate::provisioning::config::ProvisionRuntimeConfig {
                    provision_canister: Some(p),
                },
            );
            Ok(Some(p))
        }
        None => Ok(durable.provision_canister),
    }
}

ic_cdk::export_candid!();

#[cfg(test)]
mod provision_config_upgrade_tests {
    use super::*;
    use crate::facade::stable::provision_config::{
        load_provision_runtime_config, save_provision_runtime_config,
    };
    use crate::init::validate_provision_principal;
    use crate::provisioning::config::ProvisionRuntimeConfig;

    fn canonical_principal() -> Principal {
        Principal::self_authenticating([1; 32])
    }

    #[test]
    fn test_validate_provision_principal_accepts_none_and_non_anonymous() {
        assert!(validate_provision_principal(&None).is_ok());
        assert!(
            validate_provision_principal(&Some(Principal::self_authenticating([2; 32]))).is_ok()
        );
        assert_eq!(
            validate_provision_principal(&Some(Principal::anonymous())),
            Err("provision_canister cannot be anonymous")
        );
    }

    #[test]
    fn test_post_upgrade_anonymous_override_rejected_preserves_canonical() {
        // Seed a canonical durable binding.
        let canonical = canonical_principal();
        let canonical_config = ProvisionRuntimeConfig {
            provision_canister: Some(canonical),
        };
        save_provision_runtime_config(&canonical_config);
        let durable = load_provision_runtime_config();

        // An anonymous override must be rejected: the resolver returns Err, the durable
        // record is preserved, and post_upgrade would trap.
        let result = resolve_provision_canister_for_upgrade(Some(Principal::anonymous()), &durable);
        assert_eq!(result, Err("provision_canister cannot be anonymous"));
        assert_eq!(
            load_provision_runtime_config(),
            durable,
            "durable record must not be overwritten by an invalid override"
        );
    }

    #[test]
    fn test_post_upgrade_valid_override_updates_canonical() {
        let canonical = canonical_principal();
        let replacement = Principal::self_authenticating([7; 32]);
        save_provision_runtime_config(&ProvisionRuntimeConfig {
            provision_canister: Some(canonical),
        });

        let durable = load_provision_runtime_config();
        let result = resolve_provision_canister_for_upgrade(Some(replacement), &durable).unwrap();
        assert_eq!(result, Some(replacement));
        assert_eq!(
            load_provision_runtime_config(),
            ProvisionRuntimeConfig {
                provision_canister: Some(replacement),
            }
        );
    }

    #[test]
    fn test_post_upgrade_none_override_uses_durable() {
        let canonical = canonical_principal();
        save_provision_runtime_config(&ProvisionRuntimeConfig {
            provision_canister: Some(canonical),
        });

        let result =
            resolve_provision_canister_for_upgrade(None, &load_provision_runtime_config()).unwrap();
        assert_eq!(result, Some(canonical));
    }

    mod upgrade_arg_decode_tests {
        use super::*;
        use crate::init::RouterInitArgs;
        use candid::Encode;

        #[test]
        fn valid_upgrade_args_decodes() {
            let principal = Principal::self_authenticating([1; 32]);
            let bytes = Encode!(&RouterUpgradeArgs {
                provision_canister: Some(principal),
            })
            .expect("encode");
            let decoded = decode_upgrade_args(&bytes).expect("decoded");
            assert_eq!(decoded.provision_canister, Some(principal));
        }

        #[test]
        fn absent_provision_decodes_to_none_override() {
            let bytes = Encode!(&RouterUpgradeArgs {
                provision_canister: None,
            })
            .expect("encode");
            let decoded = decode_upgrade_args(&bytes).expect("decoded");
            assert_eq!(decoded.provision_canister, None);
        }

        #[test]
        fn router_init_args_decode_ignores_init_only_fields() {
            // Candid record subtyping lets a RouterInitArgs payload decode as
            // RouterUpgradeArgs: extra fields (issuing_principal, initial_admins)
            // are ignored. Only the provision_canister override matters.
            let admin = Principal::self_authenticating([2; 32]);
            let provision = Principal::self_authenticating([3; 32]);
            let bytes = Encode!(&RouterInitArgs {
                issuing_principal: admin,
                initial_admins: vec![],
                provision_canister: Some(provision),
            })
            .expect("encode");
            let decoded = decode_upgrade_args(&bytes).expect("decoded");
            assert_eq!(decoded.provision_canister, Some(provision));
        }
    }
}
