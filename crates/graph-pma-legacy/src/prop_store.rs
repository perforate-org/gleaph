use std::collections::BTreeMap;

use gleaph_graph_kernel::{EdgeId, NodeId, PropertyMap};

use crate::abtree::AbTree;
use crate::prop_codec::{
    decode_property_map, decode_value_bytes, encode_property_map, encode_value_bytes, read_string,
    read_u32, write_string, write_u32,
};
use crate::prop_key::{
    decode_edge_prop_id, decode_node_prop_id, edge_prefix, node_prefix, EdgePropKey, NodePropKey,
    PropId,
};
use crate::layout::{LayoutError, LayoutResult};

pub fn collect_properties<K>(
    properties: impl IntoIterator<Item = (K, gleaph_gql::Value)>,
) -> PropertyMap
where
    K: Into<String>,
{
    properties
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect()
}

#[derive(Clone, Debug, Default)]
pub struct PropertyKeyCatalog {
    by_name: BTreeMap<String, PropId>,
    by_id: BTreeMap<PropId, String>,
    next_id: u32,
}

impl PropertyKeyCatalog {
    pub fn intern(&mut self, name: &str) -> PropId {
        if let Some(id) = self.by_name.get(name) {
            return *id;
        }
        let id = PropId(self.next_id);
        self.next_id += 1;
        self.by_name.insert(name.to_owned(), id);
        self.by_id.insert(id, name.to_owned());
        id
    }

    pub fn resolve(&self, id: PropId) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }
}

#[derive(Clone, Debug, Default)]
struct AppendLogPropertyStore {
    keys: PropertyKeyCatalog,
    node_props: BTreeMap<NodePropKey, gleaph_gql::Value>,
    edge_props: BTreeMap<EdgePropKey, gleaph_gql::Value>,
}

#[derive(Clone, Debug, Default)]
struct AbTreePropertyStore {
    keys: PropertyKeyCatalog,
    node_props: AbTree,
    edge_props: AbTree,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyStoreBackendKind {
    AppendLog,
    AbTree,
}

#[derive(Clone, Debug)]
enum PropertyStoreBackend {
    AppendLog(AppendLogPropertyStore),
    AbTree(AbTreePropertyStore),
}

impl Default for PropertyStoreBackend {
    fn default() -> Self {
        Self::AppendLog(AppendLogPropertyStore::default())
    }
}

#[derive(Clone, Debug, Default)]
pub struct PropertyStoreRuntime {
    backend: PropertyStoreBackend,
}

impl PropertyStoreRuntime {
    pub fn with_backend(kind: PropertyStoreBackendKind) -> Self {
        let backend = match kind {
            PropertyStoreBackendKind::AppendLog => {
                PropertyStoreBackend::AppendLog(AppendLogPropertyStore::default())
            }
            PropertyStoreBackendKind::AbTree => {
                PropertyStoreBackend::AbTree(AbTreePropertyStore::default())
            }
        };
        Self { backend }
    }

    pub fn build_from_graph<'a>(
        nodes: impl Iterator<Item = &'a gleaph_graph_kernel::NodeRecord>,
        edges: impl Iterator<Item = &'a gleaph_graph_kernel::EdgeRecord>,
    ) -> Self {
        let mut store = Self::default();
        for node in nodes {
            store.index_node(node);
        }
        for edge in edges {
            store.index_edge(edge);
        }
        store
    }

    pub fn backend_kind(&self) -> PropertyStoreBackendKind {
        match self.backend {
            PropertyStoreBackend::AppendLog(_) => PropertyStoreBackendKind::AppendLog,
            PropertyStoreBackend::AbTree(_) => PropertyStoreBackendKind::AbTree,
        }
    }

    pub fn migrate_to_abtree(&mut self) {
        if matches!(self.backend, PropertyStoreBackend::AbTree(_)) {
            return;
        }
        let (keys, node_props, edge_props) = match &self.backend {
            PropertyStoreBackend::AppendLog(store) => (
                store.keys.clone(),
                abtree_from_node_props(&store.node_props),
                abtree_from_edge_props(&store.edge_props),
            ),
            PropertyStoreBackend::AbTree(store) => (
                store.keys.clone(),
                store.node_props.clone(),
                store.edge_props.clone(),
            ),
        };
        self.backend = PropertyStoreBackend::AbTree(AbTreePropertyStore {
            keys,
            node_props,
            edge_props,
        });
    }

