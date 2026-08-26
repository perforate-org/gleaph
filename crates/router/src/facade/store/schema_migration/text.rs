//! Router-owned migration-driven TEXT backfill lifecycle (plan 0297 `backfill-pull`;
//! ADR 0059 §Text build kind).
//!
//! Unlike the property-posting lane, a TEXT backfill migrates an ALREADY-PROVISIONED
//! definition: issuance (ADR 0035) assigned the text canister when the definition was
//! declared, so `Preparing` only has to resolve durable identities — the never-reused
//! physical export namespace, the prepared catalog epoch, and the home Graph shard — inside
//! one synchronous co-write with the ledger record. The pending lifecycle then mirrors the
//! property spine: bounded Register/Build steps through the text canister, a Sealing fence
//! that freezes the shared Graph export scope and captures its admission watermark, and
//! convergence gated on BOTH the text-canister scan being done AND that watermark being
//! flushed. Convergence flips the definition to [`TextIndexStatus::Ready`] — the only
//! planner/query-visible state — before the migration can reach `Applied`.

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::TextIndexId;
use gleaph_graph_kernel::index::PhysicalIndexId;
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationResult, ApplySchemaMigrationResultV1,
    MigrationFailureCode, ResolvedSchemaMigrationGraph, SchemaMigrationApplyStatus,
    SchemaMigrationProgress, SchemaMigrationProgressPhase, SchemaMigrationRecord,
    SchemaMigrationRecordState, SchemaMigrationRecordV1, SchemaMigrationStatementProfile,
};

use super::super::RouterStore;
use super::driver::TextBackfillStepResult;
use super::index::{IndexMigrationDriveError, IndexMigrationDriver, IndexMigrationStepAction};
use crate::facade::auth;
use crate::facade::stable::ROUTER_SCHEMA_MIGRATIONS;
use crate::facade::stable::graph_catalog;
use crate::facade::stable::index_name_catalog::lookup_index_name_id;
use crate::facade::stable::indexed_catalog;
use crate::facade::stable::schema_migration::StableSchemaMigrationRecord;
use crate::facade::stable::text_index_catalog::{self, TextBackfillBuildPhase};
use crate::state::RouterError;

