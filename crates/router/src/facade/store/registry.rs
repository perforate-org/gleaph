//! Graph and shard registry.

use super::super::stable::graph_catalog::{
    self, intern_graph_name, list_live_shards_for_graph_id, list_shards_for_graph_id,
    lookup_graph_id, lookup_shard_entry, next_graph_local_shard_id, require_graph_registry_entry,
    resolve_registered_graph_id,
};
use super::super::stable::{
    ROUTER_EDGE_INLINE_PROPERTY_PROFILES, ROUTER_EDGE_LABEL_CATALOG,
    ROUTER_EDGE_LABEL_LIVE_BY_SHARD, ROUTER_EDGE_LABEL_STATS, ROUTER_GQL_GRAPH_CATALOG,
    ROUTER_GRAPH_CATALOG, ROUTER_GRAPH_RUNTIME_CONFIG, ROUTER_GRAPHS, ROUTER_INDEX_NAME_CATALOG,
    ROUTER_PROPERTY_CATALOG, ROUTER_SHARD_BY_GRAPH, ROUTER_SHARDS, ROUTER_SHARDS_BY_GRAPH_ID,
    ROUTER_VERTEX_LABEL_CATALOG, ROUTER_VERTEX_LABEL_LIVE_BY_SHARD, ROUTER_VERTEX_LABEL_STATS,
};
#[cfg(test)]
use super::registry_invariants::check_registry_invariants;
use crate::facade::auth;
use crate::facade::stable::constraint_catalog;
use crate::facade::stable::memory::RouterShardState;
use crate::facade::stable::{vector_index_catalog, vector_ingest_outbox};
#[cfg(not(feature = "pocket-ic-e2e"))]
use crate::index_sync;
use crate::state::RouterError;
use crate::types::{
    AdminAttachIndexShardArgs, AdminAttachVectorIndexShardArgs, AdminRegisterShardArgs,
    GraphRegistryEntry, GraphStatus, ProvisioningState, ShardId,
};
use crate::vector_sync;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GraphShardKey, ShardRegistryEntry};

use super::{RouterStore, ic_time_ns, validate_metadata_name};

/// Registry-entry tenant check shared by the metadata ACL and the ownership-root arm of
/// data-plane evaluation (ADR 0074 §3 invariant 3: `registry.owner` is the identity anchor).
fn entry_names_tenant(entry: &GraphRegistryEntry, caller: Principal) -> bool {
    caller == entry.owner || entry.admins.contains(&caller)
}

/// Per-graph tenancy predicate shared by name resolution and [`RouterStore::resolve_graph`].
///
/// A caller may access a graph's metadata/routing data when it is the graph `owner`, listed in
/// `admins`, holds at least one unexpired data-plane grant on the graph (`caller ∪ PUBLIC`,
/// ADR 0074 slice 2b), holds an unexpired metadata elevation covering this graph
/// (`ReadMetadata` on that graph or cross-graph `ControlPlane`, [ADR 0080] §2), or is the
/// graph's own registered shard canister. The last case keeps federation/index-routing
/// inter-canister calls working: shards call the router with their `graph_canister`
/// principal, which is keyed in [`ROUTER_SHARD_BY_GRAPH`].
///
/// Visibility is admission only: none of these arms authorize data-plane access, which is
/// enforced at plan time against grant rows (`crate::authz`); by the plane-disjointness
/// contract ([ADR 0080] §1) a metadata elevation admits here but never covers data-plane
/// demands.
///
/// The former global-admin superuser arm is deleted ([ADR 0080] §2): administrative
/// capabilities confer no metadata access, so caps holders without an elevation receive
/// identical treatment to strangers (ADR 0028 NotFound non-disclosure). Time-boxed,
/// approval-backed grants are the only elevation.
fn caller_may_access_graph(
    entry: &GraphRegistryEntry,
    graph_id: GraphId,
    caller: Principal,
) -> bool {
    if entry_names_tenant(entry, caller) {
        return true;
    }
    if auth::holds_any_graph_grant(graph_id.raw(), &caller) {
        return true;
    }
    if auth::holds_metadata_graph_access(graph_id.raw(), &caller) {
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

/// Exact durable ownership proof for one in-flight Vector attach handshake.
///
/// Same-target retries share the current claim. A new claim after unregister receives a new epoch,
/// so a delayed pre-unregister finalizer cannot publish readiness for the newer operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VectorAttachClaim {
    pub(super) vector_canister: Principal,
    pub(super) epoch: u64,
}

/// Exact durable ownership proof for one in-flight shard unregister operation.
///
/// The claim is derived from the canonical shard row before the first detach await. Any
/// re-registration or later unregister advances the epoch, so an older continuation cannot detach
/// or remove the newer row even when the Vector target principal is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ShardUnregisterClaim {
    pub(super) vector_target: Option<Principal>,
    pub(super) epoch: u64,
}

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

