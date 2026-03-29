//! Small integration helpers for upper layers that use the rewrite service boundary.
//!
//! The low-level rewrite APIs are intentionally explicit, but upper layers
//! often want a slightly more declarative entrypoint for initial graph
//! bootstrap. This module keeps that convenience separate from the core
//! facade/runtime types.

use std::collections::BTreeMap;

use gleaph_gql::ast::CmpOp;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql::Value;
use gleaph_graph_kernel::{EdgeId, LabelId, NodeId};
use gleaph_graph_kernel::{
    EdgeRecord, Expansion, GraphError, GraphRead, GraphResult, GraphWrite, NodeRecord, PropertyMap,
};

use crate::facade::{
    RewriteBootstrapEdgeProjection, RewriteBootstrapGraphProjection,
    RewriteBootstrapGraphWriteSummary, RewriteBootstrapVerticesProjection,
    RewriteEdgeLocatorMapping, RewriteEdgeWriteOperation, RewriteEdgeWriteProjection,
    RewriteEnsureCapacityProjection, RewriteGraphPma, RewriteGraphPmaResult, RewriteGraphService,
    RewriteGraphStore, RewriteGraphStoreAdapter, RewriteInsertEdgeProjection,
    RewriteNodeDeleteProjection, RewritePropertyMutationWriteSummary, RewriteRefreshedVertices,
    RewriteVertexOrdinalMapping, RewriteWriteEventProjection,
};
use crate::observability::{format_last_write_event, format_write_event_history};
use crate::property_index::{
    scan_edge_property_index_value_prefix_from_stable_memory,
    scan_node_property_index_property_prefix_from_stable_memory,
    scan_node_property_index_value_prefix_from_stable_memory,
};
use crate::stable::Memory;
use crate::PropertyStoreError;
use crate::{EdgeInsertPath, GraphMutationPath};

/// Declarative specification for one initial logical edge during bootstrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootstrapEdgeSpec {
    /// Semantic id of the edge to insert.
    pub edge_id: EdgeId,
    /// Index into [`BootstrapGraphSpec::vertex_ids`] for the source vertex.
    pub src_index: usize,
    /// Index into [`BootstrapGraphSpec::vertex_ids`] for the destination vertex.
    pub dst_index: usize,
    /// Label id for the inserted edge.
    pub label_id: LabelId,
}

impl BootstrapEdgeSpec {
    /// Creates one logical edge bootstrap spec from source/destination indexes and label id.
    pub fn new(edge_id: EdgeId, src_index: usize, dst_index: usize, label_id: LabelId) -> Self {
        Self {
            edge_id,
            src_index,
            dst_index,
            label_id,
        }
    }

    /// Creates one owned bootstrap edge spec from a borrowed tuple.
    pub fn from_tuple(edge: &(EdgeId, usize, usize, LabelId)) -> Self {
        Self::new(edge.0, edge.1, edge.2, edge.3)
    }
}

/// Declarative bootstrap request for one initial graph fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapGraphSpec {
    /// Vertex ids to append first, in logical order.
    pub vertex_ids: Vec<NodeId>,
    /// Initial logical edges between those bootstrapped vertices.
    pub initial_edges: Vec<BootstrapEdgeSpec>,
}

impl BootstrapGraphSpec {
    /// Creates one bootstrap specification from owned vertex and edge lists.
    pub fn new(vertex_ids: Vec<NodeId>, initial_edges: Vec<BootstrapEdgeSpec>) -> Self {
        Self {
            vertex_ids,
            initial_edges,
        }
    }

    /// Creates one bootstrap specification by cloning borrowed vertex and edge slices.
    pub fn from_slices(vertex_ids: &[NodeId], initial_edges: &[BootstrapEdgeSpec]) -> Self {
        Self {
            vertex_ids: vertex_ids.to_vec(),
            initial_edges: initial_edges.to_vec(),
        }
    }

    /// Creates one bootstrap specification by cloning borrowed vertices and edge tuples.
    pub fn from_tuples(
        vertex_ids: &[NodeId],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> Self {
        Self {
            vertex_ids: vertex_ids.to_vec(),
            initial_edges: initial_edges
                .iter()
                .map(BootstrapEdgeSpec::from_tuple)
                .collect(),
        }
    }

    /// Creates one empty logical bootstrap specification.
    pub fn empty() -> Self {
        Self {
            vertex_ids: Vec::new(),
            initial_edges: Vec::new(),
        }
    }

    /// Appends one vertex id and returns the extended bootstrap specification.
    pub fn with_vertex(mut self, vertex_id: NodeId) -> Self {
        self.vertex_ids.push(vertex_id);
        self
    }

    /// Appends one logical edge spec and returns the extended bootstrap specification.
    pub fn with_edge(mut self, edge: BootstrapEdgeSpec) -> Self {
        self.initial_edges.push(edge);
        self
    }

    fn edge_tuples(&self) -> Vec<(EdgeId, usize, usize, LabelId)> {
        self.initial_edges
            .iter()
            .map(|edge| (edge.edge_id, edge.src_index, edge.dst_index, edge.label_id))
            .collect()
    }
}

/// Declarative specification for one kernel-facing node bootstrap step.
#[derive(Clone, Debug, PartialEq)]
pub struct KernelBootstrapNodeSpec {
    /// Labels assigned to the newly bootstrapped node record.
    pub labels: Vec<String>,
    /// Properties assigned to the newly bootstrapped node record.
    pub properties: PropertyMap,
}

impl KernelBootstrapNodeSpec {
    /// Creates one node bootstrap spec from owned labels and properties.
    ///
    /// Use this when the caller is already constructing the owned record payload.
    /// Thin helper paths should generally prefer [`Self::from_parts`] or
    /// [`Self::labeled_ref`] so they can stay borrowed-first.
    pub fn new(labels: Vec<String>, properties: PropertyMap) -> Self {
        Self { labels, properties }
    }

    /// Creates one node bootstrap spec by cloning borrowed labels and properties.
    pub fn from_parts<S: AsRef<str>>(labels: &[S], properties: &PropertyMap) -> Self {
        Self {
            labels: labels
                .iter()
                .map(|label| label.as_ref().to_owned())
                .collect(),
            properties: properties.clone(),
        }
    }

    /// Creates one node bootstrap spec with a single owned label.
    ///
    /// This is the owned constructor variant. Thin helper paths should prefer
    /// [`Self::labeled_ref`] when they are only forwarding borrowed inputs.
    pub fn labeled(label: impl Into<String>, properties: PropertyMap) -> Self {
        Self {
            labels: vec![label.into()],
            properties,
        }
    }

    /// Creates one single-label node bootstrap spec by cloning borrowed inputs.
    pub fn labeled_ref(label: &str, properties: &PropertyMap) -> Self {
        Self {
            labels: vec![label.to_owned()],
            properties: properties.clone(),
        }
    }
}

/// Declarative specification for one kernel-facing edge bootstrap step.
#[derive(Clone, Debug, PartialEq)]
pub struct KernelBootstrapEdgeSpec {
    /// Index into [`KernelBootstrapGraphSpec::nodes`] for the source node.
    pub src_index: usize,
    /// Index into [`KernelBootstrapGraphSpec::nodes`] for the destination node.
    pub dst_index: usize,
    /// Optional edge label stored in the overlay record layer.
    pub label: Option<String>,
    /// Properties assigned to the newly bootstrapped edge record.
    pub properties: PropertyMap,
}

impl KernelBootstrapEdgeSpec {
    /// Creates one edge bootstrap spec from source/destination indexes and owned payload.
    ///
    /// Use this when the caller is already materializing owned edge bootstrap
    /// data. Thin helper paths should generally prefer [`Self::from_parts`] or
    /// [`Self::unlabeled`] so they can stay borrowed-first.
    pub fn new(
        src_index: usize,
        dst_index: usize,
        label: Option<String>,
        properties: PropertyMap,
    ) -> Self {
        Self {
            src_index,
            dst_index,
            label,
            properties,
        }
    }

