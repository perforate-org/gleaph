//! Provision canister bootstrap init handler.

use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;
use crate::types::{BootstrapAuthAction, BootstrapAuthEntry, BootstrapAuthorityRecord};
use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

/// Bootstrap arguments for `init`: the single governance authority. Deployment grants are
/// seeded afterwards (per-issuer), never at init.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionInitArgs {
    pub governance_principal: Principal,
}

/// Write the durable bootstrap authority singleton + the InitialSeed audit row.
pub fn init(args: ProvisionInitArgs) {
    if args.governance_principal == Principal::anonymous() {
        ic_cdk::trap("anonymous governance principal is not allowed");
    }
    let auth_store = ProvisionBootstrapAuthStore::new();
    let now_ns = crate::ic_time_ns();

    auth_store.init_authority(BootstrapAuthorityRecord {
        governance_principal: args.governance_principal,
        seeded_at_ns: now_ns,
    });
    auth_store.put_record(
        args.governance_principal,
        BootstrapAuthEntry {
            caller: args.governance_principal,
            deployment_id: None,
            action: BootstrapAuthAction::InitialSeed,
            timestamp_ns: now_ns,
        },
    );
}