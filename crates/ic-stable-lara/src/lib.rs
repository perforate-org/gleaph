//! Stable-memory implementation of LARA, the Localized Adjacency Relocation
//! Array.
//!
//! LARA stores adjacency lists in a CSR-style slab while allowing local
//! relocation of dense physical spans. The key design boundary is that clean
//! scans remain direct:
//!
//! ```text
//! vertex_id -> vertex row -> edge slots [base_slot_start, base_slot_start + degree)
//! ```
//!
//! A clean scan is authoritative only over `base_slot_start` and `degree`.
//! It must not consult vertex `capacity`, segment span metadata, or the free
//! span manager. Update and maintenance paths may use all three vertex fields:
//! `base_slot_start`, `degree`, and `capacity`.
//!
//! `capacity` is the number of slab slots owned by a vertex. The live prefix
//! `[base_slot_start, base_slot_start + degree)` must stay contained in the
//! owned span `[base_slot_start, base_slot_start + capacity)`. Relocation
//! rewrites bases and capacities together, publishes segment span metadata, and
//! releases retired physical spans only after the query-visible state has been
//! committed.
//!
//! The main external reference for the dynamic adjacency idea is
//! [DGAP](https://github.com/DIR-LAB/DGAP), but this crate owns a separate
//! persisted layout and public API centered on LARA's explicit capacity and
//! local relocation contracts.

#![allow(incomplete_features)]
#![feature(specialization)]

use derive_more::{Display, From, Into};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use std::{
    borrow::Cow,
    error,
    fmt::{Display, Formatter},
};

pub mod bidirectional;
pub mod lara;
mod traits;
mod types;

pub use bidirectional::{BidirectionalLara, BidirectionalLaraError, BidirectionalLaraGraph};
pub use lara::{
    LaraGraph,
    edge::{
        EdgeHeaderV1, EdgeStore, InitError as EdgeInitError, LogHeaderV1,
        free_span::{FreeSpan, FreeSpanKey, FreeSpanStore},
        free_span_array::FreeSpanArrayStore,
        free_span_dual_index::{
            FreeSpanDualIndexError, FreeSpanDualIndexStore, LenStartKey, SpanLen, StartKey,
        },
        span_meta::{SegmentSpanMeta, SegmentSpanMetaStore},
    },
    maintenance::{DeferredConfig, DeferredLaraGraph, MaintenanceBudget, MaintenanceReport},
    vertex::{InitError as VertexInitError, Vertex, VertexStore},
};
pub use traits::*;

pub type Lara<E, V, MV, MC, ME, ML, MS, MF> = LaraGraph<E, V, MV, MC, ME, ML, MS, MF>;
pub type DeferredLara<E, V, MV, MC, ME, ML, MS, MF, MMQ, MDS> =
    DeferredLaraGraph<E, V, MV, MC, ME, ML, MS, MF, MMQ, MDS>;

pub use ic_stable_structures::vec_mem::VectorMemory;
use types::Address;

#[repr(transparent)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Display, From, Into,
)]
pub struct VertexId(u32);

#[repr(transparent)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Display, From, Into,
)]
pub struct SegmentId(u32);

impl From<SegmentId> for usize {
    fn from(value: SegmentId) -> Self {
        value.0 as usize
    }
}

impl Storable for SegmentId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 4,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.0.to_le_bytes().to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&bytes.as_ref()[0..4]);
        Self(u32::from_le_bytes(buf))
    }
}

#[repr(transparent)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Display, From, Into,
)]
pub struct VertexCount(u64);

const WASM_PAGE_SIZE: u64 = 65536;

/// A helper function that reads a single 32bit integer encoded as
/// little-endian from the specified memory at the specified offset.
fn read_u32<M: Memory>(m: &M, addr: Address) -> u32 {
    let mut buf: [u8; 4] = [0; 4];
    m.read(addr.get(), &mut buf);
    u32::from_le_bytes(buf)
}

/// A helper function that reads a single 64bit integer encoded as
/// little-endian from the specified memory at the specified offset.
fn read_u64<M: Memory>(m: &M, addr: Address) -> u64 {
    let mut buf: [u8; 8] = [0; 8];
    m.read(addr.get(), &mut buf);
    u64::from_le_bytes(buf)
}

fn read_i32<M: Memory>(m: &M, addr: Address) -> i32 {
    let mut buf: [u8; 4] = [0; 4];
    m.read(addr.get(), &mut buf);
    i32::from_le_bytes(buf)
}

/// Writes a single 32-bit integer encoded as little-endian.
fn write_u32<M: Memory>(m: &M, addr: Address, val: u32) {
    write(m, addr.get(), &val.to_le_bytes());
}

fn write_i32<M: Memory>(m: &M, addr: Address, val: i32) {
    write(m, addr.get(), &val.to_le_bytes());
}

/// Writes a single 64-bit integer encoded as little-endian.
fn write_u64<M: Memory>(m: &M, addr: Address, val: u64) {
    write(m, addr.get(), &val.to_le_bytes());
}

#[derive(Debug, PartialEq, Eq)]
pub struct GrowFailed {
    current_size: u64,
    delta: u64,
}

impl Display for GrowFailed {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to grow memory: current size={}, delta={}",
            self.current_size, self.delta
        )
    }
}

impl error::Error for GrowFailed {}

