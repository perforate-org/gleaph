//! Implementation of [`VecDeque`] and [`Iter`].
//!
//! Layout details and examples live on [`VecDeque`]; the [crate root](crate) summarizes the format.
//!
//! # V1 layout (segmented block-ring)
//!
//! Magic is **`SVD`** and the header occupies a **128-byte** prefix. Elements live in fixed-size
//! **blocks** of `blockSlots` slots; a **directory** of `dirSlots` consecutive 8-byte entries maps
//! block positions `0 .. numBlocks` to physical base addresses; fully drained top-most blocks are
//! recycled through an intrusive **free list** whose head base address is `freeHead`
//! (`u64::MAX` = nil) and whose links are stored in the first 8 bytes of each drained block.
//! Logical index `i` sits at virtual position `r = (headOff + i) % virtCap`, i.e. block
//! `k = r / blockSlots`, slot `k' = r % blockSlots`, address `dir[k] + k'·SLOT_SIZE`. The live
//! window may wrap across the virtual seam; routing is purely arithmetic over persisted fields.
//!
//! Growth never relocates elements in bulk. A push into a full structure appends one virtual
//! block: taken from the free list when possible, otherwise freshly allocated page-aligned at the
//! end of stable memory (doubling the directory first when it is full). Because routing depends
//! on `virtCap`, a wrapped window is first rebased by rotating the directory entries (pure
//! metadata) and the at-most-one-block of boundary slots is migrated into the newly acquired
//! block; all other element addresses remain untouched.
//!
//! When a pop fully drains the block at the consumed end, that block is recycled only if it is
//! the current top block (`numBlocks - 1`): it leaves the virtual space and joins the free list.
//! An interior block that drains keeps its directory entry; its slots stay routable and are
//! reclaimed naturally when the window later wraps onto them. Both choices preserve the property
//! that no operation ever moves or rewrites an existing element.

use crate::memory::{
    GrowFailed, WASM_PAGE_SIZE, alloc_at_end, read_u32, read_u64, safe_write, write_u32, write_u64,
};
use crate::slot;
use crate::storable::bounds;
use crate::types::Address;
use ic_stable_structures::{Memory, Storable};

use std::borrow::Cow;
use std::cmp::min;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Range;

const MAGIC: [u8; 3] = *b"SVD";

const LAYOUT_VERSION: u8 = 1;
const DATA_OFFSET: u64 = 128;
const LEN_OFFSET: u64 = 4;
const MAX_SIZE_OFFSET: u64 = 12;
const IS_FIXED_SIZE_OFFSET: u64 = 16;
const HEAD_OFF_OFFSET: u64 = 17;
const VIRT_CAP_OFFSET: u64 = 25;
const DIR_BASE_OFFSET: u64 = 33;
const DIR_SLOTS_OFFSET: u64 = 41;
const NUM_BLOCKS_OFFSET: u64 = 49;
const BLOCK_SLOTS_OFFSET: u64 = 57;
const FREE_HEAD_OFFSET: u64 = 65;

/// Target byte size of one storage block. The actual slot count per block is
/// [`TARGET_BLOCK_BYTES`] divided by the slot size of `T`, clamped to at least one.
const TARGET_BLOCK_BYTES: u64 = 256 * 1024;

/// Directory capacity (in entries) of a freshly created deque.
const INITIAL_DIR_SLOTS: u64 = 16;

/// Free-list nil marker stored in [`HeaderV1::free_head`] and in free-list links.
const FREE_LIST_NIL: u64 = u64::MAX;

/// Fixed number of slots per block for a given slot size.
fn block_slots_for(slot_size: u32) -> u64 {
    (TARGET_BLOCK_BYTES / u64::from(slot_size)).max(1)
}

/// Failure opening existing memory with [`VecDeque::init`].
#[derive(PartialEq, Eq, Debug)]
pub enum InitError {
    /// First three bytes are not magic `SVD`. Use [`VecDeque::new`] to overwrite the region.
    BadMagic { actual: [u8; 3] },
    /// Persisted layout version is not supported by this crate.
    IncompatibleVersion(u8),
    /// `T`'s [`Storable`](ic_stable_structures::Storable) bounds do not match `max_size` / `is_fixed_size` in the header.
    IncompatibleElementType,
    /// Empty memory and [`VecDeque::new`] failed (e.g. could not write header).
    OutOfMemory,
    /// Header geometry, directory location, directory entries, or allocated memory size are inconsistent.
    InvalidLayout,
}

impl fmt::Display for InitError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(fmt, "bad magic number {actual:?}, expected {MAGIC:?}")
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

