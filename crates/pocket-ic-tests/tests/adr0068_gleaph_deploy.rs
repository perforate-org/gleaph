//! PocketIC coverage for the `gleaph deploy` flow (ADR 0068): Account registration,
//! first-Router issuance via Provision, and bootstrap trust handover.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse;
use gleaph_pocket_ic_tests::{install_account_canister, install_provision_canister, new_pocket_ic};
use gleaph_provision::types::DeploymentBinding;

fn router_principal() -> Principal {
    Principal::from_slice(&[0x10; 29])
}

fn governance_principal() -> Principal {
    Principal::from_slice(&[0x64; 29])
}

fn user_principal() -> Principal {
    Principal::from_slice(&[0x30; 29])
}

#[test]
fn gleaph_deploy_registers_account_and_hands_over_bootstrap() {
    let pic = new_pocket_ic();
    let account = install_account_canister(&pic);
    let binding = DeploymentBinding {
        deployment_id: user_principal().to_text(),
        router_principal: router_principal(),
        governance_principal: governance_principal(),
        binding_version: 1,
        bootstrap_principal: Some(account),
    };
    let provision = install_provision_canister(&pic, binding);

    // 1. User registers a Personal account.
    let bytes = pic
        .update_call(
            account,
            user_principal(),
            "create_account",
            Encode!(&"alice".to_owned()).unwrap(),
        )
        .expect("create_account call");
    let created: Result<gleaph_account::types::Account, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<gleaph_account::types::Account, gleaph_account::types::AccountError>)
            .expect("decode create_account");
    let account_id = created.expect("create_account").id();

    // 2. User authorizes the first-Router issuance (Account -> Provision accept_envelope).
    //    The endpoint takes three separate arguments (String, String, Principal), so encode
    //    them as separate Candid arguments (a tuple would encode as one record).
    let bytes = pic
        .update_call(
            account,
            user_principal(),
            "authorize_router_issuance",
            candid::encode_args((&account_id.clone(), &"default".to_owned(), &provision))
                .expect("encode authorize_router_issuance args"),
        )
        .expect("authorize_router_issuance call");
    let result: Result<ProvisionAcceptResponse, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<ProvisionAcceptResponse, gleaph_account::types::AccountError>)
            .expect("decode authorize_router_issuance");
    assert!(
        matches!(result, Ok(ProvisionAcceptResponse::Accepted { .. })),
        "first-Router issuance must be accepted: {result:?}"
    );

    // 3. User completes the bootstrap trust handover (Account -> Provision complete_bootstrap).
    let bytes = pic
        .update_call(
            account,
            user_principal(),
            "complete_bootstrap",
            candid::encode_args((&account_id, &provision)).expect("encode complete_bootstrap args"),
        )
        .expect("complete_bootstrap call");
    let result: Result<(), gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<(), gleaph_account::types::AccountError>)
            .expect("decode complete_bootstrap");
    assert!(
        result.is_ok(),
        "complete_bootstrap must succeed: {result:?}"
    );

    // 4. The bootstrap principal no longer holds issuance authority: a fresh accept_envelope
    //    from the Account is now rejected by Provision.
    let bytes = pic
        .update_call(
            provision,
            account,
            "accept_envelope",
            Encode!(&gleaph_graph_kernel::provisioning::wire::ProvisionRequest {
                deployment_id: user_principal().to_text(),
                request_id: "req-2".to_owned(),
                request_fingerprint: "fp-2".to_owned(),
                intent_key: gleaph_graph_kernel::provisioning::ProvisioningIntentKey::new(
                    &user_principal().to_text(),
                    gleaph_graph_kernel::provisioning::LogicalResource::GraphShard(
                        gleaph_graph_kernel::federation::ShardId::new(2),
                    ),
                ),
                reserved_graph_id: None,
                graph_name: "g2".to_owned(),
                requested_resources: vec![
                    gleaph_graph_kernel::provisioning::wire::ProvisionableResource {
                        logical_resource:
                            gleaph_graph_kernel::provisioning::LogicalResource::GraphShard(
                                gleaph_graph_kernel::federation::ShardId::new(2),
                            ),
                    }
                ],
                authorized_caller: user_principal(),
                release_id: "rel2".to_owned(),
                router_callback_principal: account,
            })
            .unwrap(),
        )
        .expect("accept_envelope call");
    let result: gleaph_graph_kernel::provisioning::wire::ProvisionIngressResult = Decode!(
        &bytes,
        gleaph_graph_kernel::provisioning::wire::ProvisionIngressResult
    )
    .expect("decode accept_envelope");
    assert!(
        matches!(
            result,
            gleaph_graph_kernel::provisioning::wire::ProvisionIngressResult::Err(
                gleaph_graph_kernel::provisioning::wire::ProvisionIngressError::NotAuthorized
            )
        ),
        "bootstrap principal must be rejected after handover: {result:?}"
    );
}
