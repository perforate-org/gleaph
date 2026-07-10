//! Shared helpers for PocketIC federation tests.

use candid::{CandidType, Decode, Encode, Principal};

use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{GlobalVertexId, ShardId};
use gleaph_graph_kernel::vector_index::{VectorMetric, VertexEmbeddingProjectionOutcome};
use gleaph_provision::canister::init::ProvisionInitArgs;
use gleaph_provision::types::DeploymentBinding;
use gleaph_router::RouterInitArgs;
use gleaph_router::types::{
    AdminAttachVectorIndexShardArgs, AdminIngestVertexEmbeddingArgs, AdminRegisterShardArgs,
    RegisterVectorIndexArgs,
};
use gleaph_social_demo_gateway::{GatewayInitArgs, SocialDemoScenario};
use pocket_ic::{PocketIc, PocketIcBuilder};
use std::collections::BTreeSet;
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
pub struct E2eInsertVertexWithLabelArgs {
    pub label_id: u16,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eInsertDirectedEdgeWithLabelArgs {
    pub source_local_vertex_id: u32,
    pub target_local_vertex_id: u32,
    pub edge_label_id: u16,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eInsertDirectedEdgeWithPayloadArgs {
    pub source_local_vertex_id: u32,
    pub target_local_vertex_id: u32,
    pub edge_label_id: u16,
    pub payload: Vec<u8>,
    pub inline_value_profile: gleaph_graph_kernel::entry::EdgeInlineValueProfile,
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
pub struct E2eInsertVertexWithLabelAndPropertyArgs {
    pub label_id: u16,
    pub property_id: u32,
    pub value: i64,
}
#[derive(CandidType, Clone, Debug)]
pub struct E2eInsertVertexWithLabelAndTwoPropertiesArgs {
    pub label_id: u16,
    pub property_a: u32,
    pub value_a: i64,
    pub property_b: u32,
    pub value_b: i64,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eSetVertexPropertyArgs {
    pub local_vertex_id: u32,
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

#[derive(CandidType, Clone, Debug)]
pub struct E2eEnqueueForwardCompactionArgs {
    pub local_vertex_id: u32,
}

#[derive(CandidType, Clone, Debug)]
pub struct E2eSetEdgePropertyArgs {
    pub source_local_vertex_id: u32,
    pub target_local_vertex_id: u32,
    pub property_id: u32,
    pub value: i64,
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

#[derive(CandidType)]
struct VectorIndexInitArgs {
    router_canister: Principal,
}

/// Install a derived vector-index canister (`gleaph-graph-vector-index`) authorized for `router`.
/// Used by ADR 0031 Slice 4/5 activation + attach coverage.
pub fn install_vector_canister(pic: &PocketIc, router: Principal) -> Principal {
    let vector = create_funded_canister(pic);
    pic.install_canister(
        vector,
        wasm_bytes("VECTOR_INDEX_WASM"),
        Encode!(&VectorIndexInitArgs {
            router_canister: router,
        })
        .expect("encode vector init"),
        None,
    );
    vector
}

/// Install the Provision canister with a single bootstrap binding.
pub fn install_provision_canister(
    pic: &PocketIc,
    bootstrap_binding: DeploymentBinding,
) -> Principal {
    let provision = create_funded_canister(pic);
    pic.install_canister(
        provision,
        wasm_bytes("PROVISION_WASM"),
        Encode!(&ProvisionInitArgs {
            bootstrap_bindings: vec![bootstrap_binding],
        })
        .expect("encode provision init"),
        None,
    );
    provision
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
            provision_canister: None
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
            provision_canister: None
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
            provision_canister: None
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
    install_single_shard_federation_with_graph_admins(Default::default())
}

/// Router + index + one federated graph shard, with a social-demo Gateway canister
/// installed and registered as a graph administrator from the start. The Gateway is returned
/// alongside the federation environment so tests can make anonymous composite-query calls
/// through it.
pub fn install_single_shard_federation_with_gateway() -> (FederationEnv, Principal) {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = create_funded_canister(&pic);
    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: None
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
    let gateway = install_social_demo_gateway(&pic, router);
    let mut graph_admins = BTreeSet::new();
    graph_admins.insert(gateway);
    register_graph_single_shard_with_admins(
        &pic,
        admin,
        router,
        index,
        graph_source,
        SOURCE_SHARD,
        graph_admins,
    );

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

    let env = FederationEnv {
        pic,
        admin,
        router,
        index,
        graph_source,
        graph_dest: Principal::anonymous(),
    };
    (env, gateway)
}

/// Router + index + one federated graph shard, with `graph_scoped_callers` added to
/// the graph's `admins` set so those default-Executor principals can execute
/// administrator-registered prepared queries scoped to this graph. The callers are
/// **not** Router admins/owners, so general ad-hoc `gql_query` remains forbidden for them.
pub fn install_single_shard_federation_with_graph_admins(
    graph_admins: BTreeSet<Principal>,
) -> FederationEnv {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = create_funded_canister(&pic);
    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: None
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
    register_graph_single_shard_with_admins(
        &pic,
        admin,
        router,
        index,
        graph_source,
        SOURCE_SHARD,
        graph_admins,
    );

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
    register_graph_single_shard_with_admins(
        pic,
        admin,
        router,
        index,
        graph,
        shard_id,
        Default::default(),
    );
}

fn register_graph_single_shard_with_admins(
    pic: &PocketIc,
    admin: Principal,
    router: Principal,
    index: Principal,
    graph: Principal,
    shard_id: ShardId,
    admins: BTreeSet<Principal>,
) {
    let entry = GraphRegistryEntry {
        graph_id: GraphId::from_raw(0),
        graph_name: GRAPH_NAME.into(),
        canister_id: graph,
        owner: admin,
        admins,
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
            provision_canister: None
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

pub fn e2e_insert_vertex_with_label(
    env: &FederationEnv,
    graph: Principal,
    label_id: u16,
) -> E2eInsertVertexResult {
    update_as_router(
        env,
        graph,
        "e2e_insert_vertex_with_label",
        E2eInsertVertexWithLabelArgs { label_id },
    )
}

pub fn e2e_insert_vertex_with_label_and_property(
    env: &FederationEnv,
    graph: Principal,
    label_id: u16,
    property_id: u32,
    value: i64,
) -> E2eInsertVertexResult {
    update_as_router(
        env,
        graph,
        "e2e_insert_vertex_with_label_and_property",
        E2eInsertVertexWithLabelAndPropertyArgs {
            label_id,
            property_id,
            value,
        },
    )
}
pub fn e2e_insert_vertex_with_label_and_two_properties(
    env: &FederationEnv,
    graph: Principal,
    label_id: u16,
    property_a: u32,
    value_a: i64,
    property_b: u32,
    value_b: i64,
) -> E2eInsertVertexResult {
    update_as_router(
        env,
        graph,
        "e2e_insert_vertex_with_label_and_two_properties",
        E2eInsertVertexWithLabelAndTwoPropertiesArgs {
            label_id,
            property_a,
            value_a,
            property_b,
            value_b,
        },
    )
}

pub fn e2e_set_vertex_property(
    env: &FederationEnv,
    graph: Principal,
    local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    property_id: u32,
    value: i64,
) {
    update_as_router(
        env,
        graph,
        "e2e_set_vertex_property",
        E2eSetVertexPropertyArgs {
            local_vertex_id,
            property_id,
            value,
        },
    )
}

pub fn e2e_set_edge_property(
    env: &FederationEnv,
    graph: Principal,
    source_local: u32,
    target_local: u32,
    property_id: u32,
    value: i64,
) {
    let _: () = update_as_router(
        env,
        graph,
        "e2e_set_edge_property",
        E2eSetEdgePropertyArgs {
            source_local_vertex_id: source_local,
            target_local_vertex_id: target_local,
            property_id,
            value,
        },
    );
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
    gql_execute_idempotent_result_as_admin(env, query, client_mutation_key).row_count
}

/// Like [`gql_execute_idempotent_as_admin`] but returns the full `GqlQueryResult`
/// (lifecycle `phase` and ADR 0029 Phase 2 mutation `token`), not just the row count.
pub fn gql_execute_idempotent_result_as_admin(
    env: &FederationEnv,
    query: &str,
    client_mutation_key: &str,
) -> gleaph_graph_kernel::plan_exec::GqlQueryResult {
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
        Ok(Ok(result)) => result,
        Ok(Err(err)) => panic!("gql_execute_idempotent rejected: {err:?}"),
        Err(err) => panic!("decode gql_execute_idempotent: {err}"),
    }
}

/// Graph shard query (as router): smallest tracked mutation id with unapplied index
/// postings, or `None` when index work has drained (ADR 0029 Phase 2 watermark).
pub fn graph_index_pending_min_mutation_id(
    env: &FederationEnv,
    graph: Principal,
) -> Option<gleaph_graph_kernel::plan_exec::MutationId> {
    use gleaph_graph_kernel::plan_exec::MutationId;

    let bytes = env
        .pic
        .query_call(
            graph,
            env.router,
            "index_pending_min_mutation_id",
            Encode!(&()).expect("encode index_pending_min_mutation_id"),
        )
        .unwrap_or_else(|e| panic!("index_pending_min_mutation_id on {graph}: {e:?}"));
    Decode!(&bytes, Option<MutationId>).expect("decode index_pending_min_mutation_id")
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

pub fn e2e_insert_edge_with_label(
    env: &FederationEnv,
    graph: Principal,
    source_local: u32,
    target_local: u32,
    edge_label_id: u16,
) {
    let _: () = update_as_router(
        env,
        graph,
        "e2e_insert_directed_edge_with_label",
        E2eInsertDirectedEdgeWithLabelArgs {
            source_local_vertex_id: source_local,
            target_local_vertex_id: target_local,
            edge_label_id,
        },
    );
}

pub fn e2e_insert_directed_edge_with_inline_value(
    env: &FederationEnv,
    graph: Principal,
    source_local: u32,
    target_local: u32,
    edge_label_id: u16,
    payload: Vec<u8>,
    inline_value_profile: gleaph_graph_kernel::entry::EdgeInlineValueProfile,
) {
    let _: () = update_as_router(
        env,
        graph,
        "e2e_insert_directed_edge_with_inline_value",
        E2eInsertDirectedEdgeWithPayloadArgs {
            source_local_vertex_id: source_local,
            target_local_vertex_id: target_local,
            edge_label_id,
            payload,
            inline_value_profile,
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

/// Router composite `gql_query` as admin with explicit parameter blob.
pub fn gql_query_with_params_as_admin(
    env: &FederationEnv,
    query: &str,
    params_blob: Vec<u8>,
) -> gleaph_graph_kernel::plan_exec::GqlQueryResult {
    gql_query_with_params_on_router(&env.pic, env.admin, env.router, query, params_blob)
}

/// Router composite `gql_query` with explicit caller, router, and parameter blob.
pub fn gql_query_with_params_on_router(
    pic: &PocketIc,
    caller: Principal,
    router: Principal,
    query: &str,
    params_blob: Vec<u8>,
) -> gleaph_graph_kernel::plan_exec::GqlQueryResult {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::GqlQueryResult;

    let bytes = pic
        .query_call(
            router,
            caller,
            "gql_query",
            Encode!(&query.to_string(), &params_blob).expect("encode gql_query"),
        )
        .unwrap_or_else(|e| panic!("gql_query on router: {e:?}"));
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => panic!("gql_query rejected: {err:?}"),
        Err(err) => panic!("decode gql_query: {err}"),
    }
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

/// Register a named prepared query as the bootstrap admin principal.
pub fn prepared_register_as_admin(env: &FederationEnv, name: &str, query: &str) {
    use gleaph_graph_kernel::federation::RouterError;

    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "prepared_register",
            Encode!(&name.to_string(), &query.to_string()).expect("encode prepared_register"),
        )
        .unwrap_or_else(|e| panic!("prepared_register on router: {e:?}"));
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("prepared_register rejected: {err:?}"),
        Err(err) => panic!("decode prepared_register: {err}"),
    }
}

/// Execute a registered prepared query as `caller` with an explicit parameter blob.
pub fn prepared_execute_query_with_params_as(
    env: &FederationEnv,
    caller: Principal,
    name: &str,
    params_blob: Vec<u8>,
) -> gleaph_graph_kernel::plan_exec::GqlQueryResult {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::GqlQueryResult;

    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "prepared_execute_query",
            Encode!(&name.to_string(), &params_blob).expect("encode prepared_execute_query"),
        )
        .unwrap_or_else(|e| panic!("prepared_execute_query on router: {e:?}"));
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => panic!("prepared_execute_query rejected: {err:?}"),
        Err(err) => panic!("decode prepared_execute_query: {err}"),
    }
}

/// Install the social-demo-gateway canister, wiring it to the given Router.
pub fn install_social_demo_gateway(pic: &PocketIc, router: Principal) -> Principal {
    let gateway = create_funded_canister(pic);
    pic.install_canister(
        gateway,
        wasm_bytes("SOCIAL_DEMO_GATEWAY_WASM"),
        Encode!(&GatewayInitArgs {
            router_canister: router
        })
        .expect("encode gateway init"),
        None,
    );
    gateway
}

/// Execute a fixed social-demo scenario through the Gateway as `caller`.
pub fn execute_social_demo_scenario_as(
    env: &FederationEnv,
    caller: Principal,
    gateway: Principal,
    scenario: SocialDemoScenario,
) -> gleaph_graph_kernel::plan_exec::GqlQueryResult {
    use gleaph_graph_kernel::plan_exec::GqlQueryResult;
    use gleaph_social_demo_gateway::SocialDemoGatewayError;

    let bytes = env
        .pic
        .query_call(
            gateway,
            caller,
            "execute_social_demo_scenario",
            Encode!(&scenario).expect("encode execute_social_demo_scenario"),
        )
        .unwrap_or_else(|e| panic!("execute_social_demo_scenario on gateway: {e:?}"));
    match Decode!(&bytes, Result<GqlQueryResult, SocialDemoGatewayError>) {
        Ok(Ok(result)) => result,
        Ok(Err(SocialDemoGatewayError::Router(err))) => {
            panic!("gateway scenario rejected by router: {err:?}")
        }
        Ok(Err(err)) => panic!("gateway scenario failed: {err:?}"),
        Err(err) => panic!("decode execute_social_demo_scenario: {err}"),
    }
}

/// Router composite `gql_query` as `caller`, returning the raw `Result` so a test can
/// assert the exact rejection reason when ad-hoc GQL is forbidden.
pub fn gql_query_as(
    env: &FederationEnv,
    caller: Principal,
    query: &str,
) -> Result<
    gleaph_graph_kernel::plan_exec::GqlQueryResult,
    gleaph_graph_kernel::federation::RouterError,
> {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::GqlQueryResult;

    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "gql_query",
            Encode!(&query.to_string(), &Vec::<u8>::new()).expect("encode gql_query"),
        )
        .unwrap_or_else(|e| panic!("gql_query on router: {e:?}"));
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query")
}

/// Router composite `gql_query_with_consistency` (ADR 0029 §5, Phase 3) as admin.
///
/// Returns the raw `Result` so a test can assert both the served (`Ok`) and the retryable
/// projection-lag / rejected (`Err`) outcomes of the read barrier.
pub fn gql_query_with_consistency_as_admin(
    env: &FederationEnv,
    query: &str,
    read_mode: gleaph_graph_kernel::plan_exec::ReadMode,
) -> Result<
    gleaph_graph_kernel::plan_exec::GqlQueryResult,
    gleaph_graph_kernel::federation::RouterError,
> {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::GqlQueryResult;

    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "gql_query_with_consistency",
            Encode!(&query.to_string(), &Vec::<u8>::new(), &read_mode)
                .expect("encode gql_query_with_consistency"),
        )
        .unwrap_or_else(|e| panic!("gql_query_with_consistency on router: {e:?}"));
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query_with_consistency")
}

/// Router `mutation_status` (ADR 0029 Phase 4) as the bootstrap admin principal. Returns the
/// raw `Result` so a test can assert both a found saga and the not-found error.
pub fn mutation_status_as_admin(
    env: &FederationEnv,
    logical_graph_name: &str,
    client_mutation_key: &str,
) -> Result<gleaph_router::types::MutationStatus, gleaph_graph_kernel::federation::RouterError> {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_router::types::MutationStatus;

    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "mutation_status",
            Encode!(
                &logical_graph_name.to_string(),
                &client_mutation_key.to_string()
            )
            .expect("encode mutation_status"),
        )
        .unwrap_or_else(|e| panic!("mutation_status on router: {e:?}"));
    Decode!(&bytes, Result<MutationStatus, RouterError>).expect("decode mutation_status")
}

/// Test-only (`pocket-ic-e2e`): inject a projection-lagging federated saga under `client_mutation_key`
/// referencing an already-committed `mutation_id` (e.g. a token from a prior idempotent DML). Used to
/// exercise the autonomous recovery driver's `ProjectionPending` -> `Completed` convergence.
pub fn test_inject_projection_pending_saga(
    env: &FederationEnv,
    logical_graph_name: &str,
    client_mutation_key: &str,
    mutation_id: u64,
    row_count: u64,
) {
    use gleaph_graph_kernel::federation::RouterError;

    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "test_inject_projection_pending_saga",
            Encode!(
                &logical_graph_name.to_string(),
                &client_mutation_key.to_string(),
                &mutation_id,
                &row_count
            )
            .expect("encode test_inject_projection_pending_saga"),
        )
        .unwrap_or_else(|e| panic!("test_inject_projection_pending_saga on router: {e:?}"));
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("test_inject_projection_pending_saga rejected: {err:?}"),
        Err(err) => panic!("decode test_inject_projection_pending_saga: {err:?}"),
    }
}

/// Test-only (`pocket-ic-e2e`): declare a uniqueness constraint (ADR 0030). Public `CREATE`/`DROP
/// CONSTRAINT` DDL stays `NotImplemented` (CREATE pending the publication decision, DROP pending a
/// dedicated lifecycle slice — ADR 0030 Revisions #14–#15), so the E2E suite reaches the
/// admin-authorized, declare-on-empty store path through this seam. The constraint must be declared
/// on a **brand-new** vertex label (declare-on-empty) — call it before inserting any such vertex.
pub fn test_declare_unique_constraint(
    env: &FederationEnv,
    logical_graph_name: &str,
    constraint_name: &str,
    label: &str,
    property: &str,
) {
    use gleaph_graph_kernel::federation::RouterError;

    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "test_declare_unique_constraint",
            Encode!(
                &logical_graph_name.to_string(),
                &constraint_name.to_string(),
                &label.to_string(),
                &property.to_string()
            )
            .expect("encode test_declare_unique_constraint"),
        )
        .unwrap_or_else(|e| panic!("test_declare_unique_constraint on router: {e:?}"));
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("test_declare_unique_constraint rejected: {err:?}"),
        Err(err) => panic!("decode test_declare_unique_constraint: {err:?}"),
    }
}

