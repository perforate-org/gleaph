//! Row-oriented TEXT index definition catalog in stable memory (plan 0297).
//!
//! The Router is the sole SSOT for text-index definitions, mirroring the derived vector-index
//! catalog ([`super::vector_index_catalog`]). One record pins the indexed vertex label +
//! property (both Router-interned ids), the pinned analyzer pipeline id (v0 production
//! analyzer = 1, ADR 0077), an optional single canister target, and a fail-closed
//! [`TextIndexStatus`].
//!
//! - `ROUTER_TEXT_INDEXES`: `(graph_id, text_index_id) → TextIndexDefRecord`
//!
//! ## Status model (fail-closed)
//!
//! The stored status follows the backfill lifecycle: `Registered` (declared, no canister;
//! not planner-visible), `Backfilling` (provisioned; migration-driven backfill in flight;
//! NOT planner-visible), and `Ready` (backfill converged and sealed — the ONLY
//! planner/query-visible state, see [`planning_visible_text_indexes`]). A provisioned
//! definition is born `Backfilling` so an empty-but-visible index can never serve
//! false-negative reads (ADR 0059 §Text build kind readiness gate); the single exact
//! transition is [`complete_text_backfill`], which rejects every other state without
//! mutating the row.

use std::borrow::Cow;
use std::ops::Bound;

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId, VertexLabelId};
use gleaph_graph_kernel::index::PhysicalIndexId;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};

use crate::facade::stable::{ROUTER_NEXT_TEXT_INDEX_ID, ROUTER_TEXT_INDEXES};
use crate::state::RouterError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TextIndexKey {
    pub graph_id: GraphId,
    pub text_index_id: u32,
}

impl TextIndexKey {
    pub const fn new(graph_id: GraphId, text_index_id: u32) -> Self {
        Self {
            graph_id,
            text_index_id,
        }
    }
}

/// Lifecycle of a TEXT index definition. `Registered` = declared, backfill not started;
/// `Backfilling` = migration-driven backfill in flight; `Ready` = converged + sealed and
/// the only planner/query-visible state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) enum TextIndexStatus {
    Registered,
    Backfilling,
    Ready,
}

/// One TEXT index definition row. Creation-fixed shape: label/property/analyzer changes are
/// drop + recreate, never in-place mutation.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct TextIndexDefRecord {
    pub text_index_id: u32,
    /// Graph-scoped logical index name (`CREATE TEXT INDEX <name>`), interned in the shared
    /// index-name catalog so cross-kind name collisions stay detectable in one place.
    pub index_name_id: IndexNameId,
    pub label_id: VertexLabelId,
    pub property_id: PropertyId,
    /// Analyzer pipeline identity pinned at creation (ADR 0077 v0 production pipeline = 1).
    pub analyzer_id: u32,
    /// `None` while `Registered`; always non-anonymous when set.
    pub target: Option<Principal>,
    pub status: TextIndexStatus,
}

/// Versioned stable envelope (ADR 0007). Fresh-state installs only; older bytes are rejected
/// explicitly rather than silently misdecoded.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum TextIndexDefStableRecord {
    V1(TextIndexDefRecord),
}

impl Storable for TextIndexKey {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.text_index_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut index = [0; 4];
        graph.copy_from_slice(&bytes[0..4]);
        index.copy_from_slice(&bytes[4..8]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            text_index_id: u32::from_le_bytes(index),
        }
    }
}

impl Storable for TextIndexDefRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&TextIndexDefStableRecord::V1(self.clone())).expect("encode text index def"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&TextIndexDefStableRecord::V1(self)).expect("encode text index def")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), TextIndexDefStableRecord).expect("decode text index def") {
            TextIndexDefStableRecord::V1(v1) => v1,
        }
    }
}

/// Read-only allocator preflight. Allocation is monotonic and zero is permanently invalid.
pub(crate) fn preflight_allocate_text_index_id() -> Result<(), RouterError> {
    next_available_text_index_id().map(|_| ())
}

