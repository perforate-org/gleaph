use crate::integration::RewriteKernelOverlayGraph;

use super::*;

impl<M: Memory> RewriteGraphPma<M> {
    fn persist_maintenance_queue(&mut self, memory: &impl Memory) -> Result<u64, WritebackError> {
        let persisted_bytes = self.write_maintenance_queue_to_stable_memory(memory)?;
        self.production_metrics.record_maintenance_queue_write(
            persisted_bytes,
            Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION,
        );
        Ok(persisted_bytes)
    }

    fn current_maintenance_queue_storage_snapshot(
        &self,
        memory: &impl Memory,
    ) -> Result<crate::low_level::GraphMaintenanceQueueStorageSnapshot, WritebackError> {
        let projection = self
            .try_read_maintenance_queue_storage_projection_from_stable_memory(memory)
            .map_err(|_| WritebackError::MissingRegionDefinition(RegionKind::MaintenanceQueue))?;
        Ok(Self::maintenance_queue_storage_snapshot_from_projection(
            projection,
        ))
    }

    /// Writes the full forward/reverse runtime state back to stable memory.
    ///
    /// The property-index region is written as PIDX v3: a header plus serialized
    /// [`PropertyEqualityStableMap`] (see [`PropertyIndexStorageImage`]).
    pub fn write_all_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<()> {
        let runtimes =
            HydratedSurfaceRuntimes::new(self.graph.forward.clone(), self.graph.reverse.clone());
        write_surface_runtimes_to_stable_memory(
            &mut self.manager.borrow_mut(),
            memory,
            &runtimes,
        )?;
        crate::property_store::write_graph_property_stable_map_to_stable_memory(
            &mut self.manager.borrow_mut(),
            memory,
            RegionKind::NodePropertyStore,
            &self.node_property_btree_payload,
            true,
        )?;
        crate::property_store::write_graph_property_stable_map_to_stable_memory(
            &mut self.manager.borrow_mut(),
            memory,
            RegionKind::EdgePropertyStore,
            &self.edge_property_btree_payload,
            true,
        )?;
        write_property_index_stable_equality_to_stable_memory(
            &mut self.manager.borrow_mut(),
            memory,
            &self.property_index_btree_payload,
            self.property_index_dirty,
        )?;
        self.property_index_dirty = false;
        self.persist_maintenance_queue(memory)?;
        self.node_property_store_dirty = false;
        self.edge_property_store_dirty = false;
        Ok(())
    }

    pub fn try_write_all_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<()> {
        self.write_all_to_stable_memory(memory)
    }

    pub fn refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<(Vec<usize>, Vec<usize>)> {
        let property_store_was_dirty =
            self.node_property_store_dirty || self.edge_property_store_dirty;
        let refreshed = {
            let _p = crate::bench_profile::PhaseGuard::new("facade_low_level_graph_refresh_write");
            crate::canbench_scope::scope("pma_graph_refresh_write");
            self.graph
                .refresh_and_write_dirty_to_stable_memory(&mut self.manager.borrow_mut(), memory)?
        };
        if self.node_property_store_dirty {
            let _p = crate::bench_profile::PhaseGuard::new("facade_flush_node_property_region");
            crate::canbench_scope::scope("pma_node_property_store_flush");
            crate::property_store::write_graph_property_stable_map_to_stable_memory(
                &mut self.manager.borrow_mut(),
                memory,
                RegionKind::NodePropertyStore,
                &self.node_property_btree_payload,
                true,
            )?;
            self.node_property_store_dirty = false;
        }
        if self.edge_property_store_dirty {
            let _p = crate::bench_profile::PhaseGuard::new("facade_flush_edge_property_region");
            crate::canbench_scope::scope("pma_edge_property_store_flush");
            crate::property_store::write_graph_property_stable_map_to_stable_memory(
                &mut self.manager.borrow_mut(),
                memory,
                RegionKind::EdgePropertyStore,
                &self.edge_property_btree_payload,
                true,
            )?;
            self.edge_property_store_dirty = false;
        }
        if !property_store_was_dirty {
            let _p = crate::bench_profile::PhaseGuard::new("facade_maint_queue_only");
            crate::canbench_scope::scope("pma_maint_queue_persist");
            self.persist_maintenance_queue(memory)?;
            return Ok(refreshed);
        }
        {
            let _p = crate::bench_profile::PhaseGuard::new("facade_property_index_full_pipeline");
            {
                crate::canbench_scope::scope("pma_property_index_paged_flush");
                write_property_index_stable_equality_to_stable_memory(
                    &mut self.manager.borrow_mut(),
                    memory,
                    &self.property_index_btree_payload,
                    self.property_index_dirty,
                )?;
            }
            self.property_index_dirty = false;
            crate::canbench_scope::scope("pma_maint_queue_persist");
            self.persist_maintenance_queue(memory)?;
        }
        Ok(refreshed)
    }

