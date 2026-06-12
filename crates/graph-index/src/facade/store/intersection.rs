//! Multi-property equality intersection across vertex and edge posting stores (ADR 0009 §3).

use super::{IndexStore, pack_edge_identity, pack_posting_vertex};
use crate::edge_key::EdgePostingKey;
use crate::facade::stable::{INDEX_EDGE_POSTINGS, INDEX_POSTINGS};
use crate::key::PostingKey;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    EdgePostingHit, IndexEqualSpec, IndexIntersectionRequest, IndexIntersectionResult,
    IndexSubject, PostingHit,
};
use std::collections::HashSet;

impl IndexStore {
    pub fn lookup_intersection(&self, req: &IndexIntersectionRequest) -> IndexIntersectionResult {
        lookup_property_intersection(req)
    }
}

pub(crate) fn lookup_property_intersection(
    req: &IndexIntersectionRequest,
) -> IndexIntersectionResult {
    if req.specs.len() < 2 {
        return IndexIntersectionResult::Vertices(Vec::new());
    }
    let all_vertex = req
        .specs
        .iter()
        .all(|spec| matches!(spec.subject, IndexSubject::VertexProperty));
    let all_edge = req
        .specs
        .iter()
        .all(|spec| matches!(spec.subject, IndexSubject::EdgeProperty { .. }));

    if all_vertex {
        return IndexIntersectionResult::Vertices(vertex_intersection(&req.specs));
    }
    if all_edge {
        return IndexIntersectionResult::Edges(edge_intersection(&req.specs));
    }
    IndexIntersectionResult::Vertices(mixed_vertex_intersection(&req.specs))
}

fn vertex_intersection(specs: &[IndexEqualSpec]) -> Vec<PostingHit> {
    intersect_u64_sets(specs.iter().map(collect_vertex_arm).collect())
}

fn mixed_vertex_intersection(specs: &[IndexEqualSpec]) -> Vec<PostingHit> {
    intersect_u64_sets(specs.iter().map(collect_vertex_projection_arm).collect())
}

fn edge_intersection(specs: &[IndexEqualSpec]) -> Vec<EdgePostingHit> {
    intersect_u128_sets(specs.iter().map(collect_edge_arm).collect())
        .into_iter()
        .map(unpack_edge_identity)
        .collect()
}

fn collect_vertex_arm(spec: &IndexEqualSpec) -> HashSet<u64> {
    debug_assert!(matches!(spec.subject, IndexSubject::VertexProperty));
    let lo = PostingKey::prefix_lower(spec.property_id, &spec.value);
    let hi = PostingKey::prefix_upper(spec.property_id, &spec.value);
    let mut set = HashSet::new();
    INDEX_POSTINGS.with_borrow(|postings| {
        for key in postings.range(lo..=hi) {
            set.insert(pack_posting_vertex(key.shard_id, key.vertex_id));
        }
    });
    set
}

fn collect_vertex_projection_arm(spec: &IndexEqualSpec) -> HashSet<u64> {
    match spec.subject {
        IndexSubject::VertexProperty => collect_vertex_arm(spec),
        IndexSubject::EdgeProperty { label_id } => collect_edge_owner_projection(spec, label_id),
    }
}

fn collect_edge_owner_projection(spec: &IndexEqualSpec, label_id: Option<u16>) -> HashSet<u64> {
    let (lo, hi) = edge_prefix_bounds(spec.property_id, &spec.value, label_id);
    let mut set = HashSet::new();
    INDEX_EDGE_POSTINGS.with_borrow(|postings| {
        for key in postings.range(lo..=hi) {
            set.insert(pack_posting_vertex(key.shard_id, key.owner_vertex_id));
        }
    });
    set
}

fn collect_edge_arm(spec: &IndexEqualSpec) -> HashSet<u128> {
    let IndexSubject::EdgeProperty { label_id } = spec.subject else {
        return HashSet::new();
    };
    let (lo, hi) = edge_prefix_bounds(spec.property_id, &spec.value, label_id);
    let mut set = HashSet::new();
    INDEX_EDGE_POSTINGS.with_borrow(|postings| {
        for key in postings.range(lo..=hi) {
            set.insert(pack_edge_identity(
                key.shard_id,
                key.owner_vertex_id,
                key.label_id,
                key.slot_index,
            ));
        }
    });
    set
}

fn edge_prefix_bounds(
    property_id: u32,
    value: &[u8],
    label_id: Option<u16>,
) -> (EdgePostingKey, EdgePostingKey) {
    match label_id {
        Some(label) => (
            EdgePostingKey::prefix_lower_labeled(property_id, value, label),
            EdgePostingKey::prefix_upper_labeled(property_id, value, label),
        ),
        None => (
            EdgePostingKey::prefix_lower(property_id, value),
            EdgePostingKey::prefix_upper(property_id, value),
        ),
    }
}

fn intersect_u64_sets(mut sets: Vec<HashSet<u64>>) -> Vec<PostingHit> {
    if sets.is_empty() {
        return Vec::new();
    }
    sets.sort_by_key(|set| set.len());
    let mut intersection = sets[0].clone();
    for set in sets.iter().skip(1) {
        intersection = intersection.intersection(set).copied().collect();
        if intersection.is_empty() {
            return Vec::new();
        }
    }
    intersection
        .into_iter()
        .map(|packed| PostingHit {
            shard_id: ShardId::new((packed >> 32) as u32),
            vertex_id: (packed & 0xFFFF_FFFF) as u32,
        })
        .collect()
}

fn intersect_u128_sets(mut sets: Vec<HashSet<u128>>) -> HashSet<u128> {
    if sets.is_empty() {
        return HashSet::new();
    }
    sets.sort_by_key(|set| set.len());
    let mut intersection = sets[0].clone();
    for set in sets.iter().skip(1) {
        intersection = intersection.intersection(set).copied().collect();
        if intersection.is_empty() {
            return HashSet::new();
        }
    }
    intersection
}

fn unpack_edge_identity(packed: u128) -> EdgePostingHit {
    EdgePostingHit {
        shard_id: ShardId::new((packed >> 96) as u32),
        owner_vertex_id: ((packed >> 64) & 0xFFFF_FFFF) as u32,
        label_id: ((packed >> 32) & 0xFFFF) as u16,
        slot_index: (packed & 0xFFFF_FFFF) as u32,
    }
}
