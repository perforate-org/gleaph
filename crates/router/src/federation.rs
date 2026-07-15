//! Router-side sharding policy and per-shard dispatch construction.

mod aggregate_index_fast_path;
mod aggregate_merge;
mod dispatch;
mod having_filter;
mod label_export;
mod limits;
mod merge;
mod standalone;

pub(crate) use label_export::{
    collect_label_hits_for_shards, collect_label_intersection_hits_for_shards,
};

pub use aggregate_index_fast_path::{
    AggregateIndexFastPath, gql_query_result_from_label_live_count,
    gql_query_result_from_posting_counts, split_label_and_property_anchors,
    try_aggregate_index_fast_path, try_label_count_telemetry_fast_path, vertex_label_live_count,
};
#[expect(unused_imports, reason = "public federation API surface")]
pub use aggregate_merge::{
    FederatedAggregateMerge, FederatedMergeMode, federated_dispatch_plan_blob,
    federated_merge_mode_from_ops, federated_merge_mode_from_plans, merge_aggregate_blobs,
    merge_optional_aggregate_blobs, strip_post_aggregate_having,
};
#[allow(unused_imports)] // public federation API surface
pub use dispatch::resolve_seed_routings_multi;
pub use dispatch::{SeedHits, SeedRouting, latest_shard_routing};
pub use having_filter::apply_federated_aggregate_having;
pub use limits::{packed_vertices_exceed_fast_path_budget, posting_hits_exceed_fast_path_budget};
#[expect(unused_imports, reason = "public federation API surface")]
pub use merge::{
    empty_execute_plan_result, merge_add_row_count, merge_execute_plan_result, merge_row_counts,
};
pub use standalone::StandaloneSharding;

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};

use crate::facade::store::RouterStore;
use crate::seed::{IndexAnchor, seeds_for_local_shard, seeds_for_local_shard_edges};
use crate::state::RouterError;

/// Per-shard graph execution target after routing.
#[derive(Clone, Debug)]
pub struct ShardDispatch {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub seed_bindings_blob: Option<Vec<u8>>,
    /// Router-resolved non-leading `SEARCH` relation for this shard (ADR 0034 Slice 5).
    /// `None` when the dispatched plan has no `SEARCH` or uses the leading seed path.
    pub resolved_search_blob: Option<Vec<u8>>,
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
        graph_id: GraphId,
        shards: &[ShardRegistryEntry],
        anchor: IndexAnchor,
        hits: SeedHits,
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
        graph_id: GraphId,
        shards: &[ShardRegistryEntry],
        anchor: IndexAnchor,
        hits: SeedHits,
    ) -> Result<Vec<SeedRouting>, RouterError> {
        match self {
            Self::Standalone(policy) => {
                policy.resolve_with_hits(store, graph_id, shards, anchor, hits)
            }
            Self::Multi(policy) => policy.resolve_with_hits(store, graph_id, shards, anchor, hits),
        }
    }
}

pub fn routings_to_dispatches(routings: Vec<SeedRouting>) -> Vec<ShardDispatch> {
    routings
        .into_iter()
        .map(|routing| ShardDispatch {
            shard_id: routing.shard_id,
            graph_canister: routing.graph_canister,
            seed_bindings_blob: routing
                .anchor
                .as_ref()
                .and_then(|anchor| match &routing.hits {
                    SeedHits::Vertices(hits) => {
                        seeds_for_local_shard(anchor.variable(), hits, routing.shard_id)
                    }
                    SeedHits::Edges(hits) => {
                        seeds_for_local_shard_edges(anchor.variable(), hits, routing.shard_id)
                    }
                }),
            resolved_search_blob: None,
        })
        .collect()
}

/// Group dispatches by Graph canister while preserving first-seen group and item order.
///
/// A group is the unit sent through `execute_plan_update_batch`; groups must not cross canister
/// boundaries because Graph-local execution and authorization are owned by the target canister.
pub fn group_dispatches_by_graph(dispatches: Vec<ShardDispatch>) -> Vec<Vec<ShardDispatch>> {
    let mut groups: Vec<Vec<ShardDispatch>> = Vec::new();
    for dispatch in dispatches {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group[0].graph_canister == dispatch.graph_canister)
        {
            group.push(dispatch);
        } else {
            groups.push(vec![dispatch]);
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(shard_id: u32, graph_canister: Principal) -> ShardDispatch {
        ShardDispatch {
            shard_id: ShardId::new(shard_id),
            graph_canister,
            seed_bindings_blob: None,
            resolved_search_blob: None,
        }
    }

    #[test]
    fn group_dispatches_preserves_order_and_never_crosses_graph_boundary() {
        let first = Principal::from_slice(&[1]);
        let second = Principal::from_slice(&[2]);
        let groups = group_dispatches_by_graph(vec![
            dispatch(7, first),
            dispatch(3, second),
            dispatch(9, first),
        ]);

        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0]
                .iter()
                .map(|item| item.shard_id)
                .collect::<Vec<_>>(),
            vec![ShardId::new(7), ShardId::new(9)]
        );
        assert_eq!(groups[1][0].shard_id, ShardId::new(3));
        assert!(groups.iter().all(|group| {
            group
                .iter()
                .all(|item| item.graph_canister == group[0].graph_canister)
        }));
    }
}
