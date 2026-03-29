use candid::Principal;
use gleaph_types::{AccessLevel, GraphConfig, GraphInfo};

use crate::state::{REGISTRY, create_record};

/// Provisions a graph canister and registers it for the caller.
pub async fn create_graph(caller: Principal, config: GraphConfig) -> Result<GraphInfo, String> {
    let provisioned_canister = provision_graph_canister(
        caller,
        config.initial_vertex_capacity,
        config.initial_edge_capacity,
    )
    .await;
    let canister_id =
        provisioned_canister.ok_or_else(|| "failed to provision graph canister".to_string())?;
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        let id = r.next_id;
        r.next_id += 1;
        let mut rec = create_record(caller, config);
        rec.info.id = id;
        rec.info.canister_id = Some(canister_id);
        let info = rec.info.clone();
        r.graphs.insert(id, rec);
        Ok(info)
    })
}

/// Deletes a graph owned by the caller and deprovisions its canister when present.
pub async fn delete_graph(caller: Principal, id: u64) -> bool {
    let canister_id = REGISTRY.with(|r| {
        let r = r.borrow();
        let graph = r.graphs.get(&id)?;
        if graph.info.owner != caller {
            return None;
        }
        graph.info.canister_id
    });
    if let Some(canister_id) = canister_id
        && !deprovision_graph_canister(canister_id).await
    {
        return false;
    }
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        if r.graphs.get(&id).is_some_and(|g| g.info.owner != caller) {
            return false;
        }
        r.graphs.remove(&id).is_some()
    })
}

/// Lists graphs visible to the caller through ownership or ACL membership.
pub fn list_graphs(caller: Principal) -> Vec<GraphInfo> {
    REGISTRY.with(|r| {
        r.borrow()
            .graphs
            .values()
            .filter(|g| g.info.owner == caller || g.acl.contains_key(&caller))
            .map(|g| g.info.clone())
            .collect()
    })
}

/// Grants or updates access for a principal on a graph if the caller is authorized.
pub fn grant_access(
    caller: Principal,
    graph_id: u64,
    principal: Principal,
    level: AccessLevel,
) -> bool {
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        if let Some(graph) = r.graphs.get_mut(&graph_id) {
            let is_admin = matches!(graph.acl.get(&caller), Some(AccessLevel::Admin));
            if graph.info.owner != caller && !is_admin {
                return false;
            }
            graph.acl.insert(principal, level);
            true
        } else {
            false
        }
    })
}