/// Double-ended queue in stable [`Memory`](ic_stable_structures::Memory), **V1** segmented
/// block-ring (`SVD` magic, 128-byte header).
///
/// Elements live in fixed-size blocks routed through a directory; logical index `i` sits at
/// virtual position `(headOff + i) % virtCap`. Growing never relocates elements: a push into a
/// full deque appends one block (reused from the free list or freshly allocated) and at most once
/// doubles the directory.
///
/// # Type parameters
///
/// - `T`: [`Storable`](ic_stable_structures::Storable) with bounded encoding (same rules as [`ic_stable_structures::vec::Vec`]).
/// - `M`: typically [`DefaultMemoryImpl`](ic_stable_structures::DefaultMemoryImpl) in application code.
///
/// # Complexity
///
/// Every operation performs at most one element encode/decode plus O(64 bytes) of header writes.
/// Additionally, a push into a full deque performs exactly one block allocation (or free-list
/// reuse) of `blockSlots · SLOT_SIZE` bytes, at most one directory rotation plus copy of
/// `8 · dirSlots ≤ 8 · (len/blockSlots + 1)` metadata bytes, and a boundary migration of at most
/// one block of slots into the newly acquired block. All terms are constants for a fixed `T` at a
/// fixed moment in time; no operation's cost grows with the number of stored elements beyond
/// those envelopes.
///
/// # Panics
///
/// [`set`](VecDeque::set) panics if `index >= len` (unlike [`get`](VecDeque::get), which returns `None`).
///
/// # Example
///
/// ```
/// use ic_stable_structures::DefaultMemoryImpl;
/// use ic_stable_vec_deque::VecDeque;
///
/// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
/// dq.push_back(&10).unwrap();
/// dq.push_front(&5).unwrap();
/// assert_eq!(dq.get(0), Some(5));
/// assert_eq!(dq.get(1), Some(10));
/// assert_eq!(dq.pop_back(), Some(10));
/// ```
pub struct VecDeque<T: Storable, M: Memory> {
    memory: M,
    _marker: PhantomData<T>,
}

