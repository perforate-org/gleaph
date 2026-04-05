use candid::Principal;
use gleaph_graph_kernel::{EdgeRecord, GraphResult, NodeId, NodeRecord, PropertyMap};

use super::{
    GraphPmaKernelBootstrapBridge, GraphPmaKernelOverlayGraph,
    GraphPmaKernelOverlayObservability, GraphPmaOverlayEdgeWriteSummary,
    GraphPmaOverlayInsertEdgeSummary, GraphPmaOverlayNodeDeleteSummary, GraphPmaOverlayWriteEvent,
    KernelBootstrapEdgeSpec, KernelBootstrapGraphSpec, KernelBootstrapGraphSummary,
    KernelBootstrapNodeSpec, bootstrap_kernel_overlay_graph,
};
use crate::facade::{
    GraphPmaPropertyMutationWriteSummary, GraphPmaStore, GraphPmaStoreAdapter,
    GraphPmaWriteEventProjection,
};
use crate::observability::{format_last_write_event, format_write_event_history};

impl<'a, S: GraphPmaStore> GraphPmaKernelOverlayGraph<'a, S> {
    /// Creates one overlay graph from a bootstrap bridge.
    pub fn new(bridge: GraphPmaKernelBootstrapBridge<'a, S>) -> Self {
        Self { bridge }
    }

    /// Returns the underlying bootstrap bridge.
    pub fn bridge(&self) -> &GraphPmaKernelBootstrapBridge<'a, S> {
        &self.bridge
    }

    /// Returns mutable access to the underlying bootstrap bridge.
    pub fn bridge_mut(&mut self) -> &mut GraphPmaKernelBootstrapBridge<'a, S> {
        &mut self.bridge
    }

    /// Returns the most recent property-write summary observed through this overlay.
    pub fn last_property_write_summary(&self) -> Option<&GraphPmaPropertyMutationWriteSummary> {
        self.bridge.last_property_write_summary()
    }

    /// Returns recent property-write summaries in observation order.
    pub fn property_write_history(&self) -> &[GraphPmaPropertyMutationWriteSummary] {
        self.bridge.property_write_history()
    }

    /// Returns the most recent insert-edge summary observed through this overlay.
    pub fn last_insert_edge_summary(&self) -> Option<&GraphPmaOverlayInsertEdgeSummary> {
        self.bridge.last_insert_edge_summary()
    }

    /// Returns recent insert-edge summaries in observation order.
    pub fn insert_edge_history(&self) -> &[GraphPmaOverlayInsertEdgeSummary] {
        self.bridge.insert_edge_history()
    }

    /// Returns the most recent edge-write summary observed through this overlay.
    pub fn last_edge_write_summary(&self) -> Option<&GraphPmaOverlayEdgeWriteSummary> {
        self.bridge.last_edge_write_summary()
    }

    /// Returns recent edge-write summaries in observation order.
    pub fn edge_write_history(&self) -> &[GraphPmaOverlayEdgeWriteSummary] {
        self.bridge.edge_write_history()
    }

    /// Returns the most recent node-delete summary observed through this overlay.
    pub fn last_node_delete_summary(&self) -> Option<&GraphPmaOverlayNodeDeleteSummary> {
        self.bridge.last_node_delete_summary()
    }

    /// Returns recent node-delete summaries in observation order.
    pub fn node_delete_history(&self) -> &[GraphPmaOverlayNodeDeleteSummary] {
        self.bridge.node_delete_history()
    }

    /// Returns recent overlay write events in observation order.
    pub fn write_history(&self) -> &[GraphPmaOverlayWriteEvent] {
        self.bridge.write_history()
    }

    /// Returns the recent overlay write history projected onto the shared event vocabulary.
    pub fn shared_write_history(&self) -> Vec<GraphPmaWriteEventProjection> {
        self.write_history()
            .iter()
            .flat_map(GraphPmaOverlayWriteEvent::shared_projections)
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
    pub fn into_bridge(self) -> GraphPmaKernelBootstrapBridge<'a, S> {
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
        undirected: bool,
    ) -> GraphResult<EdgeRecord> {
        self.bridge
            .bootstrap_edge(src, dst, label, properties, undirected)
    }

    /// See [`GraphPmaKernelBootstrapBridge::bootstrap_edge_with_shard_canister_dst`].
    pub fn bootstrap_edge_with_shard_canister_dst(
        &mut self,
        src: NodeId,
        dst: NodeId,
        shard_canister: Principal,
        label: Option<&str>,
        properties: &PropertyMap,
        undirected: bool,
    ) -> GraphResult<EdgeRecord> {
        self.bridge.bootstrap_edge_with_shard_canister_dst(
            src,
            dst,
            shard_canister,
            label,
            properties,
            undirected,
        )
    }

    /// See [`GraphPmaKernelBootstrapBridge::insert_edge_with_shard_canister_dst`].
    pub fn insert_edge_with_shard_canister_dst(
        &mut self,
        src: NodeId,
        dst: NodeId,
        shard_canister: Principal,
        label: Option<&str>,
        properties: &PropertyMap,
        undirected: bool,
    ) -> GraphResult<EdgeRecord> {
        self.bridge.insert_edge_with_shard_canister_dst(
            src,
            dst,
            shard_canister,
            label,
            properties,
            undirected,
        )
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

impl<'a, S: GraphPmaStore> GraphPmaKernelOverlayObservability
    for GraphPmaKernelOverlayGraph<'a, S>
{
    fn last_property_write_summary(&self) -> Option<&GraphPmaPropertyMutationWriteSummary> {
        Self::last_property_write_summary(self)
    }

    fn property_write_history(&self) -> &[GraphPmaPropertyMutationWriteSummary] {
        Self::property_write_history(self)
    }

    fn last_insert_edge_summary(&self) -> Option<&GraphPmaOverlayInsertEdgeSummary> {
        Self::last_insert_edge_summary(self)
    }

    fn insert_edge_history(&self) -> &[GraphPmaOverlayInsertEdgeSummary] {
        Self::insert_edge_history(self)
    }

    fn last_edge_write_summary(&self) -> Option<&GraphPmaOverlayEdgeWriteSummary> {
        Self::last_edge_write_summary(self)
    }

    fn edge_write_history(&self) -> &[GraphPmaOverlayEdgeWriteSummary] {
        Self::edge_write_history(self)
    }

    fn last_node_delete_summary(&self) -> Option<&GraphPmaOverlayNodeDeleteSummary> {
        Self::last_node_delete_summary(self)
    }

    fn node_delete_history(&self) -> &[GraphPmaOverlayNodeDeleteSummary] {
        Self::node_delete_history(self)
    }

    fn write_history(&self) -> &[GraphPmaOverlayWriteEvent] {
        Self::write_history(self)
    }
}

impl<'a, S: GraphPmaStore> GraphPmaStoreAdapter<'a, S> {
    /// Converts one bound store adapter into a kernel-facing overlay graph.
    pub fn into_kernel_overlay(self) -> GraphPmaKernelOverlayGraph<'a, &'a mut S> {
        let (store, memory) = self.into_parts();
        GraphPmaKernelOverlayGraph::new(GraphPmaKernelBootstrapBridge::new(store, memory))
    }
}
