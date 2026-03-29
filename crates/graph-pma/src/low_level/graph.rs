//! Thin graph-level coordination across forward and reverse adjacency surfaces.

use crate::stable::Memory;
use gleaph_graph_kernel::{EdgeId, LabelId, NodeId};

use super::edge::{EdgeEntry, EdgeLocator, EdgeMeta};
use super::hydration::{
    write_dirty_forward_surface_runtime_to_stable_memory,
    write_dirty_reverse_surface_runtime_to_stable_memory, WritebackError,
};
use super::locator::EdgeLocatorSidecar;
use super::manager::RegionManager;
use super::overflow::LogOffset;
use super::region::RegionKind;
use super::runtime::{
    BaseInsertDecision, EdgeInsertPath, ForwardSurfaceRuntime, ResolvedEdgeSlot,
    ReverseSurfaceRuntime, SurfaceAppliedRebalanceSummary, SurfaceLocalRebalanceDelta,
    SurfaceWeightedWindowLayout,
};
use super::vertex::EdgeIndex;

/// Thin graph-level runtime that coordinates forward/reverse surfaces and the
/// semantic `EdgeId -> EdgeLocator` sidecar.
///
/// This is intentionally still low-level. It does not yet perform allocator
/// growth or rebalance. It only ensures that one logical edge mutation updates
/// both directional surfaces and the canonical locator sidecar together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRuntime {
    pub forward: ForwardSurfaceRuntime,
    pub reverse: ReverseSurfaceRuntime,
    pub locator_sidecar: EdgeLocatorSidecar,
    pub insert_policy: GraphInsertPolicy,
}

/// Thin batch-mutation facade over one graph runtime.
///
/// The session keeps dirty in-memory state across multiple operations and
/// exposes an explicit flush step for the end of the batch.
pub struct GraphBatchMutationSession<'a, M: Memory> {
    graph: &'a mut GraphRuntime,
    manager: &'a mut RegionManager,
    memory: &'a M,
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
    /// Radius of the vertex-local rebalance window in vertex ordinals.
    pub rebalance_window_radius: usize,
    /// Minimum live degree at which local rebalance reserves extra base slack.
    pub high_degree_reserve_threshold: u32,
    /// Divisor used to compute extra reserved slack for high-degree vertices.
    pub high_degree_reserve_divisor: u32,
}

impl Default for GraphInsertPolicy {
    fn default() -> Self {
        Self {
            max_overflow_chain_len: 8,
            rebalance_window_radius: 1,
            high_degree_reserve_threshold: 4,
            high_degree_reserve_divisor: 2,
        }
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
        locators: (EdgeLocator, EdgeLocator),
    },
    /// The insert was not applied because local rebalance is required first.
    RebalanceRequired(GraphRebalancePlan),
}

/// One surface-local rebalance target identified during insert planning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceRebalancePlan {
    /// Vertex whose local neighborhood needs rebalance.
    pub vertex: NodeId,
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

/// Result of applying one graph-level rebalance and then flushing dirty regions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphAppliedRebalanceWriteSummary {
    pub apply: GraphAppliedRebalanceSummary,
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

/// Result of one insert helper that may rebalance before writing dirty state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphInsertWriteSummary {
    pub insert: Option<GraphInsertResult>,
    pub rebalance: Option<GraphAppliedRebalanceWriteSummary>,
    pub refreshed_forward_vertices: Vec<usize>,
    pub refreshed_reverse_vertices: Vec<usize>,
}

