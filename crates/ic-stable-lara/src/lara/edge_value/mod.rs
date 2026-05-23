//! Log-backed byte CSR for per-label edge values (separate from target rows).

mod log;

use crate::lara::edge::free_span::FreeSpanStore;
use crate::lara::edge_value::log::{HeaderV1 as ValueLogHeaderV1, PAYLOAD_BYTES, ValueLogStore};
use crate::slab_index::{byte_exclusive_end_fits, byte_offset_fits, checked_add_byte_offset};
use crate::{GrowFailed, read_u64, safe_write, types::Address, write_u64};
use ic_stable_structures::Memory;
use std::{cell::Cell, fmt};

pub use log::{InitError as ValueLogInitError, ValueLogStore as ValueOverflowLogStore};

/// Magic bytes for the value byte slab.
pub const MAGIC: [u8; 3] = *b"LVG";
/// Current value slab layout version.
pub const LAYOUT_VERSION: u8 = 1;
/// Persisted header size in bytes.
pub const HEADER_SIZE: u64 = 64;

const BYTE_CAPACITY_OFFSET: u64 = 4;
const SLAB_OCCUPIED_TAIL_OFFSET: u64 = 36;

fn byte_offset(addr: u64) -> u64 {
    HEADER_SIZE + addr
}

/// Persisted V1 value-slab header (byte-addressed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    pub magic: [u8; 3],
    pub version: u8,
    /// Exclusive end of the byte address space (max valid offset + 1).
    pub byte_capacity: u64,
    pub slab_occupied_tail: u64,
}

impl HeaderV1 {
    pub fn new(byte_capacity: u64) -> Self {
        Self {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            byte_capacity,
            slab_occupied_tail: 0,
        }
    }
}

/// Errors when reopening a value slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    BadMagic { actual: [u8; 3] },
    IncompatibleVersion(u8),
    InvalidLayout,
    ByteCapacityOverflow,
    FreeSpansInvalid,
    ValueLog(log::InitError),
    ValueLogLayoutMismatch,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => write!(f, "bad value slab magic {actual:?}"),
            Self::IncompatibleVersion(v) => write!(f, "unsupported value slab version {v}"),
            Self::InvalidLayout => write!(f, "invalid value slab layout"),
            Self::ByteCapacityOverflow => write!(f, "value byte_capacity exceeds 40-bit space"),
            Self::FreeSpansInvalid => write!(f, "value free-span init failed"),
            Self::ValueLog(e) => write!(f, "value log init failed: {e}"),
            Self::ValueLogLayoutMismatch => {
                write!(f, "value log segment_count does not match edge store")
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Stable byte slab for edge values.
#[derive(Clone, Debug)]
pub struct ValueByteSlabStore<M: Memory> {
    memory: M,
}

impl<M: Memory> ValueByteSlabStore<M> {
    pub fn new(memory: M, header: HeaderV1) -> Result<Self, GrowFailed> {
        let store = Self { memory };
        store.grow_for_header(&header)?;
        store.write_header(&header);
        Ok(store)
    }

    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Err(InitError::InvalidLayout);
        }
        let store = Self { memory };
        let header = store.read_header()?;
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        if !byte_exclusive_end_fits(header.byte_capacity) {
            return Err(InitError::ByteCapacityOverflow);
        }
        Ok(store)
    }

    pub fn into_memory(self) -> M {
        self.memory
    }

    pub fn header(&self) -> Result<HeaderV1, InitError> {
        self.read_header()
    }

    pub fn write_header(&self, h: &HeaderV1) {
        self.memory.write(0, &h.magic);
        self.memory.write(3, &[h.version]);
        write_u64(
            &self.memory,
            Address::from(BYTE_CAPACITY_OFFSET),
            h.byte_capacity,
        );
        write_u64(
            &self.memory,
            Address::from(SLAB_OCCUPIED_TAIL_OFFSET),
            h.slab_occupied_tail,
        );
    }

    pub fn set_byte_capacity(&self, n: u64) -> Result<(), GrowFailed> {
        if !byte_exclusive_end_fits(n) {
            return Err(GrowFailed {
                current_size: self.memory.size(),
                delta: 0,
            });
        }
        let mut h = self.header().map_err(|_| GrowFailed {
            current_size: self.memory.size(),
            delta: 0,
        })?;
        h.byte_capacity = n;
        self.grow_for_header(&h)?;
        write_u64(&self.memory, Address::from(BYTE_CAPACITY_OFFSET), n);
        Ok(())
    }

    pub fn read_bytes(&self, offset: u64, out: &mut [u8]) {
        self.memory.read(byte_offset(offset), out);
    }

    pub fn write_bytes(&self, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
        safe_write(&self.memory, byte_offset(offset), bytes)
    }

    fn read_header(&self) -> Result<HeaderV1, InitError> {
        let mut magic = [0u8; 3];
        self.memory.read(0, &mut magic);
        if magic != MAGIC {
            return Err(InitError::BadMagic { actual: magic });
        }
        let mut version = [0u8; 1];
        self.memory.read(3, &mut version);
        Ok(HeaderV1 {
            magic,
            version: version[0],
            byte_capacity: read_u64(&self.memory, Address::from(BYTE_CAPACITY_OFFSET)),
            slab_occupied_tail: read_u64(&self.memory, Address::from(SLAB_OCCUPIED_TAIL_OFFSET)),
        })
    }

    fn grow_for_header(&self, h: &HeaderV1) -> Result<(), GrowFailed> {
        let need = HEADER_SIZE.saturating_add(h.byte_capacity);
        if need == 0 {
            return Ok(());
        }
        safe_write(&self.memory, need - 1, &[0])
    }
}

