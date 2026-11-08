//! Edge storage for LARA.
//!
//! The edge subsystem combines five stable-memory structures:
//!
//! - segment edge counts, used by update/maintenance code to decide when a
//!   segment is dense;
//! - the contiguous edge slab containing clean adjacency prefixes;
//! - per-segment overflow logs for inserts that cannot fit immediately on the
//!   slab;
//! - segment span metadata for locally relocated physical spans;
//! - free span metadata for retired physical ranges.
//!
//! [`EdgeStore::collect_out_edges_slot_order`] materializes the row in **slot order**
//! (ascending slab indices, skipping tombstoned slots,
//! then overflow-log edges oldest-to-newest). Use this when you need the exact CSR
//! / insertion layout.
//!
//! **Default contiguous read contract:** [`EdgeStore::out_edges_iter`] (see [`OutEdgesIter`])
//! walks live slab slots **low index to high**, then reserved overflow-log entries replayed
//! oldest-to-newest (still skipping tombstoned slots). This is the stable materialization order
//! and the default for all unsuffixed traversal APIs. The explicit descending path
//! ([`EdgeStore::desc_out_edges_iter`], see [`DescOutEdgesIter`]) walks the overflow log from the
//! chain head first, then live slab slots **high index to low**; it is the cache- and
//! prefetch-friendly hot path for callers that opt into newest-first ordering.
//! Labeled compact rows normally avoid the log-backed path by rewriting rows into slab tombstones
//! before deletion.
//! The log, counts, span metadata, and free span index are update-side structures.
//! They may be read while inserting, folding logs, resizing, or relocating, but they
//! are not part of the clean scan contract.
//!
//! Insertions first try to append at `base_slot_start + stored_degree`, or reuse
//! the first tail tombstone at `base_slot_start + degree` when `stored_degree > degree`.
//! Appending is allowed only when it stays before this row’s CSR slab boundary (the next
//! vertex's `base_slot_start`, PMA leaf total, or `elem_capacity`);
//! otherwise the edge is written to the segment log and later folded by
//! maintenance or relocation.
//!
//! ## Layout assumptions (update paths)
//!
//! Slab span geometry uses the successor vertex row’s `base_slot_start` inside a
//! PMA leaf, plus (when slabs are monotone across leaves) caps from later
//! leaves. A materialized segment also clamps the slab window using
//! `span_meta.physical_start + counts.total`. When monotone ordering breaks due
//! to local relocation packing a leaf into earlier slab slots, successors with
//! lower bases are ignored and PMA span metadata determines the slab tail instead.
//! If that invariant is violated, behavior is undefined; **debug builds** assert it on
//! the hot paths below. Prefer [`crate::LaraGraph`] orchestration over ad-hoc
//! [`EdgeStore`] mutation so geometry and PMA counts stay aligned.
//!
//! ## Vertex tombstones and read paths
//!
//! When [`crate::traits::CsrVertexTombstoneScan::record_is_vertex_tombstone`]
//! is true, mutating APIs still reject the row. Read-only enumeration
//! (`out_edges_iter`, `collect_out_edges_slot_order`) treats **tombstone + zero
//! degree + no log** (`log_head < 0`) as fully evacuated and returns an empty
//! neighborhood; otherwise enumeration proceeds so incremental `DeleteVertex`
//! maintenance and leaf rebalance can snapshot pending slab/log material until
//! rows clear.

#[cfg(feature = "canbench")]
mod bench;
pub mod counts;
mod edges;
pub mod free_span;
#[cfg(test)]
pub mod scan_guard;
pub mod span_meta;

use crate::labeled::graph::leaf_cover::EdgeSlot;
use crate::labeled::spill_run_store::SpillRunStore;
use crate::traits::CsrEdge;
use counts::SegmentEdgeCountsStore;
pub(crate) use edges::{BackedSlabImage, EdgeSlabStore, PhysicalSlabPayload};
pub use edges::{HeaderV1 as EdgeHeaderV1, InitError as SlabInitError, segment_tree_leaf_count};
use free_span::FreeSpanStore;
use ic_stable_structures::Memory;
use span_meta::SegmentSpanMetaStore;
use std::cell::Cell;

pub(crate) const INLINE_EDGE_BYTES: usize = 64;

/// When a clean slab row is at least this many bytes, [`OutEdgesIter`] and [`OutEdgeSlabIter`]
/// read the slab in fixed-size **descending** slot chunks instead of one stable read per edge.
/// The ascending default iterator (`[OutEdgesIter]`) uses forward (ascending) chunks.
pub(super) const OUT_EDGE_SLAB_PREFETCH_MIN_BYTES: usize = 64;
/// Number of consecutive slab slots loaded per chunk when prefetch chunking is enabled.
pub(super) const OUT_EDGE_SLAB_CHUNK_SLOTS: u32 = 32;

mod error;
mod init;
mod insert;
mod iter;
mod log_mut;
mod row_layout;
mod scan;
mod slab;
mod span;
#[cfg(test)]
mod spill_read_test;
mod targets;
#[cfg(test)]
mod test_support;
mod visit_window;

pub use error::InitError;
pub use iter::{DescOutEdgesIter, OutEdgesIter};
pub(crate) use scan::RowVisibility;
pub(crate) use targets::{DeleteTarget, EdgeLayout, InsertLocation};
pub(crate) use visit_window::OutEdgeVisitWindow;