    /// Creates one edge bootstrap spec by cloning borrowed label and properties.
    pub fn from_parts(
        src_index: usize,
        dst_index: usize,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> Self {
        Self {
            src_index,
            dst_index,
            label: label.map(str::to_owned),
            properties: properties.clone(),
        }
    }

    /// Creates one unlabeled edge bootstrap spec by cloning borrowed properties.
    pub fn unlabeled(src_index: usize, dst_index: usize, properties: &PropertyMap) -> Self {
        Self::from_parts(src_index, dst_index, None, properties)
    }
}

/// Declarative bootstrap request for one kernel-facing overlay graph fragment.
#[derive(Clone, Debug, PartialEq)]
pub struct KernelBootstrapGraphSpec {
    /// Nodes to create first, in logical order.
    pub nodes: Vec<KernelBootstrapNodeSpec>,
    /// Edges to create between the bootstrapped nodes.
    pub edges: Vec<KernelBootstrapEdgeSpec>,
}

impl KernelBootstrapGraphSpec {
    /// Creates one kernel-facing bootstrap specification from owned node and edge lists.
    pub fn new(nodes: Vec<KernelBootstrapNodeSpec>, edges: Vec<KernelBootstrapEdgeSpec>) -> Self {
        Self { nodes, edges }
    }

    /// Creates one kernel bootstrap specification by cloning borrowed node and edge slices.
    pub fn from_slices(
        nodes: &[KernelBootstrapNodeSpec],
        edges: &[KernelBootstrapEdgeSpec],
    ) -> Self {
        Self {
            nodes: nodes.to_vec(),
            edges: edges.to_vec(),
        }
    }

    /// Creates one empty kernel bootstrap graph specification.
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// Appends one node spec and returns the extended bootstrap graph specification.
    pub fn with_node(mut self, node: KernelBootstrapNodeSpec) -> Self {
        self.nodes.push(node);
        self
    }

    /// Appends one edge spec and returns the extended bootstrap graph specification.
    pub fn with_edge(mut self, edge: KernelBootstrapEdgeSpec) -> Self {
        self.edges.push(edge);
        self
    }
}

/// Result of applying one kernel-facing bootstrap specification.
#[derive(Clone, Debug, PartialEq)]
pub struct KernelBootstrapGraphSummary {
    /// Node records created in the same order as the request.
    pub nodes: Vec<NodeRecord>,
    /// Edge records created in the same order as the request.
    pub edges: Vec<EdgeRecord>,
    /// Mapping from bootstrapped logical node ids to rewrite ordinals.
    pub vertex_ordinals: Vec<RewriteVertexOrdinalMapping>,
    /// Locator mappings for bootstrapped edges, in input order.
    pub locators: Vec<RewriteEdgeLocatorMapping>,
    /// Vertices refreshed during the same bootstrap sequence.
    pub refreshed: RewriteRefreshedVertices,
}

impl KernelBootstrapGraphSummary {
    /// Projects the kernel bootstrap summary onto the fields shared with facade bootstrap summaries.
    pub fn projection(&self) -> RewriteBootstrapGraphProjection {
        RewriteBootstrapGraphProjection {
            vertex_ordinals: self.vertex_ordinals.clone(),
            locators: self.locators.clone(),
            refreshed: self.refreshed.clone(),
        }
    }
}

/// Applies one declarative bootstrap specification through the rewrite service boundary.
pub fn bootstrap_graph(
    service: &mut impl RewriteGraphService,
    spec: &BootstrapGraphSpec,
) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
    let initial_edges = spec.edge_tuples();
    service.bootstrap_vertices_and_edges(&spec.vertex_ids, &initial_edges)
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
    vertex_ordinals: Vec<RewriteVertexOrdinalMapping>,
    edge_locators: BTreeMap<EdgeId, RewriteEdgeLocatorMapping>,
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

/// Observability summary for one overlay-level edge mutation write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteOverlayEdgeWriteSummary {
    /// Overlay-level operation kind.
    pub operation: RewriteOverlayEdgeMutationKind,
    /// Chosen graph-level physical path.
    pub path: GraphMutationPath,
    /// Vertices whose label sidecars were refreshed during writeback.
    pub refreshed: RewriteRefreshedVertices,
}

impl RewriteOverlayEdgeWriteSummary {
    /// Projects the overlay edge write summary onto the fields shared with facade edge events.
    pub fn projection(&self) -> RewriteEdgeWriteProjection {
        RewriteEdgeWriteProjection {
            operation: match self.operation {
                RewriteOverlayEdgeMutationKind::ReplaceLabel => {
                    RewriteEdgeWriteOperation::ReplaceLabel
                }
                RewriteOverlayEdgeMutationKind::Delete => RewriteEdgeWriteOperation::Delete,
            },
            path: self.path,
            refreshed: self.refreshed.clone(),
        }
    }
}

/// Observability summary for one overlay-level edge insert write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteOverlayInsertEdgeSummary {
    /// Whether the insert actually happened.
    pub inserted: bool,
    /// Chosen insert path when the edge was inserted.
    pub path: Option<EdgeInsertPath>,
    /// Whether a local rebalance happened before the insert.
    pub rebalanced: bool,
    /// Total displacement applied by the rebalance, if any.
    pub total_displacement: i64,
    /// Maximum side displacement applied by the rebalance, if any.
    pub max_displacement: i64,
    /// Vertices refreshed during the same writeback.
    pub refreshed: RewriteRefreshedVertices,
}

impl RewriteOverlayInsertEdgeSummary {
    /// Projects the implicit pre-insert ensure-capacity step when a rebalance happened.
    pub fn ensure_capacity_projection(&self) -> Option<RewriteEnsureCapacityProjection> {
        self.rebalanced.then(|| RewriteEnsureCapacityProjection {
            rebalanced: true,
            total_displacement: self.total_displacement,
            max_displacement: self.max_displacement,
            refreshed: self.refreshed.clone(),
        })
    }

    /// Projects the overlay insert summary onto the fields shared with facade insert events.
    pub fn projection(&self) -> RewriteInsertEdgeProjection {
        RewriteInsertEdgeProjection {
            inserted: self.inserted,
            path: self.path,
            rebalanced: self.rebalanced,
            total_displacement: self.total_displacement,
            max_displacement: self.max_displacement,
            refreshed: self.refreshed.clone(),
        }
    }
}

/// Edge mutation kind observed through the rewrite overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewriteOverlayEdgeMutationKind {
    /// Edge label replacement.
    ReplaceLabel,
    /// Edge tombstone/delete.
    Delete,
}

/// Observability summary for one overlay-level node delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteOverlayNodeDeleteSummary {
    /// Whether the delete used detach semantics.
    pub detached: bool,
    /// Incident edge ids deleted as part of the node delete.
    pub deleted_edge_ids: Vec<EdgeId>,
    /// Edge write summaries observed while deleting incident edges.
    pub edge_writes: Vec<RewriteOverlayEdgeWriteSummary>,
}

impl RewriteOverlayNodeDeleteSummary {
    /// Projects the overlay node-delete summary onto the fields shared with higher-level observers.
    pub fn projection(&self) -> RewriteNodeDeleteProjection {
        RewriteNodeDeleteProjection {
            detached: self.detached,
            deleted_edge_ids: self.deleted_edge_ids.clone(),
            edge_writes: self
                .edge_writes
                .iter()
                .map(RewriteOverlayEdgeWriteSummary::projection)
                .collect(),
        }
    }
}

/// Observability summary for one overlay-level node bootstrap write.
#[derive(Clone, Debug, PartialEq)]
pub struct RewriteOverlayNodeBootstrapSummary {
    /// Bootstrapped node record visible through the overlay.
    pub node: NodeRecord,
    /// Forward/reverse ordinals assigned by the rewrite kernel.
    pub ordinals: (usize, usize),
    /// Vertices refreshed during the same writeback.
    pub refreshed: RewriteRefreshedVertices,
}

