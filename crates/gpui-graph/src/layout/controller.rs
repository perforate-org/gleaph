//! Layout controller (§12).
//!
//! The layout algorithm decides how to compute positions; it does not decide
//! when it should run. That responsibility belongs to a [`LayoutController`],
//! which is driven by graph changes, GPUI frames, and user interaction.

/// The run state of a layout session (§12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutRunState {
    /// The simulation is active and should be stepped.
    Running,
    /// The simulation has converged.
    Settled,
    /// The simulation is paused by user interaction.
    Paused,
}

/// Decides when a [`LayoutEngine`](super::LayoutEngine) should run.
#[derive(Debug, Clone, Copy)]
pub struct LayoutController {
    state: LayoutRunState,
}

impl Default for LayoutController {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutController {
    /// Create a controller in the settled state.
    pub fn new() -> Self {
        Self {
            state: LayoutRunState::Settled,
        }
    }

    /// The current run state.
    pub fn state(&self) -> LayoutRunState {
        self.state
    }

    /// Whether the controller wants the layout to step this frame.
    pub fn should_step(&self) -> bool {
        self.state == LayoutRunState::Running
    }

    /// Notify that the graph topology changed, reheating the simulation.
    pub fn notify_topology_changed(&mut self) {
        self.state = LayoutRunState::Running;
    }

    /// Notify that the simulation converged.
    pub fn notify_converged(&mut self) {
        self.state = LayoutRunState::Settled;
    }

    /// Pause the simulation (e.g. while the user drags a node).
    pub fn pause(&mut self) {
        self.state = LayoutRunState::Paused;
    }

    /// Resume a paused simulation.
    pub fn resume(&mut self) {
        self.state = LayoutRunState::Running;
    }

    /// Reheat a settled simulation.
    pub fn reheat(&mut self) {
        self.state = LayoutRunState::Running;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_transitions() {
        let mut c = LayoutController::new();
        assert_eq!(c.state(), LayoutRunState::Settled);
        assert!(!c.should_step());

        c.notify_topology_changed();
        assert_eq!(c.state(), LayoutRunState::Running);
        assert!(c.should_step());

        c.notify_converged();
        assert_eq!(c.state(), LayoutRunState::Settled);

        c.pause();
        assert_eq!(c.state(), LayoutRunState::Paused);
        assert!(!c.should_step());

        c.resume();
        assert_eq!(c.state(), LayoutRunState::Running);

        c.notify_converged();
        c.reheat();
        assert_eq!(c.state(), LayoutRunState::Running);
    }
}
