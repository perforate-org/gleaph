//! PocketIC E2E for ADR 0070: GQL `CREATE GRAPH` provisions graph shards and sets the home graph.
//!
//! Two scenarios, each on a fresh PocketIC with a provisioned Router (real Provision canister,
//! active release containing the real graph-shard artifact):
//!   1. ad-hoc GQL path — `CREATE GRAPH TYPE ... NEXT CREATE GRAPH demo TYPED ...` through
//!      `gql_mutate` provisions a graph shard, registers it as the single global home graph
//!      (`is_home: true`), binds the typed schema, and leaves the home graph query-resolvable.
//!      A second `CREATE GRAPH` must not steal the home slot; re-creating a bound name conflicts
//!      without re-provisioning.
//!   2. migration path — the same DDL through `apply_schema_migration` records `Applied` in the
//!      ledger and the provisioned graph is the resolvable home graph.

use candid::{Decode, Encode, Principal};
use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus};
use gleaph_graph_kernel::federation::{RouterError, ShardId, ShardRegistryEntry};
use gleaph_graph_kernel::plan_exec::{GqlQueryResult, ReadMode};
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationArgsV1, ApplySchemaMigrationResult,
    ApplySchemaMigrationResultV1, SchemaMigrationApplyStatus, SchemaMigrationGraphSelector,
};
use gleaph_pocket_ic_tests::{install_provision_canister, new_pocket_ic, wasm_bytes};
use gleaph_provision::types::{
    ArtifactId, ArtifactPublishMetadataArgs, ArtifactUploadChunkArgs, CanisterKind,
    DeploymentBinding, ReleaseActivateArgs, ReleaseId, ReleasePublishArgs, sha256,
};
use gleaph_router::RouterInitArgs;

const CHUNK_SIZE: usize = 1024 * 1024;

struct Env {
    pic: pocket_ic::PocketIc,
    admin: Principal,
    router: Principal,
    provision: Principal,
}

fn env() -> Env {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0x64; 29]);
    let router = pic.create_canister();
    pic.add_cycles(router, 2_000_000_000_000);

    // The CREATE GRAPH bridge derives deployment_id from the caller principal (ADR 0068), so the
    // binding must authorize that deployment for this Router.
    let binding = DeploymentBinding {
        deployment_id: admin.to_text(),
        router_principal: router,
        governance_principal: admin,
        binding_version: 1,
        bootstrap_principal: None,
    };
    let provision = install_provision_canister(&pic, binding);
    pic.add_cycles(provision, 100_000_000_000_000);

    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: Some(provision),
        })
        .expect("encode router init"),
        None,
    );

    Env {
        pic,
        admin,
        router,
        provision,
    }
}

/// A tiny valid WebAssembly module with `canister_init` and `memory` exports, usable as a dummy
/// artifact for the release kinds these tests never install.
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

