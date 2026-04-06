//! A double-ended queue in stable memory (ring buffer).
//!
//! Internally wraps [crate::base_vec::BaseVec] for the shared V1 header and slot region layout.
//!
//! # V1 layout
//!
//! Same 64-byte header as `BaseVec`, with the first 16 bytes of the former reserved region
//! used for the ring:
//!
//! ```text
//! Bytes 17–24: head (u64) — physical slot index of logical element 0
//! Bytes 25–32: capacity (u64) — allocated slot count (may exceed len)
//! Bytes 33–63: reserved
//! ```
//!
//! Slots start at offset 64; logical index `i` maps to physical slot `(head + i) % capacity`.

use crate::memory::{
    GrowFailed, WASM_PAGE_SIZE, grow_memory_to_at_least_bytes, read_u32, read_u64, safe_write,
    write_u32, write_u64,
};
use crate::slot;
use crate::storable::bounds;
use crate::types::Address;
use ic_stable_structures::{Memory, Storable};

#[cfg(test)]
use ic_stable_structures::VectorMemory;
use std::borrow::Cow;
use std::cmp::min;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Range;

const MAGIC: [u8; 3] = *b"SVD";

const LAYOUT_VERSION: u8 = 1;
const DATA_OFFSET: u64 = 64;
const LEN_OFFSET: u64 = 4;
const HEAD_OFFSET: u64 = 17;
const CAP_OFFSET: u64 = 25;

#[derive(PartialEq, Eq, Debug)]
pub enum InitError {
    /// The memory already contains another data structure.
    /// Use [Vec::new] to overwrite it.
    BadMagic { actual: [u8; 3], expected: [u8; 3] },
    /// The current version of [Vec] does not support the version of the
    /// memory layout.
    IncompatibleVersion(u8),
    /// The vector type is not compatible with the current vector
    /// layout: the type's bounds differ from the original initialization
    /// parameters.
    IncompatibleElementType,
    /// Failed to allocate memory for the vector.
    OutOfMemory,
    /// The deque layout is invalid.
    InvalidLayout,
}

impl fmt::Display for InitError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual, expected } => {
                write!(fmt, "bad magic number {actual:?}, expected {expected:?}")
            }
            Self::IncompatibleVersion(version) => write!(
                fmt,
                "unsupported layout version {version}; supported version numbers are 1..={LAYOUT_VERSION}"
            ),
            Self::IncompatibleElementType => write!(
                fmt,
                "the bounds (either max_size or is_fixed_size) of the element type do not match the persisted vector attributes"
            ),
            Self::OutOfMemory => write!(fmt, "failed to allocate memory for vector metadata"),
            Self::InvalidLayout => write!(fmt, "invalid deque layout"),
        }
    }
}

impl std::error::Error for InitError {}

/// A deque in stable memory backed by a growable ring buffer.
pub struct VecDeque<T: Storable, M: Memory> {
    memory: M,
    _marker: PhantomData<T>,
}

