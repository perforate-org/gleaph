//! Cursor-based backfill of derived vector-index embeddings from canonical shard state (ADR 0031).
//!
//! Mirrors [`crate::index::vertex_property_backfill`] for the vector index. Each batch replays a
//! bounded window of vertices, dispatching an upsert for every embedding whose name is indexed by
//! the ambient router-sourced catalog ([`crate::index::vector_catalog_context`]). The router admin
//! orchestration step is deferred to a later slice; Slice 2 exercises this directly with a
//! mock client and a test-installed catalog.

use crate::facade::GraphStore;
use crate::index::vector_lookup::VectorIndexLookup;
use gleaph_graph_kernel::federation::{EmbeddingBackfillArgs, EmbeddingBackfillResult};
use gleaph_graph_kernel::vector_index::{VectorEmbeddingSyncOp, VectorSubject};
use ic_stable_lara::VertexId;

pub async fn backfill_vertex_embeddings(
    store: &GraphStore,
    vector: &dyn VectorIndexLookup,
    args: EmbeddingBackfillArgs,
) -> Result<EmbeddingBackfillResult, String> {
    let Some(routing) = store.federation_routing() else {
        return Err("federation not configured".into());
    };
    let shard_id = routing.shard_id;
    let vertex_cap = u32::from(store.vertex_count());
    let max_vertices = args.max_vertices.max(1);
    let mut cursor = args.start_vertex_id.min(vertex_cap);
    let mut vertices_processed = 0u32;
    let mut embeddings_synced = 0u32;

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
        for embedding_name_id in store.vertex_embedding_names(vertex_id) {
            let Some(spec) =
                crate::index::vector_catalog_context::spec_for(embedding_name_id.raw())
            else {
                continue;
            };
            let Some(record) = store.vertex_embedding(vertex_id, embedding_name_id) else {
                continue;
            };
            // A record written before Slice 4 has no incarnation entry; treat it as the implicit
            // first incarnation (1).
            let embedding_incarnation = store
                .vertex_embedding_incarnation(vertex_id, embedding_name_id)
                .unwrap_or(1);
            vector
                .vector_upsert(VectorEmbeddingSyncOp {
                    index_id: spec.index_id,
                    embedding_name_id: embedding_name_id.raw(),
                    subject: VectorSubject::Vertex {
                        shard_id,
                        vertex_id: local_raw,
                    },
                    embedding_incarnation,
                    embedding_version: record.version,
                    encoding: record.encoding,
                    dims: record.dims,
                    metric: spec.metric,
                    bytes: record.bytes,
                    remove: false,
                })
                .await
                .map_err(|e| e.to_string())?;
            embeddings_synced = embeddings_synced.saturating_add(1);
        }
    }

    Ok(EmbeddingBackfillResult {
        next_vertex_id: cursor,
        vertices_processed,
        embeddings_synced,
        done: cursor >= vertex_cap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use crate::index::vector_catalog_context;
    use async_trait::async_trait;
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{
        IndexedEmbeddingSpec, VectorEncoding, VectorIndexKind, VectorMetric,
    };
    use std::sync::Mutex;

    struct RecordingVectorIndex {
        upserts: Mutex<Vec<VectorEmbeddingSyncOp>>,
    }

    impl RecordingVectorIndex {
        fn new() -> Self {
            Self {
                upserts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait(?Send)]
    impl VectorIndexLookup for RecordingVectorIndex {
        async fn vector_upsert(
            &self,
            op: VectorEmbeddingSyncOp,
        ) -> Result<(), crate::plan::PlanQueryError> {
            self.upserts.lock().unwrap().push(op);
            Ok(())
        }

        async fn vector_remove(
            &self,
            _op: VectorEmbeddingSyncOp,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }
    }

    fn vec_bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn spec(name: u16) -> IndexedEmbeddingSpec {
        spec_with_metric(name, VectorMetric::L2Squared)
    }

    fn spec_with_metric(name: u16, metric: VectorMetric) -> IndexedEmbeddingSpec {
        IndexedEmbeddingSpec {
            embedding_name_id: name,
            index_id: 3,
            kind: VectorIndexKind::IvfFlat,
            metric,
            encoding: VectorEncoding::F32,
            dims: 2,
        }
    }

    fn federated_store() -> GraphStore {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_index_canister: Some(Principal::management_canister()),
            }))
            .expect("routing");
        store
    }

    #[test]
    fn backfill_replays_indexed_embeddings_only() {
        let store = federated_store();
        let vector = RecordingVectorIndex::new();
        let vid = store.insert_vertex().expect("vertex");
        let indexed = gleaph_graph_kernel::entry::EmbeddingNameId::from_raw(1);
        let other = gleaph_graph_kernel::entry::EmbeddingNameId::from_raw(2);
        // Writes happen with no catalog installed → no dispatch side effects pollute the queue.
        store
            .set_vertex_embedding(vid, indexed, VectorEncoding::F32, 2, vec_bytes(&[1.0, 2.0]))
            .expect("indexed embedding");
        store
            .set_vertex_embedding(vid, other, VectorEncoding::F32, 2, vec_bytes(&[3.0, 4.0]))
            .expect("other embedding");

        let _catalog = vector_catalog_context::enter_indexed(&[spec(1)]);
        let result = pollster::block_on(backfill_vertex_embeddings(
            &store,
            &vector,
            EmbeddingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        assert!(result.done);
        assert_eq!(result.embeddings_synced, 1);
        let upserts = vector.upserts.lock().unwrap().clone();
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].embedding_name_id, 1);
        assert_eq!(upserts[0].index_id, 3);
        assert_eq!(upserts[0].bytes, vec_bytes(&[1.0, 2.0]));
        assert_eq!(
            upserts[0].subject,
            VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: u32::from(vid),
            }
        );
        assert_eq!(upserts[0].metric, VectorMetric::L2Squared);

        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn backfill_carries_cosine_metric_from_catalog() {
        let store = federated_store();
        let vector = RecordingVectorIndex::new();
        let vid = store.insert_vertex().expect("vertex");
        let indexed = gleaph_graph_kernel::entry::EmbeddingNameId::from_raw(1);
        store
            .set_vertex_embedding(vid, indexed, VectorEncoding::F32, 2, vec_bytes(&[1.0, 2.0]))
            .expect("indexed embedding");

        let _catalog =
            vector_catalog_context::enter_indexed(&[spec_with_metric(1, VectorMetric::Cosine)]);
        let result = pollster::block_on(backfill_vertex_embeddings(
            &store,
            &vector,
            EmbeddingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        assert!(result.done);
        let upserts = vector.upserts.lock().unwrap();
        assert_eq!(upserts.len(), 1);
        assert_eq!(upserts[0].metric, VectorMetric::Cosine);

        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn backfill_is_bounded_by_max_vertices() {
        let store = federated_store();
        let vector = RecordingVectorIndex::new();
        let v0 = store.insert_vertex().expect("v0");
        let v1 = store.insert_vertex().expect("v1");
        let name = gleaph_graph_kernel::entry::EmbeddingNameId::from_raw(1);
        store
            .set_vertex_embedding(v0, name, VectorEncoding::F32, 2, vec_bytes(&[1.0, 2.0]))
            .expect("v0 embedding");
        store
            .set_vertex_embedding(v1, name, VectorEncoding::F32, 2, vec_bytes(&[3.0, 4.0]))
            .expect("v1 embedding");

        let _catalog = vector_catalog_context::enter_indexed(&[spec(1)]);
        let first = pollster::block_on(backfill_vertex_embeddings(
            &store,
            &vector,
            EmbeddingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 1,
            },
        ))
        .expect("first batch");
        assert!(!first.done);
        assert_eq!(first.vertices_processed, 1);
        assert_eq!(first.next_vertex_id, 1);

        let second = pollster::block_on(backfill_vertex_embeddings(
            &store,
            &vector,
            EmbeddingBackfillArgs {
                start_vertex_id: first.next_vertex_id,
                max_vertices: 10,
            },
        ))
        .expect("second batch");
        assert!(second.done);
        assert_eq!(vector.upserts.lock().unwrap().len(), 2);

        store.set_federation_routing(None).expect("clear routing");
    }
}
