use super::*;

use gleaph_gql::Value;

use crate::property_index::{PropertyIndexNodeId, PropertyIndexNodeStoreDelta};
use crate::property_store::{
    PropertyKey, StoredPropertyValue, btree_get_edge_property, btree_get_node_property,
};

impl<M: Memory> GraphStore<M> {
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
        let mut node_property_store = self.open_fixed_slot_node_property_store();
        match old_value {
            Some(previous) => {
                node_property_store.insert(
                    PropertyKey::node(node_id, property),
                    StoredPropertyValue(previous.clone()),
                );
                self.node_property_store.insert(
                    PropertyKey::node(node_id, property),
                    StoredPropertyValue(previous.clone()),
                );
                let _ =
                    self.insert_node_property_index_binding_with_kind(node_id, property, previous)?;
            }
            None => {
                node_property_store.remove(&PropertyKey::node(node_id, property));
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
        let mut edge_property_store = self.open_fixed_slot_edge_property_store();
        match old_value {
            Some(previous) => {
                edge_property_store.insert(
                    PropertyKey::edge(edge_id, property),
                    StoredPropertyValue(previous.clone()),
                );
                self.edge_property_store.insert(
                    PropertyKey::edge(edge_id, property),
                    StoredPropertyValue(previous.clone()),
                );
                let _ =
                    self.insert_edge_property_index_binding_with_kind(edge_id, property, previous)?;
            }
            None => {
                edge_property_store.remove(&PropertyKey::edge(edge_id, property));
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
        let old_value = {
            let node_property_store = self.open_fixed_slot_node_property_store();
            btree_get_node_property(&node_property_store, node_id, property)
        };
        let _ = self.remove_node_property_index_binding_with_kind(node_id, property)?;
        let mut node_property_store = self.open_fixed_slot_node_property_store();
        node_property_store.insert(
            PropertyKey::node(node_id, property),
            StoredPropertyValue(value.clone()),
        );
        self.node_property_store.insert(
            PropertyKey::node(node_id, property),
            StoredPropertyValue(value.clone()),
        );
        match self.insert_node_property_index_binding_with_kind(node_id, property, value) {
            Ok(_) => Ok(()),
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
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let old_value = {
            let node_property_store = self.open_fixed_slot_node_property_store();
            btree_get_node_property(&node_property_store, node_id, property)
        };
        let equality_touched =
            Self::property_equality_key_changes_on_set(old_value.as_ref(), value);
        let mut node_store_operations = Vec::new();
        if equality_touched
            && let Some(kind) =
                self.remove_node_property_index_binding_with_kind(node_id, property)?
        {
            node_store_operations.push(kind);
        }
        let mut node_property_store = self.open_fixed_slot_node_property_store();
        node_property_store.insert(
            PropertyKey::node(node_id, property),
            StoredPropertyValue(value.clone()),
        );
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
        let summary = GraphStorePropertyIndexMutationSummary::from_delta(
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
        let mut node_property_store = self.open_fixed_slot_node_property_store();
        node_property_store.remove(&PropertyKey::node(node_id, property));
        self.node_property_store
            .remove(&PropertyKey::node(node_id, property));
        Ok(())
    }

    pub fn remove_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let node_store_operations: Vec<_> = self
            .remove_node_property_index_binding_with_kind(node_id, property)?
            .into_iter()
            .collect();
        let equality_touched = !node_store_operations.is_empty();
        let mut node_property_store = self.open_fixed_slot_node_property_store();
        node_property_store.remove(&PropertyKey::node(node_id, property));
        self.node_property_store
            .remove(&PropertyKey::node(node_id, property));
        let summary = GraphStorePropertyIndexMutationSummary::from_delta(
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
        let old_value = {
            let edge_property_store = self.open_fixed_slot_edge_property_store();
            btree_get_edge_property(&edge_property_store, edge_id, property)
        };
        let _ = self.remove_edge_property_index_binding_with_kind(edge_id, property)?;
        let mut edge_property_store = self.open_fixed_slot_edge_property_store();
        edge_property_store.insert(
            PropertyKey::edge(edge_id, property),
            StoredPropertyValue(value.clone()),
        );
        self.edge_property_store.insert(
            PropertyKey::edge(edge_id, property),
            StoredPropertyValue(value.clone()),
        );
        match self.insert_edge_property_index_binding_with_kind(edge_id, property, value) {
            Ok(_) => Ok(()),
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
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let old_value = {
            let edge_property_store = self.open_fixed_slot_edge_property_store();
            btree_get_edge_property(&edge_property_store, edge_id, property)
        };
        let equality_touched =
            Self::property_equality_key_changes_on_set(old_value.as_ref(), value);
        let mut node_store_operations = Vec::new();
        if equality_touched
            && let Some(kind) =
                self.remove_edge_property_index_binding_with_kind(edge_id, property)?
        {
            node_store_operations.push(kind);
        }
        let mut edge_property_store = self.open_fixed_slot_edge_property_store();
        edge_property_store.insert(
            PropertyKey::edge(edge_id, property),
            StoredPropertyValue(value.clone()),
        );
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
        let summary = GraphStorePropertyIndexMutationSummary::from_delta(
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
        let mut edge_property_store = self.open_fixed_slot_edge_property_store();
        edge_property_store.remove(&PropertyKey::edge(edge_id, property));
        self.edge_property_store
            .remove(&PropertyKey::edge(edge_id, property));
        Ok(())
    }

    pub fn remove_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<GraphStorePropertyIndexMutationSummary, PropertyStoreError> {
        Self::validate_property_key_name(property)?;
        let node_store_operations: Vec<_> = self
            .remove_edge_property_index_binding_with_kind(edge_id, property)?
            .into_iter()
            .collect();
        let equality_touched = !node_store_operations.is_empty();
        let mut edge_property_store = self.open_fixed_slot_edge_property_store();
        edge_property_store.remove(&PropertyKey::edge(edge_id, property));
        self.edge_property_store
            .remove(&PropertyKey::edge(edge_id, property));
        let summary = GraphStorePropertyIndexMutationSummary::from_delta(
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
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        let mutation = self.set_node_property_value_with_summary(node_id, property, value)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphStorePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(GraphStoreFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        let mutation = self.remove_node_property_value_with_summary(node_id, property)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphStorePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(GraphStoreFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        let mutation = self.set_edge_property_value_with_summary(edge_id, property, value)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphStorePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(GraphStoreFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStorePropertyMutationWriteSummary> {
        let mutation = self.remove_edge_property_value_with_summary(edge_id, property)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphStorePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(GraphStoreFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub(super) fn rebuild_property_indices(&mut self) -> Result<(), PropertyStoreError> {
        let node_entries: Vec<(PropertyKey, Value)> = {
            let node_property_store = self.open_fixed_slot_node_property_store();
            node_property_store
                .iter()
                .filter_map(|e| {
                    let key = e.key();
                    (key.entity_kind == crate::PropertyEntityKind::Node)
                        .then_some((key.clone(), e.value().0.clone()))
                })
                .collect()
        };
        let edge_entries: Vec<(PropertyKey, Value)> = {
            let edge_property_store = self.open_fixed_slot_edge_property_store();
            edge_property_store
                .iter()
                .filter_map(|e| {
                    let key = e.key();
                    (key.entity_kind == crate::PropertyEntityKind::Edge)
                        .then_some((key.clone(), e.value().0.clone()))
                })
                .collect()
        };
        {
            let mut property_equality_map = self.open_fixed_slot_property_equality_map();
            let existing_keys: Vec<_> = property_equality_map
                .iter()
                .map(|entry| entry.key().clone())
                .collect();
            for key in existing_keys {
                property_equality_map.remove(&key);
            }
        }
        let existing_shadow_keys: Vec<_> = self
            .property_equality_map
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for key in existing_shadow_keys {
            self.property_equality_map.remove(&key);
        }

        self.node_property_index = PropertyIndex::new(64);
        self.edge_property_index = PropertyIndex::new(64);
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

        self.property_index_dirty = false;
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
        let mut property_equality_map = self.open_fixed_slot_property_equality_map();
        property_equality_map.insert(key.clone(), PropertyIndexEntry::empty());
        self.property_index_dirty = false;
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
        let node_property_store = self.open_fixed_slot_node_property_store();
        if let Some(old_value) = btree_get_node_property(&node_property_store, node_id, property) {
            let key = PropertyIndexKey::node(
                node_id,
                property,
                old_value
                    .to_binary_bytes()
                    .expect("Value must encode to binary bytes"),
            );
            self.node_property_index.remove(&key);
            self.property_equality_map.remove(&key);
            let mut property_equality_map = self.open_fixed_slot_property_equality_map();
            property_equality_map.remove(&key);
            self.property_index_dirty = false;
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
        let mut property_equality_map = self.open_fixed_slot_property_equality_map();
        property_equality_map.insert(key.clone(), PropertyIndexEntry::empty());
        self.property_index_dirty = false;
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
        let edge_property_store = self.open_fixed_slot_edge_property_store();
        if let Some(old_value) = btree_get_edge_property(&edge_property_store, edge_id, property) {
            let key = PropertyIndexKey::edge(
                edge_id,
                property,
                old_value
                    .to_binary_bytes()
                    .expect("Value must encode to binary bytes"),
            );
            self.edge_property_index.remove(&key);
            self.property_equality_map.remove(&key);
            let mut property_equality_map = self.open_fixed_slot_property_equality_map();
            property_equality_map.remove(&key);
            self.property_index_dirty = false;
            return Ok(Some(PropertyIndexNodeStoreMutationKind::Collapse));
        }
        Ok(None)
    }
}
