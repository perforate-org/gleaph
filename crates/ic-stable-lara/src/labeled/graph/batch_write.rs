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
//!    payload occupied tail to their pre-reserve values.  Any payload bytes that
//!    were already appended are retired to the payload free-list as reusable
//!    slack.  The underlying stable-memory pages are not shrunk.  Canonical
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
//! The current slice supports existing buckets through one of three paths:
//! - direct slab append when the bucket's window has room;
//! - per-leaf overflow-log append when the slab is full but the log has room;
//! - one in-place PMA leaf expansion when neither slab nor log can absorb the
//!   projected geometry. Fixed-width payload spans are folded and extended
//!   together with the edge slab when the existing span is reusable or grows at
//!   the occupied tail. Relocation to a brand-new physical block, new-bucket
//!   creation, default/unlabeled promotion, and maintenance compaction remain
//!   unsupported and fall back to the scalar path.

use crate::{
    VertexId,
    labeled::{bucket_label_key::BucketLabelKey, slot_index::checked_add_slot_index},
    traits::{CsrEdge, CsrEdgeTombstone},
};
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{BucketSearch, LabeledLaraGraph};

/// One physical half-edge inside a one-orientation batch plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneOrientationBatchEdge<E>
where
    E: CsrEdge,
{
    /// Stable logical ordinal from the caller's chunk; joins edge and payload.
    pub logical_ordinal: u32,
    /// Vertex that owns the CSR row.
    pub owner_vertex_id: VertexId,
    /// Neighbor vertex referenced by this half-edge.
    pub neighbor_vertex_id: VertexId,
    /// Storage label, including directedness bit.
    pub label_id: BucketLabelKey,
    /// Encoded edge bytes, including the inline payload if any.
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
    /// Physical byte width per inline value slot (`0` = no payload).
    pub inline_value_width: u16,
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

/// Exact physical location of one edge written by a one-orientation batch.
///
/// This is an internal fact owned by LARA.  Overflow-log entries retain their
/// leaf and log index because they do not have a slab slot until maintenance
/// folds them into a slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OneOrientationPhysicalLocation {
    /// Edge slab slot and optional inline payload byte offset.
    Slab {
        /// Edge slab slot.
        edge_slot: u64,
        /// Payload byte offset, when the edge carries an inline value.
        payload_byte_offset: Option<u64>,
    },
    /// Edge overflow-log entry and optional payload-log entry.
    OverflowLog {
        /// PMA leaf containing the log entry.
        leaf: u32,
        /// Edge overflow-log entry index.
        edge_entry_idx: u32,
        /// Payload overflow-log entry index, when present.
        payload_entry_idx: Option<u32>,
    },
}

/// Exact location keyed by the logical ordinal supplied in the batch plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OneOrientationBatchLocation {
    /// Logical ordinal from the input batch.
    pub logical_ordinal: u32,
    /// CSR row owner used to distinguish two same-orientation undirected halves.
    pub owner_vertex_id: VertexId,
    /// Exact LARA physical location.
    pub location: OneOrientationPhysicalLocation,
}

/// Opaque reservation produced by [`LabeledLaraGraph::reserve_one_orientation_batch`].
///
/// The fields are deliberately private; the valid uses are [`BatchReservation::commit`]
/// and [`BatchReservation::rollback`].  The token embeds a snapshot of the bucket
/// identity and expected occupancy at reservation time so commit can detect
/// tampering or stale state, plus the pre-reserve edge/payload allocator state so
/// rollback can restore it.
pub struct BatchReservation<E>
where
    E: CsrEdge,
{
    plan: OneOrientationBatchPlan<E>,
    runs: Vec<BatchReservationRun>,
    graph_marker: usize,
    edge_capacity_before: u64,
    payload_tail_before: u64,
    /// Cumulative in-place leaf expansions reserved for this batch; consumed on
    /// rollback to restore the free-span store shape.
    leaf_expansions: std::collections::BTreeMap<u32, LeafExpansionState>,
}

impl<E> BatchReservation<E>
where
    E: CsrEdge,
{
    /// Whether this reservation uses the pending-aware one-shot leaf expansion
    /// path rather than a direct slab or overflow-log destination.
    pub fn uses_expansion(&self) -> bool {
        self.runs
            .iter()
            .any(|run| matches!(run.destination, RunDestination::ExpandedSlab { .. }))
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
    /// Byte width per inline value slot (`0` = no payload).
    inline_value_width: u16,
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
    inline_value_slab_slots: u32,
    inline_value_offset: u64,
    inline_value_byte_width: u16,
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
            stored_slots: bucket.stored_slots,
            degree: bucket.degree,
            inline_value_slab_slots: bucket.inline_value_slab_slots(),
            inline_value_offset: bucket.inline_value_offset(),
            inline_value_byte_width: bucket.inline_value_byte_width(),
            vertex_stored_slots: vertex.stored_slots,
        }
    }
}

/// Error returned when a one-orientation batch write cannot complete.
#[derive(Debug)]
pub enum OneOrientationBatchError {
    /// The requested geometry is not yet supported by this slice.
    UnsupportedGeometry(String),
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
    PayloadByteWidthMismatch {
        /// Payload byte width declared by the label bucket.
        bucket_width: u16,
        /// Payload byte width carried by the edge.
        edge_width: u16,
    },
    /// The slab window cannot hold the planned run.
    SlabCapacityExceeded,
    /// The overflow log cannot hold the planned run.
    LogCapacityExceeded,
    /// A payload edge carried a byte length different from the declared width.
    PayloadLengthMismatch {
        /// Logical ordinal of the offending edge.
        logical_ordinal: u32,
        /// Declared inline value width.
        expected_width: u16,
        /// Actual payload byte length.
        actual_length: usize,
    },
    /// Existing edge and payload overflow logs are not aligned by ordinal.
    PayloadLogLengthMismatch {
        /// Number of resident edge overflow-log entries.
        edge_log_len: u32,
        /// Number of resident payload overflow-log entries.
        payload_log_len: u32,
    },
    /// A stable-memory reservation or write failed.
    StorageError(LabeledOperationError),
}

/// Physical destination chosen for one bucket-local run at preflight time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunDestination {
    /// Write into the bucket's contiguous slab window.
    Slab {
        /// First edge slab slot written for the run.
        edge_start_slot: u64,
        /// Byte offset in the payload slab where the run writes, if payload-bearing.
        payload_byte_offset: Option<u64>,
        /// Number of payload bytes reserved.
        payload_byte_count: u64,
    },
    /// Append to the bucket's edge overflow log and, when applicable, payload overflow log.
    OverflowLog {
        /// Index of the first reserved edge log entry.
        edge_log_start_idx: u32,
        /// Index of the first reserved payload log entry, if payload-bearing.
        payload_log_start_idx: Option<u32>,
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
        /// Payload slab slots already resident (including any folded payload log).
        existing_payload_slots: u32,
        /// Payload overflow-log entries that must be folded before writing.
        payload_log_len: u32,
        /// Byte offset in the payload slab where the bucket's expanded value span starts.
        payload_byte_offset: Option<u64>,
        /// Number of payload bytes reserved for the pending batch edges.
        payload_byte_count: u64,
    },
}

