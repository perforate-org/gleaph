//! Frozen policy and conservative accounting for ADR 0048 adoption measurements.
//!
//! This module is deliberately side-effect free.  It does not select a caller path or inspect
//! MemoryManager pages; it gives the later fixture probes one source of truth for mode selection,
//! denominator accounting, and conservative byte totals.

#![expect(dead_code, reason = "policy is consumed by the adoption-gate probes")]

use super::mate_compression::{
    SampledPairedResidualLookup, SharedOrientationLookup, UndirectedBlockRankPermutationLookup,
    UndirectedPairRankExceptionLookup, UndirectedPairRankLookup, delta_restart_bytes,
    delta_restart_reconstruct_at, mate_slot_sequences, monotone_elias_fano_bytes,
    shared_orientation_bytes,
};
use super::mate_footprint::{MateFootprintInput, MateMode, MateSharedOverhead};
use canbench_rs::{bench, bench_fn};
use ic_stable_lara::traits::CsrEdge;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::hint::black_box;

pub(crate) const POLICY_VERSION: &str = "1.0";
pub(crate) const SAMPLED_STRIDE: u8 = 32;
pub(crate) const MIN_HUB_REQUESTS: u32 = 64;
pub(crate) const PROMOTE_MIN_LIVE_EDGES: u64 = 32;
pub(crate) const DEMOTE_MAX_LIVE_EDGES: u64 = 16;

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
                // Directed self-loops occupy one forward and one reverse orientation row.
                physical_half_edges: 2,
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