/// Test-only (`pocket-ic-e2e`): invoke the constraint-declaration seam as an arbitrary `sender`,
/// returning the router's `Result` so callers can assert the admin guard rejects non-admins.
pub fn test_declare_unique_constraint_as(
    env: &FederationEnv,
    sender: Principal,
    logical_graph_name: &str,
    constraint_name: &str,
    label: &str,
    property: &str,
) -> Result<(), gleaph_graph_kernel::federation::RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            sender,
            "test_declare_unique_constraint",
            Encode!(
                &logical_graph_name.to_string(),
                &constraint_name.to_string(),
                &label.to_string(),
                &property.to_string()
            )
            .expect("encode test_declare_unique_constraint"),
        )
        .unwrap_or_else(|e| panic!("test_declare_unique_constraint on router: {e:?}"));
    Decode!(
        &bytes,
        Result<(), gleaph_graph_kernel::federation::RouterError>
    )
    .expect("decode test_declare_unique_constraint")
}

/// Advance simulated time past the ADR 0030 reclaim eligibility window
/// (`UNIQUE_RESERVATION_TTL_NS`, 30 min) and tick repeatedly so the recovery timer's reclaim driver
/// (Driver 1) becomes eligible to act on an overdue `Reserved` reservation. Use for the
/// failure-injection reclaim/Cancel paths; for the prompt projection/effect drivers
/// [`run_router_recovery_timer`] is enough.
pub fn run_router_recovery_after_reservation_ttl(env: &FederationEnv) {
    use std::time::Duration;

    // Jump just past the 30-minute reservation TTL so the next reclaim scan finds the entry overdue.
    env.pic.advance_time(Duration::from_secs(31 * 60));
    for _ in 0..40 {
        env.pic.advance_time(Duration::from_secs(3));
        for _ in 0..12 {
            env.pic.tick();
        }
    }
}

