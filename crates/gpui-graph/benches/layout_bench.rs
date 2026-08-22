//! ForceAtlas2 layout benchmarks.
//!
//! Measures the engine's `step` on graphs that stress the two dominant force
//! phases:
//!
//! - `grid`: uniform grid graph, so repulsion (spatial-grid pair tests)
//!   dominates.
//! - `hub`: star graph whose leaves all sit near each other on a ring, so both
//!   repulsion and attraction stay heavy.
//! - `random`: pseudo-random scatter with path edges, approximating realistic
//!   large-graph density.
//!
//! Cases up to 40x40 / 1024 use 30 samples; the 100x100, 4096, and 5000-node
//! cases use 10 samples to keep total run time bounded.
//!
//! Every timed closure rebuilds a fresh engine from a cloned template state, so
//! each sample measures identical work: an unsettled configuration under a
//! fixed iteration budget, rather than progress toward convergence.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use glam::Vec2;
use gpui_graph::graph::{EdgeDirection, Graph, NodeId};
use gpui_graph::layout::{
    ForceAtlas2, LayoutBudget, LayoutDelta, LayoutEdge, LayoutEngine, LayoutGraph, LayoutIndex,
    LayoutNode, LayoutProgress, LayoutState, LayoutSync,
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
            graph: LayoutGraph::new(vec![LayoutNode {}; node_ids.len()], edges, node_ids, 0),
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

    /// Nodes scattered pseudo-randomly over a bounded region (deterministic
    /// LCG), chained by path edges plus occasional long links. Approximates a
    /// realistic scale-free-ish density where repulsion still dominates.
    fn random(count: usize) -> Self {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut g = Graph::new();
        let mut positions_by_id = HashMap::new();
        let extent = 2000.0;
        let ids: Vec<_> = (0..count)
            .map(|_| {
                let id = g.add_node(());
                let x = (next() % 1_000_001) as f32 / 1_000_000.0 * extent;
                let y = (next() % 1_000_001) as f32 / 1_000_000.0 * extent;
                positions_by_id.insert(id, Vec2::new(x, y));
                id
            })
            .collect();
        for i in 1..ids.len() {
            let target = if i % 32 == 0 && i > 100 {
                ids[i - 100]
            } else {
                ids[i - 1]
            };
            g.add_edge(ids[i - 1], target, EdgeDirection::Undirected, ());
        }
        Self::project(&g, &positions_by_id)
    }
}

fn step_bench(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    id: BenchmarkId,
    engine: &dyn Fn() -> ForceAtlas2,
    case: &BenchCase,
) {
    group.bench_with_input(id, case, |b, case| {
        b.iter(|| {
            let mut engine = engine();
            let mut state = case.state.clone();
            engine.rebuild(&case.graph, &mut state);
            std::hint::black_box(engine.step(
                &case.graph,
                &mut state,
                LayoutBudget { max_iterations: 8 },
            ));
        })
    });
}

fn bench_force_atlas2_step(c: &mut Criterion) {
    let mut group = c.benchmark_group("force_atlas2_step");
    group.sample_size(30);
    let default_engine = || ForceAtlas2::default();
    // Barnes-Hut is opt-in by default; keep its favorable (dense ring) and
    // unfavorable (sparse grid) cases measured so the activation policy
    // decision (§37) stays grounded in data.
    let bh_engine = || ForceAtlas2::default().with_barnes_hut_threshold(0);

    for side in [20usize, 40usize] {
        step_bench(
            &mut group,
            BenchmarkId::new("grid", format!("{}x{}", side, side)),
            &default_engine,
            &BenchCase::grid(side),
        );
    }

    for leaves in [256usize, 1024usize] {
        step_bench(
            &mut group,
            BenchmarkId::new("hub", leaves.to_string()),
            &default_engine,
            &BenchCase::hub(leaves),
        );
        step_bench(
            &mut group,
            BenchmarkId::new("hub_bh", leaves.to_string()),
            &bh_engine,
            &BenchCase::hub(leaves),
        );
    }

    // Large graphs: fewer samples to keep total run time bounded.
    group.sample_size(10);
    step_bench(
        &mut group,
        BenchmarkId::new("grid", "100x100"),
        &default_engine,
        &BenchCase::grid(100),
    );
    step_bench(
        &mut group,
        BenchmarkId::new("grid_bh", "100x100"),
        &bh_engine,
        &BenchCase::grid(100),
    );
    step_bench(
        &mut group,
        BenchmarkId::new("hub", "4096"),
        &default_engine,
        &BenchCase::hub(4096),
    );
    step_bench(
        &mut group,
        BenchmarkId::new("hub_bh", "4096"),
        &bh_engine,
        &BenchCase::hub(4096),
    );
    step_bench(
        &mut group,
        BenchmarkId::new("random", "5000"),
        &default_engine,
        &BenchCase::random(5000),
    );
    step_bench(
        &mut group,
        BenchmarkId::new("random_bh", "5000"),
        &bh_engine,
        &BenchCase::random(5000),
    );

    group.finish();
}

/// Iteration cap for a settle run so a pathological configuration cannot run
/// away; a healthy layout settles far below it.
const SETTLE_ITERATION_CAP: u32 = 2000;

