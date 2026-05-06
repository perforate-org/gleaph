use crate::{
    BidirectionalLaraGraph, DeferredBidirectionalLaraGraph, DeferredConfig, DeferredLaraGraph,
    LaraGraph, VertexId,
    lara::vertex::Vertex,
    test_support::{TestEdge, UndirectedTestEdge},
};
use ic_stable_structures::{
    DefaultMemoryImpl,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
};

pub(crate) const SMALL_N: u64 = 256;
pub(crate) const MEDIUM_N: u64 = 1024;
pub(crate) const LARGE_N: u64 = 4096;

pub(crate) type BenchMemory = VirtualMemory<DefaultMemoryImpl>;

pub(crate) struct BenchMemoryFactory {
    manager: MemoryManager<DefaultMemoryImpl>,
    next_id: u8,
}

impl BenchMemoryFactory {
    pub(crate) fn new() -> Self {
        Self {
            manager: MemoryManager::init(DefaultMemoryImpl::default()),
            next_id: 0,
        }
    }

    pub(crate) fn memory(&mut self) -> BenchMemory {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("benchmark memory id overflow");
        self.manager.get(MemoryId::new(id))
    }
}

#[inline]
pub(crate) fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[inline]
pub(crate) fn test_edge(seed: u64) -> TestEdge {
    TestEdge((splitmix64(seed) as u32) & 0x00ff_ffff)
}

#[inline]
pub(crate) fn vertex(base_slot_start: u64, capacity: u32) -> Vertex {
    Vertex {
        base_slot_start,
        degree: 0,
        capacity,
        log_head: -1,
    }
}

pub(crate) fn lara_graph(
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
    vertex_count: u32,
) -> LaraGraph<TestEdge, Vertex, BenchMemory> {
    let mut memories = BenchMemoryFactory::new();
    let graph = LaraGraph::new(
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        elem_capacity,
        segment_count,
        segment_size,
    )
    .expect("lara graph");
    for vid in 0..vertex_count {
        graph
            .push_vertex(vertex(u64::from(vid) * u64::from(segment_size), 0))
            .expect("push vertex");
    }
    graph
}

pub(crate) fn populated_lara_graph(
    vertex_count: u32,
    edges_per_vertex: u32,
) -> LaraGraph<TestEdge, Vertex, BenchMemory> {
    let capacity = u64::from(vertex_count)
        .saturating_mul(u64::from(edges_per_vertex).saturating_add(4))
        .max(16);
    let segment_size = 16;
    let segment_count = vertex_count.div_ceil(segment_size).max(1);
    let graph = lara_graph(capacity, segment_count, segment_size, vertex_count);
    for src in 0..vertex_count {
        for i in 0..edges_per_vertex {
            graph
                .insert_edge(
                    VertexId::from(src),
                    TestEdge(src.wrapping_add(i).wrapping_add(1)),
                )
                .expect("insert edge");
        }
    }
    graph
}

pub(crate) fn deferred_graph(
    vertex_count: u32,
) -> DeferredLaraGraph<TestEdge, Vertex, BenchMemory> {
    let segment_size = 16;
    let segment_count = vertex_count.div_ceil(segment_size).max(1);
    let mut memories = BenchMemoryFactory::new();
    let graph = DeferredLaraGraph::new_with_config(
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        u64::from(vertex_count).saturating_mul(4).max(16),
        segment_count,
        segment_size,
        DeferredConfig {
            leaf_dirty_density: 0.0,
            log_urgent_ratio: 0.80,
        },
    )
    .expect("deferred graph");
    for vid in 0..vertex_count {
        graph
            .push_vertex(vertex(u64::from(vid) * u64::from(segment_size), 0))
            .expect("push vertex");
    }
    graph
}

pub(crate) fn bidirectional_graph<E>(
    vertex_count: u32,
) -> BidirectionalLaraGraph<E, Vertex, BenchMemory>
where
    E: crate::traits::CsrEdge + crate::lara::edge::counts::EdgePmaCountsStride,
{
    let mut memories = BenchMemoryFactory::new();
    let graph = BidirectionalLaraGraph::new(
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        u64::from(vertex_count).saturating_mul(8).max(16),
        vertex_count.div_ceil(16).max(1),
        16,
    )
    .expect("bidirectional graph");
    for vid in 0..vertex_count {
        graph
            .push_vertex(vertex(u64::from(vid) * 16, 0))
            .expect("push vertex");
    }
    graph
}

pub(crate) fn deferred_bidirectional_graph(
    vertex_count: u32,
) -> DeferredBidirectionalLaraGraph<TestEdge, Vertex, BenchMemory> {
    let mut memories = BenchMemoryFactory::new();
    let graph = DeferredBidirectionalLaraGraph::new_with_config(
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        u64::from(vertex_count).saturating_mul(4).max(16),
        vertex_count.div_ceil(16).max(1),
        16,
        DeferredConfig {
            leaf_dirty_density: 0.0,
            log_urgent_ratio: 0.80,
        },
    )
    .expect("deferred bidirectional graph");
    for vid in 0..vertex_count {
        graph
            .push_vertex(vertex(u64::from(vid) * 16, 0))
            .expect("push vertex");
    }
    graph
}

#[inline]
pub(crate) fn undirected_edge(dst: u32) -> UndirectedTestEdge {
    UndirectedTestEdge::new(dst)
}
