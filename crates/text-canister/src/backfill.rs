//! Text-canister backfill pull worker for `CREATE TEXT INDEX` (ADR 0059 §Text build kind).
//!
//! Mirrors the graph-index worker shape at the text-engine boundary: one durable
//! registration cell (build identity + lifecycle phase) and one resumable cursor cell
//! (stable regions 14/15, see the [`crate::state`] region map). The controller-guarded
//! endpoints in [`crate`] drive:
//!
//! - `admin_register_text_backfill`: fail-closed identity validation BEFORE any effect;
//!   an exact replay returns the durable status, a conflicting identity is rejected.
//! - `admin_advance_text_backfill`: up to `min(budget, MAX_INDEX_BUILD_ADVANCE_PAGES)`
//!   iterations of prepare (read-only envelope from durable state) → fetch one canonical
//!   export page (`CanonicalExportTarget::Text`) → apply. NO stable mutation happens
//!   before a fully decoded successful reply; a lost/failed reply leaves the cursor
//!   untouched, so the next call re-fetches the same page.
//! - `admin_seal_text_backfill` / `admin_abort_text_backfill`: exact lifecycle
//!   transitions; abort cleanup is O(1) because the whole backfill footprint is the two
//!   cells (ingested documents deliberately remain — replay safety comes from doc-key
//!   identity dedupe, never from retained pull state).
//!
//! ## Replay safety
//!
//! Every fact becomes one `TextDoc` keyed by its vertex id; repeated ingestion of the
//! same key resolves through the engine's existing `docid_by_key` delete+insert upsert
//! path ([`crate::state`]). Unlike graph-index, no fingerprint receipts are needed: the
//! durable cursor is the single prepare source, so a reply lost AFTER commit makes the
//! next prepare mint the NEXT page instead of replaying the old one, and a reply lost
//! BEFORE commit retries the identical envelope.
//!
//! ## Page sizing
//!
//! One page carries at most [`state::MAX_DOCS_PER_INGEST`] facts so the whole decoded
//! page fits ONE atomic engine ingest batch (analyze-everything-first preflight); the
//! byte-level page ceiling remains Graph-owned.

use std::borrow::Cow;

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::canonical_export::{
    CanonicalExportError, CanonicalExportPage, CanonicalExportRequest, CanonicalExportTarget,
    CanonicalIndexableFact,
};
use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId};
use gleaph_graph_kernel::federation::TextIndexId;
use gleaph_graph_kernel::index::{
    MAX_INDEX_BUILD_ADVANCE_PAGES, MAX_INDEX_BUILD_CURSOR_BYTES, PhysicalIndexId,
};
use ic_stable_structures::Cell;
use ic_stable_structures::storable::{Bound as SBound, Storable};
use serde::{Deserialize, Serialize};

use crate::TextDoc;
use crate::state::{self, TextStores};

/// Stable region of the backfill registration cell (see the `state` region map).
const TEXT_BACKFILL_REGISTRATION: ic_stable_structures::memory_manager::MemoryId =
    ic_stable_structures::memory_manager::MemoryId::new(14);
/// Stable region of the backfill resumable cursor cell (see the `state` region map).
const TEXT_BACKFILL_CURSOR: ic_stable_structures::memory_manager::MemoryId =
    ic_stable_structures::memory_manager::MemoryId::new(15);

// -- Wire shapes ---------------------------------------------------------------------------

/// Creation-fixed raw-text scope of one text backfill (ADR 0059 §Text build kind).
///
/// Exactly one vertex label, one text property, and the creation-fixed analyzer pipeline
/// pinned by the Router TEXT definition catalog.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextBackfillScope {
    pub label_id: u16,
    pub property_id: PropertyId,
    pub analyzer_id: u32,
}

/// Controller-supplied immutable build identity for one text backfill.
///
/// `physical_index_id` is the Graph canonical-export namespace the Router froze for this
/// build (echoed verbatim in every export request); `text_index_id` is this canister's
/// logical namespace. Both are monotonic and never reused.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RegisterTextBackfillRequest {
    pub text_index_id: TextIndexId,
    /// Home Graph shard resolved once by the Router; every page is fetched from here.
    pub graph_canister: Principal,
    pub graph_id: GraphId,
    pub index_name_id: IndexNameId,
    pub physical_index_id: PhysicalIndexId,
    /// Registration (old-epoch) value echoed by every admitted pull.
    pub catalog_epoch: u64,
    pub scope: TextBackfillScope,
}

/// Recurring control envelope binding advance/seal/abort calls to the registered identity.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextBackfillControl {
    pub text_index_id: TextIndexId,
    pub catalog_epoch: u64,
}

/// Router seal proof captured at the seal fence.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextBackfillSealProof {
    /// Fresh Router catalog epoch; must strictly advance the registration epoch.
    pub seal_catalog_epoch: u64,
}

/// Backfill lifecycle. `Active`/`Ready` are deliberately absent: readiness is Router
/// catalog state published after the seal, mirroring the property-index split. There is
/// no `Aborting` intermediate because the entire abortable footprint is the cursor cell,
/// cleared atomically with the phase flip (O(1) cleanup, see module docs).
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextBackfillPhase {
    Building,
    Sealing { seal_catalog_epoch: u64 },
    Aborted,
}

/// Public durable status served to the Router convergence poll.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TextBackfillStatus {
    pub registration: RegisterTextBackfillRequest,
    pub phase: TextBackfillPhase,
    /// Sequence the NEXT fetched page must carry (1-based; 0 before/after any pull state
    /// exists, i.e. before the first page and after abort cleanup).
    pub next_page_sequence: u64,
    /// Opaque Graph continuation for the next fetch (cleared on completion and abort).
    pub cursor: Option<Vec<u8>>,
    /// True when the base scan pulled its terminal page.
    pub done: bool,
    /// Raw-text documents enqueued for indexing across all applied pages.
    pub ingested_docs: u64,
}

