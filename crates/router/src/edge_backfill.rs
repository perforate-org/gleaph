//! Router-orchestrated edge property posting backfill across graph shards.

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::federation::{EdgePostingBackfillArgs, EdgePostingBackfillResult};

use crate::facade::store::RouterStore;
use crate::state::RouterError;
use crate::types::{AdminEdgeBackfillStepArgs, AdminEdgeBackfillStepResult};

pub(crate) async fn admin_edge_backfill_step<F, Fut>(
    store: &RouterStore,
    caller: Principal,
    args: AdminEdgeBackfillStepArgs,
    call_backfill: F,
) -> Result<AdminEdgeBackfillStepResult, RouterError>
where
    F: FnOnce(Principal, EdgePostingBackfillArgs) -> Fut,
    Fut: Future<Output = Result<EdgePostingBackfillResult, String>>,
{
    store
        .admin_edge_backfill_step(caller, args, call_backfill)
        .await
}

pub(crate) fn admin_list_edge_backfill_status(
    store: &RouterStore,
    caller: Principal,
    logical_graph_name: &str,
) -> Result<Vec<crate::types::EdgeBackfillShardStatus>, RouterError> {
    store.admin_list_edge_backfill_status(caller, logical_graph_name)
}
