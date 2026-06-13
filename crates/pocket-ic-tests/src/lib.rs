//! Shared helpers for PocketIC federation tests.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{GlobalVertexId, ShardId, VertexPlacement};
use gleaph_router::RouterInitArgs;
use gleaph_router::types::AdminRegisterShardArgs;
use pocket_ic::{PocketIc, PocketIcBuilder};
use std::path::{Path, PathBuf};
use std::process::Command;

/// PocketIC instance using a server binary compatible with the `pocket-ic` crate version.
///
/// `POCKET_IC_BIN` may override the build-managed `.pocket-ic/pocket-ic` when it reports
/// `pocket-ic-server {version}` matching [`env!("POCKET_IC_VERSION")`]. A stale override
/// (for example an older install on `PATH`) is ignored with a warning.
pub fn new_pocket_ic() -> PocketIc {
    let server_binary = resolve_pocket_ic_server_binary();
    PocketIcBuilder::new()
        .with_server_binary(server_binary)
        .with_application_subnet()
        .build()
}

fn resolve_pocket_ic_server_binary() -> PathBuf {
    const BUILD_BIN: &str = env!("POCKET_IC_BIN");
    const EXPECTED_VERSION: &str = env!("POCKET_IC_VERSION");
    let build_bin = PathBuf::from(BUILD_BIN);

    if let Some(override_path) = std::env::var_os("POCKET_IC_BIN") {
        let path = PathBuf::from(override_path);
        if path == build_bin {
            return build_bin;
        }
        match validate_pocket_ic_server_binary(&path, EXPECTED_VERSION) {
            Ok(()) => return path,
            Err(reason) => eprintln!(
                "warning: ignoring POCKET_IC_BIN={} ({}); using {}",
                path.display(),
                reason,
                build_bin.display()
            ),
        }
    }

    build_bin
}

fn validate_pocket_ic_server_binary(path: &Path, version: &str) -> Result<(), String> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|e| format!("run --version: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pocket-ic --version failed: status {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.trim_end();
    let expected = format!("pocket-ic-server {version}");
    if line == expected {
        return Ok(());
    }
    if line.starts_with("pocket-ic-server ")
        && line
            .strip_prefix("pocket-ic-server ")
            .is_some_and(|v| v == version || v.starts_with(&format!("{version}.")))
    {
        return Ok(());
    }
    Err(format!("expected pocket-ic-server {version}, got {line:?}"))
}

pub const GRAPH_NAME: &str = "gleaph.pocket_ic";
pub const SOURCE_SHARD: ShardId = ShardId::new(0);
pub const DEST_SHARD: ShardId = ShardId::new(1);

pub struct FederationEnv {
    pub pic: PocketIc,
    pub admin: Principal,
    pub router: Principal,
    pub index: Principal,
    pub graph_source: Principal,
    pub graph_dest: Principal,
}

#[derive(CandidType, serde::Deserialize)]
pub struct GraphInitArgs {
    pub logical_graph_name: Option<String>,
    pub router_canister: Option<Principal>,
    pub shard_id: Option<ShardId>,
}

#[derive(CandidType, serde::Deserialize)]
pub struct E2eAttachFederationArgs {
    pub logical_graph_name: Option<String>,
    pub router_canister: Principal,
    pub index_canister: Principal,
    pub shard_id: ShardId,
}

#[derive(CandidType)]
pub struct IndexInitArgs {
    pub controllers: Vec<Principal>,
    pub router_canister: Principal,
}

#[derive(CandidType, Clone, Debug, serde::Deserialize)]
pub struct E2eInsertVertexResult {
    pub local_vertex_id: u32,
    pub global_vertex_id: GlobalVertexId,
}

#[derive(CandidType, Clone, Debug, serde::Deserialize)]
pub struct E2eInsertDirectedEdgeArgs {
    pub source_local_vertex_id: u32,
    pub target_local_vertex_id: u32,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eInsertVertexWithPropertyArgs {
    pub property_id: u32,
    pub value: i64,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eInsertDirectedEdgeWithPropertyArgs {
    pub source_local_vertex_id: u32,
    pub target_local_vertex_id: u32,
    pub edge_label_id: u16,
    pub property_id: u32,
    pub value: i64,
}

pub fn wasm_bytes(env_var: &str) -> Vec<u8> {
    let path = PathBuf::from(std::env::var(env_var).unwrap_or_else(|_| {
        panic!("build.rs must set {env_var} (run `cargo test -p gleaph-pocket-ic-tests` from workspace)")
    }));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn create_funded_canister(pic: &PocketIc) -> Principal {
    let id = pic.create_canister();
    pic.add_cycles(id, 2_000_000_000_000);
    id
}

/// Router + index only (no graph WASM). Used for placement smoke tests.
pub fn install_router_and_index() -> FederationEnv {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = create_funded_canister(&pic);
    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            controllers: vec![admin],
        })
        .expect("encode router init"),
        None,
    );