/// Advance simulated time past **both** retention windows the ADR 0030 outbox-vs-eviction test must
/// surpass: the router client-mutation-key TTL (`CLIENT_MUTATION_KEY_TTL_NS`, 7 days, ADR 0025) and
/// the longer graph mutation-journal retention (9 days, ADR 0027). Advancing past the larger (9-day)
/// window means a subsequent router key sweep **and** a graph journal eviction both treat every
/// record as age-expired, so the test can prove a reservation-pinned router record and a pinned
/// `Acquire` outbox effect both survive while the graph journal entry is evicted. Ticks once;
/// callers drive convergence.
pub fn advance_past_journal_eviction(env: &FederationEnv) {
    use std::time::Duration;

    // 9-day graph journal retention (ADR 0027) + 1h margin — also past the 7-day router key TTL.
    env.pic
        .advance_time(Duration::from_secs(9 * 24 * 60 * 60 + 60 * 60));
    env.pic.tick();
}

/// Arm (or clear, with `0`) the graph shard's ADR 0030 unique-effect ack fault (PocketIC E2E seam),
/// so the failure-injection suite can trap the Router's `Acquire` ack and exercise slice-6 re-ack
/// recovery. Called with the router principal as sender (the shard's control-plane guard).
pub fn arm_graph_unique_ack_fault(env: &FederationEnv, shard: Principal, code: u8) {
    let _: () = update_as_router(env, shard, "e2e_arm_unique_ack_fault", code);
}

