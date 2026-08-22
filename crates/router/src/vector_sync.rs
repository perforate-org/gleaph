//! Inter-canister calls from the router to a derived vector canister and to graph shards for
//! the vector attach handshake (ADR 0031 Slice 4).
//!
//! The vector attach handshake is ordered so the graph shard's **local** routing is the source of
//! truth: the router first sets the shard's `FederationRouting.vector_canister`
//! ([`admin_set_graph_vector_canister`]), then attaches the shard to the vector canister
//! ([`admin_attach_shard_to_vector`]), and only then flips its durable `vector_index_attached`
//! registry bit. This mirrors the property-index attach in [`crate::index_sync`].

use candid::{Encode, Principal};
#[cfg(not(feature = "pocket-ic-e2e"))]
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
#[cfg(target_family = "wasm")]
use gleaph_graph_kernel::federation::{ShardDetachCursor, ShardDetachStepResult};
use gleaph_graph_kernel::vector_index::{
    VectorCentroidCacheStatus, VectorMaintenanceState, VectorMaintenanceStepRequest,
    VectorMaintenanceStepResult, VectorPartitionHealthStep, VectorPartitionHealthSummary,
    VectorRebuildStatus, VectorSearchRequest, VectorSearchResult, VectorSlabStats,
    VectorSlabStatsStep, VectorSyncBatchOutcome,
};
use gleaph_message_sizing::{FitError, SizeHint, SizingPolicy, adaptive_fitting_prefix};

// The attach-handshake helpers below are only driven by `finish_shard_vector_attach`, which is itself
// `#[cfg(not(pocket-ic-e2e))]` (the e2e harness drives the handshake legs from the test instead). The
// search helper, by contrast, is the real read path and must stay live under e2e.

/// Router → graph shard: set the shard's local derived vector-index target (handshake step 1).
#[cfg(all(target_family = "wasm", not(feature = "pocket-ic-e2e")))]
pub async fn admin_set_graph_vector_canister(
    graph_canister: Principal,
    vector_canister: Principal,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(graph_canister, "admin_set_vector_canister")
        .with_args(&(vector_canister,))
        .await
        .map_err(|e| format!("graph admin_set_vector_canister call failed: {e}"))?
        .candid::<Result<(), String>>()
        .map_err(|e| format!("graph admin_set_vector_canister decode failed: {e}"))?
}

#[cfg(all(not(target_family = "wasm"), not(feature = "pocket-ic-e2e")))]
pub async fn admin_set_graph_vector_canister(
    _graph_canister: Principal,
    _vector_canister: Principal,
) -> Result<(), String> {
    Ok(())
}

/// Router → vector canister: attach a graph shard so the vector index accepts its subject sync
/// (handshake step 2). A vector canister is the single target for the whole graph (ADR 0031 Slice 4
/// target model B), so ownership is keyed by `graph_id` alone — no property-index group descriptor.
#[cfg(all(target_family = "wasm", not(feature = "pocket-ic-e2e")))]
pub async fn admin_attach_shard_to_vector(
    vector_canister: Principal,
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(vector_canister, "admin_attach_shard_canister")
        .with_args(&(graph_id, shard_id, shard_canister_principal))
        .await
        .map_err(|e| format!("vector admin_attach_shard_canister call failed: {e}"))?
        .candid()
        .map_err(|e| format!("vector admin_attach_shard_canister decode failed: {e}"))?
}

/// Router → vector canister: purge all subjects owned by one shard before Router deletes the
/// registry row. The Vector owner returns a generation-fenced cursor until explicit EOF, so this
/// helper keeps no durable Router lifecycle state and can be retried from `None` after a failure.
#[cfg(target_family = "wasm")]
pub async fn admin_detach_shard_from_vector(
    vector_canister: Principal,
    shard_id: ShardId,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    let mut resume: Option<ShardDetachCursor> = None;
    loop {
        let step: ShardDetachStepResult =
            Call::unbounded_wait(vector_canister, "admin_detach_shard_canister")
                .with_args(&(shard_id.raw(), &resume))
                .await
                .map_err(|e| format!("vector admin_detach_shard_canister call failed: {e}"))?
                .candid::<Result<ShardDetachStepResult, String>>()
                .map_err(|e| format!("vector admin_detach_shard_canister decode failed: {e}"))??;
        match step.next {
            Some(cursor) => resume = Some(cursor),
            None => return Ok(()),
        }
    }
}