    let index = create_funded_canister(&pic);
    pic.install_canister(
        index,
        wasm_bytes("INDEX_WASM"),
        Encode!(&IndexInitArgs {
            controllers: vec![admin],
            router_canister: router,
        })
        .expect("encode index init"),
        None,
    );

    FederationEnv {
        pic,
        admin,
        router,
        index,
        graph_source: Principal::anonymous(),
        graph_dest: Principal::anonymous(),
    }
}

pub fn install_federation() -> FederationEnv {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = create_funded_canister(&pic);
    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            controllers: vec![admin],
        })
        .expect("encode router init"),
        None,
    );

    let index = create_funded_canister(&pic);
    pic.install_canister(
        index,
        wasm_bytes("INDEX_WASM"),
        Encode!(&IndexInitArgs {
            controllers: vec![admin],
            router_canister: router,
        })
        .expect("encode index init"),
        None,
    );

    let graph_source = create_funded_canister(&pic);
    let graph_dest = create_funded_canister(&pic);

    register_graph_and_shards(&pic, admin, router, index, graph_source, graph_dest);

    for (graph, _shard) in [(graph_source, SOURCE_SHARD), (graph_dest, DEST_SHARD)] {
        pic.install_canister(
            graph,
            wasm_bytes("GRAPH_WASM"),
            Encode!(&GraphInitArgs {
                logical_graph_name: Some(GRAPH_NAME.into()),
                router_canister: None,
                shard_id: None,
            })
            .expect("encode graph init"),
            None,
        );
    }

    let env = FederationEnv {
        pic,
        admin,
        router,
        index,
        graph_source,
        graph_dest,
    };

    for (graph, shard) in [(graph_source, SOURCE_SHARD), (graph_dest, DEST_SHARD)] {
        let _: () = update_as_router(
            &env,
            graph,
            "e2e_attach_federation",
            E2eAttachFederationArgs {
                logical_graph_name: Some(GRAPH_NAME.into()),
                router_canister: router,
                index_canister: index,
                shard_id: shard,
            },
        );
    }

    env
}

/// Router + index + one federated graph shard (standalone dispatch policy).
pub fn install_single_shard_federation() -> FederationEnv {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = create_funded_canister(&pic);
    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            controllers: vec![admin],
        })
        .expect("encode router init"),
        None,
    );

    let index = create_funded_canister(&pic);
    pic.install_canister(
        index,
        wasm_bytes("INDEX_WASM"),
        Encode!(&IndexInitArgs {
            controllers: vec![admin],
            router_canister: router,
        })
        .expect("encode index init"),
        None,
    );

    let graph_source = create_funded_canister(&pic);
    register_graph_single_shard(&pic, admin, router, index, graph_source, SOURCE_SHARD);

    pic.install_canister(
        graph_source,
        wasm_bytes("GRAPH_WASM"),
        Encode!(&GraphInitArgs {
            logical_graph_name: Some(GRAPH_NAME.into()),
            router_canister: None,
            shard_id: None,
        })
        .expect("encode graph init"),
        None,
    );

    let env = FederationEnv {
        pic,
        admin,
        router,
        index,
        graph_source,
        graph_dest: Principal::anonymous(),
    };

    let _: () = update_as_router(
        &env,
        graph_source,
        "e2e_attach_federation",
        E2eAttachFederationArgs {
            logical_graph_name: Some(GRAPH_NAME.into()),
            router_canister: router,
            index_canister: index,
            shard_id: SOURCE_SHARD,
        },
    );

    env
}

fn register_index_shard_owner(
    pic: &PocketIc,
    router: Principal,
    index: Principal,
    shard_id: ShardId,
    graph: Principal,
) {
    let bytes = pic
        .update_call(
            index,
            router,
            "admin_set_shard_owner",
            Encode!(&shard_id, &graph).expect("encode admin_set_shard_owner"),
        )
        .expect("admin_set_shard_owner");
    match Decode!(&bytes, Result<(), String>) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("admin_set_shard_owner rejected: {err}"),
        Err(err) => panic!("decode admin_set_shard_owner: {err}"),
    }
}

