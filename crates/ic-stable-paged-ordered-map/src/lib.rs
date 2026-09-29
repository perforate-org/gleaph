//! Fixed-page stable-memory ordered map for `u64` keys and `u64` values, optimized for
//! predecessor/successor queries and sequential iteration over keys.
//!
//! # Creating and reopening maps
//!
//! - [`StablePagedOrderedMap::init`] — empty memory initializes a fresh map; otherwise the header is
//!   validated.
//! - [`StablePagedOrderedMap::new`] — writes an empty-map header at address 0 for a brand-new region.
//!
//! # Example
//!
//! ```
//! use ic_stable_paged_ordered_map::StablePagedOrderedMap;
//! use ic_stable_structures::DefaultMemoryImpl;
//!
//! let map = StablePagedOrderedMap::init(DefaultMemoryImpl::default()).unwrap();
//! assert!(map.is_empty());
//! ```
//!
//! # Layout
//!
//! Persistent layout (little-endian). Page indices are 1-based; id `0` means “none”. Each data page holds at most
//! [`PAGE_CAP`] sorted `(key, value)` pairs (`PAGE_CAP` × 16-byte entries after a 24-byte page header).
//! Only the current layout is supported; use fresh backing when its geometry changes.
//!
//! ```text
//! ---------------------------------------- <- Address 0
//! Magic `POM`                 ↕ 3 bytes
//! ----------------------------------------
//! Layout version              ↕ 1 byte
//! ----------------------------------------
//! Reserved alignment          ↕ 4 bytes
//! ----------------------------------------
//! Length                      ↕ 8 bytes
//! ----------------------------------------
//! Page count                  ↕ 8 bytes
//! ----------------------------------------
//! Free page head              ↕ 8 bytes
//! ----------------------------------------
//! First page                  ↕ 8 bytes
//! ----------------------------------------
//! Last page                   ↕ 8 bytes
//! ----------------------------------------
//! Directory length            ↕ 8 bytes
//! ----------------------------------------
//! Directory capacity          ↕ 8 bytes
//! ---------------------------------------- <- Address 64
//! Directory entries           ↕ DIR_CAP * 16 bytes
//! ---------------------------------------- <- PAGES_START
//! Fixed-size page slab
//! ```

#![cfg_attr(all(feature = "canbench", target_family = "wasm"), no_main)]

#[cfg(feature = "canbench")]
mod bench;

use core::fmt;
use ic_stable_structures::Memory;

/// Three-byte marker written at stable-memory offset `0`; peers use other magics so headers are not confused.
pub const MAGIC: [u8; 3] = *b"POM";
const LAYOUT_VERSION: u8 = 1;
const WASM_PAGE_SIZE: u64 = 65_536;

const HEADER_SIZE: u64 = 64;
/// Maximum number of directory entries (= maximum number of **active** data pages chained in key order).
///
/// Each directory slot stores the minimum key in that page; `insert` fails with [`Error::DirectoryFull`]
/// when this cap is reached.
pub const DIR_CAP: u64 = 8192;
const DIR_ENTRY_SIZE: u64 = 16;
const DIR_OFFSET: u64 = HEADER_SIZE;
const PAGES_START: u64 = DIR_OFFSET + DIR_CAP * DIR_ENTRY_SIZE;

/// Maximum number of `(key, value)` pairs stored in one page before inserting triggers a split.
pub const PAGE_CAP: u64 = 64;
const PAGE_HEADER_SIZE: u64 = 24;
const PAGE_ENTRY_SIZE: u64 = 16;
const PAGE_STRIDE: u64 = PAGE_HEADER_SIZE + PAGE_CAP * PAGE_ENTRY_SIZE;

const OFFSET_MAGIC: u64 = 0;
const OFFSET_VERSION: u64 = 3;
const OFFSET_LEN: u64 = 8;
const OFFSET_PAGE_COUNT: u64 = 16;
const OFFSET_FREE_HEAD: u64 = 24;
const OFFSET_FIRST_PAGE: u64 = 32;
const OFFSET_LAST_PAGE: u64 = 40;
const OFFSET_DIR_LEN: u64 = 48;
const OFFSET_DIR_CAP: u64 = 56;

const PAGE_OFFSET_FLAGS: u64 = 0;
const PAGE_OFFSET_LEN: u64 = 2;
const PAGE_OFFSET_PREV: u64 = 8;
const PAGE_OFFSET_NEXT: u64 = 16;
const PAGE_OFFSET_ENTRIES: u64 = PAGE_HEADER_SIZE;

const PAGE_FLAG_FREE: u8 = 0;
const PAGE_FLAG_ACTIVE: u8 = 1;

/// Type alias for [`StablePagedOrderedMap`], matching the `Stable*` / short name pattern used in
/// [`ic-stable-structures`](https://docs.rs/ic-stable-structures).
pub type PagedOrderedMap<M> = StablePagedOrderedMap<M>;

/// Failures while opening a map (`init`) or reserving space for an empty layout (`new`).
#[derive(Debug, PartialEq, Eq)]
pub enum InitError {
    /// Header bytes at offset 0 are not [`MAGIC`].
    BadMagic { actual: [u8; 3] },
    /// Stored layout version byte does not match the version compiled into this crate.
    IncompatibleVersion(u8),
    /// On-disk directory capacity field does not match the compile-time [`DIR_CAP`] constant.
    DirectoryCapacityMismatch { expected: u64, actual: u64 },
    /// `len`, page links, directory fields, or allocated memory size are inconsistent.
    InvalidLayout,
    /// Stable memory grow failed while writing metadata (empty map bootstrap).
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(f, "bad magic number {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(version) => {
                write!(f, "unsupported paged ordered map layout version {version}")
            }
            Self::DirectoryCapacityMismatch { expected, actual } => write!(
                f,
                "paged ordered map directory capacity mismatch: expected {expected}, got {actual}"
            ),
            Self::InvalidLayout => write!(f, "invalid paged ordered map layout"),
            Self::OutOfMemory => write!(f, "failed to allocate paged ordered map metadata"),
        }
    }
}

impl std::error::Error for InitError {}

/// Failures produced by mutation APIs after a map has been opened successfully.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The directory reached [`DIR_CAP`]; no further page splits can be recorded.
    DirectoryFull,
    /// Backing growth or edit scratch reservation failed.
    OutOfMemory,
    /// Internal invariants failed (detected via [`StablePagedOrderedMap::validate`] or a defensive check).
    Corrupt,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirectoryFull => write!(f, "paged ordered map directory is full"),
            Self::OutOfMemory => write!(f, "failed to reserve paged ordered map memory"),
            Self::Corrupt => write!(f, "paged ordered map invariant violated"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, PartialEq, Eq)]
pub struct GrowFailed {
    /// Current memory size in WebAssembly pages when the grow failed.
    pub current_size: u64,
    /// Number of additional WebAssembly pages requested.
    pub delta: u64,
}

impl fmt::Display for GrowFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "failed to grow stable memory from {} pages by {} pages",
            self.current_size, self.delta
        )
    }
}

impl std::error::Error for GrowFailed {}

/// One ordered mutation intent. Later intents see earlier intents in the same batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    Insert { key: u64, value: u64 },
    Remove { key: u64 },
}

/// A preparation failure, before any canonical map write.
#[derive(Debug)]
pub enum PrepareError {
    Edit {
        at: usize,
        source: Error,
    },
    Grow {
        at: usize,
        source: GrowFailed,
    },
    AddressOverflow {
        at: usize,
    },
    Scratch {
        at: Option<usize>,
        source: std::collections::TryReserveError,
    },
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Edit { at, source } => write!(f, "edit {at}: {source}"),
            Self::Grow { at, source } => write!(f, "edit {at}: {source}"),
            Self::AddressOverflow { at } => write!(f, "edit {at}: address overflow"),
            Self::Scratch { at, source } => write!(f, "scratch for edit {at:?}: {source}"),
        }
    }
}

impl std::error::Error for PrepareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Edit { source, .. } => Some(source),
            Self::Grow { source, .. } => Some(source),
            Self::Scratch { source, .. } => Some(source),
            Self::AddressOverflow { .. } => None,
        }
    }
}

/// Owner-bound, single-use prepared changes. Backing may have grown during preparation.
/// Discarding releases scratch and the borrow, without writing or compensating.
///
/// Mutation through the owner is excluded while the ready value remains live:
/// ```compile_fail,E0502
/// use ic_stable_paged_ordered_map::{Edit, StablePagedOrderedMap};
/// use ic_stable_structures::DefaultMemoryImpl;
/// let mut map = StablePagedOrderedMap::init(DefaultMemoryImpl::default()).unwrap();
/// let ready = map.prepare_edits(&[Edit::Insert { key: 1, value: 2 }]).unwrap();
/// map.insert(3, 4).unwrap();
/// ready.publish();
/// ```
/// A second preparation cannot overlap the first:
/// ```compile_fail,E0499
/// use ic_stable_paged_ordered_map::{Edit, StablePagedOrderedMap};
/// use ic_stable_structures::DefaultMemoryImpl;
/// let mut map = StablePagedOrderedMap::init(DefaultMemoryImpl::default()).unwrap();
/// let first = map.prepare_edits(&[Edit::Remove { key: 1 }]).unwrap();
/// let second = map.prepare_edits(&[]).unwrap();
/// first.discard();
/// second.discard();
/// ```
/// The publisher cannot be redirected to another map:
/// ```compile_fail,E0061
/// use ic_stable_paged_ordered_map::StablePagedOrderedMap;
/// use ic_stable_structures::DefaultMemoryImpl;
/// let mut map = StablePagedOrderedMap::init(DefaultMemoryImpl::default()).unwrap();
/// let mut other = StablePagedOrderedMap::init(DefaultMemoryImpl::default()).unwrap();
/// let ready = map.prepare_edits(&[]).unwrap();
/// ready.publish(&mut other);
/// ```
/// Publication consumes the capability:
/// ```compile_fail,E0382
/// use ic_stable_paged_ordered_map::StablePagedOrderedMap;
/// use ic_stable_structures::DefaultMemoryImpl;
/// let mut map = StablePagedOrderedMap::init(DefaultMemoryImpl::default()).unwrap();
/// let ready = map.prepare_edits(&[]).unwrap();
/// ready.publish();
/// ready.discard();
/// ```
#[must_use = "publish or explicitly discard the prepared changes"]
pub struct PreparedEdits<'a, M: Memory> {
    owner: &'a mut StablePagedOrderedMap<M>,
    image: EditImage,
}

impl<M: Memory> PreparedEdits<'_, M> {
    pub fn previous_values(&self) -> &[Option<u64>] {
        self.image.previous.as_slice()
    }

    pub fn publish(self) {
        self.owner.publish_image(&self.image);
    }

    pub fn discard(self) {}
}

// The scalar facade only projected allocation arithmetic to OutOfMemory. Other
// arithmetic faults remain pre-write panics there; the prepared API preserves them.
#[derive(Debug)]
enum EditFailure {
    Map(Error),
    AllocationOverflow,
    AddressOverflow,
    Grow(GrowFailed),
    Scratch(std::collections::TryReserveError),
}

#[derive(Debug)]
struct FailedEdit {
    at: Option<usize>,
    cause: EditFailure,
}

impl FailedEdit {
    fn prepared(self) -> PrepareError {
        if let EditFailure::Scratch(source) = self.cause {
            return PrepareError::Scratch {
                at: self.at,
                source,
            };
        }
        let at = self.at.expect("non-scratch failures have an edit index");
        match self.cause {
            EditFailure::Map(source) => PrepareError::Edit { at, source },
            EditFailure::Grow(source) => PrepareError::Grow { at, source },
            EditFailure::AddressOverflow | EditFailure::AllocationOverflow => {
                PrepareError::AddressOverflow { at }
            }
            EditFailure::Scratch(_) => unreachable!(),
        }
    }

    fn scalar(self) -> Error {
        match self.cause {
            EditFailure::Map(source) => source,
            EditFailure::AllocationOverflow | EditFailure::Grow(_) | EditFailure::Scratch(_) => {
                Error::OutOfMemory
            }
            EditFailure::AddressOverflow => panic!("paged-map address overflow before publication"),
        }
    }
}

// Three uses, one inline/overflow representation; storage never selects an editor.
enum Scratch<T, const N: usize> {
    Inline { items: [T; N], len: usize },
    Heap(Vec<T>),
}

impl<T: Copy + Default, const N: usize> Scratch<T, N> {
    fn new() -> Self {
        Self::Inline {
            items: [T::default(); N],
            len: 0,
        }
    }

    fn as_slice(&self) -> &[T] {
        match self {
            Self::Inline { items, len } => &items[..*len],
            Self::Heap(items) => items,
        }
    }

    fn as_mut_slice(&mut self) -> &mut [T] {
        match self {
            Self::Inline { items, len } => &mut items[..*len],
            Self::Heap(items) => items,
        }
    }

    fn capacity(&self) -> usize {
        match self {
            Self::Inline { .. } => N,
            Self::Heap(items) => items.capacity(),
        }
    }

    fn reserve(&mut self, additional: usize) -> Result<(), std::collections::TryReserveError> {
        match self {
            Self::Inline { items, len } => {
                if additional <= N - *len {
                    return Ok(());
                }
                let mut heap = Vec::new();
                let required = len
                    .checked_add(additional)
                    .ok_or_else(|| Vec::<u8>::new().try_reserve(usize::MAX).unwrap_err())?;
                heap.try_reserve(required)?;
                heap.extend_from_slice(&items[..*len]);
                *self = Self::Heap(heap);
                Ok(())
            }
            Self::Heap(items) => items.try_reserve(additional),
        }
    }

    #[inline(always)]
    fn resize_reserved(&mut self, new_len: usize) -> &mut [T] {
        assert!(new_len <= self.capacity(), "scratch was not reserved");
        match self {
            Self::Inline { items, len } => {
                *len = new_len;
                &mut items[..new_len]
            }
            Self::Heap(items) => {
                items.resize(new_len, T::default());
                items
            }
        }
    }

    fn push_reserved(&mut self, value: T) {
        match self {
            Self::Inline { items, len } => {
                items[*len] = value;
                *len += 1;
            }
            Self::Heap(items) => {
                assert!(items.len() < items.capacity(), "scratch was not reserved");
                items.push(value);
            }
        }
    }