#[cfg(not(target_family = "wasm"))]
pub async fn admin_detach_shard_from_vector(
    _vector_canister: Principal,
    _shard_id: ShardId,
) -> Result<(), String> {
    Ok(())
}

#[cfg(all(not(target_family = "wasm"), not(feature = "pocket-ic-e2e")))]
pub async fn admin_attach_shard_to_vector(
    _vector_canister: Principal,
    _graph_id: GraphId,
    _shard_id: ShardId,
    _shard_canister_principal: Principal,
) -> Result<(), String> {
    Ok(())
}

/// Router → vector canister: read-only exact `ivf_flat` search (ADR 0031 Slice 5). Invoked from the
/// Router composite query as a query call, mirroring [`crate::index_client::RouterIndexClient`].
#[cfg(target_family = "wasm")]
pub async fn vector_search(
    vector_canister: Principal,
    req: VectorSearchRequest,
) -> Result<VectorSearchResult, String> {
    use ic_cdk::call::Call;

    Call::bounded_wait(vector_canister, "vector_search")
        .with_args(&(req,))
        .await
        .map_err(|e| format!("vector vector_search call failed: {e}"))?
        .candid::<Result<VectorSearchResult, gleaph_graph_kernel::vector_index::VectorCanisterError>>()
        .map_err(|e| format!("vector vector_search decode failed: {e}"))?
        .map_err(|e| format!("vector vector_search rejected: {e}"))
}

#[cfg(not(target_family = "wasm"))]
pub async fn vector_search(
    _vector_canister: Principal,
    _req: VectorSearchRequest,
) -> Result<VectorSearchResult, String> {
    Ok(VectorSearchResult { hits: Vec::new() })
}

/// Router → vector canister: persist embedding bytes + stamp (ADR 0064 §6). The Router owns each
/// operation in the direct-ingestion outbox before this call; the typed result identifies the exact
/// committed prefix or terminal row.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(dead_code, reason = "driven by the Router canister batch path")
)]
fn vector_sync_batch_prefix_len(
    operations: &[gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp],
    offset: usize,
    hint: Option<SizeHint>,
) -> Result<(usize, SizeHint), String> {
    let remaining = operations.len().saturating_sub(offset);
    let fitted = adaptive_fitting_prefix(
        remaining,
        hint,
        SizingPolicy::inter_canister(),
        |count| {
            Encode!(&(operations[offset..offset + count].to_vec(),))
                .map(|encoded| encoded.len())
                .map_err(|error| error.to_string())
        },
    )
    .map_err(|error| match error {
        FitError::Measure(detail) => {
            format!("vector sync batch encode probe failed: {detail}")
        }
        FitError::NoEntryFits {
            encoded_bytes,
            hard_limit_bytes,
        } => format!(
            "single Vector batch operation is {encoded_bytes} bytes, above the safe limit of {hard_limit_bytes}"
        ),
    })?
    .ok_or_else(|| "empty Vector sync batch".to_string())?;
    Ok((fitted.entry_count, SizeHint::new(fitted.entry_count)))
}

#[cfg(target_family = "wasm")]
async fn vector_sync_batch_outcome_once(
    vector_canister: Principal,
    operations: Vec<gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp>,
) -> Result<VectorSyncBatchOutcome, String> {
    use ic_cdk::call::Call;

    let outcome = Call::bounded_wait(vector_canister, "vector_sync_batch_outcome")
        .with_args(&(operations,))
        .await
        .map_err(|e| format!("vector vector_sync_batch_outcome call failed: {e}"))?
        .candid::<Result<
            VectorSyncBatchOutcome,
            gleaph_graph_kernel::vector_index::VectorSyncBatchUnavailable,
        >>()
        .map_err(|e| format!("vector vector_sync_batch_outcome decode failed: {e}"))?
        .map_err(|e| format!("vector vector_sync_batch_outcome unavailable: {e:?}"))?;
    #[cfg(feature = "pocket-ic-e2e")]
    if crate::test_fault::drop_after_vector_batch_result() {
        return Err("pocket-ic-e2e injected loss after decoded Vector batch result".to_string());
    }
    Ok(outcome)
}

