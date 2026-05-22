//! Stable edge handle (owner vertex, wire label, slot index).

use ic_stable_lara::{VertexId, labeled::BucketLabelKey as LaraLabelId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeHandle {
    pub owner_vertex_id: VertexId,
    pub label_id: LaraLabelId,
    pub slot_index: u32,
}

impl EdgeHandle {
    pub(super) fn at_slot(
        owner_vertex_id: VertexId,
        label_id: LaraLabelId,
        slot_index: u32,
    ) -> Self {
        Self {
            owner_vertex_id,
            label_id,
            slot_index,
        }
    }
}
