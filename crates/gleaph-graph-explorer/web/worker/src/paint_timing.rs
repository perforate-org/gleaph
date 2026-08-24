//! Pure timing-harness core for indexed paint-frame construction (ADR 0076 S4a).
//!
//! The fixture and the measured operation mirror
//! `crates/gpui-graph/benches/paint_bench.rs` (`random(count)` plus the fitted
//! 1200×800 viewport and `build_indexed_paint_frame`) so that numbers produced
//! here are directly comparable with that bench's native measurements — but
//! nothing in this module is wasm-specific: the same function runs natively to
//! produce the serial baseline the scalar penalty is quoted against.
//!
//! Native targets link rayon, so the "native" number has two readings: with
//! rayon workers (the parallel baseline) and with a one-thread pool
//! (`RAYON_NUM_THREADS=1`, the serial baseline). The ADR 0076 scalar-penalty
//! estimate is replaced by wasm-serial ÷ native-serial.
//!
//! The fixture itself ([`random_fixture`]) doubles as the web entry's demo
//! graph, so the browser wiring proof animates exactly the shape the harness
//! times.

use std::hint::black_box;

use glam::Vec2;
use gpui_graph::{
    DefaultBuildHasher, GraphBatch, GraphRuntime, GraphScene, Hover, NodeId, PaintFrame,
    PaintFrameWire, Selection, Viewport, WorldBounds,
    graph::Graph,
    paint::{IndexedPaintFrameInput, build_indexed_paint_frame},
    style::GraphStyle,
};
use serde::Serialize;
use web_time::Instant;

/// Frames built before timing starts, so allocator and cache state settle.
const WARMUP_ITERATIONS: usize = 3;

/// One timed measurement series.
#[derive(Debug, Clone, Serialize)]
pub struct PaintBuildStats {
    /// Nodes in the fixture graph.
    pub nodes: usize,
    /// Edges in the fixture graph.
    pub edges: usize,
    /// Timed iterations (excluding warmup).
    pub iterations: usize,
    /// Mean build time in milliseconds.
    pub mean_ms: f64,
    /// Fastest build in milliseconds.
    pub min_ms: f64,
    /// Median build in milliseconds.
    pub p50_ms: f64,
    /// Slowest build in milliseconds.
    pub max_ms: f64,
    /// Size of the finished frame's transferable wire form (bytes) — the
    /// postMessage payload the worker would ship per frame.
    pub wire_bytes: usize,
}

/// Time repeated indexed paint-frame builds over the deterministic random
/// fixture at `node_count`.
pub fn measure_paint_build(node_count: usize, iterations: usize) -> PaintBuildStats {
    let fixture = random_fixture(node_count);
    let bench = FixtureScene::build(&fixture);
    let edges = fixture.batch.edges.len();

    let mut runtime = GraphRuntime::new();
    let mut last_frame = PaintFrame::default();
    for _ in 0..WARMUP_ITERATIONS {
        last_frame = bench.build_frame(&mut runtime);
    }
    let wire_bytes = PaintFrameWire::encode(&last_frame).to_wire_bytes().len();

    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        last_frame = bench.build_frame(&mut runtime);
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    black_box(&last_frame);

    samples.sort_by(|a, b| a.total_cmp(b));
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let p50 = samples[samples.len() / 2];
    PaintBuildStats {
        nodes: node_count,
        edges,
        iterations,
        mean_ms: mean,
        min_ms: samples[0],
        p50_ms: p50,
        max_ms: samples[samples.len() - 1],
        wire_bytes,
    }
}

/// Deterministic pseudo-random coordinates (xorshift), matching
/// `benches/paint_bench.rs::Lcg`.
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

/// A deterministic random graph as a merge batch plus world-space placement:
/// ring plus a fixed-stride shortcut (`benches/paint_bench.rs::random`), node
/// keys `n{i}`, edge keys `e{index}` in insertion order.
pub struct SceneFixture {
    /// The graph content to merge into a scene.
    pub batch: GraphBatch<String, String, String, String>,
    /// One position per node, in batch insertion order.
    pub positions: Vec<Vec2>,
}

