//! Graph and shard registry.

use super::super::stable::graph_catalog::{
    self, intern_graph_name, list_live_shards_for_graph_id, list_shards_for_graph_id,
    lookup_graph_id, lookup_shard_entry, next_graph_local_shard_id, require_graph_registry_entry,
    resolve_registered_graph_id,
};
use super::super::stable::{
    ROUTER_EDGE_LABEL_CATALOG, ROUTER_EDGE_LABEL_LIVE_BY_SHARD, ROUTER_EDGE_LABEL_STATS,
    ROUTER_EDGE_PAYLOAD_PROFILES, ROUTER_GQL_GRAPH_CATALOG, ROUTER_GRAPH_CATALOG,
    ROUTER_GRAPH_RUNTIME_CONFIG, ROUTER_GRAPHS, ROUTER_INDEX_NAME_CATALOG, ROUTER_PROPERTY_CATALOG,
    ROUTER_SHARD_BY_GRAPH, ROUTER_SHARDS, ROUTER_SHARDS_BY_GRAPH_ID, ROUTER_VERTEX_LABEL_CATALOG,
    ROUTER_VERTEX_LABEL_LIVE_BY_SHARD, ROUTER_VERTEX_LABEL_STATS,
};
#[cfg(test)]
use super::registry_invariants::check_registry_invariants;
use crate::facade::auth;
use crate::facade::stable::constraint_catalog;
#[cfg(not(feature = "pocket-ic-e2e"))]
use crate::index_sync;
use crate::state::RouterError;
use crate::types::{
    AdminAttachVectorIndexShardArgs, AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus,
    ShardId,
};
#[cfg(not(feature = "pocket-ic-e2e"))]
use crate::vector_sync;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GraphShardKey, ShardRegistryEntry};

use super::{RouterStore, ic_time_ns, validate_metadata_name};

/// Per-graph tenancy predicate shared by name resolution and [`RouterStore::resolve_graph`].
///
/// A caller may access a graph's metadata/routing data when it is the graph `owner`, listed in
/// `admins`, a global canister `Admin` (superuser bypass), or the graph's own registered shard
/// canister. The last case keeps federation/index-routing inter-canister calls working: shards
/// call the router with their `graph_canister` principal, which is keyed in
/// [`ROUTER_SHARD_BY_GRAPH`].
fn caller_may_access_graph(
    entry: &GraphRegistryEntry,
    graph_id: GraphId,
    caller: Principal,
) -> bool {
    if auth::is_admin(&caller) || caller == entry.owner || entry.admins.contains(&caller) {
        return true;
    }
    ROUTER_SHARD_BY_GRAPH
        .with_borrow(|m| m.get(&caller))
        .is_some_and(|key| key.graph_id == graph_id)
}

/// Rejects registration whose tenancy fields cannot form a trustworthy boundary.
///
/// The anonymous principal as `owner` or in `admins` would make [`caller_may_access_graph`]
/// match every unauthenticated caller, silently turning the graph world-readable. Validated
/// before any registration state is mutated so a rejected request leaves no orphaned name.
fn validate_registration_principals(entry: &GraphRegistryEntry) -> Result<(), RouterError> {
    if entry.owner == Principal::anonymous() {
        return Err(RouterError::InvalidArgument(
            "graph owner must not be the anonymous principal".into(),
        ));
    }
    if entry.admins.contains(&Principal::anonymous()) {
        return Err(RouterError::InvalidArgument(
            "graph admins must not include the anonymous principal".into(),
        ));
    }
    Ok(())
}

const ELEMENT_ID_KEY_DERIVATION_DOMAIN: &[u8] = b"gleaph:element-id-key:v1";
/// Deterministic entropy for host unit tests and `admin_register_graph` (not IC `raw_rand`).
const HOST_GRAPH_REGISTRATION_ENTROPY: &[u8] = b"router-test-entropy-seed-000000000000";

fn shard_group_index(shard_id: ShardId, index_group_size: u32) -> Result<usize, RouterError> {
    crate::index_route::index_group_index(shard_id, index_group_size).ok_or_else(|| {
        RouterError::InvalidArgument("index_group_size must be greater than zero".into())
    })
}

#[cfg(not(feature = "pocket-ic-e2e"))]
fn shard_group_index_u32(shard_id: ShardId, index_group_size: u32) -> Result<u32, RouterError> {
    if index_group_size == 0 {
        return Err(RouterError::InvalidArgument(
            "index_group_size must be greater than zero".into(),
        ));
    }
    Ok(shard_id.raw() / index_group_size)
}

fn validate_index_group_canister_assignment(
    graph_id: GraphId,
    shard_id: ShardId,
    index_canister: Principal,
) -> Result<(), RouterError> {
    ROUTER_GRAPH_RUNTIME_CONFIG.with_borrow(|cfg| {
        let runtime = cfg.get(&graph_id).ok_or_else(|| {
            RouterError::NotFound(format!("runtime config for graph {graph_id}"))
        })?;
        let group_index = shard_group_index(shard_id, runtime.index_group_size)?;
        if let Some(assigned) = runtime.index_cluster.get(group_index)
            && *assigned != index_canister
        {
            return Err(RouterError::Conflict(format!(
                "index canister mismatch for graph {graph_id} group {group_index}: expected {assigned}, got {index_canister}",
            )));
        }
        Ok(())
    })
}

