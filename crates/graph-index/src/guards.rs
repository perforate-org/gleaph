//! Caller guards for graph-index canister entrypoints.

use gleaph_graph_kernel::federation::ShardId;

/// Native unit tests call handlers directly without canister caller context.
#[cfg(not(target_family = "wasm"))]
pub fn guard_router_canister() -> Result<(), String> {
    Ok(())
}

/// Production graph-index accepts guarded reads from the configured router only.
#[cfg(target_family = "wasm")]
pub fn guard_router_canister() -> Result<(), String> {
    use crate::facade::stable::INDEX_ROUTER;
    use candid::Principal;
    use ic_cdk::api::msg_caller;

    let caller = msg_caller();
    // Defense in depth: never accept the anonymous principal as the router.
    if caller == Principal::anonymous() {
        return Err("anonymous caller is not the configured router canister".to_string());
    }
    let router = INDEX_ROUTER.with_borrow(|cell| *cell.get());
    if caller == router {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is not the configured router canister {router}"
        ))
    }
}

/// Native unit tests call handlers directly without canister caller context.
#[cfg(not(target_family = "wasm"))]
pub fn guard_shard_canister(_shard_id: ShardId) -> Result<(), String> {
    Ok(())
}

/// Posting sync updates: owning graph shard canister for `shard_id` only.
#[cfg(target_family = "wasm")]
pub fn guard_shard_canister(shard_id: ShardId) -> Result<(), String> {
    use crate::facade::stable::INDEX_SHARD_CANISTER_CATALOG;
    use ic_cdk::api::msg_caller;

    let caller = msg_caller();
    let registered =
        INDEX_SHARD_CANISTER_CATALOG.with_borrow(|catalog| catalog.shard_canister(shard_id));
    let Some(reg) = registered else {
        return Err(format!(
            "shard {} is not attached to any graph canister",
            shard_id.raw()
        ));
    };
    if caller == reg {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is not the graph canister {reg} attached to shard {}",
            shard_id.raw()
        ))
    }
}

/// Native unit tests call handlers directly without canister caller context.
#[cfg(not(target_family = "wasm"))]
pub fn guard_router_or_attached_shard_canister() -> Result<(), String> {
    Ok(())
}

/// Read APIs accept the configured router or any graph shard attached to this index canister.
/// The index canister is graph-dedicated, so every attached shard belongs to the same logical
/// graph and is entitled to read its own postings.
#[cfg(target_family = "wasm")]
pub fn guard_router_or_attached_shard_canister() -> Result<(), String> {
    use crate::facade::stable::{INDEX_ROUTER, INDEX_SHARD_CANISTER_CATALOG};
    use candid::Principal;
    use ic_cdk::api::msg_caller;

    let caller = msg_caller();
    if caller == Principal::anonymous() {
        return Err("anonymous caller is not the configured router canister".to_string());
    }
    let router = INDEX_ROUTER.with_borrow(|cell| *cell.get());
    if caller == router {
        return Ok(());
    }
    let attached = INDEX_SHARD_CANISTER_CATALOG
        .with_borrow(|catalog| catalog.is_attached_shard_canister(caller));
    if attached {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is not the configured router canister {router} or an attached shard canister"
        ))
    }
}
