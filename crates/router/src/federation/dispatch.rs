//! Multi-shard fan-out from index hits (federation target path).

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};
use gleaph_graph_kernel::index::{EdgePostingHit, PostingHit};

use crate::facade::store::RouterStore;
use crate::federation::ShardingPolicy;
use crate::seed::IndexAnchor;
use crate::state::RouterError;

/// Vertex or edge index hits from graph-index for per-shard seed encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeedHits {
    Vertices(Vec<PostingHit>),
    Edges(Vec<EdgePostingHit>),
}

impl SeedHits {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Vertices(hits) => hits.is_empty(),
            Self::Edges(hits) => hits.is_empty(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SeedRouting {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub hits: SeedHits,
    /// Present when dispatch used an index anchor; drives seed blob encoding.
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
                hits: SeedHits::Vertices(Vec::new()),
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
        graph_id: GraphId,
        _shards: &[ShardRegistryEntry],
        anchor: IndexAnchor,
        hits: SeedHits,
    ) -> Result<Vec<SeedRouting>, RouterError> {
        resolve_seed_routings_multi(store, hits, graph_id, anchor)
    }
}

/// Fan out one routing per distinct shard in index hits.
pub fn resolve_seed_routings_multi(
    store: &RouterStore,
    hits: SeedHits,
    graph_id: GraphId,
    anchor: IndexAnchor,
) -> Result<Vec<SeedRouting>, RouterError> {
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    let shard_ids = match &hits {
        SeedHits::Vertices(hits) => {
            let mut shard_ids: Vec<ShardId> = hits.iter().map(|h| h.shard_id).collect();
            shard_ids.sort_unstable();
            shard_ids.dedup();
            shard_ids
        }
        SeedHits::Edges(hits) => {
            let mut shard_ids: Vec<ShardId> = hits.iter().map(|h| h.shard_id).collect();
            shard_ids.sort_unstable();
            shard_ids.dedup();
            shard_ids
        }
    };

    let mut out = Vec::with_capacity(shard_ids.len());
    for shard_id in shard_ids {
        let entry = shards
            .iter()
            .find(|s| s.shard_id == shard_id)
            .ok_or(RouterError::ShardNotRegistered)?;
        let shard_hits = match &hits {
            SeedHits::Vertices(hits) => SeedHits::Vertices(
                hits.iter()
                    .filter(|h| h.shard_id == shard_id)
                    .cloned()
                    .collect(),
            ),
            SeedHits::Edges(hits) => SeedHits::Edges(
                hits.iter()
                    .filter(|h| h.shard_id == shard_id)
                    .cloned()
                    .collect(),
            ),
        };
        out.push(SeedRouting {
            shard_id,
            graph_canister: entry.graph_canister,
            hits: shard_hits,
            anchor: Some(anchor.clone()),
        });
    }
    Ok(out)
}
