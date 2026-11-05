//! Current-only fold economics. No automatic maintenance policy is changed.
//! Each width/degree compares unfolded, values-only-folded and both-folded states.
//! Initial reads precede eight appends and a final values scan. Maintenance, reads and
//! appends have separate scopes; setup, exact-result checks and observations are untimed.
//! The repeated series starts all arms unfolded and charges eight append/fold/read episodes.
//! The continuous-spill series inserts a retained one-row peer before each target episode.
//! The update-reuse series adds a growing consumer whose ordinary updates fold its values.

use super::{InlinePropertyBenchEdge, RangeBenchEdge};
use crate::{
    VertexId,
    labeled::{
        BucketLabelKey, InitialCapacities, LabelBucket, LabeledInlinePropertyValueBatchScratch,
        LabeledVertex, OutEdgeOrder,
        graph::{EdgePlacementPolicy, EdgeSlotMove, LabeledLaraGraph},
        spill_run_store::{ArenaRows, RUN_HEADER_ROWS, SpillRunStore},
    },
    lara::edge_inline_property::InlinePropertyBytesLogCell,
    traits::{CsrEdge, CsrVertex, NO_SPILL_RUN},
};
use canbench_rs::{BenchResult, bench, bench_fn, bench_scope};
use ic_stable_structures::{
    DefaultMemoryImpl, Memory,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
};
use std::{hint::black_box, ops::ControlFlow};

