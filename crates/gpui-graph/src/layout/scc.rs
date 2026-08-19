//! Strongly-connected-component circular layout (§15.3).
//!
//! A deterministic layout for directed graphs whose structure is best read
//! through its strongly connected components — the same block shape the
//! Frobenius normal form of a matrix produces, and the condensation DAG of the
//! graph.
//!
//! The engine:
//!
//!   1. computes the strongly connected components (Tarjan),
//!   2. contracts each component to a single node (the condensation DAG) and
//!      assigns each component a layer by its longest-path depth (Mirsky's
//!      theorem), so components at the same depth share a column,
//!   3. places each component's nodes on a circle centered at the component's
//!      position, and arranges the component circles along the layers so the
//!      drawing reads left-to-right block by block with edges flowing from
//!      earlier to later columns.
//!
//! Because the layout is derived entirely from topology, `rebuild` recomputes a
//! full deterministic placement and `step` settles immediately. Pinned nodes
//! keep their position and are not overwritten.

use glam::Vec2;

use super::graph::{LayoutGraph, LayoutIndex, LayoutState};
use super::{LayoutBudget, LayoutEngine, LayoutProgress};
use crate::graph::EdgeDirection;

/// Horizontal spacing between SCC columns (world units).
const COLUMN_SPACING: f32 = 220.0;
/// Vertical spacing between SCC circles within one column (world units).
const CIRCLE_SPACING: f32 = 200.0;
/// Base radius of a single-node SCC circle (world units).
const BASE_RADIUS: f32 = 30.0;
/// Extra radius per additional node in an SCC (world units).
const RADIUS_PER_NODE: f32 = 22.0;

/// Deterministic SCC condensation circular layout engine.
#[derive(Debug, Clone, Default)]
pub struct SccLayoutEngine;

impl LayoutEngine for SccLayoutEngine {
    fn rebuild(&mut self, graph: &LayoutGraph, state: &mut LayoutState) {
        let n = graph.node_count();
        if n == 0 {
            return;
        }
        let components = strongly_connected_components(graph);
        let layers = layer_components(graph, &components);

        for (col, layer) in layers.iter().enumerate() {
            let x = col as f32 * COLUMN_SPACING;
            // Stack the component circles in this column vertically, centered
            // about zero.
            let count = layer.len() as f32;
            for (slot, &component_index) in layer.iter().enumerate() {
                let center_y = (slot as f32 - (count - 1.0) * 0.5) * CIRCLE_SPACING;
                let center = Vec2::new(x, center_y);
                let members = &components[component_index];
                let radius = BASE_RADIUS + (members.len() as f32 - 1.0) * RADIUS_PER_NODE;
                // Record the cluster center and radius so the paint layer can bow
                // edges within this component outward from it.
                for &m in members {
                    state.cluster_centers[m] = Some((center, radius));
                }
                place_on_circle(center, radius, members, state);
            }
        }
    }

    fn step(
        &mut self,
        _graph: &LayoutGraph,
        _state: &mut LayoutState,
        _budget: LayoutBudget,
    ) -> LayoutProgress {
        // A deterministic topology-derived layout has no iterations to run.
        LayoutProgress::Settled
    }
}

/// Place `members` evenly on a circle of `radius` centered at `center`.
///
/// Members are sorted so the placement is reproducible. A single member sits at
/// the circle's top; otherwise members are spaced by equal angles starting at
/// the top and going clockwise. Pinned nodes are not overwritten.
fn place_on_circle(center: Vec2, radius: f32, members: &[usize], state: &mut LayoutState) {
    let mut sorted: Vec<usize> = members.to_vec();
    sorted.sort_unstable();
    let count = sorted.len() as f32;
    for (i, &idx) in sorted.iter().enumerate() {
        if state.is_pinned(LayoutIndex(idx as u32)) {
            continue;
        }
        let angle = if count <= 1.0 {
            -std::f32::consts::FRAC_PI_2
        } else {
            -std::f32::consts::FRAC_PI_2 + (i as f32 / count) * std::f32::consts::TAU
        };
        let pos = center + Vec2::new(angle.cos(), angle.sin()) * radius;
        state.positions[idx] = pos;
    }
}

