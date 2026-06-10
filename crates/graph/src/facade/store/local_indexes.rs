//! Local index domain: edge aliases and edge-property equality postings.

use super::super::stable::EDGE_ALIASES;
use crate::index::edge_equal;
use crate::property::PropertyValueChange;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::{
    VertexId,
    labeled::{EdgeSlotMove, LabeledOrientation},
};

use super::GraphStore;
use super::handle::EdgeHandle;
use super::helpers::edge_alias_slot_key;

impl GraphStore {
    pub(super) fn commit_insert_edge_alias(
        &self,
        alias: EdgeHandle,
        canonical: EdgeHandle,
        reverse_in: bool,
    ) {
        if alias.owner_vertex_id == canonical.owner_vertex_id
            && alias.label_id == canonical.label_id
            && alias.slot_index == canonical.slot_index
        {
            return;
        }
        debug_assert_eq!(alias.label_id, canonical.label_id);
        let alias_slot_key = edge_alias_slot_key(alias.slot_index, reverse_in);
        EDGE_ALIASES.with_borrow_mut(|aliases| {
            aliases.insert(
                alias.owner_vertex_id,
                alias.label_id.raw(),
                alias_slot_key,
                canonical.owner_vertex_id,
                canonical.slot_index,
            );
        });
    }

    pub(super) fn commit_remove_all_edge_equality_postings(
        &self,
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
    ) {
        edge_equal::remove_all_for_edge(owner_vertex_id, label_id, slot_index);
    }

    pub(super) fn commit_remove_edge_alias_entries(
        &self,
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
    ) {
        EDGE_ALIASES.with_borrow_mut(|aliases| {
            aliases.remove(owner_vertex_id, label_id, slot_index);
            aliases.remove_all_for_canonical(owner_vertex_id, label_id, slot_index);
        });
    }

    pub(super) fn commit_clear_edge_local_indexes(&self, handle: EdgeHandle) {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        self.commit_remove_all_edge_equality_postings(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index,
        );
        self.commit_remove_edge_alias_entries(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index,
        );
    }

    pub(super) fn commit_record_edge_property_equality_change(
        &self,
        change: PropertyValueChange<'_>,
    ) {
        edge_equal::record_edge_property_change(change);
    }

    pub(super) fn commit_move_edge_local_indexes_for_compaction(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
        moved_properties: &[(PropertyId, Value)],
    ) {
        let label_id = moved.label_id.raw();
        match orientation {
            LabeledOrientation::Forward => {
                for (property_id, value) in moved_properties {
                    edge_equal::record_edge_property_change(PropertyValueChange::edge(
                        owner_vertex_id,
                        label_id,
                        moved.old_slot_index,
                        *property_id,
                        Some(value),
                        None,
                    ));
                    edge_equal::record_edge_property_change(PropertyValueChange::edge(
                        owner_vertex_id,
                        label_id,
                        moved.new_slot_index,
                        *property_id,
                        None,
                        Some(value),
                    ));
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

    /// Compatibility wrapper for existing call sites.
    pub(super) fn insert_edge_alias(
        &self,
        alias: EdgeHandle,
        canonical: EdgeHandle,
        reverse_in: bool,
    ) {
        self.commit_insert_edge_alias(alias, canonical, reverse_in);
    }
}
