//! Shared helpers for PocketIC federation tests.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{GlobalVertexId, ShardId};
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
pub const GRAPH_HOME_NAME: &str = "tenant_a";
pub const GRAPH_REMOTE_NAME: &str = "tenant_b";
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

pub struct TwoGraphTwoIndexEnv {
    pub pic: PocketIc,
    pub admin: Principal,
    pub router: Principal,
    pub index_home: Principal,
    pub index_remote: Principal,
    pub graph_home: Principal,
    pub graph_remote: Principal,
}

#[derive(CandidType, serde::Deserialize)]
pub struct GraphInitArgs {
    pub logical_graph_name: Option<String>,
    pub router_canister: Option<Principal>,
    pub shard_id: Option<ShardId>,
    pub index_canister: Option<Principal>,
}

#[derive(CandidType)]
pub struct IndexInitArgs {
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
pub struct E2eInsertVertexWithTwoPropertiesArgs {
    pub property_a: u32,
    pub value_a: i64,
    pub property_b: u32,
    pub value_b: i64,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eInsertDirectedEdgeWithPropertyArgs {
    pub source_local_vertex_id: u32,
    pub target_local_vertex_id: u32,
    pub edge_label_id: u16,
    pub property_id: u32,
    pub value: i64,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eEnqueueForwardCompactionArgs {
    pub local_vertex_id: u32,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eDeleteDirectedEdgeArgs {
    pub source_local_vertex_id: u32,
    pub target_local_vertex_id: u32,
    pub property_id: u32,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eReverseResolvedEdgePropertyArgs {
    pub source_local_vertex_id: u32,
    pub target_local_vertex_id: u32,
    pub property_id: u32,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eInsertUndirectedEdgeWithPropertyArgs {
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
        })
        .expect("encode router init"),
        None,
    );

    let index = create_funded_canister(&pic);
    pic.install_canister(
        index,
        wasm_bytes("INDEX_WASM"),
        Encode!(&IndexInitArgs {
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
        })
        .expect("encode router init"),
        None,
    );

    let index_source = create_funded_canister(&pic);
    let index_dest = create_funded_canister(&pic);
    for index in [index_source, index_dest] {
        pic.install_canister(
            index,
            wasm_bytes("INDEX_WASM"),
            Encode!(&IndexInitArgs {
                router_canister: router,
            })
            .expect("encode index init"),
            None,
        );
    }

    let graph_source = create_funded_canister(&pic);
    let graph_dest = create_funded_canister(&pic);

    register_graph_and_shards(
        &pic,
        admin,
        router,
        index_source,
        index_dest,
        graph_source,
        graph_dest,
    );

    // Install each shard's graph canister with complete, validated federation routing.
    // Two shards of the same logical graph keep their distinct shard ordinals (0 and 1).
    for (graph, shard, index) in [
        (graph_source, SOURCE_SHARD, index_source),
        (graph_dest, DEST_SHARD, index_dest),
    ] {
        pic.install_canister(
            graph,
            wasm_bytes("GRAPH_WASM"),
            Encode!(&GraphInitArgs {
                logical_graph_name: Some(GRAPH_NAME.into()),
                router_canister: Some(router),
                shard_id: Some(shard),
                index_canister: Some(index),
            })
            .expect("encode graph init"),
            None,
        );
    }

    FederationEnv {
        pic,
        admin,
        router,
        index: index_source,
        graph_source,
        graph_dest,
    }
}

/// Two logical graphs (home + remote), one shard each — ADR 0011 remote `USE` / HOME e2e.
pub fn install_two_graph_federation() -> FederationEnv {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = create_funded_canister(&pic);
    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
        })
        .expect("encode router init"),
        None,
    );

    let index_home = create_funded_canister(&pic);
    let index_remote = create_funded_canister(&pic);
    for index in [index_home, index_remote] {
        pic.install_canister(
            index,
            wasm_bytes("INDEX_WASM"),
            Encode!(&IndexInitArgs {
                router_canister: router,
            })
            .expect("encode index init"),
            None,
        );
    }

