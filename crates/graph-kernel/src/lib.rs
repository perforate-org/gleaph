use std::collections::BTreeMap;
use std::fmt;
use std::ops::AddAssign;

use gleaph_gql::types::EdgeDirection;
use gleaph_gql::ast::CmpOp;
use gleaph_gql::Value;
use thiserror::Error;

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NodeId([u8; 6]);

pub type EdgeId = u64;
pub type LabelId = u16;
pub type PropertyMap = BTreeMap<String, Value>;

impl NodeId {
    pub const MAX: u64 = (1u64 << 48) - 1;

    pub const fn new(bytes: [u8; 6]) -> Self {
        Self(bytes)
    }

    pub fn to_u64(self) -> u64 {
        let [b0, b1, b2, b3, b4, b5] = self.0;
        u64::from_be_bytes([0, 0, b0, b1, b2, b3, b4, b5])
    }

    pub fn checked_next(self) -> Option<Self> {
        self.to_u64().checked_add(1).and_then(|value| Self::try_from(value).ok())
    }

    pub fn as_bytes(self) -> [u8; 6] {
        self.0
    }

    pub fn to_be_bytes(self) -> [u8; 6] {
        self.0
    }
}

impl TryFrom<u64> for NodeId {
    type Error = NodeIdOverflow;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value > Self::MAX {
            return Err(NodeIdOverflow(value));
        }
        let bytes = value.to_be_bytes();
        Ok(Self([bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]))
    }
}

impl From<NodeId> for u64 {
    fn from(value: NodeId) -> Self {
        value.to_u64()
    }
}

impl From<u8> for NodeId {
    fn from(value: u8) -> Self {
        Self::try_from(value as u64).expect("u8 always fits in NodeId")
    }
}

impl From<u16> for NodeId {
    fn from(value: u16) -> Self {
        Self::try_from(value as u64).expect("u16 always fits in NodeId")
    }
}

impl From<u32> for NodeId {
    fn from(value: u32) -> Self {
        Self::try_from(value as u64).expect("u32 always fits in NodeId")
    }
}

impl AddAssign<u64> for NodeId {
    fn add_assign(&mut self, rhs: u64) {
        *self = Self::try_from(self.to_u64().checked_add(rhs).expect("NodeId overflow"))
            .expect("NodeId overflow");
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.to_u64(), f)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.to_u64(), f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeIdOverflow(pub u64);

#[derive(Clone, Debug, PartialEq)]
pub struct NodeRecord {
    pub id: NodeId,
    pub labels: Vec<String>,
    pub properties: PropertyMap,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EdgeRecord {
    pub id: EdgeId,
    pub src: NodeId,
    pub dst: NodeId,
    pub label: Option<String>,
    pub properties: PropertyMap,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expansion {
    pub edge: EdgeRecord,
    pub node: NodeRecord,
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("node {0} not found")]
    NodeNotFound(NodeId),
    #[error("edge {0} not found")]
    EdgeNotFound(EdgeId),
    #[error("{0}")]
    Message(String),
}

pub type GraphResult<T> = Result<T, GraphError>;

pub trait GraphRead {
    fn scan_nodes(&self, label: Option<&str>) -> GraphResult<Vec<NodeRecord>>;

    fn scan_nodes_by_property(
        &self,
        property: &str,
        value: &Value,
        cmp: CmpOp,
    ) -> GraphResult<Vec<NodeRecord>>;

    fn scan_edges_by_property(
        &self,
        property: &str,
        value: &Value,
    ) -> GraphResult<Vec<EdgeRecord>>;

    fn expand(
        &self,
        from: NodeId,
        direction: EdgeDirection,
        label: Option<&str>,
    ) -> GraphResult<Vec<Expansion>>;

    fn get_node(&self, id: NodeId) -> GraphResult<Option<NodeRecord>>;
}

pub trait GraphWrite {
    fn insert_node(
        &mut self,
        labels: &[String],
        properties: &PropertyMap,
    ) -> GraphResult<NodeRecord>;

    fn insert_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord>;

    fn set_node_property(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<NodeRecord>;

    fn remove_node_property(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> GraphResult<NodeRecord>;

    fn add_node_label(
        &mut self,
        node_id: NodeId,
        label: &str,
    ) -> GraphResult<NodeRecord>;

    fn remove_node_label(
        &mut self,
        node_id: NodeId,
        label: &str,
    ) -> GraphResult<NodeRecord>;

    fn set_edge_property(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<EdgeRecord>;

    fn remove_edge_property(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> GraphResult<EdgeRecord>;

    fn set_edge_label(
        &mut self,
        edge_id: EdgeId,
        label: Option<&str>,
    ) -> GraphResult<EdgeRecord>;

    fn delete_edge(
        &mut self,
        edge_id: EdgeId,
    ) -> GraphResult<()>;

    fn delete_node(
        &mut self,
        node_id: NodeId,
        detach: bool,
    ) -> GraphResult<()>;
}

const _: () = assert!(core::mem::size_of::<NodeId>() == 6);
