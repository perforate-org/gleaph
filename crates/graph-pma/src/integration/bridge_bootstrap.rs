use std::collections::BTreeMap;

use candid::Principal;
use gleaph_graph_kernel::{
    EdgeId, EdgeRecord, GraphError, GraphResult, NodeId, NodeRecord, PropertyMap,
};

use crate::facade::{
    GraphPmaEdgeLogicalLocatorMapping, GraphPmaPropertyIndexTouchedSections,
    GraphPmaPropertyMutationWriteSummary, GraphPmaRefreshedVertices, GraphPmaStore,
    GraphPmaVertexOrdinalMapping,
};
use crate::low_level::{EdgeDirectedMetaPair, EdgeInsertPath, EdgeMeta, VertexRef};

use super::{
    GraphPmaKernelBootstrapBridge, GraphPmaOverlayBootstrapGraphSummary,
    GraphPmaOverlayEdgeBootstrapSummary, GraphPmaOverlayEdgeWriteSummary,
    GraphPmaOverlayInsertEdgeSummary, GraphPmaOverlayNodeBootstrapSummary,
    GraphPmaOverlayNodeDeleteSummary, GraphPmaOverlayWriteEvent, OVERLAY_SUMMARY_HISTORY_LIMIT,
    VertexGcState, VertexLabelIndex,
};

impl<'a, S: GraphPmaStore> GraphPmaKernelBootstrapBridge<'a, S> {
    fn rollback_bootstrapped_node_properties(
        &mut self,
        node_id: NodeId,
        properties: &PropertyMap,
    ) -> GraphResult<()> {
        for name in properties.keys() {
            self.store
                .remove_node_property_value(node_id, name)
                .map_err(|e| GraphError::Message(e.to_string()))?;
        }
        Ok(())
    }

    /// Creates one bootstrap bridge over a bound graph adapter.
    pub fn new(store: S, memory: &'a S::Mem) -> Self {
        let mut bridge = Self {
            store,
            memory,
            next_node_id: 0,
            next_edge_id: 0,
            next_label_id: 1,
            label_ids: BTreeMap::new(),
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            incident_edge_ids: BTreeMap::new(),
            vertex_ordinals: Vec::new(),
            semantic_node_id_by_forward_ordinal: Vec::new(),
            vertex_ordinal_by_node_id: BTreeMap::new(),
            vertex_label_index: VertexLabelIndex::default(),
            vertex_gc_state: VertexGcState::default(),
            edge_locators: BTreeMap::new(),
            forward_base_slots_by_ordinal: Vec::new(),
            reverse_base_slots_by_ordinal: Vec::new(),
            last_property_write_summary: None,
            last_insert_edge_summary: None,
            last_edge_write_summary: None,
            last_node_delete_summary: None,
            property_write_history: Vec::new(),
            insert_edge_history: Vec::new(),
            edge_write_history: Vec::new(),
            node_delete_history: Vec::new(),
            write_history: Vec::new(),
        };
        if let Some((label_ids, next_label_id, index, gc_state)) =
            bridge.hydrate_vertex_label_catalog()
        {
            bridge.label_ids = label_ids;
            bridge.next_label_id = next_label_id;
            bridge.vertex_label_index = index;
            bridge.vertex_gc_state = gc_state;
        }
        bridge
    }

    /// Returns the currently bootstrapped node records.
    pub fn nodes(&self) -> &BTreeMap<NodeId, NodeRecord> {
        &self.nodes
    }

    /// Returns the currently bootstrapped edge records.
    pub fn edges(&self) -> &BTreeMap<EdgeId, EdgeRecord> {
        &self.edges
    }

    /// Returns surface-local vertex ordinal mappings in forward order.
    pub fn vertex_ordinals(&self) -> &[GraphPmaVertexOrdinalMapping] {
        &self.vertex_ordinals
    }

    /// Returns the most recent property-write summary observed through this bridge.
    pub fn last_property_write_summary(&self) -> Option<&GraphPmaPropertyMutationWriteSummary> {
        self.last_property_write_summary.as_ref()
    }

    /// Returns recent property-write summaries in observation order.
    pub fn property_write_history(&self) -> &[GraphPmaPropertyMutationWriteSummary] {
        &self.property_write_history
    }

    /// Returns the most recent insert-edge summary observed through this bridge.
    pub fn last_insert_edge_summary(&self) -> Option<&GraphPmaOverlayInsertEdgeSummary> {
        self.last_insert_edge_summary.as_ref()
    }

    /// Returns recent insert-edge summaries in observation order.
    pub fn insert_edge_history(&self) -> &[GraphPmaOverlayInsertEdgeSummary] {
        &self.insert_edge_history
    }