    #[cfg(test)]
    fn dynamic_capacity(&self) -> usize {
        match self {
            Self::Inline { .. } => 0,
            Self::Heap(items) => items.capacity(),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct DirtyRange {
    address: u64,
    offset: usize,
    len: usize,
}

impl DirtyRange {
    fn end(self) -> u64 {
        self.address + self.len as u64
    }
}

struct EditImage {
    // Sorted disjoint ranges; bytes are packed in the same order, without dead images.
    ranges: Scratch<DirtyRange, 3>,
    bytes: Scratch<u8, 32>,
    previous: Scratch<Option<u64>, 1>,
    backed_end: u64,
    required_end: u64,
    required_at: usize,
}

impl EditImage {
    fn new() -> Self {
        Self {
            ranges: Scratch::new(),
            bytes: Scratch::new(),
            previous: Scratch::new(),
            backed_end: 0,
            required_end: 0,
            required_at: 0,
        }
    }

    fn read<M: Memory>(&self, memory: &M, address: u64, dst: &mut [u8]) {
        let end = address + dst.len() as u64;
        let ranges = self.ranges.as_slice();
        let first = ranges.partition_point(|r| r.end() <= address);
        let candidate = ranges.get(first);
        if address <= end && end <= self.backed_end && candidate.is_none_or(|r| r.address >= end) {
            if !dst.is_empty() {
                memory.read(address, dst);
            }
            return;
        }
        if let Some(r) = candidate
            && r.address <= address
            && end <= r.end()
        {
            let offset = r.offset + (address - r.address) as usize;
            dst.copy_from_slice(&self.bytes.as_slice()[offset..offset + dst.len()]);
            return;
        }
        // Only planned new pages may extend past current backing. Their missing
        // padding is zero; selected-page admission checks minted geometry first.
        let backed = self
            .backed_end
            .saturating_sub(address)
            .min(dst.len() as u64) as usize;
        if backed != 0 {
            memory.read(address, &mut dst[..backed]);
        }
        dst[backed..].fill(0);
        for r in &ranges[first..] {
            if r.address >= end {
                break;
            }
            let lo = address.max(r.address);
            let hi = end.min(r.end());
            let source = r.offset + (lo - r.address) as usize;
            dst[(lo - address) as usize..(hi - address) as usize]
                .copy_from_slice(&self.bytes.as_slice()[source..source + (hi - lo) as usize]);
        }
    }

    fn write(&mut self, address: u64, value: &[u8]) {
        let window = self.cover(address, value.len(), |_, _| {});
        window.copy_from_slice(value);
    }

    fn cover(
        &mut self,
        address: u64,
        count: usize,
        mut fill_gap: impl FnMut(u64, &mut [u8]),
    ) -> &mut [u8] {
        let end = address + count as u64;
        let ranges = self.ranges.as_slice();
        // An empty image has no packed bytes to move, even after reserving scratch.
        if count != 0 && ranges.is_empty() {
            let bytes = self.bytes.resize_reserved(count);
            fill_gap(address, bytes);
            self.ranges.push_reserved(DirtyRange {
                address,
                offset: 0,
                len: count,
            });
            return bytes;
        }
        let first = ranges.partition_point(|r| r.end() < address);
        if let Some(r) = ranges.get(first)
            && r.address <= address
            && end <= r.end()
        {
            let offset = r.offset + (address - r.address) as usize;
            return &mut self.bytes.as_mut_slice()[offset..offset + count];
        }
        let last = first + ranges[first..].partition_point(|r| r.address <= end);
        let start = ranges
            .get(first)
            .filter(|_| first < last)
            .map_or(address, |r| address.min(r.address));
        let stop = if first < last {
            end.max(ranges[last - 1].end())
        } else {
            end
        };
        let offset = ranges
            .get(first)
            .map_or(self.bytes.as_slice().len(), |r| r.offset);
        let old_stop = if first < last {
            ranges[last - 1].offset + ranges[last - 1].len
        } else {
            offset
        };
        let len = (stop - start) as usize;
        let added = len - (old_stop - offset);
        let old_bytes = self.bytes.as_slice().len();
        let bytes = self.bytes.resize_reserved(old_bytes + added);
        bytes.copy_within(old_stop..old_bytes, offset + len);
        for r in ranges[first..last].iter().rev() {
            let to = offset + (r.address - start) as usize;
            bytes.copy_within(r.offset..r.offset + r.len, to);
        }
        let mut cursor = start;
        for r in &ranges[first..last] {
            if cursor < r.address {
                let from = offset + (cursor - start) as usize;
                let to = offset + (r.address - start) as usize;
                fill_gap(cursor, &mut bytes[from..to]);
            }
            cursor = r.end();
        }
        if cursor < stop {
            let from = offset + (cursor - start) as usize;
            fill_gap(cursor, &mut bytes[from..offset + len]);
        }
        let old_ranges = ranges.len();
        // Remove merged descriptors, or insert one when there was no overlap.
        let new_ranges = old_ranges + 1 - (last - first);
        let view = if new_ranges > old_ranges {
            let grown = self.ranges.resize_reserved(new_ranges);
            grown.copy_within(last..old_ranges, first + 1);
            grown
        } else {
            // Shrink or equal length: move the still-live suffix on the current
            // view first, then resize to expose the final descriptor set.
            self.ranges
                .as_mut_slice()
                .copy_within(last..old_ranges, first + 1);
            self.ranges.resize_reserved(new_ranges)
        };
        view[first] = DirtyRange {
            address: start,
            offset,
            len,
        };
        for r in &mut view[first + 1..] {
            r.offset += added;
        }
        let window_start = offset + (address - start) as usize;
        &mut bytes[window_start..window_start + count]
    }
}

fn checked_page_offset(page: u64) -> Result<u64, EditFailure> {
    if page == 0 {
        return Err(EditFailure::AddressOverflow);
    }
    if page - 1 > (u64::MAX - PAGES_START) / PAGE_STRIDE {
        return Err(EditFailure::AddressOverflow);
    }
    Ok(page_offset(page))
}

struct EditPlan<'a, M: Memory> {
    owner: &'a StablePagedOrderedMap<M>,
    image: &'a mut EditImage,
    initial_pages: u64,
}

/// One intent's read-only selection: the already-validated directory position and
/// page facts plus the in-page position. Stack-local for a single dispatch; never
/// stored in the image, returned publicly, or reused across intents.
#[derive(Clone, Copy)]
struct EditLocation {
    dir_pos: u64,
    dir: DirEntry,
    offset: u64,
    header: PageHeader,
    position: Result<u16, u16>,
}

impl<'a, M: Memory> EditPlan<'a, M> {
    fn new(
        owner: &'a StablePagedOrderedMap<M>,
        image: &'a mut EditImage,
    ) -> Result<Self, EditFailure> {
        image.backed_end = owner
            .memory
            .size()
            .checked_mul(WASM_PAGE_SIZE)
            .ok_or(EditFailure::AddressOverflow)?;
        Ok(Self {
            owner,
            image,
            initial_pages: owner.read_page_count(),
        })
    }

    fn read<const N: usize>(&self, address: u64) -> [u8; N] {
        let mut bytes = [0; N];
        self.image.read(&self.owner.memory, address, &mut bytes);
        bytes
    }

    fn u64(&self, address: u64) -> u64 {
        u64::from_le_bytes(self.read(address))
    }

    fn set_u64(&mut self, address: u64, value: u64) {
        self.image.write(address, &value.to_le_bytes());
    }

    fn dir_len(probe: impl Fn(u64, &mut [u8])) -> Result<u64, EditFailure> {
        let mut bytes = [0; 8];
        probe(OFFSET_DIR_LEN, &mut bytes);
        let len = u64::from_le_bytes(bytes);
        if len > DIR_CAP {
            return Err(EditFailure::Map(Error::Corrupt));
        }
        Ok(len)
    }

    fn directory(&self, index: u64) -> DirEntry {
        let bytes = self.read::<16>(dir_entry_offset(index));
        DirEntry::decode(&bytes)
    }

    fn set_directory(&mut self, index: u64, entry: DirEntry) {
        self.set_entry(dir_entry_offset(index), (entry.min_key, entry.page_id));
    }

    fn insert_directory(&mut self, pos: u64, len: u64, entry: DirEntry) {
        let count = ((len - pos + 1) * DIR_ENTRY_SIZE) as usize;
        let old_end = dir_entry_offset(len);
        let memory = &self.owner.memory;
        let window = self
            .image
            .cover(dir_entry_offset(pos), count, |address, dst| {
                let read = old_end.saturating_sub(address).min(dst.len() as u64) as usize;
                if read != 0 {
                    memory.read(address, &mut dst[..read]);
                }
            });
        window.copy_within(..count - DIR_ENTRY_SIZE as usize, DIR_ENTRY_SIZE as usize);
        self.set_directory(pos, entry);
    }

    fn remove_directory(&mut self, pos: u64, len: u64) {
        let count = ((len - pos) * DIR_ENTRY_SIZE) as usize;
        let memory = &self.owner.memory;
        let bytes = self
            .image
            .cover(dir_entry_offset(pos), count, |address, dst| {
                memory.read(address, dst)
            });
        bytes.copy_within(DIR_ENTRY_SIZE as usize.., 0);
        bytes[count - DIR_ENTRY_SIZE as usize..].fill(0);
    }

    fn set_entry(&mut self, address: u64, entry: (u64, u64)) {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&entry.0.to_le_bytes());
        bytes[8..].copy_from_slice(&entry.1.to_le_bytes());
        self.image.write(address, &bytes);
    }

    fn page(
        &self,
        page: u64,
        probe: impl Fn(u64, &mut [u8]),
    ) -> Result<(u64, PageHeader, u64), EditFailure> {
        let offset = checked_page_offset(page)?;
        let end = offset
            .checked_add(PAGE_STRIDE)
            .ok_or(EditFailure::AddressOverflow)?;
        let count = if self.image.ranges.as_slice().is_empty() {
            // No staged write this prepare: canonical count unchanged since EditPlan::new.
            self.initial_pages
        } else {
            let mut bytes = [0; 8];
            probe(OFFSET_PAGE_COUNT, &mut bytes);
            u64::from_le_bytes(bytes)
        };
        if page > count || (page <= self.initial_pages && end > self.image.backed_end) {
            return Err(EditFailure::Map(Error::Corrupt));
        }
        let mut bytes = [0; (PAGE_HEADER_SIZE + 8) as usize];
        probe(offset, &mut bytes);
        let header = PageHeader::decode(bytes[..PAGE_HEADER_SIZE as usize].try_into().unwrap());
        if header.prev > count || header.next > count {
            return Err(EditFailure::Map(Error::Corrupt));
        }
        let first_key = u64::from_le_bytes(bytes[PAGE_HEADER_SIZE as usize..].try_into().unwrap());
        Ok((offset, header, first_key))
    }

    fn reserve(&mut self, bytes: usize, ranges: usize) -> Result<(), EditFailure> {
        self.owner
            .reserve_scratch(&mut self.image.bytes, bytes, "bytes")?;
        self.owner
            .reserve_scratch(&mut self.image.ranges, ranges, "ranges")?;
        Ok(())
    }

    fn allocate_page(&mut self, at: usize) -> Result<u64, EditFailure> {
        let free = self.u64(OFFSET_FREE_HEAD);
        if free != 0 {
            let (_, header, _) = self.page(free, |address, dst| {
                self.image.read(&self.owner.memory, address, dst)
            })?;
            let next = header.free_next().map_err(EditFailure::Map)?;
            if next == free || next > self.u64(OFFSET_PAGE_COUNT) {
                return Err(EditFailure::Map(Error::Corrupt));
            }
            self.set_u64(OFFSET_FREE_HEAD, next);
            return Ok(free);
        }
        let page = self
            .u64(OFFSET_PAGE_COUNT)
            .checked_add(1)
            .ok_or(EditFailure::AllocationOverflow)?;
        let end = checked_page_offset(page)?
            .checked_add(PAGE_STRIDE)
            .ok_or(EditFailure::AllocationOverflow)?;
        if end > self.image.required_end {
            self.image.required_end = end;
            self.image.required_at = at;
        }
        self.set_u64(OFFSET_PAGE_COUNT, page);
        Ok(page)
    }

    fn set_page(&mut self, page: u64, header: PageHeader) -> Result<(), EditFailure> {
        let offset = checked_page_offset(page)?;
        self.image.write(offset, &header.encode());
        Ok(())
    }

    fn update_neighbor(
        &mut self,
        page: u64,
        field: u64,
        old: u64,
        new: u64,
    ) -> Result<(), EditFailure> {
        let (offset, header, _) = self.page(page, |address, dst| {
            self.image.read(&self.owner.memory, address, dst)
        })?;
        let link = if field == PAGE_OFFSET_PREV {
            header.prev
        } else {
            header.next
        };
        if header.flags != PAGE_FLAG_ACTIVE || link != old {
            return Err(EditFailure::Map(Error::Corrupt));
        }
        self.set_u64(offset + field, new);
        Ok(())
    }

    // Keep split-only temporaries out of the common preparation frame.
    #[inline(never)]
    fn split_page(
        &mut self,
        page: u64,
        entry: (u64, u64),
        at: usize,
        pos: u16,
    ) -> Result<(), EditFailure> {
        let dir_len =
            Self::dir_len(|address, dst| self.image.read(&self.owner.memory, address, dst))?;
        if dir_len >= DIR_CAP {
            return Err(EditFailure::Map(Error::DirectoryFull));
        }
        let dir_pos = (0..dir_len)
            .find(|&i| self.directory(i).page_id == page)
            .ok_or(EditFailure::Map(Error::Corrupt))?;
        let (offset, header, _) = self.page(page, |address, dst| {
            self.image.read(&self.owner.memory, address, dst)
        })?;
        if header.flags != PAGE_FLAG_ACTIVE || u64::from(header.len) != PAGE_CAP {
            return Err(EditFailure::Map(Error::Corrupt));
        }
        let start = offset + PAGE_OFFSET_ENTRIES;
        let next_len = self
            .u64(OFFSET_LEN)
            .checked_add(1)
            .ok_or(EditFailure::Map(Error::Corrupt))?;
        let directory_bytes = ((dir_len - dir_pos) * DIR_ENTRY_SIZE) as usize;
        self.reserve(
            ((PAGE_CAP + 1) * PAGE_ENTRY_SIZE + PAGE_HEADER_SIZE + HEADER_SIZE) as usize
                + directory_bytes,
            12,
        )?;
        let right = self.allocate_page(at)?;
        if right == page || right == header.next || right == header.prev {
            return Err(EditFailure::Map(Error::Corrupt));
        }
        let right_offset = checked_page_offset(right)?;
        let split = header.len.div_ceil(2);
        // Cover the right destination once; its entries are written before any read.
        self.image.cover(
            right_offset + PAGE_OFFSET_ENTRIES,
            usize::from(header.len + 1 - split) * PAGE_ENTRY_SIZE as usize,
            |_, _| {},
        );
        // Unstaged source entries are backed; unbacked initialized entries are staged.
        let memory = &self.owner.memory;
        self.image.cover(
            start,
            usize::from(split) * PAGE_ENTRY_SIZE as usize,
            |address, bytes| memory.read(address, bytes),
        );
        // Snapshot the prospective right sources before either side moves. The
        // owned bytes survive image relocation without discarding earlier edits.
        let source_index = split - u16::from(pos < split);
        let mut right_entries = [0; ((PAGE_CAP / 2 + 1) * PAGE_ENTRY_SIZE) as usize];
        let source_bytes = if pos < split {
            right_entries.len()
        } else {
            right_entries.len() - PAGE_ENTRY_SIZE as usize
        };
        self.image.read(
            &self.owner.memory,
            start + u64::from(source_index) * PAGE_ENTRY_SIZE,
            &mut right_entries[..source_bytes],
        );
        let right_start = right_offset + PAGE_OFFSET_ENTRIES;
        if pos < split {
            self.image
                .write(right_start, &right_entries[..source_bytes]);
        } else {
            let prefix_bytes = usize::from(pos - split) * PAGE_ENTRY_SIZE as usize;
            self.image
                .write(right_start, &right_entries[..prefix_bytes]);
            self.set_entry(right_start + prefix_bytes as u64, entry);
            self.image.write(
                right_start + prefix_bytes as u64 + PAGE_ENTRY_SIZE,
                &right_entries[prefix_bytes..source_bytes],
            );
        }
        if pos < split {
            let count = usize::from(split) * PAGE_ENTRY_SIZE as usize;
            let from = usize::from(pos) * PAGE_ENTRY_SIZE as usize;
            let memory = &self.owner.memory;
            let left = self
                .image
                .cover(start, count, |address, bytes| memory.read(address, bytes));
            left.copy_within(
                from..count - PAGE_ENTRY_SIZE as usize,
                from + PAGE_ENTRY_SIZE as usize,
            );
            self.set_entry(start + u64::from(pos) * PAGE_ENTRY_SIZE, entry);
        }
        self.image
            .write(offset + PAGE_OFFSET_LEN, &split.to_le_bytes());
        self.set_u64(offset + PAGE_OFFSET_NEXT, right);
        self.set_page(
            right,
            PageHeader {
                flags: PAGE_FLAG_ACTIVE,
                len: header.len + 1 - split,
                prev: page,
                next: header.next,
            },
        )?;
        if header.next == 0 {
            self.set_u64(OFFSET_LAST_PAGE, right);
        } else {
            self.update_neighbor(header.next, PAGE_OFFSET_PREV, page, right)?;
        }
        if pos == 0 {
            self.set_u64(dir_entry_offset(dir_pos), entry.0);
        }
        let minimum = self.u64(right_offset + PAGE_OFFSET_ENTRIES);
        self.insert_directory(
            dir_pos + 1,
            dir_len,
            DirEntry {
                min_key: minimum,
                page_id: right,
            },
        );
        self.set_u64(OFFSET_DIR_LEN, dir_len + 1);
        self.set_u64(OFFSET_LEN, next_len);
        Ok(())
    }

    fn insert_empty(&mut self, entry: (u64, u64), at: usize) -> Result<(), EditFailure> {
        if self.u64(OFFSET_LEN) != 0 {
            return Err(EditFailure::Map(Error::Corrupt));
        }
        self.reserve(
            (PAGE_HEADER_SIZE + 2 * PAGE_ENTRY_SIZE + HEADER_SIZE) as usize,
            9,
        )?;
        let page = self.allocate_page(at)?;
        let offset = checked_page_offset(page)?;
        self.set_page(
            page,
            PageHeader {
                flags: PAGE_FLAG_ACTIVE,
                len: 1,
                prev: 0,
                next: 0,
            },
        )?;
        self.set_entry(offset + PAGE_OFFSET_ENTRIES, entry);
        self.set_directory(
            0,
            DirEntry {
                min_key: entry.0,
                page_id: page,
            },
        );
        self.set_u64(OFFSET_DIR_LEN, 1);
        self.set_u64(OFFSET_FIRST_PAGE, page);
        self.set_u64(OFFSET_LAST_PAGE, page);
        self.set_u64(OFFSET_LEN, 1);
        Ok(())
    }

    fn remove_page(
        &mut self,
        dir_pos: u64,
        page: u64,
        header: PageHeader,
    ) -> Result<(), EditFailure> {
        let dir_len =
            Self::dir_len(|address, dst| self.image.read(&self.owner.memory, address, dst))?;
        self.reserve(
            ((dir_len - dir_pos) * DIR_ENTRY_SIZE + PAGE_HEADER_SIZE + HEADER_SIZE) as usize,
            10,
        )?;
        if header.prev == 0 {
            self.set_u64(OFFSET_FIRST_PAGE, header.next);
        } else {
            self.update_neighbor(header.prev, PAGE_OFFSET_NEXT, page, header.next)?;
        }
        if header.next == 0 {
            self.set_u64(OFFSET_LAST_PAGE, header.prev);
        } else {
            self.update_neighbor(header.next, PAGE_OFFSET_PREV, page, header.prev)?;
        }
        self.remove_directory(dir_pos, dir_len);
        self.set_u64(OFFSET_DIR_LEN, dir_len - 1);
        let free_next = self.u64(OFFSET_FREE_HEAD);
        if free_next == page || free_next > self.u64(OFFSET_PAGE_COUNT) {
            return Err(EditFailure::Map(Error::Corrupt));
        }
        self.set_page(
            page,
            PageHeader {
                flags: PAGE_FLAG_FREE,
                len: 0,
                prev: free_next,
                next: 0,
            },
        )?;
        self.set_u64(OFFSET_FREE_HEAD, page);
        Ok(())
    }

    /// One read-only selection for one intent: checked directory length, directory
    /// location, selected-page validation and in-page position. Metadata,
    /// directory entries and keys all use the intent's selected `probe`;
    /// prior-value reads remain in the mutation dispatch.
    fn lookup(
        &self,
        edit: Edit,
        #[cfg(test)] at: usize,
        probe: impl Fn(u64, &mut [u8]),
    ) -> Result<Option<EditLocation>, EditFailure> {
        let key = match edit {
            Edit::Insert { key, .. } | Edit::Remove { key } => key,
        };
        let dir_len = Self::dir_len(&probe)?;
        if dir_len == 0 {
            return Ok(None);
        }
        let mut entry_bytes = [0u8; DIR_ENTRY_SIZE as usize];
        let (dir_pos, dir) = match locate_directory(dir_len, key, |i| {
            probe(dir_entry_offset(i), &mut entry_bytes);
            DirEntry::decode(&entry_bytes)
        }) {
            Some(pair) => pair,
            None if matches!(edit, Edit::Remove { .. }) => return Ok(None),
            None => {
                probe(dir_entry_offset(0), &mut entry_bytes);
                (0, DirEntry::decode(&entry_bytes))
            }
        };
        let (offset, header, first_key) = self.page(dir.page_id, &probe)?;
        #[cfg(test)]
        self.owner
            .observe_work(|w| w.selected = Some((at, dir.page_id, header.flags)));
        if header.flags != PAGE_FLAG_ACTIVE
            || header.len == 0
            || u64::from(header.len) > PAGE_CAP
            || header.prev == dir.page_id
            || header.next == dir.page_id
        {
            return Err(EditFailure::Map(Error::Corrupt));
        }
        let start = offset + PAGE_OFFSET_ENTRIES;
        if first_key != dir.min_key {
            return Err(EditFailure::Map(Error::Corrupt));
        }
        let position = search_page(header.len, key, |i| {
            if i == 0 {
                dir.min_key
            } else {
                let mut key_bytes = [0u8; 8];
                probe(start + u64::from(i) * PAGE_ENTRY_SIZE, &mut key_bytes);
                u64::from_le_bytes(key_bytes)
            }
        });
        Ok(Some(EditLocation {
            dir_pos,
            dir,
            offset,
            header,
            position,
        }))
    }

    fn apply_edit(&mut self, edit: Edit, at: usize) -> Result<Option<u64>, EditFailure> {
        let key = match edit {
            Edit::Insert { key, .. } | Edit::Remove { key } => key,
        };
        let use_backing =
            self.image.ranges.as_slice().is_empty() && self.image.backed_end >= PAGES_START;
        #[cfg(test)]
        self.owner.observe_work(|w| {
            if use_backing {
                w.backing_lookups += 1;
            } else {
                w.prospective_lookups += 1;
            }
        });
        let location = if use_backing {
            self.lookup(
                edit,
                #[cfg(test)]
                at,
                |address, dst| self.owner.memory.read(address, dst),
            )?
        } else {
            self.lookup(
                edit,
                #[cfg(test)]
                at,
                |address, dst| self.image.read(&self.owner.memory, address, dst),
            )?
        };
        let Some(location) = location else {
            if let Edit::Insert { value, .. } = edit {
                self.insert_empty((key, value), at)?;
            }
            return Ok(None);
        };
        let EditLocation {
            dir_pos,
            dir,
            offset,
            header,
            position,
        } = location;
        let start = offset + PAGE_OFFSET_ENTRIES;
        match (edit, position) {
            (Edit::Insert { value, .. }, Ok(pos)) => {
                let address = start + u64::from(pos) * PAGE_ENTRY_SIZE + 8;
                let previous = self.u64(address);
                self.reserve(8, 1)?;
                self.set_u64(address, value);
                #[cfg(test)]
                if at == 0 {
                    // Observe the staged bytes, not an echo of the requested value.
                    let staged = self.u64(address);
                    self.owner.observe_work(|w| {
                        w.first_staged = Some((Some(previous), Some(staged)));
                    });
                }
                Ok(Some(previous))
            }
            (Edit::Remove { .. }, Err(_)) => Ok(None),
            (Edit::Insert { value, .. }, Err(pos)) => {
                if u64::from(header.len) == PAGE_CAP {
                    self.split_page(dir.page_id, (key, value), at, pos)?;
                    return Ok(None);
                }
                let len = self
                    .u64(OFFSET_LEN)
                    .checked_add(1)
                    .ok_or(EditFailure::Map(Error::Corrupt))?;
                let bytes = usize::from(header.len - pos + 1) * PAGE_ENTRY_SIZE as usize
                    + 10
                    + if pos == 0 { 8 } else { 0 };
                self.reserve(bytes, if pos == 0 { 4 } else { 3 })?;
                // Avoid relocating freshly staged payload to insert lower metadata.
                // Movement uses the captured location, not the newly staged lengths.
                self.set_u64(OFFSET_LEN, len);
                if pos == 0 {
                    self.set_u64(dir_entry_offset(dir_pos), key);
                }
                self.image
                    .write(offset + PAGE_OFFSET_LEN, &(header.len + 1).to_le_bytes());
                if pos < header.len {
                    let count = usize::from(header.len - pos + 1) * PAGE_ENTRY_SIZE as usize;
                    let old_end = start + u64::from(header.len) * PAGE_ENTRY_SIZE;
                    let memory = &self.owner.memory;
                    let window = self.image.cover(
                        start + u64::from(pos) * PAGE_ENTRY_SIZE,
                        count,
                        |address, dst| {
                            // Old live entries on unbacked new pages are already staged.
                            // Do not read the new slot beyond the old live prefix.
                            let read =
                                old_end.saturating_sub(address).min(dst.len() as u64) as usize;
                            if read != 0 {
                                memory.read(address, &mut dst[..read]);
                            }
                        },
                    );
                    window
                        .copy_within(..count - PAGE_ENTRY_SIZE as usize, PAGE_ENTRY_SIZE as usize);
                }
                self.set_entry(start + u64::from(pos) * PAGE_ENTRY_SIZE, (key, value));
                Ok(None)
            }
            (Edit::Remove { .. }, Ok(pos)) => {
                let previous = self.u64(start + u64::from(pos) * PAGE_ENTRY_SIZE + 8);
                let len = self
                    .u64(OFFSET_LEN)
                    .checked_sub(1)
                    .ok_or(EditFailure::Map(Error::Corrupt))?;
                if header.len == 1 {
                    self.remove_page(dir_pos, dir.page_id, header)?;
                } else {
                    let bytes = usize::from(header.len - pos - 1) * PAGE_ENTRY_SIZE as usize
                        + 10
                        + if pos == 0 { 8 } else { 0 };
                    self.reserve(
                        bytes,
                        if pos == 0 {
                            4
                        } else if pos + 1 == header.len {
                            2
                        } else {
                            3
                        },
                    )?;
                    if pos + 1 < header.len {
                        // Save the final source before relocation, without dirtying its slot.
                        let last =
                            self.read::<16>(start + u64::from(header.len - 1) * PAGE_ENTRY_SIZE);
                        let count = usize::from(header.len - pos - 1) * PAGE_ENTRY_SIZE as usize;
                        let memory = &self.owner.memory;
                        let window = self.image.cover(
                            start + u64::from(pos) * PAGE_ENTRY_SIZE,
                            count,
                            |address, dst| memory.read(address, dst),
                        );
                        window.copy_within(PAGE_ENTRY_SIZE as usize.., 0);
                        window[count - PAGE_ENTRY_SIZE as usize..].copy_from_slice(&last);
                    }
                    self.image
                        .write(offset + PAGE_OFFSET_LEN, &(header.len - 1).to_le_bytes());
                    if pos == 0 {
                        let minimum = self.u64(start);
                        self.set_u64(dir_entry_offset(dir_pos), minimum);
                    }
                }
                self.set_u64(OFFSET_LEN, len);
                if len == 0 {
                    if self.u64(OFFSET_DIR_LEN) != 0 {
                        return Err(EditFailure::Map(Error::Corrupt));
                    }
                    self.set_u64(OFFSET_PAGE_COUNT, 0);
                    self.set_u64(OFFSET_FREE_HEAD, 0);
                }
                Ok(Some(previous))
            }
        }
    }
}

fn locate_directory(
    len: u64,
    key: u64,
    mut entry_at: impl FnMut(u64) -> DirEntry,
) -> Option<(u64, DirEntry)> {
    let mut lo = 0;
    let mut hi = len;
    let mut selected = None;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let entry = entry_at(mid);
        if entry.min_key <= key {
            lo = mid + 1;
            selected = Some((mid, entry));
        } else {
            hi = mid;
        }
    }
    selected
}

