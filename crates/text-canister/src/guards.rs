//! Caller guards for text-canister entrypoints.

/// Native unit tests call handlers directly without canister caller context.
#[cfg(not(target_family = "wasm"))]
pub fn guard_controller() -> Result<(), String> {
    Ok(())
}

/// Production admin endpoints accept the controller principal configured at install only;
/// an unset controller stores the anonymous sentinel and denies everyone.
#[cfg(target_family = "wasm")]
pub fn guard_controller() -> Result<(), String> {
    use candid::Principal;
    use ic_cdk::api::msg_caller;

    let caller = msg_caller();
    let controller = crate::state::with_stores(|stores| stores.controller());
    if caller == Principal::anonymous() {
        return Err("anonymous caller is not the text index controller".to_string());
    }
    if caller == controller {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is not the text index controller {controller}"
        ))
    }
}
