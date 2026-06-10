//! Router-orchestrated label telemetry replay from graph shard outboxes.

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::plan_exec::{LabelTelemetryEventWire, ShardEventSeq};

use crate::facade::store::RouterStore;
use crate::state::RouterError;
use crate::types::{AdminLabelTelemetryReplayStepArgs, AdminLabelTelemetryReplayStepResult};

pub(crate) async fn admin_label_telemetry_replay_step<FList, FAck, FutList, FutAck>(
    store: &RouterStore,
    caller: Principal,
    args: AdminLabelTelemetryReplayStepArgs,
    list_pending: FList,
    ack_event: FAck,
) -> Result<AdminLabelTelemetryReplayStepResult, RouterError>
where
    FList: FnOnce(Principal, ShardEventSeq, u32) -> FutList,
    FutList: Future<Output = Result<Vec<LabelTelemetryEventWire>, String>>,
    FAck: FnMut(Principal, ShardEventSeq) -> FutAck,
    FutAck: Future<Output = Result<(), String>>,
{
    store
        .admin_label_telemetry_replay_step(caller, args, list_pending, ack_event)
        .await
}
