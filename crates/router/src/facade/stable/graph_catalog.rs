//! Logical graph name ↔ [`GraphId`] catalog (ADR 0011).

use gleaph_graph_kernel::bidirectional_catalog::CatalogError;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};

use crate::facade::stable::{
    ROUTER_GRAPH_CATALOG, ROUTER_GRAPHS, ROUTER_SHARDS, ROUTER_SHARDS_BY_GRAPH_ID,
};
use crate::state::RouterError;

pub(crate) fn lookup_graph_id(name: &str) -> Option<GraphId> {
    ROUTER_GRAPH_CATALOG.with_borrow(|catalog| catalog.get_id(name))
}

pub(crate) fn graph_name(graph_id: GraphId) -> Option<String> {
    ROUTER_GRAPH_CATALOG.with_borrow(|catalog| catalog.get_name(graph_id))
}

pub(crate) fn intern_graph_name(name: &str) -> Result<GraphId, RouterError> {
    ROUTER_GRAPH_CATALOG
        .with_borrow_mut(|catalog| catalog.get_or_insert(name))
        .map_err(|e| catalog_error_to_router(e, "graph"))
}

#[allow(
    dead_code,
    reason = "catalog migration and admin paths pending ADR 0011 rollout"
)]
pub(crate) fn insert_graph_name(name: &str, graph_id: GraphId) -> Result<(), RouterError> {
    ROUTER_GRAPH_CATALOG
        .with_borrow_mut(|catalog| catalog.insert_with_id(name, graph_id))
        .map_err(|e| catalog_error_to_router(e, "graph"))
}

#[allow(
    dead_code,
    reason = "catalog migration and admin paths pending ADR 0011 rollout"
)]
pub(crate) fn graph_entry(
    graph_id: GraphId,
) -> Option<gleaph_gql_ic::graph_registry::GraphRegistryEntry> {
    ROUTER_GRAPHS.with_borrow(|graphs| graphs.get(&graph_id))
}

pub(crate) fn catalog_error_to_router<Id: std::fmt::Display>(
    err: CatalogError<Id>,
    kind: &str,
) -> RouterError {
    match err {
        CatalogError::IdExhausted => RouterError::IdExhausted(kind.to_owned()),
        other => RouterError::Conflict(format!("{kind} catalog: {other}")),
    }
}

pub(crate) fn register_shard_index(graph_id: GraphId, shard_id: ShardId) {
    ROUTER_SHARDS_BY_GRAPH_ID.with_borrow_mut(|index| {
        let mut list = index.get(&graph_id).unwrap_or_default();
        if !list.shard_ids.contains(&shard_id) {
            list.shard_ids.push(shard_id);
            index.insert(graph_id, list);
        }
    });
}

pub(crate) fn unregister_shard_index(graph_id: GraphId, shard_id: ShardId) {
    ROUTER_SHARDS_BY_GRAPH_ID.with_borrow_mut(|index| {
        let Some(mut list) = index.get(&graph_id) else {
            return;
        };
        list.shard_ids.retain(|id| *id != shard_id);
        if list.shard_ids.is_empty() {
            index.remove(&graph_id);
        } else {
            index.insert(graph_id, list);
        }
    });
}

pub(crate) fn list_shards_for_graph_id(graph_id: GraphId) -> Vec<ShardRegistryEntry> {
    let shard_ids = ROUTER_SHARDS_BY_GRAPH_ID.with_borrow(|index| {
        index
            .get(&graph_id)
            .map(|list| list.shard_ids.clone())
            .unwrap_or_default()
    });
    let mut out = Vec::with_capacity(shard_ids.len());
    ROUTER_SHARDS.with_borrow(|shards| {
        for shard_id in shard_ids {
            if let Some(entry) = shards.get(&shard_id) {
                out.push(entry);
            }
        }
    });
    out
}
