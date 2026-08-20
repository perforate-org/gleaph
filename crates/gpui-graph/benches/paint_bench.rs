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

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use glam::Vec2;
use gpui_graph::graph::EdgeDirection;
use gpui_graph::interaction::{Hover, Selection};
use gpui_graph::paint::{
    IndexedPaintFrameInput, PaintFrameInput, build_indexed_paint_frame, build_paint_frame,
};
use gpui_graph::style::GraphStyle;
use gpui_graph::viewport::Viewport;
use gpui_graph::{
    GraphBatch, GraphRuntime, GraphScene, LayoutBudget, LayoutEngine, LayoutGraph, LayoutProgress,
    LayoutState, SyncedGraphRuntime,
};

/// A synthetic graph scene with deterministic node positions. Runtime setup
/// deliberately goes through `GraphScene::sync_runtime`, the same public
/// synchronization boundary used by production views.
struct BenchGraph<S = std::collections::hash_map::RandomState>
where
    S: std::hash::BuildHasher + Default + Clone,
{
    scene: GraphScene<usize, usize, (), (), S>,
}

/// Layout engine used only to make the benchmark's cluster scenario exercise
/// the same scene-owned cluster-center path as production rendering.
struct BenchmarkClusterLayout {
    cluster_size: usize,
}

