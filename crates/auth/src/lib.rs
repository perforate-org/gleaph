//! Authorization substrate for Gleaph (Internet Computer graph canisters).
//!
//! Two orthogonal dimensions replace the former five-role ladder ([ADR 0074]):
//!
//! - **Admin capabilities** (`AdminCaps`): a global administrative bitset covering
//!   platform/federation operations, prepared-query registration, index DDL, catalog
//!   management, procedure calls, and grant administration. Seeded via the bootstrap
//!   init path; principals with **no row in stable storage** hold an empty set
//!   (**default deny**).
//! - **Data-plane grants**: `(principal | PUBLIC) × privilege` rows with a dormant
//!   `expires_at` field. `PUBLIC` is a virtual pseudo-subject resolved at evaluation
//!   time, never persisted as a principal.
//!
//! Default is empty everywhere → deny. Administrative capability never implies
//! data-plane access (ADR 0074 invariant 1). Enforced on the **router** canister;
//! graph shards trust the router as the only GQL entrypoint.
//!
//! [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md

use candid::Principal;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::fmt;

bitflags::bitflags!(
    /// Global administrative capabilities ([ADR 0074] §1).
    ///
    /// Succeeds the residue of the former role ladder: `PREPARE_REGISTER`, `INDEX_CREATE`,
    /// and `INDEX_DROP` migrate the old `ManagerCapability` bits; the remaining bits cover
    /// the former Manager/Admin authority split into narrowest governing capabilities.
    ///
    /// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct AdminCaps: u64 {
        /// Register/replace/drop prepared queries.
        const PREPARE_REGISTER = 1 << 0;
        /// Create property indexes (GQL `CREATE INDEX` and vector-index DDL).
        const INDEX_CREATE = 1 << 1;
        /// Drop property indexes (GQL `DROP INDEX` and vector-index DDL).
        const INDEX_DROP = 1 << 2;
        /// Catalog DDL: graph-type catalog statements, schema migrations, catalog interning.
        const MANAGE_CATALOG = 1 << 3;
        /// Named `CALL` procedures (until procedures become catalog objects).
        const CALL_PROCEDURE = 1 << 4;
        /// Federation topology: graph/shard registration, backfill, maintenance sweeps,
        /// dispatch activation, diagnostics.
        const MANAGE_FEDERATION = 1 << 5;
        /// Grant administration: writing other principals' capability rows.
        const MANAGE_AUTHORIZATION = 1 << 6;
    }
);

impl AdminCaps {
    /// Stable bit names, in bit order. Used by introspection surfaces.
    pub const NAMES: [&'static str; 7] = [
        "PREPARE_REGISTER",
        "INDEX_CREATE",
        "INDEX_DROP",
        "MANAGE_CATALOG",
        "CALL_PROCEDURE",
        "MANAGE_FEDERATION",
        "MANAGE_AUTHORIZATION",
    ];

    /// Names of the set bits, in bit order.
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .enumerate()
            .filter(|(bit, _)| self.bits() & (1 << bit) != 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

impl fmt::Display for AdminCaps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.names().join(","))
    }
}

/// Failure modes for privileged authorization writes.
///
/// The anonymous principal must never receive a persisted privileged row, so write and
/// bootstrap APIs reject it before mutating stable storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthWriteError {
    /// A privileged write or bootstrap targeted [`Principal::anonymous`].
    AnonymousPrincipal,
}

impl fmt::Display for AuthWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthWriteError::AnonymousPrincipal => {
                f.write_str("anonymous principal cannot hold a stored authorization row")
            }
        }
    }
}

impl std::error::Error for AuthWriteError {}

/// Authoritative, memory-independent validation of bootstrap principals.
///
/// This is the single source of truth for the rule "no anonymous bootstrap identity". Both the
/// stateful [`AuthState::bootstrap_principals`] write path and pre-mutation init preflight (e.g.
/// the router canister `init`) call this so the rule is enforced before any stable structure is
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

/// Stored administrative-capability row for one principal.
///
/// Fresh-state format ([ADR 0074] §6): exactly 8 little-endian capability bits. Legacy
/// role-ladder bytes (9+ bytes) are rejected by the decoder rather than interpreted.
///
/// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapsRecord {
    pub caps: u64,
}

