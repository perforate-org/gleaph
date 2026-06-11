//! Plan execution bindings: edge handles with stored payload bytes.

use std::sync::Arc;

use gleaph_gql::Value;
use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::entry::{Edge, EdgePayload};
use gleaph_graph_kernel::federation::{FederatedExpandNeighbor, ShardId};
use ic_stable_lara::VertexId;
use ic_stable_lara::traits::CsrEdge;

use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};

use super::super::error::PlanQueryError;

/// Edge variable binding for one traversal hop: stable handle plus stored payload bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeBinding {
    pub handle: EdgeHandle,
    pub payload: EdgePayload,
}

impl EdgeBinding {
    #[inline]
    pub fn payload_bytes_slice(&self) -> &[u8] {
        self.payload.as_slice()
    }

    #[inline]
    pub fn payload_len(&self) -> usize {
        self.payload.len()
    }

    pub fn from_edge(handle: EdgeHandle, edge: Edge) -> Self {
        Self {
            handle,
            payload: edge.payload,
        }
    }

    /// Edge binding from a federated wire hit on another shard (no local CSR hydration).
    pub fn from_federated_neighbor_hit(hit: &FederatedExpandNeighbor) -> Self {
        Self {
            handle: edge_handle_from_federated_hit_wire(hit),
            payload: hit.payload(),
        }
    }

    pub fn from_federated_hit(
        store: &GraphStore,
        hit: &FederatedExpandNeighbor,
    ) -> Result<Self, GraphStoreError> {
        let handle = edge_handle_for_federated_hit(store, hit)?;
        if let Some(edge) = store.find_outgoing_edge_record(handle)? {
            return Ok(Self::from_edge(handle, edge));
        }
        Ok(Self::from_federated_neighbor_hit(hit))
    }
}

fn edge_handle_from_federated_hit_wire(hit: &FederatedExpandNeighbor) -> EdgeHandle {
    let label_id = ic_stable_lara::BucketLabelKey::from_raw(hit.label_id_raw);
    let owner_vertex_id = if hit.anchor_local_vertex_id == 0 {
        VertexId::from(hit.neighbor_local_vertex_id)
    } else {
        VertexId::from(hit.anchor_local_vertex_id)
    };
    EdgeHandle {
        owner_vertex_id,
        label_id,
        slot_index: hit.slot_index,
    }
}

