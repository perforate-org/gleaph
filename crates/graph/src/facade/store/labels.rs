//! Label storage domain: vertex label sets plus federated label index events.

use super::super::VertexLabelStoreError;
use super::super::stable::VERTEX_LABELS;
use super::error::GraphStoreError;
use crate::facade::store::index_build_admission::{FencedTransition, trap_post_fence_commit};
use crate::index::label_pending;
use crate::property::{
    PropertyValueChange, dispatch_property_index_ops_for_physical, index_ops_for_value_change,
};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{PropertyId, Vertex, VertexLabelId};
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_lara::VertexId;
use std::collections::BTreeMap;

use super::GraphStore;

impl GraphStore {
    pub(super) fn apply_vertex_label_transition(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        labels: impl IntoIterator<Item = VertexLabelId>,
        mutation_id: MutationId,
    ) -> Result<Vertex, GraphStoreError> {
        let prev = self.vertex_labels(vertex_id, vertex);
        let next = normalize_labels(labels)?;
        if prev == next {
            return Ok(vertex);
        }

        let transitions = self.label_property_transitions(vertex_id, &prev, &next);
        let planned = self.plan_index_build_admission(transitions.iter().map(|transition| {
            FencedTransition {
                property_id: transition.property_id,
                prev: transition.previous(),
                new: transition.next(),
                membership: transition.membership,
            }
        }))?;
        let fence_committed = !planned.is_empty();
        if fence_committed {
            let subject = self.vertex_subject_for_id(vertex_id)?;
            self.commit_index_build_admission(mutation_id, subject, planned);
        }

        let updated = match VERTEX_LABELS
            .with_borrow_mut(|store| store.set_labels(vertex_id, vertex, next.iter().copied()))
        {
            Ok(updated) => updated,
            Err(error) if fence_committed => trap_post_fence_commit(error.into()),
            Err(error) => return Err(error.into()),
        };
        let updated = match self.set_vertex(vertex_id, updated) {
            Ok(()) => updated,
            Err(error) if fence_committed => trap_post_fence_commit(error.into()),
            Err(error) => {
                panic!("vertex label row persistence failed after canonical sidecar write: {error}")
            }
        };
        label_pending::record_vertex_label_set(vertex_id, &prev, &next);
        for transition in &transitions {
            dispatch_property_index_ops_for_physical(
                PropertyValueChange::vertex(
                    vertex_id,
                    transition.property_id,
                    transition.previous(),
                    transition.next(),
                ),
                transition.membership,
            );
        }
        Ok(updated)
    }

    /// Clear all labels on delete without touching the CSR vertex row.
    pub(super) fn commit_clear_vertex_labels(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
    ) -> Result<(), super::error::GraphStoreError> {
        let prev = self.vertex_labels(vertex_id, vertex);
        label_pending::record_vertex_label_set(vertex_id, &prev, &[]);
        VERTEX_LABELS
            .with_borrow_mut(|labels| labels.set_labels(vertex_id, vertex, []))
            .map(|_| ())
            .map_err(super::error::GraphStoreError::from)
    }
}

#[derive(Clone)]
struct LabelPropertyTransition {
    property_id: PropertyId,
    value: Value,
    membership: crate::index::catalog_context::IndexMembershipRef,
    gained: bool,
}

impl LabelPropertyTransition {
    fn previous(&self) -> Option<&Value> {
        (!self.gained).then_some(&self.value)
    }

    fn next(&self) -> Option<&Value> {
        self.gained.then_some(&self.value)
    }
}

