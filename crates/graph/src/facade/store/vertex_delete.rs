//! Vertex delete domain: clear derived sidecars and commit graph row removal.

use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::index::IndexBuildSubject;
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_lara::{
    DeferredBidirectionalLabeledError, VertexId,
    labeled::{LabeledOrientation, OutEdgeOrder},
    traits::CsrEdge,
};
use std::collections::BTreeSet;

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use crate::facade::store::index_build_admission::{
    FencedTransition, PlannedBuildEnvelope, trap_post_fence_commit,
};

/// One incident edge whose canonical removal will clear its derived postings.
///
/// `canonical` is the forward-owned sidecar handle (the same handle the purge's
/// [`GraphDeleteEdgeObserver`] clears), resolved through CounterpartScan because the
/// forward and reverse bucket slot indices are independent.
///
/// [`GraphDeleteEdgeObserver`]: super::helpers::GraphDeleteEdgeObserver
struct IncidentEdgeRef {
    canonical: EdgeHandle,
    inline_bytes: Vec<u8>,
}

/// Upper bound on pre-compaction drain passes for one vertex delete. The purge's step-0
/// compaction is checkpointed per maintenance item, so a huge span may need more than one
/// timer-budgeted pass; this cap keeps a single delete message bounded while covering
/// realistic backlogs.
const PRE_COMPACT_DRAIN_PASS_LIMIT: usize = 8;

