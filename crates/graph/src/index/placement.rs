//! Router placement protocol for federated graph shards.

use candid::Principal;
use gleaph_graph_kernel::federation::{
    BeginVertexMigrationArgs, CommitVertexPlacementArgs, FinishVertexMigrationArgs, LocalVertexId,
    LogicalVertexId, PhysicalPlacementKey, PhysicalVertexLocation, ReleaseLogicalVertexArgs,
    RouterError, ShardId, ShardRegistryEntry, VertexPlacement,
};
use ic_stable_lara::VertexId;
use std::cell::{Cell, RefCell};
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
    static NATIVE_TEST_LOGICAL_COUNTER: Cell<u64> = const { Cell::new(0) };
    static NATIVE_TEST_PENDING_LOGICAL: Cell<Option<LogicalVertexId>> = const { Cell::new(None) };
    static NATIVE_TEST_PLACEMENT_BY_PHYSICAL: RefCell<
        std::collections::HashMap<PhysicalPlacementKey, LogicalVertexId>,
    > = RefCell::new(std::collections::HashMap::new());
    static NATIVE_TEST_PLACEMENTS: RefCell<
        std::collections::HashMap<LogicalVertexId, VertexPlacement>,
    > = RefCell::new(std::collections::HashMap::new());
    static NATIVE_TEST_MIGRATION_COUNTER: Cell<u64> = const { Cell::new(0) };
    static NATIVE_TEST_SHARDS: RefCell<Vec<ShardRegistryEntry>> =
        const { RefCell::new(Vec::new()) };
}

pub async fn allocate_logical_vertex_id(
    router_canister: Principal,
) -> Result<LogicalVertexId, VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let logical: Result<LogicalVertexId, RouterError> =
            super::router_call::call_router0(router_canister, "allocate_logical_vertex_id")
                .await
                .map_err(VertexPlacementError::Call)?;
        return logical.map_err(VertexPlacementError::Rejected);
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        let logical = NATIVE_TEST_LOGICAL_COUNTER.with(|c| {
            let next = c.get().saturating_add(1);
            c.set(next);
            next
        });
        NATIVE_TEST_PENDING_LOGICAL.with(|p| p.set(Some(logical)));
        Ok(logical)
    }
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
        return result.map_err(VertexPlacementError::Rejected);
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        let pending = NATIVE_TEST_PENDING_LOGICAL.with(|p| p.take()).ok_or(
            VertexPlacementError::Rejected(RouterError::UnallocatedLogicalVertex),
        )?;
        if pending != args.logical_vertex_id {
            return Err(VertexPlacementError::Rejected(
                RouterError::UnallocatedLogicalVertex,
            ));
        }
        if let Some(routing) = crate::facade::GraphStore::new().federation_routing() {
            let physical = PhysicalPlacementKey::new(routing.shard_id, args.local_vertex_id);
            NATIVE_TEST_PLACEMENT_BY_PHYSICAL.with_borrow_mut(|map| {
                map.insert(physical, args.logical_vertex_id);
            });
            NATIVE_TEST_PLACEMENTS.with_borrow_mut(|map| {
                map.insert(
                    args.logical_vertex_id,
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
        return shards.map_err(VertexPlacementError::Rejected);
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
    logical_vertex_id: LogicalVertexId,
) -> Result<VertexPlacement, VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let placement: Result<VertexPlacement, RouterError> = super::router_call::call_router1(
            router_canister,
            "resolve_placement",
            logical_vertex_id,
        )
        .await
        .map_err(VertexPlacementError::Call)?;
        return placement.map_err(VertexPlacementError::Rejected);
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        NATIVE_TEST_PLACEMENTS
            .with_borrow(|map| map.get(&logical_vertex_id).copied())
            .ok_or(VertexPlacementError::Rejected(RouterError::VertexNotFound))
    }
}

pub async fn begin_vertex_migration(
    router_canister: Principal,
    args: BeginVertexMigrationArgs,
) -> Result<(), VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let result: Result<(), RouterError> =
            super::router_call::call_router1(router_canister, "begin_vertex_migration", args)
                .await
                .map_err(VertexPlacementError::Call)?;
        return result.map_err(VertexPlacementError::Rejected);
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        native_begin_vertex_migration(args)
    }
}

