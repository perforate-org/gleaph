//! Record derived vertex-embedding mutations for the `graph-vector-index` canister (ADR 0031).
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
use crate::index::vector_lookup::VectorIndexLookup;
use crate::plan::PlanQueryError;
use gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp;
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
    vector: Option<&dyn VectorIndexLookup>,
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

    for op in &ops {
        let result = if op.remove {
            vx.vector_remove(op.clone()).await
        } else {
            vx.vector_upsert(op.clone()).await
        };
        if let Err(primary) = result {
            // No compensation: re-applying the already-delivered prefix is an idempotent no-op.
            return Err(journal_and_defer(&ops, mutation_id, primary.to_string()));
        }
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

    struct FlakyVectorIndex {
        fail_after: usize,
        upserts: AtomicUsize,
        removes: AtomicUsize,
    }

    impl FlakyVectorIndex {
        fn new(fail_after: usize) -> Self {
            Self {
                fail_after,
                upserts: AtomicUsize::new(0),
                removes: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait(?Send)]
    impl VectorIndexLookup for FlakyVectorIndex {
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
            embedding_incarnation: 1,
            embedding_version: version,
            encoding: VectorEncoding::F32,
            dims: 1,
            metric: VectorMetric::L2Squared,
            bytes: vec![0, 0, 0, 0],
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
                vector_index_canister: Some(Principal::management_canister()),
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
    fn flush_delivers_all_ops_in_order() {
        with_routing(|graph| {
            let vx = FlakyVectorIndex::new(0);
            PENDING.with(|p| p.borrow_mut().extend([upsert_op(1, 1), upsert_op(2, 1)]));
            pollster::block_on(flush_pending(Some(&vx), None)).expect("flush succeeds");
            assert_eq!(vx.upserts.load(Ordering::SeqCst), 2);
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn partial_failure_journals_whole_batch_without_compensation() {
        with_routing(|graph| {
            let vx = FlakyVectorIndex::new(2);
            PENDING.with(|p| p.borrow_mut().extend([upsert_op(1, 1), upsert_op(2, 1)]));
            let err = pollster::block_on(flush_pending(Some(&vx), Some(42)))
                .expect_err("second upsert fails");
            assert!(matches!(err, PlanQueryError::IndexFlushDeferred { .. }));
            // No compensating removes were issued.
            assert_eq!(vx.removes.load(Ordering::SeqCst), 0);
            let journaled: Vec<RepairPostingOp> = graph
                .repair_journal_peek(16)
                .into_iter()
                .map(|(_, op)| op)
                .collect();
            assert_eq!(
                journaled,
                vec![
                    RepairPostingOp::VectorEmbedding {
                        op: upsert_op(1, 1)
                    },
                    RepairPostingOp::VectorEmbedding {
                        op: upsert_op(2, 1)
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
