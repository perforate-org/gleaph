//! Role checks for router GQL entrypoints (stable auth via [`crate::facade::auth`]).

use candid::Principal;
use gleaph_auth::Role;
use gleaph_gql::program_modification::ProgramModificationFlags;

use crate::facade::auth;
use crate::state::RouterError;

/// Ad-hoc GQL (`gql_query` / `gql_execute` / `force_gql_execute`): at least Read; write programs need Write.
pub fn authorize_adhoc_gql(
    caller: &Principal,
    flags: ProgramModificationFlags,
) -> Result<(), RouterError> {
    let role = auth::caller_role(caller);
    if !role.satisfies_at_least(Role::Read) {
        return Err(RouterError::Forbidden);
    }
    if flags.requires_write_path() && !role.satisfies_at_least(Role::Write) {
        return Err(RouterError::Forbidden);
    }
    Ok(())
}

/// Prepared execution: any principal with effective role (default Executor) may run registered plans.
pub fn authorize_prepared_execute(caller: &Principal) -> Result<(), RouterError> {
    let _ = auth::caller_role(caller);
    Ok(())
}

/// Index DDL (`CREATE INDEX` / `DROP INDEX`): Admin or Manager with `PREPARE_REGISTER`.
pub fn authorize_index_ddl(caller: &Principal) -> Result<(), RouterError> {
    if auth::is_admin(caller) || auth::can_prepare_register(caller) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

/// Global vector-dispatch activation control + shard vector-attach (ADR 0031 Slice 4): Admin only.
/// These are control-plane operations that change cross-graph dispatch behavior, so they require
/// the strongest role rather than the per-graph index-DDL capability.
pub fn authorize_vector_activation(caller: &Principal) -> Result<(), RouterError> {
    if auth::is_admin(caller) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

/// Vector-index maintenance forwarding + policy control (ADR 0031 Slice 10): Admin only. Like
/// [`authorize_vector_activation`], these are derived-index control-plane operations (rebuild,
/// cleanup, cache, bounded maintenance scheduling), so they require the strongest role.
pub fn authorize_vector_maintenance(caller: &Principal) -> Result<(), RouterError> {
    if auth::is_admin(caller) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

/// `prepared_register` / `prepared_drop`: Admin or Manager with `PREPARE_REGISTER`.
pub fn authorize_prepared_catalog_change(caller: &Principal) -> Result<(), RouterError> {
    if auth::can_prepare_register(caller) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_auth::{AuthRecord, ManagerCapability};
    use gleaph_gql::program_modification::ProgramModificationFlags;

    fn principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    fn upsert_role(p: Principal, role: Role, manager_caps: u64) {
        crate::facade::stable::ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
            auth.upsert_record(
                p,
                AuthRecord {
                    role: role as u8,
                    manager_caps,
                },
            )
            .expect("test principal must be non-anonymous");
        });
    }

    #[test]
    fn executor_cannot_run_adhoc_read_gql() {
        let p = principal(1);
        let flags = ProgramModificationFlags::default();
        assert!(matches!(
            authorize_adhoc_gql(&p, flags),
            Err(RouterError::Forbidden)
        ));
    }

    #[test]
    fn read_role_allows_adhoc_read_gql() {
        let p = principal(2);
        upsert_role(p, Role::Read, 0);
        authorize_adhoc_gql(&p, ProgramModificationFlags::default()).expect("read ok");
    }

    #[test]
    fn write_required_for_dml_program() {
        let p = principal(3);
        upsert_role(p, Role::Read, 0);
        let flags = ProgramModificationFlags {
            has_data_modification: true,
            ..Default::default()
        };
        assert!(matches!(
            authorize_adhoc_gql(&p, flags),
            Err(RouterError::Forbidden)
        ));
    }

    #[test]
    fn manager_without_cap_cannot_register_prepared() {
        let p = principal(4);
        upsert_role(p, Role::Manager, 0);
        assert!(matches!(
            authorize_prepared_catalog_change(&p),
            Err(RouterError::Forbidden)
        ));
    }

    #[test]
    fn manager_with_prepare_cap_can_register() {
        let p = principal(5);
        upsert_role(p, Role::Manager, ManagerCapability::PREPARE_REGISTER.bits());
        authorize_prepared_catalog_change(&p).expect("ok");
    }

    #[test]
    fn index_ddl_rejects_anonymous_and_read_roles() {
        // Guards the legacy `admin_set_indexed_{vertex,edge}_property` compat endpoints, which
        // route through `authorize_index_ddl` exactly like GQL `CREATE INDEX`.
        let anon = Principal::anonymous();
        assert!(matches!(
            authorize_index_ddl(&anon),
            Err(RouterError::Forbidden)
        ));
        let read = principal(7);
        upsert_role(read, Role::Read, 0);
        assert!(matches!(
            authorize_index_ddl(&read),
            Err(RouterError::Forbidden)
        ));
    }

    #[test]
    fn index_ddl_allows_admin_and_manager_with_prepare_cap() {
        let admin = principal(8);
        upsert_role(admin, Role::Admin, 0);
        authorize_index_ddl(&admin).expect("admin may run index DDL");

        let manager = principal(9);
        upsert_role(
            manager,
            Role::Manager,
            ManagerCapability::PREPARE_REGISTER.bits(),
        );
        authorize_index_ddl(&manager).expect("manager with prepare cap may run index DDL");
    }

    #[test]
    fn default_executor_may_execute_prepared() {
        let p = principal(6);
        authorize_prepared_execute(&p).expect("executor default");
    }

    #[test]
    fn anonymous_may_execute_prepared() {
        // Product contract: intentionally public prepared execution stays available to the
        // anonymous (default Executor) caller.
        let anon = Principal::anonymous();
        assert_eq!(auth::caller_role(&anon), Role::Executor);
        authorize_prepared_execute(&anon).expect("anonymous default executor may run prepared");
    }
}
