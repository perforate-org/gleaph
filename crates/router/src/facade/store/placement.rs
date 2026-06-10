//! Logical vertex placement and reverse physical lookup.

use super::super::stable::{
    ROUTER_LOGICAL_COUNTER, ROUTER_PENDING_LOGICAL, ROUTER_PLACEMENT_BY_PHYSICAL,
    ROUTER_PLACEMENTS, ROUTER_SHARD_BY_GRAPH,
};
use crate::state::RouterError;
use crate::types::{CommitVertexPlacementArgs, ReleaseLogicalVertexArgs, ShardId, VertexPlacement};
use candid::Principal;
use gleaph_graph_kernel::federation::{
    LocalVertexId, LogicalVertexId, PhysicalPlacementKey, PhysicalVertexLocation,
};

use super::RouterStore;

impl RouterStore {
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

    pub fn release_logical_vertex_placement(
        &self,
        caller: Principal,
        args: ReleaseLogicalVertexArgs,
    ) -> Result<(), RouterError> {
        let shard_id = self.shard_id_for_graph_caller(caller)?;

        let placement = ROUTER_PLACEMENTS
            .with_borrow(|p| p.get(&args.logical_vertex_id))
            .ok_or(RouterError::VertexNotFound)?;

        let VertexPlacement::Active(loc) = placement;
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

    pub(super) fn shard_id_for_graph_caller(
        &self,
        caller: Principal,
    ) -> Result<ShardId, RouterError> {
        ROUTER_SHARD_BY_GRAPH
            .with_borrow(|m| m.get(&caller))
            .ok_or(RouterError::ShardNotRegistered)
    }
}