/// Combined stable edge storage used by [`LaraGraph`](crate::LaraGraph).
pub struct EdgeStore<E: CsrEdge, M: Memory> {
    counts: SegmentEdgeCountsStore<E, M>,
    edges: EdgeSlabStore<E, M>,
    header: Cell<EdgeHeaderV1>,
    /// Per-bucket spill runs for rows that do not fit their slab prefix (ADR 0097). This region
    /// used to hold the shared per-leaf overflow log; the region slot is unchanged.
    spill: SpillRunStore<M>,
    span_meta: SegmentSpanMetaStore<M>,
    free_spans: FreeSpanStore<M>,
}

pub(crate) struct BackedPhysicalEdgeImage<'a, E: CsrEdge, M: Memory> {
    owner: &'a EdgeStore<E, M>,
    image: BackedSlabImage<'a, E, M>,
    header: EdgeHeaderV1,
}

impl<E: CsrEdge, M: Memory> BackedPhysicalEdgeImage<'_, E, M> {
    pub(crate) fn validate_source(&self) -> bool {
        self.image.validate_source()
    }
    #[cfg(test)]
    pub(crate) fn copy_buffer_metrics(&self) -> (usize, usize) {
        self.image.copy_buffer_metrics()
    }

    pub(crate) fn write(&self) {
        self.image.write();
        self.owner.header.set(self.header);
    }
}

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    pub(crate) fn physical_header_matches(&self, header: EdgeHeaderV1) -> bool {
        self.header() == header && self.edges.header().ok() == Some(header)
    }

    pub(crate) fn prepare_physical_copy_image(
        &self,
        header: EdgeHeaderV1,
        payloads: Vec<PhysicalSlabPayload>,
    ) -> Result<BackedSlabImage<'_, E, M>, crate::lara::operation_error::LaraOperationError> {
        self.edges
            .prepare_physical_copy_image(header, payloads)
            .map_err(crate::lara::operation_error::LaraOperationError::ResizeFailed)
    }

    pub(crate) fn prepare_physical_image<'a>(
        &'a self,
        header: EdgeHeaderV1,
        images: Vec<BackedSlabImage<'a, E, M>>,
    ) -> Result<BackedPhysicalEdgeImage<'a, E, M>, crate::lara::operation_error::LaraOperationError>
    {
        let image = self
            .edges
            .combine_physical_images(header, images)
            .map_err(crate::lara::operation_error::LaraOperationError::ResizeFailed)?;
        Ok(BackedPhysicalEdgeImage {
            owner: self,
            image,
            header,
        })
    }
    /// Returns edge-slab allocator statistics (free span accounting).
    pub fn allocator_stats(&self) -> crate::lara::edge::free_span::FreeSpanAllocatorStats {
        self.free_spans.allocator_stats()
    }

    /// Returns the current edge free spans for testing and diagnostics.
    #[cfg(test)]
    pub(crate) fn free_byte_spans(&self) -> Vec<crate::lara::edge::free_span::FreeSpan> {
        self.free_spans.spans()
    }

    /// Reads the raw bytes of one edge slab slot into `out`. The byte
    /// length of `out` must equal `E::BYTES`. Used by Plan 0318 §Step 4
    /// tree-mode promotion to transcribe the slab prefix into LTB
    /// blocks, and by Step 5 tree-mode read dispatch.
    pub(crate) fn read_slot_bytes(&self, slot: EdgeSlot, out: &mut [u8]) {
        self.edges.read_slot(slot.get(), out);
    }

    /// Reads `count * E::BYTES` contiguous bytes starting at
    /// `start_slot` into `out`. Used by Plan 0318 §Step 4 to verify
    /// the LEG root region holds the correct block_id sequence, and by
    /// Step 5 to read the root region in tree-mode accessors.
    pub(crate) fn read_slots_contiguous_bytes(&self, start_slot: u64, out: &mut [u8]) {
        self.edges.read_slots_contiguous(start_slot, out);
    }

    /// Reads `out.len()` bytes starting `byte_offset` bytes past `slot`.
    ///
    /// Tree-mode property-root ids are packed at 4 bytes regardless of `E::BYTES`
    /// (ADR 0088 §2), so a packed id can straddle an edge-record boundary. The
    /// address stays slot-anchored: callers pass a within-span byte offset.
    pub(crate) fn read_from_slot_bytes(&self, slot: u64, byte_offset: u64, out: &mut [u8]) {
        self.edges.read_from_slot_bytes(slot, byte_offset, out);
    }

    /// Writes `src` starting `byte_offset` bytes past `slot`.
    /// See [`Self::read_from_slot_bytes`].
    pub(crate) fn write_from_slot_bytes(
        &self,
        slot: u64,
        byte_offset: u64,
        src: &[u8],
    ) -> Result<(), crate::GrowFailed> {
        self.edges.write_from_slot_bytes(slot, byte_offset, src)
    }

    /// Writes `count * E::BYTES` contiguous bytes from `src` starting at
    /// `start_slot`. `src.len()` must be a multiple of `E::BYTES`. Used
    /// by Plan 0318 §Step 4 to write the block_id array into the LEG
    /// root region.
    pub(crate) fn write_slots_contiguous_bytes(
        &self,
        start_slot: u64,
        src: &[u8],
    ) -> Result<(), crate::GrowFailed> {
        self.edges.write_slots_contiguous(start_slot, src)
    }
}
