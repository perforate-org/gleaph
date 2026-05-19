//! Inter-canister calls from router to the property index canister.

use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;

#[cfg(target_family = "wasm")]
pub async fn admin_set_shard_owner(
    index_canister: Principal,
    shard_id: ShardId,
    owner_principal: Principal,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    let result: (Result<(), String>,) =
        Call::unbounded_wait(index_canister, "admin_set_shard_owner")
            .with_arg(&(shard_id, owner_principal))
            .await
            .map_err(|e| format!("index admin_set_shard_owner call failed: {e}"))?
            .candid()
            .map_err(|e| format!("index admin_set_shard_owner decode failed: {e}"))?;
    result.0
}

#[cfg(not(target_family = "wasm"))]
pub async fn admin_set_shard_owner(
    _index_canister: Principal,
    _shard_id: ShardId,
    _owner_principal: Principal,
) -> Result<(), String> {
    Ok(())
}

#[cfg(target_family = "wasm")]
pub async fn admin_clear_shard_owner(
    index_canister: Principal,
    shard_id: ShardId,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    let result: (Result<(), String>,) =
        Call::unbounded_wait(index_canister, "admin_clear_shard_owner")
            .with_arg(&(shard_id,))
            .await
            .map_err(|e| format!("index admin_clear_shard_owner call failed: {e}"))?
            .candid()
            .map_err(|e| format!("index admin_clear_shard_owner decode failed: {e}"))?;
    result.0
}

#[cfg(not(target_family = "wasm"))]
pub async fn admin_clear_shard_owner(
    _index_canister: Principal,
    _shard_id: ShardId,
) -> Result<(), String> {
    Ok(())
}