    let graph_source = create_funded_canister(&pic);
    let graph_dest = create_funded_canister(&pic);

    register_two_graphs_and_shards(
        &pic,
        admin,
        router,
        index_home,
        index_remote,
        graph_source,
        graph_dest,
    );

    // Install each logical graph's single shard with complete, validated federation routing.
    // Shard ordinals are graph-local: each one-shard logical graph owns shard 0.
    for (graph, graph_name, index) in [
        (graph_source, GRAPH_HOME_NAME, index_home),
        (graph_dest, GRAPH_REMOTE_NAME, index_remote),
    ] {
        pic.install_canister(
            graph,
            wasm_bytes("GRAPH_WASM"),
            Encode!(&GraphInitArgs {
                logical_graph_name: Some(graph_name.into()),
                router_canister: Some(router),
                shard_id: Some(SOURCE_SHARD),
                index_canister: Some(index),
            })
            .expect("encode graph init"),
            None,
        );
    }

    FederationEnv {
        pic,
        admin,
        router,
        index: index_home,
        graph_source,
        graph_dest,
    }
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
        })
        .expect("encode router init"),
        None,
    );

    let index = create_funded_canister(&pic);
    pic.install_canister(
        index,
        wasm_bytes("INDEX_WASM"),
        Encode!(&IndexInitArgs {
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
            router_canister: Some(router),
            shard_id: Some(SOURCE_SHARD),
            index_canister: Some(index),
        })
        .expect("encode graph init"),
        None,
    );

    FederationEnv {
        pic,
        admin,
        router,
        index,
        graph_source,
        graph_dest: Principal::anonymous(),
    }
}

#[allow(clippy::too_many_arguments)]
fn attach_index_shard_canister(
    pic: &PocketIc,
    graph_id: GraphId,
    index_group_size: u32,
    group_index: u32,
    router: Principal,
    index: Principal,
    shard_id: ShardId,
    graph: Principal,
) {
    let bytes = pic
        .update_call(
            index,
            router,
            "admin_attach_shard_canister",
            Encode!(
                &graph_id,
                &index_group_size,
                &group_index,
                &shard_id,
                &graph
            )
            .expect("encode admin_attach_shard_canister"),
        )
        .expect("admin_attach_shard_canister");
    match Decode!(&bytes, Result<(), String>) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("admin_attach_shard_canister rejected: {err}"),
        Err(err) => panic!("decode admin_attach_shard_canister: {err}"),
    }
}

