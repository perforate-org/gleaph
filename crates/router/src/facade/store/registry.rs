//! Graph and shard registry.

use super::super::stable::graph_catalog::{
    self, intern_graph_name, list_shards_for_graph_id, lookup_graph_id, register_shard_index,
    unregister_shard_index,
};
use super::super::stable::{ROUTER_GRAPHS, ROUTER_SHARD_BY_GRAPH, ROUTER_SHARDS};
use crate::facade::auth;
#[cfg(not(feature = "pocket-ic-e2e"))]
use crate::index_sync;
use crate::state::RouterError;
use crate::types::{AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus, ShardId};
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardRegistryEntry;

use super::{RouterStore, ic_time_ns, validate_metadata_name};

impl RouterStore {
    pub fn resolve_graph(
        &self,
        graph_name: &str,
        caller: Principal,
    ) -> Result<GraphRegistryEntry, RouterError> {
        let graph_id = lookup_graph_id(graph_name)
            .ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))?;
        let entry = ROUTER_GRAPHS
            .with_borrow(|graphs| graphs.get(&graph_id))
            .ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))?;
        if caller != entry.owner && !entry.admins.contains(&caller) {
            return Err(RouterError::Forbidden);
        }
        if !matches!(entry.status, GraphStatus::Active | GraphStatus::ReadOnly) {
            return Err(RouterError::GraphUnavailable);
        }
        Ok(entry)
    }

    pub fn resolve_graph_id(&self, graph_name: &str) -> Result<GraphId, RouterError> {
        lookup_graph_id(graph_name).ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))
    }

    pub fn list_visible_graph_ids(&self, caller: Principal) -> Result<Vec<GraphId>, RouterError> {
        let mut out = Vec::new();
        ROUTER_GRAPHS.with_borrow(|graphs| {
            for lazy in graphs.iter() {
                let entry = lazy.value();
                if caller != entry.owner && !entry.admins.contains(&caller) {
                    continue;
                }
                if matches!(entry.status, GraphStatus::Active | GraphStatus::ReadOnly) {
                    out.push(entry.graph_id);
                }
            }
        });
        Ok(out)
    }

    /// Resolve HOME graph for `caller` (ADR 0011 §1.3).
    ///
    /// Prefer exactly one visible graph with `is_home`; otherwise fall back to the sole
    /// visible graph (degenerate case A).
    pub fn resolve_home_graph_id(&self, caller: Principal) -> Result<GraphId, RouterError> {
        let mut home_marked = Vec::new();
        let mut visible = Vec::new();
        ROUTER_GRAPHS.with_borrow(|graphs| {
            for lazy in graphs.iter() {
                let entry = lazy.value();
                if caller != entry.owner && !entry.admins.contains(&caller) {
                    continue;
                }
                if !matches!(entry.status, GraphStatus::Active | GraphStatus::ReadOnly) {
                    continue;
                }
                visible.push(entry.graph_id);
                if entry.is_home {
                    home_marked.push(entry.graph_id);
                }
            }
        });
        match home_marked.as_slice() {
            [only] => Ok(*only),
            [] => match visible.as_slice() {
                [only] => Ok(*only),
                [] => Err(RouterError::InvalidArgument("no graph context".into())),
                _ => Err(RouterError::InvalidArgument(
                    "HOME_GRAPH is ambiguous: multiple graphs visible to caller".into(),
                )),
            },
            _ => Err(RouterError::InvalidArgument(
                "HOME_GRAPH is ambiguous: multiple graphs marked is_home".into(),
            )),
        }
    }

    pub fn resolve_shard(&self, shard_id: ShardId) -> Result<ShardRegistryEntry, RouterError> {
        ROUTER_SHARDS
            .with_borrow(|shards| shards.get(&shard_id))
            .ok_or(RouterError::ShardNotRegistered)
    }

    /// Returns all shard registrations for a logical graph (for federated query fan-out).
    pub fn list_shards_for_graph(
        &self,
        logical_graph_name: &str,
    ) -> Result<Vec<ShardRegistryEntry>, RouterError> {
        validate_metadata_name(logical_graph_name)?;
        let graph_id = lookup_graph_id(logical_graph_name)
            .ok_or_else(|| RouterError::NotFound(logical_graph_name.to_owned()))?;
        Ok(list_shards_for_graph_id(graph_id))
    }

    pub fn list_shards_for_graph_id(&self, graph_id: GraphId) -> Vec<ShardRegistryEntry> {
        list_shards_for_graph_id(graph_id)
    }

    pub fn admin_register_graph(
        &self,
        caller: Principal,
        mut entry: GraphRegistryEntry,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(&entry.graph_name)?;
        if lookup_graph_id(&entry.graph_name).is_some() {
            return Err(RouterError::Conflict(entry.graph_name.clone()));
        }
        let graph_id = intern_graph_name(&entry.graph_name)?;
        entry.graph_id = graph_id;
        if entry.is_home {
            let existing_home = ROUTER_GRAPHS.with_borrow(|graphs| {
                graphs.iter().find_map(|lazy| {
                    let existing = lazy.value();
                    existing.is_home.then(|| existing.graph_name.clone())
                })
            });
            if let Some(name) = existing_home {
                return Err(RouterError::Conflict(format!(
                    "home graph already registered as `{name}`"
                )));
            }
        }
        ROUTER_GRAPHS.with_borrow_mut(|g| {
            g.insert(graph_id, entry);
        });
        Ok(())
    }

    pub fn admin_update_graph_status(
        &self,
        caller: Principal,
        graph_name: &str,
        status: GraphStatus,
        version: u64,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        let graph_id = lookup_graph_id(graph_name)
            .ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))?;
        let mut entry = ROUTER_GRAPHS
            .with_borrow(|g| g.get(&graph_id))
            .ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))?;
        if entry.version != version {
            return Err(RouterError::Conflict(format!(
                "graph `{graph_name}` version mismatch: expected {}, got {}",
                entry.version, version
            )));
        }
        entry.status = status;
        entry.version = version.saturating_add(1);
        ROUTER_GRAPHS.with_borrow_mut(|g| {
            g.insert(graph_id, entry);
        });
        Ok(())
    }

    pub async fn admin_register_shard(
        &self,
        caller: Principal,
        args: AdminRegisterShardArgs,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        if args.graph_canister == Principal::anonymous()
            || args.index_canister == Principal::anonymous()
        {
            return Err(RouterError::InvalidArgument(
                "graph and index principals must be non-anonymous".into(),
            ));
        }
        validate_metadata_name(&args.logical_graph_name)?;
        let graph_id = lookup_graph_id(&args.logical_graph_name)
            .ok_or_else(|| RouterError::NotFound(args.logical_graph_name.clone()))?;

        let existing = ROUTER_SHARDS.with_borrow(|s| s.get(&args.shard_id));
        if let Some(entry) = existing {
            if entry.graph_canister != args.graph_canister
                || entry.index_canister != args.index_canister
            {
                return Err(RouterError::ShardAlreadyRegistered);
            }
            return Ok(());
        }
        if ROUTER_SHARD_BY_GRAPH
            .with_borrow(|m| m.get(&args.graph_canister))
            .is_some()
        {
            return Err(RouterError::Conflict(
                "graph canister already registered to a shard".into(),
            ));
        }

        let registered_at_ns = ic_time_ns();
        let entry = ShardRegistryEntry {
            shard_id: args.shard_id,
            graph_canister: args.graph_canister,
            index_canister: args.index_canister,
            graph_id,
            registered_at_ns,
        };

        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            index_sync::admin_attach_shard_canister(
                args.index_canister,
                args.shard_id,
                args.graph_canister,
            )
            .await
            .map_err(RouterError::Internal)?;
        }

        ROUTER_SHARDS.with_borrow_mut(|s| {
            s.insert(args.shard_id, entry);
        });
        ROUTER_SHARD_BY_GRAPH.with_borrow_mut(|m| {
            m.insert(args.graph_canister, args.shard_id);
        });
        register_shard_index(graph_id, args.shard_id);

        #[cfg(target_family = "wasm")]
        crate::peer_sync::sync_peers_after_shard_register(
            &args.logical_graph_name,
            args.graph_canister,
        )
        .await
        .map_err(RouterError::Internal)?;

        Ok(())
    }

    pub async fn admin_unregister_shard(
        &self,
        caller: Principal,
        shard_id: ShardId,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        let entry = ROUTER_SHARDS
            .with_borrow(|s| s.get(&shard_id))
            .ok_or(RouterError::ShardNotRegistered)?;

        let graph_name = graph_catalog::graph_name(entry.graph_id).unwrap_or_default();
        let _siblings: Vec<Principal> = self
            .list_shards_for_graph(&graph_name)?
            .into_iter()
            .map(|shard| shard.graph_canister)
            .filter(|graph| *graph != entry.graph_canister)
            .collect();

        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            index_sync::admin_detach_shard_canister(entry.index_canister, shard_id)
                .await
                .map_err(RouterError::Internal)?;
        }

        #[cfg(target_family = "wasm")]
        crate::peer_sync::sync_peers_after_shard_unregister(entry.graph_canister, &_siblings)
            .await
            .map_err(RouterError::Internal)?;

        ROUTER_SHARDS.with_borrow_mut(|s| {
            s.remove(&shard_id);
        });
        ROUTER_SHARD_BY_GRAPH.with_borrow_mut(|m| {
            m.remove(&entry.graph_canister);
        });
        unregister_shard_index(entry.graph_id, shard_id);
        Ok(())
    }
}
