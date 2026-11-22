//! Router -> Provision cross-canister send (ADR 0035 Slice 5).

use candid::Principal;
use gleaph_graph_kernel::provisioning::wire::{
    AdmissionQuote, AdmissionQuoteError, ProvisionAcceptResponse, ProvisionIngressError,
    ProvisionIngressResult, ProvisionRequest, ResourceDeletionError, ResourceDeletionRequest,
    ResourceDeletionResponse, ResourceDeletionResult, RouterRegistrationAck,
    RouterRegistrationAckResponse, RouterRegistrationAckResult,
};

use crate::types::RouterOutboundError;

/// Send a resolved provisioning request to the Provision canister's `accept_envelope`
/// entry point. Candid encoding/decoding errors are mapped to `EncodingFailed`; IC
/// transport failures map to `CallFailed`.
pub(crate) async fn send_accept_envelope(
    provision_canister: Principal,
    resolved_request_bytes: Vec<u8>,
) -> Result<ProvisionAcceptResponse, RouterOutboundError> {
    use candid::Decode;
    use ic_cdk::call::Call;

    let request: ProvisionRequest = Decode!(&resolved_request_bytes, ProvisionRequest)
        .map_err(|e| RouterOutboundError::EncodingFailed(e.to_string()))?;
    let quote: Result<AdmissionQuote, AdmissionQuoteError> =
        Call::unbounded_wait(provision_canister, "quote_admission")
            .with_arg(
                u32::try_from(request.requested_resources.len()).map_err(|_| {
                    RouterOutboundError::EncodingFailed("resource count".to_owned())
                })?,
            )
            .await
            .map_err(|e| map_call_error(&e))?
            .candid()
            .map_err(|e| RouterOutboundError::EncodingFailed(e.to_string()))?;
    let quote = quote.map_err(|_| RouterOutboundError::CallFailed("quote_admission".to_owned()))?;

    Call::unbounded_wait(provision_canister, "accept_envelope")
        .take_raw_args(resolved_request_bytes)
        .with_cycles(quote.total)
        .await
        .map_err(|e| map_call_error(&e))?
        .candid::<ProvisionIngressResult>()
        .map_err(|e| RouterOutboundError::EncodingFailed(e.to_string()))
        .and_then(classify_ingress_result)
}

pub(crate) async fn send_registration_ack(
    provision_canister: Principal,
    ack: RouterRegistrationAck,
) -> Result<RouterRegistrationAckResponse, RouterOutboundError> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(provision_canister, "complete_graph_registration")
        .with_args(&(ack,))
        .await
        .map_err(|error| {
            RouterOutboundError::CallFailed(format!(
                "complete_graph_registration call failed: {error:?}"
            ))
        })?
        .candid::<RouterRegistrationAckResult>()
        .map_err(|error| RouterOutboundError::EncodingFailed(error.to_string()))
        .and_then(classify_registration_ack_result)
}

/// Ask Provision to stop and delete one canister it issued for this Router (ADR 0037).
pub(crate) async fn send_delete_resource(
    provision_canister: Principal,
    req: ResourceDeletionRequest,
) -> Result<ResourceDeletionResponse, RouterOutboundError> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(provision_canister, "delete_resource")
        .with_args(&(req,))
        .await
        .map_err(|error| {
            RouterOutboundError::CallFailed(format!("delete_resource call failed: {error:?}"))
        })?
        .candid::<ResourceDeletionResult>()
        .map_err(|error| RouterOutboundError::EncodingFailed(error.to_string()))
        .and_then(classify_resource_deletion_result)
}

fn classify_resource_deletion_result(
    result: ResourceDeletionResult,
) -> Result<ResourceDeletionResponse, RouterOutboundError> {
    match result {
        ResourceDeletionResult::Ok(response) => Ok(response),
        ResourceDeletionResult::Err(ResourceDeletionError::Conflict) => {
            Err(RouterOutboundError::Conflict)
        }
        ResourceDeletionResult::Err(error) => {
            Err(RouterOutboundError::IngressRejected(format!("{error:?}")))
        }
    }
}

fn classify_registration_ack_result(
    result: RouterRegistrationAckResult,
) -> Result<RouterRegistrationAckResponse, RouterOutboundError> {
    match result {
        RouterRegistrationAckResult::Ok(response) => Ok(response),
        RouterRegistrationAckResult::Err(error) => Err(map_ingress_error(error)),
    }
}

pub(crate) fn map_call_error(e: &impl std::fmt::Debug) -> RouterOutboundError {
    RouterOutboundError::CallFailed(format!("accept_envelope call failed: {e:?}"))
}

pub(crate) fn classify_ingress_result(
    result: ProvisionIngressResult,
) -> Result<ProvisionAcceptResponse, RouterOutboundError> {
    match result {
        ProvisionIngressResult::Ok(response) => Ok(response),
        ProvisionIngressResult::Err(err) => Err(map_ingress_error(err)),
    }
}

