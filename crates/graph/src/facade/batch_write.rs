//! Internal GraphStore clean-slab batch orchestration for ADR 0045.
//!
//! This module consumes physical half-edge intents produced by
//! [`super::batch_placement`] and attempts to commit them through the LARA
//! one-orientation batch primitive.  Unsupported geometry is returned to the
//! caller before any canonical write so the existing scalar path can handle it.
//! No LARA placement policy leaks outside this module.

use gleaph_graph_kernel::federation::{HOT_FORWARD_EDGE_INSERT_THRESHOLD, LocalVertexId};
use gleaph_graph_kernel::plan_exec::{
    GraphMutationRequestIdentityV1, GraphOrderedEdgeBatchReceiptV1, GraphOrderedEdgeBatchResult,
    GraphOrderedEdgeBatchResultV1, GraphOrderedMixedBatchReceiptV1, GraphOrderedMixedBatchResult,
    GraphOrderedMixedBatchResultV1, LabelStatsDelta, MutationId,
};
use std::collections::BTreeMap;

use super::GraphStore;
use super::batch_placement::{BatchEdgeInput, BatchPlacementError};
use super::mutation_executor::GraphMutationExecutor;
use crate::facade::GraphStoreError;

#[derive(Debug)]
pub(crate) enum OrderedEdgeBatchExecutionError {
    Validation(BatchPlacementError),
    /// A Building/Sealing index DML failed its Graph-owned admission before any batch write.
    IndexBuildAdmission(GraphStoreError),
}

impl std::fmt::Display for OrderedEdgeBatchExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(error) => write!(f, "ordered Graph batch validation failed: {error}"),
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
        vertex_label_stats_delta: LabelStatsDelta,
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

        let mut label_stats_delta = ordered_edge_label_stats_delta(edges);
        label_stats_delta.vertex = vertex_label_stats_delta.vertex;
        let (emitted_delta_first_seq, emitted_delta_last_seq) =
            self.commit_ordered_label_stats_delta(mutation_id, label_stats_delta);
        let receipt = GraphOrderedMixedBatchReceiptV1 {
            logical_operation_count,
            logical_vertex_count,
            logical_edge_count: edges.len() as u64,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: ordered_hot_forward_vertices(edges).into(),
            allocated_vertex_ids: allocated_vertex_ids.clone().into(),
        };
        receipt
            .validate()
            .expect("mixed Graph receipt must satisfy its bounded contract");
        self.commit_record_completed_ordered_mixed_batch_journal(
            crate::facade::JournalDelta {
                mutation_id,
                row_count: logical_operation_count,
                emitted_delta_first_seq,
                emitted_delta_last_seq,
                hot_forward_vertices: receipt.hot_forward_vertices.to_vec(),
            },
            request_identity,
            allocated_vertex_ids,
        );
        Ok(GraphOrderedMixedBatchResult::V1(
            GraphOrderedMixedBatchResultV1::Completed(receipt),
        ))
    }

    pub(crate) fn commit_ordered_edge_batch_receipt(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        edges: &[BatchEdgeInput],
    ) -> GraphOrderedEdgeBatchResult {
        let label_stats_delta = ordered_edge_label_stats_delta(edges);
        let (emitted_delta_first_seq, emitted_delta_last_seq) =
            self.commit_ordered_label_stats_delta(mutation_id, label_stats_delta);
        let hot_forward_vertices = ordered_hot_forward_vertices(edges);
        let row_count = u64::try_from(edges.len())
            .expect("ordered Graph item count was validated before batch execution");
        let receipt = GraphOrderedEdgeBatchReceiptV1 {
            logical_edge_count: row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: hot_forward_vertices.clone().into(),
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
}

impl GraphStore {
    /// The one place that appends an ordered family's label delta after its canonical write and
    /// returns the emitted event range used by the journal and receipt.
    pub(crate) fn commit_ordered_label_stats_delta(
        &self,
        mutation_id: MutationId,
        delta: LabelStatsDelta,
    ) -> (
        Option<gleaph_graph_kernel::plan_exec::ShardEventSeq>,
        Option<gleaph_graph_kernel::plan_exec::ShardEventSeq>,
    ) {
        if delta.vertex.is_empty() && delta.edge.is_empty() {
            return (None, None);
        }
        let event = self
            .commit_append_label_stats_delta(mutation_id, delta)
            .unwrap_or_else(|error| {
                panic!("ordered Graph label delta append after canonical write: {error}")
            });
        (Some(event.shard_event_seq), Some(event.shard_event_seq))
    }
}

pub(crate) fn vertex_label_stats_delta(
    labels_per_vertex: &[Vec<gleaph_graph_kernel::entry::VertexLabelId>],
) -> LabelStatsDelta {
    let mut vertex = Vec::new();
    for label in labels_per_vertex.iter().flatten() {
        if let Some((_, count)) = vertex.iter_mut().find(|(existing, _)| existing == label) {
            *count += 1;
        } else {
            vertex.push((*label, 1));
        }
    }
    LabelStatsDelta {
        vertex,
        edge: Vec::new(),
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
