use candid::{CandidType, Principal};
use ic_stable_structures::storable::Storable;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::entry::GraphId;
use crate::provisioning::{LogicalResource, ProvisioningIntentKey};

/// Derive the deterministic routing key for a graph/resource provisioning intent.
///
/// The id hashes only `(graph_name, requested_resources)`. It groups retries at one stable key but
/// is not the request's full semantic identity: Router Map 45 compares caller, owner, canonical
/// admins, the exact Provision target, and the byte-exact resolved envelope before replay. The
/// fixed-width hash replaces the former `graph_name + "-" + request_fingerprint` string key.
pub fn provisioning_request_id(
    graph_name: &str,
    requested_resources: &[ProvisionableResource],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(graph_name.as_bytes());
    for resource in requested_resources {
        hasher.update(resource.logical_resource.into_bytes());
    }
    hasher.finalize().into()
}

/// A requested provisionable resource. The enum variant is the discriminator (it doubles as the
/// resource kind).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, CandidType)]
pub struct ProvisionableResource {
    pub logical_resource: LogicalResource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct CreatedResource {
    pub logical_resource: LogicalResource,
    pub canister_id: Principal,
    pub artifact_hash: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ProvisionResultOutcome {
    Installed,
    Conflict,
    Failed { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionResult {
    pub request_id: [u8; 32],
    pub release_id: String,
    pub created_resources: Vec<CreatedResource>,
    pub terminal_outcome: ProvisionResultOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct RouterRegistrationAck {
    pub deployment_id: String,
    pub request_id: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum RouterRegistrationAckResponse {
    Applied,
    Replay,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum RouterRegistrationAckResult {
    Ok(RouterRegistrationAckResponse),
    Err(ProvisionIngressError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionRequest {
    pub deployment_id: String,
    pub request_id: [u8; 32],
    pub intent_key: ProvisioningIntentKey,
    pub reserved_graph_id: Option<GraphId>,
    pub graph_name: String,
    pub requested_resources: Vec<ProvisionableResource>,
    /// Candid-encoded init args for each requested resource, in the same order as
    /// `requested_resources`. The Router (sole owner of logical topology) constructs these;
    /// Provision installs them verbatim and never re-derives logical state.
    pub install_args: Vec<Vec<u8>>,
    pub authorized_caller: Principal,
    pub release_id: String,
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
    pub request_id: [u8; 32],
    pub deployment_id: String,
    pub state: String,
    pub active_resource_index: u32,
    pub completed_effect_count: u32,
}

/// Admission response returned by `accept_envelope`. Distinct from the
/// terminal `ProvisionResult` envelope so a successful first admission is never
/// reported as `Failed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ProvisionAcceptResponse {
    Accepted {
        job_view: ProvisionJobSummary,
        intent_lock_count: u32,
        /// Canister ids created/installed for this request, in `requested_resources` order.
        /// Populated once the async deploy completes; empty for a `Reserved` (still-in-flight) response.
        created_resources: Vec<CreatedResource>,
    },
    Replay {
        job_view: ProvisionJobSummary,
        intent_lock_count: u32,
        created_resources: Vec<CreatedResource>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::types::TypeInner;

    fn labels<T: CandidType>() -> Vec<String> {
        let mut labels = match T::ty().as_ref() {
            TypeInner::Record(fields) | TypeInner::Variant(fields) => fields
                .iter()
                .map(|field| field.id.to_string())
                .collect::<Vec<String>>(),
            other => panic!("expected record or variant, got {other:?}"),
        };
        labels.sort();
        labels
    }

    #[test]
    fn graph_registration_ack_candid_shape_is_exact_and_versionless() {
        assert_eq!(
            labels::<RouterRegistrationAck>(),
            ["deployment_id", "request_id"]
        );
        assert_eq!(
            labels::<RouterRegistrationAckResponse>(),
            ["Applied", "Replay"]
        );
    }

    #[test]
    fn provision_request_candid_shape_has_no_callback_field() {
        let fields = labels::<ProvisionRequest>();
        assert_eq!(
            fields,
            [
                "authorized_caller",
                "deployment_id",
                "graph_name",
                "install_args",
                "intent_key",
                "release_id",
                "request_id",
                "requested_resources",
                "reserved_graph_id",
            ]
        );
    }
}
