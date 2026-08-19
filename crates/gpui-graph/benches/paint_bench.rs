//! Paint pipeline benchmarks.
//!
//! Measures `build_paint_frame` across viewport scenarios that stress the
//! zoom/pan hot path and the other dominant pipeline stages:
//!
//! - `overview`: every edge visible, so the density pass is O(E·k).
//! - `deep_zoom`: few visible edges, so culling and clipping dominate.
//! - `dense`: a complete graph, so each edge's density neighborhood is large
//!   (k is high) and the density pass dominates.
//! - `labels`: every node and edge carries a label, so text shaping dominates.
//! - `clusters`: nodes grouped into clusters, so cluster-bow geometry dominates.
//! - `self_loops`: every node has a self-loop, so onigiri path building dominates.
//! - `parallel`: many parallel edges between the same node pairs, so the
//!   parallel fan dominates.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use glam::Vec2;
use gpui_graph::graph::{EdgeDirection, Graph, NodeId};
use gpui_graph::interaction::{Hover, Selection};
use gpui_graph::paint::{PaintFrameInput, build_paint_frame};
use gpui_graph::style::GraphStyle;
use gpui_graph::viewport::Viewport;

/// A synthetic graph with deterministic node positions and a node-position
/// resolver, so `build_paint_frame` can be driven without a scene.
struct BenchGraph {
    graph: Graph<(), ()>,
    positions: HashMap<NodeId, Vec2>,
    /// Optional cluster center/radius per node, resolved by `node_cluster_center`.
    clusters: HashMap<NodeId, (Vec2, f32)>,
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
        Self {
            graph,
            positions,
            clusters: HashMap::new(),
        }
    }

    /// Build a complete graph on `n` nodes placed on a circle, so every edge's
    /// midpoint is close to many others and the density neighborhood is large.
    fn complete(n: usize) -> Self {
        let mut graph = Graph::new();
        let mut positions = HashMap::new();
        let radius = 2000.0;
        let mut ids = Vec::new();
        for i in 0..n {
            let id = graph.add_node(());
            let angle = i as f32 / n as f32 * std::f32::consts::TAU;
            positions.insert(id, Vec2::new(angle.cos(), angle.sin()) * radius);
            ids.push(id);
        }
        for i in 0..n {
            for j in (i + 1)..n {
                graph.add_edge(ids[i], ids[j], EdgeDirection::Directed, ());
            }
        }
        Self {
            graph,
            positions,
            clusters: HashMap::new(),
        }
    }

    /// Build a grid graph where nodes are grouped into clusters of `cluster_size`
    /// consecutive nodes, each with a shared center and radius.
    fn grid_with_clusters(side: usize, cluster_size: usize) -> Self {
        let mut g = Self::grid(side);
        let ids: Vec<NodeId> = g.positions.keys().copied().collect();
        for (i, &id) in ids.iter().enumerate() {
            let cluster = i / cluster_size;
            let center = Vec2::new((cluster % 4) as f32 * 500.0, (cluster / 4) as f32 * 500.0);
            g.clusters.insert(id, (center, 200.0));
        }
        g
    }

    /// Build a grid graph where every node has a self-loop.
    fn grid_with_self_loops(side: usize) -> Self {
        let mut g = Self::grid(side);
        let ids: Vec<NodeId> = g.positions.keys().copied().collect();
        for &id in &ids {
            g.graph.add_edge(id, id, EdgeDirection::Directed, ());
        }
        g
    }

    /// Build a graph with `pairs` node pairs, each connected by `parallel`
    /// parallel edges.
    fn parallel(pairs: usize, parallel: usize) -> Self {
        let mut graph = Graph::new();
        let mut positions = HashMap::new();
        let mut ids = Vec::new();
        for i in 0..pairs * 2 {
            let id = graph.add_node(());
            positions.insert(id, Vec2::new(i as f32 * 100.0, 0.0));
            ids.push(id);
        }
        for p in 0..pairs {
            let a = ids[p * 2];
            let b = ids[p * 2 + 1];
            for _ in 0..parallel {
                graph.add_edge(a, b, EdgeDirection::Directed, ());
            }
        }
        Self {
            graph,
            positions,
            clusters: HashMap::new(),
        }
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

fn paint_frame(graph: &BenchGraph, viewport: &Viewport, labels: bool) {
    let style = GraphStyle::default();
    let selection = Selection::new();
    let hover = Hover::default();
    let input = PaintFrameInput {
        graph: &graph.graph,
        node_position: &|id| graph.positions.get(&id).copied(),
        node_cluster_center: &|id| graph.clusters.get(&id).copied(),
        node_label: &|_, _| {
            if labels {
                Some("node".to_string())
            } else {
                None
            }
        },
        edge_label: &|_, _| {
            if labels {
                Some("edge".to_string())
            } else {
                None
            }
        },
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
            |b, (g, vp)| b.iter(|| paint_frame(g, vp, false)),
        );
        group.bench_with_input(
            BenchmarkId::new("deep_zoom", &label),
            &(&graph, &deep),
            |b, (g, vp)| b.iter(|| paint_frame(g, vp, false)),
        );
    }

    // Dense (complete) graph: high per-edge density neighborhood.
    for n in [30usize, 60usize] {
        let graph = BenchGraph::complete(n);
        let overview = overview_viewport(&graph);
        group.bench_with_input(
            BenchmarkId::new("dense", n.to_string()),
            &(&graph, &overview),
            |b, (g, vp)| b.iter(|| paint_frame(g, vp, false)),
        );
    }

    // Labels: text shaping for every node and edge.
    for side in [20usize, 50usize] {
        let graph = BenchGraph::grid(side);
        let overview = overview_viewport(&graph);
        group.bench_with_input(
            BenchmarkId::new("labels", format!("{}x{}", side, side)),
            &(&graph, &overview),
            |b, (g, vp)| b.iter(|| paint_frame(g, vp, true)),
        );
    }

    // Clusters: cluster-bow geometry for every edge.
    for side in [20usize, 50usize] {
        let graph = BenchGraph::grid_with_clusters(side, 10);
        let overview = overview_viewport(&graph);
        group.bench_with_input(
            BenchmarkId::new("clusters", format!("{}x{}", side, side)),
            &(&graph, &overview),
            |b, (g, vp)| b.iter(|| paint_frame(g, vp, false)),
        );
    }

    // Self-loops: onigiri path building for every node.
    for side in [20usize, 50usize] {
        let graph = BenchGraph::grid_with_self_loops(side);
        let overview = overview_viewport(&graph);
        group.bench_with_input(
            BenchmarkId::new("self_loops", format!("{}x{}", side, side)),
            &(&graph, &overview),
            |b, (g, vp)| b.iter(|| paint_frame(g, vp, false)),
        );
    }

    // Parallel edges: parallel fan for every edge.
    for (pairs, parallel) in [(100usize, 10usize), (200usize, 10usize)] {
        let graph = BenchGraph::parallel(pairs, parallel);
        let overview = overview_viewport(&graph);
        group.bench_with_input(
            BenchmarkId::new("parallel", format!("{}x{}", pairs, parallel)),
            &(&graph, &overview),
            |b, (g, vp)| b.iter(|| paint_frame(g, vp, false)),
        );
    }

    group.finish();
}

criterion_group!(benches, bench_paint);
criterion_main!(benches);
