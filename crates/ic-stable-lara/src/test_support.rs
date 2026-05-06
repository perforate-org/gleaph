use crate::*;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TestEdge(pub(crate) u32);

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
pub(crate) struct LabelledTestEdge {
    pub(crate) neighbor: u32,
    pub(crate) label: u32,
}

impl LabelledTestEdge {
    pub(crate) fn new(neighbor: u32, label: u32) -> Self {
        Self { neighbor, label }
    }
}

impl CsrEdge for LabelledTestEdge {
    const BYTES: usize = 8;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            neighbor: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            label: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        }
    }

    fn write_to(self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.neighbor.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.label.to_le_bytes());
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UndirectedTestEdge {
    pub(crate) neighbor: u32,
    pub(crate) undirected: bool,
}

impl UndirectedTestEdge {
    pub(crate) fn new(neighbor: u32) -> Self {
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
pub(crate) struct TombstoneEdge {
    pub(crate) neighbor: u32,
    pub(crate) tombstone: bool,
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
pub(crate) struct PoisonCapacityVertex {
    pub(crate) base_slot_start: u64,
    pub(crate) degree: u32,
    pub(crate) log_head: i32,
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

pub(crate) static CAPACITY_READS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CountingCapacityVertex {
    pub(crate) base_slot_start: u64,
    pub(crate) degree: u32,
    pub(crate) capacity: u32,
    pub(crate) log_head: i32,
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

pub(crate) fn vector_memory() -> VectorMemory {
    Rc::new(RefCell::new(Vec::new()))
}

pub(crate) type TestBidirectionalLaraGraph<E> = BidirectionalLaraGraph<E, Vertex, VectorMemory>;

pub(crate) type TestDeferredBidirectionalLaraGraph<E> =
    crate::DeferredBidirectionalLaraGraph<E, Vertex, VectorMemory>;

pub(crate) fn test_graph(
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
    starts: &[u64],
) -> LaraGraph<TestEdge, Vertex, VectorMemory> {
    lara_test_graph(elem_capacity, segment_count, segment_size, starts)
}

pub(crate) fn lara_test_graph<E>(
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
    starts: &[u64],
) -> LaraGraph<E, Vertex, VectorMemory>
where
    E: CsrEdge + lara::edge::counts::EdgePmaCountsStride,
{
    let graph = LaraGraph::new(
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
                deleted: false,
            })
            .unwrap();
    }
    graph
}

pub(crate) fn bidirectional_test_graph<E>(starts: &[u64]) -> TestBidirectionalLaraGraph<E>
where
    E: CsrEdge + lara::edge::counts::EdgePmaCountsStride,
{
    let graph = BidirectionalLaraGraph::new(
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
                deleted: false,
            })
            .unwrap();
    }
    graph
}

pub(crate) fn deferred_bidirectional_test_graph<E>(
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
    starts: &[u64],
) -> TestDeferredBidirectionalLaraGraph<E>
where
    E: CsrEdge + lara::edge::counts::EdgePmaCountsStride,
{
    let graph = crate::DeferredBidirectionalLaraGraph::new_with_config(
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
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        elem_capacity,
        segment_count,
        segment_size,
        crate::DeferredConfig {
            leaf_dirty_density: 0.0,
            log_urgent_ratio: 0.80,
        },
    )
    .unwrap();
    for &base_slot_start in starts {
        graph
            .push_vertex(Vertex {
                base_slot_start,
                degree: 0,
                capacity: 0,
                log_head: -1,
                deleted: false,
            })
            .unwrap();
    }
    graph
}

pub(crate) fn assert_vertex_capacity_invariants(graph: &LaraGraph<TestEdge, Vertex, VectorMemory>) {
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

pub(crate) fn deferred_test_graph(
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
    starts: &[u64],
) -> DeferredLaraGraph<TestEdge, Vertex, VectorMemory> {
    let graph = DeferredLaraGraph::new(
        vector_memory(),
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
                deleted: false,
            })
            .unwrap();
    }
    graph
}
