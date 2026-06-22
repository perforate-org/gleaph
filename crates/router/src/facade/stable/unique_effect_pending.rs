//! Router-owned pending unique-effect index (ADR 0030 slice 6) — the durable discovery source for
//! Driver 2's effect recovery.
//!
//! A row `(graph_id, mutation_id, shard_id) → pinned graph canister` is registered at every dispatch
//! that may emit a unique effect (an `Acquire` from a constrained INSERT, a `Release` from a
//! constrained DELETE/REMOVE), **before the first dispatch `await`**, so it co-commits with the
//! reservation/envelope. After a crash between a shard's canonical write (and its pinned effect) and
//! the inline Confirm/reconcile, this row is the only durable handle back to the shard holding the
//! still-pinned effect.
//!
//! The pinned **canister** is captured in the row, not re-derived from the live shard registry: a
//! shard id can be unregistered and reused by a different canister, so recovery must query the exact
//! canister the effect was pinned on (ADR 0030 §Timeout, same reasoning as `ProofShard`). This makes
//! the index a superset discovery source for **both** effect kinds — including an orphan `Acquire`
//! whose reservation is gone, which Driver 1 (reservation-driven) can never find.
//!
//! Rows are keyed so a `(graph_id, mutation_id)` prefix is contiguous, letting Driver 2 enumerate
//! all shards of one mutation together. A row is removed only once a fresh `cursor=None`
//! re-enumeration of that shard's mutation effects comes back empty (every effect acked); Driver 2
//! owns that removal contract.

use std::borrow::Cow;
use std::ops::Bound;

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};

use crate::facade::stable::ROUTER_UNIQUE_EFFECT_PENDING;
use crate::facade::stable::label_stats::ClientMutationKey;

/// Fixed key width: `graph_id` (4) + `mutation_id` (8) + `shard_id` (4).
const KEY_LEN: usize = 4 + 8 + 4;

/// Current schema version of [`PendingEffectRecord`]. Bumped only on a breaking value-layout change;
/// `from_bytes` rejects an unknown version rather than silently mis-decoding.
const PENDING_EFFECT_SCHEMA_V1: u16 = 1;

/// Identity of one pending-effect discovery row. `Ord` (and thus `StableBTreeMap` ordering) is
/// `graph_id`, then `mutation_id`, then `shard_id`, so one mutation's shards form a contiguous range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UniqueEffectPendingKey {
    pub graph_id: GraphId,
    pub mutation_id: MutationId,
    pub shard_id: ShardId,
}

impl UniqueEffectPendingKey {
    pub fn new(graph_id: GraphId, mutation_id: MutationId, shard_id: ShardId) -> Self {
        Self {
            graph_id,
            mutation_id,
            shard_id,
        }
    }
}

impl Storable for UniqueEffectPendingKey {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: KEY_LEN as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = Vec::with_capacity(KEY_LEN);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.mutation_id.to_le_bytes());
        out.extend_from_slice(&self.shard_id.to_le_bytes());
        Cow::Owned(out)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let graph_id = GraphId::from_le_bytes(bytes[0..4].try_into().expect("graph_id"));
        let mutation_id = u64::from_le_bytes(bytes[4..12].try_into().expect("mutation_id"));
        let shard_id = ShardId::from_le_bytes(bytes[12..16].try_into().expect("shard_id"));
        Self {
            graph_id,
            mutation_id,
            shard_id,
        }
    }
}

/// Recovery disposition of a pending-effect row, for Driver 2.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "branched on by Driver 2's effect recovery (next commit)"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) enum PendingEffectState {
    /// A normal recovery target: an Acquire/Release awaiting reconcile + ack.
    Active,
    /// An orphan `Acquire` (no reservation) or a repeatedly-unresolved row, parked on a long
    /// re-check backoff so recovery never hot-loops it. Never acked; kept as durable evidence.
    Quarantined,
}

