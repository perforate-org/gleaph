//! Graph-owned canonical property-index export (ADR 0059).
//!
//! Router and migration code own lifecycle decisions. This module owns only the immutable
//! physical scope record, the opaque cursor encoding, and the bounded canonical storage walks.

use super::catalog_context;
use crate::facade::stable::CANONICAL_EXPORT_SCOPES;
use crate::facade::stable::derived_index_outbox::DerivedIndexOutboxOp;
use crate::facade::stable::edge_properties::EdgePropertyKey;
use crate::facade::stable::vertex_properties::VertexPropertyKey;
use crate::{
    edge_inline_property_scalar_codec::decode_edge_inline_property_scalar, facade::GraphStore,
    index::lookup::PropertyIndexLookup, property::sortable_index_key,
};
use candid::Principal;
use gleaph_gql::Value;
use gleaph_graph_kernel::{
    canonical_export::{
        CanonicalExportAdmission, CanonicalExportError, CanonicalExportPage, CanonicalExportPhase,
        CanonicalExportRecord, CanonicalExportRequest, CanonicalExportScope, CanonicalExportStatus,
        CanonicalExportTarget, CanonicalIndexableFact, CanonicalInlineProjection,
        CanonicalRecordSource, IndexBuildOutboxDrainProgress, IndexBuildOutboxDrainRequest,
        MAX_CANONICAL_EXPORT_PAGE_BYTES, MAX_CANONICAL_EXPORT_PAGE_ITEMS,
    },
    entry::{EdgeDirectedness, EdgeLabelId, GraphId, IndexNameId, PropertyId},
    index::EdgeIndexDirection,
    index::{IndexBuildSealStatus, PhysicalIndexId},
};
use ic_stable_lara::{VertexId, traits::CsrEdge};

const CURSOR_TARGET_VERTEX: u8 = 0;
const CURSOR_TARGET_EDGE: u8 = 1;
const CURSOR_TARGET_TEXT: u8 = 2;
const CURSOR_POSITION_VERTEX: u8 = 0;
const CURSOR_POSITION_EDGE_SIDECAR: u8 = 1;
const CURSOR_POSITION_EDGE_INLINE: u8 = 2;
const CURSOR_SOURCE_DIRECTED: u8 = 0;
const CURSOR_SOURCE_UNDIRECTED: u8 = 1;

/// Registers one immutable Graph-owned scope. Exact replay is idempotent; a different contract
/// (scope identity OR authorized puller) for an existing physical namespace is rejected before
/// any stable write.
pub fn register_scope(
    physical_index_id: PhysicalIndexId,
    scope: CanonicalExportScope,
    authorized_puller: Principal,
) -> Result<(), CanonicalExportError> {
    if authorized_puller == Principal::anonymous() {
        return Err(CanonicalExportError::InvalidRequest);
    }
    validate_scope(&scope)?;
    CANONICAL_EXPORT_SCOPES.with_borrow_mut(|scopes| {
        if let Some(existing) = scopes.get(physical_index_id) {
            return if existing.scope == scope && existing.authorized_puller == authorized_puller {
                Ok(())
            } else {
                Err(CanonicalExportError::ScopeConflict)
            };
        }
        let record = CanonicalExportRecord {
            epoch: scope.catalog_epoch,
            scope,
            phase: CanonicalExportPhase::Building,
            admitted_through: 0,
            drained_through: 0,
            authorized_puller,
        };
        scopes.insert(physical_index_id, record);
        Ok(())
    })
}

/// Fail-closed export-page admission: the caller must be exactly the frozen scope's bound
/// puller. An unregistered namespace reports `ScopeNotFound` (identical to the data-plane
/// lookup that would follow), so admission state leaks nothing extra.
pub fn authorize_page_pull(
    caller: Principal,
    physical_index_id: PhysicalIndexId,
) -> Result<(), CanonicalExportError> {
    let record = CANONICAL_EXPORT_SCOPES.with_borrow(|scopes| scopes.get(physical_index_id));
    match record {
        None => Err(CanonicalExportError::ScopeNotFound),
        Some(record) => {
            if caller == Principal::anonymous() || caller != record.authorized_puller {
                Err(CanonicalExportError::UnauthorizedPuller)
            } else {
                Ok(())
            }
        }
    }
}

/// Router-authorized seal transition. The logical/physical scope identity is checked exactly;
/// only the epoch and lifecycle counters change. A repeated identical transition (same frozen
/// identity and same lifecycle epoch) is an exact replay and returns the durable status without
/// another write; this includes an already-`Active` scope, so a Router crash between remote
/// activation and local convergence persists resumable instead of terminal.
pub fn seal_scope(
    physical_index_id: PhysicalIndexId,
    expected_scope: CanonicalExportScope,
    new_epoch: u64,
) -> Result<CanonicalExportStatus, CanonicalExportError> {
    validate_scope(&expected_scope)?;
    CANONICAL_EXPORT_SCOPES.with_borrow_mut(|scopes| {
        let Some(mut record) = scopes.get(physical_index_id) else {
            return Err(CanonicalExportError::ScopeNotFound);
        };
        if !same_scope_identity(&record.scope, &expected_scope)
            || record.scope.catalog_epoch != expected_scope.catalog_epoch
        {
            return Err(CanonicalExportError::ScopeMismatch);
        }
        if record.epoch == new_epoch
            && matches!(
                record.phase,
                CanonicalExportPhase::Sealing | CanonicalExportPhase::Active
            )
        {
            return Ok(status_from_record(physical_index_id, record));
        }
        if !matches!(record.phase, CanonicalExportPhase::Building) || new_epoch <= record.epoch {
            return Err(CanonicalExportError::InvalidPhase);
        }
        record.phase = CanonicalExportPhase::Sealing;
        record.epoch = new_epoch;
        scopes.insert(physical_index_id, record.clone());
        Ok(status_from_record(physical_index_id, record))
    })
}

/// Reserves the next one-based DML sequence for the immutable physical namespace.
pub fn reserve_build_dml(
    physical_index_id: PhysicalIndexId,
    expected_epoch: u64,
) -> Result<u64, CanonicalExportError> {
    reserve_build_dml_with_status(physical_index_id, expected_epoch)
        .map(|admission| admission.sequence)
}

/// Pure preflight for a build-DML admission. Callers that admit several physical memberships must
/// run every preflight before reserving any sequence so one stale membership cannot leave another
/// namespace's durable counter advanced.
pub fn validate_build_dml(
    physical_index_id: PhysicalIndexId,
    expected_epoch: u64,
) -> Result<(), CanonicalExportError> {
    CANONICAL_EXPORT_SCOPES
        .with_borrow(|scopes| scopes.get(physical_index_id))
        .ok_or(CanonicalExportError::ScopeNotFound)
        .and_then(|record| {
            ensure_scope_reservable(&record, expected_epoch)?;
            if record.admitted_through == u64::MAX {
                return Err(CanonicalExportError::SequenceOutOfRange);
            }
            Ok(())
        })
}

/// The scope must still be Building under the exact catalog epoch with capacity for the next
/// sequence. Shared by the pure preflight and the reserve commit so the two never disagree about
/// the admission boundary.
fn ensure_scope_reservable(
    record: &CanonicalExportRecord,
    expected_epoch: u64,
) -> Result<(), CanonicalExportError> {
    if record.scope.catalog_epoch != expected_epoch || record.epoch != expected_epoch {
        return Err(CanonicalExportError::ScopeMismatch);
    }
    if !matches!(record.phase, CanonicalExportPhase::Building) {
        return Err(if matches!(record.phase, CanonicalExportPhase::Sealing) {
            CanonicalExportError::RetryableSealing
        } else {
            CanonicalExportError::InvalidPhase
        });
    }
    Ok(())
}

/// Status-bearing variant used by Graph-local maintenance tests and callers that need the
/// admitted watermark in the same message as the reserved sequence.
pub fn reserve_build_dml_with_status(
    physical_index_id: PhysicalIndexId,
    expected_epoch: u64,
) -> Result<CanonicalExportAdmission, CanonicalExportError> {
    CANONICAL_EXPORT_SCOPES.with_borrow_mut(|scopes| {
        let Some(mut record) = scopes.get(physical_index_id) else {
            return Err(CanonicalExportError::ScopeNotFound);
        };
        ensure_scope_reservable(&record, expected_epoch)?;
        let sequence = record
            .admitted_through
            .checked_add(1)
            .ok_or(CanonicalExportError::SequenceOutOfRange)?;
        if sequence == 0 {
            return Err(CanonicalExportError::SequenceOutOfRange);
        }
        record.admitted_through = sequence;
        scopes.insert(physical_index_id, record);
        Ok(CanonicalExportAdmission {
            sequence,
            admitted_through: sequence,
            epoch: expected_epoch,
        })
    })
}

/// Acknowledges one exact contiguous graph-index DML sequence. During `Sealing`, only the old
/// registration epoch is accepted and no sequence beyond the captured seal watermark may drain.
pub fn ack_build_dml(
    physical_index_id: PhysicalIndexId,
    epoch: u64,
    sequence: u64,
) -> Result<(), CanonicalExportError> {
    CANONICAL_EXPORT_SCOPES.with_borrow_mut(|scopes| {
        let Some(mut record) = scopes.get(physical_index_id) else {
            return Err(CanonicalExportError::ScopeNotFound);
        };
        if epoch != record.scope.catalog_epoch {
            return Err(CanonicalExportError::ScopeMismatch);
        }
        if sequence == 0 {
            return Err(CanonicalExportError::SequenceOutOfRange);
        }
        let max_sequence = match record.phase {
            CanonicalExportPhase::Building => record.admitted_through,
            CanonicalExportPhase::Sealing => record.admitted_through,
            CanonicalExportPhase::Active | CanonicalExportPhase::Aborting => {
                return Err(CanonicalExportError::InvalidPhase);
            }
        };
        if sequence > max_sequence {
            return Err(CanonicalExportError::SequenceOutOfRange);
        }
        if sequence <= record.drained_through {
            return Err(CanonicalExportError::SequenceReplay);
        }
        let expected = record
            .drained_through
            .checked_add(1)
            .ok_or(CanonicalExportError::SequenceOutOfRange)?;
        if sequence != expected {
            return Err(CanonicalExportError::SequenceGap);
        }
        record.drained_through = sequence;
        scopes.insert(physical_index_id, record);
        Ok(())
    })
}

/// Returns the exact durable phase, epoch, scope, and sequence watermarks.
pub fn scope_status(
    physical_index_id: PhysicalIndexId,
) -> Result<CanonicalExportStatus, CanonicalExportError> {
    CANONICAL_EXPORT_SCOPES
        .with_borrow(|scopes| scopes.get(physical_index_id))
        .map(|record| status_from_record(physical_index_id, record))
        .ok_or(CanonicalExportError::ScopeNotFound)
}

/// Publishes `Active` only after graph-index proves the base scan complete and this Graph's exact
/// captured watermark has drained. The proof is checked before the first stable mutation.
pub fn activate_scope(
    physical_index_id: PhysicalIndexId,
    proof: IndexBuildSealStatus,
) -> Result<CanonicalExportStatus, CanonicalExportError> {
    CANONICAL_EXPORT_SCOPES.with_borrow_mut(|scopes| {
        let Some(mut record) = scopes.get(physical_index_id) else {
            return Err(CanonicalExportError::ScopeNotFound);
        };
        if matches!(record.phase, CanonicalExportPhase::Active) {
            if proof_matches_record(&record, &proof) {
                return Ok(status_from_record(physical_index_id, record));
            }
            return Err(CanonicalExportError::NotConverged);
        }
        if !matches!(record.phase, CanonicalExportPhase::Sealing)
            || !proof_matches_record(&record, &proof)
        {
            return Err(CanonicalExportError::NotConverged);
        }
        record.phase = CanonicalExportPhase::Active;
        scopes.insert(physical_index_id, record.clone());
        Ok(status_from_record(physical_index_id, record))
    })
}

/// Marks a namespace as aborting. The exact immutable scope identity is required; removal is a
/// separate operation so pending admitted work cannot be discarded by one accidental call.
pub fn abort_scope(
    physical_index_id: PhysicalIndexId,
    expected_scope: CanonicalExportScope,
) -> Result<CanonicalExportStatus, CanonicalExportError> {
    validate_scope(&expected_scope)?;
    CANONICAL_EXPORT_SCOPES.with_borrow_mut(|scopes| {
        let Some(mut record) = scopes.get(physical_index_id) else {
            return Err(CanonicalExportError::ScopeNotFound);
        };
        if !same_scope_identity(&record.scope, &expected_scope)
            || record.scope.catalog_epoch != expected_scope.catalog_epoch
        {
            return Err(CanonicalExportError::ScopeMismatch);
        }
        if matches!(record.phase, CanonicalExportPhase::Active) {
            return Err(CanonicalExportError::InvalidPhase);
        }
        if !matches!(record.phase, CanonicalExportPhase::Aborting) {
            record.phase = CanonicalExportPhase::Aborting;
            scopes.insert(physical_index_id, record.clone());
        }
        Ok(status_from_record(physical_index_id, record))
    })
}

