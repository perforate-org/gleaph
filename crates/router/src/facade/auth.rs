//! Stable RBAC for router user-facing GQL, prepared-query, and admin APIs.

use candid::Principal;
use gleaph_auth::{AuthRecord, Role};
use std::str::FromStr;

use crate::state::RouterError;

use super::stable::ROUTER_AUTH_STATE;

pub fn bootstrap_canister_auth(issuing_principal: Principal, initial_admins: &[Principal]) {
    ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
        auth.bootstrap_admins(issuing_principal, initial_admins);
    });
}

/// Grant [`Role::Admin`] to `principal` (tests and local bootstrap).
pub fn grant_admin(principal: Principal) {
    ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
        auth.upsert_record(
            principal,
            AuthRecord {
                role: Role::Admin as u8,
                manager_caps: 0,
            },
        );
    });
}

pub fn grant_admins(principals: &[Principal]) {
    for principal in principals {
        grant_admin(*principal);
    }
}

pub fn is_admin(principal: &Principal) -> bool {
    caller_role(principal).satisfies_at_least(Role::Admin)
}

pub fn require_admin(principal: &Principal) -> Result<(), RouterError> {
    if is_admin(principal) {
        Ok(())
    } else {
        Err(RouterError::NotAuthorized)
    }
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
