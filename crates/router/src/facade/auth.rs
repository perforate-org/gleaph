//! Administrative-capability facade for router user-facing GQL, prepared-query, and admin
//! APIs (ADR 0074).
//!
//! The former role ladder is replaced by [`AdminCaps`]: principals with no stored row hold an
//! empty set (default deny), bootstrap principals hold the full set, and grant administration
//! writes capability rows under `MANAGE_AUTHORIZATION`. Data-plane grants live in
//! `ROUTER_AUTH_GRANTS` and are evaluated by `crate::rbac`.
//!
//! [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md

use candid::Principal;
use gleaph_auth::{
    AdminCaps, AuthWriteError, ElevationEvidence, GrantRowEntry, GrantSubject, MetadataScope,
    Privilege,
};

use crate::state::RouterError;

use super::stable::{ROUTER_AUTH_GRANTS, ROUTER_AUTH_STATE};

/// Pre-mutation preflight for canister init: validates bootstrap principals via the
/// auth-owned authoritative path before any stable state is cleared or written.
pub fn validate_bootstrap_principals(
    issuing_principal: Principal,
    initial_admins: &[Principal],
) -> Result<(), AuthWriteError> {
    gleaph_auth::validate_bootstrap_principals(issuing_principal, initial_admins)
}

/// Bootstrap installer/initial admins. Seeds the full capability set and rejects the
/// anonymous principal all-or-nothing (see [`gleaph_auth::AuthState::bootstrap_principals`]).
pub fn bootstrap_canister_auth(
    issuing_principal: Principal,
    initial_admins: &[Principal],
) -> Result<(), AuthWriteError> {
    ROUTER_AUTH_STATE
        .with_borrow_mut(|auth| auth.bootstrap_principals(issuing_principal, initial_admins))
}

/// Grant the full capability set to `principal` (tests and local bootstrap).
pub fn grant_admin(principal: Principal) {
    ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
        auth.upsert_caps(principal, AdminCaps::all())
            .expect("grant_admin requires a non-anonymous principal");
    });
}

pub fn grant_admins(principals: &[Principal]) {
    for principal in principals {
        grant_admin(*principal);
    }
}

/// Effective capabilities of `principal`; empty for unknown or anonymous callers.
pub fn caps_of(principal: &Principal) -> AdminCaps {
    ROUTER_AUTH_STATE.with_borrow(|auth| auth.caps_of(principal))
}

/// Whether `principal` holds `cap` (or a superset).
pub fn has_cap(principal: &Principal, cap: AdminCaps) -> bool {
    ROUTER_AUTH_STATE.with_borrow(|auth| auth.has_cap(principal, cap))
}

/// Whether `principal` holds at least one administrative capability. Anonymous always
/// resolves to the empty set.
pub fn has_any_cap(principal: &Principal) -> bool {
    !caps_of(principal).is_empty()
}

/// Whether `principal` holds the full capability set — the bootstrap-superuser analogue of
/// the former global Admin role.
pub fn is_admin(principal: &Principal) -> bool {
    caps_of(principal) == AdminCaps::all()
}

/// Require that `principal` holds `cap`, else [`RouterError::NotAuthorized`].
///
/// Preserves the error contract of the former `require_admin` at every migrated store-level
/// surface. Every privileged surface names its narrowest governing capability here; there is
/// no implicit elevation from other capabilities or from data-plane grants (ADR 0074
/// invariant 1).
pub fn require_cap(principal: &Principal, cap: AdminCaps) -> Result<(), RouterError> {
    if has_cap(principal, cap) {
        Ok(())
    } else {
        Err(RouterError::NotAuthorized)
    }
}

/// Grant administration write path (`MANAGE_AUTHORIZATION`): replace the target's full
/// capability row. Rejects anonymous targets before any mutation.
pub fn admin_upsert_caps(
    caller: &Principal,
    target: Principal,
    caps: u64,
) -> Result<(), RouterError> {
    require_cap(caller, AdminCaps::MANAGE_AUTHORIZATION)?;
    let caps = AdminCaps::from_bits_truncate(caps);
    ROUTER_AUTH_STATE
        .with_borrow_mut(|auth| auth.upsert_caps(target, caps))
        .map_err(|_| {
            RouterError::InvalidArgument(
                "the anonymous principal cannot hold an authorization row".into(),
            )
        })
}

