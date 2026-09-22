// Corrected diagnostic: reported moves remain authoritative at Finished.
// The superseded reset oracle is preserved in adr0097-crossover-16k-finished-reset-probe.rs.
use super::*;
use crate::traits::{CsrEdge, CsrEdgeTombstone, CsrVertex};
use ic_stable_structures::Memory;
use std::{cell::RefCell, io::Write, ops::ControlFlow, rc::Rc, time::Instant};

const N: u32 = 16_384;
thread_local! { static DECODES: RefCell<[u64; 2]> = const { RefCell::new([0; 2]) }; }

// Identical 10-byte crossover representation; only the decode counter is added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BenchEdge(u32);
impl CsrEdge for BenchEdge {
    const BYTES: usize = 10;
    fn read_from(bytes: &[u8]) -> Self {
        let target = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        DECODES.with(|c| {
            c.borrow_mut()[usize::from(target == u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))] += 1
        });
        Self(target)
    }
    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
        bytes[4..10].fill(0);
    }
    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.0)
    }
    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self(u32::from(vid))
    }
}
impl CsrEdgeTombstone for BenchEdge {
    fn tombstone_edge() -> Self {
        Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
    }
}

#[derive(Clone)]
struct CountMemory {
    inner: crate::VectorMemory,
    id: usize,
    // reads, read bytes, writes, write bytes, grows, requested 64-KiB pages
    counts: Rc<RefCell<[[u64; 6]; 16]>>,
}
impl CountMemory {
    fn read_count(&self, len: usize) {
        let mut c = self.counts.borrow_mut();
        c[self.id][0] += 1;
        c[self.id][1] += len as u64;
    }
}
impl Memory for CountMemory {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn grow(&self, pages: u64) -> i64 {
        {
            let mut c = self.counts.borrow_mut();
            c[self.id][4] += 1;
            c[self.id][5] += pages;
        }
        self.inner.grow(pages)
    }
    fn read(&self, offset: u64, bytes: &mut [u8]) {
        self.read_count(bytes.len());
        self.inner.read(offset, bytes);
    }
    unsafe fn read_unsafe(&self, offset: u64, bytes: *mut u8, len: usize) {
        self.read_count(len);
        unsafe { self.inner.read_unsafe(offset, bytes, len) }
    }
    fn write(&self, offset: u64, bytes: &[u8]) {
        {
            let mut c = self.counts.borrow_mut();
            c[self.id][2] += 1;
            c[self.id][3] += bytes.len() as u64;
        }
        self.inner.write(offset, bytes);
    }
}
type Graph = LabeledLaraGraph<BenchEdge, CountMemory>;
fn begin(memory: &CountMemory) -> Instant {
    *memory.counts.borrow_mut() = [[0; 6]; 16];
    DECODES.with(|c| *c.borrow_mut() = [0; 2]);
    Instant::now()
}
fn report(memory: &CountMemory, phase: &str, since: Instant) {
    eprintln!(
        "CROSSOVER phase={phase} seconds={:.6} rwg={:?} decodes={:?}",
        since.elapsed().as_secs_f64(),
        memory.counts.borrow(),
        DECODES.with(|c| *c.borrow())
    );
}
fn shape(graph: &Graph, phase: &str, live: u32) {
    let vertex = graph.vertices().get(VertexId::from(0));
    assert!(!vertex.is_default_edge_labeled());
    let bucket = match graph
        .find_bucket(VertexId::from(0), &vertex, BucketLabelKey::from_raw(2))
        .unwrap()
    {
        BucketSearch::Found { bucket, .. } => bucket,
        _ => panic!("missing hub"),
    };
    assert!(
        !bucket.is_tree_mode(),
        "ten-byte crossover must not be a four-byte tree"
    );
    assert!(!bucket.is_tiny_mode());
    let used = if bucket.edge_spill_run() == crate::traits::NO_SPILL_RUN {
        0
    } else {
        graph
            .edges()
            .spill_used_len(graph.edges().spill_mint(bucket.edge_spill_run()).unwrap())
    };
    let counts = graph.leaf_segment_counts_for_vid(VertexId::from(0));
    eprintln!(
        "CROSSOVER shape={phase} mode=slab vertex={vertex:?} prefix={} run={} used={used} degree={} global={} leaf={counts:?}",
        bucket.stored_slots_raw(),
        bucket.edge_spill_run(),
        bucket.degree(),
        graph.edges().header().num_edges
    );
    assert_eq!(bucket.degree(), live);
    assert_eq!(graph.edges().header().num_edges as u64, u64::from(live));
    if phase == "seeded" {
        assert_eq!(bucket.stored_slots_raw().checked_add(used).unwrap(), N);
    }
}
fn collect(graph: &Graph, order: OutEdgeOrder) -> Vec<(u32, u32)> {
    let mut rows = Vec::new();
    assert_eq!(
        graph
            .visit_edges(
                VertexId::from(0),
                BucketLabelKey::from_raw(2),
                order,
                |slot, edge| {
                    rows.push((slot.raw(), edge.0));
                    ControlFlow::<()>::Continue(())
                }
            )
            .unwrap(),
        ControlFlow::Continue(())
    );
    rows
}
fn assert_rows(actual: Vec<(u32, u32)>, expected: &[(u32, u32)]) {
    assert_eq!(actual.len(), expected.len(), "row count");
    for (index, (row, truth)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(row, truth, "exact row at position {index}");
    }
}
fn apply_moves(expected: &mut [Option<u32>], moves: &[EdgeSlotMove]) {
    // Batch rewrites can place a row into another move's former source slot.
    let targets: Vec<_> = moves
        .iter()
        .map(|m| {
            assert_eq!(m.label_id, BucketLabelKey::from_raw(2));
            expected[m.old_slot_index as usize]
                .take()
                .expect("reported move source must be live")
        })
        .collect();
    for (m, target) in moves.iter().zip(targets) {
        assert!(
            expected[m.new_slot_index as usize]
                .replace(target)
                .is_none(),
            "reported move overwrote an unmoved survivor"
        );
    }
}