/// Versioned value of a pending-effect discovery row. Stored as a record (not a bare `Principal`)
/// from the outset so the orphan diagnostic, quarantine state, and backoff clock can be added
/// without a breaking stable-region value-layout change (ADR 0030 slice 6).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct PendingEffectRecord {
    /// Value schema version (see [`PENDING_EFFECT_SCHEMA_V1`]).
    pub schema_version: u16,
    /// The pinned graph canister the effect was emitted on, captured verbatim so recovery reaches
    /// the exact canister even after the shard is unregistered/reused. This is the row's stable
    /// identity: [`register`] refuses to change it.
    pub canister: Principal,
    /// The owning mutation's client key, captured verbatim so Driver 2 can resolve the
    /// `RouterMutationRecord` (its terminal-completion proof) for **any** effect kind — a `Release`
    /// or an orphan `Acquire` owns no reservation, so the reservation reverse index cannot resolve
    /// them. Self-contained in the row, it survives unregistration of the reservation index.
    pub client_key: ClientMutationKey,
    /// Recovery disposition.
    pub state: PendingEffectState,
    /// Earliest time Driver 2 should (re-)attempt this row; the quarantine/backoff gate.
    pub next_retry_ns: u64,
    /// Recovery attempts so far — diagnostic and backoff input.
    pub attempts: u32,
    /// Last diagnostic for a held/orphan row (persistent diagnostic surface).
    pub diagnostic: Option<String>,
}

impl PendingEffectRecord {
    /// A freshly registered, `Active` row with no attempts and no backoff.
    fn new_active(canister: Principal, client_key: ClientMutationKey) -> Self {
        Self {
            schema_version: PENDING_EFFECT_SCHEMA_V1,
            canister,
            client_key,
            state: PendingEffectState::Active,
            next_retry_ns: 0,
            attempts: 0,
            diagnostic: None,
        }
    }
}

impl Storable for PendingEffectRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode PendingEffectRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode PendingEffectRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let record =
            Decode!(bytes.as_ref(), PendingEffectRecord).expect("decode PendingEffectRecord");
        assert!(
            record.schema_version == PENDING_EFFECT_SCHEMA_V1,
            "unknown PendingEffectRecord schema version {} (ADR 0030 slice 6)",
            record.schema_version
        );
        record
    }
}

/// One discovered pending-effect row: which shard (by `key`) and its recovery record.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by Driver 2's effect recovery (next commit)"
    )
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingEffectRow {
    pub key: UniqueEffectPendingKey,
    pub record: PendingEffectRecord,
}

/// Register the discovery row for a dispatch that may emit a unique effect. Run before the first
/// dispatch `await`, so it co-commits with the reservation/envelope.
///
/// **Fail-closed identity:** a row's pinned `canister` is its stable identity. A deterministic replay
/// re-registers the *same* `(key → canister)` and is a no-op that preserves the existing record (its
/// quarantine/backoff/diagnostic state must survive replay). Re-registering the same `(graph, mutation,
/// shard)` to a *different* canister is an identity violation — it would orphan the old canister's
/// still-pinned effect — so it **traps** rather than silently overwriting (ADR 0030 slice 6).
pub(crate) fn register(
    graph_id: GraphId,
    mutation_id: MutationId,
    shard_id: ShardId,
    canister: Principal,
    client_key: ClientMutationKey,
) {
    let key = UniqueEffectPendingKey::new(graph_id, mutation_id, shard_id);
    ROUTER_UNIQUE_EFFECT_PENDING.with_borrow_mut(|table| {
        if let Some(existing) = table.get(&key) {
            assert!(
                existing.canister == canister,
                "pending unique-effect row {key:?} is already pinned to canister {} and cannot be \
                 re-registered to a different canister {canister}; the (graph, mutation, shard) → \
                 canister identity is immutable (ADR 0030 slice 6)",
                existing.canister
            );
            assert!(
                existing.client_key == client_key,
                "pending unique-effect row {key:?} is already owned by client key {:?} and cannot \
                 be re-registered to a different client key {client_key:?}; the (graph, mutation, \
                 shard) → owning-record identity is immutable (ADR 0030 slice 6)",
                existing.client_key
            );
            return;
        }
        table.insert(key, PendingEffectRecord::new_active(canister, client_key));
    });
}

