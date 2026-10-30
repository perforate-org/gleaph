//! Failure-atomic one-orientation batch writes for [`LabeledLaraGraph`].
//!
//! This module implements the smallest LARA-internal batch mutation slice from
//! ADR 0045: one orientation, one bounded batch, and an explicit
//! `plan -> reserve -> commit` boundary.  It reuses the existing LARA ownership
//! and placement types and does not add a public or Candid API.
//!
//! The reserve/commit design is intentionally split:
//!
//! 1. `reserve_one_orientation_batch` performs all fallible validation and
//!    backing-capacity reservation before any canonical write.  On success it
//!    returns a `BatchReservation` token that records the preflight snapshot
//!    and required destination geometry.  If reserve fails part-way through
//!    capacity reservation, it restores the logical edge-store capacity and the
//!    inline property bytes occupied tail to their pre-reserve values. Consumed
//!    free-list prefixes are restored; appended bytes return to the implicit tail,
//!    not also to the free list. Backing pages are not shrunk. Canonical
//!    adjacency and bucket metadata are never modified.
//! 2. `BatchReservation::commit` performs the actual stable-memory mutations.
//!    Because all fallible checks happened in reserve, a recoverable error
//!    cannot occur after the first canonical byte write.  A panic here is an
//!    invariant violation.  In an ICP canister message the trap rolls back the
//!    entire message, so no partial canonical state is published at that
//!    boundary; direct library callers without such a transaction boundary do
//!    not receive the same atomicity guarantee.
//! 3. `BatchReservation::rollback` consumes the token and reverts the same
//!    logical capacity and occupied-tail changes without publishing canonical
//!    state.  Because the token is consumed, a reservation cannot be rolled
//!    back twice.
//!
//! Empty plans are rejected by `reserve_one_orientation_batch`; the batch
//! boundary does not define a no-op success path.
//!
//! Current admission supports slab append, unordered slab-hole fill and tree-tail
//! append. A run requiring spill is deferred before reservation (ADR 0097 §7);
//! expanded/relocated commits have no admission path. Named bucket preparation
//! belongs to this boundary; tiny runs fall back to scalar insertion.

use crate::{
    VertexId,
    labeled::{bucket_label_key::BucketLabelKey, slot_index::checked_add_slot_index},
    traits::{CsrEdge, CsrEdgeTombstone},
};
use ic_stable_structures::Memory;

use super::compact::LabeledLeafRelocationTarget;
use super::error::LabeledOperationError;
use super::insert::EdgePlacementPolicy;
use super::{BucketMode, BucketSearch, LabeledLaraGraph};
use crate::lara::edge::free_span::FreeSpan;

/// One physical half-edge inside a one-orientation batch plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneOrientationBatchEdge<E>
where
    E: CsrEdge,
{
    /// Stable logical ordinal from the caller's chunk; joins edge and inline property bytes.
    pub logical_ordinal: u32,
    /// Vertex that owns the CSR row.
    pub owner_vertex_id: VertexId,
    /// Neighbor vertex referenced by this half-edge.
    pub neighbor_vertex_id: VertexId,
    /// Storage label, including directedness bit.
    pub label_id: BucketLabelKey,
    /// Encoded edge bytes, including the inline property bytes if any.
    pub edge: E,
}

/// One bucket-local run inside a one-orientation batch plan.
///
/// All edges in the run share the same owner vertex, storage label, and inline
/// value width, so they can be written in one contiguous slab batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneOrientationBucketRun<E>
where
    E: CsrEdge,
{
    /// Vertex that owns the CSR row.
    pub owner_vertex_id: VertexId,
    /// Storage label, including directedness bit.
    pub label_id: BucketLabelKey,
    /// Physical byte width per edge inline property slot (`0` = no inline_property_bytes).
    pub inline_property_width: u16,
    /// Storage placement policy for this bucket run (ADR 0052 §5/§6). `Unordered`
    /// is the default; the Graph facade resolves it from the label's ordering policy.
    pub placement: EdgePlacementPolicy,
    /// Edges in this bucket run, in strictly increasing logical ordinal order.
    pub edges: Vec<OneOrientationBatchEdge<E>>,
}

/// One-orientation batch plan consumed by [`LabeledLaraGraph`].
///
/// The plan is an ephemeral heap value.  It carries only the edges for a single
/// orientation; the caller (GraphStore) is responsible for orchestrating the
/// forward/reverse or undirected alias pairs at a higher layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneOrientationBatchPlan<E>
where
    E: CsrEdge,
{
    /// Bucket-local runs that together form the one-orientation batch.
    pub runs: Vec<OneOrientationBucketRun<E>>,
}

/// Request-local logical pairing metadata for an undirected batch.
///
/// The table is owned by the Graph caller and is validated together with the
/// merged physical plan. It is not persisted and does not participate in the
/// physical reservation key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndirectedBatchPair {
    /// Stable logical input ordinal.
    pub logical_ordinal: u32,
    /// Endpoint owner of the lower side of the undirected pair.
    pub lower_owner_vertex_id: VertexId,
    /// Endpoint owner of the higher side of the undirected pair.
    pub higher_owner_vertex_id: VertexId,
    /// Undirected storage label shared by both projections.
    pub label_id: BucketLabelKey,
    /// Inline property byte width shared by both projections.
    pub inline_property_width: u16,
}

/// Logical shape represented by one request-local pair-table row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchLogicalPairKind {
    /// Directed edge with distinct forward and reverse physical rows.
    Directed,
    /// Non-self undirected edge with two forward physical rows.
    Undirected,
    /// Directed self-loop with distinct forward and reverse physical rows.
    DirectedSelfLoop,
    /// Undirected self-loop with one forward physical row.
    UndirectedSelfLoop,
}

/// Request-local logical pair metadata for a mixed batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchLogicalPair {
    /// Stable logical input ordinal.
    pub logical_ordinal: u32,
    /// Logical edge shape.
    pub kind: BatchLogicalPairKind,
    /// First logical endpoint; source for directed rows and lower endpoint for undirected rows.
    pub endpoint_a: VertexId,
    /// Second logical endpoint; target for directed rows and higher endpoint for undirected rows.
    pub endpoint_b: VertexId,
    /// Storage label shared by all physical projections.
    pub label_id: BucketLabelKey,
    /// Inline property byte width shared by all physical projections.
    pub inline_property_width: u16,
}

/// Logical orientation shape for an owner-coordinated batch.
///
/// A caller cannot accidentally submit an unpaired one-sided directed batch: the only
/// single-orientation form is explicitly named `SelfLoop`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BidirectionalBatchPlan<E>
where
    E: CsrEdge,
{
    /// Directed edge batch with forward and reverse plans.
    Directed {
        /// Forward out-adjacency plan.
        forward: OneOrientationBatchPlan<E>,
        /// Reverse in-adjacency plan.
        reverse: OneOrientationBatchPlan<E>,
    },
    /// Undirected non-self batch with one merged forward plan.
    ///
    /// Both endpoint projections for one logical ordinal live in the same
    /// orientation plan. This is important when two logical edges project
    /// into the same physical bucket: the bucket must be reserved once and
    /// its final run must be validated as one ordered sequence.
    Undirected {
        /// Merged forward plan containing both endpoint projections.
        plan: OneOrientationBatchPlan<E>,
        /// Graph-owned request-local pair membership table.
        pairs: Vec<UndirectedBatchPair>,
    },
    /// Undirected self-loop batch with one forward plan.
    SelfLoop {
        /// Self-loop forward plan.
        forward: OneOrientationBatchPlan<E>,
    },
    /// Mixed-shape batch with merged physical plans and one pair-table row per logical item.
    Mixed {
        /// One merged physical plan per orientation.
        physical: Vec<(
            crate::labeled::LabeledOrientation,
            OneOrientationBatchPlan<E>,
        )>,
        /// Graph-owned request-local logical pair table.
        pairs: Vec<BatchLogicalPair>,
    },
}

impl<E> BidirectionalBatchPlan<E>
where
    E: CsrEdge,
{
    pub(crate) fn validate(&self) -> Result<(), OneOrientationBatchError> {
        fn entries<E: CsrEdge>(
            plan: &OneOrientationBatchPlan<E>,
        ) -> Vec<(u32, VertexId, VertexId, BucketLabelKey, u16)> {
            let mut rows = plan
                .runs
                .iter()
                .flat_map(|run| {
                    run.edges.iter().map(|edge| {
                        (
                            edge.logical_ordinal,
                            edge.owner_vertex_id,
                            edge.neighbor_vertex_id,
                            edge.label_id,
                            run.inline_property_width,
                        )
                    })
                })
                .collect::<Vec<_>>();
            rows.sort_unstable_by_key(|row| row.0);
            rows
        }

        fn validate_pair<E: CsrEdge>(
            left: &OneOrientationBatchPlan<E>,
            right: &OneOrientationBatchPlan<E>,
            directed: bool,
        ) -> Result<(), OneOrientationBatchError> {
            let left = entries(left);
            let right = entries(right);
            if left.is_empty() || left.len() != right.len() {
                return Err(OneOrientationBatchError::InvalidOrientationPair(
                    "orientation cardinality is empty or differs".into(),
                ));
            }
            for (a, b) in left.iter().zip(right.iter()) {
                if a.0 != b.0
                    || a.1 != b.2
                    || a.2 != b.1
                    || a.3 != b.3
                    || a.4 != b.4
                    || a.3.is_directed() != directed
                {
                    return Err(OneOrientationBatchError::InvalidOrientationPair(
                        "orientation rows are not exact reversed pairs".into(),
                    ));
                }
            }
            Ok(())
        }

        fn validate_merged_undirected<E: CsrEdge>(
            plan: &OneOrientationBatchPlan<E>,
            pairs: &[UndirectedBatchPair],
        ) -> Result<(), OneOrientationBatchError> {
            let rows = entries(plan);
            if rows.is_empty() || !rows.len().is_multiple_of(2) || pairs.is_empty() {
                return Err(OneOrientationBatchError::InvalidOrientationPair(
                    "merged undirected plan has incomplete logical pairs".into(),
                ));
            }
            let mut by_ordinal = std::collections::BTreeMap::<
                u32,
                Vec<(u32, VertexId, VertexId, BucketLabelKey, u16)>,
            >::new();
            for row in &rows {
                by_ordinal.entry(row.0).or_default().push(*row);
            }
            let mut pair_ordinals = std::collections::BTreeSet::new();
            for pair in pairs {
                if !pair_ordinals.insert(pair.logical_ordinal)
                    || pair.label_id.is_directed()
                    || pair.lower_owner_vertex_id >= pair.higher_owner_vertex_id
                {
                    return Err(OneOrientationBatchError::InvalidOrientationPair(
                        "undirected pair table contains duplicate or invalid metadata".into(),
                    ));
                }
                let Some(rows) = by_ordinal.get(&pair.logical_ordinal) else {
                    return Err(OneOrientationBatchError::InvalidOrientationPair(
                        "undirected pair table is missing a physical projection".into(),
                    ));
                };
                if rows.len() != 2 {
                    return Err(OneOrientationBatchError::InvalidOrientationPair(
                        "merged undirected plan does not contain exactly two projections".into(),
                    ));
                }
                let [left, right] = rows.as_slice() else {
                    unreachable!("pair length checked above")
                };
                if left.1 != right.2
                    || left.2 != right.1
                    || left.3 != pair.label_id
                    || right.3 != pair.label_id
                    || left.4 != pair.inline_property_width
                    || right.4 != pair.inline_property_width
                    || !((left.1 == pair.lower_owner_vertex_id
                        && right.1 == pair.higher_owner_vertex_id)
                        || (left.1 == pair.higher_owner_vertex_id
                            && right.1 == pair.lower_owner_vertex_id))
                {
                    return Err(OneOrientationBatchError::InvalidOrientationPair(
                        "merged undirected rows are not exact reversed pairs".into(),
                    ));
                }
            }
            if pair_ordinals.len() * 2 != rows.len() {
                return Err(OneOrientationBatchError::InvalidOrientationPair(
                    "undirected pair table does not cover every physical projection".into(),
                ));
            }
            Ok(())
        }

        fn validate_mixed<E: CsrEdge>(
            physical: &[(
                crate::labeled::LabeledOrientation,
                OneOrientationBatchPlan<E>,
            )],
            pairs: &[BatchLogicalPair],
        ) -> Result<(), OneOrientationBatchError> {
            if physical.is_empty() || pairs.is_empty() {
                return Err(OneOrientationBatchError::InvalidOrientationPair(
                    "mixed batch has no physical plans or logical pairs".into(),
                ));
            }
            let mut rows_by_ordinal = std::collections::BTreeMap::<
                u32,
                Vec<(
                    crate::labeled::LabeledOrientation,
                    (u32, VertexId, VertexId, BucketLabelKey, u16),
                )>,
            >::new();
            for (orientation, plan) in physical {
                for row in entries(plan) {
                    rows_by_ordinal
                        .entry(row.0)
                        .or_default()
                        .push((*orientation, row));
                }
            }
            let mut ordinals = std::collections::BTreeSet::new();
            for pair in pairs {
                if !ordinals.insert(pair.logical_ordinal) {
                    return Err(OneOrientationBatchError::InvalidOrientationPair(
                        "mixed batch pair table contains duplicate ordinals".into(),
                    ));
                }
                let Some(rows) = rows_by_ordinal.get(&pair.logical_ordinal) else {
                    return Err(OneOrientationBatchError::InvalidOrientationPair(
                        "mixed batch pair table is missing physical rows".into(),
                    ));
                };
                let expected = match pair.kind {
                    BatchLogicalPairKind::UndirectedSelfLoop => 1,
                    _ => 2,
                };
                if rows.len() != expected {
                    return Err(OneOrientationBatchError::InvalidOrientationPair(
                        "mixed batch physical projection count does not match pair kind".into(),
                    ));
                }
                let label_is_directed = pair.label_id.is_directed();
                let expects_directed = matches!(
                    pair.kind,
                    BatchLogicalPairKind::Directed | BatchLogicalPairKind::DirectedSelfLoop
                );
                if label_is_directed != expects_directed {
                    return Err(OneOrientationBatchError::InvalidOrientationPair(
                        "mixed batch pair kind disagrees with storage label".into(),
                    ));
                }
                for (_, row) in rows {
                    if row.3 != pair.label_id || row.4 != pair.inline_property_width {
                        return Err(OneOrientationBatchError::InvalidOrientationPair(
                            "mixed batch physical metadata disagrees with pair table".into(),
                        ));
                    }
                }
                match pair.kind {
                    BatchLogicalPairKind::Directed => {
                        if !rows.iter().any(|(o, r)| {
                            *o == crate::labeled::LabeledOrientation::Forward
                                && r.1 == pair.endpoint_a
                                && r.2 == pair.endpoint_b
                        }) || !rows.iter().any(|(o, r)| {
                            *o == crate::labeled::LabeledOrientation::Reverse
                                && r.1 == pair.endpoint_b
                                && r.2 == pair.endpoint_a
                        }) {
                            return Err(OneOrientationBatchError::InvalidOrientationPair(
                                "mixed directed pair has incorrect forward/reverse rows".into(),
                            ));
                        }
                    }
                    BatchLogicalPairKind::DirectedSelfLoop => {
                        if !rows.iter().any(|(o, r)| {
                            *o == crate::labeled::LabeledOrientation::Forward
                                && r.1 == pair.endpoint_a
                                && r.2 == pair.endpoint_a
                        }) || !rows.iter().any(|(o, r)| {
                            *o == crate::labeled::LabeledOrientation::Reverse
                                && r.1 == pair.endpoint_a
                                && r.2 == pair.endpoint_a
                        }) {
                            return Err(OneOrientationBatchError::InvalidOrientationPair(
                                "mixed directed self-loop has incorrect orientation rows".into(),
                            ));
                        }
                    }
                    BatchLogicalPairKind::Undirected => {
                        if !rows
                            .iter()
                            .all(|(o, _)| *o == crate::labeled::LabeledOrientation::Forward)
                            || !rows
                                .iter()
                                .any(|(_, r)| r.1 == pair.endpoint_a && r.2 == pair.endpoint_b)
                            || !rows
                                .iter()
                                .any(|(_, r)| r.1 == pair.endpoint_b && r.2 == pair.endpoint_a)
                        {
                            return Err(OneOrientationBatchError::InvalidOrientationPair(
                                "mixed undirected pair has incorrect endpoint rows".into(),
                            ));
                        }
                    }
                    BatchLogicalPairKind::UndirectedSelfLoop => {
                        let (orientation, row) = rows[0];
                        if orientation != crate::labeled::LabeledOrientation::Forward
                            || row.1 != pair.endpoint_a
                            || row.2 != pair.endpoint_a
                        {
                            return Err(OneOrientationBatchError::InvalidOrientationPair(
                                "mixed undirected self-loop has incorrect row".into(),
                            ));
                        }
                    }
                }
            }
            if ordinals.len() != rows_by_ordinal.len() {
                return Err(OneOrientationBatchError::InvalidOrientationPair(
                    "mixed batch pair table does not cover every physical ordinal".into(),
                ));
            }
            Ok(())
        }

        match self {
            Self::Directed { forward, reverse } => validate_pair(forward, reverse, true),
            Self::Undirected { plan, pairs } => validate_merged_undirected(plan, pairs),
            Self::SelfLoop { forward } => {
                let rows = entries(forward);
                if !rows.is_empty()
                    && rows.iter().all(|(_, owner, neighbor, label, _)| {
                        owner == neighbor && label.is_undirected()
                    })
                {
                    Ok(())
                } else {
                    Err(OneOrientationBatchError::InvalidOrientationPair(
                        "self-loop plan contains a non-self or directed edge".into(),
                    ))
                }
            }
            Self::Mixed { physical, pairs } => validate_mixed(physical, pairs),
        }
    }

    pub(crate) fn into_orientations(
        self,
    ) -> Vec<(
        crate::labeled::LabeledOrientation,
        OneOrientationBatchPlan<E>,
    )> {
        match self {
            Self::Directed { forward, reverse } => vec![
                (crate::labeled::LabeledOrientation::Forward, forward),
                (crate::labeled::LabeledOrientation::Reverse, reverse),
            ],
            Self::Undirected { plan, .. } => {
                vec![(crate::labeled::LabeledOrientation::Forward, plan)]
            }
            Self::SelfLoop { forward } => {
                vec![(crate::labeled::LabeledOrientation::Forward, forward)]
            }
            Self::Mixed { physical, .. } => physical,
        }
    }
}

/// Exact physical location of one edge written by a one-orientation batch.
///
/// This is an internal fact owned by LARA.  Overflow-log entries retain their
/// leaf and log index because they do not have a slab slot until maintenance
/// folds them into a slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OneOrientationPhysicalLocation {
    /// Edge slab slot and optional inline property bytes byte offset.
    Slab {
        /// Edge slab slot.
        edge_slot: u64,
        /// Inline property bytes byte offset, when the edge carries inline property bytes.
        inline_property_bytes_offset: Option<u64>,
    },
    /// Edge overflow-log entry and optional inline-property-bytes-log entry.
    OverflowLog {
        /// PMA leaf containing the log entry.
        leaf: u32,
        /// Edge overflow-log entry index.
        edge_entry_idx: u32,
        /// InlinePropertyBytes overflow-log entry index, when present.
        inline_property_bytes_entry_idx: Option<u32>,
    },
    /// Plan 0321 Step 2: tree-mode batch admission. The edge lives
    /// in the LTB tail block at `tail_block_id` (4-byte target
    /// encoding) at byte offset `byte_offset` from the start of
    /// the tail block. There is no inline property bytes slot
    /// (tree mode rejects inline-property-bearing buckets).
    Tree {
        /// LTB block id of the tail block.
        tail_block_id: u32,
        /// Byte offset from the start of the tail block.
        byte_offset: usize,
    },
}

/// Exact location keyed by the logical ordinal supplied in the batch plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OneOrientationBatchLocation {
    /// Logical ordinal from the input batch.
    pub logical_ordinal: u32,
    /// CSR row owner used to distinguish two same-orientation undirected halves.
    pub owner_vertex_id: VertexId,
    /// Logical slot within the owning bucket row, including tombstone positions.
    pub logical_slot: u32,
    /// Exact LARA physical location.
    pub location: OneOrientationPhysicalLocation,
}

/// Opaque reservation produced by [`LabeledLaraGraph::reserve_one_orientation_batch`].
///
/// The fields are deliberately private; the valid uses are [`BatchReservation::commit`]
/// and [`BatchReservation::rollback`].  The token embeds a snapshot of the bucket
/// identity and expected occupancy at reservation time so commit can detect
/// tampering or stale state, plus the pre-reserve edge/inline property bytes allocator state so
/// rollback can restore it.
pub struct BatchReservation<E>
where
    E: CsrEdge,
{
    plan: OneOrientationBatchPlan<E>,
    runs: Vec<BatchReservationRun>,
    graph_marker: usize,
    edge_capacity_before: u64,
    inline_property_bytes_tail_before: u64,
    /// Named bucket descriptors created during reservation preparation.
    new_buckets: Vec<(VertexId, BucketLabelKey)>,
    /// Cumulative in-place leaf expansions reserved for this batch; consumed on
    /// rollback to restore the free-span store shape.
    leaf_expansions: std::collections::BTreeMap<u32, LeafExpansionState>,
    /// Leaf relocation targets admitted for this reservation. Each leaf is
    /// consumed once during commit before any pending row is written.
    leaf_relocations: std::collections::BTreeMap<u32, LabeledLeafRelocationTarget>,
    /// Inline-property free-span prefixes consumed while reserving replacement spans.
    /// Restored if the reservation is rolled back before commit.
    inline_property_bytes_free_spans: Vec<FreeSpan>,
}

impl<E> BatchReservation<E>
where
    E: CsrEdge,
{
    /// Returns the canonical vertex rows whose PMA leaves are touched by this reservation.
    ///
    /// The bidirectional owner uses this before the first canonical write to hide any derived
    /// mate rows that could become stale.  The reservation remains opaque to callers; only the
    /// owner can inspect this impact set.
    pub(crate) fn affected_owner_vertices(&self) -> Vec<VertexId> {
        let mut vertices = self
            .plan
            .runs
            .iter()
            .map(|run| run.owner_vertex_id)
            .collect::<Vec<_>>();
        vertices.sort_unstable();
        vertices.dedup();
        vertices
    }

    /// Whether this reservation uses the pending-aware one-shot leaf expansion
    /// path rather than a direct slab or overflow-log destination.
    pub fn uses_expansion(&self) -> bool {
        self.runs.iter().any(|run| {
            matches!(
                &run.destination,
                RunDestination::ExpandedSlab { .. } | RunDestination::RelocatedSlab { .. }
            )
        })
    }
}

impl<E> std::fmt::Debug for BatchReservation<E>
where
    E: CsrEdge,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchReservation")
            .field("run_count", &self.runs.len())
            .finish_non_exhaustive()
    }
}

/// Per-run canonical destination captured at reservation time.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BatchReservationRun {
    /// Bucket slot in the label-bucket store that owns this run.
    bucket_slot: u64,
    /// Expected bucket fingerprint at reservation time.
    bucket_fingerprint: BucketFingerprint,
    /// Number of edge slots reserved for the run.
    edge_slot_count: u32,
    /// Byte width per edge inline property slot (`0` = no inline_property_bytes).
    inline_property_width: u16,
    /// Physical destination chosen at reservation time.
    destination: RunDestination,
}

/// Snapshot of bucket identity and occupancy used to detect stale/tampered reservations.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BucketFingerprint {
    owner_vertex_id: VertexId,
    label_id: BucketLabelKey,
    stored_slots: u32,
    degree: u32,
    inline_property_bytes_slab_slots: u32,
    inline_property_bytes_offset: u64,
    inline_property_byte_width: u16,
    /// Plan 0321 §F-2 (review rework, 2026-09-02): tree-mode flag
    /// is part of the fingerprint so a slab→tree flip (or vice
    /// versa) between reserve and commit is detected and rejected
    /// rather than silently corrupting the bucket.
    is_tree_mode: bool,
    vertex_stored_slots: u32,
}