impl RewriteOverlayNodeBootstrapSummary {
    fn mapping(&self) -> RewriteVertexOrdinalMapping {
        RewriteVertexOrdinalMapping {
            vertex_id: self.node.id,
            forward_ordinal: self.ordinals.0,
            reverse_ordinal: self.ordinals.1,
        }
    }

    fn projection(&self) -> RewriteBootstrapVerticesProjection {
        RewriteBootstrapVerticesProjection {
            ordinals: vec![self.ordinals],
            refreshed: self.refreshed.clone(),
        }
    }
}

/// Observability summary for one overlay-level edge bootstrap write.
#[derive(Clone, Debug, PartialEq)]
pub struct RewriteOverlayEdgeBootstrapSummary {
    /// Bootstrapped edge record visible through the overlay.
    pub edge: EdgeRecord,
    /// Chosen insert path for the edge pair.
    pub path: EdgeInsertPath,
    /// Vertices refreshed during the same writeback.
    pub refreshed: RewriteRefreshedVertices,
}

impl RewriteOverlayEdgeBootstrapSummary {
    fn projection(&self) -> RewriteBootstrapEdgeProjection {
        RewriteBootstrapEdgeProjection {
            path: Some(self.path),
            refreshed: self.refreshed.clone(),
        }
    }
}

/// Observability summary for one overlay-level graph bootstrap sequence.
#[derive(Clone, Debug, PartialEq)]
pub struct RewriteOverlayBootstrapGraphSummary {
    /// Node records created during the same bootstrap sequence.
    pub nodes: Vec<NodeRecord>,
    /// Edge records created during the same bootstrap sequence.
    pub edges: Vec<EdgeRecord>,
    /// Mapping from bootstrapped logical node ids to rewrite ordinals.
    pub vertex_ordinals: Vec<RewriteVertexOrdinalMapping>,
    /// Locator mappings for bootstrapped edges, in input order.
    pub locators: Vec<RewriteEdgeLocatorMapping>,
    /// Vertices refreshed during the same bootstrap sequence.
    pub refreshed: RewriteRefreshedVertices,
}

impl RewriteOverlayBootstrapGraphSummary {
    fn from_bootstrap_summaries(
        node_summaries: &[RewriteOverlayNodeBootstrapSummary],
        edge_summaries: &[RewriteOverlayEdgeBootstrapSummary],
        locators: Vec<RewriteEdgeLocatorMapping>,
    ) -> Self {
        let mut refreshed_forward = Vec::new();
        let mut refreshed_reverse = Vec::new();
        for summary in node_summaries {
            refreshed_forward.extend(summary.refreshed.forward.iter().copied());
            refreshed_reverse.extend(summary.refreshed.reverse.iter().copied());
        }
        for summary in edge_summaries {
            refreshed_forward.extend(summary.refreshed.forward.iter().copied());
            refreshed_reverse.extend(summary.refreshed.reverse.iter().copied());
        }
        refreshed_forward.sort_unstable();
        refreshed_forward.dedup();
        refreshed_reverse.sort_unstable();
        refreshed_reverse.dedup();

        Self {
            nodes: node_summaries
                .iter()
                .map(|summary| summary.node.clone())
                .collect(),
            edges: edge_summaries
                .iter()
                .map(|summary| summary.edge.clone())
                .collect(),
            vertex_ordinals: node_summaries
                .iter()
                .map(|summary| summary.mapping())
                .collect(),
            locators,
            refreshed: RewriteRefreshedVertices::new(refreshed_forward, refreshed_reverse),
        }
    }

    /// Projects the overlay bootstrap summary onto the fields shared with facade bootstrap summaries.
    pub fn projection(&self) -> RewriteBootstrapGraphProjection {
        RewriteBootstrapGraphProjection {
            vertex_ordinals: self.vertex_ordinals.clone(),
            locators: self.locators.clone(),
            refreshed: self.refreshed.clone(),
        }
    }
}

/// One overlay-level mutation event recorded in observation order.
#[derive(Clone, Debug, PartialEq)]
pub enum RewriteOverlayWriteEvent {
    /// Node bootstrap observed through the overlay.
    BootstrapNode(RewriteOverlayNodeBootstrapSummary),
    /// Edge bootstrap observed through the overlay.
    BootstrapEdge(RewriteOverlayEdgeBootstrapSummary),
    /// Edge insert observed through the overlay.
    InsertEdge(RewriteOverlayInsertEdgeSummary),
    /// Aggregate graph bootstrap observed through the overlay.
    BootstrapGraph(RewriteOverlayBootstrapGraphSummary),
    /// Property mutation write observed through the overlay.
    Property(RewritePropertyMutationWriteSummary),
    /// Edge mutation write observed through the overlay.
    Edge(RewriteOverlayEdgeWriteSummary),
    /// Node delete observed through the overlay.
    NodeDelete(RewriteOverlayNodeDeleteSummary),
}

impl RewriteOverlayWriteEvent {
    /// Projects one overlay write event onto zero or more shared event projections.
    pub fn shared_projections(&self) -> Vec<RewriteWriteEventProjection> {
        match self {
            Self::InsertEdge(summary) => {
                let mut projections = Vec::with_capacity(2);
                if let Some(ensure) = summary.ensure_capacity_projection() {
                    projections.push(RewriteWriteEventProjection::EnsureCapacity(ensure));
                }
                projections.push(RewriteWriteEventProjection::InsertEdge(
                    summary.projection(),
                ));
                projections
            }
            _ => self.shared_projection().into_iter().collect(),
        }
    }

    /// Projects overlay write events onto the shared cross-surface event vocabulary.
    pub fn shared_projection(&self) -> Option<RewriteWriteEventProjection> {
        match self {
            Self::BootstrapNode(summary) => Some(RewriteWriteEventProjection::BootstrapVertices(
                summary.projection(),
            )),
            Self::BootstrapEdge(summary) => Some(RewriteWriteEventProjection::BootstrapEdge(
                summary.projection(),
            )),
            Self::InsertEdge(summary) => Some(RewriteWriteEventProjection::InsertEdge(
                summary.projection(),
            )),
            Self::BootstrapGraph(summary) => Some(RewriteWriteEventProjection::BootstrapGraph(
                summary.projection(),
            )),
            Self::Property(summary) => {
                Some(RewriteWriteEventProjection::Property(summary.projection()))
            }
            Self::Edge(summary) => Some(RewriteWriteEventProjection::Edge(summary.projection())),
            Self::NodeDelete(summary) => Some(RewriteWriteEventProjection::NodeDelete(
                summary.projection(),
            )),
        }
    }

