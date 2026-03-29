use gleaph_gql::ast::CmpOp;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::Value;
use gleaph_graph_kernel::{
    EdgeRecord, Expansion, GraphRead, GraphResult, GraphWrite, NodeId, NodeRecord, PropertyMap,
};

use crate::edge_array::AdjacencyPma;
use crate::ids::IdAllocators;
use crate::layout::{GraphLayoutHeader, HEADER_SIZE, LayoutError, LayoutResult};
use crate::memory::Memory;
use crate::node_catalog::NodeCatalog;
use crate::prop_codec::{
    decode_property_map, decode_value, encode_property_map, encode_value, read_string, read_u8,
    read_u32, read_u64, write_string, write_u8, write_u32, write_u64,
};
use crate::prop_index::{PropertyIndexBackendKind, PropertyIndexRuntime};
use crate::prop_store::{collect_properties, PropertyStoreBackendKind, PropertyStoreRuntime};

const OP_INSERT_NODE: u8 = 1;
const OP_INSERT_EDGE: u8 = 2;
const OP_SET_NODE_PROPERTY: u8 = 3;
const OP_REMOVE_NODE_PROPERTY: u8 = 4;
const OP_ADD_NODE_LABEL: u8 = 5;
const OP_REMOVE_NODE_LABEL: u8 = 6;
const OP_SET_EDGE_PROPERTY: u8 = 7;
const OP_REMOVE_EDGE_PROPERTY: u8 = 8;
const OP_SET_EDGE_LABEL: u8 = 9;
const OP_DELETE_NODE: u8 = 10;
const OP_DELETE_EDGE: u8 = 11;
const MIN_AUTO_COMPACT_LOG_BYTES: u64 = 1024;
const AUTO_COMPACT_BASE_RATIO_NUMERATOR: u64 = 1;
const AUTO_COMPACT_BASE_RATIO_DENOMINATOR: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertySubsystemBackendKind {
    AppendLog,
    AbTree,
}

impl PropertySubsystemBackendKind {
    fn store_backend(self) -> PropertyStoreBackendKind {
        match self {
            Self::AppendLog => PropertyStoreBackendKind::AppendLog,
            Self::AbTree => PropertyStoreBackendKind::AbTree,
        }
    }

