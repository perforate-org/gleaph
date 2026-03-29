use gleaph_graph_kernel::{EdgeId, NodeId};

const NODE_PROP_TAG: u8 = 0x01;
const EDGE_PROP_TAG: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PropId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodePropKey {
    pub node_id: NodeId,
    pub prop_id: PropId,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgePropKey {
    pub edge_id: EdgeId,
    pub prop_id: PropId,
}

impl NodePropKey {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 6 + 4);
        out.push(NODE_PROP_TAG);
        out.extend_from_slice(&self.node_id.to_be_bytes());
        out.extend_from_slice(&self.prop_id.0.to_be_bytes());
        out
    }
}

impl EdgePropKey {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 8 + 4);
        out.push(EDGE_PROP_TAG);
        out.extend_from_slice(&self.edge_id.to_be_bytes());
        out.extend_from_slice(&self.prop_id.0.to_be_bytes());
        out
    }
}

pub fn node_prefix(node_id: NodeId) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 6);
    out.push(NODE_PROP_TAG);
    out.extend_from_slice(&node_id.to_be_bytes());
    out
}

pub fn edge_prefix(edge_id: EdgeId) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 8);
    out.push(EDGE_PROP_TAG);
    out.extend_from_slice(&edge_id.to_be_bytes());
    out
}

pub fn decode_node_prop_id(bytes: &[u8]) -> Option<PropId> {
    if bytes.len() != 1 + 6 + 4 || bytes.first().copied()? != NODE_PROP_TAG {
        return None;
    }
    Some(PropId(u32::from_be_bytes(bytes[7..11].try_into().ok()?)))
}

pub fn decode_edge_prop_id(bytes: &[u8]) -> Option<PropId> {
    if bytes.len() != 1 + 8 + 4 || bytes.first().copied()? != EDGE_PROP_TAG {
        return None;
    }
    Some(PropId(u32::from_be_bytes(bytes[9..13].try_into().ok()?)))
}

pub fn encode_property_index_key(property: &str, value_bytes: &[u8]) -> Vec<u8> {
    let property_bytes = property.as_bytes();
    let mut out = Vec::with_capacity(2 + property_bytes.len() + value_bytes.len());
    out.extend_from_slice(&(property_bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(property_bytes);
    out.extend_from_slice(value_bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::{EdgePropKey, NodePropKey, PropId};
    use gleaph_graph_kernel::NodeId;

    #[test]
    fn node_property_keys_are_order_preserving() {
        let a = NodePropKey {
            node_id: NodeId::from(1u8),
            prop_id: PropId(1),
        };
        let b = NodePropKey {
            node_id: NodeId::from(1u8),
            prop_id: PropId(2),
        };
        let c = NodePropKey {
            node_id: NodeId::from(2u8),
            prop_id: PropId(1),
        };
        assert!(a.encode() < b.encode());
        assert!(b.encode() < c.encode());
    }

    #[test]
    fn edge_keys_have_distinct_prefix() {
        let node = NodePropKey {
            node_id: NodeId::from(7u8),
            prop_id: PropId(3),
        };
        let edge = EdgePropKey {
            edge_id: 7,
            prop_id: PropId(3),
        };
        assert_ne!(node.encode()[0], edge.encode()[0]);
    }
}