impl Storable for CapsRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.caps.to_le_bytes().to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.caps.to_le_bytes().to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        assert!(
            b.len() == 8,
            "CapsRecord expects exactly 8 bytes, got {}",
            b.len()
        );
        Self {
            caps: u64::from_le_bytes(b.try_into().unwrap()),
        }
    }
}

/// Stable principal → administrative-capability map.
pub struct AuthState<M: Memory> {
    map: StableBTreeMap<Principal, CapsRecord, M>,
}

impl<M: Memory> AuthState<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn get_record(&self, p: &Principal) -> Option<CapsRecord> {
        self.map.get(p)
    }

    /// Effective capabilities for authorization: empty for unknown principals.
    ///
    /// Defense in depth: the anonymous principal never resolves to any capability, even if a
    /// corrupt privileged row exists in stable storage. All effective-authorization reads derive
    /// from this method, so anonymous always resolves to the empty set (default deny).
    pub fn caps_of(&self, p: &Principal) -> AdminCaps {
        if *p == Principal::anonymous() {
            return AdminCaps::empty();
        }
        self.get_record(p)
            .map(|r| AdminCaps::from_bits_truncate(r.caps))
            .unwrap_or(AdminCaps::empty())
    }

    /// Whether `p` holds `cap`.
    pub fn has_cap(&self, p: &Principal, cap: AdminCaps) -> bool {
        self.caps_of(p).contains(cap)
    }

    /// Insert or replace the full capability row (grant administration).
    ///
    /// Rejects [`Principal::anonymous`] before any mutation so a privileged row can never be
    /// persisted for the anonymous principal.
    pub fn upsert_caps(&mut self, p: Principal, caps: AdminCaps) -> Result<(), AuthWriteError> {
        if p == Principal::anonymous() {
            return Err(AuthWriteError::AnonymousPrincipal);
        }
        self.map.insert(p, CapsRecord { caps: caps.bits() });
        Ok(())
    }

    /// Bootstrap: grant the full capability set to `issuing_principal` and every entry in
    /// `initial_admins`.
    ///
    /// All-or-nothing: if the issuing principal or any initial admin is [`Principal::anonymous`],
    /// no rows are inserted and [`AuthWriteError::AnonymousPrincipal`] is returned.
    pub fn bootstrap_principals(
        &mut self,
        issuing_principal: Principal,
        initial_admins: &[Principal],
    ) -> Result<(), AuthWriteError> {
        validate_bootstrap_principals(issuing_principal, initial_admins)?;
        self.upsert_caps(issuing_principal, AdminCaps::all())?;
        for p in initial_admins {
            if *p != issuing_principal {
                self.upsert_caps(*p, AdminCaps::all())?;
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

/// Operation of a data-plane grant ([ADR 0074] §2).
///
/// Slice 1 (ADR 0074 migration) evaluates `EXECUTE PreparedQuery` only; the GRANT/REVOKE
/// grammar and label/property privilege checking extend this enum in the next slice. Each
/// variant carries its own resource payload so impossible combinations (e.g. a direction
/// modifier on a prepared query) cannot be constructed.
///
/// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Privilege {
    /// `EXECUTE ON PREPARED QUERY <name>`; the name is the Router-global prepared
    /// operation name (ADR 0063).
    ExecutePreparedQuery { name: String },
}

impl Privilege {
    /// Operation discriminator used as the leading byte of the stable grant key.
    fn discriminant(&self) -> u8 {
        match self {
            Privilege::ExecutePreparedQuery { .. } => 1,
        }
    }

    /// Variable resource payload following the discriminant in the stable grant key.
    fn resource_bytes(&self) -> Vec<u8> {
        match self {
            Privilege::ExecutePreparedQuery { name } => name.as_bytes().to_vec(),
        }
    }
}

/// Subject of a data-plane grant: a concrete principal or the virtual `PUBLIC`
/// pseudo-subject ([ADR 0074] §1). `Public` is resolved at evaluation time and is never a
/// persisted principal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantSubject {
    Principal(Principal),
    Public,
}

impl GrantSubject {
    fn kind(&self) -> u8 {
        match self {
            GrantSubject::Public => 0,
            GrantSubject::Principal(_) => 1,
        }
    }

    /// Exact principal blob. Length-prefixed in the stable key, so distinct principals of any
    /// IC-supported length map to distinct keys.
    fn principal_bytes(&self) -> Vec<u8> {
        match self {
            GrantSubject::Principal(p) => p.as_slice().to_vec(),
            GrantSubject::Public => Vec::new(),
        }
    }

    /// Canonical subject of an evaluation for caller `p`: the principal itself, except that
    /// the anonymous principal evaluates as `Public` (it cannot hold stored rows, so its only
    /// reachable grants are the PUBLIC baseline).
    pub fn effective_for(p: &Principal) -> Self {
        if *p == Principal::anonymous() {
            GrantSubject::Public
        } else {
            GrantSubject::Principal(*p)
        }
    }
}

/// Canonical stable key of one grant row: `op ‖ resource ‖ subject`.
///
/// All subjects of one privilege sort adjacently (resource prefix), so cascade scans over a
/// privilege read a contiguous range. Lookup is always by exact canonical key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GrantKey(Vec<u8>);

impl GrantKey {
    pub fn new(privilege: &Privilege, subject: &GrantSubject) -> Self {
        let resource = privilege.resource_bytes();
        let principal = subject.principal_bytes();
        let mut key = Vec::with_capacity(1 + 2 + resource.len() + 1 + 2 + principal.len());
        key.push(privilege.discriminant());
        key.extend_from_slice(&(resource.len() as u16).to_le_bytes());
        key.extend_from_slice(&resource);
        key.push(subject.kind());
        key.extend_from_slice(&(principal.len() as u16).to_le_bytes());
        key.extend_from_slice(&principal);
        Self(key)
    }

    /// Decode the canonical key parts. Decoding is total for keys produced by [`Self::new`];
    /// malformed tails trap (corrupt stable state is not recoverable input).
    #[cfg(test)]
    pub(crate) fn parts(&self) -> (u8, String, u8) {
        let b = &self.0;
        let op = b[0];
        let resource_len = u16::from_le_bytes(b[1..3].try_into().unwrap()) as usize;
        let resource = String::from_utf8(b[3..3 + resource_len].to_vec()).expect("utf8 resource");
        let subject_kind = b[3 + resource_len];
        (op, resource, subject_kind)
    }
}

impl Storable for GrantKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }
}