impl BucketFingerprint {
    fn from_bucket(
        owner_vertex_id: VertexId,
        label_id: BucketLabelKey,
        bucket: &super::LabelBucket,
        vertex: &super::LabeledVertex,
    ) -> Self {
        Self {
            owner_vertex_id,
            label_id,
            stored_slots: bucket.stored_slots(),
            degree: bucket.degree,
            inline_property_bytes_slab_slots: bucket.inline_property_bytes_slab_slots(),
            inline_property_bytes_offset: bucket.inline_property_bytes_offset(),
            inline_property_byte_width: bucket.inline_property_byte_width(),
            is_tree_mode: bucket.is_tree_mode(),
            vertex_stored_slots: vertex.stored_slots,
        }
    }
}

/// Error returned when a one-orientation batch write cannot complete.
#[derive(Debug)]
pub enum OneOrientationBatchError {
    /// The batch plan has no runs.
    EmptyPlan,
    /// A bucket run has no edges.
    EmptyRun,
    /// A bucket run is not strictly ordered by logical ordinal.
    NonIncreasingLogicalOrdinals,
    /// The same bucket appears in more than one run.
    DuplicateBucketRun,
    /// An expanded-slab path requires a pinned PMA leaf block.
    MissingPinnedLeafForExpansion,
    /// An inline property bytes span can only be moved with leaf relocation.
    InlinePropertyBytesSpanRequiresRelocation,
    /// A referenced vertex does not exist or is not live.
    VertexNotLive(VertexId),
    /// A referenced bucket was not found and could not be created.
    BucketNotFound {
        /// Vertex that owns the missing bucket.
        owner_vertex_id: VertexId,
        /// Storage label of the missing bucket.
        label_id: BucketLabelKey,
    },
    /// Inline value width does not match the existing bucket schema.
    InlinePropertyBytesWidthMismatch {
        /// InlinePropertyBytes byte width declared by the label bucket.
        bucket_width: u16,
        /// InlinePropertyBytes byte width carried by the edge.
        edge_width: u16,
    },
    /// The slab window cannot hold the planned run.
    SlabCapacityExceeded,
    /// Plan 0321: a batch run targets a tree-mode bucket. The
    /// batch planner currently has no tree-aware geometry; this
    /// failure closes the corruption hole at the batch boundary
    /// (the run is rejected and the caller falls back to scalar
    /// inserts which handle tree append correctly). The widening
    /// slice (Commit 2 of Plan 0321) is the typed follow-up that
    /// ADMITs tree runs.
    TreeModeBucketRunUnsupported {
        /// Vertex that owns the tree-mode bucket.
        owner_vertex_id: VertexId,
        /// Storage label of the tree-mode bucket.
        label_id: BucketLabelKey,
    },
    /// ADR 0096 §5: tiny-mode bucket runs stay on the scalar path (which
    /// promotes naturally via the insert dispatcher). Rejected before any run
    /// math: width bytes are payload on tiny and must never feed planning.
    TinyBucketRunUnsupported {
        /// Vertex that owns the tiny-mode bucket.
        owner_vertex_id: VertexId,
        /// Storage label of the tiny-mode bucket.
        label_id: BucketLabelKey,
    },
    /// ADR 0097 §7: a run whose rows would land in the bucket's own spill run
    /// stays on the scalar path, which appends to the run, grows it by class and
    /// promotes the bucket when the run reaches its largest class. Rejected
    /// before any reservation, so the batch writes nothing and the caller's
    /// scalar fallback is exact.
    SpillRunDeferred {
        /// Vertex that owns the bucket.
        owner_vertex_id: VertexId,
        /// Storage label of the bucket.
        label_id: BucketLabelKey,
    },
    /// Plan 0321 Step 2 (minimal first slice): the tree-mode
    /// run exceeds the current tail-block room and would cross
    /// a block boundary. The widening (follow-up) admits this
    /// via new-block mint + root region growth; the minimal
    /// first slice rejects typed and lets the caller fall back
    /// to scalar inserts.
    TreeRunExceedsTailBlock {
        /// Vertex that owns the tree-mode bucket.
        owner_vertex_id: VertexId,
        /// Storage label of the tree-mode bucket.
        label_id: BucketLabelKey,
        /// Total bytes the run would write.
        run_bytes: u64,
        /// Free bytes in the current tail block.
        tail_room_bytes: u64,
    },
    /// The overflow log cannot hold the planned run.
    LogCapacityExceeded,
    /// A inline property edge carried a byte length different from the declared width.
    InlinePropertyBytesLengthMismatch {
        /// Logical ordinal of the offending edge.
        logical_ordinal: u32,
        /// Declared edge inline property byte width.
        expected_width: u16,
        /// Actual inline property bytes length.
        actual_length: usize,
    },
    /// Existing edge and inline property bytes overflow logs are not aligned by ordinal.
    InlinePropertyBytesLogLengthMismatch {
        /// Number of resident edge overflow-log entries.
        edge_log_len: u32,
        /// Number of resident inline property bytes overflow-log entries.
        inline_property_bytes_log_len: u32,
    },
    /// A stable-memory reservation or write failed.
    StorageError(LabeledOperationError),
    /// Logical forward/reverse or undirected pairing is malformed.
    InvalidOrientationPair(String),
}

/// Edge write strategy for filling in-slab tombstone holes of an unordered bucket.
///
/// Both strategies publish identical canonical state; the choice is a pure cost
/// decision derived from the `lara_slab_*` micro-benches (see
/// [`choose_slab_hole_write_strategy`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlabHoleWriteStrategy {
    /// Write each filled hole with one `write_slot` call (pre-encoded bytes).
    PerHole,
    /// Read the bucket's stored window once, patch the holes, and write the
    /// window back in one contiguous call.
    WindowRewrite,
}

/// Per-`write_slot` call fixed cost, wasm + IC charging layers (canbench
/// `lara_slab_*`, 2026-08-01: ≈240 wasm + ≈30 IC per call).
const SLAB_WRITE_CALL_FIXED: u64 = 270;
/// Byte cost of a slab write, wasm + IC charging layers (≈1 each per byte).
const SLAB_WRITE_BYTE_COST: u64 = 2;
/// Fixed cost of two contiguous calls (one window read + one window write), wasm layer.
const SLAB_WINDOW_CALL_FIXED: u64 = 1200;
/// Cost of patching one hole into the already-read window buffer (no re-encode).
const SLAB_HOLE_PATCH_COST: u64 = 25;

// Test-only override so both write strategies can be exercised deterministically
// without depending on the cost constants staying in a particular range.
#[cfg(test)]
thread_local! {
    static SLAB_HOLE_WRITE_STRATEGY_OVERRIDE: std::cell::Cell<Option<SlabHoleWriteStrategy>> =
        const { std::cell::Cell::new(None) };
}

/// Chooses the cheaper edge write strategy for filling `holes` tombstone slots
/// inside a `window_slots`-wide bucket window of `edge_bytes`-wide rows.
///
/// The whole-window rewrite costs the fixed two-call overhead plus reading and
/// writing the full window (4·W·B across both layers) plus one patch per hole;
/// per-hole writes cost the per-call fixed cost per hole. Window rewrite is
/// chosen only when its modelled cost is below the per-hole cost, which for the
/// production 24-byte row width happens around ≈35% tombstone density and for
/// the 4-byte test width around ≈8% (the break-even density scales with edge
/// width and is roughly window-size independent).
fn choose_slab_hole_write_strategy(
    edge_bytes: usize,
    window_slots: u64,
    holes: u64,
) -> SlabHoleWriteStrategy {
    #[cfg(test)]
    if let Some(forced) = SLAB_HOLE_WRITE_STRATEGY_OVERRIDE.with(|cell| cell.get()) {
        return forced;
    }
    let window_bytes = window_slots.saturating_mul(edge_bytes as u64);
    let window_cost = SLAB_WINDOW_CALL_FIXED
        .saturating_add(window_bytes.saturating_mul(4))
        .saturating_add(SLAB_HOLE_PATCH_COST.saturating_mul(holes));
    let hole_cost = SLAB_WRITE_CALL_FIXED.saturating_mul(holes);
    if window_cost < hole_cost {
        SlabHoleWriteStrategy::WindowRewrite
    } else {
        SlabHoleWriteStrategy::PerHole
    }
}

/// Physical destination chosen for one bucket-local run at preflight time.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RunDestination {
    /// Write into the bucket's contiguous slab window.
    Slab {
        /// First edge slab slot written for the run.
        edge_start_slot: u64,
        /// Base of the complete reserved value span, including the existing prefix.
        inline_property_bytes_offset: Option<u64>,
        /// Number of inline property bytes reserved.
        inline_property_bytes_byte_count: u64,
    },
    /// Fill in-slab tombstone holes of an unordered bucket (ADR 0052 §5) and
    /// append the run's tail remainder at the slab tail.
    ///
    /// The first `hole_window_indices.len()` edges of the run are written into
    /// the tombstone holes (in window order); the remaining edges are appended
    /// contiguously at `edge_start_slot + stored_slots`.
    SlabHoles {
        /// Physical slab slot of the bucket's first stored slot (window start).
        edge_start_slot: u64,
        /// Window offsets (bucket-local, ascending) of the tombstone holes filled
        /// by this run. The live ordinal of hole `j` is `offset - j`.
        hole_window_indices: Vec<u32>,
        /// Number of run edges appended at the slab tail after the stored window.
        tail_edge_slot_count: u32,
        /// Edge write strategy chosen at preflight.
        write_strategy: SlabHoleWriteStrategy,
        /// Byte offset in the inline property bytes slab where the run writes, if inline-property-bearing.
        inline_property_bytes_offset: Option<u64>,
        /// Number of inline property bytes reserved.
        inline_property_bytes_byte_count: u64,
    },
    /// Expand the pinned PMA leaf block in place and write the run into the new slab
    /// space after folding any existing overflow log entries.
    ExpandedSlab {
        /// PMA leaf whose block is expanded.
        leaf: u32,
        /// Leaf block length before expansion.
        old_leaf_len: u64,
        /// Leaf block length after expansion.
        new_leaf_len: u64,
        /// Edge-slab slots that already exist in the bucket (including any folded log).
        existing_bucket_slots: u32,
        /// Edge overflow-log entries that must be folded into the slab before writing.
        edge_log_len: u32,
        /// InlinePropertyBytes slab slots already resident (including any folded inline property bytes log).
        existing_inline_property_bytes_slots: u32,
        /// InlinePropertyBytes overflow-log entries that must be folded before writing.
        inline_property_bytes_log_len: u32,
        /// Byte offset in the inline property bytes slab where the bucket's expanded value span starts.
        inline_property_bytes_offset: Option<u64>,
        /// Number of inline property bytes reserved for the pending batch edges.
        inline_property_bytes_byte_count: u64,
    },
    /// Relocate one pinned PMA leaf, fold its existing rows, then append the
    /// pending edge rows at each affected bucket's slab tail.
    RelocatedSlab {
        /// PMA leaf relocated once for this reservation.
        leaf: u32,
        /// Target leaf length used by the relocation plan.
        new_leaf_len: u64,
        /// Edge-slab slots that already exist in the bucket (including any folded log).
        existing_bucket_slots: u32,
        /// Edge overflow-log entries folded into the relocated slab.
        edge_log_len: u32,
        /// Inline property bytes slab slots already resident before the batch.
        existing_inline_property_bytes_slots: u32,
        /// Inline property bytes overflow-log entries folded into the relocated slab.
        inline_property_bytes_log_len: u32,
        /// Byte offset for the relocated inline property bytes span, if present.
        inline_property_bytes_offset: Option<u64>,
        /// Number of inline property bytes reserved for the pending batch edges.
        inline_property_bytes_byte_count: u64,
    },
    /// Plan 0321 Step 2: tree-mode batch admission (minimal first
    /// slice — tail-fit only). The run writes into the existing
    /// tail block at `tail_block_id + tail_offset_in_bytes` for
    /// `run.edges.len() * E::BYTES` bytes. No new LTB blocks are
    /// minted; no root region growth. Runs that would cross a
    /// block boundary (or require root growth) are rejected
    /// typed — the caller falls back to scalar inserts. The
    /// follow-up slice widens to multi-block + root-growth.
    ///
    /// Tree mode invariants (Plan 0318 / 0319 / 0320): 4-byte
    /// edges, no inline property bytes, no tombstones (no hole
    /// reuse — `Unordered` placement appends at the tail like
    /// `Insertion`).
    Tree {
        /// LTB block id of the tail block where the run's first edge writes.
        tail_block_id: u32,
        /// Byte offset within the tail block where the run's first edge writes.
        tail_offset_bytes: usize,
        /// Number of run edges that fit in the existing tail block.
        /// (For the minimal first slice this equals `run.edges.len()`.)
        run_edge_count: u32,
    },
}

/// Intermediate preflight record for one run before any mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlinePropertyBytesAllocationKind {
    /// Existing span; write at the existing offset.
    Existing { offset: u64 },
    /// New span of the given byte length must be allocated at reserve time.
    New { byte_len: u64 },
    /// Existing span at the occupied tail; grow in place by the given byte length.
    GrowInPlace {
        offset: u64,
        had_bytes: u64,
        new_bytes: u64,
    },
    /// Copy an existing non-tail span into a newly appended span at reserve time.
    Reallocated {
        offset: u64,
        had_bytes: u64,
        new_bytes: u64,
    },
}

/// Per-leaf expansion state aggregated during preflight.
///
/// Multiple runs in the same pinned PMA leaf share one in-place expansion.  The
/// first run that needs expansion computes the initial target; later runs may
/// grow the same block further.  The consumed free-span prefix is recorded so
/// reserve rollback can restore it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LeafExpansionState {
    leaf: u32,
    /// Leaf block length before any expansion in this batch.
    old_leaf_len: u64,
    /// Current reserved leaf block length.
    new_leaf_len: u64,
    /// Cumulative edge-slab slots (folded log entries + pending edges) that
    /// must fit inside the expanded leaf block.
    required_extra: u64,
    /// Free spans consumed by the cumulative expansion, recorded so rollback
    /// can restore them. Kept in allocation order; restore in reverse order.
    taken_free_spans: Vec<crate::lara::edge::free_span::FreeSpan>,
    /// Segment-total delta published during reserve so commit-time vertex-span
    /// checks observe the expanded leaf geometry. Restored on rollback.
    leaf_total_delta: i64,
}

struct PreflightRun {
    owner_vertex_id: VertexId,
    label_id: BucketLabelKey,
    bucket_slot: u64,
    bucket: super::LabelBucket,
    edge_slot_count: u32,
    inline_property_width: u16,
    inline_property_bytes_byte_count: u64,
    inline_property_bytes_allocation: Option<InlinePropertyBytesAllocationKind>,
    destination: RunDestination,
}

