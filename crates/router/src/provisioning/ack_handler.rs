//! Router `router_ack` callback handler (ADR 0035 Slice 6).
//!
//! This module owns caller authorization and error mapping for the Provision -> Router ack
//! boundary. The state machine itself lives in `RouterProvisioningRequestStore::commit_ack`.

use candid::Principal;
use gleaph_graph_kernel::provisioning::wire::{RouterAckResponse, RouterProvisionAck};

use crate::facade::store::provisioning::{AckCommitError, RouterProvisioningRequestStore};
use crate::state::RouterError;
use crate::types::ProvisioningRequestKey;

/// Handle a `RouterProvisionAck` sent by the configured Provision canister.
///
/// Authorization: only the Provision canister principal bound at init time (via
/// `provisioning::config::get()`) may call this entry point. Any other caller is rejected with
/// `RouterError::NotAuthorized`.
pub(crate) fn handle_router_ack(
    caller: Principal,
    ack: RouterProvisionAck,
) -> Result<RouterAckResponse, RouterError> {
    let expected = crate::provisioning::config::get().ok_or(RouterError::NotAuthorized)?;
    if caller != expected {
        return Err(RouterError::NotAuthorized);
    }

    if ack.accepted_registry_version == 0 {
        return Err(RouterError::InvalidState(
            "accepted_registry_version 0 is reserved and invalid".to_owned(),
        ));
    }

    let store = RouterProvisioningRequestStore::new();
    let key = ProvisioningRequestKey::new(&ack.request_id, &ack.deployment_id);
    let record = store
        .commit_ack(&key, ack.accepted_registry_version)
        .map_err(map_ack_commit_error)?;

    let accepted_registry_version = record
        .accepted_registry_version
        .expect("commit_ack guarantees accepted_registry_version is Some");
    Ok(RouterAckResponse {
        accepted_registry_version,
    })
}

