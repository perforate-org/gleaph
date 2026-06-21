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
//! ## Differential repair
//!
//! [`rebuild_reverse_adjacency`] reconciles **only the diverged keys** reported by
//! [`check_reverse_adjacency`] against the forward source of truth, instead of clearing and
//! rebuilding the whole reverse orientation. A full rebuild would reassign every reverse slot
//! index, which cascade-invalidates `EDGE_ALIASES` keys (derived from reverse slots) and the
//! reverse payload slab wholesale (the reason a naive rebuild was rejected; see ADR 0026). The
//! differential repair removes each diverged key's reverse in-edge halves and their alias rows,
//! then re-inserts one reverse half per forward out-edge — copying the forward payload bytes and
//! recreating the directed reverse-IN alias exactly as the live insert path does in
//! `commit_directed_edge_insert`. Non-diverged keys keep their slots and aliases; edge properties
//! (keyed by canonical forward identity) are untouched.
//!
//! The repair is `pub(crate)` defense-in-depth with no canister endpoint: divergence is
//! near-unreachable because IC co-updates trap-and-roll-back atomically, so the repair set is
//! normally empty. For parallel directed edges sharing one `(src, tgt, label)` key, alias
//! precision matches the live insert path's existing first-match behavior; the membership
//! invariant checked here is always restored.

use super::super::stable::EDGE_ALIASES;
use crate::facade::store::helpers::{catalog_edge_label_from_wire, edge_alias_slot_key};
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};
use gleaph_graph_kernel::entry::{Edge, EdgePayload, EdgeSlotIndex, EdgeTarget, VertexRef};
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledError, VertexId,
    labeled::{LabeledEdgePayloadBatchScratch, OutEdgeOrder},
};
use std::collections::{BTreeMap, BTreeSet};

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

/// Reconciles the derived reverse orientation with the forward source of truth, repairing only the
/// directed keys [`check_reverse_adjacency`] reports as diverged. See the module docs for why this
/// is differential rather than a full clear-and-rebuild. No-op (and cheap) when already consistent.
pub(crate) fn rebuild_reverse_adjacency(store: &GraphStore) -> Result<(), GraphStoreError> {
    let Err(divergence) = check_reverse_adjacency(store) else {
        return Ok(());
    };
    let mut keys: BTreeSet<DirectedEdgeKey> = BTreeSet::new();
    for (key, _) in &divergence.missing_in_reverse {
        keys.insert(*key);
    }
    for (key, _) in &divergence.extra_in_reverse {
        keys.insert(*key);
    }
    for key in keys {
        reconcile_diverged_key(store, key)?;
    }
    Ok(())
}

/// Builds a reverse in-edge half pointing back at `source`, carrying `payload` (empty for unlabeled
/// or payload-free directed edges). Mirrors the reverse edge shape built by the live insert path.
fn reverse_edge_to(source: VertexId, payload: &[u8]) -> Edge {
    let edge = Edge {
        target: VertexRef::local(source),
        edge_slot_index: EdgeSlotIndex::from_raw(0),
        label_id: 0,
        payload: EdgePayload::EMPTY,
    };
    if payload.is_empty() {
        edge
    } else {
        edge.with_payload_bytes(payload)
    }
}

/// Captures the forward out-edges of one diverged key as `(forward_slot, payload_bytes)`, copying
/// payloads from the slab when the label carries them so the rebuilt reverse halves match exactly.
fn collect_forward_edges_for_key(
    store: &GraphStore,
    source: VertexId,
    target: VertexId,
    wire: LaraLabelId,
    payload_width: u16,
) -> Result<Vec<(u32, Vec<u8>)>, GraphStoreError> {
    let mut forward = Vec::new();
    if payload_width > 0 {
        let catalog = catalog_edge_label_from_wire(wire)
            .expect("non-zero payload width implies a catalog edge label");
        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        store.for_each_directed_out_edges_for_label_with_payload_slices_reusing(
            source,
            catalog,
            OutEdgeOrder::Ascending,
            &mut scratch,
            |edge, payload| {
                if matches!(edge.edge_target(), Some(EdgeTarget::Local(neighbor)) if neighbor == target)
                {
                    forward.push((edge.edge_slot_index.raw(), payload.to_vec()));
                }
            },
        )?;
    } else {
        store.for_each_directed_out_edges(source, OutEdgeOrder::Ascending, |edge| {
            if edge.label_id == wire.raw()
                && matches!(edge.edge_target(), Some(EdgeTarget::Local(neighbor)) if neighbor == target)
            {
                forward.push((edge.edge_slot_index.raw(), Vec::new()));
            }
        })?;
    }
    Ok(forward)
}

