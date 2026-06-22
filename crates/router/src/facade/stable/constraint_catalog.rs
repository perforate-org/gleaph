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

use gleaph_graph_kernel::entry::{ConstraintNameId, GraphId, PropertyId, VertexLabelId};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};

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

/// First cut (ADR 0030): vertex single-property uniqueness only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConstraintDefRecord {
    pub vertex_label_id: VertexLabelId,
    pub property_id: PropertyId,
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
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: 6,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.extend_from_slice(&self.vertex_label_id.to_le_bytes());
        out.extend_from_slice(&self.property_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        Self {
            vertex_label_id: VertexLabelId::from_le_bytes(
                bytes[0..2].try_into().expect("vertex_label_id"),
            ),
            property_id: PropertyId::from_le_bytes(bytes[2..6].try_into().expect("property_id")),
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

pub(crate) fn constraint_record_exists(
    graph_id: GraphId,
    constraint_name_id: ConstraintNameId,
) -> bool {
    let key = UniqueConstraintKey::new(graph_id, constraint_name_id);
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow(|map| map.contains_key(&key))
}

pub(crate) fn drop_unique_constraint(
    graph_id: GraphId,
    constraint_name_id: ConstraintNameId,
    if_exists: bool,
) -> Result<Option<ConstraintDefRecord>, RouterError> {
    let key = UniqueConstraintKey::new(graph_id, constraint_name_id);
    let removed = ROUTER_UNIQUE_CONSTRAINTS.with_borrow_mut(|map| map.remove(&key));
    if removed.is_none() && !if_exists {
        return Err(RouterError::NotFound(constraint_name_id.to_string()));
    }
    Ok(removed)
}

/// Returns the constraint guarding `(vertex_label_id, property_id)`, if any.
/// Used by the write-path uniqueness gate (ADR 0030 slice 5).
pub(crate) fn find_unique_constraint(
    graph_id: GraphId,
    vertex_label_id: VertexLabelId,
    property_id: PropertyId,
) -> Option<(ConstraintNameId, ConstraintDefRecord)> {
    ROUTER_UNIQUE_CONSTRAINTS.with_borrow(|map| {
        let start = UniqueConstraintKey::new(graph_id, ConstraintNameId::from_raw(0));
        map.range((Bound::Included(start), graph_range_upper(graph_id)))
            .find_map(|entry| {
                let def = entry.value();
                (def.vertex_label_id == vertex_label_id && def.property_id == property_id)
                    .then_some((entry.key().constraint_name_id, def))
            })
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
        let record = ConstraintDefRecord {
            vertex_label_id: label,
            property_id: property,
        };
        assert!(create_unique_constraint(graph, name, record, false).expect("create"));
        assert_eq!(
            find_unique_constraint(graph, label, property),
            Some((name, record))
        );
        // Duplicate without IF NOT EXISTS conflicts.
        assert!(create_unique_constraint(graph, name, record, false).is_err());
        // Idempotent with IF NOT EXISTS.
        assert!(!create_unique_constraint(graph, name, record, true).expect("idempotent"));

        let dropped = drop_unique_constraint(graph, name, false).expect("drop");
        assert_eq!(dropped, Some(record));
        assert_eq!(find_unique_constraint(graph, label, property), None);
        // Drop missing without IF EXISTS errors.
        assert!(drop_unique_constraint(graph, name, false).is_err());
        assert!(
            drop_unique_constraint(graph, name, true)
                .expect("if exists")
                .is_none()
        );
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
        let record = ConstraintDefRecord {
            vertex_label_id: label,
            property_id: property,
        };
        assert!(create_unique_constraint(graph, name, record, false).expect("create"));
        assert_eq!(
            find_unique_constraint(graph, label, property),
            Some((name, record)),
            "constraint on the max graph id must be found, not silently skipped"
        );

        purge_graph_constraints(graph);
        assert_eq!(find_unique_constraint(graph, label, property), None);
    }
}
