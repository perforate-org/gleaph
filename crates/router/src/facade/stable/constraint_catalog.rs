//! Row-oriented unique-constraint catalog in stable memory (ADR 0030).
//!
//! A uniqueness constraint is a *logical* definition, distinct from an index
//! ([`super::indexed_catalog`]). The router is the sole SSOT for constraint
//! definitions; the interned [`ConstraintNameId`] is the stable handle that the
//! cross-shard reservation key references.
//!
//! - `ROUTER_UNIQUE_CONSTRAINTS`: `(graph_id, constraint_name_id) → ConstraintDefRecord`

use std::borrow::Cow;
use std::ops::Bound;

use candid::{CandidType, Decode, Encode};
use gleaph_graph_kernel::entry::{ConstraintNameId, GraphId, PropertyId, VertexLabelId};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};

use crate::facade::stable::ROUTER_UNIQUE_CONSTRAINTS;
use crate::state::RouterError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UniqueConstraintKey {
    pub graph_id: GraphId,
    pub constraint_name_id: ConstraintNameId,
}

impl UniqueConstraintKey {
    pub const fn new(graph_id: GraphId, constraint_name_id: ConstraintNameId) -> Self {
        Self {
            graph_id,
            constraint_name_id,
        }
    }
}

/// Lifecycle of a unique constraint (ADR 0030 slice 9 DROP lifecycle). `Active` is the only state
/// that enforces new acquires; `Dropping` is a tombstone — the constraint is inactive for new DML
/// (new INSERTs proceed unconstrained) while recovery drains its reservations and pending effects.
/// There is no persisted `Removed` value: an absent record (after the completion gate proved every
/// reservation and pending effect for the `constraint_id` is gone) **is** the `Removed` state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) enum ConstraintLifecycle {
    Active,
    Dropping,
}

/// First cut (ADR 0030): vertex single-property uniqueness only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct ConstraintDefRecord {
    pub vertex_label_id: VertexLabelId,
    pub property_id: PropertyId,
    /// `Active` until `DROP CONSTRAINT` flips it to `Dropping` (ADR 0030 slice 9).
    pub state: ConstraintLifecycle,
    /// When the `DROP` was initiated, for diagnostics/backoff. `None` while `Active`.
    pub dropping_at_ns: Option<u64>,
    /// Drop-drain scan-invalidation token (ADR 0030 slice 9): bumped whenever a new pending-effect
    /// row is registered for this graph while the constraint is `Dropping`, so the drop-drain driver
    /// can detect a row registered behind its cursor mid-lap and re-lap. This is **not** part of the
    /// reservation key and is unrelated to `reclaim_generation`.
    pub drop_scan_generation: u64,
}

impl ConstraintDefRecord {
    /// A freshly declared, `Active` constraint with no drop state.
    pub(crate) fn new_active(vertex_label_id: VertexLabelId, property_id: PropertyId) -> Self {
        Self {
            vertex_label_id,
            property_id,
            state: ConstraintLifecycle::Active,
            dropping_at_ns: None,
            drop_scan_generation: 0,
        }
    }
}

/// Versioned stable envelope (ADR 0007), so the constraint record schema can evolve across upgrades.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum ConstraintDefStableRecord {
    V1(ConstraintDefRecord),
}

/// Outcome of a [`begin_drop`] transition (ADR 0030 slice 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropInitiation {
    /// `Active → Dropping` happened now.
    Transitioned,
    /// The constraint was already `Dropping` (idempotent DROP replay).
    AlreadyDropping,
    /// The constraint record was absent and `IF EXISTS` was set (no-op).
    AbsentNoop,
}

/// One `Dropping` constraint discovered by [`scan_dropping_constraints`] for the drop-drain driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DroppingConstraint {
    pub graph_id: GraphId,
    pub constraint_name_id: ConstraintNameId,
    pub vertex_label_id: VertexLabelId,
    pub property_id: PropertyId,
    pub drop_scan_generation: u64,
}

impl Storable for UniqueConstraintKey {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: 6,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.constraint_name_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut name = [0; 2];
        graph.copy_from_slice(&bytes[0..4]);
        name.copy_from_slice(&bytes[4..6]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            constraint_name_id: ConstraintNameId::from_le_bytes(name),
        }
    }
}

