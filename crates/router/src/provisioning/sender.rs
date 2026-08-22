//! Router -> Provision cross-canister send (ADR 0035 Slice 5).

use candid::Principal;
use gleaph_graph_kernel::provisioning::wire::{
    ProvisionAcceptResponse, ProvisionIngressError, ProvisionIngressResult, RouterRegistrationAck,
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
    use ic_cdk::call::Call;

    Call::unbounded_wait(provision_canister, "accept_envelope")
        .take_raw_args(resolved_request_bytes)
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
                deployment_id: "deploy-1".to_owned(),
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
