//! ic-morph-dict — thin Internet Computer adapter for the morph-dict byte-image
//! dictionary layer.
//!
//! Provides `ByteImage` implementations over IC stable memory so a morph-dict container
//! can live in one stable region and be read with offset accessors instead of being
//! decoded/materialized at open. Everything here is generic over the
//! `ic_stable_structures::Memory` trait (production binds `DefaultMemoryImpl`; tests can
//! bind any in-memory backend), so the crate itself never touches `ic0` directly and
//! stays testable — a `mem.read(pos, buf)` call IS one batched `stable64_read` syscall
//! on the IC regardless of buffer size, which is why reads are not split further here.
//!
//! # Placement notes (IC cost model)
//!
//! DMT charges 5,000 instructions per 4 KiB page touched, uniform for heap and stable
//! within a message, plus 20 instructions per stable-read syscall. Placement therefore
//! changes OPEN cost, steady heap, and syscall overhead — not per-message page charges.
//! The default residency policy (see morph-dict `Analyzer::open`) copies the hot
//! random-access regions to the heap at open and leaves the feature region lazy over
//! [`StableImage`], where the (rare) output-path reads pay one syscall + the same page
//! charges they would pay from the heap.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use ic_stable_structures::Memory;
use morph_dict::byteimage::ByteImage;

/// A stable-memory-backed `ByteImage` over any `Memory` (one region).
///
/// `len` is pinned at construction (the recorded upload length), not the region's
/// allocated size — validation reads never see beyond the recorded blob.
pub struct StableImage<M: Memory> {
    mem: M,
    len: u64,
}

impl<M: Memory> StableImage<M> {
    /// Pins `len` bytes of `mem` (fail-closed: the region must be at least this large).
    pub fn new(mem: M, len: u64) -> Self {
        // `Memory::size()` counts 64 KiB pages.
        assert!(
            mem.size() * 65536 >= len,
            "stable region smaller than the pinned dictionary length: {} pages < {len} bytes",
            mem.size()
        );
        Self { mem, len }
    }

    pub fn mem(&self) -> &M {
        &self.mem
    }
}

impl<M: Memory> ByteImage for StableImage<M>
where
    M: Send + Sync,
{
    #[inline]
    fn len(&self) -> u64 {
        self.len
    }

    /// One `mem.read` call = one batched `stable64_read` syscall. Callers that copy a
    /// resident set should issue large contiguous reads (the morph-dict open path does
    /// exactly that: one read per resident region).
    #[inline]
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) {
        assert!(
            offset + buf.len() as u64 <= self.len,
            "StableImage OOB read {offset}..{} (len {})",
            offset + buf.len() as u64,
            self.len
        );
        if buf.is_empty() {
            return;
        }
        self.mem.read(offset, buf);
    }
}

/// `Send`/`Sync`-by-fiat wrapper over a [`StableImage`] for canister use.
///
/// # Safety (Send/Sync)
/// IC canister code executes on a single thread; the image is created and used from the
/// canister's execution thread only and never crosses threads. The underlying `Memory`
/// impl may be `Rc`-based (the `MemoryManager` pattern) without violating anything.
pub struct CanisterStableImage<M: Memory> {
    inner: StableImage<M>,
}

impl<M: Memory> CanisterStableImage<M> {
    pub fn new(mem: M, len: u64) -> Self {
        Self {
            inner: StableImage::new(mem, len),
        }
    }

    fn read_direct(&self, offset: u64, buf: &mut [u8]) {
        assert!(
            offset + buf.len() as u64 <= self.inner.len,
            "CanisterStableImage OOB read"
        );
        if buf.is_empty() {
            return;
        }
        self.inner.mem.read(offset, buf);
    }
}

// Safety: see the type docs — single-threaded canister execution.
unsafe impl<M: Memory> Send for CanisterStableImage<M> {}
unsafe impl<M: Memory> Sync for CanisterStableImage<M> {}