impl GraphRuntime {
    /// Starts one batch-mutation session over this graph runtime.
    pub fn begin_batch_mutation<'a, M: Memory>(
        &'a mut self,
        manager: &'a mut RegionManager,
        memory: &'a M,
    ) -> GraphBatchMutationSession<'a, M> {
        GraphBatchMutationSession::new(self, manager, memory)
    }

    fn prefers_local_rebalance_before_overflow(&self, plan: &GraphRebalancePlan) -> Option<bool> {
        let local = self.plan_local_rebalance(plan.clone())?;
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

    fn apply_local_rebalance_delta_to_surfaces(
        &mut self,
        delta: GraphLocalRebalanceDelta,
    ) -> Option<GraphAppliedRebalanceSummary> {
        let forward = self.forward.0.apply_local_rebalance_delta(delta.forward)?;
        let reverse = self.reverse.0.apply_local_rebalance_delta(delta.reverse)?;
        Some(GraphAppliedRebalanceSummary { forward, reverse })
    }

    /// Creates a thin graph-level runtime from forward/reverse surfaces and a locator sidecar.
    pub fn new(
        forward: ForwardSurfaceRuntime,
        reverse: ReverseSurfaceRuntime,
        locator_sidecar: EdgeLocatorSidecar,
    ) -> Self {
        Self::with_insert_policy(
            forward,
            reverse,
            locator_sidecar,
            GraphInsertPolicy::default(),
        )
    }

    /// Creates a thin graph-level runtime with an explicit insert policy.
    pub fn with_insert_policy(
        forward: ForwardSurfaceRuntime,
        reverse: ReverseSurfaceRuntime,
        locator_sidecar: EdgeLocatorSidecar,
        insert_policy: GraphInsertPolicy,
    ) -> Self {
        Self {
            forward,
            reverse,
            locator_sidecar,
            insert_policy,
        }
    }

    /// Appends one empty vertex slot to both directional surfaces.
    ///
    /// This is the minimal adjacency-side bootstrap step before any edge is
    /// inserted. It keeps forward and reverse ordinals aligned.
    pub fn append_empty_vertex_pair(&mut self) -> Option<(usize, usize)> {
        let forward = self.forward.append_empty_vertex()?;
        let reverse = self.reverse.append_empty_vertex()?;
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
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
    ) -> Option<GraphRebalancePlan> {
        self.plan_rebalance_for_insert_with_incoming_live_entries(
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            1,
        )
    }

    /// Builds a pure rebalance plan for an insert-like mutation that would add
    /// `incoming_live_entries` new live base candidates to both surfaces.
    pub fn plan_rebalance_for_insert_with_incoming_live_entries(
        &self,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        incoming_live_entries: u32,
    ) -> Option<GraphRebalancePlan> {
        let forward_vertex = self.forward.0.vertex_entry(src_ordinal)?;
        let reverse_vertex = self.reverse.0.vertex_entry(dst_ordinal)?;
        let forward_overflow_len = self
            .forward
            .overflow_entries_for(src_vertex, src_ordinal)?
            .len();
        let reverse_overflow_len = self
            .reverse
            .overflow_entries_for(dst_vertex, dst_ordinal)?
            .len();

        Some(GraphRebalancePlan {
            forward: SurfaceRebalancePlan {
                vertex: src_vertex,
                ordinal: src_ordinal,
                base_degree: forward_vertex.degree,
                overflow_len: forward_overflow_len,
                incoming_live_entries,
            },
            reverse: SurfaceRebalancePlan {
                vertex: dst_vertex,
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
        if live_degree < self.insert_policy.high_degree_reserve_threshold {
            return 0;
        }
        let divisor = self.insert_policy.high_degree_reserve_divisor.max(1);
        (live_degree / divisor).max(1)
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
    /// This rewrites the directional base slices and vertex metadata, but it
    /// does not yet rebuild the semantic `EdgeId -> EdgeLocator` mapping.
    /// Until a fresh sidecar is materialized from semantic edge ids, the
    /// existing sidecar is conservatively cleared.
    pub fn apply_local_rebalance_delta(
        &mut self,
        delta: GraphLocalRebalanceDelta,
    ) -> Option<GraphAppliedRebalanceSummary> {
        let summary = self.apply_local_rebalance_delta_to_surfaces(delta)?;
        self.locator_sidecar = EdgeLocatorSidecar::new();
        Some(summary)
    }

    /// Applies one local-rebalance delta and then rebuilds the canonical
    /// forward locator sidecar from externally supplied semantic edge ids.
    ///
    /// The caller must provide forward-surface vertex ids and base edge ids for
    /// every vertex ordinal in forward-surface order.
    pub fn apply_local_rebalance_delta_and_rebuild_sidecar(
        &mut self,
        delta: GraphLocalRebalanceDelta,
        forward_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<GraphAppliedRebalanceSummary> {
        let summary = self.apply_local_rebalance_delta_to_surfaces(delta)?;
        self.locator_sidecar = self.forward.0.build_locator_sidecar_from_vertex_base_ids(
            forward_vertex_ids,
            forward_base_edge_ids_by_ordinal,
        )?;
        Some(summary)
    }

    /// Applies one local-rebalance delta and refreshes only the affected
    /// forward-side locator mappings.
    ///
    /// The caller supplies semantic forward vertex ids and base edge ids only
    /// for the rewritten forward window.
    pub fn apply_local_rebalance_delta_and_refresh_sidecar_window(
        &mut self,
        delta: GraphLocalRebalanceDelta,
        forward_vertex_ids: &[NodeId],
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

        self.locator_sidecar.retain(|_, locator| {
            !(locator.surface_kind() == super::edge::SurfaceKind::Forward
                && forward_vertex_ids.contains(&locator.vertex))
        });

        self.forward.0.populate_locator_sidecar_for_window(
            start_ordinal,
            forward_vertex_ids,
            forward_base_edge_ids_by_ordinal,
            &mut self.locator_sidecar,
        )?;
        Some(summary)
    }

    /// Applies a local rebalance delta, refreshes the affected forward-side
    /// locator window, then rebuilds dirty label sidecars and flushes dirty
    /// regions to stable memory.
    pub fn apply_local_rebalance_delta_refresh_window_and_write(
        &mut self,
        delta: GraphLocalRebalanceDelta,
        forward_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<GraphAppliedRebalanceWriteSummary, WritebackError> {
        let apply = self
            .apply_local_rebalance_delta_and_refresh_sidecar_window(
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

    /// Applies a local rebalance delta, rebuilds the full forward canonical
    /// sidecar, then rebuilds dirty label sidecars and flushes dirty regions to
    /// stable memory.
    pub fn apply_local_rebalance_delta_rebuild_sidecar_and_write(
        &mut self,
        delta: GraphLocalRebalanceDelta,
        forward_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<GraphAppliedRebalanceWriteSummary, WritebackError> {
        let apply = self
            .apply_local_rebalance_delta_and_rebuild_sidecar(
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
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        planned_incoming_live_entries: u32,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_rebalance_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<bool> {
        let decision = self.choose_insert_decision_with_incoming_live_entries(
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            planned_incoming_live_entries,
        )?;
        let GraphInsertDecision::RebalanceRequired(plan) = decision else {
            return Some(false);
        };

        let local = self.plan_local_rebalance(plan)?;
        let delta = self.build_local_rebalance_delta(local)?;
        self.apply_local_rebalance_delta_and_refresh_sidecar_window(
            delta,
            forward_rebalance_vertex_ids,
            forward_rebalance_base_edge_ids_by_ordinal,
        )?;
        Some(true)
    }

    /// Ensures local capacity for an upcoming batch of live entries and writes
    /// back any resulting dirty state to stable memory.
    pub fn ensure_local_capacity_for_incoming_live_entries_and_write(
        &mut self,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        planned_incoming_live_entries: u32,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_rebalance_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<GraphEnsureCapacityWriteSummary, WritebackError> {
        let decision = self
            .choose_insert_decision_with_incoming_live_entries(
                src_vertex,
                src_ordinal,
                dst_vertex,
                dst_ordinal,
                planned_incoming_live_entries,
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
                    forward_rebalance_vertex_ids,
                    forward_rebalance_base_edge_ids_by_ordinal,
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

    /// Inserts one logical edge, performing one local rebalance cycle first if
    /// the current insert decision requires it.
    ///
    /// The caller supplies semantic forward vertex ids and base edge ids for
    /// the forward rebalance window after compaction and before the new edge is
    /// inserted.
    pub fn insert_edge_pair_with_local_rebalance(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_rebalance_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<GraphInsertResult> {
        self.insert_edge_pair_with_local_rebalance_for_incoming_live_entries(
            edge_id,
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            label_id,
            1,
            forward_rebalance_vertex_ids,
            forward_rebalance_base_edge_ids_by_ordinal,
        )
    }

    /// Inserts one logical edge while planning for a larger incoming live-entry batch.
    pub fn insert_edge_pair_with_local_rebalance_for_incoming_live_entries(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        planned_incoming_live_entries: u32,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_rebalance_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<GraphInsertResult> {
        match self.choose_insert_decision_with_incoming_live_entries(
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            planned_incoming_live_entries,
        )? {
            GraphInsertDecision::RebalanceRequired(plan) => {
                let local = self.plan_local_rebalance(plan)?;
                let delta = self.build_local_rebalance_delta(local)?;
                self.apply_local_rebalance_delta_and_refresh_sidecar_window(
                    delta,
                    forward_rebalance_vertex_ids,
                    forward_rebalance_base_edge_ids_by_ordinal,
                )?;
                self.insert_edge_pair(
                    edge_id,
                    src_vertex,
                    src_ordinal,
                    dst_vertex,
                    dst_ordinal,
                    label_id,
                )
            }
            _ => self.insert_edge_pair(
                edge_id,
                src_vertex,
                src_ordinal,
                dst_vertex,
                dst_ordinal,
                label_id,
            ),
        }
    }

    /// Inserts one logical edge, performing one local rebalance cycle first if
    /// needed, then refreshes dirty label sidecars and flushes dirty regions to
    /// stable memory.
    pub fn insert_edge_pair_with_local_rebalance_and_write(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_rebalance_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        self.insert_edge_pair_with_local_rebalance_and_write_for_incoming_live_entries(
            edge_id,
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            label_id,
            1,
            forward_rebalance_vertex_ids,
            forward_rebalance_base_edge_ids_by_ordinal,
            manager,
            memory,
        )
    }

    /// Inserts one logical edge and writes back dirty state, while planning
    /// local rebalance as if a larger batch were about to arrive.
    pub fn insert_edge_pair_with_local_rebalance_and_write_for_incoming_live_entries(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        planned_incoming_live_entries: u32,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_rebalance_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        manager: &mut RegionManager,
        memory: &impl Memory,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        let decision = self
            .choose_insert_decision_with_incoming_live_entries(
                src_vertex,
                src_ordinal,
                dst_vertex,
                dst_ordinal,
                planned_incoming_live_entries,
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
                    forward_rebalance_vertex_ids,
                    forward_rebalance_base_edge_ids_by_ordinal,
                    manager,
                    memory,
                )?)
            }
            _ => None,
        };

        let insert = self.insert_edge_pair(
            edge_id,
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            label_id,
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

    /// Chooses the current insertion decision for a new logical edge.
    ///
    /// The base path is taken only when both directional surfaces can append at
    /// the tail of the corresponding canonical base interval without shifting
    /// later base entries. Otherwise the runtime uses overflow until the local
    /// chain length reaches the policy limit, after which it asks for rebalance.
    pub fn choose_insert_decision(
        &self,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
    ) -> Option<GraphInsertDecision> {
        self.choose_insert_decision_with_incoming_live_entries(
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            1,
        )
    }

    /// Chooses the current insertion decision for a mutation that would add
    /// `incoming_live_entries` new live entries to both directional surfaces.
    pub fn choose_insert_decision_with_incoming_live_entries(
        &self,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        incoming_live_entries: u32,
    ) -> Option<GraphInsertDecision> {
        let forward_base = self.forward.choose_base_insert_slot(src_ordinal);
        let reverse_base = self.reverse.choose_base_insert_slot(dst_ordinal);
        if incoming_live_entries == 1 {
            if let (Some(forward_base), Some(reverse_base)) = (forward_base, reverse_base) {
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
        }

        let forward_overflow_len = self
            .forward
            .overflow_entries_for(src_vertex, src_ordinal)?
            .len();
        let reverse_overflow_len = self
            .reverse
            .overflow_entries_for(dst_vertex, dst_ordinal)?
            .len();
        let rebalance_plan = self.plan_rebalance_for_insert_with_incoming_live_entries(
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            incoming_live_entries,
        )?;
        if self.prefers_local_rebalance_before_overflow(&rebalance_plan)? {
            return Some(GraphInsertDecision::RebalanceRequired(rebalance_plan));
        }
        if incoming_live_entries == 1
            && forward_overflow_len < self.insert_policy.max_overflow_chain_len
            && reverse_overflow_len < self.insert_policy.max_overflow_chain_len
        {
            Some(GraphInsertDecision::Overflow)
        } else {
            Some(GraphInsertDecision::RebalanceRequired(rebalance_plan))
        }
    }

    /// Appends one logical edge directly to the tail of both canonical base intervals.
    pub fn insert_base_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
    ) -> Option<(EdgeInsertPath, EdgeInsertPath, (EdgeLocator, EdgeLocator))> {
        let forward_entry = EdgeEntry::new(dst_vertex, EdgeMeta::new(label_id, false));
        let reverse_entry = EdgeEntry::new(src_vertex, EdgeMeta::new(label_id, false));

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

        let forward_locator =
            self.forward
                .edge_locator_for(src_vertex, src_ordinal, forward_logical_index)?;
        let reverse_locator =
            self.reverse
                .edge_locator_for(dst_vertex, dst_ordinal, reverse_logical_index)?;
        self.locator_sidecar.set(edge_id, forward_locator);
        Some((
            forward_path,
            reverse_path,
            (forward_locator, reverse_locator),
        ))
    }

    /// Appends one logical edge as paired overflow entries on both directional surfaces.
    pub fn append_overflow_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
    ) -> Option<(EdgeLocator, EdgeLocator)> {
        let forward_entry = EdgeEntry::new(dst_vertex, EdgeMeta::new(label_id, false));
        let reverse_entry = EdgeEntry::new(src_vertex, EdgeMeta::new(label_id, false));

        let forward_offset =
            self.forward
                .append_overflow_entry(src_vertex, src_ordinal, edge_id, forward_entry)?;
        let reverse_offset =
            self.reverse
                .append_overflow_entry(dst_vertex, dst_ordinal, edge_id, reverse_entry)?;

        let forward_locator = EdgeLocator::new(
            self.forward.0.layout.kind,
            src_vertex,
            u32::try_from(forward_offset.raw).ok()?,
        );
        let reverse_locator = EdgeLocator::new(
            self.reverse.0.layout.kind,
            dst_vertex,
            u32::try_from(reverse_offset.raw).ok()?,
        );
        self.locator_sidecar.set(edge_id, forward_locator);
        Some((forward_locator, reverse_locator))
    }

    /// Inserts one logical edge, choosing base-tail append when both surfaces
    /// can support it and otherwise falling back to overflow.
    pub fn insert_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
    ) -> Option<GraphInsertResult> {
        let decision =
            self.choose_insert_decision(src_vertex, src_ordinal, dst_vertex, dst_ordinal)?;
        let (path, locators) = match decision {
            GraphInsertDecision::BaseInsert {
                forward_path,
                reverse_path,
            } => {
                let (actual_forward_path, actual_reverse_path, locators) = self
                    .insert_base_edge_pair(
                        edge_id,
                        src_vertex,
                        src_ordinal,
                        dst_vertex,
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
                    src_vertex,
                    src_ordinal,
                    dst_vertex,
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
        src_ordinal: usize,
        src_logical_index: usize,
        dst_ordinal: usize,
        dst_logical_index: usize,
    ) -> Option<()> {
        let locator = self.locator_sidecar.get(edge_id)?;
        let src_vertex = locator.vertex;

        let forward_entry = self
            .forward
            .tombstone_base_entry(src_ordinal, src_logical_index)?;
        let reverse_vertex = forward_entry.target;
        let _ = self
            .reverse
            .tombstone_base_entry(dst_ordinal, dst_logical_index)?;

        let _ = src_vertex;
        let _ = reverse_vertex;
        self.locator_sidecar.remove(edge_id)?;
        Some(())
    }

    /// Replaces one logical base edge on both directional surfaces.
    pub fn replace_base_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
    ) -> Option<(EdgeEntry, EdgeEntry)> {
        let _ = self.locator_sidecar.get(edge_id)?;

        let forward_old = self.forward.replace_base_entry(
            src_ordinal,
            src_logical_index,
            EdgeEntry::new(dst_vertex, EdgeMeta::new(label_id, false)),
        )?;
        let reverse_old = self.reverse.replace_base_entry(
            dst_ordinal,
            dst_logical_index,
            EdgeEntry::new(src_vertex, EdgeMeta::new(label_id, false)),
        )?;

        Some((forward_old, reverse_old))
    }

    /// Replaces one logical edge on both directional surfaces, choosing base
    /// or overflow handling from the canonical forward locator.
    ///
    /// The caller still supplies the reverse-side ordinal and the base logical
    /// indexes needed for the base path. The runtime decides whether the edge
    /// currently lives in the canonical base interval or in the overflow log.
    pub fn replace_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
    ) -> Option<(GraphMutationPath, (EdgeEntry, EdgeEntry))> {
        let locator = self.canonical_forward_locator(edge_id)?;
        match self
            .forward
            .resolve_edge_slot(src_vertex, src_ordinal, locator)?
        {
            ResolvedEdgeSlot::Base { .. } => self
                .replace_base_edge_pair(
                    edge_id,
                    src_vertex,
                    src_ordinal,
                    src_logical_index,
                    dst_vertex,
                    dst_ordinal,
                    dst_logical_index,
                    label_id,
                )
                .map(|entries| (GraphMutationPath::Base, entries)),
            ResolvedEdgeSlot::Overflow { .. } => {
                let forward_old = self.forward.replace_overflow_entry(
                    src_vertex,
                    src_ordinal,
                    edge_id,
                    EdgeEntry::new(dst_vertex, EdgeMeta::new(label_id, false)),
                )?;
                let reverse_old = self.reverse.replace_overflow_entry(
                    dst_vertex,
                    dst_ordinal,
                    edge_id,
                    EdgeEntry::new(src_vertex, EdgeMeta::new(label_id, false)),
                )?;
                Some((
                    GraphMutationPath::Overflow,
                    (forward_old.entry, reverse_old.entry),
                ))
            }
        }
    }

    /// Tombstones one logical edge on both directional surfaces, choosing the
    /// base or overflow path from the canonical forward locator.
    pub fn tombstone_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
    ) -> Option<GraphMutationPath> {
        let locator = self.canonical_forward_locator(edge_id)?;
        match self
            .forward
            .resolve_edge_slot(src_vertex, src_ordinal, locator)?
        {
            ResolvedEdgeSlot::Base { .. } => self
                .tombstone_base_edge_pair(
                    edge_id,
                    src_ordinal,
                    src_logical_index,
                    dst_ordinal,
                    dst_logical_index,
                )
                .map(|()| GraphMutationPath::Base),
            ResolvedEdgeSlot::Overflow { .. } => {
                let _ = self
                    .forward
                    .tombstone_overflow_entry(src_vertex, src_ordinal, edge_id)?;
                let _ = self
                    .reverse
                    .tombstone_overflow_entry(dst_vertex, dst_ordinal, edge_id)?;
                self.locator_sidecar.remove(edge_id)?;
                Some(GraphMutationPath::Overflow)
            }
        }
    }

    /// Refreshes label sidecars only for vertices marked dirty on each surface.
    pub fn refresh_label_sidecars(&mut self) -> Option<(Vec<usize>, Vec<usize>)> {
        let forward = self.forward.refresh_label_sidecar_for_dirty_vertices()?;
        let reverse = self.reverse.refresh_label_sidecar_for_dirty_vertices()?;
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
        let refreshed = self
            .refresh_label_sidecars()
            .expect("dirty vertex refresh should be valid before writeback");
        self.write_dirty_to_stable_memory(manager, memory)?;
        Ok(refreshed)
    }

    /// Looks up the canonical locator currently stored for a semantic edge id.
    pub fn locator(&self, edge_id: EdgeId) -> Option<EdgeLocator> {
        self.locator_sidecar.get(edge_id)
    }

    /// Returns the canonical locator only if it points into the forward surface.
    pub fn canonical_forward_locator(&self, edge_id: EdgeId) -> Option<EdgeLocator> {
        let locator = self.locator(edge_id)?;
        (locator.surface_kind() == super::edge::SurfaceKind::Forward).then_some(locator)
    }

    /// Returns the current forward overflow-log head for one vertex ordinal.
    pub fn forward_log_offset_for(&self, ordinal: usize) -> Option<LogOffset> {
        let entry = self.forward.0.vertex_entry(ordinal)?;
        Some(LogOffset::new(entry.log_offset))
    }
}

impl<'a, M: Memory> GraphBatchMutationSession<'a, M> {
    /// Creates one batch-mutation session over a graph runtime.
    pub fn new(graph: &'a mut GraphRuntime, manager: &'a mut RegionManager, memory: &'a M) -> Self {
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
    pub fn prepare_local_capacity(
        &mut self,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        planned_incoming_live_entries: u32,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<bool> {
        self.graph.ensure_local_capacity_for_incoming_live_entries(
            src_vertex,
            src_ordinal,
            dst_vertex,
            dst_ordinal,
            planned_incoming_live_entries,
            forward_rebalance_vertex_ids,
            forward_base_edge_ids_by_ordinal,
        )
    }

    /// Inserts one edge using the batch-aware rebalance path without flushing yet.
    pub fn insert_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        label_id: LabelId,
        planned_incoming_live_entries: u32,
        forward_rebalance_vertex_ids: &[NodeId],
        forward_base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<GraphInsertResult> {
        self.graph
            .insert_edge_pair_with_local_rebalance_for_incoming_live_entries(
                edge_id,
                src_vertex,
                src_ordinal,
                dst_vertex,
                dst_ordinal,
                label_id,
                planned_incoming_live_entries,
                forward_rebalance_vertex_ids,
                forward_base_edge_ids_by_ordinal,
            )
    }

    /// Replaces one logical edge without flushing yet.
    pub fn replace_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
        label_id: LabelId,
    ) -> Option<(GraphMutationPath, (EdgeEntry, EdgeEntry))> {
        self.graph.replace_edge_pair(
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
            label_id,
        )
    }

    /// Tombstones one logical edge without flushing yet.
    pub fn tombstone_edge_pair(
        &mut self,
        edge_id: EdgeId,
        src_vertex: NodeId,
        src_ordinal: usize,
        src_logical_index: usize,
        dst_vertex: NodeId,
        dst_ordinal: usize,
        dst_logical_index: usize,
    ) -> Option<GraphMutationPath> {
        self.graph.tombstone_edge_pair(
            edge_id,
            src_vertex,
            src_ordinal,
            src_logical_index,
            dst_vertex,
            dst_ordinal,
            dst_logical_index,
        )
    }

    /// Flushes dirty graph state accumulated so far in this batch.
    pub fn flush(&mut self) -> Result<(Vec<usize>, Vec<usize>), WritebackError> {
        self.graph
            .refresh_and_write_dirty_to_stable_memory(self.manager, self.memory)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GraphBatchMutationSession, GraphInsertDecision, GraphInsertPolicy, GraphInsertResult,
        GraphMutationPath, GraphRuntime,
    };
    use crate::low_level::{
        EdgeEntry, EdgeIndex, EdgeInsertPath, EdgeLocatorSidecar, EdgeMeta, ExtentChain, ExtentId,
        ForwardSurface, ForwardSurfaceRuntime, LogOffset, OverflowEntry, RegionKind, RegionManager,
        RegionRef, RegionStorageKind, ReverseSurface, ReverseSurfaceRuntime, SurfaceKind,
        SurfaceRegions, VertexEntry, WasmPages, EMPTY_LOG_OFFSET,
    };
    use crate::stable::VecMemory;
    use gleaph_graph_kernel::NodeId;

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
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            EdgeLocatorSidecar::new(),
        );

        let (forward, reverse) = graph
            .append_overflow_edge_pair(55, NodeId::from(1u8), 0, NodeId::from(2u8), 0, 9)
            .expect("pair append");

        assert_eq!(forward.surface_kind(), SurfaceKind::Forward);
        assert_eq!(reverse.surface_kind(), SurfaceKind::Reverse);
        assert_eq!(graph.forward.0.overflow_entries.len(), 1);
        assert_eq!(graph.reverse.0.overflow_entries.len(), 1);
        assert_eq!(graph.locator(55), Some(forward));
        assert_eq!(graph.forward.0.vertices[0].log_offset, 0);
        assert_eq!(graph.reverse.0.vertices[0].log_offset, 0);
    }

    #[test]
    fn graph_runtime_can_append_empty_vertex_pair() {
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::without_overflow(forward_surface(), Vec::new()),
            ReverseSurfaceRuntime::without_overflow(reverse_surface(), Vec::new()),
            EdgeLocatorSidecar::new(),
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
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::without_overflow(forward_surface(), Vec::new()),
            ReverseSurfaceRuntime::without_overflow(reverse_surface(), Vec::new()),
            EdgeLocatorSidecar::new(),
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
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            EdgeLocatorSidecar::new(),
        );

        let GraphInsertResult::Inserted {
            path,
            locators: (forward, reverse),
        } = graph
            .insert_edge_pair(88, NodeId::from(1u8), 0, NodeId::from(2u8), 0, 9)
            .expect("base append")
        else {
            panic!("expected inserted result");
        };

        assert_eq!(path, EdgeInsertPath::BaseAppend { logical_index: 1 });
        assert_eq!(forward.surface_kind(), SurfaceKind::Forward);
        assert_eq!(reverse.surface_kind(), SurfaceKind::Reverse);
        assert_eq!(graph.forward.0.base_entries.len(), 2);
        assert_eq!(graph.reverse.0.base_entries.len(), 2);
        assert_eq!(graph.forward.0.vertices[0].degree, 2);
        assert_eq!(graph.reverse.0.vertices[0].degree, 2);
        assert_eq!(graph.locator(88), Some(forward));
    }

    #[test]
    fn graph_runtime_falls_back_to_overflow_when_base_tail_append_is_not_possible() {
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::new(1), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
                ],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            EdgeLocatorSidecar::new(),
        );

        let GraphInsertResult::Inserted { path, .. } = graph
            .insert_edge_pair(89, NodeId::from(1u8), 0, NodeId::from(2u8), 0, 9)
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
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(99u8), EdgeMeta::new(12, false)),
                ],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
                vec![
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(8u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(88u8), EdgeMeta::new(12, false)),
                ],
                Vec::new(),
            ),
            EdgeLocatorSidecar::new(),
        );

        let GraphInsertResult::Inserted { path, .. } = graph
            .insert_edge_pair(189, NodeId::from(1u8), 0, NodeId::from(2u8), 0, 9)
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
        assert_eq!(u64::from(graph.forward.0.base_entries[1].target), 2);
        assert!(!graph.forward.0.base_entries[1].meta.is_tombstone());
    }

    #[test]
    fn graph_runtime_can_request_rebalance_before_inserting() {
        let mut graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 1, 0),
                    VertexEntry::new(EdgeIndex::new(1), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
                ],
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
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
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(8, false)),
                ],
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
            EdgeLocatorSidecar::new(),
            GraphInsertPolicy {
                max_overflow_chain_len: 1,
                rebalance_window_radius: 1,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision(NodeId::from(1u8), 0, NodeId::from(2u8), 0)
            .expect("decision");
        let GraphInsertDecision::RebalanceRequired(plan) = decision else {
            panic!("expected rebalance-required decision");
        };
        assert_eq!(plan.forward.vertex, NodeId::from(1u8));
        assert_eq!(plan.forward.ordinal, 0);
        assert_eq!(plan.forward.base_degree, 1);
        assert_eq!(plan.forward.overflow_len, 1);
        assert_eq!(plan.forward.incoming_live_entries, 1);
        assert_eq!(plan.reverse.vertex, NodeId::from(2u8));
        assert_eq!(plan.reverse.ordinal, 0);
        assert_eq!(plan.reverse.base_degree, 1);
        assert_eq!(plan.reverse.overflow_len, 1);
        assert_eq!(plan.reverse.incoming_live_entries, 1);

        let result = graph
            .insert_edge_pair(90, NodeId::from(1u8), 0, NodeId::from(2u8), 0, 9)
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
        let graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            EdgeLocatorSidecar::new(),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let plan = graph
            .plan_rebalance_for_insert_with_incoming_live_entries(
                NodeId::from(1u8),
                0,
                NodeId::from(2u8),
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
        let graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::new(1), 1, 1),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![
                    OverflowEntry::new(
                        55,
                        EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                        LogOffset::EMPTY,
                    ),
                    OverflowEntry::new(
                        56,
                        EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
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
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(9, false)),
                ],
                vec![
                    OverflowEntry::new(
                        55,
                        EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                        LogOffset::EMPTY,
                    ),
                    OverflowEntry::new(
                        56,
                        EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(8, false)),
                        LogOffset::EMPTY,
                    ),
                ],
            ),
            EdgeLocatorSidecar::new(),
            GraphInsertPolicy {
                max_overflow_chain_len: 1,
                rebalance_window_radius: 1,
                ..GraphInsertPolicy::default()
            },
        );

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(NodeId::from(1u8), 1, NodeId::from(2u8), 1)
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
        let graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 3, EMPTY_LOG_OFFSET),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(NodeId::from(8u8), EdgeMeta::new(10, false)),
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
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(10, false)),
                ],
                Vec::new(),
            ),
            EdgeLocatorSidecar::new(),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 1,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision(NodeId::from(1u8), 0, NodeId::from(2u8), 0)
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
        let graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            EdgeLocatorSidecar::new(),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision_with_incoming_live_entries(
                NodeId::from(1u8),
                0,
                NodeId::from(2u8),
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
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            90,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            sidecar,
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let result = graph
            .insert_edge_pair_with_local_rebalance_for_incoming_live_entries(
                91,
                NodeId::from(1u8),
                0,
                NodeId::from(2u8),
                0,
                11,
                2,
                &[NodeId::from(1u8)],
                &[vec![90, 91]],
            )
            .expect("insert result");

        let GraphInsertResult::Inserted { path, .. } = result else {
            panic!("expected inserted result");
        };
        assert_eq!(
            path,
            EdgeInsertPath::BaseReuseTombstone { logical_index: 2 }
        );
        assert_eq!(
            graph.locator(91),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                2,
            ))
        );
    }

    #[test]
    fn graph_runtime_can_prepare_local_capacity_for_batch_without_inserting() {
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            80,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        sidecar.set(
            90,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            sidecar,
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let rebalanced = graph
            .ensure_local_capacity_for_incoming_live_entries(
                NodeId::from(1u8),
                0,
                NodeId::from(2u8),
                0,
                2,
                &[NodeId::from(1u8)],
                &[vec![80, 90]],
            )
            .expect("prepare capacity");
        assert!(rebalanced);
        assert_eq!(graph.forward.0.vertices[0].log_offset, EMPTY_LOG_OFFSET);
        assert_eq!(graph.reverse.0.vertices[0].log_offset, EMPTY_LOG_OFFSET);
        assert_eq!(graph.forward.0.vertices[0].degree, 2);
        assert_eq!(graph.reverse.0.vertices[0].degree, 2);
        assert_eq!(
            graph.locator(80),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                0,
            ))
        );
        assert_eq!(
            graph.locator(90),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
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

        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            80,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        sidecar.set(
            90,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            sidecar,
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );
        let memory = VecMemory::default();

        let mut batch = GraphBatchMutationSession::new(&mut graph, &mut manager, &memory);
        let prepared = batch
            .prepare_local_capacity(
                NodeId::from(1u8),
                0,
                NodeId::from(2u8),
                0,
                2,
                &[NodeId::from(1u8)],
                &[vec![80, 90]],
            )
            .expect("prepare");
        assert!(prepared);

        let inserted = batch
            .insert_edge_pair(
                91,
                NodeId::from(1u8),
                0,
                NodeId::from(2u8),
                0,
                11,
                2,
                &[NodeId::from(1u8)],
                &[vec![80, 90]],
            )
            .expect("insert");
        let GraphInsertResult::Inserted { .. } = inserted else {
            panic!("expected inserted result");
        };

        let refreshed = batch.flush().expect("flush");
        assert_eq!(refreshed.0, vec![0]);
        assert_eq!(refreshed.1, vec![0]);
        assert_eq!(
            batch.graph().locator(91),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                2,
            ))
        );
    }

    #[test]
    fn batch_mutation_session_can_replace_and_tombstone_edges() {
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            EdgeLocatorSidecar::new(),
        );
        graph
            .insert_base_edge_pair(77, NodeId::from(1u8), 0, NodeId::from(2u8), 0, 7)
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

        let mut batch = GraphBatchMutationSession::new(&mut graph, &mut manager, &memory);
        let replaced = batch
            .replace_edge_pair(77, NodeId::from(1u8), 0, 0, NodeId::from(3u8), 0, 0, 9)
            .expect("replace");
        assert_eq!(replaced.0, GraphMutationPath::Base);

        let tombstoned = batch
            .tombstone_edge_pair(77, NodeId::from(1u8), 0, 0, NodeId::from(3u8), 0, 0)
            .expect("tombstone");
        assert_eq!(tombstoned, GraphMutationPath::Base);
    }

    #[test]
    fn graph_runtime_keeps_overflow_path_when_window_slack_cannot_absorb_existing_overflow() {
        let graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 3, 0),
                    VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(NodeId::from(8u8), EdgeMeta::new(11, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
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
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(8, true)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(9, false)),
                    EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(11, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            EdgeLocatorSidecar::new(),
            GraphInsertPolicy {
                max_overflow_chain_len: 8,
                rebalance_window_radius: 1,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision(NodeId::from(1u8), 0, NodeId::from(2u8), 0)
            .expect("decision");
        assert_eq!(decision, GraphInsertDecision::Overflow);
    }

    #[test]
    fn graph_runtime_can_build_local_rebalance_delta_from_local_plan() {
        let graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
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
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            EdgeLocatorSidecar::new(),
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let decision = graph
            .choose_insert_decision(NodeId::from(1u8), 0, NodeId::from(2u8), 0)
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
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            90,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
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
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            sidecar,
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(NodeId::from(1u8), 0, NodeId::from(2u8), 0)
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
        assert_eq!(graph.locator(90), None);
    }

    #[test]
    fn graph_runtime_can_apply_local_rebalance_delta_and_rebuild_sidecar() {
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            90,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
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
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            sidecar,
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(NodeId::from(1u8), 0, NodeId::from(2u8), 0)
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
            .apply_local_rebalance_delta_and_rebuild_sidecar(
                delta,
                &[NodeId::from(1u8), NodeId::from(9u8)],
                &[vec![90, 91, 92], vec![93]],
            )
            .expect("apply local rebalance delta with sidecar rebuild");

        assert_eq!(
            graph.locator(90),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                0,
            ))
        );
        assert_eq!(
            graph.locator(91),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                1,
            ))
        );
        assert_eq!(
            graph.locator(92),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                2,
            ))
        );
        assert_eq!(
            graph.locator(93),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(9u8),
                6,
            ))
        );
    }

    #[test]
    fn graph_runtime_can_refresh_sidecar_only_for_rebalanced_window() {
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            90,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        sidecar.set(
            200,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(9u8), 2),
        );
        let mut graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
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
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            sidecar,
            GraphInsertPolicy {
                max_overflow_chain_len: 0,
                rebalance_window_radius: 0,
                ..GraphInsertPolicy::default()
            },
        );

        let GraphInsertDecision::RebalanceRequired(plan) = graph
            .choose_insert_decision(NodeId::from(1u8), 0, NodeId::from(2u8), 0)
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
            .apply_local_rebalance_delta_and_refresh_sidecar_window(
                delta,
                &[NodeId::from(1u8)],
                &[vec![90, 91, 92]],
            )
            .expect("apply local rebalance delta with sidecar window refresh");

        assert_eq!(
            graph.locator(90),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                0,
            ))
        );
        assert_eq!(
            graph.locator(91),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                1,
            ))
        );
        assert_eq!(
            graph.locator(92),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                2,
            ))
        );
        assert_eq!(
            graph.locator(200),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(9u8),
                2,
            ))
        );
    }

    #[test]
    fn graph_runtime_can_apply_rebalance_refresh_window_and_write() {
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            90,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![
                    VertexEntry::new(EdgeIndex::new(0), 2, 0),
                    VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
                ],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
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
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(7u8), EdgeMeta::new(9, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            sidecar,
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
            .choose_insert_decision(NodeId::from(1u8), 0, NodeId::from(2u8), 0)
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
                &[NodeId::from(1u8)],
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
            graph.locator(91),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                1,
            ))
        );
        assert_eq!(
            graph.locator(92),
            Some(crate::low_level::EdgeLocator::new(
                SurfaceKind::Forward,
                NodeId::from(1u8),
                2,
            ))
        );
        assert!(!graph.forward.0.has_dirty_regions());
        assert!(!graph.reverse.0.has_dirty_regions());
    }

    #[test]
    fn graph_runtime_can_insert_via_one_rebalance_cycle_then_write() {
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            90,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::with_insert_policy(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(99u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
                vec![
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(8, false)),
                    EdgeEntry::new(NodeId::from(88u8), EdgeMeta::new(12, false)),
                ],
                vec![OverflowEntry::new(
                    90,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(10, false)),
                    LogOffset::EMPTY,
                )],
            ),
            sidecar,
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
                91,
                NodeId::from(1u8),
                0,
                NodeId::from(2u8),
                0,
                11,
                &[NodeId::from(1u8)],
                &[vec![90, 92, 93]],
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
        assert!(graph.locator(90).is_some());
        assert!(graph.locator(91).is_some());
        assert!(graph.locator(92).is_some());
        assert!(!graph.forward.0.has_dirty_regions());
        assert!(!graph.reverse.0.has_dirty_regions());
    }

    #[test]
    fn graph_runtime_tombstones_base_edge_pair_and_removes_sidecar() {
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            99,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            sidecar,
        );

        graph
            .tombstone_base_edge_pair(99, 0, 0, 0, 0)
            .expect("pair tombstone");

        assert!(graph.forward.0.base_entries[0].meta.is_tombstone());
        assert!(graph.reverse.0.base_entries[0].meta.is_tombstone());
        assert_eq!(graph.locator(99), None);
    }

    #[test]
    fn graph_runtime_replaces_base_edge_pair_on_both_surfaces() {
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            77,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            sidecar,
        );

        let (forward_old, reverse_old) = graph
            .replace_base_edge_pair(77, NodeId::from(1u8), 0, 0, NodeId::from(3u8), 0, 0, 8)
            .expect("pair replace");

        assert_eq!(u64::from(forward_old.target), 2);
        assert_eq!(u64::from(reverse_old.target), 1);
        assert_eq!(u64::from(graph.forward.0.base_entries[0].target), 3);
        assert_eq!(graph.forward.0.base_entries[0].meta.label_id(), 8);
        assert_eq!(u64::from(graph.reverse.0.base_entries[0].target), 1);
        assert_eq!(graph.reverse.0.base_entries[0].meta.label_id(), 8);
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

        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            77,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
                vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false))],
                Vec::new(),
            ),
            sidecar,
        );
        let memory = VecMemory::default();

        graph
            .replace_base_edge_pair(77, NodeId::from(1u8), 0, 0, NodeId::from(3u8), 0, 0, 8)
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
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            55,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 0, 0)],
                Vec::new(),
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 0, 0)],
                Vec::new(),
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
            sidecar,
        );

        let (path, (forward_old, reverse_old)) = graph
            .replace_edge_pair(55, NodeId::from(1u8), 0, 0, NodeId::from(3u8), 0, 0, 9)
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
        let mut sidecar = EdgeLocatorSidecar::new();
        sidecar.set(
            55,
            crate::low_level::EdgeLocator::new(SurfaceKind::Forward, NodeId::from(1u8), 0),
        );
        let mut graph = GraphRuntime::new(
            ForwardSurfaceRuntime::new(
                forward_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 0, 0)],
                Vec::new(),
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
            ReverseSurfaceRuntime::new(
                reverse_surface(),
                vec![VertexEntry::new(EdgeIndex::new(0), 0, 0)],
                Vec::new(),
                vec![OverflowEntry::new(
                    55,
                    EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                    LogOffset::EMPTY,
                )],
            ),
            sidecar,
        );

        let path = graph
            .tombstone_edge_pair(55, NodeId::from(1u8), 0, 0, NodeId::from(2u8), 0, 0)
            .expect("tombstone via overflow path");

        assert_eq!(path, GraphMutationPath::Overflow);
        assert!(graph.forward.0.overflow_entries[0]
            .entry
            .meta
            .is_tombstone());
        assert!(graph.reverse.0.overflow_entries[0]
            .entry
            .meta
            .is_tombstone());
        assert_eq!(graph.locator(55), None);
    }
}
