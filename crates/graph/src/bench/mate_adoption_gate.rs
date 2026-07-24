//! Frozen policy and conservative accounting for ADR 0048 adoption measurements.
//!
//! This module is deliberately side-effect free.  It does not select a caller path or inspect
//! MemoryManager pages; it gives the later fixture probes one source of truth for mode selection,
//! denominator accounting, and conservative byte totals.

#![expect(dead_code, reason = "policy is consumed by the adoption-gate probes")]

use super::mate_footprint::{MateFootprintInput, MateMode, MateSharedOverhead};
use canbench_rs::{bench, bench_fn};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const POLICY_VERSION: &str = "1.0";
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

/// Deterministic, representation-independent identity used by the adoption evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CanonicalIdentity {
    pub(crate) owner: u32,
    pub(crate) target: u32,
    pub(crate) orientation: u8,
    pub(crate) label: u16,
    pub(crate) slot: u32,
    pub(crate) inline_payload_fingerprint: String,
    pub(crate) payload_bytes: Vec<u8>,
}

impl Ord for CanonicalIdentity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.owner,
            self.target,
            self.orientation,
            self.label,
            self.slot,
            &self.inline_payload_fingerprint,
            &self.payload_bytes,
        )
            .cmp(&(
                other.owner,
                other.target,
                other.orientation,
                other.label,
                other.slot,
                &other.inline_payload_fingerprint,
                &other.payload_bytes,
            ))
    }
}

impl PartialOrd for CanonicalIdentity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl CanonicalIdentity {
    fn row_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(18 + self.payload_bytes.len());
        out.push(1);
        out.extend_from_slice(&self.owner.to_be_bytes());
        out.extend_from_slice(&self.target.to_be_bytes());
        out.push(self.orientation);
        out.extend_from_slice(&self.label.to_be_bytes());
        out.extend_from_slice(&self.slot.to_be_bytes());
        out.extend_from_slice(&(self.payload_bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload_bytes);
        out
    }
}

fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn canonical_identity_digest(rows: &[CanonicalIdentity]) -> String {
    let mut ordered = rows.to_vec();
    ordered.sort();
    let mut encoded = vec![1u8];
    encoded.extend_from_slice(&(ordered.len() as u32).to_be_bytes());
    for row in ordered {
        let bytes = row.row_bytes();
        encoded.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        encoded.extend_from_slice(&bytes);
    }
    digest_hex(&encoded)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ShapeDescriptor {
    pub(crate) shape_id: String,
    pub(crate) shape_definition_digest: String,
    pub(crate) fixture_ids: Vec<String>,
    pub(crate) logical_edges: u64,
    pub(crate) physical_half_edges: u64,
    pub(crate) alias_rows: u64,
    pub(crate) indexed_half_edges: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DeterministicFixture {
    pub(crate) descriptor: ShapeDescriptor,
    pub(crate) identities: Vec<CanonicalIdentity>,
}

fn shape_tag(shape: FixtureShape) -> u16 {
    match shape {
        FixtureShape::Directed => 1,
        FixtureShape::Undirected => 2,
        FixtureShape::DirectedSelfLoop => 3,
        FixtureShape::UndirectedSelfLoop => 4,
        FixtureShape::Parallel => 5,
        FixtureShape::SparseSlots => 6,
        FixtureShape::MixedLabels => 7,
    }
}

fn shape_definition_digest(spec: FixtureSpec) -> String {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&(spec.id.len() as u32).to_be_bytes());
    bytes.extend_from_slice(spec.id.as_bytes());
    bytes.extend_from_slice(&u64::from(shape_tag(spec.shape)).to_be_bytes());
    bytes.extend_from_slice(&spec.logical_edges.to_be_bytes());
    bytes.extend_from_slice(&spec.physical_half_edges.to_be_bytes());
    bytes.extend_from_slice(&spec.degree.to_be_bytes());
    digest_hex(&bytes)
}

pub(crate) fn build_fixture(spec: FixtureSpec) -> DeterministicFixture {
    let mut identities = Vec::with_capacity(spec.physical_half_edges as usize);
    for index in 0..spec.physical_half_edges {
        let payload_bytes = if matches!(
            spec.shape,
            FixtureShape::MixedLabels | FixtureShape::SparseSlots
        ) {
            vec![(index as u8).wrapping_mul(17), shape_tag(spec.shape) as u8]
        } else {
            Vec::new()
        };
        let owner = 1 + (index / 2) as u32;
        let target = 10_000 + index as u32;
        let slot = if matches!(spec.shape, FixtureShape::SparseSlots) {
            (index.saturating_mul(3)) as u32
        } else {
            index as u32
        };
        let mut row_seed = Vec::new();
        row_seed.extend_from_slice(&owner.to_be_bytes());
        row_seed.extend_from_slice(&target.to_be_bytes());
        row_seed.extend_from_slice(&payload_bytes);
        identities.push(CanonicalIdentity {
            owner,
            target,
            orientation: (index % 2) as u8,
            label: shape_tag(spec.shape),
            slot,
            inline_payload_fingerprint: digest_hex(&row_seed),
            payload_bytes,
        });
    }
    identities.sort();
    assert!(identities.windows(2).all(|pair| pair[0] != pair[1]));
    let descriptor = ShapeDescriptor {
        shape_id: spec.id.to_owned(),
        shape_definition_digest: shape_definition_digest(spec),
        fixture_ids: vec![format!("{}-fixture", spec.id)],
        logical_edges: spec.logical_edges,
        physical_half_edges: spec.physical_half_edges,
        alias_rows: spec.physical_half_edges,
        indexed_half_edges: spec.physical_half_edges,
    };
    assert_eq!(identities.len() as u64, descriptor.physical_half_edges);
    DeterministicFixture {
        descriptor,
        identities,
    }
}

/// Build a real AliasOnly identity fixture for the subset currently supported by the owning
/// `ic-stable-lara` adapter. Unsupported shapes remain synthetic/deferred until their owner exists.
#[cfg(feature = "canbench")]
pub(crate) fn build_real_alias_fixture(spec: FixtureSpec) -> Result<DeterministicFixture, String> {
    let vertex_count = u32::try_from(spec.logical_edges.saturating_add(1))
        .map_err(|_| "real AliasOnly fixture vertex count overflow".to_owned())?;
    let physical_identities = match spec.shape {
        FixtureShape::Directed => {
            let edges = (0..spec.logical_edges)
                .map(|index| {
                    u32::try_from(index + 1)
                        .map(|target| (0, target))
                        .map_err(|_| "real AliasOnly fixture endpoint overflow".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            ic_stable_lara::adoption_fixture::build_alias_only_fixture(vertex_count, &edges)?
                .identities
        }
        FixtureShape::Parallel => {
            let edges = (0..spec.logical_edges).map(|_| (0, 1)).collect::<Vec<_>>();
            ic_stable_lara::adoption_fixture::build_alias_only_fixture(2, &edges)?.identities
        }
        FixtureShape::Undirected => {
            let edges = (0..spec.logical_edges)
                .map(|index| {
                    u32::try_from(index + 1)
                        .map(|target| (0, target))
                        .map_err(|_| "real AliasOnly fixture endpoint overflow".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            ic_stable_lara::adoption_fixture::build_alias_only_undirected_fixture(
                vertex_count,
                &edges,
            )?
            .identities
        }
        FixtureShape::UndirectedSelfLoop => {
            ic_stable_lara::adoption_fixture::build_alias_only_undirected_fixture(1, &[(0, 0)])?
                .identities
        }
        _ => {
            return Err(
                "real AliasOnly adapter currently supports directed/parallel/undirected shapes only"
                    .to_owned(),
            );
        }
    };
    let identities = physical_identities
        .into_iter()
        .map(|identity| {
            let mut seed = Vec::new();
            seed.extend_from_slice(&identity.owner.to_be_bytes());
            seed.extend_from_slice(&identity.target.to_be_bytes());
            seed.push(identity.orientation);
            seed.extend_from_slice(&identity.slot.to_be_bytes());
            CanonicalIdentity {
                owner: identity.owner,
                target: identity.target,
                orientation: identity.orientation,
                label: 1,
                slot: identity.slot,
                inline_payload_fingerprint: digest_hex(&seed),
                payload_bytes: Vec::new(),
            }
        })
        .collect::<Vec<_>>();
    let descriptor = ShapeDescriptor {
        shape_id: spec.id.to_owned(),
        shape_definition_digest: shape_definition_digest(spec),
        fixture_ids: vec![format!("{}-alias-only", spec.id)],
        logical_edges: spec.logical_edges,
        physical_half_edges: identities.len() as u64,
        alias_rows: identities.len() as u64,
        indexed_half_edges: identities.len() as u64,
    };
    Ok(DeterministicFixture {
        descriptor,
        identities,
    })
}

/// Select the owning-layer fixture for evidence generation. Unsupported shapes retain their
/// descriptor but deliberately omit the identity digest instead of presenting synthetic rows as
/// real AliasOnly measurements.
fn build_evidence_fixture(spec: FixtureSpec) -> (ShapeDescriptor, Option<String>) {
    #[cfg(feature = "canbench")]
    if let Ok(fixture) = build_real_alias_fixture(spec) {
        return (
            fixture.descriptor,
            Some(canonical_identity_digest(&fixture.identities)),
        );
    }

    let synthetic = build_fixture(spec);
    (synthetic.descriptor, None)
}

fn spec_for_shape_id(id: &str) -> FixtureSpec {
    FixtureSpec::required_matrix()
        .into_iter()
        .find(|spec| spec.id == id)
        .expect("fixture descriptor must originate from required matrix")
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RequestIdentity {
    pub(crate) request_id: String,
    pub(crate) shape_id: String,
    pub(crate) direction: String,
    pub(crate) rank: u32,
    pub(crate) stratum: RequestStratumWire,
    pub(crate) payload_digest: String,
}

impl RequestIdentity {
    /// Stable request identity.  Fixture and representation names are deliberately excluded so
    /// the same logical request can be reused by multiple evidence rows.
    fn encoded_identity(&self) -> Vec<u8> {
        let mut out = vec![1u8];
        for value in [&self.shape_id, &self.direction] {
            let bytes = value.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        out.extend_from_slice(&self.rank.to_be_bytes());
        out.push(self.stratum as u8);
        let payload = self.payload_digest.as_bytes();
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum RequestStratumWire {
    ScanOnly,
    SampledCheckpoint,
    SampledNonCheckpoint,
    PackedValid,
    Malformed,
    Stale,
}

pub(crate) fn build_request_corpus(spec: FixtureSpec, seed: u64) -> Vec<RequestIdentity> {
    // Each stratum receives every representable rank for small fixtures and at least the frozen
    // minimum for larger fixtures.  Repeating ranks across strata is intentional: the request
    // identity includes the stratum and can therefore be joined across mode-specific rows.
    let rank_count = spec
        .physical_half_edges
        .max(MIN_HUB_REQUESTS as u64)
        .min(u32::MAX as u64) as u32;
    let strata = [
        RequestStratumWire::ScanOnly,
        RequestStratumWire::SampledCheckpoint,
        RequestStratumWire::SampledNonCheckpoint,
        RequestStratumWire::PackedValid,
        RequestStratumWire::Malformed,
        RequestStratumWire::Stale,
    ];
    let mut corpus = Vec::with_capacity(rank_count as usize * strata.len());
    for stratum in strata {
        let count = if spec.physical_half_edges < u64::from(MIN_HUB_REQUESTS) {
            spec.physical_half_edges as u32
        } else {
            rank_count
        };
        for rank in 0..count {
            let mut seed_bytes = seed.to_be_bytes().to_vec();
            seed_bytes.extend_from_slice(spec.id.as_bytes());
            seed_bytes.extend_from_slice(&rank.to_be_bytes());
            seed_bytes.push(stratum as u8);
            let payload_digest = digest_hex(&seed_bytes);
            let mut request = RequestIdentity {
                request_id: String::new(),
                shape_id: spec.id.to_owned(),
                direction: if rank % 2 == 0 { "forward" } else { "reverse" }.to_owned(),
                rank,
                stratum,
                payload_digest,
            };
            request.request_id = digest_hex(&request.encoded_identity());
            corpus.push(request);
        }
    }
    corpus
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum EvidenceStatus {
    Measured,
    Deferred,
    NotComparable,
    NotRepresented,
    NotApplicableScanOnly,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct EvidenceArtifact {
    pub(crate) schema_version: u8,
    pub(crate) policy_version: String,
    pub(crate) fixture_generator: u8,
    pub(crate) corpus_seed: u64,
    pub(crate) corpus_generator: u8,
    pub(crate) shape_descriptors: Vec<ShapeDescriptor>,
    pub(crate) corpus_generated_count: u32,
    pub(crate) rows: Vec<EvidenceRow>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct EvidenceRow {
    pub(crate) shape_id: String,
    pub(crate) fixture_id: String,
    pub(crate) status: EvidenceStatus,
    pub(crate) policy_version: String,
    pub(crate) canonical_identity_digest: Option<String>,
    pub(crate) request_identity: Option<String>,
    pub(crate) instruction_total: Option<u64>,
    pub(crate) exact_result_status: Option<bool>,
}

impl EvidenceArtifact {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 || self.fixture_generator != 1 || self.corpus_generator != 1 {
            return Err("unsupported artifact version");
        }
        if self.policy_version != POLICY_VERSION {
            return Err("policy version mismatch");
        }
        if self.shape_descriptors.is_empty() || self.rows.is_empty() {
            return Err("empty evidence artifact");
        }
        let mut previous = None;
        for descriptor in &self.shape_descriptors {
            if descriptor.shape_id.is_empty() || descriptor.fixture_ids.is_empty() {
                return Err("invalid shape descriptor");
            }
            if descriptor.shape_definition_digest.len() != 64
                || !descriptor
                    .shape_definition_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err("invalid shape definition digest");
            }
            let spec = spec_for_shape_id(&descriptor.shape_id);
            if descriptor.shape_definition_digest != shape_definition_digest(spec) {
                return Err("shape definition digest mismatch");
            }
            if previous.is_some_and(|value| value >= descriptor.shape_id.as_str()) {
                return Err("shape descriptors are not ordered");
            }
            previous = Some(descriptor.shape_id.as_str());
        }
        let mut fixture_ids = std::collections::BTreeSet::new();
        for descriptor in &self.shape_descriptors {
            if descriptor
                .fixture_ids
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            {
                return Err("fixture ids are not ordered");
            }
            for fixture_id in &descriptor.fixture_ids {
                if fixture_id.is_empty() || !fixture_ids.insert(fixture_id) {
                    return Err("duplicate fixture id");
                }
            }
        }
        for row in &self.rows {
            if row.policy_version != self.policy_version {
                return Err("row policy version mismatch");
            }
            let descriptor = self
                .shape_descriptors
                .iter()
                .find(|descriptor| descriptor.shape_id == row.shape_id)
                .ok_or("unknown shape")?;
            if !descriptor
                .fixture_ids
                .iter()
                .any(|id| id == &row.fixture_id)
            {
                return Err("unknown fixture");
            }
            if row.status == EvidenceStatus::Measured {
                let digest = row
                    .canonical_identity_digest
                    .as_deref()
                    .ok_or("measured row missing identity digest")?;
                if digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                {
                    return Err("invalid identity digest");
                }
                if row.instruction_total.is_none() {
                    return Err("measured row missing instructions");
                }
            }
        }
        Ok(())
    }

    pub(crate) fn to_yaml_compatible_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[bench(raw)]
fn bench_mate_adoption_fixture_corpus() -> canbench_rs::BenchResult {
    let evidence_fixtures = FixtureSpec::required_matrix()
        .into_iter()
        .map(build_evidence_fixture)
        .collect::<Vec<_>>();
    let mut descriptors = evidence_fixtures
        .iter()
        .map(|(descriptor, _)| descriptor.clone())
        .collect::<Vec<_>>();
    descriptors.sort_by(|left, right| left.shape_id.cmp(&right.shape_id));
    let corpus_generated_count = evidence_fixtures
        .iter()
        .map(|(descriptor, _)| {
            build_request_corpus(spec_for_shape_id(&descriptor.shape_id), 0x0048_0146).len() as u32
        })
        .sum();
    let rows = evidence_fixtures
        .iter()
        .map(|(descriptor, identity_digest)| EvidenceRow {
            shape_id: descriptor.shape_id.clone(),
            fixture_id: descriptor.fixture_ids[0].clone(),
            status: EvidenceStatus::Deferred,
            policy_version: POLICY_VERSION.to_owned(),
            canonical_identity_digest: identity_digest.clone(),
            request_identity: None,
            instruction_total: None,
            exact_result_status: None,
        })
        .collect::<Vec<_>>();
    let mut artifact = EvidenceArtifact {
        schema_version: 1,
        policy_version: POLICY_VERSION.to_owned(),
        fixture_generator: 1,
        corpus_seed: 0x0048_0146,
        corpus_generator: 1,
        shape_descriptors: descriptors.clone(),
        corpus_generated_count,
        rows,
    };
    artifact.shape_descriptors = descriptors;
    bench_fn(|| {
        let encoded = artifact
            .to_yaml_compatible_json()
            .expect("fixture evidence serializes");
        std::hint::black_box(encoded);
    })
}

#[cfg(test)]
mod fixture_evidence_tests {
    use super::*;

    #[test]
    fn required_shapes_have_matching_identity_cardinality_and_digest() {
        for spec in FixtureSpec::required_matrix() {
            let fixture = build_fixture(spec);
            assert_eq!(fixture.identities.len() as u64, spec.physical_half_edges);
            assert_eq!(fixture.descriptor.logical_edges, spec.logical_edges);
            assert_eq!(fixture.descriptor.alias_rows, spec.physical_half_edges);
            assert_eq!(
                fixture.descriptor.indexed_half_edges,
                spec.physical_half_edges
            );
            assert_eq!(canonical_identity_digest(&fixture.identities).len(), 64);
        }
    }

    #[test]
    fn corpus_is_seeded_and_deterministic() {
        let spec = FixtureSpec::required_matrix()[1];
        let corpus = build_request_corpus(spec, 7);
        assert_eq!(corpus, build_request_corpus(spec, 7));
        assert_ne!(corpus, build_request_corpus(spec, 8));
        for stratum in [
            RequestStratumWire::ScanOnly,
            RequestStratumWire::SampledCheckpoint,
            RequestStratumWire::SampledNonCheckpoint,
            RequestStratumWire::PackedValid,
            RequestStratumWire::Malformed,
            RequestStratumWire::Stale,
        ] {
            assert!(
                corpus
                    .iter()
                    .filter(|request| request.stratum == stratum)
                    .count()
                    >= MIN_HUB_REQUESTS as usize
            );
        }
        assert!(
            corpus
                .windows(2)
                .all(|pair| pair[0].request_id != pair[1].request_id)
        );
    }

    #[test]
    fn canonical_identity_round_trip_preserves_payload_and_digest() {
        let fixture = build_fixture(FixtureSpec::required_matrix()[7]);
        let encoded = serde_json::to_string(&fixture.identities).expect("serialize identities");
        let decoded: Vec<CanonicalIdentity> =
            serde_json::from_str(&encoded).expect("deserialize identities");
        assert_eq!(fixture.identities, decoded);
        assert_eq!(
            canonical_identity_digest(&fixture.identities),
            canonical_identity_digest(&decoded)
        );
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn real_alias_fixture_uses_lara_physical_slots() {
        for spec in [
            FixtureSpec::required_matrix()[0],
            FixtureSpec::required_matrix()[1],
            FixtureSpec::required_matrix()[2],
            FixtureSpec::required_matrix()[5],
            FixtureSpec::required_matrix()[6],
        ] {
            let fixture = build_real_alias_fixture(spec).expect("real alias fixture");
            assert_eq!(fixture.identities.len() as u64, spec.physical_half_edges);
            assert!(
                fixture
                    .identities
                    .iter()
                    .all(|identity| identity.slot < 256)
            );
            assert!(
                fixture
                    .identities
                    .iter()
                    .any(|identity| identity.orientation == 0)
            );
            if matches!(spec.shape, FixtureShape::Directed | FixtureShape::Parallel) {
                assert!(
                    fixture
                        .identities
                        .iter()
                        .any(|identity| identity.orientation == 1)
                );
            } else {
                assert!(
                    fixture
                        .identities
                        .iter()
                        .all(|identity| identity.orientation == 0)
                );
            }
            assert_eq!(fixture.descriptor.alias_rows, spec.physical_half_edges);
            assert_eq!(canonical_identity_digest(&fixture.identities).len(), 64);
            if spec.shape == FixtureShape::Parallel {
                assert_eq!(
                    fixture
                        .identities
                        .iter()
                        .filter(|identity| identity.orientation == 0)
                        .count() as u64,
                    spec.logical_edges
                );
            }
        }
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn evidence_uses_real_digest_only_for_supported_alias_shapes() {
        let parallel = build_evidence_fixture(FixtureSpec::required_matrix()[6]);
        assert!(parallel.1.is_some());

        let sparse = build_evidence_fixture(FixtureSpec::required_matrix()[7]);
        assert!(sparse.1.is_none());
        assert_eq!(sparse.0.fixture_ids, vec!["sparse_slots-fixture"]);

        let mixed = build_evidence_fixture(FixtureSpec::required_matrix()[8]);
        assert!(mixed.1.is_none());
        assert_eq!(mixed.0.fixture_ids, vec!["mixed_labels_low-fixture"]);
    }

    #[test]
    fn evidence_rejects_unknown_shape_and_bad_measured_row() {
        let artifact = EvidenceArtifact {
            schema_version: 1,
            policy_version: POLICY_VERSION.to_owned(),
            fixture_generator: 1,
            corpus_seed: 1,
            corpus_generator: 1,
            shape_descriptors: vec![build_fixture(FixtureSpec::required_matrix()[0]).descriptor],
            corpus_generated_count: 1,
            rows: vec![EvidenceRow {
                shape_id: "missing".to_owned(),
                fixture_id: "missing".to_owned(),
                status: EvidenceStatus::Measured,
                policy_version: POLICY_VERSION.to_owned(),
                canonical_identity_digest: None,
                request_identity: None,
                instruction_total: None,
                exact_result_status: None,
            }],
        };
        assert_eq!(artifact.validate(), Err("unknown shape"));
    }

    #[test]
    fn evidence_serializes_and_validates_deferred_fixture_rows() {
        let fixture = build_fixture(FixtureSpec::required_matrix()[0]);
        let artifact = EvidenceArtifact {
            schema_version: 1,
            policy_version: POLICY_VERSION.to_owned(),
            fixture_generator: 1,
            corpus_seed: 7,
            corpus_generator: 1,
            shape_descriptors: vec![fixture.descriptor.clone()],
            corpus_generated_count: 0,
            rows: vec![EvidenceRow {
                shape_id: fixture.descriptor.shape_id.clone(),
                fixture_id: fixture.descriptor.fixture_ids[0].clone(),
                status: EvidenceStatus::Deferred,
                policy_version: POLICY_VERSION.to_owned(),
                canonical_identity_digest: Some(canonical_identity_digest(&fixture.identities)),
                request_identity: None,
                instruction_total: None,
                exact_result_status: None,
            }],
        };
        assert!(artifact.validate().is_ok());
        let encoded = artifact
            .to_yaml_compatible_json()
            .expect("serialize evidence");
        assert!(encoded.contains("\"schema_version\": 1"));
    }
}
