//! Small integration helpers for upper layers that use the rewrite service boundary.
//!
//! The low-level rewrite APIs are intentionally explicit, but upper layers
//! often want a slightly more declarative entrypoint for initial graph
//! bootstrap. This module keeps that convenience separate from the core
//! facade/runtime types.

mod bootstrap_types;
mod bridge_bootstrap;
mod bridge_storage;
mod graph_read_impl;
mod graph_write_impl;
mod label_index;
mod overlay_api;
mod overlay_types;

use std::collections::BTreeMap;

use gleaph_gql::ast::CmpOp;
use gleaph_gql_planner::stats::GraphStats;
use gleaph_graph_kernel::{
    EdgeId, EdgeRecord, GraphError, GraphResult, LabelId, NodeId, NodeRecord,
};

use crate::facade::{
    RewriteBootstrapGraphWriteSummary, RewriteEdgeLogicalLocatorMapping, RewriteGraphPma,
    RewriteGraphPmaResult, RewriteGraphService, RewriteGraphStore,
    RewritePropertyMutationWriteSummary, RewriteVertexOrdinalMapping,
};
use crate::property_store::PropertyStoreError;
use crate::stable::Memory;
pub use bootstrap_types::{
    BootstrapEdgeSpec, BootstrapGraphSpec, KernelBootstrapEdgeSpec, KernelBootstrapGraphSpec,
    KernelBootstrapGraphSummary, KernelBootstrapNodeSpec,
};
pub use label_index::{
    LabelMembership, VERTEX_LABEL_PROMOTION_THRESHOLD_BASE, VacuumStats, VertexGcState,
    VertexLabelIndex, decode_vertex_label_catalog, encode_vertex_label_catalog,
};
pub use overlay_types::{
    RewriteKernelOverlayObservability, RewriteOverlayBootstrapGraphSummary,
    RewriteOverlayEdgeBootstrapSummary, RewriteOverlayEdgeMutationKind,
    RewriteOverlayEdgeWriteSummary, RewriteOverlayInsertEdgeSummary,
    RewriteOverlayNodeBootstrapSummary, RewriteOverlayNodeDeleteSummary, RewriteOverlayWriteEvent,
};

/// Applies one declarative bootstrap specification through the rewrite service boundary.
///
/// Integration owns the semantic `NodeId -> VertexRef` conversion so the
/// facade/service layer can stay `VertexRef`-native.
pub fn bootstrap_graph(
    service: &mut impl RewriteGraphService,
    spec: &BootstrapGraphSpec,
) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
    let initial_edges = spec.edge_tuples();
    let vertex_refs: Vec<_> = spec.vertex_ids.iter().copied().map(Into::into).collect();
    service.bootstrap_vertex_refs_and_edges(&vertex_refs, &initial_edges)
}

/// Applies one kernel-facing bootstrap specification through the overlay graph.
pub fn bootstrap_kernel_overlay_graph<S: RewriteGraphStore, M: Memory>(
    graph: &mut RewriteKernelOverlayGraph<'_, S, M>,
    spec: &KernelBootstrapGraphSpec,
) -> GraphResult<KernelBootstrapGraphSummary> {
    let vertex_ordinal_start = graph.bridge().vertex_ordinals.len();
    let mut node_summaries = Vec::with_capacity(spec.nodes.len());
    let mut nodes = Vec::with_capacity(spec.nodes.len());
    for node in &spec.nodes {
        nodes.push(graph.bootstrap_node(&node.labels, &node.properties)?);
        if let Some(RewriteOverlayWriteEvent::BootstrapNode(summary)) = graph.write_history().last()
        {
            node_summaries.push(summary.clone());
        }
    }

    let mut edge_summaries = Vec::with_capacity(spec.edges.len());
    let mut edges = Vec::with_capacity(spec.edges.len());
    for edge in &spec.edges {
        let src = nodes
            .get(edge.src_index)
            .ok_or_else(|| GraphError::Message("kernel bootstrap src_index out of range".into()))?
            .id;
        let dst = nodes
            .get(edge.dst_index)
            .ok_or_else(|| GraphError::Message("kernel bootstrap dst_index out of range".into()))?
            .id;
        edges.push(graph.bootstrap_edge(src, dst, edge.label.as_deref(), &edge.properties)?);
        if let Some(RewriteOverlayWriteEvent::BootstrapEdge(summary)) = graph.write_history().last()
        {
            edge_summaries.push(summary.clone());
        }
    }

    let vertex_ordinals = graph.bridge().vertex_ordinals[vertex_ordinal_start..].to_vec();
    let locators = edges
        .iter()
        .map(|edge| {
            graph
                .bridge()
                .edge_locators
                .get(&edge.id)
                .copied()
                .ok_or_else(|| {
                    GraphError::Message(format!(
                        "missing locator mapping for bootstrapped edge {}",
                        edge.id
                    ))
                })
        })
        .collect::<GraphResult<Vec<_>>>()?;
    let overlay_summary = RewriteOverlayBootstrapGraphSummary::from_bootstrap_summaries(
        &node_summaries,
        &edge_summaries,
        locators,
    );

    let summary = KernelBootstrapGraphSummary {
        nodes: overlay_summary.nodes.clone(),
        edges: overlay_summary.edges.clone(),
        vertex_ordinals: vertex_ordinals.clone(),
        locators: overlay_summary.locators.clone(),
        refreshed: overlay_summary.refreshed.clone(),
    };
    graph
        .bridge_mut()
        .record_bootstrap_graph_summary(overlay_summary);
    Ok(summary)
}

/// Bootstrap-oriented bridge from rewrite graph service calls to kernel records.
///
/// This bridge is intentionally narrow. It focuses on initial node/edge
/// creation and keeps enough local metadata to let upper layers reason in
/// terms of `NodeRecord` / `EdgeRecord` while the rewrite storage kernel is
/// still growing into the full `GraphRead` / `GraphWrite` surface.
pub struct RewriteKernelBootstrapBridge<'a, S: RewriteGraphStore, M: Memory> {
    store: S,
    memory: &'a M,
    next_node_id: u64,
    next_edge_id: u64,
    next_label_id: u16,
    label_ids: BTreeMap<String, LabelId>,
    nodes: BTreeMap<NodeId, NodeRecord>,
    edges: BTreeMap<EdgeId, EdgeRecord>,
    /// Incident edge ids per endpoint (undirected incidence) for O(deg) `expand`.
    incident_edge_ids: BTreeMap<NodeId, Vec<EdgeId>>,
    vertex_ordinals: Vec<RewriteVertexOrdinalMapping>,
    semantic_node_id_by_forward_ordinal: Vec<Option<NodeId>>,
    vertex_ordinal_by_node_id: BTreeMap<NodeId, RewriteVertexOrdinalMapping>,
    vertex_label_index: VertexLabelIndex,
    vertex_gc_state: VertexGcState,
    edge_locators: BTreeMap<EdgeId, RewriteEdgeLogicalLocatorMapping>,
    forward_base_slots_by_ordinal: Vec<Vec<Option<EdgeId>>>,
    reverse_base_slots_by_ordinal: Vec<Vec<Option<EdgeId>>>,
    last_property_write_summary: Option<RewritePropertyMutationWriteSummary>,
    last_insert_edge_summary: Option<RewriteOverlayInsertEdgeSummary>,
    last_edge_write_summary: Option<RewriteOverlayEdgeWriteSummary>,
    last_node_delete_summary: Option<RewriteOverlayNodeDeleteSummary>,
    property_write_history: Vec<RewritePropertyMutationWriteSummary>,
    insert_edge_history: Vec<RewriteOverlayInsertEdgeSummary>,
    edge_write_history: Vec<RewriteOverlayEdgeWriteSummary>,
    node_delete_history: Vec<RewriteOverlayNodeDeleteSummary>,
    write_history: Vec<RewriteOverlayWriteEvent>,
}

