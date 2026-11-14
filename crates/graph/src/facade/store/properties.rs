//! Property storage domain: primary stores plus derived index-maintenance events.

use super::super::stable::edge_properties::EdgePropertyKey;
use super::super::stable::vertex_properties::{VertexPropertyKey, validate_property_assignment};
use super::super::stable::{EDGE_PROPERTIES, VERTEX_PROPERTIES};
use crate::facade::store::index_build_admission::PlannedBuildEnvelope;
use crate::property::{
    PropertyValueChange, commit_property_index_ops, dispatch_property_index_ops,
    dispatch_vertex_property_index_ops_bulk, index_build_subject_for_change,
    preflight_property_index_ops,
};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::index::IndexBuildSubject;
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_lara::{VertexId, labeled::CanonicalEdgeOccurrence};

use super::GraphStore;
use super::error::GraphStoreError;

impl GraphStore {
    pub(crate) fn commit_vertex_property_writes_bulk(
        &self,
        assignments: &[(VertexId, PropertyId, Value)],
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        // Pure validation first: the stable-map `set` boundary only checks the property id and
        // persistability, so validating both here makes every post-fence stable write infallible.
        for (_, property_id, value) in assignments {
            crate::property::ensure_property_id(*property_id)
                .map_err(|id| GraphStoreError::PropertyValue(super::super::stable::vertex_properties::VertexPropertyStoreError::ReservedPropertyId(id)))?;
            crate::property::ensure_persistable(value).map_err(|error| {
                GraphStoreError::PropertyValue(
                    super::super::stable::vertex_properties::VertexPropertyStoreError::InvalidValue(
                        error,
                    ),
                )
            })?;
        }
        let previous = VERTEX_PROPERTIES.with_borrow(|store| {
            assignments
                .iter()
                .map(|(vertex_id, property_id, _)| {
                    (
                        *vertex_id,
                        *property_id,
                        store.get(*vertex_id, *property_id),
                    )
                })
                .collect::<Vec<_>>()
        });
        let changes = assignments
            .iter()
            .zip(previous.iter())
            .map(|((vertex_id, property_id, value), (_, _, previous))| {
                PropertyValueChange::vertex(
                    *vertex_id,
                    *property_id,
                    previous.as_ref(),
                    Some(value),
                )
            })
            .collect::<Vec<_>>();
        // Plan (pure) every assignment before any stable write so one stale membership cannot
        // leave another namespace's durable counter advanced.
        let mut pending_commit: Vec<(IndexBuildSubject, Vec<PlannedBuildEnvelope>)> = Vec::new();
        for change in &changes {
            let planned = preflight_property_index_ops(*change)?;
            if !planned.is_empty() {
                pending_commit.push((index_build_subject_for_change(*change)?, planned));
            }
        }
        // Commit (infallible): reserve sequences and append envelopes before the canonical writes.
        for (subject, planned) in pending_commit {
            commit_property_index_ops(mutation_id, subject, planned);
        }
        VERTEX_PROPERTIES
            .with_borrow_mut(|store| {
                assignments
                    .iter()
                    .map(|(vertex_id, property_id, value)| {
                        store
                            .set(*vertex_id, *property_id, value.clone())
                            .map(|_| ())
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .expect("bulk vertex property assignments were pre-validated and must be writable");
        let changes = assignments
            .iter()
            .zip(previous.iter())
            .map(|((vertex_id, property_id, value), (_, _, previous))| {
                (*vertex_id, *property_id, previous.as_ref(), value)
            })
            .collect::<Vec<_>>();
        dispatch_vertex_property_index_ops_bulk(&changes);
        Ok(())
    }

    /// Write a vertex property and enqueue federated index maintenance when enabled.
    pub(super) fn commit_vertex_property_write(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
        record_index_pending: bool,
        mutation_id: MutationId,
    ) -> Result<Option<Value>, GraphStoreError> {
        // Reject invalid input before a first access can initialize the stable map.
        validate_property_assignment(property_id, &value)
            .map_err(GraphStoreError::PropertyValue)?;
        let previous = VERTEX_PROPERTIES.with_borrow_mut(|properties| {
            let prepared = properties
                .prepare_change(VertexPropertyKey::new(vertex_id, property_id), Some(&value))
                .map_err(GraphStoreError::PropertyValue)?;
            if record_index_pending {
                let change = PropertyValueChange::vertex(
                    vertex_id,
                    property_id,
                    prepared.previous(),
                    Some(&value),
                );
                let planned = preflight_property_index_ops(change)?;
                if !planned.is_empty() {
                    commit_property_index_ops(
                        mutation_id,
                        index_build_subject_for_change(change)?,
                        planned,
                    );
                }
            }
            Ok::<_, GraphStoreError>(prepared.commit())
        })?;
        // Active/TEXT dispatch runs only after the exclusive primary-store borrow is released.
        if record_index_pending {
            dispatch_property_index_ops(PropertyValueChange::vertex(
                vertex_id,
                property_id,
                previous.as_ref(),
                Some(&value),
            ));
        }
        Ok(previous)
    }

    /// Remove a vertex property and enqueue federated index maintenance when enabled.
    pub(super) fn commit_vertex_property_remove(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        mutation_id: MutationId,
    ) -> Result<Option<Value>, GraphStoreError> {
        let previous = VERTEX_PROPERTIES.with_borrow_mut(|properties| {
            let prepared = properties
                .prepare_change(VertexPropertyKey::new(vertex_id, property_id), None)
                .map_err(GraphStoreError::PropertyValue)?;
            if let Some(old) = prepared.previous() {
                let change = PropertyValueChange::vertex(vertex_id, property_id, Some(old), None);
                let planned = preflight_property_index_ops(change)?;
                if !planned.is_empty() {
                    commit_property_index_ops(
                        mutation_id,
                        index_build_subject_for_change(change)?,
                        planned,
                    );
                }
            }
            Ok::<_, GraphStoreError>(prepared.commit())
        })?;
        if let Some(old) = previous.as_ref() {
            dispatch_property_index_ops(PropertyValueChange::vertex(
                vertex_id,
                property_id,
                Some(old),
                None,
            ));
        }
        Ok(previous)
    }

    /// Write an edge property on a canonical handle and update local equality postings.
    ///
    /// The caller supplies an explicit orientation-aware [`CanonicalEdgeOccurrence`].
    /// CounterpartScan resolves the canonical sidecar handle before any `EDGE_PROPERTIES`
    /// mutation; a lookup failure leaves sidecar and index state unchanged.
    pub(super) fn commit_edge_property_write(
        &self,
        occurrence: CanonicalEdgeOccurrence,
        property_id: PropertyId,
        value: Value,
        mutation_id: MutationId,
    ) -> Result<Option<Value>, GraphStoreError> {
        let handle = self.canonical_edge_handle_from_occurrence(occurrence)?;
        self.commit_edge_property_write_at_canonical(handle, property_id, value, mutation_id)
    }

    /// Write a property when the caller already owns an exact canonical sidecar handle.
    ///
    /// Batch insertion uses this after LARA has returned the captured location. The
    /// batch preflight has already validated every id and value, so this path does
    /// not perform a counterpart scan or introduce another fallible lookup.
    pub(crate) fn commit_edge_property_write_at_canonical(
        &self,
        handle: super::handle::EdgeHandle,
        property_id: PropertyId,
        value: Value,
        mutation_id: MutationId,
    ) -> Result<Option<Value>, GraphStoreError> {
        validate_property_assignment(property_id, &value)
            .map_err(GraphStoreError::PropertyValue)?;
        let key = EdgePropertyKey::new(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index.raw(),
            property_id,
        );
        let previous = EDGE_PROPERTIES.with_borrow_mut(|properties| {
            let prepared = properties
                .prepare_change(key, Some(&value))
                .map_err(GraphStoreError::PropertyValue)?;
            let change = PropertyValueChange::edge(
                key.owner_vertex_id(),
                key.label_id(),
                key.slot_index(),
                key.property_id(),
                prepared.previous(),
                Some(&value),
            );
            let planned = preflight_property_index_ops(change)?;
            if !planned.is_empty() {
                commit_property_index_ops(
                    mutation_id,
                    index_build_subject_for_change(change)?,
                    planned,
                );
            }
            Ok::<_, GraphStoreError>(prepared.commit())
        })?;
        dispatch_property_index_ops(PropertyValueChange::edge(
            key.owner_vertex_id(),
            key.label_id(),
            key.slot_index(),
            key.property_id(),
            previous.as_ref(),
            Some(&value),
        ));
        Ok(previous)
    }

    /// Co-write a preflighted set of initial properties at one canonical edge.
    ///
    /// The caller validates the complete set before LARA commit. This method keeps
    /// the primary sidecar writes and their derived-event dispatch under one Graph
    /// boundary; a post-preflight storage failure is an invariant violation.
    pub(crate) fn commit_edge_property_writes_at_canonical(
        &self,
        handle: super::handle::EdgeHandle,
        properties: &[(PropertyId, Value)],
    ) {
        let previous = EDGE_PROPERTIES.with_borrow_mut(|store| {
            properties
                .iter()
                .map(|(property_id, value)| {
                    let previous = store
                        .set(
                            handle.owner_vertex_id,
                            handle.label_id.raw(),
                            handle.slot_index.raw(),
                            *property_id,
                            value.clone(),
                        )
                        .expect("validated batch sidecar must be writable");
                    (*property_id, previous)
                })
                .collect::<Vec<_>>()
        });
        for ((property_id, value), (_, previous)) in properties.iter().zip(previous) {
            dispatch_property_index_ops(PropertyValueChange::edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
                *property_id,
                previous.as_ref(),
                Some(value),
            ));
        }
    }

    /// Remove an edge property on a canonical handle and update local equality postings.
    pub(super) fn commit_edge_property_remove(
        &self,
        occurrence: CanonicalEdgeOccurrence,
        property_id: PropertyId,
        mutation_id: MutationId,
    ) -> Result<Option<Value>, GraphStoreError> {
        let handle = self.canonical_edge_handle_from_occurrence(occurrence)?;
        let key = EdgePropertyKey::new(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index.raw(),
            property_id,
        );
        let previous = EDGE_PROPERTIES.with_borrow_mut(|properties| {
            let prepared = properties
                .prepare_change(key, None)
                .map_err(GraphStoreError::PropertyValue)?;
            if let Some(old) = prepared.previous() {
                let change = PropertyValueChange::edge(
                    key.owner_vertex_id(),
                    key.label_id(),
                    key.slot_index(),
                    key.property_id(),
                    Some(old),
                    None,
                );
                let planned = preflight_property_index_ops(change)?;
                if !planned.is_empty() {
                    commit_property_index_ops(
                        mutation_id,
                        index_build_subject_for_change(change)?,
                        planned,
                    );
                }
            }
            Ok::<_, GraphStoreError>(prepared.commit())
        })?;
        if let Some(old) = previous.as_ref() {
            dispatch_property_index_ops(PropertyValueChange::edge(
                key.owner_vertex_id(),
                key.label_id(),
                key.slot_index(),
                key.property_id(),
                Some(old),
                None,
            ));
        }
        Ok(previous)
    }

    /// Remove every edge property on a canonical handle.
    ///
    /// The caller must supply an explicit orientation-aware occurrence; this method resolves the
    /// canonical sidecar handle before the removal.
    pub(super) fn commit_remove_all_edge_properties(
        &self,
        occurrence: CanonicalEdgeOccurrence,
    ) -> Result<(), GraphStoreError> {
        let handle = self.canonical_edge_handle_from_occurrence(occurrence)?;
        self.commit_remove_all_edge_properties_at_canonical(handle);
        Ok(())
    }

    pub(super) fn commit_remove_all_edge_properties_at_canonical(
        &self,
        handle: super::handle::EdgeHandle,
    ) {
        EDGE_PROPERTIES.with_borrow_mut(|store| {
            store.remove_all_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
            );
        });
    }

    pub(super) fn commit_move_edge_properties(
        owner_vertex_id: VertexId,
        moved: ic_stable_lara::labeled::EdgeSlotMove,
    ) -> Vec<(PropertyId, Value)> {
        let label_id = moved.label_id.raw();
        EDGE_PROPERTIES.with_borrow_mut(|store| {
            store
                .move_all_for_edge(
                    owner_vertex_id,
                    label_id,
                    moved.old_slot_index,
                    moved.new_slot_index,
                )
                .expect("stored edge property values remain encodable")
        })
    }

    /// Remove every vertex property and dispatch Active removals only.
    ///
    /// The vertex-delete entrypoint admits Building removals through the index-build fence
    /// before calling this; this method never lets Building/Sealing work into the ordinary queue.
    pub(super) fn commit_clear_vertex_properties(&self, vertex_id: VertexId) {
        let props: Vec<PropertyId> = VERTEX_PROPERTIES.with_borrow(|store| {
            store
                .properties_for(vertex_id)
                .into_iter()
                .map(|(pid, _)| pid)
                .collect()
        });
        for property_id in props {
            let prev = VERTEX_PROPERTIES.with_borrow(|p| p.get(vertex_id, property_id));
            if let Some(ref old) = prev {
                let change = PropertyValueChange::vertex(vertex_id, property_id, Some(old), None);
                let _ = VERTEX_PROPERTIES.with_borrow_mut(|p| p.remove(vertex_id, property_id));
                dispatch_property_index_ops(change);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::stable::vertex_properties::VertexPropertyStoreError;
    use super::super::helpers::{build_edge_to, edge_storage_label, lara_label};
    use super::*;
    use ic_stable_lara::labeled::EdgePlacementPolicy;

    #[derive(Clone, Debug)]
    struct Unencodable;

    impl std::fmt::Display for Unencodable {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("unencodable")
        }
    }

    impl gleaph_gql::ExtensionValue for Unencodable {
        fn type_name(&self) -> &str {
            "unencodable"
        }
        fn clone_box(&self) -> Box<dyn gleaph_gql::ExtensionValue> {
            Box::new(self.clone())
        }
        fn eq_ext(&self, other: &dyn gleaph_gql::ExtensionValue) -> bool {
            other.as_any().is::<Self>()
        }
        fn cmp_ext(&self, other: &dyn gleaph_gql::ExtensionValue) -> Option<std::cmp::Ordering> {
            other
                .as_any()
                .is::<Self>()
                .then_some(std::cmp::Ordering::Equal)
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn invalid_scalar_assignment_does_not_initialize_property_stores() {
        let store = GraphStore::new();
        let vertex = store.insert_vertex().unwrap();
        let neighbor = store.insert_vertex().unwrap();
        // Graph's ordinary insertion opens the sidecar store even for an empty property list.
        // Seed real topology through LARA so this admission test starts with both stores unopened.
        let label = lara_label(edge_storage_label(None, false));
        let locations = store
            .with_graph_mut(|graph| {
                graph.insert_directed_edge_with_locations(
                    vertex,
                    neighbor,
                    label,
                    build_edge_to(neighbor),
                    build_edge_to(vertex),
                    ic_stable_lara::labeled::bidirectional::EdgeInsertPolicy {
                        placement: EdgePlacementPolicy::Unordered,
                        policy_for_label: &|_| EdgePlacementPolicy::Unordered,
                    },
                )
            })
            .unwrap();
        let edge = super::super::handle::EdgeHandle::at_slot(
            vertex,
            label,
            locations
                .forward
                .expect("inserted forward occurrence")
                .logical_slot,
        );
        let property_pages = || {
            super::super::super::stable::memory::stable_memory_stats()
                .regions
                .into_iter()
                .filter(|region| {
                    matches!(
                        region.name.as_str(),
                        "vertex_properties" | "edge_properties"
                    )
                })
                .map(|region| region.logical_pages)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            property_pages(),
            vec![0, 0],
            "fixture must leave both owners unopened"
        );
        let assert_rejected = |result: Result<Option<Value>, GraphStoreError>,
                               expected: &VertexPropertyStoreError| {
            assert!(
                matches!(result, Err(GraphStoreError::PropertyValue(ref actual)) if actual == expected),
                "{result:?}"
            );
            assert_eq!(property_pages(), vec![0, 0]);
        };
        for (property, value, error) in [
            (
                PropertyId::default(),
                Value::Null,
                VertexPropertyStoreError::ReservedPropertyId(PropertyId::default()),
            ),
            (
                PropertyId::from_raw(1),
                Value::Extension(Box::new(Unencodable)),
                VertexPropertyStoreError::InvalidValue(
                    gleaph_gql::ValueBinaryError::InvalidExtensionType,
                ),
            ),
        ] {
            assert_rejected(
                store.set_vertex_property(vertex, property, value.clone()),
                &error,
            );
            assert_rejected(
                store.commit_edge_property_write_at_canonical(edge, property, value, 0.into()),
                &error,
            );
        }
    }
}
