//! A stable-memory manager with an independent bucket size for each virtual memory.
//!
//! The public [`VirtualMemory`] implements the `ic_stable_structures::Memory` trait directly, so
//! it can be passed to stable structures without introducing an application-specific memory API.
//! The first version uses append-only extents: once allocated, an extent is never reclaimed.

use ic_stable_structures::{Memory, memory_manager::MemoryId};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

const MAGIC: [u8; 3] = *b"VMM";
const LAYOUT_VERSION: u8 = 1;
const MAX_MEMORY_IDS: usize = 255;
const MAX_EXTENTS: u64 = 65_536;
const UNALLOCATED: u8 = u8::MAX;
const WASM_PAGE_SIZE: u64 = 65_536;
const DATA_OFFSET_PAGES: u64 = 2;
const HEADER_SIZE: u64 = 3 + 1 + 4 + 2 + (MAX_MEMORY_IDS as u64 * 8) + (MAX_MEMORY_IDS as u64 * 2);
const HEADER_SIZE_BYTES: usize = HEADER_SIZE as usize;
const EXTENT_OWNER_OFFSET: u64 = HEADER_SIZE;
const ALLOCATED_EXTENTS_OFFSET: usize = 4;
const DEFAULT_BUCKET_SIZE_OFFSET: usize = 8;
const MEMORY_SIZES_OFFSET: usize = 10;
const MEMORY_POLICIES_OFFSET: usize = MEMORY_SIZES_OFFSET + MAX_MEMORY_IDS * 8;
const EXTENT_RESERVE_THRESHOLD: usize = 256;

/// The default bucket size used when a caller does not provide a policy.
pub const DEFAULT_BUCKET_SIZE_IN_PAGES: u16 = 128;

/// A manager that presents multiple independently growing memories over one stable memory.
pub struct MemoryManager<M: Memory> {
    inner: Rc<RefCell<Inner<M>>>,
}

impl<M: Memory> MemoryManager<M> {
    pub fn init(memory: M) -> Self {
        Self::init_with_default_bucket_size(memory, DEFAULT_BUCKET_SIZE_IN_PAGES)
    }

    pub fn init_with_default_bucket_size(memory: M, bucket_size_in_pages: u16) -> Self {
        assert!(bucket_size_in_pages > 0, "bucket size must be positive");
        let inner = if memory.size() == 0 {
            assert_ne!(
                memory.grow(DATA_OFFSET_PAGES),
                -1,
                "failed to initialize memory manager"
            );
            Inner::new(memory, bucket_size_in_pages)
        } else {
            Inner::load(memory, bucket_size_in_pages)
        };
        Self {
            inner: Rc::new(RefCell::new(inner)),
        }
    }

    /// Initializes a manager and persists the policy for the listed memory IDs.
    pub fn init_with_policies(
        memory: M,
        default_bucket_size_in_pages: u16,
        policies: &[(MemoryId, u16)],
    ) -> Self {
        let manager = Self::init_with_default_bucket_size(memory, default_bucket_size_in_pages);
        let mut inner = manager.inner.borrow_mut();
        let mut changed = false;
        for &(id, bucket_size_in_pages) in policies {
            changed |= inner.ensure_policy(id, bucket_size_in_pages);
        }
        if changed {
            inner.save_header();
        }
        drop(inner);
        manager
    }

    /// Returns a virtual memory and records its policy if it has not been used before.
    pub fn get(&self, id: MemoryId) -> VirtualMemory<M> {
        let inner = self.inner.borrow();
        let bucket_size = inner
            .policy(id)
            .unwrap_or(inner.default_bucket_size_in_pages);
        drop(inner);
        self.get_with_bucket_size(id, bucket_size)
    }

