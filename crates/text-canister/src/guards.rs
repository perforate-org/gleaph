//! Caller guards for text-canister entrypoints.

#![deny(clippy::wildcard_enum_match_arm, clippy::let_underscore_must_use)]

/// Native unit tests call handlers directly without canister caller context.
#[cfg(not(target_family = "wasm"))]
pub fn guard_controller() -> Result<(), String> {
    Ok(())
}

/// Production admin endpoints accept the controller principal configured at install only;
/// an unset controller stores the anonymous sentinel and denies everyone.
#[cfg(target_family = "wasm")]
pub fn guard_controller() -> Result<(), String> {
    use ic_cdk::api::msg_caller;

    let controller = crate::state::with_stores(|stores| stores.controller());
    authorize_controller(msg_caller(), controller)
}

/// Pure controller decision shared by every controller-guarded entrypoint.
///
/// An unset controller is stored as the anonymous sentinel, so comparing first would let an
/// anonymous caller through; reject the caller explicitly before the equality check.
pub fn authorize_controller(
    caller: candid::Principal,
    controller: candid::Principal,
) -> Result<(), String> {
    if caller == candid::Principal::anonymous() {
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

/// Pure authorization decision for the document-write endpoints (`ingest_text`,
/// `delete_docs`): accepts the stored controller (Router) or an attached Graph shard writer
/// (text attach handshake). Fail-closed: anonymous and any other principal reject.
#[cfg_attr(not(any(target_family = "wasm", test)), allow(dead_code))]
pub fn authorize_writer(
    caller: candid::Principal,
    controller: candid::Principal,
    attached_shard: bool,
) -> Result<(), String> {
    if caller == candid::Principal::anonymous() {
        return Err("anonymous caller is not a text index writer".to_string());
    }
    if caller == controller || attached_shard {
        Ok(())
    } else {
        Err(format!(
            "caller {caller} is neither the text index controller {controller} nor an attached graph shard"
        ))
    }
}

/// Writer guard for `ingest_text` / `delete_docs`: the stored controller (Router) or an
/// attached Graph shard. Graph shards deliver their pending document log directly.
#[cfg(target_family = "wasm")]
pub fn guard_controller_or_writer_shard() -> Result<(), String> {
    use ic_cdk::api::msg_caller;

    let caller = msg_caller();
    let (controller, attached) =
        crate::state::with_stores(|stores| (stores.controller(), stores.is_writer_shard(&caller)));
    authorize_writer(caller, controller, attached)
}

/// Native unit tests call handlers directly without canister caller context.
#[cfg(not(target_family = "wasm"))]
pub fn guard_controller_or_writer_shard() -> Result<(), String> {
    Ok(())
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

    use super::{authorize_controller, authorize_relay, authorize_writer};

    fn p(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    #[test]
    fn controller_authorization_matrix() {
        let router = p(1);
        let third = p(3);

        assert!(authorize_controller(router, router).is_ok());
        assert!(authorize_controller(third, router).is_err());
        // Anonymous is rejected even when the stored controller is the unset sentinel.
        assert!(authorize_controller(Principal::anonymous(), Principal::anonymous()).is_err());
        assert!(authorize_controller(router, Principal::anonymous()).is_err());
    }

    #[test]
    fn writer_authorization_matrix() {
        let router = p(1);
        let shard = p(4);
        let third = p(3);

        // Router (stored controller) allowed with or without an attached shard.
        assert!(authorize_writer(router, router, false).is_ok());
        // An attached Graph shard is allowed.
        assert!(authorize_writer(shard, router, true).is_ok());
        // A principal that is neither is rejected.
        assert!(authorize_writer(third, router, false).is_err());
        // Anonymous is rejected even if the lookup claimed it attached or the controller is unset.
        assert!(authorize_writer(Principal::anonymous(), router, true).is_err());
        assert!(authorize_writer(Principal::anonymous(), Principal::anonymous(), false).is_err());
        // Unset controller: only attached shards write.
        assert!(authorize_writer(router, Principal::anonymous(), false).is_err());
        assert!(authorize_writer(shard, Principal::anonymous(), true).is_ok());
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
