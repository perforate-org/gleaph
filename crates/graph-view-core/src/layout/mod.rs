//! Layout architecture.
//!
//! Layout is modeled as a long-lived session, not a one-shot function (§11.1).
//! A [`LayoutEngine`] computes positions; a controller decides when it runs
//! (§12). Stable graph identity is separated from dense layout indexing (§11.2).

pub mod controller;
pub mod fixed;
pub mod force_atlas2;
pub mod graph;
pub mod placement;
pub mod scc;

pub use controller::{LayoutController, LayoutRunState};
pub use fixed::FixedLayout;
pub use force_atlas2::ForceAtlas2;
pub use graph::{LayoutEdge, LayoutGraph, LayoutIndex, LayoutNode, LayoutState};
pub use placement::{Placement, Rng};
pub use scc::SccLayoutEngine;

use crate::graph::EdgeDirection;

/// A topology change expressed in dense layout index space.
///
/// Incremental topology updates are optional for an engine (§11.6). The
/// delta carries one mapping table instead of separate added/removed lists:
/// every entry of the new projection points at its previous dense index,
/// or [`None`] when the node is freshly placed. Removals appear as old
/// indices missing from the mapping (dense indices compact on removal, so
/// survivors may shift). Engines permute carried state through this table
/// and reinitialize entries without a predecessor.
#[derive(Debug, Clone, Default)]
pub struct LayoutDelta {
    /// `remap[new_index] == Some(old_index)` for carried-over nodes,
    /// `None` for fresh ones.
    pub remap: Vec<Option<u32>>,
}

/// How an engine handled an incremental topology update (§11.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutSync {
    /// The delta was applied incrementally.
    Applied,
    /// The engine requires a full rebuild.
    RebuildRequired,
}

/// The result of a layout step (§11.7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LayoutProgress {
    /// The simulation is still running.
    Running {
        /// A general stability measure in `[0, 1]`, if the engine provides one.
        stability: Option<f32>,
    },
    /// The simulation has converged.
    Settled,
}

/// A budget controlling a layout step (§11.8).
///
/// Iteration-oriented budgeting is preferred over making physical elapsed time
/// part of layout semantics. An optional wall-clock cap supports real-time
/// drivers: engines check it between iterations and stop early, always having
/// completed at least one iteration. The scheduling layer may derive such a
/// cap from its frame budget.
#[derive(Debug, Clone, Copy)]
pub struct LayoutBudget {
    /// Maximum number of iterations to run in this step.
    pub max_iterations: u32,
    /// Optional wall-clock cap on the step. `None` keeps stepping purely
    /// iteration-oriented and deterministic (tests, benches); a set cap
    /// trades trajectory reproducibility for bounded frame work.
    pub max_duration: Option<core::time::Duration>,
}

impl Default for LayoutBudget {
    fn default() -> Self {
        Self {
            max_iterations: 1,
            max_duration: None,
        }
    }
}

/// A unified stateful layout interface (§11.5).
///
/// All layouts use this single interface rather than separate static and
/// dynamic traits. Exact method signatures may evolve; the architectural
/// contract is that layout is a long-lived session operating over dense data.
///
/// The API itself is synchronous and threading-free (§30); the [`Send`] bound
/// exists so the scheduling layer *may* move an engine onto GPUI's background
/// executor between calls (§30 native strategy). Every engine stays fully
/// usable from the UI thread.
pub trait LayoutEngine: Send + Sync {
    /// Rebuild the engine's internal algorithm state for the given graph.
    ///
    /// A rebuild must not imply discarding existing positions: old nodes keep
    /// their positions, new nodes receive initial positions (§11.6).
    fn rebuild(&mut self, graph: &LayoutGraph, state: &mut LayoutState);

    /// Apply an incremental topology change.
    ///
    /// `graph` and `state` describe the *new* projection; positions for
    /// carried-over nodes are preserved by the scene, fresh nodes carry
    /// their initial placement. An engine that returns
    /// [`LayoutSync::Applied`] takes ownership of keeping every piece of its
    /// derived state (masses, adaptive factors, spatial acceleration
    /// structures) consistent with the new projection. Returning
    /// [`LayoutSync::RebuildRequired`] delegates to [`LayoutEngine::rebuild`]
    /// and is a valid default (§11.6).
    fn apply_delta(
        &mut self,
        graph: &LayoutGraph,
        delta: &LayoutDelta,
        state: &mut LayoutState,
    ) -> LayoutSync {
        let _ = (graph, delta, state);
        LayoutSync::RebuildRequired
    }

    /// Advance the simulation by up to `budget.max_iterations` iterations.
    fn step(
        &mut self,
        graph: &LayoutGraph,
        state: &mut LayoutState,
        budget: LayoutBudget,
    ) -> LayoutProgress;
}

/// A directed edge in dense layout space, used by engines that care about
/// direction.
#[derive(Debug, Clone, Copy)]
pub struct DirectedLayoutEdge {
    /// Dense index of the source node.
    pub source: LayoutIndex,
    /// Dense index of the target node.
    pub target: LayoutIndex,
    /// Directionality of the edge.
    pub direction: EdgeDirection,
}
