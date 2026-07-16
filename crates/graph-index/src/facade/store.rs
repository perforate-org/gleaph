//! Stateless facade over stable index storage ([`super::stable`]).
//!
//! Storage domains (Phase 2):
//! - [`authorization`] — admins, shard-canister attachments, router caller checks
//! - [`property_postings`] — property equality postings read/write
//! - [`label_postings`] — vertex label membership postings read/write

mod authorization;
mod edge_postings;
mod intersection;
mod label_postings;
mod posting_purge;
mod property_postings;

#[cfg(test)]
mod tests;

use crate::state::IndexError;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    IndexEqualSpec, PostingRangeRequest, validate_index_value_key_bytes,
};

pub(super) fn ensure_index_value_key(value: &[u8]) -> Result<(), IndexError> {
    validate_index_value_key_bytes(value).map_err(|_| IndexError::IndexValueKeyTooLarge)
}

#[cfg(target_family = "wasm")]
const QUERY_INSTRUCTION_BUDGET: u64 = 5_000_000_000;
#[cfg(target_family = "wasm")]
const UPDATE_INSTRUCTION_BUDGET: u64 = 40_000_000_000;
#[cfg(target_family = "wasm")]
const QUERY_BUDGET_HEADROOM: u64 = 500_000_000;
#[cfg(target_family = "wasm")]
const UPDATE_BUDGET_HEADROOM: u64 = 1_000_000_000;

/// Returns true when the canister has consumed most of its instruction budget for the current
/// message kind. Query calls use the 5B instruction limit; update calls use the 40B limit.
fn instruction_counter_near_budget(query: bool) -> bool {
    #[cfg(target_family = "wasm")]
    {
        let (budget, headroom) = if query {
            (QUERY_INSTRUCTION_BUDGET, QUERY_BUDGET_HEADROOM)
        } else {
            (UPDATE_INSTRUCTION_BUDGET, UPDATE_BUDGET_HEADROOM)
        };
        ic_cdk::api::instruction_counter() >= budget.saturating_sub(headroom)
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let _ = query;
        false
    }
}

pub(super) fn ensure_posting_range_request(req: &PostingRangeRequest) -> Result<(), IndexError> {
    match req {
        PostingRangeRequest::Ge(b)
        | PostingRangeRequest::Gt(b)
        | PostingRangeRequest::Le(b)
        | PostingRangeRequest::Lt(b) => ensure_index_value_key(b),
        PostingRangeRequest::Between { low, high } => {
            ensure_index_value_key(low)?;
            ensure_index_value_key(high)?;
            if low.is_empty() || high.is_empty() || low >= high {
                return Err(IndexError::InvalidRangeBounds);
            }
            Ok(())
        }
    }
}

pub(super) fn ensure_intersection_specs(specs: &[IndexEqualSpec]) -> Result<(), IndexError> {
    for spec in specs {
        ensure_index_value_key(&spec.value)?;
    }
    Ok(())
}

/// Default page size for [`IndexStore::lookup_label_page`] and property posting exports.
pub const DEFAULT_LOOKUP_PAGE_LIMIT: usize =
    gleaph_graph_kernel::index::MAX_POSTING_PAGE_HITS as usize;

/// Clamp a client-supplied page limit into `1..=DEFAULT_LOOKUP_PAGE_LIMIT`.
pub(super) fn clamp_posting_page_limit(limit: u32) -> usize {
    usize::try_from(limit)
        .unwrap_or(DEFAULT_LOOKUP_PAGE_LIMIT)
        .clamp(1, DEFAULT_LOOKUP_PAGE_LIMIT)
}

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

pub(crate) fn pack_edge_identity(
    shard_id: ShardId,
    owner_vertex_id: u32,
    label_id: u16,
    slot_index: u32,
) -> u128 {
    (u128::from(shard_id.raw()) << 96)
        | (u128::from(owner_vertex_id) << 64)
        | (u128::from(u32::from(label_id)) << 32)
        | u128::from(slot_index)
}
