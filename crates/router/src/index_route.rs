//! Index routing formula helpers (ADR 0019 S4).

use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;

/// Authoritative routing formula: `group_index = shard_id / index_group_size`.
pub fn index_group_index(shard_id: ShardId, index_group_size: u32) -> Option<usize> {
    if index_group_size == 0 {
        return None;
    }
    usize::try_from(shard_id.raw() / index_group_size).ok()
}

/// Resolve index canister by formula and per-graph cluster config.
pub fn index_canister_for_graph_shard(
    shard_id: ShardId,
    index_group_size: u32,
    index_cluster: &[Principal],
) -> Option<Principal> {
    let group = index_group_index(shard_id, index_group_size)?;
    let principal = *index_cluster.get(group)?;
    if principal == Principal::anonymous() {
        return None;
    }
    Some(principal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    #[test]
    fn index_group_index_uses_formula() {
        assert_eq!(index_group_index(ShardId::new(0), 2), Some(0));
        assert_eq!(index_group_index(ShardId::new(1), 2), Some(0));
        assert_eq!(index_group_index(ShardId::new(2), 2), Some(1));
        assert_eq!(index_group_index(ShardId::new(9), 3), Some(3));
    }

    #[test]
    fn index_canister_for_graph_shard_uses_group_index() {
        let cluster = vec![index_principal(3), index_principal(5)];
        assert_eq!(
            index_canister_for_graph_shard(ShardId::new(0), 2, &cluster),
            Some(index_principal(3))
        );
        assert_eq!(
            index_canister_for_graph_shard(ShardId::new(1), 2, &cluster),
            Some(index_principal(3))
        );
        assert_eq!(
            index_canister_for_graph_shard(ShardId::new(2), 2, &cluster),
            Some(index_principal(5))
        );
    }

    #[test]
    fn index_canister_for_graph_shard_rejects_invalid_config() {
        let shards = vec![index_principal(2)];
        assert_eq!(
            index_canister_for_graph_shard(ShardId::new(1), 0, &shards),
            None
        );
        assert_eq!(
            index_canister_for_graph_shard(ShardId::new(3), 2, &shards),
            None
        );
    }
}