/// Bounded drain of one physical namespace's build-DML outbox entries.
///
/// Peers at the Memory46 head, applies each matching `IndexBuildDml` envelope to graph-index via
/// the idempotent `apply_index_build_dml` client, then acknowledges the exact sequence and
/// removes the outbox entry. A transport or ambiguous failure stops the drain and keeps the
/// envelope so the next step can replay it exactly. `converged` requires both an empty build-DML
/// suffix for the namespace and `drained_through == admitted_through` in the scope record.
pub(crate) async fn drain_index_build_outbox(
    ix: &dyn PropertyIndexLookup,
    request: IndexBuildOutboxDrainRequest,
) -> Result<IndexBuildOutboxDrainProgress, CanonicalExportError> {
    if request.max_entries == 0 {
        return Err(CanonicalExportError::InvalidRequest);
    }
    let store = GraphStore::new();
    scope_status(request.physical_index_id)?;
    let limit = usize::try_from(request.max_entries).unwrap_or(usize::MAX);
    let mut drained = 0u32;
    for (seq, entry) in store.derived_index_outbox_peek(limit) {
        let DerivedIndexOutboxOp::IndexBuildDml { request: dml } = &entry.op else {
            continue;
        };
        if dml.physical_index_id != request.physical_index_id {
            continue;
        }
        match ix.apply_index_build_dml(dml.clone()).await {
            Ok(()) => {
                ack_build_dml(dml.physical_index_id, dml.catalog_epoch, dml.shard_sequence)?;
                store.derived_index_outbox_remove(seq);
                drained = drained.saturating_add(1);
            }
            // Transport/ambiguous failure: the envelope may or may not have been applied
            // remotely, so keep it for an exact replay on the next drain step.
            Err(_) => break,
        }
    }
    Ok(drain_progress(request.physical_index_id, drained))
}

fn drain_progress(
    physical_index_id: PhysicalIndexId,
    drained: u32,
) -> IndexBuildOutboxDrainProgress {
    let store = GraphStore::new();
    let remaining = store
        .derived_index_outbox_peek(usize::MAX)
        .into_iter()
        .filter(|(_, entry)| {
            matches!(
                &entry.op,
                DerivedIndexOutboxOp::IndexBuildDml { request }
                    if request.physical_index_id == physical_index_id
            )
        })
        .count() as u64;
    let converged = remaining == 0
        && scope_status(physical_index_id)
            .is_ok_and(|status| status.drained_through == status.admitted_through);
    IndexBuildOutboxDrainProgress {
        drained,
        remaining,
        converged,
    }
}

fn same_scope_identity(left: &CanonicalExportScope, right: &CanonicalExportScope) -> bool {
    left.graph_id == right.graph_id
        && left.index_name_id == right.index_name_id
        && left.target == right.target
        && left.inline == right.inline
}

/// Removes one scope only when the caller supplies the complete owner contract.
pub fn remove_scope(
    physical_index_id: PhysicalIndexId,
    scope: &CanonicalExportScope,
) -> Result<(), CanonicalExportError> {
    validate_scope(scope)?;
    CANONICAL_EXPORT_SCOPES.with_borrow_mut(|scopes| {
        let Some(existing) = scopes.get(physical_index_id) else {
            return Err(CanonicalExportError::ScopeNotFound);
        };
        if !same_scope_identity(&existing.scope, scope)
            || existing.scope.catalog_epoch != scope.catalog_epoch
        {
            return Err(CanonicalExportError::ScopeMismatch);
        }
        if matches!(
            existing.phase,
            CanonicalExportPhase::Active | CanonicalExportPhase::Sealing
        ) {
            return Err(CanonicalExportError::InvalidPhase);
        }
        let captured = existing.admitted_through;
        if existing.drained_through != existing.admitted_through
            || existing.drained_through < captured
        {
            return Err(CanonicalExportError::UnsafeRemoval);
        }
        scopes.remove(physical_index_id);
        Ok(())
    })
}

fn status_from_record(
    physical_index_id: PhysicalIndexId,
    record: CanonicalExportRecord,
) -> CanonicalExportStatus {
    CanonicalExportStatus {
        physical_index_id,
        scope: record.scope,
        phase: record.phase,
        epoch: record.epoch,
        admitted_through: record.admitted_through,
        drained_through: record.drained_through,
    }
}

fn proof_matches_record(record: &CanonicalExportRecord, proof: &IndexBuildSealStatus) -> bool {
    if !proof.base_complete
        || proof.seal_catalog_epoch != record.epoch
        || !proof
            .watermarks
            .iter()
            .all(|watermark| watermark.admitted_through == watermark.drained_through)
    {
        return false;
    }
    let captured = record.admitted_through;
    let Some(shard_id) = GraphStore::new()
        .federation_routing()
        .map(|routing| routing.shard_id.raw())
    else {
        return false;
    };
    proof.watermarks.iter().any(|watermark| {
        watermark.shard_id == shard_id
            && watermark.admitted_through == captured
            && watermark.drained_through == captured
    })
}

/// Emits one bounded, deterministic canonical page. `limit` bounds source entries examined, not
/// merely facts emitted; sparse or non-indexable entries still advance the opaque cursor.
pub fn export_page(
    request: CanonicalExportRequest,
) -> Result<CanonicalExportPage, CanonicalExportError> {
    validate_request(&request)?;
    let record = CANONICAL_EXPORT_SCOPES
        .with_borrow(|scopes| scopes.get(request.physical_index_id))
        .ok_or(CanonicalExportError::ScopeNotFound)?;
    if !matches!(record.phase, CanonicalExportPhase::Building) {
        return Err(if matches!(record.phase, CanonicalExportPhase::Sealing) {
            CanonicalExportError::RetryableSealing
        } else {
            CanonicalExportError::InvalidPhase
        });
    }
    let scope = record.scope;
    ensure_request_matches_scope(&request, &scope)?;

    match (&scope.target, scope.inline.as_ref()) {
        (
            CanonicalExportTarget::Vertex {
                label_id,
                property_id,
                record_source,
            },
            None,
        ) => export_vertex_page(&request, *label_id, *property_id, record_source.as_ref()),
        (
            CanonicalExportTarget::Text {
                label_id,
                property_id,
            },
            None,
        ) => export_text_page(&request, *label_id, *property_id),
        (CanonicalExportTarget::Edge { .. }, None) => export_edge_sidecar_page(&request),
        (CanonicalExportTarget::Edge { .. }, Some(inline)) => {
            export_edge_inline_page(&request, inline)
        }
        (CanonicalExportTarget::Vertex { .. }, Some(_)) => Err(CanonicalExportError::InvalidScope),
        (CanonicalExportTarget::Text { .. }, Some(_)) => Err(CanonicalExportError::InvalidScope),
    }
}

#[derive(Default)]
struct PageByteBudget {
    used: usize,
}

impl PageByteBudget {
    fn try_accept(&mut self, encoded_value: &[u8]) -> Result<bool, CanonicalExportError> {
        if encoded_value.len() > MAX_CANONICAL_EXPORT_PAGE_BYTES {
            return Err(CanonicalExportError::FactTooLarge {
                encoded_value_bytes: encoded_value.len() as u64,
            });
        }
        let Some(next) = self.used.checked_add(encoded_value.len()) else {
            return Ok(false);
        };
        if next > MAX_CANONICAL_EXPORT_PAGE_BYTES {
            return Ok(false);
        }
        self.used = next;
        Ok(true)
    }
}

fn cursor_before_vertex_candidate(
    request: &CanonicalExportRequest,
    previous: Option<VertexPropertyKey>,
) -> Option<Vec<u8>> {
    previous
        .map(|key| encode_cursor(request, CursorPosition::Vertex { key }))
        .or_else(|| request.cursor.clone())
}

fn cursor_before_edge_candidate(
    request: &CanonicalExportRequest,
    previous: Option<EdgePropertyKey>,
) -> Option<Vec<u8>> {
    previous
        .map(|key| encode_cursor(request, CursorPosition::EdgeSidecar { key }))
        .or_else(|| request.cursor.clone())
}

fn validate_scope(scope: &CanonicalExportScope) -> Result<(), CanonicalExportError> {
    if scope.graph_id.is_reserved() {
        return Err(CanonicalExportError::InvalidScope);
    }
    if scope.index_name_id.is_reserved() {
        return Err(CanonicalExportError::InvalidScope);
    }
    match (&scope.target, scope.inline.as_ref()) {
        (
            CanonicalExportTarget::Vertex {
                label_id,
                property_id,
                record_source,
            },
            None,
        ) => {
            if *label_id == 0 {
                return Err(CanonicalExportError::InvalidScope);
            }
            ensure_property_id(*property_id)?;
            match record_source {
                None => Ok(()),
                Some(source) => {
                    ensure_property_id(source.ancestor_property_id)?;
                    if source.field_tail.is_empty()
                        || source
                            .field_tail
                            .split('.')
                            .any(|segment| segment.is_empty())
                        || source.ancestor_property_id == *property_id
                    {
                        return Err(CanonicalExportError::InvalidScope);
                    }
                    Ok(())
                }
            }
        }
        (CanonicalExportTarget::Vertex { .. }, Some(_)) => Err(CanonicalExportError::InvalidScope),
        // Raw-text scopes freeze like any other target: exactly one label and one text
        // property, never an inline projection (ADR 0059 §Text build kind).
        (
            CanonicalExportTarget::Text {
                label_id,
                property_id,
            },
            None,
        ) => {
            if *label_id == 0 {
                return Err(CanonicalExportError::InvalidScope);
            }
            ensure_property_id(*property_id)
        }
        (CanonicalExportTarget::Text { .. }, Some(_)) => Err(CanonicalExportError::InvalidScope),
        (
            CanonicalExportTarget::Edge {
                label_id,
                property_id,
                ..
            },
            inline,
        ) => {
            if label_id.raw() == 0 {
                return Err(CanonicalExportError::InvalidScope);
            }
            ensure_property_id(*property_id)?;
            if let Some(inline) = inline {
                validate_inline_projection(inline)?;
            }
            Ok(())
        }
    }
}

fn validate_inline_projection(
    inline: &CanonicalInlineProjection,
) -> Result<(), CanonicalExportError> {
    ensure_property_id(inline.source_property_id)?;
    inline
        .source_profile
        .validate()
        .map_err(|_| CanonicalExportError::InvalidScope)?;
    inline
        .value_profile
        .validate()
        .map_err(|_| CanonicalExportError::InvalidScope)?;
    let end = inline
        .byte_offset
        .checked_add(inline.value_profile.required_byte_width())
        .ok_or(CanonicalExportError::InvalidScope)?;
    if end > inline.source_profile.required_byte_width() {
        return Err(CanonicalExportError::InvalidScope);
    }
    let zero = vec![0; usize::from(inline.value_profile.required_byte_width())];
    decode_edge_inline_property_scalar(&inline.value_profile, &zero)
        .map_err(|_| CanonicalExportError::UnsupportedInlineProfile)?;
    Ok(())
}

fn ensure_property_id(property_id: PropertyId) -> Result<(), CanonicalExportError> {
    if property_id.raw() == 0 {
        Err(CanonicalExportError::InvalidScope)
    } else {
        Ok(())
    }
}

fn validate_request(request: &CanonicalExportRequest) -> Result<(), CanonicalExportError> {
    if request.graph_id.is_reserved() {
        return Err(CanonicalExportError::InvalidRequest);
    }
    if request.index_name_id.is_reserved() {
        return Err(CanonicalExportError::InvalidRequest);
    }
    if request.limit == 0 || request.limit > MAX_CANONICAL_EXPORT_PAGE_ITEMS {
        return Err(CanonicalExportError::InvalidRequest);
    }
    validate_target(&request.target)
}

fn validate_target(target: &CanonicalExportTarget) -> Result<(), CanonicalExportError> {
    match target {
        CanonicalExportTarget::Vertex {
            label_id,
            property_id,
            record_source,
        } => {
            if *label_id == 0 {
                return Err(CanonicalExportError::InvalidRequest);
            }
            ensure_property_id(*property_id)?;
            match record_source {
                None => Ok(()),
                Some(source) => {
                    ensure_property_id(source.ancestor_property_id)?;
                    if source.field_tail.is_empty() {
                        return Err(CanonicalExportError::InvalidRequest);
                    }
                    Ok(())
                }
            }
        }
        CanonicalExportTarget::Edge {
            label_id,
            property_id,
            ..
        } => {
            if label_id.raw() == 0 {
                return Err(CanonicalExportError::InvalidRequest);
            }
            ensure_property_id(*property_id)?;
            Ok(())
        }
        // Raw-text requests carry the same label/property identity rules as the other
        // targets; the raw projection itself is chosen by the scope's target variant
        // (ADR 0059 §Text build kind).
        CanonicalExportTarget::Text {
            label_id,
            property_id,
        } => {
            if *label_id == 0 {
                return Err(CanonicalExportError::InvalidRequest);
            }
            ensure_property_id(*property_id)
        }
    }
}

