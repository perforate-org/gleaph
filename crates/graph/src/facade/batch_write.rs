//! Internal GraphStore clean-slab batch orchestration for ADR 0045.
//!
//! This module consumes physical half-edge intents produced by
//! [`super::batch_placement`] and attempts to commit them through the LARA
//! one-orientation batch primitive.  Unsupported geometry is returned to the
//! caller before any canonical write so the existing scalar path can handle it.
//! No LARA placement policy leaks outside this module.

use gleaph_graph_kernel::entry::{Edge, EdgeLabelId};
use gleaph_graph_kernel::federation::{HOT_FORWARD_EDGE_INSERT_THRESHOLD, LocalVertexId};
use gleaph_graph_kernel::plan_exec::{
    GraphMutationRequestIdentityV1, GraphOrderedEdgeBatchReceiptV1, GraphOrderedEdgeBatchResult,
    GraphOrderedEdgeBatchResultV1, GraphOrderedMixedBatchReceiptV1, GraphOrderedMixedBatchResult,
    GraphOrderedMixedBatchResultV1, LabelStatsDelta, MutationId,
};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::EdgePlacementPolicy;
#[cfg(test)]
use ic_stable_lara::labeled::batch_write::BatchReservation;
use ic_stable_lara::labeled::batch_write::{
    BatchLocationMode, BatchLogicalPair, BatchLogicalPairKind, BidirectionalBatchPlan,
    OneOrientationBatchEdge, OneOrientationBatchError, OneOrientationBatchLocation,
    OneOrientationBatchPlan, OneOrientationBatchResult, OneOrientationBucketRun,
    UndirectedBatchPair,
};
use ic_stable_lara::{CsrEdge, labeled::CanonicalEdgeOccurrence, labeled::LabeledOrientation};
use rapidhash::{HashMapExt, RapidHashMap};
use std::collections::{BTreeMap, BTreeSet};

use super::EdgeHandle;
use super::GraphStore;
use super::batch_placement::{
    BatchEdgeInput, BatchEdgeIntent, BatchEdgeIntentRole, BatchPlacementError, BatchPlacementKey,
};
use super::mutation_executor::GraphMutationExecutor;
use super::store::helpers::{
    build_edge_to, catalog_edge_label_from_wire, edge_storage_label, lara_edge_placement,
    lara_label,
};
use crate::edge_inline_property_schema::resolved_edge_label_with;
use crate::facade::{FencedTransition, GraphStoreError, PlannedBuildEnvelope, trap_build_fence};

/// Result of attempting a clean-slab batch edge insert through GraphStore.
///
/// - `Committed`: every required one-orientation reservation succeeded and was
///   committed. The contained results are ordered by orientation.
/// - `RecoverableGeometry`: at least one orientation could not be reserved on
///   the clean-slab path for a geometry condition that permits scalar fallback.
#[derive(Debug)]
pub(crate) enum BatchEdgeInsertResult {
    Committed {
        /// Aggregate edge slab slots written across all orientations.
        edge_slots_written: u64,
        /// Aggregate inline property bytes slab slots written across all orientations.
        inline_property_bytes_slots_written: u64,
        /// Paired physical locations keyed by the logical input ordinal.
        locations: Option<Vec<BatchEdgePhysicalLocation>>,
        /// True when at least one orientation used pending-aware leaf expansion.
        used_expansion: bool,
    },
    RecoverableGeometry {
        /// Typed reason the clean-slab path could not be used.
        error: OneOrientationBatchError,
    },
}

#[derive(Debug)]
pub(crate) enum OrderedEdgeBatchExecutionError {
    Validation(BatchPlacementError),
    RecoverableGeometry(OneOrientationBatchError),
    /// A Building/Sealing index DML failed its Graph-owned admission before any batch write.
    IndexBuildAdmission(GraphStoreError),
}

impl OrderedEdgeBatchExecutionError {
    pub(crate) fn is_recoverable_geometry(&self) -> bool {
        matches!(self, Self::RecoverableGeometry(_))
    }
}

impl std::fmt::Display for OrderedEdgeBatchExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(error) => write!(f, "ordered Graph batch validation failed: {error}"),
            Self::RecoverableGeometry(error) => {
                write!(
                    f,
                    "ordered Graph clean-slab geometry is unsupported: {error}"
                )
            }
            Self::IndexBuildAdmission(error) => {
                write!(
                    f,
                    "ordered Graph batch index-build admission failed: {error}"
                )
            }
        }
    }
}

impl std::error::Error for OrderedEdgeBatchExecutionError {}

/// Physical locations for one logical edge after the orientation join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatchEdgePhysicalLocation {
    Directed {
        logical_ordinal: u32,
        forward: OneOrientationBatchLocation,
        reverse: OneOrientationBatchLocation,
    },
    Undirected {
        logical_ordinal: u32,
        owner: OneOrientationBatchLocation,
        alias: OneOrientationBatchLocation,
    },
    UndirectedSelfLoop {
        logical_ordinal: u32,
        location: OneOrientationBatchLocation,
    },
}

