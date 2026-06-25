//! Pure maintenance-recommendation policy for the derived `ivf_flat` vector index (ADR 0031
//! Slice 9).
//!
//! [`recommend_partition_maintenance`] is a deterministic, side-effect-free function over already
//! merged partition health (the head-only skew [`VectorPartitionHealthSummary`] plus the page-meta
//! tombstone [`VectorPartitionPageHealth`]) and a caller-supplied [`VectorMaintenancePolicy`]. It
//! never reads storage; the canister gathers the health, the operator supplies the thresholds, and
//! this decides the three-state [`VectorMaintenanceRecommendation`].

use gleaph_graph_kernel::vector_index::{
    VectorIndexError, VectorMaintenancePolicy, VectorMaintenanceRecommendation,
    VectorPartitionHealthSummary, VectorPartitionPageHealth,
};

/// Basis-points denominator (1 bp = 1/10000).
const BPS_SCALE: u128 = 10_000;

/// Severity of a single signal, ordered so the overall recommendation is the max across signals.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Healthy,
    Recommended,
    Required,
}

/// Classifies one signal whose value is `num / den` (a ratio) against `recommended`/`required`
/// thresholds expressed in basis points, using `u128` so the cross-multiplied comparison cannot
/// overflow. `num`/`den` are the already-bps-scaled numerator/denominator (caller multiplies the
/// numerator by [`BPS_SCALE`]); crossing is `>=` (at-or-above the threshold trips it).
fn classify(num: u128, den: u128, recommended_bps: u32, required_bps: u32) -> Severity {
    if den == 0 {
        return Severity::Healthy;
    }
    if num >= den * required_bps as u128 {
        Severity::Required
    } else if num >= den * recommended_bps as u128 {
        Severity::Recommended
    } else {
        Severity::Healthy
    }
}