/// Combined stable edge-value storage for labeled graphs.
pub struct EdgeValueStore<M: Memory> {
    slab: ValueByteSlabStore<M>,
    log: ValueLogStore<M>,
    free_spans: FreeSpanStore<M>,
    header: Cell<HeaderV1>,
}

impl<M: Memory> EdgeValueStore<M> {
    pub fn new(
        slab_memory: M,
        value_log: M,
        free_spans: M,
        free_span_by_start: M,
        byte_capacity: u64,
        segment_count: u32,
    ) -> Result<Self, GrowFailed> {
        if !byte_exclusive_end_fits(byte_capacity) {
            return Err(GrowFailed {
                current_size: slab_memory.size(),
                delta: 0,
            });
        }
        let header = HeaderV1::new(byte_capacity);
        let slab = ValueByteSlabStore::new(slab_memory, header)?;
        let log = ValueLogStore::new(value_log, ValueLogHeaderV1::new(segment_count))?;
        let free_spans = FreeSpanStore::new(free_spans, free_span_by_start)?;
        Ok(Self {
            slab,
            log,
            free_spans,
            header: Cell::new(header),
        })
    }

    pub fn init(
        slab_memory: M,
        value_log: M,
        free_spans: M,
        free_span_by_start: M,
        byte_capacity: u64,
        edge_segment_count: u32,
    ) -> Result<Self, InitError> {
        let slab = if slab_memory.size() == 0 {
            ValueByteSlabStore::new(slab_memory, HeaderV1::new(byte_capacity))
                .map_err(|_| InitError::InvalidLayout)?
        } else {
            ValueByteSlabStore::init(slab_memory)?
        };
        let log = if value_log.size() == 0 {
            ValueLogStore::new(value_log, ValueLogHeaderV1::new(edge_segment_count))
                .map_err(|_| InitError::InvalidLayout)?
        } else {
            ValueLogStore::init(value_log).map_err(InitError::ValueLog)?
        };
        if log.header().segment_count != edge_segment_count {
            return Err(InitError::ValueLogLayoutMismatch);
        }
        let free_spans = if free_spans.size() == 0 {
            FreeSpanStore::new(free_spans, free_span_by_start)
                .map_err(|_| InitError::InvalidLayout)?
        } else {
            FreeSpanStore::init(free_spans, free_span_by_start)
                .map_err(|_| InitError::FreeSpansInvalid)?
        };
        let header = slab.header()?;
        Ok(Self {
            slab,
            log,
            free_spans,
            header: Cell::new(header),
        })
    }

