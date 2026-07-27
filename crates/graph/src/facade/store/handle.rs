//! Stable edge handle (owner vertex, wire label, slot index).

use ic_stable_lara::{
    VertexId,
    labeled::{BucketLabelKey as LaraLabelId, CanonicalEdgeOccurrence, LabeledOrientation},
    traverse::BucketEntryPosition,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeHandle {
    pub owner_vertex_id: VertexId,
    pub label_id: LaraLabelId,
    pub slot_index: BucketEntryPosition,
}

impl EdgeHandle {
    pub(crate) fn at_slot(
        owner_vertex_id: VertexId,
        label_id: LaraLabelId,
        slot_index: impl Into<BucketEntryPosition>,
    ) -> Self {
        Self {
            owner_vertex_id,
            label_id,
            slot_index: slot_index.into(),
        }
    }

    /// Build a LARA [`CanonicalEdgeOccurrence`] from this handle and an orientation.
    pub(crate) fn occurrence(self, orientation: LabeledOrientation) -> CanonicalEdgeOccurrence {
        CanonicalEdgeOccurrence {
            orientation,
            owner_vertex_id: self.owner_vertex_id,
            label_id: self.label_id,
            slot_index: self.slot_index,
        }
    }
}
