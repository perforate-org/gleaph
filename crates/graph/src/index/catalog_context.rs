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
#[cfg(test)]
use gleaph_graph_kernel::entry::EdgeLabelId;
use gleaph_graph_kernel::entry::{PropertyId, VertexLabelId};
use gleaph_graph_kernel::index::{
    EdgeIndexDirection, IndexMaintenancePhase, IndexedPropertyCatalog, PhysicalIndexId,
};
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

/// Exact lifecycle identity projected from one Router catalog membership.
///
/// This is a read-only Graph-local view used to tag volatile and durable derived-index work.
/// The Router-owned membership remains the source of truth; Graph never allocates or rewrites
/// any of these fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexMembershipRef {
    pub(crate) physical_index_id: PhysicalIndexId,
    pub(crate) catalog_epoch: u64,
    pub(crate) phase: IndexMaintenancePhase,
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

/// Cheap early-out for the pre-canonical index-build fence: `true` when any catalog membership is
/// not Active (Building or Sealing).
///
/// Every fence transition is derived from catalog memberships, so when every membership is Active
/// the fence has no envelope to commit and no Sealing rejection to raise; the per-edge INLINE
/// decode, sidecar resolution, canonical-handle scans, and planning are then pure overhead and may
/// be skipped. This is the overwhelmingly common case during normal operation (no in-flight
/// index build), and keeps the fence's hot-path cost at a single thread-local scan.
pub(crate) fn has_non_active_membership() -> bool {
    CURRENT.with(|c| match c.borrow().as_ref() {
        Some(catalog) => {
            catalog
                .vertex_indexes
                .iter()
                .any(|membership| !membership.phase.is_active())
                || catalog
                    .edge_indexes
                    .iter()
                    .any(|membership| !membership.phase.is_active())
        }
        None => false,
    })
}

/// Return every exact Router-allocated namespace that indexes `property_id` on vertices.
///
/// Multiple logical indexes may intentionally cover the same property (for example, separate
/// label-scoped definitions). Graph emits one namespace-scoped operation per catalog membership;
/// it never selects or invents a local replacement namespace.
pub(crate) fn vertex_physical_index_ids(property_id: PropertyId) -> Vec<PhysicalIndexId> {
    vertex_index_memberships_for_property(property_id)
        .into_iter()
        .map(|membership| membership.physical_index_id)
        .collect()
}

fn project_vertex_membership(
    membership: &gleaph_graph_kernel::index::IndexedVertexMembership,
) -> IndexMembershipRef {
    IndexMembershipRef {
        physical_index_id: membership.physical_index_id,
        catalog_epoch: membership.catalog_epoch,
        phase: membership.phase,
    }
}

/// Return the exact Router namespace for one `(label, property)` pair.
pub(crate) fn vertex_index_memberships(
    label_id: VertexLabelId,
    property_id: PropertyId,
) -> Vec<IndexMembershipRef> {
    with_catalog(
        |catalog| {
            catalog
                .vertex_indexes
                .iter()
                .filter(|membership| {
                    membership.label_id == label_id.raw()
                        && membership.property_id == property_id.raw()
                })
                .map(project_vertex_membership)
                .collect()
        },
        Vec::new(),
    )
}

/// One vertex posting target resolved for a canonical property write.
///
/// Flat memberships post the written property itself. Nested record memberships (ADR 0073)
/// post their Router-interned leaf identity after walking the stored record along
/// [`VertexIndexTarget::field_tail`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VertexIndexTarget {
    pub(crate) membership: IndexMembershipRef,
    /// Posting key identity: the written property id (flat) or the interned leaf id (nested).
    pub(crate) posting_property_id: PropertyId,
    /// Dotted path inside the stored record; empty for flat memberships.
    pub(crate) field_tail: String,
}

