//! Router-owned synchronous application of a migration-driven `CREATE VECTOR INDEX` (ADR 0071).
//!
//! Unlike the property-index build (ADR 0059), a vector index is a one-shot Router catalog write:
//! the vector canister owns embedding bytes and there is no cross-canister backfill or export build.
//! In provisioned mode the graph's first vector target is provisioned (a remote side effect whose
//! idempotency is owned by the shared Provision request store) before the definition is registered;
//! in dev mode the definition is registered targetless (`Registered`), matching the generic GQL
//! path. The migration ledger is then written synchronously as `Applied`.

use candid::Principal;
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationResult, ApplySchemaMigrationResultV1,
    ResolvedSchemaMigrationGraph, SchemaMigrationApplyStatus, SchemaMigrationRecord,
    SchemaMigrationRecordState, SchemaMigrationRecordV1, SchemaMigrationStatementProfile,
};

use super::super::RouterStore;
use super::index::IndexMigrationDriver;
use crate::facade::auth;
use crate::facade::stable::ROUTER_SCHEMA_MIGRATIONS;
use crate::facade::stable::graph_catalog;
use crate::facade::stable::schema_migration::StableSchemaMigrationRecord;
use crate::index_catalog::execute_vector_index_ddl_for_graph;
use crate::state::RouterError;