#[cfg(target_family = "wasm")]
pub async fn vector_sync_batch_outcome(
    vector_canister: Principal,
    operations: Vec<gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp>,
) -> Result<VectorSyncBatchOutcome, String> {
    if operations.is_empty() {
        return Err("vector sync batch requires at least one operation".to_string());
    }

    let mut offset = 0usize;
    let mut size_hint = None;
    while offset < operations.len() {
        let (chunk_len, next_hint) = vector_sync_batch_prefix_len(&operations, offset, size_hint)?;
        size_hint = Some(next_hint);
        let end = offset
            .checked_add(chunk_len)
            .ok_or_else(|| "vector sync batch chunk length overflow".to_string())?;
        let outcome =
            vector_sync_batch_outcome_once(vector_canister, operations[offset..end].to_vec())
                .await?;
        outcome
            .validate(chunk_len)
            .map_err(|error| format!("invalid vector sync batch outcome: {error}"))?;

        match outcome {
            VectorSyncBatchOutcome::Progress { applied } => {
                let applied = usize::try_from(applied)
                    .map_err(|_| "vector sync batch applied count overflows usize".to_string())?;
                if applied == 0 {
                    return Err(
                        "vector sync batch returned zero progress for a nonempty batch".to_string(),
                    );
                }
                offset = offset
                    .checked_add(applied)
                    .ok_or_else(|| "vector sync batch applied count overflow".to_string())?;
                if applied < chunk_len {
                    return Ok(VectorSyncBatchOutcome::Progress {
                        applied: u32::try_from(offset).map_err(|_| {
                            "vector sync batch applied count exceeds u32".to_string()
                        })?,
                    });
                }
            }
            VectorSyncBatchOutcome::Terminal {
                applied,
                failed_index,
                error,
            } => {
                let applied = usize::try_from(applied)
                    .map_err(|_| "vector sync batch applied count overflows usize".to_string())?;
                let failed_index = usize::try_from(failed_index)
                    .map_err(|_| "vector sync batch failed index overflows usize".to_string())?;
                let applied_total = offset
                    .checked_add(applied)
                    .ok_or_else(|| "vector sync batch applied count overflow".to_string())?;
                let failed_total = offset
                    .checked_add(failed_index)
                    .ok_or_else(|| "vector sync batch failed index overflow".to_string())?;
                return Ok(VectorSyncBatchOutcome::Terminal {
                    applied: u32::try_from(applied_total)
                        .map_err(|_| "vector sync batch applied count exceeds u32".to_string())?,
                    failed_index: u32::try_from(failed_total)
                        .map_err(|_| "vector sync batch failed index exceeds u32".to_string())?,
                    error,
                });
            }
        }
    }

    Ok(VectorSyncBatchOutcome::Progress {
        applied: u32::try_from(offset)
            .map_err(|_| "vector sync batch applied count exceeds u32".to_string())?,
    })
}

#[cfg(not(target_family = "wasm"))]
pub async fn vector_sync_batch_outcome(
    _vector_canister: Principal,
    _operations: Vec<gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp>,
) -> Result<VectorSyncBatchOutcome, String> {
    Err("vector vector_sync_batch_outcome is unavailable in native builds".to_string())
}

/// Router → Vector: publish the contiguous Router frontier for one exact shard lane. A bounded
/// wait makes timeout/unknown outcomes retryable by the durable Router marker queue.
#[cfg(target_family = "wasm")]
pub async fn admin_advance_router_frontier(
    vector_canister: Principal,
    shard_id: ShardId,
    frontier: u64,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::bounded_wait(vector_canister, "admin_advance_router_frontier")
        .with_args(&(shard_id, frontier))
        .await
        .map_err(|e| format!("vector admin_advance_router_frontier call failed: {e}"))?
        .candid::<Result<(), String>>()
        .map_err(|e| format!("vector admin_advance_router_frontier decode failed: {e}"))?
}

#[cfg(not(target_family = "wasm"))]
pub async fn admin_advance_router_frontier(
    _vector_canister: Principal,
    _shard_id: ShardId,
    _frontier: u64,
) -> Result<(), String> {
    Err("vector admin_advance_router_frontier is unavailable in native builds".to_string())
}

/// Narrow publisher seam used by the Router frontier recovery owner. Production delegates to the
/// bounded inter-canister call; native tests inject only this exact publication operation so they
/// exercise recovery orchestration and retirement without fabricating a second recovery driver.
pub(crate) async fn publish_router_frontier(
    vector_canister: Principal,
    shard_id: ShardId,
    frontier: u64,
) -> Result<(), String> {
    #[cfg(test)]
    if let Some(result) = FRONTIER_PUBLISHER.with_borrow_mut(|publisher| {
        publisher
            .as_mut()
            .map(|publish| publish(vector_canister, shard_id, frontier))
    }) {
        return result;
    }

    let result = admin_advance_router_frontier(vector_canister, shard_id, frontier).await;
    #[cfg(feature = "pocket-ic-e2e")]
    if result.is_ok() && crate::test_fault::drop_after_frontier_reply() {
        return Err("pocket-ic-e2e injected loss after Vector frontier reply".to_string());
    }
    result
}

