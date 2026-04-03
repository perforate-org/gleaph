use std::cell::{Ref, RefMut};

use super::*;

impl<T> GraphPmaStore for &mut T
where
    T: GraphPmaStore + ?Sized,
{
    type Mem = T::Mem;

    fn last_write_event(&self) -> Option<&GraphPmaFacadeWriteEvent> {
        (**self).last_write_event()
    }

    fn write_history(&self) -> &[GraphPmaFacadeWriteEvent] {
        (**self).write_history()
    }

    fn manager(&self) -> Ref<'_, RegionManager> {
        (**self).manager()
    }

    fn manager_mut(&mut self) -> RefMut<'_, RegionManager> {
        (**self).manager_mut()
    }

    fn graph(&self) -> &GraphRuntime {
        (**self).graph()
    }

    fn graph_mut(&mut self) -> &mut GraphRuntime {
        (**self).graph_mut()
    }

    fn node_property_store(&self) -> &GraphPropertyStableMap<Self::Mem> {
        (**self).node_property_store()
    }

    fn node_property_store_mut(&mut self) -> &mut GraphPropertyStableMap<Self::Mem> {
        (**self).node_property_store_mut()
    }

    fn edge_property_store(&self) -> &GraphPropertyStableMap<Self::Mem> {
        (**self).edge_property_store()
    }

    fn edge_property_store_mut(&mut self) -> &mut GraphPropertyStableMap<Self::Mem> {
        (**self).edge_property_store_mut()
    }

    fn scan_node_properties(&self, node_id: NodeId) -> PropertyMap {
        (**self).scan_node_properties(node_id)
    }

    fn scan_edge_properties(&self, edge_id: EdgeId) -> PropertyMap {
        (**self).scan_edge_properties(edge_id)
    }

    fn scan_node_properties_batch(&self, node_ids: &[NodeId]) -> BTreeMap<NodeId, PropertyMap> {
        (**self).scan_node_properties_batch(node_ids)
    }

    fn scan_node_properties_batch_subset(
        &self,
        node_ids: &[NodeId],
        property_names: &BTreeSet<String>,
    ) -> BTreeMap<NodeId, PropertyMap> {
        (**self).scan_node_properties_batch_subset(node_ids, property_names)
    }

    fn scan_edge_properties_batch_subset(
        &self,
        edge_ids: &[EdgeId],
        property_names: &BTreeSet<String>,
    ) -> BTreeMap<EdgeId, PropertyMap> {
        (**self).scan_edge_properties_batch_subset(edge_ids, property_names)
    }

    fn get_node_property_value(&self, node_id: NodeId, property: &str) -> Option<Value> {
        (**self).get_node_property_value(node_id, property)
    }

    fn get_edge_property_value(&self, edge_id: EdgeId, property: &str) -> Option<Value> {
        (**self).get_edge_property_value(edge_id, property)
    }

    fn distinct_node_property_names(&self) -> BTreeSet<String> {
        (**self).distinct_node_property_names()
    }

    fn distinct_edge_property_names(&self) -> BTreeSet<String> {
        (**self).distinct_edge_property_names()
    }

    fn scan_node_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<NodeId> {
        (**self).scan_node_ids_by_property_eq(property, value)
    }

    fn scan_node_ids_by_property(&self, property: &str) -> Vec<NodeId> {
        (**self).scan_node_ids_by_property(property)
    }

    fn scan_edge_ids_by_property(&self, property: &str) -> Vec<EdgeId> {
        (**self).scan_edge_ids_by_property(property)
    }

    fn scan_edge_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<EdgeId> {
        (**self).scan_edge_ids_by_property_eq(property, value)
    }

    fn node_property_store_is_dirty(&self) -> bool {
        (**self).node_property_store_is_dirty()
    }

    fn edge_property_store_is_dirty(&self) -> bool {
        (**self).edge_property_store_is_dirty()
    }

    fn set_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        (**self).set_node_property_value(node_id, property, value)
    }

    fn remove_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        (**self).remove_node_property_value(node_id, property)
    }

    fn set_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        (**self).set_edge_property_value(edge_id, property, value)
    }

    fn remove_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        (**self).remove_edge_property_value(edge_id, property)
    }

    fn set_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        (**self).set_node_property_value_with_summary(node_id, property, value)
    }

    fn remove_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        (**self).remove_node_property_value_with_summary(node_id, property)
    }

    fn set_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        (**self).set_edge_property_value_with_summary(edge_id, property, value)
    }

    fn remove_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        (**self).remove_edge_property_value_with_summary(edge_id, property)
    }

    fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        (**self).set_node_property_value_and_write(node_id, property, value, memory)
    }

    fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        (**self).remove_node_property_value_and_write(node_id, property, memory)
    }

    fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        (**self).set_edge_property_value_and_write(edge_id, property, value, memory)
    }

    fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        (**self).remove_edge_property_value_and_write(edge_id, property, memory)
    }

    fn try_rebuild_logical_locator_sidecar(
        &mut self,
        forward_vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> GraphPmaResult<()> {
        (**self).try_rebuild_logical_locator_sidecar(
            forward_vertex_refs,
            forward_base_edge_ids_by_ordinal,
        )
    }

    fn try_write_all_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> GraphPmaResult<()> {
        (**self).try_write_all_to_stable_memory(memory)
    }

    fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> GraphPmaResult<(Vec<usize>, Vec<usize>)> {
        (**self).try_refresh_and_write_dirty_to_stable_memory(memory)
    }

    fn append_empty_vertex_pair(&mut self) -> GraphPmaResult<(usize, usize)> {
        (**self).append_empty_vertex_pair()
    }

    fn append_empty_vertex_pairs(
        &mut self,
        count: usize,
    ) -> GraphPmaResult<Vec<(usize, usize)>> {
        (**self).append_empty_vertex_pairs(count)
    }

    fn bootstrap_vertex_refs_and_edges_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaBootstrapGraphWriteSummary> {
        (**self).bootstrap_vertex_refs_and_edges_and_write(vertex_refs, initial_edges, memory)
    }

    fn insert_edge_pair_with_local_rebalance_and_write(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        (**self).insert_edge_pair_with_local_rebalance_and_write(spec, memory)
    }

    fn replace_edge_pair_and_write(
        &mut self,
        spec: EdgeReplaceSpec,
        memory: &impl Memory,
    ) -> Result<GraphPmaReplaceEdgeSummary, WritebackError> {
        (**self).replace_edge_pair_and_write(spec, memory)
    }

    fn tombstone_edge_pair_and_write(
        &mut self,
        spec: EdgeTombstoneSpec,
        memory: &impl Memory,
    ) -> Result<GraphPmaMutationWriteSummary<GraphMutationPath>, WritebackError> {
        (**self).tombstone_edge_pair_and_write(spec, memory)
    }
}

