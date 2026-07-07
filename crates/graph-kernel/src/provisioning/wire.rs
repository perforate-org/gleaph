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

/// Response returned by the Router canister `router_ack` callback.
///
/// A successful response denotes that the Router catalog is durably in `Completed` for this
/// request and version. The ack cannot be lost: subsequent `router_ack` calls with the same
/// `(request_id, deployment_id, accepted_registry_version)` return the same response, while
/// a different version returns `AckConflict { stored }`.
///
/// `completed` is implied `true` on the Router side: the callback only succeeds after the
/// Router has durably committed the ack version. The Provision canister receives this value
/// back and uses `accepted_registry_version` as the authoritative registry watermark.
///
/// Protocol invariant: registry versions start at 1; version 0 is reserved as "unset" and
/// must not appear in ack payloads. The Router rejects any ack with
/// `accepted_registry_version == 0` as `InvalidState`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct RouterAckResponse {
    pub accepted_registry_version: u64,
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

// === Moved from gleaph-provision canister/mod.rs (Plan 0058 P1-1) =============
// These types are the Candid-visible ingress/response surface of the Provision
// canister. They are single-sourced here so gleaph-router can decode the
// `accept_envelope` response without depending on the sibling gleaph-provision crate.

/// Failure modes returned by the Provision canister `accept_envelope` ingress path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ProvisionIngressError {
    NotAuthorized,
    UnknownDeployment,
    Conflict,
    NotFound,
    InvalidState,
    StateAdvanceFailed,
    ResultMappingError,
    AckConflict { stored: u64 },
    IntentLockHeld,
    InvalidResources { reason: String },
}

/// Candid wire Result for `accept_envelope`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ProvisionIngressResult {
    Ok(ProvisionAcceptResponse),
    Err(ProvisionIngressError),
}

/// Redacted job summary for admission responses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionJobSummary {
    pub request_id: String,
    pub deployment_id: String,
    pub state: String,
    pub active_resource_index: u32,
    pub completed_effect_count: u32,
    pub accepted_registry_version: Option<u64>,
}

/// Admission response returned by `accept_envelope`. Distinct from the
/// terminal `ProvisionResult` envelope so a successful first admission is never
/// reported as `Failed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ProvisionAcceptResponse {
    Accepted {
        job_view: ProvisionJobSummary,
        intent_lock_count: u32,
    },
    Replay {
        job_view: ProvisionJobSummary,
        intent_lock_count: u32,
    },
}
