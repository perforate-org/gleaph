#![cfg_attr(test, feature(f128))]

pub mod auth;
#[cfg(feature = "canbench")]
mod bench;
pub mod facade;
pub mod gql_run;
pub mod plan;
mod stable;

mod canister;

pub use facade::GraphStore;

// --- Canister surface (ic-cdk macros stay here; logic lives in `canister::`) ---

use candid::Principal;
use gleaph_gql_ic::IcWireValue;
use ic_cdk_macros::{init, query, update};

use crate::canister::{
    GrantRoleArgs, GraphInitArgs,
    guards::{guard_admin, guard_prepare_register, guard_read},
};

#[init]
fn canister_init(args: GraphInitArgs) {
    canister::handlers::init(args);
}

#[update(guard = "guard_read")]
fn gql_execute(query: String, params: Vec<(String, IcWireValue)>) -> Result<u64, String> {
    canister::handlers::gql_execute(query, params)
}

#[update(guard = "guard_prepare_register")]
fn prepared_register(name: String, query: String) -> Result<(), String> {
    canister::handlers::prepared_register(name, query)
}

#[update(guard = "guard_prepare_register")]
fn prepared_drop(name: String) -> Result<(), String> {
    canister::handlers::prepared_drop(name)
}

#[update]
fn prepared_execute(name: String, params: Vec<(String, IcWireValue)>) -> Result<u64, String> {
    canister::handlers::prepared_execute(name, params)
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