    fn index_backend(self) -> PropertyIndexBackendKind {
        match self {
            Self::AppendLog => PropertyIndexBackendKind::AppendLog,
            Self::AbTree => PropertyIndexBackendKind::AbTree,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GraphPma<M> {
    memory: M,
    ids: IdAllocators,
    nodes: NodeCatalog,
    edges: AdjacencyPma,
    prop_store: PropertyStoreRuntime,
    prop_index: PropertyIndexRuntime,
}

impl<M: Memory> GraphPma<M> {
    pub fn init(memory: M) -> Self {
        Self::init_with_property_subsystem_backend(memory, PropertySubsystemBackendKind::AppendLog)
    }

    pub fn init_with_property_subsystem_backend(
        memory: M,
        backend: PropertySubsystemBackendKind,
    ) -> Self {
        Self::init_with_property_backends(memory, backend.store_backend(), backend.index_backend())
    }

    pub fn init_with_property_store_backend(
        memory: M,
        property_store_backend: PropertyStoreBackendKind,
    ) -> Self {
        Self::init_with_property_backends(
            memory,
            property_store_backend,
            PropertyIndexBackendKind::AppendLog,
        )
    }

    pub fn init_with_property_backends(
        mut memory: M,
        property_store_backend: PropertyStoreBackendKind,
        property_index_backend: PropertyIndexBackendKind,
    ) -> Self {
        let header = GraphLayoutHeader::new();
        header.write_into(&mut memory);
        Self {
            memory,
            ids: IdAllocators::default(),
            nodes: NodeCatalog::default(),
            edges: AdjacencyPma::default(),
            prop_store: PropertyStoreRuntime::with_backend(property_store_backend),
            prop_index: PropertyIndexRuntime::with_backend(property_index_backend),
        }
    }

    pub fn property_store_backend_kind(&self) -> PropertyStoreBackendKind {
        self.prop_store.backend_kind()
    }

    pub fn migrate_property_store_to_abtree(&mut self) {
        self.prop_store.migrate_to_abtree();
    }

    pub fn property_subsystem_backend_kind(&self) -> Option<PropertySubsystemBackendKind> {
        match (
            self.property_store_backend_kind(),
            self.property_index_backend_kind(),
        ) {
            (PropertyStoreBackendKind::AppendLog, PropertyIndexBackendKind::AppendLog) => {
                Some(PropertySubsystemBackendKind::AppendLog)
            }
            (PropertyStoreBackendKind::AbTree, PropertyIndexBackendKind::AbTree) => {
                Some(PropertySubsystemBackendKind::AbTree)
            }
            _ => None,
        }
    }

    pub fn property_index_backend_kind(&self) -> PropertyIndexBackendKind {
        self.prop_index.backend_kind()
    }

    pub fn migrate_property_index_to_abtree(&mut self) {
        self.prop_index.migrate_to_abtree();
    }

    pub fn migrate_property_subsystem_to_abtree(&mut self) {
        self.migrate_property_store_to_abtree();
        self.migrate_property_index_to_abtree();
    }

    pub fn open(memory: M) -> LayoutResult<Self> {
        let header = GraphLayoutHeader::read_from(&memory)?;
        let (mut nodes, mut edges) = load_payload(&memory, &header)?;
        replay_op_log(&memory, &header, &mut nodes, &mut edges)?;
        let nodes = NodeCatalog::from_records(nodes);
        let (adjacency_offset, property_offset, property_index_offset, _) = payload_offsets(&header);
        let edges = if header.adjacency_bytes_len > 0 && header.op_log_bytes_len == 0 {
            let bytes = read_region(&memory, adjacency_offset, header.adjacency_bytes_len as usize);
            AdjacencyPma::from_snapshot(&bytes, edges)?
        } else {
            AdjacencyPma::from_edges(edges)
        };
        let prop_store = if header.property_bytes_len > 0 && header.op_log_bytes_len == 0 {
            let bytes = read_region(&memory, property_offset, header.property_bytes_len as usize);
            PropertyStoreRuntime::from_snapshot(&bytes)?
        } else {
            PropertyStoreRuntime::build_from_graph(nodes.iter(), edges.iter())
        };
        let prop_index =
            if header.property_index_bytes_len > 0 && header.op_log_bytes_len == 0 {
                let bytes = read_region(
                    &memory,
                    property_index_offset,
                    header.property_index_bytes_len as usize,
                );
                PropertyIndexRuntime::from_snapshot(&bytes)?
            } else {
                PropertyIndexRuntime::build_from_graph(nodes.iter(), edges.iter())
            };
        Ok(Self {
            memory,
            ids: IdAllocators::with_counts(header.node_count, header.edge_count),
            nodes,
            edges,
            prop_store,
            prop_index,
        })
    }

    pub fn insert_node<I, K>(
        &mut self,
        labels: I,
        properties: impl IntoIterator<Item = (K, Value)>,
    ) -> NodeId
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
        K: Into<String>,
    {
        let id = self.ids.next_node_id();
        self.nodes.insert(
            id,
            labels
                .into_iter()
                .map(|label| label.as_ref().to_owned())
                .collect(),
            collect_properties(properties),
        );
        let node = self.nodes.get(id).cloned().expect("inserted node exists");
        self.prop_store.index_node(&node);
        self.prop_index.index_node(&node);
        self.append_node_op(&node);
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
        let edge = EdgeRecord {
            id: self.ids.next_edge_id(),
            src,
            dst,
            label: label.map(str::to_owned),
            properties: collect_properties(properties),
        };
        let id = self.edges.insert(edge.clone());
        self.prop_store.index_edge(&edge);
        self.prop_index.index_edge(&edge);
        self.append_edge_op(&edge);
        id
    }

    pub fn memory(&self) -> &M {
        &self.memory
    }

    pub fn compact(&mut self) {
        let node_bytes = encode_nodes(&self.nodes);
        let edge_bytes = encode_edges(&self.edges);
        let adjacency_bytes = self.edges.snapshot_bytes();
        let property_bytes = self.prop_store.snapshot_bytes().unwrap_or_default();
        let property_index_bytes = self.prop_index.snapshot_bytes().unwrap_or_default();
        let total_len =
            HEADER_SIZE
                + node_bytes.len()
                + edge_bytes.len()
                + adjacency_bytes.len()
                + property_bytes.len()
                + property_index_bytes.len();
        self.memory.resize(total_len);
        GraphLayoutHeader {
            version: crate::layout::GRAPH_PMA_VERSION,
            node_count: self.ids.node_count(),
            edge_count: self.ids.edge_count(),
            node_bytes_len: node_bytes.len() as u64,
            edge_bytes_len: edge_bytes.len() as u64,
            adjacency_bytes_len: adjacency_bytes.len() as u64,
            property_bytes_len: property_bytes.len() as u64,
            property_index_bytes_len: property_index_bytes.len() as u64,
            op_log_bytes_len: 0,
        }
        .write_into(&mut self.memory);
        self.memory.write(HEADER_SIZE, &node_bytes);
        self.memory.write(HEADER_SIZE + node_bytes.len(), &edge_bytes);
        self.memory.write(
            HEADER_SIZE + node_bytes.len() + edge_bytes.len(),
            &adjacency_bytes,
        );
        self.memory.write(
            HEADER_SIZE + node_bytes.len() + edge_bytes.len() + adjacency_bytes.len(),
            &property_bytes,
        );
        self.memory.write(
            HEADER_SIZE
                + node_bytes.len()
                + edge_bytes.len()
                + adjacency_bytes.len()
                + property_bytes.len(),
            &property_index_bytes,
        );
    }

    fn append_node_op(&mut self, node: &NodeRecord) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_INSERT_NODE);
        encode_node_record(node, &mut op);
        self.append_op(&op);
    }

    fn append_edge_op(&mut self, edge: &EdgeRecord) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_INSERT_EDGE);
        encode_edge_record(edge, &mut op);
        self.append_op(&op);
    }

    fn append_set_node_property_op(&mut self, node_id: NodeId, property: &str, value: &Value) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_SET_NODE_PROPERTY);
        write_u64(&mut op, node_id.into());
        write_string(&mut op, property);
        encode_value(value, &mut op);
        self.append_op(&op);
    }

    fn append_remove_node_property_op(&mut self, node_id: NodeId, property: &str) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_REMOVE_NODE_PROPERTY);
        write_u64(&mut op, node_id.into());
        write_string(&mut op, property);
        self.append_op(&op);
    }

    fn append_add_node_label_op(&mut self, node_id: NodeId, label: &str) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_ADD_NODE_LABEL);
        write_u64(&mut op, node_id.into());
        write_string(&mut op, label);
        self.append_op(&op);
    }

    fn append_remove_node_label_op(&mut self, node_id: NodeId, label: &str) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_REMOVE_NODE_LABEL);
        write_u64(&mut op, node_id.into());
        write_string(&mut op, label);
        self.append_op(&op);
    }

    fn append_set_edge_property_op(&mut self, edge_id: u64, property: &str, value: &Value) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_SET_EDGE_PROPERTY);
        write_u64(&mut op, edge_id);
        write_string(&mut op, property);
        encode_value(value, &mut op);
        self.append_op(&op);
    }

    fn append_remove_edge_property_op(&mut self, edge_id: u64, property: &str) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_REMOVE_EDGE_PROPERTY);
        write_u64(&mut op, edge_id);
        write_string(&mut op, property);
        self.append_op(&op);
    }

    fn append_set_edge_label_op(&mut self, edge_id: u64, label: Option<&str>) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_SET_EDGE_LABEL);
        write_u64(&mut op, edge_id);
        match label {
            Some(label) => {
                write_u8(&mut op, 1);
                write_string(&mut op, label);
            }
            None => write_u8(&mut op, 0),
        }
        self.append_op(&op);
    }

    fn append_delete_node_op(&mut self, node_id: NodeId, detach: bool) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_DELETE_NODE);
        write_u64(&mut op, node_id.into());
        write_u8(&mut op, u8::from(detach));
        self.append_op(&op);
    }

    fn append_delete_edge_op(&mut self, edge_id: u64) {
        let mut op = Vec::new();
        write_u8(&mut op, OP_DELETE_EDGE);
        write_u64(&mut op, edge_id);
        self.append_op(&op);
    }

    fn append_op(&mut self, op: &[u8]) {
        let mut header = GraphLayoutHeader::read_from(&self.memory).expect("graph header must exist");
        let (_, _, _, base_offset) = payload_offsets(&header);
        let log_offset = base_offset + header.op_log_bytes_len as usize;
        let new_len = log_offset + op.len();
        self.memory.resize(new_len);
        self.memory.write(log_offset, op);
        header.node_count = self.ids.node_count();
        header.edge_count = self.ids.edge_count();
        header.op_log_bytes_len += op.len() as u64;
        header.write_into(&mut self.memory);
        if should_auto_compact(&header) {
            self.compact();
        }
    }
}

