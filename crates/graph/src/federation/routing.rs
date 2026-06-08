//! Federation routing metadata on the graph shard.

use crate::facade::{FederationRouting, GraphStore};
use crate::plan::PlanQueryError;

pub(crate) fn federation_routing(store: &GraphStore) -> Result<FederationRouting, PlanQueryError> {
    store
        .federation_routing()
        .ok_or(PlanQueryError::UnsupportedOp("IndexScan(no shard routing)"))
}
