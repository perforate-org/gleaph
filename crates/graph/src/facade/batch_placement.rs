//! Internal read-only batch edge placement planning for ADR 0045.
//!
//! This module models how a bounded set of logical edges expands into ordinal-
//! tagged physical half-edge intents and computes a projected LARA ownership/
//! capacity summary without publishing canonical state. It is the first slice of
//! the unordered batch mutation path; no slab write, overflow-log append,
//! rebalance, relocation, alias, sidecar, or derived-index mutation occurs here.

use std::collections::{BTreeMap, BTreeSet};

use gleaph_graph_kernel::entry::EdgeLabelId;
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, VertexId,
    labeled::{LabelBucketPlacementInfo, LabeledOrientation, LeafBucketPlacementStats},
};

use super::store::helpers::{canonical_undirected_owner, edge_storage_label, lara_label};
use super::{GraphStore, GraphStoreError, stable::GRAPH};
use crate::edge_inline_value_schema::lookup_edge_inline_value_profile;
use rapidhash::{HashMapExt, RapidHashMap};

/// One logical edge supplied by a client for unordered batch planning.
///
/// The planner expands this into one or two physical intents owned by LARA.
/// Forward/reverse halves and undirected canonical/alias halves are derived, not
/// supplied. Edge properties (other than the fixed-width inline payload) are out
/// of scope for this slice and fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchEdgeInput {
    /// Source endpoint. For undirected edges this is endpoint A.
    pub source_vertex_id: VertexId,
    /// Destination endpoint. For undirected edges this is endpoint B.
    pub target_vertex_id: VertexId,
    /// Optional catalog label. `None` means the default unlabeled edge.
    pub catalog_label: Option<EdgeLabelId>,
    /// Whether the edge is directed (`source -> target`) or undirected.
    pub directed: bool,
    /// Fixed-width inline payload bytes. Must match the label profile width.
    pub inline_value_bytes: Vec<u8>,
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
/// halves and with the eventual inline-value slot. The ordinal is stable within
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
    /// Physical byte width per inline value slot (`0` = no payload).
    pub inline_value_width: u16,
    /// Inline payload bytes carried by this half-edge.
    pub inline_value_bytes: Vec<u8>,
}

/// Error returned when batch planning cannot produce a read-only summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchPlacementError {
    /// A referenced vertex does not exist or is not live.
    VertexNotLive(VertexId),
    /// An edge label id is not catalog-allocatable.
    InvalidEdgeLabelId(EdgeLabelId),
    /// Inline payload width does not match the label profile.
    InlineValueWidthMismatch {
        label: Option<EdgeLabelId>,
        expected: usize,
        actual: usize,
    },
    /// Duplicate logical edge target in the same unordered chunk.
    DuplicateEdgeTarget,
    /// The same logical edge target was supplied with conflicting inline values.
    ConflictingDuplicateEdgeTarget,
    /// A logical ordinal overflowed the bounded chunk capacity.
    OrdinalOverflow,
    /// A capacity projection sum overflowed.
    ProjectedCapacityOverflow,
    /// A count or size projection sum overflowed.
    ProjectedCountOverflow,
    /// The logical edge batch exceeded the bounded input limit.
    BatchTooLarge,
    /// A stable-memory read or placement observation failed.
    PlacementReadFailed(String),
    /// A leaf contains payloads with incompatible inline widths.
    PayloadWidthMixed,
    /// Default/unlabeled edges are not supported by the read-only batch planner.
    ///
    /// The default-label bypass path has its own stable layout; supporting it in
    /// the planner requires a separate read-only occupancy accessor. Plan 0121
    /// rejects the path rather than silently under-count existing occupancy.
    DefaultLabelUnsupported,
}

impl std::fmt::Display for BatchPlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VertexNotLive(vid) => write!(f, "vertex {vid:?} is not live"),
            Self::InvalidEdgeLabelId(id) => write!(f, "invalid edge label id {id:?}"),
            Self::InlineValueWidthMismatch {
                label,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "inline value width mismatch for label {label:?}: expected {expected}, actual {actual}"
                )
            }
            Self::DuplicateEdgeTarget => write!(f, "duplicate edge target in batch"),
            Self::ConflictingDuplicateEdgeTarget => {
                write!(
                    f,
                    "duplicate edge target with conflicting inline value in batch"
                )
            }
            Self::OrdinalOverflow => write!(f, "batch logical ordinal overflow"),
            Self::BatchTooLarge => write!(f, "batch logical edge count exceeds the bounded limit"),
            Self::PlacementReadFailed(detail) => {
                write!(f, "placement read failed: {detail}")
            }
            Self::DefaultLabelUnsupported => {
                write!(
                    f,
                    "default/unlabeled edges are not supported by batch placement"
                )
            }
            Self::PayloadWidthMixed => write!(f, "payload widths are mixed within one leaf"),
            Self::ProjectedCapacityOverflow => write!(f, "projected capacity overflow"),
            Self::ProjectedCountOverflow => write!(f, "projected count overflow"),
        }
    }
}

impl std::error::Error for BatchPlacementError {}