fn commit_index_group_canister_assignment(
    graph_id: GraphId,
    shard_id: ShardId,
    index_canister: Principal,
) -> Result<(), RouterError> {
    ROUTER_GRAPH_RUNTIME_CONFIG.with_borrow_mut(|cfg| {
        let mut runtime = cfg.get(&graph_id).ok_or_else(|| {
            RouterError::NotFound(format!("runtime config for graph {graph_id}"))
        })?;
        let group_index = shard_group_index(shard_id, runtime.index_group_size)?;
        if group_index >= runtime.index_cluster.len() {
            runtime.index_cluster.resize(group_index + 1, index_canister);
        } else if runtime.index_cluster[group_index] != index_canister {
            return Err(RouterError::Conflict(format!(
                "index canister mismatch for graph {graph_id} group {group_index}: expected {}, got {index_canister}",
                runtime.index_cluster[group_index],
            )));
        }
        cfg.insert(graph_id, runtime);
        Ok(())
    })
}

fn reconcile_index_cluster_after_shard_removal(graph_id: GraphId) -> Result<(), RouterError> {
    ROUTER_GRAPH_RUNTIME_CONFIG.with_borrow_mut(|cfg| {
        let mut runtime = cfg
            .get(&graph_id)
            .ok_or_else(|| RouterError::NotFound(format!("runtime config for graph {graph_id}")))?;
        let max_group = ROUTER_SHARDS.with_borrow(|shards| {
            shards
                .iter()
                .filter_map(|lazy| {
                    let key = *lazy.key();
                    if key.graph_id != graph_id {
                        return None;
                    }
                    shard_group_index(key.shard_id, runtime.index_group_size).ok()
                })
                .max()
        });
        match max_group {
            None => runtime.index_cluster.clear(),
            Some(max) => runtime.index_cluster.truncate(max + 1),
        }
        cfg.insert(graph_id, runtime);
        Ok(())
    })
}

#[cfg(not(feature = "pocket-ic-e2e"))]
fn rollback_failed_shard_registration(
    graph_id: GraphId,
    shard_id: ShardId,
) -> Result<(), RouterError> {
    let _ = RouterStore::commit_unregister_shard(graph_id, shard_id)?;
    reconcile_index_cluster_after_shard_removal(graph_id)
}