#[cfg(test)]
impl Default for PreflightRun {
    fn default() -> Self {
        Self {
            owner_vertex_id: VertexId::from(0),
            label_id: BucketLabelKey::directed_from_index(0),
            bucket_slot: 0,
            bucket: super::LabelBucket::default(),
            edge_slot_count: 0,
            inline_property_width: 0,
            inline_property_bytes_byte_count: 0,
            inline_property_bytes_allocation: None,
            destination: RunDestination::Slab {
                edge_start_slot: 0,
                inline_property_bytes_offset: None,
                inline_property_bytes_byte_count: 0,
            },
        }
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Prepare every named bucket referenced by a batch plan before reservation.
    ///
    /// Bucket creation is owned by LARA because descriptor placement and its
    /// inline-property schema are storage invariants. The operation is
    /// idempotent for an existing bucket and leaves the batch reservation as
    /// the sole owner of edge capacity publication.
    pub(crate) fn prepare_batch_buckets(
        &self,
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<
        (
            Vec<(VertexId, BucketLabelKey)>,
            std::collections::BTreeMap<(VertexId, BucketLabelKey), (u64, super::LabelBucket)>,
        ),
        OneOrientationBatchError,
    >
    where
        E: CsrEdgeTombstone,
    {
        let mut new_buckets = Vec::new();
        let mut existing_buckets = std::collections::BTreeMap::new();
        let mut seen = std::collections::BTreeSet::new();
        let result = (|| {
            for run in &plan.runs {
                if seen.insert((run.owner_vertex_id, run.label_id)) {
                    let vertex = self.vertices.get(run.owner_vertex_id);
                    let bucket_missing = match self
                        .find_bucket(run.owner_vertex_id, &vertex, run.label_id)
                        .map_err(OneOrientationBatchError::from)?
                    {
                        BucketSearch::Found { slot, bucket } => {
                            existing_buckets
                                .insert((run.owner_vertex_id, run.label_id), (slot, bucket));
                            false
                        }
                        BucketSearch::Missing { .. } => {
                            new_buckets.push((run.owner_vertex_id, run.label_id));
                            true
                        }
                    };
                    if bucket_missing {
                        self.ensure_label_bucket_inline_property_byte_width(
                            run.owner_vertex_id,
                            run.label_id,
                            run.inline_property_width,
                        )
                        .map_err(OneOrientationBatchError::from)?;
                    }
                }
            }
            Ok::<(), OneOrientationBatchError>(())
        })();
        if let Err(error) = result {
            self.rollback_prepared_buckets(&new_buckets);
            return Err(error);
        }
        Ok((new_buckets, existing_buckets))
    }

    fn rollback_prepared_buckets(&self, new_buckets: &[(VertexId, BucketLabelKey)]) {
        for &(owner, label) in new_buckets.iter().rev() {
            self.buckets
                .remove_label_bucket(&self.vertices, owner, label)
                .expect("batch reservation must remove its prepared bucket");
        }
    }

    /// Validate a one-orientation batch plan and reserve the required capacity.
    ///
    /// This is the `reserve` step of the ADR 0045 boundary.  It performs all
    /// fallible validation before any canonical write.  If reserve fails after
    /// growing edge/inline-property-bytes backing capacity, it restores the logical edge-store
    /// capacity and inline property bytes occupied tail to their pre-reserve values.  InlinePropertyBytes
    /// bytes that were appended return to the implicit tail; consumed free-list
    /// prefixes are restored separately. Stable-memory pages are not shrunk.
    /// Canonical adjacency and edge rows are never modified. New named bucket
    /// descriptors are structurally prepared and owned by the returned
    /// reservation token. On success it returns an opaque [`BatchReservation`]
    /// token; the valid operations on
    /// that token are [`BatchReservation::commit`] and
    /// [`BatchReservation::rollback`].
    pub(crate) fn reserve_one_orientation_batch(
        &self,
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<BatchReservation<E>, OneOrientationBatchError>
    where
        E: CsrEdgeTombstone,
    {
        // Phase 1: validate plan invariants without touching canonical state.
        Self::validate_plan_invariants(plan)?;
        let empty_existing_buckets = std::collections::BTreeMap::new();
        match self.reserve_one_orientation_batch_prepared(plan, Vec::new(), empty_existing_buckets)
        {
            Ok(reservation) => Ok(reservation),
            Err(OneOrientationBatchError::BucketNotFound { .. }) => {
                let (new_buckets, existing_buckets) = self.prepare_batch_buckets(plan)?;
                let existing_buckets = if new_buckets.is_empty() {
                    existing_buckets
                } else {
                    // Preparing a new descriptor can rewrite the owning vertex's
                    // contiguous descriptor window. Cached existing-bucket slots
                    // are therefore invalid for this retry; preflight must reread
                    // the canonical descriptor layout.
                    std::collections::BTreeMap::new()
                };
                match self.reserve_one_orientation_batch_prepared(
                    plan,
                    new_buckets.clone(),
                    existing_buckets,
                ) {
                    Ok(reservation) => Ok(reservation),
                    Err(error) => {
                        self.rollback_prepared_buckets(&new_buckets);
                        Err(error)
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    fn reserve_one_orientation_batch_prepared(
        &self,
        plan: &OneOrientationBatchPlan<E>,
        new_buckets: Vec<(VertexId, BucketLabelKey)>,
        existing_buckets: std::collections::BTreeMap<
            (VertexId, BucketLabelKey),
            (u64, super::LabelBucket),
        >,
    ) -> Result<BatchReservation<E>, OneOrientationBatchError>
    where
        E: CsrEdgeTombstone,
    {
        // Phase 2: mutation-free preflight of every run.
        // Expanded-slab runs share per-leaf physical block growth, so their
        // reservations are tracked per leaf and the aggregate check sees the whole
        // batch. Runs that would land in a bucket's own spill run are deferred to
        // the scalar path (ADR 0097 §7) before any reservation.
        let mut preflight = Vec::with_capacity(plan.runs.len());
        let mut max_edge_end_slot: u64 = 0;
        let mut leaf_expansion_cursors: std::collections::BTreeMap<u32, LeafExpansionState> =
            std::collections::BTreeMap::new();
        let mut leaf_relocations: std::collections::BTreeMap<u32, LabeledLeafRelocationTarget> =
            std::collections::BTreeMap::new();
        for run in &plan.runs {
            let p = self.preflight_run(run, &mut leaf_expansion_cursors, &existing_buckets)?;
            match &p.destination {
                RunDestination::Slab {
                    edge_start_slot, ..
                } => {
                    max_edge_end_slot =
                        max_edge_end_slot.max(*edge_start_slot + u64::from(p.edge_slot_count));
                }
                RunDestination::SlabHoles {
                    edge_start_slot,
                    tail_edge_slot_count,
                    ..
                } => {
                    let tail_end = (*edge_start_slot)
                        .saturating_add(u64::from(p.bucket.stored_slots()))
                        .saturating_add(u64::from(*tail_edge_slot_count));
                    max_edge_end_slot = max_edge_end_slot.max(tail_end);
                }
                _ => {}
            }
            preflight.push(p);
        }

        // Each allocating run can retire its old span at commit OR restore one
        // consumed free prefix on rollback. Tail rewind adds no free-list entry.
        let value_allocations = preflight
            .iter()
            .filter(|run| {
                matches!(
                    run.inline_property_bytes_allocation,
                    Some(
                        InlinePropertyBytesAllocationKind::New { .. }
                            | InlinePropertyBytesAllocationKind::Reallocated { .. }
                    )
                )
            })
            .count() as u64;
        if value_allocations > 0 {
            self.values
                .reserve_retired_byte_spans(value_allocations)
                .map_err(storage_resize_error)?;
        }

        // Phase 2.25: reserve the cumulative in-place leaf expansion for every
        // leaf that needs it. Doing this once per leaf after all runs have been
        // projected avoids fragmenting the free span store with incremental
        // per-run expansions.
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        for (leaf, state) in leaf_expansion_cursors.iter_mut() {
            if state.new_leaf_len <= state.old_leaf_len {
                continue;
            }
            let start_vid = (*leaf).saturating_mul(seg);
            let (leaf_start, _) =
                match self.labeled_leaf_physical_range(VertexId::from(start_vid))? {
                    Some(range) => range,
                    None => {
                        Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                        return Err(OneOrientationBatchError::MissingPinnedLeafForExpansion);
                    }
                };
            let mut grown = match self.try_expand_labeled_leaf_in_place(
                leaf_start,
                state.old_leaf_len,
                state.new_leaf_len,
            ) {
                Ok(grown) => grown,
                Err(err) => {
                    Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                    return Err(OneOrientationBatchError::from(err));
                }
            };
            if !grown {
                let leaf_end = match leaf_start.checked_add(state.old_leaf_len) {
                    Some(end) => end,
                    None => {
                        Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                        return Err(OneOrientationBatchError::SlabCapacityExceeded);
                    }
                };
                if leaf_end == header.elem_capacity {
                    if let Err(err) = self.edges.set_elem_capacity(state.new_leaf_len) {
                        Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                        return Err(storage_resize_error(err));
                    }
                    grown = true;
                }
            }
            if !grown {
                let target = self
                    .plan_labeled_leaf_relocation(VertexId::from(start_vid))
                    .map_err(OneOrientationBatchError::from)?;
                let target = LabeledLeafRelocationTarget {
                    new_len: target.new_len.max(state.new_leaf_len),
                    ..target
                };
                leaf_relocations.insert(*leaf, target);
                continue;
            }
            let delta = state.new_leaf_len - state.old_leaf_len;
            let adjacent_start = match leaf_start.checked_add(state.old_leaf_len) {
                Some(start) => start,
                None => {
                    Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                    return Err(OneOrientationBatchError::SlabCapacityExceeded);
                }
            };
            state
                .taken_free_spans
                .push(crate::lara::edge::free_span::FreeSpan {
                    start_slot: adjacent_start,
                    len: delta,
                });
            // A pinned leaf created by older setup paths can have a physical
            // span while its segment `total` is still zero. In that case the
            // first publication must establish the full leaf length, not only
            // the incremental growth; otherwise readers expose a block shorter
            // than the physical expansion and rebalance rejects valid spans.
            let prior_total = self
                .leaf_segment_counts_for_vid(VertexId::from(start_vid))
                .total;
            let published_delta = if prior_total == 0 {
                state.new_leaf_len
            } else {
                delta
            };
            let delta_i64 = match i64::try_from(published_delta) {
                Ok(delta) => delta,
                Err(_) => {
                    Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                    return Err(OneOrientationBatchError::SlabCapacityExceeded);
                }
            };
            if let Err(err) =
                self.edges
                    .bump_vertex_segment_counts(VertexId::from(start_vid), 0, delta_i64)
            {
                Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                return Err(OneOrientationBatchError::from(
                    LabeledOperationError::Store(err),
                ));
            }
            state.leaf_total_delta = delta_i64;
        }

        for preflight_run in &mut preflight {
            let RunDestination::ExpandedSlab { leaf, .. } = &preflight_run.destination else {
                continue;
            };
            let leaf = *leaf;
            let Some(target) = leaf_relocations.get(&leaf) else {
                // The leaf can expand in place while an existing inline-property span is
                // non-tail. The reservation already copied that span into the free-span-first
                // destination; commit will publish the replacement bucket offset and retire the
                // old span without requiring a whole-leaf relocation.
                continue;
            };
            let RunDestination::ExpandedSlab {
                existing_bucket_slots,
                edge_log_len,
                existing_inline_property_bytes_slots,
                inline_property_bytes_log_len,
                inline_property_bytes_offset,
                inline_property_bytes_byte_count,
                ..
            } = &preflight_run.destination
            else {
                unreachable!("relocation conversion requires expanded-slab preflight");
            };
            // Copy the scalar fields so the borrow of `destination` ends before the
            // assignment below (the destination is a non-Copy enum now).
            let existing_bucket_slots = *existing_bucket_slots;
            let edge_log_len = *edge_log_len;
            let existing_inline_property_bytes_slots = *existing_inline_property_bytes_slots;
            let inline_property_bytes_log_len = *inline_property_bytes_log_len;
            let inline_property_bytes_offset = *inline_property_bytes_offset;
            let inline_property_bytes_byte_count = *inline_property_bytes_byte_count;
            preflight_run.destination = RunDestination::RelocatedSlab {
                leaf,
                new_leaf_len: target.new_len,
                existing_bucket_slots,
                edge_log_len,
                existing_inline_property_bytes_slots,
                inline_property_bytes_log_len,
                inline_property_bytes_offset,
                inline_property_bytes_byte_count,
            };
        }

        // Phase 2.5: prove that the total result counts fit in the public u32 fields.
        if let Err(err) = Self::check_total_result_counts_fit_u32(&preflight) {
            Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
            return Err(err);
        }

        // Phase 3: mutate allocator state for slab-bound runs only.  Overflow-log
        // runs reserve only ephemeral log capacity checked in preflight; they do not
        // touch the edge-store logical capacity or inline property bytes occupied tail before commit.
        let edge_capacity_before = self.edges.header().elem_capacity;
        if max_edge_end_slot > edge_capacity_before
            && let Err(err) = self.edges.set_elem_capacity(max_edge_end_slot)
        {
            Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
            return Err(storage_resize_error(err));
        }

        let inline_property_bytes_tail_before = self.values.header().slab_occupied_tail;
        let mut allocated_inline_property_bytes_offsets: Vec<Option<(u64, u64)>> =
            Vec::with_capacity(preflight.len());
        let mut allocated_inline_property_bytes_free_spans = Vec::new();
        for p in &preflight {
            match &p.destination {
                RunDestination::Slab { .. } | RunDestination::SlabHoles { .. } => {
                    if let Some(allocation) = p.inline_property_bytes_allocation {
                        match allocation {
                            InlinePropertyBytesAllocationKind::Existing { offset } => {
                                allocated_inline_property_bytes_offsets.push(Some((offset, 0)));
                            }
                            InlinePropertyBytesAllocationKind::New { byte_len } => {
                                let (actual_offset, free_span) = self
                                    .values
                                    .allocate_byte_span_with_origin(byte_len)
                                    .map_err(|e| {
                                        Self::rollback_leaf_expansions(
                                            self,
                                            &leaf_expansion_cursors,
                                        );
                                        self.rollback_inline_property_bytes_tail(
                                            inline_property_bytes_tail_before,
                                            &allocated_inline_property_bytes_free_spans,
                                        );
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                if let Some(span) = free_span {
                                    assert_eq!(actual_offset, span.start_slot);
                                    allocated_inline_property_bytes_free_spans.push(span);
                                }
                                allocated_inline_property_bytes_offsets
                                    .push(Some((actual_offset, byte_len)));
                            }
                            InlinePropertyBytesAllocationKind::GrowInPlace {
                                offset,
                                had_bytes,
                                new_bytes,
                            } => {
                                let grown = self
                                    .values
                                    .grow_byte_span_in_place(offset, had_bytes, new_bytes)
                                    .map_err(|e| {
                                        Self::rollback_leaf_expansions(
                                            self,
                                            &leaf_expansion_cursors,
                                        );
                                        self.rollback_inline_property_bytes_tail(
                                            inline_property_bytes_tail_before,
                                            &allocated_inline_property_bytes_free_spans,
                                        );
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                if !grown {
                                    Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                                    self.rollback_inline_property_bytes_tail(
                                        inline_property_bytes_tail_before,
                                        &allocated_inline_property_bytes_free_spans,
                                    );
                                    self.rollback_edge_capacity(edge_capacity_before);
                                    return Err(
                                        OneOrientationBatchError::InlinePropertyBytesSpanRequiresRelocation,
                                    );
                                }
                                allocated_inline_property_bytes_offsets
                                    .push(Some((offset, new_bytes)));
                            }
                            InlinePropertyBytesAllocationKind::Reallocated {
                                offset,
                                had_bytes,
                                new_bytes,
                            } => {
                                let (actual_offset, free_span) = self
                                    .values
                                    .allocate_byte_span_with_origin(new_bytes)
                                    .map_err(|e| {
                                        Self::rollback_leaf_expansions(
                                            self,
                                            &leaf_expansion_cursors,
                                        );
                                        self.rollback_inline_property_bytes_tail(
                                            inline_property_bytes_tail_before,
                                            &allocated_inline_property_bytes_free_spans,
                                        );
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                if let Some(span) = free_span {
                                    assert_eq!(
                                        actual_offset, span.start_slot,
                                        "inline property free-span allocation must use the peeked best-fit span"
                                    );
                                    allocated_inline_property_bytes_free_spans.push(span);
                                }
                                let mut existing =
                                    vec![
                                        0u8;
                                        usize::try_from(had_bytes).map_err(|_| {
                                            Self::rollback_leaf_expansions(
                                                self,
                                                &leaf_expansion_cursors,
                                            );
                                            self.rollback_inline_property_bytes_tail(
                                                inline_property_bytes_tail_before,
                                                &allocated_inline_property_bytes_free_spans,
                                            );
                                            self.rollback_edge_capacity(edge_capacity_before);
                                            OneOrientationBatchError::SlabCapacityExceeded
                                        })?
                                    ];
                                self.values.read_bytes(offset, &mut existing);
                                self.values
                                    .write_bytes(actual_offset, &existing)
                                    .map_err(|e| {
                                        Self::rollback_leaf_expansions(
                                            self,
                                            &leaf_expansion_cursors,
                                        );
                                        self.rollback_inline_property_bytes_tail(
                                            inline_property_bytes_tail_before,
                                            &allocated_inline_property_bytes_free_spans,
                                        );
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                allocated_inline_property_bytes_offsets
                                    .push(Some((actual_offset, new_bytes)));
                            }
                        }
                    } else {
                        allocated_inline_property_bytes_offsets.push(None);
                    }
                }
                RunDestination::ExpandedSlab { .. } | RunDestination::RelocatedSlab { .. } => {
                    // Expanded-slab runs allocate any required inline property bytes slab bytes at
                    // reserve time, just like clean-slab runs, so rollback can retire
                    // the span if the batch aborts.
                    if let Some(allocation) = p.inline_property_bytes_allocation {
                        match allocation {
                            InlinePropertyBytesAllocationKind::Existing { offset } => {
                                allocated_inline_property_bytes_offsets.push(Some((offset, 0)));
                            }
                            InlinePropertyBytesAllocationKind::New { byte_len } => {
                                let (actual_offset, free_span) = self
                                    .values
                                    .allocate_byte_span_with_origin(byte_len)
                                    .map_err(|e| {
                                        Self::rollback_leaf_expansions(
                                            self,
                                            &leaf_expansion_cursors,
                                        );
                                        self.rollback_inline_property_bytes_tail(
                                            inline_property_bytes_tail_before,
                                            &allocated_inline_property_bytes_free_spans,
                                        );
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                if let Some(span) = free_span {
                                    assert_eq!(actual_offset, span.start_slot);
                                    allocated_inline_property_bytes_free_spans.push(span);
                                }
                                allocated_inline_property_bytes_offsets
                                    .push(Some((actual_offset, byte_len)));
                            }
                            InlinePropertyBytesAllocationKind::GrowInPlace {
                                offset,
                                had_bytes,
                                new_bytes,
                            } => {
                                let grown = self
                                    .values
                                    .grow_byte_span_in_place(offset, had_bytes, new_bytes)
                                    .map_err(|e| {
                                        Self::rollback_leaf_expansions(
                                            self,
                                            &leaf_expansion_cursors,
                                        );
                                        self.rollback_inline_property_bytes_tail(
                                            inline_property_bytes_tail_before,
                                            &allocated_inline_property_bytes_free_spans,
                                        );
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                if !grown {
                                    Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                                    self.rollback_inline_property_bytes_tail(
                                        inline_property_bytes_tail_before,
                                        &allocated_inline_property_bytes_free_spans,
                                    );
                                    self.rollback_edge_capacity(edge_capacity_before);
                                    return Err(
                                        OneOrientationBatchError::InlinePropertyBytesSpanRequiresRelocation,
                                    );
                                }
                                allocated_inline_property_bytes_offsets
                                    .push(Some((offset, new_bytes - had_bytes)));
                            }
                            InlinePropertyBytesAllocationKind::Reallocated {
                                offset,
                                had_bytes,
                                new_bytes,
                            } => {
                                let (actual_offset, free_span) = self
                                    .values
                                    .allocate_byte_span_with_origin(new_bytes)
                                    .map_err(|e| {
                                        Self::rollback_leaf_expansions(
                                            self,
                                            &leaf_expansion_cursors,
                                        );
                                        self.rollback_inline_property_bytes_tail(
                                            inline_property_bytes_tail_before,
                                            &allocated_inline_property_bytes_free_spans,
                                        );
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                if let Some(span) = free_span {
                                    assert_eq!(
                                        actual_offset, span.start_slot,
                                        "inline property free-span allocation must use the peeked best-fit span"
                                    );
                                    allocated_inline_property_bytes_free_spans.push(span);
                                }
                                let mut existing =
                                    vec![
                                        0u8;
                                        usize::try_from(had_bytes).map_err(|_| {
                                            Self::rollback_leaf_expansions(
                                                self,
                                                &leaf_expansion_cursors,
                                            );
                                            self.rollback_inline_property_bytes_tail(
                                                inline_property_bytes_tail_before,
                                                &allocated_inline_property_bytes_free_spans,
                                            );
                                            self.rollback_edge_capacity(edge_capacity_before);
                                            OneOrientationBatchError::SlabCapacityExceeded
                                        })?
                                    ];
                                self.values.read_bytes(offset, &mut existing);
                                self.values
                                    .write_bytes(actual_offset, &existing)
                                    .map_err(|e| {
                                        Self::rollback_leaf_expansions(
                                            self,
                                            &leaf_expansion_cursors,
                                        );
                                        self.rollback_inline_property_bytes_tail(
                                            inline_property_bytes_tail_before,
                                            &allocated_inline_property_bytes_free_spans,
                                        );
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                allocated_inline_property_bytes_offsets
                                    .push(Some((actual_offset, new_bytes - had_bytes)));
                            }
                        }
                    } else {
                        allocated_inline_property_bytes_offsets.push(None);
                    }
                }
                RunDestination::Tree { .. } => {
                    // Tree-mode runs do not allocate inline property
                    // bytes (Plan 0321 Step 2 invariant: tree mode
                    // rejects inline-property-bearing buckets). Push
                    // a `None` so the reservation's inline_property
                    // slot stays empty.
                    allocated_inline_property_bytes_offsets.push(None);
                }
            }
        }

        // Phase 4: build the opaque reservation token from the preflight snapshot.
        // No fallible allocator calls remain from this point onward.
        let runs = preflight
            .into_iter()
            .zip(allocated_inline_property_bytes_offsets)
            .map(|(p, allocated)| {
                let destination = match p.destination {
                    RunDestination::Slab {
                        edge_start_slot, ..
                    } => {
                        let inline_property_bytes_offset = if p.inline_property_width > 0 {
                            let (actual_offset, _allocated_len) = allocated
                                .expect("inline-property-bearing slab run must have an allocated offset");
                            if let Some(InlinePropertyBytesAllocationKind::Existing { offset }) =
                                p.inline_property_bytes_allocation
                            {
                                assert_eq!(
                                    actual_offset, offset,
                                    "existing inline property bytes offset must not change during allocation"
                                );
                            }
                            Some(actual_offset)
                        } else {
                            debug_assert!(
                                allocated.is_none(),
                                "edge-only slab run must have no inline property bytes allocation"
                            );
                            None
                        };
                        RunDestination::Slab {
                            edge_start_slot,
                            inline_property_bytes_offset,
                            inline_property_bytes_byte_count: p.inline_property_bytes_byte_count,
                        }
                    }
                    RunDestination::SlabHoles {
                        edge_start_slot,
                        hole_window_indices,
                        tail_edge_slot_count,
                        write_strategy,
                        inline_property_bytes_byte_count,
                        ..
                    } => {
                        let inline_property_bytes_offset = if p.inline_property_width > 0 {
                            let (actual_offset, _allocated_len) = allocated.expect(
                                "inline-property-bearing hole-fill run must have an allocated offset",
                            );
                            if let Some(InlinePropertyBytesAllocationKind::Existing { offset }) =
                                p.inline_property_bytes_allocation
                            {
                                assert_eq!(
                                    actual_offset, offset,
                                    "existing inline property bytes offset must not change during allocation"
                                );
                            }
                            Some(actual_offset)
                        } else {
                            debug_assert!(
                                allocated.is_none(),
                                "edge-only hole-fill run must have no inline property bytes allocation"
                            );
                            None
                        };
                        RunDestination::SlabHoles {
                            edge_start_slot,
                            hole_window_indices,
                            tail_edge_slot_count,
                            write_strategy,
                            inline_property_bytes_offset,
                            inline_property_bytes_byte_count,
                        }
                    }
                    RunDestination::ExpandedSlab {
                        leaf,
                        old_leaf_len,
                        new_leaf_len,
                        existing_bucket_slots,
                        edge_log_len,
                        existing_inline_property_bytes_slots,
                        inline_property_bytes_log_len,
                        ..
                    } => {
                        let inline_property_bytes_offset = if p.inline_property_width > 0 {
                            let (actual_offset, _allocated_len) = allocated.expect(
                                "inline-property-bearing expanded-slab run must have an allocated offset",
                            );
                            Some(actual_offset)
                        } else {
                            debug_assert!(
                                allocated.is_none(),
                                "edge-only expanded-slab run must have no inline property bytes allocation"
                            );
                            None
                        };
                        RunDestination::ExpandedSlab {
                            leaf,
                            old_leaf_len,
                            new_leaf_len,
                            existing_bucket_slots,
                            edge_log_len,
                            existing_inline_property_bytes_slots,
                            inline_property_bytes_log_len,
                            inline_property_bytes_offset,
                            inline_property_bytes_byte_count: p.inline_property_bytes_byte_count,
                        }
                    }
                    RunDestination::RelocatedSlab {
                        leaf,
                        new_leaf_len,
                        existing_bucket_slots,
                        edge_log_len,
                        existing_inline_property_bytes_slots,
                        inline_property_bytes_log_len,
                        inline_property_bytes_offset: _,
                        inline_property_bytes_byte_count,
                    } => {
                        let inline_property_bytes_offset = if p.inline_property_width > 0 {
                            Some(
                                allocated
                                    .expect(
                                        "inline-property-bearing relocated run must have an allocated offset",
                                    )
                                    .0,
                            )
                        } else {
                            debug_assert!(
                                allocated.is_none(),
                                "edge-only relocated run must have no inline property bytes allocation"
                            );
                            None
                        };
                        RunDestination::RelocatedSlab {
                            leaf,
                            new_leaf_len,
                            existing_bucket_slots,
                            edge_log_len,
                            existing_inline_property_bytes_slots,
                            inline_property_bytes_log_len,
                            inline_property_bytes_offset,
                            inline_property_bytes_byte_count,
                        }
                    }
                    RunDestination::Tree {
                        tail_block_id,
                        tail_offset_bytes,
                        run_edge_count,
                    } => RunDestination::Tree {
                        tail_block_id,
                        tail_offset_bytes,
                        run_edge_count,
                    },
                };
                BatchReservationRun {
                    bucket_slot: p.bucket_slot,
                    bucket_fingerprint: BucketFingerprint::from_bucket(
                        p.owner_vertex_id,
                        p.label_id,
                        &p.bucket,
                        &self.vertices.get(p.owner_vertex_id),
                    ),
                    edge_slot_count: p.edge_slot_count,
                    inline_property_width: p.inline_property_width,
                    destination,
                }
            })
            .collect();

        Ok(BatchReservation {
            plan: plan.clone(),
            runs,
            graph_marker: self.instance_marker(),
            edge_capacity_before,
            inline_property_bytes_tail_before,
            new_buckets,
            leaf_expansions: leaf_expansion_cursors,
            leaf_relocations,
            inline_property_bytes_free_spans: allocated_inline_property_bytes_free_spans,
        })
    }

    fn validate_plan_invariants(
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<(), OneOrientationBatchError> {
        if plan.runs.is_empty() {
            return Err(OneOrientationBatchError::EmptyPlan);
        }
        let mut seen_buckets = std::collections::BTreeSet::<(VertexId, BucketLabelKey)>::new();
        for run in &plan.runs {
            if run.edges.is_empty() {
                return Err(OneOrientationBatchError::EmptyRun);
            }
            let mut prev_ordinal: Option<u32> = None;
            for e in &run.edges {
                if let Some(p) = prev_ordinal
                    && e.logical_ordinal <= p
                {
                    return Err(OneOrientationBatchError::NonIncreasingLogicalOrdinals);
                }
                prev_ordinal = Some(e.logical_ordinal);
            }
            if !seen_buckets.insert((run.owner_vertex_id, run.label_id)) {
                return Err(OneOrientationBatchError::DuplicateBucketRun);
            }
        }
        Ok(())
    }

    /// Roll back inline property bytes allocation performed during a partially-failed reserve.
    ///
    /// Appended bytes return to the implicit tail without entering the free list:
    /// registering them there too would let a later tail growth alias a free-list
    /// allocation. Only consumed free-list prefixes are restored. Live spans and
    /// the retained backing capacity are untouched.
    fn rollback_inline_property_bytes_tail(
        &self,
        original_tail: u64,
        allocated_free_spans: &[FreeSpan],
    ) {
        let current_tail = self.values.header().slab_occupied_tail;
        if current_tail > original_tail {
            self.values.reset_slab_occupied_tail(original_tail);
        }
        for span in allocated_free_spans.iter().rev() {
            self.values
                .restore_allocated_byte_span(*span)
                .expect("reserve rollback must restore inline property free-span consistency");
        }
    }

    /// Verify that the summed edge and inline property bytes slot counts of the preflight runs
    /// fit into the public `u32` result fields.
    ///
    /// This is pulled out so the boundary can be unit-tested directly: a batch
    /// with more than `u32::MAX` edge or inline property bytes slots is rejected at reserve time
    /// instead of truncating the commit result.
    fn check_total_result_counts_fit_u32(
        preflight: &[PreflightRun],
    ) -> Result<(), OneOrientationBatchError> {
        let total_edge_slots = preflight.iter().try_fold(0u64, |acc, p| {
            acc.checked_add(u64::from(p.edge_slot_count))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)
        })?;
        let total_inline_property_bytes_slots = preflight
            .iter()
            .filter(|p| p.inline_property_width > 0)
            .try_fold(0u64, |acc, p| {
                acc.checked_add(u64::from(p.edge_slot_count))
                    .ok_or(OneOrientationBatchError::SlabCapacityExceeded)
            })?;
        if u32::try_from(total_edge_slots).is_err()
            || u32::try_from(total_inline_property_bytes_slots).is_err()
        {
            return Err(OneOrientationBatchError::SlabCapacityExceeded);
        }
        Ok(())
    }
    /// Roll back edge-store logical capacity growth performed during a partially-failed reserve.
    ///
    /// Only the persisted logical `elem_capacity` is restored.  The underlying
    /// stable-memory pages are not shrunk and no new edge free spans are
    /// created, so the exact shape of the edge free-list is unchanged.
    fn rollback_edge_capacity(&self, original_capacity: u64) {
        let current_capacity = self.edges.header().elem_capacity;
        if current_capacity > original_capacity {
            self.edges.set_elem_capacity(original_capacity).expect(
                "shrinking edge-store logical capacity must not fail when memory already has slack",
            );
        }
    }

    /// Opaque identity marker for this graph instance, used to bind reservations.
    fn instance_marker(&self) -> usize {
        &self.edges as *const _ as usize
    }

    /// Restore free-span prefixes consumed by in-place leaf expansions.
    ///
    /// Called during reserve rollback and token consumption.  Each recorded
    /// `FreeSpan` was removed from the free-span store by
    /// `try_expand_labeled_leaf_in_place`; putting it back coalesces with the
    /// adjacent successor if present.
    fn rollback_leaf_expansions(
        &self,
        expansions: &std::collections::BTreeMap<u32, LeafExpansionState>,
    ) {
        for state in expansions.values() {
            for span in state.taken_free_spans.iter().rev() {
                self.edges
                    .free_span_store()
                    .restore_allocated_prefix(*span)
                    .expect("reserve rollback must restore free-span store consistency");
            }
            if state.leaf_total_delta != 0 {
                let seg = self.edges.header().segment_size.max(1);
                let start_vid = state.leaf.saturating_mul(seg);
                self.edges
                    .bump_vertex_segment_counts(start_vid.into(), 0, -state.leaf_total_delta)
                    .expect("reserve rollback must restore segment total");
            }
        }
    }

    /// Validates every inline-property-bearing edge's width and computes the
    /// inline property bytes byte count plus the span allocation kind for a
    /// slab-bound run (shared by the tail-append and hole-fill preflight paths).
    fn preflight_inline_property_bytes(
        &self,
        bucket: super::LabelBucket,
        run: &OneOrientationBucketRun<E>,
        edge_slot_count: u32,
    ) -> Result<(u64, Option<InlinePropertyBytesAllocationKind>), OneOrientationBatchError>
    where
        E: CsrEdgeTombstone,
    {
        // Verify every inline-property-bearing edge matches the declared width.  This
        // proves the commit-time assertion cannot fire for malformed input.
        let mut inline_property_bytes_byte_count: u64 = 0;
        if run.inline_property_width > 0 {
            let width = usize::from(run.inline_property_width);
            for e in &run.edges {
                let actual = e.edge.edge_inline_property_bytes().len();
                if actual != width {
                    return Err(
                        OneOrientationBatchError::InlinePropertyBytesLengthMismatch {
                            logical_ordinal: e.logical_ordinal,
                            expected_width: run.inline_property_width,
                            actual_length: actual,
                        },
                    );
                }
            }
            inline_property_bytes_byte_count = u64::from(edge_slot_count)
                .checked_mul(u64::from(run.inline_property_width))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
        }

        // Compute inline property bytes destination and allocation kind without mutating the
        // inline property bytes store.  For a brand-new span we only record the required byte
        // length; the actual offset is determined by the allocator in Phase 3.
        let mut inline_property_bytes_allocation = None;
        if run.inline_property_width > 0 {
            let width = u64::from(run.inline_property_width);
            let total_inline_property_bytes_slots =
                u64::from(bucket.inline_property_bytes_slab_slots())
                    .checked_add(u64::from(edge_slot_count))
                    .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let total_inline_property_bytes = total_inline_property_bytes_slots
                .checked_mul(width)
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let had_bytes = u64::from(bucket.inline_property_bytes_slab_slots())
                .checked_mul(width)
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let offset = bucket.inline_property_bytes_offset();
            let tail = self.values.header().slab_occupied_tail;
            let span_ends_at_tail = offset.checked_add(had_bytes).is_some_and(|end| end == tail);

            if had_bytes == 0 {
                inline_property_bytes_allocation = Some(InlinePropertyBytesAllocationKind::New {
                    byte_len: total_inline_property_bytes,
                });
            } else if span_ends_at_tail {
                inline_property_bytes_allocation =
                    Some(InlinePropertyBytesAllocationKind::GrowInPlace {
                        offset,
                        had_bytes,
                        new_bytes: total_inline_property_bytes,
                    });
            } else {
                inline_property_bytes_allocation =
                    Some(InlinePropertyBytesAllocationKind::Reallocated {
                        offset,
                        had_bytes,
                        new_bytes: total_inline_property_bytes,
                    });
            }
            assert_eq!(
                total_inline_property_bytes,
                inline_property_bytes_byte_count + had_bytes,
                "preflight inline property geometry mismatch"
            );
        }
        Ok((
            inline_property_bytes_byte_count,
            inline_property_bytes_allocation,
        ))
    }

    #[allow(clippy::needless_return)]
    fn preflight_run(
        &self,
        run: &OneOrientationBucketRun<E>,
        leaf_expansion_cursors: &mut std::collections::BTreeMap<u32, LeafExpansionState>,
        existing_buckets: &std::collections::BTreeMap<
            (VertexId, BucketLabelKey),
            (u64, super::LabelBucket),
        >,
    ) -> Result<PreflightRun, OneOrientationBatchError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(run.owner_vertex_id)
            .map_err(OneOrientationBatchError::from)?;
        let vertex = self.vertices.get(run.owner_vertex_id);
        let (bucket_slot, bucket) = match existing_buckets
            .get(&(run.owner_vertex_id, run.label_id))
            .copied()
        {
            Some(found) => found,
            None => match self.find_bucket(run.owner_vertex_id, &vertex, run.label_id)? {
                BucketSearch::Found { slot, bucket } => (slot, bucket),
                BucketSearch::Missing { .. } => {
                    return Err(OneOrientationBatchError::BucketNotFound {
                        owner_vertex_id: run.owner_vertex_id,
                        label_id: run.label_id,
                    });
                }
            },
        };
        // ADR 0096 §7: match-first dispatch — mode decides before any
        // storage-class read, so a future mode cannot silently inherit a path.
        match BucketMode::from_bucket(&bucket) {
            // ADR 0096 §5: tiny buckets stay on the scalar path (which promotes
            // naturally via the insert dispatcher). Guard BEFORE the width check
            // below: width bytes are payload on tiny and would misfire
            // WidthMismatch. Snapshot fields above are harmless pre-guard
            // (garbage-but-stable compares equal; nothing consumes them past this
            // rejection).
            BucketMode::Tiny => Err(OneOrientationBatchError::TinyBucketRunUnsupported {
                owner_vertex_id: run.owner_vertex_id,
                label_id: run.label_id,
            }),
            // Plan 0321 Step 1 (Commit 1): the batch planner has no
            // tree-mode geometry. Reject tree-mode bucket runs at the
            // head of `preflight_run` (after find_bucket, before any
            // edge_start_slot / hole-reuse math) so the caller's
            // existing scalar fallback (per-edge insert which handles
            // tree append correctly) takes over. The widening
            // (Commit 2 of Plan 0321) is the typed follow-up that
            // ADMITs tree runs.
            BucketMode::Tree => {
                // Plan 0321 Step 2: tree-mode batch admission (minimal
                // first slice — tail-fit only). Compute the tail block
                // room; if the run fits, admit it. If the run crosses a
                // block boundary, reject it with a typed error — the
                // caller falls back to scalar inserts (which handle
                // root-growth correctly). The follow-up widens to
                // multi-block + root-growth.
                //
                // Tree-mode invariant checked here: batch admission
                // does not carry inline-property rows.
                if run.inline_property_width != 0 {
                    // Plan 0326 LPB-in-tree: tree mode + `w > 0` is
                    // not admitted in batch (the property row writeback
                    // path is per-edge). The caller's scalar fallback
                    // (per-edge insert) handles the new property
                    // tree branch via `tree_mode_insert_edge` +
                    // property row writeback. Typed reject signals
                    // the recorder to route to scalar.
                    return Err(OneOrientationBatchError::InlinePropertyBytesWidthMismatch {
                        bucket_width: 0,
                        edge_width: run.inline_property_width,
                    });
                }
                let run_count = u32::try_from(run.edges.len())
                    .map_err(|_| OneOrientationBatchError::SlabCapacityExceeded)?;
                if run_count == 0 {
                    return Err(OneOrientationBatchError::EmptyRun);
                }
                // Tail-block room. The run must fit within the existing
                // tail block (no new LTB block, no root growth in this
                // first slice). The follow-up slice widens to
                // multi-block.
                //
                // **F-1 review (2026-09-02)**: when `stored % B == 0`
                // (the bucket ends exactly on a block boundary, e.g.
                // immediately after promotion at `stored = T_PROMOTE =
                // 4096`), `tail_offset == 0` and `tail_room = B*E::BYTES`
                // looks like the full block is free. But the **next**
                // block does not exist yet — the root region has
                // `ceil(stored/B) = stored/B` block ids, so
                // `tail_block_idx = stored/B` is out of range. The
                // scalar path (`tree_mode_insert_edge:100`) handles this
                // by minting a new block when `tail_offset == 0`. The
                // batch minimal-first-slice does NOT mint, so
                // `tail_offset == 0` is rejected typed (the caller falls
                // back to scalar inserts which mint correctly).
                let block_b: u32 = crate::labeled::tree_csr::B as u32;
                let stored = bucket.stored_slots();
                let tail_offset: u32 = (stored % block_b) * (E::BYTES as u32);
                let tail_room: u32 = (block_b * (E::BYTES as u32)) - tail_offset;
                let run_bytes = u32::try_from(run.edges.len())
                    .map_err(|_| OneOrientationBatchError::SlabCapacityExceeded)?
                    .checked_mul(E::BYTES as u32)
                    .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
                if tail_offset == 0 || run_bytes > tail_room {
                    // Tail block is exactly full (`tail_offset == 0`)
                    // OR the run would cross a block boundary. The
                    // widening (follow-up) admits both via new-block
                    // mint + root region growth; the minimal first
                    // slice rejects typed. Caller falls back to scalar
                    // inserts which handle the `tail_offset == 0` case
                    // by minting a new block (Plan 0318 §tree insert).
                    return Err(OneOrientationBatchError::TreeRunExceedsTailBlock {
                        owner_vertex_id: run.owner_vertex_id,
                        label_id: run.label_id,
                        run_bytes: u64::from(run_bytes),
                        tail_room_bytes: u64::from(tail_room),
                    });
                }
                // Resolve the tail block id via the depth-generic
                // resolver. For depth 1 this is a single LEG read; for
                // depth 2+ it descends the interior hop chain. The
                // resolver is `pub(crate)` and lives in tree_write.rs.
                let tail_block_idx = stored / block_b;
                let tail_block_id = crate::labeled::graph::tree_write::resolve_leaf_block_id(
                    self,
                    &bucket,
                    tail_block_idx,
                )
                .map_err(OneOrientationBatchError::StorageError)?;
                // Build the PreflightRun for this tree-tail-fit run.
                Ok(PreflightRun {
                    owner_vertex_id: run.owner_vertex_id,
                    label_id: run.label_id,
                    bucket_slot,
                    bucket,
                    edge_slot_count: run_count,
                    inline_property_width: 0,
                    inline_property_bytes_byte_count: 0,
                    inline_property_bytes_allocation: None,
                    destination: RunDestination::Tree {
                        tail_block_id,
                        tail_offset_bytes: tail_offset as usize,
                        run_edge_count: run_count,
                    },
                })
            }
            BucketMode::Slab => {
                // Width check lives INSIDE the Slab arm (§7.1): tiny diverged
                // above and tree returns from its arm, so only slab widths
                // (real schema) ever reach this read.
                if run.inline_property_width != bucket.inline_property_byte_width() {
                    return Err(OneOrientationBatchError::InlinePropertyBytesWidthMismatch {
                        bucket_width: bucket.inline_property_byte_width(),
                        edge_width: run.inline_property_width,
                    });
                }

                let edge_start_slot =
                    checked_add_slot_index(bucket.edge_start(), u64::from(bucket.stored_slots()))
                        .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
                let edge_slot_count = u32::try_from(run.edges.len())
                    .map_err(|_| OneOrientationBatchError::SlabCapacityExceeded)?;
                let edge_end_slot = edge_start_slot
                    .checked_add(u64::from(edge_slot_count))
                    .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;

                let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, bucket_slot)
                    .map_err(OneOrientationBatchError::from)?;
                let successor_start = self
                    .bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)
                    .map_err(OneOrientationBatchError::from)?;
                // A bucket with an existing overflow chain must continue through the
                // log/fold path. Writing directly into its slab would leave the chain
                // published alongside the new slab prefix and break edge/inline-property-bytes
                // ordinal alignment.
                if bucket.edge_spill_run() != crate::traits::NO_SPILL_RUN
                    || bucket.values_spill_run() != crate::traits::NO_SPILL_RUN
                {
                    return self.preflight_overflow_log_run(
                        run,
                        bucket_slot,
                        bucket,
                        leaf_expansion_cursors,
                    );
                }

                // Unordered placement (ADR 0052 §5): count in-slab tombstone holes as
                // reusable capacity before reserving tail or log space. Restricted to
                // chain-free buckets whose inline property bytes are slab-backed: the O(1)
                // gate `stored_slots > degree` is exact for chain-free buckets, and
                // log-backed bytes cannot be synchronized at a reused middle ordinal
                // (ADR 0052 §9, same fallback as the scalar reuse path). The window is read
                // once and decoded in-buffer; the scan stops once the run is covered. This
                // branch runs before the tail-fit check because a span that is exactly full
                // of live edges plus tombstones has no tail room but can still admit the run
                // through its holes. When holes plus the available tail cannot admit the
                // whole run, the run keeps the current tail-first/log path (a mixed
                // slab+log split of one run is out of scope).
                if run.placement == EdgePlacementPolicy::Unordered
                    && bucket.stored_slots() > bucket.degree
                {
                    let window_len = u64::from(bucket.stored_slots());
                    let mut window = vec![0u8; (window_len as usize).saturating_mul(E::BYTES)];
                    self.edges
                        .read_slots_contiguous(bucket.edge_start(), &mut window);
                    let mut hole_window_indices = Vec::new();
                    for slot_index in 0..bucket.stored_slots() {
                        let off = (slot_index as usize).saturating_mul(E::BYTES);
                        let encoded = E::read_from(&window[off..off + E::BYTES]);
                        if encoded.is_tombstone_edge() {
                            hole_window_indices.push(slot_index);
                            if hole_window_indices.len() == run.edges.len() {
                                break;
                            }
                        }
                    }
                    if !hole_window_indices.is_empty() {
                        let holes = hole_window_indices.len() as u64;
                        let tail_edge_slot_count = edge_slot_count
                            .checked_sub(hole_window_indices.len() as u32)
                            .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
                        let tail_capacity = successor_start.saturating_sub(edge_start_slot);
                        if tail_capacity >= u64::from(tail_edge_slot_count) {
                            let (
                                inline_property_bytes_byte_count,
                                inline_property_bytes_allocation,
                            ) =
                                self.preflight_inline_property_bytes(bucket, run, edge_slot_count)?;
                            let write_strategy =
                                choose_slab_hole_write_strategy(E::BYTES, window_len, holes);
                            return Ok(PreflightRun {
                                owner_vertex_id: run.owner_vertex_id,
                                label_id: run.label_id,
                                bucket_slot,
                                bucket,
                                edge_slot_count,
                                inline_property_width: run.inline_property_width,
                                inline_property_bytes_byte_count,
                                inline_property_bytes_allocation,
                                destination: RunDestination::SlabHoles {
                                    edge_start_slot: bucket.edge_start(),
                                    hole_window_indices,
                                    tail_edge_slot_count,
                                    write_strategy,
                                    inline_property_bytes_offset: None,
                                    inline_property_bytes_byte_count,
                                },
                            });
                        }
                    }
                }

                if edge_end_slot > successor_start {
                    return self.preflight_overflow_log_run(
                        run,
                        bucket_slot,
                        bucket,
                        leaf_expansion_cursors,
                    );
                }

                // Verify every inline-property-bearing edge matches the declared width.  This
                // proves the commit-time assertion cannot fire for malformed input.
                let (inline_property_bytes_byte_count, inline_property_bytes_allocation) =
                    self.preflight_inline_property_bytes(bucket, run, edge_slot_count)?;

                Ok(PreflightRun {
                    owner_vertex_id: run.owner_vertex_id,
                    label_id: run.label_id,
                    bucket_slot,
                    bucket,
                    edge_slot_count,
                    inline_property_width: run.inline_property_width,
                    inline_property_bytes_byte_count,
                    inline_property_bytes_allocation,
                    destination: RunDestination::Slab {
                        edge_start_slot,
                        inline_property_bytes_offset: None,
                        inline_property_bytes_byte_count,
                    },
                })
            }
        }
    }

    /// ADR 0097 §7: a run whose rows would land in the bucket's own spill run is
    /// deferred to the scalar path. The batch has no run-growth reservation yet
    /// (a growth copies up to 2x the live rows, and a run at its largest class
    /// promotes the bucket), so the run is rejected *before* anything is
    /// reserved: no canonical write happens, and the caller's scalar fallback
    /// appends to the run, grows it by class and promotes when it must.
    fn preflight_overflow_log_run(
        &self,
        run: &OneOrientationBucketRun<E>,
        _bucket_slot: u64,
        _bucket: super::LabelBucket,
        _leaf_expansion_cursors: &mut std::collections::BTreeMap<u32, LeafExpansionState>,
    ) -> Result<PreflightRun, OneOrientationBatchError> {
        Err(OneOrientationBatchError::SpillRunDeferred {
            owner_vertex_id: run.owner_vertex_id,
            label_id: run.label_id,
        })
    }

    /// Preflight an existing-bucket run that does not fit the clean slab window
    /// or the per-leaf overflow log by expanding the pinned PMA leaf block in
    /// place. Fixed-width inline property bytes spans are projected with the edge/log fold;
    /// a non-tail span is copied to a replacement span when the leaf relocates.
    fn preflight_expanded_run(
        &self,
        run: &OneOrientationBucketRun<E>,
        bucket_slot: u64,
        bucket: super::LabelBucket,
        leaf_expansion_cursors: &mut std::collections::BTreeMap<u32, LeafExpansionState>,
    ) -> Result<PreflightRun, OneOrientationBatchError> {
        let header = self.edges.header();
        let leaf = Self::leaf_index_for_vid(run.owner_vertex_id, header.segment_size);
        let (_leaf_start, current_leaf_len) =
            match self.labeled_leaf_physical_range(run.owner_vertex_id)? {
                Some(range) => range,
                None => {
                    return Err(OneOrientationBatchError::MissingPinnedLeafForExpansion);
                }
            };
        let edge_slot_count = u32::try_from(run.edges.len())
            .map_err(|_| OneOrientationBatchError::SlabCapacityExceeded)?;
        let edge_log_len = match self.edges.spill_mint(bucket.edge_spill_run()) {
            Ok(run) => self.edges.spill_used_len(run),
            // Unset (NO_SPILL_RUN) or outside the arena: no rows beyond the prefix.
            Err(_) => 0,
        };

        // Validate inline-property-bearing edges and project the inline property bytes slab space that
        // will hold the existing slab values, any folded inline property bytes log entries, and
        // the pending batch values after the leaf expansion.
        let mut inline_property_bytes_byte_count: u64 = 0;
        let mut inline_property_bytes_allocation: Option<InlinePropertyBytesAllocationKind> = None;
        let existing_inline_property_bytes_slots = bucket.inline_property_bytes_slab_slots();
        let inline_property_bytes_log_len = if run.inline_property_width > 0 {
            let width = usize::from(run.inline_property_width);
            for e in &run.edges {
                let actual = e.edge.edge_inline_property_bytes().len();
                if actual != width {
                    return Err(
                        OneOrientationBatchError::InlinePropertyBytesLengthMismatch {
                            logical_ordinal: e.logical_ordinal,
                            expected_width: run.inline_property_width,
                            actual_length: actual,
                        },
                    );
                }
            }
            inline_property_bytes_byte_count = u64::from(edge_slot_count)
                .checked_mul(u64::from(run.inline_property_width))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let log_len = if bucket.values_spill_run() != crate::traits::NO_SPILL_RUN {
                let chain_len = match self.values.spill_mint(bucket.values_spill_run()) {
                    Ok(run) => self.values.spill_used_len(run) as usize,
                    Err(_) => 0,
                };
                let log_len = u32::try_from(chain_len)
                    .map_err(|_| OneOrientationBatchError::LogCapacityExceeded)?;
                let metadata_len = self.bucket_values_run_rows(&bucket);
                if log_len != metadata_len || log_len != edge_log_len {
                    return Err(
                        OneOrientationBatchError::InlinePropertyBytesLogLengthMismatch {
                            edge_log_len,
                            inline_property_bytes_log_len: log_len,
                        },
                    );
                }
                log_len
            } else {
                if edge_log_len != 0 || self.bucket_values_run_rows(&bucket) != 0 {
                    return Err(
                        OneOrientationBatchError::InlinePropertyBytesLogLengthMismatch {
                            edge_log_len,
                            inline_property_bytes_log_len: 0,
                        },
                    );
                }
                0
            };
            let total_inline_property_bytes_slots = u64::from(existing_inline_property_bytes_slots)
                .checked_add(u64::from(log_len))
                .and_then(|s| s.checked_add(u64::from(edge_slot_count)))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let total_inline_property_bytes = total_inline_property_bytes_slots
                .checked_mul(u64::from(run.inline_property_width))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let had_bytes = u64::from(existing_inline_property_bytes_slots)
                .checked_mul(u64::from(run.inline_property_width))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let offset = bucket.inline_property_bytes_offset();
            let tail = self.values.header().slab_occupied_tail;
            let span_ends_at_tail = offset.checked_add(had_bytes).is_some_and(|end| end == tail);

            if had_bytes == 0 {
                inline_property_bytes_allocation = Some(InlinePropertyBytesAllocationKind::New {
                    byte_len: total_inline_property_bytes,
                });
            } else if span_ends_at_tail {
                inline_property_bytes_allocation =
                    Some(InlinePropertyBytesAllocationKind::GrowInPlace {
                        offset,
                        had_bytes,
                        new_bytes: total_inline_property_bytes,
                    });
            } else {
                inline_property_bytes_allocation =
                    Some(InlinePropertyBytesAllocationKind::Reallocated {
                        offset,
                        had_bytes,
                        new_bytes: total_inline_property_bytes,
                    });
            }
            log_len
        } else {
            if bucket.values_spill_run() != crate::traits::NO_SPILL_RUN
                || self.bucket_values_run_rows(&bucket) != 0
            {
                return Err(
                    OneOrientationBatchError::InlinePropertyBytesLogLengthMismatch {
                        edge_log_len,
                        inline_property_bytes_log_len: u32::from(
                            bucket.inline_property_bytes_log_len(),
                        ),
                    },
                );
            }
            0
        };

        // Aggregate per-leaf expansion state. Multiple runs in the same pinned PMA
        // leaf share one cumulative in-place expansion; later runs grow the same
        // reserved block further.
        let extra = u64::from(edge_slot_count)
            .checked_add(u64::from(edge_log_len))
            .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
        let state = leaf_expansion_cursors
            .entry(leaf)
            .or_insert(LeafExpansionState {
                leaf,
                old_leaf_len: current_leaf_len,
                new_leaf_len: current_leaf_len,
                required_extra: 0,
                taken_free_spans: Vec::new(),
                leaf_total_delta: 0,
            });
        state.required_extra = state
            .required_extra
            .checked_add(extra)
            .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
        let seg = header.segment_size.max(1);
        let block_len = super::leaf_arena::labeled_leaf_physical_block_len(seg);
        let geometric_slack = state.required_extra.div_ceil(8).max(u64::from(seg));
        let target_len = state
            .old_leaf_len
            .checked_add(state.required_extra)
            .and_then(|s| s.checked_add(geometric_slack))
            .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
        let target_len = target_len
            .div_ceil(block_len)
            .saturating_mul(block_len)
            .max(block_len);
        // The geometric estimate above accounts for this run's log fold and
        // pending edges, but the leaf may contain other buckets whose resident
        // spans also constrain the preferred vertex. Ensure the projected leaf
        // can satisfy the same minimum used by rebalance_vertex_edge_span.
        let vertex = self.vertices.get(run.owner_vertex_id);
        let buckets = self
            .read_vertex_label_buckets(&vertex)
            .map_err(OneOrientationBatchError::from)?;
        let resident_slots = buckets.iter().try_fold(0u32, |sum, b| {
            sum.checked_add(b.stored_slots().max(b.degree))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)
        })?;
        let min_required = resident_slots
            .checked_add(
                edge_log_len
                    .checked_add(edge_slot_count)
                    .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?,
            )
            .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
        let max_in_leaf = self
            .labeled_vertex_stored_slots_max_in_leaf(run.owner_vertex_id)
            .map_err(OneOrientationBatchError::from)? as u64;
        let projected_max = max_in_leaf
            .checked_add(state.new_leaf_len.saturating_sub(state.old_leaf_len))
            .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
        if u64::from(min_required) > projected_max {
            let deficit = u64::from(min_required) - projected_max;
            let required_len = state
                .new_leaf_len
                .checked_add(deficit)
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            state.new_leaf_len = state.new_leaf_len.max(required_len);
        }
        // Do not expand the leaf block here. All runs in the same leaf are
        // aggregated first; the actual free-span reservation happens once per
        // leaf after the entire preflight pass.
        state.new_leaf_len = state.new_leaf_len.max(target_len);

        Ok(PreflightRun {
            owner_vertex_id: run.owner_vertex_id,
            label_id: run.label_id,
            bucket_slot,
            bucket,
            edge_slot_count,
            inline_property_width: run.inline_property_width,
            inline_property_bytes_byte_count,
            inline_property_bytes_allocation,
            destination: RunDestination::ExpandedSlab {
                leaf,
                old_leaf_len: state.old_leaf_len,
                new_leaf_len: state.new_leaf_len,
                existing_bucket_slots: bucket.stored_slots(),
                edge_log_len,
                existing_inline_property_bytes_slots,
                inline_property_bytes_log_len,
                inline_property_bytes_offset: None,
                inline_property_bytes_byte_count,
            },
        })
    }
}

impl<E> BatchReservation<E>
where
    E: CsrEdgeTombstone,
{
    /// Commit the reserved batch.
    ///
    /// This is the `commit` step.  All fallible validation and capacity reservation
    /// was performed during reserve, so this method performs only canonical byte
    /// writes and metadata updates that reserve proved cannot fail due to capacity.
    /// A panic here is an invariant violation rather than a recoverable error.  In
    /// a canister message, such a panic traps and rolls back the entire message, so
    /// no partial canonical state is published.
    pub(crate) fn commit<M: Memory>(
        self,
        graph: &LabeledLaraGraph<E, M>,
    ) -> OneOrientationBatchResult {
        self.commit_with_location_mode(graph, BatchLocationMode::AggregateOnly)
    }

    /// Commit while retaining exact physical locations for every edge.
    pub(crate) fn commit_with_locations<M: Memory>(
        self,
        graph: &LabeledLaraGraph<E, M>,
    ) -> OneOrientationBatchResult {
        self.commit_with_location_mode(graph, BatchLocationMode::Capture)
    }

    /// Commit with explicit control over physical-location materialization.
    pub(crate) fn commit_with_location_mode<M: Memory>(
        self,
        graph: &LabeledLaraGraph<E, M>,
        location_mode: BatchLocationMode,
    ) -> OneOrientationBatchResult {
        assert_eq!(
            self.plan.runs.len(),
            self.runs.len(),
            "plan/reservation length mismatch: reserve and commit must be paired"
        );

        // Runtime instance check: the reservation must be committed to the same
        // graph instance that produced it.  The type system cannot enforce this
        // because the reservation outlives the reserve borrow.
        assert_eq!(
            self.graph_marker,
            graph.instance_marker(),
            "reservation was produced by a different graph instance"
        );

        let total_edge_slots: u64 = self.runs.iter().map(|r| u64::from(r.edge_slot_count)).sum();
        let mut vertex_updates = std::collections::BTreeMap::new();

        // Pre-pass: validate every run against current canonical state and prove
        // that the upcoming metadata updates cannot overflow.  Any mismatch here
        // panics before the first canonical byte write.
        for (run, res) in self.plan.runs.iter().zip(self.runs.iter()) {
            assert_eq!(
                run.owner_vertex_id, res.bucket_fingerprint.owner_vertex_id,
                "reservation owner mismatch"
            );
            assert_eq!(
                run.label_id, res.bucket_fingerprint.label_id,
                "reservation label mismatch"
            );
            assert_eq!(
                run.inline_property_width, res.inline_property_width,
                "reservation inline width mismatch"
            );
            assert_eq!(
                run.edges.len(),
                res.edge_slot_count as usize,
                "reservation edge count mismatch"
            );

            let vertex = graph.vertices.get(res.bucket_fingerprint.owner_vertex_id);
            let (bucket_slot, bucket) = match graph
                .find_bucket(
                    res.bucket_fingerprint.owner_vertex_id,
                    &vertex,
                    res.bucket_fingerprint.label_id,
                )
                .expect("reserve found this bucket")
            {
                BucketSearch::Found { slot, bucket } => (slot, bucket),
                BucketSearch::Missing { .. } => {
                    panic!("bucket disappeared between reserve and commit");
                }
            };

            assert_eq!(
                bucket_slot, res.bucket_slot,
                "bucket slot changed between reserve and commit"
            );
            // Expanded-slab runs rebalance the vertex span, so the bucket's
            // physical start is intentionally allowed to change between reserve
            // and commit.  Validate every other fingerprint field.
            let mut current_fingerprint = BucketFingerprint::from_bucket(
                res.bucket_fingerprint.owner_vertex_id,
                res.bucket_fingerprint.label_id,
                &bucket,
                &vertex,
            );
            if matches!(res.destination, RunDestination::ExpandedSlab { .. }) {
                current_fingerprint.inline_property_bytes_offset =
                    res.bucket_fingerprint.inline_property_bytes_offset;
            }
            assert_eq!(
                current_fingerprint, res.bucket_fingerprint,
                "bucket occupancy changed between reserve and commit"
            );

            // Accumulate all runs for an owner before any canonical write. Value
            // accounting and vertex coverage must not depend on the destination.
            let (projected, edge_base) = vertex_updates
                .entry(res.bucket_fingerprint.owner_vertex_id)
                .or_insert_with(|| {
                    (
                        vertex,
                        graph
                            .labeled_edge_base_from_first_bucket(
                                res.bucket_fingerprint.owner_vertex_id,
                            )
                            .expect("reserved owner has a physical edge span"),
                    )
                });
            let folded_values = match &res.destination {
                RunDestination::ExpandedSlab {
                    inline_property_bytes_log_len,
                    ..
                }
                | RunDestination::RelocatedSlab {
                    inline_property_bytes_log_len,
                    ..
                } => *inline_property_bytes_log_len,
                _ => 0,
            };
            let added_bytes = (u64::from(res.edge_slot_count) + u64::from(folded_values))
                * u64::from(res.inline_property_width);
            let allocated = projected
                .inline_property_bytes_allocated_bytes()
                .checked_add(added_bytes)
                .expect("batch vertex value allocation overflow");
            *projected = projected
                .try_with_inline_property_bytes_allocated_bytes(allocated)
                .expect("batch vertex value allocation must fit the row");
            let edge_end = match &res.destination {
                RunDestination::Slab {
                    edge_start_slot, ..
                } => Some(
                    edge_start_slot
                        .checked_add(u64::from(res.edge_slot_count))
                        .expect("batch edge end overflow"),
                ),
                RunDestination::SlabHoles {
                    edge_start_slot,
                    tail_edge_slot_count,
                    ..
                } => Some(
                    edge_start_slot
                        .checked_add(
                            u64::from(bucket.stored_slots()) + u64::from(*tail_edge_slot_count),
                        )
                        .expect("batch hole-fill end overflow"),
                ),
                _ => None,
            };
            if let Some(end) = edge_end {
                let slots = u32::try_from(
                    end.checked_sub(*edge_base)
                        .expect("batch edge span precedes its vertex"),
                )
                .expect("batch vertex span must fit u32");
                *projected = projected.with_stored_slots(projected.stored_slots.max(slots));
            }
            let _updated_degree = bucket
                .degree
                .checked_add(res.edge_slot_count)
                .expect("reserve guaranteed degree overflow safety");
            match &res.destination {
                RunDestination::Slab { .. } => {
                    let _updated_stored_slots = bucket
                        .stored_slots()
                        .checked_add(res.edge_slot_count)
                        .expect("reserve guaranteed stored_slots overflow safety");
                    if res.inline_property_width > 0 {
                        let _updated_inline_property_bytes_slots = bucket
                            .inline_property_bytes_slab_slots()
                            .checked_add(res.edge_slot_count)
                            .expect("reserve guaranteed inline property bytes slab slot overflow safety");
                    }
                }
                RunDestination::SlabHoles {
                    tail_edge_slot_count,
                    ..
                } => {
                    // Hole-fill reuses tombstoned slots (already counted in
                    // `stored_slots`); only the tail remainder grows the stored
                    // window. Inline-property bytes are keyed by live ordinal, so
                    // the byte-slot count still grows by the full run size.
                    let _updated_stored_slots = bucket
                        .stored_slots()
                        .checked_add(*tail_edge_slot_count)
                        .expect("reserve guaranteed stored_slots overflow safety");
                    if res.inline_property_width > 0 {
                        let _updated_inline_property_bytes_slots = bucket
                            .inline_property_bytes_slab_slots()
                            .checked_add(res.edge_slot_count)
                            .expect("reserve guaranteed inline property bytes slab slot overflow safety");
                    }
                }
                RunDestination::ExpandedSlab { edge_log_len, .. } => {
                    let _updated_degree = bucket
                        .degree
                        .checked_add(res.edge_slot_count)
                        .expect("reserve guaranteed expanded degree overflow safety");
                    let _updated_stored_slots = bucket
                        .stored_slots()
                        .checked_add(*edge_log_len)
                        .and_then(|x| x.checked_add(res.edge_slot_count))
                        .expect("reserve guaranteed expanded stored_slots overflow safety");
                }
                RunDestination::RelocatedSlab { edge_log_len, .. } => {
                    let _updated_stored_slots = bucket
                        .stored_slots()
                        .checked_add(*edge_log_len)
                        .and_then(|x| x.checked_add(res.edge_slot_count))
                        .expect("reserve guaranteed relocated stored_slots overflow safety");
                    if res.inline_property_width > 0 {
                        let _updated_inline_property_bytes_slots = bucket
                            .inline_property_bytes_slab_slots()
                            .checked_add(res.edge_slot_count)
                            .expect("reserve guaranteed relocated inline property bytes slot overflow safety");
                    }
                }
                RunDestination::Tree { .. } => {
                    // Tree mode: descriptor updates (stored_slots +=
                    // edge_slot_count, degree += edge_slot_count)
                    // happen at commit time when the descriptor is
                    // published. The reservation only carries the
                    // computed counts.
                }
            }
        }

        let _next_num_edges = graph
            .edges
            .header()
            .num_edges
            .checked_add(total_edge_slots)
            .expect("reserve guaranteed num_edges overflow safety");

        // First pass: write all edge and inline property bytes.
        let mut edge_slots_written: u64 = 0;
        let edge_log_entries_written: u64 = 0;
        let mut inline_property_bytes_slots_written: u64 = 0;
        let inline_property_bytes_log_entries_written: u64 = 0;
        let mut locations = location_mode
            .captures()
            .then(|| Vec::with_capacity(self.plan.runs.iter().map(|r| r.edges.len()).sum()));

        for target in self.leaf_relocations.values() {
            graph
                .relocate_labeled_leaf_physical_block(VertexId::from(
                    target
                        .leaf
                        .saturating_mul(graph.edges.header().segment_size.max(1)),
                ))
                .expect("reserve guaranteed batch leaf relocation");
        }

        for (run, res) in self.plan.runs.iter().zip(self.runs.iter()) {
            match &res.destination {
                RunDestination::Slab {
                    edge_start_slot,
                    inline_property_bytes_offset,
                    inline_property_bytes_byte_count,
                } => {
                    let bucket = graph
                        .buckets
                        .read_label_bucket_slot(res.bucket_slot)
                        .expect("reserve found this slab bucket");
                    let logical_slot_base = u32::try_from(
                        edge_start_slot
                            .checked_sub(bucket.edge_start())
                            .expect("reserve guaranteed slab slot is within bucket"),
                    )
                    .expect("reserve guaranteed logical slot fits");
                    let mut edge_bytes = Vec::with_capacity(run.edges.len() * E::BYTES);
                    for e in &run.edges {
                        let mut buf = vec![0u8; E::BYTES];
                        e.edge.write_to(&mut buf);
                        edge_bytes.extend_from_slice(&buf);
                    }
                    graph
                        .edges
                        .write_slots_contiguous(*edge_start_slot, &edge_bytes)
                        .expect("reserve guaranteed edge slab capacity");

                    let inline_property_width = u64::from(res.inline_property_width);
                    let inline_property_bytes_offset = inline_property_bytes_offset.map(|base| {
                        base.checked_add(
                            u64::from(bucket.inline_property_bytes_slab_slots())
                                * inline_property_width,
                        )
                        .expect("reserved value prefix and append must fit")
                    });
                    if let Some(locations) = locations.as_mut() {
                        locations.extend(run.edges.iter().enumerate().map(|(offset, edge)| {
                            OneOrientationBatchLocation {
                                logical_ordinal: edge.logical_ordinal,
                                owner_vertex_id: edge.owner_vertex_id,
                                logical_slot: logical_slot_base
                                    .checked_add(offset as u32)
                                    .expect("reserve guaranteed logical slot"),
                                location: OneOrientationPhysicalLocation::Slab {
                                    edge_slot: edge_start_slot
                                        .checked_add(offset as u64)
                                        .expect("reserve guaranteed edge location"),
                                    inline_property_bytes_offset: inline_property_bytes_offset
                                        .as_ref()
                                        .map(|start| {
                                            start
                                                .checked_add(
                                                    (offset as u64)
                                                        .checked_mul(inline_property_width)
                                                        .expect(
                                                            "reserve guaranteed inline property location",
                                                        ),
                                                )
                                                .expect("reserve guaranteed inline property location")
                                        }),
                                },
                            }
                        }));
                    }

                    if res.inline_property_width > 0 {
                        let inline_property_bytes_offset = inline_property_bytes_offset.expect(
                            "reserve guaranteed an inline property byte offset for inline-property-bearing run",
                        );
                        let inline_property_bytes = run
                            .edges
                            .iter()
                            .flat_map(|e| e.edge.edge_inline_property_bytes().iter().copied())
                            .collect::<Vec<u8>>();
                        assert_eq!(
                            inline_property_bytes.len() as u64,
                            *inline_property_bytes_byte_count,
                            "reserve inline property byte count must match actual inline property bytes"
                        );
                        graph
                            .values
                            .write_bytes(inline_property_bytes_offset, &inline_property_bytes)
                            .expect("reserve guaranteed inline property bytes slab capacity");
                        inline_property_bytes_slots_written = inline_property_bytes_slots_written
                            .checked_add(u64::from(res.edge_slot_count))
                            .expect("reserve guaranteed inline property bytes slot count");
                    }

                    edge_slots_written = edge_slots_written
                        .checked_add(u64::from(res.edge_slot_count))
                        .expect("reserve guaranteed edge slot count");
                }
                RunDestination::SlabHoles {
                    edge_start_slot,
                    hole_window_indices,
                    tail_edge_slot_count,
                    write_strategy,
                    inline_property_bytes_offset,
                    inline_property_bytes_byte_count,
                } => {
                    let (edge_written, inline_property_bytes_written, run_locations) =
                        Self::commit_slab_holes_run::<M>(
                            graph,
                            run,
                            res,
                            *edge_start_slot,
                            hole_window_indices,
                            *tail_edge_slot_count,
                            *write_strategy,
                            *inline_property_bytes_offset,
                            *inline_property_bytes_byte_count,
                            location_mode,
                        );
                    if let Some(locations) = locations.as_mut() {
                        locations.extend(run_locations);
                    }
                    edge_slots_written = edge_slots_written
                        .checked_add(edge_written)
                        .expect("reserve guaranteed hole-fill edge slot count");
                    inline_property_bytes_slots_written = inline_property_bytes_slots_written
                        .checked_add(inline_property_bytes_written)
                        .expect("reserve guaranteed hole-fill inline property bytes slot count");
                }
                RunDestination::ExpandedSlab {
                    old_leaf_len,
                    new_leaf_len,
                    existing_bucket_slots,
                    edge_log_len,
                    existing_inline_property_bytes_slots,
                    inline_property_bytes_log_len,
                    inline_property_bytes_offset,
                    inline_property_bytes_byte_count,
                    ..
                } => {
                    let (edge_written, inline_property_bytes_written, run_locations) =
                        Self::commit_expanded_slab_run::<M>(
                            graph,
                            run,
                            res,
                            *old_leaf_len,
                            *new_leaf_len,
                            *existing_bucket_slots,
                            *edge_log_len,
                            *existing_inline_property_bytes_slots,
                            *inline_property_bytes_log_len,
                            *inline_property_bytes_offset,
                            *inline_property_bytes_byte_count,
                            location_mode,
                        );
                    if let Some(locations) = locations.as_mut() {
                        locations.extend(run_locations);
                    }
                    edge_slots_written = edge_slots_written
                        .checked_add(edge_written)
                        .expect("reserve guaranteed expanded edge slot count");
                    inline_property_bytes_slots_written = inline_property_bytes_slots_written
                        .checked_add(inline_property_bytes_written)
                        .expect("reserve guaranteed expanded inline property bytes slot count");
                }
                RunDestination::Tree {
                    tail_block_id,
                    tail_offset_bytes,
                    run_edge_count,
                } => {
                    // Plan 0321 Step 2 (minimal first slice): the
                    // tree run's edges are written contiguously into
                    // the tail block at `tail_offset_bytes`. The
                    // block is already populated up to
                    // `tail_offset_bytes`; we append after.
                    //
                    // Build the run's edge payload (4-byte target per
                    // edge, tree invariant). Then partial-write into
                    // the tail block.
                    //
                    // F-3 review (2026-09-02): the invariants below
                    // (E::BYTES == 4, tail block writable, no
                    // overflow) are guaranteed at reserve time and
                    // re-validated by the F-2 fingerprint check
                    // (slab↔tree flip is detected). We use
                    // `debug_assert!` to surface a violation in test
                    // builds while keeping the release binary
                    // infallible; the production path never trips
                    // these checks.
                    // One edge record per LTB slot: the slot is E::BYTES wide.
                    let mut edge_bytes = vec![0u8; *run_edge_count as usize * E::BYTES];
                    for (index, e) in run.edges.iter().enumerate() {
                        let slot = &mut edge_bytes[index * E::BYTES..(index + 1) * E::BYTES];
                        e.edge.write_to(slot);
                    }
                    if let Err(e) = graph.ltb().write_payload_partial(
                        *tail_block_id,
                        *tail_offset_bytes,
                        &edge_bytes,
                    ) {
                        // F-3: in release builds we can't propagate
                        // the error (the function returns the
                        // infallible `OneOrientationBatchResult`).
                        // Surface the failure in debug builds and
                        // log a warning in release. Production
                        // callers should pair reserve+commit in one
                        // message; this branch is unreachable in
                        // that path.
                        debug_assert!(
                            false,
                            "reserve guaranteed tail block write capacity; got {e:?}"
                        );
                        eprintln!("WARN: Plan 0321 tree batch tail write failed at commit: {e:?}");
                    }
                    if let Some(locations) = locations.as_mut() {
                        locations.extend(run.edges.iter().enumerate().map(|(offset, edge)| {
                            let logical_slot = res
                                .bucket_fingerprint
                                .stored_slots
                                .checked_add(offset as u32)
                                .expect("reserve guaranteed logical slot fits u32");
                            let byte_offset = (*tail_offset_bytes)
                                .checked_add(offset * E::BYTES)
                                .expect("reserve guaranteed byte offset fits usize");
                            OneOrientationBatchLocation {
                                logical_ordinal: edge.logical_ordinal,
                                owner_vertex_id: edge.owner_vertex_id,
                                logical_slot,
                                location: OneOrientationPhysicalLocation::Tree {
                                    tail_block_id: *tail_block_id,
                                    byte_offset,
                                },
                            }
                        }));
                    }
                    edge_slots_written = edge_slots_written
                        .checked_add(*run_edge_count as u64)
                        .expect("reserve guaranteed tree edge slot count");
                }
                RunDestination::RelocatedSlab {
                    leaf,
                    new_leaf_len,
                    existing_bucket_slots,
                    edge_log_len,
                    existing_inline_property_bytes_slots,
                    inline_property_bytes_log_len,
                    inline_property_bytes_offset,
                    inline_property_bytes_byte_count,
                } => {
                    let (edge_written, inline_property_bytes_written, run_locations) =
                        Self::commit_relocated_slab_run::<M>(
                            graph,
                            run,
                            res,
                            *leaf,
                            *new_leaf_len,
                            *existing_bucket_slots,
                            *edge_log_len,
                            *existing_inline_property_bytes_slots,
                            *inline_property_bytes_log_len,
                            *inline_property_bytes_offset,
                            *inline_property_bytes_byte_count,
                            location_mode,
                        );
                    if let Some(locations) = locations.as_mut() {
                        locations.extend(run_locations);
                    }
                    edge_slots_written = edge_slots_written
                        .checked_add(edge_written)
                        .expect("reserve guaranteed relocated edge slot count");
                    inline_property_bytes_slots_written = inline_property_bytes_slots_written
                        .checked_add(inline_property_bytes_written)
                        .expect("reserve guaranteed relocated inline property bytes slot count");
                }
            }
        }

        // Second pass: publish all bucket metadata and segment counts for paths
        // that did not already publish them during the first pass.
        for res in &self.runs {
            if matches!(
                res.destination,
                RunDestination::ExpandedSlab { .. } | RunDestination::RelocatedSlab { .. }
            ) {
                continue;
            }
            let vertex = graph.vertices.get(res.bucket_fingerprint.owner_vertex_id);
            let (bucket_slot, bucket) = match graph
                .find_bucket(
                    res.bucket_fingerprint.owner_vertex_id,
                    &vertex,
                    res.bucket_fingerprint.label_id,
                )
                .expect("reserve found this bucket")
            {
                BucketSearch::Found { slot, bucket } => (slot, bucket),
                BucketSearch::Missing { .. } => {
                    panic!("bucket disappeared between reserve and commit");
                }
            };

            let next_degree = bucket
                .degree
                .checked_add(res.edge_slot_count)
                .expect("reserve guaranteed degree overflow safety");
            let mut updated_bucket = bucket.with_degree_field(next_degree);
            match &res.destination {
                RunDestination::Slab {
                    inline_property_bytes_offset,
                    ..
                } => {
                    updated_bucket = updated_bucket.with_stored_slots(
                        bucket
                            .stored_slots()
                            .checked_add(res.edge_slot_count)
                            .expect("reserve guaranteed stored_slots overflow safety"),
                    );
                    if res.inline_property_width > 0 {
                        let new_offset = inline_property_bytes_offset.expect(
                            "reserve inline_property_bytes offset for inline property bytes span",
                        );
                        updated_bucket = updated_bucket.with_inline_property_bytes_slab_slots(
                            bucket
                                .inline_property_bytes_slab_slots()
                                .checked_add(res.edge_slot_count)
                                .expect("reserve guaranteed inline property bytes slab slot overflow safety"),
                        )
                        .with_inline_property_bytes_offset(new_offset);
                    }
                }
                RunDestination::SlabHoles {
                    inline_property_bytes_offset,
                    tail_edge_slot_count,
                    ..
                } => {
                    updated_bucket = updated_bucket.with_stored_slots(
                        bucket
                            .stored_slots()
                            .checked_add(*tail_edge_slot_count)
                            .expect("reserve guaranteed stored_slots overflow safety"),
                    );
                    if res.inline_property_width > 0 {
                        let new_offset = inline_property_bytes_offset.expect(
                            "reserve inline_property_bytes offset for inline property bytes span",
                        );
                        updated_bucket = updated_bucket.with_inline_property_bytes_slab_slots(
                            bucket
                                .inline_property_bytes_slab_slots()
                                .checked_add(res.edge_slot_count)
                                .expect("reserve guaranteed inline property bytes slab slot overflow safety"),
                        )
                        .with_inline_property_bytes_offset(new_offset);
                    }
                }
                RunDestination::ExpandedSlab { .. } => unreachable!(),
                RunDestination::RelocatedSlab { .. } => unreachable!(),
                RunDestination::Tree { run_edge_count, .. } => {
                    // Plan 0321 Step 2 (minimal first slice): tree
                    // mode bump. The descriptor's `stored_slots` and
                    // `degree` both grow by the run size (no inline
                    // property bytes, no tombstones, no hole reuse).
                    // The tail block write already happened in the
                    // first pass; this is the canonical publish.
                    updated_bucket = updated_bucket.with_stored_slots(
                        bucket
                            .stored_slots()
                            .checked_add(*run_edge_count)
                            .expect("reserve guaranteed tree stored_slots overflow safety"),
                    );
                    updated_bucket = updated_bucket.with_tree_mode(true);
                }
            }
            graph.publish_bucket_inline_property_bytes_span(bucket_slot, bucket, updated_bucket);

            // ADR 0096 §5 density parity: `actual` counts live slab/log edge
            // records. A tree run appends rows to LTB blocks, so it leaves the
            // leaf PMA `actual` untouched (see `tree_write` module header).
            let actual_delta = match &res.destination {
                RunDestination::Tree { .. } => 0,
                _ => i64::from(res.edge_slot_count),
            };
            if actual_delta != 0 {
                graph
                    .edges
                    .bump_vertex_segment_counts(
                        res.bucket_fingerprint.owner_vertex_id,
                        actual_delta,
                        0,
                    )
                    .expect("reserve guaranteed segment count overflow safety");
            }
        }

        // Publish the checked per-owner aggregates once, retaining any base and
        // cover already established by physical relocation.
        for (vid, (projected, _)) in vertex_updates {
            let vertex = graph.vertices.get(vid);
            graph.vertices.set(
                vid,
                &vertex
                    .with_stored_slots(vertex.stored_slots.max(projected.stored_slots))
                    .with_inline_property_bytes_allocated_bytes(
                        projected.inline_property_bytes_allocated_bytes(),
                    ),
            );
        }

        // Finally update the global edge count.
        let next_num_edges = graph
            .edges
            .header()
            .num_edges
            .checked_add(total_edge_slots)
            .expect("reserve guaranteed num_edges overflow safety");
        graph.edges.set_num_edges(next_num_edges);

        OneOrientationBatchResult {
            edge_slots_written: u32::try_from(edge_slots_written)
                .expect("reserve guaranteed total edge slots fit in u32"),
            edge_log_entries_written: u32::try_from(edge_log_entries_written)
                .expect("reserve guaranteed total edge log entries fit in u32"),
            inline_property_bytes_slots_written: u32::try_from(inline_property_bytes_slots_written)
                .expect("reserve guaranteed total inline property bytes slots fit in u32"),
            inline_property_bytes_log_entries_written: u32::try_from(
                inline_property_bytes_log_entries_written,
            )
            .expect("reserve guaranteed total inline property bytes log entries fit in u32"),
            locations,
        }
    }

    /// Commit one edge-only run after its leaf was relocated and existing logs
    /// were folded by the storage owner.
    fn commit_relocated_slab_run<M: Memory>(
        graph: &LabeledLaraGraph<E, M>,
        run: &OneOrientationBucketRun<E>,
        res: &BatchReservationRun,
        _leaf: u32,
        _new_leaf_len: u64,
        existing_bucket_slots: u32,
        edge_log_len: u32,
        existing_inline_property_bytes_slots: u32,
        inline_property_bytes_log_len: u32,
        inline_property_bytes_offset: Option<u64>,
        inline_property_bytes_byte_count: u64,
        location_mode: BatchLocationMode,
    ) -> (u64, u64, Vec<OneOrientationBatchLocation>) {
        let owner = res.bucket_fingerprint.owner_vertex_id;
        let vertex = graph.vertices.get(owner);
        let (bucket_slot, bucket) = match graph
            .find_bucket(owner, &vertex, res.bucket_fingerprint.label_id)
            .expect("relocated reservation bucket lookup")
        {
            BucketSearch::Found { slot, bucket } => (slot, bucket),
            BucketSearch::Missing { .. } => panic!("relocated bucket disappeared during commit"),
        };
        assert!(
            bucket.edge_spill_run() == crate::traits::NO_SPILL_RUN,
            "relocation must fold the existing edge overflow log before append"
        );
        assert_eq!(
            bucket.stored_slots(),
            existing_bucket_slots
                .checked_add(edge_log_len)
                .expect("reserve guaranteed relocated resident slots"),
            "relocation must fold the planned edge log before append"
        );
        let pending_start = checked_add_slot_index(
            bucket.edge_start(),
            u64::from(existing_bucket_slots)
                .checked_add(u64::from(edge_log_len))
                .expect("reserve guaranteed relocated pending start"),
        )
        .expect("reserve guaranteed relocated pending start");
        let mut edge_bytes = Vec::with_capacity(run.edges.len() * E::BYTES);
        for edge in &run.edges {
            let mut buf = vec![0u8; E::BYTES];
            edge.edge.write_to(&mut buf);
            edge_bytes.extend_from_slice(&buf);
        }
        graph
            .edges
            .write_slots_contiguous(pending_start, &edge_bytes)
            .expect("reserve guaranteed relocated slab capacity");

        let inline_property_width = u64::from(res.inline_property_width);
        let pending_inline_property_bytes_start = inline_property_bytes_offset.map(|offset| {
            offset
                .checked_add(
                    u64::from(existing_inline_property_bytes_slots)
                        .checked_add(u64::from(inline_property_bytes_log_len))
                        .expect("reserve guaranteed relocated inline property slot offset")
                        .checked_mul(inline_property_width)
                        .expect("reserve guaranteed relocated inline property byte offset"),
                )
                .expect("reserve guaranteed relocated inline property byte offset")
        });
        let locations = if location_mode.captures() {
            run.edges
                .iter()
                .enumerate()
                .map(|(offset, edge)| OneOrientationBatchLocation {
                    logical_ordinal: edge.logical_ordinal,
                    owner_vertex_id: edge.owner_vertex_id,
                    logical_slot: existing_bucket_slots
                        .checked_add(edge_log_len)
                        .and_then(|slot| slot.checked_add(offset as u32))
                        .expect("reserve guaranteed relocated logical slot"),
                    location: OneOrientationPhysicalLocation::Slab {
                        edge_slot: pending_start
                            .checked_add(offset as u64)
                            .expect("reserve guaranteed relocated edge location"),
                        inline_property_bytes_offset: pending_inline_property_bytes_start.map(
                            |start| {
                                start
                                    .checked_add(
                                        (offset as u64).checked_mul(inline_property_width).expect(
                                            "reserve guaranteed relocated inline property location",
                                        ),
                                    )
                                    .expect("reserve guaranteed relocated inline property location")
                            },
                        ),
                    },
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut updated_bucket = bucket
            .with_stored_slots(
                bucket
                    .stored_slots()
                    .checked_add(res.edge_slot_count)
                    .expect("reserve guaranteed relocated stored slot count"),
            )
            .with_degree_field(
                bucket
                    .degree
                    .checked_add(res.edge_slot_count)
                    .expect("reserve guaranteed relocated degree"),
            );
        let mut inline_property_bytes_slots_written = 0;
        if res.inline_property_width > 0 {
            let new_offset = inline_property_bytes_offset
                .expect("reserve guaranteed relocated inline property bytes offset");
            let width = u64::from(res.inline_property_width);
            if inline_property_bytes_log_len > 0 {
                let values_run = graph
                    .values
                    .spill_mint(bucket.values_spill_run())
                    .expect("reserve guaranteed a values run id");
                let chain = graph.values.spill_asc_indices(values_run);
                let width_bytes = usize::from(res.inline_property_width);
                let mut folded = vec![
                    0u8;
                    chain.len().checked_mul(width_bytes).expect(
                        "reserve guaranteed relocated inline property fold size"
                    )
                ];
                for (offset, log_idx) in chain.into_iter().enumerate() {
                    let start = offset
                        .checked_mul(width_bytes)
                        .expect("reserve guaranteed relocated inline property fold offset");
                    graph
                        .values
                        .spill_read(
                            values_run,
                            log_idx,
                            res.inline_property_width,
                            &mut folded[start..start + width_bytes],
                        )
                        .expect("reserve guaranteed relocated inline property log readability");
                }
                let folded_offset = new_offset
                    .checked_add(
                        u64::from(existing_inline_property_bytes_slots)
                            .checked_mul(width)
                            .expect("reserve guaranteed relocated inline property fold offset"),
                    )
                    .expect("reserve guaranteed relocated inline property fold offset");
                graph
                    .values
                    .write_bytes(folded_offset, &folded)
                    .expect("reserve guaranteed relocated inline property capacity");
            }
            let pending_bytes = run
                .edges
                .iter()
                .flat_map(|edge| edge.edge.edge_inline_property_bytes().iter().copied())
                .collect::<Vec<_>>();
            assert_eq!(
                pending_bytes.len() as u64,
                inline_property_bytes_byte_count,
                "reserve relocated inline property byte count must match input"
            );
            graph
                .values
                .write_bytes(
                    pending_inline_property_bytes_start
                        .expect("reserve guaranteed relocated pending inline property offset"),
                    &pending_bytes,
                )
                .expect("reserve guaranteed relocated pending inline property capacity");
            let total_slots = u64::from(existing_inline_property_bytes_slots)
                .checked_add(u64::from(inline_property_bytes_log_len))
                .and_then(|slots| slots.checked_add(u64::from(res.edge_slot_count)))
                .expect("reserve guaranteed relocated inline property slot count");
            updated_bucket = updated_bucket
                .with_inline_property_bytes_offset(new_offset)
                .with_inline_property_bytes_slab_slots(
                    u32::try_from(total_slots)
                        .expect("reserve guaranteed relocated inline property slots fit"),
                )
                .with_inline_property_bytes_log_head(-1);
            inline_property_bytes_slots_written = u64::from(inline_property_bytes_log_len)
                .checked_add(u64::from(res.edge_slot_count))
                .expect("reserve guaranteed relocated inline property slots written");
        }
        graph.publish_bucket_inline_property_bytes_span(bucket_slot, bucket, updated_bucket);
        (
            u64::from(res.edge_slot_count),
            inline_property_bytes_slots_written,
            locations,
        )
    }

    /// Commit one hole-fill run: write the run edges into the in-slab tombstone
    /// holes (per-hole writes or a whole-window rewrite per the reservation's
    /// strategy), append the tail remainder contiguously, insert inline property
    /// bytes at the reused live ordinals with the trailing shift, and return the
    /// written slot deltas plus per-edge locations (ADR 0052 §5/§9).
    fn commit_slab_holes_run<M: Memory>(
        graph: &LabeledLaraGraph<E, M>,
        run: &OneOrientationBucketRun<E>,
        res: &BatchReservationRun,
        edge_start_slot: u64,
        hole_window_indices: &[u32],
        tail_edge_slot_count: u32,
        write_strategy: SlabHoleWriteStrategy,
        inline_property_bytes_offset: Option<u64>,
        inline_property_bytes_byte_count: u64,
        location_mode: BatchLocationMode,
    ) -> (u64, u64, Vec<OneOrientationBatchLocation>) {
        let bucket = graph
            .buckets
            .read_label_bucket_slot(res.bucket_slot)
            .expect("reserve found this slab bucket");
        let window_len = u64::from(bucket.stored_slots());
        let holes = hole_window_indices.len();
        let mut edge_bytes = Vec::with_capacity(run.edges.len() * E::BYTES);
        for e in &run.edges {
            let mut buf = vec![0u8; E::BYTES];
            e.edge.write_to(&mut buf);
            edge_bytes.extend_from_slice(&buf);
        }
        match write_strategy {
            SlabHoleWriteStrategy::PerHole => {
                for (index, hole_index) in hole_window_indices.iter().enumerate() {
                    let off = index.saturating_mul(E::BYTES);
                    let slot = edge_start_slot
                        .checked_add(u64::from(*hole_index))
                        .expect("reserve guaranteed hole slot");
                    graph
                        .edges
                        .write_slot_bytes(slot, &edge_bytes[off..off + E::BYTES])
                        .expect("reserve guaranteed edge slab capacity");
                }
            }
            SlabHoleWriteStrategy::WindowRewrite => {
                let mut window = vec![0u8; (window_len as usize).saturating_mul(E::BYTES)];
                graph
                    .edges
                    .read_slots_contiguous(edge_start_slot, &mut window);
                for (index, hole_index) in hole_window_indices.iter().enumerate() {
                    let off = usize::try_from(*hole_index)
                        .expect("reserve guaranteed window offset fits in usize")
                        .saturating_mul(E::BYTES);
                    window[off..off + E::BYTES].copy_from_slice(
                        &edge_bytes
                            [index.saturating_mul(E::BYTES)..(index + 1).saturating_mul(E::BYTES)],
                    );
                }
                graph
                    .edges
                    .write_slots_contiguous(edge_start_slot, &window)
                    .expect("reserve guaranteed edge slab capacity");
            }
        }
        let tail_start = edge_start_slot
            .checked_add(window_len)
            .expect("reserve guaranteed tail slot");
        if tail_edge_slot_count > 0 {
            let tail_bytes = &edge_bytes[holes.saturating_mul(E::BYTES)..];
            graph
                .edges
                .write_slots_contiguous(tail_start, tail_bytes)
                .expect("reserve guaranteed edge slab capacity");
        }

        let mut run_locations = Vec::new();
        let inline_property_width = u64::from(res.inline_property_width);
        if location_mode.captures() {
            run_locations.extend(run.edges.iter().enumerate().map(|(offset, edge)| {
                let (logical_slot, edge_slot, live_ordinal) = if offset < holes {
                    let hole_index = hole_window_indices[offset];
                    (
                        hole_index,
                        edge_start_slot + u64::from(hole_index),
                        // New live ordinal: live slots before the hole plus
                        // already-filled holes before it equals the window index
                        // (holes are filled in window order).
                        u64::from(hole_index),
                    )
                } else {
                    let tail_offset = (offset - holes) as u64;
                    (
                        bucket
                            .stored_slots()
                            .checked_add(tail_offset as u32)
                            .expect("reserve guaranteed logical slot"),
                        tail_start
                            .checked_add(tail_offset)
                            .expect("reserve guaranteed edge location"),
                        // Tail edges follow every live slot in the window (old
                        // live plus all filled holes).
                        u64::from(bucket.degree)
                            .checked_add(holes as u64)
                            .and_then(|o| o.checked_add(tail_offset))
                            .expect("reserve guaranteed live ordinal"),
                    )
                };
                OneOrientationBatchLocation {
                    logical_ordinal: edge.logical_ordinal,
                    owner_vertex_id: edge.owner_vertex_id,
                    logical_slot,
                    location: OneOrientationPhysicalLocation::Slab {
                        edge_slot,
                        inline_property_bytes_offset: inline_property_bytes_offset.map(|start| {
                            start
                                .checked_add(
                                    live_ordinal
                                        .checked_mul(inline_property_width)
                                        .expect("reserve guaranteed inline property location"),
                                )
                                .expect("reserve guaranteed inline property location")
                        }),
                    },
                }
            }));
        }

        if res.inline_property_width > 0 {
            let offset = inline_property_bytes_offset.expect(
                "reserve guaranteed an inline property byte offset for inline-property-bearing run",
            );
            let width = usize::from(res.inline_property_width);
            let old_slots = usize::try_from(bucket.inline_property_bytes_slab_slots())
                .expect("reserve guaranteed inline property bytes slot count");
            let mut old_bytes = vec![0u8; old_slots.saturating_mul(width)];
            graph.values.read_bytes(offset, &mut old_bytes);
            let value_blocks: Vec<u8> = run
                .edges
                .iter()
                .flat_map(|e| e.edge.edge_inline_property_bytes().iter().copied())
                .collect();
            assert_eq!(
                value_blocks.len() as u64,
                inline_property_bytes_byte_count,
                "reserve inline property byte count must match actual inline property bytes"
            );
            // Rebuild the byte window keyed by live ordinal: existing blocks keep
            // their ordinals, hole-assigned edges insert at their reused ordinals,
            // and tail edges append after the previous live suffix (ADR 0052 §9
            // trailing shift). The insert position in the OLD block sequence is
            // `k - j` (old live blocks before the hole); the NEW live ordinal of
            // the same edge is `k` (location math above).
            let mut new_bytes = Vec::with_capacity(
                old_slots
                    .saturating_add(run.edges.len())
                    .saturating_mul(width),
            );
            let mut old_index = 0usize;
            for (index, hole_index) in hole_window_indices.iter().enumerate() {
                let hole_ordinal = usize::try_from(*hole_index)
                    .expect("reserve guaranteed window offset fits in usize")
                    .saturating_sub(index);
                new_bytes.extend_from_slice(
                    &old_bytes[old_index.saturating_mul(width)..hole_ordinal.saturating_mul(width)],
                );
                old_index = hole_ordinal;
                new_bytes.extend_from_slice(
                    &value_blocks[index.saturating_mul(width)..(index + 1).saturating_mul(width)],
                );
            }
            new_bytes.extend_from_slice(&old_bytes[old_index.saturating_mul(width)..]);
            new_bytes.extend_from_slice(&value_blocks[holes.saturating_mul(width)..]);
            graph
                .values
                .write_bytes(offset, &new_bytes)
                .expect("reserve guaranteed inline property bytes slab capacity");
        }

        (
            u64::from(res.edge_slot_count),
            if res.inline_property_width > 0 {
                u64::from(res.edge_slot_count)
            } else {
                0
            },
            run_locations,
        )
    }

    /// Commit one expanded-slab run.
    ///
    /// Commit one expanded-slab run after its owning vertex has already been
    /// rebalanced and the leaf block growth has been reserved.  Folds any
    /// existing overflow-log entries into the slab, writes the pending batch
    /// edges and inline property values, and updates the bucket metadata.  Returns the
    /// number of physical edge slab slots and inline property bytes slab slots written.
    fn commit_expanded_slab_run<M: Memory>(
        graph: &LabeledLaraGraph<E, M>,
        run: &OneOrientationBucketRun<E>,
        res: &BatchReservationRun,
        old_leaf_len: u64,
        new_leaf_len: u64,
        existing_bucket_slots: u32,
        edge_log_len: u32,
        existing_inline_property_bytes_slots: u32,
        inline_property_bytes_log_len: u32,
        inline_property_bytes_offset: Option<u64>,
        inline_property_bytes_byte_count: u64,
        location_mode: BatchLocationMode,
    ) -> (u64, u64, Vec<OneOrientationBatchLocation>) {
        let owner = res.bucket_fingerprint.owner_vertex_id;
        let vertex = graph.vertices.get(owner);
        let bucket_index =
            LabeledLaraGraph::<E, M>::labeled_bucket_descriptor_index(&vertex, res.bucket_slot)
                .expect("reserve found this bucket");
        let extra = edge_log_len + res.edge_slot_count;
        graph
            .rebalance_vertex_edge_span(owner, Some(bucket_index), extra, false)
            .expect("reserve guaranteed rebalance room in expanded leaf");

        // Re-read the vertex and bucket after rebalance.
        let vertex = graph.vertices.get(owner);
        let (new_bucket_slot, new_bucket) = match graph
            .find_bucket(owner, &vertex, res.bucket_fingerprint.label_id)
            .expect("reserve found this bucket")
        {
            BucketSearch::Found { slot, bucket } => (slot, bucket),
            BucketSearch::Missing { .. } => {
                panic!("bucket disappeared between reserve and commit")
            }
        };
        let edge_start = new_bucket.edge_start();

        // Fold the bucket's spill rows into the new slab space after the existing prefix
        // (ADR 0097 §7: the rows fit the expanded prefix, so they are copied in and the run is
        // released once the descriptor no longer references it). Every resident row moves -
        // the run's length is unchanged by deletes, so tombstones are resident too.
        let folded_run = if edge_log_len > 0 {
            let run = graph
                .edges
                .spill_mint(new_bucket.edge_spill_run())
                .expect("reserve counted this bucket's run rows");
            for (offset, index) in graph.edges.spill_asc_indices(run).into_iter().enumerate() {
                let edge = graph
                    .edges
                    .spill_edge_at(run, index)
                    .expect("reserve counted this bucket's run rows");
                let out_slot = checked_add_slot_index(
                    edge_start,
                    u64::from(existing_bucket_slots)
                        .checked_add(offset as u64)
                        .expect("reserve guaranteed folded log offset"),
                )
                .expect("reserve guaranteed slab slot addressable");
                graph
                    .edges
                    .write_slot(out_slot, edge)
                    .expect("reserve guaranteed slab capacity");
            }
            Some(run)
        } else {
            None
        };

        // Write pending batch edges into the slab after the folded log.
        let pending_start = checked_add_slot_index(
            edge_start,
            u64::from(existing_bucket_slots)
                .checked_add(u64::from(edge_log_len))
                .expect("reserve guaranteed pending start offset"),
        )
        .expect("reserve guaranteed pending start addressable");
        let mut edge_bytes = Vec::with_capacity(run.edges.len() * E::BYTES);
        for e in &run.edges {
            let mut buf = vec![0u8; E::BYTES];
            e.edge.write_to(&mut buf);
            edge_bytes.extend_from_slice(&buf);
        }
        graph
            .edges
            .write_slots_contiguous(pending_start, &edge_bytes)
            .expect("reserve guaranteed slab capacity");

        let inline_property_width = u64::from(res.inline_property_width);
        let pending_inline_property_bytes_start = inline_property_bytes_offset.map(|offset| {
            offset
                .checked_add(
                    u64::from(existing_inline_property_bytes_slots)
                        .checked_add(u64::from(inline_property_bytes_log_len))
                        .expect("reserve guaranteed pending inline property slot offset")
                        .checked_mul(inline_property_width)
                        .expect("reserve guaranteed pending inline property byte offset"),
                )
                .expect("reserve guaranteed pending inline_property_bytes offset")
        });
        let locations = if location_mode.captures() {
            run.edges
                .iter()
                .enumerate()
                .map(|(offset, edge)| OneOrientationBatchLocation {
                    logical_ordinal: edge.logical_ordinal,
                    owner_vertex_id: edge.owner_vertex_id,
                    logical_slot: existing_bucket_slots
                        .checked_add(edge_log_len)
                        .and_then(|slot| slot.checked_add(offset as u32))
                        .expect("reserve guaranteed expanded logical slot"),
                    location: OneOrientationPhysicalLocation::Slab {
                        edge_slot: pending_start
                            .checked_add(offset as u64)
                            .expect("reserve guaranteed edge location"),
                        inline_property_bytes_offset: pending_inline_property_bytes_start.map(
                            |start| {
                                start
                                    .checked_add(
                                        (offset as u64)
                                            .checked_mul(inline_property_width)
                                            .expect("reserve guaranteed inline property location"),
                                    )
                                    .expect("reserve guaranteed inline property location")
                            },
                        ),
                    },
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        // Update bucket edge metadata: stored_slots now covers prefix + folded log + pending.
        let updated_stored = existing_bucket_slots
            .checked_add(edge_log_len)
            .and_then(|x| x.checked_add(res.edge_slot_count))
            .expect("reserve guaranteed stored_slots overflow safety");
        let updated_degree = new_bucket
            .degree
            .checked_add(res.edge_slot_count)
            .expect("reserve guaranteed degree overflow safety");
        let mut updated_bucket = new_bucket
            .with_stored_slots(updated_stored)
            .with_degree_field(updated_degree)
            .without_edge_spill_run();

        // Fold and append inline property values when present.
        let mut inline_property_bytes_slots_written: u64 = 0;
        if res.inline_property_width > 0 {
            let width = u64::from(res.inline_property_width);
            let new_inline_property_bytes_offset = inline_property_bytes_offset
                .expect("reserve guaranteed inline property byte offset");

            // Fold existing inline property bytes overflow-log entries into the slab.
            if inline_property_bytes_log_len > 0 {
                let values_run = graph
                    .values
                    .spill_mint(new_bucket.values_spill_run())
                    .expect("reserve guaranteed a values run id");
                let chain = graph.values.spill_asc_indices(values_run);
                let width_bytes = usize::from(res.inline_property_width);
                let mut folded = vec![
                    0u8;
                    chain
                        .len()
                        .checked_mul(width_bytes)
                        .expect("reserve guaranteed inline property fold size")
                ];
                for (offset, log_idx) in chain.into_iter().enumerate() {
                    let start = offset
                        .checked_mul(width_bytes)
                        .expect("reserve guaranteed inline property fold offset");
                    graph
                        .values
                        .spill_read(
                            values_run,
                            log_idx,
                            res.inline_property_width,
                            &mut folded[start..start + width_bytes],
                        )
                        .expect("reserve guaranteed inline property bytes log readability");
                }
                let folded_offset = new_inline_property_bytes_offset
                    .checked_add(
                        u64::from(existing_inline_property_bytes_slots)
                            .checked_mul(width)
                            .expect("reserve guaranteed folded inline property offset"),
                    )
                    .expect("reserve guaranteed folded inline property offset");
                graph
                    .values
                    .write_bytes(folded_offset, &folded)
                    .expect("reserve guaranteed inline property bytes slab capacity");
            }

            // Write pending batch inline property values after the folded log.
            let pending_inline_property_bytes: Vec<u8> = run
                .edges
                .iter()
                .flat_map(|e| e.edge.edge_inline_property_bytes().iter().copied())
                .collect();
            assert_eq!(
                pending_inline_property_bytes.len() as u64,
                inline_property_bytes_byte_count,
                "reserve inline property byte count must match actual inline property bytes"
            );
            let pending_inline_property_bytes_offset = new_inline_property_bytes_offset
                .checked_add(
                    u64::from(existing_inline_property_bytes_slots)
                        .checked_add(u64::from(inline_property_bytes_log_len))
                        .expect("reserve guaranteed pending inline property slot offset")
                        .checked_mul(width)
                        .expect("reserve guaranteed pending inline property byte offset"),
                )
                .expect("reserve guaranteed pending inline_property_bytes offset");
            graph
                .values
                .write_bytes(
                    pending_inline_property_bytes_offset,
                    &pending_inline_property_bytes,
                )
                .expect("reserve guaranteed inline property bytes slab capacity");

            let total_inline_property_bytes_slots = u64::from(existing_inline_property_bytes_slots)
                .checked_add(u64::from(inline_property_bytes_log_len))
                .and_then(|s| s.checked_add(u64::from(res.edge_slot_count)))
                .expect("reserve guaranteed inline property bytes slot count");
            updated_bucket = updated_bucket
                .with_inline_property_bytes_offset(new_inline_property_bytes_offset)
                .with_inline_property_bytes_slab_slots(total_inline_property_bytes_slots as u32)
                .with_inline_property_bytes_log_head(-1);
            inline_property_bytes_slots_written = u64::from(inline_property_bytes_log_len)
                .checked_add(u64::from(res.edge_slot_count))
                .expect("reserve guaranteed inline property bytes slot count");
        }

        graph.publish_bucket_inline_property_bytes_span(
            new_bucket_slot,
            new_bucket,
            updated_bucket,
        );
        // The run's rows now live in the expanded prefix: release it (best-effort, like every
        // other span release on this path - a failed release leaks a class, never a row).
        if let Some(run) = folded_run {
            let _ = graph.edges.spill_release(run);
        }

        // Publish the leaf block growth in the segment counts.
        let d_total = i64::try_from(new_leaf_len)
            .expect("reserve guaranteed leaf len fits in i64")
            .checked_sub(
                i64::try_from(old_leaf_len).expect("reserve guaranteed leaf len fits in i64"),
            )
            .expect("reserve guaranteed leaf total delta");
        graph
            .edges
            .bump_vertex_segment_counts(owner, 0, d_total)
            .expect("reserve guaranteed segment count overflow safety");

        let edge_slots_written = u64::from(edge_log_len)
            .checked_add(u64::from(res.edge_slot_count))
            .expect("reserve guaranteed expanded slot count");
        (
            edge_slots_written,
            inline_property_bytes_slots_written,
            locations,
        )
    }
}

impl<E> BatchReservation<E>
where
    E: CsrEdge,
{
    /// Roll back the capacity and inline property bytes reservations made by this reservation.
    ///
    /// This consumes the token and restores the edge-store logical capacity
    /// and inline property bytes occupied tail to the values captured before
    /// `reserve_one_orientation_batch` mutated them.  Any inline_property_bytes bytes that
    /// were already appended are retired to the inline property free-list as reusable
    /// slack; the underlying stable-memory pages are not shrunk. Edge rows and
    /// prepared bucket descriptors are restored. Because the token is consumed,
    /// a reservation cannot be rolled back twice.
    pub(crate) fn rollback<M: Memory>(self, graph: &LabeledLaraGraph<E, M>) {
        assert_eq!(
            self.graph_marker,
            graph.instance_marker(),
            "reservation was produced by a different graph instance"
        );
        graph.rollback_leaf_expansions(&self.leaf_expansions);
        graph.rollback_edge_capacity(self.edge_capacity_before);
        graph.rollback_inline_property_bytes_tail(
            self.inline_property_bytes_tail_before,
            &self.inline_property_bytes_free_spans,
        );
        graph.rollback_prepared_buckets(&self.new_buckets);
    }
}

/// Result of a committed one-orientation batch write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneOrientationBatchResult {
    /// Number of edge slab slots written.
    pub edge_slots_written: u32,
    /// Number of edge overflow-log entries written.
    pub edge_log_entries_written: u32,
    /// Number of inline property bytes slab slots written.
    pub inline_property_bytes_slots_written: u32,
    /// Number of inline property bytes overflow-log entries written.
    pub inline_property_bytes_log_entries_written: u32,
    /// Exact physical locations when capture was requested. Aggregate-only
    /// commits leave this as `None` and do not allocate a location vector.
    pub locations: Option<Vec<OneOrientationBatchLocation>>,
}

/// Controls whether a batch commit materializes exact physical locations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchLocationMode {
    /// Return aggregate counters only; no location vector is allocated.
    AggregateOnly,
    /// Return one exact physical location for every committed edge.
    Capture,
}

impl BatchLocationMode {
    /// Return whether this mode requests exact physical locations.
    pub fn captures(self) -> bool {
        matches!(self, Self::Capture)
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    /// Convenience: reserve and commit a one-orientation batch in one call.
    ///
    /// This is useful for tests and internal callers that do not need to hold
    /// the reservation separately.  It still preserves the reserve-then-commit
    /// failure atomicity.
    pub(crate) fn insert_one_orientation_batch(
        &self,
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<OneOrientationBatchResult, OneOrientationBatchError> {
        let reservation = self.reserve_one_orientation_batch(plan)?;
        Ok(reservation.commit::<M>(self))
    }

    /// Convenience variant that returns exact physical locations.
    pub(crate) fn insert_one_orientation_batch_with_locations(
        &self,
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<OneOrientationBatchResult, OneOrientationBatchError> {
        let reservation = self.reserve_one_orientation_batch(plan)?;
        Ok(reservation.commit_with_locations::<M>(self))
    }
}

impl std::fmt::Display for OneOrientationBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPlan => write!(f, "empty batch plan has no defined no-op semantics"),
            Self::EmptyRun => write!(f, "empty bucket run in batch plan"),
            Self::NonIncreasingLogicalOrdinals => {
                write!(
                    f,
                    "bucket run edges are not in strictly increasing ordinal order"
                )
            }
            Self::DuplicateBucketRun => write!(f, "duplicate bucket runs in batch plan"),
            Self::MissingPinnedLeafForExpansion => {
                write!(f, "expanded-slab batch requires a pinned PMA leaf block")
            }
            Self::InlinePropertyBytesSpanRequiresRelocation => write!(
                f,
                "inline property bytes span is not at occupied tail and requires leaf relocation"
            ),
            Self::VertexNotLive(vid) => write!(f, "vertex {vid:?} is not live"),
            Self::BucketNotFound {
                owner_vertex_id,
                label_id,
            } => {
                write!(
                    f,
                    "bucket not found for vertex {owner_vertex_id:?} label {label_id:?}"
                )
            }
            Self::InlinePropertyBytesWidthMismatch {
                bucket_width,
                edge_width,
            } => {
                write!(
                    f,
                    "inline property byte width mismatch: bucket {bucket_width}, edge {edge_width}"
                )
            }
            Self::SlabCapacityExceeded => write!(f, "slab capacity exceeded"),
            Self::LogCapacityExceeded => write!(f, "log capacity exceeded"),
            Self::InlinePropertyBytesLengthMismatch {
                logical_ordinal,
                expected_width,
                actual_length,
            } => {
                write!(
                    f,
                    "inline property bytes length mismatch at ordinal {logical_ordinal}: expected {expected_width}, actual {actual_length}"
                )
            }
            Self::InlinePropertyBytesLogLengthMismatch {
                edge_log_len,
                inline_property_bytes_log_len,
            } => write!(
                f,
                "edge/inline-property overflow-log length mismatch: edge {edge_log_len}, inline property {inline_property_bytes_log_len}"
            ),
            Self::StorageError(e) => write!(f, "storage error: {e}"),
            Self::InvalidOrientationPair(detail) => write!(f, "invalid orientation pair: {detail}"),
            Self::TreeModeBucketRunUnsupported {
                owner_vertex_id,
                label_id,
            } => write!(
                f,
                "tree-mode bucket run unsupported: vertex {owner_vertex_id:?} label {label_id:?} (use scalar fallback)"
            ),
            Self::TinyBucketRunUnsupported {
                owner_vertex_id,
                label_id,
            } => write!(
                f,
                "tiny-mode bucket run unsupported: vertex {owner_vertex_id:?} label {label_id:?} (use scalar fallback)"
            ),
            Self::SpillRunDeferred {
                owner_vertex_id,
                label_id,
            } => write!(
                f,
                "spill-run bucket run deferred: vertex {owner_vertex_id:?} label {label_id:?} (use scalar fallback)"
            ),
            Self::TreeRunExceedsTailBlock {
                owner_vertex_id,
                label_id,
                run_bytes,
                tail_room_bytes,
            } => write!(
                f,
                "tree-mode run exceeds tail-block room: vertex {owner_vertex_id:?} label {label_id:?} run_bytes={run_bytes} tail_room_bytes={tail_room_bytes}"
            ),
        }
    }
}

impl std::error::Error for OneOrientationBatchError {}

impl OneOrientationBatchError {
    /// Whether the batch can safely fall back to the scalar owner boundary.
    pub fn is_recoverable_geometry(&self) -> bool {
        matches!(
            self,
            Self::MissingPinnedLeafForExpansion | Self::InlinePropertyBytesSpanRequiresRelocation
            // ADR 0096 §5: tiny-bucket runs fall back to scalar inserts (which
            // promote naturally). Scalar cost on small buckets is bounded; the
            // rejection happens pre-write with nothing reserved.
            | Self::TinyBucketRunUnsupported { .. }
            | Self::SpillRunDeferred { .. }
        )
    }
}

impl From<LabeledOperationError> for OneOrientationBatchError {
    fn from(err: LabeledOperationError) -> Self {
        Self::StorageError(err)
    }
}

fn storage_resize_error(e: crate::GrowFailed) -> OneOrientationBatchError {
    OneOrientationBatchError::StorageError(LabeledOperationError::Store(
        crate::lara::operation_error::LaraOperationError::ResizeFailed(e),
    ))
}

#[cfg(test)]
mod tests {
    use crate::LabeledLaraGraph;
    use crate::VertexId;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::graph::test_support::{
        InlinePropertyTestEdge, TestEdge as GraphTestEdge, inline_property_test_graph_segment16,
        inline_property_test_graph_with_capacity, promote_bucket_to_slab, retained_compaction_rows,
        retained_compaction_value, test_graph_with_default,
    };
    use crate::labeled::graph::{BucketSearch, OutEdgeOrder};
    use crate::labeled::record::LabeledVertex;
    use crate::traits::CsrVertex;

    use super::{
        BidirectionalBatchPlan, OneOrientationBatchEdge, OneOrientationBatchError,
        OneOrientationBatchLocation, OneOrientationBatchPlan, OneOrientationBucketRun,
        OneOrientationPhysicalLocation, PreflightRun, SlabHoleWriteStrategy,
    };

    fn segment16_graph() -> LabeledLaraGraph<GraphTestEdge, crate::VectorMemory> {
        use crate::test_support::vector_memory;
        LabeledLaraGraph::<GraphTestEdge, crate::VectorMemory>::new_with_segment_size(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::directed_from_index(1),
            16,
        )
        .unwrap()
    }

    fn one_edge_plan(
        owner: VertexId,
        neighbor: VertexId,
        label: BucketLabelKey,
    ) -> OneOrientationBatchPlan<GraphTestEdge> {
        OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: owner,
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: owner,
                    neighbor_vertex_id: neighbor,
                    label_id: label,
                    edge: GraphTestEdge {
                        target: u32::from(neighbor),
                    },
                }],
            }],
        }
    }

    fn seed_batch_value_bucket<M: ic_stable_structures::Memory>(
        graph: &LabeledLaraGraph<InlinePropertyTestEdge, M>,
        owner: VertexId,
        targets: &[u32],
        width: u16,
    ) {
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(owner, label, width)
            .unwrap();
        for &target in targets {
            graph
                .insert_edge_skip_leaf_cascade(
                    owner,
                    label,
                    InlinePropertyTestEdge::with_bytes(
                        target,
                        &retained_compaction_value(target, width),
                    ),
                    super::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph.compact_vertex_edge_span(owner, 0).unwrap();
        graph
            .rewrite_vertex_edge_span(owner, Some(0), 2, true, None)
            .unwrap();
    }

    fn batch_value_run(
        owner: VertexId,
        targets: &[u32],
        width: u16,
    ) -> OneOrientationBucketRun<InlinePropertyTestEdge> {
        let label = BucketLabelKey::from_raw(2);
        OneOrientationBucketRun {
            owner_vertex_id: owner,
            label_id: label,
            inline_property_width: width,
            placement: super::EdgePlacementPolicy::Insertion,
            edges: targets
                .iter()
                .enumerate()
                .map(|(ordinal, &target)| OneOrientationBatchEdge {
                    logical_ordinal: ordinal as u32,
                    owner_vertex_id: owner,
                    neighbor_vertex_id: VertexId::from(target),
                    label_id: label,
                    edge: InlinePropertyTestEdge::with_bytes(
                        target,
                        &retained_compaction_value(target, width),
                    ),
                })
                .collect(),
        }
    }

    fn check_batch_value_span_relocation(width: u16, placement: super::EdgePlacementPolicy) {
        let graph = inline_property_test_graph_with_capacity(256);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let peer = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        seed_batch_value_bucket(&graph, src, &[11, 22, 33, 44], width);
        if placement == super::EdgePlacementPolicy::Unordered {
            graph.remove_edge_at_slot(src, label, 1).unwrap().unwrap();
        }
        seed_batch_value_bucket(&graph, peer, &[77], width);
        let vertex = graph.vertices.get(src);
        let slot = graph.find_bucket_slot(&vertex, label).unwrap().unwrap();
        let before = graph.buckets.read_label_bucket_slot(slot).unwrap();
        assert_eq!(before.edge_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_eq!(before.values_spill_run(), crate::traits::NO_SPILL_RUN);
        let stats = graph.inline_property_bytes_storage_stats().unwrap();
        let old_len = u64::from(before.inline_property_bytes_slab_slots()) * u64::from(width);
        assert!(old_len > 0);
        assert!(before.inline_property_bytes_offset() + old_len < stats.slab_occupied_tail);
        let mut run = batch_value_run(src, &[55, 66], width);
        run.placement = placement;
        let plan = OneOrientationBatchPlan { runs: vec![run] };
        let preflight = graph
            .preflight_run(&plan.runs[0], &mut Default::default(), &Default::default())
            .unwrap();
        assert_eq!(
            preflight.inline_property_bytes_allocation,
            Some(super::InlinePropertyBytesAllocationKind::Reallocated {
                offset: before.inline_property_bytes_offset(),
                had_bytes: old_len,
                new_bytes: old_len + 2 * u64::from(width),
            })
        );
        let reservation = graph.reserve_one_orientation_batch(&plan).unwrap();
        let (targets, pending_slots) = if placement == super::EdgePlacementPolicy::Insertion {
            assert!(matches!(
                reservation.runs[0].destination,
                super::RunDestination::Slab { .. }
            ));
            (vec![11, 22, 33, 44, 55, 66], [4, 5])
        } else {
            assert!(
                matches!(&reservation.runs[0].destination, super::RunDestination::SlabHoles {
                hole_window_indices, tail_edge_slot_count: 1, ..
            } if hole_window_indices == &[1])
            );
            (vec![11, 55, 33, 44, 66], [1, 4])
        };
        let result = reservation.commit_with_locations(&graph);
        assert_eq!(result.edge_slots_written, 2);
        assert_eq!(result.inline_property_bytes_slots_written, 2);
        let after = graph.buckets.read_label_bucket_slot(slot).unwrap();
        assert_ne!(
            after.inline_property_bytes_offset(),
            before.inline_property_bytes_offset()
        );
        assert_eq!(after.edge_start(), before.edge_start());
        assert_eq!(after.degree(), targets.len() as u32);
        assert_eq!(
            graph.vertices.get(src).degree(),
            vertex.degree(),
            "batch append preserves the descriptor count"
        );
        assert_eq!(after.stored_slots(), targets.len() as u32);
        assert_eq!(
            after.inline_property_bytes_slab_slots(),
            targets.len() as u32
        );
        assert_eq!(after.edge_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_eq!(after.values_spill_run(), crate::traits::NO_SPILL_RUN);
        let check_rows = || {
            for (owner, expected_targets) in [(src, targets.as_slice()), (peer, &[77][..])] {
                let mut expected: Vec<_> = expected_targets
                    .iter()
                    .enumerate()
                    .map(|(slot, &target)| {
                        (
                            slot as u32,
                            target,
                            retained_compaction_value(target, width),
                        )
                    })
                    .collect();
                for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
                    if order == OutEdgeOrder::Descending {
                        expected.reverse();
                    }
                    assert_eq!(
                        retained_compaction_rows(&graph, owner, label, order),
                        expected
                    );
                }
            }
        };
        check_rows();
        let locations = result.locations.unwrap();
        assert_eq!(locations.len(), 2);
        for (ordinal, location) in locations.iter().enumerate() {
            assert_eq!(location.logical_ordinal, ordinal as u32);
            assert_eq!(location.owner_vertex_id, src);
            assert_eq!(location.logical_slot, pending_slots[ordinal]);
            assert_eq!(
                location.location,
                OneOrientationPhysicalLocation::Slab {
                    edge_slot: after.edge_start() + u64::from(pending_slots[ordinal]),
                    inline_property_bytes_offset: Some(
                        after.inline_property_bytes_offset()
                            + u64::from(pending_slots[ordinal]) * u64::from(width)
                    ),
                }
            );
        }
        let committed = graph.inline_property_bytes_storage_stats().unwrap();
        assert_eq!(
            committed.free_bytes,
            stats.free_bytes + old_len,
            "replaced source must be retired"
        );
        assert_eq!(
            committed.allocated_bytes,
            stats.allocated_bytes + 2 * u64::from(width)
        );
        assert_eq!(
            committed.slab_occupied_tail,
            committed.allocated_bytes + committed.free_bytes
        );
        assert_eq!(
            graph
                .vertices
                .get(src)
                .inline_property_bytes_allocated_bytes(),
            vertex.inline_property_bytes_allocated_bytes() + 2 * u64::from(width)
        );
        assert_eq!(
            graph.vertices.get(src).stored_slots,
            vertex.stored_slots,
            "append within reserved edge headroom must not enlarge the vertex cover"
        );
        let reused = graph.values.allocate_byte_span(old_len).unwrap();
        assert_eq!(reused, before.inline_property_bytes_offset());
        assert_eq!(
            graph.values.header().slab_occupied_tail,
            committed.slab_occupied_tail
        );
        graph
            .values
            .write_bytes(reused, &vec![0xcc; old_len as usize])
            .unwrap();
        check_rows();
        graph.values.retire_byte_span(reused, old_len).unwrap();
        assert_eq!(
            graph.inline_property_bytes_storage_stats().unwrap(),
            committed
        );
    }

    #[test]
    fn batch_value_span_relocation_slab_8() {
        check_batch_value_span_relocation(8, super::EdgePlacementPolicy::Insertion);
    }

    #[test]
    fn batch_value_span_relocation_slab_33() {
        check_batch_value_span_relocation(33, super::EdgePlacementPolicy::Insertion);
    }

    #[test]
    fn batch_value_span_relocation_holes_8() {
        check_batch_value_span_relocation(8, super::EdgePlacementPolicy::Unordered);
    }

    #[test]
    fn batch_value_span_relocation_holes_33() {
        check_batch_value_span_relocation(33, super::EdgePlacementPolicy::Unordered);
    }

    #[test]
    fn batch_value_span_multiple_runs_share_vertex_counts() {
        let graph = inline_property_test_graph_segment16();
        let owner = VertexId::from(0);
        let mut runs = Vec::new();
        for (raw, target) in [(2, 11), (4, 22)] {
            let label = BucketLabelKey::from_raw(raw);
            graph
                .ensure_label_bucket_inline_property_byte_width(owner, label, 8)
                .unwrap();
            let mut run = batch_value_run(owner, &[target], 8);
            run.label_id = label;
            run.edges[0].label_id = label;
            runs.push(run);
        }
        graph
            .rewrite_vertex_edge_span(owner, None, 2, true, None)
            .unwrap();
        let before = graph.vertices.get(owner);
        let plan = OneOrientationBatchPlan { runs };
        let reservation = graph.reserve_one_orientation_batch(&plan).unwrap();
        assert_eq!(reservation.runs.len(), 2);
        assert!(
            reservation
                .runs
                .iter()
                .all(|run| matches!(run.destination, super::RunDestination::Slab { .. }))
        );
        reservation.commit(&graph);
        let after = graph.vertices.get(owner);
        assert_eq!(after.degree(), 2);
        assert_eq!(after.inline_property_bytes_allocated_bytes(), 16);
        assert_eq!(after.base_slot_start(), before.base_slot_start());
        assert_eq!(after.stored_slots, before.stored_slots);
        for (raw, target) in [(2, 11), (4, 22)] {
            assert_eq!(
                retained_compaction_rows(
                    &graph,
                    owner,
                    BucketLabelKey::from_raw(raw),
                    OutEdgeOrder::Ascending
                ),
                vec![(0, target, retained_compaction_value(target, 8))]
            );
        }
        let stats = graph.inline_property_bytes_storage_stats().unwrap();
        assert_eq!(stats.live_bytes, 16);
        assert_eq!(stats.allocated_bytes, 16);
        assert_eq!(stats.slab_occupied_tail, 16);
        assert_eq!(stats.free_bytes, 0);
    }

    #[test]
    fn batch_value_span_reservation_failure_and_rollback() {
        use crate::test_support::failpoint_labeled_memories;
        use ic_stable_structures::Memory;
        let memories = failpoint_labeled_memories();
        let [a, b, c, d, e, f, g, h, i, j, k, records, m, n, o, p] = memories.clone();
        let graph = LabeledLaraGraph::<InlinePropertyTestEdge, _>::new_with_segment_size(
            a,
            b,
            c,
            d,
            e,
            f,
            g,
            h,
            i,
            j,
            k,
            records.clone(),
            m,
            n,
            o,
            p,
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::from_raw(1),
            16,
        )
        .unwrap();
        for _ in 0..33 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let owners = [0, 16, 32].map(VertexId::from);
        // Scratch is owned by this test, before the last live span so that the
        // mixed plan still projects a real GrowInPlace allocation.
        let scratch = graph.values.allocate_byte_span(8192).unwrap();
        seed_batch_value_bucket(&graph, owners[0], &[], 8);
        seed_batch_value_bucket(&graph, owners[1], &[11], 8);
        seed_batch_value_bucket(&graph, owners[2], &[22], 8);
        graph.values.retire_byte_span(scratch, 64).unwrap();
        let plan = OneOrientationBatchPlan {
            runs: vec![
                batch_value_run(owners[2], &[33], 8),
                batch_value_run(owners[1], &[44], 8),
                batch_value_run(owners[0], &[55], 8),
            ],
        };
        let allocations: Vec<_> = plan
            .runs
            .iter()
            .map(|run| {
                graph
                    .preflight_run(run, &mut Default::default(), &Default::default())
                    .unwrap()
                    .inline_property_bytes_allocation
                    .unwrap()
            })
            .collect();
        assert!(matches!(
            allocations.as_slice(),
            [
                super::InlinePropertyBytesAllocationKind::GrowInPlace { .. },
                super::InlinePropertyBytesAllocationKind::Reallocated { .. },
                super::InlinePropertyBytesAllocationKind::New { .. },
            ]
        ));
        let grows = records.grow_count();
        records.fail_at_grow(grows + 1);
        let boundary = (0..4000).find(|i| {
            if graph.values.reserve_retired_byte_spans(2).is_err() {
                return true;
            }
            graph
                .values
                .retire_byte_span(scratch + 128 + 2 * i, 1)
                .unwrap();
            false
        });
        assert!(
            boundary.is_some(),
            "bounded scratch must reach the release reservation boundary"
        );
        assert_eq!(records.grow_count(), grows + 1);
        graph.values.reserve_retired_byte_spans(1).unwrap();
        assert_eq!(
            records.grow_count(),
            grows + 1,
            "one release fits without growing records"
        );
        records.fail_at_grow(grows + 2);
        let tail = graph.values.header().slab_occupied_tail;
        let grow_only = OneOrientationBatchPlan {
            runs: vec![plan.runs[0].clone()],
        };
        graph
            .reserve_one_orientation_batch(&grow_only)
            .unwrap()
            .rollback(&graph);
        assert_eq!(
            records.grow_count(),
            grows + 1,
            "tail growth and rewind need no free-list records"
        );
        assert_eq!(graph.values.header().slab_occupied_tail, tail);
        assert_eq!(
            graph.values.peek_free_byte_span(8).unwrap().start_slot,
            scratch
        );
        let vertices = owners.map(|owner| graph.vertices.get(owner));
        let buckets = owners.map(|owner| {
            graph
                .read_vertex_label_buckets(&graph.vertices.get(owner))
                .unwrap()
        });
        let rows = owners.map(|owner| graph.out_edges(owner).unwrap());
        let stats = graph.inline_property_bytes_storage_stats().unwrap();
        let edge_header = graph.edges.header();
        let backing = memories.each_ref().map(|memory| {
            let mut bytes = vec![0; memory.byte_len()];
            memory.read(0, &mut bytes);
            bytes
        });
        let unchanged = |expected_stats| {
            assert_eq!(owners.map(|owner| graph.vertices.get(owner)), vertices);
            assert_eq!(
                owners.map(|owner| graph
                    .read_vertex_label_buckets(&graph.vertices.get(owner))
                    .unwrap()),
                buckets
            );
            assert_eq!(owners.map(|owner| graph.out_edges(owner).unwrap()), rows);
            assert_eq!(
                graph.inline_property_bytes_storage_stats().unwrap(),
                expected_stats
            );
            assert_eq!(graph.edges.header(), edge_header);
        };
        let unchanged_backing = || {
            unchanged(stats);
            for (memory, saved) in memories.iter().zip(&backing) {
                let mut bytes = vec![0; saved.len()];
                memory.read(0, &mut bytes);
                assert_eq!(&bytes, saved, "pre-existing backing changed on rejection");
            }
        };
        // Neither a too-wide slab run nor a grow failure may enter the inactive
        // leaf-expansion/relocation paths or perform a canonical write.
        let oversized = OneOrientationBatchPlan {
            runs: vec![batch_value_run(
                owners[2],
                &(100..612).collect::<Vec<_>>(),
                8,
            )],
        };
        assert!(
            matches!(graph.reserve_one_orientation_batch(&oversized).unwrap_err(),
            OneOrientationBatchError::SpillRunDeferred { owner_vertex_id, label_id }
            if owner_vertex_id == owners[2] && label_id == BucketLabelKey::from_raw(2))
        );
        unchanged_backing();
        records.fail_at_grow(records.grow_count() + 1);
        assert!(
            matches!(graph.reserve_one_orientation_batch(&plan).unwrap_err(),
            OneOrientationBatchError::StorageError(super::LabeledOperationError::Store(
                crate::lara::operation_error::LaraOperationError::ResizeFailed(crate::GrowFailed { current_size, delta })
            )) if current_size == records.size() && delta == 1)
        );
        assert_eq!(records.grow_count(), grows + 2);
        unchanged_backing();
        records.fail_never();
        let reservation = graph.reserve_one_orientation_batch(&plan).unwrap();
        assert!(
            reservation
                .runs
                .iter()
                .all(|run| matches!(run.destination, super::RunDestination::Slab { .. }))
        );
        assert_eq!(
            reservation.inline_property_bytes_free_spans,
            vec![
                crate::lara::edge::free_span::FreeSpan {
                    start_slot: scratch,
                    len: 16
                },
                crate::lara::edge::free_span::FreeSpan {
                    start_slot: scratch + 16,
                    len: 8
                },
            ]
        );
        assert_eq!(
            graph.values.header().slab_occupied_tail,
            stats.slab_occupied_tail + 8
        );
        reservation.rollback(&graph);
        unchanged(stats);
        assert_eq!(
            graph.values.peek_free_byte_span(64).unwrap(),
            crate::lara::edge::free_span::FreeSpan {
                start_slot: scratch,
                len: 64
            }
        );
        assert!(graph.values.allocate_byte_span_at(scratch, 64).unwrap());
        graph.values.write_bytes(scratch, &[0xcc; 64]).unwrap();
        assert_eq!(owners.map(|owner| graph.out_edges(owner).unwrap()), rows);
        graph.values.retire_byte_span(scratch, 64).unwrap();
        unchanged(stats);

        // Fail the third allocation after tail growth and one consumed free
        // prefix. The larger New span cannot fit the remaining 48-byte prefix.
        let mut failing = plan.clone();
        failing.runs[2] = batch_value_run(owners[0], &(55..63).collect::<Vec<_>>(), 8);
        let pending = graph
            .preflight_run(
                &failing.runs[2],
                &mut Default::default(),
                &Default::default(),
            )
            .unwrap();
        assert!(matches!(
            pending.destination,
            super::RunDestination::Slab { .. }
        ));
        assert_eq!(
            pending.inline_property_bytes_allocation,
            Some(super::InlinePropertyBytesAllocationKind::New { byte_len: 64 })
        );
        let free_spans = graph.values.free_byte_spans();
        crate::lara::edge_inline_property::force_inline_property_bytes_allocation_error_after(1);
        assert!(
            matches!(graph.reserve_one_orientation_batch(&failing).unwrap_err(),
            OneOrientationBatchError::StorageError(super::LabeledOperationError::Store(
                crate::lara::operation_error::LaraOperationError::ResizeFailed(crate::GrowFailed { current_size, delta: 0 })
            )) if current_size == stats.byte_capacity)
        );
        unchanged(stats);
        assert_eq!(graph.values.free_byte_spans(), free_spans);
        let retry = graph.reserve_one_orientation_batch(&plan).unwrap();
        let mut spans: Vec<_> = retry
            .runs
            .iter()
            .zip([16, 16, 8])
            .map(|(run, len)| {
                let super::RunDestination::Slab {
                    inline_property_bytes_offset: Some(offset),
                    ..
                } = run.destination
                else {
                    panic!("retry must use the same slab destinations");
                };
                (offset, offset + len)
            })
            .collect();
        spans.sort_unstable();
        assert!(
            spans.windows(2).all(|pair| pair[0].1 <= pair[1].0),
            "reserved value spans must remain disjoint after rollback: {spans:?}"
        );
        let result = retry.commit(&graph);
        assert_eq!(result.edge_slots_written, 3);
        assert_eq!(result.inline_property_bytes_slots_written, 3);
        for (owner, expected) in owners
            .into_iter()
            .zip([&[55][..], &[11, 44][..], &[22, 33][..]])
        {
            let actual: Vec<_> = graph
                .out_edges(owner)
                .unwrap()
                .into_iter()
                .map(|edge| (edge.target, edge.value))
                .collect();
            assert_eq!(
                actual,
                expected
                    .iter()
                    .map(|&target| (target, retained_compaction_value(target, 8)))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                graph
                    .vertices
                    .get(owner)
                    .inline_property_bytes_allocated_bytes(),
                expected.len() as u64 * 8
            );
        }
    }

    #[test]
    fn logical_batch_shape_rejects_malformed_self_loop() {
        let plan = BidirectionalBatchPlan::SelfLoop {
            forward: one_edge_plan(
                VertexId::from(0),
                VertexId::from(1),
                BucketLabelKey::undirected_from_index(1),
            ),
        };
        assert!(matches!(
            plan.validate(),
            Err(OneOrientationBatchError::InvalidOrientationPair(_))
        ));
    }

    #[test]
    fn recoverable_geometry_classification_excludes_fatal_errors() {
        assert!(OneOrientationBatchError::MissingPinnedLeafForExpansion.is_recoverable_geometry());
        assert!(
            OneOrientationBatchError::InlinePropertyBytesSpanRequiresRelocation
                .is_recoverable_geometry()
        );
        assert!(!OneOrientationBatchError::EmptyPlan.is_recoverable_geometry());
        assert!(!OneOrientationBatchError::SlabCapacityExceeded.is_recoverable_geometry());
        assert!(!OneOrientationBatchError::LogCapacityExceeded.is_recoverable_geometry());
        assert!(
            !OneOrientationBatchError::InvalidOrientationPair("broken".into())
                .is_recoverable_geometry()
        );
    }

    #[test]
    fn logical_batch_shape_rejects_mismatched_directed_pair() {
        let plan = BidirectionalBatchPlan::Directed {
            forward: one_edge_plan(
                VertexId::from(0),
                VertexId::from(1),
                BucketLabelKey::directed_from_index(2),
            ),
            reverse: one_edge_plan(
                VertexId::from(1),
                VertexId::from(2),
                BucketLabelKey::directed_from_index(2),
            ),
        };
        assert!(matches!(
            plan.validate(),
            Err(OneOrientationBatchError::InvalidOrientationPair(_))
        ));
    }

    #[test]
    fn merged_undirected_plan_validates_same_bucket_projections_once() {
        let label = BucketLabelKey::undirected_from_index(3);
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(2),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![
                    OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(3),
                        neighbor_vertex_id: VertexId::from(2),
                        label_id: label,
                        edge: GraphTestEdge { target: 2 },
                    },
                    OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(2),
                        neighbor_vertex_id: VertexId::from(3),
                        label_id: label,
                        edge: GraphTestEdge { target: 3 },
                    },
                    OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(2),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: GraphTestEdge { target: 1 },
                    },
                    OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(1),
                        neighbor_vertex_id: VertexId::from(2),
                        label_id: label,
                        edge: GraphTestEdge { target: 2 },
                    },
                ],
            }],
        };
        let plan = BidirectionalBatchPlan::Undirected {
            plan,
            pairs: vec![
                super::UndirectedBatchPair {
                    logical_ordinal: 0,
                    lower_owner_vertex_id: VertexId::from(2),
                    higher_owner_vertex_id: VertexId::from(3),
                    label_id: label,
                    inline_property_width: 0,
                },
                super::UndirectedBatchPair {
                    logical_ordinal: 1,
                    lower_owner_vertex_id: VertexId::from(1),
                    higher_owner_vertex_id: VertexId::from(2),
                    label_id: label,
                    inline_property_width: 0,
                },
            ],
        };
        assert!(plan.validate().is_ok());
        assert_eq!(plan.clone().into_orientations().len(), 1);
        let mut incomplete = plan.clone();
        if let BidirectionalBatchPlan::Undirected { pairs, .. } = &mut incomplete {
            pairs.pop();
        }
        assert!(matches!(
            incomplete.validate(),
            Err(OneOrientationBatchError::InvalidOrientationPair(_))
        ));
        assert!(matches!(
            plan,
            BidirectionalBatchPlan::Undirected { plan, .. } if plan.runs[0].edges.len() == 4
        ));
    }

    #[test]
    fn reserve_checks_total_edge_and_inline_property_bytes_slot_counts_fit_in_u32_results() {
        // `check_total_result_counts_fit_u32` rejects plans whose summed edge or
        // inline property bytes slot counts would not fit in the public `u32` result fields.
        // InlinePropertyBytes byte counts are irrelevant to this check; only slot counts matter.
        let check = |preflight: &[PreflightRun]| {
            super::LabeledLaraGraph::<GraphTestEdge, crate::VectorMemory>::check_total_result_counts_fit_u32(
                preflight,
            )
        };

        let ok = vec![PreflightRun {
            edge_slot_count: 10,
            inline_property_width: 4,
            ..Default::default()
        }];
        assert!(check(&ok).is_ok(), "small counts must fit in u32");

        let too_many_edge = vec![
            PreflightRun {
                edge_slot_count: 1,
                ..Default::default()
            },
            PreflightRun {
                edge_slot_count: u32::MAX,
                ..Default::default()
            },
        ];
        assert!(
            check(&too_many_edge).is_err(),
            "edge slot count overflowing u32 must be rejected"
        );

        let too_many_inline_property = vec![
            PreflightRun {
                edge_slot_count: u32::MAX / 2 + 1,
                inline_property_width: 4,
                ..Default::default()
            },
            PreflightRun {
                edge_slot_count: u32::MAX / 2 + 1,
                inline_property_width: 4,
                ..Default::default()
            },
        ];
        assert!(
            check(&too_many_inline_property).is_err(),
            "inline property bytes slot count overflowing u32 must be rejected"
        );

        let edge_at_max_u32 = vec![PreflightRun {
            edge_slot_count: u32::MAX,
            ..Default::default()
        }];
        assert!(
            check(&edge_at_max_u32).is_ok(),
            "exactly u32::MAX edge slots must be accepted"
        );

        let inline_property_at_max_u32 = vec![PreflightRun {
            edge_slot_count: u32::MAX,
            inline_property_width: 1,
            ..Default::default()
        }];
        assert!(
            check(&inline_property_at_max_u32).is_ok(),
            "exactly u32::MAX inline property bytes slots must be accepted"
        );
    }
    #[test]
    fn reserve_prepares_new_bucket_in_initial_slice() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: BucketLabelKey::directed_from_index(1),
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: BucketLabelKey::directed_from_index(1),
                    edge: GraphTestEdge { target: 1 },
                }],
            }],
        };

        let reservation = graph
            .reserve_one_orientation_batch(&plan)
            .expect("new bucket reservation");
        reservation.rollback(&graph);
        assert!(matches!(
            graph
                .find_bucket(
                    VertexId::from(0),
                    &graph.vertices.get(VertexId::from(0)),
                    BucketLabelKey::directed_from_index(1),
                )
                .expect("bucket lookup"),
            BucketSearch::Missing { .. }
        ));
    }

    #[test]
    fn reserve_rejects_duplicate_bucket_runs() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(1),
                GraphTestEdge { target: 1 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: BucketLabelKey::directed_from_index(1),
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: BucketLabelKey::directed_from_index(1),
                        edge: GraphTestEdge { target: 1 },
                    }],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: BucketLabelKey::directed_from_index(1),
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: BucketLabelKey::directed_from_index(1),
                        edge: GraphTestEdge { target: 1 },
                    }],
                },
            ],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::DuplicateBucketRun),
            "expected DuplicateBucketRun for duplicate runs, got {err}"
        );
    }

    #[test]
    fn reserve_rejects_out_of_order_ordinals() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(1),
                GraphTestEdge { target: 1 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: BucketLabelKey::directed_from_index(1),
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![
                    OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: BucketLabelKey::directed_from_index(1),
                        edge: GraphTestEdge { target: 1 },
                    },
                    OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: BucketLabelKey::directed_from_index(1),
                        edge: GraphTestEdge { target: 1 },
                    },
                ],
            }],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::NonIncreasingLogicalOrdinals),
            "expected NonIncreasingLogicalOrdinals for out-of-order ordinals, got {err}"
        );
    }

    #[test]
    fn batch_append_preserves_order_after_scalar_leaf_relocation() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        for _ in 0..34 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let label = BucketLabelKey::directed_from_index(2);
        // ADR 0096 §3b: four inline seeds plus an explicit promotion so the
        // source leaf pins and the bucket owns a slab span for relocation.
        // Seeds avoid target 2 (reserved for the batch edge below).
        for target in [1u32, 3, 4, 5] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    GraphTestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        promote_bucket_to_slab(&graph, VertexId::from(0), label);
        graph
            .insert_edge(
                VertexId::from(32),
                label,
                GraphTestEdge { target: 33 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let (old_start, old_len) = graph
            .labeled_leaf_physical_range(VertexId::from(0))
            .unwrap()
            .expect("source leaf must be pinned before relocation");
        let relocation_target = graph
            .plan_labeled_leaf_relocation(VertexId::from(0))
            .expect("relocation target must be computable without mutation");
        assert_eq!(relocation_target.old_start, old_start);
        assert_eq!(relocation_target.old_len, old_len);
        assert!(relocation_target.new_len > old_len);
        graph
            .relocate_labeled_leaf_physical_block(VertexId::from(0))
            .expect("scalar relocation must succeed");
        let (new_start, new_len) = graph
            .labeled_leaf_physical_range(VertexId::from(0))
            .unwrap()
            .expect("relocated leaf must remain pinned");
        assert_ne!(new_start, old_start);
        assert!(new_len >= old_len);

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Insertion,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(2),
                    label_id: label,
                    edge: GraphTestEdge { target: 2 },
                }],
            }],
        };
        graph
            .insert_one_orientation_batch(&plan)
            .expect("batch append after relocation");

        assert_eq!(
            graph
                .out_edges(VertexId::from(0))
                .unwrap()
                .into_iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>(),
            vec![1, 3, 4, 5, 2],
            "batch append must remain at the live tail after relocation"
        );
    }

    #[test]
    fn slab_batch_appends_after_interior_tombstone_without_reordering_live_edges() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        for _ in 0..5 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let label = BucketLabelKey::directed_from_index(1);
        // Four inline seeds plus an explicit promotion: the delete below must
        // take the slab tombstone path (K=4 keeps four edges inline).
        for target in 1..=4 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    GraphTestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        promote_bucket_to_slab(&graph, VertexId::from(0), label);
        graph
            .remove_edge_at_slot(VertexId::from(0), label, 1)
            .unwrap()
            .expect("middle edge must be tombstoned");

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Insertion,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(4),
                    label_id: label,
                    edge: GraphTestEdge { target: 5 },
                }],
            }],
        };
        graph
            .insert_one_orientation_batch(&plan)
            .expect("append after interior tombstone");

        let live_targets = graph
            .out_edges(VertexId::from(0))
            .unwrap()
            .into_iter()
            .map(|edge| edge.target)
            .collect::<Vec<_>>();
        assert_eq!(live_targets, vec![1, 3, 4, 5]);
    }

    // --- ADR 0052 §5 batch unordered hole-fill (Plan 0199) ---

    /// Chain-free slab-backed bucket with one interior tombstone: three edges
    /// inserted, the overflow log folded into the slab, and the middle edge
    /// (slot 1) removed so `stored_slots = 3`, `degree = 2`.
    fn slab_backed_bucket_with_interior_tombstone() -> (
        LabeledLaraGraph<GraphTestEdge, crate::VectorMemory>,
        BucketLabelKey,
    ) {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::directed_from_index(1);
        // Four inline seeds (K=4 keeps them inline), then an explicit
        // promotion so the delete below takes the slab tombstone path and the
        // span is exactly four slots (the helper's contract).
        for target in 1..=4u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    GraphTestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        promote_bucket_to_slab(&graph, VertexId::from(0), label);
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        graph
            .remove_edge_at_slot(VertexId::from(0), label, 1)
            .unwrap()
            .expect("middle edge must be tombstoned");
        (graph, label)
    }

    /// RAII guard forcing one slab-hole write strategy for the duration of a test.
    struct SlabHoleStrategyGuard(SlabHoleWriteStrategy);

    impl SlabHoleStrategyGuard {
        fn force(strategy: SlabHoleWriteStrategy) -> Self {
            super::SLAB_HOLE_WRITE_STRATEGY_OVERRIDE.with(|cell| cell.set(Some(strategy)));
            Self(strategy)
        }
    }

    impl Drop for SlabHoleStrategyGuard {
        fn drop(&mut self) {
            super::SLAB_HOLE_WRITE_STRATEGY_OVERRIDE.with(|cell| cell.set(None));
        }
    }

    #[test]
    fn unordered_batch_fills_slab_tombstone_holes_first() {
        let (graph, label) = slab_backed_bucket_with_interior_tombstone();
        let _guard = SlabHoleStrategyGuard::force(SlabHoleWriteStrategy::PerHole);
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(10),
                    label_id: label,
                    edge: GraphTestEdge { target: 10 },
                }],
            }],
        };
        let result = graph
            .insert_one_orientation_batch_with_locations(&plan)
            .expect("unordered batch must fill the slab tombstone");
        let bucket_slot = graph
            .find_bucket_slot(&graph.vertices.get(VertexId::from(0)), label)
            .unwrap()
            .unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert!(matches!(
            result.locations.as_deref(),
            Some([OneOrientationBatchLocation {
                logical_slot,
                location:
                    OneOrientationPhysicalLocation::Slab {
                        edge_slot,
                        inline_property_bytes_offset: None,
                    },
                ..
            }]) if *logical_slot == 1
                && *edge_slot == bucket.edge_start() + 1
        ));
        assert_eq!(
            graph
                .out_edges(VertexId::from(0))
                .unwrap()
                .iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>(),
            vec![1, 10, 3, 4],
            "the new edge must land in the tombstone hole at slot 1"
        );
        assert_eq!(
            bucket.stored_slots(),
            4,
            "hole reuse must not grow stored_slots"
        );
        assert_eq!(bucket.degree(), 4, "degree must grow by the run size");
    }

    #[test]
    fn unordered_batch_hole_fill_window_rewrite_matches_per_hole() {
        // Both write strategies publish identical canonical state and locations.
        let run_batch = |strategy: SlabHoleWriteStrategy| {
            let (graph, label) = slab_backed_bucket_with_interior_tombstone();
            let _guard = SlabHoleStrategyGuard::force(strategy);
            let plan = OneOrientationBatchPlan {
                runs: vec![OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label,
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(10),
                        label_id: label,
                        edge: GraphTestEdge { target: 10 },
                    }],
                }],
            };
            let result = graph
                .insert_one_orientation_batch_with_locations(&plan)
                .expect("hole-fill batch");
            let targets = graph
                .out_edges(VertexId::from(0))
                .unwrap()
                .iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>();
            let bucket_slot = graph
                .find_bucket_slot(&graph.vertices.get(VertexId::from(0)), label)
                .unwrap()
                .unwrap();
            let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
            (
                targets,
                bucket.stored_slots(),
                bucket.degree(),
                result.locations,
            )
        };
        let per_hole = run_batch(SlabHoleWriteStrategy::PerHole);
        let window = run_batch(SlabHoleWriteStrategy::WindowRewrite);
        assert_eq!(per_hole.0, window.0, "read-back order must match");
        assert_eq!(per_hole.1, window.1, "stored_slots must match");
        assert_eq!(per_hole.2, window.2, "degree must match");
        assert_eq!(per_hole.3, window.3, "locations must match");
    }

    #[test]
    fn unordered_batch_hole_fill_writes_inline_property_bytes_at_live_ordinal() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 2u16)
            .unwrap();
        for target in 1..=3u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        graph
            .remove_edge_at_slot(VertexId::from(0), label, 1)
            .unwrap()
            .expect("middle edge must be tombstoned");
        let _guard = SlabHoleStrategyGuard::force(SlabHoleWriteStrategy::PerHole);
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 2,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(21),
                    label_id: label,
                    edge: InlinePropertyTestEdge::with_bytes(21, &21u16.to_le_bytes()),
                }],
            }],
        };
        graph
            .insert_one_orientation_batch(&plan)
            .expect("hole-fill with inline property bytes");
        let edges = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(
            edges.iter().map(|edge| edge.target).collect::<Vec<_>>(),
            vec![1, 21, 3],
            "hole-fill must place the new edge in the reused slot"
        );
        assert_eq!(
            edges
                .iter()
                .map(|edge| edge.value[..usize::from(edge.inline_property_len)].to_vec())
                .collect::<Vec<_>>(),
            vec![
                1u16.to_le_bytes().to_vec(),
                21u16.to_le_bytes().to_vec(),
                3u16.to_le_bytes().to_vec(),
            ],
            "inline property bytes must stay aligned to the reused live ordinal (ADR 0052 §9)"
        );
    }

    #[test]
    fn unordered_batch_hole_fill_rollback_leaves_state_unchanged() {
        let (graph, label) = slab_backed_bucket_with_interior_tombstone();
        let _guard = SlabHoleStrategyGuard::force(SlabHoleWriteStrategy::PerHole);
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(10),
                    label_id: label,
                    edge: GraphTestEdge { target: 10 },
                }],
            }],
        };
        let reservation = graph
            .reserve_one_orientation_batch(&plan)
            .expect("reserve hole-fill");
        reservation.rollback(&graph);
        assert_eq!(
            graph
                .out_edges(VertexId::from(0))
                .unwrap()
                .iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>(),
            vec![1, 3, 4],
            "rollback must leave the tombstone and live edges untouched"
        );
        let bucket_slot = graph
            .find_bucket_slot(&graph.vertices.get(VertexId::from(0)), label)
            .unwrap()
            .unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert_eq!(bucket.stored_slots(), 4);
        assert_eq!(bucket.degree(), 3);
    }
}