/// Stored grant row value. `expires_at_ns` is dormant in this slice ([ADR 0074] §1b): reads
/// treat a row with `expires_at_ns < now` as absent, so later time-boxing is not a destructive
/// schema change.
///
/// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantRow {
    pub expires_at_ns: Option<u64>,
}

impl Storable for GrantRow {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(match self.expires_at_ns {
            None => vec![0],
            Some(ts) => {
                let mut v = Vec::with_capacity(9);
                v.push(1);
                v.extend_from_slice(&ts.to_le_bytes());
                v
            }
        })
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        assert!(!b.is_empty(), "GrantRow expects at least 1 byte");
        Self {
            expires_at_ns: match b[0] {
                0 => None,
                1 => Some(u64::from_le_bytes(
                    b[1..9].try_into().expect("GrantRow expiry payload"),
                )),
                other => panic!("unknown GrantRow tag {other}"),
            },
        }
    }
}

/// Stable data-plane grant collection ([ADR 0074] §6).
///
/// Owns the `(principal | PUBLIC) × privilege` rows. The anonymous principal can never hold a
/// stored row (invariant 2); evaluations for anonymous callers consult the `PUBLIC` baseline
/// via [`GrantSubject::effective_for`].
///
/// [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md
pub struct GrantState<M: Memory> {
    grants: StableBTreeMap<GrantKey, GrantRow, M>,
}

impl<M: Memory> GrantState<M> {
    pub fn init(memory: M) -> Self {
        Self {
            grants: StableBTreeMap::init(memory),
        }
    }

