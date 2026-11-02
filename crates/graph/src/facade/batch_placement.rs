//! Internal read-only batch edge placement planning for ADR 0045.
//!
//! This module models how a bounded set of logical edges expands into ordinal-
//! tagged physical half-edge intents and computes a projected LARA ownership/
//! capacity summary without publishing canonical state. It is the first slice of
//! the optimized batch mutation path; no slab write, overflow-log append,
//! rebalance, relocation, alias, sidecar, or derived-index mutation occurs here.

use std::collections::BTreeSet;

use gleaph_gql::{Value, ValueBinaryError};
use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId};
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, VertexId,
    labeled::{LabelBucketPlacementInfo, LabeledOrientation},
};

use super::store::helpers::{canonical_undirected_owner, edge_storage_label, lara_label};
use super::{GraphStore, GraphStoreError, stable::GRAPH};
use crate::edge_inline_property_schema::lookup_edge_inline_property_profile;
use crate::edge_inline_property_schema::resolved_edge_label_with;
use rapidhash::RapidHashMap;

/// One logical edge supplied for optimized batch planning.
///
/// The planner expands this into one or two physical intents owned by LARA.
/// Forward/reverse halves and undirected canonical/alias halves are derived, not
/// supplied. Initial sidecar properties are keyed by `PropertyId`; their order
/// is not semantic and duplicate ids are rejected before the batch write.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchEdgeInput {
    /// Source endpoint. For undirected edges this is endpoint A.
    pub source_vertex_id: VertexId,
    /// Destination endpoint. For undirected edges this is endpoint B.
    pub target_vertex_id: VertexId,
    /// Optional catalog label. `None` means the default unlabeled edge.
    pub catalog_label: Option<EdgeLabelId>,
    /// Whether the edge is directed (`source -> target`) or undirected.
    pub directed: bool,
    /// Fixed-width inline property bytes bytes. Must match the label profile width.
    pub inline_property_bytes: Vec<u8>,
    /// Initial non-inline edge properties owned by the canonical forward row.
    pub initial_edge_properties: Vec<(PropertyId, Value)>,
}

/// Role of one physical half-edge intent within a logical edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BatchEdgeIntentRole {
    /// Canonical forward half of a directed edge, owned by the source vertex.
    CanonicalForward,
    /// Derived reverse half of a directed edge, owned by the target vertex.
    DerivedReverse,
    /// Canonical forward half of an undirected edge, owned by the higher vertex id.
    UndirectedOwnerForward,
    /// Alias forward half of an undirected edge, owned by the lower vertex id.
    UndirectedAliasForward,
}

/// One physical half-edge intent produced by expanding a logical edge.
///
/// Each intent carries a chunk-local ordinal that joins it with its sibling
/// halves and with the eventual inline-property-bytes slot. The ordinal is stable within
/// one planning call and never relies on post-insert neighbor search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchEdgeIntent {
    /// Stable logical ordinal of the input edge that produced this intent.
    pub logical_ordinal: u32,
    /// Physical orientation in the bidirectional LARA store.
    pub orientation: LabeledOrientation,
    /// Role of this half-edge relative to the logical edge.
    pub role: BatchEdgeIntentRole,
    /// Vertex that owns this half-edge (the CSR row owner).
    pub owner_vertex_id: VertexId,
    /// Neighbor vertex referenced by this half-edge.
    pub neighbor_vertex_id: VertexId,
    /// Storage label, including directedness bit.
    pub storage_label: LaraLabelId,
    /// Physical byte width per inline property slot (`0` = no inline property bytes).
    pub inline_property_width: u16,
    /// Inline property bytes carried by this half-edge.
    pub inline_property_bytes: Vec<u8>,
}