pub fn register_graph_single_shard(
    pic: &PocketIc,
    admin: Principal,
    router: Principal,
    index: Principal,
    graph: Principal,
    shard_id: ShardId,
) {
    let entry = GraphRegistryEntry {
        graph_id: GraphId::from_raw(0),
        graph_name: GRAPH_NAME.into(),
        canister_id: graph,
        owner: admin,
        admins: Default::default(),
        status: GraphStatus::Active,
        version: 1,
        updated_at_ns: 0,
        provisioning_state: ProvisioningState::None,
        is_home: false,
    };
    pic.update_call(
        router,
        admin,
        "admin_register_graph",
        Encode!(&entry).expect("encode graph registry"),
    )
    .expect("admin_register_graph");

    let args = AdminRegisterShardArgs {
        shard_id,
        graph_canister: graph,
        index_canister: index,
        logical_graph_name: GRAPH_NAME.into(),
    };
    pic.update_call(
        router,
        admin,
        "admin_register_shard",
        Encode!(&args).expect("encode register shard"),
    )
    .expect("admin_register_shard");
    register_index_shard_owner(pic, router, index, shard_id, graph);
}

pub fn register_graph_and_shards(
    pic: &PocketIc,
    admin: Principal,
    router: Principal,
    index: Principal,
    graph_source: Principal,
    graph_dest: Principal,
) {
    let entry = GraphRegistryEntry {
        graph_id: GraphId::from_raw(0),
        graph_name: GRAPH_NAME.into(),
        canister_id: graph_source,
        owner: admin,
        admins: Default::default(),
        status: GraphStatus::Active,
        version: 1,
        updated_at_ns: 0,
        provisioning_state: ProvisioningState::None,
        is_home: false,
    };
    pic.update_call(
        router,
        admin,
        "admin_register_graph",
        Encode!(&entry).expect("encode graph registry"),
    )
    .expect("admin_register_graph");

    for (shard, graph) in [(SOURCE_SHARD, graph_source), (DEST_SHARD, graph_dest)] {
        let args = AdminRegisterShardArgs {
            shard_id: shard,
            graph_canister: graph,
            index_canister: index,
            logical_graph_name: GRAPH_NAME.into(),
        };
        pic.update_call(
            router,
            admin,
            "admin_register_shard",
            Encode!(&args).expect("encode register shard"),
        )
        .expect("admin_register_shard");
        register_index_shard_owner(pic, router, index, shard, graph);
    }
}

pub fn update_as_router<T: CandidType, R: CandidType + serde::de::DeserializeOwned>(
    env: &FederationEnv,
    canister: Principal,
    method: &str,
    args: T,
) -> R {
    let bytes = env
        .pic
        .update_call(
            canister,
            env.router,
            method,
            Encode!(&args).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("{method} on {canister}: {e:?}"));
    match Decode!(&bytes, Result<R, String>) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => panic!("{method} on {canister} rejected: {err}"),
        Err(err) => panic!("decode {method}: {err}"),
    }
}

pub fn query_as_router<T: CandidType, R: CandidType + serde::de::DeserializeOwned>(
    env: &FederationEnv,
    canister: Principal,
    method: &str,
    args: T,
) -> R {
    let bytes = env
        .pic
        .query_call(
            canister,
            env.router,
            method,
            Encode!(&args).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("{method} on {canister}: {e:?}"));
    match Decode!(&bytes, Result<R, String>) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => panic!("{method} on {canister} rejected: {err}"),
        Err(err) => panic!("decode {method}: {err}"),
    }
}

pub fn execute_plan_query_as_router_reject(
    env: &FederationEnv,
    graph: Principal,
    args: gleaph_graph_kernel::plan_exec::ExecutePlanArgs,
) -> String {
    use gleaph_graph_kernel::plan_exec::ExecutePlanResult;

    let bytes = env
        .pic
        .query_call(
            graph,
            env.router,
            "execute_plan_query",
            Encode!(&args).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("execute_plan_query on {graph}: {e:?}"));
    match Decode!(&bytes, Result<ExecutePlanResult, String>) {
        Ok(Err(err)) => err,
        Ok(Ok(result)) => panic!("execute_plan_query should reject, got {result:?}"),
        Err(err) => panic!("decode execute_plan_query: {err}"),
    }
}

pub fn e2e_insert_vertex(env: &FederationEnv, graph: Principal) -> E2eInsertVertexResult {
    update_as_router(env, graph, "e2e_insert_vertex", ())
}

pub fn e2e_insert_vertex_with_property(
    env: &FederationEnv,
    graph: Principal,
    property_id: u32,
    value: i64,
) -> E2eInsertVertexResult {
    update_as_router(
        env,
        graph,
        "e2e_insert_vertex_with_property",
        E2eInsertVertexWithPropertyArgs { property_id, value },
    )
}

