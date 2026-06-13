//! Label telemetry replay: drain graph shard outbox into router aggregates.

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::plan_exec::{LabelTelemetryEventWire, ShardEventSeq};

use super::super::stable::graph_catalog::lookup_graph_id;
use super::RouterStore;
use crate::state::RouterError;
use crate::types::{AdminLabelTelemetryReplayStepArgs, AdminLabelTelemetryReplayStepResult};

impl RouterStore {
    pub(crate) async fn admin_label_telemetry_replay_step<FList, FAck, FutList, FutAck>(
        &self,
        caller: Principal,
        args: AdminLabelTelemetryReplayStepArgs,
        list_pending: FList,
        mut ack_event: FAck,
    ) -> Result<AdminLabelTelemetryReplayStepResult, RouterError>
    where
        FList: FnOnce(Principal, ShardEventSeq, u32) -> FutList,
        FutList: Future<Output = Result<Vec<LabelTelemetryEventWire>, String>>,
        FAck: FnMut(Principal, ShardEventSeq) -> FutAck,
        FutAck: Future<Output = Result<(), String>>,
    {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        if args.max_events == 0 {
            return Err(RouterError::InvalidArgument(
                "max_events must be greater than zero".into(),
            ));
        }

        let shard = self.resolve_shard_for_replay(&args.logical_graph_name, args.shard_id)?;
        let events = list_pending(shard.graph_canister, 0, args.max_events)
            .await
            .map_err(RouterError::Internal)?;
        if events.is_empty() {
            return Ok(AdminLabelTelemetryReplayStepResult {
                shard_id: args.shard_id,
                events_drained: 0,
                events_applied: 0,
                done: true,
            });
        }

        let mut events_applied = 0u32;
        for event in &events {
            if self.apply_label_telemetry_event(args.shard_id, event) {
                events_applied = events_applied.saturating_add(1);
            }
            ack_event(shard.graph_canister, event.shard_event_seq)
                .await
                .map_err(RouterError::Internal)?;
        }

        let events_drained = u32::try_from(events.len()).unwrap_or(u32::MAX);
        let done = events.len() < args.max_events as usize;

        Ok(AdminLabelTelemetryReplayStepResult {
            shard_id: args.shard_id,
            events_drained,
            events_applied,
            done,
        })
    }

    fn resolve_shard_for_replay(
        &self,
        logical_graph_name: &str,
        shard_id: gleaph_graph_kernel::federation::ShardId,
    ) -> Result<gleaph_graph_kernel::federation::ShardRegistryEntry, RouterError> {
        let graph_id = lookup_graph_id(logical_graph_name)
            .ok_or_else(|| RouterError::NotFound(logical_graph_name.to_owned()))?;
        let entry = self.resolve_shard(shard_id)?;
        if entry.graph_id != graph_id {
            return Err(RouterError::InvalidArgument(format!(
                "shard {shard_id} is registered for graph {}, not {logical_graph_name}",
                entry.graph_id
            )));
        }
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::RouterInitArgs;
    use crate::types::{
        AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus, ProvisioningState,
    };
    use gleaph_graph_kernel::entry::{GraphId, VertexLabelId};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::plan_exec::LabelUsageDelta;
    use std::collections::BTreeSet;

    fn test_init_args() -> RouterInitArgs {
        RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        }
    }

    fn graph_principal(n: u8) -> Principal {
        Principal::from_slice(&[n])
    }

    fn register_test_graph(store: &RouterStore, admin: Principal, name: &str) {
        store
            .admin_register_graph(
                admin,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    graph_name: name.to_owned(),
                    canister_id: Principal::management_canister(),
                    owner: admin,
                    admins: BTreeSet::new(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: false,
                },
            )
            .expect("register graph");
    }

    #[test]
    fn admin_label_telemetry_replay_step_drains_outbox() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        register_test_graph(&store, admin, "tenant.main");
        let label = VertexLabelId::from_raw(3);

        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: ShardId::new(0),
                graph_canister: graph_principal(1),
                index_canister: graph_principal(2),
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register shard");

        let events = vec![
            LabelTelemetryEventWire {
                mutation_id: 1,
                shard_event_seq: 10,
                label_usage_delta: LabelUsageDelta {
                    vertex: vec![(label, 2)],
                    edge: vec![],
                },
            },
            LabelTelemetryEventWire {
                mutation_id: 2,
                shard_event_seq: 11,
                label_usage_delta: LabelUsageDelta {
                    vertex: vec![(label, 1)],
                    edge: vec![],
                },
            },
        ];
        let pending = events.clone();
        let acked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let result = futures::executor::block_on(store.admin_label_telemetry_replay_step(
            admin,
            AdminLabelTelemetryReplayStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_events: 10,
            },
            move |_graph, _from, _limit| {
                let pending = pending.clone();
                async move { Ok(pending) }
            },
            {
                let acked = std::sync::Arc::clone(&acked);
                move |_graph, seq| {
                    acked.lock().expect("lock").push(seq);
                    async move { Ok(()) }
                }
            },
        ))
        .expect("replay step");

        assert_eq!(result.shard_id, ShardId::new(0));
        assert_eq!(result.events_drained, 2);
        assert_eq!(result.events_applied, 2);
        assert!(result.done);
        assert_eq!(*acked.lock().expect("lock"), vec![10, 11]);
        assert_eq!(
            store.vertex_label_shard_live_count(ShardId::new(0), label),
            3
        );

        let replay = futures::executor::block_on(store.admin_label_telemetry_replay_step(
            admin,
            AdminLabelTelemetryReplayStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_events: 10,
            },
            |_graph, _from, _limit| async { Ok(Vec::new()) },
            |_graph, _seq| async { Ok(()) },
        ))
        .expect("empty replay");
        assert!(replay.done);
        assert_eq!(replay.events_drained, 0);
    }

    #[test]
    fn admin_label_telemetry_replay_step_acks_duplicates_without_double_apply() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        register_test_graph(&store, admin, "tenant.main");
        let label = VertexLabelId::from_raw(5);

        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: ShardId::new(0),
                graph_canister: graph_principal(1),
                index_canister: graph_principal(2),
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register shard");

        let event = LabelTelemetryEventWire {
            mutation_id: 1,
            shard_event_seq: 42,
            label_usage_delta: LabelUsageDelta {
                vertex: vec![(label, 4)],
                edge: vec![],
            },
        };
        assert!(store.apply_label_telemetry_event(ShardId::new(0), &event));

        let result = futures::executor::block_on(store.admin_label_telemetry_replay_step(
            admin,
            AdminLabelTelemetryReplayStepArgs {
                logical_graph_name: "tenant.main".into(),
                shard_id: ShardId::new(0),
                max_events: 1,
            },
            move |_graph, _from, _limit| {
                let event = event.clone();
                async move { Ok(vec![event]) }
            },
            |_graph, _seq| async { Ok(()) },
        ))
        .expect("replay");

        assert_eq!(result.events_drained, 1);
        assert_eq!(result.events_applied, 0);
        assert_eq!(
            store.vertex_label_shard_live_count(ShardId::new(0), label),
            4
        );
    }
}
