//! Row-oriented derived vector-index definition catalog in stable memory (ADR 0031 Slice 3).
//!
//! The Router is the sole SSOT for vector-index definitions, mirroring the property index catalog
//! ([`super::indexed_catalog`]) and the unique-constraint catalog ([`super::constraint_catalog`]).
//! A definition pins the embedding it derives from (the Router-interned [`EmbeddingNameId`], see
//! [`super::embedding_name_catalog`]), its physical shape (`kind`/`metric`/`encoding`/`dims`), an
//! optional single [`VectorIndexTarget`], and a fail-closed [`VectorIndexActivationState`].
//!
//! - `ROUTER_VECTOR_INDEXES`: `(graph_id, index_id) → VectorIndexDefRecord`
//!
//! ## Activation gate (fail-closed)
//!
//! Production catalog-backed dispatch MUST stay blocked until delete-spanning incarnation/epoch
//! fencing exists (the "reverse-orphan race" prerequisite documented in ADR 0031). The single
//! switch is [`incarnation_fencing_enabled`], which is `const false` in Slice 3, so a definition
//! that has a target can only ever reach [`VectorIndexActivationState::DispatchBlockedMissingIncarnationFence`].
//! [`VectorIndexActivationState::DispatchEnabled`] is unreachable until the fencing slice flips the gate.

use std::borrow::Cow;
use std::ops::Bound;

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::{EmbeddingNameId, GraphId};
use gleaph_graph_kernel::vector_index::{
    IndexedEmbeddingCatalog, IndexedEmbeddingSpec, VectorEncoding, VectorIndexKind, VectorMetric,
};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};

use crate::facade::stable::ROUTER_VECTOR_INDEXES;
use crate::state::{RouterError, VectorActivationBlockReason};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct VectorIndexKey {
    pub graph_id: GraphId,
    pub index_id: u32,
}

impl VectorIndexKey {
    pub const fn new(graph_id: GraphId, index_id: u32) -> Self {
        Self { graph_id, index_id }
    }
}

/// Single dispatch target for a vector-index definition (ADR 0031 Slice 3, target model B).
///
/// Slice 3 stores the target as catalog-local metadata only; it is **not** pushed into graph
/// shards or consumed by any execution path until activation wiring + fencing lands (setter
/// deferral C). Slice 4+ may promote this to a fleet/cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct VectorIndexTarget {
    pub canister: Principal,
}

/// Lifecycle of a vector-index definition (ADR 0031 Slice 3). Both dispatch and backfill stay
/// blocked until incarnation fencing, so there is intentionally no `BackfillReady` state — it
/// would falsely imply partial activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) enum VectorIndexActivationState {
    /// Definition created; a target may not be set yet.
    Registered,
    /// Target + shape metadata complete, but production dispatch **and** backfill remain blocked by
    /// the missing delete-spanning incarnation/epoch fence. Terminal Slice 3 state.
    DispatchBlockedMissingIncarnationFence,
    /// Dispatch is live. Unreachable in Slice 3 (the fencing slice flips [`incarnation_fencing_enabled`]).
    DispatchEnabled,
}

/// The single activation switch (ADR 0031 Slice 3). `const false` until delete-spanning
/// incarnation/epoch fencing lands; the fencing slice is the only place that flips this.
pub(crate) const fn incarnation_fencing_enabled() -> bool {
    false
}

/// The fail-closed block reason for an activation state, if any. `Registered` (no target yet) and
/// `DispatchEnabled` (unreachable in Slice 3) are not "blocked"; only
/// `DispatchBlockedMissingIncarnationFence` carries a reason.
pub(crate) fn activation_block_reason(
    state: VectorIndexActivationState,
) -> Option<VectorActivationBlockReason> {
    match state {
        VectorIndexActivationState::DispatchBlockedMissingIncarnationFence => {
            Some(VectorActivationBlockReason::MissingEmbeddingIncarnationFence)
        }
        VectorIndexActivationState::Registered | VectorIndexActivationState::DispatchEnabled => {
            None
        }
    }
}

