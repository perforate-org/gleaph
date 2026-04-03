use super::*;

impl RewriteGraphPma {
    fn note_node_property_store_appended_from(&mut self, append_from: usize) {
        let append_from = u32::try_from(append_from).unwrap_or(u32::MAX);
        self.node_property_store_append_from = Some(
            self.node_property_store_append_from
                .map_or(append_from, |current| current.min(append_from)),
        );
    }

    fn note_edge_property_store_appended_from(&mut self, append_from: usize) {
        let append_from = u32::try_from(append_from).unwrap_or(u32::MAX);
        self.edge_property_store_append_from = Some(
            self.edge_property_store_append_from
                .map_or(append_from, |current| current.min(append_from)),
        );
    }

    pub(super) fn property_index_page_size_bytes(&self) -> u32 {
        const DEFAULT_PROPERTY_INDEX_PAGE_SIZE_BYTES: u32 = 4096;
        #[cfg(test)]
        if let Some(page) = property_index_page_size_test_hook::get() {
            return page;
        }
        DEFAULT_PROPERTY_INDEX_PAGE_SIZE_BYTES
    }

    fn try_sync_node_property_index_node_store(&mut self) -> Result<(), PropertyIndexError> {
        #[cfg(test)]
        if FAIL_NEXT_NODE_PROPERTY_INDEX_SYNC_TEST.swap(false, Ordering::SeqCst) {
            return Err(PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage);
        }
        let page_size_bytes = self.property_index_page_size_bytes();
        self.node_property_index_nodes =
            PropertyIndexNodeStore::try_from_index(&self.node_property_index, page_size_bytes)?;
        Ok(())
    }

    fn try_sync_edge_property_index_node_store(&mut self) -> Result<(), PropertyIndexError> {
        #[cfg(test)]
        if FAIL_NEXT_EDGE_PROPERTY_INDEX_SYNC_TEST.swap(false, Ordering::SeqCst) {
            return Err(PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage);
        }
        let page_size_bytes = self.property_index_page_size_bytes();
        self.edge_property_index_nodes =
            PropertyIndexNodeStore::try_from_index(&self.edge_property_index, page_size_bytes)?;
        Ok(())
    }

    pub(super) fn try_sync_property_index_node_stores(&mut self) -> Result<(), PropertyIndexError> {
        self.try_sync_node_property_index_node_store()?;
        self.try_sync_edge_property_index_node_store()?;
        Ok(())
    }

