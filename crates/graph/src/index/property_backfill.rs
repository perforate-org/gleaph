//! Cursor-based backfill of property index postings from shard-local vertex properties.

use crate::facade::GraphStore;
use crate::index::lookup::PropertyIndexLookup;
use crate::property::sortable_index_key;
use gleaph_graph_kernel::federation::{PostingBackfillArgs, PostingBackfillResult};
use ic_stable_lara::VertexId;

pub async fn backfill_property_postings(
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
            if !crate::index::registry::is_vertex_property_indexed(property_id) {
                continue;
            }
            let Some(payload_bytes) = sortable_index_key(&value) else {
                continue;
            };
            index
                .posting_insert_at(shard_id, property_id.raw(), payload_bytes, local_raw)
                .await
                .map_err(|e| e.to_string())?;
            postings_synced = postings_synced.saturating_add(1);
        }
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
    use gleaph_graph_kernel::index::{IndexIntersectionRequest, PostingHit, PostingRangeRequest};
    use std::sync::Mutex;

    struct RecordingIndex {
        inserts: Mutex<Vec<(u32, u32, Vec<u8>, u32)>>,
    }

    impl RecordingIndex {
        fn new() -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for RecordingIndex {
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
        crate::test_labels::register_indexed_vertex_property_named("backfill_name");
        store
            .set_vertex_property(vid, name, Value::Int64(42))
            .expect("name");
        store
            .set_vertex_property(vid, score, Value::Int64(99))
            .expect("score");

        let result = pollster::block_on(backfill_property_postings(
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

        let result = pollster::block_on(backfill_property_postings(
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
}
