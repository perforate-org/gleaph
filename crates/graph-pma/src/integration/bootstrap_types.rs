use gleaph_graph_kernel::{EdgeId, EdgeRecord, LabelId, NodeId, NodeRecord, PropertyMap};

use crate::facade::{
    GraphPmaBootstrapGraphProjection, GraphPmaEdgeLogicalLocatorMapping, GraphPmaRefreshedVertices,
    GraphPmaVertexOrdinalMapping,
};

/// Declarative specification for one initial logical edge during bootstrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootstrapEdgeSpec {
    pub edge_id: EdgeId,
    pub src_index: usize,
    pub dst_index: usize,
    pub label_id: LabelId,
}

impl BootstrapEdgeSpec {
    pub fn new(edge_id: EdgeId, src_index: usize, dst_index: usize, label_id: LabelId) -> Self {
        Self {
            edge_id,
            src_index,
            dst_index,
            label_id,
        }
    }

    pub fn from_tuple(edge: &(EdgeId, usize, usize, LabelId)) -> Self {
        Self::new(edge.0, edge.1, edge.2, edge.3)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapGraphSpec {
    pub vertex_ids: Vec<NodeId>,
    pub initial_edges: Vec<BootstrapEdgeSpec>,
}

impl BootstrapGraphSpec {
    pub fn new(vertex_ids: Vec<NodeId>, initial_edges: Vec<BootstrapEdgeSpec>) -> Self {
        Self {
            vertex_ids,
            initial_edges,
        }
    }

    pub fn from_slices(vertex_ids: &[NodeId], initial_edges: &[BootstrapEdgeSpec]) -> Self {
        Self {
            vertex_ids: vertex_ids.to_vec(),
            initial_edges: initial_edges.to_vec(),
        }
    }

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

    pub fn empty() -> Self {
        Self {
            vertex_ids: Vec::new(),
            initial_edges: Vec::new(),
        }
    }

    pub fn with_vertex(mut self, vertex_id: NodeId) -> Self {
        self.vertex_ids.push(vertex_id);
        self
    }

    pub fn with_edge(mut self, edge: BootstrapEdgeSpec) -> Self {
        self.initial_edges.push(edge);
        self
    }

    pub(crate) fn edge_tuples(&self) -> Vec<(EdgeId, usize, usize, LabelId)> {
        self.initial_edges
            .iter()
            .map(|edge| (edge.edge_id, edge.src_index, edge.dst_index, edge.label_id))
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct KernelBootstrapNodeSpec {
    pub labels: Vec<String>,
    pub properties: PropertyMap,
}

impl KernelBootstrapNodeSpec {
    pub fn new(labels: Vec<String>, properties: PropertyMap) -> Self {
        Self { labels, properties }
    }

    pub fn from_parts<S: AsRef<str>>(labels: &[S], properties: &PropertyMap) -> Self {
        Self {
            labels: labels
                .iter()
                .map(|label| label.as_ref().to_owned())
                .collect(),
            properties: properties.clone(),
        }
    }

    pub fn labeled(label: impl Into<String>, properties: PropertyMap) -> Self {
        Self {
            labels: vec![label.into()],
            properties,
        }
    }

    pub fn labeled_ref(label: &str, properties: &PropertyMap) -> Self {
        Self {
            labels: vec![label.to_owned()],
            properties: properties.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct KernelBootstrapEdgeSpec {
    pub src_index: usize,
    pub dst_index: usize,
    pub label: Option<String>,
    pub properties: PropertyMap,
}

impl KernelBootstrapEdgeSpec {
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

    pub fn unlabeled(src_index: usize, dst_index: usize, properties: &PropertyMap) -> Self {
        Self::from_parts(src_index, dst_index, None, properties)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct KernelBootstrapGraphSpec {
    pub nodes: Vec<KernelBootstrapNodeSpec>,
    pub edges: Vec<KernelBootstrapEdgeSpec>,
}

impl KernelBootstrapGraphSpec {
    pub fn new(nodes: Vec<KernelBootstrapNodeSpec>, edges: Vec<KernelBootstrapEdgeSpec>) -> Self {
        Self { nodes, edges }
    }

    pub fn from_slices(
        nodes: &[KernelBootstrapNodeSpec],
        edges: &[KernelBootstrapEdgeSpec],
    ) -> Self {
        Self {
            nodes: nodes.to_vec(),
            edges: edges.to_vec(),
        }
    }

    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    pub fn with_node(mut self, node: KernelBootstrapNodeSpec) -> Self {
        self.nodes.push(node);
        self
    }

    pub fn with_edge(mut self, edge: KernelBootstrapEdgeSpec) -> Self {
        self.edges.push(edge);
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct KernelBootstrapGraphSummary {
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
    pub vertex_ordinals: Vec<GraphPmaVertexOrdinalMapping>,
    pub locators: Vec<GraphPmaEdgeLogicalLocatorMapping>,
    pub refreshed: GraphPmaRefreshedVertices,
}

impl KernelBootstrapGraphSummary {
    pub fn projection(&self) -> GraphPmaBootstrapGraphProjection {
        GraphPmaBootstrapGraphProjection {
            vertex_ordinals: self.vertex_ordinals.clone(),
            locators: self.locators.clone(),
            refreshed: self.refreshed.clone(),
        }
    }
}
