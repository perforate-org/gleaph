use crate::traits::CsrEdgeTombstone;
use crate::*;
use std::{cell::RefCell, rc::Rc};

/// Per-region backing I/O accounting shared by storage tests and instruction probes.
/// Clones share both backing and counters; `sibling` shares only byte counters.
#[derive(Clone, Default)]
pub(crate) struct CountMemory {
    pub(crate) inner: crate::VectorMemory,
    pub(crate) reads: Rc<std::cell::Cell<usize>>,
    pub(crate) bytes: Rc<std::cell::Cell<usize>>,
    pub(crate) write_calls: Rc<std::cell::Cell<usize>>,
    pub(crate) writes: Rc<std::cell::Cell<usize>>,
    pub(crate) grow_calls: Rc<std::cell::Cell<usize>>,
    pub(crate) grown_pages: Rc<std::cell::Cell<u64>>,
}

impl CountMemory {
    pub(crate) fn counted() -> (Self, Rc<std::cell::Cell<usize>>, Rc<std::cell::Cell<usize>>) {
        let memory = Self::default();
        (memory.clone(), memory.bytes.clone(), memory.writes.clone())
    }

    /// Fresh backing with shared read/write byte totals, preserving graph fixture accounting.
    pub(crate) fn sibling(
        reads: &Rc<std::cell::Cell<usize>>,
        writes: &Rc<std::cell::Cell<usize>>,
    ) -> Self {
        Self {
            bytes: reads.clone(),
            writes: writes.clone(),
            ..Self::default()
        }
    }
}

impl ic_stable_structures::Memory for CountMemory {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn grow(&self, pages: u64) -> i64 {
        self.grow_calls.set(self.grow_calls.get() + 1);
        let previous = self.inner.grow(pages);
        if previous >= 0 {
            self.grown_pages.set(self.grown_pages.get() + pages);
        }
        previous
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
        self.write_calls.set(self.write_calls.get() + 1);
        self.writes.set(self.writes.get() + bytes.len());
        self.inner.write(offset, bytes)
    }
}

#[cfg(test)]
mod count_memory_tests {
    use super::CountMemory;
    use ic_stable_structures::Memory;
    use std::rc::Rc;

    #[test]
    fn backing_counters_distinguish_calls_bytes_and_shared_clones() {
        let memory = CountMemory::default();
        let clone = memory.clone();
        assert!(Rc::ptr_eq(&memory.inner, &clone.inner));
        assert_eq!(clone.grow(1), 0);
        clone.write(7, &[1, 2, 3]);
        let mut safe = [0; 3];
        memory.read(7, &mut safe);
        let mut raw = [0; 2];
        unsafe { clone.read_unsafe(8, raw.as_mut_ptr(), raw.len()) };
        assert_eq!(safe, [1, 2, 3]);
        assert_eq!(raw, [2, 3]);
        assert_eq!(
            (
                memory.reads.get(),
                memory.bytes.get(),
                memory.write_calls.get(),
                memory.writes.get()
            ),
            (2, 5, 1, 3),
        );
        assert_eq!(memory.grow_calls.get(), 1);
        assert_eq!(memory.grown_pages.get(), 1);
        assert_eq!(memory.size(), 1);
        assert_eq!(clone.grow(0), 1);
        assert_eq!(memory.grow_calls.get(), 2);
        assert_eq!(memory.grown_pages.get(), 1);
    }
}

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

/// Sixteen fresh failpoint memories in [`LabeledLaraGraph`] constructor order.
/// The 16th memory (added in Plan 0318 §Step 2) backs the per-orientation
/// `LtbRawBlockStore` (LARA Tree Block store).
#[cfg(test)]
pub(crate) fn failpoint_labeled_memories() -> [FailpointMemory; 16] {
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
        vector_memory(),
    )
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
