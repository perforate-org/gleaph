//! Stateless facade over router stable storage.

use super::stable::{
    ROUTER_CONTROLLERS, ROUTER_EDGE_LABEL_BY_ID, ROUTER_EDGE_LABEL_BY_NAME, ROUTER_GRAPHS,
    ROUTER_LOGICAL_COUNTER, ROUTER_PENDING_LOGICAL, ROUTER_PLACEMENT_BY_PHYSICAL,
    ROUTER_PLACEMENTS, ROUTER_PROPERTY_BY_ID,
    ROUTER_PROPERTY_BY_NAME, ROUTER_SHARD_BY_GRAPH, ROUTER_SHARDS, ROUTER_VERTEX_LABEL_BY_ID,
    ROUTER_VERTEX_LABEL_BY_NAME,
};
use crate::index_sync;
use crate::init::RouterInitArgs;
use crate::state::RouterError;
use crate::types::{
    AdminRegisterShardArgs, CommitVertexPlacementArgs, EdgeLabelId, GraphRegistryEntry, GraphStatus,
    PropertyId, ShardId, VertexLabelId, VertexPlacement,
};
use candid::Principal;
use gleaph_graph_kernel::entry::EDGE_LABEL_CATALOG_MAX;
use gleaph_graph_kernel::federation::{
    LocalVertexId, LogicalVertexId, PhysicalPlacementKey, PhysicalVertexLocation,
    ShardRegistryEntry,
};

const MAX_METADATA_NAME_BYTES: usize = 256;

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
        ROUTER_LOGICAL_COUNTER.with_borrow_mut(|c| {
            c.set(0);
        });
        ROUTER_PENDING_LOGICAL.with_borrow_mut(|p| p.clear_new());
        ROUTER_VERTEX_LABEL_BY_NAME.with_borrow_mut(|m| m.clear_new());
        ROUTER_VERTEX_LABEL_BY_ID.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_BY_NAME.with_borrow_mut(|m| m.clear_new());
        ROUTER_EDGE_LABEL_BY_ID.with_borrow_mut(|m| m.clear_new());
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
            .with_borrow(|p| {
                p.get(PhysicalPlacementKey::new(shard_id, local_vertex_id))
            })
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
        if args.graph_canister == Principal::anonymous() || args.index_canister == Principal::anonymous()
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

        index_sync::admin_set_shard_owner(
            args.index_canister,
            args.shard_id,
            args.graph_canister,
        )
        .await
        .map_err(RouterError::Internal)?;

        ROUTER_SHARDS.with_borrow_mut(|s| {
            s.insert(args.shard_id, entry);
        });
        ROUTER_SHARD_BY_GRAPH.with_borrow_mut(|m| {
            m.insert(args.graph_canister, args.shard_id);
        });
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

        index_sync::admin_clear_shard_owner(entry.index_canister, shard_id)
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

    pub fn reverse_vertex_label_name(&self, label_id: VertexLabelId) -> Result<String, RouterError> {
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

        if ROUTER_PLACEMENTS
            .with_borrow(|p| p.contains_key(&args.logical_vertex_id))
        {
            return Err(RouterError::PlacementAlreadyCommitted);
        }

        let placement = VertexPlacement::Active(PhysicalVertexLocation::new(
            shard_id,
            args.local_vertex_id,
        ));
        let physical_key =
            PhysicalPlacementKey::new(shard_id, args.local_vertex_id);
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

fn validate_metadata_name(name: &str) -> Result<(), RouterError> {
    if name.is_empty() {
        return Err(RouterError::InvalidArgument("name must not be empty".into()));
    }
    if name.len() > MAX_METADATA_NAME_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "name exceeds {MAX_METADATA_NAME_BYTES} UTF-8 bytes"
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
    use std::collections::BTreeSet;

    fn graph_principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    #[test]
    fn register_shard_and_allocate_commit_placement() {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            controllers: vec![],
        });
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

        let logical = store
            .allocate_logical_vertex_id(graph)
            .expect("allocate");
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

        assert_eq!(
            store.resolve_logical_at(7, 42).expect("reverse"),
            logical
        );

        let placement = store.resolve_placement(logical).expect("resolve");
        assert_eq!(
            placement,
            VertexPlacement::Active(PhysicalVertexLocation::new(7, 42))
        );
    }

    #[test]
    fn resolve_graph_checks_permissions() {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            controllers: vec![],
        });
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
        store.init_from_args(&RouterInitArgs {
            controllers: vec![],
        });
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
}