impl<M: Memory> GraphRead for GraphPma<M> {
    fn scan_nodes(&self, label: Option<&str>) -> GraphResult<Vec<NodeRecord>> {
        Ok(self.nodes.scan_by_label(label))
    }

    fn scan_nodes_by_property(
        &self,
        property: &str,
        value: &Value,
        cmp: CmpOp,
    ) -> GraphResult<Vec<NodeRecord>> {
        Ok(self
            .prop_index
            .scan_nodes(self.nodes.iter(), property, value, cmp))
    }

    fn scan_edges_by_property(
        &self,
        property: &str,
        value: &Value,
    ) -> GraphResult<Vec<EdgeRecord>> {
        Ok(self
            .prop_index
            .scan_edges(self.edges.iter(), property, value))
    }

    fn expand(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        label: Option<&str>,
    ) -> GraphResult<Vec<Expansion>> {
        Ok(self.edges.expand(&self.nodes, from, direction, label))
    }

    fn get_node(&self, id: NodeId) -> GraphResult<Option<NodeRecord>> {
        Ok(self.nodes.get(id).cloned())
    }
}

impl<M: Memory> GraphWrite for GraphPma<M> {
    fn insert_node(
        &mut self,
        labels: &[String],
        properties: &PropertyMap,
    ) -> GraphResult<NodeRecord> {
        let id = self.insert_node(labels.iter().map(String::as_str), properties.clone());
        Ok(self.nodes.get(id).cloned().expect("inserted node exists"))
    }

