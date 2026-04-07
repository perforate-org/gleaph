//! On-disk implementation of [`SlotMap`], [`SlotKey`], and [`IterOccupied`].
//!
//! The persistent format is documented at the [crate root](crate#logical-layout-memory-bytes).
//! This module owns the V1 header layout (`SSM` magic, freelist, slot array at byte offset 64).

use crate::memory::{
    GrowFailed, WASM_PAGE_SIZE, grow_memory_to_at_least_bytes, read_u32, read_u64, safe_write,
    write_u32, write_u64,
};
use crate::slot_cell::SlotCell::{Occupied, Vacant};
use crate::slot_cell::{SlotCell, TAG_OCCUPIED, read_cell_tag, slot_cell_size};
use crate::types::Address;
use ic_stable_structures::{Memory, Storable};
use std::fmt;
use std::iter::FusedIterator;
use std::marker::PhantomData;

const MAGIC: [u8; 3] = *b"SSM";
const LAYOUT_VERSION: u8 = 1;
const DATA_OFFSET: u64 = 64;
const LIVE_COUNT_OFFSET: u64 = 4;
const MAX_CELL_SIZE_OFFSET: u64 = 12;
const IS_FIXED_OFFSET: u64 = 16;
const SLOT_CAPACITY_OFFSET: u64 = 17;
const FREE_HEAD_OFFSET: u64 = 25;

/// Sentinel stored in the header when no slots are on the free list.
///
/// Must not equal any valid physical slot index (`0 .. slot_capacity`). Using `u32::MAX`
/// matches that property for all capacities representable by this map.
pub const FREELIST_EMPTY: u32 = u32::MAX;

/// Handle returned by [`SlotMap::insert`] and required by [`SlotMap::get`],
/// [`SlotMap::set`], and [`SlotMap::remove`].
///
/// The pair **(index, generation)** identifies a logical slot. After [`SlotMap::remove`]
/// at `index`, the stored generation is advanced, so old [`SlotKey`] values become **stale** and
/// must not be used again for that slot (a later [`SlotMap::insert`] may reuse `index` with
/// a new generation).
///
/// # Example
///
/// ```
/// use ic_stable_slot_map::SlotMap;
/// use ic_stable_structures::DefaultMemoryImpl;
///
/// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
/// let key = map.insert(&7).unwrap();
/// assert_eq!(key.index, 0);
/// assert_eq!(key.generation, 1);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SlotKey {
    /// Physical slot row in stable memory: `0 .. slot_capacity` (not necessarily dense).
    pub index: u32,
    /// Version counter for this `index`; must match the cell for lookups and updates to succeed.
    pub generation: u32,
}

/// Failure to attach a [`SlotMap`] to existing stable memory ([`SlotMap::init`]).
#[derive(PartialEq, Eq, Debug)]
pub enum InitError {
    /// The first three bytes are not the expected `SSM` magic.
    BadMagic { actual: [u8; 3], expected: [u8; 3] },
    /// Layout version in the header is not supported by this crate version.
    IncompatibleVersion(u8),
    /// Persisted cell width / fixed-size flag does not match the cell layout required for `T`.
    IncompatibleElementType,
    /// Empty memory could not be initialized (e.g. grow failure while writing the header).
    OutOfMemory,
    /// Header fields contradict each other or the allocated memory size (capacity, freelist,
    /// `live_count`, or required byte length).
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
                "the persisted slot cell bounds do not match the element type"
            ),
            Self::OutOfMemory => write!(fmt, "failed to allocate memory for slot map metadata"),
            Self::InvalidLayout => write!(fmt, "invalid slot map layout"),
        }
    }
}

impl std::error::Error for InitError {}

/// Failure of [`SlotMap::set`] or [`SlotMap::remove`] when the key or slot state is wrong.
#[derive(PartialEq, Eq, Debug)]
pub enum SlotMapError {
    /// `key.index` is not less than current [`SlotMap::slot_capacity`].
    OutOfRange,
    /// The slot at `key.index` is occupied but its generation differs from `key.generation`
    /// (stale key, or wrong key for that index).
    StaleKey,
    /// The slot at `key.index` is vacant (already removed, or never filled with this generation).
    Vacant,
}

