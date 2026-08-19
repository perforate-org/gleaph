//! Paint pipeline benchmarks.
//!
//! Measures `build_paint_frame` across viewport scenarios that stress the
//! zoom/pan hot path: a full-graph overview (every edge visible, so the
//! density pass is O(E·k)), a deep zoom into a small region (few visible
//! edges, so culling and clipping dominate), and a medium graph.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use glam::Vec2;
use gpui_graph::graph::{EdgeDirection, Graph, NodeId};
use gpui_graph::interaction::{Hover, Selection};
use gpui_graph::paint::{PaintFrameInput, build_paint_frame};
use gpui_graph::style::GraphStyle;
use gpui_graph::viewport::Viewport;

/// A synthetic graph laid out on a grid so node positions are deterministic
/// and edges connect nearby nodes (a common ForceAtlas2-like outcome).
struct BenchGraph {
    graph: Graph<(), ()>,
    positions: HashMap<NodeId, Vec2>,
}

impl BenchGraph {
    /// Build a grid graph with `side * side` nodes and edges to the right and
    /// down neighbors, plus a few long-range edges to create density variation.
    fn grid(side: usize) -> Self {
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
        for y in 0..side {
            for x in 0..side {
                let id = at(x, y);
                if x + 1 < side {
                    graph.add_edge(id, at(x + 1, y), EdgeDirection::Directed, ());
                }
                if y + 1 < side {
                    graph.add_edge(id, at(x, y + 1), EdgeDirection::Directed, ());
                }
            }
        }
        // A few long-range edges to vary local density.
        for i in 0..(side / 4) {
            let a = at(i * 4 % side, i % side);
            let b = at((i * 7 + 3) % side, (i * 5 + 1) % side);
            if a != b {
                graph.add_edge(a, b, EdgeDirection::Directed, ());
            }
        }
        Self { graph, positions }
    }
}

/// A viewport centered on the graph's bounding box, sized to show it all.
fn overview_viewport(graph: &BenchGraph) -> Viewport {
    let mut min = Vec2::splat(f32::INFINITY);
    let mut max = Vec2::splat(f32::NEG_INFINITY);
    for &p in graph.positions.values() {
        min = min.min(p);
        max = max.max(p);
    }
    let world_size = max - min;
    let size = Vec2::new(1600.0, 1000.0);
    let zoom = (size / world_size).min_element() * 0.9;
    let mut vp = Viewport::new();
    vp.set_size(size);
    vp.zoom_at(Vec2::new(size.x * 0.5, size.y * 0.5), zoom);
    vp
}

/// A viewport zoomed deep into the center of the graph, so only a handful of
/// nodes and edges are visible.
fn deep_zoom_viewport(graph: &BenchGraph) -> Viewport {
    let mut min = Vec2::splat(f32::INFINITY);
    let mut max = Vec2::splat(f32::NEG_INFINITY);
    for &p in graph.positions.values() {
        min = min.min(p);
        max = max.max(p);
    }
    let center = (min + max) * 0.5;
    let size = Vec2::new(1600.0, 1000.0);
    let mut vp = Viewport::new();
    vp.set_size(size);
    // Zoom so the viewport spans ~4 world units (a few nodes).
    vp.zoom_at(Vec2::new(size.x * 0.5, size.y * 0.5), 400.0);
    vp.pan(center - vp.screen_to_world(Vec2::new(size.x * 0.5, size.y * 0.5)));
    vp
}

fn paint_frame(graph: &BenchGraph, viewport: &Viewport) {
    let style = GraphStyle::default();
    let selection = Selection::new();
    let hover = Hover::default();
    let input = PaintFrameInput {
        graph: &graph.graph,
        node_position: &|id| graph.positions.get(&id).copied(),
        node_cluster_center: &|_| None,
        node_label: &|_, _| None,
        edge_label: &|_, _| None,
        viewport,
        style: &style,
        selection: &selection,
        hover: &hover,
    };
    let frame = build_paint_frame(input);
    std::hint::black_box(frame);
}

fn bench_paint(c: &mut Criterion) {
    let mut group = c.benchmark_group("paint_frame");

    for side in [20usize, 50usize] {
        let graph = BenchGraph::grid(side);
        let overview = overview_viewport(&graph);
        let deep = deep_zoom_viewport(&graph);
        let label = format!("{}x{}", side, side);

        group.bench_with_input(
            BenchmarkId::new("overview", &label),
            &(&graph, &overview),
            |b, (g, vp)| b.iter(|| paint_frame(g, vp)),
        );
        group.bench_with_input(
            BenchmarkId::new("deep_zoom", &label),
            &(&graph, &deep),
            |b, (g, vp)| b.iter(|| paint_frame(g, vp)),
        );
    }

    group.finish();
}

criterion_group!(benches, bench_paint);
criterion_main!(benches);