/// Error returned when batch planning cannot produce a read-only summary.
#[derive(Debug)]
pub enum BatchPlacementError {
    /// A referenced vertex does not exist or is not live.
    VertexNotLive(VertexId),
    /// An edge label id is not catalog-allocatable.
    InvalidEdgeLabelId(EdgeLabelId),
    /// Inline inline property byte width does not match the label profile.
    InlinePropertyBytesWidthMismatch {
        label: Option<EdgeLabelId>,
        expected: usize,
        actual: usize,
    },
    /// A logical ordinal overflowed the bounded chunk capacity.
    OrdinalOverflow,
    /// A capacity projection sum overflowed.
    ProjectedCapacityOverflow,
    /// A count or size projection sum overflowed.
    ProjectedCountOverflow,
    /// A stable-memory read or placement observation failed.
    PlacementReadFailed(String),
    /// A leaf contains inline property bytes with incompatible widths.
    PayloadWidthMixed,
    /// A logical undirected item did not produce exactly two projections.
    PhysicalProjectionCountMismatch {
        logical_ordinal: u32,
        expected: usize,
        actual: usize,
    },
    /// Physical projections for one logical item disagree on shared metadata.
    PhysicalProjectionMetadataMismatch { logical_ordinal: u32 },
    /// An initial sidecar property id is reserved and cannot be persisted.
    InvalidInitialPropertyId(PropertyId),
    /// An initial sidecar property value cannot be persisted.
    InvalidInitialPropertyValue {
        property_id: PropertyId,
        error: ValueBinaryError,
    },
    /// An item contains the same initial sidecar property id more than once.
    DuplicateInitialPropertyId {
        logical_ordinal: u32,
        property_id: PropertyId,
    },
    /// An initial sidecar property duplicates the label's inline property.
    InitialPropertyConflictsWithInline {
        logical_ordinal: u32,
        property_id: PropertyId,
    },
    /// The one-orientation writer rejected a non-recoverable batch condition.
    /// A Building/Sealing index DML failed its Graph-owned admission before any batch write.
    IndexBuildAdmission(GraphStoreError),
}

impl std::fmt::Display for BatchPlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VertexNotLive(vid) => write!(f, "vertex {vid:?} is not live"),
            Self::InvalidEdgeLabelId(id) => write!(f, "invalid edge label id {id:?}"),
            Self::InlinePropertyBytesWidthMismatch {
                label,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "inline property byte width mismatch for label {label:?}: expected {expected}, actual {actual}"
                )
            }
            Self::OrdinalOverflow => write!(f, "batch logical ordinal overflow"),
            Self::PlacementReadFailed(detail) => {
                write!(f, "placement read failed: {detail}")
            }
            Self::InvalidInitialPropertyId(id) => {
                write!(f, "initial property id {} is reserved", id.raw())
            }
            Self::InvalidInitialPropertyValue { property_id, error } => write!(
                f,
                "initial property {} has an invalid value: {error}",
                property_id.raw()
            ),
            Self::DuplicateInitialPropertyId {
                logical_ordinal,
                property_id,
            } => write!(
                f,
                "logical ordinal {logical_ordinal} repeats initial property id {}",
                property_id.raw()
            ),
            Self::InitialPropertyConflictsWithInline {
                logical_ordinal,
                property_id,
            } => write!(
                f,
                "logical ordinal {logical_ordinal} repeats inline property id {}",
                property_id.raw()
            ),
            Self::PayloadWidthMixed => {
                write!(f, "inline property byte widths are mixed within one leaf")
            }
            Self::PhysicalProjectionCountMismatch {
                logical_ordinal,
                expected,
                actual,
            } => write!(
                f,
                "physical projection count mismatch at ordinal {logical_ordinal}: expected {expected}, actual {actual}"
            ),
            Self::PhysicalProjectionMetadataMismatch { logical_ordinal } => write!(
                f,
                "physical projection metadata mismatch at ordinal {logical_ordinal}"
            ),
            Self::ProjectedCapacityOverflow => write!(f, "projected capacity overflow"),
            Self::ProjectedCountOverflow => write!(f, "projected count overflow"),
            Self::IndexBuildAdmission(error) => {
                write!(f, "index-build admission rejected the batch: {error}")
            }
        }
    }
}

impl std::error::Error for BatchPlacementError {}

impl BatchPlacementError {
    fn from_graph_store_error_for_label(err: GraphStoreError) -> Self {
        match err {
            GraphStoreError::InvalidEdgeLabelId(id) => BatchPlacementError::InvalidEdgeLabelId(id),
            GraphStoreError::EdgeInlinePropertyBytesWidthMismatch {
                label,
                expected,
                actual,
            } => BatchPlacementError::InlinePropertyBytesWidthMismatch {
                label,
                expected,
                actual,
            },
            other => BatchPlacementError::PlacementReadFailed(format!("{other}")),
        }
    }
}