    /// Returns the most recent edge-write summary observed through this bridge.
    pub fn last_edge_write_summary(&self) -> Option<&GraphPmaOverlayEdgeWriteSummary> {
        self.last_edge_write_summary.as_ref()
    }

    /// Returns recent edge-write summaries in observation order.
    pub fn edge_write_history(&self) -> &[GraphPmaOverlayEdgeWriteSummary] {
        &self.edge_write_history
    }

    /// Returns the most recent node-delete summary observed through this bridge.
    pub fn last_node_delete_summary(&self) -> Option<&GraphPmaOverlayNodeDeleteSummary> {
        self.last_node_delete_summary.as_ref()
    }

    /// Returns recent node-delete summaries in observation order.
    pub fn node_delete_history(&self) -> &[GraphPmaOverlayNodeDeleteSummary] {
        &self.node_delete_history
    }

    /// Returns recent overlay write events in observation order.
    pub fn write_history(&self) -> &[GraphPmaOverlayWriteEvent] {
        &self.write_history
    }

    fn record_write_event(&mut self, event: GraphPmaOverlayWriteEvent) {
        self.write_history.push(event);
        if self.write_history.len() > OVERLAY_SUMMARY_HISTORY_LIMIT {
            self.write_history.remove(0);
        }
    }

    pub(crate) fn record_property_write_summary(
        &mut self,
        summary: GraphPmaPropertyMutationWriteSummary,
    ) {
        self.record_write_event(GraphPmaOverlayWriteEvent::Property(summary.clone()));
        self.last_property_write_summary = Some(summary.clone());
        self.property_write_history.push(summary);
        if self.property_write_history.len() > OVERLAY_SUMMARY_HISTORY_LIMIT {
            self.property_write_history.remove(0);
        }
    }

    pub(crate) fn patch_pending_property_summaries_after_stable_flush(
        &mut self,
        refreshed: GraphPmaRefreshedVertices,
    ) {
        for event in &mut self.write_history {
            if let GraphPmaOverlayWriteEvent::Property(summary) = event
                && summary.is_pending_stable_flush()
            {
                summary.flushed_sections = summary.mutation.sections;
                if !summary.flushed_sections.property_store
                    && !summary.flushed_sections.logical_index
                    && !summary.flushed_sections.node_store
                {
                    summary.flushed_sections = GraphPmaPropertyIndexTouchedSections {
                        property_store: true,
                        logical_index: true,
                        node_store: true,
                    };
                }
                summary.refreshed = refreshed.clone();
            }
        }
        for summary in &mut self.property_write_history {
            if summary.is_pending_stable_flush() {
                summary.flushed_sections = summary.mutation.sections;
                if !summary.flushed_sections.property_store
                    && !summary.flushed_sections.logical_index
                    && !summary.flushed_sections.node_store
                {
                    summary.flushed_sections = GraphPmaPropertyIndexTouchedSections {
                        property_store: true,
                        logical_index: true,
                        node_store: true,
                    };
                }
                summary.refreshed = refreshed.clone();
            }
        }
        if let Some(last) = self.property_write_history.last() {
            self.last_property_write_summary = Some(last.clone());
        }
    }

    pub(crate) fn record_insert_edge_summary(&mut self, summary: GraphPmaOverlayInsertEdgeSummary) {
        self.record_write_event(GraphPmaOverlayWriteEvent::InsertEdge(summary.clone()));
        self.last_insert_edge_summary = Some(summary.clone());
        self.insert_edge_history.push(summary);
        if self.insert_edge_history.len() > OVERLAY_SUMMARY_HISTORY_LIMIT {
            self.insert_edge_history.remove(0);
        }
    }

    pub(crate) fn record_edge_write_summary(&mut self, summary: GraphPmaOverlayEdgeWriteSummary) {
        self.record_write_event(GraphPmaOverlayWriteEvent::Edge(summary.clone()));
        self.last_edge_write_summary = Some(summary.clone());
        self.edge_write_history.push(summary);
        if self.edge_write_history.len() > OVERLAY_SUMMARY_HISTORY_LIMIT {
            self.edge_write_history.remove(0);
        }
    }

    pub(crate) fn record_node_delete_summary(&mut self, summary: GraphPmaOverlayNodeDeleteSummary) {
        self.record_write_event(GraphPmaOverlayWriteEvent::NodeDelete(summary.clone()));
        self.last_node_delete_summary = Some(summary.clone());
        self.node_delete_history.push(summary);
        if self.node_delete_history.len() > OVERLAY_SUMMARY_HISTORY_LIMIT {
            self.node_delete_history.remove(0);
        }
    }

