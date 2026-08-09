//! Stable-memory vector page store (physical layer).
//!
//! The crate owns the **physical facts** of the Gleaph vector canister's row slab (ADR 0064):
//!
//! - the two-table page format `[PageHeader] [run_table] [row_meta] [vector_bytes]`;
//! - the run table that shares a shard across contiguous rows;
//! - the packed 30-bit [`VertexPayload`] row identity with a tombstone bit (mirroring the graph's
//!   `VertexRef`); and
//! - the distance kernels (sub-square L2 with early exit, dot, binary popcount) over stored byte
//!   spans.
//!
//! **Boundary.** This crate must not know subject-map clocks, partitions, centroids, labels,
//! rebuild, or search semantics: strides and aux widths arrive as parameters, and header
//! validation is fail-closed (magic + binary format version 1; the discarded ASCII-magic format is
//! rejected). The domain layers live in the vector canister.
//!
//! Format lineage restarts at version 1 (breaking; dev data wiped). Headers use a 3-byte magic
//! (`VSL` / `VPG`) plus a binary `u8` version byte `1`; the discarded ASCII magic (`VSL1` / `VPG1`,
//! 4th byte `0x31`) is rejected because its version byte no longer matches.

#![cfg_attr(all(feature = "canbench", target_family = "wasm"), no_main)]
// wasm64 exposes the wasm SIMD intrinsics behind this nightly feature gate (rust-lang #90599).
// `unused_features` fires on host builds where this gate is cfg'd out (clippy does not honor the
// cfg for this lint); the feature is genuinely required on wasm64.
#![cfg_attr(
    all(target_family = "wasm", target_arch = "wasm64"),
    feature(simd_wasm64)
)]
#![allow(unused_features)]
#![warn(missing_docs)]

#[cfg(feature = "canbench")]
mod bench;
pub mod header;
pub mod kernel;
pub mod layout;
pub mod payload;
pub mod run;
pub mod slab;

pub use header::{
    FORMAT_VERSION, HeaderError, MAGIC_PAGE, MAGIC_SLAB, MAX_RUNS, PAGE_HEADER_SIZE, PageHeader,
    SLAB_HEADER_SIZE, SlabHeader,
};
pub use layout::PageLayout;
pub use payload::{MAX_VERTEX_ID, PayloadError, RowMeta, VertexPayload};
pub use run::{RunEntry, read_run, write_run};
pub use slab::{Slab, SlabError};
