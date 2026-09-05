//! Plan 0333 spike — ByteImage abstraction over MeCrab's `Arc<Mmap>` dictionary
//! parameter.
//!
//! The vendored MeCrab dict layer (upstream @ 85444b5) reads dictionaries through
//! `Arc<memmap2::Mmap>` (contiguous `&[u8]` deref + raw-pointer accessors). This module
//! replaces that parameter with an offset-accessor trait so the SAME accessor bodies run
//! over (A) a heap copy and (B) a simulated stable-memory image with an LRU page cache
//! that records access statistics (the Tier-C projection of `stable64_read` paging).
//!
//! Spike-authored; upstream MeCrab has no equivalent (it is mmap-only).

/// Byte-image, offset-accessor parameter for the vendored dictionary layer.
///
/// All vendored accessors fetch bytes exclusively through `read_exact_at`; no contiguous
/// slice of the image is ever assumed (the redesign points upstream required: the raw
/// `*const Token` / `*const Unit` / `*const i16` pointers and the pointer-walking NUL
/// scan in `SysDic::get_feature`).
pub trait ByteImage: Send + Sync {
    /// Total image size in bytes.
    fn len(&self) -> u64;

    /// Read exactly `buf.len()` bytes at `offset`.
    ///
    /// # Panics
    /// Panics if `offset + buf.len()` exceeds [`ByteImage::len`] (dict accessors validate
    /// offsets at load time; a panic here means a vendored-accessor bug).
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]);
}

// ── Impl A: heap copy ────────────────────────────────────────────────────────────────

/// Tier-A parameter: one heap copy at load, zero-copy accessors thereafter. This is the
/// closest analogue of the upstream `Mmap` (minus OS paging) and the shape the CURRENT
/// landed analyzer effectively uses after materialization.
pub struct HeapImage(Box<[u8]>);

impl HeapImage {
    pub fn from_vec(v: Vec<u8>) -> Self {
        Self(v.into_boxed_slice())
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl ByteImage for HeapImage {
    #[inline]
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    #[inline]
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) {
        let off = offset as usize;
        let end = off + buf.len();
        assert!(end <= self.0.len(), "HeapImage OOB read {off}..{end}");
        buf.copy_from_slice(&self.0[off..end]);
    }
}

// ── Impl B: simulated stable-memory image with LRU page cache ────────────────────────

/// Page size of the simulated stable image (64 KiB frames, per plan 0333).
const FRAME_SIZE: usize = 64 * 1024;

/// Access statistics recorded per [`SimulatedStableImage`].
#[derive(Default, Clone, Debug)]
pub struct AccessStats {
    /// Number of `read_exact_at` calls (simulated `stable64_read` calls).
    pub read_calls: u64,
    /// Total bytes copied out of the image.
    pub bytes_read: u64,
    /// Bytes read that hit a cached frame.
    pub cache_hit_calls: u64,
    /// Unique 64 KiB frames EVER touched (hot set in frames; ×64 KiB = hot-set bytes).
    pub unique_frames: u64,
    /// Frame loads (misses that required a frame materialization into the cache).
    pub frame_loads: u64,
}

impl AccessStats {
    /// Hot-set size in bytes (unique frames ever touched × 64 KiB).
    pub fn hot_set_bytes(&self) -> u64 {
        self.unique_frames * FRAME_SIZE as u64
    }

    /// Hit rate over all read calls.
    pub fn hit_rate(&self) -> f64 {
        if self.read_calls == 0 {
            0.0
        } else {
            self.cache_hit_calls as f64 / self.read_calls as f64
        }
    }
}

/// Tier-B/C parameter: backing is a set of independent 64 KiB frames (never materialized
/// contiguously); a parameterized LRU page cache (byte budget) sits in front. Every
/// accessor read goes through the cache exactly as a per-access `stable64_read` reader
/// would on the canister.
pub struct SimulatedStableImage {
    /// Backing frames (64 KiB each; the last frame is zero-padded internally while
    /// `raw_len` preserves the true image size).
    frames: Vec<Box<[u8]>>,
    n_frames: u64,
    raw_len: u64,
    cache: std::sync::Mutex<LruCache>,
    stats: std::sync::Mutex<AccessStats>,
    /// Monotonic set of frames ever touched (hot-set accounting).
    unique: std::sync::Mutex<std::collections::HashSet<u64>>,
}

struct LruCache {
    /// cache_key = frame index.
    map: std::collections::HashMap<u64, std::sync::Arc<[u8]>>,
    /// Recency order (front = most recent).
    order: std::collections::VecDeque<u64>,
    /// Maximum cached bytes.
    budget: usize,
    used: usize,
}

impl LruCache {
    fn new(budget: usize) -> Self {
        Self {
            map: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            budget,
            used: 0,
        }
    }