    pub(crate) fn grow_segment_count_to(&self, new_count: u32) -> Result<(), GrowFailed> {
        self.log.grow_segment_count_to(new_count)
    }

    pub(crate) fn release_value_log_segment(&self, leaf_segment: u32) -> Result<(), GrowFailed> {
        self.log.release_segment(leaf_segment)
    }

    pub(crate) fn write_value_log_entry(
        &self,
        leaf_segment: u32,
        entry_idx: u32,
        prev_head: i32,
        src: i32,
        width: u8,
        value_bytes: &[u8],
    ) -> Result<(), GrowFailed> {
        let mut payload = [0u8; PAYLOAD_BYTES];
        let n = usize::from(width).min(PAYLOAD_BYTES).min(value_bytes.len());
        payload[..n].copy_from_slice(&value_bytes[..n]);
        let h = self.log.header();
        self.log
            .write_entry_with_header(&h, leaf_segment, entry_idx, prev_head, src, &payload)?;
        self.log.write_idx_at_least(
            leaf_segment,
            i32::try_from(entry_idx).unwrap_or(i32::MAX) + 1,
        );
        Ok(())
    }

    pub(crate) fn read_value_log_entry(
        &self,
        leaf_segment: u32,
        entry_idx: u32,
        width: u8,
        out: &mut [u8],
    ) {
        let mut payload = [0u8; PAYLOAD_BYTES];
        let h = self.log.header();
        self.log
            .read_entry_with_header(&h, leaf_segment, entry_idx, &mut payload);
        let n = usize::from(width).min(out.len()).min(PAYLOAD_BYTES);
        out[..n].copy_from_slice(&payload[..n]);
    }

    /// Returns value-log entry indices from oldest to newest by walking `log_head`.
    pub(crate) fn value_log_chain_asc_indices(&self, leaf_segment: u32, log_head: i32) -> Vec<u32> {
        if log_head < 0 {
            return Vec::new();
        }
        let mut chain = Vec::new();
        let mut cur = log_head;
        while cur >= 0 {
            chain.push(cur as u32);
            let mut scratch = [0u8; PAYLOAD_BYTES];
            let h = self.log.header();
            let (prev, _) =
                self.log
                    .read_entry_with_header(&h, leaf_segment, cur as u32, &mut scratch);
            cur = prev;
        }
        chain.reverse();
        chain
    }

    /// Reads the value stored at `asc_log_index` in oldest-to-newest log order.
    pub(crate) fn read_value_log_asc_index(
        &self,
        leaf_segment: u32,
        log_head: i32,
        asc_log_index: u32,
        width: u8,
        out: &mut [u8],
    ) {
        if log_head < 0 || width == 0 {
            return;
        }
        let mut chain = Vec::new();
        let mut cur = log_head;
        while cur >= 0 {
            chain.push(cur as u32);
            let mut payload = [0u8; PAYLOAD_BYTES];
            let h = self.log.header();
            let (prev, _) =
                self.log
                    .read_entry_with_header(&h, leaf_segment, cur as u32, &mut payload);
            cur = prev;
        }
        chain.reverse();
        if let Some(&idx) = chain.get(asc_log_index as usize) {
            self.read_value_log_entry(leaf_segment, idx, width, out);
        }
    }

    pub fn header(&self) -> HeaderV1 {
        self.header.get()
    }

    pub fn byte_capacity(&self) -> u64 {
        self.header().byte_capacity
    }

    pub fn set_byte_capacity(&self, end: u64) -> Result<(), GrowFailed> {
        self.slab.set_byte_capacity(end)?;
        let mut h = self.header();
        h.byte_capacity = end;
        self.header.set(h);
        Ok(())
    }

    pub fn read_bytes(&self, offset: u64, out: &mut [u8]) {
        debug_assert!(byte_offset_fits(offset));
        self.slab.read_bytes(offset, out);
    }