fn search_page(len: u16, key: u64, mut key_at: impl FnMut(u16) -> u64) -> Result<u16, u16> {
    let mut lo = 0;
    let mut hi = len;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match key_at(mid).cmp(&key) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok(mid),
        }
    }
    Err(lo)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirEntry {
    min_key: u64,
    page_id: u64,
}

impl DirEntry {
    fn decode(bytes: &[u8; DIR_ENTRY_SIZE as usize]) -> Self {
        Self {
            min_key: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            page_id: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageHeader {
    flags: u8,
    len: u16,
    // Shared link slot: previous active page, or next free page when flags == FREE.
    prev: u64,
    next: u64,
}

impl PageHeader {
    fn decode(bytes: &[u8; PAGE_HEADER_SIZE as usize]) -> Self {
        Self {
            flags: bytes[PAGE_OFFSET_FLAGS as usize],
            len: u16::from_le_bytes(
                bytes[PAGE_OFFSET_LEN as usize..PAGE_OFFSET_LEN as usize + 2]
                    .try_into()
                    .unwrap(),
            ),
            prev: u64::from_le_bytes(
                bytes[PAGE_OFFSET_PREV as usize..PAGE_OFFSET_PREV as usize + 8]
                    .try_into()
                    .unwrap(),
            ),
            next: u64::from_le_bytes(
                bytes[PAGE_OFFSET_NEXT as usize..PAGE_OFFSET_NEXT as usize + 8]
                    .try_into()
                    .unwrap(),
            ),
        }
    }

    fn encode(self) -> [u8; PAGE_HEADER_SIZE as usize] {
        let mut bytes = [0; PAGE_HEADER_SIZE as usize];
        bytes[PAGE_OFFSET_FLAGS as usize] = self.flags;
        bytes[PAGE_OFFSET_LEN as usize..PAGE_OFFSET_LEN as usize + 2]
            .copy_from_slice(&self.len.to_le_bytes());
        bytes[PAGE_OFFSET_PREV as usize..PAGE_OFFSET_PREV as usize + 8]
            .copy_from_slice(&self.prev.to_le_bytes());
        bytes[PAGE_OFFSET_NEXT as usize..PAGE_OFFSET_NEXT as usize + 8]
            .copy_from_slice(&self.next.to_le_bytes());
        bytes
    }

    fn free_next(self) -> Result<u64, Error> {
        if self.flags != PAGE_FLAG_FREE || self.len != 0 || self.next != 0 {
            return Err(Error::Corrupt);
        }
        Ok(self.prev)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    pub magic: [u8; 3],
    pub version: u8,
    pub len: u64,
    pub page_count: u64,
    pub free_head: u64,
    pub first_page: u64,
    pub last_page: u64,
    pub dir_len: u64,
    pub dir_cap: u64,
}

/// Ordered key/value store in stable memory: unique `u64` keys map to `u64` values with `O(log n)` page
/// directory lookup plus `O(log page_len)` search inside a page.
///
/// Keys are stored sorted. Navigational queries ([`predecessor`](Self::predecessor),
/// [`successor`](Self::successor), [`first`](Self::first), [`last`](Self::last)) are efficient for range-style
/// workloads on the IC.
///
/// Mutating methods take `&self` because [`Memory`] uses interior mutability, matching
/// [`BTreeMap`](ic_stable_structures::BTreeMap) in `ic-stable-structures`.
pub struct StablePagedOrderedMap<M: Memory> {
    memory: M,
    #[cfg(test)]
    work: std::cell::Cell<WorkObservation>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WorkObservation {
    observed: bool,
    dirty_bytes: usize,
    ranges: usize,
    byte_capacity: usize,
    range_capacity: usize,
    result_capacity: usize,
    reserves: usize,
    grows: usize,
    grown_pages: u64,
    selected: Option<(usize, u64, u8)>,
    first_staged: Option<(Option<u64>, Option<u64>)>,
    completed: usize,
    fail_site: Option<&'static str>,
    backing_lookups: usize,
    prospective_lookups: usize,
}

#[cfg(any(test, feature = "canbench"))]
fn seed_scalar_work<M: Memory>(map: &StablePagedOrderedMap<M>) {
    for i in 0..64 {
        let key = 4 * i;
        map.insert(key, 0xa500_0000_0000_0000 | key).unwrap();
    }
    map.insert(1000, 0xa500_0000_0000_03e8).unwrap();
    for i in 0..32 {
        let key = 4 * i + 2;
        map.insert(key, 0xa500_0000_0000_0000 | key).unwrap();
    }
}

impl<M: Memory> StablePagedOrderedMap<M> {
    /// Creates a **new** empty map, writing the header at address `0`.
    ///
    /// The `memory` region must be reserved for this structure only (see crate-level **Exclusive memory regions**).
    /// This matches [`BTreeMap::new`](ic_stable_structures::BTreeMap::new): callers that own a fresh buffer use
    /// `new`, while [`Self::init`] also reopens persisted state.
    ///
    /// # Errors
    ///
    /// Returns [`GrowFailed`] when growing stable memory for the initial header layout fails.
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        write_header(
            &memory,
            &HeaderV1 {
                magic: MAGIC,
                version: LAYOUT_VERSION,
                len: 0,
                page_count: 0,
                free_head: 0,
                first_page: 0,
                last_page: 0,
                dir_len: 0,
                dir_cap: DIR_CAP,
            },
        )?;
        safe_write(&memory, PAGES_START - 1, &[0])?;
        Ok(Self {
            memory,
            #[cfg(test)]
            work: std::cell::Cell::default(),
        })
    }

    /// Opens a map backed by `memory`.
    ///
    /// - If `memory` is empty (`size() == 0`), a fresh header is written (same end state as [`Self::new`]).
    /// - Otherwise the header is validated; on success the existing map is used.
    ///
    /// # Errors
    ///
    /// Returns [`InitError`] when the header is missing, corrupt, or incompatible, or when bootstrapping
    /// empty memory hits an out-of-memory grow failure.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let h = read_header(&memory);
        validate_header(&memory, &h)?;
        Ok(Self {
            memory,
            #[cfg(test)]
            work: std::cell::Cell::default(),
        })
    }

    /// Returns the stable header fields currently persisted in memory.
    pub fn header(&self) -> HeaderV1 {
        read_header(&self.memory)
    }

    /// Consumes `self` and returns the underlying [`Memory`] handle (e.g. to persist, wrap, or re-open).
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Returns the current stable-memory size in Wasm pages.
    ///
    /// This is a read-only inspection primitive for owners that need to include the backing
    /// region in a stable-memory snapshot.
    pub fn memory_size(&self) -> u64 {
        self.memory.size()
    }

    /// Number of `(key, value)` pairs in the map.
    pub fn len(&self) -> u64 {
        read_u64(&self.memory, OFFSET_LEN)
    }

    /// `true` when [`len`](Self::len) is zero.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the value for `key`, or `None` if the key is absent.
    pub fn get(&self, key: u64) -> Option<u64> {
        let page_id = self.find_page_for_key(key)?;
        let pos = self.page_search(page_id, key).ok()?;
        Some(self.read_entry(page_id, pos).1)
    }

    /// Inserts `(key, value)` or updates the value if `key` already exists.
    ///
    /// # Errors
    ///
    /// - [`Error::DirectoryFull`] — cannot add another page to the directory during a split.
    /// - [`Error::OutOfMemory`] — backing growth or edit scratch reservation failed.
    /// - [`Error::Corrupt`] — broken directory linkage (should not happen on a map that only used this API).
    ///
    /// On success: `Ok(None)` means the key was new; `Ok(Some(previous))` means the key existed and the value was replaced.
    pub fn insert(&self, key: u64, value: u64) -> Result<Option<u64>, Error> {
        self.scalar_edit(Edit::Insert { key, value })
    }

    /// Prepare ordered edits without publishing canonical bytes. Each prior value
    /// refers to the prospective state immediately before that intent. An error
    /// discards all private changes; successful backing growth may remain.
    ///
    /// The map must exclusively own its backing region. Independently reopened
    /// maps and raw mutable Memory aliases are not policed by the Rust borrow.
    pub fn prepare_edits(&mut self, edits: &[Edit]) -> Result<PreparedEdits<'_, M>, PrepareError> {
        let mut image = EditImage::new();
        self.prepare_image(edits, &mut image)
            .map_err(FailedEdit::prepared)?;
        Ok(PreparedEdits { owner: self, image })
    }

    fn scalar_edit(&self, edit: Edit) -> Result<Option<u64>, Error> {
        let mut image = EditImage::new();
        self.prepare_image(&[edit], &mut image)
            .map_err(FailedEdit::scalar)?;
        let previous = image.previous.as_slice()[0];
        self.publish_image(&image);
        Ok(previous)
    }

    fn prepare_image(&self, edits: &[Edit], image: &mut EditImage) -> Result<(), FailedEdit> {
        #[cfg(test)]
        self.work.set(WorkObservation {
            observed: true,
            fail_site: self.work.get().fail_site,
            ..WorkObservation::default()
        });
        if edits.is_empty() {
            return Ok(());
        }
        let mut plan =
            EditPlan::new(self, image).map_err(|cause| FailedEdit { at: Some(0), cause })?;
        self.reserve_scratch(&mut plan.image.previous, edits.len(), "results")
            .map_err(|cause| FailedEdit { at: None, cause })?;
        for (at, &edit) in edits.iter().enumerate() {
            let result = plan.apply_edit(edit, at);
            #[cfg(test)]
            self.observe_image(plan.image);
            let previous = result.map_err(|cause| FailedEdit {
                at: Some(at),
                cause,
            })?;
            plan.image.previous.push_reserved(previous);
            #[cfg(test)]
            self.observe_work(|w| w.completed = at + 1);
        }
        self.admit_image(plan.image)?;
        #[cfg(test)]
        self.observe_image(plan.image);
        Ok(())
    }

    fn reserve_scratch<T: Copy + Default, const N: usize>(
        &self,
        scratch: &mut Scratch<T, N>,
        additional: usize,
        #[cfg(test)] site: &'static str,
        // The label is instrumentation only, not a production editing mode.
        #[cfg(not(test))] _: &'static str,
    ) -> Result<(), EditFailure> {
        #[cfg(test)]
        if scratch
            .as_slice()
            .len()
            .checked_add(additional)
            .is_none_or(|len| len > scratch.capacity())
        {
            self.observe_work(|w| w.reserves += 1);
            if self.work.get().fail_site == Some(site) {
                return Err(EditFailure::Scratch(
                    Vec::<u8>::new().try_reserve(usize::MAX).unwrap_err(),
                ));
            }
        }
        scratch.reserve(additional).map_err(EditFailure::Scratch)
    }

    fn admit_image(&self, image: &EditImage) -> Result<(), FailedEdit> {
        if image.required_end <= image.backed_end {
            return Ok(());
        }
        let current_size = image.backed_end / WASM_PAGE_SIZE;
        let delta = image.required_end.div_ceil(WASM_PAGE_SIZE) - current_size;
        #[cfg(test)]
        self.observe_work(|w| w.grows += 1);
        if self.memory.grow(delta) == -1 {
            return Err(FailedEdit {
                at: Some(image.required_at),
                cause: EditFailure::Grow(GrowFailed {
                    current_size,
                    delta,
                }),
            });
        }
        #[cfg(test)]
        self.observe_work(|w| w.grown_pages += delta);
        Ok(())
    }

    fn publish_image(&self, image: &EditImage) {
        // All bounds, bytes and backing were prepared under the owning call/borrow.
        // No encoding, discovery, allocation, growth or recoverable error remains.
        for range in image.ranges.as_slice() {
            self.memory.write(
                range.address,
                &image.bytes.as_slice()[range.offset..range.offset + range.len],
            );
        }
    }

    #[cfg(test)]
    fn observe_work(&self, f: impl FnOnce(&mut WorkObservation)) {
        let mut work = self.work.get();
        f(&mut work);
        self.work.set(work);
    }

    #[cfg(test)]
    fn observe_image(&self, image: &EditImage) {
        self.observe_work(|w| {
            w.dirty_bytes = image.bytes.as_slice().len();
            w.ranges = image.ranges.as_slice().len();
            w.byte_capacity = image.bytes.dynamic_capacity();
            w.range_capacity = image.ranges.dynamic_capacity();
            w.result_capacity = image.previous.dynamic_capacity();
        });
    }

    // Retain the existing direct split-admission test at its owning seam, not a
    // second editor. Its deliberately full directory must reject before lookup.
    #[cfg(test)]
    fn split_page_and_insert(
        &self,
        page: u64,
        key: u64,
        value: u64,
        pos: u16,
    ) -> Result<Option<u64>, Error> {
        let mut image = EditImage::new();
        let mut plan = EditPlan::new(self, &mut image)
            .map_err(|cause| FailedEdit { at: Some(0), cause }.scalar())?;
        plan.split_page(page, (key, value), 0, pos)
            .map_err(|cause| FailedEdit { at: Some(0), cause }.scalar())?;
        self.admit_image(plan.image).map_err(FailedEdit::scalar)?;
        self.publish_image(plan.image);
        Ok(None)
    }

    /// Grows the backing memory so the map can absorb `additional_entries` more
    /// key/value pairs without allocating additional pages during the commit phase.
    ///
    /// This is a preflight primitive: after a successful reservation, a sequence
    /// of `insert` calls that adds at most `additional_entries` total entries
    /// will not fail with [`Error::OutOfMemory`] from page allocation, provided
    /// no other concurrent mutation interferes. Zero or already-backed reservations do not write.
    /// This reserves backing only, not directory admission, corruption checks or heap allocation.
    pub fn reserve_for_inserts(&self, additional_entries: u64) -> Result<(), GrowFailed> {
        if additional_entries == 0 {
            return Ok(());
        }
        let h = self.header();
        // Worst case: every new entry causes a page split, allocating one new
        // page. Physical reservation does not publish page allocation metadata.
        let target_pages = h.page_count.saturating_add(additional_entries);
        let need = PAGES_START.saturating_add(target_pages.saturating_mul(PAGE_STRIDE));
        if need.div_ceil(WASM_PAGE_SIZE) <= self.memory.size() {
            return Ok(());
        }
        safe_write(&self.memory, need - 1, &[0])
    }

    /// Removes `key` and returns the former value, or `Ok(None)` when the key was not present.
    ///
    /// # Errors
    ///
    /// Same categories as [`insert`](Self::insert) for split/corruption cases; removal itself also updates the total length.
    pub fn remove(&self, key: u64) -> Result<Option<u64>, Error> {
        self.scalar_edit(Edit::Remove { key })
    }

    /// Greatest key **strictly less** than `key`, with its value: `pred` such that `pred.0 < key`.
    ///
    /// Returns `None` when every stored key is `>= key` (including the empty map).
    pub fn predecessor(&self, key: u64) -> Option<(u64, u64)> {
        let page = self.find_page_for_key(key)?;
        match self.page_lower_bound(page, key) {
            0 => {
                let prev = self.read_page_header(page).prev;
                if prev == 0 {
                    None
                } else {
                    let len = self.read_page_header(prev).len;
                    Some(self.read_entry(prev, len - 1))
                }
            }
            pos => Some(self.read_entry(page, pos - 1)),
        }
    }

    /// Greatest key **less than or equal to** `key`, with its value: `pred` such that
    /// `pred.0 <= key`.
    ///
    /// Unlike [`Self::predecessor`], an exact match is reported, so callers that must
    /// distinguish "already present" from "previous entry" (duplicate-or-predecessor
    /// checks on insert) need one directory walk instead of `get` plus `predecessor`.
    /// Returns `None` when every stored key is `> key` (including the empty map).
    pub fn predecessor_or_equal(&self, key: u64) -> Option<(u64, u64)> {
        let page = self.find_page_for_key(key)?;
        let pos = self.page_lower_bound(page, key);
        let len = self.read_page_header(page).len;
        if pos < len {
            let entry = self.read_entry(page, pos);
            if entry.0 == key {
                return Some(entry);
            }
        }
        // `pos` holds the first key greater than `key` (or is past the page end), so
        // the answer is the entry before it, falling back to the previous page.
        if pos == 0 {
            let prev = self.read_page_header(page).prev;
            if prev == 0 {
                return None;
            }
            let prev_len = self.read_page_header(prev).len;
            Some(self.read_entry(prev, prev_len - 1))
        } else {
            Some(self.read_entry(page, pos - 1))
        }
    }

    /// Smallest key **strictly greater** than `key`, with its value: `succ` such that `succ.0 > key`.
    ///
    /// When `key` is smaller than all keys, the search starts at the first page. Returns `None` when no larger key exists.
    pub fn successor(&self, key: u64) -> Option<(u64, u64)> {
        let page = match self.find_page_for_key(key) {
            Some(page) => page,
            None => self.read_first_page(),
        };
        if page == 0 {
            return None;
        }
        let header = self.read_page_header(page);
        let pos = self.page_upper_bound(page, key);
        if pos < header.len {
            Some(self.read_entry(page, pos))
        } else if header.next == 0 {
            None
        } else {
            Some(self.read_entry(header.next, 0))
        }
    }

    /// Smallest key in the map and its value (`None` when empty).
    pub fn first(&self) -> Option<(u64, u64)> {
        let page = self.read_first_page();
        if page == 0 {
            None
        } else {
            Some(self.read_entry(page, 0))
        }
    }

    /// Largest key in the map and its value (`None` when empty).
    pub fn last(&self) -> Option<(u64, u64)> {
        let page = self.read_last_page();
        if page == 0 {
            None
        } else {
            let len = self.read_page_header(page).len;
            Some(self.read_entry(page, len - 1))
        }
    }

    /// Forward cursor over all `(key, value)` pairs in ascending key order.
    ///
    /// Iteration walks the active page chain sequentially (`first_page` then each
    /// `next` link) and reads entries in-page, so the full traversal costs
    /// `O(len)` reads with no per-key directory search. This is the cheap path for
    /// whole-map scans, in contrast to repeated [`successor`](Self::successor)
    /// calls which re-search the directory on every step.
    pub fn iter(&self) -> Iter<'_, M> {
        Iter {
            map: self,
            page: self.read_first_page(),
            pos: 0,
        }
    }

    /// Walks directory + page list + free list and checks numeric invariants ([`DIR_CAP`] match, strictly increasing keys,
    /// sensible page links, freelist bookkeeping).
    ///
    /// Intended for assertions after upgrades or when debugging corrupted stable memory copies.
    pub fn validate(&self) -> Result<(), Error> {
        if self.read_dir_cap() != DIR_CAP {
            return Err(Error::Corrupt);
        }
        let dir_len = self.read_dir_len();
        let mut prev_page = 0;
        let mut expected_count = 0u64;
        let mut last_key = None;
        let mut page = self.read_first_page();
        for i in 0..dir_len {
            if page == 0 {
                return Err(Error::Corrupt);
            }
            let dir = self.read_dir_entry(i);
            if dir.page_id != page {
                return Err(Error::Corrupt);
            }
            let header = self.read_page_header(page);
            if header.flags != PAGE_FLAG_ACTIVE || header.prev != prev_page || header.len == 0 {
                return Err(Error::Corrupt);
            }
            if header.len as u64 > PAGE_CAP {
                return Err(Error::Corrupt);
            }
            let first = self.read_entry(page, 0).0;
            if dir.min_key != first {
                return Err(Error::Corrupt);
            }
            for j in 0..header.len {
                let key = self.read_entry(page, j).0;
                if let Some(prev) = last_key
                    && key <= prev
                {
                    return Err(Error::Corrupt);
                }
                last_key = Some(key);
                expected_count += 1;
            }
            prev_page = page;
            page = header.next;
        }
        if page != 0 || prev_page != self.read_last_page() || expected_count != self.len() {
            return Err(Error::Corrupt);
        }

        let mut free = self.read_free_head();
        let mut seen = std::collections::BTreeSet::new();
        while free != 0 {
            if free > self.read_page_count() || !seen.insert(free) {
                return Err(Error::Corrupt);
            }
            let header = self.read_page_header(free);
            free = header.free_next()?;
        }
        Ok(())
    }

    fn find_page_for_key(&self, key: u64) -> Option<u64> {
        locate_directory(self.read_dir_len(), key, |i| self.read_dir_entry(i))
            .map(|(_, entry)| entry.page_id)
    }

    fn page_search(&self, page: u64, key: u64) -> Result<u16, u16> {
        search_page(self.read_page_header(page).len, key, |i| {
            self.read_entry(page, i).0
        })
    }

    fn page_lower_bound(&self, page: u64, key: u64) -> u16 {
        match self.page_search(page, key) {
            Ok(pos) | Err(pos) => pos,
        }
    }

    fn page_upper_bound(&self, page: u64, key: u64) -> u16 {
        let len = self.read_page_header(page).len;
        let mut lo = 0u16;
        let mut hi = len;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.read_entry(page, mid).0 <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn read_entry(&self, page: u64, index: u16) -> (u64, u64) {
        let off = page_entry_offset(page, index);
        (read_u64(&self.memory, off), read_u64(&self.memory, off + 8))
    }

    #[cfg(test)]
    fn write_entry(&self, page: u64, index: u16, key: u64, value: u64) {
        let off = page_entry_offset(page, index);
        write_u64(&self.memory, off, key);
        write_u64(&self.memory, off + 8, value);
    }

    fn read_dir_entry(&self, index: u64) -> DirEntry {
        let off = dir_entry_offset(index);
        let mut bytes = [0u8; DIR_ENTRY_SIZE as usize];
        self.memory.read(off, &mut bytes);
        DirEntry::decode(&bytes)
    }

    #[cfg(test)]
    fn write_dir_entry(&self, index: u64, entry: DirEntry) {
        let off = dir_entry_offset(index);
        write_u64(&self.memory, off, entry.min_key);
        write_u64(&self.memory, off + 8, entry.page_id);
    }

    fn read_page_header(&self, page: u64) -> PageHeader {
        let mut bytes = [0; PAGE_HEADER_SIZE as usize];
        self.memory.read(page_offset(page), &mut bytes);
        PageHeader::decode(&bytes)
    }

    #[cfg(test)]
    fn write_page_header(&self, page: u64, header: PageHeader) {
        write(&self.memory, page_offset(page), &header.encode());
    }

    fn read_dir_len(&self) -> u64 {
        read_u64(&self.memory, OFFSET_DIR_LEN)
    }

    #[cfg(test)]
    fn write_dir_len(&self, len: u64) {
        write_u64(&self.memory, OFFSET_DIR_LEN, len);
    }

    fn read_dir_cap(&self) -> u64 {
        read_u64(&self.memory, OFFSET_DIR_CAP)
    }

    fn read_page_count(&self) -> u64 {
        read_u64(&self.memory, OFFSET_PAGE_COUNT)
    }

    fn read_free_head(&self) -> u64 {
        read_u64(&self.memory, OFFSET_FREE_HEAD)
    }

    fn read_first_page(&self) -> u64 {
        read_u64(&self.memory, OFFSET_FIRST_PAGE)
    }

    fn read_last_page(&self) -> u64 {
        read_u64(&self.memory, OFFSET_LAST_PAGE)
    }
}

/// Ascending `(key, value)` cursor returned by [`StablePagedOrderedMap::iter`].
///
/// Holds the current page id and in-page position and follows page `next` links,
/// so a complete scan reads each active page once without any directory search.
pub struct Iter<'a, M: Memory> {
    map: &'a StablePagedOrderedMap<M>,
    page: u64,
    pos: u16,
}

impl<M: Memory> Iterator for Iter<'_, M> {
    type Item = (u64, u64);

