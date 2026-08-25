//! Derives `text-canister` document mutations from canonical DML (plan 0297, ADR 0077).
//!
//! Document keys are the caller-owned `u64` vertex identity, and values ship **raw** — analysis
//! happens canister-side, so unlike vector dispatch ([`crate::index::vector_dispatch`]) the graph
//! *can* derive upserts from its own canonical store. A transition therefore yields exactly one
//! op: an upsert when the property is text-valued after the change (re-ingest supersedes any
//! previous document for the same key, so no tombstone for the old value is needed), and a delete
//! when it stopped being text-valued (removed, or retyped to a non-text value).
//!
//! Label-scoped membership changes ride the same rails as property postings: the label machinery
//! synthesizes value transitions (gain: absent→value; loss: value→absent) that reach
//! [`dispatch_vertex_property_change`] like any write. Whole-vertex deletion has no per-property
//! transitions, so [`dispatch_vertex_removes_for_all_indexed`] covers it with a single delete op;
//! over-notification is safe because unknown-key deletes are deterministic no-ops at flush.
//!
//! Everything is gated by the ephemeral router-sourced catalog
//! ([`crate::index::text_catalog_context`]): with no catalog installed (the production posture
//! until the Router TEXT install lands) dispatch is inert.
//!
//! ## Dormancy (deliberate, scope-forced)
//!
//! The property-write hook sites (`crate::property::index_dispatch` paths) are owned by another
//! slice's boundary, so no production caller invokes these functions yet; they are exercised by
//! unit tests. The `allow(dead_code)` below covers only that window.

#![cfg_attr(not(test), allow(dead_code))]

use crate::facade::GraphStore;
use crate::index::text_catalog_context::{self, IndexedTextSpec};
use crate::index::text_pending::{
    self, TEXT_INGEST_MAX_TEXT_BYTES_PER_DOC, TextPendingOp, TextPendingOpKind,
};
use crate::property::PropertyValueChange;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyEntity;
use ic_stable_lara::VertexId;

/// Enqueues the document mutation implied by one canonical **vertex** property transition.
///
/// Edges are ignored: edge-property text indexes are a documented v1 non-goal
/// (`design/index/text-index.md` §Non-goals).
pub(crate) fn dispatch_vertex_property_change(change: PropertyValueChange<'_>) {
    let PropertyEntity::Vertex(vertex_id) = change.entity else {
        return;
    };
    if !spec_applies(change.property_id, vertex_id) {
        return;
    }
    let key = u64::from(vertex_id);
    match (change.prev, change.new) {
        // No-op rewrite: the confirmed document already holds this exact text.
        (Some(prev), Some(new)) if prev == new => {}
        // Text-valued after the change (created or overwritten): one idempotent upsert keyed by
        // the vertex identity replaces whatever was indexed before.
        (_, Some(Value::Text(text))) => {
            // Oversized values would be rejected by the canister's whole-batch admission check,
            // permanently stalling every batch containing them; they stay canonical-only
            // (under-posted, see text_pending module docs) instead.
            if text.len() <= TEXT_INGEST_MAX_TEXT_BYTES_PER_DOC {
                text_pending::push_text_op(TextPendingOp {
                    key,
                    kind: TextPendingOpKind::Upsert { text: text.clone() },
                });
            }
        }
        // The property stopped being text-valued (removed or retyped): retire the document.
        // Retyping from a non-text value falls through: nothing text-indexed existed before.
        (Some(Value::Text(_)), _) => {
            text_pending::push_text_op(TextPendingOp {
                key,
                kind: TextPendingOpKind::Delete,
            });
        }
        _ => {}
    }
}

/// Queues one delete covering every indexed text property of `vertex_id` (vertex-delete sidecar
/// clear). Safe to call unconditionally on the catalog gate alone: unknown-key deletes are
/// deterministic no-ops at flush, so over-notifying vertices without text documents is harmless.
pub(crate) fn dispatch_vertex_removes_for_all_indexed(vertex_id: VertexId) {
    if !text_catalog_context::has_specs() {
        return;
    }
    text_pending::push_text_op(TextPendingOp {
        key: u64::from(vertex_id),
        kind: TextPendingOpKind::Delete,
    });
}

