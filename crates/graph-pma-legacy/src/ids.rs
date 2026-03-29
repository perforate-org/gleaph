use gleaph_graph_kernel::{EdgeId, NodeId};

#[derive(Clone, Debug, Default)]
pub struct IdAllocators {
    next_node_id: NodeId,
    next_edge_id: EdgeId,
}

impl IdAllocators {
    pub fn with_counts(node_count: u64, edge_count: u64) -> Self {
        Self {
            next_node_id: NodeId::try_from(node_count).expect("node count exceeds NodeId range"),
            next_edge_id: edge_count,
        }
    }

    pub fn next_node_id(&mut self) -> NodeId {
        self.next_node_id += 1;
        self.next_node_id
    }

    pub fn next_edge_id(&mut self) -> EdgeId {
        self.next_edge_id += 1;
        self.next_edge_id
    }

    pub fn node_count(&self) -> u64 {
        self.next_node_id.into()
    }

    pub fn edge_count(&self) -> u64 {
        self.next_edge_id
    }
}
