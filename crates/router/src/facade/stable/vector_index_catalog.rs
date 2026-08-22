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
use gleaph_graph_kernel::entry::{EmbeddingNameId, GraphId, IndexNameId, VertexLabelId};
use gleaph_graph_kernel::vector_index::{
    IndexedEmbeddingCatalog, IndexedEmbeddingSpec, VectorEncoding, VectorIndexKind, VectorMetric,
};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};

use crate::facade::stable::{ROUTER_NEXT_VECTOR_INDEX_ID, ROUTER_VECTOR_INDEXES};
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

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct VectorIndexDefRecord {
    pub index_id: u32,
    /// Graph-scoped logical index name. This is distinct from the typed embedding field name.
    pub index_name_id: IndexNameId,
    pub embedding_name_id: EmbeddingNameId,
    /// Creation-fixed label set the index is scoped to (ADR 0064 §Router catalog); change = drop +
    /// recreate. The graph uses it as the label-membership upper bound for vector presence.
    pub labels: Vec<VertexLabelId>,
    pub kind: VectorIndexKind,
    pub metric: VectorMetric,
    pub encoding: VectorEncoding,
    pub dims: u16,
    /// `None` while `Registered`; set via [`set_vector_index_target`]. Always non-anonymous when set.
    pub target: Option<VectorIndexTarget>,
    pub activation_state: VectorIndexActivationState,
}

/// V1 stable shape retained only so upgrades fail with an explicit breaking-version diagnostic.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
struct VectorIndexDefRecordV1 {
    index_id: u32,
    embedding_name_id: EmbeddingNameId,
    labels: Vec<VertexLabelId>,
    kind: VectorIndexKind,
    metric: VectorMetric,
    encoding: VectorEncoding,
    dims: u16,
    target: Option<VectorIndexTarget>,
    activation_state: VectorIndexActivationState,
}

/// Versioned stable envelope (ADR 0007). ADR 0065 deliberately makes V2 breaking because a
/// distinct logical index name cannot be inferred from a V1 embedding name.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum VectorIndexDefStableRecord {
    V1(VectorIndexDefRecordV1),
    V2(VectorIndexDefRecord),
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
            Encode!(&VectorIndexDefStableRecord::V2(self.clone()))
                .expect("encode vector index def"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&VectorIndexDefStableRecord::V2(self)).expect("encode vector index def")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), VectorIndexDefStableRecord).expect("decode vector index def")
        {
            VectorIndexDefStableRecord::V1(_) => panic!(
                "vector index catalog V1 is incompatible with ADR 0065; reinstall with an empty V2 catalog"
            ),
            VectorIndexDefStableRecord::V2(v2) => v2,
        }
    }
}

/// Read-only allocator preflight. Allocation is monotonic and zero is permanently invalid.
pub(crate) fn preflight_allocate_vector_index_id() -> Result<(), RouterError> {
    next_available_vector_index_id().map(|_| ())
}

/// Allocate one opaque physical vector-index id. IDs are never rewound or reused.
pub(crate) fn allocate_vector_index_id() -> Result<u32, RouterError> {
    let raw = next_available_vector_index_id()?;
    ROUTER_NEXT_VECTOR_INDEX_ID.with_borrow_mut(|next_id| {
        next_id.set(raw + 1);
        Ok(raw)
    })
}

