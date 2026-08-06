//! Real inter-canister driver for one bounded ADR 0059 migration step (Router → graph-index and
//! Router → Graph).
//!
//! The driver is pure orchestration. Every downstream envelope is derived exactly from the
//! immutable [`IndexMigrationStepRequest`]; failures are classified by one shared retryable vs
//! terminal rule; and the response echoes the request identity so the state machine's
//! `response_matches_request` fails closed on stale or cross-target replies. Router never relays
//! Graph canonical-export pages: graph-index pulls pages directly, and Router only drives
//! graph-index build control plus the Graph export-scope lifecycle and bounded outbox drain.

use std::future::Future;
use std::pin::Pin;

use candid::Principal;
use gleaph_graph_kernel::canonical_export::{
    CanonicalExportError, CanonicalExportScope, CanonicalExportStatus,
    IndexBuildOutboxDrainProgress, IndexBuildOutboxDrainRequest,
};
use gleaph_graph_kernel::index::{
    IndexBuildCleanupStatus, IndexBuildControlRequest, IndexBuildError, IndexBuildSealRequest,
    IndexBuildSealStatus, IndexBuildSealTarget, IndexBuildStatus, PhysicalIndexId,
    RegisterIndexBuildRequest,
};
use gleaph_migration_api::MigrationFailureCode;

use super::index::{
    IndexMigrationDriveError, IndexMigrationDriver, IndexMigrationExportScope,
    IndexMigrationStepAction, IndexMigrationStepRequest, IndexMigrationStepResponse,
    IndexMigrationStepResult,
};
use crate::facade::stable::indexed_catalog::IndexShardWatermark;

/// Build-DML outbox entries drained by one Graph call. Each entry is one graph-index DML call, so
/// the per-call bound keeps a single Graph drain message within its instruction budget.
pub(crate) const MAX_OUTBOX_DRAIN_ENTRIES_PER_CALL: u32 = 32;

/// Total outbox drain calls allowed in one drive so a single Router message stays bounded and the
/// next drive resumes the same exact envelope.
pub(crate) const MAX_OUTBOX_DRAIN_CALLS_PER_DRIVE: u32 = 16;

/// Transport/decode taxonomy shared by every typed downstream call before retryability
/// classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TypedCallFailure {
    /// `.await` transport error: reject, timeout, or unknown. The exact envelope stays retryable.
    Transport,
    /// Candid decode failure on a completed reply that is not the expected `Result<T, E>` shape.
    #[allow(
        dead_code,
        reason = "constructed by the wasm call_typed decode-failure path"
    )]
    Decode,
}

/// One graph-index build-control call outcome with its typed owner rejection preserved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexBuildCallError {
    Transport,
    Decode,
    Typed(IndexBuildError),
}

/// One Graph export-scope call outcome with its typed owner rejection preserved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GraphScopeCallError {
    Transport,
    Decode,
    Typed(CanonicalExportError),
}

impl From<TypedCallFailure> for IndexBuildCallError {
    fn from(failure: TypedCallFailure) -> Self {
        match failure {
            TypedCallFailure::Transport => IndexBuildCallError::Transport,
            TypedCallFailure::Decode => IndexBuildCallError::Decode,
        }
    }
}

impl From<TypedCallFailure> for GraphScopeCallError {
    fn from(failure: TypedCallFailure) -> Self {
        match failure {
            TypedCallFailure::Transport => GraphScopeCallError::Transport,
            TypedCallFailure::Decode => GraphScopeCallError::Decode,
        }
    }
}

