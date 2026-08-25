//! ADR 0059 physical-index build registration, touched-first DML, and base seeding.

use std::collections::BTreeSet;

use candid::{Encode, Principal};
use gleaph_graph_kernel::canonical_export::{
    CanonicalExportRequest, CanonicalExportTarget, CanonicalIndexableFact,
    MAX_CANONICAL_EXPORT_PAGE_ITEMS,
};
use gleaph_graph_kernel::entry::TaggedEdgeLabelId;
use gleaph_graph_kernel::index::{
    IndexBuildControlRequest, IndexBuildDmlRequest, IndexBuildSealRequest, IndexBuildSealStatus,
    IndexBuildSeedDisposition, IndexBuildSeedPageRequest, IndexBuildSeedPageResult,
    IndexBuildStatus, IndexBuildSubject, IndexBuildTarget, MAX_INDEX_BUILD_CURSOR_BYTES,
    MAX_INDEX_BUILD_DML_VALUES, MAX_INDEX_BUILD_TARGET_SHARDS, PhysicalIndexId,
    RegisterIndexBuildRequest,
};

use super::{IndexStore, ensure_index_value_key};
use crate::build_key::IndexBuildTouchedKey;
use crate::edge_key::EdgePostingKey;
use crate::facade::stable::build_state::{
    IndexBuildLastPage, IndexBuildLifecycle, IndexBuildState,
};
use crate::facade::stable::{
    INDEX_BUILD_STATES, INDEX_BUILD_TOUCHED_SUBJECTS, INDEX_EDGE_POSTINGS, INDEX_OWNERSHIP_CONFIG,
    INDEX_SHARD_CANISTER_CATALOG, INDEX_VERTEX_POSTINGS,
};
use crate::key::PostingKey;
use crate::state::IndexError;

#[derive(Clone, Debug)]
enum PreparedBuildPosting {
    Vertex(PostingKey),
    Edge(EdgePostingKey),
}

/// The one edge-label identity rule for stored edge postings (GAP-2026-08-22-001).
///
/// Postings, seed facts, and DML build subjects carry the LARA wire tag
/// (`TaggedEdgeLabelId`: catalog id plus the directed MSB); registrations name the catalog
/// label. A wire label matches its target when its catalog index equals the registered
/// catalog label and its bucket packing is covered by the registration direction. Translation
/// goes through `gleaph_graph_kernel::entry` types — no ad-hoc bit arithmetic.
fn edge_wire_matches_target(target: &IndexBuildTarget, wire: TaggedEdgeLabelId) -> bool {
    let IndexBuildTarget::Edge {
        label_id: target_label,
        direction,
        ..
    } = target
    else {
        return false;
    };
    if wire.label_index() != *target_label {
        return false;
    }
    if wire.is_directed() {
        direction.includes_directed()
    } else {
        direction.includes_undirected()
    }
}

/// One immutable Graph call prepared from the durable build state.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedIndexBuildPull {
    pub(crate) graph_canister: Principal,
    pub(crate) export: CanonicalExportRequest,
    pub(crate) page_sequence: u64,
    pub(crate) shard_id: u32,
    pub(crate) expected_cursor: Option<Vec<u8>>,
}

fn canonical_target(target: &IndexBuildTarget) -> CanonicalExportTarget {
    match target {
        IndexBuildTarget::Vertex {
            label_id,
            property_id,
            record_source,
        } => CanonicalExportTarget::Vertex {
            label_id: *label_id,
            property_id: *property_id,
            record_source: record_source.clone(),
        },
        IndexBuildTarget::Edge {
            label_id,
            property_id,
            direction,
        } => CanonicalExportTarget::Edge {
            label_id: gleaph_graph_kernel::entry::EdgeLabelId::from_raw(*label_id),
            property_id: *property_id,
            direction: *direction,
        },
        // Pure projection only. Text registrations are rejected before any page flows:
        // subject checks reject their DML, and Graph scope validation rejects a Text target
        // so `export_page` can never run for one (ADR 0059 §Text build kind).
        IndexBuildTarget::Text {
            label_id,
            property_id,
            ..
        } => CanonicalExportTarget::Text {
            label_id: *label_id,
            property_id: *property_id,
        },
    }
}

fn ensure_subject_matches_target(
    target: &IndexBuildTarget,
    subject: IndexBuildSubject,
) -> Result<(), IndexError> {
    match (target, subject) {
        (IndexBuildTarget::Vertex { .. }, IndexBuildSubject::Vertex { .. }) => Ok(()),
        // DML subjects arrive in wire space from Graph canonical handles; the target
        // speaks catalog ids. One identity rule decides the match (GAP-2026-08-22-001).
        (
            IndexBuildTarget::Edge { .. },
            IndexBuildSubject::Edge {
                label_id: subject_wire,
                ..
            },
        ) if edge_wire_matches_target(target, TaggedEdgeLabelId::from_raw(subject_wire)) => Ok(()),
        _ => Err(IndexError::InvalidIndexBuildTarget),
    }
}

pub(super) fn ensure_control(
    state: &IndexBuildState,
    control: &IndexBuildControlRequest,
) -> Result<(), IndexError> {
    if state.registration(control.registration.physical_index_id) != control.registration {
        return Err(IndexError::InvalidIndexBuildControl);
    }
    Ok(())
}

fn ensure_request_bytes<T: candid::CandidType>(request: &T) -> Result<(), IndexError> {
    let bytes = Encode!(request).map_err(|error| {
        IndexError::IndexBuildFingerprintFailed(format!("request encode failed: {error}"))
    })?;
    if bytes.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        return Err(IndexError::IndexBuildRequestTooLarge);
    }
    Ok(())
}

fn ensure_cursor(cursor: &Option<Vec<u8>>) -> Result<(), IndexError> {
    if cursor
        .as_ref()
        .is_some_and(|bytes| bytes.len() > MAX_INDEX_BUILD_CURSOR_BYTES)
    {
        return Err(IndexError::InvalidIndexBuildCursor);
    }
    Ok(())
}

fn prepare_subject_posting(
    physical_index_id: PhysicalIndexId,
    target: &IndexBuildTarget,
    subject: IndexBuildSubject,
    value: Vec<u8>,
) -> Result<PreparedBuildPosting, IndexError> {
    ensure_subject_matches_target(target, subject)?;
    ensure_index_value_key(&value)?;
    let property_id = target.property_id().raw();
    match (target, subject) {
        (
            IndexBuildTarget::Vertex { .. },
            IndexBuildSubject::Vertex {
                shard_id,
                vertex_id,
            },
        ) => Ok(PreparedBuildPosting::Vertex(PostingKey {
            physical_index_id,
            property_id,
            value,
            shard_id: shard_id.into(),
            vertex_id,
        })),
        (
            IndexBuildTarget::Edge { .. },
            IndexBuildSubject::Edge {
                shard_id,
                owner_vertex_id,
                label_id,
                slot_index,
            },
        ) => Ok(PreparedBuildPosting::Edge(EdgePostingKey {
            physical_index_id,
            property_id,
            value,
            label_id,
            shard_id: shard_id.into(),
            owner_vertex_id,
            slot_index,
        })),
        _ => Err(IndexError::InvalidIndexBuildTarget),
    }
}

fn prepare_fact_posting(
    physical_index_id: PhysicalIndexId,
    target: &IndexBuildTarget,
    shard_id: u32,
    fact: CanonicalIndexableFact,
) -> Result<(IndexBuildSubject, PreparedBuildPosting), IndexError> {
    match fact {
        CanonicalIndexableFact::Vertex {
            vertex_id,
            property_id,
            encoded_value,
        } if matches!(target, IndexBuildTarget::Vertex { .. })
            && property_id == target.property_id() =>
        {
            let subject = IndexBuildSubject::Vertex {
                shard_id,
                vertex_id,
            };
            let posting =
                prepare_subject_posting(physical_index_id, target, subject, encoded_value)?;
            Ok((subject, posting))
        }
        CanonicalIndexableFact::Edge {
            owner_vertex_id,
            label_id,
            slot_index,
            property_id,
            encoded_value,
        } if matches!(target, IndexBuildTarget::Edge { .. })
            && edge_wire_matches_target(target, TaggedEdgeLabelId::from_raw(label_id))
            && property_id == target.property_id() =>
        {
            let subject = IndexBuildSubject::Edge {
                shard_id,
                owner_vertex_id,
                label_id,
                slot_index,
            };
            let posting =
                prepare_subject_posting(physical_index_id, target, subject, encoded_value)?;
            Ok((subject, posting))
        }
        _ => Err(IndexError::InvalidIndexBuildTarget),
    }
}

fn insert_prepared(posting: PreparedBuildPosting) {
    match posting {
        PreparedBuildPosting::Vertex(key) => {
            INDEX_VERTEX_POSTINGS.with_borrow_mut(|postings| {
                postings.insert(key);
            });
        }
        PreparedBuildPosting::Edge(key) => {
            INDEX_EDGE_POSTINGS.with_borrow_mut(|postings| {
                postings.insert(key);
            });
        }
    }
}

