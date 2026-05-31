use crate::lara::edge::EdgeLayout;
use crate::traits::CsrEdgeTombstone;
use crate::*;
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TestEdge(pub(crate) u32);

impl CsrEdge for TestEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self(u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.0)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self(u32::from(vid))
    }
}

impl CsrEdgeTombstone for TestEdge {
    fn tombstone_edge() -> Self {
        Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
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

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.neighbor.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.label.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.neighbor)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self {
            neighbor: u32::from(vid),
            ..*self
        }
    }
}

impl CsrEdgeTombstone for LabelledTestEdge {
    fn tombstone_edge() -> Self {
        Self {
            neighbor: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            label: 0,
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

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.neighbor.to_le_bytes());
        bytes[4] = u8::from(self.undirected);
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.neighbor)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self {
            neighbor: u32::from(vid),
            ..*self
        }
    }
}

impl CsrEdgeTombstone for UndirectedTestEdge {
    fn tombstone_edge() -> Self {
        Self {
            neighbor: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            undirected: false,
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

pub(crate) fn vector_memory() -> VectorMemory {
    Rc::new(RefCell::new(Vec::new()))
}

#[allow(clippy::type_complexity)]
pub(crate) fn labeled_lara_memories() -> (
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
    VectorMemory,
    VectorMemory,
    VectorMemory,
) {
    (
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
    )
}

pub(crate) type TestBidirectionalLaraGraph<E> = BidirectionalLaraGraph<E, Vertex, VectorMemory>;

pub(crate) type TestDeferredBidirectionalLaraGraph<E> =
    crate::DeferredBidirectionalLaraGraph<E, Vertex, VectorMemory>;

pub(crate) fn test_graph(
    elem_capacity: u64,
    segment_size: u32,
    starts: &[u64],
) -> LaraGraph<TestEdge, Vertex, VectorMemory> {
    lara_test_graph(elem_capacity, segment_size, starts)
}

pub(crate) fn lara_test_graph<E>(
    elem_capacity: u64,
    segment_size: u32,
    starts: &[u64],
) -> LaraGraph<E, Vertex, VectorMemory>
where
    E: CsrEdge,
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
        segment_size,
        0,
    )
    .unwrap();
    for &base_slot_start in starts {
        graph
            .push_vertex(Vertex::from_parts(base_slot_start, 0, 0, -1, false))
            .unwrap();
    }
    graph
}

pub(crate) fn bidirectional_test_graph<E>(starts: &[u64]) -> TestBidirectionalLaraGraph<E>
where
    E: CsrEdge,
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
        0,
    )
    .unwrap();
    for &base_slot_start in starts {
        graph
            .push_vertex(Vertex::from_parts(base_slot_start, 0, 0, -1, false))
            .unwrap();
    }
    graph
}

pub(crate) fn deferred_bidirectional_test_graph<E>(
    elem_capacity: u64,
    segment_size: u32,
    starts: &[u64],
) -> TestDeferredBidirectionalLaraGraph<E>
where
    E: CsrEdge + CsrEdgeTombstone,
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
        segment_size,
        0,
        crate::DeferredConfig {
            leaf_dirty_density: 0.0,
            log_urgent_ratio: 0.80,
        },
    )
    .unwrap();
    for &base_slot_start in starts {
        graph
            .push_vertex(Vertex::from_parts(base_slot_start, 0, 0, -1, false))
            .unwrap();
    }
    graph
}

pub(crate) fn assert_vertex_capacity_invariants(graph: &LaraGraph<TestEdge, Vertex, VectorMemory>) {
    use crate::traits::CsrVertex;
    let layout: EdgeLayout = graph.edges().header().into();
    let mut owned_spans = Vec::new();
    for vidx in 0..graph.vertices().len() {
        let v = graph.vertices().get(VertexId::from(vidx));
        let end = graph
            .edges()
            .slab_window_exclusive_end(&layout, graph.vertices(), vidx, &v);
        assert!(
            end >= v.base_slot_start(),
            "vertex {vidx}: csr window end {end} before base {}",
            v.base_slot_start()
        );
        if v.log_head() < 0 {
            assert!(
                v.base_slot_start().saturating_add(u64::from(v.degree())) <= end,
                "vertex {vidx}: live slab prefix extends past csr window end {end}"
            );
        }
        if end > v.base_slot_start() {
            owned_spans.push((vidx, v.base_slot_start(), end));
        }
    }

    for free in graph.edges().free_span_store().spans() {
        let free_start = free.start_slot;
        let free_end = free.start_slot.saturating_add(free.len);
        for &(vidx, owned_start, owned_end) in &owned_spans {
            assert!(
                !spans_overlap(free_start, free_end, owned_start, owned_end),
                "free span [{free_start}, {free_end}) overlaps vertex {vidx} csr window [{owned_start}, {owned_end})"
            );
        }
    }
}

fn spans_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

pub(crate) fn deferred_test_graph(
    elem_capacity: u64,
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
        segment_size,
        0,
    )
    .unwrap();
    for &base_slot_start in starts {
        graph
            .push_vertex(Vertex::from_parts(base_slot_start, 0, 0, -1, false))
            .unwrap();
    }
    graph
}
