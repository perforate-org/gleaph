//! Push sibling graph ACL to shards when the router registers or unregisters a shard.

use candid::Principal;

use crate::facade::store::RouterStore;
use crate::graph_client;

/// After `admin_register_shard`, tell existing shards about the newcomer and seed the newcomer.
pub async fn sync_peers_after_shard_register(
    logical_graph_name: &str,
    new_graph_canister: Principal,
) -> Result<(), String> {
    let siblings: Vec<Principal> = RouterStore::new()
        .list_shards_for_graph(logical_graph_name)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|entry| entry.graph_canister)
        .filter(|principal| *principal != new_graph_canister)
        .collect();

    for existing in &siblings {
        graph_client::add_graph_peer(*existing, new_graph_canister).await?;
    }

    if !siblings.is_empty() {
        graph_client::bootstrap_graph_peers(new_graph_canister, siblings).await?;
    }

    Ok(())
}

/// After `admin_unregister_shard`, drop the departing graph from sibling ACLs and vice versa.
pub async fn sync_peers_after_shard_unregister(
    departing_graph_canister: Principal,
    siblings: &[Principal],
) -> Result<(), String> {
    for existing in siblings {
        graph_client::remove_graph_peer(*existing, departing_graph_canister).await?;
    }
    for sibling in siblings {
        graph_client::remove_graph_peer(departing_graph_canister, *sibling).await?;
    }
    Ok(())
}