    /// Insert or replace the grant row for `(privilege, subject)`.
    ///
    /// Rejects [`Principal::anonymous`] subjects before any mutation; the virtual
    /// [`GrantSubject::Public`] subject is the only way to publish to unauthenticated callers.
    pub fn grant(
        &mut self,
        subject: GrantSubject,
        privilege: &Privilege,
        expires_at_ns: Option<u64>,
    ) -> Result<(), AuthWriteError> {
        if let GrantSubject::Principal(p) = &subject
            && *p == Principal::anonymous()
        {
            return Err(AuthWriteError::AnonymousPrincipal);
        }
        let key = GrantKey::new(privilege, &subject);
        self.grants.insert(key, GrantRow { expires_at_ns });
        Ok(())
    }

    /// Remove the exact grant row for `(privilege, subject)`. Returns whether a row existed.
    pub fn revoke(&mut self, subject: GrantSubject, privilege: &Privilege) -> bool {
        self.grants
            .remove(&GrantKey::new(privilege, &subject))
            .is_some()
    }

    /// Whether the exact grant row exists and is unexpired at `now_ns`.
    ///
    /// A row with `expires_at_ns < now_ns` is treated as absent (fail closed); equality is
    /// still valid.
    pub fn holds(&self, subject: GrantSubject, privilege: &Privilege, now_ns: u64) -> bool {
        match self.grants.get(&GrantKey::new(privilege, &subject)) {
            Some(row) => !row.expires_at_ns.is_some_and(|expiry| expiry < now_ns),
            None => false,
        }
    }

    pub fn len(&self) -> u64 {
        self.grants.len()
    }

    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec_priv(name: &str) -> Privilege {
        Privilege::ExecutePreparedQuery {
            name: name.to_string(),
        }
    }

    fn principal(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    // --- AdminCaps ---

    #[test]
    fn caps_names_round_trip() {
        for name in AdminCaps::NAMES {
            let cap = AdminCaps::from_name(name).expect("known name parses");
            assert_eq!(cap.names(), [name]);
        }
        assert!(AdminCaps::from_name("NO_SUCH_CAP").is_none());
        assert_eq!(
            AdminCaps::all().names(),
            AdminCaps::NAMES.to_vec(),
            "all() covers every named bit"
        );
    }

    #[test]
    fn migrated_manager_bits_keep_their_positions() {
        // The three migrated ManagerCapability bits keep their historical positions so the
        // wire-visible bit values stay stable across the destructive replacement.
        assert_eq!(AdminCaps::PREPARE_REGISTER.bits(), 1 << 0);
        assert_eq!(AdminCaps::INDEX_CREATE.bits(), 1 << 1);
        assert_eq!(AdminCaps::INDEX_DROP.bits(), 1 << 2);
    }

    // --- AuthState ---

    #[test]
    fn unknown_principal_defaults_to_deny() {
        use ic_stable_structures::DefaultMemoryImpl;
        let auth = AuthState::init(DefaultMemoryImpl::default());
        let p = principal(1);
        assert_eq!(auth.caps_of(&p), AdminCaps::empty());
        assert!(!auth.has_cap(&p, AdminCaps::PREPARE_REGISTER));
        assert!(!auth.has_cap(&p, AdminCaps::MANAGE_AUTHORIZATION));
    }

    #[test]
    fn upsert_caps_and_has_cap() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let p = principal(2);
        auth.upsert_caps(p, AdminCaps::PREPARE_REGISTER | AdminCaps::INDEX_CREATE)
            .expect("non-anonymous upsert");
        assert!(auth.has_cap(&p, AdminCaps::PREPARE_REGISTER));
        assert!(auth.has_cap(&p, AdminCaps::INDEX_CREATE));
        assert!(!auth.has_cap(&p, AdminCaps::INDEX_DROP));
    }

    #[test]
    fn caps_record_rejects_legacy_role_ladder_bytes() {
        // Legacy AuthRecord rows were ≥9 bytes (role byte + manager_caps). Fresh-state contract:
        // reject old bytes instead of interpreting them.
        let legacy = {
            let mut v = vec![4u8]; // former Role::Admin discriminator
            v.extend_from_slice(&0u64.to_le_bytes());
            v
        };
        let result = std::panic::catch_unwind(|| CapsRecord::from_bytes(Cow::Owned(legacy)));
        assert!(result.is_err(), "legacy 9-byte role rows must be rejected");
    }