pub(super) async fn apply_text_index_migration<D: IndexMigrationDriver>(
    store: &RouterStore,
    caller: Principal,
    args: ApplySchemaMigrationArgs,
    driver: &D,
) -> Result<ApplySchemaMigrationResult, RouterError> {
    auth::require_cap(&caller, gleaph_auth::AdminCaps::MANAGE_CATALOG)?;
    let ApplySchemaMigrationArgs::V1(args) = args;

    let statement = match crate::index_ddl::try_parse_text(&args.statement) {
        Some(Ok(statement)) => statement,
        Some(Err(error)) => {
            return Err(RouterError::InvalidArgument(format!(
                "invalid migration CREATE TEXT INDEX syntax: {error}"
            )));
        }
        None => {
            return Err(RouterError::InvalidArgument(
                "expected exactly one CREATE TEXT INDEX migration statement".into(),
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

    // Exact replay: the same id/checksum resumes its pending lifecycle without re-resolving
    // anything; a different payload for a recorded id conflicts before any effect.
    let crate::index_ddl::TextIndexDdlStatement::Create {
        index_name,
        label,
        property,
    } = statement;
    if let Some(existing) = ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.get(&args.id)) {
        let existing_v1 = super::record_v1(&existing.0)?;
        if !super::record_matches_args(existing_v1, &args)
            || !existing_v1
                .profile
                .iter()
                .all(|profile| *profile == SchemaMigrationStatementProfile::CreateTextBackfill)
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
                    record: existing.0,
                },
            )),
            SchemaMigrationRecordState::Failed { code, .. } => Ok(ApplySchemaMigrationResult::V1(
                ApplySchemaMigrationResultV1 {
                    status: SchemaMigrationApplyStatus::Failed(*code),
                    record: existing.0,
                },
            )),
            SchemaMigrationRecordState::PendingIndex { .. } => {
                advance_text_backfill_migration(existing.0, driver).await
            }
        };
    }

    super::index::validate_new_chain(&args)?;
    if super::index::pending_migration_exists() {
        return Err(RouterError::Conflict(
            "another schema migration is still pending".into(),
        ));
    }

    // The definition must already exist, be provisioned, and still be awaiting backfill.
    let def = resolve_backfill_definition(graph_id, &index_name, &label, &property)?;

    // Fallible identity resolution BEFORE the first durable write.
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    let home = shards
        .iter()
        .min_by_key(|shard| shard.shard_id.raw())
        .ok_or_else(|| {
            RouterError::InvalidArgument(
                "CREATE TEXT INDEX migration requires at least one live shard".into(),
            )
        })?;
    if home.graph_canister == Principal::anonymous() {
        return Err(RouterError::InvalidArgument(
            "CREATE TEXT INDEX migration resolved an anonymous home graph canister".into(),
        ));
    }
    let routes: Vec<(u32, Principal, Principal)> = shards
        .iter()
        .map(|shard| {
            (
                shard.shard_id.raw(),
                shard.graph_canister,
                shard.index_canister,
            )
        })
        .collect();
    let topology_epoch = super::index::topology_epoch(&routes);
    let index_name_id = lookup_index_name_id(graph_id, &index_name).ok_or_else(|| {
        RouterError::InvalidState("text definition lost its interned name".into())
    })?;

    // Synchronous co-write: physical namespace + catalog epoch + build record + ledger row.
    let (physical_index_id, prepared_catalog_epoch) =
        indexed_catalog::prepare_text_backfill_identity()?;
    text_index_catalog::prepare_text_backfill_build(
        graph_id,
        def.text_index_id,
        text_index_catalog::TextBackfillBuildRecord {
            migration_id: args.id.clone(),
            topology_epoch,
            prepared_catalog_epoch,
            physical_index_id,
            home_shard_id: home.shard_id.raw(),
            home_graph_canister: home.graph_canister,
            registered: false,
            phase: TextBackfillBuildPhase::Building,
        },
    )?;
    let pending = vec![gleaph_migration_api::PendingIndexBuild {
        index_name_id,
        physical_index_id,
    }];
    let record = SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
        id: args.id.clone(),
        parent: args.parent,
        graph_selector: args.graph_selector,
        resolved_graph: Some(resolved_graph),
        checksum: args.checksum,
        actor: caller,
        recorded_at: super::super::ic_time_ns(),
        statement: args.statement,
        profile: vec![SchemaMigrationStatementProfile::CreateTextBackfill],
        state: SchemaMigrationRecordState::PendingIndex { pending },
    });
    ROUTER_SCHEMA_MIGRATIONS.with_borrow_mut(|ledger| {
        ledger.insert(args.id, StableSchemaMigrationRecord(record.clone()));
    });
    Ok(ApplySchemaMigrationResult::V1(
        ApplySchemaMigrationResultV1 {
            status: SchemaMigrationApplyStatus::Progress(text_progress(
                SchemaMigrationProgressPhase::Preparing,
            )),
            record,
        },
    ))
}

/// Resolves the provisioned definition a text backfill migrates. The declaration must match
/// the existing definition exactly — migrations drive backfill, they never redefine.
fn resolve_backfill_definition(
    graph_id: GraphId,
    index_name: &str,
    label: &str,
    property: &str,
) -> Result<text_index_catalog::TextIndexDefRecord, RouterError> {
    use crate::facade::stable::indexed_catalog::get_named_index;

    let name_id = lookup_index_name_id(graph_id, index_name).ok_or_else(|| {
        RouterError::NotFound(format!(
            "no TEXT definition named {index_name}; declare it via CREATE TEXT INDEX DDL first"
        ))
    })?;
    if get_named_index(graph_id, name_id).is_some() {
        return Err(RouterError::Conflict(format!(
            "index name already belongs to a property index: {index_name}"
        )));
    }
    let def = text_index_catalog::get_text_index_by_name_id(graph_id, name_id)
        .ok_or_else(|| RouterError::NotFound(format!("no TEXT definition named {index_name}")))?;
    let label_id = RouterStore::new().lookup_vertex_label_id(graph_id, label)?;
    let property_id = RouterStore::new().lookup_property_id(graph_id, property)?;
    if def.label_id != label_id || def.property_id != property_id {
        return Err(RouterError::Conflict(format!(
            "text index already exists with a different declaration: {index_name}"
        )));
    }
    let target = def.target.ok_or_else(|| {
        RouterError::InvalidArgument(
            "TEXT backfill requires a provisioned text canister; dev-mode definitions have no backfill"
                .into(),
        )
    })?;
    if target == Principal::anonymous() {
        return Err(RouterError::InvalidArgument(
            "TEXT backfill requires a non-anonymous text canister".into(),
        ));
    }
    Ok(def)
}

