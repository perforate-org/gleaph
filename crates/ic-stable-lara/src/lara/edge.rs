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
//! [`EdgeStore::asc_out_edges`] materializes the row in **slot order**
//! (ascending slab indices, skipping tombstoned slots,
//! then overflow-log edges oldest-to-newest). Use this when you need the exact CSR
//! / insertion layout.
//!
//! **Default contiguous read contract:** [`EdgeStore::out_edges_iter`] (see [`OutEdgesIter`])
//! walks the overflow log from the chain head first, then live slab slots **high index to low**
//! (still skipping tombstoned slots). Log-backed rows prefetch the log chain at iterator
//! construction so slab scans can skip tombstoned log entries without decoding masked slab slots.
//! Labeled compact rows normally avoid this log-backed
//! path by rewriting rows into slab tombstones before deletion. The descending scan is the preferred
//! hot path (cache- and prefetch-friendly, newest log entries first). Callers that need slot order should use
//! `asc_out_edges` instead, or reverse the vector produced by `out_edges_iter` when
//! packing rows contiguously (e.g. segment rebalance snapshots).
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
//! (`out_edges_iter`, `asc_out_edges`) treats **tombstone + zero
//! degree + no log** (`log_head < 0`) as fully evacuated and returns an empty
//! neighborhood; otherwise enumeration proceeds so incremental `DeleteVertex`
//! maintenance and leaf rebalance can snapshot pending slab/log material until
//! rows clear.

#[cfg(feature = "canbench")]
mod bench;
pub mod counts;
mod edges;
pub mod free_span;
mod log;
#[cfg(test)]
pub mod scan_guard;
pub mod span_meta;

use crate::traits::CsrEdge;
use counts::SegmentEdgeCountsStore;
pub(crate) use edges::EdgeSlabStore;
pub use edges::{HeaderV1 as EdgeHeaderV1, InitError as SlabInitError, segment_tree_leaf_count};
use free_span::FreeSpanStore;
use ic_stable_structures::Memory;
use log::LogStore;
pub use log::{DEFAULT_MAX_LOG_ENTRIES, HeaderV1 as LogHeaderV1};
use span_meta::SegmentSpanMetaStore;
use std::cell::Cell;

pub(super) const INLINE_EDGE_BYTES: usize = 64;

/// When a clean slab row is at least this many bytes, [`OutEdgesIter`] and [`OutEdgeSlabIter`]
/// read the slab in fixed-size **descending** slot chunks instead of one stable read per edge.
pub(super) const OUT_EDGE_SLAB_PREFETCH_MIN_BYTES: usize = 64;
/// Number of consecutive slab slots loaded per chunk when prefetch chunking is enabled.
pub(super) const OUT_EDGE_SLAB_CHUNK_SLOTS: u32 = 32;

mod error;
mod init;
mod insert;
mod log_mut;
mod row_layout;
mod scan;
mod scan_iter;
mod slab;
mod span;
mod targets;
#[cfg(test)]
mod test_support;
mod visit_window;

pub use error::InitError;
pub(crate) use scan_iter::OutEdgeSlabIter;
pub use scan_iter::{AscOutEdgesIter, OutEdgesIter};
pub(crate) use scan_iter::{OutOverflowAscParts, OutOverflowDescParts};
pub(crate) use targets::{DeleteTarget, EdgeLayout, InsertLocation};
pub(crate) use visit_window::OutEdgeVisitWindow;

/// Combined stable edge storage used by [`LaraGraph`](crate::LaraGraph).
pub struct EdgeStore<E: CsrEdge, M: Memory> {
    counts: SegmentEdgeCountsStore<E, M>,
    edges: EdgeSlabStore<E, M>,
    header: Cell<EdgeHeaderV1>,
    log: LogStore<E, M>,
    span_meta: SegmentSpanMetaStore<M>,
    free_spans: FreeSpanStore<M>,
}