/// Grouping key for projected LARA ownership.
///
/// Intents that share a key compete for the same slab window and overflow log,
/// so their pending counts must be aggregated before projecting capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchPlacementKey {
    /// Forward or reverse orientation.
    pub orientation: LabeledOrientation,
    /// PMA leaf segment that owns the vertex row.
    pub leaf_segment: u32,
    /// Vertex that owns the CSR row.
    pub owner_vertex_id: VertexId,
    /// Storage label, including directedness bit.
    pub storage_label: LaraLabelId,
    /// Physical byte width per inline property slot (`0` = no inline property bytes).
    pub inline_property_width: u16,
}

impl BatchPlacementKey {
    fn as_tuple(self) -> (u8, u32, u32, u64, u16) {
        (
            match self.orientation {
                LabeledOrientation::Forward => 0,
                LabeledOrientation::Reverse => 1,
            },
            self.leaf_segment,
            u32::from(self.owner_vertex_id),
            u64::from(self.storage_label.raw()),
            self.inline_property_width,
        )
    }
}

impl PartialOrd for BatchPlacementKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BatchPlacementKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_tuple().cmp(&other.as_tuple())
    }
}

impl std::hash::Hash for BatchPlacementKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_tuple().hash(state);
    }
}

/// Cheap, mutation-free classification result shared with the ordered writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BatchEdgeClassification {
    pub(crate) intents: Vec<BatchEdgeIntent>,
    pub(crate) logical_ordinals_with_multi_runs: BTreeSet<u32>,
    pub(crate) pending_intents_by_run: RapidHashMap<BatchPlacementKey, Vec<u32>>,
}

impl GraphStore {
    pub(crate) fn validate_batch_initial_edge_properties(
        edges: &[BatchEdgeInput],
    ) -> Result<(), BatchPlacementError> {
        for (logical_ordinal, edge) in edges.iter().enumerate() {
            let logical_ordinal =
                u32::try_from(logical_ordinal).map_err(|_| BatchPlacementError::OrdinalOverflow)?;
            let mut property_ids = BTreeSet::new();
            for (property_id, value) in &edge.initial_edge_properties {
                if !property_ids.insert(*property_id) {
                    return Err(BatchPlacementError::DuplicateInitialPropertyId {
                        logical_ordinal,
                        property_id: *property_id,
                    });
                }
                crate::property::ensure_property_id(*property_id)
                    .map_err(|_| BatchPlacementError::InvalidInitialPropertyId(*property_id))?;
                crate::property::ensure_persistable(value).map_err(|error| {
                    BatchPlacementError::InvalidInitialPropertyValue {
                        property_id: *property_id,
                        error,
                    }
                })?;
                if edge
                    .catalog_label
                    .and_then(|label| resolved_edge_label_with(None, label))
                    .and_then(|label| label.inline_property_id())
                    == Some(*property_id)
                {
                    return Err(BatchPlacementError::InitialPropertyConflictsWithInline {
                        logical_ordinal,
                        property_id: *property_id,
                    });
                }
            }
        }
        Ok(())
    }

    /// Read-only planning entry point for an optimized edge batch.
    ///
    /// Validates vertices and label widths; expands each
    /// logical edge into physical intents; groups by LARA ownership; and reads
    /// existing bucket occupancy to produce a projected-capacity summary. No
    /// canonical state is written.
    /// Validate a batch and expand it into physical half-edge
    /// intents. This is reusable by callers that need the intents without the
    /// full placement summary.
    pub(crate) fn expand_batch_edge_intents(
        &self,
        edges: &[BatchEdgeInput],
    ) -> Result<Vec<BatchEdgeIntent>, BatchPlacementError> {
        #[cfg(feature = "canbench")]
        let _scope = canbench_rs::bench_scope("ordered_batch_expand_intents");
        let mut intents = Vec::with_capacity(edges.len().checked_mul(2).unwrap_or(edges.len()));

        for (ordinal, input) in edges.iter().enumerate() {
            let ordinal =
                u32::try_from(ordinal).map_err(|_| BatchPlacementError::OrdinalOverflow)?;
            expand_logical_edge_to_intents(self, input, ordinal, &mut intents)?;
        }

        Ok(intents)
    }