fn ensure_request_matches_scope(
    request: &CanonicalExportRequest,
    scope: &CanonicalExportScope,
) -> Result<(), CanonicalExportError> {
    if request.graph_id != scope.graph_id {
        return Err(CanonicalExportError::ScopeMismatch);
    }
    if request.index_name_id != scope.index_name_id {
        return Err(CanonicalExportError::ScopeMismatch);
    }
    if request.catalog_epoch != scope.catalog_epoch {
        return Err(CanonicalExportError::ScopeMismatch);
    }
    if request.target != scope.target {
        return Err(CanonicalExportError::ScopeMismatch);
    }
    Ok(())
}

/// Emits one vertex page. A flat target scans the indexed property's own rows; a nested
/// record target scans the ancestor record rows and walks each value along `record_source`
/// (ADR 0073 §3), emitting leaf facts keyed by the Router-interned leaf identity.
fn export_vertex_page(
    request: &CanonicalExportRequest,
    label_id: u16,
    property_id: PropertyId,
    record_source: Option<&CanonicalRecordSource>,
) -> Result<CanonicalExportPage, CanonicalExportError> {
    let after = decode_cursor(request.cursor.as_deref(), request, CursorKind::Vertex)?;
    let (scan_property_id, field_tail) = match record_source {
        None => (property_id, None),
        Some(source) => (
            source.ancestor_property_id,
            Some(source.field_tail.as_str()),
        ),
    };
    let rows = GraphStore::new()
        .scan_vertex_properties_batch(after.vertex_key_bytes(), request.limit)
        .map_err(|_| CanonicalExportError::Storage)?;
    let mut facts = Vec::new();
    let mut budget = PageByteBudget::default();
    let mut previous = after.vertex_key();
    for (key, value) in rows.iter() {
        let vertex_id = key.vertex_id();
        let has_label = GraphStore::new()
            .vertex(vertex_id)
            .map(|vertex| {
                GraphStore::new().vertex_has_label(
                    vertex_id,
                    vertex,
                    gleaph_graph_kernel::entry::VertexLabelId::from_raw(label_id),
                )
            })
            .unwrap_or(false);
        if key.property_id() != scan_property_id || !has_label {
            previous = Some(*key);
            continue;
        }
        let leaf_value = match field_tail {
            None => Some(value),
            Some(tail) => crate::property::record_value_at_dotted_path(value, tail)
                .and_then(crate::property::nested_leaf_posting_value),
        };
        if let Some(leaf_value) = leaf_value
            && let Some(encoded_value) = sortable_index_key(leaf_value)
        {
            if !budget.try_accept(&encoded_value)? {
                return Ok(CanonicalExportPage {
                    facts,
                    next: cursor_before_vertex_candidate(request, previous),
                    done: false,
                });
            }
            facts.push(CanonicalIndexableFact::Vertex {
                vertex_id: u32::from(key.vertex_id()),
                property_id,
                encoded_value,
            });
        }
        previous = Some(*key);
    }
    if rows.len() < request.limit as usize {
        return Ok(CanonicalExportPage {
            facts,
            next: None,
            done: true,
        });
    }
    let last = rows.last().expect("a full page contains one row").0;
    Ok(CanonicalExportPage {
        facts,
        next: Some(encode_cursor(request, CursorPosition::Vertex { key: last })),
        done: false,
    })
}

/// Emits one raw-text vertex page for a Text build-kind scope (ADR 0059 §Text build kind).
///
/// Reuses the [`export_vertex_page`] paging skeleton: bounded `scan_vertex_properties_batch`
/// in deterministic storage order, and an opaque cursor emitted BEFORE the candidate that
/// overflows the byte budget. The budget counts RAW UTF-8 bytes of the raw value — not
/// sortable-key bytes, which do not exist here: matching facts carry [`CanonicalIndexableFact::VertexText`]
/// values verbatim with no index-key projection. Only vertices whose label AND property match
/// are emitted; stored values that are not `Value::Text` have no raw UTF-8 form and are skipped
/// like other non-indexable values.
fn export_text_page(
    request: &CanonicalExportRequest,
    label_id: u16,
    property_id: PropertyId,
) -> Result<CanonicalExportPage, CanonicalExportError> {
    let after = decode_cursor(request.cursor.as_deref(), request, CursorKind::Vertex)?;
    let rows = GraphStore::new()
        .scan_vertex_properties_batch(after.vertex_key_bytes(), request.limit)
        .map_err(|_| CanonicalExportError::Storage)?;
    let mut facts = Vec::new();
    let mut budget = PageByteBudget::default();
    let mut previous = after.vertex_key();
    for (key, value) in rows.iter() {
        let vertex_id = key.vertex_id();
        let has_label = GraphStore::new()
            .vertex(vertex_id)
            .map(|vertex| {
                GraphStore::new().vertex_has_label(
                    vertex_id,
                    vertex,
                    gleaph_graph_kernel::entry::VertexLabelId::from_raw(label_id),
                )
            })
            .unwrap_or(false);
        if key.property_id() != property_id || !has_label {
            previous = Some(*key);
            continue;
        }
        let Value::Text(raw_value) = value else {
            previous = Some(*key);
            continue;
        };
        if !budget.try_accept(raw_value.as_bytes())? {
            return Ok(CanonicalExportPage {
                facts,
                next: cursor_before_vertex_candidate(request, previous),
                done: false,
            });
        }
        facts.push(CanonicalIndexableFact::VertexText {
            vertex_id: u32::from(key.vertex_id()),
            property_id,
            raw_value: raw_value.clone(),
        });
        previous = Some(*key);
    }
    if rows.len() < request.limit as usize {
        return Ok(CanonicalExportPage {
            facts,
            next: None,
            done: true,
        });
    }
    let last = rows.last().expect("a full page contains one row").0;
    Ok(CanonicalExportPage {
        facts,
        next: Some(encode_cursor(request, CursorPosition::Vertex { key: last })),
        done: false,
    })
}

fn export_edge_sidecar_page(
    request: &CanonicalExportRequest,
) -> Result<CanonicalExportPage, CanonicalExportError> {
    let after = decode_cursor(request.cursor.as_deref(), request, CursorKind::EdgeSidecar)?;
    let rows = GraphStore::new()
        .scan_edge_properties_batch(after.edge_key_bytes(), request.limit)
        .map_err(|_| CanonicalExportError::Storage)?;
    let (target_label, target_property, direction) = match request.target {
        CanonicalExportTarget::Edge {
            label_id,
            property_id,
            direction,
        } => (label_id, property_id, direction),
        CanonicalExportTarget::Vertex { .. } | CanonicalExportTarget::Text { .. } => {
            return Err(CanonicalExportError::InvalidRequest);
        }
    };
    let mut facts = Vec::new();
    let mut budget = PageByteBudget::default();
    let mut previous = after.edge_key();
    for (key, value) in rows.iter() {
        if key.property_id() != target_property
            || !catalog_context::edge_posting_matches_registration(
                key.label_id(),
                target_label.raw(),
                direction,
            )
        {
            previous = Some(*key);
            continue;
        }
        if let Some(encoded_value) = sortable_index_key(value) {
            if !budget.try_accept(&encoded_value)? {
                return Ok(CanonicalExportPage {
                    facts,
                    next: cursor_before_edge_candidate(request, previous),
                    done: false,
                });
            }
            facts.push(CanonicalIndexableFact::Edge {
                owner_vertex_id: u32::from(key.owner_vertex_id()),
                label_id: key.label_id(),
                slot_index: key.slot_index(),
                property_id: target_property,
                encoded_value,
            });
        }
        previous = Some(*key);
    }
    if rows.len() < request.limit as usize {
        return Ok(CanonicalExportPage {
            facts,
            next: None,
            done: true,
        });
    }
    let last = rows.last().expect("a full page contains one row").0;
    Ok(CanonicalExportPage {
        facts,
        next: Some(encode_cursor(
            request,
            CursorPosition::EdgeSidecar { key: last },
        )),
        done: false,
    })
}

