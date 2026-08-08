//! Re-application of the durable index repair journal (ADR 0023 D5).
//!
//! The maintenance driver calls [`drain_once`] each tick (and after
//! `post_upgrade` once the timer re-arms) to replay failed-flush postings the
//! [`crate::facade::stable::repair_journal`] persisted. Each op is re-issued to
//! graph-index and removed from the journal on success; on the first failure the
//! drain stops, leaving the remaining entries for a later tick (the index is
//! presumed unavailable). Re-application is idempotent, so no compensation is
//! needed here.
//!
//! Vector ops (ADR 0031) are not replayed verbatim. Because the graph no longer stores embedding
//! bytes (ADR 0064 §1), the drain cannot re-derive the canonical state; each vector entry is
//! delivered as-is and the vector canister's `mutation_id` fence (`stamp <= clock`) makes a stale
//! replay a no-op. A vector entry with no configured vector client is skipped (left durable) so it
//! never wedges the property repairs queued after it.

use crate::facade::stable::derived_index_outbox::DerivedIndexOutboxOp;
use crate::facade::{GraphStore, RepairPostingOp};
use crate::index::lookup::PropertyIndexLookup;
use crate::index::vector_lookup::VectorCanisterLookup;
use crate::plan::PlanQueryError;
use candid::Encode;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::IndexBuildDmlRequest;
use gleaph_graph_kernel::index::IndexPostingMutation;
use gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp;
use gleaph_message_sizing::{FitError, SizingPolicy, adaptive_fitting_prefix};

/// Outcome of re-applying a single journal entry.
enum ApplyOutcome {
    /// The entry was delivered (or reconciled away); remove it from the journal.
    Applied,
    /// The entry could not be delivered yet but must not block the drain; leave
    /// it durable for a later tick.
    Skipped,
}

/// Re-applies the oldest durable prefix, removing each applied entry. The downstream
/// index/vector canister owns the per-call instruction budget and returns partial
/// progress when that budget is reached; the durable queue retains the unacknowledged
/// suffix for the next maintenance pass.
/// Skipped entries (e.g. a vector op with no vector client) are left durable but
/// do not stop the drain.
pub(crate) async fn drain_once(
    ix: &dyn PropertyIndexLookup,
    vector: Option<&dyn VectorCanisterLookup>,
) -> Result<(), PlanQueryError> {
    drain_queue(DurableQueue::RepairJournal, ix, vector).await
}

/// Drains the durable Plan 0088 outbox directly, without first copying it into the repair journal.
/// The same idempotent dispatcher is shared so property/vector batching and canonical-wins vector
/// reconciliation have one implementation.
pub(crate) async fn drain_outbox_once(
    ix: &dyn PropertyIndexLookup,
    vector: Option<&dyn VectorCanisterLookup>,
) -> Result<(), PlanQueryError> {
    drain_queue(DurableQueue::DerivedIndexOutbox, ix, vector).await
}

#[derive(Clone, Copy)]
enum DurableQueue {
    RepairJournal,
    DerivedIndexOutbox,
}

#[derive(Clone, Debug)]
enum DurableOp {
    Ordinary(RepairPostingOp),
    BuildDml(IndexBuildDmlRequest),
}

fn peek_queue(store: &GraphStore, queue: DurableQueue, limit: usize) -> Vec<(u64, DurableOp)> {
    match queue {
        DurableQueue::RepairJournal => store
            .repair_journal_peek(limit)
            .into_iter()
            .map(|(seq, op)| (seq, DurableOp::Ordinary(op)))
            .collect(),
        DurableQueue::DerivedIndexOutbox => store
            .derived_index_outbox_peek(limit)
            .into_iter()
            .map(|(seq, entry)| {
                let op = match entry.op {
                    DerivedIndexOutboxOp::Ordinary(op) => DurableOp::Ordinary(op),
                    DerivedIndexOutboxOp::IndexBuildDml { request } => DurableOp::BuildDml(request),
                };
                (seq, op)
            })
            .collect(),
    }
}

fn remove_from_queue(store: &GraphStore, queue: DurableQueue, seq: u64) {
    match queue {
        DurableQueue::RepairJournal => store.repair_journal_remove(seq),
        DurableQueue::DerivedIndexOutbox => store.derived_index_outbox_remove(seq),
    }
}

