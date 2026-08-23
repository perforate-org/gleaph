//! Capability checks for router GQL entrypoints (stable auth via [`crate::facade::auth`]).
//!
//! [ADR 0074] replaces the former role ladder: every gate names the narrowest governing
//! capability, default is deny, and data-plane access flows through grants (`caller ∪
//! PUBLIC`), never through administrative capabilities.
//!
//! **Interim narrowing (accepted for this slice; superseded by slice 2):** ad-hoc GQL
//! admission requires the ADR 0028 tenancy predicate or non-empty caps instead of the
//! former global Read tier. Today's shipped flows are owner-centric (per-developer routers,
//! ADR 0068) and are unaffected.
//!
//! [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md

use candid::Principal;
use gleaph_auth::{AdminCaps, GrantSubject, Privilege};
use gleaph_gql::ast::GqlProgram;
use gleaph_gql::program_modification::ProgramModificationFlags;
use gleaph_graph_kernel::entry::GraphId;

use crate::facade::auth;
use crate::facade::stable::ROUTER_AUTH_GRANTS;
use crate::state::RouterError;

/// Ad-hoc GQL (`gql_query` / `gql_mutate`): interim ADR 0074 policy.
///
/// Admission is `(ADR 0028 tenancy predicate) OR (non-empty admin caps)`, with named-`CALL`
/// programs additionally requiring `CALL_PROCEDURE` (unknown principals cannot run platform
/// procedures). Per-graph data-modification privilege checking arrives with slice 2's
/// plan-time enforcement; until then no read/write tier distinction is applied here.
///
/// The tenancy arm resolves the program's effective graph through the same resolver used for
/// execution, so a caller without tenancy gets the ADR 0028 `NotFound` non-disclosure error
/// rather than a distinguishable authorization failure.
pub fn authorize_adhoc_gql(
    caller: &Principal,
    flags: ProgramModificationFlags,
    program: &GqlProgram,
) -> Result<(), RouterError> {
    let caps = auth::caps_of(caller);
    if flags.has_call_procedure && !caps.contains(AdminCaps::CALL_PROCEDURE) {
        return Err(RouterError::Forbidden);
    }
    if !caps.is_empty() {
        return Ok(());
    }
    let store = crate::facade::store::RouterStore::new();
    crate::graph_context::resolve_graph_context(&store, program, *caller)
        .map(|_| ())
        // Deny without disclosing graph existence (ADR 0028).
        .map_err(|_| RouterError::NotFound("graph context".into()))
}

