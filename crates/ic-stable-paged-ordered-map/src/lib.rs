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
//! [`PAGE_CAP`] sorted `(key, value)` pairs (`PAGE_CAP` × 16-byte entries after a 32-byte page header).
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
const PAGE_HEADER_SIZE: u64 = 32;
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
const PAGE_OFFSET_FREE_NEXT: u64 = 24;
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
    /// [`Memory::grow`] failed while allocating a new page.
    OutOfMemory,
    /// Internal invariants failed (detected via [`StablePagedOrderedMap::validate`] or a defensive check).
    Corrupt,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirectoryFull => write!(f, "paged ordered map directory is full"),
            Self::OutOfMemory => write!(f, "failed to grow paged ordered map memory"),
            Self::Corrupt => write!(f, "paged ordered map invariant violated"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, PartialEq, Eq)]
pub struct GrowFailed {
    current_size: u64,
    delta: u64,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirEntry {
    min_key: u64,
    page_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageHeader {
    flags: u8,
    len: u16,
    prev: u64,
    next: u64,
    free_next: u64,
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
        Ok(Self { memory })
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
        Ok(Self { memory })
    }

    /// Returns the stable V1 header fields currently persisted in memory.
    pub fn header(&self) -> HeaderV1 {
        read_header(&self.memory)
    }

    /// Consumes `self` and returns the underlying [`Memory`] handle (e.g. to persist, wrap, or re-open).
    pub fn into_memory(self) -> M {
        self.memory
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
    /// - [`Error::OutOfMemory`] — allocating a new backing page failed.
    /// - [`Error::Corrupt`] — broken directory linkage (should not happen on a map that only used this API).
    ///
    /// On success: `Ok(None)` means the key was new; `Ok(Some(previous))` means the key existed and the value was replaced.
    pub fn insert(&self, key: u64, value: u64) -> Result<Option<u64>, Error> {
        if self.read_dir_len() == 0 {
            let page = self.alloc_page()?;
            self.write_page_header(
                page,
                PageHeader {
                    flags: PAGE_FLAG_ACTIVE,
                    len: 1,
                    prev: 0,
                    next: 0,
                    free_next: 0,
                },
            );
            self.write_entry(page, 0, key, value);
            self.insert_dir_entry(
                0,
                DirEntry {
                    min_key: key,
                    page_id: page,
                },
            )?;
            self.write_first_page(page);
            self.write_last_page(page);
            self.inc_len(1);
            return Ok(None);
        }

        let page = self
            .find_page_for_key(key)
            .unwrap_or_else(|| self.read_first_page());
        match self.page_search(page, key) {
            Ok(pos) => {
                let old = self.read_entry(page, pos).1;
                self.write_entry(page, pos, key, value);
                Ok(Some(old))
            }
            Err(pos) => {
                if self.read_page_header(page).len as u64 >= PAGE_CAP {
                    self.split_page_and_insert(page, key, value)
                } else {
                    self.insert_into_page(page, pos, key, value)?;
                    self.inc_len(1);
                    Ok(None)
                }
            }
        }
    }

    /// Removes `key` and returns the former value, or `Ok(None)` when the key was not present.
    ///
    /// # Errors
    ///
    /// Same categories as [`insert`](Self::insert) for split/corruption cases; removal itself also updates the total length.
    pub fn remove(&self, key: u64) -> Result<Option<u64>, Error> {
        let Some(page) = self.find_page_for_key(key) else {
            return Ok(None);
        };
        let Ok(pos) = self.page_search(page, key) else {
            return Ok(None);
        };
        let old = self.read_entry(page, pos).1;
        self.remove_from_page(page, pos)?;
        self.inc_len(-1);
        Ok(Some(old))
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
            if header.flags != PAGE_FLAG_FREE {
                return Err(Error::Corrupt);
            }
            free = header.free_next;
        }
        Ok(())
    }

    fn split_page_and_insert(&self, page: u64, key: u64, value: u64) -> Result<Option<u64>, Error> {
        let dir_len = self.read_dir_len();
        if dir_len >= DIR_CAP {
            return Err(Error::DirectoryFull);
        }
        let dir_pos = (0..dir_len)
            .find(|&i| self.read_dir_entry(i).page_id == page)
            .ok_or(Error::Corrupt)?;
        let header = self.read_page_header(page);
        let mut entries = Vec::with_capacity((PAGE_CAP + 1) as usize);
        let mut inserted = false;
        for i in 0..header.len {
            let entry = self.read_entry(page, i);
            if !inserted && key < entry.0 {
                entries.push((key, value));
                inserted = true;
            }
            entries.push(entry);
        }
        if !inserted {
            entries.push((key, value));
        }
        let split = entries.len() / 2;
        let right_entries = entries.split_off(split);
        let left_entries = entries;
        let right = self.alloc_page()?;

        let old_next = header.next;
        self.write_page_header(
            page,
            PageHeader {
                len: left_entries.len() as u16,
                ..header
            },
        );
        for (i, (k, v)) in left_entries.iter().copied().enumerate() {
            self.write_entry(page, i as u16, k, v);
        }

        self.write_page_header(
            right,
            PageHeader {
                flags: PAGE_FLAG_ACTIVE,
                len: right_entries.len() as u16,
                prev: page,
                next: old_next,
                free_next: 0,
            },
        );
        for (i, (k, v)) in right_entries.iter().copied().enumerate() {
            self.write_entry(right, i as u16, k, v);
        }
        if old_next != 0 {
            self.write_page_fields(old_next, |h| h.prev = right);
        } else {
            self.write_last_page(right);
        }
        self.write_page_fields(page, |h| h.next = right);

        self.update_dir_min(dir_pos, left_entries[0].0);
        self.insert_dir_entry(
            dir_pos + 1,
            DirEntry {
                min_key: right_entries[0].0,
                page_id: right,
            },
        )?;
        self.inc_len(1);
        Ok(None)
    }

    fn insert_into_page(&self, page: u64, pos: u16, key: u64, value: u64) -> Result<(), Error> {
        let mut header = self.read_page_header(page);
        for i in (pos..header.len).rev() {
            let (k, v) = self.read_entry(page, i);
            self.write_entry(page, i + 1, k, v);
        }
        self.write_entry(page, pos, key, value);
        header.len += 1;
        self.write_page_header(page, header);
        if pos == 0 {
            let dir_pos = self.dir_index_for_page(page).ok_or(Error::Corrupt)?;
            self.update_dir_min(dir_pos, key);
        }
        Ok(())
    }

    fn remove_from_page(&self, page: u64, pos: u16) -> Result<(), Error> {
        let mut header = self.read_page_header(page);
        for i in pos + 1..header.len {
            let (k, v) = self.read_entry(page, i);
            self.write_entry(page, i - 1, k, v);
        }
        header.len -= 1;
        if header.len == 0 {
            self.unlink_and_free_page(page, header)?;
        } else {
            self.write_page_header(page, header);
            if pos == 0 {
                let dir_pos = self.dir_index_for_page(page).ok_or(Error::Corrupt)?;
                self.update_dir_min(dir_pos, self.read_entry(page, 0).0);
            }
        }
        Ok(())
    }

    fn unlink_and_free_page(&self, page: u64, header: PageHeader) -> Result<(), Error> {
        if header.prev != 0 {
            self.write_page_fields(header.prev, |h| h.next = header.next);
        } else {
            self.write_first_page(header.next);
        }
        if header.next != 0 {
            self.write_page_fields(header.next, |h| h.prev = header.prev);
        } else {
            self.write_last_page(header.prev);
        }
        let dir_pos = self.dir_index_for_page(page).ok_or(Error::Corrupt)?;
        self.remove_dir_entry(dir_pos)?;
        self.free_page(page);
        Ok(())
    }

    fn find_page_for_key(&self, key: u64) -> Option<u64> {
        let len = self.read_dir_len();
        if len == 0 {
            return None;
        }
        let mut lo = 0;
        let mut hi = len;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.read_dir_entry(mid).min_key <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            None
        } else {
            Some(self.read_dir_entry(lo - 1).page_id)
        }
    }

