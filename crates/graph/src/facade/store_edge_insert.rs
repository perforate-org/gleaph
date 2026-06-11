//! Canonical edge insert paths (directed / undirected, local / logical, valued / unvalued).

use gleaph_graph_kernel::entry::EdgeLabelId;
use gleaph_graph_kernel::federation::{GlobalVertexId, VertexPlacement};
use ic_stable_lara::VertexId;

use crate::index::placement;

use super::store::{EdgeHandle, GraphStore, GraphStoreError};

fn resolve_local_endpoint(store: &GraphStore, vertex_id: GlobalVertexId) -> Option<VertexId> {
    let routing = store.federation_routing()?;
    if vertex_id.shard_id != routing.shard_id {
        return None;
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let placement = pollster::block_on(placement::resolve_placement(
            routing.router_canister,
            vertex_id,
        ))
        .ok()?;
        let VertexPlacement::Active(loc) = placement;
        if loc.shard_id != routing.shard_id {
            return None;
        }
        Some(VertexId::from(loc.local_vertex_id))
    }
    #[cfg(target_family = "wasm")]
    {
        Some(VertexId::from(vertex_id.local_vertex_id))
    }
}

/// Target endpoint for an edge insert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertEdgeTarget {
    Local(VertexId),
    Remote(GlobalVertexId),
}

/// Edge topology for insert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertEdgeTopology {
    Directed,
    Undirected,
}

/// Single edge insert request (facade-internal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InsertEdgeSpec<'a> {
    pub topology: InsertEdgeTopology,
    pub target: InsertEdgeTarget,
    pub catalog_label: Option<EdgeLabelId>,
    pub payload_bytes: Option<&'a [u8]>,
}

impl GraphStore {
    pub(crate) fn insert_edge_by_spec(
        &self,
        source_vertex_id: VertexId,
        spec: InsertEdgeSpec<'_>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let valued = spec.payload_bytes.is_some_and(|b| !b.is_empty());
        let value = spec.payload_bytes.unwrap_or(&[]);

        match (spec.target, spec.topology, valued) {
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Directed, false) => {
                self.insert_directed_edge(source_vertex_id, target, spec.catalog_label)
            }
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Directed, true) => self
                .insert_directed_edge_with_payload_bytes(
                    source_vertex_id,
                    target,
                    spec.catalog_label,
                    value,
                ),
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Undirected, false) => {
                self.insert_undirected_edge(source_vertex_id, target, spec.catalog_label)
            }
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Undirected, true) => self
                .insert_undirected_edge_with_payload_bytes(
                    source_vertex_id,
                    target,
                    spec.catalog_label,
                    value,
                ),
            (InsertEdgeTarget::Remote(vertex_id), InsertEdgeTopology::Directed, false) => self
                .insert_directed_edge_to_logical(source_vertex_id, vertex_id, spec.catalog_label),
            (InsertEdgeTarget::Remote(vertex_id), InsertEdgeTopology::Directed, true) => self
                .insert_directed_edge_to_logical_with_payload_bytes(
                    source_vertex_id,
                    vertex_id,
                    spec.catalog_label,
                    value,
                ),
            (InsertEdgeTarget::Remote(vertex_id), InsertEdgeTopology::Undirected, _) => self
                .insert_undirected_edge_to_logical_with_payload_bytes(
                    source_vertex_id,
                    vertex_id,
                    spec.catalog_label,
                    value,
                ),
        }
    }
}
