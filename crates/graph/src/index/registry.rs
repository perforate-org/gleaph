//! Shard-local registry of administrator-registered indexed properties (ADR 0009 §2, ADR 0012).
//!
//! The router is SSOT for names; graph shards store numeric registrations updated via
//! [`register_indexed_property`](crate::canister::handlers::register_indexed_property) and
//! [`register_indexed_edge_index`](crate::canister::handlers::register_indexed_edge_index).

use crate::facade::catalog_edge_label_from_wire;
use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::index::{
    IndexedPropertyKind, RegisterIndexedEdgeIndexArgs, RegisterIndexedPropertyArgs,
};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use std::cell::RefCell;
use std::collections::BTreeSet;

thread_local! {
    static INDEXED_VERTEX_PROPERTIES: RefCell<BTreeSet<u32>> = const { RefCell::new(BTreeSet::new()) };
    static INDEXED_EDGE_PROPERTIES: RefCell<BTreeSet<u32>> = const { RefCell::new(BTreeSet::new()) };
    static INDEXED_EDGE_INDEXES: RefCell<BTreeSet<(u16, u32, u8)>> = const { RefCell::new(BTreeSet::new()) };
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

pub(crate) fn register_edge_index(args: RegisterIndexedEdgeIndexArgs) {
    INDEXED_EDGE_INDEXES.with(|set| {
        set.borrow_mut()
            .insert((args.label_id, args.property_id, args.direction_tag));
    });
}

pub(crate) fn unregister_edge_index(args: RegisterIndexedEdgeIndexArgs) {
    INDEXED_EDGE_INDEXES.with(|set| {
        set.borrow_mut()
            .remove(&(args.label_id, args.property_id, args.direction_tag));
    });
}

pub(crate) fn is_vertex_property_indexed(property_id: PropertyId) -> bool {
    INDEXED_VERTEX_PROPERTIES.with(|set| set.borrow().contains(&property_id.raw()))
}

pub(crate) fn is_edge_property_indexed(property_id: PropertyId) -> bool {
    INDEXED_EDGE_PROPERTIES.with(|set| set.borrow().contains(&property_id.raw()))
}

fn edge_posting_matches_registration(wire_label_id: u16, label_id: u16, direction_tag: u8) -> bool {
    use ic_stable_lara::labeled::BUCKET_LABEL_DIRECTED_BIT;
    let wire = LaraLabelId::from_raw(wire_label_id);
    let Some(catalog) = catalog_edge_label_from_wire(wire) else {
        return false;
    };
    if catalog.raw() != label_id {
        return false;
    }
    let edge_class = if wire_label_id & BUCKET_LABEL_DIRECTED_BIT != 0 {
        "directed"
    } else if wire_label_id == 0 {
        return false;
    } else {
        "undirected"
    };
    let maintains_directed = matches!(direction_tag, 1 | 2 | 3 | 7 | 6 | 5);
    let maintains_undirected = matches!(direction_tag, 4..=7);
    match edge_class {
        "directed" => maintains_directed,
        "undirected" => maintains_undirected,
        _ => false,
    }
}

pub(crate) fn should_maintain_edge_posting(wire_label_id: u16, property_id: PropertyId) -> bool {
    if !is_edge_property_indexed(property_id) {
        return false;
    }
    INDEXED_EDGE_INDEXES.with(|set| {
        let regs = set.borrow();
        if regs.is_empty() {
            return true;
        }
        regs.iter().any(|(label_id, pid, direction_tag)| {
            *pid == property_id.raw()
                && edge_posting_matches_registration(wire_label_id, *label_id, *direction_tag)
        })
    })
}

pub(crate) fn apply_register(args: RegisterIndexedPropertyArgs) {
    let property_id = PropertyId::from_raw(args.property_id);
    match args.kind {
        IndexedPropertyKind::Vertex => register_vertex_property(property_id),
        IndexedPropertyKind::Edge => register_edge_property(property_id),
    }
}

pub(crate) fn apply_register_edge_index(args: RegisterIndexedEdgeIndexArgs) {
    register_edge_index(args);
}

pub(crate) fn apply_unregister(args: RegisterIndexedPropertyArgs) {
    let property_id = PropertyId::from_raw(args.property_id);
    match args.kind {
        IndexedPropertyKind::Vertex => unregister_vertex_property(property_id),
        IndexedPropertyKind::Edge => unregister_edge_property(property_id),
    }
}

pub(crate) fn apply_unregister_edge_index(args: RegisterIndexedEdgeIndexArgs) {
    unregister_edge_index(args);
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
    INDEXED_EDGE_INDEXES.with(|set| set.borrow_mut().clear());
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

    #[test]
    fn edge_index_registration_filters_by_wire_class() {
        clear_for_test();
        register_edge_property(PropertyId::from_raw(1));
        apply_register_edge_index(RegisterIndexedEdgeIndexArgs {
            label_id: 1,
            property_id: 1,
            direction_tag: 1, // PointingRight: directed only
        });
        assert!(should_maintain_edge_posting(
            0x8001,
            PropertyId::from_raw(1)
        ));
        assert!(!should_maintain_edge_posting(
            0x0001,
            PropertyId::from_raw(1)
        ));
    }
}