/// Resolves whether any installed spec indexes `property_id` on a vertex carrying `vertex_id`'s
/// current labels. A missing vertex row has no document state to maintain.
fn spec_applies(property_id: gleaph_graph_kernel::entry::PropertyId, vertex_id: VertexId) -> bool {
    let specs: Vec<IndexedTextSpec> = text_catalog_context::specs_for_property(property_id);
    if specs.is_empty() {
        return false;
    }
    let store = GraphStore::new();
    let Some(vertex) = store.vertex(vertex_id) else {
        return false;
    };
    let labels = store.vertex_labels(vertex_id, vertex);
    specs.iter().any(|spec| spec.matches_labels(&labels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use crate::index::text_pending::TEXT_INGEST_MAX_TEXT_BYTES_PER_DOC;
    use candid::Principal;
    use gleaph_graph_kernel::entry::{PropertyId, VertexLabelId};
    use gleaph_graph_kernel::federation::ShardId;

    fn prop(id: u32) -> PropertyId {
        PropertyId::from_raw(id)
    }

    fn label(id: u16) -> VertexLabelId {
        VertexLabelId::from_raw(id)
    }

    fn spec(property_id: u32, labels: &[u16]) -> IndexedTextSpec {
        IndexedTextSpec {
            property_id: prop(property_id),
            labels: labels.iter().copied().map(label).collect(),
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
        text_pending::clear_pending();
        crate::index::label_pending::clear_pending();
        let out = body(&graph);
        text_pending::clear_pending();
        crate::index::label_pending::clear_pending();
        graph.set_federation_routing(None).expect("clear routing");
        out
    }

    /// Inserts a labeled vertex and returns its id.
    fn labeled_vertex(store: &GraphStore, label_id: u16) -> VertexId {
        let vid = store.insert_vertex().expect("vertex");
        let vertex = store.vertex(vid).expect("vertex row");
        store
            .add_vertex_label(vid, vertex, label(label_id))
            .expect("label");
        vid
    }

    fn change<'a>(
        vertex_id: VertexId,
        property_id: u32,
        prev: Option<&'a Value>,
        new: Option<&'a Value>,
    ) -> PropertyValueChange<'a> {
        PropertyValueChange::vertex(vertex_id, prop(property_id), prev, new)
    }

    #[test]
    fn text_upsert_enqueues_raw_value_keyed_by_vertex() {
        with_routing(|store| {
            let vid = labeled_vertex(store, 1);
            let _guard = text_catalog_context::enter_indexed(&[spec(10, &[1])]);
            let value = Value::Text("hello world".into());
            dispatch_vertex_property_change(change(vid, 10, None, Some(&value)));
            assert_eq!(
                text_pending::pending_snapshot(),
                vec![TextPendingOp {
                    key: u64::from(vid),
                    kind: TextPendingOpKind::Upsert {
                        text: "hello world".into()
                    },
                }]
            );
        });
    }

    #[test]
    fn overwrite_is_a_single_upsert_without_tombstone() {
        with_routing(|store| {
            let vid = labeled_vertex(store, 1);
            let _guard = text_catalog_context::enter_indexed(&[spec(10, &[1])]);
            let old = Value::Text("old".into());
            let new = Value::Text("new".into());
            dispatch_vertex_property_change(change(vid, 10, Some(&old), Some(&new)));
            let ops = text_pending::pending_snapshot();
            assert_eq!(ops.len(), 1);
            assert_eq!(
                ops[0],
                TextPendingOp {
                    key: u64::from(vid),
                    kind: TextPendingOpKind::Upsert { text: "new".into() },
                }
            );
        });
    }

    #[test]
    fn property_remove_enqueues_delete() {
        with_routing(|store| {
            let vid = labeled_vertex(store, 1);
            let _guard = text_catalog_context::enter_indexed(&[spec(10, &[1])]);
            let old = Value::Text("gone".into());
            dispatch_vertex_property_change(change(vid, 10, Some(&old), None));
            assert_eq!(
                text_pending::pending_snapshot(),
                vec![TextPendingOp {
                    key: u64::from(vid),
                    kind: TextPendingOpKind::Delete,
                }]
            );
        });
    }

    #[test]
    fn retype_from_text_to_non_text_enqueues_delete() {
        with_routing(|store| {
            let vid = labeled_vertex(store, 1);
            let _guard = text_catalog_context::enter_indexed(&[spec(10, &[1])]);
            let old = Value::Text("42".into());
            let new = Value::Bool(true);
            dispatch_vertex_property_change(change(vid, 10, Some(&old), Some(&new)));
            assert_eq!(
                text_pending::pending_snapshot(),
                vec![TextPendingOp {
                    key: u64::from(vid),
                    kind: TextPendingOpKind::Delete,
                }]
            );
        });
    }

    #[test]
    fn unchanged_text_and_non_text_transitions_enqueue_nothing() {
        with_routing(|store| {
            let vid = labeled_vertex(store, 1);
            let _guard = text_catalog_context::enter_indexed(&[spec(10, &[1])]);
            let same = Value::Text("same".into());
            dispatch_vertex_property_change(change(vid, 10, Some(&same), Some(&same)));
            let non_text = Value::Bool(false);
            dispatch_vertex_property_change(change(vid, 10, None, Some(&non_text)));
            dispatch_vertex_property_change(change(vid, 10, Some(&non_text), None));
            assert!(text_pending::pending_snapshot().is_empty());
        });
    }

    #[test]
    fn oversized_text_is_under_posted_not_queued() {
        with_routing(|store| {
            let vid = labeled_vertex(store, 1);
            let _guard = text_catalog_context::enter_indexed(&[spec(10, &[1])]);
            let big = Value::Text("x".repeat(TEXT_INGEST_MAX_TEXT_BYTES_PER_DOC + 1));
            dispatch_vertex_property_change(change(vid, 10, None, Some(&big)));
            assert!(text_pending::pending_snapshot().is_empty());

            // Exactly at the cap is admitted (canister admission is `>`).
            let at_cap = Value::Text("y".repeat(TEXT_INGEST_MAX_TEXT_BYTES_PER_DOC));
            dispatch_vertex_property_change(change(vid, 10, None, Some(&at_cap)));
            assert_eq!(text_pending::pending_snapshot().len(), 1);
        });
    }

    #[test]
    fn without_catalog_dispatch_is_inert() {
        with_routing(|store| {
            let vid = labeled_vertex(store, 1);
            let value = Value::Text("no catalog".into());
            dispatch_vertex_property_change(change(vid, 10, None, Some(&value)));
            dispatch_vertex_removes_for_all_indexed(vid);
            assert!(text_pending::pending_snapshot().is_empty());
        });
    }

    #[test]
    fn label_scope_gates_dispatch() {
        with_routing(|store| {
            // Spec scoped to label 2; the vertex carries only label 1 → no doc op.
            let out_of_scope = labeled_vertex(store, 1);
            let _guard = text_catalog_context::enter_indexed(&[spec(10, &[2])]);
            let value = Value::Text("scoped".into());
            dispatch_vertex_property_change(change(out_of_scope, 10, None, Some(&value)));
            assert!(text_pending::pending_snapshot().is_empty());

            // A vertex whose labels intersect the scope is dispatched.
            let in_scope = labeled_vertex(store, 2);
            dispatch_vertex_property_change(change(in_scope, 10, None, Some(&value)));
            assert_eq!(text_pending::pending_snapshot().len(), 1);
        });
    }

    #[test]
    fn unindexed_property_and_missing_row_enqueue_nothing() {
        with_routing(|store| {
            let _guard = text_catalog_context::enter_indexed(&[spec(10, &[])]);
            let vid = labeled_vertex(store, 1);
            let value = Value::Text("unindexed prop".into());
            dispatch_vertex_property_change(change(vid, 99, None, Some(&value)));

            let missing = VertexId::from(u32::from(vid) + 1000);
            dispatch_vertex_property_change(change(missing, 10, None, Some(&value)));
            assert!(text_pending::pending_snapshot().is_empty());
        });
    }

    #[test]
    fn edge_property_changes_are_ignored() {
        with_routing(|_store| {
            let _guard = text_catalog_context::enter_indexed(&[spec(10, &[])]);
            let owner = VertexId::from(1u32);
            let value = Value::Text("edge".into());
            let edge_change = PropertyValueChange::edge(owner, 0, 0, prop(10), None, Some(&value));
            dispatch_vertex_property_change(edge_change);
            assert!(text_pending::pending_snapshot().is_empty());
        });
    }

    #[test]
    fn vertex_delete_enqueues_one_delete_for_the_whole_vertex() {
        with_routing(|store| {
            let vid = labeled_vertex(store, 1);
            let _guard = text_catalog_context::enter_indexed(&[spec(10, &[1]), spec(20, &[1])]);
            dispatch_vertex_removes_for_all_indexed(vid);
            // One op covers both indexes: the doc key is the vertex identity itself.
            assert_eq!(
                text_pending::pending_snapshot(),
                vec![TextPendingOp {
                    key: u64::from(vid),
                    kind: TextPendingOpKind::Delete,
                }]
            );
        });
    }

    #[test]
    fn push_is_inert_without_federation_routing() {
        let graph = GraphStore::new();
        graph.set_federation_routing(None).expect("clear routing");
        text_pending::clear_pending();
        let vid = graph.insert_vertex().expect("vertex");
        let _guard = text_catalog_context::enter_indexed(&[spec(10, &[])]);
        let value = Value::Text("unrouted".into());
        dispatch_vertex_property_change(change(vid, 10, None, Some(&value)));
        dispatch_vertex_removes_for_all_indexed(vid);
        assert!(text_pending::pending_snapshot().is_empty());
    }
}