impl<T: Storable, M: Memory> VecDeque<T, M> {
    /// Writes a fresh V1 block-ring header (`SVD`, `len = 0`, empty directory) over `memory`.
    ///
    /// # Errors
    ///
    /// [`GrowFailed`] if the header or the initial directory cannot be written.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// assert!(dq.is_empty());
    /// ```
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        let t_bounds = bounds::<T>();
        let dir_slots = INITIAL_DIR_SLOTS;
        let _header_region = alloc_at_end(&memory, DATA_OFFSET)?;
        let dir_base = alloc_at_end(&memory, dir_slots * 8)?;
        write_deque_header(
            &memory,
            &HeaderV1 {
                magic: MAGIC,
                version: LAYOUT_VERSION,
                len: 0,
                max_size: t_bounds.max_size,
                is_fixed_size: t_bounds.is_fixed_size,
                head_off: 0,
                virt_cap: 0,
                dir_base,
                dir_slots,
                num_blocks: 0,
                block_slots: block_slots_for(slot::slot_size::<T>()),
                free_head: FREE_LIST_NIL,
            },
        )?;
        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    /// Attaches to a region previously written by [`VecDeque::new`] (or compatible producer).
    ///
    /// # Errors
    ///
    /// See [`InitError`] variants.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let mem = DefaultMemoryImpl::default();
    /// let mem = {
    ///     let dq = VecDeque::<u64, _>::new(mem).unwrap();
    ///     dq.push_back(&1).unwrap();
    ///     dq.into_memory()
    /// };
    /// let dq = VecDeque::<u64, _>::init(mem).unwrap();
    /// assert_eq!(dq.get(0), Some(1));
    /// ```
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let h = read_deque_header(&memory);
        if h.magic != MAGIC {
            return Err(InitError::BadMagic { actual: h.magic });
        }
        if h.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(h.version));
        }
        let t_bounds = bounds::<T>();
        if h.max_size != t_bounds.max_size || h.is_fixed_size != t_bounds.is_fixed_size {
            return Err(InitError::IncompatibleElementType);
        }

        let slot_size = u64::from(slot::slot_size::<T>());
        if h.block_slots == 0 || h.block_slots != block_slots_for(slot::slot_size::<T>()) {
            return Err(InitError::InvalidLayout);
        }
        if h.dir_slots == 0 || !h.dir_slots.is_power_of_two() {
            return Err(InitError::InvalidLayout);
        }
        if h.num_blocks > h.dir_slots {
            return Err(InitError::InvalidLayout);
        }
        if h.virt_cap != h.num_blocks.saturating_mul(h.block_slots) {
            return Err(InitError::InvalidLayout);
        }
        if h.len > h.virt_cap {
            return Err(InitError::InvalidLayout);
        }
        if h.len == 0 {
            if h.head_off != 0 {
                return Err(InitError::InvalidLayout);
            }
        } else if h.head_off >= h.virt_cap {
            return Err(InitError::InvalidLayout);
        }
        let mem_bytes = memory.size().saturating_mul(WASM_PAGE_SIZE);
        if h.dir_base < DATA_OFFSET {
            return Err(InitError::InvalidLayout);
        }
        if h.free_head != FREE_LIST_NIL && h.free_head >= mem_bytes {
            return Err(InitError::InvalidLayout);
        }

        let dir_end = h
            .dir_base
            .checked_add(h.dir_slots * 8)
            .ok_or(InitError::InvalidLayout)?;
        if dir_end > mem_bytes {
            return Err(InitError::InvalidLayout);
        }
        let block_bytes = h
            .block_slots
            .checked_mul(slot_size)
            .ok_or(InitError::InvalidLayout)?;
        for k in 0..h.dir_slots {
            let entry = read_u64(&memory, Address::from(h.dir_base + k * 8));
            if entry > mem_bytes {
                return Err(InitError::InvalidLayout);
            }
            if k < h.num_blocks
                && entry
                    .checked_add(block_bytes)
                    .is_none_or(|end| end > mem_bytes)
            {
                return Err(InitError::InvalidLayout);
            }
        }

        Ok(Self {
            memory,
            _marker: PhantomData,
        })
    }

    /// Returns the backing [`Memory`](ic_stable_structures::Memory) for persistence or [`init`](VecDeque::init).
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// let mem = dq.into_memory();
    /// let _ = VecDeque::<u64, _>::init(mem).unwrap();
    /// ```
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Returns the stable V1 header fields currently persisted in memory.
    pub fn header(&self) -> HeaderV1 {
        read_deque_header(&self.memory)
    }

    /// `true` when [`len`](VecDeque::len) is zero.
    ///
    /// # Example
    ///
    /// ```
    /// # use ic_stable_structures::DefaultMemoryImpl;
    /// # use ic_stable_vec_deque::VecDeque;
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// assert!(dq.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of elements (logical length).
    pub fn len(&self) -> u64 {
        read_u64(&self.memory, Address::from(LEN_OFFSET))
    }

    fn head_off(&self) -> u64 {
        read_u64(&self.memory, Address::from(HEAD_OFF_OFFSET))
    }

    fn virt_cap(&self) -> u64 {
        read_u64(&self.memory, Address::from(VIRT_CAP_OFFSET))
    }

    fn dir_base(&self) -> u64 {
        read_u64(&self.memory, Address::from(DIR_BASE_OFFSET))
    }

    fn dir_slots(&self) -> u64 {
        read_u64(&self.memory, Address::from(DIR_SLOTS_OFFSET))
    }

    fn num_blocks(&self) -> u64 {
        read_u64(&self.memory, Address::from(NUM_BLOCKS_OFFSET))
    }

    fn block_slots(&self) -> u64 {
        read_u64(&self.memory, Address::from(BLOCK_SLOTS_OFFSET))
    }

    fn free_head(&self) -> u64 {
        read_u64(&self.memory, Address::from(FREE_HEAD_OFFSET))
    }

    fn set_len(&self, len: u64) {
        write_u64(&self.memory, Address::from(LEN_OFFSET), len);
    }

    fn set_head_off(&self, head_off: u64) {
        write_u64(&self.memory, Address::from(HEAD_OFF_OFFSET), head_off);
    }

    fn set_virt_cap(&self, virt_cap: u64) {
        write_u64(&self.memory, Address::from(VIRT_CAP_OFFSET), virt_cap);
    }

    fn set_dir_base(&self, dir_base: u64) {
        write_u64(&self.memory, Address::from(DIR_BASE_OFFSET), dir_base);
    }

    fn set_dir_slots(&self, dir_slots: u64) {
        write_u64(&self.memory, Address::from(DIR_SLOTS_OFFSET), dir_slots);
    }

    fn set_num_blocks(&self, num_blocks: u64) {
        write_u64(&self.memory, Address::from(NUM_BLOCKS_OFFSET), num_blocks);
    }

    fn set_free_head(&self, free_head: u64) {
        write_u64(&self.memory, Address::from(FREE_HEAD_OFFSET), free_head);
    }

    fn dir_entry_addr(&self, block_index: u64) -> u64 {
        self.dir_base() + block_index * 8
    }

    fn block_base(&self, block_index: u64) -> u64 {
        read_u64(
            &self.memory,
            Address::from(self.dir_entry_addr(block_index)),
        )
    }

    fn write_dir_entry(&self, block_index: u64, base: u64) {
        write_u64(
            &self.memory,
            Address::from(self.dir_entry_addr(block_index)),
            base,
        );
    }

    fn slot_addr(&self, virtual_pos: u64) -> u64 {
        let block_slots = self.block_slots();
        self.block_base(virtual_pos / block_slots)
            + (virtual_pos % block_slots) * u64::from(slot::slot_size::<T>())
    }

    fn virtual_index(&self, logical: u64) -> u64 {
        (self.head_off() + logical) % self.virt_cap()
    }

    /// Prepares one additional virtual block for a push into a full structure.
    ///
    /// Wrapping the live window makes a plain capacity change unsound: existing elements route
    /// through `(headOff + i) % virtCap`, so changing `virtCap` while `headOff > 0` would move
    /// every routed position off its physical bytes. Growth therefore first rotates the directory
    /// by `headOff / blockSlots` entries (a pure metadata permutation that rebases `headOff`
    /// below one block) and then migrates the at-most-one-block of boundary slots that still
    /// wraps into the newly acquired block. Together with the directory copy and the block
    /// allocation this keeps growth bounded by O(8·dirSlots) metadata bytes plus one block of
    /// slot I/O, and it never touches the slots of non-boundary elements.
    fn grow_for_push(&self) -> Result<(), GrowFailed> {
        let block_slots = self.block_slots();
        let num_blocks = self.num_blocks();
        let head_off = self.head_off();
        let rotate_by = head_off / block_slots;
        let boundary_slots = head_off % block_slots;
        if num_blocks == self.dir_slots() {
            self.double_directory()?;
        }
        if rotate_by != 0 {
            self.rotate_directory(rotate_by);
            self.set_head_off(boundary_slots);
        }
        let base = if self.free_head() != FREE_LIST_NIL {
            let base = self.free_head();
            let next = read_u64(&self.memory, Address::from(base));
            self.set_free_head(next);
            base
        } else {
            let bytes = block_slots * u64::from(slot::slot_size::<T>());
            alloc_at_end(&self.memory, bytes)?
        };
        self.write_dir_entry(num_blocks, base);
        if boundary_slots != 0 {
            let bytes = (boundary_slots * u64::from(slot::slot_size::<T>())) as usize;
            let mut buf = std::vec![0u8; bytes];
            self.memory.read(self.block_base(0), &mut buf);
            self.memory.write(base, &buf);
        }
        self.set_num_blocks(num_blocks + 1);
        self.set_virt_cap((num_blocks + 1) * block_slots);
        Ok(())
    }

    /// Permutes the live directory entries down by `rotate_by` positions (entry
    /// `k + rotate_by` becomes entry `k`, modulo the block count), compensating for `headOff`
    /// block drift. Scratch entries beyond `numBlocks` are left untouched, which is sound because
    /// the free list chains base addresses rather than block indices.
    fn rotate_directory(&self, rotate_by: u64) {
        let num_blocks = self.num_blocks();
        let dir_base = self.dir_base();
        let mut buf = std::vec![0u8; (num_blocks * 8) as usize];
        self.memory.read(dir_base, &mut buf);
        let mut rotated = buf.clone();
        for k in 0..num_blocks {
            let src = ((k + rotate_by) % num_blocks) as usize * 8;
            rotated[k as usize * 8..(k + 1) as usize * 8].copy_from_slice(&buf[src..src + 8]);
        }
        self.memory.write(dir_base, &rotated);
    }

    /// Allocates a fresh directory of twice the capacity at the end of memory and copies the old
    /// entries over. The old directory region is abandoned.
    fn double_directory(&self) -> Result<(), GrowFailed> {
        let old_slots = self.dir_slots();
        let old_base = self.dir_base();
        let new_slots = old_slots * 2;
        let new_base = alloc_at_end(&self.memory, new_slots * 8)?;
        let mut buf = std::vec![0u8; (old_slots * 8) as usize];
        self.memory.read(old_base, &mut buf);
        self.memory.write(new_base, &buf);
        self.set_dir_base(new_base);
        self.set_dir_slots(new_slots);
        Ok(())
    }

    /// Moves the top block (`numBlocks - 1`) out of the virtual space and onto the free list.
    fn retire_top_block(&self) {
        let num_blocks = self.num_blocks();
        debug_assert!(num_blocks > 0);
        let index = num_blocks - 1;
        let base = self.block_base(index);
        write_u64(&self.memory, Address::from(base), self.free_head());
        self.set_free_head(base);
        self.set_num_blocks(num_blocks - 1);
        self.set_virt_cap((num_blocks - 1) * self.block_slots());
    }

    /// Returns element at logical `index`, or `None` if `index >= len`.
    ///
    /// # Complexity
    ///
    /// O(size of `T`) for one slot read plus one directory load.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// dq.push_back(&3).unwrap();
    /// assert_eq!(dq.get(0), Some(3));
    /// assert_eq!(dq.get(1), None);
    /// ```
    pub fn get(&self, index: u64) -> Option<T> {
        if index >= self.len() {
            return None;
        }
        Some(slot::read_slot(
            &self.memory,
            self.slot_addr(self.virtual_index(index)),
        ))
    }

    /// Overwrites the element at logical `index`.
    ///
    /// # Panics
    ///
    /// Panics if `index >= len` (use [`get`](VecDeque::get) for a non-panicking check).
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// dq.push_back(&1).unwrap();
    /// dq.set(0, &2);
    /// assert_eq!(dq.get(0), Some(2));
    /// ```
    pub fn set(&self, index: u64, item: &T) {
        assert!(index < self.len());
        slot::write_slot(
            &self.memory,
            self.slot_addr(self.virtual_index(index)),
            item,
        )
        .expect("writing into an allocated block must succeed");
    }

    /// Appends `item` at the back. If the deque is full, one block is appended first (see
    /// [complexity](VecDeque#complexity)); no existing element is touched.
    ///
    /// # Errors
    ///
    /// [`GrowFailed`] if stable memory cannot grow.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// dq.push_back(&1).unwrap();
    /// dq.push_back(&2).unwrap();
    /// assert_eq!(dq.to_vec(), vec![1, 2]);
    /// ```
    pub fn push_back(&self, item: &T) -> Result<(), GrowFailed> {
        let len = self.len();
        if len == self.virt_cap() {
            self.grow_for_push()?;
        }
        let r = (self.head_off() + len) % self.virt_cap();
        slot::write_slot(&self.memory, self.slot_addr(r), item)?;
        self.set_len(len + 1);
        Ok(())
    }

    /// Prepends `item` at the front, appending one block first when the deque is full (see
    /// [complexity](VecDeque#complexity)); no existing element is touched.
    ///
    /// # Errors
    ///
    /// [`GrowFailed`] if stable memory cannot grow.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// dq.push_front(&2).unwrap();
    /// dq.push_front(&1).unwrap();
    /// assert_eq!(dq.to_vec(), vec![1, 2]);
    /// ```
    pub fn push_front(&self, item: &T) -> Result<(), GrowFailed> {
        let len = self.len();
        if len == self.virt_cap() {
            self.grow_for_push()?;
        }
        let virt_cap = self.virt_cap();
        let head_off = (self.head_off() + virt_cap - 1) % virt_cap;
        slot::write_slot(&self.memory, self.slot_addr(head_off), item)?;
        self.set_head_off(head_off);
        self.set_len(len + 1);
        Ok(())
    }

    /// Removes and returns the back element, or `None` if empty. When this drains the top block,
    /// the block is recycled onto the free list.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// dq.push_back(&1).unwrap();
    /// assert_eq!(dq.pop_back(), Some(1));
    /// assert_eq!(dq.pop_back(), None);
    /// ```
    pub fn pop_back(&self) -> Option<T> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let virt_cap = self.virt_cap();
        let head_off = self.head_off();
        let block_slots = self.block_slots();
        let pos = (head_off + len - 1) % virt_cap;
        let value = slot::read_slot(&self.memory, self.slot_addr(pos));
        let new_len = len - 1;
        self.set_len(new_len);
        let block = pos / block_slots;
        let top_start = (self.num_blocks() - 1) * block_slots;
        let top_drained = new_len == 0 || self.head_off() + new_len <= top_start;
        if block + 1 == self.num_blocks() && top_drained {
            self.retire_top_block();
        }
        if new_len == 0 {
            self.set_head_off(0);
        }
        Some(value)
    }

    /// Removes and returns the front element, or `None` if empty. When this drains the top block,
    /// the block is recycled onto the free list; drained interior blocks keep their directory
    /// entry and are reclaimed when the window wraps onto them.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// dq.push_back(&1).unwrap();
    /// assert_eq!(dq.pop_front(), Some(1));
    /// ```
    pub fn pop_front(&self) -> Option<T> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let virt_cap = self.virt_cap();
        let head_off = self.head_off();
        let block_slots = self.block_slots();
        let value = slot::read_slot(&self.memory, self.slot_addr(head_off));
        let new_len = len - 1;
        self.set_len(new_len);
        let block = head_off / block_slots;
        let wrapping = head_off + 1 == virt_cap;
        let drained = new_len == 0
            || if wrapping {
                new_len + block_slots <= virt_cap
            } else {
                (head_off + 1).is_multiple_of(block_slots)
            };
        let head_off = if new_len == 0 {
            0
        } else {
            (head_off + 1) % virt_cap
        };
        if drained && block + 1 == self.num_blocks() {
            self.retire_top_block();
        }
        self.set_head_off(head_off);
        Some(value)
    }

    /// Borrows the deque as a forward iterator over logical order `[0, len)`.
    ///
    /// Also implements [`DoubleEndedIterator`] (see [`Iter`]).
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// dq.push_back(&1).unwrap();
    /// dq.push_back(&2).unwrap();
    /// let v: Vec<_> = dq.iter().collect();
    /// assert_eq!(v, vec![1, 2]);
    /// ```
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
        slot::read_entry_to::<M, T>(
            &self.memory,
            self.slot_addr(self.virtual_index(logical_index)),
            buf,
        );
    }

    /// Copies all elements into a heap [`Vec`](std::vec::Vec) in logical order.
    ///
    /// # Example
    ///
    /// ```
    /// use ic_stable_structures::DefaultMemoryImpl;
    /// use ic_stable_vec_deque::VecDeque;
    ///
    /// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
    /// dq.push_back(&7).unwrap();
    /// assert_eq!(dq.to_vec(), vec![7]);
    /// ```
    pub fn to_vec(&self) -> std::vec::Vec<T> {
        self.iter().collect()
    }
}

