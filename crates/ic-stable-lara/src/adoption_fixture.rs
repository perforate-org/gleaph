//! Feature-gated, measurement-only LARA fixtures for ADR 0048.
//!
//! This module is not part of the production stable layout. It owns a fresh in-memory manager,
//! allocates usable `MemoryId`s from 254 downward, and exposes only exact physical identities.

use crate::{
    BidirectionalLaraGraph, Vertex, VertexId,
    traits::{CsrEdge, CsrEdgeUndirected},
};
use ic_stable_structures::{
    DefaultMemoryImpl,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
};

/// In-memory virtual region used by one measurement fixture.
pub type FixtureMemory = VirtualMemory<DefaultMemoryImpl>;

const MAX_USABLE_MEMORY_ID: u8 = u8::MAX - 1;

/// Representation candidate used by measurement fixtures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureRepresentation {
    /// Existing alias-backed adjacency only.
    AliasOnly,
    /// ScanOnly candidate (reserved for the later fixture slice).
    ScanOnly,
    /// Published mate candidate (reserved for the later fixture slice).
    Published,
}

/// Exact physical half-edge identity extracted from LARA.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub struct PhysicalIdentity {
    /// Owning vertex row.
    pub owner: u32,
    /// Neighbor vertex row.
    pub target: u32,
    /// Forward (`0`) or reverse (`1`) orientation.
    pub orientation: u8,
    /// Slot index within the owning row.
    pub slot: u32,
}

/// Four-byte fixture edge payload with an explicit undirected marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureEdge {
    /// Neighbor vertex ID.
    pub neighbor: u32,
    /// Whether the record is an undirected adjacency.
    pub undirected: bool,
    slot: u32,
}

impl CsrEdge for FixtureEdge {
    const BYTES: usize = 5;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            neighbor: u32::from_le_bytes(bytes[..4].try_into().expect("fixture edge bytes")),
            undirected: bytes[4] != 0,
            slot: 0,
        }
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[..4].copy_from_slice(&self.neighbor.to_le_bytes());
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

    fn with_slot_index(mut self, slot: u32) -> Self {
        self.slot = slot;
        self
    }

    fn edge_slot_index_raw(&self) -> u32 {
        self.slot
    }
}

impl CsrEdgeUndirected for FixtureEdge {
    fn is_undirected(&self) -> bool {
        self.undirected
    }

    fn with_undirected(self, undirected: bool) -> Self {
        Self { undirected, ..self }
    }
}

struct MemoryBundle {
    manager: MemoryManager<DefaultMemoryImpl>,
    next_id: u8,
    representation: FixtureRepresentation,
}

impl MemoryBundle {
    fn new(representation: FixtureRepresentation) -> Self {
        Self {
            manager: MemoryManager::init(DefaultMemoryImpl::default()),
            next_id: MAX_USABLE_MEMORY_ID,
            representation,
        }
    }

    fn memory(&mut self) -> FixtureMemory {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_sub(1)
            .expect("fixture MemoryId exhausted");
        self.manager.get(MemoryId::new(id))
    }
}

/// Populated AliasOnly bidirectional LARA fixture and its identity rows.
pub struct AliasOnlyFixture {
    /// The independently owned bidirectional graph.
    pub graph: BidirectionalLaraGraph<FixtureEdge, Vertex, FixtureMemory>,
    /// Sorted, duplicate-free physical half-edge identities.
    pub identities: Vec<PhysicalIdentity>,
    /// Representation tag for evidence routing.
    pub representation: FixtureRepresentation,
}

