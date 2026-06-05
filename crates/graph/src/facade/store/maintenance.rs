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

}