impl<T: Storable + fmt::Debug, M: Memory> fmt::Debug for VecDeque<T, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_vec().fmt(f)
    }
}

/// Persisted V1 header fields of the segmented block-ring layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    pub magic: [u8; 3],
    pub version: u8,
    pub len: u64,
    pub max_size: u32,
    pub is_fixed_size: bool,
    /// Virtual position of logical index 0.
    pub head_off: u64,
    /// Virtual capacity: `num_blocks * block_slots`.
    pub virt_cap: u64,
    /// Byte offset of the directory (consecutive 8-byte block base entries).
    pub dir_base: u64,
    /// Directory capacity in entries; always a power of two.
    pub dir_slots: u64,
    /// Blocks tracked by the directory (the live virtual space).
    pub num_blocks: u64,
    /// Slots per block, fixed at creation from `T`'s slot size.
    pub block_slots: u64,
    /// Intrusive free-block list head as a base address; `u64::MAX` (= [`FREE_LIST_NIL`]) when nil.
    pub free_head: u64,
}

fn write_deque_header<M: Memory>(memory: &M, h: &HeaderV1) -> Result<(), GrowFailed> {
    safe_write(memory, 0, &h.magic)?;
    memory.write(3, &[h.version; 1]);
    write_u64(memory, Address::from(LEN_OFFSET), h.len);
    write_u32(memory, Address::from(MAX_SIZE_OFFSET), h.max_size);
    memory.write(
        IS_FIXED_SIZE_OFFSET,
        &[if h.is_fixed_size { 1u8 } else { 0u8 }; 1],
    );
    write_u64(memory, Address::from(HEAD_OFF_OFFSET), h.head_off);
    write_u64(memory, Address::from(VIRT_CAP_OFFSET), h.virt_cap);
    write_u64(memory, Address::from(DIR_BASE_OFFSET), h.dir_base);
    write_u64(memory, Address::from(DIR_SLOTS_OFFSET), h.dir_slots);
    write_u64(memory, Address::from(NUM_BLOCKS_OFFSET), h.num_blocks);
    write_u64(memory, Address::from(BLOCK_SLOTS_OFFSET), h.block_slots);
    write_u64(memory, Address::from(FREE_HEAD_OFFSET), h.free_head);
    Ok(())
}

