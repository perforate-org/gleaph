//! Micro-benchmarks for the individual stages of the paint pipeline.
//!
//! These isolate the cost of each stage that `build_paint_frame` runs, so a
//! regression in one stage can be attributed to that stage rather than to the
//! whole frame. Each benchmark measures a single function on a representative
//! input, matching the sizes used in `paint_bench`.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use glam::Vec2;
use gpui_graph::graph::{EdgeDirection, Graph, NodeId};
use gpui_graph::paint::{
    DENSITY_RADIUS, DensityGrid, EdgeCurveContext, ObstacleGrid, apply_node_avoidance,
    edge_control_point, signed_densities, signed_densities_for,
};

/// A grid graph with `side * side` nodes and edges to the right and down
/// neighbors, matching `paint_bench::BenchGraph::grid`.
struct GridGraph {
    positions: HashMap<NodeId, Vec2>,
    edges: Vec<(NodeId, NodeId)>,
}

impl GridGraph {
    fn new(side: usize) -> Self {
        let mut graph = Graph::new();
        let mut positions = HashMap::new();
        let spacing = 60.0;
        let mut ids = Vec::new();
        for y in 0..side {
            for x in 0..side {
                let id = graph.add_node(());
                positions.insert(id, Vec2::new(x as f32 * spacing, y as f32 * spacing));
                ids.push(id);
            }
        }
        let at = |x: usize, y: usize| ids[y * side + x];
        let mut edges = Vec::new();
        for y in 0..side {
            for x in 0..side {
                let id = at(x, y);
                if x + 1 < side {
                    graph.add_edge(id, at(x + 1, y), EdgeDirection::Directed, ());
                    edges.push((id, at(x + 1, y)));
                }
                if y + 1 < side {
                    graph.add_edge(id, at(x, y + 1), EdgeDirection::Directed, ());
                    edges.push((id, at(x, y + 1)));
                }
            }
        }
        Self { positions, edges }
    }

    fn midpoints_and_normals(&self) -> (Vec<Vec2>, Vec<Vec2>) {
        let mut midpoints = Vec::new();
        let mut normals = Vec::new();
        for &(s, t) in &self.edges {
            let s = self.positions[&s];
            let t = self.positions[&t];
            midpoints.push((s + t) * 0.5);
            let dir = t - s;
            let len = dir.length();
            normals.push(if len < f32::EPSILON {
                Vec2::new(0.0, -1.0)
            } else {
                Vec2::new(-dir.y, dir.x) / len
            });
        }
        (midpoints, normals)
    }
}

/// A complete graph on `n` nodes, so every edge's density neighborhood is large.
struct CompleteGraph {
    positions: Vec<Vec2>,
    edges: Vec<(usize, usize)>,
}

impl CompleteGraph {
    fn new(n: usize) -> Self {
        let radius = 2000.0;
        let positions: Vec<Vec2> = (0..n)
            .map(|i| {
                let angle = i as f32 / n as f32 * std::f32::consts::TAU;
                Vec2::new(angle.cos(), angle.sin()) * radius
            })
            .collect();
        let mut edges = Vec::new();
        for i in 0..n {
            for j in (i + 1)..n {
                edges.push((i, j));
            }
        }
        Self { positions, edges }
    }

    fn midpoints_and_normals(&self) -> (Vec<Vec2>, Vec<Vec2>) {
        let mut midpoints = Vec::new();
        let mut normals = Vec::new();
        for &(i, j) in &self.edges {
            let s = self.positions[i];
            let t = self.positions[j];
            midpoints.push((s + t) * 0.5);
            let dir = t - s;
            let len = dir.length();
            normals.push(if len < f32::EPSILON {
                Vec2::new(0.0, -1.0)
            } else {
                Vec2::new(-dir.y, dir.x) / len
            });
        }
        (midpoints, normals)
    }
}

