//! Graph-scoped index name ↔ [`IndexNameId`] catalog (ADR 0011).

use gleaph_graph_kernel::entry::{GraphId, IndexNameId};

use crate::facade::stable::{ROUTER_INDEX_NAME_CATALOG, graph_catalog::catalog_error_to_router};
use crate::state::RouterError;

pub(crate) fn lookup_index_name_id(graph_id: GraphId, name: &str) -> Option<IndexNameId> {
    ROUTER_INDEX_NAME_CATALOG.with_borrow(|catalog| catalog.get_id(graph_id, name))
}

#[allow(dead_code, reason = "reverse lookup for index DDL and admin tooling pending")]
pub(crate) fn index_name(graph_id: GraphId, id: IndexNameId) -> Option<String> {
    ROUTER_INDEX_NAME_CATALOG.with_borrow(|catalog| catalog.get_name(graph_id, id))
}

pub(crate) fn intern_index_name(graph_id: GraphId, name: &str) -> Result<IndexNameId, RouterError> {
    ROUTER_INDEX_NAME_CATALOG
        .with_borrow_mut(|catalog| catalog.get_or_insert(graph_id, name))
        .map_err(|e| catalog_error_to_router(e, "index"))
}
