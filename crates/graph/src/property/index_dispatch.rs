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
    let memberships = memberships_for_change(change);
    for membership in memberships {
        dispatch_property_index_ops_for_physical(change, membership);
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
        PropertyEntity::Vertex(_) => {
            crate::index::catalog_context::vertex_index_memberships(change.property_id)
        }
        PropertyEntity::Edge { label_id, .. } => {
            crate::index::catalog_context::edge_index_memberships(label_id, change.property_id)
        }
    }
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
    let memberships = memberships_for_change(change);
    let transitions = memberships
        .into_iter()
        .map(|membership| FencedTransition::from_change(change, membership));
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
        for membership in crate::index::catalog_context::vertex_index_memberships(*property_id) {
            if !membership.phase.is_active() {
                continue;
            }
            pending.push((
                *vertex_id,
                membership,
                index_ops_for_value_change(*property_id, *previous, Some(*value)),
            ));
        }
    }
    for (vertex_id, membership, ops) in pending {
        crate::index::pending::push_vertex_index_ops(vertex_id, membership, ops);
    }
}
