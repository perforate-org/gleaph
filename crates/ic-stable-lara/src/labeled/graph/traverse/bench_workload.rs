//! Plan 0336 interleaved workload harness and candidate C prototype.
//!
//! Produces the integrated-crossover evidence for GAP-2026-07-25-002
//! (`plans/0336-tombstone-offset-reactive-crossover.md`): candidate C — an
//! ephemeral traversal tombstone-pressure counter that triggers the existing
//! bucket compaction work-item drain at the mutation boundary — measured
//! against the D status quo over the Plan 0283 87.5%-tombstone fixture shape,
//! with the decision function fixed in the plan (ε = 0.10,
//! `I_restored = 27,095`).
//!
//! Declared before any measurement run (Plan 0336 todo `c-prototype`): the C
//! trigger threshold is `C_TRIGGER_THRESHOLD` accumulated
//! tombstone-observations. It is derived from the FIXED Plan 0283 anchors —
//! one targeted compaction pays for
//! `831,216,051 / (872,634 − 27,095) ≈ 983` window scans of overshoot, and
//! the fixture's walked region passes ≈ 7,200 tombstones per scan, so the
//! threshold is one compaction's worth of scan overshoot. The threshold is
//! not retuned after seeing numbers.
//!
//! Workload shape (fixed with the threshold): every mutation removes one of
//! the 120 live rows that sit BELOW the measured window (slots 0..952,
//! stride 8) and reinserts the same target under the Insertion policy, which
//! appends to the bucket-local live suffix (slab tail growth, overflow log
//! when the span cannot grow). The four window rows (slots 960..984) are
//! never mutation targets, so under D (which never compacts this bucket —
//! the current policy only admits maintenance on PMA-dense leaves) the
//! window content is constant and the walked region grows by one tombstone
//! per first-pass mutation only.
//!
//! Bench-scope only (`canbench` feature). Production `visit_edges*`, the
//! maintenance drain, and the compaction code are the exact production
//! implementations; the only new logic is the C counter/trigger prototype
//! below and the workload driver.

use super::bench::{
    BenchEdge, OFFSET_WINDOW_LIMIT, OFFSET_WINDOW_OFFSET, OFFSET_WORKLOAD_EXTENT,
    OFFSET_WORKLOAD_STRIDE, workload_target,
};
use crate::labeled::{
    BucketLabelKey, DeferredBidirectionalLabeledLaraGraph, LabeledVertex, OutEdgeOrder,
    graph::EdgePlacementPolicy,
    graph::traverse::{BucketEntryPosition, LabeledTraversalRequest},
};
use crate::traits::{CsrEdge, CsrEdgeTombstone};
use crate::traverse::{Traversal, TraversalWindow};
use crate::{MaintenanceBudget, VertexId};
use canbench_rs::{bench, bench_fn};
use std::hint::black_box;

/// C trigger threshold in accumulated tombstone-observations (declared above):
/// `983` break-even scans at the fixture's ≈ 868-tombstone walked region.
const C_TRIGGER_THRESHOLD: u64 = 853_000;

/// Loop length for every churn configuration. Sized so the declared
/// threshold fires at most a few times per loop while staying inside the
/// repository long-running-validation budget.
const WORKLOAD_STEPS: u32 = 2_048;

/// Query phase length for churn-light (q ≈ 102) and burst burst spacing.
const INTERLEAVE_PHASE: u32 = 100;

/// Bursty batch size per burst (2 bursts per loop; 160 mutations total).
const BURST_SIZE: u32 = 80;

/// Uniform churn period: one mutation every `UNIFORM_PERIOD` steps
/// (≈ 158 mutations per loop, q ≈ 13).
const UNIFORM_PERIOD: u32 = 13;

/// Churn axis: queries per mutation in the closed loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Churn {
    /// Query-heavy: one mutation after every 100 queries.
    ChurnLight,
    /// Bursty batch: 100 mutations in one burst after every 100 queries
    /// (Gleaph batch-mutation shape).
    Bursty,
    /// Uniform: one mutation per query.
    Uniform,
}

impl Churn {
    fn name(self) -> &'static str {
        match self {
            Churn::ChurnLight => "churn_light",
            Churn::Bursty => "bursty",
            Churn::Uniform => "uniform",
        }
    }
}

/// Sentinel for "this target has no live row right now".
const TARGET_ABSENT: u32 = u32::MAX;

