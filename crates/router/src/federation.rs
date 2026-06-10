//! Router-side sharding policy and per-shard dispatch construction.

mod aggregate_index_fast_path;
mod aggregate_merge;
mod dispatch;
mod having_filter;
mod limits;
mod merge;
mod standalone;

pub use aggregate_index_fast_path::{
    AggregateIndexFastPath, LabelCountTelemetryFastPath, gql_query_result_from_label_live_count,
    gql_query_result_from_posting_counts, split_label_and_property_anchors,
    try_aggregate_index_fast_path, try_label_count_telemetry_fast_path, vertex_label_live_count,
};
#[expect(unused_imports, reason = "public federation API surface")]
pub use aggregate_merge::{
    FederatedAggregateMerge, FederatedMergeMode, federated_dispatch_plan_blob,
    federated_merge_mode_from_ops, federated_merge_mode_from_plans, merge_aggregate_blobs,
    merge_optional_aggregate_blobs, strip_post_aggregate_having,
};
#[expect(unused_imports, reason = "re-exported for gql integration tests")]
pub use dispatch::{SeedRouting, resolve_seed_routings_multi, resolve_unseeded_all_shards};
pub use having_filter::apply_federated_aggregate_having;
pub use limits::{
    FAST_PATH_MAX_VERTEX_FILTER_HITS, packed_vertices_exceed_fast_path_budget,
    posting_hits_exceed_fast_path_budget,
};
#[expect(unused_imports, reason = "public federation API surface")]
pub use merge::{
    empty_execute_plan_result, merge_add_row_count, merge_execute_plan_result, merge_row_counts,
};
pub use standalone::StandaloneSharding;

use candid::Principal;
use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};
use gleaph_graph_kernel::index::PostingHit;

use crate::facade::store::RouterStore;
use crate::seed::{IndexAnchor, seeds_for_local_shard};
use crate::state::RouterError;

/// Per-shard graph execution target after routing.
#[derive(Clone, Debug)]
pub struct ShardDispatch {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub seed_bindings_blob: Option<Vec<u8>>,
}

/// How a logical graph is routed to one or more shards.
pub trait ShardingPolicy {
    fn resolve_without_anchor(
        &self,
        shards: &[ShardRegistryEntry],
    ) -> Result<Vec<SeedRouting>, RouterError>;

    fn resolve_with_hits(
        &self,
        store: &RouterStore,
        logical_graph_name: &str,
        shards: &[ShardRegistryEntry],
        anchor: IndexAnchor,
        hits: &[PostingHit],
    ) -> Result<Vec<SeedRouting>, RouterError>;
}

pub fn sharding_policy_for(shards: &[ShardRegistryEntry]) -> ActiveShardingPolicy {
    if shards.len() == 1 {
        ActiveShardingPolicy::Standalone(StandaloneSharding)
    } else {
        ActiveShardingPolicy::Multi(dispatch::MultiShardDispatch)
    }
}

pub enum ActiveShardingPolicy {
    Standalone(StandaloneSharding),
    Multi(dispatch::MultiShardDispatch),
}

impl ShardingPolicy for ActiveShardingPolicy {
    fn resolve_without_anchor(
        &self,
        shards: &[ShardRegistryEntry],
    ) -> Result<Vec<SeedRouting>, RouterError> {
        match self {
            Self::Standalone(policy) => policy.resolve_without_anchor(shards),
            Self::Multi(policy) => policy.resolve_without_anchor(shards),
        }
    }

    fn resolve_with_hits(
        &self,
        store: &RouterStore,
        logical_graph_name: &str,
        shards: &[ShardRegistryEntry],
        anchor: IndexAnchor,
        hits: &[PostingHit],
    ) -> Result<Vec<SeedRouting>, RouterError> {
        match self {
            Self::Standalone(policy) => {
                policy.resolve_with_hits(store, logical_graph_name, shards, anchor, hits)
            }
            Self::Multi(policy) => {
                policy.resolve_with_hits(store, logical_graph_name, shards, anchor, hits)
            }
        }
    }
}

pub fn routings_to_dispatches(routings: Vec<SeedRouting>) -> Vec<ShardDispatch> {
    routings
        .into_iter()
        .map(|routing| ShardDispatch {
            shard_id: routing.shard_id,
            graph_canister: routing.graph_canister,
            seed_bindings_blob: routing.anchor.as_ref().and_then(|anchor| {
                seeds_for_local_shard(anchor.variable(), &routing.hits, routing.shard_id)
            }),
        })
        .collect()
}
