//! Graph-owned pre-canonical index-build admission fence (ADR 0059 sections 6.3–6.5).
//!
//! Every canonical mutation path that can touch a Building/Sealing physical namespace runs the
//! same two-phase fence through this single admission owner:
//!
//! 1. **Plan (pure)** — [`GraphStore::plan_index_build_admission`] resolves the affected
//!    memberships, validates each Building scope identity/phase/epoch *without* reserving a
//!    sequence, and computes the exact removal+insertion envelopes (one envelope per physical
//!    membership, one contiguous sequence per envelope). A Sealing membership rejects with
//!    [`GraphStoreError::IndexBuildAdmission(RetryableSealing)`] before any canonical or durable
//!    write; any other admission error also aborts the mutation with nothing written.
//! 2. **Commit (infallible)** — [`GraphStore::commit_index_build_admission`] binds the exact
//!    canonical subject, reserves one contiguous sequence per planned envelope (the first stable
//!    write; cannot fail after a successful plan within one message), and appends the exact
//!    requests to the Memory46 derived-index outbox under the canonical mutation identity. The
//!    caller performs the canonical store mutations and dispatches Active memberships to the
//!    ordinary queue *after* this commit; a post-commit failure must trap so the IC rolls the
//!    whole message back rather than exposing partial state.
//!
//! The subject is deliberately bound at commit time, not plan time: an edge INSERT does not know
//! its canonical slot until LARA placement returns the handle, so the plan (which must reject
//! Sealing before any canonical write) runs first and the commit binds the real slot.

use gleaph_graph_kernel::canonical_export::CanonicalExportError;
use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::index::{
    IndexBuildDmlRequest, IndexBuildSubject, IndexMaintenancePhase, MAX_INDEX_BUILD_DML_VALUES,
};
use gleaph_graph_kernel::plan_exec::MutationId;

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use crate::facade::maintenance_timer;
use crate::index::canonical_export::{reserve_build_dml, validate_build_dml};
use crate::index::catalog_context::IndexMembershipRef;
use crate::property::{PropertyIndexOp, PropertyValueChange, index_ops_for_value_change};
use gleaph_gql::Value;

/// Traps an index-build fence rejection that occurs after the first canonical write of the
/// mutation (for example a slot-move rekey), so the IC rolls the whole message back instead of
/// exposing canonical state without its derived build-DML.
pub(crate) fn trap_build_fence<T>(error: GraphStoreError) -> T {
    panic!("index-build fence rejection after canonical mutation: {error}")
}

/// Traps a recoverable error that the mutation contract requires to be unreachable AFTER the
/// index-build fence committed its durable outbox admission (the first stable write).
///
/// Returning `Err` from the enclosing message at this point would persist the reserved sequence
/// and envelope while skipping the canonical mutation, exposing canonical state without its
/// derived build-DML. Every such error is therefore an invariant violation and must roll the
/// whole IC message back.
pub(crate) fn trap_post_fence_commit<T>(error: GraphStoreError) -> T {
    panic!("unreachable error after index-build fence commit: {error}")
}

/// One affected property transition on one exact catalog membership.
///
/// The membership is resolved by the caller (from the catalog or from inline-value decoding) so
/// the fence plans exactly the same namespaces that the ordinary Active dispatch maintains; the
/// two never disagree about the affected set.
#[derive(Clone, Copy)]
pub(crate) struct FencedTransition<'a> {
    pub property_id: PropertyId,
    pub prev: Option<&'a Value>,
    pub new: Option<&'a Value>,
    pub membership: IndexMembershipRef,
}

impl<'a> FencedTransition<'a> {
    /// Builds the transition for one exact membership of a property value change. The change's
    /// entity is ignored by the plan (the subject is bound at commit), so callers may construct
    /// the change with placeholder ids when the canonical identity is not yet known.
    pub(crate) fn from_change(
        change: PropertyValueChange<'a>,
        membership: IndexMembershipRef,
    ) -> Self {
        Self {
            property_id: change.property_id,
            prev: change.prev,
            new: change.new,
            membership,
        }
    }
}

/// One planned Building envelope whose `shard_sequence` is reserved only during the commit phase.
pub(crate) struct PlannedBuildEnvelope {
    pub physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId,
    pub catalog_epoch: u64,
    pub removals: Vec<Vec<u8>>,
    pub insertions: Vec<Vec<u8>>,
}