#[cfg(test)]
type FrontierPublisher = Box<dyn FnMut(Principal, ShardId, u64) -> Result<(), String> + 'static>;

#[cfg(test)]
thread_local! {
    static FRONTIER_PUBLISHER: std::cell::RefCell<Option<FrontierPublisher>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct FrontierPublisherGuard;

#[cfg(test)]
impl Drop for FrontierPublisherGuard {
    fn drop(&mut self) {
        FRONTIER_PUBLISHER.with_borrow_mut(|publisher| *publisher = None);
    }
}

#[cfg(test)]
pub(crate) fn install_frontier_publisher<F>(publisher: F) -> FrontierPublisherGuard
where
    F: FnMut(Principal, ShardId, u64) -> Result<(), String> + 'static,
{
    FRONTIER_PUBLISHER.with_borrow_mut(|slot| {
        let publisher: FrontierPublisher = Box::new(publisher);
        assert!(slot.replace(publisher).is_none());
    });
    FrontierPublisherGuard
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::vector_ingest_outbox;
    use candid::Principal;
    use gleaph_graph_kernel::entry::{GraphId, VertexLabelId};
    use gleaph_graph_kernel::federation::LocalVertexId;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{
        IndexedEmbeddingSpec, VectorEncoding, VectorIndexKind, VectorMetric, VectorSubject,
    };
    use vector_ingest_outbox::VectorIngestIntentPhase;

    fn operation(bytes_len: usize) -> gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp {
        gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp {
            index_id: 7,
            embedding_name_id: 3,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(2),
                vertex_id: 9,
            },
            mutation_id: 41,
            encoding: VectorEncoding::F32,
            dims: u16::try_from(bytes_len / 4).unwrap_or(u16::MAX),
            metric: VectorMetric::L2Squared,
            bytes: vec![0; bytes_len],
            remove: false,
        }
    }

    fn intent(
        mutation_id: u64,
        phase: VectorIngestIntentPhase,
    ) -> vector_ingest_outbox::VectorIngestOutboxState {
        vector_ingest_outbox::intent_for_test(
            vector_ingest_outbox::NewVectorIngestIntent {
                graph_id: GraphId::from_raw(1),
                graph_target: Principal::from_slice(&[9; 29]),
                vector_target: Principal::from_slice(&[1; 29]),
                shard_id: ShardId::new(2),
                local_vertex_id: LocalVertexId::from(mutation_id as u32),
                spec: IndexedEmbeddingSpec {
                    embedding_name_id: 3,
                    index_id: 7,
                    kind: VectorIndexKind::IvfFlat,
                    metric: VectorMetric::L2Squared,
                    encoding: VectorEncoding::F32,
                    dims: 1,
                    labels: vec![VertexLabelId::from_raw(1)],
                },
                bytes: vec![mutation_id as u8, 0, 0, 0],
            },
            mutation_id,
            phase,
        )
    }

    #[test]
    fn vector_batch_chunk_probe_measures_the_complete_candid_request() {
        let op = operation(gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES);
        let encoded = Encode!(&(vec![op.clone()],)).expect("encode vector request");
        assert!(
            encoded.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        );
        let error = vector_sync_batch_prefix_len(std::slice::from_ref(&op), 0, None)
            .expect_err("oversized single vector operation");
        assert!(error.contains("single Vector batch operation"));
    }

    #[test]
    fn vector_batch_chunk_probe_keeps_operation_order_in_each_prefix() {
        let operations = vec![operation(1_000_000), operation(1_000_000)];
        let (chunk_len, hint) =
            vector_sync_batch_prefix_len(&operations, 0, None).expect("fit first vector prefix");
        assert_eq!(chunk_len, 1);
        assert_eq!(hint, SizeHint::new(1));
        let (next_len, _) = vector_sync_batch_prefix_len(&operations, chunk_len, Some(hint))
            .expect("fit second vector prefix");
        assert_eq!(next_len, 1);
    }

    #[test]
    fn resolved_rows_transition_to_awaiting_frontier_before_publish() {
        let _guard = vector_ingest_outbox::test_lock();
        vector_ingest_outbox::clear_for_test();
        let row = intent(41, VectorIngestIntentPhase::AwaitingVector);
        vector_ingest_outbox::insert_intents_for_test(std::slice::from_ref(&row))
            .expect("seed vector intent");

        vector_ingest_outbox::apply_outcome(
            std::slice::from_ref(&row),
            VectorSyncBatchOutcome::Progress { applied: 1 },
        )
        .expect("observe exact applied prefix");
        let resolved_rows = vector_ingest_outbox::scan(None, 8).0;
        assert_eq!(resolved_rows.len(), 1, "one durable frontier marker");
        let resolved = &resolved_rows[0];
        assert_eq!(
            resolved.phase,
            VectorIngestIntentPhase::AwaitingFrontier,
            "applied rows remain durable until frontier publication is observed"
        );
        vector_ingest_outbox::clear_for_test();
    }

    #[test]
    fn frontier_response_loss_retains_exact_marker_snapshot() {
        let _guard = vector_ingest_outbox::test_lock();
        vector_ingest_outbox::clear_for_test();
        let marker = intent(51, VectorIngestIntentPhase::AwaitingFrontier);
        let later_marker = intent(52, VectorIngestIntentPhase::AwaitingFrontier);
        vector_ingest_outbox::insert_intents_for_test(&[marker.clone(), later_marker.clone()])
            .expect("seed frontier markers");
        crate::facade::stable::ROUTER_MUTATION_COUNTER
            .with_borrow_mut(|counter| counter.set(later_marker.mutation_id));
        let before = vector_ingest_outbox::scan(None, 8).0;
        let expected_snapshot = vector_ingest_outbox::derive_frontier_snapshots_from_rows(
            &before,
            later_marker.mutation_id,
        )
        .expect("derive frontier snapshot")
        .into_iter()
        .next()
        .expect("one frontier lane");

        let remote_commits = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let remote_commits_for_publisher = remote_commits.clone();
        let _publisher = install_frontier_publisher(move |vector_target, shard_id, frontier| {
            remote_commits_for_publisher
                .borrow_mut()
                .push((vector_target, shard_id, frontier));
            Err("simulated lost reply after remote frontier commit".to_string())
        });

        futures::executor::block_on(vector_ingest_outbox::run_recovery_pass(None, 8));

        assert_eq!(
            *remote_commits.borrow(),
            vec![(
                expected_snapshot.vector_target,
                expected_snapshot.shard_id,
                expected_snapshot.frontier
            )],
            "the remote commit succeeded before its reply was lost"
        );
        let after = vector_ingest_outbox::scan(None, 8).0;
        assert_eq!(
            after, before,
            "a lost frontier reply must retain every captured marker without retirement"
        );
        assert_eq!(
            after
                .iter()
                .map(vector_ingest_outbox::VectorIngestOutboxKey::from_state)
                .collect::<Vec<_>>(),
            expected_snapshot.marker_keys,
            "the exact captured marker keys remain the retry source"
        );
        vector_ingest_outbox::clear_for_test();
    }
}

