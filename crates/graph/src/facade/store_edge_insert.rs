//! Canonical edge insert paths (directed / undirected, local / logical, valued / unvalued).

use gleaph_graph_kernel::entry::EdgeLabelId;
use gleaph_graph_kernel::federation::GlobalVertexId;
use ic_stable_lara::VertexId;

use super::store::{EdgeHandle, GraphStore, GraphStoreError};

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
    pub inline_value_bytes: Option<&'a [u8]>,
}

impl GraphStore {
    pub(crate) fn insert_edge_by_spec(
        &self,
        source_vertex_id: VertexId,
        spec: InsertEdgeSpec<'_>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let valued = spec.inline_value_bytes.is_some_and(|b| !b.is_empty());
        let value = spec.inline_value_bytes.unwrap_or(&[]);

        match (spec.target, spec.topology, valued) {
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Directed, false) => {
                self.insert_directed_edge(source_vertex_id, target, spec.catalog_label)
            }
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Directed, true) => self
                .insert_directed_edge_with_inline_value_bytes(
                    source_vertex_id,
                    target,
                    spec.catalog_label,
                    value,
                ),
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Undirected, false) => {
                self.insert_undirected_edge(source_vertex_id, target, spec.catalog_label)
            }
            (InsertEdgeTarget::Local(target), InsertEdgeTopology::Undirected, true) => self
                .insert_undirected_edge_with_inline_value_bytes(
                    source_vertex_id,
                    target,
                    spec.catalog_label,
                    value,
                ),
            (InsertEdgeTarget::Remote(_), _, _) => Err(GraphStoreError::RemoteEdgeNotSupported),
        }
    }
}
