use gleaph_gql::Value;
use gleaph_graph_kernel::{
    EdgeId, EdgeRecord, GraphError, GraphResult, GraphWrite, NodeId, NodeRecord, PropertyMap,
};

use crate::facade::RewritePropertyMutationWriteSummary;
use crate::low_level::GraphMutationPath;

use super::{
    RewriteKernelOverlayGraph, RewriteOverlayEdgeMutationKind, RewriteOverlayEdgeWriteSummary,
    RewriteOverlayNodeDeleteSummary,
};

impl<'a, S: super::RewriteGraphStore, M: super::Memory> RewriteKernelOverlayGraph<'a, S, M> {
    fn delete_edge_without_flush(&mut self, edge_id: EdgeId) -> GraphResult<()> {
        let edge = self
            .bridge
            .edges
            .get(&edge_id)
            .cloned()
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        self.bridge.remove_persisted_edge_properties(edge_id)?;
        let src_mapping = self
            .bridge
            .vertex_mapping(edge.src)
            .ok_or(GraphError::NodeNotFound(edge.src))?;
        let dst_mapping = self
            .bridge
            .vertex_mapping(edge.dst)
            .ok_or(GraphError::NodeNotFound(edge.dst))?;
        let (forward_loc, reverse_loc) = self
            .bridge
            .edge_locators
            .get(&edge_id)
            .map(|m| (m.forward, m.reverse))
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        let path = self
            .bridge
            .store
            .tombstone_edge_pair_and_write(
                crate::low_level::EdgeTombstoneSpec {
                    edge_id,
                    endpoints: crate::low_level::EdgePairEndpoints {
                        src_vertex_ref: edge.src.into(),
                        src_ordinal: src_mapping.forward_ordinal,
                        dst_vertex_ref: edge.dst.into(),
                        dst_ordinal: dst_mapping.reverse_ordinal,
                    },
                    locators: crate::low_level::EdgePairLogicalLocators {
                        forward: forward_loc,
                        reverse: reverse_loc,
                    },
                },
                self.bridge.memory,
            )
            .map_err(|err| GraphError::Message(err.to_string()))?;
        self.bridge
            .record_edge_write_summary(RewriteOverlayEdgeWriteSummary {
                operation: RewriteOverlayEdgeMutationKind::Delete,
                path: path.mutation,
                refreshed: path.refreshed,
            });
        let path = path.mutation;
        if matches!(path, GraphMutationPath::Base) {
            if let Some(index) = Self::edge_base_logical_index(
                &self.bridge.forward_base_slots_by_ordinal[src_mapping.forward_ordinal],
                edge_id,
            ) {
                self.bridge.forward_base_slots_by_ordinal[src_mapping.forward_ordinal][index] =
                    None;
            }
            if let Some(index) = Self::edge_base_logical_index(
                &self.bridge.reverse_base_slots_by_ordinal[dst_mapping.reverse_ordinal],
                edge_id,
            ) {
                self.bridge.reverse_base_slots_by_ordinal[dst_mapping.reverse_ordinal][index] =
                    None;
            }
        }
        self.bridge.edge_locators.remove(&edge_id);
        self.bridge
            .unregister_incident_edge(edge.src, edge.dst, edge_id);
        self.bridge
            .edges
            .remove(&edge_id)
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        Ok(())
    }
}

