//! Federation routing metadata on the graph shard.

use crate::facade::{FederationRouting, GraphStore};
use crate::plan::PlanQueryError;

#[expect(
    dead_code,
    reason = "executor helper for index scans without routing metadata"
)]
pub(crate) fn federation_routing(store: &GraphStore) -> Result<FederationRouting, PlanQueryError> {
    store
        .federation_routing()
        .ok_or(PlanQueryError::UnsupportedOp("IndexScan(no shard routing)"))
}
