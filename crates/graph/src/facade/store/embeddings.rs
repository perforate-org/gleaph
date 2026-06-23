//! Canonical vertex embedding domain (ADR 0031).
//!
//! Graph-owned write/read path for vertex embeddings. Derived vector-index op dispatch is not part
//! of this slice; `commit_clear_vertex_embeddings` mirrors `commit_clear_vertex_properties` so a
//! later phase can hook delta dispatch in without restructuring.

use super::super::stable::VERTEX_EMBEDDINGS;
use super::super::stable::vertex_embeddings::StoredEmbedding;
use super::GraphStore;
use super::error::GraphStoreError;
use gleaph_graph_kernel::entry::EmbeddingNameId;
use gleaph_graph_kernel::vector_index::VectorEncoding;
use ic_stable_lara::VertexId;

impl GraphStore {
    /// Inserts or updates a canonical vertex embedding.
    ///
    /// Validates byte width against `dims` and rejects reserved embedding names, dimension changes
    /// on an existing embedding, and version overflow before any stable mutation. Returns the new
    /// record version (`1` on first insert).
    pub fn set_vertex_embedding(
        &self,
        vertex_id: VertexId,
        embedding_name_id: EmbeddingNameId,
        encoding: VectorEncoding,
        dims: u16,
        bytes: Vec<u8>,
    ) -> Result<u64, GraphStoreError> {
        VERTEX_EMBEDDINGS
            .with_borrow_mut(|store| store.set(vertex_id, embedding_name_id, encoding, dims, bytes))
            .map_err(GraphStoreError::from)
    }

    pub fn vertex_embedding(
        &self,
        vertex_id: VertexId,
        embedding_name_id: EmbeddingNameId,
    ) -> Option<StoredEmbedding> {
        VERTEX_EMBEDDINGS.with_borrow(|store| store.get(vertex_id, embedding_name_id))
    }

    pub fn remove_vertex_embedding(
        &self,
        vertex_id: VertexId,
        embedding_name_id: EmbeddingNameId,
    ) -> Option<StoredEmbedding> {
        VERTEX_EMBEDDINGS.with_borrow_mut(|store| store.remove(vertex_id, embedding_name_id))
    }

    /// Removes every embedding owned by `vertex_id` (vertex-delete sidecar clear).
    pub(super) fn commit_clear_vertex_embeddings(&self, vertex_id: VertexId) {
        let names: Vec<EmbeddingNameId> =
            VERTEX_EMBEDDINGS.with_borrow(|store| store.names_for(vertex_id));
        for embedding_name_id in names {
            VERTEX_EMBEDDINGS.with_borrow_mut(|store| store.remove(vertex_id, embedding_name_id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn set_get_remove_round_trip_through_facade() {
        let store = GraphStore::new();
        let vid = store.insert_vertex().expect("insert vertex");
        let name = EmbeddingNameId::from_raw(1);

        assert_eq!(
            store
                .set_vertex_embedding(vid, name, VectorEncoding::F32, 2, vec_bytes(&[1.0, 2.0]))
                .expect("set embedding"),
            1
        );
        assert_eq!(
            store.vertex_embedding(vid, name).expect("present").version,
            1
        );
        assert!(store.remove_vertex_embedding(vid, name).is_some());
        assert!(store.vertex_embedding(vid, name).is_none());
    }

    #[test]
    fn reserved_embedding_name_is_rejected() {
        let store = GraphStore::new();
        let vid = store.insert_vertex().expect("insert vertex");
        let err = store
            .set_vertex_embedding(
                vid,
                EmbeddingNameId::from_raw(0),
                VectorEncoding::F32,
                1,
                vec_bytes(&[1.0]),
            )
            .expect_err("reserved name rejected");
        assert!(matches!(err, GraphStoreError::Embedding(_)));
    }

    #[test]
    fn vertex_delete_clears_embeddings() {
        let store = GraphStore::new();
        let vid = store.insert_vertex().expect("insert vertex");
        let one = EmbeddingNameId::from_raw(1);
        let two = EmbeddingNameId::from_raw(2);

        store
            .set_vertex_embedding(vid, one, VectorEncoding::F32, 1, vec_bytes(&[1.0]))
            .expect("set embedding one");
        store
            .set_vertex_embedding(vid, two, VectorEncoding::F32, 2, vec_bytes(&[2.0, 3.0]))
            .expect("set embedding two");

        store.delete_vertex(vid).expect("delete detached vertex");

        assert!(store.vertex_embedding(vid, one).is_none());
        assert!(store.vertex_embedding(vid, two).is_none());
    }
}