fn edge_handle_for_federated_hit(
    store: &GraphStore,
    hit: &FederatedExpandNeighbor,
) -> Result<EdgeHandle, GraphStoreError> {
    let wire = edge_handle_from_federated_hit_wire(hit);
    let anchor = VertexId::from(hit.anchor_local_vertex_id);
    let neighbor = VertexId::from(hit.neighbor_local_vertex_id);
    if hit.anchor_local_vertex_id == 0 {
        return Ok(wire);
    }
    if store.vertex(anchor).is_some() {
        let at_anchor = EdgeHandle {
            owner_vertex_id: anchor,
            label_id: wire.label_id,
            slot_index: wire.slot_index,
        };
        if store
            .find_outgoing_edge_record(at_anchor)?
            .is_some_and(|edge| edge.neighbor_vid() == neighbor)
        {
            return Ok(at_anchor);
        }
        if store.vertex(neighbor).is_some() {
            let at_neighbor = EdgeHandle {
                owner_vertex_id: neighbor,
                label_id: wire.label_id,
                slot_index: wire.slot_index,
            };
            if store.find_outgoing_edge_record(at_neighbor)?.is_some() {
                return Ok(at_neighbor);
            }
        }
    }
    Ok(wire)
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

/// Per-hop auxiliary scalar for `{edge}__hop_aux` (inline edge payload bytes).
pub(crate) fn hop_aux_scalar(edge: &EdgeBinding) -> Value {
    let bytes = edge.payload_bytes_slice();
    if bytes.is_empty() {
        Value::Null
    } else {
        Value::Bytes(bytes.to_vec())
    }
}

/// Per-hop auxiliary group for variable-length `{edge}__hop_aux`.
pub(crate) fn hop_aux_group(edges: &[EdgeBinding]) -> Value {
    Value::List(edges.iter().map(hop_aux_scalar).collect())
}

/// Collect traversed edges for a variable-length path state, in hop order (first → last).
pub(crate) fn edge_bindings_along_var_len_path<T>(
    states: &[T],
    state_idx: usize,
    edge: impl Fn(&T) -> Option<&EdgeBinding>,
    previous: impl Fn(&T) -> Option<usize>,
) -> Arc<[EdgeBinding]> {
    let mut edges = Vec::new();
    let mut idx = state_idx;
    loop {
        let state = &states[idx];
        if let Some(e) = edge(state) {
            edges.push(e.clone());
        }
        let Some(prev) = previous(state) else {
            break;
        };
        idx = prev;
    }
    edges.reverse();
    edges.into()
}

/// Collect per-hop **near** or **far** endpoint vertices for a variable-length path.
pub(crate) fn vertices_along_var_len_path<T>(
    states: &[T],
    state_idx: usize,
    vertex_at: impl Fn(&T) -> VertexId,
    hop_taken: impl Fn(&T) -> bool,
    previous: impl Fn(&T) -> Option<usize>,
    near_endpoints: bool,
) -> Arc<[VertexId]> {
    let mut vertices = Vec::new();
    let mut idx = state_idx;
    loop {
        let state = &states[idx];
        if hop_taken(state) {
            let Some(prev) = previous(state) else {
                break;
            };
            let v = if near_endpoints {
                vertex_at(&states[prev])
            } else {
                vertex_at(state)
            };
            vertices.push(v);
            idx = prev;
        } else {
            let Some(prev) = previous(state) else {
                break;
            };
            idx = prev;
        }
    }
    vertices.reverse();
    vertices.into()
}

pub(crate) fn vertex_group_element_at_index(vertices: &[VertexId], index: i64) -> Option<VertexId> {
    if vertices.is_empty() {
        return None;
    }
    let len = vertices.len() as i64;
    let idx = if index < 0 { len + index } else { index };
    usize::try_from(idx)
        .ok()
        .and_then(|i| vertices.get(i))
        .copied()
}

/// Resolve a list index for an edge group (`0` / `-1` supported).
pub(crate) fn path_group_element_at_index(
    paths: &[super::path::PathBinding],
    index: i64,
) -> Option<&super::path::PathBinding> {
    if paths.is_empty() {
        return None;
    }
    let len = paths.len() as i64;
    let idx = if index < 0 { len + index } else { index };
    usize::try_from(idx).ok().and_then(|i| paths.get(i))
}

pub(crate) fn edge_group_element_at_index(
    edges: &[EdgeBinding],
    index: i64,
) -> Option<&EdgeBinding> {
    if edges.is_empty() {
        return None;
    }
    let len = edges.len() as i64;
    let idx = if index < 0 { len + index } else { index };
    usize::try_from(idx).ok().and_then(|i| edges.get(i))
}
#[cfg(test)]
mod tests {
    use super::super::test_support::*;

    #[test]
    fn reverse_expand_binding_resolves_forward_edge_payload_and_owner() {
        let store = GraphStore::new();
        let a = store.insert_vertex().expect("a");
        let b = store.insert_vertex().expect("b");
        let label_id = crate::test_labels::edge_label_id_for_name("RevExpandWgt");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                gleaph_graph_kernel::entry::EdgeWeightProfile {
                    encoding: gleaph_graph_kernel::entry::WeightEncoding::RawU16,
                },
            )
            .expect("profile");
        store
            .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &42u16.to_le_bytes())
            .expect("edge");

        let in_edge = store.directed_in_edges(b).expect("in edges")[0].clone();
        let binding = edge_binding_for_expand(&store, b, EdgeDirection::PointingLeft, in_edge)
            .expect("binding");
        assert_eq!(binding.handle.owner_vertex_id, a);
        assert_eq!(binding.payload_bytes_slice(), &[42, 0]);

        let weight = crate::plan::query::gleaph_weight::decode_traversal_edge_weight(
            &store,
            binding.handle,
            binding.payload_len(),
            binding.payload_bytes_slice(),
        )
        .expect("decode weight");
        assert_eq!(weight, 42.0);
    }
}
