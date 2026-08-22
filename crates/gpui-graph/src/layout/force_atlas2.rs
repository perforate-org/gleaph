//! ForceAtlas2 layout (§15.1).
//!
//! The primary dynamic graph layout. The public API wraps the implementation
//! behind [`ForceAtlas2`] rather than exposing raw settings, keeping the layout
//! implementation replaceable.
//!
//! This is a self-contained implementation of the ForceAtlas2 force model
//! (repulsion, attraction, gravity) with adaptive speed and a convergence
//! threshold. Barnes-Hut acceleration is deferred (§37).
//!
//! Numerical note: the force model computes through [`f32::algebraic_*`]
//! arithmetic (see the component-wise helpers below), which permits
//! reassociation, FMA contraction, and reciprocal-multiply. Results are not
//! bit-reproducible across runs, compiler versions, or platforms; that is
//! acceptable here because this is an animated simulation whose tests assert
//! convergence, never exact coordinates. The algebraic helpers must stay out
//! of hit-testing, paint geometry, and viewport math, where epsilon
//! comparisons rely on stable operation-by-operation precision.

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

/// Component-wise [`f32::algebraic_add`].
#[inline]
fn algebraic_add(a: Vec2, b: Vec2) -> Vec2 {
    Vec2::new(a.x.algebraic_add(b.x), a.y.algebraic_add(b.y))
}

/// Component-wise [`f32::algebraic_sub`].
#[inline]
fn algebraic_sub(a: Vec2, b: Vec2) -> Vec2 {
    Vec2::new(a.x.algebraic_sub(b.x), a.y.algebraic_sub(b.y))
}

/// Scalar [`f32::algebraic_mul`] applied to both components.
#[inline]
fn algebraic_mul_scalar(v: Vec2, s: f32) -> Vec2 {
    Vec2::new(v.x.algebraic_mul(s), v.y.algebraic_mul(s))
}

/// Packs a cell coordinate into one sortable key. Negative components wrap to
/// a fixed order via `as u32`; only injectivity matters, not numeric order.
fn cell_key(cx: i32, cy: i32) -> u64 {
    ((cx as u32 as u64) << 32) | cy as u32 as u64
}

/// Neighbor offsets that visit each adjacent cell pair exactly once: together
/// with their negations they cover all eight Moore directions.
const FORWARD_STENCIL: [(i32, i32); 4] = [(1, 0), (1, 1), (0, 1), (1, -1)];

/// A uniform spatial hash over node positions, rebuilt every iteration into
/// reused flat buffers. Nodes are grouped by grid cell into one contiguous
/// `entries` array (`offsets` is its per-cell index), so pair enumeration
/// walks memory sequentially instead of chasing per-cell heap vectors and
/// hashing every candidate lookup.
///
/// With `cell_size == repulsion_radius`, only cells that are adjacent or equal
/// can hold a pair within the radius. Each such pair is visited exactly once:
/// intra-cell pairs directly, cross-cell pairs through [`FORWARD_STENCIL`].
#[derive(Debug, Clone, Default)]
struct RepulsionGrid {
    /// Grid pitch; equals the repulsion radius.
    cell_size: f32,
    /// Unique packed cell coordinates, sorted ascending.
    cells: Vec<u64>,
    /// Per-cell start offset into `entries`, plus one end sentinel.
    offsets: Vec<u32>,
    /// Node indices grouped by cell.
    entries: Vec<u32>,
    /// Scratch reused across rebuilds; empty between calls.
    scratch: Vec<(u64, u32)>,
}

