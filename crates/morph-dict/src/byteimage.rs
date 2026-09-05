//! ByteImage abstraction over MeCrab's `Arc<Mmap>` dictionary parameter.
//!
//! The vendored MeCrab dict layer (upstream @ 85444b5) reads dictionaries through
//! `Arc<memmap2::Mmap>` (contiguous `&[u8]` deref + raw-pointer accessors). This module
//! replaces that parameter with an offset-accessor trait so the SAME accessor bodies run
//! over (A) a heap copy, (B) a resident-prefix/lazy-suffix split, or (C) a
//! stable-memory-backed image supplied by the `ic-morph-dict` adapter.
//!
//! Spike-authored/landing-authored; upstream MeCrab has no equivalent (it is mmap-only).

use std::sync::Arc;

/// Byte-image, offset-accessor parameter for the dictionary layer.
///
/// All accessors fetch bytes exclusively through `read_exact_at`; no contiguous slice of
/// the image is ever assumed.
pub trait ByteImage: Send + Sync {
    /// Total image size in bytes.
    fn len(&self) -> u64;

    /// Read exactly `buf.len()` bytes at `offset`.
    ///
    /// # Panics
    /// Panics if `offset + buf.len()` exceeds [`ByteImage::len`] (dictionary accessors
    /// validate offsets at load time; a panic here means an accessor bug).
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]);

    /// Contiguous view of the image when one exists (heap-resident images). Accessors
    /// use this fast path so a RESIDENT image pays no per-access copy; lazy images
    /// return `None` and keep paying `read_exact_at` (the desired stable-lazy behavior).
    fn as_contiguous(&self) -> Option<&[u8]> {
        None
    }
}

// ── HeapImage ────────────────────────────────────────────────────────────────────────

/// One heap copy at load, zero-copy accessors thereafter.
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
    fn as_contiguous(&self) -> Option<&[u8]> {
        Some(&self.0)
    }

    #[inline]
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) {
        let off = offset as usize;
        let end = off + buf.len();
        assert!(end <= self.0.len(), "HeapImage OOB read {off}..{end}");
        buf.copy_from_slice(&self.0[off..end]);
    }
}

// ── OffsetImage ──────────────────────────────────────────────────────────────────────

/// A fixed-offset view over another image: reads at `o` go to `inner.read_exact_at(base + o)`.
/// Used to address one container entry (e.g. sys.dic inside the container) as a
/// standalone image without copying.
pub struct OffsetImage {
    inner: Arc<dyn ByteImage>,
    base: u64,
    len: u64,
}

impl OffsetImage {
    pub fn new(inner: Arc<dyn ByteImage>, base: u64, len: u64) -> Self {
        Self { inner, base, len }
    }
}

impl std::fmt::Debug for OffsetImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OffsetImage")
            .field("base", &self.base)
            .field("len", &self.len)
            .finish()
    }
}

impl ByteImage for OffsetImage {
    #[inline]
    fn len(&self) -> u64 {
        self.len
    }

    #[inline]
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) {
        assert!(
            offset + buf.len() as u64 <= self.len,
            "OffsetImage OOB read {offset}..{} (len {})",
            offset + buf.len() as u64,
            self.len
        );
        self.inner.read_exact_at(self.base + offset, buf);
    }
}

// ── SplitImage (resident prefix + lazy suffix) ───────────────────────────────────────

/// Access statistics recorded over a [`SplitImage`], classified by region boundaries.
/// The classification is caller-defined (the sys.dic residency split: trie region,
/// word-params region, feature region) and drives the measurement-driven residency
/// decisions; counts are advisory (never consulted by accessors).
#[derive(Default, Clone, Debug)]
pub struct SplitStats {
    /// `read_exact_at` calls landing in the resident prefix.
    pub prefix_calls: u64,
    /// Read calls landing in the lazy suffix.
    pub suffix_calls: u64,
    /// Distinct 4 KiB pages of the SUFFIX touched (the stable-lazy page cost driver).
    pub suffix_unique_pages: u64,
    /// Bytes copied out of the suffix.
    pub suffix_bytes: u64,
}