    /// Projects overlay node-delete events onto the fields shared with higher-level observers.
    pub fn node_delete_projection(&self) -> Option<RewriteNodeDeleteProjection> {
        match self {
            Self::NodeDelete(summary) => Some(summary.projection()),
            _ => None,
        }
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

/// Read-only observability boundary for the kernel-facing rewrite overlay.
pub trait RewriteKernelOverlayObservability {
    /// Returns the most recent property-write summary observed through this overlay.
    fn last_property_write_summary(&self) -> Option<&RewritePropertyMutationWriteSummary>;

    /// Returns recent property-write summaries in observation order.
    fn property_write_history(&self) -> &[RewritePropertyMutationWriteSummary];

    /// Returns the most recent insert-edge summary observed through this overlay.
    fn last_insert_edge_summary(&self) -> Option<&RewriteOverlayInsertEdgeSummary>;

    /// Returns recent insert-edge summaries in observation order.
    fn insert_edge_history(&self) -> &[RewriteOverlayInsertEdgeSummary];

    /// Returns the most recent edge-write summary observed through this overlay.
    fn last_edge_write_summary(&self) -> Option<&RewriteOverlayEdgeWriteSummary>;

    /// Returns recent edge-write summaries in observation order.
    fn edge_write_history(&self) -> &[RewriteOverlayEdgeWriteSummary];

    /// Returns the most recent node-delete summary observed through this overlay.
    fn last_node_delete_summary(&self) -> Option<&RewriteOverlayNodeDeleteSummary>;

    /// Returns recent node-delete summaries in observation order.
    fn node_delete_history(&self) -> &[RewriteOverlayNodeDeleteSummary];

    /// Returns recent overlay write events in observation order.
    fn write_history(&self) -> &[RewriteOverlayWriteEvent];

    /// Returns the recent overlay write history projected onto the shared event vocabulary.
    fn shared_write_history(&self) -> Vec<RewriteWriteEventProjection> {
        self.write_history()
            .iter()
            .flat_map(RewriteOverlayWriteEvent::shared_projections)
            .collect()
    }

    /// Returns the recent overlay write history formatted as compact diagnostics lines.
    fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(&self.shared_write_history())
    }

    /// Returns the most recent overlay write event formatted as one diagnostics line.
    fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(&self.shared_write_history())
    }
}

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

    /// Bootstraps logical vertices and edges directly, without requiring an explicit spec value.
    pub fn bootstrap_vertices_and_edges(
        &mut self,
        vertex_ids: &[NodeId],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
        let spec = BootstrapGraphSpec::from_tuples(vertex_ids, initial_edges);
        self.bootstrap_graph(&spec)
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

impl<'a, S: RewriteGraphStore, M: Memory> RewriteKernelOverlayGraph<'a, S, M> {
    /// Creates one overlay graph from a rewrite bootstrap bridge.
    pub fn new(bridge: RewriteKernelBootstrapBridge<'a, S, M>) -> Self {
        Self { bridge }
    }

    /// Returns the underlying bootstrap bridge.
    pub fn bridge(&self) -> &RewriteKernelBootstrapBridge<'a, S, M> {
        &self.bridge
    }

    /// Returns mutable access to the underlying bootstrap bridge.
    pub fn bridge_mut(&mut self) -> &mut RewriteKernelBootstrapBridge<'a, S, M> {
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
    pub fn into_bridge(self) -> RewriteKernelBootstrapBridge<'a, S, M> {
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

impl<'a, S: RewriteGraphStore, M: Memory> RewriteKernelOverlayObservability
    for RewriteKernelOverlayGraph<'a, S, M>
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

impl<'a, S: RewriteGraphStore, M: Memory> RewriteGraphStoreAdapter<'a, S, M> {
    /// Converts one bound rewrite adapter into a kernel-facing overlay graph.
    pub fn into_kernel_overlay(self) -> RewriteKernelOverlayGraph<'a, &'a mut S, M> {
        let (store, memory) = self.into_parts();
        RewriteKernelOverlayGraph::new(RewriteKernelBootstrapBridge::new(store, memory))
    }
}

impl<'a, S: RewriteGraphStore, M: Memory> RewriteKernelBootstrapBridge<'a, S, M> {
    /// Creates one bootstrap bridge over a bound rewrite graph adapter.
    pub fn new(store: S, memory: &'a M) -> Self {
        Self {
            store,
            memory,
            next_node_id: 0,
            next_edge_id: 0,
            next_label_id: 1,
            label_ids: BTreeMap::new(),
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            vertex_ordinals: Vec::new(),
            edge_locators: BTreeMap::new(),
            forward_base_slots_by_ordinal: Vec::new(),
            reverse_base_slots_by_ordinal: Vec::new(),
            last_property_write_summary: None,
            last_insert_edge_summary: None,
            last_edge_write_summary: None,
            last_node_delete_summary: None,
            property_write_history: Vec::new(),
            insert_edge_history: Vec::new(),
            edge_write_history: Vec::new(),
            node_delete_history: Vec::new(),
            write_history: Vec::new(),
        }
    }

    /// Returns the currently bootstrapped node records.
    pub fn nodes(&self) -> &BTreeMap<NodeId, NodeRecord> {
        &self.nodes
    }

    /// Returns the currently bootstrapped edge records.
    pub fn edges(&self) -> &BTreeMap<EdgeId, EdgeRecord> {
        &self.edges
    }

    /// Returns surface-local vertex ordinal mappings in forward order.
    pub fn vertex_ordinals(&self) -> &[RewriteVertexOrdinalMapping] {
        &self.vertex_ordinals
    }

    /// Returns the most recent property-write summary observed through this bridge.
    pub fn last_property_write_summary(&self) -> Option<&RewritePropertyMutationWriteSummary> {
        self.last_property_write_summary.as_ref()
    }

    /// Returns recent property-write summaries in observation order.
    pub fn property_write_history(&self) -> &[RewritePropertyMutationWriteSummary] {
        &self.property_write_history
    }

    /// Returns the most recent insert-edge summary observed through this bridge.
    pub fn last_insert_edge_summary(&self) -> Option<&RewriteOverlayInsertEdgeSummary> {
        self.last_insert_edge_summary.as_ref()
    }

    /// Returns recent insert-edge summaries in observation order.
    pub fn insert_edge_history(&self) -> &[RewriteOverlayInsertEdgeSummary] {
        &self.insert_edge_history
    }

    /// Returns the most recent edge-write summary observed through this bridge.
    pub fn last_edge_write_summary(&self) -> Option<&RewriteOverlayEdgeWriteSummary> {
        self.last_edge_write_summary.as_ref()
    }

    /// Returns recent edge-write summaries in observation order.
    pub fn edge_write_history(&self) -> &[RewriteOverlayEdgeWriteSummary] {
        &self.edge_write_history
    }

    /// Returns the most recent node-delete summary observed through this bridge.
    pub fn last_node_delete_summary(&self) -> Option<&RewriteOverlayNodeDeleteSummary> {
        self.last_node_delete_summary.as_ref()
    }

    /// Returns recent node-delete summaries in observation order.
    pub fn node_delete_history(&self) -> &[RewriteOverlayNodeDeleteSummary] {
        &self.node_delete_history
    }

    /// Returns recent overlay write events in observation order.
    pub fn write_history(&self) -> &[RewriteOverlayWriteEvent] {
        &self.write_history
    }

    fn record_write_event(&mut self, event: RewriteOverlayWriteEvent) {
        self.write_history.push(event);
        if self.write_history.len() > OVERLAY_SUMMARY_HISTORY_LIMIT {
            self.write_history.remove(0);
        }
    }

    fn record_property_write_summary(&mut self, summary: RewritePropertyMutationWriteSummary) {
        self.record_write_event(RewriteOverlayWriteEvent::Property(summary.clone()));
        self.last_property_write_summary = Some(summary.clone());
        self.property_write_history.push(summary);
        if self.property_write_history.len() > OVERLAY_SUMMARY_HISTORY_LIMIT {
            self.property_write_history.remove(0);
        }
    }

    fn record_insert_edge_summary(&mut self, summary: RewriteOverlayInsertEdgeSummary) {
        self.record_write_event(RewriteOverlayWriteEvent::InsertEdge(summary.clone()));
        self.last_insert_edge_summary = Some(summary.clone());
        self.insert_edge_history.push(summary);
        if self.insert_edge_history.len() > OVERLAY_SUMMARY_HISTORY_LIMIT {
            self.insert_edge_history.remove(0);
        }
    }

    fn record_edge_write_summary(&mut self, summary: RewriteOverlayEdgeWriteSummary) {
        self.record_write_event(RewriteOverlayWriteEvent::Edge(summary.clone()));
        self.last_edge_write_summary = Some(summary.clone());
        self.edge_write_history.push(summary);
        if self.edge_write_history.len() > OVERLAY_SUMMARY_HISTORY_LIMIT {
            self.edge_write_history.remove(0);
        }
    }

    fn record_node_delete_summary(&mut self, summary: RewriteOverlayNodeDeleteSummary) {
        self.record_write_event(RewriteOverlayWriteEvent::NodeDelete(summary.clone()));
        self.last_node_delete_summary = Some(summary.clone());
        self.node_delete_history.push(summary);
        if self.node_delete_history.len() > OVERLAY_SUMMARY_HISTORY_LIMIT {
            self.node_delete_history.remove(0);
        }
    }

    fn record_node_bootstrap_summary(&mut self, summary: RewriteOverlayNodeBootstrapSummary) {
        self.record_write_event(RewriteOverlayWriteEvent::BootstrapNode(summary));
    }

    fn record_edge_bootstrap_summary(&mut self, summary: RewriteOverlayEdgeBootstrapSummary) {
        self.record_write_event(RewriteOverlayWriteEvent::BootstrapEdge(summary));
    }

    fn record_bootstrap_graph_summary(&mut self, summary: RewriteOverlayBootstrapGraphSummary) {
        self.record_write_event(RewriteOverlayWriteEvent::BootstrapGraph(summary));
    }

    /// Bootstraps one logical node by appending one new rewrite vertex slot pair.
    pub fn bootstrap_node(
        &mut self,
        labels: &[String],
        properties: &PropertyMap,
    ) -> GraphResult<NodeRecord> {
        self.next_node_id += 1;
        let node_id = NodeId::try_from(self.next_node_id)
            .map_err(|_| GraphError::Message("node id overflow during bootstrap".into()))?;

        let summary = self
            .store
            .bootstrap_vertices_and_edges_and_write(&[node_id], &[], self.memory)
            .map_err(|err| GraphError::Message(err.to_string()))?;
        let refreshed = summary.refreshed.clone();
        let mapping =
            summary.vertex_ordinals.into_iter().next().ok_or_else(|| {
                GraphError::Message("bootstrap did not create vertex mapping".into())
            })?;

        self.vertex_ordinals.push(mapping);
        self.forward_base_slots_by_ordinal.push(Vec::new());
        self.reverse_base_slots_by_ordinal.push(Vec::new());

        self.persist_node_properties(node_id, &properties)?;
        let record = NodeRecord {
            id: node_id,
            labels: labels.to_vec(),
            properties: self.load_node_properties(node_id),
        };
        self.nodes.insert(node_id, record.clone());
        self.record_node_bootstrap_summary(RewriteOverlayNodeBootstrapSummary {
            node: record.clone(),
            ordinals: (mapping.forward_ordinal, mapping.reverse_ordinal),
            refreshed,
        });
        Ok(record)
    }

    /// Bootstraps one logical edge between already bootstrapped nodes.
    pub fn bootstrap_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        self.insert_edge_record(src, dst, label, properties, true)
    }

    /// Inserts one logical edge between already bootstrapped nodes.
    pub fn insert_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        self.insert_edge_record(src, dst, label, properties, false)
    }

    fn insert_edge_record(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
        bootstrap_event: bool,
    ) -> GraphResult<EdgeRecord> {
        let src_mapping = self
            .vertex_mapping(src)
            .ok_or(GraphError::NodeNotFound(src))?;
        let dst_mapping = self
            .vertex_mapping(dst)
            .ok_or(GraphError::NodeNotFound(dst))?;

        self.next_edge_id += 1;
        let edge_id = self.next_edge_id;
        let label_id = self.label_id_for(label);
        let (forward_rebalance_vertex_ids, forward_rebalance_base_edge_ids_by_ordinal) = match self
            .store
            .graph()
            .choose_insert_decision_with_incoming_live_entries(
                src,
                src_mapping.forward_ordinal,
                dst,
                dst_mapping.reverse_ordinal,
                1,
            ) {
            Some(crate::GraphInsertDecision::RebalanceRequired(plan)) => {
                let local = self
                    .store
                    .graph()
                    .plan_local_rebalance(plan)
                    .ok_or_else(|| {
                        GraphError::Message(
                            "failed to build local rebalance window for overlay insert".into(),
                        )
                    })?;
                let start = local.forward.start_ordinal;
                let end = local.forward.end_ordinal_exclusive;
                let vertex_ids = self
                    .vertex_ordinals
                    .get(start..end)
                    .ok_or_else(|| {
                        GraphError::Message(
                            "rebalance window exceeds overlay vertex mappings".into(),
                        )
                    })?
                    .iter()
                    .map(|mapping| mapping.vertex_id)
                    .collect();
                let base_edge_ids = self
                    .forward_base_slots_by_ordinal
                    .get(start..end)
                    .ok_or_else(|| {
                        GraphError::Message(
                            "rebalance window exceeds overlay base-slot mappings".into(),
                        )
                    })?
                    .iter()
                    .map(|slots| slots.iter().flatten().copied().collect())
                    .collect();
                (vertex_ids, base_edge_ids)
            }
            _ => (
                self.forward_vertex_ids(),
                self.forward_live_base_edge_ids_by_ordinal(),
            ),
        };

        let summary = self
            .store
            .insert_edge_pair_with_local_rebalance_and_write(
                edge_id,
                src,
                src_mapping.forward_ordinal,
                dst,
                dst_mapping.reverse_ordinal,
                label_id,
                &forward_rebalance_vertex_ids,
                &forward_rebalance_base_edge_ids_by_ordinal,
                self.memory,
            )
            .map_err(|err| GraphError::Message(err.to_string()))?;

        let refreshed = RewriteRefreshedVertices::new(
            summary.refreshed_forward_vertices.clone(),
            summary.refreshed_reverse_vertices.clone(),
        );
        let inserted = summary
            .insert
            .ok_or_else(|| GraphError::Message("edge insert produced no result".into()))?;
        let (path, locators) = match inserted {
            crate::GraphInsertResult::Inserted { path, locators } => (path, locators),
            crate::GraphInsertResult::RebalanceRequired(_) => {
                return Err(GraphError::Message(
                    "edge insert still requires rebalance after write helper".into(),
                ));
            }
        };
        if matches!(
            path,
            EdgeInsertPath::BaseAppend { .. } | EdgeInsertPath::BaseReuseTombstone { .. }
        ) {
            let Some(src_logical_index) =
                self.base_logical_index_from_path(path, src_mapping.forward_ordinal, locators.0)
            else {
                return Err(GraphError::Message(
                    "failed to resolve forward base logical index".into(),
                ));
            };
            let Some(dst_logical_index) = self.base_logical_index_from_reverse_locator(
                dst,
                dst_mapping.reverse_ordinal,
                locators.1,
            ) else {
                return Err(GraphError::Message(
                    "failed to resolve reverse base logical index".into(),
                ));
            };
            Self::set_base_slot(
                &mut self.forward_base_slots_by_ordinal[src_mapping.forward_ordinal],
                src_logical_index,
                edge_id,
            );
            Self::set_base_slot(
                &mut self.reverse_base_slots_by_ordinal[dst_mapping.reverse_ordinal],
                dst_logical_index,
                edge_id,
            );
        }
        self.edge_locators.insert(
            edge_id,
            RewriteEdgeLocatorMapping {
                edge_id,
                canonical: locators.0,
                forward: locators.0,
                reverse: locators.1,
            },
        );

        self.persist_edge_properties(edge_id, &properties)?;
        let record = EdgeRecord {
            id: edge_id,
            src,
            dst,
            label: label.map(str::to_owned),
            properties: self.load_edge_properties(edge_id),
        };
        self.edges.insert(edge_id, record.clone());
        if bootstrap_event {
            self.record_edge_bootstrap_summary(RewriteOverlayEdgeBootstrapSummary {
                edge: record.clone(),
                path,
                refreshed,
            });
        } else {
            let (total_displacement, max_displacement) = summary
                .rebalance
                .as_ref()
                .map(|rebalance| {
                    (
                        rebalance.apply.total_displacement(),
                        rebalance.apply.max_displacement(),
                    )
                })
                .unwrap_or((0, 0));
            self.record_insert_edge_summary(RewriteOverlayInsertEdgeSummary {
                inserted: summary.insert.is_some(),
                path: Some(path),
                rebalanced: summary.rebalance.is_some(),
                total_displacement,
                max_displacement,
                refreshed,
            });
        }
        Ok(record)
    }

    fn property_store_error(err: PropertyStoreError) -> GraphError {
        GraphError::Message(err.to_string())
    }

    fn persist_node_properties(
        &mut self,
        node_id: NodeId,
        properties: &PropertyMap,
    ) -> GraphResult<()> {
        for (name, value) in properties {
            self.store
                .set_node_property_value(node_id, name, value)
                .map_err(Self::property_store_error)?;
        }
        Ok(())
    }

    fn persist_edge_properties(
        &mut self,
        edge_id: EdgeId,
        properties: &PropertyMap,
    ) -> GraphResult<()> {
        for (name, value) in properties {
            self.store
                .set_edge_property_value(edge_id, name, value)
                .map_err(Self::property_store_error)?;
        }
        Ok(())
    }

    fn load_node_properties(&self, node_id: NodeId) -> PropertyMap {
        self.store.scan_node_properties(node_id)
    }

    fn load_edge_properties(&self, edge_id: EdgeId) -> PropertyMap {
        self.store.scan_edge_properties(edge_id)
    }

    fn node_property_candidate_ids_eq(&self, property: &str, value: &Value) -> Vec<NodeId> {
        if !self.store.node_property_store_is_dirty() {
            let encoded_value = value
                .to_stable_bytes()
                .expect("Value must encode to stable bytes");
            if let Ok(matches) = scan_node_property_index_value_prefix_from_stable_memory(
                self.store.manager(),
                self.memory,
                property,
                &encoded_value,
            ) {
                return matches
                    .into_iter()
                    .filter_map(|(key, _)| NodeId::try_from(key.entity_id).ok())
                    .collect();
            }
        }
        self.store.scan_node_ids_by_property_eq(property, value)
    }

    fn node_property_candidate_ids(&self, property: &str) -> Vec<NodeId> {
        if !self.store.node_property_store_is_dirty() {
            if let Ok(matches) = scan_node_property_index_property_prefix_from_stable_memory(
                self.store.manager(),
                self.memory,
                property,
            ) {
                return matches
                    .into_iter()
                    .filter_map(|(key, _)| NodeId::try_from(key.entity_id).ok())
                    .collect();
            }
        }
        self.store.scan_node_ids_by_property(property)
    }

    fn edge_property_candidate_ids_eq(&self, property: &str, value: &Value) -> Vec<EdgeId> {
        if !self.store.edge_property_store_is_dirty() {
            let encoded_value = value
                .to_stable_bytes()
                .expect("Value must encode to stable bytes");
            if let Ok(matches) = scan_edge_property_index_value_prefix_from_stable_memory(
                self.store.manager(),
                self.memory,
                property,
                &encoded_value,
            ) {
                return matches.into_iter().map(|(key, _)| key.entity_id).collect();
            }
        }
        self.store.scan_edge_ids_by_property_eq(property, value)
    }

    fn refreshed_node_record(&self, node_id: NodeId) -> GraphResult<NodeRecord> {
        let mut record = self
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or(GraphError::NodeNotFound(node_id))?;
        record.properties = self.load_node_properties(node_id);
        Ok(record)
    }

    fn refreshed_edge_record(&self, edge_id: EdgeId) -> GraphResult<EdgeRecord> {
        let mut record = self
            .edges
            .get(&edge_id)
            .cloned()
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        record.properties = self.load_edge_properties(edge_id);
        Ok(record)
    }

    fn vertex_mapping(&self, node_id: NodeId) -> Option<RewriteVertexOrdinalMapping> {
        self.vertex_ordinals
            .iter()
            .copied()
            .find(|mapping| mapping.vertex_id == node_id)
    }

    fn forward_vertex_ids(&self) -> Vec<NodeId> {
        self.vertex_ordinals
            .iter()
            .map(|mapping| mapping.vertex_id)
            .collect()
    }

    fn forward_live_base_edge_ids_by_ordinal(&self) -> Vec<Vec<EdgeId>> {
        self.forward_base_slots_by_ordinal
            .iter()
            .map(|slots| slots.iter().flatten().copied().collect())
            .collect()
    }

    fn base_logical_index_from_path(
        &self,
        path: EdgeInsertPath,
        ordinal: usize,
        locator: crate::EdgeLocator,
    ) -> Option<usize> {
        match path {
            EdgeInsertPath::BaseAppend { logical_index }
            | EdgeInsertPath::BaseReuseTombstone { logical_index } => Some(logical_index),
            EdgeInsertPath::Overflow => self
                .store
                .graph()
                .forward
                .resolve_edge_slot(locator.vertex, ordinal, locator)
                .and_then(|slot| match slot {
                    crate::ResolvedEdgeSlot::Base { logical_index } => Some(logical_index),
                    crate::ResolvedEdgeSlot::Overflow { .. } => None,
                }),
        }
    }

    fn base_logical_index_from_reverse_locator(
        &self,
        vertex: NodeId,
        ordinal: usize,
        locator: crate::EdgeLocator,
    ) -> Option<usize> {
        self.store
            .graph()
            .reverse
            .resolve_edge_slot(vertex, ordinal, locator)
            .and_then(|slot| match slot {
                crate::ResolvedEdgeSlot::Base { logical_index } => Some(logical_index),
                crate::ResolvedEdgeSlot::Overflow { .. } => None,
            })
    }

    fn find_base_logical_index(slots: &[Option<EdgeId>], edge_id: EdgeId) -> Option<usize> {
        slots.iter().position(|slot| slot == &Some(edge_id))
    }

    fn set_base_slot(slots: &mut Vec<Option<EdgeId>>, logical_index: usize, edge_id: EdgeId) {
        if logical_index >= slots.len() {
            slots.resize(logical_index + 1, None);
        }
        slots[logical_index] = Some(edge_id);
    }

    fn label_id_for(&mut self, label: Option<&str>) -> LabelId {
        let Some(label) = label else {
            return 0;
        };
        if let Some(existing) = self.label_ids.get(label).copied() {
            return existing;
        }
        let label_id = self.next_label_id;
        self.next_label_id = self.next_label_id.saturating_add(1);
        self.label_ids.insert(label.to_owned(), label_id);
        label_id
    }
}