/// Move a row into `Quarantined` with a fresh re-check backoff and a persistent diagnostic. Used for
/// an orphan `Acquire` (no reservation, mutation effect-generation already terminated): the row and
/// its evidence are kept — never acked — but parked so recovery does not hot-loop it. A missing row
/// is a no-op (a concurrent drain removed it).
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by Driver 2's effect recovery (next commit)"
    )
)]
pub(crate) fn quarantine(
    graph_id: GraphId,
    mutation_id: MutationId,
    shard_id: ShardId,
    next_retry_ns: u64,
    diagnostic: String,
) {
    let key = UniqueEffectPendingKey::new(graph_id, mutation_id, shard_id);
    ROUTER_UNIQUE_EFFECT_PENDING.with_borrow_mut(|table| {
        if let Some(mut record) = table.get(&key) {
            record.state = PendingEffectState::Quarantined;
            record.attempts = record.attempts.saturating_add(1);
            record.next_retry_ns = next_retry_ns;
            record.diagnostic = Some(diagnostic);
            table.insert(key, record);
        }
    });
}

/// Remove one discovery row. Driver 2 calls this only after proving the shard has no remaining
/// un-acked effects for the mutation (a fresh `cursor=None` enumeration came back empty).
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by Driver 2's effect recovery (next commit)"
    )
)]
pub(crate) fn remove(graph_id: GraphId, mutation_id: MutationId, shard_id: ShardId) {
    ROUTER_UNIQUE_EFFECT_PENDING.with_borrow_mut(|table| {
        table.remove(&UniqueEffectPendingKey::new(
            graph_id,
            mutation_id,
            shard_id,
        ));
    });
}

/// The recovery record for one discovery row, if present.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by Driver 2's effect recovery (next commit)"
    )
)]
pub(crate) fn lookup(
    graph_id: GraphId,
    mutation_id: MutationId,
    shard_id: ShardId,
) -> Option<PendingEffectRecord> {
    ROUTER_UNIQUE_EFFECT_PENDING.with_borrow(|table| {
        table.get(&UniqueEffectPendingKey::new(
            graph_id,
            mutation_id,
            shard_id,
        ))
    })
}

/// `true` while at least one pending-effect row exists for `(graph_id, mutation_id)`. The owning
/// `RouterMutationRecord` is the terminal-completion proof Driver 2 reads before removing a row, so
/// that record must be GC-pinned while any row remains (ADR 0030 slice 6). A `(graph_id, mutation_id)`
/// prefix is contiguous, so this is a single bounded range probe.
pub(crate) fn pending_effect_pinned(graph_id: GraphId, mutation_id: MutationId) -> bool {
    ROUTER_UNIQUE_EFFECT_PENDING.with_borrow(|table| {
        let start = UniqueEffectPendingKey::new(graph_id, mutation_id, ShardId::new(0));
        table
            .range((
                Bound::Included(start),
                mutation_range_upper(graph_id, mutation_id),
            ))
            .next()
            .is_some()
    })
}

/// Bounded, cursor-based work discovery for Driver 2: up to `budget` rows after `start_after`, plus
/// the last key examined (the next cursor) and the count scanned. Read-only.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by Driver 2's effect recovery (next commit)"
    )
)]
pub(crate) fn scan(
    start_after: Option<&UniqueEffectPendingKey>,
    budget: usize,
) -> (Vec<PendingEffectRow>, Option<UniqueEffectPendingKey>, u32) {
    let mut scanned: u32 = 0;
    let mut last_key: Option<UniqueEffectPendingKey> = None;
    let mut rows: Vec<PendingEffectRow> = Vec::new();
    ROUTER_UNIQUE_EFFECT_PENDING.with_borrow(|table| {
        let lower = match start_after {
            Some(key) => Bound::Excluded(*key),
            None => Bound::Unbounded,
        };
        for entry in table.range((lower, Bound::Unbounded)).take(budget) {
            let key = *entry.key();
            scanned += 1;
            rows.push(PendingEffectRow {
                key,
                record: entry.value(),
            });
            last_key = Some(key);
        }
    });
    (rows, last_key, scanned)
}

/// Exclusive upper bound of one graph's key range (`graph_id` is the most-significant component).
/// `Unbounded` at `GraphId::MAX`, where a saturating `+1` would collapse to an empty range and skip
/// the max graph.
fn graph_range_upper(graph_id: GraphId) -> Bound<UniqueEffectPendingKey> {
    match graph_id.raw().checked_add(1) {
        Some(next) => Bound::Excluded(UniqueEffectPendingKey::new(
            GraphId::from_raw(next),
            0,
            ShardId::new(0),
        )),
        None => Bound::Unbounded,
    }
}