/// Resolves every vertex posting target applicable to one canonical property write.
///
/// Flat targets match `property_id` directly; nested targets match on the membership's
/// Router-owned ancestor id, because Graph owns no property-name catalog (ADR 0023).
pub(crate) fn vertex_index_targets_for_labels(
    labels: &[VertexLabelId],
    property_id: PropertyId,
) -> Vec<VertexIndexTarget> {
    with_catalog(
        |catalog| {
            let mut targets = Vec::new();
            for membership in catalog.vertex_indexes.iter().filter(|membership| {
                labels.is_empty()
                    || membership.label_id == 0
                    || labels
                        .iter()
                        .any(|label| label.raw() == membership.label_id)
            }) {
                let projected = project_vertex_membership(membership);
                if targets
                    .iter()
                    .any(|target: &VertexIndexTarget| target.membership == projected)
                {
                    continue;
                }
                if membership.field_path.is_empty() {
                    if membership.property_id != property_id.raw() {
                        continue;
                    }
                    targets.push(VertexIndexTarget {
                        membership: projected,
                        posting_property_id: PropertyId::from_raw(membership.property_id),
                        field_tail: String::new(),
                    });
                } else if membership.ancestor_property_id == property_id.raw() {
                    let (_, tail) = membership
                        .field_path
                        .split_once('.')
                        .expect("non-empty nested field path contains a dot");
                    targets.push(VertexIndexTarget {
                        membership: projected,
                        posting_property_id: PropertyId::from_raw(membership.property_id),
                        field_tail: tail.to_owned(),
                    });
                }
            }
            targets
        },
        Vec::new(),
    )
}

/// Return every `(property, namespace)` pair scoped to one vertex label.
pub(crate) fn vertex_index_memberships_for_label(
    label_id: VertexLabelId,
) -> Vec<(PropertyId, IndexMembershipRef)> {
    with_catalog(
        |catalog| {
            catalog
                .vertex_indexes
                .iter()
                .filter(|membership| membership.label_id == label_id.raw())
                .map(|membership| {
                    (
                        PropertyId::from_raw(membership.property_id),
                        project_vertex_membership(membership),
                    )
                })
                .collect()
        },
        Vec::new(),
    )
}

/// Return all namespaces for a property when no vertex-label discriminator is available.
/// Mutation and delete paths must use one of the exact label-aware selectors instead.
pub(crate) fn vertex_index_memberships_for_property(
    property_id: PropertyId,
) -> Vec<IndexMembershipRef> {
    with_catalog(
        |catalog| {
            catalog
                .vertex_indexes
                .iter()
                .filter(|membership| membership.property_id == property_id.raw())
                .map(project_vertex_membership)
                .collect()
        },
        Vec::new(),
    )
}

/// Resolve one unambiguous vertex index namespace for a property lookup.
///
/// A property can be covered by multiple logical indexes for DML maintenance, but a
/// property-only lookup has no label/definition discriminator.  Keep that distinction
/// explicit and fail closed when the router catalog does not identify exactly one
/// namespace.
pub(crate) fn active_vertex_physical_index_ids(property_id: PropertyId) -> Vec<PhysicalIndexId> {
    vertex_index_memberships_for_property(property_id)
        .into_iter()
        .filter(|membership| membership.phase.is_active())
        .map(|membership| membership.physical_index_id)
        .collect()
}

pub(crate) fn unique_active_vertex_physical_index_id(
    property_id: PropertyId,
) -> Option<PhysicalIndexId> {
    let ids = active_vertex_physical_index_ids(property_id);
    match ids.as_slice() {
        [id] => Some(*id),
        _ => None,
    }
}

pub(crate) fn is_vertex_property_indexed(property_id: PropertyId) -> bool {
    !vertex_physical_index_ids(property_id).is_empty()
}

pub(crate) fn is_edge_property_indexed(property_id: PropertyId) -> bool {
    with_catalog(
        |catalog| {
            catalog
                .edge_indexes
                .iter()
                .any(|membership| membership.property_id == property_id.raw())
        },
        false,
    )
}

/// Return every exact namespace applicable to one edge storage label and property.
pub(crate) fn edge_physical_index_ids(
    wire_label_id: u16,
    property_id: PropertyId,
) -> Vec<PhysicalIndexId> {
    edge_index_memberships(wire_label_id, property_id)
        .into_iter()
        .map(|membership| membership.physical_index_id)
        .collect()
}

pub(crate) fn active_edge_physical_index_ids(
    wire_label_id: u16,
    property_id: PropertyId,
) -> Vec<PhysicalIndexId> {
    edge_index_memberships(wire_label_id, property_id)
        .into_iter()
        .filter(|membership| membership.phase.is_active())
        .map(|membership| membership.physical_index_id)
        .collect()
}

