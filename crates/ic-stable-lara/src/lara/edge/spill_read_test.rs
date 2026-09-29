use super::*;
use crate::VertexId;
use crate::traits::{CsrEdge, CsrEdgeTombstone};
use ic_stable_structures::Memory;
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Default)]
struct CountMemory {
    inner: crate::VectorMemory,
    reads: Rc<Cell<usize>>,
    bytes: Rc<Cell<usize>>,
}
impl Memory for CountMemory {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn grow(&self, pages: u64) -> i64 {
        self.inner.grow(pages)
    }
    fn read(&self, offset: u64, bytes: &mut [u8]) {
        self.reads.set(self.reads.get() + 1);
        self.bytes.set(self.bytes.get() + bytes.len());
        self.inner.read(offset, bytes);
    }
    unsafe fn read_unsafe(&self, offset: u64, bytes: *mut u8, len: usize) {
        self.reads.set(self.reads.get() + 1);
        self.bytes.set(self.bytes.get() + len);
        unsafe { self.inner.read_unsafe(offset, bytes, len) }
    }
    fn write(&self, offset: u64, bytes: &[u8]) {
        self.inner.write(offset, bytes)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row<const N: usize>(u32);
impl<const N: usize> CsrEdge for Row<N> {
    const BYTES: usize = N;
    fn read_from(bytes: &[u8]) -> Self {
        Self(u32::from_le_bytes(bytes[..4].try_into().unwrap()))
    }
    fn write_to(&self, bytes: &mut [u8]) {
        bytes.fill(0);
        bytes[..4].copy_from_slice(&self.0.to_le_bytes());
    }
    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.0)
    }
    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self(u32::from(vid))
    }
}
impl<const N: usize> CsrEdgeTombstone for Row<N> {
    fn tombstone_edge() -> Self {
        Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
    }
}

fn replay<const N: usize>() {
    let memory = CountMemory::default();
    let store = EdgeStore::<Row<N>, _>::new(
        CountMemory::default(),
        CountMemory::default(),
        memory.clone(),
        CountMemory::default(),
        CountMemory::default(),
        CountMemory::default(),
        8,
        1,
        0,
    )
    .unwrap();
    let len = OUT_EDGE_SLAB_CHUNK_SLOTS * 2 + 3;
    let run = store.spill.allocate(len).unwrap();
    for i in 0..len {
        let edge = if [
            0,
            OUT_EDGE_SLAB_CHUNK_SLOTS - 1,
            OUT_EDGE_SLAB_CHUNK_SLOTS,
            len - 1,
        ]
        .contains(&i)
        {
            Row::<N>::tombstone_edge()
        } else {
            Row::<N>(i)
        };
        let mut bytes = vec![0; N];
        edge.write_to(&mut bytes);
        store.spill.write_row(run, i, &bytes).unwrap();
    }
    memory.reads.set(0);
    let replay = store.spill_replay_entries(run).unwrap();
    let expected: Vec<_> = (0..len)
        .map(|i| {
            if [
                0,
                OUT_EDGE_SLAB_CHUNK_SLOTS - 1,
                OUT_EDGE_SLAB_CHUNK_SLOTS,
                len - 1,
            ]
            .contains(&i)
            {
                None
            } else {
                Some(Row::<N>(i))
            }
        })
        .collect();
    assert_eq!(replay, expected);
    // One length snapshot plus one checked-header and one payload read per chunk.
    assert_eq!(memory.reads.get(), 1 + 2 * 3);
}
fn skipping_fixture<const N: usize>() -> (
    EdgeStore<Row<N>, CountMemory>,
    crate::labeled::spill_run_store::SpillRunId,
) {
    let store = EdgeStore::<Row<N>, _>::new(
        CountMemory::default(),
        CountMemory::default(),
        CountMemory::default(),
        CountMemory::default(),
        CountMemory::default(),
        CountMemory::default(),
        8,
        1,
        0,
    )
    .unwrap();
    let run = store
        .spill
        .allocate(OUT_EDGE_SLAB_CHUNK_SLOTS * 2 + 3)
        .unwrap();
    for i in 0..OUT_EDGE_SLAB_CHUNK_SLOTS * 2 + 3 {
        let mut bytes = vec![0; N];
        Row::<N>(100 + i).write_to(&mut bytes);
        store.spill.write_row(run, i, &bytes).unwrap();
    }
    (store, run)
}
#[test]
fn spill_iterator_fresh_nth_loads_requested_chunk() {
    fn check<const N: usize>() {
        let (store, run) = skipping_fixture::<N>();
        assert_eq!(
            store
                .spill_edges_iter(run)
                .unwrap()
                .nth(1)
                .unwrap()
                .unwrap(),
            Row::<N>(101)
        );
    }
    check::<4>();
    check::<10>();
}
#[test]
fn spill_iterator_nth_crosses_loaded_chunk() {
    fn check<const N: usize>() {
        let (store, run) = skipping_fixture::<N>();
        let mut iter = store.spill_edges_iter(run).unwrap();
        assert_eq!(iter.next().unwrap().unwrap(), Row::<N>(100));
        assert_eq!(
            iter.nth(OUT_EDGE_SLAB_CHUNK_SLOTS as usize)
                .unwrap()
                .unwrap(),
            Row::<N>(101 + OUT_EDGE_SLAB_CHUNK_SLOTS)
        );
    }
    check::<4>();
    check::<10>();
}
#[test]
fn spill_iterator_last_loads_partial_tail() {
    fn check<const N: usize>() {
        let (store, run) = skipping_fixture::<N>();
        assert_eq!(
            store
                .spill_edges_iter(run)
                .unwrap()
                .last()
                .unwrap()
                .unwrap(),
            Row::<N>(102 + OUT_EDGE_SLAB_CHUNK_SLOTS * 2)
        );
    }
    check::<4>();
    check::<10>();
}

