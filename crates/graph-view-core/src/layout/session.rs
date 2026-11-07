//! The layout session: the engine, its dense projection, and the one rule that
//! decides whether a completed background step may land.
//!
//! Layout runs in two places. [`GraphScene`](crate::scene::GraphScene) drives it
//! from GPUI frames, and a host that renders elsewhere (a Rune or egui console)
//! drives it from its own loop. Both must agree about *what a stale step is*, so
//! this module owns the state machine and both drivers call into it:
//!
//! ```text
//! LayoutSession::begin(budget)   →  LayoutFlight  (engine + snapshot leave)
//!         executor runs it       →  CompletedLayoutStep
//! LayoutSession::complete(step)  →  Adopted | Discarded
//! ```
//!
//! `Adopted` means the step's positions landed in the caller's [`LayoutState`];
//! `Discarded` means its snapshot went stale (topology changed, or the user
//! wrote a position directly) and the caller must rebuild the engine against the
//! current projection. The engine always comes home; that is why a failed or
//! stale step can still step again.

use super::controller::{LayoutController, LayoutRunState};
use super::{
    CompletedLayoutStep, LayoutBudget, LayoutEngine, LayoutGraph, LayoutProgress, LayoutState,
};

/// Which driver steps the layout (§12, §30).
///
/// The layout algorithm decides how positions are computed; the controller
/// decides when; this decides *where*. `InProcess` steps on the calling
/// thread. `BackgroundThread` runs one budget on a dedicated OS thread and
/// adopts the result on a later call, through the same staleness rule as the
/// GPUI-executor path, so a host without GPUI still keeps layout off its frame
/// thread.
///
/// A build without threads (wasm) has no worker thread and falls back to
/// `InProcess`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LayoutSource {
    /// Step on the calling thread.
    #[default]
    InProcess,
    /// Step on a dedicated OS thread; wait for the result on a later call.
    BackgroundThread,
}

/// One step handed to an executor.
///
/// The engine and the snapshot it will advance move together; [`Self::run`]
/// executes the step on whichever thread the host chooses and returns the
/// [`CompletedLayoutStep`] that [`LayoutSession::complete`] decides on.
pub struct LayoutFlight {
    engine: Box<dyn LayoutEngine>,
    graph: LayoutGraph,
    state: LayoutState,
    budget: LayoutBudget,
    topology_revision: u64,
    write_epoch: u64,
}

impl LayoutFlight {
    /// Run one step on the calling thread. Hosts on an executor call this from
    /// the task; the std-thread driver calls it from its thread.
    pub fn run(mut self) -> CompletedLayoutStep {
        let progress = self.engine.step(&self.graph, &mut self.state, self.budget);
        CompletedLayoutStep {
            engine: self.engine,
            state: self.state,
            progress,
            topology_revision: self.topology_revision,
            write_epoch: self.write_epoch,
        }
    }
}

/// What became of a completed step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LayoutCompletion {
    /// The step's positions landed in the session's [`LayoutState`].
    Adopted(LayoutProgress),
    /// The snapshot went stale; the result was dropped and the engine rebuilt
    /// against the current projection by the caller.
    Discarded,
}

/// The engine, the projection it advances, the write epoch, and the controller.
///
/// A host owns one of these per graph and drives it: `step` for the calling
/// thread, `begin`/`complete` for a background step. The layout projection
/// ([`LayoutGraph`] and [`LayoutState`]) is passed in rather than stored, so the
/// host keeps ownership of the data its paint and hit-test paths read.
pub struct LayoutSession {
    engine: Option<Box<dyn LayoutEngine>>,
    pending_engine: Option<Box<dyn LayoutEngine>>,
    pending_engine_rebuild: bool,
    write_epoch: u64,
    controller: LayoutController,
    source: LayoutSource,
}

impl LayoutSession {
    /// Create a session with `engine` as the active layout engine.
    ///
    /// The controller starts settled, as an empty projection has nothing to do;
    /// a host reheats ([`Self::reheat`] or [`Self::notify_topology_changed`])
    /// once it has a projection worth laying out.
    pub fn new(engine: Box<dyn LayoutEngine>) -> Self {
        Self {
            engine: Some(engine),
            pending_engine: None,
            pending_engine_rebuild: false,
            write_epoch: 0,
            controller: LayoutController::new(),
            source: LayoutSource::default(),
        }
    }

    /// The controller's run state.
    pub fn state(&self) -> LayoutRunState {
        self.controller.state()
    }