impl<M: Memory> ByteImage for CanisterStableImage<M> {
    #[inline]
    fn len(&self) -> u64 {
        self.inner.len
    }

    #[inline]
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) {
        self.read_direct(offset, buf);
    }
}

/// Optional 64 KiB-frame LRU cache in front of a [`StableImage`]. Only for paths the
/// residency policy keeps lazy with enough temporal locality to amortize the frame
/// copies; the hot random-access regions are resident at open instead.
pub struct CachedStableImage<M: Memory> {
    inner: StableImage<M>,
    cache: Mutex<Lru>,
    touched: Mutex<HashSet<u64>>,
    stats: Mutex<CacheStats>,
}

/// Cache counters (advisory; never consulted by accessors).
#[derive(Default, Clone, Debug)]
pub struct CacheStats {
    pub read_calls: u64,
    pub cache_hits: u64,
    pub frame_loads: u64,
    pub unique_frames: u64,
}

const FRAME: usize = 64 * 1024;

struct Lru {
    map: HashMap<u64, Arc<[u8]>>,
    order: VecDeque<u64>,
    budget_frames: usize,
}

impl<M: Memory> CachedStableImage<M> {
    /// `cache_budget_bytes` is rounded down to whole 64 KiB frames; 0 disables caching
    /// (pure pass-through).
    pub fn new(inner: StableImage<M>, cache_budget_bytes: usize) -> Self {
        Self {
            inner,
            cache: Mutex::new(Lru {
                map: HashMap::new(),
                order: VecDeque::new(),
                budget_frames: cache_budget_bytes / FRAME,
            }),
            touched: Mutex::new(HashSet::new()),
            stats: Mutex::new(CacheStats::default()),
        }
    }

    pub fn stats(&self) -> CacheStats {
        self.stats.lock().unwrap().clone()
    }
}

impl<M: Memory> ByteImage for CachedStableImage<M>
where
    M: Send + Sync,
{
    #[inline]
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) {
        assert!(
            offset + buf.len() as u64 <= self.inner.len(),
            "CachedStableImage OOB read"
        );
        if buf.is_empty() {
            return;
        }
        let mut stats = self.stats.lock().unwrap();
        stats.read_calls += 1;
        let mut copied = 0usize;
        let mut miss = false;
        while copied < buf.len() {
            let pos = offset as usize + copied;
            let frame = (pos / FRAME) as u64;
            let intra = pos % FRAME;
            let take = (FRAME - intra).min(buf.len() - copied);
            let data = {
                let mut cache = self.cache.lock().unwrap();
                if let Some(d) = cache.map.get(&frame) {
                    stats.cache_hits += 1;
                    d.clone()
                } else {
                    miss = true;
                    stats.frame_loads += 1;
                    let mut f = vec![0u8; FRAME];
                    let fstart = frame * FRAME as u64;
                    let flen = (FRAME as u64).min(self.inner.len().saturating_sub(fstart)) as usize;
                    self.inner.read_exact_at(fstart, &mut f[..flen]);
                    let arc: Arc<[u8]> = Arc::from(f.into_boxed_slice());
                    if cache.budget_frames > 0 {
                        while cache.map.len() >= cache.budget_frames {
                            if let Some(evict) = cache.order.pop_back() {
                                cache.map.remove(&evict);
                            } else {
                                break;
                            }
                        }
                        cache.map.insert(frame, Arc::clone(&arc));
                        cache.order.push_front(frame);
                    }
                    arc
                }
            };
            buf[copied..copied + take].copy_from_slice(&data[intra..intra + take]);
            copied += take;
        }
        if miss {
            let mut t = self.touched.lock().unwrap();
            let first = offset / FRAME as u64;
            let last = (offset + buf.len() as u64 - 1) / FRAME as u64;
            for p in first..=last {
                t.insert(p);
            }
            stats.unique_frames = t.len() as u64;
        }
    }
}