/// Count of currently pinned (un-acked) unique effects in a graph shard's outbox (PocketIC E2E).
pub fn graph_unique_outbox_len(env: &FederationEnv, shard: Principal) -> u64 {
    query_as_router(env, shard, "e2e_unique_outbox_len", ())
}

/// Count of entries in a graph shard's mutation journal (PocketIC E2E).
pub fn graph_mutation_journal_len(env: &FederationEnv, shard: Principal) -> u64 {
    query_as_router(env, shard, "e2e_mutation_journal_len", ())
}

/// Run a full graph mutation-journal retention sweep (ADR 0027, 9-day window) on a graph shard at the
/// current simulated time and return the remaining entry count (PocketIC E2E).
pub fn evict_graph_mutation_journal(env: &FederationEnv, shard: Principal) -> u64 {
    update_as_router(env, shard, "e2e_evict_mutation_journal", ())
}

// --- two-shard (`install_federation`) FederatedTcc helpers (ADR 0030 slice 10) ---
//
// Slice 10 froze single-shard constraints to the `ShardLocalGlobal` fast path, which bypasses the
// federated reservation / outbox / ack machinery. Tests that exercise that machinery must run on a
// two-shard graph (`install_federation`), where a constrained value routes by hash to *one* of the
// two shards. These helpers act on *both* shards so a test stays agnostic to which one owns the value.