/// Resolve the activation state a definition should hold given whether it has a target. This is the
/// **only** place a definition can become [`VectorIndexActivationState::DispatchEnabled`], and it
/// can do so only when [`incarnation_fencing_enabled`] is true — the fail-closed gate.
fn resolve_activation_state(has_target: bool) -> VectorIndexActivationState {
    if !has_target {
        VectorIndexActivationState::Registered
    } else if incarnation_fencing_enabled() {
        VectorIndexActivationState::DispatchEnabled
    } else {
        VectorIndexActivationState::DispatchBlockedMissingIncarnationFence
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct VectorIndexDefRecord {
    pub index_id: u32,
    pub embedding_name_id: EmbeddingNameId,
    pub kind: VectorIndexKind,
    pub metric: VectorMetric,
    pub encoding: VectorEncoding,
    pub dims: u16,
    /// `None` while `Registered`; set via [`set_vector_index_target`]. Always non-anonymous when set.
    pub target: Option<VectorIndexTarget>,
    pub activation_state: VectorIndexActivationState,
}

/// Versioned stable envelope (ADR 0007) so the record schema can evolve across upgrades.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum VectorIndexDefStableRecord {
    V1(VectorIndexDefRecord),
}

impl Storable for VectorIndexKey {
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
        out.extend_from_slice(&self.index_id.to_le_bytes());
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
            index_id: u32::from_le_bytes(index),
        }
    }
}

impl Storable for VectorIndexDefRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&VectorIndexDefStableRecord::V1(*self)).expect("encode vector index def"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&VectorIndexDefStableRecord::V1(self)).expect("encode vector index def")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), VectorIndexDefStableRecord).expect("decode vector index def")
        {
            VectorIndexDefStableRecord::V1(v1) => v1,
        }
    }
}

fn reject_anonymous(target: VectorIndexTarget) -> Result<VectorIndexTarget, RouterError> {
    if target.canister == Principal::anonymous() {
        return Err(RouterError::InvalidArgument(
            "vector index target canister must not be the anonymous principal".to_owned(),
        ));
    }
    Ok(target)
}

/// Outcome of [`preflight_register`]: whether the caller should proceed to a durable insert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegisterPreflight {
    /// No definition exists for `index_id` and the target (if any) is valid; proceed to insert.
    Proceed,
    /// A definition already exists and `if_not_exists` was set; the caller should report a no-op
    /// (`Ok(false)`) **without** any side effects (notably without interning an embedding name).
    AlreadyExists,
}

/// Validate a registration request **without mutating any state** (ADR 0031 Slice 3). This is the
/// single source of truth for the register decision: it rejects an anonymous target and a
/// conflicting `index_id`, and reports an idempotent no-op for `if_not_exists`.
///
/// `admin_register_vector_index` calls this *before* interning the embedding name so a rejected or
/// no-op registration never allocates a durable [`EmbeddingNameId`] (which would pollute the
/// graph-scoped name catalog and could exhaust the `u16` name space through failed DDL).
pub(crate) fn preflight_register(
    graph_id: GraphId,
    index_id: u32,
    target: Option<VectorIndexTarget>,
    if_not_exists: bool,
) -> Result<RegisterPreflight, RouterError> {
    if let Some(target) = target {
        reject_anonymous(target)?;
    }
    let key = VectorIndexKey::new(graph_id, index_id);
    let exists = ROUTER_VECTOR_INDEXES.with_borrow(|map| map.contains_key(&key));
    if exists {
        if if_not_exists {
            return Ok(RegisterPreflight::AlreadyExists);
        }
        return Err(RouterError::Conflict(format!(
            "vector index already exists: {index_id}"
        )));
    }
    Ok(RegisterPreflight::Proceed)
}

/// Register a new vector-index definition. The embedding is identified by an already-interned
/// [`EmbeddingNameId`] (resolved by name via [`super::embedding_name_catalog`]). Validation goes
/// through [`preflight_register`] (anonymous target rejected, conflicts/no-ops handled); the
/// activation state is computed by the fail-closed gate.
pub(crate) fn register_vector_index(
    graph_id: GraphId,
    index_id: u32,
    embedding_name_id: EmbeddingNameId,
    kind: VectorIndexKind,
    metric: VectorMetric,
    encoding: VectorEncoding,
    dims: u16,
    target: Option<VectorIndexTarget>,
    if_not_exists: bool,
) -> Result<bool, RouterError> {
    let key = VectorIndexKey::new(graph_id, index_id);
    match preflight_register(graph_id, index_id, target, if_not_exists)? {
        RegisterPreflight::AlreadyExists => return Ok(false),
        RegisterPreflight::Proceed => {}
    }

    let activation_state = resolve_activation_state(target.is_some());
    let def = VectorIndexDefRecord {
        index_id,
        embedding_name_id,
        kind,
        metric,
        encoding,
        dims,
        target,
        activation_state,
    };
    ROUTER_VECTOR_INDEXES.with_borrow_mut(|map| {
        map.insert(key, def);
    });
    Ok(true)
}

