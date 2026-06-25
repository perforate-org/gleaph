//! Router-owned vector maintenance policy catalog in stable memory (ADR 0031 Slice 10).
//!
//! The Router is the SSOT for maintenance *policy* (thresholds + per-step budgets); the vector
//! canister owns the maintenance *execution* state. A policy is keyed by `(graph_id, index_id)` and
//! is **absent / disabled by default**, so the push scheduler does nothing until an operator
//! explicitly enables one.
//!
//! - `ROUTER_VECTOR_MAINTENANCE_POLICIES`: `(graph_id, index_id) → VectorMaintenancePolicyRecord`

use std::borrow::Cow;
use std::ops::Bound;

use candid::{CandidType, Decode, Encode};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::vector_index::VectorMaintenancePolicy;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};

use crate::facade::stable::ROUTER_VECTOR_MAINTENANCE_POLICIES;
use crate::facade::stable::vector_index_catalog::{VectorIndexKey, get_vector_index};
use crate::state::RouterError;

/// A durable maintenance policy for one vector index (ADR 0031 Slice 10). The Router snapshots this
/// into a `VectorMaintenanceStepRequest` when forwarding a bounded maintenance step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct VectorMaintenancePolicyRecord {
    pub graph_id: GraphId,
    pub index_id: u32,
    /// When `false` (default), the push scheduler is a no-op for this index.
    pub enabled: bool,
    /// Threshold policy evaluated when a page-health scan exhausts.
    pub policy: VectorMaintenancePolicy,
    /// Rebuild `nlist`; `None` defaults to the current `def.nlist` (degenerate `nlist=1` requires an
    /// explicit value at trigger time).
    pub target_nlist: Option<u32>,
    /// Rebuild sampling limit forwarded to the rebuild start.
    pub sample_limit: u32,
    /// Max page-meta entries scanned per bounded scan step.
    pub scan_max_pages: u32,
    /// Max subjects processed per bounded rebuild step.
    pub rebuild_max_subjects: u32,
    /// Max work units processed per bounded cleanup/abort step.
    pub cleanup_max_work: u32,
}

/// Versioned stable envelope (ADR 0007) so the record schema can evolve across upgrades.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum VectorMaintenancePolicyStableRecord {
    V1(VectorMaintenancePolicyRecord),
}

impl Storable for VectorMaintenancePolicyRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&VectorMaintenancePolicyStableRecord::V1(*self))
                .expect("encode vector maintenance policy"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&VectorMaintenancePolicyStableRecord::V1(self))
            .expect("encode vector maintenance policy")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), VectorMaintenancePolicyStableRecord)
            .expect("decode vector maintenance policy")
        {
            VectorMaintenancePolicyStableRecord::V1(v1) => v1,
        }
    }
}

/// Validates a policy without mutating state: `recommended_*_bps <= required_*_bps`, nonzero budgets,
/// and the vector-index definition must exist.
fn validate(record: &VectorMaintenancePolicyRecord) -> Result<(), RouterError> {
    if record.policy.recommended_tombstone_ratio_bps > record.policy.required_tombstone_ratio_bps
        || record.policy.recommended_skew_ratio_bps > record.policy.required_skew_ratio_bps
    {
        return Err(RouterError::InvalidArgument(
            "recommended_*_bps must not exceed required_*_bps".to_owned(),
        ));
    }
    if record.sample_limit == 0
        || record.scan_max_pages == 0
        || record.rebuild_max_subjects == 0
        || record.cleanup_max_work == 0
    {
        return Err(RouterError::InvalidArgument(
            "maintenance per-step budgets must be nonzero".to_owned(),
        ));
    }
    if get_vector_index(record.graph_id, record.index_id).is_none() {
        return Err(RouterError::NotFound(format!(
            "vector index {}",
            record.index_id
        )));
    }
    Ok(())
}

/// Sets (or replaces) the policy for `(graph_id, index_id)` after validation.
pub(crate) fn set_policy(record: VectorMaintenancePolicyRecord) -> Result<(), RouterError> {
    validate(&record)?;
    let key = VectorIndexKey::new(record.graph_id, record.index_id);
    ROUTER_VECTOR_MAINTENANCE_POLICIES.with_borrow_mut(|map| {
        map.insert(key, record);
    });
    Ok(())
}

/// Flips `enabled = false` for an existing policy. `NotFound` if no policy exists.
pub(crate) fn disable_policy(graph_id: GraphId, index_id: u32) -> Result<(), RouterError> {
    let key = VectorIndexKey::new(graph_id, index_id);
    ROUTER_VECTOR_MAINTENANCE_POLICIES.with_borrow_mut(|map| {
        let mut record = map.get(&key).ok_or_else(|| {
            RouterError::NotFound(format!("vector maintenance policy {index_id}"))
        })?;
        record.enabled = false;
        map.insert(key, record);
        Ok(())
    })
}

/// Removes a policy. Returns whether a policy was present.
pub(crate) fn delete_policy(graph_id: GraphId, index_id: u32) -> bool {
    let key = VectorIndexKey::new(graph_id, index_id);
    ROUTER_VECTOR_MAINTENANCE_POLICIES.with_borrow_mut(|map| map.remove(&key).is_some())
}