/// Allocate one opaque physical text-index id. IDs are never rewound or reused.
pub(crate) fn allocate_text_index_id() -> Result<u32, RouterError> {
    let raw = next_available_text_index_id()?;
    ROUTER_NEXT_TEXT_INDEX_ID.with_borrow_mut(|next_id| {
        next_id.set(raw + 1);
        Ok(raw)
    })
}

fn next_available_text_index_id() -> Result<u32, RouterError> {
    let mut raw = ROUTER_NEXT_TEXT_INDEX_ID.with_borrow(|next_id| *next_id.get());
    if raw == 0 {
        return Err(RouterError::Internal(
            "text index allocator stored zero".into(),
        ));
    }
    loop {
        let _next = raw
            .checked_add(1)
            .ok_or_else(|| RouterError::IdExhausted("text index id".into()))?;
        let occupied = ROUTER_TEXT_INDEXES
            .with_borrow(|map| map.iter().any(|entry| entry.key().text_index_id == raw));
        if !occupied {
            return Ok(raw);
        }
        raw += 1;
    }
}

/// Register one TEXT index definition after full validation. The caller has already interned
/// `index_name_id` and resolved `label_id`/`property_id`; this function owns the durable insert
/// and every conflict check against existing definitions (cross-kind name reuse, duplicate
/// property coverage, id-key conflicts).
///
/// Returns whether the definition was newly created; an exact duplicate of `(index_name_id)`
/// with `if_not_exists` returns `Ok(false)` without mutating anything.
pub(crate) fn register_text_index(
    graph_id: GraphId,
    text_index_id: u32,
    index_name_id: IndexNameId,
    label_id: VertexLabelId,
    property_id: PropertyId,
    analyzer_id: u32,
    target: Option<Principal>,
    if_not_exists: bool,
) -> Result<bool, RouterError> {
    if target.is_some_and(|target| target == Principal::anonymous()) {
        return Err(RouterError::InvalidArgument(
            "text index target canister must not be the anonymous principal".to_owned(),
        ));
    }

    let key = TextIndexKey::new(graph_id, text_index_id);
    if ROUTER_TEXT_INDEXES.with_borrow(|map| map.contains_key(&key)) {
        if if_not_exists {
            return Ok(false);
        }
        return Err(RouterError::Conflict(format!(
            "text index already exists: {text_index_id}"
        )));
    }

    if super::index_name_catalog::index_name(graph_id, index_name_id).is_none() {
        return Err(RouterError::InvalidArgument(format!(
            "logical text index name id {} is not registered in this graph",
            index_name_id.raw()
        )));
    }

    // Cross-kind logical-name exclusivity: a name already owned by a property or vector index
    // can never be claimed by a text index (and vice versa is enforced by those catalogs'
    // callers consulting this catalog).
    if get_text_index_by_name_id(graph_id, index_name_id).is_some() {
        if if_not_exists {
            return Ok(false);
        }
        return Err(RouterError::Conflict(format!(
            "logical index name id {} already has a text index in this graph",
            index_name_id.raw()
        )));
    }

    // At most one TEXT index per (label, property) per graph (v1 contract, mirroring the
    // vector catalog's one-index-per-embedding rule): backfill/DML fan-out keys on the
    // (label, property) pair, so a second definition would receive duplicated writes.
    let property_conflict = ROUTER_TEXT_INDEXES.with_borrow(|map| {
        map.range((
            Bound::Included(graph_lower(graph_id)),
            graph_upper(graph_id),
        ))
        .any(|entry| entry.value().label_id == label_id && entry.value().property_id == property_id)
    });
    if property_conflict {
        return Err(RouterError::Conflict(format!(
            "vertex label {} property {} already has a text index in this graph",
            label_id.raw(),
            property_id.raw()
        )));
    }

    let def = TextIndexDefRecord {
        text_index_id,
        index_name_id,
        label_id,
        property_id,
        analyzer_id,
        target,
        status: resolve_status(target.is_some()),
    };
    ROUTER_TEXT_INDEXES.with_borrow_mut(|map| {
        map.insert(key, def);
    });
    Ok(true)
}