    fn dir_index_for_page(&self, page: u64) -> Option<u64> {
        (0..self.read_dir_len()).find(|&i| self.read_dir_entry(i).page_id == page)
    }

    fn insert_dir_entry(&self, pos: u64, entry: DirEntry) -> Result<(), Error> {
        let len = self.read_dir_len();
        if len >= DIR_CAP {
            return Err(Error::DirectoryFull);
        }
        for i in (pos..len).rev() {
            let e = self.read_dir_entry(i);
            self.write_dir_entry(i + 1, e);
        }
        self.write_dir_entry(pos, entry);
        self.write_dir_len(len + 1);
        Ok(())
    }

    fn remove_dir_entry(&self, pos: u64) -> Result<(), Error> {
        let len = self.read_dir_len();
        if pos >= len {
            return Err(Error::Corrupt);
        }
        for i in pos + 1..len {
            let e = self.read_dir_entry(i);
            self.write_dir_entry(i - 1, e);
        }
        self.write_dir_entry(
            len - 1,
            DirEntry {
                min_key: 0,
                page_id: 0,
            },
        );
        self.write_dir_len(len - 1);
        Ok(())
    }

    fn update_dir_min(&self, pos: u64, min_key: u64) {
        let mut entry = self.read_dir_entry(pos);
        entry.min_key = min_key;
        self.write_dir_entry(pos, entry);
    }

    fn page_search(&self, page: u64, key: u64) -> Result<u16, u16> {
        let len = self.read_page_header(page).len;
        let mut lo = 0u16;
        let mut hi = len;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let k = self.read_entry(page, mid).0;
            if k < key {
                lo = mid + 1;
            } else if k > key {
                hi = mid;
            } else {
                return Ok(mid);
            }
        }
        Err(lo)
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

    fn alloc_page(&self) -> Result<u64, Error> {
        let free = self.read_free_head();
        if free != 0 {
            let header = self.read_page_header(free);
            self.write_free_head(header.free_next);
            return Ok(free);
        }
        let page = self
            .read_page_count()
            .checked_add(1)
            .ok_or(Error::OutOfMemory)?;
        let end = page_offset(page)
            .checked_add(PAGE_STRIDE)
            .ok_or(Error::OutOfMemory)?;
        safe_write(&self.memory, end - 1, &[0]).map_err(|_| Error::OutOfMemory)?;
        self.write_page_count(page);
        Ok(page)
    }