// --- ADR 0031 Slice 10: Router-forwarded vector maintenance surface ---
//
// Each helper forwards to a router-guarded vector-canister admin endpoint (which returns
// `Result<T, String>`). Reads use `bounded_wait` (query), mutators/drivers use `unbounded_wait`
// (update). Under native builds the helpers fail closed: actual forwarding is verified by PocketIC
// e2e, while native unit tests only cover Router policy validation / CRUD / RBAC / readiness gating.

/// Generates one Router→vector forward helper. Defined twice (wasm real call / native stub) via a
/// cfg gate on the macro itself, so the call body never needs a cfg attribute on an inner block.
#[cfg(target_family = "wasm")]
macro_rules! forward_vector {
    ($fn:ident, $method:literal, $waiter:ident, (), $ret:ty) => {
        pub async fn $fn(canister: Principal) -> Result<$ret, String> {
            use ic_cdk::call::Call;
            Call::$waiter(canister, $method)
                .await
                .map_err(|e| format!(concat!("vector ", $method, " call failed: {}"), e))?
                .candid::<Result<$ret, String>>()
                .map_err(|e| format!(concat!("vector ", $method, " decode failed: {}"), e))?
        }
    };
    ($fn:ident, $method:literal, $waiter:ident, ($($arg:ident: $aty:ty),+ $(,)?), $ret:ty) => {
        pub async fn $fn(canister: Principal, $($arg: $aty),+) -> Result<$ret, String> {
            use ic_cdk::call::Call;
            Call::$waiter(canister, $method)
                .with_args(&($($arg,)+))
                .await
                .map_err(|e| format!(concat!("vector ", $method, " call failed: {}"), e))?
                .candid::<Result<$ret, String>>()
                .map_err(|e| format!(concat!("vector ", $method, " decode failed: {}"), e))?
        }
    };
}