    /// Whether the controller wants a step now.
    pub fn should_step(&self) -> bool {
        self.controller.should_step()
    }

    /// Reheat a settled or paused simulation.
    pub fn reheat(&mut self) {
        self.controller.reheat();
    }

    /// Pause the simulation (for example while the user drags a node).
    pub fn pause(&mut self) {
        self.controller.pause();
    }

    /// Resume a paused simulation.
    pub fn resume(&mut self) {
        self.controller.resume();
    }

    /// Tell the controller the topology changed, reheating the simulation.
    pub fn notify_topology_changed(&mut self) {
        self.controller.notify_topology_changed();
    }

    /// The driver that steps the layout.
    pub fn source(&self) -> LayoutSource {
        self.source
    }

    /// Choose the layout driver. Takes effect at the next step; a step already
    /// running finishes and is adopted under the same staleness rule.
    pub fn set_source(&mut self, source: LayoutSource) {
        self.source = source;
    }

    /// The epoch of direct position writes (drags). A completed step whose
    /// snapshot saw a different epoch is stale.
    pub fn write_epoch(&self) -> u64 {
        self.write_epoch
    }

    /// Record a direct position write, invalidating any step in flight.
    pub fn bump_write_epoch(&mut self) {
        self.write_epoch += 1;
    }

    /// Whether an engine is available right now. `false` while a background
    /// step parks it; a host must then either wait for the step or park a
    /// replacement with [`Self::set_engine`].
    pub fn has_engine(&self) -> bool {
        self.engine.is_some()
    }

    /// The active engine, when it is not parked in a background step.
    pub fn engine_mut(&mut self) -> Option<&mut Box<dyn LayoutEngine>> {
        self.engine.as_mut()
    }

    /// Replace the engine, rebuilding its internal state.
    ///
    /// While a background step holds the previous engine the replacement is
    /// parked and swaps in (with a rebuild against the current projection) when
    /// the step completes; the returning engine is dropped.
    pub fn set_engine(
        &mut self,
        engine: Box<dyn LayoutEngine>,
        graph: &LayoutGraph,
        state: &mut LayoutState,
    ) {
        match self.engine.as_mut() {
            Some(slot) => {
                *slot = engine;
                self.rebuild_engine(graph, state);
            }
            None => {
                self.pending_engine = Some(engine);
                // The parked engine's step result will be discarded anyway.
                self.pending_engine_rebuild = true;
            }
        }
        self.controller.reheat();
    }

    /// Rebuild the active engine from the current projection and state.
    ///
    /// Cluster centers are derived output owned by the active engine; they are
    /// cleared before every rebuild so an engine that does not provide clusters
    /// cannot inherit geometry from a previous engine, while canonical node
    /// positions remain untouched. A parked engine defers the rebuild to its
    /// return.
    pub fn rebuild_engine(&mut self, graph: &LayoutGraph, state: &mut LayoutState) {
        let Some(engine) = self.engine.as_mut() else {
            self.pending_engine_rebuild = true;
            return;
        };
        state.cluster_centers.fill(None);
        engine.rebuild(graph, state);
    }

    /// Step on the calling thread. `None` means nothing ran: the controller is
    /// settled or paused, or the engine is parked in a background step.
    pub fn step(
        &mut self,
        graph: &LayoutGraph,
        state: &mut LayoutState,
        budget: LayoutBudget,
    ) -> Option<LayoutProgress> {
        if !self.controller.should_step() {
            return None;
        }
        let engine = self.engine.as_mut()?;
        let progress = engine.step(graph, state, budget);
        if progress == LayoutProgress::Settled {
            self.controller.notify_converged();
        }
        Some(progress)
    }

    /// Take the engine and a snapshot for one background step. `None` when the
    /// controller is settled or the engine is already parked in a step.
    pub fn begin(
        &mut self,
        graph: &LayoutGraph,
        state: &LayoutState,
        budget: LayoutBudget,
    ) -> Option<LayoutFlight> {
        if !self.controller.should_step() {
            return None;
        }
        let engine = self.engine.take()?;
        Some(LayoutFlight {
            engine,
            graph: graph.clone(),
            state: state.clone(),
            budget,
            topology_revision: graph.topology_revision,
            write_epoch: self.write_epoch,
        })
    }

