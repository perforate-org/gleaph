//! Failure-atomic one-orientation batch writes for [`LabeledLaraGraph`].
//!
//! This module implements the smallest LARA-internal batch mutation slice from
//! ADR 0045: one orientation, one bounded batch, and an explicit
//! `plan -> reserve -> commit` boundary.  It reuses the existing LARA ownership
//! and placement types and does not add a public or Candid API.
//!
//! The initial slice only supports bucket runs that fit cleanly in an existing
//! clean slab window without overflow-log growth, rebalance, relocation, or
//! dynamic leaf expansion.  Unsupported geometries fail closed and leave
//! canonical state untouched.

use crate::{
    VertexId,
    labeled::{bucket_label_key::BucketLabelKey, slot_index::checked_add_slot_index},
    lara::operation_error::LaraOperationError,
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
    /// Edges in this bucket run, in ordinal order.
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

/// Per-run reservation produced by [`LabeledLaraGraph::reserve_one_orientation_batch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneOrientationBatchReservation {
    /// Vertex that owns the bucket row.
    pub owner_vertex_id: VertexId,
    /// Storage label, including directedness bit.
    pub label_id: BucketLabelKey,
    /// First edge slab slot written for the run.
    pub edge_start_slot: u64,
    /// Number of edge slots reserved for the run.
    pub edge_slot_count: u32,
    /// Byte width per inline value slot (`0` = no payload).
    pub inline_value_width: u16,
    /// Byte offset in the payload slab where the run writes, if payload-bearing.
    pub payload_byte_offset: Option<u64>,
    /// Number of payload bytes reserved.
    pub payload_byte_count: u64,
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
    /// A stable-memory reservation or write failed.
    StorageError(LabeledOperationError),
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Validate a one-orientation batch plan and reserve the required capacity.
    ///
    /// This is the `reserve` step of the ADR 0045 boundary.  It performs all
    /// fallible validation and backing-capacity reservation before any canonical
    /// adjacency or payload byte is written.  A failure here leaves the store
    /// unchanged; capacity that was reserved (e.g., grown edge/payload backing
    /// memory) is non-canonical preallocation and is safe to retain.
    pub fn reserve_one_orientation_batch(
        &self,
        plan: &OneOrientationBatchPlan<E>,
    ) -> Result<Vec<OneOrientationBatchReservation>, OneOrientationBatchError> {
        // Phase 1: validate plan invariants without touching canonical state.
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

        // Phase 2: preflight every reservation and required backing capacity.
        let mut reservations = Vec::with_capacity(plan.runs.len());
        for run in &plan.runs {
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
            let window_end = self
                .bucket_slab_window_end_exclusive_after_bucket(&vertex, bucket_index, &bucket)
                .map_err(OneOrientationBatchError::from)?;
            if edge_end_slot > window_end {
                return Err(OneOrientationBatchError::SlabCapacityExceeded);
            }

            self.edges.set_elem_capacity(edge_end_slot).map_err(|e| {
                OneOrientationBatchError::StorageError(LabeledOperationError::Store(
                    LaraOperationError::ResizeFailed(e),
                ))
            })?;

            let mut payload_byte_offset = None;
            let mut payload_byte_count: u64 = 0;
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
                let span_ends_at_tail =
                    offset.checked_add(had_bytes).is_some_and(|end| end == tail);

                if had_bytes == 0 {
                    let bytes = self
                        .values
                        .allocate_byte_span(total_payload_bytes)
                        .map_err(|e| {
                            OneOrientationBatchError::StorageError(LabeledOperationError::Store(
                                LaraOperationError::ResizeFailed(e),
                            ))
                        })?;
                    payload_byte_offset = Some(bytes);
                } else if span_ends_at_tail {
                    let grown = self
                        .values
                        .grow_byte_span_in_place(offset, had_bytes, total_payload_bytes)
                        .map_err(|e| {
                            OneOrientationBatchError::StorageError(LabeledOperationError::Store(
                                LaraOperationError::ResizeFailed(e),
                            ))
                        })?;
                    if !grown {
                        return Err(OneOrientationBatchError::UnsupportedGeometry(
                            "payload span is not at occupied tail and cannot grow in place".into(),
                        ));
                    }
                    payload_byte_offset = Some(offset);
                } else {
                    return Err(OneOrientationBatchError::UnsupportedGeometry(
                        "payload span is not at occupied tail and cannot grow in place".into(),
                    ));
                }
                payload_byte_count = total_payload_bytes;
            }

            reservations.push(OneOrientationBatchReservation {
                owner_vertex_id: run.owner_vertex_id,
                label_id: run.label_id,
                edge_start_slot,
                edge_slot_count,
                inline_value_width: run.inline_value_width,
                payload_byte_offset,
                payload_byte_count,
            });
        }
        Ok(reservations)
    }

    /// Commit a previously reserved one-orientation batch plan.
    ///
    /// This is the `commit` step.  It must be called only after
    /// [`reserve_one_orientation_batch`] has returned successfully for the same plan.
    ///
    /// All fallible validation and capacity reservation was performed during reserve.
    /// This function therefore performs only the infallible canonical byte writes and
    /// metadata updates.  A panic here is an invariant violation rather than a
    /// recoverable error.
    pub fn commit_one_orientation_batch(
        &self,
        plan: OneOrientationBatchPlan<E>,
        reservation: Vec<OneOrientationBatchReservation>,
    ) {
        assert_eq!(
            plan.runs.len(),
            reservation.len(),
            "plan/reservation length mismatch: reserve and commit must be paired"
        );

        let mut edge_slots_written: u64 = 0;
        let mut payload_slots_written: u64 = 0;

        // First pass: write all edge and payload bytes.  No recoverable failure is
        // possible here because reserve already guaranteed capacity and layout.
        for (run, res) in plan.runs.iter().zip(reservation.iter()) {
            let mut edge_bytes = Vec::with_capacity(run.edges.len() * E::BYTES);
            for e in &run.edges {
                let mut buf = vec![0u8; E::BYTES];
                e.edge.write_to(&mut buf);
                edge_bytes.extend_from_slice(&buf);
            }
            self.edges
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
                self.values
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
        for (_run, res) in plan.runs.into_iter().zip(reservation) {
            let vertex = self.vertices.get(res.owner_vertex_id);
            let (bucket_slot, bucket) = match self
                .find_bucket(res.owner_vertex_id, &vertex, res.label_id)
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
            self.buckets
                .write_label_bucket_slot(bucket_slot, updated_bucket)
                .expect("reserve guaranteed bucket slot writability");

            self.edges
                .bump_vertex_segment_counts(res.owner_vertex_id, i64::from(res.edge_slot_count), 0)
                .expect("reserve guaranteed segment count overflow safety");
        }

        // Finally update the global edge count.
        let next_num_edges = self
            .edges
            .header()
            .num_edges
            .checked_add(edge_slots_written)
            .expect("reserve guaranteed num_edges overflow safety");
        self.edges.set_num_edges(next_num_edges);
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