pub(super) async fn apply_vector_index_migration<D: IndexMigrationDriver>(
    store: &RouterStore,
    caller: Principal,
    args: ApplySchemaMigrationArgs,
    _driver: &D,
) -> Result<ApplySchemaMigrationResult, RouterError> {
    auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_CATALOG)?;
    let ApplySchemaMigrationArgs::V1(args) = args;
    super::validate_apply_args(&args)?;

    let statement = match crate::index_ddl::try_parse_vector(&args.statement) {
        Some(Ok(statement)) => statement,
        Some(Err(error)) => {
            return Err(RouterError::InvalidArgument(format!(
                "invalid migration CREATE VECTOR INDEX syntax: {error}"
            )));
        }
        None => {
            return Err(RouterError::InvalidArgument(
                "expected exactly one CREATE VECTOR INDEX migration statement".into(),
            ));
        }
    };

    let checksum = gleaph_migration_api::schema_migration_checksum(
        &args.id,
        args.parent.as_deref(),
        &args.graph_selector,
        args.statement.as_bytes(),
    );
    if args.checksum != checksum {
        return Err(RouterError::InvalidArgument(
            "schema migration checksum does not match the exact request envelope".into(),
        ));
    }

    // A CREATE VECTOR INDEX migration is always single-statement and graph-specific.
    let graph_id = match &args.graph_selector {
        gleaph_migration_api::SchemaMigrationGraphSelector::Default => {
            store.resolve_home_graph_id(caller)?
        }
        gleaph_migration_api::SchemaMigrationGraphSelector::Named(name) => {
            store.resolve_graph_id_authorized(name, caller)?
        }
    };
    let graph_name = graph_catalog::graph_name(graph_id).ok_or_else(|| {
        RouterError::NotFound(format!("canonical graph name for graph {}", graph_id.raw()))
    })?;
    let resolved_graph = ResolvedSchemaMigrationGraph {
        graph_id,
        graph_name,
    };

    if let Some(existing) = ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.get(&args.id)) {
        let existing = existing.0;
        let existing_v1 = super::record_v1(&existing)?;
        if !super::record_matches_args(existing_v1, &args)
            || !existing_v1
                .profile
                .iter()
                .all(|profile| *profile == SchemaMigrationStatementProfile::CreateVectorIndex)
        {
            return Err(RouterError::Conflict(format!(
                "schema migration id `{}` already exists with a different payload",
                args.id
            )));
        }
        return match &existing_v1.state {
            SchemaMigrationRecordState::Applied { .. } => Ok(ApplySchemaMigrationResult::V1(
                ApplySchemaMigrationResultV1 {
                    status: SchemaMigrationApplyStatus::Replay,
                    record: existing,
                },
            )),
            SchemaMigrationRecordState::Failed { code, .. } => Ok(ApplySchemaMigrationResult::V1(
                ApplySchemaMigrationResultV1 {
                    status: SchemaMigrationApplyStatus::Failed(*code),
                    record: existing,
                },
            )),
            SchemaMigrationRecordState::PendingIndex { .. } => Err(RouterError::InvalidState(
                "vector index migration record unexpectedly entered the property-index lifecycle"
                    .into(),
            )),
        };
    }

    super::index::validate_new_chain(&args)?;

    // Provision (cross-canister, idempotent via the shared Provision request store) and register
    // the vector definition. In dev mode (no provision_canister) this registers targetless.
    execute_vector_index_ddl_for_graph(graph_id, statement)
        .await
        .map_err(|error| match error {
            RouterError::Busy { .. } => RouterError::Busy {
                operation: "schema_migration.vector_index".into(),
            },
            other => other,
        })?;

    let recorded_at = super::super::ic_time_ns();
    let record = SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
        id: args.id.clone(),
        parent: args.parent,
        graph_selector: args.graph_selector,
        resolved_graph: Some(resolved_graph),
        checksum: args.checksum,
        actor: caller,
        recorded_at,
        statement: args.statement,
        profile: vec![SchemaMigrationStatementProfile::CreateVectorIndex],
        state: SchemaMigrationRecordState::Applied {
            applied_at: recorded_at,
        },
    });
    ROUTER_SCHEMA_MIGRATIONS.with_borrow_mut(|ledger| {
        ledger.insert(args.id, StableSchemaMigrationRecord(record.clone()));
    });
    Ok(ApplySchemaMigrationResult::V1(
        ApplySchemaMigrationResultV1 {
            status: SchemaMigrationApplyStatus::Applied,
            record,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::auth;
    use crate::facade::stable::{graph_catalog, vector_index_catalog};
    use crate::facade::store::tests::{register_test_graph, test_init_args};
    use gleaph_migration_api::SchemaMigrationGraphSelector;

    const VECTOR_DDL: &str = "CREATE VECTOR INDEX document_embedding FOR (d:Document) ON d.embedding OPTIONS { dimensions: 768, metric: \"cosine\", encoding: \"i8\", algorithm: \"ivf_flat\" }";

    struct NoopDriver;
    impl IndexMigrationDriver for NoopDriver {
        fn drive(
            &self,
            _request: super::super::index::IndexMigrationStepRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            super::super::index::IndexMigrationStepResponse,
                            super::super::index::IndexMigrationDriveError,
                        >,
                    > + '_,
            >,
        > {
            unreachable!("vector migration does not drive a property-index step")
        }
    }

    fn vector_args(id: &str, parent: Option<&str>) -> ApplySchemaMigrationArgs {
        let graph_selector = SchemaMigrationGraphSelector::Named("tenant.main".into());
        ApplySchemaMigrationArgs::V1(gleaph_migration_api::ApplySchemaMigrationArgsV1 {
            id: id.into(),
            parent: parent.map(str::to_owned),
            graph_selector: graph_selector.clone(),
            checksum: gleaph_migration_api::schema_migration_checksum(
                id,
                parent,
                &graph_selector,
                VECTOR_DDL.as_bytes(),
            ),
            statement: VECTOR_DDL.into(),
        })
    }

    #[test]
    fn vector_migration_applies_synchronously_as_applied() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([21; 32]);
        auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");
        let graph_id = graph_catalog::lookup_graph_id("tenant.main").expect("registered graph");
        RouterStore::commit_intern_vertex_label_name(graph_id, "Document").expect("vertex label");

        let result = futures::executor::block_on(store.admin_apply_schema_migration_control(
            admin,
            vector_args("000001_vec", None),
            &NoopDriver,
        ))
        .expect("vector migration applies");
        let ApplySchemaMigrationResult::V1(result) = &result;
        assert_eq!(result.status, SchemaMigrationApplyStatus::Applied);
        let SchemaMigrationRecord::V1(record) = &result.record;
        assert_eq!(
            record.profile,
            vec![SchemaMigrationStatementProfile::CreateVectorIndex]
        );
        assert!(record.resolved_graph.is_some());
        assert!(matches!(
            record.state,
            SchemaMigrationRecordState::Applied { .. }
        ));
        // The definition is registered targetless in dev mode.
        let index_name_id = crate::facade::stable::index_name_catalog::intern_index_name(
            graph_id,
            "document_embedding",
        )
        .expect("interned index name");
        assert!(
            vector_index_catalog::get_vector_index_by_name_id(graph_id, index_name_id).is_some()
        );
    }

    #[test]
    fn vector_migration_replays_existing_applied_envelope() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([22; 32]);
        auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");
        let graph_id = graph_catalog::lookup_graph_id("tenant.main").expect("registered graph");
        RouterStore::commit_intern_vertex_label_name(graph_id, "Document").expect("vertex label");

        let args = vector_args("000001_vec", None);
        futures::executor::block_on(store.admin_apply_schema_migration_control(
            admin,
            args.clone(),
            &NoopDriver,
        ))
        .expect("first apply");
        let result = futures::executor::block_on(store.admin_apply_schema_migration_control(
            admin,
            args,
            &NoopDriver,
        ))
        .expect("replay");
        let ApplySchemaMigrationResult::V1(result) = &result;
        assert_eq!(result.status, SchemaMigrationApplyStatus::Replay);
    }
}
