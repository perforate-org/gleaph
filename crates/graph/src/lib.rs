#![cfg_attr(test, feature(f128))]

#[cfg(feature = "canbench")]
mod bench;
mod facade;
use facade::auth;
mod gql_run;
mod index;
mod plan;

mod canister;

// --- Canister surface (ic-cdk macros stay here; logic lives in `canister::`) ---

use candid::Principal;
use gleaph_gql_ic::IcWireValue;
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
async fn gql_query(query: String, params: Vec<(String, IcWireValue)>) -> Result<u64, String> {
    canister::handlers::gql_query(query, params).await
}

#[update(guard = "guard_write")]
async fn gql_execute(query: String, params: Vec<(String, IcWireValue)>) -> Result<u64, String> {
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
async fn prepared_execute_query(
    name: String,
    params: Vec<(String, IcWireValue)>,
) -> Result<u64, String> {
    canister::handlers::prepared_execute_query(name, params).await
}

#[update]
async fn prepared_execute_update(
    name: String,
    params: Vec<(String, IcWireValue)>,
) -> Result<u64, String> {
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