    fn get(&mut self, frame: u64) -> Option<std::sync::Arc<[u8]>> {
        if self.map.contains_key(&frame) {
            self.order.retain(|&f| f != frame);
            self.order.push_front(frame);
            self.map.get(&frame).cloned()
        } else {
            None
        }
    }

    fn put(&mut self, frame: u64, data: std::sync::Arc<[u8]>) {
        const FRAME_SIZE: usize = 64 * 1024;
        if self.budget < FRAME_SIZE {
            return; // degenerate: zero-capacity cache
        }
        while self.used + FRAME_SIZE > self.budget {
            let evict = self.order.pop_back();
            match evict {
                Some(f) => {
                    self.map.remove(&f);
                    self.used = self.used.saturating_sub(FRAME_SIZE);
                }
                None => break,
            }
        }
        if self.map.insert(frame, data).is_none() {
            self.used += FRAME_SIZE;
            self.order.push_front(frame);
        }
    }
}

impl SimulatedStableImage {
    /// Build from raw image bytes. The bytes are held as 64 KiB independent frames; a
    /// contiguous view is never exposed.
    pub fn new(bytes: Vec<u8>, cache_budget_bytes: usize) -> Self {
        let n = bytes.len().div_ceil(FRAME_SIZE).max(1);
        let mut frames = Vec::with_capacity(n);
        for i in 0..n {
            let start = i * FRAME_SIZE;
            let end = (start + FRAME_SIZE).min(bytes.len());
            let mut f = bytes[start..end].to_vec();
            f.resize(FRAME_SIZE, 0);
            frames.push(f.into_boxed_slice());
        }
        Self {
            frames,
            n_frames: n as u64,
            raw_len: bytes.len() as u64,
            cache: std::sync::Mutex::new(LruCache::new(cache_budget_bytes)),
            stats: std::sync::Mutex::new(AccessStats::default()),
            unique: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub fn stats(&self) -> AccessStats {
        self.stats.lock().unwrap().clone()
    }

    pub fn frame_count(&self) -> u64 {
        self.n_frames
    }
}

impl ByteImage for SimulatedStableImage {
    #[inline]
    fn len(&self) -> u64 {
        self.raw_len
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) {
        let mut copied = 0usize;
        let mut stats = self.stats.lock().unwrap();
        stats.read_calls += 1;
        stats.bytes_read += buf.len() as u64;
        let mut hit = true;

        while copied < buf.len() {
            let pos = offset as usize + copied;
            let frame = (pos / FRAME_SIZE) as u64;
            let intra = pos % FRAME_SIZE;
            let take = (FRAME_SIZE - intra).min(buf.len() - copied);

            let data = {
                let mut cache = self.cache.lock().unwrap();
                match cache.get(frame) {
                    Some(d) => d,
                    None => {
                        // frame fault: materialize the frame (simulated stable64_read)
                        stats.frame_loads += 1;
                        hit = false;
                        let arc: std::sync::Arc<[u8]> =
                            std::sync::Arc::from(self.frames[frame as usize].as_ref());
                        cache.put(frame, std::sync::Arc::clone(&arc));
                        arc
                    }
                }
            };
            // unique-frames bookkeeping (monotonic hot-set set)
            self.unique.lock().unwrap().insert(frame);
            buf[copied..copied + take].copy_from_slice(&data[intra..intra + take]);
            copied += take;
        }
        if hit {
            stats.cache_hit_calls += 1;
        }
        stats.unique_frames = self.unique.lock().unwrap().len() as u64;
    }
}