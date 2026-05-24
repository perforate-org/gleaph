//! GraphStore `vertex` implementation.

use super::super::stable::{GRAPH, REMOTE_VERTEX_REFS, VERTEX_LOGICAL_IDS, VERTEX_MIGRATION_STATE};
use crate::index::placement;
use gleaph_graph_kernel::entry::{Edge, EdgeTarget, RemoteRefId, Vertex};
use gleaph_graph_kernel::federation::{
    CommitVertexPlacementArgs, LogicalVertexId, standalone_logical_vertex_id,
};
use gleaph_graph_kernel::path::GraphPathVertexId;
use ic_stable_lara::{DeferredBidirectionalLabeledError, VertexCount, VertexId, traits::CsrEdge};

use super::GraphStore;
use super::error::GraphStoreError;
use super::helpers::canonical_undirected_owner;

impl GraphStore {
    pub fn vertex_count(&self) -> VertexCount {
        GRAPH.with_borrow(|graph| graph.vertex_count())
    }

    pub fn insert_vertex(&self) -> Result<VertexId, GraphStoreError> {
        #[cfg(not(target_family = "wasm"))]
        {
            return pollster::block_on(self.insert_vertex_row(Vertex::default()));
        }
        #[cfg(target_family = "wasm")]
        {
            ic_cdk::trap("insert_vertex: use insert_vertex_row().await on wasm");
        }
    }

    pub async fn insert_vertex_row(&self, vertex: Vertex) -> Result<VertexId, GraphStoreError> {
        let pending_logical = match self.federation_routing() {
            Some(routing) => {
                Some(placement::allocate_logical_vertex_id(routing.router_canister).await?)
            }
            None => None,
        };

        let vertex_id = self
            .with_graph_mut(|graph| graph.push_vertex_row(vertex.into()))
            .map_err(GraphStoreError::from)?;

        let logical_vertex_id = match pending_logical {
            Some(logical_vertex_id) => {
                let routing = self
                    .federation_routing()
                    .expect("federation routing required after allocate");
                placement::commit_vertex_placement(
                    routing.router_canister,
                    CommitVertexPlacementArgs {
                        logical_vertex_id,
                        local_vertex_id: placement::local_vertex_id_raw(vertex_id),
                    },
                )
                .await?;
                logical_vertex_id
            }
            None => standalone_logical_vertex_id(vertex_id),
        };

        VERTEX_LOGICAL_IDS.with_borrow_mut(|map| {
            map.insert(vertex_id, logical_vertex_id);
        });
        Ok(vertex_id)
    }

    pub fn logical_vertex_id(&self, vertex_id: VertexId) -> Option<LogicalVertexId> {
        VERTEX_LOGICAL_IDS
            .with_borrow(|map| map.get(vertex_id))
            .or_else(|| {
                self.federation_routing()
                    .is_none()
                    .then(|| standalone_logical_vertex_id(vertex_id))
            })
    }

    pub(crate) fn assert_local_vertex_writable(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        if self.vertex(vertex_id).is_some_and(|v| v.is_tombstone()) {
            return Err(GraphStoreError::VertexTombstoned);
        }
        let Some(routing) = self.federation_routing() else {
            return Ok(());
        };
        let Some(logical_vertex_id) = self.logical_vertex_id(vertex_id) else {
            return Ok(());
        };
        let local = placement::local_vertex_id_raw(vertex_id);
        #[cfg(target_family = "wasm")]
        let placement = {
            let _ = logical_vertex_id;
            gleaph_graph_kernel::federation::VertexPlacement::Active(
                gleaph_graph_kernel::federation::PhysicalVertexLocation::new(
                    routing.shard_id,
                    local,
                ),
            )
        };
        #[cfg(not(target_family = "wasm"))]
        let placement = pollster::block_on(placement::resolve_placement(
            routing.router_canister,
            logical_vertex_id,
        ))?;
        if let Some(state) =
            crate::facade::stable::VERTEX_MIGRATION_STATE.with_borrow(|m| m.get(local))
        {
            match state {
                gleaph_graph_kernel::federation::VertexMigrationState::TargetStaging { .. }
                | gleaph_graph_kernel::federation::VertexMigrationState::ForwardingStub {
                    ..
                } => {
                    return Err(GraphStoreError::VertexMigrating);
                }
                gleaph_graph_kernel::federation::VertexMigrationState::SourceMigrating {
                    ..
                } => {}
                gleaph_graph_kernel::federation::VertexMigrationState::Active => {}
            }
        }
        if let gleaph_graph_kernel::federation::VertexPlacement::Migrating {
            destination_shard_id,
            ..
        } = placement
        {
            if destination_shard_id == routing.shard_id && {
                VERTEX_MIGRATION_STATE
                    .with_borrow(|m| m.get(local))
                    .is_some_and(|s| {
                        matches!(
                            s,
                            gleaph_graph_kernel::federation::VertexMigrationState::TargetStaging {
                                ..
                            }
                        )
                    })
            } {
                return Err(GraphStoreError::VertexMigrating);
            }
        }
        Ok(())
    }

    pub(crate) fn path_vertex_element_id(&self, vertex_id: VertexId) -> Option<GraphPathVertexId> {
        self.logical_vertex_id(vertex_id)
            .map(GraphPathVertexId::new)
    }

    pub fn ensure_remote_ref(&self, logical_vertex_id: LogicalVertexId) -> RemoteRefId {
        REMOTE_VERTEX_REFS.with_borrow_mut(|table| table.ensure_remote_ref(logical_vertex_id))
    }

    pub fn logical_vertex_for_remote_ref(
        &self,
        remote_ref: RemoteRefId,
    ) -> Option<LogicalVertexId> {
        REMOTE_VERTEX_REFS.with_borrow(|table| table.logical_vertex_id(remote_ref))
    }

    pub fn remote_ref_for_logical(
        &self,
        logical_vertex_id: LogicalVertexId,
    ) -> Option<RemoteRefId> {
        REMOTE_VERTEX_REFS.with_borrow(|table| table.remote_ref_for_logical(logical_vertex_id))
    }

    pub(crate) fn edge_sidecar_owner_from_in_row(&self, dst: VertexId, edge: &Edge) -> VertexId {
        if self.edge_is_undirected(dst, edge).unwrap_or(false) {
            canonical_undirected_owner(dst, edge.neighbor_vid())
        } else {
            edge.neighbor_vid()
        }
    }

    pub fn edge_target(&self, edge: &Edge) -> Option<EdgeTarget> {
        edge.edge_target()
    }

    pub(crate) fn push_migrated_vertex_row(
        &self,
        vertex: Vertex,
    ) -> Result<VertexId, DeferredBidirectionalLabeledError> {
        self.with_graph_mut(|graph| graph.push_vertex_row(vertex.into()))
    }

    pub(crate) fn register_logical_vertex_mapping(
        &self,
        vertex_id: VertexId,
        logical_vertex_id: LogicalVertexId,
    ) {
        VERTEX_LOGICAL_IDS.with_borrow_mut(|map| {
            map.insert(vertex_id, logical_vertex_id);
        });
    }
}
