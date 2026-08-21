//! Record derived vertex-embedding mutations for the `vector-canister` canister (ADR 0031).
//!
//! ## Sync failure semantics
//!
//! Unlike property postings ([`crate::index::pending`]), vector mutations need **no compensation**
//! on partial failure: each [`VectorEmbeddingSyncOp`] is idempotent and version-guarded on the
//! canister (a replayed upsert at an already-stored `embedding_version` is a no-op; a remove writes
//! a tombstone clock that blocks resurrection by a stale upsert). On the first failed delivery the
//! whole batch — including the already-applied prefix — is appended to the durable repair journal
//! ([`crate::facade::stable::repair_journal`], ADR 0023 D5) and the maintenance timer is armed; the
//! index converges by idempotent re-application (ADR 0024).

use crate::facade::{GraphStore, RepairPostingOp};
use crate::index::vector_lookup::VectorCanisterLookup;
use crate::plan::PlanQueryError;
use gleaph_graph_kernel::vector_index::{VectorEmbeddingSyncOp, VectorSyncBatchOutcome};
use std::cell::RefCell;

thread_local! {
    static PENDING: RefCell<Vec<VectorEmbeddingSyncOp>> = const { RefCell::new(Vec::new()) };
}

/// Clears the pending queue. Not invoked at the start of each GQL run: [`flush_pending`] may
/// re-queue work after a partial failure so a later update can retry.
pub(crate) fn clear_pending() {
    PENDING.with(|p| p.borrow_mut().clear());
}

pub(crate) fn push_vector_op(op: VectorEmbeddingSyncOp) {
    if !GraphStore::new().federation_configured() {
        return;
    }
    PENDING.with(|p| p.borrow_mut().push(op));
}

fn to_repair_op(op: &VectorEmbeddingSyncOp) -> RepairPostingOp {
    RepairPostingOp::VectorEmbedding { op: op.clone() }
}

/// Takes volatile vector work as durable operations for a maintenance tick that already has
/// older repair entries. The repair drain will reconcile these operations against canonical Graph
/// state before delivery.
pub(crate) fn take_pending_as_repair() -> Vec<RepairPostingOp> {
    PENDING.with(|p| {
        std::mem::take(&mut *p.borrow_mut())
            .iter()
            .map(to_repair_op)
            .collect()
    })
}

/// Takes volatile vector work for the durable derived-index outbox. Vector replay still applies
/// the canonical incarnation/version fence when the maintenance dispatcher delivers the entry.
pub(crate) fn take_pending_as_outbox() -> Vec<RepairPostingOp> {
    take_pending_as_repair()
}

#[cfg(test)]
pub(crate) fn pending_snapshot() -> Vec<VectorEmbeddingSyncOp> {
    PENDING.with(|p| p.borrow().clone())
}

fn journal_and_defer(
    ops: &[VectorEmbeddingSyncOp],
    mutation_id: u64,
    detail: String,
) -> PlanQueryError {
    GraphStore::new().repair_journal_append(mutation_id, ops.iter().map(to_repair_op));
    crate::facade::maintenance_timer::arm_if_needed();
    PlanQueryError::IndexFlushDeferred {
        op: "vector_flush",
        detail,
    }
}

