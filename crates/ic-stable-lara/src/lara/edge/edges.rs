//! Stable LARA CSR edge slab (`edges_`) plus graph-wide edge metadata.
//!
//! # V1 layout
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! Magic "LEG"                           ↕ 3 bytes
//! --------------------------------------------------
//! Layout version                        ↕ 1 byte
//! --------------------------------------------------
//! Element capacity                      ↕ 8 bytes
//! --------------------------------------------------
//! Number of leaf segments               ↕ 4 bytes
//! --------------------------------------------------
//! Segment size in vertices              ↕ 4 bytes
//! --------------------------------------------------
//! PMA tree height                       ↕ 4 bytes
//! --------------------------------------------------
//! Number of logical edges               ↕ 8 bytes
//! --------------------------------------------------
//! Edge slot stride                      ↕ 4 bytes
//! --------------------------------------------------
//! Slab occupied tail                    ↕ 8 bytes
//! --------------------------------------------------
//! Reserved                              ↕ 20 bytes
//! -------------------------------------------------- <- Address 64
//! E_0                                   ↕ E::BYTES bytes
//! --------------------------------------------------
//! E_1                                   ↕ E::BYTES bytes
//! --------------------------------------------------
//! ...
//! --------------------------------------------------
//! E_(elem_capacity-1)                   ↕ E::BYTES bytes
//! --------------------------------------------------
//! Unallocated space
//! ```

use crate::{
    GrowFailed, read_u32, read_u64, safe_write, traits::CsrEdge, types::Address, write_u32,
    write_u64,
};
use ic_stable_structures::Memory;
use std::{fmt, marker::PhantomData};

/// Magic bytes that identify a LARA edge slab memory.
pub const MAGIC: [u8; 3] = *b"LEG";
/// Current edge slab layout version.
pub const LAYOUT_VERSION: u8 = 1;
/// Size of the persisted edge slab header in bytes.
pub const HEADER_SIZE: u64 = 64;

const ELEM_CAPACITY_OFFSET: u64 = 4;
const SEGMENT_COUNT_OFFSET: u64 = 12;
const SEGMENT_SIZE_OFFSET: u64 = 16;
const TREE_HEIGHT_OFFSET: u64 = 20;
const NUM_EDGES_OFFSET: u64 = 24;
const EDGE_STRIDE_OFFSET: u64 = 32;
const SLAB_OCCUPIED_TAIL_OFFSET: u64 = 36;
const INITIAL_VERTEX_EDGE_SLOTS_OFFSET: u64 = 44;
const RESERVED_OFFSET: u64 = 48;
const RESERVED_SIZE: usize = 16;

/// Persisted V1 edge slab header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    /// Magic bytes, always `LEG` for this layout.
    pub magic: [u8; 3],
    /// Layout version for this header.
    pub version: u8,
    /// Number of edge slots allocated in the slab.
    pub elem_capacity: u64,
    /// Number of leaf segments in the PMA segment tree.
    pub segment_count: u32,
    /// Number of vertices covered by one leaf segment.
    pub segment_size: u32,
    /// Height of the PMA segment tree.
    pub tree_height: u32,
    /// Number of logical edges stored by the graph.
    pub num_edges: u64,
    /// Encoded byte width of one edge record.
    pub stride: u32,
    /// Highest occupied slab slot boundary used by tail allocation.
    pub slab_occupied_tail: u64,
    /// When non-zero, new vertex batches in a leaf may allocate `n_L * this`
    /// slab slots eagerly. Zero preserves legacy implicit packing.
    pub initial_vertex_edge_slots: u32,
}

impl HeaderV1 {
    /// Builds a fresh V1 header for a new edge slab.
    pub fn new(
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        stride: u32,
        initial_vertex_edge_slots: u32,
    ) -> Self {
        Self {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            elem_capacity,
            segment_count,
            segment_size,
            tree_height: tree_height_for_segment_count(segment_count),
            num_edges: 0,
            stride,
            slab_occupied_tail: 0,
            initial_vertex_edge_slots,
        }
    }
}

