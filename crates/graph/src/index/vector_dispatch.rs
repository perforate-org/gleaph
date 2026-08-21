//! Derives `vector-canister` mutations from DML-driven removes (ADR 0064).
//!
//! The graph no longer stores embedding bytes (ADR 0064 §1), so it cannot dispatch upserts from its
//! own canonical store — the Router sends bytes + stamp directly to the vector canister. The graph
//! only dispatches **removes** for DML-driven deletions (vertex delete, label loss), gated by the
//! ephemeral router-sourced catalog ([`crate::index::vector_catalog_context`]). Because the graph
//! cannot enumerate a vertex's embeddings, it over-notifies by dispatching a remove for every indexed
//! name; a remove for a missing subject writes a deleted subject clock without creating a live
//! row, so over-notification remains safe and stale replays stay fenced.

use crate::facade::GraphStore;
use crate::index::vector_pending;
use gleaph_graph_kernel::entry::VertexLabelId;
use gleaph_graph_kernel::vector_index::{VectorEmbeddingSyncOp, VectorSubject};
use ic_stable_lara::VertexId;

fn vertex_id_raw(vertex_id: VertexId) -> u32 {
    u32::try_from(u64::from(vertex_id)).unwrap_or(0)
}

/// Queues a remove for every indexed embedding name (vertex-delete sidecar clear). The graph no
/// longer stores embedding bytes, so it cannot enumerate a vertex's embeddings; it over-notifies by
/// dispatching a remove for every indexed name. The op carries the DML `mutation_id` so the canister's
/// stamp-fenced clock supersedes a stale same-stamp replay without tombstoning a newer reinsert
/// (`bytes` is empty on remove).
pub(crate) fn dispatch_vertex_removes_for_all_indexed(vertex_id: VertexId, mutation_id: u64) {
    let Some(routing) = GraphStore::new().federation_routing() else {
        return;
    };
    for spec in crate::index::vector_catalog_context::specs() {
        vector_pending::push_vector_op(VectorEmbeddingSyncOp {
            index_id: spec.index_id,
            embedding_name_id: spec.embedding_name_id,
            subject: VectorSubject::Vertex {
                shard_id: routing.shard_id,
                vertex_id: vertex_id_raw(vertex_id),
            },
            mutation_id,
            encoding: spec.encoding,
            dims: spec.dims,
            metric: spec.metric,
            bytes: Vec::new(),
            remove: true,
        });
    }
}