impl GraphStore {
    /// Detached vertex delete: clear sidecars, remove CSR row, drain maintenance.
    pub(super) fn commit_delete_detached_vertex(
        &self,
        vertex_id: VertexId,
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        self.assert_local_vertex_writable(vertex_id)?;
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        if self.vertex_has_incident_edges(vertex_id)? {
            return Err(GraphStoreError::VertexNotDetached { vertex_id });
        }
        // Fence the vertex's own property removals before any canonical write. With no
        // Building/Sealing membership the plans are empty, so skip the resolution entirely.
        if crate::index::catalog_context::has_non_active_membership() {
            let planned = self.plan_vertex_property_removals(vertex_id)?;
            self.commit_vertex_delete_planned(mutation_id, planned);
        }
        // The fence commit is the first stable write of this mutation, so no recoverable error
        // may be returned past this point: the IC would keep the reserved outbox admission while
        // skipping the canonical delete. The vertex was validated writable above; a trap rolls
        // the whole message back instead of exposing canonical state without its build-DML.
        self.commit_prepare_vertex_sidecars_for_delete(vertex_id, mutation_id)
            .unwrap_or_else(trap_post_fence_commit);
        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))
            .map_err(GraphStoreError::from)
            .unwrap_or_else(trap_post_fence_commit);
        self.drain_deferred_maintenance()
            .unwrap_or_else(trap_post_fence_commit);
        Ok(())
    }

    /// Detach-delete: resumable tombstone-first vertex purge (ADR 0021 Stage 2).
    ///
    /// Clears the vertex's own sidecars, marks it pending-purge so the read gate
    /// hides its surviving back-edges, tombstones both orientation rows in O(1)
    /// (preserving buckets), then enqueues a [`MaintenanceWorkItem::DeleteVertex`]
    /// purge and drains it under the delete budget. The incident-edge sidecars are
    /// cleared incrementally by [`GraphDeleteEdgeObserver`] as the purge drains
    /// each edge, and the vertex leaves the pending-purge set when the purge
    /// completes. Super-node deletes that exceed the per-message budget spill to
    /// the maintenance timer instead of trapping, so the legacy synchronous
    /// degree ceiling is gone.
    ///
    /// [`MaintenanceWorkItem::DeleteVertex`]: ic_stable_lara::labeled::MaintenanceWorkItem
    /// [`GraphDeleteEdgeObserver`]: super::helpers::GraphDeleteEdgeObserver
    pub(super) fn commit_detach_delete_vertex(
        &self,
        vertex_id: VertexId,
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        self.assert_local_vertex_writable(vertex_id)?;
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        // The fence is work only when a Building/Sealing membership exists: every transition is
        // derived from catalog memberships, so with none the plans are empty and there is nothing
        // to reject or commit. The span compaction below is required regardless (purge step 0).
        let fenced = crate::index::catalog_context::has_non_active_membership();
        if fenced {
            // Plan (pure) every affected transition (the vertex's own property removals and every
            // incident edge's inline + sidecar removals) BEFORE any canonical write, so a Sealing
            // membership rejects the whole delete with nothing written. The envelopes are
            // discarded: the purge's step-0 span compaction runs next and the slots are
            // re-resolved afterwards.
            self.validate_vertex_delete_admission(vertex_id)?;
        }
        // Compact the vertex's own spans (the purge's step-0 compaction) BEFORE the fence
        // commit. The purge publishes slot moves only in that first step; committing the
        // removal envelopes after it keys them to the final slots, so a post-fence move can
        // never insert postings at the new slots of edges that are about to be deleted.
        self.pre_compact_vertex_delete_spans(vertex_id)?;
        if fenced {
            let mut planned = self.plan_vertex_property_removals(vertex_id)?;
            for edge_ref in self.collect_incident_edge_refs(vertex_id)? {
                planned.extend(self.plan_incident_edge_removals(edge_ref)?);
            }
            self.commit_vertex_delete_planned(mutation_id, planned);
        }
        // The fence commit (and the span pre-compaction before it) is the first stable write of
        // this mutation, so no recoverable error may be returned past this point: the IC would
        // keep the reserved outbox admissions while skipping the tombstone purge. The vertex
        // and its incident edges were validated live above; a trap rolls the whole message back
        // instead of exposing canonical state without its build-DML.
        self.commit_prepare_vertex_sidecars_for_delete(vertex_id, mutation_id)
            .unwrap_or_else(trap_post_fence_commit);
        // Gate before tombstone: if marking fails we must not tombstone, or the
        // vertex's surviving incident edges would be visible as ghost edges
        // (ADR 0021). Post-fence a failure traps so the whole message (including the
        // gate) rolls back atomically.
        self.mark_vertex_pending_purge(vertex_id)
            .unwrap_or_else(trap_post_fence_commit);
        self.with_graph_mut(|graph| graph.begin_vertex_delete_deferred(vertex_id))
            .map_err(GraphStoreError::from)
            .unwrap_or_else(trap_post_fence_commit);
        self.drain_deferred_maintenance()
            .unwrap_or_else(trap_post_fence_commit);
        Ok(())
    }

    /// Pure admission validation for one detach-delete (no sequences reserved, no writes).
    ///
    /// Rejects a Sealing membership cleanly before the pre-compaction moves any canonical slot.
    fn validate_vertex_delete_admission(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.plan_vertex_property_removals(vertex_id)?;
        for edge_ref in self.collect_incident_edge_refs(vertex_id)? {
            self.plan_incident_edge_removals(edge_ref)?;
        }
        Ok(())
    }

    /// Runs the purge's step-0 span compaction (forward + reverse) with the standard move
    /// observers wired, so its slot moves (and their exact move envelopes) are committed before
    /// the fence commits the removal envelopes.
    fn pre_compact_vertex_delete_spans(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.with_graph_mut(|graph| {
            graph.mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                vertex_id,
                0,
                &Self::maintenance_policy_for_label,
            )?;
            graph.mark_compact_vertex_edge_span(
                LabeledOrientation::Reverse,
                vertex_id,
                0,
                &Self::maintenance_policy_for_label,
            )?;
            Ok::<(), GraphStoreError>(())
        })?;
        // Drain the queued compaction to completion under a bounded number of passes. Native
        // builds drain fully in one pass; on canisters the timer budget may need several for a
        // large span. The purge's own step-0 compaction then finds an already-compacted span.
        // The drain can commit build-DML move envelopes (slot-move rekeys), so once it starts a
        // failure must trap rather than return a recoverable error after the first stable write.
        let budget = crate::facade::delete_maintenance_budget();
        for _ in 0..PRE_COMPACT_DRAIN_PASS_LIMIT {
            self.run_maintenance_best_effort(budget)
                .unwrap_or_else(trap_post_fence_commit);
            if self.maintenance_queue_len() == 0 {
                break;
            }
        }
        Ok(())
    }

    /// Property and label sidecars before a vertex CSR row is removed.
    ///
    /// The caller must have already admitted the affected Building removals through the
    /// index-build fence; this method only clears canonical sidecars and dispatches Active
    /// removals to the ordinary queue.
    pub(super) fn commit_prepare_vertex_sidecars_for_delete(
        &self,
        vertex_id: VertexId,
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        self.commit_clear_vertex_properties(vertex_id);
        // The graph no longer stores embedding bytes (ADR 0064 §1), so it cannot enumerate a
        // vertex's embeddings; it over-notifies by dispatching a remove for every indexed name.
        crate::index::vector_dispatch::dispatch_vertex_removes_for_all_indexed(
            vertex_id,
            mutation_id,
        );

        let vertex = self.vertex(vertex_id).ok_or_else(|| {
            GraphStoreError::Graph(DeferredBidirectionalLabeledError::VertexOutOfRange {
                vid: vertex_id,
                len: self.vertex_count(),
            })
        })?;
        // Label sidecars live in `VERTEX_LABELS`; the CSR row is unchanged. Do not call
        // `set_vertex` here: it mirrors the forward row into reverse and would corrupt
        // reverse-only locator state for this `VertexId`.
        self.commit_clear_vertex_labels(vertex_id, vertex)
    }

    /// Plans the removal transitions for every indexed property on the vertex itself.
    fn plan_vertex_property_removals(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<(IndexBuildSubject, Vec<PlannedBuildEnvelope>)>, GraphStoreError> {
        // Existence gate first: a missing vertex row owns no posting state.
        if self.vertex(vertex_id).is_none() {
            return Err(GraphStoreError::Graph(
                DeferredBidirectionalLabeledError::VertexOutOfRange {
                    vid: vertex_id,
                    len: self.vertex_count(),
                },
            ));
        }
        let props: Vec<PropertyId> = super::super::stable::VERTEX_PROPERTIES.with_borrow(|store| {
            store
                .properties_for(vertex_id)
                .into_iter()
                .map(|(pid, _)| pid)
                .collect()
        });
        let mut groups = Vec::new();
        for property_id in props {
            let value = super::super::stable::VERTEX_PROPERTIES
                .with_borrow(|store| store.get(vertex_id, property_id));
            let Some(value) = value else {
                continue;
            };
            let change = crate::property::PropertyValueChange::vertex(
                vertex_id,
                property_id,
                Some(&value),
                None,
            );
            let transitions = crate::property::vertex_posting_transitions(&change)
                .into_iter()
                .map(|transition| FencedTransition {
                    property_id: transition.property_id,
                    prev: transition.prev,
                    new: transition.new,
                    membership: transition.membership,
                })
                .collect::<Vec<_>>();
            let planned = self.plan_index_build_admission(transitions)?;
            if !planned.is_empty() {
                groups.push((self.vertex_subject_for_id(vertex_id)?, planned));
            }
        }
        Ok(groups)
    }

    /// Enumerates the canonical incident-edge references that the purge will remove.
    ///
    /// Directed out-edges are already canonical (forward row at `vertex_id`); directed in-edges
    /// and undirected rows are resolved through CounterpartScan because per-store bucket slots
    /// are independent (a forward-only compaction moves one half but not the other). This mirrors
    /// [`GraphDeleteEdgeObserver`], which clears sidecars at the same canonical handles.
    ///
    /// [`GraphDeleteEdgeObserver`]: super::helpers::GraphDeleteEdgeObserver
    fn collect_incident_edge_refs(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<IncidentEdgeRef>, GraphStoreError> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        let mut push = |canonical: EdgeHandle, inline_bytes: Vec<u8>| {
            if seen.insert((
                u32::from(canonical.owner_vertex_id),
                canonical.label_id.raw(),
                canonical.slot_index.raw(),
            )) {
                out.push(IncidentEdgeRef {
                    canonical,
                    inline_bytes,
                });
            }
        };
        self.for_each_directed_out_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            push(
                EdgeHandle::at_slot(
                    vertex_id,
                    ic_stable_lara::BucketLabelKey::from_raw(edge.label_id),
                    edge.edge_slot_index.raw(),
                ),
                edge.edge_inline_property_bytes().to_vec(),
            );
        })?;
        self.for_each_directed_in_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            let row = EdgeHandle::at_slot(
                vertex_id,
                ic_stable_lara::BucketLabelKey::from_raw(edge.label_id),
                edge.edge_slot_index.raw(),
            );
            if let Ok(canonical) =
                self.scan_only_canonical_edge_handle(row, LabeledOrientation::Reverse)
            {
                push(canonical, edge.edge_inline_property_bytes().to_vec());
            }
        })?;
        self.for_each_undirected_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            let row = EdgeHandle::at_slot(
                vertex_id,
                ic_stable_lara::BucketLabelKey::from_raw(edge.label_id),
                edge.edge_slot_index.raw(),
            );
            if let Ok(canonical) =
                self.scan_only_canonical_edge_handle(row, LabeledOrientation::Forward)
            {
                push(canonical, edge.edge_inline_property_bytes().to_vec());
            }
        })?;
        Ok(out)
    }

    /// Plans the removal transitions for one incident edge (INLINE values plus sidecars).
    fn plan_incident_edge_removals(
        &self,
        edge_ref: IncidentEdgeRef,
    ) -> Result<Vec<(IndexBuildSubject, Vec<PlannedBuildEnvelope>)>, GraphStoreError> {
        let canonical = edge_ref.canonical;
        let Some((edge, _)) = self.lookup_edge_entry(canonical)? else {
            // The canonical row is gone (dangling half); the purge observer skips it too.
            return Ok(Vec::new());
        };
        let mut transitions = Vec::new();
        let inline_values = crate::property::inline_index_values(
            canonical.label_id.raw(),
            edge.edge_inline_property_bytes(),
        )
        .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })?;
        let mut sidecar_values: Vec<(
            crate::index::catalog_context::IndexMembershipRef,
            PropertyId,
            gleaph_gql::Value,
        )> = Vec::new();
        GraphStore::for_each_indexed_edge_property_value_on_edge(
            canonical.owner_vertex_id,
            canonical.label_id.raw(),
            canonical.slot_index.raw(),
            |membership, pid, value| sidecar_values.push((membership, pid, value)),
        );
        for (membership, property_id, value) in &inline_values {
            transitions.push(FencedTransition {
                property_id: *property_id,
                prev: Some(value),
                new: None,
                membership: *membership,
            });
        }
        for (membership, pid, value) in &sidecar_values {
            transitions.push(FencedTransition {
                property_id: *pid,
                prev: Some(value),
                new: None,
                membership: *membership,
            });
        }
        let planned = self.plan_index_build_admission(transitions)?;
        if planned.is_empty() {
            return Ok(Vec::new());
        }
        let subject = self.edge_subject_for_handle(canonical)?;
        Ok(vec![(subject, planned)])
    }

    /// Infallible fence commit for one vertex delete: reserves sequences and appends every
    /// planned envelope (per subject) before the canonical delete begins.
    fn commit_vertex_delete_planned(
        &self,
        mutation_id: MutationId,
        planned: Vec<(IndexBuildSubject, Vec<PlannedBuildEnvelope>)>,
    ) {
        for (subject, envelopes) in planned {
            self.commit_index_build_admission(mutation_id, subject, envelopes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::vertex_hidden_by_pending_purge;
    use gleaph_gql::Value;
    use ic_stable_lara::{MaintenanceBudget, labeled::LabeledOrientation, traits::CsrEdge};

    fn one_step_delete_budget() -> MaintenanceBudget {
        MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: Some(1),
        }
    }

    fn neighbors_pointing_to(store: &GraphStore, neighbors: &[VertexId], hub: VertexId) -> usize {
        neighbors
            .iter()
            .filter(|&&n| {
                store
                    .directed_out_edges(n)
                    .expect("out edges")
                    .iter()
                    .any(|e| e.neighbor_vid() == hub)
            })
            .count()
    }

    /// ADR 0021 Stage 2: a tombstone-first `DETACH DELETE` whose purge is only
    /// partially drained must keep its surviving back-edges *physically present*
    /// yet *gated out* of reads, then reconcile both once fully drained.
    #[test]
    fn partial_purge_gates_surviving_back_edges_then_full_drain_reconciles() {
        let store = GraphStore::new();
        let hub = store.insert_vertex().expect("hub");
        let neighbors: Vec<VertexId> = (0..6)
            .map(|_| {
                let n = store.insert_vertex().expect("n");
                store.insert_directed_edge(n, hub, None).expect("n->hub");
                n
            })
            .collect();
        assert_eq!(neighbors_pointing_to(&store, &neighbors, hub), 6);

        // Tombstone-first start without draining the purge to completion.
        store
            .commit_prepare_vertex_sidecars_for_delete(hub, 1)
            .expect("prepare hub sidecars");
        store
            .mark_vertex_pending_purge(hub)
            .expect("mark pending purge");
        store
            .with_graph_mut(|graph| graph.begin_vertex_delete_deferred(hub))
            .expect("begin resumable delete");

        // One delete step: at least one back-edge survives physically.
        store
            .run_maintenance_best_effort(one_step_delete_budget())
            .expect("partial purge step");
        assert!(
            store.vertex_is_pending_purge(hub),
            "hub stays pending while incident edges drain"
        );
        assert!(
            neighbors_pointing_to(&store, &neighbors, hub) > 0,
            "partial purge must leave surviving back-edges physically present"
        );
        // The read gate keys off the pending set, so the executor hides hub.
        assert!(vertex_hidden_by_pending_purge(hub));

        // Drain the rest: pending clears and every back-edge is purged.
        store
            .drain_deferred_maintenance_with_budget(
                crate::facade::bulk_ingest_finalize_maintenance_budget(),
            )
            .expect("full drain");
        assert!(!store.vertex_is_pending_purge(hub));
        assert!(!vertex_hidden_by_pending_purge(hub));
        assert_eq!(
            neighbors_pointing_to(&store, &neighbors, hub),
            0,
            "full purge removes every incident back-edge"
        );
    }

    /// Regression: deleting a vertex whose incident edges are payload-free directed
    /// in-edges from distinct sources must drain every back-edge. The reverse-branch
    /// purge previously matched the neighbor's forward edge by slot and spun forever
    /// (ADR 0021).
    #[test]
    fn detach_delete_hub_with_no_inline_property_in_edges_drains_every_back_edge() {
        let store = GraphStore::new();
        let hub = store.insert_vertex().expect("hub");
        let neighbors: Vec<VertexId> = (0..8)
            .map(|_| {
                let n = store.insert_vertex().expect("n");
                store.insert_directed_edge(n, hub, None).expect("n->hub");
                n
            })
            .collect();
        assert_eq!(neighbors_pointing_to(&store, &neighbors, hub), 8);

        store.detach_delete_vertex(hub).expect("detach delete hub");

        assert!(!store.is_vertex_live(hub), "hub is tombstoned after purge");
        assert!(!store.vertex_is_pending_purge(hub));
        assert_eq!(
            neighbors_pointing_to(&store, &neighbors, hub),
            0,
            "no neighbor keeps a dangling forward edge to the deleted hub"
        );
    }

    /// A directed in-edge whose forward slot diverged from its reverse slot (forward-only
    /// compaction) must be fenced under the CANONICAL forward handle, not the reverse-row slot:
    /// the purge removes the forward row by neighbor matching and clears sidecars at the forward
    /// handle, so a subject built from the reverse slot would target the wrong edge.
    #[test]
    fn detach_delete_fence_uses_canonical_forward_slot_for_diverged_directed_in_edge() {
        use gleaph_graph_kernel::canonical_export::{CanonicalExportScope, CanonicalExportTarget};
        use gleaph_graph_kernel::entry::IndexNameId;
        use gleaph_graph_kernel::index::{
            EdgeIndexDirection, IndexBuildSubject, IndexMaintenancePhase, IndexedEdgeMembership,
            PhysicalIndexId,
        };
        use ic_stable_lara::BucketLabelKey;

        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("routing");
        let n = store.insert_vertex().expect("source");
        let v = store.insert_vertex().expect("target");
        let w = store.insert_vertex().expect("other target");
        let label = crate::test_labels::edge_label_id_for_name("FenceCanonicalSlot");
        let wire_label = BucketLabelKey::directed_from_index(label.raw());
        let property = PropertyId::from_raw(9_000_030);
        let physical = PhysicalIndexId::new(900_040).unwrap();

        let scope = CanonicalExportScope {
            graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(1),
            index_name_id: IndexNameId::from_raw(9),
            catalog_epoch: 1,
            target: CanonicalExportTarget::Edge {
                label_id: label,
                property_id: property,
                direction: EdgeIndexDirection::Any,
            },
            inline: None,
        };
        crate::index::canonical_export::register_scope(physical, scope.clone()).expect("register");
        let _catalog = crate::index::catalog_context::enter(
            gleaph_graph_kernel::index::IndexedPropertyCatalog {
                edge_indexes: vec![IndexedEdgeMembership {
                    physical_index_id: physical,
                    catalog_epoch: 1,
                    phase: IndexMaintenancePhase::Building,
                    label_id: label.raw(),
                    property_id: property.raw(),
                    direction: EdgeIndexDirection::Any,
                    field_path: String::new(),
                }],
                ..Default::default()
            },
        );

        let e_a = store
            .insert_directed_edge(n, v, Some(label))
            .expect("first n->v");
        let e_b = store
            .insert_directed_edge(n, v, Some(label))
            .expect("second n->v");
        store.insert_directed_edge(n, w, Some(label)).expect("n->w");
        store
            .set_edge_property(
                e_b.occurrence(ic_stable_lara::labeled::LabeledOrientation::Forward),
                property,
                Value::Int64(7),
            )
            .expect("set sidecar on surviving edge");

        // Delete the first n->v edge, then compact only the source's forward span (as routine
        // maintenance does); the target's reverse bucket holds a different edge set and stays put.
        store.delete_edge_by_handle(e_a).expect("delete first");
        store.with_graph_mut(|graph| {
            graph
                .mark_compact_vertex_edge_span(
                    ic_stable_lara::labeled::LabeledOrientation::Forward,
                    n,
                    0,
                    &super::GraphStore::maintenance_policy_for_label,
                )
                .expect("mark forward compaction");
        });
        store
            .run_maintenance_best_effort(ic_stable_lara::MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("drain compaction");

        let forward_slots: Vec<u32> = store
            .directed_out_edges(n)
            .expect("out edges")
            .into_iter()
            .filter(|edge| edge.neighbor_vid() == v)
            .map(|edge| edge.edge_slot_index.raw())
            .collect();
        let reverse_slots: Vec<u32> = store
            .directed_in_edges(v)
            .expect("in edges")
            .into_iter()
            .map(|edge| edge.edge_slot_index.raw())
            .collect();
        assert_eq!(forward_slots.len(), 1);
        assert_ne!(
            forward_slots[0], reverse_slots[0],
            "precondition: forward and reverse slots must diverge"
        );

        store.detach_delete_vertex(v).expect("detach delete");

        // The removal envelope must be keyed to the canonical forward handle (owner n, forward
        // slot), matching the sidecar handle the purge clears.
        let envelopes = store.derived_index_outbox_peek(usize::MAX);
        let removal = envelopes
            .iter()
            .filter_map(|(_, entry)| {
                match &entry.op {
                crate::facade::stable::derived_index_outbox::DerivedIndexOutboxOp::IndexBuildDml {
                    request,
                } if request.physical_index_id == physical
                    && matches!(request.subject, IndexBuildSubject::Edge { .. }) =>
                {
                    Some(request)
                }
                _ => None,
            }
            })
            .next_back()
            .expect("removal envelope");
        match removal.subject {
            IndexBuildSubject::Edge {
                owner_vertex_id,
                label_id,
                slot_index,
                ..
            } => {
                assert_eq!(owner_vertex_id, u32::from(n));
                assert_eq!(label_id, wire_label.raw());
                assert_eq!(slot_index, forward_slots[0]);
            }
            other => panic!("unexpected subject {other:?}"),
        }

        // Drain the admitted watermark (sidecar insert + removal) so the scope can be removed.
        let status = crate::index::canonical_export::scope_status(physical).expect("status");
        for sequence in 1..=status.admitted_through {
            crate::index::canonical_export::ack_build_dml(physical, 1, sequence)
                .expect("ack sequence");
        }
        crate::index::canonical_export::abort_scope(physical, scope.clone()).expect("abort");
        crate::index::canonical_export::remove_scope(physical, &scope).expect("cleanup");
    }

    /// A detach-delete that touches a Sealing edge membership is rejected BEFORE any canonical
    /// mutation: the vertex stays live, the Memory46 outbox is unchanged, and the scope
    /// watermarks are frozen. The rejection happens in the pure plan, before the purge's span
    /// pre-compaction runs.
    #[test]
    fn detach_delete_rejects_sealing_incident_edge_before_any_canonical_mutation() {
        use gleaph_graph_kernel::canonical_export::{
            CanonicalExportError, CanonicalExportPhase, CanonicalExportScope, CanonicalExportTarget,
        };
        use gleaph_graph_kernel::entry::IndexNameId;
        use gleaph_graph_kernel::index::{
            EdgeIndexDirection, IndexMaintenancePhase, IndexedEdgeMembership, PhysicalIndexId,
        };

        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("routing");
        let v = store.insert_vertex().expect("v");
        let a = store.insert_vertex().expect("a");
        let label = crate::test_labels::edge_label_id_for_name("FenceSealingDetach");
        let property = PropertyId::from_raw(9_000_051);
        let physical = PhysicalIndexId::new(900_051).unwrap();
        let scope = CanonicalExportScope {
            graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(1),
            index_name_id: IndexNameId::from_raw(9),
            catalog_epoch: 1,
            target: CanonicalExportTarget::Edge {
                label_id: label,
                property_id: property,
                direction: EdgeIndexDirection::Any,
            },
            inline: None,
        };
        crate::index::canonical_export::register_scope(physical, scope.clone()).expect("register");
        let membership = |phase| IndexedEdgeMembership {
            physical_index_id: physical,
            catalog_epoch: 1,
            phase,
            label_id: label.raw(),
            property_id: property.raw(),
            direction: EdgeIndexDirection::Any,
            field_path: String::new(),
        };
        let _catalog = crate::index::catalog_context::enter(
            gleaph_graph_kernel::index::IndexedPropertyCatalog {
                edge_indexes: vec![membership(IndexMaintenancePhase::Building)],
                ..Default::default()
            },
        );
        let handle = store.insert_directed_edge(v, a, Some(label)).expect("v->a");
        store
            .set_edge_property(
                handle.occurrence(ic_stable_lara::labeled::LabeledOrientation::Forward),
                property,
                Value::Int64(7),
            )
            .expect("set sidecar");
        drop(_catalog);
        crate::index::canonical_export::seal_scope(physical, scope.clone(), 2).expect("seal");
        let _sealing = crate::index::catalog_context::enter(
            gleaph_graph_kernel::index::IndexedPropertyCatalog {
                edge_indexes: vec![membership(IndexMaintenancePhase::Sealing)],
                ..Default::default()
            },
        );

        let error = store
            .detach_delete_vertex(v)
            .expect_err("sealing rejects the detach-delete");
        assert!(matches!(
            error,
            GraphStoreError::IndexBuildAdmission(CanonicalExportError::RetryableSealing)
        ));
        assert!(
            store.is_vertex_live(v),
            "the vertex must not be tombstoned by the rejected delete"
        );
        let entries = store.derived_index_outbox_peek(usize::MAX);
        assert_eq!(
            entries.len(),
            1,
            "only the earlier Building insert envelope may remain; the rejected delete appends nothing"
        );
        let status = crate::index::canonical_export::scope_status(physical).expect("status");
        assert_eq!(status.phase, CanonicalExportPhase::Sealing);
        assert_eq!(status.epoch, 2);
        assert_eq!(status.admitted_through, 1);
        assert_eq!(status.drained_through, 0);
        crate::index::canonical_export::ack_build_dml(physical, 1, 1).expect("ack");
        drop(_sealing);
        crate::index::canonical_export::abort_scope(physical, scope.clone()).expect("abort");
        crate::index::canonical_export::remove_scope(physical, &scope).expect("cleanup");
    }

    /// Regression: a detach-delete whose purge step-0 compaction moves a Building-membership
    /// edge must not leave stale postings. The compaction runs BEFORE the fence commits the
    /// removal envelopes, so every insertion envelope is followed by an exact removal at the
    /// same subject; a post-fence move would insert postings at the new slots of edges that are
    /// about to be deleted with no matching removal.
    #[test]
    fn detach_delete_purge_compaction_leaves_no_stale_build_dml_insertions() {
        use gleaph_graph_kernel::canonical_export::{CanonicalExportScope, CanonicalExportTarget};
        use gleaph_graph_kernel::entry::IndexNameId;
        use gleaph_graph_kernel::index::{
            EdgeIndexDirection, IndexBuildSubject, IndexMaintenancePhase, IndexedEdgeMembership,
            PhysicalIndexId,
        };

        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("routing");
        let v = store.insert_vertex().expect("v");
        let a = store.insert_vertex().expect("a");
        let b = store.insert_vertex().expect("b");
        let c = store.insert_vertex().expect("c");
        let label = crate::test_labels::edge_label_id_for_name("FencePurgeCompaction");
        let property = PropertyId::from_raw(9_000_050);
        let physical = PhysicalIndexId::new(900_050).unwrap();
        let scope = CanonicalExportScope {
            graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(1),
            index_name_id: IndexNameId::from_raw(9),
            catalog_epoch: 1,
            target: CanonicalExportTarget::Edge {
                label_id: label,
                property_id: property,
                direction: EdgeIndexDirection::Any,
            },
            inline: None,
        };
        crate::index::canonical_export::register_scope(physical, scope.clone()).expect("register");
        let _catalog = crate::index::catalog_context::enter(
            gleaph_graph_kernel::index::IndexedPropertyCatalog {
                edge_indexes: vec![IndexedEdgeMembership {
                    physical_index_id: physical,
                    catalog_epoch: 1,
                    phase: IndexMaintenancePhase::Building,
                    label_id: label.raw(),
                    property_id: property.raw(),
                    direction: EdgeIndexDirection::Any,
                    field_path: String::new(),
                }],
                ..Default::default()
            },
        );

        store.insert_directed_edge(v, a, Some(label)).expect("v->a");
        store.insert_directed_edge(v, b, Some(label)).expect("v->b");
        let e3 = store.insert_directed_edge(v, c, Some(label)).expect("v->c");
        store
            .set_edge_property(
                e3.occurrence(ic_stable_lara::labeled::LabeledOrientation::Forward),
                property,
                Value::Int64(7),
            )
            .expect("set sidecar on e3");
        // Fold the deferred-insert overflow log into the slab so the interior delete tombstones
        // in place (slab deletes do not pack), leaving a slab tombstone for the purge compaction.
        store.with_graph_mut(|graph| {
            graph
                .mark_compact_vertex_edge_span(
                    ic_stable_lara::labeled::LabeledOrientation::Forward,
                    v,
                    0,
                    &super::GraphStore::maintenance_policy_for_label,
                )
                .expect("mark pre-fold");
        });
        store
            .run_maintenance_best_effort(ic_stable_lara::MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("pre-fold drain");
        // Interior slab tombstone left by deleting the middle edge: the purge step-0 left-pack
        // compaction must move e3 from slot 2 into slot 1, and the move-sidecar observer fences
        // a move envelope for it. The fence now runs after the pre-compaction, so the removal
        // envelope is keyed to the post-compaction slot.
        let e2 = store
            .directed_out_edges(v)
            .expect("out edges")
            .into_iter()
            .find(|edge| edge.neighbor_vid() == b)
            .expect("v->b row");
        store
            .delete_edge_by_handle(EdgeHandle::at_slot(
                v,
                ic_stable_lara::BucketLabelKey::from_raw(e2.label_id),
                e2.edge_slot_index.raw(),
            ))
            .expect("delete e2");

        store.detach_delete_vertex(v).expect("detach delete");

        // Simulate the drain: apply every envelope in outbox order, tracking per-subject
        // postings. Every insertion must be removed by a later envelope at the same subject;
        // the deleted edge must end with no postings under any slot.
        let envelopes = store.derived_index_outbox_peek(usize::MAX);
        let mut live: std::collections::BTreeMap<(u32, u16, u32), usize> =
            std::collections::BTreeMap::new();
        let mut saw_build_dml = false;
        for (_, entry) in &envelopes {
            let crate::facade::stable::derived_index_outbox::DerivedIndexOutboxOp::IndexBuildDml {
                request,
            } = &entry.op
            else {
                continue;
            };
            if request.physical_index_id != physical {
                continue;
            }
            saw_build_dml = true;
            let IndexBuildSubject::Edge {
                owner_vertex_id,
                label_id,
                slot_index,
                ..
            } = request.subject
            else {
                continue;
            };
            let key = (owner_vertex_id, label_id, slot_index);
            for _ in &request.insertions {
                *live.entry(key).or_default() += 1;
            }
            for _ in &request.removals {
                let count = live.entry(key).or_default();
                assert!(
                    *count > 0,
                    "removal without a matching insertion at subject {key:?}"
                );
                *count -= 1;
                if *count == 0 {
                    live.remove(&key);
                }
            }
        }
        assert!(saw_build_dml, "the purge must emit build-DML envelopes");
        assert!(
            live.is_empty(),
            "stale postings remain after the drain: {live:?}"
        );

        // The compaction must have moved e3 (tombstone consumed by the move): without it the
        // test would not exercise the defect.
        let status = crate::index::canonical_export::scope_status(physical).expect("status");
        assert!(
            status.admitted_through >= 2,
            "expected insert + move envelopes before the removal"
        );
        for sequence in 1..=status.admitted_through {
            crate::index::canonical_export::ack_build_dml(physical, 1, sequence)
                .expect("ack sequence");
        }
        crate::index::canonical_export::abort_scope(physical, scope.clone()).expect("abort");
        crate::index::canonical_export::remove_scope(physical, &scope).expect("cleanup");
    }

    #[test]
    fn detach_delete_undirected_self_loop_clears_canonical_sidecars_once() {
        let store = GraphStore::new();
        let vertex = store.insert_vertex().expect("vertex");
        let handle = store
            .insert_undirected_edge(vertex, vertex, None)
            .expect("self-loop");
        let property = store
            .get_or_insert_property_id("delete_self_loop")
            .expect("property");
        store
            .set_edge_property(
                handle.occurrence(LabeledOrientation::Forward),
                property,
                Value::Int64(7),
            )
            .expect("set property");

        store.detach_delete_vertex(vertex).expect("detach delete");

        let remaining = super::super::super::stable::EDGE_PROPERTIES.with_borrow(|properties| {
            properties.get(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
                property,
            )
        });
        assert!(
            remaining.is_none(),
            "self-loop sidecar must be cleared once"
        );
    }

    #[test]
    fn delete_internal_label_clear_does_not_emit_second_property_removal() {
        use crate::index::{catalog_context, label_pending, pending};
        use gleaph_graph_kernel::canonical_export::{CanonicalExportScope, CanonicalExportTarget};
        use gleaph_graph_kernel::entry::{GraphId, IndexNameId, VertexLabelId};
        use gleaph_graph_kernel::index::{
            IndexBuildSubject, IndexMaintenancePhase, IndexedPropertyCatalog,
            IndexedVertexMembership, PhysicalIndexId,
        };

        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
            }))
            .expect("routing");
        pending::clear_pending();
        label_pending::clear_pending();
        store.derived_index_outbox_clear();

        let target_label = VertexLabelId::from_raw(11);
        let decoy_label = VertexLabelId::from_raw(12);
        let property = PropertyId::from_raw(9_100_001);
        let target_physical = PhysicalIndexId::new(9_100_011).unwrap();
        let active_physical = PhysicalIndexId::new(9_100_013).unwrap();
        let decoy_physical = PhysicalIndexId::new(9_100_012).unwrap();
        let target_scope = CanonicalExportScope {
            graph_id: GraphId::from_raw(991),
            index_name_id: IndexNameId::from_raw(991),
            catalog_epoch: 1,
            target: CanonicalExportTarget::Vertex {
                label_id: target_label.raw(),
                property_id: property,
                record_source: None,
            },
            inline: None,
        };
        let decoy_scope = CanonicalExportScope {
            graph_id: GraphId::from_raw(991),
            index_name_id: IndexNameId::from_raw(992),
            catalog_epoch: 1,
            target: CanonicalExportTarget::Vertex {
                label_id: decoy_label.raw(),
                property_id: property,
                record_source: None,
            },
            inline: None,
        };
        crate::index::canonical_export::register_scope(target_physical, target_scope.clone())
            .expect("target scope");
        crate::index::canonical_export::register_scope(decoy_physical, decoy_scope.clone())
            .expect("decoy scope");
        crate::index::canonical_export::seal_scope(decoy_physical, decoy_scope.clone(), 2)
            .expect("decoy seal");

        let fixture = |label: VertexLabelId| {
            let vertex_id = store.insert_vertex().expect("vertex");
            let vertex = store.vertex(vertex_id).expect("vertex row");
            store
                .set_vertex_labels(vertex_id, vertex, [label])
                .expect("label");
            store
                .set_vertex_property(vertex_id, property, Value::Int64(42))
                .expect("property");
            vertex_id
        };
        let loss_vertex = fixture(target_label);
        let clear_probe = fixture(target_label);
        let delete_vertex = fixture(target_label);
        pending::clear_pending();
        label_pending::clear_pending();
        store.derived_index_outbox_clear();

        let _catalog = catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                IndexedVertexMembership {
                    field_path: String::new(),
                    ancestor_property_id: 0,
                    physical_index_id: target_physical,
                    catalog_epoch: 1,
                    phase: IndexMaintenancePhase::Building,
                    property_id: property.raw(),
                    label_id: target_label.raw(),
                },
                IndexedVertexMembership {
                    field_path: String::new(),
                    ancestor_property_id: 0,
                    physical_index_id: active_physical,
                    catalog_epoch: 1,
                    phase: IndexMaintenancePhase::Active,
                    property_id: property.raw(),
                    label_id: target_label.raw(),
                },
                IndexedVertexMembership {
                    field_path: String::new(),
                    ancestor_property_id: 0,
                    physical_index_id: decoy_physical,
                    catalog_epoch: 1,
                    phase: IndexMaintenancePhase::Sealing,
                    property_id: property.raw(),
                    label_id: decoy_label.raw(),
                },
            ],
            ..Default::default()
        });

        store
            .remove_vertex_label_with_mutation_id(
                loss_vertex,
                store.vertex(loss_vertex).expect("loss row"),
                target_label,
                91_101,
            )
            .expect("label loss");
        let loss_entries = store.derived_index_outbox_peek(usize::MAX);
        assert_eq!(loss_entries.len(), 1);
        let (loss_sequence, loss_entry) = &loss_entries[0];
        assert_eq!(*loss_sequence, 0);
        assert_eq!(loss_entry.mutation_id, 91_101);
        let crate::facade::stable::derived_index_outbox::DerivedIndexOutboxOp::IndexBuildDml {
            request: loss_request,
        } = &loss_entry.op
        else {
            panic!("label loss must emit a build DML envelope");
        };
        assert_eq!(loss_request.physical_index_id, target_physical);
        assert_eq!(loss_request.catalog_epoch, 1);
        assert_eq!(loss_request.shard_sequence, 1);
        assert_eq!(
            loss_request.subject,
            IndexBuildSubject::Vertex {
                shard_id: 0,
                vertex_id: u32::from(loss_vertex),
            }
        );
        let expected_payload = gleaph_gql::value_to_index_key_bytes(&Value::Int64(42))
            .expect("sortable value")
            .expect("encoded value");
        assert_eq!(loss_request.removals, vec![expected_payload.clone()]);
        assert!(loss_request.insertions.is_empty());
        let loss_pending = pending::take_pending();
        assert_eq!(loss_pending.len(), 1);
        assert!(matches!(
            loss_pending.as_slice(),
            [pending::PendingPostingOp::Remove {
                physical_index_id,
                catalog_epoch,
                phase,
                property_id,
                payload_bytes,
                vertex_id,
            }] if *physical_index_id == active_physical
                && *catalog_epoch == 1
                && phase.is_active()
                && *property_id == property.raw()
                && *payload_bytes == expected_payload
                && *vertex_id == u32::from(loss_vertex)
        ));
        let loss_labels = label_pending::take_pending();
        assert!(matches!(
            loss_labels.as_slice(),
            [label_pending::PendingLabelOp::Remove { label_id, vertex_id }]
                if *label_id == u32::from(target_label.raw())
                    && *vertex_id == u32::from(loss_vertex)
        ));
        crate::index::canonical_export::ack_build_dml(target_physical, 1, 1)
            .expect("ack label loss");
        store.derived_index_outbox_clear();

        let before_clear = store.derived_index_outbox_peek(usize::MAX);
        assert!(before_clear.is_empty());
        store
            .commit_clear_vertex_labels(clear_probe, store.vertex(clear_probe).expect("clear row"))
            .expect("delete-only label clear");
        assert!(
            store.derived_index_outbox_peek(usize::MAX).is_empty(),
            "internal clear must not emit a property removal"
        );
        assert!(pending::take_pending().is_empty());
        let clear_labels = label_pending::take_pending();
        assert!(matches!(
            clear_labels.as_slice(),
            [label_pending::PendingLabelOp::Remove { label_id, vertex_id }]
                if *label_id == u32::from(target_label.raw())
                    && *vertex_id == u32::from(clear_probe)
        ));

        store
            .delete_vertex_with_mutation_id(delete_vertex, 91_102)
            .expect("delete vertex");
        let delete_entries = store.derived_index_outbox_peek(usize::MAX);
        assert_eq!(delete_entries.len(), 1);
        let (delete_sequence, delete_entry) = &delete_entries[0];
        assert_eq!(*delete_sequence, 0);
        assert_eq!(delete_entry.mutation_id, 91_102);
        let crate::facade::stable::derived_index_outbox::DerivedIndexOutboxOp::IndexBuildDml {
            request: delete_request,
        } = &delete_entry.op
        else {
            panic!("delete must emit a build DML envelope");
        };
        assert_eq!(delete_request.physical_index_id, target_physical);
        assert_eq!(delete_request.catalog_epoch, 1);
        assert_eq!(delete_request.shard_sequence, 2);
        assert_eq!(
            delete_request.subject,
            IndexBuildSubject::Vertex {
                shard_id: 0,
                vertex_id: u32::from(delete_vertex),
            }
        );
        assert_eq!(delete_request.removals, vec![expected_payload.clone()]);
        assert!(delete_request.insertions.is_empty());
        let delete_pending = pending::take_pending();
        assert_eq!(delete_pending.len(), 1);
        assert!(matches!(
            delete_pending.as_slice(),
            [pending::PendingPostingOp::Remove {
                physical_index_id,
                catalog_epoch,
                phase,
                property_id,
                payload_bytes,
                vertex_id,
            }] if *physical_index_id == active_physical
                && *catalog_epoch == 1
                && phase.is_active()
                && *property_id == property.raw()
                && *payload_bytes == expected_payload
                && *vertex_id == u32::from(delete_vertex)
        ));
        let delete_labels = label_pending::take_pending();
        assert!(matches!(
            delete_labels.as_slice(),
            [label_pending::PendingLabelOp::Remove { label_id, vertex_id }]
                if *label_id == u32::from(target_label.raw())
                    && *vertex_id == u32::from(delete_vertex)
        ));
        crate::index::canonical_export::ack_build_dml(target_physical, 1, 2).expect("ack delete");
        crate::index::canonical_export::abort_scope(target_physical, target_scope.clone())
            .expect("abort target");
        crate::index::canonical_export::remove_scope(target_physical, &target_scope)
            .expect("remove target");
        crate::index::canonical_export::abort_scope(decoy_physical, decoy_scope.clone())
            .expect("abort decoy");
        crate::index::canonical_export::remove_scope(decoy_physical, &decoy_scope)
            .expect("remove decoy");
        store.derived_index_outbox_clear();
        store.set_federation_routing(None).expect("clear routing");
    }
}
