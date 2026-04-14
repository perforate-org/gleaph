use crate::integration::GraphStoreKernelOverlayGraph;
use crate::maintenance_dirty::{merge_dirty_ordinal_interval, pop_first_dirty_interval};

use super::*;

impl<M: Memory + Clone> GraphStore<M> {
    fn note_maintenance_dirty_after_forward_refresh(&mut self, refreshed_forward: &[usize]) {
        let radius = self.graph.insert_policy.rebalance_window_radius;
        let vmax = self.graph.forward.0.vertices.len();
        for &ordinal in refreshed_forward {
            let lo = ordinal.saturating_sub(radius);
            let hi = ordinal
                .saturating_add(radius)
                .saturating_add(1)
                .min(vmax);
            self.merge_maintenance_dirty_forward_ordinal_interval(lo as u64, hi as u64);
        }
    }

    /// Builds an in-memory [`RegionManager`] layout used for PMA adjacency read/write paths.
    ///
    /// Stable bytes for surfaces and legacy bucket-backed property regions still use this
    /// metadata to resolve physical addresses. Callers that only need a matching skeleton for
    /// hydration should use the same `bucket_size_in_pages` as the store that wrote `memory`.
    pub(super) fn bootstrap_region_manager_skeleton(
        bucket_size_in_pages: BucketSizeInPages,
    ) -> RegionManager {
        let mut manager = RegionManager::with_bucket_size(bucket_size_in_pages);
        Self::define_empty_surface_regions(&mut manager, crate::low_level::SurfaceKind::Forward);
        Self::define_empty_surface_regions(&mut manager, crate::low_level::SurfaceKind::Reverse);
        Self::define_empty_property_regions(&mut manager);
        manager
    }

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
    pub fn write_all_to_stable_memory(&mut self, memory: &impl Memory) -> GraphStoreResult<()> {
        let runtimes =
            HydratedSurfaceRuntimes::new(self.graph.forward.clone(), self.graph.reverse.clone());
        write_surface_runtimes_to_stable_memory(&mut self.manager.borrow_mut(), memory, &runtimes)?;
        // Property stores and equality index are direct-stable fixed-slot structures now.
        // They are updated at mutation time and do not participate in region writeback.
        self.persist_maintenance_queue(memory)?;
        self.write_shard_canister_directory_to_stable_memory(memory)
            .map_err(GraphStoreError::Writeback)?;
        self.node_property_store_dirty = false;
        self.edge_property_store_dirty = false;
        self.property_index_dirty = false;
        Ok(())
    }

    pub fn try_write_all_to_stable_memory(&mut self, memory: &impl Memory) -> GraphStoreResult<()> {
        self.write_all_to_stable_memory(memory)
    }

    pub fn refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> GraphStoreResult<(Vec<usize>, Vec<usize>)> {
        let refreshed = {
            let _p = crate::bench_profile::PhaseGuard::new("facade_low_level_graph_refresh_write");
            crate::canbench_scope::scope("pma_graph_refresh_write");
            self.graph
                .refresh_and_write_dirty_to_stable_memory(&mut self.manager.borrow_mut(), memory)?
        };
        self.note_maintenance_dirty_after_forward_refresh(&refreshed.0);
        self.node_property_store_dirty = false;
        self.edge_property_store_dirty = false;
        self.property_index_dirty = false;
        let _p = crate::bench_profile::PhaseGuard::new("facade_maint_queue_only");
        crate::canbench_scope::scope("pma_maint_queue_persist");
        self.persist_maintenance_queue(memory)?;
        self.write_shard_canister_directory_to_stable_memory(memory)
            .map_err(GraphStoreError::Writeback)?;
        Ok(refreshed)
    }

    pub fn try_refresh_and_write_dirty_to_stable_memory(
        &mut self,
        memory: &impl Memory,
    ) -> GraphStoreResult<(Vec<usize>, Vec<usize>)> {
        self.refresh_and_write_dirty_to_stable_memory(memory)
    }

    pub fn append_empty_vertex_pair(&mut self) -> GraphStoreResult<(usize, usize)> {
        self.graph
            .append_empty_vertex_pair()
            .ok_or(GraphStoreError::InvalidLocatorInputs)
    }

    pub fn append_empty_vertex_pairs(
        &mut self,
        count: usize,
    ) -> GraphStoreResult<Vec<(usize, usize)>> {
        self.graph
            .append_empty_vertex_pairs(count)
            .ok_or(GraphStoreError::InvalidLocatorInputs)
    }

