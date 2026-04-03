//! Thin graph-level coordination across forward and reverse adjacency surfaces.

use std::collections::BTreeMap;

use crate::stable::Memory;
use gleaph_graph_kernel::{EdgeId, LabelId};

use super::edge::{EdgeEntry, EdgeMeta};
use super::extent::{EdgeSegmentHeader, EdgeSegmentState};
use super::hydration::{
    WritebackError, write_dirty_forward_surface_runtime_to_stable_memory,
    write_dirty_reverse_surface_runtime_to_stable_memory,
};
use super::ids::VertexRef;
use super::locator::EdgeLogicalLocatorSidecar;
use super::manager::RegionManager;
use super::overflow::LogOffset;
use super::region::RegionKind;
use super::runtime::{
    BaseInsertDecision, EdgeInsertPath, ForwardSurfaceRuntime, ResolvedEdgeSlot,
    ReverseSurfaceRuntime, SurfaceAppliedRebalanceSummary, SurfaceLocalRebalanceDelta,
    SurfaceVertexWindowSummary, SurfaceWeightedWindowLayout,
};
use super::vertex::EdgeIndex;

/// Thin graph-level runtime that coordinates forward/reverse surfaces and the
/// semantic `EdgeId -> logical locator` sidecar.
///
/// This is intentionally still low-level. It does not yet perform allocator
/// growth or rebalance. It only ensures that one logical edge mutation updates
/// both directional surfaces and the canonical logical locator sidecar
/// together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRuntime {
    pub forward: ForwardSurfaceRuntime,
    pub reverse: ReverseSurfaceRuntime,
    logical_locator_sidecar: EdgeLogicalLocatorSidecar,
    pub insert_policy: GraphInsertPolicy,
    recent_maintenance_epochs_by_ordinal: BTreeMap<usize, u64>,
    maintenance_queue: Vec<GraphMaintenanceWorkItem>,
}

/// Thin batch-mutation facade over one graph runtime.
///
/// The session keeps dirty in-memory state across multiple operations and
/// exposes an explicit flush step for the end of the batch.
pub struct GraphBatchMutationSession<'a, M: Memory> {
    graph: &'a mut GraphRuntime,
    manager: &'a std::cell::RefCell<RegionManager>,
    memory: &'a M,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgePairEndpoints {
    pub src_vertex_ref: VertexRef,
    pub src_ordinal: usize,
    pub dst_vertex_ref: VertexRef,
    pub dst_ordinal: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgePairLogicalLocators {
    pub forward: super::edge::LogicalEdgeLocator,
    pub reverse: super::edge::LogicalEdgeLocator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeReplaceSpec {
    pub edge_id: EdgeId,
    pub endpoints: EdgePairEndpoints,
    pub locators: EdgePairLogicalLocators,
    pub label_id: LabelId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeTombstoneSpec {
    pub edge_id: EdgeId,
    pub endpoints: EdgePairEndpoints,
    pub locators: EdgePairLogicalLocators,
}

#[derive(Clone, Copy, Debug)]
pub struct RebalancePrepareSpec<'a> {
    pub endpoints: EdgePairEndpoints,
    pub planned_incoming_live_entries: u32,
    pub forward_rebalance_vertex_ids: &'a [VertexRef],
    pub forward_rebalance_base_edge_ids_by_ordinal: &'a [Vec<EdgeId>],
}

#[derive(Clone, Copy, Debug)]
pub struct RebalanceInsertSpec<'a> {
    pub edge_id: EdgeId,
    pub endpoints: EdgePairEndpoints,
    pub label_id: LabelId,
    pub planned_incoming_live_entries: u32,
    pub forward_rebalance_vertex_ids: &'a [VertexRef],
    pub forward_rebalance_base_edge_ids_by_ordinal: &'a [Vec<EdgeId>],
}

/// Chosen graph-level mutation path after resolving canonical edge placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphMutationPath {
    /// The mutation touched canonical base entries on both surfaces.
    Base,
    /// The mutation touched overflow-log entries on both surfaces.
    Overflow,
}

/// Insert policy used by the graph-level runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphInsertPolicy {
    /// Maximum allowed overflow-chain length before insert asks for rebalance.
    pub max_overflow_chain_len: usize,
    /// When true, foreground single-edge inserts prefer overflow over immediate
    /// rebalance until the hard overflow limit is reached.
    pub defer_rebalance_to_maintenance: bool,
    /// Hard overflow-chain limit beyond which foreground inserts must ask for
    /// rebalance before proceeding.
    pub hard_overflow_chain_len: usize,
    /// Radius of the vertex-local rebalance window in vertex ordinals.
    pub rebalance_window_radius: usize,
    /// Minimum live degree at which local rebalance reserves extra base slack.
    pub high_degree_reserve_threshold: u32,
    /// Divisor used to compute extra reserved slack for high-degree vertices.
    pub high_degree_reserve_divisor: u32,
    /// Number of maintenance epochs during which a recently-maintained window
    /// should be de-prioritized.
    pub maintenance_recent_epoch_window: u64,
    /// Score penalty applied for each remaining epoch inside the recent-window.
    pub maintenance_recent_epoch_penalty: u64,
}

impl Default for GraphInsertPolicy {
    fn default() -> Self {
        Self {
            max_overflow_chain_len: 8,
            defer_rebalance_to_maintenance: false,
            hard_overflow_chain_len: 64,
            rebalance_window_radius: 1,
            high_degree_reserve_threshold: 4,
            high_degree_reserve_divisor: 2,
            maintenance_recent_epoch_window: 2,
            maintenance_recent_epoch_penalty: 20_000,
        }
    }
}

impl GraphInsertPolicy {
    /// Estimates reserve targets from a vertex-only window summary.
    ///
    /// This is a lower-bound hint because it does not account for tombstoned
    /// base capacity or exact overflow-entry counts.
    pub fn estimate_vertex_window_reserve_hint(
        self,
        summary: SurfaceVertexWindowSummary,
        anchor_live_degree_after_rebalance: u32,
        incoming_live_entries: u32,
    ) -> Option<SurfaceVertexWindowReserveHint> {
        let live_span_len_lower_bound = u32::try_from(
            summary
                .live_end_exclusive
                .raw
                .checked_sub(summary.base_start.raw)?,
        )
        .ok()?;
        let target_base_len_lower_bound = summary
            .total_live_degree
            .checked_add(incoming_live_entries)?;
        let extra_slots_for_anchor_degree =
            reserve_extra_slots_for_degree(self, anchor_live_degree_after_rebalance);
        let preferred_reserved_base_len_lower_bound =
            target_base_len_lower_bound.checked_add(extra_slots_for_anchor_degree)?;
        let vertex_count = summary
            .end_ordinal_exclusive
            .checked_sub(summary.start_ordinal)?;
        let total_weight =
            u64::from(summary.total_live_degree).checked_add(u64::try_from(vertex_count).ok()?)?;

        Some(SurfaceVertexWindowReserveHint {
            start_ordinal: summary.start_ordinal,
            end_ordinal_exclusive: summary.end_ordinal_exclusive,
            live_span_len_lower_bound,
            target_base_len_lower_bound,
            preferred_reserved_base_len_lower_bound,
            extra_slots_for_anchor_degree,
            total_weight,
            vertices_with_overflow: summary.vertices_with_overflow,
        })
    }
}

/// Chosen graph-level insert decision before mutating either surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphInsertDecision {
    /// The edge can be inserted directly into both canonical base intervals.
    BaseInsert {
        forward_path: EdgeInsertPath,
        reverse_path: EdgeInsertPath,
    },
    /// The edge should be absorbed by both overflow chains.
    Overflow,
    /// Local rebalance should happen before accepting this insert.
    RebalanceRequired(GraphRebalancePlan),
}

/// Result of attempting one graph-level insert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphInsertResult {
    /// The insert was applied immediately.
    Inserted {
        path: EdgeInsertPath,
        locators: EdgePairLogicalLocators,
    },
    /// The insert was not applied because local rebalance is required first.
    RebalanceRequired(GraphRebalancePlan),
}

/// One surface-local rebalance target identified during insert planning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceRebalancePlan {
    /// Vertex whose local neighborhood needs rebalance.
    pub vertex_ref: VertexRef,
    /// Ordinal of the vertex entry inside the directional surface.
    pub ordinal: usize,
    /// Current canonical base degree for that vertex.
    pub base_degree: u32,
    /// Current overflow-chain length for that vertex.
    pub overflow_len: usize,
    /// Number of new live entries the rebalance is planning to absorb.
    pub incoming_live_entries: u32,
}

/// Graph-level rebalance plan produced before mutating either surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphRebalancePlan {
    /// Forward-surface rebalance target.
    pub forward: SurfaceRebalancePlan,
    /// Reverse-surface rebalance target.
    pub reverse: SurfaceRebalancePlan,
}

/// Pure surface-local rebalance window chosen from one surface target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceRebalanceWindowPlan {
    /// Target vertex ordinal around which this local rebalance was planned.
    pub anchor_ordinal: usize,
    /// First vertex ordinal included in the local rebalance window.
    pub start_ordinal: usize,
    /// One-past-the-end vertex ordinal of the local rebalance window.
    pub end_ordinal_exclusive: usize,
    /// Current base-capacity span length covered by the selected window.
    pub current_window_span_len: u32,
    /// Minimum number of entries the rebalance should be able to fit back into base.
    pub target_base_len: u32,
    /// Reserved number of base slots to keep in the rewritten window.
    pub reserved_base_len: u32,
    /// Extra gap budget available after placing all live entries in the window.
    pub gap_budget: u32,
    /// Total VCSR-style placement weight for the window, using `degree + 1`.
    pub total_weight: u64,
    /// Expected weighted placement for the rewritten window.
    pub weighted_layout: SurfaceWeightedWindowLayout,
}

/// Lower-bound reserve estimate derived only from vertex-table records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceVertexWindowReserveHint {
    pub start_ordinal: usize,
    pub end_ordinal_exclusive: usize,
    pub live_span_len_lower_bound: u32,
    pub target_base_len_lower_bound: u32,
    pub preferred_reserved_base_len_lower_bound: u32,
    pub extra_slots_for_anchor_degree: u32,
    pub total_weight: u64,
    pub vertices_with_overflow: usize,
}

/// Cheap maintenance hint for one vertex gathered from both directional surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphMaintenanceCandidate {
    pub vertex_ref: VertexRef,
    pub ordinal: usize,
    pub forward_overflow_len: usize,
    pub reverse_overflow_len: usize,
    pub forward_window_overflow_entries: usize,
    pub reverse_window_overflow_entries: usize,
    pub forward_reclaimable_tombstones: usize,
    pub reverse_reclaimable_tombstones: usize,
    pub forward_window_total_base_slots: usize,
    pub reverse_window_total_base_slots: usize,
    pub last_maintenance_epoch: Option<u64>,
    pub recent_maintenance_penalty: u64,
    pub priority_score: u64,
}

impl GraphMaintenanceCandidate {
    /// Returns whether this candidate has any overflow backlog.
    pub const fn has_overflow_backlog(self) -> bool {
        self.forward_overflow_len > 0 || self.reverse_overflow_len > 0
    }

    /// Returns whether this candidate has any reclaimable tombstoned base slots.
    pub const fn has_reclaimable_tombstones(self) -> bool {
        self.forward_reclaimable_tombstones > 0 || self.reverse_reclaimable_tombstones > 0
    }

    /// Converts one vertex-level candidate into a queueable work item for its
    /// local maintenance window.
    pub fn into_work_item(
        self,
        rebalance_window_radius: usize,
        vertex_count: usize,
    ) -> GraphMaintenanceWorkItem {
        let start_ordinal = self.ordinal.saturating_sub(rebalance_window_radius);
        let end_ordinal_exclusive = self
            .ordinal
            .saturating_add(rebalance_window_radius)
            .saturating_add(1)
            .min(vertex_count);
        GraphMaintenanceWorkItem {
            vertex_ref: self.vertex_ref,
            anchor_ordinal: self.ordinal,
            start_ordinal,
            end_ordinal_exclusive,
            priority_score: self.priority_score,
            last_maintenance_epoch: self.last_maintenance_epoch,
            recent_maintenance_penalty: self.recent_maintenance_penalty,
        }
    }
}

/// Queueable maintenance work item representing one rebalance/compaction window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphMaintenanceWorkItem {
    pub vertex_ref: VertexRef,
    pub anchor_ordinal: usize,
    pub start_ordinal: usize,
    pub end_ordinal_exclusive: usize,
    pub priority_score: u64,
    pub last_maintenance_epoch: Option<u64>,
    pub recent_maintenance_penalty: u64,
}

/// One chosen maintenance cycle before any mutation is applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMaintenanceCyclePlan {
    pub candidate: GraphMaintenanceCandidate,
    pub rebalance: GraphLocalRebalancePlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphMaintenanceQueueStorageSnapshot {
    pub logical_len_bytes: u64,
    pub queue_len: usize,
    pub format_version: Option<u32>,
    pub checksum_valid: Option<bool>,
}

/// Result of executing one maintenance cycle and flushing dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMaintenanceCycleWriteSummary {
    pub candidate: GraphMaintenanceCandidate,
    pub window_start_ordinal: usize,
    pub window_end_ordinal_exclusive: usize,
    pub rebalance: GraphAppliedSegmentRebalanceWriteSummary,
    pub queue_storage_before: Option<GraphMaintenanceQueueStorageSnapshot>,
    pub queue_storage_after: Option<GraphMaintenanceQueueStorageSnapshot>,
}

/// Result of running multiple maintenance cycles plus retired-segment sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMaintenanceBatchWriteSummary {
    pub cycles: Vec<GraphMaintenanceCycleWriteSummary>,
    pub queue_len_before: usize,
    pub queue_len_after: usize,
    pub swept_forward_segments: Vec<EdgeSegmentHeader>,
    pub swept_reverse_segments: Vec<EdgeSegmentHeader>,
    pub queue_storage_before: Option<GraphMaintenanceQueueStorageSnapshot>,
    pub queue_storage_after: Option<GraphMaintenanceQueueStorageSnapshot>,
}

impl SurfaceRebalanceWindowPlan {
    /// Returns the expected exclusive end of the rewritten base-capacity span.
    pub fn expected_base_end_exclusive(&self) -> Option<EdgeIndex> {
        self.weighted_layout.end_exclusive()
    }

    /// Returns the expected total base-capacity span length for the window.
    pub const fn expected_capacity_span_len(&self) -> u32 {
        self.weighted_layout.capacity_span_len()
    }

    /// Returns how many `EdgeEntry` slots this rebalance would shift later
    /// vertex base indices by, relative to the current window span.
    pub fn expected_displacement_against_current_span(&self) -> i64 {
        self.weighted_layout
            .displacement_against_current_span(self.current_window_span_len)
    }
}

/// Pure graph-level local rebalance plan derived from both surface targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphLocalRebalancePlan {
    /// Forward-surface rebalance window.
    pub forward: SurfaceRebalanceWindowPlan,
    /// Reverse-surface rebalance window.
    pub reverse: SurfaceRebalanceWindowPlan,
}

impl GraphLocalRebalancePlan {
    /// Returns the expected displacement of the forward rewritten window.
    pub fn forward_expected_displacement(&self) -> i64 {
        self.forward.expected_displacement_against_current_span()
    }

    /// Returns the expected displacement of the reverse rewritten window.
    pub fn reverse_expected_displacement(&self) -> i64 {
        self.reverse.expected_displacement_against_current_span()
    }

    /// Returns the sum of expected forward and reverse displacement.
    pub fn total_expected_displacement(&self) -> i64 {
        self.forward_expected_displacement() + self.reverse_expected_displacement()
    }

    /// Returns the larger expected displacement across the two surfaces.
    pub fn max_expected_displacement(&self) -> i64 {
        self.forward_expected_displacement()
            .max(self.reverse_expected_displacement())
    }
}

/// Pure graph-level rebalance delta derived from one local rebalance plan.
///
/// This is still metadata only: it describes the rewritten base slices and
/// vertex entries that would result from compacting both directional windows.
/// Applying those changes to runtime state or stable memory is a later step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphLocalRebalanceDelta {
    /// Forward-surface rewrite for the selected local window.
    pub forward: SurfaceLocalRebalanceDelta,
    /// Reverse-surface rewrite for the selected local window.
    pub reverse: SurfaceLocalRebalanceDelta,
}

impl GraphLocalRebalanceDelta {
    /// Returns the rewritten forward capacity span length.
    pub const fn forward_capacity_span_len(&self) -> u32 {
        self.forward.capacity_span_len()
    }

    /// Returns the rewritten reverse capacity span length.
    pub const fn reverse_capacity_span_len(&self) -> u32 {
        self.reverse.capacity_span_len()
    }

    /// Returns the rewritten forward displacement against the planned current span.
    pub fn forward_displacement_against_plan(&self, plan: &GraphLocalRebalancePlan) -> i64 {
        self.forward
            .displacement_against_current_span(plan.forward.current_window_span_len)
    }

    /// Returns the rewritten reverse displacement against the planned current span.
    pub fn reverse_displacement_against_plan(&self, plan: &GraphLocalRebalancePlan) -> i64 {
        self.reverse
            .displacement_against_current_span(plan.reverse.current_window_span_len)
    }

    /// Returns the sum of rewritten forward and reverse displacement.
    pub fn total_displacement_against_plan(&self, plan: &GraphLocalRebalancePlan) -> i64 {
        self.forward_displacement_against_plan(plan) + self.reverse_displacement_against_plan(plan)
    }

    /// Returns the larger rewritten displacement across both surfaces.
    pub fn max_displacement_against_plan(&self, plan: &GraphLocalRebalancePlan) -> i64 {
        self.forward_displacement_against_plan(plan)
            .max(self.reverse_displacement_against_plan(plan))
    }
}

/// Summary produced after applying one graph-level local rebalance delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphAppliedRebalanceSummary {
    pub forward: SurfaceAppliedRebalanceSummary,
    pub reverse: SurfaceAppliedRebalanceSummary,
}

impl GraphAppliedRebalanceSummary {
    /// Returns the sum of applied forward and reverse displacement.
    pub fn total_displacement(&self) -> i64 {
        self.forward.displacement + self.reverse.displacement
    }

    /// Returns the larger applied displacement across both surfaces.
    pub fn max_displacement(&self) -> i64 {
        self.forward.displacement.max(self.reverse.displacement)
    }
}

/// Segment replacement result for one directional surface during maintenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceSegmentReplacementSummary {
    pub new_segment: EdgeSegmentHeader,
    pub retired_segment: Option<EdgeSegmentHeader>,
}

/// Graph-level segment replacement result across both directional surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphSegmentReplacementSummary {
    pub forward: SurfaceSegmentReplacementSummary,
    pub reverse: SurfaceSegmentReplacementSummary,
}

/// Summary produced after applying one graph-level rebalance onto fresh segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphAppliedSegmentRebalanceSummary {
    pub apply: GraphAppliedRebalanceSummary,
    pub segments: GraphSegmentReplacementSummary,
}

/// Result of applying one graph-level rebalance and then flushing dirty regions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphAppliedRebalanceWriteSummary {
    pub apply: GraphAppliedRebalanceSummary,
    pub refreshed_forward_vertices: Vec<usize>,
    pub refreshed_reverse_vertices: Vec<usize>,
}

/// Result of applying one graph-level rebalance onto fresh segments and then
/// flushing dirty regions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphAppliedSegmentRebalanceWriteSummary {
    pub apply: GraphAppliedSegmentRebalanceSummary,
    pub refreshed_forward_vertices: Vec<usize>,
    pub refreshed_reverse_vertices: Vec<usize>,
}

/// Result of ensuring local capacity and flushing resulting dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEnsureCapacityWriteSummary {
    pub rebalanced: bool,
    pub rebalance: Option<GraphAppliedRebalanceWriteSummary>,
    pub refreshed_forward_vertices: Vec<usize>,
    pub refreshed_reverse_vertices: Vec<usize>,
}

/// Result of ensuring local capacity via fresh-segment replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEnsureCapacitySegmentSummary {
    pub rebalanced: bool,
    pub rebalance: Option<GraphAppliedSegmentRebalanceSummary>,
}

/// Result of ensuring local capacity via fresh-segment replacement and then
/// flushing resulting dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEnsureCapacitySegmentWriteSummary {
    pub rebalanced: bool,
    pub rebalance: Option<GraphAppliedSegmentRebalanceWriteSummary>,
    pub refreshed_forward_vertices: Vec<usize>,
    pub refreshed_reverse_vertices: Vec<usize>,
}

/// Result of one insert helper that may rebalance before writing dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphInsertWriteSummary {
    pub insert: Option<GraphInsertResult>,
    pub rebalance: Option<GraphAppliedRebalanceWriteSummary>,
    pub refreshed_forward_vertices: Vec<usize>,
    pub refreshed_reverse_vertices: Vec<usize>,
}

/// Result of one insert helper that may rebalance via fresh segments first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphInsertSegmentSummary {
    pub insert: Option<GraphInsertResult>,
    pub rebalance: Option<GraphAppliedSegmentRebalanceSummary>,
}

/// Result of one insert helper that may rebalance via fresh segments first and
/// then flush dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphInsertSegmentWriteSummary {
    pub insert: Option<GraphInsertResult>,
    pub rebalance: Option<GraphAppliedSegmentRebalanceWriteSummary>,
    pub refreshed_forward_vertices: Vec<usize>,
    pub refreshed_reverse_vertices: Vec<usize>,
}

impl GraphRuntime {
    fn maintenance_candidate_priority_score(
        &self,
        forward_overflow_len: usize,
        reverse_overflow_len: usize,
        forward_window_overflow_entries: usize,
        reverse_window_overflow_entries: usize,
        forward_reclaimable_tombstones: usize,
        reverse_reclaimable_tombstones: usize,
        forward_window_total_base_slots: usize,
        reverse_window_total_base_slots: usize,
        ordinal: usize,
        current_epoch: Option<u64>,
    ) -> Option<u64> {
        let overflow_total = forward_overflow_len.checked_add(reverse_overflow_len)?;
        let window_overflow_total =
            forward_window_overflow_entries.checked_add(reverse_window_overflow_entries)?;
        let tombstone_total =
            forward_reclaimable_tombstones.checked_add(reverse_reclaimable_tombstones)?;
        let window_total_base_slots =
            forward_window_total_base_slots.checked_add(reverse_window_total_base_slots)?;
        let hard_limit_pressure = u64::try_from(overflow_total).ok()?.checked_mul(10_000)?;
        let window_overflow_score = u64::try_from(window_overflow_total)
            .ok()?
            .checked_mul(1_000)?;
        let tombstone_score = u64::try_from(tombstone_total).ok()?.checked_mul(10)?;
        let density_bonus = u64::try_from(window_total_base_slots).ok()?;
        let base_score = hard_limit_pressure
            .checked_add(window_overflow_score)?
            .checked_add(tombstone_score)?
            .checked_add(density_bonus)?;
        let penalty = self.recent_maintenance_penalty_for_ordinal(ordinal, current_epoch);
        Some(base_score.saturating_sub(penalty))
    }