pub async fn release_logical_vertex_placement(
    router_canister: Principal,
    args: ReleaseLogicalVertexArgs,
) -> Result<(), VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let result: Result<(), RouterError> = super::router_call::call_router1(
            router_canister,
            "release_logical_vertex_placement",
            args,
        )
        .await
        .map_err(VertexPlacementError::Call)?;
        return result.map_err(VertexPlacementError::Rejected);
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        native_release_logical_vertex_placement(args)
    }
}

pub async fn finish_vertex_migration(
    router_canister: Principal,
    args: FinishVertexMigrationArgs,
) -> Result<(), VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let result: Result<(), RouterError> =
            super::router_call::call_router1(router_canister, "finish_vertex_migration", args)
                .await
                .map_err(VertexPlacementError::Call)?;
        return result.map_err(VertexPlacementError::Rejected);
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        native_finish_vertex_migration(args)
    }
}

#[cfg(not(target_family = "wasm"))]
fn native_release_logical_vertex_placement(
    args: ReleaseLogicalVertexArgs,
) -> Result<(), VertexPlacementError> {
    let routing = crate::facade::GraphStore::new()
        .federation_routing()
        .ok_or(VertexPlacementError::Rejected(
            RouterError::ShardNotRegistered,
        ))?;

    let placement = NATIVE_TEST_PLACEMENTS
        .with_borrow(|map| map.get(&args.logical_vertex_id).copied())
        .ok_or(VertexPlacementError::Rejected(RouterError::VertexNotFound))?;

    let VertexPlacement::Active(loc) = placement else {
        return Err(VertexPlacementError::Rejected(RouterError::Forbidden));
    };
    if loc.shard_id != routing.shard_id {
        return Err(VertexPlacementError::Rejected(RouterError::Forbidden));
    }

    let physical = PhysicalPlacementKey::new(loc.shard_id, loc.local_vertex_id);
    NATIVE_TEST_PLACEMENT_BY_PHYSICAL.with_borrow_mut(|map| {
        map.remove(&physical);
    });
    NATIVE_TEST_PLACEMENTS.with_borrow_mut(|map| {
        map.remove(&args.logical_vertex_id);
    });
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
fn native_begin_vertex_migration(
    args: BeginVertexMigrationArgs,
) -> Result<(), VertexPlacementError> {
    let routing = crate::facade::GraphStore::new()
        .federation_routing()
        .ok_or(VertexPlacementError::Rejected(
            RouterError::ShardNotRegistered,
        ))?;

    let placement = NATIVE_TEST_PLACEMENTS
        .with_borrow(|map| map.get(&args.logical_vertex_id).copied())
        .ok_or(VertexPlacementError::Rejected(RouterError::VertexNotFound))?;

    let VertexPlacement::Active(source) = placement else {
        return Err(VertexPlacementError::Rejected(RouterError::VertexMigrating));
    };
    if source.shard_id != routing.shard_id {
        return Err(VertexPlacementError::Rejected(RouterError::Forbidden));
    }
    if source.shard_id == args.destination_shard_id {
        return Err(VertexPlacementError::Rejected(
            RouterError::InvalidMigrationState("destination shard must differ from source".into()),
        ));
    }

    let epoch = NATIVE_TEST_MIGRATION_COUNTER.with(|c| {
        let next = c.get().saturating_add(1);
        c.set(next);
        next
    });

    NATIVE_TEST_PLACEMENTS.with_borrow_mut(|map| {
        map.insert(
            args.logical_vertex_id,
            VertexPlacement::Migrating {
                epoch,
                source,
                destination_shard_id: args.destination_shard_id,
            },
        );
    });
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
fn native_finish_vertex_migration(
    args: FinishVertexMigrationArgs,
) -> Result<(), VertexPlacementError> {
    let routing = crate::facade::GraphStore::new()
        .federation_routing()
        .ok_or(VertexPlacementError::Rejected(
            RouterError::ShardNotRegistered,
        ))?;

    let placement = NATIVE_TEST_PLACEMENTS
        .with_borrow(|map| map.get(&args.logical_vertex_id).copied())
        .ok_or(VertexPlacementError::Rejected(RouterError::VertexNotFound))?;

    let VertexPlacement::Migrating {
        source,
        destination_shard_id,
        ..
    } = placement
    else {
        return Err(VertexPlacementError::Rejected(
            RouterError::VertexNotMigrating,
        ));
    };
    if destination_shard_id != routing.shard_id {
        return Err(VertexPlacementError::Rejected(RouterError::Forbidden));
    }

    let destination =
        PhysicalVertexLocation::new(routing.shard_id, args.destination_local_vertex_id);
    NATIVE_TEST_PLACEMENT_BY_PHYSICAL.with_borrow_mut(|map| {
        map.remove(&PhysicalPlacementKey::new(
            source.shard_id,
            source.local_vertex_id,
        ));
        map.insert(
            PhysicalPlacementKey::new(destination.shard_id, destination.local_vertex_id),
            args.logical_vertex_id,
        );
    });
    NATIVE_TEST_PLACEMENTS.with_borrow_mut(|map| {
        map.insert(args.logical_vertex_id, VertexPlacement::Active(destination));
    });
    Ok(())
}

pub fn local_vertex_id_raw(vertex_id: VertexId) -> LocalVertexId {
    u32::from_le_bytes(vertex_id.to_le_bytes())
}

/// Test-only registry for federated index materialization without a router canister.
#[cfg(test)]
pub fn native_test_register_physical_placement(
    shard_id: ShardId,
    local_vertex_id: LocalVertexId,
    logical_vertex_id: LogicalVertexId,
) {
    NATIVE_TEST_PLACEMENT_BY_PHYSICAL.with_borrow_mut(|map| {
        map.insert(
            PhysicalPlacementKey::new(shard_id, local_vertex_id),
            logical_vertex_id,
        );
    });
}

pub async fn resolve_logical_at(
    router_canister: Principal,
    shard_id: ShardId,
    local_vertex_id: LocalVertexId,
) -> Result<Option<LogicalVertexId>, VertexPlacementError> {
    #[cfg(target_family = "wasm")]
    {
        let logical: Result<LogicalVertexId, RouterError> = super::router_call::call_router2(
            router_canister,
            "resolve_logical_at",
            shard_id,
            local_vertex_id,
        )
        .await
        .map_err(VertexPlacementError::Call)?;

        return match logical {
            Ok(id) => Ok(Some(id)),
            Err(RouterError::VertexNotFound) => Ok(None),
            Err(err) => Err(VertexPlacementError::Rejected(err)),
        };
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        Ok(NATIVE_TEST_PLACEMENT_BY_PHYSICAL.with(|map| {
            map.borrow()
                .get(&PhysicalPlacementKey::new(shard_id, local_vertex_id))
                .copied()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::mutation_executor::GraphMutationExecutor;
    use crate::facade::{FederationRouting, GraphStore, GraphStoreError};
    use gleaph_gql::Value;

    #[test]
    fn list_shards_for_graph_uses_native_registry() {
        native_test_register_shard(ShardRegistryEntry {
            shard_id: 7,
            graph_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            logical_graph_name: "tenant.main".into(),
            registered_at_ns: 0,
        });
        native_test_register_shard(ShardRegistryEntry {
            shard_id: 9,
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
                shard_id: 7,
            }))
            .expect("routing");

        let vid = store.insert_vertex().expect("insert");
        let logical = store.logical_vertex_id(vid).expect("logical");

        store.delete_vertex(vid).expect("delete");

        assert!(matches!(
            pollster::block_on(resolve_placement(Principal::management_canister(), logical)),
            Err(VertexPlacementError::Rejected(RouterError::VertexNotFound))
        ));
        assert!(store.logical_vertex_id(vid).is_none());
    }

    #[test]
    fn migrating_source_vertex_remains_writable_on_graph_shard() {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("routing");

        let vid = store
            .insert_vertex_named(["Migrating"], [("age", Value::Uint8(1))])
            .expect("insert");
        let logical = store.logical_vertex_id(vid).expect("logical");

        pollster::block_on(begin_vertex_migration(
            Principal::management_canister(),
            BeginVertexMigrationArgs {
                logical_vertex_id: logical,
                destination_shard_id: 9,
            },
        ))
        .expect("begin");

        store
            .assert_local_vertex_writable(vid)
            .expect("source writable");
    }
}