/// Tarjan's strongly connected components of a directed graph.
///
/// `graph.edges` gives connectivity: a directed edge contributes
/// `source -> target`; an undirected edge contributes both directions. Nodes
/// without incident edges form singleton components.
fn strongly_connected_components(graph: &LayoutGraph) -> Vec<Vec<usize>> {
    let n = graph.node_count();
    if n == 0 {
        return Vec::new();
    }
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];
    for edge in &graph.edges {
        let s = edge.source.0 as usize;
        let t = edge.target.0 as usize;
        adjacency[s].push(t);
        if edge.direction == EdgeDirection::Undirected && t != s {
            adjacency[t].push(s);
        }
    }

    let mut index = vec![0usize; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut components: Vec<Vec<usize>> = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn strong_connect(
        v: usize,
        adjacency: &[Vec<usize>],
        index: &mut [usize],
        lowlink: &mut [usize],
        on_stack: &mut [bool],
        stack: &mut Vec<usize>,
        next_index: &mut usize,
        components: &mut Vec<Vec<usize>>,
    ) {
        index[v] = *next_index;
        lowlink[v] = *next_index;
        *next_index += 1;
        stack.push(v);
        on_stack[v] = true;

        for &w in &adjacency[v] {
            if index[w] == 0 {
                strong_connect(
                    w, adjacency, index, lowlink, on_stack, stack, next_index, components,
                );
                lowlink[v] = lowlink[v].min(lowlink[w]);
            } else if on_stack[w] {
                lowlink[v] = lowlink[v].min(index[w]);
            }
        }

        if lowlink[v] == index[v] {
            let mut component = Vec::new();
            loop {
                let w = stack.pop().expect("stack non-empty");
                on_stack[w] = false;
                component.push(w);
                if w == v {
                    break;
                }
            }
            components.push(component);
        }
    }

    for v in 0..n {
        if index[v] == 0 {
            strong_connect(
                v,
                &adjacency,
                &mut index,
                &mut lowlink,
                &mut on_stack,
                &mut stack,
                &mut next_index,
                &mut components,
            );
        }
    }
    components
}