/// Index DDL (`CREATE INDEX` / `DROP INDEX` ordinary parse path, vector-index DDL, and the
/// legacy `admin_set_indexed_*` compat endpoints): holds `INDEX_CREATE` or `INDEX_DROP`.
pub fn authorize_index_ddl(caller: &Principal) -> Result<(), RouterError> {
    if auth::caps_of(caller).intersects(AdminCaps::INDEX_CREATE | AdminCaps::INDEX_DROP) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

/// Global vector-dispatch activation control + shard vector-attach (ADR 0031 Slice 4):
/// cross-graph dispatch topology control requires `MANAGE_FEDERATION`.
pub fn authorize_vector_activation(caller: &Principal) -> Result<(), RouterError> {
    if auth::has_cap(caller, AdminCaps::MANAGE_FEDERATION) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

/// Vector-index maintenance forwarding + policy control (ADR 0031 Slice 10): derived-index
/// control-plane operations require `MANAGE_FEDERATION`.
pub fn authorize_vector_maintenance(caller: &Principal) -> Result<(), RouterError> {
    if auth::has_cap(caller, AdminCaps::MANAGE_FEDERATION) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

/// Stable-memory diagnostics are control-plane observability and therefore require
/// `MANAGE_FEDERATION`.
pub fn authorize_stable_memory_diagnostics(caller: &Principal) -> Result<(), RouterError> {
    if auth::has_cap(caller, AdminCaps::MANAGE_FEDERATION) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

/// `prepare` (batch registration) / `drop_prepared`: `PREPARE_REGISTER`.
pub fn authorize_prepared_catalog_change(caller: &Principal) -> Result<(), RouterError> {
    if auth::has_cap(caller, AdminCaps::PREPARE_REGISTER) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

/// Prepared execution (ADR 0074 §1b/§4): the former Executor default is removed. Effective
/// privilege is `caller-grants ∪ PUBLIC-grants ∪ ownership-derived`, evaluated under SECURITY
/// INVOKER semantics.
///
/// - The caller must hold an explicit `EXECUTE PreparedQuery` grant, or
/// - the query's `PUBLIC` grant row exists (the registration-time bridge), or
/// - the caller is owner/admin of the query's bound graph (`registry.owner` is the SSOT; issuer
///   authority is derived at evaluation time and never duplicated into rows).
///
/// The anonymous principal evaluates as the `PUBLIC` subject only — it can never hold a stored
/// row and is never a graph tenant.
pub fn authorize_prepared_execute(
    caller: &Principal,
    name: &str,
    graph_id: GraphId,
) -> Result<(), RouterError> {
    let privilege = Privilege::ExecutePreparedQuery {
        name: name.to_string(),
    };
    let now_ns = crate::facade::store::ic_time_ns();
    let granted = ROUTER_AUTH_GRANTS.with_borrow(|grants| {
        grants.holds(GrantSubject::effective_for(caller), &privilege, now_ns)
            || grants.holds(GrantSubject::Public, &privilege, now_ns)
    });
    if granted {
        return Ok(());
    }
    // Ownership-derived arm: no admin-caps bypass on the data plane (ADR 0074 invariant 1).
    if crate::facade::store::RouterStore::new().is_graph_tenant(graph_id, *caller) {
        return Ok(());
    }
    Err(RouterError::Forbidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::program_modification::ProgramModificationFlags;

    fn principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    fn upsert_caps(p: Principal, caps: AdminCaps) {
        crate::facade::stable::ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
            auth.upsert_caps(p, caps)
                .expect("test principal must be non-anonymous");
        });
    }

    fn grant_public_exec(name: &str) {
        ROUTER_AUTH_GRANTS.with_borrow_mut(|grants| {
            grants
                .grant(
                    GrantSubject::Public,
                    &Privilege::ExecutePreparedQuery { name: name.into() },
                    None,
                )
                .expect("public subject");
        });
    }

    fn parser_program(query: &str) -> GqlProgram {
        gleaph_gql::parser::parse(query).expect("parse")
    }

    #[test]
    fn capsless_caller_without_tenancy_is_denied_adhoc_read_gql() {
        let p = principal(1);
        let program = parser_program("MATCH (n) RETURN n");
        // Default deny with ADR 0028 non-disclosure: an unknown caller cannot confirm any
        // graph exists, so the denial surfaces as the resolver's NotFound.
        assert!(matches!(
            authorize_adhoc_gql(&p, ProgramModificationFlags::default(), &program),
            Err(RouterError::NotFound(_))
        ));
    }

    #[test]
    fn any_caps_holder_may_run_read_and_write_adhoc_gql() {
        // Interim policy: one narrow cap admits ad-hoc GQL; write-path privilege checking is
        // slice-2 scope.
        let p = principal(2);
        upsert_caps(p, AdminCaps::PREPARE_REGISTER);
        let read = parser_program("MATCH (n) RETURN n");
        authorize_adhoc_gql(&p, ProgramModificationFlags::default(), &read).expect("read ok");

        let write_flags = ProgramModificationFlags {
            has_data_modification: true,
            ..Default::default()
        };
        let write = parser_program("MATCH (n) SET n.x = 1");
        authorize_adhoc_gql(&p, write_flags, &write).expect("write admitted under interim policy");
    }

    #[test]
    fn named_call_requires_call_procedure_cap_even_with_other_caps() {
        let p = principal(3);
        upsert_caps(p, AdminCaps::PREPARE_REGISTER);
        let call = parser_program("CALL GLEAPH.DRAIN_DEFERRED_MAINTENANCE()");
        let flags = ProgramModificationFlags {
            has_call_procedure: true,
            ..Default::default()
        };
        assert!(matches!(
            authorize_adhoc_gql(&p, flags, &call),
            Err(RouterError::Forbidden)
        ));

        upsert_caps(p, AdminCaps::PREPARE_REGISTER | AdminCaps::CALL_PROCEDURE);
        authorize_adhoc_gql(&p, flags, &call).expect("CALL_PROCEDURE admits named CALL");
    }

    #[test]
    fn anonymous_is_never_admitted_by_caps_arm() {
        let call = parser_program("CALL GLEAPH.FINALIZE_BACKFILL()");
        let flags = ProgramModificationFlags {
            has_call_procedure: true,
            ..Default::default()
        };
        assert!(matches!(
            authorize_adhoc_gql(&Principal::anonymous(), flags, &call),
            Err(RouterError::Forbidden)
        ));
    }

    #[test]
    fn owner_without_caps_runs_adhoc_gql_on_own_graph() {
        let store = crate::facade::store::RouterStore::new();
        let owner = principal(20);
        // Registration is topology control (MANAGE_FEDERATION); the registered owner need
        // not hold any cap. This mirrors dev-mode/provisioned flows where an operator
        // registers a graph owned by a tenant.
        let registrar = principal(21);
        crate::facade::auth::grant_admins(&[registrar]);
        store
            .admin_register_graph(
                registrar,
                gleaph_gql_ic::graph_registry::GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    canister_id: Principal::management_canister(),
                    owner,
                    admins: Default::default(),
                    status: gleaph_gql_ic::graph_registry::GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: gleaph_gql_ic::graph_registry::ProvisioningState::None,
                    is_home: true,
                },
                "rbac-owner-graph",
            )
            .expect("register graph");

        // Tenancy arm: an owner with an empty capability set still runs ad-hoc GQL against
        // the graph they own (ADR 0028 predicate arm of the interim policy).
        assert_eq!(auth::caps_of(&owner), AdminCaps::empty());
        let program = parser_program("MATCH (n) RETURN n");
        authorize_adhoc_gql(&owner, ProgramModificationFlags::default(), &program)
            .expect("owner passes via tenancy");
    }

    #[test]
    fn prepared_register_gate_uses_prepare_register_bit() {
        let without = principal(4);
        assert!(matches!(
            authorize_prepared_catalog_change(&without),
            Err(RouterError::Forbidden)
        ));

        let with = principal(5);
        upsert_caps(with, AdminCaps::PREPARE_REGISTER);
        authorize_prepared_catalog_change(&with).expect("ok");
    }

    #[test]
    fn index_ddl_rejects_anonymous_and_capless_principals() {
        // Guards the legacy `admin_set_indexed_{vertex,edge}_property` compat endpoints, which
        // route through `authorize_index_ddl` exactly like GQL `CREATE INDEX`.
        assert!(matches!(
            authorize_index_ddl(&Principal::anonymous()),
            Err(RouterError::Forbidden)
        ));
        let capless = principal(7);
        upsert_caps(capless, AdminCaps::MANAGE_CATALOG);
        assert!(matches!(
            authorize_index_ddl(&capless),
            Err(RouterError::Forbidden)
        ));
    }

    #[test]
    fn index_ddl_accepts_either_index_bit_but_not_unrelated_caps() {
        let creator = principal(8);
        upsert_caps(creator, AdminCaps::INDEX_CREATE);
        authorize_index_ddl(&creator).expect("INDEX_CREATE may run index DDL");

        let dropper = principal(9);
        upsert_caps(dropper, AdminCaps::INDEX_DROP);
        authorize_index_ddl(&dropper).expect("INDEX_DROP may run index DDL");
    }

    #[test]
    fn federation_family_requires_manage_federation() {
        let other = principal(12);
        upsert_caps(other, AdminCaps::PREPARE_REGISTER);
        assert!(matches!(
            authorize_stable_memory_diagnostics(&other),
            Err(RouterError::Forbidden)
        ));

        let fed = principal(13);
        upsert_caps(fed, AdminCaps::MANAGE_FEDERATION);
        authorize_stable_memory_diagnostics(&fed).expect("federation cap admits diagnostics");
        authorize_vector_activation(&fed).expect("federation cap admits activation");
        authorize_vector_maintenance(&fed).expect("federation cap admits maintenance");
    }

    #[test]
    fn prepared_execution_is_default_deny_without_grants() {
        let p = principal(14);
        let graph_id = GraphId::from_raw(701);
        // Even full-caps holders are denied on the data plane when they hold no grant and are
        // not the bound graph's tenant (ADR 0074 invariant 1: authority ≠ data access).
        upsert_caps(p, AdminCaps::all());
        assert!(matches!(
            authorize_prepared_execute(&p, "q1", graph_id),
            Err(RouterError::Forbidden)
        ));
    }

    #[test]
    fn prepared_execution_via_public_or_explicit_grant() {
        grant_public_exec("public-q");
        let stranger = principal(15);
        authorize_prepared_execute(&stranger, "public-q", GraphId::from_raw(1))
            .expect("PUBLIC baseline applies");
        assert!(matches!(
            authorize_prepared_execute(&stranger, "other-q", GraphId::from_raw(1)),
            Err(RouterError::Forbidden)
        ));

        ROUTER_AUTH_GRANTS.with_borrow_mut(|grants| {
            grants
                .grant(
                    GrantSubject::Principal(principal(16)),
                    &Privilege::ExecutePreparedQuery {
                        name: "private-q".into(),
                    },
                    None,
                )
                .expect("explicit grant");
        });
        authorize_prepared_execute(&principal(16), "private-q", GraphId::from_raw(2))
            .expect("explicit grant applies");
        assert!(matches!(
            authorize_prepared_execute(&principal(17), "private-q", GraphId::from_raw(2)),
            Err(RouterError::Forbidden)
        ));
    }

    #[test]
    fn anonymous_prepared_execution_resolves_only_the_public_row() {
        assert!(matches!(
            authorize_prepared_execute(&Principal::anonymous(), "any-q", GraphId::from_raw(3)),
            Err(RouterError::Forbidden)
        ));
        grant_public_exec("anon-q");
        authorize_prepared_execute(&Principal::anonymous(), "anon-q", GraphId::from_raw(3))
            .expect("anonymous executes via PUBLIC");
    }

    #[test]
    fn ownership_derived_arm_admits_only_the_bound_graphs_tenants() {
        let store = crate::facade::store::RouterStore::new();
        let owner = principal(18);
        crate::facade::auth::grant_admins(&[owner]);
        store
            .admin_register_graph(
                owner,
                gleaph_gql_ic::graph_registry::GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    canister_id: Principal::management_canister(),
                    owner,
                    admins: Default::default(),
                    status: gleaph_gql_ic::graph_registry::GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: gleaph_gql_ic::graph_registry::ProvisioningState::None,
                    is_home: false,
                },
                "rbac-ownership-graph",
            )
            .expect("register graph");

        // No grant rows exist for this query; the owner executes through the registry SSOT.
        // The store assigned the canonical GraphId at registration — use it, not a fabricated id.
        let bound_graph_id =
            crate::facade::stable::graph_catalog::lookup_graph_id("rbac-ownership-graph")
                .expect("registered graph resolves");
        authorize_prepared_execute(&owner, "owner-only-q", bound_graph_id)
            .expect("registry owner derives execution authority");
        // A full-caps non-tenant still cannot execute it.
        let superuser = principal(19);
        upsert_caps(superuser, AdminCaps::all());
        assert!(matches!(
            authorize_prepared_execute(&superuser, "owner-only-q", bound_graph_id),
            Err(RouterError::Forbidden)
        ));
    }
}
