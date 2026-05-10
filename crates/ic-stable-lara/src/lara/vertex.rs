//! Stable LARA vertex column.
//!
//! The default row stores a direct edge base, live degree, owned span capacity,
//! and per-segment log head (`-1` when the whole neighborhood is on the slab).
//! `base_slot_start` and `degree` are the only fields required by clean scans.
//! `capacity` is update-side ownership metadata: inserts and relocation use it
//! to determine whether the current slab span can absorb more edges.
//!
//! The default row invariant is:
//!
//! ```text
//! degree <= capacity
//! [base_slot_start, base_slot_start + degree)
//!     is contained in
//! [base_slot_start, base_slot_start + capacity)
//! ```
//!
//! # V1 layout
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! Magic "LVX"                           ↕ 3 bytes
//! --------------------------------------------------
//! Layout version                        ↕ 1 byte
//! --------------------------------------------------
//! Number of vertices                    ↕ 4 bytes
//! --------------------------------------------------
//! Vertex row stride                     ↕ 4 bytes
//! --------------------------------------------------
//! Reserved                              ↕ 52 bytes
//! -------------------------------------------------- <- Address 64
//! V_0                                   ↕ V::BYTES bytes
//! --------------------------------------------------
//! V_1                                   ↕ V::BYTES bytes
//! --------------------------------------------------
//! ...
//! --------------------------------------------------
//! V_(len-1)                             ↕ V::BYTES bytes
//! --------------------------------------------------
//! Unallocated space
//! ```

use crate::{
    GrowFailed, VertexId, read_u32, safe_write,
    traits::{CsrVertex, CsrVertexTombstone},
    types::Address,
    write_u32,
};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use std::{borrow::Cow, fmt};

/// Magic bytes that identify a LARA vertex-column memory.
pub const MAGIC: [u8; 3] = *b"LVX";
const LAYOUT_VERSION: u8 = 1;
const DATA_OFFSET: u64 = 64;
const LEN_OFFSET: u64 = 4;
const STRIDE_OFFSET: u64 = 8;
/// Stack buffer width for [`VertexStore::get`] when `V::BYTES` is small enough.
const INLINE_VERTEX_ROW_BYTES: usize = 64;

#[derive(Debug)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u32,
    stride: u32,
}

/// Errors returned when reopening a persisted [`VertexStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The memory header does not contain the LARA vertex magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The stored layout version is not supported by this crate version.
    IncompatibleVersion(u8),
    /// The persisted row width does not match the vertex type `V`.
    StrideMismatch {
        /// Expected row width for `V`.
        expected: u32,
        /// Row width read from stable memory.
        actual: u32,
    },
    /// The vertex type does not use a fixed-width [`Storable`] encoding.
    VariableWidthVertex,
    /// The store could not allocate its header while initializing empty memory.
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(f, "bad vertex magic {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(v) => write!(f, "unsupported vertex layout version {v}"),
            Self::StrideMismatch { expected, actual } => {
                write!(
                    f,
                    "vertex stride mismatch: expected {expected}, got {actual}"
                )
            }
            Self::VariableWidthVertex => {
                write!(f, "LARA vertices must use fixed-width Storable encoding")
            }
            Self::OutOfMemory => write!(f, "failed to allocate vertex metadata"),
        }
    }
}

impl std::error::Error for InitError {}

/// Default fixed-width LARA vertex row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vertex {
    /// First edge slot in this vertex's clean slab prefix.
    pub base_slot_start: u64,
    /// Number of live outgoing edges visible through clean scans.
    pub degree: u32,
    /// Number of slab slots owned by this vertex's current span.
    pub capacity: u32,
    /// Head entry in the per-segment overflow log, or `-1` when no log is present.
    pub log_head: i32,
    /// Logical deletion marker. Deleted vertex ids are never reused.
    pub deleted: bool,
}

impl CsrVertex for Vertex {
    const BYTES: usize = 24;

    fn base_slot_start(&self) -> u64 {
        self.base_slot_start
    }
    fn degree(&self) -> u32 {
        self.degree
    }
    fn with_base_slot_start(mut self, start: u64) -> Self {
        self.base_slot_start = start;
        self
    }
    fn with_degree(mut self, degree: u32) -> Self {
        self.degree = degree;
        self
    }
    fn log_head(self) -> i32 {
        self.log_head
    }
    fn with_log_head(mut self, idx: i32) -> Self {
        self.log_head = idx;
        self
    }

