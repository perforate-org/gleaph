//! Router placement protocol for federated graph shards.

use candid::Principal;
#[cfg(not(target_family = "wasm"))]
use gleaph_graph_kernel::federation::PhysicalVertexLocation;
use gleaph_graph_kernel::federation::{
    CommitVertexPlacementArgs, GlobalVertexId, LocalVertexId, ReleaseVertexPlacementArgs,
    RouterError, ShardId, ShardRegistryEntry, VertexPlacement,
};
use ic_stable_lara::VertexId;
use std::cell::RefCell;
use std::fmt;

#[derive(Clone, Debug)]
pub enum VertexPlacementError {
    Call(String),
    Rejected(RouterError),
}

impl fmt::Display for VertexPlacementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Call(msg) => write!(f, "router placement call failed: {msg}"),
            Self::Rejected(err) => write!(f, "router rejected placement: {err:?}"),
        }
    }
}

impl std::error::Error for VertexPlacementError {}

thread_local! {
    static NATIVE_TEST_PLACEMENTS: RefCell<
        std::collections::HashMap<GlobalVertexId, VertexPlacement>,
    > = RefCell::new(std::collections::HashMap::new());
    static NATIVE_TEST_SHARDS: RefCell<Vec<ShardRegistryEntry>> =
        const { RefCell::new(Vec::new()) };
}

pub async fn commit_vertex_placement(
    router_canister: Principal,
    args: CommitVertexPlacementArgs,
) -> Result<(), VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let result: Result<(), RouterError> =
            super::router_call::call_router1(router_canister, "commit_vertex_placement", args)
                .await
                .map_err(VertexPlacementError::Call)?;
        result.map_err(VertexPlacementError::Rejected)
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        if let Some(routing) = crate::facade::GraphStore::new().federation_routing() {
            let vertex_id = GlobalVertexId::new(routing.shard_id, args.local_vertex_id);
            NATIVE_TEST_PLACEMENTS.with_borrow_mut(|map| {
                map.insert(
                    vertex_id,
                    VertexPlacement::Active(PhysicalVertexLocation::new(
                        routing.shard_id,
                        args.local_vertex_id,
                    )),
                );
            });
        }
        Ok(())
    }
}

pub async fn list_shards_for_graph(
    router_canister: Principal,
    logical_graph_name: &str,
) -> Result<Vec<ShardRegistryEntry>, VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let shards: Result<Vec<ShardRegistryEntry>, RouterError> =
            super::router_call::call_router1(
                router_canister,
                "list_shards_for_graph",
                logical_graph_name.to_string(),
            )
            .await
            .map_err(VertexPlacementError::Call)?;
        shards.map_err(VertexPlacementError::Rejected)
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        Ok(NATIVE_TEST_SHARDS.with_borrow(|shards| {
            shards
                .iter()
                .filter(|entry| entry.logical_graph_name == logical_graph_name)
                .cloned()
                .collect()
        }))
    }
}

/// Registers a shard in the native test registry (unit tests only).
#[cfg(test)]
pub fn native_test_register_shard(entry: ShardRegistryEntry) {
    NATIVE_TEST_SHARDS.with_borrow_mut(|shards| {
        if let Some(idx) = shards.iter().position(|s| s.shard_id == entry.shard_id) {
            shards[idx] = entry;
        } else {
            shards.push(entry);
        }
    });
}

pub async fn resolve_placement(
    router_canister: Principal,
    vertex_id: GlobalVertexId,
) -> Result<VertexPlacement, VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let placement: Result<VertexPlacement, RouterError> =
            super::router_call::call_router1(router_canister, "resolve_placement", vertex_id)
                .await
                .map_err(VertexPlacementError::Call)?;
        placement.map_err(VertexPlacementError::Rejected)
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        NATIVE_TEST_PLACEMENTS
            .with_borrow(|map| map.get(&vertex_id).copied())
            .ok_or(VertexPlacementError::Rejected(RouterError::VertexNotFound))
    }
}