impl BatchPlacementError {
    fn from_graph_store_error_for_label(err: GraphStoreError) -> Self {
        match err {
            GraphStoreError::InvalidEdgeLabelId(id) => BatchPlacementError::InvalidEdgeLabelId(id),
            GraphStoreError::EdgeInlineValueWidthMismatch {
                label,
                expected,
                actual,
            } => BatchPlacementError::InlineValueWidthMismatch {
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
    /// Physical byte width per inline value slot (`0` = no payload).
    pub inline_value_width: u16,
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
            self.inline_value_width,
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

/// Placement summary for one ownership group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchPlacementGroup {
    /// Ownership key for this group.
    pub key: BatchPlacementKey,
    /// Number of physical half-edge intents pending in this group.
    pub pending_edge_intents: u64,
    /// Existing edge-slab slots reserved for this bucket, or zero if no bucket exists.
    pub resident_slab_edge_slots: u32,
    /// Existing edge overflow-log entries for this bucket, or zero if none.
    pub resident_log_edge_slots: u32,
    /// Existing inline-value slab slots reserved for this bucket, or zero.
    pub resident_slab_payload_slots: u32,
    /// Existing inline-value overflow-log entries for this bucket, or zero.
    pub resident_log_payload_slots: u32,
}

impl BatchPlacementGroup {
    /// Minimum edge slots required to hold all resident and pending edges.
    ///
    /// Uses checked arithmetic and fails closed on overflow.
    pub fn projected_minimum_edge_slots(&self) -> Result<u64, BatchPlacementError> {
        u64::from(self.resident_slab_edge_slots)
            .checked_add(u64::from(self.resident_log_edge_slots))
            .and_then(|s| s.checked_add(self.pending_edge_intents))
            .ok_or(BatchPlacementError::ProjectedCapacityOverflow)
    }

    /// Minimum inline-value slots required to hold all resident and pending values.
    pub fn projected_minimum_payload_slots(&self) -> Result<u64, BatchPlacementError> {
        u64::from(self.resident_slab_payload_slots)
            .checked_add(u64::from(self.resident_log_payload_slots))
            .and_then(|s| s.checked_add(self.pending_edge_intents))
            .ok_or(BatchPlacementError::ProjectedCapacityOverflow)
    }

    /// Minimum inline-value bytes required to hold all resident and pending values.
    pub fn projected_minimum_payload_bytes(&self) -> Result<u64, BatchPlacementError> {
        let slots = self.projected_minimum_payload_slots()?;
        slots
            .checked_mul(u64::from(self.key.inline_value_width))
            .ok_or(BatchPlacementError::ProjectedCapacityOverflow)
    }
}

/// Aggregated projected capacity for one PMA leaf segment and orientation.
///
/// One leaf hosts many bucket groups that share physical slab/log capacity.
/// The per-bucket [`BatchPlacementGroup`] minimum is necessary but not sufficient;
/// this aggregate catches leaf-level pressure when multiple owner/label groups
/// share the same leaf.
///
/// Resident counts are reported in two ways:
///
/// - `target_*`: sum over buckets directly targeted by the pending batch.
/// - `full_leaf_*`: sum over **all** existing buckets on the leaf, including buckets
///   that are not targeted by this batch. ADR 0045 projected geometry must use the
///   full-leaf view because an untouched bucket still occupies shared slab/log space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchPlacementLeafSummary {
    /// Orientation that owns this leaf projection.
    pub orientation: LabeledOrientation,
    /// PMA leaf segment index.
    pub leaf_segment: u32,
    /// Sum of edge-slab slots reserved by buckets targeted by this batch.
    pub target_resident_slab_edge_slots: u64,
    /// Sum of edge overflow-log slots reserved by buckets targeted by this batch.
    pub target_resident_log_edge_slots: u64,
    /// Number of pending edge intents targeting this leaf.
    pub pending_edge_intents: u64,
    /// Sum of edge-slab slots reserved by **all** existing buckets on this leaf.
    pub full_leaf_resident_slab_edge_slots: u64,
    /// Sum of edge overflow-log slots reserved by **all** existing buckets on this leaf.
    pub full_leaf_resident_log_edge_slots: u64,
    /// Sum of inline-value slab slots reserved by buckets targeted by this batch.
    pub target_resident_slab_payload_slots: u64,
    /// Sum of inline-value overflow-log slots reserved by buckets targeted by this batch.
    pub target_resident_log_payload_slots: u64,
    /// Number of pending payload intents targeting this leaf.
    pub pending_payload_intents: u64,
    /// Sum of inline-value slab slots reserved by **all** existing buckets on this leaf.
    pub full_leaf_resident_slab_payload_slots: u64,
    /// Sum of inline-value overflow-log slots reserved by **all** existing buckets on this leaf.
    pub full_leaf_resident_log_payload_slots: u64,
    /// Payload widths represented by resident and pending payloads on this leaf.
    pub payload_widths: BTreeSet<u16>,
}

impl BatchPlacementLeafSummary {
    /// Minimum edge slots required to hold all leaf-wide resident and pending edges.
    ///
    /// Uses the full-leaf resident view, not only the targeted buckets.
    pub fn projected_minimum_edge_slots(&self) -> Result<u64, BatchPlacementError> {
        self.full_leaf_resident_slab_edge_slots
            .checked_add(self.full_leaf_resident_log_edge_slots)
            .and_then(|s| s.checked_add(self.pending_edge_intents))
            .ok_or(BatchPlacementError::ProjectedCapacityOverflow)
    }

    /// Minimum inline-value slots required to hold all leaf-wide resident and pending values.
    pub fn projected_minimum_payload_slots(&self) -> Result<u64, BatchPlacementError> {
        if self.payload_widths.len() > 1 {
            return Err(BatchPlacementError::PayloadWidthMixed);
        }
        self.full_leaf_resident_slab_payload_slots
            .checked_add(self.full_leaf_resident_log_payload_slots)
            .and_then(|s| s.checked_add(self.pending_payload_intents))
            .ok_or(BatchPlacementError::ProjectedCapacityOverflow)
    }
}

/// Read-only placement summary for a bounded unordered edge batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchPlacementSummary {
    /// Number of logical edges supplied as input.
    pub logical_edge_count: u32,
    /// Total number of physical half-edge intents after expansion.
    pub physical_intent_count: u64,
    /// Placement groups keyed by ownership.
    pub groups: BTreeMap<BatchPlacementKey, BatchPlacementGroup>,
    /// Leaf-level projected capacity by orientation and leaf segment.
    pub leaf_summaries: BTreeMap<OrientationLeafKey, BatchPlacementLeafSummary>,
}

/// Key for leaf-level summaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrientationLeafKey {
    /// Orientation that owns this leaf.
    pub orientation: LabeledOrientation,
    /// PMA leaf segment index.
    pub leaf_segment: u32,
}

