//! Router-orchestrated label posting backfill across graph shards.

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::federation::{LabelPostingBackfillArgs, LabelPostingBackfillResult};

use crate::facade::store::RouterStore;
use crate::state::RouterError;
use crate::types::{AdminLabelBackfillStepArgs, AdminLabelBackfillStepResult};

pub(crate) async fn admin_label_backfill_step<F, Fut>(
    store: &RouterStore,
    caller: Principal,
    args: AdminLabelBackfillStepArgs,
    call_backfill: F,
) -> Result<AdminLabelBackfillStepResult, RouterError>
where
    F: FnOnce(Principal, LabelPostingBackfillArgs) -> Fut,
    Fut: Future<Output = Result<LabelPostingBackfillResult, String>>,
{
    store
        .admin_label_backfill_step(caller, args, call_backfill)
        .await
}

pub(crate) fn admin_list_label_backfill_status(
    store: &RouterStore,
    caller: Principal,
    logical_graph_name: &str,
) -> Result<Vec<crate::types::LabelBackfillShardStatus>, RouterError> {
    store.admin_list_label_backfill_status(caller, logical_graph_name)
}
