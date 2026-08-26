//! Caller guards for graph canister entrypoints (control plane only — no end-user RBAC).

/// Native unit tests call handlers directly without a router caller principal.
#[cfg(not(target_family = "wasm"))]
pub fn guard_router_canister() -> Result<(), String> {
    Ok(())
}

/// Canonical export page reads are admitted per-scope inside the handler: every frozen scope
/// names exactly one authorized puller (`authorize_page_pull`), replacing the former
/// single-canister fn guard.

/// Production graph shards accept plan execution only from the configured router.
#[cfg(target_family = "wasm")]
pub fn guard_router_canister() -> Result<(), String> {
    use crate::facade::GraphStore;
    use ic_cdk::api::msg_caller;

    let caller = msg_caller();
    let routing = GraphStore::new()
        .federation_routing()
        .ok_or("federation routing not configured")?;
    authorize_router_caller(caller, routing.router_canister)
}

#[cfg(any(target_family = "wasm", test))]
fn authorize_router_caller(
    caller: candid::Principal,
    configured_router: candid::Principal,
) -> Result<(), String> {
    // Defense in depth: never trust the anonymous principal, even if a corrupt routing record
    // somehow named it as the router.
    if caller == candid::Principal::anonymous() {
        return Err("anonymous caller is not the configured router canister".to_string());
    }
    if caller == configured_router {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is not the configured router canister {}",
            configured_router
        ))
    }
}

/// Migration and other control-plane admin hooks (installer / router operations).
pub fn guard_control_plane_admin() -> Result<(), String> {
    guard_router_canister()
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;

    /// Router-only authorization: every index-export control endpoint is guarded by
    /// `guard_router_canister`, and the shared authorization primitive rejects a non-router
    /// caller while accepting the exact configured router principal.
    #[test]
    fn router_authorization_rejects_non_router_and_anonymous_callers() {
        let router = Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").expect("router id");
        let interloper = Principal::from_text("aaaaa-aa").expect("management id");
        assert!(authorize_router_caller(router, router).is_ok());
        assert_eq!(
            authorize_router_caller(interloper, router),
            Err(format!(
                "caller {interloper} is not the configured router canister {router}"
            ))
        );
        assert_eq!(
            authorize_router_caller(Principal::anonymous(), router),
            Err("anonymous caller is not the configured router canister".to_string())
        );
    }
}
