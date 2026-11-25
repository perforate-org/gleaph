//! GraphStore `vertex_row` implementation.
use crate::facade::topology::TopologyError;
use gleaph_graph_kernel::entry::Vertex;
use gleaph_graph_kernel::entry::VertexId;

use super::GraphStore;

impl GraphStore {
    pub fn vertex(&self, vertex_id: VertexId) -> Option<Vertex> {
        crate::facade::topology::vertex::row(vertex_id)
    }

    pub fn set_vertex(&self, vertex_id: VertexId, vertex: Vertex) -> Result<(), TopologyError> {
        crate::facade::topology::vertex::set_row(vertex_id, vertex)
    }

    pub(crate) fn set_vertex_rows_bulk(
        &self,
        rows: &[(VertexId, Vertex)],
    ) -> Result<(), TopologyError> {
        let Some((start, _)) = rows.first() else {
            return Ok(());
        };
        for (offset, (vertex_id, _)) in rows.iter().enumerate() {
            assert_eq!(u32::from(*vertex_id), u32::from(*start) + offset as u32);
        }
        crate::facade::topology::vertex::set_rows(*start, rows.iter().map(|(_, vertex)| *vertex))
    }
}
