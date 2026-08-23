//! Paint-path benchmarks (§18.2, §22): indexed paint frame construction and
//! edge path tessellation. Both run headlessly — the paint frame builder is
//! pure geometry, and `PathBuilder::build` tessellates on the CPU without a
//! window — so these measure exactly the per-frame work `GraphView` performs
//! in its prepaint and paint closures.

use std::time::Duration;

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use glam::Vec2;
use gpui_graph::{
    Hover, NodeId, PaintFrame, Selection, Viewport, WorldBounds,
    graph::{EdgeDirection, Graph},
    paint::{IndexedPaintFrameInput, build_indexed_paint_frame},
    runtime::GraphRuntime,
    scene::GraphScene,
    style::GraphStyle,
};

/// Deterministic pseudo-random coordinates (xorshift), so fixture geometry is
/// reproducible across runs without pulling a rand dependency.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn coord(&mut self, span: f32) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0 * span - span * 0.5
    }
}

/// A graph plus the world-space placement the paint benches view.
struct Fixture {
    graph: Graph<(), ()>,
    positions: Vec<Vec2>,
}

fn grid(side: usize) -> Fixture {
    let spacing = 30.0;
    let mut graph = Graph::new();
    let mut positions = Vec::with_capacity(side * side);
    // Row-major ids; neighbors are referenced through this vector because
    // NodeId is an opaque slotmap key.
    let ids: Vec<NodeId> = (0..side * side)
        .map(|i| {
            let (x, y) = (i % side, i / side);
            positions.push(Vec2::new(x as f32 * spacing, y as f32 * spacing));
            graph.add_node(())
        })
        .collect();
    for y in 0..side {
        for x in 0..side {
            let id = ids[y * side + x];
            if x + 1 < side {
                graph.add_edge(id, ids[y * side + x + 1], EdgeDirection::Undirected, ());
            }
            if y + 1 < side {
                graph.add_edge(id, ids[(y + 1) * side + x], EdgeDirection::Undirected, ());
            }
        }
    }
    Fixture { graph, positions }
}

fn hub(leaves: usize) -> Fixture {
    let radius = 600.0;
    let mut graph = Graph::new();
    let mut positions = Vec::with_capacity(leaves + 1);
    let center = graph.add_node(());
    positions.push(Vec2::ZERO);
    for i in 0..leaves {
        let angle = i as f32 / leaves as f32 * std::f32::consts::TAU;
        let leaf = graph.add_node(());
        positions.push(Vec2::from_angle(angle) * radius);
        graph.add_edge(center, leaf, EdgeDirection::Undirected, ());
    }
    Fixture { graph, positions }
}

fn random(count: usize) -> Fixture {
    let span = 1500.0;
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    let mut graph = Graph::new();
    let mut positions = Vec::with_capacity(count);
    let ids: Vec<NodeId> = (0..count)
        .map(|_| {
            positions.push(Vec2::new(rng.coord(span), rng.coord(span)));
            graph.add_node(())
        })
        .collect();
    // Ring plus a fixed-stride shortcut keeps degree bounded and
    // deterministic while producing plenty of long crossing edges.
    for i in 0..count {
        let prev = (i + count - 1) % count;
        graph.add_edge(ids[i], ids[prev], EdgeDirection::Undirected, ());
        let skip = (i + 7) % count;
        if skip != i && skip != prev {
            graph.add_edge(ids[i], ids[skip], EdgeDirection::Undirected, ());
        }
    }
    Fixture { graph, positions }
}

/// A scene with the fixture merged in and positions set, plus a viewport
/// fitted to show every node (the worst case: nothing culls).
struct PaintBench {
    scene: GraphScene<String, String, (), (), gpui_graph::DefaultBuildHasher>,
    viewport: Viewport,
    style: GraphStyle,
    selection: Selection,
    hover: Hover,
}

impl PaintBench {
    fn build(fixture: &Fixture) -> Self {
        let n = fixture.graph.node_count();
        let mut scene = GraphScene::new();
        let batch = (0..n).fold(gpui_graph::GraphBatch::new(), |b, i| {
            b.node(format!("n{i}"), ())
        });
        // String keys mirror the logical fixture's insertion order.
        let keys: std::collections::HashMap<NodeId, String> = fixture
            .graph
            .nodes()
            .enumerate()
            .map(|(i, (id, _))| (id, format!("n{i}")))
            .collect();
        let batch = fixture
            .graph
            .edges()
            .enumerate()
            .fold(batch, |b, (index, (_, edge))| {
                b.edge(
                    format!("e{index}"),
                    keys[&edge.source].clone(),
                    keys[&edge.target].clone(),
                    edge.direction,
                    (),
                )
            });
        scene.merge(batch);

        // Iterate the merged ids (insertion order) and place each node.
        let ids: Vec<NodeId> = scene.graph().nodes().map(|(id, _)| id).collect();
        for (id, position) in ids.into_iter().zip(fixture.positions.iter()) {
            scene.set_position(id, *position);
        }

        let mut viewport = Viewport::new();
        viewport.set_size(Vec2::new(1200.0, 800.0));
        let mut bounds = WorldBounds {
            min: Vec2::splat(f32::INFINITY),
            max: Vec2::splat(f32::NEG_INFINITY),
        };
        for p in &fixture.positions {
            bounds.min = bounds.min.min(*p);
            bounds.max = bounds.max.max(*p);
        }
        viewport.fit_bounds(bounds, 0.0);

        Self {
            scene,
            viewport,
            style: GraphStyle::default(),
            selection: Selection::new(),
            hover: Hover::default(),
        }
    }

    /// Zoom into the central third of the fitted view: roughly one node in
    /// nine stays visible and the rest of the candidate set reaches the
    /// precise cull with both endpoints off-screen.
    fn zoom_viewport(&mut self) {
        self.viewport.zoom_at(self.viewport.size() * 0.5, 3.0);
    }

    /// Build one indexed paint frame against a freshly synced runtime.
    fn build_frame(&self, runtime: &mut GraphRuntime) -> PaintFrame {
        let synced = self.scene.sync_runtime(runtime);
        build_indexed_paint_frame(IndexedPaintFrameInput {
            synced: &synced,
            node_label: &|_, _| Some("Node".to_string()),
            edge_label: &|_, _| None,
            viewport: &self.viewport,
            style: &self.style,
            selection: &self.selection,
            hover: &self.hover,
            node_overlay: None,
            edge_overlay: None,
        })
    }
}

fn bench_paint_frame(c: &mut Criterion) {
    let mut group = c.benchmark_group("paint_frame");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(10));

    for (name, fixture, zoomed) in [
        ("grid_100x100", grid(100), false),
        ("hub_4096", hub(4096), false),
        ("random_5000", random(5000), false),
        // A zoomed-in view: most edges have both endpoints off-screen, so
        // this case exercises the precise cull that the fitted views skip.
        ("random_5000_zoomed", random(5000), true),
    ] {
        let mut bench = PaintBench::build(&fixture);
        if zoomed {
            bench.zoom_viewport();
        }
        let mut runtime = GraphRuntime::new();
        let throughput =
            Throughput::Elements((fixture.graph.node_count() + fixture.graph.edge_count()) as u64);
        group.throughput(throughput);
        group.bench_function(name, |b| {
            b.iter(|| {
                let frame = bench.build_frame(&mut runtime);
                black_box((frame.nodes.len(), frame.edges.len()))
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_paint_frame);
criterion_main!(benches);
