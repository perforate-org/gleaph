use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

use crate::entry::GraphId;
use crate::provisioning::{ProvisionableResourceKind, ProvisioningIntentKey};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, CandidType)]
pub struct ProvisionableResource {
    pub kind: ProvisionableResourceKind,
    pub logical_resource_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct CreatedResource {
    pub kind: ProvisionableResourceKind,
    pub canister_id: Principal,
    pub artifact_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ProvisionResultOutcome {
    Installed,
    Conflict,
    Failed { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionResult {
    pub request_id: String,
    pub request_fingerprint: String,
    pub release_id: String,
    pub created_resources: Vec<CreatedResource>,
    pub terminal_outcome: ProvisionResultOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct RouterProvisionAck {
    // `deployment_id` is required so the canonical ProvisionJobRequestKey
    // (request_id, deployment_id) can be formed without ambiguity across
    // deployment bindings. This is the only Slice 1 wire-shape change.
    pub deployment_id: String,
    pub request_id: String,
    pub accepted_registry_version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionRequest {
    pub deployment_id: String,
    pub request_id: String,
    pub request_fingerprint: String,
    pub intent_key: ProvisioningIntentKey,
    pub reserved_graph_id: Option<GraphId>,
    pub graph_name: String,
    pub requested_resources: Vec<ProvisionableResource>,
    pub authorized_caller: Principal,
    pub release_id: String,
    pub router_callback_principal: Principal,
}
