//! PocketIC E2E for ADR 0068 lazy Router issuance through the Provision artifact catalog.
//!
//! Proves that `LogicalResource::Router` via `accept_envelope` installs a real, functioning Router
//! canister: the Account authorizes the first-Router issuance, Provision publishes the Router
//! artifact + release, installs a Router from it, and the issued Router responds to a query. This
//! is the production path that replaces the dev-mode manual `gleaph deploy` install.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse;
use gleaph_pocket_ic_tests::CanisterWasm;
use gleaph_pocket_ic_tests::{
    install_account_canister, install_provision_canister_with_scopes, new_pocket_ic,
};
use gleaph_provision::types::{
    ArtifactId, ArtifactPublishMetadataArgs, ArtifactUploadChunkArgs, CanisterKind,
    DeploymentCanister, DeploymentJobSummary, DeploymentQueryError, DeploymentRecord,
    FailedCanisterRepair, ProvisionJobView, ReleaseActivateArgs, ReleaseId, ReleasePublishArgs,
    ServiceLedger, WithdrawArgs, WithdrawError, WithdrawResult, WithdrawalKind, WithdrawalRecord,
    WithdrawalStatus, sha256,
};

const CHUNK_SIZE: usize = 1024 * 1024;

struct Env {
    pic: pocket_ic::PocketIc,
    user: Principal,
    admin: Principal,
    account: Principal,
    provision: Principal,
}

fn user_principal() -> Principal {
    Principal::from_slice(&[0x30; 29])
}

fn admin_principal() -> Principal {
    Principal::from_slice(&[0x64; 29])
}

/// Install Account + Provision with the Account canister as the granted issuer. Under the grant
/// model the deployment is the issuer itself: the Account canister (deployment = account
/// principal) requests the first-Router issuance, and the issued Router is auto-granted at
/// install time.
fn env() -> Env {
    let pic = new_pocket_ic();
    let user = user_principal();
    let admin = admin_principal();
    let account = install_account_canister(&pic);
    let provision = install_provision_canister_with_scopes(
        &pic,
        admin,
        &[(
            account,
            gleaph_provision::types::DeploymentGrantScope::Relay,
        )],
    );
    pic.add_cycles(provision, 100_000_000_000_000);
    Env {
        pic,
        user,
        admin,
        account,
        provision,
    }
}

/// A tiny valid WebAssembly module with `canister_init` and `memory` exports, usable as a dummy
/// artifact for the three release kinds that this test never installs.
fn minimal_canister_wasm() -> Vec<u8> {
    vec![
        0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, // magic + version
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section: () -> ()
        0x03, 0x02, 0x01, 0x00, // function section
        0x05, 0x03, 0x01, 0x00, 0x01, // memory section: 1 page
        0x07, 0x1A, // export section
        0x02, // 2 exports
        0x0D, 0x63, 0x61, 0x6E, 0x69, 0x73, 0x74, 0x65, 0x72, 0x5F, 0x69, 0x6E, 0x69, 0x74, 0x00,
        0x00, // func export "canister_init"
        0x06, 0x6D, 0x65, 0x6D, 0x6F, 0x72, 0x79, 0x02, 0x00, // memory export "memory"
        0x0A, 0x04, 0x01, 0x02, 0x00, 0x0B, // code section
    ]
}

/// Publish one single-chunk artifact (metadata + upload) into the Provision store. The caller
/// passes a wasm whose SHA-256 is recorded; the release manifest then references this id.
fn publish_artifact(env: &Env, kind: CanisterKind, version: &str, wasm: &[u8]) -> ArtifactId {
    let full_sha = sha256(wasm);
    let id = ArtifactId::new(kind.clone(), version.to_owned(), full_sha);
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "artifact_publish_metadata",
            Encode!(&ArtifactPublishMetadataArgs {
                canister_kind: kind,
                semantic_version: version.to_owned(),
                sha256: full_sha,
                byte_length: wasm.len() as u64,
                chunk_hashes: vec![full_sha],
            })
            .expect("encode artifact_publish_metadata"),
        )
        .unwrap_or_else(|e| panic!("artifact_publish_metadata: {e:?}"));
    let _: Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError> =
        Decode!(&bytes, Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError>)
            .expect("decode artifact_publish_metadata");
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "artifact_upload_chunk",
            Encode!(&ArtifactUploadChunkArgs {
                artifact_id: id.clone(),
                chunk_index: 0,
                bytes: wasm.to_vec(),
            })
            .expect("encode artifact_upload_chunk"),
        )
        .unwrap_or_else(|e| panic!("artifact_upload_chunk: {e:?}"));
    let _: Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError> =
        Decode!(&bytes, Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError>)
            .expect("decode artifact_upload_chunk");
    id
}