fn lookup_graph_id(
    pic: &PocketIc,
    admin: Principal,
    router: Principal,
    graph_name: &str,
) -> GraphId {
    let bytes = pic
        .query_call(
            router,
            admin,
            "lookup_graph_id",
            Encode!(&graph_name.to_string()).expect("encode lookup_graph_id"),
        )
        .expect("lookup_graph_id");
    match Decode!(
        &bytes,
        Result<gleaph_graph_kernel::entry::GraphId, gleaph_graph_kernel::federation::RouterError>
    ) {
        Ok(Ok(graph_id)) => graph_id,
        Ok(Err(err)) => panic!("lookup_graph_id rejected: {err:?}"),
        Err(err) => panic!("decode lookup_graph_id: {err}"),
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
    let graph_id = lookup_graph_id(pic, admin, router, GRAPH_NAME);
    attach_index_shard_canister(pic, graph_id, 1, 0, router, index, shard_id, graph);
}

pub fn register_graph_and_shards(
    pic: &PocketIc,
    admin: Principal,
    router: Principal,
    source_index: Principal,
    dest_index: Principal,
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

    for (shard, graph, index) in [
        (SOURCE_SHARD, graph_source, source_index),
        (DEST_SHARD, graph_dest, dest_index),
    ] {
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
        let graph_id = lookup_graph_id(pic, admin, router, GRAPH_NAME);
        let group_index = shard.raw();
        attach_index_shard_canister(pic, graph_id, 1, group_index, router, index, shard, graph);
    }
}

pub fn register_two_graphs_and_shards(
    pic: &PocketIc,
    admin: Principal,
    router: Principal,
    home_index: Principal,
    remote_index: Principal,
    graph_home: Principal,
    graph_remote: Principal,
) {
    for (name, graph, is_home) in [
        (GRAPH_HOME_NAME, graph_home, true),
        (GRAPH_REMOTE_NAME, graph_remote, false),
    ] {
        let entry = GraphRegistryEntry {
            graph_id: GraphId::from_raw(0),
            graph_name: name.into(),
            canister_id: graph,
            owner: admin,
            admins: Default::default(),
            status: GraphStatus::Active,
            version: 1,
            updated_at_ns: 0,
            provisioning_state: ProvisioningState::None,
            is_home,
        };
        pic.update_call(
            router,
            admin,
            "admin_register_graph",
            Encode!(&entry).expect("encode graph registry"),
        )
        .expect("admin_register_graph");
    }

    for (shard, graph, graph_name, index) in [
        (SOURCE_SHARD, graph_home, GRAPH_HOME_NAME, home_index),
        // Shard ordinals are graph-local: each one-shard logical graph owns shard 0.
        (SOURCE_SHARD, graph_remote, GRAPH_REMOTE_NAME, remote_index),
    ] {
        let args = AdminRegisterShardArgs {
            shard_id: shard,
            graph_canister: graph,
            index_canister: index,
            logical_graph_name: graph_name.into(),
        };
        pic.update_call(
            router,
            admin,
            "admin_register_shard",
            Encode!(&args).expect("encode register shard"),
        )
        .expect("admin_register_shard");
        let graph_id = lookup_graph_id(pic, admin, router, graph_name);
        attach_index_shard_canister(pic, graph_id, 1, 0, router, index, shard, graph);
    }
}

/// Two logical graphs, both graph-local `ShardId(0)`, with distinct index canisters.
pub fn install_two_graph_two_index_federation() -> TwoGraphTwoIndexEnv {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = create_funded_canister(&pic);
    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
        })
        .expect("encode router init"),
        None,
    );

    let index_home = create_funded_canister(&pic);
    let index_remote = create_funded_canister(&pic);
    for index in [index_home, index_remote] {
        pic.install_canister(
            index,
            wasm_bytes("INDEX_WASM"),
            Encode!(&IndexInitArgs {
                router_canister: router,
            })
            .expect("encode index init"),
            None,
        );
    }

    let graph_home = create_funded_canister(&pic);
    let graph_remote = create_funded_canister(&pic);
    for (graph, graph_name) in [
        (graph_home, GRAPH_HOME_NAME),
        (graph_remote, GRAPH_REMOTE_NAME),
    ] {
        let entry = GraphRegistryEntry {
            graph_id: GraphId::from_raw(0),
            graph_name: graph_name.into(),
            canister_id: graph,
            owner: admin,
            admins: Default::default(),
            status: GraphStatus::Active,
            version: 1,
            updated_at_ns: 0,
            provisioning_state: ProvisioningState::None,
            is_home: graph_name == GRAPH_HOME_NAME,
        };
        pic.update_call(
            router,
            admin,
            "admin_register_graph",
            Encode!(&entry).expect("encode graph registry"),
        )
        .expect("admin_register_graph");
    }

    for (graph_name, graph, index) in [
        (GRAPH_HOME_NAME, graph_home, index_home),
        (GRAPH_REMOTE_NAME, graph_remote, index_remote),
    ] {
        let args = AdminRegisterShardArgs {
            shard_id: SOURCE_SHARD,
            graph_canister: graph,
            index_canister: index,
            logical_graph_name: graph_name.into(),
        };
        pic.update_call(
            router,
            admin,
            "admin_register_shard",
            Encode!(&args).expect("encode register shard"),
        )
        .expect("admin_register_shard");

        let graph_id = lookup_graph_id(&pic, admin, router, graph_name);
        attach_index_shard_canister(&pic, graph_id, 1, 0, router, index, SOURCE_SHARD, graph);
    }

    // Install each logical graph's single shard with complete, validated federation routing.
    // Both one-shard logical graphs own graph-local shard 0 with distinct index canisters.
    for (graph, graph_name, index) in [
        (graph_home, GRAPH_HOME_NAME, index_home),
        (graph_remote, GRAPH_REMOTE_NAME, index_remote),
    ] {
        pic.install_canister(
            graph,
            wasm_bytes("GRAPH_WASM"),
            Encode!(&GraphInitArgs {
                logical_graph_name: Some(graph_name.into()),
                router_canister: Some(router),
                shard_id: Some(SOURCE_SHARD),
                index_canister: Some(index),
            })
            .expect("encode graph init"),
            None,
        );
    }

    TwoGraphTwoIndexEnv {
        pic,
        admin,
        router,
        index_home,
        index_remote,
        graph_home,
        graph_remote,
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

pub fn e2e_insert_vertex_with_two_properties(
    env: &FederationEnv,
    graph: Principal,
    property_a: u32,
    value_a: i64,
    property_b: u32,
    value_b: i64,
) -> E2eInsertVertexResult {
    update_as_router(
        env,
        graph,
        "e2e_insert_vertex_with_two_properties",
        E2eInsertVertexWithTwoPropertiesArgs {
            property_a,
            value_a,
            property_b,
            value_b,
        },
    )
}

pub fn admin_intern_property(
    env: &FederationEnv,
    name: &str,
) -> gleaph_graph_kernel::entry::PropertyId {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_intern_property",
            Encode!(&GRAPH_NAME.to_string(), &name.to_string())
                .expect("encode admin_intern_property"),
        )
        .unwrap_or_else(|e| panic!("admin_intern_property on {}: {e:?}", env.router));
    match Decode!(
        &bytes,
        Result<
            gleaph_graph_kernel::entry::PropertyId,
            gleaph_graph_kernel::federation::RouterError
        >
    ) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => panic!("admin_intern_property rejected: {err:?}"),
        Err(err) => panic!("decode admin_intern_property: {err}"),
    }
}

