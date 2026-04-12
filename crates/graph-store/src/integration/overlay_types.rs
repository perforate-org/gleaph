use candid::Principal;
use gleaph_graph_kernel::{EdgeId, EdgeRecord, Expansion, NodeRecord};

use crate::facade::{
    GraphStoreBootstrapEdgeProjection, GraphStoreBootstrapGraphProjection,
    GraphStoreBootstrapVerticesProjection, GraphStoreEdgeLogicalLocatorMapping,
    GraphStoreEdgeWriteOperation, GraphStoreEdgeWriteProjection, GraphStoreEnsureCapacityProjection,
    GraphStoreInsertEdgeProjection, GraphStoreNodeDeleteProjection,
    GraphStorePropertyMutationWriteSummary, GraphStoreRefreshedVertices, GraphStoreVertexOrdinalMapping,
    GraphStoreWriteEventProjection,
};
use crate::low_level::{EdgeInsertPath, GraphMutationPath, VertexRef};
use crate::observability::{format_last_write_event, format_write_event_history};

/// One expand hop plus optional remote shard principal from forward-side [`EdgeMeta`](crate::low_level::edge::EdgeMeta).
#[derive(Clone, Debug, PartialEq)]
pub struct ExpansionWithShard {
    pub expansion: Expansion,
    pub shard_canister_dst: Option<Principal>,
}

/// Observability summary for one overlay-level edge mutation write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreOverlayEdgeWriteSummary {
    pub operation: GraphStoreOverlayEdgeMutationKind,
    pub path: GraphMutationPath,
    pub refreshed: GraphStoreRefreshedVertices,
}

