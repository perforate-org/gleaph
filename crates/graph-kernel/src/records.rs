use std::collections::BTreeMap;

use gleaph_gql::Value;

use crate::{EdgeId, NodeId};

pub type PropertyMap = BTreeMap<String, Value>;

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
