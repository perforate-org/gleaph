//! Reverse adjacency consistency: the canonical source is the **forward** directed edges in
//! [`GRAPH`]; the reverse orientation (`REV_*` regions) is a derived mirror co-updated on edge
//! insert/delete. This oracle detects forward/reverse divergence (a forward edge with no reverse
//! in-edge half, or a reverse in-edge with no forward out-edge half) — the membership invariant
//! `reverse == projection of forward over local directed edges`.
//!
//! Scope:
//! - Only **directed**, **local→local** edges participate. Undirected edges live solely in the
//!   forward orientation, and an edge with a remote endpoint is intentionally one-sided in a shard.
//! - Tombstoned vertices are skipped symmetrically (an edge counts only when both endpoints are
//!   live), so an in-progress resumable delete does not register as divergence.
//!
//! Rebuild is intentionally **not** provided here. Unlike [`super::edge_alias`], rebuilding the
//! reverse orientation would reassign reverse slot indices, which cascade-invalidates
//! `EDGE_ALIASES` keys (derived from reverse slots) and reverse payload sidecars. A sound rebuild
//! is a larger, multi-store operation deferred to a future ADR.

use crate::facade::{GraphStore, GraphStoreError};
use gleaph_graph_kernel::entry::EdgeTarget;
use ic_stable_lara::{VertexId, labeled::OutEdgeOrder};
use std::collections::BTreeMap;

/// Identity of one directed local edge: the membership unit shared by the forward out-edge and the
/// reverse in-edge halves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DirectedEdgeKey {
    source_vertex_id: u32,
    target_vertex_id: u32,
    label_id: u16,
}

/// Divergence between forward directed edges and the derived reverse orientation. Each entry pairs
/// an edge identity with the count gap for that identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReverseAdjacencyInconsistency {
    /// Forward out-edges with too few reverse in-edge halves.
    pub missing_in_reverse: Vec<(DirectedEdgeKey, usize)>,
    /// Reverse in-edges with no (or too few) forward out-edge halves.
    pub extra_in_reverse: Vec<(DirectedEdgeKey, usize)>,
}

/// Returns a per-vertex liveness map: `live[i] == true` when vertex `i` exists and is not a
/// tombstone. Precomputed in its own pass so the edge scans (which already hold the `GRAPH` borrow)
/// never re-borrow it.
fn live_vertices(store: &GraphStore) -> Vec<bool> {
    let vertex_cap: u32 = store.vertex_count().into();
    let mut live = vec![false; vertex_cap as usize];
    for raw in 0..vertex_cap {
        live[raw as usize] = store
            .vertex(VertexId::from(raw))
            .is_some_and(|vertex| !vertex.is_tombstone());
    }
    live
}

fn collect_forward_directed_local(
    store: &GraphStore,
    live: &[bool],
) -> Result<BTreeMap<DirectedEdgeKey, usize>, GraphStoreError> {
    let mut counts = BTreeMap::new();
    for raw in 0..live.len() as u32 {
        if !live[raw as usize] {
            continue;
        }
        store.for_each_directed_out_edges(
            VertexId::from(raw),
            OutEdgeOrder::Ascending,
            |edge| {
                let Some(EdgeTarget::Local(target)) = edge.edge_target() else {
                    return;
                };
                if !live[u32::from(target) as usize] {
                    return;
                }
                *counts
                    .entry(DirectedEdgeKey {
                        source_vertex_id: raw,
                        target_vertex_id: u32::from(target),
                        label_id: edge.label_id,
                    })
                    .or_insert(0) += 1;
            },
        )?;
    }
    Ok(counts)
}

fn collect_reverse_directed_local(
    store: &GraphStore,
    live: &[bool],
) -> Result<BTreeMap<DirectedEdgeKey, usize>, GraphStoreError> {
    let mut counts = BTreeMap::new();
    for raw in 0..live.len() as u32 {
        if !live[raw as usize] {
            continue;
        }
        store.for_each_directed_in_edges(VertexId::from(raw), OutEdgeOrder::Ascending, |edge| {
            let Some(EdgeTarget::Local(source)) = edge.edge_target() else {
                return;
            };
            if !live[u32::from(source) as usize] {
                return;
            }
            *counts
                .entry(DirectedEdgeKey {
                    source_vertex_id: u32::from(source),
                    target_vertex_id: raw,
                    label_id: edge.label_id,
                })
                .or_insert(0) += 1;
        })?;
    }
    Ok(counts)
}