impl<T: Storable, M: Memory> VecDeque<T, M> {
    /// Creates a new empty deque, overwriting any previous contents of `memory`.
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        let t_bounds = bounds::<T>();
        write_deque_header(
            &memory,
            &DequeHeaderV1 {
                magic: MAGIC,
                version: LAYOUT_VERSION,
                len: 0,
                max_size: t_bounds.max_size,
                is_fixed_size: t_bounds.is_fixed_size,
                head: 0,
                capacity: 0,
            },
        )?;
        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    /// Initializes a deque from existing memory.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let h = read_deque_header(&memory);
        if h.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: h.magic,
                expected: MAGIC,
            });
        }
        if h.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(h.version));
        }
        let t_bounds = bounds::<T>();
        if h.max_size != t_bounds.max_size || h.is_fixed_size != t_bounds.is_fixed_size {
            return Err(InitError::IncompatibleElementType);
        }

        if h.capacity == 0 {
            if h.len != 0 || h.head != 0 {
                return Err(InitError::InvalidLayout);
            }
        } else if h.len > h.capacity || h.head >= h.capacity {
            return Err(InitError::InvalidLayout);
        }

        if h.len == 0 && h.head != 0 {
            return Err(InitError::InvalidLayout);
        }

        let slot = slot::slot_size::<T>() as u64;
        let need = DATA_OFFSET.saturating_add(h.capacity.saturating_mul(slot));
        let pages = memory.size();
        let bytes = pages.saturating_mul(WASM_PAGE_SIZE);
        if bytes < need {
            return Err(InitError::InvalidLayout);
        }

        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    pub fn into_memory(self) -> M {
        self.memory
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> u64 {
        read_u64(&self.memory, Address::from(LEN_OFFSET))
    }

    fn head(&self) -> u64 {
        read_u64(&self.memory, Address::from(HEAD_OFFSET))
    }

    fn capacity(&self) -> u64 {
        read_u64(&self.memory, Address::from(CAP_OFFSET))
    }

    fn set_len(&self, len: u64) {
        write_u64(&self.memory, Address::from(LEN_OFFSET), len);
    }

    fn set_head(&self, head: u64) {
        write_u64(&self.memory, Address::from(HEAD_OFFSET), head);
    }

    fn set_capacity(&self, capacity: u64) {
        write_u64(&self.memory, Address::from(CAP_OFFSET), capacity);
    }

    fn slot_byte_offset(&self, physical_slot: u64) -> u64 {
        DATA_OFFSET + physical_slot * slot::slot_size::<T>() as u64
    }

    /// When `len == capacity`, grows the ring and linearizes elements at physical slots `0..len`.
    fn grow_if_full(&self) -> Result<(), GrowFailed> {
        let len = self.len();
        let cap = self.capacity();
        if len < cap {
            return Ok(());
        }
        let slot = slot::slot_size::<T>() as u64;
        let new_cap = if cap == 0 {
            1
        } else {
            cap.saturating_mul(2).max(len.saturating_add(1))
        };
        let need = DATA_OFFSET + new_cap * slot;
        grow_memory_to_at_least_bytes(&self.memory, need)?;

        if cap == 0 {
            self.set_capacity(new_cap);
            return Ok(());
        }

        let head = self.head();
        for i in 0..len {
            let phys = (head + i) % cap;
            let old_off = DATA_OFFSET + phys * slot;
            let value: T = slot::read_slot::<M, T>(&self.memory, old_off);
            let new_off = DATA_OFFSET + i * slot;
            slot::write_slot(&self.memory, new_off, &value)?;
        }
        self.set_head(0);
        self.set_capacity(new_cap);
        Ok(())
    }

    fn physical_index(&self, logical: u64) -> u64 {
        let cap = self.capacity();
        let head = self.head();
        (head + logical) % cap
    }

    pub fn get(&self, index: u64) -> Option<T> {
        let len = self.len();
        if index >= len {
            return None;
        }
        let cap = self.capacity();
        debug_assert!(cap > 0);
        let phys = self.physical_index(index);
        Some(slot::read_slot(&self.memory, self.slot_byte_offset(phys)))
    }

    pub fn set(&self, index: u64, item: &T) {
        assert!(index < self.len());
        let cap = self.capacity();
        assert!(cap > 0);
        let phys = self.physical_index(index);
        slot::write_slot(&self.memory, self.slot_byte_offset(phys), item)
            .expect("writing into allocated ring must succeed");
    }

    pub fn push_back(&self, item: &T) -> Result<(), GrowFailed> {
        self.grow_if_full()?;
        let len = self.len();
        let cap = self.capacity();
        let head = self.head();
        let phys = (head + len) % cap;
        slot::write_slot(&self.memory, self.slot_byte_offset(phys), item)?;
        self.set_len(len + 1);
        Ok(())
    }

    pub fn push_front(&self, item: &T) -> Result<(), GrowFailed> {
        self.grow_if_full()?;
        let len = self.len();
        let cap = self.capacity();
        debug_assert!(cap > 0);
        let head = self.head();
        let new_head = (head + cap - 1) % cap;
        self.set_head(new_head);
        slot::write_slot(&self.memory, self.slot_byte_offset(new_head), item)?;
        self.set_len(len + 1);
        Ok(())
    }

    pub fn pop_back(&self) -> Option<T> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let cap = self.capacity();
        let head = self.head();
        let phys = (head + len - 1) % cap;
        let value = slot::read_slot(&self.memory, self.slot_byte_offset(phys));
        let new_len = len - 1;
        self.set_len(new_len);
        if new_len == 0 {
            self.set_head(0);
        }
        Some(value)
    }

    pub fn pop_front(&self) -> Option<T> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let cap = self.capacity();
        let head = self.head();
        let value = slot::read_slot(&self.memory, self.slot_byte_offset(head));
        let new_len = len - 1;
        self.set_len(new_len);
        if new_len == 0 {
            self.set_head(0);
        } else if cap > 1 {
            self.set_head((head + 1) % cap);
        }
        Some(value)
    }

    pub fn iter(&self) -> Iter<'_, T, M> {
        Iter {
            deque: self,
            buf: vec![],
            range: Range {
                start: 0,
                end: self.len(),
            },
        }
    }

    fn read_entry_to(&self, logical_index: u64, buf: &mut std::vec::Vec<u8>) {
        let phys = self.physical_index(logical_index);
        slot::read_entry_to::<M, T>(&self.memory, self.slot_byte_offset(phys), buf);
    }

    pub fn to_vec(&self) -> std::vec::Vec<T> {
        self.iter().collect()
    }
}