    /// Returns a virtual memory with an explicit persisted bucket-size policy.
    pub fn get_with_bucket_size(
        &self,
        id: MemoryId,
        bucket_size_in_pages: u16,
    ) -> VirtualMemory<M> {
        assert!(bucket_size_in_pages > 0, "bucket size must be positive");
        let mut inner = self.inner.borrow_mut();
        if inner.ensure_policy(id, bucket_size_in_pages) {
            inner.save_policy(index(id));
        }
        drop(inner);
        VirtualMemory {
            memory_index: index(id),
            memory_manager: Rc::clone(&self.inner),
            cache: BucketCache::new(bucket_size_in_pages as u64 * WASM_PAGE_SIZE),
        }
    }

    pub fn into_memory(self) -> Option<M> {
        Rc::into_inner(self.inner).map(|inner| inner.into_inner().memory)
    }
}

#[derive(Clone)]
pub struct VirtualMemory<M: Memory> {
    memory_index: usize,
    memory_manager: Rc<RefCell<Inner<M>>>,
    cache: BucketCache,
}

impl<M: Memory> Memory for VirtualMemory<M> {
    fn size(&self) -> u64 {
        self.memory_manager.borrow().sizes[self.memory_index]
    }
    fn grow(&self, pages: u64) -> i64 {
        self.memory_manager
            .borrow_mut()
            .grow(self.memory_index, pages)
    }
    fn read(&self, offset: u64, dst: &mut [u8]) {
        self.memory_manager
            .borrow()
            .read(self.memory_index, offset, dst, &self.cache)
    }
    unsafe fn read_unsafe(&self, offset: u64, dst: *mut u8, count: usize) {
        unsafe {
            self.memory_manager.borrow().read_unsafe(
                self.memory_index,
                offset,
                dst,
                count,
                &self.cache,
            )
        }
    }
    fn write(&self, offset: u64, src: &[u8]) {
        self.memory_manager
            .borrow()
            .write(self.memory_index, offset, src, &self.cache)
    }
}

struct Inner<M: Memory> {
    memory: M,
    header: [u8; HEADER_SIZE_BYTES],
    owner_buffer: Vec<u8>,
    default_bucket_size_in_pages: u16,
    allocated_extents: u32,
    sizes: [u64; MAX_MEMORY_IDS],
    policies: [u16; MAX_MEMORY_IDS],
    next_address: u64,
    extents_by_memory: Vec<Vec<Extent>>,
}

#[derive(Clone, Copy)]
struct Extent {
    address: u64,
}

impl<M: Memory> Inner<M> {
    fn new(memory: M, default_bucket_size_in_pages: u16) -> Self {
        let mut header = [0; HEADER_SIZE_BYTES];
        header[0..3].copy_from_slice(&MAGIC);
        header[3] = LAYOUT_VERSION;
        write_u16(
            &mut header,
            DEFAULT_BUCKET_SIZE_OFFSET,
            default_bucket_size_in_pages,
        );
        let inner = Self {
            memory,
            header,
            owner_buffer: Vec::new(),
            default_bucket_size_in_pages,
            allocated_extents: 0,
            sizes: [0; MAX_MEMORY_IDS],
            policies: [0; MAX_MEMORY_IDS],
            extents_by_memory: empty_extent_index(),
            next_address: DATA_OFFSET_PAGES * WASM_PAGE_SIZE,
        };
        inner.save_header();
        inner
            .memory
            .write(EXTENT_OWNER_OFFSET, &[UNALLOCATED; MAX_EXTENTS as usize]);
        inner
    }

