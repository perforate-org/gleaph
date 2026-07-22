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

    fn retain_live_shards(&mut self, live_shards: &[ShardRegistryEntry]) {
        match self {
            Self::Vertices(hits) => hits.retain(|hit| {
                live_shards
                    .iter()
                    .any(|shard| shard.shard_id == hit.shard_id)
            }),
            Self::Edges(hits) => hits.retain(|hit| {
                live_shards
                    .iter()
                    .any(|shard| shard.shard_id == hit.shard_id)
            }),
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

/// Placement for a completely-new (pure-insert) write that has no index anchor: route the whole
/// plan to the graph's **latest shard** — the live shard with the greatest graph-local `shard_id`
/// (shard ids grow densely `0..n-1`, so the maximum is the most recently added shard). The routing
/// carries no anchor and no hits, so the shard executes the plan with no seeds and creates the new
/// elements locally (ADR 0029 §6, Phase 5 contract 1). Works for single- and multi-shard graphs.
pub fn latest_shard_routing(
    shards: &[ShardRegistryEntry],
) -> Result<Vec<SeedRouting>, RouterError> {
    let entry = shards
        .iter()
        .max_by_key(|entry| entry.shard_id.raw())
        .ok_or(RouterError::ShardNotRegistered)?;
    Ok(vec![SeedRouting {
        shard_id: entry.shard_id,
        graph_canister: entry.graph_canister,
        hits: SeedHits::Vertices(Vec::new()),
        anchor: None,
    }])
}

/// Fan out one routing per distinct shard in index hits.
pub fn resolve_seed_routings_multi(
    store: &RouterStore,
    mut hits: SeedHits,
    graph_id: GraphId,
    anchor: IndexAnchor,
) -> Result<Vec<SeedRouting>, RouterError> {
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    // Index postings are derived state and can outlive a shard registration after a local
    // reinstall or a delayed repair. The Router registry is the source of truth for dispatch;
    // discard such postings at this boundary instead of routing them as unknown shards.
    hits.retain_live_shards(&shards);
    if hits.is_empty() {
        return Ok(Vec::new());
    }
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

#[cfg(test)]
mod tests {
    use candid::Principal;
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};

    use crate::federation::dispatch::{SeedHits, SeedRouting, latest_shard_routing};
    use crate::state::RouterError;

    fn shard_entry(shard_id: u32, graph_byte: u8) -> ShardRegistryEntry {
        ShardRegistryEntry {
            shard_id: ShardId::new(shard_id),
            graph_canister: Principal::self_authenticating([graph_byte; 32]),
            index_canister: Principal::anonymous(),
            graph_id: GraphId::from_raw(7),
            registered_at_ns: 0,
            index_attached: true,
            vector_index_canister: None,
            vector_index_attached: false,
            typed_seed_batch_v1: false,
        }
    }

    fn assert_routing(routing: &SeedRouting, expected_shard_id: u32, expected_canister: Principal) {
        assert_eq!(routing.shard_id, ShardId::new(expected_shard_id));
        assert_eq!(routing.graph_canister, expected_canister);
        assert!(
            matches!(&routing.hits, SeedHits::Vertices(hits) if hits.is_empty()),
            "latest-shard routing must carry empty vertex hits, got {:?}",
            routing.hits
        );
        assert!(
            routing.anchor.is_none(),
            "latest-shard routing must have no anchor, got {:?}",
            routing.anchor
        );
    }

    /// ADR 0029 §6, Phase 5 contract 1: with no live shards there is no latest shard.
    #[test]
    fn latest_shard_routing_empty_input_returns_not_registered() {
        let err = latest_shard_routing(&[]).expect_err("empty shard list must fail");
        assert!(
            matches!(err, RouterError::ShardNotRegistered),
            "unexpected error: {err:?}"
        );
    }

    /// ADR 0029 §6, Phase 5 contract 1: the only live shard is the latest shard.
    #[test]
    fn latest_shard_routing_one_shard_selects_it() {
        let shard = shard_entry(0, 1);
        let canister = shard.graph_canister;
        let routings = latest_shard_routing(&[shard]).expect("one shard routes");

        assert_eq!(routings.len(), 1);
        assert_routing(&routings[0], 0, canister);
    }

    /// ADR 0029 §6, Phase 5 contract 1: latest shard is the greatest graph-local shard id,
    /// independent of input order, and the returned canister belongs to that shard.
    #[test]
    fn latest_shard_routing_unordered_shards_chooses_greatest_id() {
        let shard0 = shard_entry(0, 1);
        let shard2 = shard_entry(2, 3);
        let shard1 = shard_entry(1, 2);
        let expected_canister = shard2.graph_canister;

        let routings = latest_shard_routing(&[shard0, shard2, shard1])
            .expect("unordered multi-shard list routes");

        assert_eq!(routings.len(), 1);
        assert_routing(&routings[0], 2, expected_canister);
    }
}
