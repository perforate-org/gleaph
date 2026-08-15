//! Gleaph Account canister — ADR 0068.
//!
//! Owns developer identity/registration and the account↔Router mapping.
//! Does not own graph topology, graph tenancy, or routing catalogs.

#![cfg_attr(not(test), allow(dead_code))]

pub mod canister;
pub mod stable;
pub mod types;

use canister::{
    add_member_with_caller, authorize_router_issuance_with_caller, complete_bootstrap_with_caller,
    create_account_with_caller, create_org_account_with_caller, delete_account_with_caller,
    get_account_with_caller, list_routers_with_caller, register_router_with_caller,
    remove_member_with_caller, resolve_my_accounts_with_caller, resolve_router_with_caller,
    unregister_router_with_caller,
};
use ic_cdk_macros::{init, post_upgrade, query, update};
use types::{Account, AccountError, Role, RouterEntry};

#[init]
fn init() {
    // All account state lives in stable memory; nothing to seed.
}

#[post_upgrade]
fn post_upgrade() {}

#[update]
fn create_account(name: String) -> Result<Account, AccountError> {
    create_account_with_caller(
        ic_cdk::api::msg_caller(),
        name,
        &stable::store::AccountStore::new(),
    )
}

#[update]
fn create_org_account(name: String) -> Result<Account, AccountError> {
    create_org_account_with_caller(
        ic_cdk::api::msg_caller(),
        name,
        ic_time_ns(),
        &stable::store::AccountStore::new(),
    )
}

#[query]
fn get_account(account_id: candid::Principal) -> Result<Account, AccountError> {
    get_account_with_caller(
        ic_cdk::api::msg_caller(),
        &account_id,
        &stable::store::AccountStore::new(),
    )
}

#[update]
fn delete_account(account_id: candid::Principal) -> Result<(), AccountError> {
    delete_account_with_caller(
        ic_cdk::api::msg_caller(),
        &account_id,
        &stable::store::AccountStore::new(),
    )
}

#[update]
fn add_member(
    account_id: candid::Principal,
    principal: candid::Principal,
    role: Role,
) -> Result<(), AccountError> {
    add_member_with_caller(
        ic_cdk::api::msg_caller(),
        &account_id,
        principal,
        role,
        &stable::store::AccountStore::new(),
    )
}

#[update]
fn remove_member(
    account_id: candid::Principal,
    principal: candid::Principal,
) -> Result<(), AccountError> {
    remove_member_with_caller(
        ic_cdk::api::msg_caller(),
        &account_id,
        principal,
        &stable::store::AccountStore::new(),
    )
}

#[query]
fn resolve_my_accounts() -> Vec<candid::Principal> {
    resolve_my_accounts_with_caller(
        ic_cdk::api::msg_caller(),
        &stable::store::AccountStore::new(),
    )
}

#[update]
fn register_router(account_id: candid::Principal, router: RouterEntry) -> Result<(), AccountError> {
    register_router_with_caller(
        ic_cdk::api::msg_caller(),
        &account_id,
        router,
        &stable::store::AccountStore::new(),
    )
}

#[update]
fn unregister_router(account_id: candid::Principal, router_id: String) -> Result<(), AccountError> {
    unregister_router_with_caller(
        ic_cdk::api::msg_caller(),
        &account_id,
        &router_id,
        &stable::store::AccountStore::new(),
    )
}

#[query]
fn list_routers(account_id: candid::Principal) -> Result<Vec<RouterEntry>, AccountError> {
    list_routers_with_caller(
        ic_cdk::api::msg_caller(),
        &account_id,
        &stable::store::AccountStore::new(),
    )
}

#[query]
fn resolve_router(
    account_id: candid::Principal,
    router_id: String,
) -> Result<candid::Principal, AccountError> {
    resolve_router_with_caller(
        ic_cdk::api::msg_caller(),
        &account_id,
        &router_id,
        &stable::store::AccountStore::new(),
    )
}

#[update]
async fn authorize_router_issuance(
    account_id: candid::Principal,
    router_id: String,
    provision_canister: candid::Principal,
) -> Result<gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse, AccountError> {
    authorize_router_issuance_with_caller(
        ic_cdk::api::msg_caller(),
        &account_id,
        &router_id,
        provision_canister,
        &stable::store::AccountStore::new(),
    )
    .await
}

#[update]
async fn complete_bootstrap(
    account_id: candid::Principal,
    provision_canister: candid::Principal,
) -> Result<(), AccountError> {
    complete_bootstrap_with_caller(
        ic_cdk::api::msg_caller(),
        &account_id,
        provision_canister,
        &stable::store::AccountStore::new(),
    )
    .await
}

#[cfg(test)]
pub fn export_service_string() -> String {
    __export_service()
}

ic_cdk::export_candid!();

/// IC NNS timestamp in nanoseconds (0 off-wasm, for deterministic unit tests).
#[allow(dead_code)]
pub(crate) fn ic_time_ns() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::time()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}