    fn load(memory: M, default_bucket_size_in_pages: u16) -> Self {
        let mut header = [0; HEADER_SIZE_BYTES];
        memory.read(0, &mut header);
        assert_eq!(
            &header[0..3],
            MAGIC.as_slice(),
            "bad variable memory manager magic"
        );
        assert_eq!(
            header[3], LAYOUT_VERSION,
            "unsupported variable memory manager layout"
        );
        let allocated_extents = read_u32(&header, ALLOCATED_EXTENTS_OFFSET);
        let persisted_default = read_u16(&header, DEFAULT_BUCKET_SIZE_OFFSET);
        let mut sizes = [0; MAX_MEMORY_IDS];
        let mut policies = [0; MAX_MEMORY_IDS];
        for (i, size) in sizes.iter_mut().enumerate() {
            *size = read_u64(&header, MEMORY_SIZES_OFFSET + i * 8);
        }
        for (i, policy) in policies.iter_mut().enumerate() {
            *policy = read_u16(&header, MEMORY_POLICIES_OFFSET + i * 2);
        }
        assert_eq!(
            persisted_default, default_bucket_size_in_pages,
            "default bucket policy changed"
        );
        assert!(
            (allocated_extents as u64) <= MAX_EXTENTS,
            "extent count exceeds manager limit"
        );
        let mut owners = vec![UNALLOCATED; allocated_extents as usize];
        memory.read(EXTENT_OWNER_OFFSET, &mut owners);
        let mut extents_by_memory = empty_extent_index();
        if owners.len() >= EXTENT_RESERVE_THRESHOLD {
            let mut extent_counts = [0usize; MAX_MEMORY_IDS];
            for &owner in &owners {
                let owner_index = owner as usize;
                assert!(
                    owner_index < MAX_MEMORY_IDS,
                    "allocated extent has an invalid owner"
                );
                let bucket = policies[owner_index];
                assert_ne!(bucket, 0, "extent owner has no bucket policy");
                extent_counts[owner_index] = extent_counts[owner_index]
                    .checked_add(1)
                    .expect("extent count overflow");
            }
            for (extents, &count) in extents_by_memory.iter_mut().zip(&extent_counts) {
                extents.reserve_exact(count);
            }
        }
        let mut address = DATA_OFFSET_PAGES * WASM_PAGE_SIZE;
        for owner in owners {
            let owner_index = owner as usize;
            assert!(
                owner_index < MAX_MEMORY_IDS,
                "allocated extent has an invalid owner"
            );
            let bucket = policies[owner_index];
            assert_ne!(bucket, 0, "extent owner has no bucket policy");
            let size_in_bytes = bucket as u64 * WASM_PAGE_SIZE;
            extents_by_memory[owner_index].push(Extent { address });
            address = address
                .checked_add(size_in_bytes)
                .expect("extent address overflow");
        }
        Self {
            memory,
            header,
            owner_buffer: Vec::new(),
            default_bucket_size_in_pages: persisted_default,
            allocated_extents,
            sizes,
            policies,
            extents_by_memory,
            next_address: address,
        }
    }

    fn ensure_policy(&mut self, id: MemoryId, bucket_size: u16) -> bool {
        let i = index(id);
        let slot = &mut self.policies[i];
        if *slot == 0 {
            *slot = bucket_size;
            write_u16(
                &mut self.header,
                MEMORY_POLICIES_OFFSET + i * 2,
                bucket_size,
            );
            true
        } else {
            assert_eq!(
                *slot, bucket_size,
                "bucket policy changed for an existing memory"
            );
            false
        }
    }

    fn policy(&self, id: MemoryId) -> Option<u16> {
        let policy = self.policies[index(id)];
        (policy != 0).then_some(policy)
    }