fn publish_artifact(env: &Env, kind: CanisterKind, wasm: &[u8]) -> ArtifactId {
    let full_sha = sha256(wasm);
    let mut chunk_hashes = Vec::new();
    let mut offset = 0;
    while offset < wasm.len() {
        let end = (offset + CHUNK_SIZE).min(wasm.len());
        chunk_hashes.push(sha256(&wasm[offset..end]));
        offset = end;
    }
    let id = ArtifactId::new(kind.clone(), "0.1.0".to_owned(), full_sha);
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "artifact_publish_metadata",
            Encode!(&ArtifactPublishMetadataArgs {
                canister_kind: kind,
                semantic_version: "0.1.0".to_owned(),
                sha256: full_sha,
                byte_length: wasm.len() as u64,
                chunk_hashes,
            })
            .expect("encode artifact_publish_metadata"),
        )
        .unwrap_or_else(|e| panic!("artifact_publish_metadata: {e:?}"));
    let _: Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError> =
        Decode!(&bytes, Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError>)
            .expect("decode artifact_publish_metadata");

    let mut offset = 0;
    let mut chunk_index = 0u32;
    while offset < wasm.len() {
        let end = (offset + CHUNK_SIZE).min(wasm.len());
        let bytes = env
            .pic
            .update_call(
                env.provision,
                env.admin,
                "artifact_upload_chunk",
                Encode!(&ArtifactUploadChunkArgs {
                    artifact_id: id.clone(),
                    chunk_index,
                    bytes: wasm[offset..end].to_vec(),
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
    id
}

/// Publish the release manifest (real graph-shard and property-index artifacts — a `CREATE
/// INDEX` after bootstrap provisions and attaches the index canister; minimal dummies for the
/// kinds these tests never install) and activate it so provisioning can install from it.
fn activate_release(env: &Env) {
    let graph_id = publish_artifact(env, CanisterKind::Graph, &wasm_bytes("GRAPH_WASM"));
    let prop_id = publish_artifact(env, CanisterKind::PropertyIndex, &wasm_bytes("INDEX_WASM"));
    let minimal = minimal_canister_wasm();
    let router_id = publish_artifact(env, CanisterKind::Router, &minimal);
    let vec_id = publish_artifact(env, CanisterKind::VectorCanister, &minimal);

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "release_publish",
            Encode!(&ReleasePublishArgs {
                release_id: ReleaseId("release-create-graph".to_owned()),
                artifact_ids: vec![graph_id, router_id, prop_id, vec_id],
            })
            .expect("encode release_publish"),
        )
        .unwrap_or_else(|e| panic!("release_publish: {e:?}"));
    let _: Result<gleaph_provision::types::ReleaseManifest, gleaph_provision::types::ReleaseError> =
        Decode!(&bytes, Result<gleaph_provision::types::ReleaseManifest, gleaph_provision::types::ReleaseError>)
            .expect("decode release_publish");

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "release_activate",
            Encode!(&ReleaseActivateArgs {
                release_id: ReleaseId("release-create-graph".to_owned()),
            })
            .expect("encode release_activate"),
        )
        .unwrap_or_else(|e| panic!("release_activate: {e:?}"));
    let result: Result<
        gleaph_provision::types::ReleaseActivateResult,
        gleaph_provision::types::ReleaseError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_provision::types::ReleaseActivateResult,
            gleaph_provision::types::ReleaseError,
        >
    )
    .expect("decode release_activate");
    assert!(result.is_ok(), "release_activate must succeed: {result:?}");
}

fn gql_mutate(env: &Env, query: &str) -> Result<GqlQueryResult, RouterError> {
    gql_mutate_as(env, env.admin, query)
}

fn gql_mutate_as(env: &Env, caller: Principal, query: &str) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "gql_mutate",
            Encode!(&query.to_owned(), &Vec::<u8>::new(), &"adr0070".to_owned())
                .expect("encode gql_mutate"),
        )
        .unwrap_or_else(|e| panic!("gql_mutate on router: {e:?}"));
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_mutate")
}

fn gql_query(env: &Env, query: &str) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "gql_query",
            Encode!(&query.to_owned(), &Vec::<u8>::new(), &ReadMode::Eventual)
                .expect("encode gql_query"),
        )
        .unwrap_or_else(|e| panic!("gql_query on router: {e:?}"));
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query")
}

fn get_graph(env: &Env, name: &str) -> GraphRegistryEntry {
    get_graph_as(env, env.admin, name)
}

fn get_graph_as(env: &Env, caller: Principal, name: &str) -> GraphRegistryEntry {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "get_graph",
            Encode!(&name.to_owned()).expect("encode get_graph"),
        )
        .unwrap_or_else(|e| panic!("get_graph({name}) on router: {e:?}"));
    Decode!(&bytes, Result<GraphRegistryEntry, RouterError>)
        .expect("decode get_graph")
        .expect("graph `{name}` must be registered")
}

fn list_shards(env: &Env, graph_name: &str) -> Vec<ShardRegistryEntry> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_shards",
            Encode!(&graph_name.to_owned()).expect("encode list_shards"),
        )
        .unwrap_or_else(|error| panic!("list_shards({graph_name}) on router: {error:?}"));
    Decode!(&bytes, Result<Vec<ShardRegistryEntry>, RouterError>)
        .expect("decode list_shards")
        .expect("list_shards must succeed")
}