/// Arm/clear the unique-ack fault on both shards of a two-shard federation (PocketIC E2E).
pub fn arm_graph_unique_ack_fault_all_shards(env: &FederationEnv, code: u8) {
    arm_graph_unique_ack_fault(env, env.graph_source, code);
    arm_graph_unique_ack_fault(env, env.graph_dest, code);
}

/// Sum of currently pinned (un-acked) unique effects across both shards (PocketIC E2E).
pub fn graph_unique_outbox_len_all_shards(env: &FederationEnv) -> u64 {
    graph_unique_outbox_len(env, env.graph_source) + graph_unique_outbox_len(env, env.graph_dest)
}

/// Sum of mutation-journal entries across both shards (PocketIC E2E).
pub fn graph_mutation_journal_len_all_shards(env: &FederationEnv) -> u64 {
    graph_mutation_journal_len(env, env.graph_source)
        + graph_mutation_journal_len(env, env.graph_dest)
}

/// Evict the mutation journal on both shards and return the summed remaining entry count (PocketIC E2E).
pub fn evict_graph_mutation_journal_all_shards(env: &FederationEnv) -> u64 {
    evict_graph_mutation_journal(env, env.graph_source)
        + evict_graph_mutation_journal(env, env.graph_dest)
}

/// Stop both shards of a two-shard federation so a constrained value's canonical dispatch fails no
/// matter which shard owns it (PocketIC E2E).
pub fn stop_graph_shards_all(env: &FederationEnv) {
    stop_graph_shard(env, env.graph_source);
    stop_graph_shard(env, env.graph_dest);
}

/// Start both shards of a two-shard federation (PocketIC E2E).
pub fn start_graph_shards_all(env: &FederationEnv) {
    start_graph_shard(env, env.graph_source);
    start_graph_shard(env, env.graph_dest);
}

/// Test-only (`pocket-ic-e2e`): force a `Reserved` reservation for `(label, property, value)` into
/// `Reclaiming` (admin). Returns whether the transition happened. See `gleaph_router` `test_fault`
/// neighbours; used by the ADR 0030 reclaim-during-retry fence test.
pub fn test_force_reclaiming(
    env: &FederationEnv,
    logical_graph_name: &str,
    label: &str,
    property: &str,
    value: &str,
) -> bool {
    use gleaph_graph_kernel::federation::RouterError;

    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "test_force_reclaiming",
            Encode!(
                &logical_graph_name.to_string(),
                &label.to_string(),
                &property.to_string(),
                &value.to_string()
            )
            .expect("encode test_force_reclaiming"),
        )
        .unwrap_or_else(|e| panic!("test_force_reclaiming on router: {e:?}"));
    Decode!(&bytes, Result<bool, RouterError>)
        .expect("decode test_force_reclaiming")
        .unwrap_or_else(|err| panic!("test_force_reclaiming rejected: {err:?}"))
}

