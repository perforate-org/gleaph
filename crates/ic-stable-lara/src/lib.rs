//! Stable-memory implementation of LARA, the Localized Adjacency Relocation
//! Array.
//!
//! LARA stores adjacency lists in a CSR-style slab while allowing local
//! relocation of dense physical spans. The key design boundary is that clean
//! scans remain direct:
//!
//! ```text
//! vertex_id -> vertex row -> edge slots [base_slot_start, base_slot_start + degree)
//! ```
//!
//! A clean scan is authoritative only over `base_slot_start` and `degree`.
//! It must not consult vertex `capacity`, segment span metadata, or the free
//! span manager. Update and maintenance paths may use all three vertex fields:
//! `base_slot_start`, `degree`, and `capacity`.
//!
//! `capacity` is the number of slab slots owned by a vertex. The live prefix
//! `[base_slot_start, base_slot_start + degree)` must stay contained in the
//! owned span `[base_slot_start, base_slot_start + capacity)`. Relocation
//! rewrites bases and capacities together, publishes segment span metadata, and
//! releases retired physical spans only after the query-visible state has been
//! committed.
//!
//! The main external reference for the dynamic adjacency idea is
//! [DGAP](https://github.com/DIR-LAB/DGAP), but this crate owns a separate
//! persisted layout and public API centered on LARA's explicit capacity and
//! local relocation contracts.

#![allow(incomplete_features)]
#![cfg_attr(all(feature = "canbench", target_arch = "wasm32"), no_main)]
#![feature(specialization)]

use derive_more::{Display, From, Into};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use std::{
    borrow::Cow,
    error,
    fmt::{Display, Formatter},
};

#[cfg(feature = "canbench")]
mod bench;
pub mod bidirectional;
pub mod lara;
mod traits;
mod types;

pub use bidirectional::{
    BidirectionalLara, BidirectionalLaraError, BidirectionalLaraGraph,
    BidirectionalMaintenanceReport, DeferredBidirectionalLara, DeferredBidirectionalLaraError,
    DeferredBidirectionalLaraGraph,
};
pub use lara::{
    LaraGraph,
    edge::{
        EdgeHeaderV1, EdgeStore, InitError as EdgeInitError, LogHeaderV1,
        free_span::{FreeSpan, FreeSpanError, FreeSpanStore, InitError as FreeSpanInitError},
        span_meta::{SegmentSpanMeta, SegmentSpanMetaStore},
    },
    maintenance::{
        DeferredConfig, DeferredLaraGraph, MaintenanceBudget, MaintenanceReport,
        MaintenanceWorkReport,
    },
    vertex::{InitError as VertexInitError, Vertex, VertexStore},
};
pub use traits::*;

pub type Lara<E, V, M> = LaraGraph<E, V, M>;
pub type DeferredLara<E, V, M> = DeferredLaraGraph<E, V, M>;

pub use ic_stable_structures::vec_mem::VectorMemory;
use types::Address;

#[repr(transparent)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Display, From, Into,
)]
pub struct VertexId(u32);

#[repr(transparent)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Display, From, Into,
)]
pub struct SegmentId(u32);

impl From<SegmentId> for usize {
    fn from(value: SegmentId) -> Self {
        value.0 as usize
    }
}

impl Storable for SegmentId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 4,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.0.to_le_bytes().to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&bytes.as_ref()[0..4]);
        Self(u32::from_le_bytes(buf))
    }
}

#[repr(transparent)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Display, From, Into,
)]
pub struct VertexCount(u64);

const WASM_PAGE_SIZE: u64 = 65536;

/// A helper function that reads a single 32bit integer encoded as
/// little-endian from the specified memory at the specified offset.
fn read_u32<M: Memory>(m: &M, addr: Address) -> u32 {
    let mut buf: [u8; 4] = [0; 4];
    m.read(addr.get(), &mut buf);
    u32::from_le_bytes(buf)
}

/// A helper function that reads a single 64bit integer encoded as
/// little-endian from the specified memory at the specified offset.
fn read_u64<M: Memory>(m: &M, addr: Address) -> u64 {
    let mut buf: [u8; 8] = [0; 8];
    m.read(addr.get(), &mut buf);
    u64::from_le_bytes(buf)
}

fn read_i32<M: Memory>(m: &M, addr: Address) -> i32 {
    let mut buf: [u8; 4] = [0; 4];
    m.read(addr.get(), &mut buf);
    i32::from_le_bytes(buf)
}

/// Writes a single 32-bit integer encoded as little-endian.
fn write_u32<M: Memory>(m: &M, addr: Address, val: u32) {
    write(m, addr.get(), &val.to_le_bytes());
}

fn write_i32<M: Memory>(m: &M, addr: Address, val: i32) {
    write(m, addr.get(), &val.to_le_bytes());
}

/// Writes a single 64-bit integer encoded as little-endian.
fn write_u64<M: Memory>(m: &M, addr: Address, val: u64) {
    write(m, addr.get(), &val.to_le_bytes());
}

#[derive(Debug, PartialEq, Eq)]
pub struct GrowFailed {
    current_size: u64,
    delta: u64,
}

impl Display for GrowFailed {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to grow memory: current size={}, delta={}",
            self.current_size, self.delta
        )
    }
}

impl error::Error for GrowFailed {}

/// Writes the bytes at the specified offset, growing the memory size if needed.
fn safe_write<M: Memory>(memory: &M, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
    let last_byte = offset
        .checked_add(bytes.len() as u64)
        .expect("Address space overflow");

    let size_pages = memory.size();
    let size_bytes = size_pages
        .checked_mul(WASM_PAGE_SIZE)
        .expect("Address space overflow");

    if size_bytes < last_byte {
        let diff_bytes = last_byte - size_bytes;
        let diff_pages = diff_bytes
            .checked_add(WASM_PAGE_SIZE - 1)
            .expect("Address space overflow")
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

/// Like [safe_write], but panics if the memory.grow fails.
fn write<M: Memory>(memory: &M, offset: u64, bytes: &[u8]) {
    if let Err(GrowFailed {
        current_size,
        delta,
    }) = safe_write(memory, offset, bytes)
    {
        panic!(
            "Failed to grow memory from {} pages to {} pages (delta = {} pages).",
            current_size,
            current_size + delta,
            delta
        );
    }
}

#[cfg(any(test, feature = "canbench"))]
#[allow(dead_code)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    struct FailingGrowMemory {
        writes: Cell<usize>,
    }

    impl Memory for FailingGrowMemory {
        fn size(&self) -> u64 {
            0
        }

        fn grow(&self, _pages: u64) -> i64 {
            -1
        }

        fn read(&self, _offset: u64, _dst: &mut [u8]) {
            unreachable!("safe_write should fail before reading")
        }

        fn write(&self, _offset: u64, _src: &[u8]) {
            self.writes.set(self.writes.get() + 1);
        }
    }

    #[test]
    fn grow_failed_display_includes_current_size_and_delta() {
        assert_eq!(
            GrowFailed {
                current_size: 2,
                delta: 3,
            }
            .to_string(),
            "Failed to grow memory: current size=2, delta=3"
        );
    }

    #[test]
    fn safe_write_returns_grow_failed_when_memory_cannot_grow() {
        let memory = FailingGrowMemory::default();

        let err = safe_write(&memory, 0, &[1]).unwrap_err();

        assert_eq!(
            err,
            GrowFailed {
                current_size: 0,
                delta: 1,
            }
        );
        assert_eq!(memory.writes.get(), 0);
    }
}