/// Recommends partition maintenance from merged health and a policy (ADR 0031 Slice 9).
///
/// Pure and deterministic. The result is the **max severity** across two independently gated signals
/// (so neither gate can mask the other):
///
/// - **Tombstone bloat** — ratio `tombstoned_rows / total_rows` (from the page-meta
///   [`VectorPartitionPageHealth`]). Judged only when `total_rows >= policy.min_total_rows` **and**
///   `tombstoned_rows >= policy.min_tombstoned_rows`.
/// - **Partition skew** — ratio `max_partition_live_rows / (live_rows / nlist)` i.e.
///   `max_partition_live_rows * nlist / live_rows` (from the head-only
///   [`VectorPartitionHealthSummary`]). Judged only when `live_rows >= policy.min_total_rows`
///   (the tombstone-specific `min_tombstoned_rows` gate does **not** apply to skew).
///
/// Both ratios are compared in basis points with `u128` cross-multiplication (no floats, no
/// overflow). Returns [`VectorIndexError::InvalidMaintenancePolicy`] if a `recommended_*_bps`
/// exceeds its paired `required_*_bps` (a nonsensical policy where "recommended" would be stricter
/// than "required").
pub(crate) fn recommend_partition_maintenance(
    summary: &VectorPartitionHealthSummary,
    page_health: &VectorPartitionPageHealth,
    policy: &VectorMaintenancePolicy,
) -> Result<VectorMaintenanceRecommendation, VectorIndexError> {
    if policy.recommended_tombstone_ratio_bps > policy.required_tombstone_ratio_bps
        || policy.recommended_skew_ratio_bps > policy.required_skew_ratio_bps
    {
        return Err(VectorIndexError::InvalidMaintenancePolicy);
    }

    // Tombstone signal: gated on physical total + absolute tombstone floor.
    let tombstone = if page_health.total_rows >= policy.min_total_rows
        && page_health.tombstoned_rows >= policy.min_tombstoned_rows
    {
        classify(
            page_health.tombstoned_rows as u128 * BPS_SCALE,
            page_health.total_rows as u128,
            policy.recommended_tombstone_ratio_bps,
            policy.required_tombstone_ratio_bps,
        )
    } else {
        Severity::Healthy
    };

    // Skew signal: gated only on live-row floor (independent of the tombstone gate).
    let skew = if summary.live_rows >= policy.min_total_rows && summary.nlist > 0 {
        classify(
            summary.max_partition_live_rows as u128 * summary.nlist as u128 * BPS_SCALE,
            summary.live_rows as u128,
            policy.recommended_skew_ratio_bps,
            policy.required_skew_ratio_bps,
        )
    } else {
        Severity::Healthy
    };

    Ok(match tombstone.max(skew) {
        Severity::Healthy => VectorMaintenanceRecommendation::Healthy,
        Severity::Recommended => VectorMaintenanceRecommendation::RebuildRecommended,
        Severity::Required => VectorMaintenanceRecommendation::RebuildRequired,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(
        nlist: u32,
        live_rows: u64,
        max_partition_live_rows: u64,
    ) -> VectorPartitionHealthSummary {
        VectorPartitionHealthSummary {
            nlist,
            partitions_examined: nlist,
            live_rows,
            page_count: 0,
            max_partition_live_rows,
        }
    }

    fn page(total_rows: u64, tombstoned_rows: u64) -> VectorPartitionPageHealth {
        VectorPartitionPageHealth {
            index_id: 1,
            index_version: 1,
            page_count: 1,
            total_rows,
            physical_live_rows: total_rows - tombstoned_rows,
            tombstoned_rows,
        }
    }

    /// Tombstone-focused policy: skew effectively disabled by an unreachable threshold.
    fn tombstone_policy() -> VectorMaintenancePolicy {
        VectorMaintenancePolicy {
            recommended_tombstone_ratio_bps: 2_000, // 20%
            required_tombstone_ratio_bps: 5_000,    // 50%
            recommended_skew_ratio_bps: u32::MAX,
            required_skew_ratio_bps: u32::MAX,
            min_total_rows: 100,
            min_tombstoned_rows: 10,
        }
    }

    /// Skew-focused policy: tombstone effectively disabled by an unreachable threshold.
    fn skew_policy() -> VectorMaintenancePolicy {
        VectorMaintenancePolicy {
            recommended_tombstone_ratio_bps: u32::MAX,
            required_tombstone_ratio_bps: u32::MAX,
            recommended_skew_ratio_bps: 20_000, // 2.0x average
            required_skew_ratio_bps: 40_000,    // 4.0x average
            min_total_rows: 100,
            min_tombstoned_rows: 0,
        }
    }

    #[test]
    fn invalid_policy_recommended_above_required_is_rejected() {
        let mut p = tombstone_policy();
        p.recommended_tombstone_ratio_bps = 6_000;
        p.required_tombstone_ratio_bps = 5_000;
        assert_eq!(
            recommend_partition_maintenance(&summary(1, 0, 0), &page(0, 0), &p).unwrap_err(),
            VectorIndexError::InvalidMaintenancePolicy
        );

        let mut p = skew_policy();
        p.recommended_skew_ratio_bps = 50_000;
        p.required_skew_ratio_bps = 40_000;
        assert_eq!(
            recommend_partition_maintenance(&summary(1, 0, 0), &page(0, 0), &p).unwrap_err(),
            VectorIndexError::InvalidMaintenancePolicy
        );
    }

    #[test]
    fn tombstone_ratio_three_states() {
        let p = tombstone_policy();
        let s = summary(4, 0, 0); // skew disabled by threshold + zero live
        // 10% tombstones: below recommended (20%).
        assert_eq!(
            recommend_partition_maintenance(&s, &page(1_000, 100), &p).unwrap(),
            VectorMaintenanceRecommendation::Healthy
        );
        // 30% tombstones: between recommended and required.
        assert_eq!(
            recommend_partition_maintenance(&s, &page(1_000, 300), &p).unwrap(),
            VectorMaintenanceRecommendation::RebuildRecommended
        );
        // 60% tombstones: at/above required.
        assert_eq!(
            recommend_partition_maintenance(&s, &page(1_000, 600), &p).unwrap(),
            VectorMaintenanceRecommendation::RebuildRequired
        );
    }

    #[test]
    fn tombstone_threshold_is_inclusive() {
        let p = tombstone_policy();
        let s = summary(1, 0, 0);
        // Exactly 20% trips recommended (>=).
        assert_eq!(
            recommend_partition_maintenance(&s, &page(1_000, 200), &p).unwrap(),
            VectorMaintenanceRecommendation::RebuildRecommended
        );
        // Exactly 50% trips required.
        assert_eq!(
            recommend_partition_maintenance(&s, &page(1_000, 500), &p).unwrap(),
            VectorMaintenanceRecommendation::RebuildRequired
        );
    }

    #[test]
    fn tombstone_gates_suppress_small_or_few() {
        let p = tombstone_policy();
        let s = summary(1, 0, 0);
        // total_rows below min_total_rows (100) -> not judged even at 100% tombstones.
        assert_eq!(
            recommend_partition_maintenance(&s, &page(50, 50), &p).unwrap(),
            VectorMaintenanceRecommendation::Healthy
        );
        // tombstoned_rows below min_tombstoned_rows (10) -> not judged even past ratio.
        assert_eq!(
            recommend_partition_maintenance(&s, &page(1_000, 9), &p).unwrap(),
            VectorMaintenanceRecommendation::Healthy
        );
    }

    #[test]
    fn skew_ratio_three_states() {
        let p = skew_policy();
        let pg = page(0, 0); // tombstone disabled
        // nlist=4, live=1000 -> avg=250. max=400 -> 1.6x: below recommended (2.0x).
        assert_eq!(
            recommend_partition_maintenance(&summary(4, 1_000, 400), &pg, &p).unwrap(),
            VectorMaintenanceRecommendation::Healthy
        );
        // max=600 -> 2.4x: between recommended and required.
        assert_eq!(
            recommend_partition_maintenance(&summary(4, 1_000, 600), &pg, &p).unwrap(),
            VectorMaintenanceRecommendation::RebuildRecommended
        );
        // max=1000 -> 4.0x: at required.
        assert_eq!(
            recommend_partition_maintenance(&summary(4, 1_000, 1_000), &pg, &p).unwrap(),
            VectorMaintenanceRecommendation::RebuildRequired
        );
    }

    #[test]
    fn skew_not_gated_by_min_tombstoned_rows() {
        // A highly skewed index with zero tombstones must still be flagged: skew uses only
        // min_total_rows, never min_tombstoned_rows.
        let mut p = skew_policy();
        p.min_tombstoned_rows = 1_000_000;
        assert_eq!(
            recommend_partition_maintenance(&summary(4, 1_000, 1_000), &page(1_000, 0), &p)
                .unwrap(),
            VectorMaintenanceRecommendation::RebuildRequired
        );
    }

    #[test]
    fn skew_gate_suppresses_below_min_total_rows() {
        let p = skew_policy();
        // live_rows (50) below min_total_rows (100) -> not judged despite extreme skew.
        assert_eq!(
            recommend_partition_maintenance(&summary(4, 50, 50), &page(0, 0), &p).unwrap(),
            VectorMaintenanceRecommendation::Healthy
        );
    }

    #[test]
    fn result_is_max_severity_across_independent_signals() {
        // Tombstone recommends, skew requires -> overall required.
        let p = VectorMaintenancePolicy {
            recommended_tombstone_ratio_bps: 2_000,
            required_tombstone_ratio_bps: 5_000,
            recommended_skew_ratio_bps: 20_000,
            required_skew_ratio_bps: 40_000,
            min_total_rows: 100,
            min_tombstoned_rows: 10,
        };
        let s = summary(4, 1_000, 1_000); // 4.0x skew -> required
        let pg = page(1_000, 300); // 30% tombstone -> recommended
        assert_eq!(
            recommend_partition_maintenance(&s, &pg, &p).unwrap(),
            VectorMaintenanceRecommendation::RebuildRequired
        );
    }

    #[test]
    fn large_counts_do_not_overflow() {
        // Near-u64-max counts: u128 cross-multiplication must not panic and must classify.
        let p = skew_policy();
        let big = u64::MAX / 2;
        let s = summary(1_024, big, big); // max==live with nlist=1024 -> 1024x skew
        assert_eq!(
            recommend_partition_maintenance(&s, &page(0, 0), &p).unwrap(),
            VectorMaintenanceRecommendation::RebuildRequired
        );
    }
}
