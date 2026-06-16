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
    use ic_cdk::api::msg_caller;

    let caller = msg_caller();
    let router = INDEX_ROUTER.with_borrow(|cell| *cell.get());
    if caller == router {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is not the configured router canister {router}"
        ))
    }
}

/// Read access for shard-scoped endpoints: router or the shard's owning graph canister.
#[cfg(not(target_family = "wasm"))]
pub fn guard_router_or_shard_canister(_shard_id: ShardId) -> Result<(), String> {
    Ok(())
}

/// Read access for shard-scoped endpoints: router or the shard's owning graph canister.
#[cfg(target_family = "wasm")]
pub fn guard_router_or_shard_canister(shard_id: ShardId) -> Result<(), String> {
    use crate::facade::stable::{INDEX_ROUTER, INDEX_SHARD_CANISTER_CATALOG};
    use ic_cdk::api::msg_caller;

    let caller = msg_caller();
    let router = INDEX_ROUTER.with_borrow(|cell| *cell.get());
    if caller == router {
        return Ok(());
    }
    let caller_shard =
        INDEX_SHARD_CANISTER_CATALOG.with_borrow(|catalog| catalog.shard_for_canister(caller));
    match caller_shard {
        Some(s) if s == shard_id => Ok(()),
        Some(s) => Err(format!(
            "caller {caller} is attached to shard {} (requested shard {})",
            s.raw(),
            shard_id.raw()
        )),
        None => Err(format!(
            "caller {caller} is not router {router} and is not attached to any shard canister"
        )),
    }
}