fn bench_density(c: &mut Criterion) {
    let mut group = c.benchmark_group("density");

    for side in [20usize, 50usize] {
        let g = GridGraph::new(side);
        let (midpoints, normals) = g.midpoints_and_normals();
        let grid = DensityGrid::new(&midpoints, DENSITY_RADIUS);
        let all: Vec<usize> = (0..midpoints.len()).collect();
        group.bench_with_input(
            BenchmarkId::new("grid_all", format!("{}x{}", side, side)),
            &(&midpoints, &normals, &grid, &all),
            |b, (m, n, grid, all)| b.iter(|| signed_densities_for(grid, m, n, DENSITY_RADIUS, all)),
        );
    }

    for n in [30usize, 60usize] {
        let g = CompleteGraph::new(n);
        let (midpoints, normals) = g.midpoints_and_normals();
        let grid = DensityGrid::new(&midpoints, DENSITY_RADIUS);
        let all: Vec<usize> = (0..midpoints.len()).collect();
        group.bench_with_input(
            BenchmarkId::new("grid_all", n.to_string()),
            &(&midpoints, &normals, &grid, &all),
            |b, (m, n, grid, all)| b.iter(|| signed_densities_for(grid, m, n, DENSITY_RADIUS, all)),
        );
    }

    group.finish();
}

fn bench_control_point(c: &mut Criterion) {
    let mut group = c.benchmark_group("control_point");

    for side in [20usize, 50usize] {
        let g = GridGraph::new(side);
        let (midpoints, normals) = g.midpoints_and_normals();
        let densities = signed_densities(&midpoints, &normals, DENSITY_RADIUS);
        let has_reverse = vec![false; midpoints.len()];
        let parallel = vec![None; midpoints.len()];
        let obstacles: Vec<Vec2> = g.positions.values().copied().collect();
        let obstacle_cell = 6.0 * 2.0 + 30.0;
        let obstacle_grid = ObstacleGrid::new(&obstacles, obstacle_cell);
        let empty_grid = ObstacleGrid::new(&[], obstacle_cell);

        // With obstacles (the overview case: every node is an obstacle).
        let ctx = EdgeCurveContext {
            index: 0,
            signed_density: densities[0],
            has_reverse: &has_reverse,
            parallel: &parallel,
            zoom: 1.0,
            obstacles: &obstacle_grid,
            node_radius: 6.0,
        };
        let (s, t) = g.edges[0];
        let source = g.positions[&s];
        let target = g.positions[&t];
        group.bench_with_input(
            BenchmarkId::new("with_obstacles", format!("{}x{}", side, side)),
            &(source, target, &ctx),
            |b, (s, t, ctx)| b.iter(|| edge_control_point(*s, *t, ctx, None)),
        );

        // Without obstacles (the culling case: empty grid).
        let ctx_empty = EdgeCurveContext {
            index: 0,
            signed_density: densities[0],
            has_reverse: &has_reverse,
            parallel: &parallel,
            zoom: 1.0,
            obstacles: &empty_grid,
            node_radius: 6.0,
        };
        group.bench_with_input(
            BenchmarkId::new("no_obstacles", format!("{}x{}", side, side)),
            &(source, target, &ctx_empty),
            |b, (s, t, ctx)| b.iter(|| edge_control_point(*s, *t, ctx, None)),
        );
    }

    group.finish();
}

fn bench_node_avoidance(c: &mut Criterion) {
    let mut group = c.benchmark_group("node_avoidance");

    for side in [20usize, 50usize] {
        let g = GridGraph::new(side);
        let obstacles: Vec<Vec2> = g.positions.values().copied().collect();
        let obstacle_cell = 6.0 * 2.0 + 30.0;
        let obstacle_grid = ObstacleGrid::new(&obstacles, obstacle_cell);
        let has_reverse = vec![false; g.edges.len()];
        let parallel = vec![None; g.edges.len()];
        let ctx = EdgeCurveContext {
            index: 0,
            signed_density: 0.0,
            has_reverse: &has_reverse,
            parallel: &parallel,
            zoom: 1.0,
            obstacles: &obstacle_grid,
            node_radius: 6.0,
        };
        let (s, t) = g.edges[0];
        let source = g.positions[&s];
        let target = g.positions[&t];
        let midpoint = (source + target) * 0.5;
        let dir = target - source;
        let unit = dir / dir.length();
        let normal = Vec2::new(-unit.y, unit.x);
        group.bench_with_input(
            BenchmarkId::new("grid", format!("{}x{}", side, side)),
            &(source, target, midpoint, unit, normal, &ctx),
            |b, (s, t, m, u, n, ctx)| {
                b.iter(|| {
                    let mut control = *m;
                    apply_node_avoidance(&mut control, *s, *t, *m, *u, *n, ctx);
                    control
                })
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_density,
    bench_control_point,
    bench_node_avoidance
);
criterion_main!(benches);
