//! Routes derived index operations to federated vertex or local edge backends.

use gleaph_gql::Value;
use gleaph_graph_kernel::canonical_export::CanonicalExportError;
use gleaph_graph_kernel::entry::{PropertyEntity, PropertyId};
use gleaph_graph_kernel::index::IndexBuildSubject;
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_lara::VertexId;

use super::{PropertyValueChange, index_ops_for_value_change};
use crate::facade::{FencedTransition, GraphStore, GraphStoreError, PlannedBuildEnvelope};
use crate::index::catalog_context::IndexMembershipRef;

/// Applies index-maintenance operations implied by a primary-store property change.
pub(crate) fn dispatch_property_index_ops(change: PropertyValueChange<'_>) {
    // Text-document sync (plan 0297) is independent derived state: derive exactly once per
    // canonical change, before property-index namespace fan-out, so it neither duplicates per
    // membership nor depends on any property-index namespace existing for this property. It
    // self-gates on the ephemeral TEXT catalog and ignores edge entities (v1 non-goal).
    crate::index::text_dispatch::dispatch_vertex_property_change(change);
    match change.entity {
        PropertyEntity::Vertex(_) => {
            for transition in vertex_posting_transitions(&change) {
                let derived = PropertyValueChange {
                    entity: change.entity,
                    property_id: transition.property_id,
                    prev: transition.prev,
                    new: transition.new,
                };
                dispatch_property_postings_for_physical(derived, transition.membership);
            }
        }
        PropertyEntity::Edge { .. } => {
            for membership in memberships_for_change(change) {
                dispatch_property_postings_for_physical(change, membership);
            }
        }
    }
}

/// Applies one property transition to one exact Router-allocated namespace.
///
/// Direct callers — the label-transition path, inline decode, and sidecar/move observers —
/// arrive here with an already-resolved membership. Text derivation runs first because text
/// sync is independent derived state: it must not be suppressed by, or duplicated across,
/// property-index lifecycle fences. Callers routed through
/// [`dispatch_property_index_ops`] already had their text op derived once and land in
/// [`dispatch_property_postings_for_physical`] directly.
///
/// Inline-property decoding already resolves the concrete membership before dispatch, so it
/// uses this narrow helper to avoid re-expanding a membership into duplicate posting operations.
pub(crate) fn dispatch_property_index_ops_for_physical(
    change: PropertyValueChange<'_>,
    membership: IndexMembershipRef,
) {
    crate::index::text_dispatch::dispatch_vertex_property_change(change);
    dispatch_property_postings_for_physical(change, membership);
}

/// Posting-only half of [`dispatch_property_index_ops_for_physical`] (no text derivation).
fn dispatch_property_postings_for_physical(
    change: PropertyValueChange<'_>,
    membership: IndexMembershipRef,
) {
    // Building/Sealing work is admitted before the canonical write and persisted directly in the
    // Memory46 outbox. Never let it fall through to the ordinary Active-only queue.
    if !membership.phase.is_active() {
        return;
    }
    let ops = index_ops_for_value_change(change.property_id, change.prev, change.new);
    if ops.is_empty() {
        return;
    }
    match change.entity {
        PropertyEntity::Vertex(vertex_id) => {
            for op in ops {
                crate::index::pending::push_vertex_index_op(vertex_id, membership, op);
            }
        }
        PropertyEntity::Edge {
            owner_vertex_id,
            label_id,
            slot_index,
        } => {
            for op in ops {
                crate::index::edge_pending::push_edge_index_op(
                    owner_vertex_id,
                    label_id,
                    slot_index,
                    membership,
                    op,
                );
            }
        }
    }
}

/// Resolves the exact Router-allocated namespaces maintained for one property transition.
fn memberships_for_change(change: PropertyValueChange<'_>) -> Vec<IndexMembershipRef> {
    match change.entity {
        PropertyEntity::Vertex(_) => vertex_posting_transitions(&change)
            .into_iter()
            .map(|transition| transition.membership)
            .collect(),
        PropertyEntity::Edge { label_id, .. } => {
            crate::index::catalog_context::edge_index_memberships(label_id, change.property_id)
        }
    }
}

/// One per-namespace posting transition derived from a canonical vertex property change.
///
/// Flat memberships carry the change values unchanged. Nested record memberships carry the
/// leaf value walked from the record along the declared path and post under their interned
/// leaf identity (ADR 0073 §2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VertexPostingTransition<'a> {
    pub(crate) membership: IndexMembershipRef,
    pub(crate) property_id: PropertyId,
    pub(crate) prev: Option<&'a Value>,
    pub(crate) new: Option<&'a Value>,
}

