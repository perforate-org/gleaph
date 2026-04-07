use std::collections::BTreeMap;

use candid::Principal;
use gleaph_gql::Value;
use gleaph_graph_kernel::{
    EdgeId, EdgeRecord, GraphError, GraphResult, LabelId, NodeId, NodeRecord, PropertyMap,
};

use super::{
    GraphPmaKernelBootstrapBridge, VacuumStats, VertexGcState, VertexLabelIndex,
    decode_vertex_label_catalog, encode_vertex_label_catalog, graph_error_from_property_store,
    label_index,
};
use crate::facade::{
    GraphPmaPropertyMutationWriteSummary, GraphPmaStore, GraphPmaVertexOrdinalMapping,
};
use crate::low_level::{
    EdgeEntry, EdgeInsertPath, LogicalEdgeLocator, RegionKind, ResolvedEdgeSlot, SurfaceKind,
    VertexRef,
};
use crate::property_index::{
    scan_edge_property_index_value_prefix_from_stable_memory,
    scan_node_property_index_property_prefix_from_stable_memory,
    scan_node_property_index_value_prefix_from_stable_memory,
};
use crate::property_store::PropertyStoreError;
use ic_stable_structures::Memory;

impl<'a, S: GraphPmaStore> GraphPmaKernelBootstrapBridge<'a, S> {
    fn property_store_error(err: PropertyStoreError) -> GraphError {
        graph_error_from_property_store(err)
    }

    pub(crate) fn persist_node_properties(
        &mut self,
        node_id: NodeId,
        properties: &PropertyMap,
    ) -> GraphResult<()> {
        for (name, value) in properties {
            self.store
                .set_node_property_value(node_id, name, value)
                .map_err(Self::property_store_error)?;
        }
        Ok(())
    }

    pub(crate) fn persist_edge_properties(
        &mut self,
        edge_id: EdgeId,
        properties: &PropertyMap,
    ) -> GraphResult<()> {
        for (name, value) in properties {
            self.store
                .set_edge_property_value(edge_id, name, value)
                .map_err(Self::property_store_error)?;
        }
        Ok(())
    }

    pub(crate) fn load_node_properties(&self, node_id: NodeId) -> PropertyMap {
        self.store.scan_node_properties(node_id)
    }

    pub(crate) fn load_edge_properties(&self, edge_id: EdgeId) -> PropertyMap {
        self.store.scan_edge_properties(edge_id)
    }

    pub(crate) fn remove_persisted_node_properties(&mut self, node_id: NodeId) -> GraphResult<()> {
        for property in self.load_node_properties(node_id).into_keys() {
            let mutation = self
                .store
                .remove_node_property_value_with_summary(node_id, &property)
                .map_err(Self::property_store_error)?;
            self.record_property_write_summary(
                GraphPmaPropertyMutationWriteSummary::pending_from_mutation(mutation),
            );
        }
        Ok(())
    }

    pub(crate) fn remove_persisted_edge_properties(&mut self, edge_id: EdgeId) -> GraphResult<()> {
        for property in self.load_edge_properties(edge_id).into_keys() {
            let mutation = self
                .store
                .remove_edge_property_value_with_summary(edge_id, &property)
                .map_err(Self::property_store_error)?;
            self.record_property_write_summary(
                GraphPmaPropertyMutationWriteSummary::pending_from_mutation(mutation),
            );
        }
        Ok(())
    }

    pub(crate) fn node_property_candidate_ids_eq(
        &self,
        property: &str,
        value: &Value,
    ) -> Vec<NodeId> {
        if !self.store.node_property_store_is_dirty() {
            let encoded_value = value
                .to_binary_bytes()
                .expect("Value must encode to binary bytes");
            if let Ok(matches) = scan_node_property_index_value_prefix_from_stable_memory(
                &self.store.manager(),
                self.memory,
                property,
                &encoded_value,
            ) {
                return matches
                    .into_iter()
                    .filter_map(|(key, _)| NodeId::try_from(key.entity_id).ok())
                    .collect();
            }
        }
        self.store.scan_node_ids_by_property_eq(property, value)
    }

    pub(crate) fn node_property_candidate_ids(&self, property: &str) -> Vec<NodeId> {
        if !self.store.node_property_store_is_dirty()
            && let Ok(matches) = scan_node_property_index_property_prefix_from_stable_memory(
                &self.store.manager(),
                self.memory,
                property,
            )
        {
            return matches
                .into_iter()
                .filter_map(|(key, _)| NodeId::try_from(key.entity_id).ok())
                .collect();
        }
        self.store.scan_node_ids_by_property(property)
    }

