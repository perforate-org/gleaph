//! Feature-gated, measurement-only LARA fixtures for ADR 0048.
//!
//! This module is not part of the production stable layout. It owns a fresh in-memory manager,
//! allocates usable `MemoryId`s from 254 downward, and exposes only exact physical identities.

use crate::labeled::bidirectional::mate_ranked_prototype::{RankedBlob, RankedBucket};
use crate::{
    BidirectionalLaraGraph, Vertex, VertexId,
    labeled::{
        BucketLabelKey, DeferredBidirectionalLabeledLaraGraph, InitialCapacities,
        MateStorageMemories, OutEdgeOrder, bidirectional::Orientation as LabeledOrientation,
        bidirectional::mate_enumeration::default_mate_leaf_enumeration_policy,
    },
    traits::{CsrEdge, CsrEdgeTombstone, CsrEdgeUndirected},
};
use ic_stable_structures::{
    DefaultMemoryImpl,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
};
use std::collections::BTreeMap;

/// In-memory virtual region used by one measurement fixture.
pub type FixtureMemory = VirtualMemory<DefaultMemoryImpl>;

const MAX_USABLE_MEMORY_ID: u8 = u8::MAX - 1;

/// Representation candidate used by measurement fixtures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureRepresentation {
    /// Existing alias-backed adjacency only.
    AliasOnly,
    /// Canonical adjacency with no mate metadata.
    ScanOnly,
    /// Published mate candidate with an independently owned mate storage bundle.
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

/// Exact physical identity for a labeled measurement row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Ord, PartialOrd)]
pub struct LabeledPhysicalIdentity {
    /// Owning vertex row.
    pub owner: u32,
    /// Neighbor vertex row.
    pub target: u32,
    /// Raw bucket label key (including directedness bit).
    pub label: u16,
    /// Forward (`0`) or reverse (`1`) orientation.
    pub orientation: u8,
    /// Slot index within the owning row and bucket.
    pub slot: u32,
}

/// Encode a rank-indexed Packed blob from the same physical identities used by the fixture.
///
/// This is a measurement-only adapter: it derives counterpart slots by occurrence rank and does
/// not mutate or inspect production mate storage. `undirected` selects the single forward bucket
/// orientation; directed fixtures contain both forward and reverse identity rows.
pub fn ranked_packed_blob_bytes(
    identities: &[PhysicalIdentity],
    undirected: bool,
) -> Result<u64, String> {
    ranked_packed_blob(identities, undirected).map(|bytes| bytes.len() as u64)
}

/// Encode and return the measurement-only rank-indexed Packed bytes.
pub fn ranked_packed_blob(
    identities: &[PhysicalIdentity],
    undirected: bool,
) -> Result<Vec<u8>, String> {
    let mut grouped: BTreeMap<(u32, u32, u8), Vec<u32>> = BTreeMap::new();
    for identity in identities {
        let orientation = if undirected { 0 } else { identity.orientation };
        grouped
            .entry((identity.owner, identity.target, orientation))
            .or_default()
            .push(identity.slot);
    }
    for slots in grouped.values_mut() {
        slots.sort_unstable();
    }
    let mut buckets: BTreeMap<(u32, u8), Vec<(u32, u32)>> = BTreeMap::new();
    for identity in identities {
        let orientation = if undirected { 0 } else { identity.orientation };
        let counterpart_orientation = if undirected { 0 } else { 1 - orientation };
        let source_slots = grouped
            .get(&(identity.owner, identity.target, orientation))
            .ok_or_else(|| "rank source group missing".to_owned())?;
        let rank = source_slots
            .binary_search(&identity.slot)
            .map_err(|_| "rank source slot missing".to_owned())?;
        let mate_slots = grouped
            .get(&(identity.target, identity.owner, counterpart_orientation))
            .ok_or_else(|| "rank counterpart group missing".to_owned())?;
        let mate = *mate_slots
            .get(rank)
            .ok_or_else(|| "rank counterpart occurrence missing".to_owned())?;
        buckets
            .entry((identity.owner, orientation))
            .or_default()
            .push((identity.slot, mate));
    }
    let mut ranked_buckets = Vec::with_capacity(buckets.len());
    for ((owner, orientation), mut rows) in buckets {
        rows.sort_unstable_by_key(|(slot, _)| *slot);
        rows.dedup_by_key(|(slot, _)| *slot);
        if rows.is_empty() {
            continue;
        }
        let max_slot = rows
            .iter()
            .flat_map(|(source, mate)| [*source, *mate])
            .max()
            .ok_or_else(|| "rank bucket has no slots".to_owned())?;
        let width = if max_slot <= 0xff {
            1
        } else if max_slot <= 0xffff {
            2
        } else if max_slot <= 0x00ff_ffff {
            3
        } else {
            4
        };
        let mut mate_bytes = Vec::with_capacity(rows.len() * usize::from(width));
        for (_, mate) in rows {
            let bytes = mate.to_be_bytes();
            mate_bytes.extend_from_slice(&bytes[4 - usize::from(width)..]);
        }
        ranked_buckets.push(RankedBucket {
            owner_vertex_id: owner,
            bucket_label_key: if orientation == 0 { 1 } else { 2 },
            entries: u32::try_from(mate_bytes.len() / usize::from(width))
                .map_err(|_| "rank entry count overflow".to_owned())?,
            width_bytes: width,
            mate_slots: mate_bytes,
        });
    }
    RankedBlob {
        buckets: ranked_buckets,
    }
    .encode()
    .map_err(|error| format!("ranked blob encode failed: {error:?}"))
}