/// Set (or replace) the single target of an existing definition and recompute its activation state
/// through the fail-closed gate. Rejects an anonymous principal.
pub(crate) fn set_vector_index_target(
    graph_id: GraphId,
    index_id: u32,
    target: VectorIndexTarget,
) -> Result<(), RouterError> {
    let target = reject_anonymous(target)?;
    let key = VectorIndexKey::new(graph_id, index_id);
    ROUTER_VECTOR_INDEXES.with_borrow_mut(|map| {
        let Some(mut def) = map.get(&key) else {
            return Err(RouterError::NotFound(format!("vector index {index_id}")));
        };
        def.target = Some(target);
        def.activation_state = resolve_activation_state(true);
        map.insert(key, def);
        Ok(())
    })
}

pub(crate) fn get_vector_index(graph_id: GraphId, index_id: u32) -> Option<VectorIndexDefRecord> {
    ROUTER_VECTOR_INDEXES.with_borrow(|map| map.get(&VectorIndexKey::new(graph_id, index_id)))
}

/// Resolve the single dispatch target of a definition to its canister principal (ADR 0031 Slice 3,
/// target model B). Rejects a missing definition, an unset target, and (defensively) an anonymous
/// principal.
///
/// **Inspect/admin-visible only in Slice 3.** The target is never pushed to graph shards (setter
/// deferral C), so this helper MUST NOT be consumed by graph execution, ephemeral catalog injection,
/// pending flush, the repair drain, or backfill until activation wiring + incarnation fencing lands.
/// Its only Slice 3 consumers are the Router admin/query surface and tests.
pub(crate) fn vector_index_target_for(
    graph_id: GraphId,
    index_id: u32,
) -> Result<Principal, RouterError> {
    let def = get_vector_index(graph_id, index_id)
        .ok_or_else(|| RouterError::NotFound(format!("vector index {index_id}")))?;
    let target = def.target.ok_or_else(|| {
        RouterError::Conflict(format!("vector index {index_id} has no target set"))
    })?;
    Ok(reject_anonymous(target)?.canister)
}

pub(crate) fn list_vector_indexes(graph_id: GraphId) -> Vec<VectorIndexDefRecord> {
    ROUTER_VECTOR_INDEXES.with_borrow(|map| {
        let start = VectorIndexKey::new(graph_id, 0);
        map.range((Bound::Included(start), graph_upper(graph_id)))
            .map(|entry| entry.value())
            .collect()
    })
}

/// Build the ephemeral indexed-embedding catalog the Router stamps onto `ExecutePlanArgs` for a
/// graph (ADR 0031 Slice 3), mirroring `to_indexed_property_catalog`.
///
/// **This is the single fail-closed activation gate.** Only [`VectorIndexActivationState::DispatchEnabled`]
/// definitions are exported. Because [`incarnation_fencing_enabled`] is `const false` in Slice 3, no
/// definition is ever `DispatchEnabled`, so the production catalog is **always empty**:
/// `vector_dispatch::spec_for` returns `None` and derived vector sync stays inert. Activating
/// dispatch later is the one fence flip — there is no silent partial activation.
pub(crate) fn to_indexed_embedding_catalog(graph_id: GraphId) -> IndexedEmbeddingCatalog {
    let embeddings = ROUTER_VECTOR_INDEXES.with_borrow(|map| {
        let start = VectorIndexKey::new(graph_id, 0);
        map.range((Bound::Included(start), graph_upper(graph_id)))
            .map(|entry| entry.value())
            .filter(|def| def.activation_state == VectorIndexActivationState::DispatchEnabled)
            .map(|def| IndexedEmbeddingSpec {
                embedding_name_id: def.embedding_name_id.raw(),
                index_id: def.index_id,
                kind: def.kind,
                metric: def.metric,
                encoding: def.encoding,
                dims: def.dims,
            })
            .collect()
    });
    IndexedEmbeddingCatalog { embeddings }
}

pub(crate) fn purge_graph_vector_indexes(graph_id: GraphId) {
    ROUTER_VECTOR_INDEXES.with_borrow_mut(|map| {
        let start = VectorIndexKey::new(graph_id, 0);
        let keys: Vec<_> = map
            .range((Bound::Included(start), graph_upper(graph_id)))
            .map(|entry| *entry.key())
            .collect();
        for key in keys {
            map.remove(&key);
        }
    });
}

