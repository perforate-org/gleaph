//! Stateless facade over router stable storage.

use super::stable::label_telemetry::{
    AppliedLabelTelemetryKey, ClientMutationKey, LabelShardKey, LabelStats, RouterMutationRecord,
    RouterMutationShard,
};
use super::stable::{
    ROUTER_APPLIED_LABEL_TELEMETRY, ROUTER_CONTROLLERS, ROUTER_EDGE_LABEL_BY_ID,
    ROUTER_EDGE_LABEL_BY_NAME, ROUTER_EDGE_LABEL_LIVE_BY_SHARD, ROUTER_EDGE_LABEL_STATS,
    ROUTER_GRAPHS, ROUTER_LOGICAL_COUNTER, ROUTER_MIGRATION_COUNTER, ROUTER_MUTATION_BY_CLIENT_KEY,
    ROUTER_MUTATION_COUNTER, ROUTER_PENDING_LOGICAL, ROUTER_PLACEMENT_BY_PHYSICAL,
    ROUTER_PLACEMENTS, ROUTER_PROPERTY_BY_ID, ROUTER_PROPERTY_BY_NAME, ROUTER_SHARD_BY_GRAPH,
    ROUTER_SHARDS, ROUTER_VERTEX_LABEL_BY_ID, ROUTER_VERTEX_LABEL_BY_NAME,
    ROUTER_VERTEX_LABEL_LIVE_BY_SHARD, ROUTER_VERTEX_LABEL_STATS,
};
use crate::index_sync;
use crate::init::RouterInitArgs;
use crate::state::RouterError;
use crate::types::{
    AdminRegisterShardArgs, BeginVertexMigrationArgs, CommitVertexPlacementArgs, EdgeLabelId,
    FinishVertexMigrationArgs, GraphRegistryEntry, GraphStatus, PropertyId,
    ReleaseLogicalVertexArgs, ShardId, VertexLabelId, VertexPlacement,
};
use candid::Principal;
use gleaph_gql_planner::{LabelUseIntent, PhysicalPlan};
use gleaph_graph_kernel::entry::EDGE_LABEL_CATALOG_MAX;
use gleaph_graph_kernel::federation::{
    LocalVertexId, LogicalVertexId, PhysicalPlacementKey, PhysicalVertexLocation,
    ShardRegistryEntry,
};
use gleaph_graph_kernel::plan_exec::{
    LabelTelemetryEventWire, LabelUsageDelta, MutationId, ResolvedEdgeLabel, ResolvedLabelTable,
    ResolvedVertexLabel,
};

