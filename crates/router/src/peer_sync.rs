//! Push sibling graph ACL to shards when the router registers or unregisters a shard.
//!
//! Peer graph ACL stable was removed; cross-shard expand is deferred until a follow-up design.

use candid::Principal;

/// After `admin_register_shard`, tell existing shards about the newcomer and seed the newcomer.
pub async fn sync_peers_after_shard_register(
    _logical_graph_name: &str,
    _new_graph_canister: Principal,
) -> Result<(), String> {
    Ok(())
}

/// After `admin_unregister_shard`, drop the departing graph from sibling ACLs and vice versa.
pub async fn sync_peers_after_shard_unregister(
    _departing_graph_canister: Principal,
    _siblings: &[Principal],
) -> Result<(), String> {
    Ok(())
}