pub fn admin_intern_vertex_label(
    env: &FederationEnv,
    name: &str,
) -> gleaph_graph_kernel::entry::VertexLabelId {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_intern_vertex_label",
            Encode!(&GRAPH_NAME.to_string(), &name.to_string())
                .expect("encode admin_intern_vertex_label"),
        )
        .unwrap_or_else(|e| panic!("admin_intern_vertex_label on {}: {e:?}", env.router));
    match Decode!(
        &bytes,
        Result<
            gleaph_graph_kernel::entry::VertexLabelId,
            gleaph_graph_kernel::federation::RouterError
        >
    ) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => panic!("admin_intern_vertex_label rejected: {err:?}"),
        Err(err) => panic!("decode admin_intern_vertex_label: {err}"),
    }
}

pub fn admin_intern_edge_label(
    env: &FederationEnv,
    name: &str,
) -> gleaph_graph_kernel::entry::EdgeLabelId {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_intern_edge_label",
            Encode!(&GRAPH_NAME.to_string(), &name.to_string())
                .expect("encode admin_intern_edge_label"),
        )
        .unwrap_or_else(|e| panic!("admin_intern_edge_label on {}: {e:?}", env.router));
    match Decode!(
        &bytes,
        Result<
            gleaph_graph_kernel::entry::EdgeLabelId,
            gleaph_graph_kernel::federation::RouterError
        >
    ) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => panic!("admin_intern_edge_label rejected: {err:?}"),
        Err(err) => panic!("decode admin_intern_edge_label: {err}"),
    }
}