/// Publish the real Router artifact and activate a release so the Provision canister can install
/// a Router on demand.
fn activate_release(env: &Env) {
    let router_wasm = gleaph_pocket_ic_tests::wasm_bytes(CanisterWasm::Router);
    let full_sha = sha256(&router_wasm);
    let mut chunk_hashes = Vec::new();
    let mut offset = 0;
    while offset < router_wasm.len() {
        let end = (offset + CHUNK_SIZE).min(router_wasm.len());
        chunk_hashes.push(sha256(&router_wasm[offset..end]));
        offset = end;
    }
    let router_id = ArtifactId::new(CanisterKind::Router, "0.1.0".to_owned(), full_sha);

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "artifact_publish_metadata",
            Encode!(&ArtifactPublishMetadataArgs {
                canister_kind: CanisterKind::Router,
                semantic_version: "0.1.0".to_owned(),
                sha256: full_sha,
                byte_length: router_wasm.len() as u64,
                chunk_hashes: chunk_hashes.clone(),
            })
            .expect("encode artifact_publish_metadata"),
        )
        .unwrap_or_else(|e| panic!("artifact_publish_metadata: {e:?}"));
    let _: Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError> =
        Decode!(&bytes, Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError>)
            .expect("decode artifact_publish_metadata");

    // Upload each chunk individually.
    let mut offset = 0;
    let mut chunk_index = 0u32;
    while offset < router_wasm.len() {
        let end = (offset + CHUNK_SIZE).min(router_wasm.len());
        let bytes = env
            .pic
            .update_call(
                env.provision,
                env.admin,
                "artifact_upload_chunk",
                Encode!(&ArtifactUploadChunkArgs {
                    artifact_id: router_id.clone(),
                    chunk_index,
                    bytes: router_wasm[offset..end].to_vec(),
                })
                .expect("encode artifact_upload_chunk"),
            )
            .unwrap_or_else(|e| panic!("artifact_upload_chunk: {e:?}"));
        let _: Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError> =
            Decode!(&bytes, Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError>)
                .expect("decode artifact_upload_chunk");
        offset = end;
        chunk_index += 1;
    }

    // Publish the release with all five kinds (the manifest requires them). The other four kinds
    // use a minimal valid wasm that is never installed by this test.
    let minimal = minimal_canister_wasm();
    let minimal_sha = sha256(&minimal);
    let graph_id = ArtifactId::new(CanisterKind::Graph, "0.1.0".to_owned(), minimal_sha);
    publish_artifact(env, CanisterKind::Graph, "0.1.0", &minimal);
    let prop_id = ArtifactId::new(CanisterKind::PropertyIndex, "0.1.0".to_owned(), minimal_sha);
    publish_artifact(env, CanisterKind::PropertyIndex, "0.1.0", &minimal);
    let vec_id = ArtifactId::new(
        CanisterKind::VectorCanister,
        "0.1.0".to_owned(),
        minimal_sha,
    );
    publish_artifact(env, CanisterKind::VectorCanister, "0.1.0", &minimal);
    let text_id = ArtifactId::new(CanisterKind::TextCanister, "0.1.0".to_owned(), minimal_sha);
    publish_artifact(env, CanisterKind::TextCanister, "0.1.0", &minimal);
    let inference_id = ArtifactId::new(CanisterKind::Inference, "0.1.0".to_owned(), minimal_sha);
    publish_artifact(env, CanisterKind::Inference, "0.1.0", &minimal);

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "release_publish",
            Encode!(&ReleasePublishArgs {
                release_id: ReleaseId("release-router".to_owned()),
                artifact_ids: vec![router_id, graph_id, prop_id, vec_id, text_id, inference_id,],
            })
            .expect("encode release_publish"),
        )
        .unwrap_or_else(|e| panic!("release_publish: {e:?}"));
    let published: Result<
        gleaph_provision::types::ReleaseManifest,
        gleaph_provision::types::ReleaseError,
    > = Decode!(
        &bytes,
        Result<gleaph_provision::types::ReleaseManifest, gleaph_provision::types::ReleaseError>
    )
    .expect("decode release_publish");
    published.expect("release_publish must succeed");

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "release_activate",
            Encode!(&ReleaseActivateArgs {
                release_id: ReleaseId("release-router".to_owned()),
            })
            .expect("encode release_activate"),
        )
        .unwrap_or_else(|e| panic!("release_activate: {e:?}"));
    let result: Result<gleaph_provision::types::ReleaseActivateResult, gleaph_provision::types::ReleaseError> =
        Decode!(&bytes, Result<gleaph_provision::types::ReleaseActivateResult, gleaph_provision::types::ReleaseError>)
            .expect("decode release_activate");
    assert!(result.is_ok(), "release_activate must succeed: {result:?}");
}

#[test]
fn router_issuance_installs_a_functioning_router() {
    let env = env();
    activate_release(&env);

    // User registers a Personal account.
    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "create_account",
            Encode!(&"alice".to_owned()).unwrap(),
        )
        .expect("create_account call");
    let created: Result<gleaph_account::types::Account, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<gleaph_account::types::Account, gleaph_account::types::AccountError>)
            .expect("decode create_account");
    let account_id = created.expect("create_account").id();

    // User authorizes the first-Router issuance (Account -> Provision accept_envelope). The
    // endpoint takes three separate Candid arguments.
    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "authorize_router_issuance",
            candid::encode_args((
                &account_id.clone(),
                &"default".to_owned(),
                &env.provision,
                &gleaph_graph_kernel::provisioning::ControllerMode::SelfCustody,
            ))
            .expect("encode authorize_router_issuance args"),
        )
        .expect("authorize_router_issuance call");
    let result: Result<ProvisionAcceptResponse, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<ProvisionAcceptResponse, gleaph_account::types::AccountError>)
            .expect("decode authorize_router_issuance");
    let response = result.expect("authorize_router_issuance");
    let created_resources = match &response {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => created_resources,
        _ => panic!("expected Accepted, got {response:?}"),
    };
    assert!(
        created_resources.len() == 1,
        "must create exactly one Router, got {created_resources:?}"
    );
    let router_canister = created_resources[0].canister_id;
    assert!(
        matches!(
            created_resources[0].logical_resource,
            gleaph_graph_kernel::provisioning::LogicalResource::Router
        ),
        "created resource must be the Router, got {:?}",
        created_resources[0].logical_resource
    );

    // The issued Router must be a live canister that answers the `whoami` query.
    let bytes = env
        .pic
        .query_call(router_canister, env.user, "whoami", Encode!(&()).unwrap())
        .expect("whoami on issued Router");
    let caller: Principal = Decode!(&bytes, Principal).expect("decode whoami");
    assert_eq!(caller, env.user, "issued Router must respond to whoami");
}