/// A provisioned definition is born `Backfilling`: it stays planner-INVISIBLE until its
/// backfill converges ([`complete_text_backfill`]), closing the false-negative window an
/// empty-but-visible index would open (ADR 0059 §Text build kind readiness gate).
fn resolve_status(has_target: bool) -> TextIndexStatus {
    if has_target {
        TextIndexStatus::Backfilling
    } else {
        TextIndexStatus::Registered
    }
}

/// Completes the backfill lifecycle `Backfilling → Ready` once the Router convergence
/// proof holds (text-canister scan done AND the Graph flushed watermark). This is the
/// only transition that makes a definition planner/query-visible. Any other current
/// state rejects without mutating the row.
pub(crate) fn complete_text_backfill(
    graph_id: GraphId,
    text_index_id: u32,
) -> Result<TextIndexDefRecord, RouterError> {
    transition_text_backfill_status(graph_id, text_index_id, |status| match status {
        TextIndexStatus::Backfilling => Some(TextIndexStatus::Ready),
        _ => None,
    })
}

/// Applies the single exact status move selected by `step`; `None` means the current
/// state forbids the transition and the row is left untouched (fail closed).
fn transition_text_backfill_status(
    graph_id: GraphId,
    text_index_id: u32,
    step: impl FnOnce(TextIndexStatus) -> Option<TextIndexStatus>,
) -> Result<TextIndexDefRecord, RouterError> {
    let key = TextIndexKey::new(graph_id, text_index_id);
    let existing = ROUTER_TEXT_INDEXES
        .with_borrow(|map| map.get(&key))
        .ok_or_else(|| RouterError::NotFound(format!("text index {text_index_id}")))?;
    let next = step(existing.status).ok_or_else(|| {
        RouterError::InvalidState(format!(
            "text index {text_index_id} status {:?} rejects the backfill transition",
            existing.status
        ))
    })?;
    let updated = TextIndexDefRecord {
        status: next,
        ..existing.clone()
    };
    ROUTER_TEXT_INDEXES.with_borrow_mut(|map| {
        map.insert(key, updated.clone());
    });
    Ok(updated)
}

/// Planner/query projection: ONLY `Ready` definitions with an attached canister are
/// visible to query planning; `Registered` and `Backfilling` rows stay invisible exactly
/// like non-Active property indexes (ADR 0059 §Text build kind readiness gate).
#[allow(
    dead_code,
    reason = "text planner projection wiring lands with plan 0297"
)]
pub(crate) fn planning_visible_text_indexes(graph_id: GraphId) -> Vec<TextIndexDefRecord> {
    list_text_indexes(graph_id)
        .into_iter()
        .filter(|def| def.status == TextIndexStatus::Ready && def.target.is_some())
        .collect()
}

pub(crate) fn get_text_index(graph_id: GraphId, text_index_id: u32) -> Option<TextIndexDefRecord> {
    ROUTER_TEXT_INDEXES.with_borrow(|map| map.get(&TextIndexKey::new(graph_id, text_index_id)))
}

// -- Migration-driven backfill build records (plan 0297 backfill-pull) ----------------------

/// Lifecycle of one migration-driven TEXT backfill build. Mirrors the property lane's
/// Building → Sealing → Converged spine; there is no Active state because readiness is the
/// TEXT catalog row's own [`TextIndexStatus::Ready`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) enum TextBackfillBuildPhase {
    /// Registered with the text canister; bounded pull steps advance it.
    Building,
    /// Graph scope frozen at the fresh epoch; awaiting both convergence gates.
    Sealing,
    /// Both gates proven; the definition reached `Ready` and the sub-build is terminal.
    Converged,
}

/// Durable per-build identity for one migration-driven TEXT backfill (the text analogue of
/// the property `IndexDefRecord.build` metadata). Created once at migration prepare and
/// dropped when the build converges or its cleanup completes.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct TextBackfillBuildRecord {
    pub migration_id: String,
    pub topology_epoch: u64,
    pub prepared_catalog_epoch: u64,
    pub physical_index_id: PhysicalIndexId,
    pub home_shard_id: u32,
    pub home_graph_canister: Principal,
    /// One-shot registration guard for the Building phase.
    pub registered: bool,
    pub phase: TextBackfillBuildPhase,
}

