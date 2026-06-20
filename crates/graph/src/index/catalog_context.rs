//! Ephemeral, router-sourced indexed-property catalog for the current operation
//! (ADR 0023 D1/D3).
//!
//! Replaces the former shard-local `registry` thread-local gate. The catalog is
//! installed at the start of an operation from the router-supplied snapshot
//! ([`gleaph_graph_kernel::plan_exec::ExecutePlanArgs::indexed_properties`]) and
//! cleared when the operation completes. The shard therefore never persists
//! derived index state, so the catalog can never go stale across the canister
//! upgrade boundary (the defect class ADR 0023 removes structurally).

use crate::facade::catalog_edge_label_from_wire;
use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::index::IndexedPropertyCatalog;
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use std::cell::RefCell;

thread_local! {
    static CURRENT: RefCell<Option<IndexedPropertyCatalog>> = const { RefCell::new(None) };
}

/// RAII guard that keeps a router-sourced catalog active for the current
/// operation and restores the previous value (if any) on drop.
#[must_use = "the catalog is only active while the guard is alive"]
pub(crate) struct CatalogGuard {
    previous: Option<IndexedPropertyCatalog>,
}

impl Drop for CatalogGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.previous.take());
    }
}

/// Install `catalog` as the current operation's indexed-property catalog. The
/// previous value is restored when the returned guard is dropped.
pub(crate) fn enter(catalog: IndexedPropertyCatalog) -> CatalogGuard {
    let previous = CURRENT.with(|c| c.borrow_mut().replace(catalog));
    CatalogGuard { previous }
}

fn with_catalog<R>(present: impl FnOnce(&IndexedPropertyCatalog) -> R, absent: R) -> R {
    CURRENT.with(|c| match c.borrow().as_ref() {
        Some(catalog) => present(catalog),
        None => absent,
    })
}

pub(crate) fn is_vertex_property_indexed(property_id: PropertyId) -> bool {
    with_catalog(
        |catalog| catalog.vertex_property_ids.contains(&property_id.raw()),
        false,
    )
}

pub(crate) fn is_edge_property_indexed(property_id: PropertyId) -> bool {
    with_catalog(
        |catalog| catalog.edge_property_ids.contains(&property_id.raw()),
        false,
    )
}

pub(crate) fn should_maintain_edge_posting(wire_label_id: u16, property_id: PropertyId) -> bool {
    with_catalog(
        |catalog| {
            if !catalog.edge_property_ids.contains(&property_id.raw()) {
                return false;
            }
            if catalog.edge_indexes.is_empty() {
                return true;
            }
            catalog.edge_indexes.iter().any(|m| {
                m.property_id == property_id.raw()
                    && edge_posting_matches_registration(wire_label_id, m.label_id, m.direction_tag)
            })
        },
        false,
    )
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

#[cfg(test)]
pub(crate) fn enter_vertex_indexed(property_ids: &[PropertyId]) -> CatalogGuard {
    enter(IndexedPropertyCatalog {
        vertex_property_ids: property_ids.iter().map(|p| p.raw()).collect(),
        ..Default::default()
    })
}

#[cfg(test)]
pub(crate) fn enter_edge_indexed(property_ids: &[PropertyId]) -> CatalogGuard {
    enter(IndexedPropertyCatalog {
        edge_property_ids: property_ids.iter().map(|p| p.raw()).collect(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::GraphStore;
    use crate::index::edge_lookup;
    use crate::property::{PropertyValueChange, dispatch_property_index_ops};
    use gleaph_gql::Value;
    use gleaph_graph_kernel::index::IndexedEdgeMembership;

    #[test]
    fn unindexed_edge_property_skips_equality_scan() {
        let owner = ic_stable_lara::VertexId::from(1u32);
        let pid = PropertyId::from_raw(12);
        // No catalog installed → property is not indexed → no posting enqueued.
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
    fn indexed_edge_property_visible_via_store_scan() {
        let store = GraphStore::new();
        let owner = store.insert_vertex().expect("owner");
        let neighbor = store.insert_vertex().expect("neighbor");
        let handle = store
            .insert_directed_edge(owner, neighbor, None)
            .expect("edge");
        let canonical = store.canonical_edge_handle(handle);
        let pid = PropertyId::from_raw(12);
        let _guard = enter_edge_indexed(&[pid]);
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
    fn edge_index_membership_filters_by_wire_class() {
        let pid = PropertyId::from_raw(1);
        let _guard = enter(IndexedPropertyCatalog {
            edge_property_ids: vec![pid.raw()],
            edge_indexes: vec![IndexedEdgeMembership {
                label_id: 1,
                property_id: 1,
                direction_tag: 1, // PointingRight: directed only
            }],
            ..Default::default()
        });
        assert!(should_maintain_edge_posting(0x8001, pid));
        assert!(!should_maintain_edge_posting(0x0001, pid));
    }

    #[test]
    fn absent_catalog_reports_not_indexed() {
        assert!(!is_vertex_property_indexed(PropertyId::from_raw(7)));
        assert!(!is_edge_property_indexed(PropertyId::from_raw(7)));
    }
}
