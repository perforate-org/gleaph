//! Cursor-based backfill of label postings from shard-local vertex label state.

use crate::facade::GraphStore;
use crate::index::lookup::{PropertyIndexLookup, dispatch_posting_batch};
use gleaph_graph_kernel::federation::{PostingBackfillArgs, PostingBackfillResult};
use gleaph_graph_kernel::index::IndexPostingMutation;
use ic_stable_lara::VertexId;

pub async fn backfill_label_postings(
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
        let labels = store.vertex_labels(vertex_id, vertex);
        let local_raw = u32::from_le_bytes(vertex_id.to_le_bytes());
        for label in labels {
            if index.supports_posting_batch() {
                batch.push(IndexPostingMutation::Label {
                    remove: false,
                    label_id: u32::from(label.raw()),
                    vertex_id: local_raw,
                });
            } else {
                index
                    .label_posting_insert_at(shard_id, u32::from(label.raw()), local_raw)
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
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{
        IndexIntersectionRequest, IndexPostingBatchProgress, PostingHit, PostingRangeRequest,
    };
    use std::sync::Mutex;

    struct RecordingIndex {
        batches: Mutex<Vec<Vec<IndexPostingMutation>>>,
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for RecordingIndex {
        fn supports_posting_batch(&self) -> bool {
            true
        }

        async fn posting_batch_at(
            &self,
            _shard_id: ShardId,
            operations: Vec<IndexPostingMutation>,
        ) -> Result<IndexPostingBatchProgress, crate::plan::PlanQueryError> {
            let applied = operations.len() as u32;
            self.batches.lock().unwrap().push(operations);
            Ok(IndexPostingBatchProgress {
                applied,
                next_index: None,
                instruction_budget_exhausted: false,
            })
        }

        async fn lookup_equal(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Result<Vec<PostingHit>, crate::plan::PlanQueryError> {
            Ok(Vec::new())
        }

        async fn lookup_range(
            &self,
            _property_id: u32,
            _req: &PostingRangeRequest,
        ) -> Result<Vec<PostingHit>, crate::plan::PlanQueryError> {
            Ok(Vec::new())
        }

        async fn lookup_intersection(
            &self,
            _req: &IndexIntersectionRequest,
        ) -> Result<gleaph_graph_kernel::index::IndexIntersectionResult, crate::plan::PlanQueryError>
        {
            Ok(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices(Vec::new()))
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
        ) -> Result<(), crate::plan::PlanQueryError> {
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

    #[test]
    fn backfill_batches_multiple_vertex_labels() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_index_canister: None,
            }))
            .expect("routing");
        let vertex_id = store.insert_vertex().expect("vertex");
        let vertex = store.vertex(vertex_id).expect("vertex row");
        store
            .set_vertex_labels(
                vertex_id,
                vertex,
                [
                    gleaph_graph_kernel::entry::VertexLabelId::from_raw(1),
                    gleaph_graph_kernel::entry::VertexLabelId::from_raw(2),
                ],
            )
            .expect("labels");
        let index = RecordingIndex {
            batches: Mutex::new(Vec::new()),
        };

        let result = pollster::block_on(backfill_label_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        assert_eq!(result.postings_synced, 2);
        assert_eq!(index.batches.lock().unwrap().len(), 1);
    }
}
