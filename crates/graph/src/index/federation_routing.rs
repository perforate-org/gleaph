//! Federated shard listing and local vertex id helpers (router registry client).

use candid::Principal;
#[cfg(any(not(target_family = "wasm"), test))]
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{LocalVertexId, RouterError, ShardRegistryEntry};
use gleaph_graph_kernel::index::IndexedPropertyCatalog;
use ic_stable_lara::VertexId;
use std::cell::RefCell;
use std::fmt;

#[derive(Clone, Debug)]
pub enum FederationRoutingError {
    Call(String),
    Rejected(RouterError),
}

impl fmt::Display for FederationRoutingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Call(msg) => write!(f, "router federation call failed: {msg}"),
            Self::Rejected(err) => write!(f, "router rejected federation call: {err:?}"),
        }
    }
}

impl std::error::Error for FederationRoutingError {}

thread_local! {
    static NATIVE_TEST_SHARDS: RefCell<Vec<ShardRegistryEntry>> =
        const { RefCell::new(Vec::new()) };
}

pub async fn list_shards_for_graph(
    router_canister: Principal,
    logical_graph_name: &str,
) -> Result<Vec<ShardRegistryEntry>, FederationRoutingError> {
    #[cfg(target_family = "wasm")]
    {
        let shards: Result<Vec<ShardRegistryEntry>, RouterError> =
            super::router_call::call_router1(
                router_canister,
                "list_shards_for_graph",
                logical_graph_name.to_string(),
            )
            .await
            .map_err(FederationRoutingError::Call)?;
        shards.map_err(FederationRoutingError::Rejected)
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = router_canister;
        Ok(NATIVE_TEST_SHARDS.with_borrow(|shards| {
            let graph_id = native_test_graph_id(logical_graph_name);
            shards
                .iter()
                .filter(|entry| Some(entry.graph_id) == graph_id)
                .cloned()
                .collect()
        }))
    }
}

/// Fetches the router-sourced indexed-property catalog for this shard's graph
/// (ADR 0023 D1/D3/P2). The async maintenance tick consults it ephemerally so the
/// timer-driven compaction re-keys postings without any shard-local index state.
pub async fn fetch_indexed_catalog(
    router_canister: Principal,
    logical_graph_name: &str,
) -> Result<IndexedPropertyCatalog, FederationRoutingError> {
    #[cfg(target_family = "wasm")]
    {
        let catalog: Result<IndexedPropertyCatalog, RouterError> =
            super::router_call::call_router1(
                router_canister,
                "indexed_property_catalog",
                logical_graph_name.to_string(),
            )
            .await
            .map_err(FederationRoutingError::Call)?;
        catalog.map_err(FederationRoutingError::Rejected)
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let _ = (router_canister, logical_graph_name);
        Ok(NATIVE_TEST_CATALOG.with_borrow(|catalog| catalog.clone()))
    }
}

#[cfg(not(target_family = "wasm"))]
thread_local! {
    static NATIVE_TEST_CATALOG: RefCell<IndexedPropertyCatalog> =
        RefCell::new(IndexedPropertyCatalog::default());
}

/// Installs the catalog returned by [`fetch_indexed_catalog`] on native builds
/// (unit tests only).
#[cfg(test)]
pub fn native_test_set_indexed_catalog(catalog: IndexedPropertyCatalog) {
    NATIVE_TEST_CATALOG.with_borrow_mut(|slot| *slot = catalog);
}

#[cfg(not(target_family = "wasm"))]
fn native_test_graph_id(logical_graph_name: &str) -> Option<GraphId> {
    NATIVE_TEST_GRAPH_NAMES.with_borrow(|names| names.get(logical_graph_name).copied())
}

/// Registers a logical graph name for native shard listing (unit tests only).
#[cfg(test)]
pub fn native_test_register_graph_name(name: &str, graph_id: GraphId) {
    NATIVE_TEST_GRAPH_NAMES.with_borrow_mut(|names| {
        names.insert(name.to_owned(), graph_id);
    });
}

#[cfg(not(target_family = "wasm"))]
thread_local! {
    static NATIVE_TEST_GRAPH_NAMES: RefCell<std::collections::BTreeMap<String, GraphId>> =
        const { RefCell::new(std::collections::BTreeMap::new()) };
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

pub fn local_vertex_id_raw(vertex_id: VertexId) -> LocalVertexId {
    u32::from_le_bytes(vertex_id.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::ShardId;

    #[test]
    fn list_shards_for_graph_uses_native_registry() {
        let graph_id = GraphId::from_raw(1);
        native_test_register_graph_name("tenant.main", graph_id);
        native_test_register_shard(ShardRegistryEntry {
            shard_id: ShardId::new(0),
            graph_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            graph_id,
            registered_at_ns: 0,
            index_attached: true,
            vector_index_canister: None,
            vector_index_attached: false,
            typed_seed_batch_v1: false,
        });
        native_test_register_shard(ShardRegistryEntry {
            shard_id: ShardId::new(1),
            graph_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            graph_id,
            registered_at_ns: 0,
            index_attached: true,
            vector_index_canister: None,
            vector_index_attached: false,
            typed_seed_batch_v1: false,
        });

        let listed = pollster::block_on(list_shards_for_graph(
            Principal::management_canister(),
            "tenant.main",
        ))
        .expect("list");
        assert_eq!(listed.len(), 2);
    }
}