    fn insert_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        let id = self.insert_edge(src, dst, label, properties.clone());
        Ok(self.edges.edge(id).cloned().expect("inserted edge exists"))
    }

    fn set_node_property(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<NodeRecord> {
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or(gleaph_graph_kernel::GraphError::NodeNotFound(node_id))?;
        let old_value = node.properties.insert(property.to_owned(), value.clone());
        self.prop_store.set_node_property(node_id, property, value.clone());
        let node = node.clone();
        self.prop_index
            .update_node_property(node_id, property, old_value.as_ref(), Some(value));
        self.append_set_node_property_op(node_id, property, value);
        Ok(node)
    }

    fn remove_node_property(&mut self, node_id: NodeId, property: &str) -> GraphResult<NodeRecord> {
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or(gleaph_graph_kernel::GraphError::NodeNotFound(node_id))?;
        let old_value = node.properties.remove(property);
        self.prop_store.remove_node_property(node_id, property);
        let node = node.clone();
        self.prop_index
            .update_node_property(node_id, property, old_value.as_ref(), None);
        self.append_remove_node_property_op(node_id, property);
        Ok(node)
    }

    fn add_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord> {
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or(gleaph_graph_kernel::GraphError::NodeNotFound(node_id))?;
        if !node.labels.iter().any(|existing| existing == label) {
            node.labels.push(label.to_owned());
        }
        let node = node.clone();
        self.append_add_node_label_op(node_id, label);
        Ok(node)
    }

    fn remove_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord> {
        let node = self
            .nodes
            .get_mut(node_id)
            .ok_or(gleaph_graph_kernel::GraphError::NodeNotFound(node_id))?;
        node.labels.retain(|existing| existing != label);
        let node = node.clone();
        self.append_remove_node_label_op(node_id, label);
        Ok(node)
    }

    fn set_edge_property(
        &mut self,
        edge_id: gleaph_graph_kernel::EdgeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<EdgeRecord> {
        let edge = self
            .edges
            .edge_mut(edge_id)
            .ok_or(gleaph_graph_kernel::GraphError::EdgeNotFound(edge_id))?;
        let old_value = edge.properties.insert(property.to_owned(), value.clone());
        self.prop_store.set_edge_property(edge_id, property, value.clone());
        let edge = edge.clone();
        self.prop_index
            .update_edge_property(edge_id, property, old_value.as_ref(), Some(value));
        self.append_set_edge_property_op(edge_id, property, value);
        Ok(edge)
    }

    fn remove_edge_property(
        &mut self,
        edge_id: gleaph_graph_kernel::EdgeId,
        property: &str,
    ) -> GraphResult<EdgeRecord> {
        let edge = self
            .edges
            .edge_mut(edge_id)
            .ok_or(gleaph_graph_kernel::GraphError::EdgeNotFound(edge_id))?;
        let old_value = edge.properties.remove(property);
        self.prop_store.remove_edge_property(edge_id, property);
        let edge = edge.clone();
        self.prop_index
            .update_edge_property(edge_id, property, old_value.as_ref(), None);
        self.append_remove_edge_property_op(edge_id, property);
        Ok(edge)
    }

    fn set_edge_label(
        &mut self,
        edge_id: gleaph_graph_kernel::EdgeId,
        label: Option<&str>,
    ) -> GraphResult<EdgeRecord> {
        let edge = self
            .edges
            .edge_mut(edge_id)
            .ok_or(gleaph_graph_kernel::GraphError::EdgeNotFound(edge_id))?;
        edge.label = label.map(str::to_owned);
        let edge = edge.clone();
        self.append_set_edge_label_op(edge_id, label);
        Ok(edge)
    }

    fn delete_edge(&mut self, edge_id: gleaph_graph_kernel::EdgeId) -> GraphResult<()> {
        let removed = self
            .edges
            .remove_edge(edge_id)
            .ok_or(gleaph_graph_kernel::GraphError::EdgeNotFound(edge_id))?;
        self.prop_store.deindex_edge(&removed);
        self.prop_index.deindex_edge(&removed);
        self.append_delete_edge_op(edge_id);
        Ok(())
    }

    fn delete_node(&mut self, node_id: NodeId, detach: bool) -> GraphResult<()> {
        if self.nodes.get(node_id).is_none() {
            return Err(gleaph_graph_kernel::GraphError::NodeNotFound(node_id));
        }
        if self.edges.has_incident_edges(node_id) && !detach {
            return Err(gleaph_graph_kernel::GraphError::Message(
                "node has incident edges".to_owned(),
            ));
        }
        if detach {
            for edge in self.edges.remove_incident_edges(node_id) {
                self.prop_store.deindex_edge(&edge);
                self.prop_index.deindex_edge(&edge);
            }
        }
        let removed = self.nodes.remove(node_id).expect("checked node exists");
        self.prop_store.deindex_node(&removed);
        self.prop_index.deindex_node(&removed);
        self.append_delete_node_op(node_id, detach);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct GraphPmaBuilder<M> {
    memory: M,
    property_store_backend: PropertyStoreBackendKind,
    property_index_backend: PropertyIndexBackendKind,
}

impl<M: Memory> GraphPmaBuilder<M> {
    pub fn new(memory: M) -> Self {
        Self::with_property_subsystem_backend(memory, PropertySubsystemBackendKind::AppendLog)
    }

    pub fn with_property_subsystem_backend(
        memory: M,
        backend: PropertySubsystemBackendKind,
    ) -> Self {
        Self {
            memory,
            property_store_backend: backend.store_backend(),
            property_index_backend: backend.index_backend(),
        }
    }

    pub fn property_store_backend(mut self, backend: PropertyStoreBackendKind) -> Self {
        self.property_store_backend = backend;
        self
    }

    pub fn property_index_backend(mut self, backend: PropertyIndexBackendKind) -> Self {
        self.property_index_backend = backend;
        self
    }

    pub fn property_subsystem_backend(mut self, backend: PropertySubsystemBackendKind) -> Self {
        self.property_store_backend = backend.store_backend();
        self.property_index_backend = backend.index_backend();
        self
    }

    pub fn build(self) -> GraphPma<M> {
        GraphPma::init_with_property_backends(
            self.memory,
            self.property_store_backend,
            self.property_index_backend,
        )
    }
}

fn encode_nodes(nodes: &NodeCatalog) -> Vec<u8> {
    let mut out = Vec::new();
    write_u32(&mut out, nodes.len() as u32);
    for node in nodes.iter() {
        encode_node_record(node, &mut out);
    }
    out
}

fn decode_nodes(bytes: &[u8]) -> LayoutResult<Vec<NodeRecord>> {
    let mut cursor = 0;
    let len = read_u32(bytes, &mut cursor)? as usize;
    let mut nodes = Vec::with_capacity(len);
    for _ in 0..len {
        nodes.push(decode_node_record(bytes, &mut cursor)?);
    }
    Ok(nodes)
}

fn encode_edges(edges: &AdjacencyPma) -> Vec<u8> {
    let mut out = Vec::new();
    write_u32(&mut out, edges.len() as u32);
    for edge in edges.iter() {
        encode_edge_record(edge, &mut out);
    }
    out
}

fn decode_edges(bytes: &[u8]) -> LayoutResult<Vec<EdgeRecord>> {
    let mut cursor = 0;
    let len = read_u32(bytes, &mut cursor)? as usize;
    let mut edges = Vec::with_capacity(len);
    for _ in 0..len {
        edges.push(decode_edge_record(bytes, &mut cursor)?);
    }
    Ok(edges)
}

fn encode_node_record(node: &NodeRecord, out: &mut Vec<u8>) {
    write_u64(out, node.id.into());
    write_u32(out, node.labels.len() as u32);
    for label in &node.labels {
        write_string(out, label);
    }
    encode_property_map(&node.properties, out);
}

fn decode_node_record(bytes: &[u8], cursor: &mut usize) -> LayoutResult<NodeRecord> {
    let id = NodeId::try_from(read_u64(bytes, cursor)?).map_err(|_| LayoutError::InvalidPayload)?;
    let label_len = read_u32(bytes, cursor)? as usize;
    let mut labels = Vec::with_capacity(label_len);
    for _ in 0..label_len {
        labels.push(read_string(bytes, cursor)?);
    }
    let properties = decode_property_map(bytes, cursor)?;
    Ok(NodeRecord { id, labels, properties })
}

fn encode_edge_record(edge: &EdgeRecord, out: &mut Vec<u8>) {
    write_u64(out, edge.id);
    write_u64(out, edge.src.into());
    write_u64(out, edge.dst.into());
    match &edge.label {
        Some(label) => {
            write_u8(out, 1);
            write_string(out, label);
        }
        None => write_u8(out, 0),
    }
    encode_property_map(&edge.properties, out);
}

fn decode_edge_record(bytes: &[u8], cursor: &mut usize) -> LayoutResult<EdgeRecord> {
    let id = read_u64(bytes, cursor)?;
    let src = NodeId::try_from(read_u64(bytes, cursor)?).map_err(|_| LayoutError::InvalidPayload)?;
    let dst = NodeId::try_from(read_u64(bytes, cursor)?).map_err(|_| LayoutError::InvalidPayload)?;
    let has_label = read_u8(bytes, cursor)?;
    let label = if has_label == 1 {
        Some(read_string(bytes, cursor)?)
    } else {
        None
    };
    let properties = decode_property_map(bytes, cursor)?;
    Ok(EdgeRecord { id, src, dst, label, properties })
}

fn load_payload<M: Memory>(
    memory: &M,
    header: &GraphLayoutHeader,
) -> LayoutResult<(Vec<NodeRecord>, Vec<EdgeRecord>)> {
    let node_len = header.node_bytes_len as usize;
    let edge_len = header.edge_bytes_len as usize;
    if node_len == 0 && edge_len == 0 {
        return Ok((Vec::new(), Vec::new()));
    }

    let node_bytes = read_region(memory, HEADER_SIZE, node_len);
    let edge_bytes = read_region(memory, HEADER_SIZE + node_len, edge_len);

    Ok((decode_nodes(&node_bytes)?, decode_edges(&edge_bytes)?))
}

fn replay_op_log<M: Memory>(
    memory: &M,
    header: &GraphLayoutHeader,
    nodes: &mut Vec<NodeRecord>,
    edges: &mut Vec<EdgeRecord>,
) -> LayoutResult<()> {
    let log_len = header.op_log_bytes_len as usize;
    if log_len == 0 {
        return Ok(());
    }

    let (_, _, _, offset) = payload_offsets(header);
    let mut bytes = vec![0u8; log_len];
    memory.read(offset, &mut bytes);

    let mut cursor = 0;
    while cursor < bytes.len() {
        match read_u8(&bytes, &mut cursor)? {
            OP_INSERT_NODE => nodes.push(decode_node_record(&bytes, &mut cursor)?),
            OP_INSERT_EDGE => edges.push(decode_edge_record(&bytes, &mut cursor)?),
            OP_SET_NODE_PROPERTY => {
                let node_id = NodeId::try_from(read_u64(&bytes, &mut cursor)?)
                    .map_err(|_| crate::layout::LayoutError::InvalidPayload)?;
                let property = read_string(&bytes, &mut cursor)?;
                let value = decode_value(&bytes, &mut cursor)?;
                let Some(node) = nodes.iter_mut().find(|node| node.id == node_id) else {
                    return Err(crate::layout::LayoutError::InvalidPayload);
                };
                node.properties.insert(property, value);
            }
            OP_REMOVE_NODE_PROPERTY => {
                let node_id = NodeId::try_from(read_u64(&bytes, &mut cursor)?)
                    .map_err(|_| crate::layout::LayoutError::InvalidPayload)?;
                let property = read_string(&bytes, &mut cursor)?;
                let Some(node) = nodes.iter_mut().find(|node| node.id == node_id) else {
                    return Err(crate::layout::LayoutError::InvalidPayload);
                };
                node.properties.remove(&property);
            }
            OP_ADD_NODE_LABEL => {
                let node_id = NodeId::try_from(read_u64(&bytes, &mut cursor)?)
                    .map_err(|_| crate::layout::LayoutError::InvalidPayload)?;
                let label = read_string(&bytes, &mut cursor)?;
                let Some(node) = nodes.iter_mut().find(|node| node.id == node_id) else {
                    return Err(crate::layout::LayoutError::InvalidPayload);
                };
                if !node.labels.contains(&label) {
                    node.labels.push(label);
                }
            }
            OP_REMOVE_NODE_LABEL => {
                let node_id = NodeId::try_from(read_u64(&bytes, &mut cursor)?)
                    .map_err(|_| crate::layout::LayoutError::InvalidPayload)?;
                let label = read_string(&bytes, &mut cursor)?;
                let Some(node) = nodes.iter_mut().find(|node| node.id == node_id) else {
                    return Err(crate::layout::LayoutError::InvalidPayload);
                };
                node.labels.retain(|existing| existing != &label);
            }
            OP_SET_EDGE_PROPERTY => {
                let edge_id = read_u64(&bytes, &mut cursor)?;
                let property = read_string(&bytes, &mut cursor)?;
                let value = decode_value(&bytes, &mut cursor)?;
                let Some(edge) = edges.iter_mut().find(|edge| edge.id == edge_id) else {
                    return Err(crate::layout::LayoutError::InvalidPayload);
                };
                edge.properties.insert(property, value);
            }
            OP_REMOVE_EDGE_PROPERTY => {
                let edge_id = read_u64(&bytes, &mut cursor)?;
                let property = read_string(&bytes, &mut cursor)?;
                let Some(edge) = edges.iter_mut().find(|edge| edge.id == edge_id) else {
                    return Err(crate::layout::LayoutError::InvalidPayload);
                };
                edge.properties.remove(&property);
            }
            OP_SET_EDGE_LABEL => {
                let edge_id = read_u64(&bytes, &mut cursor)?;
                let has_label = read_u8(&bytes, &mut cursor)?;
                let label = if has_label == 1 {
                    Some(read_string(&bytes, &mut cursor)?)
                } else {
                    None
                };
                let Some(edge) = edges.iter_mut().find(|edge| edge.id == edge_id) else {
                    return Err(crate::layout::LayoutError::InvalidPayload);
                };
                edge.label = label;
            }
            OP_DELETE_NODE => {
                let node_id = NodeId::try_from(read_u64(&bytes, &mut cursor)?)
                    .map_err(|_| crate::layout::LayoutError::InvalidPayload)?;
                let detach = read_u8(&bytes, &mut cursor)? != 0;
                if detach {
                    edges.retain(|edge| edge.src != node_id && edge.dst != node_id);
                } else if edges.iter().any(|edge| edge.src == node_id || edge.dst == node_id) {
                    return Err(crate::layout::LayoutError::InvalidPayload);
                }
                let before_len = nodes.len();
                nodes.retain(|node| node.id != node_id);
                if nodes.len() == before_len {
                    return Err(crate::layout::LayoutError::InvalidPayload);
                }
            }
            OP_DELETE_EDGE => {
                let edge_id = read_u64(&bytes, &mut cursor)?;
                let before_len = edges.len();
                edges.retain(|edge| edge.id != edge_id);
                if edges.len() == before_len {
                    return Err(crate::layout::LayoutError::InvalidPayload);
                }
            }
            _ => return Err(crate::layout::LayoutError::InvalidPayload),
        }
    }

    Ok(())
}

fn should_auto_compact(header: &GraphLayoutHeader) -> bool {
    header.op_log_bytes_len >= auto_compact_threshold_bytes(header)
}

fn auto_compact_threshold_bytes(header: &GraphLayoutHeader) -> u64 {
    let base_bytes = header.node_bytes_len
        + header.edge_bytes_len
        + header.adjacency_bytes_len
        + header.property_bytes_len
        + header.property_index_bytes_len;
    let ratio_bytes =
        base_bytes.saturating_mul(AUTO_COMPACT_BASE_RATIO_NUMERATOR) / AUTO_COMPACT_BASE_RATIO_DENOMINATOR;
    ratio_bytes.max(MIN_AUTO_COMPACT_LOG_BYTES)
}

fn payload_offsets(header: &GraphLayoutHeader) -> (usize, usize, usize, usize) {
    let adjacency_offset = HEADER_SIZE + header.node_bytes_len as usize + header.edge_bytes_len as usize;
    let property_offset = adjacency_offset + header.adjacency_bytes_len as usize;
    let property_index_offset = property_offset + header.property_bytes_len as usize;
    let log_offset = property_index_offset + header.property_index_bytes_len as usize;
    (adjacency_offset, property_offset, property_index_offset, log_offset)
}

fn read_region<M: Memory>(memory: &M, offset: usize, len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    if len > 0 {
        memory.read(offset, &mut bytes);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use gleaph_gql::ast::CmpOp;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::{GraphRead, GraphWrite, NodeId};
    use gleaph_graph_kernel::PropertyMap;

    use crate::layout::GraphLayoutHeader;
    use crate::memory::VecMemory;

    use super::{
        auto_compact_threshold_bytes, GraphPma, GraphPmaBuilder, PropertySubsystemBackendKind,
        MIN_AUTO_COMPACT_LOG_BYTES,
    };
    use crate::prop_index::PropertyIndexBackendKind;
    use crate::prop_store::PropertyStoreBackendKind;

    impl GraphPma<VecMemory> {
        fn node_props_for_test(&self, node_id: NodeId) -> PropertyMap {
            self.prop_store.node_properties(node_id)
        }

        fn edge_props_for_test(&self, edge_id: u64) -> PropertyMap {
            self.prop_store.edge_properties(edge_id)
        }
    }

    #[test]
    fn persists_header_counts_into_memory() {
        let memory = VecMemory::new();
        let mut graph = GraphPma::init(memory);
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("Hello".to_owned()))]);
        graph.insert_edge(alice, post, Some("AUTHORED"), [("since", Value::Int64(2024))]);

        let header = GraphLayoutHeader::read_from(graph.memory()).expect("header should be readable");
        assert_eq!(header.node_count, 2);
        assert_eq!(header.edge_count, 1);
        assert!(header.op_log_bytes_len > 0);
    }

    #[test]
    fn supports_graph_read_contract() {
        let memory = VecMemory::new();
        let mut graph = GraphPma::init(memory);
        let alice = graph.insert_node(
            ["User"],
            [("uid", Value::Text("u1".to_owned()))],
        );
        let bob = graph.insert_node(
            ["User"],
            [("uid", Value::Text("u2".to_owned()))],
        );
        let post = graph.insert_node(
            ["Post"],
            [("title", Value::Text("Hello".to_owned()))],
        );
        graph.insert_edge(alice, post, Some("AUTHORED"), [("weight", Value::Int64(10))]);
        graph.insert_edge(bob, post, Some("LIKED"), [("weight", Value::Int64(5))]);

        assert_eq!(graph.scan_nodes(Some("User")).expect("scan").len(), 2);
        assert_eq!(
            graph.scan_nodes_by_property("uid", &Value::Text("u2".to_owned()), gleaph_gql::ast::CmpOp::Eq)
                .expect("property scan")
                .len(),
            1
        );
        assert_eq!(
            graph.scan_edges_by_property("weight", &Value::Int64(10))
                .expect("edge property scan")
                .len(),
            1
        );
        assert_eq!(
            graph.expand(alice, EdgeDirection::PointingRight, Some("AUTHORED"))
                .expect("expand")
                .len(),
            1
        );
    }

    #[test]
    fn reopens_graph_from_memory_payload() {
        let memory = VecMemory::new();
        let mut graph = GraphPma::init(memory);
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("Hello".to_owned()))]);
        graph.insert_edge(alice, post, Some("AUTHORED"), [("weight", Value::Int64(10))]);

        let memory = graph.memory().clone();
        let reopened = GraphPma::open(memory).expect("graph should reopen");

        assert_eq!(reopened.scan_nodes(Some("User")).expect("scan").len(), 1);
        assert_eq!(
            reopened
                .scan_nodes_by_property("uid", &Value::Text("u1".to_owned()), gleaph_gql::ast::CmpOp::Eq)
                .expect("property scan")
                .len(),
            1
        );
        assert_eq!(
            reopened
                .expand(alice, EdgeDirection::PointingRight, Some("AUTHORED"))
                .expect("expand")
                .len(),
            1
        );
        assert_eq!(
            reopened
                .scan_edges_by_property("weight", &Value::Int64(10))
                .expect("edge property scan")
                .len(),
            1
        );
    }

    #[test]
    fn compaction_moves_log_into_base_snapshot() {
        let memory = VecMemory::new();
        let mut graph = GraphPma::init(memory);
        let alice = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("Hello".to_owned()))]);
        graph.insert_edge(alice, post, Some("AUTHORED"), [("weight", Value::Int64(10))]);

        let before = GraphLayoutHeader::read_from(graph.memory()).expect("header");
        assert!(before.op_log_bytes_len > 0);

        graph.compact();

        let after = GraphLayoutHeader::read_from(graph.memory()).expect("header");
        assert_eq!(after.op_log_bytes_len, 0);
        assert!(after.node_bytes_len > 0);
        assert!(after.edge_bytes_len > 0);

        let reopened = GraphPma::open(graph.memory().clone()).expect("reopen");
        assert_eq!(reopened.scan_nodes(Some("User")).expect("scan").len(), 1);
    }

    #[test]
    fn replays_mutation_ops_from_log() {
        let memory = VecMemory::new();
        let mut graph = GraphPma::init(memory);
        let user_labels = vec!["User".to_owned()];
        let post_labels = vec!["Post".to_owned()];
        let alice_props: PropertyMap =
            [("uid".to_owned(), Value::Text("u1".to_owned()))].into_iter().collect();
        let post_props: PropertyMap =
            [("title".to_owned(), Value::Text("Hello".to_owned()))].into_iter().collect();
        let edge_props: PropertyMap =
            [("weight".to_owned(), Value::Int64(10))].into_iter().collect();
        let alice = GraphWrite::insert_node(
            &mut graph,
            &user_labels,
            &alice_props,
        )
        .expect("insert node");
        let post = GraphWrite::insert_node(
            &mut graph,
            &post_labels,
            &post_props,
        )
        .expect("insert node");
        let edge = GraphWrite::insert_edge(
            &mut graph,
            alice.id,
            post.id,
            Some("AUTHORED"),
            &edge_props,
        )
        .expect("insert edge");

        GraphWrite::set_node_property(
            &mut graph,
            alice.id,
            "name",
            &Value::Text("Alice".to_owned()),
        )
        .expect("set node property");
        GraphWrite::remove_node_property(&mut graph, alice.id, "uid")
            .expect("remove node property");
        GraphWrite::add_node_label(&mut graph, alice.id, "Person")
            .expect("add node label");
        GraphWrite::remove_node_label(&mut graph, alice.id, "User")
            .expect("remove node label");
        GraphWrite::set_edge_property(
            &mut graph,
            edge.id,
            "score",
            &Value::Int64(99),
        )
        .expect("set edge property");
        GraphWrite::remove_edge_property(&mut graph, edge.id, "weight")
            .expect("remove edge property");
        GraphWrite::set_edge_label(&mut graph, edge.id, Some("WROTE"))
            .expect("set edge label");
        GraphWrite::delete_edge(&mut graph, edge.id).expect("delete edge");
        GraphWrite::delete_node(&mut graph, post.id, true).expect("detach delete node");

        let reopened = GraphPma::open(graph.memory().clone()).expect("graph should reopen");
        let alice = reopened.get_node(alice.id).expect("get node").expect("node exists");
        assert_eq!(alice.labels, vec!["Person".to_owned()]);
        assert_eq!(
            alice.properties.get("name"),
            Some(&Value::Text("Alice".to_owned()))
        );
        assert!(!alice.properties.contains_key("uid"));
        assert_eq!(reopened.get_node(post.id).expect("get node"), None);
        assert!(
            reopened
                .scan_edges_by_property("score", &Value::Int64(99))
                .expect("scan edges")
                .is_empty()
        );
        assert!(
            reopened
                .expand(alice.id, EdgeDirection::PointingRight, None)
                .expect("expand")
                .is_empty()
        );
    }

    #[test]
    fn auto_compacts_when_log_crosses_threshold() {
        let memory = VecMemory::new();
        let mut graph = GraphPma::init(memory);
        let user_labels = vec!["User".to_owned()];
        let alice_props: PropertyMap =
            [("uid".to_owned(), Value::Text("u1".to_owned()))].into_iter().collect();
        let alice = GraphWrite::insert_node(
            &mut graph,
            &user_labels,
            &alice_props,
        )
        .expect("insert node");

        for index in 0..32 {
            let property = format!("k{index}");
            let value = Value::Text("x".repeat(32));
            GraphWrite::set_node_property(
                &mut graph,
                alice.id,
                &property,
                &value,
            )
            .expect("set node property");
        }

        let header = GraphLayoutHeader::read_from(graph.memory()).expect("header");
        assert!(header.op_log_bytes_len < auto_compact_threshold_bytes(&header));
        assert!(header.node_bytes_len > 0);

        let reopened = GraphPma::open(graph.memory().clone()).expect("graph should reopen");
        let alice = reopened.get_node(alice.id).expect("get node").expect("node exists");
        assert_eq!(
            alice.properties.get("k31"),
            Some(&Value::Text("x".repeat(32)))
        );
    }

    #[test]
    fn auto_compact_threshold_scales_with_base_snapshot_size() {
        let small = GraphLayoutHeader {
            node_bytes_len: 100,
            edge_bytes_len: 200,
            op_log_bytes_len: 0,
            ..GraphLayoutHeader::new()
        };
        assert_eq!(auto_compact_threshold_bytes(&small), MIN_AUTO_COMPACT_LOG_BYTES);

        let large = GraphLayoutHeader {
            node_bytes_len: 4_000,
            edge_bytes_len: 2_000,
            op_log_bytes_len: 0,
            ..GraphLayoutHeader::new()
        };
        assert_eq!(auto_compact_threshold_bytes(&large), 3_000);
    }

    #[test]
    fn property_store_runtime_stays_in_sync_with_graph_mutations() {
        let memory = VecMemory::new();
        let mut graph = GraphPma::init(memory);
        let user = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let edge = graph.insert_edge(user, user, Some("SELF"), [("since", Value::Int64(2024))]);

        assert_eq!(
            graph.node_props_for_test(user).get("uid"),
            Some(&Value::Text("u1".to_owned()))
        );
        assert_eq!(
            graph.edge_props_for_test(edge).get("since"),
            Some(&Value::Int64(2024))
        );

        graph
            .set_node_property(user, "uid", &Value::Text("u2".to_owned()))
            .expect("set node property");
        graph
            .remove_edge_property(edge, "since")
            .expect("remove edge property");

        assert_eq!(
            graph.node_props_for_test(user).get("uid"),
            Some(&Value::Text("u2".to_owned()))
        );
        assert!(graph.edge_props_for_test(edge).is_empty());

        graph.delete_edge(edge).expect("delete edge");
        graph.delete_node(user, true).expect("delete node");

        assert!(graph.edge_props_for_test(edge).is_empty());
        assert!(graph.node_props_for_test(user).is_empty());
    }

    #[test]
    fn compact_persists_property_regions_for_abtree_backend() {
        let memory = VecMemory::new();
        let mut graph = GraphPmaBuilder::with_property_subsystem_backend(
            memory,
            PropertySubsystemBackendKind::AbTree,
        )
        .build();
        let user = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        graph.compact();

        let header = GraphLayoutHeader::read_from(graph.memory()).expect("header");
        assert!(header.adjacency_bytes_len > 0);
        assert!(header.property_bytes_len > 0);
        assert!(header.property_index_bytes_len > 0);
        assert_eq!(header.op_log_bytes_len, 0);

        let reopened = GraphPma::open(graph.memory().clone()).expect("reopen");
        assert_eq!(
            reopened
                .get_node(user)
                .expect("get node")
                .expect("node exists")
                .properties
                .get("uid"),
            Some(&Value::Text("u1".to_owned()))
        );
        assert_eq!(
            reopened
                .expand(user, EdgeDirection::AnyDirection, None)
                .expect("expand")
                .len(),
            0
        );
    }

    #[test]
    fn compact_persists_adjacency_region_for_default_backend() {
        let memory = VecMemory::new();
        let mut graph = GraphPma::init(memory);
        let user = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        let post = graph.insert_node(["Post"], [("title", Value::Text("Hello".to_owned()))]);
        graph.insert_edge(user, post, Some("AUTHORED"), [("weight", Value::Int64(1))]);
        graph.compact();

        let header = GraphLayoutHeader::read_from(graph.memory()).expect("header");
        assert!(header.adjacency_bytes_len > 0);

        let reopened = GraphPma::open(graph.memory().clone()).expect("reopen");
        assert_eq!(
            reopened
                .expand(user, EdgeDirection::PointingRight, Some("AUTHORED"))
                .expect("expand")
                .len(),
            1
        );
    }

    #[test]
    fn graph_builder_can_select_abtree_property_store_backend() {
        let memory = VecMemory::new();
        let mut graph = GraphPmaBuilder::with_property_subsystem_backend(
            memory,
            PropertySubsystemBackendKind::AbTree,
        )
            .build();
        assert_eq!(
            graph.property_store_backend_kind(),
            PropertyStoreBackendKind::AbTree
        );
        assert_eq!(
            graph.property_index_backend_kind(),
            PropertyIndexBackendKind::AbTree
        );
        assert_eq!(
            graph.property_subsystem_backend_kind(),
            Some(PropertySubsystemBackendKind::AbTree)
        );

        let user = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        assert_eq!(
            graph.node_props_for_test(user).get("uid"),
            Some(&Value::Text("u1".to_owned()))
        );
    }

    #[test]
    fn graph_can_migrate_property_store_backend_without_losing_data() {
        let memory = VecMemory::new();
        let mut graph = GraphPma::init(memory);
        let user = graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);
        graph.set_node_property(user, "name", &Value::Text("Alice".to_owned()))
            .expect("set node property");

        assert_eq!(
            graph.property_store_backend_kind(),
            PropertyStoreBackendKind::AppendLog
        );
        assert_eq!(
            graph.property_index_backend_kind(),
            PropertyIndexBackendKind::AppendLog
        );
        assert_eq!(
            graph.property_subsystem_backend_kind(),
            Some(PropertySubsystemBackendKind::AppendLog)
        );
        graph.migrate_property_subsystem_to_abtree();
        assert_eq!(
            graph.property_store_backend_kind(),
            PropertyStoreBackendKind::AbTree
        );
        assert_eq!(
            graph.property_index_backend_kind(),
            PropertyIndexBackendKind::AbTree
        );
        assert_eq!(
            graph.property_subsystem_backend_kind(),
            Some(PropertySubsystemBackendKind::AbTree)
        );
        assert_eq!(
            graph.node_props_for_test(user).get("uid"),
            Some(&Value::Text("u1".to_owned()))
        );
        assert_eq!(
            graph.node_props_for_test(user).get("name"),
            Some(&Value::Text("Alice".to_owned()))
        );
        assert_eq!(
            graph.scan_nodes_by_property("uid", &Value::Text("u1".to_owned()), CmpOp::Eq)
                .expect("scan nodes")
                .len(),
            1
        );
    }

    #[test]
    fn mixed_property_backend_selection_reports_no_unified_subsystem_kind() {
        let graph = GraphPmaBuilder::new(VecMemory::new())
            .property_store_backend(PropertyStoreBackendKind::AbTree)
            .property_index_backend(PropertyIndexBackendKind::AppendLog)
            .build();
        assert_eq!(graph.property_subsystem_backend_kind(), None);
    }
}