type BenchMemory = VirtualMemory<DefaultMemoryImpl>;
const LABEL: BucketLabelKey = BucketLabelKey::from_raw(2);
const PEER: BucketLabelKey = BucketLabelKey::from_raw(3);
const CONSUMER: BucketLabelKey = BucketLabelKey::from_raw(12);
const APPENDS: u32 = 8;
const ROUNDS: [&str; 8] = [
    "round_1", "round_2", "round_3", "round_4", "round_5", "round_6", "round_7", "round_8",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepeatedFold {
    Unfolded,
    Values,
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValueFoldPeriod {
    Never = 0,
    Each = 1,
    Four = 4,
    Eight = 8,
}

impl ValueFoldPeriod {
    fn due(self, round: u32) -> bool {
        self != Self::Never && round.is_multiple_of(self as u32)
    }
}

struct FoldFixture<const W: usize> {
    graph: LabeledLaraGraph<RangeBenchEdge<W>, BenchMemory>,
    src: VertexId,
    degree: u32,
    peers: Vec<(BucketLabelKey, u32)>,
    regions: [BenchMemory; 16],
    physical: DefaultMemoryImpl,
}

impl<const W: usize> FoldFixture<W> {
    fn new(degree: u32) -> Self {
        // Retain the physical backing too: sum(VirtualMemory::size) is a different metric.
        let physical = DefaultMemoryImpl::default();
        let manager = MemoryManager::init(physical.clone());
        let regions = std::array::from_fn(|i| manager.get(MemoryId::new(i as u8)));
        let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = regions.clone();
        let graph = LabeledLaraGraph::new(
            a,
            b,
            c,
            d,
            e,
            f,
            g,
            h,
            i,
            j,
            k,
            l,
            m,
            n,
            o,
            p,
            InitialCapacities::uniform(4096),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .ensure_label_bucket_inline_property_byte_width(src, LABEL, W as u16)
            .unwrap();
        let fixture = Self {
            graph,
            src,
            degree,
            peers: vec![(PEER, 9000)],
            regions,
            physical,
        };
        for slot in 0..degree {
            if slot == 32 {
                fixture
                    .graph
                    .ensure_label_bucket_inline_property_byte_width(src, PEER, W as u16)
                    .unwrap();
                fixture.insert(PEER, 9000);
            }
            fixture.insert(LABEL, slot);
        }
        let (_, bucket) = fixture.bucket();
        assert!(!bucket.is_tree_mode());
        assert_ne!(bucket.edge_spill_run(), NO_SPILL_RUN);
        assert_ne!(bucket.values_spill_run(), NO_SPILL_RUN);
        fixture
    }

    fn insert(&self, label: BucketLabelKey, slot: u32) {
        self.graph
            .insert_edge_skip_leaf_cascade(
                self.src,
                label,
                RangeBenchEdge(InlinePropertyBenchEdge::new(slot + 10, u64::from(slot)), 0),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }

    fn bucket(&self) -> (u64, LabelBucket) {
        self.bucket_for(LABEL)
    }

    fn bucket_for(&self, label: BucketLabelKey) -> (u64, LabelBucket) {
        let vertex = self.graph.vertices().get(self.src);
        let slot = self
            .graph
            .find_bucket_slot(&vertex, label)
            .unwrap()
            .unwrap();
        (
            slot,
            self.graph.buckets().read_label_bucket_slot(slot).unwrap(),
        )
    }

    fn runs(&self) -> [(u32, u32); 2] {
        self.runs_for(LABEL)
    }

    fn runs_for(&self, label: BucketLabelKey) -> [(u32, u32); 2] {
        let (_, bucket) = self.bucket_for(label);
        let edge = bucket.edge_spill_run();
        let values = bucket.values_spill_run();
        [
            (
                edge,
                if edge == NO_SPILL_RUN {
                    0
                } else {
                    self.graph
                        .edges
                        .spill_used_len(self.graph.edges.spill_mint(edge).unwrap())
                },
            ),
            (
                values,
                if values == NO_SPILL_RUN {
                    0
                } else {
                    self.graph
                        .values
                        .spill_used_len(self.graph.values.spill_mint(values).unwrap())
                },
            ),
        ]
    }

    fn fold_edges(&self) -> Vec<EdgeSlotMove> {
        self.graph
            .vertex_layout(self.src)
            .compact_span_with_moves()
            .unwrap()
    }

    /// Folds this bucket's spill run into its span, values included.
    ///
    /// ADR 0099: the values-only fold this used to call is gone - it moved the values plane without
    /// the rows, which is the divergence that ADR removes. The paired fold moves both, so it is what
    /// "folding values" now means.
    fn fold_values(&self) {
        let (slot, bucket) = self.bucket();
        let _ = self
            .graph
            .layout(slot, bucket)
            .fold_head(self.src, u32::MAX)
            .unwrap();
    }

    fn assert_values_folded(&self, before_vertex: LabeledVertex, before_bucket: LabelBucket) {
        let bucket = self.assert_value_only_edges(LABEL, before_vertex, before_bucket);
        assert_eq!(bucket.values_spill_run(), NO_SPILL_RUN);
        assert_eq!(bucket.inline_property_bytes_slab_slots(), self.degree);
        self.assert_rows(self.degree);
    }

    fn assert_value_only_edges(
        &self,
        label: BucketLabelKey,
        before_vertex: LabeledVertex,
        before_bucket: LabelBucket,
    ) -> LabelBucket {
        let vertex = self.graph.vertices().get(self.src);
        let (_, bucket) = self.bucket_for(label);
        // Value allocation accounting may change; the vertex's edge span must not.
        assert_eq!(
            (vertex.base_slot_start(), vertex.stored_slots, vertex.degree),
            (
                before_vertex.base_slot_start(),
                before_vertex.stored_slots,
                before_vertex.degree
            )
        );
        assert_eq!(
            (
                bucket.edge_start(),
                bucket.stored_slots(),
                bucket.edge_spill_run()
            ),
            (
                before_bucket.edge_start(),
                before_bucket.stored_slots(),
                before_bucket.edge_spill_run()
            )
        );
        assert_ne!(bucket.edge_spill_run(), NO_SPILL_RUN);
        bucket
    }

    fn scan_values(&self, scratch: &mut LabeledInlinePropertyValueBatchScratch) -> usize {
        self.scan_bucket_values(LABEL, scratch)
    }

    fn scan_bucket_values(
        &self,
        label: BucketLabelKey,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
    ) -> usize {
        let mut rows = 0;
        self.graph
            .visit_out_inline_property_batches_for_label(
                self.src,
                label,
                OutEdgeOrder::Descending,
                scratch,
                |batch| {
                    black_box((batch.slot_indices, batch.values));
                    rows += batch.slot_indices.len();
                },
            )
            .unwrap();
        rows
    }

    fn assert_rows(&self, degree: u32) {
        for (label, count, value_base) in std::iter::once((LABEL, degree, 0))
            .chain(self.peers.iter().map(|&(label, base)| (label, 1, base)))
        {
            let want: Vec<_> = (0..count)
                .map(|slot| {
                    (
                        slot,
                        slot + value_base + 10,
                        InlinePropertyBenchEdge::<W>::new(0, u64::from(slot + value_base))
                            .inline_property
                            .to_vec(),
                    )
                })
                .collect();
            self.assert_bucket_rows(label, &want);
        }
    }

    fn assert_bucket_rows(&self, label: BucketLabelKey, expected: &[(u32, u32, Vec<u8>)]) {
        for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
            let mut want = expected.to_vec();
            if order == OutEdgeOrder::Descending {
                want.reverse();
            }
            let mut seen = Vec::new();
            let _ = self
                .graph
                .visit_edges_with_inline_property::<()>(self.src, label, order, |slot, item| {
                    seen.push((
                        slot.raw(),
                        item.edge.0.target,
                        item.inline_property.bytes().to_vec(),
                    ));
                    ControlFlow::Continue(())
                })
                .unwrap();
            assert_eq!(seen, want);
            let mut topology = Vec::new();
            let _ = self
                .graph
                .visit_edges(self.src, label, order, |slot, edge| {
                    topology.push((slot.raw(), edge.0.target));
                    ControlFlow::<()>::Continue(())
                })
                .unwrap();
            assert_eq!(
                topology,
                want.iter().map(|(s, t, _)| (*s, *t)).collect::<Vec<_>>()
            );
            let mut values = Vec::new();
            let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
            self.graph
                .visit_out_inline_property_batches_for_label(
                    self.src,
                    label,
                    order,
                    &mut scratch,
                    |batch| {
                        assert_eq!(batch.values.len(), batch.slot_indices.len() * W);
                        values.extend(
                            batch.slot_indices.iter().copied().zip(
                                batch
                                    .values
                                    .as_chunks::<W>()
                                    .0
                                    .iter()
                                    .map(|bytes| bytes.to_vec()),
                            ),
                        );
                    },
                )
                .unwrap();
            assert_eq!(
                values,
                want.into_iter().map(|(s, _, v)| (s, v)).collect::<Vec<_>>()
            );
        }
    }

    fn snapshot(&self, phase: &str) {
        let (_, bucket) = self.bucket();
        let pages = self.regions.each_ref().map(Memory::size);
        let runs = self.runs();
        let run_bytes = |used, width| {
            if used == 0 {
                0
            } else {
                u64::from(SpillRunStore::<BenchMemory>::class_rows(used) + RUN_HEADER_ROWS) * width
            }
        };
        let values = self.graph.values.allocator_stats();
        let edges = self.graph.edges.allocator_stats();
        let record = format!(
            "FOLD_CAP {{\"width\":{W},\"degree\":{},\"phase\":\"{phase}\",\"pages\":{pages:?},\"physical_pages\":{},\"edge_prefix\":{},\"edge_run_used\":{},\"edge_run_bytes\":{},\"value_slab_slots\":{},\"value_run_used\":{},\"value_run_bytes\":{},\"vertex_edge_slots\":{},\"edge_slab_capacity_slots\":{},\"edge_free_slots\":{},\"value_slab_capacity_bytes\":{},\"value_slab_tail_bytes\":{},\"value_free_bytes\":{}}}",
            self.degree,
            self.physical.size(),
            bucket.stored_slots(),
            runs[0].1,
            run_bytes(runs[0].1, RangeBenchEdge::<W>::BYTES as u64),
            bucket.inline_property_bytes_slab_slots(),
            runs[1].1,
            run_bytes(
                runs[1].1,
                InlinePropertyBytesLogCell::EMPTY.as_bytes().len() as u64
            ),
            self.graph.vertices().get(self.src).stored_slots,
            self.graph.edges.header().elem_capacity,
            edges.free_bytes,
            values.byte_capacity,
            values.slab_occupied_tail,
            values.free_bytes,
        );
        #[cfg(target_family = "wasm")]
        ic_cdk::println!("{record}");
        #[cfg(not(target_family = "wasm"))]
        println!("{record}");
    }

    fn arenas(&self) -> [SpillRunStore<BenchMemory>; 2] {
        [
            (6, RangeBenchEdge::<W>::BYTES as u32, ArenaRows::EDGE),
            (
                13,
                InlinePropertyBytesLogCell::EMPTY.as_bytes().len() as u32,
                ArenaRows::VALUES,
            ),
        ]
        .map(|(region, width, bound)| {
            SpillRunStore::new(self.regions[region].clone(), width, bound).unwrap()
        })
    }

    /// Probe only unowned, released runs, through the allocator owner, then release them again.
    /// It establishes class reuse, not shrinking physical stable memory or B-tree heap accounting.
    fn assert_reusable(&self, runs: [(u32, u32); 2]) {
        // Probes must compare against the high-water mark **before** the operation that freed the runs:
        // releasing a run that sits at the arena's tail gives the rows back by shrinking `arena_rows`, so
        // measuring afterwards and demanding it stay put asserts the opposite of a working release.
        self.assert_reusable_against(runs, self.arenas().map(|arena| arena.arena_rows()));
    }

    fn assert_reusable_against(&self, runs: [(u32, u32); 2], pre_fold_rows: [u32; 2]) {
        let pages = self.regions.each_ref().map(Memory::size);
        let physical = self.physical.size();
        // These bounded fixtures have zero in the allocator's touched tail byte.
        // Compare all backing bytes, including free-list links, without decoding its layout.
        let backing = self.arenas().map(|arena| {
            let memory = arena.into_memory();
            let mut bytes = vec![0; (memory.size() * crate::WASM_PAGE_SIZE) as usize];
            memory.read(0, &mut bytes);
            (memory, bytes)
        });
        let arenas = self.arenas();
        for (index, ((id, used), current)) in runs.into_iter().zip(self.runs()).enumerate() {
            let arena = &arenas[index.min(1)];
            if id == NO_SPILL_RUN || current.0 != NO_SPILL_RUN {
                // Retained and already-absent planes must be unchanged; never probe either.
                assert_eq!(current, (id, used));
                continue;
            }
            let reused = arena.allocate(used).unwrap();
            assert!(
                arena.arena_rows() <= pre_fold_rows[index.min(1)],
                "allocating the class of released run {id} grew the arena past its pre-free high-water mark"
            );
            arena.release(reused).unwrap();
        }
        for (memory, before) in backing {
            let mut after = vec![0; before.len()];
            memory.read(0, &mut after);
            assert!(before == after, "run probe must restore every backing byte");
        }
        assert_eq!(self.regions.each_ref().map(Memory::size), pages);
        assert_eq!(self.physical.size(), physical);
        self.assert_rows(self.degree);
    }

    #[cfg(test)]
    fn check_appends(&self, phase: &str) {
        self.assert_rows(self.degree);
        self.snapshot(phase);
        self.insert(LABEL, self.degree);
        self.assert_rows(self.degree + 1);
        self.snapshot(&format!("{phase}_first_append"));
        for slot in self.degree + 1..self.degree + APPENDS {
            self.insert(LABEL, slot);
        }
        self.assert_rows(self.degree + APPENDS);
        self.snapshot(&format!("{phase}_after_appends"));
    }
}

impl<const W: usize> FoldFixture<W> {
    fn append_peer(&mut self, round: u32) {
        #[cfg(target_family = "wasm")]
        let _scope = bench_scope("peer_append");
        let label = BucketLabelKey::from_raw(u16::try_from(3 + round).unwrap());
        let base = 9000 + round;
        self.graph
            .ensure_label_bucket_inline_property_byte_width(self.src, label, W as u16)
            .unwrap();
        self.insert(label, base);
        self.peers.push((label, base));
    }

    fn append_round(&mut self) {
        #[cfg(target_family = "wasm")]
        let _scope = bench_scope("append_round");
        for slot in self.degree..self.degree + APPENDS {
            self.insert(LABEL, slot);
        }
        self.degree += APPENDS;
    }

    fn fold_round(&self, mode: RepeatedFold) -> Vec<EdgeSlotMove> {
        let mut moves = Vec::new();
        if mode == RepeatedFold::Both {
            #[cfg(target_family = "wasm")]
            let _scope = bench_scope("edge_fold");
            moves = self.fold_edges();
        }
        if mode != RepeatedFold::Unfolded {
            #[cfg(target_family = "wasm")]
            let _scope = bench_scope("value_fold");
            self.fold_values();
        }
        moves
    }

    fn scan_round(
        &self,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        reads: u32,
    ) -> usize {
        let mut rows = 0;
        for _ in 0..reads {
            #[cfg(target_family = "wasm")]
            let _scope = bench_scope("values_scan");
            rows += self.scan_values(scratch);
        }
        rows
    }

    fn assert_repeated_state(&self, mode: RepeatedFold) {
        let (_, bucket) = self.bucket();
        let runs = self.runs();
        assert!(!bucket.is_tree_mode());
        if mode == RepeatedFold::Both {
            assert_eq!(runs[0], (NO_SPILL_RUN, 0));
            assert_eq!(bucket.stored_slots(), self.degree);
        } else {
            assert_ne!(runs[0].0, NO_SPILL_RUN);
            assert_eq!(runs[0].1, self.degree);
            assert_eq!(bucket.stored_slots(), 0);
        }
        if mode == RepeatedFold::Unfolded {
            assert_ne!(runs[1].0, NO_SPILL_RUN);
            assert_eq!(
                runs[1].1,
                self.degree - bucket.inline_property_bytes_slab_slots()
            );
        } else {
            assert_eq!(runs[1], (NO_SPILL_RUN, 0));
            // The row count is the geometry that holds under ADR 0099; the byte-span allocator's tail is
            // not (values are addressed by row slot, so it reads zero).
            assert_eq!(bucket.inline_property_bytes_slab_slots(), self.degree);
        }
        self.assert_rows(self.degree);
    }

    fn repeated_snapshot(&self, phase: &str) {
        self.snapshot(phase);
        let (_, bucket) = self.bucket();
        let runs = self.runs().map(|(id, used)| [id, used]);
        let arena_rows = self.arenas().map(|arena| arena.arena_rows());
        let free: Vec<_> = self
            .graph
            .values
            .free_byte_spans()
            .into_iter()
            .map(|span| [span.start_slot, span.len])
            .collect();
        let (leaf_start, leaf_len) = self
            .graph
            .labeled_leaf_physical_range(self.src)
            .unwrap()
            .unwrap();
        let edge_free: Vec<_> = self
            .graph
            .edges
            .free_span_store()
            .spans()
            .into_iter()
            .map(|span| [span.start_slot, span.len])
            .collect();
        let record = format!(
            "FOLD_REUSE {{\"width\":{W},\"degree\":{},\"phase\":\"{phase}\",\"runs\":{runs:?},\"arena_rows\":{arena_rows:?},\"edge_offset\":{},\"edge_leaf\":[{leaf_start},{leaf_len}],\"edge_free_spans\":{edge_free:?},\"value_offset\":{},\"value_free_spans\":{free:?}}}",
            self.degree,
            bucket.edge_start(),
            bucket.inline_property_bytes_offset()
        );
        #[cfg(target_family = "wasm")]
        ic_cdk::println!("{record}");
        #[cfg(not(target_family = "wasm"))]
        println!("{record}");
    }

    fn assert_repeated_reuse(
        &self,
        mode: RepeatedFold,
        initial_bucket: LabelBucket,
        first_fold_runs: [(u32, u32); 2],
    ) {
        // The run-reuse probe runs in `repeated_workload`, right after the first fold frees those runs.
        // Probing here would ask about space the workload has since reused, which the allocator never
        // promised to keep free.
        let _ = first_fold_runs;
        // ADR 0099: the byte-span allocator this checked (`allocate_byte_span`, `slab_occupied_tail`,
        // `free_bytes`) no longer holds the values - they are addressed by row slot - so there is no
        // retired prefix to probe. The reuse probe in `repeated_workload` covers the runs that are freed.
        let _ = (mode, initial_bucket);
    }
}

fn repeated_workload<const W: usize>(degree: u32, reads: u32, mode: RepeatedFold) -> BenchResult {
    let mut fixture = FoldFixture::<W>::new(degree);
    let (_, initial_bucket) = fixture.bucket();
    // Both initial runs are arena tails. The native witness checks that the first
    // append group grows them in place; probe their FIRST-FOLD classes, not initial classes.
    let first_fold_runs = fixture.runs().map(|(id, used)| (id, used + APPENDS));
    let phase = format!("repeat_{mode:?}_r{reads}_n{degree}");
    fixture.assert_rows(degree);
    fixture.repeated_snapshot(&format!("{phase}_before"));
    let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
    assert_eq!(fixture.scan_values(&mut scratch), degree as usize);
    let mut scanned = 0;
    let mut slots_unchanged = true;
    let mut probed_first_fold = false;
    let result = bench_fn(|| {
        for round in ROUNDS {
            let _scope = bench_scope(round);
            fixture.append_round();
            slots_unchanged &= fixture.fold_round(mode).is_empty();
            // Probe the runs the *first* fold freed while they are still the most recent release: later
            // rounds reuse that space by design, so asking at the end of the workload whether a run freed
            // several folds ago is still free is not something the allocator promises.
            if !probed_first_fold {
                fixture.assert_reusable(first_fold_runs);
                probed_first_fold = true;
            }
            scanned += fixture.scan_round(&mut scratch, reads);
        }
    });
    assert!(slots_unchanged, "dense folds must not change logical slots");
    let expected: u32 = (1..=ROUNDS.len() as u32)
        .map(|r| degree + r * APPENDS)
        .sum();
    assert_eq!(scanned, (expected * reads) as usize);
    fixture.assert_repeated_state(mode);
    fixture.repeated_snapshot(&format!("{phase}_after"));
    fixture.assert_repeated_reuse(mode, initial_bucket, first_fold_runs);
    fixture.repeated_snapshot(&format!("{phase}_reused"));
    result
}

#[cfg(test)]
fn check_repeated_fold<const W: usize>(degree: u32) {
    // ADR 0099 removed the values-only fold: a bucket's values mirror its edge run, so folding "values"
    // without rows is not an operation any more and the mode that named it went with it.
    for mode in [RepeatedFold::Unfolded, RepeatedFold::Both] {
        let mut fixture = FoldFixture::<W>::new(degree);
        let (_, initial_bucket) = fixture.bucket();
        let initial_tail = fixture.graph.values.allocator_stats().slab_occupied_tail;
        let first_fold_runs = fixture.runs().map(|(id, used)| (id, used + APPENDS));
        let phase = format!("repeat_{mode:?}_r8_n{degree}");
        let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
        assert_eq!(fixture.scan_values(&mut scratch), degree as usize);
        fixture.repeated_snapshot(&format!("{phase}_before"));
        let mut previous_edge = NO_SPILL_RUN;
        for round in 1..=ROUNDS.len() {
            fixture.append_round();
            assert_eq!(fixture.degree, degree + round as u32 * APPENDS);
            let runs = fixture.runs();
            if round == 1 {
                assert_eq!(
                    runs, first_fold_runs,
                    "first append group grows arena tails in place"
                );
            }
            let edge_free = fixture.graph.edges.free_byte_spans();
            let old_leaf = fixture
                .graph
                .labeled_leaf_physical_range(fixture.src)
                .unwrap()
                .unwrap();
            fixture.repeated_snapshot(&format!("{phase}_append_{round}"));
            // ADR 0099: the values run is not a byte span that folds into a tail, so "later values stay at
            // the tail" stopped describing the plane; the edge run's reuse below still does.
            if mode == RepeatedFold::Both && round >= 3 {
                assert_eq!(runs[0], (previous_edge, APPENDS), "natural edge-run reuse");
            }
            previous_edge = runs[0].0;
            assert!(fixture.fold_round(mode).is_empty());
            fixture.assert_repeated_state(mode);
            if mode == RepeatedFold::Both && round >= 4 {
                let (start, len) = fixture
                    .graph
                    .labeled_leaf_physical_range(fixture.src)
                    .unwrap()
                    .unwrap();
                // Relocation claims a whole free span; in-place expansion claims only
                // the adjacent suffix. Observe the owner's physical leaf, not live rows.
                let (reused_start, reused_len) = if start == old_leaf.0 {
                    assert!(len > old_leaf.1);
                    (start + old_leaf.1, len - old_leaf.1)
                } else {
                    (start, len)
                };
                assert!(
                    edge_free.iter().any(|span| {
                        span.start_slot <= reused_start
                            && reused_start + reused_len <= span.start_slot + span.len
                    }),
                    "new edge allocation comes from the pre-fold free inventory"
                );
                assert!(
                    fixture.graph.edges.free_byte_spans().iter().all(|span| {
                        span.start_slot + span.len <= start || start + len <= span.start_slot
                    }),
                    "the live leaf must not remain allocator-owned free space"
                );
            }
            assert_eq!(
                fixture.scan_round(&mut scratch, 8),
                fixture.degree as usize * 8
            );
            // ADR 0099: values are addressed by row slot, so the byte-span allocator's tail and free-byte
            // accounting no longer follow the fill. What the rounds still guarantee is the scan above.
            let _ = (initial_tail, initial_bucket);
            fixture.repeated_snapshot(&format!("{phase}_after_{round}"));
        }
        fixture.assert_repeated_reuse(mode, initial_bucket, first_fold_runs);
        fixture.repeated_snapshot(&format!("{phase}_reused"));
    }
}

fn continuous_spill_workload<const W: usize>(
    degree: u32,
    reads: u32,
    period: ValueFoldPeriod,
) -> BenchResult {
    let mut fixture = FoldFixture::<W>::new(degree);
    let (_, initial_bucket) = fixture.bucket();
    let mut first_fold_runs = fixture.runs();
    first_fold_runs[1].1 += APPENDS * period as u32;
    let phase = format!("spill_{period:?}_r{reads}_n{degree}");
    fixture.assert_rows(degree);
    fixture.repeated_snapshot(&format!("{phase}_before"));
    let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
    assert_eq!(fixture.scan_values(&mut scratch), degree as usize);
    let mut scanned = 0;
    let result = bench_fn(|| {
        for (index, scope) in ROUNDS.into_iter().enumerate() {
            let _scope = bench_scope(scope);
            let round = index as u32 + 1;
            fixture.append_peer(round);
            fixture.append_round();
            if period.due(round) {
                fixture.fold_round(RepeatedFold::Values);
            }
            scanned += fixture.scan_round(&mut scratch, reads);
        }
    });
    let expected: u32 = (1..=ROUNDS.len() as u32)
        .map(|r| degree + r * APPENDS)
        .sum();
    assert_eq!(scanned, (expected * reads) as usize);
    assert_eq!(fixture.peers.len(), ROUNDS.len() + 1);
    let mode = if period == ValueFoldPeriod::Never {
        RepeatedFold::Unfolded
    } else {
        RepeatedFold::Values
    };
    fixture.assert_repeated_state(mode);
    fixture.repeated_snapshot(&format!("{phase}_after"));
    fixture.assert_repeated_reuse(mode, initial_bucket, first_fold_runs);
    fixture.repeated_snapshot(&format!("{phase}_reused"));
    result
}

#[cfg(test)]
fn check_continuous_spill<const W: usize>(degree: u32) {
    for period in [
        ValueFoldPeriod::Never,
        ValueFoldPeriod::Each,
        ValueFoldPeriod::Four,
        ValueFoldPeriod::Eight,
    ] {
        let mut fixture = FoldFixture::<W>::new(degree);
        let (_, initial_bucket) = fixture.bucket();
        let mut first_fold_runs = fixture.runs();
        first_fold_runs[1].1 += APPENDS * period as u32;
        let phase = format!("spill_{period:?}_r8_n{degree}");
        let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
        assert_eq!(fixture.scan_values(&mut scratch), degree as usize);
        fixture.repeated_snapshot(&format!("{phase}_before"));
        for round in 1..=ROUNDS.len() as u32 {
            let (_, before_peer) = fixture.bucket();
            let tail = fixture.graph.values.allocator_stats().slab_occupied_tail;
            let free = fixture.graph.values.free_byte_spans();
            fixture.append_peer(round);
            assert_eq!(fixture.peers.len(), round as usize + 1);
            assert_eq!(
                fixture.graph.values.allocator_stats().slab_occupied_tail,
                tail + W as u64,
                "a real peer claims the value tail"
            );
            assert_eq!(fixture.graph.values.free_byte_spans(), free);
            let (_, bucket) = fixture.bucket();
            assert_eq!(
                (
                    bucket.inline_property_bytes_offset(),
                    bucket.inline_property_bytes_slab_slots(),
                    bucket.values_spill_run()
                ),
                (
                    before_peer.inline_property_bytes_offset(),
                    before_peer.inline_property_bytes_slab_slots(),
                    before_peer.values_spill_run()
                ),
                "peer admission must not fold or relocate target values"
            );
            assert!(
                bucket.inline_property_bytes_offset()
                    + u64::from(bucket.inline_property_bytes_slab_slots()) * (W as u64)
                    < tail + W as u64,
                "target is non-tail before every append group"
            );
            fixture.repeated_snapshot(&format!("{phase}_peer_{round}"));
            fixture.append_round();
            assert_eq!(fixture.degree, degree + round * APPENDS);
            let runs = fixture.runs();
            assert_ne!(runs[1].0, NO_SPILL_RUN, "every target turn must spill");
            assert_eq!(
                runs[1].1,
                fixture.degree - bucket.inline_property_bytes_slab_slots()
            );
            if round == period as u32 {
                assert_eq!(runs[1], first_fold_runs[1], "first-fold class and ID");
            }
            fixture.repeated_snapshot(&format!("{phase}_append_{round}"));
            let vertex = fixture.graph.vertices().get(fixture.src);
            let (_, before_fold) = fixture.bucket();
            if period.due(round) {
                assert!(fixture.fold_round(RepeatedFold::Values).is_empty());
                fixture.assert_values_folded(vertex, before_fold);
                let old_start = before_fold.inline_property_bytes_offset();
                let old_len = u64::from(before_fold.inline_property_bytes_slab_slots()) * W as u64;
                assert!(
                    fixture.graph.values.free_byte_spans().iter().any(|span| {
                        span.start_slot <= old_start
                            && old_start + old_len <= span.start_slot + span.len
                    }),
                    "the replaced prefix is allocator-owned free space"
                );
            } else {
                fixture.assert_rows(fixture.degree);
            }
            // Independent expected schedule: a broken due() must not redefine the oracle.
            let last_fold = if period == ValueFoldPeriod::Never {
                0
            } else {
                round / period as u32 * period as u32
            };
            let prefix = if last_fold == 0 {
                initial_bucket.inline_property_bytes_slab_slots()
            } else {
                degree + last_fold * APPENDS
            };
            let (_, after) = fixture.bucket();
            assert_eq!(after.inline_property_bytes_slab_slots(), prefix);
            assert_eq!(fixture.runs()[1].1, fixture.degree - prefix);
            assert_eq!(
                after.values_spill_run() == NO_SPILL_RUN,
                fixture.degree == prefix
            );
            assert_eq!(after.stored_slots(), 0);
            assert_eq!(fixture.runs()[0].1, fixture.degree);
            assert!(!after.is_tree_mode());
            assert_eq!(
                fixture.scan_round(&mut scratch, 8),
                fixture.degree as usize * 8
            );
            fixture.repeated_snapshot(&format!("{phase}_after_{round}"));
        }
        let mode = if period == ValueFoldPeriod::Never {
            RepeatedFold::Unfolded
        } else {
            RepeatedFold::Values
        };
        fixture.assert_repeated_reuse(mode, initial_bucket, first_fold_runs);
        fixture.repeated_snapshot(&format!("{phase}_reused"));
    }
}

impl<const W: usize> FoldFixture<W> {
    fn new_update_reuse() -> Self {
        let fixture = Self::new(256);
        fixture
            .graph
            .ensure_label_bucket_inline_property_byte_width(fixture.src, CONSUMER, W as u16)
            .unwrap();
        for slot in 0..16 {
            fixture.insert(CONSUMER, 20_000 + slot);
        }
        fixture.assert_rows(256);
        fixture.assert_consumer_rows(0, 0);
        assert_eq!(fixture.bucket().1.inline_property_bytes_slab_slots(), 32);
        assert_eq!(fixture.runs()[1].1, 224);
        let consumer = fixture.bucket_for(CONSUMER).1;
        assert_eq!(consumer.inline_property_bytes_slab_slots(), 16);
        assert_eq!(fixture.runs_for(CONSUMER)[1], (NO_SPILL_RUN, 0));
        // ADR 0099: values are addressed by row slot, so the byte-span allocator's tail and free-byte
        // accounting no longer describe this plane (they read `0` forever). The geometry that does hold is
        // the one above: the target owns 32 slots, the consumer 16, and the consumer's span starts after
        // the target's.
        assert!(
            consumer.inline_property_bytes_offset()
                >= fixture.bucket().1.inline_property_bytes_offset() + 32 * W as u64
        );
        fixture
    }

    fn append_consumer(&self, round: u32) {
        #[cfg(target_family = "wasm")]
        let _scope = bench_scope("consumer_append");
        let first = 16 + (round - 1) * APPENDS;
        for slot in first..first + APPENDS {
            self.insert(CONSUMER, 20_000 + slot);
        }
    }

    fn update_consumer(&self, round: u32) -> bool {
        #[cfg(target_family = "wasm")]
        let _scope = bench_scope("consumer_update");
        self.graph
            .update_edge_inline_property_at_slot(
                self.src,
                CONSUMER,
                0,
                RangeBenchEdge(
                    InlinePropertyBenchEdge::new(20_010, 1_000_000 + u64::from(round)),
                    0,
                ),
            )
            .unwrap()
    }

    // Append and update are separate phases; the expected first value must not come from storage.
    fn assert_consumer_rows(&self, appended_round: u32, updated_round: u32) {
        let want: Vec<_> = (0..16 + appended_round * APPENDS)
            .map(|slot| {
                let seed = if slot == 0 && updated_round > 0 {
                    1_000_000 + updated_round
                } else {
                    20_000 + slot
                };
                (
                    slot,
                    20_010 + slot,
                    InlinePropertyBenchEdge::<W>::new(0, u64::from(seed))
                        .inline_property
                        .to_vec(),
                )
            })
            .collect();
        self.assert_bucket_rows(CONSUMER, &want);
    }

    fn update_snapshot(&self, phase: &str) {
        self.repeated_snapshot(phase);
        let (_, bucket) = self.bucket_for(CONSUMER);
        let runs = self.runs_for(CONSUMER).map(|(id, used)| [id, used]);
        let mut first = Vec::new();
        let _ = self
            .graph
            .visit_edges_with_inline_property::<()>(
                self.src,
                CONSUMER,
                OutEdgeOrder::Ascending,
                |slot, item| {
                    assert_eq!(slot.raw(), 0);
                    first = item.inline_property.bytes().to_vec();
                    ControlFlow::Break(())
                },
            )
            .unwrap();
        let record = format!(
            "FOLD_CONSUMER {{\"width\":{W},\"phase\":\"{phase}\",\"degree\":{},\"runs\":{runs:?},\"value_offset\":{},\"value_slab_slots\":{},\"edge_offset\":{},\"edge_prefix\":{},\"first_value\":{first:?}}}",
            bucket.degree(),
            bucket.inline_property_bytes_offset(),
            bucket.inline_property_bytes_slab_slots(),
            bucket.edge_start(),
            bucket.stored_slots(),
        );
        #[cfg(target_family = "wasm")]
        ic_cdk::println!("{record}");
        #[cfg(not(target_family = "wasm"))]
        println!("{record}");
    }

    #[cfg(test)]
    fn value_span(&self, label: BucketLabelKey) -> std::ops::Range<u64> {
        let (_, bucket) = self.bucket_for(label);
        let start = bucket.inline_property_bytes_offset();
        start..start + self.graph.bucket_resident_inline_property_bytes(&bucket)
    }

    #[cfg(test)]
    fn assert_update_ownership(&self) {
        let stats = self.graph.values.allocator_stats();
        let free = self.graph.values.free_byte_spans();
        let mut spans = Vec::new();
        let mut runs = [Vec::new(), Vec::new()];
        for label in [LABEL, CONSUMER]
            .into_iter()
            .chain(self.peers.iter().map(|p| p.0))
        {
            let bucket = self.bucket_for(label).1;
            assert!(!bucket.is_tree_mode());
            assert_eq!(bucket.inline_property_byte_width(), W as u16);
            let span = self.value_span(label);
            assert!(span.start < span.end && span.end <= stats.slab_occupied_tail);
            assert!(
                free.iter()
                    .all(|f| f.start_slot + f.len <= span.start || span.end <= f.start_slot)
            );
            spans.push(span);
            for ((id, _), ids) in self.runs_for(label).into_iter().zip(&mut runs) {
                if id != NO_SPILL_RUN {
                    assert!(
                        !ids.contains(&id),
                        "two live buckets must not own the same run in one arena"
                    );
                    ids.push(id);
                }
            }
        }
        spans.sort_by_key(|s| s.start);
        assert!(spans.windows(2).all(|s| s[0].end <= s[1].start));
        let live: u64 = spans.iter().map(|s| s.end - s.start).sum();
        assert_eq!(
            live,
            self.graph
                .vertices()
                .get(self.src)
                .inline_property_bytes_allocated_bytes()
        );
        assert_eq!(stats.free_bytes, free.iter().map(|f| f.len).sum::<u64>());
        let mut free_ranges: Vec<_> = free
            .iter()
            .map(|f| f.start_slot..f.start_slot + f.len)
            .collect();
        free_ranges.sort_by_key(|s| s.start);
        assert!(
            free_ranges
                .iter()
                .all(|s| s.start < s.end && s.end <= stats.slab_occupied_tail)
        );
        assert!(free_ranges.windows(2).all(|s| s[0].end <= s[1].start));
        assert_eq!(
            stats.slab_occupied_tail,
            live + stats.free_bytes,
            "every occupied byte has exactly one owner"
        );
    }
}

fn update_reuse_workload<const W: usize>(period: ValueFoldPeriod) -> BenchResult {
    let mut fixture = FoldFixture::<W>::new_update_reuse();
    let phase = format!("update_{period:?}");
    let mut producer = LabeledInlinePropertyValueBatchScratch::default();
    let mut consumer = LabeledInlinePropertyValueBatchScratch::default();
    assert_eq!(fixture.scan_values(&mut producer), 256);
    assert_eq!(fixture.scan_bucket_values(CONSUMER, &mut consumer), 16);
    fixture.update_snapshot(&format!("{phase}_before"));
    let mut scanned = [0, 0];
    let mut updated = 0;
    let result = bench_fn(|| {
        for (index, scope) in ROUNDS.into_iter().enumerate() {
            let _scope = bench_scope(scope);
            let round = index as u32 + 1;
            fixture.append_peer(round);
            fixture.append_round();
            fixture.append_consumer(round);
            if period.due(round) {
                fixture.fold_round(RepeatedFold::Values);
            }
            updated += u32::from(fixture.update_consumer(round));
            scanned[0] += fixture.scan_round(&mut producer, 8);
            let _scope = bench_scope("consumer_scan");
            scanned[1] += fixture.scan_bucket_values(CONSUMER, &mut consumer);
        }
    });
    assert_eq!(updated, 8);
    assert_eq!(scanned, [18_688, 416]);
    assert_eq!(fixture.degree, 320);
    assert_eq!(fixture.peers.len(), 9);
    fixture.assert_rows(320);
    fixture.assert_consumer_rows(8, 8);
    fixture.update_snapshot(&format!("{phase}_after"));
    result
}

#[cfg(test)]
fn check_update_reuse<const W: usize>() {
    for period in [
        ValueFoldPeriod::Never,
        ValueFoldPeriod::Each,
        ValueFoldPeriod::Four,
        ValueFoldPeriod::Eight,
    ] {
        let mut fixture = FoldFixture::<W>::new_update_reuse();
        let phase = format!("update_{period:?}");
        let mut producer = LabeledInlinePropertyValueBatchScratch::default();
        let mut consumer = LabeledInlinePropertyValueBatchScratch::default();
        assert_eq!(fixture.scan_values(&mut producer), 256);
        assert_eq!(fixture.scan_bucket_values(CONSUMER, &mut consumer), 16);
        fixture.assert_update_ownership();
        fixture.update_snapshot(&format!("{phase}_before"));
        let mut hits = 0;
        for round in 1..=8 {
            let actors = [LABEL, CONSUMER]
                .map(|label| (label, fixture.value_span(label), fixture.runs_for(label)[1]));
            let before_peer = fixture.graph.values.allocator_stats();
            let free = fixture.graph.values.free_byte_spans();
            fixture.append_peer(round);
            assert_eq!(fixture.peers.len(), round as usize + 1);
            let tail = fixture.graph.values.allocator_stats().slab_occupied_tail;
            assert_eq!(
                tail,
                before_peer.slab_occupied_tail + W as u64,
                "peer claims the value tail"
            );
            assert_eq!(fixture.graph.values.free_byte_spans(), free);
            for (label, span, run) in actors {
                assert_eq!(fixture.value_span(label), span);
                assert_eq!(fixture.runs_for(label)[1], run);
                assert!(
                    span.end < tail,
                    "both actors must be non-tail before appends"
                );
            }
            fixture.update_snapshot(&format!("{phase}_peer_{round}"));
            fixture.append_round();
            fixture.append_consumer(round);
            assert_eq!(fixture.degree, 256 + round * APPENDS);
            let consumer_degree = 16 + round * APPENDS;
            assert_eq!(fixture.bucket_for(CONSUMER).1.degree(), consumer_degree);
            assert_ne!(fixture.runs()[1].0, NO_SPILL_RUN);
            assert_ne!(fixture.runs_for(CONSUMER)[1].0, NO_SPILL_RUN);
            assert_eq!(fixture.runs_for(CONSUMER)[1].1, APPENDS);
            fixture.assert_rows(fixture.degree);
            fixture.assert_consumer_rows(round, round - 1);
            fixture.assert_update_ownership();
            fixture.update_snapshot(&format!("{phase}_append_{round}"));
            let vertex = fixture.graph.vertices().get(fixture.src);
            let (_, before_fold) = fixture.bucket();
            let old_producer = fixture.value_span(LABEL);
            if period.due(round) {
                assert!(fixture.fold_round(RepeatedFold::Values).is_empty());
                fixture.assert_values_folded(vertex, before_fold);
                assert!(
                    fixture
                        .graph
                        .values
                        .free_byte_spans()
                        .iter()
                        .any(|f| f.start_slot <= old_producer.start
                            && old_producer.end <= f.start_slot + f.len)
                );
            }
            // Arithmetic, not due(), defines the independent producer-prefix oracle.
            let last_fold = if period == ValueFoldPeriod::Never {
                0
            } else {
                round / period as u32 * period as u32
            };
            let prefix = if last_fold == 0 {
                32
            } else {
                256 + last_fold * APPENDS
            };
            let after_fold = fixture.bucket().1;
            assert_eq!(after_fold.inline_property_bytes_slab_slots(), prefix);
            assert_eq!(fixture.runs()[1].1, fixture.degree - prefix);
            assert_eq!(
                after_fold.values_spill_run() == NO_SPILL_RUN,
                fixture.degree == prefix
            );
            assert_eq!(after_fold.stored_slots(), 0);
            assert_eq!(fixture.runs()[0].1, fixture.degree);
            fixture.assert_consumer_rows(round, round - 1);
            fixture.assert_update_ownership();
            fixture.update_snapshot(&format!("{phase}_fold_{round}"));
            let free = fixture.graph.values.free_byte_spans();
            let before_update = fixture.graph.values.allocator_stats();
            let old_consumer = fixture.value_span(CONSUMER);
            let vertex = fixture.graph.vertices().get(fixture.src);
            let bucket = fixture.bucket_for(CONSUMER).1;
            assert!(
                fixture.update_consumer(round),
                "ordinary consumer update must succeed"
            );
            let after = fixture.assert_value_only_edges(CONSUMER, vertex, bucket);
            assert_eq!(
                after.values_spill_run(),
                NO_SPILL_RUN,
                "update must fold the nonempty consumer run"
            );
            assert_eq!(after.inline_property_bytes_slab_slots(), consumer_degree);
            let destination = fixture.value_span(CONSUMER);
            let bytes = destination.end - destination.start;
            let reused = free.iter().any(|f| {
                f.start_slot <= destination.start && destination.end <= f.start_slot + f.len
            });
            assert_eq!(
                reused,
                free.iter().any(|f| f.len >= bytes),
                "a fitting free span must be consumed"
            );
            assert_eq!(
                reused,
                period == ValueFoldPeriod::Each || (period == ValueFoldPeriod::Four && round == 8)
            );
            hits += u32::from(reused);
            let stats = fixture.graph.values.allocator_stats();
            assert_eq!(
                stats.slab_occupied_tail,
                before_update.slab_occupied_tail + if reused { 0 } else { bytes }
            );
            assert_eq!(
                stats.free_bytes,
                before_update.free_bytes + old_consumer.end
                    - old_consumer.start
                    - if reused { bytes } else { 0 }
            );
            assert!(
                fixture
                    .graph
                    .values
                    .free_byte_spans()
                    .iter()
                    .any(|f| f.start_slot <= old_consumer.start
                        && old_consumer.end <= f.start_slot + f.len)
            );
            fixture.assert_update_ownership();
            fixture.assert_rows(fixture.degree);
            fixture.assert_consumer_rows(round, round);
            assert_eq!(
                fixture.scan_round(&mut producer, 8),
                fixture.degree as usize * 8
            );
            assert_eq!(
                fixture.scan_bucket_values(CONSUMER, &mut consumer),
                consumer_degree as usize
            );
            fixture.update_snapshot(&format!("{phase}_after_{round}"));
        }
        let (expected_hits, tail, free) = match period {
            ValueFoldPeriod::Never => (0, 473, 352),
            ValueFoldPeriod::Each => (8, 2393, 1984),
            ValueFoldPeriod::Four => (1, 1001, 592),
            ValueFoldPeriod::Eight => (0, 793, 384),
        };
        assert_eq!(hits, expected_hits);
        let stats = fixture.graph.values.allocator_stats();
        assert_eq!(stats.slab_occupied_tail, tail * W as u64);
        assert_eq!(stats.free_bytes, free * W as u64);
    }
}

// `update_reuse_case!` measured how an updated bucket's value blocks reused the byte-span allocator
// across `ValueFoldPeriod` variants. ADR 0099 addresses values by row slot, so those allocator statistics
// (`slab_occupied_tail`, `free_bytes`) read zero and the fixtures' own assertions no longer describe the
// plane - recorded in `GAP-LARA-BENCH-VERIFIERS-PRE-0099` with the rest of the pre-ADR verifiers.

fn fold_cost<const W: usize>(degree: u32) -> BenchResult {
    let fixture = FoldFixture::<W>::new(degree);
    fixture.assert_rows(degree);
    fixture.snapshot("unfolded");
    let runs = fixture.runs();
    let pre_fold_rows = fixture.arenas().map(|arena| arena.arena_rows());
    let mut moves = Vec::new();
    let result = bench_fn(|| {
        {
            let _scope = bench_scope("edge_fold");
            moves = fixture.fold_edges();
        }
        {
            let _scope = bench_scope("value_fold");
            fixture.fold_values();
        }
    });
    assert!(moves.is_empty(), "dense fold must not change logical slots");
    fixture.assert_rows(degree);
    fixture.snapshot("folded");
    assert_eq!(fixture.runs(), [(NO_SPILL_RUN, 0); 2]);
    fixture.assert_reusable_against(runs, pre_fold_rows);
    result
}

fn workload<const W: usize>(fixture: FoldFixture<W>, phase: &str) -> BenchResult {
    fixture.assert_rows(fixture.degree);
    fixture.snapshot(phase);
    let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
    // Match scratch reuse in the earlier range series; initialization is outside timing.
    fixture.scan_values(&mut scratch);
    let result = bench_fn(|| {
        {
            let _scope = bench_scope("topology");
            let _ = fixture
                .graph
                .visit_edges(fixture.src, LABEL, OutEdgeOrder::Ascending, |slot, edge| {
                    black_box((slot, edge));
                    ControlFlow::<()>::Continue(())
                })
                .unwrap();
        }
        {
            let _scope = bench_scope("values");
            fixture.scan_values(&mut scratch);
        }
        {
            let _scope = bench_scope("hydrated");
            let _ = fixture
                .graph
                .visit_edges_with_inline_property::<()>(
                    fixture.src,
                    LABEL,
                    OutEdgeOrder::Descending,
                    |slot, item| {
                        black_box((slot, item.edge, item.inline_property.bytes()));
                        ControlFlow::Continue(())
                    },
                )
                .unwrap();
        }
        {
            let _scope = bench_scope("append");
            fixture.insert(LABEL, fixture.degree);
        }
        {
            let _scope = bench_scope("append_tail");
            for slot in fixture.degree + 1..fixture.degree + APPENDS {
                fixture.insert(LABEL, slot);
            }
        }
        {
            let _scope = bench_scope("values_after_appends");
            fixture.scan_values(&mut scratch);
        }
    });
    fixture.assert_rows(fixture.degree + APPENDS);
    fixture.snapshot(&format!("{phase}_after_appends"));
    result
}

#[cfg(test)]
fn check_fold_economics<const W: usize>(degree: u32) {
    {
        let before = FoldFixture::<W>::new(degree);
        before.check_appends("unfolded");
    }
    // ADR 0099 removed the values-only fold: a bucket's values mirror its edge run, so a state with
    // edges in the span and values still in a run no longer exists. `fold_values` is the paired fold and
    // cannot move values without rows to move, so this block's case went with that operation.
    let fixture = FoldFixture::<W>::new(degree);
    fixture.assert_rows(degree);
    fixture.snapshot("unfolded");
    let runs = fixture.runs();
    let (_, before) = fixture.bucket();
    assert!(fixture.fold_edges().is_empty());
    let (_, after_edge) = fixture.bucket();
    assert_eq!(after_edge.edge_spill_run(), NO_SPILL_RUN);
    // ADR 0099: the values plane mirrors the edge plane, so folding the edges carries the values into the
    // prefix with them. The pre-ADR expectation - that folding edges left the values plane untouched - is
    // the divergence ADR 0099 removed.
    assert_eq!(after_edge.values_spill_run(), NO_SPILL_RUN);
    assert_eq!(after_edge.inline_property_bytes_slab_slots(), degree);
    let _ = before;
    fixture.assert_rows(degree);
    fixture.snapshot("edge_folded");
    fixture.fold_values();
    let (_, after_value) = fixture.bucket();
    assert_eq!(
        (after_value.edge_start(), after_value.stored_slots()),
        (after_edge.edge_start(), after_edge.stored_slots())
    );
    fixture.assert_rows(degree);
    fixture.snapshot("folded");
    assert_eq!(fixture.runs(), [(NO_SPILL_RUN, 0); 2]);
    fixture.assert_reusable(runs);
    fixture.check_appends("folded");
}

macro_rules! fold_case {
    ($fold:ident, $before:ident, $after:ident, $test:ident, $width:literal, $degree:literal) => {
        #[bench(raw)]
        fn $fold() -> BenchResult {
            fold_cost::<$width>($degree)
        }
        #[bench(raw)]
        fn $before() -> BenchResult {
            workload(FoldFixture::<$width>::new($degree), "unfolded")
        }
        #[bench(raw)]
        fn $after() -> BenchResult {
            let fixture = FoldFixture::<$width>::new($degree);
            assert!(fixture.fold_edges().is_empty());
            fixture.fold_values();
            assert_eq!(fixture.runs(), [(NO_SPILL_RUN, 0); 2]);
            workload(fixture, "folded")
        }
        #[cfg(test)]
        #[test]
        fn $test() {
            check_fold_economics::<$width>($degree);
        }
    };
}

fold_case!(
    bench_t_fold_cost_8_64,
    bench_t_fold_before_8_64,
    bench_t_fold_after_8_64,
    fold_economics_8_64,
    8,
    64
);
fold_case!(
    bench_t_fold_cost_8_256,
    bench_t_fold_before_8_256,
    bench_t_fold_after_8_256,
    fold_economics_8_256,
    8,
    256
);
fold_case!(
    bench_t_fold_cost_33_64,
    bench_t_fold_before_33_64,
    bench_t_fold_after_33_64,
    fold_economics_33_64,
    33,
    64
);
fold_case!(
    bench_t_fold_cost_33_256,
    bench_t_fold_before_33_256,
    bench_t_fold_after_33_256,
    fold_economics_33_256,
    33,
    256
);

macro_rules! repeated_case {
    ($width:literal, $degree:literal, $test:ident;
     $($reads:literal => ($unfolded:ident, $both:ident)),+ $(,)?) => {
        $(
            #[bench(raw)]
            fn $unfolded() -> BenchResult {
                repeated_workload::<$width>($degree, $reads, RepeatedFold::Unfolded)
            }
            #[bench(raw)]
            fn $both() -> BenchResult {
                repeated_workload::<$width>($degree, $reads, RepeatedFold::Both)
            }
        )+
        #[cfg(test)]
        #[test]
        fn $test() {
            check_repeated_fold::<$width>($degree);
        }
    };
}

repeated_case!(8, 64, repeated_fold_8_64;
    1 => (bench_t_repeat_unfolded_8_64_r1, bench_t_repeat_both_8_64_r1),
    8 => (bench_t_repeat_unfolded_8_64_r8, bench_t_repeat_both_8_64_r8),
);
repeated_case!(8, 256, repeated_fold_8_256;
    1 => (bench_t_repeat_unfolded_8_256_r1, bench_t_repeat_both_8_256_r1),
    8 => (bench_t_repeat_unfolded_8_256_r8, bench_t_repeat_both_8_256_r8),
);
repeated_case!(33, 64, repeated_fold_33_64;
    1 => (bench_t_repeat_unfolded_33_64_r1, bench_t_repeat_both_33_64_r1),
    8 => (bench_t_repeat_unfolded_33_64_r8, bench_t_repeat_both_33_64_r8),
);
repeated_case!(33, 256, repeated_fold_33_256;
    1 => (bench_t_repeat_unfolded_33_256_r1, bench_t_repeat_both_33_256_r1),
    8 => (bench_t_repeat_unfolded_33_256_r8, bench_t_repeat_both_33_256_r8),
);
