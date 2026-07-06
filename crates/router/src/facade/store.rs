//! Stateless facade over router stable storage.
//!
//! Storage domains (Phase 2):
//! - [`registry`] — graph and shard registration
//! - [`catalogs`] — federated label and property name resolution
//! - [`label_stats_projection`] — label stats projection from graph shard deltas (ADR 0015)
//! - [`idempotency`] — mutation ids and client mutation keys
//! - [`backfill`] — label, vertex property, and edge posting backfill cursors and shard orchestration

mod backfill;
mod catalogs;
mod idempotency;
mod label_stats_projection;
pub(crate) mod provisioning;
mod registry;
mod registry_invariants;
pub(crate) mod uniqueness;

#[cfg(test)]
mod tests;

use super::stable::{
    ROUTER_CONSTRAINT_NAME_CATALOG, ROUTER_EDGE_LABEL_CATALOG, ROUTER_EDGE_LABEL_LIVE_BY_SHARD,
    ROUTER_EDGE_LABEL_STATS, ROUTER_GRAPH_CATALOG, ROUTER_GRAPH_TYPE_CATALOG, ROUTER_GRAPHS,
    ROUTER_INDEX_NAME_CATALOG, ROUTER_LABEL_STATS_PROJECTION, ROUTER_MUTATION_BY_CLIENT_KEY,
    ROUTER_MUTATION_COUNTER, ROUTER_PROPERTY_CATALOG, ROUTER_PROVISIONING_BY_GRAPH,
    ROUTER_PROVISIONING_INTENT_LOCK, ROUTER_PROVISIONING_REQUESTS, ROUTER_SHARD_BY_GRAPH,
    ROUTER_SHARDS, ROUTER_SHARDS_BY_GRAPH_ID, ROUTER_UNIQUE_CONSTRAINTS,
    ROUTER_UNIQUE_RESERVATIONS, ROUTER_VERTEX_LABEL_CATALOG, ROUTER_VERTEX_LABEL_LIVE_BY_SHARD,
    ROUTER_VERTEX_LABEL_STATS,
};
use crate::init::RouterInitArgs;
use crate::state::RouterError;
use gleaph_graph_kernel::plan_exec::MutationId;

/// Maximum UTF-8 byte length for graph and catalog metadata names.
pub(crate) const MAX_METADATA_NAME_BYTES: usize = 256;
const MAX_CLIENT_MUTATION_KEY_BYTES: usize = 256;
pub(crate) const CLIENT_MUTATION_KEY_TTL_NS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;
/// How long a routing reservation (`routing_in_progress`) is honored before a retry may
/// reclaim it (ADR 0029 Phase 4). The routing phase only issues read-only index lookups
/// before persisting the dispatch envelope, so an owner that has not released the lease
/// within this window is treated as crashed; reclaiming is safe because no canonical write
/// has happened yet (the envelope, which clears the lease, is persisted before any shard
/// DML). Generous relative to a healthy routing pass to avoid racing a slow-but-live owner.
pub(crate) const ROUTING_LEASE_TTL_NS: u64 = 5 * 60 * 1_000_000_000;

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

    pub fn init_from_args(&self, _args: &RouterInitArgs) {
        ROUTER_GRAPHS.with_borrow_mut(|g| g.clear_new());
        ROUTER_SHARDS.with_borrow_mut(|s| s.clear_new());
        ROUTER_SHARD_BY_GRAPH.with_borrow_mut(|m| m.clear_new());
        ROUTER_SHARDS_BY_GRAPH_ID.with_borrow_mut(|m| m.clear_new());
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|c| {
            c.set(0);
        });
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| m.clear_new());
        ROUTER_GRAPH_CATALOG.with_borrow_mut(|c| c.clear_new());
        ROUTER_GRAPH_TYPE_CATALOG.with_borrow_mut(|c| c.clear_new());
        ROUTER_INDEX_NAME_CATALOG.with_borrow_mut(|c| c.clear_new());
        ROUTER_CONSTRAINT_NAME_CATALOG.with_borrow_mut(|c| c.clear_new());
        ROUTER_UNIQUE_CONSTRAINTS.with_borrow_mut(|m| m.clear_new());
        ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_CATALOG.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_CATALOG.with_borrow_mut(|m| m.clear_new());
        ROUTER_PROPERTY_CATALOG.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_STATS.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_STATS.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_LIVE_BY_SHARD.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_LIVE_BY_SHARD.with_borrow_mut(|m| m.clear_new());
        ROUTER_LABEL_STATS_PROJECTION.replace(super::stable::memory::init_label_stats_projection());
        ROUTER_PROVISIONING_REQUESTS.with_borrow_mut(|m| m.clear_new());
        ROUTER_PROVISIONING_BY_GRAPH.with_borrow_mut(|m| m.clear_new());
        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow_mut(|m| m.clear_new());
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

#[cfg(test)]
pub(crate) mod catalog_test_support {
    use super::RouterStore;
    use crate::facade::auth;
    use crate::init::RouterInitArgs;
    use crate::types::{
        AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus, ProvisioningState,
    };
    use candid::Principal;
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::ShardId;
    use std::collections::BTreeSet;

    pub const GRAPH: &str = "tenant.main";

    pub fn register_graph(store: &RouterStore, admin: Principal, name: &str) {
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

    pub fn register_shard(store: &RouterStore, admin: Principal, shard_id: ShardId) {
        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id,
                graph_canister: Principal::from_slice(&[1]),
                index_canister: Principal::from_slice(&[2]),
                logical_graph_name: GRAPH.into(),
            },
        ))
        .expect("register shard");
    }

    pub fn setup() -> (RouterStore, Principal, GraphId) {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            provision_canister: None,
        });
        let admin = Principal::from_slice(&[1; 29]);
        auth::grant_admins(&[admin]);
        register_graph(&store, admin, GRAPH);
        let graph_id = store.resolve_graph_id(GRAPH).expect("graph id");
        (store, admin, graph_id)
    }

    pub fn setup_with_shard(shard_id: ShardId) -> (RouterStore, Principal, GraphId) {
        let (store, admin, graph_id) = setup();
        register_shard(&store, admin, shard_id);
        (store, admin, graph_id)
    }
}
