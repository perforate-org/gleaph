//! Frozen policy and conservative accounting for ADR 0048 adoption measurements.
//!
//! This module is deliberately side-effect free.  It does not select a caller path or inspect
//! MemoryManager pages; it gives the later fixture probes one source of truth for mode selection,
//! denominator accounting, and conservative byte totals.

#![expect(dead_code, reason = "policy is consumed by the adoption-gate probes")]

use super::mate_footprint::{MateFootprintInput, MateMode, MateSharedOverhead};
use canbench_rs::{bench, bench_fn};

pub(crate) const POLICY_VERSION: &str = "adr0048-v1";
pub(crate) const SAMPLED_STRIDE: u8 = 32;
pub(crate) const MIN_HUB_REQUESTS: u32 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FixtureShape {
    Directed,
    Undirected,
    DirectedSelfLoop,
    UndirectedSelfLoop,
    Parallel,
    SparseSlots,
    MixedLabels,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FixtureSpec {
    pub(crate) id: &'static str,
    pub(crate) shape: FixtureShape,
    pub(crate) logical_edges: u64,
    pub(crate) physical_half_edges: u64,
    pub(crate) degree: u64,
}

impl FixtureSpec {
    /// Frozen matrix used by the gate.  The entries are intentionally separate rows; callers must
    /// not average shapes or replace a sparse/self-loop row with a denser proxy.
    pub(crate) const fn required_matrix() -> [Self; 10] {
        [
            Self {
                id: "directed_low",
                shape: FixtureShape::Directed,
                logical_edges: 8,
                physical_half_edges: 16,
                degree: 8,
            },
            Self {
                id: "directed_high",
                shape: FixtureShape::Directed,
                logical_edges: 128,
                physical_half_edges: 256,
                degree: 128,
            },
            Self {
                id: "undirected_low",
                shape: FixtureShape::Undirected,
                logical_edges: 8,
                physical_half_edges: 16,
                degree: 8,
            },
            Self {
                id: "undirected_high",
                shape: FixtureShape::Undirected,
                logical_edges: 128,
                physical_half_edges: 256,
                degree: 128,
            },
            Self {
                id: "directed_self_loop",
                shape: FixtureShape::DirectedSelfLoop,
                logical_edges: 1,
                physical_half_edges: 1,
                degree: 1,
            },
            Self {
                id: "undirected_self_loop",
                shape: FixtureShape::UndirectedSelfLoop,
                logical_edges: 1,
                physical_half_edges: 1,
                degree: 1,
            },
            Self {
                id: "parallel",
                shape: FixtureShape::Parallel,
                logical_edges: 32,
                physical_half_edges: 64,
                degree: 32,
            },
            Self {
                id: "sparse_slots",
                shape: FixtureShape::SparseSlots,
                logical_edges: 32,
                physical_half_edges: 64,
                degree: 32,
            },
            Self {
                id: "mixed_labels_low",
                shape: FixtureShape::MixedLabels,
                logical_edges: 8,
                physical_half_edges: 16,
                degree: 8,
            },
            Self {
                id: "mixed_labels_high",
                shape: FixtureShape::MixedLabels,
                logical_edges: 128,
                physical_half_edges: 256,
                degree: 128,
            },
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessProfile {
    Cold,
    Warm,
    Hot,
}

impl AccessProfile {
    pub(crate) const fn prior_accesses(self) -> u32 {
        match self {
            Self::Cold => 0,
            Self::Warm => 64,
            Self::Hot => 256,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectedMode {
    ScanOnly,
    Sampled { stride: u8 },
    Packed { width_bytes: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestStratum {
    ScanOnly,
    SampledCheckpoint,
    SampledNonCheckpoint,
    PackedValid,
    Malformed,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RejectionReason {
    Malformed,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeObservation {
    pub(crate) mode: SelectedMode,
    pub(crate) stratum: RequestStratum,
    pub(crate) requests: u32,
    pub(crate) required_min_requests: u32,
    pub(crate) fallbacks: u32,
    pub(crate) exact_results: bool,
    pub(crate) rejection: Option<RejectionReason>,
}

impl RuntimeObservation {
    pub(crate) fn satisfies_guard(self) -> bool {
        if self.required_min_requests == 0
            || self.requests == 0
            || self.requests < self.required_min_requests
            || self.fallbacks > self.requests
        {
            return false;
        }
        match (self.mode, self.stratum) {
            (SelectedMode::ScanOnly, RequestStratum::ScanOnly) => {
                self.rejection.is_none() && self.exact_results && self.fallbacks == 0
            }
            (SelectedMode::Sampled { .. }, RequestStratum::SampledCheckpoint) => {
                self.rejection.is_none() && self.exact_results && self.fallbacks == 0
            }
            (SelectedMode::Sampled { .. }, RequestStratum::SampledNonCheckpoint) => {
                self.rejection.is_none() && self.exact_results && self.fallbacks == self.requests
            }
            (SelectedMode::Packed { .. }, RequestStratum::PackedValid) => {
                self.rejection.is_none() && self.exact_results && self.fallbacks == 0
            }
            (_, RequestStratum::Malformed) => {
                !self.exact_results && self.rejection == Some(RejectionReason::Malformed)
            }
            (_, RequestStratum::Stale) => {
                !self.exact_results && self.rejection == Some(RejectionReason::Stale)
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SelectionInputs {
    pub(crate) live_degree: u64,
    pub(crate) physical_capacity: u64,
    pub(crate) max_live_slot: u64,
    pub(crate) access_profile: AccessProfile,
}

impl SelectionInputs {
    pub(crate) fn validate(self) -> bool {
        self.physical_capacity != 0
            && self.live_degree <= self.physical_capacity
            && self.max_live_slot < self.physical_capacity
    }

    pub(crate) fn occupancy_numerator_denominator(self) -> (u64, u64) {
        (self.live_degree, self.physical_capacity)
    }
}

/// Frozen adaptive precedence from ADR 0048.  Occupancy is recorded as an input but intentionally
/// does not add a second threshold in this gate.
pub(crate) fn select_mode(inputs: SelectionInputs) -> Option<SelectedMode> {
    if !inputs.validate() {
        return None;
    }
    if inputs.live_degree < 32 || matches!(inputs.access_profile, AccessProfile::Cold) {
        return Some(SelectedMode::ScanOnly);
    }
    if matches!(inputs.access_profile, AccessProfile::Warm) {
        return Some(SelectedMode::Sampled {
            stride: SAMPLED_STRIDE,
        });
    }
    Some(match packed_width_for_slot(inputs.max_live_slot) {
        Some(width_bytes) => SelectedMode::Packed { width_bytes },
        None => SelectedMode::Sampled {
            stride: SAMPLED_STRIDE,
        },
    })
}

pub(crate) const fn packed_width_for_slot(slot: u64) -> Option<u8> {
    if slot <= 0xff {
        Some(1)
    } else if slot <= 0xffff {
        Some(2)
    } else if slot <= 0x00ff_ffff {
        Some(3)
    } else if slot <= 0xffff_ffff {
        Some(4)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ByteCategories {
    pub(crate) raw_payload: u64,
    pub(crate) locator: u64,
    pub(crate) blob_header: u64,
    pub(crate) directory: u64,
    pub(crate) allocator: u64,
    pub(crate) stable_structure: u64,
    pub(crate) region: u64,
    pub(crate) retained_free_span: u64,
    pub(crate) rebuild_reserve: u64,
    pub(crate) retired_blob: u64,
    pub(crate) unknown_upper_bound: u64,
    pub(crate) unknown_bound_proven: bool,
}

impl ByteCategories {
    pub(crate) fn conservative_total(self) -> Option<u64> {
        if !self.unknown_bound_proven {
            return None;
        }
        [
            self.raw_payload,
            self.locator,
            self.blob_header,
            self.directory,
            self.allocator,
            self.stable_structure,
            self.region,
            self.retained_free_span,
            self.rebuild_reserve,
            self.retired_blob,
            self.unknown_upper_bound,
        ]
        .into_iter()
        .try_fold(0u64, u64::checked_add)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Denominators {
    pub(crate) logical_edges: u64,
    pub(crate) physical_half_edges: u64,
    pub(crate) alias_rows: u64,
    pub(crate) indexed_half_edges: u64,
}

#[bench(raw)]
fn bench_mate_adoption_policy_matrix() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let mut packed = 0u64;
        for fixture in FixtureSpec::required_matrix() {
            for profile in [AccessProfile::Cold, AccessProfile::Warm, AccessProfile::Hot] {
                let mode = select_mode(SelectionInputs {
                    live_degree: fixture.degree,
                    physical_capacity: fixture.degree.saturating_mul(2),
                    max_live_slot: fixture.degree.saturating_mul(2).saturating_sub(1),
                    access_profile: profile,
                })
                .expect("valid frozen fixture selection");
                if matches!(mode, SelectedMode::Packed { .. }) {
                    packed = packed.saturating_add(1);
                }
            }
        }
        std::hint::black_box(packed);
    })
}

#[bench(raw)]
fn bench_mate_adoption_dense_locator_accounting() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let footprint = MateFootprintInput {
            entries: 128,
            mode: MateMode::Sampled { stride: 32 },
            shared: MateSharedOverhead {
                blob_header_bytes: 32,
                indexed_bucket_directory_bytes: 16,
                free_span_bytes: 24,
                rebuild_reserve_bytes: 64,
            },
        }
        .calculate_with_locator_rows(24)
        .expect("frozen footprint");
        std::hint::black_box(footprint);
    })
}

impl Denominators {
    pub(crate) fn bytes_per_logical_edge(self, bytes: u64) -> Option<u64> {
        self.ceil_div(bytes, self.logical_edges)
    }

    pub(crate) fn bytes_per_physical_half_edge(self, bytes: u64) -> Option<u64> {
        self.ceil_div(bytes, self.physical_half_edges)
    }

    fn ceil_div(self, bytes: u64, denominator: u64) -> Option<u64> {
        (denominator != 0)
            .then_some(bytes)
            .and_then(|bytes| bytes.checked_add(denominator - 1))
            .map(|bytes| bytes / denominator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_precedence_is_frozen() {
        assert_eq!(
            select_mode(SelectionInputs {
                live_degree: 31,
                physical_capacity: 2_048,
                max_live_slot: 1_000,
                access_profile: AccessProfile::Hot,
            })
            .expect("valid selection"),
            SelectedMode::ScanOnly
        );
        assert_eq!(
            select_mode(SelectionInputs {
                live_degree: 64,
                physical_capacity: 2_048,
                max_live_slot: 1_000,
                access_profile: AccessProfile::Cold,
            })
            .expect("valid selection"),
            SelectedMode::ScanOnly
        );
        assert_eq!(
            select_mode(SelectionInputs {
                live_degree: 64,
                physical_capacity: 2_048,
                max_live_slot: 1_000,
                access_profile: AccessProfile::Warm,
            })
            .expect("valid selection"),
            SelectedMode::Sampled { stride: 32 }
        );
        assert_eq!(
            select_mode(SelectionInputs {
                live_degree: 64,
                physical_capacity: 65_536,
                max_live_slot: 0xffff,
                access_profile: AccessProfile::Hot,
            })
            .expect("valid selection"),
            SelectedMode::Packed { width_bytes: 2 }
        );
    }

    #[test]
    fn unrepresentable_slot_falls_back_to_sampled() {
        assert_eq!(packed_width_for_slot(u32::MAX.into()), Some(4));
        assert_eq!(
            select_mode(SelectionInputs {
                live_degree: 64,
                physical_capacity: u64::from(u32::MAX) + 2,
                max_live_slot: u64::from(u32::MAX) + 1,
                access_profile: AccessProfile::Hot,
            })
            .expect("valid selection"),
            SelectedMode::Sampled { stride: 32 }
        );
    }

    #[test]
    fn invalid_occupancy_is_rejected_before_mode_selection() {
        assert_eq!(
            select_mode(SelectionInputs {
                live_degree: 9,
                physical_capacity: 8,
                max_live_slot: 7,
                access_profile: AccessProfile::Hot,
            }),
            None
        );
        assert_eq!(
            select_mode(SelectionInputs {
                live_degree: 1,
                physical_capacity: 0,
                max_live_slot: 0,
                access_profile: AccessProfile::Cold,
            }),
            None
        );
    }

    #[test]
    fn byte_total_includes_unknown_bound_and_overflow_fails_closed() {
        let categories = ByteCategories {
            raw_payload: 10,
            unknown_upper_bound: 7,
            unknown_bound_proven: true,
            ..ByteCategories::default()
        };
        assert_eq!(categories.conservative_total(), Some(17));
        assert_eq!(
            ByteCategories {
                raw_payload: u64::MAX,
                unknown_upper_bound: 1,
                unknown_bound_proven: true,
                ..ByteCategories::default()
            }
            .conservative_total(),
            None
        );
        assert_eq!(
            ByteCategories {
                raw_payload: 10,
                unknown_upper_bound: 7,
                ..ByteCategories::default()
            }
            .conservative_total(),
            None
        );
    }

    #[test]
    fn denominators_never_silently_divide_by_zero() {
        let denominators = Denominators {
            logical_edges: 0,
            physical_half_edges: 0,
            alias_rows: 0,
            indexed_half_edges: 0,
        };
        assert_eq!(denominators.bytes_per_logical_edge(10), None);
        assert_eq!(denominators.bytes_per_physical_half_edge(10), None);
        let denominators = Denominators {
            logical_edges: 2,
            physical_half_edges: 3,
            alias_rows: 0,
            indexed_half_edges: 0,
        };
        assert_eq!(denominators.bytes_per_logical_edge(19), Some(10));
        assert_eq!(denominators.bytes_per_physical_half_edge(19), Some(7));
    }

    #[test]
    fn fixture_matrix_keeps_edge_shapes_as_separate_rows() {
        let matrix = FixtureSpec::required_matrix();
        assert_eq!(matrix.len(), 10);
        assert!(
            matrix
                .iter()
                .any(|row| row.shape == FixtureShape::DirectedSelfLoop)
        );
        assert!(
            matrix
                .iter()
                .any(|row| row.shape == FixtureShape::UndirectedSelfLoop)
        );
        assert!(
            matrix
                .iter()
                .any(|row| row.shape == FixtureShape::SparseSlots)
        );
        assert!(matrix.iter().any(|row| row.shape == FixtureShape::Parallel));
    }

    #[test]
    fn runtime_guard_requires_mode_specific_fallback_counts() {
        assert!(
            RuntimeObservation {
                mode: SelectedMode::Sampled { stride: 32 },
                stratum: RequestStratum::SampledNonCheckpoint,
                requests: 4,
                required_min_requests: 1,
                fallbacks: 4,
                exact_results: true,
                rejection: None,
            }
            .satisfies_guard()
        );
        assert!(
            !RuntimeObservation {
                mode: SelectedMode::Packed { width_bytes: 1 },
                stratum: RequestStratum::PackedValid,
                requests: 4,
                required_min_requests: 1,
                fallbacks: 1,
                exact_results: true,
                rejection: None,
            }
            .satisfies_guard()
        );
    }

    #[test]
    fn zero_or_underrepresented_strata_do_not_pass() {
        for requests in [0, 63] {
            assert!(
                !RuntimeObservation {
                    mode: SelectedMode::Packed { width_bytes: 1 },
                    stratum: RequestStratum::PackedValid,
                    requests,
                    required_min_requests: 64,
                    fallbacks: 0,
                    exact_results: true,
                    rejection: None,
                }
                .satisfies_guard()
            );
        }
    }

    #[test]
    fn malformed_and_stale_probes_require_rejection() {
        assert!(
            !RuntimeObservation {
                mode: SelectedMode::Packed { width_bytes: 1 },
                stratum: RequestStratum::Malformed,
                requests: 1,
                required_min_requests: 1,
                fallbacks: 0,
                exact_results: true,
                rejection: Some(RejectionReason::Stale),
            }
            .satisfies_guard()
        );
        assert!(
            RuntimeObservation {
                mode: SelectedMode::Packed { width_bytes: 1 },
                stratum: RequestStratum::Malformed,
                requests: 1,
                required_min_requests: 1,
                fallbacks: 0,
                exact_results: false,
                rejection: Some(RejectionReason::Malformed),
            }
            .satisfies_guard()
        );
        assert!(
            !RuntimeObservation {
                mode: SelectedMode::Packed { width_bytes: 1 },
                stratum: RequestStratum::Malformed,
                requests: 1,
                required_min_requests: 1,
                fallbacks: 0,
                exact_results: true,
                rejection: Some(RejectionReason::Malformed),
            }
            .satisfies_guard()
        );
        assert!(
            RuntimeObservation {
                mode: SelectedMode::Packed { width_bytes: 1 },
                stratum: RequestStratum::Stale,
                requests: 1,
                required_min_requests: 1,
                fallbacks: 0,
                exact_results: false,
                rejection: Some(RejectionReason::Stale),
            }
            .satisfies_guard()
        );
    }
}
