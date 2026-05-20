//! Request bodies for canister methods (called from `lib.rs` ic-cdk entrypoints).

use std::collections::BTreeMap;

use crate::gql_execution_context::GqlExecutionContext;
use crate::facade::migration::{
    export_local_vertex_for_migration, import_migrated_vertex, tombstone_migrated_vertex,
};
#[cfg(target_family = "wasm")]
use crate::facade::migration::{
    import_migrated_vertex_with_index, tombstone_migrated_vertex_with_index,
};
use crate::facade::{FederationRouting, GraphMetadata, GraphStore};
use crate::gql_run::{kernel_execution_mode, run_wire_plan_last_read_row_count};
use crate::index::ic::IcPropertyIndexClient;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::router::verify_shard_attachment;
use candid::Decode;
use gleaph_gql::Value;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_graph_kernel::federation::{
    BeginVertexMigrationArgs, ExportedVertex, FederatedExpandArgs, FederatedExpandNeighbor,
};
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, ExecutePlanResult, GqlExecutionMode, SeedBindingsWire,
};
use ic_stable_lara::VertexId;

use super::types::GraphInitArgs;

pub fn init(args: GraphInitArgs) {
    let federation_routing = match (args.router_canister, args.shard_id) {
        (Some(router_canister), Some(shard_id)) => {
            let entry = verify_shard_attachment(
                router_canister,
                shard_id,
                args.logical_graph_name.as_deref(),
            )
            .unwrap_or_else(|e| ic_cdk::trap(e.to_string()));
            Some(FederationRouting {
                router_canister,
                shard_id,
                index_canister: entry.index_canister,
            })
        }
        (None, None) => None,
        _ => ic_cdk::trap(
            "GraphInitArgs: router_canister and shard_id must both be set or both omitted",
        ),
    };

    let mut metadata = GraphMetadata::default();
    metadata.set_logical_graph_name(args.logical_graph_name);
    metadata.set_federation_routing(federation_routing);

    if let Err(err) = GraphStore::new().set_metadata(metadata) {
        ic_cdk::trap(err.to_string());
    }
}

pub(crate) fn decode_gql_param_map(params: Vec<u8>) -> Result<BTreeMap<String, Value>, String> {
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = canbench_rs::bench_scope("gql_ic_params_blob_decode");
    decode_gql_params_blob(&params).map_err(|e| e.to_string())
}

fn wasm_index_client_holder() -> Option<IcPropertyIndexClient> {
    GraphStore::new()
        .federation_routing()
        .map(|r| IcPropertyIndexClient {
            index_principal: r.index_canister,
            shard_id: r.shard_id,
        })
}

fn ensure_execution_mode(
    args_mode: GqlExecutionMode,
    expected: GqlExecutionMode,
    entrypoint: &str,
) -> Result<(), String> {
    if args_mode != expected {
        return Err(format!(
            "{entrypoint} requires {expected:?} mode (got {args_mode:?})"
        ));
    }
    Ok(())
}

pub async fn execute_plan_query(args: ExecutePlanArgs) -> Result<ExecutePlanResult, String> {
    ensure_execution_mode(args.mode, GqlExecutionMode::Query, "execute_plan_query")?;
    execute_plan_impl(args).await
}

pub async fn execute_plan_update(args: ExecutePlanArgs) -> Result<ExecutePlanResult, String> {
    ensure_execution_mode(args.mode, GqlExecutionMode::Update, "execute_plan_update")?;
    execute_plan_impl(args).await
}