    fn grow(&mut self, i: usize, pages: u64) -> i64 {
        let old_size = self.sizes[i];
        let new_size = match old_size.checked_add(pages) {
            Some(value) => value,
            None => return -1,
        };
        let bucket_pages = self.policies[i] as u64;
        assert_ne!(bucket_pages, 0, "memory policy was not registered");
        let old_extents = old_size.div_ceil(bucket_pages);
        let new_extents = new_size.div_ceil(bucket_pages);
        let additional = new_extents - old_extents;
        let total_extents = self.allocated_extents as u64 + additional;
        if total_extents > MAX_EXTENTS {
            return -1;
        }
        let added_bytes = additional
            .checked_mul(bucket_pages)
            .and_then(|p| p.checked_mul(WASM_PAGE_SIZE));
        let end = self.next_address;
        let required_pages = match end
            .checked_add(added_bytes.unwrap_or(u64::MAX))
            .and_then(|b| b.div_ceil(WASM_PAGE_SIZE).checked_add(0))
        {
            Some(value) => value,
            None => return -1,
        };
        if required_pages > self.memory.size()
            && self.memory.grow(required_pages - self.memory.size()) == -1
        {
            return -1;
        }
        let mut address = end;
        let additional = additional as usize;
        if additional > 0 {
            self.owner_buffer.resize(additional, i as u8);
            self.owner_buffer.fill(i as u8);
            self.memory.write(
                EXTENT_OWNER_OFFSET + self.allocated_extents as u64,
                &self.owner_buffer,
            );
        }
        for _ in 0..additional {
            let size_in_bytes = bucket_pages * WASM_PAGE_SIZE;
            let extent = Extent { address };
            self.extents_by_memory[i].push(extent);
            self.allocated_extents += 1;
            address += size_in_bytes;
        }
        self.next_address = address;
        self.sizes[i] = new_size;
        write_u32(
            &mut self.header,
            ALLOCATED_EXTENTS_OFFSET,
            self.allocated_extents,
        );
        write_u64(&mut self.header, MEMORY_SIZES_OFFSET + i * 8, new_size);
        self.save_grow_header(i);
        old_size as i64
    }

    fn save_header(&self) {
        self.memory.write(0, &self.header);
    }

    fn save_grow_header(&self, memory_index: usize) {
        self.memory.write(
            ALLOCATED_EXTENTS_OFFSET as u64,
            &self.header[ALLOCATED_EXTENTS_OFFSET..ALLOCATED_EXTENTS_OFFSET + 4],
        );
        let size_offset = MEMORY_SIZES_OFFSET + memory_index * 8;
        self.memory.write(
            size_offset as u64,
            &self.header[size_offset..size_offset + 8],
        );
    }

    fn save_policy(&self, memory_index: usize) {
        let offset = MEMORY_POLICIES_OFFSET + memory_index * 2;
        self.memory
            .write(offset as u64, &self.header[offset..offset + 2]);
    }

    #[inline]
    fn read(&self, memory_index: usize, offset: u64, dst: &mut [u8], cache: &BucketCache) {
        unsafe {
            self.read_unsafe(memory_index, offset, dst.as_mut_ptr(), dst.len(), cache);
        }
    }

    #[inline]
    unsafe fn read_unsafe(
        &self,
        memory_index: usize,
        offset: u64,
        dst: *mut u8,
        count: usize,
        cache: &BucketCache,
    ) {
        if let Some(real_address) = cache.get(offset, count as u64) {
            unsafe { self.memory.read_unsafe(real_address, dst, count) };
            return;
        }

        self.for_each_segment(
            memory_index,
            offset,
            count,
            cache,
            |address, length, copied| {
                unsafe { self.memory.read_unsafe(address, dst.add(copied), length) };
            },
        );
    }

    #[inline]
    fn write(&self, memory_index: usize, offset: u64, src: &[u8], cache: &BucketCache) {
        if let Some(real_address) = cache.get(offset, src.len() as u64) {
            self.memory.write(real_address, src);
            return;
        }

        self.for_each_segment(
            memory_index,
            offset,
            src.len(),
            cache,
            |address, length, copied| {
                self.memory.write(address, &src[copied..copied + length]);
            },
        );
    }