fn read_deque_header<M: Memory>(memory: &M) -> HeaderV1 {
    let mut magic = [0u8; 3];
    let mut version = [0u8; 1];
    let mut is_fixed_size = [0u8; 1];
    memory.read(0, &mut magic);
    memory.read(3, &mut version);
    let len = read_u64(memory, Address::from(LEN_OFFSET));
    let max_size = read_u32(memory, Address::from(MAX_SIZE_OFFSET));
    memory.read(IS_FIXED_SIZE_OFFSET, &mut is_fixed_size);
    HeaderV1 {
        magic,
        version: version[0],
        len,
        max_size,
        is_fixed_size: is_fixed_size[0] != 0,
        head_off: read_u64(memory, Address::from(HEAD_OFF_OFFSET)),
        virt_cap: read_u64(memory, Address::from(VIRT_CAP_OFFSET)),
        dir_base: read_u64(memory, Address::from(DIR_BASE_OFFSET)),
        dir_slots: read_u64(memory, Address::from(DIR_SLOTS_OFFSET)),
        num_blocks: read_u64(memory, Address::from(NUM_BLOCKS_OFFSET)),
        block_slots: read_u64(memory, Address::from(BLOCK_SLOTS_OFFSET)),
        free_head: read_u64(memory, Address::from(FREE_HEAD_OFFSET)),
    }
}

