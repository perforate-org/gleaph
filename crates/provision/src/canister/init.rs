//! Provision canister bootstrap init handler.

use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;
use crate::stable::store::DeploymentTrustStore;
use crate::types::{
    AdminInstallDeploymentBindingArgs, BootstrapAuthAction, BootstrapAuthEntry,
    BootstrapAuthorityRecord, DeploymentBinding,
};
use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

/// Bootstrap arguments for `init`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionInitArgs {
    pub bootstrap_bindings: Vec<DeploymentBinding>,
}

/// Seed the deployment trust store with bootstrap bindings and write the durable bootstrap
/// authority singleton + first audit row(s).
pub fn init(args: ProvisionInitArgs) {
    let store = DeploymentTrustStore::new();
    let auth_store = ProvisionBootstrapAuthStore::new();
    let now_ns = crate::ic_time_ns();

    for (index, binding) in args.bootstrap_bindings.iter().enumerate() {
        if binding.governance_principal == Principal::anonymous() {
            ic_cdk::trap("anonymous governance principal is not allowed");
        }
        let seeded = store.get_or_install(binding.clone());

        // Every bootstrap binding seeds an InitialSeed audit row for its governance principal.
        auth_store.put_record(
            seeded.governance_principal,
            BootstrapAuthEntry {
                caller: seeded.governance_principal,
                deployment_id: Some(seeded.deployment_id.clone()),
                action: BootstrapAuthAction::InitialSeed,
                timestamp_ns: now_ns,
                registry_version: Some(seeded.binding_version),
            },
        );

        // Only the first bootstrap binding establishes the durable singleton authority.
        if index == 0 {
            auth_store.init_authority(BootstrapAuthorityRecord {
                governance_principal: seeded.governance_principal,
                binding_version_at_seed: seeded.binding_version,
                seeded_at_ns: now_ns,
            });
        }
    }
}

/// Build a `DeploymentBinding` from the admin-install args so it can be persisted by the store.
pub(crate) fn binding_from_admin_args(
    args: AdminInstallDeploymentBindingArgs,
) -> DeploymentBinding {
    DeploymentBinding {
        deployment_id: args.deployment_id,
        router_principal: args.router_principal,
        governance_principal: args.governance_principal,
        binding_version: args.binding_version,
    }
}