/// Representation selected by the measurement-only adoption gate.  This is deliberately not a
/// GraphStore activation switch: callers still use the canonical path until a later slice wires
/// one of these dispositions into production.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdoptionDisposition {
    ScanOnly,
    SharedOrientation,
    PairRank,
    RankIndexedPacked,
    Deferred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdoptionStatus {
    Adopt,
    Partial { ready: u8, total: u8 },
    Hold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AdoptionFixtureId {
    DirectedLow,
    DirectedHigh,
    UndirectedLow,
    UndirectedHigh,
    DirectedSelfLoop,
    UndirectedSelfLoop,
    Parallel,
    SparseSlots,
    MixedLabelsLow,
    MixedLabelsHigh,
}

impl AdoptionFixtureId {
    pub(crate) fn from_fixture_id(value: &str) -> Option<Self> {
        match value {
            "directed_low" => Some(Self::DirectedLow),
            "directed_high" => Some(Self::DirectedHigh),
            "undirected_low" => Some(Self::UndirectedLow),
            "undirected_high" => Some(Self::UndirectedHigh),
            "directed_self_loop" => Some(Self::DirectedSelfLoop),
            "undirected_self_loop" => Some(Self::UndirectedSelfLoop),
            "parallel" => Some(Self::Parallel),
            "sparse_slots" => Some(Self::SparseSlots),
            "mixed_labels_low" => Some(Self::MixedLabelsLow),
            "mixed_labels_high" => Some(Self::MixedLabelsHigh),
            _ => None,
        }
    }

    fn shape(self) -> FixtureShape {
        match self {
            Self::DirectedLow | Self::DirectedHigh => FixtureShape::Directed,
            Self::UndirectedLow | Self::UndirectedHigh => FixtureShape::Undirected,
            Self::DirectedSelfLoop => FixtureShape::DirectedSelfLoop,
            Self::UndirectedSelfLoop => FixtureShape::UndirectedSelfLoop,
            Self::Parallel => FixtureShape::Parallel,
            Self::SparseSlots => FixtureShape::SparseSlots,
            Self::MixedLabelsLow | Self::MixedLabelsHigh => FixtureShape::MixedLabels,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdoptionEvidenceRow {
    pub(crate) fixture_id: AdoptionFixtureId,
    pub(crate) disposition: AdoptionDisposition,
    pub(crate) evidence_present: bool,
    pub(crate) exact_results: bool,
    pub(crate) fallback_safe: bool,
    pub(crate) logical_bytes_pass: bool,
    pub(crate) runtime_pass: bool,
}

/// Result of a matched candidate probe.  The probe owns the exactness and fallback claims;
/// callers must not manufacture an adoption row from a canbench total alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MatchedAdoptionProbe {
    pub(crate) observation: CompressionPolicyObservation,
    pub(crate) exact_results: bool,
    pub(crate) fallback_safe: bool,
}

impl MatchedAdoptionProbe {
    pub(crate) fn into_row(self, fixture_id: AdoptionFixtureId) -> AdoptionEvidenceRow {
        adoption_row_from_observation(
            fixture_id,
            self.observation,
            self.exact_results,
            self.fallback_safe,
        )
    }
}

pub(crate) fn adoption_row_from_observation(
    fixture_id: AdoptionFixtureId,
    observation: CompressionPolicyObservation,
    exact_results: bool,
    fallback_safe: bool,
) -> AdoptionEvidenceRow {
    let disposition =
        select_adoption_disposition(fixture_id.shape(), AccessProfile::Hot, Some(observation));
    let has_candidate_evidence = match fixture_id.shape() {
        FixtureShape::Undirected => {
            observation.pair_rank_bytes.is_some() && observation.pair_rank_instructions.is_some()
        }
        FixtureShape::Directed
        | FixtureShape::Parallel
        | FixtureShape::SparseSlots
        | FixtureShape::MixedLabels => {
            observation.ranked_bytes != 0
                || observation.shared_bytes.is_some()
                || observation.compressed_bytes.is_some()
        }
        FixtureShape::DirectedSelfLoop | FixtureShape::UndirectedSelfLoop => true,
    };
    let candidate_passes = match disposition {
        AdoptionDisposition::SharedOrientation => observation
            .shared_bytes
            .zip(observation.shared_instructions)
            .is_some_and(|(bytes, instructions)| {
                bytes != 0
                    && bytes <= observation.alias_bytes
                    && instructions <= observation.scan_instructions
            }),
        AdoptionDisposition::PairRank => observation
            .pair_rank_bytes
            .zip(observation.pair_rank_instructions)
            .is_some_and(|(bytes, instructions)| {
                bytes != 0
                    && bytes <= observation.alias_bytes
                    && instructions <= observation.scan_instructions
            }),
        AdoptionDisposition::RankIndexedPacked => {
            Some((observation.ranked_bytes, observation.ranked_instructions)).is_some_and(
                |(bytes, instructions)| {
                    bytes != 0
                        && bytes <= observation.alias_bytes
                        && instructions <= observation.scan_instructions
                },
            )
        }
        AdoptionDisposition::ScanOnly | AdoptionDisposition::Deferred => false,
    };
    AdoptionEvidenceRow {
        fixture_id,
        disposition,
        evidence_present: has_candidate_evidence,
        exact_results,
        fallback_safe,
        logical_bytes_pass: candidate_passes,
        runtime_pass: candidate_passes,
    }
}

const REQUIRED_ADOPTION_FIXTURE_IDS: [AdoptionFixtureId; 10] = [
    AdoptionFixtureId::DirectedLow,
    AdoptionFixtureId::DirectedHigh,
    AdoptionFixtureId::UndirectedLow,
    AdoptionFixtureId::UndirectedHigh,
    AdoptionFixtureId::DirectedSelfLoop,
    AdoptionFixtureId::UndirectedSelfLoop,
    AdoptionFixtureId::Parallel,
    AdoptionFixtureId::SparseSlots,
    AdoptionFixtureId::MixedLabelsLow,
    AdoptionFixtureId::MixedLabelsHigh,
];

pub(crate) fn aggregate_adoption_status(rows: &[AdoptionEvidenceRow]) -> AdoptionStatus {
    if rows.len() != REQUIRED_ADOPTION_FIXTURE_IDS.len()
        || REQUIRED_ADOPTION_FIXTURE_IDS.iter().any(|required| {
            rows.iter()
                .filter(|row| row.fixture_id == *required)
                .count()
                != 1
        })
    {
        return AdoptionStatus::Hold;
    }
    if rows.iter().any(|row| {
        !row.evidence_present
            || matches!(row.disposition, AdoptionDisposition::Deferred)
            || !row.exact_results
            || !row.fallback_safe
    }) {
        return AdoptionStatus::Hold;
    }
    let total = rows.len() as u8;
    let ready = rows
        .iter()
        .filter(|row| {
            row.logical_bytes_pass
                && row.runtime_pass
                && (!fixture_requires_candidate(row.fixture_id)
                    || !matches!(row.disposition, AdoptionDisposition::ScanOnly))
        })
        .count() as u8;
    if ready == total {
        AdoptionStatus::Adopt
    } else {
        AdoptionStatus::Partial { ready, total }
    }
}

const fn fixture_requires_candidate(fixture_id: AdoptionFixtureId) -> bool {
    !matches!(
        fixture_id,
        AdoptionFixtureId::DirectedLow
            | AdoptionFixtureId::UndirectedLow
            | AdoptionFixtureId::DirectedSelfLoop
            | AdoptionFixtureId::UndirectedSelfLoop
    )
}

/// Inventory of rows currently connected to the typed gate. Existing canbench output that has not
/// been converted into a matched row remains explicitly Deferred rather than being inferred.
pub(crate) fn current_adoption_evidence_inventory() -> Vec<AdoptionEvidenceRow> {
    REQUIRED_ADOPTION_FIXTURE_IDS
        .into_iter()
        .map(|fixture_id| {
            if fixture_requires_candidate(fixture_id) {
                AdoptionEvidenceRow {
                    fixture_id,
                    disposition: AdoptionDisposition::Deferred,
                    evidence_present: false,
                    exact_results: false,
                    fallback_safe: false,
                    logical_bytes_pass: false,
                    runtime_pass: false,
                }
            } else {
                AdoptionEvidenceRow {
                    fixture_id,
                    disposition: AdoptionDisposition::ScanOnly,
                    evidence_present: true,
                    exact_results: true,
                    fallback_safe: true,
                    logical_bytes_pass: true,
                    runtime_pass: true,
                }
            }
        })
        .collect()
}

pub(crate) fn current_adoption_status() -> AdoptionStatus {
    aggregate_adoption_status(&current_adoption_evidence_inventory())
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
    if inputs.live_degree < PROMOTE_MIN_LIVE_EDGES
        || matches!(inputs.access_profile, AccessProfile::Cold)
    {
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

/// Apply the topology matrix after the byte/runtime/exactness gate has been evaluated.
///
/// Low-degree, cold, and self-loop buckets have a deterministic ScanOnly fallback.  Dense
/// topologies require current evidence; absence of evidence is `Deferred`, never an implicit
/// promotion.  Undirected buckets may only select the rank-indexed candidate because the shared
/// orientation model requires directed counterpart groups.  Undirected pair-rank remains a
/// measurement-only candidate until its own maintenance gate is complete.
pub(crate) fn select_adoption_disposition(
    shape: FixtureShape,
    access_profile: AccessProfile,
    evidence: Option<CompressionPolicyObservation>,
) -> AdoptionDisposition {
    if matches!(access_profile, AccessProfile::Cold)
        || matches!(
            shape,
            FixtureShape::DirectedSelfLoop | FixtureShape::UndirectedSelfLoop
        )
    {
        return AdoptionDisposition::ScanOnly;
    }
    let Some(observation) = evidence else {
        return AdoptionDisposition::Deferred;
    };
    if observation.live_degree < PROMOTE_MIN_LIVE_EDGES {
        return AdoptionDisposition::ScanOnly;
    }
    if shape == FixtureShape::Undirected {
        let (Some(bytes), Some(instructions)) = (
            observation.pair_rank_bytes,
            observation.pair_rank_instructions,
        ) else {
            return AdoptionDisposition::Deferred;
        };
        return if observation.exact_and_fail_closed
            && bytes != 0
            && bytes <= observation.alias_bytes
            && instructions <= observation.scan_instructions
        {
            AdoptionDisposition::PairRank
        } else {
            AdoptionDisposition::ScanOnly
        };
    }
    match (shape, select_compression_candidate(observation)) {
        (FixtureShape::Directed, CompressionCandidate::SharedOrientation)
        | (FixtureShape::Parallel, CompressionCandidate::SharedOrientation)
        | (FixtureShape::SparseSlots, CompressionCandidate::SharedOrientation)
        | (FixtureShape::MixedLabels, CompressionCandidate::SharedOrientation) => {
            AdoptionDisposition::SharedOrientation
        }
        (FixtureShape::Directed, CompressionCandidate::RankedPacked)
        | (FixtureShape::Directed, CompressionCandidate::MonotoneCompressed)
        | (FixtureShape::Parallel, CompressionCandidate::RankedPacked)
        | (FixtureShape::Parallel, CompressionCandidate::MonotoneCompressed)
        | (FixtureShape::SparseSlots, CompressionCandidate::RankedPacked)
        | (FixtureShape::SparseSlots, CompressionCandidate::MonotoneCompressed)
        | (FixtureShape::MixedLabels, CompressionCandidate::RankedPacked)
        | (FixtureShape::MixedLabels, CompressionCandidate::MonotoneCompressed) => {
            AdoptionDisposition::RankIndexedPacked
        }
        _ => AdoptionDisposition::ScanOnly,
    }
}

/// Measurement-only policy inputs for Plans 0158/0160. The policy never activates a production
/// path; it records the conservative evidence gate used by the candidate benches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CompressionPolicyObservation {
    pub(crate) live_degree: u64,
    pub(crate) requests: u32,
    pub(crate) monotone_rank_sequence: bool,
    pub(crate) alias_bytes: u64,
    pub(crate) scan_instructions: u64,
    pub(crate) ranked_bytes: u64,
    pub(crate) ranked_instructions: u64,
    pub(crate) shared_bytes: Option<u64>,
    pub(crate) shared_instructions: Option<u64>,
    pub(crate) compressed_bytes: Option<u64>,
    pub(crate) compressed_instructions: Option<u64>,
    pub(crate) pair_rank_bytes: Option<u64>,
    pub(crate) pair_rank_instructions: Option<u64>,
    pub(crate) exact_and_fail_closed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompressionCandidate {
    ScanOnly,
    SharedOrientation,
    RankedPacked,
    MonotoneCompressed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AmortizationObservation {
    pub(crate) compression: CompressionPolicyObservation,
    pub(crate) shared_update_instructions: u64,
    pub(crate) ranked_update_instructions: u64,
    pub(crate) reads_per_update: u64,
}

pub(crate) fn break_even_reads(
    scan_instructions: u64,
    candidate_instructions: u64,
    update_instructions: u64,
) -> Option<u64> {
    let savings = scan_instructions.checked_sub(candidate_instructions)?;
    if savings == 0 {
        return None;
    }
    Some(update_instructions.saturating_add(savings - 1) / savings)
}

pub(crate) fn select_amortized_candidate(
    observation: AmortizationObservation,
) -> CompressionCandidate {
    let candidate = select_compression_candidate(observation.compression);
    let update_cost = |candidate| match candidate {
        CompressionCandidate::SharedOrientation => observation.shared_update_instructions,
        CompressionCandidate::RankedPacked => observation.ranked_update_instructions,
        _ => 0,
    };
    if matches!(
        candidate,
        CompressionCandidate::SharedOrientation | CompressionCandidate::RankedPacked
    ) && break_even_reads(
        observation.compression.scan_instructions,
        match candidate {
            CompressionCandidate::SharedOrientation => observation
                .compression
                .shared_instructions
                .unwrap_or(u64::MAX),
            CompressionCandidate::RankedPacked => observation.compression.ranked_instructions,
            _ => 0,
        },
        update_cost(candidate),
    )
    .is_some_and(|reads| observation.reads_per_update >= reads)
    {
        candidate
    } else {
        CompressionCandidate::ScanOnly
    }
}

pub(crate) const MIN_RANK_LIVE_DEGREE: u64 = PROMOTE_MIN_LIVE_EDGES;
pub(crate) const MIN_RANK_REQUESTS: u32 = 64;

pub(crate) const fn cardinality_allows_promotion(live_edges: u64) -> bool {
    live_edges >= PROMOTE_MIN_LIVE_EDGES
}

pub(crate) const fn cardinality_requires_demotion(live_edges: u64) -> bool {
    live_edges <= DEMOTE_MAX_LIVE_EDGES
}

pub(crate) fn select_compression_candidate(
    observation: CompressionPolicyObservation,
) -> CompressionCandidate {
    if observation.live_degree < MIN_RANK_LIVE_DEGREE
        || observation.requests < MIN_RANK_REQUESTS
        || !observation.exact_and_fail_closed
    {
        return CompressionCandidate::ScanOnly;
    }
    let ranked_ok = observation.ranked_bytes != 0
        && observation.ranked_instructions <= observation.scan_instructions
        && observation.ranked_bytes <= observation.alias_bytes;
    let shared_ok = observation
        .shared_bytes
        .is_some_and(|bytes| bytes != 0 && bytes <= observation.alias_bytes)
        && observation
            .shared_instructions
            .is_some_and(|instructions| instructions <= observation.scan_instructions);
    if observation.monotone_rank_sequence
        && ranked_ok
        && observation
            .compressed_bytes
            .is_some_and(|bytes| bytes != 0 && bytes <= observation.ranked_bytes)
        && observation
            .compressed_instructions
            .is_some_and(|instructions| instructions <= observation.ranked_instructions)
    {
        return CompressionCandidate::MonotoneCompressed;
    }
    if shared_ok
        && (!ranked_ok
            || (observation.shared_bytes <= Some(observation.ranked_bytes)
                && observation.shared_instructions <= Some(observation.ranked_instructions)))
    {
        return CompressionCandidate::SharedOrientation;
    }
    if ranked_ok {
        CompressionCandidate::RankedPacked
    } else {
        CompressionCandidate::ScanOnly
    }
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

#[cfg(feature = "canbench")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct ByteFootprintRow {
    shape_id: &'static str,
    logical_edges: u64,
    alias_raw_bytes: u64,
    sampled_known_bytes: u64,
    packed_known_bytes: u64,
    published_blob_bytes: Option<u64>,
}

#[cfg(feature = "canbench")]
fn alias_raw_bytes_for_shape(spec: FixtureSpec) -> u64 {
    if matches!(
        spec.shape,
        FixtureShape::DirectedSelfLoop | FixtureShape::UndirectedSelfLoop
    ) {
        0
    } else {
        spec.logical_edges.saturating_mul(18)
    }
}

#[cfg(feature = "canbench")]
fn published_blob_bytes_for_shape(spec: FixtureSpec) -> Option<u64> {
    match spec.shape {
        FixtureShape::Directed if spec.logical_edges == 128 => {
            let vertex_count = 64u32;
            let edges = (0..vertex_count)
                .flat_map(|source| {
                    [
                        (source, (source + 1) % vertex_count),
                        (source, (source + 2) % vertex_count),
                    ]
                })
                .collect::<Vec<_>>();
            ic_stable_lara::adoption_fixture::build_published_fixture(vertex_count, &edges)
                .ok()
                .and_then(|fixture| fixture.graph.published_mate_blob_bytes().ok())
        }
        FixtureShape::Parallel if spec.logical_edges == 32 => {
            let edges = (0..32).map(|_| (0, 1)).collect::<Vec<_>>();
            ic_stable_lara::adoption_fixture::build_published_fixture(2, &edges)
                .ok()
                .and_then(|fixture| fixture.graph.published_mate_blob_bytes().ok())
        }
        FixtureShape::Undirected if spec.logical_edges == 128 => {
            let vertex_count = 64u32;
            let edges = (0..vertex_count)
                .flat_map(|source| {
                    [
                        (source, (source + 1) % vertex_count),
                        (source, (source + 2) % vertex_count),
                    ]
                })
                .collect::<Vec<_>>();
            ic_stable_lara::adoption_fixture::build_published_undirected_fixture(
                vertex_count,
                &edges,
            )
            .ok()
            .and_then(|fixture| fixture.graph.published_mate_blob_bytes().ok())
        }
        _ => None,
    }
}

#[cfg(feature = "canbench")]
fn byte_footprint_report() -> Vec<ByteFootprintRow> {
    FixtureSpec::required_matrix()
        .into_iter()
        .map(|spec| {
            let sampled = MateFootprintInput {
                entries: spec.physical_half_edges,
                mode: MateMode::Sampled { stride: 32 },
                shared: MateSharedOverhead::zero(),
            }
            .calculate_with_geometry(
                spec.physical_half_edges,
                spec.physical_half_edges,
                spec.physical_half_edges,
            )
            .expect("sampled footprint");
            let packed = MateFootprintInput {
                entries: spec.physical_half_edges,
                mode: MateMode::Packed { width_bytes: 1 },
                shared: MateSharedOverhead::zero(),
            }
            .calculate_with_geometry(
                spec.physical_half_edges,
                spec.physical_half_edges,
                spec.physical_half_edges,
            )
            .expect("packed footprint");
            ByteFootprintRow {
                shape_id: spec.id,
                logical_edges: spec.logical_edges,
                alias_raw_bytes: alias_raw_bytes_for_shape(spec),
                sampled_known_bytes: sampled.known_logical_bytes,
                packed_known_bytes: packed.known_logical_bytes,
                published_blob_bytes: published_blob_bytes_for_shape(spec),
            }
        })
        .collect()
}

#[cfg(feature = "canbench")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct SizeSeriesRow {
    topology: &'static str,
    logical_edges: u64,
    bucket_count: u64,
    alias_raw_bytes: u64,
    published_blob_bytes: u64,
    published_mate_storage_pages: u64,
}

#[cfg(feature = "canbench")]
fn published_size_series() -> Vec<SizeSeriesRow> {
    let mut rows = Vec::new();
    for vertex_count in [16u32, 32, 64, 128] {
        let edges = (0..vertex_count)
            .flat_map(|source| {
                [
                    (source, (source + 1) % vertex_count),
                    (source, (source + 2) % vertex_count),
                ]
            })
            .collect::<Vec<_>>();
        if let Ok(fixture) =
            ic_stable_lara::adoption_fixture::build_published_fixture(vertex_count, &edges)
        {
            rows.push(SizeSeriesRow {
                topology: "directed",
                logical_edges: edges.len() as u64,
                bucket_count: fixture
                    .graph
                    .published_mate_blob_count()
                    .expect("directed blob count"),
                alias_raw_bytes: (edges.len() as u64).saturating_mul(18),
                published_blob_bytes: fixture
                    .graph
                    .published_mate_blob_bytes()
                    .expect("directed blob bytes"),
                published_mate_storage_pages: fixture.graph.published_mate_storage_pages(),
            });
        }
        if let Ok(fixture) = ic_stable_lara::adoption_fixture::build_published_undirected_fixture(
            vertex_count,
            &edges,
        ) {
            rows.push(SizeSeriesRow {
                topology: "undirected",
                logical_edges: edges.len() as u64,
                bucket_count: fixture
                    .graph
                    .published_mate_blob_count()
                    .expect("undirected blob count"),
                alias_raw_bytes: (edges.len() as u64).saturating_mul(18),
                published_blob_bytes: fixture
                    .graph
                    .published_mate_blob_bytes()
                    .expect("undirected blob bytes"),
                published_mate_storage_pages: fixture.graph.published_mate_storage_pages(),
            });
        }
    }
    for edge_count in [32usize, 64, 128, 256] {
        let edges = (0..edge_count).map(|_| (0, 1)).collect::<Vec<_>>();
        if let Ok(fixture) = ic_stable_lara::adoption_fixture::build_published_fixture(2, &edges) {
            rows.push(SizeSeriesRow {
                topology: "parallel",
                logical_edges: edge_count as u64,
                bucket_count: fixture
                    .graph
                    .published_mate_blob_count()
                    .expect("parallel blob count"),
                alias_raw_bytes: (edge_count as u64).saturating_mul(18),
                published_blob_bytes: fixture
                    .graph
                    .published_mate_blob_bytes()
                    .expect("parallel blob bytes"),
                published_mate_storage_pages: fixture.graph.published_mate_storage_pages(),
            });
        }
    }
    rows
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
    fn cardinality_hysteresis_has_strict_admission_boundaries() {
        assert!(!cardinality_allows_promotion(PROMOTE_MIN_LIVE_EDGES - 1));
        assert!(cardinality_allows_promotion(PROMOTE_MIN_LIVE_EDGES));
        assert!(cardinality_requires_demotion(DEMOTE_MAX_LIVE_EDGES));
        assert!(!cardinality_requires_demotion(DEMOTE_MAX_LIVE_EDGES + 1));
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

/// Exact logical byte length of the canonical identity envelope used by the evidence schema.
pub(crate) fn canonical_identity_encoded_bytes(rows: &[CanonicalIdentity]) -> u64 {
    let mut ordered = rows.to_vec();
    ordered.sort();
    ordered.iter().fold(5u64, |total, row| {
        total
            .saturating_add(4)
            .saturating_add(row.row_bytes().len() as u64)
    })
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
        let label = if matches!(spec.shape, FixtureShape::MixedLabels) {
            shape_tag(spec.shape) + (index as u16 % 2)
        } else {
            shape_tag(spec.shape)
        };
        identities.push(CanonicalIdentity {
            owner,
            target,
            orientation: (index % 2) as u8,
            label,
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

#[cfg(feature = "canbench")]
fn deterministic_fixture_from_physical_canonical(
    spec: FixtureSpec,
    representation: ic_stable_lara::adoption_fixture::FixtureRepresentation,
    mut identities: Vec<CanonicalIdentity>,
) -> DeterministicFixture {
    identities.sort();
    let descriptor = ShapeDescriptor {
        shape_id: spec.id.to_owned(),
        shape_definition_digest: shape_definition_digest(spec),
        fixture_ids: vec![format!(
            "{}-{}",
            spec.id,
            match representation {
                ic_stable_lara::adoption_fixture::FixtureRepresentation::AliasOnly => "alias-only",
                ic_stable_lara::adoption_fixture::FixtureRepresentation::ScanOnly => "scan-only",
                ic_stable_lara::adoption_fixture::FixtureRepresentation::Published => "published",
            }
        )],
        logical_edges: spec.logical_edges,
        physical_half_edges: identities.len() as u64,
        alias_rows: identities.len() as u64,
        indexed_half_edges: identities.len() as u64,
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
    build_real_adjacency_fixture(
        spec,
        ic_stable_lara::adoption_fixture::FixtureRepresentation::AliasOnly,
    )
}

/// Build a real ScanOnly identity fixture from canonical adjacency without mate metadata.
#[cfg(feature = "canbench")]
pub(crate) fn build_real_scan_fixture(spec: FixtureSpec) -> Result<DeterministicFixture, String> {
    build_real_adjacency_fixture(
        spec,
        ic_stable_lara::adoption_fixture::FixtureRepresentation::ScanOnly,
    )
}

#[cfg(feature = "canbench")]
fn build_real_sparse_fixture(spec: FixtureSpec) -> Result<DeterministicFixture, String> {
    if spec.shape != FixtureShape::SparseSlots {
        return Err("sparse fixture requested for a non-sparse shape".to_owned());
    }
    let fixture = ic_stable_lara::adoption_fixture::build_sparse_slot_published_fixture(64)?;
    Ok(deterministic_fixture_from_physical(
        spec,
        ic_stable_lara::adoption_fixture::FixtureRepresentation::Published,
        fixture.identities,
    ))
}

#[cfg(feature = "canbench")]
fn build_real_mixed_label_fixture(spec: FixtureSpec) -> Result<DeterministicFixture, String> {
    if spec.shape != FixtureShape::MixedLabels || !spec.logical_edges.is_multiple_of(2) {
        return Err("mixed-label fixture requested for an incompatible shape".to_owned());
    }
    let fixture = ic_stable_lara::adoption_fixture::build_mixed_label_published_fixture(
        2,
        u32::try_from(spec.logical_edges / 2)
            .map_err(|_| "mixed-label edge count overflow".to_owned())?,
    )?;
    let identities = fixture
        .identities
        .into_iter()
        .map(|identity| {
            let mut seed = Vec::new();
            seed.extend_from_slice(&identity.owner.to_be_bytes());
            seed.extend_from_slice(&identity.target.to_be_bytes());
            seed.extend_from_slice(&identity.label.to_be_bytes());
            seed.push(identity.orientation);
            seed.extend_from_slice(&identity.slot.to_be_bytes());
            CanonicalIdentity {
                owner: identity.owner,
                target: identity.target,
                orientation: identity.orientation,
                label: identity.label,
                slot: identity.slot,
                inline_payload_fingerprint: digest_hex(&seed),
                payload_bytes: Vec::new(),
            }
        })
        .collect();
    Ok(deterministic_fixture_from_physical_canonical(
        spec,
        ic_stable_lara::adoption_fixture::FixtureRepresentation::Published,
        identities,
    ))
}

/// Build a real Published identity fixture for promotion-eligible directed/parallel shapes.
#[cfg(feature = "canbench")]
pub(crate) fn build_real_published_fixture(
    spec: FixtureSpec,
) -> Result<DeterministicFixture, String> {
    let (vertex_count, edges) = match spec.shape {
        FixtureShape::Directed if spec.logical_edges == 128 => {
            let vertex_count = 64u32;
            let mut edges = Vec::with_capacity(128);
            for source in 0..vertex_count {
                edges.push((source, (source + 1) % vertex_count));
                edges.push((source, (source + 2) % vertex_count));
            }
            (vertex_count, edges)
        }
        FixtureShape::Parallel if spec.logical_edges == 32 => {
            (2, (0..32).map(|_| (0, 1)).collect::<Vec<_>>())
        }
        FixtureShape::MixedLabels if spec.logical_edges.is_multiple_of(2) => {
            return Err("mixed-label fixture uses the labeled owner directly".to_owned());
        }
        FixtureShape::Undirected if spec.logical_edges == 128 => {
            let vertex_count = 64u32;
            let edges = (0..vertex_count)
                .flat_map(|source| {
                    [
                        (source, (source + 1) % vertex_count),
                        (source, (source + 2) % vertex_count),
                    ]
                })
                .collect::<Vec<_>>();
            let fixture = ic_stable_lara::adoption_fixture::build_published_undirected_fixture(
                vertex_count,
                &edges,
            )?;
            return Ok(deterministic_fixture_from_physical(
                spec,
                ic_stable_lara::adoption_fixture::FixtureRepresentation::Published,
                fixture.identities,
            ));
        }
        _ => {
            return Err("Published fixture requires a promotion-eligible topology".to_owned());
        }
    };
    let fixture = ic_stable_lara::adoption_fixture::build_published_fixture(vertex_count, &edges)?;
    Ok(deterministic_fixture_from_physical(
        spec,
        ic_stable_lara::adoption_fixture::FixtureRepresentation::Published,
        fixture.identities,
    ))
}

#[cfg(feature = "canbench")]
fn build_real_adjacency_fixture(
    spec: FixtureSpec,
    representation: ic_stable_lara::adoption_fixture::FixtureRepresentation,
) -> Result<DeterministicFixture, String> {
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
            match representation {
                ic_stable_lara::adoption_fixture::FixtureRepresentation::AliasOnly => {
                    ic_stable_lara::adoption_fixture::build_alias_only_fixture(
                        vertex_count,
                        &edges,
                    )?
                    .identities
                }
                ic_stable_lara::adoption_fixture::FixtureRepresentation::ScanOnly => {
                    ic_stable_lara::adoption_fixture::build_scan_only_fixture(vertex_count, &edges)?
                        .identities
                }
                ic_stable_lara::adoption_fixture::FixtureRepresentation::Published => {
                    return Err("Published fixture owner is not wired".to_owned());
                }
            }
        }
        FixtureShape::Parallel => {
            let edges = (0..spec.logical_edges).map(|_| (0, 1)).collect::<Vec<_>>();
            match representation {
                ic_stable_lara::adoption_fixture::FixtureRepresentation::AliasOnly => {
                    ic_stable_lara::adoption_fixture::build_alias_only_fixture(2, &edges)?
                        .identities
                }
                ic_stable_lara::adoption_fixture::FixtureRepresentation::ScanOnly => {
                    ic_stable_lara::adoption_fixture::build_scan_only_fixture(2, &edges)?.identities
                }
                ic_stable_lara::adoption_fixture::FixtureRepresentation::Published => {
                    return Err("Published fixture owner is not wired".to_owned());
                }
            }
        }
        FixtureShape::DirectedSelfLoop => {
            let edges = [(0, 0)];
            match representation {
                ic_stable_lara::adoption_fixture::FixtureRepresentation::AliasOnly => {
                    ic_stable_lara::adoption_fixture::build_alias_only_fixture(1, &edges)?
                        .identities
                }
                ic_stable_lara::adoption_fixture::FixtureRepresentation::ScanOnly => {
                    ic_stable_lara::adoption_fixture::build_scan_only_fixture(1, &edges)?.identities
                }
                ic_stable_lara::adoption_fixture::FixtureRepresentation::Published => {
                    return Err("Published directed self-loop fixture is not wired".to_owned());
                }
            }
        }
        FixtureShape::Undirected => {
            let edges = (0..spec.logical_edges)
                .map(|index| {
                    u32::try_from(index + 1)
                        .map(|target| (0, target))
                        .map_err(|_| "real AliasOnly fixture endpoint overflow".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            match representation {
                ic_stable_lara::adoption_fixture::FixtureRepresentation::AliasOnly => {
                    ic_stable_lara::adoption_fixture::build_alias_only_undirected_fixture(
                        vertex_count,
                        &edges,
                    )?
                    .identities
                }
                ic_stable_lara::adoption_fixture::FixtureRepresentation::ScanOnly => {
                    ic_stable_lara::adoption_fixture::build_scan_only_undirected_fixture(
                        vertex_count,
                        &edges,
                    )?
                    .identities
                }
                ic_stable_lara::adoption_fixture::FixtureRepresentation::Published => {
                    return Err("Published fixture owner is not wired".to_owned());
                }
            }
        }
        FixtureShape::UndirectedSelfLoop => match representation {
            ic_stable_lara::adoption_fixture::FixtureRepresentation::AliasOnly => {
                ic_stable_lara::adoption_fixture::build_alias_only_undirected_fixture(1, &[(0, 0)])?
                    .identities
            }
            ic_stable_lara::adoption_fixture::FixtureRepresentation::ScanOnly => {
                ic_stable_lara::adoption_fixture::build_scan_only_undirected_fixture(1, &[(0, 0)])?
                    .identities
            }
            ic_stable_lara::adoption_fixture::FixtureRepresentation::Published => {
                return Err("Published fixture owner is not wired".to_owned());
            }
        },
        _ => {
            return Err(
                "real AliasOnly adapter currently supports directed/parallel/undirected shapes only"
                    .to_owned(),
            );
        }
    };
    Ok(deterministic_fixture_from_physical(
        spec,
        representation,
        physical_identities,
    ))
}

#[cfg(feature = "canbench")]
fn deterministic_fixture_from_physical(
    spec: FixtureSpec,
    representation: ic_stable_lara::adoption_fixture::FixtureRepresentation,
    physical_identities: Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>,
) -> DeterministicFixture {
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
        fixture_ids: vec![format!(
            "{}-{}",
            spec.id,
            match representation {
                ic_stable_lara::adoption_fixture::FixtureRepresentation::AliasOnly => "alias-only",
                ic_stable_lara::adoption_fixture::FixtureRepresentation::ScanOnly => "scan-only",
                ic_stable_lara::adoption_fixture::FixtureRepresentation::Published => "published",
            }
        )],
        logical_edges: spec.logical_edges,
        physical_half_edges: identities.len() as u64,
        alias_rows: identities.len() as u64,
        indexed_half_edges: identities.len() as u64,
    };
    DeterministicFixture {
        descriptor,
        identities,
    }
}

/// Select the owning-layer fixture for evidence generation. Unsupported shapes retain their
/// descriptor but deliberately omit the identity digest instead of presenting synthetic rows as
/// real AliasOnly measurements.
fn build_evidence_fixture(spec: FixtureSpec) -> (ShapeDescriptor, Option<String>, Option<u64>) {
    #[cfg(feature = "canbench")]
    if spec.shape == FixtureShape::SparseSlots
        && let Ok(fixture) = build_real_sparse_fixture(spec)
    {
        return (
            fixture.descriptor,
            Some(canonical_identity_digest(&fixture.identities)),
            Some(canonical_identity_encoded_bytes(&fixture.identities)),
        );
    }
    #[cfg(feature = "canbench")]
    if spec.shape == FixtureShape::MixedLabels
        && let Ok(fixture) = build_real_mixed_label_fixture(spec)
    {
        return (
            fixture.descriptor,
            Some(canonical_identity_digest(&fixture.identities)),
            Some(canonical_identity_encoded_bytes(&fixture.identities)),
        );
    }
    #[cfg(feature = "canbench")]
    if let Ok(fixture) = build_real_alias_fixture(spec) {
        return (
            fixture.descriptor,
            Some(canonical_identity_digest(&fixture.identities)),
            Some(canonical_identity_encoded_bytes(&fixture.identities)),
        );
    }

    let synthetic = build_fixture(spec);
    (synthetic.descriptor, None, None)
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
    pub(crate) canonical_identity_bytes: Option<u64>,
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
            if row.canonical_identity_bytes == Some(0) {
                return Err("invalid identity byte length");
            }
            if row.canonical_identity_digest.is_some() != row.canonical_identity_bytes.is_some() {
                return Err("identity digest/byte length mismatch");
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
    let mut descriptors = Vec::new();
    let mut rows = Vec::new();
    let mut corpus_generated_count = 0u32;
    for spec in FixtureSpec::required_matrix() {
        let (mut descriptor, alias_digest, alias_bytes) = build_evidence_fixture(spec);
        let alias_fixture_id = descriptor.fixture_ids[0].clone();
        let mut candidate_rows = vec![(alias_fixture_id, alias_digest, alias_bytes)];
        #[cfg(feature = "canbench")]
        if let Ok(scan) = build_real_scan_fixture(spec) {
            candidate_rows.push((
                scan.descriptor.fixture_ids[0].clone(),
                Some(canonical_identity_digest(&scan.identities)),
                Some(canonical_identity_encoded_bytes(&scan.identities)),
            ));
        }
        #[cfg(feature = "canbench")]
        if let Ok(published) = build_real_published_fixture(spec) {
            candidate_rows.push((
                published.descriptor.fixture_ids[0].clone(),
                Some(canonical_identity_digest(&published.identities)),
                Some(canonical_identity_encoded_bytes(&published.identities)),
            ));
        }
        candidate_rows.sort_by(|left, right| left.0.cmp(&right.0));
        descriptor.fixture_ids = candidate_rows
            .iter()
            .map(|(fixture_id, _, _)| fixture_id.clone())
            .collect();
        descriptors.push(descriptor.clone());
        rows.extend(
            candidate_rows
                .into_iter()
                .map(|(fixture_id, digest, identity_bytes)| EvidenceRow {
                    shape_id: descriptor.shape_id.clone(),
                    fixture_id,
                    status: EvidenceStatus::Deferred,
                    policy_version: POLICY_VERSION.to_owned(),
                    canonical_identity_digest: digest,
                    canonical_identity_bytes: identity_bytes,
                    request_identity: None,
                    instruction_total: None,
                    exact_result_status: None,
                }),
        );
        corpus_generated_count = corpus_generated_count
            .saturating_add(build_request_corpus(spec, 0x0048_0146).len() as u32);
    }
    descriptors.sort_by(|left, right| left.shape_id.cmp(&right.shape_id));
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

#[cfg(feature = "canbench")]
macro_rules! fixture_setup_bench {
    ($name:ident, $builder:ident, $spec_index:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let spec = FixtureSpec::required_matrix()[$spec_index];
            bench_fn(|| {
                let fixture = $builder(spec).expect("adoption fixture setup");
                black_box(fixture.identities.len());
            })
        }
    };
}

fixture_setup_bench!(
    bench_mate_adoption_alias_directed_high,
    build_real_alias_fixture,
    1
);
fixture_setup_bench!(
    bench_mate_adoption_scan_directed_high,
    build_real_scan_fixture,
    1
);
fixture_setup_bench!(
    bench_mate_adoption_published_directed_high,
    build_real_published_fixture,
    1
);
fixture_setup_bench!(
    bench_mate_adoption_published_parallel,
    build_real_published_fixture,
    6
);
fixture_setup_bench!(
    bench_mate_adoption_published_undirected_high,
    build_real_published_fixture,
    3
);

#[cfg(feature = "canbench")]
fn bench_ranked_bytes(spec: FixtureSpec, undirected: bool) -> canbench_rs::BenchResult {
    let (vertex_count, edges) = if undirected {
        let vertex_count = (spec.logical_edges / 2) as u32;
        let edges = (0..vertex_count)
            .flat_map(|source| {
                [
                    (source, (source + 1) % vertex_count),
                    (source, (source + 2) % vertex_count),
                ]
            })
            .collect::<Vec<_>>();
        (vertex_count, edges)
    } else if matches!(spec.shape, FixtureShape::Parallel) {
        (
            2,
            (0..spec.logical_edges).map(|_| (0, 1)).collect::<Vec<_>>(),
        )
    } else {
        let vertex_count = (spec.logical_edges / 2) as u32;
        let edges = (0..vertex_count)
            .flat_map(|source| {
                [
                    (source, (source + 1) % vertex_count),
                    (source, (source + 2) % vertex_count),
                ]
            })
            .collect::<Vec<_>>();
        (vertex_count, edges)
    };
    let fixture = if undirected {
        ic_stable_lara::adoption_fixture::build_alias_only_undirected_fixture(vertex_count, &edges)
            .expect("ranked byte fixture")
    } else {
        ic_stable_lara::adoption_fixture::build_alias_only_fixture(vertex_count, &edges)
            .expect("ranked byte fixture")
    };
    bench_fn(|| {
        let bytes = ic_stable_lara::adoption_fixture::ranked_packed_blob_bytes(
            &fixture.identities,
            undirected,
        )
        .expect("ranked bytes");
        black_box(bytes);
    })
}

#[cfg(feature = "canbench")]
fn compression_sequences_for_spec(
    spec: FixtureSpec,
    undirected: bool,
) -> (
    Vec<Vec<u32>>,
    Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>,
) {
    let (vertex_count, edges) = if undirected {
        let vertex_count = (spec.logical_edges / 2) as u32;
        let edges = (0..vertex_count)
            .flat_map(|source| {
                [
                    (source, (source + 1) % vertex_count),
                    (source, (source + 2) % vertex_count),
                ]
            })
            .collect::<Vec<_>>();
        (vertex_count, edges)
    } else if matches!(spec.shape, FixtureShape::Parallel) {
        (
            2,
            (0..spec.logical_edges).map(|_| (0, 1)).collect::<Vec<_>>(),
        )
    } else {
        let vertex_count = (spec.logical_edges / 2) as u32;
        let edges = (0..vertex_count)
            .flat_map(|source| {
                [
                    (source, (source + 1) % vertex_count),
                    (source, (source + 2) % vertex_count),
                ]
            })
            .collect::<Vec<_>>();
        (vertex_count, edges)
    };
    let fixture = if undirected {
        ic_stable_lara::adoption_fixture::build_alias_only_undirected_fixture(vertex_count, &edges)
            .expect("compression fixture")
    } else {
        ic_stable_lara::adoption_fixture::build_alias_only_fixture(vertex_count, &edges)
            .expect("compression fixture")
    };
    (
        mate_slot_sequences(&fixture.identities, undirected).expect("mate sequences"),
        fixture.identities,
    )
}

#[cfg(feature = "canbench")]
fn synthetic_sparse_slot_identities() -> Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity> {
    (0..32u32)
        .flat_map(|rank| {
            let slot = rank.saturating_mul(1024);
            [
                ic_stable_lara::adoption_fixture::PhysicalIdentity {
                    owner: 1,
                    target: 2,
                    orientation: 0,
                    slot,
                },
                ic_stable_lara::adoption_fixture::PhysicalIdentity {
                    owner: 2,
                    target: 1,
                    orientation: 1,
                    slot: (31 - rank).saturating_mul(1024),
                },
            ]
        })
        .collect()
}

#[cfg(feature = "canbench")]
fn synthetic_mixed_label_identity_sets()
-> [Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>; 2] {
    [
        (0..16u32)
            .flat_map(|rank| {
                [
                    ic_stable_lara::adoption_fixture::PhysicalIdentity {
                        owner: 1,
                        target: 2,
                        orientation: 0,
                        slot: rank,
                    },
                    ic_stable_lara::adoption_fixture::PhysicalIdentity {
                        owner: 2,
                        target: 1,
                        orientation: 1,
                        slot: 15 - rank,
                    },
                ]
            })
            .collect(),
        (0..16u32)
            .flat_map(|rank| {
                [
                    ic_stable_lara::adoption_fixture::PhysicalIdentity {
                        owner: 1,
                        target: 3,
                        orientation: 0,
                        slot: rank,
                    },
                    ic_stable_lara::adoption_fixture::PhysicalIdentity {
                        owner: 3,
                        target: 1,
                        orientation: 1,
                        slot: 15 - rank,
                    },
                ]
            })
            .collect(),
    ]
}

#[cfg(feature = "canbench")]
fn real_mixed_label_identity_sets_for_bench(
    edges_per_label: u32,
) -> Vec<Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>> {
    let fixture =
        ic_stable_lara::adoption_fixture::build_mixed_label_published_fixture(2, edges_per_label)
            .expect("real mixed-label fixture");
    let labels = fixture
        .identities
        .iter()
        .map(|identity| identity.label)
        .collect::<std::collections::BTreeSet<_>>();
    labels
        .into_iter()
        .map(|label| {
            fixture
                .identities
                .iter()
                .filter(|identity| identity.label == label)
                .map(
                    |identity| ic_stable_lara::adoption_fixture::PhysicalIdentity {
                        owner: identity.owner,
                        target: identity.target,
                        orientation: identity.orientation,
                        slot: identity.slot,
                    },
                )
                .collect()
        })
        .collect()
}

#[cfg(feature = "canbench")]
fn real_sparse_slot_identities_for_bench() -> Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>
{
    ic_stable_lara::adoption_fixture::build_sparse_slot_published_fixture(64)
        .expect("real sparse-slot fixture")
        .identities
}

#[cfg(feature = "canbench")]
fn real_sparse_scan_refs_for_bench() -> (
    ic_stable_lara::adoption_fixture::SparseSlotPublishedFixture,
    Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
) {
    let fixture = ic_stable_lara::adoption_fixture::build_sparse_slot_published_fixture(64)
        .expect("real sparse-slot fixture");
    let label = ic_stable_lara::labeled::BucketLabelKey::directed_from_index(1);
    let mut refs = Vec::new();
    fixture
        .graph
        .forward()
        .for_each_live_edge_slot_for_label(ic_stable_lara::VertexId::from(0), label, |slot, _| {
            refs.push(ic_stable_lara::labeled::CanonicalEdgeOccurrence {
                orientation: ic_stable_lara::labeled::LabeledOrientation::Forward,
                owner_vertex_id: ic_stable_lara::VertexId::from(0),
                label_id: label,
                slot_index: slot.into(),
            })
        })
        .expect("sparse forward scan");
    fixture
        .graph
        .reverse()
        .for_each_live_edge_slot_for_label(ic_stable_lara::VertexId::from(1), label, |slot, _| {
            refs.push(ic_stable_lara::labeled::CanonicalEdgeOccurrence {
                orientation: ic_stable_lara::labeled::LabeledOrientation::Reverse,
                owner_vertex_id: ic_stable_lara::VertexId::from(1),
                label_id: label,
                slot_index: slot.into(),
            })
        })
        .expect("sparse reverse scan");
    (fixture, refs)
}

#[cfg(feature = "canbench")]
fn real_mixed_scan_refs_for_bench(
    edges_per_label: u32,
) -> (
    ic_stable_lara::adoption_fixture::MixedLabelPublishedFixture,
    Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
) {
    let fixture =
        ic_stable_lara::adoption_fixture::build_mixed_label_published_fixture(2, edges_per_label)
            .expect("real mixed-label fixture");
    let mut refs = Vec::new();
    for label_index in 1..=2u32 {
        let label =
            ic_stable_lara::labeled::BucketLabelKey::directed_from_index(label_index as u16);
        fixture
            .graph
            .forward()
            .for_each_live_edge_slot_for_label(
                ic_stable_lara::VertexId::from(0),
                label,
                |slot, _| {
                    refs.push(ic_stable_lara::labeled::CanonicalEdgeOccurrence {
                        orientation: ic_stable_lara::labeled::LabeledOrientation::Forward,
                        owner_vertex_id: ic_stable_lara::VertexId::from(0),
                        label_id: label,
                        slot_index: slot.into(),
                    })
                },
            )
            .expect("mixed forward scan");
    }
    (fixture, refs)
}

#[cfg(feature = "canbench")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaintenanceTraceOp {
    Insert,
    Delete,
    Reorder,
}

#[cfg(feature = "canbench")]
fn mutate_identity_trace(
    base: &[ic_stable_lara::adoption_fixture::PhysicalIdentity],
    op: MaintenanceTraceOp,
) -> Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity> {
    let mut identities = base.to_vec();
    match op {
        MaintenanceTraceOp::Insert => {
            let forward_max = identities
                .iter()
                .filter(|identity| identity.orientation == 0)
                .map(|identity| identity.slot)
                .max()
                .expect("forward identity");
            let reverse_max = identities
                .iter()
                .filter(|identity| identity.orientation == 1)
                .map(|identity| identity.slot)
                .max()
                .expect("reverse identity");
            identities.extend([
                ic_stable_lara::adoption_fixture::PhysicalIdentity {
                    owner: 0,
                    target: 1,
                    orientation: 0,
                    slot: forward_max.saturating_add(1),
                },
                ic_stable_lara::adoption_fixture::PhysicalIdentity {
                    owner: 1,
                    target: 0,
                    orientation: 1,
                    slot: reverse_max.saturating_add(1),
                },
            ]);
        }
        MaintenanceTraceOp::Delete => {
            let forward = identities
                .iter()
                .find(|identity| identity.orientation == 0)
                .copied()
                .expect("forward identity");
            let reverse = identities
                .iter()
                .find(|identity| identity.orientation == 1)
                .copied()
                .expect("reverse identity");
            identities.retain(|identity| *identity != forward && *identity != reverse);
        }
        MaintenanceTraceOp::Reorder => identities.reverse(),
    }
    identities.sort();
    identities
}

#[cfg(feature = "canbench")]
fn sparse_maintenance_trace() -> Vec<Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>> {
    let base = real_sparse_slot_identities_for_bench();
    [
        MaintenanceTraceOp::Insert,
        MaintenanceTraceOp::Delete,
        MaintenanceTraceOp::Reorder,
    ]
    .into_iter()
    .map(|op| mutate_identity_trace(&base, op))
    .collect()
}

#[cfg(feature = "canbench")]
fn mixed_maintenance_trace() -> Vec<Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>> {
    let fixture = ic_stable_lara::adoption_fixture::build_mixed_label_published_fixture(2, 4)
        .expect("real mixed-label fixture");
    let label = fixture
        .identities
        .iter()
        .map(|identity| identity.label)
        .min()
        .expect("mixed label");
    let base = fixture
        .identities
        .iter()
        .filter(|identity| identity.label == label)
        .map(
            |identity| ic_stable_lara::adoption_fixture::PhysicalIdentity {
                owner: identity.owner,
                target: identity.target,
                orientation: identity.orientation,
                slot: identity.slot,
            },
        )
        .collect::<Vec<_>>();
    [
        MaintenanceTraceOp::Insert,
        MaintenanceTraceOp::Delete,
        MaintenanceTraceOp::Reorder,
    ]
    .into_iter()
    .map(|op| mutate_identity_trace(&base, op))
    .collect()
}

#[cfg(feature = "canbench")]
fn maintenance_rebuild_checksum(
    traces: &[Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>],
) -> u64 {
    traces.iter().fold(0u64, |checksum, identities| {
        let shared = SharedOrientationLookup::build(identities, false);
        let shared_bytes = match shared.as_ref() {
            Ok(lookup) => lookup.encode().map_or(0, |bytes| bytes.len() as u64),
            Err(_) => 0,
        };
        let shared_invalid = shared.is_err();
        let ranked_bytes = ic_stable_lara::adoption_fixture::ranked_packed_blob(identities, false)
            .map_or(0, |bytes| bytes.len() as u64);
        checksum
            .wrapping_add(shared_bytes)
            .wrapping_add(ranked_bytes)
            .wrapping_add(u64::from(shared_invalid))
    })
}

#[cfg(feature = "canbench")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaintenanceCandidate {
    Shared,
    Ranked,
}

#[cfg(feature = "canbench")]
fn maintenance_candidate_checksum(
    traces: &[Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>],
    candidate: MaintenanceCandidate,
) -> u64 {
    traces.iter().fold(0u64, |checksum, identities| {
        let bytes = match candidate {
            MaintenanceCandidate::Shared => {
                SharedOrientationLookup::build(identities, false).and_then(|lookup| lookup.encode())
            }
            MaintenanceCandidate::Ranked => {
                ic_stable_lara::adoption_fixture::ranked_packed_blob(identities, false)
            }
        };
        checksum.wrapping_add(bytes.map_or(0, |value| value.len() as u64))
    })
}

#[cfg(feature = "canbench")]
fn identity_trace_digest(
    identities: &[ic_stable_lara::adoption_fixture::PhysicalIdentity],
) -> String {
    let mut bytes = Vec::with_capacity(identities.len().saturating_mul(13));
    for identity in identities {
        bytes.extend_from_slice(&identity.owner.to_be_bytes());
        bytes.extend_from_slice(&identity.target.to_be_bytes());
        bytes.push(identity.orientation);
        bytes.extend_from_slice(&identity.slot.to_be_bytes());
    }
    digest_hex(&bytes)
}

#[cfg(feature = "canbench")]
fn stale_detection_checksum(
    baseline: &[ic_stable_lara::adoption_fixture::PhysicalIdentity],
    traces: &[Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>],
) -> u64 {
    let baseline_digest = identity_trace_digest(baseline);
    traces
        .iter()
        .filter(|trace| identity_trace_digest(trace) != baseline_digest)
        .count() as u64
}

#[cfg(feature = "canbench")]
fn amortized_read_savings(
    scan_instructions: u64,
    candidate_instructions: u64,
    maintenance_instructions: u64,
    reads_per_update: u64,
) -> i128 {
    i128::from(scan_instructions.saturating_sub(candidate_instructions))
        .saturating_mul(i128::from(reads_per_update))
        .saturating_sub(i128::from(maintenance_instructions))
}

#[cfg(feature = "canbench")]
fn extract_sparse_identities(
    graph: &ic_stable_lara::labeled::DeferredBidirectionalLabeledLaraGraph<
        ic_stable_lara::adoption_fixture::PublishedEdge,
        ic_stable_lara::adoption_fixture::FixtureMemory,
    >,
) -> Result<Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>, String> {
    let label = ic_stable_lara::labeled::BucketLabelKey::directed_from_index(1);
    let mut identities = Vec::new();
    graph
        .forward()
        .for_each_live_physical_edge_location_for_label(
            ic_stable_lara::VertexId::from(0),
            label,
            |slot, edge| {
                identities.push(ic_stable_lara::adoption_fixture::PhysicalIdentity {
                    owner: 0,
                    target: u32::from(edge.neighbor_vid()),
                    orientation: 0,
                    slot,
                })
            },
        )
        .map_err(|error| format!("sparse forward extract failed: {error}"))?;
    graph
        .reverse()
        .for_each_live_physical_edge_location_for_label(
            ic_stable_lara::VertexId::from(1),
            label,
            |slot, edge| {
                identities.push(ic_stable_lara::adoption_fixture::PhysicalIdentity {
                    owner: 1,
                    target: u32::from(edge.neighbor_vid()),
                    orientation: 1,
                    slot,
                })
            },
        )
        .map_err(|error| format!("sparse reverse extract failed: {error}"))?;
    identities.sort();
    Ok(identities)
}

#[cfg(feature = "canbench")]
fn integrated_sparse_mutation_checksum() -> u64 {
    let fixture = ic_stable_lara::adoption_fixture::build_sparse_slot_published_fixture(64)
        .expect("sparse fixture");
    let label = ic_stable_lara::labeled::BucketLabelKey::directed_from_index(1);
    let mut checksum = fixture.identities.len() as u64;
    {
        let _scope = canbench_rs::bench_scope("mate_canonical_insert_sparse");
        fixture
            .graph
            .insert_directed_edge(
                ic_stable_lara::VertexId::from(0),
                ic_stable_lara::VertexId::from(1),
                label,
                ic_stable_lara::adoption_fixture::PublishedEdge::new(1, 0),
                ic_stable_lara::adoption_fixture::PublishedEdge::new(0, 0),
            )
            .expect("sparse canonical insert");
    }
    let after_insert = {
        let _scope = canbench_rs::bench_scope("mate_physical_extract_sparse");
        extract_sparse_identities(&fixture.graph).expect("sparse extract after insert")
    };
    checksum = checksum.wrapping_add(after_insert.len() as u64);
    {
        let _scope = canbench_rs::bench_scope("mate_canonical_delete_sparse");
        fixture
            .graph
            .remove_forward_edge_at_slot(ic_stable_lara::VertexId::from(0), label, 1)
            .expect("sparse forward delete");
        fixture
            .graph
            .remove_reverse_edge_at_slot(ic_stable_lara::VertexId::from(1), label, 1)
            .expect("sparse reverse delete");
    }
    let after_delete = {
        let _scope = canbench_rs::bench_scope("mate_physical_extract_sparse");
        extract_sparse_identities(&fixture.graph).expect("sparse extract after delete")
    };
    checksum = checksum.wrapping_add(after_delete.len() as u64);
    let _scope = canbench_rs::bench_scope("mate_candidate_rebuild_sparse");
    checksum
        .wrapping_add(maintenance_candidate_checksum(
            &[after_insert, after_delete],
            MaintenanceCandidate::Shared,
        ))
        .wrapping_add(maintenance_candidate_checksum(
            &[fixture.identities],
            MaintenanceCandidate::Ranked,
        ))
}

#[cfg(feature = "canbench")]
fn extract_mixed_label_identities(
    graph: &ic_stable_lara::labeled::DeferredBidirectionalLabeledLaraGraph<
        ic_stable_lara::adoption_fixture::PublishedEdge,
        ic_stable_lara::adoption_fixture::FixtureMemory,
    >,
    label: ic_stable_lara::labeled::BucketLabelKey,
) -> Result<Vec<ic_stable_lara::adoption_fixture::PhysicalIdentity>, String> {
    let mut identities = Vec::new();
    graph
        .forward()
        .for_each_live_physical_edge_location_for_label(
            ic_stable_lara::VertexId::from(0),
            label,
            |slot, edge| {
                identities.push(ic_stable_lara::adoption_fixture::PhysicalIdentity {
                    owner: 0,
                    target: u32::from(edge.neighbor_vid()),
                    orientation: 0,
                    slot,
                })
            },
        )
        .map_err(|error| format!("mixed forward extract failed: {error}"))?;
    graph
        .reverse()
        .for_each_live_physical_edge_location_for_label(
            ic_stable_lara::VertexId::from(1),
            label,
            |slot, edge| {
                identities.push(ic_stable_lara::adoption_fixture::PhysicalIdentity {
                    owner: 1,
                    target: u32::from(edge.neighbor_vid()),
                    orientation: 1,
                    slot,
                })
            },
        )
        .map_err(|error| format!("mixed reverse extract failed: {error}"))?;
    identities.sort();
    Ok(identities)
}

#[cfg(feature = "canbench")]
fn integrated_mixed_mutation_checksum() -> u64 {
    let fixture = ic_stable_lara::adoption_fixture::build_mixed_label_published_fixture(2, 4)
        .expect("mixed fixture");
    let label = ic_stable_lara::labeled::BucketLabelKey::directed_from_index(1);
    let mut checksum = fixture.identities.len() as u64;
    {
        let _scope = canbench_rs::bench_scope("mate_canonical_insert_mixed");
        fixture
            .graph
            .insert_directed_edge(
                ic_stable_lara::VertexId::from(0),
                ic_stable_lara::VertexId::from(1),
                label,
                ic_stable_lara::adoption_fixture::PublishedEdge::new(1, 0),
                ic_stable_lara::adoption_fixture::PublishedEdge::new(0, 0),
            )
            .expect("mixed canonical insert");
    }
    let after_insert = {
        let _scope = canbench_rs::bench_scope("mate_physical_extract_mixed");
        extract_mixed_label_identities(&fixture.graph, label).expect("mixed extract after insert")
    };
    checksum = checksum.wrapping_add(after_insert.len() as u64);
    {
        let _scope = canbench_rs::bench_scope("mate_canonical_delete_mixed");
        fixture
            .graph
            .remove_forward_edge_at_slot(ic_stable_lara::VertexId::from(0), label, 1)
            .expect("mixed forward delete");
        fixture
            .graph
            .remove_reverse_edge_at_slot(ic_stable_lara::VertexId::from(1), label, 1)
            .expect("mixed reverse delete");
    }
    let after_delete = {
        let _scope = canbench_rs::bench_scope("mate_physical_extract_mixed");
        extract_mixed_label_identities(&fixture.graph, label).expect("mixed extract after delete")
    };
    checksum = checksum.wrapping_add(after_delete.len() as u64);
    let _scope = canbench_rs::bench_scope("mate_candidate_rebuild_mixed");
    checksum.wrapping_add(maintenance_candidate_checksum(
        &[after_insert, after_delete],
        MaintenanceCandidate::Shared,
    ))
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_integrated_canonical_mutation_real_sparse_slots() -> canbench_rs::BenchResult {
    bench_fn(|| black_box(integrated_sparse_mutation_checksum()))
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_integrated_canonical_mutation_real_mixed_labels() -> canbench_rs::BenchResult {
    bench_fn(|| black_box(integrated_mixed_mutation_checksum()))
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_maintenance_rebuild_real_sparse_slots() -> canbench_rs::BenchResult {
    let traces = sparse_maintenance_trace();
    bench_fn(|| black_box(maintenance_rebuild_checksum(&traces)))
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_maintenance_rebuild_real_mixed_labels() -> canbench_rs::BenchResult {
    let traces = mixed_maintenance_trace();
    bench_fn(|| black_box(maintenance_rebuild_checksum(&traces)))
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_maintenance_shared_real_sparse_slots() -> canbench_rs::BenchResult {
    let traces = sparse_maintenance_trace();
    bench_fn(|| {
        black_box(maintenance_candidate_checksum(
            &traces,
            MaintenanceCandidate::Shared,
        ))
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_maintenance_ranked_real_sparse_slots() -> canbench_rs::BenchResult {
    let traces = sparse_maintenance_trace();
    bench_fn(|| {
        black_box(maintenance_candidate_checksum(
            &traces,
            MaintenanceCandidate::Ranked,
        ))
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_maintenance_shared_real_mixed_labels() -> canbench_rs::BenchResult {
    let traces = mixed_maintenance_trace();
    bench_fn(|| {
        black_box(maintenance_candidate_checksum(
            &traces,
            MaintenanceCandidate::Shared,
        ))
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_maintenance_ranked_real_mixed_labels() -> canbench_rs::BenchResult {
    let traces = mixed_maintenance_trace();
    bench_fn(|| {
        black_box(maintenance_candidate_checksum(
            &traces,
            MaintenanceCandidate::Ranked,
        ))
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_maintenance_stale_detection_real_sparse_slots() -> canbench_rs::BenchResult {
    let baseline = real_sparse_slot_identities_for_bench();
    let traces = sparse_maintenance_trace();
    bench_fn(|| black_box(stale_detection_checksum(&baseline, &traces)))
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_maintenance_stale_detection_real_mixed_labels() -> canbench_rs::BenchResult {
    let traces = mixed_maintenance_trace();
    let baseline = traces.first().expect("mixed trace");
    bench_fn(|| black_box(stale_detection_checksum(baseline, &traces)))
}

#[cfg(feature = "canbench")]
fn compression_candidate_probe(
    sequences: &[Vec<u32>],
    identities: &[ic_stable_lara::adoption_fixture::PhysicalIdentity],
    undirected: bool,
) -> (u64, u64, u64, u64) {
    let delta = sequences
        .iter()
        .map(|sequence| delta_restart_bytes(sequence, 16).expect("delta bytes"))
        .sum();
    let elias_fano = sequences
        .iter()
        .filter_map(|sequence| monotone_elias_fano_bytes(sequence))
        .sum();
    let monotone_count = sequences
        .iter()
        .filter(|sequence| monotone_elias_fano_bytes(sequence).is_some())
        .count() as u64;
    let shared = shared_orientation_bytes(identities, undirected).unwrap_or(0);
    (delta, elias_fano, monotone_count, shared)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_candidates_directed_high() -> canbench_rs::BenchResult {
    let (sequences, identities) =
        compression_sequences_for_spec(FixtureSpec::required_matrix()[1], false);
    bench_fn(|| {
        let result = compression_candidate_probe(&sequences, &identities, false);
        black_box(result);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_candidates_parallel() -> canbench_rs::BenchResult {
    let (sequences, identities) =
        compression_sequences_for_spec(FixtureSpec::required_matrix()[6], false);
    bench_fn(|| {
        let result = compression_candidate_probe(&sequences, &identities, false);
        black_box(result);
    })
}

#[cfg(feature = "canbench")]
fn bench_restart_lookup_for_spec(spec: FixtureSpec) -> canbench_rs::BenchResult {
    let (sequences, _) = compression_sequences_for_spec(spec, false);
    bench_fn(|| {
        let mut checksum = 0u64;
        for sequence in &sequences {
            for index in 0..1024usize {
                let index = index % sequence.len();
                let value = delta_restart_reconstruct_at(sequence, 16, index)
                    .expect("restart reconstruction");
                checksum = checksum.wrapping_add(u64::from(value));
            }
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_restart_lookup_directed_high() -> canbench_rs::BenchResult {
    bench_restart_lookup_for_spec(FixtureSpec::required_matrix()[1])
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_restart_lookup_parallel() -> canbench_rs::BenchResult {
    bench_restart_lookup_for_spec(FixtureSpec::required_matrix()[6])
}

#[cfg(feature = "canbench")]
fn bench_shared_orientation_lookup_for_spec(spec: FixtureSpec) -> canbench_rs::BenchResult {
    let (_, identities) = compression_sequences_for_spec(spec, false);
    let lookup = SharedOrientationLookup::build(&identities, false).expect("shared lookup");
    let encoded = lookup.encode().expect("shared encode");
    let lookup = SharedOrientationLookup::decode(&encoded).expect("shared decode");
    let queries = identities
        .iter()
        .filter(|identity| identity.orientation == 0)
        .map(|identity| {
            let rank = lookup
                .rank_for(identity.owner, identity.target, identity.slot)
                .expect("shared source rank");
            (identity.owner, identity.target, rank)
        })
        .collect::<Vec<_>>();
    bench_fn(|| {
        let mut checksum = 0u64;
        for &(owner, target, rank) in queries.iter().cycle().take(1024) {
            checksum = checksum.wrapping_add(u64::from(
                lookup.lookup(owner, target, rank).expect("shared mate"),
            ));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_shared_lookup_directed_high() -> canbench_rs::BenchResult {
    bench_shared_orientation_lookup_for_spec(FixtureSpec::required_matrix()[1])
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_shared_lookup_parallel() -> canbench_rs::BenchResult {
    bench_shared_orientation_lookup_for_spec(FixtureSpec::required_matrix()[6])
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_undirected_pair_rank() -> canbench_rs::BenchResult {
    let spec = FixtureSpec::required_matrix()[3];
    let (_, identities) = compression_sequences_for_spec(spec, true);
    let lookup = UndirectedPairRankLookup::build(&identities).expect("undirected pair rank");
    let queries = identities
        .iter()
        .filter(|identity| identity.owner < identity.target)
        .map(|identity| {
            let rank = identities
                .iter()
                .filter(|other| {
                    other.owner == identity.owner
                        && other.target == identity.target
                        && other.slot <= identity.slot
                })
                .count()
                .checked_sub(1)
                .expect("undirected pair-rank") as u32;
            (identity.owner, identity.target, rank)
        })
        .collect::<Vec<_>>();
    bench_fn(|| {
        let mut checksum = 0u64;
        for &(owner, target, rank) in queries.iter().cycle().take(1024) {
            checksum = checksum.wrapping_add(u64::from(
                lookup
                    .lookup(owner, target, rank)
                    .expect("undirected pair-rank mate"),
            ));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_undirected_pair_rank_exception() -> canbench_rs::BenchResult {
    let pairs = (0..128u32)
        .map(|rank| (rank, 127u32.saturating_sub(rank)))
        .collect::<Vec<_>>();
    let lookup = UndirectedPairRankExceptionLookup::from_ordered_pairs(1, 2, &pairs)
        .expect("exception lookup");
    bench_fn(|| {
        let mut checksum = 0u64;
        for rank in (0..128u32).cycle().take(1024) {
            checksum = checksum.saturating_add(u64::from(
                lookup.lookup(1, 2, rank).expect("exception rank"),
            ));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
fn bench_undirected_block_rank_permutation(block_size: u32) -> canbench_rs::BenchResult {
    let pairs = (0..128u32)
        .map(|rank| (rank, 127u32.saturating_sub(rank)))
        .collect::<Vec<_>>();
    let lookup = UndirectedBlockRankPermutationLookup::from_ordered_pairs(1, 2, &pairs, block_size)
        .expect("block permutation lookup");
    bench_fn(|| {
        let mut checksum = 0u64;
        for rank in (0..128u32).cycle().take(1024) {
            checksum =
                checksum.saturating_add(u64::from(lookup.lookup(1, 2, rank).expect("block rank")));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_undirected_block_rank_8() -> canbench_rs::BenchResult {
    bench_undirected_block_rank_permutation(8)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_undirected_block_rank_16() -> canbench_rs::BenchResult {
    bench_undirected_block_rank_permutation(16)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_undirected_block_rank_32() -> canbench_rs::BenchResult {
    bench_undirected_block_rank_permutation(32)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_undirected_block_rank_64() -> canbench_rs::BenchResult {
    bench_undirected_block_rank_permutation(64)
}

#[cfg(feature = "canbench")]
fn bench_sampled_paired_residual_lookup_for_spec(
    spec: FixtureSpec,
    block_size: usize,
) -> canbench_rs::BenchResult {
    let (_, identities) = compression_sequences_for_spec(spec, false);
    let lookup = SampledPairedResidualLookup::build(&identities, block_size)
        .expect("sampled residual lookup");
    let encoded = lookup.encode().expect("sampled residual encode");
    let lookup =
        SampledPairedResidualLookup::decode(&encoded, block_size).expect("sampled residual decode");
    let queries = identities
        .iter()
        .filter(|identity| identity.orientation == 0)
        .map(|identity| {
            let rank = identities
                .iter()
                .filter(|other| {
                    other.owner == identity.owner
                        && other.target == identity.target
                        && other.orientation == identity.orientation
                })
                .map(|other| other.slot)
                .filter(|&slot| slot <= identity.slot)
                .count()
                .checked_sub(1)
                .expect("sampled rank") as u32;
            (identity.owner, identity.target, rank, identity.slot)
        })
        .collect::<Vec<_>>();
    bench_fn(|| {
        let mut checksum = 0u64;
        for &(owner, target, rank, source_slot) in queries.iter().cycle().take(1024) {
            checksum = checksum.wrapping_add(u64::from(
                lookup
                    .lookup(owner, target, rank, source_slot)
                    .expect("sampled mate"),
            ));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
fn bench_sampled_paired_residual_local_scan_for_spec(
    spec: FixtureSpec,
    block_size: usize,
) -> canbench_rs::BenchResult {
    let (_, identities) = compression_sequences_for_spec(spec, false);
    let lookup = SampledPairedResidualLookup::build(&identities, block_size)
        .expect("sampled residual lookup");
    let encoded = lookup.encode().expect("sampled residual encode");
    let lookup =
        SampledPairedResidualLookup::decode(&encoded, block_size).expect("sampled residual decode");
    let mut source_groups = std::collections::BTreeMap::<(u32, u32), Vec<u32>>::new();
    for identity in identities
        .iter()
        .filter(|identity| identity.orientation == 0)
    {
        source_groups
            .entry((identity.owner, identity.target))
            .or_default()
            .push(identity.slot);
    }
    for slots in source_groups.values_mut() {
        slots.sort_unstable();
    }
    let queries = identities
        .iter()
        .filter(|identity| identity.orientation == 0)
        .map(|identity| {
            let source_slots = source_groups
                .get(&(identity.owner, identity.target))
                .expect("source group");
            let rank = source_slots
                .binary_search(&identity.slot)
                .expect("source rank") as u32;
            (identity.owner, identity.target, rank)
        })
        .collect::<Vec<_>>();
    bench_fn(|| {
        let mut checksum = 0u64;
        for &(owner, target, rank) in queries.iter().cycle().take(1024) {
            let source_slots = source_groups.get(&(owner, target)).expect("source group");
            checksum = checksum.wrapping_add(u64::from(
                lookup
                    .lookup_local_scan(owner, target, rank, source_slots)
                    .expect("sampled local-scan mate"),
            ));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_b8_directed_high() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_lookup_for_spec(FixtureSpec::required_matrix()[1], 8)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_b16_directed_high() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_lookup_for_spec(FixtureSpec::required_matrix()[1], 16)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_b32_directed_high() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_lookup_for_spec(FixtureSpec::required_matrix()[1], 32)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_b64_directed_high() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_lookup_for_spec(FixtureSpec::required_matrix()[1], 64)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_b8_parallel() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_lookup_for_spec(FixtureSpec::required_matrix()[6], 8)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_b16_parallel() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_lookup_for_spec(FixtureSpec::required_matrix()[6], 16)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_b32_parallel() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_lookup_for_spec(FixtureSpec::required_matrix()[6], 32)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_b64_parallel() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_lookup_for_spec(FixtureSpec::required_matrix()[6], 64)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_scan_b8_parallel() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_local_scan_for_spec(FixtureSpec::required_matrix()[6], 8)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_scan_b32_parallel() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_local_scan_for_spec(FixtureSpec::required_matrix()[6], 32)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_sampled_paired_residual_scan_b64_parallel() -> canbench_rs::BenchResult {
    bench_sampled_paired_residual_local_scan_for_spec(FixtureSpec::required_matrix()[6], 64)
}

#[cfg(feature = "canbench")]
fn build_alias_runtime_probe(
    spec: FixtureSpec,
    undirected: bool,
) -> (
    crate::facade::stable::edge_alias::EdgeAliasIndex<ic_stable_structures::VectorMemory>,
    Vec<(ic_stable_lara::VertexId, u16, u32)>,
    Vec<(u32, u8, u32)>,
    ic_stable_lara::adoption_fixture::RankedPackedLookup,
) {
    let (vertex_count, edges) = if undirected {
        let vertex_count = (spec.logical_edges / 2) as u32;
        let edges = (0..vertex_count)
            .flat_map(|source| {
                [
                    (source, (source + 1) % vertex_count),
                    (source, (source + 2) % vertex_count),
                ]
            })
            .collect::<Vec<_>>();
        (vertex_count, edges)
    } else if matches!(spec.shape, FixtureShape::Parallel) {
        (
            2,
            (0..spec.logical_edges).map(|_| (0, 1)).collect::<Vec<_>>(),
        )
    } else {
        let vertex_count = (spec.logical_edges / 2) as u32;
        let edges = (0..vertex_count)
            .flat_map(|source| {
                [
                    (source, (source + 1) % vertex_count),
                    (source, (source + 2) % vertex_count),
                ]
            })
            .collect::<Vec<_>>();
        (vertex_count, edges)
    };
    let fixture = if undirected {
        ic_stable_lara::adoption_fixture::build_alias_only_undirected_fixture(vertex_count, &edges)
            .expect("alias probe fixture")
    } else {
        ic_stable_lara::adoption_fixture::build_alias_only_fixture(vertex_count, &edges)
            .expect("alias probe fixture")
    };
    let mut aliases = crate::facade::stable::edge_alias::EdgeAliasIndex::init(
        ic_stable_structures::VectorMemory::default(),
    );
    let mut alias_queries = Vec::new();
    let mut rank_queries = Vec::new();
    for identity in &fixture.identities {
        let is_alias = if undirected {
            identity.owner > identity.target
        } else {
            identity.orientation == 1
        };
        if !is_alias {
            continue;
        }
        let counterpart_orientation = if undirected {
            0
        } else {
            1 - identity.orientation
        };
        let mut source_slots = fixture
            .identities
            .iter()
            .filter(|other| {
                other.owner == identity.owner
                    && other.target == identity.target
                    && (undirected || other.orientation == identity.orientation)
            })
            .map(|other| other.slot)
            .collect::<Vec<_>>();
        source_slots.sort_unstable();
        let rank = source_slots
            .binary_search(&identity.slot)
            .expect("alias source rank");
        let mut counterpart = fixture
            .identities
            .iter()
            .filter(|other| {
                other.owner == identity.target
                    && other.target == identity.owner
                    && (undirected || other.orientation == counterpart_orientation)
            })
            .collect::<Vec<_>>();
        counterpart.sort_unstable_by_key(|other| other.slot);
        let canonical = counterpart.get(rank).expect("alias counterpart");
        aliases.insert(
            ic_stable_lara::VertexId::from(identity.owner),
            1,
            identity.slot,
            ic_stable_lara::VertexId::from(canonical.owner),
            canonical.slot,
        );
        alias_queries.push((
            ic_stable_lara::VertexId::from(identity.owner),
            1,
            identity.slot,
        ));
        rank_queries.push((
            identity.owner,
            if undirected { 0 } else { identity.orientation },
            rank as u32,
        ));
    }
    let bytes =
        ic_stable_lara::adoption_fixture::ranked_packed_blob(&fixture.identities, undirected)
            .expect("ranked probe bytes");
    let ranked = ic_stable_lara::adoption_fixture::RankedPackedLookup::decode(&bytes)
        .expect("ranked probe decode");
    (aliases, alias_queries, rank_queries, ranked)
}

#[cfg(feature = "canbench")]
fn bench_alias_hit_vs_rank_decode(
    spec: FixtureSpec,
    undirected: bool,
) -> (canbench_rs::BenchResult, canbench_rs::BenchResult) {
    let (aliases, alias_queries, rank_queries, ranked) =
        build_alias_runtime_probe(spec, undirected);
    let alias_result = bench_fn(|| {
        let mut checksum = 0u64;
        for (vertex, label, slot) in alias_queries.iter().cycle().take(1024) {
            if let Some(value) = aliases.get(*vertex, *label, *slot) {
                checksum = checksum.saturating_add(u64::from(value.canonical_slot_index()));
            }
        }
        black_box(checksum);
    });
    let rank_result = bench_fn(|| {
        let mut checksum = 0u64;
        for (owner, orientation, rank) in rank_queries.iter().cycle().take(1024) {
            let slot = ranked
                .lookup(*owner, *orientation, *rank)
                .expect("rank lookup");
            checksum = checksum.saturating_add(u64::from(slot));
        }
        black_box(checksum);
    });
    (alias_result, rank_result)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_alias_hit_directed_high() -> canbench_rs::BenchResult {
    bench_alias_hit_vs_rank_decode(FixtureSpec::required_matrix()[1], false).0
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_rank_decode_directed_high() -> canbench_rs::BenchResult {
    bench_alias_hit_vs_rank_decode(FixtureSpec::required_matrix()[1], false).1
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_alias_hit_parallel() -> canbench_rs::BenchResult {
    bench_alias_hit_vs_rank_decode(FixtureSpec::required_matrix()[6], false).0
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_rank_decode_parallel() -> canbench_rs::BenchResult {
    bench_alias_hit_vs_rank_decode(FixtureSpec::required_matrix()[6], false).1
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_rank_decode_undirected_high() -> canbench_rs::BenchResult {
    bench_alias_hit_vs_rank_decode(FixtureSpec::required_matrix()[3], true).1
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_ranked_bytes_directed_high() -> canbench_rs::BenchResult {
    bench_ranked_bytes(FixtureSpec::required_matrix()[1], false)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_ranked_bytes_parallel() -> canbench_rs::BenchResult {
    bench_ranked_bytes(FixtureSpec::required_matrix()[6], false)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_ranked_bytes_undirected_high() -> canbench_rs::BenchResult {
    bench_ranked_bytes(FixtureSpec::required_matrix()[3], true)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_ranked_bytes_sparse_slots() -> canbench_rs::BenchResult {
    let identities = synthetic_sparse_slot_identities();
    bench_fn(|| {
        let bytes = ic_stable_lara::adoption_fixture::ranked_packed_blob_bytes(&identities, false)
            .expect("sparse ranked bytes");
        black_box(bytes);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_shared_lookup_sparse_slots() -> canbench_rs::BenchResult {
    let identities = synthetic_sparse_slot_identities();
    let lookup = SharedOrientationLookup::build(&identities, false).expect("sparse shared lookup");
    bench_fn(|| {
        let mut checksum = 0u64;
        for rank in (0..32u32).cycle().take(1024) {
            checksum = checksum.saturating_add(u64::from(
                lookup.lookup(1, 2, rank).expect("sparse shared mate"),
            ));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_rank_lookup_sparse_slots() -> canbench_rs::BenchResult {
    let identities = synthetic_sparse_slot_identities();
    let bytes = ic_stable_lara::adoption_fixture::ranked_packed_blob(&identities, false)
        .expect("sparse ranked blob");
    let lookup = ic_stable_lara::adoption_fixture::RankedPackedLookup::decode(&bytes)
        .expect("sparse ranked lookup");
    bench_fn(|| {
        let mut checksum = 0u64;
        for rank in (0..32u32).cycle().take(1024) {
            checksum = checksum.saturating_add(u64::from(
                lookup.lookup(1, 0, rank).expect("sparse ranked mate"),
            ));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_mixed_label_shared_lookup() -> canbench_rs::BenchResult {
    let [first, second] = synthetic_mixed_label_identity_sets();
    let first_lookup = SharedOrientationLookup::build(&first, false).expect("first label lookup");
    let second_lookup =
        SharedOrientationLookup::build(&second, false).expect("second label lookup");
    bench_fn(|| {
        let mut checksum = 0u64;
        for rank in (0..16u32).cycle().take(1024) {
            let lookup = if rank % 2 == 0 {
                &first_lookup
            } else {
                &second_lookup
            };
            checksum = checksum.saturating_add(u64::from(
                lookup
                    .lookup(1, if rank % 2 == 0 { 2 } else { 3 }, rank / 2)
                    .expect("mixed label mate"),
            ));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_rank_lookup_mixed_labels() -> canbench_rs::BenchResult {
    let [first, second] = synthetic_mixed_label_identity_sets();
    let first_bytes = ic_stable_lara::adoption_fixture::ranked_packed_blob(&first, false)
        .expect("first label ranked blob");
    let second_bytes = ic_stable_lara::adoption_fixture::ranked_packed_blob(&second, false)
        .expect("second label ranked blob");
    let first_lookup = ic_stable_lara::adoption_fixture::RankedPackedLookup::decode(&first_bytes)
        .expect("first label ranked lookup");
    let second_lookup = ic_stable_lara::adoption_fixture::RankedPackedLookup::decode(&second_bytes)
        .expect("second label ranked lookup");
    bench_fn(|| {
        let mut checksum = 0u64;
        for rank in (0..16u32).cycle().take(1024) {
            let slot = if rank % 2 == 0 {
                first_lookup
                    .lookup(1, 0, rank / 2)
                    .expect("first label mate")
            } else {
                second_lookup
                    .lookup(1, 0, rank / 2)
                    .expect("second label mate")
            };
            checksum = checksum.saturating_add(u64::from(slot));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_shared_lookup_real_mixed_labels() -> canbench_rs::BenchResult {
    let sets = real_mixed_label_identity_sets_for_bench(4);
    let lookups = sets
        .iter()
        .map(|identities| {
            let lookup = SharedOrientationLookup::build(identities, false).expect("shared lookup");
            let queries = identities
                .iter()
                .filter(|identity| identity.orientation == 0)
                .map(|identity| {
                    let rank = lookup
                        .rank_for(identity.owner, identity.target, identity.slot)
                        .expect("shared rank");
                    (identity.owner, identity.target, rank)
                })
                .collect::<Vec<_>>();
            (lookup, queries)
        })
        .collect::<Vec<_>>();
    bench_fn(|| {
        let mut checksum = 0u64;
        for (lookup, queries) in &lookups {
            for &(owner, target, rank) in queries.iter().cycle().take(1024) {
                checksum = checksum.wrapping_add(u64::from(
                    lookup.lookup(owner, target, rank).expect("shared mate"),
                ));
            }
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_rank_lookup_real_mixed_labels() -> canbench_rs::BenchResult {
    let lookups = real_mixed_label_identity_sets_for_bench(4)
        .into_iter()
        .map(|identities| {
            let bytes = ic_stable_lara::adoption_fixture::ranked_packed_blob(&identities, false)
                .expect("ranked encode");
            let lookup = ic_stable_lara::adoption_fixture::RankedPackedLookup::decode(&bytes)
                .expect("ranked decode");
            let queries = identities
                .iter()
                .filter(|identity| identity.orientation == 0)
                .enumerate()
                .map(|(rank, identity)| (identity.owner, rank as u32))
                .collect::<Vec<_>>();
            (lookup, queries)
        })
        .collect::<Vec<_>>();
    bench_fn(|| {
        let mut checksum = 0u64;
        for (lookup, queries) in &lookups {
            for &(owner, rank) in queries.iter().cycle().take(1024) {
                checksum = checksum.wrapping_add(u64::from(
                    lookup.lookup(owner, 0, rank).expect("ranked mate"),
                ));
            }
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_compression_shared_lookup_real_sparse_slots() -> canbench_rs::BenchResult {
    let identities = real_sparse_slot_identities_for_bench();
    let lookup = SharedOrientationLookup::build(&identities, false).expect("shared lookup");
    let queries = identities
        .iter()
        .filter(|identity| identity.orientation == 0)
        .map(|identity| {
            let rank = lookup
                .rank_for(identity.owner, identity.target, identity.slot)
                .expect("shared rank");
            (identity.owner, identity.target, rank)
        })
        .collect::<Vec<_>>();
    bench_fn(|| {
        let mut checksum = 0u64;
        for &(owner, target, rank) in queries.iter().cycle().take(1024) {
            checksum = checksum.wrapping_add(u64::from(
                lookup.lookup(owner, target, rank).expect("shared mate"),
            ));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_rank_lookup_real_sparse_slots() -> canbench_rs::BenchResult {
    let identities = real_sparse_slot_identities_for_bench();
    let bytes = ic_stable_lara::adoption_fixture::ranked_packed_blob(&identities, false)
        .expect("ranked encode");
    let lookup = ic_stable_lara::adoption_fixture::RankedPackedLookup::decode(&bytes)
        .expect("ranked decode");
    let queries = identities
        .iter()
        .filter(|identity| identity.orientation == 0)
        .enumerate()
        .map(|(rank, identity)| (identity.owner, rank as u32))
        .collect::<Vec<_>>();
    bench_fn(|| {
        let mut checksum = 0u64;
        for &(owner, rank) in queries.iter().cycle().take(1024) {
            checksum = checksum.wrapping_add(u64::from(
                lookup.lookup(owner, 0, rank).expect("ranked mate"),
            ));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
fn bench_real_scan_lookup(
    fixture: ic_stable_lara::adoption_fixture::SparseSlotPublishedFixture,
    refs: Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
) -> canbench_rs::BenchResult {
    bench_fn(|| {
        let mut checksum = 0u64;
        for edge in refs.iter().cycle().take(1024) {
            let mate = fixture.graph.mate_of(*edge).expect("sparse scan mate");
            checksum = checksum.wrapping_add(u64::from(mate.slot_index.raw()));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_scan_lookup_real_sparse_slots() -> canbench_rs::BenchResult {
    let (fixture, refs) = real_sparse_scan_refs_for_bench();
    bench_real_scan_lookup(fixture, refs)
}

#[cfg(feature = "canbench")]
fn bench_real_mixed_scan_lookup(
    fixture: ic_stable_lara::adoption_fixture::MixedLabelPublishedFixture,
    refs: Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
) -> canbench_rs::BenchResult {
    bench_fn(|| {
        let mut checksum = 0u64;
        for edge in refs.iter().cycle().take(1024) {
            let mate = fixture.graph.mate_of(*edge).expect("mixed scan mate");
            checksum = checksum.wrapping_add(u64::from(mate.slot_index.raw()));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_scan_lookup_real_mixed_labels() -> canbench_rs::BenchResult {
    let (fixture, refs) = real_mixed_scan_refs_for_bench(4);
    bench_real_mixed_scan_lookup(fixture, refs)
}

#[cfg(feature = "canbench")]
fn published_directed_runtime_fixture() -> (
    ic_stable_lara::adoption_fixture::PublishedFixture,
    Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
) {
    published_directed_runtime_fixture_with_logical_edges(128)
}

#[cfg(feature = "canbench")]
fn build_alias_reverse_probe() -> (
    ic_stable_lara::adoption_fixture::PublishedFixture,
    crate::facade::stable::edge_alias::EdgeAliasIndex<ic_stable_structures::VectorMemory>,
    Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
) {
    let (fixture, _) = published_directed_runtime_fixture();
    let label = ic_stable_lara::labeled::BucketLabelKey::directed_from_index(1);
    let mut aliases = crate::facade::stable::edge_alias::EdgeAliasIndex::init(
        ic_stable_structures::VectorMemory::default(),
    );
    let mut canonical_refs = Vec::new();
    for identity in fixture.identities.iter().filter(|row| row.orientation == 1) {
        let mut source_slots = fixture
            .identities
            .iter()
            .filter(|other| {
                other.orientation == 1
                    && other.owner == identity.owner
                    && other.target == identity.target
            })
            .map(|other| other.slot)
            .collect::<Vec<_>>();
        source_slots.sort_unstable();
        let rank = source_slots
            .binary_search(&identity.slot)
            .expect("reverse source rank");
        let mut counterparts = fixture
            .identities
            .iter()
            .filter(|other| {
                other.orientation == 0
                    && other.owner == identity.target
                    && other.target == identity.owner
            })
            .collect::<Vec<_>>();
        counterparts.sort_unstable_by_key(|other| other.slot);
        let canonical = counterparts.get(rank).expect("reverse counterpart");
        aliases.insert(
            ic_stable_lara::VertexId::from(identity.owner),
            label.raw(),
            identity.slot,
            ic_stable_lara::VertexId::from(canonical.owner),
            canonical.slot,
        );
    }
    for identity in fixture.identities.iter().filter(|row| row.orientation == 0) {
        canonical_refs.push(ic_stable_lara::labeled::CanonicalEdgeOccurrence {
            orientation: ic_stable_lara::labeled::LabeledOrientation::Forward,
            owner_vertex_id: ic_stable_lara::VertexId::from(identity.owner),
            label_id: label,
            slot_index: identity.slot.into(),
        });
    }
    (fixture, aliases, canonical_refs)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_alias_reverse_lookup_directed_high() -> canbench_rs::BenchResult {
    let (_fixture, aliases, canonical_refs) = build_alias_reverse_probe();
    let label = ic_stable_lara::labeled::BucketLabelKey::directed_from_index(1);
    bench_fn(|| {
        let mut checksum = 0u64;
        for reference in canonical_refs.iter().cycle().take(1024) {
            if let Some((vertex, slot)) = aliases.find_alias_for_canonical(
                reference.owner_vertex_id,
                label.raw(),
                reference.slot_index.raw(),
            ) {
                checksum = checksum
                    .saturating_add(u64::from(vertex))
                    .saturating_add(u64::from(slot));
            }
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_alias_miss_canonical_fallback_directed_high() -> canbench_rs::BenchResult {
    let (fixture, _aliases, canonical_refs) = build_alias_reverse_probe();
    bench_fn(|| {
        let mut checksum = 0u64;
        for reference in canonical_refs.iter().cycle().take(1024) {
            let mate = fixture
                .graph
                .mate_of(*reference)
                .expect("canonical fallback");
            checksum = checksum.saturating_add(u64::from(mate.slot_index.raw()));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
fn published_directed_runtime_fixture_with_logical_edges(
    logical_edges: u32,
) -> (
    ic_stable_lara::adoption_fixture::PublishedFixture,
    Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
) {
    // The fixture type owns a mate-capable graph so the same canonical rows can also support the
    // retired-path comparison.  These scan-only benches call `mate_of` exclusively; no published
    // blob lookup is performed and the published result is never consulted.
    assert!(logical_edges >= 2 && logical_edges.is_multiple_of(2));
    let vertex_count = logical_edges / 2;
    let edges = (0..vertex_count)
        .flat_map(|source| {
            [
                (source, (source + 1) % vertex_count),
                (source, (source + 2) % vertex_count),
            ]
        })
        .collect::<Vec<_>>();
    let fixture = ic_stable_lara::adoption_fixture::build_published_fixture(vertex_count, &edges)
        .expect("published runtime fixture");
    let directed_label = ic_stable_lara::labeled::BucketLabelKey::directed_from_index(1);
    let refs = fixture
        .identities
        .iter()
        .map(
            |identity| ic_stable_lara::labeled::CanonicalEdgeOccurrence {
                orientation: if identity.orientation == 0 {
                    ic_stable_lara::labeled::LabeledOrientation::Forward
                } else {
                    ic_stable_lara::labeled::LabeledOrientation::Reverse
                },
                owner_vertex_id: ic_stable_lara::VertexId::from(identity.owner),
                label_id: directed_label,
                slot_index: identity.slot.into(),
            },
        )
        .collect();
    (fixture, refs)
}

#[cfg(feature = "canbench")]
fn bench_scan_runtime_lookup_count(
    fixture: ic_stable_lara::adoption_fixture::PublishedFixture,
    refs: Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
    count: usize,
) -> canbench_rs::BenchResult {
    bench_fn(|| {
        let mut checksum = 0u64;
        for edge in refs.iter().cycle().take(count) {
            let mate = fixture.graph.mate_of(*edge).expect("canonical mate");
            checksum = checksum.saturating_add(u64::from(mate.slot_index.raw()));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
fn bench_scan_runtime_single_ref(
    fixture: ic_stable_lara::adoption_fixture::PublishedFixture,
    refs: Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
    index: usize,
) -> canbench_rs::BenchResult {
    let edge = refs[index];
    bench_fn(|| {
        let mut checksum = 0u64;
        for _ in 0..1024 {
            let mate = fixture.graph.mate_of(edge).expect("canonical mate");
            checksum = checksum.saturating_add(u64::from(mate.slot_index.raw()));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
fn published_parallel_runtime_fixture() -> (
    ic_stable_lara::adoption_fixture::PublishedFixture,
    Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
) {
    published_parallel_runtime_fixture_with_edges(32)
}

#[cfg(feature = "canbench")]
fn published_parallel_runtime_fixture_with_edges(
    logical_edges: u32,
) -> (
    ic_stable_lara::adoption_fixture::PublishedFixture,
    Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
) {
    assert!(logical_edges > 0);
    let edges = (0..logical_edges).map(|_| (0, 1)).collect::<Vec<_>>();
    let fixture = ic_stable_lara::adoption_fixture::build_published_fixture(2, &edges)
        .expect("published parallel runtime fixture");
    let label = ic_stable_lara::labeled::BucketLabelKey::directed_from_index(1);
    let refs = fixture
        .identities
        .iter()
        .map(
            |identity| ic_stable_lara::labeled::CanonicalEdgeOccurrence {
                orientation: if identity.orientation == 0 {
                    ic_stable_lara::labeled::LabeledOrientation::Forward
                } else {
                    ic_stable_lara::labeled::LabeledOrientation::Reverse
                },
                owner_vertex_id: ic_stable_lara::VertexId::from(identity.owner),
                label_id: label,
                slot_index: identity.slot.into(),
            },
        )
        .collect();
    (fixture, refs)
}

#[cfg(feature = "canbench")]
fn published_undirected_runtime_fixture() -> (
    ic_stable_lara::adoption_fixture::PublishedFixture,
    Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
) {
    let vertex_count = 64u32;
    let edges = (0..vertex_count)
        .flat_map(|source| {
            [
                (source, (source + 1) % vertex_count),
                (source, (source + 2) % vertex_count),
            ]
        })
        .collect::<Vec<_>>();
    let fixture =
        ic_stable_lara::adoption_fixture::build_published_undirected_fixture(vertex_count, &edges)
            .expect("published undirected runtime fixture");
    let label = ic_stable_lara::labeled::BucketLabelKey::undirected_from_index(1);
    let refs = fixture
        .identities
        .iter()
        .map(
            |identity| ic_stable_lara::labeled::CanonicalEdgeOccurrence {
                orientation: ic_stable_lara::labeled::LabeledOrientation::Forward,
                owner_vertex_id: ic_stable_lara::VertexId::from(identity.owner),
                label_id: label,
                slot_index: identity.slot.into(),
            },
        )
        .collect();
    (fixture, refs)
}

#[cfg(feature = "canbench")]
fn bench_runtime_lookup(
    fixture: ic_stable_lara::adoption_fixture::PublishedFixture,
    refs: Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
    published: bool,
) -> canbench_rs::BenchResult {
    bench_runtime_lookup_count(fixture, refs, published, 1024)
}

#[cfg(feature = "canbench")]
fn bench_runtime_lookup_count(
    fixture: ic_stable_lara::adoption_fixture::PublishedFixture,
    refs: Vec<ic_stable_lara::labeled::CanonicalEdgeOccurrence>,
    published: bool,
    count: usize,
) -> canbench_rs::BenchResult {
    bench_fn(|| {
        let mut checksum = 0u64;
        for edge in refs.iter().cycle().take(count) {
            let mate = if published {
                fixture.graph.published_mate_of(*edge)
            } else {
                fixture.graph.mate_of(*edge)
            }
            .expect("runtime mate");
            checksum = checksum.saturating_add(u64::from(mate.slot_index.raw()));
        }
        black_box(checksum);
    })
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_directed_high() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_directed_runtime_fixture();
    bench_scan_runtime_lookup_count(fixture, refs, 1024)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_directed_low() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_directed_runtime_fixture_with_logical_edges(32);
    bench_scan_runtime_lookup_count(fixture, refs, 1024)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_directed_wide() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_directed_runtime_fixture_with_logical_edges(256);
    bench_scan_runtime_lookup_count(fixture, refs, 1024)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_published_directed_high() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_directed_runtime_fixture();
    bench_runtime_lookup(fixture, refs, true)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_directed_single() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_directed_runtime_fixture();
    bench_runtime_lookup_count(fixture, refs, false, 1)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_published_directed_single() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_directed_runtime_fixture();
    bench_runtime_lookup_count(fixture, refs, true, 1)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_parallel() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_parallel_runtime_fixture();
    bench_runtime_lookup(fixture, refs, false)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_parallel_128() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_parallel_runtime_fixture_with_edges(128);
    bench_runtime_lookup(fixture, refs, false)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_parallel_256() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_parallel_runtime_fixture_with_edges(256);
    bench_runtime_lookup(fixture, refs, false)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_parallel_mid_32() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_parallel_runtime_fixture_with_edges(32);
    bench_scan_runtime_single_ref(fixture, refs, 16)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_parallel_mid_128() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_parallel_runtime_fixture_with_edges(128);
    bench_scan_runtime_single_ref(fixture, refs, 64)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_parallel_mid_256() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_parallel_runtime_fixture_with_edges(256);
    bench_scan_runtime_single_ref(fixture, refs, 128)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_published_parallel() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_parallel_runtime_fixture();
    bench_runtime_lookup(fixture, refs, true)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_scan_undirected_high() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_undirected_runtime_fixture();
    bench_runtime_lookup(fixture, refs, false)
}

#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_mate_adoption_runtime_published_undirected_high() -> canbench_rs::BenchResult {
    let (fixture, refs) = published_undirected_runtime_fixture();
    bench_runtime_lookup(fixture, refs, true)
}

#[cfg(test)]
mod fixture_evidence_tests {
    use super::*;
    use crate::bench::mate_compression::UndirectedPairRankLookup;

    fn observation() -> CompressionPolicyObservation {
        CompressionPolicyObservation {
            live_degree: 128,
            requests: 1024,
            monotone_rank_sequence: false,
            alias_bytes: 2304,
            scan_instructions: 18_000_000,
            ranked_bytes: 2000,
            ranked_instructions: 1_500_000,
            shared_bytes: None,
            shared_instructions: None,
            compressed_bytes: None,
            compressed_instructions: None,
            pair_rank_bytes: None,
            pair_rank_instructions: None,
            exact_and_fail_closed: true,
        }
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn maintenance_traces_are_deterministic_and_pair_preserving() {
        let first = sparse_maintenance_trace();
        let second = sparse_maintenance_trace();
        assert_eq!(first, second);
        assert_eq!(first.len(), 3);
        for trace in &first {
            assert_eq!(trace.len() % 2, 0);
            assert!(SharedOrientationLookup::build(trace, false).is_ok());
            assert!(ic_stable_lara::adoption_fixture::ranked_packed_blob(trace, false).is_ok());
        }
        assert_eq!(
            stale_detection_checksum(&real_sparse_slot_identities_for_bench(), &first),
            2
        );
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn malformed_maintenance_trace_fails_closed_without_mutating_base() {
        let base = real_sparse_slot_identities_for_bench();
        let snapshot = base.clone();
        let mut malformed = base.clone();
        let removed = malformed
            .iter()
            .find(|identity| identity.orientation == 1)
            .copied()
            .expect("reverse identity");
        malformed.retain(|identity| *identity != removed);
        assert!(SharedOrientationLookup::build(&malformed, false).is_err());
        assert!(ic_stable_lara::adoption_fixture::ranked_packed_blob(&malformed, false).is_err());
        assert_eq!(base, snapshot);
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn maintenance_amortization_requires_explicit_read_update_ratio() {
        assert!(amortized_read_savings(45_630_000, 45_500_000, 326_110, 1) < 0);
        assert!(amortized_read_savings(45_630_000, 175_430, 326_110, 1_024) > 0);
        assert!(amortized_read_savings(16_010_000, 350_590, 67_190, 1) > 0);
    }

    #[test]
    fn final_amortization_gate_reports_break_even_and_fail_closed_outcomes() {
        assert_eq!(break_even_reads(45_590_000, 175_430, 506_530), Some(1));
        let mut sparse = observation();
        sparse.live_degree = 32;
        sparse.requests = 1024;
        sparse.scan_instructions = 45_590_000;
        sparse.alias_bytes = 2304;
        sparse.ranked_bytes = 128;
        sparse.ranked_instructions = 300_350;
        sparse.shared_bytes = Some(84);
        sparse.shared_instructions = Some(175_430);
        assert_eq!(
            select_amortized_candidate(AmortizationObservation {
                compression: sparse,
                shared_update_instructions: 506_530,
                ranked_update_instructions: 506_530,
                reads_per_update: 1,
            }),
            CompressionCandidate::SharedOrientation
        );
        let mut negative = sparse;
        negative.scan_instructions = 1_000_000;
        negative.ranked_instructions = 900_000;
        negative.shared_instructions = Some(900_000);
        assert_eq!(
            select_amortized_candidate(AmortizationObservation {
                compression: negative,
                shared_update_instructions: 506_530,
                ranked_update_instructions: 506_530,
                reads_per_update: 1,
            }),
            CompressionCandidate::ScanOnly
        );
        let mut malformed = sparse;
        malformed.exact_and_fail_closed = false;
        assert_eq!(
            select_amortized_candidate(AmortizationObservation {
                compression: malformed,
                shared_update_instructions: 1,
                ranked_update_instructions: 1,
                reads_per_update: u64::MAX,
            }),
            CompressionCandidate::ScanOnly
        );
    }

    #[test]
    fn adaptive_policy_keeps_low_or_insufficient_evidence_scan_only() {
        let mut low = observation();
        low.live_degree = MIN_RANK_LIVE_DEGREE - 1;
        assert_eq!(
            select_compression_candidate(low),
            CompressionCandidate::ScanOnly
        );

        let mut cold = observation();
        cold.requests = MIN_RANK_REQUESTS - 1;
        assert_eq!(
            select_compression_candidate(cold),
            CompressionCandidate::ScanOnly
        );

        let mut unsafe_result = observation();
        unsafe_result.exact_and_fail_closed = false;
        assert_eq!(
            select_compression_candidate(unsafe_result),
            CompressionCandidate::ScanOnly
        );
    }

    #[test]
    fn topology_adoption_matrix_is_explicit_and_fail_closed() {
        let evidence = observation();
        for shape in [
            FixtureShape::Directed,
            FixtureShape::Undirected,
            FixtureShape::DirectedSelfLoop,
            FixtureShape::UndirectedSelfLoop,
            FixtureShape::Parallel,
            FixtureShape::SparseSlots,
            FixtureShape::MixedLabels,
        ] {
            assert_eq!(
                select_adoption_disposition(shape, AccessProfile::Cold, Some(evidence)),
                AdoptionDisposition::ScanOnly,
                "cold {shape:?} must remain ScanOnly"
            );
        }
        assert_eq!(
            select_adoption_disposition(FixtureShape::Directed, AccessProfile::Cold, None),
            AdoptionDisposition::ScanOnly
        );
        let mut low = evidence;
        low.live_degree = PROMOTE_MIN_LIVE_EDGES - 1;
        assert_eq!(
            select_adoption_disposition(FixtureShape::Directed, AccessProfile::Hot, Some(low)),
            AdoptionDisposition::ScanOnly
        );
        for shape in [
            FixtureShape::Directed,
            FixtureShape::Undirected,
            FixtureShape::Parallel,
            FixtureShape::SparseSlots,
            FixtureShape::MixedLabels,
        ] {
            assert_eq!(
                select_adoption_disposition(shape, AccessProfile::Hot, None),
                AdoptionDisposition::Deferred,
                "missing evidence for {shape:?} must not promote"
            );
        }
        assert_eq!(
            select_adoption_disposition(
                FixtureShape::DirectedSelfLoop,
                AccessProfile::Hot,
                Some(evidence)
            ),
            AdoptionDisposition::ScanOnly
        );
        assert_eq!(
            select_adoption_disposition(
                FixtureShape::UndirectedSelfLoop,
                AccessProfile::Hot,
                Some(evidence)
            ),
            AdoptionDisposition::ScanOnly
        );
    }

    #[test]
    fn topology_adoption_matrix_maps_only_gated_candidates() {
        let mut shared = observation();
        shared.shared_bytes = Some(1_800);
        shared.shared_instructions = Some(672_220);
        assert_eq!(
            select_adoption_disposition(FixtureShape::Directed, AccessProfile::Hot, Some(shared)),
            AdoptionDisposition::SharedOrientation
        );
        assert_eq!(
            select_adoption_disposition(
                FixtureShape::MixedLabels,
                AccessProfile::Hot,
                Some(shared)
            ),
            AdoptionDisposition::SharedOrientation
        );

        let mut undirected = observation();
        undirected.shared_bytes = None;
        undirected.shared_instructions = None;
        undirected.pair_rank_bytes = Some(1_544);
        undirected.pair_rank_instructions = Some(721_260);
        assert_eq!(
            select_adoption_disposition(
                FixtureShape::Undirected,
                AccessProfile::Hot,
                Some(undirected)
            ),
            AdoptionDisposition::PairRank
        );
        let mut pair_rank_fail = undirected;
        pair_rank_fail.pair_rank_instructions = Some(pair_rank_fail.scan_instructions + 1);
        assert_eq!(
            select_adoption_disposition(
                FixtureShape::Undirected,
                AccessProfile::Hot,
                Some(pair_rank_fail)
            ),
            AdoptionDisposition::ScanOnly
        );
        let mut pair_rank_missing = undirected;
        pair_rank_missing.pair_rank_bytes = None;
        assert_eq!(
            select_adoption_disposition(
                FixtureShape::Undirected,
                AccessProfile::Hot,
                Some(pair_rank_missing)
            ),
            AdoptionDisposition::Deferred
        );

        let mut unsafe_result = shared;
        unsafe_result.exact_and_fail_closed = false;
        assert_eq!(
            select_adoption_disposition(
                FixtureShape::Directed,
                AccessProfile::Hot,
                Some(unsafe_result)
            ),
            AdoptionDisposition::ScanOnly
        );
    }

    fn complete_adoption_rows() -> Vec<AdoptionEvidenceRow> {
        REQUIRED_ADOPTION_FIXTURE_IDS
            .into_iter()
            .map(|fixture_id| AdoptionEvidenceRow {
                fixture_id,
                disposition: if fixture_requires_candidate(fixture_id) {
                    AdoptionDisposition::RankIndexedPacked
                } else {
                    AdoptionDisposition::ScanOnly
                },
                evidence_present: true,
                exact_results: true,
                fallback_safe: true,
                logical_bytes_pass: true,
                runtime_pass: true,
            })
            .collect()
    }

    #[test]
    fn adoption_status_requires_complete_unique_safe_matrix() {
        assert_eq!(
            aggregate_adoption_status(&complete_adoption_rows()),
            AdoptionStatus::Adopt
        );

        let mut missing = complete_adoption_rows();
        missing.pop();
        assert_eq!(aggregate_adoption_status(&missing), AdoptionStatus::Hold);

        let mut duplicate = complete_adoption_rows();
        duplicate[1].fixture_id = duplicate[0].fixture_id;
        assert_eq!(aggregate_adoption_status(&duplicate), AdoptionStatus::Hold);

        let mut absent = complete_adoption_rows();
        absent[0].evidence_present = false;
        assert_eq!(aggregate_adoption_status(&absent), AdoptionStatus::Hold);

        let mut unsafe_result = complete_adoption_rows();
        unsafe_result[0].exact_results = false;
        assert_eq!(
            aggregate_adoption_status(&unsafe_result),
            AdoptionStatus::Hold
        );
    }

    #[test]
    fn adoption_status_distinguishes_performance_partial_from_hold() {
        let mut partial = complete_adoption_rows();
        partial[1].logical_bytes_pass = false;
        partial[6].runtime_pass = false;
        assert_eq!(
            aggregate_adoption_status(&partial),
            AdoptionStatus::Partial {
                ready: 8,
                total: 10
            }
        );

        partial[2].disposition = AdoptionDisposition::Deferred;
        assert_eq!(aggregate_adoption_status(&partial), AdoptionStatus::Hold);

        let mut no_candidate = complete_adoption_rows();
        no_candidate[1].disposition = AdoptionDisposition::ScanOnly;
        assert_eq!(
            aggregate_adoption_status(&no_candidate),
            AdoptionStatus::Partial {
                ready: 9,
                total: 10
            }
        );
    }

    #[test]
    fn self_loop_contracts_keep_directed_orientations_and_undirected_single_entry() {
        let directed = ic_stable_lara::adoption_fixture::build_alias_only_fixture(1, &[(0, 0)])
            .expect("directed self-loop fixture");
        assert_eq!(directed.identities.len(), 2);
        assert_eq!(
            directed
                .identities
                .iter()
                .map(|identity| identity.orientation)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            2
        );

        let undirected =
            ic_stable_lara::adoption_fixture::build_alias_only_undirected_fixture(1, &[(0, 0)])
                .expect("undirected self-loop fixture");
        assert_eq!(undirected.identities.len(), 1);
        assert_eq!(
            undirected.identities[0].owner,
            undirected.identities[0].target
        );
    }

    #[test]
    fn sparse_and_mixed_topologies_are_evaluated_per_bucket() {
        let sparse = build_fixture(FixtureSpec::required_matrix()[7]);
        let max_sparse_slot = sparse
            .identities
            .iter()
            .map(|identity| identity.slot)
            .max()
            .expect("sparse slot");
        assert!(max_sparse_slot >= sparse.identities.len() as u32 * 2);

        let mixed = build_fixture(FixtureSpec::required_matrix()[8]);
        let labels = mixed
            .identities
            .iter()
            .map(|identity| identity.label)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(labels.len() >= 2);
        let mut per_label = observation();
        per_label.live_degree = MIN_RANK_LIVE_DEGREE / 2;
        assert_eq!(
            select_compression_candidate(per_label),
            CompressionCandidate::ScanOnly
        );
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn synthetic_sparse_and_mixed_lookup_models_keep_bucket_boundaries() {
        let sparse = synthetic_sparse_slot_identities();
        let sparse_lookup = SharedOrientationLookup::build(&sparse, false).expect("sparse lookup");
        let sparse_ranked_bytes =
            ic_stable_lara::adoption_fixture::ranked_packed_blob_bytes(&sparse, false)
                .expect("sparse ranked bytes");
        let sparse_shared_bytes = sparse_lookup.encode().expect("sparse encoding").len();
        println!(
            "sparse_slots: ranked_bytes={sparse_ranked_bytes} shared_bytes={sparse_shared_bytes}"
        );
        assert!(sparse_shared_bytes > 100);
        assert_eq!(sparse_lookup.lookup(1, 2, 3), Some(3 * 1024));

        let [first, second] = synthetic_mixed_label_identity_sets();
        let first_lookup = SharedOrientationLookup::build(&first, false).expect("first label");
        let second_lookup = SharedOrientationLookup::build(&second, false).expect("second label");
        println!(
            "mixed_labels: first_shared_bytes={} second_shared_bytes={} first_ranked_bytes={} second_ranked_bytes={}",
            first_lookup.encode().expect("first encoding").len(),
            second_lookup.encode().expect("second encoding").len(),
            ic_stable_lara::adoption_fixture::ranked_packed_blob_bytes(&first, false)
                .expect("first ranked bytes"),
            ic_stable_lara::adoption_fixture::ranked_packed_blob_bytes(&second, false)
                .expect("second ranked bytes")
        );
        assert_eq!(first_lookup.lookup(1, 2, 3), Some(3));
        assert_eq!(second_lookup.lookup(1, 3, 3), Some(3));
        assert!(first_lookup.lookup(1, 3, 0).is_none());
        assert!(second_lookup.lookup(1, 2, 0).is_none());
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn real_mixed_label_fixture_keeps_label_local_candidates() {
        let fixture = ic_stable_lara::adoption_fixture::build_mixed_label_published_fixture(2, 16)
            .expect("real mixed-label fixture");
        let labels = fixture
            .identities
            .iter()
            .map(|identity| identity.label)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(labels.len(), 2);
        for label in labels {
            let identities = fixture
                .identities
                .iter()
                .filter(|identity| identity.label == label)
                .map(
                    |identity| ic_stable_lara::adoption_fixture::PhysicalIdentity {
                        owner: identity.owner,
                        target: identity.target,
                        orientation: identity.orientation,
                        slot: identity.slot,
                    },
                )
                .collect::<Vec<_>>();
            let shared = SharedOrientationLookup::build(&identities, false).expect("shared");
            let ranked =
                ic_stable_lara::adoption_fixture::ranked_packed_blob_bytes(&identities, false)
                    .expect("ranked");
            println!(
                "real_mixed_label label={label}: ranked_bytes={ranked} shared_bytes={}",
                shared.encode().expect("shared encoding").len()
            );
            assert!(!identities.is_empty());
        }
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn real_sparse_fixture_exposes_overflow_log_locations() {
        let fixture = ic_stable_lara::adoption_fixture::build_sparse_slot_published_fixture(16)
            .expect("real sparse-slot fixture");
        assert_eq!(fixture.identities.len(), 16);
        assert!(
            fixture
                .identities
                .iter()
                .all(|identity| identity.slot & 0x8000_0000 != 0)
        );
        let ranked =
            ic_stable_lara::adoption_fixture::ranked_packed_blob_bytes(&fixture.identities, false)
                .expect("ranked bytes");
        let shared = SharedOrientationLookup::build(&fixture.identities, false)
            .expect("shared lookup")
            .encode()
            .expect("shared bytes")
            .len();
        println!("real_sparse_slots: ranked_bytes={ranked} shared_bytes={shared}");
        assert!(ranked > 0);
        assert!(shared > 0);
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn topology_fixture_gate_records_self_loop_bytes_and_unsupported_rows() {
        let directed = build_real_alias_fixture(FixtureSpec::required_matrix()[4])
            .expect("directed self-loop fixture");
        let directed_ids = directed
            .identities
            .iter()
            .map(
                |identity| ic_stable_lara::adoption_fixture::PhysicalIdentity {
                    owner: identity.owner,
                    target: identity.target,
                    orientation: identity.orientation,
                    slot: identity.slot as u32,
                },
            )
            .collect::<Vec<_>>();
        assert!(
            ic_stable_lara::adoption_fixture::ranked_packed_blob_bytes(&directed_ids, false)
                .expect("directed self-loop ranked bytes")
                > 0
        );

        let undirected = build_real_alias_fixture(FixtureSpec::required_matrix()[5])
            .expect("undirected self-loop fixture");
        let undirected_ids = undirected
            .identities
            .iter()
            .map(
                |identity| ic_stable_lara::adoption_fixture::PhysicalIdentity {
                    owner: identity.owner,
                    target: identity.target,
                    orientation: identity.orientation,
                    slot: identity.slot as u32,
                },
            )
            .collect::<Vec<_>>();
        let pair_rank =
            UndirectedPairRankLookup::build(&undirected_ids).expect("self-loop pair rank");
        assert_eq!(pair_rank.logical_bytes(), Some(8));

        assert!(build_real_alias_fixture(FixtureSpec::required_matrix()[7]).is_err());
        assert!(build_real_alias_fixture(FixtureSpec::required_matrix()[8]).is_err());
    }

    #[test]
    fn adaptive_policy_requires_both_byte_and_runtime_gates() {
        assert_eq!(
            select_compression_candidate(observation()),
            CompressionCandidate::RankedPacked
        );

        let mut bytes_fail = observation();
        bytes_fail.ranked_bytes = bytes_fail.alias_bytes + 1;
        assert_eq!(
            select_compression_candidate(bytes_fail),
            CompressionCandidate::ScanOnly
        );

        let mut runtime_fail = observation();
        runtime_fail.ranked_instructions = runtime_fail.scan_instructions + 1;
        assert_eq!(
            select_compression_candidate(runtime_fail),
            CompressionCandidate::ScanOnly
        );
    }

    #[test]
    fn adaptive_policy_accepts_compressed_candidate_only_with_monotone_proof() {
        let mut compressed = observation();
        compressed.monotone_rank_sequence = true;
        compressed.compressed_bytes = Some(1000);
        compressed.compressed_instructions = Some(1_000_000);
        assert_eq!(
            select_compression_candidate(compressed),
            CompressionCandidate::MonotoneCompressed
        );

        let mut non_monotone = compressed;
        non_monotone.monotone_rank_sequence = false;
        assert_eq!(
            select_compression_candidate(non_monotone),
            CompressionCandidate::RankedPacked
        );

        let mut shared = observation();
        shared.shared_bytes = Some(1800);
        shared.shared_instructions = Some(672_000);
        assert_eq!(
            select_compression_candidate(shared),
            CompressionCandidate::SharedOrientation
        );

        let mut shared_bytes_fail = shared;
        shared_bytes_fail.shared_bytes = Some(shared_bytes_fail.ranked_bytes + 1);
        assert_eq!(
            select_compression_candidate(shared_bytes_fail),
            CompressionCandidate::RankedPacked
        );

        let mut shared_runtime_fail = shared;
        shared_runtime_fail.shared_instructions = Some(shared_runtime_fail.ranked_instructions + 1);
        assert_eq!(
            select_compression_candidate(shared_runtime_fail),
            CompressionCandidate::RankedPacked
        );

        let mut undirected = observation();
        undirected.shared_bytes = None;
        undirected.shared_instructions = None;
        assert_eq!(
            select_compression_candidate(undirected),
            CompressionCandidate::RankedPacked
        );
    }

    #[test]
    fn measured_common_fixture_policy_prefers_shared_only_for_directed_shapes() {
        let directed = CompressionPolicyObservation {
            live_degree: 128,
            requests: 1024,
            monotone_rank_sequence: false,
            alias_bytes: 2304,
            scan_instructions: 13_720_000,
            ranked_bytes: 2840,
            ranked_instructions: 1_560_000,
            shared_bytes: Some(1800),
            shared_instructions: Some(672_220),
            compressed_bytes: None,
            compressed_instructions: None,
            pair_rank_bytes: None,
            pair_rank_instructions: None,
            exact_and_fail_closed: true,
        };
        assert_eq!(
            select_compression_candidate(directed),
            CompressionCandidate::SharedOrientation
        );

        let parallel = CompressionPolicyObservation {
            ranked_bytes: 128,
            ranked_instructions: 323_900,
            shared_bytes: Some(84),
            shared_instructions: Some(175_430),
            ..directed
        };
        assert_eq!(
            select_compression_candidate(parallel),
            CompressionCandidate::SharedOrientation
        );

        let undirected = CompressionPolicyObservation {
            ranked_bytes: 1560,
            ranked_instructions: 1_560_000,
            shared_bytes: None,
            shared_instructions: None,
            pair_rank_bytes: Some(1_544),
            pair_rank_instructions: Some(721_260),
            ..directed
        };
        assert_eq!(
            select_compression_candidate(undirected),
            CompressionCandidate::RankedPacked
        );
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn candidate_size_models_report_real_fixture_values() {
        for (name, spec, undirected) in [
            ("directed-high", FixtureSpec::required_matrix()[1], false),
            ("parallel-32", FixtureSpec::required_matrix()[6], false),
        ] {
            let (sequences, identities) = compression_sequences_for_spec(spec, undirected);
            let (delta, elias_fano, monotone_count, shared) =
                compression_candidate_probe(&sequences, &identities, undirected);
            println!(
                "{name}: delta_restart={delta} elias_fano={elias_fano} shared={shared} monotone={monotone_count}/{}",
                sequences.len()
            );
            assert!(delta > 0);
            assert!(monotone_count <= sequences.len() as u64);
        }
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn common_fixture_policy_reports_undirected_without_shared_orientation() {
        let spec = FixtureSpec::required_matrix()[3];
        let (_, identities) = compression_sequences_for_spec(spec, true);
        let ranked = ic_stable_lara::adoption_fixture::ranked_packed_blob_bytes(&identities, true)
            .expect("undirected rank bytes");
        println!(
            "{}: rank_indexed={} shared=unsupported scan_only=default",
            spec.id, ranked
        );
        assert!(ranked > 0);
        assert!(SharedOrientationLookup::build(&identities, true).is_err());
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn undirected_pair_rank_reports_identity_metadata_size() {
        let spec = FixtureSpec::required_matrix()[3];
        let (_, identities) = compression_sequences_for_spec(spec, true);
        let lookup = UndirectedPairRankLookup::build(&identities).expect("pair-rank fixture");
        let bytes = lookup.logical_bytes().expect("pair-rank bytes");
        println!(
            "{}: pair_rank_bytes={} bytes_per_edge={:.3}",
            spec.id,
            bytes,
            bytes as f64 / spec.logical_edges as f64
        );
        assert!(bytes > 0);
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn shared_orientation_round_trip_matches_canonical_counterparts() {
        for spec in [
            FixtureSpec::required_matrix()[1],
            FixtureSpec::required_matrix()[6],
        ] {
            let (_, identities) = compression_sequences_for_spec(spec, false);
            let lookup =
                SharedOrientationLookup::build(&identities, false).expect("shared fixture lookup");
            let encoded = lookup.encode().expect("shared fixture encode");
            let decoded = SharedOrientationLookup::decode(&encoded).expect("shared fixture decode");
            for identity in &identities {
                let mut source_slots = identities
                    .iter()
                    .filter(|other| {
                        other.owner == identity.owner
                            && other.target == identity.target
                            && other.orientation == identity.orientation
                    })
                    .map(|other| other.slot)
                    .collect::<Vec<_>>();
                source_slots.sort_unstable();
                let rank = source_slots
                    .binary_search(&identity.slot)
                    .expect("source rank") as u32;
                let mut counterparts = identities
                    .iter()
                    .filter(|other| {
                        other.owner == identity.target
                            && other.target == identity.owner
                            && other.orientation != identity.orientation
                    })
                    .map(|other| other.slot)
                    .collect::<Vec<_>>();
                counterparts.sort_unstable();
                assert_eq!(
                    decoded.lookup(identity.owner, identity.target, rank),
                    counterparts.get(rank as usize).copied()
                );
                assert_eq!(
                    decoded.lookup(identity.owner, identity.target, rank + 1_000_000),
                    None
                );
            }
        }
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn sampled_paired_residual_reports_block_size_series() {
        for (name, spec) in [
            ("directed-high", FixtureSpec::required_matrix()[1]),
            ("parallel-32", FixtureSpec::required_matrix()[6]),
            (
                "parallel-128",
                FixtureSpec {
                    id: "parallel-128",
                    shape: FixtureShape::Parallel,
                    logical_edges: 128,
                    physical_half_edges: 256,
                    degree: 128,
                },
            ),
            (
                "parallel-256",
                FixtureSpec {
                    id: "parallel-256",
                    shape: FixtureShape::Parallel,
                    logical_edges: 256,
                    physical_half_edges: 512,
                    degree: 256,
                },
            ),
        ] {
            let (_, identities) = compression_sequences_for_spec(spec, false);
            for block_size in [8usize, 16, 32, 64] {
                let lookup = SampledPairedResidualLookup::build(&identities, block_size)
                    .expect("sampled fixture lookup");
                let bytes = lookup.logical_bytes().expect("sampled bytes");
                let encoded = lookup.encode().expect("sampled fixture encode");
                let decoded = SampledPairedResidualLookup::decode(&encoded, block_size)
                    .expect("sampled fixture decode");
                println!(
                    "{name}: block={block_size} bytes={bytes} bytes_per_edge={:.3}",
                    bytes as f64 / spec.logical_edges as f64
                );
                assert!(bytes > 0);
                assert_eq!(u64::try_from(encoded.len()).expect("encoded length"), bytes);
                let identity = identities
                    .iter()
                    .find(|identity| identity.orientation == 0)
                    .expect("forward identity");
                assert_eq!(
                    decoded.lookup(identity.owner, identity.target, 0, identity.slot),
                    lookup.lookup(identity.owner, identity.target, 0, identity.slot)
                );
                let source_slots = identities
                    .iter()
                    .filter(|other| {
                        other.owner == identity.owner
                            && other.target == identity.target
                            && other.orientation == identity.orientation
                    })
                    .map(|other| other.slot)
                    .collect::<Vec<_>>();
                assert_eq!(
                    decoded.lookup_local_scan(identity.owner, identity.target, 0, &source_slots,),
                    decoded.lookup(identity.owner, identity.target, 0, identity.slot)
                );
            }
        }
    }

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
    fn required_fixture_specs_map_to_typed_ids_without_substitution() {
        for spec in FixtureSpec::required_matrix() {
            assert!(AdoptionFixtureId::from_fixture_id(spec.id).is_some());
        }
        assert_eq!(AdoptionFixtureId::from_fixture_id("unknown_topology"), None);
    }

    #[test]
    fn current_evidence_inventory_is_explicitly_hold_until_rows_are_connected() {
        let rows = current_adoption_evidence_inventory();
        assert_eq!(rows.len(), REQUIRED_ADOPTION_FIXTURE_IDS.len());
        assert_eq!(current_adoption_status(), AdoptionStatus::Hold);
        assert!(rows.iter().any(|row| {
            matches!(row.disposition, AdoptionDisposition::Deferred) && !row.evidence_present
        }));
        assert!(rows.iter().any(|row| {
            matches!(row.disposition, AdoptionDisposition::ScanOnly) && row.evidence_present
        }));
    }

    #[test]
    fn observation_constructor_uses_candidate_specific_metrics() {
        let mut directed = observation();
        directed.shared_bytes = Some(1_800);
        directed.shared_instructions = Some(672_220);
        let directed_row =
            adoption_row_from_observation(AdoptionFixtureId::DirectedHigh, directed, true, true);
        assert_eq!(
            directed_row.disposition,
            AdoptionDisposition::SharedOrientation
        );
        assert!(directed_row.evidence_present);
        assert!(directed_row.logical_bytes_pass && directed_row.runtime_pass);

        let mut undirected = observation();
        undirected.pair_rank_bytes = Some(1_544);
        undirected.pair_rank_instructions = Some(721_260);
        let undirected_row = adoption_row_from_observation(
            AdoptionFixtureId::UndirectedHigh,
            undirected,
            true,
            true,
        );
        assert_eq!(undirected_row.disposition, AdoptionDisposition::PairRank);
        assert!(undirected_row.logical_bytes_pass && undirected_row.runtime_pass);

        undirected.pair_rank_bytes = Some(undirected.alias_bytes + 1);
        let failed = adoption_row_from_observation(
            AdoptionFixtureId::UndirectedHigh,
            undirected,
            true,
            true,
        );
        assert_eq!(failed.disposition, AdoptionDisposition::ScanOnly);
        assert!(failed.evidence_present);
        assert!(!failed.logical_bytes_pass && !failed.runtime_pass);
    }

    #[test]
    fn matched_probe_owns_exactness_and_fallback_claims() {
        let mut candidate = observation();
        candidate.shared_bytes = Some(1_800);
        candidate.shared_instructions = Some(672_220);
        let row = MatchedAdoptionProbe {
            observation: candidate,
            exact_results: true,
            fallback_safe: true,
        }
        .into_row(AdoptionFixtureId::DirectedHigh);
        assert!(row.evidence_present);
        assert!(row.exact_results && row.fallback_safe);
        assert_eq!(row.disposition, AdoptionDisposition::SharedOrientation);

        let unsafe_row = MatchedAdoptionProbe {
            observation: candidate,
            exact_results: true,
            fallback_safe: false,
        }
        .into_row(AdoptionFixtureId::DirectedHigh);
        assert!(!unsafe_row.fallback_safe);
        assert_eq!(
            aggregate_adoption_status(&[
                unsafe_row,
                AdoptionEvidenceRow {
                    fixture_id: AdoptionFixtureId::DirectedLow,
                    disposition: AdoptionDisposition::ScanOnly,
                    evidence_present: true,
                    exact_results: true,
                    fallback_safe: true,
                    logical_bytes_pass: true,
                    runtime_pass: true,
                },
            ]),
            AdoptionStatus::Hold
        );
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn real_candidate_probes_match_canonical_and_fail_closed() {
        let (directed, directed_refs) = published_directed_runtime_fixture();
        let shared = SharedOrientationLookup::build(&directed.identities, false)
            .expect("directed shared candidate");
        for edge in directed_refs {
            let canonical = directed
                .graph
                .mate_of(edge)
                .expect("canonical directed mate");
            let rank = shared
                .rank_for(
                    u32::from(edge.owner_vertex_id),
                    u32::from(canonical.owner_vertex_id),
                    edge.slot_index.raw(),
                )
                .expect("shared source rank");
            assert_eq!(
                shared.lookup(
                    u32::from(edge.owner_vertex_id),
                    u32::from(canonical.owner_vertex_id),
                    rank,
                ),
                Some(canonical.slot_index.raw())
            );
        }
        assert!(shared.lookup(0, 1, u32::MAX).is_none());
        assert!(shared.lookup(u32::MAX, 0, 0).is_none());

        let (parallel, parallel_refs) = published_parallel_runtime_fixture();
        let parallel_shared = SharedOrientationLookup::build(&parallel.identities, false)
            .expect("parallel shared candidate");
        for edge in parallel_refs {
            let canonical = parallel
                .graph
                .mate_of(edge)
                .expect("canonical parallel mate");
            let rank = parallel_shared
                .rank_for(
                    u32::from(edge.owner_vertex_id),
                    u32::from(canonical.owner_vertex_id),
                    edge.slot_index.raw(),
                )
                .expect("parallel source rank");
            assert_eq!(
                parallel_shared.lookup(
                    u32::from(edge.owner_vertex_id),
                    u32::from(canonical.owner_vertex_id),
                    rank,
                ),
                Some(canonical.slot_index.raw())
            );
        }
        assert!(parallel_shared.lookup(0, 1, u32::MAX).is_none());

        let sparse = ic_stable_lara::adoption_fixture::build_sparse_slot_published_fixture(64)
            .expect("sparse candidate fixture");
        let sparse_shared = SharedOrientationLookup::build(&sparse.identities, false)
            .expect("sparse shared candidate");
        let sparse_label = ic_stable_lara::labeled::BucketLabelKey::directed_from_index(1);
        for identity in &sparse.identities {
            let edge = ic_stable_lara::labeled::CanonicalEdgeOccurrence {
                orientation: if identity.orientation == 0 {
                    ic_stable_lara::labeled::LabeledOrientation::Forward
                } else {
                    ic_stable_lara::labeled::LabeledOrientation::Reverse
                },
                owner_vertex_id: ic_stable_lara::VertexId::from(identity.owner),
                label_id: sparse_label,
                slot_index: identity.slot.into(),
            };
            // The canonical mate scan intentionally addresses slab slots only. Sparse fixture
            // identities retain overflow-log locations (high-bit slots), so the independent
            // fixture identity relation is the oracle for this physical-location probe.
            let mut counterparts = sparse
                .identities
                .iter()
                .filter(|other| {
                    other.owner == identity.target
                        && other.target == identity.owner
                        && other.orientation != identity.orientation
                })
                .collect::<Vec<_>>();
            counterparts.sort_unstable_by_key(|other| other.slot);
            let mut sources = sparse
                .identities
                .iter()
                .filter(|other| {
                    other.owner == identity.owner
                        && other.target == identity.target
                        && other.orientation == identity.orientation
                })
                .collect::<Vec<_>>();
            sources.sort_unstable_by_key(|other| other.slot);
            let source_rank = sources
                .iter()
                .position(|other| other.slot == identity.slot)
                .expect("sparse source rank");
            let expected_slot = counterparts
                .get(source_rank)
                .expect("sparse counterpart rank")
                .slot;
            let rank = sparse_shared
                .rank_for(
                    u32::from(edge.owner_vertex_id),
                    identity.target,
                    edge.slot_index.raw(),
                )
                .expect("sparse source rank");
            assert_eq!(
                sparse_shared.lookup(u32::from(edge.owner_vertex_id), identity.target, rank,),
                Some(expected_slot)
            );
        }
        assert!(sparse_shared.lookup(0, 1, u32::MAX).is_none());

        let mixed = ic_stable_lara::adoption_fixture::build_mixed_label_published_fixture(2, 128)
            .expect("mixed-label candidate fixture");
        for label_raw in [1u16, 2u16] {
            let label = ic_stable_lara::labeled::BucketLabelKey::from_raw(label_raw);
            let label_identities = mixed
                .identities
                .iter()
                .filter(|identity| identity.label == label_raw)
                .map(
                    |identity| ic_stable_lara::adoption_fixture::PhysicalIdentity {
                        owner: identity.owner,
                        target: identity.target,
                        orientation: identity.orientation,
                        slot: identity.slot,
                    },
                )
                .collect::<Vec<_>>();
            let label_shared = SharedOrientationLookup::build(&label_identities, false)
                .expect("mixed-label shared candidate");
            for identity in mixed
                .identities
                .iter()
                .filter(|identity| identity.label == label_raw)
            {
                let edge = ic_stable_lara::labeled::CanonicalEdgeOccurrence {
                    orientation: if identity.orientation == 0 {
                        ic_stable_lara::labeled::LabeledOrientation::Forward
                    } else {
                        ic_stable_lara::labeled::LabeledOrientation::Reverse
                    },
                    owner_vertex_id: ic_stable_lara::VertexId::from(identity.owner),
                    label_id: label,
                    slot_index: identity.slot.into(),
                };
                let canonical = mixed
                    .graph
                    .mate_of(edge)
                    .expect("canonical mixed-label mate");
                let rank = label_shared
                    .rank_for(
                        u32::from(edge.owner_vertex_id),
                        u32::from(canonical.owner_vertex_id),
                        edge.slot_index.raw(),
                    )
                    .expect("mixed-label source rank");
                assert_eq!(
                    label_shared.lookup(
                        u32::from(edge.owner_vertex_id),
                        u32::from(canonical.owner_vertex_id),
                        rank,
                    ),
                    Some(canonical.slot_index.raw())
                );
            }
            assert!(label_shared.lookup(0, 1, u32::MAX).is_none());
        }

        let (undirected, undirected_refs) = published_undirected_runtime_fixture();
        let pair_rank = UndirectedPairRankLookup::build(&undirected.identities)
            .expect("undirected pair-rank candidate");
        for edge in undirected_refs {
            let canonical = undirected
                .graph
                .mate_of(edge)
                .expect("canonical undirected mate");
            let rank = undirected
                .identities
                .iter()
                .filter(|identity| {
                    identity.owner == u32::from(edge.owner_vertex_id)
                        && identity.target == u32::from(canonical.owner_vertex_id)
                        && identity.slot <= edge.slot_index.raw()
                })
                .count()
                .checked_sub(1)
                .expect("pair-rank source rank") as u32;
            assert_eq!(
                pair_rank.lookup(
                    u32::from(edge.owner_vertex_id),
                    u32::from(canonical.owner_vertex_id),
                    rank,
                ),
                Some(canonical.slot_index.raw())
            );
        }
        assert!(pair_rank.lookup(0, 1, u32::MAX).is_none());
        assert!(pair_rank.lookup(u32::MAX, 0, 0).is_none());
    }

    #[test]
    fn committed_probe_snapshots_populate_known_non_deterministic_rows() {
        let mut directed = observation();
        directed.alias_bytes = 2_304;
        directed.scan_instructions = 13_720_000;
        directed.ranked_bytes = 2_840;
        directed.ranked_instructions = 1_560_000;
        directed.shared_bytes = Some(1_800);
        directed.shared_instructions = Some(672_220);
        let directed_row =
            adoption_row_from_observation(AdoptionFixtureId::DirectedHigh, directed, true, true);
        assert_eq!(
            directed_row.disposition,
            AdoptionDisposition::SharedOrientation
        );

        let mut parallel = observation();
        parallel.alias_bytes = 576;
        parallel.scan_instructions = 43_724_263;
        parallel.ranked_bytes = 128;
        parallel.ranked_instructions = 323_900;
        parallel.shared_bytes = Some(84);
        parallel.shared_instructions = Some(175_430);
        let parallel_row =
            adoption_row_from_observation(AdoptionFixtureId::Parallel, parallel, true, true);
        assert_eq!(
            parallel_row.disposition,
            AdoptionDisposition::SharedOrientation
        );

        let mut undirected = observation();
        undirected.alias_bytes = 2_304;
        undirected.scan_instructions = 17_302_087;
        undirected.ranked_bytes = 1_560;
        undirected.ranked_instructions = 945_735;
        undirected.pair_rank_bytes = Some(1_544);
        undirected.pair_rank_instructions = Some(721_260);
        let undirected_row = adoption_row_from_observation(
            AdoptionFixtureId::UndirectedHigh,
            undirected,
            true,
            true,
        );
        assert_eq!(undirected_row.disposition, AdoptionDisposition::PairRank);

        let mut mixed = observation();
        mixed.alias_bytes = 144;
        mixed.scan_instructions = 15_940_000;
        mixed.ranked_bytes = 96;
        mixed.ranked_instructions = 594_287;
        mixed.shared_bytes = Some(52);
        mixed.shared_instructions = Some(350_590);
        let mixed_row =
            adoption_row_from_observation(AdoptionFixtureId::MixedLabelsLow, mixed, true, true);
        assert_eq!(
            mixed_row.disposition,
            AdoptionDisposition::SharedOrientation
        );
        assert!(directed_row.logical_bytes_pass);
        assert!(parallel_row.logical_bytes_pass);
        assert!(undirected_row.logical_bytes_pass);
        assert!(mixed_row.logical_bytes_pass);

        let mut sparse = observation();
        sparse.alias_bytes = 576;
        sparse.scan_instructions = 45_590_000;
        sparse.ranked_bytes = 128;
        sparse.ranked_instructions = 300_350;
        sparse.shared_bytes = Some(84);
        sparse.shared_instructions = Some(175_430);
        let sparse_row =
            adoption_row_from_observation(AdoptionFixtureId::SparseSlots, sparse, true, true);
        assert_eq!(
            sparse_row.disposition,
            AdoptionDisposition::SharedOrientation
        );
        assert!(sparse_row.logical_bytes_pass && sparse_row.runtime_pass);
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
            FixtureSpec::required_matrix()[4],
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
            if matches!(
                spec.shape,
                FixtureShape::Directed | FixtureShape::DirectedSelfLoop | FixtureShape::Parallel
            ) {
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
    fn real_scan_fixture_uses_independent_canonical_rows() {
        let spec = FixtureSpec::required_matrix()[0];
        let fixture = build_real_scan_fixture(spec).expect("real ScanOnly fixture");
        assert_eq!(fixture.identities.len() as u64, spec.physical_half_edges);
        assert_eq!(
            fixture.descriptor.fixture_ids,
            vec!["directed_low-scan-only"]
        );
        assert!(fixture.identities.windows(2).all(|pair| pair[0] != pair[1]));
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn real_published_fixture_uses_promotion_eligible_directed_high_topology() {
        let spec = FixtureSpec::required_matrix()[1];
        let fixture = build_real_published_fixture(spec).expect("published fixture");
        assert_eq!(fixture.identities.len() as u64, spec.physical_half_edges);
        assert_eq!(
            fixture.descriptor.fixture_ids,
            vec!["directed_high-published"]
        );
        assert!(build_real_published_fixture(FixtureSpec::required_matrix()[0]).is_err());

        let parallel = build_real_published_fixture(FixtureSpec::required_matrix()[6])
            .expect("parallel published fixture");
        assert_eq!(parallel.identities.len() as u64, 64);
        assert_eq!(parallel.descriptor.fixture_ids, vec!["parallel-published"]);

        let undirected = build_real_published_fixture(FixtureSpec::required_matrix()[3])
            .expect("undirected published fixture");
        assert_eq!(undirected.identities.len() as u64, 256);
        assert_eq!(
            undirected.descriptor.fixture_ids,
            vec!["undirected_high-published"]
        );
        assert!(build_real_published_fixture(FixtureSpec::required_matrix()[2]).is_err());
        assert!(build_real_published_fixture(FixtureSpec::required_matrix()[5]).is_err());
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn evidence_uses_real_digest_for_real_topology_shapes() {
        let parallel = build_evidence_fixture(FixtureSpec::required_matrix()[6]);
        assert!(parallel.1.is_some());
        assert!(parallel.2.is_some_and(|bytes| bytes > 0));

        let sparse = build_evidence_fixture(FixtureSpec::required_matrix()[7]);
        assert!(sparse.1.is_some());
        assert!(sparse.2.is_some_and(|bytes| bytes > 0));
        assert_eq!(sparse.0.fixture_ids, vec!["sparse_slots-published"]);

        let mixed = build_evidence_fixture(FixtureSpec::required_matrix()[8]);
        assert!(mixed.1.is_some());
        assert!(mixed.2.is_some_and(|bytes| bytes > 0));
        assert_eq!(mixed.0.fixture_ids, vec!["mixed_labels_low-published"]);
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
                canonical_identity_bytes: None,
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
                canonical_identity_bytes: Some(canonical_identity_encoded_bytes(
                    &fixture.identities,
                )),
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

    #[test]
    fn evidence_rejects_mismatched_identity_digest_and_byte_length() {
        let fixture = build_fixture(FixtureSpec::required_matrix()[0]);
        let artifact = EvidenceArtifact {
            schema_version: 1,
            policy_version: POLICY_VERSION.to_owned(),
            fixture_generator: 1,
            corpus_seed: 1,
            corpus_generator: 1,
            shape_descriptors: vec![fixture.descriptor.clone()],
            corpus_generated_count: 0,
            rows: vec![EvidenceRow {
                shape_id: fixture.descriptor.shape_id,
                fixture_id: fixture.descriptor.fixture_ids[0].clone(),
                status: EvidenceStatus::Deferred,
                policy_version: POLICY_VERSION.to_owned(),
                canonical_identity_digest: Some("a".repeat(64)),
                canonical_identity_bytes: None,
                request_identity: None,
                instruction_total: None,
                exact_result_status: None,
            }],
        };
        assert_eq!(
            artifact.validate(),
            Err("identity digest/byte length mismatch")
        );
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn published_runtime_matches_canonical_for_all_fixture_rows() {
        let (fixture, refs) = published_directed_runtime_fixture();
        let before = fixture.identities.clone();
        for edge in refs {
            let expected = fixture.graph.mate_of(edge).expect("canonical mate");
            let actual = fixture
                .graph
                .published_mate_of(edge)
                .expect("published mate");
            assert_eq!(actual, expected);
        }
        assert_eq!(fixture.identities, before);
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn published_runtime_matches_canonical_for_parallel_and_undirected_rows() {
        for (fixture, refs) in [
            published_parallel_runtime_fixture(),
            published_undirected_runtime_fixture(),
        ] {
            for edge in refs {
                let expected = fixture.graph.mate_of(edge).expect("canonical mate");
                let actual = fixture
                    .graph
                    .published_mate_of(edge)
                    .expect("published mate");
                assert_eq!(actual, expected);
            }
        }
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn scan_runtime_fixture_covers_required_directed_cardinalities() {
        for logical_edges in [32, 128, 256] {
            let (fixture, refs) =
                published_directed_runtime_fixture_with_logical_edges(logical_edges);
            assert_eq!(refs.len(), (logical_edges as usize) * 2);
            for edge in refs.iter().take(8) {
                let canonical = fixture.graph.mate_of(*edge).expect("canonical mate");
                assert!(canonical.slot_index.raw() < logical_edges);
            }
        }
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn scan_runtime_fixture_covers_parallel_degree_series() {
        for logical_edges in [32, 128, 256] {
            let (fixture, refs) = published_parallel_runtime_fixture_with_edges(logical_edges);
            assert_eq!(refs.len(), (logical_edges as usize) * 2);
            let mate = fixture.graph.mate_of(refs[0]).expect("canonical mate");
            assert_eq!(mate.owner_vertex_id, ic_stable_lara::VertexId::from(1));
        }
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn byte_footprint_report_separates_alias_and_published_bytes() {
        let rows = byte_footprint_report();
        let directed_high = rows
            .iter()
            .find(|row| row.shape_id == "directed_high")
            .expect("directed-high row");
        assert_eq!(directed_high.alias_raw_bytes, 128 * 18);
        assert!(
            directed_high
                .published_blob_bytes
                .is_some_and(|bytes| bytes > 0)
        );
        assert!(directed_high.sampled_known_bytes > 0);
        assert!(directed_high.packed_known_bytes > 0);

        let directed_self_loop = rows
            .iter()
            .find(|row| row.shape_id == "directed_self_loop")
            .expect("directed self-loop row");
        assert_eq!(directed_self_loop.alias_raw_bytes, 0);
        assert!(directed_self_loop.published_blob_bytes.is_none());
    }

    #[cfg(feature = "canbench")]
    #[test]
    fn published_size_series_is_monotonic_and_shape_separated() {
        let rows = published_size_series();
        assert!(rows.iter().any(|row| row.topology == "directed"));
        assert!(rows.iter().any(|row| row.topology == "undirected"));
        assert!(rows.iter().any(|row| row.topology == "parallel"));
        for topology in ["directed", "undirected", "parallel"] {
            let mut series = rows
                .iter()
                .filter(|row| row.topology == topology)
                .collect::<Vec<_>>();
            series.sort_by_key(|row| row.logical_edges);
            assert!(series.windows(2).all(|pair| {
                pair[0].logical_edges < pair[1].logical_edges
                    && pair[0].published_blob_bytes <= pair[1].published_blob_bytes
                    && pair[0].bucket_count > 0
                    && pair[0].published_mate_storage_pages > 0
            }));
        }
    }
}