/// Exclusive upper bound of one graph's `VectorIndexKey` range. `graph_id` is the most-significant
/// key component, so `[(graph_id, 0), (graph_id + 1, 0))` covers exactly that graph. At
/// `GraphId::MAX` there is no `graph_id + 1`; the bound must be `Unbounded` — a saturating `+1`
/// would collapse to `(MAX, 0)` and silently drop the max graph's definitions.
fn graph_upper(graph_id: GraphId) -> Bound<VectorIndexKey> {
    match graph_id.raw().checked_add(1) {
        Some(next) => Bound::Excluded(VectorIndexKey::new(GraphId::from_raw(next), 0)),
        None => Bound::Unbounded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_def(graph: GraphId, index_id: u32, target: Option<VectorIndexTarget>) -> bool {
        register_vector_index(
            graph,
            index_id,
            EmbeddingNameId::from_raw(1),
            VectorIndexKind::IvfFlat,
            VectorMetric::L2Squared,
            VectorEncoding::F32,
            16,
            target,
            false,
        )
        .expect("register vector index")
    }

    #[test]
    fn fencing_gate_is_off_in_slice_3() {
        assert!(
            !incarnation_fencing_enabled(),
            "Slice 3 must keep production dispatch fail-closed"
        );
    }

    #[test]
    fn key_storable_roundtrip() {
        let key = VectorIndexKey::new(GraphId::from_raw(7), 42);
        assert_eq!(
            VectorIndexKey::from_bytes(Cow::Owned(key.into_bytes())),
            key
        );
    }

    #[test]
    fn record_storable_roundtrip() {
        let record = VectorIndexDefRecord {
            index_id: 9,
            embedding_name_id: EmbeddingNameId::from_raw(3),
            kind: VectorIndexKind::IvfFlat,
            metric: VectorMetric::L2Squared,
            encoding: VectorEncoding::F32,
            dims: 8,
            target: Some(VectorIndexTarget {
                canister: Principal::management_canister(),
            }),
            activation_state: VectorIndexActivationState::DispatchBlockedMissingIncarnationFence,
        };
        assert_eq!(
            VectorIndexDefRecord::from_bytes(Cow::Owned(record.into_bytes())),
            record
        );
    }

    #[test]
    fn registration_without_target_is_registered() {
        let graph = GraphId::from_raw(920_001);
        assert!(sample_def(graph, 1, None));
        let def = get_vector_index(graph, 1).expect("def");
        assert_eq!(def.activation_state, VectorIndexActivationState::Registered);
        assert!(def.target.is_none());
    }

    #[test]
    fn registration_with_target_blocks_on_missing_fence() {
        let graph = GraphId::from_raw(920_002);
        let target = VectorIndexTarget {
            canister: Principal::management_canister(),
        };
        assert!(sample_def(graph, 1, Some(target)));
        let def = get_vector_index(graph, 1).expect("def");
        assert_eq!(
            def.activation_state,
            VectorIndexActivationState::DispatchBlockedMissingIncarnationFence,
            "a targeted def can never reach DispatchEnabled while the fence is off"
        );
    }

    #[test]
    fn set_target_transitions_to_blocked() {
        let graph = GraphId::from_raw(920_003);
        assert!(sample_def(graph, 1, None));
        set_vector_index_target(
            graph,
            1,
            VectorIndexTarget {
                canister: Principal::management_canister(),
            },
        )
        .expect("set target");
        let def = get_vector_index(graph, 1).expect("def");
        assert_eq!(
            def.activation_state,
            VectorIndexActivationState::DispatchBlockedMissingIncarnationFence
        );
        assert!(def.target.is_some());
    }

    #[test]
    fn anonymous_target_is_rejected() {
        let graph = GraphId::from_raw(920_004);
        let anon = Some(VectorIndexTarget {
            canister: Principal::anonymous(),
        });
        assert!(matches!(
            register_vector_index(
                graph,
                1,
                EmbeddingNameId::from_raw(1),
                VectorIndexKind::IvfFlat,
                VectorMetric::L2Squared,
                VectorEncoding::F32,
                16,
                anon,
                false,
            ),
            Err(RouterError::InvalidArgument(_))
        ));
        assert!(
            get_vector_index(graph, 1).is_none(),
            "rejected def must not persist"
        );

        assert!(sample_def(graph, 1, None));
        assert!(matches!(
            set_vector_index_target(
                graph,
                1,
                VectorIndexTarget {
                    canister: Principal::anonymous()
                }
            ),
            Err(RouterError::InvalidArgument(_))
        ));
    }

    #[test]
    fn preflight_validates_without_mutating_state() {
        let graph = GraphId::from_raw(920_030);
        let anon = VectorIndexTarget {
            canister: Principal::anonymous(),
        };
        // Anonymous target is rejected and nothing is inserted.
        assert!(matches!(
            preflight_register(graph, 1, Some(anon), false),
            Err(RouterError::InvalidArgument(_))
        ));
        // A fresh def proceeds; preflight itself must not insert it.
        assert_eq!(
            preflight_register(graph, 1, None, false).expect("preflight"),
            RegisterPreflight::Proceed
        );
        assert!(
            get_vector_index(graph, 1).is_none(),
            "preflight must not mutate the catalog"
        );

        // After a real registration, conflict vs. if-not-exists no-op are distinguished.
        assert!(sample_def(graph, 1, None));
        assert!(matches!(
            preflight_register(graph, 1, None, false),
            Err(RouterError::Conflict(_))
        ));
        assert_eq!(
            preflight_register(graph, 1, None, true).expect("preflight if-not-exists"),
            RegisterPreflight::AlreadyExists
        );
    }

    #[test]
    fn duplicate_registration_conflicts_unless_if_not_exists() {
        let graph = GraphId::from_raw(920_005);
        assert!(sample_def(graph, 1, None));
        assert!(matches!(
            register_vector_index(
                graph,
                1,
                EmbeddingNameId::from_raw(1),
                VectorIndexKind::IvfFlat,
                VectorMetric::L2Squared,
                VectorEncoding::F32,
                16,
                None,
                false,
            ),
            Err(RouterError::Conflict(_))
        ));
        // Idempotent replay with IF NOT EXISTS reports "not newly created".
        assert!(
            !register_vector_index(
                graph,
                1,
                EmbeddingNameId::from_raw(1),
                VectorIndexKind::IvfFlat,
                VectorMetric::L2Squared,
                VectorEncoding::F32,
                16,
                None,
                true,
            )
            .expect("if-not-exists replay")
        );
    }

    #[test]
    fn list_and_purge_are_graph_scoped() {
        let graph = GraphId::from_raw(920_006);
        let other = GraphId::from_raw(920_007);
        assert!(sample_def(graph, 1, None));
        assert!(sample_def(graph, 2, None));
        assert!(sample_def(other, 1, None));

        let listed = list_vector_indexes(graph);
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|d| d.index_id == 1));
        assert!(listed.iter().any(|d| d.index_id == 2));

        purge_graph_vector_indexes(graph);
        assert!(list_vector_indexes(graph).is_empty());
        // A different graph is untouched.
        assert_eq!(list_vector_indexes(other).len(), 1);
    }

    #[test]
    fn production_embedding_catalog_is_empty_while_fence_off() {
        let graph = GraphId::from_raw(920_020);
        // Even a fully-targeted definition stays DispatchBlocked, so the builder exports nothing.
        assert!(sample_def(
            graph,
            1,
            Some(VectorIndexTarget {
                canister: Principal::management_canister(),
            })
        ));
        let catalog = to_indexed_embedding_catalog(graph);
        assert!(
            catalog.is_empty(),
            "fail-closed: no DispatchEnabled defs while the incarnation fence is off"
        );
    }

    #[test]
    fn activation_block_reason_only_for_blocked_state() {
        assert_eq!(
            activation_block_reason(
                VectorIndexActivationState::DispatchBlockedMissingIncarnationFence
            ),
            Some(VectorActivationBlockReason::MissingEmbeddingIncarnationFence)
        );
        assert_eq!(
            activation_block_reason(VectorIndexActivationState::Registered),
            None
        );
        assert_eq!(
            activation_block_reason(VectorIndexActivationState::DispatchEnabled),
            None
        );
    }

    #[test]
    fn target_resolution_returns_the_single_canister() {
        let graph = GraphId::from_raw(920_010);
        let canister = Principal::management_canister();
        assert!(sample_def(graph, 1, Some(VectorIndexTarget { canister })));
        assert_eq!(
            vector_index_target_for(graph, 1).expect("resolve"),
            canister
        );
    }

    #[test]
    fn target_resolution_rejects_missing_def_and_unset_target() {
        let graph = GraphId::from_raw(920_011);
        assert!(matches!(
            vector_index_target_for(graph, 99),
            Err(RouterError::NotFound(_))
        ));
        assert!(sample_def(graph, 1, None));
        assert!(matches!(
            vector_index_target_for(graph, 1),
            Err(RouterError::Conflict(_))
        ));
    }

    #[test]
    fn range_scans_cover_the_max_graph_id() {
        let graph = GraphId::from_raw(u32::MAX);
        assert!(sample_def(graph, 1, None));
        assert!(sample_def(graph, 2, None));
        assert_eq!(list_vector_indexes(graph).len(), 2);
        purge_graph_vector_indexes(graph);
        assert!(list_vector_indexes(graph).is_empty());
    }
}
