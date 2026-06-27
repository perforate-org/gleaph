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
//! Vector ops (ADR 0031) are not replayed verbatim. Because the canonical Graph
//! store resets `embedding_version` to `1` on re-insert, a stored vector op's
//! version alone cannot be ordered against the vector canister's clock. Instead
//! each vector entry is *reconciled* against the canonical store at drain time
//! (canonical wins): if the subject still owns the embedding we deliver a current
//! upsert re-derived with the canonical `(embedding_incarnation, embedding_version)`;
//! if it was deleted we deliver a remove stamped with the persisted (delete-spanning)
//! incarnation. Since the incarnation strictly increases across each reinsert and the
//! vector canister orders by `(incarnation, version)` (ADR 0031 Slice 4), the
//! reconcile remove is incarnation-fenced: it can no longer tombstone a newer
//! reinsert that raced ahead of it. A vector entry with no configured vector client
//! is skipped (left durable) so it never wedges the property repairs queued after it.

use crate::facade::{GraphStore, RepairPostingOp};
use crate::index::lookup::PropertyIndexLookup;
use crate::index::vector_lookup::VectorIndexLookup;
use crate::plan::PlanQueryError;
use gleaph_graph_kernel::entry::EmbeddingNameId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{VectorEmbeddingSyncOp, VectorSubject};
use ic_stable_lara::VertexId;

/// Max journal entries re-applied per tick; bounds per-message cross-canister
/// work. Remaining entries drain on subsequent ticks.
const REPAIR_DRAIN_BATCH: usize = 128;

/// `embedding_version` stamped on a reconcile-driven remove when the canonical
/// store no longer owns the subject. The journaled op's own version cannot be
/// trusted: it may be *older* than a newer live slot of the **same incarnation**
/// already in the vector index, in which case the canister would no-op the remove
/// (`version < clock`) and the derived vector would be orphaned once the drain
/// drops the journal entry. A canonical-wins removal therefore uses the maximum
/// version so it supersedes any live slot **within its incarnation**. The remove
/// also carries the canonical delete-spanning incarnation (ADR 0031 Slice 4), so a
/// strictly newer reinsert (higher incarnation) is never tombstoned by this remove.
const RECONCILE_TOMBSTONE_VERSION: u64 = u64::MAX;

/// Outcome of re-applying a single journal entry.
enum ApplyOutcome {
    /// The entry was delivered (or reconciled away); remove it from the journal.
    Applied,
    /// The entry could not be delivered yet but must not block the drain; leave
    /// it durable for a later tick.
    Skipped,
}

/// Re-applies up to [`REPAIR_DRAIN_BATCH`] oldest journal entries, removing each
/// applied entry. Stops (returning the error) at the first failed re-application
/// so the offending and following entries stay durable for the next tick.
/// Skipped entries (e.g. a vector op with no vector client) are left durable but
/// do not stop the drain.
pub(crate) async fn drain_once(
    ix: &dyn PropertyIndexLookup,
    vector: Option<&dyn VectorIndexLookup>,
) -> Result<(), PlanQueryError> {
    let store = GraphStore::new();
    if !store.federation_configured() {
        return Ok(());
    }
    let shard_id = ix.local_shard_id();
    for (seq, op) in store.repair_journal_peek(REPAIR_DRAIN_BATCH) {
        match apply(ix, vector, shard_id, &op).await? {
            ApplyOutcome::Applied => store.repair_journal_remove(seq),
            ApplyOutcome::Skipped => {}
        }
    }
    Ok(())
}

