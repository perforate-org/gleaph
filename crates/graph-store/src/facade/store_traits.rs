use std::cell::{Ref, RefMut};

use crate::low_level::{GraphRebalancePlan, ShardCanisterDirectory};

use super::*;

impl<T> GraphStoreStore for &mut T
where
    T: GraphStoreStore + ?Sized,
{
    type Mem = T::Mem;

    fn last_write_event(&self) -> Option<&GraphStoreFacadeWriteEvent> {
        (**self).last_write_event()
    }

    fn write_history(&self) -> &[GraphStoreFacadeWriteEvent] {
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

    fn resolve_forward_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        (**self).resolve_forward_logical_edge_slot(vertex_ref, ordinal, locator)
    }

    fn resolve_reverse_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        (**self).resolve_reverse_logical_edge_slot(vertex_ref, ordinal, locator)
    }

    fn choose_insert_decision_with_incoming_live_entries(
        &self,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        incoming_live_entries: usize,
    ) -> Option<GraphInsertDecision> {
        (**self).choose_insert_decision_with_incoming_live_entries(
            src_vertex_ref,
            src_ordinal,
            dst_vertex_ref,
            dst_ordinal,
            incoming_live_entries,
        )
    }

    fn plan_local_rebalance(
        &self,
        plan: GraphRebalancePlan,
    ) -> Option<GraphLocalRebalancePlan> {
        (**self).plan_local_rebalance(plan)
    }

    fn build_local_rebalance_delta(
        &self,
        plan: GraphLocalRebalancePlan,
    ) -> Option<GraphLocalRebalanceDelta> {
        (**self).build_local_rebalance_delta(plan)
    }

    fn shard_canister_directory(&self) -> &ShardCanisterDirectory {
        (**self).shard_canister_directory()
    }

    fn shard_canister_directory_mut(&mut self) -> &mut ShardCanisterDirectory {
        (**self).shard_canister_directory_mut()
    }

    fn node_property_store(&self) -> &GraphStoreNodePropertyMap<Self::Mem> {
        (**self).node_property_store()
    }

    fn node_property_store_mut(&mut self) -> &mut GraphStoreNodePropertyMap<Self::Mem> {
        (**self).node_property_store_mut()
    }

    fn edge_property_store(&self) -> &GraphStoreEdgePropertyMap<Self::Mem> {
        (**self).edge_property_store()
    }

    fn edge_property_store_mut(&mut self) -> &mut GraphStoreEdgePropertyMap<Self::Mem> {
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
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        (**self).set_node_property_value_with_summary(node_id, property, value)
    }

    fn remove_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        (**self).remove_node_property_value_with_summary(node_id, property)
    }

    fn set_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        (**self).set_edge_property_value_with_summary(edge_id, property, value)
    }

    fn remove_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        (**self).remove_edge_property_value_with_summary(edge_id, property)
    }

    fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        (**self).set_node_property_value_and_write(node_id, property, value, memory)
    }

    fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        (**self).remove_node_property_value_and_write(node_id, property, memory)
    }

    fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        (**self).set_edge_property_value_and_write(edge_id, property, value, memory)
    }

    fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        (**self).remove_edge_property_value_and_write(edge_id, property, memory)
    }

    fn try_rebuild_logical_locator_sidecar(
        &mut self,
        forward_vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> GraphStoreResult<()> {
        (**self).try_rebuild_logical_locator_sidecar(
            forward_vertex_refs,
            forward_base_edge_ids_by_ordinal,
        )
    }

    fn try_write_all_to_stable_memory(&mut self, memory: &impl Memory) -> GraphStoreResult<()> {
        (**self).try_write_all_to_stable_memory(memory)
    }

    fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> GraphStoreResult<(Vec<usize>, Vec<usize>)> {
        (**self).try_refresh_and_write_dirty_to_stable_memory(memory)
    }

    fn append_empty_vertex_pair(&mut self) -> GraphStoreResult<(usize, usize)> {
        (**self).append_empty_vertex_pair()
    }

    fn append_empty_vertex_pairs(&mut self, count: usize) -> GraphStoreResult<Vec<(usize, usize)>> {
        (**self).append_empty_vertex_pairs(count)
    }

    fn bootstrap_vertex_refs_and_edges_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStoreBootstrapGraphWriteSummary> {
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
    ) -> Result<GraphStoreReplaceEdgeSummary, WritebackError> {
        (**self).replace_edge_pair_and_write(spec, memory)
    }

    fn tombstone_edge_pair_and_write(
        &mut self,
        spec: EdgeTombstoneSpec,
        memory: &impl Memory,
    ) -> Result<GraphStoreMutationWriteSummary<GraphMutationPath>, WritebackError> {
        (**self).tombstone_edge_pair_and_write(spec, memory)
    }

    fn merge_maintenance_dirty_forward_ordinal_interval(&mut self, start: u64, end: u64) {
        (**self).merge_maintenance_dirty_forward_ordinal_interval(start, end)
    }

    fn maintenance_dirty_forward_ordinal_interval_count(&self) -> u64 {
        (**self).maintenance_dirty_forward_ordinal_interval_count()
    }

    fn maintenance_queue_len(&self) -> usize {
        (**self).maintenance_queue_len()
    }

    fn property_maintenance_backlog(&self) -> GraphStorePropertyMaintenanceBacklog {
        (**self).property_maintenance_backlog()
    }

    fn peek_smallest_maintenance_dirty_forward_interval(&self) -> Option<(u64, u64)> {
        (**self).peek_smallest_maintenance_dirty_forward_interval()
    }

    fn drain_maintenance_dirty_into_queue_at_epoch_with_budget_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        current_epoch: Option<u64>,
        max_intervals: usize,
        budget: Option<&mut crate::InstructionBudget>,
        vertex_refs_base_ordinal: usize,
        memory: &impl Memory,
    ) -> Result<GraphStoreMaintenanceDirtyDrainSummary, WritebackError> {
        (**self).drain_maintenance_dirty_into_queue_at_epoch_with_budget_and_write(
            vertex_refs,
            current_epoch,
            max_intervals,
            budget,
            vertex_refs_base_ordinal,
            memory,
        )
    }

    fn run_queued_maintenance_cycles_with_segment_replacement_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
        retired_epoch: u64,
        max_cycles: usize,
        min_retired_epochs_before_sweep: u64,
    ) -> Result<GraphMaintenanceBatchWriteSummary, WritebackError> {
        (**self).run_queued_maintenance_cycles_with_segment_replacement_and_write(
            vertex_refs,
            forward_base_edge_ids_by_ordinal,
            memory,
            retired_epoch,
            max_cycles,
            min_retired_epochs_before_sweep,
        )
    }
}

