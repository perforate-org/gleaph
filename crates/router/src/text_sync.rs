//! Router calls for the TEXT attach handshake (plan 0297).
//!
//! `CREATE TEXT INDEX` provisions a text canister, registers the definition, and then attaches
//! every live shard: the text canister first authorizes the shard canister as a document writer,
//! then the target is written into the shard's local `FederationRouting.text_canister` so the
//! shard's DML and maintenance flushes can deliver the pending document log. The shard's local routing stays the
//! source of truth; the Router retries the write on exact re-declaration, mirroring the vector
//! attach handshake in [`crate::vector_sync`].

use candid::Principal;

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
#[cfg(not(feature = "pocket-ic-e2e"))]
pub async fn attach_shard_to_text_canister(
    shard: &gleaph_graph_kernel::federation::ShardRegistryEntry,
    label_id: u16,
    property_id: u32,
    text_canister: Principal,
) -> Result<(), String> {
    admin_attach_shard_to_text(text_canister, shard.shard_id.raw(), shard.graph_canister).await?;
    admin_set_graph_text_canister(shard.graph_canister, label_id, property_id, text_canister).await
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
    _text_canister: Principal,
    _shard_id: u32,
    _shard_canister: Principal,
) -> Result<(), String> {
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
    _graph_canister: Principal,
    _label_id: u16,
    _property_id: u32,
    _text_canister: Principal,
) -> Result<(), String> {
    Ok(())
}
