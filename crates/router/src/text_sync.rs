//! Router calls for the TEXT attach handshake (plan 0297).
//!
//! `CREATE TEXT INDEX` provisions a text canister, registers the definition, and then attaches
//! every live shard: the text canister first authorizes the shard canister as a document writer,
//! then the target is written into the shard's local `FederationRouting.text_canister` so the
//! shard's DML and maintenance flushes can deliver the pending document log. The shard's local routing stays the
//! source of truth; the Router retries the write on exact re-declaration, mirroring the vector
//! attach handshake in [`crate::vector_sync`].
//!
//! The writer set follows the shard registry: a shard registered after `CREATE TEXT INDEX` is
//! attached to every targeted definition of its graph ([`attach_shard_to_graph_text_indexes`]),
//! and unregistering a shard detaches it from each of those text canisters
//! ([`detach_shard_from_graph_text_indexes`]). `DROP TEXT INDEX` retires the definition on every
//! live shard ([`retire_text_target_on_live_shards`]) before Provision deletes its canister; a
//! `Dropping` definition is never attached to a new shard again.

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};

use crate::facade::stable::text_index_catalog;

/// Bounded steps attempted before a text scan, so a query sees the documents delivered since the
/// last flush instead of the under-posted lag the text canister documents. Best-effort: a flush
/// failure leaves the scan to run against the already-flushed postings.
#[cfg(all(target_family = "wasm", not(feature = "pocket-ic-e2e")))]
const TEXT_FLUSH_STEPS_BEFORE_SCAN: u32 = 16;

#[cfg(all(target_family = "wasm", not(feature = "pocket-ic-e2e")))]
use gleaph_graph_kernel::text_wire::FlushReport;