/// Errors returned when reopening a persisted edge slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The memory header does not contain the LARA edge magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The stored layout version is not supported by this crate version.
    IncompatibleVersion(u8),
    /// The memory is empty or does not contain a valid slab header.
    InvalidLayout,
    /// The store could not allocate its metadata.
    OutOfMemory,
    /// The persisted edge width does not match the edge type `E`.
    StrideMismatch {
        /// Expected edge record width.
        expected: u32,
        /// Edge record width read from stable memory.
        actual: u32,
    },
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => write!(f, "bad edge magic {actual:?}, expected {MAGIC:?}"),
            Self::IncompatibleVersion(v) => write!(f, "unsupported edge layout version {v}"),
            Self::InvalidLayout => write!(f, "invalid edge slab layout"),
            Self::OutOfMemory => write!(f, "failed to allocate edge slab metadata"),
            Self::StrideMismatch { expected, actual } => {
                write!(f, "edge stride mismatch: expected {expected}, got {actual}")
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Stable storage for raw fixed-width edge slots.
#[derive(Clone, Debug)]
pub struct EdgeSlabStore<E: CsrEdge, M: Memory> {
    memory: M,
    _marker: PhantomData<E>,
}

impl<E: CsrEdge, M: Memory> EdgeSlabStore<E, M> {
    /// Creates a fresh edge slab with `header`.
    pub fn new(memory: M, header: HeaderV1) -> Result<Self, GrowFailed> {
        let store = Self {
            memory,
            _marker: PhantomData,
        };
        store.grow_for_header(&header)?;
        store.write_header(&header);
        Ok(store)
    }

    /// Reopens an existing edge slab from stable memory.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Err(InitError::InvalidLayout);
        }
        let store = Self {
            memory,
            _marker: PhantomData,
        };
        let header = store.read_header()?;
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        if header.stride as usize != E::BYTES {
            return Err(InitError::StrideMismatch {
                expected: E::BYTES as u32,
                actual: header.stride,
            });
        }
        Ok(store)
    }

    /// Consumes the store and returns the underlying memory.
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Reads the current persisted slab header.
    pub fn header(&self) -> Result<HeaderV1, InitError> {
        self.read_header()
    }

    /// Writes the full slab header to stable memory.
    pub fn write_header(&self, h: &HeaderV1) {
        self.memory.write(0, &h.magic);
        self.memory.write(3, &[h.version]);
        write_u64(
            &self.memory,
            Address::from(ELEM_CAPACITY_OFFSET),
            h.elem_capacity,
        );
        write_u32(
            &self.memory,
            Address::from(SEGMENT_COUNT_OFFSET),
            h.segment_count,
        );
        write_u32(
            &self.memory,
            Address::from(SEGMENT_SIZE_OFFSET),
            h.segment_size,
        );
        write_u32(
            &self.memory,
            Address::from(TREE_HEIGHT_OFFSET),
            h.tree_height,
        );
        write_u64(&self.memory, Address::from(NUM_EDGES_OFFSET), h.num_edges);
        write_u32(&self.memory, Address::from(EDGE_STRIDE_OFFSET), h.stride);
        write_u64(
            &self.memory,
            Address::from(SLAB_OCCUPIED_TAIL_OFFSET),
            h.slab_occupied_tail,
        );
        write_u32(
            &self.memory,
            Address::from(INITIAL_VERTEX_EDGE_SLOTS_OFFSET),
            h.initial_vertex_edge_slots,
        );
        self.memory.write(RESERVED_OFFSET, &[0u8; RESERVED_SIZE]);
    }

    /// Updates the logical edge count field in the header.
    pub fn set_num_edges(&self, n: u64) {
        write_u64(&self.memory, Address::from(NUM_EDGES_OFFSET), n);
    }
    /// Updates the slab capacity and grows stable memory if needed.
    pub fn set_elem_capacity(&self, n: u64) -> Result<(), GrowFailed> {
        let mut h = self.header().map_err(|_| GrowFailed {
            current_size: self.memory.size(),
            delta: 0,
        })?;
        h.elem_capacity = n;
        self.grow_for_header(&h)?;
        write_u64(&self.memory, Address::from(ELEM_CAPACITY_OFFSET), n);
        Ok(())
    }
    /// Reads the raw bytes for `slot` into `out`.
    pub fn read_slot(&self, slot: u64, out: &mut [u8]) {
        self.memory.read(slot_offset::<E>(slot), out);
    }

    /// Reads `count` contiguous slots starting at `start_slot` into `out`.
    ///
    /// `out.len()` must equal `count * E::BYTES` for some `count >= 0`.
    pub(crate) fn read_slots_contiguous(&self, start_slot: u64, out: &mut [u8]) {
        debug_assert_eq!(out.len() % E::BYTES, 0);
        if out.is_empty() {
            return;
        }
        self.memory.read(slot_offset::<E>(start_slot), out);
    }

    /// Writes raw encoded edge bytes to `slot`.
    pub fn write_slot(&self, slot: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
        debug_assert_eq!(bytes.len(), E::BYTES);
        safe_write(&self.memory, slot_offset::<E>(slot), bytes)
    }

    fn read_header(&self) -> Result<HeaderV1, InitError> {
        let mut magic = [0u8; 3];
        self.memory.read(0, &mut magic);
        if magic != MAGIC {
            return Err(InitError::BadMagic { actual: magic });
        }
        let mut version = [0u8; 1];
        self.memory.read(3, &mut version);
        if version[0] != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(version[0]));
        }
        Ok(HeaderV1 {
            magic,
            version: version[0],
            elem_capacity: read_u64(&self.memory, Address::from(ELEM_CAPACITY_OFFSET)),
            segment_count: read_u32(&self.memory, Address::from(SEGMENT_COUNT_OFFSET)),
            segment_size: read_u32(&self.memory, Address::from(SEGMENT_SIZE_OFFSET)),
            tree_height: read_u32(&self.memory, Address::from(TREE_HEIGHT_OFFSET)),
            num_edges: read_u64(&self.memory, Address::from(NUM_EDGES_OFFSET)),
            stride: read_u32(&self.memory, Address::from(EDGE_STRIDE_OFFSET)),
            slab_occupied_tail: read_u64(&self.memory, Address::from(SLAB_OCCUPIED_TAIL_OFFSET)),
            initial_vertex_edge_slots: read_u32(
                &self.memory,
                Address::from(INITIAL_VERTEX_EDGE_SLOTS_OFFSET),
            ),
        })
    }

    fn grow_for_header(&self, h: &HeaderV1) -> Result<(), GrowFailed> {
        let need = HEADER_SIZE + h.elem_capacity.saturating_mul(E::BYTES as u64);
        if need == 0 {
            return Ok(());
        }
        safe_write(&self.memory, need - 1, &[0])
    }
}