#[test]
fn bucket_append_preflight_checks_width_and_keeps_spill_prefix() {
    use crate::{
        labeled::{BucketLabelKey, record::LabelBucket},
        traits::CsrVertex,
    };
    let bucket = LabelBucket::from_parts(BucketLabelKey::from_raw(2), 0, 7, 5);
    assert!(
        bucket
            .with_degree(u32::MAX)
            .try_grow_packed_slab_by_one()
            .is_err()
    );
    assert!(
        bucket
            .with_stored_slots(u32::MAX)
            .try_grow_packed_slab_by_one()
            .is_err()
    );
    let spilled = bucket.try_with_spill_run(0).unwrap();
    assert!(
        spilled
            .with_degree(u32::MAX)
            .try_grow_packed_slab_by_one()
            .is_err()
    );
    let grown = spilled.try_grow_packed_slab_by_one().unwrap();
    assert_eq!(grown.degree(), 8);
    assert_eq!(grown.stored_degree(), 5);
    assert_eq!(grown.spill_run(), 0);
}

#[test]
fn core_vertex_append_preflight_rejects_width_overflow() {
    use crate::{lara::vertex::Vertex, traits::CsrVertex};
    for (live, stored) in [(u32::MAX, u32::MAX), (1, u32::MAX)] {
        let vertex = Vertex::from_parts(0, live, stored, -1, false);
        assert!(vertex.try_grow_packed_slab_by_one().is_err());
        assert_eq!(vertex.degree(), live);
        assert_eq!(vertex.stored_degree(), stored);
    }
}

