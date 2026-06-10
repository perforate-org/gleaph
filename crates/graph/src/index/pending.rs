//! Record vertex property changes for the federated index canister.
//!
//! ## Index posting keys (`payload_bytes`)
//!
//! Each [`PendingPostingOp`] carries a `payload_bytes` field: the **sortable property-index key** for
//! that snapshot of the property value, from [`gleaph_gql::value_to_index_key_bytes`]. The federated
//! index uses these bytes (with `property_id`) for equality and range lookups.
//!
//! [`record_vertex_property_change`] queues postings only when
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
//! index matches its pre-flush state for this batch, then the full batch is re-queued for a later
//! retry. If compensation itself fails, the canister **traps** on Wasm (there is no safe automatic
//! recovery across two canisters).

use crate::facade::GraphStore;
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use crate::property::{PropertyIndexOp, PropertyValueChange, index_ops_for_value_change};
use gleaph_graph_kernel::entry::PropertyEntity;
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

pub(crate) fn record_vertex_property_change(change: PropertyValueChange<'_>) {
    let PropertyEntity::Vertex(vertex_id) = change.entity else {
        return;
    };
    let vid = u32::try_from(u64::from(vertex_id)).unwrap_or(0);
    for op in index_ops_for_value_change(change.property_id, change.prev, change.new) {
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
) -> Result<(), PlanQueryError> {
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
                    PENDING.with(|p| p.borrow_mut().extend(ops.iter().cloned()));
                    return Err(primary);
                }
                Err(rollback_err) => {
                    #[cfg(target_family = "wasm")]
                    ic_cdk::trap(&format!(
                        "gleaph-graph: federated index sync failed and rollback failed (op error: {primary}; rollback: {rollback_err})"
                    ));
                    #[cfg(not(target_family = "wasm"))]
                    {
                        return Err(PlanQueryError::FederatedIndexCall {
                            op: "compensate",
                            detail: format!("primary: {primary}; rollback: {rollback_err}"),
                        });
                    }
                }
            }
        }
        applied.push(op.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use async_trait::async_trait;
    use candid::Principal;
    use gleaph_graph_kernel::index::{IndexIntersectionRequest, PostingHit, PostingRangeRequest};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FlakyIndex {
        fail_after: usize,
        insert_calls: AtomicUsize,
        remove_calls: AtomicUsize,
    }

    impl FlakyIndex {
        fn new(fail_after: usize) -> Self {
            Self {
                fail_after,
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
        ) -> Result<Vec<PostingHit>, PlanQueryError> {
            Ok(vec![])
        }

        fn local_shard_id(&self) -> u32 {
            0
        }

        async fn posting_insert_at(
            &self,
            _shard_id: u32,
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
            _shard_id: u32,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            self.remove_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn label_posting_insert_at(
            &self,
            _shard_id: u32,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }

        async fn label_posting_remove_at(
            &self,
            _shard_id: u32,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }
    }

    #[test]
    fn flush_requeues_full_batch_after_partial_insert_failure() {
        let index = FlakyIndex::new(2);
        let graph = GraphStore::new();
        graph
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 0,
            }))
            .expect("set routing");

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

        let err = pollster::block_on(flush_pending(Some(&index))).expect_err("second insert fails");
        assert!(err.to_string().contains("test_insert_fail"));

        assert_eq!(index.insert_calls.load(Ordering::SeqCst), 2);
        assert_eq!(index.remove_calls.load(Ordering::SeqCst), 1);

        let restored = PENDING.with(|p| p.borrow().clone());
        assert_eq!(restored.len(), 2);
        assert!(matches!(
            &restored[0],
            PendingPostingOp::Insert {
                property_id: 1,
                payload_bytes,
                vertex_id: 1
            } if payload_bytes == &[10]
        ));
        assert!(matches!(
            &restored[1],
            PendingPostingOp::Insert {
                property_id: 1,
                payload_bytes,
                vertex_id: 2
            } if payload_bytes == &[11]
        ));

        graph.set_federation_routing(None).expect("clear routing");
        clear_pending();
    }
}