impl GraphStore {
    /// Pure admission planning (the preflight half of the fence).
    ///
    /// For every transition that yields indexable postings, rejects the whole mutation when any
    /// affected membership is Sealing (`RetryableSealing`) or when a Building scope fails its
    /// identity/phase/epoch validation or lacks sequence capacity. Active memberships produce no
    /// envelope; they remain on the ordinary queue. The returned envelopes carry no reserved
    /// sequence and no subject; the caller binds both at commit time.
    pub(crate) fn plan_index_build_admission<'a>(
        &self,
        transitions: impl IntoIterator<Item = FencedTransition<'a>>,
    ) -> Result<Vec<PlannedBuildEnvelope>, GraphStoreError> {
        let mut planned = Vec::new();
        for transition in transitions {
            let ops =
                index_ops_for_value_change(transition.property_id, transition.prev, transition.new);
            if ops.is_empty() {
                continue;
            }
            if matches!(transition.membership.phase, IndexMaintenancePhase::Sealing) {
                return Err(GraphStoreError::IndexBuildAdmission(
                    CanonicalExportError::RetryableSealing,
                ));
            }
            if !matches!(transition.membership.phase, IndexMaintenancePhase::Building) {
                continue;
            }
            if ops.len() > MAX_INDEX_BUILD_DML_VALUES {
                return Err(GraphStoreError::IndexBuildAdmission(
                    CanonicalExportError::InvalidRequest,
                ));
            }
            // Pure validation only: the scope must still be Building under the exact catalog
            // epoch with capacity for the next sequence. No sequence is reserved here.
            validate_build_dml(
                transition.membership.physical_index_id,
                transition.membership.catalog_epoch,
            )
            .map_err(GraphStoreError::IndexBuildAdmission)?;
            let mut removals = Vec::new();
            let mut insertions = Vec::new();
            for op in &ops {
                match op {
                    PropertyIndexOp::Insert { payload_bytes, .. } => {
                        insertions.push(payload_bytes.clone());
                    }
                    PropertyIndexOp::Remove { payload_bytes, .. } => {
                        removals.push(payload_bytes.clone());
                    }
                }
            }
            planned.push(PlannedBuildEnvelope {
                physical_index_id: transition.membership.physical_index_id,
                catalog_epoch: transition.membership.catalog_epoch,
                removals,
                insertions,
            });
        }
        Ok(planned)
    }

    /// Resolves the exact canonical edge build-DML subject for one handle.
    ///
    /// Requires federation routing; a Building/Sealing transition without a shard identity fails
    /// closed before the primary store is touched.
    pub(crate) fn edge_subject_for_handle(
        &self,
        handle: EdgeHandle,
    ) -> Result<IndexBuildSubject, GraphStoreError> {
        let Some(routing) = self.federation_routing() else {
            return Err(GraphStoreError::IndexBuildAdmission(
                CanonicalExportError::InvalidRequest,
            ));
        };
        Ok(IndexBuildSubject::Edge {
            shard_id: routing.shard_id.raw(),
            owner_vertex_id: u32::try_from(u64::from(handle.owner_vertex_id)).map_err(|_| {
                GraphStoreError::IndexBuildAdmission(CanonicalExportError::InvalidRequest)
            })?,
            label_id: handle.label_id.raw(),
            slot_index: handle.slot_index.raw(),
        })
    }

    /// Resolves the exact canonical vertex build-DML subject for one id.
    pub(crate) fn vertex_subject_for_id(
        &self,
        vertex_id: ic_stable_lara::VertexId,
    ) -> Result<IndexBuildSubject, GraphStoreError> {
        let Some(routing) = self.federation_routing() else {
            return Err(GraphStoreError::IndexBuildAdmission(
                CanonicalExportError::InvalidRequest,
            ));
        };
        Ok(IndexBuildSubject::Vertex {
            shard_id: routing.shard_id.raw(),
            vertex_id: u32::try_from(u64::from(vertex_id)).map_err(|_| {
                GraphStoreError::IndexBuildAdmission(CanonicalExportError::InvalidRequest)
            })?,
        })
    }

    /// Infallible commit half of the fence.
    ///
    /// Binds every planned envelope to one exact canonical `subject`, reserves one contiguous
    /// sequence per envelope (the first stable write), and appends the exact requests to the
    /// Memory46 outbox under `mutation_id`. Callers invoke this only after a successful
    /// [`Self::plan_index_build_admission`] and before the first canonical store mutation; a
    /// reserve failure after a successful plan is an invariant violation and traps so the IC
    /// rolls the whole message back.
    pub(crate) fn commit_index_build_admission(
        &self,
        mutation_id: MutationId,
        subject: IndexBuildSubject,
        planned: Vec<PlannedBuildEnvelope>,
    ) {
        if planned.is_empty() {
            return;
        }
        let mut requests = Vec::with_capacity(planned.len());
        for envelope in planned {
            let shard_sequence = reserve_build_dml(
                envelope.physical_index_id,
                envelope.catalog_epoch,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "preflighted index-build scope {:?}/epoch {} must remain reservable within one message: {error}",
                    envelope.physical_index_id, envelope.catalog_epoch
                )
            });
            requests.push(IndexBuildDmlRequest {
                physical_index_id: envelope.physical_index_id,
                catalog_epoch: envelope.catalog_epoch,
                shard_sequence,
                subject,
                removals: envelope.removals,
                insertions: envelope.insertions,
            });
        }
        self.derived_index_build_outbox_append(mutation_id, requests);
        maintenance_timer::arm_if_needed();
    }
}