const MAX_METADATA_NAME_BYTES: usize = 256;
const MAX_CLIENT_MUTATION_KEY_BYTES: usize = 256;
const CLIENT_MUTATION_KEY_TTL_NS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;

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
        ROUTER_CONTROLLERS.with_borrow_mut(|admins| {
            admins.clear();
            for p in &args.controllers {
                admins.insert(*p);
            }
        });
        ROUTER_GRAPHS.with_borrow_mut(|g| g.clear_new());
        ROUTER_SHARDS.with_borrow_mut(|s| s.clear_new());
        ROUTER_SHARD_BY_GRAPH.with_borrow_mut(|m| m.clear_new());
        ROUTER_PLACEMENTS.with_borrow_mut(|p| p.clear_new());
        ROUTER_PLACEMENT_BY_PHYSICAL.with_borrow_mut(|p| p.clear_new());
        ROUTER_MIGRATION_COUNTER.with_borrow_mut(|c| {
            c.set(0);
        });
        ROUTER_LOGICAL_COUNTER.with_borrow_mut(|c| {
            c.set(0);
        });
        ROUTER_PENDING_LOGICAL.with_borrow_mut(|p| p.clear_new());
        ROUTER_VERTEX_LABEL_BY_NAME.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_BY_ID.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_BY_NAME.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_BY_ID.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_STATS.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_STATS.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_LIVE_BY_SHARD.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_LIVE_BY_SHARD.with_borrow_mut(|m| m.clear_new());
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|c| {
            c.set(0);
        });
        ROUTER_APPLIED_LABEL_TELEMETRY.with_borrow_mut(|m| m.clear());
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| m.clear_new());
        ROUTER_PROPERTY_BY_NAME.with_borrow_mut(|m| m.clear_new());
        ROUTER_PROPERTY_BY_ID.with_borrow_mut(|m| m.clear_new());
    }

    pub fn bootstrap_controllers(&self, principals: &[Principal]) {
        ROUTER_CONTROLLERS.with_borrow_mut(|admins| {
            for p in principals {
                admins.insert(*p);
            }
        });
    }

    fn is_controller(&self, caller: Principal) -> bool {
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

    pub fn resolve_placement(
        &self,
        logical_vertex_id: LogicalVertexId,
    ) -> Result<VertexPlacement, RouterError> {
        ROUTER_PLACEMENTS
            .with_borrow(|p| p.get(&logical_vertex_id))
            .ok_or(RouterError::VertexNotFound)
    }

    pub fn resolve_logical_at(
        &self,
        shard_id: ShardId,
        local_vertex_id: LocalVertexId,
    ) -> Result<LogicalVertexId, RouterError> {
        ROUTER_PLACEMENT_BY_PHYSICAL
            .with_borrow(|p| p.get(PhysicalPlacementKey::new(shard_id, local_vertex_id)))
            .ok_or(RouterError::VertexNotFound)
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

    pub fn admin_intern_vertex_label(
        &self,
        caller: Principal,
        name: &str,
    ) -> Result<VertexLabelId, RouterError> {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        validate_metadata_name(name)?;
        intern_vertex_label_name(name)
    }

    pub fn admin_intern_edge_label(
        &self,
        caller: Principal,
        name: &str,
    ) -> Result<EdgeLabelId, RouterError> {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        validate_metadata_name(name)?;
        intern_edge_label_name(name)
    }

    pub fn admin_intern_property(
        &self,
        caller: Principal,
        name: &str,
    ) -> Result<PropertyId, RouterError> {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        validate_metadata_name(name)?;
        if let Some(id) = ROUTER_PROPERTY_BY_NAME.with_borrow(|m| m.get(&name.to_string())) {
            return Ok(PropertyId::from_raw(id));
        }
        let next_id = ROUTER_PROPERTY_BY_ID.with_borrow(|m| m.keys().max().unwrap_or(0)) + 1;
        ROUTER_PROPERTY_BY_NAME.with_borrow_mut(|m| {
            m.insert(name.to_string(), next_id);
        });
        ROUTER_PROPERTY_BY_ID.with_borrow_mut(|m| {
            m.insert(next_id, name.to_string());
        });
        Ok(PropertyId::from_raw(next_id))
    }

    pub fn lookup_vertex_label_id(&self, name: &str) -> Result<VertexLabelId, RouterError> {
        ROUTER_VERTEX_LABEL_BY_NAME
            .with_borrow(|m| m.get(&name.to_string()))
            .map(VertexLabelId::from_raw)
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn lookup_edge_label_id(&self, name: &str) -> Result<EdgeLabelId, RouterError> {
        ROUTER_EDGE_LABEL_BY_NAME
            .with_borrow(|m| m.get(&name.to_string()))
            .map(EdgeLabelId::from_raw)
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn lookup_property_id(&self, name: &str) -> Result<PropertyId, RouterError> {
        ROUTER_PROPERTY_BY_NAME
            .with_borrow(|m| m.get(&name.to_string()))
            .map(PropertyId::from_raw)
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn reverse_vertex_label_name(
        &self,
        label_id: VertexLabelId,
    ) -> Result<String, RouterError> {
        ROUTER_VERTEX_LABEL_BY_ID
            .with_borrow(|m| m.get(&label_id.raw()))
            .ok_or_else(|| RouterError::NotFound(format!("vertex label id {}", label_id.raw())))
    }

    pub fn reverse_edge_label_name(&self, label_id: EdgeLabelId) -> Result<String, RouterError> {
        ROUTER_EDGE_LABEL_BY_ID
            .with_borrow(|m| m.get(&label_id.raw()))
            .ok_or_else(|| RouterError::NotFound(format!("edge label id {}", label_id.raw())))
    }

    pub fn reverse_property_name(&self, property_id: PropertyId) -> Result<String, RouterError> {
        ROUTER_PROPERTY_BY_ID
            .with_borrow(|m| m.get(&property_id.raw()))
            .ok_or_else(|| RouterError::NotFound(format!("property id {}", property_id.raw())))
    }

    pub fn resolve_plan_labels(
        &self,
        plans: &[PhysicalPlan],
    ) -> Result<ResolvedLabelTable, RouterError> {
        let mut out = ResolvedLabelTable::default();
        for plan in plans {
            let uses = plan.label_uses();
            for (name, intent) in uses.node_labels {
                validate_metadata_name(&name)?;
                let id = match intent {
                    LabelUseIntent::ReadExisting => self.lookup_vertex_label_id(&name)?,
                    LabelUseIntent::CreateIfMissing => intern_vertex_label_name(&name)?,
                };
                if !out.vertex.iter().any(|entry| entry.name == name.as_ref()) {
                    out.vertex.push(ResolvedVertexLabel {
                        name: name.to_string(),
                        id,
                    });
                }
            }
            for (name, intent) in uses.edge_labels {
                validate_metadata_name(&name)?;
                let id = match intent {
                    LabelUseIntent::ReadExisting => self.lookup_edge_label_id(&name)?,
                    LabelUseIntent::CreateIfMissing => intern_edge_label_name(&name)?,
                };
                if !out.edge.iter().any(|entry| entry.name == name.as_ref()) {
                    out.edge.push(ResolvedEdgeLabel {
                        name: name.to_string(),
                        id,
                    });
                }
            }
        }
        Ok(out)
    }

    pub fn vertex_label_stats(&self, label_id: VertexLabelId) -> LabelStats {
        ROUTER_VERTEX_LABEL_STATS
            .with_borrow(|m| m.get(&label_id.raw()))
            .unwrap_or_default()
    }

    pub fn edge_label_stats(&self, label_id: EdgeLabelId) -> LabelStats {
        ROUTER_EDGE_LABEL_STATS
            .with_borrow(|m| m.get(&label_id.raw()))
            .unwrap_or_default()
    }

    pub fn vertex_label_shard_live_count(&self, shard_id: ShardId, label_id: VertexLabelId) -> u64 {
        ROUTER_VERTEX_LABEL_LIVE_BY_SHARD
            .with_borrow(|m| m.get(&LabelShardKey::new(shard_id, label_id.raw())))
            .unwrap_or(0)
    }

    pub fn edge_label_shard_live_count(&self, shard_id: ShardId, label_id: EdgeLabelId) -> u64 {
        ROUTER_EDGE_LABEL_LIVE_BY_SHARD
            .with_borrow(|m| m.get(&LabelShardKey::new(shard_id, label_id.raw())))
            .unwrap_or(0)
    }

    pub fn apply_label_usage_delta(&self, shard_id: ShardId, delta: &LabelUsageDelta) {
        for (label_id, value) in &delta.vertex {
            apply_label_delta(
                label_id.raw(),
                shard_id,
                *value,
                &ROUTER_VERTEX_LABEL_STATS,
                &ROUTER_VERTEX_LABEL_LIVE_BY_SHARD,
            );
        }
        for (label_id, value) in &delta.edge {
            apply_label_delta(
                label_id.raw(),
                shard_id,
                *value,
                &ROUTER_EDGE_LABEL_STATS,
                &ROUTER_EDGE_LABEL_LIVE_BY_SHARD,
            );
        }
    }

    pub fn allocate_mutation_id(&self) -> Result<MutationId, RouterError> {
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| {
            let next = counter
                .get()
                .checked_add(1)
                .ok_or_else(|| RouterError::IdExhausted("mutation_id".into()))?;
            if next == 0 {
                return Err(RouterError::IdExhausted("mutation_id".into()));
            }
            counter.set(next);
            Ok(next)
        })
    }

    pub fn reserve_mutation_id_for_client_key(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        client_key: &str,
        request_fingerprint: Vec<u8>,
    ) -> Result<ClientMutationReservation, RouterError> {
        self.reserve_mutation_id_for_client_key_at(
            caller,
            logical_graph_name,
            client_key,
            request_fingerprint,
            ic_time_ns(),
        )
    }

    fn reserve_mutation_id_for_client_key_at(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        client_key: &str,
        request_fingerprint: Vec<u8>,
        now: u64,
    ) -> Result<ClientMutationReservation, RouterError> {
        validate_client_mutation_key(client_key)?;
        let key =
            ClientMutationKey::new(caller, logical_graph_name.to_owned(), client_key.to_owned());
        if let Some(mut record) = ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&key)) {
            if now.saturating_sub(record.created_at_ns) > CLIENT_MUTATION_KEY_TTL_NS {
                return Err(RouterError::InvalidArgument(
                    "client_mutation_key expired; use a new key for a new mutation".into(),
                ));
            }
            if record.request_fingerprint != request_fingerprint {
                return Err(RouterError::Conflict(
                    "client_mutation_key was already used for a different request".into(),
                ));
            }
            if record.routing_in_progress {
                return Err(RouterError::Conflict(
                    "client_mutation_key is already in progress; retry later".into(),
                ));
            }
            if record.shards.is_empty() && record.completed_row_count.is_none() {
                record.routing_in_progress = true;
                let mutation_id = record.mutation_id;
                ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
                    m.insert(key, record);
                });
                return Ok(ClientMutationReservation {
                    mutation_id,
                    routing_owner: true,
                });
            }
            return Ok(ClientMutationReservation {
                mutation_id: record.mutation_id,
                routing_owner: false,
            });
        }
        let mutation_id = self.allocate_mutation_id()?;
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            m.insert(
                key,
                RouterMutationRecord::new(mutation_id, now, request_fingerprint),
            );
        });
        Ok(ClientMutationReservation {
            mutation_id,
            routing_owner: true,
        })
    }

    pub fn router_mutation_record(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        client_key: &str,
    ) -> Option<RouterMutationRecord> {
        let key =
            ClientMutationKey::new(caller, logical_graph_name.to_owned(), client_key.to_owned());
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&key))
    }

    pub fn record_router_mutation_shards(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        client_key: &str,
        resolved_labels: ResolvedLabelTable,
        shards: Vec<RouterMutationShard>,
    ) -> Result<(), RouterError> {
        let key =
            ClientMutationKey::new(caller, logical_graph_name.to_owned(), client_key.to_owned());
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            if record.shards.is_empty() && record.completed_row_count.is_none() {
                record.resolved_labels = Some(resolved_labels);
                record.routing_in_progress = false;
                record.shards = shards;
                m.insert(key, record);
            }
            Ok(())
        })
    }

    pub fn record_router_mutation_completed_without_shards(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        client_key: &str,
        resolved_labels: ResolvedLabelTable,
        row_count: u64,
    ) -> Result<(), RouterError> {
        let key =
            ClientMutationKey::new(caller, logical_graph_name.to_owned(), client_key.to_owned());
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            if record.shards.is_empty() && record.completed_row_count.is_none() {
                record.resolved_labels = Some(resolved_labels);
                record.completed_row_count = Some(row_count);
                record.routing_in_progress = false;
                m.insert(key, record);
            }
            Ok(())
        })
    }

    pub fn abandon_router_mutation_routing_reservation(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        client_key: &str,
    ) -> Result<(), RouterError> {
        let key =
            ClientMutationKey::new(caller, logical_graph_name.to_owned(), client_key.to_owned());
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            record.routing_in_progress = false;
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn record_router_mutation_shard_completed(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        client_key: &str,
        shard_id: ShardId,
        row_count: u64,
        events: Vec<LabelTelemetryEventWire>,
    ) -> Result<(), RouterError> {
        let key =
            ClientMutationKey::new(caller, logical_graph_name.to_owned(), client_key.to_owned());
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            let shard = record
                .shards
                .iter_mut()
                .find(|shard| shard.shard_id == shard_id)
                .ok_or_else(|| RouterError::ShardNotRegistered)?;
            shard.completed = true;
            shard.telemetry_acked = false;
            shard.row_count = row_count;
            shard.label_telemetry_events = events;
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn record_router_mutation_shard_telemetry_acked(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        client_key: &str,
        shard_id: ShardId,
    ) -> Result<(), RouterError> {
        let key =
            ClientMutationKey::new(caller, logical_graph_name.to_owned(), client_key.to_owned());
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            let shard = record
                .shards
                .iter_mut()
                .find(|shard| shard.shard_id == shard_id)
                .ok_or_else(|| RouterError::ShardNotRegistered)?;
            shard.telemetry_acked = true;
            shard.label_telemetry_events.clear();
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn router_mutation_completed_row_count(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        client_key: &str,
    ) -> Option<u64> {
        let record = self.router_mutation_record(caller, logical_graph_name, client_key)?;
        if let Some(row_count) = record.completed_row_count {
            return Some(row_count);
        }
        if record.shards.is_empty()
            || record
                .shards
                .iter()
                .any(|shard| !shard.completed || !shard.telemetry_acked)
        {
            return None;
        }
        Some(
            record
                .shards
                .iter()
                .fold(0u64, |total, shard| total.saturating_add(shard.row_count)),
        )
    }

    pub fn apply_label_telemetry_event(
        &self,
        shard_id: ShardId,
        event: &LabelTelemetryEventWire,
    ) -> bool {
        let key = AppliedLabelTelemetryKey::new(shard_id, event.shard_event_seq);
        if ROUTER_APPLIED_LABEL_TELEMETRY.with_borrow(|applied| applied.contains(&key)) {
            return false;
        }
        self.apply_label_usage_delta(shard_id, &event.label_usage_delta);
        ROUTER_APPLIED_LABEL_TELEMETRY.with_borrow_mut(|applied| {
            applied.insert(key);
        });
        true
    }

    pub fn allocate_logical_vertex_id(
        &self,
        caller: Principal,
    ) -> Result<LogicalVertexId, RouterError> {
        let shard_id = self.shard_id_for_graph_caller(caller)?;
        let _ = shard_id;

        let logical_id = ROUTER_LOGICAL_COUNTER.with_borrow_mut(|c| {
            let next = c.get() + 1;
            c.set(next);
            next
        });

        ROUTER_PENDING_LOGICAL.with_borrow_mut(|p| {
            if let Some(prev) = p.insert(caller, logical_id) {
                let _ = prev;
            }
        });

        Ok(logical_id)
    }

    pub fn commit_vertex_placement(
        &self,
        caller: Principal,
        args: CommitVertexPlacementArgs,
    ) -> Result<(), RouterError> {
        let shard_id = self.shard_id_for_graph_caller(caller)?;

        let pending = ROUTER_PENDING_LOGICAL
            .with_borrow(|p| p.get(&caller))
            .ok_or(RouterError::UnallocatedLogicalVertex)?;
        if pending != args.logical_vertex_id {
            return Err(RouterError::UnallocatedLogicalVertex);
        }

        if ROUTER_PLACEMENTS.with_borrow(|p| p.contains_key(&args.logical_vertex_id)) {
            return Err(RouterError::PlacementAlreadyCommitted);
        }

        let placement =
            VertexPlacement::Active(PhysicalVertexLocation::new(shard_id, args.local_vertex_id));
        let physical_key = PhysicalPlacementKey::new(shard_id, args.local_vertex_id);
        ROUTER_PLACEMENTS.with_borrow_mut(|p| {
            p.insert(args.logical_vertex_id, placement);
        });
        ROUTER_PLACEMENT_BY_PHYSICAL.with_borrow_mut(|p| {
            p.insert(physical_key, args.logical_vertex_id);
        });
        ROUTER_PENDING_LOGICAL.with_borrow_mut(|p| {
            p.remove(&caller);
        });
        Ok(())
    }

    pub fn begin_vertex_migration(
        &self,
        caller: Principal,
        args: BeginVertexMigrationArgs,
    ) -> Result<(), RouterError> {
        let source_shard = self.shard_id_for_graph_caller(caller)?;
        if !ROUTER_SHARDS.with_borrow(|s| s.contains_key(&args.destination_shard_id)) {
            return Err(RouterError::ShardNotRegistered);
        }
        if source_shard == args.destination_shard_id {
            return Err(RouterError::InvalidMigrationState(
                "destination shard must differ from source".into(),
            ));
        }

        let placement = ROUTER_PLACEMENTS
            .with_borrow(|p| p.get(&args.logical_vertex_id))
            .ok_or(RouterError::VertexNotFound)?;

        let VertexPlacement::Active(source) = placement else {
            return Err(RouterError::VertexMigrating);
        };
        if source.shard_id != source_shard {
            return Err(RouterError::Forbidden);
        }

        let epoch = ROUTER_MIGRATION_COUNTER.with_borrow_mut(|c| {
            let next = c.get().saturating_add(1);
            c.set(next);
            next
        });

        ROUTER_PLACEMENTS.with_borrow_mut(|p| {
            p.insert(
                args.logical_vertex_id,
                VertexPlacement::Migrating {
                    epoch,
                    source,
                    destination_shard_id: args.destination_shard_id,
                },
            );
        });
        Ok(())
    }

    pub fn finish_vertex_migration(
        &self,
        caller: Principal,
        args: FinishVertexMigrationArgs,
    ) -> Result<(), RouterError> {
        let destination_shard = self.shard_id_for_graph_caller(caller)?;

        let placement = ROUTER_PLACEMENTS
            .with_borrow(|p| p.get(&args.logical_vertex_id))
            .ok_or(RouterError::VertexNotFound)?;

        let VertexPlacement::Migrating {
            source,
            destination_shard_id,
            ..
        } = placement
        else {
            return Err(RouterError::VertexNotMigrating);
        };
        if destination_shard_id != destination_shard {
            return Err(RouterError::Forbidden);
        }

        let destination =
            PhysicalVertexLocation::new(destination_shard, args.destination_local_vertex_id);
        let old_physical = PhysicalPlacementKey::new(source.shard_id, source.local_vertex_id);
        let new_physical =
            PhysicalPlacementKey::new(destination.shard_id, destination.local_vertex_id);

        ROUTER_PLACEMENT_BY_PHYSICAL.with_borrow_mut(|p| {
            p.remove(old_physical);
            p.insert(new_physical, args.logical_vertex_id);
        });
        ROUTER_PLACEMENTS.with_borrow_mut(|p| {
            p.insert(args.logical_vertex_id, VertexPlacement::Active(destination));
        });
        Ok(())
    }

    pub fn release_logical_vertex_placement(
        &self,
        caller: Principal,
        args: ReleaseLogicalVertexArgs,
    ) -> Result<(), RouterError> {
        let shard_id = self.shard_id_for_graph_caller(caller)?;

        let placement = ROUTER_PLACEMENTS
            .with_borrow(|p| p.get(&args.logical_vertex_id))
            .ok_or(RouterError::VertexNotFound)?;

        let VertexPlacement::Active(loc) = placement else {
            return Err(RouterError::Forbidden);
        };
        if loc.shard_id != shard_id {
            return Err(RouterError::Forbidden);
        }

        let physical_key = PhysicalPlacementKey::new(loc.shard_id, loc.local_vertex_id);
        ROUTER_PLACEMENT_BY_PHYSICAL.with_borrow_mut(|p| {
            p.remove(physical_key);
        });
        ROUTER_PLACEMENTS.with_borrow_mut(|p| {
            p.remove(&args.logical_vertex_id);
        });
        Ok(())
    }

    fn shard_id_for_graph_caller(&self, caller: Principal) -> Result<ShardId, RouterError> {
        ROUTER_SHARD_BY_GRAPH
            .with_borrow(|m| m.get(&caller))
            .ok_or(RouterError::ShardNotRegistered)
    }
}

fn intern_vertex_label_name(name: &str) -> Result<VertexLabelId, RouterError> {
    if let Some(id) = ROUTER_VERTEX_LABEL_BY_NAME.with_borrow(|m| m.get(&name.to_string())) {
        return Ok(VertexLabelId::from_raw(id));
    }
    let next_id = ROUTER_VERTEX_LABEL_BY_ID
        .with_borrow(|m| m.keys().max().unwrap_or(0))
        .saturating_add(1);
    if next_id == 0 {
        return Err(RouterError::InvalidArgument(
            "vertex label id 0 is reserved".into(),
        ));
    }
    ROUTER_VERTEX_LABEL_BY_NAME.with_borrow_mut(|m| {
        m.insert(name.to_string(), next_id);
    });
    ROUTER_VERTEX_LABEL_BY_ID.with_borrow_mut(|m| {
        m.insert(next_id, name.to_string());
    });
    Ok(VertexLabelId::from_raw(next_id))
}

fn intern_edge_label_name(name: &str) -> Result<EdgeLabelId, RouterError> {
    if let Some(id) = ROUTER_EDGE_LABEL_BY_NAME.with_borrow(|m| m.get(&name.to_string())) {
        return Ok(EdgeLabelId::from_raw(id));
    }
    let next_id = ROUTER_EDGE_LABEL_BY_ID
        .with_borrow(|m| m.keys().max().unwrap_or(0))
        .saturating_add(1);
    if next_id == 0 || next_id > EDGE_LABEL_CATALOG_MAX {
        return Err(RouterError::InvalidArgument(format!(
            "edge label id exhausted (max {EDGE_LABEL_CATALOG_MAX})"
        )));
    }
    ROUTER_EDGE_LABEL_BY_NAME.with_borrow_mut(|m| {
        m.insert(name.to_string(), next_id);
    });
    ROUTER_EDGE_LABEL_BY_ID.with_borrow_mut(|m| {
        m.insert(next_id, name.to_string());
    });
    Ok(EdgeLabelId::from_raw(next_id))
}

fn apply_label_delta(
    label_id: u16,
    shard_id: ShardId,
    delta: i64,
    stats_map: &'static std::thread::LocalKey<
        std::cell::RefCell<super::stable::memory::StableLabelStatsMap>,
    >,
    live_by_shard: &'static std::thread::LocalKey<
        std::cell::RefCell<super::stable::memory::StableLabelShardLiveMap>,
    >,
) {
    if delta == 0 {
        return;
    }
    let magnitude = delta.unsigned_abs();
    stats_map.with_borrow_mut(|stats| {
        let mut entry = stats.get(&label_id).unwrap_or_default();
        if delta > 0 {
            entry.live_count = entry.live_count.saturating_add(magnitude);
            entry.total_adds = entry.total_adds.saturating_add(magnitude);
        } else {
            entry.live_count = entry.live_count.saturating_sub(magnitude);
            entry.total_removes = entry.total_removes.saturating_add(magnitude);
        }
        stats.insert(label_id, entry);
    });

    let key = LabelShardKey::new(shard_id, label_id);
    live_by_shard.with_borrow_mut(|live| {
        let current = live.get(&key).unwrap_or(0);
        let next = if delta > 0 {
            current.saturating_add(magnitude)
        } else {
            current.saturating_sub(magnitude)
        };
        if next == 0 {
            live.remove(&key);
        } else {
            live.insert(key, next);
        }
    });
}

fn validate_metadata_name(name: &str) -> Result<(), RouterError> {
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

fn validate_client_mutation_key(key: &str) -> Result<(), RouterError> {
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

fn ic_time_ns() -> u64 {
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
mod tests {
    use super::*;
    use crate::init::RouterInitArgs;
    use crate::types::{GraphStatus, ProvisioningState};
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql_planner::{NodeLabelRef, PlanOp};
    use std::collections::BTreeSet;

    fn graph_principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    fn test_init_args() -> RouterInitArgs {
        RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        }
    }

    #[test]
    fn register_shard_and_allocate_commit_placement() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);

        let graph = graph_principal(1);
        let index = graph_principal(2);

        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: 7,
                graph_canister: graph,
                index_canister: index,
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register");

        let logical = store.allocate_logical_vertex_id(graph).expect("allocate");
        assert_eq!(logical, 1);

        store
            .commit_vertex_placement(
                graph,
                CommitVertexPlacementArgs {
                    logical_vertex_id: logical,
                    local_vertex_id: 42,
                },
            )
            .expect("commit");

        assert_eq!(store.resolve_logical_at(7, 42).expect("reverse"), logical);

        let placement = store.resolve_placement(logical).expect("resolve");
        assert_eq!(
            placement,
            VertexPlacement::Active(PhysicalVertexLocation::new(7, 42))
        );
    }

    #[test]
    fn list_shards_for_graph_returns_matching_registrations() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);

        let graph_a = graph_principal(1);
        let graph_b = graph_principal(4);
        let graph_c = graph_principal(5);
        let index = graph_principal(2);

        for (shard_id, graph) in [(7, graph_a), (9, graph_c), (11, graph_b)] {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id,
                    graph_canister: graph,
                    index_canister: index,
                    logical_graph_name: if shard_id != 11 {
                        "tenant.main".into()
                    } else {
                        "other.graph".into()
                    },
                },
            ))
            .expect("register");
        }

        let listed = store.list_shards_for_graph("tenant.main").expect("list");
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|e| e.shard_id == 7));
        assert!(listed.iter().any(|e| e.shard_id == 9));
    }

    #[test]
    fn unregister_shard_removes_registry_and_leaves_siblings() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);

        let graph_a = graph_principal(1);
        let graph_b = graph_principal(4);
        let index = graph_principal(2);

        for (shard_id, graph) in [(7, graph_a), (9, graph_b)] {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id,
                    graph_canister: graph,
                    index_canister: index,
                    logical_graph_name: "tenant.main".into(),
                },
            ))
            .expect("register");
        }

        futures::executor::block_on(store.admin_unregister_shard(admin, 7)).expect("unregister");

        let listed = store.list_shards_for_graph("tenant.main").expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].shard_id, 9);
        assert_eq!(listed[0].graph_canister, graph_b);
        assert!(store.resolve_shard(7).is_err());
        assert!(store.resolve_shard(9).is_ok());
    }

    #[test]
    fn release_logical_vertex_placement_clears_registry() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);

        let graph = graph_principal(1);
        let index = graph_principal(2);

        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: 7,
                graph_canister: graph,
                index_canister: index,
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register");

        let logical = store.allocate_logical_vertex_id(graph).expect("allocate");
        store
            .commit_vertex_placement(
                graph,
                CommitVertexPlacementArgs {
                    logical_vertex_id: logical,
                    local_vertex_id: 42,
                },
            )
            .expect("commit");

        store
            .release_logical_vertex_placement(
                graph,
                ReleaseLogicalVertexArgs {
                    logical_vertex_id: logical,
                },
            )
            .expect("release");

        assert!(store.resolve_placement(logical).is_err());
        assert!(store.resolve_logical_at(7, 42).is_err());
    }

    #[test]
    fn vertex_migration_updates_placement_and_physical_reverse_index() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);

        let source_graph = graph_principal(1);
        let dest_graph = graph_principal(3);
        let index = graph_principal(2);

        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: 7,
                graph_canister: source_graph,
                index_canister: index,
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register source");

        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: 9,
                graph_canister: dest_graph,
                index_canister: index,
                logical_graph_name: "tenant.main".into(),
            },
        ))
        .expect("register destination");

        let logical = store
            .allocate_logical_vertex_id(source_graph)
            .expect("allocate");
        store
            .commit_vertex_placement(
                source_graph,
                CommitVertexPlacementArgs {
                    logical_vertex_id: logical,
                    local_vertex_id: 42,
                },
            )
            .expect("commit");

        store
            .begin_vertex_migration(
                source_graph,
                BeginVertexMigrationArgs {
                    logical_vertex_id: logical,
                    destination_shard_id: 9,
                },
            )
            .expect("begin");

        assert_eq!(
            store.resolve_placement(logical).expect("placement"),
            VertexPlacement::Migrating {
                epoch: 1,
                source: PhysicalVertexLocation::new(7, 42),
                destination_shard_id: 9,
            }
        );

        store
            .finish_vertex_migration(
                dest_graph,
                FinishVertexMigrationArgs {
                    logical_vertex_id: logical,
                    destination_local_vertex_id: 5,
                },
            )
            .expect("finish");

        assert_eq!(
            store.resolve_placement(logical).expect("placement"),
            VertexPlacement::Active(PhysicalVertexLocation::new(9, 5))
        );
        assert_eq!(
            store.resolve_logical_at(9, 5).expect("new physical"),
            logical
        );
        assert!(store.resolve_logical_at(7, 42).is_err());
    }

    #[test]
    fn resolve_graph_checks_permissions() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        let owner = graph_principal(10);
        let other = graph_principal(11);

        store
            .admin_register_graph(
                admin,
                GraphRegistryEntry {
                    graph_name: "g".into(),
                    canister_id: owner,
                    owner,
                    admins: BTreeSet::new(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                },
            )
            .expect("register");

        assert!(store.resolve_graph("g", owner).is_ok());
        assert_eq!(store.resolve_graph("g", other), Err(RouterError::Forbidden));
    }

    #[test]
    fn vertex_and_edge_labels_with_same_name_get_distinct_ids() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);

        let v = store
            .admin_intern_vertex_label(admin, "Person")
            .expect("vertex label");
        let e = store
            .admin_intern_edge_label(admin, "Person")
            .expect("edge label");
        // Same numeric id is fine — namespaces are separate.
        assert_eq!(v.raw(), 1);
        assert_eq!(e.raw(), 1);
        assert_eq!(store.lookup_vertex_label_id("Person").unwrap(), v);
        assert_eq!(store.lookup_edge_label_id("Person").unwrap(), e);
        assert!(store.lookup_edge_label_id("KNOWS").is_err());
        let v2 = store
            .admin_intern_vertex_label(admin, "KNOWS")
            .expect("vertex only");
        assert_eq!(v2.raw(), 2);
    }

    #[test]
    fn read_plan_requires_existing_label() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());

        let plan = PhysicalPlan::from_ops(vec![PlanOp::NodeScan {
            variable: "n".into(),
            label: Some(NodeLabelRef::from("Missing")),
            property_projection: None,
        }]);

        assert_eq!(
            store.resolve_plan_labels(&[plan]),
            Err(RouterError::NotFound("Missing".into()))
        );
    }

    #[test]
    fn dml_plan_creates_only_requested_label_namespaces() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());

        let node_only = PhysicalPlan::from_ops(vec![PlanOp::InsertVertex {
            variable: Some("n".into()),
            labels: vec![NodeLabelRef::from("Person")],
            properties: vec![],
        }]);

        let resolved = store
            .resolve_plan_labels(&[node_only])
            .expect("resolve node DML labels");
        assert_eq!(resolved.vertex.len(), 1);
        assert_eq!(resolved.vertex[0].name, "Person");
        assert_eq!(resolved.vertex[0].id.raw(), 1);
        assert!(resolved.edge.is_empty());
        assert_eq!(store.lookup_vertex_label_id("Person").unwrap().raw(), 1);
        assert!(store.lookup_edge_label_id("Person").is_err());

        let edge_only = PhysicalPlan::from_ops(vec![PlanOp::InsertEdge {
            variable: Some("e".into()),
            src: "a".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            labels: vec!["Person".into()],
            properties: vec![],
        }]);

        let resolved = store
            .resolve_plan_labels(&[edge_only])
            .expect("resolve edge DML labels");
        assert_eq!(resolved.edge.len(), 1);
        assert_eq!(resolved.edge[0].name, "Person");
        assert_eq!(resolved.edge[0].id.raw(), 1);
        assert_eq!(store.lookup_vertex_label_id("Person").unwrap().raw(), 1);
        assert_eq!(store.lookup_edge_label_id("Person").unwrap().raw(), 1);
    }

    #[test]
    fn label_usage_delta_updates_namespace_separated_stats() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);

        let vertex_label = store
            .admin_intern_vertex_label(admin, "Person")
            .expect("vertex label");
        let edge_label = store
            .admin_intern_edge_label(admin, "Person")
            .expect("edge label");

        store.apply_label_usage_delta(
            7,
            &LabelUsageDelta {
                vertex: vec![(vertex_label, 2)],
                edge: vec![(edge_label, 3)],
            },
        );

        assert_eq!(
            store.vertex_label_stats(vertex_label),
            LabelStats {
                live_count: 2,
                total_adds: 2,
                total_removes: 0
            }
        );
        assert_eq!(
            store.edge_label_stats(edge_label),
            LabelStats {
                live_count: 3,
                total_adds: 3,
                total_removes: 0
            }
        );
        assert_eq!(store.vertex_label_shard_live_count(7, vertex_label), 2);
        assert_eq!(store.edge_label_shard_live_count(7, edge_label), 3);

        store.apply_label_usage_delta(
            7,
            &LabelUsageDelta {
                vertex: vec![(vertex_label, -1)],
                edge: vec![(edge_label, -2)],
            },
        );

        assert_eq!(
            store.vertex_label_stats(vertex_label),
            LabelStats {
                live_count: 1,
                total_adds: 2,
                total_removes: 1
            }
        );
        assert_eq!(
            store.edge_label_stats(edge_label),
            LabelStats {
                live_count: 1,
                total_adds: 3,
                total_removes: 2
            }
        );
        assert_eq!(store.vertex_label_shard_live_count(7, vertex_label), 1);
        assert_eq!(store.edge_label_shard_live_count(7, edge_label), 1);
    }

    #[test]
    fn label_usage_delta_tracks_per_shard_live_counts() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        let label = store
            .admin_intern_vertex_label(admin, "Person")
            .expect("vertex label");

        store.apply_label_usage_delta(
            7,
            &LabelUsageDelta {
                vertex: vec![(label, 2)],
                edge: vec![],
            },
        );
        store.apply_label_usage_delta(
            9,
            &LabelUsageDelta {
                vertex: vec![(label, 1)],
                edge: vec![],
            },
        );
        store.apply_label_usage_delta(
            7,
            &LabelUsageDelta {
                vertex: vec![(label, -1)],
                edge: vec![],
            },
        );

        assert_eq!(
            store.vertex_label_stats(label),
            LabelStats {
                live_count: 2,
                total_adds: 3,
                total_removes: 1
            }
        );
        assert_eq!(store.vertex_label_shard_live_count(7, label), 1);
        assert_eq!(store.vertex_label_shard_live_count(9, label), 1);
    }

    #[test]
    fn label_telemetry_event_applies_once_by_shard_event_seq() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        let label = store
            .admin_intern_vertex_label(admin, "Person")
            .expect("vertex label");
        let event = LabelTelemetryEventWire {
            mutation_id: 1,
            shard_event_seq: 99,
            label_usage_delta: LabelUsageDelta {
                vertex: vec![(label, 2)],
                edge: vec![],
            },
        };

        assert!(store.apply_label_telemetry_event(7, &event));
        assert!(!store.apply_label_telemetry_event(7, &event));
        assert_eq!(
            store.vertex_label_stats(label),
            LabelStats {
                live_count: 2,
                total_adds: 2,
                total_removes: 0
            }
        );

        assert!(store.apply_label_telemetry_event(9, &event));
        assert_eq!(
            store.vertex_label_stats(label),
            LabelStats {
                live_count: 4,
                total_adds: 4,
                total_removes: 0
            }
        );
    }

    #[test]
    fn mutation_id_is_monotonic_and_rejects_exhaustion() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());

        assert_eq!(store.allocate_mutation_id().expect("first"), 1);
        assert_eq!(store.allocate_mutation_id().expect("second"), 2);

        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| {
            counter.set(u64::MAX);
        });
        assert_eq!(
            store.allocate_mutation_id(),
            Err(RouterError::IdExhausted("mutation_id".into()))
        );
    }

    #[test]
    fn client_mutation_key_reuses_router_mutation_id() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = graph_principal(42);
        let request = b"request-a".to_vec();

        let first = store
            .reserve_mutation_id_for_client_key(
                caller,
                "tenant.main",
                "client-key-1",
                request.clone(),
            )
            .expect("first mutation id")
            .mutation_id;
        assert_eq!(first, 1);
        store
            .record_router_mutation_shards(
                caller,
                "tenant.main",
                "client-key-1",
                ResolvedLabelTable::default(),
                vec![RouterMutationShard::new(7, graph_principal(1), None)],
            )
            .expect("record empty envelope");
        assert_eq!(
            store
                .reserve_mutation_id_for_client_key(
                    caller,
                    "tenant.main",
                    "client-key-1",
                    request.clone()
                )
                .expect("retry mutation id")
                .mutation_id,
            first
        );
        assert_eq!(
            store
                .reserve_mutation_id_for_client_key(
                    caller,
                    "tenant.main",
                    "client-key-2",
                    request.clone()
                )
                .expect("second mutation id")
                .mutation_id,
            2
        );
        assert_eq!(
            store
                .reserve_mutation_id_for_client_key(
                    graph_principal(43),
                    "tenant.main",
                    "client-key-1",
                    request
                )
                .expect("different caller mutation id")
                .mutation_id,
            3
        );
    }

    #[test]
    fn client_mutation_key_rejects_different_request() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = graph_principal(42);

        assert_eq!(
            store
                .reserve_mutation_id_for_client_key(
                    caller,
                    "tenant.main",
                    "client-key-1",
                    b"a".to_vec()
                )
                .expect("first mutation id")
                .mutation_id,
            1
        );
        assert_eq!(
            store.reserve_mutation_id_for_client_key(
                caller,
                "tenant.main",
                "client-key-1",
                b"b".to_vec()
            ),
            Err(RouterError::Conflict(
                "client_mutation_key was already used for a different request".into()
            ))
        );
    }

    #[test]
    fn client_mutation_key_rejects_expired_key() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = graph_principal(42);
        let key = ClientMutationKey::new(caller, "tenant.main".into(), "client-key-1".into());
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            m.insert(
                key,
                RouterMutationRecord::new(
                    1,
                    0u64.saturating_sub(CLIENT_MUTATION_KEY_TTL_NS + 1),
                    b"a".to_vec(),
                ),
            );
        });

        assert_eq!(
            store.reserve_mutation_id_for_client_key_at(
                caller,
                "tenant.main",
                "client-key-1",
                b"a".to_vec(),
                CLIENT_MUTATION_KEY_TTL_NS + 1
            ),
            Err(RouterError::InvalidArgument(
                "client_mutation_key expired; use a new key for a new mutation".into()
            ))
        );
    }

    #[test]
    fn client_mutation_key_blocks_concurrent_routing_owner() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = graph_principal(42);

        let first = store
            .reserve_mutation_id_for_client_key(
                caller,
                "tenant.main",
                "client-key-1",
                b"a".to_vec(),
            )
            .expect("first owner");
        assert_eq!(first.mutation_id, 1);
        assert!(first.routing_owner);

        assert_eq!(
            store.reserve_mutation_id_for_client_key(
                caller,
                "tenant.main",
                "client-key-1",
                b"a".to_vec(),
            ),
            Err(RouterError::Conflict(
                "client_mutation_key is already in progress; retry later".into()
            ))
        );

        store
            .record_router_mutation_shards(
                caller,
                "tenant.main",
                "client-key-1",
                ResolvedLabelTable::default(),
                vec![RouterMutationShard::new(7, graph_principal(1), None)],
            )
            .expect("record envelope");
        let retry = store
            .reserve_mutation_id_for_client_key(
                caller,
                "tenant.main",
                "client-key-1",
                b"a".to_vec(),
            )
            .expect("retry after envelope");
        assert_eq!(retry.mutation_id, first.mutation_id);
        assert!(!retry.routing_owner);
    }

    #[test]
    fn abandoned_routing_reservation_preserves_id_and_allows_new_owner() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = graph_principal(42);

        let first = store
            .reserve_mutation_id_for_client_key(
                caller,
                "tenant.main",
                "client-key-1",
                b"a".to_vec(),
            )
            .expect("first owner");
        assert_eq!(first.mutation_id, 1);
        assert!(first.routing_owner);

        store
            .abandon_router_mutation_routing_reservation(caller, "tenant.main", "client-key-1")
            .expect("abandon reservation");
        let record = store
            .router_mutation_record(caller, "tenant.main", "client-key-1")
            .expect("record");
        assert_eq!(record.mutation_id, first.mutation_id);
        assert_eq!(record.request_fingerprint, b"a".to_vec());
        assert!(!record.routing_in_progress);

        let retry = store
            .reserve_mutation_id_for_client_key(
                caller,
                "tenant.main",
                "client-key-1",
                b"a".to_vec(),
            )
            .expect("retry owner");
        assert_eq!(retry.mutation_id, first.mutation_id);
        assert!(retry.routing_owner);
    }

    #[test]
    fn router_mutation_journal_tracks_shard_completion() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = graph_principal(42);
        store
            .reserve_mutation_id_for_client_key(
                caller,
                "tenant.main",
                "client-key-1",
                b"a".to_vec(),
            )
            .expect("mutation id");
        store
            .record_router_mutation_shards(
                caller,
                "tenant.main",
                "client-key-1",
                ResolvedLabelTable::default(),
                vec![
                    RouterMutationShard::new(7, graph_principal(1), Some(vec![1])),
                    RouterMutationShard::new(9, graph_principal(2), None),
                ],
            )
            .expect("record shards");
        let record = store
            .router_mutation_record(caller, "tenant.main", "client-key-1")
            .expect("record");
        assert_eq!(record.resolved_labels, Some(ResolvedLabelTable::default()));
        assert_eq!(
            store.router_mutation_completed_row_count(caller, "tenant.main", "client-key-1"),
            None
        );

        store
            .record_router_mutation_shard_completed(
                caller,
                "tenant.main",
                "client-key-1",
                7,
                2,
                Vec::new(),
            )
            .expect("complete shard 7");
        store
            .record_router_mutation_shard_telemetry_acked(caller, "tenant.main", "client-key-1", 7)
            .expect("ack shard 7");
        assert_eq!(
            store.router_mutation_completed_row_count(caller, "tenant.main", "client-key-1"),
            None
        );

        store
            .record_router_mutation_shard_completed(
                caller,
                "tenant.main",
                "client-key-1",
                9,
                3,
                Vec::new(),
            )
            .expect("complete shard 9");
        store
            .record_router_mutation_shard_telemetry_acked(caller, "tenant.main", "client-key-1", 9)
            .expect("ack shard 9");
        assert_eq!(
            store.router_mutation_completed_row_count(caller, "tenant.main", "client-key-1"),
            Some(5)
        );
    }

    #[test]
    fn router_mutation_journal_records_zero_shard_completion() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = graph_principal(42);
        store
            .reserve_mutation_id_for_client_key(
                caller,
                "tenant.main",
                "client-key-1",
                b"a".to_vec(),
            )
            .expect("mutation id");
        store
            .record_router_mutation_completed_without_shards(
                caller,
                "tenant.main",
                "client-key-1",
                ResolvedLabelTable::default(),
                0,
            )
            .expect("record zero-shard completion");

        let record = store
            .router_mutation_record(caller, "tenant.main", "client-key-1")
            .expect("record");
        assert_eq!(record.completed_row_count, Some(0));
        assert!(record.shards.is_empty());
        assert_eq!(
            store.router_mutation_completed_row_count(caller, "tenant.main", "client-key-1"),
            Some(0)
        );
    }
}
