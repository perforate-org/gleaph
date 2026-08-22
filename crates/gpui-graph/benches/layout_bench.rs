//! ForceAtlas2 layout benchmarks.
//!
//! Measures the engine's `step` on graphs that stress the two dominant force
//! phases:
//!
//! - `grid`: uniform grid graph, so repulsion (spatial-grid pair tests)
//!   dominates.
//! - `hub`: star graph whose leaves all sit near each other on a ring, so both
//!   repulsion and attraction stay heavy.
//!
//! Every timed closure rebuilds a fresh engine from a cloned template state, so
//! each sample measures identical work: an unsettled configuration under a
//! fixed iteration budget, rather than progress toward convergence.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use glam::Vec2;
use gpui_graph::graph::{EdgeDirection, Graph, NodeId};
use gpui_graph::layout::{
    ForceAtlas2, LayoutBudget, LayoutEdge, LayoutEngine, LayoutGraph, LayoutIndex, LayoutNode,
    LayoutState,
};

/// A prepared layout problem plus its dense projection.
struct BenchCase {
    graph: LayoutGraph,
    state: LayoutState,
}

impl BenchCase {
    /// Project a logical graph into the dense layout representation and seed
    /// positions from `positions_by_id`.
    fn project(graph: &Graph<(), ()>, positions_by_id: &HashMap<NodeId, Vec2>) -> Self {
        let node_ids: Vec<_> = graph.nodes().map(|(id, _)| id).collect();
        let mut state = LayoutState::new();
        state.resize(node_ids.len());
        for (index, id) in node_ids.iter().enumerate() {
            state.positions[index] = positions_by_id[id];
        }
        let edges = graph
            .edges()
            .map(|(_, e)| {
                let source = node_ids.iter().position(|&x| x == e.source).unwrap() as u32;
                let target = node_ids.iter().position(|&x| x == e.target).unwrap() as u32;
                LayoutEdge {
                    source: LayoutIndex(source),
                    target: LayoutIndex(target),
                    direction: e.direction,
                }
            })
            .collect();
        Self {
            graph: LayoutGraph {
                nodes: vec![LayoutNode {}; node_ids.len()],
                edges,
                node_ids,
                topology_revision: 0,
            },
            state,
        }
    }

    /// A uniform grid graph at fixed spacing.
    fn grid(side: usize) -> Self {
        let mut g = Graph::new();
        let mut positions_by_id = HashMap::new();
        let spacing = 60.0;
        let ids: Vec<_> = (0..side * side)
            .map(|i| {
                let id = g.add_node(());
                positions_by_id.insert(
                    id,
                    Vec2::new((i % side) as f32 * spacing, (i / side) as f32 * spacing),
                );
                id
            })
            .collect();
        let at = |x: usize, y: usize| ids[y * side + x];
        for y in 0..side {
            for x in 0..side {
                if x + 1 < side {
                    g.add_edge(at(x, y), at(x + 1, y), EdgeDirection::Undirected, ());
                }
                if y + 1 < side {
                    g.add_edge(at(x, y), at(x, y + 1), EdgeDirection::Undirected, ());
                }
            }
        }
        Self::project(&g, &positions_by_id)
    }

    /// A star graph: one hub connected to every leaf, leaves arranged on a
    /// ring around the hub so neighbors are close enough to repel.
    fn hub(leaves: usize) -> Self {
        let mut g = Graph::new();
        let mut positions_by_id = HashMap::new();
        let hub = g.add_node(());
        positions_by_id.insert(hub, Vec2::ZERO);
        let radius = 300.0;
        for i in 0..leaves {
            let leaf = g.add_node(());
            let angle = i as f32 / leaves as f32 * std::f32::consts::TAU;
            positions_by_id.insert(leaf, Vec2::new(angle.cos(), angle.sin()) * radius);
            g.add_edge(hub, leaf, EdgeDirection::Undirected, ());
        }
        Self::project(&g, &positions_by_id)
    }
}

fn bench_force_atlas2_step(c: &mut Criterion) {
    let mut group = c.benchmark_group("force_atlas2_step");
    group.sample_size(30);

    for side in [20usize, 40usize] {
        let case = BenchCase::grid(side);
        group.bench_with_input(
            BenchmarkId::new("grid", format!("{}x{}", side, side)),
            &case,
            |b, case| {
                b.iter(|| {
                    let mut engine = ForceAtlas2::default();
                    let mut state = case.state.clone();
                    engine.rebuild(&case.graph, &mut state);
                    std::hint::black_box(engine.step(
                        &case.graph,
                        &mut state,
                        LayoutBudget { max_iterations: 8 },
                    ));
                })
            },
        );
    }

    for leaves in [256usize, 1024usize] {
        let case = BenchCase::hub(leaves);
        group.bench_with_input(
            BenchmarkId::new("hub", leaves.to_string()),
            &case,
            |b, case| {
                b.iter(|| {
                    let mut engine = ForceAtlas2::default();
                    let mut state = case.state.clone();
                    engine.rebuild(&case.graph, &mut state);
                    std::hint::black_box(engine.step(
                        &case.graph,
                        &mut state,
                        LayoutBudget { max_iterations: 8 },
                    ));
                })
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_force_atlas2_step);
criterion_main!(benches);