impl GraphStoreOverlayEdgeWriteSummary {
    pub fn projection(&self) -> GraphStoreEdgeWriteProjection {
        GraphStoreEdgeWriteProjection {
            operation: match self.operation {
                GraphStoreOverlayEdgeMutationKind::ReplaceLabel => {
                    GraphStoreEdgeWriteOperation::ReplaceLabel
                }
                GraphStoreOverlayEdgeMutationKind::Delete => GraphStoreEdgeWriteOperation::Delete,
            },
            path: self.path,
            refreshed: self.refreshed.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreOverlayInsertEdgeSummary {
    pub inserted: bool,
    pub path: Option<EdgeInsertPath>,
    pub rebalanced: bool,
    pub total_displacement: i64,
    pub max_displacement: i64,
    pub refreshed: GraphStoreRefreshedVertices,
}

impl GraphStoreOverlayInsertEdgeSummary {
    pub fn ensure_capacity_projection(&self) -> Option<GraphStoreEnsureCapacityProjection> {
        self.rebalanced.then(|| GraphStoreEnsureCapacityProjection {
            rebalanced: true,
            total_displacement: self.total_displacement,
            max_displacement: self.max_displacement,
            refreshed: self.refreshed.clone(),
        })
    }

    pub fn projection(&self) -> GraphStoreInsertEdgeProjection {
        GraphStoreInsertEdgeProjection {
            inserted: self.inserted,
            path: self.path,
            rebalanced: self.rebalanced,
            total_displacement: self.total_displacement,
            max_displacement: self.max_displacement,
            refreshed: self.refreshed.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphStoreOverlayEdgeMutationKind {
    ReplaceLabel,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphStoreOverlayNodeDeleteSummary {
    pub detached: bool,
    pub deleted_edge_ids: Vec<EdgeId>,
    pub edge_writes: Vec<GraphStoreOverlayEdgeWriteSummary>,
}

impl GraphStoreOverlayNodeDeleteSummary {
    pub fn projection(&self) -> GraphStoreNodeDeleteProjection {
        GraphStoreNodeDeleteProjection {
            detached: self.detached,
            deleted_edge_ids: self.deleted_edge_ids.clone(),
            edge_writes: self
                .edge_writes
                .iter()
                .map(GraphStoreOverlayEdgeWriteSummary::projection)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphStoreOverlayNodeBootstrapSummary {
    pub node: NodeRecord,
    pub ordinals: (usize, usize),
    pub refreshed: GraphStoreRefreshedVertices,
}

impl GraphStoreOverlayNodeBootstrapSummary {
    fn mapping(&self) -> GraphStoreVertexOrdinalMapping {
        GraphStoreVertexOrdinalMapping {
            vertex_ref: VertexRef::from(self.node.id),
            forward_ordinal: self.ordinals.0,
            reverse_ordinal: self.ordinals.1,
        }
    }

    fn projection(&self) -> GraphStoreBootstrapVerticesProjection {
        GraphStoreBootstrapVerticesProjection {
            ordinals: vec![self.ordinals],
            refreshed: self.refreshed.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphStoreOverlayEdgeBootstrapSummary {
    pub edge: EdgeRecord,
    pub path: EdgeInsertPath,
    pub refreshed: GraphStoreRefreshedVertices,
}

impl GraphStoreOverlayEdgeBootstrapSummary {
    fn projection(&self) -> GraphStoreBootstrapEdgeProjection {
        GraphStoreBootstrapEdgeProjection {
            path: Some(self.path),
            refreshed: self.refreshed.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphStoreOverlayBootstrapGraphSummary {
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
    pub vertex_ordinals: Vec<GraphStoreVertexOrdinalMapping>,
    pub locators: Vec<GraphStoreEdgeLogicalLocatorMapping>,
    pub refreshed: GraphStoreRefreshedVertices,
}

impl GraphStoreOverlayBootstrapGraphSummary {
    pub fn from_bootstrap_summaries(
        node_summaries: &[GraphStoreOverlayNodeBootstrapSummary],
        edge_summaries: &[GraphStoreOverlayEdgeBootstrapSummary],
        locators: Vec<GraphStoreEdgeLogicalLocatorMapping>,
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
            refreshed: GraphStoreRefreshedVertices::new(refreshed_forward, refreshed_reverse),
        }
    }

    pub fn projection(&self) -> GraphStoreBootstrapGraphProjection {
        GraphStoreBootstrapGraphProjection {
            vertex_ordinals: self.vertex_ordinals.clone(),
            locators: self.locators.clone(),
            refreshed: self.refreshed.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum GraphStoreOverlayWriteEvent {
    BootstrapNode(GraphStoreOverlayNodeBootstrapSummary),
    BootstrapEdge(GraphStoreOverlayEdgeBootstrapSummary),
    InsertEdge(GraphStoreOverlayInsertEdgeSummary),
    BootstrapGraph(GraphStoreOverlayBootstrapGraphSummary),
    Property(GraphStorePropertyMutationWriteSummary),
    Edge(GraphStoreOverlayEdgeWriteSummary),
    NodeDelete(GraphStoreOverlayNodeDeleteSummary),
}

impl GraphStoreOverlayWriteEvent {
    pub fn shared_projections(&self) -> Vec<GraphStoreWriteEventProjection> {
        match self {
            Self::InsertEdge(summary) => {
                let mut projections = Vec::with_capacity(2);
                if let Some(ensure) = summary.ensure_capacity_projection() {
                    projections.push(GraphStoreWriteEventProjection::EnsureCapacity(ensure));
                }
                projections.push(GraphStoreWriteEventProjection::InsertEdge(
                    summary.projection(),
                ));
                projections
            }
            _ => self.shared_projection().into_iter().collect(),
        }
    }

    pub fn shared_projection(&self) -> Option<GraphStoreWriteEventProjection> {
        match self {
            Self::BootstrapNode(summary) => Some(GraphStoreWriteEventProjection::BootstrapVertices(
                summary.projection(),
            )),
            Self::BootstrapEdge(summary) => Some(GraphStoreWriteEventProjection::BootstrapEdge(
                summary.projection(),
            )),
            Self::InsertEdge(summary) => Some(GraphStoreWriteEventProjection::InsertEdge(
                summary.projection(),
            )),
            Self::BootstrapGraph(summary) => Some(GraphStoreWriteEventProjection::BootstrapGraph(
                summary.projection(),
            )),
            Self::Property(summary) => {
                Some(GraphStoreWriteEventProjection::Property(summary.projection()))
            }
            Self::Edge(summary) => Some(GraphStoreWriteEventProjection::Edge(summary.projection())),
            Self::NodeDelete(summary) => Some(GraphStoreWriteEventProjection::NodeDelete(
                summary.projection(),
            )),
        }
    }

    pub fn node_delete_projection(&self) -> Option<GraphStoreNodeDeleteProjection> {
        match self {
            Self::NodeDelete(summary) => Some(summary.projection()),
            _ => None,
        }
    }
}

pub trait GraphStoreKernelOverlayObservability {
    fn last_property_write_summary(&self) -> Option<&GraphStorePropertyMutationWriteSummary>;
    fn property_write_history(&self) -> &[GraphStorePropertyMutationWriteSummary];
    fn last_insert_edge_summary(&self) -> Option<&GraphStoreOverlayInsertEdgeSummary>;
    fn insert_edge_history(&self) -> &[GraphStoreOverlayInsertEdgeSummary];
    fn last_edge_write_summary(&self) -> Option<&GraphStoreOverlayEdgeWriteSummary>;
    fn edge_write_history(&self) -> &[GraphStoreOverlayEdgeWriteSummary];
    fn last_node_delete_summary(&self) -> Option<&GraphStoreOverlayNodeDeleteSummary>;
    fn node_delete_history(&self) -> &[GraphStoreOverlayNodeDeleteSummary];
    fn write_history(&self) -> &[GraphStoreOverlayWriteEvent];

    fn shared_write_history(&self) -> Vec<GraphStoreWriteEventProjection> {
        self.write_history()
            .iter()
            .flat_map(GraphStoreOverlayWriteEvent::shared_projections)
            .collect()
    }

    fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(&self.shared_write_history())
    }

    fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(&self.shared_write_history())
    }
}