    #[test]
    fn caps_record_round_trip() {
        let record = CapsRecord {
            caps: AdminCaps::all().bits(),
        };
        let decoded = CapsRecord::from_bytes(record.to_bytes());
        assert_eq!(decoded, record);
    }

    #[test]
    fn upsert_caps_rejects_anonymous() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let err = auth
            .upsert_caps(Principal::anonymous(), AdminCaps::all())
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(auth.is_empty());
        assert_eq!(auth.caps_of(&Principal::anonymous()), AdminCaps::empty());
    }

    #[test]
    fn validate_bootstrap_principals_accepts_all_non_anonymous() {
        let issuer = principal(1);
        let admin = principal(2);
        validate_bootstrap_principals(issuer, &[admin]).expect("all non-anonymous is valid");
    }

    #[test]
    fn validate_bootstrap_principals_rejects_anonymous_issuer_with_valid_admin() {
        let valid = principal(2);
        assert_eq!(
            validate_bootstrap_principals(Principal::anonymous(), &[valid]),
            Err(AuthWriteError::AnonymousPrincipal)
        );
    }

    #[test]
    fn validate_bootstrap_principals_rejects_anonymous_initial_admin() {
        let issuer = principal(1);
        let valid = principal(2);
        assert_eq!(
            validate_bootstrap_principals(issuer, &[valid, Principal::anonymous()]),
            Err(AuthWriteError::AnonymousPrincipal)
        );
    }

    #[test]
    fn bootstrap_seeds_full_caps() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let issuer = principal(1);
        let other = principal(2);
        auth.bootstrap_principals(issuer, &[other])
            .expect("bootstrap");
        assert_eq!(auth.caps_of(&issuer), AdminCaps::all());
        assert_eq!(auth.caps_of(&other), AdminCaps::all());
        assert_eq!(auth.len(), 2);
    }

    #[test]
    fn bootstrap_rejects_anonymous_issuer_without_inserting_rows() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let real_admin = principal(1);
        let err = auth
            .bootstrap_principals(Principal::anonymous(), &[real_admin])
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(auth.is_empty(), "no rows inserted on rejected bootstrap");
        // The supplied valid initial admin was not elevated.
        assert_eq!(auth.caps_of(&real_admin), AdminCaps::empty());
    }

    #[test]
    fn bootstrap_rejects_anonymous_initial_admin_all_or_nothing() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        let issuer = principal(1);
        let valid = principal(2);
        let err = auth
            .bootstrap_principals(issuer, &[valid, Principal::anonymous()])
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(
            auth.is_empty(),
            "issuer and valid admin must not be inserted when any initial admin is anonymous"
        );
        // Neither the issuer nor the valid initial admin from the same request was elevated.
        assert_eq!(auth.caps_of(&issuer), AdminCaps::empty());
        assert_eq!(auth.caps_of(&valid), AdminCaps::empty());
    }

    #[test]
    fn corrupt_anonymous_row_does_not_elevate_effective_caps() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut auth = AuthState::init(DefaultMemoryImpl::default());
        // Simulate a corrupt persisted row by inserting directly into the backing map,
        // bypassing the guarded write path.
        auth.map.insert(
            Principal::anonymous(),
            CapsRecord {
                caps: AdminCaps::all().bits(),
            },
        );
        assert_eq!(auth.caps_of(&Principal::anonymous()), AdminCaps::empty());
        assert!(!auth.has_cap(&Principal::anonymous(), AdminCaps::PREPARE_REGISTER));
    }

    // --- GrantState ---

    #[test]
    fn grant_then_holds_exact_key_addressing() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(3);
        grants
            .grant(GrantSubject::Principal(p), &exec_priv("q1"), None)
            .expect("principal subject");
        assert!(grants.holds(GrantSubject::Principal(p), &exec_priv("q1"), 0));
        // Different query name: no grant (exact canonical key drives lookup).
        assert!(!grants.holds(GrantSubject::Principal(p), &exec_priv("q2"), 0));
        // Different subject: no grant.
        assert!(!grants.holds(GrantSubject::Principal(principal(4)), &exec_priv("q1"), 0));
        // PUBLIC subject: separate row.
        assert!(!grants.holds(GrantSubject::Public, &exec_priv("q1"), 0));
    }

    #[test]
    fn grant_key_groups_subjects_by_resource_prefix() {
        let key_a = GrantKey::new(&exec_priv("q1"), &GrantSubject::Public);
        let key_b = GrantKey::new(&exec_priv("q1"), &GrantSubject::Principal(principal(1)));
        let key_c = GrantKey::new(&exec_priv("q2"), &GrantSubject::Public);
        assert!(key_a < key_b, "same privilege sorts subjects adjacently");
        assert!(key_b < key_c, "different privileges do not interleave");
        let (op, resource, subject_kind) = key_a.parts();
        assert_eq!(op, 1);
        assert_eq!(resource, "q1");
        assert_eq!(subject_kind, 0);
    }

    #[test]
    fn grant_keys_distinguish_short_principals() {
        // IC principals are variable length; the canonical key must not collapse them.
        let a = GrantKey::new(
            &exec_priv("q1"),
            &GrantSubject::Principal(Principal::from_slice(&[5; 10])),
        );
        let b = GrantKey::new(
            &exec_priv("q1"),
            &GrantSubject::Principal(Principal::from_slice(&[6; 10])),
        );
        assert_ne!(a, b);
    }

    #[test]
    fn grant_rejects_anonymous_subject() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let err = grants
            .grant(
                GrantSubject::Principal(Principal::anonymous()),
                &exec_priv("q1"),
                None,
            )
            .unwrap_err();
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert!(grants.is_empty(), "rejected grant must persist no row");
    }

    #[test]
    fn public_row_is_the_only_path_for_anonymous_evaluation() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        grants
            .grant(GrantSubject::Public, &exec_priv("public-q"), None)
            .expect("public subject is storable");
        // Anonymous evaluation resolves to the PUBLIC subject.
        let anon = GrantSubject::effective_for(&Principal::anonymous());
        assert_eq!(anon, GrantSubject::Public);
        assert!(grants.holds(anon, &exec_priv("public-q"), 0));
        // A named principal's evaluation never falls through to the PUBLIC row.
        let named = GrantSubject::effective_for(&principal(5));
        assert!(!grants.holds(named, &exec_priv("public-q"), 0));
    }

    #[test]
    fn expired_rows_are_treated_as_absent() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        grants
            .grant(GrantSubject::Public, &exec_priv("timed"), Some(100))
            .expect("grant with expiry");
        assert!(grants.holds(GrantSubject::Public, &exec_priv("timed"), 100));
        assert!(!grants.holds(GrantSubject::Public, &exec_priv("timed"), 101));
        assert!(!grants.holds(GrantSubject::Public, &exec_priv("timed"), 1_000));
    }

    #[test]
    fn revoke_removes_only_the_exact_row() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut grants = GrantState::init(DefaultMemoryImpl::default());
        let p = principal(6);
        grants
            .grant(GrantSubject::Principal(p), &exec_priv("q1"), None)
            .expect("grant");
        grants
            .grant(GrantSubject::Public, &exec_priv("q1"), None)
            .expect("public grant");
        assert!(grants.revoke(GrantSubject::Principal(p), &exec_priv("q1")));
        assert!(!grants.revoke(GrantSubject::Principal(p), &exec_priv("q1")));
        assert!(grants.holds(GrantSubject::Public, &exec_priv("q1"), 0));
        assert_eq!(grants.len(), 1);
    }

    #[test]
    fn grant_row_round_trip() {
        for row in [
            GrantRow {
                expires_at_ns: None,
            },
            GrantRow {
                expires_at_ns: Some(u64::MAX),
            },
        ] {
            assert_eq!(GrantRow::from_bytes(row.to_bytes()), row);
        }
    }
}