/// Run one full expired-client-mutation-key sweep pass (admin) and return how many records it
/// evicted. A record pinned by a non-terminal reservation or a pending unique-effect row must not be
/// evicted (ADR 0030 slice 6 GC-pin).
pub fn admin_sweep_mutation_keys(env: &FederationEnv, max_scan: u32) -> u32 {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_router::types::{AdminSweepMutationKeysStepArgs, AdminSweepMutationKeysStepResult};

    let args = AdminSweepMutationKeysStepArgs {
        start_after: None,
        max_scan,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_sweep_expired_client_mutation_keys",
            Encode!(&args).expect("encode admin_sweep_expired_client_mutation_keys"),
        )
        .unwrap_or_else(|e| panic!("admin_sweep_expired_client_mutation_keys on router: {e:?}"));
    let result = Decode!(&bytes, Result<AdminSweepMutationKeysStepResult, RouterError>)
        .expect("decode admin_sweep_expired_client_mutation_keys")
        .unwrap_or_else(|err| panic!("admin_sweep_expired_client_mutation_keys rejected: {err:?}"));
    result.removed
}

/// Advance simulated time and tick so the router's autonomous recovery timer fires
/// (ADR 0029 Phase 4).
pub fn run_router_recovery_timer(env: &FederationEnv) {
    use std::time::Duration;

    for _ in 0..6 {
        env.pic.advance_time(Duration::from_secs(3));
        for _ in 0..12 {
            env.pic.tick();
        }
    }
}

/// Stop a graph-shard canister to simulate a crashed / unavailable shard mid-saga
/// (federated-saga recovery tests). Canisters are created with the anonymous controller, so the
/// management call is sent as `None` (anonymous).
pub fn stop_graph_shard(env: &FederationEnv, shard: Principal) {
    env.pic
        .stop_canister(shard, None)
        .unwrap_or_else(|e| panic!("stop graph shard {shard}: {e:?}"));
}

/// Restart a previously [`stop_graph_shard`]-ed graph-shard canister.
pub fn start_graph_shard(env: &FederationEnv, shard: Principal) {
    env.pic
        .start_canister(shard, None)
        .unwrap_or_else(|e| panic!("start graph shard {shard}: {e:?}"));
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

/// Register and fully activate a single-shard vector index for the social demo graph.
///
/// Registers `embedding_name` with dimension `dims` and `L2Squared` metric, enables global vector
/// dispatch, wires the graph shard to `vector`, and attaches the shard bidirectionally.
pub fn admin_fully_activate_social_vector_index(
    env: &FederationEnv,
    vector: Principal,
    index_id: u32,
    embedding_name: &str,
    dims: u16,
) {
    let register_args = RegisterVectorIndexArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        embedding_name: embedding_name.to_string(),
        index_id,
        dims,
        metric: Some(VectorMetric::L2Squared),
        target: Some(vector),
        if_not_exists: false,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_register_vector_index",
            Encode!(&register_args).expect("encode admin_register_vector_index"),
        )
        .expect("admin_register_vector_index");
    let registered: Result<bool, gleaph_graph_kernel::federation::RouterError> =
        Decode!(&bytes, Result<bool, gleaph_graph_kernel::federation::RouterError>)
            .expect("decode admin_register_vector_index");
    assert!(
        registered.expect("admin_register_vector_index succeeds"),
        "first vector index registration must create the index"
    );

    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_vector_dispatch_activation",
            Encode!(&true).expect("encode admin_set_vector_dispatch_activation"),
        )
        .expect("admin_set_vector_dispatch_activation");
    let _: () = Decode!(&bytes, Result<(), gleaph_graph_kernel::federation::RouterError>)
        .expect("decode admin_set_vector_dispatch_activation")
        .expect("dispatch activation succeeds");

    let graph_id = {
        let bytes = env
            .pic
            .query_call(
                env.router,
                env.admin,
                "lookup_graph_id",
                Encode!(&GRAPH_NAME.to_string()).expect("encode lookup_graph_id"),
            )
            .expect("lookup_graph_id");
        Decode!(
            &bytes,
            Result<GraphId, gleaph_graph_kernel::federation::RouterError>
        )
        .expect("decode lookup_graph_id")
        .expect("graph id found")
    };

    let bytes = env
        .pic
        .update_call(
            env.graph_source,
            env.router,
            "admin_set_vector_index_canister",
            Encode!(&vector).expect("encode admin_set_vector_index_canister"),
        )
        .expect("admin_set_vector_index_canister");
    let _: () = Decode!(&bytes, Result<(), String>)
        .expect("decode admin_set_vector_index_canister")
        .expect("graph accepts vector routing");

    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_attach_shard_canister",
            Encode!(&graph_id, &ShardId::new(0), &env.graph_source).expect("encode vector attach"),
        )
        .expect("vector admin_attach_shard_canister");
    let _: () = Decode!(&bytes, Result<(), String>)
        .expect("decode vector attach")
        .expect("vector accepts shard");

    let attach_args = AdminAttachVectorIndexShardArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        shard_id: ShardId::new(0),
        vector_index_canister: vector,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_attach_vector_index_shard",
            Encode!(&attach_args).expect("encode admin_attach_vector_index_shard"),
        )
        .expect("admin_attach_vector_index_shard");
    let _: () = Decode!(&bytes, Result<(), gleaph_graph_kernel::federation::RouterError>)
        .expect("decode admin_attach_vector_index_shard")
        .expect("router attaches shard");
}

