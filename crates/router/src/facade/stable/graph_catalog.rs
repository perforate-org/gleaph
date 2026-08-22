//! Logical graph name ↔ [`GraphId`] catalog (ADR 0011).

use candid::Principal;
use gleaph_graph_kernel::bidirectional_catalog::CatalogError;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{GraphShardKey, ShardId, ShardRegistryEntry};
use std::collections::BTreeSet;
use std::ops::Bound;

use crate::facade::stable::{
    ROUTER_GRAPH_CATALOG, ROUTER_GRAPH_RUNTIME_CONFIG, ROUTER_GRAPHS, ROUTER_SHARDS,
    ROUTER_SHARDS_BY_GRAPH_ID,
};
use crate::state::RouterError;

pub(crate) fn lookup_graph_id(name: &str) -> Option<GraphId> {
    ROUTER_GRAPH_CATALOG.with_borrow(|catalog| catalog.get_id(name))
}

pub(crate) fn graph_name(graph_id: GraphId) -> Option<String> {
    ROUTER_GRAPH_CATALOG.with_borrow(|catalog| catalog.get_name(graph_id))
}

pub(crate) fn intern_graph_name(name: &str) -> Result<GraphId, RouterError> {
    ROUTER_GRAPH_CATALOG
        .with_borrow_mut(|catalog| catalog.get_or_insert(name))
        .map_err(|e| catalog_error_to_router(e, "graph"))
}

#[allow(
    dead_code,
    reason = "catalog migration and admin paths pending ADR 0011 rollout"
)]
pub(crate) fn insert_graph_name(name: &str, graph_id: GraphId) -> Result<(), RouterError> {
    ROUTER_GRAPH_CATALOG
        .with_borrow_mut(|catalog| catalog.insert_with_id(name, graph_id))
        .map_err(|e| catalog_error_to_router(e, "graph"))
}

#[allow(
    dead_code,
    reason = "catalog migration and admin paths pending ADR 0011 rollout"
)]
pub(crate) fn graph_entry(
    graph_id: GraphId,
) -> Option<gleaph_gql_ic::graph_registry::GraphRegistryEntry> {
    ROUTER_GRAPHS.with_borrow(|graphs| graphs.get(&graph_id))
}

/// Resolves a logical graph name to a `GraphId` with a matching `ROUTER_GRAPHS` entry.
pub(crate) fn resolve_registered_graph_id(name: &str) -> Result<GraphId, RouterError> {
    let graph_id = lookup_graph_id(name).ok_or_else(|| RouterError::NotFound(name.to_owned()))?;
    if graph_entry(graph_id).is_none() {
        return Err(RouterError::NotFound(name.to_owned()));
    }
    Ok(graph_id)
}

pub(crate) fn require_graph_registry_entry(graph_id: GraphId) -> Result<(), RouterError> {
    if graph_entry(graph_id).is_none() {
        return Err(RouterError::NotFound(format!(
            "graph {graph_id:?} not registered"
        )));
    }
    Ok(())
}

pub(crate) fn catalog_error_to_router<Id: std::fmt::Display>(
    err: CatalogError<Id>,
    kind: &str,
) -> RouterError {
    match err {
        CatalogError::IdExhausted => RouterError::IdExhausted(kind.to_owned()),
        other => RouterError::Conflict(format!("{kind} catalog: {other}")),
    }
}

/// Fan-out listing via `ROUTER_SHARDS_BY_GRAPH_ID` (O(shards for graph)), not a full registry scan.
///
/// Validates index-local integrity only: duplicate ids, missing primary rows, and per-row
/// `graph_id` mismatches. Full bidirectional registry consistency is enforced on commit and by
/// `check_registry_invariants` in tests.
pub(crate) fn list_shards_for_graph_id(
    graph_id: GraphId,
) -> Result<Vec<ShardRegistryEntry>, RouterError> {
    let shard_ids = ROUTER_SHARDS_BY_GRAPH_ID.with_borrow(|index| {
        index
            .get(&graph_id)
            .map(|list| list.shard_ids.clone())
            .unwrap_or_default()
    });

    let mut indexed_unique = BTreeSet::new();
    for shard_id in &shard_ids {
        if !indexed_unique.insert(*shard_id) {
            return Err(RouterError::Internal(format!(
                "registry invariant violation: ROUTER_SHARDS_BY_GRAPH_ID[{graph_id:?}] duplicate shard {shard_id:?}"
            )));
        }
    }

    let mut out = Vec::with_capacity(indexed_unique.len());
    for shard_id in shard_ids {
        let entry = ROUTER_SHARDS
            .with_borrow(|shards| shards.get(&GraphShardKey::new(graph_id, shard_id)))
            .ok_or_else(|| {
                RouterError::Internal(format!(
                    "registry invariant violation: shard {shard_id:?} listed for graph {graph_id:?} but missing from ROUTER_SHARDS"
                ))
            })?;
        if entry.graph_id != graph_id {
            return Err(RouterError::Internal(format!(
                "registry invariant violation: shard {shard_id:?} has graph_id {:?}, expected {graph_id:?}",
                entry.graph_id
            )));
        }
        out.push(entry.entry);
    }
    Ok(out)
}

