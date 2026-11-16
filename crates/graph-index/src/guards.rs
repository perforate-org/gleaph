//! Caller guards for graph-index canister entrypoints.

use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;

/// Pure router guard: anonymous callers and callers other than the configured router fail.
pub(crate) fn router_guard(caller: Principal, router: Principal) -> Result<(), String> {
    if caller == Principal::anonymous() {
        return Err("anonymous caller is not the configured router canister".to_string());
    }
    if caller == router {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is not the configured router canister {router}"
        ))
    }
}

/// Pure shard guard: the caller must be the graph canister registered for `shard_id`.
pub(crate) fn shard_guard(
    caller: Principal,
    shard_id: ShardId,
    registered: Option<Principal>,
) -> Result<(), String> {
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

/// Pure read guard: the router or any shard attached to this graph-dedicated index.
pub(crate) fn router_or_attached_guard(
    caller: Principal,
    router: Principal,
    attached: bool,
) -> Result<(), String> {
    if caller == Principal::anonymous() {
        return Err("anonymous caller is not the configured router canister".to_string());
    }
    if caller == router || attached {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is neither the configured router canister {router} nor an attached shard"
        ))
    }
}

/// Native unit tests call handlers directly without canister caller context.
#[cfg(not(target_family = "wasm"))]
pub fn guard_router_canister() -> Result<(), String> {
    Ok(())
}

/// Production graph-index accepts guarded reads from the configured router only.
#[cfg(target_family = "wasm")]
pub fn guard_router_canister() -> Result<(), String> {
    use crate::facade::stable::INDEX_ROUTER;

    let caller = ic_cdk::api::msg_caller();
    let router = INDEX_ROUTER.with_borrow(|cell| *cell.get());
    router_guard(caller, router)
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

    let caller = ic_cdk::api::msg_caller();
    let registered =
        INDEX_SHARD_CANISTER_CATALOG.with_borrow(|catalog| catalog.shard_canister(shard_id));
    shard_guard(caller, shard_id, registered)
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

    let caller = ic_cdk::api::msg_caller();
    let router = INDEX_ROUTER.with_borrow(|cell| *cell.get());
    let attached = INDEX_SHARD_CANISTER_CATALOG
        .with_borrow(|catalog| catalog.is_attached_shard_canister(caller));
    router_or_attached_guard(caller, router, attached)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    #[test]
    fn router_guard_rejects_anonymous_and_other_callers() {
        let router = principal(1);
        assert!(router_guard(router, router).is_ok());
        assert!(router_guard(principal(2), router).is_err());
        assert!(router_guard(Principal::anonymous(), router).is_err());
    }

    #[test]
    fn shard_guard_requires_the_registered_canister() {
        let shard = ShardId::new(3);
        let owner = principal(4);
        assert!(shard_guard(owner, shard, Some(owner)).is_ok());
        assert!(shard_guard(principal(5), shard, Some(owner)).is_err());
        assert!(shard_guard(owner, shard, None).is_err());
    }

    #[test]
    fn read_guard_accepts_router_or_attached_and_rejects_anonymous() {
        let router = principal(1);
        assert!(router_or_attached_guard(router, router, false).is_ok());
        assert!(router_or_attached_guard(principal(2), router, true).is_ok());
        assert!(router_or_attached_guard(principal(2), router, false).is_err());
        assert!(router_or_attached_guard(Principal::anonymous(), router, true).is_err());
    }
}
