//! Stateless facade over router stable storage.
//!
//! Storage domains (Phase 2):
//! - [`registry`] — graph and shard registration
//! - [`placement`] — logical vertex placement and reverse physical lookup
//! - [`catalogs`] — federated label and property name resolution
//! - [`telemetry`] — label usage aggregates from graph shard events
//! - [`idempotency`] — mutation ids and client mutation keys

mod catalogs;
mod idempotency;
mod placement;
mod registry;
mod telemetry;

#[cfg(test)]
mod tests;

use super::stable::label_telemetry::LabelShardKey;
use super::stable::{
    ROUTER_APPLIED_LABEL_TELEMETRY, ROUTER_CONTROLLERS, ROUTER_EDGE_LABEL_BY_ID,
    ROUTER_EDGE_LABEL_BY_NAME, ROUTER_EDGE_LABEL_LIVE_BY_SHARD, ROUTER_EDGE_LABEL_STATS,
    ROUTER_GRAPHS, ROUTER_LOGICAL_COUNTER, ROUTER_MUTATION_BY_CLIENT_KEY, ROUTER_MUTATION_COUNTER,
    ROUTER_PENDING_LOGICAL, ROUTER_PLACEMENT_BY_PHYSICAL, ROUTER_PLACEMENTS, ROUTER_PROPERTY_BY_ID,
    ROUTER_PROPERTY_BY_NAME, ROUTER_SHARD_BY_GRAPH, ROUTER_SHARDS, ROUTER_VERTEX_LABEL_BY_ID,
    ROUTER_VERTEX_LABEL_BY_NAME, ROUTER_VERTEX_LABEL_LIVE_BY_SHARD, ROUTER_VERTEX_LABEL_STATS,
};
use crate::init::RouterInitArgs;
use crate::state::RouterError;
use crate::types::{EdgeLabelId, ShardId, VertexLabelId};
use candid::Principal;
use gleaph_graph_kernel::entry::EDGE_LABEL_CATALOG_MAX;
use gleaph_graph_kernel::plan_exec::MutationId;

/// Maximum UTF-8 byte length for graph and catalog metadata names.
pub(crate) const MAX_METADATA_NAME_BYTES: usize = 256;
const MAX_CLIENT_MUTATION_KEY_BYTES: usize = 256;
pub(crate) const CLIENT_MUTATION_KEY_TTL_NS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientMutationReservation {
    pub mutation_id: MutationId,
    pub routing_owner: bool,
}

/// Stateless facade over router stable structures.
#[derive(Clone, Copy, Debug, Default)]
pub struct RouterStore;

impl RouterStore {
    pub const fn new() -> Self {
        Self
    }

    pub fn init_from_args(&self, args: &RouterInitArgs) {
        ROUTER_CONTROLLERS.with_borrow_mut(|admins| {
            admins.clear();
            for p in &args.controllers {
                admins.insert(*p);
            }
        });
        ROUTER_GRAPHS.with_borrow_mut(|g| g.clear_new());
        ROUTER_SHARDS.with_borrow_mut(|s| s.clear_new());
        ROUTER_SHARD_BY_GRAPH.with_borrow_mut(|m| m.clear_new());
        ROUTER_PLACEMENTS.with_borrow_mut(|p| p.clear_new());
        ROUTER_PLACEMENT_BY_PHYSICAL.with_borrow_mut(|p| p.clear_new());
        ROUTER_LOGICAL_COUNTER.with_borrow_mut(|c| {
            c.set(0);
        });
        ROUTER_PENDING_LOGICAL.with_borrow_mut(|p| p.clear_new());
        ROUTER_VERTEX_LABEL_BY_NAME.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_BY_ID.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_BY_NAME.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_BY_ID.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_STATS.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_STATS.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_LIVE_BY_SHARD.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_LIVE_BY_SHARD.with_borrow_mut(|m| m.clear_new());
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|c| {
            c.set(0);
        });
        ROUTER_APPLIED_LABEL_TELEMETRY.with_borrow_mut(|m| m.clear());
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| m.clear_new());
        ROUTER_PROPERTY_BY_NAME.with_borrow_mut(|m| m.clear_new());
        ROUTER_PROPERTY_BY_ID.with_borrow_mut(|m| m.clear_new());
    }

    pub fn bootstrap_controllers(&self, principals: &[Principal]) {
        ROUTER_CONTROLLERS.with_borrow_mut(|admins| {
            for p in principals {
                admins.insert(*p);
            }
        });
    }

    pub(crate) fn is_controller(&self, caller: Principal) -> bool {
        ROUTER_CONTROLLERS.with_borrow(|admins| admins.contains(&caller))
    }
}

