//! Caller guards for graph-vector-index canister entrypoints.

/// Native unit tests call handlers directly without canister caller context.
#[cfg(not(target_family = "wasm"))]
pub fn guard_router_canister() -> Result<(), String> {
    Ok(())
}

/// Production graph-vector-index accepts guarded admin/reads from the configured router only.
#[cfg(target_family = "wasm")]
pub fn guard_router_canister() -> Result<(), String> {
    use crate::facade::stable::VECTOR_INDEX_ROUTER;
    use candid::Principal;
    use ic_cdk::api::msg_caller;

    let caller = msg_caller();
    if caller == Principal::anonymous() {
        return Err("anonymous caller is not the configured router canister".to_string());
    }
    let router = VECTOR_INDEX_ROUTER.with_borrow(|cell| *cell.get());
    if caller == router {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is not the configured router canister {router}"
        ))
    }
}
