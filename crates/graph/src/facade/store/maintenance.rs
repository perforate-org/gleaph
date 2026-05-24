//! GraphStore `maintenance` implementation.

use super::super::stable::GRAPH;
use ic_stable_lara::{MaintenanceBudget, labeled::LabeledBidirectionalMaintenanceReport};

use super::GraphStore;
use super::error::GraphStoreError;
use super::helpers::GraphSidecarMoveObserver;
use crate::facade::migration::{
    migration_maintenance_step, prune_migrated_source_maintenance_step,
};

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
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = pollster::block_on(migration_maintenance_step(self));
            let _ = prune_migrated_source_maintenance_step(self);
        }
        #[cfg(target_family = "wasm")]
        {
            let _ = prune_migrated_source_maintenance_step(self);
        }
        Ok(report)
    }

    pub fn run_timer_maintenance_tick(
        &self,
    ) -> Result<LabeledBidirectionalMaintenanceReport, GraphStoreError> {
        self.run_maintenance_best_effort(crate::facade::timer_lara_maintenance_budget())
    }

    /// Compact CSR rows after stub payload removal (no migration/prune queue recursion).
    pub(crate) fn compact_lara_graph_after_stub_prune()
    -> Result<LabeledBidirectionalMaintenanceReport, GraphStoreError> {
        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        let mut observer = GraphSidecarMoveObserver;
        GRAPH.with_borrow(|graph| {
            graph
                .maintenance_with_edge_slot_move_observer(budget, &mut observer)
                .map_err(GraphStoreError::from)
        })
    }
}