    pub(crate) fn edge_property_candidate_ids_eq(
        &self,
        property: &str,
        value: &Value,
    ) -> Vec<EdgeId> {
        if !self.store.edge_property_store_is_dirty() {
            let encoded_value = value
                .to_binary_bytes()
                .expect("Value must encode to binary bytes");
            if let Ok(matches) = scan_edge_property_index_value_prefix_from_stable_memory(
                &self.store.manager(),
                self.memory,
                property,
                &encoded_value,
            ) {
                return matches.into_iter().map(|(key, _)| key.entity_id).collect();
            }
        }
        self.store.scan_edge_ids_by_property_eq(property, value)
    }

    pub(crate) fn refreshed_node_record(&self, node_id: NodeId) -> GraphResult<NodeRecord> {
        self.nodes
            .get(&node_id)
            .cloned()
            .ok_or(GraphError::NodeNotFound(node_id))
    }

    pub(crate) fn refreshed_edge_record(&self, edge_id: EdgeId) -> GraphResult<EdgeRecord> {
        self.edges
            .get(&edge_id)
            .cloned()
            .ok_or(GraphError::EdgeNotFound(edge_id))
    }

    pub(crate) fn vertex_mapping(&self, node_id: NodeId) -> Option<GraphPmaVertexOrdinalMapping> {
        self.vertex_ordinal_by_node_id.get(&node_id).copied()
    }

    pub(crate) fn forward_node_ids(&self) -> Vec<NodeId> {
        self.vertex_ordinals
            .iter()
            .map(|mapping| mapping.vertex_ref.into())
            .collect()
    }

    pub(crate) fn forward_live_base_edge_ids_by_ordinal(&self) -> Vec<Vec<EdgeId>> {
        self.forward_base_slots_by_ordinal
            .iter()
            .map(|slots| slots.iter().flatten().copied().collect())
            .collect()
    }