    /// Classify logical items using only physical-run multiplicity.
    ///
    /// This is the cheap admission check for the all-batch ordered path. It validates and
    /// expands the request, but deliberately does not read leaf or bucket occupancy. The batch
    /// writer remains responsible for the complete geometry reservation and for falling back
    /// before any canonical write when that reservation is unsupported.
    pub(crate) fn classify_batch_edge_insertion(
        &self,
        edges: &[BatchEdgeInput],
    ) -> Result<BatchEdgeClassification, BatchPlacementError> {
        #[cfg(feature = "canbench")]
        let _scope = canbench_rs::bench_scope("ordered_batch_classify_runs");
        let intents = self.expand_batch_edge_intents(edges)?;
        let mut ordinals_by_run = RapidHashMap::<BatchPlacementKey, Vec<u32>>::default();
        for intent in &intents {
            ordinals_by_run
                .entry(batch_placement_key(intent))
                .or_default()
                .push(intent.logical_ordinal);
        }
        let logical_ordinals_with_multi_runs = ordinals_by_run
            .values()
            .filter(|ordinals| ordinals.len() > 1)
            .flat_map(|ordinals| ordinals.iter().copied())
            .collect();
        Ok(BatchEdgeClassification {
            intents,
            logical_ordinals_with_multi_runs,
            pending_intents_by_run: ordinals_by_run,
        })
    }
}

fn expand_logical_edge_to_intents(
    store: &GraphStore,
    input: &BatchEdgeInput,
    ordinal: u32,
    out: &mut Vec<BatchEdgeIntent>,
) -> Result<(), BatchPlacementError> {
    // Validate endpoints are live local vertices.
    if !store.is_vertex_live(input.source_vertex_id) {
        return Err(BatchPlacementError::VertexNotLive(input.source_vertex_id));
    }
    if !store.is_vertex_live(input.target_vertex_id) {
        return Err(BatchPlacementError::VertexNotLive(input.target_vertex_id));
    }

    // Validate a catalog label when present; the absent label uses the default zero-width profile.
    if let Some(catalog_label) = input.catalog_label {
        GraphStore::validate_catalog_edge_label(Some(catalog_label))
            .map_err(BatchPlacementError::from_graph_store_error_for_label)?;
    }
    let expected_width = input
        .catalog_label
        .map(lookup_edge_inline_property_profile)
        .map_or(0, |profile| profile.required_byte_width());
    let actual = input.inline_property_bytes.len();
    let expected = usize::from(expected_width);
    if actual != expected {
        return Err(BatchPlacementError::InlinePropertyBytesWidthMismatch {
            label: input.catalog_label,
            expected,
            actual,
        });
    }

    let storage_label = lara_label(edge_storage_label(input.catalog_label, !input.directed));

    if input.directed {
        // Directed: canonical forward at source, derived reverse at target.
        out.push(BatchEdgeIntent {
            logical_ordinal: ordinal,
            orientation: LabeledOrientation::Forward,
            role: BatchEdgeIntentRole::CanonicalForward,
            owner_vertex_id: input.source_vertex_id,
            neighbor_vertex_id: input.target_vertex_id,
            storage_label,
            inline_property_width: expected_width,
            inline_property_bytes: input.inline_property_bytes.clone(),
        });
        out.push(BatchEdgeIntent {
            logical_ordinal: ordinal,
            orientation: LabeledOrientation::Reverse,
            role: BatchEdgeIntentRole::DerivedReverse,
            owner_vertex_id: input.target_vertex_id,
            neighbor_vertex_id: input.source_vertex_id,
            storage_label,
            inline_property_width: expected_width,
            inline_property_bytes: input.inline_property_bytes.clone(),
        });
    } else {
        // Undirected: two forward halves. The canonical owner is the higher id.
        let owner = canonical_undirected_owner(input.source_vertex_id, input.target_vertex_id);
        let alias = if owner == input.source_vertex_id {
            input.target_vertex_id
        } else {
            input.source_vertex_id
        };
        out.push(BatchEdgeIntent {
            logical_ordinal: ordinal,
            orientation: LabeledOrientation::Forward,
            role: BatchEdgeIntentRole::UndirectedOwnerForward,
            owner_vertex_id: owner,
            neighbor_vertex_id: if owner == input.source_vertex_id {
                input.target_vertex_id
            } else {
                input.source_vertex_id
            },
            storage_label,
            inline_property_width: expected_width,
            inline_property_bytes: input.inline_property_bytes.clone(),
        });
        // Self-loops produce only one forward half.
        if owner != alias {
            out.push(BatchEdgeIntent {
                logical_ordinal: ordinal,
                orientation: LabeledOrientation::Forward,
                role: BatchEdgeIntentRole::UndirectedAliasForward,
                owner_vertex_id: alias,
                neighbor_vertex_id: owner,
                storage_label,
                inline_property_width: expected_width,
                inline_property_bytes: input.inline_property_bytes.clone(),
            });
        }
    }

    Ok(())
}