/// Map a canonical social Post text key (e.g. `post-alice-1`) to its deterministic
/// numeric `demo_id` assigned by `frontend/apps/social-demo/scripts/build-config.mjs`.
fn social_post_demo_id_to_numeric(demo_id: &str) -> i64 {
    match demo_id {
        "post-alice-1" => 9,
        "post-bob-1" => 10,
        "post-bob-2" => 11,
        "post-carol-1" => 12,
        "post-dave-1" => 13,
        "post-eve-1" => 14,
        "post-eve-private" => 15,
        other => panic!("unknown social post demo_id: {other}"),
    }
}

/// Resolve the opaque encoded `ELEMENT_ID` for one seeded Post by its `demo_id`.
pub fn resolve_social_post_element_id(env: &FederationEnv, demo_id: &str) -> Vec<u8> {
    let numeric_id = social_post_demo_id_to_numeric(demo_id);
    let query = format!(
        "MATCH (p:Post {{demo_id: {}}}) RETURN ELEMENT_ID(p) AS element_id",
        numeric_id
    );
    let result =
        gql_query_with_params_on_router(&env.pic, env.admin, env.router, &query, Vec::new());
    element_id_bytes_from_gql_result(&result, "element_id")
}
/// Encode a deterministic `f32` embedding vector into little-endian bytes.
pub fn encode_f32_embedding(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|v| v.to_le_bytes().to_vec())
        .collect()
}

/// Ingest every Post embedding from the canonical social manifest through Router's canonical
/// ingestion boundary. `embeddings` is the `embeddings` object from the generated social seeds
/// artifact, keyed by Post `demo_id`.
pub fn admin_ingest_social_embeddings(env: &FederationEnv, embeddings: &serde_json::Value) {
    let embeddings = embeddings
        .as_object()
        .expect("social embeddings must be a JSON object");
    for (demo_id, meta) in embeddings {
        let encoded = resolve_social_post_element_id(env, demo_id);
        let values: Vec<f32> = meta["values"]
            .as_array()
            .unwrap_or_else(|| panic!("embedding values for {demo_id} must be an array"))
            .iter()
            .map(|v| {
                v.as_f64()
                    .unwrap_or_else(|| panic!("embedding value for {demo_id} must be a number"))
                    as f32
            })
            .collect();
        let args = AdminIngestVertexEmbeddingArgs {
            logical_graph_name: GRAPH_NAME.to_string(),
            encoded_vertex_id: encoded,
            embedding_name: "post_vec".to_string(),
            values,
        };
        let bytes = env
            .pic
            .update_call(
                env.router,
                env.admin,
                "admin_ingest_vertex_embedding",
                Encode!(&args).expect("encode admin_ingest_vertex_embedding"),
            )
            .expect("admin_ingest_vertex_embedding");
        let result: Result<
            gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult,
            gleaph_graph_kernel::federation::RouterError,
        > = Decode!(
            &bytes,
            Result<
                gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult,
                gleaph_graph_kernel::federation::RouterError,
            >
        )
        .expect("decode admin_ingest_vertex_embedding");
        let outcome = result.expect("admin_ingest_vertex_embedding succeeds");
        assert_eq!(
            outcome.embedding_version, 1,
            "first canonical write for {demo_id} must be version 1"
        );
        assert!(
            matches!(
                outcome.projection_outcome,
                VertexEmbeddingProjectionOutcome::Applied
            ),
            "embedding projection for {demo_id} must be applied on activated index"
        );
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

/// Test-only (`pocket-ic-e2e`): arm (or clear, with `0`) an ADR 0030 write-path fault injection on
/// the router (admin). See `gleaph_router::test_fault`.
pub fn arm_router_fault(env: &FederationEnv, code: u8) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "test_arm_fault",
            Encode!(&code).expect("encode test_arm_fault"),
        )
        .unwrap_or_else(|e| panic!("test_arm_fault on router: {e:?}"));
    Decode!(
        &bytes,
        Result<(), gleaph_graph_kernel::federation::RouterError>
    )
    .expect("decode test_arm_fault")
    .unwrap_or_else(|err| panic!("test_arm_fault rejected: {err:?}"));
}

/// Run `gql_execute_idempotent` as admin expecting the message itself to **trap** (an injected
/// fault), i.e. the ingress is rejected rather than returning an application `Result`.
pub fn gql_execute_idempotent_as_admin_expect_trap(
    env: &FederationEnv,
    query: &str,
    client_mutation_key: &str,
) {
    let result = env.pic.update_call(
        env.router,
        env.admin,
        "gql_execute_idempotent",
        Encode!(
            &query.to_string(),
            &Vec::<u8>::new(),
            &client_mutation_key.to_string()
        )
        .expect("encode gql_execute_idempotent"),
    );
    assert!(
        result.is_err(),
        "expected the injected fault to trap the ingress, got Ok({:?})",
        result.ok().map(|bytes| bytes.len())
    );
}