/// Iterator over [`VecDeque`] in logical index order (also [`DoubleEndedIterator`]).
///
/// # Example
///
/// ```
/// use ic_stable_structures::DefaultMemoryImpl;
/// use ic_stable_vec_deque::VecDeque;
///
/// let dq = VecDeque::<u64, _>::new(DefaultMemoryImpl::default()).unwrap();
/// dq.push_back(&1).unwrap();
/// dq.push_back(&2).unwrap();
/// let rev: Vec<_> = dq.iter().rev().collect();
/// assert_eq!(rev, vec![2, 1]);
/// ```
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
    use ic_stable_structures::storable::{Bound, Storable};
    use std::collections::VecDeque as StdDeque;

    #[derive(Clone, PartialEq, Eq, Debug)]
    struct Test {
        x: u64,
        y: u32,
    }

    #[derive(Clone, PartialEq, Eq, Debug)]
    struct VariableTest(String);

    impl Storable for VariableTest {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Borrowed(self.0.as_bytes())
        }

        fn into_bytes(self) -> std::vec::Vec<u8> {
            self.0.into_bytes()
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            Self(String::from_utf8(bytes.into_owned()).unwrap())
        }

        const BOUND: Bound = Bound::Bounded {
            max_size: 32,
            is_fixed_size: false,
        };
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

        const BOUND: Bound = Bound::Bounded {
            max_size: 12,
            is_fixed_size: true,
        };
    }

    fn sample(i: u64) -> Test {
        Test { x: i, y: i as u32 }
    }

    /// Slot size 65536 gives `block_slots == 4`, so a handful of elements crosses block and
    /// directory boundaries.
    #[derive(Clone, PartialEq, Eq, Debug)]
    struct BigSlot([u8; 65_536]);

    impl Storable for BigSlot {
        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Borrowed(&self.0)
        }

        fn into_bytes(self) -> std::vec::Vec<u8> {
            self.0.to_vec()
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            Self(bytes.as_ref().try_into().unwrap())
        }

        const BOUND: Bound = Bound::Bounded {
            max_size: 65_536,
            is_fixed_size: true,
        };
    }

    fn big(i: u64) -> BigSlot {
        let mut raw = [0u8; 65_536];
        raw[0..8].copy_from_slice(&i.to_le_bytes());
        BigSlot(raw)
    }

    fn assert_vec_eq(got: &[BigSlot], want: &[BigSlot]) {
        assert_eq!(got.len(), want.len(), "length mismatch");
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            let gv = u64::from_le_bytes(g.0[0..8].try_into().unwrap());
            let wv = u64::from_le_bytes(w.0[0..8].try_into().unwrap());
            assert_eq!(gv, wv, "element mismatch at index {i}");
        }
    }

    #[test]
    fn mirror_random_ops_u64() {
        let mem = ic_stable_structures::DefaultMemoryImpl::default();
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
        let mem = ic_stable_structures::DefaultMemoryImpl::default();
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
        let mem = ic_stable_structures::DefaultMemoryImpl::default();
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

    #[test]
    fn grow_preserves_wrapped_fixed_size_elements() {
        let mem = ic_stable_structures::DefaultMemoryImpl::default();
        let dq = VecDeque::<u64, _>::new(mem).unwrap();
        for value in 0..4 {
            dq.push_back(&value).unwrap();
        }
        assert_eq!(dq.pop_front(), Some(0));
        assert_eq!(dq.pop_front(), Some(1));
        dq.push_back(&4).unwrap();
        dq.push_back(&5).unwrap();

        dq.push_back(&6).unwrap();

        assert_eq!(dq.to_vec(), vec![2, 3, 4, 5, 6]);
    }

    #[test]
    fn grow_preserves_wrapped_variable_size_elements() {
        let mem = ic_stable_structures::DefaultMemoryImpl::default();
        let dq = VecDeque::<VariableTest, _>::new(mem).unwrap();
        for value in ["zero", "one", "two", "three"] {
            dq.push_back(&VariableTest(value.into())).unwrap();
        }
        assert_eq!(dq.pop_front(), Some(VariableTest("zero".into())));
        assert_eq!(dq.pop_front(), Some(VariableTest("one".into())));
        dq.push_back(&VariableTest("four-four".into())).unwrap();
        dq.push_back(&VariableTest("five-five-five".into()))
            .unwrap();

        dq.push_back(&VariableTest("six".into())).unwrap();

        assert_eq!(
            dq.to_vec(),
            ["two", "three", "four-four", "five-five-five", "six"]
                .map(|value| VariableTest(value.into()))
        );
    }

    #[test]
    fn mirror_growth_push_front_heavy() {
        let mem = ic_stable_structures::DefaultMemoryImpl::default();
        let dq = VecDeque::<BigSlot, _>::new(mem).unwrap();
        let mut std_dq = StdDeque::new();

        for step in 0u64..300 {
            dq.push_front(&big(step)).unwrap();
            std_dq.push_front(big(step));
            if step % 3 == 0 {
                dq.push_back(&big(1_000_000 + step)).unwrap();
                std_dq.push_back(big(1_000_000 + step));
            }
            if step % 7 == 0 && !std_dq.is_empty() {
                assert_eq!(dq.pop_back(), std_dq.pop_back());
            }
            if step % 11 == 0 && !std_dq.is_empty() {
                assert_eq!(dq.pop_front(), std_dq.pop_front());
            }
            if step % 23 == 0 && !std_dq.is_empty() {
                let i = (step as usize) % std_dq.len();
                let got = dq.get(i as u64);
                let want = std_dq.get(i).cloned();
                assert_eq!(
                    got.map(|b| u64::from_le_bytes(b.0[0..8].try_into().unwrap())),
                    want.map(|b| u64::from_le_bytes(b.0[0..8].try_into().unwrap())),
                    "get({i}) mismatch"
                );
            }
            assert_eq!(dq.len(), std_dq.len() as u64);
        }

        let header = dq.header();
        assert!(header.num_blocks > 16);
        assert!(header.dir_slots >= 32);
        let got = dq.to_vec();
        let want = std_dq.into_iter().collect::<std::vec::Vec<_>>();
        assert_vec_eq(&got, &want);
    }

    #[test]
    fn block_recycling_keeps_num_blocks_stable() {
        let mem = ic_stable_structures::DefaultMemoryImpl::default();
        let dq = VecDeque::<BigSlot, _>::new(mem).unwrap();

        for i in 0..16u64 {
            dq.push_back(&big(i)).unwrap();
        }
        assert_eq!(dq.header().num_blocks, 4);

        for _ in 0..4 {
            dq.pop_back();
        }
        let header = dq.header();
        assert_eq!(header.num_blocks, 3);
        assert_eq!(header.virt_cap, 12);
        assert_ne!(header.free_head, u64::MAX);

        for i in 16..20u64 {
            dq.push_back(&big(i)).unwrap();
        }
        let header = dq.header();
        assert_eq!(header.num_blocks, 4);
        assert_eq!(header.free_head, u64::MAX);
        let got = dq.to_vec();
        let want = (0..12u64)
            .chain(16..20)
            .map(big)
            .collect::<std::vec::Vec<_>>();
        assert_vec_eq(&got, &want);

        for _ in 0..4 {
            dq.pop_front();
        }
        assert_eq!(dq.header().num_blocks, 4);

        dq.push_back(&big(99)).unwrap();
        assert_eq!(dq.get(12), Some(big(99)));
        assert_eq!(dq.len(), 13);
    }

    #[test]
    fn directory_doubles_when_blocks_run_out() {
        let mem = ic_stable_structures::DefaultMemoryImpl::default();
        let dq = VecDeque::<BigSlot, _>::new(mem).unwrap();
        assert_eq!(dq.header().dir_slots, 16);

        for i in 0..68u64 {
            dq.push_back(&big(i)).unwrap();
        }
        let header = dq.header();
        assert_eq!(header.num_blocks, 17);
        assert_eq!(header.dir_slots, 32);
        assert_eq!(header.virt_cap, 68);
        for i in 0..68u64 {
            assert_eq!(dq.get(i), Some(big(i)));
        }

        for i in 68..132u64 {
            dq.push_front(&big(i)).unwrap();
        }
        let header = dq.header();
        assert_eq!(header.num_blocks, 33);
        assert_eq!(header.dir_slots, 64);
        let want = (68..132u64)
            .rev()
            .chain(0..68)
            .map(big)
            .collect::<std::vec::Vec<_>>();
        assert_vec_eq(&dq.to_vec(), &want);

        let mem = dq.into_memory();
        let dq = VecDeque::<BigSlot, _>::init(mem).unwrap();
        assert_vec_eq(&dq.to_vec(), &want);
    }

    #[test]
    fn init_roundtrip_after_growth_and_recycling() {
        let mem = ic_stable_structures::DefaultMemoryImpl::default();
        let dq = VecDeque::<BigSlot, _>::new(mem).unwrap();
        for i in 0..40u64 {
            dq.push_back(&big(i)).unwrap();
        }
        for _ in 0..12 {
            dq.pop_front();
        }
        for i in 100..140u64 {
            dq.push_front(&big(i)).unwrap();
        }
        for _ in 0..5 {
            dq.pop_back();
        }
        for i in 200..260u64 {
            dq.push_back(&big(i)).unwrap();
        }

        let expected_header = dq.header();
        let expected = dq.to_vec();
        let mem = dq.into_memory();

        let dq2 = VecDeque::<BigSlot, _>::init(mem).unwrap();
        assert_eq!(dq2.header(), expected_header);
        assert_eq!(dq2.to_vec(), expected);
    }

    #[test]
    fn empty_reset_resets_head_offset() {
        let mem = ic_stable_structures::DefaultMemoryImpl::default();
        let dq = VecDeque::<BigSlot, _>::new(mem).unwrap();

        for i in 0..9u64 {
            dq.push_back(&big(i)).unwrap();
        }
        dq.push_front(&big(100)).unwrap();
        for i in 9..12u64 {
            dq.push_back(&big(i)).unwrap();
        }
        assert!(dq.header().head_off != 0 || dq.header().len != 0);

        while dq.pop_front().is_some() {}
        let header = dq.header();
        assert_eq!(header.len, 0);
        assert_eq!(header.head_off, 0);

        let mem = dq.into_memory();
        let dq2 = VecDeque::<BigSlot, _>::init(mem).unwrap();
        assert!(dq2.is_empty());
        dq2.push_back(&big(7)).unwrap();
        dq2.push_front(&big(8)).unwrap();
        assert_eq!(dq2.to_vec(), vec![big(8), big(7)]);

        let mem = dq2.into_memory();
        let dq3 = VecDeque::<BigSlot, _>::init(mem).unwrap();
        assert_eq!(dq3.to_vec(), vec![big(8), big(7)]);
    }
}