impl<T: Storable + fmt::Debug, M: Memory> fmt::Debug for VecDeque<T, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_vec().fmt(f)
    }
}

struct DequeHeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u64,
    max_size: u32,
    is_fixed_size: bool,
    head: u64,
    capacity: u64,
}

fn write_deque_header<M: Memory>(memory: &M, h: &DequeHeaderV1) -> Result<(), GrowFailed> {
    safe_write(memory, 0, &h.magic)?;
    memory.write(3, &[h.version; 1]);
    write_u64(memory, Address::from(LEN_OFFSET), h.len);
    write_u32(memory, Address::from(12), h.max_size);
    memory.write(16, &[if h.is_fixed_size { 1u8 } else { 0u8 }; 1]);
    write_u64(memory, Address::from(HEAD_OFFSET), h.head);
    write_u64(memory, Address::from(CAP_OFFSET), h.capacity);
    Ok(())
}

fn read_deque_header<M: Memory>(memory: &M) -> DequeHeaderV1 {
    let mut magic = [0u8; 3];
    let mut version = [0u8; 1];
    let mut is_fixed_size = [0u8; 1];
    memory.read(0, &mut magic);
    memory.read(3, &mut version);
    let len = read_u64(memory, Address::from(LEN_OFFSET));
    let max_size = read_u32(memory, Address::from(12));
    memory.read(16, &mut is_fixed_size);
    let head = read_u64(memory, Address::from(HEAD_OFFSET));
    let capacity = read_u64(memory, Address::from(CAP_OFFSET));
    DequeHeaderV1 {
        magic,
        version: version[0],
        len,
        max_size,
        is_fixed_size: is_fixed_size[0] != 0,
        head,
        capacity,
    }
}

pub struct Iter<'a, T, M>
where
    T: Storable,
    M: Memory,
{
    deque: &'a VecDeque<T, M>,
    buf: std::vec::Vec<u8>,
    range: Range<u64>,
}