// -- Durable records -----------------------------------------------------------------------

/// Registration cell value (region 14): frozen request identity, lifecycle phase, and the
/// lifetime ingest counter (kept here so abort cleanup cannot erase build evidence).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct BackfillRegistration {
    request: RegisterTextBackfillRequest,
    phase: TextBackfillPhase,
    ingested_docs: u64,
}

impl Storable for BackfillRegistration {
    const BOUND: SBound = SBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode backfill registration"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode backfill registration")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), BackfillRegistration).expect("decode backfill registration")
    }
}

/// Cursor cell value (region 15): the resumable pull position. Absent (cell `None`)
/// exactly when no pull state exists — before the first page or after abort cleanup.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct BackfillCursor {
    next_page_sequence: u64,
    cursor: Option<Vec<u8>>,
    done: bool,
}

impl Storable for BackfillCursor {
    const BOUND: SBound = SBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode backfill cursor"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode backfill cursor")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), BackfillCursor).expect("decode backfill cursor")
    }
}

/// The two stable cells of the backfill worker, generic over the memory backend so unit
/// tests bind fresh `VectorMemory` regions while production binds regions 14/15.
pub(crate) struct BackfillCells<M: ic_stable_structures::Memory> {
    registration: Cell<Option<BackfillRegistration>, M>,
    cursor: Cell<Option<BackfillCursor>, M>,
}

impl BackfillCells<state::Memory> {
    /// Binds the production cells through the shared `MemoryManager`.
    pub(crate) fn production() -> Self {
        Self::init(
            state::region(TEXT_BACKFILL_REGISTRATION),
            state::region(TEXT_BACKFILL_CURSOR),
        )
    }
}

impl<M: ic_stable_structures::Memory> BackfillCells<M> {
    pub(crate) fn init(registration_region: M, cursor_region: M) -> Self {
        Self {
            registration: Cell::init(registration_region, None),
            cursor: Cell::init(cursor_region, None),
        }
    }

    fn registration(&self) -> Option<BackfillRegistration> {
        self.registration.get().clone()
    }

    fn cursor(&self) -> Option<BackfillCursor> {
        self.cursor.get().clone()
    }
}