fn ensure_graph_registration_slot_available(
    graph_name: &str,
    is_home: bool,
) -> Result<(), RouterError> {
    validate_metadata_name(graph_name)?;
    if resolve_registered_graph_id(graph_name).is_ok() {
        return Err(RouterError::Conflict(graph_name.to_owned()));
    }
    if is_home {
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
    Ok(())
}

impl RouterStore {
    fn purge_graph_vocabulary_partitions(graph_id: GraphId) {
        ROUTER_VERTEX_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.remove_graph(graph_id));
        ROUTER_EDGE_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.remove_graph(graph_id));
        ROUTER_PROPERTY_CATALOG.with_borrow_mut(|catalog| catalog.remove_graph(graph_id));
        ROUTER_INDEX_NAME_CATALOG.with_borrow_mut(|catalog| catalog.remove_graph(graph_id));
        ROUTER_EDGE_PAYLOAD_PROFILES.with_borrow_mut(|store| store.remove_graph(graph_id));
        super::super::stable::indexed_catalog::purge_graph_indexes(graph_id);
        super::super::stable::constraint_catalog::purge_graph_constraints(graph_id);
        super::super::stable::reservation_catalog::purge_graph_reservations(graph_id);
        super::super::stable::unique_effect_pending::purge_graph(graph_id);
        super::super::stable::vector_index_catalog::purge_graph_vector_indexes(graph_id);
        super::super::stable::vector_maintenance_policy::purge_graph_policies(graph_id);
        super::super::stable::embedding_name_catalog::purge_graph_embedding_names(graph_id);
        super::super::stable::ROUTER_CONSTRAINT_NAME_CATALOG
            .with_borrow_mut(|catalog| catalog.remove_graph(graph_id));
        ROUTER_GQL_GRAPH_CATALOG.with_borrow_mut(|catalog| catalog.remove_graph_binding(graph_id));

        ROUTER_VERTEX_LABEL_STATS.with_borrow_mut(|map| {
            let keys: Vec<_> = map
                .iter()
                .filter_map(|entry| (entry.key().graph_id == graph_id).then_some(*entry.key()))
                .collect();
            for key in keys {
                map.remove(&key);
            }
        });
        ROUTER_EDGE_LABEL_STATS.with_borrow_mut(|map| {
            let keys: Vec<_> = map
                .iter()
                .filter_map(|entry| (entry.key().graph_id == graph_id).then_some(*entry.key()))
                .collect();
            for key in keys {
                map.remove(&key);
            }
        });
        ROUTER_VERTEX_LABEL_LIVE_BY_SHARD.with_borrow_mut(|map| {
            let keys: Vec<_> = map
                .iter()
                .filter_map(|entry| (entry.key().graph_id == graph_id).then_some(*entry.key()))
                .collect();
            for key in keys {
                map.remove(&key);
            }
        });
        ROUTER_EDGE_LABEL_LIVE_BY_SHARD.with_borrow_mut(|map| {
            let keys: Vec<_> = map
                .iter()
                .filter_map(|entry| (entry.key().graph_id == graph_id).then_some(*entry.key()))
                .collect();
            for key in keys {
                map.remove(&key);
            }
        });
    }

    /// Atomically interns the graph name and inserts the registry entry.
    pub(super) fn commit_register_graph(
        mut entry: GraphRegistryEntry,
        runtime_config: super::super::stable::memory::GraphRuntimeConfig,
    ) -> Result<GraphId, RouterError> {
        let graph_id = intern_graph_name(&entry.graph_name)?;
        if ROUTER_GRAPHS.with_borrow(|graphs| graphs.get(&graph_id).is_some()) {
            return Err(RouterError::Conflict(entry.graph_name.clone()));
        }
        entry.graph_id = graph_id;
        ROUTER_GRAPHS.with_borrow_mut(|g| {
            g.insert(graph_id, entry);
        });
        ROUTER_GRAPH_RUNTIME_CONFIG.with_borrow_mut(|cfg| {
            cfg.insert(graph_id, runtime_config);
        });
        Self::verify_registry_invariants_after_commit()?;
        Ok(graph_id)
    }

    /// Atomically inserts shard registry, canister map, and per-graph shard index.
    pub(super) fn commit_register_shard(entry: ShardRegistryEntry) -> Result<(), RouterError> {
        require_graph_registry_entry(entry.graph_id)?;
        let graph_id = entry.graph_id;
        let shard_id = entry.shard_id;
        let graph_canister = entry.graph_canister;
        let key = GraphShardKey::new(graph_id, shard_id);

        if ROUTER_SHARDS.with_borrow(|s| s.get(&key).is_some()) {
            return Err(RouterError::ShardAlreadyRegistered);
        }
        if ROUTER_SHARD_BY_GRAPH
            .with_borrow(|m| m.get(&graph_canister))
            .is_some()
        {
            return Err(RouterError::ShardAlreadyRegistered);
        }

        ROUTER_SHARDS.with_borrow_mut(|s| {
            s.insert(key, entry);
        });
        ROUTER_SHARD_BY_GRAPH.with_borrow_mut(|m| {
            m.insert(graph_canister, key);
        });
        ROUTER_SHARDS_BY_GRAPH_ID.with_borrow_mut(|index| {
            let mut list = index.get(&graph_id).unwrap_or_default();
            if !list.shard_ids.contains(&shard_id) {
                list.shard_ids.push(shard_id);
                index.insert(graph_id, list);
            }
        });

        Self::verify_registry_invariants_after_commit()
    }

    /// Atomically removes shard registry, canister map, and per-graph shard index.
    pub(super) fn commit_unregister_shard(
        graph_id: GraphId,
        shard_id: ShardId,
    ) -> Result<ShardRegistryEntry, RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        let entry = ROUTER_SHARDS
            .with_borrow(|s| s.get(&key))
            .ok_or(RouterError::ShardNotRegistered)?;

        ROUTER_SHARDS.with_borrow_mut(|s| {
            s.remove(&key);
        });
        ROUTER_SHARD_BY_GRAPH.with_borrow_mut(|m| {
            m.remove(&entry.graph_canister);
        });
        ROUTER_SHARDS_BY_GRAPH_ID.with_borrow_mut(|index| {
            let Some(mut list) = index.get(&entry.graph_id) else {
                return;
            };
            list.shard_ids.retain(|id| *id != shard_id);
            if list.shard_ids.is_empty() {
                index.remove(&entry.graph_id);
            } else {
                index.insert(entry.graph_id, list);
            }
        });
        // Drop the shard's derived posting-backfill cursors so a re-registered shard
        // reusing this key starts from a clean cursor instead of a stale one.
        super::backfill::purge_backfill_state(key);

        Self::verify_registry_invariants_after_commit()?;
        Ok(entry)
    }

    fn commit_set_shard_index_attached(
        graph_id: GraphId,
        shard_id: ShardId,
        index_attached: bool,
    ) -> Result<(), RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        ROUTER_SHARDS.with_borrow_mut(|shards| {
            let mut entry = shards.get(&key).ok_or(RouterError::ShardNotRegistered)?;
            entry.index_attached = index_attached;
            shards.insert(key, entry);
            Ok(())
        })?;
        Self::verify_registry_invariants_after_commit()
    }

    #[cfg(not(feature = "pocket-ic-e2e"))]
    async fn attach_shard_to_index(
        graph_id: GraphId,
        shard_id: ShardId,
        index_canister: Principal,
        graph_canister: Principal,
    ) -> Result<(), RouterError> {
        let runtime = ROUTER_GRAPH_RUNTIME_CONFIG
            .with_borrow(|cfg| cfg.get(&graph_id))
            .ok_or_else(|| RouterError::NotFound(format!("runtime config for graph {graph_id}")))?;
        let group_index = shard_group_index_u32(shard_id, runtime.index_group_size)?;
        index_sync::admin_attach_shard_canister(
            index_canister,
            graph_id,
            runtime.index_group_size,
            group_index,
            shard_id,
            graph_canister,
        )
        .await
        .map_err(RouterError::Internal)
    }

    #[cfg(not(feature = "pocket-ic-e2e"))]
    async fn detach_shard_from_index(
        index_canister: Principal,
        shard_id: ShardId,
    ) -> Result<(), RouterError> {
        index_sync::admin_detach_shard_canister(index_canister, shard_id)
            .await
            .map_err(RouterError::Internal)
    }

    #[cfg(not(feature = "pocket-ic-e2e"))]
    async fn finish_shard_index_attach(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
        index_canister: Principal,
        graph_canister: Principal,
    ) -> Result<(), RouterError> {
        if let Err(err) =
            Self::attach_shard_to_index(graph_id, shard_id, index_canister, graph_canister).await
        {
            let _ = rollback_failed_shard_registration(graph_id, shard_id);
            return Err(err);
        }
        if let Err(err) = Self::commit_set_shard_index_attached(graph_id, shard_id, true) {
            let _ = Self::detach_shard_from_index(index_canister, shard_id).await;
            if lookup_shard_entry(graph_id, shard_id).is_some() {
                let _ = rollback_failed_shard_registration(graph_id, shard_id);
            }
            return Err(err);
        }
        Ok(())
    }

    async fn complete_shard_index_attach(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
        index_canister: Principal,
        graph_canister: Principal,
    ) -> Result<(), RouterError> {
        #[cfg(feature = "pocket-ic-e2e")]
        {
            let _ = (index_canister, graph_canister);
            Self::commit_set_shard_index_attached(graph_id, shard_id, true)
        }

        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            self.finish_shard_index_attach(graph_id, shard_id, index_canister, graph_canister)
                .await
        }
    }

    /// Records this shard's derived vector-index target and durable readiness bit (ADR 0031
    /// Slice 4). The final step of the vector attach handshake; the `vector_index_attached` bit is
    /// the registry-side proxy for "graph-local routing set *and* shard attached to the vector
    /// canister", mirroring `index_attached`.
    fn commit_set_shard_vector_attached(
        graph_id: GraphId,
        shard_id: ShardId,
        vector_index_canister: Principal,
        vector_index_attached: bool,
    ) -> Result<(), RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        ROUTER_SHARDS.with_borrow_mut(|shards| {
            let mut entry = shards.get(&key).ok_or(RouterError::ShardNotRegistered)?;
            entry.vector_index_canister = Some(vector_index_canister);
            entry.vector_index_attached = vector_index_attached;
            shards.insert(key, entry);
            Ok::<(), RouterError>(())
        })?;
        Self::verify_registry_invariants_after_commit()
    }

    #[cfg(not(feature = "pocket-ic-e2e"))]
    async fn finish_shard_vector_attach(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
        vector_index_canister: Principal,
        graph_canister: Principal,
    ) -> Result<(), RouterError> {
        // Step 1: make the shard's *local* routing carry the target before anything observes it as
        // ready. If this fails we never recorded readiness, so there is nothing to roll back.
        vector_sync::admin_set_graph_vector_index_canister(graph_canister, vector_index_canister)
            .await
            .map_err(RouterError::Internal)?;
        // Step 2: attach the shard to the vector canister so it accepts the shard's subject sync.
        // The vector canister is the single target for the whole graph (ADR 0031 Slice 4), so it
        // owns shards by `graph_id` alone — no property-index group descriptor is sent (sending the
        // property `index_group_size` would split a multi-shard graph into per-shard groups the
        // single target rejects).
        vector_sync::admin_attach_shard_to_vector(
            vector_index_canister,
            graph_id,
            shard_id,
            graph_canister,
        )
        .await
        .map_err(RouterError::Internal)?;
        // Step 3: only now flip the durable readiness bit; the predicate gates dispatch on it.
        Self::commit_set_shard_vector_attached(graph_id, shard_id, vector_index_canister, true)
    }

    async fn complete_shard_vector_attach(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
        vector_index_canister: Principal,
        graph_canister: Principal,
    ) -> Result<(), RouterError> {
        #[cfg(feature = "pocket-ic-e2e")]
        {
            let _ = graph_canister;
            Self::commit_set_shard_vector_attached(graph_id, shard_id, vector_index_canister, true)
        }

        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            self.finish_shard_vector_attach(
                graph_id,
                shard_id,
                vector_index_canister,
                graph_canister,
            )
            .await
        }
    }

    /// Wires (or retrofits) a derived vector-index target onto an already-registered, index-attached
    /// shard and drives the attach handshake (ADR 0031 Slice 4). Idempotent: a shard already
    /// attached to the same target is a no-op. Enforces **one vector-index target per graph** —
    /// every attached shard of the graph must point at the same vector canister.
    pub async fn admin_attach_vector_index_shard(
        &self,
        caller: Principal,
        args: AdminAttachVectorIndexShardArgs,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        if args.vector_index_canister == Principal::anonymous() {
            return Err(RouterError::InvalidArgument(
                "vector_index_canister must not be the anonymous principal".into(),
            ));
        }
        validate_metadata_name(&args.logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(&args.logical_graph_name)?;
        let entry =
            lookup_shard_entry(graph_id, args.shard_id).ok_or(RouterError::ShardNotRegistered)?;
        // A shard must be index-attached (i.e. fully registered) before it can host a derived index.
        if !entry.index_attached {
            return Err(RouterError::Conflict(
                "shard is not index-attached; complete shard registration before vector attach"
                    .into(),
            ));
        }
        // One vector-index target per graph: any other shard already attached to a *different*
        // vector canister is a misconfiguration that would split dispatch across targets.
        if let Some(conflict) = list_shards_for_graph_id(graph_id)?
            .into_iter()
            .find(|other| {
                other.shard_id != args.shard_id
                    && other.vector_index_canister.is_some()
                    && other.vector_index_canister != Some(args.vector_index_canister)
            })
        {
            return Err(RouterError::Conflict(format!(
                "graph already targets vector canister {:?}; one vector-index target per graph",
                conflict.vector_index_canister,
            )));
        }
        if entry.vector_index_attached
            && entry.vector_index_canister == Some(args.vector_index_canister)
        {
            return Ok(());
        }
        self.complete_shard_vector_attach(
            graph_id,
            args.shard_id,
            args.vector_index_canister,
            entry.graph_canister,
        )
        .await
    }

    /// Predicate gating production vector dispatch/backfill for a graph (ADR 0031 Slice 4). True
    /// only when the global activation flag is on, the graph has a single resolved vector-index
    /// target, **and** every live (index-attached) shard of the graph is attached to *that exact
    /// target* (`vector_index_canister == target && vector_index_attached`). Requiring the target
    /// match (not merely a non-anonymous canister) closes the silent-misrouting hole where a shard
    /// attached to one canister could mark a graph ready for a def pointing at another. Fail-closed:
    /// no def target, an empty shard set, or any mismatched/unattached shard yields `false`.
    pub fn graph_vector_dispatch_ready(&self, graph_id: GraphId) -> bool {
        if !super::super::stable::vector_activation::vector_dispatch_globally_enabled() {
            return false;
        }
        let Some(target) =
            super::super::stable::vector_index_catalog::graph_single_target(graph_id)
        else {
            return false;
        };
        let Ok(shards) = list_live_shards_for_graph_id(graph_id) else {
            return false;
        };
        !shards.is_empty()
            && shards.iter().all(|shard| {
                shard.vector_index_attached && shard.vector_index_canister == Some(target)
            })
    }

    #[cfg(test)]
    fn verify_registry_invariants_after_commit() -> Result<(), RouterError> {
        check_registry_invariants().map_err(RouterError::Internal)
    }

    #[cfg(not(test))]
    fn verify_registry_invariants_after_commit() -> Result<(), RouterError> {
        Ok(())
    }

    /// Read-only check of all registry denormalization invariants across regions
    /// 1–5 and 15–16. Per-commit verification is disabled in production for cost,
    /// so this is the on-demand oracle (admin endpoint) used to confirm registry
    /// consistency at any point, including across a canister upgrade.
    pub(crate) fn check_registry_invariants(&self) -> Result<(), String> {
        super::registry_invariants::check_registry_invariants()
    }

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
        if !caller_may_access_graph(&entry, graph_id, caller) {
            // Existence non-disclosure: a non-tenant sees the same error as a missing graph.
            return Err(RouterError::NotFound(graph_name.to_owned()));
        }
        if !matches!(entry.status, GraphStatus::Active | GraphStatus::ReadOnly) {
            return Err(RouterError::GraphUnavailable);
        }
        Ok(entry)
    }

    pub fn resolve_graph_id(&self, graph_name: &str) -> Result<GraphId, RouterError> {
        lookup_graph_id(graph_name).ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))
    }

    /// Resolve `graph_name` to its `GraphId` only when `caller` may access the graph.
    ///
    /// Enforces the per-graph tenancy ACL (see [`caller_may_access_graph`]). A caller who is
    /// not a tenant gets `NotFound` rather than `Forbidden`, so a non-tenant cannot even confirm
    /// the graph exists (cross-tenant existence non-disclosure).
    pub fn resolve_graph_id_authorized(
        &self,
        graph_name: &str,
        caller: Principal,
    ) -> Result<GraphId, RouterError> {
        let graph_id = lookup_graph_id(graph_name)
            .ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))?;
        let entry = ROUTER_GRAPHS
            .with_borrow(|graphs| graphs.get(&graph_id))
            .ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))?;
        if caller_may_access_graph(&entry, graph_id, caller) {
            Ok(graph_id)
        } else {
            Err(RouterError::NotFound(graph_name.to_owned()))
        }
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

    pub fn resolve_shard(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
    ) -> Result<ShardRegistryEntry, RouterError> {
        lookup_shard_entry(graph_id, shard_id).ok_or(RouterError::ShardNotRegistered)
    }

    /// Returns all shard registrations for a logical graph (for federated query fan-out).
    pub fn list_shards_for_graph(
        &self,
        logical_graph_name: &str,
    ) -> Result<Vec<ShardRegistryEntry>, RouterError> {
        validate_metadata_name(logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        list_shards_for_graph_id(graph_id)
    }

    pub fn list_shards_for_graph_id(
        &self,
        graph_id: GraphId,
    ) -> Result<Vec<ShardRegistryEntry>, RouterError> {
        list_shards_for_graph_id(graph_id)
    }

    /// Index-attached shards only (excludes pending registration).
    pub fn list_live_shards_for_graph_id(
        &self,
        graph_id: GraphId,
    ) -> Result<Vec<ShardRegistryEntry>, RouterError> {
        list_live_shards_for_graph_id(graph_id)
    }

    pub fn list_live_shards_for_graph(
        &self,
        logical_graph_name: &str,
    ) -> Result<Vec<ShardRegistryEntry>, RouterError> {
        validate_metadata_name(logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        list_live_shards_for_graph_id(graph_id)
    }

    /// Host/test registration with deterministic element-id key derivation.
    ///
    /// Production canister ingress uses [`Self::admin_register_graph_with_random_key`] (IC
    /// `raw_rand()` entropy). This sync helper shares the host entropy fixture so unit tests do
    /// not depend on [`ElementIdEncodingKey::standalone`].
    pub fn admin_register_graph(
        &self,
        caller: Principal,
        entry: GraphRegistryEntry,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        validate_registration_principals(&entry)?;
        ensure_graph_registration_slot_available(&entry.graph_name, entry.is_home)?;
        let graph_id = intern_graph_name(&entry.graph_name)?;
        let key = derive_element_id_encoding_key(graph_id, HOST_GRAPH_REGISTRATION_ENTROPY);
        Self::commit_register_graph(
            entry,
            super::super::stable::memory::GraphRuntimeConfig::with_element_id_encoding_key(key),
        )?;
        Ok(())
    }

    pub async fn admin_register_graph_with_random_key(
        &self,
        caller: Principal,
        entry: GraphRegistryEntry,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        validate_registration_principals(&entry)?;
        ensure_graph_registration_slot_available(&entry.graph_name, entry.is_home)?;

        let random_bytes = graph_registration_random_entropy().await?;
        ensure_graph_registration_slot_available(&entry.graph_name, entry.is_home)?;

        let graph_id = intern_graph_name(&entry.graph_name)?;
        let key = derive_element_id_encoding_key(graph_id, &random_bytes);
        Self::commit_register_graph(
            entry,
            super::super::stable::memory::GraphRuntimeConfig::with_element_id_encoding_key(key),
        )?;
        Ok(())
    }

    pub fn graph_element_id_encoding_key(
        &self,
        graph_id: GraphId,
    ) -> Result<ElementIdEncodingKey, RouterError> {
        let config = ROUTER_GRAPH_RUNTIME_CONFIG
            .with_borrow(|cfg| cfg.get(&graph_id))
            .ok_or_else(|| RouterError::NotFound(format!("runtime config for graph {graph_id}")))?;
        Ok(ElementIdEncodingKey(config.element_id_encoding_key))
    }

    pub fn graph_index_lookup_targets(
        &self,
        graph_id: GraphId,
    ) -> Result<Vec<Principal>, RouterError> {
        let mut targets: Vec<Principal> = self
            .list_live_shards_for_graph_id(graph_id)?
            .into_iter()
            .map(|entry| entry.index_canister)
            .collect();
        targets.retain(|principal| *principal != Principal::anonymous());
        targets.sort();
        targets.dedup();
        Ok(targets)
    }

    pub fn graph_index_canister_for_shard(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
    ) -> Result<Principal, RouterError> {
        let runtime = ROUTER_GRAPH_RUNTIME_CONFIG
            .with_borrow(|cfg| cfg.get(&graph_id))
            .ok_or_else(|| RouterError::NotFound(format!("runtime config for graph {graph_id}")))?;
        let group_index = shard_group_index(shard_id, runtime.index_group_size)?;
        let index_canister = crate::index_route::index_canister_for_graph_shard(
            shard_id,
            runtime.index_group_size,
            &runtime.index_cluster,
        )
        .ok_or_else(|| {
            RouterError::InvalidArgument(format!(
                "missing/invalid index cluster entry for graph {graph_id} group {group_index}"
            ))
        })?;
        Ok(index_canister)
    }

    pub fn admin_unregister_graph(
        &self,
        caller: Principal,
        logical_graph_name: &str,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        if !list_shards_for_graph_id(graph_id)?.is_empty() {
            return Err(RouterError::Conflict(format!(
                "graph `{logical_graph_name}` still has registered shards"
            )));
        }
        ROUTER_GRAPHS.with_borrow_mut(|g| {
            g.remove(&graph_id);
        });
        ROUTER_GRAPH_RUNTIME_CONFIG.with_borrow_mut(|cfg| {
            cfg.remove(&graph_id);
        });
        ROUTER_GRAPH_CATALOG.with_borrow_mut(|catalog| {
            let _ = catalog.remove_by_name(logical_graph_name);
        });
        Self::purge_graph_vocabulary_partitions(graph_id);
        Self::verify_registry_invariants_after_commit()
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
        Self::verify_registry_invariants_after_commit()?;
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
        let graph_id = resolve_registered_graph_id(&args.logical_graph_name)?;

        if let Some(key) = ROUTER_SHARD_BY_GRAPH.with_borrow(|m| m.get(&args.graph_canister)) {
            let existing = ROUTER_SHARDS
                .with_borrow(|s| s.get(&key))
                .ok_or(RouterError::ShardNotRegistered)?;
            if existing.index_canister != args.index_canister {
                return Err(RouterError::ShardAlreadyRegistered);
            }
            if existing.graph_id != graph_id {
                return Err(RouterError::Conflict(format!(
                    "graph canister already registered to graph {:?}, not `{logical_graph}`",
                    existing.graph_id,
                    logical_graph = args.logical_graph_name,
                )));
            }
            if existing.shard_id != args.shard_id {
                return Err(RouterError::Conflict(format!(
                    "graph canister already registered as shard {:?}, not {:?}",
                    existing.shard_id, args.shard_id,
                )));
            }
            if !existing.index_attached {
                return self
                    .complete_shard_index_attach(
                        graph_id,
                        existing.shard_id,
                        args.index_canister,
                        args.graph_canister,
                    )
                    .await;
            }
            return Ok(());
        }

        // ADR 0030 slice 10: a graph must not scale from one to multiple shards while any
        // `Active`/`Dropping` `ShardLocalGlobal` constraint exists. Such a constraint enforces its
        // graph-wide uniqueness entirely inside its single owning shard's local table; a new shard
        // would not see those values, so adding one could silently admit duplicates. This branch is
        // a brand-new shard (the graph canister is not already registered), so a non-empty shard set
        // here means the registration would make the graph multi-shard.
        if !list_shards_for_graph_id(graph_id)?.is_empty()
            && constraint_catalog::has_shard_local_global_constraint(graph_id)
        {
            return Err(RouterError::Conflict(
                "cannot register a second shard while shard-local global unique constraints exist; \
                 drop or migrate those constraints first"
                    .into(),
            ));
        }

        let allocated_shard_id = next_graph_local_shard_id(graph_id);
        if args.shard_id != allocated_shard_id {
            return Err(RouterError::Conflict(format!(
                "expected next graph-local shard {:?} for `{}`, got {:?}",
                allocated_shard_id, args.logical_graph_name, args.shard_id,
            )));
        }
        validate_index_group_canister_assignment(
            graph_id,
            allocated_shard_id,
            args.index_canister,
        )?;

        let registered_at_ns = ic_time_ns();
        let entry = ShardRegistryEntry {
            shard_id: allocated_shard_id,
            graph_canister: args.graph_canister,
            index_canister: args.index_canister,
            graph_id,
            registered_at_ns,
            index_attached: false,
            // Vector wiring is a separate, optional handshake (ADR 0031 Slice 4); a freshly
            // registered shard starts with no vector target and unattached.
            vector_index_canister: None,
            vector_index_attached: false,
            typed_seed_batch_v1: false,
        };

        commit_index_group_canister_assignment(graph_id, allocated_shard_id, args.index_canister)?;
        if let Err(err) = Self::commit_register_shard(entry) {
            let _ = reconcile_index_cluster_after_shard_removal(graph_id);
            return Err(err);
        }

        self.complete_shard_index_attach(
            graph_id,
            allocated_shard_id,
            args.index_canister,
            args.graph_canister,
        )
        .await?;

        #[cfg(target_family = "wasm")]
        crate::peer_sync::sync_peers_after_shard_register(
            &args.logical_graph_name,
            args.graph_canister,
        )
        .await
        .map_err(RouterError::Internal)?;

        Ok(())
    }
}

/// Snapshot of a shard registry entry captured before the async Graph capability call,
/// used so unit tests can manipulate registry state between capture and commit.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShardCapabilityRefreshCapture {
    pub key: GraphShardKey,
    pub graph_canister: Principal,
    pub prior_registered_at_ns: u64,
}

