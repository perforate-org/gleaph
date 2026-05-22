//! Caller guards for graph canister entrypoints (control plane only — no end-user RBAC).

/// Native unit tests call handlers directly without a router caller principal.
#[cfg(not(target_family = "wasm"))]
pub fn guard_router_canister() -> Result<(), String> {
    Ok(())
}

/// Production graph shards accept plan execution only from the configured router.
#[cfg(target_family = "wasm")]
pub fn guard_router_canister() -> Result<(), String> {
    let caller = msg_caller();
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

/// `federated_expand` is invoked by the router or a sibling graph shard (not end users).
#[cfg(not(target_family = "wasm"))]
pub fn guard_router_or_peer_graph() -> Result<(), String> {
    Ok(())
}

#[cfg(target_family = "wasm")]
pub fn guard_router_or_peer_graph() -> Result<(), String> {
    let caller = msg_caller();
    if guard_router_canister().is_ok() {
        return Ok(());
    }
    if GraphStore::new().is_peer_graph_canister(&caller) {
        return Ok(());
    }
    Err(format!(
        "caller {caller} is not the router or a registered peer graph canister"
    ))
}

/// Migration and other control-plane admin hooks (installer / router operations).
pub fn guard_control_plane_admin() -> Result<(), String> {
    guard_router_canister()
}