    pub(crate) fn base_logical_index_from_path(
        &self,
        path: EdgeInsertPath,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<usize> {
        match path {
            EdgeInsertPath::BaseAppend { logical_index }
            | EdgeInsertPath::BaseReuseTombstone { logical_index } => Some(logical_index),
            EdgeInsertPath::Overflow => self
                .store
                .graph()
                .forward
                .resolve_logical_edge_slot(locator.vertex_ref, ordinal, locator)
                .and_then(|slot| match slot {
                    crate::ResolvedEdgeSlot::Base { logical_index } => Some(logical_index),
                    crate::ResolvedEdgeSlot::Overflow { .. } => None,
                }),
        }
    }

    pub(crate) fn base_logical_index_from_reverse_locator(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<usize> {
        self.store
            .graph()
            .reverse
            .resolve_logical_edge_slot(vertex_ref, ordinal, locator)
            .and_then(|slot| match slot {
                crate::ResolvedEdgeSlot::Base { logical_index } => Some(logical_index),
                crate::ResolvedEdgeSlot::Overflow { .. } => None,
            })
    }

    pub(crate) fn find_base_logical_index(
        slots: &[Option<EdgeId>],
        edge_id: EdgeId,
    ) -> Option<usize> {
        slots.iter().position(|slot| slot == &Some(edge_id))
    }

    pub(crate) fn set_base_slot(
        slots: &mut Vec<Option<EdgeId>>,
        logical_index: usize,
        edge_id: EdgeId,
    ) {
        if logical_index >= slots.len() {
            slots.resize(logical_index + 1, None);
        }
        slots[logical_index] = Some(edge_id);
    }

    pub(crate) fn label_id_for(&mut self, label: Option<&str>) -> LabelId {
        let Some(label) = label else {
            return 0;
        };
        if let Some(existing) = self.label_ids.get(label).copied() {
            return existing;
        }
        let label_id = self.next_label_id;
        self.next_label_id = self.next_label_id.saturating_add(1);
        self.label_ids.insert(label.to_owned(), label_id);
        label_id
    }

    pub(crate) fn lookup_label_id(&self, label: &str) -> Option<LabelId> {
        self.label_ids.get(label).copied()
    }

    pub(crate) fn sync_node_labels_to_index(&mut self, node_id: NodeId, labels: &[String]) {
        let Some(mapping) = self.vertex_mapping(node_id) else {
            return;
        };
        for label in labels {
            let label_id = self.label_id_for(Some(label));
            let threshold = self.promotion_threshold_for(label_id);
            self.vertex_label_index
                .insert(label_id, mapping.forward_ordinal, threshold);
        }
        let _ = self.persist_vertex_label_index();
    }

    pub(crate) fn remove_node_labels_from_index(&mut self, node_id: NodeId, labels: &[String]) {
        let Some(mapping) = self.vertex_mapping(node_id) else {
            return;
        };
        for label in labels {
            if let Some(label_id) = self.lookup_label_id(label) {
                self.vertex_label_index
                    .remove(label_id, mapping.forward_ordinal);
            }
        }
        let _ = self.persist_vertex_label_index();
    }

    fn promotion_threshold_for(&self, label_id: LabelId) -> usize {
        let card = self.vertex_label_index.cardinality(label_id);
        label_index::vertex_label_promotion_threshold(card)
    }

    pub(crate) fn hydrate_vertex_label_catalog(
        &self,
    ) -> Option<(
        BTreeMap<String, LabelId>,
        LabelId,
        VertexLabelIndex,
        VertexGcState,
    )> {
        let manager = self.store.manager();
        let region = manager.layout.region(RegionKind::LabelCatalog)?;
        let extent = manager.region_extent(RegionKind::LabelCatalog)?;
        let logical_len = usize::try_from(region.logical_len_bytes).ok()?;
        if logical_len == 0 {
            return Some((
                BTreeMap::new(),
                1,
                VertexLabelIndex::default(),
                VertexGcState::default(),
            ));
        }
        let mut bytes = vec![0u8; logical_len];
        self.memory.read(extent.addr.0, &mut bytes);
        decode_vertex_label_catalog(&bytes)
    }

    pub(crate) fn persist_vertex_label_index(&mut self) -> Option<()> {
        let bytes = encode_vertex_label_catalog(
            &self.label_ids,
            self.next_label_id,
            &self.vertex_label_index,
            &self.vertex_gc_state,
        );
        let mut manager = self.store.manager_mut();
        let extent = manager.region_extent(RegionKind::LabelCatalog)?;
        if bytes.len() > usize::try_from(extent.len_bytes).ok()? {
            return None;
        }
        if !bytes.is_empty() {
            self.memory.write(extent.addr.0, &bytes);
        }
        manager.set_region_logical_len(RegionKind::LabelCatalog, bytes.len() as u64)
    }

    pub(crate) fn enqueue_vertex_reclaim(&mut self, ordinal: usize) {
        if self.vertex_gc_state.tombstone_ordinals.insert(ordinal) {
            self.vertex_gc_state.reclaim_queue.push_back(ordinal);
        }
        let _ = self.persist_vertex_label_index();
    }

    pub(crate) fn vacuum_step_internal(&mut self, max_ops: usize) -> usize {
        let mut ops = 0usize;
        while ops < max_ops {
            let Some(ordinal) = self.vertex_gc_state.reclaim_queue.pop_front() else {
                break;
            };
            self.vertex_gc_state.queue_cursor = self.vertex_gc_state.queue_cursor.saturating_add(1);
            self.vertex_gc_state.tombstone_ordinals.remove(&ordinal);
            self.vertex_gc_state.free_list.push(ordinal);
            ops += 1;
        }
        if ops > 0 {
            let _ = self.persist_vertex_label_index();
        }
        ops
    }

    pub(crate) fn vacuum_stats(&self) -> VacuumStats {
        VacuumStats {
            tombstones: self.vertex_gc_state.tombstone_ordinals.len(),
            queue_len: self.vertex_gc_state.reclaim_queue.len(),
            free_list_len: self.vertex_gc_state.free_list.len(),
            queue_cursor: self.vertex_gc_state.queue_cursor,
        }
    }

    /// Forward-surface [`EdgeEntry`] for `edge_id` at the source vertex (canonical insert layout).
    pub(crate) fn forward_edge_entry_for_edge_id(&self, edge_id: EdgeId) -> Option<EdgeEntry> {
        let edge = self.edges.get(&edge_id)?;
        let src = edge.src;
        let mapping = self.vertex_mapping(src)?;
        let locator = self.edge_locators.get(&edge_id)?.forward;
        let vertex_ref = VertexRef::from(src);
        if locator.surface_kind() != SurfaceKind::Forward || locator.vertex_ref != vertex_ref {
            return None;
        }
        let ordinal = mapping.forward_ordinal;
        let forward = &self.store.graph().forward;
        let surface = &forward.0;
        let vertex = surface.vertex_entry(ordinal)?;
        match forward.resolve_logical_edge_slot(vertex_ref, ordinal, locator)? {
            ResolvedEdgeSlot::Base { logical_index } => surface
                .base_entries
                .live_entry_for_vertex(vertex, logical_index),
            ResolvedEdgeSlot::Overflow { offset, .. } => {
                Some(surface.overflow_entry(offset)?.entry)
            }
        }
    }

    /// Remote canister principal when this edge's forward metadata is cross-shard (`shard_canister`).
    pub(crate) fn shard_canister_principal_for_edge(&self, edge_id: EdgeId) -> Option<Principal> {
        let entry = self.forward_edge_entry_for_edge_id(edge_id)?;
        if !entry.meta.is_shard_canister() {
            return None;
        }
        let slot = entry.meta.shard_canister_slot()?;
        self.store.shard_canister_directory().principal(slot)
    }
}
