//! Edge equality postings: canonical source is [`EDGE_PROPERTIES`]; derived store is
//! [`EDGE_EQUALITY_POSTINGS`], updated synchronously on property DML via
//! [`crate::property::dispatch_property_index_ops`].

use super::super::stable::edge_equality_postings::EdgeEqualityPostingKey;
use super::super::stable::{EDGE_EQUALITY_POSTINGS, EDGE_PROPERTIES};
use crate::property::sortable_index_key;
use std::collections::BTreeSet;

/// Mismatch between canonical edge properties and derived equality postings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EdgeEqualityDerivedInconsistency {
    pub missing_in_derived: Vec<EdgeEqualityPostingKey>,
    pub extra_in_derived: Vec<EdgeEqualityPostingKey>,
}

fn expected_keys_from_canonical() -> BTreeSet<EdgeEqualityPostingKey> {
    let mut expected = BTreeSet::new();
    EDGE_PROPERTIES.with_borrow(|properties| {
        properties.for_each_property(|key, value| {
            if let Some(bytes) = sortable_index_key(value) {
                expected.insert(EdgeEqualityPostingKey::new(
                    key.property_id(),
                    &bytes,
                    key.owner_vertex_id(),
                    key.label_id(),
                    key.slot_index(),
                ));
            }
        });
    });
    expected
}

fn actual_keys_in_derived() -> BTreeSet<EdgeEqualityPostingKey> {
    let mut actual = BTreeSet::new();
    EDGE_EQUALITY_POSTINGS.with_borrow(|index| {
        index.for_each_posting(|key| {
            actual.insert(key);
        });
    });
    actual
}

/// Returns `Ok(())` when derived postings match indexable canonical edge properties.
pub(crate) fn check_edge_equality_postings() -> Result<(), EdgeEqualityDerivedInconsistency> {
    let expected = expected_keys_from_canonical();
    let actual = actual_keys_in_derived();
    if expected == actual {
        return Ok(());
    }
    let missing_in_derived: Vec<_> = expected.difference(&actual).cloned().collect();
    let extra_in_derived: Vec<_> = actual.difference(&expected).cloned().collect();
    Err(EdgeEqualityDerivedInconsistency {
        missing_in_derived,
        extra_in_derived,
    })
}

/// Rebuilds [`EDGE_EQUALITY_POSTINGS`] from canonical [`EDGE_PROPERTIES`].
pub(crate) fn rebuild_edge_equality_postings() {
    let expected = expected_keys_from_canonical();
    EDGE_EQUALITY_POSTINGS.with_borrow_mut(|index| {
        index.clear_all();
        for key in expected {
            index.insert(key);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::GraphStore;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::PropertyId;

    #[test]
    fn empty_stores_are_consistent() {
        let _store = GraphStore::new();
        check_edge_equality_postings().expect("empty derived matches empty canonical");
    }

    #[test]
    fn property_dml_keeps_derived_store_consistent() {
        let store = GraphStore::new();
        let owner = store.insert_vertex().expect("owner");
        let neighbor = store.insert_vertex().expect("neighbor");
        let handle = store
            .insert_directed_edge(owner, neighbor, None)
            .expect("edge");
        let canonical = store.canonical_edge_handle(handle);
        let pid = PropertyId::from_raw(9);
        store
            .set_edge_property(canonical, pid, Value::Int64(42))
            .expect("edge property");

        check_edge_equality_postings().expect("sync update path keeps derived consistent");
    }

    #[test]
    fn rebuild_repairs_extra_derived_postings() {
        let store = GraphStore::new();
        let owner = store.insert_vertex().expect("owner");
        let neighbor = store.insert_vertex().expect("neighbor");
        let handle = store
            .insert_directed_edge(owner, neighbor, None)
            .expect("edge");
        let canonical = store.canonical_edge_handle(handle);
        let pid = PropertyId::from_raw(9);
        store
            .set_edge_property(canonical, pid, Value::Int64(42))
            .expect("edge property");

        EDGE_EQUALITY_POSTINGS.with_borrow_mut(|index| {
            index.insert(EdgeEqualityPostingKey::new(
                PropertyId::from_raw(99),
                &[1],
                owner,
                0,
                0,
            ));
        });
        assert!(check_edge_equality_postings().is_err());

        rebuild_edge_equality_postings();
        check_edge_equality_postings().expect("rebuild restores consistency");
    }

    #[test]
    fn rebuild_repairs_missing_derived_postings() {
        let store = GraphStore::new();
        let owner = store.insert_vertex().expect("owner");
        let neighbor = store.insert_vertex().expect("neighbor");
        let handle = store
            .insert_directed_edge(owner, neighbor, None)
            .expect("edge");
        let canonical = store.canonical_edge_handle(handle);
        let pid = PropertyId::from_raw(9);
        let value = Value::Int64(7);
        store
            .set_edge_property(canonical, pid, value.clone())
            .expect("edge property");

        EDGE_EQUALITY_POSTINGS.with_borrow_mut(|index| index.clear_all());
        assert!(check_edge_equality_postings().is_err());

        rebuild_edge_equality_postings();
        check_edge_equality_postings().expect("rebuild restores consistency");

        let bytes = sortable_index_key(&value).expect("indexable");
        let postings = crate::facade::edge_equality_index::lookup_equal(pid, &bytes)
            .expect("posting restored");
        assert_eq!(postings.len(), 1);
        assert_eq!(postings[0].owner_vertex_id, canonical.owner_vertex_id);
        assert_eq!(postings[0].slot_index, canonical.slot_index);
    }

    #[test]
    fn unindexable_property_values_have_no_derived_postings() {
        let store = GraphStore::new();
        let owner = store.insert_vertex().expect("owner");
        let neighbor = store.insert_vertex().expect("neighbor");
        let handle = store
            .insert_directed_edge(owner, neighbor, None)
            .expect("edge");
        let canonical = store.canonical_edge_handle(handle);
        let pid = PropertyId::from_raw(9);
        store
            .set_edge_property(canonical, pid, Value::Float64(f64::NAN))
            .expect("persistable nan");

        check_edge_equality_postings().expect("unindexable canonical values omit derived rows");
    }
}
