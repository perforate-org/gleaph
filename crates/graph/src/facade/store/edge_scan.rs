//! GraphStore `edge_scan` implementation.

use super::super::stable::GRAPH;
use gleaph_graph_kernel::entry::{Edge, EdgeDirectedness, EdgeLabelId};
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledError, VertexId,
    labeled::{
        BucketDirectedness, LabeledEdgeInlinePropertyBatch, LabeledEdgeInlinePropertyBatchScratch,
        OutEdgeOrder,
    },
    traits::CsrEdge,
    traverse::BucketEntryPosition,
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

    pub(crate) fn visit_out_inline_property_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        scratch: &mut ic_stable_lara::labeled::LabeledInlinePropertyValueBatchScratch,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(ic_stable_lara::labeled::LabeledInlinePropertyValueBatch<'b>),
    {
        GRAPH.with_borrow(|graph| {
            graph.visit_out_inline_property_batches_for_label(
                vertex_id, label, order, scratch, visit,
            )
        })
    }

    pub(crate) fn out_label_bucket_dense_inline_property_batch_eligible(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
    ) -> Result<bool, GraphStoreError> {
        Ok(GRAPH.with_borrow(|graph| {
            graph.out_bucket_dense_inline_property_batch_eligible(vertex_id, label)
        })?)
    }

    pub(crate) fn in_label_bucket_dense_inline_property_batch_eligible(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
    ) -> Result<bool, GraphStoreError> {
        Ok(GRAPH.with_borrow(|graph| {
            graph.in_bucket_dense_inline_property_batch_eligible(vertex_id, label)
        })?)
    }

    pub(crate) fn out_label_bucket_inline_property_bytes_first_predicate_eligible(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
    ) -> Result<bool, GraphStoreError> {
        Ok(GRAPH.with_borrow(|graph| {
            graph.out_bucket_inline_property_bytes_first_predicate_eligible(vertex_id, label)
        })?)
    }

    pub(crate) fn in_label_bucket_inline_property_bytes_first_predicate_eligible(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
    ) -> Result<bool, GraphStoreError> {
        Ok(GRAPH.with_borrow(|graph| {
            graph.in_bucket_inline_property_bytes_first_predicate_eligible(vertex_id, label)
        })?)
    }

    pub(crate) fn visit_in_inline_property_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        scratch: &mut ic_stable_lara::labeled::LabeledInlinePropertyValueBatchScratch,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(ic_stable_lara::labeled::LabeledInlinePropertyValueBatch<'b>),
    {
        GRAPH.with_borrow(|graph| {
            graph
                .visit_in_inline_property_batches_for_label(vertex_id, label, order, scratch, visit)
        })
    }

    pub(crate) fn read_out_edge_slots_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        slots: &[BucketEntryPosition],
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

    pub(crate) fn read_out_edge_slots_for_label_reusing_inline_property_scratch<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        slots: &[BucketEntryPosition],
        order: OutEdgeOrder,
        scratch: &ic_stable_lara::labeled::LabeledInlinePropertyValueBatchScratch,
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
        slots: &[BucketEntryPosition],
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

    pub(crate) fn read_in_edge_slots_for_label_reusing_inline_property_scratch<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        slots: &[BucketEntryPosition],
        order: OutEdgeOrder,
        scratch: &ic_stable_lara::labeled::LabeledInlinePropertyValueBatchScratch,
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
    pub(crate) fn visit_directed_out_inline_property_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut ic_stable_lara::labeled::LabeledInlinePropertyValueBatchScratch,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: for<'b> FnMut(ic_stable_lara::labeled::LabeledInlinePropertyValueBatch<'b>),
    {
        self.visit_out_inline_property_batches_for_label(
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
        slots: &[BucketEntryPosition],
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
    pub(crate) fn visit_directed_out_edge_inline_property_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: for<'b> FnMut(LabeledEdgeInlinePropertyBatch<'b, Edge>),
    {
        self.visit_out_edge_inline_property_batches_for_label(
            vertex_id,
            LaraLabelId::from_raw(label.pack(EdgeDirectedness::Directed).raw()),
            order,
            scratch,
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    pub(crate) fn visit_out_edge_inline_property_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(LabeledEdgeInlinePropertyBatch<'b, Edge>),
    {
        GRAPH.with_borrow(|graph| {
            graph.visit_out_edge_inline_property_batches_for_label(
                vertex_id, label, order, scratch, visit,
            )
        })
    }

    pub(crate) fn visit_in_edge_inline_property_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(LabeledEdgeInlinePropertyBatch<'b, Edge>),
    {
        GRAPH.with_borrow(|graph| {
            graph.visit_in_edge_inline_property_batches_for_label(
                vertex_id, label, order, scratch, visit,
            )
        })
    }

    pub(crate) fn for_each_directed_out_edges_for_label_with_inline_property_bytes<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        let mut scratch = LabeledEdgeInlinePropertyBatchScratch::default();
        self.for_each_directed_out_edges_for_label_with_inline_property_bytes_reusing(
            vertex_id,
            label,
            order,
            &mut scratch,
            visit,
        )
    }

    pub(crate) fn for_each_directed_out_edges_for_label_with_inline_property_byte_slices_reusing<
        Visit,
    >(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<Edge>,
        mut visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(&Edge, &[u8]),
    {
        if self
            .edge_label_inline_property_profile(label)
            .is_some_and(|profile| profile.required_byte_width() > 0)
        {
            let storage_label = LaraLabelId::from_raw(label.pack(EdgeDirectedness::Directed).raw());
            self.visit_out_edge_inline_property_batches_for_label(
                vertex_id,
                storage_label,
                order,
                scratch,
                |batch| {
                    let width = usize::from(batch.byte_width);
                    debug_assert_eq!(batch.inline_property_bytes.len(), batch.edges.len() * width);
                    for (edge, value) in batch
                        .edges
                        .iter()
                        .zip(batch.inline_property_bytes.chunks_exact(width))
                    {
                        visit(edge, value);
                    }
                },
            )
            .map_err(GraphStoreError::from)
        } else {
            self.for_each_directed_out_edges_for_label(vertex_id, label, order, |edge| {
                visit(&edge, edge.edge_inline_property_bytes());
            })
        }
    }

    pub(crate) fn for_each_directed_out_edges_for_label_with_inline_property_bytes_reusing<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        let mut visit = visit;
        self.for_each_directed_out_edges_for_label_with_inline_property_byte_slices_reusing(
            vertex_id,
            label,
            order,
            scratch,
            |edge, value| {
                visit(
                    edge.clone()
                        .with_stored_inline_property_bytes(value.len() as u16, value),
                )
            },
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

    pub(crate) fn for_each_directed_in_edges_for_label_with_inline_property_bytes<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        let mut scratch = LabeledEdgeInlinePropertyBatchScratch::default();
        self.for_each_directed_in_edges_for_label_with_inline_property_bytes_reusing(
            vertex_id,
            label,
            order,
            &mut scratch,
            visit,
        )
    }

    pub(crate) fn for_each_directed_in_edges_for_label_with_inline_property_byte_slices_reusing<
        Visit,
    >(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<Edge>,
        mut visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(&Edge, &[u8]),
    {
        if self
            .edge_label_inline_property_profile(label)
            .is_some_and(|profile| profile.required_byte_width() > 0)
        {
            let storage_label = LaraLabelId::from_raw(label.pack(EdgeDirectedness::Directed).raw());
            self.visit_in_edge_inline_property_batches_for_label(
                vertex_id,
                storage_label,
                order,
                scratch,
                |batch| {
                    let width = usize::from(batch.byte_width);
                    debug_assert_eq!(batch.inline_property_bytes.len(), batch.edges.len() * width);
                    for (edge, value) in batch
                        .edges
                        .iter()
                        .zip(batch.inline_property_bytes.chunks_exact(width))
                    {
                        visit(edge, value);
                    }
                },
            )
            .map_err(GraphStoreError::from)
        } else {
            self.for_each_directed_in_edges_for_label(vertex_id, label, order, |edge| {
                visit(&edge, edge.edge_inline_property_bytes());
            })
        }
    }

    pub(crate) fn for_each_directed_in_edges_for_label_with_inline_property_bytes_reusing<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        let mut visit = visit;
        self.for_each_directed_in_edges_for_label_with_inline_property_byte_slices_reusing(
            vertex_id,
            label,
            order,
            scratch,
            |edge, value| {
                visit(
                    edge.clone()
                        .with_stored_inline_property_bytes(value.len() as u16, value),
                )
            },
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

    /// Visits at most `limit` logical slots for one forward label bucket without rescanning a
    /// prior cursor prefix. The returned `(next_slot, exhausted)` pair is suitable for a durable
    /// Graph-owned export cursor; tombstones still consume one bounded logical slot.
    pub(crate) fn visit_out_edge_window_from_slot<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        directedness: EdgeDirectedness,
        start_slot: u32,
        limit: u32,
        mut visit: Visit,
    ) -> Result<(u32, bool), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        let storage_label = LaraLabelId::from_raw(label.pack(directedness).raw());
        let (next_slot, exhausted) = GRAPH
            .with_borrow(|graph| {
                graph.visit_forward_edges_from_slot_with_inline_property(
                    vertex_id,
                    storage_label,
                    start_slot,
                    limit,
                    |_, edge| visit(edge),
                )
            })
            .map_err(GraphStoreError::from)?;
        Ok((next_slot, exhausted))
    }

    /// Enumerates canonical edges whose inline property bytes decode to at least one Active
    /// indexed value, for the inline domain of the edge property backfill export.
    ///
    /// `start` is the inclusive `(wire_label_id, owner_vertex_raw)` resume position. Only
    /// outgoing rows are visited: a directed edge's canonical identity is its forward row and
    /// an undirected pair's canonical owner is the max endpoint, so each logical edge yields
    /// exactly one entry carrying the same identity mutations dispatch. Vertices are visited
    /// whole, so the entry count may exceed `max_edges` by at most one vertex's matching
    /// degree. Returns the entries plus the resume position: `Some(next unstarted
    /// \`(wire label, owner)\`)` when the budget stopped enumeration, or `None` when every
    /// candidate edge was enumerated.
    pub(crate) fn scan_canonical_inline_property_edges(
        &self,
        start: Option<(u16, u32)>,
        max_edges: u32,
    ) -> Result<(Vec<InlinePropertyBackfillEntry>, Option<(u16, u32)>), String> {
        // Candidate wire labels from the router-supplied catalog: expand each Active
        // (catalog label, direction) registration into the concrete directed/undirected
        // bucket packings it covers.
        let mut wires: Vec<u16> = Vec::new();
        for (catalog_label_raw, direction) in
            crate::index::catalog_context::active_indexed_edge_label_registrations()
        {
            let catalog_label = EdgeLabelId::from_raw(catalog_label_raw);
            if direction.includes_directed() {
                wires.push(catalog_label.pack(EdgeDirectedness::Directed).raw());
            }
            if direction.includes_undirected() {
                wires.push(catalog_label.pack(EdgeDirectedness::Undirected).raw());
            }
        }
        wires.sort_unstable();
        wires.dedup();

        use gleaph_graph_kernel::entry::PropertyId;

        let cap = max_edges as usize;
        let vertex_len: u32 = GRAPH.with_borrow(|graph| u32::from(graph.vertex_count()));
        let mut entries: Vec<InlinePropertyBackfillEntry> = Vec::new();
        let mut decode_error: Option<String> = None;
        let mut resume: Option<(u16, u32)> = None;

        // Collects one candidate row. `canonical` is precomputed by the caller: directed
        // out-rows are always canonical; undirected rows are canonical on their max endpoint.
        // A started vertex is collected fully even past the budget: the resume position only
        // advances between vertices, so stopping mid-vertex would make the next call skip
        // this vertex's remaining edges.
        let consider = |wire: u16,
                        owner: u32,
                        slot_index: u32,
                        bytes: &[u8],
                        canonical: bool,
                        entries: &mut Vec<InlinePropertyBackfillEntry>,
                        decode_error: &mut Option<String>| {
            if decode_error.is_some() {
                return;
            }
            let values = match crate::property::inline_index_values(wire, bytes) {
                Ok(values) => values,
                Err(detail) => {
                    *decode_error = Some(detail);
                    return;
                }
            };
            if values.is_empty() {
                return;
            }
            if !canonical {
                return;
            }
            let values: Vec<(PropertyId, gleaph_gql::Value)> = values
                .into_iter()
                .map(|(_, property_id, value)| (property_id, value))
                .collect();
            entries.push(InlinePropertyBackfillEntry {
                wire_label_id: wire,
                owner_vertex_raw: owner,
                slot_index,
                values,
            });
        };

        'outer: for &wire in &wires {
            let begin_owner = match start {
                Some((start_wire, start_owner)) if start_wire == wire => start_owner,
                Some((start_wire, _)) if start_wire > wire => continue,
                _ => 0,
            };
            let is_directed_wire = LaraLabelId::from_raw(wire).is_directed();
            for owner in begin_owner..vertex_len {
                if entries.len() >= cap {
                    resume = Some((wire, owner));
                    break 'outer;
                }
                if is_directed_wire {
                    let mut scratch =
                        ic_stable_lara::labeled::LabeledInlinePropertyValueBatchScratch::default();
                    let visit_result = self.visit_out_inline_property_batches_for_label(
                        VertexId::from(owner),
                        LaraLabelId::from_raw(wire),
                        OutEdgeOrder::Ascending,
                        &mut scratch,
                        |batch| {
                            let width = usize::from(batch.byte_width);
                            for (index, &slot_index) in batch.slot_indices.iter().enumerate() {
                                consider(
                                    wire,
                                    owner,
                                    slot_index,
                                    &batch.values[index * width..(index + 1) * width],
                                    true,
                                    &mut entries,
                                    &mut decode_error,
                                );
                            }
                        },
                    );
                    if let Err(error) = visit_result {
                        return Err(format!("inline backfill traversal failed: {error}"));
                    }
                } else {
                    // Undirected buckets have no batch visitor; walk their rows directly and
                    // keep only the max-endpoint canonical owner.
                    let undirected_label = gleaph_graph_kernel::entry::EdgeLabelId::from_raw(wire);
                    let visit_result = self.for_each_undirected_edges_for_label(
                        VertexId::from(owner),
                        undirected_label,
                        OutEdgeOrder::Ascending,
                        |edge| {
                            consider(
                                wire,
                                owner,
                                edge.edge_slot_index_raw(),
                                edge.edge_inline_property_bytes(),
                                u64::from(owner) >= u64::from(edge.neighbor_vid()),
                                &mut entries,
                                &mut decode_error,
                            );
                        },
                    );
                    if let Err(error) = visit_result {
                        return Err(format!("inline backfill traversal failed: {error}"));
                    }
                }
                if let Some(detail) = decode_error.take() {
                    return Err(detail);
                }
            }
        }

        Ok((entries, resume))
    }
}

/// One canonical edge carrying indexed inline property values, enumerated by the inline
/// backfill domain of the edge property export.
pub(crate) struct InlinePropertyBackfillEntry {
    pub wire_label_id: u16,
    pub owner_vertex_raw: u32,
    pub slot_index: u32,
    /// Decoded indexed values, one per Active membership resolved from the current catalog.
    pub values: Vec<(gleaph_graph_kernel::entry::PropertyId, gleaph_gql::Value)>,
}