    pub fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<(Vec<usize>, Vec<usize>)> {
        self.refresh_and_write_dirty_to_stable_memory(memory)
    }

    pub fn append_empty_vertex_pair(&mut self) -> RewriteGraphPmaResult<(usize, usize)> {
        self.graph
            .append_empty_vertex_pair()
            .ok_or(RewriteGraphPmaError::InvalidLocatorInputs)
    }

    pub fn append_empty_vertex_pairs(
        &mut self,
        count: usize,
    ) -> RewriteGraphPmaResult<Vec<(usize, usize)>> {
        self.graph
            .append_empty_vertex_pairs(count)
            .ok_or(RewriteGraphPmaError::InvalidLocatorInputs)
    }

    pub fn append_empty_vertex_pair_and_write(
        &mut self,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteAppendVertexWriteSummary> {
        let ordinals = self.append_empty_vertex_pair()?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewriteAppendVertexWriteSummary {
            ordinals,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::AppendVertex(summary.clone()));
        Ok(summary)
    }

    pub fn append_empty_vertex_pairs_and_write(
        &mut self,
        count: usize,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteAppendVerticesWriteSummary> {
        let ordinals = self.append_empty_vertex_pairs(count)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewriteAppendVerticesWriteSummary {
            ordinals,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::AppendVertices(summary.clone()));
        Ok(summary)
    }

    pub fn bootstrap_edge_between_new_vertices_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        dst_vertex: NodeId,
        label_id: LabelId,
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteBootstrapEdgeWriteSummary> {
        let (src_ordinal, _) = self.append_empty_vertex_pair()?;
        let (dst_ordinal, _) = self.append_empty_vertex_pair()?;
        let insert = self
            .graph
            .insert_edge_pair(
                edge_id,
                src_vertex.into(),
                src_ordinal,
                dst_vertex.into(),
                dst_ordinal,
                label_id,
            )
            .ok_or(RewriteGraphPmaError::InvalidLocatorInputs)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewriteBootstrapEdgeWriteSummary {
            ordinals: (src_ordinal, dst_ordinal),
            insert,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::BootstrapEdge(summary.clone()));
        Ok(summary)
    }

    pub fn bootstrap_vertex_refs_and_edges_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> RewriteGraphPmaResult<RewriteBootstrapGraphWriteSummary> {
        if initial_edges.is_empty() {
            let ordinals = self.append_empty_vertex_pairs(vertex_refs.len())?;
            let vertex_ordinals = vertex_refs
                .iter()
                .copied()
                .zip(ordinals.iter().copied())
                .map(|(vertex_ref, (forward_ordinal, reverse_ordinal))| {
                    RewriteVertexOrdinalMapping {
                        vertex_ref,
                        forward_ordinal,
                        reverse_ordinal,
                    }
                })
                .collect();
            let (refreshed_forward_vertices, refreshed_reverse_vertices) =
                self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
            let summary = RewriteBootstrapGraphWriteSummary {
                vertex_ordinals,
                inserts: Vec::new(),
                locators: Vec::new(),
                refreshed: RewriteRefreshedVertices::new(
                    refreshed_forward_vertices,
                    refreshed_reverse_vertices,
                ),
            };
            self.record_write_event(RewriteFacadeWriteEvent::BootstrapGraph(summary.clone()));
            return Ok(summary);
        }

        let ordinals = self.append_empty_vertex_pairs(vertex_refs.len())?;
        let vertex_ordinals: Vec<RewriteVertexOrdinalMapping> = vertex_refs
            .iter()
            .copied()
            .zip(ordinals.iter().copied())
            .map(
                |(vertex_ref, (forward_ordinal, reverse_ordinal))| RewriteVertexOrdinalMapping {
                    vertex_ref,
                    forward_ordinal,
                    reverse_ordinal,
                },
            )
            .collect();

        let mut inserts = Vec::with_capacity(initial_edges.len());
        let mut locators = Vec::with_capacity(initial_edges.len());
        for (edge_id, src_index, dst_index, label_id) in initial_edges.iter().copied() {
            let Some(src_mapping) = vertex_ordinals.get(src_index).copied() else {
                return Err(RewriteGraphPmaError::InvalidLocatorInputs);
            };
            let Some(dst_mapping) = vertex_ordinals.get(dst_index).copied() else {
                return Err(RewriteGraphPmaError::InvalidLocatorInputs);
            };
            let insert = self
                .graph
                .insert_edge_pair(
                    edge_id,
                    src_mapping.vertex_ref,
                    src_mapping.forward_ordinal,
                    dst_mapping.vertex_ref,
                    dst_mapping.reverse_ordinal,
                    label_id,
                )
                .ok_or(RewriteGraphPmaError::InvalidLocatorInputs)?;
            let GraphInsertResult::Inserted {
                locators: inserted_locators,
                ..
            } = insert
            else {
                return Err(RewriteGraphPmaError::InvalidLocatorInputs);
            };
            inserts.push(insert);
            locators.push(RewriteEdgeLogicalLocatorMapping {
                edge_id,
                canonical: inserted_locators.forward,
                forward: inserted_locators.forward,
                reverse: inserted_locators.reverse,
            });
        }

        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = RewriteBootstrapGraphWriteSummary {
            vertex_ordinals,
            inserts,
            locators,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::BootstrapGraph(summary.clone()));
        Ok(summary)
    }

    pub(super) fn define_empty_surface_regions(
        manager: &mut RegionManager,
        surface: crate::low_level::SurfaceKind,
    ) {
        let kinds = match surface {
            crate::low_level::SurfaceKind::Forward => [
                RegionKind::ForwardVertexTable,
                RegionKind::ForwardEdgeEntries,
                RegionKind::ForwardLabelIndex,
                RegionKind::ForwardSegmentLog,
            ],
            crate::low_level::SurfaceKind::Reverse => [
                RegionKind::ReverseVertexTable,
                RegionKind::ReverseEdgeEntries,
                RegionKind::ReverseLabelIndex,
                RegionKind::ReverseSegmentLog,
            ],
        };

        for kind in kinds {
            let is_vertex_table = matches!(
                kind,
                RegionKind::ForwardVertexTable | RegionKind::ReverseVertexTable
            );
            if is_vertex_table {
                manager.define_bucket_region(kind, default_property_region_chain());
            } else {
                manager.define_extent_region(
                    kind,
                    ExtentChain::new(
                        ExtentId::NULL,
                        ExtentId::NULL,
                        0,
                        WasmPages::new(1),
                        WasmPages::new(1),
                    ),
                );
            }
        }
    }

    pub(super) fn define_empty_property_regions(manager: &mut RegionManager) {
        for kind in [
            RegionKind::NodePropertyStore,
            RegionKind::EdgePropertyStore,
            RegionKind::PropertyIndex,
        ] {
            manager.define_bucket_region(kind, default_property_region_chain());
        }
        manager.define_extent_region(
            RegionKind::MaintenanceQueue,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );
    }

    pub fn ensure_local_capacity_for_incoming_live_entries_and_write(
        &mut self,
        spec: RebalancePrepareSpec<'_>,
        memory: &impl Memory,
    ) -> Result<GraphEnsureCapacityWriteSummary, WritebackError> {
        let summary = self
            .graph
            .ensure_local_capacity_for_incoming_live_entries_and_write(
                spec,
                &mut self.manager.borrow_mut(),
                memory,
            )?;
        self.record_write_event(RewriteFacadeWriteEvent::EnsureCapacity(summary.clone()));
        Ok(summary)
    }

    pub fn ensure_local_capacity_for_incoming_live_entries_with_segment_replacement_and_write(
        &mut self,
        spec: RebalancePrepareSpec<'_>,
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<GraphEnsureCapacitySegmentWriteSummary, WritebackError> {
        let summary = self
            .graph
            .ensure_local_capacity_for_incoming_live_entries_with_segment_replacement_and_write(
                spec,
                &mut self.manager.borrow_mut(),
                memory,
                retired_epoch,
            )?;
        self.record_write_event(RewriteFacadeWriteEvent::EnsureCapacitySegment(
            summary.clone(),
        ));
        Ok(summary)
    }

    pub fn insert_edge_pair_with_local_rebalance_and_write(
        &mut self,
        mut spec: RebalanceInsertSpec<'_>,
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        spec.planned_incoming_live_entries = 1;
        let summary = self.graph.insert_edge_pair_with_local_rebalance_and_write(
            spec,
            &mut self.manager.borrow_mut(),
            memory,
        )?;
        self.record_write_event(RewriteFacadeWriteEvent::InsertEdge(summary.clone()));
        Ok(summary)
    }

    pub fn insert_edge_pair_with_local_rebalance_and_segment_replacement_and_write(
        &mut self,
        mut spec: RebalanceInsertSpec<'_>,
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<GraphInsertSegmentWriteSummary, WritebackError> {
        spec.planned_incoming_live_entries = 1;
        let summary = self
            .graph
            .insert_edge_pair_with_local_rebalance_and_segment_replacement_and_write(
                spec,
                &mut self.manager.borrow_mut(),
                memory,
                retired_epoch,
            )?;
        self.record_write_event(RewriteFacadeWriteEvent::InsertEdgeSegment(summary.clone()));
        Ok(summary)
    }

    pub fn set_insert_policy(&mut self, insert_policy: GraphInsertPolicy) {
        self.graph.insert_policy = insert_policy;
    }

    pub fn enable_deferred_foreground_rebalance(&mut self, hard_overflow_chain_len: usize) {
        self.graph.insert_policy.defer_rebalance_to_maintenance = true;
        self.graph.insert_policy.hard_overflow_chain_len = hard_overflow_chain_len;
    }

    pub fn disable_deferred_foreground_rebalance(&mut self) {
        self.graph.insert_policy.defer_rebalance_to_maintenance = false;
    }

    pub fn set_maintenance_fairness(
        &mut self,
        recent_epoch_window: u64,
        recent_epoch_penalty: u64,
    ) {
        self.graph.insert_policy.maintenance_recent_epoch_window = recent_epoch_window;
        self.graph.insert_policy.maintenance_recent_epoch_penalty = recent_epoch_penalty;
    }

    pub fn disable_maintenance_fairness(&mut self) {
        self.set_maintenance_fairness(0, 0);
    }

    pub fn collect_maintenance_candidates(
        &self,
        vertex_refs: &[VertexRef],
    ) -> Option<Vec<GraphMaintenanceCandidate>> {
        self.graph.collect_maintenance_candidates(vertex_refs)
    }

    pub fn collect_maintenance_candidates_at_epoch(
        &self,
        vertex_refs: &[VertexRef],
        current_epoch: u64,
    ) -> Option<Vec<GraphMaintenanceCandidate>> {
        self.graph
            .collect_maintenance_candidates_at_epoch(vertex_refs, Some(current_epoch))
    }

    pub fn collect_maintenance_work_items(
        &self,
        vertex_refs: &[VertexRef],
    ) -> Option<Vec<GraphMaintenanceWorkItem>> {
        self.graph.collect_maintenance_work_items(vertex_refs)
    }

    pub fn collect_maintenance_work_items_at_epoch(
        &self,
        vertex_refs: &[VertexRef],
        current_epoch: u64,
    ) -> Option<Vec<GraphMaintenanceWorkItem>> {
        self.graph
            .collect_maintenance_work_items_at_epoch(vertex_refs, Some(current_epoch))
    }

    pub fn rebuild_maintenance_queue(&mut self, vertex_refs: &[VertexRef]) -> Option<usize> {
        let queue_len_before = self.graph.maintenance_queue().len();
        let queue_len_after = self.graph.rebuild_maintenance_queue(vertex_refs)?;
        let persisted_bytes = Self::maintenance_queue_serialized_len(queue_len_after)
            .expect("queue serialized len should fit");
        self.record_write_event(RewriteFacadeWriteEvent::MaintenanceQueue(
            RewriteMaintenanceQueueProjection {
                action: RewriteMaintenanceQueueAction::Rebuild,
                queue_len_before,
                queue_len_after,
                persisted_bytes,
                format_version: Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION,
            },
        ));
        self.production_metrics.record_maintenance_queue_rebuild();
        Some(queue_len_after)
    }

    pub fn rebuild_maintenance_queue_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        memory: &impl Memory,
    ) -> Result<Option<usize>, WritebackError> {
        let queue_len_after = self.rebuild_maintenance_queue(vertex_refs);
        self.persist_maintenance_queue(memory)?;
        Ok(queue_len_after)
    }

    pub fn rebuild_maintenance_queue_at_epoch(
        &mut self,
        vertex_refs: &[VertexRef],
        current_epoch: u64,
    ) -> Option<usize> {
        let queue_len_before = self.graph.maintenance_queue().len();
        let queue_len_after = self
            .graph
            .rebuild_maintenance_queue_at_epoch(vertex_refs, Some(current_epoch))?;
        let persisted_bytes = Self::maintenance_queue_serialized_len(queue_len_after)
            .expect("queue serialized len should fit");
        self.record_write_event(RewriteFacadeWriteEvent::MaintenanceQueue(
            RewriteMaintenanceQueueProjection {
                action: RewriteMaintenanceQueueAction::Rebuild,
                queue_len_before,
                queue_len_after,
                persisted_bytes,
                format_version: Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION,
            },
        ));
        self.production_metrics.record_maintenance_queue_rebuild();
        Some(queue_len_after)
    }

    pub fn rebuild_maintenance_queue_at_epoch_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        current_epoch: u64,
        memory: &impl Memory,
    ) -> Result<Option<usize>, WritebackError> {
        let queue_len_after = self.rebuild_maintenance_queue_at_epoch(vertex_refs, current_epoch);
        self.persist_maintenance_queue(memory)?;
        Ok(queue_len_after)
    }

    pub fn refresh_maintenance_queue(&mut self, vertex_refs: &[VertexRef]) -> Option<usize> {
        let queue_len_before = self.graph.maintenance_queue().len();
        let queue_len_after = self.graph.refresh_maintenance_queue(vertex_refs)?;
        let persisted_bytes = Self::maintenance_queue_serialized_len(queue_len_after)
            .expect("queue serialized len should fit");
        self.record_write_event(RewriteFacadeWriteEvent::MaintenanceQueue(
            RewriteMaintenanceQueueProjection {
                action: RewriteMaintenanceQueueAction::Refresh,
                queue_len_before,
                queue_len_after,
                persisted_bytes,
                format_version: Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION,
            },
        ));
        self.production_metrics.record_maintenance_queue_refresh();
        Some(queue_len_after)
    }

    pub fn refresh_maintenance_queue_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        memory: &impl Memory,
    ) -> Result<Option<usize>, WritebackError> {
        let queue_len_after = self.refresh_maintenance_queue(vertex_refs);
        self.persist_maintenance_queue(memory)?;
        Ok(queue_len_after)
    }

    pub fn refresh_maintenance_queue_at_epoch(
        &mut self,
        vertex_refs: &[VertexRef],
        current_epoch: u64,
    ) -> Option<usize> {
        let queue_len_before = self.graph.maintenance_queue().len();
        let queue_len_after = self
            .graph
            .refresh_maintenance_queue_at_epoch(vertex_refs, Some(current_epoch))?;
        let persisted_bytes = Self::maintenance_queue_serialized_len(queue_len_after)
            .expect("queue serialized len should fit");
        self.record_write_event(RewriteFacadeWriteEvent::MaintenanceQueue(
            RewriteMaintenanceQueueProjection {
                action: RewriteMaintenanceQueueAction::Refresh,
                queue_len_before,
                queue_len_after,
                persisted_bytes,
                format_version: Self::SERIALIZED_MAINTENANCE_QUEUE_VERSION,
            },
        ));
        self.production_metrics.record_maintenance_queue_refresh();
        Some(queue_len_after)
    }

    pub fn refresh_maintenance_queue_at_epoch_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        current_epoch: u64,
        memory: &impl Memory,
    ) -> Result<Option<usize>, WritebackError> {
        let queue_len_after = self.refresh_maintenance_queue_at_epoch(vertex_refs, current_epoch);
        self.persist_maintenance_queue(memory)?;
        Ok(queue_len_after)
    }

