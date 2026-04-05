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

/// One graph hop from [`GraphRead::expand_hops_with_shard_meta`], including optional shard metadata.
///
/// `shard_canister_principal` is raw Internet Computer principal bytes when the backing store
/// resolves a cross-canister stub edge; [`None`] for purely local hops or stores that do not expose shard principals.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpansionHop {
    pub expansion: Expansion,
    pub shard_canister_principal: Option<Vec<u8>>,
}
