//! Stable RBAC for router user-facing GQL, prepared-query, and admin APIs.

use candid::Principal;
use gleaph_auth::{AuthRecord, AuthWriteError, Role};
use std::str::FromStr;

use crate::state::RouterError;

use super::stable::ROUTER_AUTH_STATE;

/// Pre-mutation preflight for canister init: validates bootstrap principals via the
/// auth-owned authoritative path before any stable state is cleared or written.
pub fn validate_bootstrap_principals(
    issuing_principal: Principal,
    initial_admins: &[Principal],
) -> Result<(), AuthWriteError> {
    gleaph_auth::validate_bootstrap_principals(issuing_principal, initial_admins)
}

/// Bootstrap installer/initial admins. Rejects the anonymous principal all-or-nothing
/// (see [`gleaph_auth::AuthState::bootstrap_admins`]).
pub fn bootstrap_canister_auth(
    issuing_principal: Principal,
    initial_admins: &[Principal],
) -> Result<(), AuthWriteError> {
    ROUTER_AUTH_STATE
        .with_borrow_mut(|auth| auth.bootstrap_admins(issuing_principal, initial_admins))
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
        )
        .expect("grant_admin requires a non-anonymous principal");
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
        )
        .map_err(|e| e.to_string())
    })
}

pub fn parse_role(s: &str) -> Result<Role, String> {
    Role::from_str(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_upsert_principal_rejects_anonymous_target() {
        let admin = Principal::from_slice(&[1; 29]);
        grant_admin(admin);
        let err = admin_upsert_principal(&admin, Principal::anonymous(), Role::Admin, 0)
            .expect_err("anonymous target must be rejected");
        assert!(
            err.contains("anonymous"),
            "error should name the anonymous principal, got: {err}"
        );
        // The anonymous principal must remain the default Executor (no persisted elevation).
        assert_eq!(caller_role(&Principal::anonymous()), Role::Executor);
    }

    #[test]
    fn bootstrap_canister_auth_rejects_anonymous_issuer_without_persisting() {
        // A distinctive valid initial admin supplied alongside the anonymous issuer.
        let valid = Principal::from_slice(&[0xA1; 29]);
        let err = bootstrap_canister_auth(Principal::anonymous(), &[valid])
            .expect_err("anonymous issuer must be rejected");
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert_eq!(caller_role(&Principal::anonymous()), Role::Executor);
        // The valid initial admin from the rejected request was not partially inserted/elevated.
        assert_eq!(caller_role(&valid), Role::Executor);
    }

    #[test]
    fn bootstrap_canister_auth_rejects_anonymous_initial_admin_without_persisting() {
        let issuer = Principal::from_slice(&[0xA2; 29]);
        let valid = Principal::from_slice(&[0xA3; 29]);
        let err = bootstrap_canister_auth(issuer, &[valid, Principal::anonymous()])
            .expect_err("anonymous initial admin must be rejected");
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        // Neither the issuer nor the valid initial admin from the same request was elevated.
        assert_eq!(caller_role(&issuer), Role::Executor);
        assert_eq!(caller_role(&valid), Role::Executor);
    }

    #[test]
    fn init_preflight_rejects_invalid_bootstrap_before_elevating_valid_admin() {
        // Mirrors the order in `canister::init`: the auth-owned preflight runs before any Router
        // stable state is cleared/written, so an anonymous issuer is rejected even when a valid
        // initial admin is supplied — without relying on IC trap rollback.
        let valid = Principal::from_slice(&[0xA4; 29]);
        let err = validate_bootstrap_principals(Principal::anonymous(), &[valid])
            .expect_err("preflight must reject anonymous issuer");
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert_eq!(caller_role(&valid), Role::Executor);
    }
}
