//! Stateless facade over stable index storage ([`super::stable`]).
//!
//! Storage domains (Phase 2):
//! - [`authorization`] — admins, shard owners, router caller checks
//! - [`property_postings`] — property equality postings read/write
//! - [`label_postings`] — vertex label membership postings read/write

mod authorization;
mod label_postings;
mod property_postings;

#[cfg(test)]
mod tests;

use gleaph_graph_kernel::federation::ShardId;

/// Default cap on groups returned by [`IndexStore::count_postings_by_value`].
pub const DEFAULT_COUNT_POSTINGS_MAX_GROUPS: usize = 10_000;

/// Default page size for [`IndexStore::lookup_label_page`].
pub const DEFAULT_LABEL_LOOKUP_PAGE_LIMIT: usize = 10_000;

/// Stateless facade over index stable structures initialized in [`super::stable`].
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexStore;

impl IndexStore {
    pub const fn new() -> Self {
        Self
    }
}

pub(crate) fn pack_posting_vertex(shard_id: ShardId, vertex_id: u32) -> u64 {
    (u64::from(shard_id.raw()) << 32) | u64::from(vertex_id)
}