pub(crate) async fn flush_pending(
    vector: Option<&dyn VectorCanisterLookup>,
    mutation_id: Option<u64>,
) -> Result<(), PlanQueryError> {
    let mutation_id = mutation_id.unwrap_or(0);
    if !GraphStore::new().federation_configured() {
        clear_pending();
        return Ok(());
    }
    let ops: Vec<VectorEmbeddingSyncOp> = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    if ops.is_empty() {
        return Ok(());
    }

    let Some(vx) = vector else {
        // No client to deliver to: journal the batch durably so the deltas are not lost, and defer.
        return Err(journal_and_defer(
            &ops,
            mutation_id,
            "no vector index client".into(),
        ));
    };

    let mut offset = 0usize;
    while offset < ops.len() {
        let remaining = ops.len() - offset;
        let outcome = match vx.vector_sync_batch_outcome(ops[offset..].to_vec()).await {
            Ok(outcome) => {
                if let Err(detail) = outcome.validate(remaining) {
                    return Err(journal_and_defer(
                        &ops[offset..],
                        mutation_id,
                        format!("invalid vector batch outcome: {detail}"),
                    ));
                }
                outcome
            }
            Err(primary) => {
                // A transport or availability error makes the committed prefix ambiguous, so
                // retain the complete submitted suffix for idempotent replay.
                return Err(journal_and_defer(
                    &ops[offset..],
                    mutation_id,
                    primary.to_string(),
                ));
            }
        };

        let applied = match outcome {
            VectorSyncBatchOutcome::Progress { applied } => {
                let applied = applied as usize;
                if applied == 0 {
                    return Err(journal_and_defer(
                        &ops[offset..],
                        mutation_id,
                        "vector batch made no progress".into(),
                    ));
                }
                applied
            }
            VectorSyncBatchOutcome::Terminal { applied, .. } => {
                let applied = applied as usize;
                offset += applied;
                return Err(journal_and_defer(
                    &ops[offset..],
                    mutation_id,
                    "vector batch reached terminal admission result".into(),
                ));
            }
        };
        offset += applied;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use async_trait::async_trait;
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{VectorEncoding, VectorMetric, VectorSubject};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FlakyVectorCanister {
        fail_after: usize,
        upserts: AtomicUsize,
        removes: AtomicUsize,
        seen: std::sync::Mutex<Vec<VectorEmbeddingSyncOp>>,
    }

    impl FlakyVectorCanister {
        fn new(fail_after: usize) -> Self {
            Self {
                fail_after,
                upserts: AtomicUsize::new(0),
                removes: AtomicUsize::new(0),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait(?Send)]
    impl VectorCanisterLookup for FlakyVectorCanister {
        async fn vector_sync_batch_outcome(
            &self,
            operations: Vec<VectorEmbeddingSyncOp>,
        ) -> Result<VectorSyncBatchOutcome, PlanQueryError> {
            self.seen.lock().unwrap().extend(operations.iter().cloned());
            for operation in operations.iter().cloned() {
                if operation.remove {
                    self.vector_remove(operation).await?;
                } else {
                    self.vector_upsert(operation).await?;
                }
            }
            Ok(VectorSyncBatchOutcome::Progress {
                applied: operations.len() as u32,
            })
        }

        async fn vector_upsert(&self, _op: VectorEmbeddingSyncOp) -> Result<(), PlanQueryError> {
            let n = self.upserts.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.fail_after {
                return Err(PlanQueryError::UnsupportedOp("test_vector_upsert_fail"));
            }
            Ok(())
        }

        async fn vector_remove(&self, _op: VectorEmbeddingSyncOp) -> Result<(), PlanQueryError> {
            self.removes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn upsert_op(vertex_id: u32, version: u64) -> VectorEmbeddingSyncOp {
        VectorEmbeddingSyncOp {
            index_id: 1,
            embedding_name_id: 1,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id,
            },
            mutation_id: version,
            encoding: VectorEncoding::F32,
            dims: 1,
            metric: VectorMetric::L2Squared,
            bytes: vec![0xa0, vertex_id as u8, version as u8, 0x5a],
            remove: false,
        }
    }

    fn with_routing<R>(body: impl FnOnce(&GraphStore) -> R) -> R {
        let graph = GraphStore::new();
        graph
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_canister: Some(Principal::management_canister()),
            }))
            .expect("set routing");
        for (seq, _) in graph.repair_journal_peek(usize::MAX) {
            graph.repair_journal_remove(seq);
        }
        clear_pending();
        let out = body(&graph);
        for (seq, _) in graph.repair_journal_peek(usize::MAX) {
            graph.repair_journal_remove(seq);
        }
        clear_pending();
        graph.set_federation_routing(None).expect("clear routing");
        out
    }

    #[test]
    fn take_pending_as_repair_preserves_vector_fence() {
        with_routing(|_| {
            let op = upsert_op(7, 3);
            PENDING.with(|p| p.borrow_mut().push(op.clone()));

            assert_eq!(
                take_pending_as_repair(),
                vec![RepairPostingOp::VectorEmbedding { op }]
            );
            assert!(PENDING.with(|p| p.borrow().is_empty()));
        });
    }

    #[test]
    fn flush_delivers_all_ops_in_order() {
        with_routing(|graph| {
            let vx = FlakyVectorCanister::new(0);
            let expected = [upsert_op(1, 11), upsert_op(2, 12)];
            PENDING.with(|p| p.borrow_mut().extend(expected.clone()));
            pollster::block_on(flush_pending(Some(&vx), None)).expect("flush succeeds");
            assert_eq!(vx.upserts.load(Ordering::SeqCst), 2);
            assert_eq!(*vx.seen.lock().unwrap(), expected);
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn partial_failure_journals_whole_batch_without_compensation() {
        with_routing(|graph| {
            let vx = FlakyVectorCanister::new(2);
            let expected = [upsert_op(1, 21), upsert_op(2, 22)];
            PENDING.with(|p| p.borrow_mut().extend(expected.clone()));
            let err = pollster::block_on(flush_pending(Some(&vx), Some(42)))
                .expect_err("second upsert fails");
            assert!(matches!(err, PlanQueryError::IndexFlushDeferred { .. }));
            // No compensating removes were issued.
            assert_eq!(vx.removes.load(Ordering::SeqCst), 0);
            assert_eq!(*vx.seen.lock().unwrap(), expected);
            let journaled: Vec<RepairPostingOp> = graph
                .repair_journal_peek(16)
                .into_iter()
                .map(|(_, op)| op)
                .collect();
            assert_eq!(
                journaled,
                vec![
                    RepairPostingOp::VectorEmbedding {
                        op: expected[0].clone()
                    },
                    RepairPostingOp::VectorEmbedding {
                        op: expected[1].clone()
                    },
                ]
            );
            // The deferred batch pins the federated mutation.
            assert_eq!(graph.index_pending_min_mutation_id(), Some(42));
        });
    }

    #[test]
    fn missing_client_with_queued_ops_journals_and_defers() {
        with_routing(|graph| {
            PENDING.with(|p| p.borrow_mut().push(upsert_op(7, 1)));
            let err =
                pollster::block_on(flush_pending(None, None)).expect_err("no client → deferred");
            assert!(matches!(err, PlanQueryError::IndexFlushDeferred { .. }));
            assert!(!graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn unconfigured_shard_drops_pending() {
        let graph = GraphStore::new();
        graph.set_federation_routing(None).expect("clear routing");
        PENDING.with(|p| p.borrow_mut().push(upsert_op(1, 1)));
        pollster::block_on(flush_pending(None, None)).expect("no-op when unconfigured");
        assert!(PENDING.with(|p| p.borrow().is_empty()));
    }
}