fn settle_bench(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    id: BenchmarkId,
    case: &BenchCase,
) {
    group.bench_with_input(id, case, |b, case| {
        b.iter(|| {
            let mut engine = ForceAtlas2::default();
            let mut state = case.state.clone();
            engine.rebuild(&case.graph, &mut state);
            let budget = LayoutBudget { max_iterations: 1 };
            let mut iters = 0u32;
            while iters < SETTLE_ITERATION_CAP
                && engine.step(&case.graph, &mut state, budget) != LayoutProgress::Settled
            {
                iters += 1;
            }
            std::hint::black_box(iters);
        })
    });
}

/// Time-to-settle, not per-step cost: this is the metric adaptive speed
/// targets (fewer iterations to convergence without oscillation), and it is
/// the baseline any integration change must be judged against.
/// Time-to-settle after an incremental topology change (§11.6): the engine
/// settles a hub graph, then 16 fresh leaves join. `hub_sync` applies the
/// growth through [`LayoutEngine::apply_delta`] (adaptation carried over);
/// `hub_rebuild` is the fallback path a non-syncing engine would take, full
/// `rebuild` over the grown projection with positions preserved. The gap is
/// the measured value of incremental state carrying.
fn bench_force_atlas2_dynamic(c: &mut Criterion) {
    let mut group = c.benchmark_group("force_atlas2_dynamic");
    group.sample_size(10);
    let budget = LayoutBudget { max_iterations: 1 };

    for (name, sync) in [("hub_sync", true), ("hub_rebuild", false)] {
        group.bench_with_input(BenchmarkId::new(name, "256+16"), &sync, |b, &sync| {
            b.iter(|| {
                let base = BenchCase::hub(256);
                let mut engine = ForceAtlas2::default();
                let mut settled = base.state.clone();
                engine.rebuild(&base.graph, &mut settled);
                let mut iters = 0u32;
                while iters < SETTLE_ITERATION_CAP
                    && engine.step(&base.graph, &mut settled, budget) != LayoutProgress::Settled
                {
                    iters += 1;
                }

                // Grow the star by 16 leaves; carried nodes keep their
                // indices because the builder appends new leaves last.
                let grown = BenchCase::hub(272);
                let mut grown_state = grown.state.clone();
                let carried = settled.positions.len();
                grown_state.positions[..carried].copy_from_slice(&settled.positions);
                apply_growth(&mut engine, &grown.graph, &mut grown_state, carried, sync);
                let mut iters = 0u32;
                while iters < SETTLE_ITERATION_CAP
                    && engine.step(&grown.graph, &mut grown_state, budget)
                        != LayoutProgress::Settled
                {
                    iters += 1;
                }
                std::hint::black_box(iters);
            })
        });
    }

    // Stiff-equilibrium variant: a settled grid gains one extra row. Global
    // reheat over a stiff equilibrium is exactly where carrying adapted
    // factors should diverge from a full rebuild.
    for (name, sync) in [("grid_sync", true), ("grid_rebuild", false)] {
        group.bench_with_input(BenchmarkId::new(name, "20x20+20"), &sync, |b, &sync| {
            b.iter(|| {
                let base = BenchCase::grid(20);
                let mut engine = ForceAtlas2::default();
                let mut settled = base.state.clone();
                engine.rebuild(&base.graph, &mut settled);
                let mut iters = 0u32;
                while iters < SETTLE_ITERATION_CAP
                    && engine.step(&base.graph, &mut settled, budget) != LayoutProgress::Settled
                {
                    iters += 1;
                }

                // The builder appends the extra row last: first 400 indices
                // are carried unchanged.
                let grown = BenchCase::grid(21);
                let mut grown_state = grown.state.clone();
                let carried = settled.positions.len();
                grown_state.positions[..carried].copy_from_slice(&settled.positions);
                apply_growth(&mut engine, &grown.graph, &mut grown_state, carried, sync);

                let mut iters = 0u32;
                while iters < SETTLE_ITERATION_CAP
                    && engine.step(&grown.graph, &mut grown_state, budget)
                        != LayoutProgress::Settled
                {
                    iters += 1;
                }
                std::hint::black_box(iters);
            })
        });
    }
    group.finish();
}

/// Carry `carried` leading indices through the sync or the rebuild path;
/// remaining graph nodes are treated as appended fresh ones.
fn apply_growth(
    engine: &mut ForceAtlas2,
    graph: &LayoutGraph,
    state: &mut LayoutState,
    carried: usize,
    sync: bool,
) {
    if sync {
        let fresh = graph.node_count() - carried;
        let remap: Vec<Option<u32>> = (0..carried as u32)
            .map(Some)
            .chain(std::iter::repeat_n(None, fresh))
            .collect();
        assert_eq!(
            engine.apply_delta(graph, &LayoutDelta { remap }, state),
            LayoutSync::Applied
        );
    } else {
        engine.rebuild(graph, state);
    }
}

fn bench_force_atlas2_settle(c: &mut Criterion) {
    let mut group = c.benchmark_group("force_atlas2_settle");
    group.sample_size(10);
    settle_bench(
        &mut group,
        BenchmarkId::new("grid", "20x20"),
        &BenchCase::grid(20),
    );
    settle_bench(
        &mut group,
        BenchmarkId::new("grid", "40x40"),
        &BenchCase::grid(40),
    );
    settle_bench(
        &mut group,
        BenchmarkId::new("hub", "256"),
        &BenchCase::hub(256),
    );
    settle_bench(
        &mut group,
        BenchmarkId::new("hub", "1024"),
        &BenchCase::hub(1024),
    );
    group.finish();
}

criterion_group!(
    benches,
    bench_force_atlas2_step,
    bench_force_atlas2_settle,
    bench_force_atlas2_dynamic
);
criterion_main!(benches);