fn map_ack_commit_error(err: AckCommitError) -> RouterError {
    match err {
        AckCommitError::Conflict { stored } => RouterError::AckConflict { stored },
        AckCommitError::InvalidState(msg) => RouterError::InvalidState(msg),
        AckCommitError::NotFound(msg) => RouterError::NotFound(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;
    use gleaph_graph_kernel::provisioning::wire::RouterProvisionAck;
    use gleaph_graph_kernel::provisioning::{ProvisionableResourceKind, ProvisioningIntentKey};

    use crate::facade::store::RouterStore;
    use crate::facade::store::provisioning::RouterProvisioningRequestStore;
    use crate::init::RouterInitArgs;
    use crate::types::{
        IntentLockOwner, ProvisionableResource, RouterProvisioningRequest,
        RouterProvisioningRequestState,
    };

    fn provision_principal() -> Principal {
        Principal::from_slice(&[0xCD; 29])
    }

    fn other_principal() -> Principal {
        Principal::from_slice(&[0xEF; 29])
    }

    fn reset_store() {
        let router = RouterStore::new();
        router.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            provision_canister: None,
        });
    }

    fn sample_record() -> RouterProvisioningRequest {
        RouterProvisioningRequest {
            request_id: "req-1".to_owned(),
            request_fingerprint: "fp-1".to_owned(),
            caller: Principal::anonymous(),
            graph_name: "g".to_owned(),
            reserved_graph_id: None,
            requested_resources: vec![ProvisionableResource {
                kind: ProvisionableResourceKind::GraphShard,
                logical_resource_key: "shard-0".to_owned(),
            }],
            state: RouterProvisioningRequestState::AwaitingAck,
            provision_receipt: None,
            accepted_registry_version: None,
            created_at_ns: 0,
        }
    }

    fn insert_awaiting(deployment_id: &str) {
        reset_store();
        let store = RouterProvisioningRequestStore::new();
        let mut record = sample_record();
        record.request_id = format!("{}-req", deployment_id);
        let _ = store.insert(deployment_id, record).expect("insert sample");
    }

    #[test]
    fn test_handle_router_ack_wrong_caller_rejected() {
        reset_store();
        crate::provisioning::config::set(Some(provision_principal()));
        let ack = RouterProvisionAck {
            deployment_id: "d".to_owned(),
            request_id: "r".to_owned(),
            accepted_registry_version: 1,
        };
        let result = handle_router_ack(other_principal(), ack);
        assert_eq!(result, Err(RouterError::NotAuthorized));
    }

    #[test]
    fn test_handle_router_ack_missing_config_rejected() {
        reset_store();
        crate::provisioning::config::set(None);
        let ack = RouterProvisionAck {
            deployment_id: "d".to_owned(),
            request_id: "r".to_owned(),
            accepted_registry_version: 1,
        };
        let result = handle_router_ack(provision_principal(), ack);
        assert_eq!(result, Err(RouterError::NotAuthorized));
    }

    #[test]
    fn test_handle_router_ack_version_zero_rejected() {
        reset_store();
        crate::provisioning::config::set(Some(provision_principal()));
        let deployment_id = "d-zero";
        insert_awaiting(deployment_id);
        let request_id = format!("{}-req", deployment_id);
        let ack = RouterProvisionAck {
            deployment_id: deployment_id.to_owned(),
            request_id: request_id.clone(),
            accepted_registry_version: 0,
        };
        let result = handle_router_ack(provision_principal(), ack);
        assert!(
            matches!(result, Err(RouterError::InvalidState(_))),
            "expected InvalidState for version 0, got {result:?}"
        );

        // Record must remain AwaitingAck: no mutation happened.
        let store = RouterProvisioningRequestStore::new();
        let key = ProvisioningRequestKey::new(&request_id, deployment_id);
        let record = store.get_by_request_id(&key).expect("record exists");
        assert_eq!(record.state, RouterProvisioningRequestState::AwaitingAck);
    }

    #[test]
    fn test_handle_router_ack_awaiting_to_completed() {
        reset_store();
        crate::provisioning::config::set(Some(provision_principal()));
        let deployment_id = "d-await";
        insert_awaiting(deployment_id);
        let request_id = format!("{}-req", deployment_id);
        let ack = RouterProvisionAck {
            deployment_id: deployment_id.to_owned(),
            request_id: request_id.clone(),
            accepted_registry_version: 7,
        };
        let response = handle_router_ack(provision_principal(), ack).expect("ack accepted");
        assert_eq!(response.accepted_registry_version, 7);

        let store = RouterProvisioningRequestStore::new();
        let key = ProvisioningRequestKey::new(&request_id, deployment_id);
        let record = store.get_by_request_id(&key).expect("record exists");
        assert_eq!(record.state, RouterProvisioningRequestState::Completed);
        assert_eq!(record.accepted_registry_version, Some(7));
        let owner = IntentLockOwner::new(
            ProvisioningRequestKey::new(&request_id, deployment_id),
            "fp-1".to_owned(),
        );
        assert!(
            !store.intent_locked(
                &ProvisioningIntentKey::new(
                    deployment_id,
                    ProvisionableResourceKind::GraphShard,
                    "shard-0"
                ),
                &owner,
            ),
            "intent lock must be released after Completed"
        );
    }

    #[test]
    fn test_handle_router_ack_completed_replay() {
        reset_store();
        crate::provisioning::config::set(Some(provision_principal()));
        let deployment_id = "d-replay";
        insert_awaiting(deployment_id);
        let request_id = format!("{}-req", deployment_id);
        let ack = RouterProvisionAck {
            deployment_id: deployment_id.to_owned(),
            request_id: request_id.clone(),
            accepted_registry_version: 7,
        };
        let first = handle_router_ack(provision_principal(), ack.clone()).expect("first ack");
        assert_eq!(first.accepted_registry_version, 7);

        let second = handle_router_ack(provision_principal(), ack).expect("replay ack");
        assert_eq!(second.accepted_registry_version, 7);
    }

    #[test]
    fn test_handle_router_ack_completed_conflict() {
        reset_store();
        crate::provisioning::config::set(Some(provision_principal()));
        let deployment_id = "d-conflict";
        insert_awaiting(deployment_id);
        let request_id = format!("{}-req", deployment_id);
        let first = RouterProvisionAck {
            deployment_id: deployment_id.to_owned(),
            request_id: request_id.clone(),
            accepted_registry_version: 7,
        };
        handle_router_ack(provision_principal(), first).expect("first ack");

        let second = RouterProvisionAck {
            deployment_id: deployment_id.to_owned(),
            request_id,
            accepted_registry_version: 8,
        };
        let result = handle_router_ack(provision_principal(), second);
        assert_eq!(result, Err(RouterError::AckConflict { stored: 7 }));
    }

    #[test]
    fn test_handle_router_ack_missing_record() {
        reset_store();
        crate::provisioning::config::set(Some(provision_principal()));
        let ack = RouterProvisionAck {
            deployment_id: "d-missing".to_owned(),
            request_id: "no-such".to_owned(),
            accepted_registry_version: 1,
        };
        let result = handle_router_ack(provision_principal(), ack);
        assert!(
            matches!(result, Err(RouterError::NotFound(_))),
            "expected NotFound, got {result:?}"
        );
    }

    #[test]
    fn test_handle_router_ack_completed_missing_version_returns_invalid_state() {
        reset_store();
        crate::provisioning::config::set(Some(provision_principal()));
        let s = RouterProvisioningRequestStore::new();
        let mut req = sample_record();
        req.state = RouterProvisioningRequestState::Completed;
        req.accepted_registry_version = None;
        s.insert("d-complete-none", req).expect("insert completed");

        let ack = RouterProvisionAck {
            deployment_id: "d-complete-none".to_owned(),
            request_id: "req-1".to_owned(),
            accepted_registry_version: 7,
        };
        let result = handle_router_ack(provision_principal(), ack);
        assert!(
            matches!(result, Err(RouterError::InvalidState(_))),
            "expected InvalidState, got {result:?}"
        );
    }

    #[test]
    fn test_handle_router_ack_non_awaiting_non_completed_returns_invalid_state() {
        reset_store();
        crate::provisioning::config::set(Some(provision_principal()));
        let s = RouterProvisioningRequestStore::new();
        let mut req = sample_record();
        req.state = RouterProvisioningRequestState::Pending;
        s.insert("d-pending", req).expect("insert pending");

        let ack = RouterProvisionAck {
            deployment_id: "d-pending".to_owned(),
            request_id: "req-1".to_owned(),
            accepted_registry_version: 7,
        };
        let result = handle_router_ack(provision_principal(), ack);
        assert!(
            matches!(result, Err(RouterError::InvalidState(_))),
            "expected InvalidState, got {result:?}"
        );
    }
}
