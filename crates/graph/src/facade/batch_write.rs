//! Internal GraphStore clean-slab batch orchestration for ADR 0045.
//!
//! This module consumes physical half-edge intents produced by
//! [`super::batch_placement`] and attempts to commit them through the LARA
//! one-orientation batch primitive.  Unsupported geometry is returned to the
//! caller before any canonical write so the existing scalar path can handle it.
//! No LARA placement policy leaks outside this module.

use gleaph_graph_kernel::entry::{Edge, EdgeLabelId};
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::batch_write::{
    BatchReservation, OneOrientationBatchEdge, OneOrientationBatchError, OneOrientationBatchPlan,
    OneOrientationBatchResult, OneOrientationBucketRun, OneOrientationPhysicalLocation,
};
use ic_stable_lara::{CsrEdge, labeled::LabeledOrientation};
use rapidhash::{HashMapExt, RapidHashMap};

use super::GraphStore;
use super::batch_placement::{
    BatchEdgeInput, BatchEdgeIntent, BatchEdgeIntentRole, BatchPlacementError, BatchPlacementKey,
};
use super::store::helpers::{
    build_edge_to, build_edge_to_with_inline_value_bytes, edge_storage_label, lara_label,
};

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
        /// Aggregate payload slab slots written across all orientations.
        payload_slots_written: u64,
        /// Paired physical locations keyed by the logical input ordinal.
        locations: Vec<BatchEdgePhysicalLocation>,
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
        forward: OneOrientationPhysicalLocation,
        reverse: OneOrientationPhysicalLocation,
    },
    Undirected {
        logical_ordinal: u32,
        owner: OneOrientationPhysicalLocation,
        alias: OneOrientationPhysicalLocation,
    },
    UndirectedSelfLoop {
        logical_ordinal: u32,
        location: OneOrientationPhysicalLocation,
    },
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

    /// Total payload slab slots written across all committed orientations.
    pub(crate) fn total_payload_slots(&self) -> Option<u64> {
        match self {
            Self::Committed {
                payload_slots_written,
                ..
            } => Some(*payload_slots_written),
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
            g.ensure_directed_edge_inline_value_width(src, dst, storage, width)
                .expect("ensure directed buckets");
        });
    }

    /// Pre-create empty undirected buckets for `{a,b}` with the given inline value
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
            g.ensure_undirected_edge_inline_value_width(a, b, storage, width)
                .expect("ensure undirected buckets");
        });
    }

    /// Attempt to insert a bounded unordered batch of logical edges through the
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
    /// the payload occupied tail, and retires any allocated payload bytes to the
    /// free-list as reusable slack. The underlying stable-memory pages are not
    /// shrunk. This leaves no leaked capacity or tail before the caller falls
    /// back to the existing scalar path.
    pub(crate) fn try_insert_batch_edges_clean_slab(
        &self,
        edges: &[super::batch_placement::BatchEdgeInput],
    ) -> Result<BatchEdgeInsertResult, BatchPlacementError> {
        if edges.is_empty() {
            return Ok(BatchEdgeInsertResult::Unsupported {
                reason: "empty batch is not admitted to the clean-slab path".into(),
            });
        }

        let intents = self.expand_batch_edge_intents(edges)?;
        let requests = self.build_one_orientation_batch_plans(&intents, encode_intent_edge)?;

        // Reserve every orientation first. If any orientation is unsupported, roll
        // back every previously successful reservation before returning unsupported.
        // No canonical write occurs on this path.
        let mut reservations: Vec<(LabeledOrientation, BatchReservation<Edge>)> =
            Vec::with_capacity(requests.len());
        for req in requests {
            match self.reserve_one_orientation_plan(&req.plan, req.orientation) {
                Ok(reservation) => reservations.push((req.orientation, reservation)),
                Err(err) => {
                    self.rollback_one_orientation_reservations(reservations);
                    return Ok(BatchEdgeInsertResult::Unsupported {
                        reason: format!("{err}"),
                    });
                }
            }
        }

        // All reservations succeeded: commit each orientation.
        let used_expansion = reservations
            .iter()
            .any(|(_, reservation)| reservation.uses_expansion());
        let mut results = Vec::with_capacity(reservations.len());
        for (orientation, reservation) in reservations {
            let result = self.with_graph_mut(|graph| {
                let labeled = match orientation {
                    LabeledOrientation::Forward => graph.forward(),
                    LabeledOrientation::Reverse => graph.reverse(),
                };
                reservation.commit(labeled)
            });
            results.push((orientation, result));
        }

        let edge_slots_written = results
            .iter()
            .map(|(_, result)| u64::from(result.edge_slots_written))
            .sum();
        let payload_slots_written = results
            .iter()
            .map(|(_, result)| u64::from(result.payload_slots_written))
            .sum();
        let locations = join_physical_locations(edges, &intents, &results)
            .expect("committed batch must publish one complete location per intent");

        Ok(BatchEdgeInsertResult::Committed {
            edge_slots_written,
            payload_slots_written,
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
        let mut forward_runs: RapidHashMap<BatchPlacementKey, Vec<OneOrientationBatchEdge<E>>> =
            RapidHashMap::default();
        let mut reverse_runs: RapidHashMap<BatchPlacementKey, Vec<OneOrientationBatchEdge<E>>> =
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
                inline_value_width: intent.inline_value_width,
            };
            let edge = encode_edge(intent)?;
            let entry = OneOrientationBatchEdge {
                logical_ordinal: intent.logical_ordinal,
                owner_vertex_id: intent.owner_vertex_id,
                neighbor_vertex_id: intent.neighbor_vertex_id,
                label_id: intent.storage_label,
                edge,
            };
            match intent.orientation {
                LabeledOrientation::Forward => forward_runs.entry(key).or_default().push(entry),
                LabeledOrientation::Reverse => reverse_runs.entry(key).or_default().push(entry),
            }
        }

        // Ensure each run is sorted by logical ordinal so edge/payload alignment is
        // deterministic. The LARA reserve step also checks this, but doing it here
        // keeps the GraphStore contract closer to the source of physical intents.
        for runs in [&mut forward_runs, &mut reverse_runs] {
            for edges in runs.values_mut() {
                edges.sort_by_key(|e| e.logical_ordinal);
            }
        }

        let mut requests = Vec::with_capacity(2);
        if !forward_runs.is_empty() {
            requests.push(OneOrientationBatchWriteRequest {
                orientation: LabeledOrientation::Forward,
                plan: OneOrientationBatchPlan {
                    runs: runs_from_map(forward_runs),
                },
            });
        }
        if !reverse_runs.is_empty() {
            requests.push(OneOrientationBatchWriteRequest {
                orientation: LabeledOrientation::Reverse,
                plan: OneOrientationBatchPlan {
                    runs: runs_from_map(reverse_runs),
                },
            });
        }

        Ok(requests)
    }

    fn reserve_one_orientation_plan(
        &self,
        plan: &OneOrientationBatchPlan<Edge>,
        orientation: LabeledOrientation,
    ) -> Result<BatchReservation<Edge>, OneOrientationBatchError> {
        self.with_graph_mut(|graph| {
            let labeled = match orientation {
                LabeledOrientation::Forward => graph.forward(),
                LabeledOrientation::Reverse => graph.reverse(),
            };
            labeled.reserve_one_orientation_batch(plan)
        })
    }

    fn rollback_one_orientation_reservations(
        &self,
        reservations: Vec<(LabeledOrientation, BatchReservation<Edge>)>,
    ) {
        for (orientation, reservation) in reservations {
            self.with_graph_mut(|graph| {
                graph.rollback_batch_reservation(orientation, reservation);
            });
        }
    }
}