    pub(crate) fn record_node_bootstrap_summary(
        &mut self,
        summary: GraphPmaOverlayNodeBootstrapSummary,
    ) {
        self.record_write_event(GraphPmaOverlayWriteEvent::BootstrapNode(summary));
    }

    pub(crate) fn record_edge_bootstrap_summary(
        &mut self,
        summary: GraphPmaOverlayEdgeBootstrapSummary,
    ) {
        self.record_write_event(GraphPmaOverlayWriteEvent::BootstrapEdge(summary));
    }

    pub(crate) fn record_bootstrap_graph_summary(
        &mut self,
        summary: GraphPmaOverlayBootstrapGraphSummary,
    ) {
        self.record_write_event(GraphPmaOverlayWriteEvent::BootstrapGraph(summary));
    }

    /// Bootstraps one logical node by appending one new vertex slot pair.
    pub fn bootstrap_node(
        &mut self,
        labels: &[String],
        properties: &PropertyMap,
    ) -> GraphResult<NodeRecord> {
        for label in labels {
            gleaph_gql::name_limits::validate_label_name(label)
                .map_err(|e| GraphError::Message(e.to_string()))?;
        }
        for name in properties.keys() {
            gleaph_gql::name_limits::validate_property_name(name)
                .map_err(|e| GraphError::Message(e.to_string()))?;
        }
        let next_id = self
            .next_node_id
            .checked_add(1)
            .ok_or_else(|| GraphError::Message("node id overflow during bootstrap".into()))?;
        let node_id = NodeId::try_from(next_id)
            .map_err(|_| GraphError::Message("node id overflow during bootstrap".into()))?;

        {
            let _p = crate::bench_profile::PhaseGuard::new("bootstrap_node_persist_properties");
            self.persist_node_properties(node_id, properties)?;
        }

        let summary = {
            let _p = crate::bench_profile::PhaseGuard::new("bootstrap_node_vertex_refs_and_write");
            match self.store.bootstrap_vertex_refs_and_edges_and_write(
                &[node_id.into()],
                &[],
                self.memory,
            ) {
                Ok(s) => s,
                Err(err) => {
                    let _p = crate::bench_profile::PhaseGuard::new("bootstrap_node_rollback_props");
                    self.rollback_bootstrapped_node_properties(node_id, properties)?;
                    return Err(GraphError::Message(err.to_string()));
                }
            }
        };
        self.next_node_id = next_id;

        let refreshed = summary.refreshed.clone();
        let mapping =
            summary.vertex_ordinals.into_iter().next().ok_or_else(|| {
                GraphError::Message("bootstrap did not create vertex mapping".into())
            })?;

        self.vertex_ordinals.push(mapping);
        self.semantic_node_id_by_forward_ordinal.push(Some(node_id));
        self.vertex_ordinal_by_node_id.insert(node_id, mapping);
        self.forward_base_slots_by_ordinal.push(Vec::new());
        self.reverse_base_slots_by_ordinal.push(Vec::new());

        let record = NodeRecord {
            id: node_id,
            labels: labels.to_vec(),
            properties: properties.clone(),
        };
        self.nodes.insert(node_id, record.clone());
        self.sync_node_labels_to_index(node_id, &record.labels);
        self.record_node_bootstrap_summary(GraphPmaOverlayNodeBootstrapSummary {
            node: record.clone(),
            ordinals: (mapping.forward_ordinal, mapping.reverse_ordinal),
            refreshed,
        });
        Ok(record)
    }