fn property_batch_prefix(
    shard_id: ShardId,
    entries: &[(u64, RepairPostingOp)],
) -> Result<Vec<IndexPostingMutation>, PlanQueryError> {
    let fitted = adaptive_fitting_prefix(
        entries.len(),
        None,
        SizingPolicy::inter_canister(),
        |count| {
            let candidate: Vec<_> = entries[..count]
                .iter()
                .map(|(_, op)| to_index_mutation(op))
                .collect::<Result<_, _>>()
                .map_err(|error: PlanQueryError| error.to_string())?;
            Encode!(&(shard_id, &candidate))
                .map(|encoded| encoded.len())
                .map_err(|error| error.to_string())
        },
    )
    .map_err(|error| match error {
        FitError::Measure(_) => {
            PlanQueryError::UnsupportedOp("failed to encode index posting batch")
        }
        FitError::NoEntryFits { .. } => PlanQueryError::UnsupportedOp(
            "single index posting exceeds the safe inter-canister request payload limit",
        ),
    })?
    .ok_or(PlanQueryError::UnsupportedOp("empty index posting batch"))?;
    let best = fitted.entry_count;
    entries[..best]
        .iter()
        .map(|(_, op)| to_index_mutation(op))
        .collect()
}

fn vector_batch_prefix(entries: &[VectorEmbeddingSyncOp]) -> Result<usize, PlanQueryError> {
    let fitted = adaptive_fitting_prefix(
        entries.len(),
        None,
        SizingPolicy::inter_canister(),
        |count| {
            let candidate = entries[..count].to_vec();
            Encode!(&(&candidate,))
                .map(|encoded| encoded.len())
                .map_err(|error| error.to_string())
        },
    )
    .map_err(|error| match error {
        FitError::Measure(_) => PlanQueryError::UnsupportedOp("failed to encode vector sync batch"),
        FitError::NoEntryFits { .. } => PlanQueryError::UnsupportedOp(
            "single vector operation exceeds the safe inter-canister request payload limit",
        ),
    })?
    .ok_or(PlanQueryError::UnsupportedOp("empty vector sync batch"))?;
    Ok(fitted.entry_count)
}