    fn free_page(&self, page: u64) {
        let head = self.read_free_head();
        self.write_page_header(
            page,
            PageHeader {
                flags: PAGE_FLAG_FREE,
                len: 0,
                prev: 0,
                next: 0,
                free_next: head,
            },
        );
        self.write_free_head(page);
    }

    fn read_entry(&self, page: u64, index: u16) -> (u64, u64) {
        let off = page_entry_offset(page, index);
        (read_u64(&self.memory, off), read_u64(&self.memory, off + 8))
    }

    fn write_entry(&self, page: u64, index: u16, key: u64, value: u64) {
        let off = page_entry_offset(page, index);
        write_u64(&self.memory, off, key);
        write_u64(&self.memory, off + 8, value);
    }

    fn read_dir_entry(&self, index: u64) -> DirEntry {
        let off = dir_entry_offset(index);
        DirEntry {
            min_key: read_u64(&self.memory, off),
            page_id: read_u64(&self.memory, off + 8),
        }
    }

    fn write_dir_entry(&self, index: u64, entry: DirEntry) {
        let off = dir_entry_offset(index);
        write_u64(&self.memory, off, entry.min_key);
        write_u64(&self.memory, off + 8, entry.page_id);
    }

    fn read_page_header(&self, page: u64) -> PageHeader {
        let off = page_offset(page);
        PageHeader {
            flags: read_u8(&self.memory, off + PAGE_OFFSET_FLAGS),
            len: read_u16(&self.memory, off + PAGE_OFFSET_LEN),
            prev: read_u64(&self.memory, off + PAGE_OFFSET_PREV),
            next: read_u64(&self.memory, off + PAGE_OFFSET_NEXT),
            free_next: read_u64(&self.memory, off + PAGE_OFFSET_FREE_NEXT),
        }
    }

    fn write_page_header(&self, page: u64, header: PageHeader) {
        let off = page_offset(page);
        write_u8(&self.memory, off + PAGE_OFFSET_FLAGS, header.flags);
        write_u16(&self.memory, off + PAGE_OFFSET_LEN, header.len);
        write_u64(&self.memory, off + PAGE_OFFSET_PREV, header.prev);
        write_u64(&self.memory, off + PAGE_OFFSET_NEXT, header.next);
        write_u64(&self.memory, off + PAGE_OFFSET_FREE_NEXT, header.free_next);
    }

    fn write_page_fields<F>(&self, page: u64, f: F)
    where
        F: FnOnce(&mut PageHeader),
    {
        let mut header = self.read_page_header(page);
        f(&mut header);
        self.write_page_header(page, header);
    }

    fn read_dir_len(&self) -> u64 {
        read_u64(&self.memory, OFFSET_DIR_LEN)
    }

    fn write_dir_len(&self, len: u64) {
        write_u64(&self.memory, OFFSET_DIR_LEN, len);
    }

    fn read_dir_cap(&self) -> u64 {
        read_u64(&self.memory, OFFSET_DIR_CAP)
    }

    fn read_page_count(&self) -> u64 {
        read_u64(&self.memory, OFFSET_PAGE_COUNT)
    }

    fn write_page_count(&self, count: u64) {
        write_u64(&self.memory, OFFSET_PAGE_COUNT, count);
    }

    fn read_free_head(&self) -> u64 {
        read_u64(&self.memory, OFFSET_FREE_HEAD)
    }

    fn write_free_head(&self, page: u64) {
        write_u64(&self.memory, OFFSET_FREE_HEAD, page);
    }

    fn read_first_page(&self) -> u64 {
        read_u64(&self.memory, OFFSET_FIRST_PAGE)
    }

    fn write_first_page(&self, page: u64) {
        write_u64(&self.memory, OFFSET_FIRST_PAGE, page);
    }

    fn read_last_page(&self) -> u64 {
        read_u64(&self.memory, OFFSET_LAST_PAGE)
    }

    fn write_last_page(&self, page: u64) {
        write_u64(&self.memory, OFFSET_LAST_PAGE, page);
    }

    fn inc_len(&self, delta: i64) {
        let len = self.len();
        let next = if delta >= 0 {
            len + delta as u64
        } else {
            len - (-delta) as u64
        };
        write_u64(&self.memory, OFFSET_LEN, next);
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

fn write_u8<M: Memory>(memory: &M, offset: u64, value: u8) {
    write(memory, offset, &[value]);
}

fn read_u16<M: Memory>(memory: &M, offset: u64) -> u16 {
    let mut b = [0u8; 2];
    memory.read(offset, &mut b);
    u16::from_le_bytes(b)
}

fn write_u16<M: Memory>(memory: &M, offset: u64, value: u16) {
    write(memory, offset, &value.to_le_bytes());
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
            m.split_page_and_insert(page, PAGE_CAP, PAGE_CAP + 1),
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
}
