use super::*;
use crate::VertexId;
use crate::traits::{CsrEdge, CsrEdgeTombstone};
use ic_stable_structures::Memory;
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Default)]
struct CountMemory {
    inner: crate::VectorMemory,
    reads: Rc<Cell<usize>>,
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
        self.inner.read(offset, bytes);
    }
    unsafe fn read_unsafe(&self, offset: u64, bytes: *mut u8, len: usize) {
        self.reads.set(self.reads.get() + 1);
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

#[test]
fn spill_replay_reads_chunks_not_scalar_rows() {
    replay::<4>();
}
#[test]
fn spill_replay_reads_chunks_for_ten_byte_rows() {
    replay::<10>();
}