async fn drain_queue(
    queue: DurableQueue,
    ix: &dyn PropertyIndexLookup,
    vector: Option<&dyn VectorCanisterLookup>,
) -> Result<(), PlanQueryError> {
    let store = GraphStore::new();
    if !store.federation_configured() {
        return Ok(());
    }
    let shard_id = ix.local_shard_id();
    // Do not impose a second item-count budget here. The target canister has the authoritative
    // instruction counter and returns the largest safe applied prefix for the current call.
    // Passing the complete durable suffix lets one call consume that full target-side budget;
    // the queue itself remains bounded by stable-memory capacity and is never acknowledged past
    // the returned progress.
    let entries = peek_queue(&store, queue, usize::MAX);
    let mut offset = 0usize;
    while offset < entries.len() {
        if let DurableOp::BuildDml(request) = &entries[offset].1 {
            // Build envelopes carry their own epoch, shard sequence, and exact subject/value
            // sets. They are never folded into the ordinary posting batch. Remove the outbox
            // entry only after graph-index accepts the request, then advance Graph's local drain
            // watermark under the same exact identity.
            ix.apply_index_build_dml(request.clone()).await?;
            crate::index::canonical_export::ack_build_dml(
                request.physical_index_id,
                request.catalog_epoch,
                request.shard_sequence,
            )
            .map_err(|error| PlanQueryError::IndexFlushDeferred {
                op: "index_build_drain_ack",
                detail: error.to_string(),
            })?;
            remove_from_queue(&store, queue, entries[offset].0);
            offset += 1;
            continue;
        }

        let is_vector = matches!(
            entries[offset].1,
            DurableOp::Ordinary(RepairPostingOp::VectorEmbedding { .. })
        );
        if is_vector {
            let start = offset;
            while offset < entries.len()
                && matches!(
                    entries[offset].1,
                    DurableOp::Ordinary(RepairPostingOp::VectorEmbedding { .. })
                )
            {
                offset += 1;
            }
            let group: Vec<_> = entries[start..offset]
                .iter()
                .filter_map(|(seq, op)| match op {
                    DurableOp::Ordinary(op @ RepairPostingOp::VectorEmbedding { .. }) => {
                        Some((*seq, op.clone()))
                    }
                    _ => None,
                })
                .collect();
            let Some(vx) = vector else {
                continue;
            };
            if vx.supports_sync_batch() {
                let mut reconciled = Vec::with_capacity(group.len());
                for (_, op) in &group {
                    match op {
                        RepairPostingOp::VectorEmbedding { op } => {
                            reconciled.push(reconcile_vector_op(op).await?);
                        }
                        _ => unreachable!("vector group contains property entry"),
                    }
                }
                let mut group_offset = 0usize;
                while group_offset < group.len() {
                    let reconciled_prefix = vector_batch_prefix(&reconciled[group_offset..])?;
                    let progress = vx
                        .vector_sync_batch(
                            reconciled[group_offset..group_offset + reconciled_prefix].to_vec(),
                        )
                        .await?;
                    let applied = usize::try_from(progress.applied).map_err(|_| {
                        PlanQueryError::UnsupportedOp("invalid vector repair progress")
                    })?;
                    if applied == 0 || applied > reconciled_prefix {
                        return Err(PlanQueryError::UnsupportedOp(
                            "invalid vector repair progress",
                        ));
                    }
                    for (seq, _) in &group[group_offset..group_offset + applied] {
                        remove_from_queue(&store, queue, *seq);
                    }
                    group_offset += applied;
                    if progress.next_index.is_none() {
                        break;
                    }
                }
            } else {
                for (seq, op) in group {
                    apply(ix, Some(vx), shard_id, &op).await?;
                    remove_from_queue(&store, queue, seq);
                }
            }
            continue;
        }

        let start = offset;
        while offset < entries.len() {
            if matches!(
                entries[offset].1,
                DurableOp::Ordinary(RepairPostingOp::VectorEmbedding { .. })
                    | DurableOp::BuildDml(_)
            ) {
                break;
            }
            offset += 1;
        }
        let group: Vec<_> = entries[start..offset]
            .iter()
            .filter_map(|(seq, op)| match op {
                DurableOp::Ordinary(op) => Some((*seq, op.clone())),
                DurableOp::BuildDml(_) => None,
            })
            .collect();
        if group.is_empty() {
            continue;
        }
        if ix.supports_posting_batch() {
            let mut group_offset = 0usize;
            while group_offset < group.len() {
                let operations = property_batch_prefix(shard_id, &group[group_offset..])?;
                let operation_count = operations.len();
                let progress = ix.posting_batch_at(shard_id, operations).await?;
                let applied = usize::try_from(progress.applied)
                    .map_err(|_| PlanQueryError::UnsupportedOp("invalid index repair progress"))?;
                if applied == 0 || applied > operation_count {
                    return Err(PlanQueryError::UnsupportedOp(
                        "invalid index repair progress",
                    ));
                }
                for (seq, _) in &group[group_offset..group_offset + applied] {
                    remove_from_queue(&store, queue, *seq);
                }
                group_offset += applied;
                if progress.next_index.is_none() {
                    break;
                }
            }
        } else {
            for (seq, op) in group {
                apply(ix, vector, shard_id, &op).await?;
                remove_from_queue(&store, queue, seq);
            }
        }
    }
    Ok(())
}

fn to_index_mutation(op: &RepairPostingOp) -> Result<IndexPostingMutation, PlanQueryError> {
    match op {
        RepairPostingOp::VertexProperty {
            physical_index_id,
            phase,
            remove,
            property_id,
            payload_bytes,
            vertex_id,
            ..
        } if phase.is_active() => Ok(IndexPostingMutation::VertexProperty {
            physical_index_id: *physical_index_id,
            remove: *remove,
            property_id: *property_id,
            value: payload_bytes.clone(),
            vertex_id: *vertex_id,
        }),
        RepairPostingOp::EdgeProperty {
            physical_index_id,
            phase,
            remove,
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
            ..
        } if phase.is_active() => Ok(IndexPostingMutation::EdgeProperty {
            physical_index_id: *physical_index_id,
            remove: *remove,
            property_id: *property_id,
            value: payload_bytes.clone(),
            label_id: *label_id,
            owner_vertex_id: *owner_vertex_id,
            slot_index: *slot_index,
        }),
        RepairPostingOp::Label {
            remove,
            label_id,
            vertex_id,
        } => Ok(IndexPostingMutation::Label {
            remove: *remove,
            label_id: *label_id,
            vertex_id: *vertex_id,
        }),
        RepairPostingOp::VectorEmbedding { .. } => unreachable!("vector entries are not batched"),
        RepairPostingOp::VertexProperty { .. } | RepairPostingOp::EdgeProperty { .. } => {
            Err(PlanQueryError::UnsupportedOp(
                "index repair dispatch requires Active maintenance phase",
            ))
        }
    }
}

