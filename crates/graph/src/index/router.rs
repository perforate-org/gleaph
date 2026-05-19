//! Inter-canister client for `gleaph-router` (Wasm init verification).

use candid::Principal;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};

#[derive(Clone, Debug)]
pub enum RouterInitError {
    Call(String),
    Rejected(String),
}

impl std::fmt::Display for RouterInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Call(msg) => write!(f, "router resolve_shard failed: {msg}"),
            Self::Rejected(msg) => write!(f, "router rejected shard attachment: {msg}"),
        }
    }
}

impl std::error::Error for RouterInitError {}

/// Verifies this graph shard is registered on the router and returns routing metadata.
#[cfg(target_family = "wasm")]
pub fn verify_shard_attachment(
    router_canister: Principal,
    shard_id: ShardId,
    expected_graph_name: Option<&str>,
) -> Result<ShardRegistryEntry, RouterInitError> {
    use ic_cdk::api::canister_self;
    use ic_cdk::call::Call;

    let entry: Result<ShardRegistryEntry, RouterError> =
        Call::unbounded_wait(router_canister, "resolve_shard")
            .with_arg(&(shard_id,))
            .wait()
            .map_err(|e| RouterInitError::Call(format!("{e:?}")))?
            .candid()
            .map_err(|e| RouterInitError::Call(format!("candid decode: {e}")))?;

    let entry = entry.map_err(|e| RouterInitError::Rejected(format!("{e:?}")))?;

    let self_id = canister_self();
    if entry.graph_canister != self_id {
        return Err(RouterInitError::Rejected(format!(
            "shard {shard_id} is registered to a different graph canister"
        )));
    }

    if let Some(expected) = expected_graph_name {
        if entry.logical_graph_name != expected {
            return Err(RouterInitError::Rejected(format!(
                "logical_graph_name mismatch: expected `{expected}`, got `{}`",
                entry.logical_graph_name
            )));
        }
    }

    Ok(entry)
}

#[cfg(not(target_family = "wasm"))]
pub fn verify_shard_attachment(
    _router_canister: Principal,
    _shard_id: ShardId,
    _expected_graph_name: Option<&str>,
) -> Result<ShardRegistryEntry, RouterInitError> {
    Err(RouterInitError::Call(
        "router verification is only available on wasm".into(),
    ))
}
