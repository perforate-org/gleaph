//! Provision canister ingress handler foundation (ADR 0035 Slice 3).
//!
//! These are plain `pub(crate)` functions with explicit caller injection so unit tests can
//! drive every authorization and idempotency branch. Callable canister endpoints
//! (`#[init]`/`#[query]`/`#[update]` annotations) remain a follow-up slice.

use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::stable::store::{DeploymentTrustStore, ProvisionJobStore};
use crate::types::{
    CreatedResource, JobState, ProvisionJobRecord, ProvisionJobRequestKey, ProvisionRequest,
    ProvisionResult, ProvisionResultOutcome, ProvisionableResourceKind, ProvisioningIntentKey,
    ResourceJobEntry, RouterProvisionAck, state_name,
};

pub mod init;

// === Errors ==================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionQueryError {
    NotAuthorized,
    UnknownDeployment,
    NotFound,
}

// === Wire views ==============================================================

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionJobView {
    pub request_id: String,
    pub deployment_id: String,
    pub request_fingerprint: String,
    pub reserved_graph_id: Option<gleaph_graph_kernel::entry::GraphId>,
    pub graph_name: String,
    pub state_name: String,
    pub active_resource_index: u32,
    pub completed_effect_count: u32,
    pub accepted_registry_version: Option<u64>,
    pub resources: Vec<ResourceJobView>,
    pub is_authorized_caller: bool,
    pub has_router_callback: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ResourceJobView {
    pub resource_kind: ProvisionableResourceKind,
    pub logical_resource_key: String,
    pub canister_id: Option<Principal>,
    pub artifact_hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionRouterAckResult {
    pub completed: bool,
    pub accepted_registry_version: u64,
}

/// Admission response returned by `accept_envelope_with_caller`. Distinct from the
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

// === Helpers =================================================================

pub(crate) fn build_record_from_request(req: ProvisionRequest, now_ns: u64) -> ProvisionJobRecord {
    ProvisionJobRecord {
        request_id: req.request_id,
        deployment_id: req.deployment_id,
        request_fingerprint: req.request_fingerprint,
        intent_key: req.intent_key,
        reserved_graph_id: req.reserved_graph_id,
        graph_name: req.graph_name,
        authorized_caller: req.authorized_caller,
        release_id: req.release_id,
        router_callback_principal: req.router_callback_principal,
        resources: req
            .requested_resources
            .into_iter()
            .map(|r| ResourceJobEntry {
                resource_kind: r.kind,
                logical_resource_key: r.logical_resource_key,
                canister_id: None,
                artifact_hash: None,
            })
            .collect(),
        current_state: JobState::Submitted,
        active_resource_index: 0,
        completed_effect_count: 0,
        accepted_registry_version: None,
        created_at_ns: now_ns,
        last_transition_ns: now_ns,
    }
}

/// Map a canonical `ProvisionJobRecord` to the terminal `ProvisionResult` envelope.
///
/// `ProvisionResult` is reserved for terminal outcomes only. A non-terminal state is
/// not a valid input to this mapper; it returns `Err(InvalidState)` so callers cannot
/// accidentally forge a terminal result for a job that is still in progress.
pub(crate) fn record_to_result(
    record: &ProvisionJobRecord,
) -> Result<ProvisionResult, ProvisionIngressError> {
    match &record.current_state {
        JobState::Completed => {
            let created_resources: Result<Vec<CreatedResource>, ProvisionIngressError> = record
                .resources
                .iter()
                .map(|r| {
                    let canister_id = r
                        .canister_id
                        .ok_or(ProvisionIngressError::ResultMappingError)?;
                    let artifact_hash = r
                        .artifact_hash
                        .clone()
                        .ok_or(ProvisionIngressError::ResultMappingError)?;
                    Ok(CreatedResource {
                        kind: r.resource_kind,
                        canister_id,
                        artifact_hash,
                    })
                })
                .collect();
            Ok(ProvisionResult {
                request_id: record.request_id.clone(),
                request_fingerprint: record.request_fingerprint.clone(),
                release_id: record.release_id.clone(),
                created_resources: created_resources?,
                terminal_outcome: ProvisionResultOutcome::Installed,
            })
        }
        JobState::Failed { reason } => Ok(ProvisionResult {
            request_id: record.request_id.clone(),
            request_fingerprint: record.request_fingerprint.clone(),
            release_id: record.release_id.clone(),
            created_resources: vec![],
            terminal_outcome: ProvisionResultOutcome::Failed {
                reason: reason.clone(),
            },
        }),
        _other => Err(ProvisionIngressError::InvalidState),
    }
}

pub(crate) fn build_job_summary(record: &ProvisionJobRecord) -> ProvisionJobSummary {
    ProvisionJobSummary {
        request_id: record.request_id.clone(),
        deployment_id: record.deployment_id.clone(),
        state: state_name(&record.current_state).to_owned(),
        active_resource_index: record.active_resource_index as u32,
        completed_effect_count: record.completed_effect_count,
        accepted_registry_version: record.accepted_registry_version,
    }
}

fn build_job_view(record: &ProvisionJobRecord, _caller: Principal) -> ProvisionJobView {
    ProvisionJobView {
        request_id: record.request_id.clone(),
        deployment_id: record.deployment_id.clone(),
        request_fingerprint: record.request_fingerprint.clone(),
        reserved_graph_id: record.reserved_graph_id,
        graph_name: record.graph_name.clone(),
        state_name: state_name(&record.current_state).to_owned(),
        active_resource_index: record.active_resource_index as u32,
        completed_effect_count: record.completed_effect_count,
        accepted_registry_version: record.accepted_registry_version,
        resources: record
            .resources
            .iter()
            .map(|r| ResourceJobView {
                resource_kind: r.resource_kind,
                logical_resource_key: r.logical_resource_key.clone(),
                canister_id: r.canister_id,
                artifact_hash: r.artifact_hash.clone(),
            })
            .collect(),
        is_authorized_caller: record.authorized_caller != Principal::anonymous(),
        has_router_callback: record.router_callback_principal != Principal::anonymous(),
    }
}

// === Handlers ================================================================

pub(crate) fn accept_envelope_with_caller(
    caller: Principal,
    store: &ProvisionJobStore,
    deployment_store: &DeploymentTrustStore,
    req: ProvisionRequest,
    now_ns: u64,
) -> Result<ProvisionAcceptResponse, ProvisionIngressError> {
    // 1. Authenticate first (Step 5A). Unauthorized callers never reach the store.
    let binding = deployment_store
        .get(&req.deployment_id)
        .ok_or(ProvisionIngressError::UnknownDeployment)?;
    if caller != binding.router_principal {
        return Err(ProvisionIngressError::NotAuthorized);
    }

    // 2. Validate requested_resources.
    if req.requested_resources.is_empty() {
        return Err(ProvisionIngressError::InvalidResources {
            reason: "requested_resources is empty".to_owned(),
        });
    }
    let mut seen = HashSet::new();
    for resource in &req.requested_resources {
        if !seen.insert((resource.kind, resource.logical_resource_key.clone())) {
            return Err(ProvisionIngressError::InvalidResources {
                reason: format!(
                    "duplicate resource: {:?}/{}",
                    resource.kind, resource.logical_resource_key
                ),
            });
        }
    }
    let canonical_intent_present = req.requested_resources.iter().any(|resource| {
        resource.kind == req.intent_key.resource_kind
            && resource.logical_resource_key == req.intent_key.logical_resource_key
    });
    if !canonical_intent_present {
        return Err(ProvisionIngressError::InvalidResources {
            reason: "envelope intent_key is not represented in requested_resources".to_owned(),
        });
    }

    // 3. Single store boundary: preflights locks, co-writes job + derived rows + locks,
    // and advances the fresh record to Reserved atomically.
    let record = build_record_from_request(req, now_ns);
    match store.insert_with_intent_locks(record, now_ns) {
        Ok(crate::stable::store::InsertWithLocksOutcome::InsertedFresh(updated)) => {
            Ok(ProvisionAcceptResponse::Accepted {
                job_view: build_job_summary(&updated),
                intent_lock_count: store.intent_lock_count_for_record(&updated) as u32,
            })
        }
        Ok(crate::stable::store::InsertWithLocksOutcome::IdempotentReplay(existing)) => {
            Ok(ProvisionAcceptResponse::Replay {
                job_view: build_job_summary(&existing),
                intent_lock_count: store.intent_lock_count_for_record(&existing) as u32,
            })
        }
        Err(crate::stable::store::InsertWithLocksError::Conflict) => {
            Err(ProvisionIngressError::Conflict)
        }
        Err(crate::stable::store::InsertWithLocksError::IntentLockHeld) => {
            Err(ProvisionIngressError::IntentLockHeld)
        }
    }
}

pub(crate) fn query_job_with_caller(
    caller: Principal,
    store: &ProvisionJobStore,
    deployment_store: &DeploymentTrustStore,
    request_id: String,
    deployment_id: String,
) -> Result<ProvisionJobView, ProvisionQueryError> {
    let binding = deployment_store
        .get(&deployment_id)
        .ok_or(ProvisionQueryError::UnknownDeployment)?;
    let record = store
        .get_by_request(&request_id, &deployment_id)
        .ok_or(ProvisionQueryError::NotFound)?;
    if caller != binding.router_principal && caller != binding.governance_principal {
        return Err(ProvisionQueryError::NotAuthorized);
    }
    Ok(build_job_view(&record, caller))
}

pub(crate) fn router_ack_with_caller(
    caller: Principal,
    store: &ProvisionJobStore,
    deployment_store: &DeploymentTrustStore,
    ack: RouterProvisionAck,
    now_ns: u64,
) -> Result<ProvisionRouterAckResult, ProvisionIngressError> {
    let mut record = store
        .get_by_request(&ack.request_id, &ack.deployment_id)
        .ok_or(ProvisionIngressError::NotFound)?;
    let key = ProvisionJobRequestKey::new(&ack.request_id, &ack.deployment_id);

    let binding = deployment_store
        .get(&ack.deployment_id)
        .ok_or(ProvisionIngressError::UnknownDeployment)?;
    if caller != binding.router_principal {
        return Err(ProvisionIngressError::NotAuthorized);
    }

    // Idempotent replay branches before the fresh-ack path.
    if record.current_state == JobState::Completed {
        match record.accepted_registry_version {
            Some(stored) if stored == ack.accepted_registry_version => {
                return Ok(ProvisionRouterAckResult {
                    completed: true,
                    accepted_registry_version: stored,
                });
            }
            Some(stored) => {
                return Err(ProvisionIngressError::AckConflict { stored });
            }
            None => return Err(ProvisionIngressError::InvalidState),
        }
    }

    if !matches!(record.current_state, JobState::RouterAckPending) {
        return Err(ProvisionIngressError::InvalidState);
    }

    // Preflight the lock invariant before any durable write. A RouterAckPending
    // record must have all of its intent locks held; a missing lock indicates
    // state corruption, not a recoverable flow.
    for resource in &record.resources {
        let lock_key = ProvisioningIntentKey {
            deployment_id: record.deployment_id.clone(),
            resource_kind: resource.resource_kind,
            logical_resource_key: resource.logical_resource_key.clone(),
        };
        if !store.intent_locked(&lock_key) {
            return Err(ProvisionIngressError::InvalidState);
        }
    }

    record.accepted_registry_version = Some(ack.accepted_registry_version);
    store.put(&key, record.clone());

    store
        .advance_state(&key, JobState::Completed, None, now_ns)
        .map_err(|_| ProvisionIngressError::StateAdvanceFailed)?;

    let _released = store.clear_intent_locks_for_record(&record);

    Ok(ProvisionRouterAckResult {
        completed: true,
        accepted_registry_version: ack.accepted_registry_version,
    })
}

#[cfg(test)]
mod tests;