fn text_progress(phase: SchemaMigrationProgressPhase) -> SchemaMigrationProgress {
    SchemaMigrationProgress {
        phase,
        completed_targets: 0,
        total_targets: 1,
        active_index: 0,
        total_indexes: 1,
    }
}

/// Drives one pending TEXT backfill by one bounded step. A converged sub-build drops its
/// build record and reports terminal progress; the ledger reaches `Applied` on the NEXT
/// apply, which observes Ready-without-record and short-circuits (crash between the flip
/// and the ledger write resumes exactly here).
async fn advance_text_backfill_migration<D: IndexMigrationDriver>(
    migration_record: SchemaMigrationRecord,
    driver: &D,
) -> Result<ApplySchemaMigrationResult, RouterError> {
    let record = super::record_v1(&migration_record)?.clone();
    let SchemaMigrationRecordState::PendingIndex { pending } = &record.state else {
        return Err(RouterError::InvalidState(
            "advance_text_backfill requires a pending text backfill migration".into(),
        ));
    };
    let Some(resolved) = &record.resolved_graph else {
        return Err(RouterError::InvalidState(
            "pending text backfill migration has no resolved graph".into(),
        ));
    };
    let graph_id = resolved.graph_id;
    for build_pointer in pending {
        let Some((def, build)) = text_index_catalog::get_text_backfill_build_by_name_id(
            graph_id,
            build_pointer.index_name_id,
        ) else {
            // No record left + Ready definition == this sub-build already converged in a
            // prior drive whose ledger write did not land (exact crash-window resume).
            continue;
        };
        validate_pending_identity(&record, build_pointer.physical_index_id, &def, &build)?;
        if build.phase == TextBackfillBuildPhase::Converged {
            continue;
        }
        return advance_one_text(graph_id, def, build, driver)
            .await
            .map_err(|error| match error {
                IndexMigrationDriveError::Retryable => RouterError::Busy {
                    operation: "schema_migration.text_backfill_driver".into(),
                },
                IndexMigrationDriveError::Terminal(code) => {
                    RouterError::Conflict(format!("text backfill failed: {code:?}"))
                }
            })
            .map(|phase| {
                ApplySchemaMigrationResult::V1(ApplySchemaMigrationResultV1 {
                    status: SchemaMigrationApplyStatus::Progress(text_progress(phase)),
                    record: migration_record,
                })
            });
    }
    Ok(ApplySchemaMigrationResult::V1(
        ApplySchemaMigrationResultV1 {
            status: SchemaMigrationApplyStatus::Applied,
            record: SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
                state: SchemaMigrationRecordState::Applied {
                    applied_at: super::super::ic_time_ns(),
                },
                ..record.clone()
            }),
        },
    ))
}

fn validate_pending_identity(
    migration: &SchemaMigrationRecordV1,
    physical_index_id: PhysicalIndexId,
    def: &text_index_catalog::TextIndexDefRecord,
    build: &text_index_catalog::TextBackfillBuildRecord,
) -> Result<(), RouterError> {
    if build.migration_id != migration.id
        || build.physical_index_id != physical_index_id
        || build.prepared_catalog_epoch == 0
    {
        return Err(RouterError::Conflict(
            "pending text backfill identity mismatch".into(),
        ));
    }
    let _ = def;
    Ok(())
}

