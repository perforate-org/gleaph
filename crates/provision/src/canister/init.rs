//! Provision canister bootstrap init handler.

use crate::stable::store::DeploymentTrustStore;
use crate::types::DeploymentBinding;
use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

/// Bootstrap arguments for `init`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionInitArgs {
    pub bootstrap_bindings: Vec<DeploymentBinding>,
}

/// Seed the deployment trust store with bootstrap bindings. Traps on anonymous governance.
pub fn init(args: ProvisionInitArgs) {
    let store = DeploymentTrustStore::new();
    for binding in args.bootstrap_bindings {
        if binding.governance_principal == Principal::anonymous() {
            ic_cdk::trap("anonymous governance principal is not allowed");
        }
        store.get_or_install(binding);
    }
}
