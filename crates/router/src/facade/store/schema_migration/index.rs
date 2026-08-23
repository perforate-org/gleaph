//! Router-owned bounded orchestration for migration-driven CREATE INDEX (ADR 0059).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use candid::Principal;
use gleaph_graph_kernel::canonical_export::{
    CanonicalExportScope, CanonicalExportTarget, CanonicalInlineProjection, CanonicalRecordSource,
};
use gleaph_graph_kernel::entry::{EdgeLabelId, GraphId, IndexNameId, PropertyId};
use gleaph_graph_kernel::index::{
    IndexBuildStatus, IndexBuildTarget, PhysicalIndexId, RegisterIndexBuildRequest,
};
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationResult, ApplySchemaMigrationResultV1,
    MigrationFailureCode, PendingIndexBuild, ResolvedSchemaMigrationGraph,
    SchemaMigrationApplyStatus, SchemaMigrationChecksumAlgorithm, SchemaMigrationProgress,
    SchemaMigrationProgressPhase, SchemaMigrationRecord, SchemaMigrationRecordState,
    SchemaMigrationRecordV1, SchemaMigrationStatementProfile,
};
use sha2::{Digest, Sha256};

use super::super::RouterStore;
use crate::facade::auth;
use crate::facade::stable::index_name_catalog::{intern_index_name, lookup_index_name_id};
use crate::facade::stable::indexed_catalog::{
    self, IndexBuildMetadata, IndexDefRecord, IndexLifecycleState, IndexLifecycleTarget,
    IndexLifecycleTargetState, IndexShardWatermark, PrepareIndexLifecycleArgs,
};
use crate::facade::stable::schema_migration::StableSchemaMigrationRecord;
use crate::facade::stable::{ROUTER_SCHEMA_MIGRATIONS, graph_catalog};
use crate::index_catalog::resolve_index_definition;
use crate::state::RouterError;

/// One bounded graph-index owner operation. Router never carries canonical export pages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexMigrationStepAction {
    Register,
    Build,
    Seal,
    Cleanup,
}

/// Exact replay envelope for one downstream target operation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IndexMigrationExportScope {
    pub shard_id: u32,
    pub graph_canister: Principal,
    pub scope: CanonicalExportScope,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IndexMigrationStepRequest {
    pub migration_id: String,
    pub topology_epoch: u64,
    pub lifecycle_catalog_epoch: u64,
    pub index_canister: Principal,
    pub registration: RegisterIndexBuildRequest,
    pub export_scopes: Vec<IndexMigrationExportScope>,
    pub action: IndexMigrationStepAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum IndexMigrationStepResult {
    Registered(IndexBuildStatus),
    BuildProgress(IndexBuildStatus),
    SealProgress {
        watermarks: Vec<IndexShardWatermark>,
        converged: bool,
    },
    CleanupProgress {
        done: bool,
    },
}

/// Response echoes the immutable request identity so stale and cross-target replies fail closed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IndexMigrationStepResponse {
    pub migration_id: String,
    pub topology_epoch: u64,
    pub lifecycle_catalog_epoch: u64,
    pub index_canister: Principal,
    pub registration: RegisterIndexBuildRequest,
    pub export_scopes: Vec<IndexMigrationExportScope>,
    pub result: IndexMigrationStepResult,
}

/// Outcome of advancing one migration-driven index build by one bounded step. The migration ledger
/// is untouched here; the sequence driver maps these to migration-level status.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SubBuildOutcome {
    /// The build made one unit of progress and remains pending.
    Progress(SchemaMigrationProgress),
    /// The build reached `Active`; more builds may remain in the migration payload.
    ReachedActive,
    /// The build's cleanup completed and its catalog row was dropped.
    Failed(MigrationFailureCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexMigrationDriveError {
    /// Ambiguous/unavailable transport. The exact pending envelope remains retryable.
    Retryable,
    /// Deterministic owner rejection. Router enters resumable cleanup.
    Terminal(MigrationFailureCode),
}

pub(crate) trait IndexMigrationDriver {
    fn drive(
        &self,
        request: IndexMigrationStepRequest,
    ) -> Pin<
        Box<dyn Future<Output = Result<IndexMigrationStepResponse, IndexMigrationDriveError>> + '_>,
    >;
}

pub(super) async fn apply_index_migration<D: IndexMigrationDriver>(
    store: &RouterStore,
    caller: Principal,
    args: ApplySchemaMigrationArgs,
    driver: &D,
) -> Result<ApplySchemaMigrationResult, RouterError> {
    auth::require_admin(&caller)?;
    let ApplySchemaMigrationArgs::V1(args) = args;
    super::validate_apply_args(&args)?;

    let statements = match gleaph_index_ddl::try_parse(&args.statement) {
        Some(Ok(statements)) => statements,
        Some(Err(error)) => {
            return Err(RouterError::InvalidArgument(format!(
                "invalid migration CREATE INDEX syntax: {error}"
            )));
        }
        None => {
            return Err(RouterError::InvalidArgument(
                "expected one or more CREATE INDEX migration statements".into(),
            ));
        }
    };
    let mut builds = Vec::with_capacity(statements.len());
    for statement in statements {
        match statement {
            gleaph_index_ddl::IndexDdlStatement::Create {
                if_not_exists: true,
                ..
            } => {
                return Err(RouterError::InvalidArgument(
                    "CREATE INDEX migrations forbid IF NOT EXISTS".into(),
                ));
            }
            gleaph_index_ddl::IndexDdlStatement::Create {
                index_name,
                if_not_exists: false,
                target,
            } => builds.push((index_name, target)),
            gleaph_index_ddl::IndexDdlStatement::Drop { .. } => {
                return Err(RouterError::InvalidArgument(
                    "schema migrations do not support DROP INDEX".into(),
                ));
            }
        }
    }
    validate_checksum(&args)?;

    if let Some(existing) = ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| ledger.get(&args.id)) {
        let existing = existing.0;
        let existing_v1 = super::record_v1(&existing)?;
        if !super::record_matches_args(existing_v1, &args)
            || !existing_v1
                .profile
                .iter()
                .all(|profile| *profile == SchemaMigrationStatementProfile::CreateIndex)
        {
            return Err(RouterError::Conflict(format!(
                "schema migration id `{}` already exists with a different payload",
                args.id
            )));
        }
        return resume_existing(existing, driver).await;
    }

    validate_new_chain(&args)?;
    if pending_migration_exists() {
        return Err(RouterError::Conflict(
            "another schema migration is still pending".into(),
        ));
    }

    let graph_id = resolve_graph_selector(store, caller, &args.graph_selector)?;
    let graph_name = graph_catalog::graph_name(graph_id).ok_or_else(|| {
        RouterError::NotFound(format!("canonical graph name for graph {}", graph_id.raw()))
    })?;
    let resolved_graph = ResolvedSchemaMigrationGraph {
        graph_id,
        graph_name,
    };

    // Preflight every statement before the first fallible mutation: names, catalog conflicts,
    // definitions, and inline projections are all validated for the whole payload first.
    let mut definitions = Vec::with_capacity(builds.len());
    for (index_name, target) in &builds {
        crate::facade::store::validate_metadata_name(index_name)?;
        if lookup_index_name_id(graph_id, index_name).is_some() {
            return Err(RouterError::Conflict(format!(
                "index `{index_name}` already exists"
            )));
        }
        let definition = resolve_index_definition(graph_id, target)?;
        resolve_inline_projection(
            graph_id,
            definition.entry.kind,
            definition.label_id,
            definition.property_id,
        )?;
        definitions.push(definition);
    }
    let (targets, topology_epoch) = capture_targets(store, graph_id)?;
    let prepared_at_ns = super::super::ic_time_ns();
    let mut prepare_args_list = Vec::with_capacity(builds.len());
    for ((index_name, _), definition) in builds.iter().zip(&definitions) {
        let prepare_args = PrepareIndexLifecycleArgs {
            graph_selector: args.graph_selector.clone(),
            resolved_graph: resolved_graph.clone(),
            kind: definition.entry.kind,
            property_id: definition.property_id,
            label_id: definition.label_id,
            edge_direction: definition.edge_direction,
            migration_id: args.id.clone(),
            topology_epoch,
            targets: targets.clone(),
            prepared_at_ns,
        };
        indexed_catalog::preflight_prepare_index_scope(graph_id, &prepare_args)?;
        prepare_args_list.push((index_name.clone(), prepare_args));
    }

    // This is one synchronous message execution with no await. The no-write preflight above makes
    // name interning the first fallible mutation. Once it succeeds, every following failure is an
    // invariant violation and traps so IC rolls the complete co-write back.
    let mut pending = Vec::with_capacity(builds.len());
    for (index_name, prepare_args) in prepare_args_list {
        let index_name_id = intern_index_name(graph_id, &index_name)?;
        let prepared = indexed_catalog::prepare_named_index(graph_id, index_name_id, prepare_args)
            .unwrap_or_else(|error| {
                ic_cdk::trap(format!(
                    "schema migration Preparing co-write invariant failed: {error}"
                ))
            });
        pending.push(PendingIndexBuild {
            index_name_id,
            physical_index_id: prepared.physical_index_id,
        });
    }
    let recorded_at = super::super::ic_time_ns();
    let pending_count = pending.len();
    let first_index_name_id = pending[0].index_name_id;
    let record = SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
        id: args.id.clone(),
        parent: args.parent,
        graph_selector: args.graph_selector,
        resolved_graph: Some(resolved_graph),
        checksum: args.checksum,
        actor: caller,
        recorded_at,
        statement: args.statement,
        profile: vec![SchemaMigrationStatementProfile::CreateIndex; pending.len()],
        state: SchemaMigrationRecordState::PendingIndex { pending },
    });
    ROUTER_SCHEMA_MIGRATIONS.with_borrow_mut(|ledger| {
        ledger.insert(args.id, StableSchemaMigrationRecord(record.clone()));
    });
    let first = indexed_catalog::get_named_index(graph_id, first_index_name_id)
        .ok_or_else(|| RouterError::InvalidState("pending index catalog row missing".into()))?;
    Ok(apply_result(
        migration_progress(progress_for(&first), 0, pending_count as u32),
        record,
    ))
}