/// Return every Active namespace whose membership names this CATALOG edge label directly.
///
/// Read-path resolution seam of the posting label identity rule (GAP-2026-08-22-001):
/// lookups speak catalog ids while stored postings are wire-tagged, so this must not go
/// through [`edge_posting_matches_registration`], which expects a wire label. Maintenance
/// paths that hold a wire label keep using [`active_edge_physical_index_ids`].
pub(crate) fn active_edge_physical_index_ids_for_catalog_label(
    catalog_label_raw: u16,
    property_id: PropertyId,
) -> Vec<PhysicalIndexId> {
    with_catalog(
        |catalog| {
            catalog
                .edge_indexes
                .iter()
                .filter(|membership| {
                    membership.property_id == property_id.raw()
                        && membership.label_id == catalog_label_raw
                        && membership.phase.is_active()
                })
                .map(|membership| membership.physical_index_id)
                .collect()
        },
        Vec::new(),
    )
}

pub(crate) fn edge_index_memberships(
    wire_label_id: u16,
    property_id: PropertyId,
) -> Vec<IndexMembershipRef> {
    with_catalog(
        |catalog| {
            catalog
                .edge_indexes
                .iter()
                .filter(|membership| {
                    membership.property_id == property_id.raw()
                        && edge_posting_matches_registration(
                            wire_label_id,
                            membership.label_id,
                            membership.direction,
                        )
                })
                .map(|membership| IndexMembershipRef {
                    physical_index_id: membership.physical_index_id,
                    catalog_epoch: membership.catalog_epoch,
                    phase: membership.phase,
                })
                .collect()
        },
        Vec::new(),
    )
}

/// Return every exact edge-index namespace for a property when the lookup has no
/// label discriminator.  The caller must query each returned namespace; an empty
/// result is not a license to use a legacy/default namespace.
pub(crate) fn edge_physical_index_ids_for_property(
    property_id: PropertyId,
) -> Vec<PhysicalIndexId> {
    with_catalog(
        |catalog| {
            let mut ids = catalog
                .edge_indexes
                .iter()
                .filter(|membership| membership.property_id == property_id.raw())
                .map(|membership| membership.physical_index_id)
                .collect::<Vec<_>>();
            ids.sort_unstable();
            ids.dedup();
            ids
        },
        Vec::new(),
    )
}

pub(crate) fn active_edge_physical_index_ids_for_property(
    property_id: PropertyId,
) -> Vec<PhysicalIndexId> {
    let mut ids = with_catalog(
        |catalog| {
            catalog
                .edge_indexes
                .iter()
                .filter(|membership| {
                    membership.property_id == property_id.raw() && membership.phase.is_active()
                })
                .map(|membership| membership.physical_index_id)
                .collect::<Vec<_>>()
        },
        Vec::new(),
    );
    ids.sort_unstable();
    ids.dedup();
    ids
}

pub(crate) fn should_maintain_edge_posting(wire_label_id: u16, property_id: PropertyId) -> bool {
    !edge_physical_index_ids(wire_label_id, property_id).is_empty()
}

/// Return the indexed edge-property identities applicable to one physical edge label. The
/// returned property id is the posting key: for an inline struct leaf it is the Router-interned
/// dotted field property id, while `field_path` identifies the corresponding decoded leaf.
pub(crate) fn indexed_edge_memberships(
    wire_label_id: u16,
    inline_property_id: PropertyId,
) -> Vec<(IndexMembershipRef, PropertyId, String)> {
    with_catalog(
        |catalog| {
            catalog
                .edge_indexes
                .iter()
                .filter(|m| {
                    edge_posting_matches_registration(wire_label_id, m.label_id, m.direction)
                })
                .filter_map(|m| {
                    if m.field_path.is_empty() {
                        (m.property_id == inline_property_id.raw()).then(|| {
                            (
                                IndexMembershipRef {
                                    physical_index_id: m.physical_index_id,
                                    catalog_epoch: m.catalog_epoch,
                                    phase: m.phase,
                                },
                                PropertyId::from_raw(m.property_id),
                                String::new(),
                            )
                        })
                    } else {
                        Some((
                            IndexMembershipRef {
                                physical_index_id: m.physical_index_id,
                                catalog_epoch: m.catalog_epoch,
                                phase: m.phase,
                            },
                            PropertyId::from_raw(m.property_id),
                            m.field_path.clone(),
                        ))
                    }
                })
                .collect()
        },
        Vec::new(),
    )
}

