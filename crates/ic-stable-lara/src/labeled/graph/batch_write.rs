//! Failure-atomic one-orientation batch writes for [`LabeledLaraGraph`].
//!
//! This module implements the smallest LARA-internal batch mutation slice from
//! ADR 0045: one orientation, one bounded batch, and an explicit
//! `plan -> reserve -> commit` boundary.  It reuses the existing LARA ownership
//! and placement types and does not add a public or Candid API.
//!
//! The reserve/commit design is intentionally split:
//!
//! 1. `reserve_one_orientation_batch` performs **only** read-only validation and
//!    mutation-free capacity projection.  It returns a `BatchReservation` token
//!    that records the preflight snapshot and required destination geometry.
//! 2. `BatchReservation::commit` performs the actual stable-memory mutations.
//!    Because all fallible checks happened in reserve, commit is infallible.
//!
//! If reserve fails after any run, it has not yet grown edge/payload backing
//! memory, so allocator state is unchanged.  Commit is all-or-nothing and
//! panics on invariant violation rather than returning a recoverable error.
//!
//! The initial slice supports only existing buckets whose run fits the current
//! slab window with payload span growth at the occupied tail.  New buckets,
//! overflow-log appends, rebalance, relocation, and dynamic expansion are
//! deferred.

use crate::{
    VertexId,
    labeled::{bucket_label_key::BucketLabelKey, slot_index::checked_add_slot_index},
    traits::CsrEdge,
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

/// Opaque reservation produced by [`LabeledLaraGraph::reserve_one_orientation_batch`].
///
/// The fields are deliberately private; the only valid use is passing it to
/// [`BatchReservation::commit`].  The token embeds a snapshot of the bucket
/// identity and expected occupancy at reservation time so commit can detect
/// tampering or stale state.
pub struct BatchReservation<E>
where
    E: CsrEdge,
{
    plan: OneOrientationBatchPlan<E>,
    runs: Vec<BatchReservationRun>,
    graph_marker: usize,
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
    /// First edge slab slot written for the run.
    edge_start_slot: u64,
    /// Number of edge slots reserved for the run.
    edge_slot_count: u32,
    /// Byte width per inline value slot (`0` = no payload).
    inline_value_width: u16,
    /// Byte offset in the payload slab where the run writes, if payload-bearing.
    payload_byte_offset: Option<u64>,
    /// Number of payload bytes reserved.
    payload_byte_count: u64,
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
    /// A stable-memory reservation or write failed.
    StorageError(LabeledOperationError),
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

struct PreflightRun {
    owner_vertex_id: VertexId,
    label_id: BucketLabelKey,
    bucket_slot: u64,
    bucket: super::LabelBucket,
    edge_start_slot: u64,
    edge_slot_count: u32,
    inline_value_width: u16,
    payload_byte_offset: Option<u64>,
    payload_byte_count: u64,
    payload_allocation: Option<PayloadAllocationKind>,
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Validate a one-orientation batch plan and reserve the required capacity.
    ///
    /// This is the `reserve` step of the ADR 0045 boundary.  It performs all
    /// fallible validation **without mutating canonical or allocator state**.  A
    /// failure here leaves the store unchanged.  On success it returns an opaque
    /// [`BatchReservation`] token; the only valid operation on that token is
    /// [`BatchReservation::commit`].
    pub fn reserve_one_orientation_batch(
        &self,
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<BatchReservation<E>, OneOrientationBatchError> {
        // Phase 1: validate plan invariants without touching canonical state.
        Self::validate_plan_invariants(plan)?;

        // Phase 2: mutation-free preflight of every run.
        let mut preflight = Vec::with_capacity(plan.runs.len());
        let mut max_edge_end_slot: u64 = 0;
        for run in &plan.runs {
            let p = self.preflight_run(run)?;
            max_edge_end_slot =
                max_edge_end_slot.max(p.edge_start_slot + u64::from(p.edge_slot_count));
            preflight.push(p);
        }

        // Phase 3: mutate allocator state.  We resize edge-store capacity once and
        // then perform every payload allocation/growth.  If any payload operation
        // fails, we roll back the payload occupied tail to its pre-reserve value.
        // Edge-store capacity growth is monotone and safe to retain.
        self.edges
            .set_elem_capacity(max_edge_end_slot)
            .map_err(storage_resize_error)?;

        let payload_tail_before = self.values.header().slab_occupied_tail;
        let mut allocated_payload_offsets: Vec<Option<(u64, u64)>> =
            Vec::with_capacity(preflight.len());
        for p in &preflight {
            if let Some(allocation) = p.payload_allocation {
                match allocation {
                    PayloadAllocationKind::Existing { offset } => {
                        allocated_payload_offsets.push(Some((offset, 0)));
                    }
                    PayloadAllocationKind::New { byte_len } => {
                        let actual_offset =
                            self.values.append_byte_span(byte_len).map_err(|e| {
                                self.rollback_payload_tail(payload_tail_before);
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
                                self.rollback_payload_tail(payload_tail_before);
                                storage_resize_error(e)
                            })?;
                        if !grown {
                            self.rollback_payload_tail(payload_tail_before);
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

        // Phase 4: build the opaque reservation token from the preflight snapshot.
        // No fallible allocator calls remain from this point onward.
        let runs = preflight
            .into_iter()
            .zip(allocated_payload_offsets)
            .map(|(p, allocated)| {
                let payload_byte_offset = if p.inline_value_width > 0 {
                    let (actual_offset, _allocated_len) =
                        allocated.expect("payload-bearing run must have an allocated offset");
                    // Defensive: for existing spans the actual offset must equal the
                    // preflight placeholder.
                    if let Some(PayloadAllocationKind::Existing { offset }) = p.payload_allocation {
                        assert_eq!(
                            actual_offset, offset,
                            "existing payload offset must not change during allocation"
                        );
                    }
                    Some(actual_offset)
                } else {
                    debug_assert!(
                        allocated.is_none(),
                        "edge-only run must have no payload allocation"
                    );
                    None
                };
                BatchReservationRun {
                    bucket_slot: p.bucket_slot,
                    bucket_fingerprint: BucketFingerprint::from_bucket(
                        p.owner_vertex_id,
                        p.label_id,
                        &p.bucket,
                        &self.vertices.get(p.owner_vertex_id),
                    ),
                    edge_start_slot: p.edge_start_slot,
                    edge_slot_count: p.edge_slot_count,
                    inline_value_width: p.inline_value_width,
                    payload_byte_offset,
                    payload_byte_count: p.payload_byte_count,
                }
            })
            .collect();

        Ok(BatchReservation {
            plan: plan.clone(),
            runs,
            graph_marker: self.instance_marker(),
        })
    }

    fn validate_plan_invariants(
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<(), OneOrientationBatchError> {
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
    /// range and restore the header.  Existing payload spans are untouched.
    fn rollback_payload_tail(&self, original_tail: u64) {
        let current_tail = self.values.header().slab_occupied_tail;
        if current_tail > original_tail {
            let _ = self
                .values
                .retire_byte_span(original_tail, current_tail - original_tail);
            self.values.reset_slab_occupied_tail(original_tail);
        }
    }

    /// Opaque identity marker for this graph instance, used to bind reservations.
    fn instance_marker(&self) -> usize {
        &self.edges as *const _ as usize
    }

    fn preflight_run(
        &self,
        run: &OneOrientationBucketRun<E>,
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
        if edge_end_slot > successor_start {
            return Err(OneOrientationBatchError::SlabCapacityExceeded);
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
        let mut payload_byte_offset = None;
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
                payload_byte_offset = Some(offset);
            } else if span_ends_at_tail {
                payload_allocation = Some(PayloadAllocationKind::GrowInPlace {
                    offset,
                    had_bytes,
                    new_bytes: total_payload_bytes,
                });
                payload_byte_offset = Some(offset);
            } else {
                return Err(OneOrientationBatchError::UnsupportedGeometry(
                    "payload span is not at occupied tail and cannot grow in place".into(),
                ));
            }
            // Consistency check: the projected byte count from the bucket geometry
            // must equal the sum of per-edge payload widths.
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
            edge_start_slot,
            edge_slot_count,
            inline_value_width: run.inline_value_width,
            payload_byte_offset,
            payload_byte_count,
            payload_allocation,
        })
    }
}

impl<E> BatchReservation<E>
where
    E: CsrEdge,
{
    /// Commit the reserved batch.
    ///
    /// This is the `commit` step.  All fallible validation and capacity
    /// reservation was performed during reserve, so this method performs only
    /// infallible canonical byte writes and metadata updates.  A panic here is an
    /// invariant violation rather than a recoverable error.
    ///
    /// The reservation embeds a runtime marker of the graph instance that produced
    /// it; passing the token to a different graph of the same type panics.
    pub fn commit<M: Memory>(self, graph: &LabeledLaraGraph<E, M>) -> OneOrientationBatchResult {
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
            assert_eq!(
                BucketFingerprint::from_bucket(
                    res.bucket_fingerprint.owner_vertex_id,
                    res.bucket_fingerprint.label_id,
                    &bucket,
                    &vertex
                ),
                res.bucket_fingerprint,
                "bucket occupancy changed between reserve and commit"
            );

            let _updated_stored_slots = bucket
                .stored_slots
                .checked_add(res.edge_slot_count)
                .expect("reserve guaranteed stored_slots overflow safety");
            let _updated_degree = bucket
                .degree
                .checked_add(res.edge_slot_count)
                .expect("reserve guaranteed degree overflow safety");
            if res.inline_value_width > 0 {
                let _updated_payload_slots = bucket
                    .inline_value_slab_slots()
                    .checked_add(res.edge_slot_count)
                    .expect("reserve guaranteed payload slab slot overflow safety");
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
        let mut payload_slots_written: u64 = 0;
        for (run, res) in self.plan.runs.iter().zip(self.runs.iter()) {
            let mut edge_bytes = Vec::with_capacity(run.edges.len() * E::BYTES);
            for e in &run.edges {
                let mut buf = vec![0u8; E::BYTES];
                e.edge.write_to(&mut buf);
                edge_bytes.extend_from_slice(&buf);
            }
            graph
                .edges
                .write_slots_contiguous(res.edge_start_slot, &edge_bytes)
                .expect("reserve guaranteed edge slab capacity");

            if res.inline_value_width > 0 {
                let payload_offset = res
                    .payload_byte_offset
                    .expect("reserve guaranteed a payload byte offset for payload-bearing run");
                let payload_bytes = run
                    .edges
                    .iter()
                    .flat_map(|e| e.edge.edge_inline_value_bytes().iter().copied())
                    .collect::<Vec<u8>>();
                assert_eq!(
                    payload_bytes.len() as u64,
                    res.payload_byte_count,
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

        // Second pass: publish all bucket metadata and segment counts.
        for res in &self.runs {
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

            let mut updated_bucket = bucket
                .with_stored_slots(
                    bucket
                        .stored_slots
                        .checked_add(res.edge_slot_count)
                        .expect("reserve guaranteed stored_slots overflow safety"),
                )
                .with_degree_field(
                    bucket
                        .degree
                        .checked_add(res.edge_slot_count)
                        .expect("reserve guaranteed degree overflow safety"),
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
                        res.payload_byte_offset
                            .expect("reserve payload offset for new payload span"),
                    );
                }
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
        // vertex span, so the vertex row is unchanged.
        let mut vertex_stored_slot_ends: std::collections::BTreeMap<VertexId, u64> =
            std::collections::BTreeMap::new();
        for res in &self.runs {
            let end = res.edge_start_slot + u64::from(res.edge_slot_count);
            let entry = vertex_stored_slot_ends
                .entry(res.bucket_fingerprint.owner_vertex_id)
                .or_default();
            *entry = (*entry).max(end);
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
            .checked_add(edge_slots_written)
            .expect("reserve guaranteed num_edges overflow safety");
        graph.edges.set_num_edges(next_num_edges);

        OneOrientationBatchResult {
            edge_slots_written: edge_slots_written as u32,
            edge_log_entries_written: 0,
            payload_slots_written: payload_slots_written as u32,
            payload_log_entries_written: 0,
        }
    }
}

/// Result of a committed one-orientation batch write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OneOrientationBatchResult {
    /// Number of edge slab slots written.
    pub edge_slots_written: u32,
    /// Number of edge overflow-log entries written.
    pub edge_log_entries_written: u32,
    /// Number of payload slab slots written.
    pub payload_slots_written: u32,
    /// Number of payload overflow-log entries written.
    pub payload_log_entries_written: u32,
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
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
    use crate::VertexId;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::graph::test_support::{TestEdge as GraphTestEdge, test_graph_with_default};
    use crate::labeled::record::LabeledVertex;

    use super::{
        OneOrientationBatchEdge, OneOrientationBatchError, OneOrientationBatchPlan,
        OneOrientationBucketRun,
    };

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
    fn reserve_rejects_slab_capacity_exceeded() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        // Fill the bucket slab window with one edge per slot.
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

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::SlabCapacityExceeded),
            "expected SlabCapacityExceeded, got {err}"
        );
    }
}