    fn recent_maintenance_penalty_for_ordinal(
        &self,
        ordinal: usize,
        current_epoch: Option<u64>,
    ) -> u64 {
        let Some(current_epoch) = current_epoch else {
            return 0;
        };
        let Some(last_epoch) = self
            .recent_maintenance_epochs_by_ordinal
            .get(&ordinal)
            .copied()
        else {
            return 0;
        };
        let window = self.insert_policy.maintenance_recent_epoch_window;
        if window == 0 {
            return 0;
        }
        let age = current_epoch.saturating_sub(last_epoch);
        if age >= window {
            return 0;
        }
        let remaining = window.saturating_sub(age);
        remaining.saturating_mul(self.insert_policy.maintenance_recent_epoch_penalty)
    }

    pub(crate) fn record_recent_maintenance_window(
        &mut self,
        start_ordinal: usize,
        end_ordinal_exclusive: usize,
        epoch: u64,
    ) {
        for ordinal in start_ordinal..end_ordinal_exclusive {
            self.recent_maintenance_epochs_by_ordinal
                .insert(ordinal, epoch);
        }
    }

    fn cleanup_allocated_edge_segment(
        manager: &mut RegionManager,
        kind: RegionKind,
        segment_id: u32,
    ) {
        if segment_id == 0 {
            return;
        }
        let _ = manager.free_edge_segment(kind, segment_id);
        let _ = manager.reclaim_edge_segment_storage(kind, segment_id);
    }

    fn sync_surface_segment_capacity_from_header(
        surface: &mut super::runtime::SurfaceRuntime,
        header: EdgeSegmentHeader,
    ) {
        surface
            .sync_base_segment_slot_capacity_from_manager(header.segment_id, header.slot_capacity);
    }

    fn sync_surface_segment_capacities_from_manager(
        surface: &mut super::runtime::SurfaceRuntime,
        manager: &RegionManager,
        kind: RegionKind,
    ) -> Option<()> {
        Self::sync_surface_segment_capacity_from_header(surface, manager.edge_segment(kind, 0)?);
        for header in manager.edge_segment_directory(kind)?.iter().copied() {
            Self::sync_surface_segment_capacity_from_header(surface, header);
        }
        Some(())
    }

    pub fn sync_base_segment_capacities_from_manager(
        &mut self,
        manager: &RegionManager,
    ) -> Option<()> {
        Self::sync_surface_segment_capacities_from_manager(
            &mut self.forward.0,
            manager,
            RegionKind::ForwardEdgeEntries,
        )?;
        Self::sync_surface_segment_capacities_from_manager(
            &mut self.reverse.0,
            manager,
            RegionKind::ReverseEdgeEntries,
        )?;
        Some(())
    }

    fn sync_base_segment_capacities_from_manager_best_effort(&mut self, manager: &RegionManager) {
        let _ = self.sync_base_segment_capacities_from_manager(manager);
    }

    fn prepare_surface_segment_replacement(
        surface: &super::runtime::SurfaceRuntime,
        delta: &SurfaceLocalRebalanceDelta,
        new_segment: EdgeSegmentHeader,
    ) -> Option<(
        BTreeMap<u32, Vec<EdgeEntry>>,
        BTreeMap<u32, u64>,
        SurfaceAppliedRebalanceSummary,
        u32,
        Vec<usize>,
    )> {
        if delta.start_ordinal >= delta.end_ordinal_exclusive
            || delta.end_ordinal_exclusive > surface.vertices.len()
            || delta.rewritten_vertices.len()
                != delta
                    .end_ordinal_exclusive
                    .checked_sub(delta.start_ordinal)?
        {
            return None;
        }

        let last_ordinal = delta.end_ordinal_exclusive.checked_sub(1)?;
        let old_first = surface.vertex_entry(delta.start_ordinal)?;
        let old_last = surface.vertex_entry(last_ordinal)?;
        let after_last = surface.vertex_entry(delta.end_ordinal_exclusive);
        let old_segment_id = old_first.segment_id();
        let old_segment_slot_capacity =
            surface.base_entries.segment_slot_capacity(old_segment_id)?;
        let segment_start_ordinal = surface.segment_start_ordinal(delta.start_ordinal)?;
        let segment_end_ordinal = surface.segment_end_ordinal_exclusive(delta.start_ordinal)?;
        let (old_start, old_end) = surface.base_entries.window_span_for_vertices(
            old_first,
            old_last,
            after_last,
            old_segment_slot_capacity,
        )?;
        let old_span_len = u32::try_from(old_end.checked_sub(old_start)?).ok()?;

        let mut previous_index = None;
        for vertex in &delta.rewritten_vertices {
            if vertex.has_overflow() || vertex.segment_id() != new_segment.segment_id {
                return None;
            }
            if let Some(previous) = previous_index {
                if vertex.start_slot() < previous {
                    return None;
                }
            } else if vertex.edge_ref() != delta.base_start.as_edge_ref() {
                return None;
            }
            previous_index = Some(vertex.start_slot() + u64::from(vertex.degree));
        }
        if usize::try_from(delta.reserved_base_len).ok()? != delta.compacted_base_entries.len() {
            return None;
        }

        let (mut segments, mut slot_capacities) =
            surface.materialize_segmented_base_storage_parts()?;
        segments.insert(new_segment.segment_id, delta.compacted_base_entries.clone());
        slot_capacities.insert(new_segment.segment_id, new_segment.slot_capacity);

        Some((
            segments,
            slot_capacities,
            SurfaceAppliedRebalanceSummary {
                start_ordinal: delta.start_ordinal,
                end_ordinal_exclusive: delta.end_ordinal_exclusive,
                old_span_len,
                new_span_len: delta.reserved_base_len,
                displacement: delta.displacement_against_current_span(old_span_len),
            },
            old_segment_id,
            (segment_start_ordinal..segment_end_ordinal).collect(),
        ))
    }

    fn commit_surface_segment_replacement(
        surface: &mut super::runtime::SurfaceRuntime,
        delta: SurfaceLocalRebalanceDelta,
        segments: BTreeMap<u32, Vec<EdgeEntry>>,
        slot_capacities: BTreeMap<u32, u64>,
        dirty_ordinals: Vec<usize>,
    ) {
        surface.replace_base_storage_with_segmented(segments, slot_capacities);
        for (offset, vertex) in delta.rewritten_vertices.into_iter().enumerate() {
            let ordinal = delta.start_ordinal + offset;
            surface.vertices[ordinal] = vertex;
        }
        for ordinal in dirty_ordinals {
            surface.dirty_vertices.insert(ordinal);
        }
        surface.dirty_regions.edge_entries = true;
        surface.dirty_regions.vertex_table = true;
        surface.dirty_regions.vertex_table_suffix_start = None;
    }

    /// Replaces forward and reverse base adjacency with explicit segmented storage (see [`SurfaceRuntime::replace_base_storage_with_segmented`]).
    pub fn replace_base_storages_with_segmented(
        &mut self,
        forward_segments: BTreeMap<u32, Vec<EdgeEntry>>,
        forward_slot_capacities: BTreeMap<u32, u64>,
        reverse_segments: BTreeMap<u32, Vec<EdgeEntry>>,
        reverse_slot_capacities: BTreeMap<u32, u64>,
    ) {
        self.forward
            .0
            .replace_base_storage_with_segmented(forward_segments, forward_slot_capacities);
        self.reverse
            .0
            .replace_base_storage_with_segmented(reverse_segments, reverse_slot_capacities);
    }

    /// Wraps contiguous base backing into segment `0` segmented storage on both surfaces without changing packed [`EdgeRef`] layout for segment `0`.
    pub fn migrate_contiguous_base_to_segment_zero(&mut self) {
        self.forward
            .0
            .migrate_contiguous_base_to_segment_zero();
        self.reverse
            .0
            .migrate_contiguous_base_to_segment_zero();
    }

