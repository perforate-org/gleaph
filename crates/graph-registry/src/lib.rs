#[macro_use(concat_string)]
extern crate concat_string;

use candid::{CandidType, Principal};
use ic_cdk_macros::{post_upgrade, pre_upgrade, query, update};
#[cfg(target_arch = "wasm32")]
use ic_cdk_management_canister::{
    CanisterIdRecord, CanisterInstallMode, CanisterSettings, ChunkHash, ClearChunkStoreArgs,
    CreateCanisterArgs, DeleteCanisterArgs, InstallChunkedCodeArgs, InstallCodeArgs,
    StopCanisterArgs, UploadChunkArgs, clear_chunk_store, create_canister_with_extra_cycles,
    delete_canister, install_chunked_code, install_code, stop_canister, upload_chunk,
};
#[cfg(target_arch = "wasm32")]
use sha2::{Digest, Sha256};

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

use gleaph_gql_ic::graph_registry::{GraphStatus, ProvisioningState};

thread_local! {
    static REGISTRY: RefCell<BTreeMap<String, GraphEntry>> = const { RefCell::new(BTreeMap::new()) };
}

type RegistryStableState = Vec<(String, GraphEntry)>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct GraphEntry {
    pub graph_name: String,
    pub canister_id: Principal,
    pub owner: Principal,
    pub admins: Vec<Principal>,
    pub status: GraphStatus,
    pub version: u64,
    pub updated_at_ns: u64,
    pub provisioning_state: ProvisioningState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct GraphResolution {
    pub graph_name: String,
    pub canister_id: Principal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct RegisterGraphRequest {
    pub graph_name: String,
    pub canister_id: Principal,
    pub owner: Option<Principal>,
    pub admins: Vec<Principal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct CreateGraphRequest {
    pub graph_name: String,
    pub owner: Option<Principal>,
    pub admins: Vec<Principal>,
    pub status: Option<GraphStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct UpdateAdminsRequest {
    pub graph_name: String,
    pub admins: Vec<Principal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct TransferOwnerRequest {
    pub graph_name: String,
    pub new_owner: Principal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct RetryProvisioningRequest {
    pub graph_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReconcileGraphRequest {
    pub graph_name: String,
    pub canister_id: Option<Principal>,
    pub status: Option<GraphStatus>,
    pub provisioning_state: Option<ProvisioningState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ListGraphsResponse {
    pub items: Vec<GraphEntry>,
}

#[derive(Debug, Error, CandidType, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum RegistryError {
    #[error("graph `{0}` not found")]
    NotFound(String),
    #[error("graph `{0}` already exists")]
    Conflict(String),
    #[error("forbidden")]
    Forbidden,
    #[error("invalid graph name")]
    InvalidName,
    #[error("graph unavailable: {0}")]
    Unavailable(String),
    #[error("management canister error: {0}")]
    ManagementError(String),
}

fn validate_graph_name(name: &str) -> Result<(), RegistryError> {
    if name.is_empty() || name.len() > 128 {
        return Err(RegistryError::InvalidName);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return Err(RegistryError::InvalidName);
    }
    Ok(())
}

fn now_ns() -> u64 {
    ic_cdk::api::time()
}

fn ensure_authenticated_caller(caller: Principal) -> Result<(), RegistryError> {
    if caller == Principal::anonymous() {
        return Err(RegistryError::Forbidden);
    }
    Ok(())
}

fn ensure_controller(caller: Principal) -> Result<(), RegistryError> {
    ensure_authenticated_caller(caller)?;
    if !ic_cdk::api::is_controller(&caller) {
        return Err(RegistryError::Forbidden);
    }
    Ok(())
}

fn normalize_admins(owner: Principal, caller: Principal, admins: Vec<Principal>) -> Vec<Principal> {
    let mut normalized: BTreeSet<Principal> = admins
        .into_iter()
        .filter(|principal| *principal != Principal::anonymous())
        .collect();
    normalized.insert(owner);
    normalized.insert(caller);
    normalized.into_iter().collect()
}

fn can_read(entry: &GraphEntry, caller: Principal, caller_is_controller: bool) -> bool {
    caller_is_controller || caller == entry.owner || entry.admins.contains(&caller)
}

fn can_admin(entry: &GraphEntry, caller: Principal, caller_is_controller: bool) -> bool {
    can_read(entry, caller, caller_is_controller)
}

fn can_transfer_owner(entry: &GraphEntry, caller: Principal, caller_is_controller: bool) -> bool {
    caller_is_controller || caller == entry.owner
}

fn validate_create_graph_request(
    caller: Principal,
    caller_is_controller: bool,
    req: &CreateGraphRequest,
) -> Result<Principal, RegistryError> {
    ensure_authenticated_caller(caller)?;
    let owner = req.owner.unwrap_or(caller);
    if owner != caller && !caller_is_controller {
        return Err(RegistryError::Forbidden);
    }
    Ok(owner)
}

fn graph_unavailable_reason(entry: &GraphEntry) -> Option<String> {
    if !matches!(entry.status, GraphStatus::Active | GraphStatus::ReadOnly) {
        return Some(format!("graph status is {:?}", entry.status));
    }
    match &entry.provisioning_state {
        ProvisioningState::None => None,
        ProvisioningState::Pending { request_id } => Some(concat_string!(
            "graph provisioning is still pending (",
            request_id,
            ")"
        )),
        ProvisioningState::Failed { request_id, reason } => Some(concat_string!(
            "graph provisioning failed (",
            request_id,
            "): ",
            reason
        )),
    }
}

fn snapshot_registry_state() -> RegistryStableState {
    REGISTRY.with(|registry| {
        registry
            .borrow()
            .iter()
            .map(|(name, entry)| (name.clone(), entry.clone()))
            .collect()
    })
}

fn restore_registry_state(state: RegistryStableState) {
    REGISTRY.with(|registry| {
        let mut guard = registry.borrow_mut();
        guard.clear();
        guard.extend(state);
    });
}

fn mark_provisioning_failed(graph_name: &str, reason: String) {
    REGISTRY.with(|registry| {
        let mut guard = registry.borrow_mut();
        if let Some(entry) = guard.get_mut(graph_name) {
            let request_id = match &entry.provisioning_state {
                ProvisioningState::Pending { request_id } => request_id.clone(),
                ProvisioningState::Failed { request_id, .. } => request_id.clone(),
                ProvisioningState::None => {
                    concat_string!("failed:", graph_name, ":", now_ns().to_string())
                }
            };
            entry.provisioning_state = ProvisioningState::Failed { request_id, reason };
            entry.version += 1;
            entry.updated_at_ns = now_ns();
        }
    });
}

async fn provision_graph_canister(controllers: Vec<Principal>) -> Result<Principal, RegistryError> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = controllers;
        Err(RegistryError::ManagementError(
            "graph provisioning is only available on wasm32 canisters".to_owned(),
        ))
    }

    #[cfg(target_arch = "wasm32")]
    {
        const CREATE_CANISTER_CYCLES: u128 = 500_000_000_000;
        const INSTALL_CODE_PAYLOAD_SOFT_LIMIT: usize = 9 * 1024 * 1024;
        const WASM_CHUNK_SIZE: usize = 900 * 1024;
        static GRAPH_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gleaph_graph.wasm"));

        if GRAPH_WASM.is_empty() {
            return Err(RegistryError::ManagementError(
                "embedded graph wasm is missing; build gleaph-graph for wasm32 first".to_owned(),
            ));
        }

        let settings = CanisterSettings {
            controllers: Some(controllers),
            compute_allocation: None,
            memory_allocation: None,
            freezing_threshold: None,
            reserved_cycles_limit: None,
            log_visibility: None,
            wasm_memory_limit: None,
            wasm_memory_threshold: None,
            environment_variables: None,
        };

        let created = create_canister_with_extra_cycles(
            &CreateCanisterArgs {
                settings: Some(settings),
            },
            CREATE_CANISTER_CYCLES,
        )
        .await
        .map_err(|e| {
            RegistryError::ManagementError(concat_string!("create_canister failed: ", e))
        })?;

        let init_arg = candid::encode_args(()).map_err(|e| {
            RegistryError::ManagementError(concat_string!("encode init arg failed: ", e))
        })?;

        let install_result = if GRAPH_WASM.len() <= INSTALL_CODE_PAYLOAD_SOFT_LIMIT {
            install_code(&InstallCodeArgs {
                mode: CanisterInstallMode::Install,
                canister_id: created.canister_id,
                wasm_module: GRAPH_WASM.to_vec(),
                arg: init_arg.clone(),
            })
            .await
        } else {
            install_chunked_graph_wasm(created.canister_id, init_arg).await
        };

        if let Err(err) = install_result {
            let _ = stop_canister(&StopCanisterArgs {
                canister_id: created.canister_id,
            })
            .await;
            let _ = delete_canister(&DeleteCanisterArgs {
                canister_id: created.canister_id,
            })
            .await;
            return Err(RegistryError::ManagementError(concat_string!(
                "install graph code failed: ",
                err
            )));
        }

        return Ok(created.canister_id);

        async fn install_chunked_graph_wasm(
            canister_id: Principal,
            init_arg: Vec<u8>,
        ) -> Result<(), (ic_cdk::api::call::RejectionCode, String)> {
            static GRAPH_WASM: &[u8] =
                include_bytes!(concat!(env!("OUT_DIR"), "/gleaph_graph.wasm"));

            let _ = clear_chunk_store(&ClearChunkStoreArgs { canister_id }).await;
            let mut chunk_hashes: Vec<ChunkHash> = Vec::new();
            for chunk in GRAPH_WASM.chunks(WASM_CHUNK_SIZE) {
                match upload_chunk(&UploadChunkArgs {
                    canister_id,
                    chunk: chunk.to_vec(),
                })
                .await
                {
                    Ok(hash) => chunk_hashes.push(hash),
                    Err(err) => {
                        let _ = clear_chunk_store(&ClearChunkStoreArgs { canister_id }).await;
                        return Err(err);
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
        }
    }
}

#[query]
pub fn resolve_graph(graph_name: String) -> Result<GraphResolution, RegistryError> {
    let caller = ic_cdk::api::msg_caller();
    let caller_is_controller = ic_cdk::api::is_controller(&caller);
    REGISTRY.with(|registry| {
        let guard = registry.borrow();
        let entry = guard
            .get(&graph_name)
            .ok_or_else(|| RegistryError::NotFound(graph_name.clone()))?;
        if !can_read(entry, caller, caller_is_controller) {
            return Err(RegistryError::Forbidden);
        }
        if let Some(reason) = graph_unavailable_reason(entry) {
            return Err(RegistryError::Unavailable(reason));
        }
        Ok(GraphResolution {
            graph_name: entry.graph_name.clone(),
            canister_id: entry.canister_id,
        })
    })
}

#[query]
pub fn list_graphs() -> ListGraphsResponse {
    let caller = ic_cdk::api::msg_caller();
    let caller_is_controller = ic_cdk::api::is_controller(&caller);
    REGISTRY.with(|registry| {
        let guard = registry.borrow();
        let items = guard
            .values()
            .filter(|entry| can_read(entry, caller, caller_is_controller))
            .cloned()
            .collect();
        ListGraphsResponse { items }
    })
}

#[update]
pub fn register_graph(req: RegisterGraphRequest) -> Result<GraphEntry, RegistryError> {
    validate_graph_name(&req.graph_name)?;
    let caller = ic_cdk::api::msg_caller();
    ensure_controller(caller)?;
    let owner = req.owner.unwrap_or(caller);

    REGISTRY.with(|registry| {
        let mut guard = registry.borrow_mut();
        if guard.contains_key(&req.graph_name) {
            return Err(RegistryError::Conflict(req.graph_name.clone()));
        }
        let entry = GraphEntry {
            graph_name: req.graph_name.clone(),
            canister_id: req.canister_id,
            owner,
            admins: normalize_admins(owner, caller, req.admins),
            status: GraphStatus::Active,
            version: 1,
            updated_at_ns: now_ns(),
            provisioning_state: ProvisioningState::None,
        };
        guard.insert(req.graph_name, entry.clone());
        Ok(entry)
    })
}

#[update]
pub async fn create_graph(req: CreateGraphRequest) -> Result<GraphEntry, RegistryError> {
    validate_graph_name(&req.graph_name)?;
    let caller = ic_cdk::api::msg_caller();
    let caller_is_controller = ic_cdk::api::is_controller(&caller);
    let owner = validate_create_graph_request(caller, caller_is_controller, &req)?;
    let admins = normalize_admins(owner, caller, req.admins.clone());

    REGISTRY.with(|registry| -> Result<(), RegistryError> {
        let mut guard = registry.borrow_mut();
        if guard.contains_key(&req.graph_name) {
            return Err(RegistryError::Conflict(req.graph_name.clone()));
        }
        let pending = GraphEntry {
            graph_name: req.graph_name.clone(),
            canister_id: Principal::management_canister(),
            owner,
            admins: admins.clone(),
            status: req.status.clone().unwrap_or(GraphStatus::Active),
            version: 1,
            updated_at_ns: now_ns(),
            provisioning_state: ProvisioningState::Pending {
                request_id: concat_string!(req.graph_name, ":", now_ns().to_string()),
            },
        };
        guard.insert(req.graph_name.clone(), pending);
        Ok(())
    })?;

    let controllers = admins;

    let created_canister_id = match provision_graph_canister(controllers).await {
        Ok(id) => id,
        Err(err) => {
            mark_provisioning_failed(&req.graph_name, err.to_string());
            return Err(err);
        }
    };

    REGISTRY.with(|registry| {
        let mut guard = registry.borrow_mut();
        let entry = guard
            .get_mut(&req.graph_name)
            .ok_or_else(|| RegistryError::NotFound(req.graph_name.clone()))?;
        if !can_admin(entry, caller, caller_is_controller) {
            return Err(RegistryError::Forbidden);
        }
        entry.canister_id = created_canister_id;
        entry.provisioning_state = ProvisioningState::None;
        entry.version += 1;
        entry.updated_at_ns = now_ns();
        Ok(entry.clone())
    })
}

#[pre_upgrade]
fn canister_pre_upgrade() {
    let state = snapshot_registry_state();
    if let Err(err) = ic_cdk::storage::stable_save((state,)) {
        ic_cdk::trap(concat_string!(
            "failed to persist graph registry state: ",
            err.to_string()
        ));
    }
}

#[post_upgrade]
fn canister_post_upgrade() {
    let (state,): (RegistryStableState,) =
        ic_cdk::storage::stable_restore().unwrap_or_else(|err| {
            ic_cdk::trap(concat_string!(
                "failed to restore graph registry state: ",
                err.to_string()
            ))
        });
    restore_registry_state(state);
}

#[update]
pub fn attach_existing_graph(
    graph_name: String,
    canister_id: Principal,
) -> Result<GraphEntry, RegistryError> {
    ensure_controller(ic_cdk::api::msg_caller())?;
    register_graph(RegisterGraphRequest {
        graph_name,
        canister_id,
        owner: None,
        admins: Vec::new(),
    })
}

#[update]
pub fn deprecate_graph(graph_name: String) -> Result<GraphEntry, RegistryError> {
    let caller = ic_cdk::api::msg_caller();
    let caller_is_controller = ic_cdk::api::is_controller(&caller);
    ensure_authenticated_caller(caller)?;
    REGISTRY.with(|registry| {
        let mut guard = registry.borrow_mut();
        let entry = guard
            .get_mut(&graph_name)
            .ok_or_else(|| RegistryError::NotFound(graph_name.clone()))?;
        if !can_admin(entry, caller, caller_is_controller) {
            return Err(RegistryError::Forbidden);
        }
        entry.status = GraphStatus::Deprecated;
        entry.version += 1;
        entry.updated_at_ns = now_ns();
        Ok(entry.clone())
    })
}

#[update]
pub fn update_graph_admins(req: UpdateAdminsRequest) -> Result<GraphEntry, RegistryError> {
    let caller = ic_cdk::api::msg_caller();
    let caller_is_controller = ic_cdk::api::is_controller(&caller);
    ensure_authenticated_caller(caller)?;
    REGISTRY.with(|registry| {
        let mut guard = registry.borrow_mut();
        let entry = guard
            .get_mut(&req.graph_name)
            .ok_or_else(|| RegistryError::NotFound(req.graph_name.clone()))?;
        if !can_admin(entry, caller, caller_is_controller) {
            return Err(RegistryError::Forbidden);
        }
        entry.admins = normalize_admins(entry.owner, caller, req.admins);
        entry.version += 1;
        entry.updated_at_ns = now_ns();
        Ok(entry.clone())
    })
}

#[update]
pub fn transfer_graph_owner(req: TransferOwnerRequest) -> Result<GraphEntry, RegistryError> {
    let caller = ic_cdk::api::msg_caller();
    let caller_is_controller = ic_cdk::api::is_controller(&caller);
    ensure_authenticated_caller(caller)?;
    REGISTRY.with(|registry| {
        let mut guard = registry.borrow_mut();
        let entry = guard
            .get_mut(&req.graph_name)
            .ok_or_else(|| RegistryError::NotFound(req.graph_name.clone()))?;
        if !can_transfer_owner(entry, caller, caller_is_controller) {
            return Err(RegistryError::Forbidden);
        }
        entry.owner = req.new_owner;
        entry.admins = normalize_admins(entry.owner, caller, std::mem::take(&mut entry.admins));
        entry.version += 1;
        entry.updated_at_ns = now_ns();
        Ok(entry.clone())
    })
}

#[update]
pub async fn retry_provisioning(
    req: RetryProvisioningRequest,
) -> Result<GraphEntry, RegistryError> {
    let caller = ic_cdk::api::msg_caller();
    let caller_is_controller = ic_cdk::api::is_controller(&caller);
    ensure_authenticated_caller(caller)?;
    let (graph_name, controllers) = REGISTRY.with(|registry| {
        let mut guard = registry.borrow_mut();
        let entry = guard
            .get_mut(&req.graph_name)
            .ok_or_else(|| RegistryError::NotFound(req.graph_name.clone()))?;
        if !can_admin(entry, caller, caller_is_controller) {
            return Err(RegistryError::Forbidden);
        }
        match entry.provisioning_state {
            ProvisioningState::Failed { .. } | ProvisioningState::Pending { .. } => {}
            ProvisioningState::None => return Err(RegistryError::Conflict(req.graph_name.clone())),
        }
        entry.provisioning_state = ProvisioningState::Pending {
            request_id: concat_string!("retry:", req.graph_name, ":", now_ns().to_string()),
        };
        entry.updated_at_ns = now_ns();
        let controllers = normalize_admins(entry.owner, caller, entry.admins.clone());
        Ok((entry.graph_name.clone(), controllers))
    })?;

    let created_canister_id = match provision_graph_canister(controllers).await {
        Ok(id) => id,
        Err(err) => {
            mark_provisioning_failed(&graph_name, err.to_string());
            return Err(err);
        }
    };
    REGISTRY.with(|registry| {
        let mut guard = registry.borrow_mut();
        let entry = guard
            .get_mut(&graph_name)
            .ok_or_else(|| RegistryError::NotFound(graph_name.clone()))?;
        entry.canister_id = created_canister_id;
        entry.provisioning_state = ProvisioningState::None;
        entry.version += 1;
        entry.updated_at_ns = now_ns();
        Ok(entry.clone())
    })
}

#[update]
pub fn reconcile_graph(req: ReconcileGraphRequest) -> Result<GraphEntry, RegistryError> {
    let caller = ic_cdk::api::msg_caller();
    ensure_controller(caller)?;
    REGISTRY.with(|registry| {
        let mut guard = registry.borrow_mut();
        let entry = guard
            .get_mut(&req.graph_name)
            .ok_or_else(|| RegistryError::NotFound(req.graph_name.clone()))?;
        if let Some(canister_id) = req.canister_id {
            entry.canister_id = canister_id;
        }
        if let Some(status) = req.status {
            entry.status = status;
        }
        if let Some(state) = req.provisioning_state {
            entry.provisioning_state = state;
        }
        entry.version += 1;
        entry.updated_at_ns = now_ns();
        Ok(entry.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_name_validation() {
        assert!(validate_graph_name("tenant.main").is_ok());
        assert!(validate_graph_name("a_b-c.1").is_ok());
        assert!(validate_graph_name("").is_err());
        assert!(validate_graph_name("bad name").is_err());
    }

    #[test]
    fn resolve_graph_rejects_pending_entry() {
        let owner = Principal::from_text("2vxsx-fae").expect("owner");
        let graph_canister =
            Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").expect("graph canister");
        restore_registry_state(vec![(
            "tenant.main".to_owned(),
            GraphEntry {
                graph_name: "tenant.main".to_owned(),
                canister_id: graph_canister,
                owner,
                admins: vec![owner],
                status: GraphStatus::Active,
                version: 1,
                updated_at_ns: 0,
                provisioning_state: ProvisioningState::Pending {
                    request_id: "req-1".to_owned(),
                },
            },
        )]);

        REGISTRY.with(|registry| {
            let guard = registry.borrow();
            let entry = guard.get("tenant.main").expect("entry");
            let err = graph_unavailable_reason(entry).expect("pending should be unavailable");
            assert!(err.contains("pending"));
        });
    }

    #[test]
    fn registry_state_snapshot_round_trips() {
        let owner = Principal::from_text("2vxsx-fae").expect("owner");
        let graph_canister =
            Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").expect("graph canister");
        restore_registry_state(vec![(
            "tenant.main".to_owned(),
            GraphEntry {
                graph_name: "tenant.main".to_owned(),
                canister_id: graph_canister,
                owner,
                admins: vec![owner],
                status: GraphStatus::ReadOnly,
                version: 2,
                updated_at_ns: 123,
                provisioning_state: ProvisioningState::None,
            },
        )]);

        let snapshot = snapshot_registry_state();
        restore_registry_state(Vec::new());
        restore_registry_state(snapshot.clone());
        assert_eq!(snapshot_registry_state(), snapshot);
    }

    #[test]
    fn normalize_admins_keeps_owner_and_caller_and_filters_anonymous() {
        let owner = Principal::from_text("renrk-eyaaa-aaaaa-aaada-cai").expect("owner");
        let caller = Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").expect("caller");
        let other = Principal::from_text("rwlgt-iiaaa-aaaaa-aaaaa-cai").expect("other");

        let admins = normalize_admins(
            owner,
            caller,
            vec![other, Principal::anonymous(), owner, caller, other],
        );

        assert_eq!(admins.len(), 3);
        assert!(admins.contains(&owner));
        assert!(admins.contains(&caller));
        assert!(admins.contains(&other));
        assert!(!admins.contains(&Principal::anonymous()));
    }

    #[test]
    fn create_graph_owner_override_requires_controller() {
        let caller = Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").expect("caller");
        let other_owner = Principal::from_text("renrk-eyaaa-aaaaa-aaada-cai").expect("owner");
        let req = CreateGraphRequest {
            graph_name: "tenant.main".to_owned(),
            owner: Some(other_owner),
            admins: vec![],
            status: None,
        };

        let err = validate_create_graph_request(caller, false, &req)
            .expect_err("non-controller owner override should be rejected");
        assert_eq!(err, RegistryError::Forbidden);

        let owner = validate_create_graph_request(caller, true, &req)
            .expect("controller should be allowed to override owner");
        assert_eq!(owner, other_owner);
    }

    #[test]
    fn transfer_owner_requires_owner_or_controller() {
        let owner = Principal::from_text("2vxsx-fae").expect("owner");
        let admin = Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").expect("admin");
        let entry = GraphEntry {
            graph_name: "tenant.main".to_owned(),
            canister_id: Principal::management_canister(),
            owner,
            admins: vec![owner, admin],
            status: GraphStatus::Active,
            version: 1,
            updated_at_ns: 0,
            provisioning_state: ProvisioningState::None,
        };

        assert!(can_transfer_owner(&entry, owner, false));
        assert!(!can_transfer_owner(&entry, admin, false));
        assert!(can_transfer_owner(&entry, admin, true));
    }
}