fn graph_error_from_property_store(err: PropertyStoreError) -> GraphError {
    match err {
        PropertyStoreError::PropertyIndex(inner) => GraphError::property_index(inner),
        other => GraphError::property_store(other),
    }
}

const OVERLAY_SUMMARY_HISTORY_LIMIT: usize = 8;

/// Thin kernel-facing overlay adapter over the rewrite bootstrap bridge.
///
/// This adapter is intentionally conservative. Structural bootstrap goes
/// through the rewrite graph facade, while the kernel-facing record layer is
/// currently maintained in-memory and treated as authoritative for
/// `GraphRead` / `GraphWrite`. That keeps the boundary usable while the
/// rewrite storage kernel grows towards a full record-authoritative graph
/// implementation.
pub struct RewriteKernelOverlayGraph<'a, S: RewriteGraphStore, M: Memory> {
    bridge: RewriteKernelBootstrapBridge<'a, S, M>,
}

/// Concrete kernel overlay shape used when binding the main rewrite facade directly.
pub type RewriteGraphPmaKernelOverlay<'a, M> =
    RewriteKernelOverlayGraph<'a, &'a mut RewriteGraphPma, M>;

/// Small owned harness that keeps the rewrite facade and its stable memory together.
///
/// This is mainly useful for integration tests and higher-level experiments
/// that want one reusable owner for the rewrite graph plus its bound stable
/// memory, without repeating the same bootstrap-and-bind setup in every call
/// site.
pub struct RewriteGraphPmaKernelHarness<M: Memory> {
    facade: RewriteGraphPma,
    memory: M,
}

impl<M: Memory> RewriteGraphPmaKernelHarness<M> {
    /// Bootstraps one empty rewrite graph together with owned stable memory.
    pub fn bootstrap_empty(memory: M) -> RewriteGraphPmaResult<Self> {
        let facade = RewriteGraphPma::bootstrap_empty(&memory)?;
        Ok(Self { facade, memory })
    }

    /// Returns the owned stable-memory handle.
    pub fn memory(&self) -> &M {
        &self.memory
    }

    /// Returns the rewrite facade stored inside this harness.
    pub fn facade(&self) -> &RewriteGraphPma {
        &self.facade
    }

    /// Returns mutable access to the rewrite facade stored inside this harness.
    pub fn facade_mut(&mut self) -> &mut RewriteGraphPma {
        &mut self.facade
    }

    /// Binds the owned facade and memory as one kernel-facing overlay graph.
    pub fn bind_overlay(&mut self) -> RewriteGraphPmaKernelOverlay<'_, M> {
        self.facade.bind_kernel_overlay(&self.memory)
    }

    /// Binds one overlay and seeds it on that same bound instance.
    pub fn bind_overlay_with_graph(
        &mut self,
        spec: &KernelBootstrapGraphSpec,
    ) -> GraphResult<(
        RewriteGraphPmaKernelOverlay<'_, M>,
        KernelBootstrapGraphSummary,
    )> {
        let mut overlay = self.bind_overlay();
        let summary = overlay.bootstrap_graph(spec)?;
        Ok((overlay, summary))
    }

    /// Applies one declarative bootstrap specification through the owned rewrite facade.
    pub fn bootstrap_graph(
        &mut self,
        spec: &BootstrapGraphSpec,
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
        let mut adapter = self.facade.bind(&self.memory);
        bootstrap_graph(&mut adapter, spec)
    }

    /// Bootstraps packed vertex refs and logical edges directly through the owned rewrite facade.
    pub fn bootstrap_vertex_refs_and_edges(
        &mut self,
        vertex_refs: &[crate::low_level::VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
        let mut adapter = self.facade.bind(&self.memory);
        adapter.bootstrap_vertex_refs_and_edges(vertex_refs, initial_edges)
    }

    /// Bootstraps logical vertices and edges directly, without requiring an explicit spec value.
    pub fn bootstrap_vertices_and_edges(
        &mut self,
        vertex_ids: &[NodeId],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
        let vertex_refs: Vec<_> = vertex_ids.iter().copied().map(Into::into).collect();
        self.bootstrap_vertex_refs_and_edges(&vertex_refs, initial_edges)
    }

    /// Applies one kernel-facing bootstrap specification through an owned overlay binding.
    pub fn bootstrap_kernel_overlay_graph(
        &mut self,
        spec: &KernelBootstrapGraphSpec,
    ) -> GraphResult<KernelBootstrapGraphSummary> {
        let mut overlay = self.bind_overlay();
        bootstrap_kernel_overlay_graph(&mut overlay, spec)
    }

    /// Bootstraps kernel-facing node/edge record payloads directly, without requiring an explicit spec value.
    pub fn bootstrap_kernel_nodes_and_edges(
        &mut self,
        nodes: &[KernelBootstrapNodeSpec],
        edges: &[KernelBootstrapEdgeSpec],
    ) -> GraphResult<KernelBootstrapGraphSummary> {
        let spec = KernelBootstrapGraphSpec::from_slices(nodes, edges);
        self.bootstrap_kernel_overlay_graph(&spec)
    }
}

impl<'a, S: RewriteGraphStore, M: Memory> GraphStats for RewriteKernelOverlayGraph<'a, S, M> {
    fn label_cardinality(&self, label: &str) -> Option<u64> {
        let label_id = self.bridge.lookup_label_id(label)?;
        Some(self.bridge.vertex_label_index.cardinality(label_id) as u64)
    }

    fn label_cardinality_id(&self, label_id: u16) -> Option<u64> {
        Some(self.bridge.vertex_label_index.cardinality(label_id) as u64)
    }
}

impl<'a, S: RewriteGraphStore, M: Memory> RewriteKernelOverlayGraph<'a, S, M> {
    fn edge_base_logical_index(slots: &[Option<EdgeId>], edge_id: EdgeId) -> Option<usize> {
        RewriteKernelBootstrapBridge::<S, M>::find_base_logical_index(slots, edge_id)
    }

    /// Runs a bounded GC step for tombstoned vertices.
    pub fn vacuum_step(&mut self, max_ops: usize) -> usize {
        self.bridge.vacuum_step_internal(max_ops)
    }

    /// Returns current GC queue/tombstone/free-list counters.
    pub fn vacuum_stats(&self) -> VacuumStats {
        self.bridge.vacuum_stats()
    }
}