    pub fn append_empty_vertex_pair_and_write(
        &mut self,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStoreAppendVertexWriteSummary> {
        let ordinals = self.append_empty_vertex_pair()?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphStoreAppendVertexWriteSummary {
            ordinals,
            refreshed: GraphStoreRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(GraphStoreFacadeWriteEvent::AppendVertex(summary.clone()));
        Ok(summary)
    }

    pub fn append_empty_vertex_pairs_and_write(
        &mut self,
        count: usize,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStoreAppendVerticesWriteSummary> {
        let ordinals = self.append_empty_vertex_pairs(count)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphStoreAppendVerticesWriteSummary {
            ordinals,
            refreshed: GraphStoreRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(GraphStoreFacadeWriteEvent::AppendVertices(summary.clone()));
        Ok(summary)
    }

    pub fn bootstrap_edge_between_new_vertices_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        dst_vertex: NodeId,
        label_id: LabelId,
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStoreBootstrapEdgeWriteSummary> {
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
                label_id.into(),
            )
            .ok_or(GraphStoreError::InvalidLocatorInputs)?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphStoreBootstrapEdgeWriteSummary {
            ordinals: (src_ordinal, dst_ordinal),
            insert,
            refreshed: GraphStoreRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(GraphStoreFacadeWriteEvent::BootstrapEdge(summary.clone()));
        Ok(summary)
    }

    pub fn bootstrap_vertex_refs_and_edges_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
        memory: &impl Memory,
    ) -> GraphStoreResult<GraphStoreBootstrapGraphWriteSummary> {
        if initial_edges.is_empty() {
            let ordinals = self.append_empty_vertex_pairs(vertex_refs.len())?;
            let vertex_ordinals = vertex_refs
                .iter()
                .copied()
                .zip(ordinals.iter().copied())
                .map(|(vertex_ref, (forward_ordinal, reverse_ordinal))| {
                    GraphStoreVertexOrdinalMapping {
                        vertex_ref,
                        forward_ordinal,
                        reverse_ordinal,
                    }
                })
                .collect();
            let (refreshed_forward_vertices, refreshed_reverse_vertices) =
                self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
            let summary = GraphStoreBootstrapGraphWriteSummary {
                vertex_ordinals,
                inserts: Vec::new(),
                locators: Vec::new(),
                refreshed: GraphStoreRefreshedVertices::new(
                    refreshed_forward_vertices,
                    refreshed_reverse_vertices,
                ),
            };
            self.record_write_event(GraphStoreFacadeWriteEvent::BootstrapGraph(summary.clone()));
            return Ok(summary);
        }

        let ordinals = self.append_empty_vertex_pairs(vertex_refs.len())?;
        let vertex_ordinals: Vec<GraphStoreVertexOrdinalMapping> = vertex_refs
            .iter()
            .copied()
            .zip(ordinals.iter().copied())
            .map(
                |(vertex_ref, (forward_ordinal, reverse_ordinal))| GraphStoreVertexOrdinalMapping {
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
                return Err(GraphStoreError::InvalidLocatorInputs);
            };
            let Some(dst_mapping) = vertex_ordinals.get(dst_index).copied() else {
                return Err(GraphStoreError::InvalidLocatorInputs);
            };
            let insert = self
                .graph
                .insert_edge_pair(
                    edge_id,
                    src_mapping.vertex_ref,
                    src_mapping.forward_ordinal,
                    dst_mapping.vertex_ref,
                    dst_mapping.reverse_ordinal,
                    label_id.into(),
                )
                .ok_or(GraphStoreError::InvalidLocatorInputs)?;
            let GraphInsertResult::Inserted {
                locators: inserted_locators,
                ..
            } = insert
            else {
                return Err(GraphStoreError::InvalidLocatorInputs);
            };
            inserts.push(insert);
            locators.push(GraphStoreEdgeLogicalLocatorMapping {
                edge_id,
                canonical: inserted_locators.forward,
                forward: inserted_locators.forward,
                reverse: inserted_locators.reverse,
            });
        }

        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.try_refresh_and_write_dirty_to_stable_memory(memory)?;
        let summary = GraphStoreBootstrapGraphWriteSummary {
            vertex_ordinals,
            inserts,
            locators,
            refreshed: GraphStoreRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(GraphStoreFacadeWriteEvent::BootstrapGraph(summary.clone()));
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
            RegionKind::ShardCanisterDirectory,
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
        self.note_maintenance_dirty_after_forward_refresh(&summary.refreshed_forward_vertices);
        self.record_write_event(GraphStoreFacadeWriteEvent::EnsureCapacity(summary.clone()));
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
        self.note_maintenance_dirty_after_forward_refresh(&summary.refreshed_forward_vertices);
        self.record_write_event(GraphStoreFacadeWriteEvent::EnsureCapacitySegment(
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
        self.note_maintenance_dirty_after_forward_refresh(&summary.refreshed_forward_vertices);
        self.record_write_event(GraphStoreFacadeWriteEvent::InsertEdge(summary.clone()));
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
        self.note_maintenance_dirty_after_forward_refresh(&summary.refreshed_forward_vertices);
        self.record_write_event(GraphStoreFacadeWriteEvent::InsertEdgeSegment(summary.clone()));
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

    /// Records one half-open forward-layout ordinal interval `[start, end)` into the stable dirty btree,
    /// merging overlaps. Used by incremental maintenance (no full-graph rescans).
    pub fn merge_maintenance_dirty_forward_ordinal_interval(&mut self, start: u64, end: u64) {
        merge_dirty_ordinal_interval(&mut self.maintenance_dirty_ordinal_map, start, end);
    }

    /// Count of disjoint dirty intervals currently in stable memory (slot 15).
    pub fn maintenance_dirty_forward_ordinal_interval_count(&self) -> u64 {
        self.maintenance_dirty_ordinal_map.len()
    }

    /// Removes and returns the smallest dirty interval `(start, end)` by lexicographic key order.
    pub fn pop_maintenance_dirty_forward_ordinal_interval(&mut self) -> Option<(u64, u64)> {
        pop_first_dirty_interval(&mut self.maintenance_dirty_ordinal_map)
    }

    /// Pops up to `max_intervals` disjoint dirty ranges from stable memory and, for each forward
    /// ordinal in those ranges, scores maintenance need and merges resulting work items into the
    /// in-memory queue (no full-graph candidate scan).
    pub fn drain_maintenance_dirty_into_queue_at_epoch(
        &mut self,
        vertex_refs: &[VertexRef],
        current_epoch: Option<u64>,
        max_intervals: usize,
    ) -> GraphStoreMaintenanceDirtyDrainSummary {
        let mut intervals_drained = 0usize;
        let mut work_items_merged = 0usize;
        let radius = self.graph.insert_policy.rebalance_window_radius;
        let vertex_count = vertex_refs
            .len()
            .min(self.graph.forward.0.vertices.len())
            .min(self.graph.reverse.0.vertices.len());
        for _ in 0..max_intervals {
            let Some((start_u64, end_u64)) = self.pop_maintenance_dirty_forward_ordinal_interval()
            else {
                break;
            };
            intervals_drained += 1;
            let Ok(start) = usize::try_from(start_u64) else {
                continue;
            };
            let Ok(end) = usize::try_from(end_u64) else {
                continue;
            };
            if start >= end {
                continue;
            }
            let end = end.min(vertex_count);
            for ordinal in start..end {
                let Some(candidate) = self.graph.collect_maintenance_candidate_at_ordinal_at_epoch(
                    vertex_refs,
                    current_epoch,
                    ordinal,
                ) else {
                    continue;
                };
                let item = candidate.into_work_item(radius, vertex_count);
                self.graph.merge_maintenance_work_item_into_queue(item);
                work_items_merged += 1;
            }
        }
        let queue_len_after = self.graph.maintenance_queue().len();
        GraphStoreMaintenanceDirtyDrainSummary {
            intervals_drained,
            work_items_merged,
            queue_len_after,
        }
    }

    pub fn drain_maintenance_dirty_into_queue(
        &mut self,
        vertex_refs: &[VertexRef],
        max_intervals: usize,
    ) -> GraphStoreMaintenanceDirtyDrainSummary {
        self.drain_maintenance_dirty_into_queue_at_epoch(vertex_refs, None, max_intervals)
    }

    pub fn drain_maintenance_dirty_into_queue_at_epoch_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        current_epoch: Option<u64>,
        max_intervals: usize,
        memory: &impl Memory,
    ) -> Result<GraphStoreMaintenanceDirtyDrainSummary, WritebackError> {
        let summary = self.drain_maintenance_dirty_into_queue_at_epoch(
            vertex_refs,
            current_epoch,
            max_intervals,
        );
        self.persist_maintenance_queue(memory)?;
        Ok(summary)
    }

    pub fn drain_maintenance_dirty_into_queue_and_write(
        &mut self,
        vertex_refs: &[VertexRef],
        max_intervals: usize,
        memory: &impl Memory,
    ) -> Result<GraphStoreMaintenanceDirtyDrainSummary, WritebackError> {
        self.drain_maintenance_dirty_into_queue_at_epoch_and_write(vertex_refs, None, max_intervals, memory)
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
        self.record_write_event(GraphStoreFacadeWriteEvent::MaintenanceQueue(
            GraphStoreMaintenanceQueueProjection {
                action: GraphStoreMaintenanceQueueAction::Rebuild,
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
        self.record_write_event(GraphStoreFacadeWriteEvent::MaintenanceQueue(
            GraphStoreMaintenanceQueueProjection {
                action: GraphStoreMaintenanceQueueAction::Rebuild,
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
        self.record_write_event(GraphStoreFacadeWriteEvent::MaintenanceQueue(
            GraphStoreMaintenanceQueueProjection {
                action: GraphStoreMaintenanceQueueAction::Refresh,
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
        self.record_write_event(GraphStoreFacadeWriteEvent::MaintenanceQueue(
            GraphStoreMaintenanceQueueProjection {
                action: GraphStoreMaintenanceQueueAction::Refresh,
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
            self.record_write_event(GraphStoreFacadeWriteEvent::MaintenanceCycle(summary.clone()));
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
            self.record_write_event(GraphStoreFacadeWriteEvent::MaintenanceCycle(summary.clone()));
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
            self.record_write_event(GraphStoreFacadeWriteEvent::MaintenanceCycle(summary.clone()));
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
                crate::low_level::MaintenanceCycleVertexInputs {
                    vertex_ids: vertex_refs,
                    forward_base_edge_ids_by_ordinal,
                },
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
        self.record_write_event(GraphStoreFacadeWriteEvent::MaintenanceBatch(summary.clone()));
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
                crate::low_level::MaintenanceCycleVertexInputs {
                    vertex_ids: vertex_refs,
                    forward_base_edge_ids_by_ordinal,
                },
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
        self.record_write_event(GraphStoreFacadeWriteEvent::MaintenanceBatch(summary.clone()));
        self.production_metrics.record_maintenance_queued_batch();
        Ok(summary)
    }

    pub fn replace_edge_pair_and_write(
        &mut self,
        spec: EdgeReplaceSpec,
        memory: &impl Memory,
    ) -> Result<GraphStoreReplaceEdgeSummary, WritebackError> {
        let mutation =
            self.graph
                .replace_edge_pair(spec)
                .ok_or(WritebackError::MissingRegionDefinition(
                    crate::low_level::RegionKind::ForwardEdgeEntries,
                ))?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) = self
            .graph
            .refresh_and_write_dirty_to_stable_memory(&mut self.manager.borrow_mut(), memory)?;
        self.note_maintenance_dirty_after_forward_refresh(&refreshed_forward_vertices);
        let summary = GraphStoreMutationWriteSummary {
            mutation,
            refreshed: GraphStoreRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(GraphStoreFacadeWriteEvent::ReplaceEdge(summary.clone()));
        Ok(summary)
    }

    pub fn tombstone_edge_pair_and_write(
        &mut self,
        spec: EdgeTombstoneSpec,
        memory: &impl Memory,
    ) -> Result<GraphStoreMutationWriteSummary<GraphMutationPath>, WritebackError> {
        let mutation =
            self.graph
                .tombstone_edge_pair(spec)
                .ok_or(WritebackError::MissingRegionDefinition(
                    crate::low_level::RegionKind::ForwardEdgeEntries,
                ))?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) = self
            .graph
            .refresh_and_write_dirty_to_stable_memory(&mut self.manager.borrow_mut(), memory)?;
        self.note_maintenance_dirty_after_forward_refresh(&refreshed_forward_vertices);
        let summary = GraphStoreMutationWriteSummary {
            mutation,
            refreshed: GraphStoreRefreshedVertices::new(
                refreshed_forward_vertices,
                refreshed_reverse_vertices,
            ),
        };
        self.record_write_event(GraphStoreFacadeWriteEvent::DeleteEdge(summary.clone()));
        Ok(summary)
    }

    pub fn begin_batch_mutation<'a>(&'a mut self, memory: &'a M) -> GraphStoreBatchSession<'a, M> {
        GraphStoreBatchSession::new(&mut self.graph, &self.manager, memory)
    }

    pub fn bind<'a>(&'a mut self, memory: &'a M) -> GraphStoreStoreAdapter<'a, Self> {
        GraphStoreStoreAdapter::new(self, memory)
    }

    pub fn bind_kernel_overlay<'a>(
        &'a mut self,
        memory: &'a M,
    ) -> GraphStoreKernelOverlayGraph<'a, &'a mut Self> {
        self.bind(memory).into_kernel_overlay()
    }
}