/// Index-attached shards only — used for dispatch, index fan-out, and backfill orchestration.
pub(crate) fn list_live_shards_for_graph_id(
    graph_id: GraphId,
) -> Result<Vec<ShardRegistryEntry>, RouterError> {
    Ok(list_shards_for_graph_id(graph_id)?
        .into_iter()
        .filter(|entry| entry.index_attached)
        .collect())
}

/// Returns the next graph-local [`ShardId`] that has never been issued.
pub(crate) fn next_graph_local_shard_id(graph_id: GraphId) -> Result<ShardId, RouterError> {
    let next = ROUTER_GRAPH_RUNTIME_CONFIG
        .with_borrow(|configs| configs.get(&graph_id))
        .ok_or_else(|| RouterError::NotFound(format!("runtime config for graph {graph_id:?}")))?
        .next_shard_id;
    let raw = u32::try_from(next)
        .map_err(|_| RouterError::IdExhausted(format!("shard for graph {graph_id:?}")))?;
    Ok(ShardId::new(raw))
}

pub(crate) fn lookup_shard_entry(
    graph_id: GraphId,
    shard_id: ShardId,
) -> Option<ShardRegistryEntry> {
    ROUTER_SHARDS.with_borrow(|shards| {
        shards
            .get(&GraphShardKey::new(graph_id, shard_id))
            .map(|state| state.entry)
    })
}