/// Exclusive upper bound of one `(graph_id, mutation_id)` prefix. When `mutation_id` is `u64::MAX`
/// the prefix spills into the next graph, so it falls back to the graph range upper (which is
/// `Unbounded` at `GraphId::MAX`).
fn mutation_range_upper(
    graph_id: GraphId,
    mutation_id: MutationId,
) -> Bound<UniqueEffectPendingKey> {
    match mutation_id.checked_add(1) {
        Some(next) => Bound::Excluded(UniqueEffectPendingKey::new(graph_id, next, ShardId::new(0))),
        None => graph_range_upper(graph_id),
    }
}

/// Removes every pending-effect row for a graph (graph teardown). Mirrors the reservation purge.
pub(crate) fn purge_graph(graph_id: GraphId) {
    ROUTER_UNIQUE_EFFECT_PENDING.with_borrow_mut(|table| {
        let start = UniqueEffectPendingKey::new(graph_id, 0, ShardId::new(0));
        let keys: Vec<_> = table
            .range((Bound::Included(start), graph_range_upper(graph_id)))
            .map(|entry| *entry.key())
            .collect();
        for key in keys {
            table.remove(&key);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(seed: u32) -> GraphId {
        GraphId::from_raw(770_000 + seed)
    }

    fn ck() -> ClientMutationKey {
        ClientMutationKey::new(
            Principal::anonymous(),
            GraphId::from_raw(1),
            "ck".to_string(),
        )
    }

    #[test]
    fn register_lookup_remove_roundtrip() {
        let canister = Principal::management_canister();
        register(g(1), 10, ShardId::new(3), canister, ck());
        let record = lookup(g(1), 10, ShardId::new(3)).expect("row present");
        assert_eq!(record.canister, canister);
        assert_eq!(record.client_key, ck());
        assert_eq!(record.state, PendingEffectState::Active);
        assert_eq!(record.schema_version, PENDING_EFFECT_SCHEMA_V1);
        remove(g(1), 10, ShardId::new(3));
        assert_eq!(lookup(g(1), 10, ShardId::new(3)), None);
    }

    #[test]
    fn register_is_idempotent_on_replay() {
        let canister = Principal::anonymous();
        register(g(2), 20, ShardId::new(0), canister, ck());
        register(g(2), 20, ShardId::new(0), canister, ck());
        let (rows, _next, _scanned) = scan(None, 4096);
        assert_eq!(
            rows.iter()
                .filter(|r| r.key.graph_id == g(2) && r.key.mutation_id == 20)
                .count(),
            1
        );
    }

    #[test]
    #[should_panic(expected = "identity is immutable")]
    fn register_to_a_different_canister_traps() {
        register(g(20), 1, ShardId::new(0), Principal::anonymous(), ck());
        // Re-registering the same (graph, mutation, shard) to another canister would orphan the
        // first canister's still-pinned effect — fail-closed.
        register(
            g(20),
            1,
            ShardId::new(0),
            Principal::management_canister(),
            ck(),
        );
    }

    #[test]
    #[should_panic(expected = "owning-record identity is immutable")]
    fn register_to_a_different_client_key_traps() {
        let c = Principal::anonymous();
        register(g(25), 1, ShardId::new(0), c, ck());
        // Same (graph, mutation, shard) + canister but a different owning record would let recovery
        // resolve the wrong terminal proof — fail-closed.
        let other =
            ClientMutationKey::new(Principal::anonymous(), GraphId::from_raw(2), "x".into());
        register(g(25), 1, ShardId::new(0), c, other);
    }

    #[test]
    fn record_storable_roundtrip_preserves_fields() {
        let record = PendingEffectRecord {
            schema_version: PENDING_EFFECT_SCHEMA_V1,
            canister: Principal::management_canister(),
            client_key: ck(),
            state: PendingEffectState::Quarantined,
            next_retry_ns: 12_345,
            attempts: 7,
            diagnostic: Some("orphan acquire: no reservation".to_string()),
        };
        let decoded = PendingEffectRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
    }

    #[test]
    fn pending_effect_pin_tracks_row_presence() {
        let c = Principal::anonymous();
        assert!(!pending_effect_pinned(g(21), 99));
        register(g(21), 99, ShardId::new(0), c, ck());
        register(g(21), 99, ShardId::new(5), c, ck());
        assert!(pending_effect_pinned(g(21), 99));
        // A neighbouring mutation in the same graph is not pinned by this mutation's rows.
        assert!(!pending_effect_pinned(g(21), 100));
        remove(g(21), 99, ShardId::new(0));
        assert!(pending_effect_pinned(g(21), 99));
        remove(g(21), 99, ShardId::new(5));
        assert!(!pending_effect_pinned(g(21), 99));
    }

    #[test]
    fn quarantine_parks_row_with_backoff_and_diagnostic() {
        let c = Principal::anonymous();
        register(g(23), 1, ShardId::new(0), c, ck());
        quarantine(
            g(23),
            1,
            ShardId::new(0),
            5_000,
            "orphan acquire".to_string(),
        );
        let record = lookup(g(23), 1, ShardId::new(0)).expect("still present");
        assert_eq!(record.state, PendingEffectState::Quarantined);
        assert_eq!(record.next_retry_ns, 5_000);
        assert_eq!(record.attempts, 1);
        assert_eq!(record.diagnostic.as_deref(), Some("orphan acquire"));
        // The row and its canister identity are retained — never dropped.
        assert_eq!(record.canister, c);
    }

    #[test]
    fn quarantine_missing_row_is_noop() {
        quarantine(g(24), 1, ShardId::new(0), 5_000, "x".to_string());
        assert_eq!(lookup(g(24), 1, ShardId::new(0)), None);
    }

    #[test]
    fn pending_effect_pin_at_max_mutation_id() {
        let c = Principal::anonymous();
        register(g(22), u64::MAX, ShardId::new(0), c, ck());
        assert!(pending_effect_pinned(g(22), u64::MAX));
        remove(g(22), u64::MAX, ShardId::new(0));
        assert!(!pending_effect_pinned(g(22), u64::MAX));
    }

    #[test]
    fn key_storable_roundtrip_preserves_fields() {
        let key =
            UniqueEffectPendingKey::new(GraphId::from_raw(0x0A0B_0C0D), u64::MAX, ShardId::new(7));
        let decoded = UniqueEffectPendingKey::from_bytes(Cow::Owned(key.into_bytes()));
        assert_eq!(decoded, key);
    }

    #[test]
    fn scan_orders_by_graph_then_mutation_then_shard() {
        let gx = g(3);
        let c = Principal::anonymous();
        register(gx, 2, ShardId::new(1), c, ck());
        register(gx, 1, ShardId::new(9), c, ck());
        register(gx, 1, ShardId::new(2), c, ck());
        let (rows, _next, _scanned) = scan(None, 4096);
        let ours: Vec<_> = rows
            .iter()
            .filter(|r| r.key.graph_id == gx)
            .map(|r| (r.key.mutation_id, r.key.shard_id))
            .collect();
        assert_eq!(
            ours,
            vec![
                (1, ShardId::new(2)),
                (1, ShardId::new(9)),
                (2, ShardId::new(1)),
            ]
        );
    }

    #[test]
    fn purge_graph_removes_only_that_graph() {
        let c = Principal::anonymous();
        register(g(4), 1, ShardId::new(0), c, ck());
        register(g(4), 2, ShardId::new(0), c, ck());
        register(g(5), 1, ShardId::new(0), c, ck());
        purge_graph(g(4));
        let (rows, _n, _s) = scan(None, 4096);
        assert!(rows.iter().all(|r| r.key.graph_id != g(4)));
        assert!(rows.iter().any(|r| r.key.graph_id == g(5)));
    }

    #[test]
    fn purge_graph_at_max_graph_id_is_not_skipped() {
        let c = Principal::anonymous();
        register(GraphId::from_raw(u32::MAX), 1, ShardId::new(0), c, ck());
        purge_graph(GraphId::from_raw(u32::MAX));
        assert_eq!(
            lookup(GraphId::from_raw(u32::MAX), 1, ShardId::new(0)),
            None
        );
    }
}
