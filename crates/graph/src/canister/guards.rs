//! RBAC guards for canister entrypoints.

use candid::Principal;
use ic_cdk::api::msg_caller;

use crate::auth::{can_prepare_register, require_at_least};
use gleaph_auth::Role;

pub fn guard_read() -> Result<(), String> {
    require_at_least(&msg_caller(), Role::Read)
}

pub fn guard_prepare_register() -> Result<(), String> {
    let p: Principal = msg_caller();
    if can_prepare_register(&p) {
        Ok(())
    } else {
        Err("Admin role or Manager with PREPARE_REGISTER is required".into())
    }
}

pub fn guard_admin() -> Result<(), String> {
    require_at_least(&msg_caller(), Role::Admin)
}