fn compare_op(ordering: Option<std::cmp::Ordering>, cmp: CmpOp) -> bool {
    match cmp {
        CmpOp::Eq => ordering == Some(std::cmp::Ordering::Equal),
        CmpOp::Ne => ordering != Some(std::cmp::Ordering::Equal),
        CmpOp::Lt => ordering == Some(std::cmp::Ordering::Less),
        CmpOp::Le => matches!(
            ordering,
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ),
        CmpOp::Gt => ordering == Some(std::cmp::Ordering::Greater),
        CmpOp::Ge => matches!(
            ordering,
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        BootstrapEdgeSpec, BootstrapGraphSpec, KernelBootstrapEdgeSpec, KernelBootstrapGraphSpec,
        KernelBootstrapNodeSpec, LabelMembership, RewriteGraphPmaKernelHarness,
        RewriteKernelBootstrapBridge, VERTEX_LABEL_PROMOTION_THRESHOLD_BASE, VertexGcState,
        VertexLabelIndex, bootstrap_graph, bootstrap_kernel_overlay_graph,
        decode_vertex_label_catalog, encode_vertex_label_catalog, graph_error_from_property_store,
    };
    use crate::RewriteGraphPma;
    use crate::facade::{
        RewriteBootstrapVerticesProjection, RewriteEdgeWriteOperation, RewriteRefreshedVertices,
        RewriteVertexOrdinalMapping, RewriteWriteEventProjection,
    };
    use crate::integration::{
        RewriteKernelOverlayObservability, RewriteOverlayEdgeMutationKind, RewriteOverlayWriteEvent,
    };
    use crate::low_level::{
        EdgeEntry, EdgeIndex, EdgeInsertPath, EdgeLogicalLocatorSidecar, EdgeMeta,
        GraphInsertPolicy, GraphMutationPath, LogOffset, LogicalEdgeLocator, OverflowEntry,
        RegionKind, SurfaceBaseStorage, SurfaceKind, VertexEntry, VertexLabelIndexEntry,
    };
    use crate::observability::{
        RewriteDiagnosticsView, project_overlay_write_event, project_overlay_write_history,
    };
    use crate::property_index::{PropertyIndexError, PropertyIndexNodeStoreMutationKind};
    use crate::property_store::PropertyStoreError;
    use crate::stable::VecMemory;
    use gleaph_gql::Value;
    use gleaph_gql::ast::{CmpOp, Statement};
    use gleaph_gql::parser;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql_planner::build_plan;
    use gleaph_graph_kernel::{
        EdgeLabelFilter, GraphError, GraphRead, GraphWrite, NodeId, NodeRecord, PropertyMap,
    };

    fn assert_projected_history(
        events: &[RewriteOverlayWriteEvent],
        expected: Vec<RewriteWriteEventProjection>,
    ) {
        assert_eq!(project_overlay_write_history(events), expected);
    }

    #[test]
    fn graph_error_from_property_store_maps_index_separately_from_append_log() {
        use std::error::Error as StdError;

        let inner = PropertyIndexError::LeafPartitionSingletonNotEncodable;
        let graph_err =
            graph_error_from_property_store(PropertyStoreError::PropertyIndex(inner.clone()));
        assert!(matches!(graph_err, GraphError::PropertyIndex { .. }));
        let source = graph_err
            .source()
            .expect("property index error chains source");
        assert_eq!(source.downcast_ref::<PropertyIndexError>(), Some(&inner));

        let store_err = PropertyStoreError::LengthOverflow;
        let graph_err = graph_error_from_property_store(store_err.clone());
        assert!(matches!(graph_err, GraphError::PropertyStore { .. }));
        let source = graph_err
            .source()
            .expect("property store error chains source");
        assert_eq!(
            source.downcast_ref::<PropertyStoreError>(),
            Some(&store_err)
        );
    }

    #[test]
    fn bootstrap_graph_uses_service_boundary() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut adapter = facade.bind(&memory);

        let spec = BootstrapGraphSpec::empty()
            .with_vertex(NodeId::from(51u8))
            .with_vertex(NodeId::from(52u8))
            .with_vertex(NodeId::from(53u8))
            .with_edge(BootstrapEdgeSpec::new(1001, 0, 1, 17))
            .with_edge(BootstrapEdgeSpec::new(1002, 1, 2, 19));

        let summary = bootstrap_graph(&mut adapter, &spec).expect("bootstrap through integration");
        assert_eq!(summary.vertex_ordinals.len(), 3);
        assert_eq!(summary.inserts.len(), 2);
        assert_eq!(summary.locators.len(), 2);
    }

    #[test]
    fn kernel_harness_can_bootstrap_graph_spec() {
        let mut harness = RewriteGraphPmaKernelHarness::bootstrap_empty(VecMemory::default())
            .expect("bootstrap harness");

        let spec = BootstrapGraphSpec::empty()
            .with_vertex(NodeId::from(61u8))
            .with_vertex(NodeId::from(62u8))
            .with_edge(BootstrapEdgeSpec::new(2001, 0, 1, 23));

        let summary = harness
            .bootstrap_graph(&spec)
            .expect("bootstrap via harness");
        assert_eq!(summary.vertex_ordinals.len(), 2);
        assert_eq!(summary.inserts.len(), 1);
        assert_eq!(summary.locators.len(), 1);
    }

    #[test]
    fn kernel_harness_can_bootstrap_vertices_and_edges_without_explicit_spec() {
        let mut harness = RewriteGraphPmaKernelHarness::bootstrap_empty(VecMemory::default())
            .expect("bootstrap harness");

        let summary = harness
            .bootstrap_vertices_and_edges(
                &[NodeId::from(71u8), NodeId::from(72u8)],
                &[(3001, 0, 1, 29)],
            )
            .expect("bootstrap via convenience entry");
        assert_eq!(summary.vertex_ordinals.len(), 2);
        assert_eq!(summary.inserts.len(), 1);
    }

    #[test]
    fn kernel_harness_can_bootstrap_kernel_overlay_graph_spec() {
        let mut harness = RewriteGraphPmaKernelHarness::bootstrap_empty(VecMemory::default())
            .expect("bootstrap harness");
        let user_properties: PropertyMap = [("name".to_owned(), Value::Text("Alice".to_owned()))]
            .into_iter()
            .collect();
        let post_properties: PropertyMap = [("title".to_owned(), Value::Text("Hello".to_owned()))]
            .into_iter()
            .collect();
        let edge_properties: PropertyMap = [("since".to_owned(), Value::Int64(2024))]
            .into_iter()
            .collect();

        let spec = KernelBootstrapGraphSpec::empty()
            .with_node(KernelBootstrapNodeSpec::from_parts(
                &["User"],
                &user_properties,
            ))
            .with_node(KernelBootstrapNodeSpec::from_parts(
                &["Post"],
                &post_properties,
            ))
            .with_edge(KernelBootstrapEdgeSpec::from_parts(
                0,
                1,
                Some("AUTHORED"),
                &edge_properties,
            ));

        let summary = harness
            .bootstrap_kernel_overlay_graph(&spec)
            .expect("kernel bootstrap via harness");
        assert_eq!(summary.nodes.len(), 2);
        assert_eq!(summary.edges.len(), 1);
        assert_eq!(summary.vertex_ordinals.len(), 2);
        assert_eq!(summary.locators.len(), 1);
        assert!(!summary.refreshed.forward.is_empty());
        assert!(!summary.refreshed.reverse.is_empty());
        assert_eq!(summary.edges[0].label.as_deref(), Some("AUTHORED"));
    }

    #[test]
    fn kernel_harness_can_bind_and_bootstrap_same_overlay_instance() {
        let mut harness = RewriteGraphPmaKernelHarness::bootstrap_empty(VecMemory::default())
            .expect("bootstrap harness");
        let empty_properties = PropertyMap::new();

        let spec = KernelBootstrapGraphSpec::empty()
            .with_node(KernelBootstrapNodeSpec::from_parts(
                &["User"],
                &empty_properties,
            ))
            .with_node(KernelBootstrapNodeSpec::from_parts(
                &["Post"],
                &empty_properties,
            ))
            .with_edge(KernelBootstrapEdgeSpec::from_parts(
                0,
                1,
                Some("AUTHORED"),
                &empty_properties,
            ));

        let (graph, summary) = harness
            .bind_overlay_with_graph(&spec)
            .expect("bind and bootstrap overlay");
        assert_eq!(summary.nodes.len(), 2);
        assert_eq!(summary.edges.len(), 1);
        assert_eq!(summary.vertex_ordinals.len(), 2);
        assert_eq!(summary.locators.len(), 1);
        assert_eq!(
            graph
                .expand(
                    summary.nodes[0].id,
                    EdgeDirection::PointingRight,
                    EdgeLabelFilter::Single("AUTHORED"),
                )
                .expect("expand")
                .len(),
            1
        );
        assert!(matches!(
            graph.write_history(),
            [
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapEdge(_),
                RewriteOverlayWriteEvent::BootstrapGraph(summary)
            ] if summary.nodes.len() == 2
                && summary.edges.len() == 1
                && summary.vertex_ordinals.len() == 2
                && summary.locators.len() == 1
                && !summary.refreshed.forward.is_empty()
                && !summary.refreshed.reverse.is_empty()
        ));
        let expected_bootstrap = crate::observability::format_write_event_projection(
            &RewriteWriteEventProjection::BootstrapGraph(summary.projection()),
        );
        assert_eq!(
            graph.formatted_last_write_event(),
            Some(expected_bootstrap.clone())
        );
        assert_eq!(
            RewriteDiagnosticsView::debug_report(&graph)
                .lines()
                .last()
                .map(str::to_owned),
            Some(expected_bootstrap)
        );
    }

    #[test]
    fn kernel_harness_can_bootstrap_kernel_nodes_and_edges_without_explicit_spec() {
        let mut harness = RewriteGraphPmaKernelHarness::bootstrap_empty(VecMemory::default())
            .expect("bootstrap harness");
        let user_properties: PropertyMap = [("name".to_owned(), Value::Text("Alice".to_owned()))]
            .into_iter()
            .collect();
        let empty_properties = PropertyMap::new();

        let summary = harness
            .bootstrap_kernel_nodes_and_edges(
                &[
                    KernelBootstrapNodeSpec::from_parts(&["User"], &user_properties),
                    KernelBootstrapNodeSpec::from_parts(&["Post"], &empty_properties),
                ],
                &[KernelBootstrapEdgeSpec::from_parts(
                    0,
                    1,
                    Some("AUTHORED"),
                    &empty_properties,
                )],
            )
            .expect("kernel bootstrap via convenience entry");
        assert_eq!(summary.nodes.len(), 2);
        assert_eq!(summary.edges.len(), 1);
        assert_eq!(summary.vertex_ordinals.len(), 2);
        assert_eq!(summary.locators.len(), 1);
        assert!(!summary.refreshed.forward.is_empty());
        assert!(!summary.refreshed.reverse.is_empty());
    }

    #[test]
    fn kernel_overlay_bootstrap_function_can_seed_graph_records() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut overlay = facade.bind_kernel_overlay(&memory);
        let empty_properties = PropertyMap::new();
        let spec = KernelBootstrapGraphSpec::empty()
            .with_node(KernelBootstrapNodeSpec::from_parts(
                &["User"],
                &empty_properties,
            ))
            .with_node(KernelBootstrapNodeSpec::from_parts(
                &["User"],
                &empty_properties,
            ))
            .with_edge(KernelBootstrapEdgeSpec::from_parts(
                0,
                1,
                Some("KNOWS"),
                &empty_properties,
            ));

        let summary = bootstrap_kernel_overlay_graph(&mut overlay, &spec)
            .expect("bootstrap kernel overlay graph");
        assert_eq!(summary.nodes.len(), 2);
        assert_eq!(summary.edges.len(), 1);
        assert_eq!(summary.vertex_ordinals.len(), 2);
        assert_eq!(summary.locators.len(), 1);
        assert!(!summary.refreshed.forward.is_empty());
        assert!(!summary.refreshed.reverse.is_empty());
        assert_eq!(summary.edges[0].label.as_deref(), Some("KNOWS"));
        assert!(matches!(
            overlay.write_history(),
            [
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapEdge(_),
                RewriteOverlayWriteEvent::BootstrapGraph(graph_summary)
            ] if graph_summary.nodes.len() == 2
                && graph_summary.edges.len() == 1
                && graph_summary.vertex_ordinals.len() == 2
                && graph_summary.locators.len() == 1
                && !graph_summary.refreshed.forward.is_empty()
                && !graph_summary.refreshed.reverse.is_empty()
        ));
        let overlay_projection = match overlay.write_history().last() {
            Some(RewriteOverlayWriteEvent::BootstrapGraph(graph_summary)) => {
                graph_summary.projection()
            }
            other => panic!("expected aggregate bootstrap graph event, got {other:?}"),
        };
        assert_eq!(summary.projection(), overlay_projection);
    }

    #[test]
    fn kernel_bootstrap_bridge_can_create_node_and_edge_records() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut bridge = RewriteKernelBootstrapBridge::new(&mut facade, &memory);
        let person_labels = vec!["Person".to_owned()];
        let empty_properties = PropertyMap::new();

        let alice = bridge
            .bootstrap_node(&person_labels, &empty_properties)
            .expect("bootstrap alice");
        let bob = bridge
            .bootstrap_node(&person_labels, &empty_properties)
            .expect("bootstrap bob");
        let edge = bridge
            .bootstrap_edge(alice.id, bob.id, Some("KNOWS"), &empty_properties)
            .expect("bootstrap edge");

        assert_eq!(bridge.nodes().len(), 2);
        assert_eq!(bridge.edges().len(), 1);
        assert_eq!(edge.src, alice.id);
        assert_eq!(edge.dst, bob.id);
        assert_eq!(edge.label.as_deref(), Some("KNOWS"));
        assert_eq!(bridge.vertex_ordinals().len(), 2);
    }

    #[test]
    fn kernel_overlay_graph_implements_basic_graph_kernel_reads_and_writes() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let person_labels = vec!["Person".to_owned()];
        let alice_properties: PropertyMap = [("name".to_owned(), Value::Text("Alice".into()))]
            .into_iter()
            .collect();
        let bob_properties: PropertyMap = [("name".to_owned(), Value::Text("Bob".into()))]
            .into_iter()
            .collect();
        let edge_properties: PropertyMap = [("weight".to_owned(), Value::Int64(5))]
            .into_iter()
            .collect();

        let alice = graph
            .insert_node(&person_labels, &alice_properties)
            .expect("insert alice");
        let bob = graph
            .insert_node(&person_labels, &bob_properties)
            .expect("insert bob");
        let edge = graph
            .insert_edge(alice.id, bob.id, Some("KNOWS"), &edge_properties)
            .expect("insert edge");

        let scanned = graph.scan_nodes(Some("Person")).expect("scan nodes");
        assert_eq!(scanned.len(), 2);

        let property_scanned = graph
            .scan_nodes_by_property("name", &Value::Text("Alice".into()), CmpOp::Eq)
            .expect("scan by property");
        assert_eq!(property_scanned, vec![alice.clone()]);

        let expansions = graph
            .expand(
                alice.id,
                EdgeDirection::PointingRight,
                EdgeLabelFilter::Single("KNOWS"),
            )
            .expect("expand");
        assert_eq!(expansions.len(), 1);
        assert_eq!(expansions[0].edge, edge);
        assert_eq!(expansions[0].node, bob);

        let edge_scanned = graph
            .scan_edges_by_property("weight", &Value::Int64(5))
            .expect("scan edges");
        assert_eq!(edge_scanned.len(), 1);
    }

    #[test]
    fn expand_single_label_uses_label_id_path_when_edge_label_string_missing() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let person_labels = vec!["Person".to_owned()];
        let empty_properties = PropertyMap::new();
        let alice = graph
            .insert_node(&person_labels, &empty_properties)
            .expect("insert alice");
        let bob = graph
            .insert_node(&person_labels, &empty_properties)
            .expect("insert bob");
        let edge = graph
            .insert_edge(alice.id, bob.id, Some("KNOWS"), &empty_properties)
            .expect("insert edge");

        graph
            .bridge_mut()
            .edges
            .get_mut(&edge.id)
            .expect("edge should exist")
            .label = None;

        let expansions = graph
            .expand(
                alice.id,
                EdgeDirection::PointingRight,
                EdgeLabelFilter::Single("KNOWS"),
            )
            .expect("expand");
        assert_eq!(expansions.len(), 1);
        assert_eq!(expansions[0].edge.id, edge.id);
    }

    #[test]
    fn expand_anyof_uses_label_id_path_when_edge_label_strings_missing() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let person_labels = vec!["Person".to_owned()];
        let empty_properties = PropertyMap::new();
        let alice = graph
            .insert_node(&person_labels, &empty_properties)
            .expect("insert alice");
        let bob = graph
            .insert_node(&person_labels, &empty_properties)
            .expect("insert bob");
        let charlie = graph
            .insert_node(&person_labels, &empty_properties)
            .expect("insert charlie");
        let e1 = graph
            .insert_edge(alice.id, bob.id, Some("KNOWS"), &empty_properties)
            .expect("insert knows");
        let e2 = graph
            .insert_edge(alice.id, charlie.id, Some("LIKES"), &empty_properties)
            .expect("insert likes");

        graph
            .bridge_mut()
            .edges
            .get_mut(&e1.id)
            .expect("edge1 should exist")
            .label = None;
        graph
            .bridge_mut()
            .edges
            .get_mut(&e2.id)
            .expect("edge2 should exist")
            .label = None;

        let names = vec!["KNOWS".to_owned(), "LIKES".to_owned()];
        let expansions = graph
            .expand(
                alice.id,
                EdgeDirection::PointingRight,
                EdgeLabelFilter::AnyOf(&names),
            )
            .expect("expand");
        let mut ids: Vec<u64> = expansions.iter().map(|e| e.edge.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![e1.id, e2.id]);
    }

    #[test]
    fn kernel_overlay_graph_exposes_property_write_summary() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let person_labels = vec!["Person".to_owned()];
        let empty_properties = PropertyMap::new();

        let alice = graph
            .insert_node(&person_labels, &empty_properties)
            .expect("insert alice");
        graph
            .set_node_property(alice.id, "uid", &Value::Text("alice".into()))
            .expect("set node property");
        graph.flush().expect("flush after deferred property set");
        let summary = graph
            .last_property_write_summary()
            .expect("node property summary");
        assert!(summary.flushed_sections.property_store);
        assert!(summary.flushed_sections.logical_index);
        assert_eq!(
            summary.mutation.node_store_operations,
            vec![PropertyIndexNodeStoreMutationKind::Rebuild]
        );
        assert!(!summary.mutation.touched_node_ids.is_empty());
        let node_property_projection = summary.projection();
        assert_projected_history(
            graph.write_history(),
            vec![
                RewriteWriteEventProjection::BootstrapVertices(
                    RewriteBootstrapVerticesProjection {
                        ordinals: vec![(0, 0)],
                        refreshed: RewriteRefreshedVertices::new(Vec::new(), Vec::new()),
                    },
                ),
                RewriteWriteEventProjection::Property(node_property_projection.clone()),
            ],
        );

        let edge = graph
            .insert_edge(alice.id, alice.id, Some("SELF"), &empty_properties)
            .expect("insert self edge");
        let insert_summary = graph
            .last_insert_edge_summary()
            .expect("edge insert summary");
        assert!(insert_summary.inserted);
        assert_eq!(
            insert_summary.path,
            Some(EdgeInsertPath::BaseAppend { logical_index: 0 })
        );
        assert_eq!(graph.insert_edge_history().len(), 1);
        assert_eq!(insert_summary.ensure_capacity_projection(), None);
        graph
            .set_edge_property(edge.id, "weight", &Value::Int64(1))
            .expect("set edge property");
        graph.flush().expect("flush after deferred edge property set");
        let edge_summary = graph
            .last_property_write_summary()
            .expect("edge property summary");
        assert!(edge_summary.flushed_sections.property_store);
        assert!(edge_summary.flushed_sections.logical_index);
        assert_eq!(
            edge_summary.mutation.node_store_operations,
            vec![PropertyIndexNodeStoreMutationKind::Rebuild]
        );
        assert!(!edge_summary.mutation.touched_node_ids.is_empty());
        let edge_property_projection = edge_summary.projection();
        assert_eq!(graph.property_write_history().len(), 2);
        let shared_history = graph.shared_write_history();
        assert!(matches!(
            shared_history.as_slice(),
            [
                RewriteWriteEventProjection::BootstrapVertices(_),
                RewriteWriteEventProjection::Property(_),
                RewriteWriteEventProjection::InsertEdge(_),
                RewriteWriteEventProjection::Property(_)
            ]
        ));
        assert_eq!(
            shared_history[1],
            RewriteWriteEventProjection::Property(node_property_projection)
        );
        assert_eq!(
            shared_history[3],
            RewriteWriteEventProjection::Property(edge_property_projection)
        );
        assert_eq!(
            project_overlay_write_event(graph.write_history().last().expect("last overlay event")),
            vec![RewriteWriteEventProjection::Property(
                graph
                    .last_property_write_summary()
                    .expect("last property summary")
                    .projection()
            )]
        );
        assert!(matches!(
            graph.write_history(),
            [
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::Property(_),
                RewriteOverlayWriteEvent::InsertEdge(_),
                RewriteOverlayWriteEvent::Property(_)
            ]
        ));
        let shared_history = graph.shared_write_history();
        assert!(matches!(
            shared_history.as_slice(),
            [
                RewriteWriteEventProjection::BootstrapVertices(_),
                RewriteWriteEventProjection::Property(_),
                RewriteWriteEventProjection::InsertEdge(_),
                RewriteWriteEventProjection::Property(_)
            ]
        ));
    }

    #[test]
    fn vertex_label_index_tracks_label_mutations() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let person_labels = vec!["Person".to_owned()];
        let empty_properties = PropertyMap::new();

        let alice = graph
            .insert_node(&person_labels, &empty_properties)
            .expect("insert alice");
        assert_eq!(graph.scan_nodes(Some("Person")).expect("scan").len(), 1);

        graph
            .add_node_label(alice.id, "Engineer")
            .expect("add label");
        assert_eq!(graph.scan_nodes(Some("Engineer")).expect("scan").len(), 1);

        graph
            .remove_node_label(alice.id, "Engineer")
            .expect("remove label");
        assert_eq!(graph.scan_nodes(Some("Engineer")).expect("scan").len(), 0);

        graph.delete_node(alice.id, true).expect("delete node");
        assert_eq!(graph.scan_nodes(Some("Person")).expect("scan").len(), 0);
    }

    #[test]
    fn vertex_label_index_promotes_to_bitmap_when_hot_label_grows() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let labels = vec!["Hot".to_owned()];
        let empty_properties = PropertyMap::new();

        for _ in 0..VERTEX_LABEL_PROMOTION_THRESHOLD_BASE {
            graph
                .insert_node(&labels, &empty_properties)
                .expect("insert node");
        }

        let label_id = graph
            .bridge()
            .lookup_label_id("Hot")
            .expect("hot label id should exist");
        let membership = graph
            .bridge()
            .vertex_label_index
            .by_label
            .get(&label_id)
            .expect("membership should exist");
        assert!(matches!(membership, LabelMembership::Roaring(_)));
    }

    #[test]
    fn scan_nodes_label_path_has_no_duplicates_or_missing() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let labels = vec!["Person".to_owned()];
        let empty = PropertyMap::new();
        let mut expected = Vec::new();
        for _ in 0..64 {
            let node = graph.insert_node(&labels, &empty).expect("insert node");
            expected.push(node.id);
        }
        let scanned = graph.scan_nodes(Some("Person")).expect("scan");
        let mut got: Vec<NodeId> = scanned.into_iter().map(|n| n.id).collect();
        expected.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, expected);
    }

    #[test]
    fn planner_uses_runtime_label_cardinality_id_from_overlay_graph() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let empty = PropertyMap::new();
        for _ in 0..200 {
            graph
                .insert_node(&["L7".to_owned()], &empty)
                .expect("insert L7");
        }
        for _ in 0..5 {
            graph
                .insert_node(&["L3".to_owned()], &empty)
                .expect("insert L3");
        }
        let program = parser::parse("MATCH (a:L7)-[:X]->(b:L3) RETURN a, b").expect("parse");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("block");
        let Statement::Query(q) = block.first else {
            panic!("expected query statement");
        };
        let plan = build_plan(&q.left, Some(&graph)).expect("plan");
        let anchor = plan.annotations.optimizer.anchor.expect("anchor");
        assert_eq!(&*anchor.variable, "b");
    }

    #[test]
    fn vertex_label_catalog_codec_round_trips() {
        let mut labels = BTreeMap::new();
        labels.insert("Person".to_owned(), 1);
        labels.insert("Engineer".to_owned(), 2);
        let mut index = VertexLabelIndex::default();
        index.insert(1, 0, VERTEX_LABEL_PROMOTION_THRESHOLD_BASE);
        index.insert(1, 2, VERTEX_LABEL_PROMOTION_THRESHOLD_BASE);
        index.insert(2, 2, VERTEX_LABEL_PROMOTION_THRESHOLD_BASE);
        let encoded = encode_vertex_label_catalog(&labels, 3, &index, &VertexGcState::default());
        let decoded = decode_vertex_label_catalog(&encoded).expect("decode");
        assert_eq!(decoded.0, labels);
        assert_eq!(decoded.1, 3);
        assert_eq!(decoded.2, index);
    }

    #[test]
    fn delete_detach_enqueues_reclaim_and_vacuum_moves_to_free_list() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let node = graph
            .insert_node(&["Person".to_owned()], &PropertyMap::new())
            .expect("insert node");

        graph.delete_node(node.id, true).expect("delete");
        let before = graph.vacuum_stats();
        assert_eq!(before.queue_len, 1);
        assert_eq!(before.free_list_len, 0);

        let processed = graph.vacuum_step(8);
        assert_eq!(processed, 1);
        let after = graph.vacuum_stats();
        assert_eq!(after.queue_len, 0);
        assert_eq!(after.free_list_len, 1);
    }

    #[test]
    fn kernel_overlay_graph_insert_summary_exposes_rebalance_projection() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut manager = crate::low_level::RegionManager::with_bucket_size(
            crate::low_level::BucketSizeInPages::DEFAULT,
        );
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 16_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 16_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                crate::low_level::ExtentChain::new(
                    crate::low_level::ExtentId::NULL,
                    crate::low_level::ExtentId::NULL,
                    logical_len,
                    crate::low_level::WasmPages::new(1),
                    crate::low_level::WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        for kind in [
            RegionKind::NodePropertyStore,
            RegionKind::EdgePropertyStore,
            RegionKind::PropertyIndex,
        ] {
            manager
                .define_bucket_region(kind, crate::property_store::default_property_region_chain());
        }
        facade.manager = manager;
        facade.graph.forward.0.vertices = vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)];
        facade.graph.forward.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
            EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
            EdgeEntry::new(NodeId::from(99u8), EdgeMeta::new(12, false)),
        ]);
        facade.graph.forward.0.overflow_entries = vec![OverflowEntry::new(
            90,
            EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
            LogOffset::EMPTY,
        )];
        facade.graph.forward.0.label_index_entries = vec![VertexLabelIndexEntry::new(0, 0)];
        facade.graph.forward.0.label_ranges = Vec::new();
        facade.graph.forward.0.dirty_regions = Default::default();
        facade.graph.forward.0.dirty_vertices.clear();

        facade.graph.reverse.0.vertices = vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)];
        facade.graph.reverse.0.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
            EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, false)),
            EdgeEntry::new(NodeId::from(88u8), EdgeMeta::new(12, false)),
        ]);
        facade.graph.reverse.0.overflow_entries = vec![OverflowEntry::new(
            90,
            EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
            LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.label_index_entries = vec![VertexLabelIndexEntry::new(0, 0)];
        facade.graph.reverse.0.label_ranges = Vec::new();
        facade.graph.reverse.0.dirty_regions = Default::default();
        facade.graph.reverse.0.dirty_vertices.clear();

        let mut sidecar = EdgeLogicalLocatorSidecar::new();
        sidecar.set(
            90,
            LogicalEdgeLocator::base(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        facade.graph.replace_logical_locator_sidecar(sidecar);
        facade.graph.insert_policy = GraphInsertPolicy {
            max_overflow_chain_len: 1,
            rebalance_window_radius: 0,
            ..GraphInsertPolicy::default()
        };
        let mut graph = facade.bind_kernel_overlay(&memory);
        let empty_properties = PropertyMap::new();
        let src = NodeId::from(1u8);
        let dst = NodeId::from(2u8);
        graph.bridge_mut().next_node_id = 6;
        graph.bridge_mut().next_edge_id = 90;
        graph.bridge_mut().nodes.insert(
            src,
            NodeRecord {
                id: src,
                labels: vec!["User".to_owned()],
                properties: PropertyMap::new(),
            },
        );
        graph.bridge_mut().nodes.insert(
            dst,
            NodeRecord {
                id: dst,
                labels: vec!["Post".to_owned()],
                properties: PropertyMap::new(),
            },
        );
        graph.bridge_mut().vertex_ordinals = vec![
            RewriteVertexOrdinalMapping {
                vertex_ref: src.into(),
                forward_ordinal: 0,
                reverse_ordinal: 0,
            },
            RewriteVertexOrdinalMapping {
                vertex_ref: dst.into(),
                forward_ordinal: 0,
                reverse_ordinal: 0,
            },
        ];
        graph.bridge_mut().vertex_ordinal_by_node_id = graph
            .bridge()
            .vertex_ordinals
            .iter()
            .map(|mapping| (mapping.vertex_ref.into(), *mapping))
            .collect();
        graph.bridge_mut().forward_base_slots_by_ordinal = vec![vec![Some(90), Some(92), Some(93)]];
        graph.bridge_mut().reverse_base_slots_by_ordinal = vec![vec![Some(90), Some(92), Some(93)]];

        let decision = graph
            .bridge()
            .store
            .graph()
            .choose_insert_decision_with_incoming_live_entries(src.into(), 0, dst.into(), 0, 1)
            .expect("rebalance test insert decision");
        let crate::GraphInsertDecision::RebalanceRequired(plan) = decision else {
            panic!("expected rebalance-required decision, got {decision:?}");
        };
        let local = graph
            .bridge()
            .store
            .graph()
            .plan_local_rebalance(plan)
            .expect("rebalance test local plan");
        graph
            .bridge()
            .store
            .graph()
            .build_local_rebalance_delta(local)
            .expect("rebalance test delta");

        let edge = graph
            .insert_edge(src, dst, Some("LIKES"), &empty_properties)
            .expect("insert with rebalance");
        let summary = graph.last_insert_edge_summary().expect("insert summary");
        assert_eq!(edge.id, 91);
        assert!(summary.inserted);
        assert!(summary.rebalanced);
        assert_eq!(
            summary.path,
            Some(EdgeInsertPath::BaseReuseTombstone { logical_index: 3 })
        );
        let ensure = summary
            .ensure_capacity_projection()
            .expect("ensure-capacity projection");
        assert!(ensure.rebalanced);
        assert!(ensure.total_displacement >= 0);
        assert!(ensure.max_displacement >= 0);
        assert_eq!(graph.insert_edge_history().len(), 1);
        let shared_history = graph.shared_write_history();
        assert!(matches!(
            shared_history.as_slice(),
            [
                ..,
                RewriteWriteEventProjection::EnsureCapacity(_),
                RewriteWriteEventProjection::InsertEdge(_)
            ]
        ));
        assert_eq!(
            shared_history.last(),
            Some(&RewriteWriteEventProjection::InsertEdge(
                summary.projection()
            ))
        );
        assert!(matches!(
            graph.write_history().last(),
            Some(RewriteOverlayWriteEvent::InsertEdge(_))
        ));
        assert_eq!(
            project_overlay_write_event(graph.write_history().last().expect("last overlay event")),
            vec![
                RewriteWriteEventProjection::EnsureCapacity(ensure.clone()),
                RewriteWriteEventProjection::InsertEdge(summary.projection()),
            ]
        );
        assert_eq!(
            graph.formatted_write_history().last(),
            Some(&format!(
                "insert-edge inserted=true path={:?} rebalanced=true displacement=({}, {}) refreshed=(1,1) fwd=[0] rev=[0]",
                summary.path, summary.total_displacement, summary.max_displacement,
            ))
        );
        assert_eq!(
            graph.formatted_write_history().iter().rev().nth(1),
            Some(&format!(
                "ensure-capacity rebalanced=true displacement=({}, {}) refreshed=(1,1) fwd=[0] rev=[0]",
                ensure.total_displacement, ensure.max_displacement,
            ))
        );
        assert_eq!(
            graph.formatted_last_write_event(),
            Some(format!(
                "insert-edge inserted=true path={:?} rebalanced=true displacement=({}, {}) refreshed=(1,1) fwd=[0] rev=[0]",
                summary.path, summary.total_displacement, summary.max_displacement,
            ))
        );
        let debug_report = RewriteDiagnosticsView::debug_report(&graph);
        assert!(debug_report.contains("ensure-capacity rebalanced=true"));
        assert!(debug_report.contains("insert-edge inserted=true"));
    }

    #[test]
    fn rewrite_graph_store_adapter_can_convert_into_kernel_overlay() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let adapter = facade.bind(&memory);
        let mut graph = adapter.into_kernel_overlay();
        let person_labels = vec!["Person".to_owned()];
        let properties = PropertyMap::new();

        let alice = graph
            .insert_node(&person_labels, &properties)
            .expect("insert alice");
        let fetched = graph.get_node(alice.id).expect("get node");
        assert_eq!(fetched, Some(alice));
    }

    #[test]
    fn kernel_overlay_graph_can_update_and_delete_edges() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let person_labels = vec!["Person".to_owned()];
        let properties = PropertyMap::new();

        let alice = graph
            .insert_node(&person_labels, &properties)
            .expect("insert alice");
        let bob = graph
            .insert_node(&person_labels, &properties)
            .expect("insert bob");
        let edge = graph
            .insert_edge(alice.id, bob.id, Some("KNOWS"), &properties)
            .expect("insert edge");
        let insert_summary = graph
            .last_insert_edge_summary()
            .expect("insert edge summary");
        assert!(insert_summary.inserted);
        assert_eq!(graph.insert_edge_history().len(), 1);
        assert_eq!(insert_summary.ensure_capacity_projection(), None);

        let updated = graph
            .set_edge_label(edge.id, Some("LIKES"))
            .expect("set edge label");
        assert_eq!(updated.label.as_deref(), Some("LIKES"));
        let label_summary = graph
            .last_edge_write_summary()
            .expect("edge label write summary");
        assert_eq!(
            label_summary.operation,
            RewriteOverlayEdgeMutationKind::ReplaceLabel
        );
        assert_eq!(label_summary.path, GraphMutationPath::Base);
        let label_projection = label_summary.projection();
        assert_eq!(
            label_projection.operation,
            RewriteEdgeWriteOperation::ReplaceLabel
        );
        assert_eq!(label_projection.path, GraphMutationPath::Base);
        assert!(label_summary.refreshed.forward.contains(&0));
        assert_eq!(
            project_overlay_write_event(graph.write_history().last().expect("last overlay event")),
            vec![RewriteWriteEventProjection::Edge(label_projection.clone())]
        );

        let expansions = graph
            .expand(
                alice.id,
                EdgeDirection::PointingRight,
                EdgeLabelFilter::Single("LIKES"),
            )
            .expect("expand after label update");
        assert_eq!(expansions.len(), 1);
        assert_eq!(expansions[0].edge.id, edge.id);

        graph.delete_edge(edge.id).expect("delete edge");
        let delete_summary = graph
            .last_edge_write_summary()
            .expect("edge delete write summary");
        assert_eq!(
            delete_summary.operation,
            RewriteOverlayEdgeMutationKind::Delete
        );
        assert_eq!(delete_summary.path, GraphMutationPath::Base);
        let delete_projection = delete_summary.projection();
        assert_eq!(
            delete_projection.operation,
            RewriteEdgeWriteOperation::Delete
        );
        assert_eq!(delete_projection.path, GraphMutationPath::Base);
        assert!(delete_summary.refreshed.forward.contains(&0));
        assert_eq!(graph.edge_write_history().len(), 2);
        assert!(matches!(
            graph.write_history(),
            [
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::InsertEdge(_),
                RewriteOverlayWriteEvent::Edge(_),
                RewriteOverlayWriteEvent::Edge(_)
            ]
        ));
        let expansions = graph
            .expand(
                alice.id,
                EdgeDirection::PointingRight,
                EdgeLabelFilter::Single("LIKES"),
            )
            .expect("expand after delete");
        assert!(expansions.is_empty());
    }

    #[test]
    fn deleting_edge_removes_property_keys_and_values() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let person_labels = vec!["Person".to_owned()];
        let edge_properties: PropertyMap = [("weight".to_owned(), Value::Int64(5))]
            .into_iter()
            .collect();

        let alice = graph
            .insert_node(&person_labels, &PropertyMap::new())
            .expect("insert alice");
        let bob = graph
            .insert_node(&person_labels, &PropertyMap::new())
            .expect("insert bob");
        let edge = graph
            .insert_edge(alice.id, bob.id, Some("KNOWS"), &edge_properties)
            .expect("insert edge");

        assert_eq!(
            graph
                .get_edge_property_value(edge.id, "weight")
                .expect("edge property lookup before delete"),
            Some(Value::Int64(5))
        );
        assert!(
            graph
                .all_property_key_names()
                .expect("property keys before delete")
                .contains("weight")
        );

        graph.delete_edge(edge.id).expect("delete edge");

        assert_eq!(
            graph
                .get_edge_property_value(edge.id, "weight")
                .expect("edge property lookup after delete"),
            None
        );
        assert!(
            !graph
                .all_property_key_names()
                .expect("property keys after delete")
                .contains("weight")
        );
    }

    #[test]
    fn kernel_overlay_graph_can_delete_node_with_detach() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let person_labels = vec!["Person".to_owned()];
        let properties = PropertyMap::new();

        let alice = graph
            .insert_node(&person_labels, &properties)
            .expect("insert alice");
        let bob = graph
            .insert_node(&person_labels, &properties)
            .expect("insert bob");
        let edge = graph
            .insert_edge(alice.id, bob.id, Some("KNOWS"), &properties)
            .expect("insert edge");
        assert_eq!(graph.insert_edge_history().len(), 1);

        let err = graph
            .delete_node(alice.id, false)
            .expect_err("delete without detach should fail");
        assert!(matches!(err, GraphError::Message(_)));

        graph
            .delete_node(alice.id, true)
            .expect("detach delete should succeed");
        let summary = graph
            .last_node_delete_summary()
            .expect("node delete summary");
        assert!(summary.detached);
        assert_eq!(summary.deleted_edge_ids, vec![edge.id]);
        assert_eq!(summary.edge_writes.len(), 1);
        assert_eq!(
            summary.edge_writes[0].operation,
            RewriteOverlayEdgeMutationKind::Delete
        );
        let event_projection = graph
            .write_history()
            .iter()
            .find_map(RewriteOverlayWriteEvent::node_delete_projection)
            .expect("node delete event projection");
        assert_eq!(summary.projection(), event_projection);
        assert_eq!(
            project_overlay_write_event(graph.write_history().last().expect("last overlay event")),
            vec![RewriteWriteEventProjection::NodeDelete(
                summary.projection()
            )]
        );
        assert_eq!(
            graph.shared_write_history().last(),
            Some(&RewriteWriteEventProjection::NodeDelete(
                summary.projection()
            ))
        );
        assert_eq!(
            graph.formatted_write_history().last(),
            Some(
                &"node-delete detached=true edges=1 deleted=[1] edge-writes=Delete@Base".to_owned()
            )
        );
        assert_eq!(
            graph.formatted_last_write_event(),
            Some(
                "node-delete detached=true edges=1 deleted=[1] edge-writes=Delete@Base".to_owned()
            )
        );
        assert_eq!(
            RewriteKernelOverlayObservability::formatted_last_write_event(&graph),
            Some(
                "node-delete detached=true edges=1 deleted=[1] edge-writes=Delete@Base".to_owned()
            )
        );
        assert_eq!(
            RewriteDiagnosticsView::formatted_last_write_event(&graph),
            Some(
                "node-delete detached=true edges=1 deleted=[1] edge-writes=Delete@Base".to_owned()
            )
        );
        assert_eq!(
            RewriteDiagnosticsView::debug_report(&graph)
                .lines()
                .last()
                .map(str::to_owned),
            Some(
                "node-delete detached=true edges=1 deleted=[1] edge-writes=Delete@Base".to_owned()
            )
        );
        assert_eq!(graph.node_delete_history().len(), 1);
        assert!(matches!(
            graph.write_history(),
            [
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::BootstrapNode(_),
                RewriteOverlayWriteEvent::InsertEdge(_),
                RewriteOverlayWriteEvent::Edge(_),
                RewriteOverlayWriteEvent::NodeDelete(_)
            ]
        ));
        assert_eq!(graph.get_node(alice.id).expect("get node"), None);
        assert!(
            graph
                .expand(
                    bob.id,
                    EdgeDirection::PointingLeft,
                    EdgeLabelFilter::Single("KNOWS")
                )
                .expect("expand after detach delete")
                .is_empty()
        );
        assert_eq!(graph.bridge().edges().get(&edge.id), None);
    }

    #[test]
    fn deleting_node_removes_property_keys_and_values() {
        let memory = VecMemory::default();
        let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
        let mut graph = facade.bind_kernel_overlay(&memory);
        let node_properties: PropertyMap = [("name".to_owned(), Value::Text("Alice".to_owned()))]
            .into_iter()
            .collect();

        let alice = graph
            .insert_node(&["Person".to_owned()], &node_properties)
            .expect("insert alice");

        assert_eq!(
            graph
                .get_node_property_value(alice.id, "name")
                .expect("node property lookup before delete"),
            Some(Value::Text("Alice".to_owned()))
        );
        assert!(
            graph
                .all_property_key_names()
                .expect("property keys before delete")
                .contains("name")
        );

        graph.delete_node(alice.id, true).expect("delete node");

        assert_eq!(
            graph
                .get_node_property_value(alice.id, "name")
                .expect("node property lookup after delete"),
            None
        );
        assert!(
            !graph
                .all_property_key_names()
                .expect("property keys after delete")
                .contains("name")
        );
    }

    /// Run with `GLEAPH_BENCH_PROFILE=1` to print split timings for repeated `bootstrap_node`.
    /// Use `cargo test ... bench_profile_smoke -- --test-threads=1` so samples are not mixed.
    #[test]
    fn bench_profile_smoke_repeated_bootstrap_node() {
        use crate::bench_profile;

        let iterations = if std::env::var_os("GLEAPH_BENCH_PROFILE").is_some() {
            bench_profile::reset();
            80
        } else {
            2
        };

        let mut harness = RewriteGraphPmaKernelHarness::bootstrap_empty(VecMemory::default())
            .expect("harness");
        let mut graph = harness.bind_overlay();
        let labels = vec!["Person".to_owned()];
        let mut props = PropertyMap::new();
        props.insert("name".into(), Value::Text("canbench".into()));
        for _ in 0..iterations {
            graph.bootstrap_node(&labels, &props).expect("bootstrap_node");
        }
        if std::env::var_os("GLEAPH_BENCH_PROFILE").is_some() {
            bench_profile::dump_report("repeated_bootstrap_node");
        }
    }

    /// Run with `GLEAPH_BENCH_PROFILE=1` to print expand vs surface-walk timings on a small ring.
    #[test]
    fn bench_profile_smoke_expand_labeled_ring() {
        use crate::bench_profile;

        let mut spec = KernelBootstrapGraphSpec::empty();
        for i in 0..32 {
            let mut props = PropertyMap::new();
            props.insert("uid".into(), Value::Text(format!("u{i}")));
            spec = spec.with_node(KernelBootstrapNodeSpec::from_parts(&["Person"], &props));
        }
        for i in 0..32usize {
            let j = (i + 1) % 32;
            spec = spec.with_edge(KernelBootstrapEdgeSpec::from_parts(
                i,
                j,
                Some("KNOWS"),
                &PropertyMap::new(),
            ));
        }

        let mut harness = RewriteGraphPmaKernelHarness::bootstrap_empty(VecMemory::default())
            .expect("harness");
        let (graph, summary) = harness
            .bind_overlay_with_graph(&spec)
            .expect("seed ring");
        let first = summary.nodes[0].id;

        let iterations = if std::env::var_os("GLEAPH_BENCH_PROFILE").is_some() {
            bench_profile::reset();
            4000
        } else {
            1
        };

        for _ in 0..iterations {
            let _ = graph
                .expand(
                    first,
                    EdgeDirection::PointingRight,
                    EdgeLabelFilter::Single("KNOWS"),
                )
                .expect("expand");
        }

        if std::env::var_os("GLEAPH_BENCH_PROFILE").is_some() {
            bench_profile::dump_report("expand_labeled_ring");
        }
    }
}