    pub fn write_bytes(&self, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
        if bytes.is_empty() {
            return Ok(());
        }
        let end = checked_add_byte_offset(offset, bytes.len() as u64).ok_or(GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })?;
        if end > self.byte_capacity() {
            self.set_byte_capacity(end.max(self.byte_capacity()))?;
        }
        self.slab.write_bytes(offset, bytes)?;
        let mut h = self.header();
        if end > h.slab_occupied_tail {
            h.slab_occupied_tail = end;
            self.slab.write_header(&h);
            self.header.set(h);
        }
        Ok(())
    }

    pub fn read_value_slot(&self, offset: u64, width: u8, out: &mut [u8]) {
        debug_assert_eq!(out.len(), usize::from(width));
        if width == 0 {
            return;
        }
        self.read_bytes(offset, out);
    }

    pub fn write_value_slot(&self, offset: u64, width: u8, bytes: &[u8]) -> Result<(), GrowFailed> {
        debug_assert_eq!(bytes.len(), usize::from(width));
        if width == 0 {
            return Ok(());
        }
        self.write_bytes(offset, bytes)
    }

    pub fn write_range(&self, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
        self.write_bytes(offset, bytes)
    }

    /// Allocates a byte span, preferring the free list then bumping the occupied tail.
    pub fn allocate_byte_span(&self, len: u64) -> Result<u64, GrowFailed> {
        if len == 0 {
            return Ok(0);
        }
        if let Some(span) = self.free_spans.take_best_fit(len).map_err(|_| GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })? {
            return Ok(span.start_slot);
        }
        self.append_byte_span(len)
    }

    /// Grows a span when it ends at the occupied tail (no free-list churn).
    pub fn grow_byte_span_in_place(
        &self,
        offset: u64,
        old_len: u64,
        new_len: u64,
    ) -> Result<bool, GrowFailed> {
        if new_len <= old_len {
            return Ok(true);
        }
        let tail = self.header().slab_occupied_tail;
        if offset.saturating_add(old_len) != tail {
            return Ok(false);
        }
        let pad_start = offset.checked_add(old_len).ok_or(GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })?;
        let pad_len = new_len.saturating_sub(old_len);
        if pad_len > 0 {
            self.write_bytes(
                pad_start,
                &vec![
                    0u8;
                    usize::try_from(pad_len).map_err(|_| GrowFailed {
                        current_size: self.byte_capacity(),
                        delta: 0,
                    })?
                ],
            )?;
        }
        let mut h = self.header();
        h.slab_occupied_tail = offset.checked_add(new_len).ok_or(GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })?;
        self.slab.write_header(&h);
        self.header.set(h);
        Ok(true)
    }

    /// Returns a retired byte range to the free list regardless of occupied tail.
    pub fn retire_byte_span(&self, offset: u64, len: u64) -> Result<(), GrowFailed> {
        if len == 0 {
            return Ok(());
        }
        self.free_spans
            .release_span(offset, len)
            .map_err(|_| GrowFailed {
                current_size: self.byte_capacity(),
                delta: 0,
            })
    }

    /// Returns retired byte ranges to the free list.
    ///
    /// Spans still covered by [`HeaderV1::slab_occupied_tail`] are ignored so a
    /// failed in-place grow cannot recycle bytes that remain live at the tail.
    pub fn release_byte_span(&self, offset: u64, len: u64) -> Result<(), GrowFailed> {
        if len == 0 {
            return Ok(());
        }
        let tail = self.header().slab_occupied_tail;
        let end = offset.checked_add(len).ok_or(GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })?;
        if end <= tail {
            return Ok(());
        }
        self.free_spans
            .release_span(offset, len)
            .map_err(|_| GrowFailed {
                current_size: self.byte_capacity(),
                delta: 0,
            })
    }

    /// Allocates at the occupied tail without consulting the free list.
    pub fn append_byte_span(&self, len: u64) -> Result<u64, GrowFailed> {
        let start = self.header().slab_occupied_tail;
        let end = checked_add_byte_offset(start, len).ok_or(GrowFailed {
            current_size: self.byte_capacity(),
            delta: 0,
        })?;
        if end > self.byte_capacity() {
            self.set_byte_capacity(end.max(self.byte_capacity()))?;
        }
        let mut h = self.header();
        h.slab_occupied_tail = end;
        self.slab.write_header(&h);
        self.header.set(h);
        Ok(start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;
    use std::{cell::RefCell, rc::Rc};

    fn mem() -> VectorMemory {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn test_store() -> EdgeValueStore<VectorMemory> {
        EdgeValueStore::new(mem(), mem(), mem(), mem(), 1024, 1).expect("store")
    }

    #[test]
    fn value_log_round_trips_payload() {
        let store = test_store();
        store
            .write_value_log_entry(0, 0, -1, 0, 4, &[1, 2, 3, 4])
            .expect("write");
        let mut out = [0u8; 4];
        store.read_value_log_entry(0, 0, 4, &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
        store.read_value_log_asc_index(0, 0, 0, 4, &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn value_slab_round_trips_bytes() {
        let store = test_store();
        store.write_bytes(10, &[1, 2, 3, 4]).expect("write");
        let mut buf = [0u8; 4];
        store.read_bytes(10, &mut buf);
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn retire_byte_span_reuses_via_free_list() {
        let store = test_store();
        let a = store.allocate_byte_span(4).expect("a");
        store.write_bytes(a, &[1, 2, 3, 4]).expect("write a");
        store.retire_byte_span(a, 4).expect("retire a");
        let b = store.allocate_byte_span(4).expect("b");
        assert_eq!(b, a);
        let mut buf = [0u8; 4];
        store.read_bytes(b, &mut buf);
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn release_byte_span_at_tail_does_not_recycle() {
        let store = test_store();
        let a = store.allocate_byte_span(4).expect("a");
        store.release_byte_span(a, 4).expect("release at tail");
        let b = store.allocate_byte_span(4).expect("b");
        assert_eq!(b, 4, "tail bump alloc after release at occupied tail");
    }

    #[test]
    fn value_log_asc_index_follows_oldest_to_newest() {
        let store = test_store();
        store
            .write_value_log_entry(0, 0, -1, 0, 2, &[10, 0])
            .expect("entry 0");
        store
            .write_value_log_entry(0, 1, 0, 0, 2, &[20, 0])
            .expect("entry 1");
        let mut out = [0u8; 2];
        store.read_value_log_asc_index(0, 1, 0, 2, &mut out);
        assert_eq!(out, [10, 0]);
        store.read_value_log_asc_index(0, 1, 1, 2, &mut out);
        assert_eq!(out, [20, 0]);
    }

    #[test]
    fn value_log_grows_segment_count() {
        let store = test_store();
        assert_eq!(store.log.header().segment_count, 1);
        store.grow_segment_count_to(4).expect("grow");
        assert_eq!(store.log.header().segment_count, 4);
        store
            .write_value_log_entry(3, 0, -1, 0, 1, &[7])
            .expect("leaf 3");
        let mut out = [0u8; 1];
        store.read_value_log_entry(3, 0, 1, &mut out);
        assert_eq!(out, [7]);
    }

    #[test]
    fn grow_byte_span_in_place_extends_tail_span() {
        let store = test_store();
        let offset = store.allocate_byte_span(4).expect("allocate");
        store.write_bytes(offset, &[1, 2, 3, 4]).expect("write");
        assert!(
            store
                .grow_byte_span_in_place(offset, 4, 8)
                .expect("grow in place")
        );
        let mut buf = [0u8; 8];
        store.read_bytes(offset, &mut buf);
        assert_eq!(buf, [1, 2, 3, 4, 0, 0, 0, 0]);
    }

    #[test]
    fn init_rejects_mismatched_log_segment_count() {
        use super::log::{HeaderV1 as LogHeader, ValueLogStore};

        let edge_segments = 3u32;
        let log_mem = mem();
        let log = ValueLogStore::new(log_mem, LogHeader::new(2))
            .expect("log with fewer segments than edge store");
        let log_mem = log.into_memory();
        assert!(matches!(
            EdgeValueStore::init(mem(), log_mem, mem(), mem(), 1024, edge_segments),
            Err(InitError::ValueLogLayoutMismatch)
        ));
    }
}