    pub fn maintenance_queue(&self) -> &[GraphMaintenanceWorkItem] {
        self.graph.maintenance_queue()
    }

    pub fn plan_one_maintenance_cycle(
        &self,
        vertex_refs: &[VertexRef],
    ) -> Option<GraphMaintenanceCyclePlan> {
        self.graph.plan_one_maintenance_cycle(vertex_refs)
    }

    pub fn plan_one_maintenance_cycle_at_epoch(
        &self,
        vertex_refs: &[VertexRef],
        current_epoch: u64,
    ) -> Option<GraphMaintenanceCyclePlan> {
        self.graph
            .plan_one_maintenance_cycle_at_epoch(vertex_refs, Some(current_epoch))
    }

    pub fn plan_maintenance_cycle_from_work_item(
        &self,
        work_item: GraphMaintenanceWorkItem,
    ) -> Option<GraphMaintenanceCyclePlan> {
        self.graph.plan_maintenance_cycle_from_work_item(work_item)
    }

    pub fn run_one_maintenance_cycle_with_segment_replacement_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<Option<GraphMaintenanceCycleWriteSummary>, WritebackError> {
        let queue_storage_before = Some(self.current_maintenance_queue_storage_snapshot(memory)?);
        let mut summary = self
            .graph
            .run_one_maintenance_cycle_with_segment_replacement_and_write(
                vertex_refs,
                forward_base_edge_ids_by_ordinal,
                &mut self.manager.borrow_mut(),
                memory,
                retired_epoch,
            )?;
        self.persist_maintenance_queue(memory)?;
        let queue_storage_after = Some(self.current_maintenance_queue_storage_snapshot(memory)?);
        if let Some(summary) = &mut summary {
            summary.queue_storage_before = queue_storage_before;
            summary.queue_storage_after = queue_storage_after;
            self.record_write_event(RewriteFacadeWriteEvent::MaintenanceCycle(summary.clone()));
        }
        Ok(summary)
    }