/// Versioned stable envelope (ADR 0007). Fresh-state installs only.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum TextBackfillBuildStableRecord {
    V1(TextBackfillBuildRecord),
}

impl Storable for TextBackfillBuildRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&TextBackfillBuildStableRecord::V1(self.clone()))
                .expect("encode text backfill build"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&TextBackfillBuildStableRecord::V1(self)).expect("encode text backfill build")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), TextBackfillBuildStableRecord)
            .expect("decode text backfill build")
        {
            TextBackfillBuildStableRecord::V1(v1) => v1,
        }
    }
}

/// Creates one durable build record after full validation. The caller has already resolved
/// the identity allocations (physical id + prepared epoch) inside the migration co-write;
/// this function rejects a conflicting live record for the same definition fail-closed.
pub(crate) fn prepare_text_backfill_build(
    graph_id: GraphId,
    text_index_id: u32,
    record: TextBackfillBuildRecord,
) -> Result<(), RouterError> {
    let key = TextIndexKey::new(graph_id, text_index_id);
    if crate::facade::stable::ROUTER_TEXT_BACKFILL_BUILDS.with_borrow(|map| map.contains_key(&key))
    {
        return Err(RouterError::Conflict(format!(
            "text index {text_index_id} already has a pending backfill build"
        )));
    }
    crate::facade::stable::ROUTER_TEXT_BACKFILL_BUILDS.with_borrow_mut(|map| {
        map.insert(key, record);
    });
    Ok(())
}

pub(crate) fn get_text_backfill_build(
    graph_id: GraphId,
    text_index_id: u32,
) -> Option<TextBackfillBuildRecord> {
    crate::facade::stable::ROUTER_TEXT_BACKFILL_BUILDS
        .with_borrow(|map| map.get(&TextIndexKey::new(graph_id, text_index_id)))
}

/// Marks the one-shot text-canister registration complete. Rejecting an already-registered
/// build keeps the Register step exactly-once even across ambiguous retries.
pub(crate) fn mark_text_backfill_registered(
    graph_id: GraphId,
    text_index_id: u32,
) -> Result<(), RouterError> {
    let key = TextIndexKey::new(graph_id, text_index_id);
    let existing = crate::facade::stable::ROUTER_TEXT_BACKFILL_BUILDS
        .with_borrow(|map| map.get(&key))
        .ok_or_else(|| RouterError::NotFound(format!("text backfill build {text_index_id}")))?;
    if existing.registered {
        return Err(RouterError::Conflict(
            "text backfill already registered".into(),
        ));
    }
    let updated = TextBackfillBuildRecord {
        registered: true,
        ..existing.clone()
    };
    crate::facade::stable::ROUTER_TEXT_BACKFILL_BUILDS.with_borrow_mut(|map| {
        map.insert(key, updated);
    });
    Ok(())
}

/// Advances the durable phase after validating the exact state-machine edge.
pub(crate) fn transition_text_backfill_build(
    graph_id: GraphId,
    text_index_id: u32,
    next: TextBackfillBuildPhase,
) -> Result<TextBackfillBuildRecord, RouterError> {
    let key = TextIndexKey::new(graph_id, text_index_id);
    let existing = crate::facade::stable::ROUTER_TEXT_BACKFILL_BUILDS
        .with_borrow(|map| map.get(&key))
        .ok_or_else(|| RouterError::NotFound(format!("text backfill build {text_index_id}")))?;
    let valid = matches!(
        (existing.phase, next),
        (
            TextBackfillBuildPhase::Building,
            TextBackfillBuildPhase::Sealing
        ) | (
            TextBackfillBuildPhase::Sealing,
            TextBackfillBuildPhase::Converged
        )
    );
    if !valid || existing.phase == next {
        return Err(RouterError::InvalidState(format!(
            "text backfill build {text_index_id} phase {:?} rejects {:?}",
            existing.phase, next
        )));
    }
    let updated = TextBackfillBuildRecord {
        phase: next,
        ..existing.clone()
    };
    crate::facade::stable::ROUTER_TEXT_BACKFILL_BUILDS.with_borrow_mut(|map| {
        map.insert(key, updated.clone());
    });
    Ok(updated)
}