/// Splits one logical image into a RESIDENT PREFIX (copied to the heap at open; the
/// random 2–16 B hot path, syscall-hostile) and a LAZY SUFFIX (reads forwarded to the
/// backing image — e.g. the stable-memory container — on the output path only).
///
/// For sys.dic: prefix = header + double-array trie + token array (word params);
/// suffix = the feature-string region. The boundary is `feature_offset`, computed from
/// the header at load.
pub struct SplitImage {
    prefix: Arc<dyn ByteImage>,
    /// Suffix image covering the FULL image address space (offsets are absolute).
    suffix: Arc<dyn ByteImage>,
    /// Boundary in image coordinates: `[0, boundary)` → prefix, `[boundary, len)` → suffix.
    boundary: u64,
    stats: std::sync::Mutex<SplitStats>,
    touched: std::sync::Mutex<std::collections::HashSet<u64>>,
}

impl SplitImage {
    /// `prefix` covers image offsets `[0, boundary)`; `suffix` must cover the WHOLE
    /// image (absolute offsets), e.g. an [`OffsetImage`] over the container entry.
    pub fn new(
        prefix: Arc<dyn ByteImage>,
        suffix: Arc<dyn ByteImage>,
        boundary: u64,
    ) -> Self {
        Self {
            prefix,
            suffix,
            boundary,
            stats: std::sync::Mutex::new(SplitStats::default()),
            touched: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub fn stats(&self) -> SplitStats {
        self.stats.lock().unwrap().clone()
    }

    /// Split boundary in image coordinates.
    pub fn boundary(&self) -> u64 {
        self.boundary
    }
}

impl ByteImage for SplitImage {
    #[inline]
    fn len(&self) -> u64 {
        self.suffix.len()
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) {
        let end = offset + buf.len() as u64;
        assert!(end <= self.len(), "SplitImage OOB read {offset}..{end}");
        let mut stats = self.stats.lock().unwrap();
        if end <= self.boundary {
            stats.prefix_calls += 1;
            drop(stats);
            self.prefix.read_exact_at(offset, buf);
        } else if offset >= self.boundary {
            stats.suffix_calls += 1;
            stats.suffix_bytes += buf.len() as u64;
            const PAGE: u64 = 4096;
            let first = offset / PAGE;
            let last = (end - 1) / PAGE;
            let mut t = self.touched.lock().unwrap();
            for p in first..=last {
                t.insert(p);
            }
            stats.suffix_unique_pages = t.len() as u64;
            drop(stats);
            drop(t);
            self.suffix.read_exact_at(offset, buf);
        } else {
            // Straddling read: split across prefix and suffix at the boundary.
            let head = (self.boundary - offset) as usize;
            stats.prefix_calls += 1;
            stats.suffix_calls += 1;
            stats.suffix_bytes += (buf.len() - head) as u64;
            drop(stats);
            self.prefix.read_exact_at(offset, &mut buf[..head]);
            self.suffix.read_exact_at(offset + head as u64, &mut buf[head..]);
        }
    }
}

// ── Instrumented wrapper (per-structure accounting) ─────────────────────────────────

/// Counts every read crossing this wrapper (used by the residency measurement tests).
pub struct CountingImage {
    inner: Arc<dyn ByteImage>,
    stats: std::sync::Mutex<AccessStats>,
    touched: std::sync::Mutex<std::collections::HashSet<u64>>,
}

/// Aggregate read statistics (calls, bytes, distinct 4 KiB pages touched).
#[derive(Default, Clone, Debug)]
pub struct AccessStats {
    /// Number of `read_exact_at` calls.
    pub read_calls: u64,
    /// Total bytes copied out of the image.
    pub bytes_read: u64,
    /// Distinct 4 KiB pages EVER touched (hot-set footprint).
    pub unique_pages: u64,
}

impl CountingImage {
    pub fn new(inner: Arc<dyn ByteImage>) -> Self {
        Self {
            inner,
            stats: std::sync::Mutex::new(AccessStats::default()),
            touched: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub fn stats(&self) -> AccessStats {
        self.stats.lock().unwrap().clone()
    }
}

impl ByteImage for CountingImage {
    #[inline]
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) {
        let end = offset + buf.len() as u64;
        let mut stats = self.stats.lock().unwrap();
        stats.read_calls += 1;
        stats.bytes_read += buf.len() as u64;
        const PAGE: u64 = 4096;
        let mut t = self.touched.lock().unwrap();
        for p in offset / PAGE..=(end.saturating_sub(1)) / PAGE {
            t.insert(p);
        }
        stats.unique_pages = t.len() as u64;
        drop(stats);
        drop(t);
        self.inner.read_exact_at(offset, buf);
    }
}