fn batch_placement_key(intent: &BatchEdgeIntent) -> BatchPlacementKey {
    BatchPlacementKey {
        orientation: intent.orientation,
        leaf_segment: leaf_index_for_vertex(intent.owner_vertex_id, segment_size()),
        owner_vertex_id: intent.owner_vertex_id,
        storage_label: intent.storage_label,
        inline_property_width: intent.inline_property_width,
    }
}

fn read_existing_bucket_placement(
    key: BatchPlacementKey,
) -> Result<Option<LabelBucketPlacementInfo>, BatchPlacementError> {
    GRAPH.with_borrow(|graph| {
        let result = match key.orientation {
            LabeledOrientation::Forward => {
                graph.read_forward_bucket_placement_info(key.owner_vertex_id, key.storage_label)
            }
            LabeledOrientation::Reverse => {
                graph.read_reverse_bucket_placement_info(key.owner_vertex_id, key.storage_label)
            }
        };
        result.map_err(|err| {
            BatchPlacementError::PlacementReadFailed(format!(
                "bucket placement info for owner {:?} label {:?} {:?}: {err}",
                key.owner_vertex_id, key.storage_label, key.orientation
            ))
        })
    })
}

pub(crate) fn segment_size() -> u32 {
    GRAPH.with_borrow(|graph| graph.forward().edges().header().segment_size.max(1))
}

pub(crate) fn leaf_index_for_vertex(vid: VertexId, segment_size: u32) -> u32 {
    u32::from(vid) / segment_size.max(1)
}

#[cfg(test)]
mod tests {
    use super::super::stable::{
        DERIVED_INDEX_OUTBOX, GRAPH_MUTATION_JOURNAL, LABEL_STATS_DELTA_LOG, PENDING_VERTEX_PURGES,
        UNIQUE_EFFECT_OUTBOX, memory::stable_memory_stats,
    };
    use super::*;
    use crate::test_labels::{edge_label_id_for_name, install_test_edge_inline_property_profile};
    use gleaph_graph_kernel::entry::{Edge, EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};
    use gleaph_graph_kernel::stable_memory::StableMemoryStats;

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

