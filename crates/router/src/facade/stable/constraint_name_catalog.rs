//! Graph-scoped constraint name ↔ [`ConstraintNameId`] catalog (ADR 0030).

use gleaph_graph_kernel::entry::{ConstraintNameId, GraphId};

use crate::facade::stable::{
    ROUTER_CONSTRAINT_NAME_CATALOG, graph_catalog::catalog_error_to_router,
};
use crate::state::RouterError;

pub(crate) fn lookup_constraint_name_id(graph_id: GraphId, name: &str) -> Option<ConstraintNameId> {
    ROUTER_CONSTRAINT_NAME_CATALOG.with_borrow(|catalog| catalog.get_id(graph_id, name))
}

#[allow(
    dead_code,
    reason = "reverse lookup for constraint admin tooling pending a later ADR 0030 slice"
)]
pub(crate) fn constraint_name(graph_id: GraphId, id: ConstraintNameId) -> Option<String> {
    ROUTER_CONSTRAINT_NAME_CATALOG.with_borrow(|catalog| catalog.get_name(graph_id, id))
}

pub(crate) fn intern_constraint_name(
    graph_id: GraphId,
    name: &str,
) -> Result<ConstraintNameId, RouterError> {
    ROUTER_CONSTRAINT_NAME_CATALOG
        .with_borrow_mut(|catalog| catalog.get_or_insert(graph_id, name))
        .map_err(|e| catalog_error_to_router(e, "constraint"))
}
