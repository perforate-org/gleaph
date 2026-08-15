//! Account canister public types and stable-memory encodings (ADR 0068).
//!
//! Slice 1: account enum (Personal / Org), member RBAC, lifecycle.
//! Slice 2: Router mapping (`routers`).

use candid::{CandidType, Decode, Encode, Principal};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;

/// Org account role. Ordering is meaningful: `owner >= admin >= member`.
#[repr(u8)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub enum Role {
    Member = 0,
    Admin = 1,
    Owner = 2,
}

impl Role {
    /// True when `self` is at least `min` in the hierarchy.
    pub fn at_least(self, min: Role) -> bool {
        (self as u8) >= (min as u8)
    }
}

/// A Router owned by an account. `router_id` is a logical name unique within the account.
/// An unissued Router is simply absent from the map; no separate status field is needed yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct RouterEntry {
    pub router_id: String,
    pub router_canister: Principal,
}

/// Account ownership boundary. The **enum variant is the discriminator**; no separate
/// `AccountKind` field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum Account {
    /// Single-owner account; `account_id == principal`.
    Personal {
        name: String,
        principal: Principal,
        routers: BTreeMap<String, RouterEntry>,
    },
    /// Multi-member account with an owner-independent generated id.
    Org {
        name: String,
        account_id: String,
        members: BTreeMap<Principal, Role>,
        routers: BTreeMap<String, RouterEntry>,
    },
}

impl Account {
    /// Canonical storage key (the `account_id`).
    pub fn id(&self) -> String {
        match self {
            Account::Personal { principal, .. } => principal.to_text(),
            Account::Org { account_id, .. } => account_id.clone(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Account::Personal { name, .. } => name,
            Account::Org { name, .. } => name,
        }
    }

    /// True when `caller` is an owner.
    pub fn is_owner(&self, caller: &Principal) -> bool {
        match self {
            Account::Personal { principal, .. } => caller == principal,
            Account::Org { members, .. } => members.get(caller).is_some_and(|r| *r == Role::Owner),
        }
    }

    /// True when `caller` is an owner or admin.
    pub fn is_owner_or_admin(&self, caller: &Principal) -> bool {
        match self {
            Account::Personal { principal, .. } => caller == principal,
            Account::Org { members, .. } => {
                members.get(caller).is_some_and(|r| r.at_least(Role::Admin))
            }
        }
    }

    /// True when `caller` is any member (owner/admin/member).
    pub fn is_member(&self, caller: &Principal) -> bool {
        match self {
            Account::Personal { principal, .. } => caller == principal,
            Account::Org { members, .. } => members.contains_key(caller),
        }
    }
}

impl Storable for Account {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(&AccountStableRecord::V1(self.clone())).expect("encode Account"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&AccountStableRecord::V1(self)).expect("encode Account")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), AccountStableRecord).expect("decode Account") {
            AccountStableRecord::V1(v1) => v1,
        }
    }
}

#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
pub enum AccountStableRecord {
    V1(Account),
}

/// Candid wire errors for the account lifecycle surface.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum AccountError {
    NotAuthorized,
    NotFound,
    AlreadyExists,
    AnonymousPrincipal,
    InvalidRole,
    /// Cross-canister or internal failure (e.g. Provision call).
    Message(String),
}

/// Generate an Org `account_id`. `now_ns` is injected for deterministic tests.
// ponytail: naive caller+time id; collisions are practically impossible. Upgrade to raw_rand
// if ids need to be unpredictable/opaque.
pub fn generate_org_account_id(caller: &Principal, now_ns: u64) -> String {
    format!("org-{}-{}", caller.to_text(), now_ns)
}
