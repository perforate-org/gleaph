//! Inter-canister calls from router to the property index canister.

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::IndexPurgeKind;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::federation::{IndexPostingPurgeCursor, IndexPostingPurgeStepResult};
#[cfg(target_family = "wasm")]
use gleaph_graph_kernel::federation::{ShardDetachCursor, ShardDetachStepResult};

#[cfg(all(test, not(target_family = "wasm")))]
thread_local! {
    static TEST_DETACH_SHARD_HOOK: std::cell::RefCell<
        Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>>,
    > =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) fn set_test_detach_shard_hook(hook: impl std::future::Future<Output = ()> + 'static) {
    TEST_DETACH_SHARD_HOOK.with_borrow_mut(|current| *current = Some(Box::pin(hook)));
}

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
    #[cfg(test)]
    if let Some(hook) = TEST_DETACH_SHARD_HOOK.with_borrow_mut(Option::take) {
        hook.await;
    }
    Ok(())
}

/// Advances one bounded posting-purge step on one index canister. The returned
/// [`IndexPostingPurgeStepResult`] carries the next resume cursor (`None` when `done`);
/// replaying the same request with the same resume is an idempotent bounded delete.
#[cfg(target_family = "wasm")]
pub async fn admin_purge_property_postings_step(
    index_canister: Principal,
    physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId,
    kind: IndexPurgeKind,
    property_id: u32,
    label_id: u16,
    resume: Option<IndexPostingPurgeCursor>,
) -> Result<IndexPostingPurgeStepResult, String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(index_canister, "admin_purge_property_postings")
        .with_args(&(physical_index_id, kind, property_id, label_id, &resume))
        .await
        .map_err(|e| format!("index admin_purge_property_postings call failed: {e}"))?
        .candid::<Result<IndexPostingPurgeStepResult, String>>()
        .map_err(|e| format!("index admin_purge_property_postings decode failed: {e}"))?
}

/// Native stub: one purge step that reports immediate completion.
#[cfg(not(target_family = "wasm"))]
pub async fn admin_purge_property_postings_step(
    _index_canister: Principal,
    _physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId,
    _kind: IndexPurgeKind,
    _property_id: u32,
    _label_id: u16,
    _resume: Option<IndexPostingPurgeCursor>,
) -> Result<IndexPostingPurgeStepResult, String> {
    Ok(IndexPostingPurgeStepResult {
        next: None,
        examined: 0,
        removed: 0,
        done: true,
    })
}