#[test]
fn account_first_router_pays_the_quoted_admission() {
    let env = env();
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_set_service_config",
            Encode!(&gleaph_provision::types::ServiceConfig {
                version: 3,
                require_prepaid_admission: true,
                platform_fee_bps: 100,
                reserve_bps: 50,
                operator: Some(env.admin),
                ..gleaph_provision::types::ServiceConfig::default()
            })
            .expect("encode ServiceConfig"),
        )
        .expect("admin_set_service_config");
    let config: Result<
        gleaph_provision::types::ServiceConfig,
        gleaph_provision::types::ServiceConfigUpdateError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_provision::types::ServiceConfig,
            gleaph_provision::types::ServiceConfigUpdateError,
        >
    )
    .expect("decode admin_set_service_config");
    config.expect("enable paid admission");
    activate_release(&env);

    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "create_account",
            Encode!(&"paid".to_owned()).unwrap(),
        )
        .expect("create_account call");
    let created: Result<gleaph_account::types::Account, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<gleaph_account::types::Account, gleaph_account::types::AccountError>)
            .expect("decode create_account");
    let account_id = created.expect("create_account").id();

    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "authorize_router_issuance",
            candid::encode_args((
                &account_id.clone(),
                &"default".to_owned(),
                &env.provision,
                &gleaph_graph_kernel::provisioning::ControllerMode::SelfCustody,
            ))
            .expect("encode authorize_router_issuance args"),
        )
        .expect("authorize_router_issuance call");
    let result: Result<ProvisionAcceptResponse, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<ProvisionAcceptResponse, gleaph_account::types::AccountError>)
            .expect("decode authorize_router_issuance");
    let response = result.expect("paid authorize_router_issuance");
    match response {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => assert_eq!(created_resources.len(), 1),
        other => panic!("expected Accepted, got {other:?}"),
    }

    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.admin,
            "get_deployment",
            Encode!(&account_id).expect("encode deployment query"),
        )
        .expect("get_deployment query");
    let deployment: Result<DeploymentRecord, DeploymentQueryError> =
        Decode!(&bytes, Result<DeploymentRecord, DeploymentQueryError>).expect("decode deployment");
    let deployment = deployment.expect("deployment query");
    assert_ne!(deployment.owner, Principal::anonymous());

    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.admin,
            "get_deployment_canisters",
            Encode!(&account_id).expect("encode deployment canisters query"),
        )
        .expect("get_deployment_canisters query");
    let canisters: Result<Vec<DeploymentCanister>, DeploymentQueryError> = Decode!(
        &bytes,
        Result<Vec<DeploymentCanister>, DeploymentQueryError>
    )
    .expect("decode deployment canisters");
    let canisters = canisters.expect("deployment canisters query");
    assert!(
        canisters.iter().any(|record| record.active),
        "issued deployment must record at least one active canister"
    );

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "repair_failed_deployment_canisters",
            Encode!(&account_id).expect("encode repair call"),
        )
        .expect("repair_failed_deployment_canisters call");
    let repaired: Result<Vec<FailedCanisterRepair>, DeploymentQueryError> = Decode!(
        &bytes,
        Result<Vec<FailedCanisterRepair>, DeploymentQueryError>
    )
    .expect("decode repair result");
    assert!(
        repaired.expect("repair call").is_empty(),
        "an active deployment has no failed-job canisters to repair"
    );

    // The operator collects the accrued fee from the service ledger. The quote is the single
    // source for the amount, so the test never re-derives the fee formula.
    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.admin,
            "quote_admission",
            Encode!(&1u32).expect("encode quote"),
        )
        .expect("quote_admission query");
    let quote: Result<
        gleaph_graph_kernel::provisioning::wire::AdmissionQuote,
        gleaph_graph_kernel::provisioning::wire::AdmissionQuoteError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_graph_kernel::provisioning::wire::AdmissionQuote,
            gleaph_graph_kernel::provisioning::wire::AdmissionQuoteError,
        >
    )
    .expect("decode quote");
    let quote = quote.expect("quote");
    assert!(quote.fee > 0, "configured fee must accrue");

    let destination = env.pic.create_canister();
    let before = env.pic.cycle_balance(destination);
    let args = WithdrawArgs {
        idempotency_key: [0x5A; 32],
        kind: WithdrawalKind::PlatformFees,
        destination,
        amount: quote.fee,
    };
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "withdraw_fees",
            Encode!(&args).expect("encode withdraw"),
        )
        .expect("withdraw_fees call");
    let withdrawn: Result<WithdrawResult, WithdrawError> =
        Decode!(&bytes, Result<WithdrawResult, WithdrawError>).expect("decode withdraw");
    assert_eq!(
        withdrawn.expect("withdraw"),
        WithdrawResult {
            amount: quote.fee,
            replayed: false,
        }
    );
    assert_eq!(
        env.pic.cycle_balance(destination),
        before + quote.fee,
        "the destination canister receives the withdrawn fee cycles"
    );

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "withdraw_fees",
            Encode!(&args).expect("encode replay"),
        )
        .expect("withdraw_fees replay");
    let replayed: Result<WithdrawResult, WithdrawError> =
        Decode!(&bytes, Result<WithdrawResult, WithdrawError>).expect("decode replay");
    assert_eq!(
        replayed.expect("replay"),
        WithdrawResult {
            amount: quote.fee,
            replayed: true,
        }
    );
    assert_eq!(
        env.pic.cycle_balance(destination),
        before + quote.fee,
        "an idempotent replay must not transfer a second time"
    );

    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.admin,
            "get_service_ledger",
            Encode!(&()).expect("encode ledger query"),
        )
        .expect("get_service_ledger query");
    let ledger: Result<ServiceLedger, WithdrawError> =
        Decode!(&bytes, Result<ServiceLedger, WithdrawError>).expect("decode ledger");
    let ledger = ledger.expect("ledger query");
    assert_eq!(ledger.platform_fees, 0, "the fee was fully withdrawn");
    assert!(
        ledger.compensation_reserve > 0,
        "the reserve stays withheld"
    );

    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.admin,
            "get_withdrawal",
            Encode!(&args.idempotency_key).expect("encode withdrawal query"),
        )
        .expect("get_withdrawal query");
    let record: Result<Option<WithdrawalRecord>, WithdrawError> =
        Decode!(&bytes, Result<Option<WithdrawalRecord>, WithdrawError>)
            .expect("decode withdrawal");
    assert_eq!(
        record.expect("withdrawal query").expect("record").status,
        WithdrawalStatus::Completed
    );

    // A definitive rejection restores the reserve pool, while the failed key stays terminal
    // and a fresh key can retry the transfer.
    let bad_destination = Principal::from_slice(&[0x99; 29]);
    let failed_key = [0x5C; 32];
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "withdraw_fees",
            Encode!(&WithdrawArgs {
                idempotency_key: failed_key,
                kind: WithdrawalKind::CompensationReserve,
                destination: bad_destination,
                amount: quote.reserve,
            })
            .expect("encode failed withdrawal"),
        )
        .expect("withdraw_fees failed call");
    let failed: Result<WithdrawResult, WithdrawError> =
        Decode!(&bytes, Result<WithdrawResult, WithdrawError>).expect("decode failed");
    assert!(
        matches!(failed, Err(WithdrawError::TransferFailed(_))),
        "a non-canister destination must fail definitively: {failed:?}"
    );
    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.admin,
            "get_service_ledger",
            Encode!(&()).expect("encode ledger query"),
        )
        .expect("get_service_ledger after failure");
    let restored: Result<ServiceLedger, WithdrawError> =
        Decode!(&bytes, Result<ServiceLedger, WithdrawError>).expect("decode ledger");
    assert_eq!(
        restored.expect("ledger after failure").compensation_reserve,
        quote.reserve,
        "a failed transfer restores the reserve pool"
    );
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "withdraw_fees",
            Encode!(&WithdrawArgs {
                idempotency_key: failed_key,
                kind: WithdrawalKind::CompensationReserve,
                destination: bad_destination,
                amount: quote.reserve,
            })
            .expect("encode failed-key replay"),
        )
        .expect("withdraw_fees failed-key replay");
    let replay_failed: Result<WithdrawResult, WithdrawError> =
        Decode!(&bytes, Result<WithdrawResult, WithdrawError>).expect("decode failed replay");
    assert!(
        matches!(replay_failed, Err(WithdrawError::PreviousAttemptFailed)),
        "a failed key cannot silently retry: {replay_failed:?}"
    );
    let reserve_destination = env.pic.create_canister();
    let reserve_before = env.pic.cycle_balance(reserve_destination);
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "withdraw_fees",
            Encode!(&WithdrawArgs {
                idempotency_key: [0x5D; 32],
                kind: WithdrawalKind::CompensationReserve,
                destination: reserve_destination,
                amount: quote.reserve,
            })
            .expect("encode reserve retry"),
        )
        .expect("withdraw_fees reserve retry");
    let reserve_retry: Result<WithdrawResult, WithdrawError> =
        Decode!(&bytes, Result<WithdrawResult, WithdrawError>).expect("decode reserve retry");
    assert_eq!(
        reserve_retry.expect("reserve retry"),
        WithdrawResult {
            amount: quote.reserve,
            replayed: false,
        }
    );
    assert_eq!(
        env.pic.cycle_balance(reserve_destination),
        reserve_before + quote.reserve
    );

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "withdraw_fees",
            Encode!(&WithdrawArgs {
                idempotency_key: [0x5B; 32],
                ..args
            })
            .expect("encode overdraft"),
        )
        .expect("withdraw_fees overdraft");
    let overdraft: Result<WithdrawResult, WithdrawError> =
        Decode!(&bytes, Result<WithdrawResult, WithdrawError>).expect("decode overdraft");
    assert!(
        matches!(overdraft, Err(WithdrawError::InsufficientLedger)),
        "the fee pool is empty after the first withdrawal: {overdraft:?}"
    );
}

