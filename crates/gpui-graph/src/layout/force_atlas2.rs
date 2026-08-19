//! ForceAtlas2 layout (§15.1).
//!
//! The primary dynamic graph layout. The public API wraps the implementation
//! behind [`ForceAtlas2`] rather than exposing raw settings, keeping the layout
//! implementation replaceable.
//!
//! This is a self-contained implementation of the ForceAtlas2 force model
//! (repulsion, attraction, gravity) with adaptive speed and a convergence
//! threshold. Barnes-Hut acceleration is deferred (§37).

use glam::Vec2;

use super::graph::{LayoutGraph, LayoutIndex, LayoutState};
use super::{LayoutBudget, LayoutEngine, LayoutProgress};

/// Velocity damping per iteration. Values below 1 dissipate energy so the
/// layout converges instead of oscillating forever around an equilibrium.
const DAMPING: f32 = 0.5;
/// Maximum per-iteration step (world units) a node may move, so a single
/// strong force cannot fling a node across the graph in one frame.
const MAX_STEP: f32 = 0.1;
/// Velocity magnitude below which a node's velocity is zeroed. Without this
/// dead-band, the force model keeps re-injecting a tiny velocity at the
/// equilibrium and the node jitters forever instead of settling.
const VELOCITY_EPSILON: f32 = 0.01;

/// ForceAtlas2 layout engine.
#[derive(Debug, Clone)]
pub struct ForceAtlas2 {
    /// Repulsion scaling factor.
    scaling: f32,
    /// Gravity strength pulling nodes toward the origin.
    gravity: f32,
    /// Use `log(1 + dist)` attraction instead of linear attraction.
    lin_log: bool,
    /// Simulation speed: how much of the accumulated force is added to velocity
    /// each iteration.
    speed: f32,
    /// Total displacement below which the layout is considered settled.
    settled_threshold: f32,
    /// Per-node velocity, algorithm-specific state (§11.4).
    velocity: Vec<Vec2>,
    /// Reused per-iteration force buffer, so `iterate` does not allocate.
    forces: Vec<Vec2>,
}

impl Default for ForceAtlas2 {
    fn default() -> Self {
        Self {
            scaling: 1.0,
            gravity: 0.1,
            lin_log: false,
            speed: 0.1,
            settled_threshold: 0.001,
            velocity: Vec::new(),
            forces: Vec::new(),
        }
    }
}

impl ForceAtlas2 {
    /// Set the repulsion scaling factor.
    pub fn with_scaling(mut self, scaling: f32) -> Self {
        self.scaling = scaling;
        self
    }

    /// Set the gravity strength.
    pub fn with_gravity(mut self, gravity: f32) -> Self {
        self.gravity = gravity;
        self
    }

    /// Enable `log(1 + dist)` attraction.
    pub fn with_lin_log(mut self, lin_log: bool) -> Self {
        self.lin_log = lin_log;
        self
    }

    /// Set the simulation speed: how much of the accumulated force is added to
    /// velocity each iteration.
    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = speed;
        self
    }

    /// Set the total-displacement threshold below which the layout settles.
    pub fn with_settled_threshold(mut self, threshold: f32) -> Self {
        self.settled_threshold = threshold;
        self
    }
}

impl LayoutEngine for ForceAtlas2 {
    fn rebuild(&mut self, graph: &LayoutGraph, state: &mut LayoutState) {
        // Rebuild algorithm-specific state. Positions are owned by the scene
        // and preserved across rebuilds (§11.6). Tunable parameters (scaling,
        // gravity, speed, ...) are preserved across rebuilds.
        self.velocity.resize(graph.node_count(), Vec2::ZERO);
        self.forces.resize(graph.node_count(), Vec2::ZERO);
        let _ = state;
    }

    fn step(
        &mut self,
        graph: &LayoutGraph,
        state: &mut LayoutState,
        budget: LayoutBudget,
    ) -> LayoutProgress {
        let n = graph.node_count();
        if n == 0 {
            return LayoutProgress::Settled;
        }
        if self.velocity.len() != n {
            self.velocity.resize(n, Vec2::ZERO);
            self.forces.resize(n, Vec2::ZERO);
        }

        let mut last_displacement = 0.0f32;
        for _ in 0..budget.max_iterations {
            last_displacement = self.iterate(graph, state);
        }

        // `iterate` returns the total displacement across all nodes; compare the
        // average per-node displacement so the threshold is independent of graph
        // size. A small graph with a few jittering nodes must still settle.
        let avg_displacement = last_displacement / n as f32;
        if avg_displacement < self.settled_threshold {
            LayoutProgress::Settled
        } else {
            let stability = (1.0 - (avg_displacement / 10.0).min(1.0)).max(0.0);
            LayoutProgress::Running {
                stability: Some(stability),
            }
        }
    }
}