fn remove_prepared(posting: &PreparedBuildPosting) {
    match posting {
        PreparedBuildPosting::Vertex(key) => {
            INDEX_VERTEX_POSTINGS.with_borrow_mut(|postings| {
                postings.remove(key);
            });
        }
        PreparedBuildPosting::Edge(key) => {
            INDEX_EDGE_POSTINGS.with_borrow_mut(|postings| {
                postings.remove(key);
            });
        }
    }
}

impl IndexStore {
    pub fn register_index_build(
        &self,
        caller: Principal,
        request: &RegisterIndexBuildRequest,
    ) -> Result<IndexBuildStatus, IndexError> {
        self.assert_router_caller(caller)?;
        ensure_request_bytes(request)?;
        let invalid_target = match &request.target {
            IndexBuildTarget::Vertex {
                label_id,
                property_id,
                record_source,
            } => {
                // A nested leaf scope must name a well-formed walk rooted at another
                // property; flat scopes carry no source at all.
                let invalid_source = match record_source {
                    None => false,
                    Some(source) => {
                        source.ancestor_property_id.raw() == 0
                            || source.field_tail.is_empty()
                            || source
                                .field_tail
                                .split('.')
                                .any(|segment| segment.is_empty())
                            || source.ancestor_property_id == *property_id
                    }
                };
                *label_id == 0 || property_id.raw() == 0 || invalid_source
            }
            IndexBuildTarget::Edge {
                label_id,
                property_id,
                ..
            } => *label_id == 0 || property_id.raw() == 0,
            // Text builds are analyzed and ingested by the text canister, never by the
            // posting store; admission rejects them here (ADR 0059 §Text build kind).
            IndexBuildTarget::Text { .. } => true,
        };
        if request.graph_id.is_reserved() || request.index_name_id.is_reserved() || invalid_target {
            return Err(IndexError::InvalidIndexBuildScope);
        }
        if request.target_shard_ids.is_empty()
            || request.target_shard_ids.len() > MAX_INDEX_BUILD_TARGET_SHARDS
            || request
                .target_shard_ids
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(IndexError::InvalidIndexBuildTargetShards);
        }

        let ownership = INDEX_OWNERSHIP_CONFIG.with_borrow(|cell| cell.get().clone());
        if !ownership.initialized || ownership.graph_id != request.graph_id {
            return Err(IndexError::InvalidIndexBuildScope);
        }
        let all_attached = INDEX_SHARD_CANISTER_CATALOG.with_borrow(|catalog| {
            request
                .target_shard_ids
                .iter()
                .all(|shard_id| catalog.shard_canister((*shard_id).into()).is_some())
        });
        if !all_attached {
            return Err(IndexError::InvalidIndexBuildTargetShards);
        }

        if let Some(existing) =
            INDEX_BUILD_STATES.with_borrow(|states| states.get(&request.physical_index_id))
        {
            if existing.registration(request.physical_index_id) == *request {
                return Ok(existing.status(request.physical_index_id));
            }
            return Err(IndexError::IndexBuildAlreadyRegistered);
        }

