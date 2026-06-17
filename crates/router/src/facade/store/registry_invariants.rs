//! Registry denormalization invariants across five stable regions.
//!
//! At every commit boundary the following must hold:
//! - `ROUTER_GRAPH_CATALOG` ↔ `ROUTER_GRAPHS` (name ↔ `GraphId`, entry.graph_id matches key)
//! - `ROUTER_SHARDS` ↔ `ROUTER_SHARD_BY_GRAPH` (`graph_canister` ↔ `GraphShardKey`)
//! - `ROUTER_SHARDS` ↔ `ROUTER_SHARDS_BY_GRAPH_ID` (`graph_id` ↔ shard list)
//! - every shard `graph_id` exists in `ROUTER_GRAPHS`

use super::super::stable::{
    ROUTER_GRAPH_CATALOG, ROUTER_GRAPH_RUNTIME_CONFIG, ROUTER_GRAPHS, ROUTER_SHARD_BY_GRAPH,
    ROUTER_SHARDS, ROUTER_SHARDS_BY_GRAPH_ID,
};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{GraphShardKey, ShardId};
use std::collections::{BTreeMap, BTreeSet};

/// Returns `Ok(())` when all registry denormalization invariants hold.
pub(super) fn check_registry_invariants() -> Result<(), String> {
    let mut graph_ids_in_registry = BTreeSet::new();

    ROUTER_GRAPHS.with_borrow(|graphs| -> Result<(), String> {
        for lazy in graphs.iter() {
            let graph_id = *lazy.key();
            let entry = lazy.value();
            graph_ids_in_registry.insert(graph_id);

            if entry.graph_id != graph_id {
                return Err(format!(
                    "ROUTER_GRAPHS[{graph_id:?}].graph_id is {:?}, expected key",
                    entry.graph_id
                ));
            }

            let catalog_name = ROUTER_GRAPH_CATALOG
                .with_borrow(|catalog| catalog.get_name(graph_id))
                .ok_or_else(|| {
                    format!("ROUTER_GRAPHS[{graph_id:?}] missing from ROUTER_GRAPH_CATALOG")
                })?;
            if catalog_name != entry.graph_name {
                return Err(format!(
                    "ROUTER_GRAPH_CATALOG name `{catalog_name}` != ROUTER_GRAPHS graph_name `{}`",
                    entry.graph_name
                ));
            }
            let catalog_id = ROUTER_GRAPH_CATALOG
                .with_borrow(|catalog| catalog.get_id(&entry.graph_name))
                .ok_or_else(|| {
                    format!(
                        "ROUTER_GRAPHS graph_name `{}` missing from ROUTER_GRAPH_CATALOG",
                        entry.graph_name
                    )
                })?;
            if catalog_id != graph_id {
                return Err(format!(
                    "ROUTER_GRAPH_CATALOG id for `{}` is {catalog_id:?}, expected {graph_id:?}",
                    entry.graph_name
                ));
            }
        }
        Ok(())
    })?;

    ROUTER_GRAPH_CATALOG.with_borrow(|catalog| -> Result<(), String> {
        for graph_id in catalog.iter_ids() {
            if !graph_ids_in_registry.contains(&graph_id) {
                let name = catalog
                    .get_name(graph_id)
                    .unwrap_or_else(|| "<unknown>".to_owned());
                return Err(format!(
                    "ROUTER_GRAPH_CATALOG[{graph_id:?}] name `{name}` has no ROUTER_GRAPHS entry"
                ));
            }
        }
        Ok(())
    })?;

    let mut runtime_configs = BTreeMap::new();
    ROUTER_GRAPH_RUNTIME_CONFIG.with_borrow(|configs| -> Result<(), String> {
        for lazy in configs.iter() {
            let graph_id = *lazy.key();
            let runtime = lazy.value();
            if !graph_ids_in_registry.contains(&graph_id) {
                return Err(format!(
                    "ROUTER_GRAPH_RUNTIME_CONFIG[{graph_id:?}] has no ROUTER_GRAPHS entry"
                ));
            }
            if runtime.index_group_size == 0 {
                return Err(format!(
                    "ROUTER_GRAPH_RUNTIME_CONFIG[{graph_id:?}] has index_group_size=0"
                ));
            }
            if runtime
                .index_cluster
                .iter()
                .any(|principal| *principal == candid::Principal::anonymous())
            {
                return Err(format!(
                    "ROUTER_GRAPH_RUNTIME_CONFIG[{graph_id:?}] has anonymous index_cluster member"
                ));
            }
            runtime_configs.insert(graph_id, runtime.clone());
        }
        Ok(())
    })?;
    for graph_id in &graph_ids_in_registry {
        if !runtime_configs.contains_key(graph_id) {
            return Err(format!(
                "ROUTER_GRAPHS[{graph_id:?}] missing ROUTER_GRAPH_RUNTIME_CONFIG entry"
            ));
        }
    }

    let mut shards_by_graph: BTreeMap<GraphId, BTreeSet<ShardId>> = BTreeMap::new();

    ROUTER_SHARDS.with_borrow(|shards| -> Result<(), String> {
        for lazy in shards.iter() {
            let key = *lazy.key();
            let entry = lazy.value();

            if entry.shard_id != key.shard_id {
                return Err(format!(
                    "ROUTER_SHARDS[{key:?}].shard_id is {:?}, expected key shard_id",
                    entry.shard_id
                ));
            }
            if entry.graph_id != key.graph_id {
                return Err(format!(
                    "ROUTER_SHARDS[{key:?}].graph_id is {:?}, expected key graph_id",
                    entry.graph_id
                ));
            }
            if !graph_ids_in_registry.contains(&entry.graph_id) {
                return Err(format!(
                    "ROUTER_SHARDS[{key:?}].graph_id {:?} not in ROUTER_GRAPHS",
                    entry.graph_id
                ));
            }
            let runtime = runtime_configs.get(&entry.graph_id).ok_or_else(|| {
                format!(
                    "ROUTER_SHARDS[{key:?}] graph {:?} missing ROUTER_GRAPH_RUNTIME_CONFIG",
                    entry.graph_id
                )
            })?;
            let group_index = usize::try_from(key.shard_id.raw() / runtime.index_group_size)
                .map_err(|_| format!("ROUTER_SHARDS[{key:?}] group index overflow"))?;
            let expected_index_canister = runtime.index_cluster.get(group_index).ok_or_else(|| {
                format!(
                    "ROUTER_SHARDS[{key:?}] group {group_index} out of runtime index_cluster bounds {}",
                    runtime.index_cluster.len()
                )
            })?;
            if entry.index_canister != *expected_index_canister {
                return Err(format!(
                    "ROUTER_SHARDS[{key:?}] index_canister {:?} != runtime index_cluster[{group_index}] {:?}",
                    entry.index_canister, expected_index_canister
                ));
            }

            shards_by_graph
                .entry(entry.graph_id)
                .or_default()
                .insert(key.shard_id);

            let mapped_key = ROUTER_SHARD_BY_GRAPH
                .with_borrow(|m| m.get(&entry.graph_canister))
                .ok_or_else(|| {
                    format!(
                        "ROUTER_SHARDS[{key:?}].graph_canister {:?} missing from ROUTER_SHARD_BY_GRAPH",
                        entry.graph_canister
                    )
                })?;
            if mapped_key != key {
                return Err(format!(
                    "ROUTER_SHARD_BY_GRAPH[{:?}] is {mapped_key:?}, expected {key:?}",
                    entry.graph_canister
                ));
            }
        }
        Ok(())
    })?;

    ROUTER_SHARD_BY_GRAPH.with_borrow(|m| -> Result<(), String> {
        for lazy in m.iter() {
            let principal = *lazy.key();
            let key = lazy.value();
            let entry = ROUTER_SHARDS
                .with_borrow(|shards| shards.get(&key))
                .ok_or_else(|| {
                    format!(
                        "ROUTER_SHARD_BY_GRAPH[{principal:?}] -> {key:?} missing from ROUTER_SHARDS"
                    )
                })?;
            if entry.graph_canister != principal {
                return Err(format!(
                    "ROUTER_SHARD_BY_GRAPH[{principal:?}] -> {key:?} but ROUTER_SHARDS graph_canister is {:?}",
                    entry.graph_canister
                ));
            }
        }
        Ok(())
    })?;

    ROUTER_SHARDS_BY_GRAPH_ID.with_borrow(|index| -> Result<(), String> {
        for lazy in index.iter() {
            let graph_id = *lazy.key();
            let list = lazy.value();

            if !graph_ids_in_registry.contains(&graph_id) {
                return Err(format!(
                    "ROUTER_SHARDS_BY_GRAPH_ID[{graph_id:?}] not in ROUTER_GRAPHS"
                ));
            }

            let mut seen = BTreeSet::new();
            for shard_id in &list.shard_ids {
                if !seen.insert(*shard_id) {
                    return Err(format!(
                        "ROUTER_SHARDS_BY_GRAPH_ID[{graph_id:?}] duplicate shard {shard_id:?}"
                    ));
                }
                let entry = ROUTER_SHARDS
                    .with_borrow(|shards| {
                        shards.get(&GraphShardKey::new(graph_id, *shard_id))
                    })
                    .ok_or_else(|| {
                        format!(
                            "ROUTER_SHARDS_BY_GRAPH_ID[{graph_id:?}] lists {shard_id:?} missing from ROUTER_SHARDS"
                        )
                    })?;
                if entry.graph_id != graph_id {
                    return Err(format!(
                        "ROUTER_SHARDS_BY_GRAPH_ID[{graph_id:?}] lists {shard_id:?} with graph_id {:?}",
                        entry.graph_id
                    ));
                }
            }

            let indexed = shards_by_graph.get(&graph_id).cloned().unwrap_or_default();
            if indexed != seen {
                return Err(format!(
                    "ROUTER_SHARDS_BY_GRAPH_ID[{graph_id:?}] shard set {seen:?} != ROUTER_SHARDS-derived {indexed:?}"
                ));
            }
        }
        Ok(())
    })?;

    for (graph_id, shard_set) in &shards_by_graph {
        let indexed = ROUTER_SHARDS_BY_GRAPH_ID
            .with_borrow(|index| index.get(graph_id).map(|list| list.shard_ids.clone()))
            .unwrap_or_default();
        let mut indexed_unique = BTreeSet::new();
        for shard_id in &indexed {
            if !indexed_unique.insert(*shard_id) {
                return Err(format!(
                    "ROUTER_SHARDS_BY_GRAPH_ID[{graph_id:?}] duplicate shard {shard_id:?}"
                ));
            }
        }
        if indexed_unique != *shard_set {
            return Err(format!(
                "ROUTER_SHARDS graph_id {graph_id:?} shards {shard_set:?} missing from ROUTER_SHARDS_BY_GRAPH_ID (has {indexed_unique:?})"
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
pub(crate) fn assert_registry_invariants() {
    check_registry_invariants().expect("registry invariants");
}