/// ADR 0068 / ownership: a Managed deployment can move to SelfCustody by its owner, then a
/// governance transfer changes the SelfCustody owner. A separate Managed deployment exercises the
/// record-only `set_deployment_owner` path. Controller handover itself is covered by the
/// Provision unit tests; this E2E proves the public endpoints, auth, and state transitions.
#[test]
fn ownership_handover_flow() {
    use gleaph_graph_kernel::provisioning::ControllerMode;
    use gleaph_provision::types::{
        DeploymentOwnerUpdateError, InitializeDeploymentArgs, InitializeDeploymentError,
        SelfCustodyTransferError, TakeOwnershipError,
    };

    let env = env();
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_set_service_config",
            Encode!(&gleaph_provision::types::ServiceConfig {
                version: 3,
                require_prepaid_admission: false,
                operator: Some(env.admin),
                ..gleaph_provision::types::ServiceConfig::default()
            })
            .expect("encode ServiceConfig"),
        )
        .expect("admin_set_service_config");
    let _: Result<
        gleaph_provision::types::ServiceConfig,
        gleaph_provision::types::ServiceConfigUpdateError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_provision::types::ServiceConfig,
            gleaph_provision::types::ServiceConfigUpdateError,
        >
    )
    .expect("decode admin_set_service_config");

    let managed = Principal::from_slice(&[0x77; 29]);
    let second = Principal::from_slice(&[0x79; 29]);
    let new_owner = Principal::from_slice(&[0x78; 29]);

    for deployment in [managed, second] {
        let bytes = env
            .pic
            .update_call(
                env.provision,
                env.user,
                "initialize_deployment",
                Encode!(&InitializeDeploymentArgs {
                    deployment_id: deployment.into(),
                    owner: env.user,
                    controller_mode: ControllerMode::Managed,
                })
                .expect("encode initialize_deployment"),
            )
            .expect("initialize_deployment");
        let result: Result<DeploymentRecord, InitializeDeploymentError> = Decode!(
            &bytes,
            Result<DeploymentRecord, InitializeDeploymentError>
        )
        .expect("decode initialize_deployment");
        let record = result.expect("initialize Managed deployment");
        assert_eq!(record.controller_mode, ControllerMode::Managed);
        assert_eq!(record.operator, Some(env.admin));
    }

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.user,
            "take_ownership",
            Encode!(&managed).expect("encode take_ownership"),
        )
        .expect("take_ownership");
    let result: Result<DeploymentRecord, TakeOwnershipError> =
        Decode!(&bytes, Result<DeploymentRecord, TakeOwnershipError>)
            .expect("decode take_ownership");
    let record = result.expect("owner takes controllers");
    assert_eq!(record.controller_mode, ControllerMode::SelfCustody);
    assert_eq!(record.owner, env.user);
    assert_eq!(record.operator, None);

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_transfer_self_custody",
            Encode!(&managed, &new_owner).expect("encode transfer"),
        )
        .expect("admin_transfer_self_custody");
    let result: Result<DeploymentRecord, SelfCustodyTransferError> =
        Decode!(&bytes, Result<DeploymentRecord, SelfCustodyTransferError>)
            .expect("decode transfer");
    let record = result.expect("governance transfers SelfCustody owner");
    assert_eq!(record.owner, new_owner);
    assert_eq!(record.controller_mode, ControllerMode::SelfCustody);

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_set_deployment_owner",
            Encode!(&second, &new_owner).expect("encode set owner"),
        )
        .expect("admin_set_deployment_owner");
    let result: Result<DeploymentRecord, DeploymentOwnerUpdateError> = Decode!(
        &bytes,
        Result<DeploymentRecord, DeploymentOwnerUpdateError>
    )
    .expect("decode set owner");
    let record = result.expect("governance updates Managed owner");
    assert_eq!(record.owner, new_owner);
    assert_eq!(record.controller_mode, ControllerMode::Managed);
    assert_eq!(record.operator, Some(env.admin));
}

