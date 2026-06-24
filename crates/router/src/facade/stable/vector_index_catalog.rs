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
//! ## Activation gate (fail-closed, ADR 0031 Slice 4)
//!
//! The delete-spanning incarnation fence now exists (graph-owned `embedding_incarnation`), so the
//! remaining gate is operational, **dynamic**, and computed at read time — never stored:
//! `dispatch_ready = global activation flag ON && every live shard of the graph vector-attached`
//! (see [`super::vector_activation`] and `RouterStore::graph_vector_dispatch_ready`). A definition's
//! stored [`VectorIndexActivationState`] is the *static* classification (`Registered` with no
//! target, else `DispatchBlocked`); the effective state and the catalog export are recomputed from
//! `dispatch_ready` on every read so flipping the flag (or attaching shards) takes effect at once.

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

/// Lifecycle of a vector-index definition (ADR 0031). The **stored** state is static
/// (`Registered`/`DispatchBlocked`); `DispatchEnabled` is only ever produced dynamically by
/// [`effective_activation_state`] from the operational `dispatch_ready` gate, never persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) enum VectorIndexActivationState {
    /// Definition created; a target may not be set yet.
    Registered,
    /// Target + shape metadata complete, but production dispatch/backfill are not currently live
    /// (the global activation flag is off, or the graph's shards are not all vector-attached).
    DispatchBlocked,
    /// Dispatch is live: the global flag is on and every live shard of the graph is vector-attached.
    DispatchEnabled,
}

/// The static classification stored for a definition: `Registered` with no target, else
/// `DispatchBlocked`. The dynamic `DispatchEnabled` promotion happens at read time only.
fn resolve_activation_state(has_target: bool) -> VectorIndexActivationState {
    if has_target {
        VectorIndexActivationState::DispatchBlocked
    } else {
        VectorIndexActivationState::Registered
    }
}

/// The effective activation state of a stored definition given the per-graph `dispatch_ready` gate
/// (global flag ON && all live shards vector-attached). A targeted def is `DispatchEnabled` iff
/// `dispatch_ready`; otherwise it stays `DispatchBlocked`. A def with no target is always
/// `Registered`.
pub(crate) fn effective_activation_state(
    stored: VectorIndexActivationState,
    dispatch_ready: bool,
) -> VectorIndexActivationState {
    match stored {
        VectorIndexActivationState::Registered => VectorIndexActivationState::Registered,
        VectorIndexActivationState::DispatchBlocked
        | VectorIndexActivationState::DispatchEnabled => {
            if dispatch_ready {
                VectorIndexActivationState::DispatchEnabled
            } else {
                VectorIndexActivationState::DispatchBlocked
            }
        }
    }
}