    fn directed_edge_expands_to_forward_and_reverse_intents() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchDirected");
        let edges = vec![input(v[0], v[1], Some(label), true, vec![])];
        let classification = store
            .classify_batch_edge_insertion(&edges)
            .expect("classify");
        assert_eq!(classification.intents.len(), 2);
        let forward = classification
            .intents
            .iter()
            .find(|i| i.orientation == LabeledOrientation::Forward)
            .expect("forward intent");
        assert_eq!(forward.owner_vertex_id, v[0]);
        let reverse = classification
            .intents
            .iter()
            .find(|i| i.orientation == LabeledOrientation::Reverse)
            .expect("reverse intent");
        assert_eq!(reverse.owner_vertex_id, v[1]);
    }

    fn undirected_edge_expands_to_two_forward_intents() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchUndirected");
        let edges = vec![input(v[0], v[1], Some(label), false, vec![])];
        let classification = store
            .classify_batch_edge_insertion(&edges)
            .expect("classify");
        assert_eq!(classification.intents.len(), 2);
        assert!(
            classification
                .intents
                .iter()
                .all(|i| i.orientation == LabeledOrientation::Forward)
        );
        let owners: Vec<_> = classification
            .intents
            .iter()
            .map(|i| i.owner_vertex_id)
            .collect();
        assert!(owners.contains(&v[0]));
        assert!(owners.contains(&v[1]));
    }

    fn undirected_self_loop_produces_single_forward_intent() {
        let store = fresh_store();
        let v = make_vertices(&store, 1);
        let label = edge_label_id_for_name("BatchSelfLoop");
        let edges = vec![input(v[0], v[0], Some(label), false, vec![])];
        let classification = store
            .classify_batch_edge_insertion(&edges)
            .expect("classify");
        assert_eq!(classification.intents.len(), 1);
        assert_eq!(
            classification.intents[0].orientation,
            LabeledOrientation::Forward
        );
    }

    fn parallel_edges_share_owner_and_label_but_different_neighbor() {
        let store = fresh_store();
        let v = make_vertices(&store, 3);
        let label = edge_label_id_for_name("BatchParallel");
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![]),
            input(v[0], v[2], Some(label), true, vec![]),
        ];
        let classification = store
            .classify_batch_edge_insertion(&edges)
            .expect("classify");
        let forward: Vec<_> = classification
            .intents
            .iter()
            .filter(|i| i.orientation == LabeledOrientation::Forward)
            .collect();
        assert_eq!(forward.len(), 2, "one forward intent per logical edge");
        assert!(forward.iter().all(|i| i.owner_vertex_id == v[0]));
    }

    #[test]
    fn singleton_physical_runs_are_classified_for_scalar_fallback() {
        let store = fresh_store();
        let v = make_vertices(&store, 4);
        let label = edge_label_id_for_name("BatchSingletonClassification");
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![]),
            input(v[2], v[3], Some(label), true, vec![]),
        ];
        assert!(
            store
                .classify_batch_edge_insertion(&edges)
                .expect("classify")
                .logical_ordinals_with_multi_runs
                .is_empty()
        );
    }

    #[test]
    fn multi_item_physical_run_remains_batch_eligible() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchMultiRunClassification");
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![]),
            input(v[0], v[1], Some(label), true, vec![]),
        ];
        assert_eq!(
            store
                .classify_batch_edge_insertion(&edges)
                .expect("classify")
                .logical_ordinals_with_multi_runs
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn parallel_logical_edge_target_is_admitted() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchDuplicate");
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![]),
            input(v[0], v[1], Some(label), true, vec![]),
        ];
        let classification = store
            .classify_batch_edge_insertion(&edges)
            .expect("parallel plan");
        assert_eq!(
            classification.intents.len(),
            4,
            "two intents per logical edge"
        );
    }

    #[test]
    fn undirected_parallel_edge_endpoints_are_admitted_in_input_order() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchUndirectedCanonical");
        let edges = vec![
            input(v[0], v[1], Some(label), false, vec![]),
            input(v[1], v[0], Some(label), false, vec![]),
        ];
        let classification = store
            .classify_batch_edge_insertion(&edges)
            .expect("parallel plan");
        assert_eq!(
            classification.intents.len(),
            4,
            "two intents per logical edge"
        );
    }

    #[test]
    fn parallel_edges_may_have_distinct_inline_properties() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchConflictPayload");
        install_test_edge_inline_property_profile(
            label,
            EdgeInlinePropertyProfile {
                byte_width: 2,
                encoding: EdgeInlinePropertyEncoding::RawU16,
            },
        );
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![1, 0]),
            input(v[0], v[1], Some(label), true, vec![2, 0]),
        ];
        let classification = store
            .classify_batch_edge_insertion(&edges)
            .expect("parallel plan");
        assert_eq!(
            classification.intents.len(),
            4,
            "two intents per logical edge"
        );
    }

    #[test]
    fn parallel_edges_may_have_identical_inline_properties() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchIdenticalPayload");
        install_test_edge_inline_property_profile(
            label,
            EdgeInlinePropertyProfile {
                byte_width: 2,
                encoding: EdgeInlinePropertyEncoding::RawU16,
            },
        );
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![1, 0]),
            input(v[0], v[1], Some(label), true, vec![1, 0]),
        ];
        let classification = store
            .classify_batch_edge_insertion(&edges)
            .expect("parallel plan");
        assert_eq!(
            classification.intents.len(),
            4,
            "two intents per logical edge"
        );
    }

    #[test]
    fn missing_vertex_is_rejected_before_any_intent_is_emitted() {
        let store = fresh_store();
        let v0 = store.insert_vertex().expect("vertex");
        let missing = VertexId::from(999);
        let label = edge_label_id_for_name("BatchMissingVertex");
        let edges = vec![input(v0, missing, Some(label), true, vec![])];
        let err = store
            .classify_batch_edge_insertion(&edges)
            .expect_err("missing");
        assert!(matches!(err, BatchPlacementError::VertexNotLive(_)));
    }

    #[test]
    fn inline_property_width_must_match_label_profile() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchWidth");
        install_test_edge_inline_property_profile(
            label,
            EdgeInlinePropertyProfile {
                byte_width: 2,
                encoding: EdgeInlinePropertyEncoding::RawU16,
            },
        );
        let edges = vec![input(v[0], v[1], Some(label), true, vec![1])];
        let err = store
            .classify_batch_edge_insertion(&edges)
            .expect_err("width");
        assert!(matches!(
            err,
            BatchPlacementError::InlinePropertyBytesWidthMismatch {
                expected: 2,
                actual: 1,
                ..
            }
        ));
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct GraphStateSnapshot {
        vertex_count: u64,
        out_edges: Vec<Edge>,
        in_edges: Vec<Edge>,
        undirected_edges: Vec<Edge>,
        stable_memory_stats: StableMemoryStats,
        mutation_journal_len: u64,
        unique_outbox_len: u64,
        repair_journal_len: u64,
        derived_index_outbox_len: u64,
        label_stats_delta_log_len: u64,
        has_pending_vertex_purges: bool,
    }

    fn snapshot_graph_state(store: &GraphStore, sample: &[VertexId]) -> GraphStateSnapshot {
        // Read all stable-derived lengths first so that lazy thread-local initialization
        // happens before `stable_memory_stats()` observes memory sizes. This keeps the
        // before/after snapshots comparable even though the planner does not mutate state.
        let mutation_journal_len = GRAPH_MUTATION_JOURNAL.with_borrow(|m| m.len());
        let unique_outbox_len = UNIQUE_EFFECT_OUTBOX.with_borrow(|o| o.len());
        let repair_journal_len = store.repair_journal_len();
        let derived_index_outbox_len = DERIVED_INDEX_OUTBOX.with_borrow(|o| o.len());
        let label_stats_delta_log_len = LABEL_STATS_DELTA_LOG.with_borrow(|l| l.len());
        let has_pending_vertex_purges = PENDING_VERTEX_PURGES.with_borrow(|s| !s.is_empty());
        GraphStateSnapshot {
            vertex_count: u64::from(u32::from(store.vertex_count())),
            out_edges: store
                .directed_out_edges(sample[0])
                .expect("out edges snapshot"),
            in_edges: store
                .directed_in_edges(sample[1])
                .expect("in edges snapshot"),
            undirected_edges: store
                .undirected_edges(sample[2])
                .expect("undirected edges snapshot"),
            stable_memory_stats: stable_memory_stats(),
            mutation_journal_len,
            unique_outbox_len,
            repair_journal_len,
            derived_index_outbox_len,
            label_stats_delta_log_len,
            has_pending_vertex_purges,
        }
    }

    #[test]
    fn default_label_edge_is_planned_with_default_storage_bucket() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let edges = vec![input(v[0], v[1], None, true, vec![])];
        let classification = store
            .classify_batch_edge_insertion(&edges)
            .expect("default label batch plan");
        assert_eq!(classification.intents.len(), 2);
        assert!(
            classification
                .intents
                .iter()
                .all(|intent| intent.storage_label == LaraLabelId::UNLABELED_DIRECTED)
        );
    }
}
