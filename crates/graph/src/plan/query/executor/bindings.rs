//! Plan execution bindings: edge handles with stored value bytes.

use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::entry::{Edge, EdgeDirectedness, EdgeLabelId, EdgeValuePayload};
use gleaph_graph_kernel::federation::{FederatedExpandNeighbor, ShardId};

use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};

use super::super::error::PlanQueryError;

/// Edge variable binding for one traversal hop: stable handle plus stored value bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeBinding {
    pub handle: EdgeHandle,
    pub value: EdgeValuePayload,
}

impl EdgeBinding {
    #[inline]
    pub fn value_bytes_slice(&self) -> &[u8] {
        self.value.as_slice()
    }

    #[inline]
    pub fn value_len(&self) -> usize {
        self.value.len()
    }

    #[inline]
    pub fn inline_value(&self) -> u16 {
        self.value.inline_u16()
    }

    pub fn from_edge(handle: EdgeHandle, edge: Edge) -> Self {
        Self {
            handle,
            value: edge.value,
        }
    }

    /// Edge binding from a federated wire hit on another shard (no local CSR hydration).
    pub fn from_federated_neighbor_hit(hit: &FederatedExpandNeighbor) -> Self {
        Self {
            handle: crate::facade::federation_expand::edge_handle_from_federated_hit_wire(hit),
            value: hit.value_payload(),
        }
    }

    pub fn from_federated_hit(
        store: &GraphStore,
        hit: &FederatedExpandNeighbor,
    ) -> Result<Self, GraphStoreError> {
        let handle = crate::facade::federation_expand::edge_handle_for_federated_hit(store, hit)?;
        if let Some(edge) = store.find_outgoing_edge_record(handle)? {
            return Ok(Self::from_edge(handle, edge));
        }
        Ok(Self::from_federated_neighbor_hit(hit))
    }
}

pub(crate) fn edge_binding_for_federated_expand_hit(
    store: &GraphStore,
    hit: &FederatedExpandNeighbor,
    local_shard_id: ShardId,
) -> Result<EdgeBinding, PlanQueryError> {
    if hit.shard_id == local_shard_id {
        EdgeBinding::from_federated_hit(store, hit).map_err(PlanQueryError::from)
    } else {
        Ok(EdgeBinding::from_federated_neighbor_hit(hit))
    }
}

pub(crate) fn federated_expand_label_id_raw(
    label_id: Option<EdgeLabelId>,
    direction: EdgeDirection,
) -> Option<u16> {
    label_id.map(|lid| {
        let directedness = match direction {
            EdgeDirection::Undirected => EdgeDirectedness::Undirected,
            EdgeDirection::PointingLeft | EdgeDirection::PointingRight => {
                EdgeDirectedness::Directed
            }
            _ => EdgeDirectedness::Directed,
        };
        lid.pack(directedness).raw()
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use pollster;
    #[test]
    fn reverse_expand_binding_resolves_forward_edge_value_and_owner() {
        let store = GraphStore::new();
        let a = store.insert_vertex().expect("a");
        let b = store.insert_vertex().expect("b");
        let label_id = store
            .get_or_insert_edge_label_id("RevExpandWgt")
            .expect("label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                gleaph_graph_kernel::entry::EdgeWeightProfile {
                    encoding: gleaph_graph_kernel::entry::WeightEncoding::RawU16,
                },
            )
            .expect("profile");
        store
            .insert_directed_edge_with_inline_value(a, b, Some(label_id), 42)
            .expect("edge");

        let in_edge = store.directed_in_edges(b).expect("in edges")[0].clone();
        let binding = edge_binding_for_expand(&store, b, EdgeDirection::PointingLeft, in_edge)
            .expect("binding");
        assert_eq!(binding.handle.owner_vertex_id, a);
        assert_eq!(binding.value_bytes_slice(), &[42, 0]);

        let weight = crate::plan::query::gleaph_weight::decode_traversal_edge_weight(
            &store,
            binding.handle,
            binding.value_len(),
            binding.value_bytes_slice(),
            binding.inline_value(),
            None,
        )
        .expect("decode weight");
        assert_eq!(weight, 42.0);
    }
}