impl<'a, S: RewriteGraphStore, M: Memory> GraphRead for RewriteKernelOverlayGraph<'a, S, M> {
    fn scan_nodes(&self, label: Option<&str>) -> GraphResult<Vec<NodeRecord>> {
        Ok(self
            .bridge
            .nodes
            .values()
            .filter(|node| label.is_none_or(|label| node.labels.iter().any(|it| it == label)))
            .filter_map(|node| self.bridge.refreshed_node_record(node.id).ok())
            .collect())
    }

    fn scan_nodes_by_property(
        &self,
        property: &str,
        value: &Value,
        cmp: CmpOp,
    ) -> GraphResult<Vec<NodeRecord>> {
        if cmp == CmpOp::Eq {
            return Ok(self
                .bridge
                .node_property_candidate_ids_eq(property, value)
                .into_iter()
                .filter_map(|node_id| self.bridge.refreshed_node_record(node_id).ok())
                .collect());
        }

        Ok(self
            .bridge
            .node_property_candidate_ids(property)
            .into_iter()
            .filter(|node_id| {
                self.bridge
                    .load_node_properties(*node_id)
                    .get(property)
                    .is_some_and(|candidate| compare_op(compare_values(candidate, value), cmp))
            })
            .filter_map(|node_id| self.bridge.refreshed_node_record(node_id).ok())
            .collect())
    }