pub(crate) fn map_ingress_error(err: ProvisionIngressError) -> RouterOutboundError {
    match err {
        ProvisionIngressError::UnknownDeployment => RouterOutboundError::UnknownDeployment,
        ProvisionIngressError::NotAuthorized
        | ProvisionIngressError::IntentLockHeld
        | ProvisionIngressError::InvalidResources { .. } => {
            RouterOutboundError::ProvenPreEffectRejection(format!("{err:?}"))
        }
        ProvisionIngressError::Conflict => RouterOutboundError::Conflict,
        _ => RouterOutboundError::IngressRejected(format!("{err:?}")),
    }
}

pub(crate) fn is_proven_pre_effect_rejection(error: &RouterOutboundError) -> bool {
    matches!(
        error,
        RouterOutboundError::UnknownDeployment | RouterOutboundError::ProvenPreEffectRejection(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};
    use gleaph_graph_kernel::provisioning::wire::{
        ProvisionAcceptResponse, ProvisionIngressError, ProvisionIngressResult, ProvisionJobSummary,
    };

    fn accepted_response() -> ProvisionAcceptResponse {
        ProvisionAcceptResponse::Accepted {
            job_view: ProvisionJobSummary {
                request_id: [1u8; 32],
                deployment_id: gleaph_graph_kernel::provisioning::DeploymentId::new(
                    Principal::from_slice(b"deploy-1"),
                ),
                state: "Reserved".to_owned(),
                active_resource_index: 0,
                completed_effect_count: 0,
            },
            intent_lock_count: 1,
            created_resources: vec![],
        }
    }

    /// Round-trip a synthetic `ProvisionIngressResult` through Candid, then classify the
    /// decoded value. This proves the sender's decode-and-map seam without an IC runtime.
    fn roundtrip_and_classify(
        result: ProvisionIngressResult,
    ) -> Result<ProvisionAcceptResponse, RouterOutboundError> {
        let bytes = Encode!(&result).expect("encode ProvisionIngressResult");
        let decoded: ProvisionIngressResult =
            Decode!(&bytes, ProvisionIngressResult).expect("decode ProvisionIngressResult");
        classify_ingress_result(decoded)
    }

    #[test]
    fn test_sender_successful_decode() {
        let response = accepted_response();
        let classified = roundtrip_and_classify(ProvisionIngressResult::Ok(response.clone()))
            .expect("successful ingress must classify to Ok");
        assert_eq!(classified, response);
    }

    #[test]
    fn test_sender_call_failed_mapping() {
        // The IC call-failure path is not unit-testable without a mock runtime, but the
        // mapping contract for any Debug error value is deterministic.
        let err = map_call_error(&"rejected by transport");
        assert!(
            matches!(&err, RouterOutboundError::CallFailed(s) if s.contains("rejected by transport")),
            "transport refusal must map to CallFailed: {err:?}"
        );
    }

    #[test]
    fn test_sender_unknown_deployment_mapping() {
        let err = roundtrip_and_classify(ProvisionIngressResult::Err(
            ProvisionIngressError::UnknownDeployment,
        ))
        .expect_err("UnknownDeployment must be an error");
        assert_eq!(err, RouterOutboundError::UnknownDeployment);
    }

    #[test]
    fn test_sender_conflict_mapping() {
        let err =
            roundtrip_and_classify(ProvisionIngressResult::Err(ProvisionIngressError::Conflict))
                .expect_err("conflict variant must be an error");
        assert_eq!(err, RouterOutboundError::Conflict);
    }

    #[test]
    fn test_sender_ingress_rejected_mapping() {
        let err = roundtrip_and_classify(ProvisionIngressResult::Err(
            ProvisionIngressError::InvalidState,
        ))
        .expect_err("InvalidState must be rejected");
        assert!(
            matches!(&err, RouterOutboundError::IngressRejected(s) if s.contains("InvalidState")),
            "non-specialized ingress error must map to IngressRejected: {err:?}"
        );
    }

    #[test]
    fn resource_deletion_decodes_both_successes_and_keeps_the_refusal_reason() {
        for response in [
            ResourceDeletionResponse::Deleted,
            ResourceDeletionResponse::Replay,
        ] {
            let encoded = Encode!(&ResourceDeletionResult::Ok(response)).unwrap();
            let decoded = Decode!(&encoded, ResourceDeletionResult).unwrap();
            assert_eq!(classify_resource_deletion_result(decoded), Ok(response));
        }
        assert_eq!(
            classify_resource_deletion_result(ResourceDeletionResult::Err(
                ResourceDeletionError::Conflict
            )),
            Err(RouterOutboundError::Conflict)
        );
        let pending = classify_resource_deletion_result(ResourceDeletionResult::Err(
            ResourceDeletionError::ManagementCallFailed {
                reason: "stop_canister: timeout".to_owned(),
            },
        ))
        .expect_err("a failed management call is an error");
        assert!(
            matches!(&pending, RouterOutboundError::IngressRejected(detail) if detail.contains("stop_canister: timeout")),
            "{pending:?}"
        );
    }

    #[test]
    fn registration_ack_decodes_applied_and_replay() {
        for response in [
            RouterRegistrationAckResponse::Applied,
            RouterRegistrationAckResponse::Replay,
        ] {
            let encoded = Encode!(&RouterRegistrationAckResult::Ok(response.clone())).unwrap();
            let decoded = Decode!(&encoded, RouterRegistrationAckResult).unwrap();
            assert_eq!(classify_registration_ack_result(decoded), Ok(response));
        }
    }
}