fn validate_checksum(
    args: &gleaph_migration_api::ApplySchemaMigrationArgsV1,
) -> Result<(), RouterError> {
    if args.checksum.algorithm != SchemaMigrationChecksumAlgorithm::Sha256 {
        return Err(RouterError::InvalidArgument(
            "unsupported schema migration checksum algorithm".into(),
        ));
    }
    let expected = gleaph_migration_api::schema_migration_checksum(
        &args.id,
        args.parent.as_deref(),
        &args.graph_selector,
        args.statement.as_bytes(),
    );
    if args.checksum != expected {
        return Err(RouterError::InvalidArgument(
            "schema migration checksum does not match the exact request envelope".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_new_chain(
    args: &gleaph_migration_api::ApplySchemaMigrationArgsV1,
) -> Result<(), RouterError> {
    let chain = super::inspect_canonical_chain(None, 0)?;
    if chain.count >= crate::facade::stable::schema_migration::MAX_SCHEMA_MIGRATIONS {
        return Err(RouterError::InvalidArgument(
            "schema migration ledger is full".into(),
        ));
    }
    match args.parent.as_deref() {
        None if chain.count != 0 => Err(RouterError::Conflict(
            "a non-empty schema migration ledger already has a root".into(),
        )),
        Some(parent) if chain.head.as_deref() != Some(parent) => Err(RouterError::Conflict(
            format!("schema migration parent `{parent}` is not the current head"),
        )),
        Some(parent) => {
            let parent_sequence = gleaph_migration_api::parse_schema_migration_id(parent)
                .ok_or_else(|| RouterError::Internal("invalid migration head id".into()))?;
            let sequence = gleaph_migration_api::parse_schema_migration_id(&args.id)
                .ok_or_else(|| RouterError::Internal("invalid migration id".into()))?;
            if sequence <= parent_sequence {
                Err(RouterError::Conflict(
                    "migration sequence must be greater than its parent".into(),
                ))
            } else {
                Ok(())
            }
        }
        None => Ok(()),
    }
}

fn pending_migration_exists() -> bool {
    ROUTER_SCHEMA_MIGRATIONS.with_borrow(|ledger| {
        ledger.iter().any(|entry| {
            matches!(
                super::record_v1(&entry.value().0).map(|record| &record.state),
                Ok(SchemaMigrationRecordState::PendingIndex { .. })
            )
        })
    })
}

fn resolve_graph_selector(
    store: &RouterStore,
    caller: Principal,
    selector: &gleaph_migration_api::SchemaMigrationGraphSelector,
) -> Result<GraphId, RouterError> {
    match selector {
        gleaph_migration_api::SchemaMigrationGraphSelector::Default => {
            store.resolve_home_graph_id(caller)
        }
        gleaph_migration_api::SchemaMigrationGraphSelector::Named(name) => {
            store.resolve_graph_id_authorized(name, caller)
        }
    }
}

fn capture_targets(
    store: &RouterStore,
    graph_id: GraphId,
) -> Result<(Vec<IndexLifecycleTarget>, u64), RouterError> {
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    if shards.is_empty() {
        return Err(RouterError::InvalidArgument(
            "CREATE INDEX migration requires at least one live shard".into(),
        ));
    }
    let routes = shards
        .iter()
        .map(|shard| {
            (
                shard.shard_id.raw(),
                shard.graph_canister,
                shard.index_canister,
            )
        })
        .collect::<Vec<_>>();
    let topology_epoch = topology_epoch(&routes);
    let mut grouped = BTreeMap::<Principal, Vec<u32>>::new();
    for shard in shards {
        if shard.index_canister == Principal::anonymous() {
            return Err(RouterError::InvalidArgument(
                "CREATE INDEX migration target has an anonymous graph-index canister".into(),
            ));
        }
        grouped
            .entry(shard.index_canister)
            .or_default()
            .push(shard.shard_id.raw());
    }
    let mut targets = Vec::with_capacity(grouped.len());
    for (index_canister, mut shard_ids) in grouped {
        shard_ids.sort_unstable();
        shard_ids.dedup();
        targets.push(IndexLifecycleTarget {
            index_canister,
            shard_ids,
            state: IndexLifecycleTargetState::Registering,
        });
    }
    targets.sort_by(|left, right| {
        left.index_canister
            .as_slice()
            .cmp(right.index_canister.as_slice())
    });
    Ok((targets, topology_epoch))
}

fn topology_epoch(routes: &[(u32, Principal, Principal)]) -> u64 {
    let mut routes = routes.to_vec();
    routes.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.as_slice().cmp(right.1.as_slice()))
            .then_with(|| left.2.as_slice().cmp(right.2.as_slice()))
    });
    let mut hasher = Sha256::new();
    hasher.update(b"gleaph:index-migration-topology:v1\0");
    for (shard_id, graph_canister, index_canister) in routes {
        hasher.update(shard_id.to_le_bytes());
        hasher.update((graph_canister.as_slice().len() as u64).to_le_bytes());
        hasher.update(graph_canister.as_slice());
        hasher.update((index_canister.as_slice().len() as u64).to_le_bytes());
        hasher.update(index_canister.as_slice());
    }
    u64::from_le_bytes(hasher.finalize()[..8].try_into().expect("sha256 prefix"))
}

fn resolve_inline_projection(
    graph_id: GraphId,
    kind: gleaph_graph_kernel::index::IndexedPropertyKind,
    label_id: u16,
    property_id: gleaph_graph_kernel::entry::PropertyId,
) -> Result<Option<CanonicalInlineProjection>, RouterError> {
    if kind == gleaph_graph_kernel::index::IndexedPropertyKind::Vertex {
        return Ok(None);
    }
    let (source_profile, schema) = crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES
        .with_borrow(|profiles| {
            profiles.get_profile_and_inline_schema(graph_id, EdgeLabelId::from_raw(label_id))
        });
    let Some(schema) = schema else {
        return Ok(None);
    };
    match schema {
        gleaph_graph_kernel::plan_exec::ResolvedInlineSchema::Scalar {
            property_id: source_property_id,
        } => {
            if source_property_id != property_id {
                return Ok(None);
            }
            Ok(Some(CanonicalInlineProjection {
                source_property_id,
                byte_offset: 0,
                source_profile: source_profile.clone(),
                value_profile: source_profile,
            }))
        }
        gleaph_graph_kernel::plan_exec::ResolvedInlineSchema::Struct {
            property_id: source_property_id,
            fields,
        } => {
            if source_property_id == property_id {
                return Err(RouterError::Conflict(
                    "an inline struct must be indexed by one leaf field".into(),
                ));
            }
            let property_name = RouterStore::new().reverse_property_name(graph_id, property_id)?;
            if !property_name.contains('.') {
                return Ok(None);
            }
            let source_property_name =
                RouterStore::new().reverse_property_name(graph_id, source_property_id)?;
            let mut matching = fields.into_iter().filter(|field| {
                field.name == property_name
                    || format!("{source_property_name}.{}", field.name) == property_name
            });
            let field = matching.next().ok_or_else(|| {
                RouterError::Conflict(format!(
                    "inline field projection for property `{property_name}` is unavailable"
                ))
            })?;
            if matching.next().is_some() {
                return Err(RouterError::InvalidState(format!(
                    "inline field projection for property `{property_name}` is ambiguous"
                )));
            }
            Ok(Some(CanonicalInlineProjection {
                source_property_id,
                byte_offset: field.byte_offset,
                source_profile,
                value_profile: field.profile,
            }))
        }
    }
}

async fn resume_existing<D: IndexMigrationDriver>(
    migration_record: SchemaMigrationRecord,
    driver: &D,
) -> Result<ApplySchemaMigrationResult, RouterError> {
    let record = super::record_v1(&migration_record)?.clone();
    match record.state {
        SchemaMigrationRecordState::Applied { .. } => Ok(apply_result(
            SchemaMigrationApplyStatus::Replay,
            migration_record,
        )),
        SchemaMigrationRecordState::Failed { code, .. } => Ok(apply_result(
            SchemaMigrationApplyStatus::Failed(code),
            migration_record,
        )),
        SchemaMigrationRecordState::PendingIndex { .. } => {
            advance_sequence(migration_record, driver).await
        }
    }
}

/// Drive a pending index migration by one bounded step. Sub-builds advance sequentially in payload
/// order: each apply round moves the first non-`Active` build forward; the migration is `Applied`
/// only when every build is `Active`. A terminal failure of one build terminates the migration
/// (`Failed`) and releases every not-yet-`Active` lifecycle row while earlier `Active` builds stay
/// published (ADR 0059 multi-index migrations).
async fn advance_sequence<D: IndexMigrationDriver>(
    migration_record: SchemaMigrationRecord,
    driver: &D,
) -> Result<ApplySchemaMigrationResult, RouterError> {
    let record = super::record_v1(&migration_record)?.clone();
    let SchemaMigrationRecordState::PendingIndex { pending } = &record.state else {
        return Err(RouterError::InvalidState(
            "advance_sequence requires a pending index migration".into(),
        ));
    };
    let Some(resolved) = &record.resolved_graph else {
        return Err(RouterError::InvalidState(
            "pending index migration has no resolved graph".into(),
        ));
    };
    let total = pending.len() as u32;
    for (active_index, build) in pending.iter().enumerate() {
        let index = indexed_catalog::get_named_index(resolved.graph_id, build.index_name_id)
            .ok_or_else(|| RouterError::InvalidState("pending index catalog row missing".into()))?;
        validate_pending_identity(&record, build.physical_index_id, &index)?;
        if matches!(index.lifecycle, IndexLifecycleState::Active { .. }) {
            continue;
        }
        match advance_one_index(build.index_name_id, index, driver).await? {
            SubBuildOutcome::Progress(progress) => {
                return Ok(apply_result(
                    migration_progress(progress, active_index as u32, total),
                    migration_record,
                ));
            }
            SubBuildOutcome::ReachedActive => {
                // This build published; continue to the next pending build or terminate Applied.
            }
            SubBuildOutcome::Failed(code) => {
                return finish_failed_migration(migration_record, code);
            }
        }
    }
    finish_applied_migration(migration_record)
}

fn validate_pending_identity(
    migration: &SchemaMigrationRecordV1,
    physical_index_id: PhysicalIndexId,
    index: &IndexDefRecord,
) -> Result<(), RouterError> {
    let Some(resolved) = &migration.resolved_graph else {
        return Err(RouterError::InvalidState(
            "pending index migration lost its resolved graph".into(),
        ));
    };
    if index.physical_index_id != physical_index_id
        || index.graph_selector != migration.graph_selector
        || &index.resolved_graph != resolved
        || !matches!(
            &index.build,
            IndexBuildMetadata::Migration { migration_id, .. } if migration_id == &migration.id
        )
    {
        return Err(RouterError::Conflict(
            "pending migration/index lifecycle identity mismatch".into(),
        ));
    }
    Ok(())
}

/// Advance one migration-driven index build by one bounded step without touching the migration
/// ledger. `ReachedActive` publishes the catalog row as `Active`; `Failed` reports a fully cleaned
/// failure whose lifecycle row was already dropped.
async fn advance_one_index<D: IndexMigrationDriver>(
    index_name_id: IndexNameId,
    index: IndexDefRecord,
    driver: &D,
) -> Result<SubBuildOutcome, RouterError> {
    if !matches!(index.lifecycle, IndexLifecycleState::Aborting { .. })
        && !topology_matches(&index)?
    {
        let aborting = enter_aborting(index, index_name_id, MigrationFailureCode::TopologyChanged)?;
        return Ok(SubBuildOutcome::Progress(progress_for(&aborting)));
    }

    match &index.lifecycle {
        IndexLifecycleState::Preparing { targets } => {
            if targets
                .iter()
                .all(|target| matches!(target.state, IndexLifecycleTargetState::Registered))
            {
                let building = targets
                    .iter()
                    .cloned()
                    .map(|mut target| {
                        target.state = IndexLifecycleTargetState::Building { seeded_items: 0 };
                        target
                    })
                    .collect();
                let next = indexed_catalog::transition_index_lifecycle(
                    index.resolved_graph.graph_id,
                    index_name_id,
                    IndexLifecycleState::Building { targets: building },
                )?;
                return Ok(SubBuildOutcome::Progress(progress_for(&next)));
            }
            drive_target_index(
                index_name_id,
                index,
                IndexMigrationStepAction::Register,
                driver,
            )
            .await
        }
        IndexLifecycleState::Building { targets } => {
            if targets
                .iter()
                .all(|target| matches!(target.state, IndexLifecycleTargetState::Built { .. }))
            {
                let sealing_targets = targets
                    .iter()
                    .cloned()
                    .map(|mut target| {
                        target.state = IndexLifecycleTargetState::Sealing;
                        target
                    })
                    .collect();
                let next = indexed_catalog::begin_index_sealing(
                    index.resolved_graph.graph_id,
                    index_name_id,
                    sealing_targets,
                    super::super::ic_time_ns(),
                )
                .or_else(|error| match error {
                    RouterError::Conflict(_) => enter_aborting(
                        index,
                        index_name_id,
                        MigrationFailureCode::StaleOrMismatchedResponse,
                    ),
                    other => Err(other),
                })?;
                return Ok(SubBuildOutcome::Progress(progress_for(&next)));
            }
            drive_target_index(
                index_name_id,
                index,
                IndexMigrationStepAction::Build,
                driver,
            )
            .await
        }
        IndexLifecycleState::Sealing {
            targets,
            catalog_epoch,
            ..
        } => {
            if targets
                .iter()
                .all(|target| matches!(target.state, IndexLifecycleTargetState::Converged { .. }))
            {
                // Publication precedes the migration ledger terminal write. If the message traps
                // after this transition, IC rolls both writes back; the sequence driver's Active
                // skip completes recovery on the next apply.
                indexed_catalog::transition_index_lifecycle(
                    index.resolved_graph.graph_id,
                    index_name_id,
                    IndexLifecycleState::Active {
                        catalog_epoch: *catalog_epoch,
                        activated_at_ns: super::super::ic_time_ns(),
                    },
                )?;
                return Ok(SubBuildOutcome::ReachedActive);
            }
            drive_target_index(index_name_id, index, IndexMigrationStepAction::Seal, driver).await
        }
        IndexLifecycleState::Aborting {
            targets, failure, ..
        } => {
            if targets
                .iter()
                .all(|target| matches!(target.state, IndexLifecycleTargetState::Cleaned))
            {
                indexed_catalog::drop_named_index(
                    index.resolved_graph.graph_id,
                    index_name_id,
                    false,
                )?;
                return Ok(SubBuildOutcome::Failed(*failure));
            }
            drive_target_index(
                index_name_id,
                index,
                IndexMigrationStepAction::Cleanup,
                driver,
            )
            .await
        }
        IndexLifecycleState::Active { .. } => Ok(SubBuildOutcome::ReachedActive),
    }
}

fn topology_matches(index: &IndexDefRecord) -> Result<bool, RouterError> {
    let expected = match &index.build {
        IndexBuildMetadata::Migration { topology_epoch, .. } => *topology_epoch,
        IndexBuildMetadata::ImmediateActive => return Ok(true),
    };
    let (_, actual) = capture_targets(&RouterStore::new(), index.resolved_graph.graph_id)?;
    Ok(expected == actual)
}

async fn drive_target_index<D: IndexMigrationDriver>(
    index_name_id: IndexNameId,
    index: IndexDefRecord,
    action: IndexMigrationStepAction,
    driver: &D,
) -> Result<SubBuildOutcome, RouterError> {
    let target_index = target_for_action(&index, action)
        .ok_or_else(|| RouterError::InvalidState(format!("no target is ready for {action:?}")))?;
    let request = step_request(&index, index_name_id, target_index, action)?;
    let response = match driver.drive(request.clone()).await {
        Ok(response) => response,
        Err(IndexMigrationDriveError::Retryable) => {
            return Err(RouterError::Busy {
                operation: "schema_migration.index_driver".into(),
            });
        }
        Err(IndexMigrationDriveError::Terminal(code)) => {
            let aborting = enter_aborting(index, index_name_id, code)?;
            return Ok(SubBuildOutcome::Progress(progress_for(&aborting)));
        }
    };
    if !response_matches_request(&request, &response) {
        return mismatched_outcome(index_name_id, index);
    }

    let mut targets = lifecycle_targets(&index.lifecycle).to_vec();
    let target = &mut targets[target_index];
    let response_state = match response.result {
        IndexMigrationStepResult::Registered(status) => {
            if action != IndexMigrationStepAction::Register
                || status.registration != request.registration
                || status.phase != gleaph_graph_kernel::index::IndexBuildPhase::Building
            {
                return mismatched_outcome(index_name_id, index);
            }
            IndexLifecycleTargetState::Registered
        }
        IndexMigrationStepResult::BuildProgress(status) => {
            if action != IndexMigrationStepAction::Build
                || status.registration != request.registration
                || status.phase != gleaph_graph_kernel::index::IndexBuildPhase::Building
            {
                return mismatched_outcome(index_name_id, index);
            }
            if status.progress.done {
                IndexLifecycleTargetState::Built {
                    seeded_items: status.progress.seeded_items,
                }
            } else {
                IndexLifecycleTargetState::Building {
                    seeded_items: status.progress.seeded_items,
                }
            }
        }
        IndexMigrationStepResult::SealProgress {
            watermarks,
            converged,
        } => {
            if action != IndexMigrationStepAction::Seal
                || !valid_watermark_ids(&target.shard_ids, &watermarks)
                || (converged && !watermarks_converged(&watermarks))
            {
                return mismatched_outcome(index_name_id, index);
            }
            if converged {
                IndexLifecycleTargetState::Converged { watermarks }
            } else {
                IndexLifecycleTargetState::Sealing
            }
        }
        IndexMigrationStepResult::CleanupProgress { done } => {
            if action != IndexMigrationStepAction::Cleanup {
                return mismatched_outcome(index_name_id, index);
            }
            if done {
                IndexLifecycleTargetState::Cleaned
            } else {
                IndexLifecycleTargetState::Cleaning
            }
        }
    };
    target.state = response_state;
    let next_lifecycle = with_targets(&index.lifecycle, targets);
    let next = indexed_catalog::transition_index_lifecycle(
        index.resolved_graph.graph_id,
        index_name_id,
        next_lifecycle,
    )?;
    Ok(SubBuildOutcome::Progress(progress_for(&next)))
}

fn mismatched_outcome(
    index_name_id: IndexNameId,
    index: IndexDefRecord,
) -> Result<SubBuildOutcome, RouterError> {
    let aborting = enter_aborting(
        index,
        index_name_id,
        MigrationFailureCode::StaleOrMismatchedResponse,
    )?;
    Ok(SubBuildOutcome::Progress(progress_for(&aborting)))
}

fn target_for_action(index: &IndexDefRecord, action: IndexMigrationStepAction) -> Option<usize> {
    lifecycle_targets(&index.lifecycle)
        .iter()
        .position(|target| match action {
            IndexMigrationStepAction::Register => {
                matches!(target.state, IndexLifecycleTargetState::Registering)
            }
            IndexMigrationStepAction::Build => {
                matches!(target.state, IndexLifecycleTargetState::Building { .. })
            }
            IndexMigrationStepAction::Seal => {
                matches!(target.state, IndexLifecycleTargetState::Sealing)
            }
            IndexMigrationStepAction::Cleanup => {
                matches!(target.state, IndexLifecycleTargetState::Cleaning)
            }
        })
}

fn step_request(
    index: &IndexDefRecord,
    index_name_id: IndexNameId,
    target_index: usize,
    action: IndexMigrationStepAction,
) -> Result<IndexMigrationStepRequest, RouterError> {
    let target = &lifecycle_targets(&index.lifecycle)[target_index];
    let (migration_id, topology_epoch, prepared_catalog_epoch) = match &index.build {
        IndexBuildMetadata::Migration {
            migration_id,
            topology_epoch,
            prepared_catalog_epoch,
            ..
        } => (
            migration_id.clone(),
            *topology_epoch,
            *prepared_catalog_epoch,
        ),
        IndexBuildMetadata::ImmediateActive => {
            return Err(RouterError::InvalidState(
                "immediate-active index has no migration driver envelope".into(),
            ));
        }
    };
    let lifecycle_catalog_epoch = match index.lifecycle {
        IndexLifecycleState::Sealing { catalog_epoch, .. }
        | IndexLifecycleState::Aborting { catalog_epoch, .. }
        | IndexLifecycleState::Active { catalog_epoch, .. } => catalog_epoch,
        IndexLifecycleState::Preparing { .. } | IndexLifecycleState::Building { .. } => {
            prepared_catalog_epoch
        }
    };
    let build_target = match index.kind {
        gleaph_graph_kernel::index::IndexedPropertyKind::Vertex => {
            let record_source =
                vertex_record_source(index.resolved_graph.graph_id, index.property_id)?;
            IndexBuildTarget::Vertex {
                label_id: index.label_id,
                property_id: index.property_id,
                record_source,
            }
        }
        gleaph_graph_kernel::index::IndexedPropertyKind::Edge => IndexBuildTarget::Edge {
            label_id: index.label_id,
            property_id: index.property_id,
            direction: index.edge_direction.ok_or_else(|| {
                RouterError::InvalidState("edge index missing canonical direction".into())
            })?,
        },
    };
    let export_target = canonical_export_target(&build_target)?;
    let inline = resolve_inline_projection(
        index.resolved_graph.graph_id,
        index.kind,
        index.label_id,
        index.property_id,
    )?;
    let scope = CanonicalExportScope {
        graph_id: index.resolved_graph.graph_id,
        index_name_id,
        catalog_epoch: lifecycle_catalog_epoch,
        target: export_target,
        inline,
    };
    let live_shards =
        RouterStore::new().list_live_shards_for_graph_id(index.resolved_graph.graph_id)?;
    let mut export_scopes = Vec::with_capacity(target.shard_ids.len());
    for shard_id in &target.shard_ids {
        let shard = live_shards
            .iter()
            .find(|shard| {
                shard.shard_id.raw() == *shard_id && shard.index_canister == target.index_canister
            })
            .ok_or_else(|| {
                RouterError::Conflict(format!(
                    "captured shard {shard_id} no longer resolves to the expected graph-index target"
                ))
            })?;
        export_scopes.push(IndexMigrationExportScope {
            shard_id: *shard_id,
            graph_canister: shard.graph_canister,
            scope: scope.clone(),
        });
    }
    Ok(IndexMigrationStepRequest {
        migration_id,
        topology_epoch,
        lifecycle_catalog_epoch,
        index_canister: target.index_canister,
        registration: RegisterIndexBuildRequest {
            physical_index_id: index.physical_index_id,
            graph_id: index.resolved_graph.graph_id,
            index_name_id,
            catalog_epoch: prepared_catalog_epoch,
            topology_epoch,
            target: build_target,
            target_shard_ids: target.shard_ids.clone(),
        },
        export_scopes,
        action,
    })
}

fn canonical_export_target(
    build_target: &IndexBuildTarget,
) -> Result<CanonicalExportTarget, RouterError> {
    match build_target {
        IndexBuildTarget::Vertex {
            label_id,
            property_id,
            record_source,
        } => Ok(CanonicalExportTarget::Vertex {
            label_id: *label_id,
            property_id: *property_id,
            record_source: record_source.clone(),
        }),
        IndexBuildTarget::Edge {
            label_id,
            property_id,
            direction,
        } => Ok(CanonicalExportTarget::Edge {
            label_id: EdgeLabelId::from_raw(*label_id),
            property_id: *property_id,
            direction: *direction,
        }),
    }
}

/// Projects the nested-record walk for one vertex leaf definition at the DDL/write admission
/// boundary. The ancestor id and tail path derive from the Router's own property-name catalog,
/// mirroring the read-only catalog projection. Unlike that infallible projection, this boundary
/// must reject an inconsistent row before catalog publication or build registration instead of
/// omitting it.
fn vertex_record_source(
    graph_id: GraphId,
    leaf_property_id: PropertyId,
) -> Result<Option<CanonicalRecordSource>, RouterError> {
    let Some((field_path, ancestor_property_id)) =
        indexed_catalog::vertex_leaf_projection(graph_id, leaf_property_id)
    else {
        return Err(RouterError::InvalidState(format!(
            "vertex leaf projection for property {} is inconsistent; refusing to publish or register it",
            leaf_property_id.raw()
        )));
    };
    if field_path.is_empty() {
        return Ok(None);
    }
    let Some((_, tail)) = field_path.split_once('.') else {
        return Err(RouterError::InvalidState(format!(
            "vertex leaf projection for property {} has no record tail",
            leaf_property_id.raw()
        )));
    };
    Ok(Some(CanonicalRecordSource {
        ancestor_property_id: PropertyId::from_raw(ancestor_property_id),
        field_tail: tail.to_owned(),
    }))
}

fn response_matches_request(
    request: &IndexMigrationStepRequest,
    response: &IndexMigrationStepResponse,
) -> bool {
    response.migration_id == request.migration_id
        && response.topology_epoch == request.topology_epoch
        && response.lifecycle_catalog_epoch == request.lifecycle_catalog_epoch
        && response.index_canister == request.index_canister
        && response.registration == request.registration
        && response.export_scopes == request.export_scopes
}

fn valid_watermark_ids(shard_ids: &[u32], watermarks: &[IndexShardWatermark]) -> bool {
    shard_ids.len() == watermarks.len()
        && shard_ids
            .iter()
            .zip(watermarks)
            .all(|(shard, watermark)| *shard == watermark.shard_id)
}

fn watermarks_converged(watermarks: &[IndexShardWatermark]) -> bool {
    watermarks
        .iter()
        .all(|watermark| watermark.drained_through >= watermark.admitted_through)
}

fn enter_aborting(
    index: IndexDefRecord,
    index_name_id: IndexNameId,
    failure: MigrationFailureCode,
) -> Result<IndexDefRecord, RouterError> {
    if matches!(index.lifecycle, IndexLifecycleState::Aborting { .. }) {
        return Ok(index);
    }
    let targets = lifecycle_targets(&index.lifecycle)
        .iter()
        .cloned()
        .map(|mut target| {
            target.state = IndexLifecycleTargetState::Cleaning;
            target
        })
        .collect();
    indexed_catalog::transition_index_lifecycle(
        index.resolved_graph.graph_id,
        index_name_id,
        IndexLifecycleState::Aborting {
            catalog_epoch: indexed_catalog::current_index_catalog_epoch(),
            failure,
            targets,
            started_at_ns: super::super::ic_time_ns(),
        },
    )
}

fn lifecycle_targets(state: &IndexLifecycleState) -> &[IndexLifecycleTarget] {
    match state {
        IndexLifecycleState::Preparing { targets }
        | IndexLifecycleState::Building { targets }
        | IndexLifecycleState::Sealing { targets, .. }
        | IndexLifecycleState::Aborting { targets, .. } => targets,
        IndexLifecycleState::Active { .. } => &[],
    }
}

fn progress_for(index: &IndexDefRecord) -> SchemaMigrationProgress {
    let (phase, completed_targets, total_targets) = match &index.lifecycle {
        IndexLifecycleState::Preparing { targets } => (
            SchemaMigrationProgressPhase::Preparing,
            targets
                .iter()
                .filter(|target| matches!(target.state, IndexLifecycleTargetState::Registered))
                .count(),
            targets.len(),
        ),
        IndexLifecycleState::Building { targets } => (
            SchemaMigrationProgressPhase::Building,
            targets
                .iter()
                .filter(|target| matches!(target.state, IndexLifecycleTargetState::Built { .. }))
                .count(),
            targets.len(),
        ),
        IndexLifecycleState::Sealing { targets, .. } => (
            SchemaMigrationProgressPhase::Sealing,
            targets
                .iter()
                .filter(|target| {
                    matches!(target.state, IndexLifecycleTargetState::Converged { .. })
                })
                .count(),
            targets.len(),
        ),
        IndexLifecycleState::Aborting { targets, .. } => (
            SchemaMigrationProgressPhase::Aborting,
            targets
                .iter()
                .filter(|target| matches!(target.state, IndexLifecycleTargetState::Cleaned))
                .count(),
            targets.len(),
        ),
        IndexLifecycleState::Active { .. } => {
            unreachable!("Active indexes are skipped by the sequence driver")
        }
    };
    SchemaMigrationProgress {
        phase,
        completed_targets: completed_targets as u32,
        total_targets: total_targets as u32,
        active_index: 0,
        total_indexes: 0,
    }
}

/// Wrap one sub-build's progress with its migration-level ordinal so the CLI can reset its bounded
/// round budget when the advancing build changes.
fn migration_progress(
    sub: SchemaMigrationProgress,
    active_index: u32,
    total_indexes: u32,
) -> SchemaMigrationApplyStatus {
    let mut sub = sub;
    sub.active_index = active_index;
    sub.total_indexes = total_indexes;
    SchemaMigrationApplyStatus::Progress(sub)
}

/// Mark the migration ledger `Applied`. All pending sub-builds are already `Active`.
fn finish_applied_migration(
    migration_record: SchemaMigrationRecord,
) -> Result<ApplySchemaMigrationResult, RouterError> {
    let SchemaMigrationRecord::V1(mut record) = migration_record;
    record.state = SchemaMigrationRecordState::Applied {
        applied_at: super::super::ic_time_ns(),
    };
    let id = record.id.clone();
    let terminal = SchemaMigrationRecord::V1(record);
    ROUTER_SCHEMA_MIGRATIONS.with_borrow_mut(|ledger| {
        ledger.insert(id, StableSchemaMigrationRecord(terminal.clone()));
    });
    Ok(apply_result(SchemaMigrationApplyStatus::Applied, terminal))
}

/// Mark the migration ledger `Failed` and release every not-yet-`Active` sub-build lifecycle row so
/// no orphaned catalog state remains. Earlier `Active` builds stay published (ADR 0059 multi-index
/// migrations), and index names stay interned as in the single-index failure contract.
fn finish_failed_migration(
    migration_record: SchemaMigrationRecord,
    code: MigrationFailureCode,
) -> Result<ApplySchemaMigrationResult, RouterError> {
    let record = super::record_v1(&migration_record)?.clone();
    let SchemaMigrationRecordState::PendingIndex { pending } = &record.state else {
        return Err(RouterError::InvalidState(
            "finish_failed_migration requires a pending index migration".into(),
        ));
    };
    let Some(resolved) = &record.resolved_graph else {
        return Err(RouterError::InvalidState(
            "pending index migration has no resolved graph".into(),
        ));
    };
    for build in pending {
        if let Some(index) =
            indexed_catalog::get_named_index(resolved.graph_id, build.index_name_id)
            && !matches!(index.lifecycle, IndexLifecycleState::Active { .. })
        {
            indexed_catalog::drop_named_index(resolved.graph_id, build.index_name_id, false)?;
        }
    }
    let SchemaMigrationRecord::V1(mut record) = migration_record;
    record.state = SchemaMigrationRecordState::Failed {
        failed_at: super::super::ic_time_ns(),
        code,
    };
    let id = record.id.clone();
    let terminal = SchemaMigrationRecord::V1(record);
    ROUTER_SCHEMA_MIGRATIONS.with_borrow_mut(|ledger| {
        ledger.insert(id, StableSchemaMigrationRecord(terminal.clone()));
    });
    Ok(apply_result(
        SchemaMigrationApplyStatus::Failed(code),
        terminal,
    ))
}

fn with_targets(
    state: &IndexLifecycleState,
    targets: Vec<IndexLifecycleTarget>,
) -> IndexLifecycleState {
    match state {
        IndexLifecycleState::Preparing { .. } => IndexLifecycleState::Preparing { targets },
        IndexLifecycleState::Building { .. } => IndexLifecycleState::Building { targets },
        IndexLifecycleState::Sealing {
            catalog_epoch,
            started_at_ns,
            ..
        } => IndexLifecycleState::Sealing {
            catalog_epoch: *catalog_epoch,
            targets,
            started_at_ns: *started_at_ns,
        },
        IndexLifecycleState::Aborting {
            catalog_epoch,
            failure,
            started_at_ns,
            ..
        } => IndexLifecycleState::Aborting {
            catalog_epoch: *catalog_epoch,
            failure: *failure,
            targets,
            started_at_ns: *started_at_ns,
        },
        IndexLifecycleState::Active { .. } => unreachable!("Active has no targets to update"),
    }
}

fn apply_result(
    status: SchemaMigrationApplyStatus,
    record: SchemaMigrationRecord,
) -> ApplySchemaMigrationResult {
    ApplySchemaMigrationResult::V1(ApplySchemaMigrationResultV1 { status, record })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::facade::stable::edge_inline_property_profiles::{
        InlineScalarType, InlineStructFieldSpec,
    };
    use crate::facade::stable::index_name_catalog::lookup_index_name_id;
    use crate::facade::store::catalog_test_support::{GRAPH, setup_with_shard};
    use crate::types::AdminRegisterShardArgs;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{IndexBuildPhase, IndexBuildProgress};

    #[derive(Clone, Copy)]
    enum FakeBehavior {
        Happy,
        Retryable,
        Mismatch,
    }

    struct FakeDriver {
        behavior: RefCell<FakeBehavior>,
        requests: RefCell<Vec<IndexMigrationStepRequest>>,
    }

    impl FakeDriver {
        fn new(behavior: FakeBehavior) -> Self {
            Self {
                behavior: RefCell::new(behavior),
                requests: RefCell::new(Vec::new()),
            }
        }

        fn set_behavior(&self, behavior: FakeBehavior) {
            *self.behavior.borrow_mut() = behavior;
        }
    }

    impl IndexMigrationDriver for FakeDriver {
        fn drive(
            &self,
            request: IndexMigrationStepRequest,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<IndexMigrationStepResponse, IndexMigrationDriveError>>
                    + '_,
            >,
        > {
            self.requests.borrow_mut().push(request.clone());
            let behavior = *self.behavior.borrow();
            Box::pin(async move {
                if matches!(behavior, FakeBehavior::Retryable) {
                    return Err(IndexMigrationDriveError::Retryable);
                }
                let result = match request.action {
                    IndexMigrationStepAction::Register => {
                        IndexMigrationStepResult::Registered(fake_status(&request, false, 0))
                    }
                    IndexMigrationStepAction::Build => {
                        IndexMigrationStepResult::BuildProgress(fake_status(&request, true, 17))
                    }
                    IndexMigrationStepAction::Seal => IndexMigrationStepResult::SealProgress {
                        watermarks: request
                            .registration
                            .target_shard_ids
                            .iter()
                            .map(|shard_id| IndexShardWatermark {
                                shard_id: *shard_id,
                                admitted_through: 23,
                                drained_through: 23,
                            })
                            .collect(),
                        converged: true,
                    },
                    IndexMigrationStepAction::Cleanup => {
                        IndexMigrationStepResult::CleanupProgress { done: true }
                    }
                };
                let mut response = IndexMigrationStepResponse {
                    migration_id: request.migration_id.clone(),
                    topology_epoch: request.topology_epoch,
                    lifecycle_catalog_epoch: request.lifecycle_catalog_epoch,
                    index_canister: request.index_canister,
                    registration: request.registration.clone(),
                    export_scopes: request.export_scopes.clone(),
                    result,
                };
                if matches!(behavior, FakeBehavior::Mismatch) {
                    response.migration_id.push_str("_stale");
                }
                Ok(response)
            })
        }
    }

    fn fake_status(
        request: &IndexMigrationStepRequest,
        done: bool,
        seeded_items: u64,
    ) -> IndexBuildStatus {
        IndexBuildStatus {
            registration: request.registration.clone(),
            progress: IndexBuildProgress {
                next_page_sequence: u64::from(done),
                next_shard_index: if done {
                    request.registration.target_shard_ids.len() as u32
                } else {
                    0
                },
                expected_shard_id: (!done)
                    .then(|| request.registration.target_shard_ids.first().copied())
                    .flatten(),
                cursor: None,
                seeded_items,
                done,
            },
            phase: IndexBuildPhase::Building,
            watermarks: Vec::new(),
        }
    }

    fn index_args(id: &str, parent: Option<&str>, index_name: &str) -> ApplySchemaMigrationArgs {
        let statement = format!("CREATE INDEX {index_name} FOR (n:Person) ON (n.age)");
        let graph_selector =
            gleaph_migration_api::SchemaMigrationGraphSelector::Named(GRAPH.into());
        ApplySchemaMigrationArgs::V1(gleaph_migration_api::ApplySchemaMigrationArgsV1 {
            id: id.into(),
            parent: parent.map(str::to_owned),
            graph_selector: graph_selector.clone(),
            checksum: gleaph_migration_api::schema_migration_checksum(
                id,
                parent,
                &graph_selector,
                statement.as_bytes(),
            ),
            statement,
        })
    }

    fn result_status(result: &ApplySchemaMigrationResult) -> &SchemaMigrationApplyStatus {
        let ApplySchemaMigrationResult::V1(result) = result;
        &result.status
    }

    fn result_record(result: &ApplySchemaMigrationResult) -> &SchemaMigrationRecordV1 {
        let ApplySchemaMigrationResult::V1(result) = result;
        let SchemaMigrationRecord::V1(record) = &result.record;
        record
    }

    fn register_second_shard(store: &RouterStore, admin: Principal, index_canister: Principal) {
        futures::executor::block_on(store.admin_register_shard(
            admin,
            AdminRegisterShardArgs {
                shard_id: ShardId::new(1),
                graph_canister: Principal::from_slice(&[3]),
                index_canister,
                logical_graph_name: GRAPH.into(),
            },
        ))
        .expect("register second shard");
    }

    fn setup_index_fixture(two_shards: bool) -> (RouterStore, Principal, GraphId) {
        let (store, admin, graph_id) = setup_with_shard(ShardId::new(0));
        indexed_catalog::purge_graph_indexes(graph_id);
        RouterStore::commit_intern_vertex_label_name(graph_id, "Person").expect("vertex label");
        RouterStore::commit_intern_property_name(graph_id, "age").expect("property");
        if two_shards {
            register_second_shard(&store, admin, Principal::from_slice(&[2]));
        }
        (store, admin, graph_id)
    }

    #[test]
    fn topology_epoch_is_order_stable_and_identity_sensitive() {
        let principal_a = Principal::from_slice(&[1; 29]);
        let principal_b = Principal::from_slice(&[2; 29]);
        let routes = vec![(2, principal_b, principal_a), (0, principal_a, principal_b)];
        let reordered = vec![(0, principal_a, principal_b), (2, principal_b, principal_a)];
        assert_eq!(topology_epoch(&routes), topology_epoch(&reordered));
        let graph_changed = vec![(0, principal_b, principal_b), (2, principal_b, principal_a)];
        assert_ne!(topology_epoch(&routes), topology_epoch(&graph_changed));
        let index_changed = vec![(0, principal_a, principal_a), (2, principal_b, principal_a)];
        assert_ne!(topology_epoch(&routes), topology_epoch(&index_changed));
    }

    #[test]
    fn watermark_validation_requires_exact_shards_and_drain_convergence() {
        let valid = vec![
            IndexShardWatermark {
                shard_id: 1,
                admitted_through: 4,
                drained_through: 4,
            },
            IndexShardWatermark {
                shard_id: 3,
                admitted_through: 7,
                drained_through: 8,
            },
        ];
        assert!(valid_watermark_ids(&[1, 3], &valid));
        assert!(watermarks_converged(&valid));
        let mut stale = valid.clone();
        stale[1].drained_through = 6;
        assert!(valid_watermark_ids(&[1, 3], &stale));
        assert!(!watermarks_converged(&stale));
        assert!(!valid_watermark_ids(&[1, 2], &valid));
    }

    #[test]
    fn inline_struct_leaf_projection_is_exact_and_sidecars_remain_none() {
        let (store, _admin, graph_id) = setup_index_fixture(false);
        store
            .commit_set_edge_label_inline_struct_schema(
                graph_id,
                "AFFINITY",
                "stats",
                vec![
                    InlineStructFieldSpec {
                        name: "score".into(),
                        scalar_type: InlineScalarType::F32,
                    },
                    InlineStructFieldSpec {
                        name: "rank".into(),
                        scalar_type: InlineScalarType::U16,
                    },
                ],
            )
            .expect("inline struct schema");
        let label_id = store
            .lookup_edge_label_id(graph_id, "AFFINITY")
            .expect("edge label");
        let score_id = store
            .lookup_property_id(graph_id, "stats.score")
            .expect("leaf property");

        let projection = resolve_inline_projection(
            graph_id,
            gleaph_graph_kernel::index::IndexedPropertyKind::Edge,
            label_id.raw(),
            score_id,
        )
        .expect("projection")
        .expect("inline leaf");
        assert_eq!(
            projection.source_property_id,
            store.lookup_property_id(graph_id, "stats").expect("source")
        );
        assert_eq!(projection.byte_offset, 0);
        assert_eq!(projection.value_profile.byte_width, 4);

        let sidecar_id =
            RouterStore::commit_intern_property_name(graph_id, "note").expect("sidecar property");
        assert_eq!(
            resolve_inline_projection(
                graph_id,
                gleaph_graph_kernel::index::IndexedPropertyKind::Edge,
                label_id.raw(),
                sidecar_id,
            )
            .expect("sidecar projection"),
            None
        );
    }

    #[test]
    fn vertex_record_source_projects_exact_nested_walk_and_flat_none() {
        let (_store, _admin, graph_id) = setup_with_shard(ShardId::new(0));
        indexed_catalog::purge_graph_indexes(graph_id);
        let ancestor =
            RouterStore::commit_intern_property_name(graph_id, "stats").expect("intern ancestor");
        let leaf = RouterStore::commit_intern_property_name(graph_id, "stats.score")
            .expect("intern dotted leaf");
        assert_eq!(
            vertex_record_source(graph_id, leaf),
            Ok(Some(CanonicalRecordSource {
                ancestor_property_id: ancestor,
                field_tail: "score".to_owned(),
            })),
            "a valid nested leaf propagates its exact ancestor id and tail"
        );
        let flat =
            RouterStore::commit_intern_property_name(graph_id, "age").expect("intern flat leaf");
        assert_eq!(
            vertex_record_source(graph_id, flat),
            Ok(None),
            "a valid flat leaf carries no record source"
        );
    }

    #[test]
    fn vertex_record_source_rejects_inconsistent_rows_without_mutation() {
        let (store, _admin, graph_id) = setup_with_shard(ShardId::new(0));
        indexed_catalog::purge_graph_indexes(graph_id);
        let stats =
            RouterStore::commit_intern_property_name(graph_id, "stats").expect("intern ancestor");
        let dotted = RouterStore::commit_intern_property_name(graph_id, "stats.score")
            .expect("intern dotted leaf");

        let assert_catalog_unchanged = |label: &str| {
            assert_eq!(store.lookup_property_id(graph_id, "stats"), Ok(stats));
            assert_eq!(
                store.lookup_property_id(graph_id, "stats.score"),
                Ok(dotted)
            );
            assert!(
                store.lookup_property_id(graph_id, "orphan").is_err(),
                "{label}: no late interning may repair an inconsistent row"
            );
            assert!(
                indexed_catalog::load_indexed_property_catalog(graph_id)
                    .vertex_indexes
                    .is_empty(),
                "{label}: no membership row may be published for inconsistent rows"
            );
        };
        assert_catalog_unchanged("baseline");

        // Missing reverse name.
        let uninterned = PropertyId::from_raw(9_999_002);
        assert!(matches!(
            vertex_record_source(graph_id, uninterned),
            Err(RouterError::InvalidState(_))
        ));
        // Malformed dotted path (empty tail component).
        let malformed = RouterStore::commit_intern_property_name(graph_id, "stats.")
            .expect("catalog corruption is expressible");
        assert!(matches!(
            vertex_record_source(graph_id, malformed),
            Err(RouterError::InvalidState(_))
        ));
        // Dotted leaf whose top-level property never resolved.
        let orphan_leaf = RouterStore::commit_intern_property_name(graph_id, "orphan.score")
            .expect("intern orphan leaf");
        assert!(matches!(
            vertex_record_source(graph_id, orphan_leaf),
            Err(RouterError::InvalidState(_))
        ));
        assert_catalog_unchanged("after rejections");
    }

    #[test]
    fn vertex_export_target_binds_the_resolved_label_before_effects() {
        let property_id = gleaph_graph_kernel::entry::PropertyId::from_raw(9);
        let exact = canonical_export_target(&IndexBuildTarget::Vertex {
            label_id: 3,
            property_id,
            record_source: None,
        })
        .expect("flat vertex target");
        assert_eq!(
            exact,
            CanonicalExportTarget::Vertex {
                label_id: 3,
                property_id,
                record_source: None,
            }
        );
        assert_ne!(
            exact,
            CanonicalExportTarget::Vertex {
                label_id: 4,
                property_id,
                record_source: None,
            }
        );
    }

    #[test]
    fn fake_driver_completes_two_shard_fresh_resume_and_exact_replay() {
        let (store, admin, graph_id) = setup_index_fixture(true);
        let args = index_args("000001_age_index", None, "person_age");
        let driver = FakeDriver::new(FakeBehavior::Happy);

        let fresh = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("fresh prepare");
        assert_eq!(
            result_status(&fresh),
            &SchemaMigrationApplyStatus::Progress(SchemaMigrationProgress {
                phase: SchemaMigrationProgressPhase::Preparing,
                completed_targets: 0,
                total_targets: 1,
                active_index: 0,
                total_indexes: 1,
            })
        );
        assert!(driver.requests.borrow().is_empty());

        let blocked = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            index_args("000002_other_index", Some("000001_age_index"), "other_age"),
            &driver,
        ));
        assert!(
            matches!(blocked, Err(RouterError::Conflict(message)) if message.contains("pending"))
        );

        let expected = [
            (SchemaMigrationProgressPhase::Preparing, 1, 1),
            (SchemaMigrationProgressPhase::Building, 0, 1),
            (SchemaMigrationProgressPhase::Building, 1, 1),
            (SchemaMigrationProgressPhase::Sealing, 0, 1),
            (SchemaMigrationProgressPhase::Sealing, 1, 1),
        ];
        for (phase, completed_targets, total_targets) in expected {
            let progress = futures::executor::block_on(apply_index_migration(
                &store,
                admin,
                args.clone(),
                &driver,
            ))
            .expect("bounded lifecycle step");
            assert_eq!(
                result_status(&progress),
                &SchemaMigrationApplyStatus::Progress(SchemaMigrationProgress {
                    phase,
                    completed_targets,
                    total_targets,
                    active_index: 0,
                    total_indexes: 1,
                })
            );
        }
        let applied = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("publish active and apply ledger");
        assert_eq!(
            result_status(&applied),
            &SchemaMigrationApplyStatus::Applied
        );
        assert!(matches!(
            result_record(&applied).state,
            SchemaMigrationRecordState::Applied { .. }
        ));

        let index_name_id = lookup_index_name_id(graph_id, "person_age").expect("index name id");
        let active = indexed_catalog::get_named_index(graph_id, index_name_id).expect("catalog");
        assert!(matches!(
            active.lifecycle,
            IndexLifecycleState::Active { .. }
        ));
        let seal_request = driver
            .requests
            .borrow()
            .iter()
            .find(|request| request.action == IndexMigrationStepAction::Seal)
            .cloned()
            .expect("seal request");
        assert_eq!(seal_request.registration.target_shard_ids, vec![0, 1]);

        let replay =
            futures::executor::block_on(apply_index_migration(&store, admin, args, &driver))
                .expect("exact replay");
        assert_eq!(result_status(&replay), &SchemaMigrationApplyStatus::Replay);
    }

    #[test]
    fn multi_statement_migration_drives_subbuilds_sequentially_to_applied() {
        let (store, admin, graph_id) = setup_index_fixture(true);
        RouterStore::commit_intern_property_name(graph_id, "demo_id").expect("second property");
        RouterStore::commit_intern_vertex_label_name(graph_id, "Post").expect("vertex label");
        let statement = "CREATE INDEX person_age FOR (n:Person) ON (n.age)\nNEXT CREATE INDEX post_demo_id FOR (n:Post) ON (n.demo_id)";
        let graph_selector =
            gleaph_migration_api::SchemaMigrationGraphSelector::Named(GRAPH.into());
        let args = ApplySchemaMigrationArgs::V1(gleaph_migration_api::ApplySchemaMigrationArgsV1 {
            id: "000001_two_indexes".into(),
            parent: None,
            graph_selector: graph_selector.clone(),
            checksum: gleaph_migration_api::schema_migration_checksum(
                "000001_two_indexes",
                None,
                &graph_selector,
                statement.as_bytes(),
            ),
            statement: statement.into(),
        });
        let driver = FakeDriver::new(FakeBehavior::Happy);

        let fresh = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("fresh prepare");
        assert_eq!(
            result_status(&fresh),
            &SchemaMigrationApplyStatus::Progress(SchemaMigrationProgress {
                phase: SchemaMigrationProgressPhase::Preparing,
                completed_targets: 0,
                total_targets: 1,
                active_index: 0,
                total_indexes: 2,
            })
        );

        // Drive until Applied; the advancing sub-build ordinal must move from 0 to 1.
        let mut seen_active_indexes = std::collections::BTreeSet::new();
        let mut status = result_status(&fresh).clone();
        let mut applied = fresh;
        let mut rounds = 0;
        while !matches!(status, SchemaMigrationApplyStatus::Applied) {
            rounds += 1;
            assert!(
                rounds < 30,
                "multi-index drive did not terminate; last status {status:?}"
            );
            if let SchemaMigrationApplyStatus::Progress(progress) = &status {
                seen_active_indexes.insert(progress.active_index);
            }
            applied = futures::executor::block_on(apply_index_migration(
                &store,
                admin,
                args.clone(),
                &driver,
            ))
            .expect("bounded lifecycle step");
            status = result_status(&applied).clone();
        }
        assert_eq!(
            seen_active_indexes,
            std::collections::BTreeSet::from([0, 1])
        );
        assert!(matches!(
            result_record(&applied).state,
            SchemaMigrationRecordState::Applied { .. }
        ));

        // Both catalog lifecycle rows are Active.
        let age_name_id = lookup_index_name_id(graph_id, "person_age").expect("age name");
        let demo_name_id = lookup_index_name_id(graph_id, "post_demo_id").expect("demo name");
        assert!(matches!(
            indexed_catalog::get_named_index(graph_id, age_name_id)
                .expect("age row")
                .lifecycle,
            IndexLifecycleState::Active { .. }
        ));
        assert!(matches!(
            indexed_catalog::get_named_index(graph_id, demo_name_id)
                .expect("demo row")
                .lifecycle,
            IndexLifecycleState::Active { .. }
        ));
    }

    #[test]
    fn retryable_driver_preserves_pending_catalog_and_ledger() {
        let (store, admin, graph_id) = setup_index_fixture(false);
        let args = index_args("000001_retry_index", None, "retry_age");
        let driver = FakeDriver::new(FakeBehavior::Retryable);
        let fresh = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("fresh prepare");
        let before_record = result_record(&fresh).clone();
        let index_name_id = lookup_index_name_id(graph_id, "retry_age").expect("index name id");
        let before_index = indexed_catalog::get_named_index(graph_id, index_name_id).expect("row");

        let unavailable =
            futures::executor::block_on(apply_index_migration(&store, admin, args, &driver));
        assert!(matches!(unavailable, Err(RouterError::Busy { .. })));
        assert_eq!(
            indexed_catalog::get_named_index(graph_id, index_name_id),
            Some(before_index)
        );
        let ledger = ROUTER_SCHEMA_MIGRATIONS
            .with_borrow(|ledger| ledger.get(&before_record.id))
            .expect("ledger record");
        assert_eq!(ledger.0, SchemaMigrationRecord::V1(before_record));
    }

    #[test]
    fn mismatched_response_aborts_cleans_and_fails_before_releasing_gate() {
        let (store, admin, graph_id) = setup_index_fixture(false);
        let args = index_args("000001_mismatch_index", None, "mismatch_age");
        let driver = FakeDriver::new(FakeBehavior::Mismatch);
        futures::executor::block_on(apply_index_migration(&store, admin, args.clone(), &driver))
            .expect("fresh prepare");

        let aborting = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("mismatch enters aborting");
        assert!(matches!(
            result_status(&aborting),
            SchemaMigrationApplyStatus::Progress(SchemaMigrationProgress {
                phase: SchemaMigrationProgressPhase::Aborting,
                completed_targets: 0,
                total_targets: 1,
                active_index: 0,
                total_indexes: 1,
            })
        ));
        assert!(matches!(
            result_record(&aborting).state,
            SchemaMigrationRecordState::PendingIndex { .. }
        ));

        driver.set_behavior(FakeBehavior::Happy);
        let cleaned = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("cleanup step");
        assert!(matches!(
            result_status(&cleaned),
            SchemaMigrationApplyStatus::Progress(SchemaMigrationProgress {
                phase: SchemaMigrationProgressPhase::Aborting,
                completed_targets: 1,
                total_targets: 1,
                active_index: 0,
                total_indexes: 1,
            })
        ));
        let failed =
            futures::executor::block_on(apply_index_migration(&store, admin, args, &driver))
                .expect("cleanup completion");
        assert_eq!(
            result_status(&failed),
            &SchemaMigrationApplyStatus::Failed(MigrationFailureCode::StaleOrMismatchedResponse)
        );
        assert!(lookup_index_name_id(graph_id, "mismatch_age").is_some());
        let index_name_id = lookup_index_name_id(graph_id, "mismatch_age").expect("name retained");
        assert!(indexed_catalog::get_named_index(graph_id, index_name_id).is_none());
    }

    #[test]
    fn topology_change_enters_aborting_without_calling_driver() {
        let (store, admin, graph_id) = setup_index_fixture(false);
        let args = index_args("000001_topology_index", None, "topology_age");
        let driver = FakeDriver::new(FakeBehavior::Happy);
        futures::executor::block_on(apply_index_migration(&store, admin, args.clone(), &driver))
            .expect("fresh prepare");
        register_second_shard(&store, admin, Principal::from_slice(&[5; 29]));

        let aborting =
            futures::executor::block_on(apply_index_migration(&store, admin, args, &driver))
                .expect("topology change enters aborting");
        assert!(matches!(
            result_status(&aborting),
            SchemaMigrationApplyStatus::Progress(SchemaMigrationProgress {
                phase: SchemaMigrationProgressPhase::Aborting,
                ..
            })
        ));
        assert!(driver.requests.borrow().is_empty());
        let index_name_id = lookup_index_name_id(graph_id, "topology_age").expect("index id");
        let row = indexed_catalog::get_named_index(graph_id, index_name_id).expect("catalog row");
        assert!(matches!(
            row.lifecycle,
            IndexLifecycleState::Aborting {
                failure: MigrationFailureCode::TopologyChanged,
                ..
            }
        ));
    }
}