    fn span_capacity(&self) -> u32 {
        self.capacity
    }

    fn with_span_capacity(mut self, capacity: u32) -> Self {
        self.capacity = capacity;
        self
    }
}

impl CsrVertexTombstone for Vertex {
    fn is_tombstone(&self) -> bool {
        self.deleted
    }

    fn with_tombstone(mut self, tomb: bool) -> Self {
        self.deleted = tomb;
        self
    }
}

fn vertex_row_bytes(v: &Vertex) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[0..8].copy_from_slice(&v.base_slot_start.to_le_bytes());
    b[8..12].copy_from_slice(&v.degree.to_le_bytes());
    b[12..16].copy_from_slice(&v.capacity.to_le_bytes());
    b[16..20].copy_from_slice(&v.log_head.to_le_bytes());
    b[20] = u8::from(v.deleted);
    b
}

impl Storable for Vertex {
    const BOUND: Bound = Bound::Bounded {
        max_size: 24,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(vertex_row_bytes(self)))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(vertex_row_bytes(&self))
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let b = bytes.as_ref();
        let mut u = [0u8; 8];
        let mut d = [0u8; 4];
        let mut c = [0u8; 4];
        let mut l = [0u8; 4];
        u.copy_from_slice(&b[0..8]);
        d.copy_from_slice(&b[8..12]);
        c.copy_from_slice(&b[12..16]);
        l.copy_from_slice(&b[16..20]);
        Self {
            base_slot_start: u64::from_le_bytes(u),
            degree: u32::from_le_bytes(d),
            capacity: u32::from_le_bytes(c),
            log_head: i32::from_le_bytes(l),
            deleted: b.get(20).copied().unwrap_or(0) != 0,
        }
    }
}

/// Stable vector storing fixed-width LARA vertex rows.
#[derive(Clone, Debug)]
pub struct VertexStore<V: CsrVertex, M: Memory> {
    memory: M,
    _marker: std::marker::PhantomData<V>,
}

impl<V: CsrVertex, M: Memory> VertexStore<V, M> {
    /// Creates a fresh vertex store, overwriting any existing contents of `memory`.
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        verify_vertex_width::<V>().expect("LARA vertices must be fixed-width");
        let header = HeaderV1 {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            len: 0,
            stride: V::BYTES as u32,
        };
        Self::write_header(&header, &memory)?;
        Ok(Self {
            memory,
            _marker: std::marker::PhantomData,
        })
    }

    /// Reopens an existing vertex store, or creates one if `memory` is empty.
    pub fn init(memory: M) -> Result<Self, InitError> {
        verify_vertex_width::<V>()?;
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let header = Self::read_header(&memory);
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        let expected_stride = V::BYTES as u32;
        if header.stride != expected_stride {
            return Err(InitError::StrideMismatch {
                expected: expected_stride,
                actual: header.stride,
            });
        }
        Ok(Self {
            memory,
            _marker: std::marker::PhantomData,
        })
    }

    /// Returns the number of vertex rows in the store.
    pub fn len(&self) -> u32 {
        read_u32(&self.memory, Address::from(LEN_OFFSET))
    }
    /// Returns `true` when the store contains no vertex rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Consumes the store and returns the underlying stable memory.
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Reads the vertex row for `id`.
    ///
    /// Panics if `id >= self.len()`.
    pub fn get(&self, id: VertexId) -> V {
        let index = u64::from(id);
        assert!(index < u64::from(self.len()));
        if V::BYTES <= INLINE_VERTEX_ROW_BYTES {
            let mut buf = [0u8; INLINE_VERTEX_ROW_BYTES];
            self.memory
                .read(self.entry_offset(index), &mut buf[..V::BYTES]);
            V::from_bytes(Cow::Borrowed(&buf[..V::BYTES]))
        } else {
            let mut buf = vec![0u8; V::BYTES];
            self.memory.read(self.entry_offset(index), &mut buf);
            V::from_bytes(Cow::Owned(buf))
        }
    }

    /// Replaces the vertex row for `id`.
    ///
    /// Panics if `id >= self.len()`.
    pub fn set(&self, id: VertexId, item: &V) {
        let index = u64::from(id);
        assert!(index < u64::from(self.len()));
        crate::write(
            &self.memory,
            self.entry_offset(index),
            &item.to_bytes_checked(),
        );
    }

    /// Appends a vertex row and grows stable memory if necessary.
    pub fn push(&self, item: V) -> Result<(), GrowFailed> {
        let len = self.len();
        let new_len = len
            .checked_add(1)
            .expect("vertex store length exceeds u32::MAX");
        safe_write(
            &self.memory,
            self.entry_offset(u64::from(len)),
            &item.to_bytes_checked(),
        )?;
        write_u32(&self.memory, Address::from(LEN_OFFSET), new_len);
        Ok(())
    }

    fn entry_offset(&self, index: u64) -> u64 {
        DATA_OFFSET + V::BYTES as u64 * index
    }

    fn write_header(header: &HeaderV1, memory: &M) -> Result<(), GrowFailed> {
        safe_write(memory, 0, &header.magic)?;
        memory.write(3, &[header.version]);
        write_u32(memory, Address::from(LEN_OFFSET), header.len);
        write_u32(memory, Address::from(STRIDE_OFFSET), header.stride);
        Ok(())
    }

    fn read_header(memory: &M) -> HeaderV1 {
        debug_assert!(memory.size() > 0);

        let mut magic = [0u8; 3];
        let mut version = [0u8; 1];
        memory.read(0, &mut magic);
        memory.read(3, &mut version);
        let len = read_u32(memory, Address::from(LEN_OFFSET));
        let stride = read_u32(memory, Address::from(STRIDE_OFFSET));

        HeaderV1 {
            magic,
            version: version[0],
            len,
            stride,
        }
    }
}

