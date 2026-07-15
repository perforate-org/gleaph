//! Record vertex property changes for the federated index canister.
//!
//! ## Index posting keys (`payload_bytes`)
//!
//! Each [`PendingPostingOp`] carries a `payload_bytes` field: the **sortable property-index key** for
//! that snapshot of the property value, from [`gleaph_gql::value_to_index_key_bytes`]. The federated
//! index uses these bytes (with `property_id`) for equality and range lookups.
//!
//! [`crate::property::dispatch_property_index_ops`] queues postings only when
//! [`gleaph_gql::value_to_index_key_bytes`] returns `Ok(Some(key))`. For `Ok(None)` or `Err`, no
//! insert/remove is queued for that snapshot:
//!
//! - `Ok(None)` is produced only for [`gleaph_gql::Value::Null`] (nulls are absent from the index).
//! - `Err` covers non-finite floats, values with no index-key encoding, extensions without
//!   [`gleaph_gql::ExtensionValue::sortable_index_key`], and similar cases.
//!
//! Vertices in those situations remain in the primary property store but can be missed by
//! index-only equality or range scans.
//!
//! ## Persistence vs index
//!
//! Stable vertex storage serializes [`gleaph_gql::Value`] with [`gleaph_gql::Value::to_binary_bytes`].
//! That encoding is **not** what appears in `payload_bytes` here. A value can be persisted on the graph
//! while producing no postings when [`gleaph_gql::value_to_index_key_bytes`] returns `None` or `Err`.
//! Extensions such as [`gleaph_gql_ic::PrincipalValue`] participate in the index when they supply a
//! sortable key so [`gleaph_gql::value_to_index_key_bytes`] succeeds.
//!
//! ## Sync failure semantics
//!
//! [`flush_pending`] applies postings in order. If an inter-canister call fails after earlier
//! calls succeeded, successful prefix operations are **compensated** (inverse postings) so the
//! index matches its pre-flush state for this batch, then the full batch is persisted to the
//! durable repair journal ([`crate::facade::stable::repair_journal`], ADR 0023 D5) for the
//! maintenance driver to re-apply. If compensation itself **also** fails, the canister no longer
//! traps (ADR 0023 P4): the full batch is journaled all the same, since idempotent re-application
//! converges the index to the store regardless of the partial compensation state.

use crate::facade::{GraphStore, RepairPostingOp};
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use crate::property::PropertyIndexOp;
use gleaph_graph_kernel::index::IndexPostingMutation;
use ic_stable_lara::VertexId;
use std::cell::RefCell;

#[derive(Clone, Debug)]
pub(crate) enum PendingPostingOp {
    Insert {
        property_id: u32,
        payload_bytes: Vec<u8>,
        vertex_id: u32,
    },
    Remove {
        property_id: u32,
        payload_bytes: Vec<u8>,
        vertex_id: u32,
    },
}

thread_local! {
    static PENDING: RefCell<Vec<PendingPostingOp>> = const { RefCell::new(Vec::new()) };
}

/// Clears the posting queue (e.g. when disabling the index). Not invoked at the start of each GQL
/// run: [`flush_pending`] may re-queue work after a partial failure so a later update can retry.
pub(crate) fn clear_pending() {
    PENDING.with(|p| p.borrow_mut().clear());
}

fn push(op: PendingPostingOp) {
    if !GraphStore::new().federation_configured() {
        return;
    }
    PENDING.with(|p| p.borrow_mut().push(op));
}

