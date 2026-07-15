//! Cursor-based backfill of vertex property index postings from shard-local vertex properties.

use crate::facade::GraphStore;
use crate::index::lookup::{PropertyIndexLookup, dispatch_posting_batch};
use crate::property::sortable_index_key;
use gleaph_graph_kernel::federation::{PostingBackfillArgs, PostingBackfillResult};
use gleaph_graph_kernel::index::IndexPostingMutation;
use ic_stable_lara::VertexId;

pub async fn backfill_vertex_property_postings(
    store: &GraphStore,
    index: &dyn PropertyIndexLookup,
    args: PostingBackfillArgs,
) -> Result<PostingBackfillResult, String> {
    if !store.federation_configured() {
        return Err("federation not configured".into());
    }
    let shard_id = index.local_shard_id();
    let vertex_cap = u32::from(store.vertex_count());
    let max_vertices = args.max_vertices.max(1);
    let mut cursor = args.start_vertex_id.min(vertex_cap);
    let mut vertices_processed = 0u32;
    let mut postings_synced = 0u32;
    let mut batch = Vec::new();

    while vertices_processed < max_vertices && cursor < vertex_cap {
        let vertex_id = VertexId::from(cursor);
        cursor = cursor.saturating_add(1);
        vertices_processed = vertices_processed.saturating_add(1);

        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone() {
            continue;
        }
        let local_raw = u32::from_le_bytes(vertex_id.to_le_bytes());
        for (property_id, value) in store.vertex_properties(vertex_id) {
            if !crate::index::catalog_context::is_vertex_property_indexed(property_id) {
                continue;
            }
            let Some(payload_bytes) = sortable_index_key(&value) else {
                continue;
            };
            if index.supports_posting_batch() {
                batch.push(IndexPostingMutation::VertexProperty {
                    remove: false,
                    property_id: property_id.raw(),
                    value: payload_bytes,
                    vertex_id: local_raw,
                });
            } else {
                index
                    .posting_insert_at(shard_id, property_id.raw(), payload_bytes, local_raw)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            postings_synced = postings_synced.saturating_add(1);
        }
    }

    if !batch.is_empty() {
        dispatch_posting_batch(index, shard_id, batch)
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(PostingBackfillResult {
        next_vertex_id: cursor,
        vertices_processed,
        postings_synced,
        done: cursor >= vertex_cap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use async_trait::async_trait;
    use candid::Principal;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::PropertyId;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{
        IndexIntersectionRequest, IndexPostingBatchProgress, IndexPostingMutation, PostingHit,
        PostingRangeRequest,
    };
    use std::sync::Mutex;

    struct RecordingIndex {
        inserts: Mutex<Vec<(u32, u32, Vec<u8>, u32)>>,
        batches: Mutex<Vec<Vec<IndexPostingMutation>>>,
        batch_mode: bool,
        batch_limit: Option<usize>,
        fail_batch: bool,
    }

    impl RecordingIndex {
        fn new() -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                batch_mode: false,
                batch_limit: None,
                fail_batch: false,
            }
        }

        fn batch() -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                batch_mode: true,
                batch_limit: None,
                fail_batch: false,
            }
        }

        fn batch_with_limit(limit: usize) -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                batch_mode: true,
                batch_limit: Some(limit),
                fail_batch: false,
            }
        }

        fn batch_failure() -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                batch_mode: true,
                batch_limit: None,
                fail_batch: true,
            }
        }
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for RecordingIndex {
        fn supports_posting_batch(&self) -> bool {
            self.batch_mode
        }

        async fn posting_batch_at(
            &self,
            _shard_id: ShardId,
            operations: Vec<IndexPostingMutation>,
        ) -> Result<IndexPostingBatchProgress, crate::plan::PlanQueryError> {
            if self.fail_batch {
                return Err(crate::plan::PlanQueryError::UnsupportedOp(
                    "forced backfill batch failure",
                ));
            }
            let applied = self
                .batch_limit
                .map_or(operations.len(), |limit| limit.min(operations.len()));
            self.batches
                .lock()
                .unwrap()
                .push(operations[..applied].to_vec());
            Ok(IndexPostingBatchProgress {
                applied: applied as u32,
                next_index: (applied < operations.len()).then_some(applied as u32),
                instruction_budget_exhausted: false,
            })
        }

        async fn lookup_equal(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Result<Vec<PostingHit>, crate::plan::PlanQueryError> {
            Ok(vec![])
        }

        async fn lookup_range(
            &self,
            _property_id: u32,
            _req: &PostingRangeRequest,
        ) -> Result<Vec<PostingHit>, crate::plan::PlanQueryError> {
            Ok(vec![])
        }

        async fn lookup_intersection(
            &self,
            _req: &IndexIntersectionRequest,
        ) -> Result<gleaph_graph_kernel::index::IndexIntersectionResult, crate::plan::PlanQueryError>
        {
            Ok(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices(vec![]))
        }

        fn local_shard_id(&self) -> ShardId {
            ShardId::new(0)
        }

        async fn posting_insert_at(
            &self,
            shard_id: ShardId,
            property_id: u32,
            value: Vec<u8>,
            vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            self.inserts
                .lock()
                .unwrap()
                .push((shard_id.raw(), property_id, value, vertex_id));
            Ok(())
        }

        async fn posting_remove_at(
            &self,
            _shard_id: ShardId,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }

        async fn label_posting_insert_at(
            &self,
            _shard_id: ShardId,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }

        async fn label_posting_remove_at(
            &self,
            _shard_id: ShardId,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }
    }

    fn federated_store() -> GraphStore {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_index_canister: None,
            }))
            .expect("routing");
        store
    }

    #[test]
    fn backfill_replays_indexable_vertex_properties() {
        let store = federated_store();
        let index = RecordingIndex::new();
        let vid = store.insert_vertex().expect("vertex");
        let name = crate::test_labels::property_id_for_name("backfill_name");
        let score = crate::test_labels::property_id_for_name("backfill_score");
        let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[name]);
        store
            .set_vertex_property(vid, name, Value::Int64(42))
            .expect("name");
        store
            .set_vertex_property(vid, score, Value::Int64(99))
            .expect("score");

        let result = pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        assert!(result.done);
        let inserts = index.inserts.lock().unwrap().clone();
        assert_eq!(
            inserts.len(),
            1,
            "only registered properties are backfilled"
        );
        assert!(inserts.iter().all(|(shard, _, _, _)| *shard == 0));
        assert!(inserts.iter().any(|(_, property_id, _, vertex_id)| {
            *property_id == name.raw() && *vertex_id == u32::from(vid)
        }));
    }

    #[test]
    fn backfill_skips_unindexable_values() {
        let store = federated_store();
        let index = RecordingIndex::new();
        let vid = store.insert_vertex().expect("vertex");
        let pid = PropertyId::from_raw(5);
        store
            .set_vertex_property(vid, pid, Value::Float64(f64::NAN))
            .expect("nan property");

        let result = pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        assert_eq!(result.postings_synced, 0);
        assert!(index.inserts.lock().unwrap().is_empty());
    }

    #[test]
    fn backfill_batches_multiple_vertex_properties() {
        let store = federated_store();
        let index = RecordingIndex::batch();
        let vid = store.insert_vertex().expect("vertex");
        let name = crate::test_labels::property_id_for_name("batch_name");
        let score = crate::test_labels::property_id_for_name("batch_score");
        let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[name, score]);
        store
            .set_vertex_property(vid, name, Value::Int64(42))
            .expect("name");
        store
            .set_vertex_property(vid, score, Value::Int64(99))
            .expect("score");

        let result = pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        assert_eq!(result.postings_synced, 2);
        let batches = index.batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }

    #[test]
    fn backfill_continues_after_partial_batch_progress() {
        let store = federated_store();
        let index = RecordingIndex::batch_with_limit(1);
        let vid = store.insert_vertex().expect("vertex");
        let first = crate::test_labels::property_id_for_name("partial_first");
        let second = crate::test_labels::property_id_for_name("partial_second");
        let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[first, second]);
        store
            .set_vertex_property(vid, first, Value::Int64(1))
            .expect("first");
        store
            .set_vertex_property(vid, second, Value::Int64(2))
            .expect("second");

        pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        let batches = index.batches.lock().unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn backfill_propagates_batch_failure_without_success_result() {
        let store = federated_store();
        let index = RecordingIndex::batch_failure();
        let vid = store.insert_vertex().expect("vertex");
        let property = crate::test_labels::property_id_for_name("failed_batch");
        let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[property]);
        store
            .set_vertex_property(vid, property, Value::Int64(7))
            .expect("property");

        let result = pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ));
        assert!(result.is_err());
    }
}