impl OrientationLeafKey {
    fn as_tuple(self) -> (u8, u32) {
        (
            match self.orientation {
                LabeledOrientation::Forward => 0,
                LabeledOrientation::Reverse => 1,
            },
            self.leaf_segment,
        )
    }
}

impl PartialOrd for OrientationLeafKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrientationLeafKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_tuple().cmp(&other.as_tuple())
    }
}

impl std::hash::Hash for OrientationLeafKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_tuple().hash(state);
    }
}

impl BatchPlacementSummary {
    /// Total pending edge intents across all groups.
    pub fn total_pending_edge_intents(&self) -> u64 {
        self.groups.values().map(|g| g.pending_edge_intents).sum()
    }

    /// Total pending edge intents across all leaf summaries.
    pub fn total_leaf_pending_edge_intents(&self) -> u64 {
        self.leaf_summaries
            .values()
            .map(|l| l.pending_edge_intents)
            .sum()
    }
}

/// Internal duplicate-detection key for a logical edge target.
///
/// ADR 0045 rejects duplicate mutations targeting the same logical edge in one
/// unordered chunk, regardless of inline payload. Directed edges are keyed by
/// (source, target, label). Undirected edges are keyed by canonical endpoint pair
/// (higher id first) and label, so `(a,b)` and `(b,a)` collide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct EdgeTargetKey {
    endpoint_a: VertexId,
    endpoint_b: VertexId,
    catalog_label: Option<EdgeLabelId>,
    directed: bool,
}

impl BatchEdgeInput {
    /// Duplicate-detection key for the logical edge target.
    ///
    /// Payload is intentionally **not** part of the key: the same logical target
    /// with different inline values is a conflicting duplicate and is rejected
    /// separately via [`BatchPlacementError::ConflictingDuplicateEdgeTarget`].
    fn target_key(&self) -> EdgeTargetKey {
        if self.directed {
            EdgeTargetKey {
                endpoint_a: self.source_vertex_id,
                endpoint_b: self.target_vertex_id,
                catalog_label: self.catalog_label,
                directed: true,
            }
        } else {
            // Canonicalize undirected endpoints so (a,b) and (b,a) collide.
            let a = canonical_undirected_owner(self.source_vertex_id, self.target_vertex_id);
            let b = if a == self.source_vertex_id {
                self.target_vertex_id
            } else {
                self.source_vertex_id
            };
            EdgeTargetKey {
                endpoint_a: a,
                endpoint_b: b,
                catalog_label: self.catalog_label,
                directed: false,
            }
        }
    }

    /// Returns `true` if two inputs share the same logical edge target.
    fn same_logical_target(&self, other: &BatchEdgeInput) -> bool {
        self.target_key() == other.target_key()
    }
}

impl GraphStore {
    /// Read-only planning entry point for an unordered edge batch.
    ///
    /// Validates vertices, label widths, and duplicate targets; expands each
    /// logical edge into physical intents; groups by LARA ownership; and reads
    /// existing bucket occupancy to produce a projected-capacity summary. No
    /// canonical state is written.
    /// Maximum logical edges accepted in one read-only planning call.
    ///
    /// This is a focused-slice bound for the baseline planner; later slices that
    /// add ingress chunking may raise it once request-size and instruction
    /// budgets are measured.
    pub const MAX_LOGICAL_EDGES: u32 = 1024 * 1024;

