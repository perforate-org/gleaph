use std::collections::BTreeMap;

use gleaph_gql::ast::CmpOp;
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql::Value;
use gleaph_graph_kernel::{EdgeId, EdgeRecord, NodeId, NodeRecord};

use crate::abtree::AbTree;
use crate::layout::{LayoutError, LayoutResult};
use crate::prop_codec::{decode_u64_list, encode_u64_list, read_u32, write_u32};
use crate::prop_key::encode_property_index_key;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyIndexBackendKind {
    AppendLog,
    AbTree,
}

#[derive(Clone, Debug, Default)]
struct AppendLogPropertyIndex {
    node_eq: BTreeMap<(String, IndexValue), Vec<NodeId>>,
    edge_eq: BTreeMap<(String, IndexValue), Vec<EdgeId>>,
}

#[derive(Clone, Debug, Default)]
struct AbTreePropertyIndex {
    node_eq: AbTree,
    edge_eq: AbTree,
}

#[derive(Clone, Debug)]
enum PropertyIndexBackend {
    AppendLog(AppendLogPropertyIndex),
    AbTree(AbTreePropertyIndex),
}

impl Default for PropertyIndexBackend {
    fn default() -> Self {
        Self::AppendLog(AppendLogPropertyIndex::default())
    }
}

#[derive(Clone, Debug, Default)]
pub struct PropertyIndexRuntime {
    backend: PropertyIndexBackend,
}

impl PropertyIndexRuntime {
    pub fn with_backend(kind: PropertyIndexBackendKind) -> Self {
        let backend = match kind {
            PropertyIndexBackendKind::AppendLog => {
                PropertyIndexBackend::AppendLog(AppendLogPropertyIndex::default())
            }
            PropertyIndexBackendKind::AbTree => {
                PropertyIndexBackend::AbTree(AbTreePropertyIndex::default())
            }
        };
        Self { backend }
    }

    pub fn backend_kind(&self) -> PropertyIndexBackendKind {
        match self.backend {
            PropertyIndexBackend::AppendLog(_) => PropertyIndexBackendKind::AppendLog,
            PropertyIndexBackend::AbTree(_) => PropertyIndexBackendKind::AbTree,
        }
    }

    pub fn migrate_to_abtree(&mut self) {
        if matches!(self.backend, PropertyIndexBackend::AbTree(_)) {
            return;
        }
        let (node_eq, edge_eq) = match &self.backend {
            PropertyIndexBackend::AppendLog(index) => {
                (abtree_from_node_eq(&index.node_eq), abtree_from_edge_eq(&index.edge_eq))
            }
            PropertyIndexBackend::AbTree(index) => (index.node_eq.clone(), index.edge_eq.clone()),
        };
        self.backend = PropertyIndexBackend::AbTree(AbTreePropertyIndex { node_eq, edge_eq });
    }

    pub fn snapshot_bytes(&self) -> Option<Vec<u8>> {
        let PropertyIndexBackend::AbTree(index) = &self.backend else {
            return None;
        };
        let mut out = Vec::new();
        let node_bytes = index.node_eq.to_bytes();
        let edge_bytes = index.edge_eq.to_bytes();
        write_u32(&mut out, node_bytes.len() as u32);
        out.extend_from_slice(&node_bytes);
        write_u32(&mut out, edge_bytes.len() as u32);
        out.extend_from_slice(&edge_bytes);
        Some(out)
    }

    pub fn from_snapshot(bytes: &[u8]) -> LayoutResult<Self> {
        let mut cursor = 0;
        let node_len = read_u32(bytes, &mut cursor)? as usize;
        if cursor + node_len > bytes.len() {
            return Err(LayoutError::UnexpectedEof);
        }
        let node_eq = AbTree::from_bytes(&bytes[cursor..cursor + node_len])?;
        cursor += node_len;
        let edge_len = read_u32(bytes, &mut cursor)? as usize;
        if cursor + edge_len > bytes.len() {
            return Err(LayoutError::UnexpectedEof);
        }
        let edge_eq = AbTree::from_bytes(&bytes[cursor..cursor + edge_len])?;
        Ok(Self {
            backend: PropertyIndexBackend::AbTree(AbTreePropertyIndex { node_eq, edge_eq }),
        })
    }