impl BatchEdgePhysicalLocation {
    /// Resolve the canonical Graph sidecar occurrence from a captured batch location.
    ///
    /// LARA may return both orientations for a logical edge, but Graph sidecars are
    /// owned only by the canonical forward row: directed edges use the source row,
    /// undirected edges use the higher-owner row, and self-loops have one owner row.
    /// The captured `logical_slot` is the bucket-local slot contract and is valid
    /// independently of the raw slab or overflow-log location.
    pub(crate) fn canonical_occurrence(&self, input: &BatchEdgeInput) -> CanonicalEdgeOccurrence {
        let location = match self {
            Self::Directed { forward, .. } => forward,
            Self::Undirected { owner, .. } => owner,
            Self::UndirectedSelfLoop { location, .. } => location,
        };
        CanonicalEdgeOccurrence {
            orientation: LabeledOrientation::Forward,
            owner_vertex_id: location.owner_vertex_id,
            label_id: lara_label(edge_storage_label(input.catalog_label, !input.directed)),
            slot_index: location.logical_slot.into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum BatchLocationJoinError {
    Missing {
        logical_ordinal: u32,
        role: BatchEdgeIntentRole,
    },
    Duplicate {
        logical_ordinal: u32,
        role: BatchEdgeIntentRole,
    },
    Unexpected {
        logical_ordinal: u32,
        owner_vertex_id: VertexId,
    },
}

impl BatchEdgeInsertResult {
    /// Total edge slab slots written across all committed orientations.
    pub(crate) fn total_edge_slots(&self) -> Option<u64> {
        match self {
            Self::Committed {
                edge_slots_written, ..
            } => Some(*edge_slots_written),
            Self::RecoverableGeometry { .. } => None,
        }
    }

    /// Total inline property bytes slab slots written across all committed orientations.
    pub(crate) fn total_inline_property_bytes_slots(&self) -> Option<u64> {
        match self {
            Self::Committed {
                inline_property_bytes_slots_written,
                ..
            } => Some(*inline_property_bytes_slots_written),
            Self::RecoverableGeometry { .. } => None,
        }
    }

    pub(crate) fn used_expansion(&self) -> bool {
        matches!(
            self,
            Self::Committed {
                used_expansion: true,
                ..
            }
        )
    }
}

/// One-orientation batch write request derived from a placement summary.
pub(crate) struct OneOrientationBatchWriteRequest<E: CsrEdge> {
    pub(crate) orientation: LabeledOrientation,
    pub(crate) role: BatchEdgeIntentRole,
    pub(crate) plan: OneOrientationBatchPlan<E>,
}

impl GraphStore {
    /// Execute an unsupported optimized geometry through the existing scalar owner boundary.
    ///
    /// The ordered planner has already validated the complete request. Scalar writes therefore
    /// run in input order, while an unexpected post-write error traps so the canister message
    /// rolls back rather than exposing a partially committed ordered batch.
    pub(crate) fn execute_ordered_edge_batch_scalar_fallback(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        edges: &[BatchEdgeInput],
    ) -> GraphOrderedEdgeBatchResult {
        self.execute_ordered_scalar_edges(edges, mutation_id);
        self.commit_ordered_edge_batch_receipt(mutation_id, request_identity, edges)
    }

    /// Apply the scalar owner boundary for a subset of a mixed ordered request.
    ///
    /// The caller must have completed whole-request planning before invoking this helper. Any
    /// unexpected write error traps so a prior batch commit in the same Graph message rolls back.
    pub(crate) fn execute_ordered_scalar_edges(
        &self,
        edges: &[BatchEdgeInput],
        mutation_id: MutationId,
    ) {
        for edge in edges {
            let properties = edge.initial_edge_properties.iter().cloned();
            if edge.directed {
                GraphMutationExecutor::insert_directed_edge_with_inline_property_bytes(
                    self,
                    edge.source_vertex_id,
                    edge.target_vertex_id,
                    edge.catalog_label,
                    &edge.inline_property_bytes,
                    properties,
                    mutation_id,
                )
                .unwrap_or_else(|error| {
                    panic!("ordered Graph scalar fallback failed after preflight: {error}")
                });
            } else {
                GraphMutationExecutor::insert_undirected_edge_with_inline_property_bytes(
                    self,
                    edge.source_vertex_id,
                    edge.target_vertex_id,
                    edge.catalog_label,
                    &edge.inline_property_bytes,
                    properties,
                    mutation_id,
                )
                .unwrap_or_else(|error| {
                    panic!("ordered Graph scalar fallback failed after preflight: {error}")
                });
            }
        }
    }

    /// Execute the edge phase of a mixed batch and publish one mixed aggregate receipt.
    ///
    /// Vertex allocation and all endpoint resolution happen at the caller boundary. This
    /// method owns only the existing ordered edge placement, sidecars, label delta, and
    /// mixed journal completion after the edge phase has started.
    pub(crate) fn execute_ordered_edge_phase_for_mixed(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        edges: &[BatchEdgeInput],
        logical_vertex_count: u64,
        logical_operation_count: u64,
        allocated_vertex_ids: Vec<LocalVertexId>,
    ) -> Result<GraphOrderedMixedBatchResult, OrderedEdgeBatchExecutionError> {
        // Validation only. The clean-slab batch this used to select is measured as a loss on the
        // one shape it can serve - 6.01 M against 4.23 M for the same 128 edges in the same
        // message shape - and it declines the shape it could amortize (one bucket, many rows), so
        // every edge goes through the scalar path, which is also the reference the equivalence
        // tests compare against.
        self.classify_batch_edge_insertion(edges)
            .map_err(OrderedEdgeBatchExecutionError::Validation)?;
        self.execute_ordered_scalar_edges(edges, mutation_id);

        let label_stats_delta = ordered_edge_label_stats_delta(edges);
        let delta_event = (!label_stats_delta.edge.is_empty()).then(|| {
            self.commit_append_label_stats_delta(mutation_id, label_stats_delta)
                .unwrap_or_else(|error| {
                    panic!("mixed Graph label delta append after canonical write: {error}")
                })
        });
        let (emitted_delta_first_seq, emitted_delta_last_seq) = delta_event
            .as_ref()
            .map(|event| (Some(event.shard_event_seq), Some(event.shard_event_seq)))
            .unwrap_or((None, None));
        let receipt = GraphOrderedMixedBatchReceiptV1 {
            logical_operation_count,
            logical_vertex_count,
            logical_edge_count: edges.len() as u64,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: ordered_hot_forward_vertices(edges),
            allocated_vertex_ids: allocated_vertex_ids.clone(),
        };
        receipt
            .validate()
            .expect("mixed Graph receipt must satisfy its bounded contract");
        self.commit_record_completed_ordered_mixed_batch_journal(
            mutation_id,
            request_identity,
            logical_operation_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            receipt.hot_forward_vertices.clone(),
            allocated_vertex_ids,
        );
        Ok(GraphOrderedMixedBatchResult::V1(
            GraphOrderedMixedBatchResultV1::Completed(receipt),
        ))
    }

    fn execute_mixed_clean_slab(
        &self,
        edges: &[BatchEdgeInput],
        intents: &[BatchEdgeIntent],
        mutation_id: MutationId,
    ) -> Result<(), OrderedEdgeBatchExecutionError> {
        // Fence plan BEFORE the batch insert: every initial sidecar transition (prev=None).
        let planned = self
            .plan_ordered_batch_sidecar_admission(edges)
            .map_err(OrderedEdgeBatchExecutionError::IndexBuildAdmission)?;
        let result = self
            .try_insert_ordered_edge_batch_clean_slab_with_intents(edges, intents)
            .map_err(OrderedEdgeBatchExecutionError::Validation)?;
        if let BatchEdgeInsertResult::RecoverableGeometry { error } = result {
            return Err(OrderedEdgeBatchExecutionError::RecoverableGeometry(error));
        }
        self.commit_ordered_edge_batch_initial_sidecars(edges, &result, mutation_id, planned);
        Ok(())
    }

    fn execute_mixed_partitioned(
        &self,
        edges: &[BatchEdgeInput],
        batch_ordinals: &BTreeSet<u32>,
        mutation_id: MutationId,
    ) -> Result<(), OrderedEdgeBatchExecutionError> {
        let (batch_edges, scalar_edges): (Vec<_>, Vec<_>) = edges
            .iter()
            .enumerate()
            .partition(|(ordinal, _)| batch_ordinals.contains(&(*ordinal as u32)));
        let batch_edges: Vec<_> = batch_edges
            .into_iter()
            .map(|(_, edge)| edge.clone())
            .collect();
        let scalar_edges: Vec<_> = scalar_edges
            .into_iter()
            .map(|(_, edge)| edge.clone())
            .collect();
        // Fence plan BEFORE the batch insert: every initial sidecar transition (prev=None).
        let planned = self
            .plan_ordered_batch_sidecar_admission(&batch_edges)
            .map_err(OrderedEdgeBatchExecutionError::IndexBuildAdmission)?;
        match self.try_insert_ordered_edge_batch_clean_slab(&batch_edges) {
            Ok(result @ BatchEdgeInsertResult::Committed { .. }) => {
                self.commit_ordered_edge_batch_initial_sidecars(
                    &batch_edges,
                    &result,
                    mutation_id,
                    planned,
                );
                self.execute_ordered_scalar_edges(&scalar_edges, mutation_id);
                Ok(())
            }
            Ok(BatchEdgeInsertResult::RecoverableGeometry { .. }) => {
                self.execute_ordered_scalar_edges(edges, mutation_id);
                Ok(())
            }
            Err(error) => Err(OrderedEdgeBatchExecutionError::Validation(error)),
        }
    }

    /// Pure admission planning for an ordered batch's initial sidecar transitions.
    ///
    /// Runs before any batch insert so a Sealing membership rejects with nothing written. The
    /// canonical slots are not known until placement, so this returns subject-less envelopes per
    /// logical edge; [`Self::commit_ordered_edge_batch_initial_sidecars`] binds each subject.
    fn plan_ordered_batch_sidecar_admission(
        &self,
        edges: &[BatchEdgeInput],
    ) -> Result<Vec<Vec<PlannedBuildEnvelope>>, GraphStoreError> {
        // Fast path: with no Building/Sealing membership every transition is Active, so the fence
        // has no envelope to commit and nothing to reject. Skip the per-edge membership
        // resolution entirely (the common case with no in-flight index build).
        if !crate::index::catalog_context::has_non_active_membership() {
            return Ok((0..edges.len()).map(|_| Vec::new()).collect());
        }
        let mut planned_by_edge = Vec::with_capacity(edges.len());
        for edge in edges {
            let wire_label = lara_label(edge_storage_label(edge.catalog_label, !edge.directed));
            let mut transitions = Vec::new();
            for (property_id, value) in &edge.initial_edge_properties {
                for membership in crate::index::catalog_context::edge_index_memberships(
                    wire_label.raw(),
                    *property_id,
                ) {
                    transitions.push(FencedTransition {
                        property_id: *property_id,
                        prev: None,
                        new: Some(value),
                        membership,
                    });
                }
            }
            planned_by_edge.push(self.plan_index_build_admission(transitions)?);
        }
        Ok(planned_by_edge)
    }

    fn commit_ordered_edge_batch_initial_sidecars(
        &self,
        edges: &[BatchEdgeInput],
        result: &BatchEdgeInsertResult,
        mutation_id: MutationId,
        planned_by_edge: Vec<Vec<PlannedBuildEnvelope>>,
    ) {
        if !edges
            .iter()
            .any(|edge| !edge.initial_edge_properties.is_empty())
        {
            return;
        }
        let BatchEdgeInsertResult::Committed {
            locations: Some(locations),
            ..
        } = result
        else {
            panic!("property-bearing ordered batch must capture canonical locations");
        };
        assert_eq!(
            locations.len(),
            edges.len(),
            "ordered batch sidecar locations must align with logical items"
        );
        assert_eq!(
            planned_by_edge.len(),
            edges.len(),
            "ordered batch sidecar plans must align with logical items"
        );
        for ((edge, location), planned) in edges.iter().zip(locations).zip(planned_by_edge) {
            let occurrence = location.canonical_occurrence(edge);
            let handle = EdgeHandle::at_slot(
                occurrence.owner_vertex_id,
                occurrence.label_id,
                occurrence.slot_index,
            );
            // Commit (infallible): bind the exact canonical slot and append the envelopes before
            // the sidecar writes. The plan already rejected Sealing before the batch insert.
            if !planned.is_empty() {
                self.commit_index_build_admission(
                    mutation_id,
                    self.edge_subject_for_handle(handle)
                        .unwrap_or_else(trap_build_fence),
                    planned,
                );
            }
            self.commit_edge_property_writes_at_canonical(handle, &edge.initial_edge_properties);
        }
    }

    pub(crate) fn commit_ordered_edge_batch_receipt(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        edges: &[BatchEdgeInput],
    ) -> GraphOrderedEdgeBatchResult {
        let label_stats_delta = ordered_edge_label_stats_delta(edges);
        let delta_event = (!label_stats_delta.edge.is_empty()).then(|| {
            self.commit_append_label_stats_delta(mutation_id, label_stats_delta)
                .unwrap_or_else(|error| {
                    panic!("ordered Graph label delta append after canonical write: {error}")
                })
        });
        let hot_forward_vertices = ordered_hot_forward_vertices(edges);
        let row_count = u64::try_from(edges.len())
            .expect("ordered Graph item count was validated before batch execution");
        let (emitted_delta_first_seq, emitted_delta_last_seq) = delta_event
            .as_ref()
            .map(|event| (Some(event.shard_event_seq), Some(event.shard_event_seq)))
            .unwrap_or((None, None));
        let receipt = GraphOrderedEdgeBatchReceiptV1 {
            logical_edge_count: row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: hot_forward_vertices.clone(),
        };
        receipt
            .validate()
            .expect("ordered Graph receipt must satisfy its bounded contract");
        self.commit_record_completed_ordered_edge_batch_journal(
            mutation_id,
            request_identity,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
        );
        GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1::Completed(receipt))
    }

    /// Select the cheapest clean-slab location mode needed by an ordered batch.
    ///
    /// Ordered receipts only need aggregate counts. Exact canonical locations are needed only
    /// when initial edge properties must be written after the LARA commit; inline property bytes
    /// are already part of the canonical batch input and do not require location capture.
    fn try_insert_ordered_edge_batch_clean_slab(
        &self,
        edges: &[BatchEdgeInput],
    ) -> Result<BatchEdgeInsertResult, BatchPlacementError> {
        let intents = self.expand_batch_edge_intents(edges)?;
        self.try_insert_ordered_edge_batch_clean_slab_with_intents(edges, &intents)
    }

    fn try_insert_ordered_edge_batch_clean_slab_with_intents(
        &self,
        edges: &[BatchEdgeInput],
        intents: &[BatchEdgeIntent],
    ) -> Result<BatchEdgeInsertResult, BatchPlacementError> {
        if edges
            .iter()
            .any(|edge| !edge.initial_edge_properties.is_empty())
        {
            self.try_insert_batch_edges_clean_slab_with_mode_and_intents(
                edges,
                intents,
                BatchLocationMode::Capture,
            )
        } else {
            self.try_insert_batch_edges_clean_slab_with_mode_and_intents(
                edges,
                intents,
                BatchLocationMode::AggregateOnly,
            )
        }
    }

    /// Pre-create empty directed buckets for `src -> dst` with the given inline
    /// value width so a later clean-slab batch can consume the per-bucket initial
    /// quota. This is a test/bench helper and is not part of the public API.
    pub(crate) fn prepare_clean_slab_dir_buckets(
        &self,
        src: VertexId,
        dst: VertexId,
        label: EdgeLabelId,
        width: u16,
    ) {
        let storage = lara_label(edge_storage_label(Some(label), false));
        self.with_graph_mut(|g| {
            g.ensure_directed_edge_inline_property_width(src, dst, storage, width)
                .expect("ensure directed buckets");
        });
    }

    /// Pre-create empty undirected buckets for `{a,b}` with the given inline property bytes
    /// width so a later clean-slab batch can consume the per-bucket initial quota.
    pub(crate) fn prepare_clean_slab_undir_buckets(
        &self,
        a: VertexId,
        b: VertexId,
        label: EdgeLabelId,
        width: u16,
    ) {
        let storage = lara_label(edge_storage_label(Some(label), true));
        self.with_graph_mut(|g| {
            g.ensure_undirected_edge_inline_property_width(a, b, storage, width)
                .expect("ensure undirected buckets");
        });
    }

    /// Attempt to insert a bounded batch of logical edges through the optimized
    /// clean-slab one-orientation path.
    ///
    /// This is the GraphStore orchestration entry point for Plan 0123. It:
    /// 1. Validates the input and expands it into physical half-edge intents.
    /// 2. Groups the intents into one-orientation plans.
    /// 3. Reserves every orientation before committing any orientation.
    /// 4. Commits only after all reservations succeed.
    ///
    /// If any orientation cannot be reserved on the clean-slab path, this method
    /// returns [`BatchEdgeInsertResult::RecoverableGeometry`] without writing any canonical
    /// adjacency. Any reservation that succeeded before the failure is rolled back
    /// by consuming its token; the rollback restores logical edge capacity and
    /// the inline property bytes occupied tail, and retires any allocated inline property bytes to the
    /// free-list as reusable slack. The underlying stable-memory pages are not
    /// shrunk. This leaves no leaked capacity or tail before the caller falls
    /// back to the existing scalar path.
    pub(crate) fn try_insert_batch_edges_clean_slab(
        &self,
        edges: &[super::batch_placement::BatchEdgeInput],
    ) -> Result<BatchEdgeInsertResult, BatchPlacementError> {
        self.try_insert_batch_edges_clean_slab_with_mode(edges, BatchLocationMode::AggregateOnly)
    }

    /// Attempt the same batch write while retaining exact physical locations.
    ///
    /// This is reserved for the future mate-index path. Ordinary batch writes
    /// should use [`Self::try_insert_batch_edges_clean_slab`] so they do not pay
    /// location materialization or ordinal join costs.
    pub(crate) fn try_insert_batch_edges_clean_slab_with_locations(
        &self,
        edges: &[super::batch_placement::BatchEdgeInput],
    ) -> Result<BatchEdgeInsertResult, BatchPlacementError> {
        self.try_insert_batch_edges_clean_slab_with_mode(edges, BatchLocationMode::Capture)
    }

    /// Insert a clean-slab batch and commit its initial canonical sidecars under the index-build
    /// admission fence.
    ///
    /// Location capture is mandatory for this path. Property validation happens before
    /// reservation, and sidecars are addressed directly by the captured canonical
    /// owner/label/logical-slot tuple after LARA commit. The fence runs the same pure plan and
    /// infallible commit as the ordered batch path, so a Building membership receives one exact
    /// build-DML envelope per affected transition and a Sealing membership rejects before any
    /// canonical write.
    pub(crate) fn try_insert_batch_edges_clean_slab_with_initial_properties(
        &self,
        edges: &[super::batch_placement::BatchEdgeInput],
        mutation_id: MutationId,
    ) -> Result<BatchEdgeInsertResult, BatchPlacementError> {
        let planned_by_edge = self
            .plan_ordered_batch_sidecar_admission(edges)
            .map_err(BatchPlacementError::IndexBuildAdmission)?;
        let result =
            self.try_insert_batch_edges_clean_slab_with_mode(edges, BatchLocationMode::Capture)?;
        if let BatchEdgeInsertResult::Committed { .. } = &result {
            self.commit_ordered_edge_batch_initial_sidecars(
                edges,
                &result,
                mutation_id,
                planned_by_edge,
            );
        }
        Ok(result)
    }

    fn try_insert_batch_edges_clean_slab_with_mode(
        &self,
        edges: &[super::batch_placement::BatchEdgeInput],
        location_mode: BatchLocationMode,
    ) -> Result<BatchEdgeInsertResult, BatchPlacementError> {
        let intents = self.expand_batch_edge_intents(edges)?;
        self.try_insert_batch_edges_clean_slab_with_mode_and_intents(edges, &intents, location_mode)
    }

    fn try_insert_batch_edges_clean_slab_with_mode_and_intents(
        &self,
        edges: &[super::batch_placement::BatchEdgeInput],
        intents: &[BatchEdgeIntent],
        location_mode: BatchLocationMode,
    ) -> Result<BatchEdgeInsertResult, BatchPlacementError> {
        Self::validate_batch_initial_edge_properties(edges)?;
        if edges.is_empty() {
            return Err(BatchPlacementError::BatchWrite(
                OneOrientationBatchError::EmptyPlan,
            ));
        }

        let directed = edges[0].directed;
        let undirected_self_loop =
            !directed && edges[0].source_vertex_id == edges[0].target_vertex_id;
        let homogeneous = edges.iter().all(|edge| {
            edge.directed == directed
                && (directed
                    || (edge.source_vertex_id == edge.target_vertex_id) == undirected_self_loop)
        });

        let owner_plan = if homogeneous {
            let requests = {
                #[cfg(feature = "canbench")]
                let _scope = canbench_rs::bench_scope("ordered_batch_build_orientation_plans");
                self.build_one_orientation_batch_plans(intents, encode_intent_edge)?
            };
            let undirected_pairs = (!directed && !undirected_self_loop)
                .then(|| build_undirected_batch_pairs(intents))
                .transpose()?;

            match requests.as_slice() {
                [request] if request.role == BatchEdgeIntentRole::UndirectedOwnerForward => {
                    BidirectionalBatchPlan::SelfLoop {
                        forward: request.plan.clone(),
                    }
                }
                [first, second]
                    if first.role == BatchEdgeIntentRole::CanonicalForward
                        && second.role == BatchEdgeIntentRole::DerivedReverse =>
                {
                    BidirectionalBatchPlan::Directed {
                        forward: first.plan.clone(),
                        reverse: second.plan.clone(),
                    }
                }
                [first, second]
                    if first.role == BatchEdgeIntentRole::UndirectedOwnerForward
                        && second.role == BatchEdgeIntentRole::UndirectedAliasForward =>
                {
                    BidirectionalBatchPlan::Undirected {
                        plan: merge_one_orientation_batch_plans(&first.plan, &second.plan),
                        pairs: undirected_pairs.expect("non-self undirected pairs are built above"),
                    }
                }
                _ => {
                    return Err(BatchPlacementError::BatchWrite(
                        OneOrientationBatchError::InvalidOrientationPair(
                            "physical intent roles do not form one logical batch shape".into(),
                        ),
                    ));
                }
            }
        } else {
            BidirectionalBatchPlan::Mixed {
                physical: {
                    #[cfg(feature = "canbench")]
                    let _scope = canbench_rs::bench_scope("ordered_batch_build_merged_plans");
                    self.build_merged_orientation_batch_plans(intents, encode_intent_edge)?
                },
                pairs: build_mixed_batch_pairs(edges, intents)?,
            }
        };

        // Reserve every orientation first. If any orientation is unsupported, roll
        // back every previously successful reservation before returning unsupported.
        // No canonical write occurs on this path.
        let reservation_result = {
            #[cfg(feature = "canbench")]
            let _scope = canbench_rs::bench_scope("ordered_batch_reserve_orientations");
            self.with_graph_mut(|graph| graph.reserve_batch_orientations(owner_plan))
        };
        let reservations = match reservation_result {
            Ok(reservations) => reservations,
            Err(err) => {
                if err.is_recoverable_geometry() {
                    return Ok(BatchEdgeInsertResult::RecoverableGeometry { error: err });
                }
                return Err(BatchPlacementError::BatchWrite(err));
            }
        };

        // All reservations succeeded: commit each orientation.
        let used_expansion = reservations
            .iter()
            .any(|(_, reservation)| reservation.uses_expansion());
        let results = {
            #[cfg(feature = "canbench")]
            let _scope = canbench_rs::bench_scope("ordered_batch_commit_orientations");
            self.with_graph_mut(|graph| {
                graph.commit_batch_orientations(reservations, location_mode)
            })
        };

        let edge_slots_written = results
            .iter()
            .map(|(_, result)| u64::from(result.edge_slots_written))
            .sum();
        let inline_property_bytes_slots_written = results
            .iter()
            .map(|(_, result)| u64::from(result.inline_property_bytes_slots_written))
            .sum();
        let locations = location_mode.captures().then(|| {
            join_physical_locations(edges, intents, &results)
                .expect("committed batch must publish one complete location per intent")
        });

        Ok(BatchEdgeInsertResult::Committed {
            edge_slots_written,
            inline_property_bytes_slots_written,
            locations,
            used_expansion,
        })
    }

    /// Convert physical intents into per-orientation batch write plans.
    fn build_one_orientation_batch_plans<E: CsrEdge>(
        &self,
        intents: &[BatchEdgeIntent],
        encode_edge: impl Fn(&BatchEdgeIntent) -> Result<E, BatchPlacementError>,
    ) -> Result<Vec<OneOrientationBatchWriteRequest<E>>, BatchPlacementError> {
        let mut runs_by_role: RapidHashMap<
            (BatchEdgeIntentRole, BatchPlacementKey),
            Vec<OneOrientationBatchEdge<E>>,
        > = RapidHashMap::default();

        for intent in intents {
            let key = BatchPlacementKey {
                orientation: intent.orientation,
                leaf_segment: super::batch_placement::leaf_index_for_vertex(
                    intent.owner_vertex_id,
                    super::batch_placement::segment_size(),
                ),
                owner_vertex_id: intent.owner_vertex_id,
                storage_label: intent.storage_label,
                inline_property_width: intent.inline_property_width,
            };
            let edge = encode_edge(intent)?;
            let entry = OneOrientationBatchEdge {
                logical_ordinal: intent.logical_ordinal,
                owner_vertex_id: intent.owner_vertex_id,
                neighbor_vertex_id: intent.neighbor_vertex_id,
                label_id: intent.storage_label,
                edge,
            };
            runs_by_role
                .entry((intent.role, key))
                .or_default()
                .push(entry);
        }

        // `expand_batch_edge_intents` emits intents in logical-input order. HashMap grouping
        // changes only which run is visited, not the order of entries within a run, so each run
        // is already ordinal-sorted when it reaches the LARA reserve contract.

        let mut grouped: RapidHashMap<
            BatchEdgeIntentRole,
            RapidHashMap<BatchPlacementKey, Vec<OneOrientationBatchEdge<E>>>,
        > = RapidHashMap::default();
        for ((role, key), edges) in runs_by_role {
            grouped.entry(role).or_default().insert(key, edges);
        }
        // The placement policy is per storage label. Resolve it once per distinct
        // label for the whole plan instead of once per run: under canbench (and in
        // production with a projected table) the resolution clones the resolved
        // label entry, so per-run resolution measurably regresses the plan build.
        let mut requests = grouped
            .into_iter()
            .map(|(role, runs)| OneOrientationBatchWriteRequest {
                orientation: match role {
                    BatchEdgeIntentRole::DerivedReverse => LabeledOrientation::Reverse,
                    _ => LabeledOrientation::Forward,
                },
                role,
                plan: OneOrientationBatchPlan {
                    runs: runs_from_map(runs),
                },
            })
            .collect::<Vec<_>>();
        requests.sort_by_key(|request| match request.role {
            BatchEdgeIntentRole::CanonicalForward => 0,
            BatchEdgeIntentRole::DerivedReverse => 1,
            BatchEdgeIntentRole::UndirectedOwnerForward => 2,
            BatchEdgeIntentRole::UndirectedAliasForward => 3,
        });

        Ok(requests)
    }

    fn build_merged_orientation_batch_plans<E: CsrEdge>(
        &self,
        intents: &[BatchEdgeIntent],
        encode_edge: impl Fn(&BatchEdgeIntent) -> Result<E, BatchPlacementError>,
    ) -> Result<Vec<(LabeledOrientation, OneOrientationBatchPlan<E>)>, BatchPlacementError> {
        let mut runs: RapidHashMap<BatchPlacementKey, Vec<OneOrientationBatchEdge<E>>> =
            RapidHashMap::default();
        for intent in intents {
            let key = BatchPlacementKey {
                orientation: intent.orientation,
                leaf_segment: super::batch_placement::leaf_index_for_vertex(
                    intent.owner_vertex_id,
                    super::batch_placement::segment_size(),
                ),
                owner_vertex_id: intent.owner_vertex_id,
                storage_label: intent.storage_label,
                inline_property_width: intent.inline_property_width,
            };
            runs.entry(key).or_default().push(OneOrientationBatchEdge {
                logical_ordinal: intent.logical_ordinal,
                owner_vertex_id: intent.owner_vertex_id,
                neighbor_vertex_id: intent.neighbor_vertex_id,
                label_id: intent.storage_label,
                edge: encode_edge(intent)?,
            });
        }
        // Intent expansion preserves logical-input order within each grouped run.
        let mut by_orientation: RapidHashMap<
            u8,
            RapidHashMap<BatchPlacementKey, Vec<OneOrientationBatchEdge<E>>>,
        > = RapidHashMap::default();
        for (key, edges) in runs {
            by_orientation
                .entry(match key.orientation {
                    LabeledOrientation::Forward => 0,
                    LabeledOrientation::Reverse => 1,
                })
                .or_default()
                .insert(key, edges);
        }
        // Resolve the per-storage-label placement once per distinct label; both
        // orientations share the sorted table built inside `runs_from_map`.
        let mut plans = by_orientation
            .into_iter()
            .map(|(orientation, runs)| {
                (
                    if orientation == 0 {
                        LabeledOrientation::Forward
                    } else {
                        LabeledOrientation::Reverse
                    },
                    OneOrientationBatchPlan {
                        runs: runs_from_map(runs),
                    },
                )
            })
            .collect::<Vec<_>>();
        plans.sort_by_key(|(orientation, _)| match orientation {
            LabeledOrientation::Forward => 0,
            LabeledOrientation::Reverse => 1,
        });
        Ok(plans)
    }

    #[cfg(test)]
    fn rollback_one_orientation_reservations(
        &self,
        reservations: Vec<(LabeledOrientation, BatchReservation<Edge>)>,
    ) {
        for (orientation, reservation) in reservations {
            self.with_graph_mut(|graph| graph.rollback_batch_reservation(orientation, reservation));
        }
    }
}

fn ordered_edge_label_stats_delta(edges: &[BatchEdgeInput]) -> LabelStatsDelta {
    let mut edge = Vec::new();
    for label in edges.iter().filter_map(|edge| edge.catalog_label) {
        if let Some((_, count)) = edge.iter_mut().find(|(existing, _)| *existing == label) {
            *count += 1;
        } else {
            edge.push((label, 1));
        }
    }
    LabelStatsDelta {
        vertex: Vec::new(),
        edge,
    }
}

fn ordered_hot_forward_vertices(edges: &[BatchEdgeInput]) -> Vec<LocalVertexId> {
    let mut counts = BTreeMap::<LocalVertexId, u32>::new();
    for edge in edges {
        let source = edge.source_vertex_id.into();
        *counts.entry(source).or_default() += 1;
    }
    counts
        .into_iter()
        .filter_map(|(vertex, count)| {
            (count >= HOT_FORWARD_EDGE_INSERT_THRESHOLD).then_some(vertex)
        })
        .collect()
}

fn encode_intent_edge(intent: &BatchEdgeIntent) -> Result<Edge, BatchPlacementError> {
    if intent.inline_property_width == 0 {
        Ok(build_edge_to(intent.neighbor_vertex_id))
    } else {
        Ok(
            build_edge_to(intent.neighbor_vertex_id).with_stored_inline_property_bytes(
                intent.inline_property_width,
                &intent.inline_property_bytes,
            ),
        )
    }
}

fn join_physical_locations(
    inputs: &[BatchEdgeInput],
    intents: &[BatchEdgeIntent],
    results: &[(LabeledOrientation, OneOrientationBatchResult)],
) -> Result<Vec<BatchEdgePhysicalLocation>, BatchLocationJoinError> {
    let orientation_key = |orientation: LabeledOrientation| match orientation {
        LabeledOrientation::Forward => 0u8,
        LabeledOrientation::Reverse => 1u8,
    };
    let mut intent_by_key = RapidHashMap::with_capacity(intents.len());
    for intent in intents {
        let key = (
            intent.logical_ordinal,
            orientation_key(intent.orientation),
            intent.owner_vertex_id,
        );
        if intent_by_key.insert(key, intent.role).is_some() {
            return Err(BatchLocationJoinError::Duplicate {
                logical_ordinal: intent.logical_ordinal,
                role: intent.role,
            });
        }
    }
    let mut by_key = RapidHashMap::with_capacity(intents.len());
    for (orientation, result) in results {
        for location in result
            .locations
            .as_ref()
            .expect("location join requires capture mode")
        {
            let role = intent_by_key
                .get(&(
                    location.logical_ordinal,
                    orientation_key(*orientation),
                    location.owner_vertex_id,
                ))
                .copied()
                .ok_or(BatchLocationJoinError::Unexpected {
                    logical_ordinal: location.logical_ordinal,
                    owner_vertex_id: location.owner_vertex_id,
                })?;
            let key = (location.logical_ordinal, role);
            if by_key.insert(key, *location).is_some() {
                return Err(BatchLocationJoinError::Duplicate {
                    logical_ordinal: location.logical_ordinal,
                    role,
                });
            }
        }
    }

    let mut joined = Vec::with_capacity(inputs.len());
    for (logical_ordinal, input) in inputs.iter().enumerate() {
        let logical_ordinal = u32::try_from(logical_ordinal).expect("input ordinal is bounded");
        if input.directed {
            let forward = *by_key
                .get(&(logical_ordinal, BatchEdgeIntentRole::CanonicalForward))
                .ok_or(BatchLocationJoinError::Missing {
                    logical_ordinal,
                    role: BatchEdgeIntentRole::CanonicalForward,
                })?;
            let reverse = *by_key
                .get(&(logical_ordinal, BatchEdgeIntentRole::DerivedReverse))
                .ok_or(BatchLocationJoinError::Missing {
                    logical_ordinal,
                    role: BatchEdgeIntentRole::DerivedReverse,
                })?;
            joined.push(BatchEdgePhysicalLocation::Directed {
                logical_ordinal,
                forward,
                reverse,
            });
        } else {
            let owner = *by_key
                .get(&(logical_ordinal, BatchEdgeIntentRole::UndirectedOwnerForward))
                .ok_or(BatchLocationJoinError::Missing {
                    logical_ordinal,
                    role: BatchEdgeIntentRole::UndirectedOwnerForward,
                })?;
            if input.source_vertex_id == input.target_vertex_id {
                joined.push(BatchEdgePhysicalLocation::UndirectedSelfLoop {
                    logical_ordinal,
                    location: owner,
                });
            } else {
                let alias = *by_key
                    .get(&(logical_ordinal, BatchEdgeIntentRole::UndirectedAliasForward))
                    .ok_or(BatchLocationJoinError::Missing {
                        logical_ordinal,
                        role: BatchEdgeIntentRole::UndirectedAliasForward,
                    })?;
                joined.push(BatchEdgePhysicalLocation::Undirected {
                    logical_ordinal,
                    owner,
                    alias,
                });
            }
        }
    }
    Ok(joined)
}

fn runs_from_map<E: CsrEdge>(
    map: RapidHashMap<BatchPlacementKey, Vec<OneOrientationBatchEdge<E>>>,
) -> Vec<OneOrientationBucketRun<E>> {
    // Resolve the per-storage-label placement once per distinct label: the
    // resolution clones the resolved label entry (test_labels fallback under
    // canbench, projected table in production), so per-run resolution would
    // dominate the plan build. Runs binary-search the small sorted table instead
    // of paying a hash-map probe per run.
    let placements: Vec<(LaraLabelId, EdgePlacementPolicy)> = {
        let mut labels: Vec<LaraLabelId> = map.keys().map(|key| key.storage_label).collect();
        labels.sort_unstable();
        labels.dedup();
        labels
            .into_iter()
            .map(|label| (label, run_placement_for_storage_label(label)))
            .collect()
    };
    let mut runs: Vec<_> = map.into_iter().collect();
    runs.sort_by_key(|(key, _)| *key);
    runs.into_iter()
        .map(|(key, edges)| OneOrientationBucketRun {
            owner_vertex_id: key.owner_vertex_id,
            label_id: key.storage_label,
            inline_property_width: key.inline_property_width,
            placement: placements
                .binary_search_by_key(&key.storage_label, |(label, _)| *label)
                .map(|index| placements[index].1)
                .unwrap_or(EdgePlacementPolicy::Unordered),
            edges,
        })
        .collect()
}

/// Resolves the LARA placement policy for one batch run from its storage label,
/// mirroring the scalar mutation boundary: the storage label maps back to the
/// catalog label, whose resolved ordering policy becomes the placement
/// (ADR 0052 §4; undeclared/unknown -> Unordered). During execution the active
/// resolved-label table is set; host tests without one fall back to Unordered.
fn run_placement_for_storage_label(storage: LaraLabelId) -> EdgePlacementPolicy {
    lara_edge_placement(
        catalog_edge_label_from_wire(storage)
            .and_then(|label| resolved_edge_label_with(None, label).map(|l| l.ordering)),
    )
}

fn merge_one_orientation_batch_plans<E: CsrEdge>(
    first: &OneOrientationBatchPlan<E>,
    second: &OneOrientationBatchPlan<E>,
) -> OneOrientationBatchPlan<E> {
    let mut runs = RapidHashMap::<BatchPlacementKey, Vec<OneOrientationBatchEdge<E>>>::default();
    for run in first.runs.iter().chain(second.runs.iter()) {
        let key = BatchPlacementKey {
            orientation: LabeledOrientation::Forward,
            leaf_segment: super::batch_placement::leaf_index_for_vertex(
                run.owner_vertex_id,
                super::batch_placement::segment_size(),
            ),
            owner_vertex_id: run.owner_vertex_id,
            storage_label: run.label_id,
            inline_property_width: run.inline_property_width,
        };
        runs.entry(key)
            .or_default()
            .extend(run.edges.iter().cloned());
    }
    for edges in runs.values_mut() {
        edges.sort_by_key(|edge| edge.logical_ordinal);
    }
    OneOrientationBatchPlan {
        runs: runs_from_map(runs),
    }
}

fn build_undirected_batch_pairs(
    intents: &[BatchEdgeIntent],
) -> Result<Vec<UndirectedBatchPair>, BatchPlacementError> {
    let mut by_ordinal: RapidHashMap<u32, Vec<&BatchEdgeIntent>> = RapidHashMap::default();
    for intent in intents {
        by_ordinal
            .entry(intent.logical_ordinal)
            .or_default()
            .push(intent);
    }
    let mut pairs = Vec::with_capacity(by_ordinal.len());
    for (logical_ordinal, projections) in by_ordinal {
        if projections.len() != 2 {
            return Err(BatchPlacementError::PhysicalProjectionCountMismatch {
                logical_ordinal,
                expected: 2,
                actual: projections.len(),
            });
        }
        let lower_owner_vertex_id = projections
            .iter()
            .map(|intent| intent.owner_vertex_id)
            .min()
            .expect("two projections are non-empty");
        let higher_owner_vertex_id = projections
            .iter()
            .map(|intent| intent.owner_vertex_id)
            .max()
            .expect("two projections are non-empty");
        let first = projections[0];
        if projections.iter().any(|intent| {
            intent.storage_label != first.storage_label
                || intent.inline_property_width != first.inline_property_width
        }) {
            return Err(BatchPlacementError::PhysicalProjectionMetadataMismatch {
                logical_ordinal,
            });
        }
        pairs.push(UndirectedBatchPair {
            logical_ordinal,
            lower_owner_vertex_id,
            higher_owner_vertex_id,
            label_id: first.storage_label,
            inline_property_width: first.inline_property_width,
        });
    }
    pairs.sort_by_key(|pair| pair.logical_ordinal);
    Ok(pairs)
}

fn build_mixed_batch_pairs(
    inputs: &[BatchEdgeInput],
    intents: &[BatchEdgeIntent],
) -> Result<Vec<BatchLogicalPair>, BatchPlacementError> {
    let mut intents_by_ordinal: RapidHashMap<u32, Vec<&BatchEdgeIntent>> = RapidHashMap::default();
    for intent in intents {
        intents_by_ordinal
            .entry(intent.logical_ordinal)
            .or_default()
            .push(intent);
    }
    let mut pairs = Vec::with_capacity(inputs.len());
    for (ordinal, input) in inputs.iter().enumerate() {
        let logical_ordinal =
            u32::try_from(ordinal).map_err(|_| BatchPlacementError::OrdinalOverflow)?;
        let projections = intents_by_ordinal.get(&logical_ordinal).ok_or_else(|| {
            BatchPlacementError::PlacementReadFailed(format!(
                "missing physical projections for logical ordinal {logical_ordinal}"
            ))
        })?;
        let undirected = !input.directed;
        let self_loop = input.source_vertex_id == input.target_vertex_id;
        let expected = if undirected && self_loop { 1 } else { 2 };
        if projections.len() != expected {
            return Err(BatchPlacementError::PlacementReadFailed(format!(
                "logical ordinal {logical_ordinal} produced {} projections, expected {expected}",
                projections.len()
            )));
        }
        let first = projections[0];
        let (endpoint_a, endpoint_b) = if undirected {
            (
                input.source_vertex_id.min(input.target_vertex_id),
                input.source_vertex_id.max(input.target_vertex_id),
            )
        } else {
            (input.source_vertex_id, input.target_vertex_id)
        };
        let kind = match (input.directed, self_loop) {
            (true, false) => BatchLogicalPairKind::Directed,
            (true, true) => BatchLogicalPairKind::DirectedSelfLoop,
            (false, false) => BatchLogicalPairKind::Undirected,
            (false, true) => BatchLogicalPairKind::UndirectedSelfLoop,
        };
        pairs.push(BatchLogicalPair {
            logical_ordinal,
            kind,
            endpoint_a,
            endpoint_b,
            label_id: first.storage_label,
            inline_property_width: first.inline_property_width,
        });
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::super::batch_placement::BatchEdgeInput;
    use super::super::store::helpers::{edge_storage_label, lara_label};
    use super::*;
    use crate::test_labels::install_test_edge_inline_property_profile;
    use gleaph_graph_kernel::entry::EdgeLabelId;
    use ic_stable_lara::VertexId;
    use ic_stable_lara::labeled::batch_write::{
        OneOrientationBatchLocation, OneOrientationPhysicalLocation,
    };
    use ic_stable_lara::lara::edge::free_span::FreeSpanAllocatorStats;
    use ic_stable_lara::lara::edge_inline_property::InlinePropertyBytesAllocatorStats;

    fn fresh_store() -> GraphStore {
        GraphStore::new()
    }

    fn make_vertices(store: &GraphStore, n: u32) -> Vec<VertexId> {
        (0..n)
            .map(|_| store.insert_vertex().expect("vertex"))
            .collect()
    }

    /// One vertex's observable adjacency: the edges in ordinal order, with the neighbor written
    /// as its position inside the group under test and the inline bytes the read boundary hands
    /// back. Order is compared; absolute slot addresses and allocator bookkeeping are not - a
    /// batch may allocate differently, it may not reorder or drop edges.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ObservableView {
        out: Vec<(u8, u16, Vec<u8>)>,
        undirected: Vec<(u8, u16, Vec<u8>)>,
        in_: Vec<(u8, u16, Vec<u8>)>,
    }

    impl ObservableView {
        fn of(store: &GraphStore, vid: VertexId, group: &[VertexId]) -> Self {
            let position = |target: VertexId| -> u8 {
                group
                    .iter()
                    .position(|candidate| *candidate == target)
                    .map_or(u8::MAX, |index| index as u8)
            };
            let project = |edges: Vec<gleaph_graph_kernel::entry::Edge>| {
                edges
                    .into_iter()
                    .map(|edge| {
                        let target = edge.target.local_id();
                        (
                            position(target),
                            edge.label_id,
                            edge.inline_property.as_slice().to_vec(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            Self {
                out: project(store.directed_out_edges(vid).expect("out edges")),
                undirected: project(store.undirected_edges(vid).expect("undirected edges")),
                in_: project(store.directed_in_edges(vid).expect("in edges")),
            }
        }
    }

    fn observable_state(store: &GraphStore, group: &[VertexId]) -> Vec<ObservableView> {
        group
            .iter()
            .map(|vid| ObservableView::of(store, *vid, group))
            .collect()
    }

    /// The reference implementation: every edge through the scalar path, one at a time.
    fn scalar_edges(store: &GraphStore, edges: &[BatchEdgeInput]) {
        store.execute_ordered_scalar_edges(edges, 0);
    }

    /// The production shape: try the clean-slab batch, and on recoverable geometry re-run the
    /// same logical edges through the scalar path. This is what a caller does, so what it leaves
    /// behind is what a caller can observe.
    fn batch_edges_with_scalar_fallback(store: &GraphStore, edges: &[BatchEdgeInput]) -> bool {
        match store
            .try_insert_batch_edges_clean_slab_with_locations(edges)
            .expect("plan/encode ok")
        {
            BatchEdgeInsertResult::Committed { .. } => true,
            BatchEdgeInsertResult::RecoverableGeometry { .. } => {
                scalar_edges(store, edges);
                false
            }
        }
    }

    /// The shadow window must be transparent: what a reader sees through the overlay has to equal
    /// what it sees once the overlay is published into the backend memory. This is the property the
    /// batch rewrite leans on - the ladder runs against the overlay and the heap caches describe the
    /// same bytes the commit writes out.
    #[test]
    fn a_shadowed_insert_reads_back_the_same_graph_after_commit() {
        let guard = ic_stable_memory_backend::enter_shadow();
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(3101);
        install_width(label, 1);
        let v = make_vertices(&store, 4);
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![7]),
            input(v[2], v[1], Some(label), true, vec![9]),
        ];
        scalar_edges(&store, &edges);

        let through_overlay = observable_state(&store, &v);
        let dirty = guard.dirty_bytes();
        let ranges = guard.dirty_ranges();
        guard
            .commit()
            .expect("the overlay must fit the backend memory");

        assert!(
            dirty > 0,
            "the insert must have written through the overlay, not around it"
        );
        assert_eq!(
            through_overlay,
            observable_state(&store, &v),
            "commit published {dirty} bytes in {ranges} ranges and changed what a reader sees"
        );
    }

    fn input(
        source: VertexId,
        target: VertexId,
        label: Option<EdgeLabelId>,
        directed: bool,
        bytes: Vec<u8>,
    ) -> BatchEdgeInput {
        BatchEdgeInput {
            source_vertex_id: source,
            target_vertex_id: target,
            catalog_label: label,
            directed,
            inline_property_bytes: bytes,
            initial_edge_properties: Vec::new(),
        }
    }

    fn captured_location(
        owner_vertex_id: VertexId,
        logical_slot: u32,
    ) -> OneOrientationBatchLocation {
        OneOrientationBatchLocation {
            logical_ordinal: 0,
            owner_vertex_id,
            logical_slot,
            location: OneOrientationPhysicalLocation::OverflowLog {
                leaf: 0,
                edge_entry_idx: logical_slot,
                inline_property_bytes_entry_idx: None,
            },
        }
    }

    #[test]
    fn canonical_sidecar_occurrence_uses_owner_and_logical_slot() {
        let label = EdgeLabelId::from_raw(4001);
        let source = VertexId::from(10);
        let target = VertexId::from(20);

        let directed_input = input(source, target, Some(label), true, vec![]);
        let directed = BatchEdgePhysicalLocation::Directed {
            logical_ordinal: 0,
            forward: captured_location(source, 7),
            reverse: captured_location(target, 11),
        };
        assert_eq!(
            directed.canonical_occurrence(&directed_input),
            CanonicalEdgeOccurrence {
                orientation: LabeledOrientation::Forward,
                owner_vertex_id: source,
                label_id: lara_label(edge_storage_label(Some(label), false)),
                slot_index: 7.into(),
            }
        );

        let undirected_input = input(source, target, Some(label), false, vec![]);
        let undirected = BatchEdgePhysicalLocation::Undirected {
            logical_ordinal: 0,
            owner: captured_location(target, 13),
            alias: captured_location(source, 5),
        };
        assert_eq!(
            undirected.canonical_occurrence(&undirected_input),
            CanonicalEdgeOccurrence {
                orientation: LabeledOrientation::Forward,
                owner_vertex_id: target,
                label_id: lara_label(edge_storage_label(Some(label), true)),
                slot_index: 13.into(),
            }
        );

        let self_loop_input = input(source, source, Some(label), false, vec![]);
        let self_loop = BatchEdgePhysicalLocation::UndirectedSelfLoop {
            logical_ordinal: 0,
            location: captured_location(source, 17),
        };
        assert_eq!(
            self_loop.canonical_occurrence(&self_loop_input),
            CanonicalEdgeOccurrence {
                orientation: LabeledOrientation::Forward,
                owner_vertex_id: source,
                label_id: lara_label(edge_storage_label(Some(label), true)),
                slot_index: 17.into(),
            }
        );
    }

    fn storage_label_for(catalog_label: Option<EdgeLabelId>, directed: bool) -> u16 {
        lara_label(edge_storage_label(catalog_label, !directed)).raw()
    }

    fn install_width(label: EdgeLabelId, width: u16) {
        install_test_edge_inline_property_profile(
            label,
            gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
                byte_width: width,
                encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawBytes,
            },
        );
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct AllocatorSnapshot {
        forward_edge_capacity: u64,
        reverse_edge_capacity: u64,
        forward_edge_free: FreeSpanAllocatorStats,
        reverse_edge_free: FreeSpanAllocatorStats,
        forward_inline_property_bytes: InlinePropertyBytesAllocatorStats,
        reverse_inline_property_bytes: InlinePropertyBytesAllocatorStats,
    }

    fn allocator_snapshot(store: &GraphStore) -> AllocatorSnapshot {
        store.with_graph_mut(|graph| AllocatorSnapshot {
            forward_edge_capacity: graph.forward().edges().header().elem_capacity,
            reverse_edge_capacity: graph.reverse().edges().header().elem_capacity,
            forward_edge_free: graph.forward().edges().allocator_stats(),
            reverse_edge_free: graph.reverse().edges().allocator_stats(),
            forward_inline_property_bytes: graph.forward().values().allocator_stats(),
            reverse_inline_property_bytes: graph.reverse().values().allocator_stats(),
        })
    }

    fn count_labeled_dir_edges(
        store: &GraphStore,
        vertex_id: VertexId,
        storage_label: u16,
        outgoing: bool,
    ) -> usize {
        let edges = if outgoing {
            store.directed_out_edges(vertex_id).expect("out")
        } else {
            store.directed_in_edges(vertex_id).expect("in")
        };
        edges
            .into_iter()
            .filter(|e| e.label_id == storage_label)
            .count()
    }

    #[test]
    fn location_join_rejects_missing_and_duplicate_entries() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(7001);
        install_width(label, 0);
        let vertices = make_vertices(&store, 2);
        let input = input(vertices[0], vertices[1], Some(label), true, vec![]);
        let intents = store
            .expand_batch_edge_intents(std::slice::from_ref(&input))
            .expect("intents");
        let forward = intents
            .iter()
            .find(|intent| intent.role == BatchEdgeIntentRole::CanonicalForward)
            .expect("forward intent");
        let location = OneOrientationBatchLocation {
            logical_ordinal: forward.logical_ordinal,
            owner_vertex_id: forward.owner_vertex_id,
            logical_slot: 0,
            location: OneOrientationPhysicalLocation::Slab {
                edge_slot: 10,
                inline_property_bytes_offset: None,
            },
        };
        let result = OneOrientationBatchResult {
            edge_slots_written: 1,
            edge_log_entries_written: 0,
            inline_property_bytes_slots_written: 0,
            inline_property_bytes_log_entries_written: 0,
            locations: Some(vec![location]),
        };
        assert!(matches!(
            join_physical_locations(
                std::slice::from_ref(&input),
                &intents,
                &[(LabeledOrientation::Forward, result.clone())],
            ),
            Err(BatchLocationJoinError::Missing {
                role: BatchEdgeIntentRole::DerivedReverse,
                ..
            })
        ));
        assert!(matches!(
            join_physical_locations(
                std::slice::from_ref(&input),
                &intents,
                &[(
                    LabeledOrientation::Forward,
                    OneOrientationBatchResult {
                        locations: Some(vec![location, location]),
                        ..result
                    }
                )],
            ),
            Err(BatchLocationJoinError::Duplicate {
                role: BatchEdgeIntentRole::CanonicalForward,
                ..
            })
        ));
    }
}