/// Submit two `gql_execute_idempotent` ingress messages (as admin) **before** executing any round,
/// then drive rounds so they interleave (each yields at its dispatch `await`). Returns both decoded
/// application results so a caller can assert exactly one winner. Used for the ADR 0030 true
/// concurrent same-value conflict test.
#[allow(clippy::type_complexity)]
pub fn gql_execute_idempotent_pair_concurrent_as_admin(
    env: &FederationEnv,
    query_a: &str,
    key_a: &str,
    query_b: &str,
    key_b: &str,
) -> (
    Result<
        gleaph_graph_kernel::plan_exec::GqlQueryResult,
        gleaph_graph_kernel::federation::RouterError,
    >,
    Result<
        gleaph_graph_kernel::plan_exec::GqlQueryResult,
        gleaph_graph_kernel::federation::RouterError,
    >,
) {
    let encode = |query: &str, key: &str| {
        Encode!(&query.to_string(), &Vec::<u8>::new(), &key.to_string())
            .expect("encode gql_execute_idempotent")
    };
    let msg_a = env
        .pic
        .submit_call(
            env.router,
            env.admin,
            "gql_execute_idempotent",
            encode(query_a, key_a),
        )
        .expect("submit gql_execute_idempotent a");
    let msg_b = env
        .pic
        .submit_call(
            env.router,
            env.admin,
            "gql_execute_idempotent",
            encode(query_b, key_b),
        )
        .expect("submit gql_execute_idempotent b");
    // Both ingress messages are now queued; ticking executes them in interleaved rounds.
    for _ in 0..60 {
        env.pic.tick();
    }
    let result_a = match env.pic.await_call(msg_a) {
        Ok(bytes) => Decode!(
            &bytes,
            Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, gleaph_graph_kernel::federation::RouterError>
        )
        .expect("decode gql_execute_idempotent a"),
        Err(reject) => panic!("concurrent ingress a unexpectedly trapped: {reject:?}"),
    };
    let result_b = match env.pic.await_call(msg_b) {
        Ok(bytes) => Decode!(
            &bytes,
            Result<gleaph_graph_kernel::plan_exec::GqlQueryResult, gleaph_graph_kernel::federation::RouterError>
        )
        .expect("decode gql_execute_idempotent b"),
        Err(reject) => panic!("concurrent ingress b unexpectedly trapped: {reject:?}"),
    };
    (result_a, result_b)
}

const KNOWLEDGE_MAP_SEEDS_JSON: &str =
    include_str!("../../../frontend/apps/knowledge-map/seeds/knowledge-map-seeds.json");

const SOCIAL_SEEDS_JSON: &str =
    include_str!("../../../frontend/apps/knowledge-map/seeds/social-seeds.json");

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
/// Seed the social demo graph through Router `gql_execute_idempotent`.
pub fn seed_social_graph(env: &FederationEnv) {
    let parsed: serde_json::Value =
        serde_json::from_str(SOCIAL_SEEDS_JSON).expect("parse social seeds");
    for seed in parsed["seeds"].as_array().expect("social seed array") {
        let gql = seed["gql"].as_str().expect("social seed gql");
        let key = seed["key"].as_str().expect("social seed key");
        let row_count = gql_execute_idempotent_as_admin(env, gql, key);
        assert_eq!(row_count, 0, "seed {key} should not return rows");
    }

    // Verify every seeded Post body is readable from the graph.
    use gleaph_gql::Value;
    use gleaph_gql_ic::IcWirePlanQueryResult;

    let expected_bodies: std::collections::HashMap<&str, &str> = [
        ("post-alice-1", "Alice's public reply"),
        ("post-bob-1", "Bob's topic note"),
        ("post-bob-2", "Bob's second note"),
        ("post-carol-1", "Carol's public note"),
        ("post-dave-1", "Dave's public note"),
        ("post-eve-1", "Eve's public note"),
        ("post-eve-private", "Eve's draft"),
    ]
    .into_iter()
    .collect();
    for (post_id, expected_body) in expected_bodies {
        let numeric_id = social_post_demo_id_to_numeric(post_id);
        let query = format!("MATCH (p:Post {{demo_id: {}}}) RETURN p.body AS body LIMIT 1", numeric_id);
        let result = gql_query_with_params_on_router(&env.pic, env.admin, env.router, &query, Vec::new());
        let rows_blob = result.rows_blob.as_ref().expect("body query should return rows_blob");
        let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode body rows");
        assert_eq!(
            wire.rows.len(),
            1,
            "body query for {post_id} should return exactly one row"
        );
        let row = wire.rows.into_iter().next().expect("one row").try_into_value_row().expect("wire row to value row");
        let body = match row.get("body").unwrap_or_else(|| panic!("missing body column for {post_id}")) {
            Value::Text(value) => value.as_str(),
            other => panic!("expected Text body for {post_id}, got {other:?}"),
        };
        assert_eq!(
            body, expected_body,
            "body mismatch for {post_id}"
        );
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