// ──── Data-plane grant rows (ADR 0074 §6) ────

/// Insert or replace one grant row (`GRANT`, ADR 0074 §5), including its optional
/// compiled conditional-policy predicate ([ADR 0075] §1). The anonymous-subject guard
/// is defense in depth: the statement executor rejects anonymous subjects before any
/// write.
pub fn add_grant(
    subject: GrantSubject,
    privilege: &Privilege,
    expires_at_ns: Option<u64>,
    predicate: Option<std::rc::Rc<gleaph_auth::CompiledPredicate>>,
) -> Result<(), AuthWriteError> {
    // Any grant write may change which policy predicates apply to a caller, so the
    // per-caller lowered-plan heap cache is invalidated eagerly ([ADR 0075] §5).
    crate::policy_pushdown::invalidate_lowered_plan_cache();
    ROUTER_AUTH_GRANTS
        .with_borrow_mut(|grants| grants.grant(subject, privilege, expires_at_ns, predicate))
}

/// Insert or replace one loop-issued elevation row ([ADR 0080] §3): the canonical
/// issuance path for windowed, evidence-complete metadata grants. The evidence is
/// validated here at the row-shape owner before any stable mutation.
pub fn add_elevation_grant(
    requester: Principal,
    scope: MetadataScope,
    expires_at_ns: u64,
    approver: Principal,
    justification: String,
    emergency: bool,
) -> Result<(), RouterError> {
    let evidence = ElevationEvidence {
        approver,
        justification: justification.clone(),
        emergency,
    };
    evidence.validate().map_err(|err| match err {
        AuthWriteError::EmptyJustification | AuthWriteError::JustificationTooLong(_) => {
            RouterError::InvalidArgument(err.to_string())
        }
        AuthWriteError::AnonymousPrincipal => RouterError::NotAuthorized,
    })?;
    if requester == Principal::anonymous() {
        return Err(RouterError::NotAuthorized);
    }
    crate::policy_pushdown::invalidate_lowered_plan_cache();
    ROUTER_AUTH_GRANTS
        .with_borrow_mut(|grants| {
            grants.grant_elevation(
                GrantSubject::Principal(requester),
                &Privilege::Metadata(scope),
                expires_at_ns,
                evidence,
            )
        })
        .map_err(|err| RouterError::InvalidArgument(err.to_string()))
}

/// Whether `caller` (`caller ∪ PUBLIC`) holds an unexpired metadata-plane elevation that
/// covers `graph_raw`: a graph-scoped `ReadMetadata` row or the cross-graph
/// `ControlPlane` scope ([ADR 0080] §2). Exact-key probes only — no scan, and by the
/// plane-disjointness contract these probes can never be satisfied by data-plane rows.
pub fn holds_metadata_graph_access(graph_raw: u32, caller: &Principal) -> bool {
    let now_ns = crate::facade::store::ic_time_ns();
    ROUTER_AUTH_GRANTS.with_borrow(|grants| {
        let effective = GrantSubject::effective_for(caller);
        [
            (
                effective,
                Privilege::Metadata(MetadataScope::Graph(graph_raw)),
            ),
            (effective, Privilege::Metadata(MetadataScope::ControlPlane)),
            (
                GrantSubject::Public,
                Privilege::Metadata(MetadataScope::Graph(graph_raw)),
            ),
            (
                GrantSubject::Public,
                Privilege::Metadata(MetadataScope::ControlPlane),
            ),
        ]
        .into_iter()
        .any(|(subject, privilege)| grants.holds(subject, &privilege, now_ns))
    })
}

/// Remove the exact grant row; `true` when it existed (REVOKE).
pub fn remove_grant(subject: GrantSubject, privilege: &Privilege) -> bool {
    // Conditional-policy plans are derived from grant rows; any write invalidates the
    // per-caller lowered-plan heap cache so revocations take effect immediately
    // ([ADR 0075] §5).
    crate::policy_pushdown::invalidate_lowered_plan_cache();
    ROUTER_AUTH_GRANTS.with_borrow_mut(|grants| grants.revoke(subject, privilege))
}