    /// Bootstraps one logical edge between already bootstrapped nodes.
    pub fn bootstrap_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        self.insert_edge_record(src, dst, label, properties, true)
    }

    /// Like [`Self::bootstrap_edge`], but stores a shard-canister slot on the forward [`EdgeMeta`].
    pub fn bootstrap_edge_with_shard_canister_dst(
        &mut self,
        src: NodeId,
        dst: NodeId,
        shard_canister: Principal,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        self.validate_edge_insert_inputs(label, properties)?;
        let slot = self
            .store
            .shard_canister_directory_mut()
            .push_principal(shard_canister, false)
            .ok_or_else(|| GraphError::Message("shard canister directory full".into()))?;
        let label_id = self.label_id_for(label);
        let edge_meta = EdgeDirectedMetaPair {
            forward: EdgeMeta::new_shard_canister(slot, false),
            reverse: EdgeMeta::new(label_id, false),
        };
        self.insert_edge_record_with_meta(src, dst, label, properties, true, edge_meta)
    }

    /// Inserts one logical edge between already bootstrapped nodes.
    pub fn insert_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        self.insert_edge_record(src, dst, label, properties, false)
    }

    /// Resolves shard canister slots for cross-canister adjacency metadata.
    pub fn shard_canister_directory(&self) -> &crate::low_level::ShardCanisterDirectory {
        self.store.shard_canister_directory()
    }

    /// Mutable shard directory (e.g. to pre-register principals before inserting edges).
    pub fn shard_canister_directory_mut(&mut self) -> &mut crate::low_level::ShardCanisterDirectory {
        self.store.shard_canister_directory_mut()
    }

    /// Inserts an edge from local `src` to local stub `dst` that represents a vertex on `shard_canister`.
    ///
    /// Forward [`EdgeMeta`] stores a shard slot; reverse stores the local label id.
    pub fn insert_edge_with_shard_canister_dst(
        &mut self,
        src: NodeId,
        dst: NodeId,
        shard_canister: Principal,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        self.validate_edge_insert_inputs(label, properties)?;
        let slot = self
            .store
            .shard_canister_directory_mut()
            .push_principal(shard_canister, false)
            .ok_or_else(|| GraphError::Message("shard canister directory full".into()))?;
        let label_id = self.label_id_for(label);
        let edge_meta = EdgeDirectedMetaPair {
            forward: EdgeMeta::new_shard_canister(slot, false),
            reverse: EdgeMeta::new(label_id, false),
        };
        self.insert_edge_record_with_meta(src, dst, label, properties, false, edge_meta)
    }

    pub(crate) fn register_incident_edge(&mut self, src: NodeId, dst: NodeId, edge_id: EdgeId) {
        self.incident_edge_ids.entry(src).or_default().push(edge_id);
        if dst != src {
            self.incident_edge_ids.entry(dst).or_default().push(edge_id);
        }
    }

    pub(crate) fn unregister_incident_edge(&mut self, src: NodeId, dst: NodeId, edge_id: EdgeId) {
        if src == dst {
            let empty = match self.incident_edge_ids.get_mut(&src) {
                None => return,
                Some(v) => {
                    v.retain(|&id| id != edge_id);
                    v.is_empty()
                }
            };
            if empty {
                self.incident_edge_ids.remove(&src);
            }
            return;
        }
        for nid in [src, dst] {
            let empty = match self.incident_edge_ids.get_mut(&nid) {
                None => continue,
                Some(v) => {
                    v.retain(|&id| id != edge_id);
                    v.is_empty()
                }
            };
            if empty {
                self.incident_edge_ids.remove(&nid);
            }
        }
    }

    fn validate_edge_insert_inputs(
        &self,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<()> {
        if let Some(l) = label {
            gleaph_gql::name_limits::validate_label_name(l)
                .map_err(|e| GraphError::Message(e.to_string()))?;
        }
        for name in properties.keys() {
            gleaph_gql::name_limits::validate_property_name(name)
                .map_err(|e| GraphError::Message(e.to_string()))?;
        }
        Ok(())
    }

    fn insert_edge_record(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
        bootstrap_event: bool,
    ) -> GraphResult<EdgeRecord> {
        self.validate_edge_insert_inputs(label, properties)?;
        let edge_meta = self.label_id_for(label).into();
        self.insert_edge_record_with_meta(src, dst, label, properties, bootstrap_event, edge_meta)
    }

    fn insert_edge_record_with_meta(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
        bootstrap_event: bool,
        edge_meta: EdgeDirectedMetaPair,
    ) -> GraphResult<EdgeRecord> {
        let src_mapping = self
            .vertex_mapping(src)
            .ok_or(GraphError::NodeNotFound(src))?;
        let dst_mapping = self
            .vertex_mapping(dst)
            .ok_or(GraphError::NodeNotFound(dst))?;

        self.next_edge_id += 1;
        let edge_id = self.next_edge_id;
        let (forward_rebalance_vertex_ids, forward_rebalance_base_edge_ids_by_ordinal) = match self
            .store
            .graph()
            .choose_insert_decision_with_incoming_live_entries(
                src.into(),
                src_mapping.forward_ordinal,
                dst.into(),
                dst_mapping.reverse_ordinal,
                1,
            ) {
            Some(crate::GraphInsertDecision::RebalanceRequired(plan)) => {
                let local = self
                    .store
                    .graph()
                    .plan_local_rebalance(plan)
                    .ok_or_else(|| {
                        GraphError::Message(
                            "failed to build local rebalance window for overlay insert".into(),
                        )
                    })?;
                let start = local.forward.start_ordinal;
                let end = local.forward.end_ordinal_exclusive;
                let vertex_ids: Vec<VertexRef> = self
                    .vertex_ordinals
                    .get(start..end)
                    .ok_or_else(|| {
                        GraphError::Message(
                            "rebalance window exceeds overlay vertex mappings".into(),
                        )
                    })?
                    .iter()
                    .map(|mapping| mapping.vertex_ref)
                    .collect();
                let base_edge_ids = self
                    .forward_base_slots_by_ordinal
                    .get(start..end)
                    .ok_or_else(|| {
                        GraphError::Message(
                            "rebalance window exceeds overlay base-slot mappings".into(),
                        )
                    })?
                    .iter()
                    .map(|slots| slots.iter().flatten().copied().collect())
                    .collect();
                (vertex_ids, base_edge_ids)
            }
            _ => (
                self.forward_node_ids()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                self.forward_live_base_edge_ids_by_ordinal(),
            ),
        };

        let summary = self
            .store
            .insert_edge_pair_with_local_rebalance_and_write(
                crate::low_level::RebalanceInsertSpec {
                    edge_id,
                    endpoints: crate::low_level::EdgePairEndpoints {
                        src_vertex_ref: src.into(),
                        src_ordinal: src_mapping.forward_ordinal,
                        dst_vertex_ref: dst.into(),
                        dst_ordinal: dst_mapping.reverse_ordinal,
                    },
                    edge_meta,
                    planned_incoming_live_entries: 1,
                    forward_rebalance_vertex_ids: &forward_rebalance_vertex_ids,
                    forward_rebalance_base_edge_ids_by_ordinal:
                        &forward_rebalance_base_edge_ids_by_ordinal,
                },
                self.memory,
            )
            .map_err(|err| GraphError::Message(err.to_string()))?;

        let refreshed = GraphPmaRefreshedVertices::new(
            summary.refreshed_forward_vertices.clone(),
            summary.refreshed_reverse_vertices.clone(),
        );
        let inserted = summary
            .insert
            .ok_or_else(|| GraphError::Message("edge insert produced no result".into()))?;
        let (path, locators) = match inserted {
            crate::GraphInsertResult::Inserted { path, locators } => (path, locators),
            crate::GraphInsertResult::RebalanceRequired(_) => {
                return Err(GraphError::Message(
                    "edge insert still requires rebalance after write helper".into(),
                ));
            }
        };
        if matches!(
            path,
            EdgeInsertPath::BaseAppend { .. } | EdgeInsertPath::BaseReuseTombstone { .. }
        ) {
            let Some(src_logical_index) = self.base_logical_index_from_path(
                path,
                src_mapping.forward_ordinal,
                locators.forward,
            ) else {
                return Err(GraphError::Message(
                    "failed to resolve forward base logical index".into(),
                ));
            };
            let Some(dst_logical_index) = self.base_logical_index_from_reverse_locator(
                dst.into(),
                dst_mapping.reverse_ordinal,
                locators.reverse,
            ) else {
                return Err(GraphError::Message(
                    "failed to resolve reverse base logical index".into(),
                ));
            };
            Self::set_base_slot(
                &mut self.forward_base_slots_by_ordinal[src_mapping.forward_ordinal],
                src_logical_index,
                edge_id,
            );
            Self::set_base_slot(
                &mut self.reverse_base_slots_by_ordinal[dst_mapping.reverse_ordinal],
                dst_logical_index,
                edge_id,
            );
        }
        self.edge_locators.insert(
            edge_id,
            GraphPmaEdgeLogicalLocatorMapping {
                edge_id,
                canonical: locators.forward,
                forward: locators.forward,
                reverse: locators.reverse,
            },
        );

        self.persist_edge_properties(edge_id, properties)?;
        let record = EdgeRecord {
            id: edge_id,
            src,
            dst,
            label: label.map(str::to_owned),
            properties: properties.clone(),
        };
        self.edges.insert(edge_id, record.clone());
        self.register_incident_edge(src, dst, edge_id);
        if bootstrap_event {
            self.record_edge_bootstrap_summary(GraphPmaOverlayEdgeBootstrapSummary {
                edge: record.clone(),
                path,
                refreshed,
            });
        } else {
            let (total_displacement, max_displacement) = summary
                .rebalance
                .as_ref()
                .map(|rebalance| {
                    (
                        rebalance.apply.total_displacement(),
                        rebalance.apply.max_displacement(),
                    )
                })
                .unwrap_or((0, 0));
            self.record_insert_edge_summary(GraphPmaOverlayInsertEdgeSummary {
                inserted: summary.insert.is_some(),
                path: Some(path),
                rebalanced: summary.rebalance.is_some(),
                total_displacement,
                max_displacement,
                refreshed,
            });
        }
        Ok(record)
    }
}