/// Queues a remove for each index the vertex **fell out of** after losing `removed_label` (ADR 0064
/// §DML-driven removes: "dispatches removes for indexes the vertex fell out of").
///
/// The vertex fell out of an index iff the index's creation-fixed label set includes `removed_label`
/// (so the vertex qualified before) and none of `remaining_labels` (the vertex's labels after the
/// removal) is in the index's label set. Indexes the vertex still qualifies for are skipped, so a
/// label loss does not drop an embedding the vertex still merits. The op carries the DML `mutation_id`.
pub(crate) fn dispatch_vertex_removes_for_label_loss(
    vertex_id: VertexId,
    removed_label: VertexLabelId,
    remaining_labels: &[VertexLabelId],
    mutation_id: u64,
) {
    let Some(routing) = GraphStore::new().federation_routing() else {
        return;
    };
    for spec in crate::index::vector_catalog_context::specs_for_label(removed_label) {
        let still_qualifies = remaining_labels.iter().any(|l| spec.labels.contains(l));
        if still_qualifies {
            continue;
        }
        vector_pending::push_vector_op(VectorEmbeddingSyncOp {
            index_id: spec.index_id,
            embedding_name_id: spec.embedding_name_id,
            subject: VectorSubject::Vertex {
                shard_id: routing.shard_id,
                vertex_id: vertex_id_raw(vertex_id),
            },
            mutation_id,
            encoding: spec.encoding,
            dims: spec.dims,
            metric: spec.metric,
            bytes: Vec::new(),
            remove: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use crate::index::{vector_catalog_context, vector_pending};
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{IndexedEmbeddingSpec, VectorIndexKind, VectorMetric};

    fn spec(name: u16) -> IndexedEmbeddingSpec {
        IndexedEmbeddingSpec {
            embedding_name_id: name,
            index_id: 7,
            kind: VectorIndexKind::IvfFlat,
            metric: VectorMetric::L2Squared,
            encoding: gleaph_graph_kernel::vector_index::VectorEncoding::F32,
            dims: 2,
            labels: vec![gleaph_graph_kernel::entry::VertexLabelId::from_raw(1)],
        }
    }

    fn spec_with_labels(
        name: u16,
        index_id: u32,
        labels: Vec<gleaph_graph_kernel::entry::VertexLabelId>,
    ) -> IndexedEmbeddingSpec {
        IndexedEmbeddingSpec {
            embedding_name_id: name,
            index_id,
            kind: VectorIndexKind::IvfFlat,
            metric: VectorMetric::L2Squared,
            encoding: gleaph_graph_kernel::vector_index::VectorEncoding::F32,
            dims: 2,
            labels,
        }
    }

    fn with_routing<R>(body: impl FnOnce(&GraphStore) -> R) -> R {
        let graph = GraphStore::new();
        graph
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_canister: Some(Principal::management_canister()),
            }))
            .expect("set routing");
        vector_pending::clear_pending();
        let out = body(&graph);
        vector_pending::clear_pending();
        graph.set_federation_routing(None).expect("clear routing");
        out
    }

    #[test]
    fn vertex_delete_dispatches_remove_for_every_indexed_name() {
        with_routing(|store| {
            let vid = store.insert_vertex().expect("vertex");
            let _guard = vector_catalog_context::enter_indexed(&[spec(1), spec(2)]);
            dispatch_vertex_removes_for_all_indexed(vid, 7);
            let ops = vector_pending::pending_snapshot();
            assert_eq!(ops.len(), 2);
            assert!(ops.iter().all(|op| op.remove));
            assert!(ops.iter().all(|op| op.bytes.is_empty()));
            assert!(ops.iter().all(|op| op.mutation_id == 7));
            assert!(ops.iter().all(|op| op.index_id == 7));
        });
    }

    #[test]
    fn vertex_delete_with_no_catalog_dispatches_nothing() {
        with_routing(|store| {
            let vid = store.insert_vertex().expect("vertex");
            dispatch_vertex_removes_for_all_indexed(vid, 7);
            assert!(vector_pending::pending_snapshot().is_empty());
        });
    }

    #[test]
    fn label_loss_dispatches_remove_for_fell_out_index() {
        with_routing(|store| {
            let vid = store.insert_vertex().expect("vertex");
            let l1 = gleaph_graph_kernel::entry::VertexLabelId::from_raw(1);
            let l2 = gleaph_graph_kernel::entry::VertexLabelId::from_raw(2);
            // Index 7 is scoped to label 1; index 8 to label 2. The vertex loses label 1 and has no
            // remaining labels, so it fell out of index 7 only.
            let _guard = vector_catalog_context::enter_indexed(&[
                spec_with_labels(1, 7, vec![l1]),
                spec_with_labels(2, 8, vec![l2]),
            ]);
            dispatch_vertex_removes_for_label_loss(vid, l1, &[], 7);
            let ops = vector_pending::pending_snapshot();
            assert_eq!(ops.len(), 1);
            assert_eq!(ops[0].index_id, 7);
            assert!(ops[0].remove);
            assert!(ops[0].bytes.is_empty());
            assert_eq!(ops[0].mutation_id, 7);
        });
    }

    #[test]
    fn label_loss_skips_index_vertex_still_qualifies_for() {
        with_routing(|store| {
            let vid = store.insert_vertex().expect("vertex");
            let l1 = gleaph_graph_kernel::entry::VertexLabelId::from_raw(1);
            let l2 = gleaph_graph_kernel::entry::VertexLabelId::from_raw(2);
            // Index 7 is scoped to {1, 2}. The vertex loses label 1 but still has label 2, so it
            // still qualifies for index 7 and no remove is dispatched.
            let _guard =
                vector_catalog_context::enter_indexed(&[spec_with_labels(1, 7, vec![l1, l2])]);
            dispatch_vertex_removes_for_label_loss(vid, l1, &[l2], 7);
            assert!(vector_pending::pending_snapshot().is_empty());
        });
    }

    #[test]
    fn label_loss_with_no_catalog_dispatches_nothing() {
        with_routing(|store| {
            let vid = store.insert_vertex().expect("vertex");
            let l1 = gleaph_graph_kernel::entry::VertexLabelId::from_raw(1);
            dispatch_vertex_removes_for_label_loss(vid, l1, &[], 7);
            assert!(vector_pending::pending_snapshot().is_empty());
        });
    }
}
