//! GraphStore `maintenance` implementation.

use super::super::stable::GRAPH;
use ic_stable_lara::{MaintenanceBudget, labeled::LabeledBidirectionalMaintenanceReport};

use super::GraphStore;
use super::error::GraphStoreError;
use super::helpers::GraphSidecarMoveObserver;

impl GraphStore {
    pub fn run_maintenance_best_effort(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<LabeledBidirectionalMaintenanceReport, GraphStoreError> {
        let mut observer = GraphSidecarMoveObserver;
        let report = GRAPH
            .with_borrow(|graph| {
                graph.maintenance_with_edge_slot_move_observer(budget, &mut observer)
            })
            .map_err(GraphStoreError::from)?;
        Ok(report)
    }

    pub fn run_timer_maintenance_tick(
        &self,
    ) -> Result<LabeledBidirectionalMaintenanceReport, GraphStoreError> {
        self.run_maintenance_best_effort(crate::facade::timer_lara_maintenance_budget())
    }

    /// Drains all queued LARA maintenance work (used after delete-style mutations).
    pub(crate) fn drain_deferred_maintenance(&self) -> Result<(), GraphStoreError> {
        self.drain_deferred_maintenance_with_budget(
            crate::facade::unlimited_lara_maintenance_budget(),
        )
    }

    pub(crate) fn drain_deferred_maintenance_with_budget(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<(), GraphStoreError> {
        self.run_maintenance_best_effort(budget)?;
        Ok(())
    }

    /// Drains maintenance queued by the LARA insert path (`mark_compact_dense`, etc.).
    ///
    /// Does not enqueue extra compaction work: forcing `CompactVertexEdgeSpan` on every
    /// insert breaks tombstoned buckets (see `valued_insert_after_delete_*` tests).
    pub(crate) fn run_post_edge_insert_maintenance(&self) -> Result<(), GraphStoreError> {
        self.drain_deferred_maintenance_with_budget(
            crate::facade::post_edge_insert_maintenance_budget(),
        )
    }
}