impl Storable for ConstraintDefRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(&ConstraintDefStableRecord::V1(*self)).expect("encode constraint def"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&ConstraintDefStableRecord::V1(self)).expect("encode constraint def")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), ConstraintDefStableRecord).expect("decode constraint def") {
            ConstraintDefStableRecord::V1(v1) => v1,
        }
    }
}

/// Exclusive upper bound of one graph's key range. `graph_id` is the most-significant key
/// component, so the half-open range `[(graph_id, 0), (graph_id + 1, 0))` covers exactly that
/// graph. At `GraphId::MAX` there is no `graph_id + 1`; the bound must be `Unbounded` (every
/// remaining key belongs to the max graph) — a saturating `+1` would collapse to `(MAX, 0)` and
/// yield an empty range, silently dropping the max graph's constraints.
fn graph_range_upper(graph_id: GraphId) -> Bound<UniqueConstraintKey> {
    match graph_id.raw().checked_add(1) {
        Some(next) => Bound::Excluded(UniqueConstraintKey::new(
            GraphId::from_raw(next),
            ConstraintNameId::from_raw(0),
        )),
        None => Bound::Unbounded,
    }
}

/// Registers a new unique constraint definition. The caller guarantees, via the
/// declare-on-empty preflight (ADR 0030), that the target label is brand-new.
pub(crate) fn create_unique_constraint(
    graph_id: GraphId,
    constraint_name_id: ConstraintNameId,
    record: ConstraintDefRecord,
    if_not_exists: bool,
) -> Result<bool, RouterError> {
    let key = UniqueConstraintKey::new(graph_id, constraint_name_id);
    let exists = ROUTER_UNIQUE_CONSTRAINTS.with_borrow(|map| map.contains_key(&key));
    if exists {
        if if_not_exists {
            return Ok(false);
        }
        return Err(RouterError::Conflict(format!(
            "constraint already exists: {constraint_name_id}"
        )));
    }
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow_mut(|map| {
        map.insert(key, record);
    });
    Ok(true)
}

/// The constraint record for `(graph_id, constraint_name_id)` in **any** lifecycle state (ADR 0030
/// slice 9). Used by the same-name re-CREATE guard and DROP initiation idempotency: both must see a
/// `Dropping` tombstone, not just `Active` records.
pub(crate) fn find_unique_constraint_any_lifecycle(
    graph_id: GraphId,
    constraint_name_id: ConstraintNameId,
) -> Option<ConstraintDefRecord> {
    let key = UniqueConstraintKey::new(graph_id, constraint_name_id);
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow(|map| map.get(&key))
}

/// Flip a constraint `Active → Dropping` (ADR 0030 slice 9): synchronous, no `await`. Idempotent on
/// a constraint already `Dropping`. With `if_exists`, an absent record is a no-op; otherwise it is a
/// `NotFound` error. The `drop_scan_generation` is left untouched (the drop-drain driver snapshots
/// it across laps).
pub(crate) fn begin_drop(
    graph_id: GraphId,
    constraint_name_id: ConstraintNameId,
    if_exists: bool,
    now_ns: u64,
) -> Result<DropInitiation, RouterError> {
    let key = UniqueConstraintKey::new(graph_id, constraint_name_id);
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow_mut(|map| {
        let Some(mut record) = map.get(&key) else {
            if if_exists {
                return Ok(DropInitiation::AbsentNoop);
            }
            return Err(RouterError::NotFound(constraint_name_id.to_string()));
        };
        match record.state {
            ConstraintLifecycle::Dropping => Ok(DropInitiation::AlreadyDropping),
            ConstraintLifecycle::Active => {
                record.state = ConstraintLifecycle::Dropping;
                record.dropping_at_ns = Some(now_ns);
                map.insert(key, record);
                Ok(DropInitiation::Transitioned)
            }
        }
    })
}

/// Bump `drop_scan_generation` on **every** `Dropping` constraint in `graph_id` (ADR 0030 slice 9).
/// Called whenever a new pending-effect row is registered for the graph, so the drop-drain driver
/// re-laps if a row appeared behind its cursor. A no-op when no constraint is `Dropping`. The bump
/// is **conservative**: it may also fire on idempotent re-registration / deterministic replay; this
/// only invalidates the current clean lap (costing one extra lap) and can never make cleanup unsafe.
pub(crate) fn bump_drop_scan_generation(graph_id: GraphId) {
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow_mut(|map| {
        let start = UniqueConstraintKey::new(graph_id, ConstraintNameId::from_raw(0));
        let dropping: Vec<UniqueConstraintKey> = map
            .range((Bound::Included(start), graph_range_upper(graph_id)))
            .filter(|entry| entry.value().state == ConstraintLifecycle::Dropping)
            .map(|entry| *entry.key())
            .collect();
        for key in dropping {
            if let Some(mut record) = map.get(&key) {
                record.drop_scan_generation = record.drop_scan_generation.saturating_add(1);
                map.insert(key, record);
            }
        }
    });
}