fn export_edge_inline_page(
    request: &CanonicalExportRequest,
    inline: &CanonicalInlineProjection,
) -> Result<CanonicalExportPage, CanonicalExportError> {
    let (target_label, target_property, direction) = match request.target {
        CanonicalExportTarget::Edge {
            label_id,
            property_id,
            direction,
        } => (label_id, property_id, direction),
        CanonicalExportTarget::Vertex { .. } | CanonicalExportTarget::Text { .. } => {
            return Err(CanonicalExportError::InvalidRequest);
        }
    };
    let mut position = decode_cursor(request.cursor.as_deref(), request, CursorKind::EdgeInline)?
        .inline_position();
    let store = GraphStore::new();
    let vertex_count = u32::from(store.vertex_count());
    let mut facts = Vec::new();
    let mut budget = PageByteBudget::default();
    let mut examined = 0u32;

    while examined < request.limit {
        let Some((directedness, source_kind)) =
            inline_source_at_or_after(direction, position.source_kind)
        else {
            return Ok(CanonicalExportPage {
                facts,
                next: None,
                done: true,
            });
        };
        position.source_kind = source_kind;
        if position.owner >= vertex_count {
            position = match next_inline_source(direction, source_kind) {
                Some(next_kind) => InlinePosition {
                    source_kind: next_kind,
                    owner: 0,
                    slot: 0,
                },
                None => {
                    return Ok(CanonicalExportPage {
                        facts,
                        next: None,
                        done: true,
                    });
                }
            };
            continue;
        }

        let start_slot = position.slot;
        let remaining = request.limit - examined;
        let mut callback_error = None;
        let mut byte_boundary = None;
        let owner = VertexId::from(position.owner);
        let owner_raw = position.owner;
        let (next_slot, exhausted) = store
            .visit_out_edge_window_from_slot(
                owner,
                target_label,
                directedness,
                start_slot,
                remaining,
                |edge| {
                    if callback_error.is_some() || byte_boundary.is_some() {
                        return;
                    }
                    if directedness == EdgeDirectedness::Undirected
                        && crate::facade::canonical_undirected_owner(owner, edge.neighbor_vid())
                            != owner
                    {
                        return;
                    }
                    if !catalog_context::edge_posting_matches_registration(
                        edge.label_id,
                        target_label.raw(),
                        direction,
                    ) {
                        return;
                    }
                    let bytes = edge.edge_inline_property_bytes();
                    let width = usize::from(inline.source_profile.required_byte_width());
                    if bytes.len() != width {
                        callback_error = Some(CanonicalExportError::Storage);
                        return;
                    }
                    let offset = usize::from(inline.byte_offset);
                    let value_width = usize::from(inline.value_profile.required_byte_width());
                    let Some(end) = offset.checked_add(value_width) else {
                        callback_error = Some(CanonicalExportError::InvalidScope);
                        return;
                    };
                    let Some(value_bytes) = bytes.get(offset..end) else {
                        callback_error = Some(CanonicalExportError::Storage);
                        return;
                    };
                    if let Some(encoded_value) =
                        decode_edge_inline_property_scalar(&inline.value_profile, value_bytes)
                            .ok()
                            .and_then(|value| sortable_index_key(&value))
                    {
                        let slot = edge.edge_slot_index_raw();
                        match budget.try_accept(&encoded_value) {
                            Ok(true) => facts.push(CanonicalIndexableFact::Edge {
                                owner_vertex_id: owner_raw,
                                label_id: edge.label_id,
                                slot_index: slot,
                                property_id: target_property,
                                encoded_value,
                            }),
                            Ok(false) => {
                                byte_boundary = Some(InlinePosition {
                                    source_kind,
                                    owner: owner_raw,
                                    slot,
                                });
                            }
                            Err(error) => callback_error = Some(error),
                        }
                    }
                },
            )
            .map_err(|_| CanonicalExportError::Storage)?;
        if let Some(error) = callback_error {
            return Err(error);
        }
        if let Some(candidate) = byte_boundary {
            return Ok(CanonicalExportPage {
                facts,
                next: Some(encode_cursor(
                    request,
                    CursorPosition::EdgeInline {
                        position: candidate,
                    },
                )),
                done: false,
            });
        }
        let consumed = next_slot.saturating_sub(start_slot);
        // A missing bucket (or an empty/exhausted bucket) still costs one bounded owner probe.
        // Charging that probe prevents a sparse graph from consuming an entire message while
        // advancing across vertices that have no matching label.
        examined = examined.saturating_add(if consumed == 0 && exhausted {
            1
        } else {
            consumed
        });
        position.slot = next_slot;
        if exhausted {
            position = match next_inline_owner_or_source(direction, source_kind, position.owner) {
                Some((next_kind, owner)) => InlinePosition {
                    source_kind: next_kind,
                    owner,
                    slot: 0,
                },
                None => {
                    return Ok(CanonicalExportPage {
                        facts,
                        next: None,
                        done: true,
                    });
                }
            };
        }
        if consumed == 0 && !exhausted {
            return Err(CanonicalExportError::Storage);
        }
    }

    Ok(CanonicalExportPage {
        facts,
        next: Some(encode_cursor(
            request,
            CursorPosition::EdgeInline { position },
        )),
        done: false,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorKind {
    Vertex,
    EdgeSidecar,
    EdgeInline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InlinePosition {
    source_kind: u8,
    owner: u32,
    slot: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorPosition {
    Start,
    Vertex { key: VertexPropertyKey },
    EdgeSidecar { key: EdgePropertyKey },
    EdgeInline { position: InlinePosition },
}

impl CursorPosition {
    fn vertex_key(self) -> Option<VertexPropertyKey> {
        match self {
            Self::Vertex { key } => Some(key),
            _ => None,
        }
    }

    fn edge_key(self) -> Option<EdgePropertyKey> {
        match self {
            Self::EdgeSidecar { key } => Some(key),
            _ => None,
        }
    }

    fn vertex_key_bytes(self) -> Option<Vec<u8>> {
        self.vertex_key().map(GraphStore::vertex_property_cursor)
    }

    fn edge_key_bytes(self) -> Option<Vec<u8>> {
        self.edge_key().map(GraphStore::edge_property_cursor)
    }

    fn inline_position(self) -> InlinePosition {
        match self {
            Self::EdgeInline { position } => position,
            _ => InlinePosition {
                source_kind: CURSOR_SOURCE_DIRECTED,
                owner: 0,
                slot: 0,
            },
        }
    }
}

fn encode_cursor(request: &CanonicalExportRequest, position: CursorPosition) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.push(gleaph_graph_kernel::canonical_export::CANONICAL_EXPORT_CURSOR_VERSION);
    out.extend_from_slice(&request.graph_id.raw().to_le_bytes());
    out.extend_from_slice(&request.index_name_id.raw().to_le_bytes());
    out.extend_from_slice(&request.physical_index_id.raw().to_le_bytes());
    out.extend_from_slice(&request.catalog_epoch.to_le_bytes());
    encode_target(&mut out, &request.target);
    match position {
        CursorPosition::Start => unreachable!("a start cursor is never persisted"),
        CursorPosition::Vertex { key } => {
            out.push(CURSOR_POSITION_VERTEX);
            out.extend_from_slice(&u32::from(key.vertex_id()).to_le_bytes());
            out.extend_from_slice(&key.property_id().raw().to_le_bytes());
        }
        CursorPosition::EdgeSidecar { key } => {
            out.push(CURSOR_POSITION_EDGE_SIDECAR);
            out.extend_from_slice(&u32::from(key.owner_vertex_id()).to_le_bytes());
            out.extend_from_slice(&key.label_id().to_le_bytes());
            out.extend_from_slice(&key.slot_index().to_le_bytes());
            out.extend_from_slice(&key.property_id().raw().to_le_bytes());
        }
        CursorPosition::EdgeInline { position } => {
            out.push(CURSOR_POSITION_EDGE_INLINE);
            out.push(position.source_kind);
            out.extend_from_slice(&position.owner.to_le_bytes());
            out.extend_from_slice(&position.slot.to_le_bytes());
        }
    }
    out
}

fn encode_target(out: &mut Vec<u8>, target: &CanonicalExportTarget) {
    match target {
        CanonicalExportTarget::Vertex {
            label_id,
            property_id,
            record_source,
        } => {
            out.push(CURSOR_TARGET_VERTEX);
            out.extend_from_slice(&label_id.to_le_bytes());
            out.extend_from_slice(&property_id.raw().to_le_bytes());
            match record_source {
                None => out.push(0),
                Some(source) => {
                    out.push(1);
                    out.extend_from_slice(&source.ancestor_property_id.raw().to_le_bytes());
                    let tail = source.field_tail.as_bytes();
                    out.extend_from_slice(&(tail.len() as u16).to_le_bytes());
                    out.extend_from_slice(tail);
                }
            }
        }
        CanonicalExportTarget::Edge {
            label_id,
            property_id,
            direction,
        } => {
            out.push(CURSOR_TARGET_EDGE);
            out.extend_from_slice(&label_id.raw().to_le_bytes());
            out.extend_from_slice(&property_id.raw().to_le_bytes());
            out.push(*direction as u8);
        }
        CanonicalExportTarget::Text {
            label_id,
            property_id,
        } => {
            out.push(CURSOR_TARGET_TEXT);
            out.extend_from_slice(&label_id.to_le_bytes());
            out.extend_from_slice(&property_id.raw().to_le_bytes());
        }
    }
}

fn decode_cursor(
    bytes: Option<&[u8]>,
    request: &CanonicalExportRequest,
    expected_kind: CursorKind,
) -> Result<CursorPosition, CanonicalExportError> {
    let Some(bytes) = bytes else {
        return Ok(CursorPosition::Start);
    };
    let mut reader = CursorReader::new(bytes);
    if reader.byte()? != gleaph_graph_kernel::canonical_export::CANONICAL_EXPORT_CURSOR_VERSION {
        return Err(CanonicalExportError::CursorMalformed);
    }
    let graph_id = GraphId::from_raw(reader.u32()?);
    let index_name_id = IndexNameId::from_raw(reader.u16()?);
    let physical = PhysicalIndexId::from_le_bytes(reader.array::<8>()?)
        .ok_or(CanonicalExportError::CursorMalformed)?;
    let epoch = reader.u64()?;
    let target = decode_target(&mut reader)?;
    if graph_id != request.graph_id
        || index_name_id != request.index_name_id
        || physical != request.physical_index_id
        || epoch != request.catalog_epoch
        || target != request.target
    {
        return Err(CanonicalExportError::ScopeMismatch);
    }
    let position_tag = reader.byte()?;
    let position = match position_tag {
        CURSOR_POSITION_VERTEX if expected_kind == CursorKind::Vertex => {
            let key = VertexPropertyKey::new(
                VertexId::from(reader.u32()?),
                PropertyId::from_raw(reader.u32()?),
            );
            CursorPosition::Vertex { key }
        }
        CURSOR_POSITION_EDGE_SIDECAR if expected_kind == CursorKind::EdgeSidecar => {
            let key = EdgePropertyKey::new(
                VertexId::from(reader.u32()?),
                reader.u16()?,
                reader.u32()?,
                PropertyId::from_raw(reader.u32()?),
            );
            CursorPosition::EdgeSidecar { key }
        }
        CURSOR_POSITION_EDGE_INLINE if expected_kind == CursorKind::EdgeInline => {
            let source_kind = reader.byte()?;
            if source_kind > CURSOR_SOURCE_UNDIRECTED {
                return Err(CanonicalExportError::CursorMalformed);
            }
            if !source_kind_allowed(direction_from_target(request), source_kind) {
                return Err(CanonicalExportError::CursorMalformed);
            }
            CursorPosition::EdgeInline {
                position: InlinePosition {
                    source_kind,
                    owner: reader.u32()?,
                    slot: reader.u32()?,
                },
            }
        }
        _ => {
            return Err(CanonicalExportError::CursorMalformed);
        }
    };
    reader.finish()?;
    Ok(position)
}

fn decode_target(
    reader: &mut CursorReader<'_>,
) -> Result<CanonicalExportTarget, CanonicalExportError> {
    match reader.byte()? {
        CURSOR_TARGET_VERTEX => {
            let label_id = reader.u16()?;
            let property_id = PropertyId::from_raw(reader.u32()?);
            let record_source = match reader.byte()? {
                0 => None,
                1 => {
                    let ancestor_property_id = PropertyId::from_raw(reader.u32()?);
                    let tail_len = reader.u16()? as usize;
                    let tail_bytes = reader.bytes(tail_len)?;
                    let field_tail = std::str::from_utf8(tail_bytes)
                        .map_err(|_| CanonicalExportError::CursorMalformed)?
                        .to_owned();
                    Some(CanonicalRecordSource {
                        ancestor_property_id,
                        field_tail,
                    })
                }
                _ => return Err(CanonicalExportError::CursorMalformed),
            };
            Ok(CanonicalExportTarget::Vertex {
                label_id,
                property_id,
                record_source,
            })
        }
        CURSOR_TARGET_EDGE => Ok(CanonicalExportTarget::Edge {
            label_id: EdgeLabelId::from_raw(reader.u16()?),
            property_id: PropertyId::from_raw(reader.u32()?),
            direction: decode_direction(reader.byte()?)?,
        }),
        CURSOR_TARGET_TEXT => Ok(CanonicalExportTarget::Text {
            label_id: reader.u16()?,
            property_id: PropertyId::from_raw(reader.u32()?),
        }),
        _ => Err(CanonicalExportError::CursorMalformed),
    }
}

fn decode_direction(byte: u8) -> Result<EdgeIndexDirection, CanonicalExportError> {
    match byte {
        1 => Ok(EdgeIndexDirection::Outgoing),
        2 => Ok(EdgeIndexDirection::Incoming),
        3 => Ok(EdgeIndexDirection::OutgoingOrIncoming),
        4 => Ok(EdgeIndexDirection::Undirected),
        5 => Ok(EdgeIndexDirection::OutgoingOrUndirected),
        6 => Ok(EdgeIndexDirection::IncomingOrUndirected),
        7 => Ok(EdgeIndexDirection::Any),
        _ => Err(CanonicalExportError::CursorMalformed),
    }
}

struct CursorReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> CursorReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], CanonicalExportError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or(CanonicalExportError::CursorMalformed)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(CanonicalExportError::CursorMalformed)?;
        self.offset = end;
        Ok(slice
            .try_into()
            .expect("cursor reader copied exact fixed width"))
    }

    fn byte(&mut self) -> Result<u8, CanonicalExportError> {
        Ok(self.take::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, CanonicalExportError> {
        Ok(u16::from_le_bytes(self.take::<2>()?))
    }

    fn u32(&mut self) -> Result<u32, CanonicalExportError> {
        Ok(u32::from_le_bytes(self.take::<4>()?))
    }

    fn u64(&mut self) -> Result<u64, CanonicalExportError> {
        Ok(u64::from_le_bytes(self.take::<8>()?))
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], CanonicalExportError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(CanonicalExportError::CursorMalformed)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(CanonicalExportError::CursorMalformed)?;
        self.offset = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CanonicalExportError> {
        self.take::<N>()
    }

    fn finish(self) -> Result<(), CanonicalExportError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(CanonicalExportError::CursorMalformed)
        }
    }
}

fn direction_from_target(request: &CanonicalExportRequest) -> Option<EdgeIndexDirection> {
    match request.target {
        CanonicalExportTarget::Edge { direction, .. } => Some(direction),
        CanonicalExportTarget::Vertex { .. } | CanonicalExportTarget::Text { .. } => None,
    }
}

fn source_kind_allowed(direction: Option<EdgeIndexDirection>, source_kind: u8) -> bool {
    let Some(direction) = direction else {
        return false;
    };
    (source_kind == CURSOR_SOURCE_DIRECTED && direction.includes_directed())
        || (source_kind == CURSOR_SOURCE_UNDIRECTED && direction.includes_undirected())
}

fn inline_source_at_or_after(
    direction: EdgeIndexDirection,
    source_kind: u8,
) -> Option<(EdgeDirectedness, u8)> {
    if source_kind == CURSOR_SOURCE_DIRECTED && direction.includes_directed() {
        Some((EdgeDirectedness::Directed, CURSOR_SOURCE_DIRECTED))
    } else if source_kind <= CURSOR_SOURCE_UNDIRECTED && direction.includes_undirected() {
        Some((EdgeDirectedness::Undirected, CURSOR_SOURCE_UNDIRECTED))
    } else {
        None
    }
}

fn next_inline_source(direction: EdgeIndexDirection, source_kind: u8) -> Option<u8> {
    if source_kind == CURSOR_SOURCE_DIRECTED && direction.includes_undirected() {
        Some(CURSOR_SOURCE_UNDIRECTED)
    } else {
        None
    }
}

fn next_inline_owner_or_source(
    direction: EdgeIndexDirection,
    source_kind: u8,
    owner: u32,
) -> Option<(u8, u32)> {
    owner
        .checked_add(1)
        .map(|next| (source_kind, next))
        .or_else(|| next_inline_source(direction, source_kind).map(|next_kind| (next_kind, 0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use candid::Principal;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::canonical_export::CanonicalExportScope;
    use gleaph_graph_kernel::entry::{
        EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, VertexLabelId,
    };
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::EdgeIndexDirection;
    use gleaph_graph_kernel::index::{
        IndexBuildDmlRequest, IndexMaintenancePhase, IndexedPropertyCatalog,
        IndexedVertexMembership,
    };
    use ic_stable_lara::labeled::LabeledOrientation;
    use std::cell::RefCell;

    use crate::facade::stable::derived_index_outbox::DerivedIndexOutboxOp;
    use crate::facade::{FederationRouting, GraphStoreError};
    use crate::index::lookup::PropertyIndexLookup;

    fn request(target: CanonicalExportTarget) -> CanonicalExportRequest {
        CanonicalExportRequest {
            graph_id: GraphId::from_raw(1),
            index_name_id: IndexNameId::from_raw(1),
            physical_index_id: PhysicalIndexId::new(900_001).unwrap(),
            catalog_epoch: 1,
            target,
            cursor: None,
            limit: 1,
        }
    }

    fn scope(target: CanonicalExportTarget) -> CanonicalExportScope {
        let request = request(target.clone());
        CanonicalExportScope {
            graph_id: request.graph_id,
            index_name_id: request.index_name_id,
            catalog_epoch: request.catalog_epoch,
            target,
            inline: None,
        }
    }

    fn request_with(
        target: CanonicalExportTarget,
        physical_index_id: PhysicalIndexId,
    ) -> CanonicalExportRequest {
        CanonicalExportRequest {
            physical_index_id,
            ..request(target)
        }
    }

    fn drain(mut request: CanonicalExportRequest) -> Vec<CanonicalIndexableFact> {
        let mut facts = Vec::new();
        loop {
            let page = export_page(request.clone()).expect("canonical export page");
            facts.extend(page.facts);
            if page.done {
                break;
            }
            request.cursor = page.next;
            assert!(
                request.cursor.is_some(),
                "non-terminal page must have a cursor"
            );
        }
        facts
    }

    fn text_target() -> CanonicalExportTarget {
        CanonicalExportTarget::Text {
            label_id: 7,
            property_id: PropertyId::from_raw(11),
        }
    }

    #[test]
    fn text_requests_validate_under_shared_identity_rules() {
        assert_eq!(validate_request(&request(text_target())), Ok(()));
        assert_eq!(
            validate_request(&request(CanonicalExportTarget::Text {
                label_id: 0,
                property_id: PropertyId::from_raw(11),
            })),
            Err(CanonicalExportError::InvalidRequest)
        );
        assert_eq!(
            // Zero property ids surface through the shared `ensure_property_id` guard, the
            // same `InvalidScope` mapping every target gets inside request validation.
            validate_request(&request(CanonicalExportTarget::Text {
                label_id: 7,
                property_id: PropertyId::from_raw(0),
            })),
            Err(CanonicalExportError::InvalidScope)
        );
    }

    #[test]
    fn text_scopes_freeze_with_full_identity_and_without_inline() {
        assert_eq!(validate_scope(&scope(text_target())), Ok(()));
        assert_eq!(
            validate_scope(&scope(CanonicalExportTarget::Text {
                label_id: 0,
                property_id: PropertyId::from_raw(11),
            })),
            Err(CanonicalExportError::InvalidScope)
        );
        assert_eq!(
            validate_scope(&scope(CanonicalExportTarget::Text {
                label_id: 7,
                property_id: PropertyId::from_raw(0),
            })),
            Err(CanonicalExportError::InvalidScope)
        );
        let mut with_inline = scope(text_target());
        with_inline.inline = Some(CanonicalInlineProjection {
            source_property_id: PropertyId::from_raw(11),
            byte_offset: 0,
            source_profile: EdgeInlinePropertyProfile {
                byte_width: 4,
                encoding: EdgeInlinePropertyEncoding::F32,
            },
            value_profile: EdgeInlinePropertyProfile {
                byte_width: 4,
                encoding: EdgeInlinePropertyEncoding::F32,
            },
        });
        assert_eq!(
            validate_scope(&with_inline),
            Err(CanonicalExportError::InvalidScope),
            "a text scope never carries an inline projection"
        );
    }

    #[test]
    fn text_cursor_target_roundtrips_losslessly() {
        let target = text_target();
        let mut out = Vec::new();
        encode_target(&mut out, &target);
        let mut reader = CursorReader::new(&out);
        assert_eq!(
            decode_target(&mut reader).expect("decode text cursor target"),
            target
        );
    }

    #[test]
    fn text_targets_carry_no_edge_direction() {
        assert_eq!(direction_from_target(&request(text_target())), None);
    }

    /// Full drain over a mixed fixture: every vertex whose label AND property match emits
    /// exactly one VertexText fact carrying the raw value, in deterministic key order;
    /// a non-matching label, a non-matching property, and a non-text stored value are all
    /// excluded.
    #[test]
    fn text_export_drain_emits_matching_vertices_once_in_deterministic_order() {
        let store = GraphStore::new();
        let label = crate::test_labels::vertex_label_id_for_name("text_export_label");
        let other_label = crate::test_labels::vertex_label_id_for_name("text_export_other_label");
        let property = crate::test_labels::property_id_for_name("text_export_value");
        let unrelated = crate::test_labels::property_id_for_name("text_export_unrelated");

        let first = store.insert_vertex().expect("first vertex");
        let second = store.insert_vertex().expect("second vertex");
        let third = store.insert_vertex().expect("third vertex");
        let fourth = store.insert_vertex().expect("fourth vertex");
        for vertex in [first, second, third] {
            store
                .add_vertex_label(vertex, store.vertex(vertex).expect("row"), label)
                .expect("target label");
        }
        store
            .add_vertex_label(fourth, store.vertex(fourth).expect("row"), other_label)
            .expect("other label");
        store
            .set_vertex_property(first, property, Value::Text("alpha".to_owned()))
            .expect("first raw value");
        store
            .set_vertex_property(second, property, Value::Text("beta".to_owned()))
            .expect("second raw value");
        store
            .set_vertex_property(third, unrelated, Value::Text("gamma".to_owned()))
            .expect("non-matching property");
        store
            .set_vertex_property(third, property, Value::Int64(7))
            .expect("non-text value at the target property");
        store
            .set_vertex_property(fourth, property, Value::Text("delta".to_owned()))
            .expect("non-matching label value");

        let physical = PhysicalIndexId::new(900_030).unwrap();
        let target = CanonicalExportTarget::Text {
            label_id: label.raw(),
            property_id: property,
        };
        register_scope(
            physical,
            scope(target.clone()),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        let mut request = request_with(target.clone(), physical);
        request.limit = 1_000;

        let page = export_page(request).expect("single-page text export");
        assert!(page.done, "the fixture fits one page");
        assert_eq!(page.next, None, "the terminal page carries no cursor");
        assert_eq!(
            page.facts,
            vec![
                CanonicalIndexableFact::VertexText {
                    vertex_id: u32::from(first),
                    property_id: property,
                    raw_value: "alpha".to_owned(),
                },
                CanonicalIndexableFact::VertexText {
                    vertex_id: u32::from(second),
                    property_id: property,
                    raw_value: "beta".to_owned(),
                },
            ],
            "only matching label AND property AND text values emit, in key order"
        );
        remove_scope(physical, &scope(target)).expect("cleanup");
    }

    /// Raw UTF-8 bytes drive the page budget: equal-length documents split exactly at the
    /// budget boundary and the opaque cursor resumes without loss or duplication.
    #[test]
    fn text_export_budget_overflow_splits_pages_without_loss_or_duplicate() {
        let store = GraphStore::new();
        let label = crate::test_labels::vertex_label_id_for_name("text_export_budget_label");
        let property = crate::test_labels::property_id_for_name("text_export_budget_value");
        let unit = MAX_CANONICAL_EXPORT_PAGE_BYTES / 387;
        let document = "x".repeat(unit);

        for _ in 0..388 {
            let vertex = store.insert_vertex().expect("vertex");
            store
                .add_vertex_label(vertex, store.vertex(vertex).expect("row"), label)
                .expect("vertex label");
            store
                .set_vertex_property(vertex, property, Value::Text(document.clone()))
                .expect("large target document");
        }

        let physical = PhysicalIndexId::new(900_031).unwrap();
        let target = CanonicalExportTarget::Text {
            label_id: label.raw(),
            property_id: property,
        };
        register_scope(
            physical,
            scope(target.clone()),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        let mut request = request_with(target.clone(), physical);
        // The limit must not be the splitter: the RAW-byte budget decides the page break.
        request.limit = 1_000;

        let first = export_page(request.clone()).expect("first budget-bounded page");
        assert_eq!(
            first.facts.len(),
            387,
            "raw-value bytes fill exactly 387 documents per page"
        );
        assert!(!first.done);
        let mut emitted: Vec<(u32, String)> = first
            .facts
            .iter()
            .map(|fact| match fact {
                CanonicalIndexableFact::VertexText {
                    vertex_id,
                    raw_value,
                    ..
                } => (*vertex_id, raw_value.clone()),
                _ => panic!("a text page only carries VertexText facts"),
            })
            .collect();
        request.cursor = first.next;

        let second = export_page(request).expect("resumed budget-bounded page");
        assert_eq!(second.facts.len(), 1);
        assert!(second.done, "the resumed page is terminal");
        emitted.extend(second.facts.iter().map(|fact| match fact {
            CanonicalIndexableFact::VertexText {
                vertex_id,
                raw_value,
                ..
            } => (*vertex_id, raw_value.clone()),
            _ => panic!("a text page only carries VertexText facts"),
        }));

        assert_eq!(
            emitted.iter().filter(|(_, raw)| raw == &document).count(),
            388,
            "every emitted fact carries the raw value verbatim"
        );
        emitted.sort_unstable();
        emitted.dedup();
        assert_eq!(
            emitted.len(),
            388,
            "no loss and no duplication across the cursor boundary"
        );
        remove_scope(physical, &scope(target)).expect("cleanup");
    }

    #[test]
    fn cursor_scope_mismatch_is_rejected() {
        let request = request(CanonicalExportTarget::Vertex {
            label_id: 1,
            property_id: PropertyId::from_raw(1),
            record_source: None,
        });
        let cursor = encode_cursor(
            &request,
            CursorPosition::Vertex {
                key: VertexPropertyKey::new(VertexId::from(3), PropertyId::from_raw(1)),
            },
        );
        let mut changed = request.clone();
        changed.graph_id = GraphId::from_raw(2);
        changed.cursor = Some(cursor);
        assert!(matches!(
            decode_cursor(changed.cursor.as_deref(), &changed, CursorKind::Vertex),
            Err(CanonicalExportError::ScopeMismatch)
        ));
    }

    #[test]
    fn canonical_export_cursor_round_trips_exact_position() {
        for record_source in [
            None,
            Some(CanonicalRecordSource {
                ancestor_property_id: PropertyId::from_raw(3),
                field_tail: "meta.deep".to_owned(),
            }),
        ] {
            let request = request(CanonicalExportTarget::Vertex {
                label_id: 1,
                property_id: PropertyId::from_raw(2),
                record_source: record_source.clone(),
            });
            let position = CursorPosition::Vertex {
                key: VertexPropertyKey::new(VertexId::from(7), PropertyId::from_raw(2)),
            };
            let cursor = encode_cursor(&request, position);
            assert_eq!(
                cursor[0],
                gleaph_graph_kernel::canonical_export::CANONICAL_EXPORT_CURSOR_VERSION
            );
            assert_eq!(
                decode_cursor(Some(&cursor), &request, CursorKind::Vertex),
                Ok(position),
                "a cursor resumes at its exact vertex position"
            );
        }
    }

    #[test]
    fn canonical_export_cursor_rejects_foreign_versions_as_malformed() {
        let request = request(CanonicalExportTarget::Edge {
            label_id: EdgeLabelId::from_raw(1),
            property_id: PropertyId::from_raw(2),
            direction: EdgeIndexDirection::Outgoing,
        });
        for foreign_version in [0u8, 2, 3, u8::MAX] {
            let mut cursor = encode_cursor(
                &request,
                CursorPosition::EdgeSidecar {
                    key: EdgePropertyKey::new(VertexId::from(5), 1, 0, PropertyId::from_raw(2)),
                },
            );
            cursor[0] = foreign_version;
            assert_eq!(
                decode_cursor(Some(&cursor), &request, CursorKind::EdgeSidecar),
                Err(CanonicalExportError::CursorMalformed),
                "version byte {foreign_version} is never reinterpreted as another layout"
            );
        }
    }

    #[test]
    fn exact_scope_registration_is_idempotent_and_conflict_is_rejected() {
        let physical = PhysicalIndexId::new(900_002).unwrap();
        let scope = scope(CanonicalExportTarget::Vertex {
            label_id: 1,
            property_id: PropertyId::from_raw(2),
            record_source: None,
        });
        register_scope(
            physical,
            scope.clone(),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        register_scope(
            physical,
            scope.clone(),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("exact replay");
        let mut conflict = scope.clone();
        conflict.catalog_epoch += 1;
        assert_eq!(
            register_scope(
                physical,
                conflict.clone(),
                candid::Principal::from_slice(&[0x5E, 0x11])
            ),
            Err(CanonicalExportError::ScopeConflict)
        );
        // Seal advances `record.epoch` to 2 while `record.scope.catalog_epoch` stays frozen at
        // registration; the sealed scope is the ORIGINAL registration identity.
        seal_scope(physical, scope.clone(), 2).expect("seal at fresh epoch");
        seal_scope(physical, scope.clone(), 2).expect("exact seal replay");
        // A stale seal (no epoch advance) is rejected.
        assert_eq!(
            seal_scope(physical, scope.clone(), 1),
            Err(CanonicalExportError::InvalidPhase)
        );
        let mut changed_target = scope.clone();
        changed_target.target = CanonicalExportTarget::Vertex {
            label_id: 1,
            property_id: PropertyId::from_raw(3),
            record_source: None,
        };
        assert_eq!(
            seal_scope(physical, changed_target, 2),
            Err(CanonicalExportError::ScopeMismatch)
        );
        // The seal advanced `record.epoch` while `record.scope.catalog_epoch` stayed frozen at
        // registration. Removal and abort therefore require the ORIGINAL registration scope
        // identity; the advanced `conflict` scope is rejected as a mismatch.
        assert_eq!(
            remove_scope(physical, &conflict),
            Err(CanonicalExportError::ScopeMismatch)
        );
        // Sealing forbids direct removal; the caller must abort first.
        assert_eq!(
            remove_scope(physical, &scope),
            Err(CanonicalExportError::InvalidPhase)
        );
        assert_eq!(
            abort_scope(physical, conflict.clone()),
            Err(CanonicalExportError::ScopeMismatch)
        );
        abort_scope(physical, scope.clone()).expect("abort with original registration scope");
        remove_scope(physical, &scope).expect("cleanup");
    }

    #[test]
    fn vertex_page_size_one_resumes_sparse_nonmatching_rows_and_exact_boundary() {
        let store = GraphStore::new();
        let first = store.insert_vertex().expect("first vertex");
        let second = store.insert_vertex().expect("second vertex");
        let label = crate::test_labels::vertex_label_id_for_name("canonical_export_vertex_label");
        store
            .add_vertex_label(first, store.vertex(first).expect("first row"), label)
            .expect("first label");
        store
            .add_vertex_label(second, store.vertex(second).expect("second row"), label)
            .expect("second label");
        let property = crate::test_labels::property_id_for_name("canonical_export_vertex_target");
        let unrelated = crate::test_labels::property_id_for_name("canonical_export_vertex_sparse");
        store
            .set_vertex_property(first, unrelated, Value::Int64(1))
            .expect("unrelated property");
        store
            .set_vertex_property(first, property, Value::Int64(2))
            .expect("first target");
        store
            .set_vertex_property(second, property, Value::Int64(3))
            .expect("second target");

        let physical = PhysicalIndexId::new(900_003).unwrap();
        let target = CanonicalExportTarget::Vertex {
            label_id: label.raw(),
            property_id: property,
            record_source: None,
        };
        let frozen = scope(target.clone());
        register_scope(
            physical,
            frozen,
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        let facts = drain(request_with(target, physical));
        let matching: Vec<_> = facts
            .into_iter()
            .filter_map(|fact| match fact {
                CanonicalIndexableFact::Vertex { vertex_id, .. } => Some(vertex_id),
                _ => None,
            })
            .collect();
        assert!(matching.contains(&u32::from(first)));
        assert!(matching.contains(&u32::from(second)));
        assert_eq!(
            matching
                .iter()
                .filter(|id| **id == u32::from(first))
                .count(),
            1
        );
        assert_eq!(
            matching
                .iter()
                .filter(|id| **id == u32::from(second))
                .count(),
            1
        );
        remove_scope(
            physical,
            &scope(CanonicalExportTarget::Vertex {
                label_id: label.raw(),
                property_id: property,
                record_source: None,
            }),
        )
        .expect("cleanup");
    }

    #[test]
    fn export_vertex_page_walks_nested_records_into_leaf_facts() {
        let store = GraphStore::new();
        let stats = PropertyId::from_raw(8_100_001);
        let score_leaf = PropertyId::from_raw(8_100_002);
        let label = crate::test_labels::vertex_label_id_for_name("nested_export_label");
        let record = |score: i64| {
            Value::Record(vec![
                ("score".to_owned(), Value::Int64(score)),
                (
                    "meta".to_owned(),
                    Value::Record(vec![("deep".to_owned(), Value::Int64(score * 2))]),
                ),
            ])
        };
        let scored = store.insert_vertex().expect("scored vertex");
        store
            .add_vertex_label(scored, store.vertex(scored).expect("row"), label)
            .expect("label");
        store
            .set_vertex_property(scored, stats, record(30))
            .expect("scored record");
        let untyped = store.insert_vertex().expect("absence vertex");
        store
            .add_vertex_label(untyped, store.vertex(untyped).expect("row"), label)
            .expect("label");
        // Absence shapes: missing root, non-record node, container leaf.
        store
            .set_vertex_property(untyped, PropertyId::from_raw(8_100_003), Value::Int64(1))
            .expect("unrelated value");
        store
            .set_vertex_property(untyped, stats, Value::Int64(5))
            .expect("non-record root");

        let physical = PhysicalIndexId::new(900_009).unwrap();
        let target = CanonicalExportTarget::Vertex {
            label_id: label.raw(),
            property_id: score_leaf,
            record_source: Some(CanonicalRecordSource {
                ancestor_property_id: stats,
                field_tail: "score".to_owned(),
            }),
        };
        register_scope(
            physical,
            scope(target.clone()),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        let mut request = request_with(target, physical);
        request.limit = 1_000;

        let page = export_page(request.clone()).expect("export page");
        assert!(page.done, "fixture fits one page");
        assert_eq!(
            page.facts,
            vec![CanonicalIndexableFact::Vertex {
                vertex_id: u32::from(scored),
                property_id: score_leaf,
                encoded_value: sortable_index_key(&Value::Int64(30)).expect("int64 indexable"),
            }],
            "only the scalar leaf of the declared path is exported"
        );

        // The deep tail resolves through nested records under the same ancestor.
        let target = CanonicalExportTarget::Vertex {
            label_id: label.raw(),
            property_id: score_leaf,
            record_source: Some(CanonicalRecordSource {
                ancestor_property_id: stats,
                field_tail: "meta.deep".to_owned(),
            }),
        };
        remove_scope(
            physical,
            &scope(CanonicalExportTarget::Vertex {
                label_id: label.raw(),
                property_id: score_leaf,
                record_source: Some(CanonicalRecordSource {
                    ancestor_property_id: stats,
                    field_tail: "score".to_owned(),
                }),
            }),
        )
        .expect("cleanup first scope");
        let physical = PhysicalIndexId::new(900_010).unwrap();
        register_scope(
            physical,
            scope(target.clone()),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register deep scope");
        let mut request = request_with(target, physical);
        request.limit = 1_000;

        let page = export_page(request).expect("deep export page");
        assert!(page.done);
        assert_eq!(
            page.facts,
            vec![CanonicalIndexableFact::Vertex {
                vertex_id: u32::from(scored),
                property_id: score_leaf,
                encoded_value: sortable_index_key(&Value::Int64(60)).expect("int64 indexable"),
            }],
            "the deep leaf walks through the intermediate record"
        );
        remove_scope(
            physical,
            &scope(CanonicalExportTarget::Vertex {
                label_id: label.raw(),
                property_id: score_leaf,
                record_source: Some(CanonicalRecordSource {
                    ancestor_property_id: stats,
                    field_tail: "meta.deep".to_owned(),
                }),
            }),
        )
        .expect("cleanup deep scope");
    }

    #[test]
    fn byte_boundary_resumes_vertex_candidate_without_loss_or_duplicate() {
        let store = GraphStore::new();
        let property = crate::test_labels::property_id_for_name("canonical_export_byte_boundary");
        let value = Value::Bytes(vec![0x7f; 4093]);
        let encoded = sortable_index_key(&value).expect("fixed-size bytes are indexable");
        assert_eq!(encoded.len(), MAX_CANONICAL_EXPORT_PAGE_BYTES / 387);

        for _ in 0..388 {
            let vertex = store.insert_vertex().expect("vertex");
            let label = crate::test_labels::vertex_label_id_for_name("canonical_export_byte_label");
            store
                .add_vertex_label(vertex, store.vertex(vertex).expect("vertex row"), label)
                .expect("vertex label");
            store
                .set_vertex_property(vertex, property, value.clone())
                .expect("large target property");
        }

        let physical = PhysicalIndexId::new(900_008).unwrap();
        let target = CanonicalExportTarget::Vertex {
            label_id: crate::test_labels::vertex_label_id_for_name("canonical_export_byte_label")
                .raw(),
            property_id: property,
            record_source: None,
        };
        register_scope(
            physical,
            scope(target.clone()),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        let mut request = request_with(target, physical);
        request.limit = 1_000;

        let first = export_page(request.clone()).expect("first byte-bounded page");
        assert_eq!(first.facts.len(), 387);
        assert!(!first.done);
        let first_ids: Vec<_> = first
            .facts
            .iter()
            .filter_map(|fact| match fact {
                CanonicalIndexableFact::Vertex { vertex_id, .. } => Some(*vertex_id),
                _ => None,
            })
            .collect();
        request.cursor = first.next;

        let second = export_page(request).expect("resumed byte-bounded page");
        assert_eq!(second.facts.len(), 1);
        assert!(second.done);
        let second_id = match &second.facts[0] {
            CanonicalIndexableFact::Vertex { vertex_id, .. } => *vertex_id,
            _ => panic!("expected vertex fact"),
        };
        assert!(!first_ids.contains(&second_id));
        assert_eq!(first_ids.len() + 1, 388);
        remove_scope(
            physical,
            &scope(CanonicalExportTarget::Vertex {
                label_id: crate::test_labels::vertex_label_id_for_name(
                    "canonical_export_byte_label",
                )
                .raw(),
                property_id: property,
                record_source: None,
            }),
        )
        .expect("cleanup");
    }

    #[test]
    fn oversized_single_fact_returns_deterministic_error() {
        let mut exact = PageByteBudget::default();
        assert!(
            exact
                .try_accept(&vec![0; MAX_CANONICAL_EXPORT_PAGE_BYTES])
                .expect("exact page budget fits")
        );
        assert!(
            !exact
                .try_accept(&[0])
                .expect("next fact crosses the page budget")
        );

        let mut budget = PageByteBudget::default();
        let oversized = vec![0; MAX_CANONICAL_EXPORT_PAGE_BYTES + 1];
        assert_eq!(
            budget.try_accept(&oversized),
            Err(CanonicalExportError::FactTooLarge {
                encoded_value_bytes: (MAX_CANONICAL_EXPORT_PAGE_BYTES + 1) as u64,
            })
        );
    }

    #[test]
    fn scope_mismatch_fences_graph_logical_epoch_target_and_physical_dimensions() {
        let property = PropertyId::from_raw(9_000_004);
        let physical = PhysicalIndexId::new(900_004).unwrap();
        let target = CanonicalExportTarget::Vertex {
            label_id: 1,
            property_id: property,
            record_source: None,
        };
        register_scope(
            physical,
            scope(target.clone()),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        let mut changed = request_with(target.clone(), physical);
        changed.graph_id = GraphId::from_raw(2);
        assert!(matches!(
            export_page(changed),
            Err(CanonicalExportError::ScopeMismatch)
        ));
        let mut changed = request_with(target.clone(), physical);
        changed.index_name_id = IndexNameId::from_raw(2);
        assert!(matches!(
            export_page(changed),
            Err(CanonicalExportError::ScopeMismatch)
        ));
        let mut changed = request_with(target.clone(), physical);
        changed.catalog_epoch = 2;
        assert!(matches!(
            export_page(changed),
            Err(CanonicalExportError::ScopeMismatch)
        ));
        let mut changed = request_with(
            CanonicalExportTarget::Vertex {
                label_id: 1,
                property_id: PropertyId::from_raw(9_000_005),
                record_source: None,
            },
            physical,
        );
        assert!(matches!(
            export_page(changed.clone()),
            Err(CanonicalExportError::ScopeMismatch)
        ));
        changed.physical_index_id = PhysicalIndexId::new(900_005).unwrap();
        assert_eq!(
            export_page(changed),
            Err(CanonicalExportError::ScopeNotFound)
        );
        remove_scope(physical, &scope(target)).expect("cleanup");
    }

    #[test]
    fn inline_export_suppresses_undirected_mirror_and_sidecar_conflict() {
        let store = GraphStore::new();
        let label = crate::test_labels::edge_label_id_for_name("canonical_export_inline_label");
        let property = crate::test_labels::property_id_for_name("canonical_export_inline_value");
        let profile = EdgeInlinePropertyProfile {
            byte_width: 4,
            encoding: EdgeInlinePropertyEncoding::F32,
        };
        crate::test_labels::install_test_edge_inline_property_profile(label, profile.clone());
        crate::test_labels::install_test_edge_inline_property(label, property);
        let directed_source = store.insert_vertex().expect("directed source");
        let directed_target = store.insert_vertex().expect("directed target");
        let undirected_low = store.insert_vertex().expect("undirected low");
        let undirected_high = store.insert_vertex().expect("undirected high");
        let directed = store
            .insert_directed_edge_with_inline_property_bytes(
                directed_source,
                directed_target,
                Some(label),
                &1.25f32.to_le_bytes(),
            )
            .expect("directed inline edge");
        store
            .set_edge_property(
                directed.occurrence(LabeledOrientation::Forward),
                property,
                Value::Float32(99.0),
            )
            .expect("conflicting sidecar");
        store
            .insert_undirected_edge_with_inline_property_bytes(
                undirected_low,
                undirected_high,
                Some(label),
                &2.5f32.to_le_bytes(),
            )
            .expect("undirected inline edge");

        let target = CanonicalExportTarget::Edge {
            label_id: label,
            property_id: property,
            direction: EdgeIndexDirection::Any,
        };
        let physical = PhysicalIndexId::new(900_006).unwrap();
        let mut frozen = scope(target.clone());
        frozen.inline = Some(CanonicalInlineProjection {
            source_property_id: property,
            byte_offset: 0,
            source_profile: profile.clone(),
            value_profile: profile,
        });
        register_scope(
            physical,
            frozen,
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register inline scope");
        let facts = drain(request_with(target, physical));
        let edge_facts: Vec<_> = facts
            .into_iter()
            .filter_map(|fact| match fact {
                CanonicalIndexableFact::Edge {
                    owner_vertex_id,
                    encoded_value,
                    ..
                } => Some((owner_vertex_id, encoded_value)),
                _ => None,
            })
            .collect();
        assert_eq!(
            edge_facts.len(),
            2,
            "directed + one canonical undirected fact"
        );
        let expected_directed = sortable_index_key(&Value::Float32(1.25)).unwrap();
        let expected_undirected = sortable_index_key(&Value::Float32(2.5)).unwrap();
        assert!(
            edge_facts
                .iter()
                .any(|(_, value)| *value == expected_directed)
        );
        assert!(
            edge_facts
                .iter()
                .any(|(_, value)| *value == expected_undirected)
        );
        remove_scope(
            physical,
            &CanonicalExportScope {
                graph_id: GraphId::from_raw(1),
                index_name_id: IndexNameId::from_raw(1),
                catalog_epoch: 1,
                target: CanonicalExportTarget::Edge {
                    label_id: label,
                    property_id: property,
                    direction: EdgeIndexDirection::Any,
                },
                inline: Some(CanonicalInlineProjection {
                    source_property_id: property,
                    byte_offset: 0,
                    source_profile: EdgeInlinePropertyProfile {
                        byte_width: 4,
                        encoding: EdgeInlinePropertyEncoding::F32,
                    },
                    value_profile: EdgeInlinePropertyProfile {
                        byte_width: 4,
                        encoding: EdgeInlinePropertyEncoding::F32,
                    },
                }),
            },
        )
        .expect("cleanup");
    }

    #[test]
    fn edge_sidecar_page_size_one_resumes_sparse_rows_without_duplicate_aliases() {
        let store = GraphStore::new();
        let label = crate::test_labels::edge_label_id_for_name("canonical_export_sidecar_label");
        let unrelated_label =
            crate::test_labels::edge_label_id_for_name("canonical_export_sidecar_unrelated_label");
        let property = crate::test_labels::property_id_for_name("canonical_export_sidecar_value");
        let owner = store.insert_vertex().expect("owner");
        let directed_target = store.insert_vertex().expect("directed target");
        let undirected_low = store.insert_vertex().expect("undirected low");
        let undirected_high = store.insert_vertex().expect("undirected high");
        let unrelated_target = store.insert_vertex().expect("unrelated target");
        let directed = store
            .insert_directed_edge(owner, directed_target, Some(label))
            .expect("directed edge");
        store
            .set_edge_property(
                directed.occurrence(LabeledOrientation::Forward),
                property,
                Value::Int64(11),
            )
            .expect("directed sidecar");
        let undirected = store
            .insert_undirected_edge(undirected_low, undirected_high, Some(label))
            .expect("undirected edge");
        store
            .set_edge_property(
                undirected.occurrence(LabeledOrientation::Forward),
                property,
                Value::Int64(22),
            )
            .expect("undirected sidecar");
        let unrelated = store
            .insert_directed_edge(owner, unrelated_target, Some(unrelated_label))
            .expect("unrelated edge");
        store
            .set_edge_property(
                unrelated.occurrence(LabeledOrientation::Forward),
                property,
                Value::Int64(99),
            )
            .expect("unrelated sidecar");

        let target = CanonicalExportTarget::Edge {
            label_id: label,
            property_id: property,
            direction: EdgeIndexDirection::Any,
        };
        let physical = PhysicalIndexId::new(900_007).unwrap();
        register_scope(
            physical,
            scope(target.clone()),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register sidecar scope");
        let facts = drain(request_with(target, physical));
        let values: Vec<_> = facts
            .into_iter()
            .filter_map(|fact| match fact {
                CanonicalIndexableFact::Edge {
                    owner_vertex_id,
                    encoded_value,
                    ..
                } => Some((owner_vertex_id, encoded_value)),
                _ => None,
            })
            .collect();
        assert_eq!(
            values.len(),
            2,
            "one directed and one canonical undirected sidecar"
        );
        assert!(
            values
                .iter()
                .any(|(_, value)| *value == sortable_index_key(&Value::Int64(11)).unwrap())
        );
        assert!(
            values
                .iter()
                .any(|(_, value)| *value == sortable_index_key(&Value::Int64(22)).unwrap())
        );
        remove_scope(
            physical,
            &scope(CanonicalExportTarget::Edge {
                label_id: label,
                property_id: property,
                direction: EdgeIndexDirection::Any,
            }),
        )
        .expect("cleanup");
    }

    fn configure_test_routing(store: &GraphStore) {
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_canister: None,
            }))
            .expect("routing");
    }

    fn build_membership(
        physical: PhysicalIndexId,
        catalog_epoch: u64,
        phase: IndexMaintenancePhase,
        property: PropertyId,
        label: VertexLabelId,
    ) -> IndexedVertexMembership {
        IndexedVertexMembership {
            physical_index_id: physical,
            catalog_epoch,
            phase,
            property_id: property.raw(),
            label_id: label.raw(),
            field_path: String::new(),
            ancestor_property_id: 0,
        }
    }

    fn build_outbox_entries_for(
        physical: PhysicalIndexId,
    ) -> Vec<(
        u64,
        crate::facade::stable::derived_index_outbox::DerivedIndexOutboxEntry,
    )> {
        GraphStore::new()
            .derived_index_outbox_peek(usize::MAX)
            .into_iter()
            .filter(|(_, entry)| {
                matches!(
                    &entry.op,
                    DerivedIndexOutboxOp::IndexBuildDml { request }
                        if request.physical_index_id == physical
                )
            })
            .collect()
    }

    /// Two labels share the same property; only the indexed label's vertices are exported.
    #[test]
    fn vertex_export_excludes_other_labels_sharing_the_property() {
        let store = GraphStore::new();
        let indexed = store.insert_vertex().expect("indexed vertex");
        let other = store.insert_vertex().expect("other-label vertex");
        let indexed_label =
            crate::test_labels::vertex_label_id_for_name("export_cross_label_indexed");
        let other_label = crate::test_labels::vertex_label_id_for_name("export_cross_label_other");
        let property = crate::test_labels::property_id_for_name("export_cross_label_property");
        store
            .add_vertex_label(indexed, store.vertex(indexed).expect("row"), indexed_label)
            .expect("indexed label");
        store
            .add_vertex_label(other, store.vertex(other).expect("row"), other_label)
            .expect("other label");
        store
            .set_vertex_property(indexed, property, Value::Int64(1))
            .expect("indexed value");
        store
            .set_vertex_property(other, property, Value::Int64(2))
            .expect("other value");

        let physical = PhysicalIndexId::new(900_020).unwrap();
        let target = CanonicalExportTarget::Vertex {
            label_id: indexed_label.raw(),
            property_id: property,
            record_source: None,
        };
        register_scope(
            physical,
            scope(target.clone()),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        let facts = drain(request_with(target.clone(), physical));
        let exported: Vec<_> = facts
            .iter()
            .filter_map(|fact| match fact {
                CanonicalIndexableFact::Vertex { vertex_id, .. } => Some(*vertex_id),
                _ => None,
            })
            .collect();
        assert!(
            exported.contains(&u32::from(indexed)),
            "the indexed label's vertex must be exported"
        );
        assert!(
            !exported.contains(&u32::from(other)),
            "the other label's vertex must be excluded"
        );
        remove_scope(physical, &scope(target)).expect("cleanup");
    }

    /// A write affected by a Sealing namespace is rejected BEFORE any canonical mutation: the
    /// vertex property store, the Memory46 outbox, and the scope watermarks are all unchanged.
    #[test]
    fn sealing_rejects_affected_vertex_property_write_before_any_canonical_mutation() {
        let store = GraphStore::new();
        let vertex = store.insert_vertex().expect("vertex");
        store
            .add_vertex_label(
                vertex,
                store.vertex(vertex).expect("vertex row"),
                VertexLabelId::from_raw(1),
            )
            .expect("target label");
        let property = PropertyId::from_raw(9_000_010);
        let physical = PhysicalIndexId::new(900_021).unwrap();
        let target = CanonicalExportTarget::Vertex {
            label_id: 1,
            property_id: property,
            record_source: None,
        };
        let frozen = scope(target.clone());
        register_scope(
            physical,
            frozen.clone(),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        seal_scope(physical, frozen.clone(), 2).expect("seal");
        let _catalog = crate::index::catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![build_membership(
                physical,
                1,
                IndexMaintenancePhase::Sealing,
                property,
                VertexLabelId::from_raw(1),
            )],
            ..Default::default()
        });
        let error = store
            .set_vertex_property(vertex, property, Value::Int64(7))
            .expect_err("sealing rejects the affected write");
        assert!(matches!(
            error,
            GraphStoreError::IndexBuildAdmission(CanonicalExportError::RetryableSealing)
        ));
        assert_eq!(
            store.vertex_property(vertex, property),
            None,
            "canonical vertex property must be unchanged"
        );
        assert!(
            build_outbox_entries_for(physical).is_empty(),
            "Memory46 outbox must be unchanged"
        );
        let status = scope_status(physical).expect("status");
        assert_eq!(status.phase, CanonicalExportPhase::Sealing);
        assert_eq!(status.epoch, 2);
        assert_eq!(status.admitted_through, 0);
        assert_eq!(status.drained_through, 0);
        abort_scope(physical, frozen.clone()).expect("abort");
        remove_scope(physical, &frozen).expect("cleanup");
    }

    /// A write that does not touch the Sealing namespace succeeds: canonical data is updated
    /// while the scope watermarks stay frozen.
    #[test]
    fn sealing_accepts_unrelated_vertex_property_write() {
        let store = GraphStore::new();
        let vertex = store.insert_vertex().expect("vertex");
        store
            .add_vertex_label(
                vertex,
                store.vertex(vertex).expect("vertex row"),
                VertexLabelId::from_raw(1),
            )
            .expect("target label");
        let indexed = PropertyId::from_raw(9_000_011);
        let unrelated = PropertyId::from_raw(9_000_012);
        let physical = PhysicalIndexId::new(900_022).unwrap();
        let target = CanonicalExportTarget::Vertex {
            label_id: 1,
            property_id: indexed,
            record_source: None,
        };
        let frozen = scope(target.clone());
        register_scope(
            physical,
            frozen.clone(),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        seal_scope(physical, frozen.clone(), 2).expect("seal");
        let _catalog = crate::index::catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![build_membership(
                physical,
                1,
                IndexMaintenancePhase::Sealing,
                indexed,
                VertexLabelId::from_raw(1),
            )],
            ..Default::default()
        });
        store
            .set_vertex_property(vertex, unrelated, Value::Int64(42))
            .expect("unrelated write succeeds during sealing");
        assert_eq!(
            store.vertex_property(vertex, unrelated),
            Some(Value::Int64(42)),
            "unrelated canonical write must be visible"
        );
        let status = scope_status(physical).expect("status");
        assert_eq!(status.phase, CanonicalExportPhase::Sealing);
        assert_eq!(status.epoch, 2);
        assert_eq!(status.admitted_through, 0);
        assert_eq!(status.drained_through, 0);
        abort_scope(physical, frozen.clone()).expect("abort");
        remove_scope(physical, &frozen).expect("cleanup");
    }

    /// A re-drive after the Router seal-activation crash window must not strand the migration:
    /// sealing an already-`Active` scope under the exact same frozen identity and lifecycle epoch
    /// is an exact replay returning the durable status; a different epoch or identity still
    /// fails closed, and `Active` remains non-abortable and non-removable.
    #[test]
    fn seal_scope_replays_already_active_scope_after_activation_crash_window() {
        let store = GraphStore::new();
        configure_test_routing(&store);
        let property = PropertyId::from_raw(9_000_015);
        let physical = PhysicalIndexId::new(900_025).unwrap();
        let target = CanonicalExportTarget::Vertex {
            label_id: 1,
            property_id: property,
            record_source: None,
        };
        let frozen = scope(target);
        register_scope(
            physical,
            frozen.clone(),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        seal_scope(physical, frozen.clone(), 2).expect("seal");
        let proof = IndexBuildSealStatus {
            base_complete: true,
            seal_catalog_epoch: 2,
            watermarks: vec![gleaph_graph_kernel::index::IndexBuildShardWatermark {
                shard_id: 0,
                admitted_through: 0,
                drained_through: 0,
            }],
        };
        let activated = activate_scope(physical, proof).expect("activate");
        assert_eq!(activated.phase, CanonicalExportPhase::Active);

        // Crash-window replay: same frozen identity, same lifecycle epoch.
        let replay = seal_scope(physical, frozen.clone(), 2).expect("active seal replay");
        assert_eq!(replay.phase, CanonicalExportPhase::Active);
        assert_eq!(replay.epoch, 2);
        assert_eq!(replay.admitted_through, 0);
        assert_eq!(replay.drained_through, 0);

        // A different lifecycle epoch is stale and fails closed.
        assert_eq!(
            seal_scope(physical, frozen.clone(), 3),
            Err(CanonicalExportError::InvalidPhase)
        );
        // A different identity on the same physical namespace fails closed.
        let mut other = frozen.clone();
        other.target = CanonicalExportTarget::Vertex {
            label_id: 1,
            property_id: PropertyId::from_raw(9_000_016),
            record_source: None,
        };
        assert_eq!(
            seal_scope(physical, other, 2),
            Err(CanonicalExportError::ScopeMismatch)
        );
        // Active stays planner-visible: abort and removal are both rejected.
        assert_eq!(
            abort_scope(physical, frozen.clone()),
            Err(CanonicalExportError::InvalidPhase)
        );
        assert_eq!(
            remove_scope(physical, &frozen),
            Err(CanonicalExportError::InvalidPhase)
        );
    }

    /// A Building update is ONE request with ONE contiguous sequence: a single envelope carrying
    /// the removals AND insertions together, never two separate admissions.
    #[test]
    fn building_update_emits_one_envelope_with_combined_removals_and_insertions() {
        let store = GraphStore::new();
        configure_test_routing(&store);
        let vertex = store.insert_vertex().expect("vertex");
        store
            .add_vertex_label(
                vertex,
                store.vertex(vertex).expect("vertex row"),
                VertexLabelId::from_raw(1),
            )
            .expect("target label");
        let property = PropertyId::from_raw(9_000_013);
        let physical = PhysicalIndexId::new(900_023).unwrap();
        let target = CanonicalExportTarget::Vertex {
            label_id: 1,
            property_id: property,
            record_source: None,
        };
        let frozen = scope(target.clone());
        register_scope(
            physical,
            frozen.clone(),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        let _catalog = crate::index::catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![build_membership(
                physical,
                1,
                IndexMaintenancePhase::Building,
                property,
                VertexLabelId::from_raw(1),
            )],
            ..Default::default()
        });
        store
            .set_vertex_property(vertex, property, Value::Int64(5))
            .expect("first write");
        store
            .set_vertex_property(vertex, property, Value::Int64(9))
            .expect("update");
        let entries = build_outbox_entries_for(physical);
        assert_eq!(entries.len(), 2, "one envelope per write");
        let (_, second) = &entries[1];
        let DerivedIndexOutboxOp::IndexBuildDml { request } = &second.op else {
            panic!("expected build DML envelope");
        };
        assert_eq!(
            request.shard_sequence, 2,
            "one contiguous sequence per envelope"
        );
        assert_eq!(request.removals.len(), 1, "the old value is removed");
        assert_eq!(request.insertions.len(), 1, "the new value is inserted");
        assert_eq!(
            request.removals[0],
            sortable_index_key(&Value::Int64(5)).unwrap()
        );
        assert_eq!(
            request.insertions[0],
            sortable_index_key(&Value::Int64(9)).unwrap()
        );
        let status = scope_status(physical).expect("status");
        assert_eq!(status.admitted_through, 2);
        ack_build_dml(physical, 1, 1).expect("ack first");
        ack_build_dml(physical, 1, 2).expect("ack second");
        abort_scope(physical, frozen.clone()).expect("abort");
        remove_scope(physical, &frozen).expect("cleanup");
    }

    struct DrainingIndex {
        applied: RefCell<Vec<IndexBuildDmlRequest>>,
        fail_next: bool,
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for DrainingIndex {
        async fn apply_index_build_dml(
            &self,
            request: IndexBuildDmlRequest,
        ) -> Result<(), crate::plan::PlanQueryError> {
            if self.fail_next {
                return Err(crate::plan::PlanQueryError::FederatedIndexCall {
                    op: "apply_index_build_dml",
                    detail: "transport failure".into(),
                });
            }
            self.applied.borrow_mut().push(request);
            Ok(())
        }

        async fn lookup_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Result<Vec<gleaph_graph_kernel::index::PostingHit>, crate::plan::PlanQueryError>
        {
            Ok(vec![])
        }

        async fn lookup_range(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _req: &gleaph_graph_kernel::index::PostingRangeRequest,
        ) -> Result<Vec<gleaph_graph_kernel::index::PostingHit>, crate::plan::PlanQueryError>
        {
            Ok(vec![])
        }

        async fn lookup_intersection(
            &self,
            _req: &gleaph_graph_kernel::index::IndexIntersectionRequest,
        ) -> Result<gleaph_graph_kernel::index::IndexIntersectionResult, crate::plan::PlanQueryError>
        {
            Ok(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices(vec![]))
        }

        fn local_shard_id(&self) -> ShardId {
            ShardId::new(0)
        }

        async fn posting_insert_at(
            &self,
            _shard_id: ShardId,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }

        async fn posting_remove_at(
            &self,
            _shard_id: ShardId,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }

        async fn label_posting_insert_at(
            &self,
            _shard_id: ShardId,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }

        async fn label_posting_remove_at(
            &self,
            _shard_id: ShardId,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }
    }

    fn admit_one_building_envelope(
        store: &GraphStore,
        physical: PhysicalIndexId,
    ) -> CanonicalExportScope {
        let vertex = store.insert_vertex().expect("vertex");
        store
            .add_vertex_label(
                vertex,
                store.vertex(vertex).expect("vertex row"),
                VertexLabelId::from_raw(1),
            )
            .expect("target label");
        let property = PropertyId::from_raw(9_000_014);
        let target = CanonicalExportTarget::Vertex {
            label_id: 1,
            property_id: property,
            record_source: None,
        };
        let frozen = scope(target.clone());
        register_scope(
            physical,
            frozen.clone(),
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register");
        let _catalog = crate::index::catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![build_membership(
                physical,
                1,
                IndexMaintenancePhase::Building,
                property,
                VertexLabelId::from_raw(1),
            )],
            ..Default::default()
        });
        store
            .set_vertex_property(vertex, property, Value::Int64(5))
            .expect("admit building envelope");
        frozen
    }

    /// Drain: an exact acknowledgement advances `drained_through` and removes the outbox entry.
    #[test]
    fn drain_acks_exact_envelope_and_removes_outbox_entry() {
        let store = GraphStore::new();
        configure_test_routing(&store);
        let physical = PhysicalIndexId::new(900_024).unwrap();
        let frozen = admit_one_building_envelope(&store, physical);
        let client = DrainingIndex {
            applied: RefCell::new(Vec::new()),
            fail_next: false,
        };
        let progress = pollster::block_on(drain_index_build_outbox(
            &client,
            IndexBuildOutboxDrainRequest {
                physical_index_id: physical,
                max_entries: 10,
            },
        ))
        .expect("drain");
        assert_eq!(progress.drained, 1);
        assert_eq!(progress.remaining, 0);
        assert!(progress.converged);
        assert_eq!(client.applied.borrow().len(), 1);
        assert_eq!(client.applied.borrow()[0].shard_sequence, 1);
        assert!(
            build_outbox_entries_for(physical).is_empty(),
            "the acked envelope must be removed from the outbox"
        );
        let status = scope_status(physical).expect("status");
        assert_eq!(status.admitted_through, 1);
        assert_eq!(status.drained_through, 1);
        abort_scope(physical, frozen.clone()).expect("abort");
        remove_scope(physical, &frozen).expect("cleanup");
    }

    /// Drain: a transport/ambiguous failure stops the drain and retains the envelope.
    #[test]
    fn drain_ambiguous_failure_retains_envelope() {
        let store = GraphStore::new();
        configure_test_routing(&store);
        let physical = PhysicalIndexId::new(900_025).unwrap();
        let frozen = admit_one_building_envelope(&store, physical);
        let client = DrainingIndex {
            applied: RefCell::new(Vec::new()),
            fail_next: true,
        };
        let progress = pollster::block_on(drain_index_build_outbox(
            &client,
            IndexBuildOutboxDrainRequest {
                physical_index_id: physical,
                max_entries: 10,
            },
        ))
        .expect("drain stops");
        assert_eq!(progress.drained, 0);
        assert_eq!(progress.remaining, 1);
        assert!(!progress.converged);
        assert!(client.applied.borrow().is_empty());
        assert_eq!(
            build_outbox_entries_for(physical).len(),
            1,
            "the envelope must be retained for an exact replay"
        );
        let status = scope_status(physical).expect("status");
        assert_eq!(status.drained_through, 0);
        assert_eq!(status.admitted_through, 1);
        // Test hygiene: acknowledge the retained envelope so the scope reaches drained == admitted
        // and can be removed cleanly (a later drain step would do this in production).
        ack_build_dml(physical, 1, 1).expect("ack retained envelope");
        abort_scope(physical, frozen.clone()).expect("abort");
        remove_scope(physical, &frozen).expect("cleanup");
    }

    /// Scope-bound export admission (plan 0297 text backfill): the frozen scope names exactly
    /// one authorized puller; everyone else — including anonymous and the formerly
    /// hardcoded graph-index identity — fails closed without leaking scope state.
    #[test]
    fn page_pulls_are_admitted_only_for_the_bound_puller() {
        use candid::Principal;

        let physical = PhysicalIndexId::new(940_101).expect("non-zero physical id");
        let puller = Principal::from_slice(&[0x5E, 0xAD]);
        let interloper = Principal::from_slice(&[0x0B, 0xAD]);

        // No scope yet: unknown namespaces fail closed identically for any caller.
        assert_eq!(
            authorize_page_pull(puller, physical),
            Err(CanonicalExportError::ScopeNotFound)
        );

        register_scope(
            physical,
            scope(CanonicalExportTarget::Vertex {
                label_id: 1,
                property_id: PropertyId::from_raw(2),
                record_source: None,
            }),
            puller,
        )
        .expect("register with explicit puller");

        assert_eq!(authorize_page_pull(puller, physical), Ok(()));
        assert_eq!(
            authorize_page_pull(interloper, physical),
            Err(CanonicalExportError::UnauthorizedPuller)
        );
        assert_eq!(
            authorize_page_pull(Principal::anonymous(), physical),
            Err(CanonicalExportError::UnauthorizedPuller)
        );

        // Replay exactness covers the puller too: same contract replays, a different
        // puller conflicts before any durable write.
        let bound = CANONICAL_EXPORT_SCOPES.with_borrow(|scopes| scopes.get(physical));
        drop(bound);
        register_scope(
            physical,
            scope(CanonicalExportTarget::Vertex {
                label_id: 1,
                property_id: PropertyId::from_raw(2),
                record_source: None,
            }),
            puller,
        )
        .expect("exact replay with same puller");
        assert_eq!(
            register_scope(
                physical,
                scope(CanonicalExportTarget::Vertex {
                    label_id: 1,
                    property_id: PropertyId::from_raw(2),
                    record_source: None,
                }),
                interloper,
            ),
            Err(CanonicalExportError::ScopeConflict)
        );
        remove_scope(
            physical,
            &scope(CanonicalExportTarget::Vertex {
                label_id: 1,
                property_id: PropertyId::from_raw(2),
                record_source: None,
            }),
        )
        .expect("cleanup");
    }
}