#[cfg(target_arch = "wasm32")]
async fn provision_graph_canister(
    caller: Principal,
    initial_vertex_capacity: u32,
    initial_edge_capacity: u64,
) -> Option<Principal> {
    use candid::encode_args;
    use ic_cdk::management_canister::{
        CanisterIdRecord, CanisterInstallMode, CanisterSettings, ChunkHash, ClearChunkStoreArgs,
        CreateCanisterArgs, InstallChunkedCodeArgs, InstallCodeArgs, UploadChunkArgs,
        clear_chunk_store, create_canister_with_extra_cycles, install_chunked_code, install_code,
        upload_chunk,
    };
    use sha2::{Digest, Sha256};
    // Chunked installs of large debug wasm modules can require substantially more cycles than the
    // small payload `install_code` path. Keep a larger balance on the child canister so registry
    // provisioning works in local PocketIC/debug builds too.
    const EXTRA_CYCLES_AFTER_CREATE: u128 = 500_000_000_000;
    const INSTALL_CODE_PAYLOAD_SOFT_LIMIT: usize = 9 * 1024 * 1024;
    const WASM_CHUNK_SIZE: usize = 900 * 1024;
    static GRAPH_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gleaph_graph.wasm"));

    let settings = CanisterSettings {
        controllers: Some(vec![caller, ic_cdk::api::canister_self()]),
        compute_allocation: None,
        memory_allocation: None,
        freezing_threshold: None,
        reserved_cycles_limit: None,
        log_visibility: None,
        wasm_memory_limit: None,
        wasm_memory_threshold: None,
        environment_variables: None,
    };

    let canister_id = match create_canister_with_extra_cycles(
        &CreateCanisterArgs {
            settings: Some(settings),
        },
        EXTRA_CYCLES_AFTER_CREATE,
    )
    .await
    {
        Ok(record) => record.canister_id,
        Err(e) => {
            ic_cdk::println!("create_canister failed: {:?}", e);
            return None;
        }
    };

    if GRAPH_WASM.is_empty() {
        ic_cdk::println!(
            "graph wasm artifact not embedded; refusing to return uninstalled canister"
        );
        let _ = deprovision_graph_canister(canister_id).await;
        return None;
    }

    let init_arg = match encode_args((Some(initial_vertex_capacity), Some(initial_edge_capacity))) {
        Ok(arg) => arg,
        Err(e) => {
            ic_cdk::println!("encode graph init arg failed: {:?}", e);
            let _ = deprovision_graph_canister(canister_id).await;
            return None;
        }
    };

    let install_result = if GRAPH_WASM.len() <= INSTALL_CODE_PAYLOAD_SOFT_LIMIT {
        let install_args = InstallCodeArgs {
            mode: CanisterInstallMode::Install,
            canister_id,
            wasm_module: GRAPH_WASM.to_vec(),
            arg: init_arg.clone(),
        };
        install_code(&install_args).await
    } else {
        ic_cdk::println!(
            "graph wasm is {} bytes; using chunked install for {}",
            GRAPH_WASM.len(),
            canister_id
        );

        let _ = clear_chunk_store(&CanisterIdRecord { canister_id }).await;

        let mut chunk_hashes: Vec<ChunkHash> = Vec::new();
        for chunk in GRAPH_WASM.chunks(WASM_CHUNK_SIZE) {
            match upload_chunk(&UploadChunkArgs {
                canister_id,
                chunk: chunk.to_vec(),
            })
            .await
            {
                Ok(hash) => chunk_hashes.push(hash),
                Err(e) => {
                    ic_cdk::println!("upload_chunk failed for {}: {:?}", canister_id, e);
                    let _ = clear_chunk_store(&ClearChunkStoreArgs { canister_id }).await;
                    let _ = deprovision_graph_canister(canister_id).await;
                    return None;
                }
            }
        }

        let wasm_hash = Sha256::digest(GRAPH_WASM).to_vec();
        let result = install_chunked_code(&InstallChunkedCodeArgs {
            mode: CanisterInstallMode::Install,
            target_canister: canister_id,
            store_canister: None,
            chunk_hashes_list: chunk_hashes,
            wasm_module_hash: wasm_hash,
            arg: init_arg,
        })
        .await;
        let _ = clear_chunk_store(&CanisterIdRecord { canister_id }).await;
        result
    };
    if let Err(e) = install_result {
        ic_cdk::println!("install graph code failed for {}: {:?}", canister_id, e);
        let _ = deprovision_graph_canister(canister_id).await;
        return None;
    }

    Some(canister_id)
}

#[cfg(not(target_arch = "wasm32"))]
async fn provision_graph_canister(
    _caller: Principal,
    _initial_vertex_capacity: u32,
    _initial_edge_capacity: u64,
) -> Option<Principal> {
    None
}

#[cfg(target_arch = "wasm32")]
async fn deprovision_graph_canister(canister_id: Principal) -> bool {
    use ic_cdk::management_canister::{
        DeleteCanisterArgs, StopCanisterArgs, delete_canister, stop_canister,
    };

    let stop_arg = StopCanisterArgs { canister_id };
    if let Err(e) = stop_canister(&stop_arg).await {
        ic_cdk::println!("stop_canister failed for {}: {:?}", canister_id, e);
        return false;
    }
    let delete_arg = DeleteCanisterArgs { canister_id };
    if let Err(e) = delete_canister(&delete_arg).await {
        ic_cdk::println!("delete_canister failed for {}: {:?}", canister_id, e);
        return false;
    }
    true
}

#[cfg(not(target_arch = "wasm32"))]
async fn deprovision_graph_canister(_canister_id: Principal) -> bool {
    true
}

#[cfg(test)]
mod tests {
    // Non-wasm provision path remains stubbed in tests; integration coverage lives in PocketIC e2e.
}
