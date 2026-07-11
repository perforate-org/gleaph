use crate::lara::edge::EdgeLayout;
use crate::traits::CsrEdgeTombstone;
use crate::*;
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TestEdge(pub(crate) u32);

impl CsrEdge for TestEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self(u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.0)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self(u32::from(vid))
    }
}

impl CsrEdgeTombstone for TestEdge {
    fn tombstone_edge() -> Self {
        Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LabelledTestEdge {
    pub(crate) neighbor: u32,
    pub(crate) label: u32,
}

impl LabelledTestEdge {
    pub(crate) fn new(neighbor: u32, label: u32) -> Self {
        Self { neighbor, label }
    }
}

impl CsrEdge for LabelledTestEdge {
    const BYTES: usize = 8;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            neighbor: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            label: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        }
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.neighbor.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.label.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.neighbor)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self {
            neighbor: u32::from(vid),
            ..*self
        }
    }
}

impl CsrEdgeTombstone for LabelledTestEdge {
    fn tombstone_edge() -> Self {
        Self {
            neighbor: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            label: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UndirectedTestEdge {
    pub(crate) neighbor: u32,
    pub(crate) undirected: bool,
}

impl UndirectedTestEdge {
    pub(crate) fn new(neighbor: u32) -> Self {
        Self {
            neighbor,
            undirected: false,
        }
    }
}

impl CsrEdge for UndirectedTestEdge {
    const BYTES: usize = 5;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            neighbor: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            undirected: bytes[4] != 0,
        }
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.neighbor.to_le_bytes());
        bytes[4] = u8::from(self.undirected);
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.neighbor)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self {
            neighbor: u32::from(vid),
            ..*self
        }
    }
}

impl CsrEdgeTombstone for UndirectedTestEdge {
    fn tombstone_edge() -> Self {
        Self {
            neighbor: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            undirected: false,
        }
    }
}

impl CsrEdgeUndirected for UndirectedTestEdge {
    fn is_undirected(&self) -> bool {
        self.undirected
    }

    fn with_undirected(self, undirected: bool) -> Self {
        Self { undirected, ..self }
    }
}

pub(crate) fn vector_memory() -> VectorMemory {
    Rc::new(RefCell::new(Vec::new()))
}

/// Test-only [`Memory`] wrapper that can deterministically fail a selected
/// `grow` call while preserving all writes made before the failure.
///
/// The wrapped bytes are shared via `Rc<RefCell>`, so a graph can be built,
/// dropped after a failure, and then reopened from a fresh clone pointing at
/// the same stable bytes.
///
/// `size()` always reports whole WebAssembly pages (`bytes.len() / WASM_PAGE_SIZE`),
/// so the memory remains reopenable as long as at least one page has been
/// allocated. Tests that need sub-page precision should fill the structure to
/// just below a page boundary and then trigger a real page grow, not truncate
/// the backing buffer (truncation below a page makes `size()` return `0` and
/// breaks reopen).
#[derive(Clone, Debug)]
pub(crate) struct FailpointMemory {
    inner: Rc<RefCell<FailingGrowState>>,
}

#[derive(Debug)]
struct FailingGrowState {
    bytes: Vec<u8>,
    grow_count: usize,
    fail_at_grow: Option<usize>,
}

impl Default for FailpointMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl FailpointMemory {
    pub(crate) fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(FailingGrowState {
                bytes: Vec::new(),
                grow_count: 0,
                fail_at_grow: None,
            })),
        }
    }

    /// Configure the memory to fail the `n`th `grow` call (1-indexed).
    pub(crate) fn fail_at_grow(&self, n: usize) {
        self.inner.borrow_mut().fail_at_grow = Some(n);
    }

    /// Clear any pending grow failure.
    pub(crate) fn fail_never(&self) {
        self.inner.borrow_mut().fail_at_grow = None;
    }

    /// Returns the number of `grow` calls that have been made so far.
    #[cfg(test)]
    pub(crate) fn grow_count(&self) -> usize {
        self.inner.borrow().grow_count
    }

    /// Returns the current byte length of the backing buffer.
    ///
    /// This is only meaningful for tests that need to observe when a
    /// `Memory::grow` would be required without relying on `size()`, because
    /// `size()` reports whole WebAssembly pages and an empty-looking memory
    /// would break reopen.
    #[cfg(test)]
    pub(crate) fn byte_len(&self) -> usize {
        self.inner.borrow().bytes.len()
    }
}