/// Writes the bytes at the specified offset, growing the memory size if needed.
fn safe_write<M: Memory>(memory: &M, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
    let last_byte = offset
        .checked_add(bytes.len() as u64)
        .expect("Address space overflow");

    let size_pages = memory.size();
    let size_bytes = size_pages
        .checked_mul(WASM_PAGE_SIZE)
        .expect("Address space overflow");

    if size_bytes < last_byte {
        let diff_bytes = last_byte - size_bytes;
        let diff_pages = diff_bytes
            .checked_add(WASM_PAGE_SIZE - 1)
            .expect("Address space overflow")
            / WASM_PAGE_SIZE;
        if memory.grow(diff_pages) == -1 {
            return Err(GrowFailed {
                current_size: size_pages,
                delta: diff_pages,
            });
        }
    }
    memory.write(offset, bytes);
    Ok(())
}

/// Like [safe_write], but panics if the memory.grow fails.
fn write<M: Memory>(memory: &M, offset: u64, bytes: &[u8]) {
    if let Err(GrowFailed {
        current_size,
        delta,
    }) = safe_write(memory, offset, bytes)
    {
        panic!(
            "Failed to grow memory from {} pages to {} pages (delta = {} pages).",
            current_size,
            current_size + delta,
            delta
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        rc::Rc,
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestEdge(u32);

    impl CsrEdge for TestEdge {
        const BYTES: usize = 4;

        fn read_from(bytes: &[u8]) -> Self {
            Self(u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
        }

        fn write_to(self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.0)
        }

        fn with_neighbor_vid(self, vid: VertexId) -> Self {
            Self(u32::from(vid))
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct UndirectedTestEdge {
        neighbor: u32,
        undirected: bool,
    }

    impl UndirectedTestEdge {
        fn new(neighbor: u32) -> Self {
            Self {
                neighbor,
                undirected: false,
            }
        }
    }

    impl CsrEdge for UndirectedTestEdge {
        const BYTES: usize = 5;

        fn read_from(bytes: &[u8]) -> Self {
            Self {
                neighbor: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
                undirected: bytes[4] != 0,
            }
        }

        fn write_to(self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.neighbor.to_le_bytes());
            bytes[4] = u8::from(self.undirected);
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.neighbor)
        }

        fn with_neighbor_vid(self, vid: VertexId) -> Self {
            Self {
                neighbor: u32::from(vid),
                ..self
            }
        }
    }

    impl CsrEdgeUndirected for UndirectedTestEdge {
        fn is_undirected(&self) -> bool {
            self.undirected
        }

        fn with_undirected(self, undirected: bool) -> Self {
            Self { undirected, ..self }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TombstoneEdge {
        neighbor: u32,
        tombstone: bool,
    }

    impl CsrEdge for TombstoneEdge {
        const BYTES: usize = 5;

        fn read_from(bytes: &[u8]) -> Self {
            Self {
                neighbor: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
                tombstone: bytes[4] != 0,
            }
        }

        fn write_to(self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.neighbor.to_le_bytes());
            bytes[4] = u8::from(self.tombstone);
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.neighbor)
        }

        fn with_neighbor_vid(self, vid: VertexId) -> Self {
            Self {
                neighbor: u32::from(vid),
                ..self
            }
        }
    }

    impl CsrEdgeTombstone for TombstoneEdge {
        fn is_tombstone(&self) -> bool {
            self.tombstone
        }

        fn with_tombstone(self, tombstone: bool) -> Self {
            Self { tombstone, ..self }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct PoisonCapacityVertex {
        base_slot_start: u64,
        degree: u32,
        log_head: i32,
    }

    impl CsrVertex for PoisonCapacityVertex {
        const BYTES: usize = 16;

        fn base_slot_start(&self) -> u64 {
            self.base_slot_start
        }

        fn degree(&self) -> u32 {
            self.degree
        }

        fn with_base_slot_start(mut self, start: u64) -> Self {
            self.base_slot_start = start;
            self
        }

        fn with_degree(mut self, degree: u32) -> Self {
            self.degree = degree;
            self
        }

        fn log_head(self) -> i32 {
            self.log_head
        }

        fn with_log_head(mut self, idx: i32) -> Self {
            self.log_head = idx;
            self
        }
    }

    impl LaraVertex for PoisonCapacityVertex {
        fn span_capacity(&self) -> u32 {
            panic!("clean scan must not read vertex capacity")
        }

        fn with_span_capacity(self, _capacity: u32) -> Self {
            panic!("clean scan must not write vertex capacity")
        }
    }

    impl Storable for PoisonCapacityVertex {
        const BOUND: Bound = Bound::Bounded {
            max_size: Self::BYTES as u32,
            is_fixed_size: true,
        };

        fn to_bytes(&self) -> Cow<'_, [u8]> {
            let mut b = [0u8; Self::BYTES];
            b[0..8].copy_from_slice(&self.base_slot_start.to_le_bytes());
            b[8..12].copy_from_slice(&self.degree.to_le_bytes());
            b[12..16].copy_from_slice(&self.log_head.to_le_bytes());
            Cow::Owned(b.to_vec())
        }

        fn into_bytes(self) -> Vec<u8> {
            self.to_bytes().into_owned()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            let mut base = [0u8; 8];
            let mut degree = [0u8; 4];
            let mut log_head = [0u8; 4];
            base.copy_from_slice(&bytes.as_ref()[0..8]);
            degree.copy_from_slice(&bytes.as_ref()[8..12]);
            log_head.copy_from_slice(&bytes.as_ref()[12..16]);
            Self {
                base_slot_start: u64::from_le_bytes(base),
                degree: u32::from_le_bytes(degree),
                log_head: i32::from_le_bytes(log_head),
            }
        }
    }

    static CAPACITY_READS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct CountingCapacityVertex {
        base_slot_start: u64,
        degree: u32,
        capacity: u32,
        log_head: i32,
    }

    impl CsrVertex for CountingCapacityVertex {
        const BYTES: usize = 20;

        fn base_slot_start(&self) -> u64 {
            self.base_slot_start
        }

        fn degree(&self) -> u32 {
            self.degree
        }

        fn with_base_slot_start(mut self, start: u64) -> Self {
            self.base_slot_start = start;
            self
        }

        fn with_degree(mut self, degree: u32) -> Self {
            self.degree = degree;
            self
        }

        fn log_head(self) -> i32 {
            self.log_head
        }

        fn with_log_head(mut self, idx: i32) -> Self {
            self.log_head = idx;
            self
        }
    }

    impl LaraVertex for CountingCapacityVertex {
        fn span_capacity(&self) -> u32 {
            CAPACITY_READS.fetch_add(1, Ordering::SeqCst);
            self.capacity
        }

        fn with_span_capacity(mut self, capacity: u32) -> Self {
            self.capacity = capacity;
            self
        }
    }

    impl Storable for CountingCapacityVertex {
        const BOUND: Bound = Bound::Bounded {
            max_size: Self::BYTES as u32,
            is_fixed_size: true,
        };

        fn to_bytes(&self) -> Cow<'_, [u8]> {
            let mut b = [0u8; Self::BYTES];
            b[0..8].copy_from_slice(&self.base_slot_start.to_le_bytes());
            b[8..12].copy_from_slice(&self.degree.to_le_bytes());
            b[12..16].copy_from_slice(&self.capacity.to_le_bytes());
            b[16..20].copy_from_slice(&self.log_head.to_le_bytes());
            Cow::Owned(b.to_vec())
        }

        fn into_bytes(self) -> Vec<u8> {
            self.to_bytes().into_owned()
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            let mut base = [0u8; 8];
            let mut degree = [0u8; 4];
            let mut capacity = [0u8; 4];
            let mut log_head = [0u8; 4];
            base.copy_from_slice(&bytes.as_ref()[0..8]);
            degree.copy_from_slice(&bytes.as_ref()[8..12]);
            capacity.copy_from_slice(&bytes.as_ref()[12..16]);
            log_head.copy_from_slice(&bytes.as_ref()[16..20]);
            Self {
                base_slot_start: u64::from_le_bytes(base),
                degree: u32::from_le_bytes(degree),
                capacity: u32::from_le_bytes(capacity),
                log_head: i32::from_le_bytes(log_head),
            }
        }
    }

    fn vector_memory() -> VectorMemory {
        Rc::new(RefCell::new(Vec::new()))
    }

    type TestBidirectionalLaraGraph<E> = BidirectionalLaraGraph<
        E,
        Vertex,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
    >;

    fn test_graph(
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        starts: &[u64],
    ) -> LaraGraph<
        TestEdge,
        Vertex,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
    > {
        let graph = LaraGraph::<TestEdge, Vertex, _, _, _, _, _, _>::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            elem_capacity,
            segment_count,
            segment_size,
        )
        .unwrap();
        for &base_slot_start in starts {
            graph
                .push_vertex(Vertex {
                    base_slot_start,
                    degree: 0,
                    capacity: 0,
                    log_head: -1,
                })
                .unwrap();
        }
        graph
    }

    fn bidirectional_test_graph<E>(starts: &[u64]) -> TestBidirectionalLaraGraph<E>
    where
        E: CsrEdge + lara::edge::counts::EdgePmaCountsStride,
    {
        let graph = BidirectionalLaraGraph::<E, Vertex, _, _, _, _, _, _, _, _, _, _, _, _>::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            32,
            4,
            4,
        )
        .unwrap();
        for &base_slot_start in starts {
            graph
                .push_vertex(Vertex {
                    base_slot_start,
                    degree: 0,
                    capacity: 0,
                    log_head: -1,
                })
                .unwrap();
        }
        graph
    }

    fn assert_vertex_capacity_invariants(
        graph: &LaraGraph<
            TestEdge,
            Vertex,
            VectorMemory,
            VectorMemory,
            VectorMemory,
            VectorMemory,
            VectorMemory,
            VectorMemory,
        >,
    ) {
        let mut owned_spans = Vec::new();
        for vidx in 0..graph.vertices().len() {
            let v = graph.vertices().get(vidx);
            assert!(
                v.degree <= v.capacity,
                "vertex {vidx} has degree {} beyond capacity {}",
                v.degree,
                v.capacity
            );
            assert!(
                v.base_slot_start.saturating_add(u64::from(v.degree))
                    <= v.base_slot_start.saturating_add(u64::from(v.capacity)),
                "vertex {vidx} live prefix exceeds owned span"
            );
            if v.capacity > 0 {
                owned_spans.push((
                    vidx,
                    v.base_slot_start,
                    v.base_slot_start.saturating_add(u64::from(v.capacity)),
                ));
            }
        }

        for free in graph.edges().free_span_store().spans() {
            let free_start = free.start_slot;
            let free_end = free.start_slot.saturating_add(free.len);
            for &(vidx, owned_start, owned_end) in &owned_spans {
                assert!(
                    !spans_overlap(free_start, free_end, owned_start, owned_end),
                    "free span [{free_start}, {free_end}) overlaps vertex {vidx} owned span [{owned_start}, {owned_end})"
                );
            }
        }
    }

    fn spans_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
        a_start < b_end && b_start < a_end
    }

    fn deferred_test_graph(
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        starts: &[u64],
    ) -> DeferredLaraGraph<
        TestEdge,
        Vertex,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
    > {
        let graph = DeferredLaraGraph::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            elem_capacity,
            segment_count,
            segment_size,
        )
        .unwrap();
        for &base_slot_start in starts {
            graph
                .push_vertex(Vertex {
                    base_slot_start,
                    degree: 0,
                    capacity: 0,
                    log_head: -1,
                })
                .unwrap();
        }
        graph
    }

    #[test]
    fn segment_edge_counts_stride_depends_on_edge_tombstone_capability() {
        use crate::lara::edge::counts::{SegmentEdgeCounts, SegmentEdgeCountsStore};

        let plain = SegmentEdgeCountsStore::<TestEdge, _>::new(vector_memory()).unwrap();
        let tombstone = SegmentEdgeCountsStore::<TombstoneEdge, _>::new(vector_memory()).unwrap();

        assert_eq!(
            SegmentEdgeCountsStore::<TestEdge, VectorMemory>::entry_size(),
            16
        );
        assert_eq!(
            SegmentEdgeCountsStore::<TombstoneEdge, VectorMemory>::entry_size(),
            24
        );

        let counts = SegmentEdgeCounts {
            actual: 1,
            total: 2,
            tombstone: 3,
        };
        plain.push(counts).unwrap();
        tombstone.push(counts).unwrap();

        assert_eq!(
            plain.get(0),
            SegmentEdgeCounts {
                tombstone: 0,
                ..counts
            }
        );
        assert_eq!(tombstone.get(0), counts);
    }

    #[test]
    fn bidirectional_directed_insert_updates_forward_and_reverse() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4, 8]);

        graph
            .insert_directed(VertexId::from(0), VertexId::from(2), TestEdge(2))
            .unwrap();

        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(2)]
        );
        assert_eq!(graph.out_edges(VertexId::from(2)).unwrap(), Vec::new());
        assert_eq!(
            graph.in_edges(VertexId::from(2)).unwrap(),
            vec![TestEdge(0)]
        );
        assert_eq!(graph.in_edges(VertexId::from(0)).unwrap(), Vec::new());
    }

    #[test]
    fn bidirectional_directed_insert_rejects_neighbor_mismatch() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4]);

        let err = graph
            .insert_directed(VertexId::from(0), VertexId::from(1), TestEdge(0))
            .unwrap_err();

        assert!(matches!(
            err,
            BidirectionalLaraError::NeighborMismatch {
                expected,
                actual
            } if expected == VertexId::from(1) && actual == VertexId::from(0)
        ));
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap(), Vec::new());
        assert_eq!(graph.in_edges(VertexId::from(1)).unwrap(), Vec::new());
    }

    #[test]
    fn bidirectional_directed_insert_rejects_undirected_edge() {
        let graph = bidirectional_test_graph::<UndirectedTestEdge>(&[0, 4]);
        let edge = UndirectedTestEdge::new(1).with_undirected(true);

        let err = graph
            .insert_directed(VertexId::from(0), VertexId::from(1), edge)
            .unwrap_err();

        assert!(matches!(
            err,
            BidirectionalLaraError::UndirectedEdgeInDirectedInsert
        ));
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap(), Vec::new());
        assert_eq!(graph.in_edges(VertexId::from(1)).unwrap(), Vec::new());
    }

    #[test]
    fn bidirectional_undirected_insert_materializes_symmetric_adjacency() {
        let graph = bidirectional_test_graph::<UndirectedTestEdge>(&[0, 4, 8]);

        graph
            .insert_undirected(
                VertexId::from(0),
                VertexId::from(2),
                UndirectedTestEdge::new(2),
            )
            .unwrap();

        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 2,
                undirected: true
            }]
        );
        assert_eq!(
            graph.out_edges(VertexId::from(2)).unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 0,
                undirected: true
            }]
        );
        assert_eq!(
            graph.in_edges(VertexId::from(0)).unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 2,
                undirected: true
            }]
        );
        assert_eq!(
            graph.in_edges(VertexId::from(2)).unwrap(),
            vec![UndirectedTestEdge {
                neighbor: 0,
                undirected: true
            }]
        );
    }

    #[test]
    fn bidirectional_undirected_self_loop_stores_one_loop_per_orientation() {
        let graph = bidirectional_test_graph::<UndirectedTestEdge>(&[0, 4]);

        graph
            .insert_undirected(
                VertexId::from(1),
                VertexId::from(1),
                UndirectedTestEdge::new(1),
            )
            .unwrap();

        let loop_edge = UndirectedTestEdge {
            neighbor: 1,
            undirected: true,
        };
        assert_eq!(graph.out_edges(VertexId::from(1)).unwrap(), vec![loop_edge]);
        assert_eq!(graph.in_edges(VertexId::from(1)).unwrap(), vec![loop_edge]);
    }

    #[test]
    fn bidirectional_reopen_preserves_forward_and_reverse_stores() {
        let graph = bidirectional_test_graph::<TestEdge>(&[0, 4, 8]);
        graph
            .insert_directed(VertexId::from(0), VertexId::from(2), TestEdge(2))
            .unwrap();

        let (fv, fc, fe, fl, fs, ff, rv, rc, re, rl, rs, rf) = graph.into_memories();
        let reopened =
            BidirectionalLaraGraph::<TestEdge, Vertex, _, _, _, _, _, _, _, _, _, _, _, _>::init(
                fv, fc, fe, fl, fs, ff, rv, rc, re, rl, rs, rf,
            )
            .unwrap();

        assert_eq!(
            reopened.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(2)]
        );
        assert_eq!(
            reopened.in_edges(VertexId::from(2)).unwrap(),
            vec![TestEdge(0)]
        );
    }

    #[test]
    fn segment_span_meta_store_reopens_physical_starts() {
        let memory = vector_memory();
        let store = SegmentSpanMetaStore::new(memory.clone()).unwrap();
        store.push(SegmentSpanMeta { physical_start: 12 }).unwrap();
        store.push(SegmentSpanMeta { physical_start: 48 }).unwrap();

        let reopened = SegmentSpanMetaStore::init(memory).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.get(0), SegmentSpanMeta { physical_start: 12 });
        assert_eq!(reopened.get(1), SegmentSpanMeta { physical_start: 48 });
    }

    #[test]
    fn free_span_array_store_take_best_fit_prefers_smallest_len() {
        let memory = vector_memory();
        let store = FreeSpanArrayStore::new(memory).unwrap();
        store
            .push(FreeSpan {
                start_slot: 0,
                len: 100,
            })
            .unwrap();
        store
            .push(FreeSpan {
                start_slot: 1000,
                len: 50,
            })
            .unwrap();
        store
            .push(FreeSpan {
                start_slot: 2000,
                len: 80,
            })
            .unwrap();
        let got = store.take_best_fit(45).unwrap().unwrap();
        assert_eq!(
            got,
            FreeSpan {
                start_slot: 1000,
                len: 50
            }
        );
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn free_span_array_store_release_coalescing_linear_merges_neighbors() {
        let memory = vector_memory();
        let store = FreeSpanArrayStore::new(memory).unwrap();
        store
            .push(FreeSpan {
                start_slot: 100,
                len: 20,
            })
            .unwrap();
        store
            .push(FreeSpan {
                start_slot: 140,
                len: 10,
            })
            .unwrap();

        store
            .release_coalescing_linear(FreeSpan {
                start_slot: 120,
                len: 20,
            })
            .unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get(0),
            FreeSpan {
                start_slot: 100,
                len: 50,
            }
        );
    }

    #[test]
    fn free_span_array_store_reopens_and_pops_lifo() {
        let memory = vector_memory();
        let store = FreeSpanArrayStore::new(memory.clone()).unwrap();
        store
            .push(FreeSpan {
                start_slot: 16,
                len: 4,
            })
            .unwrap();
        store
            .push(FreeSpan {
                start_slot: 64,
                len: 12,
            })
            .unwrap();

        let reopened = FreeSpanArrayStore::init(memory).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(
            reopened.pop(),
            Some(FreeSpan {
                start_slot: 64,
                len: 12,
            })
        );
        assert_eq!(
            reopened.pop(),
            Some(FreeSpan {
                start_slot: 16,
                len: 4,
            })
        );
        assert_eq!(reopened.pop(), None);
    }

    #[test]
    fn free_span_dual_index_release_coalesces_neighbors() {
        let mut store = FreeSpanDualIndexStore::init(vector_memory(), vector_memory());

        store.release_span(100, 20).unwrap();
        store.release_span(140, 10).unwrap();
        store.release_span(120, 20).unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get_by_start(100),
            Some(FreeSpan {
                start_slot: 100,
                len: 50
            })
        );
    }

    #[test]
    fn free_span_dual_index_take_best_fit_splits_remainder() {
        let mut store = FreeSpanDualIndexStore::init(vector_memory(), vector_memory());
        store.release_span(1000, 80).unwrap();
        store.release_span(2000, 32).unwrap();
        store.release_span(3000, 128).unwrap();

        assert_eq!(
            store.take_best_fit(40).unwrap(),
            Some(FreeSpan {
                start_slot: 1000,
                len: 40
            })
        );
        assert_eq!(
            store.get_by_start(1040),
            Some(FreeSpan {
                start_slot: 1040,
                len: 40
            })
        );
        assert_eq!(store.get_by_start(1000), None);
    }

    #[test]
    fn free_span_store_take_best_fit_splits_remainder() {
        let store = FreeSpanStore::init(vector_memory());
        store.release_span(1000, 80);
        store.release_span(2000, 32);
        store.release_span(3000, 128);

        assert_eq!(
            store.take_best_fit(40),
            Some(FreeSpan {
                start_slot: 1000,
                len: 40
            })
        );
        assert_eq!(
            store.take_best_fit_whole(40),
            Some(FreeSpan {
                start_slot: 1040,
                len: 40
            })
        );
    }

    #[test]
    fn free_span_dual_index_rejects_overlap() {
        let mut store = FreeSpanDualIndexStore::init(vector_memory(), vector_memory());
        store.release_span(100, 20).unwrap();

        let err = store.release_span(110, 20).unwrap_err();
        assert!(matches!(
            err,
            FreeSpanDualIndexError::OverlapPrevious { .. }
                | FreeSpanDualIndexError::OverlapNext { .. }
        ));
    }

    #[test]
    fn free_span_dual_index_rejects_duplicate_without_mutation() {
        let mut store = FreeSpanDualIndexStore::init(vector_memory(), vector_memory());
        store.release_span(100, 20).unwrap();

        let err = store.release_span(100, 8).unwrap_err();
        assert!(matches!(
            err,
            FreeSpanDualIndexError::DuplicateStart { start_slot: 100 }
        ));
        assert_eq!(
            store.get_by_start(100),
            Some(FreeSpan {
                start_slot: 100,
                len: 20
            })
        );
    }

    #[test]
    fn free_span_dual_index_reopens_both_indexes() {
        let by_len = vector_memory();
        let by_start = vector_memory();
        let mut store = FreeSpanDualIndexStore::init(by_len.clone(), by_start.clone());
        store.release_span(64, 8).unwrap();
        store.release_span(128, 32).unwrap();
        drop(store);

        let mut reopened = FreeSpanDualIndexStore::init(by_len, by_start);
        assert_eq!(
            reopened.take_best_fit(16).unwrap(),
            Some(FreeSpan {
                start_slot: 128,
                len: 16
            })
        );
        assert_eq!(
            reopened.get_by_start(144),
            Some(FreeSpan {
                start_slot: 144,
                len: 16
            })
        );
    }

    #[test]
    fn free_span_dual_index_inserts_by_len_in_release_builds() {
        let mut store = FreeSpanDualIndexStore::init(vector_memory(), vector_memory());
        store.release_span(4096, 128).unwrap();

        assert_eq!(
            store.take_best_fit_whole(96).unwrap(),
            Some(FreeSpan {
                start_slot: 4096,
                len: 128
            })
        );
        assert!(store.is_empty());
    }

    #[test]
    fn edge_store_reads_slab_then_log_neighborhood() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 0,
                degree: 0,
                capacity: 1,
                log_head: -1,
            })
            .unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 1,
                degree: 0,
                capacity: 1,
                log_head: -1,
            })
            .unwrap();

        let edges = EdgeStore::<TestEdge, _, _, _, _, _>::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            2,
            1,
            2,
        )
        .unwrap();
        assert_eq!(edges.span_meta_store().len(), 1);

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert_eq!(
            edges
                .collect_out_edges(&vertices, VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
        assert_eq!(vertices.get(0).degree, 2);
        assert!(vertices.get(0).log_head >= 0);
    }

    #[test]
    fn edge_store_uses_vertex_capacity_for_slab_space() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 0,
                degree: 0,
                capacity: 2,
                log_head: -1,
            })
            .unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 1,
                degree: 0,
                capacity: 1,
                log_head: -1,
            })
            .unwrap();

        let edges = EdgeStore::<TestEdge, _, _, _, _, _>::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            4,
            1,
            2,
        )
        .unwrap();

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert_eq!(vertices.get(0).degree, 2);
        assert_eq!(vertices.get(0).log_head, -1);
        assert_eq!(
            edges
                .collect_out_edges(&vertices, VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
    }

    #[test]
    fn edge_store_insert_reads_capacity_for_update_boundary() {
        CAPACITY_READS.store(0, Ordering::SeqCst);

        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<CountingCapacityVertex, _>::new(mv).unwrap();
        vertices
            .push(CountingCapacityVertex {
                base_slot_start: 0,
                degree: 0,
                capacity: 2,
                log_head: -1,
            })
            .unwrap();
        vertices
            .push(CountingCapacityVertex {
                base_slot_start: 1,
                degree: 0,
                capacity: 1,
                log_head: -1,
            })
            .unwrap();

        let edges = EdgeStore::<TestEdge, _, _, _, _, _>::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            4,
            1,
            2,
        )
        .unwrap();

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert!(CAPACITY_READS.load(Ordering::SeqCst) >= 2);
        assert_eq!(vertices.get(0).degree, 2);
        assert_eq!(vertices.get(0).log_head, -1);
        assert_eq!(
            edges
                .collect_out_edges(&vertices, VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
    }

    #[test]
    fn edge_store_scan_uses_base_and_degree_not_capacity() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<PoisonCapacityVertex, _>::new(mv).unwrap();
        vertices
            .push(PoisonCapacityVertex {
                base_slot_start: 0,
                degree: 2,
                log_head: -1,
            })
            .unwrap();

        let edges = EdgeStore::<TestEdge, _, _, _, _, _>::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            2,
            1,
            1,
        )
        .unwrap();
        edges.write_slot(0, TestEdge(10)).unwrap();
        edges.write_slot(1, TestEdge(11)).unwrap();

        assert_eq!(
            edges
                .collect_out_edges(&vertices, VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
    }

    #[test]
    fn lara_resize_folds_log_edges_back_into_slab() {
        let graph = test_graph(2, 1, 2, &[0, 1]);

        graph.insert_edge(VertexId::from(0), TestEdge(10)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(11)).unwrap();

        graph.resize().unwrap();

        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
        assert_eq!(graph.vertices().get(0).degree, 2);
        assert_eq!(graph.vertices().get(0).log_head, -1);
        assert!(graph.edges().header().elem_capacity >= 4);
    }

    #[test]
    fn lara_insert_rebalances_parent_window_before_resizing() {
        let graph = test_graph(8, 2, 2, &[0, 2, 4, 6]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        assert_eq!(graph.edges().header().elem_capacity, 8);
        assert_eq!(graph.vertices().get(0).log_head, -1);
        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert!(
            graph.vertices().get(1).base_slot_start > graph.vertices().get(0).base_slot_start + 3
        );
    }

    #[test]
    fn lara_parent_rebalance_recomputes_reference_segment_counts() {
        let graph = test_graph(8, 2, 2, &[0, 2, 4, 6]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        let counts = graph.edges().counts_store();
        assert_eq!(counts.get(1).actual, 4);
        assert_eq!(counts.get(1).total, 8);
        assert_eq!(counts.get(2).actual, 4);
        assert_eq!(counts.get(2).total, 6);
        assert_eq!(counts.get(3).actual, 0);
        assert_eq!(counts.get(3).total, 2);
    }

    #[test]
    fn lara_root_saturation_relocates_hot_segment_to_tail() {
        let graph = test_graph(4, 2, 1, &[0, 2]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        assert_eq!(graph.edges().header().elem_capacity, 10);
        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert_eq!(graph.edges().span_meta_store().get(0).physical_start, 4);
        assert_eq!(graph.edges().free_span_store().len(), 1);
        let released = graph.edges().free_span_store().peek_best_fit(1).unwrap();
        assert_eq!(released.start_slot, 0);
        assert!(released.len > 0);
        assert_eq!(graph.vertices().get(0).base_slot_start, 4);
        assert_eq!(graph.vertices().get(0).degree, 4);
        assert!(graph.vertices().get(0).capacity >= graph.vertices().get(0).degree);
        assert_eq!(graph.vertices().get(0).log_head, -1);
        assert_eq!(graph.edges().counts_store().get(1).actual, 4);
        assert_eq!(graph.edges().counts_store().get(1).total, 7);
        assert_eq!(graph.edges().counts_store().get(2).actual, 4);
        assert_eq!(graph.edges().counts_store().get(2).total, 6);
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_local_relocation_reuses_prior_free_span() {
        let graph = test_graph(12, 2, 1, &[0, 10]);

        for dst in 10..20 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }
        assert_eq!(graph.vertices().get(0).base_slot_start, 12);
        assert_eq!(
            graph
                .edges()
                .free_span_store()
                .peek_best_fit(10)
                .unwrap()
                .start_slot,
            0
        );

        for dst in 20..25 {
            graph.insert_edge(VertexId::from(1), TestEdge(dst)).unwrap();
        }

        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![
                TestEdge(10),
                TestEdge(11),
                TestEdge(12),
                TestEdge(13),
                TestEdge(14),
                TestEdge(15),
                TestEdge(16),
                TestEdge(17),
                TestEdge(18),
                TestEdge(19)
            ]
        );
        assert_eq!(
            graph.collect_out_edges(VertexId::from(1)).unwrap(),
            vec![
                TestEdge(20),
                TestEdge(21),
                TestEdge(22),
                TestEdge(23),
                TestEdge(24)
            ]
        );
        assert_eq!(graph.vertices().get(1).base_slot_start, 0);
        assert_eq!(graph.edges().span_meta_store().get(0).physical_start, 12);
        assert_eq!(graph.edges().span_meta_store().get(1).physical_start, 0);
        let root = graph.edges().counts_store().get(1);
        let left = graph.edges().counts_store().get(2);
        let right = graph.edges().counts_store().get(3);
        assert_eq!(root.actual, left.actual + right.actual);
        assert_eq!(root.total, left.total + right.total);
        assert_eq!(left.actual, 10);
        assert_eq!(right.actual, 5);
        assert!(left.total >= left.actual);
        assert!(right.total >= right.actual);
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_local_relocation_metadata_survives_reopen() {
        let graph = test_graph(4, 2, 1, &[0, 2]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        let memories = graph.into_memories();
        let reopened = LaraGraph::<TestEdge, Vertex, _, _, _, _, _, _>::init(
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5,
        )
        .unwrap();

        assert_eq!(reopened.edges().span_meta_store().get(0).physical_start, 4);
        assert_eq!(reopened.edges().free_span_store().len(), 1);
        let released = reopened.edges().free_span_store().peek_best_fit(1).unwrap();
        assert_eq!(released.start_slot, 0);
        assert!(released.len > 0);
        assert_eq!(
            reopened.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert_eq!(reopened.edges().counts_store().get(2).total, 6);
        assert_vertex_capacity_invariants(&reopened);
    }

    #[test]
    fn lara_reopen_preserves_rebalanced_layout_and_counts() {
        let graph = test_graph(8, 2, 2, &[0, 2, 4, 6]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        let memories = graph.into_memories();
        let reopened = LaraGraph::<TestEdge, Vertex, _, _, _, _, _, _>::init(
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5,
        )
        .unwrap();

        assert_eq!(reopened.edges().header().elem_capacity, 8);
        assert_eq!(reopened.edges().span_meta_store().len(), 2);
        assert_eq!(reopened.vertices().get(0).degree, 4);
        assert!(reopened.vertices().get(0).capacity >= reopened.vertices().get(0).degree);
        assert_eq!(reopened.vertices().get(0).log_head, -1);
        assert_eq!(
            reopened.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert_eq!(reopened.edges().counts_store().get(2).total, 6);
        assert_vertex_capacity_invariants(&reopened);
    }

    #[test]
    fn maintenance_queue_deduplicates_and_prioritizes_urgent_segments() {
        let mq =
            lara::maintenance::MaintenanceQueue::new(vector_memory(), vector_memory()).unwrap();

        assert!(mq.mark_dirty(SegmentId::from(2)).unwrap().inserted);
        assert!(!mq.mark_dirty(SegmentId::from(2)).unwrap().inserted);
        assert!(mq.mark_urgent(SegmentId::from(7)).unwrap().inserted);

        assert_eq!(mq.len(), 2);
        assert_eq!(mq.pop_next().unwrap(), Some(SegmentId::from(7)));
        assert_eq!(mq.pop_next().unwrap(), Some(SegmentId::from(2)));
        assert_eq!(mq.pop_next().unwrap(), None);
    }

    #[test]
    fn maintenance_queue_reopens_dirty_membership_and_order() {
        let mq =
            lara::maintenance::MaintenanceQueue::new(vector_memory(), vector_memory()).unwrap();
        mq.mark_dirty(SegmentId::from(1)).unwrap();
        mq.mark_dirty(SegmentId::from(3)).unwrap();

        let memories = mq.into_memories();
        let reopened = lara::maintenance::MaintenanceQueue::init(memories.0, memories.1).unwrap();

        assert!(reopened.is_dirty(SegmentId::from(1)));
        assert!(reopened.is_dirty(SegmentId::from(3)));
        assert_eq!(reopened.pop_next().unwrap(), Some(SegmentId::from(1)));
        assert!(!reopened.is_dirty(SegmentId::from(1)));
        assert_eq!(reopened.pop_next().unwrap(), Some(SegmentId::from(3)));
        assert_eq!(reopened.pop_next().unwrap(), None);
    }

    #[test]
    fn deferred_insert_keeps_reads_correct_until_maintenance_folds_log() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);

        for dst in 10..13 {
            graph
                .insert_edge_deferred(VertexId::from(0), TestEdge(dst))
                .unwrap();
        }

        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12)]
        );
        assert!(graph.graph().vertices().get(0).log_head >= 0);
        assert!(graph.maintenance_queue().is_dirty(SegmentId::from(0)));

        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
            })
            .unwrap();

        assert_eq!(report.processed_segments, 1);
        assert_eq!(report.rebalanced_segments, 1);
        assert_eq!(graph.graph().vertices().get(0).log_head, -1);
        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12)]
        );
    }

    #[test]
    fn deferred_maintenance_segment_cap_leaves_unprocessed_segments_queued() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);

        for dst in 10..13 {
            graph
                .insert_edge_deferred(VertexId::from(0), TestEdge(dst))
                .unwrap();
        }
        graph
            .maintenance_queue()
            .mark_dirty(SegmentId::from(1))
            .unwrap();

        let report = graph
            .maintenance(MaintenanceBudget {
                max_instructions: 0,
                max_segments: Some(1),
            })
            .unwrap();

        assert_eq!(report.processed_segments, 1);
        assert_eq!(report.rebalanced_segments, 1);
        assert!(!graph.maintenance_queue().is_dirty(SegmentId::from(0)));
        assert!(graph.maintenance_queue().is_dirty(SegmentId::from(1)));
        assert_eq!(graph.maintenance_queue().len(), 1);
    }

    #[test]
    fn deferred_lara_graph_reopens_maintenance_state() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);
        for dst in 10..13 {
            graph
                .insert_edge_deferred(VertexId::from(0), TestEdge(dst))
                .unwrap();
        }

        let memories = graph.into_memories();
        let reopened = DeferredLaraGraph::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::init(
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5, memories.6,
            memories.7,
        )
        .unwrap();

        assert!(reopened.maintenance_queue().is_dirty(SegmentId::from(0)));
        assert_eq!(
            reopened.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12)]
        );
    }

    #[test]
    fn deferred_insert_skips_dirty_when_slab_insert_is_below_soft_threshold() {
        let graph = deferred_test_graph(16, 2, 4, &[0, 4, 8, 12]);

        graph
            .insert_edge_deferred(VertexId::from(0), TestEdge(10))
            .unwrap();

        assert!(!graph.maintenance_queue().is_dirty(SegmentId::from(0)));
        assert_eq!(graph.maintenance_queue().len(), 0);
        assert_eq!(
            graph.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10)]
        );
    }

    #[test]
    fn deferred_config_controls_dirty_threshold() {
        let graph = DeferredLaraGraph::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::new_with_config(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            16,
            2,
            4,
            DeferredConfig {
                leaf_dirty_density: 0.05,
                log_urgent_ratio: 0.80,
            },
        )
        .unwrap();
        for slot in [0, 4, 8, 12] {
            graph
                .push_vertex(Vertex {
                    base_slot_start: slot,
                    degree: 0,
                    capacity: 0,
                    log_head: -1,
                })
                .unwrap();
        }

        graph
            .insert_edge_deferred(VertexId::from(0), TestEdge(10))
            .unwrap();

        assert_eq!(graph.config().leaf_dirty_density, 0.05);
        assert!(graph.maintenance_queue().is_dirty(SegmentId::from(0)));
    }

    #[test]
    fn deferred_config_rejects_invalid_thresholds() {
        let err =
            match DeferredLaraGraph::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::new_with_config(
                vector_memory(),
                vector_memory(),
                vector_memory(),
                vector_memory(),
                vector_memory(),
                vector_memory(),
                vector_memory(),
                vector_memory(),
                16,
                2,
                4,
                DeferredConfig {
                    leaf_dirty_density: f64::NAN,
                    log_urgent_ratio: 0.80,
                },
            ) {
                Ok(_) => panic!("invalid deferred config was accepted"),
                Err(err) => err,
            };

        assert!(matches!(
            err,
            lara::maintenance::DeferredError::InvalidConfig(_)
        ));
    }
}
