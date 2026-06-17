//! Logical graph name ↔ [`GraphId`] catalog (ADR 0011).

use gleaph_graph_kernel::bidirectional_catalog::CatalogError;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{GraphShardKey, ShardId, ShardRegistryEntry};
use std::collections::BTreeSet;

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

/// Resolves a logical graph name to a `GraphId` with a matching `ROUTER_GRAPHS` entry.
pub(crate) fn resolve_registered_graph_id(name: &str) -> Result<GraphId, RouterError> {
    let graph_id = lookup_graph_id(name).ok_or_else(|| RouterError::NotFound(name.to_owned()))?;
    if graph_entry(graph_id).is_none() {
        return Err(RouterError::NotFound(name.to_owned()));
    }
    Ok(graph_id)
}

pub(crate) fn require_graph_registry_entry(graph_id: GraphId) -> Result<(), RouterError> {
    if graph_entry(graph_id).is_none() {
        return Err(RouterError::NotFound(format!(
            "graph {graph_id:?} not registered"
        )));
    }
    Ok(())
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

/// Fan-out listing via `ROUTER_SHARDS_BY_GRAPH_ID` (O(shards for graph)), not a full registry scan.
///
/// Validates index-local integrity only: duplicate ids, missing primary rows, and per-row
/// `graph_id` mismatches. Full bidirectional registry consistency is enforced on commit and by
/// `check_registry_invariants` in tests.
pub(crate) fn list_shards_for_graph_id(
    graph_id: GraphId,
) -> Result<Vec<ShardRegistryEntry>, RouterError> {
    let shard_ids = ROUTER_SHARDS_BY_GRAPH_ID.with_borrow(|index| {
        index
            .get(&graph_id)
            .map(|list| list.shard_ids.clone())
            .unwrap_or_default()
    });

    let mut indexed_unique = BTreeSet::new();
    for shard_id in &shard_ids {
        if !indexed_unique.insert(*shard_id) {
            return Err(RouterError::Internal(format!(
                "registry invariant violation: ROUTER_SHARDS_BY_GRAPH_ID[{graph_id:?}] duplicate shard {shard_id:?}"
            )));
        }
    }

    let mut out = Vec::with_capacity(indexed_unique.len());
    for shard_id in shard_ids {
        let entry = ROUTER_SHARDS
            .with_borrow(|shards| shards.get(&GraphShardKey::new(graph_id, shard_id)))
            .ok_or_else(|| {
                RouterError::Internal(format!(
                    "registry invariant violation: shard {shard_id:?} listed for graph {graph_id:?} but missing from ROUTER_SHARDS"
                ))
            })?;
        if entry.graph_id != graph_id {
            return Err(RouterError::Internal(format!(
                "registry invariant violation: shard {shard_id:?} has graph_id {:?}, expected {graph_id:?}",
                entry.graph_id
            )));
        }
        out.push(entry);
    }
    Ok(out)
}

/// Index-attached shards only — used for dispatch, index fan-out, and backfill orchestration.
pub(crate) fn list_live_shards_for_graph_id(
    graph_id: GraphId,
) -> Result<Vec<ShardRegistryEntry>, RouterError> {
    Ok(list_shards_for_graph_id(graph_id)?
        .into_iter()
        .filter(|entry| entry.index_attached)
        .collect())
}

/// Next graph-local [`ShardId`] for `admin_register_shard` (dense `0..n-1` growth).
pub(crate) fn next_graph_local_shard_id(graph_id: GraphId) -> ShardId {
    let shard_ids = ROUTER_SHARDS_BY_GRAPH_ID.with_borrow(|index| {
        index
            .get(&graph_id)
            .map(|list| list.shard_ids.clone())
            .unwrap_or_default()
    });
    let next = shard_ids
        .iter()
        .map(|id| id.raw())
        .max()
        .map(|max| max.saturating_add(1))
        .unwrap_or(0);
    ShardId::new(next)
}

pub(crate) fn lookup_shard_entry(
    graph_id: GraphId,
    shard_id: ShardId,
) -> Option<ShardRegistryEntry> {
    ROUTER_SHARDS.with_borrow(|shards| shards.get(&GraphShardKey::new(graph_id, shard_id)))
}
