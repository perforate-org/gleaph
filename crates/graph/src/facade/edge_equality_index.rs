//! Stable equality postings for edge properties (owner + slot).
//!
//! Complements the federated vertex index: expand with `indexed_edge_equality` can
//! probe `(property_id, sortable value bytes)` and restrict adjacency enumeration
//! to matching edge slots without re-reading every edge property map.

use super::stable::edge_equality_postings::EdgeEqualityPostingKey;
use super::stable::{EDGE_EQUALITY_POSTINGS, EDGE_PROPERTIES};
use gleaph_gql::Value;
use gleaph_gql::value_to_index_key_bytes;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::VertexId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EdgeEqualityPosting {
    pub owner_vertex_id: VertexId,
    pub label_id: u16,
    pub slot_index: u32,
}

fn encode_value(value: &Value) -> Option<Vec<u8>> {
    value_to_index_key_bytes(value).ok().flatten()
}

pub(crate) fn record_edge_property_change(
    owner_vertex_id: VertexId,
    label_id: u16,
    slot_index: u32,
    property_id: PropertyId,
    prev: Option<&Value>,
    new: Option<&Value>,
) {
    match (prev, new) {
        (None, Some(n)) => {
            if let Some(bytes) = encode_value(n) {
                insert_posting(owner_vertex_id, label_id, slot_index, property_id, bytes);
            }
        }
        (Some(p), Some(n)) if p != n => {
            if let Some(old_bytes) = encode_value(p) {
                remove_posting(
                    owner_vertex_id,
                    label_id,
                    slot_index,
                    property_id,
                    old_bytes,
                );
            }
            if let Some(new_bytes) = encode_value(n) {
                insert_posting(
                    owner_vertex_id,
                    label_id,
                    slot_index,
                    property_id,
                    new_bytes,
                );
            }
        }
        (Some(p), None) => {
            if let Some(bytes) = encode_value(p) {
                remove_posting(owner_vertex_id, label_id, slot_index, property_id, bytes);
            }
        }
        _ => {}
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
                if let Some(bytes) = encode_value(&value) {
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

        record_edge_property_change(owner, 0, slot, pid, None, Some(&Value::Int64(5)));
        let hits = lookup_equal(pid, &encode_value(&Value::Int64(5)).unwrap()).unwrap();
        assert_eq!(hits.len(), 1);

        record_edge_property_change(
            owner,
            0,
            slot,
            pid,
            Some(&Value::Int64(5)),
            Some(&Value::Int64(9)),
        );
        assert!(lookup_equal(pid, &encode_value(&Value::Int64(5)).unwrap()).is_none());
        assert_eq!(
            lookup_equal(pid, &encode_value(&Value::Int64(9)).unwrap())
                .unwrap()
                .len(),
            1
        );

        record_edge_property_change(owner, 0, slot, pid, Some(&Value::Int64(9)), None);
        assert!(lookup_equal(pid, &encode_value(&Value::Int64(9)).unwrap()).is_none());
    }
}