impl ic_stable_structures::Memory for FailpointMemory {
    fn size(&self) -> u64 {
        self.inner.borrow().bytes.len() as u64 / crate::WASM_PAGE_SIZE
    }

    fn grow(&self, pages: u64) -> i64 {
        let current_size = self.size();
        let mut state = self.inner.borrow_mut();
        state.grow_count = state.grow_count.saturating_add(1);
        if state.fail_at_grow == Some(state.grow_count) {
            return -1;
        }
        match current_size.checked_add(pages) {
            Some(n) => {
                let max_pages = i64::MAX as u64 / crate::WASM_PAGE_SIZE;
                if n > max_pages {
                    return -1;
                }
                let target_bytes = n.checked_mul(crate::WASM_PAGE_SIZE).expect("grow overflow");
                state.bytes.resize(target_bytes as usize, 0);
                current_size as i64
            }
            None => -1,
        }
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        let state = self.inner.borrow();
        let n = offset
            .checked_add(dst.len() as u64)
            .expect("read: out of bounds");
        if n as usize > state.bytes.len() {
            panic!("read: out of bounds");
        }
        dst.copy_from_slice(&state.bytes[offset as usize..n as usize]);
    }

    unsafe fn read_unsafe(&self, offset: u64, dst: *mut u8, count: usize) {
        let state = self.inner.borrow();
        let n = offset
            .checked_add(count as u64)
            .expect("read_unsafe: out of bounds");
        if n as usize > state.bytes.len() {
            panic!("read_unsafe: out of bounds");
        }
        unsafe {
            std::ptr::copy(state.bytes.as_ptr().add(offset as usize), dst, count);
        }
    }

    fn write(&self, offset: u64, src: &[u8]) {
        let mut state = self.inner.borrow_mut();
        let n = offset
            .checked_add(src.len() as u64)
            .expect("write: out of bounds");
        if n as usize > state.bytes.len() {
            panic!("write: out of bounds");
        }
        state.bytes[offset as usize..n as usize].copy_from_slice(src);
    }
}

/// Fifteen fresh failpoint memories in [`LabeledLaraGraph`] constructor order.
#[cfg(test)]
pub(crate) fn failpoint_labeled_memories() -> [FailpointMemory; 15] {
    std::array::from_fn(|_| FailpointMemory::new())
}

#[allow(clippy::type_complexity)]
pub(crate) fn labeled_lara_memories() -> (
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
) {
    (
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
    )
}

pub(crate) type TestBidirectionalLaraGraph<E> = BidirectionalLaraGraph<E, Vertex, VectorMemory>;

pub(crate) type TestDeferredBidirectionalLaraGraph<E> =
    crate::DeferredBidirectionalLaraGraph<E, Vertex, VectorMemory>;

pub(crate) fn test_graph(
    elem_capacity: u64,
    segment_size: u32,
    starts: &[u64],
) -> LaraGraph<TestEdge, Vertex, VectorMemory> {
    lara_test_graph(elem_capacity, segment_size, starts)
}

pub(crate) fn lara_test_graph<E>(
    elem_capacity: u64,
    segment_size: u32,
    starts: &[u64],
) -> LaraGraph<E, Vertex, VectorMemory>
where
    E: CsrEdge,
{
    let graph = LaraGraph::new(
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        elem_capacity,
        segment_size,
        0,
    )
    .unwrap();
    for &base_slot_start in starts {
        graph
            .push_vertex(Vertex::from_parts(base_slot_start, 0, 0, -1, false))
            .unwrap();
    }
    graph
}

