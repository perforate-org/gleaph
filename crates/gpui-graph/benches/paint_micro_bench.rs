//! Micro-benchmarks for the individual stages of the paint pipeline.
//!
//! These isolate the cost of each stage that `build_paint_frame` runs, so a
//! regression in one stage can be attributed to that stage rather than to the
//! whole frame. Each benchmark measures a single function on a representative
//! input, matching the sizes used in `paint_bench`.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use glam::Vec2;
use gpui::{PathBuilder, point, px};
use gpui_graph::graph::{EdgeDirection, Graph, NodeId};
use gpui_graph::paint::{
    DENSITY_RADIUS, EdgeCurveContext, ObstacleField, apply_node_avoidance, edge_control_point,
    edge_path, signed_densities, signed_densities_for, trim_curve_to_node_boundary,
};
use gpui_graph::runtime::DensityGrid;
use gpui_graph::style::GraphStyle;
use gpui_graph::viewport::Viewport;

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
            normals.push(if len.is_finite() && len > 0.0 {
                Vec2::new(-dir.y, dir.x) / len
            } else {
                Vec2::new(0.0, -1.0)
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
            normals.push(if len.is_finite() && len > 0.0 {
                Vec2::new(-dir.y, dir.x) / len
            } else {
                Vec2::new(0.0, -1.0)
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
        let obstacle_radius = 6.0 * 2.0 + 30.0;
        let obstacle_grid = ObstacleField::new(&obstacles, obstacle_radius);
        let empty_grid = ObstacleField::new(&[], obstacle_radius);

        // With obstacles (the overview case: every node is an obstacle).
        let ctx = EdgeCurveContext {
            index: 0,
            signed_density: densities[0],
            has_reverse: &has_reverse,
            parallel: &parallel,
            obstacles: &obstacle_grid,
            obstacle_radius: 6.0 * 2.0 + 30.0,
            // The bench field spans every grid position, including this
            // edge's own endpoints.
            endpoints_in_field: (true, true),
            self_loop_has_node_label: false,
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
            obstacles: &empty_grid,
            obstacle_radius: 0.0,
            endpoints_in_field: (false, false),
            self_loop_has_node_label: false,
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
        let obstacle_radius = 6.0 * 2.0 + 30.0;
        let obstacle_grid = ObstacleField::new(&obstacles, obstacle_radius);
        let has_reverse = vec![false; g.edges.len()];
        let parallel = vec![None; g.edges.len()];
        let ctx = EdgeCurveContext {
            index: 0,
            signed_density: 0.0,
            has_reverse: &has_reverse,
            parallel: &parallel,
            obstacles: &obstacle_grid,
            obstacle_radius: 6.0 * 2.0 + 30.0,
            // The bench field spans every grid position, including this
            // edge's own endpoints.
            endpoints_in_field: (true, true),
            self_loop_has_node_label: false,
        };
        let (s, t) = g.edges[0];
        let source = g.positions[&s];
        let target = g.positions[&t];
        group.bench_with_input(
            BenchmarkId::new("grid", format!("{}x{}", side, side)),
            &(source, target, &ctx),
            |b, (s, t, ctx)| {
                b.iter(|| {
                    // Start from the chord midpoint, as the paint path does.
                    let mut control = (*s + *t) * 0.5;
                    apply_node_avoidance(&mut control, *s, *t, ctx);
                    control
                })
            },
        );
    }

    group.finish();
}

fn bench_edge_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("edge_path");

    for side in [20usize, 50usize] {
        let g = GridGraph::new(side);
        let (midpoints, normals) = g.midpoints_and_normals();
        let densities = signed_densities(&midpoints, &normals, DENSITY_RADIUS);
        let has_reverse = vec![false; midpoints.len()];
        let parallel = vec![None; midpoints.len()];
        let obstacles: Vec<Vec2> = g.positions.values().copied().collect();
        let obstacle_radius = 6.0 * 2.0 + 30.0;
        let obstacle_grid = ObstacleField::new(&obstacles, obstacle_radius);
        let style = GraphStyle::default();
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(1600.0, 1000.0));
        vp.zoom_at(Vec2::new(800.0, 500.0), 1.0);

        // Rebuild a graph with edges so `edge_path` can look up the edge.
        let mut graph = Graph::new();
        let mut ids = Vec::new();
        for _ in 0..side * side {
            ids.push(graph.add_node(()));
        }
        let at = |x: usize, y: usize| ids[y * side + x];
        let mut edge_ids = Vec::new();
        for y in 0..side {
            for x in 0..side {
                let id = at(x, y);
                if x + 1 < side {
                    edge_ids.push(graph.add_edge(id, at(x + 1, y), EdgeDirection::Directed, ()));
                }
                if y + 1 < side {
                    edge_ids.push(graph.add_edge(id, at(x, y + 1), EdgeDirection::Directed, ()));
                }
            }
        }
        let first_edge = graph
            .edge(edge_ids[0].expect("edge exists"))
            .expect("edge exists");
        let ctx = EdgeCurveContext {
            index: 0,
            signed_density: densities[0],
            has_reverse: &has_reverse,
            parallel: &parallel,
            obstacles: &obstacle_grid,
            obstacle_radius: 6.0 * 2.0 + 30.0,
            // The bench field spans every grid position, including this
            // edge's own endpoints.
            endpoints_in_field: (true, true),
            self_loop_has_node_label: false,
        };
        group.bench_with_input(
            BenchmarkId::new("grid", format!("{}x{}", side, side)),
            &(first_edge, &ctx, &graph, &g.positions, &vp, &style),
            |b, (edge, ctx, graph, positions, vp, style)| {
                b.iter(|| {
                    edge_path(
                        edge,
                        ctx,
                        graph,
                        &|id| positions.get(&id).copied(),
                        &|_| None,
                        vp,
                        style,
                    )
                })
            },
        );
    }

    group.finish();
}

fn bench_trim(c: &mut Criterion) {
    let mut group = c.benchmark_group("trim_curve");

    for side in [20usize, 50usize] {
        let g = GridGraph::new(side);
        let (s, t) = g.edges[0];
        let source = g.positions[&s];
        let target = g.positions[&t];
        let control = (source + target) * 0.5;
        group.bench_with_input(
            BenchmarkId::new("grid", format!("{}x{}", side, side)),
            &(source, control, target),
            |b, (s, c, t)| b.iter(|| trim_curve_to_node_boundary(*s, *c, *t, 6.0)),
        );
    }

    group.finish();
}

/// Stroke-build amortization: the paint layer accumulates every visible
/// edge's curves into one `PathBuilder` per color instead of building one path
/// per edge. This measures the tessellation side of that win headlessly (the
/// per-primitive draw-call saving is on top of it, but needs a window).
fn bench_stroke_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("edge_stroke_build");
    let edge_width = 1.0;

    // Straight-LOD-style chords across a 1200x800 canvas, matching an
    // overview of a mid-size graph.
    for edge_count in [500usize, 2000] {
        let curves: Vec<(Vec2, Vec2, Vec2)> = (0..edge_count)
            .map(|i| {
                let f = i as f32 / edge_count as f32;
                let y = 40.0 + f * 720.0;
                (
                    Vec2::new(10.0, y),
                    Vec2::new(600.0 + (f * 100.0 - 50.0), y + 5.0),
                    Vec2::new(1190.0, y),
                )
            })
            .collect();

        group.bench_with_input(
            BenchmarkId::new("per_edge", edge_count),
            &curves,
            |b, curves| {
                b.iter(|| {
                    let count = curves.len();
                    let mut total = 0usize;
                    for (p0, p1, p2) in curves {
                        let mut builder = PathBuilder::stroke(px(edge_width));
                        builder.move_to(point(px(p0.x), px(p0.y)));
                        builder.curve_to(point(px(p2.x), px(p2.y)), point(px(p1.x), px(p1.y)));
                        if builder.build().is_ok() {
                            total += 1;
                        }
                    }
                    std::hint::black_box((total, count))
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("batched", edge_count),
            &curves,
            |b, curves| {
                b.iter(|| {
                    let mut builder = PathBuilder::stroke(px(edge_width));
                    for (p0, p1, p2) in curves {
                        builder.move_to(point(px(p0.x), px(p0.y)));
                        builder.curve_to(point(px(p2.x), px(p2.y)), point(px(p1.x), px(p1.y)));
                    }
                    builder.build().map(|p| p.vertices.len()).ok()
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
    bench_node_avoidance,
    bench_edge_path,
    bench_trim,
    bench_stroke_build
);
criterion_main!(benches);