/// Build an isolated AliasOnly fixture from directed endpoint pairs.
pub fn build_alias_only_fixture(
    vertex_count: u32,
    directed_edges: &[(u32, u32)],
) -> Result<AliasOnlyFixture, String> {
    let mut memories = MemoryBundle::new(FixtureRepresentation::AliasOnly);
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
    .map_err(|error| format!("fixture graph init failed: {error}"))?;
    let target_segments = crate::lara::edge::segment_tree_leaf_count(vertex_count.into(), 16);
    graph
        .forward()
        .edges()
        .grow_segment_tree_to(target_segments)
        .map_err(|error| format!("forward fixture geometry failed: {error}"))?;
    graph
        .reverse()
        .edges()
        .grow_segment_tree_to(target_segments)
        .map_err(|error| format!("reverse fixture geometry failed: {error}"))?;
    for vid in 0..vertex_count {
        graph
            .push_vertex(Vertex::from_parts(u64::from(vid) * 16, 0, 0, -1, false))
            .map_err(|error| format!("fixture vertex insert failed: {error}"))?;
    }
    for &(source, target) in directed_edges {
        if source >= vertex_count || target >= vertex_count {
            return Err("fixture edge endpoint is out of range".to_owned());
        }
        graph
            .insert_directed(
                VertexId::from(source),
                VertexId::from(target),
                FixtureEdge {
                    neighbor: target,
                    undirected: false,
                    slot: 0,
                },
            )
            .map_err(|error| format!("fixture edge insert failed: {error}"))?;
    }

    let mut identities = Vec::with_capacity(directed_edges.len().saturating_mul(2));
    for owner in 0..vertex_count {
        for edge in graph
            .directed_out_edges_iter(VertexId::from(owner), crate::OutEdgeOrder::Ascending)
            .map_err(|error| format!("forward identity scan failed: {error}"))?
        {
            identities.push(PhysicalIdentity {
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
            identities.push(PhysicalIdentity {
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
    Ok(AliasOnlyFixture {
        graph,
        identities,
        representation: memories.representation,
    })
}

/// Build an isolated AliasOnly fixture from undirected endpoint pairs.
pub fn build_alias_only_undirected_fixture(
    vertex_count: u32,
    edges: &[(u32, u32)],
) -> Result<AliasOnlyFixture, String> {
    let mut memories = MemoryBundle::new(FixtureRepresentation::AliasOnly);
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
    .map_err(|error| format!("fixture graph init failed: {error}"))?;
    let target_segments = crate::lara::edge::segment_tree_leaf_count(vertex_count.into(), 16);
    graph
        .forward()
        .edges()
        .grow_segment_tree_to(target_segments)
        .map_err(|error| format!("forward fixture geometry failed: {error}"))?;
    graph
        .reverse()
        .edges()
        .grow_segment_tree_to(target_segments)
        .map_err(|error| format!("reverse fixture geometry failed: {error}"))?;
    for vid in 0..vertex_count {
        graph
            .push_vertex(Vertex::from_parts(u64::from(vid) * 16, 0, 0, -1, false))
            .map_err(|error| format!("fixture vertex insert failed: {error}"))?;
    }
    for &(source, target) in edges {
        if source >= vertex_count || target >= vertex_count {
            return Err("fixture edge endpoint is out of range".to_owned());
        }
        graph
            .insert_undirected(
                VertexId::from(source),
                VertexId::from(target),
                FixtureEdge {
                    neighbor: target,
                    undirected: true,
                    slot: 0,
                },
            )
            .map_err(|error| format!("fixture edge insert failed: {error}"))?;
    }

    let expected = edges
        .iter()
        .map(|(source, target)| if source == target { 1 } else { 2 })
        .sum::<usize>();
    let mut identities = Vec::with_capacity(expected);
    for owner in 0..vertex_count {
        for edge in graph
            .undirected_edges_iter(VertexId::from(owner), crate::OutEdgeOrder::Ascending)
            .map_err(|error| format!("undirected identity scan failed: {error}"))?
        {
            identities.push(PhysicalIdentity {
                owner,
                target: u32::from(edge.neighbor_vid()),
                orientation: 0,
                slot: edge.edge_slot_index_raw(),
            });
        }
    }
    identities.sort();
    if identities.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("fixture produced duplicate physical identities".to_owned());
    }
    if identities.len() != expected {
        return Err(format!(
            "fixture physical identity cardinality mismatch: expected {expected}, got {}",
            identities.len()
        ));
    }
    Ok(AliasOnlyFixture {
        graph,
        identities,
        representation: memories.representation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_fixture_has_exact_forward_reverse_rows() {
        let fixture = build_alias_only_fixture(4, &[(0, 1), (0, 2)]).expect("fixture");
        assert_eq!(fixture.identities.len(), 4);
        assert_eq!(fixture.representation, FixtureRepresentation::AliasOnly);
        assert!(fixture.identities.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn undirected_fixture_has_two_rows_and_self_loop_has_one() {
        let fixture =
            build_alias_only_undirected_fixture(4, &[(0, 1), (2, 2)]).expect("undirected fixture");
        assert_eq!(fixture.identities.len(), 3);
        assert_eq!(
            fixture
                .identities
                .iter()
                .filter(|identity| identity.owner == identity.target)
                .count(),
            1
        );
    }
}
