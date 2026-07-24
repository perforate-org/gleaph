use crate::{
    BidirectionalLaraGraph, DeferredBidirectionalLaraGraph, DeferredConfig, DeferredLaraGraph,
    LaraGraph, VertexId,
    lara::edge::segment_tree_leaf_count,
    lara::vertex::Vertex,
    test_support::{TestEdge, UndirectedTestEdge},
    traits::CsrEdge,
};
#[cfg(test)]
use ic_stable_structures::Memory;
use ic_stable_structures::{
    DefaultMemoryImpl,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
};

pub(crate) const SMALL_N: u64 = 256;
pub(crate) const MEDIUM_N: u64 = 1024;
pub(crate) const LARGE_N: u64 = 4096;

pub(crate) type BenchMemory = VirtualMemory<DefaultMemoryImpl>;
/// Highest usable `MemoryId`; `u8::MAX` is reserved internally by `MemoryManager`.
pub(crate) const MEASUREMENT_MEMORY_ID_MAX: u8 = u8::MAX - 1;

#[allow(
    dead_code,
    reason = "ScanOnly and Published are consumed by the Plan 0147 fixture adapter"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MeasurementRepresentation {
    AliasOnly,
    ScanOnly,
    Published,
}

/// Owns one isolated measurement memory bundle. Each candidate gets a fresh manager and allocates
/// IDs from the high end so benchmark regions cannot overlap the production low-ID layout.
pub(crate) struct MeasurementMemoryBundle {
    manager: MemoryManager<DefaultMemoryImpl>,
    next_id: u8,
    allocated_ids: Vec<u8>,
    #[allow(
        dead_code,
        reason = "candidate tag is consumed by the Plan 0147 fixture adapter"
    )]
    representation: MeasurementRepresentation,
}

impl MeasurementMemoryBundle {
    pub(crate) fn new() -> Self {
        Self::with_representation(MeasurementRepresentation::AliasOnly)
    }

    pub(crate) fn with_representation(representation: MeasurementRepresentation) -> Self {
        Self {
            manager: MemoryManager::init(DefaultMemoryImpl::default()),
            // Bench-only regions are allocated from the top of the u8 MemoryId space so future
            // production layouts can continue allocating from the low end without collisions.
            next_id: MEASUREMENT_MEMORY_ID_MAX,
            allocated_ids: Vec::new(),
            representation,
        }
    }

    pub(crate) fn memory(&mut self) -> BenchMemory {
        let id = self.next_id;
        self.allocated_ids.push(id);
        self.next_id = self
            .next_id
            .checked_sub(1)
            .expect("benchmark memory id overflow");
        self.manager.get(MemoryId::new(id))
    }

    #[allow(
        dead_code,
        reason = "candidate tag is consumed by the Plan 0147 fixture adapter"
    )]
    pub(crate) const fn representation(&self) -> MeasurementRepresentation {
        self.representation
    }
}

pub(crate) type BenchMemoryFactory = MeasurementMemoryBundle;

#[allow(dead_code, reason = "consumed by the Plan 0146 Graph evidence adapter")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub(crate) struct MeasurementPhysicalIdentity {
    pub(crate) owner: u32,
    pub(crate) target: u32,
    pub(crate) orientation: u8,
    pub(crate) slot: u32,
}

#[allow(dead_code, reason = "consumed by the Plan 0146 Graph evidence adapter")]
pub(crate) struct AliasOnlyMeasurementFixture {
    pub(crate) graph: BidirectionalLaraGraph<TestEdge, Vertex, BenchMemory>,
    pub(crate) identities: Vec<MeasurementPhysicalIdentity>,
}

/// Builds an alias-only candidate from real bidirectional LARA storage and returns its physical
/// identity rows. Other representations are intentionally not accepted until their owning stores
/// are wired into the fixture boundary.
#[allow(dead_code, reason = "consumed by the Plan 0146 Graph evidence adapter")]
pub(crate) fn build_alias_only_measurement_fixture(
    vertex_count: u32,
    directed_edges: &[(u32, u32)],
) -> Result<AliasOnlyMeasurementFixture, String> {
    let graph = bidirectional_graph::<TestEdge>(vertex_count);
    for &(source, target) in directed_edges {
        if source >= vertex_count || target >= vertex_count {
            return Err("fixture edge endpoint is out of range".to_owned());
        }
        graph
            .insert_directed(
                VertexId::from(source),
                VertexId::from(target),
                TestEdge(target),
            )
            .map_err(|error| format!("fixture edge insert failed: {error}"))?;
    }

    let mut identities = Vec::with_capacity(directed_edges.len().saturating_mul(2));
    for owner in 0..vertex_count {
        for edge in graph
            .directed_out_edges_iter(VertexId::from(owner), crate::OutEdgeOrder::Ascending)
            .map_err(|error| format!("forward identity scan failed: {error}"))?
        {
            identities.push(MeasurementPhysicalIdentity {
                owner,
                target: u32::from(edge.neighbor_vid()),
                orientation: 0,
                slot: edge.edge_slot_index_raw(),
            });
        }
        for edge in graph
            .directed_in_edges_iter(VertexId::from(owner), crate::OutEdgeOrder::Ascending)
            .map_err(|error| format!("reverse identity scan failed: {error}"))?
        {
            identities.push(MeasurementPhysicalIdentity {
                owner,
                target: u32::from(edge.neighbor_vid()),
                orientation: 1,
                slot: edge.edge_slot_index_raw(),
            });
        }
    }
    identities.sort();
    if identities.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("fixture produced duplicate physical identities".to_owned());
    }
    if identities.len() != directed_edges.len().saturating_mul(2) {
        return Err("fixture physical identity cardinality mismatch".to_owned());
    }
    Ok(AliasOnlyMeasurementFixture { graph, identities })
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