        let state = IndexBuildState::new(request);
        let status = state.status(request.physical_index_id);
        INDEX_BUILD_STATES.with_borrow_mut(|states| {
            states.insert(request.physical_index_id, state);
        });
        Ok(status)
    }

    pub fn index_build_status(
        &self,
        caller: Principal,
        physical_index_id: PhysicalIndexId,
    ) -> Result<IndexBuildStatus, IndexError> {
        self.assert_router_caller(caller)?;
        INDEX_BUILD_STATES
            .with_borrow(|states| states.get(&physical_index_id))
            .map(|state| state.status(physical_index_id))
            .ok_or(IndexError::UnknownIndexBuild)
    }

    /// Prepares one Graph-owned canonical export call without mutating progress.
    ///
    /// The response callback must pass the returned envelope to [`Self::apply_index_build_pull`].
    /// Re-reading durable state after every await makes concurrent exact retries safe.
    pub(crate) fn prepare_index_build_pull(
        &self,
        caller: Principal,
        control: &IndexBuildControlRequest,
    ) -> Result<Option<PreparedIndexBuildPull>, IndexError> {
        self.assert_router_caller(caller)?;
        ensure_request_bytes(control)?;
        let state = INDEX_BUILD_STATES
            .with_borrow(|states| states.get(&control.registration.physical_index_id))
            .ok_or(IndexError::UnknownIndexBuild)?;
        ensure_control(&state, control)?;
        if !matches!(&state.lifecycle, IndexBuildLifecycle::Building) {
            return Err(IndexError::IndexBuildNotBuilding);
        }
        if state.done() {
            return Ok(None);
        }
        let shard_id = state
            .progress()
            .expected_shard_id
            .ok_or(IndexError::StaleIndexBuildProgress)?;
        let graph_canister = INDEX_SHARD_CANISTER_CATALOG
            .with_borrow(|catalog| catalog.shard_canister(shard_id.into()))
            .ok_or(IndexError::UnknownShard)?;
        let cursor = state.cursor.clone();
        Ok(Some(PreparedIndexBuildPull {
            graph_canister,
            export: CanonicalExportRequest {
                graph_id: state.scope.graph_id,
                index_name_id: state.scope.index_name_id,
                physical_index_id: control.registration.physical_index_id,
                catalog_epoch: state.scope.catalog_epoch,
                target: canonical_target(&state.scope.target),
                cursor: cursor.clone(),
                limit: MAX_CANONICAL_EXPORT_PAGE_ITEMS,
            },
            page_sequence: state.next_page_sequence,
            shard_id,
            expected_cursor: cursor,
        }))
    }

    /// Applies one fetched Graph page after revalidating the exact control and immutable export
    /// identity against current durable state.
    ///
    /// The durable seed receipt owns current-cursor validation. Avoiding a second preparation here
    /// lets two callbacks for the same page converge through its O(1) exact-replay path.
    pub(crate) fn apply_index_build_pull(
        &self,
        caller: Principal,
        control: &IndexBuildControlRequest,
        prepared: &PreparedIndexBuildPull,
        page: gleaph_graph_kernel::canonical_export::CanonicalExportPage,
    ) -> Result<IndexBuildStatus, IndexError> {
        let state = INDEX_BUILD_STATES
            .with_borrow(|states| states.get(&control.registration.physical_index_id))
            .ok_or(IndexError::UnknownIndexBuild)?;
        self.assert_router_caller(caller)?;
        ensure_control(&state, control)?;
        if !matches!(&state.lifecycle, IndexBuildLifecycle::Building) {
            return Err(IndexError::IndexBuildNotBuilding);
        }
        let expected_graph_canister = INDEX_SHARD_CANISTER_CATALOG
            .with_borrow(|catalog| catalog.shard_canister(prepared.shard_id.into()))
            .ok_or(IndexError::UnknownShard)?;
        let expected_export = CanonicalExportRequest {
            graph_id: state.scope.graph_id,
            index_name_id: state.scope.index_name_id,
            physical_index_id: control.registration.physical_index_id,
            catalog_epoch: state.scope.catalog_epoch,
            target: canonical_target(&state.scope.target),
            cursor: prepared.expected_cursor.clone(),
            limit: MAX_CANONICAL_EXPORT_PAGE_ITEMS,
        };
        if expected_graph_canister != prepared.graph_canister
            || expected_export != prepared.export
            || !state
                .shards
                .iter()
                .any(|shard| shard.shard_id == prepared.shard_id)
        {
            return Err(IndexError::StaleIndexBuildProgress);
        }
        self.seed_index_build_page(&IndexBuildSeedPageRequest {
            physical_index_id: control.registration.physical_index_id,
            catalog_epoch: control.registration.catalog_epoch,
            page_sequence: prepared.page_sequence,
            shard_id: prepared.shard_id,
            expected_cursor: prepared.expected_cursor.clone(),
            facts: page.facts,
            next_cursor: page.next,
            done: page.done,
        })?;
        INDEX_BUILD_STATES
            .with_borrow(|states| states.get(&control.registration.physical_index_id))
            .map(|state| state.status(control.registration.physical_index_id))
            .ok_or(IndexError::UnknownIndexBuild)
    }

    pub fn seal_index_build(
        &self,
        caller: Principal,
        request: &IndexBuildSealRequest,
    ) -> Result<IndexBuildSealStatus, IndexError> {
        self.assert_router_caller(caller)?;
        ensure_request_bytes(request)?;
        let physical_index_id = request.control.registration.physical_index_id;
        let mut state = INDEX_BUILD_STATES
            .with_borrow(|states| states.get(&physical_index_id))
            .ok_or(IndexError::UnknownIndexBuild)?;
        ensure_control(&state, &request.control)?;

        if let Some(status) = state.seal_status() {
            let exact_targets = status
                .watermarks
                .iter()
                .map(
                    |watermark| gleaph_graph_kernel::index::IndexBuildSealTarget {
                        shard_id: watermark.shard_id,
                        admitted_through: watermark.admitted_through,
                    },
                )
                .collect::<Vec<_>>();
            if status.seal_catalog_epoch == request.seal_catalog_epoch
                && exact_targets == request.shard_targets
            {
                return Ok(status);
            }
            return Err(IndexError::InvalidIndexBuildSeal);
        }
        if !matches!(&state.lifecycle, IndexBuildLifecycle::Building) {
            return Err(IndexError::IndexBuildAborted);
        }
        if !state.done() {
            return Err(IndexError::IndexBuildNotReadyToSeal);
        }
        if request.seal_catalog_epoch <= state.scope.catalog_epoch
            || request.shard_targets.len() != state.shards.len()
            || request
                .shard_targets
                .windows(2)
                .any(|pair| pair[0].shard_id >= pair[1].shard_id)
        {
            return Err(IndexError::InvalidIndexBuildSeal);
        }
        for (shard, target) in state.shards.iter_mut().zip(&request.shard_targets) {
            if shard.shard_id != target.shard_id
                || target.admitted_through < shard.acknowledged_through
            {
                return Err(IndexError::InvalidIndexBuildSeal);
            }
            shard.seal_target = Some(target.admitted_through);
        }
        state.lifecycle = IndexBuildLifecycle::Sealing {
            seal_catalog_epoch: request.seal_catalog_epoch,
        };
        let status = state
            .seal_status()
            .expect("sealing transition constructs seal status");
        INDEX_BUILD_STATES.with_borrow_mut(|states| {
            states.insert(physical_index_id, state);
        });
        Ok(status)
    }

    pub fn apply_index_build_dml(
        &self,
        caller: Principal,
        request: &IndexBuildDmlRequest,
    ) -> Result<(), IndexError> {
        ensure_request_bytes(request)?;
        let row_count = request
            .removals
            .len()
            .checked_add(request.insertions.len())
            .ok_or(IndexError::TooManyIndexBuildRows)?;
        if row_count > MAX_INDEX_BUILD_DML_VALUES {
            return Err(IndexError::TooManyIndexBuildRows);
        }
        let shard_id = request.subject.shard_id();
        self.assert_shard_canister(caller, shard_id)?;
        if request.shard_sequence == 0 {
            return Err(IndexError::InvalidIndexBuildSequence);
        }
        let fingerprint = request
            .fingerprint()
            .map_err(|error| IndexError::IndexBuildFingerprintFailed(error.to_string()))?;
        let mut state = INDEX_BUILD_STATES
            .with_borrow(|states| states.get(&request.physical_index_id))
            .ok_or(IndexError::UnknownIndexBuild)?;
        if state.scope.catalog_epoch != request.catalog_epoch {
            return Err(IndexError::StaleIndexBuildEpoch);
        }
        ensure_subject_matches_target(&state.scope.target, request.subject)?;
        let shard = state
            .shards
            .iter_mut()
            .find(|progress| progress.shard_id == shard_id.raw())
            .ok_or(IndexError::InvalidIndexBuildScope)?;
        let seal_target = match &state.lifecycle {
            IndexBuildLifecycle::Building => None,
            IndexBuildLifecycle::Sealing { .. } => shard.seal_target,
            IndexBuildLifecycle::Aborting { .. } | IndexBuildLifecycle::Aborted => {
                return Err(IndexError::IndexBuildAborted);
            }
        };
        if seal_target.is_some_and(|target| request.shard_sequence > target) {
            return Err(IndexError::StaleIndexBuildEpoch);
        }
        if request.shard_sequence == shard.acknowledged_through {
            return if shard.last_fingerprint == Some(fingerprint) {
                Ok(())
            } else {
                Err(IndexError::IndexBuildReplayConflict)
            };
        }
        if request.shard_sequence < shard.acknowledged_through {
            return Err(IndexError::IndexBuildReplayTooOld);
        }
        let expected_sequence = shard
            .acknowledged_through
            .checked_add(1)
            .ok_or(IndexError::IndexBuildProgressOverflow)?;
        if request.shard_sequence != expected_sequence {
            return Err(IndexError::IndexBuildSequenceGap);
        }

        let removals = request
            .removals
            .iter()
            .cloned()
            .map(|value| {
                prepare_subject_posting(
                    request.physical_index_id,
                    &state.scope.target,
                    request.subject,
                    value,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let insertions = request
            .insertions
            .iter()
            .cloned()
            .map(|value| {
                prepare_subject_posting(
                    request.physical_index_id,
                    &state.scope.target,
                    request.subject,
                    value,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;

        // No fallible operation follows the first stable write. IC message execution serializes
        // this touched-first co-write and rolls the whole message back if a stable write traps.
        INDEX_BUILD_TOUCHED_SUBJECTS.with_borrow_mut(|touched| {
            touched.insert(IndexBuildTouchedKey::new(
                request.physical_index_id,
                request.subject,
            ));
        });
        for posting in &removals {
            remove_prepared(posting);
        }
        for posting in insertions {
            insert_prepared(posting);
        }
        shard.acknowledged_through = request.shard_sequence;
        shard.last_fingerprint = Some(fingerprint);
        INDEX_BUILD_STATES.with_borrow_mut(|states| {
            states.insert(request.physical_index_id, state);
        });
        Ok(())
    }

    /// Applies one already-fetched Graph page. The later graph-index pull loop calls this
    /// synchronously from its response message; this method performs no inter-canister call.
    pub fn seed_index_build_page(
        &self,
        request: &IndexBuildSeedPageRequest,
    ) -> Result<IndexBuildSeedPageResult, IndexError> {
        ensure_request_bytes(request)?;
        if request.facts.len() > MAX_CANONICAL_EXPORT_PAGE_ITEMS as usize {
            return Err(IndexError::TooManyIndexBuildRows);
        }
        ensure_cursor(&request.expected_cursor)?;
        ensure_cursor(&request.next_cursor)?;
        if request.done != request.next_cursor.is_none()
            || (!request.done && request.next_cursor == request.expected_cursor)
        {
            return Err(IndexError::InvalidIndexBuildCursor);
        }
        let fingerprint = request
            .fingerprint()
            .map_err(|error| IndexError::IndexBuildFingerprintFailed(error.to_string()))?;
        let mut state = INDEX_BUILD_STATES
            .with_borrow(|states| states.get(&request.physical_index_id))
            .ok_or(IndexError::UnknownIndexBuild)?;
        if state.scope.catalog_epoch != request.catalog_epoch {
            return Err(IndexError::StaleIndexBuildEpoch);
        }
        if !matches!(&state.lifecycle, IndexBuildLifecycle::Building) {
            return Err(IndexError::IndexBuildNotBuilding);
        }
        if let Some(last) = &state.last_page
            && request.page_sequence == last.sequence
        {
            if fingerprint != last.fingerprint {
                return Err(IndexError::IndexBuildReplayConflict);
            }
            return Ok(IndexBuildSeedPageResult {
                disposition: IndexBuildSeedDisposition::Replay,
                inserted_facts: last.inserted_facts,
                skipped_touched_facts: last.skipped_touched_facts,
                progress: state.progress(),
            });
        }
        if state.done() {
            return Err(IndexError::IndexBuildAlreadyDone);
        }
        let progress = state.progress();
        if request.page_sequence != state.next_page_sequence
            || progress.expected_shard_id != Some(request.shard_id)
            || request.expected_cursor != state.cursor
        {
            return Err(IndexError::StaleIndexBuildProgress);
        }

        let mut subjects = BTreeSet::new();
        let mut prepared = Vec::with_capacity(request.facts.len());
        for fact in request.facts.iter().cloned() {
            let (subject, posting) = prepare_fact_posting(
                request.physical_index_id,
                &state.scope.target,
                request.shard_id,
                fact,
            )?;
            if !subjects.insert(subject) {
                return Err(IndexError::DuplicateIndexBuildSubject);
            }
            prepared.push((subject, posting));
        }

        let mut inserted = Vec::with_capacity(prepared.len());
        let mut skipped_touched = 0u32;
        INDEX_BUILD_TOUCHED_SUBJECTS.with_borrow(|touched| {
            for (subject, posting) in prepared {
                if touched.contains(&IndexBuildTouchedKey::new(
                    request.physical_index_id,
                    subject,
                )) {
                    skipped_touched = skipped_touched
                        .checked_add(1)
                        .expect("page row bound fits u32");
                } else {
                    inserted.push(posting);
                }
            }
        });

        let inserted_facts =
            u32::try_from(inserted.len()).map_err(|_| IndexError::IndexBuildProgressOverflow)?;
        state.seeded_items = state
            .seeded_items
            .checked_add(u64::from(inserted_facts))
            .ok_or(IndexError::IndexBuildProgressOverflow)?;
        state.next_page_sequence = state
            .next_page_sequence
            .checked_add(1)
            .ok_or(IndexError::IndexBuildProgressOverflow)?;
        if request.done {
            state.next_shard_index = state
                .next_shard_index
                .checked_add(1)
                .ok_or(IndexError::IndexBuildProgressOverflow)?;
            state.cursor = None;
        } else {
            state.cursor = request.next_cursor.clone();
        }
        let result = IndexBuildSeedPageResult {
            disposition: IndexBuildSeedDisposition::Applied,
            inserted_facts,
            skipped_touched_facts: skipped_touched,
            progress: state.progress(),
        };
        state.last_page = Some(IndexBuildLastPage {
            sequence: request.page_sequence,
            fingerprint,
            inserted_facts,
            skipped_touched_facts: skipped_touched,
        });

        // No fallible operation follows the first stable write. Posting writes and the durable
        // cursor/receipt advance commit or roll back together in this single serialized message.
        for posting in inserted {
            insert_prepared(posting);
        }
        INDEX_BUILD_STATES.with_borrow_mut(|states| {
            states.insert(request.physical_index_id, state);
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use candid::Principal;
    use gleaph_graph_kernel::canonical_export::CanonicalExportTarget;
    use gleaph_graph_kernel::canonical_export::CanonicalIndexableFact;
    use gleaph_graph_kernel::canonical_export::CanonicalRecordSource;
    use gleaph_graph_kernel::entry::{
        EdgeDirectedness, EdgeLabelId, GraphId, IndexNameId, PropertyId,
    };
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{
        EdgeIndexDirection, IndexBuildCleanupStatus, IndexBuildControlRequest,
        IndexBuildDmlRequest, IndexBuildPhase, IndexBuildSealRequest, IndexBuildSealTarget,
        IndexBuildSeedDisposition, IndexBuildSeedPageRequest, IndexBuildSubject, IndexBuildTarget,
        PhysicalIndexId, RegisterIndexBuildRequest,
    };
    use ic_stable_structures::Storable;

    use super::*;
    use crate::facade::stable::INDEX_BUILD_TOUCHED_SUBJECTS;
    use crate::init::IndexInitArgs;

    const GRAPH_ID: GraphId = GraphId::from_raw(1);
    const PROPERTY_ID: PropertyId = PropertyId::from_raw(42);
    const ANCESTOR_PROPERTY_ID: PropertyId = PropertyId::from_raw(43);
    const VERTEX_LABEL_ID: u16 = 8;
    const EDGE_LABEL_ID: u16 = 9;

    fn physical(raw: u64) -> PhysicalIndexId {
        PhysicalIndexId::new(raw).expect("test physical index id is non-zero")
    }

    /// A well-formed nested vertex target rooted at a different ancestor property.
    fn nested_record_source() -> Option<CanonicalRecordSource> {
        Some(CanonicalRecordSource {
            ancestor_property_id: ANCESTOR_PROPERTY_ID,
            field_tail: "meta.deep".to_owned(),
        })
    }

    fn nested_vertex_registration(
        physical_index_id: PhysicalIndexId,
        target_shard_ids: Vec<u32>,
    ) -> RegisterIndexBuildRequest {
        let mut registration = vertex_registration(physical_index_id, target_shard_ids);
        registration.target = IndexBuildTarget::Vertex {
            label_id: VERTEX_LABEL_ID,
            property_id: PROPERTY_ID,
            record_source: nested_record_source(),
        };
        registration
    }

    fn setup() -> (IndexStore, Principal, Principal, Principal) {
        let store = IndexStore::new();
        let router = Principal::from_slice(&[91]);
        let shard0 = Principal::from_slice(&[92]);
        let shard1 = Principal::from_slice(&[93]);
        store
            .init_from_args(&IndexInitArgs {
                router_canister: router,
            })
            .expect("initialize graph-index");
        for (shard_id, principal) in [(ShardId::new(0), shard0), (ShardId::new(1), shard1)] {
            store
                .admin_attach_shard_canister(router, GRAPH_ID, 2, 0, shard_id, principal)
                .expect("attach test shard");
        }
        (store, router, shard0, shard1)
    }

    fn vertex_registration(
        physical_index_id: PhysicalIndexId,
        target_shard_ids: Vec<u32>,
    ) -> RegisterIndexBuildRequest {
        RegisterIndexBuildRequest {
            physical_index_id,
            graph_id: GRAPH_ID,
            index_name_id: IndexNameId::from_raw(7),
            catalog_epoch: 11,
            topology_epoch: 17,
            target: IndexBuildTarget::Vertex {
                label_id: VERTEX_LABEL_ID,
                property_id: PROPERTY_ID,
                record_source: None,
            },
            target_shard_ids,
        }
    }

    fn edge_registration(physical_index_id: PhysicalIndexId) -> RegisterIndexBuildRequest {
        edge_registration_with(
            physical_index_id,
            EDGE_LABEL_ID,
            EdgeIndexDirection::Outgoing,
        )
    }

    /// `label_id` is the Router-registered catalog label; `direction` the ADR 0012 subset.
    fn edge_registration_with(
        physical_index_id: PhysicalIndexId,
        label_id: u16,
        direction: EdgeIndexDirection,
    ) -> RegisterIndexBuildRequest {
        RegisterIndexBuildRequest {
            physical_index_id,
            graph_id: GRAPH_ID,
            index_name_id: IndexNameId::from_raw(8),
            catalog_epoch: 11,
            topology_epoch: 17,
            target: IndexBuildTarget::Edge {
                label_id,
                property_id: PROPERTY_ID,
                direction,
            },
            target_shard_ids: vec![0],
        }
    }

    fn control(registration: RegisterIndexBuildRequest) -> IndexBuildControlRequest {
        IndexBuildControlRequest { registration }
    }

    fn vertex_fact(vertex_id: u32, value: &[u8]) -> CanonicalIndexableFact {
        CanonicalIndexableFact::Vertex {
            vertex_id,
            property_id: PROPERTY_ID,
            encoded_value: value.to_vec(),
        }
    }

    /// `label_id` is the exact wire value the Graph export emitted for the fact.
    fn edge_fact_with_label(
        owner_vertex_id: u32,
        slot_index: u32,
        value: &[u8],
        label_id: u16,
    ) -> CanonicalIndexableFact {
        CanonicalIndexableFact::Edge {
            owner_vertex_id,
            label_id,
            slot_index,
            property_id: PROPERTY_ID,
            encoded_value: value.to_vec(),
        }
    }

    fn seed_request(
        physical_index_id: PhysicalIndexId,
        sequence: u64,
        shard_id: u32,
        expected_cursor: Option<Vec<u8>>,
        facts: Vec<CanonicalIndexableFact>,
        next_cursor: Option<Vec<u8>>,
        done: bool,
    ) -> IndexBuildSeedPageRequest {
        IndexBuildSeedPageRequest {
            physical_index_id,
            catalog_epoch: 11,
            page_sequence: sequence,
            shard_id,
            expected_cursor,
            facts,
            next_cursor,
            done,
        }
    }

    #[test]
    fn dml_first_marks_vertex_touched_and_stale_seed_cannot_overwrite_it() {
        let (store, router, shard, _) = setup();
        let physical_index_id = physical(1_001);
        store
            .register_index_build(router, &vertex_registration(physical_index_id, vec![0]))
            .expect("register vertex build");
        let subject = IndexBuildSubject::Vertex {
            shard_id: 0,
            vertex_id: 5,
        };
        store
            .apply_index_build_dml(
                shard,
                &IndexBuildDmlRequest {
                    physical_index_id,
                    catalog_epoch: 11,
                    shard_sequence: 1,
                    subject,
                    removals: vec![b"old".to_vec()],
                    insertions: vec![b"new".to_vec()],
                },
            )
            .expect("apply exact DML");

        let seeded = store
            .seed_index_build_page(&seed_request(
                physical_index_id,
                0,
                0,
                None,
                vec![vertex_fact(5, b"old")],
                None,
                true,
            ))
            .expect("seed stale base page");
        assert_eq!(seeded.inserted_facts, 0);
        assert_eq!(seeded.skipped_touched_facts, 1);
        assert!(
            store
                .lookup_equal(physical_index_id, PROPERTY_ID.raw(), b"old")
                .expect("lookup old")
                .is_empty()
        );
        assert_eq!(
            store
                .lookup_equal(physical_index_id, PROPERTY_ID.raw(), b"new")
                .expect("lookup new")
                .len(),
            1
        );
        assert!(INDEX_BUILD_TOUCHED_SUBJECTS.with_borrow(|touched| {
            touched.contains(&IndexBuildTouchedKey::new(physical_index_id, subject))
        }));
    }

    #[test]
    fn seed_first_then_edge_dml_removes_stale_posting_and_inserts_correction() {
        let (store, router, shard, _) = setup();
        let physical_index_id = physical(1_002);
        let directed_wire = EdgeLabelId::from_raw(EDGE_LABEL_ID)
            .pack(EdgeDirectedness::Directed)
            .raw();
        store
            .register_index_build(router, &edge_registration(physical_index_id))
            .expect("register edge build");
        store
            .seed_index_build_page(&seed_request(
                physical_index_id,
                0,
                0,
                None,
                vec![edge_fact_with_label(6, 3, b"old", directed_wire)],
                None,
                true,
            ))
            .expect("seed edge base");

        let subject = IndexBuildSubject::Edge {
            shard_id: 0,
            owner_vertex_id: 6,
            label_id: directed_wire,
            slot_index: 3,
        };
        store
            .apply_index_build_dml(
                shard,
                &IndexBuildDmlRequest {
                    physical_index_id,
                    catalog_epoch: 11,
                    shard_sequence: 1,
                    subject,
                    removals: vec![b"old".to_vec()],
                    insertions: vec![b"new".to_vec()],
                },
            )
            .expect("correct edge posting after base scan completed");

        assert!(
            store
                .lookup_edge_equal(physical_index_id, PROPERTY_ID.raw(), b"old", None)
                .expect("lookup old edge")
                .is_empty()
        );
        assert_eq!(
            store
                .lookup_edge_equal(physical_index_id, PROPERTY_ID.raw(), b"new", None)
                .expect("lookup corrected edge")
                .len(),
            1
        );
    }

    // --- edge posting label identity contract (GAP-2026-08-22-001) ---

    /// Registrations speak catalog ids; Graph facts arrive wire-tagged. The seed
    /// identity check must compare the fact's catalog index with the registered
    /// label and store the posting under the fact's wire tag, and the equality
    /// sieve must find wire-keyed postings from a catalog-id request.
    #[test]
    fn directed_wire_fact_seeds_under_catalog_target_and_catalog_sieve_finds_it() {
        let (store, router, _shard0, _shard1) = setup();
        let physical_index_id = physical(1_004);
        let directed_wire = EdgeLabelId::from_raw(EDGE_LABEL_ID)
            .pack(EdgeDirectedness::Directed)
            .raw();
        assert_ne!(
            directed_wire, EDGE_LABEL_ID,
            "precondition: the two identity spaces diverge for directed buckets"
        );

        store
            .register_index_build(
                router,
                &edge_registration_with(
                    physical_index_id,
                    EDGE_LABEL_ID,
                    EdgeIndexDirection::Outgoing,
                ),
            )
            .expect("register directed edge build");

        store
            .seed_index_build_page(&seed_request(
                physical_index_id,
                0,
                0,
                None,
                vec![edge_fact_with_label(6, 3, b"v9", directed_wire)],
                None,
                true,
            ))
            .expect("a directed wire-tagged fact must seed under its catalog target");

        let hits = store
            .lookup_edge_equal(
                physical_index_id,
                PROPERTY_ID.raw(),
                b"v9",
                Some(EDGE_LABEL_ID),
            )
            .expect("catalog-label sieve must reach wire-keyed postings");
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].label_id, directed_wire,
            "stored postings keep the wire tag so read binding resolves the LARA bucket"
        );
        assert!(
            store
                .lookup_edge_equal(physical_index_id, PROPERTY_ID.raw(), b"v9", Some(8))
                .expect("sieve by a different catalog label")
                .is_empty()
        );
    }

    #[test]
    fn any_direction_target_accepts_both_bucket_packings_of_its_catalog_label() {
        let (store, router, _shard0, _shard1) = setup();
        let physical_index_id = physical(1_005);
        let directed_wire = EdgeLabelId::from_raw(EDGE_LABEL_ID)
            .pack(EdgeDirectedness::Directed)
            .raw();

        store
            .register_index_build(
                router,
                &edge_registration_with(physical_index_id, EDGE_LABEL_ID, EdgeIndexDirection::Any),
            )
            .expect("register any-direction edge build");

        store
            .seed_index_build_page(&seed_request(
                physical_index_id,
                0,
                0,
                None,
                vec![
                    edge_fact_with_label(5, 0, b"da", directed_wire),
                    edge_fact_with_label(7, 1, b"ua", EDGE_LABEL_ID),
                ],
                None,
                true,
            ))
            .expect("both bucket packings of the registered label seed under an Any target");

        let mut labels: Vec<u16> = store
            .lookup_edge_equal(
                physical_index_id,
                PROPERTY_ID.raw(),
                b"da",
                Some(EDGE_LABEL_ID),
            )
            .expect("directed packing sieve")
            .into_iter()
            .map(|hit| hit.label_id)
            .collect();
        labels.extend(
            store
                .lookup_edge_equal(
                    physical_index_id,
                    PROPERTY_ID.raw(),
                    b"ua",
                    Some(EDGE_LABEL_ID),
                )
                .expect("undirected packing sieve")
                .into_iter()
                .map(|hit| hit.label_id),
        );
        assert!(labels.contains(&directed_wire));
        assert!(labels.contains(&EDGE_LABEL_ID));
    }

    #[test]
    fn facts_from_a_different_catalog_label_are_rejected_and_store_nothing() {
        let (store, router, _shard0, _shard1) = setup();
        let physical_index_id = physical(1_006);
        const OTHER_CATALOG: u16 = 10;
        let other_directed_wire = EdgeLabelId::from_raw(OTHER_CATALOG)
            .pack(EdgeDirectedness::Directed)
            .raw();

        store
            .register_index_build(
                router,
                &edge_registration_with(
                    physical_index_id,
                    EDGE_LABEL_ID,
                    EdgeIndexDirection::Outgoing,
                ),
            )
            .expect("register directed edge build");

        for (space, fact) in [
            ("catalog", edge_fact_with_label(5, 0, b"x", OTHER_CATALOG)),
            (
                "wire",
                edge_fact_with_label(6, 0, b"y", other_directed_wire),
            ),
        ] {
            let err = store
                .seed_index_build_page(&seed_request(
                    physical_index_id,
                    0,
                    0,
                    None,
                    vec![fact],
                    None,
                    true,
                ))
                .expect_err(&format!("a {space}-foreign label must be rejected"));
            assert!(
                matches!(err, IndexError::InvalidIndexBuildTarget),
                "expected InvalidIndexBuildTarget for the {space}-foreign label, got {err:?}"
            );
        }
        assert!(
            store
                .lookup_edge_equal(physical_index_id, PROPERTY_ID.raw(), b"x", None)
                .expect("lookup after rejections")
                .is_empty(),
            "rejected facts must not leave postings behind"
        );
    }

    #[test]
    fn dml_build_subjects_carry_wire_labels_against_catalog_targets() {
        let (store, router, shard, _shard1) = setup();
        let physical_index_id = physical(1_007);
        let directed_wire = EdgeLabelId::from_raw(EDGE_LABEL_ID)
            .pack(EdgeDirectedness::Directed)
            .raw();

        store
            .register_index_build(router, &edge_registration(physical_index_id))
            .expect("register edge build");
        store
            .seed_index_build_page(&seed_request(
                physical_index_id,
                0,
                0,
                None,
                vec![edge_fact_with_label(6, 3, b"old", directed_wire)],
                None,
                true,
            ))
            .expect("seed directed-wire base page");

        store
            .apply_index_build_dml(
                shard,
                &IndexBuildDmlRequest {
                    physical_index_id,
                    catalog_epoch: 11,
                    shard_sequence: 1,
                    subject: IndexBuildSubject::Edge {
                        shard_id: 0,
                        owner_vertex_id: 6,
                        label_id: directed_wire,
                        slot_index: 3,
                    },
                    removals: vec![b"old".to_vec()],
                    insertions: vec![b"new".to_vec()],
                },
            )
            .expect("a wire-tagged DML subject must match its catalog target");

        assert!(
            store
                .lookup_edge_equal(
                    physical_index_id,
                    PROPERTY_ID.raw(),
                    b"old",
                    Some(EDGE_LABEL_ID)
                )
                .expect("lookup removed value")
                .is_empty()
        );
        assert_eq!(
            store
                .lookup_edge_equal(
                    physical_index_id,
                    PROPERTY_ID.raw(),
                    b"new",
                    Some(EDGE_LABEL_ID)
                )
                .expect("lookup inserted value")
                .len(),
            1
        );
    }

    #[test]
    fn exact_seed_and_dml_replay_are_idempotent_and_conflicting_page_replay_rejects() {
        let (store, router, shard, _) = setup();
        let physical_index_id = physical(1_003);
        store
            .register_index_build(router, &vertex_registration(physical_index_id, vec![0]))
            .expect("register build");
        let page = seed_request(
            physical_index_id,
            0,
            0,
            None,
            vec![vertex_fact(7, b"seed")],
            None,
            true,
        );
        let applied = store.seed_index_build_page(&page).expect("apply page");
        let replayed = store.seed_index_build_page(&page).expect("replay page");
        assert_eq!(applied.disposition, IndexBuildSeedDisposition::Applied);
        assert_eq!(replayed.disposition, IndexBuildSeedDisposition::Replay);
        assert_eq!(replayed.progress, applied.progress);
        assert_eq!(replayed.inserted_facts, applied.inserted_facts);

        let mut conflict = page.clone();
        conflict.facts[0] = vertex_fact(7, b"different");
        assert_eq!(
            store.seed_index_build_page(&conflict),
            Err(IndexError::IndexBuildReplayConflict)
        );

        let dml = IndexBuildDmlRequest {
            physical_index_id,
            catalog_epoch: 11,
            shard_sequence: 1,
            subject: IndexBuildSubject::Vertex {
                shard_id: 0,
                vertex_id: 7,
            },
            removals: vec![b"seed".to_vec()],
            insertions: vec![b"live".to_vec()],
        };
        store.apply_index_build_dml(shard, &dml).expect("apply DML");
        store
            .apply_index_build_dml(shard, &dml)
            .expect("exact replay DML");
        assert_eq!(
            store
                .lookup_equal(physical_index_id, PROPERTY_ID.raw(), b"live")
                .expect("lookup live")
                .len(),
            1
        );
    }

    #[test]
    fn touched_subjects_and_seed_postings_are_isolated_by_physical_namespace() {
        let (store, router, shard, _) = setup();
        let first = physical(1_004);
        let second = physical(1_005);
        for id in [first, second] {
            store
                .register_index_build(router, &vertex_registration(id, vec![0]))
                .expect("register namespace");
        }
        let subject = IndexBuildSubject::Vertex {
            shard_id: 0,
            vertex_id: 8,
        };
        store
            .apply_index_build_dml(
                shard,
                &IndexBuildDmlRequest {
                    physical_index_id: first,
                    catalog_epoch: 11,
                    shard_sequence: 1,
                    subject,
                    removals: Vec::new(),
                    insertions: vec![b"live".to_vec()],
                },
            )
            .expect("touch first namespace");
        for id in [first, second] {
            store
                .seed_index_build_page(&seed_request(
                    id,
                    0,
                    0,
                    None,
                    vec![vertex_fact(8, b"base")],
                    None,
                    true,
                ))
                .expect("seed namespace");
        }
        assert!(
            store
                .lookup_equal(first, PROPERTY_ID.raw(), b"base")
                .expect("first lookup")
                .is_empty()
        );
        assert_eq!(
            store
                .lookup_equal(second, PROPERTY_ID.raw(), b"base")
                .expect("second lookup")
                .len(),
            1
        );
    }

    #[test]
    fn stale_epoch_wrong_scope_and_duplicate_page_fail_without_mutation() {
        let (store, router, shard, _) = setup();
        let physical_index_id = physical(1_006);
        store
            .register_index_build(router, &vertex_registration(physical_index_id, vec![0]))
            .expect("register build");
        let subject = IndexBuildSubject::Vertex {
            shard_id: 0,
            vertex_id: 9,
        };
        let stale_dml = IndexBuildDmlRequest {
            physical_index_id,
            catalog_epoch: 10,
            shard_sequence: 1,
            subject,
            removals: Vec::new(),
            insertions: vec![b"bad".to_vec()],
        };
        assert_eq!(
            store.apply_index_build_dml(shard, &stale_dml),
            Err(IndexError::StaleIndexBuildEpoch)
        );
        assert!(!INDEX_BUILD_TOUCHED_SUBJECTS.with_borrow(|touched| {
            touched.contains(&IndexBuildTouchedKey::new(physical_index_id, subject))
        }));

        let wrong_scope_subject = IndexBuildSubject::Edge {
            shard_id: 0,
            owner_vertex_id: 9,
            label_id: EDGE_LABEL_ID,
            slot_index: 0,
        };
        let wrong_scope_dml = IndexBuildDmlRequest {
            physical_index_id,
            catalog_epoch: 11,
            shard_sequence: 1,
            subject: wrong_scope_subject,
            removals: Vec::new(),
            insertions: vec![b"wrong-scope".to_vec()],
        };
        assert_eq!(
            store.apply_index_build_dml(shard, &wrong_scope_dml),
            Err(IndexError::InvalidIndexBuildTarget)
        );
        assert!(!INDEX_BUILD_TOUCHED_SUBJECTS.with_borrow(|touched| {
            touched.contains(&IndexBuildTouchedKey::new(
                physical_index_id,
                wrong_scope_subject,
            ))
        }));
        assert!(
            store
                .lookup_edge_equal(physical_index_id, PROPERTY_ID.raw(), b"wrong-scope", None,)
                .expect("lookup rejected wrong-scope value")
                .is_empty()
        );

        let duplicate = seed_request(
            physical_index_id,
            0,
            0,
            None,
            vec![vertex_fact(9, b"one"), vertex_fact(9, b"two")],
            None,
            true,
        );
        assert_eq!(
            store.seed_index_build_page(&duplicate),
            Err(IndexError::DuplicateIndexBuildSubject)
        );
        let status = store
            .index_build_status(router, physical_index_id)
            .expect("read unchanged progress");
        assert_eq!(status.progress.next_page_sequence, 0);
        assert_eq!(status.progress.seeded_items, 0);
        for value in [b"bad".as_slice(), b"one".as_slice(), b"two".as_slice()] {
            assert!(
                store
                    .lookup_equal(physical_index_id, PROPERTY_ID.raw(), value)
                    .expect("lookup rejected value")
                    .is_empty()
            );
        }
    }

    #[test]
    fn empty_dml_with_wrong_subject_errors_without_touching_state() {
        let (store, router, shard, _) = setup();
        let vertex_physical = physical(1_020);
        store
            .register_index_build(router, &vertex_registration(vertex_physical, vec![0]))
            .expect("register vertex build");
        let edge_physical = physical(1_021);
        store
            .register_index_build(router, &edge_registration(edge_physical))
            .expect("register edge build");

        // Empty DML with the wrong subject kind: an edge subject against a vertex target.
        let wrong_kind_subject = IndexBuildSubject::Edge {
            shard_id: 0,
            owner_vertex_id: 40,
            label_id: EDGE_LABEL_ID,
            slot_index: 0,
        };
        let wrong_kind = IndexBuildDmlRequest {
            physical_index_id: vertex_physical,
            catalog_epoch: 11,
            shard_sequence: 1,
            subject: wrong_kind_subject,
            removals: Vec::new(),
            insertions: Vec::new(),
        };
        assert_eq!(
            store.apply_index_build_dml(shard, &wrong_kind),
            Err(IndexError::InvalidIndexBuildTarget)
        );
        assert!(!INDEX_BUILD_TOUCHED_SUBJECTS.with_borrow(|touched| {
            touched.contains(&IndexBuildTouchedKey::new(
                vertex_physical,
                wrong_kind_subject,
            ))
        }));
        let vertex_status = store
            .index_build_status(router, vertex_physical)
            .expect("vertex status unchanged");
        assert_eq!(vertex_status.progress.next_page_sequence, 0);
        assert_eq!(vertex_status.progress.seeded_items, 0);
        assert_eq!(vertex_status.watermarks[0].drained_through, 0);

        // Empty DML with the wrong edge label against an edge target.
        let wrong_label_subject = IndexBuildSubject::Edge {
            shard_id: 0,
            owner_vertex_id: 41,
            label_id: VERTEX_LABEL_ID,
            slot_index: 0,
        };
        let wrong_label = IndexBuildDmlRequest {
            physical_index_id: edge_physical,
            catalog_epoch: 11,
            shard_sequence: 1,
            subject: wrong_label_subject,
            removals: Vec::new(),
            insertions: Vec::new(),
        };
        assert_eq!(
            store.apply_index_build_dml(shard, &wrong_label),
            Err(IndexError::InvalidIndexBuildTarget)
        );
        assert!(!INDEX_BUILD_TOUCHED_SUBJECTS.with_borrow(|touched| {
            touched.contains(&IndexBuildTouchedKey::new(
                edge_physical,
                wrong_label_subject,
            ))
        }));
        let edge_status = store
            .index_build_status(router, edge_physical)
            .expect("edge status unchanged");
        assert_eq!(edge_status.watermarks[0].drained_through, 0);
    }

    #[test]
    fn empty_dml_with_correct_subject_acknowledges_and_blocks_stale_seed() {
        let (store, router, shard, _) = setup();
        let physical_index_id = physical(1_022);
        store
            .register_index_build(router, &vertex_registration(physical_index_id, vec![0]))
            .expect("register vertex build");
        let subject = IndexBuildSubject::Vertex {
            shard_id: 0,
            vertex_id: 42,
        };
        let empty = IndexBuildDmlRequest {
            physical_index_id,
            catalog_epoch: 11,
            shard_sequence: 1,
            subject,
            removals: Vec::new(),
            insertions: Vec::new(),
        };
        // Subject-vs-target is validated before any stable write, so an empty correction for the
        // right subject may advance the acknowledgement: the touched marker still stops the base
        // seed from restoring postings for the corrected subject.
        store
            .apply_index_build_dml(shard, &empty)
            .expect("empty correction acknowledges");
        assert!(INDEX_BUILD_TOUCHED_SUBJECTS.with_borrow(|touched| {
            touched.contains(&IndexBuildTouchedKey::new(physical_index_id, subject))
        }));
        let status = store
            .index_build_status(router, physical_index_id)
            .expect("acknowledged status");
        assert_eq!(status.watermarks[0].drained_through, 1);
        assert_eq!(status.progress.seeded_items, 0);

        // The touched marker prevents a later stale base page from restoring postings for the
        // empty-corrected subject (touched-first convergence).
        let seeded = store
            .seed_index_build_page(&seed_request(
                physical_index_id,
                0,
                0,
                None,
                vec![vertex_fact(42, b"stale")],
                None,
                true,
            ))
            .expect("seed stale base page");
        assert_eq!(seeded.inserted_facts, 0);
        assert_eq!(seeded.skipped_touched_facts, 1);
        assert!(
            store
                .lookup_equal(physical_index_id, PROPERTY_ID.raw(), b"stale")
                .expect("lookup stale")
                .is_empty()
        );
    }

    #[test]
    fn register_index_build_rejects_zero_label_and_leaves_build_map_unchanged() {
        let (store, router, _, _) = setup();
        let vertex_physical = physical(1_023);
        let mut vertex = vertex_registration(vertex_physical, vec![0]);
        vertex.target = IndexBuildTarget::Vertex {
            label_id: 0,
            property_id: PROPERTY_ID,
            record_source: None,
        };
        assert_eq!(
            store.register_index_build(router, &vertex),
            Err(IndexError::InvalidIndexBuildScope)
        );
        assert_eq!(
            store.index_build_status(router, vertex_physical),
            Err(IndexError::UnknownIndexBuild)
        );
        assert!(INDEX_BUILD_STATES.with_borrow(|states| states.get(&vertex_physical).is_none()));

        let edge_physical = physical(1_024);
        let mut edge = edge_registration(edge_physical);
        edge.target = IndexBuildTarget::Edge {
            label_id: 0,
            property_id: PROPERTY_ID,
            direction: EdgeIndexDirection::Outgoing,
        };
        assert_eq!(
            store.register_index_build(router, &edge),
            Err(IndexError::InvalidIndexBuildScope)
        );
        assert_eq!(
            store.index_build_status(router, edge_physical),
            Err(IndexError::UnknownIndexBuild)
        );
        assert!(INDEX_BUILD_STATES.with_borrow(|states| states.get(&edge_physical).is_none()));
    }

    #[test]
    fn register_rejects_text_targets_and_the_export_projection_keeps_label_and_property() {
        let (store, router, _, _) = setup();
        let text_physical = physical(1_025);
        let mut text = vertex_registration(text_physical, vec![0]);
        text.target = IndexBuildTarget::Text {
            label_id: VERTEX_LABEL_ID,
            property_id: PROPERTY_ID,
            analyzer_id: 1,
        };
        assert_eq!(
            store.register_index_build(router, &text),
            Err(IndexError::InvalidIndexBuildScope)
        );
        assert_eq!(
            store.index_build_status(router, text_physical),
            Err(IndexError::UnknownIndexBuild)
        );
        assert!(INDEX_BUILD_STATES.with_borrow(|states| states.get(&text_physical).is_none()));

        // Pure projection: the export scope echoes the label and property without the
        // analyzer id (ADR 0059 §Text build kind).
        assert_eq!(
            canonical_target(&text.target),
            CanonicalExportTarget::Text {
                label_id: VERTEX_LABEL_ID,
                property_id: PROPERTY_ID,
            }
        );
    }

    #[test]
    fn text_targets_fail_closed_against_every_subject_kind() {
        let target = IndexBuildTarget::Text {
            label_id: VERTEX_LABEL_ID,
            property_id: PROPERTY_ID,
            analyzer_id: 1,
        };
        assert_eq!(
            ensure_subject_matches_target(
                &target,
                IndexBuildSubject::Vertex {
                    shard_id: 0,
                    vertex_id: 1,
                }
            ),
            Err(IndexError::InvalidIndexBuildTarget)
        );
        assert_eq!(
            ensure_subject_matches_target(
                &target,
                IndexBuildSubject::Edge {
                    shard_id: 0,
                    owner_vertex_id: 1,
                    label_id: VERTEX_LABEL_ID,
                    slot_index: 0,
                }
            ),
            Err(IndexError::InvalidIndexBuildTarget)
        );
    }

    #[test]
    fn cursor_and_shard_progress_advance_atomically_with_seed_writes() {
        let (store, router, _, _) = setup();
        let physical_index_id = physical(1_007);
        store
            .register_index_build(router, &vertex_registration(physical_index_id, vec![0, 1]))
            .expect("register two-shard build");
        let first = seed_request(
            physical_index_id,
            0,
            0,
            None,
            vec![vertex_fact(10, b"a")],
            Some(vec![1]),
            false,
        );
        let first_result = store.seed_index_build_page(&first).expect("first page");
        assert_eq!(first_result.progress.next_page_sequence, 1);
        assert_eq!(first_result.progress.expected_shard_id, Some(0));
        assert_eq!(first_result.progress.cursor, Some(vec![1]));
        assert_eq!(
            store
                .seed_index_build_page(&first)
                .expect("lost-reply replay")
                .disposition,
            IndexBuildSeedDisposition::Replay
        );

        let second = seed_request(
            physical_index_id,
            1,
            0,
            Some(vec![1]),
            vec![vertex_fact(11, b"b")],
            None,
            true,
        );
        let second_result = store
            .seed_index_build_page(&second)
            .expect("finish shard zero");
        assert_eq!(second_result.progress.next_page_sequence, 2);
        assert_eq!(second_result.progress.next_shard_index, 1);
        assert_eq!(second_result.progress.expected_shard_id, Some(1));
        assert_eq!(second_result.progress.cursor, None);
        assert!(!second_result.progress.done);

        let third = seed_request(
            physical_index_id,
            2,
            1,
            None,
            vec![vertex_fact(12, b"c")],
            None,
            true,
        );
        let third_result = store.seed_index_build_page(&third).expect("finish build");
        assert!(third_result.progress.done);
        assert_eq!(third_result.progress.expected_shard_id, None);
        for value in [b"a".as_slice(), b"b".as_slice(), b"c".as_slice()] {
            assert_eq!(
                store
                    .lookup_equal(physical_index_id, PROPERTY_ID.raw(), value)
                    .expect("lookup committed page")
                    .len(),
                1
            );
        }
    }

    #[test]
    fn build_state_and_touched_set_reopen_in_separate_stable_regions() {
        let (store, router, shard, _) = setup();
        let physical_index_id = physical(1_008);
        let registration = nested_vertex_registration(physical_index_id, vec![0]);
        store
            .register_index_build(router, &registration)
            .expect("register build");
        let subject = IndexBuildSubject::Vertex {
            shard_id: 0,
            vertex_id: 13,
        };
        store
            .apply_index_build_dml(
                shard,
                &IndexBuildDmlRequest {
                    physical_index_id,
                    catalog_epoch: 11,
                    shard_sequence: 1,
                    subject,
                    removals: Vec::new(),
                    insertions: vec![b"live".to_vec()],
                },
            )
            .expect("persist touched subject");

        let reopened_states = crate::facade::stable::memory::init_index_build_states();
        let reopened_touched = crate::facade::stable::memory::init_index_build_touched_subjects();
        assert_eq!(reopened_states.len(), 1);
        assert_eq!(reopened_touched.len(), 1);
        let reopened_state = reopened_states
            .get(&physical_index_id)
            .expect("reopen build state");
        assert_eq!(reopened_state.scope.catalog_epoch, 11);
        assert_eq!(
            reopened_state.scope.target,
            IndexBuildTarget::Vertex {
                label_id: VERTEX_LABEL_ID,
                property_id: PROPERTY_ID,
                record_source: nested_record_source(),
            },
            "the exact nested-target Candid state survives a stable reopen"
        );
        assert!(reopened_touched.contains(&IndexBuildTouchedKey::new(physical_index_id, subject)));
        assert_ne!(
            Storable::into_bytes(reopened_state).len(),
            Storable::into_bytes(IndexBuildTouchedKey::new(physical_index_id, subject)).len(),
            "state and touched records have distinct storage shapes and regions"
        );
    }

    #[test]
    fn nested_vertex_record_source_replays_exact_export_and_registration() {
        let (store, router, shard0, _) = setup();
        let physical_index_id = physical(1_009);
        let registration = nested_vertex_registration(physical_index_id, vec![0]);
        let status = store
            .register_index_build(router, &registration)
            .expect("register nested build");
        assert_eq!(
            status.registration.target, registration.target,
            "the durable status echoes the exact nested target"
        );
        // Exact replay of the same registration is idempotent; no alternate shape exists.
        let replayed = store
            .register_index_build(router, &registration)
            .expect("exact nested replay");
        assert_eq!(replayed.registration.target, registration.target);

        let pull = store
            .prepare_index_build_pull(router, &control(registration.clone()))
            .expect("prepared export pull")
            .expect("building scope still has base pages");
        assert_eq!(pull.graph_canister, shard0);
        assert_eq!(
            pull.export.target,
            CanonicalExportTarget::Vertex {
                label_id: VERTEX_LABEL_ID,
                property_id: PROPERTY_ID,
                record_source: nested_record_source(),
            },
            "the prepared Graph pull carries the exact nested record_source"
        );
        assert_eq!(pull.export.graph_id, GRAPH_ID);
        assert_eq!(pull.shard_id, 0);

        // Reopen the stable region and require the exact Candid target again.
        let reopened = crate::facade::stable::memory::init_index_build_states()
            .get(&physical_index_id)
            .expect("reopen nested build state");
        assert_eq!(reopened.scope.target, registration.target);
    }

    #[test]
    fn nested_vertex_record_source_registration_rejects_invalid_sources_without_mutation() {
        let invalid_sources = [
            // Zero ancestor identity.
            Some(CanonicalRecordSource {
                ancestor_property_id: PropertyId::from_raw(0),
                field_tail: "score".to_owned(),
            }),
            // Empty tail.
            Some(CanonicalRecordSource {
                ancestor_property_id: ANCESTOR_PROPERTY_ID,
                field_tail: String::new(),
            }),
            // Empty path segment.
            Some(CanonicalRecordSource {
                ancestor_property_id: ANCESTOR_PROPERTY_ID,
                field_tail: "meta..deep".to_owned(),
            }),
            // The walk must be rooted at another property, not the leaf itself.
            Some(CanonicalRecordSource {
                ancestor_property_id: PROPERTY_ID,
                field_tail: "self".to_owned(),
            }),
        ];
        for source in invalid_sources {
            let (store, router, _, _) = setup();
            let physical_index_id = physical(1_010);
            let mut registration = nested_vertex_registration(physical_index_id, vec![0]);
            registration.target = IndexBuildTarget::Vertex {
                label_id: VERTEX_LABEL_ID,
                property_id: PROPERTY_ID,
                record_source: source,
            };
            assert_eq!(
                store.register_index_build(router, &registration),
                Err(IndexError::InvalidIndexBuildScope),
                "an invalid nested record source must reject before any stable mutation"
            );
            assert_eq!(
                store.index_build_status(router, physical_index_id),
                Err(IndexError::UnknownIndexBuild)
            );
            assert!(
                INDEX_BUILD_STATES.with_borrow(|states| states.get(&physical_index_id).is_none()),
                "a rejected registration leaves the build map unchanged"
            );
        }
    }

    #[test]
    fn seal_drains_only_contiguous_old_epoch_work_through_captured_targets() {
        let (store, router, shard0, shard1) = setup();
        let physical_index_id = physical(1_009);
        let registration = vertex_registration(physical_index_id, vec![0, 1]);
        store
            .register_index_build(router, &registration)
            .expect("register build");
        for (sequence, shard_id) in [(0, 0), (1, 1)] {
            store
                .seed_index_build_page(&seed_request(
                    physical_index_id,
                    sequence,
                    shard_id,
                    None,
                    Vec::new(),
                    None,
                    true,
                ))
                .expect("complete empty shard base");
        }
        let first = IndexBuildDmlRequest {
            physical_index_id,
            catalog_epoch: 11,
            shard_sequence: 1,
            subject: IndexBuildSubject::Vertex {
                shard_id: 0,
                vertex_id: 20,
            },
            removals: Vec::new(),
            insertions: vec![b"one".to_vec()],
        };
        store
            .apply_index_build_dml(shard0, &first)
            .expect("apply first admitted DML");

        let seal = IndexBuildSealRequest {
            control: control(registration.clone()),
            seal_catalog_epoch: 12,
            shard_targets: vec![
                IndexBuildSealTarget {
                    shard_id: 0,
                    admitted_through: 2,
                },
                IndexBuildSealTarget {
                    shard_id: 1,
                    admitted_through: 2,
                },
            ],
        };
        let initial = store
            .seal_index_build(router, &seal)
            .expect("register seal");
        assert!(initial.base_complete);
        assert_eq!(initial.watermarks[0].drained_through, 1);
        assert_eq!(initial.watermarks[1].drained_through, 0);
        assert_eq!(store.seal_index_build(router, &seal), Ok(initial));

        let gap = IndexBuildDmlRequest {
            physical_index_id,
            catalog_epoch: 11,
            shard_sequence: 2,
            subject: IndexBuildSubject::Vertex {
                shard_id: 1,
                vertex_id: 21,
            },
            removals: Vec::new(),
            insertions: vec![b"gap".to_vec()],
        };
        assert_eq!(
            store.apply_index_build_dml(shard1, &gap),
            Err(IndexError::IndexBuildSequenceGap)
        );

        let second = IndexBuildDmlRequest {
            physical_index_id,
            catalog_epoch: 11,
            shard_sequence: 2,
            subject: IndexBuildSubject::Vertex {
                shard_id: 0,
                vertex_id: 22,
            },
            removals: Vec::new(),
            insertions: vec![b"two".to_vec()],
        };
        store
            .apply_index_build_dml(shard0, &second)
            .expect("drain captured shard-zero target");
        let beyond_target = IndexBuildDmlRequest {
            shard_sequence: 3,
            ..second.clone()
        };
        assert_eq!(
            store.apply_index_build_dml(shard0, &beyond_target),
            Err(IndexError::StaleIndexBuildEpoch)
        );
        let shard_one = IndexBuildDmlRequest {
            shard_sequence: 1,
            ..gap.clone()
        };
        store
            .apply_index_build_dml(shard1, &shard_one)
            .expect("drain first shard-one sequence");
        store
            .apply_index_build_dml(shard1, &gap)
            .expect("drain captured shard-one target");
        store
            .apply_index_build_dml(shard1, &gap)
            .expect("exact final replay");
        let conflicting_replay = IndexBuildDmlRequest {
            insertions: vec![b"different".to_vec()],
            ..gap
        };
        assert_eq!(
            store.apply_index_build_dml(shard1, &conflicting_replay),
            Err(IndexError::IndexBuildReplayConflict)
        );

        let status = store
            .index_build_status(router, physical_index_id)
            .expect("sealed status");
        assert_eq!(
            status.phase,
            IndexBuildPhase::Sealing {
                seal_catalog_epoch: 12
            }
        );
        assert!(
            status
                .watermarks
                .iter()
                .all(|watermark| watermark.drained_through == watermark.admitted_through)
        );
    }

    #[test]
    fn abort_cleanup_is_namespace_isolated_bounded_and_reopens() {
        let (store, router, shard, _) = setup();
        let first = physical(1_010);
        let second = physical(1_011);
        let first_registration = edge_registration(first);
        let second_registration = edge_registration(second);
        for registration in [&first_registration, &second_registration] {
            store
                .register_index_build(router, registration)
                .expect("register build");
        }
        let directed_wire = EdgeLabelId::from_raw(EDGE_LABEL_ID)
            .pack(EdgeDirectedness::Directed)
            .raw();
        for (physical_index_id, value) in
            [(first, b"first".as_slice()), (second, b"second".as_slice())]
        {
            store
                .apply_index_build_dml(
                    shard,
                    &IndexBuildDmlRequest {
                        physical_index_id,
                        catalog_epoch: 11,
                        shard_sequence: 1,
                        subject: IndexBuildSubject::Edge {
                            shard_id: 0,
                            owner_vertex_id: 30,
                            label_id: directed_wire,
                            slot_index: 0,
                        },
                        removals: Vec::new(),
                        insertions: vec![value.to_vec()],
                    },
                )
                .expect("insert edge build DML");
            store
                .posting_insert(
                    shard,
                    ShardId::new(0),
                    physical_index_id,
                    PROPERTY_ID.raw(),
                    value.to_vec(),
                    31,
                )
                .expect("insert namespace vertex posting");
        }

        let control = control(first_registration);
        let first_step = store
            .abort_index_build_step_for_test(router, &control, 1)
            .expect("begin cleanup");
        assert!(!first_step.done);
        let reopened_midway = crate::facade::stable::memory::init_index_build_states()
            .get(&first)
            .expect("reopen cleanup cursor");
        assert!(matches!(
            reopened_midway.lifecycle,
            IndexBuildLifecycle::Aborting { .. }
        ));
        let mut done = false;
        for _ in 0..8 {
            done = store
                .abort_index_build_step_for_test(router, &control, 1)
                .expect("resume cleanup")
                .done;
            if done {
                break;
            }
        }
        assert!(done);
        assert!(
            store
                .lookup_equal(first, PROPERTY_ID.raw(), b"first")
                .expect("first vertex lookup")
                .is_empty()
        );
        assert!(
            store
                .lookup_edge_equal(first, PROPERTY_ID.raw(), b"first", None)
                .expect("first edge lookup")
                .is_empty()
        );
        assert_eq!(
            store
                .lookup_equal(second, PROPERTY_ID.raw(), b"second")
                .expect("second vertex lookup")
                .len(),
            1
        );
        assert_eq!(
            store
                .lookup_edge_equal(second, PROPERTY_ID.raw(), b"second", None)
                .expect("second edge lookup")
                .len(),
            1
        );
        let reopened_touched = crate::facade::stable::memory::init_index_build_touched_subjects();
        assert!(
            !reopened_touched
                .iter()
                .any(|key| key.physical_index_id == first)
        );
        assert!(
            reopened_touched
                .iter()
                .any(|key| key.physical_index_id == second)
        );
        let status = store
            .index_build_status(router, first)
            .expect("retained cleanup receipt");
        assert_eq!(status.phase, IndexBuildPhase::Aborted);
        assert_eq!(
            store.abort_index_build(router, &control),
            Ok(IndexBuildCleanupStatus { done: true })
        );
    }
}