impl LayoutEngine for BenchmarkClusterLayout {
    fn rebuild(&mut self, _graph: &LayoutGraph, state: &mut LayoutState) {
        state.cluster_centers.fill(None);
        for index in 0..state.cluster_centers.len() {
            let cluster = index / self.cluster_size;
            let center = Vec2::new((cluster % 4) as f32 * 500.0, (cluster / 4) as f32 * 500.0);
            state.cluster_centers[index] = Some((center, 200.0));
        }
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

impl BenchGraph {
    /// Build a grid graph with `side * side` nodes and edges to the right and
    /// down neighbors, plus a few long-range edges to create density variation.
    fn grid(side: usize) -> Self {
        Self::grid_with_hasher(side, std::collections::hash_map::RandomState::default())
    }

    /// Build a complete graph on `n` nodes placed on a circle, so every edge's
    /// midpoint is close to many others and the density neighborhood is large.
    fn complete(n: usize) -> Self {
        let mut batch = GraphBatch::new();
        let radius = 2000.0;
        for i in 0..n {
            batch = batch.node(i, ());
        }
        let mut edge_key = 0;
        for i in 0..n {
            for j in (i + 1)..n {
                batch = batch.edge(edge_key, i, j, EdgeDirection::Directed, ());
                edge_key += 1;
            }
        }
        let mut scene = GraphScene::new();
        scene.merge(batch);
        for i in 0..n {
            let angle = i as f32 / n as f32 * std::f32::consts::TAU;
            let node = scene.node_id(&i).expect("complete graph node exists");
            scene.set_position(node, Vec2::new(angle.cos(), angle.sin()) * radius);
        }
        Self { scene }
    }

    /// Build a grid graph where nodes are grouped into clusters of `cluster_size`
    /// consecutive nodes, each with a shared center and radius.
    fn grid_with_clusters(side: usize, cluster_size: usize) -> Self {
        let mut g = Self::grid(side);
        g.scene
            .set_layout(Box::new(BenchmarkClusterLayout { cluster_size }));
        g
    }

    /// Build a grid graph where every node has a self-loop.
    fn grid_with_self_loops(side: usize) -> Self {
        let mut g = Self::grid(side);
        let mut batch = GraphBatch::new();
        for (edge_key, id) in (10_000..).zip(0..side * side) {
            batch = batch.edge(edge_key, id, id, EdgeDirection::Directed, ());
        }
        g.scene.merge(batch);
        g
    }

    /// Build a graph with `pairs` node pairs, each connected by `parallel`
    /// parallel edges.
    fn parallel(pairs: usize, parallel: usize) -> Self {
        let mut batch = GraphBatch::new();
        for i in 0..pairs * 2 {
            batch = batch.node(i, ());
        }
        let mut edge_key = 0;
        for p in 0..pairs {
            let a = p * 2;
            let b = p * 2 + 1;
            for _ in 0..parallel {
                batch = batch.edge(edge_key, a, b, EdgeDirection::Directed, ());
                edge_key += 1;
            }
        }
        let mut scene = GraphScene::new();
        scene.merge(batch);
        for i in 0..pairs * 2 {
            let node = scene.node_id(&i).expect("parallel graph node exists");
            scene.set_position(node, Vec2::new(i as f32 * 100.0, 0.0));
        }
        Self { scene }
    }
}

impl<S> BenchGraph<S>
where
    S: std::hash::BuildHasher + Default + Clone,
{
    /// Build a grid graph with `side * side` nodes and edges to the right and
    /// down neighbors, plus a few long-range edges to create density variation.
    fn grid_with_hasher(side: usize, hasher: S) -> Self {
        let mut batch = GraphBatch::new();
        let spacing = 60.0;
        let mut ids = Vec::new();
        for y in 0..side {
            for x in 0..side {
                let id = y * side + x;
                batch = batch.node(id, ());
                ids.push(id);
            }
        }
        let at = |x: usize, y: usize| ids[y * side + x];
        let mut edge_key = 0;
        for y in 0..side {
            for x in 0..side {
                let id = at(x, y);
                if x + 1 < side {
                    batch = batch.edge(edge_key, id, at(x + 1, y), EdgeDirection::Directed, ());
                    edge_key += 1;
                }
                if y + 1 < side {
                    batch = batch.edge(edge_key, id, at(x, y + 1), EdgeDirection::Directed, ());
                    edge_key += 1;
                }
            }
        }
        // A few long-range edges to vary local density.
        for i in 0..(side / 4) {
            let a = at(i * 4 % side, i % side);
            let b = at((i * 7 + 3) % side, (i * 5 + 1) % side);
            if a != b {
                batch = batch.edge(edge_key, a, b, EdgeDirection::Directed, ());
                edge_key += 1;
            }
        }
        let mut scene = GraphScene::with_hasher(hasher);
        scene.merge(batch);
        for y in 0..side {
            for x in 0..side {
                let id = at(x, y);
                let node = scene.node_id(&id).expect("grid node exists");
                scene.set_position(node, Vec2::new(x as f32 * spacing, y as f32 * spacing));
            }
        }
        Self { scene }
    }
}

/// A viewport centered on the graph's bounding box, sized to show it all.
fn overview_viewport(graph: &BenchGraph) -> Viewport {
    let mut min = Vec2::splat(f32::INFINITY);
    let mut max = Vec2::splat(f32::NEG_INFINITY);
    for (id, _) in graph.scene.graph().nodes() {
        if let Some(position) = graph.scene.node_position(id) {
            min = min.min(position);
            max = max.max(position);
        }
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
    for (id, _) in graph.scene.graph().nodes() {
        if let Some(position) = graph.scene.node_position(id) {
            min = min.min(position);
            max = max.max(position);
        }
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
        graph: graph.scene.graph(),
        node_position: &|id| graph.scene.node_position(id),
        node_cluster_center: &|id| graph.scene.node_cluster_center(id),
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

/// Build a paint frame using the spatial index, given a prebuilt runtime.
fn paint_frame_indexed_with<S>(
    viewport: &Viewport,
    labels: bool,
    synced: &SyncedGraphRuntime<'_, usize, usize, (), (), S>,
) where
    S: std::hash::BuildHasher + Default + Clone,
{
    let style = GraphStyle::default();
    let selection = Selection::new();
    let hover = Hover::default();
    let input = IndexedPaintFrameInput {
        synced,
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
    let frame = build_indexed_paint_frame(input);
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

/// Compare the linear-scan path against the spatial-index path. The index is
/// rebuilt once and reused, so this isolates the per-frame query + visibility
/// cost. The deep-zoom scenario (few visible primitives in a large graph) is
/// where the index should win; the overview scenario (everything visible) shows
/// the index's query overhead.
fn bench_paint_indexed(c: &mut Criterion) {
    let mut group = c.benchmark_group("paint_frame_indexed");

    for side in [20usize, 50usize] {
        let graph = BenchGraph::grid(side);
        let overview = overview_viewport(&graph);
        let deep = deep_zoom_viewport(&graph);
        let label = format!("{}x{}", side, side);
        // Build the spatial index once, outside the timed closure, so the
        // benchmark measures the per-frame indexed path (not the rebuild).
        let mut runtime = GraphRuntime::new();
        let synced = graph.scene.sync_runtime(&mut runtime);

        group.bench_with_input(
            BenchmarkId::new("overview_scan", &label),
            &(&graph, &overview),
            |b, (g, vp)| b.iter(|| paint_frame(g, vp, false)),
        );
        group.bench_with_input(
            BenchmarkId::new("overview_indexed", &label),
            &(&overview, &synced),
            |b, (vp, proof)| b.iter(|| paint_frame_indexed_with(vp, false, proof)),
        );
        group.bench_with_input(
            BenchmarkId::new("deep_zoom_scan", &label),
            &(&graph, &deep),
            |b, (g, vp)| b.iter(|| paint_frame(g, vp, false)),
        );
        group.bench_with_input(
            BenchmarkId::new("deep_zoom_indexed", &label),
            &(&deep, &synced),
            |b, (vp, proof)| b.iter(|| paint_frame_indexed_with(vp, false, proof)),
        );
    }

    group.finish();
}

/// Compare the indexed paint path under the default SipHash hasher against the
/// same path under `rapidhash::fast::RandomState`, isolating the hasher's
/// contribution to the per-frame spatial-grid cost.
fn bench_paint_hashers(c: &mut Criterion) {
    let mut group = c.benchmark_group("paint_frame_hasher");

    for side in [20usize, 50usize] {
        let graph = BenchGraph::grid(side);
        let rapid_graph = BenchGraph::<rapidhash::fast::RandomState>::grid_with_hasher(
            side,
            rapidhash::fast::RandomState::default(),
        );
        let overview = overview_viewport(&graph);
        let deep = deep_zoom_viewport(&graph);
        let label = format!("{}x{}", side, side);

        // Default SipHash hasher.
        let mut sip = GraphRuntime::new();
        let sip_synced = graph.scene.sync_runtime(&mut sip);
        // rapidhash hasher.
        let mut rapid = GraphRuntime::<rapidhash::fast::RandomState>::default();
        let rapid_synced = rapid_graph.scene.sync_runtime(&mut rapid);

        group.bench_with_input(
            BenchmarkId::new("overview_sip", &label),
            &(&overview, &sip_synced),
            |b, (vp, proof)| b.iter(|| paint_frame_indexed_with(vp, false, proof)),
        );
        group.bench_with_input(
            BenchmarkId::new("overview_rapid", &label),
            &(&overview, &rapid_synced),
            |b, (vp, proof)| b.iter(|| paint_frame_indexed_with(vp, false, proof)),
        );
        group.bench_with_input(
            BenchmarkId::new("deep_zoom_sip", &label),
            &(&deep, &sip_synced),
            |b, (vp, proof)| b.iter(|| paint_frame_indexed_with(vp, false, proof)),
        );
        group.bench_with_input(
            BenchmarkId::new("deep_zoom_rapid", &label),
            &(&deep, &rapid_synced),
            |b, (vp, proof)| b.iter(|| paint_frame_indexed_with(vp, false, proof)),
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_paint,
    bench_paint_indexed,
    bench_paint_hashers
);
criterion_main!(benches);