fn bulk_row<const N: usize>() {
    use crate::{
        labeled::{BucketLabelKey, record::LabelBucket},
        traits::CsrVertex,
        traverse::{TraversalOrder, TraversalWindow},
    };
    use std::ops::ControlFlow;

    for prefix in [0, 5, 67] {
        let memories: [CountMemory; 6] = std::array::from_fn(|_| CountMemory::default());
        let store = EdgeStore::<Row<N>, _>::new(
            memories[0].clone(),
            memories[1].clone(),
            memories[2].clone(),
            memories[3].clone(),
            memories[4].clone(),
            memories[5].clone(),
            128,
            1,
            0,
        )
        .unwrap();
        store.set_elem_capacity(128).unwrap();
        let mut bucket = LabelBucket::from_parts(BucketLabelKey::from_raw(2), 7, 67, prefix);
        for slot in 0..prefix {
            store
                .write_slot(7 + u64::from(slot), Row::<N>(1000 + slot))
                .unwrap();
        }
        let run = if prefix < 67 {
            let run = store.spill.allocate(67 - prefix).unwrap();
            for slot in prefix..67 {
                let mut bytes = vec![0; N];
                Row::<N>(1000 + slot).write_to(&mut bytes);
                store.spill.write_row(run, slot - prefix, &bytes).unwrap();
            }
            bucket = bucket.try_with_spill_run(run.get()).unwrap();
            Some(run)
        } else {
            None
        };
        let snapshot = || {
            memories
                .iter()
                .map(|m| m.inner.borrow().clone())
                .collect::<Vec<_>>()
        };
        for holes in [false, true] {
            let dead = [0, 4, 5, 31, 32, 66];
            if holes {
                for slot in dead {
                    if slot < prefix {
                        store
                            .write_slot(7 + u64::from(slot), Row::<N>::tombstone_edge())
                            .unwrap();
                    } else {
                        let mut bytes = vec![0; N];
                        Row::<N>::tombstone_edge().write_to(&mut bytes);
                        store
                            .spill
                            .write_row(run.unwrap(), slot - prefix, &bytes)
                            .unwrap();
                    }
                }
                bucket = bucket.with_degree(67 - dead.len() as u32);
            }
            let before = snapshot();
            for order in [TraversalOrder::Ascending, TraversalOrder::Descending] {
                let slots: Vec<_> = match order {
                    TraversalOrder::Ascending => (0..67u32).collect(),
                    TraversalOrder::Descending => (0..67u32).rev().collect(),
                };
                for window in [
                    TraversalWindow::unbounded(),
                    TraversalWindow::new(0, Some(0)),
                    TraversalWindow::new(0, Some(3)),
                    TraversalWindow::new(3, Some(4)),
                    TraversalWindow::new(31, Some(4)),
                    TraversalWindow::new(65, Some(10)),
                    TraversalWindow::new(67, None),
                    TraversalWindow::new(u32::MAX, Some(u32::MAX)),
                ] {
                    let selected: Vec<_> = slots
                        .iter()
                        .copied()
                        .skip(window.offset as usize)
                        .take(window.limit.unwrap_or(u32::MAX) as usize)
                        .collect();
                    let expected: Vec<_> = selected
                        .iter()
                        .copied()
                        .filter(|s| !holes || !dead.contains(s))
                        .map(|s| (s, Row::<N>(1000 + s)))
                        .collect();
                    for m in &memories {
                        m.reads.set(0);
                        m.bytes.set(0);
                    }
                    let mut seen = Vec::new();
                    assert_eq!(
                        store
                            .visit_row_edges(&bucket, order, window, |slot, edge| {
                                seen.push((slot, edge));
                                ControlFlow::<()>::Continue(())
                            })
                            .unwrap(),
                        ControlFlow::Continue(())
                    );
                    assert_eq!(
                        seen, expected,
                        "width={N} prefix={prefix} holes={holes} order={order:?} window={window:?}"
                    );
                    let slab_rows = selected.iter().filter(|s| **s < prefix).count();
                    let spill_rows = selected.len() - slab_rows;
                    assert_eq!(memories[1].reads.get(), usize::from(slab_rows > 0));
                    assert_eq!(memories[1].bytes.get(), slab_rows * N);
                    let length_reads = usize::from(run.is_some() && !window.is_empty());
                    assert_eq!(
                        memories[2].reads.get(),
                        length_reads + 2 * usize::from(spill_rows > 0)
                    );
                    assert_eq!(
                        memories[2].bytes.get(),
                        4 * (length_reads + usize::from(spill_rows > 0)) + spill_rows * N
                    );
                    for i in [0, 3, 4, 5] {
                        assert_eq!(memories[i].reads.get(), 0);
                    }
                }
                let expected: Vec<_> = slots
                    .iter()
                    .copied()
                    .filter(|s| !holes || !dead.contains(s))
                    .take(3)
                    .map(|s| (s, Row::<N>(1000 + s)))
                    .collect();
                let mut seen = Vec::new();
                let result = store
                    .visit_row_edges(
                        &bucket,
                        order,
                        TraversalWindow::unbounded(),
                        |slot, edge| {
                            seen.push((slot, edge));
                            if seen.len() == 3 {
                                ControlFlow::Break(0xfeed_u32)
                            } else {
                                ControlFlow::Continue(())
                            }
                        },
                    )
                    .unwrap();
                assert_eq!(result, ControlFlow::Break(0xfeed));
                assert_eq!(seen, expected);
            }
            assert_eq!(snapshot(), before, "readers never mutate stable state");
        }
        if let Some(run) = run {
            let invalid = crate::lara::vertex::Vertex::from_parts(7, 1, 66 - prefix, -1, false)
                .try_with_spill_run(run.get())
                .unwrap();
            let before = snapshot();
            let mut callbacks = 0;
            let result = store.visit_row_edges(
                &invalid,
                TraversalOrder::Descending,
                TraversalWindow::unbounded(),
                |_, _| {
                    callbacks += 1;
                    ControlFlow::<()>::Continue(())
                },
            );
            assert!(matches!(
                result,
                Err(crate::lara::operation_error::LaraOperationError::CollectAllocationOverflow)
            ));
            assert_eq!(callbacks, 0);
            assert_eq!(snapshot(), before);
        }
    }
}

#[test]
fn bulk_row_four_byte_reads_are_bounded_and_ordered() {
    bulk_row::<4>();
}

#[test]
fn bulk_row_ten_byte_reads_are_bounded_and_ordered() {
    bulk_row::<10>();
}

#[test]
fn spill_replay_reads_chunks_not_scalar_rows() {
    replay::<4>();
}
#[test]
fn spill_replay_reads_chunks_for_ten_byte_rows() {
    replay::<10>();
}
