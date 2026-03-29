mod api;
pub mod state;

use candid::Principal;
use ic_cdk::export_candid;
use ic_cdk_macros::{init, post_upgrade, pre_upgrade, query, update};

#[init]
/// Initializes the registry canister.
fn init() {}

#[pre_upgrade]
/// Persists registry state before a canister upgrade.
fn pre_upgrade() {
    state::REGISTRY.with(|r| {
        if let Err(e) = ic_cdk::storage::stable_save((r.borrow().clone(),)) {
            ic_cdk::trap(format!("failed to persist registry state: {e}"));
        }
    });
}

#[post_upgrade]
/// Restores registry state after a canister upgrade.
fn post_upgrade() {
    if ic_cdk::stable::stable_size() == 0 {
        state::REGISTRY.with(|r| *r.borrow_mut() = state::TenantRegistry::default());
        return;
    }
    match restore_registry() {
        Ok((registry,)) => state::REGISTRY.with(|r| *r.borrow_mut() = registry),
        Err(e) => ic_cdk::trap(format!("failed to restore registry state: {e}")),
    }
}

fn restore_registry() -> Result<(state::TenantRegistry,), String> {
    ic_cdk::storage::stable_restore::<(state::TenantRegistry,)>()
        .map_err(|e| format!("registry restore failed: {e}"))
}

#[update]
/// Creates a graph canister for the caller and registers it.
async fn create_graph(
    config: gleaph_types::GraphConfig,
) -> Result<gleaph_types::GraphInfo, String> {
    let caller = ic_cdk::api::msg_caller();
    api::create_graph(caller, config).await
}

#[update]
/// Deletes a graph if the caller is the owner.
async fn delete_graph(id: u64) -> bool {
    let caller = ic_cdk::api::msg_caller();
    api::delete_graph(caller, id).await
}

#[query]
/// Lists graphs visible to the caller.
fn list_graphs() -> Vec<gleaph_types::GraphInfo> {
    let caller = ic_cdk::api::msg_caller();
    api::list_graphs(caller)
}

#[update]
/// Grants an access level to a principal for a graph.
fn grant_access(graph_id: u64, principal: Principal, level: gleaph_types::AccessLevel) -> bool {
    let caller = ic_cdk::api::msg_caller();
    api::grant_access(caller, graph_id, principal, level)
}

export_candid!();

#[cfg(test)]
mod tests {
    use crate::state::{GraphRecord, TenantRegistry};
    use candid::{Principal, encode_args};
    use gleaph_types::GraphInfo;
    use std::collections::BTreeMap;

    fn record(id: u64) -> GraphRecord {
        GraphRecord {
            info: GraphInfo {
                id,
                name: format!("g{id}"),
                canister_id: None,
                owner: Principal::anonymous(),
                max_vertices: 16,
            },
            acl: BTreeMap::new(),
        }
    }

    #[test]
    fn candid_round_trip_current_snapshot() {
        let mut graphs = BTreeMap::new();
        graphs.insert(1, record(1));
        let registry = TenantRegistry { next_id: 2, graphs };
        let bytes = encode_args((registry.clone(),)).expect("encode current snapshot");

        let (decoded,): (TenantRegistry,) = candid::decode_args(&bytes).expect("decode current");
        assert_eq!(decoded.next_id, 2);
        assert_eq!(decoded.graphs.len(), 1);
        assert_eq!(decoded.graphs.get(&1).map(|g| g.info.id), Some(1));
    }
}
