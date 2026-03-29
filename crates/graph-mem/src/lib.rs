use std::collections::BTreeMap;

use gleaph_gql::ast::CmpOp;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql::Value;
use gleaph_graph_kernel::{
    EdgeRecord, Expansion, GraphRead, GraphResult, GraphWrite, NodeId, NodeRecord, PropertyMap,
};

#[derive(Clone, Debug, Default)]
pub struct InMemoryGraph {
    next_node_id: u64,
    next_edge_id: u64,
    nodes: BTreeMap<NodeId, NodeRecord>,
    edges: BTreeMap<u64, EdgeRecord>,
}

impl InMemoryGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_node<I, K>(&mut self, labels: I, properties: impl IntoIterator<Item = (K, Value)>) -> NodeId
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
        K: Into<String>,
    {
        self.next_node_id += 1;
        let id = NodeId::try_from(self.next_node_id).expect("in-memory node id overflow");
        let record = NodeRecord {
            id,
            labels: labels.into_iter().map(|label| label.as_ref().to_owned()).collect(),
            properties: collect_properties(properties),
        };
        self.nodes.insert(id, record);
        id
    }

    pub fn insert_edge<K>(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: impl IntoIterator<Item = (K, Value)>,
    ) -> u64
    where
        K: Into<String>,
    {
        self.next_edge_id += 1;
        let id = self.next_edge_id;
        let edge = EdgeRecord {
            id,
            src,
            dst,
            label: label.map(str::to_owned),
            properties: collect_properties(properties),
        };
        self.edges.insert(id, edge);
        id
    }
}

impl GraphRead for InMemoryGraph {
    fn scan_nodes(&self, label: Option<&str>) -> GraphResult<Vec<NodeRecord>> {
        Ok(self
            .nodes
            .values()
            .filter(|node| label.is_none_or(|label| node.labels.iter().any(|it| it == label)))
            .cloned()
            .collect())
    }

    fn scan_nodes_by_property(
        &self,
        property: &str,
        value: &Value,
        cmp: CmpOp,
    ) -> GraphResult<Vec<NodeRecord>> {
        Ok(self
            .nodes
            .values()
            .filter(|node| {
                node.properties
                    .get(property)
                    .is_some_and(|candidate| compare_op(compare_values(candidate, value), cmp))
            })
            .cloned()
            .collect())
    }

    fn scan_edges_by_property(
        &self,
        property: &str,
        value: &Value,
    ) -> GraphResult<Vec<EdgeRecord>> {
        Ok(self
            .edges
            .values()
            .filter(|edge| edge.properties.get(property) == Some(value))
            .cloned()
            .collect())
    }

    fn expand(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        label: Option<&str>,
    ) -> GraphResult<Vec<Expansion>> {
        let mut out = Vec::new();
        for edge in self.edges.values() {
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
            if let Some(node) = self.nodes.get(&target) {
                out.push(Expansion {
                    edge: edge.clone(),
                    node: node.clone(),
                });
            }
        }
        Ok(out)
    }

    fn get_node(&self, id: NodeId) -> GraphResult<Option<NodeRecord>> {
        Ok(self.nodes.get(&id).cloned())
    }
}

impl GraphWrite for InMemoryGraph {
    fn insert_node(
        &mut self,
        labels: &[String],
        properties: &PropertyMap,
    ) -> GraphResult<NodeRecord> {
        let id = self.insert_node(labels.iter().map(String::as_str), properties.clone());
        Ok(self.nodes.get(&id).cloned().expect("inserted node exists"))
    }

    fn insert_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        let id = self.insert_edge(src, dst, label, properties.clone());
        Ok(self.edges.get(&id).cloned().expect("inserted edge exists"))
    }

    fn set_node_property(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<NodeRecord> {
        let node = self
            .nodes
            .get_mut(&node_id)
            .ok_or(gleaph_graph_kernel::GraphError::NodeNotFound(node_id))?;
        node.properties.insert(property.to_owned(), value.clone());
        Ok(node.clone())
    }

    fn remove_node_property(&mut self, node_id: NodeId, property: &str) -> GraphResult<NodeRecord> {
        let node = self
            .nodes
            .get_mut(&node_id)
            .ok_or(gleaph_graph_kernel::GraphError::NodeNotFound(node_id))?;
        node.properties.remove(property);
        Ok(node.clone())
    }

    fn add_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord> {
        let node = self
            .nodes
            .get_mut(&node_id)
            .ok_or(gleaph_graph_kernel::GraphError::NodeNotFound(node_id))?;
        if !node.labels.iter().any(|existing| existing == label) {
            node.labels.push(label.to_owned());
        }
        Ok(node.clone())
    }

    fn remove_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord> {
        let node = self
            .nodes
            .get_mut(&node_id)
            .ok_or(gleaph_graph_kernel::GraphError::NodeNotFound(node_id))?;
        node.labels.retain(|existing| existing != label);
        Ok(node.clone())
    }

    fn set_edge_property(
        &mut self,
        edge_id: gleaph_graph_kernel::EdgeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<EdgeRecord> {
        let edge = self
            .edges
            .get_mut(&edge_id)
            .ok_or(gleaph_graph_kernel::GraphError::EdgeNotFound(edge_id))?;
        edge.properties.insert(property.to_owned(), value.clone());
        Ok(edge.clone())
    }

    fn remove_edge_property(
        &mut self,
        edge_id: gleaph_graph_kernel::EdgeId,
        property: &str,
    ) -> GraphResult<EdgeRecord> {
        let edge = self
            .edges
            .get_mut(&edge_id)
            .ok_or(gleaph_graph_kernel::GraphError::EdgeNotFound(edge_id))?;
        edge.properties.remove(property);
        Ok(edge.clone())
    }

    fn set_edge_label(
        &mut self,
        edge_id: gleaph_graph_kernel::EdgeId,
        label: Option<&str>,
    ) -> GraphResult<EdgeRecord> {
        let edge = self
            .edges
            .get_mut(&edge_id)
            .ok_or(gleaph_graph_kernel::GraphError::EdgeNotFound(edge_id))?;
        edge.label = label.map(str::to_owned);
        Ok(edge.clone())
    }

    fn delete_edge(&mut self, edge_id: gleaph_graph_kernel::EdgeId) -> GraphResult<()> {
        self.edges
            .remove(&edge_id)
            .ok_or(gleaph_graph_kernel::GraphError::EdgeNotFound(edge_id))?;
        Ok(())
    }

    fn delete_node(&mut self, node_id: NodeId, detach: bool) -> GraphResult<()> {
        let has_incident = self
            .edges
            .values()
            .any(|edge| edge.src == node_id || edge.dst == node_id);
        if has_incident && !detach {
            return Err(gleaph_graph_kernel::GraphError::Message(
                "node has incident edges".to_owned(),
            ));
        }
        if detach {
            self.edges
                .retain(|_, edge| edge.src != node_id && edge.dst != node_id);
        }
        self.nodes
            .remove(&node_id)
            .ok_or(gleaph_graph_kernel::GraphError::NodeNotFound(node_id))?;
        Ok(())
    }
}

fn collect_properties<K>(
    properties: impl IntoIterator<Item = (K, Value)>,
) -> PropertyMap
where
    K: Into<String>,
{
    properties
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect()
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