/// Resolve one rank-indexed mate slot from a measurement blob.
pub fn ranked_packed_lookup(
    bytes: &[u8],
    owner: u32,
    orientation: u8,
    rank: u32,
) -> Result<u32, String> {
    let blob =
        RankedBlob::decode(bytes).map_err(|error| format!("ranked decode failed: {error:?}"))?;
    let label = if orientation == 0 { 1 } else { 2 };
    blob.buckets
        .iter()
        .find(|bucket| bucket.owner_vertex_id == owner && bucket.bucket_label_key == label)
        .ok_or_else(|| "ranked bucket missing".to_owned())?
        .mate_slot_for_rank(rank)
        .map_err(|error| format!("ranked lookup failed: {error:?}"))
}

/// Decoded measurement-only rank lookup handle. Decode is performed once during setup so runtime
/// probes measure lookup rather than repeatedly charging blob parsing.
pub struct RankedPackedLookup {
    blob: RankedBlob,
}

impl RankedPackedLookup {
    /// Decode one measurement-only rank blob once for repeated lookup probes.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        RankedBlob::decode(bytes)
            .map(|blob| Self { blob })
            .map_err(|error| format!("ranked decode failed: {error:?}"))
    }

    /// Resolve a mate slot by owner, orientation, and canonical occurrence rank.
    pub fn lookup(&self, owner: u32, orientation: u8, rank: u32) -> Result<u32, String> {
        let label = if orientation == 0 { 1 } else { 2 };
        self.blob
            .buckets
            .iter()
            .find(|bucket| bucket.owner_vertex_id == owner && bucket.bucket_label_key == label)
            .ok_or_else(|| "ranked bucket missing".to_owned())?
            .mate_slot_for_rank(rank)
            .map_err(|error| format!("ranked lookup failed: {error:?}"))
    }
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

/// Labeled edge record used only by the Published measurement fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublishedEdge {
    neighbor: u32,
    slot: u32,
}

impl PublishedEdge {
    /// Construct a labeled fixture edge for measurement-only canonical mutation traces.
    pub const fn new(neighbor: u32, slot: u32) -> Self {
        Self { neighbor, slot }
    }
}

impl CsrEdge for PublishedEdge {
    const BYTES: usize = 10;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            neighbor: u32::from_le_bytes(bytes[..4].try_into().expect("published edge bytes")),
            slot: 0,
        }
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[..4].copy_from_slice(&self.neighbor.to_le_bytes());
        bytes[4..10].fill(0);
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