    #[inline]
    fn for_each_segment(
        &self,
        memory_index: usize,
        offset: u64,
        count: usize,
        cache: &BucketCache,
        mut f: impl FnMut(u64, usize, usize),
    ) {
        let size = self.sizes[memory_index]
            .checked_mul(WASM_PAGE_SIZE)
            .expect("memory size overflow");
        let end = offset
            .checked_add(count as u64)
            .expect("memory access overflow");
        assert!(end <= size, "memory access out of bounds");
        if count == 0 {
            return;
        }
        let bucket = cache.bucket_size_in_bytes;
        let extents = &self.extents_by_memory[memory_index];
        let mut virtual_offset = offset;
        let mut copied = 0;
        while copied < count {
            let extent_index = (virtual_offset / bucket) as usize;
            let extent = &extents[extent_index];
            let within = virtual_offset % bucket;
            let length = (bucket - within).min((count - copied) as u64) as usize;
            let real_address = extent.address + within;
            cache.store(virtual_offset - within, bucket, extent.address);
            f(real_address, length, copied);
            copied += length;
            virtual_offset += length as u64;
        }
    }
}

#[derive(Clone)]
struct BucketCache {
    start: Cell<u64>,
    length: Cell<u64>,
    real: Cell<u64>,
    bucket_size_in_bytes: u64,
}
impl BucketCache {
    fn new(bucket_size_in_bytes: u64) -> Self {
        Self {
            start: Cell::new(0),
            length: Cell::new(0),
            real: Cell::new(0),
            bucket_size_in_bytes,
        }
    }
    #[inline]
    fn get(&self, start: u64, length: u64) -> Option<u64> {
        (start >= self.start.get()
            && start.checked_add(length)? <= self.start.get() + self.length.get())
        .then(|| self.real.get() + start - self.start.get())
    }
    #[inline]
    fn store(&self, start: u64, length: u64, real: u64) {
        self.start.set(start);
        self.length.set(length);
        self.real.set(real);
    }
}

fn index(id: MemoryId) -> usize {
    (0..MAX_MEMORY_IDS)
        .find(|i| MemoryId::new(*i as u8) == id)
        .expect("invalid memory id")
}

