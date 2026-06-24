//! Stateless facade over stable vector-index storage ([`super::stable`]).
//!
//! Storage domains (ADR 0031 Slice 2):
//! - [`authorization`] — router auth, shard-canister attachments, detach purge
//! - [`mutation`] — `vector_upsert` / `vector_remove` with embedding_version + subject-clock
//!   idempotence over a degenerate `ivf_flat` page store

mod authorization;
mod mutation;
mod search;

#[cfg(feature = "canbench")]
pub(crate) use search::SearchTuning;

#[cfg(any(test, feature = "canbench"))]
mod seed;

#[cfg(test)]
mod tests;

/// Default page byte budget when an index def is created lazily on first upsert.
///
/// Chosen for one StableMemory-friendly read plus a heap scoring buffer once search lands (Slice
/// 4+). `slots_per_page` is derived from this and the index `stride_bytes` (see [`mutation`]).
pub(crate) const DEFAULT_MAX_PAGE_BYTES: u32 = 64 * 1024;

/// Fixed per-page overhead reserved for the page header when computing `slots_per_page`.
pub(crate) const PAGE_HEADER_BYTES: u32 = 64;

/// Degenerate `ivf_flat` partition: a single partition `0` in Slice 2.
pub(crate) const DEGENERATE_PARTITION_ID: u32 = 0;

/// First physical index generation, assigned on index creation.
pub(crate) const INITIAL_INDEX_VERSION: u64 = 1;

/// First `VectorId` / `generation`; `0` is reserved as "none".
pub(crate) const FIRST_ALLOCATION: u64 = 1;

/// Stateless facade over vector-index stable structures initialized in [`super::stable`].
#[derive(Clone, Copy, Debug, Default)]
pub struct VectorIndexStore;

impl VectorIndexStore {
    pub const fn new() -> Self {
        Self
    }
}
