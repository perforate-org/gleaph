//! Global vertex placement registry.

use super::super::stable::{ROUTER_PLACEMENTS, ROUTER_SHARD_BY_GRAPH};
use crate::state::RouterError;
use crate::types::{
    CommitVertexPlacementArgs, GlobalVertexId, ReleaseVertexPlacementArgs, ShardId, VertexPlacement,
};
use candid::Principal;
use gleaph_graph_kernel::federation::{LocalVertexId, PhysicalVertexLocation};

use super::RouterStore;

impl RouterStore {
    pub fn resolve_placement(
        &self,
        vertex_id: GlobalVertexId,
    ) -> Result<VertexPlacement, RouterError> {
        ROUTER_PLACEMENTS
            .with_borrow(|p| p.get(&vertex_id))
            .ok_or(RouterError::VertexNotFound)
    }

    pub fn resolve_global_at(
        &self,
        shard_id: ShardId,
        local_vertex_id: LocalVertexId,
    ) -> Result<GlobalVertexId, RouterError> {
        let vertex_id = GlobalVertexId::new(shard_id, local_vertex_id);
        self.resolve_placement(vertex_id)?;
        Ok(vertex_id)
    }

    pub fn commit_vertex_placement(
        &self,
        caller: Principal,
        args: CommitVertexPlacementArgs,
    ) -> Result<(), RouterError> {
        let shard_id = self.shard_id_for_graph_caller(caller)?;
        let vertex_id = GlobalVertexId::new(shard_id, args.local_vertex_id);

        if ROUTER_PLACEMENTS.with_borrow(|p| p.contains_key(&vertex_id)) {
            return Err(RouterError::PlacementAlreadyCommitted);
        }

        let placement =
            VertexPlacement::Active(PhysicalVertexLocation::new(shard_id, args.local_vertex_id));
        ROUTER_PLACEMENTS.with_borrow_mut(|p| {
            p.insert(vertex_id, placement);
        });
        Ok(())
    }

    pub fn release_vertex_placement(
        &self,
        caller: Principal,
        args: ReleaseVertexPlacementArgs,
    ) -> Result<(), RouterError> {
        let shard_id = self.shard_id_for_graph_caller(caller)?;
        let vertex_id = GlobalVertexId::new(shard_id, args.local_vertex_id);

        let placement = ROUTER_PLACEMENTS
            .with_borrow(|p| p.get(&vertex_id))
            .ok_or(RouterError::VertexNotFound)?;

        let VertexPlacement::Active(loc) = placement;
        if loc.shard_id != shard_id {
            return Err(RouterError::Forbidden);
        }

        ROUTER_PLACEMENTS.with_borrow_mut(|p| {
            p.remove(&vertex_id);
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
