//! Router-orchestrated property posting backfill across graph shards.

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::federation::{PropertyPostingBackfillArgs, PropertyPostingBackfillResult};

use crate::facade::store::RouterStore;
use crate::state::RouterError;
use crate::types::{AdminPropertyBackfillStepArgs, AdminPropertyBackfillStepResult};

pub(crate) async fn admin_property_backfill_step<F, Fut>(
    store: &RouterStore,
    caller: Principal,
    args: AdminPropertyBackfillStepArgs,
    call_backfill: F,
) -> Result<AdminPropertyBackfillStepResult, RouterError>
where
    F: FnOnce(Principal, PropertyPostingBackfillArgs) -> Fut,
    Fut: Future<Output = Result<PropertyPostingBackfillResult, String>>,
{
    store
        .admin_property_backfill_step(caller, args, call_backfill)
        .await
}

pub(crate) fn admin_list_property_backfill_status(
    store: &RouterStore,
    caller: Principal,
    logical_graph_name: &str,
) -> Result<Vec<crate::types::PropertyBackfillShardStatus>, RouterError> {
    store.admin_list_property_backfill_status(caller, logical_graph_name)
}
