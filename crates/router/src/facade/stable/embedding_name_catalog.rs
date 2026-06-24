//! Graph-scoped embedding name ↔ [`EmbeddingNameId`] catalog (ADR 0031 Slice 3).
//!
//! The Router is the sole authority that allocates `EmbeddingNameId`s. Vector-index
//! registration resolves an embedding **by name (string)** and interns it here, so the id stored
//! on a [`super::vector_index_catalog::VectorIndexDefRecord`] is guaranteed to be the same id the
//! graph stamps on canonical embedding writes (`vector_dispatch` reads `embedding_name_id.raw()`).
//! Callers never supply a raw `u16`, which could otherwise reference an id that never matches a
//! canonical write.

use gleaph_graph_kernel::entry::{EmbeddingNameId, GraphId};

use crate::facade::stable::{
    ROUTER_EMBEDDING_NAME_CATALOG, graph_catalog::catalog_error_to_router,
};
use crate::state::RouterError;

#[allow(
    dead_code,
    reason = "embedding-name read path for the ephemeral catalog builder (Phase 5 unit)"
)]
pub(crate) fn lookup_embedding_name_id(graph_id: GraphId, name: &str) -> Option<EmbeddingNameId> {
    ROUTER_EMBEDDING_NAME_CATALOG.with_borrow(|catalog| catalog.get_id(graph_id, name))
}

#[allow(
    dead_code,
    reason = "reverse lookup for vector-index admin/query surface (Phase 4)"
)]
pub(crate) fn embedding_name(graph_id: GraphId, id: EmbeddingNameId) -> Option<String> {
    ROUTER_EMBEDDING_NAME_CATALOG.with_borrow(|catalog| catalog.get_name(graph_id, id))
}

pub(crate) fn intern_embedding_name(
    graph_id: GraphId,
    name: &str,
) -> Result<EmbeddingNameId, RouterError> {
    ROUTER_EMBEDDING_NAME_CATALOG
        .with_borrow_mut(|catalog| catalog.get_or_insert(graph_id, name))
        .map_err(|e| catalog_error_to_router(e, "embedding name"))
}

pub(crate) fn purge_graph_embedding_names(graph_id: GraphId) {
    ROUTER_EMBEDDING_NAME_CATALOG.with_borrow_mut(|catalog| catalog.remove_graph(graph_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_by_name_and_round_trips() {
        let graph = GraphId::from_raw(910_001);
        let id = intern_embedding_name(graph, "title_vec").expect("intern");
        assert!(!id.is_reserved(), "allocated id must not be the reserved 0");
        // Re-interning the same name is stable.
        assert_eq!(
            intern_embedding_name(graph, "title_vec").expect("re-intern"),
            id
        );
        assert_eq!(lookup_embedding_name_id(graph, "title_vec"), Some(id));
        assert_eq!(embedding_name(graph, id), Some("title_vec".to_owned()));
    }

    #[test]
    fn names_are_scoped_per_graph() {
        let g1 = GraphId::from_raw(910_002);
        let g2 = GraphId::from_raw(910_003);
        let a1 = intern_embedding_name(g1, "body_vec").expect("g1 intern");
        let a2 = intern_embedding_name(g2, "body_vec").expect("g2 intern");
        // Same first id per graph (dense from 1); identity is the (graph, id) pair.
        assert_eq!(a1.raw(), 1);
        assert_eq!(a2.raw(), 1);
        assert_eq!(lookup_embedding_name_id(g1, "body_vec"), Some(a1));
        // A second distinct name in g1 takes the next dense id.
        let b1 = intern_embedding_name(g1, "summary_vec").expect("g1 intern 2");
        assert_eq!(b1.raw(), 2);
    }

    #[test]
    fn unknown_name_has_no_id() {
        let graph = GraphId::from_raw(910_004);
        assert_eq!(lookup_embedding_name_id(graph, "never_registered"), None);
    }

    #[test]
    fn purge_clears_graph_scope() {
        let graph = GraphId::from_raw(910_005);
        let id = intern_embedding_name(graph, "vec").expect("intern");
        purge_graph_embedding_names(graph);
        assert_eq!(lookup_embedding_name_id(graph, "vec"), None);
        assert_eq!(embedding_name(graph, id), None);
    }
}