async fn apply(
    ix: &dyn PropertyIndexLookup,
    vector: Option<&dyn VectorCanisterLookup>,
    shard_id: ShardId,
    op: &RepairPostingOp,
) -> Result<ApplyOutcome, PlanQueryError> {
    match op {
        RepairPostingOp::VertexProperty {
            physical_index_id,
            phase,
            remove,
            property_id,
            payload_bytes,
            vertex_id,
            ..
        } => {
            if !phase.is_active() {
                return Err(PlanQueryError::UnsupportedOp(
                    "index repair dispatch requires Active maintenance phase",
                ));
            }
            if *remove {
                ix.posting_remove(
                    *physical_index_id,
                    *property_id,
                    payload_bytes.clone(),
                    *vertex_id,
                )
                .await?;
            } else {
                ix.posting_insert(
                    *physical_index_id,
                    *property_id,
                    payload_bytes.clone(),
                    *vertex_id,
                )
                .await?;
            }
            Ok(ApplyOutcome::Applied)
        }
        RepairPostingOp::EdgeProperty {
            physical_index_id,
            phase,
            remove,
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
            ..
        } => {
            if !phase.is_active() {
                return Err(PlanQueryError::UnsupportedOp(
                    "index repair dispatch requires Active maintenance phase",
                ));
            }
            if *remove {
                ix.edge_posting_remove_at(
                    shard_id,
                    *physical_index_id,
                    *property_id,
                    payload_bytes.clone(),
                    *label_id,
                    *owner_vertex_id,
                    *slot_index,
                )
                .await?;
            } else {
                ix.edge_posting_insert_at(
                    shard_id,
                    *physical_index_id,
                    *property_id,
                    payload_bytes.clone(),
                    *label_id,
                    *owner_vertex_id,
                    *slot_index,
                )
                .await?;
            }
            Ok(ApplyOutcome::Applied)
        }
        RepairPostingOp::Label {
            remove,
            label_id,
            vertex_id,
        } => {
            if *remove {
                ix.label_posting_remove(*label_id, *vertex_id).await?;
            } else {
                ix.label_posting_insert(*label_id, *vertex_id).await?;
            }
            Ok(ApplyOutcome::Applied)
        }
        RepairPostingOp::VectorEmbedding { op } => {
            let Some(vx) = vector else {
                // No client to deliver to: leave this entry durable so it does not wedge the
                // property repairs queued after it. It re-applies once a vector client exists.
                return Ok(ApplyOutcome::Skipped);
            };
            let reconciled = reconcile_vector_op(op).await?;
            if vx.supports_sync_batch() {
                vx.vector_sync_batch(vec![reconciled]).await?;
            } else if reconciled.remove {
                vx.vector_remove(reconciled).await?;
            } else {
                vx.vector_upsert(reconciled).await?;
            }
            Ok(ApplyOutcome::Applied)
        }
    }
}