    /// Starts one batch-mutation session over this graph runtime.
    pub fn begin_batch_mutation<'a, M: Memory>(
        &'a mut self,
        manager: &'a std::cell::RefCell<RegionManager>,
        memory: &'a M,
    ) -> GraphBatchMutationSession<'a, M> {
        GraphBatchMutationSession::new(self, manager, memory)
    }

    fn prefers_local_rebalance_before_overflow(&self, plan: &GraphRebalancePlan) -> Option<bool> {
        let local = self.plan_local_rebalance(*plan)?;
        let forward_summary = self.forward.0.summarize_window_slack(
            local.forward.start_ordinal,
            local.forward.end_ordinal_exclusive,
        )?;
        let reverse_summary = self.reverse.0.summarize_window_slack(
            local.reverse.start_ordinal,
            local.reverse.end_ordinal_exclusive,
        )?;
        Some(
            forward_summary.can_absorb_additional_live_entries(
                usize::try_from(plan.forward.incoming_live_entries).ok()?,
            ) && reverse_summary.can_absorb_additional_live_entries(
                usize::try_from(plan.reverse.incoming_live_entries).ok()?,
            ),
        )
    }

    /// Collects cheap maintenance candidates for timer-driven rebalance or compaction.
    ///
    /// Candidates are returned in descending priority order, preferring overflow
    /// backlog first and reclaimable tombstones second.
    pub fn collect_maintenance_candidates(
        &self,
        vertex_ids: &[VertexRef],
    ) -> Option<Vec<GraphMaintenanceCandidate>> {
        self.collect_maintenance_candidates_at_epoch(vertex_ids, None)
    }

    /// Collects maintenance candidates while applying a fairness penalty to
    /// windows maintained recently relative to `current_epoch`.
    pub fn collect_maintenance_candidates_at_epoch(
        &self,
        vertex_ids: &[VertexRef],
        current_epoch: Option<u64>,
    ) -> Option<Vec<GraphMaintenanceCandidate>> {
        let vertex_count = vertex_ids
            .len()
            .min(self.forward.0.vertices.len())
            .min(self.reverse.0.vertices.len());
        let mut candidates = Vec::new();
        for (ordinal, &vertex) in vertex_ids.iter().take(vertex_count).enumerate() {
            let window_start = ordinal.saturating_sub(self.insert_policy.rebalance_window_radius);
            let window_end_exclusive = ordinal
                .saturating_add(self.insert_policy.rebalance_window_radius)
                .saturating_add(1)
                .min(vertex_count);
            let forward_window_summary = self
                .forward
                .0
                .summarize_window_slack(window_start, window_end_exclusive)?;
            let reverse_window_summary = self
                .reverse
                .0
                .summarize_window_slack(window_start, window_end_exclusive)?;
            let forward_overflow_len = self
                .forward
                .overflow_entries_for(vertex, ordinal)?
                .len();
            let reverse_overflow_len = self
                .reverse
                .overflow_entries_for(vertex, ordinal)?
                .len();
            let forward_reclaimable_tombstones = forward_window_summary.reclaimable_tombstones;
            let reverse_reclaimable_tombstones = reverse_window_summary.reclaimable_tombstones;
            let forward_window_overflow_entries = forward_window_summary.overflow_entries_in_window;
            let reverse_window_overflow_entries = reverse_window_summary.overflow_entries_in_window;
            let forward_window_total_base_slots = forward_window_summary.total_base_slots;
            let reverse_window_total_base_slots = reverse_window_summary.total_base_slots;
            if forward_overflow_len == 0
                && reverse_overflow_len == 0
                && forward_reclaimable_tombstones == 0
                && reverse_reclaimable_tombstones == 0
            {
                continue;
            }
            candidates.push(GraphMaintenanceCandidate {
                vertex_ref: vertex,
                ordinal,
                forward_overflow_len,
                reverse_overflow_len,
                forward_window_overflow_entries,
                reverse_window_overflow_entries,
                forward_reclaimable_tombstones,
                reverse_reclaimable_tombstones,
                forward_window_total_base_slots,
                reverse_window_total_base_slots,
                last_maintenance_epoch: self
                    .recent_maintenance_epochs_by_ordinal
                    .get(&ordinal)
                    .copied(),
                recent_maintenance_penalty: self
                    .recent_maintenance_penalty_for_ordinal(ordinal, current_epoch),
                priority_score: self.maintenance_candidate_priority_score(
                    forward_overflow_len,
                    reverse_overflow_len,
                    forward_window_overflow_entries,
                    reverse_window_overflow_entries,
                    forward_reclaimable_tombstones,
                    reverse_reclaimable_tombstones,
                    forward_window_total_base_slots,
                    reverse_window_total_base_slots,
                    ordinal,
                    current_epoch,
                )?,
            });
        }
        candidates.sort_by(|left, right| {
            right
                .priority_score
                .cmp(&left.priority_score)
                .then_with(|| left.ordinal.cmp(&right.ordinal))
        });
        Some(candidates)
    }

    /// Collects deduplicated maintenance work items keyed by local rebalance
    /// window so timer code can queue work without rescanning every vertex.
    pub fn collect_maintenance_work_items_at_epoch(
        &self,
        vertex_ids: &[VertexRef],
        current_epoch: Option<u64>,
    ) -> Option<Vec<GraphMaintenanceWorkItem>> {
        let vertex_count = vertex_ids
            .len()
            .min(self.forward.0.vertices.len())
            .min(self.reverse.0.vertices.len());
        let candidates = self.collect_maintenance_candidates_at_epoch(vertex_ids, current_epoch)?;
        let mut by_window: BTreeMap<(usize, usize), GraphMaintenanceWorkItem> = BTreeMap::new();
        for candidate in candidates {
            let item =
                candidate.into_work_item(self.insert_policy.rebalance_window_radius, vertex_count);
            by_window
                .entry((item.start_ordinal, item.end_ordinal_exclusive))
                .and_modify(|existing| {
                    if item.priority_score > existing.priority_score
                        || (item.priority_score == existing.priority_score
                            && item.anchor_ordinal < existing.anchor_ordinal)
                    {
                        *existing = item;
                    }
                })
                .or_insert(item);
        }
        let mut items: Vec<_> = by_window.into_values().collect();
        items.sort_by(|left, right| {
            right
                .priority_score
                .cmp(&left.priority_score)
                .then_with(|| left.anchor_ordinal.cmp(&right.anchor_ordinal))
        });
        Some(items)
    }

    pub fn collect_maintenance_work_items(
        &self,
        vertex_ids: &[VertexRef],
    ) -> Option<Vec<GraphMaintenanceWorkItem>> {
        self.collect_maintenance_work_items_at_epoch(vertex_ids, None)
    }

    fn refresh_maintenance_work_item_at_epoch(
        &self,
        work_item: GraphMaintenanceWorkItem,
        vertex_ids: &[VertexRef],
        current_epoch: Option<u64>,
    ) -> Option<GraphMaintenanceWorkItem> {
        let vertex_count = vertex_ids
            .len()
            .min(self.forward.0.vertices.len())
            .min(self.reverse.0.vertices.len());
        if work_item.anchor_ordinal >= vertex_count
            || work_item.start_ordinal >= work_item.end_ordinal_exclusive
            || work_item.end_ordinal_exclusive > vertex_count
        {
            return None;
        }
        let vertex = *vertex_ids.get(work_item.anchor_ordinal)?;
        let forward_window_summary = self
            .forward
            .0
            .summarize_window_slack(work_item.start_ordinal, work_item.end_ordinal_exclusive)?;
        let reverse_window_summary = self
            .reverse
            .0
            .summarize_window_slack(work_item.start_ordinal, work_item.end_ordinal_exclusive)?;
        let forward_overflow_len = self
            .forward
            .overflow_entries_for(vertex, work_item.anchor_ordinal)?
            .len();
        let reverse_overflow_len = self
            .reverse
            .overflow_entries_for(vertex, work_item.anchor_ordinal)?
            .len();
        let forward_reclaimable_tombstones = forward_window_summary.reclaimable_tombstones;
        let reverse_reclaimable_tombstones = reverse_window_summary.reclaimable_tombstones;
        if forward_overflow_len == 0
            && reverse_overflow_len == 0
            && forward_reclaimable_tombstones == 0
            && reverse_reclaimable_tombstones == 0
        {
            return None;
        }
        Some(GraphMaintenanceWorkItem {
            vertex_ref: vertex,
            anchor_ordinal: work_item.anchor_ordinal,
            start_ordinal: work_item.start_ordinal,
            end_ordinal_exclusive: work_item.end_ordinal_exclusive,
            priority_score: self.maintenance_candidate_priority_score(
                forward_overflow_len,
                reverse_overflow_len,
                forward_window_summary.overflow_entries_in_window,
                reverse_window_summary.overflow_entries_in_window,
                forward_reclaimable_tombstones,
                reverse_reclaimable_tombstones,
                forward_window_summary.total_base_slots,
                reverse_window_summary.total_base_slots,
                work_item.anchor_ordinal,
                current_epoch,
            )?,
            last_maintenance_epoch: self
                .recent_maintenance_epochs_by_ordinal
                .get(&work_item.anchor_ordinal)
                .copied(),
            recent_maintenance_penalty: self
                .recent_maintenance_penalty_for_ordinal(work_item.anchor_ordinal, current_epoch),
        })
    }

    /// Rebuilds the retained maintenance queue from current runtime state.
    pub fn rebuild_maintenance_queue_at_epoch(
        &mut self,
        vertex_ids: &[VertexRef],
        current_epoch: Option<u64>,
    ) -> Option<usize> {
        self.maintenance_queue =
            self.collect_maintenance_work_items_at_epoch(vertex_ids, current_epoch)?;
        Some(self.maintenance_queue.len())
    }

    pub fn rebuild_maintenance_queue(&mut self, vertex_ids: &[VertexRef]) -> Option<usize> {
        self.rebuild_maintenance_queue_at_epoch(vertex_ids, None)
    }

    /// Re-scores the retained maintenance queue against current runtime state,
    /// dropping windows that no longer need work.
    pub fn refresh_maintenance_queue_at_epoch(
        &mut self,
        vertex_ids: &[VertexRef],
        current_epoch: Option<u64>,
    ) -> Option<usize> {
        let mut refreshed = Vec::with_capacity(self.maintenance_queue.len());
        for work_item in self.maintenance_queue.iter().copied() {
            if let Some(updated) =
                self.refresh_maintenance_work_item_at_epoch(work_item, vertex_ids, current_epoch)
            {
                refreshed.push(updated);
            }
        }
        refreshed.sort_by(|left, right| {
            right
                .priority_score
                .cmp(&left.priority_score)
                .then_with(|| left.anchor_ordinal.cmp(&right.anchor_ordinal))
        });
        self.maintenance_queue = refreshed;
        Some(self.maintenance_queue.len())
    }

    pub fn refresh_maintenance_queue(&mut self, vertex_ids: &[VertexRef]) -> Option<usize> {
        self.refresh_maintenance_queue_at_epoch(vertex_ids, None)
    }

    /// Returns a snapshot of the retained maintenance queue.
    pub fn maintenance_queue(&self) -> &[GraphMaintenanceWorkItem] {
        &self.maintenance_queue
    }

    pub(crate) fn replace_maintenance_queue(
        &mut self,
        maintenance_queue: Vec<GraphMaintenanceWorkItem>,
    ) {
        self.maintenance_queue = maintenance_queue;
    }

    /// Pops the next queued maintenance work item, if any.
    pub fn pop_next_maintenance_work_item(&mut self) -> Option<GraphMaintenanceWorkItem> {
        if self.maintenance_queue.is_empty() {
            None
        } else {
            Some(self.maintenance_queue.remove(0))
        }
    }

    /// Chooses the highest-priority maintenance cycle that can be planned
    /// from the current in-memory state.
    pub fn plan_one_maintenance_cycle(
        &self,
        vertex_ids: &[VertexRef],
    ) -> Option<GraphMaintenanceCyclePlan> {
        self.plan_one_maintenance_cycle_at_epoch(vertex_ids, None)
    }

    /// Chooses the highest-priority maintenance cycle, penalizing windows that
    /// were maintained recently relative to `current_epoch`.
    pub fn plan_one_maintenance_cycle_at_epoch(
        &self,
        vertex_ids: &[VertexRef],
        current_epoch: Option<u64>,
    ) -> Option<GraphMaintenanceCyclePlan> {
        let item = self
            .collect_maintenance_work_items_at_epoch(vertex_ids, current_epoch)?
            .into_iter()
            .next()?;
        self.plan_maintenance_cycle_from_work_item(item)
    }

    /// Builds one maintenance cycle plan directly from a queued work item.
    pub fn plan_maintenance_cycle_from_work_item(
        &self,
        work_item: GraphMaintenanceWorkItem,
    ) -> Option<GraphMaintenanceCyclePlan> {
        let forward_vertex = self.forward.0.vertex_entry(work_item.anchor_ordinal)?;
        let reverse_vertex = self.reverse.0.vertex_entry(work_item.anchor_ordinal)?;
        let candidate = GraphMaintenanceCandidate {
            vertex_ref: work_item.vertex_ref,
            ordinal: work_item.anchor_ordinal,
            forward_overflow_len: self
                .forward
                .overflow_entries_for(work_item.vertex_ref, work_item.anchor_ordinal)?
                .len(),
            reverse_overflow_len: self
                .reverse
                .overflow_entries_for(work_item.vertex_ref, work_item.anchor_ordinal)?
                .len(),
            forward_window_overflow_entries: self
                .forward
                .0
                .summarize_window_slack(work_item.start_ordinal, work_item.end_ordinal_exclusive)?
                .overflow_entries_in_window,
            reverse_window_overflow_entries: self
                .reverse
                .0
                .summarize_window_slack(work_item.start_ordinal, work_item.end_ordinal_exclusive)?
                .overflow_entries_in_window,
            forward_reclaimable_tombstones: self
                .forward
                .0
                .summarize_window_slack(work_item.start_ordinal, work_item.end_ordinal_exclusive)?
                .reclaimable_tombstones,
            reverse_reclaimable_tombstones: self
                .reverse
                .0
                .summarize_window_slack(work_item.start_ordinal, work_item.end_ordinal_exclusive)?
                .reclaimable_tombstones,
            forward_window_total_base_slots: self
                .forward
                .0
                .summarize_window_slack(work_item.start_ordinal, work_item.end_ordinal_exclusive)?
                .total_base_slots,
            reverse_window_total_base_slots: self
                .reverse
                .0
                .summarize_window_slack(work_item.start_ordinal, work_item.end_ordinal_exclusive)?
                .total_base_slots,
            last_maintenance_epoch: work_item.last_maintenance_epoch,
            recent_maintenance_penalty: work_item.recent_maintenance_penalty,
            priority_score: work_item.priority_score,
        };
        let rebalance = self.plan_local_rebalance(GraphRebalancePlan {
            forward: SurfaceRebalancePlan {
                vertex_ref: work_item.vertex_ref,
                ordinal: work_item.anchor_ordinal,
                base_degree: forward_vertex.degree,
                overflow_len: candidate.forward_overflow_len,
                incoming_live_entries: 0,
            },
            reverse: SurfaceRebalancePlan {
                vertex_ref: work_item.vertex_ref,
                ordinal: work_item.anchor_ordinal,
                base_degree: reverse_vertex.degree,
                overflow_len: candidate.reverse_overflow_len,
                incoming_live_entries: 0,
            },
        })?;
        Some(GraphMaintenanceCyclePlan {
            candidate,
            rebalance,
        })
    }

    fn apply_local_rebalance_delta_to_surfaces(
        &mut self,
        delta: GraphLocalRebalanceDelta,
    ) -> Option<GraphAppliedRebalanceSummary> {
        let forward = self.forward.0.apply_local_rebalance_delta(delta.forward)?;
        let reverse = self.reverse.0.apply_local_rebalance_delta(delta.reverse)?;
        Some(GraphAppliedRebalanceSummary { forward, reverse })
    }

    /// Creates a thin graph-level runtime from forward/reverse surfaces with
    /// empty physical/logical sidecars.
    pub fn new_with_empty_sidecars(
        forward: ForwardSurfaceRuntime,
        reverse: ReverseSurfaceRuntime,
    ) -> Self {
        Self::with_insert_policy_and_empty_sidecars(forward, reverse, GraphInsertPolicy::default())
    }

    /// Creates a thin graph-level runtime with an explicit insert policy and
    /// empty physical/logical sidecars.
    pub fn with_insert_policy_and_empty_sidecars(
        forward: ForwardSurfaceRuntime,
        reverse: ReverseSurfaceRuntime,
        insert_policy: GraphInsertPolicy,
    ) -> Self {
        Self {
            forward,
            reverse,
            logical_locator_sidecar: EdgeLogicalLocatorSidecar::new(),
            insert_policy,
            recent_maintenance_epochs_by_ordinal: BTreeMap::new(),
            maintenance_queue: Vec::new(),
        }
    }

    /// Replaces the canonical logical-locator sidecar as one unit.
    pub fn replace_logical_locator_sidecar(
        &mut self,
        logical_locator_sidecar: EdgeLogicalLocatorSidecar,
    ) {
        self.logical_locator_sidecar = logical_locator_sidecar;
    }

    /// Appends one empty vertex slot to both directional surfaces.
    ///
    /// This is the minimal adjacency-side bootstrap step before any edge is
    /// inserted. It keeps forward and reverse ordinals aligned.
    pub fn append_empty_vertex_pair(&mut self) -> Option<(usize, usize)> {
        let forward = self.forward.append_empty_vertex()?;
        let reverse = self.reverse.append_empty_vertex()?;
        self.recent_maintenance_epochs_by_ordinal.remove(&forward);
        self.maintenance_queue.clear();
        Some((forward, reverse))
    }

    /// Appends `count` empty vertex slot pairs to both directional surfaces.
    ///
    /// This keeps forward and reverse ordinals aligned for each appended pair.
    pub fn append_empty_vertex_pairs(&mut self, count: usize) -> Option<Vec<(usize, usize)>> {
        let mut ordinals = Vec::with_capacity(count);
        for _ in 0..count {
            ordinals.push(self.append_empty_vertex_pair()?);
        }
        Some(ordinals)
    }

    /// Builds a pure rebalance plan for one logical insert candidate.
    pub fn plan_rebalance_for_insert(
        &self,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
    ) -> Option<GraphRebalancePlan> {
        self.plan_rebalance_for_insert_with_incoming_live_entries(
            src_vertex_ref,
            src_ordinal,
            dst_vertex_ref,
            dst_ordinal,
            1,
        )
    }

    /// Builds a pure rebalance plan for an insert-like mutation that would add
    /// `incoming_live_entries` new live base candidates to both surfaces.
    pub fn plan_rebalance_for_insert_with_incoming_live_entries(
        &self,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        incoming_live_entries: u32,
    ) -> Option<GraphRebalancePlan> {
        let forward_vertex = self.forward.0.vertex_entry(src_ordinal)?;
        let reverse_vertex = self.reverse.0.vertex_entry(dst_ordinal)?;
        let forward_overflow_len = self
            .forward
            .overflow_entries_for(src_vertex_ref, src_ordinal)?
            .len();
        let reverse_overflow_len = self
            .reverse
            .overflow_entries_for(dst_vertex_ref, dst_ordinal)?
            .len();

        Some(GraphRebalancePlan {
            forward: SurfaceRebalancePlan {
                vertex_ref: src_vertex_ref,
                ordinal: src_ordinal,
                base_degree: forward_vertex.degree,
                overflow_len: forward_overflow_len,
                incoming_live_entries,
            },
            reverse: SurfaceRebalancePlan {
                vertex_ref: dst_vertex_ref,
                ordinal: dst_ordinal,
                base_degree: reverse_vertex.degree,
                overflow_len: reverse_overflow_len,
                incoming_live_entries,
            },
        })
    }

    /// Refines a rebalance target into concrete local windows on both surfaces.
    pub fn plan_local_rebalance(
        &self,
        plan: GraphRebalancePlan,
    ) -> Option<GraphLocalRebalancePlan> {
        Some(GraphLocalRebalancePlan {
            forward: self.plan_surface_local_rebalance(
                &self.forward.0,
                self.forward.0.vertices.len(),
                plan.forward,
            )?,
            reverse: self.plan_surface_local_rebalance(
                &self.reverse.0,
                self.reverse.0.vertices.len(),
                plan.reverse,
            )?,
        })
    }

    fn plan_surface_local_rebalance(
        &self,
        surface: &super::runtime::SurfaceRuntime,
        vertex_count: usize,
        plan: SurfaceRebalancePlan,
    ) -> Option<SurfaceRebalanceWindowPlan> {
        if plan.ordinal >= vertex_count {
            return None;
        }

        let radius = self.insert_policy.rebalance_window_radius;
        let start_ordinal = plan.ordinal.saturating_sub(radius);
        let end_ordinal_exclusive = plan
            .ordinal
            .saturating_add(radius)
            .saturating_add(1)
            .min(vertex_count);
        let summary = surface.summarize_window_slack(start_ordinal, end_ordinal_exclusive)?;
        let live_base_entries = u32::try_from(
            summary
                .total_base_slots
                .checked_sub(summary.reclaimable_tombstones)?,
        )
        .ok()?;
        let target_base_len = live_base_entries
            .checked_add(u32::try_from(summary.overflow_entries_in_window).ok()?)?
            .checked_add(plan.incoming_live_entries)?;
        let target_vertex_live_after_rebalance = plan
            .base_degree
            .checked_add(u32::try_from(plan.overflow_len).ok()?)?
            .checked_add(plan.incoming_live_entries)?;
        let current_window_capacity = u32::try_from(summary.total_base_slots).ok()?;
        let preferred_reserved_base_len = target_base_len
            .checked_add(self.reserve_extra_slots_for_degree(target_vertex_live_after_rebalance))?;
        let reserved_base_len = current_window_capacity.max(preferred_reserved_base_len);
        let gap_budget = reserved_base_len.checked_sub(target_base_len)?;
        let total_weight: u64 = (start_ordinal..end_ordinal_exclusive)
            .map(|ordinal| {
                surface
                    .vertex_entry(ordinal)
                    .map(|vertex| u64::from(vertex.degree) + 1)
            })
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .sum();
        let weighted_layout = surface.build_weighted_window_layout(
            plan.ordinal,
            start_ordinal,
            end_ordinal_exclusive,
            reserved_base_len,
        )?;

        Some(SurfaceRebalanceWindowPlan {
            anchor_ordinal: plan.ordinal,
            start_ordinal,
            end_ordinal_exclusive,
            current_window_span_len: current_window_capacity,
            target_base_len,
            reserved_base_len,
            gap_budget,
            total_weight,
            weighted_layout,
        })
    }

    fn reserve_extra_slots_for_degree(&self, live_degree: u32) -> u32 {
        reserve_extra_slots_for_degree(self.insert_policy, live_degree)
    }

    /// Builds the pure per-surface rewrite delta for one previously planned
    /// local rebalance window.
    pub fn build_local_rebalance_delta(
        &self,
        plan: GraphLocalRebalancePlan,
    ) -> Option<GraphLocalRebalanceDelta> {
        Some(GraphLocalRebalanceDelta {
            forward: self.forward.0.build_local_rebalance_delta(
                plan.forward.anchor_ordinal,
                plan.forward.start_ordinal,
                plan.forward.end_ordinal_exclusive,
                plan.forward.reserved_base_len,
            )?,
            reverse: self.reverse.0.build_local_rebalance_delta(
                plan.reverse.anchor_ordinal,
                plan.reverse.start_ordinal,
                plan.reverse.end_ordinal_exclusive,
                plan.reverse.reserved_base_len,
            )?,
        })
    }

    /// Applies one previously built local-rebalance delta to both surfaces.
    ///
    /// This rewrites the directional base slices and vertex metadata, then
    /// conservatively clears the canonical logical locator sidecar until a
    /// fresh semantic mapping is materialized.
    pub fn apply_local_rebalance_delta(
        &mut self,
        delta: GraphLocalRebalanceDelta,
    ) -> Option<GraphAppliedRebalanceSummary> {
        let summary = self.apply_local_rebalance_delta_to_surfaces(delta)?;
        self.logical_locator_sidecar = EdgeLogicalLocatorSidecar::new();
        Some(summary)
    }

    /// Applies one local-rebalance delta onto fresh explicit segments.
    ///
    /// **Canonical maintenance path:** structural reshaping should prefer this over
    /// [`Self::apply_local_rebalance_delta`], which splices base storage in place on the
    /// segments the graph already uses. Here, forward/reverse windows are retargeted to newly
    /// allocated active segments, the in-memory rewrite runs as **segment-local contiguous
    /// windows**, then previously backing explicit segments are retired. Segment `0`
    /// remains the root segment and is never retired.
    ///
    /// See also: [`SurfaceBaseStorage::rewrite_vertex_window_span`] for the per-surface
    /// in-place window rewrite primitive (used by [`SurfaceRuntime::apply_local_rebalance_delta`]).
    pub fn apply_local_rebalance_delta_with_segment_replacement(
        &mut self,
        delta: GraphLocalRebalanceDelta,
        manager: &mut RegionManager,
        retired_epoch: u64,
    ) -> Option<GraphAppliedSegmentRebalanceSummary> {
        let forward_new_segment = manager.allocate_edge_segment(
            RegionKind::ForwardEdgeEntries,
            u64::from(delta.forward.capacity_span_len()),
            EdgeSegmentState::Active,
        )?;
        let reverse_new_segment = match manager.allocate_edge_segment(
            RegionKind::ReverseEdgeEntries,
            u64::from(delta.reverse.capacity_span_len()),
            EdgeSegmentState::Active,
        ) {
            Some(segment) => segment,
            None => {
                Self::cleanup_allocated_edge_segment(
                    manager,
                    RegionKind::ForwardEdgeEntries,
                    forward_new_segment.segment_id,
                );
                return None;
            }
        };

        let retargeted_delta = GraphLocalRebalanceDelta {
            forward: delta
                .forward
                .retargeted_to_segment(forward_new_segment.segment_id),
            reverse: delta
                .reverse
                .retargeted_to_segment(reverse_new_segment.segment_id),
        };

        let (
            forward_segments,
            forward_slot_capacities,
            forward_apply,
            forward_old_segment_id,
            forward_dirty_ordinals,
        ) = match Self::prepare_surface_segment_replacement(
            &self.forward.0,
            &retargeted_delta.forward,
            forward_new_segment,
        ) {
            Some(prepared) => prepared,
            None => {
                Self::cleanup_allocated_edge_segment(
                    manager,
                    RegionKind::ForwardEdgeEntries,
                    forward_new_segment.segment_id,
                );
                Self::cleanup_allocated_edge_segment(
                    manager,
                    RegionKind::ReverseEdgeEntries,
                    reverse_new_segment.segment_id,
                );
                return None;
            }
        };
        let (
            reverse_segments,
            reverse_slot_capacities,
            reverse_apply,
            reverse_old_segment_id,
            reverse_dirty_ordinals,
        ) = match Self::prepare_surface_segment_replacement(
            &self.reverse.0,
            &retargeted_delta.reverse,
            reverse_new_segment,
        ) {
            Some(prepared) => prepared,
            None => {
                Self::cleanup_allocated_edge_segment(
                    manager,
                    RegionKind::ForwardEdgeEntries,
                    forward_new_segment.segment_id,
                );
                Self::cleanup_allocated_edge_segment(
                    manager,
                    RegionKind::ReverseEdgeEntries,
                    reverse_new_segment.segment_id,
                );
                return None;
            }
        };

        Self::commit_surface_segment_replacement(
            &mut self.forward.0,
            retargeted_delta.forward,
            forward_segments,
            forward_slot_capacities,
            forward_dirty_ordinals,
        );
        Self::commit_surface_segment_replacement(
            &mut self.reverse.0,
            retargeted_delta.reverse,
            reverse_segments,
            reverse_slot_capacities,
            reverse_dirty_ordinals,
        );

        let apply = GraphAppliedRebalanceSummary {
            forward: forward_apply,
            reverse: reverse_apply,
        };

        let retired_forward_segment = if forward_old_segment_id == 0 {
            None
        } else {
            manager.retire_edge_segment(
                RegionKind::ForwardEdgeEntries,
                forward_old_segment_id,
                retired_epoch,
            )?;
            manager.edge_segment(RegionKind::ForwardEdgeEntries, forward_old_segment_id)
        };
        let retired_reverse_segment = if reverse_old_segment_id == 0 {
            None
        } else {
            manager.retire_edge_segment(
                RegionKind::ReverseEdgeEntries,
                reverse_old_segment_id,
                retired_epoch,
            )?;
            manager.edge_segment(RegionKind::ReverseEdgeEntries, reverse_old_segment_id)
        };

        self.logical_locator_sidecar = EdgeLogicalLocatorSidecar::new();

        Some(GraphAppliedSegmentRebalanceSummary {
            apply,
            segments: GraphSegmentReplacementSummary {
                forward: SurfaceSegmentReplacementSummary {
                    new_segment: forward_new_segment,
                    retired_segment: retired_forward_segment,
                },
                reverse: SurfaceSegmentReplacementSummary {
                    new_segment: reverse_new_segment,
                    retired_segment: retired_reverse_segment,
                },
            },
        })
    }

    /// Applies one local-rebalance delta and then rebuilds the canonical
    /// forward logical-locator sidecar from externally supplied semantic edge ids.
    ///
    /// The caller must provide forward-surface vertex ids and base edge ids for
    /// every vertex ordinal in forward-surface order.
    pub fn apply_local_rebalance_delta_and_rebuild_logical_locator_sidecar(
        &mut self,
        delta: GraphLocalRebalanceDelta,
        forward_vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<GraphAppliedRebalanceSummary> {
        let summary = self.apply_local_rebalance_delta_to_surfaces(delta)?;
        self.logical_locator_sidecar = self
            .forward
            .0
            .build_logical_locator_sidecar_from_vertex_base_ids(
                forward_vertex_ids,
                forward_base_edge_ids_by_ordinal,
            )?;
        Some(summary)
    }

    /// Applies one local-rebalance delta and refreshes only the affected
    /// forward-side logical-locator mappings.
    ///
    /// The caller supplies semantic forward vertex ids and base edge ids only
    /// for the rewritten forward window.
    pub fn apply_local_rebalance_delta_and_refresh_logical_locator_sidecar_window(
        &mut self,
        delta: GraphLocalRebalanceDelta,
        forward_vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<GraphAppliedRebalanceSummary> {
        let start_ordinal = delta.forward.start_ordinal;
        let end_ordinal_exclusive = delta.forward.end_ordinal_exclusive;
        if forward_vertex_ids.len() != end_ordinal_exclusive.checked_sub(start_ordinal)?
            || forward_base_edge_ids_by_ordinal.len() != forward_vertex_ids.len()
        {
            return None;
        }

        let summary = self.apply_local_rebalance_delta_to_surfaces(delta)?;

        self.logical_locator_sidecar.retain(|_, locator| {
            !(locator.surface_kind() == super::edge::SurfaceKind::Forward
                && forward_vertex_ids.contains(&locator.vertex_ref))
        });

        self.forward.0.populate_logical_locator_sidecar_for_window(
            start_ordinal,
            forward_vertex_ids,
            forward_base_edge_ids_by_ordinal,
            &mut self.logical_locator_sidecar,
        )?;
        Some(summary)
    }

    /// Applies a local rebalance delta, refreshes the affected forward-side
    /// locator window, then rebuilds dirty label sidecars and flushes dirty
    /// regions to stable memory.
    pub fn apply_local_rebalance_delta_refresh_window_and_write(
        &mut self,
        delta: GraphLocalRebalanceDelta,
        forward_vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<GraphAppliedRebalanceWriteSummary, WritebackError> {
        let apply = self
            .apply_local_rebalance_delta_and_refresh_logical_locator_sidecar_window(
                delta,
                forward_vertex_ids,
                forward_base_edge_ids_by_ordinal,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.refresh_and_write_dirty_to_stable_memory(manager, memory)?;
        Ok(GraphAppliedRebalanceWriteSummary {
            apply,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        })
    }

    /// Applies a local rebalance delta onto fresh segments, refreshes the
    /// affected forward-side locator window, then flushes dirty regions.
    pub fn apply_local_rebalance_delta_with_segment_replacement_refresh_window_and_write(
        &mut self,
        delta: GraphLocalRebalanceDelta,
        forward_vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<GraphAppliedSegmentRebalanceWriteSummary, WritebackError> {
        let start_ordinal = delta.forward.start_ordinal;
        let end_ordinal_exclusive = delta.forward.end_ordinal_exclusive;
        if forward_vertex_ids.len()
            != end_ordinal_exclusive.checked_sub(start_ordinal).ok_or(
                WritebackError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
            )?
            || forward_base_edge_ids_by_ordinal.len() != forward_vertex_ids.len()
        {
            return Err(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ));
        }

        let apply = self
            .apply_local_rebalance_delta_with_segment_replacement(delta, manager, retired_epoch)
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;

        self.forward
            .0
            .populate_logical_locator_sidecar_for_window(
                start_ordinal,
                forward_vertex_ids,
                forward_base_edge_ids_by_ordinal,
                &mut self.logical_locator_sidecar,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;

        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.refresh_and_write_dirty_to_stable_memory(manager, memory)?;
        Ok(GraphAppliedSegmentRebalanceWriteSummary {
            apply,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        })
    }

    /// Runs one timer-style maintenance cycle on the highest-priority candidate.
    ///
    /// The caller supplies full forward vertex ids and semantic base edge ids
    /// for every forward ordinal so the selected window can refresh the
    /// canonical locator sidecar after segment replacement.
    pub fn run_one_maintenance_cycle_with_segment_replacement_and_write(
        &mut self,
        vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<Option<GraphMaintenanceCycleWriteSummary>, WritebackError> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let plan = match self.plan_one_maintenance_cycle_at_epoch(vertex_ids, Some(retired_epoch)) {
            Some(plan) => plan,
            None => return Ok(None),
        };
        self.run_maintenance_cycle_from_plan_with_segment_replacement_and_write(
            plan,
            vertex_ids,
            forward_base_edge_ids_by_ordinal,
            manager,
            memory,
            retired_epoch,
        )
        .map(Some)
    }

    /// Runs one timer-style maintenance cycle from an explicit queued work item.
    pub fn run_one_maintenance_cycle_from_work_item_with_segment_replacement_and_write(
        &mut self,
        work_item: GraphMaintenanceWorkItem,
        vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<Option<GraphMaintenanceCycleWriteSummary>, WritebackError> {
        let Some(plan) = self.plan_maintenance_cycle_from_work_item(work_item) else {
            return Ok(None);
        };
        self.run_maintenance_cycle_from_plan_with_segment_replacement_and_write(
            plan,
            vertex_ids,
            forward_base_edge_ids_by_ordinal,
            manager,
            memory,
            retired_epoch,
        )
        .map(Some)
    }

    /// Pops and runs the next queued maintenance work item, if any.
    pub fn run_next_queued_maintenance_cycle_with_segment_replacement_and_write(
        &mut self,
        vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<Option<GraphMaintenanceCycleWriteSummary>, WritebackError> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let Some(work_item) = self.pop_next_maintenance_work_item() else {
            return Ok(None);
        };
        self.run_one_maintenance_cycle_from_work_item_with_segment_replacement_and_write(
            work_item,
            vertex_ids,
            forward_base_edge_ids_by_ordinal,
            manager,
            memory,
            retired_epoch,
        )
    }

    fn run_maintenance_cycle_from_plan_with_segment_replacement_and_write(
        &mut self,
        plan: GraphMaintenanceCyclePlan,
        vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<GraphMaintenanceCycleWriteSummary, WritebackError> {
        let delta = self
            .build_local_rebalance_delta(plan.rebalance.clone())
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;
        let start = plan.rebalance.forward.start_ordinal;
        let end = plan.rebalance.forward.end_ordinal_exclusive;
        let forward_vertex_ids =
            vertex_ids
                .get(start..end)
                .ok_or(WritebackError::MissingRegionDefinition(
                    RegionKind::ForwardEdgeEntries,
                ))?;
        let forward_base_edge_ids = forward_base_edge_ids_by_ordinal.get(start..end).ok_or(
            WritebackError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
        )?;
        let rebalance = self
            .apply_local_rebalance_delta_with_segment_replacement_refresh_window_and_write(
                delta,
                forward_vertex_ids,
                forward_base_edge_ids,
                manager,
                memory,
                retired_epoch,
            )?;
        self.record_recent_maintenance_window(
            plan.rebalance.forward.start_ordinal,
            plan.rebalance.forward.end_ordinal_exclusive,
            retired_epoch,
        );
        Ok(GraphMaintenanceCycleWriteSummary {
            candidate: plan.candidate,
            window_start_ordinal: plan.rebalance.forward.start_ordinal,
            window_end_ordinal_exclusive: plan.rebalance.forward.end_ordinal_exclusive,
            rebalance,
            queue_storage_before: None,
            queue_storage_after: None,
        })
    }

    /// Runs up to `max_cycles` timer-style maintenance cycles and then sweeps
    /// retired explicit segments old enough to reclaim.
    pub fn run_maintenance_cycles_with_segment_replacement_and_write(
        &mut self,
        vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
        retired_epoch: u64,
        max_cycles: usize,
        min_retired_epochs_before_sweep: u64,
    ) -> Result<GraphMaintenanceBatchWriteSummary, WritebackError> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let mut cycles = Vec::new();
        for step in 0..max_cycles {
            let current_epoch =
                retired_epoch.saturating_add(u64::try_from(step).unwrap_or(u64::MAX));
            let Some(summary) = self.run_one_maintenance_cycle_with_segment_replacement_and_write(
                vertex_ids,
                forward_base_edge_ids_by_ordinal,
                manager,
                memory,
                current_epoch,
            )?
            else {
                break;
            };
            cycles.push(summary);
        }

        let swept_forward_segments = manager
            .sweep_retired_edge_segments(
                RegionKind::ForwardEdgeEntries,
                retired_epoch,
                min_retired_epochs_before_sweep,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;
        let swept_reverse_segments = manager
            .sweep_retired_edge_segments(
                RegionKind::ReverseEdgeEntries,
                retired_epoch,
                min_retired_epochs_before_sweep,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ReverseEdgeEntries,
            ))?;

        Ok(GraphMaintenanceBatchWriteSummary {
            cycles,
            queue_len_before: 0,
            queue_len_after: 0,
            swept_forward_segments,
            swept_reverse_segments,
            queue_storage_before: None,
            queue_storage_after: None,
        })
    }

    /// Runs up to `max_cycles` maintenance cycles from the retained queue,
    /// refreshing remaining work items after each successful cycle.
    pub fn run_queued_maintenance_cycles_with_segment_replacement_and_write(
        &mut self,
        vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
        retired_epoch: u64,
        max_cycles: usize,
        min_retired_epochs_before_sweep: u64,
    ) -> Result<GraphMaintenanceBatchWriteSummary, WritebackError> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let queue_len_before = self.maintenance_queue.len();
        let mut cycles = Vec::new();
        for step in 0..max_cycles {
            let current_epoch =
                retired_epoch.saturating_add(u64::try_from(step).unwrap_or(u64::MAX));
            let next = self.run_next_queued_maintenance_cycle_with_segment_replacement_and_write(
                vertex_ids,
                forward_base_edge_ids_by_ordinal,
                manager,
                memory,
                current_epoch,
            );
            let Some(summary) = (match next {
                Ok(summary) => summary,
                Err(WritebackError::MissingRegionDefinition(_)) => {
                    let _ =
                        self.refresh_maintenance_queue_at_epoch(vertex_ids, Some(current_epoch));
                    continue;
                }
                Err(err) => return Err(err),
            }) else {
                break;
            };
            cycles.push(summary);
            let _ = self.refresh_maintenance_queue_at_epoch(vertex_ids, Some(current_epoch));
        }

        let swept_forward_segments = manager
            .sweep_retired_edge_segments(
                RegionKind::ForwardEdgeEntries,
                retired_epoch,
                min_retired_epochs_before_sweep,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;
        let swept_reverse_segments = manager
            .sweep_retired_edge_segments(
                RegionKind::ReverseEdgeEntries,
                retired_epoch,
                min_retired_epochs_before_sweep,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ReverseEdgeEntries,
            ))?;

        Ok(GraphMaintenanceBatchWriteSummary {
            cycles,
            queue_len_before,
            queue_len_after: self.maintenance_queue.len(),
            swept_forward_segments,
            swept_reverse_segments,
            queue_storage_before: None,
            queue_storage_after: None,
        })
    }

    /// Applies a local rebalance delta, rebuilds the full forward canonical
    /// logical-locator sidecar, then rebuilds dirty label sidecars and flushes
    /// dirty regions to stable memory.
    pub fn apply_local_rebalance_delta_rebuild_logical_locator_sidecar_and_write(
        &mut self,
        delta: GraphLocalRebalanceDelta,
        forward_vertex_ids: &[VertexRef],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<GraphAppliedRebalanceWriteSummary, WritebackError> {
        let apply = self
            .apply_local_rebalance_delta_and_rebuild_logical_locator_sidecar(
                delta,
                forward_vertex_ids,
                forward_base_edge_ids_by_ordinal,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;
        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            self.refresh_and_write_dirty_to_stable_memory(manager, memory)?;
        Ok(GraphAppliedRebalanceWriteSummary {
            apply,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        })
    }

    /// Ensures local capacity for an upcoming batch of live entries without
    /// applying any concrete edge insert yet.
    ///
    /// Returns `true` when one local rebalance cycle was applied, and `false`
    /// when the current surfaces are already ready for the requested batch.
    pub fn ensure_local_capacity_for_incoming_live_entries(
        &mut self,
        spec: RebalancePrepareSpec<'_>,
    ) -> Option<bool> {
        let decision = self.choose_insert_decision_with_incoming_live_entries(
            spec.endpoints.src_vertex_ref,
            spec.endpoints.src_ordinal,
            spec.endpoints.dst_vertex_ref,
            spec.endpoints.dst_ordinal,
            spec.planned_incoming_live_entries,
        )?;
        let GraphInsertDecision::RebalanceRequired(plan) = decision else {
            return Some(false);
        };

        let local = self.plan_local_rebalance(plan)?;
        let delta = self.build_local_rebalance_delta(local)?;
        self.apply_local_rebalance_delta_and_refresh_logical_locator_sidecar_window(
            delta,
            spec.forward_rebalance_vertex_ids,
            spec.forward_rebalance_base_edge_ids_by_ordinal,
        )?;
        Some(true)
    }

    /// Ensures local capacity by rewriting onto fresh backing segments when a
    /// rebalance is required.
    pub fn ensure_local_capacity_for_incoming_live_entries_with_segment_replacement(
        &mut self,
        spec: RebalancePrepareSpec<'_>,
        manager: &mut RegionManager,
        retired_epoch: u64,
    ) -> Option<GraphEnsureCapacitySegmentSummary> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let decision = self.choose_insert_decision_with_incoming_live_entries_and_manager(
            spec.endpoints.src_vertex_ref,
            spec.endpoints.src_ordinal,
            spec.endpoints.dst_vertex_ref,
            spec.endpoints.dst_ordinal,
            spec.planned_incoming_live_entries,
            manager,
        )?;
        let rebalance = match decision {
            GraphInsertDecision::RebalanceRequired(plan) => {
                let local = self.plan_local_rebalance(plan)?;
                let delta = self.build_local_rebalance_delta(local)?;
                let apply = self.apply_local_rebalance_delta_with_segment_replacement(
                    delta,
                    manager,
                    retired_epoch,
                )?;
                self.forward.0.populate_logical_locator_sidecar_for_window(
                    apply.apply.forward.start_ordinal,
                    spec.forward_rebalance_vertex_ids,
                    spec.forward_rebalance_base_edge_ids_by_ordinal,
                    &mut self.logical_locator_sidecar,
                )?;
                Some(apply)
            }
            _ => None,
        };
        Some(GraphEnsureCapacitySegmentSummary {
            rebalanced: rebalance.is_some(),
            rebalance,
        })
    }

    /// Ensures local capacity for an upcoming batch of live entries and writes
    /// back any resulting dirty state to stable memory.
    pub fn ensure_local_capacity_for_incoming_live_entries_and_write(
        &mut self,
        spec: RebalancePrepareSpec<'_>,
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<GraphEnsureCapacityWriteSummary, WritebackError> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let decision = self
            .choose_insert_decision_with_incoming_live_entries_and_manager(
                spec.endpoints.src_vertex_ref,
                spec.endpoints.src_ordinal,
                spec.endpoints.dst_vertex_ref,
                spec.endpoints.dst_ordinal,
                spec.planned_incoming_live_entries,
                manager,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;

        let rebalance = match decision {
            GraphInsertDecision::RebalanceRequired(plan) => {
                let local = self.plan_local_rebalance(plan).ok_or(
                    WritebackError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
                )?;
                let delta = self.build_local_rebalance_delta(local).ok_or(
                    WritebackError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
                )?;
                Some(self.apply_local_rebalance_delta_refresh_window_and_write(
                    delta,
                    spec.forward_rebalance_vertex_ids,
                    spec.forward_rebalance_base_edge_ids_by_ordinal,
                    manager,
                    memory,
                )?)
            }
            _ => None,
        };

        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            if let Some(summary) = &rebalance {
                (
                    summary.refreshed_forward_vertices.clone(),
                    summary.refreshed_reverse_vertices.clone(),
                )
            } else if self.forward.0.has_dirty_regions() || self.reverse.0.has_dirty_regions() {
                self.refresh_and_write_dirty_to_stable_memory(manager, memory)?
            } else {
                (Vec::new(), Vec::new())
            };

        Ok(GraphEnsureCapacityWriteSummary {
            rebalanced: rebalance.is_some(),
            rebalance,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        })
    }

    /// Ensures local capacity via fresh-segment replacement and writes back
    /// any resulting dirty state to stable memory.
    pub fn ensure_local_capacity_for_incoming_live_entries_with_segment_replacement_and_write(
        &mut self,
        spec: RebalancePrepareSpec<'_>,
        manager: &mut RegionManager,
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<GraphEnsureCapacitySegmentWriteSummary, WritebackError> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let decision = self
            .choose_insert_decision_with_incoming_live_entries_and_manager(
                spec.endpoints.src_vertex_ref,
                spec.endpoints.src_ordinal,
                spec.endpoints.dst_vertex_ref,
                spec.endpoints.dst_ordinal,
                spec.planned_incoming_live_entries,
                manager,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;

        let rebalance = match decision {
            GraphInsertDecision::RebalanceRequired(plan) => {
                let local = self.plan_local_rebalance(plan).ok_or(
                    WritebackError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
                )?;
                let delta = self.build_local_rebalance_delta(local).ok_or(
                    WritebackError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
                )?;
                Some(
                    self.apply_local_rebalance_delta_with_segment_replacement_refresh_window_and_write(
                        delta,
                        spec.forward_rebalance_vertex_ids,
                        spec.forward_rebalance_base_edge_ids_by_ordinal,
                        manager,
                        memory,
                        retired_epoch,
                    )?,
                )
            }
            _ => None,
        };

        let (refreshed_forward_vertices, refreshed_reverse_vertices) =
            if let Some(summary) = &rebalance {
                (
                    summary.refreshed_forward_vertices.clone(),
                    summary.refreshed_reverse_vertices.clone(),
                )
            } else if self.forward.0.has_dirty_regions() || self.reverse.0.has_dirty_regions() {
                self.refresh_and_write_dirty_to_stable_memory(manager, memory)?
            } else {
                (Vec::new(), Vec::new())
            };

        Ok(GraphEnsureCapacitySegmentWriteSummary {
            rebalanced: rebalance.is_some(),
            rebalance,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        })
    }

    /// Inserts one logical edge, performing one local rebalance cycle first if
    /// the current insert decision requires it.
    ///
    /// The caller supplies semantic forward vertex ids and base edge ids for
    /// the forward rebalance window after compaction and before the new edge is
    /// inserted.
    pub fn insert_edge_pair_with_local_rebalance(
        &mut self,
        mut spec: RebalanceInsertSpec<'_>,
    ) -> Option<GraphInsertResult> {
        spec.planned_incoming_live_entries = 1;
        self.insert_edge_pair_with_local_rebalance_for_incoming_live_entries(spec)
    }

    /// Inserts one logical edge while planning for a larger incoming live-entry batch.
    pub fn insert_edge_pair_with_local_rebalance_for_incoming_live_entries(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
    ) -> Option<GraphInsertResult> {
        match self.choose_insert_decision_with_incoming_live_entries(
            spec.endpoints.src_vertex_ref,
            spec.endpoints.src_ordinal,
            spec.endpoints.dst_vertex_ref,
            spec.endpoints.dst_ordinal,
            spec.planned_incoming_live_entries,
        )? {
            GraphInsertDecision::RebalanceRequired(plan) => {
                let local = self.plan_local_rebalance(plan)?;
                let delta = self.build_local_rebalance_delta(local)?;
                self.apply_local_rebalance_delta_and_refresh_logical_locator_sidecar_window(
                    delta,
                    spec.forward_rebalance_vertex_ids,
                    spec.forward_rebalance_base_edge_ids_by_ordinal,
                )?;
                self.insert_edge_pair(
                    spec.edge_id,
                    spec.endpoints.src_vertex_ref,
                    spec.endpoints.src_ordinal,
                    spec.endpoints.dst_vertex_ref,
                    spec.endpoints.dst_ordinal,
                    spec.label_id,
                )
            }
            _ => self.insert_edge_pair(
                spec.edge_id,
                spec.endpoints.src_vertex_ref,
                spec.endpoints.src_ordinal,
                spec.endpoints.dst_vertex_ref,
                spec.endpoints.dst_ordinal,
                spec.label_id,
            ),
        }
    }

    /// Inserts one logical edge while using fresh-segment replacement when a
    /// local rebalance is required.
    pub fn insert_edge_pair_with_local_rebalance_and_segment_replacement(
        &mut self,
        mut spec: RebalanceInsertSpec<'_>,
        manager: &mut RegionManager,
        retired_epoch: u64,
    ) -> Option<GraphInsertSegmentSummary> {
        spec.planned_incoming_live_entries = 1;
        self.insert_edge_pair_with_local_rebalance_and_segment_replacement_for_incoming_live_entries(
            spec,
            manager,
            retired_epoch,
        )
    }

    /// Inserts one logical edge while planning for a larger incoming batch and
    /// using fresh-segment replacement when rebalance is required.
    pub fn insert_edge_pair_with_local_rebalance_and_segment_replacement_for_incoming_live_entries(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
        manager: &mut RegionManager,
        retired_epoch: u64,
    ) -> Option<GraphInsertSegmentSummary> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let rebalance = match self.choose_insert_decision_with_incoming_live_entries_and_manager(
            spec.endpoints.src_vertex_ref,
            spec.endpoints.src_ordinal,
            spec.endpoints.dst_vertex_ref,
            spec.endpoints.dst_ordinal,
            spec.planned_incoming_live_entries,
            manager,
        )? {
            GraphInsertDecision::RebalanceRequired(plan) => {
                let local = self.plan_local_rebalance(plan)?;
                let delta = self.build_local_rebalance_delta(local)?;
                Some(self.apply_local_rebalance_delta_with_segment_replacement(
                    delta,
                    manager,
                    retired_epoch,
                )?)
            }
            _ => None,
        };

        if rebalance.is_some() {
            self.forward.0.populate_logical_locator_sidecar_for_window(
                spec.endpoints.src_ordinal,
                spec.forward_rebalance_vertex_ids,
                spec.forward_rebalance_base_edge_ids_by_ordinal,
                &mut self.logical_locator_sidecar,
            )?;
        }

        let insert = self.insert_edge_pair_with_manager(
            spec.edge_id,
            spec.endpoints.src_vertex_ref,
            spec.endpoints.src_ordinal,
            spec.endpoints.dst_vertex_ref,
            spec.endpoints.dst_ordinal,
            spec.label_id,
            manager,
        );

        Some(GraphInsertSegmentSummary { insert, rebalance })
    }

    /// Inserts one logical edge, performing one local rebalance cycle first if
    /// needed, then refreshes dirty label sidecars and flushes dirty regions to
    /// stable memory.
    pub fn insert_edge_pair_with_local_rebalance_and_write(
        &mut self,
        mut spec: RebalanceInsertSpec<'_>,
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        spec.planned_incoming_live_entries = 1;
        self.insert_edge_pair_with_local_rebalance_and_write_for_incoming_live_entries(
            spec, manager, memory,
        )
    }

    /// Inserts one logical edge and writes back dirty state, while planning
    /// local rebalance as if a larger batch were about to arrive.
    pub fn insert_edge_pair_with_local_rebalance_and_write_for_incoming_live_entries(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let decision = self
            .choose_insert_decision_with_incoming_live_entries_and_manager(
                spec.endpoints.src_vertex_ref,
                spec.endpoints.src_ordinal,
                spec.endpoints.dst_vertex_ref,
                spec.endpoints.dst_ordinal,
                spec.planned_incoming_live_entries,
                manager,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;

        let rebalance = match decision {
            GraphInsertDecision::RebalanceRequired(plan) => {
                let local = self.plan_local_rebalance(plan).ok_or(
                    WritebackError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
                )?;
                let delta = self.build_local_rebalance_delta(local).ok_or(
                    WritebackError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
                )?;
                Some(self.apply_local_rebalance_delta_refresh_window_and_write(
                    delta,
                    spec.forward_rebalance_vertex_ids,
                    spec.forward_rebalance_base_edge_ids_by_ordinal,
                    manager,
                    memory,
                )?)
            }
            _ => None,
        };

        let insert = self.insert_edge_pair_with_manager(
            spec.edge_id,
            spec.endpoints.src_vertex_ref,
            spec.endpoints.src_ordinal,
            spec.endpoints.dst_vertex_ref,
            spec.endpoints.dst_ordinal,
            spec.label_id,
            manager,
        );

        let mut refreshed_forward_vertices = rebalance
            .as_ref()
            .map(|summary| summary.refreshed_forward_vertices.clone())
            .unwrap_or_default();
        let mut refreshed_reverse_vertices = rebalance
            .as_ref()
            .map(|summary| summary.refreshed_reverse_vertices.clone())
            .unwrap_or_default();
        if self.forward.0.has_dirty_regions() || self.reverse.0.has_dirty_regions() {
            let (more_forward, more_reverse) =
                self.refresh_and_write_dirty_to_stable_memory(manager, memory)?;
            refreshed_forward_vertices.extend(more_forward);
            refreshed_reverse_vertices.extend(more_reverse);
        }
        refreshed_forward_vertices.sort_unstable();
        refreshed_forward_vertices.dedup();
        refreshed_reverse_vertices.sort_unstable();
        refreshed_reverse_vertices.dedup();

        Ok(GraphInsertWriteSummary {
            insert,
            rebalance,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        })
    }

    /// Inserts one logical edge, performing fresh-segment replacement first if
    /// needed, then refreshes dirty label sidecars and flushes dirty regions.
    pub fn insert_edge_pair_with_local_rebalance_and_segment_replacement_and_write(
        &mut self,
        mut spec: RebalanceInsertSpec<'_>,
        manager: &mut RegionManager,
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<GraphInsertSegmentWriteSummary, WritebackError> {
        spec.planned_incoming_live_entries = 1;
        self.insert_edge_pair_with_local_rebalance_and_segment_replacement_and_write_for_incoming_live_entries(
            spec,
            manager,
            memory,
            retired_epoch,
        )
    }

    /// Inserts one logical edge and writes back dirty state, while using
    /// fresh-segment replacement when local rebalance is required.
    pub fn insert_edge_pair_with_local_rebalance_and_segment_replacement_and_write_for_incoming_live_entries(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
        manager: &mut RegionManager,
        memory: &impl Memory,
        retired_epoch: u64,
    ) -> Result<GraphInsertSegmentWriteSummary, WritebackError> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let decision = self
            .choose_insert_decision_with_incoming_live_entries_and_manager(
                spec.endpoints.src_vertex_ref,
                spec.endpoints.src_ordinal,
                spec.endpoints.dst_vertex_ref,
                spec.endpoints.dst_ordinal,
                spec.planned_incoming_live_entries,
                manager,
            )
            .ok_or(WritebackError::MissingRegionDefinition(
                RegionKind::ForwardEdgeEntries,
            ))?;

        let rebalance = match decision {
            GraphInsertDecision::RebalanceRequired(plan) => {
                let local = self.plan_local_rebalance(plan).ok_or(
                    WritebackError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
                )?;
                let delta = self.build_local_rebalance_delta(local).ok_or(
                    WritebackError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
                )?;
                Some(self.apply_local_rebalance_delta_with_segment_replacement_refresh_window_and_write(
                    delta,
                    spec.forward_rebalance_vertex_ids,
                    spec.forward_rebalance_base_edge_ids_by_ordinal,
                    manager,
                    memory,
                    retired_epoch,
                )?)
            }
            _ => None,
        };

        let insert = self.insert_edge_pair_with_manager(
            spec.edge_id,
            spec.endpoints.src_vertex_ref,
            spec.endpoints.src_ordinal,
            spec.endpoints.dst_vertex_ref,
            spec.endpoints.dst_ordinal,
            spec.label_id,
            manager,
        );

        let mut refreshed_forward_vertices = rebalance
            .as_ref()
            .map(|summary| summary.refreshed_forward_vertices.clone())
            .unwrap_or_default();
        let mut refreshed_reverse_vertices = rebalance
            .as_ref()
            .map(|summary| summary.refreshed_reverse_vertices.clone())
            .unwrap_or_default();
        if self.forward.0.has_dirty_regions() || self.reverse.0.has_dirty_regions() {
            let (more_forward, more_reverse) =
                self.refresh_and_write_dirty_to_stable_memory(manager, memory)?;
            refreshed_forward_vertices.extend(more_forward);
            refreshed_reverse_vertices.extend(more_reverse);
        }
        refreshed_forward_vertices.sort_unstable();
        refreshed_forward_vertices.dedup();
        refreshed_reverse_vertices.sort_unstable();
        refreshed_reverse_vertices.dedup();

        Ok(GraphInsertSegmentWriteSummary {
            insert,
            rebalance,
            refreshed_forward_vertices,
            refreshed_reverse_vertices,
        })
    }

    /// Chooses the current insertion decision for a new logical edge.
    ///
    /// The base path is taken only when both directional surfaces can append at
    /// the tail of the corresponding canonical base interval without shifting
    /// later base entries. Otherwise the runtime uses overflow until the local
    /// chain length reaches the policy limit, after which it asks for rebalance.
    pub fn choose_insert_decision(
        &self,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
    ) -> Option<GraphInsertDecision> {
        self.choose_insert_decision_with_incoming_live_entries(
            src_vertex_ref,
            src_ordinal,
            dst_vertex_ref,
            dst_ordinal,
            1,
        )
    }

    fn insert_edge_pair_for_decision(
        &mut self,
        edge_id: EdgeId,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        label_id: LabelId,
        decision: GraphInsertDecision,
    ) -> Option<GraphInsertResult> {
        let (path, locators) = match decision {
            GraphInsertDecision::BaseInsert {
                forward_path,
                reverse_path,
            } => {
                let (actual_forward_path, actual_reverse_path, locators) = self
                    .insert_base_edge_pair(
                        edge_id,
                        src_vertex_ref,
                        src_ordinal,
                        dst_vertex_ref,
                        dst_ordinal,
                        label_id,
                    )?;
                debug_assert_eq!(actual_forward_path, forward_path);
                debug_assert_eq!(actual_reverse_path, reverse_path);
                (actual_forward_path, locators)
            }
            GraphInsertDecision::Overflow => (
                EdgeInsertPath::Overflow,
                self.append_overflow_edge_pair(
                    edge_id,
                    src_vertex_ref,
                    src_ordinal,
                    dst_vertex_ref,
                    dst_ordinal,
                    label_id,
                )?,
            ),
            GraphInsertDecision::RebalanceRequired(plan) => {
                return Some(GraphInsertResult::RebalanceRequired(plan));
            }
        };
        Some(GraphInsertResult::Inserted { path, locators })
    }

    /// Chooses the current insertion decision for a mutation that would add
    /// `incoming_live_entries` new live entries to both directional surfaces.
    pub fn choose_insert_decision_with_incoming_live_entries(
        &self,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        incoming_live_entries: u32,
    ) -> Option<GraphInsertDecision> {
        let forward_base = self.forward.choose_base_insert_slot(src_ordinal);
        let reverse_base = self.reverse.choose_base_insert_slot(dst_ordinal);
        if incoming_live_entries == 1
            && let (Some(forward_base), Some(reverse_base)) = (forward_base, reverse_base)
        {
            let forward_path = match forward_base {
                BaseInsertDecision::Append { logical_index } => {
                    EdgeInsertPath::BaseAppend { logical_index }
                }
                BaseInsertDecision::ReuseTombstone { logical_index } => {
                    EdgeInsertPath::BaseReuseTombstone { logical_index }
                }
            };
            let reverse_path = match reverse_base {
                BaseInsertDecision::Append { logical_index } => {
                    EdgeInsertPath::BaseAppend { logical_index }
                }
                BaseInsertDecision::ReuseTombstone { logical_index } => {
                    EdgeInsertPath::BaseReuseTombstone { logical_index }
                }
            };
            return Some(GraphInsertDecision::BaseInsert {
                forward_path,
                reverse_path,
            });
        }

        let forward_overflow_len = self
            .forward
            .overflow_entries_for(src_vertex_ref, src_ordinal)?
            .len();
        let reverse_overflow_len = self
            .reverse
            .overflow_entries_for(dst_vertex_ref, dst_ordinal)?
            .len();
        let rebalance_plan = self.plan_rebalance_for_insert_with_incoming_live_entries(
            src_vertex_ref,
            src_ordinal,
            dst_vertex_ref,
            dst_ordinal,
            incoming_live_entries,
        )?;
        let within_soft_overflow_limit = forward_overflow_len
            < self.insert_policy.max_overflow_chain_len
            && reverse_overflow_len < self.insert_policy.max_overflow_chain_len;
        let within_hard_overflow_limit = forward_overflow_len
            < self.insert_policy.hard_overflow_chain_len
            && reverse_overflow_len < self.insert_policy.hard_overflow_chain_len;

        if incoming_live_entries == 1
            && self.insert_policy.defer_rebalance_to_maintenance
            && within_hard_overflow_limit
        {
            return Some(GraphInsertDecision::Overflow);
        }

        if self.prefers_local_rebalance_before_overflow(&rebalance_plan)? {
            return Some(GraphInsertDecision::RebalanceRequired(rebalance_plan));
        }
        if incoming_live_entries == 1 && within_soft_overflow_limit {
            Some(GraphInsertDecision::Overflow)
        } else {
            Some(GraphInsertDecision::RebalanceRequired(rebalance_plan))
        }
    }

    /// Chooses the current insertion decision while resolving base slot
    /// availability against manager-backed segment capacities.
    pub fn choose_insert_decision_with_incoming_live_entries_and_manager(
        &self,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        incoming_live_entries: u32,
        manager: &RegionManager,
    ) -> Option<GraphInsertDecision> {
        let forward_base = self
            .forward
            .choose_base_insert_slot_with(src_ordinal, |segment_id| {
                manager
                    .edge_segment(RegionKind::ForwardEdgeEntries, segment_id)
                    .map(|segment| segment.slot_capacity)
            });
        let reverse_base = self
            .reverse
            .choose_base_insert_slot_with(dst_ordinal, |segment_id| {
                manager
                    .edge_segment(RegionKind::ReverseEdgeEntries, segment_id)
                    .map(|segment| segment.slot_capacity)
            });
        if incoming_live_entries == 1
            && let (Some(forward_base), Some(reverse_base)) = (forward_base, reverse_base)
        {
            let forward_path = match forward_base {
                BaseInsertDecision::Append { logical_index } => {
                    EdgeInsertPath::BaseAppend { logical_index }
                }
                BaseInsertDecision::ReuseTombstone { logical_index } => {
                    EdgeInsertPath::BaseReuseTombstone { logical_index }
                }
            };
            let reverse_path = match reverse_base {
                BaseInsertDecision::Append { logical_index } => {
                    EdgeInsertPath::BaseAppend { logical_index }
                }
                BaseInsertDecision::ReuseTombstone { logical_index } => {
                    EdgeInsertPath::BaseReuseTombstone { logical_index }
                }
            };
            return Some(GraphInsertDecision::BaseInsert {
                forward_path,
                reverse_path,
            });
        }

        let forward_overflow_len = self
            .forward
            .overflow_entries_for(src_vertex_ref, src_ordinal)?
            .len();
        let reverse_overflow_len = self
            .reverse
            .overflow_entries_for(dst_vertex_ref, dst_ordinal)?
            .len();
        let rebalance_plan = self.plan_rebalance_for_insert_with_incoming_live_entries(
            src_vertex_ref,
            src_ordinal,
            dst_vertex_ref,
            dst_ordinal,
            incoming_live_entries,
        )?;
        let within_soft_overflow_limit = forward_overflow_len
            < self.insert_policy.max_overflow_chain_len
            && reverse_overflow_len < self.insert_policy.max_overflow_chain_len;
        let within_hard_overflow_limit = forward_overflow_len
            < self.insert_policy.hard_overflow_chain_len
            && reverse_overflow_len < self.insert_policy.hard_overflow_chain_len;

        if incoming_live_entries == 1
            && self.insert_policy.defer_rebalance_to_maintenance
            && within_hard_overflow_limit
        {
            return Some(GraphInsertDecision::Overflow);
        }

        if self.prefers_local_rebalance_before_overflow(&rebalance_plan)? {
            return Some(GraphInsertDecision::RebalanceRequired(rebalance_plan));
        }
        if incoming_live_entries == 1 && within_soft_overflow_limit {
            Some(GraphInsertDecision::Overflow)
        } else {
            Some(GraphInsertDecision::RebalanceRequired(rebalance_plan))
        }
    }

    /// Appends one logical edge directly to the tail of both canonical base intervals.
    pub fn insert_base_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        label_id: LabelId,
    ) -> Option<(EdgeInsertPath, EdgeInsertPath, EdgePairLogicalLocators)> {
        let forward_entry = EdgeEntry::new(dst_vertex_ref, EdgeMeta::new(label_id, false));
        let reverse_entry = EdgeEntry::new(src_vertex_ref, EdgeMeta::new(label_id, false));

        let forward_path = self.forward.insert_base_entry(src_ordinal, forward_entry)?;
        let reverse_path = self.reverse.insert_base_entry(dst_ordinal, reverse_entry)?;

        let forward_logical_index = match forward_path {
            EdgeInsertPath::BaseAppend { logical_index }
            | EdgeInsertPath::BaseReuseTombstone { logical_index } => logical_index,
            EdgeInsertPath::Overflow => return None,
        };
        let reverse_logical_index = match reverse_path {
            EdgeInsertPath::BaseAppend { logical_index }
            | EdgeInsertPath::BaseReuseTombstone { logical_index } => logical_index,
            EdgeInsertPath::Overflow => return None,
        };

        let forward_logical_locator = self.forward.logical_edge_locator_for(
            src_vertex_ref,
            src_ordinal,
            forward_logical_index,
        )?;
        self.logical_locator_sidecar
            .set(edge_id, forward_logical_locator);
        Some((
            forward_path,
            reverse_path,
            EdgePairLogicalLocators {
                forward: forward_logical_locator,
                reverse: self.reverse.logical_edge_locator_for(
                    dst_vertex_ref,
                    dst_ordinal,
                    reverse_logical_index,
                )?,
            },
        ))
    }

    /// Inserts one logical edge directly into both canonical base intervals
    /// while resolving base-capacity against manager-backed segment metadata.
    pub fn insert_base_edge_pair_with_manager(
        &mut self,
        edge_id: EdgeId,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        label_id: LabelId,
        manager: &RegionManager,
    ) -> Option<(EdgeInsertPath, EdgeInsertPath, EdgePairLogicalLocators)> {
        let forward_entry = EdgeEntry::new(dst_vertex_ref, EdgeMeta::new(label_id, false));
        let reverse_entry = EdgeEntry::new(src_vertex_ref, EdgeMeta::new(label_id, false));

        let forward_path =
            self.forward
                .insert_base_entry_with(src_ordinal, forward_entry, |segment_id| {
                    manager
                        .edge_segment(RegionKind::ForwardEdgeEntries, segment_id)
                        .map(|segment| segment.slot_capacity)
                })?;
        let reverse_path =
            self.reverse
                .insert_base_entry_with(dst_ordinal, reverse_entry, |segment_id| {
                    manager
                        .edge_segment(RegionKind::ReverseEdgeEntries, segment_id)
                        .map(|segment| segment.slot_capacity)
                })?;

        let forward_logical_index = match forward_path {
            EdgeInsertPath::BaseAppend { logical_index }
            | EdgeInsertPath::BaseReuseTombstone { logical_index } => logical_index,
            EdgeInsertPath::Overflow => return None,
        };
        let reverse_logical_index = match reverse_path {
            EdgeInsertPath::BaseAppend { logical_index }
            | EdgeInsertPath::BaseReuseTombstone { logical_index } => logical_index,
            EdgeInsertPath::Overflow => return None,
        };

        let forward_logical_locator = self.forward.logical_edge_locator_for(
            src_vertex_ref,
            src_ordinal,
            forward_logical_index,
        )?;
        self.logical_locator_sidecar
            .set(edge_id, forward_logical_locator);
        Some((
            forward_path,
            reverse_path,
            EdgePairLogicalLocators {
                forward: forward_logical_locator,
                reverse: self.reverse.logical_edge_locator_for(
                    dst_vertex_ref,
                    dst_ordinal,
                    reverse_logical_index,
                )?,
            },
        ))
    }

    /// Appends one logical edge as paired overflow entries on both directional surfaces.
    pub fn append_overflow_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        label_id: LabelId,
    ) -> Option<EdgePairLogicalLocators> {
        let forward_entry = EdgeEntry::new(dst_vertex_ref, EdgeMeta::new(label_id, false));
        let reverse_entry = EdgeEntry::new(src_vertex_ref, EdgeMeta::new(label_id, false));

        let _forward_offset = self.forward.append_overflow_entry(
            src_vertex_ref,
            src_ordinal,
            edge_id,
            forward_entry,
        )?;
        let reverse_offset = self.reverse.append_overflow_entry(
            dst_vertex_ref,
            dst_ordinal,
            edge_id,
            reverse_entry,
        )?;

        let _ = reverse_offset;
        self.logical_locator_sidecar.set(
            edge_id,
            super::edge::LogicalEdgeLocator::overflow(
                super::edge::SurfaceKind::Forward,
                src_vertex_ref,
                0,
            ),
        );
        Some(EdgePairLogicalLocators {
            forward: super::edge::LogicalEdgeLocator::overflow(
                super::edge::SurfaceKind::Forward,
                src_vertex_ref,
                0,
            ),
            reverse: super::edge::LogicalEdgeLocator::overflow(
                super::edge::SurfaceKind::Reverse,
                dst_vertex_ref,
                0,
            ),
        })
    }

    /// Inserts one logical edge, choosing base-tail append when both surfaces
    /// can support it and otherwise falling back to overflow.
    pub fn insert_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        label_id: LabelId,
    ) -> Option<GraphInsertResult> {
        let decision =
            self.choose_insert_decision(src_vertex_ref, src_ordinal, dst_vertex_ref, dst_ordinal)?;
        self.insert_edge_pair_for_decision(
            edge_id,
            src_vertex_ref,
            src_ordinal,
            dst_vertex_ref,
            dst_ordinal,
            label_id,
            decision,
        )
    }

    /// Inserts one logical edge while resolving the final write path against
    /// manager-backed segment capacities.
    pub fn insert_edge_pair_with_manager(
        &mut self,
        edge_id: EdgeId,
        src_vertex_ref: VertexRef,
        src_ordinal: usize,
        dst_vertex_ref: VertexRef,
        dst_ordinal: usize,
        label_id: LabelId,
        manager: &RegionManager,
    ) -> Option<GraphInsertResult> {
        self.sync_base_segment_capacities_from_manager_best_effort(manager);
        let decision = self.choose_insert_decision_with_incoming_live_entries_and_manager(
            src_vertex_ref,
            src_ordinal,
            dst_vertex_ref,
            dst_ordinal,
            1,
            manager,
        )?;
        let (path, locators) = match decision {
            GraphInsertDecision::BaseInsert {
                forward_path,
                reverse_path,
            } => {
                let (actual_forward_path, actual_reverse_path, locators) = self
                    .insert_base_edge_pair_with_manager(
                        edge_id,
                        src_vertex_ref,
                        src_ordinal,
                        dst_vertex_ref,
                        dst_ordinal,
                        label_id,
                        manager,
                    )?;
                debug_assert_eq!(actual_forward_path, forward_path);
                debug_assert_eq!(actual_reverse_path, reverse_path);
                (actual_forward_path, locators)
            }
            GraphInsertDecision::Overflow => (
                EdgeInsertPath::Overflow,
                self.append_overflow_edge_pair(
                    edge_id,
                    src_vertex_ref,
                    src_ordinal,
                    dst_vertex_ref,
                    dst_ordinal,
                    label_id,
                )?,
            ),
            GraphInsertDecision::RebalanceRequired(plan) => {
                return Some(GraphInsertResult::RebalanceRequired(plan));
            }
        };
        Some(GraphInsertResult::Inserted { path, locators })
    }

    /// Tombstones one logical base edge on both directional surfaces and removes its locator.
    pub fn tombstone_base_edge_pair(
        &mut self,
        edge_id: EdgeId,
        endpoints: EdgePairEndpoints,
        locators: EdgePairLogicalLocators,
    ) -> Option<()> {
        let ResolvedEdgeSlot::Base {
            logical_index: src_logical_index,
        } = self.forward.resolve_logical_edge_slot(
            endpoints.src_vertex_ref,
            endpoints.src_ordinal,
            locators.forward,
        )?
        else {
            return None;
        };
        let ResolvedEdgeSlot::Base {
            logical_index: dst_logical_index,
        } = self.reverse.resolve_logical_edge_slot(
            endpoints.dst_vertex_ref,
            endpoints.dst_ordinal,
            locators.reverse,
        )?
        else {
            return None;
        };

        let forward_entry = self
            .forward
            .tombstone_base_entry(endpoints.src_ordinal, src_logical_index)?;
        let reverse_vertex = forward_entry.target;
        let _ = self
            .reverse
            .tombstone_base_entry(endpoints.dst_ordinal, dst_logical_index)?;

        let _ = reverse_vertex;
        let _ = self.logical_locator_sidecar.remove(edge_id);
        Some(())
    }

    /// Replaces one logical base edge on both directional surfaces.
    pub fn replace_base_edge_pair(
        &mut self,
        spec: EdgeReplaceSpec,
    ) -> Option<(EdgeEntry, EdgeEntry)> {
        let ResolvedEdgeSlot::Base {
            logical_index: src_logical_index,
        } = self.forward.resolve_logical_edge_slot(
            spec.endpoints.src_vertex_ref,
            spec.endpoints.src_ordinal,
            spec.locators.forward,
        )?
        else {
            return None;
        };
        let ResolvedEdgeSlot::Base {
            logical_index: dst_logical_index,
        } = self.reverse.resolve_logical_edge_slot(
            spec.endpoints.dst_vertex_ref,
            spec.endpoints.dst_ordinal,
            spec.locators.reverse,
        )?
        else {
            return None;
        };

        let forward_old = self.forward.replace_base_entry(
            spec.endpoints.src_ordinal,
            src_logical_index,
            EdgeEntry::new(
                spec.endpoints.dst_vertex_ref,
                EdgeMeta::new(spec.label_id, false),
            ),
        )?;
        let reverse_old = self.reverse.replace_base_entry(
            spec.endpoints.dst_ordinal,
            dst_logical_index,
            EdgeEntry::new(
                spec.endpoints.src_vertex_ref,
                EdgeMeta::new(spec.label_id, false),
            ),
        )?;

        Some((forward_old, reverse_old))
    }

    /// Replaces one logical edge on both directional surfaces, choosing base
    /// or overflow handling from the supplied forward logical locator.
    pub fn replace_edge_pair(
        &mut self,
        spec: EdgeReplaceSpec,
    ) -> Option<(GraphMutationPath, (EdgeEntry, EdgeEntry))> {
        let resolved = self.forward.resolve_logical_edge_slot(
            spec.endpoints.src_vertex_ref,
            spec.endpoints.src_ordinal,
            spec.locators.forward,
        )?;
        match resolved {
            ResolvedEdgeSlot::Base { .. } => self
                .replace_base_edge_pair(spec)
                .map(|entries| (GraphMutationPath::Base, entries)),
            ResolvedEdgeSlot::Overflow { .. } => {
                let forward_old = self.forward.replace_overflow_entry(
                    spec.endpoints.src_vertex_ref,
                    spec.endpoints.src_ordinal,
                    spec.edge_id,
                    EdgeEntry::new(
                        spec.endpoints.dst_vertex_ref,
                        EdgeMeta::new(spec.label_id, false),
                    ),
                )?;
                let reverse_old = self.reverse.replace_overflow_entry(
                    spec.endpoints.dst_vertex_ref,
                    spec.endpoints.dst_ordinal,
                    spec.edge_id,
                    EdgeEntry::new(
                        spec.endpoints.src_vertex_ref,
                        EdgeMeta::new(spec.label_id, false),
                    ),
                )?;
                Some((
                    GraphMutationPath::Overflow,
                    (forward_old.entry, reverse_old.entry),
                ))
            }
        }
    }

    /// Tombstones one logical edge on both directional surfaces, choosing the
    /// base or overflow path from the supplied forward logical locator.
    pub fn tombstone_edge_pair(&mut self, spec: EdgeTombstoneSpec) -> Option<GraphMutationPath> {
        let resolved = self.forward.resolve_logical_edge_slot(
            spec.endpoints.src_vertex_ref,
            spec.endpoints.src_ordinal,
            spec.locators.forward,
        )?;
        match resolved {
            ResolvedEdgeSlot::Base { .. } => self
                .tombstone_base_edge_pair(spec.edge_id, spec.endpoints, spec.locators)
                .map(|()| GraphMutationPath::Base),
            ResolvedEdgeSlot::Overflow { .. } => {
                let _ = self.forward.tombstone_overflow_entry(
                    spec.endpoints.src_vertex_ref,
                    spec.endpoints.src_ordinal,
                    spec.edge_id,
                )?;
                let _ = self.reverse.tombstone_overflow_entry(
                    spec.endpoints.dst_vertex_ref,
                    spec.endpoints.dst_ordinal,
                    spec.edge_id,
                )?;
                let _ = self.logical_locator_sidecar.remove(spec.edge_id);
                Some(GraphMutationPath::Overflow)
            }
        }
    }

    /// Refreshes label sidecars only for vertices marked dirty on each surface.
    pub fn refresh_label_sidecars(&mut self) -> Option<(Vec<usize>, Vec<usize>)> {
        let forward = match self.forward.refresh_label_sidecar_for_dirty_vertices() {
            Some(refreshed) => refreshed,
            None => {
                self.forward.0.rebuild_label_sidecar()?;
                (0..self.forward.0.vertices.len()).collect()
            }
        };
        let reverse = match self.reverse.refresh_label_sidecar_for_dirty_vertices() {
            Some(refreshed) => refreshed,
            None => {
                self.reverse.0.rebuild_label_sidecar()?;
                (0..self.reverse.0.vertices.len()).collect()
            }
        };
        Some((forward, reverse))
    }

    /// Flushes dirty forward and reverse surface regions to stable memory.
    pub fn write_dirty_to_stable_memory(
        &mut self,
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<(), WritebackError> {
        write_dirty_forward_surface_runtime_to_stable_memory(manager, memory, &mut self.forward)?;
        write_dirty_reverse_surface_runtime_to_stable_memory(manager, memory, &mut self.reverse)?;
        Ok(())
    }

    /// Refreshes dirty label sidecars and then flushes dirty regions to stable memory.
    pub fn refresh_and_write_dirty_to_stable_memory(
        &mut self,
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<(Vec<usize>, Vec<usize>), WritebackError> {
        if self.forward.0.dirty_vertices().next().is_none()
            && self.reverse.0.dirty_vertices().next().is_none()
        {
            self.write_dirty_to_stable_memory(manager, memory)?;
            return Ok((Vec::new(), Vec::new()));
        }
        let refreshed = self
            .refresh_label_sidecars()
            .expect("dirty vertex refresh should be valid before writeback");
        self.write_dirty_to_stable_memory(manager, memory)?;
        Ok(refreshed)
    }

    /// Looks up the canonical logical locator currently stored for a semantic edge id.
    pub fn logical_locator(&self, edge_id: EdgeId) -> Option<super::edge::LogicalEdgeLocator> {
        self.logical_locator_sidecar.get(edge_id)
    }

    /// Returns the canonical logical locator only if it points into the forward surface.
    pub fn canonical_forward_logical_locator(
        &self,
        edge_id: EdgeId,
    ) -> Option<super::edge::LogicalEdgeLocator> {
        let locator = self.logical_locator(edge_id)?;
        (locator.surface_kind() == super::edge::SurfaceKind::Forward).then_some(locator)
    }

    /// Returns the current forward overflow-log head for one vertex ordinal.
    pub fn forward_log_offset_for(&self, ordinal: usize) -> Option<LogOffset> {
        let entry = self.forward.0.vertex_entry(ordinal)?;
        Some(LogOffset::new(entry.log_offset))
    }
}

fn reserve_extra_slots_for_degree(policy: GraphInsertPolicy, live_degree: u32) -> u32 {
    if live_degree < policy.high_degree_reserve_threshold {
        return 0;
    }
    let divisor = policy.high_degree_reserve_divisor.max(1);
    (live_degree / divisor).max(1)
}

impl<'a, M: Memory> GraphBatchMutationSession<'a, M> {
    /// Creates one batch-mutation session over a graph runtime.
    pub fn new(
        graph: &'a mut GraphRuntime,
        manager: &'a std::cell::RefCell<RegionManager>,
        memory: &'a M,
    ) -> Self {
        Self {
            graph,
            manager,
            memory,
        }
    }

    /// Returns the graph runtime currently being mutated.
    pub fn graph(&self) -> &GraphRuntime {
        self.graph
    }

    /// Returns the graph runtime mutably.
    pub fn graph_mut(&mut self) -> &mut GraphRuntime {
        self.graph
    }

    /// Prepares local capacity for an upcoming batch without inserting yet.
    pub fn prepare_local_capacity(&mut self, spec: RebalancePrepareSpec<'_>) -> Option<bool> {
        self.graph
            .ensure_local_capacity_for_incoming_live_entries(spec)
    }

    /// Inserts one edge using the batch-aware rebalance path without flushing yet.
    pub fn insert_edge_pair(&mut self, spec: RebalanceInsertSpec<'_>) -> Option<GraphInsertResult> {
        self.graph
            .insert_edge_pair_with_local_rebalance_for_incoming_live_entries(spec)
    }

    /// Replaces one logical edge without flushing yet.
    pub fn replace_edge_pair(
        &mut self,
        spec: EdgeReplaceSpec,
    ) -> Option<(GraphMutationPath, (EdgeEntry, EdgeEntry))> {
        self.graph.replace_edge_pair(spec)
    }

    /// Tombstones one logical edge without flushing yet.
    pub fn tombstone_edge_pair(&mut self, spec: EdgeTombstoneSpec) -> Option<GraphMutationPath> {
        self.graph.tombstone_edge_pair(spec)
    }

    /// Flushes dirty graph state accumulated so far in this batch.
    pub fn flush(&mut self) -> Result<(Vec<usize>, Vec<usize>), WritebackError> {
        self.graph
            .refresh_and_write_dirty_to_stable_memory(&mut self.manager.borrow_mut(), self.memory)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EdgePairEndpoints, EdgePairLogicalLocators, EdgeReplaceSpec, EdgeTombstoneSpec,
        GraphBatchMutationSession, GraphInsertDecision, GraphInsertPolicy, GraphInsertResult,
        GraphMutationPath, GraphRuntime, RebalanceInsertSpec, RebalancePrepareSpec,
    };
    use crate::low_level::runtime::SurfaceBaseStorage;
    use crate::low_level::{
        EMPTY_LOG_OFFSET, EdgeEntry, EdgeIndex, EdgeInsertPath, EdgeMeta, EdgeRef,
        EdgeSegmentHeader, EdgeSegmentState, ExtentChain, ExtentId, ForwardSurface,
        ForwardSurfaceRuntime, LogOffset, LogicalEdgeLocator, OverflowEntry, RegionKind,
        RegionManager, RegionRef, RegionStorageKind, ReverseSurface, ReverseSurfaceRuntime,
        SurfaceKind, SurfaceRegions, VertexEntry, VertexRef, WasmPages,
    };
    use crate::stable::VecMemory;

    fn forward_surface() -> ForwardSurface {
        ForwardSurface::new(SurfaceRegions::new(
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardVertexTable,
                1,
                128,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardEdgeEntries,
                2,
                4096,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardLabelIndex,
                3,
                256,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardSegmentLog,
                4,
                1024,
            ),
        ))
    }

    fn reverse_surface() -> ReverseSurface {
        ReverseSurface::new(SurfaceRegions::new(
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseVertexTable,
                1,
                128,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseEdgeEntries,
                2,
                4096,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseLabelIndex,
                3,
                256,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseSegmentLog,
                4,
                1024,
            ),
        ))
    }

    #[test]
    fn graph_runtime_appends_overflow_edge_to_both_surfaces_and_sidecar() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
        );

        let locators = graph
            .append_overflow_edge_pair(
                55,
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
                9,
            )
            .expect("pair append");

        assert_eq!(locators.forward.surface_kind(), SurfaceKind::Forward);
        assert_eq!(locators.reverse.surface_kind(), SurfaceKind::Reverse);
        assert_eq!(graph.forward.0.overflow_entries.len(), 1);
        assert_eq!(graph.reverse.0.overflow_entries.len(), 1);
        assert_eq!(graph.logical_locator(55), Some(locators.forward));
        assert_eq!(graph.forward.0.vertices[0].log_offset, 0);
        assert_eq!(graph.reverse.0.vertices[0].log_offset, 0);
    }

    #[test]
    fn graph_runtime_can_append_empty_vertex_pair() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::without_overflow(forward_surface(), Vec::new()),
            ReverseSurfaceRuntime::without_overflow(reverse_surface(), Vec::new()),
        );

        let ordinals = graph.append_empty_vertex_pair().expect("append empty pair");

        assert_eq!(ordinals, (0, 0));
        assert_eq!(graph.forward.0.vertices.len(), 1);
        assert_eq!(graph.reverse.0.vertices.len(), 1);
        assert_eq!(graph.forward.0.vertices[0].degree, 0);
        assert_eq!(graph.reverse.0.vertices[0].degree, 0);
    }

    #[test]
    fn graph_runtime_can_append_multiple_empty_vertex_pairs() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::without_overflow(forward_surface(), Vec::new()),
            ReverseSurfaceRuntime::without_overflow(reverse_surface(), Vec::new()),
        );

        let ordinals = graph
            .append_empty_vertex_pairs(3)
            .expect("append empty pairs");

        assert_eq!(ordinals, vec![(0, 0), (1, 1), (2, 2)]);
        assert_eq!(graph.forward.0.vertices.len(), 3);
        assert_eq!(graph.reverse.0.vertices.len(), 3);
    }

    #[test]
    fn graph_runtime_can_append_base_pair_when_both_surfaces_have_tail_capacity() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
        );

        let GraphInsertResult::Inserted { path, locators } = graph
            .insert_edge_pair(
                88,
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
                9,
            )
            .expect("base append")
        else {
            panic!("expected inserted result");
        };

        assert_eq!(path, EdgeInsertPath::BaseAppend { logical_index: 1 });
        assert_eq!(locators.forward.surface_kind(), SurfaceKind::Forward);
        assert_eq!(locators.reverse.surface_kind(), SurfaceKind::Reverse);
        assert_eq!(graph.forward.0.base_entries.len(), 2);
        assert_eq!(graph.reverse.0.base_entries.len(), 2);
        assert_eq!(graph.forward.0.vertices[0].degree, 2);
        assert_eq!(graph.reverse.0.vertices[0].degree, 2);
        assert_eq!(graph.logical_locator(88), Some(locators.forward));
    }

    #[test]
    fn graph_runtime_falls_back_to_overflow_when_base_tail_append_is_not_possible() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::new(1), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                ],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
        );

        let GraphInsertResult::Inserted { path, .. } = graph
            .insert_edge_pair(
                89,
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
                9,
            )
            .expect("overflow fallback")
        else {
            panic!("expected inserted result");
        };

        assert_eq!(path, EdgeInsertPath::Overflow);
        assert_eq!(graph.forward.0.overflow_entries.len(), 1);
        assert_eq!(graph.reverse.0.overflow_entries.len(), 1);
    }

    #[test]
    fn graph_runtime_can_reuse_tombstoned_tail_base_slots_before_overflow() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(12, false)),
                ],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(88u8), EdgeMeta::new(12, false)),
                ],
                Vec::new(),
            ),
        );

        let GraphInsertResult::Inserted { path, .. } = graph
            .insert_edge_pair(
                189,
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
                9,
            )
            .expect("tombstone reuse insert")
        else {
            panic!("expected inserted result");
        };

        assert_eq!(
            path,
            EdgeInsertPath::BaseReuseTombstone { logical_index: 1 }
        );
        assert_eq!(graph.forward.0.overflow_entries.len(), 0);
        assert_eq!(graph.reverse.0.overflow_entries.len(), 0);
        let fe1 = graph
            .forward
            .0
            .base_entries
            .get(1)
            .copied()
            .expect("forward base entry 1");
        assert_eq!(u64::from(fe1.target), 2);
        assert!(!fe1.meta.is_tombstone());
    }

    #[test]
    fn graph_runtime_migrate_contiguous_base_to_segment_zero_preserves_slots() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                ],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(8, false)),
                ],
                Vec::new(),
            ),
        );
        assert!(!graph.forward.0.base_entries.is_segmented());
        assert!(!graph.reverse.0.base_entries.is_segmented());
        let forward_before: Vec<EdgeEntry> =
            SurfaceBaseStorage::iter(&graph.forward.0.base_entries)
                .copied()
                .collect();
        let reverse_before: Vec<EdgeEntry> =
            SurfaceBaseStorage::iter(&graph.reverse.0.base_entries)
                .copied()
                .collect();

        graph.migrate_contiguous_base_to_segment_zero();

        assert!(graph.forward.0.base_entries.is_segmented());
        assert!(graph.reverse.0.base_entries.is_segmented());
        assert_eq!(graph.forward.0.base_entries.len(), forward_before.len());
        assert_eq!(graph.reverse.0.base_entries.len(), reverse_before.len());
        for (i, expected) in forward_before.iter().enumerate() {
            assert_eq!(
                graph.forward.0.base_entries.get(i).copied(),
                Some(*expected)
            );
        }
        for (i, expected) in reverse_before.iter().enumerate() {
            assert_eq!(
                graph.reverse.0.base_entries.get(i).copied(),
                Some(*expected)
            );
        }
    }

    #[test]
    fn graph_runtime_replace_base_storages_with_segmented_sets_explicit_segments() {
        use std::collections::BTreeMap;

        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(3, false),
                )],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(4, false),
                )],
                Vec::new(),
            ),
        );
        graph.replace_base_storages_with_segmented(
            BTreeMap::from([(
                0,
                vec![EdgeEntry::new(
                    VertexRef::from(22u8),
                    EdgeMeta::new(3, false),
                )],
            )]),
            BTreeMap::from([(0, 1_u64)]),
            BTreeMap::from([(
                0,
                vec![EdgeEntry::new(
                    VertexRef::from(11u8),
                    EdgeMeta::new(4, false),
                )],
            )]),
            BTreeMap::from([(0, 1_u64)]),
        );
        assert!(graph.forward.0.base_entries.is_segmented());
        assert!(graph.reverse.0.base_entries.is_segmented());
        let f0 = graph
            .forward
            .0
            .base_entries
            .get(0)
            .copied()
            .expect("forward base entry 0");
        let r0 = graph
            .reverse
            .0
            .base_entries
            .get(0)
            .copied()
            .expect("reverse base entry 0");
        assert_eq!(u64::from(f0.target), 22);
        assert_eq!(u64::from(r0.target), 11);
    }

    #[test]
    fn graph_runtime_can_request_rebalance_before_inserting() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 1, 0),
                    VertexEntry::new(EdgeIndex::new(1), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                ],
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 1, 0),
                    VertexEntry::new(EdgeIndex::new(1), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(8, false)),
                ],
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 1,
                rebalance_window_radius: 1,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision");
        let GraphInsertDecision::RebalanceRequired(plan) = decision else {
            panic!("expected rebalance-required decision");
        };
        assert_eq!(plan.forward.vertex_ref, VertexRef::from(1u8).into());
        assert_eq!(plan.forward.ordinal, 0);
        assert_eq!(plan.forward.base_degree, 1);
        assert_eq!(plan.forward.overflow_len, 1);
        assert_eq!(plan.forward.incoming_live_entries, 1);
        assert_eq!(plan.reverse.vertex_ref, VertexRef::from(2u8).into());
        assert_eq!(plan.reverse.ordinal, 0);
        assert_eq!(plan.reverse.base_degree, 1);
        assert_eq!(plan.reverse.overflow_len, 1);
        assert_eq!(plan.reverse.incoming_live_entries, 1);

        let result = graph
            .insert_edge_pair(
                90,
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
                9,
            )
            .expect("insert result");
        let GraphInsertResult::RebalanceRequired(plan) = result else {
            panic!("expected rebalance-required result");
        };
        assert_eq!(plan.forward.overflow_len, 1);
        assert_eq!(plan.reverse.overflow_len, 1);
        assert_eq!(graph.forward.0.overflow_entries.len(), 1);
        assert_eq!(graph.reverse.0.overflow_entries.len(), 1);
    }

    #[test]
    fn graph_runtime_can_build_rebalance_plan_for_multiple_incoming_live_entries() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let plan = graph
            .plan_rebalance_for_insert_with_incoming_live_entries(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
                2,
            )
            .expect("rebalance plan");
        assert_eq!(plan.forward.incoming_live_entries, 2);
        assert_eq!(plan.reverse.incoming_live_entries, 2);

        let local = graph
            .plan_local_rebalance(plan)
            .expect("local rebalance plan");
        assert!(local.forward.target_base_len >= 3);
        assert!(local.reverse.target_base_len >= 3);
        assert!(local.forward.reserved_base_len >= local.forward.target_base_len);
        assert!(local.reverse.reserved_base_len >= local.reverse.target_base_len);
        assert_eq!(
            local.forward.gap_budget,
            local.forward.reserved_base_len - local.forward.target_base_len
        );
        assert_eq!(
            local.reverse.gap_budget,
            local.reverse.reserved_base_len - local.reverse.target_base_len
        );
        assert!(local.forward.total_weight > 0);
        assert!(local.reverse.total_weight > 0);
        assert_eq!(
            local.forward.weighted_layout.reserved_base_len,
            local.forward.reserved_base_len
        );
        assert_eq!(
            local.reverse.weighted_layout.reserved_base_len,
            local.reverse.reserved_base_len
        );
        assert_eq!(
            local.forward.weighted_layout.positions.len(),
            local
                .forward
                .end_ordinal_exclusive
                .saturating_sub(local.forward.start_ordinal)
        );
        assert_eq!(
            local.forward.expected_capacity_span_len(),
            local.forward.reserved_base_len
        );
        assert_eq!(local.forward.current_window_span_len, 1);
        assert_eq!(
            local.forward.expected_displacement_against_current_span(),
            i64::from(local.forward.reserved_base_len) - 1
        );
        assert_eq!(
            local.reverse.weighted_layout.positions.len(),
            local
                .reverse
                .end_ordinal_exclusive
                .saturating_sub(local.reverse.start_ordinal)
        );
        assert_eq!(
            local.reverse.expected_capacity_span_len(),
            local.reverse.reserved_base_len
        );
        assert_eq!(local.reverse.current_window_span_len, 1);
        assert_eq!(
            local.reverse.expected_displacement_against_current_span(),
            i64::from(local.reverse.reserved_base_len) - 1
        );
        assert_eq!(
            local.total_expected_displacement(),
            local.forward_expected_displacement() + local.reverse_expected_displacement()
        );
        assert_eq!(
            local.max_expected_displacement(),
            local
                .forward_expected_displacement()
                .max(local.reverse_expected_displacement())
        );
    }

    #[test]
    fn graph_runtime_can_refine_rebalance_plan_into_local_windows() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::new(1), 1, 1),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![
                    OverflowEntry::new(
                        55,
                        EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                        LogOffset::EMPTY,
                    ),
                    OverflowEntry::new(
                        56,
                        EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                        LogOffset::EMPTY,
                    ),
                ],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::new(1), 1, 1),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(9, false)),
                ],
                vec![
                    OverflowEntry::new(
                        55,
                        EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                        LogOffset::EMPTY,
                    ),
                    OverflowEntry::new(
                        56,
                        EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(8, false)),
                        LogOffset::EMPTY,
                    ),
                ],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 1,
                rebalance_window_radius: 1,
                ..GraphInsertPolicy::default()
            },
        );

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                1,
                VertexRef::from(2u8).into(),
                1,
            )
            .expect("decision")
        else {
            panic!("expected rebalance-required decision");
        };

        let local = graph
            .plan_local_rebalance(plan)
            .expect("local rebalance plan");

        assert_eq!(local.forward.start_ordinal, 0);
        assert_eq!(local.forward.end_ordinal_exclusive, 3);
        assert_eq!(local.forward.target_base_len, 5);
        assert!(local.forward.reserved_base_len >= local.forward.target_base_len);
        assert_eq!(
            local.forward.gap_budget,
            local.forward.reserved_base_len - local.forward.target_base_len
        );
        assert_eq!(local.forward.total_weight, 6);
        assert_eq!(local.forward.current_window_span_len, 3);
        assert_eq!(
            local.forward.weighted_layout.positions.len(),
            local.forward.end_ordinal_exclusive - local.forward.start_ordinal
        );
        assert_eq!(
            local.forward.weighted_layout.reserved_lengths.len(),
            local.forward.end_ordinal_exclusive - local.forward.start_ordinal
        );
        assert_eq!(
            local.forward.expected_base_end_exclusive(),
            local.forward.weighted_layout.end_exclusive()
        );
        assert_eq!(
            local.forward.expected_displacement_against_current_span(),
            i64::from(local.forward.reserved_base_len) - 3
        );
        assert_eq!(local.reverse.start_ordinal, 0);
        assert_eq!(local.reverse.end_ordinal_exclusive, 3);
        assert_eq!(local.reverse.target_base_len, 5);
        assert!(local.reverse.reserved_base_len >= local.reverse.target_base_len);
        assert_eq!(
            local.reverse.gap_budget,
            local.reverse.reserved_base_len - local.reverse.target_base_len
        );
        assert_eq!(local.reverse.total_weight, 6);
        assert_eq!(local.reverse.current_window_span_len, 3);
        assert_eq!(
            local.reverse.weighted_layout.positions.len(),
            local.reverse.end_ordinal_exclusive - local.reverse.start_ordinal
        );
        assert_eq!(
            local.reverse.weighted_layout.reserved_lengths.len(),
            local.reverse.end_ordinal_exclusive - local.reverse.start_ordinal
        );
        assert_eq!(
            local.reverse.expected_base_end_exclusive(),
            local.reverse.weighted_layout.end_exclusive()
        );
        assert_eq!(
            local.reverse.expected_displacement_against_current_span(),
            i64::from(local.reverse.reserved_base_len) - 3
        );
        assert_eq!(
            local.total_expected_displacement(),
            local.forward_expected_displacement() + local.reverse_expected_displacement()
        );
        assert_eq!(
            local.max_expected_displacement(),
            local
                .forward_expected_displacement()
                .max(local.reverse_expected_displacement())
        );
    }

    #[test]
    fn graph_runtime_prefers_rebalance_over_overflow_when_window_has_reclaimable_slack() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 3, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(10, false)),
                ],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 3, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(10, false)),
                ],
                Vec::new(),
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 1,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision");
        let GraphInsertDecision::RebalanceRequired(plan) = decision else {
            panic!("expected rebalance-required decision");
        };
        assert_eq!(plan.forward.ordinal, 0);
        assert_eq!(plan.reverse.ordinal, 0);
        assert_eq!(plan.forward.overflow_len, 0);
        assert_eq!(plan.reverse.overflow_len, 0);
        assert_eq!(plan.forward.incoming_live_entries, 1);
        assert_eq!(plan.reverse.incoming_live_entries, 1);
    }

    #[test]
    fn graph_runtime_multi_entry_insert_decision_skips_single_entry_base_fast_path() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision_with_incoming_live_entries(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
                2,
            )
            .expect("decision");
        let GraphInsertDecision::RebalanceRequired(plan) = decision else {
            panic!("expected rebalance-required decision");
        };
        assert_eq!(plan.forward.incoming_live_entries, 2);
        assert_eq!(plan.reverse.incoming_live_entries, 2);
    }

    #[test]
    fn graph_runtime_can_insert_with_batch_aware_rebalance_helper() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let result = graph
            .insert_edge_pair_with_local_rebalance_for_incoming_live_entries(RebalanceInsertSpec {
                edge_id: 91,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(2u8).into(),
                    dst_ordinal: 0,
                },
                label_id: 11,
                planned_incoming_live_entries: 2,
                forward_rebalance_vertex_ids: &[VertexRef::from(1u8).into()],
                forward_rebalance_base_edge_ids_by_ordinal: &[vec![90, 91]],
            })
            .expect("insert result");

        let GraphInsertResult::Inserted { path, .. } = result else {
            panic!("expected inserted result");
        };
        assert_eq!(
            path,
            EdgeInsertPath::BaseReuseTombstone { logical_index: 2 }
        );
        assert_eq!(
            graph.logical_locator(91),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                2,
            ))
        );
        assert_eq!(
            graph.logical_locator(91),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                2,
            ))
        );
    }

    #[test]
    fn graph_runtime_can_prepare_local_capacity_for_batch_without_inserting() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let rebalanced = graph
            .ensure_local_capacity_for_incoming_live_entries(RebalancePrepareSpec {
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(2u8).into(),
                    dst_ordinal: 0,
                },
                planned_incoming_live_entries: 2,
                forward_rebalance_vertex_ids: &[VertexRef::from(1u8).into()],
                forward_rebalance_base_edge_ids_by_ordinal: &[vec![80, 90]],
            })
            .expect("prepare capacity");
        assert!(rebalanced);
        assert_eq!(graph.forward.0.vertices[0].log_offset, EMPTY_LOG_OFFSET);
        assert_eq!(graph.reverse.0.vertices[0].log_offset, EMPTY_LOG_OFFSET);
        assert_eq!(graph.forward.0.vertices[0].degree, 2);
        assert_eq!(graph.reverse.0.vertices[0].degree, 2);
        assert_eq!(
            graph.logical_locator(80),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                0,
            ))
        );
        assert_eq!(
            graph.logical_locator(90),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                1,
            ))
        );
    }

    #[test]
    fn batch_mutation_session_can_prepare_insert_and_flush() {
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 16_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 16_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        let manager = std::cell::RefCell::new(manager);
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let memory = VecMemory::default();

        let mut batch = GraphBatchMutationSession::new(&mut graph, &manager, &memory);
        let prepared = batch
            .prepare_local_capacity(RebalancePrepareSpec {
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(2u8).into(),
                    dst_ordinal: 0,
                },
                planned_incoming_live_entries: 2,
                forward_rebalance_vertex_ids: &[VertexRef::from(1u8).into()],
                forward_rebalance_base_edge_ids_by_ordinal: &[vec![80, 90]],
            })
            .expect("prepare");
        assert!(prepared);

        let inserted = batch
            .insert_edge_pair(RebalanceInsertSpec {
                edge_id: 91,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(2u8).into(),
                    dst_ordinal: 0,
                },
                label_id: 11,
                planned_incoming_live_entries: 2,
                forward_rebalance_vertex_ids: &[VertexRef::from(1u8).into()],
                forward_rebalance_base_edge_ids_by_ordinal: &[vec![80, 90]],
            })
            .expect("insert");
        let GraphInsertResult::Inserted { .. } = inserted else {
            panic!("expected inserted result");
        };

        let refreshed = batch.flush().expect("flush");
        assert_eq!(refreshed.0, vec![0]);
        assert_eq!(refreshed.1, vec![0]);
        assert_eq!(
            batch.graph().logical_locator(91),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                2,
            ))
        );
    }

    #[test]
    fn graph_runtime_can_prepare_capacity_with_segment_replacement() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );

        let prepared = graph
            .ensure_local_capacity_for_incoming_live_entries_with_segment_replacement(
                RebalancePrepareSpec {
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: VertexRef::from(1u8).into(),
                        src_ordinal: 0,
                        dst_vertex_ref: VertexRef::from(2u8).into(),
                        dst_ordinal: 0,
                    },
                    planned_incoming_live_entries: 2,
                    forward_rebalance_vertex_ids: &[VertexRef::from(1u8).into()],
                    forward_rebalance_base_edge_ids_by_ordinal: &[vec![80, 90]],
                },
                &mut manager,
                101,
            )
            .expect("prepare capacity with segment replacement");

        assert!(prepared.rebalanced);
        assert_eq!(
            prepared
                .rebalance
                .expect("rebalance summary")
                .segments
                .forward
                .new_segment
                .segment_id,
            1
        );
        assert_eq!(graph.forward.0.vertices[0].segment_id(), 1);
        assert_eq!(graph.reverse.0.vertices[0].segment_id(), 1);
        assert_eq!(graph.forward.0.vertices[0].log_offset, EMPTY_LOG_OFFSET);
        assert_eq!(graph.reverse.0.vertices[0].log_offset, EMPTY_LOG_OFFSET);
        assert_eq!(
            graph.logical_locator(80),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                0,
            ))
        );
        assert_eq!(
            graph.logical_locator(90),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                1,
            ))
        );
    }

    #[test]
    fn batch_mutation_session_can_replace_and_tombstone_edges() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
        );
        graph
            .insert_base_edge_pair(
                77,
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
                7,
            )
            .expect("seed sidecar");

        let memory = VecMemory::default();
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::new(1));
        for kind in [
            RegionKind::ForwardVertexTable,
            RegionKind::ForwardEdgeEntries,
            RegionKind::ForwardLabelIndex,
            RegionKind::ForwardSegmentLog,
            RegionKind::ReverseVertexTable,
            RegionKind::ReverseEdgeEntries,
            RegionKind::ReverseLabelIndex,
            RegionKind::ReverseSegmentLog,
        ] {
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

        let manager = std::cell::RefCell::new(manager);
        let mut batch = GraphBatchMutationSession::new(&mut graph, &manager, &memory);
        let replaced = batch
            .replace_edge_pair(EdgeReplaceSpec {
                edge_id: 77,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(3u8).into(),
                    dst_ordinal: 0,
                },
                locators: EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::base(
                        SurfaceKind::Forward,
                        VertexRef::from(1u8),
                        0,
                    ),
                    reverse: LogicalEdgeLocator::base(
                        SurfaceKind::Reverse,
                        VertexRef::from(3u8),
                        0,
                    ),
                },
                label_id: 9,
            })
            .expect("replace");
        assert_eq!(replaced.0, GraphMutationPath::Base);

        let tombstoned = batch
            .tombstone_edge_pair(EdgeTombstoneSpec {
                edge_id: 77,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(3u8).into(),
                    dst_ordinal: 0,
                },
                locators: EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::base(
                        SurfaceKind::Forward,
                        VertexRef::from(1u8),
                        0,
                    ),
                    reverse: LogicalEdgeLocator::base(
                        SurfaceKind::Reverse,
                        VertexRef::from(3u8),
                        0,
                    ),
                },
            })
            .expect("tombstone");
        assert_eq!(tombstoned, GraphMutationPath::Base);
    }

    #[test]
    fn graph_runtime_keeps_overflow_path_when_window_slack_cannot_absorb_existing_overflow() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 3, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(11, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 3, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(11, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 1,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision");
        assert_eq!(decision, GraphInsertDecision::Overflow);
    }

    #[test]
    fn graph_runtime_can_defer_rebalance_to_maintenance_for_foreground_insert() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                defer_rebalance_to_maintenance: true,
                hard_overflow_chain_len: 4,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision");
        assert_eq!(decision, GraphInsertDecision::Overflow);
    }

    #[test]
    fn graph_runtime_still_requires_rebalance_after_hard_overflow_limit() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                defer_rebalance_to_maintenance: true,
                hard_overflow_chain_len: 1,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision");
        assert!(matches!(
            decision,
            GraphInsertDecision::RebalanceRequired(_)
        ));
    }

    #[test]
    fn graph_runtime_collects_maintenance_candidates_by_priority() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    91,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let candidates = graph
            .collect_maintenance_candidates(&[
                VertexRef::from(1u8).into(),
                VertexRef::from(9u8).into(),
            ])
            .expect("maintenance candidates");

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].vertex_ref, VertexRef::from(1u8).into());
        assert!(candidates[0].has_overflow_backlog());
        assert!(candidates[0].forward_window_overflow_entries > 0);
        assert!(candidates[0].forward_window_total_base_slots > 0);
        assert!(candidates[0].priority_score > candidates[1].priority_score);
        assert_eq!(candidates[1].vertex_ref, VertexRef::from(9u8).into());
        assert!(candidates[1].has_reclaimable_tombstones());
    }

    #[test]
    fn graph_runtime_penalizes_recently_maintained_candidates() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    91,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                rebalance_window_radius: 0,
                maintenance_recent_epoch_window: 3,
                maintenance_recent_epoch_penalty: 100_000,
                ..GraphInsertPolicy::default()
            },
        );
        graph.recent_maintenance_epochs_by_ordinal.insert(0, 10);

        let candidates = graph
            .collect_maintenance_candidates_at_epoch(
                &[VertexRef::from(1u8).into(), VertexRef::from(9u8).into()],
                Some(11),
            )
            .expect("maintenance candidates");

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].vertex_ref, VertexRef::from(9u8).into());
        assert_eq!(candidates[0].recent_maintenance_penalty, 0);
        assert_eq!(candidates[1].vertex_ref, VertexRef::from(1u8).into());
        assert_eq!(candidates[1].last_maintenance_epoch, Some(10));
        assert!(candidates[1].recent_maintenance_penalty > 0);
    }

    #[test]
    fn graph_runtime_collects_deduplicated_maintenance_work_items() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, 0),
                    VertexEntry::new(EdgeIndex::new(5), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, true)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(11, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(12, true)),
                ],
                vec![
                    OverflowEntry::new(
                        90,
                        EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(13, false)),
                        LogOffset::EMPTY,
                    ),
                    OverflowEntry::new(
                        91,
                        EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(14, false)),
                        LogOffset::EMPTY,
                    ),
                ],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, 0),
                    VertexEntry::new(EdgeIndex::new(5), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(11u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(12u8), EdgeMeta::new(10, true)),
                    EdgeEntry::new(VertexRef::from(13u8), EdgeMeta::new(11, false)),
                    EdgeEntry::new(VertexRef::from(14u8), EdgeMeta::new(12, true)),
                ],
                vec![
                    OverflowEntry::new(
                        92,
                        EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(13, false)),
                        LogOffset::EMPTY,
                    ),
                    OverflowEntry::new(
                        93,
                        EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(14, false)),
                        LogOffset::EMPTY,
                    ),
                ],
            ),
            GraphInsertPolicy {
                rebalance_window_radius: 1,
                ..GraphInsertPolicy::default()
            },
        );

        let items = graph
            .collect_maintenance_work_items(&[
                VertexRef::from(1u8).into(),
                VertexRef::from(2u8).into(),
                VertexRef::from(3u8).into(),
            ])
            .expect("work items");

        assert_eq!(items.len(), 3);
        assert!(items[0].priority_score >= items[1].priority_score);
        let windows: std::collections::BTreeSet<_> = items
            .iter()
            .map(|item| (item.start_ordinal, item.end_ordinal_exclusive))
            .collect();
        assert!(windows.contains(&(0, 2)));
        assert!(windows.contains(&(0, 3)));
        assert!(windows.contains(&(1, 3)));
    }

    #[test]
    fn graph_runtime_can_rebuild_and_run_next_maintenance_queue_item() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    91,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 32_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 32_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        let memory = VecMemory::default();

        assert_eq!(
            graph.rebuild_maintenance_queue(&[
                VertexRef::from(1u8).into(),
                VertexRef::from(9u8).into()
            ]),
            Some(2)
        );
        assert_eq!(graph.maintenance_queue().len(), 2);

        let summary = graph
            .run_next_queued_maintenance_cycle_with_segment_replacement_and_write(
                &[VertexRef::from(1u8).into(), VertexRef::from(9u8).into()],
                &[vec![80, 81], vec![82]],
                &mut manager,
                &memory,
                171,
            )
            .expect("run queued maintenance")
            .expect("queued maintenance summary");

        assert_eq!(summary.candidate.vertex_ref, VertexRef::from(1u8).into());
        assert_eq!(graph.maintenance_queue().len(), 1);
    }

    #[test]
    fn graph_runtime_can_refresh_retained_maintenance_queue() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    91,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                rebalance_window_radius: 0,
                maintenance_recent_epoch_window: 3,
                maintenance_recent_epoch_penalty: 100_000,
                ..GraphInsertPolicy::default()
            },
        );

        assert_eq!(
            graph.rebuild_maintenance_queue(&[
                VertexRef::from(1u8).into(),
                VertexRef::from(9u8).into()
            ]),
            Some(2)
        );
        graph.record_recent_maintenance_window(0, 1, 10);

        let refreshed_len = graph
            .refresh_maintenance_queue_at_epoch(
                &[VertexRef::from(1u8).into(), VertexRef::from(9u8).into()],
                Some(11),
            )
            .expect("refresh queue");

        assert_eq!(refreshed_len, 2);
        assert_eq!(
            graph.maintenance_queue()[0].vertex_ref,
            VertexRef::from(9u8).into()
        );
        assert_eq!(
            graph.maintenance_queue()[1].vertex_ref,
            VertexRef::from(1u8).into()
        );
        assert!(graph.maintenance_queue()[1].recent_maintenance_penalty > 0);
    }

    #[test]
    fn graph_runtime_can_run_queued_maintenance_batch_with_refresh() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    91,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 32_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 32_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        let forward_segment = manager
            .allocate_edge_segment(RegionKind::ForwardEdgeEntries, 4, EdgeSegmentState::Active)
            .expect("allocate forward segment");
        let reverse_segment = manager
            .allocate_edge_segment(RegionKind::ReverseEdgeEntries, 4, EdgeSegmentState::Active)
            .expect("allocate reverse segment");
        graph.forward.0.vertices[0] = VertexEntry::new(
            EdgeIndex::from(EdgeRef::new(forward_segment.segment_id, 0)),
            2,
            0,
        );
        graph.forward.0.vertices[1] = VertexEntry::new(
            EdgeIndex::from(EdgeRef::new(forward_segment.segment_id, 3)),
            1,
            EMPTY_LOG_OFFSET,
        );
        graph.reverse.0.vertices[0] = VertexEntry::new(
            EdgeIndex::from(EdgeRef::new(reverse_segment.segment_id, 0)),
            2,
            0,
        );
        graph.reverse.0.vertices[1] = VertexEntry::new(
            EdgeIndex::from(EdgeRef::new(reverse_segment.segment_id, 3)),
            1,
            EMPTY_LOG_OFFSET,
        );
        let memory = VecMemory::default();
        let _ = graph.sync_base_segment_capacities_from_manager(&manager);

        assert_eq!(
            graph.rebuild_maintenance_queue(&[
                VertexRef::from(1u8).into(),
                VertexRef::from(9u8).into()
            ]),
            Some(2)
        );

        let summary = graph
            .run_queued_maintenance_cycles_with_segment_replacement_and_write(
                &[VertexRef::from(1u8).into(), VertexRef::from(9u8).into()],
                &[vec![80, 81], vec![82]],
                &mut manager,
                &memory,
                210,
                2,
                0,
            )
            .expect("run queued maintenance batch");

        assert_eq!(summary.cycles.len(), 1);
        assert_eq!(summary.queue_len_before, 2);
        assert_eq!(summary.queue_len_after, 0);
        assert!(graph.maintenance_queue().is_empty());
    }

    #[test]
    fn graph_runtime_can_plan_one_maintenance_cycle() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    91,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let plan = graph
            .plan_one_maintenance_cycle(&[VertexRef::from(1u8).into(), VertexRef::from(9u8).into()])
            .expect("maintenance cycle plan");

        assert_eq!(plan.candidate.vertex_ref, VertexRef::from(1u8).into());
        assert_eq!(plan.rebalance.forward.anchor_ordinal, 0);
        assert_eq!(plan.rebalance.reverse.anchor_ordinal, 0);
    }

    #[test]
    fn graph_runtime_can_run_one_maintenance_cycle_with_segment_replacement_and_write() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    91,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 32_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 32_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        let memory = VecMemory::default();

        let summary = graph
            .run_one_maintenance_cycle_with_segment_replacement_and_write(
                &[VertexRef::from(1u8).into(), VertexRef::from(9u8).into()],
                &[vec![80, 81], vec![82]],
                &mut manager,
                &memory,
                141,
            )
            .expect("run maintenance cycle")
            .expect("maintenance summary");

        assert_eq!(summary.candidate.vertex_ref, VertexRef::from(1u8).into());
        assert_eq!(
            summary
                .rebalance
                .apply
                .segments
                .forward
                .new_segment
                .segment_id,
            1
        );
        assert_eq!(summary.rebalance.refreshed_forward_vertices, vec![0, 1]);
        assert!(!graph.forward.0.has_dirty_regions());
        assert!(!graph.reverse.0.has_dirty_regions());
    }

    #[test]
    fn graph_runtime_can_run_maintenance_batch_and_sweep_retired_segments() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(10, true)),
                ],
                vec![OverflowEntry::new(
                    91,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(11, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 32_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 32_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        let forward_segment = manager
            .allocate_edge_segment(RegionKind::ForwardEdgeEntries, 4, EdgeSegmentState::Active)
            .expect("allocate forward segment");
        let reverse_segment = manager
            .allocate_edge_segment(RegionKind::ReverseEdgeEntries, 4, EdgeSegmentState::Active)
            .expect("allocate reverse segment");
        graph.forward.0.vertices[0] = VertexEntry::new(
            EdgeIndex::from(EdgeRef::new(forward_segment.segment_id, 0)),
            2,
            0,
        );
        graph.forward.0.vertices[1] = VertexEntry::new(
            EdgeIndex::from(EdgeRef::new(forward_segment.segment_id, 3)),
            1,
            EMPTY_LOG_OFFSET,
        );
        graph.reverse.0.vertices[0] = VertexEntry::new(
            EdgeIndex::from(EdgeRef::new(reverse_segment.segment_id, 0)),
            2,
            0,
        );
        graph.reverse.0.vertices[1] = VertexEntry::new(
            EdgeIndex::from(EdgeRef::new(reverse_segment.segment_id, 3)),
            1,
            EMPTY_LOG_OFFSET,
        );
        let memory = VecMemory::default();

        let summary = graph
            .run_maintenance_cycles_with_segment_replacement_and_write(
                &[VertexRef::from(1u8).into(), VertexRef::from(9u8).into()],
                &[vec![80, 81], vec![82]],
                &mut manager,
                &memory,
                200,
                1,
                0,
            )
            .expect("run maintenance batch");

        assert_eq!(summary.cycles.len(), 1);
        assert_eq!(summary.swept_forward_segments.len(), 1);
        assert_eq!(summary.swept_reverse_segments.len(), 1);
        assert_eq!(
            summary.swept_forward_segments[0].segment_id,
            forward_segment.segment_id
        );
        assert_eq!(
            summary.swept_reverse_segments[0].segment_id,
            reverse_segment.segment_id
        );
    }

    #[test]
    fn graph_runtime_can_build_local_rebalance_delta_from_local_plan() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision");
        let GraphInsertDecision::RebalanceRequired(plan) = decision else {
            panic!("expected rebalance-required decision");
        };
        let local = graph
            .plan_local_rebalance(plan)
            .expect("local rebalance plan");
        let delta = graph
            .build_local_rebalance_delta(local.clone())
            .expect("local rebalance delta");

        assert_eq!(delta.forward.compacted_base_entries.len(), 6);
        assert_eq!(delta.forward.rewritten_vertices[0].degree, 3);
        assert_eq!(
            delta.forward.rewritten_vertices[0].log_offset,
            EMPTY_LOG_OFFSET
        );
        assert_eq!(delta.forward.reserved_base_len, 6);
        assert_eq!(delta.reverse.compacted_base_entries.len(), 6);
        assert_eq!(delta.reverse.rewritten_vertices[0].degree, 3);
        assert_eq!(
            delta.reverse.rewritten_vertices[0].log_offset,
            EMPTY_LOG_OFFSET
        );
        assert_eq!(delta.reverse.reserved_base_len, 6);
        assert_eq!(
            delta.forward_capacity_span_len(),
            local.forward.expected_capacity_span_len()
        );
        assert_eq!(
            delta.reverse_capacity_span_len(),
            local.reverse.expected_capacity_span_len()
        );
        assert_eq!(
            delta.forward_displacement_against_plan(&local),
            local.forward_expected_displacement()
        );
        assert_eq!(
            delta.reverse_displacement_against_plan(&local),
            local.reverse_expected_displacement()
        );
        assert_eq!(
            delta.total_displacement_against_plan(&local),
            local.total_expected_displacement()
        );
        assert_eq!(
            delta.max_displacement_against_plan(&local),
            local.max_expected_displacement()
        );
    }

    #[test]
    fn graph_runtime_can_apply_local_rebalance_delta_and_clear_sidecar() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision")
        else {
            panic!("expected rebalance-required decision");
        };
        let local = graph
            .plan_local_rebalance(plan)
            .expect("local rebalance plan");
        let delta = graph
            .build_local_rebalance_delta(local)
            .expect("local rebalance delta");
        let summary = graph
            .apply_local_rebalance_delta(delta)
            .expect("apply local rebalance delta");
        assert_eq!(summary.forward.displacement, 4);
        assert_eq!(summary.reverse.displacement, 4);
        assert_eq!(summary.total_displacement(), 8);
        assert_eq!(summary.max_displacement(), 4);

        assert_eq!(graph.forward.0.vertices[0].degree, 3);
        assert_eq!(graph.forward.0.vertices[0].log_offset, EMPTY_LOG_OFFSET);
        assert_eq!(graph.reverse.0.vertices[0].degree, 3);
        assert_eq!(graph.reverse.0.vertices[0].log_offset, EMPTY_LOG_OFFSET);
        assert_eq!(graph.logical_locator(90), None);
    }

    #[test]
    fn graph_runtime_can_apply_local_rebalance_delta_with_segment_replacement() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision")
        else {
            panic!("expected rebalance-required decision");
        };
        let local = graph
            .plan_local_rebalance(plan)
            .expect("local rebalance plan");
        let delta = graph
            .build_local_rebalance_delta(local)
            .expect("local rebalance delta");
        let applied = graph
            .apply_local_rebalance_delta_with_segment_replacement(delta, &mut manager, 77)
            .expect("apply local rebalance delta with segment replacement");

        assert_eq!(applied.apply.total_displacement(), 8);
        assert_eq!(applied.segments.forward.new_segment.segment_id, 1);
        assert_eq!(applied.segments.reverse.new_segment.segment_id, 1);
        assert_eq!(applied.segments.forward.retired_segment, None);
        assert_eq!(applied.segments.reverse.retired_segment, None);
        assert_eq!(graph.forward.0.vertices[0].segment_id(), 1);
        assert_eq!(graph.forward.0.vertices[1].segment_id(), 0);
        assert_eq!(graph.reverse.0.vertices[0].segment_id(), 1);
        assert_eq!(graph.reverse.0.vertices[1].segment_id(), 0);
        assert_eq!(graph.logical_locator(90), None);
        assert_eq!(
            manager
                .edge_segment(RegionKind::ForwardEdgeEntries, 1)
                .expect("forward segment"),
            applied.segments.forward.new_segment
        );
        assert_eq!(
            manager
                .edge_segment(RegionKind::ReverseEdgeEntries, 1)
                .expect("reverse segment"),
            applied.segments.reverse.new_segment
        );
    }

    #[test]
    fn graph_runtime_segment_replacement_retires_explicit_segments() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::from(EdgeRef::new(7, 0)), 2, 0),
                    VertexEntry::new(EdgeIndex::from(EdgeRef::new(7, 2)), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::from(EdgeRef::new(9, 0)), 2, 0),
                    VertexEntry::new(EdgeIndex::from(EdgeRef::new(9, 2)), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager
            .register_edge_segment(
                RegionKind::ForwardEdgeEntries,
                EdgeSegmentHeader::new(7, ExtentId::new(70), 3, 0, EdgeSegmentState::Active),
            )
            .expect("register forward segment");
        manager
            .register_edge_segment(
                RegionKind::ReverseEdgeEntries,
                EdgeSegmentHeader::new(9, ExtentId::new(90), 3, 0, EdgeSegmentState::Active),
            )
            .expect("register reverse segment");
        let _ = graph.sync_base_segment_capacities_from_manager(&manager);

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision")
        else {
            panic!("expected rebalance-required decision");
        };
        let local = graph
            .plan_local_rebalance(plan)
            .expect("local rebalance plan");
        let delta = graph
            .build_local_rebalance_delta(local)
            .expect("local rebalance delta");
        let applied = graph
            .apply_local_rebalance_delta_with_segment_replacement(delta, &mut manager, 91)
            .expect("segment replacement");

        assert_eq!(
            graph.forward.0.vertices[0].segment_id(),
            applied.segments.forward.new_segment.segment_id
        );
        assert_eq!(
            graph.reverse.0.vertices[0].segment_id(),
            applied.segments.reverse.new_segment.segment_id
        );
        assert_eq!(
            applied
                .segments
                .forward
                .retired_segment
                .expect("retired forward")
                .state,
            EdgeSegmentState::Retired
        );
        assert_eq!(
            applied
                .segments
                .reverse
                .retired_segment
                .expect("retired reverse")
                .state,
            EdgeSegmentState::Retired
        );
        assert_eq!(
            manager
                .edge_segment(RegionKind::ForwardEdgeEntries, 7)
                .expect("forward retired")
                .state,
            EdgeSegmentState::Retired
        );
        assert_eq!(
            manager
                .edge_segment(RegionKind::ReverseEdgeEntries, 9)
                .expect("reverse retired")
                .state,
            EdgeSegmentState::Retired
        );
        assert_eq!(
            graph
                .forward
                .0
                .base_segment_slot_capacity(applied.segments.forward.new_segment.segment_id),
            Some(applied.segments.forward.new_segment.slot_capacity)
        );
        assert_eq!(
            graph
                .reverse
                .0
                .base_segment_slot_capacity(applied.segments.reverse.new_segment.segment_id),
            Some(applied.segments.reverse.new_segment.slot_capacity)
        );
    }

    #[test]
    fn manager_aware_insert_decision_ignores_gap_before_next_segment() {
        let graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::from(EdgeRef::new(7, 0)), 1, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::from(EdgeRef::new(8, 2)), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(200u8), EdgeMeta::new(0, true)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(7, false)),
                ],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::from(EdgeRef::new(9, 0)), 1, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::from(EdgeRef::new(10, 2)), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(201u8), EdgeMeta::new(0, true)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(7, false)),
                ],
                Vec::new(),
            ),
            GraphInsertPolicy::default(),
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager
            .register_edge_segment(
                RegionKind::ForwardEdgeEntries,
                EdgeSegmentHeader::new(7, ExtentId::new(70), 1, 0, EdgeSegmentState::Active),
            )
            .expect("register forward segment a");
        manager
            .register_edge_segment(
                RegionKind::ForwardEdgeEntries,
                EdgeSegmentHeader::new(8, ExtentId::new(80), 2, 0, EdgeSegmentState::Active),
            )
            .expect("register forward segment b");
        manager
            .register_edge_segment(
                RegionKind::ReverseEdgeEntries,
                EdgeSegmentHeader::new(9, ExtentId::new(90), 1, 0, EdgeSegmentState::Active),
            )
            .expect("register reverse segment a");
        manager
            .register_edge_segment(
                RegionKind::ReverseEdgeEntries,
                EdgeSegmentHeader::new(10, ExtentId::new(100), 2, 0, EdgeSegmentState::Active),
            )
            .expect("register reverse segment b");

        let forward_root = graph.forward.choose_base_insert_slot(0);
        let manager_aware_forward = graph.forward.choose_base_insert_slot_with(0, |segment_id| {
            manager
                .edge_segment(RegionKind::ForwardEdgeEntries, segment_id)
                .map(|segment| segment.slot_capacity)
        });
        let reverse_root = graph.reverse.choose_base_insert_slot(0);
        let manager_aware_reverse = graph.reverse.choose_base_insert_slot_with(0, |segment_id| {
            manager
                .edge_segment(RegionKind::ReverseEdgeEntries, segment_id)
                .map(|segment| segment.slot_capacity)
        });

        assert!(forward_root.is_none());
        assert!(manager_aware_forward.is_none());
        assert!(reverse_root.is_none());
        assert!(manager_aware_reverse.is_none());
    }

    #[test]
    fn segment_replacement_write_path_avoids_reusing_gap_before_next_segment() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new((7_u64 << 40) | 0), 1, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::new((8_u64 << 40) | 2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(200u8), EdgeMeta::new(0, true)),
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(7, false)),
                ],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new((9_u64 << 40) | 0), 1, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::new((10_u64 << 40) | 2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(201u8), EdgeMeta::new(0, true)),
                    EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(7, false)),
                ],
                Vec::new(),
            ),
            GraphInsertPolicy {
                defer_rebalance_to_maintenance: true,
                hard_overflow_chain_len: 8,
                ..GraphInsertPolicy::default()
            },
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 32_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 0_u64),
            (RegionKind::ReverseVertexTable, 32_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 0_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        manager
            .register_edge_segment(
                RegionKind::ForwardEdgeEntries,
                EdgeSegmentHeader::new(7, ExtentId::new(70), 1, 0, EdgeSegmentState::Active),
            )
            .expect("register forward segment a");
        manager
            .register_edge_segment(
                RegionKind::ForwardEdgeEntries,
                EdgeSegmentHeader::new(8, ExtentId::new(80), 2, 0, EdgeSegmentState::Active),
            )
            .expect("register forward segment b");
        manager
            .register_edge_segment(
                RegionKind::ReverseEdgeEntries,
                EdgeSegmentHeader::new(9, ExtentId::new(90), 1, 0, EdgeSegmentState::Active),
            )
            .expect("register reverse segment a");
        manager
            .register_edge_segment(
                RegionKind::ReverseEdgeEntries,
                EdgeSegmentHeader::new(10, ExtentId::new(100), 2, 0, EdgeSegmentState::Active),
            )
            .expect("register reverse segment b");
        let memory = VecMemory::default();

        let result = graph
            .insert_edge_pair_with_local_rebalance_and_segment_replacement_and_write_for_incoming_live_entries(
                RebalanceInsertSpec {
                    edge_id: 91,
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: VertexRef::from(1u8).into(),
                        src_ordinal: 0,
                        dst_vertex_ref: VertexRef::from(2u8).into(),
                        dst_ordinal: 0,
                    },
                    label_id: 11,
                    planned_incoming_live_entries: 1,
                    forward_rebalance_vertex_ids: &[VertexRef::from(1u8).into()],
                    forward_rebalance_base_edge_ids_by_ordinal: &[vec![91]],
                },
                &mut manager,
                &memory,
                0,
            )
            .expect("writeback result");

        assert!(result.rebalance.is_none());
        let GraphInsertResult::Inserted { path, .. } = result.insert.expect("insert result") else {
            panic!("expected inserted result");
        };
        assert_eq!(path, EdgeInsertPath::Overflow);
        assert_eq!(graph.forward.0.overflow_entries.len(), 1);
        assert_eq!(graph.reverse.0.overflow_entries.len(), 1);
    }

    #[test]
    fn graph_runtime_can_apply_local_rebalance_delta_and_rebuild_logical_locator_sidecar() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision")
        else {
            panic!("expected rebalance-required decision");
        };
        let local = graph
            .plan_local_rebalance(plan)
            .expect("local rebalance plan");
        let delta = graph
            .build_local_rebalance_delta(local)
            .expect("local rebalance delta");
        graph
            .apply_local_rebalance_delta_and_rebuild_logical_locator_sidecar(
                delta,
                &[VertexRef::from(1u8).into(), VertexRef::from(9u8).into()],
                &[vec![90, 91, 92], vec![93]],
            )
            .expect("apply local rebalance delta with logical locator sidecar rebuild");

        assert_eq!(
            graph.logical_locator(90),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                0,
            ))
        );
        assert_eq!(
            graph.logical_locator(91),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                1,
            ))
        );
        assert_eq!(
            graph.logical_locator(92),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                2,
            ))
        );
        assert_eq!(
            graph.logical_locator(93),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(9u8),
                0,
            ))
        );
    }

    #[test]
    fn graph_runtime_can_refresh_sidecar_only_for_rebalanced_window() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let mut sidecar = crate::low_level::EdgeLogicalLocatorSidecar::new();
        sidecar.set(
            200,
            crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(9u8),
                0,
            ),
        );
        graph.replace_logical_locator_sidecar(sidecar);

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision")
        else {
            panic!("expected rebalance-required decision");
        };
        let local = graph
            .plan_local_rebalance(plan)
            .expect("local rebalance plan");
        let delta = graph
            .build_local_rebalance_delta(local)
            .expect("local rebalance delta");
        graph
            .apply_local_rebalance_delta_and_refresh_logical_locator_sidecar_window(
                delta,
                &[VertexRef::from(1u8).into()],
                &[vec![90, 91, 92]],
            )
            .expect("apply local rebalance delta with logical locator sidecar window refresh");

        assert_eq!(
            graph.logical_locator(90),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                0,
            ))
        );
        assert_eq!(
            graph.logical_locator(91),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                1,
            ))
        );
        assert_eq!(
            graph.logical_locator(92),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                2,
            ))
        );
        assert_eq!(
            graph.logical_locator(200),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(9u8),
                0,
            ))
        );
    }

    #[test]
    fn graph_runtime_can_apply_rebalance_refresh_window_and_write() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 32_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 32_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        let memory = VecMemory::default();

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision")
        else {
            panic!("expected rebalance-required decision");
        };
        let local = graph
            .plan_local_rebalance(plan)
            .expect("local rebalance plan");
        let delta = graph
            .build_local_rebalance_delta(local)
            .expect("local rebalance delta");
        let refreshed = graph
            .apply_local_rebalance_delta_refresh_window_and_write(
                delta,
                &[VertexRef::from(1u8).into()],
                &[vec![90, 91, 92]],
                &mut manager,
                &memory,
            )
            .expect("apply rebalance and write");

        assert_eq!(refreshed.refreshed_forward_vertices, vec![0, 1]);
        assert_eq!(refreshed.refreshed_reverse_vertices, vec![0, 1]);
        assert_eq!(refreshed.apply.total_displacement(), 8);
        assert_eq!(refreshed.apply.max_displacement(), 4);
        assert_eq!(
            graph.logical_locator(91),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                1,
            ))
        );
        assert_eq!(
            graph.logical_locator(92),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                2,
            ))
        );
        assert!(!graph.forward.0.has_dirty_regions());
        assert!(!graph.reverse.0.has_dirty_regions());
    }

    #[test]
    fn graph_runtime_can_apply_segment_rebalance_refresh_window_and_write() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 32_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 32_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        let memory = VecMemory::default();

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(
                VertexRef::from(1u8).into(),
                0,
                VertexRef::from(2u8).into(),
                0,
            )
            .expect("decision")
        else {
            panic!("expected rebalance-required decision");
        };
        let local = graph
            .plan_local_rebalance(plan)
            .expect("local rebalance plan");
        let delta = graph
            .build_local_rebalance_delta(local)
            .expect("local rebalance delta");
        let refreshed = graph
            .apply_local_rebalance_delta_with_segment_replacement_refresh_window_and_write(
                delta,
                &[VertexRef::from(1u8).into()],
                &[vec![90, 91, 92]],
                &mut manager,
                &memory,
                55,
            )
            .expect("apply rebalance and write with segment replacement");

        assert_eq!(refreshed.refreshed_forward_vertices, vec![0, 1]);
        assert_eq!(refreshed.refreshed_reverse_vertices, vec![0, 1]);
        assert_eq!(refreshed.apply.apply.total_displacement(), 8);
        assert_eq!(refreshed.apply.segments.forward.new_segment.segment_id, 1);
        assert_eq!(refreshed.apply.segments.reverse.new_segment.segment_id, 1);
        assert_eq!(
            graph.logical_locator(91),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                1,
            ))
        );
        assert_eq!(
            graph.logical_locator(92),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                2,
            ))
        );
        assert!(!graph.forward.0.has_dirty_regions());
        assert!(!graph.reverse.0.has_dirty_regions());
    }

    #[test]
    fn graph_runtime_can_prepare_capacity_with_segment_replacement_and_write() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 16_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 16_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        let memory = VecMemory::default();

        let prepared = graph
            .ensure_local_capacity_for_incoming_live_entries_with_segment_replacement_and_write(
                RebalancePrepareSpec {
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: VertexRef::from(1u8).into(),
                        src_ordinal: 0,
                        dst_vertex_ref: VertexRef::from(2u8).into(),
                        dst_ordinal: 0,
                    },
                    planned_incoming_live_entries: 2,
                    forward_rebalance_vertex_ids: &[VertexRef::from(1u8).into()],
                    forward_rebalance_base_edge_ids_by_ordinal: &[vec![80, 90]],
                },
                &mut manager,
                &memory,
                111,
            )
            .expect("prepare capacity with segment replacement and write");

        assert!(prepared.rebalanced);
        assert_eq!(prepared.refreshed_forward_vertices, vec![0]);
        assert_eq!(prepared.refreshed_reverse_vertices, vec![0]);
        assert_eq!(
            prepared
                .rebalance
                .expect("rebalance summary")
                .apply
                .segments
                .forward
                .new_segment
                .segment_id,
            1
        );
        assert!(!graph.forward.0.has_dirty_regions());
        assert!(!graph.reverse.0.has_dirty_regions());
    }

    #[test]
    fn graph_runtime_can_insert_with_local_rebalance_and_segment_replacement() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );

        let result = graph
            .insert_edge_pair_with_local_rebalance_and_segment_replacement_for_incoming_live_entries(
                RebalanceInsertSpec {
                    edge_id: 91,
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: VertexRef::from(1u8).into(),
                        src_ordinal: 0,
                        dst_vertex_ref: VertexRef::from(2u8).into(),
                        dst_ordinal: 0,
                    },
                    label_id: 11,
                    planned_incoming_live_entries: 2,
                    forward_rebalance_vertex_ids: &[VertexRef::from(1u8).into()],
                    forward_rebalance_base_edge_ids_by_ordinal: &[vec![90, 91]],
                },
                &mut manager,
                121,
            )
            .expect("insert result");

        assert!(result.rebalance.is_some());
        assert_eq!(
            result
                .rebalance
                .expect("rebalance")
                .segments
                .forward
                .new_segment
                .segment_id,
            1
        );
        let GraphInsertResult::Inserted { path, .. } = result.insert.expect("insert result") else {
            panic!("expected inserted result");
        };
        assert_eq!(
            path,
            EdgeInsertPath::BaseReuseTombstone { logical_index: 2 }
        );
        assert_eq!(graph.forward.0.vertices[0].segment_id(), 1);
        assert_eq!(graph.reverse.0.vertices[0].segment_id(), 1);
        assert_eq!(
            graph.logical_locator(91),
            Some(crate::low_level::LogicalEdgeLocator::base(
                SurfaceKind::Forward,
                VertexRef::from(1u8),
                2,
            ))
        );
    }

    #[test]
    fn graph_runtime_can_insert_with_local_rebalance_and_segment_replacement_and_write() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(VertexRef::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 16_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 16_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        let memory = VecMemory::default();

        let result = graph
            .insert_edge_pair_with_local_rebalance_and_segment_replacement_and_write_for_incoming_live_entries(
                RebalanceInsertSpec {
                    edge_id: 91,
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: VertexRef::from(1u8).into(),
                        src_ordinal: 0,
                        dst_vertex_ref: VertexRef::from(2u8).into(),
                        dst_ordinal: 0,
                    },
                    label_id: 11,
                    planned_incoming_live_entries: 2,
                    forward_rebalance_vertex_ids: &[VertexRef::from(1u8).into()],
                    forward_rebalance_base_edge_ids_by_ordinal: &[vec![90, 92]],
                },
                &mut manager,
                &memory,
                131,
            )
            .expect("writeback result");

        assert!(result.rebalance.is_some());
        assert_eq!(result.refreshed_forward_vertices, vec![0]);
        assert_eq!(result.refreshed_reverse_vertices, vec![0]);
        let GraphInsertResult::Inserted { path, .. } = result.insert.expect("insert result") else {
            panic!("expected inserted result");
        };
        assert_eq!(
            path,
            EdgeInsertPath::BaseReuseTombstone { logical_index: 2 }
        );
        assert_eq!(
            result
                .rebalance
                .expect("rebalance summary")
                .apply
                .segments
                .forward
                .new_segment
                .segment_id,
            1
        );
        assert!(graph.logical_locator(90).is_some());
        assert!(graph.logical_locator(91).is_some());
        assert!(graph.logical_locator(92).is_some());
        assert!(!graph.forward.0.has_dirty_regions());
        assert!(!graph.reverse.0.has_dirty_regions());
    }

    #[test]
    fn graph_runtime_can_insert_via_one_rebalance_cycle_then_write() {
        let mut graph = GraphRuntime::with_insert_policy_and_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(VertexRef::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            GraphInsertPolicy {
                max_overflow_chain_len: 1,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 16_u64),
            (RegionKind::ForwardEdgeEntries, 24_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 20_u64),
            (RegionKind::ReverseVertexTable, 16_u64),
            (RegionKind::ReverseEdgeEntries, 24_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 20_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(0),
                ),
            );
            manager
                .set_region_logical_len(kind, logical_len)
                .expect("set logical len");
        }
        let memory = VecMemory::default();

        let result = graph
            .insert_edge_pair_with_local_rebalance_and_write(
                RebalanceInsertSpec {
                    edge_id: 91,
                    endpoints: EdgePairEndpoints {
                        src_vertex_ref: VertexRef::from(1u8).into(),
                        src_ordinal: 0,
                        dst_vertex_ref: VertexRef::from(2u8).into(),
                        dst_ordinal: 0,
                    },
                    label_id: 11,
                    planned_incoming_live_entries: 1,
                    forward_rebalance_vertex_ids: &[VertexRef::from(1u8).into()],
                    forward_rebalance_base_edge_ids_by_ordinal: &[vec![90, 92, 93]],
                },
                &mut manager,
                &memory,
            )
            .expect("writeback result");

        assert!(result.rebalance.is_some());
        assert_eq!(result.refreshed_forward_vertices, vec![0]);
        assert_eq!(result.refreshed_reverse_vertices, vec![0]);
        let GraphInsertResult::Inserted { path, .. } = result.insert.expect("insert result") else {
            panic!("expected inserted result");
        };
        assert_eq!(
            path,
            EdgeInsertPath::BaseReuseTombstone { logical_index: 3 }
        );
        assert!(graph.logical_locator(90).is_some());
        assert!(graph.logical_locator(91).is_some());
        assert!(graph.logical_locator(92).is_some());
        assert!(!graph.forward.0.has_dirty_regions());
        assert!(!graph.reverse.0.has_dirty_regions());
    }

    #[test]
    fn graph_runtime_tombstones_base_edge_pair_and_removes_sidecar() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
        );

        graph
            .tombstone_base_edge_pair(
                99,
                EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(2u8).into(),
                    dst_ordinal: 0,
                },
                EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::base(
                        SurfaceKind::Forward,
                        VertexRef::from(1u8),
                        0,
                    ),
                    reverse: LogicalEdgeLocator::base(
                        SurfaceKind::Reverse,
                        VertexRef::from(2u8),
                        0,
                    ),
                },
            )
            .expect("pair tombstone");

        assert!(
            graph
                .forward
                .0
                .base_entries
                .get(0)
                .expect("forward base entry 0")
                .meta
                .is_tombstone()
        );
        assert!(
            graph
                .reverse
                .0
                .base_entries
                .get(0)
                .expect("reverse base entry 0")
                .meta
                .is_tombstone()
        );
        assert_eq!(graph.logical_locator(99), None);
    }

    #[test]
    fn graph_runtime_replaces_base_edge_pair_on_both_surfaces() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
        );

        let (forward_old, reverse_old) = graph
            .replace_base_edge_pair(EdgeReplaceSpec {
                edge_id: 77,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(3u8).into(),
                    dst_ordinal: 0,
                },
                locators: EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::base(
                        SurfaceKind::Forward,
                        VertexRef::from(1u8),
                        0,
                    ),
                    reverse: LogicalEdgeLocator::base(
                        SurfaceKind::Reverse,
                        VertexRef::from(3u8),
                        0,
                    ),
                },
                label_id: 8,
            })
            .expect("pair replace");

        assert_eq!(u64::from(forward_old.target), 2);
        assert_eq!(u64::from(reverse_old.target), 1);
        let f0 = graph
            .forward
            .0
            .base_entries
            .get(0)
            .copied()
            .expect("forward base entry 0");
        let r0 = graph
            .reverse
            .0
            .base_entries
            .get(0)
            .copied()
            .expect("reverse base entry 0");
        assert_eq!(u64::from(f0.target), 3);
        assert_eq!(f0.meta.label_id(), 8);
        assert_eq!(u64::from(r0.target), 1);
        assert_eq!(r0.meta.label_id(), 8);
    }

    #[test]
    fn graph_runtime_can_refresh_and_write_dirty_surfaces() {
        let mut manager =
            RegionManager::with_bucket_size(crate::low_level::BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 16_u64),
            (RegionKind::ForwardEdgeEntries, 8_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 0_u64),
            (RegionKind::ReverseVertexTable, 16_u64),
            (RegionKind::ReverseEdgeEntries, 8_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 0_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(if logical_len == 0 { 1 } else { 0 }),
                ),
            );
        }
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(7, false),
                )],
                Vec::new(),
            ),
        );
        let memory = VecMemory::default();

        graph
            .replace_base_edge_pair(EdgeReplaceSpec {
                edge_id: 77,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(3u8).into(),
                    dst_ordinal: 0,
                },
                locators: EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::base(
                        SurfaceKind::Forward,
                        VertexRef::from(1u8),
                        0,
                    ),
                    reverse: LogicalEdgeLocator::base(
                        SurfaceKind::Reverse,
                        VertexRef::from(3u8),
                        0,
                    ),
                },
                label_id: 8,
            })
            .expect("pair replace");
        let refreshed = graph
            .refresh_and_write_dirty_to_stable_memory(&mut manager, &memory)
            .expect("refresh+write");

        assert_eq!(refreshed, (vec![0], vec![0]));
        assert!(!graph.forward.0.has_dirty_regions());
        assert!(!graph.reverse.0.has_dirty_regions());
        assert_eq!(
            manager
                .layout
                .region(RegionKind::ForwardLabelIndex)
                .expect("forward label region")
                .logical_len_bytes,
            28
        );
    }

    #[test]
    fn graph_runtime_replaces_overflow_pair_via_auto_path_selection() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 0, 0)],
                Vec::new(),
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 0, 0)],
                Vec::new(),
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
        );

        let (path, (forward_old, reverse_old)) = graph
            .replace_edge_pair(EdgeReplaceSpec {
                edge_id: 55,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(3u8).into(),
                    dst_ordinal: 0,
                },
                locators: EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::overflow(
                        SurfaceKind::Forward,
                        VertexRef::from(1u8),
                        0,
                    ),
                    reverse: LogicalEdgeLocator::overflow(
                        SurfaceKind::Reverse,
                        VertexRef::from(3u8),
                        0,
                    ),
                },
                label_id: 9,
            })
            .expect("replace via overflow path");

        assert_eq!(path, GraphMutationPath::Overflow);
        assert_eq!(u64::from(forward_old.target), 2);
        assert_eq!(u64::from(reverse_old.target), 1);
        assert_eq!(
            u64::from(graph.forward.0.overflow_entries[0].entry.target),
            3
        );
        assert_eq!(graph.forward.0.overflow_entries[0].entry.meta.label_id(), 9);
        assert_eq!(graph.reverse.0.overflow_entries[0].entry.meta.label_id(), 9);
    }

    #[test]
    fn graph_runtime_tombstones_overflow_pair_via_auto_path_selection() {
        let mut graph = GraphRuntime::new_with_empty_sidecars(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 0, 0)],
                Vec::new(),
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 0, 0)],
                Vec::new(),
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
        );

        let path = graph
            .tombstone_edge_pair(EdgeTombstoneSpec {
                edge_id: 55,
                endpoints: EdgePairEndpoints {
                    src_vertex_ref: VertexRef::from(1u8).into(),
                    src_ordinal: 0,
                    dst_vertex_ref: VertexRef::from(2u8).into(),
                    dst_ordinal: 0,
                },
                locators: EdgePairLogicalLocators {
                    forward: LogicalEdgeLocator::overflow(
                        SurfaceKind::Forward,
                        VertexRef::from(1u8),
                        0,
                    ),
                    reverse: LogicalEdgeLocator::overflow(
                        SurfaceKind::Reverse,
                        VertexRef::from(2u8),
                        0,
                    ),
                },
            })
            .expect("tombstone via overflow path");

        assert_eq!(path, GraphMutationPath::Overflow);
        assert!(
            graph.forward.0.overflow_entries[0]
                .entry
                .meta
                .is_tombstone()
        );
        assert!(
            graph.reverse.0.overflow_entries[0]
                .entry
                .meta
                .is_tombstone()
        );
        assert_eq!(graph.logical_locator(55), None);
    }
}