pub fn admin_intern_property(
    env: &FederationEnv,
    name: &str,
) -> gleaph_graph_kernel::entry::PropertyId {
    update_as_admin(env, env.router, "admin_intern_property", name.to_string())
}

pub fn admin_intern_vertex_label(
    env: &FederationEnv,
    name: &str,
) -> gleaph_graph_kernel::entry::VertexLabelId {
    update_as_admin(
        env,
        env.router,
        "admin_intern_vertex_label",
        name.to_string(),
    )
}

pub fn admin_intern_edge_label(
    env: &FederationEnv,
    name: &str,
) -> gleaph_graph_kernel::entry::EdgeLabelId {
    update_as_admin(env, env.router, "admin_intern_edge_label", name.to_string())
}

/// Gleaph extension DDL on the router update path (`gql_execute_idempotent`).
pub fn gql_execute_idempotent_as_admin(
    env: &FederationEnv,
    query: &str,
    client_mutation_key: &str,
) -> u64 {
    use gleaph_graph_kernel::federation::RouterError;

    let query = query.to_string();
    let params: Vec<u8> = Vec::new();
    let mutation_key = client_mutation_key.to_string();
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "gql_execute_idempotent",
            Encode!(&query, &params, &mutation_key).expect("encode gql_execute_idempotent"),
        )
        .unwrap_or_else(|e| panic!("gql_execute_idempotent on router: {e:?}"));
    match Decode!(&bytes, Result<u64, RouterError>) {
        Ok(Ok(row_count)) => row_count,
        Ok(Err(err)) => panic!("gql_execute_idempotent rejected: {err:?}"),
        Err(err) => panic!("decode gql_execute_idempotent: {err}"),
    }
}

/// Register an edge property index via `CREATE INDEX` (ADR 0009 phase E).
pub fn create_edge_property_index(
    env: &FederationEnv,
    index_name: &str,
    edge_label: &str,
    property: &str,
    client_mutation_key: &str,
) {
    admin_intern_edge_label(env, edge_label);
    let _ = admin_intern_property(env, property);
    let ddl = format!(
        "CREATE INDEX {index_name} IF NOT EXISTS FOR ()-[e:{edge_label}]-() ON (e.{property})"
    );
    let row_count = gql_execute_idempotent_as_admin(env, &ddl, client_mutation_key);
    assert_eq!(
        row_count, 0,
        "CREATE INDEX DDL should return row_count 0, got {row_count}"
    );
}

/// Register a directed-only edge property index (`FOR ()-[e:L]->()`, ADR 0012 F6).
pub fn create_directed_edge_property_index(
    env: &FederationEnv,
    index_name: &str,
    edge_label: &str,
    property: &str,
    client_mutation_key: &str,
) {
    admin_intern_edge_label(env, edge_label);
    let _ = admin_intern_property(env, property);
    let ddl = format!(
        "CREATE INDEX {index_name} IF NOT EXISTS FOR ()-[e:{edge_label}]->() ON (e.{property})"
    );
    let row_count = gql_execute_idempotent_as_admin(env, &ddl, client_mutation_key);
    assert_eq!(
        row_count, 0,
        "CREATE INDEX DDL should return row_count 0, got {row_count}"
    );
}

/// Register a vertex property index via `CREATE INDEX` (ADR 0009 phase E).
pub fn create_vertex_property_index(
    env: &FederationEnv,
    index_name: &str,
    vertex_label: &str,
    property: &str,
    client_mutation_key: &str,
) {
    admin_intern_vertex_label(env, vertex_label);
    let _ = admin_intern_property(env, property);
    let ddl =
        format!("CREATE INDEX {index_name} IF NOT EXISTS FOR (n:{vertex_label}) ON (n.{property})");
    let row_count = gql_execute_idempotent_as_admin(env, &ddl, client_mutation_key);
    assert_eq!(
        row_count, 0,
        "CREATE INDEX DDL should return row_count 0, got {row_count}"
    );
}

/// Drop a named index via `DROP INDEX` (ADR 0009 phase E).
pub fn drop_vertex_property_index(
    env: &FederationEnv,
    index_name: &str,
    if_exists: bool,
    client_mutation_key: &str,
) {
    let ddl = if if_exists {
        format!("DROP INDEX {index_name} IF EXISTS")
    } else {
        format!("DROP INDEX {index_name}")
    };
    let row_count = gql_execute_idempotent_as_admin(env, &ddl, client_mutation_key);
    assert_eq!(
        row_count, 0,
        "DROP INDEX DDL should return row_count 0, got {row_count}"
    );
}