/// Terminal deletion of a `Dropping` constraint record (the `Removed` state, ADR 0030 slice 9).
/// **Recovery-only**: the drop-drain driver calls this **only** after the full completion gate holds
/// (no reservations and no pending unique effects for the `constraint_id`). Removes only a record
/// still in `Dropping`; returns whether a record was removed.
pub(crate) fn remove_dropped_constraint_record(
    graph_id: GraphId,
    constraint_name_id: ConstraintNameId,
) -> bool {
    let key = UniqueConstraintKey::new(graph_id, constraint_name_id);
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow_mut(|map| match map.get(&key) {
        Some(record) if record.state == ConstraintLifecycle::Dropping => {
            map.remove(&key);
            true
        }
        _ => false,
    })
}

/// Bounded, cursor-based discovery of `Dropping` constraints for the drop-drain driver (ADR 0030
/// slice 9). Scans up to `budget` records after `start_after` across the whole keyspace, returning
/// those in `Dropping`, the last key examined (next cursor), and the count scanned. Read-only.
pub(crate) fn scan_dropping_constraints(
    start_after: Option<&UniqueConstraintKey>,
    budget: usize,
) -> (Vec<DroppingConstraint>, Option<UniqueConstraintKey>, u32) {
    let mut scanned: u32 = 0;
    let mut last_key: Option<UniqueConstraintKey> = None;
    let mut out: Vec<DroppingConstraint> = Vec::new();
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow(|map| {
        let lower = match start_after {
            Some(key) => Bound::Excluded(*key),
            None => Bound::Unbounded,
        };
        for entry in map.range((lower, Bound::Unbounded)).take(budget) {
            let key = *entry.key();
            let def = entry.value();
            scanned += 1;
            if def.state == ConstraintLifecycle::Dropping {
                out.push(DroppingConstraint {
                    graph_id: key.graph_id,
                    constraint_name_id: key.constraint_name_id,
                    vertex_label_id: def.vertex_label_id,
                    property_id: def.property_id,
                    drop_scan_generation: def.drop_scan_generation,
                });
            }
            last_key = Some(key);
        }
    });
    (out, last_key, scanned)
}

/// Returns the **Active** constraint guarding `(vertex_label_id, property_id)`, if any (ADR 0030
/// slice 9). The acquire path uses this: a `Dropping` constraint reads as absent, so a new INSERT
/// makes no claim and proceeds unconstrained.
pub(crate) fn find_active_unique_constraint(
    graph_id: GraphId,
    vertex_label_id: VertexLabelId,
    property_id: PropertyId,
) -> Option<(ConstraintNameId, ConstraintDefRecord)> {
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow(|map| {
        let start = UniqueConstraintKey::new(graph_id, ConstraintNameId::from_raw(0));
        map.range((Bound::Included(start), graph_range_upper(graph_id)))
            .find_map(|entry| {
                let def = entry.value();
                (def.state == ConstraintLifecycle::Active
                    && def.vertex_label_id == vertex_label_id
                    && def.property_id == property_id)
                    .then_some((entry.key().constraint_name_id, def))
            })
    })
}

/// All constrained `(vertex_label_id, property_id, constraint_name_id)` triples for a graph in **any**
/// lifecycle (`Active` + `Dropping`), in catalog-key order (ADR 0030 slice 5b / slice 9). The
/// release/drain path uses this: a `Dropping` constraint still captures `Release` effects so old
/// ownership is reconciled while the constraint drains.
pub(crate) fn constrained_properties_for_graph(
    graph_id: GraphId,
) -> Vec<(VertexLabelId, PropertyId, ConstraintNameId)> {
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow(|map| {
        let start = UniqueConstraintKey::new(graph_id, ConstraintNameId::from_raw(0));
        map.range((Bound::Included(start), graph_range_upper(graph_id)))
            .map(|entry| {
                let def = entry.value();
                (
                    def.vertex_label_id,
                    def.property_id,
                    entry.key().constraint_name_id,
                )
            })
            .collect()
    })
}