    /// Validate a bounded unordered batch and expand it into physical half-edge
    /// intents. This is reusable by callers that need the intents without the
    /// full placement summary.
    pub(crate) fn expand_batch_edge_intents(
        &self,
        edges: &[BatchEdgeInput],
    ) -> Result<Vec<BatchEdgeIntent>, BatchPlacementError> {
        let logical_count =
            u32::try_from(edges.len()).map_err(|_| BatchPlacementError::BatchTooLarge)?;
        if logical_count > Self::MAX_LOGICAL_EDGES {
            return Err(BatchPlacementError::BatchTooLarge);
        }

        let mut first_index: RapidHashMap<EdgeTargetKey, usize> =
            RapidHashMap::with_capacity(edges.len());
        let mut intents = Vec::with_capacity(edges.len().checked_mul(2).unwrap_or(edges.len()));

        for (ordinal, input) in edges.iter().enumerate() {
            let ordinal =
                u32::try_from(ordinal).map_err(|_| BatchPlacementError::OrdinalOverflow)?;
            let key = input.target_key();
            if let Some(&first_idx) = first_index.get(&key) {
                if edges[first_idx].inline_value_bytes != input.inline_value_bytes {
                    return Err(BatchPlacementError::ConflictingDuplicateEdgeTarget);
                }
                return Err(BatchPlacementError::DuplicateEdgeTarget);
            }
            first_index.insert(key, ordinal as usize);
            expand_logical_edge_to_intents(self, input, ordinal, &mut intents)?;
        }

        Ok(intents)
    }

