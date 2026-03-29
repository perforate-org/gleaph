use std::cell::RefCell;
use std::collections::BTreeMap;

use candid::{CandidType, Principal};
use gleaph_types::{AccessLevel, GraphConfig, GraphInfo};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
/// Registry entry holding graph metadata and its access control list.
pub struct GraphRecord {
    pub info: GraphInfo,
    pub acl: BTreeMap<Principal, AccessLevel>,
}

#[derive(Default, Clone, Debug, CandidType, Serialize, Deserialize)]
/// Per-tenant registry state stored in canister memory.
pub struct TenantRegistry {
    pub next_id: u64,
    pub graphs: BTreeMap<u64, GraphRecord>,
}

thread_local! {
    pub static REGISTRY: RefCell<TenantRegistry> = RefCell::new(TenantRegistry::default());
}

/// Creates a new graph registry record owned by `caller`.
pub fn create_record(caller: Principal, cfg: GraphConfig) -> GraphRecord {
    GraphRecord {
        info: GraphInfo {
            id: 0,
            name: cfg.name,
            canister_id: None,
            owner: caller,
            max_vertices: cfg.initial_vertex_capacity,
        },
        acl: BTreeMap::new(),
    }
}