/// Whether any registered graph is marked home (ADR 0070). Global, not caller-scoped: there is at
/// most one home graph per Router, so the first created graph claims it.
pub(crate) fn any_home_graph_exists() -> bool {
    ROUTER_GRAPHS.with_borrow(|graphs| graphs.iter().any(|lazy| lazy.value().is_home))
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
                existing.is_home.then(|| {
                    graph_catalog::graph_name(existing.graph_id)
                        .unwrap_or_else(|| format!("{:?}", existing.graph_id))
                })
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
    /// Removes every catalog partition and derived row of `graph_id` after the graph leaves
    /// the registry. Runs in one no-await message segment, so a failure anywhere rolls the
    /// whole teardown back.
    fn purge_graph_vocabulary_partitions(graph_id: GraphId) -> Result<(), RouterError> {
        ROUTER_VERTEX_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.remove_graph(graph_id));
        ROUTER_EDGE_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.remove_graph(graph_id));
        ROUTER_PROPERTY_CATALOG.with_borrow_mut(|catalog| catalog.remove_graph(graph_id));
        // ADR 0074 §3 invariant 4: with the label/property partitions gone, every stored
        // graph-scoped grant row targeting this graph references ids that are monotonic and
        // never reused — permanently uncoverable dead rows. Sweep them in the same commit
        // segment so storage and introspection stay truthful.
        auth::sweep_graph_grants(graph_id.raw());
        ROUTER_INDEX_NAME_CATALOG.with_borrow_mut(|catalog| catalog.remove_graph(graph_id));
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES.with_borrow_mut(|store| store.remove_graph(graph_id));
        super::super::stable::indexed_catalog::purge_graph_indexes(graph_id);
        super::super::stable::constraint_catalog::purge_graph_constraints(graph_id);
        super::super::stable::reservation_catalog::purge_graph_reservations(graph_id);
        super::super::stable::unique_effect_pending::purge_graph(graph_id);
        super::super::stable::vector_index_catalog::purge_graph_vector_indexes(graph_id)?;
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
        Ok(())
    }

    /// Atomically interns the graph name and inserts the registry entry.
    pub(super) fn commit_register_graph(
        mut entry: GraphRegistryEntry,
        graph_name: &str,
        runtime_config: super::super::stable::memory::GraphRuntimeConfig,
    ) -> Result<GraphId, RouterError> {
        let graph_id = intern_graph_name(graph_name)?;
        if ROUTER_GRAPHS.with_borrow(|graphs| graphs.get(&graph_id).is_some()) {
            return Err(RouterError::Conflict(graph_name.to_owned()));
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

        let mut runtime = ROUTER_GRAPH_RUNTIME_CONFIG
            .with_borrow(|configs| configs.get(&graph_id))
            .ok_or_else(|| {
                RouterError::NotFound(format!("runtime config for graph {graph_id:?}"))
            })?;
        let expected_raw = u32::try_from(runtime.next_shard_id)
            .map_err(|_| RouterError::IdExhausted(format!("shard for graph {graph_id:?}")))?;
        let expected = ShardId::new(expected_raw);
        if shard_id != expected {
            return Err(RouterError::Conflict(format!(
                "expected next graph-local shard {expected:?}, got {shard_id:?}"
            )));
        }
        let next_shard_id = runtime
            .next_shard_id
            .checked_add(1)
            .ok_or_else(|| RouterError::IdExhausted(format!("shard for graph {graph_id:?}")))?;

        if ROUTER_SHARDS.with_borrow(|s| s.get(&key).is_some()) {
            return Err(RouterError::ShardAlreadyRegistered);
        }
        if ROUTER_SHARD_BY_GRAPH
            .with_borrow(|m| m.get(&graph_canister))
            .is_some()
        {
            return Err(RouterError::ShardAlreadyRegistered);
        }

        runtime.next_shard_id = next_shard_id;
        ROUTER_SHARDS.with_borrow_mut(|s| {
            s.insert(key, entry.into());
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
        ROUTER_GRAPH_RUNTIME_CONFIG.with_borrow_mut(|configs| {
            configs.insert(graph_id, runtime);
        });

        Self::verify_registry_invariants_after_commit()
    }

    /// Atomically removes shard registry, canister map, and per-graph shard index after the caller
    /// has completed every owner-local preflight in the same no-await message segment.
    fn commit_remove_shard_registry_row(
        graph_id: GraphId,
        shard_id: ShardId,
        state: RouterShardState,
    ) -> Result<ShardRegistryEntry, RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        ROUTER_SHARDS.with_borrow_mut(|s| {
            s.remove(&key);
        });
        ROUTER_SHARD_BY_GRAPH.with_borrow_mut(|m| {
            m.remove(&state.graph_canister);
        });
        ROUTER_SHARDS_BY_GRAPH_ID.with_borrow_mut(|index| {
            let Some(mut list) = index.get(&state.graph_id) else {
                return;
            };
            list.shard_ids.retain(|id| *id != shard_id);
            if list.shard_ids.is_empty() {
                index.remove(&state.graph_id);
            } else {
                index.insert(state.graph_id, list);
            }
        });
        // Backfill cursors are derived state owned by the retiring shard.
        super::backfill::purge_backfill_state(key);

        Self::verify_registry_invariants_after_commit()?;
        Ok(state.entry)
    }

    /// Atomically revalidates the exact unregister owner and removes only that lifecycle's row.
    pub(super) fn commit_finish_shard_unregister(
        graph_id: GraphId,
        shard_id: ShardId,
        claim: ShardUnregisterClaim,
    ) -> Result<ShardRegistryEntry, RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        let state = ROUTER_SHARDS
            .with_borrow(|shards| shards.get(&key))
            .ok_or(RouterError::ShardNotRegistered)?;
        Self::ensure_current_shard_unregister_claim(&state, shard_id, claim)?;
        if let Some(vector_canister) = state.vector_canister
            && vector_ingest_outbox::has_pending_for_target_shard(vector_canister, shard_id)
        {
            return Err(RouterError::Conflict(format!(
                "cannot unregister shard {shard_id:?} while direct vector-ingest work targets {vector_canister}"
            )));
        }
        Self::commit_remove_shard_registry_row(graph_id, shard_id, state)
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

    /// Completes index re-registration for an existing shard row. The first completion advances
    /// the durable epoch and clears the target, fencing both pre-unregister attach finalizers and
    /// delayed unregister continuations. A concurrent duplicate that observes an already attached
    /// row is an exact no-op and cannot erase a newer Vector claim.
    fn commit_complete_shard_index_reregistration(
        graph_id: GraphId,
        shard_id: ShardId,
    ) -> Result<(), RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        ROUTER_SHARDS.with_borrow_mut(|shards| {
            let mut state = shards.get(&key).ok_or(RouterError::ShardNotRegistered)?;
            if state.index_attached {
                return Ok(());
            }
            let next_epoch = state
                .vector_attach_epoch
                .checked_add(1)
                .ok_or_else(|| RouterError::IdExhausted("vector attach epoch".into()))?;
            state.index_attached = true;
            state.vector_canister = None;
            state.vector_index_attached = false;
            state.vector_attach_epoch = next_epoch;
            shards.insert(key, state);
            Ok(())
        })?;
        Self::verify_registry_invariants_after_commit()
    }

    /// Starts canonical shard unregister before the first remote detach await by revoking both
    /// readiness bits and advancing the Vector attach fence in one row write. The exact Vector
    /// target remains durable so a lost detach response can retry the same owner.
    pub(super) fn commit_begin_shard_unregister(
        graph_id: GraphId,
        shard_id: ShardId,
    ) -> Result<ShardUnregisterClaim, RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        let claim = ROUTER_SHARDS.with_borrow_mut(|shards| {
            let mut state = shards.get(&key).ok_or(RouterError::ShardNotRegistered)?;
            let next_epoch = state
                .vector_attach_epoch
                .checked_add(1)
                .ok_or_else(|| RouterError::IdExhausted("vector attach epoch".into()))?;
            state.index_attached = false;
            state.vector_index_attached = false;
            state.vector_attach_epoch = next_epoch;
            let claim = ShardUnregisterClaim {
                vector_target: state.vector_canister,
                epoch: next_epoch,
            };
            shards.insert(key, state);
            Ok(claim)
        })?;
        Self::verify_registry_invariants_after_commit()?;
        Ok(claim)
    }

    fn ensure_current_shard_unregister_claim(
        state: &RouterShardState,
        shard_id: ShardId,
        claim: ShardUnregisterClaim,
    ) -> Result<(), RouterError> {
        if state.vector_attach_epoch != claim.epoch
            || state.vector_canister != claim.vector_target
            || state.index_attached
            || state.vector_index_attached
        {
            return Err(RouterError::Conflict(format!(
                "stale shard unregister claim for shard {shard_id:?}: target {:?} epoch {} no longer owns the unregistering row",
                claim.vector_target, claim.epoch
            )));
        }
        Ok(())
    }

    fn ensure_shard_unregister_claim(
        graph_id: GraphId,
        shard_id: ShardId,
        claim: ShardUnregisterClaim,
    ) -> Result<(), RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        let state = ROUTER_SHARDS
            .with_borrow(|shards| shards.get(&key))
            .ok_or(RouterError::ShardNotRegistered)?;
        Self::ensure_current_shard_unregister_claim(&state, shard_id, claim)
    }

    /// Retrofit an existing (indexless) shard's index target to a newly provisioned index canister.
    /// A failed remote attach does NOT unregister the shard —
    /// the shard stays indexless and the error propagates for the caller to retry.
    fn commit_set_shard_index_canister(
        graph_id: GraphId,
        shard_id: ShardId,
        index_canister: Principal,
    ) -> Result<(), RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        ROUTER_SHARDS.with_borrow_mut(|shards| {
            let mut entry = shards.get(&key).ok_or(RouterError::ShardNotRegistered)?;
            entry.index_canister = index_canister;
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

    /// Re-establishes the property-index attachment for a retained unregister row. The remote
    /// attach must succeed before the existing row is made index-ready and its pre-unregister
    /// Vector claim is invalidated in one no-await commit.
    async fn complete_shard_index_reregistration(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
        index_canister: Principal,
        graph_canister: Principal,
    ) -> Result<(), RouterError> {
        #[cfg(feature = "pocket-ic-e2e")]
        {
            let _ = (index_canister, graph_canister);
            Self::commit_complete_shard_index_reregistration(graph_id, shard_id)
        }

        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            Self::attach_shard_to_index(graph_id, shard_id, index_canister, graph_canister).await?;
            Self::commit_complete_shard_index_reregistration(graph_id, shard_id)
        }
    }

    /// Retrofit an existing (indexless) shard onto a newly provisioned index canister (ADR 0035
    /// Slice 10). The remote attach runs first so a failure leaves the shard indexless (no partial
    /// state); only a successful attach flips the shard's index target. A failed remote attach
    /// does NOT unregister the shard.
    async fn retrofit_attach_shard_to_index(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
        index_canister: Principal,
        graph_canister: Principal,
    ) -> Result<(), RouterError> {
        #[cfg(feature = "pocket-ic-e2e")]
        {
            let _ = (index_canister, graph_canister);
            Self::commit_set_shard_index_canister(graph_id, shard_id, index_canister)
        }
        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            Self::attach_shard_to_index(graph_id, shard_id, index_canister, graph_canister).await?;
            Self::commit_set_shard_index_canister(graph_id, shard_id, index_canister)
        }
    }

    /// Claims this shard's derived Vector target before the first await. A new target claim advances
    /// the durable epoch, while an in-flight same-target duplicate reuses the exact claim.
    pub(super) fn commit_claim_shard_vector_target(
        graph_id: GraphId,
        shard_id: ShardId,
        vector_canister: Principal,
    ) -> Result<VectorAttachClaim, RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        let claim = ROUTER_SHARDS.with_borrow_mut(|shards| {
            let mut state = shards.get(&key).ok_or(RouterError::ShardNotRegistered)?;
            match state.vector_canister {
                None => {
                    let epoch = state
                        .vector_attach_epoch
                        .checked_add(1)
                        .ok_or_else(|| RouterError::IdExhausted("vector attach epoch".into()))?;
                    state.vector_canister = Some(vector_canister);
                    state.vector_index_attached = false;
                    state.vector_attach_epoch = epoch;
                    shards.insert(key, state);
                    Ok(VectorAttachClaim {
                        vector_canister,
                        epoch,
                    })
                }
                Some(current) if current == vector_canister => Ok(VectorAttachClaim {
                    vector_canister,
                    epoch: state.vector_attach_epoch,
                }),
                Some(current) => Err(RouterError::Conflict(format!(
                    "shard {shard_id:?} already claims vector target {current}, not {vector_canister}"
                ))),
            }
        })?;
        Self::verify_registry_invariants_after_commit()?;
        Ok(claim)
    }

    fn ensure_current_vector_attach_claim(
        state: &RouterShardState,
        shard_id: ShardId,
        claim: VectorAttachClaim,
    ) -> Result<(), RouterError> {
        if state.vector_attach_epoch != claim.epoch {
            return Err(RouterError::Conflict(format!(
                "stale vector attach claim for shard {shard_id:?}: epoch {} is not current epoch {}",
                claim.epoch, state.vector_attach_epoch
            )));
        }
        if state.vector_canister != Some(claim.vector_canister) {
            return Err(RouterError::Conflict(format!(
                "shard {shard_id:?} vector target is {:?}, not {}",
                state.vector_canister, claim.vector_canister
            )));
        }
        Ok(())
    }

    /// Preflight the global exact-lane identity before publishing the durable vector-readiness
    /// bit. `shard_id` is graph-local, while direct-ingestion outbox keys are `(target, shard)`;
    /// two fully attached catalog rows with the same pair would therefore have ambiguous durable
    /// ownership. This scan is the attach write boundary and catches duplicates spanning any
    /// bounded discovery page.
    fn ensure_unique_attached_vector_lane(
        key: GraphShardKey,
        vector_canister: Principal,
    ) -> Result<(), RouterError> {
        ROUTER_SHARDS.with_borrow(|shards| {
            if shards.iter().any(|lazy| {
                let other_key = *lazy.key();
                if other_key == key {
                    return false;
                }
                let other = lazy.value();
                other.index_attached
                    && other.vector_index_attached
                    && other.vector_canister == Some(vector_canister)
                    && other.shard_id == key.shard_id
            }) {
                return Err(RouterError::Conflict(format!(
                    "vector target {vector_canister} already owns an attached shard {:?}",
                    key.shard_id
                )));
            }
            Ok(())
        })
    }

    pub(super) fn commit_set_shard_vector_attached(
        graph_id: GraphId,
        shard_id: ShardId,
        claim: VectorAttachClaim,
    ) -> Result<(), RouterError> {
        let key = GraphShardKey::new(graph_id, shard_id);
        ROUTER_SHARDS.with_borrow(|shards| {
            let state = shards.get(&key).ok_or(RouterError::ShardNotRegistered)?;
            Self::ensure_current_vector_attach_claim(&state, shard_id, claim)?;
            if !state.index_attached {
                return Err(RouterError::Conflict(
                    "shard is not index-attached; vector readiness cannot be committed".into(),
                ));
            }
            Ok(())
        })?;
        Self::ensure_unique_attached_vector_lane(key, claim.vector_canister)?;
        ROUTER_SHARDS.with_borrow_mut(|shards| {
            let mut state = shards.get(&key).ok_or(RouterError::ShardNotRegistered)?;
            Self::ensure_current_vector_attach_claim(&state, shard_id, claim)?;
            if !state.index_attached {
                return Err(RouterError::Conflict(
                    "shard is not index-attached; vector readiness cannot be committed".into(),
                ));
            }
            state.vector_index_attached = true;
            shards.insert(key, state);
            Ok::<(), RouterError>(())
        })?;
        Self::verify_registry_invariants_after_commit()
    }

    #[cfg(not(feature = "pocket-ic-e2e"))]
    async fn finish_shard_vector_attach(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
        claim: VectorAttachClaim,
        graph_canister: Principal,
    ) -> Result<(), RouterError> {
        // The durable shard claim is made by `admin_attach_vector_index_shard` before entering this
        // async handshake. This call only updates the graph-local route and then performs the
        // remote attach; a failed await therefore leaves an exact, retryable target claim.
        // Step 1: make the shard's *local* routing carry the target before anything observes it as
        // ready.
        vector_sync::admin_set_graph_vector_canister(graph_canister, claim.vector_canister)
            .await
            .map_err(RouterError::Internal)?;
        // Step 2: attach the shard to the vector canister so it accepts the shard's subject sync.
        // The vector canister is the single target for the whole graph (ADR 0031 Slice 4), so it
        // owns shards by `graph_id` alone — no property-index group descriptor is sent (sending the
        // property `index_group_size` would split a multi-shard graph into per-shard groups the
        // single target rejects).
        vector_sync::admin_attach_shard_to_vector(
            claim.vector_canister,
            graph_id,
            shard_id,
            graph_canister,
        )
        .await
        .map_err(RouterError::Internal)?;
        // Step 3: only the exact target-and-epoch claim may flip the durable readiness bit; the
        // predicate gates dispatch on it.
        Self::commit_set_shard_vector_attached(graph_id, shard_id, claim)
    }

    async fn complete_shard_vector_attach(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
        claim: VectorAttachClaim,
        graph_canister: Principal,
    ) -> Result<(), RouterError> {
        #[cfg(feature = "pocket-ic-e2e")]
        {
            let _ = graph_canister;
            Self::commit_set_shard_vector_attached(graph_id, shard_id, claim)
        }

        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            self.finish_shard_vector_attach(graph_id, shard_id, claim, graph_canister)
                .await
        }
    }

    /// Wires a graph-index canister onto an already-registered shard (ADR 0054 indexless
    /// bootstrap). Registration never carries an index target; dev mode and operations attach
    /// one explicitly here, while provisioned mode gets the same wiring through
    /// `ensure_index_canisters_provisioned` (ADR 0035 issuance + retrofit attach). Idempotent:
    /// attaching the canister the shard already points at is a no-op; re-pointing an attached
    /// shard is a conflict. The index-group assignment is committed before the remote handshake
    /// so dispatch fan-out sees a consistent cluster view.
    pub async fn admin_attach_index_shard(
        &self,
        caller: Principal,
        args: AdminAttachIndexShardArgs,
    ) -> Result<(), RouterError> {
        auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_FEDERATION)?;
        if args.index_canister == Principal::anonymous() {
            return Err(RouterError::InvalidArgument(
                "index_canister must not be the anonymous principal".into(),
            ));
        }
        validate_metadata_name(&args.logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(&args.logical_graph_name)?;
        let entry =
            lookup_shard_entry(graph_id, args.shard_id).ok_or(RouterError::ShardNotRegistered)?;
        if entry.index_canister == args.index_canister {
            // Idempotent attach: the shard already runs this exact target.
            return Ok(());
        }
        if entry.index_canister != Principal::anonymous() {
            return Err(RouterError::Conflict(format!(
                "shard {shard:?} already claims index canister {current}; re-pointing is not supported",
                shard = args.shard_id,
                current = entry.index_canister,
            )));
        }
        validate_index_group_canister_assignment(graph_id, args.shard_id, args.index_canister)?;
        commit_index_group_canister_assignment(graph_id, args.shard_id, args.index_canister)?;
        let result = self
            .retrofit_attach_shard_to_index(
                graph_id,
                args.shard_id,
                args.index_canister,
                entry.graph_canister,
            )
            .await;
        if result.is_ok() {
            crate::recovery::arm_if_needed();
        }
        result
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
        auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_FEDERATION)?;
        if args.vector_canister == Principal::anonymous() {
            return Err(RouterError::InvalidArgument(
                "vector_canister must not be the anonymous principal".into(),
            ));
        }
        validate_metadata_name(&args.logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(&args.logical_graph_name)?;
        let definition_target =
            vector_index_catalog::graph_single_target(graph_id).ok_or_else(|| {
                RouterError::Conflict(
                    "vector index definition target must be assigned before shard attach".into(),
                )
            })?;
        if definition_target != args.vector_canister {
            return Err(RouterError::Conflict(format!(
                "vector shard attach target {requested} does not match immutable definition target {definition_target}",
                requested = args.vector_canister
            )));
        }
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
                    && other.vector_canister.is_some()
                    && other.vector_canister != Some(args.vector_canister)
            })
        {
            return Err(RouterError::Conflict(format!(
                "graph already targets vector canister {:?}; one vector-index target per graph",
                conflict.vector_canister,
            )));
        }
        let key = GraphShardKey::new(graph_id, args.shard_id);
        if entry.vector_index_attached && entry.vector_canister == Some(args.vector_canister) {
            Self::ensure_unique_attached_vector_lane(key, args.vector_canister)?;
            // An idempotent attach is still a valid wake-up boundary: a prior timer may have
            // stopped while this durable lane remained attached.
            crate::recovery::arm_if_needed();
            return Ok(());
        }
        // Scan the complete stable catalog before claiming the candidate row. The final attach
        // commit repeats this preflight after the remote handshake because another shard can
        // complete concurrently while this request awaits; this first check keeps a known lane
        // conflict atomic for the public attach operation.
        Self::ensure_unique_attached_vector_lane(key, args.vector_canister)?;
        // Claim the exact definition target in the existing durable shard row before the first
        // await. A second concurrent request therefore observes the claim synchronously and can
        // only replay the same target; a different target is rejected without any remote call.
        let claim =
            Self::commit_claim_shard_vector_target(graph_id, args.shard_id, args.vector_canister)?;
        let result = self
            .complete_shard_vector_attach(graph_id, args.shard_id, claim, entry.graph_canister)
            .await;
        if result.is_ok() {
            // The catalog row is fully vector-attached only after the remote handshake and the
            // durable readiness bit commit. Wake the existing recovery timer at that boundary;
            // no separate marker or timer is needed for markerless Graph-owned lanes.
            crate::recovery::arm_if_needed();
        }
        result
    }

    /// Predicate gating production vector dispatch/backfill for a graph (ADR 0031 Slice 4). True
    /// only when the global activation flag is on, the graph has a single resolved vector-index
    /// target, **and** every live (index-attached) shard of the graph is attached to *that exact
    /// target* (`vector_canister == target && vector_index_attached`). Requiring the target
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
            && shards
                .iter()
                .all(|shard| shard.vector_index_attached && shard.vector_canister == Some(target))
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

    /// Data-plane gate: a graph whose provisioning is not `None` must not be resolvable for
    /// dispatch. Enforced once here and shared by every `resolve_graph_id*` resolution path
    /// (ADR 0056 §6 Slice A). No new `GraphStatus` variant; `GraphUnavailable` is the same
    /// error the status gate uses.
    fn assert_provisioning_complete(graph_id: GraphId) -> Result<(), RouterError> {
        let entry = ROUTER_GRAPHS
            .with_borrow(|graphs| graphs.get(&graph_id))
            .ok_or_else(|| RouterError::NotFound("graph".to_owned()))?;
        if !matches!(entry.provisioning_state, ProvisioningState::None) {
            return Err(RouterError::GraphUnavailable);
        }
        Ok(())
    }

    /// Operator-facing graph entry read (ADR 0056 §4 `get_graph`): ACL-checked but NOT gated on
    /// status or provisioning state, so operators can observe `Pending` / `Failed` / `Deprecated`
    /// graphs during provisioning and teardown.
    pub fn get_graph_operator(
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
            return Err(RouterError::NotFound(graph_name.to_owned()));
        }
        Ok(entry)
    }

    /// Admin-only: overwrite a graph's `ProvisioningState` (ADR 0056 §6 Slice A write path;
    /// Slice B drives it from the ack handler). Read-modify-write on the registry entry.
    pub fn admin_set_graph_provisioning_state(
        &self,
        caller: Principal,
        graph_name: &str,
        state: ProvisioningState,
    ) -> Result<(), RouterError> {
        auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_FEDERATION)?;
        let graph_id = resolve_registered_graph_id(graph_name)?;
        let mut entry = ROUTER_GRAPHS
            .with_borrow(|graphs| graphs.get(&graph_id))
            .ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))?;
        entry.provisioning_state = state;
        ROUTER_GRAPHS.with_borrow_mut(|graphs| {
            graphs.insert(graph_id, entry);
        });
        Ok(())
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
        Self::assert_provisioning_complete(graph_id)?;
        Ok(entry)
    }

    pub fn resolve_graph_id(&self, graph_name: &str) -> Result<GraphId, RouterError> {
        let graph_id = lookup_graph_id(graph_name)
            .ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))?;
        Self::assert_provisioning_complete(graph_id)?;
        Ok(graph_id)
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
            Self::assert_provisioning_complete(graph_id)?;
            Ok(graph_id)
        } else {
            Err(RouterError::NotFound(graph_name.to_owned()))
        }
    }

    /// Whether `caller` is a tenant of `graph_id` per the registry entry (`owner`/`admins`).
    ///
    /// The ownership-root arm of data-plane privilege evaluation (ADR 0074 §3 invariant 3):
    /// the graph owner executes without stored grant rows because ownership itself is the
    /// implicit root of data-plane authority — it is evaluated at enforcement time from the
    /// registry and never materialized as rows.
    pub fn is_graph_tenant(&self, graph_id: GraphId, caller: Principal) -> bool {
        ROUTER_GRAPHS
            .with_borrow(|graphs| graphs.get(&graph_id))
            .is_some_and(|entry| entry_names_tenant(&entry, caller))
    }

    /// Read-only visibility probe for diagnostic surfaces (ADR 0084 §3): whether
    /// `caller` may access `graph_id` under the settled admission arms (tenancy ∪
    /// grant-derived ∪ metadata-elevation ∪ own-shard). Delegates to the same ACL
    /// predicate name resolution uses, so the diagnostic cannot widen visibility.
    pub fn caller_may_access_graph(&self, graph_id: GraphId, caller: Principal) -> bool {
        ROUTER_GRAPHS
            .with_borrow(|graphs| graphs.get(&graph_id))
            .is_some_and(|entry| caller_may_access_graph(&entry, graph_id, caller))
    }

    pub fn list_visible_graph_ids(&self, caller: Principal) -> Result<Vec<GraphId>, RouterError> {
        let mut out = Vec::new();
        ROUTER_GRAPHS.with_borrow(|graphs| {
            for lazy in graphs.iter() {
                let entry = lazy.value();
                if caller != entry.owner
                    && !entry.admins.contains(&caller)
                    && !auth::holds_any_graph_grant(entry.graph_id.raw(), &caller)
                {
                    continue;
                }
                if matches!(entry.status, GraphStatus::Active | GraphStatus::ReadOnly) {
                    out.push(entry.graph_id);
                }
            }
        });
        Ok(out)
    }

    /// Registry-local `GraphSummary` rows for every graph visible to `caller` (ADR 0056 §7).
    /// No cross-canister calls, so UI/CLI polling stays cheap.
    pub(crate) fn list_graph_summaries(
        &self,
        caller: Principal,
    ) -> Result<Vec<crate::types::GraphSummary>, RouterError> {
        use crate::types::GraphSummary;

        let graph_ids = self.list_visible_graph_ids(caller)?;
        let mut out = Vec::with_capacity(graph_ids.len());
        for graph_id in graph_ids {
            let entry = graph_catalog::graph_entry(graph_id).ok_or_else(|| {
                RouterError::InvalidState(format!("graph {graph_id:?} missing registry entry"))
            })?;
            let shard_count = graph_catalog::list_shards_for_graph_id(graph_id)?.len() as u32;
            let graph_name = graph_catalog::graph_name(graph_id).ok_or_else(|| {
                RouterError::InvalidState(format!("graph {graph_id:?} missing name"))
            })?;
            out.push(GraphSummary {
                graph_id,
                graph_name,
                status: entry.status,
                provisioning_state: entry.provisioning_state,
                shard_count,
                updated_at_ns: entry.updated_at_ns,
            });
        }
        Ok(out)
    }

    /// Resolve HOME graph for `caller` (ADR 0011 §1.3).
    ///
    /// Prefer exactly one visible graph with `is_home`; otherwise fall back to the sole
    /// visible graph (degenerate case A). Visibility follows
    /// [`Self::list_visible_graph_ids`]: tenancy plus grant-derived visibility.
    pub fn resolve_home_graph_id(&self, caller: Principal) -> Result<GraphId, RouterError> {
        let mut home_marked = Vec::new();
        let mut visible = Vec::new();
        ROUTER_GRAPHS.with_borrow(|graphs| {
            for lazy in graphs.iter() {
                let entry = lazy.value();
                if caller != entry.owner
                    && !entry.admins.contains(&caller)
                    && !auth::holds_any_graph_grant(entry.graph_id.raw(), &caller)
                {
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
        graph_name: &str,
    ) -> Result<(), RouterError> {
        auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_FEDERATION)?;
        validate_registration_principals(&entry)?;
        ensure_graph_registration_slot_available(graph_name, entry.is_home)?;
        let graph_id = intern_graph_name(graph_name)?;
        let key = derive_element_id_encoding_key(graph_id, HOST_GRAPH_REGISTRATION_ENTROPY);
        Self::commit_register_graph(
            entry,
            graph_name,
            super::super::stable::memory::GraphRuntimeConfig::with_element_id_encoding_key(key),
        )?;
        Ok(())
    }

    pub async fn admin_register_graph_with_random_key(
        &self,
        caller: Principal,
        entry: GraphRegistryEntry,
        graph_name: &str,
    ) -> Result<(), RouterError> {
        auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_FEDERATION)?;
        validate_registration_principals(&entry)?;
        ensure_graph_registration_slot_available(graph_name, entry.is_home)?;

        let random_bytes = graph_registration_random_entropy().await?;
        ensure_graph_registration_slot_available(graph_name, entry.is_home)?;

        let graph_id = intern_graph_name(graph_name)?;
        let key = derive_element_id_encoding_key(graph_id, &random_bytes);
        Self::commit_register_graph(
            entry,
            graph_name,
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
        self.index_target_for_shard(graph_id, shard_id)
            .ok_or_else(|| {
                RouterError::InvalidArgument(format!(
                    "missing/invalid index cluster entry for graph {graph_id} shard {shard_id}"
                ))
            })
    }

    /// Resolves the shard's index target, or `None` when the shard is indexless (ADR 0054
    /// anonymous sentinel) or has no runtime config.
    pub fn index_target_for_shard(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
    ) -> Option<Principal> {
        let runtime = ROUTER_GRAPH_RUNTIME_CONFIG.with_borrow(|cfg| cfg.get(&graph_id))?;
        let _group_index = shard_group_index(shard_id, runtime.index_group_size).ok()?;
        crate::index_route::index_canister_for_graph_shard(
            shard_id,
            runtime.index_group_size,
            &runtime.index_cluster,
        )
    }

    /// The graph's full `index_cluster` (group ordinal → canister principal).
    pub fn graph_index_cluster(&self, graph_id: GraphId) -> Result<Vec<Principal>, RouterError> {
        ROUTER_GRAPH_RUNTIME_CONFIG
            .with_borrow(|cfg| cfg.get(&graph_id).map(|c| c.index_cluster.clone()))
            .ok_or_else(|| RouterError::NotFound(format!("runtime config for graph {graph_id}")))
    }

    /// Shard groups of `graph_id` that still have an indexless shard (anonymous index target).
    /// In provisioned mode, `CREATE INDEX` provisions these before registering the definition.
    /// Keyed off the shards' own `index_canister` (not `index_cluster`) so a group whose canister
    /// was assigned but whose shards were not yet attached is still reported as unassigned and
    /// re-provisioned idempotently on retry.
    pub fn unassigned_index_groups(&self, graph_id: GraphId) -> Result<Vec<u32>, RouterError> {
        let shards = list_live_shards_for_graph_id(graph_id)?;
        if shards.is_empty() {
            return Ok(Vec::new());
        }
        let runtime = ROUTER_GRAPH_RUNTIME_CONFIG
            .with_borrow(|cfg| cfg.get(&graph_id))
            .ok_or_else(|| RouterError::NotFound(format!("runtime config for graph {graph_id}")))?;
        let mut groups = std::collections::BTreeSet::new();
        for shard in &shards {
            if shard.index_canister == Principal::anonymous() {
                let group = shard_group_index(shard.shard_id, runtime.index_group_size)?;
                groups.insert(group as u32);
            }
        }
        Ok(groups.into_iter().collect())
    }

    /// Assign newly provisioned index canisters to `index_cluster` and retrofit-attach every live
    /// shard in the affected groups (ADR 0035 Slice 10). Idempotent per group: re-assigning the
    /// same canister and re-attaching an already-attached shard are no-ops.
    pub async fn attach_provisioned_index_canisters(
        &self,
        graph_id: GraphId,
        group_canisters: &std::collections::BTreeMap<u32, Principal>,
    ) -> Result<(), RouterError> {
        // 1. Assign each group's canister to index_cluster (idempotent).
        for (group, canister) in group_canisters {
            let runtime = ROUTER_GRAPH_RUNTIME_CONFIG
                .with_borrow(|cfg| cfg.get(&graph_id))
                .ok_or_else(|| {
                    RouterError::NotFound(format!("runtime config for graph {graph_id}"))
                })?;
            let rep_shard = ShardId::new(
                group
                    .checked_mul(runtime.index_group_size)
                    .ok_or_else(|| RouterError::InvalidArgument("index group overflow".into()))?,
            );
            commit_index_group_canister_assignment(graph_id, rep_shard, *canister)?;
        }
        // 2. Retrofit-attach each live shard in an affected group to its canister.
        let shards = list_live_shards_for_graph_id(graph_id)?;
        for shard in &shards {
            let runtime = ROUTER_GRAPH_RUNTIME_CONFIG
                .with_borrow(|cfg| cfg.get(&graph_id))
                .ok_or_else(|| {
                    RouterError::NotFound(format!("runtime config for graph {graph_id}"))
                })?;
            let group = shard_group_index(shard.shard_id, runtime.index_group_size)? as u32;
            if let Some(canister) = group_canisters.get(&group) {
                if shard.index_canister == *canister {
                    continue; // already attached to this canister
                }
                self.retrofit_attach_shard_to_index(
                    graph_id,
                    shard.shard_id,
                    *canister,
                    shard.graph_canister,
                )
                .await?;
            }
        }
        Ok(())
    }

    pub fn admin_unregister_graph(
        &self,
        caller: Principal,
        logical_graph_name: &str,
    ) -> Result<(), RouterError> {
        auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_FEDERATION)?;
        validate_metadata_name(logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        if !list_shards_for_graph_id(graph_id)?.is_empty() {
            return Err(RouterError::Conflict(format!(
                "graph `{logical_graph_name}` still has registered shards"
            )));
        }
        if vector_ingest_outbox::has_pending() {
            return Err(RouterError::Conflict(
                "cannot purge graph catalogs while direct vector-ingest work remains pending"
                    .into(),
            ));
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
        Self::purge_graph_vocabulary_partitions(graph_id)?;
        Self::verify_registry_invariants_after_commit()
    }

    pub fn admin_update_graph_status(
        &self,
        caller: Principal,
        graph_name: &str,
        status: GraphStatus,
        version: u64,
    ) -> Result<(), RouterError> {
        auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_FEDERATION)?;
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
        auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_FEDERATION)?;
        if args.graph_canister == Principal::anonymous() {
            return Err(RouterError::InvalidArgument(
                "graph principal must be non-anonymous".into(),
            ));
        }
        validate_metadata_name(&args.logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(&args.logical_graph_name)?;

        if let Some(key) = ROUTER_SHARD_BY_GRAPH.with_borrow(|m| m.get(&args.graph_canister)) {
            let existing = ROUTER_SHARDS
                .with_borrow(|s| s.get(&key))
                .ok_or(RouterError::ShardNotRegistered)?;
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
                    .complete_shard_index_reregistration(
                        graph_id,
                        existing.shard_id,
                        existing.index_canister,
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

        let allocated_shard_id = next_graph_local_shard_id(graph_id)?;
        if args.shard_id != allocated_shard_id {
            return Err(RouterError::Conflict(format!(
                "expected next graph-local shard {:?} for `{}`, got {:?}",
                allocated_shard_id, args.logical_graph_name, args.shard_id,
            )));
        }
        let registered_at_ns = ic_time_ns();
        let entry = ShardRegistryEntry {
            shard_id: allocated_shard_id,
            graph_canister: args.graph_canister,
            // ADR 0054: shards bootstrap indexless. Registration records the anonymous
            // sentinel; the attach flow (`admin_attach_index_shard` in dev mode,
            // `ensure_index_canisters_provisioned` in provisioned mode) re-points it at a
            // real canister. The stable entry layout (V2) is unchanged.
            index_canister: Principal::anonymous(),
            graph_id,
            registered_at_ns,
            index_attached: false,
            // Vector wiring is a separate, optional handshake (ADR 0031 Slice 4); a freshly
            // registered shard starts with no vector target and unattached.
            vector_canister: None,
            vector_index_attached: false,
        };
        if let Err(err) = Self::commit_register_shard(entry) {
            let _ = reconcile_index_cluster_after_shard_removal(graph_id);
            return Err(err);
        }

        // Indexless shard (ADR 0054): no index canister to attach at registration, so mark the
        // shard ready immediately. Dispatch fan-out and index reads treat a non-anonymous index
        // target as absent, which matches an indexless first shard.
        Self::commit_set_shard_index_attached(graph_id, allocated_shard_id, true)?;

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

impl RouterStore {
    pub async fn admin_unregister_shard(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        shard_id: ShardId,
    ) -> Result<(), RouterError> {
        auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_FEDERATION)?;
        validate_metadata_name(logical_graph_name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        let entry =
            lookup_shard_entry(graph_id, shard_id).ok_or(RouterError::ShardNotRegistered)?;
        if let Some(vector_canister) = entry.vector_canister
            && vector_ingest_outbox::has_pending_for_target_shard(vector_canister, shard_id)
        {
            return Err(RouterError::Conflict(format!(
                "cannot unregister shard {shard_id:?} while direct vector-ingest work targets {vector_canister}"
            )));
        }
        let graph_name = graph_catalog::graph_name(entry.graph_id).unwrap_or_default();
        let departing_graph = entry.graph_canister;
        let _siblings: Vec<Principal> = self
            .list_shards_for_graph(&graph_name)?
            .into_iter()
            .map(|shard| shard.graph_canister)
            .filter(|graph| *graph != departing_graph)
            .collect();

        let claim = Self::commit_begin_shard_unregister(graph_id, shard_id)?;
        // Keep the exact Vector target in the row until the remote detach reaches EOF. Both
        // readiness bits are already false, while a failed bounded purge must retain enough
        // identity for an exact retry.

        // No newer registration may own the row when Graph detach is issued. The Graph Index owner
        // generation-fences the remote session until EOF; this Router claim fences the continuation
        // after each response boundary.
        Self::ensure_shard_unregister_claim(graph_id, shard_id, claim)?;
        #[cfg(not(feature = "pocket-ic-e2e"))]
        {
            index_sync::admin_detach_shard_canister(entry.index_canister, shard_id)
                .await
                .map_err(RouterError::Internal)?;
        }

        // A re-registration can complete after the Graph detach response and before this callback
        // resumes. Revalidate before issuing Vector detach so the old operation cannot target a
        // newer same-principal attachment.
        Self::ensure_shard_unregister_claim(graph_id, shard_id, claim)?;
        if let Some(vector_canister) = claim.vector_target {
            vector_sync::admin_detach_shard_from_vector(vector_canister, shard_id)
                .await
                .map_err(RouterError::Internal)?;
        }

        Self::commit_finish_shard_unregister(graph_id, shard_id, claim)?;
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