/// Returns the byte offset of `slot` in an edge slab for edge type `E`.
#[inline]
pub fn slot_offset<E: CsrEdge>(slot: u64) -> u64 {
    HEADER_SIZE + slot.saturating_mul(E::BYTES as u64)
}

#[inline]
fn floor_log2(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// PMA [`HeaderV1::tree_height`] for a given leaf segment count (at least one leaf).
#[inline]
pub(crate) fn tree_height_for_segment_count(segment_count: u32) -> u32 {
    floor_log2(segment_count.max(1))
}

/// Minimum power-of-two leaf count for a vertex column (`0` vertices ⇒ one leaf).
///
/// Saturates to `u32::MAX` if the logical leaf need does not fit in a power of two
/// (extreme inputs only); callers such as [`crate::LaraGraph`] tie `vertex_len` to
/// the real vertex column length.
#[inline]
pub fn segment_tree_leaf_count(vertex_len: u64, segment_size: u32) -> u32 {
    let sz = u64::from(segment_size.max(1));
    let need = if vertex_len == 0 {
        1u32
    } else {
        u32::try_from(vertex_len.div_ceil(sz)).unwrap_or(u32::MAX)
    };
    need.max(1).checked_next_power_of_two().unwrap_or(u32::MAX)
}

#[cfg(feature = "canbench")]
mod bench {
    use std::hint::black_box;

    use canbench_rs::bench;

    use super::{EdgeSlabStore, HeaderV1};
    use crate::{bench as helper, test_support::TestEdge, traits::CsrEdge};

    /// Measures raw slab slot writes followed by raw slot reads. This is the
    /// lowest-level edge payload I/O baseline, below `EdgeStore` log and count
    /// bookkeeping.
    #[bench(raw)]
    fn bench_lara_edge_slab_write_read_1024() -> canbench_rs::BenchResult {
        let mut memories = helper::BenchMemoryFactory::new();
        let store = EdgeSlabStore::<TestEdge, _>::new(
            memories.memory(),
            HeaderV1::new(helper::MEDIUM_N, 16, 16, TestEdge::BYTES as u32, 0),
        )
        .expect("edge slab");
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_edge_slab_write_read");
            let mut payload = [0u8; TestEdge::BYTES];
            for i in 0..helper::MEDIUM_N {
                helper::test_edge(i).write_to(&mut payload);
                store.write_slot(i, &payload).expect("write slot");
            }
            let mut sum = 0u32;
            for i in 0..helper::MEDIUM_N {
                store.read_slot(i, &mut payload);
                sum ^= TestEdge::read_from(&payload).0;
            }
            black_box(sum);
        })
    }
}
