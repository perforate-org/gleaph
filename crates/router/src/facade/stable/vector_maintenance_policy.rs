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
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_EPS_BPS, VECTOR_EPS_BPS_INFINITY, VectorMaintenancePolicy,
};
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
    /// Target-generation two-level fan-in (`Some(f)`, `f >= 2`) forwarded to the policy-driven
    /// rebuild start; `None` keeps the flat lifecycle (Slice 9).
    pub target_fine_nlist: Option<u32>,
    /// Target-generation RaBitQ code tier forwarded to the policy-driven rebuild start; `None`
    /// resolves to the Router recommended default (tier on) at snapshot time, `Some(false)`
    /// explicitly opts out.
    pub code_tier: Option<bool>,
    /// Target-generation coarse-stage ε₂ pruning in basis points (`0` = nearest-partition-only,
    /// [`VECTOR_EPS_BPS_INFINITY`] = full scan); `None` = `0` (Slice 9).
    pub eps_query_bps: Option<u32>,
    /// Target-generation leaf-stage ε₂ pruning in basis points (same unit/sentinel as
    /// `eps_query_bps`); `None` = `0` (Slice 9).
    pub eps_fine_bps: Option<u32>,
    /// Slab-compaction trigger: start the plan-0278 driver when
    /// `estimated_unreferenced_bytes >= threshold`; `None` disables the driver (plan 0343).
    /// Tombstone row-ratio (the rebuild recommendation signal) is NOT what compaction fixes,
    /// so this gate is deliberately independent of the rebuild thresholds.
    pub compact_dead_bytes_threshold: Option<u64>,
    /// Per-step compaction budgets forwarded to `admin_vector_slab_compact_step` (plan 0343).
    /// Validated nonzero only when the threshold is `Some` (a `None` threshold leaves the
    /// driver disabled, so migrated V1 records keep zero budgets without failing validation).
    pub compact_max_pages: u32,
    pub compact_max_bytes: u64,
}

/// Versioned stable envelope (ADR 0007) so the record schema can evolve across upgrades.
///
/// `V1` must keep decoding forever: pre-0343 records (no compaction fields) map to a disabled
/// driver (`compact_dead_bytes_threshold: None`, zero budgets).
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum VectorMaintenancePolicyStableRecord {
    V1(VectorMaintenancePolicyRecordV1),
    V2(VectorMaintenancePolicyRecord),
}

/// Pre-0343 policy shape (no slab-compaction fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
struct VectorMaintenancePolicyRecordV1 {
    pub graph_id: GraphId,
    pub index_id: u32,
    pub enabled: bool,
    pub policy: VectorMaintenancePolicy,
    pub target_nlist: Option<u32>,
    pub sample_limit: u32,
    pub scan_max_pages: u32,
    pub rebuild_max_subjects: u32,
    pub cleanup_max_work: u32,
    pub target_fine_nlist: Option<u32>,
    pub code_tier: Option<bool>,
    pub eps_query_bps: Option<u32>,
    pub eps_fine_bps: Option<u32>,
}

impl Storable for VectorMaintenancePolicyRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&VectorMaintenancePolicyStableRecord::V2(*self))
                .expect("encode vector maintenance policy"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&VectorMaintenancePolicyStableRecord::V2(self))
            .expect("encode vector maintenance policy")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), VectorMaintenancePolicyStableRecord)
            .expect("decode vector maintenance policy")
        {
            // Pre-0343 records never drove compaction: keep the driver disabled.
            VectorMaintenancePolicyStableRecord::V1(v1) => VectorMaintenancePolicyRecord {
                graph_id: v1.graph_id,
                index_id: v1.index_id,
                enabled: v1.enabled,
                policy: v1.policy,
                target_nlist: v1.target_nlist,
                sample_limit: v1.sample_limit,
                scan_max_pages: v1.scan_max_pages,
                rebuild_max_subjects: v1.rebuild_max_subjects,
                cleanup_max_work: v1.cleanup_max_work,
                target_fine_nlist: v1.target_fine_nlist,
                code_tier: v1.code_tier,
                eps_query_bps: v1.eps_query_bps,
                eps_fine_bps: v1.eps_fine_bps,
                compact_dead_bytes_threshold: None,
                compact_max_pages: 0,
                compact_max_bytes: 0,
            },
            VectorMaintenancePolicyStableRecord::V2(record) => record,
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
    // Slice 9 tuning selections: a two-level fan-in below 2 is flat with extra storage, and an
    // ε₂ bps other than the ∞ sentinel must not exceed `MAX_VECTOR_EPS_BPS` (the threshold factor
    // would otherwise degenerate toward a full walk and blur the distinction from ∞).
    if record.target_fine_nlist.is_some_and(|f| f < 2) {
        return Err(RouterError::InvalidArgument(
            "target_fine_nlist must be >= 2".to_owned(),
        ));
    }
    for (name, bps) in [
        ("eps_query_bps", record.eps_query_bps),
        ("eps_fine_bps", record.eps_fine_bps),
    ] {
        if bps.is_some_and(|b| b != VECTOR_EPS_BPS_INFINITY && b > MAX_VECTOR_EPS_BPS) {
            return Err(RouterError::InvalidArgument(format!(
                "{name} must be <= MAX_VECTOR_EPS_BPS or the ∞ sentinel"
            )));
        }
    }
    if get_vector_index(record.graph_id, record.index_id).is_none() {
        return Err(RouterError::NotFound(format!(
            "vector index {}",
            record.index_id
        )));
    }
    // Compaction gate (plan 0343): a `Some` threshold arms the driver, so the step budgets
    // must be nonzero and the threshold itself must be non-trivial (`0` would start a
    // compaction on every advance even with no dead space). `None` disables the driver and
    // skips the budget checks (migrated V1 records carry zero budgets).
    match record.compact_dead_bytes_threshold {
        None => {}
        Some(0) => {
            return Err(RouterError::InvalidArgument(
                "compact_dead_bytes_threshold must be >= 1 when set".to_owned(),
            ));
        }
        Some(_) if record.compact_max_pages == 0 || record.compact_max_bytes == 0 => {
            return Err(RouterError::InvalidArgument(
                "compact_max_pages/compact_max_bytes must be nonzero when the compaction threshold is set".to_owned(),
            ));
        }
        Some(_) => {}
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
            target_fine_nlist: None,
            code_tier: None,
            eps_query_bps: None,
            eps_fine_bps: None,
            compact_dead_bytes_threshold: None,
            compact_max_pages: 0,
            compact_max_bytes: 0,
        }
    }

    fn register_def(graph_id: GraphId, index_id: u32) {
        let index_name_id = crate::facade::stable::index_name_catalog::intern_index_name(
            graph_id,
            &format!("test_vector_index_{index_id}"),
        )
        .expect("intern vector index name");
        let embedding_name_id =
            crate::facade::stable::embedding_name_catalog::intern_embedding_name(
                graph_id,
                &format!("test_embedding_field_{index_id}"),
            )
            .expect("intern embedding field name");
        crate::facade::stable::vector_index_catalog::register_vector_index(
            graph_id,
            index_id,
            index_name_id,
            embedding_name_id,
            vec![gleaph_graph_kernel::entry::VertexLabelId::from_raw(1)],
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
    fn set_rejects_bad_tuning_selections() {
        let graph = GraphId::from_raw(930_006);
        register_def(graph, 1);
        // A single-child fan-in is flat with extra storage, not a meaningful two-level shape.
        let mut single_child = record(graph, 1);
        single_child.target_fine_nlist = Some(1);
        assert!(matches!(
            set_policy(single_child),
            Err(RouterError::InvalidArgument(_))
        ));
        // An ε₂ bps above the documented cap (other than the ∞ sentinel) is rejected.
        let mut oversized = record(graph, 1);
        oversized.eps_query_bps = Some(MAX_VECTOR_EPS_BPS + 1);
        assert!(matches!(
            set_policy(oversized),
            Err(RouterError::InvalidArgument(_))
        ));
        let mut oversized_fine = record(graph, 1);
        oversized_fine.eps_fine_bps = Some(MAX_VECTOR_EPS_BPS + 1);
        assert!(matches!(
            set_policy(oversized_fine),
            Err(RouterError::InvalidArgument(_))
        ));
        // The boundary value and the ∞ sentinel both pass.
        let mut boundary = record(graph, 1);
        boundary.eps_query_bps = Some(MAX_VECTOR_EPS_BPS);
        boundary.eps_fine_bps = Some(VECTOR_EPS_BPS_INFINITY);
        boundary.target_fine_nlist = Some(2);
        set_policy(boundary).expect("boundary tuning selections admit");
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
    fn compaction_gate_validation() {
        let graph = GraphId::from_raw(930_007);
        register_def(graph, 1);
        // Disabled driver (None): zero budgets pass (V1-migrated shape).
        set_policy(record(graph, 1)).expect("disabled compaction passes");
        // Armed driver: nonzero budgets pass.
        let mut armed = record(graph, 1);
        armed.compact_dead_bytes_threshold = Some(65_536);
        armed.compact_max_pages = 8;
        armed.compact_max_bytes = 1 << 20;
        set_policy(armed).expect("armed compaction passes");
        // Zero threshold: degenerate always-start, rejected.
        let mut zero_threshold = record(graph, 1);
        zero_threshold.compact_dead_bytes_threshold = Some(0);
        zero_threshold.compact_max_pages = 8;
        zero_threshold.compact_max_bytes = 1 << 20;
        assert!(matches!(
            set_policy(zero_threshold),
            Err(RouterError::InvalidArgument(_))
        ));
        // Armed but zero budgets: rejected (wrong-impl guard: budgets must gate the driver).
        let mut zero_budgets = record(graph, 1);
        zero_budgets.compact_dead_bytes_threshold = Some(65_536);
        assert!(matches!(
            set_policy(zero_budgets),
            Err(RouterError::InvalidArgument(_))
        ));
    }

    #[test]
    fn v1_stable_bytes_migrate_to_disabled_compaction() {
        let rec = record(GraphId::from_raw(7), 3);
        let v1 = VectorMaintenancePolicyRecordV1 {
            graph_id: rec.graph_id,
            index_id: rec.index_id,
            enabled: rec.enabled,
            policy: rec.policy,
            target_nlist: rec.target_nlist,
            sample_limit: rec.sample_limit,
            scan_max_pages: rec.scan_max_pages,
            rebuild_max_subjects: rec.rebuild_max_subjects,
            cleanup_max_work: rec.cleanup_max_work,
            target_fine_nlist: rec.target_fine_nlist,
            code_tier: rec.code_tier,
            eps_query_bps: rec.eps_query_bps,
            eps_fine_bps: rec.eps_fine_bps,
        };
        let bytes: Cow<'_, [u8]> = Cow::Owned(
            Encode!(&VectorMaintenancePolicyStableRecord::V1(v1)).expect("encode V1 envelope"),
        );
        let migrated = VectorMaintenancePolicyRecord::from_bytes(bytes);
        assert_eq!(migrated.compact_dead_bytes_threshold, None);
        assert_eq!(migrated.compact_max_pages, 0);
        assert_eq!(migrated.compact_max_bytes, 0);
        let mut without_compaction = rec;
        without_compaction.compact_dead_bytes_threshold = None;
        without_compaction.compact_max_pages = 0;
        without_compaction.compact_max_bytes = 0;
        assert_eq!(migrated, without_compaction);
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