/// The fail-closed block reason for a targeted, not-yet-dispatching definition (ADR 0031 Slice 4).
/// `global_enabled` is the operator flag; `dispatch_ready` additionally requires all live shards
/// vector-attached. Returns `None` when the def has no target (`Registered`) or is dispatching.
pub(crate) fn activation_block_reason(
    stored: VectorIndexActivationState,
    global_enabled: bool,
    dispatch_ready: bool,
) -> Option<VectorActivationBlockReason> {
    match stored {
        VectorIndexActivationState::Registered => None,
        VectorIndexActivationState::DispatchBlocked
        | VectorIndexActivationState::DispatchEnabled => {
            if dispatch_ready {
                None
            } else if !global_enabled {
                Some(VectorActivationBlockReason::DispatchNotActivated)
            } else {
                Some(VectorActivationBlockReason::ShardsNotVectorAttached)
            }
        }
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

/// Validate a registration request **without mutating any state** (ADR 0031 Slice 3/4). This is the
/// single source of truth for the register decision: it rejects an anonymous target, a conflicting
/// `index_id`, and a target that violates one-target-per-graph, and reports an idempotent no-op for
/// `if_not_exists`.
///
/// `admin_register_vector_index` calls this *before* interning the embedding name so a rejected or
/// no-op registration never allocates a durable [`EmbeddingNameId`] (which would pollute the
/// graph-scoped name catalog and could exhaust the `u16` name space through failed DDL). The
/// target-consistency check lives here (not only inside [`register_vector_index`]) for exactly that
/// reason: a target conflict must fail closed before any side effect.
pub(crate) fn preflight_register(
    graph_id: GraphId,
    index_id: u32,
    target: Option<VectorIndexTarget>,
    if_not_exists: bool,
) -> Result<RegisterPreflight, RouterError> {
    if let Some(target) = target {
        let target = reject_anonymous(target)?;
        ensure_target_consistent(graph_id, index_id, target.canister)?;
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

    // One vector index per embedding name per graph (ADR 0031 Slice 4): dispatch/backfill key by
    // `embedding_name_id`, so a second index on the same embedding would have its writes silently
    // collapsed onto a single target. Reject before insert. The name was interned by the caller; for
    // a brand-new name no existing def can match, so this only fires on genuine reuse.
    let embedding_name_conflict = ROUTER_VECTOR_INDEXES.with_borrow(|map| {
        let start = VectorIndexKey::new(graph_id, 0);
        map.range((Bound::Included(start), graph_upper(graph_id)))
            .any(|entry| entry.value().embedding_name_id == embedding_name_id)
    });
    if embedding_name_conflict {
        return Err(RouterError::Conflict(format!(
            "embedding name id {} already has a vector index in this graph",
            embedding_name_id.raw()
        )));
    }

    // One vector-index target per graph is enforced pre-intern by `preflight_register` (called
    // above), so a target conflict has already failed closed without allocating an embedding name.

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
    if !ROUTER_VECTOR_INDEXES.with_borrow(|map| map.contains_key(&key)) {
        return Err(RouterError::NotFound(format!("vector index {index_id}")));
    }
    // One vector-index target per graph (ADR 0031 Slice 4): a target differing from any *other*
    // def's already-set target is a misrouting hazard. Checked before the mutation (and after the
    // existence check above, so a missing def still reports `NotFound`).
    ensure_target_consistent(graph_id, index_id, target.canister)?;
    ROUTER_VECTOR_INDEXES.with_borrow_mut(|map| {
        let mut def = map.get(&key).expect("existence checked above");
        def.target = Some(target);
        def.activation_state = resolve_activation_state(true);
        map.insert(key, def);
    });
    Ok(())
}

/// Reject a `requested` target that differs from any *other* definition's already-set target in the
/// graph (one vector-index target per graph; ADR 0031 Slice 4). `exclude_index_id` skips the
/// definition being registered/retargeted so re-setting the same def's target is a no-op.
fn ensure_target_consistent(
    graph_id: GraphId,
    exclude_index_id: u32,
    requested: Principal,
) -> Result<(), RouterError> {
    let conflict = ROUTER_VECTOR_INDEXES.with_borrow(|map| {
        let start = VectorIndexKey::new(graph_id, 0);
        map.range((Bound::Included(start), graph_upper(graph_id)))
            .filter_map(|entry| {
                let def = entry.value();
                (def.index_id != exclude_index_id)
                    .then_some(def.target)
                    .flatten()
            })
            .map(|t| t.canister)
            .find(|&existing| existing != requested)
    });
    match conflict {
        Some(existing) => Err(RouterError::Conflict(format!(
            "graph already targets vector canister {existing}; one vector-index target per graph"
        ))),
        None => Ok(()),
    }
}

/// The single vector-index target principal for a graph, derived from its definitions. `None` when
/// no definition has a target yet. With the one-target-per-graph invariant every targeted def shares
/// one principal, so this returns the first target found (defensively still consistent under the
/// invariant). Used by the readiness predicate to require each live shard be attached to *this*
/// target, not merely to some non-anonymous canister.
pub(crate) fn graph_single_target(graph_id: GraphId) -> Option<Principal> {
    ROUTER_VECTOR_INDEXES.with_borrow(|map| {
        let start = VectorIndexKey::new(graph_id, 0);
        map.range((Bound::Included(start), graph_upper(graph_id)))
            .find_map(|entry| entry.value().target.map(|t| t.canister))
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
/// graph (ADR 0031), mirroring `to_indexed_property_catalog`.
///
/// **This is the single fail-closed activation gate.** When `dispatch_ready` is `false` the catalog
/// is empty, so `vector_dispatch::spec_for` returns `None` and derived vector sync stays inert.
/// `dispatch_ready` must be the per-graph predicate (global activation flag ON **and** every live
/// shard vector-attached); the caller computes it via `RouterStore::graph_vector_dispatch_ready` so
/// this lower stable layer does not reach up into the shard registry. With one vector-index target
/// per graph, every targeted definition is exported when ready.
pub(crate) fn to_indexed_embedding_catalog(
    graph_id: GraphId,
    dispatch_ready: bool,
) -> IndexedEmbeddingCatalog {
    if !dispatch_ready {
        return IndexedEmbeddingCatalog {
            embeddings: Vec::new(),
        };
    }
    let embeddings = ROUTER_VECTOR_INDEXES.with_borrow(|map| {
        let start = VectorIndexKey::new(graph_id, 0);
        map.range((Bound::Included(start), graph_upper(graph_id)))
            .map(|entry| entry.value())
            .filter(|def| def.target.is_some())
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

/// Build a **single-definition** indexed-embedding catalog for the requested `index_id` (ADR 0031
/// Slice 5 backfill). Unlike [`to_indexed_embedding_catalog`], this restricts the shard worker to
/// the requested definition's embedding so a per-index backfill cannot populate sibling indexes that
/// happen to share a ready graph. Empty unless the definition exists, is targeted, and
/// `dispatch_ready`.
pub(crate) fn to_indexed_embedding_catalog_for_index(
    graph_id: GraphId,
    index_id: u32,
    dispatch_ready: bool,
) -> IndexedEmbeddingCatalog {
    let embeddings = if dispatch_ready {
        get_vector_index(graph_id, index_id)
            .filter(|def| def.target.is_some())
            .map(|def| IndexedEmbeddingSpec {
                embedding_name_id: def.embedding_name_id.raw(),
                index_id: def.index_id,
                kind: def.kind,
                metric: def.metric,
                encoding: def.encoding,
                dims: def.dims,
            })
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
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
        // Distinct embedding-name id per index so the one-index-per-embedding-name invariant does
        // not collapse unrelated test definitions in the same graph.
        register_vector_index(
            graph,
            index_id,
            EmbeddingNameId::from_raw(index_id as u16),
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
            activation_state: VectorIndexActivationState::DispatchBlocked,
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
    fn registration_with_target_is_dispatch_blocked_until_ready() {
        let graph = GraphId::from_raw(920_002);
        let target = VectorIndexTarget {
            canister: Principal::management_canister(),
        };
        assert!(sample_def(graph, 1, Some(target)));
        let def = get_vector_index(graph, 1).expect("def");
        assert_eq!(
            def.activation_state,
            VectorIndexActivationState::DispatchBlocked,
            "a targeted def stores DispatchBlocked; DispatchEnabled is computed dynamically"
        );
        assert_eq!(
            effective_activation_state(def.activation_state, false),
            VectorIndexActivationState::DispatchBlocked
        );
        assert_eq!(
            effective_activation_state(def.activation_state, true),
            VectorIndexActivationState::DispatchEnabled
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
            VectorIndexActivationState::DispatchBlocked
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
    fn second_index_on_same_embedding_name_conflicts() {
        let graph = GraphId::from_raw(920_040);
        assert!(
            register_vector_index(
                graph,
                1,
                EmbeddingNameId::from_raw(5),
                VectorIndexKind::IvfFlat,
                VectorMetric::L2Squared,
                VectorEncoding::F32,
                16,
                None,
                false,
            )
            .expect("first index")
        );
        // A different index_id but the SAME embedding name must be rejected.
        assert!(matches!(
            register_vector_index(
                graph,
                2,
                EmbeddingNameId::from_raw(5),
                VectorIndexKind::IvfFlat,
                VectorMetric::L2Squared,
                VectorEncoding::F32,
                16,
                None,
                false,
            ),
            Err(RouterError::Conflict(_))
        ));
        // A different graph with the same name id is fine (graph-scoped).
        let other = GraphId::from_raw(920_041);
        assert!(
            register_vector_index(
                other,
                2,
                EmbeddingNameId::from_raw(5),
                VectorIndexKind::IvfFlat,
                VectorMetric::L2Squared,
                VectorEncoding::F32,
                16,
                None,
                false,
            )
            .expect("other graph same name id")
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
    fn production_embedding_catalog_gated_on_dispatch_ready() {
        let graph = GraphId::from_raw(920_020);
        assert!(sample_def(
            graph,
            1,
            Some(VectorIndexTarget {
                canister: Principal::management_canister(),
            })
        ));
        // Fail-closed: not ready ⇒ empty catalog, so derived vector sync stays inert.
        assert!(
            to_indexed_embedding_catalog(graph, false).is_empty(),
            "fail-closed: no specs exported while dispatch is not ready"
        );
        // Ready ⇒ the targeted definition is exported.
        let catalog = to_indexed_embedding_catalog(graph, true);
        assert_eq!(catalog.embeddings.len(), 1);
        assert_eq!(catalog.embeddings[0].index_id, 1);
    }

    #[test]
    fn activation_block_reason_reflects_dynamic_gate() {
        // No target ⇒ never blocked.
        assert_eq!(
            activation_block_reason(VectorIndexActivationState::Registered, true, true),
            None
        );
        // Targeted + ready ⇒ not blocked.
        assert_eq!(
            activation_block_reason(VectorIndexActivationState::DispatchBlocked, true, true),
            None
        );
        // Targeted + global flag off ⇒ blocked on activation.
        assert_eq!(
            activation_block_reason(VectorIndexActivationState::DispatchBlocked, false, false),
            Some(VectorActivationBlockReason::DispatchNotActivated)
        );
        // Targeted + global flag on but shards not attached ⇒ blocked on attach.
        assert_eq!(
            activation_block_reason(VectorIndexActivationState::DispatchBlocked, true, false),
            Some(VectorActivationBlockReason::ShardsNotVectorAttached)
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
    fn second_index_with_different_target_conflicts() {
        let graph = GraphId::from_raw(920_050);
        let target_a = VectorIndexTarget {
            canister: Principal::management_canister(),
        };
        let target_b = VectorIndexTarget {
            canister: Principal::from_slice(&[4u8; 29]),
        };
        // index 1 -> A (distinct embedding name via sample_def).
        assert!(sample_def(graph, 1, Some(target_a)));
        // A different index pointing at a *different* canister must be rejected (one target/graph).
        assert!(matches!(
            register_vector_index(
                graph,
                2,
                EmbeddingNameId::from_raw(2),
                VectorIndexKind::IvfFlat,
                VectorMetric::L2Squared,
                VectorEncoding::F32,
                16,
                Some(target_b),
                false,
            ),
            Err(RouterError::Conflict(_))
        ));
        assert!(
            get_vector_index(graph, 2).is_none(),
            "a target-conflicting registration must not persist"
        );
        // The same target is allowed; the graph keeps a single resolved target.
        assert!(sample_def(graph, 2, Some(target_a)));
        assert_eq!(graph_single_target(graph), Some(target_a.canister));
    }

    #[test]
    fn set_target_to_different_principal_conflicts() {
        let graph = GraphId::from_raw(920_051);
        let target_a = VectorIndexTarget {
            canister: Principal::management_canister(),
        };
        let target_b = VectorIndexTarget {
            canister: Principal::from_slice(&[4u8; 29]),
        };
        assert!(sample_def(graph, 1, Some(target_a)));
        assert!(sample_def(graph, 2, None));
        // Retargeting index 2 to a different canister than index 1's target is rejected.
        assert!(matches!(
            set_vector_index_target(graph, 2, target_b),
            Err(RouterError::Conflict(_))
        ));
        // Re-setting index 1 to its own existing target is a no-op (excluded from the scan).
        set_vector_index_target(graph, 1, target_a).expect("idempotent re-set");
        // Setting index 2 to the shared target succeeds.
        set_vector_index_target(graph, 2, target_a).expect("shared target");
        assert_eq!(graph_single_target(graph), Some(target_a.canister));
    }

    #[test]
    fn backfill_catalog_scopes_to_requested_index() {
        let graph = GraphId::from_raw(920_052);
        let target = VectorIndexTarget {
            canister: Principal::management_canister(),
        };
        assert!(sample_def(graph, 1, Some(target)));
        assert!(sample_def(graph, 2, Some(target)));
        // Not ready ⇒ empty regardless of index.
        assert!(to_indexed_embedding_catalog_for_index(graph, 1, false).is_empty());
        // Ready ⇒ exactly the requested index's single spec (not the sibling def).
        let catalog = to_indexed_embedding_catalog_for_index(graph, 1, true);
        assert_eq!(catalog.embeddings.len(), 1);
        assert_eq!(catalog.embeddings[0].index_id, 1);
        let catalog2 = to_indexed_embedding_catalog_for_index(graph, 2, true);
        assert_eq!(catalog2.embeddings.len(), 1);
        assert_eq!(catalog2.embeddings[0].index_id, 2);
        // A targetless / missing index yields an empty catalog even when ready.
        assert!(sample_def(graph, 3, None));
        assert!(to_indexed_embedding_catalog_for_index(graph, 3, true).is_empty());
        assert!(to_indexed_embedding_catalog_for_index(graph, 99, true).is_empty());
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
