//! Inter-canister calls from router to the property index canister.

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::IndexPurgeKind;
use gleaph_graph_kernel::federation::ShardId;
#[cfg(target_family = "wasm")]
use gleaph_graph_kernel::federation::{
    IndexPostingPurgeCursor, IndexPostingPurgeStepResult, ShardDetachCursor, ShardDetachStepResult,
};

#[cfg_attr(
    feature = "pocket-ic-e2e",
    expect(
        dead_code,
        reason = "index sync skipped when pocket-ic-e2e manages shard/canister attachments"
    )
)]
#[cfg(target_family = "wasm")]
pub async fn admin_attach_shard_canister(
    index_canister: Principal,
    graph_id: GraphId,
    index_group_size: u32,
    group_index: u32,
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(index_canister, "admin_attach_shard_canister")
        .with_args(&(
            graph_id,
            index_group_size,
            group_index,
            shard_id,
            shard_canister_principal,
        ))
        .await
        .map_err(|e| format!("index admin_attach_shard_canister call failed: {e}"))?
        .candid()
        .map_err(|e| format!("index admin_attach_shard_canister decode failed: {e}"))?
}

#[cfg_attr(
    feature = "pocket-ic-e2e",
    expect(
        dead_code,
        reason = "index sync skipped when pocket-ic-e2e manages shard/canister attachments"
    )
)]
#[cfg(not(target_family = "wasm"))]
pub async fn admin_attach_shard_canister(
    _index_canister: Principal,
    _graph_id: GraphId,
    _index_group_size: u32,
    _group_index: u32,
    _shard_id: ShardId,
    _shard_canister_principal: Principal,
) -> Result<(), String> {
    Ok(())
}

#[cfg_attr(
    feature = "pocket-ic-e2e",
    expect(
        dead_code,
        reason = "index sync skipped when pocket-ic-e2e manages shard/canister attachments"
    )
)]
#[cfg(target_family = "wasm")]
pub async fn admin_detach_shard_canister(
    index_canister: Principal,
    shard_id: ShardId,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    // The index purges shard postings in bounded steps so a single message stays
    // within instruction/stable limits; drive resume cursors until done.
    let mut resume: Option<ShardDetachCursor> = None;
    loop {
        let step: ShardDetachStepResult =
            Call::unbounded_wait(index_canister, "admin_detach_shard_canister")
                .with_args(&(shard_id.raw(), &resume))
                .await
                .map_err(|e| format!("index admin_detach_shard_canister call failed: {e}"))?
                .candid::<Result<ShardDetachStepResult, String>>()
                .map_err(|e| format!("index admin_detach_shard_canister decode failed: {e}"))??;
        match step.next {
            Some(cursor) => resume = Some(cursor),
            None => return Ok(()),
        }
    }
}

#[cfg_attr(
    feature = "pocket-ic-e2e",
    expect(
        dead_code,
        reason = "index sync skipped when pocket-ic-e2e manages shard/canister attachments"
    )
)]
#[cfg(not(target_family = "wasm"))]
pub async fn admin_detach_shard_canister(
    _index_canister: Principal,
    _shard_id: ShardId,
) -> Result<(), String> {
    Ok(())
}

/// Drives a bounded, resumable `DROP INDEX` posting purge (ADR 0023 D6) on one
/// index canister until it reports `done`. For vertex purges `label_id` is
/// ignored by the index.
#[cfg(target_family = "wasm")]
pub async fn admin_purge_property_postings(
    index_canister: Principal,
    kind: IndexPurgeKind,
    property_id: u32,
    label_id: u16,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    let mut resume: Option<IndexPostingPurgeCursor> = None;
    loop {
        let step: IndexPostingPurgeStepResult =
            Call::unbounded_wait(index_canister, "admin_purge_property_postings")
                .with_args(&(kind, property_id, label_id, &resume))
                .await
                .map_err(|e| format!("index admin_purge_property_postings call failed: {e}"))?
                .candid::<Result<IndexPostingPurgeStepResult, String>>()
                .map_err(|e| format!("index admin_purge_property_postings decode failed: {e}"))??;
        match step.next {
            Some(cursor) => resume = Some(cursor),
            None => return Ok(()),
        }
    }
}

#[cfg(not(target_family = "wasm"))]
pub async fn admin_purge_property_postings(
    _index_canister: Principal,
    _kind: IndexPurgeKind,
    _property_id: u32,
    _label_id: u16,
) -> Result<(), String> {
    Ok(())
}
