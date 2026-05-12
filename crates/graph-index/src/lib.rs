//! Federated property index canister (`gleaph-graph-index`).
//!
//! Owns global postings `(property_id, value, shard_id, vertex_id)` and a shard registry
//! `shard_id → Principal` for resolving graph shard canisters.
//!
//! ## Read API visibility (v1)
//!
//! `lookup_equal`, `lookup_range`, and `resolve_shard_principal` perform **no caller-based authorization**: any
//! principal that can reach this canister may read the full posting directory and shard registry.
//! Treat that as public metadata unless you gate the canister at a higher layer.
//!
//! `lookup_range` uses the same lexicographic order on encoded value bytes as `lookup_equal` (`memcmp`).

mod facade;
mod key;
mod posting_range;
pub mod state;

pub mod init;

mod canister;

pub use facade::IndexStore;
pub use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest};
pub use init::IndexInitArgs;
pub use key::PostingKey;
pub use state::IndexError;

// --- Canister surface (`ic-cdk` macros stay here; logic lives in `canister`) ---

use candid::Principal;
use ic_cdk_macros::{init, query, update};

#[init]
fn init(args: IndexInitArgs) {
    canister::init(args);
}

#[update]
fn admin_register_shard(shard_id: u64, shard_principal: Principal) {
    canister::admin_register_shard(shard_id, shard_principal);
}

#[update]
fn posting_insert(shard_id: u64, property_id: u32, value: Vec<u8>, vertex_id: u32) {
    canister::posting_insert(shard_id, property_id, value, vertex_id);
}

#[update]
fn posting_remove(shard_id: u64, property_id: u32, value: Vec<u8>, vertex_id: u32) {
    canister::posting_remove(shard_id, property_id, value, vertex_id);
}

#[query]
fn lookup_equal(property_id: u32, value: Vec<u8>) -> Vec<PostingHit> {
    canister::lookup_equal(property_id, value)
}

#[query]
fn lookup_range(property_id: u32, req: PostingRangeRequest) -> Vec<PostingHit> {
    canister::lookup_range(property_id, req)
}

#[query]
fn resolve_shard_principal(shard_id: u64) -> Option<Principal> {
    canister::resolve_shard_principal(shard_id)
}