    fn next(&mut self) -> Option<Self::Item> {
        while self.page != 0 {
            let header = self.map.read_page_header(self.page);
            if self.pos < header.len {
                let entry = self.map.read_entry(self.page, self.pos);
                self.pos += 1;
                return Some(entry);
            }
            self.page = header.next;
            self.pos = 0;
        }
        None
    }
}

impl<'a, M: Memory> IntoIterator for &'a StablePagedOrderedMap<M> {
    type Item = (u64, u64);
    type IntoIter = Iter<'a, M>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

fn write_header<M: Memory>(memory: &M, h: &HeaderV1) -> Result<(), GrowFailed> {
    safe_write(memory, OFFSET_MAGIC, &h.magic)?;
    safe_write(memory, OFFSET_VERSION, &[h.version])?;
    safe_write(memory, 4, &[0; 4])?;
    write_u64(memory, OFFSET_LEN, h.len);
    write_u64(memory, OFFSET_PAGE_COUNT, h.page_count);
    write_u64(memory, OFFSET_FREE_HEAD, h.free_head);
    write_u64(memory, OFFSET_FIRST_PAGE, h.first_page);
    write_u64(memory, OFFSET_LAST_PAGE, h.last_page);
    write_u64(memory, OFFSET_DIR_LEN, h.dir_len);
    write_u64(memory, OFFSET_DIR_CAP, h.dir_cap);
    Ok(())
}

fn read_header<M: Memory>(memory: &M) -> HeaderV1 {
    let mut magic = [0u8; 3];
    memory.read(OFFSET_MAGIC, &mut magic);
    let version = read_u8(memory, OFFSET_VERSION);
    HeaderV1 {
        magic,
        version,
        len: read_u64(memory, OFFSET_LEN),
        page_count: read_u64(memory, OFFSET_PAGE_COUNT),
        free_head: read_u64(memory, OFFSET_FREE_HEAD),
        first_page: read_u64(memory, OFFSET_FIRST_PAGE),
        last_page: read_u64(memory, OFFSET_LAST_PAGE),
        dir_len: read_u64(memory, OFFSET_DIR_LEN),
        dir_cap: read_u64(memory, OFFSET_DIR_CAP),
    }
}

fn validate_header<M: Memory>(memory: &M, h: &HeaderV1) -> Result<(), InitError> {
    if h.magic != MAGIC {
        return Err(InitError::BadMagic { actual: h.magic });
    }
    if h.version != LAYOUT_VERSION {
        return Err(InitError::IncompatibleVersion(h.version));
    }
    if h.dir_cap != DIR_CAP {
        return Err(InitError::DirectoryCapacityMismatch {
            expected: DIR_CAP,
            actual: h.dir_cap,
        });
    }
    if h.dir_len > h.dir_cap
        || h.dir_len > h.page_count
        || h.len > h.page_count.saturating_mul(PAGE_CAP)
    {
        return Err(InitError::InvalidLayout);
    }
    if h.page_count == 0 {
        if h.len != 0 || h.dir_len != 0 || h.first_page != 0 || h.last_page != 0 {
            return Err(InitError::InvalidLayout);
        }
    } else {
        if h.first_page == 0
            || h.last_page == 0
            || h.first_page > h.page_count
            || h.last_page > h.page_count
        {
            return Err(InitError::InvalidLayout);
        }
        let bytes = memory.size().saturating_mul(WASM_PAGE_SIZE);
        let need = PAGES_START.saturating_add(h.page_count.saturating_mul(PAGE_STRIDE));
        if bytes < need {
            return Err(InitError::InvalidLayout);
        }
    }
    if h.free_head > h.page_count {
        return Err(InitError::InvalidLayout);
    }
    Ok(())
}

#[inline]
fn dir_entry_offset(index: u64) -> u64 {
    DIR_OFFSET + index * DIR_ENTRY_SIZE
}

#[inline]
fn page_offset(page_id: u64) -> u64 {
    debug_assert!(page_id != 0);
    PAGES_START + (page_id - 1) * PAGE_STRIDE
}

#[inline]
fn page_entry_offset(page_id: u64, index: u16) -> u64 {
    page_offset(page_id) + PAGE_OFFSET_ENTRIES + u64::from(index) * PAGE_ENTRY_SIZE
}

fn read_u8<M: Memory>(memory: &M, offset: u64) -> u8 {
    let mut b = [0u8; 1];
    memory.read(offset, &mut b);
    b[0]
}

fn read_u64<M: Memory>(memory: &M, offset: u64) -> u64 {
    let mut b = [0u8; 8];
    memory.read(offset, &mut b);
    u64::from_le_bytes(b)
}

fn write_u64<M: Memory>(memory: &M, offset: u64, value: u64) {
    write(memory, offset, &value.to_le_bytes());
}

fn safe_write<M: Memory>(memory: &M, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
    let last_byte = offset
        .checked_add(bytes.len() as u64)
        .expect("address overflow");
    let size_pages = memory.size();
    let size_bytes = size_pages
        .checked_mul(WASM_PAGE_SIZE)
        .expect("address overflow");
    if size_bytes < last_byte {
        let diff_pages = (last_byte - size_bytes)
            .checked_add(WASM_PAGE_SIZE - 1)
            .expect("address overflow")
            / WASM_PAGE_SIZE;
        if memory.grow(diff_pages) == -1 {
            return Err(GrowFailed {
                current_size: size_pages,
                delta: diff_pages,
            });
        }
    }
    memory.write(offset, bytes);
    Ok(())
}

fn write<M: Memory>(memory: &M, offset: u64, bytes: &[u8]) {
    safe_write(memory, offset, bytes).expect("failed to grow stable memory");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::DefaultMemoryImpl;

    fn map() -> StablePagedOrderedMap<DefaultMemoryImpl> {
        StablePagedOrderedMap::init(DefaultMemoryImpl::default()).unwrap()
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    struct Traffic {
        reads: usize,
        read_bytes: usize,
        writes: usize,
        write_bytes: usize,
        max_read: usize,
        max_write: usize,
        sizes: usize,
        grows: usize,
        grown_pages: u64,
    }

    #[derive(Clone, Default)]
    struct WorkMemory {
        inner: DefaultMemoryImpl,
        traffic: std::rc::Rc<std::cell::Cell<Traffic>>,
    }

    impl WorkMemory {
        fn observe(&self, f: impl FnOnce(&mut Traffic)) {
            let mut traffic = self.traffic.get();
            f(&mut traffic);
            self.traffic.set(traffic);
        }
    }

    impl Memory for WorkMemory {
        fn size(&self) -> u64 {
            self.observe(|t| t.sizes += 1);
            self.inner.size()
        }

        fn grow(&self, pages: u64) -> i64 {
            let result = self.inner.grow(pages);
            self.observe(|t| {
                t.grows += 1;
                if result >= 0 {
                    t.grown_pages += pages;
                }
            });
            result
        }

        fn read(&self, offset: u64, dst: &mut [u8]) {
            self.observe(|t| {
                t.reads += 1;
                t.read_bytes += dst.len();
                t.max_read = t.max_read.max(dst.len());
            });
            self.inner.read(offset, dst);
        }

        unsafe fn read_unsafe(&self, offset: u64, dst: *mut u8, count: usize) {
            self.observe(|t| {
                t.reads += 1;
                t.read_bytes += count;
                t.max_read = t.max_read.max(count);
            });
            // SAFETY: forward the caller's valid, nonoverlapping destination unchanged.
            unsafe { self.inner.read_unsafe(offset, dst, count) };
        }

        fn write(&self, offset: u64, src: &[u8]) {
            self.observe(|t| {
                t.writes += 1;
                t.write_bytes += src.len();
                t.max_write = t.max_write.max(src.len());
            });
            self.inner.write(offset, src);
        }
    }

    fn backing(memory: &impl Memory) -> Vec<u8> {
        let mut bytes = vec![0; (memory.size() * WASM_PAGE_SIZE) as usize];
        memory.read(0, &mut bytes);
        bytes
    }

    #[test]
    fn page_header_codec() {
        let active = PageHeader {
            flags: PAGE_FLAG_ACTIVE,
            len: 0x1234,
            prev: 0x0807_0605_0403_0201,
            next: 0x1817_1615_1413_1211,
        };
        let active_bytes = [
            1, 0, 0x34, 0x12, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 0x11, 0x12, 0x13, 0x14, 0x15,
            0x16, 0x17, 0x18,
        ];
        assert_eq!(active.encode(), active_bytes);
        assert_eq!(PageHeader::decode(&active_bytes), active);
        assert_eq!(active.free_next(), Err(Error::Corrupt));
        let free = PageHeader {
            flags: PAGE_FLAG_FREE,
            len: 0,
            prev: 7,
            next: 0,
        };
        let free_bytes = [
            0, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(free.encode(), free_bytes);
        assert_eq!(PageHeader::decode(&free_bytes), free);
        assert_eq!(free.free_next(), Ok(7));
        assert_eq!((PAGE_HEADER_SIZE, PAGE_STRIDE), (24, 1048));
    }

    #[test]
    fn page_reopen_and_recycle() {
        let m = StablePagedOrderedMap::init(WorkMemory::default()).unwrap();
        assert_eq!(m.header().version, 1);
        for key in 0..97 {
            m.insert(key, key ^ 0xa500).unwrap();
        }
        for key in 0..64 {
            assert_eq!(m.remove(key), Ok(Some(key ^ 0xa500)));
        }
        assert_eq!(
            (
                m.header().free_head,
                m.header().page_count,
                m.header().first_page
            ),
            (2, 3, 3)
        );
        assert_eq!(m.read_page_header(2).free_next(), Ok(1));
        assert_eq!(m.read_page_header(1).free_next(), Ok(0));
        m.validate().unwrap();
        for key in 97..128 {
            m.insert(key, key ^ 0xa500).unwrap();
        }
        let healthy = backing(&m.memory.inner);
        for (field, bytes) in [
            (PAGE_OFFSET_FLAGS, vec![7]),
            (PAGE_OFFSET_LEN, 1u16.to_le_bytes().to_vec()),
            (PAGE_OFFSET_NEXT, 1u64.to_le_bytes().to_vec()),
            (PAGE_OFFSET_PREV, 2u64.to_le_bytes().to_vec()),
            (PAGE_OFFSET_PREV, 4u64.to_le_bytes().to_vec()),
        ] {
            m.memory.write(page_offset(2) + field, &bytes);
            let poisoned = backing(&m.memory.inner);
            assert_eq!(m.validate(), Err(Error::Corrupt));
            m.memory.traffic.set(Traffic::default());
            assert_eq!(m.insert(128, 128 ^ 0xa500), Err(Error::Corrupt));
            assert_eq!(m.memory.traffic.get().writes, 0);
            assert_eq!(backing(&m.memory.inner), poisoned);
            m.memory.write(0, &healthy);
        }
        // A two-page free-list cycle is detected independently of a self-loop.
        m.memory
            .write(page_offset(1) + PAGE_OFFSET_PREV, &2u64.to_le_bytes());
        let poisoned = backing(&m.memory.inner);
        assert_eq!(m.validate(), Err(Error::Corrupt));
        assert_eq!(backing(&m.memory.inner), poisoned);
        m.memory.write(0, &healthy);
        assert_eq!(m.insert(128, 128 ^ 0xa500), Ok(None));
        assert_eq!(
            (
                m.header().free_head,
                m.header().page_count,
                m.header().last_page
            ),
            (1, 3, 2)
        );
        m.validate().unwrap();
        let expected = (64..=128)
            .map(|key| (key, key ^ 0xa500))
            .collect::<Vec<_>>();
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        let memory = m.into_memory();
        let before = backing(&memory.inner);
        let reopened = StablePagedOrderedMap::init(memory.clone()).unwrap();
        assert_eq!(reopened.iter().collect::<Vec<_>>(), expected);
        reopened.validate().unwrap();
        assert_eq!(backing(&memory.inner), before);
    }

    #[test]
    fn canonical_dir_entry_single_read_contract() {
        let m = StablePagedOrderedMap::init(WorkMemory::default()).unwrap();
        let key = 0xa1b2_c3d4_e5f6_0718u64;
        let value = key ^ 0x5a5a_0f0f_3c3c_1e1e;
        assert_eq!(m.insert(key, value), Ok(None));
        let before = backing(&m.memory.inner);
        m.memory.traffic.set(Traffic::default());
        let entry = m.read_dir_entry(0);
        let traffic = m.memory.traffic.get();
        assert_eq!(
            entry,
            DirEntry {
                min_key: key,
                page_id: 1,
            }
        );
        assert_eq!(backing(&m.memory.inner), before);
        assert_eq!(m.get(key), Some(value));
        assert_eq!(
            traffic,
            Traffic {
                reads: 1,
                read_bytes: 16,
                max_read: 16,
                ..Traffic::default()
            }
        );
    }

    #[test]
    fn scalar_edit_work_contract() {
        let m = StablePagedOrderedMap::init(WorkMemory::default()).unwrap();
        seed_scalar_work(&m);
        let value = |key| 0xa500_0000_0000_0000 | key;
        let mut expected: Vec<_> = (0..=126)
            .step_by(2)
            .chain((128..=252).step_by(4))
            .chain([1000])
            .map(|key| (key, value(key)))
            .collect();
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!(m.len(), 97);
        assert_eq!((m.header().dir_len, m.header().page_count), (2, 2));
        assert_eq!(
            (m.read_page_header(1).len, m.read_page_header(2).len),
            (64, 33)
        );
        assert_eq!(
            (m.read_dir_entry(0).min_key, m.read_dir_entry(1).min_key),
            (0, 128)
        );
        m.validate().unwrap();
        let changed = value(64) ^ (1 << 56);
        let cases = [
            ("replace", 7, 96, 2, 16, 1, 8),
            ("missing", 9, 104, 0, 0, 0, 0),
            ("tail_insert", 10, 112, 8, 51, 3, 26),
            ("tail_remove", 11, 120, 6, 35, 2, 10),
        ];
        for (name, reads, read_bytes, writes, write_bytes, new_writes, new_bytes) in cases {
            let before = backing(&m.memory.inner);
            let header = m.header();
            m.memory.traffic.set(Traffic::default());
            m.work.set(WorkObservation::default());
            let result = match name {
                "replace" => m.insert(64, changed),
                "missing" => m.remove(129),
                "tail_insert" => m.insert(2000, value(2000)),
                "tail_remove" => m.remove(2000),
                _ => unreachable!(),
            };
            let traffic = m.memory.traffic.get();
            let work = m.work.get();
            eprintln!("SCALAR_WORK {name} traffic={traffic:?} scratch={work:?}");
            assert_eq!((traffic.reads, traffic.read_bytes), (reads, read_bytes));
            assert!(traffic.writes <= writes && traffic.write_bytes <= write_bytes);
            assert!(traffic.max_read <= 32);
            assert_eq!((traffic.grows, traffic.grown_pages), (0, 0));
            if work.observed {
                assert_eq!(
                    (traffic.writes, traffic.write_bytes),
                    (new_writes, new_bytes)
                );
                assert_eq!(work.dirty_bytes, new_bytes);
                assert_eq!(work.ranges, new_writes);
                assert_eq!((work.byte_capacity, work.range_capacity), (0, 0));
                assert_eq!((work.result_capacity, work.reserves), (0, 0));
                assert_eq!((work.backing_lookups, work.prospective_lookups), (1, 0));
            } else {
                assert_eq!((traffic.reads, traffic.read_bytes), (reads, read_bytes));
                assert_eq!((traffic.writes, traffic.write_bytes), (writes, write_bytes));
            }
            let mut after_expected = before;
            match name {
                "replace" => {
                    assert_eq!(result, Ok(Some(value(64))));
                    let offset = (page_entry_offset(1, 32) + 8) as usize;
                    after_expected[offset..offset + 8].copy_from_slice(&changed.to_le_bytes());
                    expected[32].1 = changed;
                }
                "missing" => assert_eq!(result, Ok(None)),
                "tail_insert" => {
                    assert_eq!(result, Ok(None));
                    let offset = page_entry_offset(2, 33) as usize;
                    after_expected[offset..offset + 8].copy_from_slice(&2000u64.to_le_bytes());
                    after_expected[offset + 8..offset + 16]
                        .copy_from_slice(&value(2000).to_le_bytes());
                    expected.push((2000, value(2000)));
                }
                "tail_remove" => {
                    assert_eq!(result, Ok(Some(value(2000))));
                    assert_eq!(expected.pop(), Some((2000, value(2000))));
                }
                _ => unreachable!(),
            }
            if name.starts_with("tail_") {
                let len = expected.len() as u64;
                after_expected[OFFSET_LEN as usize..OFFSET_LEN as usize + 8]
                    .copy_from_slice(&len.to_le_bytes());
                let offset = (page_offset(2) + PAGE_OFFSET_LEN) as usize;
                after_expected[offset..offset + 2]
                    .copy_from_slice(&((len - 64) as u16).to_le_bytes());
                assert_eq!(m.header(), HeaderV1 { len, ..header });
            } else {
                assert_eq!(m.header(), header);
            }
            assert_eq!(backing(&m.memory.inner), after_expected);
            assert_eq!(m.iter().collect::<Vec<_>>(), expected);
            m.validate().unwrap();
            if name == "replace" {
                assert_eq!(m.insert(64, value(64)), Ok(Some(changed)));
                expected[32].1 = value(64);
            }
        }
        m.memory.traffic.set(Traffic::default());
        m.work.set(WorkObservation::default());
        let result = m.insert(1, value(1));
        let traffic = m.memory.traffic.get();
        let work = m.work.get();
        eprintln!("SCALAR_WORK split traffic={traffic:?} scratch={work:?}");
        assert_eq!(result, Ok(None));
        assert_eq!((traffic.grows, traffic.grown_pages), (0, 0));
        expected.insert(1, (1, value(1)));
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!(
            (m.header().len, m.header().dir_len, m.header().page_count),
            (98, 3, 3)
        );
        assert_eq!(
            (m.read_page_header(1).len, m.read_page_header(3).len),
            (32, 33)
        );
        assert_eq!(
            (m.read_page_header(1).next, m.read_page_header(3).next),
            (3, 2)
        );
        assert_eq!(m.read_page_header(2).prev, 3);
        assert_eq!(
            m.read_dir_entry(1),
            DirEntry {
                min_key: 62,
                page_id: 3
            }
        );
        assert_eq!(
            m.read_dir_entry(2),
            DirEntry {
                min_key: 128,
                page_id: 2
            }
        );
        m.validate().unwrap();
    }

    fn seed_full_directory(
        m: &StablePagedOrderedMap<DefaultMemoryImpl>,
        value: impl Fn(u64) -> u64,
    ) -> HeaderV1 {
        for page in 1..=DIR_CAP {
            let len = if page == 1 { PAGE_CAP as u16 } else { 1 };
            m.write_page_header(
                page,
                PageHeader {
                    flags: PAGE_FLAG_ACTIVE,
                    len,
                    prev: page - 1,
                    next: if page == DIR_CAP { 0 } else { page + 1 },
                },
            );
            m.write_dir_entry(
                page - 1,
                DirEntry {
                    min_key: 128 * page,
                    page_id: page,
                },
            );
            for index in 0..len {
                let key = 128 * page + u64::from(index);
                m.write_entry(page, index, key, value(key));
            }
        }
        let header = HeaderV1 {
            len: DIR_CAP - 1 + PAGE_CAP,
            page_count: DIR_CAP,
            free_head: 0,
            first_page: 1,
            last_page: DIR_CAP,
            dir_len: DIR_CAP,
            ..m.header()
        };
        write_header(&m.memory, &header).unwrap();
        header
    }

    #[test]
    fn split_windows_preserve_prospective_entries() {
        let value = |key| 0xa500_0000_0000_0000 | key;
        for (pos, staged) in [0u16, 1, 31, 32, 33, 63, 64]
            .into_iter()
            .flat_map(|pos| [false, true].map(move |staged| (pos, staged)))
        {
            let mut m = map();
            let mut expected: Vec<_> = (1..=PAGE_CAP)
                .map(|index| (index * 2, value(index * 2)))
                .collect();
            for &(key, value) in &expected {
                assert_eq!(m.insert(key, value), Ok(None));
            }
            assert_eq!((m.header().page_count, m.header().dir_len), (1, 1));
            assert_eq!(m.read_page_header(1).len, 64);
            let memory = m.memory.clone();
            let before = backing(&memory);
            let size = memory.size();
            let mut edits = Vec::new();
            let mut previous = Vec::new();
            if staged {
                for key in [2, 62, 64, 66, 68, 126, 128] {
                    let changed = value(key) ^ (1 << 56);
                    edits.push(Edit::Insert {
                        key,
                        value: changed,
                    });
                    previous.push(Some(value(key)));
                    expected[(key / 2 - 1) as usize].1 = changed;
                }
            }
            let key = u64::from(pos) * 2 + 1;
            let inserted = value(key) ^ (2 << 56);
            edits.push(Edit::Insert {
                key,
                value: inserted,
            });
            previous.push(None);
            expected.insert(usize::from(pos), (key, inserted));

            let ready = m.prepare_edits(&edits).unwrap();
            assert_eq!(ready.previous_values(), previous.as_slice());
            assert!(backing(&memory) == before, "prepare changed backing");
            assert_eq!(memory.size(), size);
            ready.discard();
            assert!(backing(&memory) == before, "discard changed backing");
            assert_eq!(memory.size(), size);
            let ready = m.prepare_edits(&edits).unwrap();
            assert_eq!(ready.previous_values(), previous.as_slice());
            assert!(backing(&memory) == before, "re-prepare changed backing");
            ready.publish();

            assert_eq!(
                m.iter().collect::<Vec<_>>(),
                expected,
                "position {pos}, staged={staged}"
            );
            let header = m.header();
            assert_eq!(
                (
                    header.len,
                    header.page_count,
                    header.dir_len,
                    header.first_page,
                    header.last_page,
                    header.free_head
                ),
                (65, 2, 2, 1, 2, 0)
            );
            assert_eq!(
                m.read_page_header(1),
                PageHeader {
                    flags: PAGE_FLAG_ACTIVE,
                    len: 32,
                    prev: 0,
                    next: 2,
                }
            );
            assert_eq!(
                m.read_page_header(2),
                PageHeader {
                    flags: PAGE_FLAG_ACTIVE,
                    len: 33,
                    prev: 1,
                    next: 0,
                }
            );
            assert_eq!(
                m.read_dir_entry(0),
                DirEntry {
                    min_key: expected[0].0,
                    page_id: 1,
                }
            );
            assert_eq!(
                m.read_dir_entry(1),
                DirEntry {
                    min_key: expected[32].0,
                    page_id: 2,
                }
            );
            m.validate().unwrap();
            let reopened = StablePagedOrderedMap::init(m.into_memory()).unwrap();
            assert_eq!(reopened.iter().collect::<Vec<_>>(), expected);
            reopened.validate().unwrap();
        }
    }

    #[test]
    fn non_split_windows_preserve_prospective_entries() {
        let mut m = map();
        let value = |key| 0xa500_0000_0000_0000 | key;
        let original: Vec<_> = (1..=63).map(|i| (i * 2, value(i * 2))).collect();
        // Seed independently of the shift implementation under test.
        m.write_page_header(
            1,
            PageHeader {
                flags: PAGE_FLAG_ACTIVE,
                len: 63,
                prev: 0,
                next: 0,
            },
        );
        m.write_dir_entry(
            0,
            DirEntry {
                min_key: 2,
                page_id: 1,
            },
        );
        for (i, &(key, value)) in original.iter().enumerate() {
            m.write_entry(1, i as u16, key, value);
        }
        m.write_entry(1, 63, u64::MAX, 0xcccc_cccc_cccc_cccc);
        let header = HeaderV1 {
            len: 63,
            page_count: 1,
            first_page: 1,
            last_page: 1,
            dir_len: 1,
            ..m.header()
        };
        write_header(&m.memory, &header).unwrap();
        m.validate().unwrap();
        let memory = m.memory.clone();
        let before = backing(&memory);
        let size = memory.size();
        for insert in [true, false] {
            let positions = if insert {
                [0u16, 1, 31, 62, 63]
            } else {
                [0u16, 1, 31, 61, 62]
            };
            for pos in positions {
                for staged in [false, true] {
                    eprintln!("NON_SPLIT_CASE insert={insert} pos={pos} staged={staged}");
                    // The preceding ready object has been consumed; reset only this fixture.
                    memory.write(0, &before);
                    let mut expected = original.clone();
                    let mut expected_bytes = before.clone();
                    let mut dirty = Vec::new();
                    let mut edits = Vec::new();
                    let mut previous = Vec::new();
                    if staged {
                        for (i, mask) in [(0usize, 1u64), (31, 2), (62, 3)] {
                            let (key, old) = expected[i];
                            let changed = old ^ (mask << 56);
                            edits.push(Edit::Insert {
                                key,
                                value: changed,
                            });
                            previous.push(Some(old));
                            expected[i].1 = changed;
                            let at = page_entry_offset(1, i as u16) + 8;
                            expected_bytes[at as usize..at as usize + 8]
                                .copy_from_slice(&changed.to_le_bytes());
                            dirty.extend(at..at + 8);
                        }
                    }
                    if insert {
                        let key = u64::from(pos) * 2 + 1;
                        let inserted = value(key) ^ (4 << 56);
                        edits.push(Edit::Insert {
                            key,
                            value: inserted,
                        });
                        previous.push(None);
                        expected.insert(usize::from(pos), (key, inserted));
                    } else {
                        let (key, old) = expected.remove(usize::from(pos));
                        edits.push(Edit::Remove { key });
                        previous.push(Some(old));
                    }
                    let len = expected.len();
                    let start = page_entry_offset(1, pos);
                    let end = page_entry_offset(1, len as u16);
                    dirty.extend(start..end);
                    for (i, &(key, value)) in expected.iter().enumerate() {
                        let at = page_entry_offset(1, i as u16) as usize;
                        expected_bytes[at..at + 8].copy_from_slice(&key.to_le_bytes());
                        expected_bytes[at + 8..at + 16].copy_from_slice(&value.to_le_bytes());
                    }
                    expected_bytes[OFFSET_LEN as usize..OFFSET_LEN as usize + 8]
                        .copy_from_slice(&(len as u64).to_le_bytes());
                    dirty.extend(OFFSET_LEN..OFFSET_LEN + 8);
                    let at = page_offset(1) + PAGE_OFFSET_LEN;
                    expected_bytes[at as usize..at as usize + 2]
                        .copy_from_slice(&(len as u16).to_le_bytes());
                    dirty.extend(at..at + 2);
                    if pos == 0 {
                        let at = dir_entry_offset(0);
                        expected_bytes[at as usize..at as usize + 8]
                            .copy_from_slice(&expected[0].0.to_le_bytes());
                        dirty.extend(at..at + 8);
                    }
                    dirty.sort_unstable();
                    dirty.dedup();
                    let ready = m.prepare_edits(&edits).unwrap();
                    assert_eq!(ready.previous_values(), previous);
                    let actual_dirty: Vec<_> = ready
                        .image
                        .ranges
                        .as_slice()
                        .iter()
                        .flat_map(|r| r.address..r.end())
                        .collect();
                    assert_eq!(actual_dirty, dirty, "dirty destination union");
                    assert!(
                        backing(&memory) == before,
                        "prepare changed canonical bytes"
                    );
                    assert_eq!(memory.size(), size);
                    ready.discard();
                    assert!(
                        backing(&memory) == before,
                        "discard changed canonical bytes"
                    );
                    let ready = m.prepare_edits(&edits).unwrap();
                    assert_eq!(ready.previous_values(), previous);
                    assert!(
                        backing(&memory) == before,
                        "re-prepare changed canonical bytes"
                    );
                    ready.publish();
                    assert_eq!(
                        m.iter().collect::<Vec<_>>(),
                        expected,
                        "exact shifted pairs"
                    );
                    assert_eq!(
                        m.header(),
                        HeaderV1 {
                            len: len as u64,
                            ..header
                        }
                    );
                    assert_eq!(m.read_page_header(1).len, len as u16);
                    assert_eq!(
                        m.read_dir_entry(0),
                        DirEntry {
                            min_key: expected[0].0,
                            page_id: 1
                        }
                    );
                    assert!(
                        backing(&memory) == expected_bytes,
                        "padding or unused tail changed"
                    );
                    assert_eq!(memory.size(), size);
                    m.validate().unwrap();
                }
            }
        }
    }

    #[test]
    fn prepared_edits_order_and_discard() {
        let mut m = map();
        let memory = m.memory.clone();
        let value = |key| 0xa500_0000_0000_0000 | key;
        for key in 20..=84 {
            m.insert(key, value(key)).unwrap();
        }
        let before = backing(&memory);
        let original: Vec<_> = (20..=84).map(|key| (key, value(key))).collect();
        let edits = [
            Edit::Insert {
                key: 41,
                value: value(41) ^ (1 << 56),
            },
            Edit::Insert {
                key: 41,
                value: value(41) ^ (2 << 56),
            },
            Edit::Remove { key: 41 },
            Edit::Remove { key: 41 },
            Edit::Insert {
                key: 41,
                value: value(41),
            },
            Edit::Remove { key: 999 },
            Edit::Insert {
                key: 5,
                value: value(5),
            },
        ];
        let previous = [
            Some(value(41)),
            Some(value(41) ^ (1 << 56)),
            Some(value(41) ^ (2 << 56)),
            None,
            None,
            None,
            None,
        ];
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), previous);
        assert!(backing(&memory) == before, "prepare wrote canonical bytes");
        ready.discard();
        assert!(backing(&memory) == before, "discard wrote canonical bytes");
        assert_eq!(m.iter().collect::<Vec<_>>(), original);
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), previous);
        ready.publish();
        let expected: Vec<_> = [5]
            .into_iter()
            .chain(20..=84)
            .map(|key| (key, value(key)))
            .collect();
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!(m.read_dir_entry(0).min_key, 5);
        m.validate().unwrap();

