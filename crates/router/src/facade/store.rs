//! Stateless facade over router stable storage.
//!
//! Storage domains (Phase 2):
//! - [`registry`] — graph and shard registration
//! - [`catalogs`] — federated label and property name resolution
//! - [`label_stats_projection`] — label stats projection from graph shard deltas (ADR 0015)
//! - [`idempotency`] — mutation ids and client mutation keys
//! - [`backfill`] — label, vertex property, and edge posting backfill cursors and shard orchestration

mod backfill;
pub(crate) mod bulk_load;
mod catalogs;
mod idempotency;
mod label_stats_projection;
pub(crate) mod provisioning;
mod registry;
mod registry_invariants;
mod schema_migration;
pub(crate) use schema_migration::real_index_migration_driver;
pub(crate) mod uniqueness;

#[cfg(test)]
pub(crate) mod tests;

use super::stable::{
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS, ROUTER_CONSTRAINT_NAME_CATALOG, ROUTER_EDGE_LABEL_CATALOG,
    ROUTER_EDGE_LABEL_LIVE_BY_SHARD, ROUTER_EDGE_LABEL_STATS, ROUTER_GRAPH_CATALOG,
    ROUTER_GRAPH_TYPE_CATALOG, ROUTER_GRAPHS, ROUTER_INDEX_NAME_CATALOG,
    ROUTER_LABEL_STATS_PROJECTION, ROUTER_MUTATION_BY_CLIENT_KEY, ROUTER_MUTATION_COUNTER,
    ROUTER_PROPERTY_CATALOG, ROUTER_PROVISIONING_BY_GRAPH, ROUTER_PROVISIONING_INTENT_LOCK,
    ROUTER_PROVISIONING_REQUESTS, ROUTER_SCHEMA_MIGRATIONS, ROUTER_SHARD_BY_GRAPH, ROUTER_SHARDS,
    ROUTER_SHARDS_BY_GRAPH_ID, ROUTER_UNIQUE_CONSTRAINTS, ROUTER_UNIQUE_RESERVATIONS,
    ROUTER_VERTEX_LABEL_CATALOG, ROUTER_VERTEX_LABEL_LIVE_BY_SHARD, ROUTER_VERTEX_LABEL_STATS,
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
        ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|m| m.clear_new());
        ROUTER_SCHEMA_MIGRATIONS.with_borrow_mut(|m| m.clear_new());
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
    use crate::facade::stable::index_name_catalog;
    use crate::facade::stable::indexed_catalog;
    use crate::init::RouterInitArgs;
    use crate::planner_stats::IndexCatalogEntry;
    use crate::types::{
        AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus, ProvisioningState,
    };
    use candid::Principal;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_graph_kernel::entry::{GraphId, PropertyId};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{EdgeIndexDirection, IndexedPropertyKind};
    use std::collections::BTreeSet;

    pub const GRAPH: &str = "tenant.main";

    /// Interns a single property name through the batch API so tests exercise the same
    /// interning path the CLI uses, without the `&[name.to_owned()]` noise at every site.
    pub fn intern_property(
        store: &RouterStore,
        admin: Principal,
        graph: &str,
        name: &str,
    ) -> PropertyId {
        store
            .admin_intern_properties(admin, graph, &[name.to_owned()])
            .expect("intern property")[0]
    }

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

    /// Registers an immediate-Active vertex index for one already-interned property so seed
    /// routing and aggregate fast-path tests resolve `active_vertex_physical_index` from the real
    /// catalog instead of failing closed. `label_id` matches the interned vertex label when the
    /// failing lookup proves one (`active_vertex_physical_index(_, Some(label), _)`); pass `0` for
    /// label-filter-free lookups (an omitted label matches any label id).
    pub fn register_active_vertex_index(
        store: &RouterStore,
        graph_id: GraphId,
        label_id: u16,
        property_name: &str,
    ) {
        let property_id = store
            .lookup_property_id(graph_id, property_name)
            .expect("interned property");
        let index_name_id = index_name_catalog::intern_index_name(
            graph_id,
            &format!("__test_active_vertex_index_{property_name}"),
        )
        .expect("intern index name");
        indexed_catalog::create_named_index(
            graph_id,
            index_name_id,
            IndexCatalogEntry {
                kind: IndexedPropertyKind::Vertex,
                vertex_label: None,
                edge_label: None,
                property: property_name.to_owned(),
                edge_direction: None,
            },
            property_id,
            label_id,
            None,
            false,
        )
        .expect("register active vertex index");
    }

    /// Registers an immediate-Active edge index for one already-interned edge label/property pair.
    /// `Any` direction covers every query direction (`index_applies_to_query`).
    pub fn register_active_edge_index(
        store: &RouterStore,
        graph_id: GraphId,
        edge_label_name: &str,
        property_name: &str,
    ) {
        let label_id = store
            .lookup_edge_label_id(graph_id, edge_label_name)
            .expect("interned edge label");
        let property_id = store
            .lookup_property_id(graph_id, property_name)
            .expect("interned property");
        let index_name_id = index_name_catalog::intern_index_name(
            graph_id,
            &format!("__test_active_edge_index_{property_name}"),
        )
        .expect("intern index name");
        indexed_catalog::create_named_index(
            graph_id,
            index_name_id,
            IndexCatalogEntry {
                kind: IndexedPropertyKind::Edge,
                vertex_label: None,
                edge_label: Some(edge_label_name.to_owned()),
                property: property_name.to_owned(),
                edge_direction: Some(EdgeDirection::AnyDirection),
            },
            property_id,
            label_id.raw(),
            Some(EdgeIndexDirection::Any),
            false,
        )
        .expect("register active edge index");
    }
}