/// Build the deterministic random fixture at `count` nodes.
pub fn random_fixture(count: usize) -> SceneFixture {
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
    for i in 0..count {
        let prev = (i + count - 1) % count;
        graph.add_edge(ids[i], ids[prev], gpui_graph::EdgeDirection::Undirected, ());
        let skip = (i + 7) % count;
        if skip != i && skip != prev {
            graph.add_edge(ids[i], ids[skip], gpui_graph::EdgeDirection::Undirected, ());
        }
    }

    // Key assignment mirrors benches/paint_bench.rs: nodes n{i} in insertion
    // order, edges e{index} over the logical graph's edge iteration order —
    // identical topology and ordering, string-keyed for the worker protocol.
    let mut batch = GraphBatch::new();
    for (index, _) in graph.nodes().enumerate() {
        batch = batch.node(format!("n{index}"), format!("n{index}"));
    }
    let keys: std::collections::HashMap<NodeId, String> = graph
        .nodes()
        .enumerate()
        .map(|(i, (id, _))| (id, format!("n{i}")))
        .collect();
    for (index, (_, edge)) in graph.edges().enumerate() {
        batch = batch.edge(
            format!("e{index}"),
            keys[&edge.source].clone(),
            keys[&edge.target].clone(),
            edge.direction,
            String::new(),
        );
    }

    SceneFixture { batch, positions }
}

/// The fixture merged into a scene with positions set and a viewport fitted to
/// show every node (nothing culls), exactly as `benches/paint_bench.rs` does.
struct FixtureScene {
    scene: GraphScene<String, String, (), (), DefaultBuildHasher>,
    viewport: Viewport,
    style: GraphStyle,
    selection: Selection,
    hover: Hover,
}

impl FixtureScene {
    fn build(fixture: &SceneFixture) -> Self {
        // Timing uses unit payloads; only labels reach the frame builder.
        let timed_batch = fixture.batch.nodes.iter().fold(
            GraphBatch::new(),
            |b: GraphBatch<String, String, (), ()>, (key, _)| b.node(key.clone(), ()),
        );
        let timed_batch = fixture.batch.edges.iter().enumerate().fold(
            timed_batch,
            |b, (_, (key, source, target, direction, _))| {
                b.edge(key.clone(), source.clone(), target.clone(), *direction, ())
            },
        );

        let mut scene = GraphScene::new();
        scene.merge(timed_batch);

        let ids: Vec<NodeId> = scene.graph().nodes().map(|(id, _)| id).collect();
        for (id, position) in ids.into_iter().zip(&fixture.positions) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurement_reports_fixture_counts_and_sane_samples() {
        // Every node gets its ring edge and its stride edge at this size.
        let stats = measure_paint_build(300, 5);
        assert_eq!(stats.nodes, 300);
        assert_eq!(stats.edges, 600);
        assert_eq!(stats.iterations, 5);
        assert!(stats.mean_ms > 0.0);
        assert!(stats.min_ms <= stats.p50_ms);
        assert!(stats.p50_ms <= stats.max_ms);
        assert!(stats.wire_bytes > 0, "a built frame must serialize");
    }

    #[test]
    fn tiny_fixtures_stay_deterministic() {
        // At n = 8 the stride (i + 7) % n always equals prev, so every stride
        // edge is skipped by the guard shared with the bench: ring only.
        let stats = measure_paint_build(8, 3);
        assert_eq!(stats.nodes, 8);
        assert_eq!(stats.edges, 8);
    }

    #[test]
    fn fixture_batches_carry_display_labels_for_every_node() {
        let fixture = random_fixture(120);
        assert_eq!(fixture.batch.nodes.len(), 120);
        assert_eq!(fixture.positions.len(), 120);
        assert!(
            fixture
                .batch
                .nodes
                .iter()
                .all(|(key, label)| label == key && !label.is_empty())
        );
    }
}
