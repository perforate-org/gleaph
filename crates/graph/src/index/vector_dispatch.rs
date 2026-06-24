//! Derives `graph-vector-index` mutations from canonical embedding writes (ADR 0031).
//!
//! Hooked from the [`crate::facade::store`] embedding write/delete paths. Dispatch is gated by the
//! ephemeral router-sourced catalog ([`crate::index::vector_catalog_context`]): if the embedding
//! name is not indexed for the current operation, no op is queued. The op carries the canonical
//! record's `encoding` / `dims` / `embedding_version`, so the vector index never has to consult the
//! Graph shard for embedding contents.

use crate::facade::GraphStore;
use crate::index::vector_pending;
use gleaph_graph_kernel::entry::EmbeddingNameId;
use gleaph_graph_kernel::vector_index::{VectorEmbeddingSyncOp, VectorEncoding, VectorSubject};
use ic_stable_lara::VertexId;

fn vertex_id_raw(vertex_id: VertexId) -> u32 {
    u32::try_from(u64::from(vertex_id)).unwrap_or(0)
}

/// Queues an upsert for a just-written canonical vertex embedding, if its name is indexed. Reads the
/// canonical record back to source the authoritative `embedding_version`, `encoding`, `dims`, and
/// bytes for the op.
pub(crate) fn dispatch_vertex_upsert(vertex_id: VertexId, embedding_name_id: EmbeddingNameId) {
    let Some(spec) = crate::index::vector_catalog_context::spec_for(embedding_name_id.raw()) else {
        return;
    };
    let store = GraphStore::new();
    let Some(routing) = store.federation_routing() else {
        return;
    };
    let Some(record) = store.vertex_embedding(vertex_id, embedding_name_id) else {
        return;
    };
    vector_pending::push_vector_op(VectorEmbeddingSyncOp {
        index_id: spec.index_id,
        embedding_name_id: embedding_name_id.raw(),
        subject: VectorSubject::Vertex {
            shard_id: routing.shard_id,
            vertex_id: vertex_id_raw(vertex_id),
        },
        embedding_version: record.version,
        encoding: record.encoding,
        dims: record.dims,
        bytes: record.bytes,
        remove: false,
    });
}

/// Queues a remove for a just-deleted canonical vertex embedding, if its name is indexed. The op
/// carries the deleted record's `embedding_version` so the canister's tombstone clock supersedes any
/// stale upsert replay (`bytes` is empty on remove).
pub(crate) fn dispatch_vertex_remove(
    vertex_id: VertexId,
    embedding_name_id: EmbeddingNameId,
    embedding_version: u64,
    encoding: VectorEncoding,
    dims: u16,
) {
    let Some(spec) = crate::index::vector_catalog_context::spec_for(embedding_name_id.raw()) else {
        return;
    };
    let Some(routing) = GraphStore::new().federation_routing() else {
        return;
    };
    vector_pending::push_vector_op(VectorEmbeddingSyncOp {
        index_id: spec.index_id,
        embedding_name_id: embedding_name_id.raw(),
        subject: VectorSubject::Vertex {
            shard_id: routing.shard_id,
            vertex_id: vertex_id_raw(vertex_id),
        },
        embedding_version,
        encoding,
        dims,
        bytes: Vec::new(),
        remove: true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use crate::index::{vector_catalog_context, vector_pending};
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{IndexedEmbeddingSpec, VectorIndexKind, VectorMetric};

    fn vec_bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn spec(name: u16) -> IndexedEmbeddingSpec {
        IndexedEmbeddingSpec {
            embedding_name_id: name,
            index_id: 7,
            kind: VectorIndexKind::IvfFlat,
            metric: VectorMetric::L2Squared,
            encoding: VectorEncoding::F32,
            dims: 2,
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
        vector_pending::clear_pending();
        let out = body(&graph);
        vector_pending::clear_pending();
        graph.set_federation_routing(None).expect("clear routing");
        out
    }

    #[test]
    fn indexed_embedding_write_queues_upsert_op() {
        with_routing(|store| {
            let vid = store.insert_vertex().expect("vertex");
            let name = EmbeddingNameId::from_raw(1);
            let _guard = vector_catalog_context::enter_indexed(&[spec(1)]);
            store
                .set_vertex_embedding(vid, name, VectorEncoding::F32, 2, vec_bytes(&[1.0, 2.0]))
                .expect("set embedding");
            let ops = vector_pending::pending_snapshot();
            assert_eq!(ops.len(), 1);
            assert_eq!(ops[0].index_id, 7);
            assert_eq!(ops[0].embedding_name_id, 1);
            assert_eq!(ops[0].embedding_version, 1);
            assert!(!ops[0].remove);
            assert_eq!(ops[0].bytes, vec_bytes(&[1.0, 2.0]));
        });
    }

    #[test]
    fn unindexed_embedding_write_queues_nothing() {
        with_routing(|store| {
            let vid = store.insert_vertex().expect("vertex");
            let name = EmbeddingNameId::from_raw(2);
            // Catalog registers a different name → this write is not indexed.
            let _guard = vector_catalog_context::enter_indexed(&[spec(1)]);
            store
                .set_vertex_embedding(vid, name, VectorEncoding::F32, 2, vec_bytes(&[1.0, 2.0]))
                .expect("set embedding");
            assert!(vector_pending::pending_snapshot().is_empty());
        });
    }

    #[test]
    fn no_catalog_queues_nothing() {
        with_routing(|store| {
            let vid = store.insert_vertex().expect("vertex");
            let name = EmbeddingNameId::from_raw(1);
            store
                .set_vertex_embedding(vid, name, VectorEncoding::F32, 2, vec_bytes(&[1.0, 2.0]))
                .expect("set embedding");
            assert!(vector_pending::pending_snapshot().is_empty());
        });
    }

    #[test]
    fn indexed_embedding_remove_queues_remove_op() {
        with_routing(|store| {
            let vid = store.insert_vertex().expect("vertex");
            let name = EmbeddingNameId::from_raw(1);
            let _guard = vector_catalog_context::enter_indexed(&[spec(1)]);
            store
                .set_vertex_embedding(vid, name, VectorEncoding::F32, 2, vec_bytes(&[1.0, 2.0]))
                .expect("set embedding");
            vector_pending::clear_pending();
            store.remove_vertex_embedding(vid, name).expect("removed");
            let ops = vector_pending::pending_snapshot();
            assert_eq!(ops.len(), 1);
            assert!(ops[0].remove);
            assert!(ops[0].bytes.is_empty());
            assert_eq!(ops[0].embedding_version, 1);
        });
    }
}
