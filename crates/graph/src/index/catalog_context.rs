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

/// Return the indexed edge-property identities applicable to one physical edge label. The
/// returned property id is the posting key: for an inline struct leaf it is the Router-interned
/// dotted field property id, while `field_path` identifies the corresponding decoded leaf.
pub(crate) fn indexed_edge_memberships(
    wire_label_id: u16,
    inline_property_id: PropertyId,
) -> Vec<(PropertyId, String)> {
    with_catalog(
        |catalog| {
            catalog
                .edge_indexes
                .iter()
                .filter(|m| {
                    edge_posting_matches_registration(wire_label_id, m.label_id, m.direction_tag)
                })
                .filter_map(|m| {
                    if m.field_path.is_empty() {
                        (m.property_id == inline_property_id.raw())
                            .then(|| (PropertyId::from_raw(m.property_id), String::new()))
                    } else {
                        Some((PropertyId::from_raw(m.property_id), m.field_path.clone()))
                    }
                })
                .collect()
        },
        Vec::new(),
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
    use gleaph_graph_kernel::entry::{
        EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, EdgeLabelId,
    };
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
        let pid = PropertyId::from_raw(12);
        let _guard = enter_edge_indexed(&[pid]);
        store
            .set_edge_property(
                handle.occurrence(ic_stable_lara::labeled::LabeledOrientation::Forward),
                pid,
                Value::Int64(3),
            )
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
    fn indexed_scalar_inline_property_emits_insert_update_and_remove_postings() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_index_canister: None,
            }))
            .expect("configure index routing");
        crate::index::edge_pending::clear_pending();

        let label = EdgeLabelId::from_raw(1);
        let property = PropertyId::from_raw(901);
        crate::test_labels::install_test_edge_inline_property_profile(
            label,
            EdgeInlinePropertyProfile {
                byte_width: 4,
                encoding: EdgeInlinePropertyEncoding::F32,
            },
        );
        crate::test_labels::install_test_edge_inline_property(label, property);
        let _catalog = enter(IndexedPropertyCatalog {
            edge_property_ids: vec![property.raw()],
            edge_indexes: vec![IndexedEdgeMembership {
                label_id: label.raw(),
                property_id: property.raw(),
                direction_tag: 1,
                field_path: String::new(),
            }],
            ..Default::default()
        });

        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let wire_label = label.pack(gleaph_graph_kernel::entry::EdgeDirectedness::Directed);
        let handle = store
            .insert_directed_edge_with_inline_property_bytes(
                source,
                target,
                Some(label),
                &1.5f32.to_le_bytes(),
            )
            .expect("insert inline edge");
        let inserted = crate::index::edge_pending::take_pending();
        assert!(matches!(
            inserted.as_slice(),
            [crate::index::edge_pending::PendingEdgePostingOp::Insert {
                property_id,
                label_id,
                payload_bytes,
                ..
            }] if *property_id == property.raw()
                && *label_id == wire_label.raw()
                && *payload_bytes == gleaph_gql::value_to_index_key_bytes(&Value::Float32(1.5)).unwrap().unwrap()
        ));

        store
            .update_edge_inline_property_at_handle(handle, &2.5f32.to_le_bytes())
            .expect("update inline edge");
        let updated = crate::index::edge_pending::take_pending();
        assert_eq!(updated.len(), 2);
        assert!(matches!(
            &updated[0],
            crate::index::edge_pending::PendingEdgePostingOp::Remove { property_id, .. }
                if *property_id == property.raw()
        ));
        assert!(matches!(
            &updated[1],
            crate::index::edge_pending::PendingEdgePostingOp::Insert { property_id, .. }
                if *property_id == property.raw()
        ));

        store
            .delete_edge_by_handle(handle)
            .expect("delete inline edge");
        let removed = crate::index::edge_pending::take_pending();
        assert!(matches!(
            removed.as_slice(),
            [crate::index::edge_pending::PendingEdgePostingOp::Remove {
                property_id,
                payload_bytes,
                ..
            }] if *property_id == property.raw()
                && *payload_bytes == gleaph_gql::value_to_index_key_bytes(&Value::Float32(2.5)).unwrap().unwrap()
        ));
        store.set_federation_routing(None).expect("clear routing");
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
                field_path: String::new(),
            }],
            ..Default::default()
        });
        assert!(should_maintain_edge_posting(0x8001, pid));
        assert!(!should_maintain_edge_posting(0x0001, pid));
    }

    #[test]
    fn indexed_inline_struct_field_emits_leaf_posting() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_index_canister: None,
            }))
            .expect("configure index routing");
        crate::index::edge_pending::clear_pending();

        let label = EdgeLabelId::from_raw(2);
        let top_property = PropertyId::from_raw(910);
        let field_property = PropertyId::from_raw(911);
        let scalar = EdgeInlinePropertyProfile {
            byte_width: 4,
            encoding: EdgeInlinePropertyEncoding::F32,
        };
        crate::test_labels::install_test_edge_inline_struct_property(
            label,
            top_property,
            vec![
                ("stats.score".into(), 0, scalar.clone()),
                ("stats.confidence".into(), 4, scalar),
            ],
        );
        crate::test_labels::install_test_edge_inline_property_profile(
            label,
            EdgeInlinePropertyProfile {
                byte_width: 8,
                encoding: EdgeInlinePropertyEncoding::RawBytes,
            },
        );
        let _catalog = enter(IndexedPropertyCatalog {
            edge_property_ids: vec![field_property.raw()],
            edge_indexes: vec![IndexedEdgeMembership {
                label_id: label.raw(),
                property_id: field_property.raw(),
                direction_tag: 1,
                field_path: "stats.score".into(),
            }],
            ..Default::default()
        });

        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        store
            .insert_directed_edge_with_inline_property_bytes(
                source,
                target,
                Some(label),
                &[1.5f32.to_le_bytes(), 0.25f32.to_le_bytes()].concat(),
            )
            .expect("insert inline struct edge");
        let pending = crate::index::edge_pending::take_pending();
        assert!(matches!(
            pending.as_slice(),
            [crate::index::edge_pending::PendingEdgePostingOp::Insert {
                property_id,
                payload_bytes,
                ..
            }] if *property_id == field_property.raw()
                && *payload_bytes == gleaph_gql::value_to_index_key_bytes(&Value::Float32(1.5)).unwrap().unwrap()
        ));
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn absent_catalog_reports_not_indexed() {
        assert!(!is_vertex_property_indexed(PropertyId::from_raw(7)));
        assert!(!is_edge_property_indexed(PropertyId::from_raw(7)));
    }
}
