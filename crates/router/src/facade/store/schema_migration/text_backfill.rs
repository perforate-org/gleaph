//! Router → text-canister backfill client for `CREATE TEXT INDEX` migrations
//! (ADR 0059 §Text build kind).
//!
//! The five methods map 1:1 onto the text canister's controller-guarded backfill
//! endpoints. The wire shapes below are Candid mirrors of the text-canister surface
//! (`crates/text-canister/src/backfill.rs`): field names, order, and types match exactly,
//! and every kernel newtype (GraphId/IndexNameId/PropertyId/PhysicalIndexId/TextIndexId)
//! is reused from `gleaph-graph-kernel` so encodings agree by construction. The mirror is
//! a deliberate boundary exception: Router must not depend on a canister implementation
//! crate, and no shared wire crate exists yet — the PocketIC E2E proof pins both sides to
//! one wire before production rollout.
//!
//! Rejections from the text canister arrive as plain `String` diagnostics (its v0
//! surface), so they cannot be classified by typed codes. The driver therefore gates
//! every call locally where possible (identity, epoch, done-poll before seal) and treats
//! remote rejections as retryable: the state machine re-drives idempotent envelopes, and
//! genuinely terminal failures surface through the cleanup lane instead of being guessed
//! from message text.

// The client surface below is exercised by the migration driver's text lane unit tests
// today; its production callers (text migration statement parsing + ledger routing) land
// with plan 0297 `backfill-pull`. Until then the lib-only build sees no callers.
#![allow(
    dead_code,
    reason = "text backfill wire mirrors await ledger wiring (plan 0297)"
)]

use std::future::Future;
use std::pin::Pin;

use candid::Principal;
use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId};
use gleaph_graph_kernel::federation::TextIndexId;
use gleaph_graph_kernel::index::PhysicalIndexId;

use super::driver::TypedCallFailure;
use super::index::IndexMigrationDriveError;
use gleaph_migration_api::MigrationFailureCode;

