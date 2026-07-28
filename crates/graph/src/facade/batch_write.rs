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
    GraphOrderedEdgeBatchResultV1, LabelStatsDelta, MutationId,
};
use ic_stable_lara::VertexId;
#[cfg(test)]
use ic_stable_lara::labeled::batch_write::BatchReservation;
use ic_stable_lara::labeled::batch_write::{
    BatchLocationMode, BatchLogicalPair, BatchLogicalPairKind, BidirectionalBatchPlan,
    OneOrientationBatchEdge, OneOrientationBatchLocation, OneOrientationBatchPlan,
    OneOrientationBatchResult, OneOrientationBucketRun, UndirectedBatchPair,
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
use super::store::helpers::{build_edge_to, edge_storage_label, lara_label};

/// Result of attempting a clean-slab batch edge insert through GraphStore.
///
/// - `Committed`: every required one-orientation reservation succeeded and was
///   committed. The contained results are ordered by orientation.
/// - `Unsupported`: at least one orientation could not be reserved on the clean-
///   slab path. No canonical write was published by this attempt; the caller
///   may fall back to the existing scalar insertion path.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    Unsupported {
        /// Human-readable reason the clean-slab path could not be used.
        reason: String,
    },
}

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
            Self::Unsupported { .. } => None,
        }
    }

    /// Total inline property bytes slab slots written across all committed orientations.
    pub(crate) fn total_inline_property_bytes_slots(&self) -> Option<u64> {
        match self {
            Self::Committed {
                inline_property_bytes_slots_written,
                ..
            } => Some(*inline_property_bytes_slots_written),
            Self::Unsupported { .. } => None,
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
    /// Execute the supported ordered clean-slab path and publish its Graph-owned receipt.
    ///
    /// All fallible placement and sidecar validation occurs in the batch writer before the
    /// first LARA commit. Once that commit succeeds, delta/journal persistence is part of the
    /// same no-`await` Graph update section; an invariant failure traps rather than returning a
    /// recoverable error after canonical state has changed.
    pub(crate) fn execute_ordered_edge_batch_clean_slab(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        edges: &[BatchEdgeInput],
    ) -> Result<GraphOrderedEdgeBatchResult, String> {
        let result = self
            .try_insert_ordered_edge_batch_clean_slab(edges)
            .map_err(|error| format!("ordered Graph batch validation failed: {error}"))?;
        if let BatchEdgeInsertResult::Unsupported { reason } = result {
            return Err(format!(
                "ordered Graph clean-slab geometry is unsupported: {reason}"
            ));
        }

        Ok(self.commit_ordered_edge_batch_receipt(mutation_id, request_identity, edges))
    }

    pub(crate) fn execute_ordered_edge_batch_clean_slab_with_intents(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        edges: &[BatchEdgeInput],
        intents: &[BatchEdgeIntent],
    ) -> Result<GraphOrderedEdgeBatchResult, String> {
        let result = self
            .try_insert_ordered_edge_batch_clean_slab_with_intents(edges, intents)
            .map_err(|error| format!("ordered Graph batch validation failed: {error}"))?;
        if let BatchEdgeInsertResult::Unsupported { reason } = result {
            return Err(format!(
                "ordered Graph clean-slab geometry is unsupported: {reason}"
            ));
        }

        Ok(self.commit_ordered_edge_batch_receipt(mutation_id, request_identity, edges))
    }

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
        self.execute_ordered_scalar_edges(edges);
        self.commit_ordered_edge_batch_receipt(mutation_id, request_identity, edges)
    }

    /// Apply the scalar owner boundary for a subset of a mixed ordered request.
    ///
    /// The caller must have completed whole-request planning before invoking this helper. Any
    /// unexpected write error traps so a prior batch commit in the same Graph message rolls back.
    pub(crate) fn execute_ordered_scalar_edges(&self, edges: &[BatchEdgeInput]) {
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
                )
                .unwrap_or_else(|error| {
                    panic!("ordered Graph scalar fallback failed after preflight: {error}")
                });
            }
        }
    }

    /// Execute a mixed ordered request whose multi-intent logical items can use the batch writer
    /// and whose singleton logical items can use the scalar owner boundary. The batch attempt is
    /// made before scalar writes; an unsupported batch reservation therefore falls back to the
    /// complete request without any canonical write having occurred.
    pub(crate) fn execute_ordered_edge_batch_partitioned(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        edges: &[BatchEdgeInput],
        batch_ordinals: &BTreeSet<u32>,
    ) -> Result<GraphOrderedEdgeBatchResult, String> {
        self.execute_ordered_edge_batch_partitioned_with_intents(
            mutation_id,
            request_identity,
            edges,
            batch_ordinals,
            None,
        )
    }

    pub(crate) fn execute_ordered_edge_batch_partitioned_with_intents(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        edges: &[BatchEdgeInput],
        batch_ordinals: &BTreeSet<u32>,
        prepared_intents: Option<&[BatchEdgeIntent]>,
    ) -> Result<GraphOrderedEdgeBatchResult, String> {
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

        let local_batch_intents = prepared_intents.map(|intents| {
            let local_ordinals = batch_ordinals
                .iter()
                .enumerate()
                .map(|(local, original)| (*original, local as u32))
                .collect::<BTreeMap<_, _>>();
            intents
                .iter()
                .filter_map(|intent| {
                    local_ordinals.get(&intent.logical_ordinal).map(|ordinal| {
                        let mut intent = intent.clone();
                        intent.logical_ordinal = *ordinal;
                        intent
                    })
                })
                .collect::<Vec<_>>()
        });

        let batch_result = match local_batch_intents.as_deref() {
            Some(intents) => {
                self.try_insert_ordered_edge_batch_clean_slab_with_intents(&batch_edges, intents)
            }
            None => self.try_insert_ordered_edge_batch_clean_slab(&batch_edges),
        };
        match batch_result {
            Ok(BatchEdgeInsertResult::Committed { .. }) => {
                self.execute_ordered_scalar_edges(&scalar_edges);
                Ok(self.commit_ordered_edge_batch_receipt(mutation_id, request_identity, edges))
            }
            Ok(BatchEdgeInsertResult::Unsupported { .. }) => Ok(self
                .execute_ordered_edge_batch_scalar_fallback(mutation_id, request_identity, edges)),
            Err(error) => Err(format!(
                "ordered Graph mixed batch validation failed: {error}"
            )),
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
    /// returns [`BatchEdgeInsertResult::Unsupported`] without writing any canonical
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

    /// Insert a clean-slab batch and commit its initial canonical sidecars.
    ///
    /// Location capture is mandatory for this path. Property validation happens
    /// before reservation, and sidecars are addressed directly by the captured
    /// canonical owner/label/logical-slot tuple after LARA commit.
    pub(crate) fn try_insert_batch_edges_clean_slab_with_initial_properties(
        &self,
        edges: &[super::batch_placement::BatchEdgeInput],
    ) -> Result<BatchEdgeInsertResult, BatchPlacementError> {
        let result =
            self.try_insert_batch_edges_clean_slab_with_mode(edges, BatchLocationMode::Capture)?;
        if let BatchEdgeInsertResult::Committed {
            locations: Some(locations),
            ..
        } = &result
        {
            for (input, location) in edges.iter().zip(locations) {
                let occurrence = location.canonical_occurrence(input);
                let handle = EdgeHandle::at_slot(
                    occurrence.owner_vertex_id,
                    occurrence.label_id,
                    occurrence.slot_index,
                );
                self.commit_edge_property_writes_at_canonical(
                    handle,
                    &input.initial_edge_properties,
                );
            }
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
            return Ok(BatchEdgeInsertResult::Unsupported {
                reason: "empty batch is not admitted to the clean-slab path".into(),
            });
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
                    return Ok(BatchEdgeInsertResult::Unsupported {
                        reason: "physical intent roles do not form one logical batch shape".into(),
                    });
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
                return Ok(BatchEdgeInsertResult::Unsupported {
                    reason: format!("{err}"),
                });
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
    let mut runs: Vec<_> = map.into_iter().collect();
    runs.sort_by_key(|(key, _)| *key);
    runs.into_iter()
        .map(|(key, edges)| OneOrientationBucketRun {
            owner_vertex_id: key.owner_vertex_id,
            label_id: key.storage_label,
            inline_property_width: key.inline_property_width,
            edges,
        })
        .collect()
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
    use crate::test_labels::{
        install_test_edge_inline_property, install_test_edge_inline_property_profile,
    };
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId, PropertyId};
    use ic_stable_lara::labeled::batch_write::{
        OneOrientationBatchLocation, OneOrientationPhysicalLocation,
    };
    use ic_stable_lara::lara::edge::free_span::FreeSpanAllocatorStats;
    use ic_stable_lara::lara::edge_inline_property::InlinePropertyBytesAllocatorStats;
    use ic_stable_lara::{MaintenanceBudget, VertexId};

    fn fresh_store() -> GraphStore {
        GraphStore::new()
    }

    fn make_vertices(store: &GraphStore, n: u32) -> Vec<VertexId> {
        (0..n)
            .map(|_| store.insert_vertex().expect("vertex"))
            .collect()
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

    #[test]
    fn initial_sidecar_is_written_at_captured_canonical_slot() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4002);
        let property_id = PropertyId::from_raw(77);
        let vertices = make_vertices(&store, 2);
        store.prepare_clean_slab_dir_buckets(vertices[0], vertices[1], label, 0);
        let mut edge = input(vertices[0], vertices[1], Some(label), true, vec![]);
        edge.initial_edge_properties = vec![
            (property_id, Value::Int64(42)),
            (PropertyId::from_raw(79), Value::Text("batch".into())),
        ];

        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&[edge.clone()])
            .expect("batch sidecar write");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        let occurrence = locations[0].canonical_occurrence(&edge);
        let handle = EdgeHandle::at_slot(
            occurrence.owner_vertex_id,
            occurrence.label_id,
            occurrence.slot_index,
        );
        assert_eq!(
            store.edge_property_at_canonical_handle(handle, property_id),
            Some(Value::Int64(42))
        );
        assert_eq!(
            store.edge_property_at_canonical_handle(handle, PropertyId::from_raw(79)),
            Some(Value::Text("batch".into()))
        );
    }

    #[test]
    fn mixed_ordered_write_batches_multi_runs_and_scalars_singletons_once() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4011);
        let vertices = make_vertices(&store, 6);
        for (source, target) in [(vertices[0], vertices[1]), (vertices[2], vertices[3])] {
            store.prepare_clean_slab_dir_buckets(source, target, label, 0);
            store.prepare_clean_slab_dir_buckets(target, source, label, 0);
        }
        let edges = vec![
            input(vertices[0], vertices[1], Some(label), true, vec![]),
            input(vertices[0], vertices[1], Some(label), true, vec![]),
            input(vertices[2], vertices[3], Some(label), true, vec![]),
        ];
        let classification = store
            .classify_batch_edge_insertion(&edges)
            .expect("classify");
        assert_eq!(
            classification
                .logical_ordinals_with_multi_runs
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: [0x42; 32],
            logical_item_count: 3,
        };
        let result = store
            .execute_ordered_edge_batch_partitioned_with_intents(
                4_011_001,
                identity,
                &edges,
                &classification.logical_ordinals_with_multi_runs,
                Some(&classification.intents),
            )
            .expect("mixed ordered write");
        let receipt = match result {
            GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1::Completed(receipt)) => {
                receipt
            }
            other => panic!("expected completed receipt, got {other:?}"),
        };
        assert_eq!(receipt.logical_edge_count, 3);
        let delta_seq = receipt
            .emitted_delta_first_seq
            .expect("mixed labeled edge emits a delta");
        assert_eq!(
            store
                .pending_label_stats_deltas(delta_seq, 1)
                .into_iter()
                .next()
                .expect("mixed label delta")
                .label_stats_delta
                .edge,
            vec![(label, 3)]
        );
    }

    #[test]
    fn ordered_clean_slab_write_publishes_delta_receipt_and_journal() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4010);
        let vertices = make_vertices(&store, 2);
        store.prepare_clean_slab_dir_buckets(vertices[0], vertices[1], label, 0);
        let mutation_id = 4_010_001;
        let fingerprint = [0x41; 32];
        let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: fingerprint,
            logical_item_count: 1,
        };

        let result = store
            .execute_ordered_edge_batch_clean_slab(
                mutation_id,
                identity.clone(),
                &[input(vertices[0], vertices[1], Some(label), true, vec![])],
            )
            .expect("ordered clean-slab write");
        let receipt = match result {
            GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1::Completed(receipt)) => {
                receipt
            }
            other => panic!("expected completed receipt, got {other:?}"),
        };
        assert_eq!(receipt.logical_edge_count, 1);
        let delta_seq = receipt
            .emitted_delta_first_seq
            .expect("labeled edge emits a delta");
        assert_eq!(receipt.emitted_delta_last_seq, Some(delta_seq));
        assert_eq!(
            store
                .pending_label_stats_deltas(delta_seq, 1)
                .into_iter()
                .next()
                .expect("label delta")
                .label_stats_delta
                .edge,
            vec![(label, 1)]
        );

        let replay = store
            .ordered_edge_batch_replay_result(mutation_id, &identity)
            .expect("journal lookup")
            .expect("completed journal");
        assert_eq!(
            replay,
            GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1::Completed(receipt))
        );
    }

    #[test]
    fn initial_sidecar_stays_on_undirected_owner_and_self_loop_owner() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4005);
        let owner_property_id = PropertyId::from_raw(80);
        let loop_property_id = PropertyId::from_raw(81);
        let vertices = make_vertices(&store, 3);
        let low = vertices[0];
        let high = vertices[1];
        let loop_vertex = vertices[2];
        store.prepare_clean_slab_undir_buckets(low, high, label, 0);
        store.prepare_clean_slab_undir_buckets(loop_vertex, loop_vertex, label, 0);

        let mut undirected = input(low, high, Some(label), false, vec![]);
        undirected.initial_edge_properties = vec![(owner_property_id, Value::Int64(80))];
        let mut self_loop = input(loop_vertex, loop_vertex, Some(label), false, vec![]);
        self_loop.initial_edge_properties = vec![(loop_property_id, Value::Int64(81))];
        let edges = vec![undirected.clone(), self_loop.clone()];

        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&edges)
            .expect("undirected sidecar batch");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };

        let owner_occurrence = locations[0].canonical_occurrence(&undirected);
        let owner_handle = EdgeHandle::at_slot(
            owner_occurrence.owner_vertex_id,
            owner_occurrence.label_id,
            owner_occurrence.slot_index,
        );
        assert_eq!(
            owner_occurrence.owner_vertex_id, high,
            "undirected sidecars belong to the canonical higher owner"
        );
        assert_eq!(
            store.edge_property_at_canonical_handle(owner_handle, owner_property_id),
            Some(Value::Int64(80))
        );

        let alias_location = match &locations[0] {
            BatchEdgePhysicalLocation::Undirected { alias, .. } => alias,
            other => panic!("expected undirected location, got {other:?}"),
        };
        let alias_handle = EdgeHandle::at_slot(
            alias_location.owner_vertex_id,
            owner_occurrence.label_id,
            alias_location.logical_slot,
        );
        assert_eq!(
            store.edge_property_at_canonical_handle(alias_handle, owner_property_id),
            None,
            "alias row must not receive the canonical sidecar"
        );

        let loop_occurrence = locations[1].canonical_occurrence(&self_loop);
        let loop_handle = EdgeHandle::at_slot(
            loop_occurrence.owner_vertex_id,
            loop_occurrence.label_id,
            loop_occurrence.slot_index,
        );
        assert_eq!(loop_occurrence.owner_vertex_id, loop_vertex);
        assert_eq!(
            store.edge_property_at_canonical_handle(loop_handle, loop_property_id),
            Some(Value::Int64(81))
        );
    }

    #[test]
    fn initial_sidecars_follow_logical_ordinals_in_mixed_batch() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4006);
        let vertices = make_vertices(&store, 6);
        let directed_source = vertices[0];
        let directed_target = vertices[1];
        let undirected_low = vertices[2];
        let undirected_high = vertices[3];
        let loop_vertex = vertices[4];
        store.prepare_clean_slab_dir_buckets(directed_source, directed_target, label, 0);
        store.prepare_clean_slab_undir_buckets(undirected_low, undirected_high, label, 0);
        store.prepare_clean_slab_undir_buckets(loop_vertex, loop_vertex, label, 0);

        let mut directed = input(directed_source, directed_target, Some(label), true, vec![]);
        directed.initial_edge_properties = vec![(PropertyId::from_raw(90), Value::Int64(900))];
        let mut undirected = input(undirected_low, undirected_high, Some(label), false, vec![]);
        undirected.initial_edge_properties = vec![(PropertyId::from_raw(91), Value::Int64(901))];
        let mut self_loop = input(loop_vertex, loop_vertex, Some(label), false, vec![]);
        self_loop.initial_edge_properties = vec![(PropertyId::from_raw(92), Value::Int64(902))];
        let edges = vec![directed, undirected, self_loop];

        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&edges)
            .expect("mixed property batch");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        assert_eq!(locations.len(), edges.len());

        for (expected_ordinal, edge, location, property_id, expected) in [
            (0, &edges[0], &locations[0], PropertyId::from_raw(90), 900),
            (1, &edges[1], &locations[1], PropertyId::from_raw(91), 901),
            (2, &edges[2], &locations[2], PropertyId::from_raw(92), 902),
        ] {
            assert_eq!(
                match location {
                    BatchEdgePhysicalLocation::Directed {
                        logical_ordinal, ..
                    }
                    | BatchEdgePhysicalLocation::Undirected {
                        logical_ordinal, ..
                    }
                    | BatchEdgePhysicalLocation::UndirectedSelfLoop {
                        logical_ordinal, ..
                    } => *logical_ordinal,
                },
                expected_ordinal
            );
            let occurrence = location.canonical_occurrence(edge);
            let handle = EdgeHandle::at_slot(
                occurrence.owner_vertex_id,
                occurrence.label_id,
                occurrence.slot_index,
            );
            assert_eq!(
                store.edge_property_at_canonical_handle(handle, property_id),
                Some(Value::Int64(expected))
            );
        }
    }

    #[test]
    fn batch_parallel_edge_uses_existing_equal_neighbor_pair_rank() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4007);
        let property_id = PropertyId::from_raw(93);
        let vertices = make_vertices(&store, 2);
        let source = vertices[0];
        let target = vertices[1];
        let first = store
            .insert_directed_edge(source, target, Some(label))
            .expect("existing parallel edge");
        store.prepare_clean_slab_dir_buckets(source, target, label, 0);

        let mut second = input(source, target, Some(label), true, vec![]);
        second.initial_edge_properties = vec![(property_id, Value::Int64(903))];
        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&[second.clone()])
            .expect("parallel batch edge");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        let location = &locations[0];
        let second_occurrence = location.canonical_occurrence(&second);
        let second_counterpart = store
            .counterpart_edge_occurrence(second_occurrence)
            .expect("second counterpart");
        let first_counterpart = store
            .counterpart_edge_occurrence(first.occurrence(LabeledOrientation::Forward))
            .expect("first counterpart");
        let reverse_location = match location {
            BatchEdgePhysicalLocation::Directed { reverse, .. } => reverse,
            other => panic!("expected directed location, got {other:?}"),
        };

        assert_eq!(second_counterpart.owner_vertex_id, target);
        assert_eq!(
            second_counterpart.slot_index.raw(),
            reverse_location.logical_slot
        );
        assert_ne!(
            second_counterpart.slot_index, first_counterpart.slot_index,
            "parallel edges must resolve to distinct pair ranks"
        );
        let second_handle = EdgeHandle::at_slot(
            second_occurrence.owner_vertex_id,
            second_occurrence.label_id,
            second_occurrence.slot_index,
        );
        assert_eq!(
            store.edge_property_at_canonical_handle(second_handle, property_id),
            Some(Value::Int64(903))
        );
    }

    #[test]
    fn batch_parallel_edges_preserve_ordinals_and_sidecars() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4013);
        let property_id = PropertyId::from_raw(101);
        let vertices = make_vertices(&store, 2);
        let source = vertices[0];
        let target = vertices[1];
        store.prepare_clean_slab_dir_buckets(source, target, label, 0);

        let mut first = input(source, target, Some(label), true, vec![]);
        first.initial_edge_properties = vec![(property_id, Value::Int64(911))];
        let mut second = input(source, target, Some(label), true, vec![]);
        second.initial_edge_properties = vec![(property_id, Value::Int64(912))];
        let edges = vec![first.clone(), second.clone()];
        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&edges)
            .expect("parallel batch edges");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        assert_eq!(locations.len(), 2);

        let first_occurrence = locations[0].canonical_occurrence(&first);
        let second_occurrence = locations[1].canonical_occurrence(&second);
        let first_counterpart = store
            .counterpart_edge_occurrence(first_occurrence)
            .expect("first counterpart");
        let second_counterpart = store
            .counterpart_edge_occurrence(second_occurrence)
            .expect("second counterpart");
        assert_ne!(
            first_counterpart.slot_index, second_counterpart.slot_index,
            "same-batch parallel edges must retain distinct pair ranks"
        );

        for (occurrence, expected) in [
            (first_occurrence, Value::Int64(911)),
            (second_occurrence, Value::Int64(912)),
        ] {
            let handle = EdgeHandle::at_slot(
                occurrence.owner_vertex_id,
                occurrence.label_id,
                occurrence.slot_index,
            );
            assert_eq!(
                store.edge_property_at_canonical_handle(handle, property_id),
                Some(expected)
            );
        }
    }

    #[test]
    fn batch_parallel_undirected_edges_preserve_ordinals_and_sidecars() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4014);
        let property_id = PropertyId::from_raw(102);
        let vertices = make_vertices(&store, 2);
        let low = vertices[0];
        let high = vertices[1];
        store.prepare_clean_slab_undir_buckets(low, high, label, 0);

        let mut first = input(low, high, Some(label), false, vec![]);
        first.initial_edge_properties = vec![(property_id, Value::Int64(913))];
        let mut second = input(high, low, Some(label), false, vec![]);
        second.initial_edge_properties = vec![(property_id, Value::Int64(914))];
        let edges = vec![first.clone(), second.clone()];
        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&edges)
            .expect("parallel undirected batch edges");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        assert_eq!(locations.len(), 2);

        let first_occurrence = locations[0].canonical_occurrence(&first);
        let second_occurrence = locations[1].canonical_occurrence(&second);
        let first_counterpart = store
            .counterpart_edge_occurrence(first_occurrence)
            .expect("first counterpart");
        let second_counterpart = store
            .counterpart_edge_occurrence(second_occurrence)
            .expect("second counterpart");
        assert_eq!(first_occurrence.owner_vertex_id, high);
        assert_eq!(second_occurrence.owner_vertex_id, high);
        assert_eq!(first_counterpart.owner_vertex_id, low);
        assert_eq!(second_counterpart.owner_vertex_id, low);
        assert_ne!(
            first_counterpart.slot_index, second_counterpart.slot_index,
            "same-batch undirected parallel edges must retain distinct pair ranks"
        );

        for (occurrence, expected) in [
            (first_occurrence, Value::Int64(913)),
            (second_occurrence, Value::Int64(914)),
        ] {
            let handle = EdgeHandle::at_slot(
                occurrence.owner_vertex_id,
                occurrence.label_id,
                occurrence.slot_index,
            );
            assert_eq!(
                store.edge_property_at_canonical_handle(handle, property_id),
                Some(expected)
            );
        }
    }

    #[test]
    fn batch_parallel_undirected_edge_uses_owner_alias_pair_rank() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4008);
        let property_id = PropertyId::from_raw(94);
        let vertices = make_vertices(&store, 2);
        let low = vertices[0];
        let high = vertices[1];
        let first = store
            .insert_undirected_edge(low, high, Some(label))
            .expect("existing undirected edge");
        store.prepare_clean_slab_undir_buckets(low, high, label, 0);

        let mut second = input(low, high, Some(label), false, vec![]);
        second.initial_edge_properties = vec![(property_id, Value::Int64(904))];
        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&[second.clone()])
            .expect("parallel undirected batch edge");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        let location = &locations[0];
        let second_occurrence = location.canonical_occurrence(&second);
        let second_counterpart = store
            .counterpart_edge_occurrence(second_occurrence)
            .expect("second undirected counterpart");
        let first_counterpart = store
            .counterpart_edge_occurrence(first.occurrence(LabeledOrientation::Forward))
            .expect("first undirected counterpart");
        let owner_location = match location {
            BatchEdgePhysicalLocation::Undirected { owner, .. } => owner,
            other => panic!("expected undirected location, got {other:?}"),
        };

        assert_eq!(second_occurrence.owner_vertex_id, high);
        assert_eq!(second_counterpart.owner_vertex_id, low);
        assert_eq!(
            second_occurrence.slot_index.raw(),
            owner_location.logical_slot
        );
        assert_ne!(
            second_counterpart.slot_index, first_counterpart.slot_index,
            "parallel undirected edges must resolve to distinct owner/alias pair ranks"
        );
        let second_handle = EdgeHandle::at_slot(
            second_occurrence.owner_vertex_id,
            second_occurrence.label_id,
            second_occurrence.slot_index,
        );
        assert_eq!(
            store.edge_property_at_canonical_handle(second_handle, property_id),
            Some(Value::Int64(904))
        );
    }

    #[test]
    fn batch_parallel_edge_after_delete_uses_live_pair_rank() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4009);
        let property_id = PropertyId::from_raw(95);
        let vertices = make_vertices(&store, 2);
        let source = vertices[0];
        let target = vertices[1];
        let first = store
            .insert_directed_edge(source, target, Some(label))
            .expect("first edge");
        store
            .insert_directed_edge(source, target, Some(label))
            .expect("second edge");
        store
            .delete_edge_by_handle(first)
            .expect("delete first edge");

        let surviving = store
            .directed_out_edges(source)
            .expect("surviving outgoing edge")
            .into_iter()
            .find(|edge| {
                edge.neighbor_vid() == target
                    && edge.label_id == lara_label(label.pack(EdgeDirectedness::Directed)).raw()
            })
            .map(|edge| {
                EdgeHandle::at_slot(
                    source,
                    lara_label(label.pack(EdgeDirectedness::Directed)),
                    edge.edge_slot_index.raw(),
                )
            })
            .expect("live edge after tombstone");
        store.prepare_clean_slab_dir_buckets(source, target, label, 0);

        let mut next = input(source, target, Some(label), true, vec![]);
        next.initial_edge_properties = vec![(property_id, Value::Int64(905))];
        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&[next.clone()])
            .expect("batch edge after delete");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        let next_occurrence = locations[0].canonical_occurrence(&next);
        let next_counterpart = store
            .counterpart_edge_occurrence(next_occurrence)
            .expect("next counterpart");
        let surviving_counterpart = store
            .counterpart_edge_occurrence(surviving.occurrence(LabeledOrientation::Forward))
            .expect("surviving counterpart");

        assert_eq!(next_counterpart.owner_vertex_id, target);
        assert_ne!(
            next_counterpart.slot_index, surviving_counterpart.slot_index,
            "deleted rows must not consume a live parallel pair rank"
        );
        let next_handle = EdgeHandle::at_slot(
            next_occurrence.owner_vertex_id,
            next_occurrence.label_id,
            next_occurrence.slot_index,
        );
        assert_eq!(
            store.edge_property_at_canonical_handle(next_handle, property_id),
            Some(Value::Int64(905))
        );
    }

    #[test]
    fn batch_sidecar_survives_forward_compaction_after_delete() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4010);
        let property_id = PropertyId::from_raw(96);
        let vertices = make_vertices(&store, 2);
        let source = vertices[0];
        let target = vertices[1];
        let first = store
            .insert_directed_edge(source, target, Some(label))
            .expect("first edge");
        store.prepare_clean_slab_dir_buckets(source, target, label, 0);

        let mut second = input(source, target, Some(label), true, vec![]);
        second.initial_edge_properties = vec![(property_id, Value::Int64(906))];
        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&[second.clone()])
            .expect("batch edge");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        let captured = locations[0].canonical_occurrence(&second);
        let captured_handle = EdgeHandle::at_slot(
            captured.owner_vertex_id,
            captured.label_id,
            captured.slot_index,
        );
        store
            .delete_edge_by_handle(first)
            .expect("delete first edge");
        store.with_graph_mut(|graph| {
            graph
                .mark_compact_vertex_edge_span(LabeledOrientation::Forward, source, 0)
                .expect("mark forward compaction");
        });
        store
            .run_maintenance_best_effort(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("run compaction");

        let moved = store
            .directed_out_edges(source)
            .expect("outgoing edges after compaction")
            .into_iter()
            .find(|edge| edge.neighbor_vid() == target)
            .expect("surviving batch edge");
        let moved_handle = EdgeHandle::at_slot(
            source,
            lara_label(label.pack(EdgeDirectedness::Directed)),
            moved.edge_slot_index.raw(),
        );
        assert_eq!(
            store.edge_property_at_canonical_handle(moved_handle, property_id),
            Some(Value::Int64(906))
        );
        assert_eq!(
            store.edge_property_at_canonical_handle(captured_handle, property_id),
            None,
            "the pre-compaction physical handle must not remain authoritative"
        );
    }

    #[test]
    fn batch_sidecar_survives_reverse_compaction_after_delete() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4011);
        let property_id = PropertyId::from_raw(97);
        let vertices = make_vertices(&store, 3);
        let first_source = vertices[0];
        let second_source = vertices[1];
        let target = vertices[2];
        let first = store
            .insert_directed_edge(first_source, target, Some(label))
            .expect("first edge");
        store.prepare_clean_slab_dir_buckets(second_source, target, label, 0);

        let mut second = input(second_source, target, Some(label), true, vec![]);
        second.initial_edge_properties = vec![(property_id, Value::Int64(907))];
        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&[second.clone()])
            .expect("batch edge");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        let captured = locations[0].canonical_occurrence(&second);
        let captured_handle = EdgeHandle::at_slot(
            captured.owner_vertex_id,
            captured.label_id,
            captured.slot_index,
        );
        store
            .delete_edge_by_handle(first)
            .expect("delete first edge");
        store.with_graph_mut(|graph| {
            graph
                .mark_compact_dense_labeled_vertex_maintenance(LabeledOrientation::Reverse, target)
                .expect("mark reverse compaction");
        });
        store
            .run_maintenance_best_effort(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("run compaction");

        assert_eq!(
            store.edge_property_at_canonical_handle(captured_handle, property_id),
            Some(Value::Int64(907))
        );
        let reverse = store
            .find_first_reverse_handle_descending(
                target,
                lara_label(label.pack(EdgeDirectedness::Directed)),
                |edge| edge.neighbor_vid() == second_source,
            )
            .expect("reverse lookup after compaction")
            .expect("surviving reverse edge");
        assert_eq!(
            store.canonical_reverse_in_edge_handle(reverse),
            captured_handle,
            "reverse compaction must keep the batch canonical owner"
        );
    }

    #[test]
    fn batch_sidecar_survives_undirected_alias_compaction_after_delete() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4012);
        let property_id = PropertyId::from_raw(98);
        let vertices = make_vertices(&store, 2);
        let low = vertices[0];
        let high = vertices[1];
        let first = store
            .insert_undirected_edge(low, high, Some(label))
            .expect("first edge");
        store.prepare_clean_slab_undir_buckets(low, high, label, 0);

        let mut second = input(low, high, Some(label), false, vec![]);
        second.initial_edge_properties = vec![(property_id, Value::Int64(908))];
        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&[second.clone()])
            .expect("batch edge");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        let captured = locations[0].canonical_occurrence(&second);
        let captured_handle = EdgeHandle::at_slot(
            captured.owner_vertex_id,
            captured.label_id,
            captured.slot_index,
        );
        store
            .delete_edge_by_handle(first)
            .expect("delete first edge");
        store.with_graph_mut(|graph| {
            graph
                .mark_compact_vertex_edge_span(LabeledOrientation::Forward, low, 0)
                .expect("mark alias compaction");
        });
        store
            .run_maintenance_best_effort(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("run compaction");

        assert_eq!(
            store.edge_property_at_canonical_handle(captured_handle, property_id),
            Some(Value::Int64(908))
        );
        let alias = store
            .find_first_forward_handle_descending(
                low,
                lara_label(label.pack(EdgeDirectedness::Undirected)),
                |edge| edge.neighbor_vid() == high,
            )
            .expect("alias lookup after compaction")
            .expect("surviving alias edge");
        assert_eq!(
            store.canonical_edge_handle(alias),
            captured_handle,
            "alias compaction must keep the batch canonical owner"
        );
    }

    #[test]
    fn duplicate_initial_sidecar_is_rejected_before_batch_write() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4003);
        let vertices = make_vertices(&store, 2);
        store.prepare_clean_slab_dir_buckets(vertices[0], vertices[1], label, 0);
        let mut edge = input(vertices[0], vertices[1], Some(label), true, vec![]);
        edge.initial_edge_properties = vec![
            (PropertyId::from_raw(77), Value::Int64(1)),
            (PropertyId::from_raw(77), Value::Int64(2)),
        ];

        let error = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&[edge])
            .expect_err("reserved/duplicate sidecar must fail closed");
        assert!(matches!(
            error,
            BatchPlacementError::DuplicateInitialPropertyId { .. }
        ));
        assert_eq!(
            count_labeled_dir_edges(
                &store,
                vertices[0],
                storage_label_for(Some(label), true),
                true
            ),
            0
        );
    }

    #[test]
    fn inline_property_cannot_be_repeated_as_initial_sidecar() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4004);
        let inline_property_id = PropertyId::from_raw(78);
        install_test_edge_inline_property(label, inline_property_id);
        install_width(label, 4);
        let vertices = make_vertices(&store, 2);
        store.prepare_clean_slab_dir_buckets(vertices[0], vertices[1], label, 4);
        let mut edge = input(
            vertices[0],
            vertices[1],
            Some(label),
            true,
            vec![1, 2, 3, 4],
        );
        edge.initial_edge_properties = vec![(inline_property_id, Value::Int64(1))];

        let error = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&[edge])
            .expect_err("inline property must not also be stored as a sidecar");
        assert!(matches!(
            error,
            BatchPlacementError::InitialPropertyConflictsWithInline { .. }
        ));
        assert_eq!(
            count_labeled_dir_edges(
                &store,
                vertices[0],
                storage_label_for(Some(label), true),
                true
            ),
            0
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
    fn clean_slab_directed_inline_property_bytes_success() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(1001);
        install_width(label, 8);
        let vertices = make_vertices(&store, 2);
        let source = vertices[0];
        let target = vertices[1];
        let inline_property_bytes = vec![1u8, 2, 3, 4, 5, 6, 7, 8];

        store.prepare_clean_slab_dir_buckets(source, target, label, 8);

        let edges = vec![input(
            source,
            target,
            Some(label),
            true,
            inline_property_bytes.clone(),
        )];
        let result = store
            .try_insert_batch_edges_clean_slab_with_locations(&edges)
            .expect("plan/encode ok");
        let locations = match &result {
            BatchEdgeInsertResult::Committed { locations, .. } => locations,
            other => panic!("expected committed batch, got {other:?}"),
        };
        assert!(matches!(
            locations.as_ref().expect("capture locations").as_slice(),
            [BatchEdgePhysicalLocation::Directed {
                forward: OneOrientationBatchLocation {
                    location: OneOrientationPhysicalLocation::Slab {
                        inline_property_bytes_offset: Some(_),
                        ..
                    },
                    ..
                },
                reverse: OneOrientationBatchLocation {
                    location: OneOrientationPhysicalLocation::Slab {
                        inline_property_bytes_offset: Some(_),
                        ..
                    },
                    ..
                },
                ..
            }]
        ));
        assert_eq!(result.total_edge_slots(), Some(2));
        assert_eq!(result.total_inline_property_bytes_slots(), Some(2));
        assert!(!result.used_expansion());

        let label_raw = storage_label_for(Some(label), true);
        assert_eq!(count_labeled_dir_edges(&store, source, label_raw, true), 1);
        assert_eq!(count_labeled_dir_edges(&store, target, label_raw, false), 1);

        for edge in store.directed_out_edges(source).expect("out") {
            if edge.label_id == label_raw {
                assert_eq!(
                    edge.edge_inline_property_bytes(),
                    inline_property_bytes.as_slice()
                );
            }
        }
        for edge in store.directed_in_edges(target).expect("in") {
            if edge.label_id == label_raw {
                assert_eq!(
                    edge.edge_inline_property_bytes(),
                    inline_property_bytes.as_slice()
                );
            }
        }
    }

    #[test]
    fn clean_slab_undirected_success() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(2001);
        install_width(label, 1);
        let vertices = make_vertices(&store, 2);
        let a = vertices[0];
        let b = vertices[1];
        let inline_property_bytes = vec![7u8];

        store.prepare_clean_slab_undir_buckets(a, b, label, 1);

        let edges = vec![input(
            a,
            b,
            Some(label),
            false,
            inline_property_bytes.clone(),
        )];
        let result = store
            .try_insert_batch_edges_clean_slab_with_locations(&edges)
            .expect("plan/encode ok");
        assert!(matches!(
            &result,
            BatchEdgeInsertResult::Committed {
                locations,
                ..
            } if matches!(locations.as_ref().expect("capture locations").as_slice(), [BatchEdgePhysicalLocation::Undirected { .. }])
        ));
        assert_eq!(result.total_edge_slots(), Some(2));
        assert_eq!(result.total_inline_property_bytes_slots(), Some(2));

        let label_raw = storage_label_for(Some(label), false);
        assert_eq!(
            store
                .undirected_edges(a)
                .expect("undirected")
                .into_iter()
                .filter(|e| e.label_id == label_raw)
                .count(),
            1
        );
        assert_eq!(
            store
                .undirected_edges(b)
                .expect("undirected")
                .into_iter()
                .filter(|e| e.label_id == label_raw)
                .count(),
            1
        );
    }

    #[test]
    fn clean_slab_undirected_same_bucket_uses_one_merged_run() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(2501);
        install_width(label, 0);
        let vertices = make_vertices(&store, 4);
        let one = vertices[1];
        let two = vertices[2];
        let three = vertices[3];

        // The projections for [3 -- 2, 2 -- 1] both land in vertex 2's
        // undirected bucket. They must be reserved and committed as one run.
        store.prepare_clean_slab_undir_buckets(three, two, label, 0);
        store.prepare_clean_slab_undir_buckets(two, one, label, 0);
        let edges = vec![
            input(three, two, Some(label), false, vec![]),
            input(two, one, Some(label), false, vec![]),
        ];

        let result = store
            .try_insert_batch_edges_clean_slab_with_locations(&edges)
            .expect("plan/encode ok");
        assert!(matches!(result, BatchEdgeInsertResult::Committed { .. }));
        let storage_label = storage_label_for(Some(label), false);
        assert_eq!(
            store
                .undirected_edges(two)
                .expect("undirected")
                .into_iter()
                .filter(|edge| edge.label_id == storage_label)
                .count(),
            2
        );
        let neighbors = store
            .undirected_edges(two)
            .expect("undirected")
            .into_iter()
            .filter(|edge| edge.label_id == storage_label)
            .map(|edge| edge.neighbor_vid())
            .collect::<Vec<_>>();
        assert_eq!(neighbors, vec![three, one]);
    }

    #[test]
    fn clean_slab_mixed_logical_shapes_use_one_owner_batch() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(2601);
        install_width(label, 0);
        let vertices = make_vertices(&store, 8);
        let directed_source = vertices[0];
        let directed_target = vertices[1];
        let undirected_low = vertices[2];
        let undirected_high = vertices[3];
        let directed_loop = vertices[4];
        let undirected_loop = vertices[5];

        store.prepare_clean_slab_dir_buckets(directed_source, directed_target, label, 0);
        store.prepare_clean_slab_undir_buckets(undirected_low, undirected_high, label, 0);
        store.prepare_clean_slab_dir_buckets(directed_loop, directed_loop, label, 0);
        store.prepare_clean_slab_undir_buckets(undirected_loop, undirected_loop, label, 0);

        let edges = vec![
            input(directed_source, directed_target, Some(label), true, vec![]),
            input(undirected_low, undirected_high, Some(label), false, vec![]),
            input(directed_loop, directed_loop, Some(label), true, vec![]),
            input(undirected_loop, undirected_loop, Some(label), false, vec![]),
        ];
        let result = store
            .try_insert_batch_edges_clean_slab_with_locations(&edges)
            .expect("mixed plan/encode ok");
        assert!(matches!(result, BatchEdgeInsertResult::Committed { .. }));
        assert_eq!(result.total_edge_slots(), Some(7));

        let directed_label = storage_label_for(Some(label), true);
        let undirected_label = storage_label_for(Some(label), false);
        assert_eq!(
            count_labeled_dir_edges(&store, directed_source, directed_label, true),
            1
        );
        assert_eq!(
            count_labeled_dir_edges(&store, directed_target, directed_label, false),
            1
        );
        assert_eq!(
            count_labeled_dir_edges(&store, directed_loop, directed_label, true),
            1
        );
        assert_eq!(
            count_labeled_dir_edges(&store, directed_loop, directed_label, false),
            1
        );
        assert_eq!(
            store
                .undirected_edges(undirected_low)
                .expect("undirected low")
                .into_iter()
                .filter(|edge| edge.label_id == undirected_label)
                .count(),
            1
        );
        assert_eq!(
            store
                .undirected_edges(undirected_high)
                .expect("undirected high")
                .into_iter()
                .filter(|edge| edge.label_id == undirected_label)
                .count(),
            1
        );
        assert_eq!(
            store
                .undirected_edges(undirected_loop)
                .expect("undirected loop")
                .into_iter()
                .filter(|edge| edge.label_id == undirected_label)
                .count(),
            1
        );
    }

    #[test]
    fn clean_slab_self_loop_success() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(3001);
        install_width(label, 4);
        let vertices = make_vertices(&store, 1);
        let a = vertices[0];
        let inline_property_bytes = vec![9u8, 8, 7, 6];

        store.prepare_clean_slab_undir_buckets(a, a, label, 4);

        let edges = vec![input(
            a,
            a,
            Some(label),
            false,
            inline_property_bytes.clone(),
        )];
        let result = store
            .try_insert_batch_edges_clean_slab_with_locations(&edges)
            .expect("plan/encode ok");
        assert!(matches!(
            &result,
            BatchEdgeInsertResult::Committed {
                locations,
                ..
            } if matches!(locations.as_ref().expect("capture locations").as_slice(), [BatchEdgePhysicalLocation::UndirectedSelfLoop { .. }])
        ));
        assert_eq!(result.total_edge_slots(), Some(1));
        assert_eq!(result.total_inline_property_bytes_slots(), Some(1));

        let label_raw = storage_label_for(Some(label), false);
        assert_eq!(
            store
                .undirected_edges(a)
                .expect("undirected")
                .into_iter()
                .filter(|e| e.label_id == label_raw)
                .count(),
            1
        );
    }

    #[test]
    fn clean_slab_multiple_runs_success() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(3101);
        install_width(label, 0);
        let vertices = make_vertices(&store, 4);
        let s0 = vertices[0];
        let s1 = vertices[1];
        let t0 = vertices[2];
        let t1 = vertices[3];

        store.prepare_clean_slab_dir_buckets(s0, t0, label, 0);
        store.prepare_clean_slab_dir_buckets(s1, t1, label, 0);

        let edges = vec![
            input(s0, t0, Some(label), true, vec![]),
            input(s1, t1, Some(label), true, vec![]),
        ];
        let result = store
            .try_insert_batch_edges_clean_slab(&edges)
            .expect("plan/encode ok");
        assert!(matches!(result, BatchEdgeInsertResult::Committed { .. }));
        assert!(matches!(
            &result,
            BatchEdgeInsertResult::Committed {
                locations: None,
                ..
            }
        ));
        assert_eq!(result.total_edge_slots(), Some(4));
        assert_eq!(result.total_inline_property_bytes_slots(), Some(0));
        assert!(!result.used_expansion());

        let label_raw = storage_label_for(Some(label), true);
        assert_eq!(count_labeled_dir_edges(&store, s0, label_raw, true), 1);
        assert_eq!(count_labeled_dir_edges(&store, s1, label_raw, true), 1);
        assert_eq!(count_labeled_dir_edges(&store, t0, label_raw, false), 1);
        assert_eq!(count_labeled_dir_edges(&store, t1, label_raw, false), 1);
    }

    #[test]
    fn clean_slab_preserves_order_across_both_directed_orientations_through_graph_facade() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(3151);
        let filler_labels = (0..7)
            .map(|index| EdgeLabelId::from_raw(3151 + index))
            .collect::<Vec<_>>();
        for filler_label in &filler_labels {
            install_width(*filler_label, 0);
        }
        let vertices = make_vertices(&store, 66);
        let source = vertices[0];
        let target = vertices[32];
        let later_source = vertices[64];
        let later_target = vertices[65];

        store.prepare_clean_slab_dir_buckets(source, target, label, 0);
        store.prepare_clean_slab_dir_buckets(source, vertices[33], label, 0);
        store.prepare_clean_slab_dir_buckets(later_source, later_target, label, 0);

        // Fill shared leaf logs through ordinary GraphStore writes so the
        // facade test exercises slab and overflow destinations together.
        for (label_index, filler_label) in filler_labels.iter().enumerate() {
            let end = if label_index == 6 { 51 } else { 64 };
            for target_raw in 34..end {
                store
                    .insert_directed_edge(source, VertexId::from(target_raw), Some(*filler_label))
                    .expect("fill forward and reverse slab windows");
            }
        }
        for target_raw in 1..=31 {
            store
                .insert_directed_edge(source, VertexId::from(target_raw), Some(label))
                .expect("fill the forward batch bucket");
        }
        for target_raw in 64..=65 {
            store
                .insert_directed_edge(source, VertexId::from(target_raw), Some(label))
                .expect("fill the forward batch bucket");
        }
        for source_raw in 1..=65 {
            store
                .insert_directed_edge(VertexId::from(source_raw), target, Some(label))
                .expect("fill the reverse batch bucket");
            store
                .insert_directed_edge(VertexId::from(source_raw), vertices[33], Some(label))
                .expect("fill the second reverse batch bucket");
        }
        store
            .insert_directed_edge(later_source, later_target, Some(label))
            .expect("pin a later leaf");

        let result = store
            .try_insert_batch_edges_clean_slab_with_locations(&[
                input(source, target, Some(label), true, vec![]),
                input(source, vertices[33], Some(label), true, vec![]),
            ])
            .expect("plan/encode ok");
        assert!(matches!(result, BatchEdgeInsertResult::Committed { .. }));

        let label_raw = storage_label_for(Some(label), true);
        let forward_targets = store
            .directed_out_edges(source)
            .expect("forward edges")
            .into_iter()
            .filter(|edge| edge.label_id == label_raw)
            .map(|edge| edge.neighbor_vid())
            .collect::<Vec<_>>();
        let reverse_sources = store
            .directed_in_edges(target)
            .expect("reverse edges")
            .into_iter()
            .filter(|edge| edge.label_id == label_raw)
            .map(|edge| edge.neighbor_vid())
            .collect::<Vec<_>>();
        let mut expected_forward_targets = (34..64).map(VertexId::from).collect::<Vec<_>>();
        expected_forward_targets.extend((1..=31).map(VertexId::from));
        expected_forward_targets.extend([VertexId::from(64), VertexId::from(65)]);
        expected_forward_targets.extend([target, vertices[33]]);
        assert_eq!(forward_targets.len(), 65);
        assert_eq!(forward_targets, expected_forward_targets);
        assert_eq!(reverse_sources.len(), 66);
        assert_eq!(reverse_sources.last(), Some(&source));
        assert_eq!(
            store
                .directed_in_edges(vertices[33])
                .expect("second reverse edge")
                .into_iter()
                .filter(|edge| edge.label_id == label_raw)
                .map(|edge| edge.neighbor_vid())
                .collect::<Vec<_>>(),
            (1..=65)
                .map(VertexId::from)
                .chain([source])
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn new_bucket_uses_lara_batch_reservation() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4001);
        install_width(label, 0);
        let vertices = make_vertices(&store, 2);
        let source = vertices[0];
        let target = vertices[1];

        let edges = vec![input(source, target, Some(label), true, vec![])];
        let result = store
            .try_insert_batch_edges_clean_slab(&edges)
            .expect("plan/encode ok");
        assert!(
            matches!(result, BatchEdgeInsertResult::Committed { .. }),
            "expected committed new-bucket batch, got {result:?}"
        );
        let label_raw = storage_label_for(Some(label), true);
        assert_eq!(count_labeled_dir_edges(&store, source, label_raw, true), 1);
        assert_eq!(count_labeled_dir_edges(&store, target, label_raw, false), 1);
    }

    #[test]
    fn asymmetric_new_bucket_uses_lara_batch_reservation() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(4002);
        install_width(label, 0);
        let vertices = make_vertices(&store, 2);
        let source = vertices[0];
        let target = vertices[1];
        let storage_label = lara_label(edge_storage_label(Some(label), false));

        store.with_graph_mut(|graph| {
            graph
                .ensure_forward_edge_inline_property_width(source, storage_label, 0)
                .expect("forward bucket");
        });

        let result = store
            .try_insert_batch_edges_clean_slab(&[input(source, target, Some(label), true, vec![])])
            .expect("plan/encode ok");
        assert!(
            matches!(result, BatchEdgeInsertResult::Committed { .. }),
            "expected committed asymmetric new-bucket batch, got {result:?}"
        );
        let label_raw = storage_label_for(Some(label), true);
        assert_eq!(count_labeled_dir_edges(&store, source, label_raw, true), 1);
        assert_eq!(count_labeled_dir_edges(&store, target, label_raw, false), 1);
    }

    #[test]
    fn batch_reservation_prepares_missing_reverse_buckets() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(5001);
        install_width(label, 0);
        let vertices = make_vertices(&store, 3);
        let source = vertices[0];
        let target_with_bucket = vertices[1];
        let target_without_bucket = vertices[2];

        // The reverse bucket for target_without_bucket is intentionally absent; LARA
        // prepares it as part of the batch reservation.
        store.prepare_clean_slab_dir_buckets(source, target_with_bucket, label, 0);

        let label_raw = storage_label_for(Some(label), true);
        let out_before = count_labeled_dir_edges(&store, source, label_raw, true);
        let in_before = count_labeled_dir_edges(&store, target_without_bucket, label_raw, false);

        let edges = vec![
            input(source, target_with_bucket, Some(label), true, vec![]),
            input(source, target_without_bucket, Some(label), true, vec![]),
        ];
        let result = store
            .try_insert_batch_edges_clean_slab(&edges)
            .expect("plan/encode ok");
        assert!(
            matches!(result, BatchEdgeInsertResult::Committed { .. }),
            "expected committed batch with prepared bucket, got {result:?}"
        );

        assert_eq!(
            count_labeled_dir_edges(&store, source, label_raw, true),
            out_before + 2,
            "both forward edges must be committed"
        );
        assert_eq!(
            count_labeled_dir_edges(&store, target_without_bucket, label_raw, false,),
            in_before + 1,
            "the missing reverse bucket must be prepared and committed"
        );
    }

    #[test]
    fn batch_reservation_prepares_missing_reverse_bucket_with_inline_bytes() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(5002);
        install_width(label, 8);
        let vertices = make_vertices(&store, 3);
        let source = vertices[0];
        let prepared_target = vertices[1];
        let target = vertices[2];
        store.prepare_clean_slab_dir_buckets(source, prepared_target, label, 8);
        let result = store
            .try_insert_batch_edges_clean_slab(&[input(
                source,
                target,
                Some(label),
                true,
                vec![1, 2, 3, 4, 5, 6, 7, 8],
            )])
            .expect("plan/encode ok");
        assert!(matches!(result, BatchEdgeInsertResult::Committed { .. }));
    }

    /// Reserve both orientations of a inline-property-bearing directed edge (so both
    /// forward and reverse inline property bytes allocations complete), then roll back both
    /// reservations and verify every logical allocator boundary is restored.
    ///
    /// This exercises the cross-orientation path where *multiple* orientations
    /// have already mutated allocator state before the orchestration decides to
    /// abort.  The stable-memory page slack is retained as reusable free-list
    /// space; only the logical capacity, free-list accounting, and occupied
    /// tails are restored.
    #[test]
    fn multi_orientation_inline_property_bytes_reserve_then_rollback_restores_allocator_state() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(5003);
        install_width(label, 8);
        let inline_property_bytes = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let vertices = make_vertices(&store, 3);
        let source = vertices[0];
        let target = vertices[1];
        // A third vertex we will not use for any clean-slab batch; it just keeps
        // the graph from being trivially symmetric.
        let _other = vertices[2];

        // Prepare buckets so a directed edge source -> target has both a
        // forward bucket at source and a reverse bucket at target.
        store.prepare_clean_slab_dir_buckets(source, target, label, 8);
        // Also prepare the opposite direction so we can reserve both
        // orientations independently without either failing because the bucket
        // is missing.  This does not change the test edge, but it gives both
        // stores a non-trivial allocator state to roll back.
        store.prepare_clean_slab_dir_buckets(target, source, label, 8);

        let before = allocator_snapshot(&store);

        // Build the physical intents for one directed edge.
        let intents = store
            .expand_batch_edge_intents(&[input(
                source,
                target,
                Some(label),
                true,
                inline_property_bytes.clone(),
            )])
            .expect("intents ok");
        let requests = store
            .build_one_orientation_batch_plans(&intents, encode_intent_edge)
            .expect("plans ok");

        assert_eq!(
            requests.len(),
            2,
            "directed edge must produce two orientations"
        );

        // Reserve both orientations.  Each reserve grows its own edge logical
        // capacity and allocates inline property bytes at its occupied tail.
        let reservations = store
            .with_graph_mut(|graph| {
                graph.reserve_batch_orientations(BidirectionalBatchPlan::Directed {
                    forward: requests[0].plan.clone(),
                    reverse: requests[1].plan.clone(),
                })
            })
            .expect("reserve ok for both orientations");

        let after_reserve = allocator_snapshot(&store);

        // Reserve advanced at least one inline property bytes occupied tail (the edge
        // logical capacity may already be large enough not to grow for a single
        // edge).
        assert!(
            after_reserve
                .forward_inline_property_bytes
                .slab_occupied_tail
                > before.forward_inline_property_bytes.slab_occupied_tail
                || after_reserve
                    .reverse_inline_property_bytes
                    .slab_occupied_tail
                    > before.reverse_inline_property_bytes.slab_occupied_tail,
            "reserve must advance at least one inline property bytes occupied tail"
        );
        assert!(
            after_reserve
                .forward_inline_property_bytes
                .slab_occupied_tail
                > before.forward_inline_property_bytes.slab_occupied_tail
                || after_reserve
                    .reverse_inline_property_bytes
                    .slab_occupied_tail
                    > before.reverse_inline_property_bytes.slab_occupied_tail,
            "reserve must advance at least one inline property bytes occupied tail"
        );

        // Roll back every reservation without committing.  The orchestration
        // helper consumes the tokens, so this is the exact cross-orientation
        // failure path used by `try_insert_batch_edges_clean_slab`.
        store.rollback_one_orientation_reservations(reservations);

        let after_rollback = allocator_snapshot(&store);

        // Logical edge capacity and inline_property_bytes tails are restored on both sides.
        assert_eq!(
            after_rollback.forward_edge_capacity, before.forward_edge_capacity,
            "forward edge logical capacity must be restored"
        );
        assert_eq!(
            after_rollback.reverse_edge_capacity, before.reverse_edge_capacity,
            "reverse edge logical capacity must be restored"
        );
        assert_eq!(
            after_rollback
                .forward_inline_property_bytes
                .slab_occupied_tail,
            before.forward_inline_property_bytes.slab_occupied_tail,
            "forward inline_property_bytes occupied tail must be restored"
        );
        assert_eq!(
            after_rollback
                .reverse_inline_property_bytes
                .slab_occupied_tail,
            before.reverse_inline_property_bytes.slab_occupied_tail,
            "reverse inline_property_bytes occupied tail must be restored"
        );

        // Edge free-list shape is unchanged (no edge spans were retired).
        assert_eq!(
            after_rollback.forward_edge_free, before.forward_edge_free,
            "forward edge free-list must be unchanged"
        );
        assert_eq!(
            after_rollback.reverse_edge_free, before.reverse_edge_free,
            "reverse edge free-list must be unchanged"
        );

        // The allocated inline_property_bytes bytes from both orientations became exactly one
        // free span per orientation.  Stable-memory backing capacity is not shrunk.
        let expected_inline_property_bytes = u64::try_from(inline_property_bytes.len()).unwrap();
        assert_eq!(
            after_rollback.forward_inline_property_bytes.free_bytes
                - before.forward_inline_property_bytes.free_bytes,
            expected_inline_property_bytes,
            "forward inline_property_bytes free bytes must increase by exactly the reserved run"
        );
        assert_eq!(
            after_rollback.reverse_inline_property_bytes.free_bytes
                - before.reverse_inline_property_bytes.free_bytes,
            expected_inline_property_bytes,
            "reverse inline_property_bytes free bytes must increase by exactly the reserved run"
        );
        assert_eq!(
            after_rollback.forward_inline_property_bytes.free_span_count
                - before.forward_inline_property_bytes.free_span_count,
            1,
            "forward inline property free-list must gain one retired span"
        );
        assert_eq!(
            after_rollback.reverse_inline_property_bytes.free_span_count
                - before.reverse_inline_property_bytes.free_span_count,
            1,
            "reverse inline property free-list must gain one retired span"
        );
        assert!(
            after_rollback
                .forward_inline_property_bytes
                .largest_free_span
                >= expected_inline_property_bytes,
            "forward largest free span must cover the retired run"
        );
        assert!(
            after_rollback
                .reverse_inline_property_bytes
                .largest_free_span
                >= expected_inline_property_bytes,
            "reverse largest free span must cover the retired run"
        );
        assert!(
            after_rollback.forward_inline_property_bytes.byte_capacity
                >= before.forward_inline_property_bytes.byte_capacity,
            "forward stable-memory inline_property_bytes capacity must not shrink"
        );
        assert!(
            after_rollback.reverse_inline_property_bytes.byte_capacity
                >= before.reverse_inline_property_bytes.byte_capacity,
            "reverse stable-memory inline_property_bytes capacity must not shrink"
        );
    }

    #[test]
    fn overflow_log_append_success() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(6001);
        install_width(label, 0);
        let vertices = make_vertices(&store, 2);
        let source = vertices[0];
        let target = vertices[1];

        store.prepare_clean_slab_dir_buckets(source, target, label, 0);

        let quota =
            store.with_graph_mut(|g| g.forward().edges().header().initial_vertex_edge_slots);
        for _ in 0..quota {
            store
                .insert_directed_edge(source, target, Some(label))
                .expect("scalar fill");
        }

        let label_raw = storage_label_for(Some(label), true);
        let out_before = count_labeled_dir_edges(&store, source, label_raw, true);

        let edges = vec![input(source, target, Some(label), true, vec![])];
        let result = store
            .try_insert_batch_edges_clean_slab_with_locations(&edges)
            .expect("plan/encode ok");
        assert!(
            matches!(
                &result,
                BatchEdgeInsertResult::Committed {
                    locations,
                    ..
                } if locations.as_ref().expect("capture locations").iter().all(|location| match location {
                    BatchEdgePhysicalLocation::Directed { forward, reverse, .. } => {
                        matches!(
                            forward.location,
                            OneOrientationPhysicalLocation::OverflowLog { .. }
                        ) && matches!(
                            reverse.location,
                            OneOrientationPhysicalLocation::OverflowLog { .. }
                        )
                    }
                    _ => false,
                })
            ),
            "expected committed overflow-log batch, got {result:?}"
        );
        assert_eq!(result.total_edge_slots(), Some(2));
        assert_eq!(
            count_labeled_dir_edges(&store, source, label_raw, true),
            out_before + 1,
            "overflow-log batch edge must be visible in read-back"
        );
    }

    #[test]
    fn overflow_log_batch_sidecar_uses_captured_logical_slot() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(6002);
        let property_id = PropertyId::from_raw(99);
        install_width(label, 0);
        let vertices = make_vertices(&store, 2);
        let source = vertices[0];
        let target = vertices[1];

        store.prepare_clean_slab_dir_buckets(source, target, label, 0);
        let quota =
            store.with_graph_mut(|g| g.forward().edges().header().initial_vertex_edge_slots);
        for _ in 0..quota {
            store
                .insert_directed_edge(source, target, Some(label))
                .expect("scalar fill");
        }

        let mut edge = input(source, target, Some(label), true, vec![]);
        edge.initial_edge_properties = vec![(property_id, Value::Int64(909))];
        let result = store
            .try_insert_batch_edges_clean_slab_with_initial_properties(&[edge.clone()])
            .expect("overflow-log sidecar batch");
        let locations = match result {
            BatchEdgeInsertResult::Committed {
                locations: Some(locations),
                ..
            } => locations,
            other => panic!("expected captured commit, got {other:?}"),
        };
        let location = &locations[0];
        let forward_location = match location {
            BatchEdgePhysicalLocation::Directed { forward, .. } => forward,
            other => panic!("expected directed location, got {other:?}"),
        };
        assert!(matches!(
            forward_location.location,
            OneOrientationPhysicalLocation::OverflowLog { .. }
        ));
        let occurrence = location.canonical_occurrence(&edge);
        let handle = EdgeHandle::at_slot(
            occurrence.owner_vertex_id,
            occurrence.label_id,
            occurrence.slot_index,
        );
        assert_eq!(
            store.edge_property_at_canonical_handle(handle, property_id),
            Some(Value::Int64(909))
        );
        assert_eq!(
            occurrence.slot_index.raw(),
            forward_location.logical_slot,
            "overflow-log sidecar must use the captured bucket logical slot"
        );
    }

    #[test]
    fn empty_batch_is_unsupported() {
        let store = fresh_store();
        let result = store.try_insert_batch_edges_clean_slab(&[]).expect("ok");
        assert!(matches!(result, BatchEdgeInsertResult::Unsupported { .. }));
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