pub(crate) fn edge_posting_matches_registration(
    wire_label_id: u16,
    label_id: u16,
    direction: EdgeIndexDirection,
) -> bool {
    use ic_stable_lara::labeled::BUCKET_LABEL_DIRECTED_BIT;
    let wire = LaraLabelId::from_raw(wire_label_id);
    let Some(catalog) = catalog_edge_label_from_wire(wire) else {
        return false;
    };
    if catalog.raw() != label_id {
        return false;
    }
    if wire_label_id == 0 {
        return false;
    }
    if wire_label_id & BUCKET_LABEL_DIRECTED_BIT != 0 {
        direction.includes_directed()
    } else {
        direction.includes_undirected()
    }
}

/// Distinct `(catalog edge label id, registration direction)` pairs carrying at least one
/// Active index membership, sorted by catalog label id.
///
/// Backfill enumeration expands each pair into the concrete wire labels (directed and/or
/// undirected packing) it must scan.
pub(crate) fn active_indexed_edge_label_registrations() -> Vec<(u16, EdgeIndexDirection)> {
    with_catalog(
        |catalog| {
            let mut out: Vec<(u16, EdgeIndexDirection)> = Vec::new();
            for membership in &catalog.edge_indexes {
                if !membership.phase.is_active() {
                    continue;
                }
                let pair = (membership.label_id, membership.direction);
                if !out.contains(&pair) {
                    out.push(pair);
                }
            }
            out.sort_by_key(|(label_id, _)| *label_id);
            out
        },
        Vec::new(),
    )
}

#[cfg(any(test, feature = "canbench"))]
pub(crate) fn enter_vertex_indexed(property_ids: &[PropertyId]) -> CatalogGuard {
    enter(IndexedPropertyCatalog {
        vertex_indexes: property_ids
            .iter()
            .enumerate()
            .map(
                |(offset, property_id)| gleaph_graph_kernel::index::IndexedVertexMembership {
                    field_path: String::new(),
                    ancestor_property_id: 0,
                    physical_index_id: PhysicalIndexId::new(101 + offset as u64)
                        .expect("test physical id"),
                    catalog_epoch: 1,
                    phase: IndexMaintenancePhase::Active,
                    property_id: property_id.raw(),
                    label_id: 0,
                },
            )
            .collect(),
        ..Default::default()
    })
}

#[cfg(test)]
pub(crate) fn enter_edge_indexed(property_ids: &[PropertyId]) -> CatalogGuard {
    enter_edge_indexed_with_label(property_ids, EdgeLabelId::from_raw(1))
}