/// Billing reads: a deployment owner can list its grants, while grant status is scoped to the
/// configured billing authority (or governance).
#[test]
fn billing_grant_reads_are_scoped() {
    use gleaph_provision::types::{
        BillingGrantView, GrantCreditArgs, GrantCreditError, GrantCreditResult,
        InitializeDeploymentArgs, InitializeDeploymentError,
    };

    let env = env();
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_set_service_config",
            Encode!(&gleaph_provision::types::ServiceConfig {
                version: 3,
                operator: Some(env.admin),
                billing_authority: Some(env.admin),
                ..gleaph_provision::types::ServiceConfig::default()
            })
            .expect("encode ServiceConfig"),
        )
        .expect("admin_set_service_config");
    let _: Result<
        gleaph_provision::types::ServiceConfig,
        gleaph_provision::types::ServiceConfigUpdateError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_provision::types::ServiceConfig,
            gleaph_provision::types::ServiceConfigUpdateError,
        >
    )
    .expect("decode admin_set_service_config");

    let deployment = Principal::from_slice(&[0x7A; 29]);
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.user,
            "initialize_deployment",
            Encode!(&InitializeDeploymentArgs {
                deployment_id: deployment.into(),
                owner: env.user,
                controller_mode: gleaph_graph_kernel::provisioning::ControllerMode::SelfCustody,
            })
            .expect("encode initialize_deployment"),
        )
        .expect("initialize_deployment");
    let result: Result<DeploymentRecord, InitializeDeploymentError> =
        Decode!(&bytes, Result<DeploymentRecord, InitializeDeploymentError>)
            .expect("decode initialize_deployment");
    result.expect("initialize deployment");

    let key = [0x42; 32];
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "grant_credit",
            Encode!(&GrantCreditArgs {
                deployment_id: deployment.into(),
                amount: 1_000,
                idempotency_key: key,
            })
            .expect("encode grant_credit"),
        )
        .expect("grant_credit");
    let result: Result<GrantCreditResult, GrantCreditError> =
        Decode!(&bytes, Result<GrantCreditResult, GrantCreditError>).expect("decode grant_credit");
    result.expect("grant credit");

    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.user,
            "billing_grants",
            Encode!(&deployment).expect("encode billing_grants"),
        )
        .expect("billing_grants");
    let grants: Result<Vec<BillingGrantView>, DeploymentQueryError> =
        Decode!(&bytes, Result<Vec<BillingGrantView>, DeploymentQueryError>)
            .expect("decode billing_grants");
    let grants = grants.expect("owner lists grants");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].amount, 1_000);
    assert_eq!(grants[0].idempotency_key, key);

    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.admin,
            "billing_grant_status",
            Encode!(&env.admin, &key).expect("encode grant status"),
        )
        .expect("billing_grant_status");
    let grant: Result<Option<BillingGrantView>, DeploymentQueryError> = Decode!(
        &bytes,
        Result<Option<BillingGrantView>, DeploymentQueryError>
    )
    .expect("decode grant status");
    assert_eq!(
        grant.expect("grant status").expect("grant exists").amount,
        1_000
    );

    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.user,
            "billing_grant_status",
            Encode!(&env.admin, &key).expect("encode grant status"),
        )
        .expect("billing_grant_status as owner");
    let denied: Result<Option<BillingGrantView>, DeploymentQueryError> = Decode!(
        &bytes,
        Result<Option<BillingGrantView>, DeploymentQueryError>
    )
    .expect("decode grant status");
    assert_eq!(denied, Err(DeploymentQueryError::Unauthorized));
}

/// Ownership handover with a real canister: a Managed Router is issued directly by the relay,
/// the owner takes the controllers, then governance transfers the SelfCustody owner. Every
/// transition is checked through `canister_status`, not only the record.
#[test]
fn ownership_handover_moves_real_canister_controllers() {
    use gleaph_graph_kernel::provisioning::wire::{
        ProvisionIngressResult, ProvisionRequest, ProvisionableResource,
    };
    use gleaph_graph_kernel::provisioning::{
        ControllerMode, DeploymentId, LogicalResource, ProvisioningIntentKey,
    };
    use gleaph_provision::types::{SelfCustodyTransferError, TakeOwnershipError};

    let env = env();
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_set_service_config",
            Encode!(&gleaph_provision::types::ServiceConfig {
                version: 3,
                require_prepaid_admission: false,
                operator: Some(env.admin),
                ..gleaph_provision::types::ServiceConfig::default()
            })
            .expect("encode ServiceConfig"),
        )
        .expect("admin_set_service_config");
    let _: Result<
        gleaph_provision::types::ServiceConfig,
        gleaph_provision::types::ServiceConfigUpdateError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_provision::types::ServiceConfig,
            gleaph_provision::types::ServiceConfigUpdateError,
        >
    )
    .expect("decode admin_set_service_config");
    activate_release(&env);

    let deployment_id = DeploymentId::new(env.account);
    let resources = vec![ProvisionableResource {
        logical_resource: LogicalResource::Router,
    }];
    let router_init = gleaph_graph_kernel::provisioning::init_args::RouterInitArgs {
        issuing_principal: env.user,
        initial_admins: vec![],
        provision_canister: Some(env.provision),
    };
    let request = ProvisionRequest {
        deployment_id,
        request_id: gleaph_graph_kernel::provisioning::wire::provisioning_request_id(
            "default", &resources,
        ),
        intent_key: ProvisioningIntentKey::new(deployment_id, LogicalResource::Router),
        reserved_graph_id: None,
        graph_name: "default".to_owned(),
        requested_resources: resources,
        install_args: vec![Encode!(&router_init).expect("encode RouterInitArgs")],
        authorized_caller: env.user,
        release_id: "release-router".to_owned(),
        controller_mode: ControllerMode::Managed,
    };
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.account,
            "accept_envelope",
            Encode!(&request).expect("encode accept_envelope"),
        )
        .expect("accept_envelope");
    let result: ProvisionIngressResult =
        Decode!(&bytes, ProvisionIngressResult).expect("decode accept_envelope");
    let response = match result {
        ProvisionIngressResult::Ok(response) => response,
        ProvisionIngressResult::Err(error) => panic!("Managed issuance rejected: {error:?}"),
    };
    let canister = match response {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => created_resources[0].canister_id,
        other => panic!("expected Accepted, got {other:?}"),
    };
    let status = env
        .pic
        .canister_status(canister, Some(env.admin))
        .expect("managed canister status");
    assert_eq!(status.settings.controllers, vec![env.provision, env.admin]);

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.user,
            "take_ownership",
            Encode!(&env.account).expect("encode take_ownership"),
        )
        .expect("take_ownership");
    let result: Result<DeploymentRecord, TakeOwnershipError> =
        Decode!(&bytes, Result<DeploymentRecord, TakeOwnershipError>)
            .expect("decode take_ownership");
    result.expect("owner takes controllers");
    let status = env
        .pic
        .canister_status(canister, Some(env.user))
        .expect("self-custody canister status");
    assert_eq!(status.settings.controllers, vec![env.provision, env.user]);

    let new_owner = Principal::from_slice(&[0x78; 29]);
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_transfer_self_custody",
            Encode!(&env.account, &new_owner).expect("encode transfer"),
        )
        .expect("admin_transfer_self_custody");
    let result: Result<DeploymentRecord, SelfCustodyTransferError> =
        Decode!(&bytes, Result<DeploymentRecord, SelfCustodyTransferError>)
            .expect("decode transfer");
    result.expect("governance transfers SelfCustody owner");
    let status = env
        .pic
        .canister_status(canister, Some(new_owner))
        .expect("transferred canister status");
    assert_eq!(status.settings.controllers, vec![env.provision, new_owner]);
}

