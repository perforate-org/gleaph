use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessLevel {
    Execute,
    Read,
    Write,
    Admin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclEntry {
    pub principal: String,
    pub level: AccessLevel,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthContext {
    pub caller: Option<String>,
    pub is_controller: bool,
}

impl AuthContext {
    pub fn anonymous() -> Self {
        Self::default()
    }

    pub fn principal(principal: impl Into<String>) -> Self {
        Self {
            caller: Some(principal.into()),
            is_controller: false,
        }
    }

    pub fn controller(principal: impl Into<String>) -> Self {
        Self {
            caller: Some(principal.into()),
            is_controller: true,
        }
    }

    pub fn is_anonymous(&self) -> bool {
        self.caller.is_none()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    PlanQuery,
    ExecuteQuery,
    Mutate,
    Prepare,
    ExecutePreparedQuery,
    ExecutePreparedMutation,
    DropPrepared,
    ListPrepared,
    SetAcl,
    RemoveAcl,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlanQuery => "plan_query",
            Self::ExecuteQuery => "execute_query",
            Self::Mutate => "mutate",
            Self::Prepare => "prepare",
            Self::ExecutePreparedQuery => "execute_prepared_query",
            Self::ExecutePreparedMutation => "execute_prepared_mutation",
            Self::DropPrepared => "drop_prepared",
            Self::ListPrepared => "list_prepared",
            Self::SetAcl => "set_acl_entry",
            Self::RemoveAcl => "remove_acl_entry",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PermissionChecker {
    acl: BTreeMap<String, AccessLevel>,
}

impl PermissionChecker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_acl_entry(&mut self, principal: impl Into<String>, level: AccessLevel) {
        self.acl.insert(principal.into(), level);
    }

    pub fn remove_acl_entry(&mut self, principal: &str) -> bool {
        self.acl.remove(principal).is_some()
    }

    pub fn list_acl_entries(&self) -> Vec<AclEntry> {
        self.acl
            .iter()
            .map(|(principal, level)| AclEntry {
                principal: principal.clone(),
                level: level.clone(),
            })
            .collect()
    }

    pub fn resolve_access_level(&self, auth: &AuthContext) -> Option<AccessLevel> {
        if auth.is_controller {
            return Some(AccessLevel::Admin);
        }
        if let Some(caller) = auth.caller.as_deref()
            && let Some(level) = self.acl.get(caller) {
            return Some(level.clone());
        }
        if auth.is_anonymous() {
            return Some(AccessLevel::Execute);
        }
        None
    }

    pub fn is_allowed(&self, auth: &AuthContext, op: Operation) -> bool {
        let Some(level) = self.resolve_access_level(auth) else {
            return false;
        };
        match level {
            AccessLevel::Admin => true,
            AccessLevel::Write => matches!(
                op,
                Operation::PlanQuery
                    | Operation::ExecuteQuery
                    | Operation::Mutate
                    | Operation::ExecutePreparedQuery
                    | Operation::ExecutePreparedMutation
                    | Operation::ListPrepared
            ),
            AccessLevel::Read => matches!(
                op,
                Operation::PlanQuery
                    | Operation::ExecuteQuery
                    | Operation::ExecutePreparedQuery
                    | Operation::ListPrepared
            ),
            AccessLevel::Execute => matches!(
                op,
                Operation::ExecutePreparedQuery | Operation::ExecutePreparedMutation
            ),
        }
    }
}