fn verify_vertex_width<V: CsrVertex>() -> Result<(), InitError> {
    match V::BOUND {
        Bound::Bounded {
            max_size,
            is_fixed_size: true,
        } if max_size as usize == V::BYTES => Ok(()),
        _ => Err(InitError::VariableWidthVertex),
    }
}

#[cfg(feature = "canbench")]
mod bench {
    use std::hint::black_box;

    use canbench_rs::bench;

    use super::{Vertex, VertexStore};
    use crate::{VertexId, bench as helper, traits::CsrVertex};

    fn populate_store(n: u64) -> VertexStore<Vertex, helper::BenchMemory> {
        let mut memories = helper::BenchMemoryFactory::new();
        let store = VertexStore::new(memories.memory()).expect("vertex store");
        for i in 0..n {
            store
                .push(Vertex {
                    base_slot_start: i * 4,
                    degree: (i % 8) as u32,
                    capacity: 8,
                    log_head: -1,
                    deleted: false,
                })
                .expect("push vertex");
        }
        store
    }

    /// Measures appending vertex rows to the stable vertex column. This guards
    /// the fixed-width row write path and length-header update cost.
    #[bench(raw)]
    fn bench_lara_vertex_push_1024() -> canbench_rs::BenchResult {
        let mut memories = helper::BenchMemoryFactory::new();
        let store = VertexStore::new(memories.memory()).expect("vertex store");
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_vertex_push");
            for i in 0..helper::MEDIUM_N {
                store
                    .push(Vertex {
                        base_slot_start: black_box(i * 4),
                        degree: 0,
                        capacity: 4,
                        log_head: -1,
                        deleted: false,
                    })
                    .expect("push vertex");
            }
        })
    }

    /// Measures random-ish vertex row reads followed by in-place updates. The
    /// intent is to catch regressions in row offset calculation and stable
    /// memory read/write overhead for update-side metadata.
    #[bench(raw)]
    fn bench_lara_vertex_get_set_1024() -> canbench_rs::BenchResult {
        let store = populate_store(helper::MEDIUM_N);
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_vertex_get_set");
            for i in 0..helper::MEDIUM_N {
                let idx = helper::splitmix64(i) % helper::MEDIUM_N;
                let id = VertexId::from(idx as u32);
                let v = store.get(id);
                store.set(id, &v.with_degree(black_box(v.degree.wrapping_add(1))));
            }
        })
    }
}
