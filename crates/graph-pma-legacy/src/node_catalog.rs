use std::collections::BTreeMap;

use gleaph_graph_kernel::{NodeId, NodeRecord, PropertyMap};

#[derive(Clone, Debug, Default)]
pub struct NodeCatalog {
    nodes: BTreeMap<NodeId, NodeRecord>,
}

impl NodeCatalog {
    pub fn insert(&mut self, id: NodeId, labels: Vec<String>, properties: PropertyMap) -> NodeId {
        self.nodes.insert(
            id,
            NodeRecord {
                id,
                labels,
                properties,
            },
        );
        id
    }

    pub fn get(&self, id: NodeId) -> Option<&NodeRecord> {
        self.nodes.get(&id)
    }

    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut NodeRecord> {
        self.nodes.get_mut(&id)
    }

    pub fn scan_by_label(&self, label: Option<&str>) -> Vec<NodeRecord> {
        self.nodes
            .values()
            .filter(|node| label.is_none_or(|label| node.labels.iter().any(|it| it == label)))
            .cloned()
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &NodeRecord> {
        self.nodes.values()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn from_records(records: Vec<NodeRecord>) -> Self {
        let mut nodes = BTreeMap::new();
        for record in records {
            nodes.insert(record.id, record);
        }
        Self { nodes }
    }

    pub fn remove(&mut self, id: NodeId) -> Option<NodeRecord> {
        self.nodes.remove(&id)
    }
}
