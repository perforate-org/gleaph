//! Multi-shard fan-out from index hits (federation target path).

use candid::Principal;
use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};
use gleaph_graph_kernel::index::PostingHit;

use crate::facade::store::RouterStore;
use crate::federation::ShardingPolicy;
use crate::seed::IndexAnchor;
use crate::state::RouterError;

#[derive(Clone, Debug)]
pub struct SeedRouting {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub hits: Vec<PostingHit>,
    /// Present when dispatch used an index anchor; drives [`crate::seed::seeds_for_local_shard`].
    pub anchor: Option<IndexAnchor>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MultiShardDispatch;

impl ShardingPolicy for MultiShardDispatch {
    fn resolve_without_anchor(
        &self,
        shards: &[ShardRegistryEntry],
    ) -> Result<Vec<SeedRouting>, RouterError> {
        if shards.len() == 1 {
            Ok(vec![SeedRouting {
                shard_id: shards[0].shard_id,
                graph_canister: shards[0].graph_canister,
                hits: Vec::new(),
                anchor: None,
            }])
        } else {
            Err(RouterError::InvalidArgument(
                "no index anchor: single-shard graph required".into(),
            ))
        }
    }

    fn resolve_with_hits(
        &self,
        store: &RouterStore,
        logical_graph_name: &str,
        _shards: &[ShardRegistryEntry],
        anchor: IndexAnchor,
        hits: &[PostingHit],
    ) -> Result<Vec<SeedRouting>, RouterError> {
        resolve_seed_routings_multi(store, hits, logical_graph_name, anchor)
    }
}

/// Fan out one routing per distinct shard in index hits.
pub fn resolve_seed_routings_multi(
    store: &RouterStore,
    hits: &[PostingHit],
    logical_graph_name: &str,
    anchor: IndexAnchor,
) -> Result<Vec<SeedRouting>, RouterError> {
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let shards = store.list_shards_for_graph(logical_graph_name)?;
    let mut shard_ids: Vec<ShardId> = hits.iter().map(|h| h.shard_id).collect();
    shard_ids.sort_unstable();
    shard_ids.dedup();

    let mut out = Vec::with_capacity(shard_ids.len());
    for shard_id in shard_ids {
        let entry = shards
            .iter()
            .find(|s| s.shard_id == shard_id)
            .ok_or(RouterError::ShardNotRegistered)?;
        let shard_hits: Vec<PostingHit> = hits
            .iter()
            .filter(|h| h.shard_id == shard_id)
            .cloned()
            .collect();
        out.push(SeedRouting {
            shard_id,
            graph_canister: entry.graph_canister,
            hits: shard_hits,
            anchor: Some(anchor.clone()),
        });
    }
    Ok(out)
}

/// Fan out to every shard without index seeds (oversized anchor hit list fallback).
pub fn resolve_unseeded_all_shards(shards: &[ShardRegistryEntry]) -> Vec<SeedRouting> {
    shards
        .iter()
        .map(|entry| SeedRouting {
            shard_id: entry.shard_id,
            graph_canister: entry.graph_canister,
            hits: Vec::new(),
            anchor: None,
        })
        .collect()
}