/// Gleaph extension DDL on the router update path (`gql_execute_idempotent`).
pub fn gql_execute_idempotent_as_admin(
    env: &FederationEnv,
    query: &str,
    client_mutation_key: &str,
) -> u64 {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::GqlQueryResult;

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
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Ok(result)) => result.row_count,
        Ok(Err(err)) => panic!("gql_execute_idempotent rejected: {err:?}"),
        Err(err) => panic!("decode gql_execute_idempotent: {err}"),
    }
}

/// Call the router's read-only registry-invariant oracle as admin. Returns the raw
/// `Result` so callers can assert both the consistent and divergent outcomes.
pub fn router_check_registry_invariants(
    env: &FederationEnv,
) -> Result<(), gleaph_graph_kernel::federation::RouterError> {
    use gleaph_graph_kernel::federation::RouterError;

    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "admin_check_registry_invariants",
            Encode!().expect("encode admin_check_registry_invariants"),
        )
        .unwrap_or_else(|e| panic!("admin_check_registry_invariants on router: {e:?}"));
    Decode!(&bytes, Result<(), RouterError>)
        .unwrap_or_else(|err| panic!("decode admin_check_registry_invariants: {err}"))
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

/// Register an undirected-only edge property index (`FOR () ~[e:L]~ ()`, ADR 0012).
pub fn create_undirected_edge_property_index(
    env: &FederationEnv,
    index_name: &str,
    edge_label: &str,
    property: &str,
    client_mutation_key: &str,
) {
    admin_intern_edge_label(env, edge_label);
    let _ = admin_intern_property(env, property);
    let ddl = format!(
        "CREATE INDEX {index_name} IF NOT EXISTS FOR () ~[e:{edge_label}]~ () ON (e.{property})"
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

pub fn e2e_insert_undirected_edge_with_property(
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
        "e2e_insert_undirected_edge_with_property",
        E2eInsertUndirectedEdgeWithPropertyArgs {
            source_local_vertex_id: source_local,
            target_local_vertex_id: target_local,
            edge_label_id,
            property_id,
            value,
        },
    );
}

/// Enqueue forward-span compaction on `graph` and arm its maintenance timer
/// without an inline drain (PocketIC E2E hook; see the graph canister handler).
///
/// Leaves a `CompactVertexEdgeSpan` work item in the shard's stable deferred
/// queue so [`drain_maintenance_via_timer`] can drive the wasm async timer tick.
pub fn e2e_enqueue_forward_compaction(env: &FederationEnv, graph: Principal, local_vertex_id: u32) {
    let _: () = update_as_router(
        env,
        graph,
        "e2e_enqueue_forward_compaction",
        E2eEnqueueForwardCompactionArgs { local_vertex_id },
    );
}

/// Delete the directed edge `source -> target` on `graph`, flushing the index
/// posting removal, and leave a tombstone at its slot (PocketIC E2E hook).
pub fn e2e_delete_directed_edge_with_property(
    env: &FederationEnv,
    graph: Principal,
    source_local: u32,
    target_local: u32,
    property_id: u32,
) {
    let _: () = update_as_router(
        env,
        graph,
        "e2e_delete_directed_edge_with_property",
        E2eDeleteDirectedEdgeArgs {
            source_local_vertex_id: source_local,
            target_local_vertex_id: target_local,
            property_id,
        },
    );
}

/// Reads the directed edge `source -> target`'s `property_id` value through the
/// reverse in-edge → edge-alias → canonical-forward path on `graph` (E2E hook).
///
/// Resolves at the *moved* canonical slot, so it surfaces a stale edge-alias
/// canonical target or an un-moved property sidecar after compaction — the
/// alias-resolved counterpart to the forward index-served lookup.
pub fn e2e_reverse_resolved_edge_property(
    env: &FederationEnv,
    graph: Principal,
    source_local: u32,
    target_local: u32,
    property_id: u32,
) -> Option<i64> {
    let bytes = env
        .pic
        .query_call(
            graph,
            env.router,
            "e2e_reverse_resolved_edge_property",
            Encode!(&E2eReverseResolvedEdgePropertyArgs {
                source_local_vertex_id: source_local,
                target_local_vertex_id: target_local,
                property_id,
            })
            .expect("encode e2e_reverse_resolved_edge_property"),
        )
        .unwrap_or_else(|e| panic!("e2e_reverse_resolved_edge_property on {graph}: {e:?}"));
    Decode!(&bytes, Result<Option<i64>, String>)
        .expect("decode e2e_reverse_resolved_edge_property")
        .expect("e2e_reverse_resolved_edge_property handler error")
}

/// Pending deferred-maintenance work items in `graph`'s stable queue (E2E hook).
pub fn e2e_maintenance_queue_len(env: &FederationEnv, graph: Principal) -> u64 {
    let bytes = env
        .pic
        .query_call(
            graph,
            env.router,
            "e2e_maintenance_queue_len",
            Encode!(&()).expect("encode e2e_maintenance_queue_len"),
        )
        .unwrap_or_else(|e| panic!("e2e_maintenance_queue_len on {graph}: {e:?}"));
    Decode!(&bytes, u64).expect("decode e2e_maintenance_queue_len")
}

/// Advance PocketIC time past the maintenance-timer floor delay and tick until
/// the graph shard's deferred-maintenance queue drains (timer-fire harness).
///
/// `ic-cdk-timers` one-shot timers do not fire until simulated time passes their
/// delay and the IC executes a round, and the async tick spans several
/// inter-canister hops (router catalog fetch, posting flush). Each outer round
/// advances time well past the 1s floor delay, then ticks enough times to let
/// the timer fire and its cross-canister calls settle. Panics if the queue is
/// still non-empty after the bounded number of rounds.
pub fn drain_maintenance_via_timer(env: &FederationEnv, graph: Principal) {
    use std::time::Duration;

    const MAX_ROUNDS: usize = 40;
    const TICKS_PER_ROUND: usize = 12;

    for _ in 0..MAX_ROUNDS {
        if e2e_maintenance_queue_len(env, graph) == 0 {
            return;
        }
        env.pic.advance_time(Duration::from_secs(2));
        for _ in 0..TICKS_PER_ROUND {
            env.pic.tick();
        }
    }
    assert_eq!(
        e2e_maintenance_queue_len(env, graph),
        0,
        "maintenance timer failed to drain the deferred queue within {MAX_ROUNDS} rounds"
    );
}

/// Router composite `gql_query` (parse → plan → shard dispatch) as the bootstrap admin principal.
pub fn gql_query_as_admin(
    env: &FederationEnv,
    query: &str,
) -> gleaph_graph_kernel::plan_exec::GqlQueryResult {
    gql_query_on_router(&env.pic, env.admin, env.router, query)
}

/// Router composite `gql_query` with explicit caller and router principals.
pub fn gql_query_on_router(
    pic: &PocketIc,
    caller: Principal,
    router: Principal,
    query: &str,
) -> gleaph_graph_kernel::plan_exec::GqlQueryResult {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::GqlQueryResult;

    let bytes = pic
        .query_call(
            router,
            caller,
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

/// Admin query: per-graph `ElementIdEncodingKey` bytes from router runtime config (ADR 0019).
pub fn graph_element_id_encoding_key(
    pic: &PocketIc,
    caller: Principal,
    router: Principal,
    logical_graph_name: &str,
) -> gleaph_graph_kernel::federation::ElementIdEncodingKey {
    use gleaph_graph_kernel::federation::{ElementIdEncodingKey, RouterError};

    let bytes = pic
        .query_call(
            router,
            caller,
            "graph_element_id_encoding_key",
            Encode!(&logical_graph_name.to_string()).expect("encode graph_element_id_encoding_key"),
        )
        .unwrap_or_else(|e| panic!("graph_element_id_encoding_key on router: {e:?}"));
    match Decode!(&bytes, Result<[u8; 16], RouterError>) {
        Ok(Ok(key)) => ElementIdEncodingKey(key),
        Ok(Err(err)) => panic!("graph_element_id_encoding_key rejected: {err:?}"),
        Err(err) => panic!("decode graph_element_id_encoding_key: {err}"),
    }
}

/// Insert one vertex on `graph` with the router as update caller (federation e2e).
pub fn e2e_insert_vertex_via_router(
    pic: &PocketIc,
    router: Principal,
    graph: Principal,
) -> E2eInsertVertexResult {
    let bytes = pic
        .update_call(
            graph,
            router,
            "e2e_insert_vertex",
            Encode!(&()).expect("encode e2e_insert_vertex"),
        )
        .unwrap_or_else(|e| panic!("e2e_insert_vertex on {graph}: {e:?}"));
    match Decode!(&bytes, Result<E2eInsertVertexResult, String>) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => panic!("e2e_insert_vertex rejected: {err}"),
        Err(err) => panic!("decode e2e_insert_vertex: {err}"),
    }
}

/// Decode the first row's bytes column from a router `gql_query` `rows_blob` projection.
pub fn element_id_bytes_from_gql_result(
    result: &gleaph_graph_kernel::plan_exec::GqlQueryResult,
    column: &str,
) -> Vec<u8> {
    use gleaph_gql::Value;
    use gleaph_gql_ic::IcWirePlanQueryResult;

    let rows_blob = result
        .rows_blob
        .as_ref()
        .unwrap_or_else(|| panic!("gql_query should return rows_blob for ELEMENT_ID projection"));
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode rows_blob");
    assert_eq!(wire.rows.len(), 1, "expected one ELEMENT_ID row");
    let row = wire
        .rows
        .into_iter()
        .next()
        .expect("one row")
        .try_into_value_row()
        .expect("wire row to value row");
    let Value::Bytes(id_bytes) = row.get(column).unwrap_or_else(|| {
        panic!(
            "expected ELEMENT_ID bytes in column {column}, got {:?}",
            row.get(column)
        )
    }) else {
        panic!(
            "expected ELEMENT_ID bytes in column {column}, got {:?}",
            row.get(column)
        );
    };
    id_bytes.clone()
}

/// Per-graph encoding key bytes for a federation env's registered graph name.
pub fn federation_graph_element_id_encoding_key_bytes(env: &FederationEnv) -> [u8; 16] {
    graph_element_id_encoding_key(&env.pic, env.admin, env.router, GRAPH_NAME).0
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
    use gleaph_graph_kernel::plan_exec::GqlQueryResult;

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
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(result)) => {
            panic!(
                "gql_execute_idempotent should fail, got row_count {}",
                result.row_count
            )
        }
        Err(err) => panic!("decode gql_execute_idempotent: {err}"),
    }
}