pub async fn release_vertex_placement(
    router_canister: Principal,
    args: ReleaseVertexPlacementArgs,
) -> Result<(), VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let result: Result<(), RouterError> =
            super::router_call::call_router1(router_canister, "release_vertex_placement", args)
                .await
                .map_err(VertexPlacementError::Call)?;
        result.map_err(VertexPlacementError::Rejected)
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        native_release_vertex_placement(args)
    }
}

#[cfg(not(target_family = "wasm"))]
fn native_release_vertex_placement(
    args: ReleaseVertexPlacementArgs,
) -> Result<(), VertexPlacementError> {
    let routing = crate::facade::GraphStore::new()
        .federation_routing()
        .ok_or(VertexPlacementError::Rejected(
            RouterError::ShardNotRegistered,
        ))?;

    let vertex_id = GlobalVertexId::new(routing.shard_id, args.local_vertex_id);
    let placement = NATIVE_TEST_PLACEMENTS
        .with_borrow(|map| map.get(&vertex_id).copied())
        .ok_or(VertexPlacementError::Rejected(RouterError::VertexNotFound))?;

    let VertexPlacement::Active(loc) = placement;
    if loc.shard_id != routing.shard_id {
        return Err(VertexPlacementError::Rejected(RouterError::Forbidden));
    }

    NATIVE_TEST_PLACEMENTS.with_borrow_mut(|map| {
        map.remove(&vertex_id);
    });
    Ok(())
}

pub fn local_vertex_id_raw(vertex_id: VertexId) -> LocalVertexId {
    u32::from_le_bytes(vertex_id.to_le_bytes())
}

/// Override authoritative placement for a global vertex (unit tests only).
#[cfg(test)]
pub fn native_test_set_active_placement(
    vertex_id: GlobalVertexId,
    location: PhysicalVertexLocation,
) {
    NATIVE_TEST_PLACEMENTS.with_borrow_mut(|map| {
        map.insert(vertex_id, VertexPlacement::Active(location));
    });
}

pub async fn resolve_global_at(
    router_canister: Principal,
    shard_id: ShardId,
    local_vertex_id: LocalVertexId,
) -> Result<Option<GlobalVertexId>, VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let vertex_id: Result<GlobalVertexId, RouterError> = super::router_call::call_router2(
            router_canister,
            "resolve_global_at",
            shard_id,
            local_vertex_id,
        )
        .await
        .map_err(VertexPlacementError::Call)?;

        match vertex_id {
            Ok(id) => Ok(Some(id)),
            Err(RouterError::VertexNotFound) => Ok(None),
            Err(err) => Err(VertexPlacementError::Rejected(err)),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        let vertex_id = GlobalVertexId::new(shard_id, local_vertex_id);
        Ok(NATIVE_TEST_PLACEMENTS
            .with_borrow(|map| map.contains_key(&vertex_id).then_some(vertex_id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::{FederationRouting, GraphStore};

    #[test]
    fn list_shards_for_graph_uses_native_registry() {
        native_test_register_shard(ShardRegistryEntry {
            shard_id: ShardId::new(0),
            graph_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            logical_graph_name: "tenant.main".into(),
            registered_at_ns: 0,
        });
        native_test_register_shard(ShardRegistryEntry {
            shard_id: ShardId::new(1),
            graph_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            logical_graph_name: "tenant.main".into(),
            registered_at_ns: 0,
        });

        let listed = pollster::block_on(list_shards_for_graph(
            Principal::management_canister(),
            "tenant.main",
        ))
        .expect("list");
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn delete_vertex_releases_router_placement() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
            }))
            .expect("routing");

        let vid = store.insert_vertex().expect("insert");
        let global = store.global_vertex_id(vid).expect("global");

        store.delete_vertex(vid).expect("delete");

        assert!(matches!(
            pollster::block_on(resolve_placement(Principal::management_canister(), global)),
            Err(VertexPlacementError::Rejected(RouterError::VertexNotFound))
        ));
    }
}