/// One bounded step for one sub-build, mapped over the durable build phase.
async fn advance_one_text<D: IndexMigrationDriver>(
    graph_id: GraphId,
    def: text_index_catalog::TextIndexDefRecord,
    build: text_index_catalog::TextBackfillBuildRecord,
    driver: &D,
) -> Result<SchemaMigrationProgressPhase, IndexMigrationDriveError> {
    let action = match build.phase {
        TextBackfillBuildPhase::Building if !build.registered => IndexMigrationStepAction::Register,
        TextBackfillBuildPhase::Building => IndexMigrationStepAction::Build,
        TextBackfillBuildPhase::Sealing => IndexMigrationStepAction::Seal,
        TextBackfillBuildPhase::Converged => return Ok(SchemaMigrationProgressPhase::Sealing),
    };
    // Sealing freezes the shared Graph scope at a FRESH epoch; Building steps present the
    // prepared registration epoch.
    let lifecycle_epoch = match action {
        IndexMigrationStepAction::Seal => {
            indexed_catalog::advance_index_catalog_epoch_for_text_seal()
                .map_err(|_| IndexMigrationDriveError::Retryable)?
        }
        _ => build.prepared_catalog_epoch,
    };
    let request = super::driver::text_step_request(
        build.migration_id.clone(),
        lifecycle_epoch,
        text_registration(graph_id, &def, &build),
        build.home_shard_id,
        def.target.expect("provisioned definition"),
        action,
    );
    let response = driver.drive_text_backfill(request).await?;
    match action {
        IndexMigrationStepAction::Register => {
            if response.result != TextBackfillStepResult::Registered {
                return Err(IndexMigrationDriveError::Terminal(
                    MigrationFailureCode::StaleOrMismatchedResponse,
                ));
            }
            text_index_catalog::mark_text_backfill_registered(graph_id, def.text_index_id)
                .map_err(|_| IndexMigrationDriveError::Retryable)?;
            Ok(SchemaMigrationProgressPhase::Building)
        }
        IndexMigrationStepAction::Build => match response.result {
            TextBackfillStepResult::BuildProgress { done, .. } => {
                if done {
                    transition_build(
                        graph_id,
                        def.text_index_id,
                        &build,
                        TextBackfillBuildPhase::Sealing,
                    )?;
                    Ok(SchemaMigrationProgressPhase::Sealing)
                } else {
                    Ok(SchemaMigrationProgressPhase::Building)
                }
            }
            _ => Err(IndexMigrationDriveError::Terminal(
                MigrationFailureCode::StaleOrMismatchedResponse,
            )),
        },
        IndexMigrationStepAction::Seal => match response.result {
            TextBackfillStepResult::SealProgress { converged } if converged => {
                transition_build(
                    graph_id,
                    def.text_index_id,
                    &build,
                    TextBackfillBuildPhase::Converged,
                )?;
                // Readiness flip: the definition becomes planner-visible ONLY here, after
                // both convergence gates proved out.
                text_index_catalog::complete_text_backfill(graph_id, def.text_index_id)
                    .map_err(|_| IndexMigrationDriveError::Retryable)?;
                text_index_catalog::drop_text_backfill_build(graph_id, def.text_index_id);
                Ok(SchemaMigrationProgressPhase::Sealing)
            }
            TextBackfillStepResult::SealProgress { converged: false } => {
                Ok(SchemaMigrationProgressPhase::Sealing)
            }
            _ => Err(IndexMigrationDriveError::Terminal(
                MigrationFailureCode::StaleOrMismatchedResponse,
            )),
        },
        IndexMigrationStepAction::Cleanup => unreachable!("cleanup is driven by the failure path"),
    }
}

/// Validated durable phase transition for one text backfill build record.
fn transition_build(
    graph_id: GraphId,
    text_index_id: u32,
    build: &text_index_catalog::TextBackfillBuildRecord,
    next: TextBackfillBuildPhase,
) -> Result<(), IndexMigrationDriveError> {
    text_index_catalog::transition_text_backfill_build(graph_id, text_index_id, next)
        .map_err(|_| IndexMigrationDriveError::Retryable)?;
    let _ = build;
    Ok(())
}

fn text_registration(
    graph_id: GraphId,
    def: &text_index_catalog::TextIndexDefRecord,
    build: &text_index_catalog::TextBackfillBuildRecord,
) -> crate::facade::store::schema_migration::text_backfill::RegisterTextBackfillRequest {
    crate::facade::store::schema_migration::text_backfill::RegisterTextBackfillRequest {
        text_index_id: TextIndexId::new(def.text_index_id),
        // The GRAPH shard serving canonical export pages to the text canister's pull.
        graph_canister: build.home_graph_canister,
        graph_id,
        index_name_id: def.index_name_id,
        physical_index_id: build.physical_index_id,
        catalog_epoch: build.prepared_catalog_epoch,
        scope: crate::facade::store::schema_migration::text_backfill::TextBackfillScope {
            label_id: def.label_id.raw(),
            property_id: def.property_id,
            analyzer_id: def.analyzer_id,
        },
    }
}
