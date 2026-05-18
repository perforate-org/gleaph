//! In-process equality postings for edge properties (owner + slot).
//!
//! Complements the federated vertex index: expand with `indexed_edge_equality` can
//! probe `(property_id, sortable value bytes)` and restrict adjacency enumeration
//! to matching edge slots without re-reading every edge property map.

use gleaph_gql::Value;
use gleaph_gql::value_to_index_key_bytes;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::VertexId;
use std::cell::RefCell;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EdgeEqualityPosting {
    pub owner_vertex_id: VertexId,
    pub label_id: u16,
    pub slot_index: u32,
}

type PostingKey = (u32, Vec<u8>);

thread_local! {
    static EDGE_EQUALITY: RefCell<BTreeMap<PostingKey, Vec<EdgeEqualityPosting>>> =
        const { RefCell::new(BTreeMap::new()) };
}

fn encode_value(value: &Value) -> Option<Vec<u8>> {
    value_to_index_key_bytes(value).ok().flatten()
}

fn posting_key(property_id: PropertyId, value_bytes: &[u8]) -> PostingKey {
    (property_id.raw(), value_bytes.to_vec())
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
    EDGE_EQUALITY.with(|index| {
        let mut map = index.borrow_mut();
        map.retain(|_, postings| {
            postings.retain(|p| {
                p.owner_vertex_id != owner_vertex_id
                    || p.label_id != label_id
                    || p.slot_index != slot_index
            });
            !postings.is_empty()
        });
    });
}

/// Returns `Some(postings)` when this `(property, value)` key is indexed locally
/// (including an empty slice when no edges carry the value).
pub(crate) fn lookup_equal(
    property_id: PropertyId,
    value_bytes: &[u8],
) -> Option<Vec<EdgeEqualityPosting>> {
    EDGE_EQUALITY.with(|index| {
        index
            .borrow()
            .get(&posting_key(property_id, value_bytes))
            .cloned()
    })
}

fn insert_posting(
    owner_vertex_id: VertexId,
    label_id: u16,
    slot_index: u32,
    property_id: PropertyId,
    value_bytes: Vec<u8>,
) {
    let posting = EdgeEqualityPosting {
        owner_vertex_id,
        label_id,
        slot_index,
    };
    EDGE_EQUALITY.with(|index| {
        index
            .borrow_mut()
            .entry(posting_key(property_id, &value_bytes))
            .or_default()
            .push(posting);
    });
}

fn remove_posting(
    owner_vertex_id: VertexId,
    label_id: u16,
    slot_index: u32,
    property_id: PropertyId,
    value_bytes: Vec<u8>,
) {
    EDGE_EQUALITY.with(|index| {
        let key = posting_key(property_id, &value_bytes);
        let mut map = index.borrow_mut();
        if let Some(postings) = map.get_mut(&key) {
            postings.retain(|p| {
                p.owner_vertex_id != owner_vertex_id
                    || p.label_id != label_id
                    || p.slot_index != slot_index
            });
            if postings.is_empty() {
                map.remove(&key);
            }
        }
    });
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