pub(crate) fn get_policy(
    graph_id: GraphId,
    index_id: u32,
) -> Option<VectorMaintenancePolicyRecord> {
    ROUTER_VECTOR_MAINTENANCE_POLICIES
        .with_borrow(|map| map.get(&VectorIndexKey::new(graph_id, index_id)))
}

pub(crate) fn list_policies(graph_id: GraphId) -> Vec<VectorMaintenancePolicyRecord> {
    ROUTER_VECTOR_MAINTENANCE_POLICIES.with_borrow(|map| {
        let start = VectorIndexKey::new(graph_id, 0);
        map.range((Bound::Included(start), graph_upper(graph_id)))
            .map(|entry| entry.value())
            .collect()
    })
}

pub(crate) fn purge_graph_policies(graph_id: GraphId) {
    ROUTER_VECTOR_MAINTENANCE_POLICIES.with_borrow_mut(|map| {
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

/// Exclusive upper bound of one graph's key range (`graph_id` is the most-significant key
/// component); `Unbounded` at `GraphId::MAX` so the max graph's policies are not dropped.
fn graph_upper(graph_id: GraphId) -> Bound<VectorIndexKey> {
    match graph_id.raw().checked_add(1) {
        Some(next) => Bound::Excluded(VectorIndexKey::new(GraphId::from_raw(next), 0)),
        None => Bound::Unbounded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::EmbeddingNameId;
    use gleaph_graph_kernel::vector_index::{VectorEncoding, VectorIndexKind, VectorMetric};

    fn policy() -> VectorMaintenancePolicy {
        VectorMaintenancePolicy {
            recommended_tombstone_ratio_bps: 2_000,
            required_tombstone_ratio_bps: 5_000,
            recommended_skew_ratio_bps: 20_000,
            required_skew_ratio_bps: 40_000,
            min_total_rows: 100,
            min_tombstoned_rows: 10,
        }
    }

    fn record(graph_id: GraphId, index_id: u32) -> VectorMaintenancePolicyRecord {
        VectorMaintenancePolicyRecord {
            graph_id,
            index_id,
            enabled: true,
            policy: policy(),
            target_nlist: Some(8),
            sample_limit: 10_000,
            scan_max_pages: 64,
            rebuild_max_subjects: 5_000,
            cleanup_max_work: 5_000,
        }
    }

    fn register_def(graph_id: GraphId, index_id: u32) {
        crate::facade::stable::vector_index_catalog::register_vector_index(
            graph_id,
            index_id,
            EmbeddingNameId::from_raw(index_id as u16),
            VectorIndexKind::IvfFlat,
            VectorMetric::L2Squared,
            VectorEncoding::F32,
            16,
            None,
            false,
        )
        .expect("register def");
    }

    #[test]
    fn record_storable_roundtrip() {
        let rec = record(GraphId::from_raw(7), 3);
        assert_eq!(
            VectorMaintenancePolicyRecord::from_bytes(Cow::Owned(rec.into_bytes())),
            rec
        );
    }

    #[test]
    fn set_requires_existing_def() {
        let graph = GraphId::from_raw(930_001);
        assert!(matches!(
            set_policy(record(graph, 1)),
            Err(RouterError::NotFound(_))
        ));
        register_def(graph, 1);
        set_policy(record(graph, 1)).expect("set after def exists");
        assert!(get_policy(graph, 1).is_some());
    }

    #[test]
    fn set_rejects_inverted_thresholds_and_zero_budgets() {
        let graph = GraphId::from_raw(930_002);
        register_def(graph, 1);
        let mut inverted = record(graph, 1);
        inverted.policy.recommended_tombstone_ratio_bps = 6_000;
        inverted.policy.required_tombstone_ratio_bps = 5_000;
        assert!(matches!(
            set_policy(inverted),
            Err(RouterError::InvalidArgument(_))
        ));
        let mut zero = record(graph, 1);
        zero.scan_max_pages = 0;
        assert!(matches!(
            set_policy(zero),
            Err(RouterError::InvalidArgument(_))
        ));
    }

    #[test]
    fn disable_and_delete() {
        let graph = GraphId::from_raw(930_003);
        register_def(graph, 1);
        assert!(matches!(
            disable_policy(graph, 1),
            Err(RouterError::NotFound(_))
        ));
        set_policy(record(graph, 1)).expect("set");
        disable_policy(graph, 1).expect("disable");
        assert!(!get_policy(graph, 1).expect("present").enabled);
        assert!(delete_policy(graph, 1));
        assert!(!delete_policy(graph, 1));
        assert!(get_policy(graph, 1).is_none());
    }

    #[test]
    fn list_and_purge_are_graph_scoped() {
        let graph = GraphId::from_raw(930_004);
        let other = GraphId::from_raw(930_005);
        register_def(graph, 1);
        register_def(graph, 2);
        register_def(other, 1);
        set_policy(record(graph, 1)).expect("set");
        set_policy(record(graph, 2)).expect("set");
        set_policy(record(other, 1)).expect("set");
        assert_eq!(list_policies(graph).len(), 2);
        purge_graph_policies(graph);
        assert!(list_policies(graph).is_empty());
        assert_eq!(list_policies(other).len(), 1);
    }
}