impl fmt::Display for SlotMapError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => write!(fmt, "slot index out of range"),
            Self::StaleKey => write!(fmt, "stale or invalid slot key"),
            Self::Vacant => write!(fmt, "slot is vacant"),
        }
    }
}

impl std::error::Error for SlotMapError {}

struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    live_count: u64,
    max_cell_size: u32,
    is_fixed_cell: bool,
    slot_capacity: u64,
    free_head: u32,
}

fn write_header<M: Memory>(memory: &M, h: &HeaderV1) -> Result<(), GrowFailed> {
    safe_write(memory, 0, &h.magic)?;
    memory.write(3, &[h.version; 1]);
    write_u64(memory, Address::from(LIVE_COUNT_OFFSET), h.live_count);
    write_u32(memory, Address::from(MAX_CELL_SIZE_OFFSET), h.max_cell_size);
    memory.write(
        IS_FIXED_OFFSET,
        &[if h.is_fixed_cell { 1u8 } else { 0u8 }; 1],
    );
    write_u64(memory, Address::from(SLOT_CAPACITY_OFFSET), h.slot_capacity);
    write_u32(memory, Address::from(FREE_HEAD_OFFSET), h.free_head);
    Ok(())
}

fn read_header<M: Memory>(memory: &M) -> HeaderV1 {
    let mut magic = [0u8; 3];
    let mut version = [0u8; 1];
    let mut is_fixed = [0u8; 1];
    memory.read(0, &mut magic);
    memory.read(3, &mut version);
    let live_count = read_u64(memory, Address::from(LIVE_COUNT_OFFSET));
    let max_cell_size = read_u32(memory, Address::from(MAX_CELL_SIZE_OFFSET));
    memory.read(IS_FIXED_OFFSET, &mut is_fixed);
    let slot_capacity = read_u64(memory, Address::from(SLOT_CAPACITY_OFFSET));
    let free_head = read_u32(memory, Address::from(FREE_HEAD_OFFSET));
    HeaderV1 {
        magic,
        version: version[0],
        live_count,
        max_cell_size,
        is_fixed_cell: is_fixed[0] != 0,
        slot_capacity,
        free_head,
    }
}

/// Generational **slot map** backed by a single [`ic_stable_structures::Memory`] region (V1 `SSM`).
///
/// Values of type `T` must implement [`Storable`] with a **bounded** layout; each physical slot is a
/// fixed-width cell (see [crate-level layout](crate#logical-layout-memory-bytes)).
///
/// # Model
///
/// - **Occupied** slots hold a `T` plus a **generation**; [`insert`](SlotMap::insert) returns
///   a [`SlotKey`] `(index, generation)`.
/// - **Vacant** slots form a freelist rooted at `free_head` in the header; [`remove`](SlotMap::remove)
///   turns a slot vacant and prepends it to that list for reuse.
/// - [`len`](SlotMap::len) counts occupied slots; [`slot_capacity`](SlotMap::slot_capacity)
///   is the number of physical cells (occupied + vacant). In general `len <= slot_capacity`.
///
/// # Type parameters
///
/// - `T`: element type; serialized via [`Storable`] into each cell.
/// - `M`: stable memory handle ([`ic_stable_structures::Memory`]). [`DefaultMemoryImpl`](ic_stable_structures::DefaultMemoryImpl)
///   is the usual default: canister stable memory on `wasm32`, an in-memory vector on other targets
///   (doctests and host builds).
///
/// # Concurrency
///
/// All methods take `&self` and mutate through `Memory`. Do not alias the same bytes with another
/// mutating abstraction; iterators ([`iter_occupied`](SlotMap::iter_occupied)) assume the map
/// is not concurrently rewritten.
///
/// # Example
///
/// ```
/// use ic_stable_slot_map::SlotMap;
/// use ic_stable_structures::DefaultMemoryImpl;
///
/// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
/// let key = map.insert(&10).unwrap();
/// assert_eq!(map.get(key), Some(10));
/// assert_eq!(map.len(), 1);
///
/// map.set(key, &20).unwrap();
/// assert_eq!(map.get(key), Some(20));
///
/// assert_eq!(map.remove(key).unwrap(), 20);
/// assert_eq!(map.get(key), None);
/// assert!(map.is_empty());
///
/// let key2 = map.insert(&30).unwrap();
/// let collected: Vec<_> = map.iter_occupied().collect();
/// assert_eq!(collected, vec![(key2, 30)]);
/// ```
pub struct SlotMap<T: Storable, M: Memory> {
    memory: M,
    _marker: PhantomData<T>,
}