impl CsrEdgeTombstone for PublishedEdge {
    fn tombstone_edge() -> Self {
        Self {
            neighbor: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            slot: 0,
        }
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

/// Independently owned Published candidate with canonical rows and published mate blobs.
pub struct PublishedFixture {
    /// The deferred labeled graph owning canonical and mate storage.
    pub graph: DeferredBidirectionalLabeledLaraGraph<PublishedEdge, FixtureMemory>,
    /// Sorted, duplicate-free physical half-edge identities.
    pub identities: Vec<PhysicalIdentity>,
    /// Representation tag for evidence routing.
    pub representation: FixtureRepresentation,
}

/// Published labeled fixture retaining label-local physical identities.
pub struct MixedLabelPublishedFixture {
    /// The deferred labeled graph owning canonical and mate storage.
    pub graph: DeferredBidirectionalLabeledLaraGraph<PublishedEdge, FixtureMemory>,
    /// Sorted, duplicate-free identities including their bucket labels.
    pub identities: Vec<LabeledPhysicalIdentity>,
    /// Representation tag for evidence routing.
    pub representation: FixtureRepresentation,
}

/// Published directed fixture with deletion churn retained in the physical location evidence.
pub struct SparseSlotPublishedFixture {
    /// The deferred labeled graph owning canonical and mate storage.
    pub graph: DeferredBidirectionalLabeledLaraGraph<PublishedEdge, FixtureMemory>,
    /// Sorted physical identities; overflow-log locations have the high bit set.
    pub identities: Vec<PhysicalIdentity>,
    /// Representation tag for evidence routing.
    pub representation: FixtureRepresentation,
}

fn labeled_memory_regions(memories: &mut MemoryBundle) -> [FixtureMemory; 15] {
    core::array::from_fn(|_| memories.memory())
}

fn new_published_graph(
    memories: &mut MemoryBundle,
) -> Result<DeferredBidirectionalLabeledLaraGraph<PublishedEdge, FixtureMemory>, String> {
    let forward = labeled_memory_regions(memories);
    let reverse = labeled_memory_regions(memories);
    DeferredBidirectionalLabeledLaraGraph::new(
        forward[0].clone(),
        forward[1].clone(),
        forward[2].clone(),
        forward[3].clone(),
        forward[4].clone(),
        forward[5].clone(),
        forward[6].clone(),
        forward[7].clone(),
        forward[8].clone(),
        forward[9].clone(),
        forward[10].clone(),
        forward[11].clone(),
        forward[12].clone(),
        forward[13].clone(),
        forward[14].clone(),
        reverse[0].clone(),
        reverse[1].clone(),
        reverse[2].clone(),
        reverse[3].clone(),
        reverse[4].clone(),
        reverse[5].clone(),
        reverse[6].clone(),
        reverse[7].clone(),
        reverse[8].clone(),
        reverse[9].clone(),
        reverse[10].clone(),
        reverse[11].clone(),
        reverse[12].clone(),
        reverse[13].clone(),
        reverse[14].clone(),
        MateStorageMemories::new(
            memories.memory(),
            memories.memory(),
            memories.memory(),
            memories.memory(),
        ),
        memories.memory(),
        memories.memory(),
        InitialCapacities::uniform(1 << 16),
        BucketLabelKey::UNLABELED_DIRECTED,
    )
    .map_err(|error| format!("published fixture graph init failed: {error}"))
}

/// Build a directed Published fixture and publish every canonical mate leaf.
pub fn build_published_fixture(
    vertex_count: u32,
    directed_edges: &[(u32, u32)],
) -> Result<PublishedFixture, String> {
    let mut memories = MemoryBundle::new(FixtureRepresentation::Published);
    let graph = new_published_graph(&mut memories)?;

    for _ in 0..vertex_count {
        graph
            .push_vertex()
            .map_err(|error| format!("published fixture vertex insert failed: {error}"))?;
    }
    let label = BucketLabelKey::directed_from_index(1);
    for &(source, target) in directed_edges {
        if source >= vertex_count || target >= vertex_count {
            return Err("published fixture edge endpoint is out of range".to_owned());
        }
        graph
            .insert_directed_edge(
                VertexId::from(source),
                VertexId::from(target),
                label,
                PublishedEdge {
                    neighbor: target,
                    slot: 0,
                },
                PublishedEdge {
                    neighbor: source,
                    slot: 0,
                },
            )
            .map_err(|error| format!("published fixture edge insert failed: {error}"))?;
    }
    let policy = default_mate_leaf_enumeration_policy();
    for orientation in [LabeledOrientation::Forward, LabeledOrientation::Reverse] {
        let segment_count = graph.forward().segment_count();
        for leaf in 0..segment_count {
            let aggregate = graph
                .enumerate_mate_leaf(orientation, leaf, policy)
                .map_err(|error| format!("published fixture mate enumeration failed: {error}"))?;
            graph
                .rebuild_mate_leaf_from_canonical(&aggregate)
                .map_err(|error| format!("published fixture mate publish failed: {error}"))?;
        }
    }

    let mut identities = Vec::with_capacity(directed_edges.len().saturating_mul(2));
    for owner in 0..vertex_count {
        for edge in graph
            .directed_out_edges_iter(VertexId::from(owner), OutEdgeOrder::Ascending)
            .map_err(|error| format!("published forward identity scan failed: {error}"))?
        {
            let edge =
                edge.map_err(|error| format!("published forward identity read failed: {error}"))?;
            identities.push(PhysicalIdentity {
                owner,
                target: u32::from(edge.neighbor_vid()),
                orientation: 0,
                slot: edge.edge_slot_index_raw(),
            });
        }
        for edge in graph
            .directed_in_edges_iter(VertexId::from(owner), OutEdgeOrder::Ascending)
            .map_err(|error| format!("published reverse identity scan failed: {error}"))?
        {
            let edge =
                edge.map_err(|error| format!("published reverse identity read failed: {error}"))?;
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
        return Err("published fixture produced duplicate physical identities".to_owned());
    }
    if identities.len() != directed_edges.len().saturating_mul(2) {
        return Err("published fixture physical identity cardinality mismatch".to_owned());
    }
    Ok(PublishedFixture {
        graph,
        identities,
        representation: FixtureRepresentation::Published,
    })
}

/// Build a real labeled Published fixture with two independent directed buckets.
///
/// Both labels intentionally use the same endpoints so any cross-label sharing would be visible
/// in the extracted identities. Mate publication is performed for both orientations after all
/// canonical inserts, matching the normal fixture lifecycle.
pub fn build_mixed_label_published_fixture(
    vertex_count: u32,
    edges_per_label: u32,
) -> Result<MixedLabelPublishedFixture, String> {
    if vertex_count < 2 {
        return Err("mixed-label fixture requires at least two vertices".to_owned());
    }
    let mut memories = MemoryBundle::new(FixtureRepresentation::Published);
    let graph = new_published_graph(&mut memories)?;
    for _ in 0..vertex_count {
        graph
            .push_vertex()
            .map_err(|error| format!("mixed-label fixture vertex insert failed: {error}"))?;
    }
    let labels = [
        BucketLabelKey::directed_from_index(1),
        BucketLabelKey::directed_from_index(2),
    ];
    for label in labels {
        for _ in 0..edges_per_label {
            graph
                .insert_directed_edge(
                    VertexId::from(0),
                    VertexId::from(1),
                    label,
                    PublishedEdge {
                        neighbor: 1,
                        slot: 0,
                    },
                    PublishedEdge {
                        neighbor: 0,
                        slot: 0,
                    },
                )
                .map_err(|error| format!("mixed-label fixture edge insert failed: {error}"))?;
        }
    }
    let policy = default_mate_leaf_enumeration_policy();
    for orientation in [LabeledOrientation::Forward, LabeledOrientation::Reverse] {
        let segment_count = graph.forward().segment_count();
        for leaf in 0..segment_count {
            let aggregate = graph
                .enumerate_mate_leaf(orientation, leaf, policy)
                .map_err(|error| format!("mixed-label fixture mate enumeration failed: {error}"))?;
            graph
                .rebuild_mate_leaf_from_canonical(&aggregate)
                .map_err(|error| format!("mixed-label fixture mate publish failed: {error}"))?;
        }
    }

    let mut identities = Vec::with_capacity(
        usize::try_from(edges_per_label)
            .map_err(|_| "mixed-label fixture edge count overflow".to_owned())?
            .saturating_mul(labels.len())
            .saturating_mul(2),
    );
    for owner in 0..vertex_count {
        for label in labels {
            graph
                .for_each_out_edges_for_label(VertexId::from(owner), label, |edge| {
                    identities.push(LabeledPhysicalIdentity {
                        owner,
                        target: u32::from(edge.neighbor_vid()),
                        label: label.raw(),
                        orientation: 0,
                        slot: edge.edge_slot_index_raw(),
                    });
                })
                .map_err(|error| format!("mixed-label forward identity scan failed: {error}"))?;
            graph
                .for_each_in_edges_for_label(VertexId::from(owner), label, |edge| {
                    identities.push(LabeledPhysicalIdentity {
                        owner,
                        target: u32::from(edge.neighbor_vid()),
                        label: label.raw(),
                        orientation: 1,
                        slot: edge.edge_slot_index_raw(),
                    });
                })
                .map_err(|error| format!("mixed-label reverse identity scan failed: {error}"))?;
        }
    }
    identities.sort();
    if identities.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("mixed-label fixture produced duplicate physical identities".to_owned());
    }
    let expected = usize::try_from(edges_per_label)
        .map_err(|_| "mixed-label fixture edge count overflow".to_owned())?
        .saturating_mul(labels.len())
        .saturating_mul(2);
    if identities.len() != expected {
        return Err(format!(
            "mixed-label fixture cardinality mismatch: expected {expected}, got {}",
            identities.len()
        ));
    }
    Ok(MixedLabelPublishedFixture {
        graph,
        identities,
        representation: FixtureRepresentation::Published,
    })
}

/// Build a real directed fixture with deletion-created slab/log location gaps.
pub fn build_sparse_slot_published_fixture(
    inserted_edges: u32,
) -> Result<SparseSlotPublishedFixture, String> {
    if inserted_edges < 4 || !inserted_edges.is_multiple_of(2) {
        return Err("sparse-slot fixture requires an even edge count >= 4".to_owned());
    }
    let mut memories = MemoryBundle::new(FixtureRepresentation::Published);
    let graph = new_published_graph(&mut memories)?;
    for _ in 0..2 {
        graph
            .push_vertex()
            .map_err(|error| format!("sparse-slot fixture vertex insert failed: {error}"))?;
    }
    let label = BucketLabelKey::directed_from_index(1);
    for _ in 0..inserted_edges {
        graph
            .insert_directed_edge(
                VertexId::from(0),
                VertexId::from(1),
                label,
                PublishedEdge {
                    neighbor: 1,
                    slot: 0,
                },
                PublishedEdge {
                    neighbor: 0,
                    slot: 0,
                },
            )
            .map_err(|error| format!("sparse-slot fixture edge insert failed: {error}"))?;
    }
    for slot in (0..inserted_edges).step_by(2).rev() {
        graph
            .remove_forward_edge_at_slot(VertexId::from(0), label, slot)
            .map_err(|error| format!("sparse-slot forward removal failed: {error}"))?;
        graph
            .remove_reverse_edge_at_slot(VertexId::from(1), label, slot)
            .map_err(|error| format!("sparse-slot reverse removal failed: {error}"))?;
    }
    let policy = default_mate_leaf_enumeration_policy();
    for orientation in [LabeledOrientation::Forward, LabeledOrientation::Reverse] {
        for leaf in 0..graph.forward().segment_count() {
            let aggregate = graph
                .enumerate_mate_leaf(orientation, leaf, policy)
                .map_err(|error| format!("sparse-slot fixture mate enumeration failed: {error}"))?;
            graph
                .rebuild_mate_leaf_from_canonical(&aggregate)
                .map_err(|error| format!("sparse-slot fixture mate publish failed: {error}"))?;
        }
    }
    let mut identities = Vec::new();
    graph
        .forward()
        .for_each_live_physical_edge_location_for_label(VertexId::from(0), label, |slot, edge| {
            identities.push(PhysicalIdentity {
                owner: 0,
                target: u32::from(edge.neighbor_vid()),
                orientation: 0,
                slot,
            });
        })
        .map_err(|error| format!("sparse-slot forward identity scan failed: {error}"))?;
    graph
        .reverse()
        .for_each_live_physical_edge_location_for_label(VertexId::from(1), label, |slot, edge| {
            identities.push(PhysicalIdentity {
                owner: 1,
                target: u32::from(edge.neighbor_vid()),
                orientation: 1,
                slot,
            });
        })
        .map_err(|error| format!("sparse-slot reverse identity scan failed: {error}"))?;
    identities.sort();
    let expected =
        usize::try_from(inserted_edges).map_err(|_| "sparse count overflow".to_owned())?;
    if identities.len() != expected
        || identities
            .iter()
            .any(|identity| identity.slot & 0x8000_0000 == 0)
    {
        return Err("sparse-slot fixture did not retain overflow-log locations".to_owned());
    }
    Ok(SparseSlotPublishedFixture {
        graph,
        identities,
        representation: FixtureRepresentation::Published,
    })
}

/// Build an undirected Published fixture and publish forward mate leaves.
pub fn build_published_undirected_fixture(
    vertex_count: u32,
    edges: &[(u32, u32)],
) -> Result<PublishedFixture, String> {
    let mut memories = MemoryBundle::new(FixtureRepresentation::Published);
    let graph = new_published_graph(&mut memories)?;
    for _ in 0..vertex_count {
        graph
            .push_vertex()
            .map_err(|error| format!("published fixture vertex insert failed: {error}"))?;
    }
    let label = BucketLabelKey::undirected_from_index(1);
    for &(source, target) in edges {
        if source >= vertex_count || target >= vertex_count {
            return Err("published undirected fixture edge endpoint is out of range".to_owned());
        }
        graph
            .insert_undirected_deferred(
                VertexId::from(source),
                VertexId::from(target),
                label,
                PublishedEdge {
                    neighbor: target,
                    slot: 0,
                },
                PublishedEdge {
                    neighbor: source,
                    slot: 0,
                },
            )
            .map_err(|error| format!("published undirected edge insert failed: {error}"))?;
    }
    let policy = default_mate_leaf_enumeration_policy();
    for leaf in 0..graph.forward().segment_count() {
        let aggregate = graph
            .enumerate_mate_leaf(LabeledOrientation::Forward, leaf, policy)
            .map_err(|error| format!("published undirected mate enumeration failed: {error}"))?;
        graph
            .rebuild_mate_leaf_from_canonical(&aggregate)
            .map_err(|error| format!("published undirected mate publish failed: {error}"))?;
    }

    let mut identities = Vec::new();
    for owner in 0..vertex_count {
        for edge in graph
            .undirected_edges_iter(VertexId::from(owner), OutEdgeOrder::Ascending)
            .map_err(|error| format!("published undirected identity scan failed: {error}"))?
        {
            let edge = edge
                .map_err(|error| format!("published undirected identity read failed: {error}"))?;
            identities.push(PhysicalIdentity {
                owner,
                target: u32::from(edge.neighbor_vid()),
                orientation: 0,
                slot: edge.edge_slot_index_raw(),
            });
        }
    }
    identities.sort();
    let expected = edges
        .iter()
        .map(|(source, target)| usize::from(source != target) + 1)
        .sum::<usize>();
    if identities.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("published undirected fixture produced duplicate identities".to_owned());
    }
    if identities.len() != expected {
        return Err(format!(
            "published undirected fixture cardinality mismatch: expected {expected}, got {}",
            identities.len()
        ));
    }
    Ok(PublishedFixture {
        graph,
        identities,
        representation: FixtureRepresentation::Published,
    })
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

/// Build an isolated ScanOnly fixture from the same canonical directed adjacency specification.
///
/// ScanOnly deliberately owns only canonical adjacency; this helper does not allocate or publish
/// mate metadata. The separate representation tag keeps the evidence boundary explicit while the
/// identity rows remain sourced from the owning LARA graph.
pub fn build_scan_only_fixture(
    vertex_count: u32,
    directed_edges: &[(u32, u32)],
) -> Result<AliasOnlyFixture, String> {
    let mut fixture = build_alias_only_fixture(vertex_count, directed_edges)?;
    fixture.representation = FixtureRepresentation::ScanOnly;
    Ok(fixture)
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

/// Build an isolated ScanOnly fixture for undirected and self-loop canonical adjacency.
pub fn build_scan_only_undirected_fixture(
    vertex_count: u32,
    edges: &[(u32, u32)],
) -> Result<AliasOnlyFixture, String> {
    let mut fixture = build_alias_only_undirected_fixture(vertex_count, edges)?;
    fixture.representation = FixtureRepresentation::ScanOnly;
    Ok(fixture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::ControlFlow;

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

    #[test]
    fn scan_only_fixture_has_independent_canonical_rows() {
        let fixture = build_scan_only_fixture(3, &[(0, 1), (0, 2)]).expect("scan fixture");
        assert_eq!(fixture.representation, FixtureRepresentation::ScanOnly);
        assert_eq!(fixture.identities.len(), 4);
        assert!(fixture.identities.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn published_fixture_publishes_real_mate_storage() {
        let edges = (0..80).map(|_| (0, 1)).collect::<Vec<_>>();
        let fixture = build_published_fixture(2, &edges).expect("published fixture");
        assert_eq!(fixture.representation, FixtureRepresentation::Published);
        assert_eq!(fixture.identities.len(), 160);
        assert!(fixture.identities.windows(2).all(|pair| pair[0] < pair[1]));

        let parallel_edges = (0..32).map(|_| (0, 1)).collect::<Vec<_>>();
        let parallel_fixture =
            build_published_fixture(2, &parallel_edges).expect("parallel published fixture");
        assert_eq!(parallel_fixture.identities.len(), 64);

        let undirected_edges = (0..64)
            .flat_map(|source| [(source, (source + 1) % 64), (source, (source + 2) % 64)])
            .collect::<Vec<_>>();
        let undirected_fixture = build_published_undirected_fixture(64, &undirected_edges)
            .expect("undirected published fixture");
        assert_eq!(undirected_fixture.identities.len(), 256);

        let self_loop = build_published_undirected_fixture(1, &[(0, 0)]);
        assert!(self_loop.is_err());
    }

    #[test]
    fn mixed_label_fixture_keeps_real_rows_label_local() {
        let fixture = build_mixed_label_published_fixture(2, 8).expect("mixed labels");
        assert_eq!(fixture.representation, FixtureRepresentation::Published);
        assert_eq!(fixture.identities.len(), 32);
        let labels: std::collections::BTreeSet<_> = fixture
            .identities
            .iter()
            .map(|identity| identity.label)
            .collect();
        assert_eq!(labels.len(), 2);
        for label in labels {
            assert_eq!(
                fixture
                    .identities
                    .iter()
                    .filter(|identity| identity.label == label)
                    .count(),
                16
            );
        }
        assert!(fixture.identities.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn sparse_slot_probe_is_explicitly_blocked_by_logical_slot_iterator() {
        let mut memories = MemoryBundle::new(FixtureRepresentation::Published);
        let graph = new_published_graph(&mut memories).expect("graph");
        for _ in 0..2 {
            graph.push_vertex().expect("vertex");
        }
        let label = BucketLabelKey::directed_from_index(1);
        for _ in 0..16 {
            graph
                .insert_directed_edge(
                    VertexId::from(0),
                    VertexId::from(1),
                    label,
                    PublishedEdge {
                        neighbor: 1,
                        slot: 0,
                    },
                    PublishedEdge {
                        neighbor: 0,
                        slot: 0,
                    },
                )
                .expect("edge");
        }
        for slot in (0..16u32).step_by(2).rev() {
            graph
                .remove_forward_edge_at_slot(VertexId::from(0), label, slot)
                .expect("forward remove");
            graph
                .remove_reverse_edge_at_slot(VertexId::from(1), label, slot)
                .expect("reverse remove");
        }
        let mut slots = Vec::new();
        graph
            .forward()
            .visit_edges(
                VertexId::from(0),
                label,
                OutEdgeOrder::Ascending,
                |slot, _| {
                    slots.push(slot.raw());
                    ControlFlow::<()>::Continue(())
                },
            )
            .map(|_| ())
            .expect("scan");
        assert_eq!(slots, (1..=8).collect::<Vec<_>>());

        let mut physical_slots = Vec::new();
        graph
            .forward()
            .for_each_live_physical_edge_location_for_label(VertexId::from(0), label, |slot, _| {
                physical_slots.push(slot)
            })
            .expect("physical scan");
        assert_eq!(physical_slots.len(), 8);
        assert!(physical_slots.iter().all(|slot| slot & 0x8000_0000 != 0));

        let fixture = build_sparse_slot_published_fixture(16).expect("sparse fixture");
        assert_eq!(fixture.identities.len(), 16);
        assert!(
            fixture
                .identities
                .iter()
                .all(|identity| identity.slot & 0x8000_0000 != 0)
        );
    }

    #[test]
    fn ranked_blob_adapter_preserves_directed_parallel_and_undirected_rows() {
        let directed = build_alias_only_fixture(4, &[(0, 1), (0, 2)]).expect("directed");
        let directed_bytes = ranked_packed_blob_bytes(&directed.identities, false).expect("ranked");
        assert!(directed_bytes > 0);

        let parallel_edges = (0..32).map(|_| (0, 1)).collect::<Vec<_>>();
        let parallel = build_alias_only_fixture(2, &parallel_edges).expect("parallel");
        let parallel_bytes = ranked_packed_blob_bytes(&parallel.identities, false).expect("ranked");
        assert!(parallel_bytes > 0);

        let undirected =
            build_alias_only_undirected_fixture(4, &[(0, 1), (1, 2)]).expect("undirected");
        let undirected_bytes =
            ranked_packed_blob_bytes(&undirected.identities, true).expect("ranked");
        assert!(undirected_bytes > 0);
    }

    #[test]
    fn ranked_blob_adapter_reports_shape_payloads_separately_from_alias_bytes() {
        let directed_edges = (0..64)
            .flat_map(|source| [(source, (source + 1) % 64), (source, (source + 2) % 64)])
            .collect::<Vec<_>>();
        let directed = build_alias_only_fixture(64, &directed_edges).expect("directed");
        assert_eq!(
            ranked_packed_blob_bytes(&directed.identities, false),
            Ok(2_840)
        );

        let undirected =
            build_alias_only_undirected_fixture(64, &directed_edges).expect("undirected");
        assert_eq!(
            ranked_packed_blob_bytes(&undirected.identities, true),
            Ok(1_560)
        );

        let parallel_edges = (0..32).map(|_| (0, 1)).collect::<Vec<_>>();
        let parallel = build_alias_only_fixture(2, &parallel_edges).expect("parallel");
        assert_eq!(
            ranked_packed_blob_bytes(&parallel.identities, false),
            Ok(128)
        );
    }

    #[test]
    fn ranked_lookup_has_exact_parity_and_fail_closed_decode() {
        let fixture = build_alias_only_fixture(4, &[(0, 1), (0, 2)]).expect("fixture");
        let bytes = ranked_packed_blob(&fixture.identities, false).expect("ranked bytes");
        let lookup = RankedPackedLookup::decode(&bytes).expect("decode");
        for identity in &fixture.identities {
            let counterpart = fixture
                .identities
                .iter()
                .find(|other| {
                    other.owner == identity.target
                        && other.target == identity.owner
                        && other.orientation != identity.orientation
                })
                .expect("counterpart");
            assert_eq!(
                lookup.lookup(identity.owner, identity.orientation, 0),
                Ok(counterpart.slot)
            );
        }
        assert!(RankedPackedLookup::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(lookup.lookup(1, 0, u32::MAX).is_err());
    }
}
