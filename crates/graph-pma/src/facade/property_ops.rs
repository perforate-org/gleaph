use std::rc::Rc;

use super::*;

use gleaph_gql::Value;

use crate::property_index::{PropertyIndexNodeId, PropertyIndexNodeStoreDelta};
use crate::property_store::{
    PropertyKey, StoredPropertyValue, btree_get_edge_property, btree_get_node_property,
};

impl<M: Memory> GraphPma<M> {
    fn validate_property_key_name(property: &str) -> Result<(), PropertyStoreError> {
        gleaph_gql::name_limits::validate_property_name(property)
            .map_err(|e| PropertyStoreError::InvalidIdentifier(e.to_string()))
    }

    /// Whether an equality-index key changes for a SET, matching [`PropertyIndexKey`] binary encoding.
    fn property_equality_key_changes_on_set(old_value: Option<&Value>, new_value: &Value) -> bool {
        match old_value {
            None => true,
            Some(old) => {
                old.to_binary_bytes()
                    .expect("Value must encode to binary bytes")
                    != new_value
                        .to_binary_bytes()
                        .expect("Value must encode to binary bytes")
            }
        }
    }

    fn property_index_node_store_delta_if_equality_touched(
        equality_touched: bool,
    ) -> PropertyIndexNodeStoreDelta {
        if equality_touched {
            PropertyIndexNodeStoreDelta {
                touched_node_ids: vec![PropertyIndexNodeId(1)],
                allocated_node_ids: Vec::new(),
                freed_node_ids: Vec::new(),
            }
        } else {
            PropertyIndexNodeStoreDelta::empty()
        }
    }

    fn rollback_node_property_store_after_failed_index_bind(
        &mut self,
        node_id: NodeId,
        property: &str,
        old_value: Option<&Value>,
    ) -> Result<(), PropertyStoreError> {
        match old_value {
            Some(previous) => {
                self.node_property_store.insert(
                    PropertyKey::node(node_id, property),
                    StoredPropertyValue(previous.clone()),
                );
                let _ =
                    self.insert_node_property_index_binding_with_kind(node_id, property, previous)?;
            }
            None => {
                self.node_property_store
                    .remove(&PropertyKey::node(node_id, property));
            }
        }
        Ok(())
    }

    fn rollback_edge_property_store_after_failed_index_bind(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        old_value: Option<&Value>,
    ) -> Result<(), PropertyStoreError> {
        match old_value {
            Some(previous) => {
                self.edge_property_store.insert(
                    PropertyKey::edge(edge_id, property),
                    StoredPropertyValue(previous.clone()),
                );
                let _ =
                    self.insert_edge_property_index_binding_with_kind(edge_id, property, previous)?;
            }
            None => {
                self.edge_property_store
                    .remove(&PropertyKey::edge(edge_id, property));
            }
        }
        Ok(())
    }