impl<T: Storable, M: Memory> SlotMap<T, M> {
    /// Formats `memory` as an empty slot map, overwriting any previous contents of that region.
    ///
    /// Writes the V1 header (`SSM`, version 1, `live_count = 0`, `slot_capacity = 0`);
    /// `free_head` is set to [`FREELIST_EMPTY`].
    ///
    /// # Errors
    ///
    /// Returns [`GrowFailed`] if the header cannot be written (stable memory grow failure).
    ///
    /// # Complexity
    ///
    /// O(1) plus a small constant number of writes.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_slot_map::SlotMap;
    /// use ic_stable_structures::DefaultMemoryImpl;
    ///
    /// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// assert_eq!(map.len(), 0);
    /// assert_eq!(map.slot_capacity(), 0);
    /// ```
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        let cell_sz = slot_cell_size::<T>();
        let h = HeaderV1 {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            live_count: 0,
            max_cell_size: cell_sz,
            is_fixed_cell: true,
            slot_capacity: 0,
            free_head: FREELIST_EMPTY,
        };
        write_header(&memory, &h)?;
        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    /// Opens an existing region previously written by [`SlotMap::new`] or an older compatible
    /// process using the same layout.
    ///
    /// Validates magic, version, that the cell size required for `T` matches the header, and that header fields
    /// are consistent with the allocated memory size.
    ///
    /// # Errors
    ///
    /// - [`InitError::BadMagic`] — wrong magic bytes.
    /// - [`InitError::IncompatibleVersion`] — unsupported layout version.
    /// - [`InitError::IncompatibleElementType`] — cell bounds for `T` do not match persistence.
    /// - [`InitError::OutOfMemory`] — zero-sized memory and [`new`](SlotMap::new) failed.
    /// - [`InitError::InvalidLayout`] — corrupt or truncated header / slot region.
    ///
    /// # Complexity
    ///
    /// O(1) metadata reads.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_slot_map::SlotMap;
    /// use ic_stable_structures::DefaultMemoryImpl;
    ///
    /// let mem = DefaultMemoryImpl::default();
    /// let mem = {
    ///     let map = SlotMap::<u64, _>::new(mem).unwrap();
    ///     map.insert(&42).unwrap();
    ///     map.into_memory()
    /// };
    /// let map = SlotMap::<u64, _>::init(mem).unwrap();
    /// assert_eq!(map.len(), 1);
    /// let sum: u64 = map.iter_occupied().map(|(_, v)| v).sum();
    /// assert_eq!(sum, 42);
    /// ```
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let h = read_header(&memory);
        if h.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: h.magic,
                expected: MAGIC,
            });
        }
        if h.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(h.version));
        }
        let cell_sz = slot_cell_size::<T>();
        if h.max_cell_size != cell_sz || !h.is_fixed_cell {
            return Err(InitError::IncompatibleElementType);
        }

        let slot = cell_sz as u64;
        let need = DATA_OFFSET.saturating_add(h.slot_capacity.saturating_mul(slot));
        let pages = memory.size();
        let bytes = pages.saturating_mul(WASM_PAGE_SIZE);
        if bytes < need {
            return Err(InitError::InvalidLayout);
        }

        if h.slot_capacity > u32::MAX as u64 {
            return Err(InitError::InvalidLayout);
        }
        if h.free_head != FREELIST_EMPTY && h.free_head as u64 >= h.slot_capacity {
            return Err(InitError::InvalidLayout);
        }
        if h.live_count > h.slot_capacity {
            return Err(InitError::InvalidLayout);
        }

        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    /// Consumes `self` and returns the underlying [`ic_stable_structures::Memory`] value.
    ///
    /// Use this to persist the handle or re-open with [`init`](SlotMap::init).
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_slot_map::SlotMap;
    /// use ic_stable_structures::DefaultMemoryImpl;
    ///
    /// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// let mem = map.into_memory();
    /// let _same = SlotMap::<u64, _>::init(mem).unwrap();
    /// ```
    pub fn into_memory(self) -> M {
        self.memory
    }

    fn cell_bytes(&self) -> u64 {
        slot_cell_size::<T>() as u64
    }

    /// Number of **occupied** slots (logical size).
    ///
    /// Maintained incrementally by [`insert`](SlotMap::insert) and [`remove`](SlotMap::remove).
    /// Always `<= `[`slot_capacity`](SlotMap::slot_capacity) for a consistent store.
    ///
    /// # Complexity
    ///
    /// O(1) (single `u64` read).
    ///
    /// # Example
    ///
    /// ```
    /// # use ic_stable_slot_map::SlotMap;
    /// # use ic_stable_structures::DefaultMemoryImpl;
    /// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// map.insert(&0).unwrap();
    /// assert_eq!(map.len(), 1);
    /// ```
    pub fn len(&self) -> u64 {
        read_u64(&self.memory, Address::from(LIVE_COUNT_OFFSET))
    }

    /// `true` when [`len`](SlotMap::len) is zero.
    ///
    /// # Complexity
    ///
    /// O(1).
    ///
    /// # Example
    ///
    /// ```
    /// # use ic_stable_slot_map::SlotMap;
    /// # use ic_stable_structures::DefaultMemoryImpl;
    /// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// assert!(map.is_empty());
    /// map.insert(&0).unwrap();
    /// assert!(!map.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of **physical** slot cells allocated (occupied + vacant).
    ///
    /// Grows when the freelist is empty and [`insert`](SlotMap::insert) needs a new slot
    /// (typically doubling, starting from 1). Each cell has fixed byte width for `T`.
    ///
    /// # Limits
    ///
    /// The implementation enforces `slot_capacity <= u32::MAX` on grow; [`SlotKey::index`] is `u32`.
    ///
    /// # Complexity
    ///
    /// O(1).
    ///
    /// # Example
    ///
    /// ```
    /// # use ic_stable_slot_map::SlotMap;
    /// # use ic_stable_structures::DefaultMemoryImpl;
    /// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// map.insert(&1).unwrap();
    /// assert!(map.slot_capacity() >= 1);
    /// ```
    pub fn slot_capacity(&self) -> u64 {
        read_u64(&self.memory, Address::from(SLOT_CAPACITY_OFFSET))
    }

    fn free_head(&self) -> u32 {
        read_u32(&self.memory, Address::from(FREE_HEAD_OFFSET))
    }

    fn set_live_count(&self, n: u64) {
        write_u64(&self.memory, Address::from(LIVE_COUNT_OFFSET), n);
    }

    fn set_slot_capacity(&self, n: u64) {
        write_u64(&self.memory, Address::from(SLOT_CAPACITY_OFFSET), n);
    }

    fn set_free_head(&self, n: u32) {
        write_u32(&self.memory, Address::from(FREE_HEAD_OFFSET), n);
    }

    fn slot_offset(&self, index: u32) -> u64 {
        DATA_OFFSET + index as u64 * self.cell_bytes()
    }

    fn read_cell(&self, index: u32) -> SlotCell<T> {
        SlotCell::read_from_memory(&self.memory, self.slot_offset(index))
    }

    fn write_cell(&self, index: u32, cell: &SlotCell<T>) -> Result<(), GrowFailed> {
        cell.write_to_memory(&self.memory, self.slot_offset(index))
    }

    /// Extends physical slots; links new indices into the freelist before `old_free`.
    fn grow(&self) -> Result<(), GrowFailed> {
        let old_cap = self.slot_capacity();
        let new_cap = if old_cap == 0 {
            1u64
        } else {
            old_cap.saturating_mul(2)
        };
        if new_cap > u32::MAX as u64 {
            return Err(GrowFailed::with_pages(self.memory.size(), 0));
        }

        let cell = self.cell_bytes();
        let need = DATA_OFFSET + new_cap * cell;
        grow_memory_to_at_least_bytes(&self.memory, need)?;

        let old_free = self.free_head();
        let old_cap_u32 = old_cap as u32;
        let new_cap_u32 = new_cap as u32;

        if old_cap > 0 && old_cap_u32 as u64 != old_cap {
            return Err(GrowFailed::with_pages(self.memory.size(), 0));
        }

        if new_cap_u32 as u64 != new_cap {
            return Err(GrowFailed::with_pages(self.memory.size(), 0));
        }

        for i in old_cap_u32..new_cap_u32 {
            let next_free = if i + 1 < new_cap_u32 { i + 1 } else { old_free };
            let vacant = Vacant {
                generation: 1,
                next_free,
            };
            self.write_cell(i, &vacant)?;
        }

        if old_cap == 0 {
            self.set_free_head(0);
        } else {
            self.set_free_head(old_cap_u32);
        }
        self.set_slot_capacity(new_cap);
        Ok(())
    }

    /// Allocates one slot, stores `value`, and returns its [`SlotKey`].
    ///
    /// If the freelist is empty ([`FREELIST_EMPTY`]), grows [`slot_capacity`](SlotMap::slot_capacity)
    /// (see [`slot_capacity`](SlotMap::slot_capacity)), initializes new vacant cells, then pops
    /// from the freelist. The returned key's `generation` is taken from the vacant cell (often `1` for
    /// a never-used slot, or bumped after a prior [`remove`](SlotMap::remove) at that index).
    ///
    /// # Errors
    ///
    /// Returns [`GrowFailed`] if stable memory cannot grow to fit new slots, or if internal
    /// invariants are violated (treated as grow failure).
    ///
    /// # Complexity
    ///
    /// Amortized **O(1)**; occasional **O(slot_capacity)** when copying all cells on grow.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_slot_map::SlotMap;
    /// use ic_stable_structures::DefaultMemoryImpl;
    ///
    /// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// let key = map.insert(&100).unwrap();
    /// assert_eq!(map.get(key), Some(100));
    /// ```
    pub fn insert(&self, value: &T) -> Result<SlotKey, GrowFailed> {
        let mut free = self.free_head();
        if free == FREELIST_EMPTY {
            self.grow()?;
            free = self.free_head();
        }

        let idx = free;
        let cap = self.slot_capacity();
        if idx as u64 >= cap {
            return Err(GrowFailed::with_pages(self.memory.size(), 0));
        }

        let cell = self.read_cell(idx);
        match cell {
            Vacant {
                generation,
                next_free,
            } => {
                self.set_free_head(next_free);
                let occ = Occupied {
                    generation,
                    value: T::from_bytes(value.to_bytes_checked()),
                };
                self.write_cell(idx, &occ)?;
                let live = self.len().saturating_add(1);
                self.set_live_count(live);
                Ok(SlotKey {
                    index: idx,
                    generation,
                })
            }
            Occupied { .. } => Err(GrowFailed::with_pages(self.memory.size(), 0)),
        }
    }

    /// Returns a clone-like decode of `T` for `key` if the slot is occupied and generations match.
    ///
    /// Returns `None` if:
    ///
    /// - `key.index >= slot_capacity`, or
    /// - the slot is vacant, or
    /// - the slot is occupied but `key.generation` is stale.
    ///
    /// # Complexity
    ///
    /// O(size of `T` in bytes) for one cell read/decode.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_slot_map::SlotMap;
    /// use ic_stable_structures::DefaultMemoryImpl;
    ///
    /// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// let key = map.insert(&5).unwrap();
    /// assert_eq!(map.get(key), Some(5));
    /// map.remove(key).unwrap();
    /// assert_eq!(map.get(key), None);
    /// ```
    pub fn get(&self, key: SlotKey) -> Option<T> {
        let cap = self.slot_capacity();
        if key.index as u64 >= cap {
            return None;
        }
        match self.read_cell(key.index) {
            Occupied { generation, value } if generation == key.generation => Some(value),
            _ => None,
        }
    }

    /// Overwrites the value at `key` without changing `generation`.
    ///
    /// The slot must already be occupied with the same generation as `key`.
    ///
    /// # Errors
    ///
    /// - [`SlotMapError::OutOfRange`] — `key.index` out of bounds.
    /// - [`SlotMapError::StaleKey`] — generation mismatch.
    /// - [`SlotMapError::Vacant`] — slot is not occupied.
    ///
    /// # Complexity
    ///
    /// O(size of `T`) for read + write of one cell.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_slot_map::SlotMap;
    /// use ic_stable_structures::DefaultMemoryImpl;
    ///
    /// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// let key = map.insert(&1).unwrap();
    /// map.set(key, &2).unwrap();
    /// assert_eq!(map.get(key), Some(2));
    /// ```
    pub fn set(&self, key: SlotKey, value: &T) -> Result<(), SlotMapError> {
        let cap = self.slot_capacity();
        if key.index as u64 >= cap {
            return Err(SlotMapError::OutOfRange);
        }
        match self.read_cell(key.index) {
            Occupied { generation, .. } if generation == key.generation => {
                let occ = Occupied {
                    generation,
                    value: T::from_bytes(value.to_bytes_checked()),
                };
                self.write_cell(key.index, &occ)
                    .map_err(|_| SlotMapError::OutOfRange)?;
                Ok(())
            }
            Occupied { .. } => Err(SlotMapError::StaleKey),
            Vacant { .. } => Err(SlotMapError::Vacant),
        }
    }

    /// Removes the value at `key`, returns it, marks the slot vacant, and prepends it to the freelist.
    ///
    /// The cell’s generation is advanced (`wrapping_add(1)`), so `key` becomes stale and must not be
    /// used again; a future [`insert`](SlotMap::insert) may reuse `key.index` with the new
    /// generation.
    ///
    /// # Errors
    ///
    /// Same as [`set`](SlotMap::set): [`SlotMapError::OutOfRange`], [`SlotMapError::StaleKey`],
    /// [`SlotMapError::Vacant`].
    ///
    /// # Complexity
    ///
    /// O(size of `T`) for one cell rewrite; **O(1)** metadata updates.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_slot_map::SlotMap;
    /// use ic_stable_structures::DefaultMemoryImpl;
    ///
    /// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// let key = map.insert(&9).unwrap();
    /// assert_eq!(map.remove(key).unwrap(), 9);
    /// assert!(map.remove(key).is_err());
    /// ```
    pub fn remove(&self, key: SlotKey) -> Result<T, SlotMapError> {
        let cap = self.slot_capacity();
        if key.index as u64 >= cap {
            return Err(SlotMapError::OutOfRange);
        }
        match self.read_cell(key.index) {
            Occupied { generation, value } if generation == key.generation => {
                let new_gen = generation.wrapping_add(1);
                let vac = Vacant {
                    generation: new_gen,
                    next_free: self.free_head(),
                };
                self.write_cell(key.index, &vac)
                    .map_err(|_| SlotMapError::OutOfRange)?;
                self.set_free_head(key.index);
                let live = self.len().saturating_sub(1);
                self.set_live_count(live);
                Ok(value)
            }
            Occupied { .. } => Err(SlotMapError::StaleKey),
            Vacant { .. } => Err(SlotMapError::Vacant),
        }
    }

    /// Iterates **occupied** slots in ascending [`SlotKey::index`] order.
    ///
    /// Vacant indices are skipped using a one-byte tag read per cell; occupied cells load and decode
    /// the full `T`. Yields `(SlotKey, T)` so callers can correlate handles with payloads (e.g. CSR
    /// column scans).
    ///
    /// # Iterator semantics
    ///
    /// Implements [`Iterator`] and [`FusedIterator`]. [`Iterator::size_hint`] returns `(0, Some(remaining_indices))`:
    /// the upper bound is **not** the count of remaining occupied slots, only an upper bound on how
    /// many physical indices remain to scan.
    ///
    /// # Aliasing
    ///
    /// If the same [`ic_stable_structures::Memory`] is written through another handle while this
    /// iterator is alive, yielded keys and values may be stale, duplicated, or skipped.
    ///
    /// # Complexity
    ///
    /// **O(slot_capacity)** over the full traversal.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_slot_map::SlotMap;
    /// use ic_stable_structures::DefaultMemoryImpl;
    ///
    /// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// let a = map.insert(&1).unwrap();
    /// let b = map.insert(&2).unwrap();
    /// let indices: Vec<u32> = map.iter_occupied().map(|(k, _)| k.index).collect();
    /// assert_eq!(indices, vec![a.index, b.index]);
    /// ```
    pub fn iter_occupied(&self) -> IterOccupied<'_, T, M> {
        let cap = self.slot_capacity();
        let end = if cap > u32::MAX as u64 { 0 } else { cap as u32 };
        IterOccupied {
            map: self,
            next_index: 0,
            end,
        }
    }
}

