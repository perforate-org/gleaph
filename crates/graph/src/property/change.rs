//! Typed property value changes and derived index-maintenance operations.

use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{PropertyEntity, PropertyId};
use ic_stable_lara::VertexId;

use super::index_key::sortable_index_key;

/// One index posting insert or remove implied by a property value transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PropertyIndexOp {
    Insert {
        property_id: PropertyId,
        payload_bytes: Vec<u8>,
    },
    Remove {
        property_id: PropertyId,
        payload_bytes: Vec<u8>,
    },
}

/// Primary-store property transition with explicit host identity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PropertyValueChange<'a> {
    pub entity: PropertyEntity,
    pub property_id: PropertyId,
    pub prev: Option<&'a Value>,
    pub new: Option<&'a Value>,
}

impl<'a> PropertyValueChange<'a> {
    pub(crate) fn vertex(
        vertex_id: VertexId,
        property_id: PropertyId,
        prev: Option<&'a Value>,
        new: Option<&'a Value>,
    ) -> Self {
        Self {
            entity: PropertyEntity::vertex(vertex_id),
            property_id,
            prev,
            new,
        }
    }

    pub(crate) fn edge(
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
        property_id: PropertyId,
        prev: Option<&'a Value>,
        new: Option<&'a Value>,
    ) -> Self {
        Self {
            entity: PropertyEntity::edge(owner_vertex_id, label_id, slot_index),
            property_id,
            prev,
            new,
        }
    }
}

/// Derives index posting operations from a property value transition.
///
/// Unindexable snapshots (`sortable_index_key` returns `None`) produce no ops for
/// that snapshot, matching federated vertex-index and local edge-equality behavior.
pub(crate) fn index_ops_for_value_change(
    property_id: PropertyId,
    prev: Option<&Value>,
    new: Option<&Value>,
) -> Vec<PropertyIndexOp> {
    let mut ops = Vec::new();
    match (prev, new) {
        (None, Some(n)) => {
            if let Some(bytes) = sortable_index_key(n) {
                ops.push(PropertyIndexOp::Insert {
                    property_id,
                    payload_bytes: bytes,
                });
            }
        }
        (Some(p), Some(n)) if p != n => {
            if let Some(old_bytes) = sortable_index_key(p) {
                ops.push(PropertyIndexOp::Remove {
                    property_id,
                    payload_bytes: old_bytes,
                });
            }
            if let Some(new_bytes) = sortable_index_key(n) {
                ops.push(PropertyIndexOp::Insert {
                    property_id,
                    payload_bytes: new_bytes,
                });
            }
        }
        (Some(p), None) => {
            if let Some(bytes) = sortable_index_key(p) {
                ops.push(PropertyIndexOp::Remove {
                    property_id,
                    payload_bytes: bytes,
                });
            }
        }
        _ => {}
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;

    #[test]
    fn insert_emits_single_posting_for_indexable_value() {
        let ops = index_ops_for_value_change(PropertyId::from_raw(1), None, Some(&Value::Int64(7)));
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], PropertyIndexOp::Insert { .. }));
    }

    #[test]
    fn update_emits_remove_then_insert_when_both_indexable() {
        let ops = index_ops_for_value_change(
            PropertyId::from_raw(1),
            Some(&Value::Int64(1)),
            Some(&Value::Int64(2)),
        );
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], PropertyIndexOp::Remove { .. }));
        assert!(matches!(ops[1], PropertyIndexOp::Insert { .. }));
    }

    #[test]
    fn null_insert_produces_no_ops() {
        let ops = index_ops_for_value_change(PropertyId::from_raw(1), None, Some(&Value::Null));
        assert!(ops.is_empty());
    }

    #[test]
    fn persistable_unindexable_value_produces_no_index_ops() {
        let nan = Value::Float64(f64::NAN);
        crate::property::ensure_persistable(&nan).expect("stored in primary map");
        let ops = index_ops_for_value_change(PropertyId::from_raw(1), None, Some(&nan));
        assert!(ops.is_empty());
    }

    #[test]
    fn oversized_sortable_value_produces_no_index_ops() {
        use gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES;

        let payload_len = MAX_INDEX_VALUE_KEY_BYTES - 2;
        let oversized = Value::Bytes(vec![1u8; payload_len]);
        crate::property::ensure_persistable(&oversized).expect("stored in primary map");
        let ops = index_ops_for_value_change(PropertyId::from_raw(1), None, Some(&oversized));
        assert!(ops.is_empty());
    }
}
