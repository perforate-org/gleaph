//! Shared deferred-maintenance contracts for LARA graph wrappers.
//!
//! The labeled graph layers own their concrete persistent worklists; this module
//! owns the threshold configuration, validation, budget, and work-report types
//! they share.

use std::fmt;

/// Thresholds that control when deferred inserts enqueue maintenance work.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeferredConfig {
    /// Leaf density at or above which a segment is marked dirty after insert.
    pub leaf_dirty_density: f64,
    /// Per-segment log fill ratio at or above which a segment is marked urgent.
    pub log_urgent_ratio: f64,
}

impl Default for DeferredConfig {
    fn default() -> Self {
        Self {
            leaf_dirty_density: 0.85,
            log_urgent_ratio: 0.80,
        }
    }
}

impl DeferredConfig {
    pub(crate) fn validate(self) -> Result<Self, DeferredConfigError> {
        validate_ratio("leaf_dirty_density", self.leaf_dirty_density)?;
        validate_ratio("log_urgent_ratio", self.log_urgent_ratio)?;
        Ok(self)
    }
}

/// Invalid deferred-maintenance configuration value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeferredConfigError {
    field: &'static str,
    value: f64,
}

impl fmt::Display for DeferredConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} must be in 0.0..=1.0, got {}", self.field, self.value)
    }
}

impl std::error::Error for DeferredConfigError {}

fn validate_ratio(field: &'static str, value: f64) -> Result<(), DeferredConfigError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(DeferredConfigError { field, value })
    }
}

/// Budget for one deferred maintenance call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceBudget {
    /// Maximum instructions allowed from this maintenance call's baseline.
    pub max_instructions: u64,
    /// Headroom reserved before starting another unit of work.
    pub reserve_instructions: u64,
    /// Number of loop iterations between instruction counter checks.
    pub checkpoint_every: u32,
    /// Optional hard cap on work items processed in one call.
    pub max_work_items: Option<u32>,
    /// Optional hard cap on segment steps processed in one call.
    pub max_segments: Option<u32>,
    /// Optional hard cap on delete edge steps processed in one call.
    pub max_delete_edge_steps: Option<u32>,
}

/// Work performed by one or more deferred maintenance steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceWorkReport {
    /// Number of queue work items consumed or advanced.
    pub processed_work_items: u32,
    /// Number of queue entries consumed.
    pub processed_segments: u32,
    /// Number of segments that actually needed rebalancing.
    pub rebalanced_segments: u32,
    /// Whether any step expanded the edge slab.
    pub resized: bool,
    /// Queue length after the reported work.
    pub remaining_queue_len: u64,
    /// Number of delete edge steps processed.
    pub processed_delete_edge_steps: u32,
    /// Number of vertex delete jobs completed.
    pub completed_vertex_deletes: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_config_default_thresholds_validate() {
        assert_eq!(
            DeferredConfig::validate(DeferredConfig::default()),
            Ok(DeferredConfig::default())
        );
    }

    #[test]
    fn deferred_config_rejects_out_of_range_ratios() {
        let bad = DeferredConfig {
            leaf_dirty_density: f64::NAN,
            log_urgent_ratio: 0.80,
        };
        let err = bad.validate().unwrap_err();
        assert_eq!(err.field, "leaf_dirty_density");
    }
}