/// Borrowing iterator produced by [`SlotMap::iter_occupied`].
///
/// Holds a reference to the map and scans indices `[next_index, end)`; see
/// [`SlotMap::iter_occupied`] for complexity and aliasing rules.
///
/// # Example
///
/// ```
/// use ic_stable_slot_map::SlotMap;
/// use ic_stable_structures::DefaultMemoryImpl;
///
/// let map = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
/// map.insert(&3).unwrap();
/// let sum: u64 = map.iter_occupied().map(|(_, v)| v).sum();
/// assert_eq!(sum, 3);
/// ```
pub struct IterOccupied<'a, T: Storable, M: Memory> {
    map: &'a SlotMap<T, M>,
    next_index: u32,
    end: u32,
}

impl<T: Storable, M: Memory> Iterator for IterOccupied<'_, T, M> {
    type Item = (SlotKey, T);

    fn next(&mut self) -> Option<Self::Item> {
        while self.next_index < self.end {
            let idx = self.next_index;
            self.next_index += 1;
            let off = self.map.slot_offset(idx);
            if read_cell_tag(&self.map.memory, off) != TAG_OCCUPIED {
                continue;
            }
            match SlotCell::read_from_memory(&self.map.memory, off) {
                Occupied { generation, value } => {
                    return Some((
                        SlotKey {
                            index: idx,
                            generation,
                        },
                        value,
                    ));
                }
                _ => continue,
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = (self.end - self.next_index) as usize;
        (0, Some(rem))
    }
}

impl<T: Storable, M: Memory> FusedIterator for IterOccupied<'_, T, M> {}

impl<T: Storable, M: Memory> fmt::Debug for SlotMap<T, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotMap")
            .field("len", &self.len())
            .field("slot_capacity", &self.slot_capacity())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::DefaultMemoryImpl;
    use std::borrow::Cow;

    #[derive(Clone, PartialEq, Eq, Debug)]
    struct Test {
        x: u64,
    }

    impl Storable for Test {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Owned(self.x.to_le_bytes().to_vec())
        }

        fn into_bytes(self) -> std::vec::Vec<u8> {
            self.x.to_le_bytes().to_vec()
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            let b = bytes.as_ref();
            Self {
                x: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            }
        }

        const BOUND: ic_stable_structures::storable::Bound =
            ic_stable_structures::storable::Bound::Bounded {
                max_size: 8,
                is_fixed_size: true,
            };
    }

    #[test]
    fn insert_get_remove() {
        let mem = DefaultMemoryImpl::default();
        let m = SlotMap::<Test, _>::new(mem).unwrap();
        let k = m.insert(&Test { x: 42 }).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(k), Some(Test { x: 42 }));
        assert_eq!(m.remove(k), Ok(Test { x: 42 }));
        assert_eq!(m.get(k), None);
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn stale_after_remove() {
        let mem = DefaultMemoryImpl::default();
        let m = SlotMap::<Test, _>::new(mem).unwrap();
        let k = m.insert(&Test { x: 1 }).unwrap();
        m.remove(k).unwrap();
        assert_eq!(m.get(k), None);
        let k2 = m.insert(&Test { x: 2 }).unwrap();
        assert_eq!(k.index, k2.index);
        assert_ne!(k.generation, k2.generation);
    }

    #[test]
    fn freelist_reuse_order() {
        let mem = DefaultMemoryImpl::default();
        let m = SlotMap::<u64, _>::new(mem).unwrap();
        let a = m.insert(&1).unwrap();
        let b = m.insert(&2).unwrap();
        m.remove(a).unwrap();
        m.remove(b).unwrap();
        let c = m.insert(&3).unwrap();
        assert_eq!(c.index, b.index);
    }

    #[test]
    fn init_roundtrip() {
        let mem = DefaultMemoryImpl::default();
        let mem = {
            let m = SlotMap::<u64, _>::new(mem).unwrap();
            m.insert(&10).unwrap();
            m.insert(&20).unwrap();
            m.into_memory()
        };
        let m2 = SlotMap::<u64, _>::init(mem).unwrap();
        assert_eq!(m2.len(), 2);
    }

    #[test]
    fn set_updates_value() {
        let mem = DefaultMemoryImpl::default();
        let m = SlotMap::<Test, _>::new(mem).unwrap();
        let k = m.insert(&Test { x: 1 }).unwrap();
        m.set(k, &Test { x: 99 }).unwrap();
        assert_eq!(m.get(k), Some(Test { x: 99 }));
    }

    #[test]
    fn init_rejects_wrong_element_bounds() {
        let mem = DefaultMemoryImpl::default();
        let mem = {
            let m = SlotMap::<u8, _>::new(mem).unwrap();
            m.insert(&7).unwrap();
            m.into_memory()
        };
        let err = SlotMap::<u64, _>::init(mem).unwrap_err();
        assert_eq!(err, InitError::IncompatibleElementType);
    }

    #[test]
    fn many_inserts_trigger_grow() {
        let mem = DefaultMemoryImpl::default();
        let m = SlotMap::<u64, _>::new(mem).unwrap();
        let mut keys = std::vec::Vec::new();
        for i in 0u64..64 {
            keys.push(m.insert(&i).unwrap());
        }
        assert_eq!(m.len(), 64);
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(m.get(*k), Some(i as u64));
        }
    }

    #[test]
    fn iter_occupied_empty() {
        let m = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
        assert_eq!(m.iter_occupied().count(), 0);
    }

    #[test]
    fn iter_occupied_remove_reinsert_matches_get() {
        let m = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
        let k0 = m.insert(&0).unwrap();
        let k1 = m.insert(&1).unwrap();
        m.remove(k0).unwrap();
        let k0b = m.insert(&10).unwrap();
        let collected: std::vec::Vec<_> = m.iter_occupied().collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].0.index, 0);
        assert_eq!(collected[0].1, 10);
        assert_eq!(collected[1].0.index, 1);
        assert_eq!(collected[1].1, 1);
        assert_eq!(collected[0].0.generation, k0b.generation);
        assert_eq!(collected[1].0.generation, k1.generation);
        assert_eq!(m.get(collected[0].0), Some(10));
        assert_eq!(m.get(collected[1].0), Some(1));
    }

    #[test]
    fn iter_occupied_skips_vacant_ascending_index() {
        let m = SlotMap::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
        m.insert(&1).unwrap();
        let k_mid = m.insert(&2).unwrap();
        m.insert(&3).unwrap();
        m.remove(k_mid).unwrap();
        let indices: std::vec::Vec<u32> = m.iter_occupied().map(|(k, _)| k.index).collect();
        assert_eq!(indices, vec![0, 2]);
        for (k, v) in m.iter_occupied() {
            assert_eq!(m.get(k), Some(v));
        }
    }
}