        // Free page1 while page2 stays live, then split page2 and reuse page1
        // within the same unpublished batch. Checking here prevents drain/reset
        // from masking an allocator that failed to recycle that page.
        let edits: Vec<_> = [5]
            .into_iter()
            .chain(20..=51)
            .map(|key| Edit::Remove { key })
            .chain((85..=116).map(|key| Edit::Insert {
                key,
                value: value(key),
            }))
            .collect();
        let previous: Vec<_> = [5]
            .into_iter()
            .chain(20..=51)
            .map(|key| Some(value(key)))
            .chain((85..=116).map(|_| None))
            .collect();
        let before = backing(&memory);
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), previous);
        assert!(backing(&memory) == before);
        ready.publish();
        let expected: Vec<_> = (52..=116).map(|key| (key, value(key))).collect();
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        let h = m.header();
        assert_eq!(
            (h.page_count, h.free_head, h.first_page, h.last_page),
            (2, 0, 2, 1)
        );
        assert_eq!(
            (m.read_dir_entry(0).min_key, m.read_dir_entry(1).min_key),
            (52, 84)
        );
        assert_eq!(
            (m.read_page_header(2).len, m.read_page_header(1).len),
            (32, 33)
        );
        m.validate().unwrap();

        let edits: Vec<_> = (52..=116)
            .map(|key| Edit::Remove { key })
            .chain([Edit::Insert {
                key: 100,
                value: value(100),
            }])
            .chain((0..70).rev().map(|key| Edit::Insert {
                key,
                value: value(key),
            }))
            .collect();
        let previous: Vec<_> = (52..=116)
            .map(|key| Some(value(key)))
            .chain((0..71).map(|_| None))
            .collect();
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), previous);
        ready.publish();
        let expected: Vec<_> = (0..70).chain([100]).map(|key| (key, value(key))).collect();
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!((m.header().page_count, m.header().dir_len), (2, 2));
        m.validate().unwrap();
        let before = backing(&memory);
        let ready = m.prepare_edits(&[]).unwrap();
        assert!(ready.previous_values().is_empty());
        ready.discard();
        assert!(backing(&memory) == before);
    }

    #[test]
    fn prepared_edits_directory_rejection() {
        let mut m = map();
        m.reserve_for_inserts(DIR_CAP).unwrap();
        let value = |key| 0xa500_0000_0000_0000 ^ key;
        let header = seed_full_directory(&m, value);
        let original: Vec<_> = (128..192)
            .chain((2..=DIR_CAP).map(|page| 128 * page))
            .map(|key| (key, value(key)))
            .collect();
        assert_eq!(original.len(), 8255);
        assert_eq!(m.iter().collect::<Vec<_>>(), original);
        m.validate().unwrap();
        let changed = value(128) ^ (1 << 56);
        let edits = [
            Edit::Insert {
                key: 128,
                value: changed,
            },
            Edit::Insert {
                key: 192,
                value: value(192),
            },
            Edit::Remove { key: 256 },
        ];
        let before = backing(&m.memory);
        for intents in [&edits[..2], &edits[..]] {
            match m.prepare_edits(intents) {
                Err(PrepareError::Edit { at, source }) => {
                    assert_eq!((at, source), (1, Error::DirectoryFull));
                }
                _ => panic!("expected DirectoryFull at edit1"),
            }
            assert!(
                backing(&m.memory) == before,
                "late DirectoryFull changed backing"
            );
            assert_eq!(m.get(128), Some(value(128)));
            assert_eq!(m.header(), header);
            assert_eq!(m.iter().collect::<Vec<_>>(), original);
        }
        let ready = m.prepare_edits(&[edits[2], edits[0], edits[1]]).unwrap();
        assert_eq!(
            ready.previous_values(),
            [Some(value(256)), Some(value(128)), None]
        );
        ready.publish();
        let expected: Vec<_> = (128..=192)
            .chain((3..=DIR_CAP).map(|page| 128 * page))
            .map(|key| (key, if key == 128 { changed } else { value(key) }))
            .collect();
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!(m.header(), header);
        assert_eq!(
            (m.read_page_header(1).len, m.read_page_header(2).len),
            (32, 33)
        );
        assert_eq!(m.read_page_header(1).next, 2);
        assert_eq!(
            (m.read_page_header(2).prev, m.read_page_header(2).next),
            (1, 3)
        );
        assert_eq!(m.read_page_header(3).prev, 2);
        assert_eq!(
            m.read_dir_entry(1),
            DirEntry {
                min_key: 160,
                page_id: 2
            }
        );
        m.validate().unwrap();
    }

    #[test]
    fn prepared_edits_resources_and_retry() {
        use ic_stable_structures::RestrictedMemory;
        eprintln!(
            "PREPARED_LAYOUT image={} range={} result={} inline_bytes={} inline_ranges={} inline_results={}",
            std::mem::size_of::<EditImage>(),
            std::mem::size_of::<DirtyRange>(),
            std::mem::size_of::<Option<u64>>(),
            std::mem::size_of::<Scratch<u8, 32>>(),
            std::mem::size_of::<Scratch<DirtyRange, 3>>(),
            std::mem::size_of::<Scratch<Option<u64>, 1>>(),
        );
        let memory = DefaultMemoryImpl::default();
        let mut m =
            StablePagedOrderedMap::init(RestrictedMemory::new(memory.clone(), 0..3)).unwrap();
        let value = |key| 0xa500_0000_0000_0000 | key;
        for key in 0..64 {
            m.insert(key, value(key)).unwrap();
        }
        assert_eq!(memory.size(), 3);
        assert_eq!((m.header().len, m.header().page_count), (64, 1));
        m.validate().unwrap();
        let before = backing(&memory);
        let edits: Vec<_> = (64..4064)
            .map(|key| Edit::Insert {
                key,
                value: value(key),
            })
            // Both shifts use page126's prospective-only payload before admission.
            .chain([
                Edit::Remove { key: 4001 },
                Edit::Insert {
                    key: 4001,
                    value: value(4001),
                },
            ])
            .collect();
        let mut expected_previous = vec![None; 4000];
        expected_previous.extend([Some(value(4001)), None]);
        match m.prepare_edits(&edits) {
            Err(PrepareError::Grow { at, source }) => {
                assert_eq!(at, 3968, "intent that mints the final required page126");
                assert_eq!(
                    source,
                    GrowFailed {
                        current_size: 3,
                        delta: 2
                    }
                );
            }
            _ => panic!("expected actual restricted grow3/+2 before ready"),
        }
        assert!(backing(&memory) == before);
        assert_eq!(m.work.get().grows, 1);
        assert_eq!(m.work.get().completed, 4002);
        assert_eq!(m.work.get().selected, Some((4001, 126, PAGE_FLAG_ACTIVE)));
        assert!(page_offset(126) + PAGE_STRIDE > memory.size() * WASM_PAGE_SIZE);
        eprintln!("PREPARED_RESOURCE rejected {:?}", m.work.get());
        // The first edit really splits a full page and needs overflowing byte scratch.
        m.observe_work(|w| w.fail_site = Some("bytes"));
        match m.prepare_edits(&edits) {
            Err(PrepareError::Scratch { at, source }) => {
                assert_eq!(at, Some(0));
                eprintln!("PREPARED_RESOURCE labelled bytes reservation: {source}");
            }
            _ => panic!("expected labelled split byte scratch error at edit0"),
        }
        assert!(backing(&memory) == before);
        assert_eq!(m.work.get().grows, 0);
        // Clear only the injected scratch fault and retry the identical batch.
        // It must now reach the independently unchanged backing restriction.
        m.observe_work(|w| w.fail_site = None);
        match m.prepare_edits(&edits) {
            Err(PrepareError::Grow { at, source }) => {
                assert_eq!(at, 3968);
                assert_eq!(
                    source,
                    GrowFailed {
                        current_size: 3,
                        delta: 2
                    }
                );
            }
            _ => panic!("scratch retry did not reach the real backing restriction"),
        }
        assert!(backing(&memory) == before);
        assert_eq!(m.work.get().grows, 1);
        drop(m);
        let mut m =
            StablePagedOrderedMap::init(RestrictedMemory::new(memory.clone(), 0..5)).unwrap();
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), expected_previous);
        assert_eq!(memory.size(), 5);
        let ready_work = ready.owner.work.get();
        assert_eq!((ready_work.grows, ready_work.grown_pages), (1, 2));
        let grown = backing(&memory);
        assert_eq!(&grown[..before.len()], before);
        ready.discard();
        assert!(backing(&memory) == grown);
        assert_eq!(m.work.get(), ready_work);
        assert_eq!((m.header().len, m.header().page_count), (64, 1));
        m.validate().unwrap();
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), expected_previous);
        let ready_work = ready.owner.work.get();
        assert_eq!(ready_work.grows, 0);
        eprintln!("PREPARED_RESOURCE ready {ready_work:?}");
        ready.publish();
        assert_eq!(m.work.get(), ready_work, "publication did resource work");
        assert_eq!(memory.size(), 5);
        assert_eq!((m.header().len, m.header().page_count), (4064, 126));
        let expected: Vec<_> = (0..4064).map(|key| (key, value(key))).collect();
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        m.validate().unwrap();
    }

    #[test]
    fn prepared_edits_corruption_and_scalar() {
        let mut m = map();
        seed_scalar_work(&m);
        let value = |key| 0xa500_0000_0000_0000 | key;
        let changed = value(64) ^ (1 << 56);
        let healthy = backing(&m.memory);
        assert_eq!(
            (m.read_page_header(1).len, m.read_page_header(2).len),
            (64, 33)
        );
        m.validate().unwrap();
        let poison = page_offset(2) + PAGE_OFFSET_FLAGS;
        m.memory.write(poison, &[PAGE_FLAG_FREE]);
        let poisoned = backing(&m.memory);
        let edits = [
            Edit::Insert {
                key: 64,
                value: changed,
            },
            Edit::Remove { key: 128 },
        ];
        match m.prepare_edits(&edits) {
            Err(PrepareError::Edit { at, source }) => assert_eq!((at, source), (1, Error::Corrupt)),
            _ => panic!("expected selected page2 ACTIVE rejection at edit1"),
        }
        let work = m.work.get();
        assert_eq!(work.completed, 1);
        assert_eq!(work.first_staged, Some((Some(value(64)), Some(changed))));
        assert_eq!(work.selected, Some((1, 2, PAGE_FLAG_FREE)));
        eprintln!("PREPARED_CORRUPTION reached {work:?}");
        assert!(backing(&m.memory) == poisoned);
        m.memory.write(poison, &[PAGE_FLAG_ACTIVE]);
        assert!(backing(&m.memory) == healthy);
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), [Some(value(64)), Some(value(128))]);
        ready.publish();
        let expected: Vec<_> = (0..=126)
            .step_by(2)
            .chain((132..=252).step_by(4))
            .chain([1000])
            .map(|key| (key, if key == 64 { changed } else { value(key) }))
            .collect();
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!((m.header().len, m.header().page_count), (96, 2));
        assert_eq!(m.read_dir_entry(1).min_key, 132);
        m.validate().unwrap();
        let batch = backing(&m.memory);
        // Reset this owned fixture only after consuming ready. This is not rollback.
        m.memory.write(0, &healthy);
        assert_eq!(m.insert(64, changed), Ok(Some(value(64))));
        assert_eq!(m.remove(128), Ok(Some(value(128))));
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert!(backing(&m.memory) == batch);
        m.validate().unwrap();
        let before = backing(&m.memory);
        let cause = checked_page_offset(u64::MAX).unwrap_err();
        assert!(matches!(
            FailedEdit { at: Some(7), cause }.prepared(),
            PrepareError::AddressOverflow { at: 7 }
        ));
        assert!(backing(&m.memory) == before);
    }

    #[test]
    fn checked_page_offset_boundary_contract() {
        assert!(matches!(
            checked_page_offset(0),
            Err(EditFailure::AddressOverflow)
        ));
        assert!(matches!(checked_page_offset(1), Ok(131136)));
        assert!(matches!(
            checked_page_offset(17601855032165478),
            Ok(18446744073709551032)
        ));
        assert!(matches!(
            checked_page_offset(17601855032165479),
            Err(EditFailure::AddressOverflow)
        ));
        assert!(matches!(
            checked_page_offset(u64::MAX),
            Err(EditFailure::AddressOverflow)
        ));
    }

    #[test]
    fn seeded_page_count_boundary_sequencing() {
        let mut m = StablePagedOrderedMap::init(WorkMemory::default()).unwrap();
        let value = |key| 0xa500_0000_0000_0000 | key;
        seed_scalar_work(&m);
        let expected: Vec<_> = (0..=126)
            .step_by(2)
            .chain((128..=252).step_by(4))
            .chain([1000])
            .map(|key| (key, value(key)))
            .collect();
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        let healthy_header = m.header();
        assert_eq!(
            (
                healthy_header.len,
                healthy_header.page_count,
                healthy_header.dir_len,
                healthy_header.free_head,
                healthy_header.first_page,
                healthy_header.last_page
            ),
            (97, 2, 2, 0, 1, 2)
        );
        assert_eq!(m.first(), Some((0, value(0))));
        assert_eq!(m.last(), Some((1000, value(1000))));
        assert_eq!(
            m.read_dir_entry(0),
            DirEntry {
                min_key: 0,
                page_id: 1
            }
        );
        assert_eq!(
            m.read_dir_entry(1),
            DirEntry {
                min_key: 128,
                page_id: 2
            }
        );
        assert_eq!(
            (m.read_page_header(1).len, m.read_page_header(2).len),
            (64, 33)
        );
        assert_eq!(m.read_page_header(1).flags, PAGE_FLAG_ACTIVE);
        assert_eq!(m.read_page_header(2).flags, PAGE_FLAG_ACTIVE);
        assert_eq!(
            (
                m.read_page_header(1).prev,
                m.read_page_header(1).next,
                m.read_page_header(2).prev,
                m.read_page_header(2).next
            ),
            (0, 2, 1, 0)
        );
        m.validate().unwrap();
        let changed = value(64) ^ (1 << 56);
        let healthy = backing(&m.memory.inner);

        // Case1: direct owning seam, healthy snapshot.
        let snap = backing(&m.memory.inner);
        let size_snap = m.memory.size();
        m.memory.traffic.set(Traffic::default());
        m.work.set(WorkObservation::default());
        {
            let mut image = EditImage::new();
            let plan = EditPlan::new(&m, &mut image).unwrap();
            assert!(matches!(
                plan.page(17601855032165478, |_, _| {
                    panic!("page address overflow must reject before reading")
                }),
                Err(EditFailure::AddressOverflow)
            ));
        }
        assert_eq!(backing(&m.memory.inner), snap);
        assert_eq!(m.memory.size(), size_snap);
        let traffic = m.memory.traffic.get();
        assert_eq!(
            (traffic.writes, traffic.grows, traffic.grown_pages),
            (0, 0, 0)
        );
        m.memory.write(0, &healthy);
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!(m.header(), healthy_header);
        m.validate().unwrap();

        // Case2a: poison count L-1 -> end overflow -> OutOfMemory.
        m.memory
            .write(OFFSET_PAGE_COUNT, &17601855032165477u64.to_le_bytes());
        let snap = backing(&m.memory.inner);
        let size_snap = m.memory.size();
        m.memory.traffic.set(Traffic::default());
        m.work.set(WorkObservation::default());
        let result = m.insert(1, 1);
        assert_eq!(result, Err(Error::OutOfMemory));
        let work = m.work.get();
        assert_eq!(
            (work.selected, work.completed),
            (Some((0, 1, PAGE_FLAG_ACTIVE)), 0)
        );
        let traffic = m.memory.traffic.get();
        assert_eq!(
            (traffic.writes, traffic.grows, traffic.grown_pages),
            (0, 0, 0)
        );
        assert_eq!(backing(&m.memory.inner), snap);
        assert_eq!(m.memory.size(), size_snap);
        m.memory.write(OFFSET_PAGE_COUNT, &2u64.to_le_bytes());
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!(m.header(), healthy_header);
        m.validate().unwrap();

        // Case2b: poison count L -> helper rejection -> exact pre-publication payload.
        m.memory
            .write(OFFSET_PAGE_COUNT, &17601855032165478u64.to_le_bytes());
        let snap = backing(&m.memory.inner);
        let size_snap = m.memory.size();
        m.memory.traffic.set(Traffic::default());
        m.work.set(WorkObservation::default());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| m.insert(1, 1)));
        let payload = match outcome {
            Err(boxed) => boxed
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| boxed.downcast_ref::<String>().cloned()),
            Ok(_) => None,
        };
        assert_eq!(
            payload.as_deref(),
            Some("paged-map address overflow before publication")
        );
        let work = m.work.get();
        assert_eq!(
            (work.selected, work.completed),
            (Some((0, 1, PAGE_FLAG_ACTIVE)), 0)
        );
        let traffic = m.memory.traffic.get();
        assert_eq!(
            (traffic.writes, traffic.grows, traffic.grown_pages),
            (0, 0, 0)
        );
        assert_eq!(backing(&m.memory.inner), snap);
        assert_eq!(m.memory.size(), size_snap);
        m.memory.write(OFFSET_PAGE_COUNT, &2u64.to_le_bytes());
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!(m.header(), healthy_header);
        m.validate().unwrap();

        // Case3: poisoned counts L-1 and L -> prepared address overflow at edit1.
        let edits = [
            Edit::Insert {
                key: 64,
                value: changed,
            },
            Edit::Insert {
                key: 1,
                value: value(1),
            },
        ];
        for count in [17601855032165477u64, 17601855032165478u64] {
            m.memory.write(OFFSET_PAGE_COUNT, &count.to_le_bytes());
            let snap = backing(&m.memory.inner);
            let size_snap = m.memory.size();
            m.memory.traffic.set(Traffic::default());
            m.work.set(WorkObservation::default());
            let Err(prepare_error) = m.prepare_edits(&edits) else {
                panic!("expected AddressOverflow at edit1 after poisoned count {count}");
            };
            assert!(matches!(
                prepare_error,
                PrepareError::AddressOverflow { at: 1 }
            ));
            let work = m.work.get();
            assert_eq!(work.completed, 1);
            assert_eq!(work.first_staged, Some((Some(value(64)), Some(changed))));
            assert_eq!(work.selected, Some((1, 1, PAGE_FLAG_ACTIVE)));
            let traffic = m.memory.traffic.get();
            assert_eq!(
                (traffic.writes, traffic.grows, traffic.grown_pages),
                (0, 0, 0)
            );
            assert_eq!(backing(&m.memory.inner), snap);
            assert_eq!(m.memory.size(), size_snap);
            m.memory.write(OFFSET_PAGE_COUNT, &2u64.to_le_bytes());
            assert_eq!(m.iter().collect::<Vec<_>>(), expected);
            assert_eq!(m.header(), healthy_header);
            m.validate().unwrap();
        }
    }

    #[test]
    fn a0_admission_zero_reservation() {
        let m = map();
        let expected: Vec<_> = (0..PAGE_CAP)
            .map(|key| (key, 0xa500_0000_0000_0000 ^ key))
            .collect();
        for &(key, value) in &expected {
            assert_eq!(m.insert(key, value), Ok(None));
        }
        m.validate().unwrap();
        let bytes = || {
            let mut bytes = vec![0; (m.memory.size() * WASM_PAGE_SIZE) as usize];
            m.memory.read(0, &mut bytes);
            bytes
        };
        let header = m.header();
        assert_eq!(m.read_page_header(header.last_page).len as u64, PAGE_CAP);
        let before = bytes();
        let size = m.memory.size();
        m.reserve_for_inserts(0).unwrap();
        let after = bytes();
        eprintln!("ADMISSION_ZERO zero reached: last value {:?}", m.last());
        assert!(before == after, "zero reservation changed backing");
        assert_eq!(m.memory.size(), size);
        assert_eq!(m.header(), header);
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);

        m.reserve_for_inserts(64).unwrap();
        assert_eq!(m.header(), header);
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        let last_reserved_byte = page_offset(header.page_count + 64) + PAGE_STRIDE - 1;
        assert!(last_reserved_byte >= page_offset(header.page_count) + PAGE_STRIDE);
        m.memory.write(last_reserved_byte, &[0x7d]);
        let before = bytes();
        let size = m.memory.size();
        m.reserve_for_inserts(64).unwrap();
        let after = bytes();
        assert!(
            before == after,
            "already-backed reservation overwrote its sentinel"
        );
        assert_eq!(m.memory.size(), size);
        assert_eq!(m.header(), header);
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        m.validate().unwrap();
        eprintln!("ADMISSION_ZERO zero and already-backed positive reservation passed");
    }

    #[test]
    fn a0_admission_directory_reservation() {
        let m = map();
        m.reserve_for_inserts(DIR_CAP).unwrap();
        let value = |key| 0xa500_0000_0000_0000 ^ key;
        let header = seed_full_directory(&m, value);
        let mut expected: Vec<_> = (128..192)
            .chain((2..=DIR_CAP).map(|page| 128 * page))
            .map(|key| (key, value(key)))
            .collect();
        assert_eq!(expected.len(), 8255);
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        m.validate().unwrap();
        m.reserve_for_inserts(1).unwrap();
        assert_eq!(m.header(), header);
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        let mut before = vec![0; (m.memory.size() * WASM_PAGE_SIZE) as usize];
        m.memory.read(0, &mut before);
        assert_eq!(m.insert(192, value(192)), Err(Error::DirectoryFull));
        let mut after = vec![0; (m.memory.size() * WASM_PAGE_SIZE) as usize];
        m.memory.read(0, &mut after);
        assert!(before == after, "full-directory rejection changed backing");
        assert_eq!(m.header(), header);
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        m.validate().unwrap();
        eprintln!("ADMISSION_DIRECTORY valid 8255-entry rejection passed");

        assert_eq!(m.remove(256), Ok(Some(value(256))));
        assert_eq!(m.header().free_head, 2);
        assert_eq!(m.header().dir_len, DIR_CAP - 1);
        assert_eq!(m.len(), 8254);
        assert_eq!(m.read_page_header(2).flags, PAGE_FLAG_FREE);
        m.validate().unwrap();
        assert_eq!(m.insert(192, value(192)), Ok(None));
        expected.retain(|&(key, _)| key != 256);
        expected.insert(PAGE_CAP as usize, (192, value(192)));
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!(m.header(), header);
        assert_eq!(
            m.read_page_header(1),
            PageHeader {
                flags: PAGE_FLAG_ACTIVE,
                len: 32,
                prev: 0,
                next: 2,
            }
        );
        assert_eq!(
            m.read_page_header(2),
            PageHeader {
                flags: PAGE_FLAG_ACTIVE,
                len: 33,
                prev: 1,
                next: 3,
            }
        );
        assert_eq!(m.read_page_header(3).prev, 2);
        assert_eq!(
            m.read_dir_entry(1),
            DirEntry {
                min_key: 160,
                page_id: 2
            }
        );
        m.validate().unwrap();
        eprintln!("ADMISSION_DIRECTORY exact removal/retry reused page 2");
    }

    #[test]
    fn lookup_metadata_uses_selected_reader() {
        let m = map();
        seed_scalar_work(&m);
        let before = backing(&m.memory);
        let mut image = EditImage::new();
        let mut plan = EditPlan::new(&m, &mut image).unwrap();
        for (at, (key, len, position)) in [(129, 33, Err(1)), (2000, 34, Ok(33))]
            .into_iter()
            .enumerate()
        {
            let mut expected = vec![(OFFSET_DIR_LEN, 8)];
            if at == 1 {
                assert_eq!(
                    plan.apply_edit(Edit::Insert { key, value: 42 }, at)
                        .unwrap(),
                    None
                );
                expected.push((OFFSET_PAGE_COUNT, 8));
            }
            expected.push((page_offset(2), 32));
            let reads = std::cell::RefCell::new(Vec::new());
            let location = plan
                .lookup(Edit::Remove { key }, at, |address, dst| {
                    reads.borrow_mut().push((address, dst.len()));
                    if at == 0 {
                        m.memory.read(address, dst);
                    } else {
                        plan.image.read(&m.memory, address, dst);
                    }
                })
                .unwrap()
                .unwrap();
            assert_eq!(
                (location.dir.page_id, location.header.len, location.position),
                (2, len, position)
            );
            let reads = reads.into_inner();
            assert!(
                !reads
                    .iter()
                    .any(|&(address, _)| address == page_entry_offset(2, 0)),
                "first key must share the header read"
            );
            let metadata: Vec<_> = reads
                .into_iter()
                .filter(|(address, _)| {
                    *address == OFFSET_DIR_LEN
                        || *address == OFFSET_PAGE_COUNT
                        || *address == page_offset(2)
                })
                .collect();
            assert_eq!(metadata, expected, "metadata reader for edit {at}");
            assert!(backing(&m.memory) == before, "lookup changed backing");
        }
        m.validate().unwrap();
    }

    #[test]
    fn edit_lookup_sources() {
        // Scenario A: one ordered batch over the shared97-key fixture. Remove129 is
        // a miss and stages nothing, so the next intent still probes backing; the
        // staged tail insert forces the third intent through the prospective view.
        let mut m = map();
        seed_scalar_work(&m);
        let value = |key| 0xa500_0000_0000_0000 | key;
        let memory = m.memory.clone();
        let before = backing(&memory);
        let edits = [
            Edit::Remove { key: 129 },
            Edit::Insert {
                key: 2000,
                value: value(2000),
            },
            Edit::Remove { key: 2000 },
        ];
        m.work.set(WorkObservation::default());
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), [None, None, Some(value(2000))]);
        let work = ready.owner.work.get();
        assert_eq!((work.backing_lookups, work.prospective_lookups), (2, 1));
        assert!(backing(&memory) == before, "prepare wrote canonical bytes");
        ready.discard();
        assert!(backing(&memory) == before, "discard wrote canonical bytes");
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), [None, None, Some(value(2000))]);
        assert!(
            backing(&memory) == before,
            "re-prepare wrote canonical bytes"
        );
        ready.publish();
        let expected: Vec<_> = (0..=126)
            .step_by(2)
            .chain((128..=252).step_by(4))
            .chain([1000])
            .map(|key| (key, value(key)))
            .collect();
        assert_eq!(m.iter().collect::<Vec<_>>(), expected);
        assert_eq!(m.len(), 97);
        m.validate().unwrap();

        // Scenario B: a valid empty header on one Wasm page reopens (the page_count0
        // layout check has no backing-extent requirement); a poisoned directory
        // length must route prospectively and fail with the exact pre-write address
        // error rather than a raw out-of-bounds read.
        let memory = DefaultMemoryImpl::default();
        assert_eq!(memory.grow(1), 0);
        write_header(
            &memory,
            &HeaderV1 {
                magic: MAGIC,
                version: LAYOUT_VERSION,
                len: 0,
                page_count: 0,
                free_head: 0,
                first_page: 0,
                last_page: 0,
                dir_len: 0,
                dir_cap: DIR_CAP,
            },
        )
        .unwrap();
        let mut m = StablePagedOrderedMap::init(memory.clone()).unwrap();
        m.write_dir_len(DIR_CAP);
        let before = backing(&memory);
        let size = memory.size();
        match m.prepare_edits(&[Edit::Remove { key: 0 }]) {
            Err(PrepareError::AddressOverflow { at: 0 }) => {}
            _ => panic!("expected PrepareError::AddressOverflow at edit0"),
        }
        let work = m.work.get();
        assert_eq!((work.backing_lookups, work.prospective_lookups), (0, 1));
        assert!(
            backing(&memory) == before,
            "rejected preparation changed backing"
        );
        assert_eq!(memory.size(), size);
    }

    #[test]
    fn empty_init() {
        let m = map();
        assert_eq!(m.len(), 0);
        assert!(m.is_empty());
        assert_eq!(m.first(), None);
        assert_eq!(m.last(), None);
        m.validate().unwrap();
    }

    #[test]
    fn empty_new_equals_init_on_fresh_memory() {
        let a = StablePagedOrderedMap::init(DefaultMemoryImpl::default()).unwrap();
        let b = StablePagedOrderedMap::new(DefaultMemoryImpl::default()).unwrap();
        assert!(a.is_empty() && b.is_empty());
        a.validate().unwrap();
        b.validate().unwrap();
    }

    #[test]
    fn insert_get_replace_remove() {
        let m = map();
        assert_eq!(m.insert(10, 100).unwrap(), None);
        assert_eq!(m.get(10), Some(100));
        assert_eq!(m.insert(10, 200).unwrap(), Some(100));
        assert_eq!(m.get(10), Some(200));
        assert_eq!(m.remove(10).unwrap(), Some(200));
        assert_eq!(m.remove(10).unwrap(), None);
        assert!(m.is_empty());
        m.validate().unwrap();
    }

    #[test]
    fn drained_map_reopens_and_accepts_inserts() {
        // Draining the last entry must leave reopenable header state: an
        // emptied map behaves like a fresh one (init succeeds, freed pages
        // are recycled by later inserts). Guards the free-span reopen path
        // (allocator draining its final span across an upgrade boundary).
        let m = map();
        m.insert(10, 100).unwrap();
        m.insert(20, 200).unwrap();
        assert_eq!(m.remove(10).unwrap(), Some(100));
        assert_eq!(m.remove(20).unwrap(), Some(200));
        assert!(m.is_empty());
        let h = m.header();
        assert_eq!(h.len, 0);
        assert_eq!(h.page_count, 0, "drained map must reset page count");
        assert_eq!(h.free_head, 0, "drained map must reset free head");
        assert_eq!(h.first_page, 0);
        assert_eq!(h.last_page, 0);
        assert_eq!(h.dir_len, 0);
        m.validate().unwrap();
        let mem = m.into_memory();
        let m2 = StablePagedOrderedMap::init(mem).expect("drained map must reopen");
        m2.validate().unwrap();
        assert_eq!(m2.get(10), None);
        assert_eq!(m2.insert(30, 300).unwrap(), None);
        assert_eq!(m2.get(30), Some(300));
        m2.validate().unwrap();
    }

    #[test]
    fn predecessor_successor_across_pages() {
        let m = map();
        for i in 0..200 {
            m.insert(i * 10, i).unwrap();
        }
        assert_eq!(m.predecessor(1005), Some((1000, 100)));
        assert_eq!(m.successor(1005), Some((1010, 101)));
        assert_eq!(m.predecessor(0), None);
        assert_eq!(m.successor(1990), None);
        assert_eq!(m.first(), Some((0, 0)));
        assert_eq!(m.last(), Some((1990, 199)));
        m.validate().unwrap();
    }

    #[test]
    fn predecessor_or_equal_reports_exact_matches_across_pages() {
        let m = map();
        assert_eq!(m.predecessor_or_equal(5), None);
        for i in 0..200 {
            m.insert(i * 10, i).unwrap();
        }
        // Exact match, in-page predecessor, first key, page boundary, and past the end.
        assert_eq!(m.predecessor_or_equal(1000), Some((1000, 100)));
        assert_eq!(m.predecessor_or_equal(1005), Some((1000, 100)));
        assert_eq!(m.predecessor_or_equal(0), Some((0, 0)));
        assert_eq!(m.predecessor_or_equal(1990), Some((1990, 199)));
        assert_eq!(m.predecessor_or_equal(1995), Some((1990, 199)));
        assert_eq!(m.predecessor_or_equal(u64::MAX), Some((1990, 199)));
        m.validate().unwrap();
    }

    #[test]
    fn iter_yields_ascending_across_pages() {
        let m = map();
        assert_eq!(m.iter().next(), None);
        for i in (0..200u64).rev() {
            m.insert(i * 10, i).unwrap();
        }
        let collected: Vec<(u64, u64)> = m.iter().collect();
        assert_eq!(collected.len(), 200);
        let expected: Vec<(u64, u64)> = (0..200u64).map(|i| (i * 10, i)).collect();
        assert_eq!(collected, expected);
        // IntoIterator for &map mirrors iter().
        assert_eq!((&m).into_iter().count(), 200);
    }

    #[test]
    fn inserts_out_of_order_and_splits() {
        let m = map();
        for i in (0..180).rev() {
            m.insert(i, i + 1).unwrap();
        }
        for i in 0..180 {
            assert_eq!(m.get(i), Some(i + 1));
        }
        assert_eq!(m.len(), 180);
        m.validate().unwrap();
    }

    #[test]
    fn remove_empty_page_and_reopen() {
        let memory = DefaultMemoryImpl::default();
        let m = StablePagedOrderedMap::init(memory).unwrap();
        for i in 0..90 {
            m.insert(i, i * 2).unwrap();
        }
        for i in 0..64 {
            assert_eq!(m.remove(i).unwrap(), Some(i * 2));
        }
        m.validate().unwrap();
        let memory = m.into_memory();
        let reopened = StablePagedOrderedMap::init(memory).unwrap();
        assert_eq!(reopened.first(), Some((64, 128)));
        assert_eq!(reopened.len(), 26);
        reopened.validate().unwrap();
    }

    #[test]
    fn directory_full_split_is_side_effect_free() {
        let m = map();
        for key in 0..PAGE_CAP {
            m.insert(key, key + 1).unwrap();
        }
        let page = m.read_first_page();
        m.write_dir_len(DIR_CAP);
        let header_before = m.header();
        let page_header_before = m.read_page_header(page);
        let entries_before = (0..PAGE_CAP as u16)
            .map(|index| m.read_entry(page, index))
            .collect::<Vec<_>>();

        assert_eq!(
            m.split_page_and_insert(page, PAGE_CAP, PAGE_CAP + 1, PAGE_CAP as u16),
            Err(Error::DirectoryFull)
        );

        assert_eq!(m.header(), header_before);
        assert_eq!(m.read_page_header(page), page_header_before);
        assert_eq!(
            (0..PAGE_CAP as u16)
                .map(|index| m.read_entry(page, index))
                .collect::<Vec<_>>(),
            entries_before
        );
        m.write_dir_len(1);
        m.validate().unwrap();
    }

    #[test]
    fn prepared_edits_reject_inconsistent_minimum() {
        let mut m = map();
        let memory = m.memory.clone();
        let value = |key| 0xa500_0000_0000_0000 | key;
        for key in [10, 20, 30] {
            assert_eq!(m.insert(key, value(key)), Ok(None));
        }
        m.validate().unwrap();
        let healthy = backing(&memory);
        let original = m.read_dir_entry(0);
        assert_eq!((original.min_key, original.page_id), (10, 1));
        m.write_dir_entry(
            0,
            DirEntry {
                min_key: 9,
                ..original
            },
        );
        let poisoned = backing(&memory);
        let edits = [Edit::Remove { key: 20 }];
        match m.prepare_edits(&edits) {
            Err(PrepareError::Edit { at, source }) => {
                assert_eq!((at, source), (0, Error::Corrupt));
            }
            Ok(ready) => {
                ready.discard();
                panic!("minimum mismatch unexpectedly reached ready");
            }
            Err(_) => panic!("expected minimum mismatch at edit0"),
        }
        assert!(
            backing(&memory) == poisoned,
            "minimum rejection wrote bytes"
        );
        assert_eq!(m.remove(20), Err(Error::Corrupt));
        assert!(backing(&memory) == poisoned, "scalar rejection wrote bytes");
        m.write_dir_entry(0, original);
        assert!(backing(&memory) == healthy);
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), [Some(value(20))]);
        assert!(backing(&memory) == healthy, "prepare wrote canonical bytes");
        ready.discard();
        assert!(backing(&memory) == healthy, "discard wrote canonical bytes");
        let ready = m.prepare_edits(&edits).unwrap();
        assert_eq!(ready.previous_values(), [Some(value(20))]);
        ready.publish();
        assert_eq!(
            m.iter().collect::<Vec<_>>(),
            [(10, value(10)), (30, value(30))]
        );
        assert_eq!(m.len(), 2);
        assert_eq!(m.read_dir_entry(0), original);
        m.validate().unwrap();
    }

    #[test]
    fn edit_image_cover_empty_entry_and_zero_count_contract() {
        let memory = DefaultMemoryImpl::default();
        assert_eq!(memory.grow(1), 0);
        let base = 256u64;
        let canonical: [u8; 128] = std::array::from_fn(|i| (i + 1) as u8);
        memory.write(base, &canonical);
        let before = backing(&memory);
        for (name, address, count, heap) in [
            ("inline-first", base, 8, false),
            ("heap-first", base + 64, 48, true),
            ("empty-zero", base + 120, 0, false),
        ] {
            let mut image = EditImage::new();
            image.backed_end = memory.size() * WASM_PAGE_SIZE;
            if heap {
                image.ranges.reserve(9).unwrap();
                image.bytes.reserve(48).unwrap();
            }
            assert_eq!(matches!(&image.ranges, Scratch::Heap(_)), heap);
            assert_eq!(matches!(&image.bytes, Scratch::Heap(_)), heap);
            assert!(image.ranges.as_slice().is_empty());
            assert!(image.bytes.as_slice().is_empty());
            let capacity = (image.ranges.capacity(), image.bytes.capacity());
            if heap {
                assert!(capacity.0 >= 9 && capacity.1 >= 48);
            } else {
                assert_eq!(image.ranges.dynamic_capacity(), 0);
                assert_eq!(image.bytes.dynamic_capacity(), 0);
            }
            let mut calls = Vec::new();
            eprintln!("FIRST_COVER_CASE {name}");
            let window = image.cover(address, count, |at, dst| {
                calls.push((at, dst.len()));
                memory.read(at, dst);
            });
            let expected_calls = if count == 0 {
                vec![]
            } else {
                vec![(address, count)]
            };
            assert_eq!(calls, expected_calls, "{name}: callback");
            let start = (address - base) as usize;
            assert_eq!(window.len(), count, "{name}: exact window length");
            if count == 0 {
                assert!(window.is_empty(), "{name}: zero-count window");
            } else {
                assert_eq!(
                    window,
                    &canonical[start..start + count],
                    "{name}: exact window content"
                );
                window[0] = 0xEE;
            }
            // The window borrow ends above; the owner carries the injected byte.
            if count != 0 {
                assert_eq!(
                    image.bytes.as_slice()[0],
                    0xEE,
                    "{name}: write-through at packed offset zero"
                );
                image.bytes.as_mut_slice()[0] = canonical[start];
            }
            let ranges = image.ranges.as_slice();
            assert_eq!(ranges.len(), 1);
            assert_eq!(
                (ranges[0].address, ranges[0].offset, ranges[0].len),
                (address, 0, count)
            );
            assert_eq!(image.bytes.as_slice(), &canonical[start..start + count]);
            assert_eq!((image.ranges.capacity(), image.bytes.capacity()), capacity);
            assert_eq!(backing(&memory), before);
            if count == 8 {
                let second = base + 32;
                calls.clear();
                eprintln!("FIRST_COVER_CASE inline-disjoint");
                let window = image.cover(second, 16, |at, dst| {
                    calls.push((at, dst.len()));
                    memory.read(at, dst);
                });
                assert_eq!(calls, vec![(second, 16)], "inline-disjoint: callback");
                assert_eq!(window.len(), 16, "inline-disjoint: window length");
                assert_eq!(
                    window,
                    &canonical[32..48],
                    "inline-disjoint: window at nonzero packed offset"
                );
                window[0] = 0xEE;
                assert_eq!(
                    image.bytes.as_slice()[8],
                    0xEE,
                    "inline-disjoint: write-through at packed offset eight"
                );
                image.bytes.as_mut_slice()[8] = canonical[32];
                let shape: Vec<_> = image
                    .ranges
                    .as_slice()
                    .iter()
                    .map(|range| (range.address, range.offset, range.len))
                    .collect();
                assert_eq!(shape, vec![(base, 0, 8), (second, 8, 16)]);
                assert_eq!(&image.bytes.as_slice()[..8], &canonical[..8]);
                assert_eq!(&image.bytes.as_slice()[8..], &canonical[32..48]);
                assert_eq!((image.ranges.capacity(), image.bytes.capacity()), capacity);
                assert_eq!(backing(&memory), before);
            }
        }
        // Four separated staged ranges, then a bridge exposing two ordered gaps and
        // a live trailing descriptor through grow/move/shrink on one reserved image.
        let mut image = EditImage::new();
        image.backed_end = memory.size() * WASM_PAGE_SIZE;
        image.ranges.reserve(6).unwrap();
        image.bytes.reserve(96).unwrap();
        assert!(matches!(&image.ranges, Scratch::Heap(_)));
        assert!(matches!(&image.bytes, Scratch::Heap(_)));
        let capacity = (image.ranges.capacity(), image.bytes.capacity());
        let staged_a: [u8; 8] = [0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8];
        let staged_b: [u8; 8] = [0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8];
        let staged_c: [u8; 8] = [0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8];
        let staged_d: [u8; 8] = [0xD1, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8];
        let mut calls = Vec::new();
        eprintln!("BORROWED_WINDOW_CASE four-range-bridge");
        // Stage base+40 first so base+16 inserts BEFORE it (relocation path).
        {
            let window = image.cover(base + 40, 8, |at, dst| {
                calls.push((at, dst.len()));
                memory.read(at, dst);
            });
            assert_eq!(window.len(), 8, "stage-b: window length");
            window.copy_from_slice(&staged_b);
        }
        assert_eq!(calls, vec![(base + 40, 8)], "stage-b: callback");
        assert_eq!(
            &image.bytes.as_slice()[0..8],
            &staged_b[..],
            "stage-b payload"
        );
        calls.clear();
        {
            let window = image.cover(base + 16, 8, |at, dst| {
                calls.push((at, dst.len()));
                memory.read(at, dst);
            });
            assert_eq!(window.len(), 8, "stage-a: window length");
            window.copy_from_slice(&staged_a);
        }
        assert_eq!(calls, vec![(base + 16, 8)], "stage-a: callback");
        assert_eq!(
            &image.bytes.as_slice()[0..8],
            &staged_a[..],
            "stage-a payload"
        );
        assert_eq!(
            &image.bytes.as_slice()[8..16],
            &staged_b[..],
            "stage-a: insert-before relocated the staged payload"
        );
        calls.clear();
        {
            let window = image.cover(base + 64, 8, |at, dst| {
                calls.push((at, dst.len()));
                memory.read(at, dst);
            });
            assert_eq!(window.len(), 8, "stage-c: window length");
            window.copy_from_slice(&staged_c);
        }
        assert_eq!(calls, vec![(base + 64, 8)], "stage-c: callback");
        calls.clear();
        {
            let window = image.cover(base + 96, 8, |at, dst| {
                calls.push((at, dst.len()));
                memory.read(at, dst);
            });
            assert_eq!(window.len(), 8, "stage-d: window length");
            window.copy_from_slice(&staged_d);
        }
        assert_eq!(calls, vec![(base + 96, 8)], "stage-d: callback");
        let staged_shape: Vec<_> = image
            .ranges
            .as_slice()
            .iter()
            .map(|range| (range.address, range.offset, range.len))
            .collect();
        assert_eq!(
            staged_shape,
            vec![
                (base + 16, 0, 8),
                (base + 40, 8, 8),
                (base + 64, 16, 8),
                (base + 96, 24, 8)
            ],
            "staged: four separated descriptors with packed offsets"
        );
        assert_eq!(&image.bytes.as_slice()[16..24], &staged_c[..]);
        assert_eq!(&image.bytes.as_slice()[24..32], &staged_d[..]);
        // Contained nonzero window inside the larger staged range a.
        calls.clear();
        {
            let window = image.cover(base + 18, 4, |at, dst| {
                calls.push((at, dst.len()));
                memory.read(at, dst);
            });
            assert_eq!(window.len(), 4, "contained: window length");
            assert_eq!(&window[..], &staged_a[2..6], "contained: window content");
            window[0] = 0xEE;
        }
        assert!(calls.is_empty(), "contained: no callback");
        assert_eq!(
            image.bytes.as_slice()[2],
            0xEE,
            "contained: write-through at nonzero offset within range"
        );
        image.bytes.as_mut_slice()[2] = staged_a[2];
        assert_eq!(
            &image.bytes.as_slice()[0..8],
            &staged_a[..],
            "contained: staged payload restored"
        );
        // Contained zero-count: immediate byte and descriptor equality.
        calls.clear();
        let bytes_before: Vec<u8> = image.bytes.as_slice().to_vec();
        let shape_before: Vec<_> = image
            .ranges
            .as_slice()
            .iter()
            .map(|range| (range.address, range.offset, range.len))
            .collect();
        {
            let window = image.cover(base + 20, 0, |at, dst| {
                calls.push((at, dst.len()));
                memory.read(at, dst);
            });
            assert!(window.is_empty(), "contained-zero: empty window");
        }
        assert!(calls.is_empty(), "contained-zero: no callback");
        assert_eq!(
            image.bytes.as_slice(),
            &bytes_before[..],
            "contained-zero: bytes unchanged immediately"
        );
        assert_eq!(
            image
                .ranges
                .as_slice()
                .iter()
                .map(|range| (range.address, range.offset, range.len))
                .collect::<Vec<_>>(),
            shape_before,
            "contained-zero: descriptors unchanged immediately"
        );
        // Bridge [base+20, base+68): three ranges coalesce while the trailing
        // descriptor moves and rebases, and both interior gaps fill in order.
        calls.clear();
        {
            let window = image.cover(base + 20, 48, |at, dst| {
                calls.push((at, dst.len()));
                memory.read(at, dst);
            });
            assert_eq!(
                window.len(),
                48,
                "bridge: exact requested window, not the merged union"
            );
            let mut expected_window: Vec<u8> = Vec::new();
            expected_window.extend_from_slice(&staged_a[4..8]);
            expected_window.extend_from_slice(&canonical[24..40]);
            expected_window.extend_from_slice(&staged_b);
            expected_window.extend_from_slice(&canonical[48..64]);
            expected_window.extend_from_slice(&staged_c[0..4]);
            assert_eq!(
                window,
                &expected_window[..],
                "bridge: window starts four bytes inside the merged range"
            );
            window[0] = 0xEE;
        }
        assert_eq!(
            calls,
            vec![(base + 24, 16), (base + 48, 16)],
            "bridge: two fill-gap calls in order"
        );
        let shape: Vec<_> = image
            .ranges
            .as_slice()
            .iter()
            .map(|range| (range.address, range.offset, range.len))
            .collect();
        assert_eq!(
            shape,
            vec![(base + 16, 0, 56), (base + 96, 56, 8)],
            "bridge: merged descriptor plus rebased trailing descriptor"
        );
        let mut expected_packed: Vec<u8> = Vec::new();
        expected_packed.extend_from_slice(&staged_a);
        expected_packed.extend_from_slice(&canonical[24..40]);
        expected_packed.extend_from_slice(&staged_b);
        expected_packed.extend_from_slice(&canonical[48..64]);
        expected_packed.extend_from_slice(&staged_c);
        expected_packed.extend_from_slice(&staged_d);
        expected_packed[4] = 0xEE;
        assert_eq!(
            image.bytes.as_slice(),
            &expected_packed[..],
            "bridge: complete packed bytes after the window mutation"
        );
        assert_eq!(
            &image.bytes.as_slice()[0..4],
            &staged_a[0..4],
            "bridge: untouched union prefix"
        );
        assert_eq!(
            &image.bytes.as_slice()[52..56],
            &staged_c[4..8],
            "bridge: untouched union suffix"
        );
        assert_eq!(
            &image.bytes.as_slice()[56..64],
            &staged_d[..],
            "bridge: trailing staged payload preserved"
        );
        assert_eq!(
            (image.ranges.capacity(), image.bytes.capacity()),
            capacity,
            "bridge: capacity unchanged"
        );
        assert_eq!(backing(&memory), before, "bridge: backing unchanged");

        // Extend only the first range: descriptor count stays fixed while the
        // packed suffix moves. This differs from both insertion and coalescing.
        calls.clear();
        {
            let window = image.cover(base + 8, 12, |at, dst| {
                calls.push((at, dst.len()));
                memory.read(at, dst);
            });
            let mut expected_window = canonical[8..16].to_vec();
            expected_window.extend_from_slice(&staged_a[..4]);
            assert_eq!(window, expected_window, "equal-count: exact window");
            window[0] = 0xEF;
        }
        assert_eq!(calls, vec![(base + 8, 8)], "equal-count: fill gap");
        let mut extended = canonical[8..16].to_vec();
        extended[0] = 0xEF;
        extended.extend_from_slice(&expected_packed);
        assert_eq!(
            image.bytes.as_slice(),
            extended,
            "equal-count: packed bytes"
        );
        assert_eq!(
            image
                .ranges
                .as_slice()
                .iter()
                .map(|range| (range.address, range.offset, range.len))
                .collect::<Vec<_>>(),
            vec![(base + 8, 0, 64), (base + 96, 64, 8)],
            "equal-count: descriptors and trailing rebase"
        );
        assert_eq!(
            (image.ranges.capacity(), image.bytes.capacity()),
            capacity,
            "equal-count: capacity unchanged"
        );
        assert_eq!(backing(&memory), before, "equal-count: backing unchanged");
    }

    #[test]
    fn scratch_resize_reserved_view_contract() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DEFAULTS: AtomicUsize = AtomicUsize::new(0);
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct CountedDefault(u64);
        impl Default for CountedDefault {
            fn default() -> Self {
                DEFAULTS.fetch_add(1, Ordering::Relaxed);
                Self(0)
            }
        }

        // Pure owner contract: exact view length, retained prefix, mutation through
        // the view observed after the borrow, capacity and representation fixed.
        for heap in [false, true] {
            let mut scratch: Scratch<u8, 32> = Scratch::new();
            if heap {
                scratch.reserve(40).unwrap();
            }
            assert_eq!(matches!(&scratch, Scratch::Heap(_)), heap);
            let capacity = scratch.capacity();
            assert_eq!(scratch.resize_reserved(5).len(), 5);
            scratch.as_mut_slice().copy_from_slice(&[9, 8, 7, 6, 5]);
            assert_eq!(scratch.resize_reserved(12).len(), 12);
            {
                let window = scratch.resize_reserved(20);
                assert_eq!(window.len(), 20, "exact grown length");
                assert_eq!(&window[..5], &[9, 8, 7, 6, 5][..], "retained prefix");
                window[5] = 0xEE;
            }
            assert_eq!(
                &scratch.as_slice()[..6],
                &[9, 8, 7, 6, 5, 0xEE][..],
                "mutation observed in owner after the borrow"
            );
            assert_eq!(scratch.resize_reserved(6).len(), 6);
            assert_eq!(
                scratch.as_slice(),
                &[9, 8, 7, 6, 5, 0xEE][..],
                "shrink keeps prefix"
            );
            assert_eq!(scratch.resize_reserved(14).len(), 14);
            assert_eq!(
                &scratch.as_slice()[..6],
                &[9, 8, 7, 6, 5, 0xEE][..],
                "re-expand within capacity keeps prefix"
            );
            assert_eq!(scratch.capacity(), capacity, "capacity unchanged");
            assert_eq!(
                matches!(&scratch, Scratch::Heap(_)),
                heap,
                "representation unchanged"
            );
            let prefix = scratch.as_slice().to_vec();
            scratch.reserve(capacity - prefix.len()).unwrap();
            assert_eq!(
                scratch.as_slice(),
                prefix,
                "fitting reservation preserves bytes"
            );
            assert_eq!(scratch.capacity(), capacity);
            assert_eq!(matches!(&scratch, Scratch::Heap(_)), heap);
            assert!(scratch.reserve(usize::MAX).is_err());
            assert_eq!(
                scratch.as_slice(),
                prefix,
                "failed reservation preserves bytes"
            );
            assert_eq!(scratch.capacity(), capacity);
            assert_eq!(matches!(&scratch, Scratch::Heap(_)), heap);
            if !heap {
                scratch.reserve(capacity - prefix.len() + 1).unwrap();
                assert!(matches!(&scratch, Scratch::Heap(_)));
                assert!(scratch.capacity() > capacity);
                assert_eq!(
                    scratch.as_slice(),
                    prefix,
                    "heap transition preserves prefix"
                );
            }
        }

        for heap in [false, true] {
            let mut scratch: Scratch<CountedDefault, 1> = Scratch::new();
            if heap {
                scratch.reserve(2).unwrap();
            }
            let capacity = scratch.capacity();
            DEFAULTS.store(0, Ordering::Relaxed);
            scratch.push_reserved(CountedDefault(0x1234));
            assert_eq!(scratch.as_slice(), &[CountedDefault(0x1234)]);
            assert_eq!(
                DEFAULTS.load(Ordering::Relaxed),
                0,
                "append must not construct a discarded default"
            );
            while scratch.as_slice().len() < capacity {
                let value = CountedDefault(scratch.as_slice().len() as u64 + 0x2000);
                scratch.push_reserved(value);
                assert_eq!(scratch.as_slice().last(), Some(&value));
            }
            let before = scratch.as_slice().to_vec();
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    scratch.push_reserved(CountedDefault(0xFFFF));
                }))
                .is_err()
            );
            let len = match &scratch {
                Scratch::Inline { len, .. } => *len,
                Scratch::Heap(items) => items.len(),
            };
            assert_eq!(len, before.len(), "failed append changed length");
            assert_eq!(scratch.as_slice(), before, "failed append changed values");
            assert_eq!(scratch.capacity(), capacity);
            assert_eq!(matches!(&scratch, Scratch::Heap(_)), heap);
            assert_eq!(DEFAULTS.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn edit_image_read_partition() {
        // Byte-composition contract, not a public map or page-admission fixture.
        let memory = WorkMemory::default();
        assert_eq!(memory.grow(3), 0);
        let b = 3 * WASM_PAGE_SIZE;
        assert_eq!(memory.size() * WASM_PAGE_SIZE, b);
        let first_unbacked = checked_page_offset(64).unwrap();
        assert_eq!(first_unbacked, 197160);
        assert!(first_unbacked >= b);
        let planned_end = first_unbacked.checked_add(PAGE_STRIDE).unwrap();
        let a = 256;
        let canonical: [u8; 128] = std::array::from_fn(|i| (i + 1) as u8);
        memory.write(a, &canonical);
        memory.write(b - 4, &[0x21, 0x22, 0x23, 0x24]);
        let before = backing(&memory.inner);

        let mut empty = EditImage::new();
        let mut inline = EditImage::new();
        let mut heap = EditImage::new();
        let mut unbacked = EditImage::new();
        let mut straddle = EditImage::new();
        for image in [
            &mut empty,
            &mut inline,
            &mut heap,
            &mut unbacked,
            &mut straddle,
        ] {
            image.backed_end = b;
            image.required_end = planned_end;
        }
        let first = [
            0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab,
        ];
        let second = [
            0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb,
        ];
        let third = [
            0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb,
        ];
        let fourth = [
            0xd0, 0xd1, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xdb,
        ];
        for (image, chunks) in [
            (
                &mut inline,
                &[(a + 16, first.as_slice()), (a + 40, second.as_slice())][..],
            ),
            (
                &mut heap,
                &[
                    (a + 16, first.as_slice()),
                    (a + 40, second.as_slice()),
                    (a + 64, third.as_slice()),
                    (a + 84, fourth.as_slice()),
                ][..],
            ),
            (
                &mut unbacked,
                &[(
                    b + 16,
                    &[0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8][..],
                )][..],
            ),
            (
                &mut straddle,
                &[
                    (b - 2, &[0xa1, 0xa2, 0xa3, 0xa4][..]),
                    (b + 4, &[0xb1, 0xb2][..]),
                ][..],
            ),
        ] {
            for &(address, bytes) in chunks {
                image.ranges.reserve(1).unwrap();
                image.bytes.reserve(bytes.len()).unwrap();
                image.write(address, bytes);
            }
        }
        assert!(matches!(&empty.ranges, Scratch::Inline { .. }));
        assert!(matches!(&empty.bytes, Scratch::Inline { .. }));
        assert!(matches!(&inline.ranges, Scratch::Inline { .. }));
        assert!(matches!(&inline.bytes, Scratch::Inline { .. }));
        assert_eq!(
            (
                inline.ranges.as_slice().len(),
                inline.bytes.as_slice().len()
            ),
            (2, 24)
        );
        assert!(matches!(&heap.ranges, Scratch::Heap(_)));
        assert!(matches!(&heap.bytes, Scratch::Heap(_)));
        assert_eq!(
            (heap.ranges.as_slice().len(), heap.bytes.as_slice().len()),
            (4, 48)
        );

        for (name, image, address, expected, read_bytes) in [
            ("empty-backed", &empty, a + 8, &[9, 10, 11, 12][..], 4),
            ("empty-zero", &empty, a + 8, &[][..], 0),
            (
                "inline-contained",
                &inline,
                a + 18,
                &[0xa2, 0xa3, 0xa4, 0xa5][..],
                0,
            ),
            (
                "inline-packed-offset",
                &inline,
                a + 42,
                &[0xb2, 0xb3, 0xb4, 0xb5][..],
                0,
            ),
            ("inline-before", &inline, a + 4, &[5, 6, 7, 8][..], 4),
            (
                "inline-left-adjacent",
                &inline,
                a + 12,
                &[13, 14, 15, 16][..],
                4,
            ),
            (
                "inline-right-adjacent",
                &inline,
                a + 28,
                &[29, 30, 31, 32][..],
                4,
            ),
            ("inline-gap", &inline, a + 32, &[33, 34, 35, 36][..], 4),
            ("inline-after", &inline, a + 60, &[61, 62, 63, 64][..], 4),
            (
                "inline-left-overlap",
                &inline,
                a + 14,
                &[15, 16, 0xa0, 0xa1, 0xa2, 0xa3][..],
                6,
            ),
            (
                "inline-right-overlap",
                &inline,
                a + 26,
                &[0xaa, 0xab, 29, 30, 31, 32][..],
                6,
            ),
            (
                "inline-multiple",
                &inline,
                a + 26,
                &[
                    0xaa, 0xab, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 0xb0, 0xb1, 0xb2,
                    0xb3,
                ][..],
                18,
            ),
            ("inline-zero-staged", &inline, a + 20, &[][..], 0),
            (
                "heap-packed-offset",
                &heap,
                a + 86,
                &[0xd2, 0xd3, 0xd4, 0xd5][..],
                0,
            ),
            ("heap-before", &heap, a + 4, &[5, 6, 7, 8][..], 4),
            (
                "heap-left-adjacent",
                &heap,
                a + 60,
                &[61, 62, 63, 64][..],
                4,
            ),
            (
                "heap-right-adjacent",
                &heap,
                a + 52,
                &[53, 54, 55, 56][..],
                4,
            ),
            ("heap-gap", &heap, a + 78, &[79, 80, 81, 82][..], 4),
            ("heap-after", &heap, a + 100, &[101, 102, 103, 104][..], 4),
            (
                "heap-left-overlap",
                &heap,
                a + 62,
                &[63, 64, 0xc0, 0xc1, 0xc2, 0xc3][..],
                6,
            ),
            (
                "heap-right-overlap",
                &heap,
                a + 74,
                &[0xca, 0xcb, 77, 78, 79, 80][..],
                6,
            ),
            (
                "heap-multiple",
                &heap,
                a + 74,
                &[
                    0xca, 0xcb, 77, 78, 79, 80, 81, 82, 83, 84, 0xd0, 0xd1, 0xd2, 0xd3,
                ][..],
                14,
            ),
            ("heap-zero-staged", &heap, a + 70, &[][..], 0),
            ("heap-zero-gap", &heap, a + 82, &[][..], 0),
            ("unbacked-unstaged", &empty, b + 16, &[0; 8][..], 0),
            ("unbacked-zero", &empty, b + 16, &[][..], 0),
            (
                "unbacked-contained",
                &unbacked,
                b + 16,
                &[0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8][..],
                0,
            ),
            (
                "unbacked-contained-offset",
                &unbacked,
                b + 18,
                &[0xe3, 0xe4, 0xe5, 0xe6][..],
                0,
            ),
            (
                "unbacked-left-partial",
                &unbacked,
                b + 14,
                &[0, 0, 0xe1, 0xe2, 0xe3, 0xe4][..],
                0,
            ),
            (
                "unbacked-right-partial",
                &unbacked,
                b + 22,
                &[0xe7, 0xe8, 0, 0, 0, 0][..],
                0,
            ),
            (
                "straddle-unstaged",
                &empty,
                b - 4,
                &[0x21, 0x22, 0x23, 0x24, 0, 0, 0, 0, 0, 0, 0, 0][..],
                4,
            ),
            (
                "straddle-overlay",
                &straddle,
                b - 4,
                &[0x21, 0x22, 0xa1, 0xa2, 0xa3, 0xa4, 0, 0, 0xb1, 0xb2, 0, 0][..],
                4,
            ),
            (
                "unbacked-gap-overlay",
                &straddle,
                b + 2,
                &[0, 0, 0xb1, 0xb2, 0, 0][..],
                0,
            ),
        ] {
            assert!(address.checked_add(expected.len() as u64).unwrap() <= planned_end);
            let mut actual = vec![0xee; expected.len()];
            eprintln!("IMAGE_READ_CASE {name}");
            memory.traffic.set(Traffic::default());
            image.read(&memory, address, &mut actual);
            let traffic = memory.traffic.get();
            assert_eq!(actual, expected, "IMAGE_READ {name} bytes");
            assert_eq!(
                traffic,
                Traffic {
                    reads: usize::from(read_bytes != 0),
                    read_bytes,
                    max_read: read_bytes,
                    ..Traffic::default()
                },
                "IMAGE_READ {name} traffic"
            );
            assert_eq!(backing(&memory.inner), before, "IMAGE_READ {name} backing");
        }
    }
}