/// Stable catalog rows examined by one markerless frontier discovery step.
///
/// The catalog is ordered by [`GraphShardKey`], so a heap-only cursor can resume after the
/// selected row without making lane ownership durable. A page may contain only incomplete
/// registration claims; its last key is still returned so a later row is not hidden behind the
/// ineligible prefix.
pub(crate) const VECTOR_LANE_CATALOG_PAGE_BUDGET: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AttachedVectorLane {
    pub(crate) vector_target: Principal,
    pub(crate) shard_id: ShardId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AttachedVectorLanePage {
    pub(crate) lane: Option<AttachedVectorLane>,
    /// Cursor is exclusive on the next call. When a lane is found, it advances only to the
    /// selected row so other eligible rows in the same bounded page receive a later turn.
    pub(crate) next_cursor: Option<GraphShardKey>,
    pub(crate) scanned: u32,
}

/// Enumerate at most one exact, fully attached `(Vector target, shard)` lane from the stable
/// Router shard catalog.
///
/// `ROUTER_SHARDS` is the canonical source for both attachment bits and target identity. Rows
/// whose index attach is incomplete, whose vector attach is incomplete, or whose target is
/// anonymous are ignored. Duplicate exact lane pairs in the bounded page are an invariant error;
/// callers must fail closed instead of coalescing two catalog owners into one outbox lane.
pub(crate) fn scan_attached_vector_lane(
    start_after: Option<GraphShardKey>,
) -> Result<AttachedVectorLanePage, RouterError> {
    let lower = start_after.map_or(Bound::Unbounded, Bound::Excluded);
    ROUTER_SHARDS.with_borrow(|shards| {
        let mut selected = None;
        let mut last_scanned = None;
        let mut scanned = 0u32;
        let mut exact_pairs = BTreeSet::new();

        for lazy in shards
            .range((lower, Bound::Unbounded))
            .take(VECTOR_LANE_CATALOG_PAGE_BUDGET)
        {
            let key = *lazy.key();
            let entry = lazy.value();
            scanned = scanned.saturating_add(1);
            last_scanned = Some(key);
            if !entry.index_attached || !entry.vector_index_attached {
                continue;
            }
            let Some(vector_target) = entry.vector_canister else {
                continue;
            };
            if vector_target == Principal::anonymous() {
                continue;
            }

            let pair = (vector_target, entry.shard_id);
            if !exact_pairs.insert(pair) {
                return Err(RouterError::Internal(format!(
                    "registry invariant violation: duplicate attached vector lane target {vector_target} shard {:?}",
                    entry.shard_id
                )));
            }
            if selected.is_none() {
                selected = Some((AttachedVectorLane { vector_target, shard_id: entry.shard_id }, key));
            }
        }

        // Advance only to the selected row when one exists. This preserves canonical order and
        // gives later eligible rows in the same page their own one-lane recovery turn. If the page
        // contains no eligible row, advance to its last key when the page is full. A short/empty
        // page reports the catalog end to the caller, which owns lap rotation.
        let next_cursor = selected
            .map(|(_, key)| key)
            .or_else(|| {
                (scanned as usize == VECTOR_LANE_CATALOG_PAGE_BUDGET)
                    .then_some(last_scanned)
                    .flatten()
            });
        Ok(AttachedVectorLanePage {
            lane: selected.map(|(lane, _)| lane),
            next_cursor,
            scanned,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::vector_ingest_outbox::test_lock;

    fn shard_entry(
        graph_id: u32,
        shard_id: u32,
        vector_target: Principal,
        index_attached: bool,
        vector_index_attached: bool,
    ) -> (GraphShardKey, ShardRegistryEntry) {
        let key = GraphShardKey::new(GraphId::from_raw(graph_id), ShardId::new(shard_id));
        (
            key,
            ShardRegistryEntry {
                shard_id: key.shard_id,
                graph_canister: Principal::from_slice(&[graph_id as u8; 29]),
                index_canister: Principal::management_canister(),
                graph_id: key.graph_id,
                registered_at_ns: 0,
                index_attached,
                vector_canister: Some(vector_target),
                vector_index_attached,
            },
        )
    }

    #[test]
    fn attached_vector_lanes_enumerate_exact_ready_target_shard_pairs() {
        let _guard = test_lock();
        ROUTER_SHARDS.with_borrow_mut(|shards| shards.clear_new());
        let target_a = Principal::from_slice(&[7; 29]);
        let target_b = Principal::from_slice(&[8; 29]);
        for (key, entry) in [
            shard_entry(1, 0, target_a, false, true),
            shard_entry(2, 0, target_a, true, false),
            shard_entry(3, 0, Principal::anonymous(), true, true),
            shard_entry(4, 0, target_a, true, true),
            shard_entry(5, 0, target_b, true, true),
        ] {
            ROUTER_SHARDS.with_borrow_mut(|shards| {
                shards.insert(key, entry.into());
            });
        }

        let first = scan_attached_vector_lane(None).expect("first catalog page");
        assert_eq!(
            first.lane,
            Some(AttachedVectorLane {
                vector_target: target_a,
                shard_id: ShardId::new(0),
            })
        );
        assert_eq!(
            first.next_cursor,
            Some(GraphShardKey::new(GraphId::from_raw(4), ShardId::new(0)))
        );

        let second = scan_attached_vector_lane(first.next_cursor).expect("second catalog page");
        assert_eq!(
            second.lane,
            Some(AttachedVectorLane {
                vector_target: target_b,
                shard_id: ShardId::new(0),
            })
        );
        assert_eq!(
            second.next_cursor,
            Some(GraphShardKey::new(GraphId::from_raw(5), ShardId::new(0)))
        );

        let end = scan_attached_vector_lane(second.next_cursor).expect("catalog end page");
        assert_eq!(end.lane, None);
        assert_eq!(end.next_cursor, None);
        assert_eq!(end.scanned, 0);

        let wrapped = scan_attached_vector_lane(None).expect("catalog restart page");
        assert_eq!(
            wrapped.lane,
            Some(AttachedVectorLane {
                vector_target: target_a,
                shard_id: ShardId::new(0),
            })
        );
        assert_eq!(
            wrapped.next_cursor,
            Some(GraphShardKey::new(GraphId::from_raw(4), ShardId::new(0)))
        );

        // A page made entirely of incomplete rows still advances to a later ready row.
        ROUTER_SHARDS.with_borrow_mut(|shards| shards.clear_new());
        for graph_id in 1..=u32::try_from(VECTOR_LANE_CATALOG_PAGE_BUDGET).unwrap() {
            let (key, entry) = shard_entry(graph_id, 0, target_a, false, false);
            ROUTER_SHARDS.with_borrow_mut(|shards| {
                shards.insert(key, entry.into());
            });
        }
        let (ready_key, ready_entry) = shard_entry(100, 3, target_a, true, true);
        ROUTER_SHARDS.with_borrow_mut(|shards| {
            shards.insert(ready_key, ready_entry.into());
        });
        let empty_page = scan_attached_vector_lane(None).expect("incomplete catalog page");
        assert_eq!(empty_page.lane, None);
        assert_eq!(
            empty_page.next_cursor,
            Some(GraphShardKey::new(
                GraphId::from_raw(VECTOR_LANE_CATALOG_PAGE_BUDGET as u32),
                ShardId::new(0),
            ))
        );
        let later = scan_attached_vector_lane(empty_page.next_cursor).expect("later ready row");
        assert_eq!(
            later.lane,
            Some(AttachedVectorLane {
                vector_target: target_a,
                shard_id: ShardId::new(3)
            })
        );
        ROUTER_SHARDS.with_borrow_mut(|shards| shards.clear_new());
    }

    #[test]
    fn attached_vector_lanes_fail_closed_on_duplicate_exact_pair() {
        let _guard = test_lock();
        ROUTER_SHARDS.with_borrow_mut(|shards| shards.clear_new());
        let target = Principal::from_slice(&[7; 29]);
        for graph_id in [1, 2] {
            let (key, entry) = shard_entry(graph_id, 4, target, true, true);
            ROUTER_SHARDS.with_borrow_mut(|shards| {
                shards.insert(key, entry.into());
            });
        }
        let error = scan_attached_vector_lane(None).expect_err("duplicate exact lane");
        assert!(
            matches!(error, RouterError::Internal(ref message) if message.contains("duplicate attached vector lane")),
            "unexpected duplicate error: {error:?}"
        );
        ROUTER_SHARDS.with_borrow_mut(|shards| shards.clear_new());
    }
}
