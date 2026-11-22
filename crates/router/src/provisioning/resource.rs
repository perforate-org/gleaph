//! Router side of deleting one issued add-on canister (ADR 0037 §Resource deletion).
//!
//! Provision owns the management calls and the durable deletion record; the Router owns the
//! logical definition and drives the retry. The Router therefore sends the deletion, waits for
//! Provision's answer (`Deleted` and `Replay` both mean the canister is gone), and only then
//! retires its own issuance record for the intent. A failed send leaves every Router record in
//! place, so repeating the owning DDL resends the same request and Provision resumes it.

use candid::Principal;
use gleaph_graph_kernel::provisioning::wire::{ResourceDeletionRequest, ResourceDeletionResponse};
use gleaph_graph_kernel::provisioning::{DeploymentId, LogicalResource, ProvisioningIntentKey};

use crate::facade::store::provisioning::RouterProvisioningRequestStore;
use crate::provisioning::graph::map_provision_outbound_error;
use crate::provisioning::sender::send_delete_resource;
use crate::state::RouterError;
use crate::types::RouterOutboundError;

/// Delete the canister Provision issued for `logical_resource` of this Router's deployment.
pub(crate) async fn delete_provisioned_resource(
    logical_resource: LogicalResource,
    canister: Principal,
) -> Result<(), RouterError> {
    let provision_canister = crate::facade::stable::router_config::provision_canister()
        .ok_or_else(|| {
            RouterError::InvalidState(
                "no provision canister is configured to delete an issued canister".to_owned(),
            )
        })?;
    delete_provisioned_resource_with(
        ic_cdk::api::canister_self(),
        provision_canister,
        logical_resource,
        canister,
        send_delete_resource,
    )
    .await
}

pub(crate) async fn delete_provisioned_resource_with<Send, SendFuture>(
    router_principal: Principal,
    provision_canister: Principal,
    logical_resource: LogicalResource,
    canister: Principal,
    send: Send,
) -> Result<(), RouterError>
where
    Send: FnOnce(Principal, ResourceDeletionRequest) -> SendFuture,
    SendFuture: std::future::Future<Output = Result<ResourceDeletionResponse, RouterOutboundError>>,
{
    let deployment_id = DeploymentId::new(router_principal);
    send(
        provision_canister,
        ResourceDeletionRequest {
            deployment_id,
            logical_resource,
            canister_id: canister,
        },
    )
    .await
    .map_err(map_provision_outbound_error)?;
    RouterProvisioningRequestStore::new()
        .clear_request_owning_intent(&ProvisioningIntentKey::new(deployment_id, logical_resource));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Encode;
    use gleaph_graph_kernel::federation::TextIndexId;
    use gleaph_graph_kernel::provisioning::wire::{ProvisionRequest, ProvisionableResource};
    use std::cell::RefCell;

    use crate::types::{
        IntentLockOwner, ProvisioningRequestKey, RouterProvisioningRequest,
        RouterProvisioningRequestState,
    };

    fn router() -> Principal {
        Principal::from_slice(&[41; 29])
    }

    fn provision() -> Principal {
        Principal::from_slice(&[42; 29])
    }

    fn text(id: u32) -> LogicalResource {
        LogicalResource::TextIndex(TextIndexId::new(id))
    }

    /// Insert the Router issuance record of one text canister, as `CREATE TEXT INDEX` leaves it.
    fn issue(resource: LogicalResource) -> ProvisioningRequestKey {
        let deployment_id = DeploymentId::new(router());
        let requested_resources = vec![ProvisionableResource {
            logical_resource: resource,
        }];
        let request_id = gleaph_graph_kernel::provisioning::wire::provisioning_request_id(
            "tenant.text",
            &requested_resources,
        );
        let envelope = ProvisionRequest {
            deployment_id,
            request_id,
            intent_key: ProvisioningIntentKey::new(deployment_id, resource),
            reserved_graph_id: None,
            graph_name: "tenant.text".to_owned(),
            requested_resources,
            install_args: vec![vec![]],
            authorized_caller: Principal::from_slice(&[1; 29]),
            release_id: "default".to_owned(),
            controller_mode: gleaph_graph_kernel::provisioning::ControllerMode::SelfCustody,
        };
        RouterProvisioningRequestStore::new()
            .insert(
                deployment_id,
                RouterProvisioningRequest {
                    request_id,
                    caller: Principal::from_slice(&[1; 29]),
                    owner: Principal::from_slice(&[1; 29]),
                    admins: Default::default(),
                    provision_target: provision(),
                    resolved_request_bytes: Encode!(&envelope).expect("encode envelope"),
                    state: RouterProvisioningRequestState::AwaitingAck,
                    created_at_ns: 0,
                },
            )
            .expect("insert issuance record");
        ProvisioningRequestKey::new(&request_id, deployment_id)
    }

    fn still_issued(key: &ProvisioningRequestKey, resource: LogicalResource) -> bool {
        let store = RouterProvisioningRequestStore::new();
        store.get_by_request_id(key).is_some()
            && store.intent_locked(
                &ProvisioningIntentKey::new(DeploymentId::new(router()), resource),
                &IntentLockOwner::new(key.clone()),
            )
    }

    #[test]
    fn deletion_sends_the_issued_canister_then_retires_only_its_issuance_record() {
        let dropped = issue(text(5));
        let kept = issue(text(6));
        let canister = Principal::from_slice(&[43; 29]);
        let sent = RefCell::new(Vec::new());

        futures::executor::block_on(delete_provisioned_resource_with(
            router(),
            provision(),
            text(5),
            canister,
            |target, req| {
                sent.borrow_mut().push((target, req));
                async { Ok(ResourceDeletionResponse::Replay) }
            },
        ))
        .expect("deleted");

        assert_eq!(
            sent.into_inner(),
            vec![(
                provision(),
                ResourceDeletionRequest {
                    deployment_id: DeploymentId::new(router()),
                    logical_resource: text(5),
                    canister_id: canister,
                }
            )]
        );
        assert!(!still_issued(&dropped, text(5)));
        assert!(still_issued(&kept, text(6)));
    }

    #[test]
    fn a_failed_deletion_keeps_the_issuance_record_for_the_retry() {
        let issued = issue(text(7));

        let err = futures::executor::block_on(delete_provisioned_resource_with(
            router(),
            provision(),
            text(7),
            Principal::from_slice(&[44; 29]),
            |_, _| async {
                Err(RouterOutboundError::IngressRejected(
                    "ManagementCallFailed".to_owned(),
                ))
            },
        ))
        .expect_err("refused");

        assert!(
            matches!(&err, RouterError::ProvisionRejected(detail) if detail.contains("ManagementCallFailed")),
            "{err:?}"
        );
        assert!(still_issued(&issued, text(7)));
    }
}
