//! Stable RBAC for router user-facing GQL and prepared-query APIs.

use candid::Principal;
use gleaph_auth::{AuthRecord, Role};
use std::str::FromStr;

use super::stable::ROUTER_AUTH_STATE;

pub fn bootstrap_canister_auth(issuing_principal: Principal, initial_admins: &[Principal]) {
    ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
        auth.bootstrap_admins(issuing_principal, initial_admins);
    });
}

pub fn caller_role(principal: &Principal) -> Role {
    ROUTER_AUTH_STATE.with_borrow(|auth| auth.effective_role(principal))
}

pub fn require_at_least(principal: &Principal, min: Role) -> Result<(), String> {
    ROUTER_AUTH_STATE.with_borrow(|auth| auth.require_at_least(principal, min))
}

pub fn can_prepare_register(principal: &Principal) -> bool {
    ROUTER_AUTH_STATE.with_borrow(|auth| auth.can_prepare_register(principal))
}

pub fn admin_upsert_principal(
    caller: &Principal,
    target: Principal,
    role: Role,
    manager_caps: u64,
) -> Result<(), String> {
    require_at_least(caller, Role::Admin)?;
    ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
        auth.upsert_record(
            target,
            AuthRecord {
                role: role as u8,
                manager_caps,
            },
        );
    });
    Ok(())
}

pub fn parse_role(s: &str) -> Result<Role, String> {
    Role::from_str(s)
}