pub(crate) fn take_pending() -> Vec<PendingPostingOp> {
    PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

pub(crate) fn to_repair_op(op: &PendingPostingOp) -> RepairPostingOp {
    match op {
        PendingPostingOp::Insert {
            property_id,
            payload_bytes,
            vertex_id,
        } => RepairPostingOp::VertexProperty {
            remove: false,
            property_id: *property_id,
            payload_bytes: payload_bytes.clone(),
            vertex_id: *vertex_id,
        },
        PendingPostingOp::Remove {
            property_id,
            payload_bytes,
            vertex_id,
        } => RepairPostingOp::VertexProperty {
            remove: true,
            property_id: *property_id,
            payload_bytes: payload_bytes.clone(),
            vertex_id: *vertex_id,
        },
    }
}

pub(crate) fn to_index_mutation(op: &PendingPostingOp) -> IndexPostingMutation {
    match op {
        PendingPostingOp::Insert {
            property_id,
            payload_bytes,
            vertex_id,
        } => IndexPostingMutation::VertexProperty {
            remove: false,
            property_id: *property_id,
            value: payload_bytes.clone(),
            vertex_id: *vertex_id,
        },
        PendingPostingOp::Remove {
            property_id,
            payload_bytes,
            vertex_id,
        } => IndexPostingMutation::VertexProperty {
            remove: true,
            property_id: *property_id,
            value: payload_bytes.clone(),
            vertex_id: *vertex_id,
        },
    }
}

pub(crate) fn push_vertex_index_op(vertex_id: VertexId, op: PropertyIndexOp) {
    let vid = u32::try_from(u64::from(vertex_id)).unwrap_or(0);
    let pending = match op {
        PropertyIndexOp::Insert {
            property_id,
            payload_bytes,
        } => PendingPostingOp::Insert {
            property_id: property_id.raw(),
            payload_bytes,
            vertex_id: vid,
        },
        PropertyIndexOp::Remove {
            property_id,
            payload_bytes,
        } => PendingPostingOp::Remove {
            property_id: property_id.raw(),
            payload_bytes,
            vertex_id: vid,
        },
    };
    push(pending);
}

async fn compensate_index_ops(
    ix: &dyn PropertyIndexLookup,
    applied: &[PendingPostingOp],
) -> Result<(), PlanQueryError> {
    for op in applied.iter().rev() {
        match op {
            PendingPostingOp::Insert {
                property_id,
                payload_bytes,
                vertex_id,
            } => {
                ix.posting_remove(*property_id, payload_bytes.clone(), *vertex_id)
                    .await?;
            }
            PendingPostingOp::Remove {
                property_id,
                payload_bytes,
                vertex_id,
            } => {
                ix.posting_insert(*property_id, payload_bytes.clone(), *vertex_id)
                    .await?;
            }
        }
    }
    Ok(())
}

pub(crate) async fn flush_pending(
    index: Option<&dyn PropertyIndexLookup>,
    mutation_id: Option<u64>,
) -> Result<(), PlanQueryError> {
    let mutation_id = mutation_id.unwrap_or(0);
    if !GraphStore::new().federation_configured() {
        clear_pending();
        return Ok(());
    }
    let Some(ix) = index else {
        clear_pending();
        return Err(PlanQueryError::UnsupportedOp(
            "index mutations dropped (no index client)",
        ));
    };
    let ops: Vec<PendingPostingOp> = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    if ops.is_empty() {
        return Ok(());
    }

    if ix.supports_posting_batch() {
        let mut offset = 0usize;
        while offset < ops.len() {
            let operations = ops[offset..].iter().map(to_index_mutation).collect();
            let progress = match ix.posting_batch_at(ix.local_shard_id(), operations).await {
                Ok(progress) => progress,
                Err(error) => {
                    GraphStore::new()
                        .repair_journal_append(mutation_id, ops[offset..].iter().map(to_repair_op));
                    crate::facade::maintenance_timer::arm_if_needed();
                    return Err(PlanQueryError::IndexFlushDeferred {
                        op: "vertex_batch",
                        detail: error.to_string(),
                    });
                }
            };
            let advanced = usize::try_from(progress.applied).unwrap_or(0);
            if advanced == 0 || advanced > ops.len().saturating_sub(offset) {
                GraphStore::new()
                    .repair_journal_append(mutation_id, ops[offset..].iter().map(to_repair_op));
                crate::facade::maintenance_timer::arm_if_needed();
                return Err(PlanQueryError::IndexFlushDeferred {
                    op: "vertex_batch_budget",
                    detail: "index batch made no progress".into(),
                });
            }
            offset = offset.saturating_add(advanced);
            if progress.next_index.is_none() {
                return if offset == ops.len() {
                    Ok(())
                } else {
                    GraphStore::new()
                        .repair_journal_append(mutation_id, ops[offset..].iter().map(to_repair_op));
                    crate::facade::maintenance_timer::arm_if_needed();
                    Err(PlanQueryError::IndexFlushDeferred {
                        op: "vertex_batch_progress",
                        detail: "index batch returned an invalid terminal progress".into(),
                    })
                };
            }
        }
        return Ok(());
    }

    let mut applied: Vec<PendingPostingOp> = Vec::with_capacity(ops.len());
    for op in &ops {
        let result = match op {
            PendingPostingOp::Insert {
                property_id,
                payload_bytes,
                vertex_id,
            } => {
                ix.posting_insert(*property_id, payload_bytes.clone(), *vertex_id)
                    .await
            }
            PendingPostingOp::Remove {
                property_id,
                payload_bytes,
                vertex_id,
            } => {
                ix.posting_remove(*property_id, payload_bytes.clone(), *vertex_id)
                    .await
            }
        };

        if let Err(primary) = result {
            match compensate_index_ops(ix, &applied).await {
                Ok(()) => {
                    // Index is back at its pre-batch state; persist the whole
                    // batch durably (ADR 0023 D5) so the delta survives upgrade /
                    // trap, and arm the timer to re-apply it. The batch is durable
                    // and the index converges asynchronously (ADR 0024).
                    GraphStore::new()
                        .repair_journal_append(mutation_id, ops.iter().map(to_repair_op));
                    crate::facade::maintenance_timer::arm_if_needed();
                    return Err(PlanQueryError::IndexFlushDeferred {
                        op: "vertex_flush",
                        detail: primary.to_string(),
                    });
                }
                Err(rollback_err) => {
                    // Compensation failed: the index is in an unknown partial
                    // state for this batch. Do not trap (ADR 0023 P4) — persist
                    // the full batch so idempotent re-application converges the
                    // index to the store (ADR 0024), then surface the deferred error.
                    GraphStore::new()
                        .repair_journal_append(mutation_id, ops.iter().map(to_repair_op));
                    crate::facade::maintenance_timer::arm_if_needed();
                    return Err(PlanQueryError::IndexFlushDeferred {
                        op: "vertex_compensate",
                        detail: format!(
                            "primary: {primary}; rollback: {rollback_err}; batch journaled for repair"
                        ),
                    });
                }
            }
        }
        applied.push(op.clone());
    }
    Ok(())
}

/// Flushes all property-index pending queues through one target batch when the
/// concrete client supports the bounded batch protocol. Legacy/native test
/// clients retain the original per-queue semantics through the existing flush
/// functions.
pub(crate) async fn flush_all_pending(
    index: Option<&dyn PropertyIndexLookup>,
    mutation_id: Option<u64>,
) -> Result<(), PlanQueryError> {
    let Some(ix) = index else {
        flush_pending(None, mutation_id).await?;
        return Ok(());
    };
    if !ix.supports_posting_batch() {
        flush_pending(Some(ix), mutation_id).await?;
        crate::index::edge_pending::flush_pending(Some(ix), mutation_id).await?;
        crate::index::label_pending::flush_pending(Some(ix), mutation_id).await?;
        return Ok(());
    }

    let mut ops: Vec<(IndexPostingMutation, RepairPostingOp)> = take_pending()
        .into_iter()
        .map(|op| (to_index_mutation(&op), to_repair_op(&op)))
        .collect();
    ops.extend(
        crate::index::edge_pending::take_pending()
            .into_iter()
            .map(|op| {
                (
                    crate::index::edge_pending::to_index_mutation(&op),
                    crate::index::edge_pending::to_repair_op(&op),
                )
            }),
    );
    ops.extend(
        crate::index::label_pending::take_pending()
            .into_iter()
            .map(|op| {
                (
                    crate::index::label_pending::to_index_mutation(&op),
                    crate::index::label_pending::to_repair_op(&op),
                )
            }),
    );
    if ops.is_empty() {
        return Ok(());
    }

    let mut offset = 0usize;
    while offset < ops.len() {
        let mutations = ops[offset..]
            .iter()
            .map(|(mutation, _)| mutation.clone())
            .collect();
        let progress = match ix.posting_batch_at(ix.local_shard_id(), mutations).await {
            Ok(progress) => progress,
            Err(error) => {
                GraphStore::new().repair_journal_append(
                    mutation_id.unwrap_or(0),
                    ops[offset..].iter().map(|(_, repair)| repair.clone()),
                );
                crate::facade::maintenance_timer::arm_if_needed();
                return Err(PlanQueryError::IndexFlushDeferred {
                    op: "property_batch",
                    detail: error.to_string(),
                });
            }
        };
        let applied = usize::try_from(progress.applied).unwrap_or(0);
        if applied == 0 || applied > ops.len().saturating_sub(offset) {
            GraphStore::new().repair_journal_append(
                mutation_id.unwrap_or(0),
                ops[offset..].iter().map(|(_, repair)| repair.clone()),
            );
            crate::facade::maintenance_timer::arm_if_needed();
            return Err(PlanQueryError::IndexFlushDeferred {
                op: "property_batch_progress",
                detail: "index batch made invalid progress".into(),
            });
        }
        offset += applied;
        if progress.next_index.is_none() {
            return if offset == ops.len() {
                Ok(())
            } else {
                GraphStore::new().repair_journal_append(
                    mutation_id.unwrap_or(0),
                    ops[offset..].iter().map(|(_, repair)| repair.clone()),
                );
                crate::facade::maintenance_timer::arm_if_needed();
                Err(PlanQueryError::IndexFlushDeferred {
                    op: "property_batch_progress",
                    detail: "index batch returned an invalid terminal progress".into(),
                })
            };
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
    use gleaph_graph_kernel::index::{IndexIntersectionRequest, PostingHit, PostingRangeRequest};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FlakyIndex {
        fail_after: usize,
        fail_remove: bool,
        insert_calls: AtomicUsize,
        remove_calls: AtomicUsize,
    }

    impl FlakyIndex {
        fn new(fail_after: usize) -> Self {
            Self {
                fail_after,
                fail_remove: false,
                insert_calls: AtomicUsize::new(0),
                remove_calls: AtomicUsize::new(0),
            }
        }

        /// Also fail `posting_remove_at`, so the compensation step itself fails.
        fn with_failing_remove(fail_after: usize) -> Self {
            Self {
                fail_after,
                fail_remove: true,
                insert_calls: AtomicUsize::new(0),
                remove_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for FlakyIndex {
        async fn lookup_equal(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Result<Vec<PostingHit>, PlanQueryError> {
            Ok(vec![])
        }

        async fn lookup_range(
            &self,
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

        fn local_shard_id(&self) -> gleaph_graph_kernel::federation::ShardId {
            gleaph_graph_kernel::federation::ShardId::new(0)
        }

        async fn posting_insert_at(
            &self,
            _shard_id: gleaph_graph_kernel::federation::ShardId,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            let n = self.insert_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.fail_after {
                return Err(PlanQueryError::UnsupportedOp("test_insert_fail"));
            }
            Ok(())
        }

        async fn posting_remove_at(
            &self,
            _shard_id: gleaph_graph_kernel::federation::ShardId,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            self.remove_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_remove {
                return Err(PlanQueryError::UnsupportedOp("test_remove_fail"));
            }
            Ok(())
        }

        async fn label_posting_insert_at(
            &self,
            _shard_id: gleaph_graph_kernel::federation::ShardId,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }

        async fn label_posting_remove_at(
            &self,
            _shard_id: gleaph_graph_kernel::federation::ShardId,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }
    }

    #[test]
    fn flush_journals_full_batch_after_partial_insert_failure() {
        let index = FlakyIndex::new(2);
        let graph = GraphStore::new();
        graph
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_index_canister: None,
            }))
            .expect("set routing");
        // Start from an empty journal so the assertions below are exact.
        drain_test_journal(&graph);

        PENDING.with(|p| {
            p.borrow_mut().extend([
                PendingPostingOp::Insert {
                    property_id: 1,
                    payload_bytes: vec![10],
                    vertex_id: 1,
                },
                PendingPostingOp::Insert {
                    property_id: 1,
                    payload_bytes: vec![11],
                    vertex_id: 2,
                },
            ]);
        });

        let err =
            pollster::block_on(flush_pending(Some(&index), None)).expect_err("second insert fails");
        assert!(err.to_string().contains("test_insert_fail"));

        assert_eq!(index.insert_calls.load(Ordering::SeqCst), 2);
        assert_eq!(index.remove_calls.load(Ordering::SeqCst), 1);

        // Compensation succeeded, so the whole batch is persisted to the durable
        // repair journal (not the volatile queue) for later re-application.
        assert!(PENDING.with(|p| p.borrow().is_empty()));
        let journaled: Vec<RepairPostingOp> = graph
            .repair_journal_peek(16)
            .into_iter()
            .map(|(_, op)| op)
            .collect();
        assert_eq!(
            journaled,
            vec![
                RepairPostingOp::VertexProperty {
                    remove: false,
                    property_id: 1,
                    payload_bytes: vec![10],
                    vertex_id: 1,
                },
                RepairPostingOp::VertexProperty {
                    remove: false,
                    property_id: 1,
                    payload_bytes: vec![11],
                    vertex_id: 2,
                },
            ]
        );

        drain_test_journal(&graph);
        graph.set_federation_routing(None).expect("clear routing");
        clear_pending();
    }

    fn drain_test_journal(graph: &GraphStore) {
        for (seq, _) in graph.repair_journal_peek(usize::MAX) {
            graph.repair_journal_remove(seq);
        }
    }

    #[test]
    fn deferred_flush_links_repair_batch_to_mutation_id() {
        // ADR 0029 Phase 2: a flush that defers to the repair journal under a federated
        // mutation pins that mutation in the index watermark until the batch drains.
        let index = FlakyIndex::new(1);
        let graph = GraphStore::new();
        graph
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_index_canister: None,
            }))
            .expect("set routing");
        drain_test_journal(&graph);
        assert_eq!(graph.index_pending_min_mutation_id(), None);

        PENDING.with(|p| {
            p.borrow_mut().push(PendingPostingOp::Insert {
                property_id: 1,
                payload_bytes: vec![10],
                vertex_id: 1,
            });
        });
        let err = pollster::block_on(flush_pending(Some(&index), Some(55)))
            .expect_err("first insert fails");
        assert!(matches!(err, PlanQueryError::IndexFlushDeferred { .. }));
        // The deferred batch is now linked to mutation 55.
        assert_eq!(graph.index_pending_min_mutation_id(), Some(55));

        drain_test_journal(&graph);
        graph.set_federation_routing(None).expect("clear routing");
        clear_pending();
    }

    #[test]
    fn compensation_failure_journals_batch_without_trapping() {
        // Insert fails on the 2nd op; compensation (remove of the 1st insert)
        // also fails. Pre-ADR-0023 this trapped on wasm; now the whole batch is
        // journaled for idempotent re-application (ADR 0023 P4).
        let index = FlakyIndex::with_failing_remove(2);
        let graph = GraphStore::new();
        graph
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_index_canister: None,
            }))
            .expect("set routing");
        drain_test_journal(&graph);

        PENDING.with(|p| {
            p.borrow_mut().extend([
                PendingPostingOp::Insert {
                    property_id: 7,
                    payload_bytes: vec![20],
                    vertex_id: 3,
                },
                PendingPostingOp::Remove {
                    property_id: 7,
                    payload_bytes: vec![21],
                    vertex_id: 4,
                },
            ]);
        });

        let err = pollster::block_on(flush_pending(Some(&index), None))
            .expect_err("primary + compensation both fail");
        assert!(err.to_string().contains("batch journaled for repair"));

        let journaled: Vec<RepairPostingOp> = graph
            .repair_journal_peek(16)
            .into_iter()
            .map(|(_, op)| op)
            .collect();
        assert_eq!(
            journaled,
            vec![
                RepairPostingOp::VertexProperty {
                    remove: false,
                    property_id: 7,
                    payload_bytes: vec![20],
                    vertex_id: 3,
                },
                RepairPostingOp::VertexProperty {
                    remove: true,
                    property_id: 7,
                    payload_bytes: vec![21],
                    vertex_id: 4,
                },
            ]
        );

        drain_test_journal(&graph);
        graph.set_federation_routing(None).expect("clear routing");
        clear_pending();
    }
}