    fn scan_edges_by_property(
        &self,
        property: &str,
        value: &Value,
    ) -> GraphResult<Vec<EdgeRecord>> {
        Ok(self
            .bridge
            .edge_property_candidate_ids_eq(property, value)
            .into_iter()
            .filter_map(|edge_id| self.bridge.refreshed_edge_record(edge_id).ok())
            .collect())
    }

    fn expand(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        label: Option<&str>,
    ) -> GraphResult<Vec<Expansion>> {
        let mut out = Vec::new();
        for edge in self.bridge.edges.values() {
            if label.is_some_and(|expected| edge.label.as_deref() != Some(expected)) {
                continue;
            }
            let matched = match direction {
                EdgeDirection::PointingRight => edge.src == from,
                EdgeDirection::PointingLeft => edge.dst == from,
                EdgeDirection::LeftOrRight
                | EdgeDirection::Undirected
                | EdgeDirection::LeftOrUndirected
                | EdgeDirection::UndirectedOrRight
                | EdgeDirection::AnyDirection => edge.src == from || edge.dst == from,
            };
            if !matched {
                continue;
            }
            let target = if edge.src == from { edge.dst } else { edge.src };
            if self.bridge.nodes.contains_key(&target) {
                out.push(Expansion {
                    edge: self.bridge.refreshed_edge_record(edge.id)?,
                    node: self.bridge.refreshed_node_record(target)?,
                });
            }
        }
        Ok(out)
    }