impl<T, M> Iterator for Iter<'_, T, M>
where
    T: Storable,
    M: Memory,
{
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.range.is_empty() || self.deque.len() <= self.range.start {
            return None;
        }
        self.deque.read_entry_to(self.range.start, &mut self.buf);
        self.range.start = self.range.start.saturating_add(1);
        Some(T::from_bytes(Cow::Borrowed(&self.buf)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (
            min(self.deque.len(), self.range.end).saturating_sub(self.range.start) as usize,
            None,
        )
    }

    fn count(self) -> usize {
        min(self.deque.len(), self.range.end)
            .saturating_sub(self.range.start)
            .try_into()
            .expect("Cannot express count as usize")
    }

    fn nth(&mut self, n: usize) -> Option<T> {
        self.range.start = self.range.start.saturating_add(n as u64);
        self.next()
    }
}

impl<T, M> DoubleEndedIterator for Iter<'_, T, M>
where
    T: Storable,
    M: Memory,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.range.is_empty() || self.deque.len() < self.range.end {
            return None;
        }
        self.deque.read_entry_to(self.range.end - 1, &mut self.buf);
        self.range.end = self.range.end.saturating_sub(1);
        Some(T::from_bytes(Cow::Borrowed(&self.buf)))
    }

    fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
        self.range.end = self.range.end.saturating_sub(n as u64);
        self.next_back()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::storable::Storable;
    use std::collections::VecDeque as StdDeque;

    #[derive(Clone, PartialEq, Eq, Debug)]
    struct Test {
        x: u64,
        y: u32,
    }

    impl Storable for Test {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            let mut v = vec![0u8; 12];
            v[0..8].copy_from_slice(&self.x.to_le_bytes());
            v[8..12].copy_from_slice(&self.y.to_le_bytes());
            Cow::Owned(v)
        }

        fn into_bytes(self) -> std::vec::Vec<u8> {
            self.to_bytes().into_owned()
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            let b = bytes.as_ref();
            let x = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let y = u32::from_le_bytes(b[8..12].try_into().unwrap());
            Self { x, y }
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 12,
                is_fixed_size: true,
            };
    }

    fn sample(i: u64) -> Test {
        Test { x: i, y: i as u32 }
    }

    #[test]
    fn mirror_random_ops_u64() {
        let mem = VectorMemory::default();
        let dq = VecDeque::<u64, _>::new(mem).unwrap();
        let mut std_dq = StdDeque::new();

        for step in 0u64..2000 {
            let op = step % 7;
            match op {
                0 => {
                    dq.push_back(&step).unwrap();
                    std_dq.push_back(step);
                }
                1 => {
                    dq.push_front(&step).unwrap();
                    std_dq.push_front(step);
                }
                2 => {
                    assert_eq!(dq.pop_front(), std_dq.pop_front());
                }
                3 => {
                    assert_eq!(dq.pop_back(), std_dq.pop_back());
                }
                4 if !std_dq.is_empty() => {
                    let i = (step as usize) % std_dq.len();
                    let a = dq.get(i as u64);
                    let b = std_dq.get(i).copied();
                    assert_eq!(a, b);
                }
                _ => {}
            }
            assert_eq!(dq.len(), std_dq.len() as u64);
        }
    }

    #[test]
    fn mirror_storable_type() {
        let mem = VectorMemory::default();
        let dq = VecDeque::<Test, _>::new(mem).unwrap();
        let mut std_dq = StdDeque::new();
        for i in 0..100 {
            let v = sample(i);
            dq.push_back(&v).unwrap();
            std_dq.push_back(v);
        }
        assert_eq!(
            dq.to_vec(),
            std_dq.into_iter().collect::<std::vec::Vec<_>>()
        );
    }

    #[test]
    fn init_roundtrip() {
        let mem = VectorMemory::default();
        let mem = {
            let dq = VecDeque::<u64, _>::new(mem).unwrap();
            for i in 0u64..50 {
                dq.push_back(&i).unwrap();
            }
            dq.into_memory()
        };
        let dq2 = VecDeque::<u64, _>::init(mem).unwrap();
        assert_eq!(dq2.to_vec(), (0u64..50).collect::<std::vec::Vec<_>>());
    }
}
