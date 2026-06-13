//! Index canister resolution from shard registry (ADR 0010).

use std::collections::BTreeMap;

use candid::Principal;
use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};

/// Deduped index lookup targets for a logical graph, stable-sorted by Principal bytes.
pub fn resolve_index_lookup_targets(shards: &[ShardRegistryEntry]) -> Vec<Principal> {
    let mut targets: Vec<Principal> = shards
        .iter()
        .map(|entry| entry.index_canister)
        .filter(|principal| *principal != Principal::anonymous())
        .collect();
    targets.sort();
    targets.dedup();
    targets
}

/// Index canister that holds postings for `shard_id` (write/read routing).
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "ADR 0010 routing helper for future registry and ops tooling"
    )
)]
pub fn index_canister_for_shard(
    shard_id: ShardId,
    shards: &[ShardRegistryEntry],
) -> Option<Principal> {
    shards
        .iter()
        .find(|entry| entry.shard_id == shard_id)
        .map(|entry| entry.index_canister)
        .filter(|principal| *principal != Principal::anonymous())
}

/// Shard → index map used for shard-scoped index calls (`lookup_label_page`, etc.).
pub fn shard_index_canisters(shards: &[ShardRegistryEntry]) -> BTreeMap<ShardId, Principal> {
    shards
        .iter()
        .filter(|entry| entry.index_canister != Principal::anonymous())
        .map(|entry| (entry.shard_id, entry.index_canister))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use gleaph_graph_kernel::entry::GraphId;

    fn graph_principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    fn entry(shard: u32, graph: u8, index: u8) -> ShardRegistryEntry {
        ShardRegistryEntry {
            shard_id: ShardId::new(shard),
            graph_canister: graph_principal(graph),
            index_canister: graph_principal(index),
            graph_id: GraphId::from_raw(1),
            registered_at_ns: 0,
        }
    }

    #[test]
    fn resolve_targets_dedupes_shared_index() {
        let shards = vec![entry(0, 1, 2), entry(1, 3, 2)];
        assert_eq!(
            resolve_index_lookup_targets(&shards),
            vec![graph_principal(2)]
        );
    }

    #[test]
    fn resolve_targets_returns_multiple_principals_sorted() {
        let shards = vec![entry(0, 1, 5), entry(1, 2, 3)];
        let targets = resolve_index_lookup_targets(&shards);
        assert_eq!(targets.len(), 2);
        assert!(targets[0] < targets[1]);
        assert!(targets.contains(&graph_principal(3)));
        assert!(targets.contains(&graph_principal(5)));
    }

    #[test]
    fn index_canister_for_shard_uses_registry_row() {
        let shards = vec![entry(0, 1, 2), entry(1, 3, 4)];
        assert_eq!(
            index_canister_for_shard(ShardId::new(1), &shards),
            Some(graph_principal(4))
        );
        assert_eq!(index_canister_for_shard(ShardId::new(9), &shards), None);
    }

    #[test]
    fn shard_index_map_skips_anonymous() {
        let mut anonymous = entry(2, 9, 9);
        anonymous.index_canister = Principal::anonymous();
        let shards = vec![entry(0, 1, 2), anonymous];
        let map = shard_index_canisters(&shards);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&ShardId::new(0)), Some(&graph_principal(2)));
    }
}
