//! Hit-testing benchmarks (§21).
//!
//! Measures the shipped indexed candidate path against a naive full-scan
//! nearest-node baseline across graph sizes, grounding the small-graph
//! policy question left open in §21: whether a direct scan should replace
//! the spatial index below some node count.
//!
//! Query points cover the three pointer outcomes: a node center (node hit),
//! an edge midpoint (edge hit), and an empty cell center (miss that still
//! pays for edge candidate collection).

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use glam::Vec2;
use gpui_graph::graph::{EdgeDirection, NodeId};
use gpui_graph::style::GraphStyle;
use gpui_graph::viewport::{Viewport, WorldBounds};
use gpui_graph::{GraphBatch, GraphRuntime, GraphScene, hit_test};

/// Lattice spacing in world units; matches the layout bench fixtures.
const SPACING: f32 = 40.0;

struct HitCase {
    scene: GraphScene<usize, usize, (), ()>,
    vp: Viewport,
}

/// Build a `side x side` lattice graph fitted into a typical viewport.
fn grid_case(side: usize) -> HitCase {
    let mut batch = GraphBatch::new();
    for i in 0..side * side {
        batch = batch.node(i, ());
    }
    let mut edge_key = 0usize;
    for y in 0..side {
        for x in 0..side {
            let i = y * side + x;
            if x + 1 < side {
                batch = batch.edge(edge_key, i, i + 1, EdgeDirection::Undirected, ());
                edge_key += 1;
            }
            if y + 1 < side {
                batch = batch.edge(edge_key, i, i + side, EdgeDirection::Undirected, ());
                edge_key += 1;
            }
        }
    }
    let mut scene = GraphScene::new();
    scene.merge(batch);
    for i in 0..side * side {
        let x = (i % side) as f32 * SPACING;
        let y = (i / side) as f32 * SPACING;
        let node = scene.node_id(&i).expect("grid node exists");
        scene.set_position(node, Vec2::new(x, y));
    }

    let extent = (side - 1) as f32 * SPACING;
    let mut vp = Viewport::new();
    // Keep the on-screen node spacing constant (~30px for 40-world-unit
    // spacing) across graph sizes. Fitting every size into one fixed window
    // would shrink large graphs below pointer precision and turn every query
    // into a trivial node-phase early return. Note fit_bounds' second
    // parameter is a zoom FRACTION, not pixels: 0.0 keeps the raw scale.
    vp.set_size(Vec2::splat(extent * (1.0 / 0.75)));
    vp.fit_bounds(
        WorldBounds {
            min: Vec2::ZERO,
            max: Vec2::splat(extent),
        },
        0.0,
    );
    HitCase { scene, vp }
}

/// Naive O(N) nearest-node scan: the direct-scan alternative §21 weighs the
/// spatial index against.
fn scan_nearest_node(
    scene: &GraphScene<usize, usize, (), ()>,
    world: Vec2,
    radius: f32,
) -> Option<NodeId> {
    let mut best: Option<(NodeId, f32)> = None;
    for (id, _) in scene.graph().nodes() {
        if let Some(p) = scene.node_position(id) {
            let d = p.distance(world);
            if d <= radius && best.is_none_or(|(_, b)| d < b) {
                best = Some((id, d));
            }
        }
    }
    best.map(|(id, _)| id)
}

fn bench_hit_test(c: &mut Criterion) {
    let mut group = c.benchmark_group("hit_test");
    group.sample_size(30);
    let style = GraphStyle::default();

    for side in [32usize, 100usize] {
        let case = grid_case(side);
        let size = format!("{side}x{side}");

        // Middle of the lattice: a node center, an adjacent edge midpoint,
        // and a cell center where nothing sits.
        let mid = ((side / 2) as f32) * SPACING;
        let on_node_world = Vec2::new(mid, mid);
        let on_edge_world = Vec2::new(mid + SPACING / 2.0, mid);
        let empty_world = Vec2::new(mid + SPACING / 2.0, mid + SPACING / 2.0);
        let on_node_screen = case.vp.world_to_screen(on_node_world);
        let on_edge_screen = case.vp.world_to_screen(on_edge_world);
        let empty_screen = case.vp.world_to_screen(empty_world);

        let mut runtime = GraphRuntime::new();
        let synced = case.scene.sync_runtime(&mut runtime);

        // Black-box the pointer as well: hit_test is a pure function of
        // captured constants, so without hiding the input LLVM hoists the
        // whole computation out of the measured loop.
        group.bench_function(BenchmarkId::new("indexed", format!("node/{size}")), |b| {
            b.iter(|| {
                let p = std::hint::black_box(on_node_screen);
                std::hint::black_box(hit_test(&synced, &case.vp, &style, p))
            })
        });
        group.bench_function(BenchmarkId::new("indexed", format!("edge/{size}")), |b| {
            b.iter(|| {
                let p = std::hint::black_box(on_edge_screen);
                std::hint::black_box(hit_test(&synced, &case.vp, &style, p))
            })
        });
        group.bench_function(BenchmarkId::new("indexed", format!("empty/{size}")), |b| {
            b.iter(|| {
                let p = std::hint::black_box(empty_screen);
                std::hint::black_box(hit_test(&synced, &case.vp, &style, p))
            })
        });
        group.bench_function(BenchmarkId::new("scan", format!("node/{size}")), |b| {
            b.iter(|| {
                let w = std::hint::black_box(on_node_world);
                std::hint::black_box(scan_nearest_node(&case.scene, w, style.node_radius))
            })
        });

        // Sanity: every measured point must exercise the path its name
        // claims, and both node paths must agree.
        let indexed_node = gpui_graph::hit_test(&synced, &case.vp, &style, on_node_screen);
        let scanned = scan_nearest_node(&case.scene, on_node_world, style.node_radius);
        assert_eq!(
            indexed_node.node, scanned,
            "paths disagree at {size} on_node"
        );
        let indexed_edge = hit_test(&synced, &case.vp, &style, on_edge_screen);
        assert!(
            indexed_edge.node.is_none(),
            "edge point hit a node at {size}"
        );
        assert!(
            indexed_edge.edge.is_some(),
            "edge point missed the edge at {size}"
        );
        let indexed_empty = hit_test(&synced, &case.vp, &style, empty_screen);
        assert!(
            !indexed_empty.is_hit(),
            "empty point unexpectedly hit at {size}"
        );

        group.sample_size(10);
    }

    group.finish();
}

criterion_group!(benches, bench_hit_test);
criterion_main!(benches);