    /// Decide a completed step and bring its engine home.
    ///
    /// Positions are adopted only when neither the topology revision nor the
    /// write epoch changed during the step; otherwise the result is discarded
    /// and the caller must call [`Self::rebuild_engine`] against the current
    /// projection.
    pub fn complete(
        &mut self,
        finished: CompletedLayoutStep,
        live_topology_revision: u64,
        state: &mut LayoutState,
    ) -> LayoutCompletion {
        let CompletedLayoutStep {
            engine,
            state: step_state,
            progress,
            topology_revision,
            write_epoch,
        } = finished;

        // A replacement installed during the step wins over the parked one.
        if let Some(pending) = self.pending_engine.take() {
            drop(engine);
            self.engine = Some(pending);
            self.pending_engine_rebuild = true;
        } else {
            self.engine = Some(engine);
        }

        let fresh = topology_revision == live_topology_revision && write_epoch == self.write_epoch;
        if fresh && !self.pending_engine_rebuild {
            state.positions = step_state.positions;
            state.cluster_centers = step_state.cluster_centers;
            if progress == LayoutProgress::Settled {
                self.controller.notify_converged();
            }
            LayoutCompletion::Adopted(progress)
        } else {
            LayoutCompletion::Discarded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic engine that reports convergence, so the adoption rule
    /// can be tested without threads or a GPUI context: the whole point of
    /// extracting the session.
    struct MarkerEngine;

    impl LayoutEngine for MarkerEngine {
        fn rebuild(&mut self, _graph: &LayoutGraph, _state: &mut LayoutState) {}

        fn step(
            &mut self,
            _graph: &LayoutGraph,
            _state: &mut LayoutState,
            _budget: LayoutBudget,
        ) -> LayoutProgress {
            LayoutProgress::Settled
        }
    }

    fn projection(topology_revision: u64) -> LayoutGraph {
        LayoutGraph::new(Vec::new(), Vec::new(), Vec::new(), topology_revision)
    }

    #[test]
    fn a_fresh_step_is_adopted_and_settles_the_controller() {
        let mut session = LayoutSession::new(Box::new(MarkerEngine));
        session.reheat();
        let graph = projection(3);
        let mut state = LayoutState::new();

        let flight = session
            .begin(&graph, &state, LayoutBudget::default())
            .expect("a running controller hands out a flight");
        assert!(
            !session.has_engine(),
            "a flight parks the engine so steps stay single-flight"
        );

        let finished = flight.run();
        assert_eq!(
            session.complete(finished, graph.topology_revision, &mut state),
            LayoutCompletion::Adopted(LayoutProgress::Settled)
        );
        assert!(session.has_engine(), "the engine comes home");
        assert_eq!(session.state(), LayoutRunState::Settled);
    }

    #[test]
    fn a_step_whose_snapshot_saw_a_position_write_is_discarded() {
        let mut session = LayoutSession::new(Box::new(MarkerEngine));
        session.reheat();
        let graph = projection(3);
        let mut state = LayoutState::new();

        let flight = session
            .begin(&graph, &state, LayoutBudget::default())
            .expect("flight");
        // The user drags a node while the step runs.
        session.bump_write_epoch();

        let finished = flight.run();
        assert_eq!(
            session.complete(finished, graph.topology_revision, &mut state),
            LayoutCompletion::Discarded,
            "a stale step must not overwrite a direct position write"
        );
        assert!(session.has_engine());
        assert_eq!(
            session.state(),
            LayoutRunState::Running,
            "the discarded step does not settle the controller"
        );
    }

    #[test]
    fn a_step_whose_snapshot_saw_a_topology_change_is_discarded() {
        let mut session = LayoutSession::new(Box::new(MarkerEngine));
        session.reheat();
        let graph = projection(3);
        let mut state = LayoutState::new();

        let flight = session
            .begin(&graph, &state, LayoutBudget::default())
            .expect("flight");
        let finished = flight.run();

        // The projection moved on while the step was in flight.
        assert_eq!(
            session.complete(finished, 4, &mut state),
            LayoutCompletion::Discarded
        );
        assert!(session.has_engine());
    }

    #[test]
    fn a_settled_session_hands_out_no_flight() {
        let mut session = LayoutSession::new(Box::new(MarkerEngine));
        session.reheat();
        let graph = projection(0);
        let mut state = LayoutState::new();

        let flight = session
            .begin(&graph, &state, LayoutBudget::default())
            .expect("flight");
        let finished = flight.run();
        session.complete(finished, graph.topology_revision, &mut state);

        assert!(
            session
                .begin(&graph, &state, LayoutBudget::default())
                .is_none(),
            "a settled controller must not start a step"
        );
    }
}
