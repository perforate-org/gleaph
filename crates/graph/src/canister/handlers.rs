//! Request bodies for canister methods (called from `lib.rs` ic-cdk entrypoints).

use std::collections::BTreeMap;
use std::str::FromStr;

use candid::Principal;
use ic_cdk::api::msg_caller;

use crate::GqlExecutionContext;
use crate::auth::{admin_upsert_principal, bootstrap_canister_auth, caller_role};
use crate::facade::migration::{
    export_local_vertex_for_migration, import_migrated_vertex, tombstone_migrated_vertex,
};
#[cfg(target_family = "wasm")]
use crate::facade::migration::import_migrated_vertex_with_index;
use crate::facade::{FederationRouting, GraphMetadata, GraphStore};
use crate::gql_run::{
    GqlCanisterExecutionMode, run_adhoc_gql_last_read_row_count,
    run_prepared_gql_last_read_row_count,
};
use crate::index::ic::IcPropertyIndexClient;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::router::verify_shard_attachment;
use gleaph_auth::Role;
use gleaph_gql::Value;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_graph_kernel::federation::ExportedVertex;
use ic_stable_lara::VertexId;

use super::types::{GrantRoleArgs, GraphInitArgs};

pub fn init(args: GraphInitArgs) {
    bootstrap_canister_auth(args.issuing_principal, &args.initial_admins);

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

pub async fn gql_query(query: String, params: Vec<u8>) -> Result<u64, String> {
    let p = msg_caller();
    let role = caller_role(&p);
    let pmap = decode_gql_param_map(params)?;
    #[cfg(target_family = "wasm")]
    let index_holder = wasm_index_client_holder();
    #[cfg(target_family = "wasm")]
    let ix = index_holder.as_ref().map(|c| c as &dyn PropertyIndexLookup);
    #[cfg(not(target_family = "wasm"))]
    let ix: Option<&dyn PropertyIndexLookup> = None;

    let row_count = run_adhoc_gql_last_read_row_count(
        GraphStore::new(),
        &query,
        &pmap,
        role,
        ix,
        GqlCanisterExecutionMode::CompositeQuery,
        GqlExecutionContext { caller: Some(p) },
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(row_count as u64)
}

pub async fn gql_execute(query: String, params: Vec<u8>) -> Result<u64, String> {
    let p = msg_caller();
    let role = caller_role(&p);
    let pmap = decode_gql_param_map(params)?;
    #[cfg(target_family = "wasm")]
    let index_holder = wasm_index_client_holder();
    #[cfg(target_family = "wasm")]
    let ix = index_holder.as_ref().map(|c| c as &dyn PropertyIndexLookup);
    #[cfg(not(target_family = "wasm"))]
    let ix: Option<&dyn PropertyIndexLookup> = None;

    let row_count = run_adhoc_gql_last_read_row_count(
        GraphStore::new(),
        &query,
        &pmap,
        role,
        ix,
        GqlCanisterExecutionMode::Update,
        GqlExecutionContext { caller: Some(p) },
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(row_count as u64)
}

pub fn prepared_register(name: String, query: String) -> Result<(), String> {
    let store = GraphStore::new();
    store
        .prepared_query_register(name, &query)
        .map_err(|e| e.to_string())
}

pub fn prepared_drop(name: String) -> Result<(), String> {
    GraphStore::new().prepared_query_drop(&name);
    Ok(())
}

pub async fn prepared_execute_query(name: String, params: Vec<u8>) -> Result<u64, String> {
    let p = msg_caller();
    let store = GraphStore::new();
    let record = store
        .prepared_query_get(&name)
        .ok_or_else(|| format!("unknown prepared query {name:?}"))?;
    let pmap = decode_gql_param_map(params)?;
    #[cfg(target_family = "wasm")]
    let index_holder = wasm_index_client_holder();
    #[cfg(target_family = "wasm")]
    let ix = index_holder.as_ref().map(|c| c as &dyn PropertyIndexLookup);
    #[cfg(not(target_family = "wasm"))]
    let ix: Option<&dyn PropertyIndexLookup> = None;

    let row_count = run_prepared_gql_last_read_row_count(
        store,
        &record,
        &pmap,
        ix,
        GqlCanisterExecutionMode::CompositeQuery,
        GqlExecutionContext { caller: Some(p) },
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(row_count as u64)
}

pub async fn prepared_execute_update(name: String, params: Vec<u8>) -> Result<u64, String> {
    let p = msg_caller();
    let store = GraphStore::new();
    let record = store
        .prepared_query_get(&name)
        .ok_or_else(|| format!("unknown prepared query {name:?}"))?;
    let pmap = decode_gql_param_map(params)?;
    #[cfg(target_family = "wasm")]
    let index_holder = wasm_index_client_holder();
    #[cfg(target_family = "wasm")]
    let ix = index_holder.as_ref().map(|c| c as &dyn PropertyIndexLookup);
    #[cfg(not(target_family = "wasm"))]
    let ix: Option<&dyn PropertyIndexLookup> = None;

    let row_count = run_prepared_gql_last_read_row_count(
        store,
        &record,
        &pmap,
        ix,
        GqlCanisterExecutionMode::Update,
        GqlExecutionContext { caller: Some(p) },
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(row_count as u64)
}

pub fn admin_grant_role(args: GrantRoleArgs) -> Result<(), String> {
    let role = Role::from_str(&args.role).map_err(|e| e.to_string())?;
    admin_upsert_principal(&msg_caller(), args.target, role, args.manager_caps)?;
    Ok(())
}

pub fn whoami() -> Principal {
    msg_caller()
}

pub fn my_role() -> Result<String, String> {
    let p = msg_caller();
    Ok(caller_role(&p).to_string())
}

pub fn export_vertex_for_migration(vertex_id: u32) -> Result<ExportedVertex, String> {
    let store = GraphStore::new();
    export_local_vertex_for_migration(&store, VertexId::from(vertex_id)).map_err(|e| e.to_string())
}

pub async fn import_migrated_vertex_canister(bundle: ExportedVertex) -> Result<u32, String> {
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

pub fn tombstone_migrated_vertex_canister(vertex_id: u32) -> Result<(), String> {
    let store = GraphStore::new();
    tombstone_migrated_vertex(&store, VertexId::from(vertex_id)).map_err(|e| e.to_string())
}
