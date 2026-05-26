use std::cell::RefCell;

use gleaph_gql::Value;
use gleaph_gql::types::PathElement;
use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeSlotIndex};
use gleaph_graph_kernel::path::{GraphPathEdgeId, GraphPathVertexId};
use ic_stable_lara::VertexId;

use crate::facade::{EdgeHandle, GraphStore};
use crate::plan::query::error::PlanQueryError;
use super::{PathBinding, PathSearchNode};

thread_local! {
    /// Reuses capacity when materializing many shortest-path rows on one thread (e.g. `AllShortest`).
    static PATH_MATERIALIZE_SCRATCH: RefCell<Vec<PathElement>> = const { RefCell::new(Vec::new()) };
}

/// Below this estimated element count (`depth * 2 + 1`), allocate a fresh `Vec` only: `thread_local`
/// lookup + `RefCell` borrow win nothing on tiny paths and showed up as instruction regressions in
/// `plan_query_materialize_value_rows` benches.
const PATH_MATERIALIZE_SCRATCH_MIN_ELEMENTS: usize = 16;

pub(crate) fn path_binding_to_value(store: &GraphStore, pb: &PathBinding) -> Value {
    materialize_path_from_search_states(store, pb.shard_id, pb.states.as_ref(), pb.leaf_state_idx)
}

pub(crate) fn materialize_path_from_search_states(
    store: &GraphStore,
    shard_id: gleaph_graph_kernel::federation::ShardId,
    states: &[PathSearchNode],
    state_idx: usize,
) -> Value {
    let depth = states[state_idx].depth as usize;
    let min_cap = depth.saturating_mul(2).saturating_add(1);
    if min_cap < PATH_MATERIALIZE_SCRATCH_MIN_ELEMENTS {
        let mut elements = Vec::with_capacity(min_cap);
        fill_path_elements_leaf_to_root(store, shard_id, states, state_idx, &mut elements);
        return Value::Path(elements);
    }
    PATH_MATERIALIZE_SCRATCH.with(|scratch| {
        if let Ok(mut elements) = scratch.try_borrow_mut() {
            fill_path_elements_leaf_to_root(store, shard_id, states, state_idx, &mut elements);
            return Value::Path(std::mem::take(&mut *elements));
        }
        let mut elements = Vec::with_capacity(min_cap);
        fill_path_elements_leaf_to_root(store, shard_id, states, state_idx, &mut elements);
        Value::Path(elements)
    })
}

fn fill_path_elements_leaf_to_root(
    store: &GraphStore,
    shard_id: gleaph_graph_kernel::federation::ShardId,
    states: &[PathSearchNode],
    state_idx: usize,
    elements: &mut Vec<PathElement>,
) {
    elements.clear();
    let mut chain: Vec<usize> = Vec::new();
    let mut idx = state_idx;
    loop {
        chain.push(idx);
        let Some(previous) = states[idx].previous else {
            break;
        };
        idx = previous;
    }
    let cap = chain.len() * 2;
    if elements.capacity() < cap {
        elements.reserve(cap.saturating_sub(elements.capacity()));
    }
    for (hop, &si) in chain.iter().rev().enumerate() {
        let state = &states[si];
        if hop > 0
            && let Some(edge_binding) = state.edge
        {
            elements.push(edge_path_element(shard_id, edge_binding.handle));
        }
        elements.push(vertex_path_element(store, state.current));
    }
}

pub(crate) fn local_shard_id(store: &GraphStore) -> gleaph_graph_kernel::federation::ShardId {
    store.federation_routing().map(|r| r.shard_id).unwrap_or(0)
}

pub(crate) fn vertex_element_id_bytes(
    store: &GraphStore,
    vertex_id: VertexId,
) -> Result<Vec<u8>, PlanQueryError> {
    let path_id =
        store
            .path_vertex_element_id(vertex_id)
            .ok_or_else(|| PlanQueryError::MissingBinding {
                variable: format!("logical vertex id for {vertex_id:?}"),
            })?;
    Ok(path_id.to_bytes().to_vec())
}

pub(crate) fn edge_element_id_bytes(
    shard_id: gleaph_graph_kernel::federation::ShardId,
    owner_vertex_id: VertexId,
    edge_slot_index: gleaph_graph_kernel::entry::EdgeSlotIndex,
) -> Vec<u8> {
    GraphPathEdgeId::new(shard_id, owner_vertex_id, edge_slot_index)
        .to_bytes()
        .to_vec()
}

fn vertex_path_element(store: &GraphStore, vertex_id: VertexId) -> PathElement {
    let path_id = if store.federation_configured() {
        store.path_vertex_element_id(vertex_id).unwrap_or_else(|| {
            GraphPathVertexId::new(
                gleaph_graph_kernel::federation::standalone_logical_vertex_id(vertex_id),
            )
        })
    } else {
        GraphPathVertexId::new(
            gleaph_graph_kernel::federation::standalone_logical_vertex_id(vertex_id),
        )
    };
    PathElement::Vertex(path_id.to_bytes().into())
}

fn edge_path_element(
    shard_id: gleaph_graph_kernel::federation::ShardId,
    handle: EdgeHandle,
) -> PathElement {
    PathElement::Edge(
        GraphPathEdgeId::new(
            shard_id,
            handle.owner_vertex_id,
            EdgeSlotIndex::from_raw(handle.slot_index),
        )
        .to_bytes()
        .into(),
    )
}