fn update_as_admin<T: CandidType, R: CandidType + serde::de::DeserializeOwned>(
    env: &FederationEnv,
    canister: Principal,
    method: &str,
    args: T,
) -> R {
    use gleaph_graph_kernel::federation::RouterError;

    let bytes = env
        .pic
        .update_call(canister, env.admin, method, Encode!(&args).expect("encode"))
        .unwrap_or_else(|e| panic!("{method} on {canister}: {e:?}"));
    match Decode!(&bytes, Result<R, RouterError>) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => panic!("{method} rejected: {err:?}"),
        Err(err) => panic!("decode {method}: {err}"),
    }
}

pub fn e2e_insert_edge(
    env: &FederationEnv,
    graph: Principal,
    source_local: u32,
    target_local: u32,
) {
    let _: () = update_as_router(
        env,
        graph,
        "e2e_insert_directed_edge",
        E2eInsertDirectedEdgeArgs {
            source_local_vertex_id: source_local,
            target_local_vertex_id: target_local,
        },
    );
}

pub fn e2e_insert_directed_edge_with_property(
    env: &FederationEnv,
    graph: Principal,
    source_local: u32,
    target_local: u32,
    edge_label_id: u16,
    property_id: u32,
    value: i64,
) {
    let _: () = update_as_router(
        env,
        graph,
        "e2e_insert_directed_edge_with_property",
        E2eInsertDirectedEdgeWithPropertyArgs {
            source_local_vertex_id: source_local,
            target_local_vertex_id: target_local,
            edge_label_id,
            property_id,
            value,
        },
    );
}

pub fn resolve_placement(env: &FederationEnv, vertex_id: GlobalVertexId) -> VertexPlacement {
    query_as_router(env, env.router, "resolve_placement", vertex_id)
}

/// Router composite `gql_query` (parse → plan → shard dispatch) as the bootstrap admin principal.
pub fn gql_query_as_admin(
    env: &FederationEnv,
    query: &str,
) -> gleaph_graph_kernel::plan_exec::GqlQueryResult {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::GqlQueryResult;

    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "gql_query",
            Encode!(&query.to_string(), &Vec::<u8>::new()).expect("encode gql_query"),
        )
        .unwrap_or_else(|e| panic!("gql_query on router: {e:?}"));
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => panic!("gql_query rejected: {err:?}"),
        Err(err) => panic!("decode gql_query: {err}"),
    }
}

/// Router composite `gql_query` expected to fail (e.g. after DROP INDEX removes federated anchor).
pub fn gql_query_as_admin_expect_err(
    env: &FederationEnv,
    query: &str,
) -> gleaph_graph_kernel::federation::RouterError {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::GqlQueryResult;

    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "gql_query",
            Encode!(&query.to_string(), &Vec::<u8>::new()).expect("encode gql_query"),
        )
        .unwrap_or_else(|e| panic!("gql_query on router: {e:?}"));
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(result)) => panic!("gql_query should fail, got {result:?}"),
        Err(err) => panic!("decode gql_query: {err}"),
    }
}

/// Gleaph extension DDL on the update path, expected to fail.
pub fn gql_execute_idempotent_as_admin_expect_err(
    env: &FederationEnv,
    query: &str,
    client_mutation_key: &str,
) -> gleaph_graph_kernel::federation::RouterError {
    use gleaph_graph_kernel::federation::RouterError;

    let query = query.to_string();
    let params: Vec<u8> = Vec::new();
    let mutation_key = client_mutation_key.to_string();
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "gql_execute_idempotent",
            Encode!(&query, &params, &mutation_key).expect("encode gql_execute_idempotent"),
        )
        .unwrap_or_else(|e| panic!("gql_execute_idempotent on router: {e:?}"));
    match Decode!(&bytes, Result<u64, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(row_count)) => {
            panic!("gql_execute_idempotent should fail, got row_count {row_count}")
        }
        Err(err) => panic!("decode gql_execute_idempotent: {err}"),
    }
}

#[cfg(test)]
mod pocket_ic_server_binary_tests {
    use super::validate_pocket_ic_server_binary;
    use std::path::Path;

    #[test]
    fn build_managed_binary_matches_crate_version() {
        validate_pocket_ic_server_binary(
            Path::new(env!("POCKET_IC_BIN")),
            env!("POCKET_IC_VERSION"),
        )
        .expect("build-managed PocketIC server binary");
    }
}
