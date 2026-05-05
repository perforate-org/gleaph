#![feature(specialization)]

use derive_more::{Display, From, Into};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use std::{
    borrow::Cow,
    error,
    fmt::{Display, Formatter},
};

pub mod dgap;
mod traits;
mod types;

pub use dgap::edge::{
    EdgeHeaderV1, EdgeStore, InitError as EdgeInitError, LogHeaderV1,
    free_span::{FreeSpan, FreeSpanStore},
    span_meta::{SegmentSpanMeta, SegmentSpanMetaStore},
};
pub use dgap::vertex::{InitError as VertexInitError, Vertex, VertexStore};
pub use dgap::{
    Dgap,
    maintenance::{DeferredConfig, DeferredDgap, MaintenanceBudget, MaintenanceReport},
};
pub use traits::*;

pub type Lara<E, V, MV, MC, ME, ML, MS, MF> = Dgap<E, V, MV, MC, ME, ML, MS, MF>;
pub type DeferredLara<E, V, MV, MC, ME, ML, MS, MF, MMQ, MDS> =
    DeferredDgap<E, V, MV, MC, ME, ML, MS, MF, MMQ, MDS>;

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

/// Copies `count` bytes of data starting from `addr` out of the stable memory into `dst`.
///
/// Callers are allowed to pass vectors in any state (e.g. empty vectors).
/// After the method returns, `dst.len() == count`.
/// This method is an alternative to `read` which does not require initializing a buffer and may
/// therefore be faster.
#[inline]
fn read_to_vec<M: Memory>(m: &M, addr: Address, dst: &mut std::vec::Vec<u8>, count: usize) {
    dst.clear();
    dst.reserve_exact(count);
    unsafe {
        m.read_unsafe(addr.get(), dst.as_mut_ptr(), count);
        // SAFETY: read_unsafe guarantees to initialize the first `count` bytes
        dst.set_len(count);
    }
}

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
    use std::{cell::RefCell, rc::Rc};

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

    fn vector_memory() -> VectorMemory {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn test_graph(
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        starts: &[u64],
    ) -> Dgap<
        TestEdge,
        Vertex,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
    > {
        let graph = Dgap::<TestEdge, Vertex, _, _, _, _, _, _>::new(
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

    fn deferred_test_graph(
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        starts: &[u64],
    ) -> DeferredDgap<
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
        let graph = DeferredDgap::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::new(
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
        use crate::dgap::edge::counts::{SegmentEdgeCounts, SegmentEdgeCountsStore};

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
    fn free_span_store_reopens_and_pops_lifo() {
        let memory = vector_memory();
        let store = FreeSpanStore::new(memory.clone()).unwrap();
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

        let reopened = FreeSpanStore::init(memory).unwrap();
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
    fn dgap_resize_folds_log_edges_back_into_slab() {
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
    fn dgap_insert_rebalances_parent_window_before_resizing() {
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
    fn dgap_parent_rebalance_recomputes_reference_segment_counts() {
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
        assert_eq!(graph.edges().free_span_store().get(0).start_slot, 0);
        assert!(graph.edges().free_span_store().get(0).len > 0);
        assert_eq!(graph.vertices().get(0).base_slot_start, 4);
        assert_eq!(graph.vertices().get(0).degree, 4);
        assert_eq!(graph.vertices().get(0).log_head, -1);
        assert_eq!(graph.edges().counts_store().get(1).actual, 4);
        assert_eq!(graph.edges().counts_store().get(1).total, 7);
        assert_eq!(graph.edges().counts_store().get(2).actual, 4);
        assert_eq!(graph.edges().counts_store().get(2).total, 6);
    }

    #[test]
    fn dgap_reopen_preserves_rebalanced_layout_and_counts() {
        let graph = test_graph(8, 2, 2, &[0, 2, 4, 6]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        let memories = graph.into_memories();
        let reopened = Dgap::<TestEdge, Vertex, _, _, _, _, _, _>::init(
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5,
        )
        .unwrap();

        assert_eq!(reopened.edges().header().elem_capacity, 8);
        assert_eq!(reopened.edges().span_meta_store().len(), 2);
        assert_eq!(reopened.vertices().get(0).degree, 4);
        assert_eq!(reopened.vertices().get(0).log_head, -1);
        assert_eq!(
            reopened.collect_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert_eq!(reopened.edges().counts_store().get(2).total, 6);
    }

    #[test]
    fn maintenance_queue_deduplicates_and_prioritizes_urgent_segments() {
        let mq =
            dgap::maintenance::MaintenanceQueue::new(vector_memory(), vector_memory()).unwrap();

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
            dgap::maintenance::MaintenanceQueue::new(vector_memory(), vector_memory()).unwrap();
        mq.mark_dirty(SegmentId::from(1)).unwrap();
        mq.mark_dirty(SegmentId::from(3)).unwrap();

        let memories = mq.into_memories();
        let reopened = dgap::maintenance::MaintenanceQueue::init(memories.0, memories.1).unwrap();

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
    fn deferred_dgap_reopens_maintenance_state() {
        let graph = deferred_test_graph(8, 2, 2, &[0, 2, 4, 6]);
        for dst in 10..13 {
            graph
                .insert_edge_deferred(VertexId::from(0), TestEdge(dst))
                .unwrap();
        }

        let memories = graph.into_memories();
        let reopened = DeferredDgap::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::init(
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
        let graph = DeferredDgap::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::new_with_config(
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
        let err = DeferredDgap::<TestEdge, Vertex, _, _, _, _, _, _, _, _>::new_with_config(
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
        )
        .unwrap_err();

        assert!(matches!(
            err,
            dgap::maintenance::DeferredError::InvalidConfig(_)
        ));
    }
}