/// Exact logical-slot model of the fixture bucket.
///
/// Logical slots are the production `BucketEntryPosition` space: slab slots
/// `[0, stored_slots)` followed by overflow-log chain positions. The model is
/// the parity authority for the window assertions and the source of the
/// mutation target addresses.
struct WorkloadModel {
    /// Logical slot -> live target (`None` = tombstone or absent).
    pos_target: Vec<Option<u32>>,
    /// Target value -> live logical slot ([`TARGET_ABSENT`] when tombstoned).
    target_pos: Vec<u32>,
    /// Slab extent at model-rebuild time (log positions start here).
    slab_extent: u32,
    /// Fixed mutation cycle: the live rows below the measured window.
    cycle: Vec<u32>,
    cycle_pos: usize,
}

impl WorkloadModel {
    fn expected_window(&self) -> Vec<(BucketEntryPosition, u32)> {
        let mut rows = Vec::new();
        for slot in OFFSET_WINDOW_OFFSET..OFFSET_WINDOW_OFFSET + OFFSET_WINDOW_LIMIT {
            if let Some(target) = self.pos_target.get(slot as usize).copied().flatten() {
                rows.push((BucketEntryPosition::new(slot), target));
            }
        }
        rows
    }

    fn checksum(rows: &[(BucketEntryPosition, u32)]) -> u64 {
        rows.iter().fold(0u64, |acc, (slot, target)| {
            acc.wrapping_add(u64::from(slot.raw()).wrapping_mul(31))
                .wrapping_add(u64::from(*target))
        })
    }

    /// Tombstones in the walked region `[0, offset + limit)` — the count a
    /// production in-walk counter would accumulate for one scan.
    fn walked_tombstones(&self) -> u64 {
        let walked = (OFFSET_WINDOW_OFFSET + OFFSET_WINDOW_LIMIT) as usize;
        self.pos_target[..walked.min(self.pos_target.len())]
            .iter()
            .filter(|entry| entry.is_none())
            .count() as u64
    }

    fn target_index(&self, target: u32) -> usize {
        assert!(
            target >= workload_target(0)
                && usize::try_from(target - workload_target(0))
                    .map(|idx| idx < OFFSET_WORKLOAD_EXTENT as usize)
                    .unwrap_or(false),
            "model target out of fixture range: {target}"
        );
        (target - workload_target(0)) as usize
    }
}

struct WorkloadFixture {
    graph: DeferredBidirectionalLabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
    model: WorkloadModel,
}

/// Fixture-wide leaf index for overflow-log reads (one leaf, one vertex).
fn leaf_for_fixture() -> u32 {
    0
}

impl WorkloadFixture {
    fn forward(&self) -> &crate::labeled::graph::LabeledLaraGraph<BenchEdge, crate::VectorMemory> {
        self.graph.forward()
    }