/// Reconciles a journaled vector op for delivery. The graph no longer stores embedding bytes (ADR
/// 0064 §1), so it cannot re-derive the canonical state; the op is delivered as-is. The vector
/// canister's `mutation_id` fence (`stamp <= clock`) makes a stale replay a no-op, so no
/// reconciliation is required for idempotence or ordering.
async fn reconcile_vector_op(
    op: &VectorEmbeddingSyncOp,
) -> Result<VectorEmbeddingSyncOp, PlanQueryError> {
    Ok(op.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use async_trait::async_trait;
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{
        IndexIntersectionRequest, IndexMaintenancePhase, PhysicalIndexId, PostingHit,
        PostingRangeRequest,
    };
    use gleaph_graph_kernel::vector_index::VectorMetric;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    /// Index mock that fails the Nth `posting_insert_at` (1-based) and counts
    /// successful re-applications, so a drain can be observed mid-batch.
    struct CountingIndex {
        fail_insert_at: usize,
        inserts: AtomicUsize,
        batch_calls: AtomicUsize,
        supports_batch: bool,
        batch_limit: Option<usize>,
    }

    impl CountingIndex {
        fn new(fail_insert_at: usize) -> Self {
            Self {
                fail_insert_at,
                inserts: AtomicUsize::new(0),
                batch_calls: AtomicUsize::new(0),
                supports_batch: false,
                batch_limit: None,
            }
        }

        fn batch() -> Self {
            Self {
                fail_insert_at: 0,
                inserts: AtomicUsize::new(0),
                batch_calls: AtomicUsize::new(0),
                supports_batch: true,
                batch_limit: None,
            }
        }

        fn partial_batch(limit: usize) -> Self {
            Self {
                fail_insert_at: 0,
                inserts: AtomicUsize::new(0),
                batch_calls: AtomicUsize::new(0),
                supports_batch: true,
                batch_limit: Some(limit),
            }
        }
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for CountingIndex {
        fn supports_posting_batch(&self) -> bool {
            self.supports_batch
        }

        async fn posting_batch_at(
            &self,
            _shard_id: ShardId,
            operations: Vec<gleaph_graph_kernel::index::IndexPostingMutation>,
        ) -> Result<gleaph_graph_kernel::index::IndexPostingBatchProgress, PlanQueryError> {
            self.batch_calls.fetch_add(1, Ordering::SeqCst);
            let applied = self
                .batch_limit
                .map_or(operations.len(), |limit| limit.min(operations.len()));
            Ok(gleaph_graph_kernel::index::IndexPostingBatchProgress {
                applied: applied as u32,
                next_index: (applied < operations.len()).then_some(applied as u32),
                instruction_budget_exhausted: applied < operations.len(),
            })
        }

        async fn lookup_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Result<Vec<PostingHit>, PlanQueryError> {
            Ok(vec![])
        }

        async fn lookup_range(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _req: &PostingRangeRequest,
        ) -> Result<Vec<PostingHit>, PlanQueryError> {
            Ok(vec![])
        }

        async fn lookup_intersection(
            &self,
            _req: &IndexIntersectionRequest,
        ) -> Result<gleaph_graph_kernel::index::IndexIntersectionResult, PlanQueryError> {
            Ok(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices(vec![]))
        }

        fn local_shard_id(&self) -> ShardId {
            ShardId::new(0)
        }

        async fn posting_insert_at(
            &self,
            _shard_id: ShardId,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            let n = self.inserts.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.fail_insert_at {
                return Err(PlanQueryError::UnsupportedOp("test_repair_insert_fail"));
            }
            Ok(())
        }

        async fn posting_remove_at(
            &self,
            _shard_id: ShardId,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }

        async fn label_posting_insert_at(
            &self,
            _shard_id: ShardId,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }

        async fn label_posting_remove_at(
            &self,
            _shard_id: ShardId,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }
    }

    fn vertex_insert(vertex_id: u32) -> RepairPostingOp {
        RepairPostingOp::VertexProperty {
            physical_index_id: PhysicalIndexId::new(900_103).expect("test physical id"),
            catalog_epoch: 1,
            phase: IndexMaintenancePhase::Active,
            remove: false,
            property_id: 1,
            payload_bytes: vec![vertex_id as u8],
            vertex_id,
        }
    }

    fn with_routing<R>(body: impl FnOnce(&GraphStore) -> R) -> R {
        let graph = GraphStore::new();
        graph
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_canister: None,
            }))
            .expect("set routing");
        for (seq, _) in graph.repair_journal_peek(usize::MAX) {
            graph.repair_journal_remove(seq);
        }
        for (seq, _) in graph.derived_index_outbox_peek(usize::MAX) {
            graph.derived_index_outbox_remove(seq);
        }
        let out = body(&graph);
        for (seq, _) in graph.repair_journal_peek(usize::MAX) {
            graph.repair_journal_remove(seq);
        }
        for (seq, _) in graph.derived_index_outbox_peek(usize::MAX) {
            graph.derived_index_outbox_remove(seq);
        }
        graph.set_federation_routing(None).expect("clear routing");
        out
    }

    #[test]
    fn drain_reapplies_all_and_clears_journal() {
        with_routing(|graph| {
            graph.repair_journal_append(0, [vertex_insert(1), vertex_insert(2), vertex_insert(3)]);
            let index = CountingIndex::new(0);
            pollster::block_on(drain_once(&index, None)).expect("drain succeeds");
            assert_eq!(index.inserts.load(Ordering::SeqCst), 3);
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn drain_coalesces_a_compatible_repair_tail_into_one_batch_call() {
        with_routing(|graph| {
            graph.repair_journal_append(0, [vertex_insert(1), vertex_insert(2)]);
            // This models the maintenance driver's newer pending work appended after the older
            // durable journal tail. The drain must preserve that order while using one batch call.
            graph.repair_journal_append(0, [vertex_insert(3), vertex_insert(4)]);
            let index = CountingIndex::batch();

            pollster::block_on(drain_once(&index, None)).expect("drain succeeds");
            assert_eq!(index.batch_calls.load(Ordering::SeqCst), 1);
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn drain_outbox_reuses_batch_dispatch_without_copying_to_repair_journal() {
        with_routing(|graph| {
            graph.derived_index_outbox_append(17, [vertex_insert(1), vertex_insert(2)]);
            let index = CountingIndex::batch();

            pollster::block_on(drain_outbox_once(&index, None)).expect("outbox drain succeeds");
            assert_eq!(index.batch_calls.load(Ordering::SeqCst), 1);
            assert!(graph.derived_index_outbox_is_empty());
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn index_sync_status_reflects_outbox_and_repair_journal() {
        with_routing(|graph| {
            assert!(graph.index_sync_status().converged);

            // The first-delivery outbox alone must pin convergence: batch-path postings
            // can sit there when a dynamic batch cut off before the synchronous drain.
            graph.derived_index_outbox_append(17, [vertex_insert(1)]);
            let status = graph.index_sync_status();
            assert_eq!(status.derived_index_outbox_len, 1);
            assert_eq!(status.repair_journal_len, 0);
            assert!(!status.converged);

            // A failed-flush repair batch must pin convergence independently.
            graph.repair_journal_append(9, [vertex_insert(2)]);
            let status = graph.index_sync_status();
            assert_eq!(status.derived_index_outbox_len, 1);
            assert_eq!(status.repair_journal_len, 1);
            assert!(!status.converged);

            for (seq, _) in graph.derived_index_outbox_peek(usize::MAX) {
                graph.derived_index_outbox_remove(seq);
            }
            for (seq, _) in graph.repair_journal_peek(usize::MAX) {
                graph.repair_journal_remove(seq);
            }
            assert!(graph.index_sync_status().converged);
        });
    }

    #[test]
    fn drain_batch_has_no_fixed_item_count_cap() {
        with_routing(|graph| {
            graph.derived_index_outbox_append(17, (0..129).map(vertex_insert));
            let index = CountingIndex::batch();

            pollster::block_on(drain_outbox_once(&index, None)).expect("drain succeeds");
            assert_eq!(index.batch_calls.load(Ordering::SeqCst), 1);
            assert!(graph.derived_index_outbox_is_empty());
        });
    }

    #[test]
    fn drain_retries_unacknowledged_suffix_after_partial_batch_progress() {
        with_routing(|graph| {
            graph.derived_index_outbox_append(17, (0..5).map(vertex_insert));
            let index = CountingIndex::partial_batch(2);

            pollster::block_on(drain_outbox_once(&index, None)).expect("drain succeeds");
            assert_eq!(index.batch_calls.load(Ordering::SeqCst), 3);
            assert!(graph.derived_index_outbox_is_empty());
        });
    }

    #[test]
    fn min_tracked_mutation_id_pins_lowest_unapplied_and_ignores_untracked() {
        with_routing(|graph| {
            // No tracked entries yet: fully caught up.
            assert_eq!(graph.index_pending_min_mutation_id(), None);
            // An untracked (mutation_id 0) batch never pins the watermark.
            graph.repair_journal_append(0, [vertex_insert(1)]);
            assert_eq!(graph.index_pending_min_mutation_id(), None);
            // Tracked batches pin the smallest unapplied mutation id.
            graph.repair_journal_append(7, [vertex_insert(2)]);
            graph.repair_journal_append(9, [vertex_insert(3)]);
            assert_eq!(graph.index_pending_min_mutation_id(), Some(7));
            // Draining the mutation-7 prefix advances the watermark exactly once to 9.
            let index = CountingIndex::new(2); // fail the 2nd insert (mutation 7's op)
            let _ = pollster::block_on(drain_once(&index, None));
            // The untracked op (seq 0) drained; mutation 7 remains the floor.
            assert_eq!(graph.index_pending_min_mutation_id(), Some(7));
            let healthy = CountingIndex::new(0);
            pollster::block_on(drain_once(&healthy, None)).expect("drain converges");
            assert_eq!(graph.index_pending_min_mutation_id(), None);
            assert!(graph.repair_journal_is_empty());
        });
    }

    struct RecordingVectorCanister {
        upserts: AtomicUsize,
        removes: AtomicUsize,
        last_remove_mutation_id: AtomicU64,
        last_upsert_mutation_id: AtomicU64,
        last_upsert_metric: std::sync::Mutex<VectorMetric>,
    }

    impl RecordingVectorCanister {
        fn new() -> Self {
            Self {
                upserts: AtomicUsize::new(0),
                removes: AtomicUsize::new(0),
                last_remove_mutation_id: AtomicU64::new(0),
                last_upsert_mutation_id: AtomicU64::new(0),
                last_upsert_metric: std::sync::Mutex::new(VectorMetric::L2Squared),
            }
        }
    }

    #[async_trait(?Send)]
    impl VectorCanisterLookup for RecordingVectorCanister {
        async fn vector_upsert(
            &self,
            op: gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp,
        ) -> Result<(), PlanQueryError> {
            self.upserts.fetch_add(1, Ordering::SeqCst);
            self.last_upsert_mutation_id
                .store(op.mutation_id, Ordering::SeqCst);
            *self.last_upsert_metric.lock().unwrap() = op.metric;
            Ok(())
        }

        async fn vector_remove(
            &self,
            op: gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp,
        ) -> Result<(), PlanQueryError> {
            self.removes.fetch_add(1, Ordering::SeqCst);
            self.last_remove_mutation_id
                .store(op.mutation_id, Ordering::SeqCst);
            Ok(())
        }
    }

    struct PartialVectorCanister {
        batch_calls: AtomicUsize,
        batch_limit: usize,
    }

    #[async_trait(?Send)]
    impl VectorCanisterLookup for PartialVectorCanister {
        fn supports_sync_batch(&self) -> bool {
            true
        }

        async fn vector_sync_batch(
            &self,
            operations: Vec<gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp>,
        ) -> Result<gleaph_graph_kernel::vector_index::VectorSyncBatchProgress, PlanQueryError>
        {
            self.batch_calls.fetch_add(1, Ordering::SeqCst);
            let applied = self.batch_limit.min(operations.len());
            Ok(gleaph_graph_kernel::vector_index::VectorSyncBatchProgress {
                applied: applied as u32,
                next_index: (applied < operations.len()).then_some(applied as u32),
                instruction_budget_exhausted: applied < operations.len(),
            })
        }

        async fn vector_upsert(
            &self,
            _op: gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp,
        ) -> Result<(), PlanQueryError> {
            unreachable!("partial vector test uses vector_sync_batch")
        }

        async fn vector_remove(
            &self,
            _op: gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp,
        ) -> Result<(), PlanQueryError> {
            unreachable!("partial vector test uses vector_sync_batch")
        }
    }

    fn vector_upsert_op(vertex_id: u32) -> RepairPostingOp {
        use gleaph_graph_kernel::vector_index::{
            VectorEmbeddingSyncOp, VectorEncoding, VectorSubject,
        };
        RepairPostingOp::VectorEmbedding {
            op: VectorEmbeddingSyncOp {
                index_id: 1,
                embedding_name_id: 1,
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id,
                },
                mutation_id: 1,
                encoding: VectorEncoding::F32,
                dims: 1,
                metric: VectorMetric::L2Squared,
                bytes: vec![0, 0, 0, 0],
                remove: false,
            },
        }
    }

    fn vector_cosine_upsert_op(vertex_id: u32) -> RepairPostingOp {
        use gleaph_graph_kernel::vector_index::{
            VectorEmbeddingSyncOp, VectorEncoding, VectorSubject,
        };
        RepairPostingOp::VectorEmbedding {
            op: VectorEmbeddingSyncOp {
                index_id: 1,
                embedding_name_id: 1,
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id,
                },
                mutation_id: 1,
                encoding: VectorEncoding::F32,
                dims: 1,
                metric: VectorMetric::Cosine,
                bytes: vec![0, 0, 0, 0],
                remove: false,
            },
        }
    }

    #[test]
    fn drain_retries_unacknowledged_vector_suffix_after_partial_progress() {
        with_routing(|graph| {
            for vertex_id in 0..5 {
                graph.derived_index_outbox_append(17, [vector_upsert_op(vertex_id)]);
            }
            let index = CountingIndex::batch();
            let vector = PartialVectorCanister {
                batch_calls: AtomicUsize::new(0),
                batch_limit: 2,
            };

            pollster::block_on(drain_outbox_once(&index, Some(&vector)))
                .expect("vector drain succeeds");
            assert_eq!(vector.batch_calls.load(Ordering::SeqCst), 3);
            assert!(graph.derived_index_outbox_is_empty());
        });
    }

    #[test]
    fn drain_delivers_journaled_upsert_as_is() {
        with_routing(|graph| {
            // The graph no longer stores embedding bytes (ADR 0064 §1), so the drain delivers the
            // journaled op as-is; the vector canister's mutation_id fence makes a stale replay a no-op.
            graph.repair_journal_append(0, [vector_upsert_op(1), vector_upsert_op(2)]);
            let index = CountingIndex::new(0);
            let vector = RecordingVectorCanister::new();
            pollster::block_on(drain_once(&index, Some(&vector))).expect("drain succeeds");
            assert_eq!(vector.upserts.load(Ordering::SeqCst), 2);
            assert_eq!(vector.removes.load(Ordering::SeqCst), 0);
            assert_eq!(
                vector.last_upsert_mutation_id.load(Ordering::SeqCst),
                1,
                "drain preserves the op's mutation_id stamp"
            );
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn drain_delivers_journaled_upsert_as_is_without_canonical_state() {
        with_routing(|graph| {
            // No canonical embedding store exists; the drain still delivers the journaled upsert
            // as-is rather than reconciling it to a remove.
            graph.repair_journal_append(0, [vector_upsert_op(5)]);
            let index = CountingIndex::new(0);
            let vector = RecordingVectorCanister::new();
            pollster::block_on(drain_once(&index, Some(&vector))).expect("drain succeeds");
            assert_eq!(vector.upserts.load(Ordering::SeqCst), 1);
            assert_eq!(vector.removes.load(Ordering::SeqCst), 0);
            assert_eq!(
                vector.last_upsert_mutation_id.load(Ordering::SeqCst),
                1,
                "drain preserves the op's mutation_id stamp"
            );
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn drain_skips_vector_op_without_client_without_wedging() {
        with_routing(|graph| {
            // A vector op with no vector client is left durable, but the property op queued after
            // it still drains (no wedge).
            graph.repair_journal_append(0, [vector_upsert_op(1), vertex_insert(2)]);
            let index = CountingIndex::new(0);
            pollster::block_on(drain_once(&index, None)).expect("drain does not wedge");
            assert_eq!(
                index.inserts.load(Ordering::SeqCst),
                1,
                "property op applied past the skipped vector op"
            );
            let remaining: Vec<RepairPostingOp> = graph
                .repair_journal_peek(usize::MAX)
                .into_iter()
                .map(|(_, op)| op)
                .collect();
            assert_eq!(
                remaining,
                vec![vector_upsert_op(1)],
                "only the skipped vector op remains"
            );
        });
    }

    #[test]
    fn drain_stops_at_failure_and_retains_remaining() {
        with_routing(|graph| {
            graph.repair_journal_append(0, [vertex_insert(1), vertex_insert(2), vertex_insert(3)]);
            // Fail the 2nd insert: the 1st is removed, the 2nd and 3rd persist.
            let index = CountingIndex::new(2);
            let err = pollster::block_on(drain_once(&index, None)).expect_err("drain stops");
            assert!(err.to_string().contains("test_repair_insert_fail"));
            assert_eq!(index.inserts.load(Ordering::SeqCst), 2);

            let remaining: Vec<RepairPostingOp> = graph
                .repair_journal_peek(usize::MAX)
                .into_iter()
                .map(|(_, op)| op)
                .collect();
            assert_eq!(remaining, vec![vertex_insert(2), vertex_insert(3)]);

            // A second drain with a healthy index converges to empty.
            let healthy = CountingIndex::new(0);
            pollster::block_on(drain_once(&healthy, None)).expect("second drain succeeds");
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn drain_repair_preserves_journaled_cosine_metric() {
        with_routing(|graph| {
            graph.repair_journal_append(0, [vector_cosine_upsert_op(1)]);

            let index = CountingIndex::new(0);
            let vector = RecordingVectorCanister::new();
            pollster::block_on(drain_once(&index, Some(&vector))).expect("drain succeeds");
            assert_eq!(vector.upserts.load(Ordering::SeqCst), 1);
            assert_eq!(
                *vector.last_upsert_metric.lock().unwrap(),
                VectorMetric::Cosine,
                "repair replay must preserve the journaled op's metric"
            );
            assert!(graph.repair_journal_is_empty());
        });
    }
}