impl<M: Memory + Clone> GraphStoreStore for GraphStore<M> {
    type Mem = M;

    fn last_write_event(&self) -> Option<&GraphStoreFacadeWriteEvent> {
        Self::last_write_event(self)
    }

    fn write_history(&self) -> &[GraphStoreFacadeWriteEvent] {
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

    fn resolve_forward_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        Self::resolve_forward_logical_edge_slot(self, vertex_ref, ordinal, locator)
    }

    fn resolve_reverse_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        Self::resolve_reverse_logical_edge_slot(self, vertex_ref, ordinal, locator)
    }

    fn choose_insert_decision_with_incoming_live_entries(
        &self,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        incoming_live_entries: usize,
    ) -> Option<GraphInsertDecision> {
        Self::choose_insert_decision_with_incoming_live_entries(
            self,
            src_vertex_ref,
            src_ordinal,
            dst_vertex_ref,
            dst_ordinal,
            incoming_live_entries,
        )
    }

    fn plan_local_rebalance(
        &self,
        plan: GraphRebalancePlan,
    ) -> Option<GraphLocalRebalancePlan> {
        Self::plan_local_rebalance(self, plan)
    }

    fn build_local_rebalance_delta(
        &self,
        plan: GraphLocalRebalancePlan,
    ) -> Option<GraphLocalRebalanceDelta> {
        Self::build_local_rebalance_delta(self, plan)
    }

    fn shard_canister_directory(&self) -> &ShardCanisterDirectory {
        &self.shard_canister_directory
    }

    fn shard_canister_directory_mut(&mut self) -> &mut ShardCanisterDirectory {
        &mut self.shard_canister_directory
    }

    fn node_property_store(&self) -> &GraphStoreNodePropertyMap<M> {
        Self::node_property_store(self)
    }

    fn node_property_store_mut(&mut self) -> &mut GraphStoreNodePropertyMap<M> {
        Self::node_property_store_mut(self)
    }

    fn edge_property_store(&self) -> &GraphStoreEdgePropertyMap<M> {
        Self::edge_property_store(self)
    }

    fn edge_property_store_mut(&mut self) -> &mut GraphStoreEdgePropertyMap<M> {
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
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        Self::set_node_property_value_with_summary(self, node_id, property, value)
    }

    fn remove_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        Self::remove_node_property_value_with_summary(self, node_id, property)
    }

    fn set_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        Self::set_edge_property_value_with_summary(self, edge_id, property, value)
    }

    fn remove_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        Self::remove_edge_property_value_with_summary(self, edge_id, property)
    }

    fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        Self::set_node_property_value_and_write(self, node_id, property, value, memory)
    }

    fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        Self::remove_node_property_value_and_write(self, node_id, property, memory)
    }

    fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        Self::set_edge_property_value_and_write(self, edge_id, property, value, memory)
    }

    fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        Self::remove_edge_property_value_and_write(self, edge_id, property, memory)
    }

    fn try_rebuild_logical_locator_sidecar(
        &mut self,
        forward_vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> GraphStoreResult<()> {
        Self::try_rebuild_logical_locator_sidecar(
            self,
            forward_vertex_refs,
            forward_base_edge_ids_by_ordinal,
        )
    }

    fn try_write_all_to_stable_memory(&mut self, memory: &impl Memory) -> GraphStoreResult<()> {
        Self::try_write_all_to_stable_memory(self, memory)
    }

    fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> GraphStoreResult<(Vec<usize>, Vec<usize>)> {
        Self::try_refresh_and_write_dirty_to_stable_memory(self, memory)
    }

    fn append_empty_vertex_pair(&mut self) -> GraphStoreResult<(usize, usize)> {
        Self::append_empty_vertex_pair(self)
    }

    fn append_empty_vertex_pairs(&mut self, count: usize) -> GraphStoreResult<Vec<(usize, usize)>> {
        Self::append_empty_vertex_pairs(self, count)
    }

    fn bootstrap_vertex_refs_and_edges_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStoreBootstrapGraphWriteSummary> {
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
    ) -> Result<GraphStoreReplaceEdgeSummary, WritebackError> {
        Self::replace_edge_pair_and_write(self, spec, memory)
    }

    fn tombstone_edge_pair_and_write(
        &mut self,
        spec: EdgeTombstoneSpec,
        memory: &impl Memory,
    ) -> Result<GraphStoreMutationWriteSummary<GraphMutationPath>, WritebackError> {
        Self::tombstone_edge_pair_and_write(self, spec, memory)
    }

    fn merge_maintenance_dirty_forward_ordinal_interval(&mut self, start: u64, end: u64) {
        GraphStore::merge_maintenance_dirty_forward_ordinal_interval(self, start, end)
    }

    fn maintenance_dirty_forward_ordinal_interval_count(&self) -> u64 {
        Self::maintenance_dirty_forward_ordinal_interval_count(self)
    }

    fn maintenance_queue_len(&self) -> usize {
        Self::maintenance_queue_len(self)
    }

    fn property_maintenance_backlog(&self) -> GraphStorePropertyMaintenanceBacklog {
        Self::property_maintenance_backlog(self)
    }

    fn peek_smallest_maintenance_dirty_forward_interval(&self) -> Option<(u64, u64)> {
        Self::peek_smallest_maintenance_dirty_forward_interval(self)
    }

    fn drain_maintenance_dirty_into_queue_at_epoch_with_budget_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        current_epoch: Option<u64>,
        max_intervals: usize,
        budget: Option<&mut crate::InstructionBudget>,
        vertex_refs_base_ordinal: usize,
        memory: &impl Memory,
    ) -> Result<GraphStoreMaintenanceDirtyDrainSummary, WritebackError> {
        Self::drain_maintenance_dirty_into_queue_at_epoch_with_budget_and_write(
            self,
            vertex_refs,
            current_epoch,
            max_intervals,
            budget,
            vertex_refs_base_ordinal,
            memory,
        )
    }

    fn run_queued_maintenance_cycles_with_segment_replacement_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
        retired_epoch: u64,
        max_cycles: usize,
        min_retired_epochs_before_sweep: u64,
    ) -> Result<GraphMaintenanceBatchWriteSummary, WritebackError> {
        Self::run_queued_maintenance_cycles_with_segment_replacement_and_write(
            self,
            vertex_refs,
            forward_base_edge_ids_by_ordinal,
            memory,
            retired_epoch,
            max_cycles,
            min_retired_epochs_before_sweep,
        )
    }
}