/// Managed hosting customer flow: the Account owner chooses Managed at first-Router issuance.
/// Provision installs the Router under [Provision, operator]; the owner can then take it over.
#[test]
fn account_can_request_a_managed_router_then_take_ownership() {
    use gleaph_graph_kernel::provisioning::ControllerMode;
    use gleaph_provision::types::TakeOwnershipError;

    let env = env();
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_set_service_config",
            Encode!(&gleaph_provision::types::ServiceConfig {
                version: 3,
                require_prepaid_admission: false,
                operator: Some(env.admin),
                ..gleaph_provision::types::ServiceConfig::default()
            })
            .expect("encode ServiceConfig"),
        )
        .expect("admin_set_service_config");
    let _: Result<
        gleaph_provision::types::ServiceConfig,
        gleaph_provision::types::ServiceConfigUpdateError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_provision::types::ServiceConfig,
            gleaph_provision::types::ServiceConfigUpdateError,
        >
    )
    .expect("decode admin_set_service_config");
    activate_release(&env);

    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "create_account",
            Encode!(&"managed-alice".to_owned()).expect("encode create_account"),
        )
        .expect("create_account");
    let created: Result<gleaph_account::types::Account, gleaph_account::types::AccountError> =
        Decode!(
            &bytes,
            Result<gleaph_account::types::Account, gleaph_account::types::AccountError>
        )
        .expect("decode create_account");
    let account_id = created.expect("create_account").id();

    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "authorize_router_issuance",
            Encode!(
                &account_id,
                &"default".to_owned(),
                &env.provision,
                &ControllerMode::Managed
            )
            .expect("encode authorize_router_issuance"),
        )
        .expect("authorize_router_issuance");
    let result: Result<ProvisionAcceptResponse, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<ProvisionAcceptResponse, gleaph_account::types::AccountError>)
            .expect("decode authorize_router_issuance");
    let response = result.expect("Managed issuance through Account");
    let canister = match response {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => created_resources[0].canister_id,
        other => panic!("expected Accepted, got {other:?}"),
    };
    let status = env
        .pic
        .canister_status(canister, Some(env.admin))
        .expect("managed canister status");
    assert_eq!(status.settings.controllers, vec![env.provision, env.admin]);

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.user,
            "take_ownership",
            Encode!(&account_id).expect("encode take_ownership"),
        )
        .expect("take_ownership");
    let result: Result<DeploymentRecord, TakeOwnershipError> =
        Decode!(&bytes, Result<DeploymentRecord, TakeOwnershipError>)
            .expect("decode take_ownership");
    result.expect("owner takes Managed Router");
    let status = env
        .pic
        .canister_status(canister, Some(env.user))
        .expect("self-custody canister status");
    assert_eq!(status.settings.controllers, vec![env.provision, env.user]);
}

/// Managed customer transfer: governance changes the owner while the operator keeps control,
/// then the new owner takes the canisters. The previous owner is rejected after the transfer.
#[test]
fn managed_owner_transfer_then_new_owner_takes_ownership() {
    use gleaph_graph_kernel::provisioning::ControllerMode;
    use gleaph_provision::types::{DeploymentOwnerUpdateError, TakeOwnershipError};

    let env = env();
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_set_service_config",
            Encode!(&gleaph_provision::types::ServiceConfig {
                version: 3,
                require_prepaid_admission: false,
                operator: Some(env.admin),
                ..gleaph_provision::types::ServiceConfig::default()
            })
            .expect("encode ServiceConfig"),
        )
        .expect("admin_set_service_config");
    let _: Result<
        gleaph_provision::types::ServiceConfig,
        gleaph_provision::types::ServiceConfigUpdateError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_provision::types::ServiceConfig,
            gleaph_provision::types::ServiceConfigUpdateError,
        >
    )
    .expect("decode admin_set_service_config");
    activate_release(&env);

    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "create_account",
            Encode!(&"managed-transfer".to_owned()).expect("encode create_account"),
        )
        .expect("create_account");
    let created: Result<gleaph_account::types::Account, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<gleaph_account::types::Account, gleaph_account::types::AccountError>)
            .expect("decode create_account");
    let account_id = created.expect("create_account").id();
    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "authorize_router_issuance",
            Encode!(
                &account_id,
                &"default".to_owned(),
                &env.provision,
                &ControllerMode::Managed
            )
            .expect("encode authorize_router_issuance"),
        )
        .expect("authorize_router_issuance");
    let result: Result<ProvisionAcceptResponse, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<ProvisionAcceptResponse, gleaph_account::types::AccountError>)
            .expect("decode authorize_router_issuance");
    let response = result.expect("Managed issuance through Account");
    let canister = match response {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => created_resources[0].canister_id,
        other => panic!("expected Accepted, got {other:?}"),
    };

    let new_owner = Principal::from_slice(&[0x78; 29]);
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_set_deployment_owner",
            Encode!(&account_id, &new_owner).expect("encode set owner"),
        )
        .expect("admin_set_deployment_owner");
    let result: Result<DeploymentRecord, DeploymentOwnerUpdateError> = Decode!(
        &bytes,
        Result<DeploymentRecord, DeploymentOwnerUpdateError>
    )
    .expect("decode set owner");
    let record = result.expect("governance updates Managed owner");
    assert_eq!(record.owner, new_owner);
    assert_eq!(record.controller_mode, ControllerMode::Managed);
    let status = env
        .pic
        .canister_status(canister, Some(env.admin))
        .expect("managed canister status");
    assert_eq!(
        status.settings.controllers,
        vec![env.provision, env.admin],
        "owner transfer must not move controllers"
    );

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.user,
            "take_ownership",
            Encode!(&account_id).expect("encode old-owner take"),
        )
        .expect("old-owner take_ownership");
    let result: Result<DeploymentRecord, TakeOwnershipError> =
        Decode!(&bytes, Result<DeploymentRecord, TakeOwnershipError>)
            .expect("decode old-owner take");
    assert_eq!(result, Err(TakeOwnershipError::Unauthorized));

    let bytes = env
        .pic
        .update_call(
            env.provision,
            new_owner,
            "take_ownership",
            Encode!(&account_id).expect("encode new-owner take"),
        )
        .expect("new-owner take_ownership");
    let result: Result<DeploymentRecord, TakeOwnershipError> =
        Decode!(&bytes, Result<DeploymentRecord, TakeOwnershipError>)
            .expect("decode new-owner take");
    result.expect("new owner takes Managed Router");
    let status = env
        .pic
        .canister_status(canister, Some(new_owner))
        .expect("self-custody canister status");
    assert_eq!(status.settings.controllers, vec![env.provision, new_owner]);
}