    pub fn run_one_maintenance_cycle_from_work_item_with_segment_replacement_and_write(
        &mut self,
        work_item: GraphMaintenanceWorkItem,
        vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<Option<GraphMaintenanceCycleWriteSummary>, WritebackError> {
        let queue_storage_before = Some(self.current_maintenance_queue_storage_snapshot(memory)?);
        let mut summary = self
            .graph
            .run_one_maintenance_cycle_from_work_item_with_segment_replacement_and_write(
                work_item,
                vertex_refs,
                forward_base_edge_ids_by_ordinal,
                &mut self.manager.borrow_mut(),
                memory,
                retired_epoch,
            )?;
        self.persist_maintenance_queue(memory)?;
        let queue_storage_after = Some(self.current_maintenance_queue_storage_snapshot(memory)?);
        if let Some(summary) = &mut summary {
            summary.queue_storage_before = queue_storage_before;
            summary.queue_storage_after = queue_storage_after;
            self.record_write_event(RewriteFacadeWriteEvent::MaintenanceCycle(summary.clone()));
        }
        Ok(summary)
    }

    pub fn run_next_queued_maintenance_cycle_with_segment_replacement_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<Option<GraphMaintenanceCycleWriteSummary>, WritebackError> {
        let queue_storage_before = Some(self.current_maintenance_queue_storage_snapshot(memory)?);
        let mut summary = self
            .graph
            .run_next_queued_maintenance_cycle_with_segment_replacement_and_write(
                vertex_refs,
                forward_base_edge_ids_by_ordinal,
                &mut self.manager.borrow_mut(),
                memory,
                retired_epoch,
            )?;
        self.persist_maintenance_queue(memory)?;
        let queue_storage_after = Some(self.current_maintenance_queue_storage_snapshot(memory)?);
        if let Some(summary) = &mut summary {
            summary.queue_storage_before = queue_storage_before;
            summary.queue_storage_after = queue_storage_after;
            self.record_write_event(RewriteFacadeWriteEvent::MaintenanceCycle(summary.clone()));
        }
        Ok(summary)
    }

    pub fn run_maintenance_cycles_with_segment_replacement_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
        retired_epoch: u64,
        max_cycles: usize,
        min_retired_epochs_before_sweep: u64,
    ) -> Result<GraphMaintenanceBatchWriteSummary, WritebackError> {
        let queue_storage_before = Some(self.current_maintenance_queue_storage_snapshot(memory)?);
        let mut summary = self
            .graph
            .run_maintenance_cycles_with_segment_replacement_and_write(
                vertex_refs,
                forward_base_edge_ids_by_ordinal,
                &mut self.manager.borrow_mut(),
                memory,
                retired_epoch,
                max_cycles,
                min_retired_epochs_before_sweep,
            )?;
        self.persist_maintenance_queue(memory)?;
        let queue_storage_after = Some(self.current_maintenance_queue_storage_snapshot(memory)?);
        summary.queue_storage_before = queue_storage_before;
        summary.queue_storage_after = queue_storage_after;
        self.record_write_event(RewriteFacadeWriteEvent::MaintenanceBatch(summary.clone()));
        Ok(summary)
    }