#[test]
fn create_graph_provisions_shard_and_sets_home_graph() {
    let env = env();
    activate_release(&env);

    let ddl =
        "CREATE GRAPH TYPE kg_t { NODE Person { name STRING } } NEXT CREATE GRAPH demo TYPED kg_t";
    let result = gql_mutate(&env, ddl);
    assert!(result.is_ok(), "CREATE GRAPH DDL must succeed: {result:?}");

    // The provisioned bootstrap shard is indexless by design (ADR 0054), so label-anchored
    // MATCH queries are served only after a property index is provisioned and attached. The
    // demo follows the same order: graph first, then indexes, then queries.
    //
    // ADR 0070 home-graph resolution itself needs no query: the created graph is Active, is
    // the single global home, and its schema binding answers catalog reads.
    let entry = get_graph(&env, "demo");
    assert!(matches!(entry.status, GraphStatus::Active));
    assert!(
        entry.is_home,
        "first CREATE GRAPH must set is_home on the created graph"
    );
    let first_shards = list_shards(&env, "demo");
    assert_eq!(first_shards.len(), 1);
    let first_shard = &first_shards[0];
    assert_eq!(first_shard.graph_id, entry.graph_id);
    assert_eq!(first_shard.shard_id, ShardId::new(0));
    assert_eq!(first_shard.graph_canister, entry.canister_id);
    assert_ne!(first_shard.graph_canister, Principal::anonymous());

    // The completed Router -> Provision ACK releases the caller/deployment's GraphShard(0) lock,
    // so the same caller can provision a second graph without stealing the single home slot.
    let ddl2 = "CREATE GRAPH TYPE t2 { NODE City } NEXT CREATE GRAPH other TYPED t2";
    let result = gql_mutate(&env, ddl2);
    assert!(
        result.is_ok(),
        "second CREATE GRAPH must succeed: {result:?}"
    );
    let second = get_graph(&env, "other");
    assert!(
        !second.is_home,
        "a later CREATE GRAPH must not reassign the home slot"
    );
    let first_after_second = get_graph(&env, "demo");
    assert!(first_after_second.is_home);
    assert_eq!(first_after_second.graph_id, entry.graph_id);
    let second_shards = list_shards(&env, "other");
    assert_eq!(second_shards.len(), 1);
    let second_shard = &second_shards[0];
    assert_eq!(second_shard.graph_id, second.graph_id);
    assert_eq!(second_shard.shard_id, ShardId::new(0));
    assert_eq!(second_shard.graph_canister, second.canister_id);
    assert_ne!(entry.graph_id, second.graph_id);
    assert_ne!(first_shard.graph_canister, second_shard.graph_canister);

    // Re-creating a bound name takes the binding-only catalog path and fails closed without
    // re-provisioning (no OR REPLACE / IF NOT EXISTS).
    let conflict = gql_mutate(&env, "CREATE GRAPH demo TYPED kg_t");
    let err = conflict.expect_err("re-creating a bound graph name must conflict");
    assert!(
        err.to_string().contains("already exists"),
        "expected already-exists conflict, got {err:?}"
    );
    assert!(get_graph(&env, "demo").is_home);

    // CREATE INDEX provisions a PropertyIndex canister for the unassigned group through the same
    // admission flow and retrofit-attaches it to the indexless shard (ADR 0035 Slice 10).
    let result = gql_mutate(&env, "CREATE INDEX person_name FOR (n:Person) ON (n.name)");
    assert!(
        result.is_ok(),
        "CREATE INDEX on provisioned graph: {result:?}"
    );

    // The home graph resolves without any session context; label anchors route through the
    // attached index canister to the provisioned shard.
    let scan = gql_query(&env, "MATCH (n:Person) RETURN n").expect("home graph scan");
    assert_eq!(scan.row_count, 0, "empty typed graph scans to zero rows");
}

#[test]
fn migration_create_graph_provisions_and_records_applied() {
    let env = env();
    activate_release(&env);

    let statement = "CREATE GRAPH TYPE m_t { NODE Person } NEXT CREATE GRAPH m_demo TYPED m_t";
    let selector = SchemaMigrationGraphSelector::Default;
    let args = ApplySchemaMigrationArgs::V1(ApplySchemaMigrationArgsV1 {
        id: "000001_knowledge".to_owned(),
        parent: None,
        graph_selector: selector.clone(),
        checksum: gleaph_migration_api::schema_migration_checksum(
            "000001_knowledge",
            None,
            &selector,
            statement.as_bytes(),
        ),
        statement: statement.to_owned(),
    });

    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "apply_schema_migration",
            Encode!(&args).expect("encode apply_schema_migration"),
        )
        .unwrap_or_else(|e| panic!("apply_schema_migration on router: {e:?}"));
    let applied: Result<ApplySchemaMigrationResult, RouterError> =
        Decode!(&bytes, Result<ApplySchemaMigrationResult, RouterError>)
            .expect("decode apply_schema_migration");
    let ApplySchemaMigrationResult::V1(ApplySchemaMigrationResultV1 { status, .. }) =
        applied.expect("migration must apply");
    assert_eq!(status, SchemaMigrationApplyStatus::Applied);

    // The provisioned migration graph is the resolvable home graph.
    let entry = get_graph(&env, "m_demo");
    assert!(entry.is_home, "migration-created graph must claim home");
}
