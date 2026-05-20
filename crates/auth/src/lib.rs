//! Role-based access control for Gleaph (Internet Computer graph canisters).
//!
//! ## Role ordering
//!
//! `Executor < Read < Write < Manager < Admin` (higher implies all lower capabilities).
//!
//! Principals with **no row in stable storage** are treated as [`Role::Executor`].
//!
//! Prepared-query **registration** requires [`Role::Admin`] or [`Role::Manager`] with
//! [`ManagerCapability::PREPARE_REGISTER`]. Prepared-query **execution** uses the default
//! [`Role::Executor`] for any caller without a stored row; ad-hoc GQL requires at least
//! [`Role::Read`] (write programs require [`Role::Write`]). Enforced on the **router** canister;
//! graph shards trust the router as the only GQL entrypoint.

use candid::Principal;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

bitflags::bitflags! {
    /// Extra permissions for [`Role::Manager`] (ignored for [`Role::Admin`], who has all).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ManagerCapability: u64 {
        const PREPARE_REGISTER = 1 << 0;
        const INDEX_CREATE = 1 << 1;
        const INDEX_DROP = 1 << 2;
    }
}

/// Gleaph runtime role (stable ordinal `u8`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Executor = 0,
    Read = 1,
    Write = 2,
    Manager = 3,
    Admin = 4,
}

impl Role {
    pub const fn rank(self) -> u8 {
        self as u8
    }

    pub fn from_discriminant(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Executor),
            1 => Some(Self::Read),
            2 => Some(Self::Write),
            3 => Some(Self::Manager),
            4 => Some(Self::Admin),
            _ => None,
        }
    }

    /// Whether `self` has at least the privileges of `min` (inclusion chain).
    pub fn satisfies_at_least(self, min: Role) -> bool {
        self.rank() >= min.rank()
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Executor => "Executor",
            Role::Read => "Read",
            Role::Write => "Write",
            Role::Manager => "Manager",
            Role::Admin => "Admin",
        })
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "executor" => Ok(Role::Executor),
            "read" => Ok(Role::Read),
            "write" => Ok(Role::Write),
            "manager" => Ok(Role::Manager),
            "admin" => Ok(Role::Admin),
            _ => Err(format!("unknown role {s:?}")),
        }
    }
}

/// Stored authorization row for one principal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthRecord {
    pub role: u8,
    pub manager_caps: u64,
}

impl Storable for AuthRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut v = Vec::with_capacity(1 + 8);
        v.push(self.role);
        v.extend_from_slice(&self.manager_caps.to_le_bytes());
        v
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        assert!(
            b.len() >= 9,
            "AuthRecord expects at least 9 bytes, got {}",
            b.len()
        );
        Self {
            role: b[0],
            manager_caps: u64::from_le_bytes(b[1..9].try_into().unwrap()),
        }
    }
}

/// Stable principal → auth record map.
pub struct AuthState<M: Memory> {
    map: StableBTreeMap<Principal, AuthRecord, M>,
}

impl<M: Memory> AuthState<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn get_record(&self, p: &Principal) -> Option<AuthRecord> {
        self.map.get(p)
    }

    /// Role stored in stable map, if any and valid.
    pub fn role_of(&self, p: &Principal) -> Option<Role> {
        self.get_record(p)
            .and_then(|r| Role::from_discriminant(r.role))
    }

    /// Effective role for authorization: [`Role::Executor`] when there is no stored record.
    pub fn effective_role(&self, p: &Principal) -> Role {
        self.role_of(p).unwrap_or(Role::Executor)
    }

    pub fn require_at_least(&self, p: &Principal, min: Role) -> Result<(), String> {
        let role = self.effective_role(p);
        if role.satisfies_at_least(min) {
            Ok(())
        } else {
            Err(format!(
                "caller {} has role {} but {:?} or higher is required",
                p, role, min
            ))
        }
    }

    pub fn can_prepare_register(&self, p: &Principal) -> bool {
        match self.effective_role(p) {
            Role::Admin => true,
            Role::Manager => {
                let rec = match self.get_record(p) {
                    Some(r) => r,
                    None => return false,
                };
                rec.manager_caps & ManagerCapability::PREPARE_REGISTER.bits() != 0
            }
            _ => false,
        }
    }

    pub fn has_manager_capability(&self, p: &Principal, cap: ManagerCapability) -> bool {
        let role = self.effective_role(p);
        if role == Role::Admin {
            return true;
        }
        if role != Role::Manager {
            return false;
        }
        let Some(rec) = self.get_record(p) else {
            return false;
        };
        rec.manager_caps & cap.bits() != 0
    }

    /// Insert or replace the full record (Admin maintenance).
    pub fn upsert_record(&mut self, p: Principal, record: AuthRecord) {
        self.map.insert(p, record);
    }

    /// Bootstrap: grant [`Role::Admin`] to `issuing_principal` and every entry in `initial_admins`.
    pub fn bootstrap_admins(&mut self, issuing_principal: Principal, initial_admins: &[Principal]) {
        let admin = AuthRecord {
            role: Role::Admin as u8,
            manager_caps: 0,
        };
        self.upsert_record(issuing_principal, admin);
        for p in initial_admins {
            if *p != issuing_principal {
                self.upsert_record(*p, admin);
            }
        }
    }

    pub fn len(&self) -> u64 {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_ordering() {
        assert!(Role::Read.satisfies_at_least(Role::Executor));
        assert!(Role::Write.satisfies_at_least(Role::Read));
        assert!(Role::Manager.satisfies_at_least(Role::Write));
        assert!(Role::Admin.satisfies_at_least(Role::Manager));
    }

    #[test]
    fn unknown_principal_defaults_to_executor() {
        use ic_stable_structures::DefaultMemoryImpl;
        let auth = AuthState::init(DefaultMemoryImpl::default());
        let p = Principal::from_text("aaaaa-aa").unwrap();
        assert_eq!(auth.effective_role(&p), Role::Executor);
        assert!(auth.require_at_least(&p, Role::Executor).is_ok());
        assert!(auth.require_at_least(&p, Role::Read).is_err());
    }

    #[test]
    fn manager_prepare_cap() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let p = Principal::from_text("aaaaa-aa").unwrap();
        auth.upsert_record(
            p,
            AuthRecord {
                role: Role::Manager as u8,
                manager_caps: ManagerCapability::PREPARE_REGISTER.bits(),
            },
        );
        assert!(auth.can_prepare_register(&p));

        let p2 = Principal::from_text("2vxsx-fae").unwrap();
        auth.upsert_record(
            p2,
            AuthRecord {
                role: Role::Manager as u8,
                manager_caps: 0,
            },
        );
        assert!(!auth.can_prepare_register(&p2));
    }
}