/// Router → graph-index build control surface used by the migration driver.
pub(crate) trait IndexBuildClient {
    fn register_index_build(
        &self,
        canister: Principal,
        request: RegisterIndexBuildRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexBuildStatus, IndexBuildCallError>> + '_>>;

    fn advance_index_build(
        &self,
        canister: Principal,
        request: IndexBuildControlRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexBuildStatus, IndexBuildCallError>> + '_>>;

    fn seal_index_build(
        &self,
        canister: Principal,
        request: IndexBuildSealRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexBuildSealStatus, IndexBuildCallError>> + '_>>;

    fn abort_index_build(
        &self,
        canister: Principal,
        request: IndexBuildControlRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexBuildCleanupStatus, IndexBuildCallError>> + '_>>;

    fn index_build_status(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
    ) -> Pin<Box<dyn Future<Output = Result<IndexBuildStatus, IndexBuildCallError>> + '_>>;
}

/// Router → Graph export-scope lifecycle and outbox-drain surface used by the migration driver.
pub(crate) trait GraphScopeClient {
    fn register_scope(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
        scope: CanonicalExportScope,
    ) -> Pin<Box<dyn Future<Output = Result<(), GraphScopeCallError>> + '_>>;

    fn seal_scope(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
        expected_scope: CanonicalExportScope,
        new_epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<CanonicalExportStatus, GraphScopeCallError>> + '_>>;

    fn activate_scope(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
        proof: IndexBuildSealStatus,
    ) -> Pin<Box<dyn Future<Output = Result<CanonicalExportStatus, GraphScopeCallError>> + '_>>;

    fn abort_scope(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
        expected_scope: CanonicalExportScope,
    ) -> Pin<Box<dyn Future<Output = Result<CanonicalExportStatus, GraphScopeCallError>> + '_>>;

    fn remove_scope(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
        expected_scope: CanonicalExportScope,
    ) -> Pin<Box<dyn Future<Output = Result<(), GraphScopeCallError>> + '_>>;

    fn drain_outbox(
        &self,
        canister: Principal,
        request: IndexBuildOutboxDrainRequest,
    ) -> Pin<
        Box<dyn Future<Output = Result<IndexBuildOutboxDrainProgress, GraphScopeCallError>> + '_>,
    >;
}

/// Production graph-index build client. Canister-only: native builds fail closed with the
/// transport classification so the Router can never observe a fake success.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IcIndexBuildClient;

/// Production Graph export-scope client. Canister-only: native builds fail closed with the
/// transport classification so the Router can never observe a fake success.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IcGraphScopeClient;

/// Real driver composition: one graph-index client and one Graph export-scope client.
pub(crate) struct RealIndexMigrationDriver<IB, GS> {
    index_build: IB,
    graph_scope: GS,
}

impl<IB: IndexBuildClient, GS: GraphScopeClient> RealIndexMigrationDriver<IB, GS> {
    pub(crate) fn new(index_build: IB, graph_scope: GS) -> Self {
        Self {
            index_build,
            graph_scope,
        }
    }

    /// Preparing: register the graph-index build, then freeze every Graph export scope.
    async fn drive_register(
        &self,
        request: IndexMigrationStepRequest,
    ) -> Result<IndexMigrationStepResponse, IndexMigrationDriveError> {
        let status = self
            .index_build
            .register_index_build(request.index_canister, request.registration.clone())
            .await
            .map_err(classify_index_build_call)?;
        for scope in &request.export_scopes {
            self.graph_scope
                .register_scope(
                    scope.graph_canister,
                    request.registration.physical_index_id,
                    scope.scope.clone(),
                )
                .await
                .map_err(classify_graph_scope_call)?;
        }
        Ok(response_for(
            request,
            IndexMigrationStepResult::Registered(status),
        ))
    }

    /// Building: one bounded graph-index advance. Graph-index pulls Graph pages itself.
    async fn drive_build(
        &self,
        request: IndexMigrationStepRequest,
    ) -> Result<IndexMigrationStepResponse, IndexMigrationDriveError> {
        let status = self
            .index_build
            .advance_index_build(
                request.index_canister,
                IndexBuildControlRequest {
                    registration: request.registration.clone(),
                },
            )
            .await
            .map_err(classify_index_build_call)?;
        Ok(response_for(
            request,
            IndexMigrationStepResult::BuildProgress(status),
        ))
    }

    /// Sealing: freeze scopes in shard order, seal graph-index, drain outboxes, poll convergence,
    /// then publish every scope with the graph-index seal proof.
    async fn drive_seal(
        &self,
        request: IndexMigrationStepRequest,
    ) -> Result<IndexMigrationStepResponse, IndexMigrationDriveError> {
        let physical_index_id = request.registration.physical_index_id;
        // 1. Freeze every export scope at the fresh lifecycle epoch, capturing the old-epoch
        //    admission watermark per shard. Export scopes are already in shard order.
        let mut shard_targets = Vec::with_capacity(request.export_scopes.len());
        for scope in &request.export_scopes {
            let status = self
                .graph_scope
                .seal_scope(
                    scope.graph_canister,
                    physical_index_id,
                    frozen_scope(scope, &request),
                    request.lifecycle_catalog_epoch,
                )
                .await
                .map_err(classify_graph_scope_call)?;
            shard_targets.push(IndexBuildSealTarget {
                shard_id: scope.shard_id,
                admitted_through: status.admitted_through,
            });
        }
        shard_targets.sort_by_key(|target| target.shard_id);
        // 2. graph-index seal with the captured admission watermarks.
        let seal = self
            .index_build
            .seal_index_build(
                request.index_canister,
                IndexBuildSealRequest {
                    control: IndexBuildControlRequest {
                        registration: request.registration.clone(),
                    },
                    seal_catalog_epoch: request.lifecycle_catalog_epoch,
                    shard_targets,
                },
            )
            .await
            .map_err(classify_index_build_call)?;
        // 3. Drive the Graph outbox drains (bounded; transport stops the drive retryably). A
        //    missing scope mid-seal is a genuine inconsistency and stays terminal.
        self.drain_outboxes(&request, false).await?;
        // 4. Poll graph-index for base completion and per-shard drain convergence.
        let status = self
            .index_build
            .index_build_status(request.index_canister, physical_index_id)
            .await
            .map_err(classify_index_build_call)?;
        let converged = status.progress.done
            && status
                .watermarks
                .iter()
                .all(|watermark| watermark.drained_through >= watermark.admitted_through);
        if converged {
            // 5. Publish every Graph scope with the graph-index seal proof. A proof captured
            //    before this drive's drain finished is retryable: the next drive's idempotent
            //    `seal_index_build` replay returns the refreshed proof.
            for scope in &request.export_scopes {
                self.graph_scope
                    .activate_scope(scope.graph_canister, physical_index_id, seal.clone())
                    .await
                    .map_err(classify_graph_scope_call)?;
            }
        }
        let watermarks = status
            .watermarks
            .iter()
            .map(|watermark| IndexShardWatermark {
                shard_id: watermark.shard_id,
                admitted_through: watermark.admitted_through,
                drained_through: watermark.drained_through,
            })
            .collect();
        Ok(response_for(
            request,
            IndexMigrationStepResult::SealProgress {
                watermarks,
                converged,
            },
        ))
    }

    /// Cleanup: drain the outbox to convergence FIRST (so graph-index still accepts DML), then
    /// abort graph-index, abort every scope, and only then remove scopes whose graph-index
    /// cleanup finished. Scopes that are already gone (removed by a prior drive or never
    /// registered) count as cleaned so a crash between any two steps resumes idempotently.
    async fn drive_cleanup(
        &self,
        request: IndexMigrationStepRequest,
    ) -> Result<IndexMigrationStepResponse, IndexMigrationDriveError> {
        let physical_index_id = request.registration.physical_index_id;
        // 1. Drain must converge before graph-index abort; a bounded step defers the rest.
        let drained = self.drain_outboxes(&request, true).await?;
        if !drained {
            return Ok(response_for(
                request,
                IndexMigrationStepResult::CleanupProgress { done: false },
            ));
        }
        // 2. graph-index bounded cleanup (idempotent, resumable).
        let cleanup = self
            .index_build
            .abort_index_build(
                request.index_canister,
                IndexBuildControlRequest {
                    registration: request.registration.clone(),
                },
            )
            .await
            .map_err(classify_index_build_call)?;
        // 3. Abort every Graph scope under the frozen registration identity. A deterministic
        //    abort rejection is terminal; a missing scope is already cleaned.
        for scope in &request.export_scopes {
            let result = self
                .graph_scope
                .abort_scope(
                    scope.graph_canister,
                    physical_index_id,
                    frozen_scope(scope, &request),
                )
                .await;
            match result {
                Ok(_) => {}
                Err(GraphScopeCallError::Typed(CanonicalExportError::ScopeNotFound)) => {}
                Err(error) => return Err(classify_graph_scope_call(error)),
            }
        }
        // 4. Removal is safe only after graph-index cleanup finished (every scope is already
        //    aborted and drained by this point). Already-removed scopes count as removed.
        if cleanup.done {
            for scope in &request.export_scopes {
                let result = self
                    .graph_scope
                    .remove_scope(
                        scope.graph_canister,
                        physical_index_id,
                        frozen_scope(scope, &request),
                    )
                    .await;
                match result {
                    Ok(()) => {}
                    Err(GraphScopeCallError::Typed(CanonicalExportError::ScopeNotFound)) => {}
                    Err(error) => return Err(classify_graph_scope_call(error)),
                }
            }
        }
        Ok(response_for(
            request,
            IndexMigrationStepResult::CleanupProgress { done: cleanup.done },
        ))
    }

    /// Drains every distinct Graph canister's build-DML outbox until it converges or the bounded
    /// per-drive call cap is hit. Returns `Ok(true)` only when every outbox converged.
    ///
    /// In the seal path a missing scope is terminal (the scope must exist to seal). In the
    /// cleanup path a missing scope means it was already removed or never registered, and counts
    /// as converged so recovery resumes idempotently.
    async fn drain_outboxes(
        &self,
        request: &IndexMigrationStepRequest,
        tolerate_missing_scope: bool,
    ) -> Result<bool, IndexMigrationDriveError> {
        let physical_index_id = request.registration.physical_index_id;
        let mut calls_remaining = MAX_OUTBOX_DRAIN_CALLS_PER_DRIVE;
        let mut graph_canisters = Vec::with_capacity(request.export_scopes.len());
        for scope in &request.export_scopes {
            if !graph_canisters.contains(&scope.graph_canister) {
                graph_canisters.push(scope.graph_canister);
            }
        }
        for canister in graph_canisters {
            loop {
                if calls_remaining == 0 {
                    return Ok(false);
                }
                calls_remaining -= 1;
                let progress = self
                    .graph_scope
                    .drain_outbox(
                        canister,
                        IndexBuildOutboxDrainRequest {
                            physical_index_id,
                            max_entries: MAX_OUTBOX_DRAIN_ENTRIES_PER_CALL,
                        },
                    )
                    .await;
                let progress = match progress {
                    Ok(progress) => progress,
                    Err(GraphScopeCallError::Typed(CanonicalExportError::ScopeNotFound))
                        if tolerate_missing_scope =>
                    {
                        break;
                    }
                    Err(error) => return Err(classify_graph_scope_call(error)),
                };
                if progress.converged {
                    break;
                }
            }
        }
        Ok(true)
    }
}

impl<IB: IndexBuildClient, GS: GraphScopeClient> IndexMigrationDriver
    for RealIndexMigrationDriver<IB, GS>
{
    fn drive(
        &self,
        request: IndexMigrationStepRequest,
    ) -> Pin<
        Box<dyn Future<Output = Result<IndexMigrationStepResponse, IndexMigrationDriveError>> + '_>,
    > {
        Box::pin(async move {
            match request.action {
                IndexMigrationStepAction::Register => self.drive_register(request).await,
                IndexMigrationStepAction::Build => self.drive_build(request).await,
                IndexMigrationStepAction::Seal => self.drive_seal(request).await,
                IndexMigrationStepAction::Cleanup => self.drive_cleanup(request).await,
            }
        })
    }
}

/// Production driver instance wired into the control plane.
pub(crate) fn real_index_migration_driver()
-> RealIndexMigrationDriver<IcIndexBuildClient, IcGraphScopeClient> {
    RealIndexMigrationDriver::new(IcIndexBuildClient, IcGraphScopeClient)
}

/// The Graph export scope freezes `catalog_epoch` at registration. Sealing, aborting, and removing
/// under the fresh lifecycle epoch must present the frozen registration scope identity, so the
/// driver rewrites `catalog_epoch` to the registration epoch. All other scope fields are already
/// the immutable registration identity.
fn frozen_scope(
    scope: &IndexMigrationExportScope,
    request: &IndexMigrationStepRequest,
) -> CanonicalExportScope {
    let mut frozen = scope.scope.clone();
    frozen.catalog_epoch = request.registration.catalog_epoch;
    frozen
}

/// Response echoes the immutable request identity so stale and cross-target replies fail closed.
fn response_for(
    request: IndexMigrationStepRequest,
    result: IndexMigrationStepResult,
) -> IndexMigrationStepResponse {
    IndexMigrationStepResponse {
        migration_id: request.migration_id,
        topology_epoch: request.topology_epoch,
        lifecycle_catalog_epoch: request.lifecycle_catalog_epoch,
        index_canister: request.index_canister,
        registration: request.registration,
        export_scopes: request.export_scopes,
        result,
    }
}

/// Single retryability classifier for graph-index build-control errors.
fn classify_index_build_call(error: IndexBuildCallError) -> IndexMigrationDriveError {
    match error {
        IndexBuildCallError::Transport => IndexMigrationDriveError::Retryable,
        IndexBuildCallError::Decode => {
            IndexMigrationDriveError::Terminal(MigrationFailureCode::StaleOrMismatchedResponse)
        }
        IndexBuildCallError::Typed(owner) if owner.is_retryable() => {
            IndexMigrationDriveError::Retryable
        }
        IndexBuildCallError::Typed(_) => {
            IndexMigrationDriveError::Terminal(MigrationFailureCode::TargetRejected)
        }
    }
}

/// Single retryability classifier for Graph export-scope errors.
fn classify_graph_scope_call(error: GraphScopeCallError) -> IndexMigrationDriveError {
    match error {
        GraphScopeCallError::Transport => IndexMigrationDriveError::Retryable,
        GraphScopeCallError::Decode => {
            IndexMigrationDriveError::Terminal(MigrationFailureCode::StaleOrMismatchedResponse)
        }
        GraphScopeCallError::Typed(owner) if graph_scope_error_is_retryable(owner) => {
            IndexMigrationDriveError::Retryable
        }
        GraphScopeCallError::Typed(_) => {
            IndexMigrationDriveError::Terminal(MigrationFailureCode::TargetRejected)
        }
    }
}

fn graph_scope_error_is_retryable(error: CanonicalExportError) -> bool {
    matches!(
        error,
        CanonicalExportError::RetryableSealing
            | CanonicalExportError::SequenceGap
            | CanonicalExportError::NotConverged
            | CanonicalExportError::UnsafeRemoval
            | CanonicalExportError::Storage
    )
}

#[cfg(target_family = "wasm")]
async fn call_typed<T, R, E>(
    canister: Principal,
    method: &str,
    args: T,
) -> Result<Result<R, E>, TypedCallFailure>
where
    T: candid::utils::ArgumentEncoder,
    R: candid::CandidType + serde::de::DeserializeOwned,
    E: candid::CandidType + serde::de::DeserializeOwned,
{
    use ic_cdk::call::Call;

    let reply: Result<R, E> = Call::bounded_wait(canister, method)
        .with_args(&args)
        .await
        .map_err(|_| TypedCallFailure::Transport)?
        .candid()
        .map_err(|_| TypedCallFailure::Decode)?;
    Ok(reply)
}

#[cfg(not(target_family = "wasm"))]
async fn call_typed<T, R, E>(
    _canister: Principal,
    method: &str,
    _args: T,
) -> Result<Result<R, E>, TypedCallFailure> {
    let _ = method;
    Err(TypedCallFailure::Transport)
}

impl IndexBuildClient for IcIndexBuildClient {
    fn register_index_build(
        &self,
        canister: Principal,
        request: RegisterIndexBuildRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexBuildStatus, IndexBuildCallError>> + '_>> {
        Box::pin(async move {
            call_typed::<_, IndexBuildStatus, IndexBuildError>(
                canister,
                "register_index_build",
                (request,),
            )
            .await?
            .map_err(IndexBuildCallError::Typed)
        })
    }

    fn advance_index_build(
        &self,
        canister: Principal,
        request: IndexBuildControlRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexBuildStatus, IndexBuildCallError>> + '_>> {
        Box::pin(async move {
            call_typed::<_, IndexBuildStatus, IndexBuildError>(
                canister,
                "advance_index_build",
                (request,),
            )
            .await?
            .map_err(IndexBuildCallError::Typed)
        })
    }

    fn seal_index_build(
        &self,
        canister: Principal,
        request: IndexBuildSealRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexBuildSealStatus, IndexBuildCallError>> + '_>> {
        Box::pin(async move {
            call_typed::<_, IndexBuildSealStatus, IndexBuildError>(
                canister,
                "seal_index_build",
                (request,),
            )
            .await?
            .map_err(IndexBuildCallError::Typed)
        })
    }

    fn abort_index_build(
        &self,
        canister: Principal,
        request: IndexBuildControlRequest,
    ) -> Pin<Box<dyn Future<Output = Result<IndexBuildCleanupStatus, IndexBuildCallError>> + '_>>
    {
        Box::pin(async move {
            call_typed::<_, IndexBuildCleanupStatus, IndexBuildError>(
                canister,
                "abort_index_build",
                (request,),
            )
            .await?
            .map_err(IndexBuildCallError::Typed)
        })
    }

    fn index_build_status(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
    ) -> Pin<Box<dyn Future<Output = Result<IndexBuildStatus, IndexBuildCallError>> + '_>> {
        Box::pin(async move {
            call_typed::<_, IndexBuildStatus, IndexBuildError>(
                canister,
                "index_build_status",
                (physical_index_id,),
            )
            .await?
            .map_err(IndexBuildCallError::Typed)
        })
    }
}

impl GraphScopeClient for IcGraphScopeClient {
    fn register_scope(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
        scope: CanonicalExportScope,
    ) -> Pin<Box<dyn Future<Output = Result<(), GraphScopeCallError>> + '_>> {
        Box::pin(async move {
            call_typed::<_, (), CanonicalExportError>(
                canister,
                "admin_register_index_export_scope",
                (physical_index_id, scope),
            )
            .await?
            .map_err(GraphScopeCallError::Typed)
        })
    }

    fn seal_scope(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
        expected_scope: CanonicalExportScope,
        new_epoch: u64,
    ) -> Pin<Box<dyn Future<Output = Result<CanonicalExportStatus, GraphScopeCallError>> + '_>>
    {
        Box::pin(async move {
            call_typed::<_, CanonicalExportStatus, CanonicalExportError>(
                canister,
                "admin_seal_index_export_scope",
                (physical_index_id, expected_scope, new_epoch),
            )
            .await?
            .map_err(GraphScopeCallError::Typed)
        })
    }

    fn activate_scope(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
        proof: IndexBuildSealStatus,
    ) -> Pin<Box<dyn Future<Output = Result<CanonicalExportStatus, GraphScopeCallError>> + '_>>
    {
        Box::pin(async move {
            call_typed::<_, CanonicalExportStatus, CanonicalExportError>(
                canister,
                "admin_activate_index_export_scope",
                (physical_index_id, proof),
            )
            .await?
            .map_err(GraphScopeCallError::Typed)
        })
    }

    fn abort_scope(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
        expected_scope: CanonicalExportScope,
    ) -> Pin<Box<dyn Future<Output = Result<CanonicalExportStatus, GraphScopeCallError>> + '_>>
    {
        Box::pin(async move {
            call_typed::<_, CanonicalExportStatus, CanonicalExportError>(
                canister,
                "admin_abort_index_export_scope",
                (physical_index_id, expected_scope),
            )
            .await?
            .map_err(GraphScopeCallError::Typed)
        })
    }

    fn remove_scope(
        &self,
        canister: Principal,
        physical_index_id: PhysicalIndexId,
        expected_scope: CanonicalExportScope,
    ) -> Pin<Box<dyn Future<Output = Result<(), GraphScopeCallError>> + '_>> {
        Box::pin(async move {
            call_typed::<_, (), CanonicalExportError>(
                canister,
                "admin_remove_index_export_scope",
                (physical_index_id, expected_scope),
            )
            .await?
            .map_err(GraphScopeCallError::Typed)
        })
    }

    fn drain_outbox(
        &self,
        canister: Principal,
        request: IndexBuildOutboxDrainRequest,
    ) -> Pin<
        Box<dyn Future<Output = Result<IndexBuildOutboxDrainProgress, GraphScopeCallError>> + '_>,
    > {
        Box::pin(async move {
            call_typed::<_, IndexBuildOutboxDrainProgress, CanonicalExportError>(
                canister,
                "admin_drain_index_build_outbox",
                (request,),
            )
            .await?
            .map_err(GraphScopeCallError::Typed)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use super::super::index::apply_index_migration;
    use super::*;
    use crate::facade::stable::index_name_catalog::lookup_index_name_id;
    use crate::facade::stable::indexed_catalog::{self, IndexLifecycleState};
    use crate::facade::store::RouterStore;
    use crate::facade::store::catalog_test_support::{GRAPH, setup_with_shard};
    use crate::types::AdminRegisterShardArgs;
    use gleaph_graph_kernel::canonical_export::{CanonicalExportPhase, CanonicalExportTarget};
    use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{
        IndexBuildPhase, IndexBuildProgress, IndexBuildStoreError, IndexBuildTarget,
    };
    use gleaph_migration_api::{
        ApplySchemaMigrationArgs, ApplySchemaMigrationResult, SchemaMigrationApplyStatus,
        SchemaMigrationProgress, SchemaMigrationProgressPhase, SchemaMigrationRecord,
        SchemaMigrationRecordState, SchemaMigrationRecordV1,
    };

    /// Cross-client ordering ledger: every fake call appends its event so tests can assert the
    /// exact inter-canister call sequence the driver produces (for example that the Graph outbox
    /// drain precedes the graph-index abort).
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum TimelineEvent {
        IndexRegister,
        IndexAdvance,
        IndexSeal,
        IndexAbort,
        IndexStatus,
        GraphRegister(u32),
        GraphSeal(u32),
        GraphDrain(u32),
        GraphActivate(u32),
        GraphAbort(u32),
        GraphRemove(u32),
    }

    type SharedTimeline = Rc<RefCell<Vec<TimelineEvent>>>;
    /// Per-shard graph-index acknowledged watermark fed by the Graph outbox drain.
    type SharedLedger = Rc<RefCell<BTreeMap<u32, u64>>>;

    fn graph_0() -> Principal {
        Principal::from_slice(&[1])
    }

    fn graph_1() -> Principal {
        Principal::from_slice(&[3])
    }

    fn index_canister() -> Principal {
        Principal::from_slice(&[2])
    }

    fn new_harness() -> (
        FakeIndexBuildClient,
        FakeGraphScopeClient,
        SharedTimeline,
        SharedLedger,
    ) {
        let timeline = Rc::new(RefCell::new(Vec::new()));
        let ledger = Rc::new(RefCell::new(BTreeMap::new()));
        let index_fake = FakeIndexBuildClient::new(timeline.clone(), ledger.clone());
        let graph_fake = FakeGraphScopeClient::new(timeline.clone(), ledger.clone());
        (index_fake, graph_fake, timeline, ledger)
    }

    fn drive<D: IndexMigrationDriver>(
        driver: &D,
        request: IndexMigrationStepRequest,
    ) -> Result<IndexMigrationStepResponse, IndexMigrationDriveError> {
        futures::executor::block_on(driver.drive(request))
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum IndexCallKind {
        Register,
        Advance,
        Seal,
        Abort,
        Status,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum IndexCall {
        Register {
            canister: Principal,
            request: RegisterIndexBuildRequest,
        },
        Advance {
            canister: Principal,
            control: IndexBuildControlRequest,
        },
        Seal {
            canister: Principal,
            request: IndexBuildSealRequest,
        },
        Abort {
            canister: Principal,
            control: IndexBuildControlRequest,
        },
        Status {
            canister: Principal,
            physical_index_id: PhysicalIndexId,
        },
    }

    /// Stateful graph-index fake: registration, one-advance build completion, idempotent seal
    /// whose proof reflects the acknowledged ledger, and one-step abort (configurable to be
    /// resumable). The acknowledged watermark is fed by the Graph outbox drain. `Clone` shares
    /// the underlying state so the driver and the test observe the same calls.
    #[derive(Clone)]
    struct FakeIndexBuildClient {
        calls: Rc<RefCell<Vec<IndexCall>>>,
        timeline: SharedTimeline,
        registered: Rc<RefCell<Option<RegisterIndexBuildRequest>>>,
        phase: Rc<Cell<IndexBuildPhase>>,
        done: Rc<Cell<bool>>,
        seal_targets: Rc<RefCell<Vec<IndexBuildSealTarget>>>,
        seal_epoch: Rc<Cell<u64>>,
        drained_by_shard: SharedLedger,
        fail: Rc<RefCell<Option<(IndexCallKind, IndexBuildCallError)>>>,
        abort_steps: Rc<Cell<u32>>,
    }

    impl FakeIndexBuildClient {
        fn new(timeline: SharedTimeline, drained_by_shard: SharedLedger) -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                timeline,
                registered: Rc::new(RefCell::new(None)),
                phase: Rc::new(Cell::new(IndexBuildPhase::Building)),
                done: Rc::new(Cell::new(false)),
                seal_targets: Rc::new(RefCell::new(Vec::new())),
                seal_epoch: Rc::new(Cell::new(0)),
                drained_by_shard,
                fail: Rc::new(RefCell::new(None)),
                abort_steps: Rc::new(Cell::new(1)),
            }
        }

        fn seed_build(&self, registration: RegisterIndexBuildRequest, done: bool) {
            *self.registered.borrow_mut() = Some(registration);
            self.done.set(done);
        }

        /// Seeds a build that a prior drive already sealed at `seal_epoch` with `targets`, for
        /// re-drive-after-crash-window scenarios where the trapped drive must resume idempotently.
        fn seed_sealed(
            &self,
            registration: RegisterIndexBuildRequest,
            done: bool,
            seal_epoch: u64,
            targets: Vec<IndexBuildSealTarget>,
        ) {
            *self.registered.borrow_mut() = Some(registration);
            self.done.set(done);
            self.phase.set(IndexBuildPhase::Sealing {
                seal_catalog_epoch: seal_epoch,
            });
            self.seal_epoch.set(seal_epoch);
            *self.seal_targets.borrow_mut() = targets;
        }

        fn fail_next(&self, kind: IndexCallKind, error: IndexBuildCallError) {
            *self.fail.borrow_mut() = Some((kind, error));
        }

        fn take_failure(&self, kind: IndexCallKind) -> Option<IndexBuildCallError> {
            let mut fail = self.fail.borrow_mut();
            match *fail {
                Some((failed_kind, error)) if failed_kind == kind => {
                    *fail = None;
                    Some(error)
                }
                _ => None,
            }
        }

        fn registered(&self) -> RegisterIndexBuildRequest {
            self.registered
                .borrow()
                .clone()
                .expect("fake build registered")
        }

        fn watermarks(&self) -> Vec<gleaph_graph_kernel::index::IndexBuildShardWatermark> {
            self.seal_targets
                .borrow()
                .iter()
                .map(
                    |target| gleaph_graph_kernel::index::IndexBuildShardWatermark {
                        shard_id: target.shard_id,
                        admitted_through: target.admitted_through,
                        drained_through: self
                            .drained_by_shard
                            .borrow()
                            .get(&target.shard_id)
                            .copied()
                            .unwrap_or(0),
                    },
                )
                .collect()
        }

        fn status(&self, _physical_index_id: PhysicalIndexId) -> IndexBuildStatus {
            IndexBuildStatus {
                registration: self.registered(),
                progress: IndexBuildProgress {
                    next_page_sequence: u64::from(self.done.get()),
                    next_shard_index: 0,
                    expected_shard_id: None,
                    cursor: None,
                    seeded_items: 17,
                    done: self.done.get(),
                },
                phase: self.phase.get(),
                watermarks: self.watermarks(),
            }
        }

        fn seal_status(&self) -> IndexBuildSealStatus {
            IndexBuildSealStatus {
                base_complete: self.done.get(),
                seal_catalog_epoch: self.seal_epoch.get(),
                watermarks: self.watermarks(),
            }
        }

        fn register(
            &self,
            canister: Principal,
            request: RegisterIndexBuildRequest,
        ) -> Result<IndexBuildStatus, IndexBuildCallError> {
            self.calls.borrow_mut().push(IndexCall::Register {
                canister,
                request: request.clone(),
            });
            self.timeline
                .borrow_mut()
                .push(TimelineEvent::IndexRegister);
            if let Some(error) = self.take_failure(IndexCallKind::Register) {
                return Err(error);
            }
            if let Some(existing) = self.registered.borrow().as_ref() {
                if existing == &request {
                    return Ok(self.status(request.physical_index_id));
                }
                return Err(IndexBuildCallError::Typed(IndexBuildError::Store(
                    IndexBuildStoreError::AlreadyRegistered,
                )));
            }
            *self.registered.borrow_mut() = Some(request.clone());
            self.phase.set(IndexBuildPhase::Building);
            self.done.set(false);
            Ok(self.status(request.physical_index_id))
        }

        fn advance(
            &self,
            canister: Principal,
            control: IndexBuildControlRequest,
        ) -> Result<IndexBuildStatus, IndexBuildCallError> {
            self.calls.borrow_mut().push(IndexCall::Advance {
                canister,
                control: control.clone(),
            });
            self.timeline.borrow_mut().push(TimelineEvent::IndexAdvance);
            if let Some(error) = self.take_failure(IndexCallKind::Advance) {
                return Err(error);
            }
            if control.registration != self.registered() {
                return Err(IndexBuildCallError::Typed(IndexBuildError::Store(
                    IndexBuildStoreError::InvalidControl,
                )));
            }
            self.done.set(true);
            Ok(self.status(control.registration.physical_index_id))
        }

        fn seal(
            &self,
            canister: Principal,
            request: IndexBuildSealRequest,
        ) -> Result<IndexBuildSealStatus, IndexBuildCallError> {
            self.calls.borrow_mut().push(IndexCall::Seal {
                canister,
                request: request.clone(),
            });
            self.timeline.borrow_mut().push(TimelineEvent::IndexSeal);
            if let Some(error) = self.take_failure(IndexCallKind::Seal) {
                return Err(error);
            }
            let registration = self.registered();
            if request.control.registration != registration {
                return Err(IndexBuildCallError::Typed(IndexBuildError::Store(
                    IndexBuildStoreError::InvalidControl,
                )));
            }
            let already_sealed = matches!(self.phase.get(), IndexBuildPhase::Sealing { .. })
                && self.seal_epoch.get() == request.seal_catalog_epoch
                && *self.seal_targets.borrow() == request.shard_targets;
            if already_sealed {
                return Ok(self.seal_status());
            }
            if !matches!(self.phase.get(), IndexBuildPhase::Building) {
                return Err(IndexBuildCallError::Typed(IndexBuildError::Store(
                    IndexBuildStoreError::Aborted,
                )));
            }
            if request.seal_catalog_epoch <= registration.catalog_epoch
                || request
                    .shard_targets
                    .windows(2)
                    .any(|pair| pair[0].shard_id >= pair[1].shard_id)
            {
                return Err(IndexBuildCallError::Typed(IndexBuildError::Store(
                    IndexBuildStoreError::InvalidSeal,
                )));
            }
            self.phase.set(IndexBuildPhase::Sealing {
                seal_catalog_epoch: request.seal_catalog_epoch,
            });
            self.seal_epoch.set(request.seal_catalog_epoch);
            *self.seal_targets.borrow_mut() = request.shard_targets;
            Ok(self.seal_status())
        }

        fn abort(
            &self,
            canister: Principal,
            control: IndexBuildControlRequest,
        ) -> Result<IndexBuildCleanupStatus, IndexBuildCallError> {
            self.calls.borrow_mut().push(IndexCall::Abort {
                canister,
                control: control.clone(),
            });
            self.timeline.borrow_mut().push(TimelineEvent::IndexAbort);
            if let Some(error) = self.take_failure(IndexCallKind::Abort) {
                return Err(error);
            }
            if control.registration != self.registered() {
                return Err(IndexBuildCallError::Typed(IndexBuildError::Store(
                    IndexBuildStoreError::InvalidControl,
                )));
            }
            if matches!(self.phase.get(), IndexBuildPhase::Aborted) {
                return Ok(IndexBuildCleanupStatus { done: true });
            }
            let steps = self.abort_steps.get();
            if steps > 1 {
                self.abort_steps.set(steps - 1);
                self.phase.set(IndexBuildPhase::Aborting);
                return Ok(IndexBuildCleanupStatus { done: false });
            }
            self.phase.set(IndexBuildPhase::Aborted);
            Ok(IndexBuildCleanupStatus { done: true })
        }

        fn status_call(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
        ) -> Result<IndexBuildStatus, IndexBuildCallError> {
            self.calls.borrow_mut().push(IndexCall::Status {
                canister,
                physical_index_id,
            });
            self.timeline.borrow_mut().push(TimelineEvent::IndexStatus);
            if let Some(error) = self.take_failure(IndexCallKind::Status) {
                return Err(error);
            }
            if self.registered.borrow().is_none()
                || physical_index_id != self.registered().physical_index_id
            {
                return Err(IndexBuildCallError::Typed(IndexBuildError::Store(
                    IndexBuildStoreError::UnknownBuild,
                )));
            }
            Ok(self.status(physical_index_id))
        }
    }

    impl IndexBuildClient for FakeIndexBuildClient {
        fn register_index_build(
            &self,
            canister: Principal,
            request: RegisterIndexBuildRequest,
        ) -> Pin<Box<dyn Future<Output = Result<IndexBuildStatus, IndexBuildCallError>> + '_>>
        {
            Box::pin(async move { self.register(canister, request) })
        }

        fn advance_index_build(
            &self,
            canister: Principal,
            request: IndexBuildControlRequest,
        ) -> Pin<Box<dyn Future<Output = Result<IndexBuildStatus, IndexBuildCallError>> + '_>>
        {
            Box::pin(async move { self.advance(canister, request) })
        }

        fn seal_index_build(
            &self,
            canister: Principal,
            request: IndexBuildSealRequest,
        ) -> Pin<Box<dyn Future<Output = Result<IndexBuildSealStatus, IndexBuildCallError>> + '_>>
        {
            Box::pin(async move { self.seal(canister, request) })
        }

        fn abort_index_build(
            &self,
            canister: Principal,
            request: IndexBuildControlRequest,
        ) -> Pin<Box<dyn Future<Output = Result<IndexBuildCleanupStatus, IndexBuildCallError>> + '_>>
        {
            Box::pin(async move { self.abort(canister, request) })
        }

        fn index_build_status(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
        ) -> Pin<Box<dyn Future<Output = Result<IndexBuildStatus, IndexBuildCallError>> + '_>>
        {
            Box::pin(async move { self.status_call(canister, physical_index_id) })
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    enum GraphScopeCall {
        Register {
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            scope: CanonicalExportScope,
        },
        Seal {
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            expected_scope: CanonicalExportScope,
            new_epoch: u64,
        },
        Drain {
            canister: Principal,
            request: IndexBuildOutboxDrainRequest,
        },
        Activate {
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            proof: IndexBuildSealStatus,
        },
        Abort {
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            expected_scope: CanonicalExportScope,
        },
        Remove {
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            expected_scope: CanonicalExportScope,
        },
    }

    #[derive(Clone, Debug)]
    struct FakeScope {
        scope: CanonicalExportScope,
        phase: CanonicalExportPhase,
        epoch: u64,
        admitted_through: u64,
        drained_through: u64,
        outbox_remaining: u64,
    }

    /// Stateful Graph export-scope fake mirroring the real Graph contract: exact-identity seal
    /// (idempotent), existence-only drain that feeds the graph-index ledger, proof-validated
    /// activation, identity-checked abort/remove with the drained==admitted removal guard.
    /// `Clone` shares the underlying state so the driver and the test observe the same calls.
    #[derive(Clone)]
    struct FakeGraphScopeClient {
        calls: Rc<RefCell<Vec<GraphScopeCall>>>,
        timeline: SharedTimeline,
        scopes: Rc<RefCell<BTreeMap<(Principal, PhysicalIndexId), FakeScope>>>,
        shard_by_canister: Rc<RefCell<BTreeMap<Principal, u32>>>,
        drained_by_shard: SharedLedger,
        fail_seal_on: Rc<RefCell<Option<Principal>>>,
    }

    impl FakeGraphScopeClient {
        fn new(timeline: SharedTimeline, drained_by_shard: SharedLedger) -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                timeline,
                scopes: Rc::new(RefCell::new(BTreeMap::new())),
                shard_by_canister: Rc::new(RefCell::new(BTreeMap::new())),
                drained_by_shard,
                fail_seal_on: Rc::new(RefCell::new(None)),
            }
        }

        fn seed_shard(&self, canister: Principal, shard_id: u32) {
            self.shard_by_canister
                .borrow_mut()
                .insert(canister, shard_id);
        }

        fn seed_scope(
            &self,
            canister: Principal,
            shard_id: u32,
            physical_index_id: PhysicalIndexId,
            scope: CanonicalExportScope,
            admitted_through: u64,
            outbox_remaining: u64,
        ) {
            self.shard_by_canister
                .borrow_mut()
                .insert(canister, shard_id);
            let epoch = scope.catalog_epoch;
            self.scopes.borrow_mut().insert(
                (canister, physical_index_id),
                FakeScope {
                    scope,
                    phase: CanonicalExportPhase::Building,
                    epoch,
                    admitted_through,
                    drained_through: 0,
                    outbox_remaining,
                },
            );
        }

        fn set_fail_seal(&self, canister: Principal) {
            *self.fail_seal_on.borrow_mut() = Some(canister);
        }

        fn set_outbox(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            outbox_remaining: u64,
        ) {
            let mut scopes = self.scopes.borrow_mut();
            if let Some(record) = scopes.get_mut(&(canister, physical_index_id)) {
                // Keep the fake invariant outbox == admitted - drained so convergence is
                // computed exactly like the real Graph drain progress.
                record.outbox_remaining = outbox_remaining;
                record.drained_through = record.admitted_through.saturating_sub(outbox_remaining);
            }
        }

        /// Seeds a scope already published `Active` at `epoch` with fully drained watermarks: the
        /// durable Graph record left behind when a Router message traps after remote activation.
        fn seed_active_scope(
            &self,
            canister: Principal,
            shard_id: u32,
            physical_index_id: PhysicalIndexId,
            scope: CanonicalExportScope,
            epoch: u64,
            admitted_through: u64,
        ) {
            self.shard_by_canister
                .borrow_mut()
                .insert(canister, shard_id);
            self.scopes.borrow_mut().insert(
                (canister, physical_index_id),
                FakeScope {
                    scope,
                    phase: CanonicalExportPhase::Active,
                    epoch,
                    admitted_through,
                    drained_through: admitted_through,
                    outbox_remaining: 0,
                },
            );
        }

        fn status(
            &self,
            physical_index_id: PhysicalIndexId,
            record: &FakeScope,
        ) -> CanonicalExportStatus {
            CanonicalExportStatus {
                physical_index_id,
                scope: record.scope.clone(),
                phase: record.phase,
                epoch: record.epoch,
                admitted_through: record.admitted_through,
                drained_through: record.drained_through,
            }
        }

        fn shard(&self, canister: Principal) -> u32 {
            *self
                .shard_by_canister
                .borrow()
                .get(&canister)
                .expect("seeded graph canister")
        }

        fn proof_matches(
            &self,
            record: &FakeScope,
            proof: &IndexBuildSealStatus,
            canister: Principal,
        ) -> bool {
            if !proof.base_complete
                || proof.seal_catalog_epoch != record.epoch
                || !proof
                    .watermarks
                    .iter()
                    .all(|watermark| watermark.admitted_through == watermark.drained_through)
            {
                return false;
            }
            proof.watermarks.iter().any(|watermark| {
                watermark.shard_id == self.shard(canister)
                    && watermark.admitted_through == record.admitted_through
                    && watermark.drained_through == record.admitted_through
            })
        }

        fn register(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            scope: CanonicalExportScope,
        ) -> Result<(), GraphScopeCallError> {
            self.calls.borrow_mut().push(GraphScopeCall::Register {
                canister,
                physical_index_id,
                scope: scope.clone(),
            });
            self.timeline
                .borrow_mut()
                .push(TimelineEvent::GraphRegister(self.shard(canister)));
            let key = (canister, physical_index_id);
            let mut scopes = self.scopes.borrow_mut();
            if let Some(existing) = scopes.get(&key) {
                return if existing.scope == scope {
                    Ok(())
                } else {
                    Err(GraphScopeCallError::Typed(
                        CanonicalExportError::ScopeConflict,
                    ))
                };
            }
            let epoch = scope.catalog_epoch;
            scopes.insert(
                key,
                FakeScope {
                    scope,
                    phase: CanonicalExportPhase::Building,
                    epoch,
                    admitted_through: 0,
                    drained_through: 0,
                    outbox_remaining: 0,
                },
            );
            Ok(())
        }

        fn seal(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            expected_scope: CanonicalExportScope,
            new_epoch: u64,
        ) -> Result<CanonicalExportStatus, GraphScopeCallError> {
            self.calls.borrow_mut().push(GraphScopeCall::Seal {
                canister,
                physical_index_id,
                expected_scope: expected_scope.clone(),
                new_epoch,
            });
            self.timeline
                .borrow_mut()
                .push(TimelineEvent::GraphSeal(self.shard(canister)));
            if *self.fail_seal_on.borrow() == Some(canister) {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::ScopeMismatch,
                ));
            }
            let key = (canister, physical_index_id);
            let mut scopes = self.scopes.borrow_mut();
            let Some(record) = scopes.get_mut(&key) else {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::ScopeNotFound,
                ));
            };
            if record.scope != expected_scope {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::ScopeMismatch,
                ));
            }
            if record.epoch == new_epoch
                && matches!(
                    record.phase,
                    CanonicalExportPhase::Sealing | CanonicalExportPhase::Active
                )
            {
                return Ok(self.status(physical_index_id, record));
            }
            if record.phase != CanonicalExportPhase::Building || new_epoch <= record.epoch {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::InvalidPhase,
                ));
            }
            record.phase = CanonicalExportPhase::Sealing;
            record.epoch = new_epoch;
            Ok(self.status(physical_index_id, record))
        }

        fn drain(
            &self,
            canister: Principal,
            request: IndexBuildOutboxDrainRequest,
        ) -> Result<IndexBuildOutboxDrainProgress, GraphScopeCallError> {
            self.calls.borrow_mut().push(GraphScopeCall::Drain {
                canister,
                request: request.clone(),
            });
            self.timeline
                .borrow_mut()
                .push(TimelineEvent::GraphDrain(self.shard(canister)));
            let key = (canister, request.physical_index_id);
            let mut scopes = self.scopes.borrow_mut();
            let Some(record) = scopes.get_mut(&key) else {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::ScopeNotFound,
                ));
            };
            let entries = record.outbox_remaining.min(u64::from(request.max_entries));
            record.outbox_remaining -= entries;
            record.drained_through += entries;
            let shard_id = self.shard(canister);
            self.drained_by_shard
                .borrow_mut()
                .insert(shard_id, record.drained_through);
            Ok(IndexBuildOutboxDrainProgress {
                drained: u32::try_from(entries).unwrap_or(u32::MAX),
                remaining: record.outbox_remaining,
                converged: record.outbox_remaining == 0
                    && record.drained_through == record.admitted_through,
            })
        }

        fn activate(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            proof: IndexBuildSealStatus,
        ) -> Result<CanonicalExportStatus, GraphScopeCallError> {
            self.calls.borrow_mut().push(GraphScopeCall::Activate {
                canister,
                physical_index_id,
                proof: proof.clone(),
            });
            self.timeline
                .borrow_mut()
                .push(TimelineEvent::GraphActivate(self.shard(canister)));
            let key = (canister, physical_index_id);
            let mut scopes = self.scopes.borrow_mut();
            let Some(record) = scopes.get_mut(&key) else {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::ScopeNotFound,
                ));
            };
            if record.phase == CanonicalExportPhase::Active {
                if self.proof_matches(record, &proof, canister) {
                    return Ok(self.status(physical_index_id, record));
                }
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::NotConverged,
                ));
            }
            if record.phase != CanonicalExportPhase::Sealing
                || !self.proof_matches(record, &proof, canister)
            {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::NotConverged,
                ));
            }
            record.phase = CanonicalExportPhase::Active;
            Ok(self.status(physical_index_id, record))
        }

        fn abort_scope(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            expected_scope: CanonicalExportScope,
        ) -> Result<CanonicalExportStatus, GraphScopeCallError> {
            self.calls.borrow_mut().push(GraphScopeCall::Abort {
                canister,
                physical_index_id,
                expected_scope: expected_scope.clone(),
            });
            self.timeline
                .borrow_mut()
                .push(TimelineEvent::GraphAbort(self.shard(canister)));
            let key = (canister, physical_index_id);
            let mut scopes = self.scopes.borrow_mut();
            let Some(record) = scopes.get_mut(&key) else {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::ScopeNotFound,
                ));
            };
            if record.scope != expected_scope {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::ScopeMismatch,
                ));
            }
            if record.phase == CanonicalExportPhase::Active {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::InvalidPhase,
                ));
            }
            if record.phase != CanonicalExportPhase::Aborting {
                record.phase = CanonicalExportPhase::Aborting;
            }
            Ok(self.status(physical_index_id, record))
        }

        fn remove_scope(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            expected_scope: CanonicalExportScope,
        ) -> Result<(), GraphScopeCallError> {
            self.calls.borrow_mut().push(GraphScopeCall::Remove {
                canister,
                physical_index_id,
                expected_scope: expected_scope.clone(),
            });
            self.timeline
                .borrow_mut()
                .push(TimelineEvent::GraphRemove(self.shard(canister)));
            let key = (canister, physical_index_id);
            let mut scopes = self.scopes.borrow_mut();
            let Some(record) = scopes.get(&key) else {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::ScopeNotFound,
                ));
            };
            if record.scope != expected_scope {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::ScopeMismatch,
                ));
            }
            if matches!(
                record.phase,
                CanonicalExportPhase::Active | CanonicalExportPhase::Sealing
            ) {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::InvalidPhase,
                ));
            }
            if record.drained_through != record.admitted_through {
                return Err(GraphScopeCallError::Typed(
                    CanonicalExportError::UnsafeRemoval,
                ));
            }
            scopes.remove(&key);
            Ok(())
        }
    }

    impl GraphScopeClient for FakeGraphScopeClient {
        fn register_scope(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            scope: CanonicalExportScope,
        ) -> Pin<Box<dyn Future<Output = Result<(), GraphScopeCallError>> + '_>> {
            Box::pin(async move { self.register(canister, physical_index_id, scope) })
        }

        fn seal_scope(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            expected_scope: CanonicalExportScope,
            new_epoch: u64,
        ) -> Pin<Box<dyn Future<Output = Result<CanonicalExportStatus, GraphScopeCallError>> + '_>>
        {
            Box::pin(
                async move { self.seal(canister, physical_index_id, expected_scope, new_epoch) },
            )
        }

        fn activate_scope(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            proof: IndexBuildSealStatus,
        ) -> Pin<Box<dyn Future<Output = Result<CanonicalExportStatus, GraphScopeCallError>> + '_>>
        {
            Box::pin(async move { self.activate(canister, physical_index_id, proof) })
        }

        fn abort_scope(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            expected_scope: CanonicalExportScope,
        ) -> Pin<Box<dyn Future<Output = Result<CanonicalExportStatus, GraphScopeCallError>> + '_>>
        {
            Box::pin(async move { self.abort_scope(canister, physical_index_id, expected_scope) })
        }

        fn remove_scope(
            &self,
            canister: Principal,
            physical_index_id: PhysicalIndexId,
            expected_scope: CanonicalExportScope,
        ) -> Pin<Box<dyn Future<Output = Result<(), GraphScopeCallError>> + '_>> {
            Box::pin(async move { self.remove_scope(canister, physical_index_id, expected_scope) })
        }

        fn drain_outbox(
            &self,
            canister: Principal,
            request: IndexBuildOutboxDrainRequest,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<IndexBuildOutboxDrainProgress, GraphScopeCallError>>
                    + '_,
            >,
        > {
            Box::pin(async move { self.drain(canister, request) })
        }
    }

    fn export_scope(
        shard_id: u32,
        graph_canister: Principal,
        catalog_epoch: u64,
    ) -> IndexMigrationExportScope {
        IndexMigrationExportScope {
            shard_id,
            graph_canister,
            scope: CanonicalExportScope {
                graph_id: GraphId::from_raw(0),
                index_name_id: IndexNameId::from_raw(9),
                catalog_epoch,
                target: CanonicalExportTarget::Vertex {
                    label_id: 1,
                    property_id: PropertyId::from_raw(2),
                },
                inline: None,
            },
        }
    }

    fn step_request(action: IndexMigrationStepAction) -> IndexMigrationStepRequest {
        // Registration epoch 7; Seal/Cleanup carry the fresh lifecycle epoch 8.
        let lifecycle_catalog_epoch = if matches!(
            action,
            IndexMigrationStepAction::Seal | IndexMigrationStepAction::Cleanup
        ) {
            8
        } else {
            7
        };
        let registration = RegisterIndexBuildRequest {
            physical_index_id: PhysicalIndexId::new(42).expect("physical id"),
            graph_id: GraphId::from_raw(0),
            index_name_id: IndexNameId::from_raw(9),
            catalog_epoch: 7,
            topology_epoch: 5,
            target: IndexBuildTarget::Vertex {
                label_id: 1,
                property_id: PropertyId::from_raw(2),
            },
            target_shard_ids: vec![0, 1],
        };
        IndexMigrationStepRequest {
            migration_id: "000042_seal_index".into(),
            topology_epoch: 5,
            lifecycle_catalog_epoch,
            index_canister: index_canister(),
            registration,
            export_scopes: vec![
                export_scope(0, graph_0(), lifecycle_catalog_epoch),
                export_scope(1, graph_1(), lifecycle_catalog_epoch),
            ],
            action,
        }
    }

    /// The frozen registration scope identity (registration epoch) that the driver must present on
    /// seal/abort/remove.
    fn seeded_scope(
        scope: &IndexMigrationExportScope,
        request: &IndexMigrationStepRequest,
    ) -> CanonicalExportScope {
        let mut seeded = scope.scope.clone();
        seeded.catalog_epoch = request.registration.catalog_epoch;
        seeded
    }

    fn assert_echo(request: &IndexMigrationStepRequest, response: &IndexMigrationStepResponse) {
        assert_eq!(response.migration_id, request.migration_id);
        assert_eq!(response.topology_epoch, request.topology_epoch);
        assert_eq!(
            response.lifecycle_catalog_epoch,
            request.lifecycle_catalog_epoch
        );
        assert_eq!(response.index_canister, request.index_canister);
        assert_eq!(response.registration, request.registration);
        assert_eq!(response.export_scopes, request.export_scopes);
    }

    /// The single retryability classifier must map a completed-but-undecodable reply to a
    /// deterministic terminal stale-response (never a transport retry), transport loss to a
    /// retryable exact-envelope replay, and typed owner rejections to retryable or terminal by
    /// category.
    #[test]
    fn classifier_maps_decode_transport_and_typed_failures_to_their_retry_policy() {
        assert_eq!(
            classify_index_build_call(IndexBuildCallError::Decode),
            IndexMigrationDriveError::Terminal(MigrationFailureCode::StaleOrMismatchedResponse)
        );
        assert_eq!(
            classify_graph_scope_call(GraphScopeCallError::Decode),
            IndexMigrationDriveError::Terminal(MigrationFailureCode::StaleOrMismatchedResponse)
        );
        assert_eq!(
            classify_index_build_call(IndexBuildCallError::Transport),
            IndexMigrationDriveError::Retryable
        );
        assert_eq!(
            classify_graph_scope_call(GraphScopeCallError::Transport),
            IndexMigrationDriveError::Retryable
        );
        assert_eq!(
            classify_graph_scope_call(GraphScopeCallError::Typed(
                CanonicalExportError::RetryableSealing
            )),
            IndexMigrationDriveError::Retryable
        );
        assert_eq!(
            classify_graph_scope_call(GraphScopeCallError::Typed(
                CanonicalExportError::InvalidPhase
            )),
            IndexMigrationDriveError::Terminal(MigrationFailureCode::TargetRejected)
        );
        assert_eq!(
            classify_index_build_call(IndexBuildCallError::Typed(
                gleaph_graph_kernel::index::IndexBuildError::Graph(
                    CanonicalExportError::RetryableSealing
                )
            )),
            IndexMigrationDriveError::Retryable
        );
        assert_eq!(
            classify_index_build_call(IndexBuildCallError::Typed(
                gleaph_graph_kernel::index::IndexBuildError::Store(
                    gleaph_graph_kernel::index::IndexBuildStoreError::UnknownBuild
                )
            )),
            IndexMigrationDriveError::Terminal(MigrationFailureCode::TargetRejected)
        );
    }

    #[test]
    fn driver_register_registers_index_then_every_graph_scope_and_replays_exactly() {
        let (index_fake, graph_fake, timeline, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());
        let request = step_request(IndexMigrationStepAction::Register);

        let response = drive(&driver, request.clone()).expect("register");
        assert!(matches!(
            &response.result,
            IndexMigrationStepResult::Registered(status)
                if status.registration == request.registration
                    && status.phase == IndexBuildPhase::Building
        ));
        assert_echo(&request, &response);
        assert_eq!(
            *index_fake.calls.borrow(),
            vec![IndexCall::Register {
                canister: index_canister(),
                request: request.registration.clone(),
            }]
        );
        assert_eq!(
            *graph_fake.calls.borrow(),
            vec![
                GraphScopeCall::Register {
                    canister: graph_0(),
                    physical_index_id: request.registration.physical_index_id,
                    scope: request.export_scopes[0].scope.clone(),
                },
                GraphScopeCall::Register {
                    canister: graph_1(),
                    physical_index_id: request.registration.physical_index_id,
                    scope: request.export_scopes[1].scope.clone(),
                },
            ]
        );
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::IndexRegister,
                TimelineEvent::GraphRegister(0),
                TimelineEvent::GraphRegister(1),
            ]
        );

        // Exact replay drives the same idempotent calls with the same envelope.
        let replay = drive(&driver, request.clone()).expect("idempotent replay");
        assert!(matches!(
            replay.result,
            IndexMigrationStepResult::Registered(_)
        ));
        assert_echo(&request, &replay);
        assert_eq!(index_fake.calls.borrow().len(), 2);
        assert_eq!(graph_fake.calls.borrow().len(), 4);
    }

    #[test]
    fn driver_build_advances_the_index_and_returns_build_progress() {
        let (index_fake, graph_fake, timeline, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());
        let register = step_request(IndexMigrationStepAction::Register);
        drive(&driver, register.clone()).expect("register first");

        let request = step_request(IndexMigrationStepAction::Build);
        let response = drive(&driver, request.clone()).expect("build");
        assert!(matches!(
            &response.result,
            IndexMigrationStepResult::BuildProgress(status)
                if status.progress.done && status.registration == request.registration
        ));
        assert_echo(&request, &response);
        assert_eq!(
            *index_fake.calls.borrow(),
            vec![
                IndexCall::Register {
                    canister: index_canister(),
                    request: register.registration.clone(),
                },
                IndexCall::Advance {
                    canister: index_canister(),
                    control: IndexBuildControlRequest {
                        registration: request.registration.clone(),
                    },
                },
            ]
        );
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::IndexRegister,
                TimelineEvent::GraphRegister(0),
                TimelineEvent::GraphRegister(1),
                TimelineEvent::IndexAdvance,
            ]
        );
    }

    #[test]
    fn driver_build_transport_error_is_retryable() {
        let (index_fake, graph_fake, _, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());
        drive(&driver, step_request(IndexMigrationStepAction::Register)).expect("register");
        index_fake.fail_next(IndexCallKind::Advance, IndexBuildCallError::Transport);

        let error =
            drive(&driver, step_request(IndexMigrationStepAction::Build)).expect_err("transport");
        assert_eq!(error, IndexMigrationDriveError::Retryable);
        assert_eq!(index_fake.calls.borrow().len(), 2);
    }

    #[test]
    fn driver_seal_freezes_scopes_drains_and_activates_after_proof_refresh() {
        let (index_fake, graph_fake, timeline, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());
        let request = step_request(IndexMigrationStepAction::Seal);
        let pid = request.registration.physical_index_id;
        graph_fake.seed_scope(
            graph_0(),
            0,
            pid,
            seeded_scope(&request.export_scopes[0], &request),
            23,
            23,
        );
        graph_fake.seed_scope(
            graph_1(),
            1,
            pid,
            seeded_scope(&request.export_scopes[1], &request),
            23,
            23,
        );
        index_fake.seed_build(request.registration.clone(), true);

        // Drive 1: the drain converges, but the step-2 seal proof predates the drain, so the first
        // activation is NotConverged -> Retryable (the exact envelope remains resumable).
        let first = drive(&driver, request.clone()).expect_err("stale proof is retryable");
        assert_eq!(first, IndexMigrationDriveError::Retryable);
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::GraphSeal(0),
                TimelineEvent::GraphSeal(1),
                TimelineEvent::IndexSeal,
                TimelineEvent::GraphDrain(0),
                TimelineEvent::GraphDrain(1),
                TimelineEvent::IndexStatus,
                TimelineEvent::GraphActivate(0),
            ]
        );

        // Drive 2: the idempotent seal replay re-reads the refreshed proof; activation completes.
        let response = drive(&driver, request.clone()).expect("converged seal");
        assert_echo(&request, &response);
        match response.result {
            IndexMigrationStepResult::SealProgress {
                watermarks,
                converged,
            } => {
                assert!(converged);
                assert_eq!(
                    watermarks,
                    vec![
                        IndexShardWatermark {
                            shard_id: 0,
                            admitted_through: 23,
                            drained_through: 23,
                        },
                        IndexShardWatermark {
                            shard_id: 1,
                            admitted_through: 23,
                            drained_through: 23,
                        },
                    ]
                );
            }
            other => panic!("unexpected seal result {other:?}"),
        }
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::GraphSeal(0),
                TimelineEvent::GraphSeal(1),
                TimelineEvent::IndexSeal,
                TimelineEvent::GraphDrain(0),
                TimelineEvent::GraphDrain(1),
                TimelineEvent::IndexStatus,
                TimelineEvent::GraphActivate(0),
                TimelineEvent::GraphSeal(0),
                TimelineEvent::GraphSeal(1),
                TimelineEvent::IndexSeal,
                TimelineEvent::GraphDrain(0),
                TimelineEvent::GraphDrain(1),
                TimelineEvent::IndexStatus,
                TimelineEvent::GraphActivate(0),
                TimelineEvent::GraphActivate(1),
            ]
        );

        // Every graph seal presented the FROZEN registration scope and the fresh lifecycle epoch.
        let seals = graph_fake
            .calls
            .borrow()
            .iter()
            .filter_map(|call| match call {
                GraphScopeCall::Seal {
                    expected_scope,
                    new_epoch,
                    ..
                } => Some((expected_scope.catalog_epoch, *new_epoch)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(seals.len(), 4);
        assert!(seals.iter().all(|(catalog_epoch, new_epoch)| {
            *catalog_epoch == request.registration.catalog_epoch
                && *new_epoch == request.lifecycle_catalog_epoch
        }));
        let index_seal = index_fake
            .calls
            .borrow()
            .iter()
            .find_map(|call| match call {
                IndexCall::Seal { request, .. } => Some(request.clone()),
                _ => None,
            })
            .expect("index seal call");
        assert_eq!(index_seal.control.registration, request.registration);
        assert_eq!(
            index_seal.seal_catalog_epoch,
            request.lifecycle_catalog_epoch
        );
        assert_eq!(
            index_seal.shard_targets,
            vec![
                IndexBuildSealTarget {
                    shard_id: 0,
                    admitted_through: 23,
                },
                IndexBuildSealTarget {
                    shard_id: 1,
                    admitted_through: 23,
                },
            ]
        );
    }

    #[test]
    fn driver_seal_transport_loss_returns_retryable_without_activation() {
        let (index_fake, graph_fake, timeline, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());
        let request = step_request(IndexMigrationStepAction::Seal);
        let pid = request.registration.physical_index_id;
        graph_fake.seed_scope(
            graph_0(),
            0,
            pid,
            seeded_scope(&request.export_scopes[0], &request),
            23,
            23,
        );
        graph_fake.seed_scope(
            graph_1(),
            1,
            pid,
            seeded_scope(&request.export_scopes[1], &request),
            23,
            23,
        );
        index_fake.seed_build(request.registration.clone(), true);
        index_fake.fail_next(IndexCallKind::Seal, IndexBuildCallError::Transport);

        let error = drive(&driver, request.clone()).expect_err("seal transport");
        assert_eq!(error, IndexMigrationDriveError::Retryable);
        // No drain, no status poll, no activation followed the transport loss.
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::GraphSeal(0),
                TimelineEvent::GraphSeal(1),
                TimelineEvent::IndexSeal,
            ]
        );
        let index_calls = index_fake.calls.borrow();
        assert_eq!(index_calls.len(), 1);
        let IndexCall::Seal {
            canister,
            request: seal_request,
        } = &index_calls[0]
        else {
            panic!("expected the index seal call");
        };
        assert_eq!(*canister, index_canister());
        assert_eq!(seal_request.control.registration, request.registration);
        assert_eq!(
            seal_request.seal_catalog_epoch,
            request.lifecycle_catalog_epoch
        );
    }

    #[test]
    fn driver_seal_scope_mismatch_is_terminal_target_rejected() {
        let (index_fake, graph_fake, timeline, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());
        let request = step_request(IndexMigrationStepAction::Seal);
        let pid = request.registration.physical_index_id;
        // Seed both scopes under the UNFROZEN lifecycle epoch: the driver's frozen identity
        // (registration epoch) mismatches deterministically before any graph-index call.
        graph_fake.seed_scope(
            graph_0(),
            0,
            pid,
            request.export_scopes[0].scope.clone(),
            23,
            0,
        );
        graph_fake.seed_scope(
            graph_1(),
            1,
            pid,
            request.export_scopes[1].scope.clone(),
            23,
            0,
        );

        let error = drive(&driver, request).expect_err("scope mismatch");
        assert_eq!(
            error,
            IndexMigrationDriveError::Terminal(MigrationFailureCode::TargetRejected)
        );
        assert_eq!(*timeline.borrow(), vec![TimelineEvent::GraphSeal(0)]);
        assert!(index_fake.calls.borrow().is_empty());
    }

    /// A Router message that traps after remote activation but before persisting `Converged`
    /// leaves every scope `Active` remotely. Re-driving the same exact seal envelope must not
    /// strand the migration: the already-`Active` scope is an exact replay, graph-index re-seals
    /// idempotently, and the drive converges and re-activates without error.
    #[test]
    fn driver_seal_resumes_after_activation_crash_window() {
        let (index_fake, graph_fake, timeline, ledger) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());
        let request = step_request(IndexMigrationStepAction::Seal);
        let pid = request.registration.physical_index_id;
        // Both scopes were activated by the trapped drive: frozen registration identity, lifecycle
        // epoch 8, fully drained watermarks captured at 23.
        graph_fake.seed_active_scope(
            graph_0(),
            0,
            pid,
            seeded_scope(&request.export_scopes[0], &request),
            request.lifecycle_catalog_epoch,
            23,
        );
        graph_fake.seed_active_scope(
            graph_1(),
            1,
            pid,
            seeded_scope(&request.export_scopes[1], &request),
            request.lifecycle_catalog_epoch,
            23,
        );
        // graph-index was already sealed by the trapped drive at the same epoch and targets, and
        // its acknowledged watermarks are durable at 23 across the crash (the trapped drive's
        // drain completed before it activated the scopes).
        index_fake.seed_sealed(
            request.registration.clone(),
            true,
            request.lifecycle_catalog_epoch,
            vec![
                IndexBuildSealTarget {
                    shard_id: 0,
                    admitted_through: 23,
                },
                IndexBuildSealTarget {
                    shard_id: 1,
                    admitted_through: 23,
                },
            ],
        );
        ledger.borrow_mut().insert(0, 23);
        ledger.borrow_mut().insert(1, 23);

        let response = drive(&driver, request.clone()).expect("crash-window seal resume converges");
        assert_echo(&request, &response);
        match response.result {
            IndexMigrationStepResult::SealProgress {
                watermarks,
                converged,
            } => {
                assert!(converged);
                assert_eq!(
                    watermarks,
                    vec![
                        IndexShardWatermark {
                            shard_id: 0,
                            admitted_through: 23,
                            drained_through: 23,
                        },
                        IndexShardWatermark {
                            shard_id: 1,
                            admitted_through: 23,
                            drained_through: 23,
                        },
                    ]
                );
            }
            other => panic!("unexpected seal result {other:?}"),
        }
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::GraphSeal(0),
                TimelineEvent::GraphSeal(1),
                TimelineEvent::IndexSeal,
                TimelineEvent::GraphDrain(0),
                TimelineEvent::GraphDrain(1),
                TimelineEvent::IndexStatus,
                TimelineEvent::GraphActivate(0),
                TimelineEvent::GraphActivate(1),
            ]
        );
    }

    #[test]
    fn driver_cleanup_drains_before_abort_then_aborts_and_removes_scopes() {
        let (index_fake, graph_fake, timeline, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());
        let request = step_request(IndexMigrationStepAction::Cleanup);
        let pid = request.registration.physical_index_id;
        graph_fake.seed_scope(
            graph_0(),
            0,
            pid,
            seeded_scope(&request.export_scopes[0], &request),
            23,
            23,
        );
        graph_fake.seed_scope(
            graph_1(),
            1,
            pid,
            seeded_scope(&request.export_scopes[1], &request),
            23,
            23,
        );
        index_fake.seed_build(request.registration.clone(), true);

        let response = drive(&driver, request.clone()).expect("cleanup");
        assert_echo(&request, &response);
        assert_eq!(
            response.result,
            IndexMigrationStepResult::CleanupProgress { done: true }
        );
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::GraphDrain(0),
                TimelineEvent::GraphDrain(1),
                TimelineEvent::IndexAbort,
                TimelineEvent::GraphAbort(0),
                TimelineEvent::GraphAbort(1),
                TimelineEvent::GraphRemove(0),
                TimelineEvent::GraphRemove(1),
            ]
        );
        // Abort/remove carry the frozen registration scope identity.
        let aborts = graph_fake
            .calls
            .borrow()
            .iter()
            .filter_map(|call| match call {
                GraphScopeCall::Abort { expected_scope, .. } => Some(expected_scope.catalog_epoch),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(aborts, vec![7, 7]);
        let removes = graph_fake
            .calls
            .borrow()
            .iter()
            .filter_map(|call| match call {
                GraphScopeCall::Remove { expected_scope, .. } => Some(expected_scope.catalog_epoch),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(removes, vec![7, 7]);
        let index_abort = index_fake
            .calls
            .borrow()
            .iter()
            .find_map(|call| match call {
                IndexCall::Abort { control, .. } => Some(control.registration.clone()),
                _ => None,
            })
            .expect("index abort call");
        assert_eq!(index_abort, request.registration);
    }

    #[test]
    fn driver_cleanup_bounded_drain_defers_abort_and_remove() {
        let (index_fake, graph_fake, timeline, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());
        let request = step_request(IndexMigrationStepAction::Cleanup);
        let pid = request.registration.physical_index_id;
        // Shard 0 holds more entries than the per-drive drain cap: the drive returns done:false
        // without abort. Shard 1's smaller outbox fits in the next drive.
        let overflow = u64::from(MAX_OUTBOX_DRAIN_ENTRIES_PER_CALL)
            * u64::from(MAX_OUTBOX_DRAIN_CALLS_PER_DRIVE)
            + 1;
        graph_fake.seed_scope(
            graph_0(),
            0,
            pid,
            seeded_scope(&request.export_scopes[0], &request),
            overflow,
            overflow,
        );
        graph_fake.seed_scope(
            graph_1(),
            1,
            pid,
            seeded_scope(&request.export_scopes[1], &request),
            40,
            40,
        );
        index_fake.seed_build(request.registration.clone(), true);

        let response = drive(&driver, request.clone()).expect("bounded cleanup");
        assert_echo(&request, &response);
        assert_eq!(
            response.result,
            IndexMigrationStepResult::CleanupProgress { done: false }
        );
        assert!(
            index_fake.calls.borrow().is_empty(),
            "graph-index abort must wait for drain convergence"
        );
        assert_eq!(
            timeline.borrow().len(),
            MAX_OUTBOX_DRAIN_CALLS_PER_DRIVE as usize
        );
        assert!(
            timeline
                .borrow()
                .iter()
                .all(|event| matches!(event, TimelineEvent::GraphDrain(0)))
        );

        // The next drive resumes the drain (one entry left per scope) and only then aborts/removes.
        graph_fake.set_outbox(graph_0(), pid, 1);
        graph_fake.set_outbox(graph_1(), pid, 1);
        let response = drive(&driver, request).expect("continued cleanup");
        assert_eq!(
            response.result,
            IndexMigrationStepResult::CleanupProgress { done: true }
        );
        let abort_position = timeline
            .borrow()
            .iter()
            .position(|event| *event == TimelineEvent::IndexAbort)
            .expect("abort call");
        assert!(
            timeline
                .borrow()
                .iter()
                .take(abort_position)
                .any(|event| { *event == TimelineEvent::GraphDrain(0) })
        );
        assert!(
            timeline
                .borrow()
                .iter()
                .take(abort_position)
                .any(|event| { *event == TimelineEvent::GraphDrain(1) })
        );
    }

    #[test]
    fn driver_cleanup_waits_for_graph_index_abort_before_removing_scopes() {
        let (index_fake, graph_fake, timeline, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());
        let request = step_request(IndexMigrationStepAction::Cleanup);
        let pid = request.registration.physical_index_id;
        graph_fake.seed_scope(
            graph_0(),
            0,
            pid,
            seeded_scope(&request.export_scopes[0], &request),
            23,
            23,
        );
        graph_fake.seed_scope(
            graph_1(),
            1,
            pid,
            seeded_scope(&request.export_scopes[1], &request),
            23,
            23,
        );
        index_fake.seed_build(request.registration.clone(), true);
        index_fake.abort_steps.set(2);

        // First abort step: scopes abort but removal waits for graph-index cleanup completion.
        let response = drive(&driver, request.clone()).expect("first abort step");
        assert_eq!(
            response.result,
            IndexMigrationStepResult::CleanupProgress { done: false }
        );
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::GraphDrain(0),
                TimelineEvent::GraphDrain(1),
                TimelineEvent::IndexAbort,
                TimelineEvent::GraphAbort(0),
                TimelineEvent::GraphAbort(1),
            ]
        );

        // Second abort step completes graph-index cleanup; only then are scopes removed.
        let response = drive(&driver, request).expect("second abort step");
        assert_eq!(
            response.result,
            IndexMigrationStepResult::CleanupProgress { done: true }
        );
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::GraphDrain(0),
                TimelineEvent::GraphDrain(1),
                TimelineEvent::IndexAbort,
                TimelineEvent::GraphAbort(0),
                TimelineEvent::GraphAbort(1),
                TimelineEvent::GraphDrain(0),
                TimelineEvent::GraphDrain(1),
                TimelineEvent::IndexAbort,
                TimelineEvent::GraphAbort(0),
                TimelineEvent::GraphAbort(1),
                TimelineEvent::GraphRemove(0),
                TimelineEvent::GraphRemove(1),
            ]
        );
    }

    fn integration_fixture(two_shards: bool) -> (RouterStore, Principal, GraphId) {
        let (store, admin, graph_id) = setup_with_shard(ShardId::new(0));
        indexed_catalog::purge_graph_indexes(graph_id);
        RouterStore::commit_intern_vertex_label_name(graph_id, "Person").expect("vertex label");
        RouterStore::commit_intern_property_name(graph_id, "age").expect("property");
        if two_shards {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id: ShardId::new(1),
                    graph_canister: graph_1(),
                    index_canister: index_canister(),
                    logical_graph_name: GRAPH.into(),
                },
            ))
            .expect("register second shard");
        }
        (store, admin, graph_id)
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

    fn progress(
        phase: SchemaMigrationProgressPhase,
        completed: u32,
        total: u32,
    ) -> SchemaMigrationApplyStatus {
        SchemaMigrationApplyStatus::Progress(SchemaMigrationProgress {
            phase,
            completed_targets: completed,
            total_targets: total,
            active_index: 0,
            total_indexes: 1,
        })
    }

    #[test]
    fn real_driver_drives_two_shard_migration_to_applied() {
        let (store, admin, graph_id) = integration_fixture(true);
        let args = index_args("000001_real_index", None, "real_age");
        let (index_fake, graph_fake, timeline, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());

        let fresh = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("fresh prepare");
        assert_eq!(
            result_status(&fresh),
            &progress(SchemaMigrationProgressPhase::Preparing, 0, 1)
        );
        let registered = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("register");
        assert_eq!(
            result_status(&registered),
            &progress(SchemaMigrationProgressPhase::Preparing, 1, 1)
        );
        let building = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("building");
        assert_eq!(
            result_status(&building),
            &progress(SchemaMigrationProgressPhase::Building, 0, 1)
        );
        let built = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("built");
        assert_eq!(
            result_status(&built),
            &progress(SchemaMigrationProgressPhase::Building, 1, 1)
        );
        let sealing = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("sealing");
        assert_eq!(
            result_status(&sealing),
            &progress(SchemaMigrationProgressPhase::Sealing, 0, 1)
        );
        let sealed = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("sealed");
        assert_eq!(
            result_status(&sealed),
            &progress(SchemaMigrationProgressPhase::Sealing, 1, 1)
        );
        let applied =
            futures::executor::block_on(apply_index_migration(&store, admin, args, &driver))
                .expect("applied");
        assert_eq!(
            result_status(&applied),
            &SchemaMigrationApplyStatus::Applied
        );
        assert!(matches!(
            result_record(&applied).state,
            SchemaMigrationRecordState::Applied { .. }
        ));

        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::IndexRegister,
                TimelineEvent::GraphRegister(0),
                TimelineEvent::GraphRegister(1),
                TimelineEvent::IndexAdvance,
                TimelineEvent::GraphSeal(0),
                TimelineEvent::GraphSeal(1),
                TimelineEvent::IndexSeal,
                TimelineEvent::GraphDrain(0),
                TimelineEvent::GraphDrain(1),
                TimelineEvent::IndexStatus,
                TimelineEvent::GraphActivate(0),
                TimelineEvent::GraphActivate(1),
            ]
        );

        let index_name_id = lookup_index_name_id(graph_id, "real_age").expect("index name");
        let active =
            indexed_catalog::get_named_index(graph_id, index_name_id).expect("catalog row");
        assert!(matches!(
            active.lifecycle,
            IndexLifecycleState::Active { .. }
        ));

        // The seal presented the frozen registration scope and the fresh fence epoch.
        let registration = index_fake
            .calls
            .borrow()
            .iter()
            .find_map(|call| match call {
                IndexCall::Register { request, .. } => Some(request.clone()),
                _ => None,
            })
            .expect("registration call");
        let seals = graph_fake
            .calls
            .borrow()
            .iter()
            .filter_map(|call| match call {
                GraphScopeCall::Seal {
                    expected_scope,
                    new_epoch,
                    ..
                } => Some((expected_scope.catalog_epoch, *new_epoch)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            seals,
            vec![(registration.catalog_epoch, registration.catalog_epoch + 1); 2]
        );
    }

    #[test]
    fn real_driver_drives_migration_into_aborting_and_failed_on_scope_mismatch() {
        let (store, admin, graph_id) = integration_fixture(true);
        let args = index_args("000001_fail_index", None, "fail_age");
        let (index_fake, graph_fake, timeline, _) = new_harness();
        graph_fake.seed_shard(graph_0(), 0);
        graph_fake.seed_shard(graph_1(), 1);
        graph_fake.set_fail_seal(graph_1());
        let driver = RealIndexMigrationDriver::new(index_fake.clone(), graph_fake.clone());

        futures::executor::block_on(apply_index_migration(&store, admin, args.clone(), &driver))
            .expect("fresh");
        futures::executor::block_on(apply_index_migration(&store, admin, args.clone(), &driver))
            .expect("register");
        futures::executor::block_on(apply_index_migration(&store, admin, args.clone(), &driver))
            .expect("building");
        futures::executor::block_on(apply_index_migration(&store, admin, args.clone(), &driver))
            .expect("built");
        futures::executor::block_on(apply_index_migration(&store, admin, args.clone(), &driver))
            .expect("sealing");

        let aborting = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("terminal seal");
        assert_eq!(
            result_status(&aborting),
            &progress(SchemaMigrationProgressPhase::Aborting, 0, 1)
        );
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::IndexRegister,
                TimelineEvent::GraphRegister(0),
                TimelineEvent::GraphRegister(1),
                TimelineEvent::IndexAdvance,
                TimelineEvent::GraphSeal(0),
                TimelineEvent::GraphSeal(1),
            ]
        );
        assert!(
            index_fake
                .calls
                .borrow()
                .iter()
                .all(|call| !matches!(call, IndexCall::Seal { .. }))
        );

        let cleaned = futures::executor::block_on(apply_index_migration(
            &store,
            admin,
            args.clone(),
            &driver,
        ))
        .expect("cleanup");
        assert_eq!(
            result_status(&cleaned),
            &progress(SchemaMigrationProgressPhase::Aborting, 1, 1)
        );
        let failed =
            futures::executor::block_on(apply_index_migration(&store, admin, args, &driver))
                .expect("failed");
        assert_eq!(
            result_status(&failed),
            &SchemaMigrationApplyStatus::Failed(MigrationFailureCode::TargetRejected)
        );
        assert!(matches!(
            result_record(&failed).state,
            SchemaMigrationRecordState::Failed {
                code: MigrationFailureCode::TargetRejected,
                ..
            }
        ));
        assert_eq!(
            *timeline.borrow(),
            vec![
                TimelineEvent::IndexRegister,
                TimelineEvent::GraphRegister(0),
                TimelineEvent::GraphRegister(1),
                TimelineEvent::IndexAdvance,
                TimelineEvent::GraphSeal(0),
                TimelineEvent::GraphSeal(1),
                TimelineEvent::GraphDrain(0),
                TimelineEvent::GraphDrain(1),
                TimelineEvent::IndexAbort,
                TimelineEvent::GraphAbort(0),
                TimelineEvent::GraphAbort(1),
                TimelineEvent::GraphRemove(0),
                TimelineEvent::GraphRemove(1),
            ]
        );
        let index_name_id =
            lookup_index_name_id(graph_id, "fail_age").expect("index name retained");
        assert!(indexed_catalog::get_named_index(graph_id, index_name_id).is_none());
    }
}