    pub fn run_queued_maintenance_cycles_with_segment_replacement_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        memory: &impl Memory,
        retired_epoch: u64,
        max_cycles: usize,
        min_retired_epochs_before_sweep: u64,
    ) -> Result<GraphMaintenanceBatchWriteSummary, WritebackError> {
        let queue_storage_before = Some(self.current_maintenance_queue_storage_snapshot(memory)?);
        let mut summary = self
            .graph
            .run_queued_maintenance_cycles_with_segment_replacement_and_write(
                vertex_refs,
                forward_base_edge_ids_by_ordinal,
                &mut self.manager.borrow_mut(),
                memory,
                retired_epoch,
                max_cycles,
                min_retired_epochs_before_sweep,
            )?;
        self.persist_maintenance_queue(memory)?;
        let queue_storage_after = Some(self.current_maintenance_queue_storage_snapshot(memory)?);
        summary.queue_storage_before = queue_storage_before;
        summary.queue_storage_after = queue_storage_after;
        self.record_write_event(RewriteFacadeWriteEvent::MaintenanceBatch(summary.clone()));
        self.production_metrics.record_maintenance_queued_batch();
        Ok(summary)
    }

    pub fn replace_edge_pair_and_write(
        &mut self,
        spec: EdgeReplaceSpec,
        memory: &impl Memory,
    ) -> Result<RewriteReplaceEdgeSummary, WritebackError> {
        let mutation =
            self.graph
                .replace_edge_pair(spec)
                .ok_or(WritebackError::MissingRegionDefinition(
                    crate::low_level::RegionKind::ForwardEdgeEntries,
                ))?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) = self
            .graph
            .refresh_and_write_dirty_to_stable_memory(&mut self.manager.borrow_mut(), memory)?;
        let summary = RewriteGraphMutationWriteSummary {
            mutation,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::ReplaceEdge(summary.clone()));
        Ok(summary)
    }

    pub fn tombstone_edge_pair_and_write(
        &mut self,
        spec: EdgeTombstoneSpec,
        memory: &impl Memory,
    ) -> Result<RewriteGraphMutationWriteSummary<GraphMutationPath>, WritebackError> {
        let mutation =
            self.graph
                .tombstone_edge_pair(spec)
                .ok_or(WritebackError::MissingRegionDefinition(
                    crate::low_level::RegionKind::ForwardEdgeEntries,
                ))?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) = self
            .graph
            .refresh_and_write_dirty_to_stable_memory(&mut self.manager.borrow_mut(), memory)?;
        let summary = RewriteGraphMutationWriteSummary {
            mutation,
            refreshed: RewriteRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(RewriteFacadeWriteEvent::DeleteEdge(summary.clone()));
        Ok(summary)
    }

    pub fn begin_batch_mutation<'a>(
        &'a mut self,
        memory: &'a M,
    ) -> RewriteGraphPmaBatchSession<'a, M> {
        RewriteGraphPmaBatchSession::new(&mut self.graph, &self.manager, memory)
    }

    pub fn bind<'a>(&'a mut self, memory: &'a M) -> RewriteGraphStoreAdapter<'a, Self> {
        RewriteGraphStoreAdapter::new(self, memory)
    }

    pub fn bind_kernel_overlay<'a>(
        &'a mut self,
        memory: &'a M,
    ) -> RewriteKernelOverlayGraph<'a, &'a mut Self> {
        self.bind(memory).into_kernel_overlay()
    }
}
