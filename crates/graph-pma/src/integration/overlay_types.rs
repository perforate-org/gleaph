use gleaph_graph_kernel::{EdgeId, EdgeRecord, NodeRecord};

use crate::facade::{
    RewriteBootstrapEdgeProjection, RewriteBootstrapGraphProjection,
    RewriteBootstrapVerticesProjection, RewriteEdgeLogicalLocatorMapping, RewriteEdgeWriteOperation,
    RewriteEdgeWriteProjection, RewriteEnsureCapacityProjection, RewriteInsertEdgeProjection,
    RewriteNodeDeleteProjection, RewritePropertyMutationWriteSummary, RewriteRefreshedVertices,
    RewriteVertexOrdinalMapping, RewriteWriteEventProjection,
};
use crate::low_level::{EdgeInsertPath, GraphMutationPath, VertexRef};
use crate::observability::{format_last_write_event, format_write_event_history};

/// Observability summary for one overlay-level edge mutation write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteOverlayEdgeWriteSummary {
    pub operation: RewriteOverlayEdgeMutationKind,
    pub path: GraphMutationPath,
    pub refreshed: RewriteRefreshedVertices,
}

impl RewriteOverlayEdgeWriteSummary {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteOverlayInsertEdgeSummary {
    pub inserted: bool,
    pub path: Option<EdgeInsertPath>,
    pub rebalanced: bool,
    pub total_displacement: i64,
    pub max_displacement: i64,
    pub refreshed: RewriteRefreshedVertices,
}

impl RewriteOverlayInsertEdgeSummary {
    pub fn ensure_capacity_projection(&self) -> Option<RewriteEnsureCapacityProjection> {
        self.rebalanced.then(|| RewriteEnsureCapacityProjection {
            rebalanced: true,
            total_displacement: self.total_displacement,
            max_displacement: self.max_displacement,
            refreshed: self.refreshed.clone(),
        })
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewriteOverlayEdgeMutationKind {
    ReplaceLabel,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteOverlayNodeDeleteSummary {
    pub detached: bool,
    pub deleted_edge_ids: Vec<EdgeId>,
    pub edge_writes: Vec<RewriteOverlayEdgeWriteSummary>,
}

impl RewriteOverlayNodeDeleteSummary {
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

#[derive(Clone, Debug, PartialEq)]
pub struct RewriteOverlayNodeBootstrapSummary {
    pub node: NodeRecord,
    pub ordinals: (usize, usize),
    pub refreshed: RewriteRefreshedVertices,
}

impl RewriteOverlayNodeBootstrapSummary {
    fn mapping(&self) -> RewriteVertexOrdinalMapping {
        RewriteVertexOrdinalMapping {
            vertex_ref: VertexRef::from(self.node.id),
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

#[derive(Clone, Debug, PartialEq)]
pub struct RewriteOverlayEdgeBootstrapSummary {
    pub edge: EdgeRecord,
    pub path: EdgeInsertPath,
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

#[derive(Clone, Debug, PartialEq)]
pub struct RewriteOverlayBootstrapGraphSummary {
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
    pub vertex_ordinals: Vec<RewriteVertexOrdinalMapping>,
    pub locators: Vec<RewriteEdgeLogicalLocatorMapping>,
    pub refreshed: RewriteRefreshedVertices,
}

impl RewriteOverlayBootstrapGraphSummary {
    pub fn from_bootstrap_summaries(
        node_summaries: &[RewriteOverlayNodeBootstrapSummary],
        edge_summaries: &[RewriteOverlayEdgeBootstrapSummary],
        locators: Vec<RewriteEdgeLogicalLocatorMapping>,
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

    pub fn projection(&self) -> RewriteBootstrapGraphProjection {
        RewriteBootstrapGraphProjection {
            vertex_ordinals: self.vertex_ordinals.clone(),
            locators: self.locators.clone(),
            refreshed: self.refreshed.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RewriteOverlayWriteEvent {
    BootstrapNode(RewriteOverlayNodeBootstrapSummary),
    BootstrapEdge(RewriteOverlayEdgeBootstrapSummary),
    InsertEdge(RewriteOverlayInsertEdgeSummary),
    BootstrapGraph(RewriteOverlayBootstrapGraphSummary),
    Property(RewritePropertyMutationWriteSummary),
    Edge(RewriteOverlayEdgeWriteSummary),
    NodeDelete(RewriteOverlayNodeDeleteSummary),
}

impl RewriteOverlayWriteEvent {
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

    pub fn node_delete_projection(&self) -> Option<RewriteNodeDeleteProjection> {
        match self {
            Self::NodeDelete(summary) => Some(summary.projection()),
            _ => None,
        }
    }
}

pub trait RewriteKernelOverlayObservability {
    fn last_property_write_summary(&self) -> Option<&RewritePropertyMutationWriteSummary>;
    fn property_write_history(&self) -> &[RewritePropertyMutationWriteSummary];
    fn last_insert_edge_summary(&self) -> Option<&RewriteOverlayInsertEdgeSummary>;
    fn insert_edge_history(&self) -> &[RewriteOverlayInsertEdgeSummary];
    fn last_edge_write_summary(&self) -> Option<&RewriteOverlayEdgeWriteSummary>;
    fn edge_write_history(&self) -> &[RewriteOverlayEdgeWriteSummary];
    fn last_node_delete_summary(&self) -> Option<&RewriteOverlayNodeDeleteSummary>;
    fn node_delete_history(&self) -> &[RewriteOverlayNodeDeleteSummary];
    fn write_history(&self) -> &[RewriteOverlayWriteEvent];

    fn shared_write_history(&self) -> Vec<RewriteWriteEventProjection> {
        self.write_history()
            .iter()
            .flat_map(RewriteOverlayWriteEvent::shared_projections)
            .collect()
    }

    fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(&self.shared_write_history())
    }

    fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(&self.shared_write_history())
    }
}
