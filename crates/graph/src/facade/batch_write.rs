//! Internal GraphStore clean-slab batch orchestration for ADR 0045.
//!
//! This module consumes physical half-edge intents produced by
//! [`super::batch_placement`] and attempts to commit them through the LARA
//! one-orientation batch primitive.  Unsupported geometry is returned to the
//! caller before any canonical write so the existing scalar path can handle it.
//! No LARA placement policy leaks outside this module.

use gleaph_graph_kernel::entry::{Edge, EdgeLabelId};
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

use super::GraphStore;
use super::batch_placement::{
    BatchEdgeInput, BatchEdgeIntent, BatchEdgeIntentRole, BatchPlacementError, BatchPlacementKey,
};
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

    fn try_insert_batch_edges_clean_slab_with_mode(
        &self,
        edges: &[super::batch_placement::BatchEdgeInput],
        location_mode: BatchLocationMode,
    ) -> Result<BatchEdgeInsertResult, BatchPlacementError> {
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

        let intents = self.expand_batch_edge_intents(edges)?;
        let owner_plan = if homogeneous {
            let requests = self.build_one_orientation_batch_plans(&intents, encode_intent_edge)?;
            let undirected_pairs = (!directed && !undirected_self_loop)
                .then(|| build_undirected_batch_pairs(&intents))
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
                physical: self
                    .build_merged_orientation_batch_plans(&intents, encode_intent_edge)?,
                pairs: build_mixed_batch_pairs(edges, &intents)?,
            }
        };

        // Reserve every orientation first. If any orientation is unsupported, roll
        // back every previously successful reservation before returning unsupported.
        // No canonical write occurs on this path.
        let reservations =
            match self.with_graph_mut(|graph| graph.reserve_batch_orientations(owner_plan)) {
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
        let results = self
            .with_graph_mut(|graph| graph.commit_batch_orientations(reservations, location_mode));

        let edge_slots_written = results
            .iter()
            .map(|(_, result)| u64::from(result.edge_slots_written))
            .sum();
        let inline_property_bytes_slots_written = results
            .iter()
            .map(|(_, result)| u64::from(result.inline_property_bytes_slots_written))
            .sum();
        let locations = location_mode.captures().then(|| {
            join_physical_locations(edges, &intents, &results)
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

        // Ensure each run is sorted by logical ordinal so edge/inline-property-bytes alignment is
        // deterministic. The LARA reserve step also checks this, but doing it here
        // keeps the GraphStore contract closer to the source of physical intents.
        for edges in runs_by_role.values_mut() {
            edges.sort_by_key(|e| e.logical_ordinal);
        }

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
        for edges in runs.values_mut() {
            edges.sort_by_key(|edge| edge.logical_ordinal);
        }
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
    fn unsupported_new_bucket_falls_back_to_scalar() {
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
            matches!(result, BatchEdgeInsertResult::Unsupported { .. }),
            "expected unsupported for new bucket, got {result:?}"
        );

        store
            .insert_directed_edge(source, target, Some(label))
            .expect("scalar fallback");
        let label_raw = storage_label_for(Some(label), true);
        assert_eq!(count_labeled_dir_edges(&store, source, label_raw, true), 1);
        assert_eq!(count_labeled_dir_edges(&store, target, label_raw, false), 1);
    }

    #[test]
    fn reserve_failure_leaves_canonical_state_unchanged() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(5001);
        install_width(label, 0);
        let vertices = make_vertices(&store, 3);
        let source = vertices[0];
        let target_with_bucket = vertices[1];
        let target_without_bucket = vertices[2];

        // Forward bucket at source and reverse bucket at target_with_bucket only.
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
            matches!(result, BatchEdgeInsertResult::Unsupported { .. }),
            "expected unsupported after partial reserve, got {result:?}"
        );

        assert_eq!(
            count_labeled_dir_edges(&store, source, label_raw, true),
            out_before,
            "forward canonical state must not be partially published"
        );
        assert_eq!(
            count_labeled_dir_edges(&store, target_without_bucket, label_raw, false,),
            in_before,
            "reverse canonical state must remain absent"
        );
    }

    #[test]
    fn owner_batch_second_orientation_failure_restores_allocator_state() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(5002);
        install_width(label, 8);
        let vertices = make_vertices(&store, 3);
        let source = vertices[0];
        let prepared_target = vertices[1];
        let target = vertices[2];
        store.prepare_clean_slab_dir_buckets(source, prepared_target, label, 8);
        let before = allocator_snapshot(&store);

        let result = store
            .try_insert_batch_edges_clean_slab(&[input(
                source,
                target,
                Some(label),
                true,
                vec![1, 2, 3, 4, 5, 6, 7, 8],
            )])
            .expect("plan/encode ok");
        assert!(matches!(result, BatchEdgeInsertResult::Unsupported { .. }));
        let after = allocator_snapshot(&store);
        assert_eq!(after.forward_edge_capacity, before.forward_edge_capacity);
        assert_eq!(after.reverse_edge_capacity, before.reverse_edge_capacity);
        assert_eq!(
            after.forward_inline_property_bytes.slab_occupied_tail,
            before.forward_inline_property_bytes.slab_occupied_tail
        );
        assert_eq!(
            after.reverse_inline_property_bytes.slab_occupied_tail,
            before.reverse_inline_property_bytes.slab_occupied_tail
        );
        assert!(
            after.forward_inline_property_bytes.free_bytes
                >= before.forward_inline_property_bytes.free_bytes
        );
        assert_eq!(
            after.reverse_inline_property_bytes,
            before.reverse_inline_property_bytes
        );
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
    fn reserve_failure_restores_allocator_state() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(5002);
        install_width(label, 8);
        let inline_property_bytes = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let vertices = make_vertices(&store, 3);
        let source = vertices[0];
        let target_with_bucket = vertices[1];
        let target_without_bucket = vertices[2];

        // Prepare a forward bucket at source (and an unused reverse bucket at
        // target_with_bucket).  The edge below targets target_without_bucket, whose
        // reverse bucket does not exist, so reverse reserve fails after the forward
        // inline_property_bytes allocation has already happened.
        store.prepare_clean_slab_dir_buckets(source, target_with_bucket, label, 8);

        let before = allocator_snapshot(&store);

        let edges = vec![input(
            source,
            target_without_bucket,
            Some(label),
            true,
            inline_property_bytes.clone(),
        )];
        let result = store
            .try_insert_batch_edges_clean_slab(&edges)
            .expect("plan/encode ok");
        assert!(
            matches!(result, BatchEdgeInsertResult::Unsupported { .. }),
            "expected unsupported after partial reserve, got {result:?}"
        );

        let after = allocator_snapshot(&store);

        // Logical edge capacity is restored for both orientations.
        assert_eq!(
            after.forward_edge_capacity, before.forward_edge_capacity,
            "forward edge capacity must be restored"
        );
        assert_eq!(
            after.reverse_edge_capacity, before.reverse_edge_capacity,
            "reverse edge capacity must be restored"
        );

        // Edge free-list accounting is unchanged; no edge free spans were created.
        assert_eq!(
            after.forward_edge_free, before.forward_edge_free,
            "forward edge free-list accounting must be unchanged"
        );
        assert_eq!(
            after.reverse_edge_free, before.reverse_edge_free,
            "reverse edge free-list accounting must be unchanged"
        );

        // InlinePropertyBytes occupied tail is restored for both orientations.
        assert_eq!(
            after.forward_inline_property_bytes.slab_occupied_tail,
            before.forward_inline_property_bytes.slab_occupied_tail,
            "forward inline_property_bytes occupied tail must be restored"
        );
        assert_eq!(
            after.reverse_inline_property_bytes.slab_occupied_tail,
            before.reverse_inline_property_bytes.slab_occupied_tail,
            "reverse inline_property_bytes occupied tail must be restored"
        );

        // Reverse inline_property_bytes allocator state is untouched.
        assert_eq!(
            after.reverse_inline_property_bytes,
            before.reverse_inline_property_bytes
        );

        // The forward inline_property_bytes bytes that were allocated before the failure are
        // retired to the free-list as reusable slack. The stable-memory backing
        // capacity is not shrunk.
        let expected_forward_inline_property_bytes =
            u64::try_from(inline_property_bytes.len()).unwrap();
        assert_eq!(
            after.forward_inline_property_bytes.free_bytes
                - before.forward_inline_property_bytes.free_bytes,
            expected_forward_inline_property_bytes,
            "forward inline_property_bytes free bytes must increase by the allocated run length"
        );
        assert_eq!(
            after.forward_inline_property_bytes.free_span_count
                - before.forward_inline_property_bytes.free_span_count,
            1,
            "forward inline property free-list must gain exactly one retired span"
        );
        assert!(
            after.forward_inline_property_bytes.largest_free_span
                >= expected_forward_inline_property_bytes,
            "largest forward inline_property_bytes free span must cover the retired run"
        );
        assert!(
            after.forward_inline_property_bytes.byte_capacity
                >= before.forward_inline_property_bytes.byte_capacity,
            "stable-memory inline_property_bytes capacity must not shrink on rollback"
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