    pub fn snapshot_bytes(&self) -> Option<Vec<u8>> {
        let PropertyStoreBackend::AbTree(store) = &self.backend else {
            return None;
        };
        let mut out = Vec::new();
        write_u32(&mut out, store.keys.by_name.len() as u32);
        for (name, id) in &store.keys.by_name {
            write_string(&mut out, name);
            write_u32(&mut out, id.0);
        }
        let node_bytes = store.node_props.to_bytes();
        let edge_bytes = store.edge_props.to_bytes();
        write_u32(&mut out, node_bytes.len() as u32);
        out.extend_from_slice(&node_bytes);
        write_u32(&mut out, edge_bytes.len() as u32);
        out.extend_from_slice(&edge_bytes);
        Some(out)
    }

    pub fn from_snapshot(bytes: &[u8]) -> LayoutResult<Self> {
        let mut cursor = 0;
        let key_len = read_u32(bytes, &mut cursor)? as usize;
        let mut keys = PropertyKeyCatalog::default();
        for _ in 0..key_len {
            let name = read_string(bytes, &mut cursor)?;
            let id = PropId(read_u32(bytes, &mut cursor)?);
            keys.next_id = keys.next_id.max(id.0 + 1);
            keys.by_name.insert(name.clone(), id);
            keys.by_id.insert(id, name);
        }
        let node_len = read_u32(bytes, &mut cursor)? as usize;
        if cursor + node_len > bytes.len() {
            return Err(LayoutError::UnexpectedEof);
        }
        let node_props = AbTree::from_bytes(&bytes[cursor..cursor + node_len])?;
        cursor += node_len;
        let edge_len = read_u32(bytes, &mut cursor)? as usize;
        if cursor + edge_len > bytes.len() {
            return Err(LayoutError::UnexpectedEof);
        }
        let edge_props = AbTree::from_bytes(&bytes[cursor..cursor + edge_len])?;
        Ok(Self {
            backend: PropertyStoreBackend::AbTree(AbTreePropertyStore {
                keys,
                node_props,
                edge_props,
            }),
        })
    }

    pub fn index_node(&mut self, node: &gleaph_graph_kernel::NodeRecord) {
        for (property, value) in &node.properties {
            self.set_node_property(node.id, property, value.clone());
        }
    }

    pub fn deindex_node(&mut self, node: &gleaph_graph_kernel::NodeRecord) {
        for (property, _) in &node.properties {
            self.remove_node_property(node.id, property);
        }
    }

    pub fn index_edge(&mut self, edge: &gleaph_graph_kernel::EdgeRecord) {
        for (property, value) in &edge.properties {
            self.set_edge_property(edge.id, property, value.clone());
        }
    }

    pub fn deindex_edge(&mut self, edge: &gleaph_graph_kernel::EdgeRecord) {
        for (property, _) in &edge.properties {
            self.remove_edge_property(edge.id, property);
        }
    }

    pub fn set_node_property(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: gleaph_gql::Value,
    ) -> Option<gleaph_gql::Value> {
        let prop_id = self.keys_mut().intern(property);
        let key = NodePropKey { node_id, prop_id };
        match &mut self.backend {
            PropertyStoreBackend::AppendLog(_) => self.node_props_mut().insert(key, value),
            PropertyStoreBackend::AbTree(store) => {
                let encoded_key = key.encode();
                let old_value = store
                    .node_props
                    .get(&encoded_key)
                    .and_then(decode_value_bytes);
                store
                    .node_props
                    .insert(encoded_key, encode_value_bytes(&value));
                old_value
            }
        }
    }

