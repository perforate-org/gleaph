use gleaph_graph_kernel::{EdgeRecord, GraphResult, NodeId, NodeRecord, PropertyMap};

use super::{
    KernelBootstrapEdgeSpec, KernelBootstrapGraphSpec, KernelBootstrapGraphSummary,
    KernelBootstrapNodeSpec, RewriteKernelBootstrapBridge, RewriteKernelOverlayGraph,
    RewriteKernelOverlayObservability, RewriteOverlayEdgeWriteSummary,
    RewriteOverlayInsertEdgeSummary, RewriteOverlayNodeDeleteSummary, RewriteOverlayWriteEvent,
    bootstrap_kernel_overlay_graph,
};
use crate::facade::{
    RewriteGraphStore, RewriteGraphStoreAdapter, RewritePropertyMutationWriteSummary,
    RewriteWriteEventProjection,
};
use crate::observability::{format_last_write_event, format_write_event_history};

impl<'a, S: RewriteGraphStore> RewriteKernelOverlayGraph<'a, S> {
    /// Creates one overlay graph from a rewrite bootstrap bridge.
    pub fn new(bridge: RewriteKernelBootstrapBridge<'a, S>) -> Self {
        Self { bridge }
    }

    /// Returns the underlying bootstrap bridge.
    pub fn bridge(&self) -> &RewriteKernelBootstrapBridge<'a, S> {
        &self.bridge
    }

    /// Returns mutable access to the underlying bootstrap bridge.
    pub fn bridge_mut(&mut self) -> &mut RewriteKernelBootstrapBridge<'a, S> {
        &mut self.bridge
    }

    /// Returns the most recent property-write summary observed through this overlay.
    pub fn last_property_write_summary(&self) -> Option<&RewritePropertyMutationWriteSummary> {
        self.bridge.last_property_write_summary()
    }

    /// Returns recent property-write summaries in observation order.
    pub fn property_write_history(&self) -> &[RewritePropertyMutationWriteSummary] {
        self.bridge.property_write_history()
    }

    /// Returns the most recent insert-edge summary observed through this overlay.
    pub fn last_insert_edge_summary(&self) -> Option<&RewriteOverlayInsertEdgeSummary> {
        self.bridge.last_insert_edge_summary()
    }

    /// Returns recent insert-edge summaries in observation order.
    pub fn insert_edge_history(&self) -> &[RewriteOverlayInsertEdgeSummary] {
        self.bridge.insert_edge_history()
    }

    /// Returns the most recent edge-write summary observed through this overlay.
    pub fn last_edge_write_summary(&self) -> Option<&RewriteOverlayEdgeWriteSummary> {
        self.bridge.last_edge_write_summary()
    }

    /// Returns recent edge-write summaries in observation order.
    pub fn edge_write_history(&self) -> &[RewriteOverlayEdgeWriteSummary] {
        self.bridge.edge_write_history()
    }

    /// Returns the most recent node-delete summary observed through this overlay.
    pub fn last_node_delete_summary(&self) -> Option<&RewriteOverlayNodeDeleteSummary> {
        self.bridge.last_node_delete_summary()
    }

    /// Returns recent node-delete summaries in observation order.
    pub fn node_delete_history(&self) -> &[RewriteOverlayNodeDeleteSummary] {
        self.bridge.node_delete_history()
    }

    /// Returns recent overlay write events in observation order.
    pub fn write_history(&self) -> &[RewriteOverlayWriteEvent] {
        self.bridge.write_history()
    }

    /// Returns the recent overlay write history projected onto the shared event vocabulary.
    pub fn shared_write_history(&self) -> Vec<RewriteWriteEventProjection> {
        self.write_history()
            .iter()
            .flat_map(RewriteOverlayWriteEvent::shared_projections)
            .collect()
    }

    /// Returns the recent overlay write history formatted as compact diagnostics lines.
    pub fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(&self.shared_write_history())
    }

    pub fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(&self.shared_write_history())
    }

    /// Consumes the overlay and returns the underlying bootstrap bridge.
    pub fn into_bridge(self) -> RewriteKernelBootstrapBridge<'a, S> {
        self.bridge
    }

    /// Applies one declarative kernel-facing bootstrap specification to this overlay.
    pub fn bootstrap_graph(
        &mut self,
        spec: &KernelBootstrapGraphSpec,
    ) -> GraphResult<KernelBootstrapGraphSummary> {
        bootstrap_kernel_overlay_graph(self, spec)
    }

    /// Bootstraps one node record onto the bound overlay from borrowed labels/properties.
    pub fn bootstrap_node(
        &mut self,
        labels: &[String],
        properties: &PropertyMap,
    ) -> GraphResult<NodeRecord> {
        self.bridge.bootstrap_node(labels, properties)
    }

    /// Bootstraps one edge record onto the bound overlay from borrowed label/properties.
    pub fn bootstrap_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        self.bridge.bootstrap_edge(src, dst, label, properties)
    }

    /// Bootstraps kernel-facing node/edge record payloads directly on this overlay.
    pub fn bootstrap_nodes_and_edges(
        &mut self,
        nodes: &[KernelBootstrapNodeSpec],
        edges: &[KernelBootstrapEdgeSpec],
    ) -> GraphResult<KernelBootstrapGraphSummary> {
        self.bootstrap_graph(&KernelBootstrapGraphSpec::from_slices(nodes, edges))
    }
}

impl<'a, S: RewriteGraphStore> RewriteKernelOverlayObservability
    for RewriteKernelOverlayGraph<'a, S>
{
    fn last_property_write_summary(&self) -> Option<&RewritePropertyMutationWriteSummary> {
        Self::last_property_write_summary(self)
    }

    fn property_write_history(&self) -> &[RewritePropertyMutationWriteSummary] {
        Self::property_write_history(self)
    }

    fn last_insert_edge_summary(&self) -> Option<&RewriteOverlayInsertEdgeSummary> {
        Self::last_insert_edge_summary(self)
    }

    fn insert_edge_history(&self) -> &[RewriteOverlayInsertEdgeSummary] {
        Self::insert_edge_history(self)
    }

    fn last_edge_write_summary(&self) -> Option<&RewriteOverlayEdgeWriteSummary> {
        Self::last_edge_write_summary(self)
    }

    fn edge_write_history(&self) -> &[RewriteOverlayEdgeWriteSummary] {
        Self::edge_write_history(self)
    }

    fn last_node_delete_summary(&self) -> Option<&RewriteOverlayNodeDeleteSummary> {
        Self::last_node_delete_summary(self)
    }

    fn node_delete_history(&self) -> &[RewriteOverlayNodeDeleteSummary] {
        Self::node_delete_history(self)
    }

    fn write_history(&self) -> &[RewriteOverlayWriteEvent] {
        Self::write_history(self)
    }
}

impl<'a, S: RewriteGraphStore> RewriteGraphStoreAdapter<'a, S> {
    /// Converts one bound rewrite adapter into a kernel-facing overlay graph.
    pub fn into_kernel_overlay(self) -> RewriteKernelOverlayGraph<'a, &'a mut S> {
        let (store, memory) = self.into_parts();
        RewriteKernelOverlayGraph::new(RewriteKernelBootstrapBridge::new(store, memory))
    }
}