pub(crate) fn bidirectional_test_graph<E>(starts: &[u64]) -> TestBidirectionalLaraGraph<E>
where
    E: CsrEdge,
{
    let graph = BidirectionalLaraGraph::new(
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        32,
        4,
        0,
    )
    .unwrap();
    for &base_slot_start in starts {
        graph
            .push_vertex(Vertex::from_parts(base_slot_start, 0, 0, -1, false))
            .unwrap();
    }
    graph
}

pub(crate) fn deferred_bidirectional_test_graph<E>(
    elem_capacity: u64,
    segment_size: u32,
    starts: &[u64],
) -> TestDeferredBidirectionalLaraGraph<E>
where
    E: CsrEdge + CsrEdgeTombstone,
{
    let graph = crate::DeferredBidirectionalLaraGraph::new_with_config(
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        elem_capacity,
        segment_size,
        0,
        crate::DeferredConfig {
            leaf_dirty_density: 0.0,
            log_urgent_ratio: 0.80,
        },
    )
    .unwrap();
    for &base_slot_start in starts {
        graph
            .push_vertex(Vertex::from_parts(base_slot_start, 0, 0, -1, false))
            .unwrap();
    }
    graph
}

pub(crate) fn assert_vertex_capacity_invariants(graph: &LaraGraph<TestEdge, Vertex, VectorMemory>) {
    use crate::traits::CsrVertex;
    let layout: EdgeLayout = graph.edges().header().into();
    let mut owned_spans = Vec::new();
    for vidx in 0..graph.vertices().len() {
        let v = graph.vertices().get(VertexId::from(vidx));
        let end = graph
            .edges()
            .slab_window_exclusive_end(&layout, graph.vertices(), vidx, &v);
        assert!(
            end >= v.base_slot_start(),
            "vertex {vidx}: csr window end {end} before base {}",
            v.base_slot_start()
        );
        if v.log_head() < 0 {
            assert!(
                v.base_slot_start().saturating_add(u64::from(v.degree())) <= end,
                "vertex {vidx}: live slab prefix extends past csr window end {end}"
            );
        }
        if end > v.base_slot_start() {
            owned_spans.push((vidx, v.base_slot_start(), end));
        }
    }

    for free in graph.edges().free_span_store().spans() {
        let free_start = free.start_slot;
        let free_end = free.start_slot.saturating_add(free.len);
        for &(vidx, owned_start, owned_end) in &owned_spans {
            assert!(
                !spans_overlap(free_start, free_end, owned_start, owned_end),
                "free span [{free_start}, {free_end}) overlaps vertex {vidx} csr window [{owned_start}, {owned_end})"
            );
        }
    }
}

fn spans_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

pub(crate) fn deferred_test_graph(
    elem_capacity: u64,
    segment_size: u32,
    starts: &[u64],
) -> DeferredLaraGraph<TestEdge, Vertex, VectorMemory> {
    let graph = DeferredLaraGraph::new(
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        elem_capacity,
        segment_size,
        0,
    )
    .unwrap();
    for &base_slot_start in starts {
        graph
            .push_vertex(Vertex::from_parts(base_slot_start, 0, 0, -1, false))
            .unwrap();
    }
    graph
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failpoint_memory_preserves_writes_and_fails_selected_grow() {
        let mem = FailpointMemory::new();
        crate::safe_write(&mem, 0, b"hello").unwrap();

        // Fail the next grow, which is needed for any write beyond the first page.
        mem.fail_at_grow(mem.grow_count().saturating_add(1));
        let result = crate::safe_write(&mem, crate::WASM_PAGE_SIZE, &[1]);
        assert!(result.is_err(), "expected the selected grow to fail");

        // Pre-failure bytes must still be readable.
        let mut buf = [0u8; 5];
        mem.read(0, &mut buf);
        assert_eq!(&buf, b"hello");

        // Retry without the failure succeeds.
        mem.fail_never();
        crate::safe_write(&mem, crate::WASM_PAGE_SIZE, &[1]).unwrap();

        // A clone shares the same bytes, proving reopen from the same stable state works.
        let reopened = mem.clone();
        let mut buf2 = [0u8; 1];
        reopened.read(crate::WASM_PAGE_SIZE, &mut buf2);
        assert_eq!(buf2[0], 1);
    }
}
