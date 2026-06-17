//! Label stats projection: sole apply path for router aggregates (ADR 0015).

use std::future::Future;

use candid::Principal;
use gleaph_graph_kernel::plan_exec::{LabelStatsDelta, LabelStatsDeltaEventWire, ShardEventSeq};

use super::super::stable::graph_catalog::lookup_graph_id;
use super::super::stable::label_stats::{LabelShardKey, LabelStats};
use super::super::stable::{
    ROUTER_EDGE_LABEL_LIVE_BY_SHARD, ROUTER_EDGE_LABEL_STATS, ROUTER_LABEL_STATS_PROJECTION,
    ROUTER_VERTEX_LABEL_LIVE_BY_SHARD, ROUTER_VERTEX_LABEL_STATS,
};
use super::RouterStore;
use crate::facade::auth;
use crate::state::RouterError;
use crate::types::{
    AdminLabelStatsProjectionStepArgs, AdminLabelStatsProjectionStepResult, EdgeLabelId, ShardId,
    VertexLabelId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvanceLabelStatsProjectionResult {
    pub deltas_applied: u32,
    pub applied_through_seq: ShardEventSeq,
}

impl RouterStore {
    pub fn label_stats_projection_cursor(&self, shard_id: ShardId) -> ShardEventSeq {
        ROUTER_LABEL_STATS_PROJECTION
            .with_borrow(|projection| projection.get(&shard_id).unwrap_or(0))
    }

    fn commit_label_stats_projection_cursor(
        &self,
        shard_id: ShardId,
        applied_through_seq: ShardEventSeq,
    ) {
        ROUTER_LABEL_STATS_PROJECTION.with_borrow_mut(|projection| {
            let current = projection.get(&shard_id).unwrap_or(0);
            if applied_through_seq > current {
                projection.insert(shard_id, applied_through_seq);
            }
        });
    }

    pub fn vertex_label_stats(&self, label_id: VertexLabelId) -> LabelStats {
        ROUTER_VERTEX_LABEL_STATS
            .with_borrow(|m| m.get(&label_id.raw()))
            .unwrap_or_default()
    }

    pub fn edge_label_stats(&self, label_id: EdgeLabelId) -> LabelStats {
        ROUTER_EDGE_LABEL_STATS
            .with_borrow(|m| m.get(&label_id.raw()))
            .unwrap_or_default()
    }

    pub fn vertex_label_shard_live_count(&self, shard_id: ShardId, label_id: VertexLabelId) -> u64 {
        ROUTER_VERTEX_LABEL_LIVE_BY_SHARD
            .with_borrow(|m| m.get(&LabelShardKey::new(shard_id, label_id.raw())))
            .unwrap_or(0)
    }

    pub fn edge_label_shard_live_count(&self, shard_id: ShardId, label_id: EdgeLabelId) -> u64 {
        ROUTER_EDGE_LABEL_LIVE_BY_SHARD
            .with_borrow(|m| m.get(&LabelShardKey::new(shard_id, label_id.raw())))
            .unwrap_or(0)
    }

    fn commit_apply_label_delta(
        label_id: u16,
        shard_id: ShardId,
        delta: i64,
        stats_map: &'static std::thread::LocalKey<
            std::cell::RefCell<super::super::stable::memory::StableLabelStatsMap>,
        >,
        live_by_shard: &'static std::thread::LocalKey<
            std::cell::RefCell<super::super::stable::memory::StableLabelShardLiveMap>,
        >,
    ) {
        if delta == 0 {
            return;
        }
        let magnitude = delta.unsigned_abs();
        stats_map.with_borrow_mut(|stats| {
            let mut entry = stats.get(&label_id).unwrap_or_default();
            if delta > 0 {
                entry.live_count = entry.live_count.saturating_add(magnitude);
                entry.total_adds = entry.total_adds.saturating_add(magnitude);
            } else {
                entry.live_count = entry.live_count.saturating_sub(magnitude);
                entry.total_removes = entry.total_removes.saturating_add(magnitude);
            }
            stats.insert(label_id, entry);
        });

        let key = LabelShardKey::new(shard_id, label_id);
        live_by_shard.with_borrow_mut(|live| {
            let current = live.get(&key).unwrap_or(0);
            let next = if delta > 0 {
                current.saturating_add(magnitude)
            } else {
                current.saturating_sub(magnitude)
            };
            if next == 0 {
                live.remove(&key);
            } else {
                live.insert(key, next);
            }
        });
    }

    pub(crate) fn apply_label_stats_delta_payload(
        &self,
        shard_id: ShardId,
        delta: &LabelStatsDelta,
    ) {
        for (label_id, value) in &delta.vertex {
            Self::commit_apply_label_delta(
                label_id.raw(),
                shard_id,
                *value,
                &ROUTER_VERTEX_LABEL_STATS,
                &ROUTER_VERTEX_LABEL_LIVE_BY_SHARD,
            );
        }
        for (label_id, value) in &delta.edge {
            Self::commit_apply_label_delta(
                label_id.raw(),
                shard_id,
                *value,
                &ROUTER_EDGE_LABEL_STATS,
                &ROUTER_EDGE_LABEL_LIVE_BY_SHARD,
            );
        }
    }

    fn apply_label_stats_delta_event(&self, shard_id: ShardId, delta: &LabelStatsDeltaEventWire) {
        let cursor = self.label_stats_projection_cursor(shard_id);
        if delta.shard_event_seq <= cursor {
            return;
        }
        self.apply_label_stats_delta_payload(shard_id, &delta.label_stats_delta);
        self.commit_label_stats_projection_cursor(shard_id, delta.shard_event_seq);
    }

    pub(crate) async fn advance_label_stats_projection<FList, FAck, FutList, FutAck>(
        &self,
        graph_canister: Principal,
        shard_id: ShardId,
        limit: u32,
        list_pending: FList,
        mut ack_through: FAck,
    ) -> Result<AdvanceLabelStatsProjectionResult, RouterError>
    where
        FList: FnOnce(Principal, ShardEventSeq, u32) -> FutList,
        FutList: Future<Output = Result<Vec<LabelStatsDeltaEventWire>, String>>,
        FAck: FnMut(Principal, ShardEventSeq) -> FutAck,
        FutAck: Future<Output = Result<(), String>>,
    {
        if limit == 0 {
            return Err(RouterError::InvalidArgument(
                "limit must be greater than zero".into(),
            ));
        }

        let mut cursor = self.label_stats_projection_cursor(shard_id);
        let next_seq = cursor.checked_add(1).ok_or_else(|| {
            RouterError::Internal("label stats projection cursor exhausted".into())
        })?;
        let deltas = list_pending(graph_canister, next_seq, limit)
            .await
            .map_err(RouterError::Internal)?;
        if deltas.is_empty() {
            return Ok(AdvanceLabelStatsProjectionResult {
                deltas_applied: 0,
                applied_through_seq: cursor,
            });
        }

        let mut deltas_applied = 0u32;
        for delta in &deltas {
            let expected = cursor.checked_add(1).ok_or_else(|| {
                RouterError::Internal("label stats delta sequence exhausted".into())
            })?;
            if delta.shard_event_seq != expected {
                return Err(RouterError::InvalidArgument(format!(
                    "label stats projection gap for shard {shard_id}: expected seq {expected}, found {}",
                    delta.shard_event_seq
                )));
            }
            self.apply_label_stats_delta_event(shard_id, delta);
            cursor = delta.shard_event_seq;
            deltas_applied = deltas_applied.saturating_add(1);
        }

        ack_through(graph_canister, cursor)
            .await
            .map_err(RouterError::Internal)?;

        Ok(AdvanceLabelStatsProjectionResult {
            deltas_applied,
            applied_through_seq: cursor,
        })
    }

    pub(crate) async fn admin_label_stats_projection_step<FList, FAck, FutList, FutAck>(
        &self,
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
        auth::require_admin(&caller)?;
        if args.max_deltas == 0 {
            return Err(RouterError::InvalidArgument(
                "max_deltas must be greater than zero".into(),
            ));
        }

        let shard = self.resolve_shard_for_projection(&args.logical_graph_name, args.shard_id)?;
        let result = self
            .advance_label_stats_projection(
                shard.graph_canister,
                args.shard_id,
                args.max_deltas,
                list_pending,
                ack_through,
            )
            .await?;

        let done = result.deltas_applied < args.max_deltas;
        Ok(AdminLabelStatsProjectionStepResult {
            shard_id: args.shard_id,
            deltas_drained: result.deltas_applied,
            deltas_applied: result.deltas_applied,
            done,
        })
    }

    fn resolve_shard_for_projection(
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
    use crate::types::AdminRegisterShardArgs;
    use gleaph_graph_kernel::entry::VertexLabelId;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::plan_exec::LabelStatsDelta;

    fn test_init_args() -> RouterInitArgs {
        RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
        }
    }

    fn graph_principal(n: u8) -> Principal {
        Principal::from_slice(&[n])
    }

    #[test]
    fn advance_label_stats_projection_applies_contiguous_prefix() {
        let store = RouterStore::new();
        let shard_id = ShardId::new(0);
        let graph = Principal::from_slice(&[9]);
        let deltas = vec![
            LabelStatsDeltaEventWire {
                mutation_id: 1,
                shard_event_seq: 1,
                label_stats_delta: LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(2), 3)],
                    edge: vec![],
                },
            },
            LabelStatsDeltaEventWire {
                mutation_id: 1,
                shard_event_seq: 2,
                label_stats_delta: LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(2), 1)],
                    edge: vec![],
                },
            },
        ];
        let mut acked = 0u64;

        let result = futures::executor::block_on(store.advance_label_stats_projection(
            graph,
            shard_id,
            10,
            |_graph, from_seq, _limit| {
                assert_eq!(from_seq, 1);
                async { Ok(deltas.clone()) }
            },
            |_graph, through_seq| {
                acked = through_seq;
                async { Ok(()) }
            },
        ))
        .expect("advance projection");

        assert_eq!(result.deltas_applied, 2);
        assert_eq!(result.applied_through_seq, 2);
        assert_eq!(acked, 2);
        assert_eq!(store.label_stats_projection_cursor(shard_id), 2);
        assert_eq!(
            store
                .vertex_label_stats(VertexLabelId::from_raw(2))
                .live_count,
            4
        );
    }

    #[test]
    fn advance_label_stats_projection_stops_on_gap() {
        let store = RouterStore::new();
        let shard_id = ShardId::new(0);
        let graph = Principal::from_slice(&[9]);
        let deltas = vec![LabelStatsDeltaEventWire {
            mutation_id: 1,
            shard_event_seq: 2,
            label_stats_delta: LabelStatsDelta::default(),
        }];

        let err = futures::executor::block_on(store.advance_label_stats_projection(
            graph,
            shard_id,
            10,
            |_graph, _from_seq, _limit| async { Ok(deltas) },
            |_graph, _through_seq| async { Ok(()) },
        ))
        .expect_err("gap should fail");

        assert!(matches!(err, RouterError::InvalidArgument(_)));
        assert_eq!(store.label_stats_projection_cursor(shard_id), 0);
    }

    #[test]
    fn admin_label_stats_projection_step_drains_outbox() {
        use crate::types::{GraphRegistryEntry, GraphStatus, ProvisioningState};
        use gleaph_graph_kernel::entry::GraphId;
        use std::collections::BTreeSet;

        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        crate::facade::auth::grant_admins(&[admin]);
        store
            .admin_register_graph(
                admin,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    graph_name: "g".to_owned(),
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
        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: ShardId::new(0),
                graph_canister: graph_principal(9),
                index_canister: graph_principal(10),
                logical_graph_name: "g".into(),
            },
        ))
        .expect("register shard");

        let deltas = vec![
            LabelStatsDeltaEventWire {
                mutation_id: 1,
                shard_event_seq: 1,
                label_stats_delta: LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(1), 2)],
                    edge: vec![],
                },
            },
            LabelStatsDeltaEventWire {
                mutation_id: 1,
                shard_event_seq: 2,
                label_stats_delta: LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(1), 1)],
                    edge: vec![],
                },
            },
        ];
        let mut acked_through = 0u64;

        let result = futures::executor::block_on(store.admin_label_stats_projection_step(
            admin,
            AdminLabelStatsProjectionStepArgs {
                logical_graph_name: "g".into(),
                shard_id: ShardId::new(0),
                max_deltas: 10,
            },
            |_graph, _from, _limit| async { Ok(deltas.clone()) },
            |_graph, through| {
                acked_through = through;
                async { Ok(()) }
            },
        ))
        .expect("projection step");

        assert_eq!(result.deltas_applied, 2);
        assert_eq!(acked_through, 2);
        assert_eq!(
            store
                .vertex_label_stats(VertexLabelId::from_raw(1))
                .live_count,
            3
        );
    }
}
