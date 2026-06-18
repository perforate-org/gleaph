//! GraphStore `edge_scan` implementation.

use super::super::stable::GRAPH;
use gleaph_graph_kernel::entry::{Edge, EdgeDirectedness, EdgeLabelId};
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledError, VertexId,
    labeled::{
        BucketDirectedness, LabeledEdgePayloadBatch, LabeledEdgePayloadBatchScratch, OutEdgeOrder,
    },
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::helpers::wire_catalog_label;

impl GraphStore {
    pub fn directed_out_edges(&self, vertex_id: VertexId) -> Result<Vec<Edge>, GraphStoreError> {
        let mut edges = Vec::new();
        self.for_each_directed_out_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            edges.push(edge)
        })?;
        Ok(edges)
    }

    pub fn directed_in_edges(&self, vertex_id: VertexId) -> Result<Vec<Edge>, GraphStoreError> {
        let mut edges = Vec::new();
        self.for_each_directed_in_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            edges.push(edge)
        })?;
        Ok(edges)
    }

    pub fn undirected_edges(&self, vertex_id: VertexId) -> Result<Vec<Edge>, GraphStoreError> {
        let mut edges = Vec::new();
        self.for_each_undirected_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            edges.push(edge);
        })?;
        Ok(edges)
    }

    pub(crate) fn for_each_out_edges_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| graph.for_each_out_edges_for_label(vertex_id, label, visit))
    }

    pub(crate) fn for_each_out_edges_for_label_ordered<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| {
            graph.for_each_out_edges_for_label_ordered(vertex_id, label, order, visit)
        })
    }

    pub(crate) fn visit_out_payload_value_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        scratch: &mut ic_stable_lara::labeled::LabeledPayloadValueBatchScratch,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(ic_stable_lara::labeled::LabeledPayloadValueBatch<'b>),
    {
        GRAPH.with_borrow(|graph| {
            graph.visit_out_payload_value_batches_for_label(vertex_id, label, order, scratch, visit)
        })
    }

    pub(crate) fn out_label_bucket_dense_payload_batch_eligible(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
    ) -> Result<bool, GraphStoreError> {
        Ok(GRAPH
            .with_borrow(|graph| graph.out_bucket_dense_payload_batch_eligible(vertex_id, label))?)
    }

    pub(crate) fn in_label_bucket_dense_payload_batch_eligible(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
    ) -> Result<bool, GraphStoreError> {
        Ok(GRAPH
            .with_borrow(|graph| graph.in_bucket_dense_payload_batch_eligible(vertex_id, label))?)
    }

    pub(crate) fn out_label_bucket_payload_first_predicate_eligible(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
    ) -> Result<bool, GraphStoreError> {
        Ok(GRAPH.with_borrow(|graph| {
            graph.out_bucket_payload_first_predicate_eligible(vertex_id, label)
        })?)
    }

    pub(crate) fn in_label_bucket_payload_first_predicate_eligible(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
    ) -> Result<bool, GraphStoreError> {
        Ok(GRAPH.with_borrow(|graph| {
            graph.in_bucket_payload_first_predicate_eligible(vertex_id, label)
        })?)
    }

    pub(crate) fn visit_in_payload_value_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        scratch: &mut ic_stable_lara::labeled::LabeledPayloadValueBatchScratch,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(ic_stable_lara::labeled::LabeledPayloadValueBatch<'b>),
    {
        GRAPH.with_borrow(|graph| {
            graph.visit_in_payload_value_batches_for_label(vertex_id, label, order, scratch, visit)
        })
    }

    pub(crate) fn read_out_edge_slots_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        slots: &[u32],
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| {
            graph.read_out_edge_slots_for_label(vertex_id, label, slots, order, visit)
        })
    }

    pub(crate) fn read_out_edge_slots_for_label_reusing_payload_scratch<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        slots: &[u32],
        order: OutEdgeOrder,
        scratch: &ic_stable_lara::labeled::LabeledPayloadValueBatchScratch,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        let replay = scratch
            .hybrid_overflow_replay
            .is_active()
            .then_some(&scratch.hybrid_overflow_replay);
        GRAPH.with_borrow(|graph| {
            graph.read_out_edge_slots_for_label_with_replay(
                vertex_id, label, slots, order, replay, visit,
            )
        })
    }

    pub(crate) fn read_in_edge_slots_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        slots: &[u32],
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| {
            graph.read_in_edge_slots_for_label(vertex_id, label, slots, order, visit)
        })
    }

    pub(crate) fn read_in_edge_slots_for_label_reusing_payload_scratch<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        slots: &[u32],
        order: OutEdgeOrder,
        scratch: &ic_stable_lara::labeled::LabeledPayloadValueBatchScratch,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        let replay = scratch
            .hybrid_overflow_replay
            .is_active()
            .then_some(&scratch.hybrid_overflow_replay);
        GRAPH.with_borrow(|graph| {
            graph.read_in_edge_slots_for_label_with_replay(
                vertex_id, label, slots, order, replay, visit,
            )
        })
    }

    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn visit_directed_out_payload_value_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut ic_stable_lara::labeled::LabeledPayloadValueBatchScratch,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: for<'b> FnMut(ic_stable_lara::labeled::LabeledPayloadValueBatch<'b>),
    {
        self.visit_out_payload_value_batches_for_label(
            vertex_id,
            LaraLabelId::from_raw(label.pack(EdgeDirectedness::Directed).raw()),
            order,
            scratch,
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn read_directed_out_edge_slots_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        slots: &[u32],
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.read_out_edge_slots_for_label(
            vertex_id,
            LaraLabelId::from_raw(label.pack(EdgeDirectedness::Directed).raw()),
            slots,
            order,
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn visit_directed_out_edge_payload_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgePayloadBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: for<'b> FnMut(LabeledEdgePayloadBatch<'b, Edge>),
    {
        self.visit_out_edge_payload_batches_for_label(
            vertex_id,
            LaraLabelId::from_raw(label.pack(EdgeDirectedness::Directed).raw()),
            order,
            scratch,
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    pub(crate) fn visit_out_edge_payload_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgePayloadBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(LabeledEdgePayloadBatch<'b, Edge>),
    {
        GRAPH.with_borrow(|graph| {
            graph.visit_out_edge_payload_batches_for_label(vertex_id, label, order, scratch, visit)
        })
    }

    pub(crate) fn visit_in_edge_payload_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgePayloadBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(LabeledEdgePayloadBatch<'b, Edge>),
    {
        GRAPH.with_borrow(|graph| {
            graph.visit_in_edge_payload_batches_for_label(vertex_id, label, order, scratch, visit)
        })
    }

    pub(crate) fn for_each_directed_out_edges_for_label_with_payloads<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        self.for_each_directed_out_edges_for_label_with_payloads_reusing(
            vertex_id,
            label,
            order,
            &mut scratch,
            visit,
        )
    }

    pub(crate) fn for_each_directed_out_edges_for_label_with_payload_slices_reusing<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgePayloadBatchScratch<Edge>,
        mut visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(&Edge, &[u8]),
    {
        if self
            .edge_label_payload_profile(label)
            .is_some_and(|profile| profile.required_byte_width() > 0)
        {
            let storage_label = LaraLabelId::from_raw(label.pack(EdgeDirectedness::Directed).raw());
            self.visit_out_edge_payload_batches_for_label(
                vertex_id,
                storage_label,
                order,
                scratch,
                |batch| {
                    let width = usize::from(batch.byte_width);
                    debug_assert_eq!(batch.payload_bytes.len(), batch.edges.len() * width);
                    for (edge, value) in batch
                        .edges
                        .iter()
                        .zip(batch.payload_bytes.chunks_exact(width))
                    {
                        visit(edge, value);
                    }
                },
            )
            .map_err(GraphStoreError::from)
        } else {
            self.for_each_directed_out_edges_for_label(vertex_id, label, order, |edge| {
                visit(&edge, edge.payload_bytes());
            })
        }
    }

    pub(crate) fn for_each_directed_out_edges_for_label_with_payloads_reusing<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgePayloadBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        let mut visit = visit;
        self.for_each_directed_out_edges_for_label_with_payload_slices_reusing(
            vertex_id,
            label,
            order,
            scratch,
            |edge, value| visit(edge.with_payload_bytes(value)),
        )
    }

    pub(crate) fn for_each_directed_out_edges_for_label_topology_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_out_edges_for_label_topology_unchecked(
                    vertex_id,
                    wire_catalog_label(Some(label), EdgeDirectedness::Directed),
                    order,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    pub(crate) fn for_each_directed_in_edges_for_label_topology_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_in_edges_for_label_topology_ordered(
                    vertex_id,
                    wire_catalog_label(Some(label), EdgeDirectedness::Directed),
                    order,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    pub(crate) fn for_each_directed_in_edges_for_label_with_payloads<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        self.for_each_directed_in_edges_for_label_with_payloads_reusing(
            vertex_id,
            label,
            order,
            &mut scratch,
            visit,
        )
    }

    pub(crate) fn for_each_directed_in_edges_for_label_with_payload_slices_reusing<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgePayloadBatchScratch<Edge>,
        mut visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(&Edge, &[u8]),
    {
        if self
            .edge_label_payload_profile(label)
            .is_some_and(|profile| profile.required_byte_width() > 0)
        {
            let storage_label = LaraLabelId::from_raw(label.pack(EdgeDirectedness::Directed).raw());
            self.visit_in_edge_payload_batches_for_label(
                vertex_id,
                storage_label,
                order,
                scratch,
                |batch| {
                    let width = usize::from(batch.byte_width);
                    debug_assert_eq!(batch.payload_bytes.len(), batch.edges.len() * width);
                    for (edge, value) in batch
                        .edges
                        .iter()
                        .zip(batch.payload_bytes.chunks_exact(width))
                    {
                        visit(edge, value);
                    }
                },
            )
            .map_err(GraphStoreError::from)
        } else {
            self.for_each_directed_in_edges_for_label(vertex_id, label, order, |edge| {
                visit(&edge, edge.payload_bytes());
            })
        }
    }

    pub(crate) fn for_each_directed_in_edges_for_label_with_payloads_reusing<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgePayloadBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        let mut visit = visit;
        self.for_each_directed_in_edges_for_label_with_payload_slices_reusing(
            vertex_id,
            label,
            order,
            scratch,
            |edge, value| visit(edge.with_payload_bytes(value)),
        )
    }

    pub(crate) fn for_each_out_edges_for_label_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = canbench_rs::bench_scope("graph_store_tls_out_label_unchecked");
        GRAPH.with_borrow(|graph| {
            graph.for_each_out_edges_for_label_unchecked(vertex_id, label, visit)
        })
    }

    pub(crate) fn skip_then_visit_each_out_edge_for_label<Visit, Err>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, GraphStoreError>
    where
        Visit: FnMut(Edge) -> Result<bool, Err>,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.skip_then_visit_each_forward_out_edge_for_label(
                    vertex_id,
                    label,
                    offset_remaining,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    pub(crate) fn skip_then_visit_each_directed_out_edge<Visit, Err>(
        &self,
        vertex_id: VertexId,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, GraphStoreError>
    where
        Visit: FnMut(Edge) -> Result<bool, Err>,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.skip_then_visit_each_forward_out_edge_by_directedness(
                    vertex_id,
                    BucketDirectedness::Directed,
                    offset_remaining,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    pub(crate) fn skip_then_visit_each_undirected_edge<Visit, Err>(
        &self,
        vertex_id: VertexId,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, GraphStoreError>
    where
        Visit: FnMut(Edge) -> Result<bool, Err>,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.skip_then_visit_each_forward_out_edge_by_directedness(
                    vertex_id,
                    BucketDirectedness::Undirected,
                    offset_remaining,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    pub(crate) fn skip_then_visit_each_in_edge_for_label<Visit, Err>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, GraphStoreError>
    where
        Visit: FnMut(Edge) -> Result<bool, Err>,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.skip_then_visit_each_reverse_out_edge_for_label(
                    vertex_id,
                    label,
                    offset_remaining,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    pub(crate) fn skip_then_visit_each_directed_in_edge<Visit, Err>(
        &self,
        vertex_id: VertexId,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, GraphStoreError>
    where
        Visit: FnMut(Edge) -> Result<bool, Err>,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.skip_then_visit_each_reverse_out_edge_by_directedness(
                    vertex_id,
                    BucketDirectedness::Directed,
                    offset_remaining,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    pub(crate) fn for_each_in_edges_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| graph.for_each_in_edges_for_label(vertex_id, label, visit))
    }

    pub(crate) fn for_each_in_edges_for_label_ordered<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| {
            graph.for_each_in_edges_for_label_ordered(vertex_id, label, order, visit)
        })
    }

    pub(crate) fn for_each_in_edges_for_label_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = canbench_rs::bench_scope("graph_store_tls_in_label_unchecked");
        GRAPH.with_borrow(|graph| {
            graph.for_each_in_edges_for_label_unchecked(vertex_id, label, visit)
        })
    }

    pub fn for_each_directed_out_edges_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_out_edges_for_label_ordered(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Directed),
            order,
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    pub fn for_each_directed_out_edges_for_label_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_out_edges_for_label_unchecked(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Directed),
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    pub fn for_each_directed_in_edges_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_in_edges_for_label_ordered(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Directed),
            order,
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    pub fn for_each_directed_in_edges_for_label_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_in_edges_for_label_unchecked(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Directed),
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    pub fn for_each_undirected_edges_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_out_edges_for_label_ordered(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Undirected),
            order,
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    pub fn for_each_undirected_edges_for_label_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_out_edges_for_label_unchecked(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Undirected),
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    pub fn for_each_directed_out_edges<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| graph.for_each_directed_out_edges(vertex_id, order, visit))
            .map_err(GraphStoreError::from)
    }

    pub fn for_each_directed_in_edges<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| graph.for_each_directed_in_edges(vertex_id, order, visit))
            .map_err(GraphStoreError::from)
    }

    pub fn for_each_undirected_edges<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| graph.for_each_undirected_edges(vertex_id, order, visit))
            .map_err(GraphStoreError::from)
    }

    pub fn for_each_undirected_edges_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| graph.for_each_undirected_edges_unchecked(vertex_id, order, visit))
            .map_err(GraphStoreError::from)
    }
}
