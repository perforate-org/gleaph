//! Fixed / manual layout (§15.2).
//!
//! A fixed layout performs no automatic topology-driven repositioning. It is
//! useful for externally positioned graphs, manually arranged graphs, saved
//! layouts, tests, and deterministic demonstrations.

use super::graph::{LayoutGraph, LayoutState};
use super::{LayoutBudget, LayoutEngine, LayoutProgress};

/// A layout that never repositions nodes automatically.
///
/// Positions are owned by the scene; this engine only reports that the layout
/// is settled.
#[derive(Debug, Clone, Default)]
pub struct FixedLayout;

impl LayoutEngine for FixedLayout {
    fn rebuild(&mut self, graph: &LayoutGraph, state: &mut LayoutState) {
        // Fixed layout has no internal algorithm state. Ensure the dense state
        // is sized to the projection so callers can rely on it.
        state.resize(graph.node_count());
    }

    fn step(
        &mut self,
        _graph: &LayoutGraph,
        _state: &mut LayoutState,
        _budget: LayoutBudget,
    ) -> LayoutProgress {
        LayoutProgress::Settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::graph::LayoutIndex;

    #[test]
    fn fixed_layout_never_moves_nodes() {
        let mut layout = FixedLayout;
        let graph = LayoutGraph::new(
            vec![Default::default(), Default::default()],
            vec![],
            vec![],
            0,
        );
        let mut state = LayoutState::new();
        state.positions = vec![glam::Vec2::new(1.0, 2.0), glam::Vec2::new(3.0, 4.0)];
        state.pinned.resize(2, false);

        layout.rebuild(&graph, &mut state);
        let progress = layout.step(&graph, &mut state, LayoutBudget::default());

        assert_eq!(progress, LayoutProgress::Settled);
        assert_eq!(state.position(LayoutIndex(0)), glam::Vec2::new(1.0, 2.0));
        assert_eq!(state.position(LayoutIndex(1)), glam::Vec2::new(3.0, 4.0));
    }
}
