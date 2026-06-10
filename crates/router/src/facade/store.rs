//! Stateless facade over router stable storage.
//!
//! Storage domains (Phase 2):
//! - [`registry`] — graph and shard registration
//! - [`placement`] — logical vertex placement and reverse physical lookup
//! - [`catalogs`] — federated label and property name resolution
//! - [`telemetry`] — label usage aggregates from graph shard events
//! - [`telemetry_replay`] — drain graph label telemetry outbox into router aggregates
//! - [`idempotency`] — mutation ids and client mutation keys
//! - [`backfill`] — label and property posting backfill cursors and shard orchestration

mod backfill;
mod catalogs;
mod idempotency;
mod placement;
mod registry;
mod telemetry;
mod telemetry_replay;

#[cfg(test)]
mod tests;

use super::stable::{
    ROUTER_APPLIED_LABEL_TELEMETRY, ROUTER_EDGE_LABEL_CATALOG, ROUTER_EDGE_LABEL_LIVE_BY_SHARD,
    ROUTER_EDGE_LABEL_STATS, ROUTER_GRAPHS, ROUTER_LOGICAL_COUNTER, ROUTER_MUTATION_BY_CLIENT_KEY,
    ROUTER_MUTATION_COUNTER, ROUTER_PENDING_LOGICAL, ROUTER_PLACEMENT_BY_PHYSICAL,
    ROUTER_PLACEMENTS, ROUTER_PROPERTY_CATALOG, ROUTER_SHARD_BY_GRAPH, ROUTER_SHARDS,
    ROUTER_VERTEX_LABEL_CATALOG, ROUTER_VERTEX_LABEL_LIVE_BY_SHARD, ROUTER_VERTEX_LABEL_STATS,
};
use crate::init::RouterInitArgs;
use crate::state::RouterError;
use candid::Principal;
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
        self.commit_init_controllers(&args.controllers);
        ROUTER_GRAPHS.with_borrow_mut(|g| g.clear_new());
        ROUTER_SHARDS.with_borrow_mut(|s| s.clear_new());
        ROUTER_SHARD_BY_GRAPH.with_borrow_mut(|m| m.clear_new());
        ROUTER_PLACEMENTS.with_borrow_mut(|p| p.clear_new());
        ROUTER_PLACEMENT_BY_PHYSICAL.with_borrow_mut(|p| p.clear_new());
        ROUTER_LOGICAL_COUNTER.with_borrow_mut(|c| {
            c.set(0);
        });
        ROUTER_PENDING_LOGICAL.with_borrow_mut(|p| p.clear_new());
        ROUTER_VERTEX_LABEL_CATALOG.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_CATALOG.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_STATS.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_STATS.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_LIVE_BY_SHARD.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_LIVE_BY_SHARD.with_borrow_mut(|m| m.clear_new());
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|c| {
            c.set(0);
        });
        ROUTER_APPLIED_LABEL_TELEMETRY.with_borrow_mut(|m| m.clear());
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| m.clear_new());
        ROUTER_PROPERTY_CATALOG.with_borrow_mut(|m| m.clear_new());
    }
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
