#![cfg_attr(test, feature(f128))]

#[cfg(feature = "canbench")]
mod bench;
pub mod facade;
use facade::auth;
pub mod gql_execution_context;
pub mod gql_run;
mod index;
pub mod plan;

mod canister;

use gql_execution_context::GqlExecutionContext;

// --- Canister surface (ic-cdk macros stay here; logic lives in `canister::`) ---

use candid::Principal;
use ic_cdk_macros::{init, query, update};

use crate::canister::{
    GrantRoleArgs, GraphInitArgs,
    guards::{guard_admin, guard_prepare_register, guard_read, guard_write},
};

#[init]
fn init(args: GraphInitArgs) {
    canister::handlers::init(args);
}

#[query(composite = true, guard = "guard_read")]
async fn gql_query(query: String, params: Vec<u8>) -> Result<u64, String> {
    canister::handlers::gql_query(query, params).await
}

#[update(guard = "guard_write")]
async fn gql_execute(query: String, params: Vec<u8>) -> Result<u64, String> {
    canister::handlers::gql_execute(query, params).await
}

#[update(guard = "guard_prepare_register")]
fn prepared_register(name: String, query: String) -> Result<(), String> {
    canister::handlers::prepared_register(name, query)
}

#[update(guard = "guard_prepare_register")]
fn prepared_drop(name: String) -> Result<(), String> {
    canister::handlers::prepared_drop(name)
}

#[query(composite = true)]
async fn prepared_execute_query(name: String, params: Vec<u8>) -> Result<u64, String> {
    canister::handlers::prepared_execute_query(name, params).await
}

#[update]
async fn prepared_execute_update(name: String, params: Vec<u8>) -> Result<u64, String> {
    canister::handlers::prepared_execute_update(name, params).await
}

#[update(guard = "guard_admin")]
fn admin_grant_role(args: GrantRoleArgs) -> Result<(), String> {
    canister::handlers::admin_grant_role(args)
}

#[query]
fn whoami() -> Principal {
    canister::handlers::whoami()
}

#[query(guard = "guard_read")]
fn my_role() -> Result<String, String> {
    canister::handlers::my_role()
}

#[update(guard = "guard_admin")]
fn migration_begin(
    args: gleaph_graph_kernel::federation::BeginVertexMigrationArgs,
) -> Result<(), String> {
    canister::handlers::migration_begin(args)
}

#[query(guard = "guard_read")]
fn federated_expand(
    args: gleaph_graph_kernel::federation::FederatedExpandArgs,
) -> Result<Vec<gleaph_graph_kernel::federation::FederatedExpandNeighbor>, String> {
    canister::handlers::federated_expand(args)
}

#[query(guard = "guard_admin")]
fn migration_export(
    local_vertex_id: u32,
) -> Result<gleaph_graph_kernel::federation::ExportedVertex, String> {
    canister::handlers::migration_export(local_vertex_id)
}

#[update(guard = "guard_admin")]
async fn migration_import(
    bundle: gleaph_graph_kernel::federation::ExportedVertex,
) -> Result<u32, String> {
    canister::handlers::migration_import(bundle).await
}

#[update(guard = "guard_admin")]
async fn migration_tombstone(local_vertex_id: u32) -> Result<(), String> {
    canister::handlers::migration_tombstone(local_vertex_id).await
}

ic_cdk::export_candid!();
