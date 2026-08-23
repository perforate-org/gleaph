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
    match change.entity {
        PropertyEntity::Vertex(_) => {
            for transition in vertex_posting_transitions(&change) {
                let derived = PropertyValueChange {
                    entity: change.entity,
                    property_id: transition.property_id,
                    prev: transition.prev,
                    new: transition.new,
                };
                dispatch_property_index_ops_for_physical(derived, transition.membership);
            }
        }
        PropertyEntity::Edge { .. } => {
            for membership in memberships_for_change(change) {
                dispatch_property_index_ops_for_physical(change, membership);
            }
        }
    }
}

/// Applies one property transition to one exact Router-allocated namespace.
///
/// Inline-property decoding already resolves the concrete membership before dispatch, so it
/// uses this narrow helper to avoid re-expanding a membership into duplicate posting operations.
pub(crate) fn dispatch_property_index_ops_for_physical(
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
