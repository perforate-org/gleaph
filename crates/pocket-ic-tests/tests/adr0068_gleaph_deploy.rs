//! PocketIC coverage for the `gleaph deploy` flow (ADR 0068): Account registration and
//! first-Router issuance via Provision. Under the grant model each deploy is independent:
//! the granted Account (deployment = account principal) issues the first Router, and the
//! issued Router is auto-granted at install time — there is no bootstrap handover step.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse;
use gleaph_pocket_ic_tests::{install_account_canister, install_provision_canister, new_pocket_ic};

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
fn gleaph_deploy_registers_account_and_issues_first_router() {
    let pic = new_pocket_ic();
    let account = install_account_canister(&pic);
    // The Account canister is the granted issuer (deployment = account principal); it requests
    // the first-Router issuance through `authorize_router_issuance`.
    let provision = install_provision_canister(&pic, governance_principal(), &[account]);

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

    // 3. Deploys are independent: the Account remains a granted issuer and may request a
    //    further deployment (a graph shard) with no handover step. (No active release is
    //    seeded here, so admission returns Accepted with no created resources.)
    let shard = gleaph_graph_kernel::provisioning::wire::ProvisionableResource {
        logical_resource: gleaph_graph_kernel::provisioning::LogicalResource::GraphShard(
            gleaph_graph_kernel::federation::ShardId::new(2),
        ),
    };
    let bytes = pic
        .update_call(
            provision,
            account,
            "accept_envelope",
            Encode!(&gleaph_graph_kernel::provisioning::wire::ProvisionRequest {
                deployment_id: account.to_text(),
                request_id: gleaph_graph_kernel::provisioning::wire::provisioning_request_id(
                    "g2",
                    &[shard.clone()],
                ),
                intent_key: gleaph_graph_kernel::provisioning::ProvisioningIntentKey::new(
                    &account.to_text(),
                    gleaph_graph_kernel::provisioning::LogicalResource::GraphShard(
                        gleaph_graph_kernel::federation::ShardId::new(2),
                    ),
                ),
                reserved_graph_id: None,
                graph_name: "g2".to_owned(),
                requested_resources: vec![shard],
                install_args: vec![vec![0u8; 0]],
                authorized_caller: user_principal(),
                release_id: "rel2".to_owned(),
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
            gleaph_graph_kernel::provisioning::wire::ProvisionIngressResult::Ok(
                gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse::Accepted { .. }
            )
        ),
        "a granted issuer may request a further independent deploy: {result:?}"
    );
}