/// Drops the build record once it converged or its cleanup completed. The TEXT definition
/// row survives; only the migration-owned build identity is released.
pub(crate) fn drop_text_backfill_build(graph_id: GraphId, text_index_id: u32) {
    crate::facade::stable::ROUTER_TEXT_BACKFILL_BUILDS
        .with_borrow_mut(|map| map.remove(&TextIndexKey::new(graph_id, text_index_id)));
}

/// Resolves the migration-driven build record by logical name, if any.
pub(crate) fn get_text_backfill_build_by_name_id(
    graph_id: GraphId,
    index_name_id: IndexNameId,
) -> Option<(TextIndexDefRecord, TextBackfillBuildRecord)> {
    let def = get_text_index_by_name_id(graph_id, index_name_id)?;
    let build = get_text_backfill_build(graph_id, def.text_index_id)?;
    Some((def, build))
}

/// Resolve one TEXT definition by its graph-scoped logical name. Registration enforces
/// uniqueness at creation, so the bounded graph-local scan returns at most one record.
pub(crate) fn get_text_index_by_name_id(
    graph_id: GraphId,
    index_name_id: IndexNameId,
) -> Option<TextIndexDefRecord> {
    list_text_indexes(graph_id)
        .into_iter()
        .find(|def| def.index_name_id == index_name_id)
}

pub(crate) fn list_text_indexes(graph_id: GraphId) -> Vec<TextIndexDefRecord> {
    ROUTER_TEXT_INDEXES.with_borrow(|map| {
        map.range((
            Bound::Included(graph_lower(graph_id)),
            graph_upper(graph_id),
        ))
        .map(|entry| entry.value())
        .collect()
    })
}

/// Map a stored definition to its public wire view (plan 0297).
pub(crate) fn text_index_info(def: &TextIndexDefRecord) -> crate::types::TextIndexInfo {
    crate::types::TextIndexInfo {
        text_index_id: def.text_index_id,
        label_id: def.label_id.raw(),
        property_id: def.property_id.raw(),
        analyzer_id: def.analyzer_id,
        canister: def.target,
        status: match def.status {
            TextIndexStatus::Registered => crate::types::TextIndexStatusView::Registered,
            TextIndexStatus::Backfilling => crate::types::TextIndexStatusView::Backfilling,
            TextIndexStatus::Ready => crate::types::TextIndexStatusView::Ready,
        },
    }
}

fn graph_lower(graph_id: GraphId) -> TextIndexKey {
    TextIndexKey::new(graph_id, 0)
}

