//! Edge layout helpers and delete-target bookkeeping.

use super::edges::HeaderV1 as EdgeHeaderV1;

/// Internal delete location used by compaction and maintenance replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeleteTarget {
    Slab(u32),
    Spill {
        /// The bucket's spill run.
        run: crate::labeled::spill_run_store::SpillRunId,
        /// The row index inside the run.
        index: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertLocation {
    Slab(u32),
    /// The row was written into the bucket's own spill run (ADR 0097 §7). `run` is the run the
    /// producer published, and `logical_slot` is the row's logical slot when the caller asked for
    /// one (scalar capture); a caller that did not ask gets `None`.
    Spill {
        run: crate::labeled::spill_run_store::SpillRunId,
        logical_slot: Option<u32>,
    },
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
