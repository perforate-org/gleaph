//! Stable-graph authorization (roles and manager capabilities).

use candid::Principal;
use gleaph_auth::{AuthRecord, ManagerCapability, Role};

use crate::stable::AUTH_STATE;

/// Bootstrap admins after canister `init` (idempotent merge of issuing principal + list).
pub fn bootstrap_canister_auth(issuing_principal: Principal, initial_admins: &[Principal]) {
    AUTH_STATE.with_borrow_mut(|auth| {
        auth.bootstrap_admins(issuing_principal, initial_admins);
    });
}

pub fn require_at_least(principal: &Principal, min: Role) -> Result<(), String> {
    AUTH_STATE.with_borrow(|auth| auth.require_at_least(principal, min))
}

/// Effective role: [`Role::Executor`] when the principal has no row in stable auth storage.
pub fn caller_role(principal: &Principal) -> Role {
    AUTH_STATE.with_borrow(|auth| auth.effective_role(principal))
}

pub fn can_prepare_register(principal: &Principal) -> bool {
    AUTH_STATE.with_borrow(|auth| auth.can_prepare_register(principal))
}

pub fn admin_upsert_principal(
    caller: &Principal,
    target: Principal,
    role: Role,
    manager_caps: u64,
) -> Result<(), String> {
    require_at_least(caller, Role::Admin)?;
    AUTH_STATE.with_borrow_mut(|auth| {
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

pub fn manager_has_capability(principal: &Principal, cap: ManagerCapability) -> bool {
    AUTH_STATE.with_borrow(|auth| auth.has_manager_capability(principal, cap))
}
