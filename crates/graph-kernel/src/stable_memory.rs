//! Shared wire types for stable-memory observability.

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Logical size of one named virtual stable-memory region owned by a canister.
///
/// This excludes `MemoryManager` bucket rounding and is therefore not the
/// canister's physical stable-memory allocation.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct StableMemoryRegionStats {
    pub name: String,
    pub memory_id: u8,
    pub bucket_pages: u16,
    pub logical_pages: u64,
    pub logical_bytes: u64,
    pub allocated_pages: u64,
    pub slack_pages: u64,
}

/// Stable-memory inventory for one canister.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct StableMemoryStats {
    pub bucket_pages: u16,
    pub logical_total_pages: u64,
    pub logical_total_bytes: u64,
    pub estimated_allocated_pages: u64,
    pub estimated_allocated_bytes: u64,
    pub regions: Vec<StableMemoryRegionStats>,
}