// -- Candid-mirror wire shapes (text-canister `backfill` module) ----------------------------

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub(crate) struct TextBackfillScope {
    pub label_id: u16,
    pub property_id: PropertyId,
    pub analyzer_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub(crate) struct RegisterTextBackfillRequest {
    pub text_index_id: TextIndexId,
    pub graph_canister: Principal,
    pub graph_id: GraphId,
    pub index_name_id: IndexNameId,
    pub physical_index_id: PhysicalIndexId,
    pub catalog_epoch: u64,
    pub scope: TextBackfillScope,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub(crate) struct TextBackfillControl {
    pub text_index_id: TextIndexId,
    pub catalog_epoch: u64,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub(crate) struct TextBackfillSealProof {
    pub seal_catalog_epoch: u64,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub(crate) enum TextBackfillPhase {
    Building,
    Sealing { seal_catalog_epoch: u64 },
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub(crate) struct TextBackfillStatus {
    pub registration: RegisterTextBackfillRequest,
    pub phase: TextBackfillPhase,
    pub next_page_sequence: u64,
    pub cursor: Option<Vec<u8>>,
    pub done: bool,
    pub ingested_docs: u64,
}

// -- Client surface --------------------------------------------------------------------------

/// One text backfill call outcome. Remote `String` rejections are preserved verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TextBackfillCallError {
    Transport,
    Decode,
    Rejected(String),
}

impl From<TypedCallFailure> for TextBackfillCallError {
    fn from(failure: TypedCallFailure) -> Self {
        match failure {
            TypedCallFailure::Transport => TextBackfillCallError::Transport,
            TypedCallFailure::Decode => TextBackfillCallError::Decode,
        }
    }
}

/// Single retryability classifier for text backfill errors. Decode failures are terminal
/// (the reply shape is foreign); transport and owner rejections stay retryable because
/// every endpoint is an exact-replay idempotent envelope (see module docs).
pub(crate) fn classify_text_backfill_call(
    error: TextBackfillCallError,
) -> IndexMigrationDriveError {
    match error {
        TextBackfillCallError::Transport => IndexMigrationDriveError::Retryable,
        TextBackfillCallError::Decode => {
            IndexMigrationDriveError::Terminal(MigrationFailureCode::StaleOrMismatchedResponse)
        }
        TextBackfillCallError::Rejected(_) => IndexMigrationDriveError::Retryable,
    }
}

/// Router → text-canister backfill surface used by the migration driver's text lane.
pub(crate) trait TextBackfillClient {
    fn register_text_backfill(
        &self,
        canister: Principal,
        request: RegisterTextBackfillRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TextBackfillStatus, TextBackfillCallError>> + '_>>;

    fn advance_text_backfill(
        &self,
        canister: Principal,
        control: TextBackfillControl,
        budget: u32,
    ) -> Pin<Box<dyn Future<Output = Result<TextBackfillStatus, TextBackfillCallError>> + '_>>;

    fn seal_text_backfill(
        &self,
        canister: Principal,
        control: TextBackfillControl,
        proof: TextBackfillSealProof,
    ) -> Pin<Box<dyn Future<Output = Result<TextBackfillStatus, TextBackfillCallError>> + '_>>;

    fn abort_text_backfill(
        &self,
        canister: Principal,
        control: TextBackfillControl,
    ) -> Pin<Box<dyn Future<Output = Result<TextBackfillStatus, TextBackfillCallError>> + '_>>;

    fn text_backfill_status(
        &self,
        canister: Principal,
    ) -> Pin<Box<dyn Future<Output = Result<Option<TextBackfillStatus>, TextBackfillCallError>> + '_>>;
}

/// Production text backfill client. Canister-only: native builds fail closed with the
/// transport classification so the Router can never observe a fake success.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IcTextBackfillClient;

#[cfg(target_family = "wasm")]
async fn call_text_typed<T, R>(
    canister: Principal,
    method: &str,
    args: T,
) -> Result<Result<R, String>, TypedCallFailure>
where
    T: candid::utils::ArgumentEncoder,
    R: candid::CandidType + serde::de::DeserializeOwned,
{
    use ic_cdk::call::Call;

    let reply: Result<R, String> = Call::bounded_wait(canister, method)
        .with_args(&args)
        .await
        .map_err(|_| TypedCallFailure::Transport)?
        .candid()
        .map_err(|_| TypedCallFailure::Decode)?;
    Ok(reply)
}

#[cfg(not(target_family = "wasm"))]
async fn call_text_typed<T, R>(
    _canister: Principal,
    method: &str,
    _args: T,
) -> Result<Result<R, String>, TypedCallFailure> {
    let _ = method;
    Err(TypedCallFailure::Transport)
}

impl TextBackfillClient for IcTextBackfillClient {
    fn register_text_backfill(
        &self,
        canister: Principal,
        request: RegisterTextBackfillRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TextBackfillStatus, TextBackfillCallError>> + '_>> {
        Box::pin(async move {
            call_text_typed::<_, TextBackfillStatus>(
                canister,
                "admin_register_text_backfill",
                (request,),
            )
            .await?
            .map_err(TextBackfillCallError::Rejected)
        })
    }

    fn advance_text_backfill(
        &self,
        canister: Principal,
        control: TextBackfillControl,
        budget: u32,
    ) -> Pin<Box<dyn Future<Output = Result<TextBackfillStatus, TextBackfillCallError>> + '_>> {
        Box::pin(async move {
            call_text_typed::<_, TextBackfillStatus>(
                canister,
                "admin_advance_text_backfill",
                (control, budget),
            )
            .await?
            .map_err(TextBackfillCallError::Rejected)
        })
    }

    fn seal_text_backfill(
        &self,
        canister: Principal,
        control: TextBackfillControl,
        proof: TextBackfillSealProof,
    ) -> Pin<Box<dyn Future<Output = Result<TextBackfillStatus, TextBackfillCallError>> + '_>> {
        Box::pin(async move {
            call_text_typed::<_, TextBackfillStatus>(
                canister,
                "admin_seal_text_backfill",
                (control, proof),
            )
            .await?
            .map_err(TextBackfillCallError::Rejected)
        })
    }

    fn abort_text_backfill(
        &self,
        canister: Principal,
        control: TextBackfillControl,
    ) -> Pin<Box<dyn Future<Output = Result<TextBackfillStatus, TextBackfillCallError>> + '_>> {
        Box::pin(async move {
            call_text_typed::<_, TextBackfillStatus>(
                canister,
                "admin_abort_text_backfill",
                (control,),
            )
            .await?
            .map_err(TextBackfillCallError::Rejected)
        })
    }

    fn text_backfill_status(
        &self,
        canister: Principal,
    ) -> Pin<Box<dyn Future<Output = Result<Option<TextBackfillStatus>, TextBackfillCallError>> + '_>>
    {
        Box::pin(async move {
            call_text_typed::<_, Option<TextBackfillStatus>>(
                canister,
                "get_text_backfill_status",
                (),
            )
            .await?
            .map_err(TextBackfillCallError::Rejected)
        })
    }
}
