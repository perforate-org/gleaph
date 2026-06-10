//! Graph and shard registry plus router controller principals.

use super::super::stable::{
    ROUTER_CONTROLLERS, ROUTER_GRAPHS, ROUTER_PENDING_LOGICAL, ROUTER_SHARD_BY_GRAPH, ROUTER_SHARDS,
};
use crate::index_sync;
use crate::state::RouterError;
use crate::types::{AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus, ShardId};
use candid::Principal;
use gleaph_graph_kernel::federation::ShardRegistryEntry;

use super::{RouterStore, ic_time_ns, validate_metadata_name};

impl RouterStore {
    pub(super) fn commit_init_controllers(&self, controllers: &[Principal]) {
        ROUTER_CONTROLLERS.with_borrow_mut(|admins| {
            admins.clear();
            for p in controllers {
                admins.insert(*p);
            }
        });
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

    pub fn resolve_graph(
        &self,
        graph_name: &str,
        caller: Principal,
    ) -> Result<GraphRegistryEntry, RouterError> {
        let entry = ROUTER_GRAPHS
            .with_borrow(|graphs| graphs.get(&graph_name.to_string()))
            .ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))?;
        if caller != entry.owner && !entry.admins.contains(&caller) {
            return Err(RouterError::Forbidden);
        }
        if !matches!(entry.status, GraphStatus::Active | GraphStatus::ReadOnly) {
            return Err(RouterError::GraphUnavailable);
        }
        Ok(entry)
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
        let mut out = Vec::new();
        ROUTER_SHARDS.with_borrow(|shards| {
            for lazy in shards.iter() {
                let entry = lazy.value();
                if entry.logical_graph_name == logical_graph_name {
                    out.push(entry);
                }
            }
        });
        Ok(out)
    }

    pub fn admin_register_graph(
        &self,
        caller: Principal,
        entry: GraphRegistryEntry,
    ) -> Result<(), RouterError> {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        if ROUTER_GRAPHS.with_borrow(|g| g.contains_key(&entry.graph_name.clone())) {
            return Err(RouterError::Conflict(entry.graph_name.clone()));
        }
        ROUTER_GRAPHS.with_borrow_mut(|g| {
            g.insert(entry.graph_name.clone(), entry);
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
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        let mut entry = ROUTER_GRAPHS
            .with_borrow(|g| g.get(&graph_name.to_string()))
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
            g.insert(graph_name.to_string(), entry);
        });
        Ok(())
    }

    pub async fn admin_register_shard(
        &self,
        caller: Principal,
        args: AdminRegisterShardArgs,
    ) -> Result<(), RouterError> {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        if args.graph_canister == Principal::anonymous()
            || args.index_canister == Principal::anonymous()
        {
            return Err(RouterError::InvalidArgument(
                "graph and index principals must be non-anonymous".into(),
            ));
        }
        validate_metadata_name(&args.logical_graph_name)?;

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
            logical_graph_name: args.logical_graph_name.clone(),
            registered_at_ns,
        };

        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            index_sync::admin_set_shard_owner(
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
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        let entry = ROUTER_SHARDS
            .with_borrow(|s| s.get(&shard_id))
            .ok_or(RouterError::ShardNotRegistered)?;

        let _siblings: Vec<Principal> = self
            .list_shards_for_graph(&entry.logical_graph_name)?
            .into_iter()
            .map(|shard| shard.graph_canister)
            .filter(|graph| *graph != entry.graph_canister)
            .collect();

        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            index_sync::admin_clear_shard_owner(entry.index_canister, shard_id)
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
        ROUTER_PENDING_LOGICAL.with_borrow_mut(|p| {
            p.remove(&entry.graph_canister);
        });
        Ok(())
    }
}