const KNOWLEDGE_MAP_SEEDS_JSON: &str =
    include_str!("../../../frontend/apps/knowledge-map/seeds/knowledge-map-seeds.json");

const KNOWLEDGE_MAP_LIVE_QUERY: &str = "\
MATCH ()-[e]->() WHERE e.demo_edge_id IS NOT NULL \
RETURN e.demo_edge_id AS edge_id, e.demo_kind AS edge_kind \
ORDER BY edge_id";

/// Seed the knowledge-map demo graph through Router `gql_execute_idempotent`.
pub fn seed_knowledge_map_graph(env: &FederationEnv) {
    let parsed: serde_json::Value =
        serde_json::from_str(KNOWLEDGE_MAP_SEEDS_JSON).expect("parse knowledge-map seeds");
    for seed in parsed["seeds"]
        .as_array()
        .expect("knowledge-map seed array")
    {
        let gql = seed["gql"].as_str().expect("knowledge-map seed gql");
        let key = seed["key"].as_str().expect("knowledge-map seed key");
        let row_count = gql_execute_idempotent_as_admin(env, gql, key);
        assert_eq!(row_count, 0, "seed {key} should not return rows");
    }
}

pub fn knowledge_map_live_query() -> &'static str {
    KNOWLEDGE_MAP_LIVE_QUERY
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