impl ForceAtlas2 {
    /// Run a single force-model iteration, returning total displacement.
    fn iterate(&mut self, graph: &LayoutGraph, state: &mut LayoutState) -> f32 {
        let n = graph.node_count();
        let forces = &mut self.forces;
        forces.fill(Vec2::ZERO);

        // Repulsion: every pair of nodes repels each other.
        for i in 0..n {
            for j in (i + 1)..n {
                let delta = state.positions[i] - state.positions[j];
                let dist = delta.length().max(0.01);
                let force = self.scaling / (dist * dist);
                let dir = delta / dist;
                forces[i] += dir * force;
                forces[j] -= dir * force;
            }
        }

        // Attraction: every edge pulls its endpoints together.
        for edge in &graph.edges {
            let s = edge.source.0 as usize;
            let t = edge.target.0 as usize;
            let delta = state.positions[t] - state.positions[s];
            let dist = delta.length().max(0.01);
            let fa = if self.lin_log {
                (1.0 + dist).ln()
            } else {
                dist
            };
            let dir = delta / dist;
            forces[s] += dir * fa;
            forces[t] -= dir * fa;
        }

        // Gravity: pull every node toward the origin.
        for (force, pos) in forces.iter_mut().zip(&state.positions) {
            *force -= *pos * self.gravity;
        }

        // Integrate velocity and apply it, respecting pins. Velocity accumulates
        // force and is damped each iteration, so the system loses energy and
        // converges to an equilibrium instead of oscillating forever around it.
        let mut total = 0.0f32;
        for (i, force) in forces.iter().enumerate() {
            if state.is_pinned(LayoutIndex(i as u32)) {
                continue;
            }
            let v = &mut self.velocity[i];
            *v += *force * self.speed;
            *v *= DAMPING;
            let len = v.length();
            if len < VELOCITY_EPSILON {
                // Dead-band: below the epsilon the node is effectively at rest.
                // Zero it so the layout can report settled instead of jittering
                // forever around the equilibrium.
                *v = Vec2::ZERO;
                continue;
            }
            // Cap the per-iteration step so a single strong force cannot
            // fling a node across the graph in one frame.
            let step = len.min(MAX_STEP);
            let move_vec = *v / len * step;
            state.positions[i] += move_vec;
            total += step;
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeDirection, Graph};
    use crate::layout::graph::LayoutNode;

    fn project(graph: &Graph<(), ()>) -> (LayoutGraph, LayoutState) {
        let node_ids: Vec<_> = graph.nodes().map(|(id, _)| id).collect();
        let mut state = LayoutState::new();
        state.resize(node_ids.len());
        let edges = graph
            .edges()
            .map(|(_, e)| {
                let source = node_ids.iter().position(|&x| x == e.source).unwrap() as u32;
                let target = node_ids.iter().position(|&x| x == e.target).unwrap() as u32;
                crate::layout::graph::LayoutEdge {
                    source: LayoutIndex(source),
                    target: LayoutIndex(target),
                    direction: e.direction,
                }
            })
            .collect();
        let lg = LayoutGraph {
            nodes: vec![LayoutNode {}; node_ids.len()],
            edges,
            node_ids,
            topology_revision: 0,
        };
        (lg, state)
    }

    #[test]
    fn force_atlas2_moves_nodes_toward_edges() {
        let mut g = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Undirected, ());

        let (lg, mut state) = project(&g);
        // Start far apart.
        state.positions[0] = Vec2::new(-100.0, 0.0);
        state.positions[1] = Vec2::new(100.0, 0.0);

        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);
        let mut progress = LayoutProgress::Running { stability: None };
        for _ in 0..200 {
            progress = fa.step(&lg, &mut state, LayoutBudget { max_iterations: 1 });
        }

        // Nodes should have moved closer together.
        let dist = (state.positions[0] - state.positions[1]).length();
        assert!(dist < 200.0, "distance should shrink, got {dist}");
        assert!(matches!(progress, LayoutProgress::Running { .. }));
    }

    #[test]
    fn pinned_nodes_do_not_move() {
        let mut g = Graph::new();
        let a = g.add_node(());
        let b = g.add_node(());
        g.add_edge(a, b, EdgeDirection::Undirected, ());

        let (lg, mut state) = project(&g);
        state.positions[0] = Vec2::new(-100.0, 0.0);
        state.positions[1] = Vec2::new(100.0, 0.0);
        state.pinned.set(0, true);

        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);
        for _ in 0..50 {
            fa.step(&lg, &mut state, LayoutBudget { max_iterations: 1 });
        }

        assert_eq!(state.positions[0], Vec2::new(-100.0, 0.0));
    }

    #[test]
    fn empty_graph_settles() {
        let lg = LayoutGraph {
            nodes: vec![],
            edges: vec![],
            node_ids: vec![],
            topology_revision: 0,
        };
        let mut state = LayoutState::new();
        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);
        assert_eq!(
            fa.step(&lg, &mut state, LayoutBudget::default()),
            LayoutProgress::Settled
        );
    }

    #[test]
    fn layout_converges_instead_of_oscillating() {
        // A small graph with a hub and several neighbors. With velocity damping,
        // the layout must eventually settle rather than oscillate forever around
        // an equilibrium (which would leave the nodes jittering).
        let mut g = Graph::new();
        let ids: Vec<_> = (0..6).map(|_| g.add_node(())).collect();
        for i in 1..ids.len() {
            g.add_edge(ids[0], ids[i], EdgeDirection::Undirected, ());
        }
        let (lg, mut state) = project(&g);
        // Start nodes spread out so the forces are strong.
        for (i, pos) in state.positions.iter_mut().enumerate() {
            *pos = Vec2::new((i as f32 - 2.5) * 40.0, 0.0);
        }

        let mut fa = ForceAtlas2::default();
        fa.rebuild(&lg, &mut state);
        let mut progress = LayoutProgress::Running { stability: None };
        for _ in 0..2000 {
            progress = fa.step(&lg, &mut state, LayoutBudget { max_iterations: 1 });
            if progress == LayoutProgress::Settled {
                break;
            }
        }
        assert_eq!(
            progress,
            LayoutProgress::Settled,
            "layout should converge with velocity damping"
        );
    }
}