/// Exclusive upper bound of one graph's `TextIndexKey` range. `graph_id` is the most-significant
/// key component, so `[(graph_id, 0), (graph_id + 1, 0))` covers exactly that graph. At
/// `GraphId::MAX` there is no `graph_id + 1`; the bound must be `Unbounded`.
fn graph_upper(graph_id: GraphId) -> Bound<TextIndexKey> {
    match graph_id.raw().checked_add(1) {
        Some(next) => Bound::Excluded(TextIndexKey::new(GraphId::from_raw(next), 0)),
        None => Bound::Unbounded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_index_name_id(graph: GraphId, ordinal: u32) -> IndexNameId {
        super::super::index_name_catalog::intern_index_name(
            graph,
            &format!("test_text_index_{ordinal}"),
        )
        .expect("intern test text index name")
    }

    fn register(
        graph: GraphId,
        id: u32,
        label: u16,
        property: u32,
        target: Option<Principal>,
    ) -> bool {
        register_text_index(
            graph,
            id,
            test_index_name_id(graph, id),
            VertexLabelId::from_raw(label),
            PropertyId::from_raw(property),
            1,
            target,
            false,
        )
        .expect("register text index")
    }

    #[test]
    fn key_and_record_storable_roundtrip() {
        let key = TextIndexKey::new(GraphId::from_raw(7), 42);
        assert_eq!(TextIndexKey::from_bytes(Cow::Owned(key.into_bytes())), key);

        let record = TextIndexDefRecord {
            text_index_id: 9,
            index_name_id: IndexNameId::from_raw(4),
            label_id: VertexLabelId::from_raw(2),
            property_id: PropertyId::from_raw(6),
            analyzer_id: 1,
            target: Some(Principal::management_canister()),
            status: TextIndexStatus::Ready,
        };
        assert_eq!(
            TextIndexDefRecord::from_bytes(Cow::Owned(record.clone().into_bytes())),
            record
        );
    }

    #[test]
    fn registration_without_target_is_registered_and_provisioned_starts_backfilling() {
        let graph = GraphId::from_raw(930_001);
        assert!(register(graph, 1, 1, 10, None));
        let def = get_text_index(graph, 1).expect("def");
        assert_eq!(def.status, TextIndexStatus::Registered);
        assert_eq!(def.analyzer_id, 1);

        // A provisioned definition is born Backfilling: invisible until its backfill
        // converges (ADR 0059 §Text build kind readiness gate).
        assert!(register(
            graph,
            2,
            1,
            11,
            Some(Principal::management_canister())
        ));
        let def = get_text_index(graph, 2).expect("def");
        assert_eq!(def.status, TextIndexStatus::Backfilling);
    }

    #[test]
    fn backfill_transitions_are_exact_and_fail_closed_elsewhere() {
        let graph = GraphId::from_raw(930_005);
        register(graph, 1, 1, 10, Some(Principal::management_canister()));
        assert_eq!(
            get_text_index(graph, 1).expect("row").status,
            TextIndexStatus::Backfilling,
            "provisioned rows are born backfilling"
        );

        // Registered rows (no canister) reject completion outright.
        register(graph, 3, 3, 30, None);
        assert!(matches!(
            complete_text_backfill(graph, 3),
            Err(RouterError::InvalidState(_))
        ));

        // Backfilling -> Ready, then terminal.
        let ready = complete_text_backfill(graph, 1).expect("complete");
        assert_eq!(ready.status, TextIndexStatus::Ready);
        assert!(matches!(
            complete_text_backfill(graph, 1),
            Err(RouterError::InvalidState(_))
        ));
        assert_eq!(
            get_text_index(graph, 1).expect("row survives").status,
            TextIndexStatus::Ready
        );

        // Unknown ids fail closed without inventing rows.
        assert!(matches!(
            complete_text_backfill(graph, 99),
            Err(RouterError::NotFound(_))
        ));
    }

    #[test]
    fn planning_projection_exposes_only_ready_definitions() {
        let graph = GraphId::from_raw(930_006);
        assert!(planning_visible_text_indexes(graph).is_empty());

        // Registered without canister: declared but never visible.
        register(graph, 1, 1, 10, None);
        assert!(planning_visible_text_indexes(graph).is_empty());

        // Provisioned rows are born Backfilling: still invisible.
        let ready_id = test_index_name_id(graph, 2);
        assert!(
            register_text_index(
                graph,
                2,
                ready_id,
                VertexLabelId::from_raw(1),
                PropertyId::from_raw(11),
                1,
                Some(Principal::management_canister()),
                false
            )
            .expect("provisioned registration")
        );
        assert!(planning_visible_text_indexes(graph).is_empty());
        complete_text_backfill(graph, 2).expect("complete");
        assert_eq!(planning_visible_text_indexes(graph).len(), 1);

        // A second graph's definitions do not leak across the boundary.
        assert!(planning_visible_text_indexes(GraphId::from_raw(930_007)).is_empty());
    }

    #[test]
    fn anonymous_target_rejected_without_inserting() {
        let graph = GraphId::from_raw(930_002);
        let err = register_text_index(
            graph,
            1,
            test_index_name_id(graph, 1),
            VertexLabelId::from_raw(1),
            PropertyId::from_raw(10),
            1,
            Some(Principal::anonymous()),
            false,
        )
        .expect_err("anonymous target must fail closed");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
        assert!(get_text_index(graph, 1).is_none());
    }

    #[test]
    fn duplicate_property_coverage_conflicts_across_ids() {
        let graph = GraphId::from_raw(930_003);
        assert!(register(graph, 1, 1, 10, None));
        let err = register_text_index(
            graph,
            2,
            test_index_name_id(graph, 2),
            VertexLabelId::from_raw(1),
            PropertyId::from_raw(10),
            1,
            None,
            false,
        )
        .expect_err("second index on the same (label, property) must conflict");
        assert!(matches!(err, RouterError::Conflict(_)));
        assert!(get_text_index(graph, 2).is_none());

        // Same property under a different label is a distinct coverage domain.
        assert!(register(graph, 3, 2, 10, None));
    }

    #[test]
    fn duplicate_name_conflicts_unless_if_not_exists() {
        let graph = GraphId::from_raw(930_004);
        let name_id = test_index_name_id(graph, 1);
        assert!(
            register_text_index(
                graph,
                1,
                name_id,
                VertexLabelId::from_raw(1),
                PropertyId::from_raw(10),
                1,
                None,
                false,
            )
            .expect("first registration succeeds")
        );
        assert!(matches!(
            register_text_index(
                graph,
                2,
                name_id,
                VertexLabelId::from_raw(1),
                PropertyId::from_raw(11),
                1,
                None,
                false,
            ),
            Err(RouterError::Conflict(_))
        ));
        // IF NOT EXISTS replay reports "not newly created" and preserves the original row.
        assert!(
            !register_text_index(
                graph,
                2,
                name_id,
                VertexLabelId::from_raw(1),
                PropertyId::from_raw(11),
                1,
                None,
                true,
            )
            .expect("if-not-exists replay"),
            "if-not-exists replay must report not-newly-created"
        );
        assert_eq!(
            get_text_index_by_name_id(graph, name_id)
                .unwrap()
                .text_index_id,
            1
        );
    }

    #[test]
    fn unmapped_index_name_rejected_without_inserting() {
        let graph = GraphId::from_raw(930_005);
        let err = register_text_index(
            graph,
            1,
            IndexNameId::from_raw(999),
            VertexLabelId::from_raw(1),
            PropertyId::from_raw(10),
            1,
            None,
            false,
        )
        .expect_err("unmapped name id must fail closed");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
        assert!(get_text_index(graph, 1).is_none());
    }

    #[test]
    fn allocation_is_monotonic_and_skips_used_ids() {
        let occupied = ROUTER_NEXT_TEXT_INDEX_ID.with_borrow(|cell| *cell.get());
        let graph = GraphId::from_raw(u32::MAX - 1);
        register_text_index(
            graph,
            occupied,
            test_index_name_id(graph, occupied),
            VertexLabelId::from_raw(1),
            PropertyId::from_raw(10),
            1,
            None,
            false,
        )
        .expect("legacy registration at allocator cursor");

        let allocated = allocate_text_index_id().expect("allocate after used id");
        assert_ne!(allocated, occupied);
        assert!(allocated > occupied);
    }

    #[test]
    fn list_is_graph_scoped() {
        let graph = GraphId::from_raw(930_006);
        let other = GraphId::from_raw(930_007);
        assert!(register(graph, 1, 1, 10, None));
        assert!(register(graph, 2, 1, 11, None));
        assert!(register(other, 3, 1, 12, None));

        let listed = list_text_indexes(graph);
        assert_eq!(listed.len(), 2);
        assert_eq!(list_text_indexes(other).len(), 1);
    }

    #[test]
    fn regions_reopen_from_stable_memory_roundtrip() {
        let graph = GraphId::from_raw(930_008);
        assert!(register(
            graph,
            5,
            3,
            30,
            Some(Principal::management_canister())
        ));
        let before = get_text_index(graph, 5).expect("def before reopen");
        let cursor_before = ROUTER_NEXT_TEXT_INDEX_ID.with_borrow(|cell| *cell.get());

        super::super::reopen_text_index_regions_for_test();

        let reopened = get_text_index(graph, 5).expect("def survives reopen");
        assert_eq!(reopened, before, "reopen must preserve the full record");
        assert_eq!(
            ROUTER_NEXT_TEXT_INDEX_ID.with_borrow(|cell| *cell.get()),
            cursor_before,
            "allocator cursor survives reopen"
        );
    }
}