/// Reversible suspension: the owner stops the deployment, new issuance is rejected, and resume
/// starts the canister again. No canister or data is deleted.
#[test]
fn deployment_suspension_stops_and_resumes_a_router() {
    use gleaph_graph_kernel::provisioning::ControllerMode;
    use gleaph_provision::types::{
        DeploymentTermination, DeploymentTerminationError, DeploymentTerminationState,
    };

    let env = env();
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_set_service_config",
            Encode!(&gleaph_provision::types::ServiceConfig {
                version: 3,
                require_prepaid_admission: false,
                operator: Some(env.admin),
                ..gleaph_provision::types::ServiceConfig::default()
            })
            .expect("encode ServiceConfig"),
        )
        .expect("admin_set_service_config");
    let _: Result<
        gleaph_provision::types::ServiceConfig,
        gleaph_provision::types::ServiceConfigUpdateError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_provision::types::ServiceConfig,
            gleaph_provision::types::ServiceConfigUpdateError,
        >
    )
    .expect("decode admin_set_service_config");
    activate_release(&env);

    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "create_account",
            Encode!(&"managed-suspend".to_owned()).expect("encode create_account"),
        )
        .expect("create_account");
    let created: Result<gleaph_account::types::Account, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<gleaph_account::types::Account, gleaph_account::types::AccountError>)
            .expect("decode create_account");
    let account_id = created.expect("create_account").id();
    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "authorize_router_issuance",
            Encode!(
                &account_id,
                &"default".to_owned(),
                &env.provision,
                &ControllerMode::Managed
            )
            .expect("encode authorize_router_issuance"),
        )
        .expect("authorize_router_issuance");
    let result: Result<ProvisionAcceptResponse, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<ProvisionAcceptResponse, gleaph_account::types::AccountError>)
            .expect("decode authorize_router_issuance");
    let response = result.expect("Managed issuance through Account");
    let (canister, request_id) = match response {
        ProvisionAcceptResponse::Accepted {
            created_resources,
            job_view,
            ..
        } => (created_resources[0].canister_id, job_view.request_id),
        other => panic!("expected Accepted, got {other:?}"),
    };
    let ack = gleaph_graph_kernel::provisioning::wire::RouterRegistrationAck {
        request_id,
        deployment_id: account_id.into(),
    };
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.account,
            "complete_graph_registration",
            Encode!(&ack).expect("encode complete_graph_registration"),
        )
        .expect("complete_graph_registration");
    let _: gleaph_graph_kernel::provisioning::wire::RouterRegistrationAckResult = Decode!(
        &bytes,
        gleaph_graph_kernel::provisioning::wire::RouterRegistrationAckResult
    )
    .expect("decode complete_graph_registration");

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.user,
            "request_deployment_termination",
            Encode!(&account_id, &"customer request".to_owned()).expect("encode suspend"),
        )
        .expect("request_deployment_termination");
    let result: Result<DeploymentTermination, DeploymentTerminationError> =
        Decode!(&bytes, Result<DeploymentTermination, DeploymentTerminationError>)
            .expect("decode suspend");
    let suspended = result.expect("owner suspends");
    assert_eq!(suspended.state, DeploymentTerminationState::Suspended);
    let status = env
        .pic
        .canister_status(canister, Some(env.admin))
        .expect("suspended canister status");
    assert!(
        format!("{:?}", status.status).contains("Stopped"),
        "canister must be stopped: {:?}",
        status.status
    );

    // A suspended deployment rejects new envelopes before the singleton/credit checks.
    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "authorize_router_issuance",
            Encode!(
                &account_id,
                &"default".to_owned(),
                &env.provision,
                &ControllerMode::Managed
            )
            .expect("encode suspended issuance"),
        )
        .expect("authorize_router_issuance while suspended");
    let result: Result<ProvisionAcceptResponse, gleaph_account::types::AccountError> = Decode!(
        &bytes,
        Result<ProvisionAcceptResponse, gleaph_account::types::AccountError>
    )
    .expect("decode suspended issuance");
    let error = result.expect_err("suspended deployment must reject issuance");
    assert!(
        format!("{error:?}").contains("DeploymentSuspended"),
        "expected DeploymentSuspended, got {error:?}"
    );

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.user,
            "cancel_deployment_termination",
            Encode!(&account_id).expect("encode resume"),
        )
        .expect("cancel_deployment_termination");
    let result: Result<DeploymentTermination, DeploymentTerminationError> =
        Decode!(&bytes, Result<DeploymentTermination, DeploymentTerminationError>)
            .expect("decode resume");
    let resumed = result.expect("owner resumes");
    assert_eq!(resumed.state, DeploymentTerminationState::Cancelled);
    let status = env
        .pic
        .canister_status(canister, Some(env.admin))
        .expect("resumed canister status");
    assert!(
        format!("{:?}", status.status).contains("Running"),
        "canister must be running: {:?}",
        status.status
    );

    // Free admission quotes zero cycles, so the deployment credit stays zero and deletion is
    // allowed. Governance force skips the 30-day grace for the test.
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.user,
            "request_deployment_termination",
            Encode!(&account_id, &"delete".to_owned()).expect("encode suspend again"),
        )
        .expect("request_deployment_termination again");
    let result: Result<DeploymentTermination, DeploymentTerminationError> =
        Decode!(&bytes, Result<DeploymentTermination, DeploymentTerminationError>)
            .expect("decode suspend again");
    result.expect("suspend before delete");

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "delete_deployment_canisters",
            Encode!(&account_id, &account_id.to_string(), &true).expect("encode delete"),
        )
        .expect("delete_deployment_canisters");
    let result: Result<DeploymentTermination, DeploymentTerminationError> =
        Decode!(&bytes, Result<DeploymentTermination, DeploymentTerminationError>)
            .expect("decode delete");
    let deleted = result.expect("governance deletes suspended deployment");
    assert_eq!(deleted.state, DeploymentTerminationState::Deleted);
    assert!(
        env.pic.canister_status(canister, Some(env.admin)).is_err(),
        "deleted canister must no longer exist"
    );
}