thread_local! {
    static CELLS: std::cell::RefCell<Option<BackfillCells<state::Memory>>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs `f` against the lazily-opened production backfill cells (per-process/thread
/// binding, mirroring [`state::with_stores`]).
pub(crate) fn with_cells<R>(f: impl FnOnce(&mut BackfillCells<state::Memory>) -> R) -> R {
    CELLS.with(|slot| {
        let mut slot = slot.borrow_mut();
        let cells = slot.get_or_insert_with(BackfillCells::production);
        f(cells)
    })
}

// -- Pure validation (fail-closed, before any effect) ---------------------------------------

fn validate_request(request: &RegisterTextBackfillRequest) -> Result<(), String> {
    if request.text_index_id.raw() == 0 {
        return Err("text index id 0 is reserved".to_string());
    }
    if request.physical_index_id.raw() == 0 {
        return Err("physical index id 0 is reserved".to_string());
    }
    if request.graph_id.is_reserved() || request.index_name_id.is_reserved() {
        return Err("reserved graph or index-name identity".to_string());
    }
    if request.graph_canister == Principal::anonymous() {
        return Err("graph canister principal is required".to_string());
    }
    if request.scope.label_id == 0 || request.scope.property_id.raw() == 0 {
        return Err("text scope needs a non-zero label and property id".to_string());
    }
    if request.scope.analyzer_id != crate::analyzer::ANALYZER_ID {
        return Err(format!(
            "unknown analyzer {} (this canister serves analyzer {})",
            request.scope.analyzer_id,
            crate::analyzer::ANALYZER_ID
        ));
    }
    Ok(())
}

/// Binds a recurring call to the registered identity; a mismatched logical id or a stale
/// catalog epoch is rejected before anything is read further.
fn ensure_control(
    registration: &BackfillRegistration,
    control: &TextBackfillControl,
) -> Result<(), String> {
    if registration.request.text_index_id != control.text_index_id {
        return Err(format!(
            "no text backfill registered under text index id {}",
            control.text_index_id
        ));
    }
    if registration.request.catalog_epoch != control.catalog_epoch {
        return Err(format!(
            "stale text backfill control: registered catalog epoch {}, got {}",
            registration.request.catalog_epoch, control.catalog_epoch
        ));
    }
    Ok(())
}

fn status_of(
    registration: &BackfillRegistration,
    cursor: Option<&BackfillCursor>,
) -> TextBackfillStatus {
    let empty = BackfillCursor {
        next_page_sequence: 0,
        cursor: None,
        done: false,
    };
    let cursor = cursor.unwrap_or(&empty);
    TextBackfillStatus {
        registration: registration.request.clone(),
        phase: registration.phase,
        next_page_sequence: cursor.next_page_sequence,
        cursor: cursor.cursor.clone(),
        done: cursor.done,
        ingested_docs: registration.ingested_docs,
    }
}

/// The export page size: one page always fits ONE atomic engine ingest batch.
fn backfill_page_items() -> u32 {
    u32::try_from(state::MAX_DOCS_PER_INGEST).expect("ingest cap fits u32")
}

fn export_request_for(
    registration: &RegisterTextBackfillRequest,
    cursor: Option<Vec<u8>>,
) -> CanonicalExportRequest {
    CanonicalExportRequest {
        graph_id: registration.graph_id,
        index_name_id: registration.index_name_id,
        physical_index_id: registration.physical_index_id,
        catalog_epoch: registration.catalog_epoch,
        target: CanonicalExportTarget::Text {
            label_id: registration.scope.label_id,
            property_id: registration.scope.property_id,
        },
        cursor,
        limit: backfill_page_items(),
    }
}

// -- Lifecycle operations (generic over the memory backend) ---------------------------------

/// Registers one immutable backfill identity. All validation precedes any effect; an
/// exact replay returns the durable status; a conflicting identity is rejected without
/// touching the existing build.
pub(crate) fn register_text_backfill<M: ic_stable_structures::Memory>(
    cells: &mut BackfillCells<M>,
    request: RegisterTextBackfillRequest,
) -> Result<TextBackfillStatus, String> {
    validate_request(&request)?;
    if let Some(existing) = cells.registration() {
        // An aborted identity is terminal evidence: its namespace is never reused, so
        // even an otherwise-exact re-registration fails closed (Router issues fresh,
        // monotonic ids per build attempt).
        if existing.phase == TextBackfillPhase::Aborted {
            return Err(format!(
                "text backfill {} was aborted and its identity can never be reused",
                request.text_index_id
            ));
        }
        if existing.request == request {
            return Ok(status_of(&existing, cells.cursor().as_ref()));
        }
        return Err(format!(
            "text index id {} already registered with a different build identity",
            request.text_index_id
        ));
    }
    cells.registration.set(Some(BackfillRegistration {
        request: request.clone(),
        phase: TextBackfillPhase::Building,
        ingested_docs: 0,
    }));
    cells.cursor.set(Some(BackfillCursor {
        next_page_sequence: 1,
        cursor: None,
        done: false,
    }));
    Ok(TextBackfillStatus {
        registration: request,
        phase: TextBackfillPhase::Building,
        next_page_sequence: 1,
        cursor: None,
        done: false,
        ingested_docs: 0,
    })
}

/// One canonical export call prepared from durable state. Carries no mutable effect; the
/// response callback must pass it to [`apply_text_backfill_pull`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedTextBackfillPull {
    pub graph_canister: Principal,
    pub page_sequence: u64,
    pub expected_cursor: Option<Vec<u8>>,
    pub export: CanonicalExportRequest,
}

/// Prepares the next pull from durable state. `Ok(None)` means the base scan already
/// reached its terminal page.
pub(crate) fn prepare_text_backfill_pull<M: ic_stable_structures::Memory>(
    cells: &BackfillCells<M>,
    control: &TextBackfillControl,
) -> Result<Option<PreparedTextBackfillPull>, String> {
    let registration = cells.registration().ok_or("no text backfill registered")?;
    ensure_control(&registration, control)?;
    if registration.phase != TextBackfillPhase::Building {
        return Err("text backfill is not building".to_string());
    }
    let cursor = cells.cursor().ok_or("text backfill cursor is missing")?;
    if cursor.done {
        return Ok(None);
    }
    Ok(Some(PreparedTextBackfillPull {
        graph_canister: registration.request.graph_canister,
        page_sequence: cursor.next_page_sequence,
        expected_cursor: cursor.cursor.clone(),
        export: export_request_for(&registration.request, cursor.cursor.clone()),
    }))
}

/// Applies one fetched page after revalidating the control, the phase, and the exact
/// prepared envelope against CURRENT durable state.
///
/// Mutation order: the engine ingest first (its own analyze-everything preflight rejects
/// the whole batch before the first append), then the cursor cell. Every failure path —
/// stale sequence, foreign projection, oversized cursor, analysis cap — returns before
/// the first write, leaving the cursor exactly as the prepare saw it.
pub(crate) fn apply_text_backfill_pull<M: ic_stable_structures::Memory>(
    engine: &mut TextStores<M>,
    cells: &mut BackfillCells<M>,
    control: &TextBackfillControl,
    prepared: &PreparedTextBackfillPull,
    page: CanonicalExportPage,
) -> Result<TextBackfillStatus, String> {
    let registration = cells.registration().ok_or("no text backfill registered")?;
    ensure_control(&registration, control)?;
    if registration.phase != TextBackfillPhase::Building {
        return Err("text backfill is not building".to_string());
    }
    let cursor = cells.cursor().ok_or("text backfill cursor is missing")?;
    if cursor.next_page_sequence != prepared.page_sequence
        || cursor.cursor != prepared.expected_cursor
    {
        return Err(format!(
            "stale text backfill pull: durable cursor expects page {}, prepared page {}",
            cursor.next_page_sequence, prepared.page_sequence
        ));
    }
    let expected_export =
        export_request_for(&registration.request, prepared.expected_cursor.clone());
    if expected_export != prepared.export {
        return Err("prepared export envelope no longer matches the registered scope".to_string());
    }
    if !page.done && page.next.is_none() {
        return Err(
            "non-terminal canonical export page carried no continuation cursor".to_string(),
        );
    }
    if page
        .next
        .as_ref()
        .is_some_and(|next| next.len() > MAX_INDEX_BUILD_CURSOR_BYTES)
    {
        return Err(format!(
            "canonical export cursor exceeds {MAX_INDEX_BUILD_CURSOR_BYTES} bytes"
        ));
    }

    // Project EVERY fact before any mutation: a page mixing projections or carrying a
    // foreign property is rejected wholesale.
    let scope_property = registration.request.scope.property_id;
    let mut docs = Vec::with_capacity(page.facts.len());
    for fact in &page.facts {
        match fact {
            CanonicalIndexableFact::VertexText {
                vertex_id,
                property_id,
                raw_value,
            } if *property_id == scope_property => docs.push(TextDoc {
                key: u64::from(*vertex_id),
                text: raw_value.clone(),
            }),
            _ => return Err("canonical export page carried a non-text projection".to_string()),
        }
    }
    // Analyze-all-then-append preflight happens inside the engine; a cap violation
    // rejects the whole batch and leaves both the pending log and the cursor untouched.
    engine.enqueue_ingest(docs)?;

    let next_sequence = cursor
        .next_page_sequence
        .checked_add(1)
        .ok_or("text backfill page sequence exhausted")?;
    let ingested = registration
        .ingested_docs
        .checked_add(page.facts.len() as u64)
        .ok_or("text backfill ingest counter overflow")?;
    let advanced = BackfillCursor {
        next_page_sequence: next_sequence,
        // Terminal pages carry no continuation; drop the opaque bytes entirely.
        cursor: if page.done { None } else { page.next },
        done: page.done,
    };
    cells.registration.set(Some(BackfillRegistration {
        ingested_docs: ingested,
        ..registration
    }));
    cells.cursor.set(Some(advanced));
    let registration = cells.registration().expect("registration just written");
    Ok(status_of(&registration, cells.cursor().as_ref()))
}

/// Applies the Router seal proof. Requires the base scan to be complete and the proof
/// epoch to strictly advance the registration epoch; an identical proof during `Sealing`
/// is an exact replay.
pub(crate) fn seal_text_backfill<M: ic_stable_structures::Memory>(
    cells: &mut BackfillCells<M>,
    control: &TextBackfillControl,
    proof: &TextBackfillSealProof,
) -> Result<TextBackfillStatus, String> {
    let registration = cells.registration().ok_or("no text backfill registered")?;
    ensure_control(&registration, control)?;
    match registration.phase {
        TextBackfillPhase::Building => {
            let cursor = cells.cursor().ok_or("text backfill cursor is missing")?;
            if !cursor.done {
                return Err("text backfill base scan is not converged".to_string());
            }
            if proof.seal_catalog_epoch <= registration.request.catalog_epoch {
                return Err(format!(
                    "seal epoch {} must strictly advance the registration epoch {}",
                    proof.seal_catalog_epoch, registration.request.catalog_epoch
                ));
            }
            let sealed = BackfillRegistration {
                phase: TextBackfillPhase::Sealing {
                    seal_catalog_epoch: proof.seal_catalog_epoch,
                },
                ..registration
            };
            cells.registration.set(Some(sealed));
            let registration = cells.registration().expect("registration just written");
            Ok(status_of(&registration, cells.cursor().as_ref()))
        }
        TextBackfillPhase::Sealing { seal_catalog_epoch }
            if seal_catalog_epoch == proof.seal_catalog_epoch =>
        {
            Ok(status_of(&registration, cells.cursor().as_ref()))
        }
        phase => Err(format!("text backfill phase {phase:?} rejects sealing")),
    }
}

/// Terminal abort: clears the resumable cursor and flips the phase in place. Idempotent;
/// ingested documents remain (doc-key dedupe keeps any later rebuild replay-safe), and
/// the identity is never reusable.
pub(crate) fn abort_text_backfill<M: ic_stable_structures::Memory>(
    cells: &mut BackfillCells<M>,
    control: &TextBackfillControl,
) -> Result<TextBackfillStatus, String> {
    let registration = cells.registration().ok_or("no text backfill registered")?;
    ensure_control(&registration, control)?;
    if registration.phase == TextBackfillPhase::Aborted {
        return Ok(status_of(&registration, cells.cursor().as_ref()));
    }
    cells.cursor.set(None);
    cells.registration.set(Some(BackfillRegistration {
        phase: TextBackfillPhase::Aborted,
        ..registration
    }));
    let registration = cells.registration().expect("registration just written");
    Ok(status_of(&registration, cells.cursor().as_ref()))
}

/// Read-only status for the Router convergence poll.
pub(crate) fn text_backfill_status<M: ic_stable_structures::Memory>(
    cells: &BackfillCells<M>,
) -> Option<TextBackfillStatus> {
    cells
        .registration()
        .map(|registration| status_of(&registration, cells.cursor().as_ref()))
}

// -- Advance loop ---------------------------------------------------------------------------

#[cfg(target_family = "wasm")]
const ADVANCE_INSTRUCTION_RESERVE: u64 = 1_000_000_000;
#[cfg(target_family = "wasm")]
const UPDATE_INSTRUCTION_LIMIT: u64 = 40_000_000_000;

#[inline]
fn near_instruction_limit() -> bool {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::instruction_counter()
            >= UPDATE_INSTRUCTION_LIMIT.saturating_sub(ADVANCE_INSTRUCTION_RESERVE)
    }
    #[cfg(not(target_family = "wasm"))]
    {
        false
    }
}