    pub fn plan_batch_edge_insertion(
        &self,
        edges: &[BatchEdgeInput],
    ) -> Result<BatchPlacementSummary, BatchPlacementError> {
        let intents = self.expand_batch_edge_intents(edges)?;
        let (groups, leaf_summaries) = group_intents_for_placement(&intents)?;
        let physical_intent_count = u64::try_from(intents.len())
            .map_err(|_| BatchPlacementError::ProjectedCountOverflow)?;
        let logical_edge_count =
            u32::try_from(edges.len()).map_err(|_| BatchPlacementError::BatchTooLarge)?;

        Ok(BatchPlacementSummary {
            logical_edge_count,
            physical_intent_count,
            groups,
            leaf_summaries,
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

    // Plan 0121 only supports catalog-labeled edges. Default/unlabeled bypass
    // has a distinct stable layout and is rejected rather than silently under-
    // counting existing occupancy.
    if input.catalog_label.is_none() {
        return Err(BatchPlacementError::DefaultLabelUnsupported);
    }
    let catalog_label = input.catalog_label.expect("checked above");

    // Validate catalog label and inline value width.
    GraphStore::validate_catalog_edge_label(Some(catalog_label))
        .map_err(BatchPlacementError::from_graph_store_error_for_label)?;
    let expected_width = lookup_edge_inline_value_profile(catalog_label).required_byte_width();
    let actual = input.inline_value_bytes.len();
    let expected = usize::from(expected_width);
    if actual != expected {
        return Err(BatchPlacementError::InlineValueWidthMismatch {
            label: input.catalog_label,
            expected,
            actual,
        });
    }

    let storage_label = lara_label(edge_storage_label(Some(catalog_label), !input.directed));

    if input.directed {
        // Directed: canonical forward at source, derived reverse at target.
        out.push(BatchEdgeIntent {
            logical_ordinal: ordinal,
            orientation: LabeledOrientation::Forward,
            role: BatchEdgeIntentRole::CanonicalForward,
            owner_vertex_id: input.source_vertex_id,
            neighbor_vertex_id: input.target_vertex_id,
            storage_label,
            inline_value_width: expected_width,
            inline_value_bytes: input.inline_value_bytes.clone(),
        });
        out.push(BatchEdgeIntent {
            logical_ordinal: ordinal,
            orientation: LabeledOrientation::Reverse,
            role: BatchEdgeIntentRole::DerivedReverse,
            owner_vertex_id: input.target_vertex_id,
            neighbor_vertex_id: input.source_vertex_id,
            storage_label,
            inline_value_width: expected_width,
            inline_value_bytes: input.inline_value_bytes.clone(),
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
            inline_value_width: expected_width,
            inline_value_bytes: input.inline_value_bytes.clone(),
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
                inline_value_width: expected_width,
                inline_value_bytes: input.inline_value_bytes.clone(),
            });
        }
    }

    Ok(())
}

fn group_intents_for_placement(
    intents: &[BatchEdgeIntent],
) -> Result<
    (
        BTreeMap<BatchPlacementKey, BatchPlacementGroup>,
        BTreeMap<OrientationLeafKey, BatchPlacementLeafSummary>,
    ),
    BatchPlacementError,
> {
    let segment_size = segment_size();
    let mut groups: BTreeMap<BatchPlacementKey, BatchPlacementGroup> = BTreeMap::new();
    let mut leaf_summaries: BTreeMap<OrientationLeafKey, BatchPlacementLeafSummary> =
        BTreeMap::new();

    for intent in intents {
        let leaf_segment = leaf_index_for_vertex(intent.owner_vertex_id, segment_size);
        let key = BatchPlacementKey {
            orientation: intent.orientation,
            leaf_segment,
            owner_vertex_id: intent.owner_vertex_id,
            storage_label: intent.storage_label,
            inline_value_width: intent.inline_value_width,
        };
        use std::collections::btree_map::Entry;
        if let Entry::Vacant(slot) = groups.entry(key) {
            let existing = read_existing_bucket_placement(key)?;
            slot.insert(BatchPlacementGroup {
                key,
                pending_edge_intents: 0,
                resident_slab_edge_slots: existing.map(|e| e.stored_edge_slots).unwrap_or(0),
                resident_log_edge_slots: existing.map(|e| e.edge_overflow_log_len).unwrap_or(0),
                resident_slab_payload_slots: existing
                    .map(|e| e.inline_value_slab_slots)
                    .unwrap_or(0),
                resident_log_payload_slots: existing
                    .map(|e| e.payload_overflow_log_len)
                    .unwrap_or(0),
            });
        }
        let group = groups.get_mut(&key).expect("group inserted above");
        group.pending_edge_intents = group
            .pending_edge_intents
            .checked_add(1)
            .ok_or(BatchPlacementError::ProjectedCountOverflow)?;

        // Aggregate pending counts into leaf summary. Resident counts are added once per
        // bucket group below, after all intents have been counted.
        let leaf_key = OrientationLeafKey {
            orientation: intent.orientation,
            leaf_segment,
        };
        let leaf_entry = leaf_summaries
            .entry(leaf_key)
            .or_insert(BatchPlacementLeafSummary {
                orientation: intent.orientation,
                leaf_segment,
                target_resident_slab_edge_slots: 0,
                target_resident_log_edge_slots: 0,
                pending_edge_intents: 0,
                full_leaf_resident_slab_edge_slots: 0,
                full_leaf_resident_log_edge_slots: 0,
                target_resident_slab_payload_slots: 0,
                target_resident_log_payload_slots: 0,
                pending_payload_intents: 0,
                full_leaf_resident_slab_payload_slots: 0,
                full_leaf_resident_log_payload_slots: 0,
                payload_widths: BTreeSet::new(),
            });
        leaf_entry.pending_edge_intents = leaf_entry
            .pending_edge_intents
            .checked_add(1)
            .ok_or(BatchPlacementError::ProjectedCountOverflow)?;
        if intent.inline_value_width > 0 {
            leaf_entry.payload_widths.insert(intent.inline_value_width);
            leaf_entry.pending_payload_intents = leaf_entry
                .pending_payload_intents
                .checked_add(1)
                .ok_or(BatchPlacementError::ProjectedCountOverflow)?;
        }
    }

    // Add target resident counts from each bucket group to its leaf summary exactly once.
    for group in groups.values() {
        let leaf_key = OrientationLeafKey {
            orientation: group.key.orientation,
            leaf_segment: group.key.leaf_segment,
        };
        let leaf_entry = leaf_summaries
            .get_mut(&leaf_key)
            .expect("leaf summary created while processing intents");
        leaf_entry.target_resident_slab_edge_slots = leaf_entry
            .target_resident_slab_edge_slots
            .checked_add(u64::from(group.resident_slab_edge_slots))
            .ok_or(BatchPlacementError::ProjectedCountOverflow)?;
        leaf_entry.target_resident_log_edge_slots = leaf_entry
            .target_resident_log_edge_slots
            .checked_add(u64::from(group.resident_log_edge_slots))
            .ok_or(BatchPlacementError::ProjectedCountOverflow)?;
        leaf_entry.target_resident_slab_payload_slots = leaf_entry
            .target_resident_slab_payload_slots
            .checked_add(u64::from(group.resident_slab_payload_slots))
            .ok_or(BatchPlacementError::ProjectedCountOverflow)?;
        leaf_entry.target_resident_log_payload_slots = leaf_entry
            .target_resident_log_payload_slots
            .checked_add(u64::from(group.resident_log_payload_slots))
            .ok_or(BatchPlacementError::ProjectedCountOverflow)?;
    }

    // Add full-leaf resident counts from LARA. This includes untouched buckets that
    // share the same PMA leaf and must be part of projected geometry.
    for leaf_key in leaf_summaries.keys().copied().collect::<Vec<_>>() {
        let stats = read_leaf_placement_stats(leaf_key)?;
        let leaf_entry = leaf_summaries
            .get_mut(&leaf_key)
            .expect("leaf summary present");
        leaf_entry.full_leaf_resident_slab_edge_slots = stats.total_stored_edge_slots;
        leaf_entry.full_leaf_resident_log_edge_slots = stats.total_edge_overflow_log_slots;
        leaf_entry.full_leaf_resident_slab_payload_slots = stats.total_inline_value_slab_slots;
        leaf_entry.full_leaf_resident_log_payload_slots = stats.total_payload_overflow_log_slots;
        leaf_entry.payload_widths.extend(stats.payload_widths);
    }

    Ok((groups, leaf_summaries))
}

fn read_leaf_placement_stats(
    key: OrientationLeafKey,
) -> Result<LeafBucketPlacementStats, BatchPlacementError> {
    GRAPH.with_borrow(|graph| {
        let result = match key.orientation {
            LabeledOrientation::Forward => {
                graph.read_forward_leaf_placement_stats(key.leaf_segment)
            }
            LabeledOrientation::Reverse => {
                graph.read_reverse_leaf_placement_stats(key.leaf_segment)
            }
        };
        result.map_err(|err| {
            BatchPlacementError::PlacementReadFailed(format!(
                "leaf placement stats for {:?} leaf {}: {err}",
                key.orientation, key.leaf_segment
            ))
        })
    })
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
        DERIVED_INDEX_OUTBOX, EDGE_ALIASES, GRAPH_MUTATION_JOURNAL, LABEL_STATS_DELTA_LOG,
        PENDING_VERTEX_PURGES, UNIQUE_EFFECT_OUTBOX, memory::stable_memory_stats,
    };
    use super::*;
    use crate::test_labels::{edge_label_id_for_name, install_test_edge_inline_value_profile};
    use gleaph_graph_kernel::entry::{Edge, EdgeInlineValueEncoding, EdgeInlineValueProfile};
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
            inline_value_bytes: bytes,
        }
    }

    #[test]
    fn directed_edge_expands_to_forward_and_reverse_intents() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchDirected");
        let edges = vec![input(v[0], v[1], Some(label), true, vec![])];
        let summary = store.plan_batch_edge_insertion(&edges).expect("plan");
        assert_eq!(summary.logical_edge_count, 1);
        assert_eq!(summary.physical_intent_count, 2);
        assert_eq!(summary.groups.len(), 2);

        let forward = summary
            .groups
            .values()
            .find(|g| g.key.orientation == LabeledOrientation::Forward)
            .expect("forward group");
        assert_eq!(forward.key.owner_vertex_id, v[0]);
        assert_eq!(forward.pending_edge_intents, 1);

        let reverse = summary
            .groups
            .values()
            .find(|g| g.key.orientation == LabeledOrientation::Reverse)
            .expect("reverse group");
        assert_eq!(reverse.key.owner_vertex_id, v[1]);
        assert_eq!(reverse.pending_edge_intents, 1);
    }

    #[test]
    fn undirected_edge_expands_to_two_forward_intents() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchUndirected");
        let edges = vec![input(v[0], v[1], Some(label), false, vec![])];
        let summary = store.plan_batch_edge_insertion(&edges).expect("plan");
        assert_eq!(summary.physical_intent_count, 2);

        let owners: Vec<_> = summary
            .groups
            .values()
            .map(|g| g.key.owner_vertex_id)
            .collect();
        assert!(owners.contains(&v[0]));
        assert!(owners.contains(&v[1]));
    }

    #[test]
    fn undirected_self_loop_produces_single_forward_intent() {
        let store = fresh_store();
        let v = make_vertices(&store, 1);
        let label = edge_label_id_for_name("BatchSelfLoop");
        let edges = vec![input(v[0], v[0], Some(label), false, vec![])];
        let summary = store.plan_batch_edge_insertion(&edges).expect("plan");
        assert_eq!(summary.physical_intent_count, 1);
        assert_eq!(summary.groups.len(), 1);
    }

    #[test]
    fn parallel_edges_share_owner_and_label_but_different_neighbor() {
        let store = fresh_store();
        let v = make_vertices(&store, 3);
        let label = edge_label_id_for_name("BatchParallel");
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![]),
            input(v[0], v[2], Some(label), true, vec![]),
        ];
        let summary = store.plan_batch_edge_insertion(&edges).expect("plan");
        let forward_groups: Vec<_> = summary
            .groups
            .values()
            .filter(|g| g.key.orientation == LabeledOrientation::Forward)
            .collect();
        assert_eq!(forward_groups.len(), 1);
        assert_eq!(forward_groups[0].pending_edge_intents, 2);
    }

    #[test]
    fn duplicate_logical_edge_target_is_rejected() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchDuplicate");
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![]),
            input(v[0], v[1], Some(label), true, vec![]),
        ];
        let err = store
            .plan_batch_edge_insertion(&edges)
            .expect_err("duplicate");
        assert!(matches!(err, BatchPlacementError::DuplicateEdgeTarget));
    }

    #[test]
    fn undirected_edge_endpoints_are_canonicalized_for_duplicate_detection() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchUndirectedCanonical");
        let edges = vec![
            input(v[0], v[1], Some(label), false, vec![]),
            input(v[1], v[0], Some(label), false, vec![]),
        ];
        let err = store
            .plan_batch_edge_insertion(&edges)
            .expect_err("duplicate");
        assert!(matches!(err, BatchPlacementError::DuplicateEdgeTarget));
    }

    #[test]
    fn conflicting_inline_value_for_same_target_is_rejected() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchConflictPayload");
        install_test_edge_inline_value_profile(
            label,
            EdgeInlineValueProfile {
                byte_width: 2,
                encoding: EdgeInlineValueEncoding::RawU16,
            },
        );
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![1, 0]),
            input(v[0], v[1], Some(label), true, vec![2, 0]),
        ];
        let err = store
            .plan_batch_edge_insertion(&edges)
            .expect_err("conflict");
        assert!(matches!(
            err,
            BatchPlacementError::ConflictingDuplicateEdgeTarget
        ));
    }

    #[test]
    fn identical_inline_value_for_same_target_is_still_duplicate() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchIdenticalPayload");
        install_test_edge_inline_value_profile(
            label,
            EdgeInlineValueProfile {
                byte_width: 2,
                encoding: EdgeInlineValueEncoding::RawU16,
            },
        );
        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![1, 0]),
            input(v[0], v[1], Some(label), true, vec![1, 0]),
        ];
        let err = store
            .plan_batch_edge_insertion(&edges)
            .expect_err("duplicate");
        assert!(matches!(err, BatchPlacementError::DuplicateEdgeTarget));
    }

    #[test]
    fn missing_vertex_is_rejected_before_any_intent_is_emitted() {
        let store = fresh_store();
        let v0 = store.insert_vertex().expect("vertex");
        let missing = VertexId::from(999);
        let label = edge_label_id_for_name("BatchMissingVertex");
        let edges = vec![input(v0, missing, Some(label), true, vec![])];
        let err = store
            .plan_batch_edge_insertion(&edges)
            .expect_err("missing");
        assert!(matches!(err, BatchPlacementError::VertexNotLive(_)));
    }

    #[test]
    fn inline_value_width_must_match_label_profile() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchWidth");
        install_test_edge_inline_value_profile(
            label,
            EdgeInlineValueProfile {
                byte_width: 2,
                encoding: EdgeInlineValueEncoding::RawU16,
            },
        );
        let edges = vec![input(v[0], v[1], Some(label), true, vec![1])];
        let err = store.plan_batch_edge_insertion(&edges).expect_err("width");
        assert!(matches!(
            err,
            BatchPlacementError::InlineValueWidthMismatch {
                expected: 2,
                actual: 1,
                ..
            }
        ));
    }

    #[test]
    fn existing_bucket_occupancy_is_included_in_summary() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let label = edge_label_id_for_name("BatchExisting");
        store
            .insert_directed_edge(v[0], v[1], Some(label))
            .expect("seed edge");

        // Use a distinct target so planning succeeds; the source bucket still exists.
        let v2 = store.insert_vertex().expect("v2");
        let edges = vec![input(v[0], v2, Some(label), true, vec![])];
        let summary = store.plan_batch_edge_insertion(&edges).expect("plan");
        let forward = summary
            .groups
            .values()
            .find(|g| g.key.orientation == LabeledOrientation::Forward)
            .expect("forward group");
        assert!(forward.resident_slab_edge_slots >= 1);
        assert_eq!(forward.pending_edge_intents, 1);

        let reverse = summary
            .groups
            .values()
            .find(|g| g.key.orientation == LabeledOrientation::Reverse)
            .expect("reverse group");
        assert_eq!(reverse.key.owner_vertex_id, v2);
        assert_eq!(reverse.pending_edge_intents, 1);
    }

    #[test]
    fn leaf_summary_aggregates_multiple_buckets_on_same_leaf() {
        let store = fresh_store();
        let v = make_vertices(&store, 8);
        let label_a = edge_label_id_for_name("BatchLeafA");
        let label_b = edge_label_id_for_name("BatchLeafB");
        // Make sure all vertices fall on leaf 0 with the default segment size.
        let edges = vec![
            input(v[0], v[1], Some(label_a), true, vec![]),
            input(v[0], v[2], Some(label_b), true, vec![]),
            input(v[1], v[2], Some(label_a), true, vec![]),
        ];
        let summary = store.plan_batch_edge_insertion(&edges).expect("plan");

        // The forward orientation should have one leaf summary for leaf 0.
        let forward_leaf = summary
            .leaf_summaries
            .values()
            .find(|l| l.orientation == LabeledOrientation::Forward && l.leaf_segment == 0)
            .expect("forward leaf 0 summary");
        assert_eq!(forward_leaf.pending_edge_intents, 3);

        // Bucket groups should still be separate by owner+label.
        let forward_groups = summary
            .groups
            .values()
            .filter(|g| g.key.orientation == LabeledOrientation::Forward)
            .count();
        assert!(forward_groups >= 3);
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
        edge_alias_index_len: u64,
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
        let edge_alias_index_len = EDGE_ALIASES.with_borrow(|a| a.len());
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
            edge_alias_index_len,
            has_pending_vertex_purges,
        }
    }

    #[test]
    fn planning_leaves_no_canonical_state_change() {
        let store = fresh_store();
        let v = make_vertices(&store, 4);
        let label = edge_label_id_for_name("BatchReadOnly");

        let before = snapshot_graph_state(&store, &v);

        let edges = vec![
            input(v[0], v[1], Some(label), true, vec![]),
            input(v[2], v[3], Some(label), false, vec![]),
        ];
        let _summary = store
            .plan_batch_edge_insertion(&edges)
            .expect("read-only plan");

        let after = snapshot_graph_state(&store, &v);
        assert_eq!(after, before);
    }

    #[test]
    fn default_label_edge_is_rejected_by_planner() {
        let store = fresh_store();
        let v = make_vertices(&store, 2);
        let edges = vec![input(v[0], v[1], None, true, vec![])];
        let err = store
            .plan_batch_edge_insertion(&edges)
            .expect_err("default label");
        assert!(matches!(err, BatchPlacementError::DefaultLabelUnsupported));
    }

    #[test]
    fn projected_minimum_edge_slots_includes_resident_and_pending() {
        let group = BatchPlacementGroup {
            key: BatchPlacementKey {
                orientation: LabeledOrientation::Forward,
                leaf_segment: 0,
                owner_vertex_id: VertexId::from(0),
                storage_label: LaraLabelId::UNLABELED_DIRECTED,
                inline_value_width: 0,
            },
            pending_edge_intents: 3,
            resident_slab_edge_slots: 2,
            resident_log_edge_slots: 1,
            resident_slab_payload_slots: 0,
            resident_log_payload_slots: 0,
        };
        assert_eq!(group.projected_minimum_edge_slots().unwrap(), 6);
    }

    #[test]
    fn projected_minimum_payload_bytes_with_width() {
        let group = BatchPlacementGroup {
            key: BatchPlacementKey {
                orientation: LabeledOrientation::Forward,
                leaf_segment: 0,
                owner_vertex_id: VertexId::from(0),
                storage_label: LaraLabelId::UNLABELED_DIRECTED,
                inline_value_width: 4,
            },
            pending_edge_intents: 2,
            resident_slab_edge_slots: 0,
            resident_log_edge_slots: 0,
            resident_slab_payload_slots: 1,
            resident_log_payload_slots: 1,
        };
        assert_eq!(group.projected_minimum_payload_slots().unwrap(), 4);
        assert_eq!(group.projected_minimum_payload_bytes().unwrap(), 16);
    }

    #[test]
    fn leaf_payload_projection_rejects_mixed_widths() {
        let summary = BatchPlacementLeafSummary {
            orientation: LabeledOrientation::Forward,
            leaf_segment: 0,
            target_resident_slab_edge_slots: 0,
            target_resident_log_edge_slots: 0,
            pending_edge_intents: 2,
            full_leaf_resident_slab_edge_slots: 0,
            full_leaf_resident_log_edge_slots: 0,
            target_resident_slab_payload_slots: 0,
            target_resident_log_payload_slots: 0,
            pending_payload_intents: 2,
            full_leaf_resident_slab_payload_slots: 1,
            full_leaf_resident_log_payload_slots: 1,
            payload_widths: [1u16, 8u16].into_iter().collect(),
        };
        assert_eq!(
            summary.projected_minimum_payload_slots(),
            Err(BatchPlacementError::PayloadWidthMixed)
        );
    }

    #[test]
    fn leaf_summary_counts_untargeted_existing_buckets_on_same_leaf() {
        let store = fresh_store();
        let v = make_vertices(&store, 3);
        let label_existing = edge_label_id_for_name("BatchUntouchedExisting");
        let label_new = edge_label_id_for_name("BatchUntouchedNew");

        // Create an existing bucket that the pending batch will NOT target.
        store
            .insert_directed_edge(v[1], v[2], Some(label_existing))
            .expect("seed existing edge");

        // Plan a new edge on the same leaf (default segment size puts v[0..15] on leaf 0).
        let edges = vec![input(v[0], v[1], Some(label_new), true, vec![])];
        let summary = store.plan_batch_edge_insertion(&edges).expect("plan");

        let forward_leaf = summary
            .leaf_summaries
            .values()
            .find(|l| l.orientation == LabeledOrientation::Forward && l.leaf_segment == 0)
            .expect("forward leaf 0 summary");

        // The targeted bucket is new, so target resident counts are zero.
        assert_eq!(forward_leaf.target_resident_slab_edge_slots, 0);
        assert_eq!(forward_leaf.target_resident_log_edge_slots, 0);
        // Full-leaf view must still include the untouched existing v[1] bucket.
        assert!(
            forward_leaf.full_leaf_resident_slab_edge_slots > 0,
            "full_leaf_resident_slab_edge_slots should count untouched existing bucket"
        );

        let reverse_leaf = summary
            .leaf_summaries
            .values()
            .find(|l| l.orientation == LabeledOrientation::Reverse && l.leaf_segment == 0)
            .expect("reverse leaf 0 summary");
        // Pending reverse intent is new; targeted reverse bucket has no resident rows.
        assert_eq!(reverse_leaf.target_resident_slab_edge_slots, 0);
        assert_eq!(reverse_leaf.target_resident_log_edge_slots, 0);
        // The untouched reverse bucket for v[2] must appear in the full-leaf view.
        assert!(
            reverse_leaf.full_leaf_resident_slab_edge_slots > 0,
            "reverse full_leaf_resident_slab_edge_slots should count untouched existing bucket"
        );

        // Pending intents are still reported.
        assert_eq!(forward_leaf.pending_edge_intents, 1);
        assert_eq!(reverse_leaf.pending_edge_intents, 1);
    }

    #[test]
    fn leaf_summary_counts_existing_default_bypass_edge_occupancy() {
        let store = fresh_store();
        let v = make_vertices(&store, 3);
        let label = edge_label_id_for_name("BatchBypassOccupancy");

        // The unlabeled edge uses the default-label bypass, but still occupies
        // the same edge PMA leaf that the labeled batch must project.
        store
            .insert_directed_edge(v[1], v[2], None)
            .expect("seed default-label edge");

        let summary = store
            .plan_batch_edge_insertion(&[input(v[0], v[1], Some(label), true, vec![])])
            .expect("plan");
        let forward_leaf = summary
            .leaf_summaries
            .values()
            .find(|leaf| leaf.orientation == LabeledOrientation::Forward && leaf.leaf_segment == 0)
            .expect("forward leaf 0 summary");

        assert!(
            forward_leaf.full_leaf_resident_slab_edge_slots > 0,
            "full leaf edge occupancy must include default-label bypass rows"
        );
        assert_eq!(forward_leaf.full_leaf_resident_slab_payload_slots, 0);
    }
}