/// Makes `reverse[key] == forward[key]` for one diverged directed key by removing all of its reverse
/// in-edge halves (and alias rows) and re-inserting one reverse half per forward out-edge.
fn reconcile_diverged_key(store: &GraphStore, key: DirectedEdgeKey) -> Result<(), GraphStoreError> {
    let source = VertexId::from(key.source_vertex_id);
    let target = VertexId::from(key.target_vertex_id);
    let wire = LaraLabelId::from_raw(key.label_id);
    let payload_width = catalog_edge_label_from_wire(wire)
        .map(crate::edge_payload_schema::lookup_edge_payload_profile)
        .map(|profile| profile.required_byte_width())
        .unwrap_or(0);

    let forward = collect_forward_edges_for_key(store, source, target, wire, payload_width)?;

    // Drop the key's directed reverse-IN alias rows (keyed by reverse slot, valued by the forward
    // canonical slot) before the reverse slots they reference are reassigned below.
    EDGE_ALIASES.with_borrow_mut(|aliases| {
        for (forward_slot, _) in &forward {
            aliases.remove_all_for_canonical(source, wire.raw(), *forward_slot);
        }
    });

    // Remove every existing reverse in-edge half for this key.
    while store
        .with_graph_mut(|graph| {
            graph.remove_reverse_edge_matching(target, wire, |edge| {
                matches!(edge.edge_target(), Some(EdgeTarget::Local(neighbor)) if neighbor == source)
            })
        })?
        .is_some()
    {}

    // Re-insert one reverse half per forward out-edge and recreate its directed alias, matching the
    // live `commit_directed_edge_insert` sequence.
    for (forward_slot, payload) in &forward {
        store.with_graph_mut(|graph| {
            if payload_width > 0 {
                graph
                    .reverse()
                    .ensure_label_bucket_payload_byte_width(target, wire, payload_width)
                    .map_err(DeferredBidirectionalLabeledError::Reverse)?;
            }
            graph
                .reverse()
                .insert_edge(target, wire, reverse_edge_to(source, payload))
                .map_err(DeferredBidirectionalLabeledError::Reverse)
        })?;
        let canonical = EdgeHandle::at_slot(source, wire, *forward_slot);
        if let Some(alias) = store.find_reverse_alias_for_canonical(canonical, target, source)? {
            EDGE_ALIASES.with_borrow_mut(|aliases| {
                aliases.insert(
                    alias.owner_vertex_id,
                    wire.raw(),
                    edge_alias_slot_key(alias.slot_index, true),
                    source,
                    *forward_slot,
                );
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::GRAPH;
    use crate::facade::store::helpers::{edge_storage_label, lara_label};
    use gleaph_graph_kernel::entry::{Edge, EdgePayload, EdgeSlotIndex, VertexRef};
    use ic_stable_lara::traits::CsrEdge;

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

    #[test]
    fn repairs_forward_only_edge_into_reverse() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = crate::test_labels::edge_label_id_for_name("RevRepairMissing");
        let label = lara_label(edge_storage_label(Some(label_id), false));

        GRAPH.with_borrow(|graph| {
            graph
                .forward()
                .insert_edge(source, label, local_edge_to(target))
                .expect("inject forward-only edge");
        });
        assert!(check_reverse_adjacency(&store).is_err());

        rebuild_reverse_adjacency(&store).expect("repair");

        check_reverse_adjacency(&store).expect("repair restores reverse half");
        let in_edges = store.directed_in_edges(target).expect("in edges");
        assert_eq!(in_edges.len(), 1);
        assert_eq!(in_edges[0].neighbor_vid(), source);
    }

    #[test]
    fn repairs_reverse_orphan_by_removal() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = crate::test_labels::edge_label_id_for_name("RevRepairExtra");
        let label = lara_label(edge_storage_label(Some(label_id), false));

        GRAPH.with_borrow(|graph| {
            graph
                .reverse()
                .insert_edge(target, label, local_edge_to(source))
                .expect("inject reverse-only edge");
        });
        assert!(check_reverse_adjacency(&store).is_err());

        rebuild_reverse_adjacency(&store).expect("repair");

        check_reverse_adjacency(&store).expect("repair removes reverse orphan");
        assert!(
            store
                .directed_in_edges(target)
                .expect("in edges")
                .is_empty()
        );
    }

    #[test]
    fn rebuild_preserves_edge_payload() {
        use gleaph_graph_kernel::entry::{EdgePayloadProfile, EdgeWeightProfile, WeightEncoding};

        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = crate::test_labels::edge_label_id_for_name("RevRepairPayload");
        crate::test_labels::install_test_edge_payload_profile(
            label_id,
            EdgePayloadProfile::from(EdgeWeightProfile {
                encoding: WeightEncoding::RawU16,
            }),
        );
        let label = lara_label(edge_storage_label(Some(label_id), false));
        let payload = 0xBEEFu16.to_le_bytes();

        store
            .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &payload)
            .expect("payloaded edge");

        // Corrupt: drop the reverse half so the forward payload edge has no reverse mirror.
        GRAPH.with_borrow(|graph| {
            graph
                .remove_reverse_edge_matching(
                    target,
                    label,
                    |edge| matches!(edge.edge_target(), Some(EdgeTarget::Local(n)) if n == source),
                )
                .expect("remove reverse half");
        });
        assert!(check_reverse_adjacency(&store).is_err());

        rebuild_reverse_adjacency(&store).expect("repair");

        check_reverse_adjacency(&store).expect("repair restores reverse half");
        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        let mut restored = None;
        store
            .for_each_directed_in_edges_for_label_with_payload_slices_reusing(
                target,
                label_id,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |edge, bytes| {
                    if matches!(edge.edge_target(), Some(EdgeTarget::Local(n)) if n == source) {
                        restored = Some(bytes.to_vec());
                    }
                },
            )
            .expect("read reverse payload");
        assert_eq!(restored.as_deref(), Some(&payload[..]));
    }

    #[test]
    fn rebuild_is_noop_when_consistent() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = crate::test_labels::edge_label_id_for_name("RevRepairNoop");
        let label = lara_label(edge_storage_label(Some(label_id), false));
        store
            .insert_directed_edge(source, target, Some(label_id))
            .expect("edge");

        let before = store
            .find_first_reverse_handle_descending(target, label, |edge| {
                edge.neighbor_vid() == source
            })
            .expect("lookup")
            .expect("reverse half");

        rebuild_reverse_adjacency(&store).expect("repair");

        check_reverse_adjacency(&store).expect("still consistent");
        let after = store
            .find_first_reverse_handle_descending(target, label, |edge| {
                edge.neighbor_vid() == source
            })
            .expect("lookup")
            .expect("reverse half");
        assert_eq!(
            before, after,
            "no-op repair must not reassign reverse slots"
        );
    }

    #[test]
    fn rebuild_leaves_unrelated_reverse_slots_untouched() {
        let store = GraphStore::new();
        let keep_source = store.insert_vertex().expect("keep source");
        let repair_source = store.insert_vertex().expect("repair source");
        let target = store.insert_vertex().expect("shared target");
        let label_id = crate::test_labels::edge_label_id_for_name("RevRepairUnrelated");
        let label = lara_label(edge_storage_label(Some(label_id), false));

        // A healthy directed edge whose reverse half/alias must survive the repair untouched.
        store
            .insert_directed_edge(keep_source, target, Some(label_id))
            .expect("kept edge");
        let kept_before = store
            .find_first_reverse_handle_descending(target, label, |edge| {
                edge.neighbor_vid() == keep_source
            })
            .expect("lookup")
            .expect("kept reverse half");
        let kept_alias_before = store.canonical_reverse_in_edge_handle(kept_before);

        // A separate diverged key (forward-only) sharing the target's reverse bucket.
        GRAPH.with_borrow(|graph| {
            graph
                .forward()
                .insert_edge(repair_source, label, local_edge_to(target))
                .expect("inject forward-only edge");
        });
        assert!(check_reverse_adjacency(&store).is_err());

        rebuild_reverse_adjacency(&store).expect("repair");

        check_reverse_adjacency(&store).expect("repair restores the diverged key");
        let kept_after = store
            .find_first_reverse_handle_descending(target, label, |edge| {
                edge.neighbor_vid() == keep_source
            })
            .expect("lookup")
            .expect("kept reverse half survives");
        assert_eq!(
            kept_before, kept_after,
            "unrelated key's reverse slot must be preserved"
        );
        assert_eq!(
            kept_alias_before,
            store.canonical_reverse_in_edge_handle(kept_after),
            "unrelated key's alias must be preserved"
        );
    }
}