pub(super) fn intern_vertex_label_name(name: &str) -> Result<VertexLabelId, RouterError> {
    if let Some(id) = ROUTER_VERTEX_LABEL_BY_NAME.with_borrow(|m| m.get(&name.to_string())) {
        return Ok(VertexLabelId::from_raw(id));
    }
    let next_id = ROUTER_VERTEX_LABEL_BY_ID
        .with_borrow(|m| m.keys().max().unwrap_or(0))
        .saturating_add(1);
    if next_id == 0 {
        return Err(RouterError::InvalidArgument(
            "vertex label id 0 is reserved".into(),
        ));
    }
    ROUTER_VERTEX_LABEL_BY_NAME.with_borrow_mut(|m| {
        m.insert(name.to_string(), next_id);
    });
    ROUTER_VERTEX_LABEL_BY_ID.with_borrow_mut(|m| {
        m.insert(next_id, name.to_string());
    });
    Ok(VertexLabelId::from_raw(next_id))
}

pub(super) fn intern_edge_label_name(name: &str) -> Result<EdgeLabelId, RouterError> {
    if let Some(id) = ROUTER_EDGE_LABEL_BY_NAME.with_borrow(|m| m.get(&name.to_string())) {
        return Ok(EdgeLabelId::from_raw(id));
    }
    let next_id = ROUTER_EDGE_LABEL_BY_ID
        .with_borrow(|m| m.keys().max().unwrap_or(0))
        .saturating_add(1);
    if next_id == 0 || next_id > EDGE_LABEL_CATALOG_MAX {
        return Err(RouterError::InvalidArgument(format!(
            "edge label id exhausted (max {EDGE_LABEL_CATALOG_MAX})"
        )));
    }
    ROUTER_EDGE_LABEL_BY_NAME.with_borrow_mut(|m| {
        m.insert(name.to_string(), next_id);
    });
    ROUTER_EDGE_LABEL_BY_ID.with_borrow_mut(|m| {
        m.insert(next_id, name.to_string());
    });
    Ok(EdgeLabelId::from_raw(next_id))
}

pub(super) fn apply_label_delta(
    label_id: u16,
    shard_id: ShardId,
    delta: i64,
    stats_map: &'static std::thread::LocalKey<
        std::cell::RefCell<super::stable::memory::StableLabelStatsMap>,
    >,
    live_by_shard: &'static std::thread::LocalKey<
        std::cell::RefCell<super::stable::memory::StableLabelShardLiveMap>,
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

pub(super) fn validate_metadata_name(name: &str) -> Result<(), RouterError> {
    if name.is_empty() {
        return Err(RouterError::InvalidArgument(
            "name must not be empty".into(),
        ));
    }
    if name.len() > MAX_METADATA_NAME_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "name exceeds {MAX_METADATA_NAME_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

pub(super) fn validate_client_mutation_key(key: &str) -> Result<(), RouterError> {
    if key.is_empty() {
        return Err(RouterError::InvalidArgument(
            "client_mutation_key must not be empty".into(),
        ));
    }
    if key.len() > MAX_CLIENT_MUTATION_KEY_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "client_mutation_key exceeds {MAX_CLIENT_MUTATION_KEY_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

pub(super) fn ic_time_ns() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::time()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}
