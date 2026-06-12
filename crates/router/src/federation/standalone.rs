//! Single-shard routing: one registry entry, local hits only.

use gleaph_graph_kernel::federation::ShardRegistryEntry;
use gleaph_graph_kernel::index::PostingHit;

use crate::facade::store::RouterStore;
use crate::federation::{SeedRouting, ShardingPolicy};
use crate::seed::IndexAnchor;
use crate::state::RouterError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StandaloneSharding;

impl ShardingPolicy for StandaloneSharding {
    fn resolve_without_anchor(
        &self,
        shards: &[ShardRegistryEntry],
    ) -> Result<Vec<SeedRouting>, RouterError> {
        let routing = resolve_standalone_routing(shards)?;
        Ok(vec![routing])
    }

    fn resolve_with_hits(
        &self,
        _store: &RouterStore,
        _logical_graph_name: &str,
        shards: &[ShardRegistryEntry],
        anchor: IndexAnchor,
        hits: &[PostingHit],
    ) -> Result<Vec<SeedRouting>, RouterError> {
        let entry = single_shard_entry(shards)?;
        let shard_hits: Vec<PostingHit> = hits
            .iter()
            .filter(|hit| hit.shard_id == entry.shard_id)
            .cloned()
            .collect();
        Ok(vec![SeedRouting {
            shard_id: entry.shard_id,
            graph_canister: entry.graph_canister,
            hits: shard_hits,
            anchor: Some(anchor),
        }])
    }
}

fn single_shard_entry(shards: &[ShardRegistryEntry]) -> Result<&ShardRegistryEntry, RouterError> {
    shards.first().filter(|_| shards.len() == 1).ok_or_else(|| {
        RouterError::InvalidArgument("no index anchor: single-shard graph required".into())
    })
}

fn resolve_standalone_routing(shards: &[ShardRegistryEntry]) -> Result<SeedRouting, RouterError> {
    let entry = single_shard_entry(shards)?;
    Ok(SeedRouting {
        shard_id: entry.shard_id,
        graph_canister: entry.graph_canister,
        hits: Vec::new(),
        anchor: None,
    })
}

#[cfg(test)]
mod tests {
    use candid::Principal;

    use gleaph_graph_kernel::federation::ShardId;

    use super::*;
    use crate::federation::ShardingPolicy;
    use crate::init::RouterInitArgs;
    use crate::types::AdminRegisterShardArgs;

    fn graph_principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    fn store_with_single_shard() -> (RouterStore, ShardRegistryEntry) {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        let entry = AdminRegisterShardArgs {
            shard_id: ShardId::new(0),
            graph_canister: graph_principal(1),
            index_canister: graph_principal(2),
            logical_graph_name: "tenant.main".into(),
        };
        futures::executor::block_on(store.admin_register_shard(admin, entry.clone()))
            .expect("register shard");
        let shards = store
            .list_shards_for_graph("tenant.main")
            .expect("list shards");
        assert_eq!(shards.len(), 1);
        (store, shards[0].clone())
    }

    #[test]
    fn without_anchor_requires_single_shard() {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            controllers: vec![],
        });
        let admin = Principal::anonymous();
        store.bootstrap_controllers(&[admin]);
        for (shard_id, graph_byte) in [(ShardId::new(0), 1u8), (ShardId::new(1), 4)] {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id,
                    graph_canister: graph_principal(graph_byte),
                    index_canister: graph_principal(2),
                    logical_graph_name: "tenant.main".into(),
                },
            ))
            .expect("register shard");
        }
        let shards = store
            .list_shards_for_graph("tenant.main")
            .expect("list shards");
        let err = StandaloneSharding
            .resolve_without_anchor(&shards)
            .expect_err("multi-shard without anchor");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
    }

    #[test]
    fn without_anchor_returns_local_shard() {
        let (store, entry) = store_with_single_shard();
        let shards = store.list_shards_for_graph("tenant.main").expect("shards");
        let routings = StandaloneSharding
            .resolve_without_anchor(&shards)
            .expect("route");
        assert_eq!(routings.len(), 1);
        assert_eq!(routings[0].shard_id, entry.shard_id);
        assert_eq!(routings[0].graph_canister, entry.graph_canister);
        assert!(routings[0].hits.is_empty());
        assert!(routings[0].anchor.is_none());
    }

    #[test]
    fn with_hits_keeps_local_shard_postings_only() {
        let (store, entry) = store_with_single_shard();
        let shards = store.list_shards_for_graph("tenant.main").expect("shards");
        let anchor = IndexAnchor::Equal(crate::seed::SeedProbe {
            variable: "u".into(),
            property: "uid".into(),
            property_id: 1,
            payload_bytes: vec![1, 2, 3],
        });
        let hits = vec![
            PostingHit {
                shard_id: entry.shard_id,
                vertex_id: 10,
            },
            PostingHit {
                shard_id: ShardId::new(99),
                vertex_id: 20,
            },
        ];
        let routings = StandaloneSharding
            .resolve_with_hits(&store, "tenant.main", &shards, anchor, &hits)
            .expect("route");
        assert_eq!(routings.len(), 1);
        assert_eq!(routings[0].hits.len(), 1);
        assert_eq!(routings[0].hits[0].vertex_id, 10);
    }
}