impl<M: Memory> GraphPmaStore for GraphPma<M> {
    type Mem = M;

    fn last_write_event(&self) -> Option<&GraphPmaFacadeWriteEvent> {
        Self::last_write_event(self)
    }

    fn write_history(&self) -> &[GraphPmaFacadeWriteEvent] {
        Self::write_history(self)
    }

    fn manager(&self) -> Ref<'_, RegionManager> {
        Self::manager(self)
    }

    fn manager_mut(&mut self) -> RefMut<'_, RegionManager> {
        Self::manager_mut(self)
    }

    fn graph(&self) -> &GraphRuntime {
        Self::graph(self)
    }

    fn graph_mut(&mut self) -> &mut GraphRuntime {
        Self::graph_mut(self)
    }

    fn node_property_store(&self) -> &GraphPropertyStableMap<M> {
        Self::node_property_store(self)
    }

    fn node_property_store_mut(&mut self) -> &mut GraphPropertyStableMap<M> {
        Self::node_property_store_mut(self)
    }

    fn edge_property_store(&self) -> &GraphPropertyStableMap<M> {
        Self::edge_property_store(self)
    }

    fn edge_property_store_mut(&mut self) -> &mut GraphPropertyStableMap<M> {
        Self::edge_property_store_mut(self)
    }

    fn scan_node_properties(&self, node_id: NodeId) -> PropertyMap {
        Self::scan_node_properties(self, node_id)
    }

    fn scan_edge_properties(&self, edge_id: EdgeId) -> PropertyMap {
        Self::scan_edge_properties(self, edge_id)
    }

    fn scan_node_properties_batch(&self, node_ids: &[NodeId]) -> BTreeMap<NodeId, PropertyMap> {
        Self::scan_node_properties_batch(self, node_ids)
    }

    fn scan_node_properties_batch_subset(
        &self,
        node_ids: &[NodeId],
        property_names: &BTreeSet<String>,
    ) -> BTreeMap<NodeId, PropertyMap> {
        Self::scan_node_properties_batch_subset(self, node_ids, property_names)
    }

    fn scan_edge_properties_batch_subset(
        &self,
        edge_ids: &[EdgeId],
        property_names: &BTreeSet<String>,
    ) -> BTreeMap<EdgeId, PropertyMap> {
        Self::scan_edge_properties_batch_subset(self, edge_ids, property_names)
    }

    fn get_node_property_value(&self, node_id: NodeId, property: &str) -> Option<Value> {
        Self::get_node_property_value(self, node_id, property)
    }

    fn get_edge_property_value(&self, edge_id: EdgeId, property: &str) -> Option<Value> {
        Self::get_edge_property_value(self, edge_id, property)
    }

    fn distinct_node_property_names(&self) -> BTreeSet<String> {
        Self::distinct_node_property_names(self)
    }

    fn distinct_edge_property_names(&self) -> BTreeSet<String> {
        Self::distinct_edge_property_names(self)
    }

    fn scan_node_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<NodeId> {
        Self::scan_node_ids_by_property_eq(self, property, value)
    }

    fn scan_node_ids_by_property(&self, property: &str) -> Vec<NodeId> {
        Self::scan_node_ids_by_property(self, property)
    }

    fn scan_edge_ids_by_property(&self, property: &str) -> Vec<EdgeId> {
        Self::scan_edge_ids_by_property(self, property)
    }

    fn scan_edge_ids_by_property_eq(&self, property: &str, value: &Value) -> Vec<EdgeId> {
        Self::scan_edge_ids_by_property_eq(self, property, value)
    }

    fn node_property_store_is_dirty(&self) -> bool {
        Self::node_property_store_is_dirty(self)
    }

    fn edge_property_store_is_dirty(&self) -> bool {
        Self::edge_property_store_is_dirty(self)
    }

    fn set_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        Self::set_node_property_value(self, node_id, property, value)
    }

    fn remove_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        Self::remove_node_property_value(self, node_id, property)
    }

    fn set_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        Self::set_edge_property_value(self, edge_id, property, value)
    }

    fn remove_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        Self::remove_edge_property_value(self, edge_id, property)
    }

    fn set_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        Self::set_node_property_value_with_summary(self, node_id, property, value)
    }

    fn remove_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        Self::remove_node_property_value_with_summary(self, node_id, property)
    }

    fn set_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        Self::set_edge_property_value_with_summary(self, edge_id, property, value)
    }

    fn remove_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        Self::remove_edge_property_value_with_summary(self, edge_id, property)
    }

    fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        Self::set_node_property_value_and_write(self, node_id, property, value, memory)
    }

    fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        Self::remove_node_property_value_and_write(self, node_id, property, memory)
    }

    fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        Self::set_edge_property_value_and_write(self, edge_id, property, value, memory)
    }

    fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        Self::remove_edge_property_value_and_write(self, edge_id, property, memory)
    }

    fn try_rebuild_logical_locator_sidecar(
        &mut self,
        forward_vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> GraphPmaResult<()> {
        Self::try_rebuild_logical_locator_sidecar(
            self,
            forward_vertex_refs,
            forward_base_edge_ids_by_ordinal,
        )
    }

    fn try_write_all_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> GraphPmaResult<()> {
        Self::try_write_all_to_stable_memory(self, memory)
    }

    fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> GraphPmaResult<(Vec<usize>, Vec<usize>)> {
        Self::try_refresh_and_write_dirty_to_stable_memory(self, memory)
    }

    fn append_empty_vertex_pair(&mut self) -> GraphPmaResult<(usize, usize)> {
        Self::append_empty_vertex_pair(self)
    }

    fn append_empty_vertex_pairs(
        &mut self,
        count: usize,
    ) -> GraphPmaResult<Vec<(usize, usize)>> {
        Self::append_empty_vertex_pairs(self, count)
    }

    fn bootstrap_vertex_refs_and_edges_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaBootstrapGraphWriteSummary> {
        Self::bootstrap_vertex_refs_and_edges_and_write(self, vertex_refs, initial_edges, memory)
    }

    fn insert_edge_pair_with_local_rebalance_and_write(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        Self::insert_edge_pair_with_local_rebalance_and_write(self, spec, memory)
    }

    fn replace_edge_pair_and_write(
        &mut self,
        spec: EdgeReplaceSpec,
        memory: &impl Memory,
    ) -> Result<GraphPmaReplaceEdgeSummary, WritebackError> {
        Self::replace_edge_pair_and_write(self, spec, memory)
    }

    fn tombstone_edge_pair_and_write(
        &mut self,
        spec: EdgeTombstoneSpec,
        memory: &impl Memory,
    ) -> Result<GraphPmaMutationWriteSummary<GraphMutationPath>, WritebackError> {
        Self::tombstone_edge_pair_and_write(self, spec, memory)
    }
}