fn encode_intent_edge(intent: &BatchEdgeIntent) -> Result<Edge, BatchPlacementError> {
    if intent.inline_value_width == 0 {
        Ok(build_edge_to(intent.neighbor_vertex_id))
    } else {
        Ok(build_edge_to_with_inline_value_bytes(
            intent.neighbor_vertex_id,
            &intent.inline_value_bytes,
        ))
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
        for location in &result.locations {
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
            if by_key.insert(key, location.location).is_some() {
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
            inline_value_width: key.inline_value_width,
            edges,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::batch_placement::BatchEdgeInput;
    use super::super::store::helpers::{edge_storage_label, lara_label};
    use super::*;
    use crate::test_labels::install_test_edge_inline_value_profile;
    use gleaph_graph_kernel::entry::EdgeLabelId;
    use ic_stable_lara::VertexId;
    use ic_stable_lara::labeled::batch_write::OneOrientationBatchLocation;
    use ic_stable_lara::lara::edge::free_span::FreeSpanAllocatorStats;
    use ic_stable_lara::lara::edge_inline_value::PayloadAllocatorStats;

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
            inline_value_bytes: bytes,
        }
    }

    fn storage_label_for(catalog_label: Option<EdgeLabelId>, directed: bool) -> u16 {
        lara_label(edge_storage_label(catalog_label, !directed)).raw()
    }

    fn install_width(label: EdgeLabelId, width: u16) {
        install_test_edge_inline_value_profile(
            label,
            gleaph_graph_kernel::entry::EdgeInlineValueProfile {
                byte_width: width,
                encoding: gleaph_graph_kernel::entry::EdgeInlineValueEncoding::RawBytes,
            },
        );
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct AllocatorSnapshot {
        forward_edge_capacity: u64,
        reverse_edge_capacity: u64,
        forward_edge_free: FreeSpanAllocatorStats,
        reverse_edge_free: FreeSpanAllocatorStats,
        forward_payload: PayloadAllocatorStats,
        reverse_payload: PayloadAllocatorStats,
    }

    fn allocator_snapshot(store: &GraphStore) -> AllocatorSnapshot {
        store.with_graph_mut(|graph| AllocatorSnapshot {
            forward_edge_capacity: graph.forward().edges().header().elem_capacity,
            reverse_edge_capacity: graph.reverse().edges().header().elem_capacity,
            forward_edge_free: graph.forward().edges().allocator_stats(),
            reverse_edge_free: graph.reverse().edges().allocator_stats(),
            forward_payload: graph.forward().values().allocator_stats(),
            reverse_payload: graph.reverse().values().allocator_stats(),
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
    fn clean_slab_directed_payload_success() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(1001);
        install_width(label, 8);
        let vertices = make_vertices(&store, 2);
        let source = vertices[0];
        let target = vertices[1];
        let payload = vec![1u8, 2, 3, 4, 5, 6, 7, 8];

        store.prepare_clean_slab_dir_buckets(source, target, label, 8);

        let edges = vec![input(source, target, Some(label), true, payload.clone())];
        let result = store
            .try_insert_batch_edges_clean_slab(&edges)
            .expect("plan/encode ok");
        let locations = match &result {
            BatchEdgeInsertResult::Committed { locations, .. } => locations,
            other => panic!("expected committed batch, got {other:?}"),
        };
        assert!(matches!(
            locations.as_slice(),
            [BatchEdgePhysicalLocation::Directed {
                forward: OneOrientationPhysicalLocation::Slab {
                    payload_byte_offset: Some(_),
                    ..
                },
                reverse: OneOrientationPhysicalLocation::Slab {
                    payload_byte_offset: Some(_),
                    ..
                },
                ..
            }]
        ));
        assert_eq!(result.total_edge_slots(), Some(2));
        assert_eq!(result.total_payload_slots(), Some(2));
        assert!(!result.used_expansion());

        let label_raw = storage_label_for(Some(label), true);
        assert_eq!(count_labeled_dir_edges(&store, source, label_raw, true), 1);
        assert_eq!(count_labeled_dir_edges(&store, target, label_raw, false), 1);

        for edge in store.directed_out_edges(source).expect("out") {
            if edge.label_id == label_raw {
                assert_eq!(edge.edge_inline_value_bytes(), payload.as_slice());
            }
        }
        for edge in store.directed_in_edges(target).expect("in") {
            if edge.label_id == label_raw {
                assert_eq!(edge.edge_inline_value_bytes(), payload.as_slice());
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
        let payload = vec![7u8];

        store.prepare_clean_slab_undir_buckets(a, b, label, 1);

        let edges = vec![input(a, b, Some(label), false, payload.clone())];
        let result = store
            .try_insert_batch_edges_clean_slab(&edges)
            .expect("plan/encode ok");
        assert!(matches!(
            &result,
            BatchEdgeInsertResult::Committed {
                locations,
                ..
            } if matches!(locations.as_slice(), [BatchEdgePhysicalLocation::Undirected { .. }])
        ));
        assert_eq!(result.total_edge_slots(), Some(2));
        assert_eq!(result.total_payload_slots(), Some(2));

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
    fn clean_slab_self_loop_success() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(3001);
        install_width(label, 4);
        let vertices = make_vertices(&store, 1);
        let a = vertices[0];
        let payload = vec![9u8, 8, 7, 6];

        store.prepare_clean_slab_undir_buckets(a, a, label, 4);

        let edges = vec![input(a, a, Some(label), false, payload.clone())];
        let result = store
            .try_insert_batch_edges_clean_slab(&edges)
            .expect("plan/encode ok");
        assert!(matches!(
            &result,
            BatchEdgeInsertResult::Committed {
                locations,
                ..
            } if matches!(locations.as_slice(), [BatchEdgePhysicalLocation::UndirectedSelfLoop { .. }])
        ));
        assert_eq!(result.total_edge_slots(), Some(1));
        assert_eq!(result.total_payload_slots(), Some(1));

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
        assert_eq!(result.total_edge_slots(), Some(4));
        assert_eq!(result.total_payload_slots(), Some(0));
        assert!(!result.used_expansion());

        let label_raw = storage_label_for(Some(label), true);
        assert_eq!(count_labeled_dir_edges(&store, s0, label_raw, true), 1);
        assert_eq!(count_labeled_dir_edges(&store, s1, label_raw, true), 1);
        assert_eq!(count_labeled_dir_edges(&store, t0, label_raw, false), 1);
        assert_eq!(count_labeled_dir_edges(&store, t1, label_raw, false), 1);
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

    /// Reserve both orientations of a payload-bearing directed edge (so both
    /// forward and reverse payload allocations complete), then roll back both
    /// reservations and verify every logical allocator boundary is restored.
    ///
    /// This exercises the cross-orientation path where *multiple* orientations
    /// have already mutated allocator state before the orchestration decides to
    /// abort.  The stable-memory page slack is retained as reusable free-list
    /// space; only the logical capacity, free-list accounting, and occupied
    /// tails are restored.
    #[test]
    fn multi_orientation_payload_reserve_then_rollback_restores_allocator_state() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(5003);
        install_width(label, 8);
        let payload = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
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
            .expand_batch_edge_intents(&[input(source, target, Some(label), true, payload.clone())])
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
        // capacity and allocates payload bytes at its occupied tail.
        let reservations: Vec<(LabeledOrientation, BatchReservation<Edge>)> = requests
            .into_iter()
            .map(|req| {
                let reservation = store
                    .reserve_one_orientation_plan(&req.plan, req.orientation)
                    .expect("reserve ok for both orientations");
                (req.orientation, reservation)
            })
            .collect();

        let after_reserve = allocator_snapshot(&store);

        // Reserve advanced at least one payload occupied tail (the edge
        // logical capacity may already be large enough not to grow for a single
        // edge).
        assert!(
            after_reserve.forward_payload.slab_occupied_tail
                > before.forward_payload.slab_occupied_tail
                || after_reserve.reverse_payload.slab_occupied_tail
                    > before.reverse_payload.slab_occupied_tail,
            "reserve must advance at least one payload occupied tail"
        );
        assert!(
            after_reserve.forward_payload.slab_occupied_tail
                > before.forward_payload.slab_occupied_tail
                || after_reserve.reverse_payload.slab_occupied_tail
                    > before.reverse_payload.slab_occupied_tail,
            "reserve must advance at least one payload occupied tail"
        );

        // Roll back every reservation without committing.  The orchestration
        // helper consumes the tokens, so this is the exact cross-orientation
        // failure path used by `try_insert_batch_edges_clean_slab`.
        store.rollback_one_orientation_reservations(reservations);

        let after_rollback = allocator_snapshot(&store);

        // Logical edge capacity and payload tails are restored on both sides.
        assert_eq!(
            after_rollback.forward_edge_capacity, before.forward_edge_capacity,
            "forward edge logical capacity must be restored"
        );
        assert_eq!(
            after_rollback.reverse_edge_capacity, before.reverse_edge_capacity,
            "reverse edge logical capacity must be restored"
        );
        assert_eq!(
            after_rollback.forward_payload.slab_occupied_tail,
            before.forward_payload.slab_occupied_tail,
            "forward payload occupied tail must be restored"
        );
        assert_eq!(
            after_rollback.reverse_payload.slab_occupied_tail,
            before.reverse_payload.slab_occupied_tail,
            "reverse payload occupied tail must be restored"
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

        // The allocated payload bytes from both orientations became exactly one
        // free span per orientation.  Stable-memory backing capacity is not shrunk.
        let expected_payload_bytes = u64::try_from(payload.len()).unwrap();
        assert_eq!(
            after_rollback.forward_payload.free_bytes - before.forward_payload.free_bytes,
            expected_payload_bytes,
            "forward payload free bytes must increase by exactly the reserved run"
        );
        assert_eq!(
            after_rollback.reverse_payload.free_bytes - before.reverse_payload.free_bytes,
            expected_payload_bytes,
            "reverse payload free bytes must increase by exactly the reserved run"
        );
        assert_eq!(
            after_rollback.forward_payload.free_span_count - before.forward_payload.free_span_count,
            1,
            "forward payload free-list must gain one retired span"
        );
        assert_eq!(
            after_rollback.reverse_payload.free_span_count - before.reverse_payload.free_span_count,
            1,
            "reverse payload free-list must gain one retired span"
        );
        assert!(
            after_rollback.forward_payload.largest_free_span >= expected_payload_bytes,
            "forward largest free span must cover the retired run"
        );
        assert!(
            after_rollback.reverse_payload.largest_free_span >= expected_payload_bytes,
            "reverse largest free span must cover the retired run"
        );
        assert!(
            after_rollback.forward_payload.byte_capacity >= before.forward_payload.byte_capacity,
            "forward stable-memory payload capacity must not shrink"
        );
        assert!(
            after_rollback.reverse_payload.byte_capacity >= before.reverse_payload.byte_capacity,
            "reverse stable-memory payload capacity must not shrink"
        );
    }

    #[test]
    fn reserve_failure_restores_allocator_state() {
        let store = fresh_store();
        let label = EdgeLabelId::from_raw(5002);
        install_width(label, 8);
        let payload = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let vertices = make_vertices(&store, 3);
        let source = vertices[0];
        let target_with_bucket = vertices[1];
        let target_without_bucket = vertices[2];

        // Prepare a forward bucket at source (and an unused reverse bucket at
        // target_with_bucket).  The edge below targets target_without_bucket, whose
        // reverse bucket does not exist, so reverse reserve fails after the forward
        // payload allocation has already happened.
        store.prepare_clean_slab_dir_buckets(source, target_with_bucket, label, 8);

        let before = allocator_snapshot(&store);

        let edges = vec![input(
            source,
            target_without_bucket,
            Some(label),
            true,
            payload.clone(),
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

        // Payload occupied tail is restored for both orientations.
        assert_eq!(
            after.forward_payload.slab_occupied_tail, before.forward_payload.slab_occupied_tail,
            "forward payload occupied tail must be restored"
        );
        assert_eq!(
            after.reverse_payload.slab_occupied_tail, before.reverse_payload.slab_occupied_tail,
            "reverse payload occupied tail must be restored"
        );

        // Reverse payload allocator state is untouched.
        assert_eq!(after.reverse_payload, before.reverse_payload);

        // The forward payload bytes that were allocated before the failure are
        // retired to the free-list as reusable slack. The stable-memory backing
        // capacity is not shrunk.
        let expected_forward_payload_bytes = u64::try_from(payload.len()).unwrap();
        assert_eq!(
            after.forward_payload.free_bytes - before.forward_payload.free_bytes,
            expected_forward_payload_bytes,
            "forward payload free bytes must increase by the allocated run length"
        );
        assert_eq!(
            after.forward_payload.free_span_count - before.forward_payload.free_span_count,
            1,
            "forward payload free-list must gain exactly one retired span"
        );
        assert!(
            after.forward_payload.largest_free_span >= expected_forward_payload_bytes,
            "largest forward payload free span must cover the retired run"
        );
        assert!(
            after.forward_payload.byte_capacity >= before.forward_payload.byte_capacity,
            "stable-memory payload capacity must not shrink on rollback"
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
            .try_insert_batch_edges_clean_slab(&edges)
            .expect("plan/encode ok");
        assert!(
            matches!(
                &result,
                BatchEdgeInsertResult::Committed {
                    locations,
                    ..
                } if locations.iter().all(|location| match location {
                    BatchEdgePhysicalLocation::Directed { forward, reverse, .. } => {
                        matches!(forward, OneOrientationPhysicalLocation::OverflowLog { .. })
                            && matches!(reverse, OneOrientationPhysicalLocation::OverflowLog { .. })
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
            location: OneOrientationPhysicalLocation::Slab {
                edge_slot: 10,
                payload_byte_offset: None,
            },
        };
        let result = OneOrientationBatchResult {
            edge_slots_written: 1,
            edge_log_entries_written: 0,
            payload_slots_written: 0,
            payload_log_entries_written: 0,
            locations: vec![location],
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
                        locations: vec![location, location],
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