/// Paid-mode settlement before deletion: a granted credit is reversed to zero, then the suspended
/// deployment is deleted. Deletion refuses a positive credit, so this is the operator path.
#[test]
fn settled_credit_allows_deletion() {
    use gleaph_graph_kernel::provisioning::ControllerMode;
    use gleaph_provision::types::{
        DeploymentTermination, DeploymentTerminationError, GrantCreditArgs, GrantCreditError,
        GrantCreditResult, ReverseCreditArgs, ReverseCreditError, ReverseCreditResult,
    };

    let env = env();
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_set_service_config",
            Encode!(&gleaph_provision::types::ServiceConfig {
                version: 3,
                require_prepaid_admission: false,
                operator: Some(env.admin),
                billing_authority: Some(env.admin),
                ..gleaph_provision::types::ServiceConfig::default()
            })
            .expect("encode ServiceConfig"),
        )
        .expect("admin_set_service_config");
    let _: Result<
        gleaph_provision::types::ServiceConfig,
        gleaph_provision::types::ServiceConfigUpdateError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_provision::types::ServiceConfig,
            gleaph_provision::types::ServiceConfigUpdateError,
        >
    )
    .expect("decode admin_set_service_config");
    activate_release(&env);

    let account_id = env.account;
    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "create_account",
            Encode!(&"settled-delete".to_owned()).expect("encode create_account"),
        )
        .expect("create_account");
    let created: Result<gleaph_account::types::Account, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<gleaph_account::types::Account, gleaph_account::types::AccountError>)
            .expect("decode create_account");
    let account_id = created.expect("create_account").id();
    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "authorize_router_issuance",
            Encode!(
                &account_id,
                &"default".to_owned(),
                &env.provision,
                &ControllerMode::Managed
            )
            .expect("encode authorize_router_issuance"),
        )
        .expect("authorize_router_issuance");
    let result: Result<ProvisionAcceptResponse, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<ProvisionAcceptResponse, gleaph_account::types::AccountError>)
            .expect("decode authorize_router_issuance");
    let response = result.expect("Managed issuance through Account");
    let request_id = match response {
        ProvisionAcceptResponse::Accepted { job_view, .. } => job_view.request_id,
        other => panic!("expected Accepted, got {other:?}"),
    };
    let ack = gleaph_graph_kernel::provisioning::wire::RouterRegistrationAck {
        request_id,
        deployment_id: account_id.into(),
    };
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.account,
            "complete_graph_registration",
            Encode!(&ack).expect("encode complete_graph_registration"),
        )
        .expect("complete_graph_registration");
    let _: gleaph_graph_kernel::provisioning::wire::RouterRegistrationAckResult = Decode!(
        &bytes,
        gleaph_graph_kernel::provisioning::wire::RouterRegistrationAckResult
    )
    .expect("decode complete_graph_registration");

    let key = [0x11; 32];
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "grant_credit",
            Encode!(&GrantCreditArgs {
                deployment_id: account_id.into(),
                amount: 1_000_000_000_000,
                idempotency_key: key,
            })
            .expect("encode grant_credit"),
        )
        .expect("grant_credit");
    let result: Result<GrantCreditResult, GrantCreditError> =
        Decode!(&bytes, Result<GrantCreditResult, GrantCreditError>).expect("decode grant_credit");
    result.expect("billing authority grants");

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "reverse_credit",
            Encode!(&ReverseCreditArgs {
                grant_authority: env.admin,
                idempotency_key: key,
            })
            .expect("encode reverse_credit"),
        )
        .expect("reverse_credit");
    let result: Result<ReverseCreditResult, ReverseCreditError> =
        Decode!(&bytes, Result<ReverseCreditResult, ReverseCreditError>)
            .expect("decode reverse_credit");
    let reversed = result.expect("billing authority reverses");
    assert_eq!(reversed.credit_cycles, 0);

    let bytes = env
        .pic
        .query_call(
            env.provision,
            env.admin,
            "deployment_jobs",
            Encode!(&account_id).expect("encode jobs"),
        )
        .expect("deployment_jobs");
    let jobs: Result<Vec<DeploymentJobSummary>, DeploymentQueryError> = Decode!(
        &bytes,
        Result<Vec<DeploymentJobSummary>, DeploymentQueryError>
    )
    .expect("decode jobs");
    eprintln!(
        "jobs before suspend: {:?}",
        jobs.map(|jobs| jobs.into_iter().map(|j| j.state_name).collect::<Vec<_>>())
    );

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.user,
            "request_deployment_termination",
            Encode!(&account_id, &"settled".to_owned()).expect("encode suspend"),
        )
        .expect("request_deployment_termination");
    let result: Result<DeploymentTermination, DeploymentTerminationError> =
        Decode!(&bytes, Result<DeploymentTermination, DeploymentTerminationError>)
            .expect("decode suspend");
    result.expect("owner suspends");

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "delete_deployment_canisters",
            Encode!(&account_id, &account_id.to_string(), &true).expect("encode delete"),
        )
        .expect("delete_deployment_canisters");
    let result: Result<DeploymentTermination, DeploymentTerminationError> =
        Decode!(&bytes, Result<DeploymentTermination, DeploymentTerminationError>)
            .expect("decode delete");
    let deleted = result.expect("delete after settlement");
    assert_eq!(
        deleted.state,
        gleaph_provision::types::DeploymentTerminationState::Deleted
    );
}