    pub fn remove_node_property(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Option<gleaph_gql::Value> {
        let prop_id = self.keys_mut().intern(property);
        let key = NodePropKey { node_id, prop_id };
        match &mut self.backend {
            PropertyStoreBackend::AppendLog(_) => self.node_props_mut().remove(&key),
            PropertyStoreBackend::AbTree(store) => store
                .node_props
                .remove(&key.encode())
                .and_then(|bytes| decode_value_bytes(&bytes)),
        }
    }

    pub fn set_edge_property(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: gleaph_gql::Value,
    ) -> Option<gleaph_gql::Value> {
        let prop_id = self.keys_mut().intern(property);
        let key = EdgePropKey { edge_id, prop_id };
        match &mut self.backend {
            PropertyStoreBackend::AppendLog(_) => self.edge_props_mut().insert(key, value),
            PropertyStoreBackend::AbTree(store) => {
                let encoded_key = key.encode();
                let old_value = store
                    .edge_props
                    .get(&encoded_key)
                    .and_then(decode_value_bytes);
                store
                    .edge_props
                    .insert(encoded_key, encode_value_bytes(&value));
                old_value
            }
        }
    }

    pub fn remove_edge_property(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Option<gleaph_gql::Value> {
        let prop_id = self.keys_mut().intern(property);
        let key = EdgePropKey { edge_id, prop_id };
        match &mut self.backend {
            PropertyStoreBackend::AppendLog(_) => self.edge_props_mut().remove(&key),
            PropertyStoreBackend::AbTree(store) => store
                .edge_props
                .remove(&key.encode())
                .and_then(|bytes| decode_value_bytes(&bytes)),
        }
    }

    pub fn node_properties(&self, node_id: NodeId) -> PropertyMap {
        let mut props = PropertyMap::new();
        match &self.backend {
            PropertyStoreBackend::AppendLog(_) => {
                for (key, value) in self.node_props() {
                    if key.node_id == node_id
                        && let Some(name) = self.keys().resolve(key.prop_id)
                    {
                        props.insert(name.to_owned(), value.clone());
                    }
                }
            }
            PropertyStoreBackend::AbTree(store) => {
                let prefix = node_prefix(node_id);
                for (key, value) in store.node_props.scan_prefix(&prefix) {
                    if let Some(prop_id) = decode_node_prop_id(&key)
                        && let Some(name) = self.keys().resolve(prop_id)
                        && let Some(value) = decode_value_bytes(&value)
                    {
                        props.insert(name.to_owned(), value);
                    }
                }
            }
        }
        props
    }

    pub fn edge_properties(&self, edge_id: EdgeId) -> PropertyMap {
        let mut props = PropertyMap::new();
        match &self.backend {
            PropertyStoreBackend::AppendLog(_) => {
                for (key, value) in self.edge_props() {
                    if key.edge_id == edge_id
                        && let Some(name) = self.keys().resolve(key.prop_id)
                    {
                        props.insert(name.to_owned(), value.clone());
                    }
                }
            }
            PropertyStoreBackend::AbTree(store) => {
                let prefix = edge_prefix(edge_id);
                for (key, value) in store.edge_props.scan_prefix(&prefix) {
                    if let Some(prop_id) = decode_edge_prop_id(&key)
                        && let Some(name) = self.keys().resolve(prop_id)
                        && let Some(value) = decode_value_bytes(&value)
                    {
                        props.insert(name.to_owned(), value);
                    }
                }
            }
        }
        props
    }

    pub fn encode_node_snapshot(&self, node_id: NodeId, out: &mut Vec<u8>) {
        let props = self.node_properties(node_id);
        encode_property_map(&props, out);
    }

    pub fn decode_node_snapshot(
        &self,
        input: &[u8],
        cursor: &mut usize,
    ) -> crate::layout::LayoutResult<PropertyMap> {
        decode_property_map(input, cursor)
    }

    fn keys(&self) -> &PropertyKeyCatalog {
        match &self.backend {
            PropertyStoreBackend::AppendLog(store) => &store.keys,
            PropertyStoreBackend::AbTree(store) => &store.keys,
        }
    }

    fn keys_mut(&mut self) -> &mut PropertyKeyCatalog {
        match &mut self.backend {
            PropertyStoreBackend::AppendLog(store) => &mut store.keys,
            PropertyStoreBackend::AbTree(store) => &mut store.keys,
        }
    }

    fn node_props(&self) -> &BTreeMap<NodePropKey, gleaph_gql::Value> {
        match &self.backend {
            PropertyStoreBackend::AppendLog(store) => &store.node_props,
            PropertyStoreBackend::AbTree(_) => unreachable!("abtree backend does not expose tuple map"),
        }
    }

    fn node_props_mut(&mut self) -> &mut BTreeMap<NodePropKey, gleaph_gql::Value> {
        match &mut self.backend {
            PropertyStoreBackend::AppendLog(store) => &mut store.node_props,
            PropertyStoreBackend::AbTree(_) => unreachable!("abtree backend does not expose tuple map"),
        }
    }

    fn edge_props(&self) -> &BTreeMap<EdgePropKey, gleaph_gql::Value> {
        match &self.backend {
            PropertyStoreBackend::AppendLog(store) => &store.edge_props,
            PropertyStoreBackend::AbTree(_) => unreachable!("abtree backend does not expose tuple map"),
        }
    }

    fn edge_props_mut(&mut self) -> &mut BTreeMap<EdgePropKey, gleaph_gql::Value> {
        match &mut self.backend {
            PropertyStoreBackend::AppendLog(store) => &mut store.edge_props,
            PropertyStoreBackend::AbTree(_) => unreachable!("abtree backend does not expose tuple map"),
        }
    }
}

fn abtree_from_node_props(entries: &BTreeMap<NodePropKey, gleaph_gql::Value>) -> AbTree {
    let mut tree = AbTree::default();
    for (key, value) in entries {
        tree.insert(key.encode(), encode_value_bytes(value));
    }
    tree
}

fn abtree_from_edge_props(entries: &BTreeMap<EdgePropKey, gleaph_gql::Value>) -> AbTree {
    let mut tree = AbTree::default();
    for (key, value) in entries {
        tree.insert(key.encode(), encode_value_bytes(value));
    }
    tree
}

#[cfg(test)]
mod tests {
    use super::{PropertyStoreBackendKind, PropertyStoreRuntime};
    use gleaph_gql::Value;
    use gleaph_graph_kernel::NodeId;
    use std::collections::BTreeSet;

    #[test]
    fn property_store_runtime_round_trips_node_properties() {
        let mut store = PropertyStoreRuntime::default();
        let node_id = NodeId::from(7u8);
        store.set_node_property(node_id, "uid", Value::Int64(1));
        store.set_node_property(node_id, "name", Value::Text("Alice".to_owned()));
        let props = store.node_properties(node_id);
        assert_eq!(props.get("uid"), Some(&Value::Int64(1)));
        assert_eq!(props.get("name"), Some(&Value::Text("Alice".to_owned())));
    }

    #[test]
    fn property_store_runtime_removes_edge_properties() {
        let mut store = PropertyStoreRuntime::default();
        store.set_edge_property(9, "since", Value::Int64(2024));
        assert_eq!(store.remove_edge_property(9, "since"), Some(Value::Int64(2024)));
        assert!(store.edge_properties(9).is_empty());
    }

    #[test]
    fn property_keys_use_stable_binary_order() {
        let mut store = PropertyStoreRuntime::default();
        let node_id = NodeId::from(1u8);
        store.set_node_property(node_id, "a", Value::Int64(1));
        store.set_node_property(node_id, "b", Value::Int64(2));
        let encoded: BTreeSet<Vec<u8>> = store.node_props().keys().map(|key| key.encode()).collect();
        assert_eq!(encoded.len(), 2);
    }

    #[test]
    fn property_store_runtime_can_switch_to_abtree_backend() {
        let mut store = PropertyStoreRuntime::default();
        let node_id = NodeId::from(1u8);
        store.set_node_property(node_id, "uid", Value::Int64(7));
        assert_eq!(store.backend_kind(), PropertyStoreBackendKind::AppendLog);
        store.migrate_to_abtree();
        assert_eq!(store.backend_kind(), PropertyStoreBackendKind::AbTree);
        assert_eq!(store.node_properties(node_id).get("uid"), Some(&Value::Int64(7)));
    }

    #[test]
    fn property_store_runtime_can_start_with_abtree_backend() {
        let mut store = PropertyStoreRuntime::with_backend(PropertyStoreBackendKind::AbTree);
        store.set_edge_property(9, "since", Value::Int64(2024));
        assert_eq!(store.backend_kind(), PropertyStoreBackendKind::AbTree);
        assert_eq!(
            store.edge_properties(9).get("since"),
            Some(&Value::Int64(2024))
        );
    }
}