#[cfg(test)]
pub(crate) fn enter_edge_indexed_with_label(
    property_ids: &[PropertyId],
    label_id: EdgeLabelId,
) -> CatalogGuard {
    enter(IndexedPropertyCatalog {
        edge_indexes: property_ids
            .iter()
            .enumerate()
            .map(
                |(offset, property_id)| gleaph_graph_kernel::index::IndexedEdgeMembership {
                    physical_index_id: PhysicalIndexId::new(102 + offset as u64)
                        .expect("test physical id"),
                    catalog_epoch: 1,
                    phase: IndexMaintenancePhase::Active,
                    label_id: label_id.raw(),
                    property_id: property_id.raw(),
                    direction: EdgeIndexDirection::Any,
                    field_path: String::new(),
                },
            )
            .collect(),
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
        EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, EdgeLabelId, VertexLabelId,
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
                vector_canister: None,
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
            edge_indexes: vec![IndexedEdgeMembership {
                physical_index_id: PhysicalIndexId::new(103).expect("test physical id"),
                catalog_epoch: 1,
                phase: IndexMaintenancePhase::Active,
                label_id: label.raw(),
                property_id: property.raw(),
                direction: EdgeIndexDirection::Outgoing,
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
            edge_indexes: vec![IndexedEdgeMembership {
                physical_index_id: PhysicalIndexId::new(104).expect("test physical id"),
                catalog_epoch: 1,
                phase: IndexMaintenancePhase::Active,
                label_id: 1,
                property_id: 1,
                direction: EdgeIndexDirection::Outgoing, // PointingRight: directed only
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
                vector_canister: None,
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
            edge_indexes: vec![IndexedEdgeMembership {
                physical_index_id: PhysicalIndexId::new(105).expect("test physical id"),
                catalog_epoch: 1,
                phase: IndexMaintenancePhase::Active,
                label_id: label.raw(),
                property_id: field_property.raw(),
                direction: EdgeIndexDirection::Outgoing,
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

    #[test]
    fn active_query_namespace_is_hidden_for_building_and_sealing_memberships() {
        let property_id = PropertyId::from_raw(77);
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                gleaph_graph_kernel::index::IndexedVertexMembership {
                    field_path: String::new(),
                    ancestor_property_id: 0,
                    physical_index_id: PhysicalIndexId::new(701).expect("test physical id"),
                    catalog_epoch: 8,
                    phase: IndexMaintenancePhase::Building,
                    property_id: property_id.raw(),
                    label_id: 0,
                },
                gleaph_graph_kernel::index::IndexedVertexMembership {
                    field_path: String::new(),
                    ancestor_property_id: 0,
                    physical_index_id: PhysicalIndexId::new(702).expect("test physical id"),
                    catalog_epoch: 9,
                    phase: IndexMaintenancePhase::Sealing,
                    property_id: property_id.raw(),
                    label_id: 0,
                },
            ],
            ..Default::default()
        });
        assert!(active_vertex_physical_index_ids(property_id).is_empty());
        assert_eq!(unique_active_vertex_physical_index_id(property_id), None);
        drop(_guard);
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                gleaph_graph_kernel::index::IndexedVertexMembership {
                    field_path: String::new(),
                    ancestor_property_id: 0,
                    physical_index_id: PhysicalIndexId::new(703).expect("test physical id"),
                    catalog_epoch: 10,
                    phase: IndexMaintenancePhase::Active,
                    property_id: property_id.raw(),
                    label_id: 0,
                },
                gleaph_graph_kernel::index::IndexedVertexMembership {
                    field_path: String::new(),
                    ancestor_property_id: 0,
                    physical_index_id: PhysicalIndexId::new(704).expect("test physical id"),
                    catalog_epoch: 10,
                    phase: IndexMaintenancePhase::Active,
                    property_id: property_id.raw(),
                    label_id: 0,
                },
            ],
            ..Default::default()
        });
        assert_eq!(unique_active_vertex_physical_index_id(property_id), None);
    }

    #[test]
    fn dml_preserves_distinct_membership_identity_for_same_property() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("configure index routing");
        crate::index::pending::clear_pending();
        let property_id = PropertyId::from_raw(78);
        let vertex_id = store.insert_vertex().expect("vertex");
        let vertex = store.vertex(vertex_id).expect("vertex row");
        store
            .set_vertex_labels(vertex_id, vertex, [VertexLabelId::from_raw(1)])
            .expect("target label");
        crate::index::pending::clear_pending();
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                gleaph_graph_kernel::index::IndexedVertexMembership {
                    field_path: String::new(),
                    ancestor_property_id: 0,
                    physical_index_id: PhysicalIndexId::new(711).expect("test physical id"),
                    catalog_epoch: 10,
                    phase: IndexMaintenancePhase::Active,
                    property_id: property_id.raw(),
                    label_id: 1,
                },
                gleaph_graph_kernel::index::IndexedVertexMembership {
                    field_path: String::new(),
                    ancestor_property_id: 0,
                    physical_index_id: PhysicalIndexId::new(712).expect("test physical id"),
                    catalog_epoch: 11,
                    phase: IndexMaintenancePhase::Sealing,
                    property_id: property_id.raw(),
                    label_id: 2,
                },
            ],
            ..Default::default()
        });
        let value = Value::Int64(42);
        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            property_id,
            None,
            Some(&value),
        ));
        let pending = crate::index::pending::take_pending();
        assert_eq!(pending.len(), 1);
        assert!(pending.iter().any(|op| matches!(
            op,
            crate::index::pending::PendingPostingOp::Insert {
                physical_index_id,
                catalog_epoch,
                phase,
                ..
            } if *physical_index_id == PhysicalIndexId::new(711).unwrap()
                && *catalog_epoch == 10
                && *phase == IndexMaintenancePhase::Active
        )));
        assert_eq!(
            pending
                .iter()
                .filter(|op| matches!(
                    op,
                    crate::index::pending::PendingPostingOp::Insert {
                        physical_index_id,
                        ..
                    } if *physical_index_id == PhysicalIndexId::new(712).unwrap()
                ))
                .count(),
            0
        );
        store.set_federation_routing(None).expect("clear routing");
    }

    /// Builds an Active vertex membership for repost-contract fixtures.
    #[cfg(test)]
    fn active_vertex_membership(
        physical_index_id: u64,
        catalog_epoch: u64,
        property_id: PropertyId,
        label_id: u16,
    ) -> gleaph_graph_kernel::index::IndexedVertexMembership {
        gleaph_graph_kernel::index::IndexedVertexMembership {
            physical_index_id: PhysicalIndexId::new(physical_index_id).expect("test physical id"),
            catalog_epoch,
            phase: IndexMaintenancePhase::Active,
            property_id: property_id.raw(),
            label_id,
            field_path: String::new(),
            ancestor_property_id: 0,
        }
    }

    fn int_key(value: i64) -> Vec<u8> {
        gleaph_gql::value_to_index_key_bytes(&Value::Int64(value))
            .unwrap()
            .expect("Int64 index key")
    }

    /// Builds an Active nested-record leaf membership (ADR 0073): postings go under the
    /// Router-interned leaf identity while dispatch matches the ancestor record property.
    #[cfg(test)]
    fn nested_vertex_membership(
        physical_index_id: u64,
        catalog_epoch: u64,
        leaf_property_id: PropertyId,
        ancestor_property_id: PropertyId,
        label_id: u16,
    ) -> gleaph_graph_kernel::index::IndexedVertexMembership {
        gleaph_graph_kernel::index::IndexedVertexMembership {
            physical_index_id: PhysicalIndexId::new(physical_index_id).expect("test physical id"),
            catalog_epoch,
            phase: IndexMaintenancePhase::Active,
            property_id: leaf_property_id.raw(),
            label_id,
            field_path: "stats.score".to_owned(),
            ancestor_property_id: ancestor_property_id.raw(),
        }
    }

    fn stats_record(score: Value) -> Value {
        Value::Record(vec![("score".to_owned(), score)])
    }

    fn nested_routing_fixture() -> GraphStore {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("configure index routing");
        crate::index::pending::clear_pending();
        store
    }

    #[test]
    fn nested_record_set_swaps_leaf_posting_old_for_new() {
        let store = nested_routing_fixture();
        let stats = PropertyId::from_raw(90);
        let leaf = PropertyId::from_raw(91);
        let decoy = PropertyId::from_raw(92);
        let vertex_id = store.insert_vertex().expect("vertex");
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                nested_vertex_membership(751, 21, leaf, stats, 1),
                // A flat membership on the sibling property must never see this change...
                active_vertex_membership(752, 22, decoy, 1),
            ],
            ..Default::default()
        });

        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            stats,
            Some(&stats_record(Value::Int64(1))),
            Some(&stats_record(Value::Int64(2))),
        ));

        let pending = crate::index::pending::take_pending();
        assert_eq!(
            pending.len(),
            2,
            "record replacement must swap exactly the old and new leaf keys"
        );
        assert!(matches!(
            pending[0],
            crate::index::pending::PendingPostingOp::Remove {
                physical_index_id,
                ref payload_bytes,
                ..
            } if physical_index_id == PhysicalIndexId::new(751).unwrap()
                && payload_bytes == &int_key(1)
        ));
        assert!(matches!(
            pending[1],
            crate::index::pending::PendingPostingOp::Insert {
                physical_index_id,
                ref payload_bytes,
                ..
            } if physical_index_id == PhysicalIndexId::new(751).unwrap()
                && payload_bytes == &int_key(2)
        ));
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn nested_record_equal_value_update_and_unrelated_writes_emit_no_postings() {
        let store = nested_routing_fixture();
        let stats = PropertyId::from_raw(93);
        let meta = PropertyId::from_raw(94);
        let leaf = PropertyId::from_raw(95);
        let vertex_id = store.insert_vertex().expect("vertex");
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![nested_vertex_membership(761, 23, leaf, stats, 0)],
            ..Default::default()
        });

        // Equal leaf value: nothing churns.
        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            stats,
            Some(&stats_record(Value::Int64(7))),
            Some(&stats_record(Value::Int64(7))),
        ));
        // A write to an unrelated property does not resolve the nested membership even
        // though that record would structurally provide the same tail.
        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            meta,
            None,
            Some(&stats_record(Value::Int64(7))),
        ));

        assert!(
            crate::index::pending::take_pending().is_empty(),
            "equal-value updates and unrelated writes maintain no leaf postings"
        );
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn nested_record_leaf_maintains_only_its_own_label_scope() {
        let store = nested_routing_fixture();
        let stats = PropertyId::from_raw(96);
        let leaf = PropertyId::from_raw(97);
        let vertex_id = store.insert_vertex().expect("vertex");
        let vertex = store.vertex(vertex_id).expect("vertex row");
        store
            .set_vertex_labels(vertex_id, vertex, [VertexLabelId::from_raw(1)])
            .expect("target label");
        crate::index::pending::clear_pending();
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                nested_vertex_membership(771, 24, leaf, stats, 1),
                nested_vertex_membership(772, 25, leaf, stats, 2),
            ],
            ..Default::default()
        });

        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            stats,
            None,
            Some(&stats_record(Value::Int64(3))),
        ));

        let pending = crate::index::pending::take_pending();
        assert_eq!(pending.len(), 1, "label 2's namespace is not maintained");
        assert!(matches!(
            pending[0],
            crate::index::pending::PendingPostingOp::Insert {
                physical_index_id,
                ref payload_bytes,
                ..
            } if physical_index_id == PhysicalIndexId::new(771).unwrap()
                && payload_bytes == &int_key(3)
        ));
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn nested_record_absence_shapes_fail_closed_without_errors() {
        let store = nested_routing_fixture();
        let stats = PropertyId::from_raw(98);
        let other = PropertyId::from_raw(99);
        let leaf = PropertyId::from_raw(100);
        let vertex_id = store.insert_vertex().expect("vertex");
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![nested_vertex_membership(781, 26, leaf, stats, 0)],
            ..Default::default()
        });
        let list_leaf = Value::Record(vec![(
            "score".to_owned(),
            Value::List(vec![Value::Int64(1), Value::Int64(2)]),
        )]);

        // Leaf disappears because the new value is not a record: remove exactly the old key.
        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            stats,
            Some(&stats_record(Value::Int64(4))),
            Some(&Value::Int64(4)),
        ));
        let pending = crate::index::pending::take_pending();
        assert_eq!(pending.len(), 1, "shape drift removes only the old key");
        assert!(matches!(
            pending[0],
            crate::index::pending::PendingPostingOp::Remove { ref payload_bytes, .. }
                if payload_bytes == &int_key(4)
        ));

        // Missing intermediate / missing leaf / container leaf: no value, no posting, no error.
        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            stats,
            None,
            Some(&Value::Int64(4)),
        ));
        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            stats,
            None,
            Some(&Value::Record(vec![])),
        ));
        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            stats,
            None,
            Some(&list_leaf),
        ));
        // A record under another property never satisfies the declared path.
        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            other,
            None,
            Some(&stats_record(Value::Int64(9))),
        ));
        assert!(
            crate::index::pending::take_pending().is_empty(),
            "absence shapes yield no value and therefore no postings"
        );
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn unlabeled_legacy_set_reposts_remove_and_insert_into_label_scoped_membership() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("configure index routing");
        crate::index::pending::clear_pending();
        let property_id = PropertyId::from_raw(79);
        // No label set: the legacy-unlabeled rule maintains every namespace indexing the
        // property, including this Person-scoped one.
        let vertex_id = store.insert_vertex().expect("vertex");
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![active_vertex_membership(721, 12, property_id, 1)],
            ..Default::default()
        });

        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            property_id,
            Some(&Value::Int64(5)),
            Some(&Value::Int64(6)),
        ));

        let pending = crate::index::pending::take_pending();
        assert_eq!(
            pending.len(),
            2,
            "SET must repost remove-old plus add-new into the label-scoped membership"
        );
        assert!(matches!(
            pending[0],
            crate::index::pending::PendingPostingOp::Remove {
                physical_index_id,
                catalog_epoch,
                ref payload_bytes,
                ..
            } if physical_index_id == PhysicalIndexId::new(721).unwrap()
                && catalog_epoch == 12
                && payload_bytes == &int_key(5)
        ));
        assert!(matches!(
            pending[1],
            crate::index::pending::PendingPostingOp::Insert {
                physical_index_id,
                catalog_epoch,
                ref payload_bytes,
                ..
            } if physical_index_id == PhysicalIndexId::new(721).unwrap()
                && catalog_epoch == 12
                && payload_bytes == &int_key(6)
        ));

        crate::index::pending::clear_pending();
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn labeled_set_reposts_only_into_its_own_label_membership() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("configure index routing");
        crate::index::pending::clear_pending();
        let property_id = PropertyId::from_raw(80);
        let vertex_id = store.insert_vertex().expect("vertex");
        let vertex = store.vertex(vertex_id).expect("vertex row");
        store
            .set_vertex_labels(vertex_id, vertex, [VertexLabelId::from_raw(1)])
            .expect("target label");
        crate::index::pending::clear_pending();
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                active_vertex_membership(731, 13, property_id, 1),
                active_vertex_membership(732, 14, property_id, 2),
            ],
            ..Default::default()
        });

        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            property_id,
            Some(&Value::Int64(5)),
            Some(&Value::Int64(6)),
        ));

        let pending = crate::index::pending::take_pending();
        assert_eq!(pending.len(), 2, "label 2's namespace is not maintained");
        assert!(pending.iter().all(
            |op| matches!(op, crate::index::pending::PendingPostingOp::Remove { physical_index_id, .. }
                | crate::index::pending::PendingPostingOp::Insert { physical_index_id, .. }
                if *physical_index_id == PhysicalIndexId::new(731).unwrap())
        ));

        crate::index::pending::clear_pending();
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn remove_repost_removes_exactly_the_old_value_posting() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("configure index routing");
        crate::index::pending::clear_pending();
        let property_id = PropertyId::from_raw(81);
        let vertex_id = store.insert_vertex().expect("vertex");
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![active_vertex_membership(741, 15, property_id, 3)],
            ..Default::default()
        });

        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            property_id,
            Some(&Value::Int64(5)),
            None,
        ));

        let pending = crate::index::pending::take_pending();
        assert_eq!(
            pending.len(),
            1,
            "REMOVE must enqueue exactly one removal of the old-value posting"
        );
        assert!(matches!(
            pending[0],
            crate::index::pending::PendingPostingOp::Remove {
                physical_index_id,
                ref payload_bytes,
                ..
            } if physical_index_id == PhysicalIndexId::new(741).unwrap()
                && payload_bytes == &int_key(5)
        ));

        crate::index::pending::clear_pending();
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn multi_label_vertex_sharing_one_namespace_pushes_no_duplicate_ops() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("configure index routing");
        crate::index::pending::clear_pending();
        let property_id = PropertyId::from_raw(82);
        let vertex_id = store.insert_vertex().expect("vertex");
        let vertex = store.vertex(vertex_id).expect("vertex row");
        store
            .set_vertex_labels(
                vertex_id,
                vertex,
                [VertexLabelId::from_raw(1), VertexLabelId::from_raw(2)],
            )
            .expect("target labels");
        crate::index::pending::clear_pending();
        let _guard = enter(IndexedPropertyCatalog {
            // One namespace registered under both of the vertex's labels.
            vertex_indexes: vec![
                active_vertex_membership(751, 16, property_id, 1),
                active_vertex_membership(751, 16, property_id, 2),
            ],
            ..Default::default()
        });

        dispatch_property_index_ops(PropertyValueChange::vertex(
            vertex_id,
            property_id,
            None,
            Some(&Value::Int64(6)),
        ));

        let pending = crate::index::pending::take_pending();
        assert_eq!(
            pending.len(),
            1,
            "one shared membership identity yields exactly one posting op"
        );

        crate::index::pending::clear_pending();
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn missing_vertex_row_dispatches_no_postings() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("configure index routing");
        crate::index::pending::clear_pending();
        let property_id = PropertyId::from_raw(83);
        let _guard = enter(IndexedPropertyCatalog {
            vertex_indexes: vec![active_vertex_membership(761, 17, property_id, 0)],
            ..Default::default()
        });
        let absent = ic_stable_lara::VertexId::from(u32::MAX);

        dispatch_property_index_ops(PropertyValueChange::vertex(
            absent,
            property_id,
            None,
            Some(&Value::Int64(6)),
        ));

        assert!(crate::index::pending::take_pending().is_empty());
        store.set_federation_routing(None).expect("clear routing");
    }
}
