//! Ephemeral, router-sourced indexed-embedding catalog for the current operation (ADR 0031).
//!
//! Mirrors [`crate::index::catalog_context`] for vector embeddings: the Router owns the set of
//! indexed embedding names and supplies a snapshot per operation. The shard never persists vector
//! index definitions, so this gate can never go stale across the canister upgrade boundary. In
//! Slice 2 no production caller installs a catalog yet (Router integration is Slice 3); dispatch is
//! therefore inert in production and exercised only via the test-only [`enter_indexed`] helper.

use gleaph_graph_kernel::vector_index::{IndexedEmbeddingCatalog, IndexedEmbeddingSpec};
use std::cell::RefCell;

thread_local! {
    static CURRENT: RefCell<Option<IndexedEmbeddingCatalog>> = const { RefCell::new(None) };
}

/// RAII guard that keeps a router-sourced catalog active for the current operation and restores the
/// previous value (if any) on drop.
#[must_use = "the catalog is only active while the guard is alive"]
pub(crate) struct VectorCatalogGuard {
    previous: Option<IndexedEmbeddingCatalog>,
}

impl Drop for VectorCatalogGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.previous.take());
    }
}

/// Install `catalog` as the current operation's indexed-embedding catalog.
pub(crate) fn enter(catalog: IndexedEmbeddingCatalog) -> VectorCatalogGuard {
    let previous = CURRENT.with(|c| c.borrow_mut().replace(catalog));
    VectorCatalogGuard { previous }
}

/// The indexed spec for an embedding name, if the current operation's catalog registers it.
pub(crate) fn spec_for(embedding_name_id: u16) -> Option<IndexedEmbeddingSpec> {
    CURRENT.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|catalog| catalog.spec_for(embedding_name_id))
    })
}

#[cfg(test)]
pub(crate) fn enter_indexed(specs: &[IndexedEmbeddingSpec]) -> VectorCatalogGuard {
    enter(IndexedEmbeddingCatalog {
        embeddings: specs.to_vec(),
    })
}