#[cfg(target_family = "wasm")]
pub(crate) async fn fetch_index_export_page(
    graph_canister: Principal,
    request: CanonicalExportRequest,
) -> Result<Result<CanonicalExportPage, CanonicalExportError>, ()> {
    use ic_cdk::call::Call;

    Call::bounded_wait(graph_canister, "index_export_page")
        .with_arg(&request)
        .await
        .map_err(|_| ())?
        .candid()
        .map_err(|_| ())
}

#[cfg(not(target_family = "wasm"))]
pub(crate) async fn fetch_index_export_page(
    _graph_canister: Principal,
    _request: CanonicalExportRequest,
) -> Result<Result<CanonicalExportPage, CanonicalExportError>, ()> {
    Err(())
}

/// Pulls and atomically applies up to `min(budget, MAX_INDEX_BUILD_ADVANCE_PAGES)` pages.
///
/// `fetch` returns an outer transport outcome wrapping the Graph's compact typed result.
/// Neither the engine nor the backfill cells mutate before a fully decoded successful
/// reply; every iteration re-reads durable state so concurrent exact retries converge.
pub(crate) async fn advance_text_backfill_with<F, Fut>(
    control: TextBackfillControl,
    budget: u32,
    mut fetch: F,
) -> Result<TextBackfillStatus, String>
where
    F: FnMut(Principal, CanonicalExportRequest) -> Fut,
    Fut:
        std::future::Future<Output = Result<Result<CanonicalExportPage, CanonicalExportError>, ()>>,
{
    if budget == 0 {
        return Err("advance budget must be >= 1".to_string());
    }
    let pages = budget.min(MAX_INDEX_BUILD_ADVANCE_PAGES);
    let mut last = with_cells(|cells| text_backfill_status(cells))
        .ok_or_else(|| "no text backfill registered".to_string())?;
    for _ in 0..pages {
        if near_instruction_limit() {
            break;
        }
        // Prepare: read-only envelope minted from durable state; the borrow ends here.
        let Some(prepared) = with_cells(|cells| prepare_text_backfill_pull(cells, &control))?
        else {
            break;
        };
        // Fetch: no cell is borrowed across the await.
        let page = fetch(prepared.graph_canister, prepared.export.clone())
            .await
            .map_err(|()| "transport failure fetching canonical export page".to_string())?
            .map_err(|error| format!("graph rejected the export page: {error}"))?;
        // Apply: re-validates everything against current durable state, then commits
        // engine ingest followed by the cursor advance.
        last = state::with_stores(|engine| {
            with_cells(|cells| apply_text_backfill_pull(engine, cells, &control, &prepared, page))
        })?;
        if last.done {
            break;
        }
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    const TEXT_ID: u32 = 77;
    const PHYSICAL_RAW: u64 = 900_100;
    const EPOCH: u64 = 41;

    fn scope() -> TextBackfillScope {
        TextBackfillScope {
            label_id: 7,
            property_id: PropertyId::from_raw(11),
            analyzer_id: crate::analyzer::ANALYZER_ID,
        }
    }

    fn request(text_index_id: u32) -> RegisterTextBackfillRequest {
        RegisterTextBackfillRequest {
            text_index_id: TextIndexId::new(text_index_id),
            graph_canister: Principal::from_slice(&[9, 9]),
            graph_id: GraphId::from_raw(3),
            index_name_id: IndexNameId::from_raw(5),
            physical_index_id: PhysicalIndexId::new(PHYSICAL_RAW).unwrap(),
            catalog_epoch: EPOCH,
            scope: scope(),
        }
    }

    fn control(epoch: u64) -> TextBackfillControl {
        TextBackfillControl {
            text_index_id: TextIndexId::new(TEXT_ID),
            catalog_epoch: epoch,
        }
    }

    fn cells() -> BackfillCells<VectorMemory> {
        BackfillCells::init(VectorMemory::default(), VectorMemory::default())
    }

    fn engine() -> TextStores<VectorMemory> {
        TextStores::init(state::fresh_vector_memories())
    }

    fn text_fact(vertex_id: u32, raw: &str) -> CanonicalIndexableFact {
        CanonicalIndexableFact::VertexText {
            vertex_id,
            property_id: PropertyId::from_raw(11),
            raw_value: raw.to_owned(),
        }
    }

    fn page(
        facts: Vec<CanonicalIndexableFact>,
        next: Option<Vec<u8>>,
        done: bool,
    ) -> CanonicalExportPage {
        CanonicalExportPage { facts, next, done }
    }

    fn register_ok(cells: &mut BackfillCells<VectorMemory>) -> TextBackfillStatus {
        register_text_backfill(cells, request(TEXT_ID)).expect("valid registration")
    }

    // -- Registration ----------------------------------------------------------------------

    fn invalid_registrations() -> Vec<(RegisterTextBackfillRequest, &'static str)> {
        vec![
            (
                RegisterTextBackfillRequest {
                    text_index_id: TextIndexId::new(0),
                    ..request(TEXT_ID)
                },
                "text index id",
            ),
            (
                RegisterTextBackfillRequest {
                    graph_id: GraphId::from_raw(0),
                    ..request(TEXT_ID)
                },
                "reserved",
            ),
            (
                RegisterTextBackfillRequest {
                    graph_canister: Principal::anonymous(),
                    ..request(TEXT_ID)
                },
                "graph canister",
            ),
            (
                RegisterTextBackfillRequest {
                    scope: TextBackfillScope {
                        label_id: 0,
                        ..scope()
                    },
                    ..request(TEXT_ID)
                },
                "label",
            ),
            (
                RegisterTextBackfillRequest {
                    scope: TextBackfillScope {
                        property_id: PropertyId::from_raw(0),
                        ..scope()
                    },
                    ..request(TEXT_ID)
                },
                "property",
            ),
            (
                RegisterTextBackfillRequest {
                    scope: TextBackfillScope {
                        analyzer_id: crate::analyzer::ANALYZER_ID + 1,
                        ..scope()
                    },
                    ..request(TEXT_ID)
                },
                "analyzer",
            ),
        ]
    }

    #[test]
    fn registration_rejects_invalid_identity_without_any_effect() {
        for (case, label) in invalid_registrations() {
            let mut cells = cells();
            assert!(
                register_text_backfill(&mut cells, case)
                    .err()
                    .unwrap_or_else(|| panic!("{label} case must reject"))
                    .contains(label),
                "{label} rejection must name the cause"
            );
            assert!(
                cells.registration().is_none() && cells.cursor().is_none(),
                "{label}: rejected registration must leave both cells untouched"
            );
        }
    }

    #[test]
    fn conflicting_registration_preserves_the_original_build() {
        let mut cells = cells();
        let original = register_ok(&mut cells);
        let mut conflict = request(TEXT_ID);
        conflict.scope.property_id = PropertyId::from_raw(99);
        assert_eq!(
            register_text_backfill(&mut cells, conflict)
                .expect_err("conflicting identity must reject"),
            "text index id 77 already registered with a different build identity"
        );
        // Exact replay converges; the original survives unchanged.
        assert_eq!(
            register_text_backfill(&mut cells, request(TEXT_ID)).expect("exact replay"),
            original
        );
        assert_eq!(text_backfill_status(&cells), Some(original));
    }

    #[test]
    fn stale_control_is_rejected_on_every_recurring_operation_without_mutation() {
        let mut cells = cells();
        let mut engine = engine();
        register_ok(&mut cells);
        let before = text_backfill_status(&cells).clone().unwrap();

        assert!(
            prepare_text_backfill_pull(&cells, &control(EPOCH + 1))
                .expect_err("stale epoch prepare")
                .starts_with("stale")
        );
        assert!(
            apply_text_backfill_pull(
                &mut engine,
                &mut cells,
                &control(EPOCH + 1),
                &PreparedTextBackfillPull {
                    graph_canister: Principal::from_slice(&[9, 9]),
                    page_sequence: 1,
                    expected_cursor: None,
                    export: export_request_for(&before.registration, None),
                },
                page(vec![text_fact(1, "x")], Some(vec![1]), false),
            )
            .expect_err("stale epoch apply")
            .starts_with("stale")
        );
        assert!(
            seal_text_backfill(
                &mut cells,
                &control(EPOCH + 1),
                &TextBackfillSealProof {
                    seal_catalog_epoch: EPOCH + 2
                }
            )
            .expect_err("stale epoch seal")
            .starts_with("stale")
        );
        assert_eq!(text_backfill_status(&cells).as_ref(), Some(&before));
        assert_eq!(
            engine.get_stats().pending_ops,
            0,
            "no op may reach the pending log"
        );
    }

    // -- Prepare / apply -------------------------------------------------------------------

    #[test]
    fn prepared_envelope_echoes_the_frozen_scope_exactly() {
        let mut cells = cells();
        register_ok(&mut cells);
        let prepared = prepare_text_backfill_pull(&cells, &control(EPOCH))
            .expect("prepare")
            .expect("building scan is not done");
        assert_eq!(prepared.page_sequence, 1);
        assert_eq!(prepared.expected_cursor, None);
        assert_eq!(prepared.graph_canister, request(TEXT_ID).graph_canister);
        assert_eq!(
            prepared.export.target,
            CanonicalExportTarget::Text {
                label_id: 7,
                property_id: PropertyId::from_raw(11),
            }
        );
        assert_eq!(prepared.export.catalog_epoch, EPOCH);
        assert_eq!(prepared.export.limit, backfill_page_items());
    }

    /// A text fact carrying a FOREIGN property id (wrong projection for this scope).
    fn foreign_property_fact(vertex_id: u32, raw: &str) -> CanonicalIndexableFact {
        CanonicalIndexableFact::VertexText {
            vertex_id,
            property_id: PropertyId::from_raw(999),
            raw_value: raw.to_owned(),
        }
    }

    #[test]
    fn apply_rejects_foreign_projection_pages_without_any_mutation() {
        let mut cells = cells();
        let mut engine = engine();
        register_ok(&mut cells);
        let prepared = prepare_text_backfill_pull(&cells, &control(EPOCH))
            .expect("prepare")
            .expect("page");
        let foreign_cases = vec![
            // Posting-build projection must never enter the text engine.
            page(
                vec![CanonicalIndexableFact::Vertex {
                    vertex_id: 1,
                    property_id: PropertyId::from_raw(11),
                    encoded_value: vec![1],
                }],
                Some(vec![1]),
                false,
            ),
            // Right variant, wrong property.
            page(vec![foreign_property_fact(1, "x")], Some(vec![1]), false),
        ];
        for foreign in foreign_cases {
            assert!(
                apply_text_backfill_pull(
                    &mut engine,
                    &mut cells,
                    &control(EPOCH),
                    &prepared,
                    foreign
                )
                .expect_err("foreign projection")
                .contains("non-text projection"),
            );
        }
        let after = text_backfill_status(&cells).unwrap();
        assert_eq!(after.next_page_sequence, 1, "cursor must not advance");
        assert_eq!(after.ingested_docs, 0);
        assert_eq!(
            engine.get_stats().pending_ops,
            0,
            "pending log must stay empty"
        );
    }

    #[test]
    fn apply_commits_ingest_and_cursor_only_after_decoded_success() {
        let mut cells = cells();
        let mut engine = engine();
        register_ok(&mut cells);
        let prepared = prepare_text_backfill_pull(&cells, &control(EPOCH))
            .expect("prepare")
            .expect("page");
        let status = apply_text_backfill_pull(
            &mut engine,
            &mut cells,
            &control(EPOCH),
            &prepared,
            page(
                vec![text_fact(1, "alpha"), text_fact(2, "beta")],
                Some(vec![7]),
                false,
            ),
        )
        .expect("apply");
        assert_eq!(status.next_page_sequence, 2);
        assert_eq!(status.cursor, Some(vec![7]));
        assert!(!status.done);
        assert_eq!(status.ingested_docs, 2);
        assert_eq!(engine.get_stats().pending_ops, 2, "one durable op per fact");

        let prepared = prepare_text_backfill_pull(&cells, &control(EPOCH))
            .expect("prepare")
            .expect("page 2");
        let status = apply_text_backfill_pull(
            &mut engine,
            &mut cells,
            &control(EPOCH),
            &prepared,
            page(vec![text_fact(3, "gamma")], None, true),
        )
        .expect("terminal apply");
        assert!(status.done);
        assert_eq!(status.cursor, None, "terminal page drops the opaque cursor");
        assert_eq!(status.ingested_docs, 3);
        assert_eq!(status.next_page_sequence, 3);
        assert!(
            prepare_text_backfill_pull(&cells, &control(EPOCH))
                .expect("prepare after done")
                .is_none(),
            "a done scan prepares nothing further"
        );
    }

    #[test]
    fn nonterminal_page_without_continuation_is_rejected_before_mutation() {
        let mut cells = cells();
        let mut engine = engine();
        register_ok(&mut cells);
        let prepared = prepare_text_backfill_pull(&cells, &control(EPOCH))
            .expect("prepare")
            .expect("page");
        assert!(
            apply_text_backfill_pull(
                &mut engine,
                &mut cells,
                &control(EPOCH),
                &prepared,
                page(vec![], None, false),
            )
            .expect_err("protocol violation")
            .contains("continuation")
        );
        assert_eq!(text_backfill_status(&cells).unwrap().next_page_sequence, 1);
        assert_eq!(engine.get_stats().pending_ops, 0);
    }

    #[test]
    fn oversized_continuation_cursor_is_rejected_before_mutation() {
        let mut cells = cells();
        let mut engine = engine();
        register_ok(&mut cells);
        let prepared = prepare_text_backfill_pull(&cells, &control(EPOCH))
            .expect("prepare")
            .expect("page");
        let wide = vec![0u8; MAX_INDEX_BUILD_CURSOR_BYTES + 1];
        assert!(
            apply_text_backfill_pull(
                &mut engine,
                &mut cells,
                &control(EPOCH),
                &prepared,
                page(vec![], Some(wide), false),
            )
            .expect_err("oversized cursor")
            .contains("cursor exceeds")
        );
        assert_eq!(text_backfill_status(&cells).unwrap().next_page_sequence, 1);
    }

    #[test]
    fn stale_sequence_apply_is_rejected_after_progress_advanced() {
        let mut cells = cells();
        let mut engine = engine();
        register_ok(&mut cells);
        let prepared = prepare_text_backfill_pull(&cells, &control(EPOCH))
            .expect("prepare")
            .expect("page");
        apply_text_backfill_pull(
            &mut engine,
            &mut cells,
            &control(EPOCH),
            &prepared,
            page(vec![text_fact(1, "alpha")], Some(vec![7]), false),
        )
        .expect("first apply");
        // An ambiguous retry delivering the SAME prepared envelope must not double-ingest.
        assert!(
            apply_text_backfill_pull(
                &mut engine,
                &mut cells,
                &control(EPOCH),
                &prepared,
                page(vec![text_fact(1, "alpha")], Some(vec![7]), false),
            )
            .expect_err("stale sequence replay")
            .starts_with("stale")
        );
        assert_eq!(text_backfill_status(&cells).unwrap().ingested_docs, 1);
    }

    // -- Seal / abort ----------------------------------------------------------------------

    #[test]
    fn seal_requires_a_converged_scan_and_a_strictly_advancing_epoch() {
        let mut cells = cells();
        register_ok(&mut cells);
        assert_eq!(
            seal_text_backfill(
                &mut cells,
                &control(EPOCH),
                &TextBackfillSealProof {
                    seal_catalog_epoch: EPOCH + 1
                }
            )
            .expect_err("sealing an unconverged scan"),
            "text backfill base scan is not converged"
        );
        // Converge the scan.
        let mut engine = engine();
        let prepared = prepare_text_backfill_pull(&cells, &control(EPOCH))
            .expect("prepare")
            .expect("page");
        apply_text_backfill_pull(
            &mut engine,
            &mut cells,
            &control(EPOCH),
            &prepared,
            page(vec![], None, true),
        )
        .expect("terminal apply");

        assert!(
            seal_text_backfill(
                &mut cells,
                &control(EPOCH),
                &TextBackfillSealProof {
                    seal_catalog_epoch: EPOCH
                }
            )
            .expect_err("non-advancing epoch")
            .contains("strictly advance")
        );
        let sealed = seal_text_backfill(
            &mut cells,
            &control(EPOCH),
            &TextBackfillSealProof {
                seal_catalog_epoch: EPOCH + 1,
            },
        )
        .expect("valid seal");
        assert_eq!(
            sealed.phase,
            TextBackfillPhase::Sealing {
                seal_catalog_epoch: EPOCH + 1
            }
        );
        // Exact replay is idempotent; a different proof epoch fails closed.
        assert_eq!(
            seal_text_backfill(
                &mut cells,
                &control(EPOCH),
                &TextBackfillSealProof {
                    seal_catalog_epoch: EPOCH + 1
                }
            )
            .expect("exact seal replay")
            .phase,
            sealed.phase
        );
        assert!(
            seal_text_backfill(
                &mut cells,
                &control(EPOCH),
                &TextBackfillSealProof {
                    seal_catalog_epoch: EPOCH + 2
                }
            )
            .expect_err("mismatched replay proof")
            .contains("rejects sealing")
        );
        // Advance stops at the fence.
        assert!(
            prepare_text_backfill_pull(&cells, &control(EPOCH))
                .expect_err("preparing under sealing")
                .contains("not building")
        );
    }

    #[test]
    fn abort_is_exact_terminal_and_never_reusable() {
        let mut cells = cells();
        let mut engine = engine();
        register_ok(&mut cells);
        let prepared = prepare_text_backfill_pull(&cells, &control(EPOCH))
            .expect("prepare")
            .expect("page");
        apply_text_backfill_pull(
            &mut engine,
            &mut cells,
            &control(EPOCH),
            &prepared,
            page(vec![text_fact(1, "alpha")], Some(vec![7]), false),
        )
        .expect("apply");
        let before = text_backfill_status(&cells).unwrap();

        let aborted = abort_text_backfill(&mut cells, &control(EPOCH)).expect("abort");
        assert_eq!(aborted.phase, TextBackfillPhase::Aborted);
        assert_eq!(aborted.cursor, None, "pull state must be cleared");
        assert_eq!(aborted.ingested_docs, 1, "build evidence survives cleanup");
        assert_eq!(
            abort_text_backfill(&mut cells, &control(EPOCH)).expect("idempotent abort"),
            aborted
        );
        assert_eq!(text_backfill_status(&cells), Some(aborted.clone()));

        assert!(prepare_text_backfill_pull(&cells, &control(EPOCH)).is_err());
        assert!(
            seal_text_backfill(
                &mut cells,
                &control(EPOCH),
                &TextBackfillSealProof {
                    seal_catalog_epoch: EPOCH + 1
                }
            )
            .is_err()
        );
        assert!(
            register_text_backfill(&mut cells, request(TEXT_ID))
                .expect_err("aborted identities are never reused")
                .contains("never be reused")
        );
        assert_eq!(text_backfill_status(&cells), Some(aborted));
        assert_eq!(before.ingested_docs, 1);
    }

    // -- Full loop over the production bindings --------------------------------------------

    fn page_reply(
        facts: Vec<CanonicalIndexableFact>,
        next: Option<Vec<u8>>,
        done: bool,
    ) -> Result<Result<CanonicalExportPage, CanonicalExportError>, ()> {
        Ok(Ok(page(facts, next, done)))
    }

    #[test]
    fn budget_bounds_pages_per_call_and_zero_is_rejected() {
        futures_block_on(async {
            let control = control(EPOCH);
            with_cells(|cells| {
                register_text_backfill(cells, request(TEXT_ID)).expect("register");
            });
            assert!(
                super::advance_text_backfill_with(control, 0, unreachable_fetch)
                    .await
                    .expect_err("zero budget")
                    .contains("budget")
            );
            let served = std::cell::RefCell::new(Vec::new());
            let status = super::advance_text_backfill_with(control, 1, |_principal, request| {
                let cursor = request.cursor.clone();
                std::future::ready({
                    served.borrow_mut().push(cursor);
                    page_reply(vec![text_fact(1, "a")], Some(vec![1]), false)
                })
            })
            .await
            .expect("one-page advance");
            assert_eq!(
                served.into_inner().len(),
                1,
                "budget=1 pulls exactly one page"
            );
            assert_eq!(status.next_page_sequence, 2);
            assert!(!status.done);
        });
    }

    #[test]
    fn lost_reply_keeps_the_cursor_and_the_next_call_resumes_exactly() {
        futures_block_on(async {
            let control = control(EPOCH);
            with_cells(|cells| {
                register_text_backfill(cells, request(TEXT_ID)).expect("register");
            });
            let before = with_cells(|cells| text_backfill_status(cells)).unwrap();
            let failed: Result<Result<CanonicalExportPage, CanonicalExportError>, ()> = Err(());
            assert!(
                super::advance_text_backfill_with(control, 4, |_p, _r| {
                    std::future::ready(failed.clone())
                })
                .await
                .expect_err("transport loss")
                .contains("transport")
            );
            let after_failure = with_cells(|cells| text_backfill_status(cells)).unwrap();
            assert_eq!(after_failure, before, "cursor unchanged on a lost reply");

            // Resume: the SAME page sequence is prepared again and succeeds.
            let requested = std::cell::RefCell::new(Vec::new());
            let expected_canister = request(TEXT_ID).graph_canister;
            let status =
                super::advance_text_backfill_with(control, 4, |principal, export_request| {
                    assert_eq!(principal, expected_canister);
                    let record = (export_request.cursor.clone(), export_request.limit);
                    std::future::ready({
                        requested.borrow_mut().push(record);
                        page_reply(vec![text_fact(1, "doc")], None, true)
                    })
                })
                .await
                .expect("resumed advance");
            assert_eq!(
                requested.into_inner(),
                vec![(None, backfill_page_items())],
                "the resume re-fetches the identical first envelope"
            );
            assert!(status.done);
            assert_eq!(status.ingested_docs, 1);
        });
    }

    #[test]
    fn full_loop_walks_multi_page_scans_to_done_in_sequence_order() {
        futures_block_on(async {
            let control = control(EPOCH);
            with_cells(|cells| {
                register_text_backfill(cells, request(TEXT_ID)).expect("register");
            });
            let mut remaining = std::collections::VecDeque::from([
                page_reply(vec![text_fact(1, "one")], Some(vec![1]), false),
                page_reply(
                    vec![text_fact(2, "two"), text_fact(3, "three")],
                    Some(vec![2]),
                    false,
                ),
                page_reply(vec![], None, true),
            ]);
            let status = super::advance_text_backfill_with(control, 4, |_p, _request| {
                let reply = remaining.pop_front().expect("bounded fake replies");
                std::future::ready(reply)
            })
            .await
            .expect("multi-page advance");
            assert!(status.done);
            assert_eq!(status.ingested_docs, 3);
            assert_eq!(status.next_page_sequence, 4);
        });
    }

    #[test]
    fn replayed_doc_keys_dedupe_through_the_engine_upsert_path() {
        // The replay-safety backstop: even if the same page were enqueued twice, the
        // docid_by_key delete+insert upsert collapses duplicate keys into one live doc.
        let mut engine = engine();
        engine
            .enqueue_ingest(vec![TextDoc {
                key: 5,
                text: "hello world".to_owned(),
            }])
            .expect("first ingest");
        engine
            .enqueue_ingest(vec![TextDoc {
                key: 5,
                text: "hello world".to_owned(),
            }])
            .expect("replayed ingest");
        while !engine.flush_step(u64::MAX).done {}
        let stats = engine.get_stats();
        assert_eq!(
            stats.ndocs, 1,
            "duplicate keys must collapse to one live doc"
        );
        assert_eq!(
            stats.next_docid, 2,
            "the replay allocated a fresh docid, tombstoning the old one"
        );
    }

    // -- Helpers ---------------------------------------------------------------------------

    fn unreachable_fetch(
        _principal: Principal,
        _request: CanonicalExportRequest,
    ) -> std::future::Ready<Result<Result<CanonicalExportPage, CanonicalExportError>, ()>> {
        panic!("zero-budget advance must never fetch")
    }

    /// Minimal blocking executor for the async advance loop in native tests.
    fn futures_block_on<R>(future: impl std::future::Future<Output = R>) -> R {
        // Each test holds its own thread-local production bindings; a lightweight
        // single-threaded poll loop avoids pulling an executor dependency. The polled
        // futures here only await already-ready fetch replies, so a yield suffices.
        let mut future = Box::pin(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Ready(value) => return value,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