    pub fn set_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let old_value = btree_get_node_property(&self.node_property_store, node_id, property);
        let _ = self.remove_node_property_index_binding_with_kind(node_id, property)?;
        self.node_property_store.insert(
            PropertyKey::node(node_id, property),
            StoredPropertyValue(value.clone()),
        );
        match self.insert_node_property_index_binding_with_kind(node_id, property, value) {
            Ok(_) => {
                self.node_property_store_dirty = true;
                Ok(())
            }
            Err(e) => {
                self.rollback_node_property_store_after_failed_index_bind(
                    node_id,
                    property,
                    old_value.as_ref(),
                )?;
                Err(e.into())
            }
        }
    }

    pub fn set_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let old_value = btree_get_node_property(&self.node_property_store, node_id, property);
        let equality_touched =
            Self::property_equality_key_changes_on_set(old_value.as_ref(), value);
        let mut node_store_operations = Vec::new();
        if equality_touched
            && let Some(kind) =
                self.remove_node_property_index_binding_with_kind(node_id, property)?
            {
                node_store_operations.push(kind);
            }
        self.node_property_store.insert(
            PropertyKey::node(node_id, property),
            StoredPropertyValue(value.clone()),
        );
        if equality_touched {
            let insert_kind =
                match self.insert_node_property_index_binding_with_kind(node_id, property, value) {
                    Ok((_key, kind)) => kind,
                    Err(e) => {
                        self.rollback_node_property_store_after_failed_index_bind(
                            node_id,
                            property,
                            old_value.as_ref(),
                        )?;
                        return Err(e.into());
                    }
                };
            node_store_operations.push(insert_kind);
        }
        self.node_property_store_dirty = true;
        let summary = GraphPmaPropertyIndexMutationSummary::from_delta(
            Self::property_index_node_store_delta_if_equality_touched(equality_touched),
            node_store_operations,
            Vec::new(),
        );
        Ok(summary)
    }

    pub fn remove_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let _ = self.remove_node_property_index_binding_with_kind(node_id, property)?;
        self.node_property_store
            .remove(&PropertyKey::node(node_id, property));
        self.node_property_store_dirty = true;
        Ok(())
    }

    pub fn remove_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let node_store_operations: Vec<_> = self
            .remove_node_property_index_binding_with_kind(node_id, property)?
            .into_iter()
            .collect();
        let equality_touched = !node_store_operations.is_empty();
        self.node_property_store
            .remove(&PropertyKey::node(node_id, property));
        self.node_property_store_dirty = true;
        let summary = GraphPmaPropertyIndexMutationSummary::from_delta(
            Self::property_index_node_store_delta_if_equality_touched(equality_touched),
            node_store_operations,
            Vec::new(),
        );
        Ok(summary)
    }

    pub fn set_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let old_value = btree_get_edge_property(&self.edge_property_store, edge_id, property);
        let _ = self.remove_edge_property_index_binding_with_kind(edge_id, property)?;
        self.edge_property_store.insert(
            PropertyKey::edge(edge_id, property),
            StoredPropertyValue(value.clone()),
        );
        match self.insert_edge_property_index_binding_with_kind(edge_id, property, value) {
            Ok(_) => {
                self.edge_property_store_dirty = true;
                Ok(())
            }
            Err(e) => {
                self.rollback_edge_property_store_after_failed_index_bind(
                    edge_id,
                    property,
                    old_value.as_ref(),
                )?;
                Err(e.into())
            }
        }
    }

    pub fn set_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let old_value = btree_get_edge_property(&self.edge_property_store, edge_id, property);
        let equality_touched =
            Self::property_equality_key_changes_on_set(old_value.as_ref(), value);
        let mut node_store_operations = Vec::new();
        if equality_touched
            && let Some(kind) =
                self.remove_edge_property_index_binding_with_kind(edge_id, property)?
            {
                node_store_operations.push(kind);
            }
        self.edge_property_store.insert(
            PropertyKey::edge(edge_id, property),
            StoredPropertyValue(value.clone()),
        );
        if equality_touched {
            let insert_kind =
                match self.insert_edge_property_index_binding_with_kind(edge_id, property, value) {
                    Ok((_key, kind)) => kind,
                    Err(e) => {
                        self.rollback_edge_property_store_after_failed_index_bind(
                            edge_id,
                            property,
                            old_value.as_ref(),
                        )?;
                        return Err(e.into());
                    }
                };
            node_store_operations.push(insert_kind);
        }
        self.edge_property_store_dirty = true;
        let summary = GraphPmaPropertyIndexMutationSummary::from_delta(
            Self::property_index_node_store_delta_if_equality_touched(equality_touched),
            node_store_operations,
            Vec::new(),
        );
        Ok(summary)
    }

    pub fn remove_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let _ = self.remove_edge_property_index_binding_with_kind(edge_id, property)?;
        self.edge_property_store
            .remove(&PropertyKey::edge(edge_id, property));
        self.edge_property_store_dirty = true;
        Ok(())
    }

    pub fn remove_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<GraphPmaPropertyIndexMutationSummary, PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let node_store_operations: Vec<_> = self
            .remove_edge_property_index_binding_with_kind(edge_id, property)?
            .into_iter()
            .collect();
        let equality_touched = !node_store_operations.is_empty();
        self.edge_property_store
            .remove(&PropertyKey::edge(edge_id, property));
        self.edge_property_store_dirty = true;
        let summary = GraphPmaPropertyIndexMutationSummary::from_delta(
            Self::property_index_node_store_delta_if_equality_touched(equality_touched),
            node_store_operations,
            Vec::new(),
        );
        Ok(summary)
    }

    pub fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        let mutation = self.set_node_property_value_with_summary(node_id, property, value)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphPmaPropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(GraphPmaFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        let mutation = self.remove_node_property_value_with_summary(node_id, property)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphPmaPropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(GraphPmaFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        let mutation = self.set_edge_property_value_with_summary(edge_id, property, value)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphPmaPropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(GraphPmaFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphPmaResult<GraphPmaPropertyMutationWriteSummary> {
        let mutation = self.remove_edge_property_value_with_summary(edge_id, property)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphPmaPropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(GraphPmaFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub(super) fn rebuild_property_indices(&mut self) -> Result<(), PropertyStoreError> {
        // Snapshot property stores before touching the PIDX btree: `clear_new` on the equality
        // map rewrites allocator pages in stable memory and can clobber neighboring bucket data
        // if run while other btrees still depend on those bytes.
        let node_entries: Vec<(PropertyKey, Value)> = self
            .node_property_store
            .iter()
            .filter_map(|e| {
                let key = e.key();
                (key.entity_kind == crate::PropertyEntityKind::Node)
                    .then_some((key.clone(), e.value().0.clone()))
            })
            .collect();
        let edge_entries: Vec<(PropertyKey, Value)> = self
            .edge_property_store
            .iter()
            .filter_map(|e| {
                let key = e.key();
                (key.entity_kind == crate::PropertyEntityKind::Edge)
                    .then_some((key.clone(), e.value().0.clone()))
            })
            .collect();

        self.node_property_index = PropertyIndex::new(64);
        self.edge_property_index = PropertyIndex::new(64);
        // Match `hydrate_from_stable_memory`: do not re-init the pixmap btree with payload len 0
        // when the region already holds a BTR (`BTreeMap::new` would trash stable memory).
        let btree_rc = Rc::clone(&self.property_index_btree_payload);
        self.property_equality_map = crate::property_index::hydrate_property_equality_inplace_map(
            Rc::clone(&self.manager),
            Rc::clone(&self.memory),
            Rc::clone(&btree_rc),
        );
        self.property_equality_map.clear_new();
        for (key, v) in node_entries {
            let node_id =
                NodeId::try_from(key.entity_id).map_err(|_| PropertyStoreError::LengthOverflow)?;
            let _ =
                self.insert_node_property_index_binding_with_kind(node_id, &key.property_name, &v)?;
        }

        for (key, v) in edge_entries {
            let _ = self.insert_edge_property_index_binding_with_kind(
                key.entity_id,
                &key.property_name,
                &v,
            )?;
        }

        self.property_index_dirty = true;
        Ok(())
    }

    fn insert_node_property_index_binding_with_kind(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<(PropertyIndexKey, PropertyIndexNodeStoreMutationKind), PropertyIndexError> {
        #[cfg(test)]
        if FAIL_NEXT_NODE_PROPERTY_INDEX_SYNC_TEST.swap(false, Ordering::SeqCst) {
            return Err(PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage);
        }
        let key = PropertyIndexKey::node(
            node_id,
            property,
            value
                .to_binary_bytes()
                .expect("Value must encode to binary bytes"),
        );
        self.node_property_index
            .insert(key.clone(), PropertyIndexEntry::empty());
        self.property_equality_map
            .insert(key.clone(), PropertyIndexEntry::empty());
        self.property_index_dirty = true;
        Ok((key, PropertyIndexNodeStoreMutationKind::LocalUpdate))
    }

    fn remove_node_property_index_binding_with_kind(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<Option<PropertyIndexNodeStoreMutationKind>, PropertyIndexError> {
        #[cfg(test)]
        if FAIL_NEXT_NODE_PROPERTY_INDEX_SYNC_TEST.swap(false, Ordering::SeqCst) {
            return Err(PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage);
        }
        if let Some(old_value) =
            btree_get_node_property(&self.node_property_store, node_id, property)
        {
            let key = PropertyIndexKey::node(
                node_id,
                property,
                old_value
                    .to_binary_bytes()
                    .expect("Value must encode to binary bytes"),
            );
            self.node_property_index.remove(&key);
            self.property_equality_map.remove(&key);
            self.property_index_dirty = true;
            return Ok(Some(PropertyIndexNodeStoreMutationKind::Collapse));
        }
        Ok(None)
    }

    fn insert_edge_property_index_binding_with_kind(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<(PropertyIndexKey, PropertyIndexNodeStoreMutationKind), PropertyIndexError> {
        #[cfg(test)]
        if FAIL_NEXT_EDGE_PROPERTY_INDEX_SYNC_TEST.swap(false, Ordering::SeqCst) {
            return Err(PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage);
        }
        let key = PropertyIndexKey::edge(
            edge_id,
            property,
            value
                .to_binary_bytes()
                .expect("Value must encode to binary bytes"),
        );
        self.edge_property_index
            .insert(key.clone(), PropertyIndexEntry::empty());
        self.property_equality_map
            .insert(key.clone(), PropertyIndexEntry::empty());
        self.property_index_dirty = true;
        Ok((key, PropertyIndexNodeStoreMutationKind::LocalUpdate))
    }

    fn remove_edge_property_index_binding_with_kind(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<Option<PropertyIndexNodeStoreMutationKind>, PropertyIndexError> {
        #[cfg(test)]
        if FAIL_NEXT_EDGE_PROPERTY_INDEX_SYNC_TEST.swap(false, Ordering::SeqCst) {
            return Err(PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage);
        }
        if let Some(old_value) =
            btree_get_edge_property(&self.edge_property_store, edge_id, property)
        {
            let key = PropertyIndexKey::edge(
                edge_id,
                property,
                old_value
                    .to_binary_bytes()
                    .expect("Value must encode to binary bytes"),
            );
            self.edge_property_index.remove(&key);
            self.property_equality_map.remove(&key);
            self.property_index_dirty = true;
            return Ok(Some(PropertyIndexNodeStoreMutationKind::Collapse));
        }
        Ok(None)
    }
}