impl RepulsionGrid {
    fn new(cell_size: f32) -> Self {
        Self {
            cell_size,
            cells: Vec::new(),
            offsets: Vec::new(),
            entries: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Regroup `positions` by grid cell, reusing all buffers. The pitch tracks
    /// `radius` so the neighborhood scan stays 3×3 regardless of settings.
    fn rebuild(&mut self, positions: &[Vec2], radius: f32) {
        self.cell_size = radius;
        let cs = self.cell_size;
        self.scratch.clear();
        self.cells.clear();
        self.offsets.clear();
        self.entries.clear();
        for (i, p) in positions.iter().enumerate() {
            let key = cell_key((p.x / cs).floor() as i32, (p.y / cs).floor() as i32);
            self.scratch.push((key, i as u32));
        }
        self.scratch.sort_unstable();
        for si in 0..self.scratch.len() {
            let (key, i) = self.scratch[si];
            if self.cells.last() != Some(&key) {
                self.cells.push(key);
                self.offsets.push(self.entries.len() as u32);
            }
            self.entries.push(i);
        }
        self.offsets.push(self.entries.len() as u32);
    }

    /// Entry range of the cell at `(cx, cy)`; empty if unoccupied.
    fn cell_entries(&self, cx: i32, cy: i32) -> core::ops::Range<usize> {
        let key = cell_key(cx, cy);
        let lo = self.cells.partition_point(|&k| k < key);
        let hi = self.cells.partition_point(|&k| k <= key);
        self.offsets[lo] as usize..self.offsets[hi] as usize
    }

    /// Visit every unordered pair of nodes whose cells are adjacent or equal.
    fn for_each_pair(&self, visit: &mut impl FnMut(usize, usize)) {
        for ci in 0..self.cells.len() {
            let cx = (self.cells[ci] >> 32) as u32 as i32;
            let cy = self.cells[ci] as u32 as i32;
            let range = self.offsets[ci] as usize..self.offsets[ci + 1] as usize;

            for ai in range.clone() {
                for bi in ai + 1..range.end {
                    visit(self.entries[ai] as usize, self.entries[bi] as usize);
                }
            }

            for (dx, dy) in FORWARD_STENCIL {
                let neighbor = self.cell_entries(cx + dx, cy + dy);
                for ai in range.clone() {
                    for bi in neighbor.clone() {
                        visit(self.entries[ai] as usize, self.entries[bi] as usize);
                    }
                }
            }
        }
    }
}

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
    /// Distance beyond which two nodes no longer repel each other. Repulsion
    /// falls off as `1/dist^2`, so beyond this radius it is negligible; the
    /// spatial grid only tests nodes within this radius, making repulsion
    /// O(N·k) instead of O(N²).
    repulsion_radius: f32,
    /// Total displacement below which the layout is considered settled.
    settled_threshold: f32,
    /// Per-node velocity, algorithm-specific state (§11.4).
    velocity: Vec<Vec2>,
    /// Reused per-iteration force buffer, so `iterate` does not allocate.
    forces: Vec<Vec2>,
    /// Spatial hash for the repulsion pass, rebuilt each iteration while
    /// reusing its buffers across iterations (§37 Barnes-Hut remains open).
    grid: RepulsionGrid,
}

impl Default for ForceAtlas2 {
    fn default() -> Self {
        Self {
            scaling: 1.0,
            gravity: 0.1,
            lin_log: false,
            speed: 0.1,
            repulsion_radius: 100.0,
            settled_threshold: 0.001,
            velocity: Vec::new(),
            forces: Vec::new(),
            grid: RepulsionGrid::new(100.0),
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

    /// Set the repulsion radius: the distance beyond which two nodes no longer
    /// repel each other. A smaller radius makes repulsion cheaper (fewer nearby
    /// nodes per grid cell) at the cost of allowing nodes to pack more tightly.
    pub fn with_repulsion_radius(mut self, radius: f32) -> Self {
        self.repulsion_radius = radius;
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
        let forces = &mut self.forces;
        forces.fill(Vec2::ZERO);

        // Repulsion: nodes repel each other within `repulsion_radius`. The
        // grid restricts the pair test to nearby nodes, so repulsion is
        // O(N·k) with k the number of nodes within the radius, instead of
        // O(N²); the forward stencil visits each unordered pair once.
        let mut grid = std::mem::take(&mut self.grid);
        grid.rebuild(&state.positions, self.repulsion_radius);
        let positions = &state.positions;
        let scaling = self.scaling;
        let radius = self.repulsion_radius;
        grid.for_each_pair(&mut |i, j| {
            let delta = algebraic_sub(positions[i], positions[j]);
            let dist = delta.length().max(0.01);
            if dist > radius {
                return;
            }
            // `dir * force == delta * (scaling / dist³)`: one division per
            // pair instead of one in `force` plus two in `delta / dist`.
            let strength = scaling.algebraic_div(dist.algebraic_mul(dist).algebraic_mul(dist));
            let impulse = algebraic_mul_scalar(delta, strength);
            forces[i] = algebraic_add(forces[i], impulse);
            forces[j] = algebraic_sub(forces[j], impulse);
        });
        self.grid = grid;

        // Attraction: every edge pulls its endpoints together.
        for edge in &graph.edges {
            let s = edge.source.0 as usize;
            let t = edge.target.0 as usize;
            let delta = algebraic_sub(state.positions[t], state.positions[s]);
            let dist = delta.length().max(0.01);
            let fa = if self.lin_log {
                (1.0f32.algebraic_add(dist)).ln()
            } else {
                dist
            };
            // `dir * fa == delta * (fa / dist)`: one division instead of two.
            let pull = algebraic_mul_scalar(delta, fa.algebraic_div(dist));
            forces[s] = algebraic_add(forces[s], pull);
            forces[t] = algebraic_sub(forces[t], pull);
        }

        // Gravity: pull every node toward the origin.
        for (force, pos) in forces.iter_mut().zip(&state.positions) {
            *force = algebraic_sub(*force, algebraic_mul_scalar(*pos, self.gravity));
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
            *v = algebraic_add(*v, algebraic_mul_scalar(*force, self.speed));
            // Damping by 1/2 is exactly representable, so plain multiplication
            // is exact and needs no algebraic relaxation.
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
            // `v / len * step == v * (step / len)`: one division.
            let scaled_step = step.algebraic_div(len);
            state.positions[i] =
                algebraic_add(state.positions[i], algebraic_mul_scalar(*v, scaled_step));
            total = total.algebraic_add(step);
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
    fn repulsion_grid_pairs_match_brute_force() {
        // Scattered points across many cells, including coincident positions.
        let mut pts: Vec<Vec2> = (0..48)
            .map(|i| {
                let t = i as f32;
                Vec2::new(t * 61.7 % 800.0 - 400.0, t * 113.3 % 600.0 - 300.0)
            })
            .collect();
        pts.push(pts[3]);
        pts.push(pts[17]);
        pts.push(Vec2::ZERO);

        let cs = 100.0;
        let cell = |p: Vec2| ((p.x / cs).floor() as i32, (p.y / cs).floor() as i32);
        let mut expected = std::collections::BTreeSet::new();
        for i in 0..pts.len() {
            for j in i + 1..pts.len() {
                let (a, b) = (cell(pts[i]), cell(pts[j]));
                if (a.0 - b.0).abs() <= 1 && (a.1 - b.1).abs() <= 1 {
                    expected.insert((i, j));
                }
            }
        }

        let mut grid = RepulsionGrid::new(cs);
        grid.rebuild(&pts, cs);
        let mut got = std::collections::BTreeSet::new();
        grid.for_each_pair(&mut |i, j| {
            assert!(i != j);
            got.insert((i.min(j), i.max(j)));
        });

        assert_eq!(
            got, expected,
            "grid must visit exactly the adjacent-cell pairs"
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