    fn bucket(&self) -> crate::labeled::record::LabelBucket {
        let graph = self.forward();
        let vertex = graph.vertices().get(self.src);
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree, 1, "fixture must contain one label bucket");
        graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .expect("fixture label bucket")
    }

    /// Builds the deferred graph and the 87.5% fixture through the same
    /// constructor sequence as the 0283 family (dense insert, build
    /// compaction, stride-8 removal), then derives the exact model.
    fn build() -> Self {
        let graph = workload_deferred_graph();
        let src = graph
            .forward()
            .push_vertex(LabeledVertex::default())
            .expect("workload fixture vertex");
        let label = BucketLabelKey::from_raw(2);
        for slot in 0..OFFSET_WORKLOAD_EXTENT {
            graph
                .forward()
                .insert_edge(
                    src,
                    label,
                    BenchEdge(workload_target(slot)),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("workload fixture insert");
        }
        graph
            .forward()
            .compact_vertex_edge_span(src, 0)
            .expect("workload fixture build compaction");
        for slot in 0..OFFSET_WORKLOAD_EXTENT {
            if slot % OFFSET_WORKLOAD_STRIDE != 0 {
                assert!(
                    graph
                        .forward()
                        .remove_edge_at_slot(src, label, slot)
                        .expect("workload fixture remove")
                        .is_some()
                );
            }
        }
        let mut fixture = Self {
            graph,
            src,
            label,
            model: WorkloadModel {
                pos_target: Vec::new(),
                target_pos: Vec::new(),
                slab_extent: 0,
                cycle: Vec::new(),
                cycle_pos: 0,
            },
        };
        fixture.rebuild_model();
        fixture.model.cycle = (0..960u32)
            .step_by(OFFSET_WORKLOAD_STRIDE as usize)
            .map(workload_target)
            .collect();
        assert_eq!(fixture.model.cycle.len(), 120);
        fixture
    }

    /// Rebuilds the model from the graph: one O(slab) pass plus one
    /// overflow-log chain walk. Used at build time and after every C trigger
    /// drain (compaction re-packs live rows and folds the log, so
    /// incremental maps would be stale).
    fn rebuild_model(&mut self) {
        let bucket = self.bucket();
        let extent = bucket.stored_slots;
        let edge_start = bucket.edge_start();
        let mut pos_target: Vec<Option<u32>> = vec![None; extent as usize];
        let mut target_pos = vec![TARGET_ABSENT; OFFSET_WORKLOAD_EXTENT as usize];
        let mut live = 0u32;
        for slot in 0..extent {
            let edge = self
                .forward()
                .edges()
                .read_slot(edge_start + u64::from(slot));
            if edge.is_tombstone_edge() || edge.is_deleted_slot() {
                continue;
            }
            let target = edge.0;
            let idx = self.model.target_index(target);
            pos_target[slot as usize] = Some(target);
            target_pos[idx] = slot;
            live += 1;
        }
        // Overflow-log suffix: logical slots `[extent, extent + chain_len)`
        // follow the production replay order (oldest chain entry first; the
        // head is the newest). Tombstoned log entries consume logical-slot
        // space exactly as the production slot-order replay does.
        let leaf = leaf_for_fixture();
        let mut chain_entries: Vec<(u32, BenchEdge)> = Vec::new();
        let mut chain_cur = bucket.overflow_log_head();
        while chain_cur >= 0 {
            let (prev, edge) = self
                .forward()
                .edges()
                .read_overflow_log_entry(leaf, chain_cur as u32);
            chain_entries.push((chain_cur as u32, edge));
            chain_cur = prev;
        }
        chain_entries.reverse();
        let mut logical = extent;
        for (_, edge) in chain_entries {
            if edge.is_tombstone_edge() || edge.is_deleted_slot() {
                logical += 1;
                continue;
            }
            let target = edge.0;
            let idx = self.model.target_index(target);
            if pos_target.len() <= logical as usize {
                pos_target.resize(logical as usize + 1, None);
            }
            pos_target[logical as usize] = Some(target);
            target_pos[idx] = logical;
            live += 1;
            logical += 1;
        }
        assert_eq!(live, bucket.degree);
        self.model.pos_target = pos_target;
        self.model.target_pos = target_pos;
        self.model.slab_extent = extent;
    }

    /// One remove/reinsert mutation at the next cycle target: removes the
    /// row through the production forward-half removal (bounded survivor
    /// shifts surfaced as `EdgeRemoval.moves`) and reinserts the same target
    /// under the Insertion policy.
    fn mutate(&mut self) {
        let target = self.model.cycle[self.model.cycle_pos % self.model.cycle.len()];
        self.model.cycle_pos += 1;
        let idx = self.model.target_index(target);
        let slot = self.model.target_pos[idx];
        assert_ne!(slot, TARGET_ABSENT, "cycle target must be live");
        let removal = self
            .forward()
            .remove_edge_at_slot_with_move(self.src, self.label, slot)
            .expect("workload remove");
        let removed = removal.expect("workload remove finds its row");
        assert_eq!(removed.removed.0, target);
        self.model.pos_target[slot as usize] = None;
        self.model.target_pos[idx] = TARGET_ABSENT;
        for mv in removed.moves {
            let moved_target = self.model.pos_target[mv.old_slot_index as usize]
                .expect("moved row must be modeled");
            let moved_idx = self.model.target_index(moved_target);
            if self.model.pos_target.len() <= mv.new_slot_index as usize {
                self.model
                    .pos_target
                    .resize(mv.new_slot_index as usize + 1, None);
            }
            self.model.pos_target[mv.new_slot_index as usize] = Some(moved_target);
            self.model.pos_target[mv.old_slot_index as usize] = None;
            self.model.target_pos[moved_idx] = mv.new_slot_index;
        }
        let location = self
            .forward()
            .insert_edge_skip_leaf_cascade_with_location(
                self.src,
                self.label,
                BenchEdge(target),
                EdgePlacementPolicy::Insertion,
            )
            .expect("workload reinsert")
            .expect("insertion append captures a location");
        let new_slot = location.logical_slot;
        if self.model.pos_target.len() <= new_slot as usize {
            self.model.pos_target.resize(new_slot as usize + 1, None);
        }
        self.model.pos_target[new_slot as usize] = Some(target);
        self.model.target_pos[idx] = new_slot;
    }

    fn request(&self) -> LabeledTraversalRequest {
        LabeledTraversalRequest {
            owner: self.src,
            label: self.label,
            order: OutEdgeOrder::Ascending,
        }
    }

    fn window(&self) -> TraversalWindow {
        TraversalWindow::new(OFFSET_WINDOW_OFFSET, Some(OFFSET_WINDOW_LIMIT))
    }

    /// The exact measured-visitor shape of the 0283 family (fixed count,
    /// slot, target, checksum, `black_box`; no allocation).
    fn scan(&self) -> (u32, u64) {
        let mut count = 0u32;
        let mut checksum = 0u64;
        let result = Traversal::visit_edges_window(
            self.forward(),
            &self.request(),
            self.window(),
            |slot, edge| {
                count += 1;
                checksum = checksum
                    .wrapping_add(u64::from(slot.raw()).wrapping_mul(31))
                    .wrapping_add(u64::from(edge.0));
                black_box((slot.raw(), edge.0));
                std::ops::ControlFlow::<()>::Continue(())
            },
        )
        .expect("workload window traversal");
        assert!(result.is_continue());
        (count, checksum)
    }

    /// Per-step window parity: exact count and checksum against the model.
    /// Runs inside the measured loop identically for the D and C arms.
    fn assert_step_parity(&self, count: u32, checksum: u64) {
        let expected = self.model.expected_window();
        assert_eq!(count as usize, expected.len());
        assert_eq!(checksum, WorkloadModel::checksum(&expected));
    }

    /// Full 0283-style preflight/postflight: production window rows vs the
    /// model in exact order, plus the boundary windows 31/32/33 and the
    /// zero-limit window.
    fn assert_full_parity(&self) {
        let mut rows = Vec::new();
        let result = Traversal::visit_edges_window(
            self.forward(),
            &self.request(),
            self.window(),
            |slot, edge| {
                rows.push((slot, edge.0));
                std::ops::ControlFlow::<()>::Continue(())
            },
        )
        .expect("parity window traversal");
        assert!(result.is_continue());
        assert_eq!(rows, self.model.expected_window());
        assert!(!rows.is_empty());
        assert!(
            rows.iter()
                .all(|(_, target)| *target != u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
        );
        for boundary in [31u32, 32, 33] {
            let expected = self
                .model
                .pos_target
                .get(boundary as usize)
                .copied()
                .flatten()
                .map(|target| vec![(BucketEntryPosition::new(boundary), target)])
                .unwrap_or_default();
            let mut rows = Vec::new();
            let result = Traversal::visit_edges_window(
                self.forward(),
                &self.request(),
                TraversalWindow::new(boundary, Some(1)),
                |slot, edge| {
                    rows.push((slot, edge.0));
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .expect("boundary window traversal");
            assert!(result.is_continue());
            assert_eq!(rows, expected);
        }
        let mut zero_limit_calls = 0u32;
        let result = Traversal::visit_edges_window(
            self.forward(),
            &self.request(),
            TraversalWindow::new(0, Some(0)),
            |_slot, _edge| {
                zero_limit_calls += 1;
                std::ops::ControlFlow::<()>::Continue(())
            },
        )
        .expect("zero-limit window");
        assert!(result.is_continue());
        assert_eq!(zero_limit_calls, 0);
    }
}

/// Fresh deferred bidirectional graph with the 0283 fixture's capacity and
/// label conventions.
fn workload_deferred_graph() -> DeferredBidirectionalLabeledLaraGraph<BenchEdge, crate::VectorMemory>
{
    let (
        fv,
        fb,
        fbf,
        fbfs,
        fec,
        fe,
        fel,
        fsm,
        fefs,
        fefbs,
        fvs,
        fpfs,
        fpfbs,
        fpl,
        fpb,
        forward_ltb,
    ) = crate::test_support::labeled_lara_memories();
    let (
        rv,
        rb,
        rbf,
        rbfs,
        rec,
        re,
        rel,
        rsm,
        refs,
        refbs,
        rvs,
        rpfs,
        rpfbs,
        rpl,
        rpb,
        reverse_ltb,
    ) = crate::test_support::labeled_lara_memories();
    DeferredBidirectionalLabeledLaraGraph::new(
        fv,
        fb,
        fbf,
        fbfs,
        fec,
        fe,
        fel,
        fsm,
        fefs,
        fefbs,
        fvs,
        fpfs,
        fpfbs,
        fpl,
        fpb,
        forward_ltb,
        rv,
        rb,
        rbf,
        rbfs,
        rec,
        re,
        rel,
        rsm,
        refs,
        refbs,
        rvs,
        rpfs,
        rpfbs,
        rpl,
        rpb,
        reverse_ltb,
        crate::VectorMemory::default(),
        crate::labeled::InitialCapacities::uniform(32_768),
        BucketLabelKey::from_raw(1),
    )
    .expect("workload deferred graph")
}

/// Production maintenance drain in the 0283 audit shape: one work item per
/// call with checkpoint 1, until the queue is empty. Returns the call count.
fn drain_maintenance(
    graph: &DeferredBidirectionalLabeledLaraGraph<BenchEdge, crate::VectorMemory>,
) -> u32 {
    let budget = MaintenanceBudget {
        max_instructions: 5_000_000,
        reserve_instructions: 0,
        checkpoint_every: 1,
        max_work_items: Some(1),
        max_segments: Some(1),
        max_delete_edge_steps: Some(1),
    };
    let mut calls = 0u32;
    while graph.maintenance_queue_len() > 0 {
        let report = graph.maintenance(budget).expect("workload maintenance");
        calls += 1;
        assert_eq!(report.work.processed_work_items, 1);
        assert_eq!(
            report.work.remaining_queue_len,
            graph.maintenance_queue_len()
        );
    }
    calls
}

/// Candidate C prototype state (bench-scope): the ephemeral in-heap
/// tombstone-pressure counter and the mutation-boundary trigger. Holds no
/// durable state; the compaction it invokes is the exact production
/// `mark_compact_vertex_edge_span` enqueue + work-item drain.
struct PressureCounter {
    observed: u64,
    fired: u32,
}

impl PressureCounter {
    fn new() -> Self {
        Self {
            observed: 0,
            fired: 0,
        }
    }

    /// Prototype scan leg: the production sparse window scan plus the
    /// tombstone-observation accumulation over the walked region.
    fn scan_and_count(&mut self, fixture: &WorkloadFixture) -> (u32, u64) {
        let (count, checksum) = fixture.scan();
        self.observed += fixture.model.walked_tombstones();
        (count, checksum)
    }

    /// Mutation-boundary trigger: fires one targeted bucket compaction and
    /// resets the counter when accumulated pressure crosses the declared
    /// threshold. Mirrors the production trigger precedents (tree-mode
    /// demotion check after a successful remove; span-compaction enqueue).
    fn maybe_trigger(&mut self, fixture: &mut WorkloadFixture) {
        if self.observed < C_TRIGGER_THRESHOLD {
            return;
        }
        fixture
            .graph
            .mark_compact_vertex_edge_span(
                crate::labeled::LabeledOrientation::Forward,
                fixture.src,
                0,
                &|_| EdgePlacementPolicy::Insertion,
            )
            .expect("trigger enqueue");
        drain_maintenance(&fixture.graph);
        fixture.rebuild_model();
        self.observed = 0;
        self.fired += 1;
    }
}

/// Per-leaf overflow-log table capacity (`DEFAULT_MAX_LOG_ENTRIES`): every
/// mutation consumes one log table slot on its first reinsert, and the table
/// counter never reverts without a log fold. The loop mutation volumes below
/// stay under this cap so the workload exercises the pre-fold regime
/// uniformly across all churn points (see the plan report for the measured
/// fold-path behavior at saturation).
const LOG_TABLE_CAPACITY: u32 = 170;

/// Mutation schedule shared by the D and C loops: per-step mutation flags.
/// Churn-light: one mutation after every `INTERLEAVE_PHASE` query steps.
/// Bursty: two `BURST`-sized mutation bursts after query phases. Uniform:
/// one mutation every `UNIFORM_PERIOD` steps. Total mutations per loop are
/// capped below the per-leaf overflow-log table capacity.
fn mutation_schedule(churn: Churn) -> Vec<bool> {
    let mut schedule = vec![false; WORKLOAD_STEPS as usize];
    match churn {
        Churn::Uniform => {
            for step in (0..WORKLOAD_STEPS).step_by(UNIFORM_PERIOD as usize) {
                schedule[step as usize] = true;
            }
        }
        Churn::ChurnLight => {
            for phase in 1..=(WORKLOAD_STEPS / INTERLEAVE_PHASE) {
                schedule[(phase * INTERLEAVE_PHASE - 1) as usize] = true;
            }
        }
        Churn::Bursty => {
            for burst in 0..2u32 {
                let start = burst * (WORKLOAD_STEPS / 2) + INTERLEAVE_PHASE;
                for step in start..start + BURST_SIZE {
                    schedule[step as usize] = true;
                }
            }
        }
    }
    schedule
}

fn mutation_count(churn: Churn) -> u32 {
    mutation_schedule(churn)
        .iter()
        .filter(|step| **step)
        .count() as u32
}

/// D loop (status quo): interleaved window queries and mutations under the
/// current production policy. Any maintenance the policy admits is drained
/// when the queue is non-empty (the production timer-drain analog). No
/// counters, no trigger. Returns the drain call count for the report.
fn workload_baseline_loop(fixture: &mut WorkloadFixture, churn: Churn) -> u32 {
    let mut drain_calls = 0u32;
    for mutate_step in mutation_schedule(churn) {
        let (count, checksum) = fixture.scan();
        fixture.assert_step_parity(count, checksum);
        if fixture.graph.maintenance_queue_len() > 0 {
            drain_calls += drain_maintenance(&fixture.graph);
        }
        if mutate_step {
            fixture.mutate();
        }
    }
    drain_calls
}

/// C loop: identical schedule and identical visitor/assertion work, plus the
/// prototype pressure counter and the mutation-boundary trigger.
fn workload_candidate_loop(fixture: &mut WorkloadFixture, churn: Churn) -> PressureCounter {
    let mut counter = PressureCounter::new();
    for mutate_step in mutation_schedule(churn) {
        let (count, checksum) = counter.scan_and_count(fixture);
        fixture.assert_step_parity(count, checksum);
        if fixture.graph.maintenance_queue_len() > 0 {
            drain_maintenance(&fixture.graph);
        }
        if mutate_step {
            fixture.mutate();
            counter.maybe_trigger(fixture);
        }
    }
    counter
}

/// Shared preflight: builds the fixture and asserts full window parity.
fn preflight() -> WorkloadFixture {
    let fixture = WorkloadFixture::build();
    fixture.assert_full_parity();
    fixture
}

/// Shared postflight: full parity plus the fixture invariants (1,024 live
/// rows, exact mutation count consumed from the cycle).
fn postflight(fixture: &WorkloadFixture, churn: Churn) {
    fixture.assert_full_parity();
    assert_eq!(fixture.bucket().degree, 1_024);
    assert_eq!(fixture.model.cycle_pos, mutation_count(churn) as usize);
}

// ---------------------------------------------------------------------------
// Benchmarks — canbench group `offset_workload`
// ---------------------------------------------------------------------------

/// D baseline at the churn-light point (q ≈ 102). Reports I_D(W).
#[bench(raw)]
fn offset_workload_baseline_churn_light() -> canbench_rs::BenchResult {
    let churn = Churn::ChurnLight;
    let mut fixture = preflight();
    assert_eq!(mutation_count(churn), 20);
    let result = bench_fn(|| {
        black_box(workload_baseline_loop(&mut fixture, churn));
    });
    postflight(&fixture, churn);
    result
}

/// C candidate at the churn-light point. Reports I_C(W).
#[bench(raw)]
fn offset_workload_candidate_churn_light() -> canbench_rs::BenchResult {
    let churn = Churn::ChurnLight;
    let mut fixture = preflight();
    let result = bench_fn(|| {
        let counter = workload_candidate_loop(&mut fixture, churn);
        black_box((counter.fired, counter.observed));
    });
    postflight(&fixture, churn);
    result
}

/// D baseline at the bursty point (s = 100 mutation bursts). Reports I_D(W).
#[bench(raw)]
fn offset_workload_baseline_bursty() -> canbench_rs::BenchResult {
    let churn = Churn::Bursty;
    let mut fixture = preflight();
    assert_eq!(mutation_count(churn), 160);
    let result = bench_fn(|| {
        black_box(workload_baseline_loop(&mut fixture, churn));
    });
    postflight(&fixture, churn);
    result
}

/// C candidate at the bursty point. Reports I_C(W).
#[bench(raw)]
fn offset_workload_candidate_bursty() -> canbench_rs::BenchResult {
    let churn = Churn::Bursty;
    let mut fixture = preflight();
    let result = bench_fn(|| {
        let counter = workload_candidate_loop(&mut fixture, churn);
        black_box((counter.fired, counter.observed));
    });
    postflight(&fixture, churn);
    result
}

/// D baseline at the uniform point (q ≈ 1). Reports I_D(W).
#[bench(raw)]
fn offset_workload_baseline_uniform() -> canbench_rs::BenchResult {
    let churn = Churn::Uniform;
    let mut fixture = preflight();
    assert_eq!(mutation_count(churn), 158);
    let result = bench_fn(|| {
        black_box(workload_baseline_loop(&mut fixture, churn));
    });
    postflight(&fixture, churn);
    result
}

/// C candidate at the uniform point. Reports I_C(W).
#[bench(raw)]
fn offset_workload_candidate_uniform() -> canbench_rs::BenchResult {
    let churn = Churn::Uniform;
    let mut fixture = preflight();
    let result = bench_fn(|| {
        let counter = workload_candidate_loop(&mut fixture, churn);
        black_box((counter.fired, counter.observed));
    });
    postflight(&fixture, churn);
    result
}

/// Isolated targeted compaction cost (`I_compact_local`): one
/// pressure-triggered targeted bucket compaction under tombstone pressure,
/// measured on the fixture's full tombstone population plus 64 fresh
/// workload mutations.
#[bench(raw)]
fn offset_workload_targeted_compaction() -> canbench_rs::BenchResult {
    let mut fixture = preflight();
    for _ in 0..64 {
        fixture.mutate();
    }
    let before = fixture.bucket().stored_slots;
    let result = bench_fn(|| {
        fixture
            .graph
            .mark_compact_vertex_edge_span(
                crate::labeled::LabeledOrientation::Forward,
                fixture.src,
                0,
                &|_| EdgePlacementPolicy::Insertion,
            )
            .expect("targeted compaction enqueue");
        black_box(drain_maintenance(&fixture.graph));
    });
    let after = fixture.bucket().stored_slots;
    assert!(
        after < before,
        "targeted compaction must reclaim: {before} -> {after}"
    );
    fixture.rebuild_model();
    fixture.assert_full_parity();
    result
}

/// Per-query counter delta: one sparse window scan with the prototype
/// tombstone-observation accumulation. The delta against
/// [`offset_workload_scan_only`] is `I_counter_delta`.
#[bench(raw)]
fn offset_workload_counter_scan() -> canbench_rs::BenchResult {
    let fixture = preflight();
    let mut counter = PressureCounter::new();
    let result = bench_fn(|| {
        let (count, checksum) = counter.scan_and_count(&fixture);
        fixture.assert_step_parity(count, checksum);
        black_box(counter.observed);
    });
    assert_eq!(
        counter.observed,
        fixture.model.walked_tombstones(),
        "one scan accumulates exactly one walked region"
    );
    result
}

/// Production-scan-only control for the counter delta: one sparse window
/// scan with the exact measured visitor and no counter work.
#[bench(raw)]
fn offset_workload_scan_only() -> canbench_rs::BenchResult {
    let fixture = preflight();
    bench_fn(|| {
        let (count, checksum) = fixture.scan();
        fixture.assert_step_parity(count, checksum);
    })
}

/// Restored-query control: the production sparse window scan against the
/// fixture bucket after one full targeted compaction. Context measurement
/// for the overshoot decomposition (the decision function's `I_restored`
/// anchor stays fixed at the Plan 0283 value).
#[bench(raw)]
fn offset_workload_restored_query() -> canbench_rs::BenchResult {
    let mut fixture = preflight();
    fixture
        .graph
        .mark_compact_vertex_edge_span(
            crate::labeled::LabeledOrientation::Forward,
            fixture.src,
            0,
            &|_| EdgePlacementPolicy::Insertion,
        )
        .expect("restore compaction enqueue");
    drain_maintenance(&fixture.graph);
    fixture.rebuild_model();
    let bucket = fixture.bucket();
    assert_eq!(bucket.stored_slots, bucket.degree, "fully compacted");
    fixture.assert_full_parity();
    bench_fn(|| {
        let (count, checksum) = fixture.scan();
        fixture.assert_step_parity(count, checksum);
    })
}