impl RouterStore {
    /// Admin-only: refresh the execution-capability bit for one registered Graph shard.
    ///
    /// Queries the currently registered Graph principal, then re-reads the registry after the
    /// await and commits the advertised bit only if the same `GraphShardKey` still maps to the same
    /// principal. Preserves all other registry facts on success and leaves the record unchanged on
    /// any error or target replacement.
    pub async fn admin_refresh_shard_execution_capabilities(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        shard_id: ShardId,
    ) -> Result<bool, RouterError> {
        use crate::graph_client;

        let capture =
            self.capture_shard_for_capability_refresh(caller, logical_graph_name, shard_id)?;
        let capabilities = graph_client::execution_capabilities(capture.graph_canister)
            .await
            .map_err(|error| {
                RouterError::Internal(format!("capability refresh failed: {error}"))
            })?;
        self.commit_shard_execution_capabilities(capture, capabilities.typed_seed_batch_v1)
    }

    /// Capture stage: validate admin/graph/shard and snapshot the current entry identity.
    pub(crate) fn capture_shard_for_capability_refresh(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        shard_id: ShardId,
    ) -> Result<ShardCapabilityRefreshCapture, RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        let key = GraphShardKey::new(graph_id, shard_id);
        let (graph_canister, prior) = ROUTER_SHARDS.with_borrow(|shards| {
            let entry = shards.get(&key).ok_or(RouterError::ShardNotRegistered)?;
            Ok::<_, RouterError>((entry.graph_canister, entry.clone()))
        })?;
        Ok(ShardCapabilityRefreshCapture {
            key,
            graph_canister,
            prior_registered_at_ns: prior.registered_at_ns,
        })
    }

    /// Commit stage: apply the advertised capability bit only if the captured shard identity is
    /// still current. This is intentionally a separate function so tests can drive ABA/race cases.
    pub(crate) fn commit_shard_execution_capabilities(
        &self,
        capture: ShardCapabilityRefreshCapture,
        typed_seed_batch_v1: bool,
    ) -> Result<bool, RouterError> {
        ROUTER_SHARDS.with_borrow_mut(|shards| {
            let mut current = shards
                .get(&capture.key)
                .ok_or(RouterError::ShardNotRegistered)?;
            if current.graph_canister != capture.graph_canister {
                return Err(RouterError::Conflict(
                    "graph canister changed during capability refresh; aborting update".into(),
                ));
            }
            if current.registered_at_ns != capture.prior_registered_at_ns {
                return Err(RouterError::Conflict(
                    "shard registry entry was replaced during capability refresh; aborting update"
                        .into(),
                ));
            }
            current.typed_seed_batch_v1 = typed_seed_batch_v1;
            shards.insert(capture.key, current);
            Ok::<_, RouterError>(typed_seed_batch_v1)
        })
    }

    /// Admin-only: synchronously clear the typed-seed-batch-v1 capability bit for one shard.
    /// Affects only new-group admission; existing durable `TypedSeedBulk` records continue to
    /// replay on the typed path.
    pub fn admin_clear_shard_execution_capabilities(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        shard_id: ShardId,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        let key = GraphShardKey::new(graph_id, shard_id);
        ROUTER_SHARDS.with_borrow_mut(|shards| {
            let mut entry = shards.get(&key).ok_or(RouterError::ShardNotRegistered)?;
            entry.typed_seed_batch_v1 = false;
            shards.insert(key, entry);
            Ok::<(), RouterError>(())
        })?;
        Self::verify_registry_invariants_after_commit()
    }

    pub async fn admin_unregister_shard(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        shard_id: ShardId,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        let entry =
            lookup_shard_entry(graph_id, shard_id).ok_or(RouterError::ShardNotRegistered)?;
        let graph_name = graph_catalog::graph_name(entry.graph_id).unwrap_or_default();
        let departing_graph = entry.graph_canister;
        let _siblings: Vec<Principal> = self
            .list_shards_for_graph(&graph_name)?
            .into_iter()
            .map(|shard| shard.graph_canister)
            .filter(|graph| *graph != departing_graph)
            .collect();

        Self::commit_set_shard_index_attached(graph_id, shard_id, false)?;

        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            index_sync::admin_detach_shard_canister(entry.index_canister, shard_id)
                .await
                .map_err(RouterError::Internal)?;
        }

        Self::commit_unregister_shard(graph_id, shard_id)?;
        reconcile_index_cluster_after_shard_removal(graph_id)?;

        #[cfg(target_family = "wasm")]
        crate::peer_sync::sync_peers_after_shard_unregister(departing_graph, &_siblings)
            .await
            .map_err(RouterError::Internal)?;

        Ok(())
    }
}

fn derive_element_id_encoding_key(
    graph_id: GraphId,
    random_entropy: &[u8],
) -> ElementIdEncodingKey {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(ELEMENT_ID_KEY_DERIVATION_DOMAIN);
    hasher.update(graph_id.raw().to_le_bytes());
    hasher.update(random_entropy);
    let digest = hasher.finalize();
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[..16]);
    ElementIdEncodingKey(key)
}

#[cfg(target_family = "wasm")]
async fn graph_registration_random_entropy() -> Result<Vec<u8>, RouterError> {
    ic_cdk_management_canister::raw_rand()
        .await
        .map_err(|err| RouterError::Internal(format!("raw_rand failed: {err:?}")))
}

#[cfg(not(target_family = "wasm"))]
async fn graph_registration_random_entropy() -> Result<Vec<u8>, RouterError> {
    Ok(HOST_GRAPH_REGISTRATION_ENTROPY.to_vec())
}
