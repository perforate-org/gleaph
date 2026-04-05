use candid::Principal;
use gleaph_graph_kernel::{EdgeId, EdgeRecord, Expansion, NodeRecord};

use crate::facade::{
    GraphPmaBootstrapEdgeProjection, GraphPmaBootstrapGraphProjection,
    GraphPmaBootstrapVerticesProjection, GraphPmaEdgeLogicalLocatorMapping,
    GraphPmaEdgeWriteOperation, GraphPmaEdgeWriteProjection, GraphPmaEnsureCapacityProjection,
    GraphPmaInsertEdgeProjection, GraphPmaNodeDeleteProjection,
    GraphPmaPropertyMutationWriteSummary, GraphPmaRefreshedVertices, GraphPmaVertexOrdinalMapping,
    GraphPmaWriteEventProjection,
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
pub struct GraphPmaOverlayEdgeWriteSummary {
    pub operation: GraphPmaOverlayEdgeMutationKind,
    pub path: GraphMutationPath,
    pub refreshed: GraphPmaRefreshedVertices,
}

impl GraphPmaOverlayEdgeWriteSummary {
    pub fn projection(&self) -> GraphPmaEdgeWriteProjection {
        GraphPmaEdgeWriteProjection {
            operation: match self.operation {
                GraphPmaOverlayEdgeMutationKind::ReplaceLabel => {
                    GraphPmaEdgeWriteOperation::ReplaceLabel
                }
                GraphPmaOverlayEdgeMutationKind::Delete => GraphPmaEdgeWriteOperation::Delete,
            },
            path: self.path,
            refreshed: self.refreshed.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaOverlayInsertEdgeSummary {
    pub inserted: bool,
    pub path: Option<EdgeInsertPath>,
    pub rebalanced: bool,
    pub total_displacement: i64,
    pub max_displacement: i64,
    pub refreshed: GraphPmaRefreshedVertices,
}

impl GraphPmaOverlayInsertEdgeSummary {
    pub fn ensure_capacity_projection(&self) -> Option<GraphPmaEnsureCapacityProjection> {
        self.rebalanced.then(|| GraphPmaEnsureCapacityProjection {
            rebalanced: true,
            total_displacement: self.total_displacement,
            max_displacement: self.max_displacement,
            refreshed: self.refreshed.clone(),
        })
    }

    pub fn projection(&self) -> GraphPmaInsertEdgeProjection {
        GraphPmaInsertEdgeProjection {
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
pub enum GraphPmaOverlayEdgeMutationKind {
    ReplaceLabel,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPmaOverlayNodeDeleteSummary {
    pub detached: bool,
    pub deleted_edge_ids: Vec<EdgeId>,
    pub edge_writes: Vec<GraphPmaOverlayEdgeWriteSummary>,
}

impl GraphPmaOverlayNodeDeleteSummary {
    pub fn projection(&self) -> GraphPmaNodeDeleteProjection {
        GraphPmaNodeDeleteProjection {
            detached: self.detached,
            deleted_edge_ids: self.deleted_edge_ids.clone(),
            edge_writes: self
                .edge_writes
                .iter()
                .map(GraphPmaOverlayEdgeWriteSummary::projection)
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphPmaOverlayNodeBootstrapSummary {
    pub node: NodeRecord,
    pub ordinals: (usize, usize),
    pub refreshed: GraphPmaRefreshedVertices,
}

impl GraphPmaOverlayNodeBootstrapSummary {
    fn mapping(&self) -> GraphPmaVertexOrdinalMapping {
        GraphPmaVertexOrdinalMapping {
            vertex_ref: VertexRef::from(self.node.id),
            forward_ordinal: self.ordinals.0,
            reverse_ordinal: self.ordinals.1,
        }
    }

    fn projection(&self) -> GraphPmaBootstrapVerticesProjection {
        GraphPmaBootstrapVerticesProjection {
            ordinals: vec![self.ordinals],
            refreshed: self.refreshed.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphPmaOverlayEdgeBootstrapSummary {
    pub edge: EdgeRecord,
    pub path: EdgeInsertPath,
    pub refreshed: GraphPmaRefreshedVertices,
}

impl GraphPmaOverlayEdgeBootstrapSummary {
    fn projection(&self) -> GraphPmaBootstrapEdgeProjection {
        GraphPmaBootstrapEdgeProjection {
            path: Some(self.path),
            refreshed: self.refreshed.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphPmaOverlayBootstrapGraphSummary {
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
    pub vertex_ordinals: Vec<GraphPmaVertexOrdinalMapping>,
    pub locators: Vec<GraphPmaEdgeLogicalLocatorMapping>,
    pub refreshed: GraphPmaRefreshedVertices,
}

impl GraphPmaOverlayBootstrapGraphSummary {
    pub fn from_bootstrap_summaries(
        node_summaries: &[GraphPmaOverlayNodeBootstrapSummary],
        edge_summaries: &[GraphPmaOverlayEdgeBootstrapSummary],
        locators: Vec<GraphPmaEdgeLogicalLocatorMapping>,
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
            refreshed: GraphPmaRefreshedVertices::new(refreshed_forward, refreshed_reverse),
        }
    }

    pub fn projection(&self) -> GraphPmaBootstrapGraphProjection {
        GraphPmaBootstrapGraphProjection {
            vertex_ordinals: self.vertex_ordinals.clone(),
            locators: self.locators.clone(),
            refreshed: self.refreshed.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum GraphPmaOverlayWriteEvent {
    BootstrapNode(GraphPmaOverlayNodeBootstrapSummary),
    BootstrapEdge(GraphPmaOverlayEdgeBootstrapSummary),
    InsertEdge(GraphPmaOverlayInsertEdgeSummary),
    BootstrapGraph(GraphPmaOverlayBootstrapGraphSummary),
    Property(GraphPmaPropertyMutationWriteSummary),
    Edge(GraphPmaOverlayEdgeWriteSummary),
    NodeDelete(GraphPmaOverlayNodeDeleteSummary),
}

impl GraphPmaOverlayWriteEvent {
    pub fn shared_projections(&self) -> Vec<GraphPmaWriteEventProjection> {
        match self {
            Self::InsertEdge(summary) => {
                let mut projections = Vec::with_capacity(2);
                if let Some(ensure) = summary.ensure_capacity_projection() {
                    projections.push(GraphPmaWriteEventProjection::EnsureCapacity(ensure));
                }
                projections.push(GraphPmaWriteEventProjection::InsertEdge(
                    summary.projection(),
                ));
                projections
            }
            _ => self.shared_projection().into_iter().collect(),
        }
    }

    pub fn shared_projection(&self) -> Option<GraphPmaWriteEventProjection> {
        match self {
            Self::BootstrapNode(summary) => Some(GraphPmaWriteEventProjection::BootstrapVertices(
                summary.projection(),
            )),
            Self::BootstrapEdge(summary) => Some(GraphPmaWriteEventProjection::BootstrapEdge(
                summary.projection(),
            )),
            Self::InsertEdge(summary) => Some(GraphPmaWriteEventProjection::InsertEdge(
                summary.projection(),
            )),
            Self::BootstrapGraph(summary) => Some(GraphPmaWriteEventProjection::BootstrapGraph(
                summary.projection(),
            )),
            Self::Property(summary) => {
                Some(GraphPmaWriteEventProjection::Property(summary.projection()))
            }
            Self::Edge(summary) => Some(GraphPmaWriteEventProjection::Edge(summary.projection())),
            Self::NodeDelete(summary) => Some(GraphPmaWriteEventProjection::NodeDelete(
                summary.projection(),
            )),
        }
    }

    pub fn node_delete_projection(&self) -> Option<GraphPmaNodeDeleteProjection> {
        match self {
            Self::NodeDelete(summary) => Some(summary.projection()),
            _ => None,
        }
    }
}

pub trait GraphPmaKernelOverlayObservability {
    fn last_property_write_summary(&self) -> Option<&GraphPmaPropertyMutationWriteSummary>;
    fn property_write_history(&self) -> &[GraphPmaPropertyMutationWriteSummary];
    fn last_insert_edge_summary(&self) -> Option<&GraphPmaOverlayInsertEdgeSummary>;
    fn insert_edge_history(&self) -> &[GraphPmaOverlayInsertEdgeSummary];
    fn last_edge_write_summary(&self) -> Option<&GraphPmaOverlayEdgeWriteSummary>;
    fn edge_write_history(&self) -> &[GraphPmaOverlayEdgeWriteSummary];
    fn last_node_delete_summary(&self) -> Option<&GraphPmaOverlayNodeDeleteSummary>;
    fn node_delete_history(&self) -> &[GraphPmaOverlayNodeDeleteSummary];
    fn write_history(&self) -> &[GraphPmaOverlayWriteEvent];

    fn shared_write_history(&self) -> Vec<GraphPmaWriteEventProjection> {
        self.write_history()
            .iter()
            .flat_map(GraphPmaOverlayWriteEvent::shared_projections)
            .collect()
    }

    fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(&self.shared_write_history())
    }

    fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(&self.shared_write_history())
    }
}