    fn get_node(&self, id: NodeId) -> GraphResult<Option<NodeRecord>> {
        self.bridge
            .nodes
            .contains_key(&id)
            .then(|| self.bridge.refreshed_node_record(id))
            .transpose()
    }
}

impl<'a, S: RewriteGraphStore, M: Memory> GraphWrite for RewriteKernelOverlayGraph<'a, S, M> {
    fn insert_node(
        &mut self,
        labels: &[String],
        properties: &PropertyMap,
    ) -> GraphResult<NodeRecord> {
        self.bridge.bootstrap_node(labels, properties)
    }

    fn insert_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        self.bridge.insert_edge(src, dst, label, properties)
    }

    fn set_node_property(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<NodeRecord> {
        if !self.bridge.nodes.contains_key(&node_id) {
            return Err(GraphError::NodeNotFound(node_id));
        }
        let summary = self
            .bridge
            .store
            .set_node_property_value_and_write(node_id, property, value, self.bridge.memory)
            .map_err(|err| GraphError::Message(err.to_string()))?;
        self.bridge.record_property_write_summary(summary);
        let properties = self.bridge.load_node_properties(node_id);
        let node = self
            .bridge
            .nodes
            .get_mut(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?;
        node.properties = properties;
        Ok(node.clone())
    }

    fn remove_node_property(&mut self, node_id: NodeId, property: &str) -> GraphResult<NodeRecord> {
        if !self.bridge.nodes.contains_key(&node_id) {
            return Err(GraphError::NodeNotFound(node_id));
        }
        let summary = self
            .bridge
            .store
            .remove_node_property_value_and_write(node_id, property, self.bridge.memory)
            .map_err(|err| GraphError::Message(err.to_string()))?;
        self.bridge.record_property_write_summary(summary);
        let properties = self.bridge.load_node_properties(node_id);
        let node = self
            .bridge
            .nodes
            .get_mut(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?;
        node.properties = properties;
        Ok(node.clone())
    }

    fn add_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord> {
        let node = self
            .bridge
            .nodes
            .get_mut(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?;
        if !node.labels.iter().any(|existing| existing == label) {
            node.labels.push(label.to_owned());
        }
        Ok(node.clone())
    }

    fn remove_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord> {
        let node = self
            .bridge
            .nodes
            .get_mut(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?;
        node.labels.retain(|existing| existing != label);
        Ok(node.clone())
    }

    fn set_edge_property(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<EdgeRecord> {
        if !self.bridge.edges.contains_key(&edge_id) {
            return Err(GraphError::EdgeNotFound(edge_id));
        }
        let summary = self
            .bridge
            .store
            .set_edge_property_value_and_write(edge_id, property, value, self.bridge.memory)
            .map_err(|err| GraphError::Message(err.to_string()))?;
        self.bridge.record_property_write_summary(summary);
        let properties = self.bridge.load_edge_properties(edge_id);
        let edge = self
            .bridge
            .edges
            .get_mut(&edge_id)
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        edge.properties = properties;
        Ok(edge.clone())
    }

    fn remove_edge_property(&mut self, edge_id: EdgeId, property: &str) -> GraphResult<EdgeRecord> {
        if !self.bridge.edges.contains_key(&edge_id) {
            return Err(GraphError::EdgeNotFound(edge_id));
        }
        let summary = self
            .bridge
            .store
            .remove_edge_property_value_and_write(edge_id, property, self.bridge.memory)
            .map_err(|err| GraphError::Message(err.to_string()))?;
        self.bridge.record_property_write_summary(summary);
        let properties = self.bridge.load_edge_properties(edge_id);
        let edge = self
            .bridge
            .edges
            .get_mut(&edge_id)
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        edge.properties = properties;
        Ok(edge.clone())
    }

    fn set_edge_label(&mut self, edge_id: EdgeId, label: Option<&str>) -> GraphResult<EdgeRecord> {
        let edge = self
            .bridge
            .edges
            .get(&edge_id)
            .cloned()
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        let src_mapping = self
            .bridge
            .vertex_mapping(edge.src)
            .ok_or(GraphError::NodeNotFound(edge.src))?;
        let dst_mapping = self
            .bridge
            .vertex_mapping(edge.dst)
            .ok_or(GraphError::NodeNotFound(edge.dst))?;
        let src_logical_index = Self::edge_base_logical_index(
            &self.bridge.forward_base_slots_by_ordinal[src_mapping.forward_ordinal],
            edge_id,
        )
        .unwrap_or_default();
        let dst_logical_index = Self::edge_base_logical_index(
            &self.bridge.reverse_base_slots_by_ordinal[dst_mapping.reverse_ordinal],
            edge_id,
        )
        .unwrap_or_default();
        let label_id = self.bridge.label_id_for(label);
        self.bridge
            .store
            .replace_edge_pair_and_write(
                edge_id,
                edge.src,
                src_mapping.forward_ordinal,
                src_logical_index,
                edge.dst,
                dst_mapping.reverse_ordinal,
                dst_logical_index,
                label_id,
                self.bridge.memory,
            )
            .map_err(|err| GraphError::Message(err.to_string()))
            .map(|summary| {
                self.bridge
                    .record_edge_write_summary(RewriteOverlayEdgeWriteSummary {
                        operation: RewriteOverlayEdgeMutationKind::ReplaceLabel,
                        path: summary.mutation.0,
                        refreshed: summary.refreshed,
                    });
            })?;
        let edge = self
            .bridge
            .edges
            .get_mut(&edge_id)
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        edge.label = label.map(str::to_owned);
        Ok(edge.clone())
    }

    fn delete_edge(&mut self, edge_id: EdgeId) -> GraphResult<()> {
        let edge = self
            .bridge
            .edges
            .get(&edge_id)
            .cloned()
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        let src_mapping = self
            .bridge
            .vertex_mapping(edge.src)
            .ok_or(GraphError::NodeNotFound(edge.src))?;
        let dst_mapping = self
            .bridge
            .vertex_mapping(edge.dst)
            .ok_or(GraphError::NodeNotFound(edge.dst))?;
        let src_logical_index = Self::edge_base_logical_index(
            &self.bridge.forward_base_slots_by_ordinal[src_mapping.forward_ordinal],
            edge_id,
        )
        .unwrap_or_default();
        let dst_logical_index = Self::edge_base_logical_index(
            &self.bridge.reverse_base_slots_by_ordinal[dst_mapping.reverse_ordinal],
            edge_id,
        )
        .unwrap_or_default();
        let path = self
            .bridge
            .store
            .tombstone_edge_pair_and_write(
                edge_id,
                edge.src,
                src_mapping.forward_ordinal,
                src_logical_index,
                edge.dst,
                dst_mapping.reverse_ordinal,
                dst_logical_index,
                self.bridge.memory,
            )
            .map_err(|err| GraphError::Message(err.to_string()))?;
        self.bridge
            .record_edge_write_summary(RewriteOverlayEdgeWriteSummary {
                operation: RewriteOverlayEdgeMutationKind::Delete,
                path: path.mutation,
                refreshed: path.refreshed,
            });
        let path = path.mutation;
        if matches!(path, crate::GraphMutationPath::Base) {
            if let Some(index) = Self::edge_base_logical_index(
                &self.bridge.forward_base_slots_by_ordinal[src_mapping.forward_ordinal],
                edge_id,
            ) {
                self.bridge.forward_base_slots_by_ordinal[src_mapping.forward_ordinal][index] =
                    None;
            }
            if let Some(index) = Self::edge_base_logical_index(
                &self.bridge.reverse_base_slots_by_ordinal[dst_mapping.reverse_ordinal],
                edge_id,
            ) {
                self.bridge.reverse_base_slots_by_ordinal[dst_mapping.reverse_ordinal][index] =
                    None;
            }
        }
        self.bridge.edge_locators.remove(&edge_id);
        self.bridge
            .edges
            .remove(&edge_id)
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        Ok(())
    }

    fn delete_node(&mut self, node_id: NodeId, detach: bool) -> GraphResult<()> {
        let incident_edge_ids: Vec<EdgeId> = self
            .bridge
            .edges
            .values()
            .filter(|edge| edge.src == node_id || edge.dst == node_id)
            .map(|edge| edge.id)
            .collect();
        if !incident_edge_ids.is_empty() && !detach {
            return Err(GraphError::Message("node has incident edges".into()));
        }
        let mut edge_writes = Vec::new();
        if detach {
            for edge_id in incident_edge_ids.iter().copied() {
                self.delete_edge(edge_id)?;
                if let Some(summary) = self.bridge.last_edge_write_summary().cloned() {
                    edge_writes.push(summary);
                }
            }
        }
        self.bridge
            .nodes
            .remove(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?;
        self.bridge
            .record_node_delete_summary(RewriteOverlayNodeDeleteSummary {
                detached: detach,
                deleted_edge_ids: incident_edge_ids,
                edge_writes,
            });
        Ok(())
    }
}

impl<'a, S: RewriteGraphStore, M: Memory> RewriteKernelOverlayGraph<'a, S, M> {
    fn edge_base_logical_index(slots: &[Option<EdgeId>], edge_id: EdgeId) -> Option<usize> {
        RewriteKernelBootstrapBridge::<S, M>::find_base_logical_index(slots, edge_id)
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
    use super::{
        bootstrap_graph, bootstrap_kernel_overlay_graph, BootstrapEdgeSpec, BootstrapGraphSpec,
        KernelBootstrapEdgeSpec, KernelBootstrapGraphSpec, KernelBootstrapNodeSpec,
        RewriteGraphPmaKernelHarness, RewriteKernelBootstrapBridge,
    };
    use crate::observability::{project_overlay_write_event, project_overlay_write_history};
    use crate::property_index::PropertyIndexNodeStoreMutationKind;
    use crate::stable::VecMemory;
    use crate::RewriteEdgeWriteOperation;
    use crate::{
        EdgeEntry, EdgeIndex, EdgeInsertPath, EdgeLocatorSidecar, EdgeMeta, GraphInsertPolicy,
        GraphMutationPath, LogOffset, OverflowEntry, RegionKind,
        RewriteBootstrapVerticesProjection, RewriteDiagnosticsView, RewriteGraphPma,
        RewriteKernelOverlayObservability, RewriteOverlayEdgeMutationKind,
        RewriteOverlayWriteEvent, RewriteRefreshedVertices, RewriteWriteEventProjection,
        SurfaceKind, VertexEntry, VertexLabelIndexEntry,
    };
    use gleaph_gql::ast::CmpOp;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::{GraphError, GraphRead, GraphWrite, NodeId, NodeRecord, PropertyMap};

    fn assert_projected_history(
        events: &[RewriteOverlayWriteEvent],
        expected: Vec<RewriteWriteEventProjection>,
    ) {
        assert_eq!(project_overlay_write_history(events), expected);
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
                    Some("AUTHORED")
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
            .expand(alice.id, EdgeDirection::PointingRight, Some("KNOWS"))
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
                crate::ExtentChain::new(
                    crate::ExtentId::NULL,
                    crate::ExtentId::NULL,
                    logical_len,
                    crate::WasmPages::new(1),
                    crate::WasmPages::new(0),
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
        facade.graph.forward.0.base_entries = vec![
            EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
            EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
            EdgeEntry::new(NodeId::from(99u8), EdgeMeta::new(12, false)),
        ];
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
        facade.graph.reverse.0.base_entries = vec![
            EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
            EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, false)),
            EdgeEntry::new(NodeId::from(88u8), EdgeMeta::new(12, false)),
        ];
        facade.graph.reverse.0.overflow_entries = vec![OverflowEntry::new(
            90,
            EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
            LogOffset::EMPTY,
        )];
        facade.graph.reverse.0.label_index_entries = vec![VertexLabelIndexEntry::new(0, 0)];
        facade.graph.reverse.0.label_ranges = Vec::new();
        facade.graph.reverse.0.dirty_regions = Default::default();
        facade.graph.reverse.0.dirty_vertices.clear();

        facade.graph.locator_sidecar = {
            let mut sidecar = EdgeLocatorSidecar::new();
            sidecar.set(
                90,
                crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
            );
            sidecar
        };
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
            crate::RewriteVertexOrdinalMapping {
                vertex_id: src,
                forward_ordinal: 0,
                reverse_ordinal: 0,
            },
            crate::RewriteVertexOrdinalMapping {
                vertex_id: dst,
                forward_ordinal: 0,
                reverse_ordinal: 0,
            },
        ];
        graph.bridge_mut().forward_base_slots_by_ordinal = vec![vec![Some(90), Some(92), Some(93)]];
        graph.bridge_mut().reverse_base_slots_by_ordinal = vec![vec![Some(90), Some(92), Some(93)]];

        let decision = graph
            .bridge()
            .store
            .graph()
            .choose_insert_decision_with_incoming_live_entries(src, 0, dst, 0, 1)
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
            Some(
                &format!(
                    "insert-edge inserted=true path={:?} rebalanced=true displacement=({}, {}) refreshed=(1,1) fwd=[0] rev=[0]",
                    summary.path,
                    summary.total_displacement,
                    summary.max_displacement,
                )
            )
        );
        assert_eq!(
            graph.formatted_write_history().iter().rev().nth(1),
            Some(
                &format!(
                    "ensure-capacity rebalanced=true displacement=({}, {}) refreshed=(1,1) fwd=[0] rev=[0]",
                    ensure.total_displacement,
                    ensure.max_displacement,
                )
            )
        );
        assert_eq!(
            graph.formatted_last_write_event(),
            Some(
                format!(
                    "insert-edge inserted=true path={:?} rebalanced=true displacement=({}, {}) refreshed=(1,1) fwd=[0] rev=[0]",
                    summary.path,
                    summary.total_displacement,
                    summary.max_displacement,
                )
            )
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
            .expand(alice.id, EdgeDirection::PointingRight, Some("LIKES"))
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
            .expand(alice.id, EdgeDirection::PointingRight, Some("LIKES"))
            .expect("expand after delete");
        assert!(expansions.is_empty());
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
        assert!(graph
            .expand(bob.id, EdgeDirection::PointingLeft, Some("KNOWS"))
            .expect("expand after detach delete")
            .is_empty());
        assert_eq!(graph.bridge().edges().get(&edge.id), None);
    }
}
