//! Inter-canister calls from router to the property index canister.

use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;

#[cfg_attr(
    feature = "pocket-ic-e2e",
    expect(
        dead_code,
        reason = "index sync skipped when pocket-ic-e2e manages shard owners"
    )
)]
#[cfg(target_family = "wasm")]
pub async fn admin_set_shard_owner(
    index_canister: Principal,
    shard_id: ShardId,
    owner_principal: Principal,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(index_canister, "admin_set_shard_owner")
        .with_args(&(shard_id.raw(), owner_principal))
        .await
        .map_err(|e| format!("index admin_set_shard_owner call failed: {e}"))?
        .candid()
        .map_err(|e| format!("index admin_set_shard_owner decode failed: {e}"))?
}

#[cfg_attr(
    feature = "pocket-ic-e2e",
    expect(
        dead_code,
        reason = "index sync skipped when pocket-ic-e2e manages shard owners"
    )
)]
#[cfg(not(target_family = "wasm"))]
pub async fn admin_set_shard_owner(
    _index_canister: Principal,
    _shard_id: ShardId,
    _owner_principal: Principal,
) -> Result<(), String> {
    Ok(())
}

#[cfg_attr(
    feature = "pocket-ic-e2e",
    expect(
        dead_code,
        reason = "index sync skipped when pocket-ic-e2e manages shard owners"
    )
)]
#[cfg(target_family = "wasm")]
pub async fn admin_clear_shard_owner(
    index_canister: Principal,
    shard_id: ShardId,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(index_canister, "admin_clear_shard_owner")
        .with_args(&(shard_id.raw(),))
        .await
        .map_err(|e| format!("index admin_clear_shard_owner call failed: {e}"))?
        .candid()
        .map_err(|e| format!("index admin_clear_shard_owner decode failed: {e}"))?
}

#[cfg_attr(
    feature = "pocket-ic-e2e",
    expect(
        dead_code,
        reason = "index sync skipped when pocket-ic-e2e manages shard owners"
    )
)]
#[cfg(not(target_family = "wasm"))]
pub async fn admin_clear_shard_owner(
    _index_canister: Principal,
    _shard_id: ShardId,
) -> Result<(), String> {
    Ok(())
}