/// Find the first globally unused id at or after the allocator cursor. This explicitly bridges
/// legacy caller-assigned ids: a breaking V2 install may still exercise the legacy admin surface,
/// and a later DDL allocation must never collide with any definition it created.
fn next_available_vector_index_id() -> Result<u32, RouterError> {
    let mut raw = ROUTER_NEXT_VECTOR_INDEX_ID.with_borrow(|next_id| *next_id.get());
    if raw == 0 {
        return Err(RouterError::Internal(
            "vector index allocator stored zero".into(),
        ));
    }
    loop {
        let next = raw
            .checked_add(1)
            .ok_or_else(|| RouterError::IdExhausted("vector index id".into()))?;
        let occupied = ROUTER_VECTOR_INDEXES
            .with_borrow(|map| map.iter().any(|entry| entry.key().index_id == raw));
        if !occupied {
            return Ok(raw);
        }
        raw = next;
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

/// Resolve a registration's label strings to `VertexLabelId`s (ADR 0064 §Router catalog).
///
/// Rejects an empty label set and fails closed on an unknown label, so a rejected registration never
/// allocates an embedding name or a def. `admin_register_vector_index` calls this after preflight so a
/// conflict/no-op reports its own error instead of a label-resolution error.
pub(crate) fn resolve_vector_index_labels(
    store: &crate::facade::store::RouterStore,
    graph_id: GraphId,
    labels: &[String],
) -> Result<Vec<VertexLabelId>, RouterError> {
    if labels.is_empty() {
        return Err(RouterError::InvalidArgument(
            "labels must not be empty".to_owned(),
        ));
    }
    labels
        .iter()
        .map(|name| store.lookup_vertex_label_id(graph_id, name))
        .collect()
}

/// Register a new vector-index definition. The embedding is identified by an already-interned
/// [`EmbeddingNameId`] (resolved by name via [`super::embedding_name_catalog`]). Validation goes
/// through [`preflight_register`] (anonymous target rejected, conflicts/no-ops handled); the
/// activation state is computed by the fail-closed gate.
pub(crate) fn register_vector_index(
    graph_id: GraphId,
    index_id: u32,
    index_name_id: IndexNameId,
    embedding_name_id: EmbeddingNameId,
    labels: Vec<VertexLabelId>,
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

    if super::index_name_catalog::index_name(graph_id, index_name_id).is_none() {
        return Err(RouterError::InvalidArgument(format!(
            "logical vector index name id {} is not registered in this graph",
            index_name_id.raw()
        )));
    }
    if super::embedding_name_catalog::embedding_name(graph_id, embedding_name_id).is_none() {
        return Err(RouterError::InvalidArgument(format!(
            "embedding field name id {} is not registered in this graph",
            embedding_name_id.raw()
        )));
    }
    if super::indexed_catalog::get_named_index(graph_id, index_name_id).is_some() {
        return Err(RouterError::Conflict(format!(
            "logical index name id {} already belongs to a property index",
            index_name_id.raw()
        )));
    }

    let index_name_conflict = ROUTER_VECTOR_INDEXES.with_borrow(|map| {
        let start = VectorIndexKey::new(graph_id, 0);
        map.range((Bound::Included(start), graph_upper(graph_id)))
            .any(|entry| entry.value().index_name_id == index_name_id)
    });
    if index_name_conflict {
        return Err(RouterError::Conflict(format!(
            "logical index name id {} already has a vector index in this graph",
            index_name_id.raw()
        )));
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
        index_name_id,
        embedding_name_id,
        labels,
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

/// Assign the single target of an existing definition and recompute its activation state through
/// the fail-closed gate. Assignment is immutable: an unset target may be assigned once, an exact
/// replay is idempotent, and a different target is rejected. Rejects an anonymous principal.
pub(crate) fn set_vector_index_target(
    graph_id: GraphId,
    index_id: u32,
    target: VectorIndexTarget,
) -> Result<(), RouterError> {
    let target = reject_anonymous(target)?;
    let key = VectorIndexKey::new(graph_id, index_id);
    let current = ROUTER_VECTOR_INDEXES
        .with_borrow(|map| map.get(&key))
        .ok_or_else(|| RouterError::NotFound(format!("vector index {index_id}")))?;
    match current.target.map(|current| current.canister) {
        None => {}
        Some(current) if current == target.canister => return Ok(()),
        Some(current) => {
            return Err(RouterError::Conflict(format!(
                "vector index {index_id} target is immutable; already assigned to {current}"
            )));
        }
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

/// Test-only override of the stored activation state. Production dispatch paths must never use
/// this helper; it exists so unit tests can exercise search logic without requiring a fully
/// vector-attached shard fleet and enabled global flag.
#[cfg(test)]
pub(crate) fn set_vector_index_activation_state_for_test(
    graph_id: GraphId,
    index_id: u32,
    state: VectorIndexActivationState,
) -> Result<(), RouterError> {
    let key = VectorIndexKey::new(graph_id, index_id);
    ROUTER_VECTOR_INDEXES.with_borrow_mut(|map| {
        let mut def = map
            .get(&key)
            .ok_or_else(|| RouterError::NotFound(format!("vector index {index_id}")))?;
        def.activation_state = state;
        map.insert(key, def);
        Ok(())
    })
}

/// Reject a `requested` target that differs from any *other* definition's already-set target in the
/// graph (one vector-index target per graph; ADR 0031 Slice 4). `exclude_index_id` skips the
/// definition being registered or assigned so an exact target replay remains idempotent.
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

/// Resolve the single vector-index definition that derives from `embedding_name_id`, if any.
/// Registration enforces one vector index per embedding name per graph, so at most one record
/// matches.
pub(crate) fn get_vector_index_by_embedding_name_id(
    graph_id: GraphId,
    embedding_name_id: EmbeddingNameId,
) -> Option<VectorIndexDefRecord> {
    list_vector_indexes(graph_id)
        .into_iter()
        .find(|def| def.embedding_name_id == embedding_name_id)
}

/// Resolve one vector definition by its graph-scoped logical name. The catalog enforces uniqueness
/// at creation, so the bounded graph-local scan returns at most one record.
pub(crate) fn get_vector_index_by_name_id(
    graph_id: GraphId,
    index_name_id: IndexNameId,
) -> Option<VectorIndexDefRecord> {
    list_vector_indexes(graph_id)
        .into_iter()
        .find(|def| def.index_name_id == index_name_id)
}

/// Fail closed unless the dynamic per-graph vector dispatch gate is satisfied for `def`.
///
/// The gate is `global activation flag ON && every live shard of the graph vector-attached`. This
/// is the same check performed by the public `vector_search` surface and must precede any internal
/// Router → vector-canister forwarding (including empty-candidate early returns in GQL SEARCH).
pub(crate) fn assert_vector_search_dispatch_ready(
    graph_id: GraphId,
    store: &crate::facade::store::RouterStore,
    def: &VectorIndexDefRecord,
) -> Result<(), crate::state::RouterError> {
    let global_enabled =
        crate::facade::stable::vector_activation::vector_dispatch_globally_enabled();
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    if let Some(reason) =
        activation_block_reason(def.activation_state, global_enabled, dispatch_ready)
    {
        return Err(crate::state::RouterError::VectorDispatchActivationBlocked(
            reason,
        ));
    }
    Ok(())
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

/// Map a stored activation state to its public view (ADR 0056 §4; shared by the L2 catalog query
/// and the L3 activation-status query).
pub(crate) fn activation_state_view(
    state: VectorIndexActivationState,
) -> crate::types::VectorIndexActivationStateView {
    match state {
        VectorIndexActivationState::Registered => {
            crate::types::VectorIndexActivationStateView::Registered
        }
        VectorIndexActivationState::DispatchBlocked => {
            crate::types::VectorIndexActivationStateView::DispatchBlocked
        }
        VectorIndexActivationState::DispatchEnabled => {
            crate::types::VectorIndexActivationStateView::DispatchEnabled
        }
    }
}

/// Router view row for one derived vector-index definition (ADR 0031 Slice 3; shared by the L2
/// catalog query and the L3 activation-status query).
pub(crate) fn vector_index_info(
    def: &VectorIndexDefRecord,
    dispatch_ready: bool,
) -> crate::types::VectorIndexInfo {
    let effective = effective_activation_state(def.activation_state, dispatch_ready);
    crate::types::VectorIndexInfo {
        index_id: def.index_id,
        embedding_name_id: def.embedding_name_id.raw(),
        dims: def.dims,
        metric: def.metric,
        target: def.target.map(|t| t.canister),
        activation_state: activation_state_view(effective),
    }
}

/// Build the ephemeral indexed-embedding catalog the Router stamps onto `ExecutePlanArgs` for a
/// graph (ADR 0031), mirroring the Router's indexed-property catalog projection.
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
                labels: def.labels.clone(),
            })
            .collect()
    });
    IndexedEmbeddingCatalog { embeddings }
}

pub(crate) fn purge_graph_vector_indexes(graph_id: GraphId) -> Result<(), RouterError> {
    if super::vector_ingest_outbox::has_pending() {
        return Err(RouterError::Conflict(
            "cannot purge vector-index definitions while direct vector-ingest work remains pending"
                .into(),
        ));
    }
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
    Ok(())
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

    fn test_index_name_id(graph: GraphId, index_id: u32) -> IndexNameId {
        super::super::index_name_catalog::intern_index_name(
            graph,
            &format!("test_vector_index_{index_id}"),
        )
        .expect("intern test vector index name")
    }

    fn test_embedding_name_id(graph: GraphId, index_id: u32) -> EmbeddingNameId {
        super::super::embedding_name_catalog::intern_embedding_name(
            graph,
            &format!("test_embedding_field_{index_id}"),
        )
        .expect("intern test embedding field name")
    }

    fn sample_def(graph: GraphId, index_id: u32, target: Option<VectorIndexTarget>) -> bool {
        // Distinct embedding-name id per index so the one-index-per-embedding-name invariant does
        // not collapse unrelated test definitions in the same graph.
        register_vector_index(
            graph,
            index_id,
            test_index_name_id(graph, index_id),
            test_embedding_name_id(graph, index_id),
            vec![VertexLabelId::from_raw(1)],
            VectorIndexKind::IvfFlat,
            VectorMetric::L2Squared,
            VectorEncoding::F32,
            16,
            target,
            false,
        )
        .expect("register vector index")
    }

    fn pending_intent(
        graph: GraphId,
        index_id: u32,
        vector_target: Principal,
        mutation_id: u64,
        shard_id: u32,
    ) -> super::super::vector_ingest_outbox::VectorIngestOutboxState {
        let definition = get_vector_index(graph, index_id).expect("vector definition");
        let dims = definition.dims;
        super::super::vector_ingest_outbox::intent_for_test(
            super::super::vector_ingest_outbox::NewVectorIngestIntent {
                graph_id: graph,
                graph_target: Principal::from_slice(&[8; 29]),
                vector_target,
                shard_id: gleaph_graph_kernel::federation::ShardId::new(shard_id),
                local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId::from(1u32),
                spec: IndexedEmbeddingSpec {
                    embedding_name_id: definition.embedding_name_id.raw(),
                    index_id: definition.index_id,
                    kind: definition.kind,
                    metric: definition.metric,
                    encoding: definition.encoding,
                    dims,
                    labels: definition.labels,
                },
                bytes: vec![0; usize::from(dims) * 4],
            },
            mutation_id,
            super::super::vector_ingest_outbox::VectorIngestIntentPhase::AwaitingVector,
        )
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
    fn allocator_skips_legacy_caller_assigned_id() {
        let graph = GraphId::from_raw(920_900);
        let occupied = ROUTER_NEXT_VECTOR_INDEX_ID.with_borrow(|cell| *cell.get());
        register_vector_index(
            graph,
            occupied,
            test_index_name_id(graph, occupied),
            test_embedding_name_id(graph, occupied),
            vec![VertexLabelId::from_raw(1)],
            VectorIndexKind::IvfFlat,
            VectorMetric::L2Squared,
            VectorEncoding::F32,
            16,
            None,
            false,
        )
        .expect("legacy registration at allocator cursor");

        let allocated = allocate_vector_index_id().expect("allocate after legacy id");
        assert_ne!(allocated, occupied);
        assert!(allocated > occupied);
    }

    #[test]
    fn record_storable_roundtrip() {
        let record = VectorIndexDefRecord {
            index_id: 9,
            index_name_id: IndexNameId::from_raw(4),
            embedding_name_id: EmbeddingNameId::from_raw(3),
            labels: vec![VertexLabelId::from_raw(1), VertexLabelId::from_raw(2)],
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
            VectorIndexDefRecord::from_bytes(Cow::Owned(record.clone().into_bytes())),
            record
        );
    }

    #[test]
    #[should_panic(
        expected = "vector index catalog V1 is incompatible with ADR 0065; reinstall with an empty V2 catalog"
    )]
    fn v1_stable_record_is_rejected_explicitly() {
        let v1 = VectorIndexDefStableRecord::V1(VectorIndexDefRecordV1 {
            index_id: 9,
            embedding_name_id: EmbeddingNameId::from_raw(3),
            labels: vec![VertexLabelId::from_raw(1)],
            kind: VectorIndexKind::IvfFlat,
            metric: VectorMetric::L2Squared,
            encoding: VectorEncoding::F32,
            dims: 8,
            target: None,
            activation_state: VectorIndexActivationState::Registered,
        });
        let bytes = Encode!(&v1).expect("encode V1 fixture");
        let _ = VectorIndexDefRecord::from_bytes(Cow::Owned(bytes));
    }

    #[test]
    fn registration_rejects_unmapped_embedding_name_without_inserting() {
        let graph = GraphId::from_raw(920_901);
        let err = register_vector_index(
            graph,
            1,
            test_index_name_id(graph, 1),
            EmbeddingNameId::from_raw(777),
            vec![VertexLabelId::from_raw(1)],
            VectorIndexKind::IvfFlat,
            VectorMetric::L2Squared,
            VectorEncoding::F32,
            16,
            None,
            false,
        )
        .expect_err("unmapped embedding field id must fail closed");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
        assert!(get_vector_index(graph, 1).is_none());
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
    fn i8_encoding_registers_and_reads_back() {
        // The Router catalog records the stored encoding (I8) so the vector canister's lazy
        // `ensure_def_for_upsert` can create a matching def from the op's encoding.
        let graph = GraphId::from_raw(920_300);
        register_vector_index(
            graph,
            3,
            test_index_name_id(graph, 3),
            test_embedding_name_id(graph, 3),
            vec![VertexLabelId::from_raw(1)],
            VectorIndexKind::IvfFlat,
            VectorMetric::L2Squared,
            VectorEncoding::I8,
            16,
            None,
            false,
        )
        .expect("register i8");
        let def = get_vector_index(graph, 3).expect("def");
        assert_eq!(def.encoding, VectorEncoding::I8);
        assert_eq!(def.dims, 16);
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
    fn target_assignment_sets_unset_target() {
        let graph = GraphId::from_raw(920_003);
        assert!(sample_def(graph, 1, None));
        let target = VectorIndexTarget {
            canister: Principal::management_canister(),
        };
        set_vector_index_target(graph, 1, target).expect("set target");
        let def = get_vector_index(graph, 1).expect("def");
        assert_eq!(
            def.activation_state,
            VectorIndexActivationState::DispatchBlocked
        );
        assert_eq!(def.target, Some(target));
    }

    #[test]
    fn target_assignment_same_target_is_idempotent() {
        let graph = GraphId::from_raw(920_005);
        let target = VectorIndexTarget {
            canister: Principal::from_slice(&[5u8; 29]),
        };
        assert!(sample_def(graph, 1, Some(target)));
        let before = get_vector_index(graph, 1).expect("definition before replay");

        set_vector_index_target(graph, 1, target).expect("same target replay");

        assert_eq!(
            get_vector_index(graph, 1).expect("definition after replay"),
            before,
            "an exact target replay must preserve the complete catalog row"
        );
    }

    #[test]
    fn target_assignment_different_target_rejects_without_mutation() {
        let _guard = super::super::vector_ingest_outbox::test_lock();
        let graph = GraphId::from_raw(920_006);
        let current_target = VectorIndexTarget {
            canister: Principal::from_slice(&[5u8; 29]),
        };
        let replacement_target = VectorIndexTarget {
            canister: Principal::from_slice(&[6u8; 29]),
        };
        assert!(sample_def(graph, 1, Some(current_target)));
        let before = get_vector_index(graph, 1).expect("definition before conflict");

        let intent = pending_intent(graph, 1, current_target.canister, 1, 1);
        super::super::vector_ingest_outbox::insert_intents_for_test(&[intent])
            .expect("append pending ingestion");
        let pending_before = super::super::vector_ingest_outbox::scan(None, 16).0;

        let error = set_vector_index_target(graph, 1, replacement_target)
            .expect_err("a different target must be rejected");
        assert_eq!(
            error,
            RouterError::Conflict(format!(
                "vector index 1 target is immutable; already assigned to {}",
                current_target.canister
            ))
        );
        assert_eq!(
            get_vector_index(graph, 1).expect("definition remains after rejected assignment"),
            before,
            "a rejected target assignment must preserve the complete catalog row"
        );
        assert_eq!(
            super::super::vector_ingest_outbox::scan(None, 16).0,
            pending_before,
            "a rejected target assignment must preserve pending outbox state"
        );
        super::super::ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| table.clear_new());
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
                test_index_name_id(graph, 1),
                test_embedding_name_id(graph, 1),
                vec![VertexLabelId::from_raw(1)],
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
                test_index_name_id(graph, 1),
                test_embedding_name_id(graph, 1),
                vec![VertexLabelId::from_raw(1)],
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
                test_index_name_id(graph, 1),
                test_embedding_name_id(graph, 1),
                vec![VertexLabelId::from_raw(1)],
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
                test_index_name_id(graph, 1),
                test_embedding_name_id(graph, 5),
                vec![VertexLabelId::from_raw(1)],
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
                test_index_name_id(graph, 2),
                test_embedding_name_id(graph, 5),
                vec![VertexLabelId::from_raw(1)],
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
                test_index_name_id(other, 2),
                test_embedding_name_id(other, 5),
                vec![VertexLabelId::from_raw(1)],
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

        purge_graph_vector_indexes(graph).expect("purge vector indexes");
        assert!(list_vector_indexes(graph).is_empty());
        // A different graph is untouched.
        assert_eq!(list_vector_indexes(other).len(), 1);
    }

    #[test]
    fn purge_rejects_pending_direct_vector_work_before_catalog_mutation() {
        let _guard = super::super::vector_ingest_outbox::test_lock();
        let graph = GraphId::from_raw(920_008);
        assert!(sample_def(graph, 1, None));
        let before = list_vector_indexes(graph);
        let intent = pending_intent(graph, 1, Principal::from_slice(&[9; 29]), 1, 0);
        super::super::vector_ingest_outbox::insert_intents_for_test(&[intent])
            .expect("pending vector work");

        let err = purge_graph_vector_indexes(graph)
            .expect_err("pending vector work must block catalog purge");
        assert!(matches!(err, RouterError::Conflict(_)), "{err:?}");
        assert_eq!(list_vector_indexes(graph), before);
        super::super::ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| table.clear_new());
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
        // Ready ⇒ the targeted definition is exported with its creation-fixed label set.
        let catalog = to_indexed_embedding_catalog(graph, true);
        assert_eq!(catalog.embeddings.len(), 1);
        assert_eq!(catalog.embeddings[0].index_id, 1);
        assert_eq!(
            catalog.embeddings[0].labels,
            vec![VertexLabelId::from_raw(1)],
            "the spec carries the def's creation-fixed label set"
        );
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
                test_index_name_id(graph, 2),
                test_embedding_name_id(graph, 2),
                vec![VertexLabelId::from_raw(1)],
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
    fn range_scans_cover_the_max_graph_id() {
        let graph = GraphId::from_raw(u32::MAX);
        assert!(sample_def(graph, 1, None));
        assert!(sample_def(graph, 2, None));
        assert_eq!(list_vector_indexes(graph).len(), 2);
        purge_graph_vector_indexes(graph).expect("purge vector indexes");
        assert!(list_vector_indexes(graph).is_empty());
    }
}
