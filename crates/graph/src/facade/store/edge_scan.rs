//! GraphStore `edge_scan` implementation.

use super::super::stable::GRAPH;
use gleaph_graph_kernel::entry::{Edge, EdgeDirectedness, EdgeLabelId};
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledError, VertexId,
    labeled::{
        BucketDirectedness, LabeledEdgeValueBatch, LabeledEdgeValueBatchScratch, OutEdgeOrder,
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

    pub(crate) fn visit_out_edge_value_batches_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeValueBatchScratch<Edge>,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: for<'b> FnMut(LabeledEdgeValueBatch<'b, Edge>),
    {
        GRAPH.with_borrow(|graph| {
            graph.visit_out_edge_value_batches_for_label(vertex_id, label, order, scratch, visit)
        })
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
