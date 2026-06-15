//! Router-orchestrated label stats projection from graph shard delta logs.

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::plan_exec::{LabelStatsDeltaEventWire, ShardEventSeq};

use crate::facade::store::RouterStore;
use crate::state::RouterError;
use crate::types::{AdminLabelStatsProjectionStepArgs, AdminLabelStatsProjectionStepResult};

pub(crate) async fn admin_label_stats_projection_step<FList, FAck, FutList, FutAck>(
    store: &RouterStore,
    caller: Principal,
    args: AdminLabelStatsProjectionStepArgs,
    list_pending: FList,
    ack_through: FAck,
) -> Result<AdminLabelStatsProjectionStepResult, RouterError>
where
    FList: FnOnce(Principal, ShardEventSeq, u32) -> FutList,
    FutList: Future<Output = Result<Vec<LabelStatsDeltaEventWire>, String>>,
    FAck: FnMut(Principal, ShardEventSeq) -> FutAck,
    FutAck: Future<Output = Result<(), String>>,
{
    store
        .admin_label_stats_projection_step(caller, args, list_pending, ack_through)
        .await
}
