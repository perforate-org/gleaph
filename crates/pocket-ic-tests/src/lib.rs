//! Shared helpers for PocketIC federation tests.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
use gleaph_graph_kernel::federation::{GlobalVertexId, ShardId, VertexPlacement};
use gleaph_router::RouterInitArgs;
use gleaph_router::types::AdminRegisterShardArgs;
use pocket_ic::{PocketIc, PocketIcBuilder};
use std::path::PathBuf;

/// PocketIC instance using `POCKET_IC_BIN` at runtime, or `.pocket-ic/pocket-ic` from `build.rs`.
pub fn new_pocket_ic() -> PocketIc {
    let server_binary = std::env::var_os("POCKET_IC_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("POCKET_IC_BIN")));
    PocketIcBuilder::new()
        .with_server_binary(server_binary)
        .with_application_subnet()
        .build()
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

pub fn register_graph_single_shard(
    pic: &PocketIc,
    admin: Principal,
    router: Principal,
    index: Principal,
    graph: Principal,
    shard_id: ShardId,
) {
    let entry = GraphRegistryEntry {
        graph_name: GRAPH_NAME.into(),
        canister_id: graph,
        owner: admin,
        admins: Default::default(),
        status: GraphStatus::Active,
        version: 1,
        updated_at_ns: 0,
        provisioning_state: ProvisioningState::None,
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
        graph_name: GRAPH_NAME.into(),
        canister_id: graph_source,
        owner: admin,
        admins: Default::default(),
        status: GraphStatus::Active,
        version: 1,
        updated_at_ns: 0,
        provisioning_state: ProvisioningState::None,
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
            Encode!(
                &GRAPH_NAME.to_string(),
                &query.to_string(),
                &Vec::<u8>::new()
            )
            .expect("encode gql_query"),
        )
        .unwrap_or_else(|e| panic!("gql_query on router: {e:?}"));
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => panic!("gql_query rejected: {err:?}"),
        Err(err) => panic!("decode gql_query: {err}"),
    }
}
