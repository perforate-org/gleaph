//! Caller guards for graph canister entrypoints (control plane only — no end-user RBAC).

/// Native unit tests call handlers directly without a router caller principal.
#[cfg(not(target_family = "wasm"))]
pub fn guard_router_canister() -> Result<(), String> {
    Ok(())
}

/// Production graph shards accept plan execution only from the configured router.
#[cfg(target_family = "wasm")]
pub fn guard_router_canister() -> Result<(), String> {
    use crate::facade::GraphStore;
    use candid::Principal;
    use ic_cdk::api::msg_caller;

    let caller = msg_caller();
    // Defense in depth: never trust the anonymous principal, even if a corrupt routing record
    // somehow named it as the router.
    if caller == Principal::anonymous() {
        return Err("anonymous caller is not the configured router canister".to_string());
    }
    let routing = GraphStore::new()
        .federation_routing()
        .ok_or("federation routing not configured")?;
    if caller == routing.router_canister {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is not the configured router canister {}",
            routing.router_canister
        ))
    }
}

/// Migration and other control-plane admin hooks (installer / router operations).
pub fn guard_control_plane_admin() -> Result<(), String> {
    guard_router_canister()
}
