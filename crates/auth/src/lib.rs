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

/// Failure modes for privileged authorization writes.
///
/// The anonymous principal must never receive a persisted privileged role, so write and
/// bootstrap APIs reject it before mutating stable storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthWriteError {
    /// A privileged role write or bootstrap targeted [`Principal::anonymous`].
    AnonymousPrincipal,
}

impl fmt::Display for AuthWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthWriteError::AnonymousPrincipal => {
                f.write_str("anonymous principal cannot hold a privileged authorization role")
            }
        }
    }
}

impl std::error::Error for AuthWriteError {}

/// Authoritative, memory-independent validation of bootstrap principals.
///
/// This is the single source of truth for the rule "no anonymous bootstrap identity". Both the
/// stateful [`AuthState::bootstrap_admins`] write path and pre-mutation init preflight (e.g. the
/// router canister `init`) call this so the rule is enforced before any stable structure is
/// cleared or written, and is never duplicated.
pub fn validate_bootstrap_principals(
    issuing_principal: Principal,
    initial_admins: &[Principal],
) -> Result<(), AuthWriteError> {
    if issuing_principal == Principal::anonymous()
        || initial_admins.iter().any(|p| *p == Principal::anonymous())
    {
        return Err(AuthWriteError::AnonymousPrincipal);
    }
    Ok(())
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
    ///
    /// Defense in depth: the anonymous principal is never elevated, even if a legacy or corrupt
    /// privileged row exists in stable storage. All effective-authorization reads derive from this
    /// method, so anonymous always resolves to the [`Role::Executor`] default.
    pub fn role_of(&self, p: &Principal) -> Option<Role> {
        if *p == Principal::anonymous() {
            return None;
        }
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
    ///
    /// Rejects [`Principal::anonymous`] before any mutation so a privileged role can never be
    /// persisted for the anonymous principal.
    pub fn upsert_record(
        &mut self,
        p: Principal,
        record: AuthRecord,
    ) -> Result<(), AuthWriteError> {
        if p == Principal::anonymous() {
            return Err(AuthWriteError::AnonymousPrincipal);
        }
        self.map.insert(p, record);
        Ok(())
    }

    /// Bootstrap: grant [`Role::Admin`] to `issuing_principal` and every entry in `initial_admins`.
    ///
    /// All-or-nothing: if the issuing principal or any initial admin is [`Principal::anonymous`],
    /// no rows are inserted and [`AuthWriteError::AnonymousPrincipal`] is returned.
    pub fn bootstrap_admins(
        &mut self,
        issuing_principal: Principal,
        initial_admins: &[Principal],
    ) -> Result<(), AuthWriteError> {
        validate_bootstrap_principals(issuing_principal, initial_admins)?;
        let admin = AuthRecord {
            role: Role::Admin as u8,
            manager_caps: 0,
        };
        self.upsert_record(issuing_principal, admin)?;
        for p in initial_admins {
            if *p != issuing_principal {
                self.upsert_record(*p, admin)?;
            }
        }
        Ok(())
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
        )
        .expect("non-anonymous upsert");
        assert!(auth.can_prepare_register(&p));

        let p2 = Principal::from_slice(&[7; 29]);
        auth.upsert_record(
            p2,
            AuthRecord {
                role: Role::Manager as u8,
                manager_caps: 0,
            },
        )
        .expect("non-anonymous upsert");
        assert!(!auth.can_prepare_register(&p2));
    }

    #[test]
    fn upsert_record_rejects_anonymous() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let err = auth
            .upsert_record(
                Principal::anonymous(),
                AuthRecord {
                    role: Role::Admin as u8,
                    manager_caps: 0,
                },
            )
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(auth.is_empty());
        assert_eq!(auth.effective_role(&Principal::anonymous()), Role::Executor);
    }

    #[test]
    fn validate_bootstrap_principals_accepts_all_non_anonymous() {
        let issuer = Principal::from_slice(&[1; 29]);
        let admin = Principal::from_slice(&[2; 29]);
        validate_bootstrap_principals(issuer, &[admin]).expect("all non-anonymous is valid");
    }

    #[test]
    fn validate_bootstrap_principals_rejects_anonymous_issuer_with_valid_admin() {
        let valid = Principal::from_slice(&[2; 29]);
        assert_eq!(
            validate_bootstrap_principals(Principal::anonymous(), &[valid]),
            Err(AuthWriteError::AnonymousPrincipal)
        );
    }

    #[test]
    fn validate_bootstrap_principals_rejects_anonymous_initial_admin() {
        let issuer = Principal::from_slice(&[1; 29]);
        let valid = Principal::from_slice(&[2; 29]);
        assert_eq!(
            validate_bootstrap_principals(issuer, &[valid, Principal::anonymous()]),
            Err(AuthWriteError::AnonymousPrincipal)
        );
    }

    #[test]
    fn bootstrap_rejects_anonymous_issuer_without_inserting_rows() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let real_admin = Principal::from_slice(&[1; 29]);
        let err = auth
            .bootstrap_admins(Principal::anonymous(), &[real_admin])
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(auth.is_empty(), "no rows inserted on rejected bootstrap");
        // The supplied valid initial admin was not elevated.
        assert_eq!(auth.effective_role(&real_admin), Role::Executor);
    }

    #[test]
    fn bootstrap_rejects_anonymous_initial_admin_all_or_nothing() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let issuer = Principal::from_slice(&[1; 29]);
        let valid = Principal::from_slice(&[2; 29]);
        let err = auth
            .bootstrap_admins(issuer, &[valid, Principal::anonymous()])
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(
            auth.is_empty(),
            "issuer and valid admin must not be inserted when any initial admin is anonymous"
        );
        // Neither the issuer nor the valid initial admin from the same request was elevated.
        assert_eq!(auth.effective_role(&issuer), Role::Executor);
        assert_eq!(auth.effective_role(&valid), Role::Executor);
    }

    #[test]
    fn bootstrap_inserts_only_non_anonymous_admins() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let issuer = Principal::from_slice(&[1; 29]);
        let other = Principal::from_slice(&[2; 29]);
        auth.bootstrap_admins(issuer, &[other]).expect("bootstrap");
        assert_eq!(auth.effective_role(&issuer), Role::Admin);
        assert_eq!(auth.effective_role(&other), Role::Admin);
        assert_eq!(auth.len(), 2);
    }

    #[test]
    fn legacy_anonymous_row_does_not_elevate_effective_role() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        // Simulate a legacy/corrupt persisted row by inserting directly into the backing map,
        // bypassing the guarded write path.
        auth.map.insert(
            Principal::anonymous(),
            AuthRecord {
                role: Role::Admin as u8,
                manager_caps: ManagerCapability::PREPARE_REGISTER.bits(),
            },
        );
        assert_eq!(auth.role_of(&Principal::anonymous()), None);
        assert_eq!(auth.effective_role(&Principal::anonymous()), Role::Executor);
        assert!(!auth.can_prepare_register(&Principal::anonymous()));
        assert!(
            !auth.has_manager_capability(
                &Principal::anonymous(),
                ManagerCapability::PREPARE_REGISTER
            )
        );
        assert!(
            auth.require_at_least(&Principal::anonymous(), Role::Read)
                .is_err()
        );
    }
}