pub(crate) fn lara_graph(
    elem_capacity: u64,
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
        segment_size,
        0,
    )
    .expect("lara graph");
    for _ in 0..vertex_count {
        graph.push_vertex(Vertex::default()).expect("push vertex");
    }
    graph
}

pub(crate) fn populated_lara_graph(
    vertex_count: u32,
    edges_per_vertex: u32,
) -> LaraGraph<TestEdge, Vertex, BenchMemory> {
    // This is intentionally a production-shaped fixture: initial slab slots
    // remain zero, so insertion may use the overflow log just as the default
    // graph configuration does. Do not classify its scans as slab-only.
    let capacity = u64::from(vertex_count)
        .saturating_mul(u64::from(edges_per_vertex).saturating_add(4))
        .max(16);
    let segment_size = 16;
    let graph = lara_graph(capacity, segment_size, vertex_count);
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
        segment_size,
        0,
        DeferredConfig {
            leaf_dirty_density: 0.0,
            log_urgent_ratio: 0.80,
        },
    )
    .expect("deferred graph");
    for _ in 0..vertex_count {
        graph.push_vertex(Vertex::default()).expect("push vertex");
    }
    graph
}

pub(crate) fn bidirectional_graph<E>(
    vertex_count: u32,
) -> BidirectionalLaraGraph<E, Vertex, BenchMemory>
where
    E: crate::traits::CsrEdge,
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
        16,
        0,
    )
    .expect("bidirectional graph");
    for _ in 0..vertex_count {
        graph.push_vertex(Vertex::default()).expect("push vertex");
    }
    graph
}

pub(crate) fn deferred_bidirectional_graph<E>(
    vertex_count: u32,
) -> DeferredBidirectionalLaraGraph<E, Vertex, BenchMemory>
where
    E: crate::traits::CsrEdge + crate::traits::CsrEdgeTombstone,
{
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
        u64::from(vertex_count).saturating_mul(4).max(16),
        16,
        16,
        DeferredConfig {
            leaf_dirty_density: 0.0,
            log_urgent_ratio: 0.80,
        },
    )
    .expect("deferred bidirectional graph");
    let target_segments = segment_tree_leaf_count(vertex_count.into(), 16);
    graph
        .forward()
        .edges()
        .grow_segment_tree_to(target_segments)
        .expect("grow deferred forward graph segments");
    graph
        .reverse()
        .edges()
        .grow_segment_tree_to(target_segments)
        .expect("grow deferred reverse graph segments");
    for _ in 0..vertex_count {
        graph.push_vertex(Vertex::default()).expect("push vertex");
    }
    graph
}

#[inline]
pub(crate) fn undirected_edge(dst: u32) -> UndirectedTestEdge {
    UndirectedTestEdge::new(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurement_bundles_are_independent_and_descend_from_max_id() {
        let mut alias =
            MeasurementMemoryBundle::with_representation(MeasurementRepresentation::AliasOnly);
        let mut published =
            MeasurementMemoryBundle::with_representation(MeasurementRepresentation::Published);
        let alias_memory = alias.memory();
        let published_memory = published.memory();
        let _alias_second_memory = alias.memory();
        let _published_second_memory = published.memory();

        assert_eq!(alias.representation(), MeasurementRepresentation::AliasOnly);
        assert_eq!(
            published.representation(),
            MeasurementRepresentation::Published
        );
        assert_eq!(
            alias.allocated_ids,
            vec![MEASUREMENT_MEMORY_ID_MAX, MEASUREMENT_MEMORY_ID_MAX - 1]
        );
        assert_eq!(
            published.allocated_ids,
            vec![MEASUREMENT_MEMORY_ID_MAX, MEASUREMENT_MEMORY_ID_MAX - 1]
        );
        assert_eq!(alias.next_id, MEASUREMENT_MEMORY_ID_MAX - 2);
        assert_eq!(published.next_id, MEASUREMENT_MEMORY_ID_MAX - 2);

        alias_memory.grow(1);
        assert_eq!(alias_memory.size(), 1);
        assert_eq!(published_memory.size(), 0);
    }

    #[test]
    fn alias_fixture_extracts_forward_and_reverse_physical_identities() {
        let fixture =
            build_alias_only_measurement_fixture(4, &[(0, 1), (0, 2)]).expect("alias fixture");
        assert_eq!(fixture.identities.len(), 4);
        assert_eq!(
            fixture
                .identities
                .iter()
                .filter(|identity| identity.orientation == 0)
                .count(),
            2
        );
        assert_eq!(
            fixture
                .identities
                .iter()
                .filter(|identity| identity.orientation == 1)
                .count(),
            2
        );
        assert!(fixture.identities.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn alias_fixture_rejects_out_of_range_edges_before_population() {
        let result = build_alias_only_measurement_fixture(2, &[(0, 2)]);
        assert!(matches!(
            result,
            Err(message) if message == "fixture edge endpoint is out of range"
        ));
    }

    #[test]
    fn deferred_undirected_fixture_uses_production_vertex_materialization() {
        let graph = deferred_bidirectional_graph::<UndirectedTestEdge>(256);
        for i in 0..MEDIUM_N {
            let src = VertexId::from((i % 256) as u32);
            let dst = VertexId::from(((i + 1) % 256) as u32);
            graph
                .insert_undirected_deferred(src, dst, UndirectedTestEdge::new(u32::from(dst)))
                .expect("production-path undirected fixture insert");
        }
        assert!(graph.maintenance_queue_len() > 0);
    }
}