/// Expands one canonical vertex property change into per-namespace posting transitions.
///
/// The single write path, the bulk path, the delete planning path, and the build fence all
/// resolve through this one owner, so Active dispatch, Sealing rejection, Building admission,
/// and removals always agree on the affected namespace set and on each namespace's posting
/// identity.
pub(crate) fn vertex_posting_transitions<'a>(
    change: &PropertyValueChange<'a>,
) -> Vec<VertexPostingTransition<'a>> {
    let PropertyEntity::Vertex(vertex_id) = change.entity else {
        return Vec::new();
    };
    let store = GraphStore::new();
    // A missing vertex row has no posting state to maintain; only a live row resolves
    // memberships (an empty label set there means the legacy-unlabeled wildcard rule).
    let Some(vertex) = store.vertex(vertex_id) else {
        return Vec::new();
    };
    let labels = store.vertex_labels(vertex_id, vertex);
    crate::index::catalog_context::vertex_index_targets_for_labels(&labels, change.property_id)
        .into_iter()
        .map(|target| {
            if target.field_tail.is_empty() {
                VertexPostingTransition {
                    membership: target.membership,
                    property_id: target.posting_property_id,
                    prev: change.prev,
                    new: change.new,
                }
            } else {
                let walk = |value: Option<&'a Value>| {
                    value
                        .and_then(|value| {
                            crate::property::record_value_at_dotted_path(value, &target.field_tail)
                        })
                        .and_then(crate::property::nested_leaf_posting_value)
                };
                VertexPostingTransition {
                    membership: target.membership,
                    property_id: target.posting_property_id,
                    prev: walk(change.prev),
                    new: walk(change.new),
                }
            }
        })
        .collect()
}

/// Pure admission planning for one property transition (the preflight half of the fence).
///
/// Resolves the exact catalog memberships maintained for the change and delegates to the common
/// GraphStore admission owner. Any Sealing membership rejects with `RetryableSealing` and any
/// Building scope failure rejects before any canonical write; no sequence is reserved. Returns
/// the planned Building envelopes; the caller must bind the exact canonical subject and run
/// [`commit_property_index_ops`] before the first canonical store mutation.
pub(crate) fn preflight_property_index_ops(
    change: PropertyValueChange<'_>,
) -> Result<Vec<PlannedBuildEnvelope>, GraphStoreError> {
    let transitions = match change.entity {
        PropertyEntity::Vertex(_) => vertex_posting_transitions(&change)
            .into_iter()
            .map(|transition| FencedTransition {
                property_id: transition.property_id,
                prev: transition.prev,
                new: transition.new,
                membership: transition.membership,
            })
            .collect::<Vec<_>>(),
        PropertyEntity::Edge { .. } => memberships_for_change(change)
            .into_iter()
            .map(|membership| FencedTransition::from_change(change, membership))
            .collect(),
    };
    GraphStore::new().plan_index_build_admission(transitions)
}

/// Infallible commit half of the fence: binds `subject`, reserves one contiguous sequence per
/// planned envelope (the first stable write), and appends the exact requests to the Memory46
/// outbox under `mutation_id`. Callers invoke this only after a successful
/// [`preflight_property_index_ops`] and before the first canonical store mutation.
pub(crate) fn commit_property_index_ops(
    mutation_id: MutationId,
    subject: IndexBuildSubject,
    planned: Vec<PlannedBuildEnvelope>,
) {
    GraphStore::new().commit_index_build_admission(mutation_id, subject, planned);
}

/// Resolves the exact canonical build-DML subject for one property transition.
///
/// Requires federation routing; a Building/Sealing transition without a shard identity fails
/// closed before the primary store is touched.
pub(crate) fn index_build_subject_for_change(
    change: PropertyValueChange<'_>,
) -> Result<IndexBuildSubject, GraphStoreError> {
    let Some(routing) = GraphStore::new().federation_routing() else {
        return Err(GraphStoreError::IndexBuildAdmission(
            CanonicalExportError::InvalidRequest,
        ));
    };
    match change.entity {
        PropertyEntity::Vertex(vertex_id) => Ok(IndexBuildSubject::Vertex {
            shard_id: routing.shard_id.raw(),
            vertex_id: u32::try_from(u64::from(vertex_id)).map_err(|_| {
                GraphStoreError::IndexBuildAdmission(CanonicalExportError::InvalidRequest)
            })?,
        }),
        PropertyEntity::Edge {
            owner_vertex_id,
            label_id,
            slot_index,
        } => Ok(IndexBuildSubject::Edge {
            shard_id: routing.shard_id.raw(),
            owner_vertex_id: u32::try_from(u64::from(owner_vertex_id)).map_err(|_| {
                GraphStoreError::IndexBuildAdmission(CanonicalExportError::InvalidRequest)
            })?,
            label_id,
            slot_index,
        }),
    }
}

/// Dispatches vertex property changes while borrowing the pending queue once per batch.
///
/// Building/Sealing work is admitted through the index-build fence before the canonical write
/// and persisted directly in the Memory46 outbox, so only Active memberships reach the ordinary
/// queue here.
pub(crate) fn dispatch_vertex_property_index_ops_bulk<'a>(
    changes: &[(VertexId, PropertyId, Option<&'a Value>, &'a Value)],
) {
    let mut pending = Vec::new();
    for (vertex_id, property_id, previous, value) in changes {
        let change = PropertyValueChange::vertex(*vertex_id, *property_id, *previous, Some(*value));
        for transition in vertex_posting_transitions(&change) {
            if !transition.membership.phase.is_active() {
                continue;
            }
            pending.push((
                *vertex_id,
                transition.membership,
                index_ops_for_value_change(transition.property_id, transition.prev, transition.new),
            ));
        }
    }
    for (vertex_id, membership, ops) in pending {
        crate::index::pending::push_vertex_index_ops(vertex_id, membership, ops);
    }
}

