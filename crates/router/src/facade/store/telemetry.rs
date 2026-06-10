//! Label telemetry derived from graph shard events.

use super::super::stable::label_telemetry::{AppliedLabelTelemetryKey, LabelShardKey, LabelStats};
use super::super::stable::{
    ROUTER_APPLIED_LABEL_TELEMETRY, ROUTER_EDGE_LABEL_LIVE_BY_SHARD, ROUTER_EDGE_LABEL_STATS,
    ROUTER_VERTEX_LABEL_LIVE_BY_SHARD, ROUTER_VERTEX_LABEL_STATS,
};
use super::{RouterStore, apply_label_delta};
use crate::types::{EdgeLabelId, ShardId, VertexLabelId};
use gleaph_graph_kernel::plan_exec::{LabelTelemetryEventWire, LabelUsageDelta};

impl RouterStore {
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

    pub fn apply_label_usage_delta(&self, shard_id: ShardId, delta: &LabelUsageDelta) {
        for (label_id, value) in &delta.vertex {
            apply_label_delta(
                label_id.raw(),
                shard_id,
                *value,
                &ROUTER_VERTEX_LABEL_STATS,
                &ROUTER_VERTEX_LABEL_LIVE_BY_SHARD,
            );
        }
        for (label_id, value) in &delta.edge {
            apply_label_delta(
                label_id.raw(),
                shard_id,
                *value,
                &ROUTER_EDGE_LABEL_STATS,
                &ROUTER_EDGE_LABEL_LIVE_BY_SHARD,
            );
        }
    }

    pub fn apply_label_telemetry_event(
        &self,
        shard_id: ShardId,
        event: &LabelTelemetryEventWire,
    ) -> bool {
        let key = AppliedLabelTelemetryKey::new(shard_id, event.shard_event_seq);
        if ROUTER_APPLIED_LABEL_TELEMETRY.with_borrow(|applied| applied.contains(&key)) {
            return false;
        }
        self.apply_label_usage_delta(shard_id, &event.label_usage_delta);
        ROUTER_APPLIED_LABEL_TELEMETRY.with_borrow_mut(|applied| {
            applied.insert(key);
        });
        true
    }
}
