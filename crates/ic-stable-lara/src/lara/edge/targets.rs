//! Edge layout helpers and delete-target encoding.

use super::edges::HeaderV1 as EdgeHeaderV1;
use crate::lara::operation_error::LaraOperationError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeleteTarget {
    Slab(u32),
    Log(u32),
}

pub(crate) fn encode_delete_target(target: DeleteTarget) -> Result<i32, LaraOperationError> {
    let tag = match target {
        DeleteTarget::Slab(offset) => offset
            .checked_mul(2)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?,
        DeleteTarget::Log(index) => index
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?,
    };
    let encoded = -1i64 - i64::from(tag);
    i32::try_from(encoded).map_err(|_| LaraOperationError::CollectAllocationOverflow)
}

pub(crate) fn decode_delete_target(src: i32) -> Option<DeleteTarget> {
    if src >= 0 {
        return None;
    }
    let tag = (-1i64 - i64::from(src)) as u32;
    if tag % 2 == 0 {
        Some(DeleteTarget::Slab(tag / 2))
    } else {
        Some(DeleteTarget::Log(tag / 2))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertLocation {
    Slab(u32),
    Log,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EdgeLayout {
    pub elem_capacity: u64,
    pub segment_count: u32,
    pub segment_size: u32,
    pub num_edges: u64,
    pub initial_vertex_edge_slots: u32,
}

impl From<EdgeHeaderV1> for EdgeLayout {
    fn from(header: EdgeHeaderV1) -> Self {
        Self {
            elem_capacity: header.elem_capacity,
            segment_count: header.segment_count,
            segment_size: header.segment_size,
            num_edges: header.num_edges,
            initial_vertex_edge_slots: header.initial_vertex_edge_slots,
        }
    }
}

impl InsertLocation {
    pub(crate) fn inserted_into_log(self) -> bool {
        matches!(self, Self::Log)
    }
}
