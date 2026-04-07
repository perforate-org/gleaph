//! DGAP bridge: Gleaph [`VertexEntry`] / [`EdgeEntry`] as [`ic_stable_dgap`] CSR traits (`CsrVertex` / `CsrEdge` / `CsrEdgeUndirected`).
//!
//! [`VertexEntry::log_offset`] holds the packed overflow head used by `graph-pma`; [`CsrVertex::log_head`]
//! / [`CsrVertex::with_log_head`] map that to the DGAP per-leaf log array index (`-1` when empty).
//! [`VertexEntry::segment_id`] is the PMA leaf segment id for the vertex’s base neighborhood start slot.

use std::borrow::Cow;

use ic_stable_dgap::traits::{CsrEdge, CsrEdgeUndirected, CsrVertex};
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;

use super::EdgeIndex;
use super::edge::{EdgeEntry, EdgeMeta};
use super::ids::{EdgeRef, VertexRef};
use super::vertex::{EMPTY_LOG_OFFSET, VertexEntry};

/// Reserved [`MemoryId`](ic_stable_structures::memory_manager::MemoryId) slots when using a dedicated
/// [`ic_stable_structures::memory_manager::MemoryManager`] for [`ic_stable_dgap`]: `M_v`,
/// three `M_e` regions, optional append-only stream log.
///
/// | Slot | Role | [`ic_stable_dgap`] |
/// |------|------|-------------------|
/// | [`DGAP_VERTEX_MEMORY_SLOT`] | Vertex CSR column | `M_v` |
/// | [`DGAP_SEGMENT_EDGES_ACTUAL_MEMORY_SLOT`] | PMA `segment_edges_actual` (`VCA` + array) | `M1` |
/// | [`DGAP_SEGMENT_EDGES_TOTAL_MEMORY_SLOT`] | PMA `segment_edges_total` (`VCT` + array) | `M2` |
/// | [`DGAP_EDGES_AND_LOG_MEMORY_SLOT`] | `VCE` header + CSR slab + log idx + pool | `M3` |
/// | [`DGAP_LOG_MEMORY_SLOT`] | Optional legacy stream (not `DgapEdgeStore`) | `layout::log_region` |
pub const DGAP_VERTEX_MEMORY_SLOT: u8 = 220;
pub const DGAP_SEGMENT_EDGES_ACTUAL_MEMORY_SLOT: u8 = 221;
pub const DGAP_SEGMENT_EDGES_TOTAL_MEMORY_SLOT: u8 = 222;
pub const DGAP_EDGES_AND_LOG_MEMORY_SLOT: u8 = 223;
pub const DGAP_LOG_MEMORY_SLOT: u8 = 224;

/// Builds a tail [`VertexEntry`] for [`ic_stable_dgap::DgapStores::insert_vertex`].
///
/// `new_vid` must be [`ic_stable_dgap::csr::CsrVertexColumn::col_len`] on `M_v` **before** push.
/// `next_base` must be [`ic_stable_dgap::DgapEdgeStore::slab_append_base_slot`] on that column.
/// `segment_size` comes from the `M_e` header; `segment_id` in the packed [`EdgeRef`] is the DGAP
/// leaf index `new_vid / segment_size` (same convention as [`ic_stable_dgap::layout::dgap::dgap_leaf_segment_id`]).
pub fn vertex_entry_for_ic_stable_append(
    new_vid: usize,
    segment_size: u32,
    next_base: u64,
) -> VertexEntry {
    let leaf = (new_vid as u64 / segment_size.max(1) as u64) as u32;
    let er = EdgeRef::new(leaf, next_base);
    VertexEntry::new(EdgeIndex::from(er), 0, EMPTY_LOG_OFFSET)
}

impl Storable for EdgeEntry {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut b = [0u8; 8];
        b[..5].copy_from_slice(&self.target.as_bytes());
        b[5..8].copy_from_slice(&self.meta.to_le_bytes());
        Cow::Owned(b.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let s = bytes.as_ref();
        assert_eq!(s.len(), 8, "EdgeEntry Storable expects 8 bytes");
        let target = VertexRef::new(s[..5].try_into().expect("target bytes"));
        let meta = EdgeMeta::from_le_bytes(s[5..8].try_into().expect("meta bytes"));
        Self { target, meta }
    }

    const BOUND: Bound = Bound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };
}

impl Storable for VertexEntry {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.edge_index.raw.to_le_bytes());
        b[8..12].copy_from_slice(&self.degree.to_le_bytes());
        b[12..16].copy_from_slice(&(self.log_offset as u32).to_le_bytes());
        Cow::Owned(b.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let s = bytes.as_ref();
        assert_eq!(s.len(), 16, "VertexEntry Storable expects 16 bytes");
        Self {
            edge_index: EdgeIndex::new(u64::from_le_bytes(s[0..8].try_into().unwrap())),
            degree: u32::from_le_bytes(s[8..12].try_into().unwrap()),
            log_offset: i32::from_le_bytes(s[12..16].try_into().unwrap()),
        }
    }

    const BOUND: Bound = Bound::Bounded {
        max_size: 16,
        is_fixed_size: true,
    };
}

impl CsrEdge for EdgeEntry {
    const EDGE_BYTES: usize = 8;

    fn read_from(bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), 8);
        Storable::from_bytes(Cow::Borrowed(bytes))
    }

    fn write_to(self, bytes: &mut [u8]) {
        let v = self.to_bytes();
        bytes.copy_from_slice(v.as_ref());
    }

    fn neighbor_vid(&self) -> usize {
        usize::try_from(u64::from(self.target)).expect("vertex id fits usize")
    }

    fn with_neighbor_vid(self, vid: usize) -> Self {
        Self::new(
            VertexRef::try_from(vid as u64).expect("neighbor fits VertexRef"),
            self.meta,
        )
    }
}

impl CsrEdgeUndirected for EdgeEntry {
    fn is_undirected(&self) -> bool {
        self.meta.is_undirected()
    }

    fn with_undirected(self, undirected: bool) -> Self {
        Self {
            target: self.target,
            meta: self.meta.with_undirected(undirected),
        }
    }
}

impl CsrVertex for VertexEntry {
    fn base_slot_start(&self) -> u64 {
        self.start_slot()
    }

    fn degree(&self) -> u32 {
        self.degree
    }

    fn with_base_slot_start(self, start: u64) -> Self {
        let er = EdgeRef::new(self.segment_id(), start);
        Self {
            edge_index: EdgeIndex::from(er),
            degree: self.degree,
            log_offset: self.log_offset,
        }
    }

    fn with_degree(self, degree: u32) -> Self {
        Self { degree, ..self }
    }

    fn log_head(self) -> i32 {
        match self.overflow_head() {
            Some(h) => h as i32,
            None => -1,
        }
    }

    fn with_log_head(self, idx: i32) -> Self {
        if idx < 0 {
            self.with_overflow_head(None)
        } else {
            self.with_overflow_head(Some(idx as u32))
        }
    }
}