impl GraphStore {
    fn label_property_transitions(
        &self,
        vertex_id: VertexId,
        previous_labels: &[VertexLabelId],
        next_labels: &[VertexLabelId],
    ) -> Vec<LabelPropertyTransition> {
        let mut affected: BTreeMap<
            PropertyId,
            Vec<(crate::index::catalog_context::IndexMembershipRef, bool)>,
        > = BTreeMap::new();
        for label_id in next_labels
            .iter()
            .copied()
            .filter(|label_id| !previous_labels.contains(label_id))
        {
            for (property_id, membership) in
                crate::index::catalog_context::vertex_index_memberships_for_label(label_id)
            {
                let memberships = affected.entry(property_id).or_default();
                if !memberships.contains(&(membership, true)) {
                    memberships.push((membership, true));
                }
            }
        }
        for label_id in previous_labels
            .iter()
            .copied()
            .filter(|label_id| !next_labels.contains(label_id))
        {
            for (property_id, membership) in
                crate::index::catalog_context::vertex_index_memberships_for_label(label_id)
            {
                let memberships = affected.entry(property_id).or_default();
                if !memberships.contains(&(membership, false)) {
                    memberships.push((membership, false));
                }
            }
        }

        affected
            .into_iter()
            .flat_map(|(property_id, memberships)| {
                let Some(value) = self.vertex_property(vertex_id, property_id) else {
                    return Vec::new();
                };
                memberships
                    .into_iter()
                    .filter_map(|(membership, gained)| {
                        let previous = (!gained).then_some(&value);
                        let next = gained.then_some(&value);
                        (!index_ops_for_value_change(property_id, previous, next).is_empty())
                            .then_some(LabelPropertyTransition {
                                property_id,
                                value: value.clone(),
                                membership,
                                gained,
                            })
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

fn normalize_labels(
    labels: impl IntoIterator<Item = VertexLabelId>,
) -> Result<Vec<VertexLabelId>, GraphStoreError> {
    let mut labels: Vec<_> = labels.into_iter().collect();
    if let Some(label_id) = labels
        .iter()
        .copied()
        .find(|label_id| label_id.is_reserved())
    {
        return Err(GraphStoreError::VertexLabel(
            VertexLabelStoreError::ReservedLabelId(label_id),
        ));
    }
    labels.sort_unstable();
    labels.dedup();
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use crate::facade::stable::derived_index_outbox::DerivedIndexOutboxOp;
    use crate::index::{catalog_context, label_pending, pending};
    use candid::Principal;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::canonical_export::{CanonicalExportScope, CanonicalExportTarget};
    use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{
        IndexBuildSubject, IndexMaintenancePhase, IndexedPropertyCatalog, IndexedVertexMembership,
        PhysicalIndexId,
    };
    use ic_stable_lara::VertexId;

    fn configure_routing(store: &GraphStore) {
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_canister: None,
            }))
            .expect("routing");
        pending::clear_pending();
        label_pending::clear_pending();
        store.derived_index_outbox_clear();
    }

    fn register_scope(
        physical_index_id: PhysicalIndexId,
        label_id: u16,
        property_id: PropertyId,
    ) -> CanonicalExportScope {
        let scope = CanonicalExportScope {
            graph_id: GraphId::from_raw(990),
            index_name_id: IndexNameId::from_raw(990),
            catalog_epoch: 1,
            target: CanonicalExportTarget::Vertex {
                label_id,
                property_id,
            },
            inline: None,
        };
        crate::index::canonical_export::register_scope(physical_index_id, scope.clone())
            .expect("register scope");
        scope
    }

    fn seal_scope(physical_index_id: PhysicalIndexId, scope: &CanonicalExportScope) {
        crate::index::canonical_export::seal_scope(physical_index_id, scope.clone(), 2)
            .expect("seal scope");
    }

    fn cleanup_scope(
        physical_index_id: PhysicalIndexId,
        scope: &CanonicalExportScope,
        admitted: u64,
    ) {
        if admitted != 0 {
            crate::index::canonical_export::ack_build_dml(
                physical_index_id,
                scope.catalog_epoch,
                admitted,
            )
            .expect("ack build DML");
        }
        crate::index::canonical_export::abort_scope(physical_index_id, scope.clone())
            .expect("abort scope");
        crate::index::canonical_export::remove_scope(physical_index_id, scope)
            .expect("remove scope");
    }

    fn fixture_vertex(
        store: &GraphStore,
        property_id: PropertyId,
        label_id: Option<u16>,
    ) -> VertexId {
        let vertex_id = store.insert_vertex().expect("vertex");
        if let Some(label_id) = label_id {
            let vertex = store.vertex(vertex_id).expect("vertex row");
            store
                .set_vertex_labels(
                    vertex_id,
                    vertex,
                    [gleaph_graph_kernel::entry::VertexLabelId::from_raw(
                        label_id,
                    )],
                )
                .expect("initial label");
        }
        store
            .set_vertex_property(vertex_id, property_id, Value::Int64(42))
            .expect("property");
        pending::clear_pending();
        label_pending::clear_pending();
        vertex_id
    }

    fn vertex_membership(
        physical_index_id: u64,
        catalog_epoch: u64,
        phase: IndexMaintenancePhase,
        property_id: PropertyId,
        label_id: u16,
    ) -> IndexedVertexMembership {
        IndexedVertexMembership {
            physical_index_id: PhysicalIndexId::new(physical_index_id).expect("physical id"),
            catalog_epoch,
            phase,
            property_id: property_id.raw(),
            label_id,
        }
    }

    fn build_request(
        store: &GraphStore,
        physical_index_id: PhysicalIndexId,
    ) -> gleaph_graph_kernel::index::IndexBuildDmlRequest {
        let entries = store.derived_index_outbox_peek(usize::MAX);
        let Some((_, entry)) = entries.into_iter().find(|(_, entry)| {
            matches!(
                &entry.op,
                DerivedIndexOutboxOp::IndexBuildDml { request }
                    if request.physical_index_id == physical_index_id
            )
        }) else {
            panic!("missing build request for {physical_index_id:?}");
        };
        let DerivedIndexOutboxOp::IndexBuildDml { request } = entry.op else {
            unreachable!();
        };
        request
    }

    fn assert_build_request(
        request: &gleaph_graph_kernel::index::IndexBuildDmlRequest,
        physical_index_id: PhysicalIndexId,
        mutation_id: u64,
        vertex_id: VertexId,
        insert: bool,
        shard_sequence: u64,
    ) {
        assert_eq!(request.physical_index_id, physical_index_id);
        assert_eq!(request.catalog_epoch, 1);
        assert_eq!(request.shard_sequence, shard_sequence);
        assert_eq!(
            request.subject,
            IndexBuildSubject::Vertex {
                shard_id: 0,
                vertex_id: u32::from(vertex_id),
            }
        );
        let payload = gleaph_gql::value_to_index_key_bytes(&Value::Int64(42))
            .expect("sortable value")
            .expect("encoded value");
        if insert {
            assert!(request.removals.is_empty());
            assert_eq!(request.insertions, vec![payload]);
        } else {
            assert_eq!(request.removals, vec![payload]);
            assert!(request.insertions.is_empty());
        }
        let entry = GraphStore::new()
            .derived_index_outbox_peek(usize::MAX)
            .into_iter()
            .find(|(_, entry)| {
                matches!(
                    &entry.op,
                    DerivedIndexOutboxOp::IndexBuildDml { request: candidate }
                        if candidate.physical_index_id == physical_index_id
                )
            })
            .expect("build entry");
        assert_eq!(entry.1.mutation_id, mutation_id);
    }

    fn assert_single_active_property_op(
        operations: &[crate::index::pending::PendingPostingOp],
        physical_index_id: PhysicalIndexId,
        property_id: PropertyId,
        vertex_id: VertexId,
        remove: bool,
    ) {
        assert_eq!(
            operations.len(),
            1,
            "unexpected ordinary property operations"
        );
        let expected_payload = gleaph_gql::value_to_index_key_bytes(&Value::Int64(42))
            .expect("sortable value")
            .expect("encoded value");
        match (remove, &operations[0]) {
            (
                false,
                crate::index::pending::PendingPostingOp::Insert {
                    physical_index_id: actual_physical_index_id,
                    catalog_epoch,
                    phase,
                    property_id: actual_property_id,
                    payload_bytes,
                    vertex_id: actual_vertex_id,
                },
            )
            | (
                true,
                crate::index::pending::PendingPostingOp::Remove {
                    physical_index_id: actual_physical_index_id,
                    catalog_epoch,
                    phase,
                    property_id: actual_property_id,
                    payload_bytes,
                    vertex_id: actual_vertex_id,
                },
            ) => {
                assert_eq!(*actual_physical_index_id, physical_index_id);
                assert_eq!(*catalog_epoch, 1);
                assert!(phase.is_active());
                assert_eq!(*actual_property_id, property_id.raw());
                assert_eq!(payload_bytes, &expected_payload);
                assert_eq!(*actual_vertex_id, u32::from(vertex_id));
            }
            _ => panic!("ordinary property operation has the wrong direction"),
        }
    }

    fn seed_preexisting_rejection_queues(vertex_id: VertexId) {
        let property_id = PropertyId::from_raw(910_199);
        let payload = gleaph_gql::value_to_index_key_bytes(&Value::Int64(7))
            .expect("sortable value")
            .expect("encoded value");
        pending::push_vertex_index_op(
            vertex_id,
            catalog_context::IndexMembershipRef {
                physical_index_id: PhysicalIndexId::new(910_099).expect("preexisting physical id"),
                catalog_epoch: 7,
                phase: IndexMaintenancePhase::Active,
            },
            crate::property::PropertyIndexOp::Insert {
                property_id,
                payload_bytes: payload,
            },
        );
        label_pending::record_vertex_label_set(
            vertex_id,
            &[],
            &[gleaph_graph_kernel::entry::VertexLabelId::from_raw(99)],
        );
    }

    fn assert_preexisting_rejection_queues(
        operations: &[crate::index::pending::PendingPostingOp],
        labels: &[label_pending::PendingLabelOp],
        vertex_id: VertexId,
    ) {
        let expected_payload = gleaph_gql::value_to_index_key_bytes(&Value::Int64(7))
            .expect("sortable value")
            .expect("encoded value");
        assert_eq!(operations.len(), 1);
        assert!(matches!(
            &operations[0],
            crate::index::pending::PendingPostingOp::Insert {
                physical_index_id,
                catalog_epoch,
                phase,
                property_id,
                payload_bytes,
                vertex_id: actual_vertex_id,
            } if *physical_index_id == PhysicalIndexId::new(910_099).unwrap()
                && *catalog_epoch == 7
                && phase.is_active()
                && *property_id == 910_199
                && *payload_bytes == expected_payload
                && *actual_vertex_id == u32::from(vertex_id)
        ));
        assert!(matches!(
            labels,
            [label_pending::PendingLabelOp::Insert { label_id, vertex_id: actual_vertex_id }]
                if *label_id == 99 && *actual_vertex_id == u32::from(vertex_id)
        ));
    }

    #[test]
    fn building_label_gain_emits_exact_property_insert() {
        let store = GraphStore::new();
        configure_routing(&store);
        let property_id = PropertyId::from_raw(910_101);
        let vertex_id = fixture_vertex(&store, property_id, None);
        let target = register_scope(PhysicalIndexId::new(910_001).unwrap(), 1, property_id);
        let decoy = register_scope(PhysicalIndexId::new(910_003).unwrap(), 2, property_id);
        seal_scope(PhysicalIndexId::new(910_003).unwrap(), &decoy);
        let _catalog = catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                vertex_membership(910_001, 1, IndexMaintenancePhase::Building, property_id, 1),
                vertex_membership(910_002, 1, IndexMaintenancePhase::Active, property_id, 1),
                vertex_membership(910_003, 1, IndexMaintenancePhase::Sealing, property_id, 2),
            ],
            ..Default::default()
        });

        let vertex = store.vertex(vertex_id).expect("vertex row");
        store
            .set_vertex_labels_with_mutation_id(
                vertex_id,
                vertex,
                [gleaph_graph_kernel::entry::VertexLabelId::from_raw(1)],
                91_001,
            )
            .expect("building label gain");

        let request = build_request(&store, PhysicalIndexId::new(910_001).unwrap());
        assert_build_request(
            &request,
            PhysicalIndexId::new(910_001).unwrap(),
            91_001,
            vertex_id,
            true,
            1,
        );
        assert_eq!(store.derived_index_outbox_peek(usize::MAX).len(), 1);
        let pending = pending::take_pending();
        assert_single_active_property_op(
            &pending,
            PhysicalIndexId::new(910_002).unwrap(),
            property_id,
            vertex_id,
            false,
        );
        assert!(matches!(
            label_pending::take_pending().as_slice(),
            [label_pending::PendingLabelOp::Insert { label_id: 1, vertex_id: id }] if *id == u32::from(vertex_id)
        ));
        assert_eq!(
            store
                .vertex_labels(vertex_id, store.vertex(vertex_id).unwrap())
                .len(),
            1
        );
        cleanup_scope(PhysicalIndexId::new(910_001).unwrap(), &target, 1);
        cleanup_scope(PhysicalIndexId::new(910_003).unwrap(), &decoy, 0);
        store.derived_index_outbox_clear();
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn building_label_loss_emits_exact_property_remove_once() {
        let store = GraphStore::new();
        configure_routing(&store);
        let property_id = PropertyId::from_raw(910_111);
        let vertex_id = fixture_vertex(&store, property_id, Some(3));
        let target = register_scope(PhysicalIndexId::new(910_011).unwrap(), 3, property_id);
        let decoy = register_scope(PhysicalIndexId::new(910_013).unwrap(), 4, property_id);
        seal_scope(PhysicalIndexId::new(910_013).unwrap(), &decoy);
        let _catalog = catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                vertex_membership(910_011, 1, IndexMaintenancePhase::Building, property_id, 3),
                vertex_membership(910_012, 1, IndexMaintenancePhase::Active, property_id, 3),
                vertex_membership(910_013, 1, IndexMaintenancePhase::Sealing, property_id, 4),
            ],
            ..Default::default()
        });

        let vertex = store.vertex(vertex_id).expect("vertex row");
        store
            .remove_vertex_label_with_mutation_id(
                vertex_id,
                vertex,
                gleaph_graph_kernel::entry::VertexLabelId::from_raw(3),
                91_011,
            )
            .expect("building label loss");
        let request = build_request(&store, PhysicalIndexId::new(910_011).unwrap());
        assert_build_request(
            &request,
            PhysicalIndexId::new(910_011).unwrap(),
            91_011,
            vertex_id,
            false,
            1,
        );
        assert_eq!(store.derived_index_outbox_peek(usize::MAX).len(), 1);
        let pending = pending::take_pending();
        assert_single_active_property_op(
            &pending,
            PhysicalIndexId::new(910_012).unwrap(),
            property_id,
            vertex_id,
            true,
        );
        assert!(matches!(
            label_pending::take_pending().as_slice(),
            [label_pending::PendingLabelOp::Remove { label_id: 3, vertex_id: id }] if *id == u32::from(vertex_id)
        ));
        cleanup_scope(PhysicalIndexId::new(910_011).unwrap(), &target, 1);
        cleanup_scope(PhysicalIndexId::new(910_013).unwrap(), &decoy, 0);
        store.derived_index_outbox_clear();
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn sealing_label_gain_rejects_before_any_mutation() {
        let store = GraphStore::new();
        configure_routing(&store);
        let property_id = PropertyId::from_raw(910_121);
        let vertex_id = fixture_vertex(&store, property_id, None);
        let building = register_scope(PhysicalIndexId::new(910_021).unwrap(), 5, property_id);
        let sealing = register_scope(PhysicalIndexId::new(910_022).unwrap(), 5, property_id);
        seal_scope(PhysicalIndexId::new(910_022).unwrap(), &sealing);
        let decoy = register_scope(PhysicalIndexId::new(910_023).unwrap(), 6, property_id);
        seal_scope(PhysicalIndexId::new(910_023).unwrap(), &decoy);
        let _catalog = catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                vertex_membership(910_021, 1, IndexMaintenancePhase::Building, property_id, 5),
                vertex_membership(910_022, 1, IndexMaintenancePhase::Sealing, property_id, 5),
                vertex_membership(910_023, 1, IndexMaintenancePhase::Sealing, property_id, 6),
            ],
            ..Default::default()
        });
        let before_vertex = store.vertex(vertex_id).expect("vertex row");
        let before_labels = store.vertex_labels(vertex_id, before_vertex);
        let before_outbox = store.derived_index_outbox_peek(usize::MAX);
        let before_building =
            crate::index::canonical_export::scope_status(PhysicalIndexId::new(910_021).unwrap())
                .unwrap();
        let before_sealing =
            crate::index::canonical_export::scope_status(PhysicalIndexId::new(910_022).unwrap())
                .unwrap();
        seed_preexisting_rejection_queues(vertex_id);
        let error = store
            .add_vertex_label_with_mutation_id(
                vertex_id,
                before_vertex,
                gleaph_graph_kernel::entry::VertexLabelId::from_raw(5),
                91_021,
            )
            .expect_err("sealing label gain must reject");
        assert!(matches!(
            error,
            crate::facade::GraphStoreError::IndexBuildAdmission(
                gleaph_graph_kernel::canonical_export::CanonicalExportError::RetryableSealing
            )
        ));
        let after_vertex = store.vertex(vertex_id).expect("vertex row");
        assert_eq!(store.vertex_labels(vertex_id, after_vertex), before_labels);
        assert_eq!(after_vertex, before_vertex);
        assert_eq!(store.derived_index_outbox_peek(usize::MAX), before_outbox);
        assert_eq!(
            crate::index::canonical_export::scope_status(PhysicalIndexId::new(910_021).unwrap())
                .unwrap(),
            before_building
        );
        assert_eq!(
            crate::index::canonical_export::scope_status(PhysicalIndexId::new(910_022).unwrap())
                .unwrap(),
            before_sealing
        );
        let pending_after = pending::take_pending();
        let labels_after = label_pending::take_pending();
        assert_preexisting_rejection_queues(&pending_after, &labels_after, vertex_id);
        cleanup_scope(PhysicalIndexId::new(910_021).unwrap(), &building, 0);
        cleanup_scope(PhysicalIndexId::new(910_022).unwrap(), &sealing, 0);
        cleanup_scope(PhysicalIndexId::new(910_023).unwrap(), &decoy, 0);
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn sealing_label_loss_rejects_before_any_mutation() {
        let store = GraphStore::new();
        configure_routing(&store);
        let property_id = PropertyId::from_raw(910_131);
        let vertex_id = fixture_vertex(&store, property_id, Some(7));
        let building = register_scope(PhysicalIndexId::new(910_031).unwrap(), 7, property_id);
        let sealing = register_scope(PhysicalIndexId::new(910_032).unwrap(), 7, property_id);
        seal_scope(PhysicalIndexId::new(910_032).unwrap(), &sealing);
        let decoy = register_scope(PhysicalIndexId::new(910_033).unwrap(), 8, property_id);
        seal_scope(PhysicalIndexId::new(910_033).unwrap(), &decoy);
        let _catalog = catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                vertex_membership(910_031, 1, IndexMaintenancePhase::Building, property_id, 7),
                vertex_membership(910_032, 1, IndexMaintenancePhase::Sealing, property_id, 7),
                vertex_membership(910_033, 1, IndexMaintenancePhase::Sealing, property_id, 8),
            ],
            ..Default::default()
        });
        let before_vertex = store.vertex(vertex_id).expect("vertex row");
        let before_labels = store.vertex_labels(vertex_id, before_vertex);
        let before_outbox = store.derived_index_outbox_peek(usize::MAX);
        let before_building =
            crate::index::canonical_export::scope_status(PhysicalIndexId::new(910_031).unwrap())
                .unwrap();
        let before_sealing =
            crate::index::canonical_export::scope_status(PhysicalIndexId::new(910_032).unwrap())
                .unwrap();
        seed_preexisting_rejection_queues(vertex_id);
        let error = store
            .remove_vertex_label_with_mutation_id(
                vertex_id,
                before_vertex,
                gleaph_graph_kernel::entry::VertexLabelId::from_raw(7),
                91_031,
            )
            .expect_err("sealing label loss must reject");
        assert!(matches!(
            error,
            crate::facade::GraphStoreError::IndexBuildAdmission(
                gleaph_graph_kernel::canonical_export::CanonicalExportError::RetryableSealing
            )
        ));
        let after_vertex = store.vertex(vertex_id).expect("vertex row");
        assert_eq!(store.vertex_labels(vertex_id, after_vertex), before_labels);
        assert_eq!(after_vertex, before_vertex);
        assert_eq!(store.derived_index_outbox_peek(usize::MAX), before_outbox);
        assert_eq!(
            crate::index::canonical_export::scope_status(PhysicalIndexId::new(910_031).unwrap())
                .unwrap(),
            before_building
        );
        assert_eq!(
            crate::index::canonical_export::scope_status(PhysicalIndexId::new(910_032).unwrap())
                .unwrap(),
            before_sealing
        );
        let pending_after = pending::take_pending();
        let labels_after = label_pending::take_pending();
        assert_preexisting_rejection_queues(&pending_after, &labels_after, vertex_id);
        cleanup_scope(PhysicalIndexId::new(910_031).unwrap(), &building, 0);
        cleanup_scope(PhysicalIndexId::new(910_032).unwrap(), &sealing, 0);
        cleanup_scope(PhysicalIndexId::new(910_033).unwrap(), &decoy, 0);
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn public_mutation_id_zero_label_wrappers_preserve_index_build_admission() {
        let store = GraphStore::new();
        configure_routing(&store);
        let first_property = PropertyId::from_raw(910_141);
        let second_property = PropertyId::from_raw(910_142);
        let third_property = PropertyId::from_raw(910_143);
        let first_target_physical = PhysicalIndexId::new(910_041).unwrap();
        let second_target_physical = PhysicalIndexId::new(910_042).unwrap();
        let third_target_physical = PhysicalIndexId::new(910_043).unwrap();
        let decoy_physical = PhysicalIndexId::new(910_044).unwrap();
        let first_target_scope = register_scope(first_target_physical, 9, first_property);
        let second_target_scope = register_scope(second_target_physical, 9, second_property);
        let third_target_scope = register_scope(third_target_physical, 9, third_property);
        let decoy_scope = register_scope(decoy_physical, 10, second_property);
        seal_scope(decoy_physical, &decoy_scope);
        let first_vertex = fixture_vertex(&store, first_property, None);
        let second_vertex = fixture_vertex(&store, second_property, Some(10));
        let third_vertex = fixture_vertex(&store, third_property, Some(9));
        pending::clear_pending();
        label_pending::clear_pending();
        store.derived_index_outbox_clear();
        let _catalog = catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                vertex_membership(
                    first_target_physical.raw(),
                    1,
                    IndexMaintenancePhase::Building,
                    first_property,
                    9,
                ),
                vertex_membership(
                    second_target_physical.raw(),
                    1,
                    IndexMaintenancePhase::Building,
                    second_property,
                    9,
                ),
                vertex_membership(
                    third_target_physical.raw(),
                    1,
                    IndexMaintenancePhase::Building,
                    third_property,
                    9,
                ),
                vertex_membership(
                    decoy_physical.raw(),
                    1,
                    IndexMaintenancePhase::Sealing,
                    second_property,
                    10,
                ),
            ],
            ..Default::default()
        });

        let assert_wrapper_admission = |vertex_id: VertexId,
                                        physical_index_id: PhysicalIndexId,
                                        insert: bool,
                                        sequence: u64| {
            let request = build_request(&store, physical_index_id);
            assert_build_request(&request, physical_index_id, 0, vertex_id, insert, sequence);
            let entries = store.derived_index_outbox_peek(usize::MAX);
            assert_eq!(entries.len(), 1);
            assert!(pending::take_pending().is_empty());
            let labels = label_pending::take_pending();
            assert_eq!(labels.len(), 1);
            if insert {
                assert!(matches!(
                    labels.as_slice(),
                    [label_pending::PendingLabelOp::Insert { label_id: 9, vertex_id: id }]
                        if *id == u32::from(vertex_id)
                ));
            } else {
                assert!(matches!(
                    labels.as_slice(),
                    [label_pending::PendingLabelOp::Remove { label_id: 9, vertex_id: id }]
                    if *id == u32::from(vertex_id)
                ));
            }
            crate::index::canonical_export::ack_build_dml(physical_index_id, 1, sequence)
                .expect("ack wrapper admission");
            store.derived_index_outbox_clear();
        };

        let first_row = store.vertex(first_vertex).expect("first row");
        store
            .set_vertex_labels(first_vertex, first_row, [VertexLabelId::from_raw(9)])
            .expect("public set wrapper");
        assert_wrapper_admission(first_vertex, first_target_physical, true, 1);

        let second_row = store.vertex(second_vertex).expect("second row");
        store
            .add_vertex_label(second_vertex, second_row, VertexLabelId::from_raw(9))
            .expect("public add wrapper");
        assert_wrapper_admission(second_vertex, second_target_physical, true, 1);

        let third_row = store.vertex(third_vertex).expect("third row");
        store
            .remove_vertex_label(third_vertex, third_row, VertexLabelId::from_raw(9))
            .expect("public remove wrapper");
        assert_wrapper_admission(third_vertex, third_target_physical, false, 1);

        cleanup_scope(first_target_physical, &first_target_scope, 0);
        cleanup_scope(second_target_physical, &second_target_scope, 0);
        cleanup_scope(third_target_physical, &third_target_scope, 0);
        cleanup_scope(decoy_physical, &decoy_scope, 0);
        store.derived_index_outbox_clear();
        store.set_federation_routing(None).expect("clear routing");
    }
}
