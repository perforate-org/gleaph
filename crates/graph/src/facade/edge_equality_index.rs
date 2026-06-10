//! Stable equality postings for edge properties (owner + slot).
//!
//! Complements the federated vertex index: expand with `indexed_edge_equality` can
//! probe `(property_id, sortable value bytes)` and restrict adjacency enumeration
//! to matching edge slots without re-reading every edge property map.

use super::stable::edge_equality_postings::EdgeEqualityPostingKey;
use super::stable::{EDGE_EQUALITY_POSTINGS, EDGE_PROPERTIES};
use crate::property::{PropertyIndexOp, PropertyValueChange, sortable_index_key};
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::VertexId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EdgeEqualityPosting {
    pub owner_vertex_id: VertexId,
    pub label_id: u16,
    pub slot_index: u32,
}

pub(crate) fn apply_edge_index_op(
    owner_vertex_id: VertexId,
    label_id: u16,
    slot_index: u32,
    op: PropertyIndexOp,
) {
    match op {
        PropertyIndexOp::Insert {
            property_id,
            payload_bytes,
        } => insert_posting(
            owner_vertex_id,
            label_id,
            slot_index,
            property_id,
            payload_bytes,
        ),
        PropertyIndexOp::Remove {
            property_id,
            payload_bytes,
        } => remove_posting(
            owner_vertex_id,
            label_id,
            slot_index,
            property_id,
            payload_bytes,
        ),
    }
}

pub(crate) fn remove_all_for_edge(owner_vertex_id: VertexId, label_id: u16, slot_index: u32) {
    let keys: Vec<_> = EDGE_PROPERTIES.with_borrow(|properties| {
        let mut keys = Vec::new();
        properties.for_each_property_for_edge(
            owner_vertex_id,
            label_id,
            slot_index,
            |pid, value| {
                if let Some(bytes) = sortable_index_key(&value) {
                    keys.push(EdgeEqualityPostingKey::new(
                        pid,
                        &bytes,
                        owner_vertex_id,
                        label_id,
                        slot_index,
                    ));
                }
            },
        );
        keys
    });
    EDGE_EQUALITY_POSTINGS.with_borrow_mut(|index| {
        for key in keys {
            index.remove(&key);
        }
    });
}

/// Returns `Some(postings)` when postings exist for `(property, value)` in stable storage.
pub(crate) fn lookup_equal(
    property_id: PropertyId,
    payload_bytes: &[u8],
) -> Option<Vec<EdgeEqualityPosting>> {
    let keys =
        EDGE_EQUALITY_POSTINGS.with_borrow(|index| index.lookup_range(property_id, payload_bytes));
    if keys.is_empty() {
        return None;
    }
    Some(
        keys.into_iter()
            .map(
                |EdgeEqualityPostingKey {
                     owner_vertex_id,
                     label_id,
                     slot_index,
                     ..
                 }| EdgeEqualityPosting {
                    owner_vertex_id: VertexId::from(owner_vertex_id),
                    label_id,
                    slot_index,
                },
            )
            .collect(),
    )
}

fn insert_posting(
    owner_vertex_id: VertexId,
    label_id: u16,
    slot_index: u32,
    property_id: PropertyId,
    payload_bytes: Vec<u8>,
) {
    let key = EdgeEqualityPostingKey::new(
        property_id,
        &payload_bytes,
        owner_vertex_id,
        label_id,
        slot_index,
    );
    EDGE_EQUALITY_POSTINGS.with_borrow_mut(|index| index.insert(key));
}

fn remove_posting(
    owner_vertex_id: VertexId,
    label_id: u16,
    slot_index: u32,
    property_id: PropertyId,
    payload_bytes: Vec<u8>,
) {
    let key = EdgeEqualityPostingKey::new(
        property_id,
        &payload_bytes,
        owner_vertex_id,
        label_id,
        slot_index,
    );
    EDGE_EQUALITY_POSTINGS.with_borrow_mut(|index| index.remove(&key));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;

    #[test]
    fn lookup_tracks_insert_update_remove() {
        let owner = VertexId::from(9u32);
        let slot = 3;
        let pid = PropertyId::from_raw(7);

        crate::property::dispatch_property_index_ops(PropertyValueChange::edge(
            owner,
            0,
            slot,
            pid,
            None,
            Some(&Value::Int64(5)),
        ));
        let hits = lookup_equal(pid, &sortable_index_key(&Value::Int64(5)).unwrap()).unwrap();
        assert_eq!(hits.len(), 1);

        crate::property::dispatch_property_index_ops(PropertyValueChange::edge(
            owner,
            0,
            slot,
            pid,
            Some(&Value::Int64(5)),
            Some(&Value::Int64(9)),
        ));
        assert!(lookup_equal(pid, &sortable_index_key(&Value::Int64(5)).unwrap()).is_none());
        assert_eq!(
            lookup_equal(pid, &sortable_index_key(&Value::Int64(9)).unwrap())
                .unwrap()
                .len(),
            1
        );

        crate::property::dispatch_property_index_ops(PropertyValueChange::edge(
            owner,
            0,
            slot,
            pid,
            Some(&Value::Int64(9)),
            None,
        ));
        assert!(lookup_equal(pid, &sortable_index_key(&Value::Int64(9)).unwrap()).is_none());
    }
}
