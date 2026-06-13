use candid::Principal;
use candid::{CandidType, Decode, Encode};
use gleaph_graph_kernel::entry::GraphId;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum GraphStatus {
    Active,
    ReadOnly,
    Deprecated,
    Deleting,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ProvisioningState {
    None,
    Pending { request_id: String },
    Failed { request_id: String, reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct GraphRegistryEntry {
    pub graph_id: GraphId,
    pub graph_name: String,
    pub canister_id: Principal,
    pub owner: Principal,
    pub admins: BTreeSet<Principal>,
    pub status: GraphStatus,
    pub version: u64,
    pub updated_at_ns: u64,
    pub provisioning_state: ProvisioningState,
    /// When true, this graph is the caller's HOME graph (ADR 0011 §1.3 option B).
    pub is_home: bool,
}

impl Storable for GraphRegistryEntry {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode GraphRegistryEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode GraphRegistryEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), GraphRegistryEntry).expect("decode GraphRegistryEntry")
    }

    const BOUND: Bound = Bound::Unbounded;
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum GraphRegistryError {
    #[error("graph `{0}` not found")]
    NotFound(String),
    #[error("forbidden")]
    Forbidden,
    #[error("graph `{0}` already exists")]
    Conflict(String),
    #[error("graph unavailable")]
    Unavailable,
}

pub trait GraphRegistryStore {
    fn resolve_graph(
        &self,
        graph_name: &str,
        caller: Principal,
    ) -> Result<GraphRegistryEntry, GraphRegistryError>;

    fn register_graph(&mut self, entry: GraphRegistryEntry) -> Result<(), GraphRegistryError>;
}

#[derive(Clone, Debug, Default)]
pub struct InMemoryGraphRegistry {
    entries: BTreeMap<String, GraphRegistryEntry>,
}

impl InMemoryGraphRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GraphRegistryStore for InMemoryGraphRegistry {
    fn resolve_graph(
        &self,
        graph_name: &str,
        caller: Principal,
    ) -> Result<GraphRegistryEntry, GraphRegistryError> {
        let entry = self
            .entries
            .get(graph_name)
            .cloned()
            .ok_or_else(|| GraphRegistryError::NotFound(graph_name.to_owned()))?;
        if caller != entry.owner && !entry.admins.contains(&caller) {
            return Err(GraphRegistryError::Forbidden);
        }
        if !matches!(entry.status, GraphStatus::Active | GraphStatus::ReadOnly) {
            return Err(GraphRegistryError::Unavailable);
        }
        Ok(entry)
    }

    fn register_graph(&mut self, entry: GraphRegistryEntry) -> Result<(), GraphRegistryError> {
        if self.entries.contains_key(&entry.graph_name) {
            return Err(GraphRegistryError::Conflict(entry.graph_name));
        }
        self.entries.insert(entry.graph_name.clone(), entry);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_graph_checks_permissions() {
        let owner = Principal::from_text("2vxsx-fae").expect("owner");
        let other = Principal::from_text("aaaaa-aa").expect("other");
        let graph_canister =
            Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").expect("graph canister");

        let mut registry = InMemoryGraphRegistry::new();
        registry
            .register_graph(GraphRegistryEntry {
                graph_id: GraphId::from_raw(1),
                graph_name: "tenant.main".to_owned(),
                canister_id: graph_canister,
                owner,
                admins: BTreeSet::new(),
                status: GraphStatus::Active,
                version: 1,
                updated_at_ns: 0,
                provisioning_state: ProvisioningState::None,
                is_home: false,
            })
            .expect("register");

        let ok = registry
            .resolve_graph("tenant.main", owner)
            .expect("owner resolve");
        assert_eq!(ok.graph_name, "tenant.main");

        let err = registry
            .resolve_graph("tenant.main", other)
            .expect_err("non-owner should fail");
        assert_eq!(err, GraphRegistryError::Forbidden);
    }
}
