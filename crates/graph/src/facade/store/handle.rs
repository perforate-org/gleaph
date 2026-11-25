//! Stable edge handle (owner vertex, wire label, slot index).

use crate::facade::topology::CanonicalEdgeOccurrence;
use gleaph_graph_kernel::entry::{EdgeOrientation, EdgeSlotIndex, TaggedEdgeLabelId, VertexId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeHandle {
    pub owner_vertex_id: VertexId,
    pub label_id: TaggedEdgeLabelId,
    pub slot_index: EdgeSlotIndex,
}

impl EdgeHandle {
    pub(crate) fn at_slot(
        owner_vertex_id: VertexId,
        label_id: TaggedEdgeLabelId,
        slot_index: impl Into<EdgeSlotIndex>,
    ) -> Self {
        Self {
            owner_vertex_id,
            label_id,
            slot_index: slot_index.into(),
        }
    }

    /// Build a LARA [`CanonicalEdgeOccurrence`] from this handle and an orientation.
    pub(crate) fn occurrence(self, orientation: EdgeOrientation) -> CanonicalEdgeOccurrence {
        CanonicalEdgeOccurrence {
            orientation,
            owner_vertex_id: self.owner_vertex_id,
            label_id: self.label_id,
            slot_index: self.slot_index,
        }
    }
}