/// The **Active**-only constrained `(vertex_label_id, property_id)` set for a graph (ADR 0030 slice
/// 9). The acquire-side SET guard uses this: a `Dropping` constraint must not refuse a new
/// constrained write — new DML proceeds unconstrained while the constraint drains.
pub(crate) fn active_constrained_properties_for_graph(
    graph_id: GraphId,
) -> Vec<(VertexLabelId, PropertyId, ConstraintNameId)> {
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow(|map| {
        let start = UniqueConstraintKey::new(graph_id, ConstraintNameId::from_raw(0));
        map.range((Bound::Included(start), graph_range_upper(graph_id)))
            .filter(|entry| entry.value().state == ConstraintLifecycle::Active)
            .map(|entry| {
                let def = entry.value();
                (
                    def.vertex_label_id,
                    def.property_id,
                    entry.key().constraint_name_id,
                )
            })
            .collect()
    })
}

pub(crate) fn purge_graph_constraints(graph_id: GraphId) {
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow_mut(|map| {
        let start = UniqueConstraintKey::new(graph_id, ConstraintNameId::from_raw(0));
        let keys: Vec<_> = map
            .range((Bound::Included(start), graph_range_upper(graph_id)))
            .map(|entry| *entry.key())
            .collect();
        for key in keys {
            map.remove(&key);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_constraint_key_storable_roundtrip() {
        let key = UniqueConstraintKey::new(GraphId::from_raw(5), ConstraintNameId::from_raw(9));
        let decoded = UniqueConstraintKey::from_bytes(Cow::Owned(key.into_bytes()));
        assert_eq!(decoded, key);
    }

    #[test]
    fn constraint_def_record_storable_roundtrip() {
        let record = ConstraintDefRecord {
            vertex_label_id: VertexLabelId::from_raw(3),
            property_id: PropertyId::from_raw(42),
            state: ConstraintLifecycle::Dropping,
            dropping_at_ns: Some(99),
            drop_scan_generation: 7,
        };
        let decoded = ConstraintDefRecord::from_bytes(Cow::Owned(record.into_bytes()));
        assert_eq!(decoded, record);
    }

    #[test]
    fn create_then_find_then_drop() {
        let graph = GraphId::from_raw(900_001);
        let name = ConstraintNameId::from_raw(1);
        let label = VertexLabelId::from_raw(7);
        let property = PropertyId::from_raw(11);
        let record = ConstraintDefRecord::new_active(label, property);
        assert!(create_unique_constraint(graph, name, record, false).expect("create"));
        assert_eq!(
            find_active_unique_constraint(graph, label, property),
            Some((name, record))
        );
        // Duplicate without IF NOT EXISTS conflicts.
        assert!(create_unique_constraint(graph, name, record, false).is_err());
        // Idempotent with IF NOT EXISTS.
        assert!(!create_unique_constraint(graph, name, record, true).expect("idempotent"));

        // DROP flips to Dropping; the active lookup no longer sees it, but the record survives.
        assert_eq!(
            begin_drop(graph, name, false, 1_234).expect("drop"),
            DropInitiation::Transitioned
        );
        assert_eq!(find_active_unique_constraint(graph, label, property), None);
        assert!(matches!(
            find_unique_constraint_any_lifecycle(graph, name),
            Some(rec) if rec.state == ConstraintLifecycle::Dropping
        ));
        // Recovery deletes the record (Removed); the name is then absent.
        assert!(remove_dropped_constraint_record(graph, name));
        assert_eq!(find_unique_constraint_any_lifecycle(graph, name), None);
    }

    #[test]
    fn drop_is_idempotent_while_dropping() {
        let graph = GraphId::from_raw(900_010);
        let name = ConstraintNameId::from_raw(1);
        let record =
            ConstraintDefRecord::new_active(VertexLabelId::from_raw(1), PropertyId::from_raw(1));
        assert!(create_unique_constraint(graph, name, record, false).expect("create"));
        assert_eq!(
            begin_drop(graph, name, false, 1).expect("first drop"),
            DropInitiation::Transitioned
        );
        assert_eq!(
            begin_drop(graph, name, false, 2).expect("idempotent drop"),
            DropInitiation::AlreadyDropping
        );
        assert_eq!(
            begin_drop(graph, name, true, 3).expect("idempotent if-exists drop"),
            DropInitiation::AlreadyDropping
        );
    }

    #[test]
    fn drop_absent_constraint_respects_if_exists() {
        let graph = GraphId::from_raw(900_011);
        let name = ConstraintNameId::from_raw(9);
        assert!(begin_drop(graph, name, false, 1).is_err());
        assert_eq!(
            begin_drop(graph, name, true, 1).expect("if exists noop"),
            DropInitiation::AbsentNoop
        );
    }

    #[test]
    fn bump_drop_scan_generation_only_touches_dropping_records() {
        let graph = GraphId::from_raw(900_012);
        let active = ConstraintNameId::from_raw(1);
        let dropping = ConstraintNameId::from_raw(2);
        create_unique_constraint(
            graph,
            active,
            ConstraintDefRecord::new_active(VertexLabelId::from_raw(1), PropertyId::from_raw(1)),
            false,
        )
        .expect("create active");
        create_unique_constraint(
            graph,
            dropping,
            ConstraintDefRecord::new_active(VertexLabelId::from_raw(2), PropertyId::from_raw(2)),
            false,
        )
        .expect("create dropping");
        begin_drop(graph, dropping, false, 1).expect("drop");

        bump_drop_scan_generation(graph);
        bump_drop_scan_generation(graph);

        assert_eq!(
            find_unique_constraint_any_lifecycle(graph, active)
                .unwrap()
                .drop_scan_generation,
            0,
            "active records are untouched"
        );
        assert_eq!(
            find_unique_constraint_any_lifecycle(graph, dropping)
                .unwrap()
                .drop_scan_generation,
            2,
            "each bump increments the dropping record's generation"
        );
    }

    #[test]
    fn scan_dropping_constraints_returns_only_dropping() {
        let graph = GraphId::from_raw(900_013);
        let active = ConstraintNameId::from_raw(1);
        let dropping = ConstraintNameId::from_raw(2);
        create_unique_constraint(
            graph,
            active,
            ConstraintDefRecord::new_active(VertexLabelId::from_raw(1), PropertyId::from_raw(1)),
            false,
        )
        .expect("active");
        create_unique_constraint(
            graph,
            dropping,
            ConstraintDefRecord::new_active(VertexLabelId::from_raw(2), PropertyId::from_raw(2)),
            false,
        )
        .expect("dropping");
        begin_drop(graph, dropping, false, 1).expect("drop");

        let (rows, _cursor, scanned) = scan_dropping_constraints(None, 4096);
        assert!(scanned >= 2);
        let mine: Vec<_> = rows.into_iter().filter(|r| r.graph_id == graph).collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].constraint_name_id, dropping);
    }

    #[test]
    fn remove_dropped_record_only_removes_dropping() {
        let graph = GraphId::from_raw(900_014);
        let name = ConstraintNameId::from_raw(1);
        create_unique_constraint(
            graph,
            name,
            ConstraintDefRecord::new_active(VertexLabelId::from_raw(1), PropertyId::from_raw(1)),
            false,
        )
        .expect("create");
        // An Active record is never removed by the recovery-only terminal delete.
        assert!(!remove_dropped_constraint_record(graph, name));
        assert!(find_unique_constraint_any_lifecycle(graph, name).is_some());
        begin_drop(graph, name, false, 1).expect("drop");
        assert!(remove_dropped_constraint_record(graph, name));
        assert!(find_unique_constraint_any_lifecycle(graph, name).is_none());
    }

    #[test]
    fn find_and_purge_cover_the_max_graph_id() {
        // Regression: a saturating `graph_id + 1` upper bound collapses to an empty range at
        // GraphId::MAX, so the constraint would be missed (and after DDL ships, duplicates would be
        // admitted for the max graph). The `Unbounded` upper bound covers it.
        let graph = GraphId::from_raw(u32::MAX);
        let name = ConstraintNameId::from_raw(1);
        let label = VertexLabelId::from_raw(7);
        let property = PropertyId::from_raw(11);
        let record = ConstraintDefRecord::new_active(label, property);
        assert!(create_unique_constraint(graph, name, record, false).expect("create"));
        assert_eq!(
            find_active_unique_constraint(graph, label, property),
            Some((name, record)),
            "constraint on the max graph id must be found, not silently skipped"
        );

        purge_graph_constraints(graph);
        assert_eq!(find_active_unique_constraint(graph, label, property), None);
    }
}