/// Cascade-invalidate every graph-scoped grant row targeting `graph_raw`
/// (ADR 0074 §3 invariant 4). Called by the vocabulary-drop boundary
/// (`purge_graph_vocabulary_partitions`) after a graph's label/property
/// partitions leave the catalogs; ids are monotonic, so those rows could never
/// match again. Returns the number of removed rows.
pub fn sweep_graph_grants(graph_raw: u32) -> usize {
    crate::policy_pushdown::invalidate_lowered_plan_cache();
    ROUTER_AUTH_GRANTS.with_borrow_mut(|grants| grants.revoke_all_for_graph(graph_raw))
}

/// Read-only existence probe for revoke preflights (expired rows still exist as state).
pub fn grant_contains(subject: GrantSubject, privilege: &Privilege) -> bool {
    ROUTER_AUTH_GRANTS.with_borrow(|grants| grants.contains(subject, privilege))
}

/// All stored grant rows decoded in canonical key order (introspection surfaces).
pub fn grant_rows() -> Vec<GrantRowEntry> {
    ROUTER_AUTH_GRANTS.with_borrow(|grants| grants.rows())
}

/// Whether `caller` (or the `PUBLIC` baseline) holds at least one unexpired data-plane
/// grant targeting `graph` (ADR 0074 slice 2b).
///
/// Backs the grant-derived arm of graph visibility in `facade::store::registry`: grantees
/// of a shared graph may resolve it by name even though they are no tenant. The anonymous
/// principal evaluates as the `PUBLIC` subject only.
pub fn holds_any_graph_grant(graph_raw: u32, caller: &Principal) -> bool {
    let now_ns = crate::facade::store::ic_time_ns();
    ROUTER_AUTH_GRANTS.with_borrow(|grants| {
        grants.holds_any_graph_grant(GrantSubject::effective_for(caller), graph_raw, now_ns)
            || grants.holds_any_graph_grant(GrantSubject::Public, graph_raw, now_ns)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    #[test]
    fn admin_upsert_caps_requires_manage_authorization() {
        let caller = principal(1);
        // A caller with unrelated caps but not MANAGE_AUTHORIZATION is denied and the target
        // receives no row.
        ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
            auth.upsert_caps(caller, AdminCaps::PREPARE_REGISTER)
                .expect("non-anonymous");
        });
        let err = admin_upsert_caps(&caller, principal(2), AdminCaps::all().bits())
            .expect_err("missing MANAGE_AUTHORIZATION must be rejected");
        assert!(matches!(err, RouterError::NotAuthorized));
        assert_eq!(caps_of(&principal(2)), AdminCaps::empty());
    }

    #[test]
    fn admin_upsert_caps_rejects_anonymous_target() {
        let admin = principal(3);
        grant_admin(admin);
        let err = admin_upsert_caps(&admin, Principal::anonymous(), AdminCaps::all().bits())
            .expect_err("anonymous target must be rejected");
        assert!(
            matches!(err, RouterError::InvalidArgument(_)),
            "expected InvalidArgument, got {err:?}"
        );
        // The anonymous principal must remain default-deny (no persisted elevation).
        assert_eq!(caps_of(&Principal::anonymous()), AdminCaps::empty());
    }

    #[test]
    fn is_admin_means_full_caps_only() {
        let partial = principal(4);
        ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
            auth.upsert_caps(partial, AdminCaps::all() ^ AdminCaps::MANAGE_FEDERATION)
                .expect("non-anonymous");
        });
        assert!(!is_admin(&partial));
        assert!(has_any_cap(&partial));

        let full = principal(5);
        grant_admin(full);
        assert!(is_admin(&full));
        assert!(!is_admin(&Principal::anonymous()));
        assert!(!has_any_cap(&Principal::anonymous()));
    }

    #[test]
    fn bootstrap_canister_auth_rejects_anonymous_issuer_without_persisting() {
        // A distinctive valid initial admin supplied alongside the anonymous issuer.
        let valid = Principal::from_slice(&[0xA1; 29]);
        let err = bootstrap_canister_auth(Principal::anonymous(), &[valid])
            .expect_err("anonymous issuer must be rejected");
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert_eq!(caps_of(&Principal::anonymous()), AdminCaps::empty());
        // The valid initial admin from the rejected request was not partially inserted/elevated.
        assert_eq!(caps_of(&valid), AdminCaps::empty());
    }

    #[test]
    fn bootstrap_canister_auth_rejects_anonymous_initial_admin_without_persisting() {
        let issuer = Principal::from_slice(&[0xA2; 29]);
        let valid = Principal::from_slice(&[0xA3; 29]);
        let err = bootstrap_canister_auth(issuer, &[valid, Principal::anonymous()])
            .expect_err("anonymous initial admin must be rejected");
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        // Neither the issuer nor the valid initial admin from the same request was elevated.
        assert_eq!(caps_of(&issuer), AdminCaps::empty());
        assert_eq!(caps_of(&valid), AdminCaps::empty());
    }

    #[test]
    fn init_preflight_rejects_invalid_bootstrap_before_elevating_valid_admin() {
        // Mirrors the order in `lib.rs::init`: the auth-owned preflight runs before any Router
        // stable state is cleared/written, so an anonymous issuer is rejected even when a valid
        // initial admin is supplied — without relying on IC trap rollback.
        let valid = Principal::from_slice(&[0xA4; 29]);
        let err = validate_bootstrap_principals(Principal::anonymous(), &[valid])
            .expect_err("preflight must reject anonymous issuer");
        assert_eq!(err, AuthWriteError::AnonymousPrincipal);
        assert_eq!(caps_of(&valid), AdminCaps::empty());
    }

    #[test]
    fn control_plane_elevation_covers_every_graph_and_data_rows_cover_none() {
        use gleaph_auth::{ElevationEvidence, MetadataScope, Privilege};

        let operator = principal(60);
        ROUTER_AUTH_GRANTS.with_borrow_mut(|grants| {
            grants
                .grant_elevation(
                    GrantSubject::Principal(operator),
                    &Privilege::Metadata(MetadataScope::ControlPlane),
                    1_000,
                    ElevationEvidence {
                        approver: principal(61),
                        justification: "fleet sweep".into(),
                        emergency: false,
                    },
                )
                .expect("control-plane elevation");
        });
        // Evaluation policy: one cross-graph row covers arbitrary graphs...
        assert!(holds_metadata_graph_access(11, &operator));
        assert!(holds_metadata_graph_access(12, &operator));
        // ...while data-plane coverage stays structurally disjoint (probe at the
        // storage layer; enforced at plan time by `authz::requirements_cover`).
        ROUTER_AUTH_GRANTS.with_borrow(|grants| {
            assert!(!grants.holds(
                GrantSubject::Principal(operator),
                &Privilege::Graph(gleaph_auth::GraphPrivilege {
                    graph: 11,
                    operation: gleaph_auth::GraphOperation::Match,
                    resource: gleaph_auth::GraphResource::VertexLabel(1),
                }),
                500,
            ));
        });
    }

    #[test]
    fn metadata_graph_scope_and_expiry_drive_graph_metadata_access() {
        use gleaph_auth::{ElevationEvidence, MetadataScope, Privilege};

        let operator = principal(62);
        let other = principal(63);
        ROUTER_AUTH_GRANTS.with_borrow_mut(|grants| {
            grants
                .grant_elevation(
                    GrantSubject::Principal(operator),
                    &Privilege::Metadata(MetadataScope::Graph(21)),
                    100,
                    ElevationEvidence {
                        approver: principal(64),
                        justification: "windowed".into(),
                        emergency: false,
                    },
                )
                .expect("graph-scoped elevation");
        });
        assert!(holds_metadata_graph_access(21, &operator));
        assert!(!holds_metadata_graph_access(22, &operator), "scope-bound");
        assert!(
            !holds_metadata_graph_access(21, &other),
            "subject-bound: another principal inherits nothing"
        );
    }
}
