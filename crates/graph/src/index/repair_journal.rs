//! Re-application of the durable index repair journal (ADR 0023 D5).
//!
//! The maintenance driver calls [`drain_once`] each tick (and after
//! `post_upgrade` once the timer re-arms) to replay failed-flush postings the
//! [`crate::facade::stable::repair_journal`] persisted. Each op is re-issued to
//! graph-index and removed from the journal on success; on the first failure the
//! drain stops, leaving the remaining entries for a later tick (the index is
//! presumed unavailable). Re-application is idempotent, so no compensation is
//! needed here.

use crate::facade::{GraphStore, RepairPostingOp};
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use gleaph_graph_kernel::federation::ShardId;

/// Max journal entries re-applied per tick; bounds per-message cross-canister
/// work. Remaining entries drain on subsequent ticks.
const REPAIR_DRAIN_BATCH: usize = 128;

/// Re-applies up to [`REPAIR_DRAIN_BATCH`] oldest journal entries, removing each
/// on success. Stops (returning the error) at the first failed re-application so
/// the offending and following entries stay durable for the next tick.
pub(crate) async fn drain_once(ix: &dyn PropertyIndexLookup) -> Result<(), PlanQueryError> {
    let store = GraphStore::new();
    if !store.federation_configured() {
        return Ok(());
    }
    let shard_id = ix.local_shard_id();
    for (seq, op) in store.repair_journal_peek(REPAIR_DRAIN_BATCH) {
        apply(ix, shard_id, &op).await?;
        store.repair_journal_remove(seq);
    }
    Ok(())
}

async fn apply(
    ix: &dyn PropertyIndexLookup,
    shard_id: ShardId,
    op: &RepairPostingOp,
) -> Result<(), PlanQueryError> {
    match op {
        RepairPostingOp::VertexProperty {
            remove,
            property_id,
            payload_bytes,
            vertex_id,
        } => {
            if *remove {
                ix.posting_remove(*property_id, payload_bytes.clone(), *vertex_id)
                    .await
            } else {
                ix.posting_insert(*property_id, payload_bytes.clone(), *vertex_id)
                    .await
            }
        }
        RepairPostingOp::EdgeProperty {
            remove,
            property_id,
            payload_bytes,
            label_id,
            owner_vertex_id,
            slot_index,
        } => {
            if *remove {
                ix.edge_posting_remove_at(
                    shard_id,
                    *property_id,
                    payload_bytes.clone(),
                    *label_id,
                    *owner_vertex_id,
                    *slot_index,
                )
                .await
            } else {
                ix.edge_posting_insert_at(
                    shard_id,
                    *property_id,
                    payload_bytes.clone(),
                    *label_id,
                    *owner_vertex_id,
                    *slot_index,
                )
                .await
            }
        }
        RepairPostingOp::Label {
            remove,
            label_id,
            vertex_id,
        } => {
            if *remove {
                ix.label_posting_remove(*label_id, *vertex_id).await
            } else {
                ix.label_posting_insert(*label_id, *vertex_id).await
            }
        }
    }
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

    /// Index mock that fails the Nth `posting_insert_at` (1-based) and counts
    /// successful re-applications, so a drain can be observed mid-batch.
    struct CountingIndex {
        fail_insert_at: usize,
        inserts: AtomicUsize,
    }

    impl CountingIndex {
        fn new(fail_insert_at: usize) -> Self {
            Self {
                fail_insert_at,
                inserts: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for CountingIndex {
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

        fn local_shard_id(&self) -> ShardId {
            ShardId::new(0)
        }

        async fn posting_insert_at(
            &self,
            _shard_id: ShardId,
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
            }))
            .expect("set routing");
        for (seq, _) in graph.repair_journal_peek(usize::MAX) {
            graph.repair_journal_remove(seq);
        }
        let out = body(&graph);
        for (seq, _) in graph.repair_journal_peek(usize::MAX) {
            graph.repair_journal_remove(seq);
        }
        graph.set_federation_routing(None).expect("clear routing");
        out
    }

    #[test]
    fn drain_reapplies_all_and_clears_journal() {
        with_routing(|graph| {
            graph.repair_journal_append([vertex_insert(1), vertex_insert(2), vertex_insert(3)]);
            let index = CountingIndex::new(0);
            pollster::block_on(drain_once(&index)).expect("drain succeeds");
            assert_eq!(index.inserts.load(Ordering::SeqCst), 3);
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn drain_stops_at_failure_and_retains_remaining() {
        with_routing(|graph| {
            graph.repair_journal_append([vertex_insert(1), vertex_insert(2), vertex_insert(3)]);
            // Fail the 2nd insert: the 1st is removed, the 2nd and 3rd persist.
            let index = CountingIndex::new(2);
            let err = pollster::block_on(drain_once(&index)).expect_err("drain stops");
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
            pollster::block_on(drain_once(&healthy)).expect("second drain succeeds");
            assert!(graph.repair_journal_is_empty());
        });
    }
}
