//! GraphStore `sidecar` implementation.

use super::super::stable::{
    EDGE_ALIASES, EDGE_PROPERTIES, GRAPH, REMOTE_FORWARD_IN, VERTEX_LABELS, VERTEX_LOGICAL_IDS,
    VERTEX_PROPERTIES,
};
use crate::index::{edge_equal, label_pending, placement};
use gleaph_graph_kernel::entry::{Edge, EdgeTarget, PropertyId};
use gleaph_graph_kernel::federation::{ReleaseLogicalVertexArgs, VertexPlacement};
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledError, VertexId,
    labeled::{EdgeSlotMove, LabeledOrientation},
    traits::CsrEdge,
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{canonical_undirected_owner, edge_alias_slot_key};

impl GraphStore {
    pub(super) fn vertex_has_incident_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.has_incident_edges(vertex_id))
    }

    pub(super) fn edge_sidecar_owner_from_out_row(
        &self,
        endpoint: VertexId,
        edge: &Edge,
    ) -> VertexId {
        if self.edge_is_undirected(endpoint, edge).unwrap_or(false) {
            canonical_undirected_owner(endpoint, edge.neighbor_vid())
        } else {
            endpoint
        }
    }

    pub(super) fn clear_edge_sidecars(&self, handle: EdgeHandle) {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        edge_equal::remove_all_for_edge(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index,
        );
        EDGE_PROPERTIES.with_borrow_mut(|store| {
            store.remove_all_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
        EDGE_ALIASES.with_borrow_mut(|aliases| {
            aliases.remove(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
            aliases.remove_all_for_canonical(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
    }

    pub(super) fn move_edge_sidecars_for_compaction(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
    ) {
        let label_id = moved.label_id.raw();
        match orientation {
            LabeledOrientation::Forward => {
                let moved_properties = EDGE_PROPERTIES.with_borrow_mut(|store| {
                    store
                        .move_all_for_edge(
                            owner_vertex_id,
                            label_id,
                            moved.old_slot_index,
                            moved.new_slot_index,
                        )
                        .expect("stored edge property values remain encodable")
                });
                if !moved_properties.is_empty() {
                    for (property_id, value) in &moved_properties {
                        edge_equal::record_edge_property_change(
                            owner_vertex_id,
                            label_id,
                            moved.old_slot_index,
                            *property_id,
                            Some(value),
                            None,
                        );
                        edge_equal::record_edge_property_change(
                            owner_vertex_id,
                            label_id,
                            moved.new_slot_index,
                            *property_id,
                            None,
                            Some(value),
                        );
                    }
                }
                EDGE_ALIASES.with_borrow_mut(|aliases| {
                    aliases.move_canonical_target(
                        owner_vertex_id,
                        label_id,
                        moved.old_slot_index,
                        moved.new_slot_index,
                    );
                    aliases.move_alias_key(
                        owner_vertex_id,
                        label_id,
                        moved.old_slot_index,
                        moved.new_slot_index,
                    );
                });
                let label = LaraLabelId::from_raw(label_id);
                let _ = GRAPH.with_borrow(|graph| {
                    graph.for_each_out_edges_for_label_unchecked(owner_vertex_id, label, |edge| {
                        if edge.edge_slot_index.raw() != moved.new_slot_index {
                            return;
                        }
                        let Some(EdgeTarget::Remote(remote_ref)) = edge.edge_target() else {
                            return;
                        };
                        REMOTE_FORWARD_IN.with_borrow_mut(|index| {
                            index.move_slot(
                                remote_ref,
                                owner_vertex_id,
                                label_id,
                                moved.old_slot_index,
                                moved.new_slot_index,
                            );
                        });
                    })
                });
            }
            LabeledOrientation::Reverse => {
                EDGE_ALIASES.with_borrow_mut(|aliases| {
                    aliases.move_alias_key(
                        owner_vertex_id,
                        label_id,
                        edge_alias_slot_key(moved.old_slot_index, true),
                        edge_alias_slot_key(moved.new_slot_index, true),
                    );
                });
            }
        }
    }

    fn clear_vertex_properties_stable_only(&self, vertex_id: VertexId) {
        let props: Vec<PropertyId> = VERTEX_PROPERTIES.with_borrow(|store| {
            store
                .properties_for(vertex_id)
                .into_iter()
                .map(|(pid, _)| pid)
                .collect()
        });
        for pid in props {
            let _ = self.remove_vertex_property(vertex_id, pid);
        }
    }

    pub(super) fn clear_vertex_stable_payloads_before_graph_delete(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        self.clear_vertex_properties_stable_only(vertex_id);
        self.release_federated_vertex_placement_if_authoritative(vertex_id)?;

        let vertex = self.vertex(vertex_id).ok_or_else(|| {
            GraphStoreError::Graph(DeferredBidirectionalLabeledError::VertexOutOfRange {
                vid: vertex_id,
                len: self.vertex_count(),
            })
        })?;
        let prev_labels = self.vertex_labels(vertex_id, vertex);
        label_pending::record_vertex_label_set(vertex_id, &prev_labels, &[]);
        // Label sidecars live in `VERTEX_LABELS`; the CSR row is unchanged. Do not call
        // `set_vertex` here: it mirrors the forward row into reverse and would corrupt
        // reverse-only locator state for this `VertexId`.
        let _ = VERTEX_LABELS.with_borrow_mut(|labels| {
            labels
                .set_labels(vertex_id, vertex, [])
                .map_err(GraphStoreError::from)
        })?;
        Ok(())
    }

    fn release_federated_vertex_placement_if_authoritative(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        let Some(routing) = self.federation_routing() else {
            return Ok(());
        };
        let Some(logical_vertex_id) = self.logical_vertex_id(vertex_id) else {
            return Ok(());
        };
        #[cfg(not(target_family = "wasm"))]
        {
            let placement = pollster::block_on(placement::resolve_placement(
                routing.router_canister,
                logical_vertex_id,
            ))?;
            let VertexPlacement::Active(loc) = placement;
            if loc.shard_id != routing.shard_id
                || loc.local_vertex_id != placement::local_vertex_id_raw(vertex_id)
            {
                return Ok(());
            }
            pollster::block_on(placement::release_logical_vertex_placement(
                routing.router_canister,
                ReleaseLogicalVertexArgs { logical_vertex_id },
            ))?;
        }
        VERTEX_LOGICAL_IDS.with_borrow_mut(|map| map.remove(vertex_id));
        Ok(())
    }
}