#[cfg(not(target_family = "wasm"))]
macro_rules! forward_vector {
    ($fn:ident, $method:literal, $waiter:ident, (), $ret:ty) => {
        pub async fn $fn(canister: Principal) -> Result<$ret, String> {
            let _ = &canister;
            Err(concat!("vector ", $method, " is unavailable in native builds").to_string())
        }
    };
    ($fn:ident, $method:literal, $waiter:ident, ($($arg:ident: $aty:ty),+ $(,)?), $ret:ty) => {
        pub async fn $fn(canister: Principal, $($arg: $aty),+) -> Result<$ret, String> {
            let _ = &canister;
            $(let _ = &$arg;)+
            Err(concat!("vector ", $method, " is unavailable in native builds").to_string())
        }
    };
}

// Reads (composite-query forwards): bounded_wait query calls.
forward_vector!(
    forward_admin_vector_partition_health,
    "admin_vector_partition_health",
    bounded_wait,
    (index_id: u32),
    VectorPartitionHealthSummary
);
forward_vector!(
    forward_admin_vector_partition_health_step,
    "admin_vector_partition_health_step",
    bounded_wait,
    (index_id: u32, cursor: Option<Vec<u8>>, max_pages: u32),
    VectorPartitionHealthStep
);
forward_vector!(
    forward_admin_vector_rebuild_status,
    "admin_vector_rebuild_status",
    bounded_wait,
    (index_id: u32),
    VectorRebuildStatus
);
forward_vector!(
    forward_admin_vector_slab_stats,
    "admin_vector_slab_stats",
    bounded_wait,
    (index_id: Option<u32>),
    VectorSlabStats
);
forward_vector!(
    forward_admin_vector_slab_stats_step,
    "admin_vector_slab_stats_step",
    bounded_wait,
    (cursor: Option<Vec<u8>>, max_pages: u32, index_id: Option<u32>),
    VectorSlabStatsStep
);
forward_vector!(
    forward_admin_vector_centroid_cache_status,
    "admin_vector_centroid_cache_status",
    bounded_wait,
    (),
    VectorCentroidCacheStatus
);
forward_vector!(
    forward_admin_vector_maintenance_status,
    "admin_vector_maintenance_status",
    bounded_wait,
    (index_id: u32),
    VectorMaintenanceState
);

// Mutators / drivers: unbounded_wait update calls.
forward_vector!(
    forward_admin_start_vector_rebuild,
    "admin_start_vector_rebuild",
    unbounded_wait,
    (index_id: u32, nlist: u32, sample_limit: u32),
    ()
);
forward_vector!(
    forward_admin_vector_rebuild_step,
    "admin_vector_rebuild_step",
    unbounded_wait,
    (index_id: u32, max_subjects: u32),
    VectorRebuildStatus
);
forward_vector!(
    forward_admin_publish_vector_rebuild,
    "admin_publish_vector_rebuild",
    unbounded_wait,
    (index_id: u32),
    ()
);
forward_vector!(
    forward_admin_abort_vector_rebuild,
    "admin_abort_vector_rebuild",
    unbounded_wait,
    (index_id: u32),
    ()
);
forward_vector!(
    forward_admin_vector_rebuild_cleanup_step,
    "admin_vector_rebuild_cleanup_step",
    unbounded_wait,
    (index_id: u32, max_work: u32),
    VectorRebuildStatus
);
forward_vector!(
    forward_admin_vector_centroid_cache_warmup,
    "admin_vector_centroid_cache_warmup",
    unbounded_wait,
    (index_id: u32),
    VectorCentroidCacheStatus
);
forward_vector!(
    forward_admin_vector_centroid_cache_clear,
    "admin_vector_centroid_cache_clear",
    unbounded_wait,
    (),
    VectorCentroidCacheStatus
);
forward_vector!(
    forward_admin_vector_maintenance_step,
    "admin_vector_maintenance_step",
    unbounded_wait,
    (index_id: u32, req: VectorMaintenanceStepRequest),
    VectorMaintenanceStepResult
);
forward_vector!(
    forward_admin_vector_maintenance_reset,
    "admin_vector_maintenance_reset",
    unbounded_wait,
    (index_id: u32),
    ()
);