async fn execute_plan_impl(args: ExecutePlanArgs) -> Result<ExecutePlanResult, String> {
    let store = GraphStore::new();
    let routing = store
        .federation_routing()
        .ok_or("federation routing not configured")?;
    if routing.shard_id != args.target_shard_id {
        return Err(format!(
            "target_shard_id {} does not match this graph shard {}",
            args.target_shard_id, routing.shard_id
        ));
    }
    let pmap = decode_gql_param_map(args.params_blob)?;
    let seeds = match args.seed_bindings_blob {
        Some(blob) => {
            let wire: SeedBindingsWire = Decode!(&blob, SeedBindingsWire)
                .map_err(|e| format!("seed_bindings decode: {e}"))?;
            Some(wire)
        }
        None => None,
    };
    #[cfg(target_family = "wasm")]
    let index_holder = wasm_index_client_holder();
    #[cfg(target_family = "wasm")]
    let ix = index_holder.as_ref().map(|c| c as &dyn PropertyIndexLookup);
    #[cfg(not(target_family = "wasm"))]
    let ix: Option<&dyn PropertyIndexLookup> = None;

    let row_count = run_wire_plan_last_read_row_count(
        store,
        &args.plan_blob,
        &pmap,
        kernel_execution_mode(args.mode),
        ix,
        GqlExecutionContext::default(),
        seeds,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(ExecutePlanResult {
        row_count: row_count as u64,
    })
}

pub fn bootstrap_graph_peers(
    args: gleaph_graph_kernel::federation::BootstrapGraphPeersArgs,
) -> Result<(), String> {
    let self_canister = ic_cdk::api::canister_self();
    GraphStore::new().bootstrap_peer_graph_canisters(&args.peers, self_canister);
    Ok(())
}

pub fn add_graph_peer(args: gleaph_graph_kernel::federation::AddGraphPeerArgs) -> Result<(), String> {
    let self_canister = ic_cdk::api::canister_self();
    GraphStore::new().add_peer_graph_canister(args.peer, self_canister);
    Ok(())
}

pub fn remove_graph_peer(
    args: gleaph_graph_kernel::federation::RemoveGraphPeerArgs,
) -> Result<(), String> {
    GraphStore::new().remove_peer_graph_canister(&args.peer);
    Ok(())
}

pub fn migration_begin(args: BeginVertexMigrationArgs) -> Result<(), String> {
    let store = GraphStore::new();
    let routing = store
        .federation_routing()
        .ok_or("federation routing not configured")?;
    crate::index::placement::begin_vertex_migration(routing.router_canister, args)
        .map_err(|e| e.to_string())
}

pub fn federated_expand(args: FederatedExpandArgs) -> Result<Vec<FederatedExpandNeighbor>, String> {
    let store = GraphStore::new();
    crate::facade::federation_expand::collect_federated_expand(&store, args)
        .map_err(|e| e.to_string())
}

pub fn migration_export(local_vertex_id: u32) -> Result<ExportedVertex, String> {
    let store = GraphStore::new();
    export_local_vertex_for_migration(&store, VertexId::from(local_vertex_id))
        .map_err(|e| e.to_string())
}

pub async fn migration_import(bundle: ExportedVertex) -> Result<u32, String> {
    let store = GraphStore::new();
    #[cfg(target_family = "wasm")]
    {
        let index_holder = wasm_index_client_holder().ok_or_else(|| {
            "federated graph shard requires index_canister for migration import".to_string()
        })?;
        let vertex_id = import_migrated_vertex_with_index(
            &store,
            bundle,
            &index_holder as &dyn PropertyIndexLookup,
        )
        .await
        .map_err(|e| e.to_string())?;
        return Ok(u32::from(vertex_id));
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let vertex_id = import_migrated_vertex(&store, bundle).map_err(|e| e.to_string())?;
        Ok(u32::from(vertex_id))
    }
}

pub async fn migration_tombstone(local_vertex_id: u32) -> Result<(), String> {
    let store = GraphStore::new();
    let vertex_id = VertexId::from(local_vertex_id);
    #[cfg(target_family = "wasm")]
    {
        let index_holder = wasm_index_client_holder().ok_or_else(|| {
            "federated graph shard requires index_canister for migration tombstone".to_string()
        })?;
        return tombstone_migrated_vertex_with_index(
            &store,
            vertex_id,
            &index_holder as &dyn PropertyIndexLookup,
        )
        .await
        .map_err(|e| e.to_string());
    }
    #[cfg(not(target_family = "wasm"))]
    {
        tombstone_migrated_vertex(&store, vertex_id).map_err(|e| e.to_string())
    }
}