#[cfg(test)]
mod text_wiring_tests {
    use super::*;
    use crate::facade::FederationRouting;
    use crate::index::text_catalog_context::{self, IndexedTextSpec};
    use crate::index::text_pending::{TextPendingOp, TextPendingOpKind, pending_snapshot};
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{IndexMaintenancePhase, PhysicalIndexId};

    fn spec(property_id: u32) -> IndexedTextSpec {
        IndexedTextSpec {
            property_id: PropertyId::from_raw(property_id),
            labels: vec![gleaph_graph_kernel::entry::VertexLabelId::from_raw(1)],
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
        crate::index::text_pending::clear_pending();
        crate::index::pending::clear_pending();
        crate::index::label_pending::clear_pending();
        let out = body(&graph);
        crate::index::text_pending::clear_pending();
        crate::index::pending::clear_pending();
        crate::index::label_pending::clear_pending();
        graph.set_federation_routing(None).expect("clear routing");
        out
    }

    fn labeled_vertex(store: &GraphStore) -> VertexId {
        let vid = store.insert_vertex().expect("vertex");
        let vertex = store.vertex(vid).expect("vertex row");
        store
            .add_vertex_label(
                vid,
                vertex,
                gleaph_graph_kernel::entry::VertexLabelId::from_raw(1),
            )
            .expect("label");
        vid
    }

    fn active_membership() -> IndexMembershipRef {
        IndexMembershipRef {
            physical_index_id: PhysicalIndexId::new(1).expect("nonzero id"),
            catalog_epoch: 1,
            phase: IndexMaintenancePhase::Active,
        }
    }

    #[test]
    fn text_upsert_is_derived_once_per_write_without_any_property_namespace() {
        with_routing(|store| {
            // No property-index catalog is installed at all: zero posting namespaces fan out,
            // yet the text derivation must still fire exactly once (it is independent derived
            // state, not a per-namespace side effect).
            let vid = labeled_vertex(store);
            let _guard = text_catalog_context::enter_indexed(&[spec(10)]);
            let value = gleaph_gql::Value::Text("hello".into());
            dispatch_property_index_ops(PropertyValueChange::vertex(
                vid,
                PropertyId::from_raw(10),
                None,
                Some(&value),
            ));
            assert_eq!(
                pending_snapshot(),
                vec![TextPendingOp {
                    key: u64::from(vid),
                    kind: TextPendingOpKind::Upsert {
                        text: "hello".into()
                    },
                }]
            );
            assert!(crate::index::pending::take_pending().is_empty());
        });
    }

    #[test]
    fn label_transition_funnel_derives_text_ops_despite_sealing_fence() {
        with_routing(|store| {
            // The labels path calls the physical funnel directly. A non-Active PROPERTY-index
            // membership suppresses postings but must not suppress text sync.
            let vid = labeled_vertex(store);
            let _guard = text_catalog_context::enter_indexed(&[spec(10)]);
            let mut sealing = active_membership();
            sealing.phase = IndexMaintenancePhase::Sealing;
            let value = gleaph_gql::Value::Text("gained".into());
            dispatch_property_index_ops_for_physical(
                PropertyValueChange::vertex(vid, PropertyId::from_raw(10), None, Some(&value)),
                sealing,
            );
            assert_eq!(pending_snapshot().len(), 1);
            assert!(matches!(
                pending_snapshot()[0].kind,
                TextPendingOpKind::Upsert { .. }
            ));
            assert!(crate::index::pending::take_pending().is_empty());
        });
    }

    #[test]
    fn retype_to_non_text_through_the_write_path_enqueues_delete() {
        with_routing(|store| {
            let vid = labeled_vertex(store);
            let _guard = text_catalog_context::enter_indexed(&[spec(10)]);
            let old = gleaph_gql::Value::Text("42".into());
            let new = gleaph_gql::Value::Bool(true);
            dispatch_property_index_ops(PropertyValueChange::vertex(
                vid,
                PropertyId::from_raw(10),
                Some(&old),
                Some(&new),
            ));
            assert_eq!(
                pending_snapshot(),
                vec![TextPendingOp {
                    key: u64::from(vid),
                    kind: TextPendingOpKind::Delete,
                }]
            );
        });
    }

    #[test]
    fn without_a_text_catalog_the_write_path_stays_inert() {
        with_routing(|store| {
            let vid = labeled_vertex(store);
            let value = gleaph_gql::Value::Text("no text index".into());
            dispatch_property_index_ops(PropertyValueChange::vertex(
                vid,
                PropertyId::from_raw(10),
                None,
                Some(&value),
            ));
            assert!(pending_snapshot().is_empty());
        });
    }
}