/// Router → text canister: bounded `admin_flush` loop (controller-guarded; the Router is the
/// canister's controller). Runs before a text scan so search observes delivered documents.
#[cfg(all(target_family = "wasm", not(feature = "pocket-ic-e2e")))]
pub async fn admin_flush_text_canister(text_canister: Principal) -> Result<(), String> {
    use ic_cdk::call::Call;

    for _ in 0..TEXT_FLUSH_STEPS_BEFORE_SCAN {
        let report: FlushReport = Call::unbounded_wait(text_canister, "admin_flush")
            .with_args(&())
            .await
            .map_err(|e| format!("text admin_flush call failed: {e}"))?
            .candid()
            .map_err(|e| format!("text admin_flush decode failed: {e}"))?;
        if report.done {
            return Ok(());
        }
        if report.remaining_ops == 0 && report.drained_ops == 0 {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(all(not(target_family = "wasm"), not(feature = "pocket-ic-e2e")))]
pub async fn admin_flush_text_canister(_text_canister: Principal) -> Result<(), String> {
    Ok(())
}

#[cfg(feature = "pocket-ic-e2e")]
pub async fn admin_flush_text_canister(_text_canister: Principal) -> Result<(), String> {
    // The PocketIC harness drives flush legs explicitly.
    Ok(())
}

/// Attach one live shard to a text index target. The text canister authorizes the shard canister
/// as a document writer first, and only then does the shard learn the target, so the shard can
/// never hold a target whose canister would reject its delivery. Both legs are idempotent, so a
/// retry after a failure between them converges.
///
/// The e2e Router build leaves both legs to the PocketIC harness (fixtures drive them in this
/// order), so this is a no-op there.
pub async fn attach_shard_to_text_canister(
    shard: &ShardRegistryEntry,
    label_id: u16,
    property_id: u32,
    text_canister: Principal,
) -> Result<(), String> {
    #[cfg(feature = "pocket-ic-e2e")]
    {
        let _ = (shard, label_id, property_id, text_canister);
        Ok(())
    }
    #[cfg(not(feature = "pocket-ic-e2e"))]
    {
        admin_attach_shard_to_text(text_canister, shard.shard_id.raw(), shard.graph_canister)
            .await?;
        admin_set_graph_text_canister(shard.graph_canister, label_id, property_id, text_canister)
            .await
    }
}

/// Attach one registered shard to every TEXT definition of `graph_id` that has a target. Shard
/// registration calls this so a shard added after `CREATE TEXT INDEX` delivers its documents the
/// same way the shards attached at creation do. Idempotent per definition.
pub async fn attach_shard_to_graph_text_indexes(
    graph_id: GraphId,
    shard: &ShardRegistryEntry,
) -> Result<(), String> {
    for def in text_index_catalog::list_text_indexes(graph_id) {
        if def.status == text_index_catalog::TextIndexStatus::Dropping {
            // Its canister is being deleted: the new shard must never deliver to it.
            continue;
        }
        if let Some(text_canister) = def.target {
            attach_shard_to_text_canister(
                shard,
                def.label_id.raw(),
                def.property_id.raw(),
                text_canister,
            )
            .await?;
        }
    }
    Ok(())
}

/// Detach one shard from every text canister targeted by a TEXT definition of `graph_id`, so the
/// departed shard canister can no longer write documents. Shard unregistration calls this before
/// it deletes the registry row; each detach is idempotent, so a retry after a partial failure
/// converges. Definitions sharing a canister are detached once.
pub async fn detach_shard_from_graph_text_indexes(
    graph_id: GraphId,
    shard_id: ShardId,
) -> Result<(), String> {
    // A `Dropping` definition's canister may already be deleted; nothing is left to revoke.
    let targets: std::collections::BTreeSet<Principal> =
        text_index_catalog::list_text_indexes(graph_id)
            .into_iter()
            .filter(|def| def.status != text_index_catalog::TextIndexStatus::Dropping)
            .filter_map(|def| def.target)
            .collect();
    for text_canister in targets {
        admin_detach_shard_from_text(text_canister, shard_id.raw()).await?;
    }
    Ok(())
}

/// Retire one TEXT definition on every live shard of `graph_id` (ADR 0037 `DROP TEXT INDEX`):
/// each shard discards the definition's pending rows instead of delivering them to the canister
/// about to be deleted. Idempotent per shard, so a retried drop converges.
pub async fn retire_text_target_on_live_shards(
    graph_id: GraphId,
    label_id: u16,
    property_id: u32,
) -> Result<(), String> {
    let shards = crate::facade::stable::graph_catalog::list_shards_for_graph_id(graph_id)
        .map_err(|error| error.to_string())?;
    for shard in shards {
        admin_retire_graph_text_target(shard.graph_canister, label_id, property_id).await?;
    }
    Ok(())
}

/// Router → graph shard: retire the shard's local text target for one definition. Live in the
/// e2e build too: `DROP TEXT INDEX` is a real Router flow there.
#[cfg(target_family = "wasm")]
async fn admin_retire_graph_text_target(
    graph_canister: Principal,
    label_id: u16,
    property_id: u32,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(graph_canister, "admin_retire_text_target")
        .with_args(&(label_id, property_id))
        .await
        .map_err(|e| format!("graph admin_retire_text_target call failed: {e}"))?
        .candid::<Result<(), String>>()
        .map_err(|e| format!("graph admin_retire_text_target decode failed: {e}"))?
}

#[cfg(not(target_family = "wasm"))]
async fn admin_retire_graph_text_target(
    graph_canister: Principal,
    label_id: u16,
    property_id: u32,
) -> Result<(), String> {
    #[cfg(test)]
    record_test_call(TextHandshakeCall::RetireGraphTarget {
        graph_canister,
        label_id,
        property_id,
    });
    let _ = (graph_canister, label_id, property_id);
    Ok(())
}

/// Router → text canister: authorize one Graph shard canister to call `ingest_text` /
/// `delete_docs` (controller-guarded on the text canister).
#[cfg(all(target_family = "wasm", not(feature = "pocket-ic-e2e")))]
async fn admin_attach_shard_to_text(
    text_canister: Principal,
    shard_id: u32,
    shard_canister: Principal,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(text_canister, "admin_attach_shard_canister")
        .with_args(&(shard_id, shard_canister))
        .await
        .map_err(|e| format!("text admin_attach_shard_canister call failed: {e}"))?
        .candid::<Result<(), String>>()
        .map_err(|e| format!("text admin_attach_shard_canister decode failed: {e}"))?
}

#[cfg(all(not(target_family = "wasm"), not(feature = "pocket-ic-e2e")))]
async fn admin_attach_shard_to_text(
    text_canister: Principal,
    shard_id: u32,
    shard_canister: Principal,
) -> Result<(), String> {
    #[cfg(test)]
    record_test_call(TextHandshakeCall::Attach {
        text_canister,
        shard_id,
        shard_canister,
    });
    let _ = (text_canister, shard_id, shard_canister);
    Ok(())
}

/// Router → text canister: remove the writer attached for one shard (controller-guarded on the
/// text canister). Live in the e2e build too, like the vector detach: unregistration is a real
/// Router flow there, and the call is a no-op on a canister that never attached the shard.
#[cfg(target_family = "wasm")]
async fn admin_detach_shard_from_text(
    text_canister: Principal,
    shard_id: u32,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(text_canister, "admin_detach_shard_canister")
        .with_args(&(shard_id,))
        .await
        .map_err(|e| format!("text admin_detach_shard_canister call failed: {e}"))?
        .candid::<Result<(), String>>()
        .map_err(|e| format!("text admin_detach_shard_canister decode failed: {e}"))?
}

#[cfg(not(target_family = "wasm"))]
async fn admin_detach_shard_from_text(
    text_canister: Principal,
    shard_id: u32,
) -> Result<(), String> {
    #[cfg(test)]
    record_test_call(TextHandshakeCall::Detach {
        text_canister,
        shard_id,
    });
    let _ = (text_canister, shard_id);
    Ok(())
}

/// Router → graph shard: set the shard's local derived text-index target.
#[cfg(all(target_family = "wasm", not(feature = "pocket-ic-e2e")))]
async fn admin_set_graph_text_canister(
    graph_canister: Principal,
    label_id: u16,
    property_id: u32,
    text_canister: Principal,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(graph_canister, "admin_set_text_canister")
        .with_args(&(label_id, property_id, text_canister))
        .await
        .map_err(|e| format!("graph admin_set_text_canister call failed: {e}"))?
        .candid::<Result<(), String>>()
        .map_err(|e| format!("graph admin_set_text_canister decode failed: {e}"))?
}

#[cfg(all(not(target_family = "wasm"), not(feature = "pocket-ic-e2e")))]
async fn admin_set_graph_text_canister(
    graph_canister: Principal,
    label_id: u16,
    property_id: u32,
    text_canister: Principal,
) -> Result<(), String> {
    #[cfg(test)]
    record_test_call(TextHandshakeCall::SetGraphTarget {
        graph_canister,
        label_id,
        property_id,
        text_canister,
    });
    let _ = (graph_canister, label_id, property_id, text_canister);
    Ok(())
}

/// One remote handshake leg as the host build observed it (test-only).
#[cfg(all(test, not(target_family = "wasm")))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TextHandshakeCall {
    Attach {
        text_canister: Principal,
        shard_id: u32,
        shard_canister: Principal,
    },
    SetGraphTarget {
        graph_canister: Principal,
        label_id: u16,
        property_id: u32,
        text_canister: Principal,
    },
    Detach {
        text_canister: Principal,
        shard_id: u32,
    },
    RetireGraphTarget {
        graph_canister: Principal,
        label_id: u16,
        property_id: u32,
    },
}

#[cfg(all(test, not(target_family = "wasm")))]
thread_local! {
    static TEST_CALLS: std::cell::RefCell<Vec<TextHandshakeCall>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(all(test, not(target_family = "wasm")))]
fn record_test_call(call: TextHandshakeCall) {
    TEST_CALLS.with_borrow_mut(|calls| calls.push(call));
}

/// Drains the handshake legs the host build recorded since the last call (test-only).
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) fn take_test_calls() -> Vec<TextHandshakeCall> {
    TEST_CALLS.with_borrow_mut(std::mem::take)
}