    pub fn build_from_nodes<'a>(nodes: impl Iterator<Item = &'a NodeRecord>) -> Self {
        let mut runtime = Self::default();
        for node in nodes {
            runtime.index_node(node);
        }
        runtime
    }

    pub fn build_from_graph<'a>(
        nodes: impl Iterator<Item = &'a NodeRecord>,
        edges: impl Iterator<Item = &'a EdgeRecord>,
    ) -> Self {
        let mut runtime = Self::default();
        for node in nodes {
            runtime.index_node(node);
        }
        for edge in edges {
            runtime.index_edge(edge);
        }
        runtime
    }

    pub fn index_node(&mut self, node: &NodeRecord) {
        for (property, value) in &node.properties {
            self.insert_node_property(node.id, property, value);
        }
    }

    pub fn index_edge(&mut self, edge: &EdgeRecord) {
        for (property, value) in &edge.properties {
            self.insert_edge_property(edge.id, property, value);
        }
    }

    pub fn deindex_node(&mut self, node: &NodeRecord) {
        for (property, value) in &node.properties {
            self.remove_node_property(node.id, property, value);
        }
    }

    pub fn deindex_edge(&mut self, edge: &EdgeRecord) {
        for (property, value) in &edge.properties {
            self.remove_edge_property(edge.id, property, value);
        }
    }

    pub fn update_node_property(
        &mut self,
        node_id: NodeId,
        property: &str,
        old_value: Option<&Value>,
        new_value: Option<&Value>,
    ) {
        if let Some(old_value) = old_value {
            self.remove_node_property(node_id, property, old_value);
        }
        if let Some(new_value) = new_value {
            self.insert_node_property(node_id, property, new_value);
        }
    }

    pub fn update_edge_property(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        old_value: Option<&Value>,
        new_value: Option<&Value>,
    ) {
        if let Some(old_value) = old_value {
            self.remove_edge_property(edge_id, property, old_value);
        }
        if let Some(new_value) = new_value {
            self.insert_edge_property(edge_id, property, new_value);
        }
    }

    pub fn scan_nodes<'a>(
        &self,
        nodes: impl Iterator<Item = &'a NodeRecord>,
        property: &str,
        value: &Value,
        cmp: CmpOp,
    ) -> Vec<NodeRecord> {
        if cmp == CmpOp::Eq
            && let Some(index_value) = IndexValue::from_gql_value(value)
        {
            let ids = self.lookup_node_ids(property, &index_value);
            let node_by_id: BTreeMap<NodeId, NodeRecord> =
                nodes.map(|node| (node.id, node.clone())).collect();
            return ids
                .into_iter()
                .filter_map(|id| node_by_id.get(&id).cloned())
                .collect();
        }

        nodes
            .filter(|node| {
                node.properties
                    .get(property)
                    .is_some_and(|candidate| compare_op(compare_values(candidate, value), cmp))
            })
            .cloned()
            .collect()
    }

    pub fn scan_edges<'a>(
        &self,
        edges: impl Iterator<Item = &'a EdgeRecord>,
        property: &str,
        value: &Value,
    ) -> Vec<EdgeRecord> {
        if let Some(index_value) = IndexValue::from_gql_value(value) {
            let ids = self.lookup_edge_ids(property, &index_value);
            let edge_by_id: BTreeMap<EdgeId, EdgeRecord> =
                edges.map(|edge| (edge.id, edge.clone())).collect();
            return ids
                .into_iter()
                .filter_map(|id| edge_by_id.get(&id).cloned())
                .collect();
        }

        edges
            .filter(|edge| edge.properties.get(property) == Some(value))
            .cloned()
            .collect()
    }

    fn node_eq_mut(&mut self) -> &mut BTreeMap<(String, IndexValue), Vec<NodeId>> {
        match &mut self.backend {
            PropertyIndexBackend::AppendLog(index) => &mut index.node_eq,
            PropertyIndexBackend::AbTree(_) => unreachable!("abtree backend does not expose tuple map"),
        }
    }

    fn edge_eq_mut(&mut self) -> &mut BTreeMap<(String, IndexValue), Vec<EdgeId>> {
        match &mut self.backend {
            PropertyIndexBackend::AppendLog(index) => &mut index.edge_eq,
            PropertyIndexBackend::AbTree(_) => unreachable!("abtree backend does not expose tuple map"),
        }
    }

    fn insert_node_property(&mut self, node_id: NodeId, property: &str, value: &Value) {
        if let Some(value) = IndexValue::from_gql_value(value) {
            match &mut self.backend {
                PropertyIndexBackend::AppendLog(_) => {
                    let ids = self.node_eq_mut().entry((property.to_owned(), value)).or_default();
                    if !ids.contains(&node_id) {
                        ids.push(node_id);
                    }
                }
                PropertyIndexBackend::AbTree(index) => {
                    let key = encode_index_key(property, &value);
                    let mut ids = decode_node_ids(index.node_eq.get(&key).unwrap_or(&[]));
                    if !ids.contains(&node_id) {
                        ids.push(node_id);
                        let raw_ids = ids.iter().copied().map(u64::from).collect::<Vec<_>>();
                        index.node_eq.insert(key, encode_u64_list(&raw_ids));
                    }
                }
            }
        }
    }

    fn remove_node_property(&mut self, node_id: NodeId, property: &str, value: &Value) {
        let Some(value) = IndexValue::from_gql_value(value) else {
            return;
        };
        match &mut self.backend {
            PropertyIndexBackend::AppendLog(_) => {
                let key = (property.to_owned(), value);
                if let Some(ids) = self.node_eq_mut().get_mut(&key) {
                    ids.retain(|id| *id != node_id);
                    if ids.is_empty() {
                        self.node_eq_mut().remove(&key);
                    }
                }
            }
            PropertyIndexBackend::AbTree(index) => {
                let key = encode_index_key(property, &value);
                let mut ids = decode_node_ids(index.node_eq.get(&key).unwrap_or(&[]));
                ids.retain(|id| *id != node_id);
                if ids.is_empty() {
                    index.node_eq.remove(&key);
                } else {
                    let raw_ids = ids.iter().copied().map(u64::from).collect::<Vec<_>>();
                    index.node_eq.insert(key, encode_u64_list(&raw_ids));
                }
            }
        }
    }

    fn insert_edge_property(&mut self, edge_id: EdgeId, property: &str, value: &Value) {
        if let Some(value) = IndexValue::from_gql_value(value) {
            match &mut self.backend {
                PropertyIndexBackend::AppendLog(_) => {
                    let ids = self.edge_eq_mut().entry((property.to_owned(), value)).or_default();
                    if !ids.contains(&edge_id) {
                        ids.push(edge_id);
                    }
                }
                PropertyIndexBackend::AbTree(index) => {
                    let key = encode_index_key(property, &value);
                    let mut ids = decode_edge_ids(index.edge_eq.get(&key).unwrap_or(&[]));
                    if !ids.contains(&edge_id) {
                        ids.push(edge_id);
                        index.edge_eq.insert(key, encode_u64_list(&ids));
                    }
                }
            }
        }
    }

    fn remove_edge_property(&mut self, edge_id: EdgeId, property: &str, value: &Value) {
        let Some(value) = IndexValue::from_gql_value(value) else {
            return;
        };
        match &mut self.backend {
            PropertyIndexBackend::AppendLog(_) => {
                let key = (property.to_owned(), value);
                if let Some(ids) = self.edge_eq_mut().get_mut(&key) {
                    ids.retain(|id| *id != edge_id);
                    if ids.is_empty() {
                        self.edge_eq_mut().remove(&key);
                    }
                }
            }
            PropertyIndexBackend::AbTree(index) => {
                let key = encode_index_key(property, &value);
                let mut ids = decode_edge_ids(index.edge_eq.get(&key).unwrap_or(&[]));
                ids.retain(|id| *id != edge_id);
                if ids.is_empty() {
                    index.edge_eq.remove(&key);
                } else {
                    index.edge_eq.insert(key, encode_u64_list(&ids));
                }
            }
        }
    }

    fn lookup_node_ids(&self, property: &str, value: &IndexValue) -> Vec<NodeId> {
        match &self.backend {
            PropertyIndexBackend::AppendLog(index) => index
                .node_eq
                .get(&(property.to_owned(), value.clone()))
                .cloned()
                .unwrap_or_default(),
            PropertyIndexBackend::AbTree(index) => {
                decode_node_ids(index.node_eq.get(&encode_index_key(property, value)).unwrap_or(&[]))
            }
        }
    }

    fn lookup_edge_ids(&self, property: &str, value: &IndexValue) -> Vec<EdgeId> {
        match &self.backend {
            PropertyIndexBackend::AppendLog(index) => index
                .edge_eq
                .get(&(property.to_owned(), value.clone()))
                .cloned()
                .unwrap_or_default(),
            PropertyIndexBackend::AbTree(index) => {
                decode_edge_ids(index.edge_eq.get(&encode_index_key(property, value)).unwrap_or(&[]))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum IndexValue {
    Null,
    Bool(bool),
    Int(i128),
    Uint(u128),
    Text(String),
    Bytes(Vec<u8>),
}

impl IndexValue {
    fn from_gql_value(value: &Value) -> Option<Self> {
        match value {
            Value::Null => Some(Self::Null),
            Value::Bool(v) => Some(Self::Bool(*v)),
            Value::Int8(v) => Some(Self::Int((*v).into())),
            Value::Int16(v) => Some(Self::Int((*v).into())),
            Value::Int32(v) => Some(Self::Int((*v).into())),
            Value::Int64(v) => Some(Self::Int((*v).into())),
            Value::Int128(v) => Some(Self::Int(*v)),
            Value::Uint8(v) => Some(Self::Uint((*v).into())),
            Value::Uint16(v) => Some(Self::Uint((*v).into())),
            Value::Uint32(v) => Some(Self::Uint((*v).into())),
            Value::Uint64(v) => Some(Self::Uint((*v).into())),
            Value::Uint128(v) => Some(Self::Uint(*v)),
            Value::Text(v) => Some(Self::Text(v.clone())),
            Value::Bytes(v) => Some(Self::Bytes(v.clone())),
            _ => None,
        }
    }

    fn encode(&self) -> Vec<u8> {
        match self {
            Self::Null => vec![0x00],
            Self::Bool(value) => vec![0x01, u8::from(*value)],
            Self::Int(value) => {
                let mut out = Vec::with_capacity(1 + 16);
                out.push(0x02);
                out.extend_from_slice(&value.to_be_bytes());
                out
            }
            Self::Uint(value) => {
                let mut out = Vec::with_capacity(1 + 16);
                out.push(0x03);
                out.extend_from_slice(&value.to_be_bytes());
                out
            }
            Self::Text(value) => {
                let mut out = Vec::with_capacity(1 + 4 + value.len());
                out.push(0x04);
                out.extend_from_slice(&(value.len() as u32).to_be_bytes());
                out.extend_from_slice(value.as_bytes());
                out
            }
            Self::Bytes(value) => {
                let mut out = Vec::with_capacity(1 + 4 + value.len());
                out.push(0x05);
                out.extend_from_slice(&(value.len() as u32).to_be_bytes());
                out.extend_from_slice(value);
                out
            }
        }
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

fn abtree_from_node_eq(
    entries: &BTreeMap<(String, IndexValue), Vec<NodeId>>,
) -> AbTree {
    let mut tree = AbTree::default();
    for ((property, value), ids) in entries {
        let raw_ids = ids.iter().copied().map(u64::from).collect::<Vec<_>>();
        tree.insert(encode_index_key(property, value), encode_u64_list(&raw_ids));
    }
    tree
}

fn abtree_from_edge_eq(
    entries: &BTreeMap<(String, IndexValue), Vec<EdgeId>>,
) -> AbTree {
    let mut tree = AbTree::default();
    for ((property, value), ids) in entries {
        tree.insert(encode_index_key(property, value), encode_u64_list(ids));
    }
    tree
}

fn encode_index_key(property: &str, value: &IndexValue) -> Vec<u8> {
    encode_property_index_key(property, &value.encode())
}

fn decode_node_ids(bytes: &[u8]) -> Vec<NodeId> {
    decode_u64_list(bytes)
        .into_iter()
        .map(|id| NodeId::try_from(id).expect("stored node id exceeds NodeId range"))
        .collect()
}

fn decode_edge_ids(bytes: &[u8]) -> Vec<EdgeId> {
    decode_u64_list(bytes)
}

#[cfg(test)]
mod tests {
    use super::{PropertyIndexBackendKind, PropertyIndexRuntime};
    use gleaph_gql::ast::CmpOp;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::{EdgeRecord, NodeId, NodeRecord};

    #[test]
    fn property_index_runtime_can_switch_to_abtree_backend() {
        let node = NodeRecord {
            id: NodeId::from(1u8),
            labels: vec!["User".to_owned()],
            properties: [("uid".to_owned(), Value::Text("u1".to_owned()))]
                .into_iter()
                .collect(),
        };
        let mut index = PropertyIndexRuntime::default();
        index.index_node(&node);
        assert_eq!(index.backend_kind(), PropertyIndexBackendKind::AppendLog);
        index.migrate_to_abtree();
        assert_eq!(index.backend_kind(), PropertyIndexBackendKind::AbTree);
        assert_eq!(
            index.scan_nodes(std::iter::once(&node), "uid", &Value::Text("u1".to_owned()), CmpOp::Eq)
                .len(),
            1
        );
    }

    #[test]
    fn property_index_runtime_can_start_with_abtree_backend() {
        let edge = EdgeRecord {
            id: 9,
            src: NodeId::from(1u8),
            dst: NodeId::from(2u8),
            label: Some("LINKS".to_owned()),
            properties: [("since".to_owned(), Value::Int64(2024))]
                .into_iter()
                .collect(),
        };
        let mut index = PropertyIndexRuntime::with_backend(PropertyIndexBackendKind::AbTree);
        index.index_edge(&edge);
        assert_eq!(index.backend_kind(), PropertyIndexBackendKind::AbTree);
        assert_eq!(
            index.scan_edges(std::iter::once(&edge), "since", &Value::Int64(2024)).len(),
            1
        );
    }
}