fn empty_extent_index() -> Vec<Vec<Extent>> {
    (0..MAX_MEMORY_IDS).map(|_| Vec::new()).collect()
}
fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::{DefaultMemoryImpl, Memory};
    use std::cell::Cell;

    fn manager() -> (MemoryManager<DefaultMemoryImpl>, DefaultMemoryImpl) {
        let memory = DefaultMemoryImpl::default();
        let manager = MemoryManager::init_with_default_bucket_size(memory.clone(), 4);
        (manager, memory)
    }

    #[test]
    fn memories_use_independent_bucket_sizes_and_reopen() {
        let (manager, memory) = manager();
        let small = manager.get_with_bucket_size(MemoryId::new(0), 1);
        let large = manager.get_with_bucket_size(MemoryId::new(1), 8);
        assert_eq!(small.grow(2), 0);
        assert_eq!(large.grow(1), 0);
        small.write(65_535, &[1, 2, 3]);
        large.write(0, &[4, 5, 6]);
        let reopened = MemoryManager::init_with_default_bucket_size(memory, 4);
        let small = reopened.get_with_bucket_size(MemoryId::new(0), 1);
        let large = reopened.get_with_bucket_size(MemoryId::new(1), 8);
        let mut bytes = [0; 3];
        small.read(65_535, &mut bytes);
        assert_eq!(bytes, [1, 2, 3]);
        large.read(0, &mut bytes);
        assert_eq!(bytes, [4, 5, 6]);
        assert_eq!(small.size(), 2);
        assert_eq!(large.size(), 1);
    }

    #[test]
    fn growing_one_memory_does_not_move_another() {
        let (manager, memory) = manager();
        let first = manager.get_with_bucket_size(MemoryId::new(0), 1);
        let second = manager.get_with_bucket_size(MemoryId::new(1), 8);
        first.grow(1);
        second.grow(1);
        second.write(0, &[7; 4]);
        first.grow(100);
        let mut bytes = [0; 4];
        second.read(0, &mut bytes);
        assert_eq!(bytes, [7; 4]);
        assert!(memory.size() >= 2 + 1 + 8);
    }

    #[test]
    fn extent_index_handles_interleaved_memories_after_reopen() {
        let (manager, memory) = manager();
        let first = manager.get_with_bucket_size(MemoryId::new(0), 1);
        let second = manager.get_with_bucket_size(MemoryId::new(1), 2);
        let third = manager.get_with_bucket_size(MemoryId::new(2), 4);

        first.grow(32);
        second.grow(16);
        third.grow(8);
        first.grow(32);
        second.grow(16);

        first.write(63 * WASM_PAGE_SIZE, &[1]);
        second.write(31 * WASM_PAGE_SIZE, &[2]);
        third.write(7 * WASM_PAGE_SIZE, &[3]);

        let reopened = MemoryManager::init_with_default_bucket_size(memory, 4);
        let first = reopened.get_with_bucket_size(MemoryId::new(0), 1);
        let second = reopened.get_with_bucket_size(MemoryId::new(1), 2);
        let third = reopened.get_with_bucket_size(MemoryId::new(2), 4);
        let mut byte = [0];

        first.read(63 * WASM_PAGE_SIZE, &mut byte);
        assert_eq!(byte, [1]);
        second.read(31 * WASM_PAGE_SIZE, &mut byte);
        assert_eq!(byte, [2]);
        third.read(7 * WASM_PAGE_SIZE, &mut byte);
        assert_eq!(byte, [3]);
    }

    #[test]
    fn large_extent_index_reopens_after_capacity_reservation() {
        let (manager, memory) = manager();
        let virtual_memory = manager.get_with_bucket_size(MemoryId::new(0), 1);
        assert_eq!(virtual_memory.grow(EXTENT_RESERVE_THRESHOLD as u64), 0);
        virtual_memory.write((EXTENT_RESERVE_THRESHOLD as u64 - 1) * WASM_PAGE_SIZE, &[7]);

        let reopened = MemoryManager::init_with_default_bucket_size(memory, 4);
        let virtual_memory = reopened.get_with_bucket_size(MemoryId::new(0), 1);
        let mut byte = [0];
        virtual_memory.read(
            (EXTENT_RESERVE_THRESHOLD as u64 - 1) * WASM_PAGE_SIZE,
            &mut byte,
        );
        assert_eq!(byte, [7]);
    }

    #[test]
    fn policy_change_is_rejected_on_reopen() {
        let (manager, memory) = manager();
        let _ = manager.get_with_bucket_size(MemoryId::new(0), 2);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = MemoryManager::init_with_default_bucket_size(memory, 5);
        }));
        assert!(result.is_err());
    }

    #[derive(Clone)]
    struct FailingMemory {
        inner: DefaultMemoryImpl,
        fail_grow: Rc<Cell<bool>>,
    }

    impl Memory for FailingMemory {
        fn size(&self) -> u64 {
            self.inner.size()
        }

        fn grow(&self, pages: u64) -> i64 {
            if self.fail_grow.get() {
                -1
            } else {
                self.inner.grow(pages)
            }
        }

        fn read(&self, offset: u64, dst: &mut [u8]) {
            self.inner.read(offset, dst)
        }

        fn write(&self, offset: u64, src: &[u8]) {
            self.inner.write(offset, src)
        }
    }

    #[test]
    fn failed_physical_grow_does_not_publish_an_extent() {
        let memory = FailingMemory {
            inner: DefaultMemoryImpl::default(),
            fail_grow: Rc::new(Cell::new(false)),
        };
        let fail_grow = Rc::clone(&memory.fail_grow);
        let manager = MemoryManager::init_with_default_bucket_size(memory.clone(), 1);
        let virtual_memory = manager.get_with_bucket_size(MemoryId::new(0), 1);

        fail_grow.set(true);
        assert_eq!(virtual_memory.grow(1), -1);
        assert_eq!(virtual_memory.size(), 0);
        assert_eq!(memory.size(), 2);

        fail_grow.set(false);
        assert_eq!(virtual_memory.grow(1), 0);
        assert_eq!(virtual_memory.size(), 1);
        assert_eq!(memory.size(), 3);
    }
}