#[test]
fn diagnostic_crossover_16k_event_parity() {
    let original = exercise(false, None);
    exercise(true, Some(&original));
}

fn exercise(forward_cursor: bool, original: Option<&[Vec<u8>]>) -> Vec<Vec<u8>> {
    let expected_manifest = std::env::var("SPILL_PROBE_EXPECTED_MANIFEST").unwrap();
    assert_eq!(
        std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap(),
        std::fs::canonicalize(expected_manifest).unwrap(),
        "stale shared-target binary"
    );
    eprintln!(
        "CROSSOVER compiled_manifest={} forward_cursor={forward_cursor}",
        env!("CARGO_MANIFEST_DIR")
    );
    let counts = Rc::new(RefCell::new([[0; 6]; 16]));
    let m: Vec<_> = (0..16)
        .map(|id| CountMemory {
            inner: Rc::new(RefCell::new(Vec::new())),
            id,
            counts: counts.clone(),
        })
        .collect();
    let t = begin(&m[0]);
    let graph = Graph::new(
        m[0].clone(),
        m[1].clone(),
        m[2].clone(),
        m[3].clone(),
        m[4].clone(),
        m[5].clone(),
        m[6].clone(),
        m[7].clone(),
        m[8].clone(),
        m[9].clone(),
        m[10].clone(),
        m[11].clone(),
        m[12].clone(),
        m[13].clone(),
        m[14].clone(),
        m[15].clone(),
        crate::labeled::InitialCapacities::uniform(1 << 20),
        BucketLabelKey::from_raw(1),
    )
    .unwrap();
    report(&m[0], "constructor", t);
    let t = begin(&m[0]);
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    assert_eq!(src, VertexId::from(0));
    let label = BucketLabelKey::from_raw(2);
    for target in 0..N {
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                BenchEdge(target),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    report(&m[0], "seed", t);
    shape(&graph, "seeded", N);
    let seeded: Vec<_> = (0..N).map(|i| (i, i)).collect();
    assert_rows(collect(&graph, OutEdgeOrder::Ascending), &seeded);
    assert_rows(
        collect(&graph, OutEdgeOrder::Descending),
        &seeded.iter().rev().copied().collect::<Vec<_>>(),
    );
    eprintln!("CROSSOVER seeded identities verified={N}");

    let t = begin(&m[0]);
    for slot in (0..N).step_by(2) {
        let removed = graph
            .remove_edge_at_slot(src, label, slot)
            .unwrap()
            .expect("even slot live");
        assert_eq!(removed.0, slot, "removed target at slot {slot}");
    }
    report(&m[0], "remove_even", t);
    shape(&graph, "removed", N / 2);
    let odd: Vec<_> = (1..N).step_by(2).map(|i| (i, i)).collect();
    assert_rows(collect(&graph, OutEdgeOrder::Ascending), &odd);
    assert_rows(
        collect(&graph, OutEdgeOrder::Descending),
        &odd.iter().rev().copied().collect::<Vec<_>>(),
    );
    eprintln!("CROSSOVER precompact identities verified={}", odd.len());

    // Assert physical byte parity before comparing cursor work, not merely matching names.
    let seeded: Vec<_> = m
        .iter()
        .map(|memory| memory.inner.borrow().clone())
        .collect();
    if let Some(original) = original {
        assert_eq!(seeded.len(), original.len());
        for (role, (before, after)) in original.iter().zip(&seeded).enumerate() {
            assert!(
                before == after,
                "precompaction backing differs at role {role}"
            );
        }
        eprintln!(
            "CROSSOVER precompaction_backing_byte_equal roles={}",
            seeded.len()
        );
    }
    let out = std::path::PathBuf::from(std::env::var_os("SPILL_PROBE_OUTPUT").unwrap()).join(
        if forward_cursor {
            "forwarded-cursor"
        } else {
            "zero-cursor"
        },
    );
    std::fs::create_dir_all(&out).unwrap();
    let mut steps = std::fs::File::create(out.join("steps.tsv")).unwrap();
    let mut movements = std::fs::File::create(out.join("movements.tsv")).unwrap();
    writeln!(steps, "step\tresume_bucket\tresume_slot\toutcome\tmoves").unwrap();
    writeln!(movements, "step\told_slot\tnew_slot").unwrap();
    let mut expected: Vec<_> = (0..N).map(|i| (i % 2 == 1).then_some(i)).collect();
    let mut resume = 0;
    let mut cursor = 0;
    let mut stats = [0u32; 4]; // EdgeMoved, AdvanceBucket, OverflowRewrite, Finished
    let mut finished = false;
    let mut stopping_error = None;
    let t = begin(&m[0]);
    for step in 0..(2 * N + 16) {
        let before = resume;
        let before_cursor = cursor;
        let result = graph.compact_vertex_edge_span_one_step_with_span_policy(
            src,
            resume,
            cursor,
            &|_| EdgePlacementPolicy::Insertion,
            SpanResolutionPolicy::SlackMayBeDropped,
        );
        let (outcome, moves) = match result {
            Ok(VertexEdgeSpanCompactOneStep::EdgeMoved(m)) => {
                stats[0] += 1;
                if forward_cursor {
                    cursor = m.new_slot_index.checked_add(1).unwrap();
                }
                ("EdgeMoved", vec![m])
            }
            Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(next)) => {
                stats[1] += 1;
                resume = next;
                cursor = 0;
                ("AdvanceBucket", Vec::new())
            }
            Ok(VertexEdgeSpanCompactOneStep::OverflowRewrite(moves)) => {
                stats[2] += 1;
                resume = 0;
                cursor = 0;
                ("OverflowRewrite", moves)
            }
            Ok(VertexEdgeSpanCompactOneStep::Finished) => {
                stats[3] += 1;
                finished = true;
                ("Finished", Vec::new())
            }
            Err(error) => {
                stopping_error = Some(format!("{error:?}"));
                ("Error", Vec::new())
            }
        };
        writeln!(
            steps,
            "{step}\t{before}\t{before_cursor}\t{outcome}\t{}",
            moves.len()
        )
        .unwrap();
        for m in &moves {
            writeln!(
                movements,
                "{step}\t{}\t{}",
                m.old_slot_index, m.new_slot_index
            )
            .unwrap();
        }
        apply_moves(&mut expected, &moves);
        if step % 1024 == 0 {
            eprintln!("CROSSOVER compaction progress step={step} stats={stats:?}");
        }
        if finished || stopping_error.is_some() {
            break;
        }
    }
    report(&m[0], "compact", t);
    eprintln!("CROSSOVER outcome finished={finished} error={stopping_error:?} stats={stats:?}");
    assert!(
        finished || stopping_error.is_some(),
        "diagnostic progress bound exhausted; not a completed compaction"
    );
    shape(&graph, "after_compact", N / 2);
    let reported: Vec<_> = expected
        .into_iter()
        .enumerate()
        .filter_map(|(slot, target)| target.map(|target| (slot as u32, target)))
        .collect();
    assert_eq!(
        reported.iter().map(|r| r.1).collect::<Vec<_>>(),
        (1..N).step_by(2).collect::<Vec<_>>(),
        "insertion-order movement contract"
    );
    // Completion cannot excuse an unreported move. Neither Finished nor an error
    // replaces this event-replayed oracle with a packed/canonical view.
    let exact = reported;
    for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
        let t = begin(&m[0]);
        let rows = collect(&graph, order);
        report(
            &m[0],
            if order == OutEdgeOrder::Ascending {
                "final_asc"
            } else {
                "final_desc"
            },
            t,
        );
        let truth = if order == OutEdgeOrder::Ascending {
            exact.clone()
        } else {
            exact.iter().rev().copied().collect()
        };
        if order == OutEdgeOrder::Ascending {
            let mut final_rows = std::fs::File::create(out.join("final-ordinals.tsv")).unwrap();
            writeln!(final_rows, "slot\ttarget").unwrap();
            for (slot, target) in &rows {
                writeln!(final_rows, "{slot}\t{target}").unwrap();
            }
            eprintln!(
                "CROSSOVER unreported_slot_changes={}",
                rows.iter().zip(&truth).filter(|(a, b)| a != b).count()
            );
        }
        assert_rows(rows, &truth);
    }
    eprintln!(
        "CROSSOVER final identities verified={} completed={finished} stopping_error={stopping_error:?}",
        exact.len()
    );
    assert!(
        finished && stopping_error.is_none(),
        "pending/error is not successful completion"
    );
    seeded
}
