//! Shard-local registry of administrator-registered indexed properties (ADR 0009 §2).
//!
//! The router is SSOT for names; graph shards store numeric [`PropertyId`] sets updated via
//! [`register_indexed_property`](crate::canister::handlers::register_indexed_property).

use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::index::{IndexedPropertyKind, RegisterIndexedPropertyArgs};
use std::cell::RefCell;
use std::collections::BTreeSet;

thread_local! {
    static INDEXED_VERTEX_PROPERTIES: RefCell<BTreeSet<u32>> = const { RefCell::new(BTreeSet::new()) };
    static INDEXED_EDGE_PROPERTIES: RefCell<BTreeSet<u32>> = const { RefCell::new(BTreeSet::new()) };
}

pub(crate) fn register_vertex_property(property_id: PropertyId) {
    INDEXED_VERTEX_PROPERTIES.with(|set| {
        set.borrow_mut().insert(property_id.raw());
    });
}

pub(crate) fn register_edge_property(property_id: PropertyId) {
    INDEXED_EDGE_PROPERTIES.with(|set| {
        set.borrow_mut().insert(property_id.raw());
    });
}

pub(crate) fn is_vertex_property_indexed(property_id: PropertyId) -> bool {
    INDEXED_VERTEX_PROPERTIES.with(|set| set.borrow().contains(&property_id.raw()))
}

pub(crate) fn is_edge_property_indexed(property_id: PropertyId) -> bool {
    INDEXED_EDGE_PROPERTIES.with(|set| set.borrow().contains(&property_id.raw()))
}

pub(crate) fn apply_register(args: RegisterIndexedPropertyArgs) {
    let property_id = PropertyId::from_raw(args.property_id);
    match args.kind {
        IndexedPropertyKind::Vertex => register_vertex_property(property_id),
        IndexedPropertyKind::Edge => register_edge_property(property_id),
    }
}

pub(crate) fn apply_unregister(args: RegisterIndexedPropertyArgs) {
    let property_id = PropertyId::from_raw(args.property_id);
    match args.kind {
        IndexedPropertyKind::Vertex => unregister_vertex_property(property_id),
        IndexedPropertyKind::Edge => unregister_edge_property(property_id),
    }
}

fn unregister_vertex_property(property_id: PropertyId) {
    INDEXED_VERTEX_PROPERTIES.with(|set| {
        set.borrow_mut().remove(&property_id.raw());
    });
}

fn unregister_edge_property(property_id: PropertyId) {
    INDEXED_EDGE_PROPERTIES.with(|set| {
        set.borrow_mut().remove(&property_id.raw());
    });
}

#[cfg(test)]
pub(crate) fn clear_for_test() {
    INDEXED_VERTEX_PROPERTIES.with(|set| set.borrow_mut().clear());
    INDEXED_EDGE_PROPERTIES.with(|set| set.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::GraphStore;
    use crate::index::edge_lookup;
    use crate::property::{PropertyValueChange, dispatch_property_index_ops};
    use gleaph_gql::Value;

    #[test]
    fn unregistered_edge_property_skips_equality_scan() {
        clear_for_test();
        let owner = ic_stable_lara::VertexId::from(1u32);
        let pid = PropertyId::from_raw(12);
        dispatch_property_index_ops(PropertyValueChange::edge(
            owner,
            0,
            0,
            pid,
            None,
            Some(&Value::Int64(3)),
        ));
        let key = gleaph_gql::value_to_index_key_bytes(&Value::Int64(3))
            .unwrap()
            .unwrap();
        let hits =
            edge_lookup::lookup_edge_equal_local_sync(None, pid, &key, None).expect("lookup");
        assert!(hits.is_empty());
    }

    #[test]
    fn registered_edge_property_visible_via_store_scan() {
        clear_for_test();
        let store = GraphStore::new();
        let owner = store.insert_vertex().expect("owner");
        let neighbor = store.insert_vertex().expect("neighbor");
        let handle = store
            .insert_directed_edge(owner, neighbor, None)
            .expect("edge");
        let canonical = store.canonical_edge_handle(handle);
        let pid = PropertyId::from_raw(12);
        register_edge_property(pid);
        store
            .set_edge_property(canonical, pid, Value::Int64(3))
            .expect("set");
        let key = gleaph_gql::value_to_index_key_bytes(&Value::Int64(3))
            .unwrap()
            .unwrap();
        let hits =
            edge_lookup::lookup_edge_equal_local_sync(None, pid, &key, None).expect("lookup");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].owner_vertex_id, owner);
    }

    #[test]
    fn apply_unregister_removes_kind() {
        clear_for_test();
        let pid = PropertyId::from_raw(21);
        apply_register(RegisterIndexedPropertyArgs {
            kind: IndexedPropertyKind::Vertex,
            property_id: pid.raw(),
        });
        apply_unregister(RegisterIndexedPropertyArgs {
            kind: IndexedPropertyKind::Vertex,
            property_id: pid.raw(),
        });
        assert!(!is_vertex_property_indexed(pid));
    }

    #[test]
    fn apply_register_respects_kind() {
        clear_for_test();
        let pid = PropertyId::from_raw(20);
        apply_register(RegisterIndexedPropertyArgs {
            kind: IndexedPropertyKind::Vertex,
            property_id: pid.raw(),
        });
        assert!(is_vertex_property_indexed(pid));
        assert!(!is_edge_property_indexed(pid));
        apply_register(RegisterIndexedPropertyArgs {
            kind: IndexedPropertyKind::Edge,
            property_id: pid.raw(),
        });
        assert!(is_edge_property_indexed(pid));
    }
}
