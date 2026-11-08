//! Internal GraphStore clean-slab batch orchestration for ADR 0045.
//!
//! This module consumes physical half-edge intents produced by
//! [`super::batch_placement`] and attempts to commit them through the LARA
//! one-orientation batch primitive.  Unsupported geometry is returned to the
//! caller before any canonical write so the existing scalar path can handle it.
//! No LARA placement policy leaks outside this module.

use gleaph_graph_kernel::entry::EdgeLabelId;
use gleaph_graph_kernel::federation::{HOT_FORWARD_EDGE_INSERT_THRESHOLD, LocalVertexId};
use gleaph_graph_kernel::plan_exec::{
    GraphMutationRequestIdentityV1, GraphOrderedEdgeBatchReceiptV1, GraphOrderedEdgeBatchResult,
    GraphOrderedEdgeBatchResultV1, GraphOrderedMixedBatchReceiptV1, GraphOrderedMixedBatchResult,
    GraphOrderedMixedBatchResultV1, LabelStatsDelta, MutationId,
};
use ic_stable_lara::VertexId;
use std::collections::BTreeMap;

use super::GraphStore;
use super::batch_placement::{BatchEdgeInput, BatchPlacementError};
use super::mutation_executor::GraphMutationExecutor;
use super::store::helpers::{edge_storage_label, lara_label};
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

        let label_stats_delta = ordered_edge_label_stats_delta(edges);
        let delta_event = (!label_stats_delta.edge.is_empty()).then(|| {
            self.commit_append_label_stats_delta(mutation_id, label_stats_delta)
                .unwrap_or_else(|error| {
                    panic!("mixed Graph label delta append after canonical write: {error}")
                })
        });
        let (emitted_delta_first_seq, emitted_delta_last_seq) = delta_event
            .as_ref()
            .map(|event| (Some(event.shard_event_seq), Some(event.shard_event_seq)))
            .unwrap_or((None, None));
        let receipt = GraphOrderedMixedBatchReceiptV1 {
            logical_operation_count,
            logical_vertex_count,
            logical_edge_count: edges.len() as u64,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: ordered_hot_forward_vertices(edges),
            allocated_vertex_ids: allocated_vertex_ids.clone(),
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
                hot_forward_vertices: receipt.hot_forward_vertices.clone(),
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

#[cfg(test)]
mod tests {
    use super::super::batch_placement::BatchEdgeInput;
    use super::super::store::helpers::{edge_storage_label, lara_label};
    use super::*;
    use crate::test_labels::install_test_edge_inline_property_profile;
    use gleaph_graph_kernel::entry::EdgeLabelId;
    use ic_stable_lara::VertexId;
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

    /// One vertex's observable adjacency: the edges in ordinal order, with the neighbor written
    /// as its position inside the group under test and the inline bytes the read boundary hands
    /// back. Order is compared; absolute slot addresses and allocator bookkeeping are not - a
    /// batch may allocate differently, it may not reorder or drop edges.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ObservableView {
        out: Vec<(u8, u16, Vec<u8>)>,
        undirected: Vec<(u8, u16, Vec<u8>)>,
        in_: Vec<(u8, u16, Vec<u8>)>,
    }

    impl ObservableView {
        fn of(store: &GraphStore, vid: VertexId, group: &[VertexId]) -> Self {
            let position = |target: VertexId| -> u8 {
                group
                    .iter()
                    .position(|candidate| *candidate == target)
                    .map_or(u8::MAX, |index| index as u8)
            };
            let project = |edges: Vec<gleaph_graph_kernel::entry::Edge>| {
                edges
                    .into_iter()
                    .map(|edge| {
                        let target = edge.target.local_id();
                        (
                            position(target),
                            edge.label_id,
                            edge.inline_property.as_slice().to_vec(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            Self {
                out: project(store.directed_out_edges(vid).expect("out edges")),
                undirected: project(store.undirected_edges(vid).expect("undirected edges")),
                in_: project(store.directed_in_edges(vid).expect("in edges")),
            }
        }
    }

    fn observable_state(store: &GraphStore, group: &[VertexId]) -> Vec<ObservableView> {
        group
            .iter()
            .map(|vid| ObservableView::of(store, *vid, group))
            .collect()
    }

    /// The reference implementation: every edge through the scalar path, one at a time.
    fn scalar_edges(store: &GraphStore, edges: &[BatchEdgeInput]) {
        store.execute_ordered_scalar_edges(edges, 0);
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
}