/// Intermediate preflight record for one run before any mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PayloadAllocationKind {
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
    inline_value_width: u16,
    payload_byte_count: u64,
    payload_allocation: Option<PayloadAllocationKind>,
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
            inline_value_width: 0,
            payload_byte_count: 0,
            payload_allocation: None,
            destination: RunDestination::Slab {
                edge_start_slot: 0,
                payload_byte_offset: None,
                payload_byte_count: 0,
            },
        }
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Validate a one-orientation batch plan and reserve the required capacity.
    ///
    /// This is the `reserve` step of the ADR 0045 boundary.  It performs all
    /// fallible validation before any canonical write.  If reserve fails after
    /// growing edge/payload backing capacity, it restores the logical edge-store
    /// capacity and payload occupied tail to their pre-reserve values.  Payload
    /// bytes that were already appended are retired to the payload free-list
    /// as reusable slack; the underlying stable-memory pages are not shrunk.
    /// Canonical adjacency and bucket metadata are never modified.  On success
    /// it returns an opaque [`BatchReservation`] token; the valid operations on
    /// that token are [`BatchReservation::commit`] and
    /// [`BatchReservation::rollback`].
    pub fn reserve_one_orientation_batch(
        &self,
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<BatchReservation<E>, OneOrientationBatchError> {
        // Phase 1: validate plan invariants without touching canonical state.
        Self::validate_plan_invariants(plan)?;

        // Phase 2: mutation-free preflight of every run.
        // Overflow-log runs share per-leaf segment capacity.  We maintain virtual
        // cursors so that multiple runs targeting the same leaf reserve disjoint
        // log ranges and the aggregate capacity check sees the whole batch.
        // Expanded-slab runs similarly share per-leaf physical block growth.
        let mut preflight = Vec::with_capacity(plan.runs.len());
        let mut max_edge_end_slot: u64 = 0;
        let mut edge_log_leaf_cursors: std::collections::BTreeMap<u32, u32> =
            std::collections::BTreeMap::new();
        let mut payload_log_leaf_cursors: std::collections::BTreeMap<u32, u32> =
            std::collections::BTreeMap::new();
        let mut leaf_expansion_cursors: std::collections::BTreeMap<u32, LeafExpansionState> =
            std::collections::BTreeMap::new();
        for run in &plan.runs {
            let p = self.preflight_run(
                run,
                &mut edge_log_leaf_cursors,
                &mut payload_log_leaf_cursors,
                &mut leaf_expansion_cursors,
            )?;
            if let RunDestination::Slab {
                edge_start_slot, ..
            } = p.destination
            {
                max_edge_end_slot =
                    max_edge_end_slot.max(edge_start_slot + u64::from(p.edge_slot_count));
            }
            preflight.push(p);
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
            let (leaf_start, _) = match self.labeled_leaf_physical_range(VertexId::from(start_vid))
            {
                Some(range) => range,
                None => {
                    Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                    return Err(OneOrientationBatchError::UnsupportedGeometry(
                        "expanded-slab batch requires a pinned PMA leaf block".into(),
                    ));
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
                Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                return Err(OneOrientationBatchError::LogCapacityExceeded);
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

        // Phase 2.5: prove that the total result counts fit in the public u32 fields.
        if let Err(err) = Self::check_total_result_counts_fit_u32(&preflight) {
            Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
            return Err(err);
        }

        // Phase 3: mutate allocator state for slab-bound runs only.  Overflow-log
        // runs reserve only ephemeral log capacity checked in preflight; they do not
        // touch the edge-store logical capacity or payload occupied tail before commit.
        let edge_capacity_before = self.edges.header().elem_capacity;
        if max_edge_end_slot > edge_capacity_before
            && let Err(err) = self.edges.set_elem_capacity(max_edge_end_slot)
        {
            Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
            return Err(storage_resize_error(err));
        }

        let payload_tail_before = self.values.header().slab_occupied_tail;
        let mut allocated_payload_offsets: Vec<Option<(u64, u64)>> =
            Vec::with_capacity(preflight.len());
        for p in &preflight {
            match p.destination {
                RunDestination::Slab { .. } => {
                    if let Some(allocation) = p.payload_allocation {
                        match allocation {
                            PayloadAllocationKind::Existing { offset } => {
                                allocated_payload_offsets.push(Some((offset, 0)));
                            }
                            PayloadAllocationKind::New { byte_len } => {
                                let actual_offset =
                                    self.values.append_byte_span(byte_len).map_err(|e| {
                                        Self::rollback_leaf_expansions(
                                            self,
                                            &leaf_expansion_cursors,
                                        );
                                        self.rollback_payload_tail(payload_tail_before);
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                allocated_payload_offsets.push(Some((actual_offset, byte_len)));
                            }
                            PayloadAllocationKind::GrowInPlace {
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
                                        self.rollback_payload_tail(payload_tail_before);
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                if !grown {
                                    Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                                    self.rollback_payload_tail(payload_tail_before);
                                    self.rollback_edge_capacity(edge_capacity_before);
                                    return Err(OneOrientationBatchError::UnsupportedGeometry(
                                        "payload span is not at occupied tail and cannot grow in place"
                                            .into(),
                                    ));
                                }
                                allocated_payload_offsets.push(Some((offset, new_bytes)));
                            }
                        }
                    } else {
                        allocated_payload_offsets.push(None);
                    }
                }
                RunDestination::OverflowLog { .. } => {
                    // Overflow-log runs do not allocate payload slab bytes; payload bytes
                    // are written directly into the payload overflow log at commit time.
                    allocated_payload_offsets.push(None);
                }
                RunDestination::ExpandedSlab { .. } => {
                    // Expanded-slab runs allocate any required payload slab bytes at
                    // reserve time, just like clean-slab runs, so rollback can retire
                    // the span if the batch aborts.
                    if let Some(allocation) = p.payload_allocation {
                        match allocation {
                            PayloadAllocationKind::Existing { offset } => {
                                allocated_payload_offsets.push(Some((offset, 0)));
                            }
                            PayloadAllocationKind::New { byte_len } => {
                                let actual_offset =
                                    self.values.append_byte_span(byte_len).map_err(|e| {
                                        Self::rollback_leaf_expansions(
                                            self,
                                            &leaf_expansion_cursors,
                                        );
                                        self.rollback_payload_tail(payload_tail_before);
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                allocated_payload_offsets.push(Some((actual_offset, byte_len)));
                            }
                            PayloadAllocationKind::GrowInPlace {
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
                                        self.rollback_payload_tail(payload_tail_before);
                                        self.rollback_edge_capacity(edge_capacity_before);
                                        storage_resize_error(e)
                                    })?;
                                if !grown {
                                    Self::rollback_leaf_expansions(self, &leaf_expansion_cursors);
                                    self.rollback_payload_tail(payload_tail_before);
                                    self.rollback_edge_capacity(edge_capacity_before);
                                    return Err(OneOrientationBatchError::UnsupportedGeometry(
                                        "payload span is not at occupied tail and cannot grow in place"
                                            .into(),
                                    ));
                                }
                                allocated_payload_offsets
                                    .push(Some((offset, new_bytes - had_bytes)));
                            }
                        }
                    } else {
                        allocated_payload_offsets.push(None);
                    }
                }
            }
        }

        // Phase 4: build the opaque reservation token from the preflight snapshot.
        // No fallible allocator calls remain from this point onward.
        let runs = preflight
            .into_iter()
            .zip(allocated_payload_offsets)
            .map(|(p, allocated)| {
                let destination = match p.destination {
                    RunDestination::Slab {
                        edge_start_slot, ..
                    } => {
                        let payload_byte_offset = if p.inline_value_width > 0 {
                            let (actual_offset, _allocated_len) = allocated
                                .expect("payload-bearing slab run must have an allocated offset");
                            if let Some(PayloadAllocationKind::Existing { offset }) =
                                p.payload_allocation
                            {
                                assert_eq!(
                                    actual_offset, offset,
                                    "existing payload offset must not change during allocation"
                                );
                            }
                            Some(actual_offset)
                        } else {
                            debug_assert!(
                                allocated.is_none(),
                                "edge-only slab run must have no payload allocation"
                            );
                            None
                        };
                        RunDestination::Slab {
                            edge_start_slot,
                            payload_byte_offset,
                            payload_byte_count: p.payload_byte_count,
                        }
                    }
                    RunDestination::OverflowLog {
                        edge_log_start_idx,
                        payload_log_start_idx,
                    } => RunDestination::OverflowLog {
                        edge_log_start_idx,
                        payload_log_start_idx,
                    },
                    RunDestination::ExpandedSlab {
                        leaf,
                        old_leaf_len,
                        new_leaf_len,
                        existing_bucket_slots,
                        edge_log_len,
                        existing_payload_slots,
                        payload_log_len,
                        ..
                    } => {
                        let payload_byte_offset = if p.inline_value_width > 0 {
                            let (actual_offset, _allocated_len) = allocated.expect(
                                "payload-bearing expanded-slab run must have an allocated offset",
                            );
                            Some(actual_offset)
                        } else {
                            debug_assert!(
                                allocated.is_none(),
                                "edge-only expanded-slab run must have no payload allocation"
                            );
                            None
                        };
                        RunDestination::ExpandedSlab {
                            leaf,
                            old_leaf_len,
                            new_leaf_len,
                            existing_bucket_slots,
                            edge_log_len,
                            existing_payload_slots,
                            payload_log_len,
                            payload_byte_offset,
                            payload_byte_count: p.payload_byte_count,
                        }
                    }
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
                    inline_value_width: p.inline_value_width,
                    destination,
                }
            })
            .collect();

        Ok(BatchReservation {
            plan: plan.clone(),
            runs,
            graph_marker: self.instance_marker(),
            edge_capacity_before,
            payload_tail_before,
            leaf_expansions: leaf_expansion_cursors,
        })
    }

    fn validate_plan_invariants(
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<(), OneOrientationBatchError> {
        if plan.runs.is_empty() {
            return Err(OneOrientationBatchError::UnsupportedGeometry(
                "empty batch plan has no defined no-op semantics".into(),
            ));
        }
        let mut seen_buckets = std::collections::BTreeSet::<(VertexId, BucketLabelKey)>::new();
        for run in &plan.runs {
            if run.edges.is_empty() {
                return Err(OneOrientationBatchError::UnsupportedGeometry(
                    "empty bucket run in batch plan".into(),
                ));
            }
            let mut prev_ordinal: Option<u32> = None;
            for e in &run.edges {
                if let Some(p) = prev_ordinal
                    && e.logical_ordinal <= p
                {
                    return Err(OneOrientationBatchError::UnsupportedGeometry(
                        "bucket run edges are not in strictly increasing ordinal order".into(),
                    ));
                }
                prev_ordinal = Some(e.logical_ordinal);
            }
            if !seen_buckets.insert((run.owner_vertex_id, run.label_id)) {
                return Err(OneOrientationBatchError::UnsupportedGeometry(
                    "duplicate bucket runs in batch plan".into(),
                ));
            }
        }
        Ok(())
    }

    /// Roll back payload tail growth performed during a partially-failed reserve.
    ///
    /// All payload allocations in this batch append at the occupied tail, so the
    /// new bytes are exactly `[original_tail, current_tail)`.  We retire that
    /// range to the free list and restore the header, turning the unused bytes
    /// into reusable slack.  Existing payload spans are untouched.  The
    /// underlying stable-memory pages are not shrunk.  If retiring the tail
    /// span fails, we panic: the allocator would be left in an inconsistent
    /// state and continuing is unsafe.
    fn rollback_payload_tail(&self, original_tail: u64) {
        let current_tail = self.values.header().slab_occupied_tail;
        if current_tail > original_tail {
            self.values
                .retire_byte_span(original_tail, current_tail - original_tail)
                .expect("reserve rollback must restore payload free-list consistency");
            self.values.reset_slab_occupied_tail(original_tail);
        }
    }

    /// Verify that the summed edge and payload slot counts of the preflight runs
    /// fit into the public `u32` result fields.
    ///
    /// This is pulled out so the boundary can be unit-tested directly: a batch
    /// with more than `u32::MAX` edge or payload slots is rejected at reserve time
    /// instead of truncating the commit result.
    fn check_total_result_counts_fit_u32(
        preflight: &[PreflightRun],
    ) -> Result<(), OneOrientationBatchError> {
        let total_edge_slots = preflight.iter().try_fold(0u64, |acc, p| {
            acc.checked_add(u64::from(p.edge_slot_count))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)
        })?;
        let total_payload_slots = preflight
            .iter()
            .filter(|p| p.inline_value_width > 0)
            .try_fold(0u64, |acc, p| {
                acc.checked_add(u64::from(p.edge_slot_count))
                    .ok_or(OneOrientationBatchError::SlabCapacityExceeded)
            })?;
        if u32::try_from(total_edge_slots).is_err() || u32::try_from(total_payload_slots).is_err() {
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

    fn preflight_run(
        &self,
        run: &OneOrientationBucketRun<E>,
        edge_log_leaf_cursors: &mut std::collections::BTreeMap<u32, u32>,
        payload_log_leaf_cursors: &mut std::collections::BTreeMap<u32, u32>,
        leaf_expansion_cursors: &mut std::collections::BTreeMap<u32, LeafExpansionState>,
    ) -> Result<PreflightRun, OneOrientationBatchError> {
        self.ensure_vertex(run.owner_vertex_id)
            .map_err(OneOrientationBatchError::from)?;
        let vertex = self.vertices.get(run.owner_vertex_id);
        let (bucket_slot, bucket) =
            match self.find_bucket(run.owner_vertex_id, &vertex, run.label_id)? {
                BucketSearch::Found { slot, bucket } => (slot, bucket),
                BucketSearch::Missing { .. } => {
                    return Err(OneOrientationBatchError::UnsupportedGeometry(
                        "new bucket creation is not supported in this slice".into(),
                    ));
                }
            };
        if run.inline_value_width != bucket.inline_value_byte_width() {
            return Err(OneOrientationBatchError::PayloadByteWidthMismatch {
                bucket_width: bucket.inline_value_byte_width(),
                edge_width: run.inline_value_width,
            });
        }

        let edge_start_slot =
            checked_add_slot_index(bucket.edge_start(), u64::from(bucket.stored_slots))
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
        // published alongside the new slab prefix and break edge/payload
        // ordinal alignment.
        if bucket.overflow_log_head() >= 0 || bucket.inline_value_log_head() >= 0 {
            return self.preflight_overflow_log_run(
                run,
                bucket_slot,
                bucket,
                edge_log_leaf_cursors,
                payload_log_leaf_cursors,
                leaf_expansion_cursors,
            );
        }
        if edge_end_slot > successor_start {
            return self.preflight_overflow_log_run(
                run,
                bucket_slot,
                bucket,
                edge_log_leaf_cursors,
                payload_log_leaf_cursors,
                leaf_expansion_cursors,
            );
        }

        // Verify every payload-bearing edge matches the declared width.  This
        // proves the commit-time assertion cannot fire for malformed input.
        let mut payload_byte_count: u64 = 0;
        if run.inline_value_width > 0 {
            let width = usize::from(run.inline_value_width);
            for e in &run.edges {
                let actual = e.edge.edge_inline_value_bytes().len();
                if actual != width {
                    return Err(OneOrientationBatchError::PayloadLengthMismatch {
                        logical_ordinal: e.logical_ordinal,
                        expected_width: run.inline_value_width,
                        actual_length: actual,
                    });
                }
            }
            payload_byte_count = u64::from(edge_slot_count)
                .checked_mul(u64::from(run.inline_value_width))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
        }

        // Compute payload destination and allocation kind without mutating the
        // payload store.  For a brand-new span we only record the required byte
        // length; the actual offset is determined by the allocator in Phase 3.
        let mut payload_allocation = None;
        if run.inline_value_width > 0 {
            let width = u64::from(run.inline_value_width);
            let total_payload_slots = u64::from(bucket.inline_value_slab_slots())
                .checked_add(u64::from(edge_slot_count))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let total_payload_bytes = total_payload_slots
                .checked_mul(width)
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let had_bytes = u64::from(bucket.inline_value_slab_slots())
                .checked_mul(width)
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let offset = bucket.inline_value_offset();
            let tail = self.values.header().slab_occupied_tail;
            let span_ends_at_tail = offset.checked_add(had_bytes).is_some_and(|end| end == tail);

            if had_bytes == 0 {
                payload_allocation = Some(PayloadAllocationKind::New {
                    byte_len: total_payload_bytes,
                });
            } else if span_ends_at_tail {
                payload_allocation = Some(PayloadAllocationKind::GrowInPlace {
                    offset,
                    had_bytes,
                    new_bytes: total_payload_bytes,
                });
            } else {
                return Err(OneOrientationBatchError::UnsupportedGeometry(
                    "payload span is not at occupied tail and cannot grow in place".into(),
                ));
            }
            assert_eq!(
                total_payload_bytes,
                payload_byte_count + had_bytes,
                "preflight payload geometry mismatch"
            );
        }

        Ok(PreflightRun {
            owner_vertex_id: run.owner_vertex_id,
            label_id: run.label_id,
            bucket_slot,
            bucket,
            edge_slot_count,
            inline_value_width: run.inline_value_width,
            payload_byte_count,
            payload_allocation,
            destination: RunDestination::Slab {
                edge_start_slot,
                payload_byte_offset: None,
                payload_byte_count,
            },
        })
    }

    fn preflight_overflow_log_run(
        &self,
        run: &OneOrientationBucketRun<E>,
        bucket_slot: u64,
        bucket: super::LabelBucket,
        edge_log_leaf_cursors: &mut std::collections::BTreeMap<u32, u32>,
        payload_log_leaf_cursors: &mut std::collections::BTreeMap<u32, u32>,
        leaf_expansion_cursors: &mut std::collections::BTreeMap<u32, LeafExpansionState>,
    ) -> Result<PreflightRun, OneOrientationBatchError> {
        self.ensure_vertex(run.owner_vertex_id)
            .map_err(OneOrientationBatchError::from)?;
        let edge_slot_count = u32::try_from(run.edges.len())
            .map_err(|_| OneOrientationBatchError::LogCapacityExceeded)?;

        // Edge overflow log capacity check against the virtual leaf cursor.
        let header = self.edges.header();
        let leaf = Self::leaf_index_for_vid(run.owner_vertex_id, header.segment_size);
        let _needed_i32 = i32::try_from(edge_slot_count)
            .map_err(|_| OneOrientationBatchError::LogCapacityExceeded)?;
        let (raw_edge_idx, edge_log_capacity) = self.edges.read_overflow_log_state(leaf);
        let base_edge_idx = raw_edge_idx.max(0) as u32;
        let edge_log_start_idx = *edge_log_leaf_cursors.entry(leaf).or_insert(base_edge_idx);
        let edge_log_end_idx = edge_log_start_idx
            .checked_add(edge_slot_count)
            .ok_or(OneOrientationBatchError::LogCapacityExceeded)?;

        // Verify every payload-bearing edge matches the declared width and check
        // payload overflow log capacity.
        let mut payload_byte_count: u64 = 0;
        let mut payload_log_start_idx = None;
        if run.inline_value_width > 0 {
            let width = usize::from(run.inline_value_width);
            for e in &run.edges {
                let actual = e.edge.edge_inline_value_bytes().len();
                if actual != width {
                    return Err(OneOrientationBatchError::PayloadLengthMismatch {
                        logical_ordinal: e.logical_ordinal,
                        expected_width: run.inline_value_width,
                        actual_length: actual,
                    });
                }
            }
            payload_byte_count = u64::from(edge_slot_count)
                .checked_mul(u64::from(run.inline_value_width))
                .ok_or(OneOrientationBatchError::LogCapacityExceeded)?;

            let payload_leaf = self.payload_log_leaf(run.owner_vertex_id);
            let (raw_payload_idx, payload_log_capacity) =
                self.values.read_payload_log_state(payload_leaf);
            let base_payload_idx = raw_payload_idx.max(0) as u32;
            let payload_log_start_idx_virtual = *payload_log_leaf_cursors
                .entry(payload_leaf)
                .or_insert(base_payload_idx);
            let payload_log_end_idx = payload_log_start_idx_virtual
                .checked_add(edge_slot_count)
                .ok_or(OneOrientationBatchError::LogCapacityExceeded)?;
            if payload_log_end_idx > payload_log_capacity {
                return self.preflight_expanded_run(
                    run,
                    bucket_slot,
                    bucket,
                    leaf_expansion_cursors,
                );
            }
            payload_log_leaf_cursors.insert(payload_leaf, payload_log_end_idx);
            payload_log_start_idx = Some(payload_log_start_idx_virtual);
        }

        if edge_log_end_idx > edge_log_capacity {
            return self.preflight_expanded_run(run, bucket_slot, bucket, leaf_expansion_cursors);
        }
        edge_log_leaf_cursors.insert(leaf, edge_log_end_idx);

        Ok(PreflightRun {
            owner_vertex_id: run.owner_vertex_id,
            label_id: run.label_id,
            bucket_slot,
            bucket,
            edge_slot_count,
            inline_value_width: run.inline_value_width,
            payload_byte_count,
            payload_allocation: None,
            destination: RunDestination::OverflowLog {
                edge_log_start_idx,
                payload_log_start_idx,
            },
        })
    }

    /// Preflight an existing-bucket run that does not fit the clean slab window
    /// or the per-leaf overflow log by expanding the pinned PMA leaf block in
    /// place. Fixed-width payload spans are projected with the edge/log fold;
    /// non-tail growth and relocation remain fail-closed.
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
            match self.labeled_leaf_physical_range(run.owner_vertex_id) {
                Some(range) => range,
                None => {
                    return Err(OneOrientationBatchError::UnsupportedGeometry(
                        "expanded-slab batch requires a pinned PMA leaf block".into(),
                    ));
                }
            };
        let edge_slot_count = u32::try_from(run.edges.len())
            .map_err(|_| OneOrientationBatchError::SlabCapacityExceeded)?;
        let edge_log_len = if bucket.overflow_log_head() >= 0 {
            self.edges
                .overflow_log_chain_len(leaf, bucket.overflow_log_head())
        } else {
            0
        };

        // Validate payload-bearing edges and project the payload slab space that
        // will hold the existing slab values, any folded payload log entries, and
        // the pending batch values after the leaf expansion.
        let mut payload_byte_count: u64 = 0;
        let mut payload_allocation: Option<PayloadAllocationKind> = None;
        let existing_payload_slots = bucket.inline_value_slab_slots();
        let payload_log_len = if run.inline_value_width > 0 {
            let width = usize::from(run.inline_value_width);
            for e in &run.edges {
                let actual = e.edge.edge_inline_value_bytes().len();
                if actual != width {
                    return Err(OneOrientationBatchError::PayloadLengthMismatch {
                        logical_ordinal: e.logical_ordinal,
                        expected_width: run.inline_value_width,
                        actual_length: actual,
                    });
                }
            }
            payload_byte_count = u64::from(edge_slot_count)
                .checked_mul(u64::from(run.inline_value_width))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let log_len = if bucket.inline_value_log_head() >= 0 {
                let chain_len = self
                    .values
                    .payload_log_chain_asc_indices(
                        self.payload_log_leaf(run.owner_vertex_id),
                        bucket.inline_value_log_head(),
                    )
                    .len();
                let log_len = u32::try_from(chain_len)
                    .map_err(|_| OneOrientationBatchError::LogCapacityExceeded)?;
                let metadata_len = u32::from(bucket.inline_value_log_len());
                if log_len != metadata_len || log_len != edge_log_len {
                    return Err(OneOrientationBatchError::PayloadLogLengthMismatch {
                        edge_log_len,
                        payload_log_len: log_len,
                    });
                }
                log_len
            } else {
                if edge_log_len != 0 || bucket.inline_value_log_len() != 0 {
                    return Err(OneOrientationBatchError::PayloadLogLengthMismatch {
                        edge_log_len,
                        payload_log_len: 0,
                    });
                }
                0
            };
            let total_payload_slots = u64::from(existing_payload_slots)
                .checked_add(u64::from(log_len))
                .and_then(|s| s.checked_add(u64::from(edge_slot_count)))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let total_payload_bytes = total_payload_slots
                .checked_mul(u64::from(run.inline_value_width))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let had_bytes = u64::from(existing_payload_slots)
                .checked_mul(u64::from(run.inline_value_width))
                .ok_or(OneOrientationBatchError::SlabCapacityExceeded)?;
            let offset = bucket.inline_value_offset();
            let tail = self.values.header().slab_occupied_tail;
            let span_ends_at_tail = offset.checked_add(had_bytes).is_some_and(|end| end == tail);

            if had_bytes == 0 {
                payload_allocation = Some(PayloadAllocationKind::New {
                    byte_len: total_payload_bytes,
                });
            } else if span_ends_at_tail {
                payload_allocation = Some(PayloadAllocationKind::GrowInPlace {
                    offset,
                    had_bytes,
                    new_bytes: total_payload_bytes,
                });
            } else {
                return Err(OneOrientationBatchError::UnsupportedGeometry(
                    "payload span is not at occupied tail and cannot grow in place".into(),
                ));
            }
            log_len
        } else {
            if bucket.inline_value_log_head() >= 0 || bucket.inline_value_log_len() != 0 {
                return Err(OneOrientationBatchError::PayloadLogLengthMismatch {
                    edge_log_len,
                    payload_log_len: u32::from(bucket.inline_value_log_len()),
                });
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
        let block_len = super::leaf_pin::labeled_leaf_physical_block_len(seg);
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
            sum.checked_add(b.stored_slots.max(b.degree))
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
            inline_value_width: run.inline_value_width,
            payload_byte_count,
            payload_allocation,
            destination: RunDestination::ExpandedSlab {
                leaf,
                old_leaf_len: state.old_leaf_len,
                new_leaf_len: state.new_leaf_len,
                existing_bucket_slots: bucket.stored_slots,
                edge_log_len,
                existing_payload_slots,
                payload_log_len,
                payload_byte_offset: None,
                payload_byte_count,
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
    pub fn commit<M: Memory>(self, graph: &LabeledLaraGraph<E, M>) -> OneOrientationBatchResult {
        self.commit_with_location_mode(graph, BatchLocationMode::AggregateOnly)
    }

    /// Commit while retaining exact physical locations for every edge.
    pub fn commit_with_locations<M: Memory>(
        self,
        graph: &LabeledLaraGraph<E, M>,
    ) -> OneOrientationBatchResult {
        self.commit_with_location_mode(graph, BatchLocationMode::Capture)
    }

    /// Commit with explicit control over physical-location materialization.
    pub fn commit_with_location_mode<M: Memory>(
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
                run.inline_value_width, res.inline_value_width,
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
                current_fingerprint.inline_value_offset =
                    res.bucket_fingerprint.inline_value_offset;
            }
            assert_eq!(
                current_fingerprint, res.bucket_fingerprint,
                "bucket occupancy changed between reserve and commit"
            );

            let _updated_degree = bucket
                .degree
                .checked_add(res.edge_slot_count)
                .expect("reserve guaranteed degree overflow safety");
            match &res.destination {
                RunDestination::Slab { .. } => {
                    let _updated_stored_slots = bucket
                        .stored_slots
                        .checked_add(res.edge_slot_count)
                        .expect("reserve guaranteed stored_slots overflow safety");
                    if res.inline_value_width > 0 {
                        let _updated_payload_slots = bucket
                            .inline_value_slab_slots()
                            .checked_add(res.edge_slot_count)
                            .expect("reserve guaranteed payload slab slot overflow safety");
                    }
                }
                RunDestination::OverflowLog { .. } => {}
                RunDestination::ExpandedSlab { edge_log_len, .. } => {
                    let _updated_degree = bucket
                        .degree
                        .checked_add(res.edge_slot_count)
                        .expect("reserve guaranteed expanded degree overflow safety");
                    let _updated_stored_slots = bucket
                        .stored_slots
                        .checked_add(*edge_log_len)
                        .and_then(|x| x.checked_add(res.edge_slot_count))
                        .expect("reserve guaranteed expanded stored_slots overflow safety");
                }
            }
        }

        let _next_num_edges = graph
            .edges
            .header()
            .num_edges
            .checked_add(total_edge_slots)
            .expect("reserve guaranteed num_edges overflow safety");

        // First pass: write all edge and payload bytes.
        let mut edge_slots_written: u64 = 0;
        let mut edge_log_entries_written: u64 = 0;
        let mut payload_slots_written: u64 = 0;
        let mut payload_log_entries_written: u64 = 0;
        let mut locations = location_mode
            .captures()
            .then(|| Vec::with_capacity(self.plan.runs.iter().map(|r| r.edges.len()).sum()));
        for (run, res) in self.plan.runs.iter().zip(self.runs.iter()) {
            match &res.destination {
                RunDestination::Slab {
                    edge_start_slot,
                    payload_byte_offset,
                    payload_byte_count,
                } => {
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

                    let payload_width = u64::from(res.inline_value_width);
                    if let Some(locations) = locations.as_mut() {
                        locations.extend(run.edges.iter().enumerate().map(|(offset, edge)| {
                            OneOrientationBatchLocation {
                                logical_ordinal: edge.logical_ordinal,
                                owner_vertex_id: edge.owner_vertex_id,
                                location: OneOrientationPhysicalLocation::Slab {
                                    edge_slot: edge_start_slot
                                        .checked_add(offset as u64)
                                        .expect("reserve guaranteed edge location"),
                                    payload_byte_offset: payload_byte_offset.as_ref().map(
                                        |start| {
                                            start
                                                .checked_add(
                                                    (offset as u64)
                                                        .checked_mul(payload_width)
                                                        .expect(
                                                            "reserve guaranteed payload location",
                                                        ),
                                                )
                                                .expect("reserve guaranteed payload location")
                                        },
                                    ),
                                },
                            }
                        }));
                    }

                    if res.inline_value_width > 0 {
                        let payload_offset = payload_byte_offset.expect(
                            "reserve guaranteed a payload byte offset for payload-bearing run",
                        );
                        let payload_bytes = run
                            .edges
                            .iter()
                            .flat_map(|e| e.edge.edge_inline_value_bytes().iter().copied())
                            .collect::<Vec<u8>>();
                        assert_eq!(
                            payload_bytes.len() as u64,
                            *payload_byte_count,
                            "reserve payload byte count must match actual payload bytes"
                        );
                        graph
                            .values
                            .write_bytes(payload_offset, &payload_bytes)
                            .expect("reserve guaranteed payload slab capacity");
                        payload_slots_written = payload_slots_written
                            .checked_add(u64::from(res.edge_slot_count))
                            .expect("reserve guaranteed payload slot count");
                    }

                    edge_slots_written = edge_slots_written
                        .checked_add(u64::from(res.edge_slot_count))
                        .expect("reserve guaranteed edge slot count");
                }
                RunDestination::OverflowLog {
                    edge_log_start_idx,
                    payload_log_start_idx,
                } => {
                    let leaf = LabeledLaraGraph::<E, M>::leaf_index_for_vid(
                        res.bucket_fingerprint.owner_vertex_id,
                        graph.edges.header().segment_size,
                    );
                    let (bucket_slot, bucket) = match graph
                        .find_bucket(
                            res.bucket_fingerprint.owner_vertex_id,
                            &graph.vertices.get(res.bucket_fingerprint.owner_vertex_id),
                            res.bucket_fingerprint.label_id,
                        )
                        .expect("reserve found this bucket")
                    {
                        BucketSearch::Found { slot, bucket } => (slot, bucket),
                        BucketSearch::Missing { .. } => {
                            panic!("bucket disappeared between reserve and commit");
                        }
                    };
                    let _ = bucket_slot;

                    let prev_head = bucket.overflow_log_head();
                    let entries: Vec<(i32, E)> = run
                        .edges
                        .iter()
                        .enumerate()
                        .map(|(offset, e)| {
                            let prev = if offset == 0 {
                                prev_head
                            } else {
                                (edge_log_start_idx + offset as u32 - 1) as i32
                            };
                            (prev, e.edge.clone())
                        })
                        .collect();
                    graph
                        .edges
                        .write_overflow_log_entries(leaf, *edge_log_start_idx, &entries)
                        .expect("reserve guaranteed edge log capacity");
                    let payload_start = if res.inline_value_width > 0 {
                        Some(payload_log_start_idx.expect(
                            "reserve guaranteed a payload log start index for payload-bearing run",
                        ))
                    } else {
                        None
                    };
                    if let Some(locations) = locations.as_mut() {
                        locations.extend(run.edges.iter().enumerate().map(|(offset, edge)| {
                            OneOrientationBatchLocation {
                                logical_ordinal: edge.logical_ordinal,
                                owner_vertex_id: edge.owner_vertex_id,
                                location: OneOrientationPhysicalLocation::OverflowLog {
                                    leaf,
                                    edge_entry_idx: edge_log_start_idx
                                        .checked_add(offset as u32)
                                        .expect("reserve guaranteed edge log location"),
                                    payload_entry_idx: payload_start.map(|start| {
                                        start
                                            .checked_add(offset as u32)
                                            .expect("reserve guaranteed payload log location")
                                    }),
                                },
                            }
                        }));
                    }
                    edge_log_entries_written = edge_log_entries_written
                        .checked_add(u64::from(res.edge_slot_count))
                        .expect("reserve guaranteed edge log slot count");
                    edge_slots_written = edge_slots_written
                        .checked_add(u64::from(res.edge_slot_count))
                        .expect("reserve guaranteed edge slot count");

                    if res.inline_value_width > 0 {
                        let payload_leaf =
                            graph.payload_log_leaf(res.bucket_fingerprint.owner_vertex_id);
                        let prev_payload_head = bucket.inline_value_log_head();
                        let payload_bytes = run
                            .edges
                            .iter()
                            .flat_map(|e| e.edge.edge_inline_value_bytes().iter().copied())
                            .collect::<Vec<u8>>();
                        let payload_start = payload_log_start_idx.expect(
                            "reserve guaranteed a payload log start index for payload-bearing run",
                        );
                        graph
                            .values
                            .write_payload_log_entries(
                                payload_leaf,
                                payload_start,
                                prev_payload_head,
                                res.inline_value_width,
                                &payload_bytes,
                            )
                            .expect("reserve guaranteed payload log capacity");
                        payload_log_entries_written = payload_log_entries_written
                            .checked_add(u64::from(res.edge_slot_count))
                            .expect("reserve guaranteed payload log slot count");
                        payload_slots_written = payload_slots_written
                            .checked_add(u64::from(res.edge_slot_count))
                            .expect("reserve guaranteed payload slot count");
                    }
                }
                RunDestination::ExpandedSlab {
                    leaf,
                    old_leaf_len,
                    new_leaf_len,
                    existing_bucket_slots,
                    edge_log_len,
                    existing_payload_slots,
                    payload_log_len,
                    payload_byte_offset,
                    payload_byte_count,
                    ..
                } => {
                    let (edge_written, payload_written, run_locations) =
                        Self::commit_expanded_slab_run::<M>(
                            graph,
                            run,
                            res,
                            *leaf,
                            *old_leaf_len,
                            *new_leaf_len,
                            *existing_bucket_slots,
                            *edge_log_len,
                            *existing_payload_slots,
                            *payload_log_len,
                            *payload_byte_offset,
                            *payload_byte_count,
                            location_mode,
                        );
                    if let Some(locations) = locations.as_mut() {
                        locations.extend(run_locations);
                    }
                    edge_slots_written = edge_slots_written
                        .checked_add(edge_written)
                        .expect("reserve guaranteed expanded edge slot count");
                    payload_slots_written = payload_slots_written
                        .checked_add(payload_written)
                        .expect("reserve guaranteed expanded payload slot count");
                }
            }
        }

        // Second pass: publish all bucket metadata and segment counts for paths
        // that did not already publish them during the first pass.
        for res in &self.runs {
            if matches!(res.destination, RunDestination::ExpandedSlab { .. }) {
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
                    payload_byte_offset,
                    ..
                } => {
                    updated_bucket = updated_bucket.with_stored_slots(
                        bucket
                            .stored_slots
                            .checked_add(res.edge_slot_count)
                            .expect("reserve guaranteed stored_slots overflow safety"),
                    );
                    if res.inline_value_width > 0 {
                        updated_bucket = updated_bucket.with_inline_value_slab_slots(
                            bucket
                                .inline_value_slab_slots()
                                .checked_add(res.edge_slot_count)
                                .expect("reserve guaranteed payload slab slot overflow safety"),
                        );
                        if bucket.inline_value_slab_slots() == 0 {
                            updated_bucket = updated_bucket.with_inline_value_offset(
                                payload_byte_offset
                                    .expect("reserve payload offset for new payload span"),
                            );
                        }
                    }
                }
                RunDestination::OverflowLog {
                    edge_log_start_idx,
                    payload_log_start_idx,
                } => {
                    let last_edge_idx = edge_log_start_idx + res.edge_slot_count - 1;
                    updated_bucket = updated_bucket.with_overflow_log_head(last_edge_idx as i32);
                    if res.inline_value_width > 0 {
                        let payload_start = payload_log_start_idx.expect(
                            "reserve guaranteed payload log start index for payload-bearing run",
                        );
                        let last_payload_idx = payload_start + res.edge_slot_count - 1;
                        let next_payload_len = u32::from(bucket.inline_value_log_len())
                            .checked_add(res.edge_slot_count)
                            .expect("reserve guaranteed payload log len overflow safety");
                        let next_payload_len_u8 = u8::try_from(next_payload_len)
                            .expect("reserve guaranteed payload log len fits in u8");
                        updated_bucket = updated_bucket
                            .try_with_payload_log(last_payload_idx as i32, next_payload_len_u8)
                            .expect("reserve guaranteed payload log state");
                    }
                }
                RunDestination::ExpandedSlab { .. } => unreachable!(),
            }
            graph
                .buckets
                .write_label_bucket_slot(bucket_slot, updated_bucket)
                .expect("reserve guaranteed bucket slot writability");

            graph
                .edges
                .bump_vertex_segment_counts(
                    res.bucket_fingerprint.owner_vertex_id,
                    i64::from(res.edge_slot_count),
                    0,
                )
                .expect("reserve guaranteed segment count overflow safety");
        }

        // Publish any vertex span growth caused by growing the last bucket on a
        // vertex.  For non-last buckets the end slot stays within the existing
        // vertex span, so the vertex row is unchanged.  Overflow-log runs do not
        // change the vertex slab span.  Expanded-slab runs already updated the
        // vertex span through rebalance.
        let mut vertex_stored_slot_ends: std::collections::BTreeMap<VertexId, u64> =
            std::collections::BTreeMap::new();
        for res in &self.runs {
            if let RunDestination::Slab {
                edge_start_slot, ..
            } = &res.destination
            {
                let end = edge_start_slot + u64::from(res.edge_slot_count);
                let entry = vertex_stored_slot_ends
                    .entry(res.bucket_fingerprint.owner_vertex_id)
                    .or_default();
                *entry = (*entry).max(end);
            }
        }
        for (vid, end) in vertex_stored_slot_ends {
            let vertex = graph.vertices.get(vid);
            if end > u64::from(vertex.stored_slots) {
                let updated = vertex.with_stored_slots(
                    u32::try_from(end).expect("reserve guaranteed vertex stored_slots fit in u32"),
                );
                graph.vertices.set(vid, &updated);
            }
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
            payload_slots_written: u32::try_from(payload_slots_written)
                .expect("reserve guaranteed total payload slots fit in u32"),
            payload_log_entries_written: u32::try_from(payload_log_entries_written)
                .expect("reserve guaranteed total payload log entries fit in u32"),
            locations,
        }
    }

    /// Commit one expanded-slab run.
    ///
    /// Commit one expanded-slab run after its owning vertex has already been
    /// rebalanced and the leaf block growth has been reserved.  Folds any
    /// existing overflow-log entries into the slab, writes the pending batch
    /// edges and payload values, and updates the bucket metadata.  Returns the
    /// number of physical edge slab slots and payload slab slots written.
    fn commit_expanded_slab_run<M: Memory>(
        graph: &LabeledLaraGraph<E, M>,
        run: &OneOrientationBucketRun<E>,
        res: &BatchReservationRun,
        leaf: u32,
        old_leaf_len: u64,
        new_leaf_len: u64,
        existing_bucket_slots: u32,
        edge_log_len: u32,
        existing_payload_slots: u32,
        payload_log_len: u32,
        payload_byte_offset: Option<u64>,
        payload_byte_count: u64,
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

        // Fold existing edge overflow-log entries into the slab after the existing prefix.
        if edge_log_len > 0 {
            let chain = graph
                .edges
                .overflow_log_chain_asc_indices(leaf, new_bucket.overflow_log_head());
            for (offset, log_idx) in chain.into_iter().enumerate() {
                let (_, edge) = graph.edges.read_overflow_log_entry(leaf, log_idx);
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
        }

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

        let payload_width = u64::from(res.inline_value_width);
        let pending_payload_start = payload_byte_offset.map(|offset| {
            offset
                .checked_add(
                    u64::from(existing_payload_slots)
                        .checked_add(u64::from(payload_log_len))
                        .expect("reserve guaranteed pending payload slot offset")
                        .checked_mul(payload_width)
                        .expect("reserve guaranteed pending payload byte offset"),
                )
                .expect("reserve guaranteed pending payload offset")
        });
        let locations = if location_mode.captures() {
            run.edges
                .iter()
                .enumerate()
                .map(|(offset, edge)| OneOrientationBatchLocation {
                    logical_ordinal: edge.logical_ordinal,
                    owner_vertex_id: edge.owner_vertex_id,
                    location: OneOrientationPhysicalLocation::Slab {
                        edge_slot: pending_start
                            .checked_add(offset as u64)
                            .expect("reserve guaranteed edge location"),
                        payload_byte_offset: pending_payload_start.map(|start| {
                            start
                                .checked_add(
                                    (offset as u64)
                                        .checked_mul(payload_width)
                                        .expect("reserve guaranteed payload location"),
                                )
                                .expect("reserve guaranteed payload location")
                        }),
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
            .with_overflow_log_head(-1);

        // Fold and append payload values when present.
        let mut payload_slots_written: u64 = 0;
        if res.inline_value_width > 0 {
            let width = u64::from(res.inline_value_width);
            let new_payload_offset =
                payload_byte_offset.expect("reserve guaranteed payload byte offset");

            // Fold existing payload overflow-log entries into the slab.
            if payload_log_len > 0 {
                let payload_leaf = graph.payload_log_leaf(owner);
                let chain = graph.values.payload_log_chain_asc_indices(
                    payload_leaf,
                    new_bucket.inline_value_log_head(),
                );
                for (offset, log_idx) in chain.into_iter().enumerate() {
                    let mut buf = vec![0u8; usize::from(res.inline_value_width)];
                    graph
                        .values
                        .read_payload_log_entry(
                            payload_leaf,
                            log_idx,
                            res.inline_value_width,
                            &mut buf,
                        )
                        .expect("reserve guaranteed payload log readability");
                    let out_offset = new_payload_offset
                        .checked_add(
                            u64::from(existing_payload_slots)
                                .checked_add(offset as u64)
                                .expect("reserve guaranteed folded payload slot offset")
                                .checked_mul(width)
                                .expect("reserve guaranteed folded payload byte offset"),
                        )
                        .expect("reserve guaranteed folded payload offset");
                    graph
                        .values
                        .write_bytes(out_offset, &buf)
                        .expect("reserve guaranteed payload slab capacity");
                }
            }

            // Write pending batch payload values after the folded log.
            let pending_payload_bytes: Vec<u8> = run
                .edges
                .iter()
                .flat_map(|e| e.edge.edge_inline_value_bytes().iter().copied())
                .collect();
            assert_eq!(
                pending_payload_bytes.len() as u64,
                payload_byte_count,
                "reserve payload byte count must match actual payload bytes"
            );
            let pending_payload_offset = new_payload_offset
                .checked_add(
                    u64::from(existing_payload_slots)
                        .checked_add(u64::from(payload_log_len))
                        .expect("reserve guaranteed pending payload slot offset")
                        .checked_mul(width)
                        .expect("reserve guaranteed pending payload byte offset"),
                )
                .expect("reserve guaranteed pending payload offset");
            graph
                .values
                .write_bytes(pending_payload_offset, &pending_payload_bytes)
                .expect("reserve guaranteed payload slab capacity");

            let total_payload_slots = u64::from(existing_payload_slots)
                .checked_add(u64::from(payload_log_len))
                .and_then(|s| s.checked_add(u64::from(res.edge_slot_count)))
                .expect("reserve guaranteed payload slot count");
            updated_bucket = updated_bucket
                .with_inline_value_slab_slots(total_payload_slots as u32)
                .with_inline_value_log_head(-1);
            if existing_payload_slots == 0 {
                updated_bucket = updated_bucket.with_inline_value_offset(new_payload_offset);
            }
            payload_slots_written = u64::from(payload_log_len)
                .checked_add(u64::from(res.edge_slot_count))
                .expect("reserve guaranteed payload slot count");

            let total_payload_bytes = total_payload_slots
                .checked_mul(width)
                .expect("reserve guaranteed payload byte count");
            let had_bytes = u64::from(existing_payload_slots)
                .checked_mul(width)
                .expect("reserve guaranteed existing payload byte count");
            let alloc_delta = total_payload_bytes - had_bytes;
            if alloc_delta > 0 {
                let vertex = graph.vertices.get(owner);
                let new_alloc = vertex
                    .inline_value_allocated_bytes()
                    .checked_add(alloc_delta)
                    .expect("reserve guaranteed vertex payload allocated bytes overflow safety");
                graph
                    .vertices
                    .set(owner, &vertex.with_inline_value_allocated_bytes(new_alloc));
            }
        }

        graph
            .buckets
            .write_label_bucket_slot(new_bucket_slot, updated_bucket)
            .expect("reserve guaranteed bucket writability");

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
        (edge_slots_written, payload_slots_written, locations)
    }
}

impl<E> BatchReservation<E>
where
    E: CsrEdge,
{
    /// Roll back the capacity and payload reservations made by this reservation.
    ///
    /// This consumes the token and restores the edge-store logical capacity
    /// and payload occupied tail to the values captured before
    /// `reserve_one_orientation_batch` mutated them.  Any payload bytes that
    /// were already appended are retired to the payload free-list as reusable
    /// slack; the underlying stable-memory pages are not shrunk.  Canonical
    /// adjacency and bucket metadata are untouched.  Because the token is
    /// consumed, a reservation cannot be rolled back twice.
    pub(crate) fn rollback<M: Memory>(self, graph: &LabeledLaraGraph<E, M>) {
        assert_eq!(
            self.graph_marker,
            graph.instance_marker(),
            "reservation was produced by a different graph instance"
        );
        graph.rollback_leaf_expansions(&self.leaf_expansions);
        graph.rollback_edge_capacity(self.edge_capacity_before);
        graph.rollback_payload_tail(self.payload_tail_before);
    }
}

/// Result of a committed one-orientation batch write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneOrientationBatchResult {
    /// Number of edge slab slots written.
    pub edge_slots_written: u32,
    /// Number of edge overflow-log entries written.
    pub edge_log_entries_written: u32,
    /// Number of payload slab slots written.
    pub payload_slots_written: u32,
    /// Number of payload overflow-log entries written.
    pub payload_log_entries_written: u32,
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
    pub fn insert_one_orientation_batch(
        &self,
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<OneOrientationBatchResult, OneOrientationBatchError> {
        let reservation = self.reserve_one_orientation_batch(plan)?;
        Ok(reservation.commit::<M>(self))
    }

    /// Convenience variant that returns exact physical locations.
    pub fn insert_one_orientation_batch_with_locations(
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
            Self::UnsupportedGeometry(detail) => write!(f, "unsupported batch geometry: {detail}"),
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
            Self::PayloadByteWidthMismatch {
                bucket_width,
                edge_width,
            } => {
                write!(
                    f,
                    "payload width mismatch: bucket {bucket_width}, edge {edge_width}"
                )
            }
            Self::SlabCapacityExceeded => write!(f, "slab capacity exceeded"),
            Self::LogCapacityExceeded => write!(f, "log capacity exceeded"),
            Self::PayloadLengthMismatch {
                logical_ordinal,
                expected_width,
                actual_length,
            } => {
                write!(
                    f,
                    "payload length mismatch at ordinal {logical_ordinal}: expected {expected_width}, actual {actual_length}"
                )
            }
            Self::PayloadLogLengthMismatch {
                edge_log_len,
                payload_log_len,
            } => write!(
                f,
                "edge/payload overflow-log length mismatch: edge {edge_log_len}, payload {payload_log_len}"
            ),
            Self::StorageError(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl std::error::Error for OneOrientationBatchError {}

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
    use crate::labeled::graph::test_support::{TestEdge as GraphTestEdge, test_graph_with_default};
    use crate::labeled::record::LabeledVertex;

    use super::{
        OneOrientationBatchEdge, OneOrientationBatchError, OneOrientationBatchPlan,
        OneOrientationBucketRun, PreflightRun,
    };

    #[test]
    fn reserve_checks_total_edge_and_payload_slot_counts_fit_in_u32_results() {
        // `check_total_result_counts_fit_u32` rejects plans whose summed edge or
        // payload slot counts would not fit in the public `u32` result fields.
        // Payload byte counts are irrelevant to this check; only slot counts matter.
        let check = |preflight: &[PreflightRun]| {
            super::LabeledLaraGraph::<GraphTestEdge, crate::VectorMemory>::check_total_result_counts_fit_u32(
                preflight,
            )
        };

        let ok = vec![PreflightRun {
            edge_slot_count: 10,
            inline_value_width: 4,
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

        let too_many_payload = vec![
            PreflightRun {
                edge_slot_count: u32::MAX / 2 + 1,
                inline_value_width: 4,
                ..Default::default()
            },
            PreflightRun {
                edge_slot_count: u32::MAX / 2 + 1,
                inline_value_width: 4,
                ..Default::default()
            },
        ];
        assert!(
            check(&too_many_payload).is_err(),
            "payload slot count overflowing u32 must be rejected"
        );

        let edge_at_max_u32 = vec![PreflightRun {
            edge_slot_count: u32::MAX,
            ..Default::default()
        }];
        assert!(
            check(&edge_at_max_u32).is_ok(),
            "exactly u32::MAX edge slots must be accepted"
        );

        let payload_at_max_u32 = vec![PreflightRun {
            edge_slot_count: u32::MAX,
            inline_value_width: 1,
            ..Default::default()
        }];
        assert!(
            check(&payload_at_max_u32).is_ok(),
            "exactly u32::MAX payload slots must be accepted"
        );
    }
    #[test]
    fn reserve_rejects_new_bucket_in_initial_slice() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: BucketLabelKey::directed_from_index(1),
                inline_value_width: 0,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: BucketLabelKey::directed_from_index(1),
                    edge: GraphTestEdge { target: 1 },
                }],
            }],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::UnsupportedGeometry(_)),
            "expected UnsupportedGeometry for new bucket, got {err}"
        );
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
            )
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: BucketLabelKey::directed_from_index(1),
                    inline_value_width: 0,
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
                    inline_value_width: 0,
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
            matches!(err, OneOrientationBatchError::UnsupportedGeometry(_)),
            "expected UnsupportedGeometry for duplicate runs, got {err}"
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
            )
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: BucketLabelKey::directed_from_index(1),
                inline_value_width: 0,
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
            matches!(err, OneOrientationBatchError::UnsupportedGeometry(_)),
            "expected UnsupportedGeometry for out-of-order ordinals, got {err}"
        );
    }

    #[test]
    fn overflow_log_append_success() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        // Fill the bucket slab window so the batch must use the overflow log.
        let label = BucketLabelKey::directed_from_index(1);
        for i in 1..=3u32 {
            graph
                .insert_edge(VertexId::from(0), label, GraphTestEdge { target: i })
                .unwrap();
        }

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_value_width: 0,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: GraphTestEdge { target: 10 },
                }],
            }],
        };

        let result = graph
            .insert_one_orientation_batch(&plan)
            .expect("overflow-log batch append should succeed");
        assert_eq!(result.edge_slots_written, 1);
        assert_eq!(result.edge_log_entries_written, 1);
        assert_eq!(result.payload_slots_written, 0);

        let out = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(
            out.len(),
            4,
            "expected four out-edges after overflow append"
        );
        assert!(
            out.iter().any(|e| e.target == 10),
            "overflow-log edge must be visible in read-back"
        );
    }

    #[test]
    fn reserve_rejects_log_capacity_exceeded() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        // Push enough vertices to cover two PMA leaves (segment_size = 32).
        for _ in 0..34 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }

        let label = BucketLabelKey::directed_from_index(1);
        // Create a bucket at vertex 0 and fill its slab window.
        for i in 1..=3u32 {
            graph
                .insert_edge(VertexId::from(0), label, GraphTestEdge { target: i })
                .unwrap();
        }
        // Pin a second leaf after leaf 0 so leaf 0 is not at the allocation tail
        // and cannot expand via tail growth.  This keeps the test on the
        // no-admission path.
        graph
            .insert_edge(VertexId::from(32), label, GraphTestEdge { target: 1 })
            .unwrap();

        // Fill the per-leaf edge overflow log to capacity (170 entries).
        let header = graph.edges().header();
        let leaf = LabeledLaraGraph::<GraphTestEdge, crate::VectorMemory>::leaf_index_for_vid(
            VertexId::from(0),
            header.segment_size,
        );
        let log_capacity = graph.edges().read_overflow_log_state(leaf).1 as usize;
        let entries: Vec<(i32, GraphTestEdge)> = (0..log_capacity)
            .map(|i| {
                let prev = if i == 0 { -1 } else { (i as i32) - 1 };
                (
                    prev,
                    GraphTestEdge {
                        target: 100 + i as u32,
                    },
                )
            })
            .collect();
        graph
            .edges()
            .write_overflow_log_entries(leaf, 0, &entries)
            .expect("fill log");

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_value_width: 0,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: GraphTestEdge { target: 10 },
                }],
            }],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::LogCapacityExceeded),
            "expected LogCapacityExceeded, got {err}"
        );
    }
}