    fn rollback_node_property_store_after_failed_index_bind(
        &mut self,
        node_id: NodeId,
        property: &str,
        old_value: Option<&Value>,
    ) -> Result<(), PropertyStoreError> {
        match old_value {
            Some(previous) => {
                self.node_property_store
                    .set(PropertyKey::node(node_id, property), previous.clone())?;
                let _ =
                    self.insert_node_property_index_binding_with_kind(node_id, property, previous)?;
            }
            None => {
                self.node_property_store
                    .remove(PropertyKey::node(node_id, property))?;
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
                self.edge_property_store
                    .set(PropertyKey::edge(edge_id, property), previous.clone())?;
                let _ =
                    self.insert_edge_property_index_binding_with_kind(edge_id, property, previous)?;
            }
            None => {
                self.edge_property_store
                    .remove(PropertyKey::edge(edge_id, property))?;
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
        let old_len = self.node_property_store.records.len();
        let old_value = self
            .node_property_store
            .get_node_property(node_id, property);
        let _ = self.remove_node_property_index_binding_with_kind(node_id, property)?;
        self.node_property_store
            .set(PropertyKey::node(node_id, property), value.clone())?;
        match self.insert_node_property_index_binding_with_kind(node_id, property, value) {
            Ok(_) => {
                self.node_property_store_dirty = true;
                self.note_node_property_store_appended_from(old_len);
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
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError> {
        let old_len = self.node_property_store.records.len();
        let before = self.node_property_index_nodes.clone();
        let old_value = self
            .node_property_store
            .get_node_property(node_id, property);
        let mut node_store_operations = Vec::new();
        let mut fallback_reasons = Vec::new();
        if let Some(kind) = self.remove_node_property_index_binding_with_kind(node_id, property)? {
            if kind == PropertyIndexNodeStoreMutationKind::Rebuild {
                fallback_reasons.push(PropertyIndexFallbackReason::NodeRemoveLocalUnavailable);
                self.production_metrics.record_property_index_fallback(
                    PropertyIndexFallbackReason::NodeRemoveLocalUnavailable,
                );
            }
            node_store_operations.push(kind);
        }
        self.node_property_store
            .set(PropertyKey::node(node_id, property), value.clone())?;
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
        if insert_kind == PropertyIndexNodeStoreMutationKind::Rebuild {
            fallback_reasons.push(PropertyIndexFallbackReason::NodeUpsertLocalUnavailable);
            self.production_metrics.record_property_index_fallback(
                PropertyIndexFallbackReason::NodeUpsertLocalUnavailable,
            );
        }
        node_store_operations.push(insert_kind);
        self.node_property_store_dirty = true;
        self.note_node_property_store_appended_from(old_len);
        Ok(RewritePropertyIndexMutationSummary::from_delta(
            self.node_property_index_nodes.diff_against(&before),
            node_store_operations,
            fallback_reasons,
        ))
    }

    pub fn remove_node_property_value(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        let old_len = self.node_property_store.records.len();
        let _ = self.remove_node_property_index_binding_with_kind(node_id, property)?;
        self.node_property_store
            .remove(PropertyKey::node(node_id, property))?;
        self.node_property_store_dirty = true;
        self.note_node_property_store_appended_from(old_len);
        Ok(())
    }

    pub fn remove_node_property_value_with_summary(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError> {
        let old_len = self.node_property_store.records.len();
        let before = self.node_property_index_nodes.clone();
        let node_store_operations: Vec<_> = self
            .remove_node_property_index_binding_with_kind(node_id, property)?
            .into_iter()
            .collect();
        let fallback_reasons: Vec<_> = node_store_operations
            .iter()
            .filter_map(|kind| {
                (*kind == PropertyIndexNodeStoreMutationKind::Rebuild)
                    .then_some(PropertyIndexFallbackReason::NodeRemoveLocalUnavailable)
            })
            .collect();
        for reason in &fallback_reasons {
            self.production_metrics
                .record_property_index_fallback(*reason);
        }
        self.node_property_store
            .remove(PropertyKey::node(node_id, property))?;
        self.node_property_store_dirty = true;
        self.note_node_property_store_appended_from(old_len);
        Ok(RewritePropertyIndexMutationSummary::from_delta(
            self.node_property_index_nodes.diff_against(&before),
            node_store_operations,
            fallback_reasons,
        ))
    }

    pub fn set_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<(), PropertyStoreError> {
        let old_len = self.edge_property_store.records.len();
        let old_value = self
            .edge_property_store
            .get_edge_property(edge_id, property);
        let _ = self.remove_edge_property_index_binding_with_kind(edge_id, property)?;
        self.edge_property_store
            .set(PropertyKey::edge(edge_id, property), value.clone())?;
        match self.insert_edge_property_index_binding_with_kind(edge_id, property, value) {
            Ok(_) => {
                self.edge_property_store_dirty = true;
                self.note_edge_property_store_appended_from(old_len);
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
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError> {
        let old_len = self.edge_property_store.records.len();
        let before = self.edge_property_index_nodes.clone();
        let old_value = self
            .edge_property_store
            .get_edge_property(edge_id, property);
        let mut node_store_operations = Vec::new();
        let mut fallback_reasons = Vec::new();
        if let Some(kind) = self.remove_edge_property_index_binding_with_kind(edge_id, property)? {
            if kind == PropertyIndexNodeStoreMutationKind::Rebuild {
                fallback_reasons.push(PropertyIndexFallbackReason::EdgeRemoveLocalUnavailable);
                self.production_metrics.record_property_index_fallback(
                    PropertyIndexFallbackReason::EdgeRemoveLocalUnavailable,
                );
            }
            node_store_operations.push(kind);
        }
        self.edge_property_store
            .set(PropertyKey::edge(edge_id, property), value.clone())?;
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
        if insert_kind == PropertyIndexNodeStoreMutationKind::Rebuild {
            fallback_reasons.push(PropertyIndexFallbackReason::EdgeUpsertLocalUnavailable);
            self.production_metrics.record_property_index_fallback(
                PropertyIndexFallbackReason::EdgeUpsertLocalUnavailable,
            );
        }
        node_store_operations.push(insert_kind);
        self.edge_property_store_dirty = true;
        self.note_edge_property_store_appended_from(old_len);
        Ok(RewritePropertyIndexMutationSummary::from_delta(
            self.edge_property_index_nodes.diff_against(&before),
            node_store_operations,
            fallback_reasons,
        ))
    }

    pub fn remove_edge_property_value(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<(), PropertyStoreError> {
        let old_len = self.edge_property_store.records.len();
        let _ = self.remove_edge_property_index_binding_with_kind(edge_id, property)?;
        self.edge_property_store
            .remove(PropertyKey::edge(edge_id, property))?;
        self.edge_property_store_dirty = true;
        self.note_edge_property_store_appended_from(old_len);
        Ok(())
    }

    pub fn remove_edge_property_value_with_summary(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<RewritePropertyIndexMutationSummary, PropertyStoreError> {
        let old_len = self.edge_property_store.records.len();
        let before = self.edge_property_index_nodes.clone();
        let node_store_operations: Vec<_> = self
            .remove_edge_property_index_binding_with_kind(edge_id, property)?
            .into_iter()
            .collect();
        let fallback_reasons: Vec<_> = node_store_operations
            .iter()
            .filter_map(|kind| {
                (*kind == PropertyIndexNodeStoreMutationKind::Rebuild)
                    .then_some(PropertyIndexFallbackReason::EdgeRemoveLocalUnavailable)
            })
            .collect();
        for reason in &fallback_reasons {
            self.production_metrics
                .record_property_index_fallback(*reason);
        }
        self.edge_property_store
            .remove(PropertyKey::edge(edge_id, property))?;
        self.edge_property_store_dirty = true;
        self.note_edge_property_store_appended_from(old_len);
        Ok(RewritePropertyIndexMutationSummary::from_delta(
            self.edge_property_index_nodes.diff_against(&before),
            node_store_operations,
            fallback_reasons,
        ))
    }

    pub fn set_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        let mutation = self.set_node_property_value_with_summary(node_id, property, value)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewritePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(RewriteFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub fn remove_node_property_value_and_write(
        &mut self,
        node_id: NodeId,
        property: &str,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        let mutation = self.remove_node_property_value_with_summary(node_id, property)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewritePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(RewriteFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub fn set_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        let mutation = self.set_edge_property_value_with_summary(edge_id, property, value)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewritePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(RewriteFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub fn remove_edge_property_value_and_write(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewritePropertyMutationWriteSummary> {
        let mutation = self.remove_edge_property_value_with_summary(edge_id, property)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewritePropertyMutationWriteSummary::from_mutation_and_refresh(
            mutation,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        );
        self.record_write_event(RewriteFacadeWriteEvent::Property(summary.clone()));
        Ok(summary)
    }

    pub(super) fn rebuild_property_indices(&mut self) -> Result<(), PropertyStoreError> {
        self.node_property_index = PropertyIndex::new(64);
        self.edge_property_index = PropertyIndex::new(64);

        for (key, value) in self.node_property_store.latest_state() {
            if let Some(value) = value {
                let node_id = NodeId::try_from(key.entity_id)
                    .map_err(|_| PropertyStoreError::LengthOverflow)?;
                let _ = self.insert_node_property_index_binding_with_kind(
                    node_id,
                    &key.property_name,
                    &value,
                )?;
            }
        }

        for (key, value) in self.edge_property_store.latest_state() {
            if let Some(value) = value {
                let _ = self.insert_edge_property_index_binding_with_kind(
                    key.entity_id,
                    &key.property_name,
                    &value,
                )?;
            }
        }

        self.try_sync_property_index_node_stores()?;
        Ok(())
    }

    fn insert_node_property_index_binding_with_kind(
        &mut self,
        node_id: NodeId,
        property: &str,
        value: &Value,
    ) -> Result<(PropertyIndexKey, PropertyIndexNodeStoreMutationKind), PropertyIndexError> {
        let key = PropertyIndexKey::node(
            node_id,
            property,
            value
                .to_binary_bytes()
                .expect("Value must encode to binary bytes"),
        );
        self.node_property_index
            .insert(key.clone(), PropertyIndexEntry::empty());
        match self
            .node_property_index_nodes
            .upsert_leaf_chain_entry_with_kind(key.clone(), PropertyIndexEntry::empty())
        {
            Some(operation) => Ok((key, operation)),
            None => {
                if let Err(e) = self.try_sync_node_property_index_node_store() {
                    self.node_property_index.remove(&key);
                    return Err(e);
                }
                Ok((key, PropertyIndexNodeStoreMutationKind::Rebuild))
            }
        }
    }

    fn remove_node_property_index_binding_with_kind(
        &mut self,
        node_id: NodeId,
        property: &str,
    ) -> Result<Option<PropertyIndexNodeStoreMutationKind>, PropertyIndexError> {
        if let Some(old_value) = self
            .node_property_store
            .get_node_property(node_id, property)
        {
            let key = PropertyIndexKey::node(
                node_id,
                property,
                old_value
                    .to_binary_bytes()
                    .expect("Value must encode to binary bytes"),
            );
            self.node_property_index.remove(&key);
            return match self
                .node_property_index_nodes
                .remove_leaf_chain_entry_with_kind(&key)
            {
                Some(operation) => Ok(Some(operation)),
                None => {
                    if let Err(e) = self.try_sync_node_property_index_node_store() {
                        self.node_property_index
                            .insert(key.clone(), PropertyIndexEntry::empty());
                        return Err(e);
                    }
                    Ok(Some(PropertyIndexNodeStoreMutationKind::Rebuild))
                }
            };
        }
        Ok(None)
    }

    fn insert_edge_property_index_binding_with_kind(
        &mut self,
        edge_id: EdgeId,
        property: &str,
        value: &Value,
    ) -> Result<(PropertyIndexKey, PropertyIndexNodeStoreMutationKind), PropertyIndexError> {
        let key = PropertyIndexKey::edge(
            edge_id,
            property,
            value
                .to_binary_bytes()
                .expect("Value must encode to binary bytes"),
        );
        self.edge_property_index
            .insert(key.clone(), PropertyIndexEntry::empty());
        match self
            .edge_property_index_nodes
            .upsert_leaf_chain_entry_with_kind(key.clone(), PropertyIndexEntry::empty())
        {
            Some(operation) => Ok((key, operation)),
            None => {
                if let Err(e) = self.try_sync_edge_property_index_node_store() {
                    self.edge_property_index.remove(&key);
                    return Err(e);
                }
                Ok((key, PropertyIndexNodeStoreMutationKind::Rebuild))
            }
        }
    }

    fn remove_edge_property_index_binding_with_kind(
        &mut self,
        edge_id: EdgeId,
        property: &str,
    ) -> Result<Option<PropertyIndexNodeStoreMutationKind>, PropertyIndexError> {
        if let Some(old_value) = self
            .edge_property_store
            .get_edge_property(edge_id, property)
        {
            let key = PropertyIndexKey::edge(
                edge_id,
                property,
                old_value
                    .to_binary_bytes()
                    .expect("Value must encode to binary bytes"),
            );
            self.edge_property_index.remove(&key);
            return match self
                .edge_property_index_nodes
                .remove_leaf_chain_entry_with_kind(&key)
            {
                Some(operation) => Ok(Some(operation)),
                None => {
                    if let Err(e) = self.try_sync_edge_property_index_node_store() {
                        self.edge_property_index
                            .insert(key.clone(), PropertyIndexEntry::empty());
                        return Err(e);
                    }
                    Ok(Some(PropertyIndexNodeStoreMutationKind::Rebuild))
                }
            };
        }
        Ok(None)
    }
}
