//! Canonical edge insert paths (directed / undirected, local / logical, valued / unvalued).

use gleaph_graph_kernel::entry::EdgeLabelId;
use gleaph_graph_kernel::federation::{ExportedEdgeTarget, LogicalVertexId, VertexPlacement};
use ic_stable_lara::VertexId;

use crate::index::placement;

use super::store::{EdgeHandle, GraphStore, GraphStoreError};

fn resolve_local_endpoint(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
) -> Option<VertexId> {
    let routing = store.federation_routing()?;
    let placement =
        placement::resolve_placement(routing.router_canister, logical_vertex_id).ok()?;
    let VertexPlacement::Active(loc) = placement else {
        return None;
    };
    if loc.shard_id != routing.shard_id {
        return None;
    }
    Some(VertexId::from(loc.local_vertex_id))
}

/// Target endpoint for an edge insert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertEdgeTarget {
    Local(VertexId),
    Logical(LogicalVertexId),
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
    pub value_bytes: Option<&'a [u8]>,
}

impl GraphStore {
    pub(crate) fn insert_edge_by_spec(
        &self,
        source_vertex_id: VertexId,
        spec: InsertEdgeSpec<'_>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let valued = spec.value_bytes.is_some_and(|b| !b.is_empty());
        let value = spec.value_bytes.unwrap_or(&[]);

        match (spec.target, spec.topology, valued) {
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Directed, false) => {
                self.insert_directed_edge(source_vertex_id, target, spec.catalog_label)
            }
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Directed, true) => self
                .insert_directed_edge_with_value_bytes(
                    source_vertex_id,
                    target,
                    spec.catalog_label,
                    value,
                ),
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Undirected, false) => {
                self.insert_undirected_edge(source_vertex_id, target, spec.catalog_label)
            }
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Undirected, true) => self
                .insert_undirected_edge_with_value_bytes(
                    source_vertex_id,
                    target,
                    spec.catalog_label,
                    value,
                ),
            (InsertEdgeTarget::Logical(logical), InsertEdgeTopology::Directed, false) => {
                self.insert_directed_edge_to_logical(source_vertex_id, logical, spec.catalog_label)
            }
            (InsertEdgeTarget::Logical(logical), InsertEdgeTopology::Directed, true) => self
                .insert_directed_edge_to_logical_with_value_bytes(
                    source_vertex_id,
                    logical,
                    spec.catalog_label,
                    value,
                ),
            (InsertEdgeTarget::Logical(logical), InsertEdgeTopology::Undirected, _) => self
                .insert_undirected_edge_to_logical_with_value_bytes(
                    source_vertex_id,
                    logical,
                    spec.catalog_label,
                    value,
                ),
        }
    }

    pub(crate) fn insert_exported_out_edge(
        &self,
        owner_vertex_id: VertexId,
        target: &ExportedEdgeTarget,
        undirected: bool,
        value_bytes: &[u8],
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let logical = match *target {
            ExportedEdgeTarget::Local { logical_vertex_id }
            | ExportedEdgeTarget::Remote { logical_vertex_id } => logical_vertex_id,
        };
        let insert_target = match target {
            ExportedEdgeTarget::Local { .. } => resolve_local_endpoint(self, logical)
                .map(InsertEdgeTarget::Local)
                .unwrap_or(InsertEdgeTarget::Logical(logical)),
            ExportedEdgeTarget::Remote { .. } => InsertEdgeTarget::Logical(logical),
        };
        let topology = if undirected {
            InsertEdgeTopology::Undirected
        } else {
            InsertEdgeTopology::Directed
        };
        let value = if value_bytes.is_empty() {
            None
        } else {
            Some(value_bytes)
        };
        self.insert_edge_by_spec(
            owner_vertex_id,
            InsertEdgeSpec {
                topology,
                target: insert_target,
                catalog_label,
                value_bytes: value,
            },
        )
    }
}
