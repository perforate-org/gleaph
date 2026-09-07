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

/// Pure authorization decision for the relay guard (plan 0335 §5-2): accepts the stored
/// controller (Router) OR the provision relay caller. Fail-closed: anonymous, an unset
/// relay caller, or a third principal all reject. Extracted as a pure function so the
/// authorization matrix is unit-testable natively (the wasm guard reads the caller and
/// the two stored principals, then delegates here).
#[cfg_attr(not(any(target_family = "wasm", test)), allow(dead_code))]
pub fn authorize_relay(
    caller: candid::Principal,
    controller: candid::Principal,
    relay_caller: candid::Principal,
) -> Result<(), String> {
    if caller == candid::Principal::anonymous() {
        return Err(
            "anonymous caller is not the text index controller or relay caller".to_string(),
        );
    }
    if caller == controller || caller == relay_caller {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is neither the text index controller {controller} nor the relay caller {relay_caller}"
        ))
    }
}

/// Relay guard for the two dictionary-relay endpoints (`admin_upload_dict_chunk`,
/// `admin_finalize_dict_upload`): accepts the stored controller (Router) OR the provision
/// relay caller (plan 0335 §5-2). The provision principal is scoped to these two
/// endpoints only — every other `admin_*` endpoint keeps the plain `guard_controller`.
/// Fail-closed: anonymous, an unset relay caller, or a third principal all reject.
#[cfg(target_family = "wasm")]
pub fn guard_controller_or_relay_caller() -> Result<(), String> {
    use ic_cdk::api::msg_caller;

    let (controller, relay_caller) =
        crate::state::with_stores(|stores| (stores.controller(), stores.dict_relay_caller()));
    authorize_relay(msg_caller(), controller, relay_caller)
}

/// Native unit tests call handlers directly without canister caller context.
#[cfg(not(target_family = "wasm"))]
pub fn guard_controller_or_relay_caller() -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use candid::Principal;

    use super::authorize_relay;

    fn p(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    #[test]
    fn relay_authorization_matrix() {
        let router = p(1);
        let provision = p(2);
        let third = p(3);

        // Router (stored controller) allowed.
        assert!(authorize_relay(router, router, provision).is_ok());
        // Provision (relay caller) allowed.
        assert!(authorize_relay(provision, router, provision).is_ok());
        // Third principal rejected.
        assert!(authorize_relay(third, router, provision).is_err());
        // Anonymous rejected even when it matches neither.
        assert!(authorize_relay(Principal::anonymous(), router, provision).is_err());
        // Anonymous relay caller (unset) → only the controller is allowed.
        assert!(authorize_relay(router, router, Principal::anonymous()).is_ok());
        assert!(authorize_relay(provision, router, Principal::anonymous()).is_err());
        // Anonymous controller (unset) → only the relay caller is allowed.
        assert!(authorize_relay(provision, Principal::anonymous(), provision).is_ok());
        assert!(authorize_relay(router, Principal::anonymous(), provision).is_err());
    }
}