/// Returns `Ok(())` when the reverse orientation equals the projection of forward directed edges.
pub(crate) fn check_reverse_adjacency(
    store: &GraphStore,
) -> Result<(), ReverseAdjacencyInconsistency> {
    let live = live_vertices(store);
    let forward =
        collect_forward_directed_local(store, &live).expect("forward scan must succeed in check");
    let reverse =
        collect_reverse_directed_local(store, &live).expect("reverse scan must succeed in check");
    if forward == reverse {
        return Ok(());
    }
    let mut missing_in_reverse = Vec::new();
    for (key, &fwd) in &forward {
        let rev = reverse.get(key).copied().unwrap_or(0);
        if fwd > rev {
            missing_in_reverse.push((*key, fwd - rev));
        }
    }
    let mut extra_in_reverse = Vec::new();
    for (key, &rev) in &reverse {
        let fwd = forward.get(key).copied().unwrap_or(0);
        if rev > fwd {
            extra_in_reverse.push((*key, rev - fwd));
        }
    }
    Err(ReverseAdjacencyInconsistency {
        missing_in_reverse,
        extra_in_reverse,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::GRAPH;
    use crate::facade::store::helpers::{edge_storage_label, lara_label};
    use gleaph_graph_kernel::entry::{Edge, EdgePayload, EdgeSlotIndex, VertexRef};

    fn local_edge_to(target: VertexId) -> Edge {
        Edge {
            target: VertexRef::local(target),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            payload: EdgePayload::EMPTY,
        }
    }

    #[test]
    fn empty_stores_are_consistent() {
        let store = GraphStore::new();
        check_reverse_adjacency(&store).expect("empty reverse matches empty forward");
    }

    #[test]
    fn directed_edge_insert_keeps_reverse_consistent() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = crate::test_labels::edge_label_id_for_name("RevDirected");
        store
            .insert_directed_edge(source, target, Some(label_id))
            .expect("edge");

        check_reverse_adjacency(&store).expect("sync insert path mirrors into reverse");
    }

    #[test]
    fn forward_only_edge_is_detected_as_missing_reverse() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = crate::test_labels::edge_label_id_for_name("RevMissing");
        let label = lara_label(edge_storage_label(Some(label_id), false));

        // Inject a forward out-edge with no reverse half, simulating a co-update that committed
        // the forward write and then failed the reverse write.
        GRAPH.with_borrow(|graph| {
            graph
                .forward()
                .insert_edge(source, label, local_edge_to(target))
                .expect("inject forward-only edge");
        });

        let inconsistency =
            check_reverse_adjacency(&store).expect_err("forward-only edge must be detected");
        assert_eq!(inconsistency.missing_in_reverse.len(), 1);
        assert!(inconsistency.extra_in_reverse.is_empty());
        let (key, gap) = inconsistency.missing_in_reverse[0];
        assert_eq!(key.source_vertex_id, u32::from(source));
        assert_eq!(key.target_vertex_id, u32::from(target));
        assert_eq!(gap, 1);
    }

    #[test]
    fn reverse_orphan_is_detected_as_extra() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = crate::test_labels::edge_label_id_for_name("RevExtra");
        let label = lara_label(edge_storage_label(Some(label_id), false));

        // Inject a reverse in-edge at `target` from `source` with no canonical forward half.
        GRAPH.with_borrow(|graph| {
            graph
                .reverse()
                .insert_edge(target, label, local_edge_to(source))
                .expect("inject reverse-only edge");
        });

        let inconsistency =
            check_reverse_adjacency(&store).expect_err("reverse orphan must be detected");
        assert_eq!(inconsistency.extra_in_reverse.len(), 1);
        assert!(inconsistency.missing_in_reverse.is_empty());
        let (key, gap) = inconsistency.extra_in_reverse[0];
        assert_eq!(key.source_vertex_id, u32::from(source));
        assert_eq!(key.target_vertex_id, u32::from(target));
        assert_eq!(gap, 1);
    }
}