async fn apply(
    ix: &dyn PropertyIndexLookup,
    vector: Option<&dyn VectorIndexLookup>,
    shard_id: ShardId,
    op: &RepairPostingOp,
) -> Result<ApplyOutcome, PlanQueryError> {
    match op {
        RepairPostingOp::VertexProperty {
            remove,
            property_id,
            payload_bytes,
            vertex_id,
        } => {
            if *remove {
                ix.posting_remove(*property_id, payload_bytes.clone(), *vertex_id)
                    .await?;
            } else {
                ix.posting_insert(*property_id, payload_bytes.clone(), *vertex_id)
                    .await?;
            }
            Ok(ApplyOutcome::Applied)
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
                .await?;
            } else {
                ix.edge_posting_insert_at(
                    shard_id,
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
            reconcile_vector_op(vx, op).await?;
            Ok(ApplyOutcome::Applied)
        }
    }
}

/// Reconciles a journaled vector op against the canonical Graph store (canonical
/// wins) and delivers the current truth: a fresh upsert if the subject still owns
/// the embedding, otherwise a remove. This discards stale upserts whose subject
/// was deleted, so they cannot resurrect a tombstoned vector.
///
/// Both branches re-derive the canonical delete-spanning `embedding_incarnation`
/// (ADR 0031 Slice 4) rather than trusting the journaled op:
///
/// - **Present** -> upsert with the canonical `(incarnation, version)`. If a
///   delete + reinsert happened since the op was journaled, `incarnation_for` now
///   returns the *new* incarnation, so the replay cannot regress the clock.
/// - **Absent** -> remove stamped with the persisted (deleted) incarnation and
///   `embedding_version = RECONCILE_TOMBSTONE_VERSION`. Because the incarnation
///   strictly increases on reinsert and the vector canister orders by
///   `(incarnation, version)`, a remove for the deleted incarnation can never
///   tombstone a newer reinsert that raced ahead of the drain. This closes the
///   reverse-orphan race that made the Slice 2 blind remove unsafe to activate.
async fn reconcile_vector_op(
    vx: &dyn VectorIndexLookup,
    op: &VectorEmbeddingSyncOp,
) -> Result<(), PlanQueryError> {
    let VectorSubject::Vertex { vertex_id, .. } = op.subject;
    let vid = VertexId::from(vertex_id);
    let name = EmbeddingNameId::from_raw(op.embedding_name_id);
    let store = GraphStore::new();
    match store.vertex_embedding(vid, name) {
        Some(record) => {
            // A live record always has an incarnation; fall back to the op's stamped incarnation
            // for any pre-Slice-4 record that predates the incarnation map.
            let embedding_incarnation = store
                .vertex_embedding_incarnation(vid, name)
                .unwrap_or(op.embedding_incarnation);
            vx.vector_upsert(VectorEmbeddingSyncOp {
                index_id: op.index_id,
                embedding_name_id: op.embedding_name_id,
                subject: op.subject,
                embedding_incarnation,
                embedding_version: record.version,
                encoding: record.encoding,
                dims: record.dims,
                metric: op.metric,
                bytes: record.bytes,
                remove: false,
            })
            .await
        }
        None => {
            // The incarnation high-water mark survives the canonical remove, so it is the deleted
            // incarnation. Fall back to the op's stamped incarnation if the identity was never
            // written (e.g. a pre-Slice-4 journal entry).
            let embedding_incarnation = store
                .vertex_embedding_incarnation(vid, name)
                .unwrap_or(op.embedding_incarnation);
            vx.vector_remove(VectorEmbeddingSyncOp {
                index_id: op.index_id,
                embedding_name_id: op.embedding_name_id,
                subject: op.subject,
                embedding_incarnation,
                embedding_version: RECONCILE_TOMBSTONE_VERSION,
                encoding: op.encoding,
                dims: op.dims,
                metric: op.metric,
                bytes: Vec::new(),
                remove: true,
            })
            .await
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
    use gleaph_graph_kernel::vector_index::VectorMetric;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
                vector_index_canister: None,
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
            graph.repair_journal_append(0, [vertex_insert(1), vertex_insert(2), vertex_insert(3)]);
            let index = CountingIndex::new(0);
            pollster::block_on(drain_once(&index, None)).expect("drain succeeds");
            assert_eq!(index.inserts.load(Ordering::SeqCst), 3);
            assert!(graph.repair_journal_is_empty());
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

    struct RecordingVectorIndex {
        upserts: AtomicUsize,
        removes: AtomicUsize,
        last_remove_version: AtomicU64,
        last_remove_incarnation: AtomicU64,
        last_upsert_incarnation: AtomicU64,
        last_upsert_metric: std::sync::Mutex<VectorMetric>,
    }

    impl RecordingVectorIndex {
        fn new() -> Self {
            Self {
                upserts: AtomicUsize::new(0),
                removes: AtomicUsize::new(0),
                last_remove_version: AtomicU64::new(0),
                last_remove_incarnation: AtomicU64::new(0),
                last_upsert_incarnation: AtomicU64::new(0),
                last_upsert_metric: std::sync::Mutex::new(VectorMetric::L2Squared),
            }
        }
    }

    #[async_trait(?Send)]
    impl VectorIndexLookup for RecordingVectorIndex {
        async fn vector_upsert(
            &self,
            op: gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp,
        ) -> Result<(), PlanQueryError> {
            self.upserts.fetch_add(1, Ordering::SeqCst);
            self.last_upsert_incarnation
                .store(op.embedding_incarnation, Ordering::SeqCst);
            *self.last_upsert_metric.lock().unwrap() = op.metric;
            Ok(())
        }

        async fn vector_remove(
            &self,
            op: gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp,
        ) -> Result<(), PlanQueryError> {
            self.removes.fetch_add(1, Ordering::SeqCst);
            self.last_remove_version
                .store(op.embedding_version, Ordering::SeqCst);
            self.last_remove_incarnation
                .store(op.embedding_incarnation, Ordering::SeqCst);
            Ok(())
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
                embedding_incarnation: 1,
                embedding_version: 1,
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
                embedding_incarnation: 1,
                embedding_version: 1,
                encoding: VectorEncoding::F32,
                dims: 1,
                metric: VectorMetric::Cosine,
                bytes: vec![0, 0, 0, 0],
                remove: false,
            },
        }
    }

    #[test]
    fn drain_reconciles_present_subject_to_upsert() {
        use gleaph_graph_kernel::vector_index::VectorEncoding;
        with_routing(|graph| {
            // Canonical still owns the embeddings → reconcile re-derives current upserts, ignoring
            // the (possibly stale) journaled op contents.
            for vid in [1u32, 2] {
                graph
                    .set_vertex_embedding(
                        VertexId::from(vid),
                        EmbeddingNameId::from_raw(1),
                        VectorEncoding::F32,
                        1,
                        vec![0, 0, 0, 0],
                    )
                    .expect("set embedding");
            }
            graph.repair_journal_append(0, [vector_upsert_op(1), vector_upsert_op(2)]);
            let index = CountingIndex::new(0);
            let vector = RecordingVectorIndex::new();
            pollster::block_on(drain_once(&index, Some(&vector))).expect("drain succeeds");
            assert_eq!(vector.upserts.load(Ordering::SeqCst), 2);
            assert_eq!(vector.removes.load(Ordering::SeqCst), 0);
            assert_eq!(
                vector.last_upsert_incarnation.load(Ordering::SeqCst),
                1,
                "reconcile re-derives the canonical incarnation"
            );
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn drain_reconciles_deleted_subject_to_remove() {
        with_routing(|graph| {
            // No canonical embedding for this subject → a stale upsert replay is reconciled into a
            // remove, so it can never resurrect a tombstoned vector.
            graph.repair_journal_append(0, [vector_upsert_op(5)]);
            let index = CountingIndex::new(0);
            let vector = RecordingVectorIndex::new();
            pollster::block_on(drain_once(&index, Some(&vector))).expect("drain succeeds");
            assert_eq!(vector.upserts.load(Ordering::SeqCst), 0);
            assert_eq!(
                vector.removes.load(Ordering::SeqCst),
                1,
                "stale upsert reconciled to a remove"
            );
            assert_eq!(
                vector.last_remove_version.load(Ordering::SeqCst),
                u64::MAX,
                "canonical-wins remove uses an authoritative tombstone clock, not the stale op version"
            );
            assert_eq!(
                vector.last_remove_incarnation.load(Ordering::SeqCst),
                1,
                "reconcile remove carries the deleted incarnation so it cannot tombstone a newer reinsert"
            );
            assert!(graph.repair_journal_is_empty());
        });
    }

    #[test]
    fn drain_reconcile_reinsert_re_derives_new_incarnation() {
        use gleaph_graph_kernel::vector_index::VectorEncoding;
        with_routing(|graph| {
            let vid = VertexId::from(1u32);
            let name = EmbeddingNameId::from_raw(1);
            // Delete + reinsert bumps the canonical incarnation to 2 after the stale op (stamped
            // incarnation 1) was journaled.
            graph
                .set_vertex_embedding(vid, name, VectorEncoding::F32, 1, vec![0, 0, 0, 0])
                .expect("first insert");
            graph.remove_vertex_embedding(vid, name).expect("remove");
            graph
                .set_vertex_embedding(vid, name, VectorEncoding::F32, 1, vec![0, 0, 0, 0])
                .expect("reinsert");
            graph.repair_journal_append(0, [vector_upsert_op(1)]);

            let index = CountingIndex::new(0);
            let vector = RecordingVectorIndex::new();
            pollster::block_on(drain_once(&index, Some(&vector))).expect("drain succeeds");
            assert_eq!(vector.upserts.load(Ordering::SeqCst), 1);
            assert_eq!(
                vector.last_upsert_incarnation.load(Ordering::SeqCst),
                2,
                "the stale replay cannot regress the clock below the live reinsert incarnation"
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
        use gleaph_graph_kernel::vector_index::VectorEncoding;
        with_routing(|graph| {
            let vid = VertexId::from(1u32);
            let name = EmbeddingNameId::from_raw(1);
            graph
                .set_vertex_embedding(vid, name, VectorEncoding::F32, 1, vec![0, 0, 0, 0])
                .expect("seed canonical embedding");
            graph.repair_journal_append(0, [vector_cosine_upsert_op(1)]);

            let index = CountingIndex::new(0);
            let vector = RecordingVectorIndex::new();
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