impl<'a, S: super::RewriteGraphStore, M: super::Memory> GraphWrite
    for RewriteKernelOverlayGraph<'a, S, M>
{
    fn insert_node(
        &mut self,
        labels: &[String],
        properties: &PropertyMap,
    ) -> GraphResult<NodeRecord> {
        self.bridge.bootstrap_node(labels, properties)
    }

    fn insert_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        label: Option<&str>,
        properties: &PropertyMap,
    ) -> GraphResult<EdgeRecord> {
        self.bridge.insert_edge(src, dst, label, properties)
    }

    fn set_node_property(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<NodeRecord> {
        if !self.bridge.nodes.contains_key(&node_id) {
            return Err(GraphError::NodeNotFound(node_id));
        }
        let mutation = self
            .bridge
            .store
            .set_node_property_value_with_summary(node_id, property, value)
            .map_err(super::graph_error_from_property_store)?;
        self.bridge.record_property_write_summary(
            RewritePropertyMutationWriteSummary::pending_from_mutation(mutation),
        );
        let node = self
            .bridge
            .nodes
            .get_mut(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?;
        node.properties.insert(property.to_owned(), value.clone());
        Ok(node.clone())
    }

    fn remove_node_property(&mut self, node_id: NodeId, property: &str) -> GraphResult<NodeRecord> {
        if !self.bridge.nodes.contains_key(&node_id) {
            return Err(GraphError::NodeNotFound(node_id));
        }
        let mutation = self
            .bridge
            .store
            .remove_node_property_value_with_summary(node_id, property)
            .map_err(super::graph_error_from_property_store)?;
        self.bridge.record_property_write_summary(
            RewritePropertyMutationWriteSummary::pending_from_mutation(mutation),
        );
        let node = self
            .bridge
            .nodes
            .get_mut(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?;
        node.properties.remove(property);
        Ok(node.clone())
    }

    fn add_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord> {
        let mut should_add = false;
        let node = self
            .bridge
            .nodes
            .get_mut(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?;
        if !node.labels.iter().any(|existing| existing == label) {
            node.labels.push(label.to_owned());
            should_add = true;
        }
        let node = node.clone();
        if should_add {
            self.bridge
                .sync_node_labels_to_index(node_id, &[label.to_owned()]);
        }
        Ok(node)
    }

    fn remove_node_label(&mut self, node_id: NodeId, label: &str) -> GraphResult<NodeRecord> {
        let node = self
            .bridge
            .nodes
            .get_mut(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?;
        let before = node.labels.len();
        node.labels.retain(|existing| existing != label);
        let removed = node.labels.len() != before;
        let node = node.clone();
        if removed {
            self.bridge
                .remove_node_labels_from_index(node_id, &[label.to_owned()]);
        }
        Ok(node)
    }

    fn set_edge_property(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> GraphResult<EdgeRecord> {
        if !self.bridge.edges.contains_key(&edge_id) {
            return Err(GraphError::EdgeNotFound(edge_id));
        }
        let mutation = self
            .bridge
            .store
            .set_edge_property_value_with_summary(edge_id, property, value)
            .map_err(super::graph_error_from_property_store)?;
        self.bridge.record_property_write_summary(
            RewritePropertyMutationWriteSummary::pending_from_mutation(mutation),
        );
        let edge = self
            .bridge
            .edges
            .get_mut(&edge_id)
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        edge.properties.insert(property.to_owned(), value.clone());
        Ok(edge.clone())
    }

    fn remove_edge_property(&mut self, edge_id: EdgeId, property: &str) -> GraphResult<EdgeRecord> {
        if !self.bridge.edges.contains_key(&edge_id) {
            return Err(GraphError::EdgeNotFound(edge_id));
        }
        let mutation = self
            .bridge
            .store
            .remove_edge_property_value_with_summary(edge_id, property)
            .map_err(super::graph_error_from_property_store)?;
        self.bridge.record_property_write_summary(
            RewritePropertyMutationWriteSummary::pending_from_mutation(mutation),
        );
        let edge = self
            .bridge
            .edges
            .get_mut(&edge_id)
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        edge.properties.remove(property);
        Ok(edge.clone())
    }

    fn set_edge_label(&mut self, edge_id: EdgeId, label: Option<&str>) -> GraphResult<EdgeRecord> {
        let edge = self
            .bridge
            .edges
            .get(&edge_id)
            .cloned()
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        let src_mapping = self
            .bridge
            .vertex_mapping(edge.src)
            .ok_or(GraphError::NodeNotFound(edge.src))?;
        let dst_mapping = self
            .bridge
            .vertex_mapping(edge.dst)
            .ok_or(GraphError::NodeNotFound(edge.dst))?;
        let (forward_loc, reverse_loc) = self
            .bridge
            .edge_locators
            .get(&edge_id)
            .map(|m| (m.forward, m.reverse))
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        let label_id = self.bridge.label_id_for(label);
        self.bridge
            .store
            .replace_edge_pair_and_write(
                crate::low_level::EdgeReplaceSpec {
                    edge_id,
                    endpoints: crate::low_level::EdgePairEndpoints {
                        src_vertex_ref: edge.src.into(),
                        src_ordinal: src_mapping.forward_ordinal,
                        dst_vertex_ref: edge.dst.into(),
                        dst_ordinal: dst_mapping.reverse_ordinal,
                    },
                    locators: crate::low_level::EdgePairLogicalLocators {
                        forward: forward_loc,
                        reverse: reverse_loc,
                    },
                    label_id,
                },
                self.bridge.memory,
            )
            .map_err(|err| GraphError::Message(err.to_string()))
            .map(|summary| {
                self.bridge
                    .record_edge_write_summary(RewriteOverlayEdgeWriteSummary {
                        operation: RewriteOverlayEdgeMutationKind::ReplaceLabel,
                        path: summary.mutation.0,
                        refreshed: summary.refreshed,
                    });
            })?;
        let edge = self
            .bridge
            .edges
            .get_mut(&edge_id)
            .ok_or(GraphError::EdgeNotFound(edge_id))?;
        edge.label = label.map(str::to_owned);
        Ok(edge.clone())
    }

    fn delete_edge(&mut self, edge_id: EdgeId) -> GraphResult<()> {
        self.delete_edge_without_flush(edge_id)?;
        GraphWrite::flush(self)
    }

    fn delete_node(&mut self, node_id: NodeId, detach: bool) -> GraphResult<()> {
        let incident_edge_ids: Vec<EdgeId> = self
            .bridge
            .edges
            .values()
            .filter(|edge| edge.src == node_id || edge.dst == node_id)
            .map(|edge| edge.id)
            .collect();
        if !incident_edge_ids.is_empty() && !detach {
            return Err(GraphError::Message("node has incident edges".into()));
        }
        let mut edge_writes = Vec::new();
        if detach {
            for edge_id in incident_edge_ids.iter().copied() {
                self.delete_edge_without_flush(edge_id)?;
                if let Some(summary) = self.bridge.last_edge_write_summary().cloned() {
                    edge_writes.push(summary);
                }
            }
        }
        self.bridge.remove_persisted_node_properties(node_id)?;
        let removed_node = self
            .bridge
            .nodes
            .remove(&node_id)
            .ok_or(GraphError::NodeNotFound(node_id))?;
        if let Some(mapping) = self.bridge.vertex_ordinal_by_node_id.get(&node_id).copied() {
            if let Some(slot) = self
                .bridge
                .semantic_node_id_by_forward_ordinal
                .get_mut(mapping.forward_ordinal)
            {
                *slot = None;
            }
            for label in &removed_node.labels {
                if let Some(label_id) = self.bridge.lookup_label_id(label) {
                    self.bridge
                        .vertex_label_index
                        .remove(label_id, mapping.forward_ordinal);
                }
            }
            self.bridge.enqueue_vertex_reclaim(mapping.forward_ordinal);
            let _ = self.bridge.persist_vertex_label_index();
        }
        self.bridge.vertex_ordinal_by_node_id.remove(&node_id);
        self.bridge
            .record_node_delete_summary(RewriteOverlayNodeDeleteSummary {
                detached: detach,
                deleted_edge_ids: incident_edge_ids,
                edge_writes,
            });
        GraphWrite::flush(self)
    }

    fn flush(&mut self) -> GraphResult<()> {
        let (fwd, rev) = self
            .bridge
            .store
            .try_refresh_and_write_dirty_to_stable_memory(self.bridge.memory)
            .map_err(|e| GraphError::Message(e.to_string()))?;
        self.bridge.patch_pending_property_summaries_after_stable_flush(
            crate::facade::RewriteRefreshedVertices::new(fwd, rev),
        );
        Ok(())
    }
}
