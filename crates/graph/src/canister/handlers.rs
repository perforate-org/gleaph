//! Request bodies for canister methods (called from `lib.rs` ic-cdk entrypoints).

use std::collections::BTreeMap;
use std::str::FromStr;

use candid::Principal;
use ic_cdk::api::msg_caller;

use crate::auth::{admin_upsert_principal, bootstrap_canister_auth, caller_role};
use crate::facade::GraphStore;
use crate::gql_run::{run_adhoc_gql, run_prepared_gql};
use gleaph_auth::Role;
use gleaph_gql::Value;
use gleaph_gql_ic::IcWireValue;

use super::types::{GrantRoleArgs, GraphInitArgs};

pub fn init(args: GraphInitArgs) {
    bootstrap_canister_auth(args.issuing_principal, &args.initial_admins);
    let _ = args.logical_graph_name;
}

fn wire_to_values(params: Vec<(String, IcWireValue)>) -> Result<BTreeMap<String, Value>, String> {
    let mut m = BTreeMap::new();
    for (k, w) in params {
        let v = w.try_into_value().map_err(|e| e.to_string())?;
        m.insert(k, v);
    }
    Ok(m)
}

pub fn gql_execute(query: String, params: Vec<(String, IcWireValue)>) -> Result<u64, String> {
    let p = msg_caller();
    let role = caller_role(&p);
    let pmap = wire_to_values(params)?;
    let out = run_adhoc_gql(GraphStore::new(), &query, &pmap, role).map_err(|e| e.to_string())?;
    Ok(out.rows.len() as u64)
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

pub fn prepared_execute(name: String, params: Vec<(String, IcWireValue)>) -> Result<u64, String> {
    let store = GraphStore::new();
    let program = store
        .prepared_query_get(&name)
        .ok_or_else(|| format!("unknown prepared query {name:?}"))?;
    let pmap = wire_to_values(params)?;
    let out = run_prepared_gql(store, &program, &pmap).map_err(|e| e.to_string())?;
    Ok(out.rows.len() as u64)
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