/// Assign each SCC a layer by its longest-path depth in the condensation DAG.
///
/// Each SCC is contracted to a single node; an edge between two SCCs becomes a
/// condensation edge. A component's layer is the length of the longest path from
/// any source to it (Mirsky's theorem), so components at the same depth share a
/// column and edges always point from an earlier to a later layer. Returns the
/// components grouped by layer, each layer a list of component indices ordered
/// deterministically by each component's smallest member.
fn layer_components(graph: &LayoutGraph, components: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = components.len();
    let mut comp_of = vec![0usize; graph.node_count()];
    for (c, members) in components.iter().enumerate() {
        for &m in members {
            comp_of[m] = c;
        }
    }

    // A component's determinism key: its smallest member index.
    let mut min_of = vec![usize::MAX; n];
    for (c, members) in components.iter().enumerate() {
        min_of[c] = members.iter().copied().min().unwrap_or(c);
    }

    let mut condensation: Vec<std::collections::BTreeSet<usize>> = vec![Default::default(); n];
    let mut indegree = vec![0usize; n];
    for edge in &graph.edges {
        let s = comp_of[edge.source.0 as usize];
        let t = comp_of[edge.target.0 as usize];
        if s != t && condensation[s].insert(t) {
            indegree[t] += 1;
        }
    }

    // Longest-path layering via Kahn's algorithm: a component's layer is one
    // more than the maximum layer of its predecessors. Sources are layer 0.
    let mut layer = vec![0usize; n];
    let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<(usize, usize)>> =
        Default::default();
    for c in 0..n {
        if indegree[c] == 0 {
            ready.push(std::cmp::Reverse((min_of[c], c)));
        }
    }
    let mut processed = 0usize;
    while let Some(std::cmp::Reverse((_, c))) = ready.pop() {
        processed += 1;
        for &t in &condensation[c] {
            layer[t] = layer[t].max(layer[c] + 1);
            indegree[t] -= 1;
            if indegree[t] == 0 {
                ready.push(std::cmp::Reverse((min_of[t], t)));
            }
        }
    }
    debug_assert_eq!(processed, n, "condensation DAG must be acyclic");

    // Group components by layer, ordering each layer by smallest member so the
    // vertical arrangement is reproducible.
    let max_layer = layer.iter().copied().max().unwrap_or(0);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    for c in 0..n {
        layers[layer[c]].push(c);
    }
    for layer_members in &mut layers {
        layer_members.sort_by_key(|&c| min_of[c]);
    }
    layers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeDirection, Graph, NodeId};
    use crate::layout::graph::LayoutNode;

    fn project(edges: &[(usize, usize, EdgeDirection)]) -> (LayoutGraph, LayoutState) {
        let mut g: Graph<(), ()> = Graph::new();
        // Determine the node count from the largest endpoint referenced.
        let mut count = 0;
        for (s, t, _) in edges {
            count = count.max(*s).max(*t);
        }
        count += 1;
        let ids: Vec<NodeId> = (0..count).map(|_| g.add_node(())).collect();
        for &(s, t, d) in edges {
            g.add_edge(ids[s], ids[t], d, ());
        }
        let node_ids: Vec<_> = ids.clone();
        let mut state = LayoutState::new();
        state.resize(count);
        let layout_edges = g
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
            nodes: vec![LayoutNode {}; count],
            edges: layout_edges,
            node_ids,
            topology_revision: 0,
        };
        (lg, state)
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
        let mut layout = SccLayoutEngine;
        layout.rebuild(&lg, &mut state);
        assert_eq!(
            layout.step(&lg, &mut state, LayoutBudget::default()),
            LayoutProgress::Settled
        );
    }

    #[test]
    fn strongly_connected_nodes_form_a_circle() {
        // Nodes 0 and 1 form a 2-cycle (one SCC); node 2 is a sink.
        let (lg, mut state) = project(&[
            (0, 1, EdgeDirection::Directed),
            (1, 0, EdgeDirection::Directed),
            (1, 2, EdgeDirection::Directed),
        ]);
        let mut engine = SccLayoutEngine;
        engine.rebuild(&lg, &mut state);

        // 0 and 1 are in the same SCC, so they sit on a common circle: their
        // midpoint is the circle center and both are the same distance from it.
        let center = (state.positions[0] + state.positions[1]) * 0.5;
        let d0 = (state.positions[0] - center).length();
        let d1 = (state.positions[1] - center).length();
        assert!((d0 - d1).abs() < 1e-3, "same-SCC nodes share a circle");
        // 2 is a downstream SCC, so its circle sits strictly to the right.
        assert!(state.positions[2].x > center.x);
    }

    #[test]
    fn sources_are_placed_left_of_sinks() {
        // Chain 0 -> 1 -> 2 -> 3, all singleton SCCs.
        let (lg, mut state) = project(&[
            (0, 1, EdgeDirection::Directed),
            (1, 2, EdgeDirection::Directed),
            (2, 3, EdgeDirection::Directed),
        ]);
        let mut engine = SccLayoutEngine;
        engine.rebuild(&lg, &mut state);
        assert!(state.positions[0].x < state.positions[1].x);
        assert!(state.positions[1].x < state.positions[2].x);
        assert!(state.positions[2].x < state.positions[3].x);
    }

    #[test]
    fn layout_is_deterministic() {
        let edges = [
            (0, 1, EdgeDirection::Directed),
            (1, 0, EdgeDirection::Directed),
            (1, 2, EdgeDirection::Directed),
            (2, 3, EdgeDirection::Directed),
            (3, 4, EdgeDirection::Directed),
        ];
        let (lg, mut a) = project(&edges);
        let (_, mut b) = project(&edges);
        let mut e1 = SccLayoutEngine;
        let mut e2 = SccLayoutEngine;
        e1.rebuild(&lg, &mut a);
        e2.rebuild(&lg, &mut b);
        assert_eq!(a.positions, b.positions);
    }

    #[test]
    fn pinned_nodes_are_not_moved() {
        let (lg, mut state) = project(&[
            (0, 1, EdgeDirection::Directed),
            (1, 2, EdgeDirection::Directed),
        ]);
        state.positions[1] = Vec2::new(999.0, 999.0);
        state.pinned.set(1, true);
        let mut engine = SccLayoutEngine;
        engine.rebuild(&lg, &mut state);
        assert_eq!(state.positions[1], Vec2::new(999.0, 999.0));
        // Unpinned neighbors are still laid out.
        assert!(state.positions[0].x < state.positions[2].x);
    }

    #[test]
    fn same_depth_components_share_a_column() {
        // Two independent sources 0 and 1 both feed sink 2. The sources are at
        // the same longest-path depth, so they share the leftmost column; the
        // sink is one layer deeper, so it sits strictly to the right.
        let (lg, mut state) = project(&[
            (0, 2, EdgeDirection::Directed),
            (1, 2, EdgeDirection::Directed),
        ]);
        let mut engine = SccLayoutEngine;
        engine.rebuild(&lg, &mut state);
        assert_eq!(state.positions[0].x, state.positions[1].x);
        assert!(state.positions[2].x > state.positions[0].x);
    }
}
