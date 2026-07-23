//! Read-only, bounded admission for one orientation/PMA leaf mate blob.
//!
//! This module deliberately stops before canonical enumeration and publication.  The input
//! mapping is a pure builder output supplied by the owner; this boundary only validates the
//! bucket-local scan gates and applies one leaf-owned shared-overhead budget using the codec as
//! the encoded-size source of truth.

use super::mate_blob_prototype::{Bucket, EncodeError, MateBlob, Mode};
use crate::{VertexId, labeled::BucketLabelKey};

/// Bucket-local observations used by the read-only admission prefilter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MatePromotionInputs {
    pub owner_vertex_id: VertexId,
    pub bucket_label_key: BucketLabelKey,
    pub live_entries: u64,
    pub source_scan_rows: u64,
    pub counterpart_scan_rows: u64,
    pub sampled_stride: u8,
    pub packed_width_bytes: u8,
    pub min_scan_rows: u64,
}

/// Leaf-owned limits. Shared overhead is charged once per candidate blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MateLeafPromotionConfig {
    pub leaf_shared_overhead_bytes: u64,
    pub max_encoded_blob_bytes: u64,
    pub max_total_promotion_bytes: u64,
    pub max_bytes_per_entry: u64,
}

/// A pure candidate consisting of bucket observations and canonical pair slots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MatePromotionCandidate {
    pub inputs: MatePromotionInputs,
    pub source_slots: Vec<u32>,
    pub mate_slots: Vec<u32>,
}

/// Canonical live pair rows for one bucket, in equal-neighbor ordinal order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MatePromotionRows {
    pub inputs: MatePromotionInputs,
    pub source_slots: Vec<u32>,
    pub mate_slots: Vec<u32>,
}

/// One leaf-owned admission plan. The config is carried once for the whole leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MateLeafPromotionPlan {
    candidates: Vec<MatePromotionCandidate>,
    config: MateLeafPromotionConfig,
}

impl MateLeafPromotionPlan {
    pub(crate) fn new(
        candidates: Vec<MatePromotionCandidate>,
        config: MateLeafPromotionConfig,
    ) -> Self {
        Self { candidates, config }
    }

    pub(crate) fn decide(&self) -> MateLeafPromotionDecision {
        decide_leaf_promotion(&self.candidates, self.config)
    }
}

/// Common mode selected for all buckets in one published blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatePromotionMode {
    Sampled { stride: u8 },
    Packed { width_bytes: u8 },
}

impl MatePromotionMode {
    fn codec(self) -> Mode {
        match self {
            Self::Sampled { stride } => Mode::Sampled { stride },
            Self::Packed { width_bytes } => Mode::Packed { width_bytes },
        }
    }
}

/// Why a leaf remains ScanOnly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatePromotionRejectReason {
    InvalidConfig,
    NoEligibleBuckets,
    UnsupportedParameters,
    CodecRejected,
    EncodedLimit,
    TotalLimit,
    BytesPerEntryLimit,
    ArithmeticOverflow,
}

/// Errors raised while turning canonical rows into a checked codec blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MateBlobBuildError {
    MissingSelectedBucket,
    DuplicateSelectedBucket,
    RowCountMismatch,
    CanonicalMismatch,
    UnsupportedMode,
    SlotDoesNotFit,
    Codec(EncodeError),
    LengthMismatch,
    AdmissionSizeMismatch,
}

/// The only mode-selection result exposed by the admission boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MateLeafPromotionDecision {
    ScanOnly {
        reason: MatePromotionRejectReason,
    },
    Promote {
        mode: MatePromotionMode,
        config: MateLeafPromotionConfig,
        bucket_ids: Vec<(VertexId, BucketLabelKey)>,
        encoded_blob_bytes: u64,
        total_promotion_bytes: u64,
    },
}

pub(crate) fn supported_sampled_stride(stride: u8) -> bool {
    matches!(stride, 16 | 32 | 64)
}

pub(crate) fn supported_packed_width(width: u8) -> bool {
    (1..=4).contains(&width)
}

pub(crate) fn valid_config(config: MateLeafPromotionConfig) -> bool {
    config.max_encoded_blob_bytes != 0
        && config.max_total_promotion_bytes != 0
        && config.max_bytes_per_entry != 0
}

fn prefilter(candidate: &MatePromotionCandidate) -> Result<bool, MatePromotionRejectReason> {
    let inputs = candidate.inputs;
    let live_len = usize::try_from(inputs.live_entries)
        .map_err(|_| MatePromotionRejectReason::ArithmeticOverflow)?;
    if candidate.source_slots.len() != live_len || candidate.mate_slots.len() != live_len {
        return Err(MatePromotionRejectReason::CodecRejected);
    }
    if inputs.live_entries < 2 {
        return Ok(false);
    }
    let scan_rows = inputs
        .source_scan_rows
        .checked_add(inputs.counterpart_scan_rows)
        .ok_or(MatePromotionRejectReason::ArithmeticOverflow)?;
    if inputs.min_scan_rows == 0 || scan_rows < inputs.min_scan_rows {
        return Ok(false);
    }
    if !supported_sampled_stride(inputs.sampled_stride)
        && !supported_packed_width(inputs.packed_width_bytes)
    {
        return Err(MatePromotionRejectReason::UnsupportedParameters);
    }
    Ok(true)
}

fn evaluate_group(
    candidates: &[&MatePromotionCandidate],
    mode: MatePromotionMode,
    config: MateLeafPromotionConfig,
) -> Result<Option<MateLeafPromotionDecision>, MatePromotionRejectReason> {
    if candidates.is_empty() {
        return Ok(None);
    }
    let buckets = candidates
        .iter()
        .map(|candidate| {
            let mapping = build_mapping(mode, &candidate.source_slots, &candidate.mate_slots)
                .map_err(|error| match error {
                    MateBlobBuildError::UnsupportedMode | MateBlobBuildError::SlotDoesNotFit => {
                        MatePromotionRejectReason::UnsupportedParameters
                    }
                    MateBlobBuildError::RowCountMismatch => {
                        MatePromotionRejectReason::CodecRejected
                    }
                    MateBlobBuildError::MissingSelectedBucket
                    | MateBlobBuildError::DuplicateSelectedBucket
                    | MateBlobBuildError::CanonicalMismatch
                    | MateBlobBuildError::Codec(_)
                    | MateBlobBuildError::LengthMismatch
                    | MateBlobBuildError::AdmissionSizeMismatch => {
                        MatePromotionRejectReason::CodecRejected
                    }
                })?;
            Ok(Bucket {
                owner_vertex_id: u32::from(candidate.inputs.owner_vertex_id),
                bucket_label_key: candidate.inputs.bucket_label_key.raw(),
                entries: u32::try_from(candidate.inputs.live_entries)
                    .map_err(|_| MatePromotionRejectReason::ArithmeticOverflow)?,
                mode: mode.codec(),
                mapping,
            })
        })
        .collect::<Result<Vec<_>, MatePromotionRejectReason>>()?;
    let blob = MateBlob { buckets };
    let encoded = blob.encoded_len().map_err(|error| match error {
        EncodeError::ArithmeticOverflow | EncodeError::TooLarge => {
            MatePromotionRejectReason::ArithmeticOverflow
        }
        _ => MatePromotionRejectReason::CodecRejected,
    })?;
    let encoded_u64 =
        u64::try_from(encoded).map_err(|_| MatePromotionRejectReason::ArithmeticOverflow)?;
    let total = encoded_u64
        .checked_add(config.leaf_shared_overhead_bytes)
        .ok_or(MatePromotionRejectReason::ArithmeticOverflow)?;
    let entries = candidates.iter().try_fold(0u64, |sum, candidate| {
        sum.checked_add(candidate.inputs.live_entries)
            .ok_or(MatePromotionRejectReason::ArithmeticOverflow)
    })?;
    let per_entry = config
        .max_bytes_per_entry
        .checked_mul(entries)
        .ok_or(MatePromotionRejectReason::ArithmeticOverflow)?;
    if encoded_u64 > config.max_encoded_blob_bytes {
        return Ok(None);
    }
    if total > config.max_total_promotion_bytes {
        return Ok(None);
    }
    if total > per_entry {
        return Ok(None);
    }
    Ok(Some(MateLeafPromotionDecision::Promote {
        mode,
        config,
        bucket_ids: candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.inputs.owner_vertex_id,
                    candidate.inputs.bucket_label_key,
                )
            })
            .collect(),
        encoded_blob_bytes: encoded_u64,
        total_promotion_bytes: total,
    }))
}

/// Selects the smallest valid common-mode candidate without mutating LARA or stable memory.
pub(crate) fn decide_leaf_promotion(
    candidates: &[MatePromotionCandidate],
    config: MateLeafPromotionConfig,
) -> MateLeafPromotionDecision {
    if !valid_config(config) {
        return MateLeafPromotionDecision::ScanOnly {
            reason: MatePromotionRejectReason::InvalidConfig,
        };
    }
    let mut eligible = Vec::new();
    for candidate in candidates {
        match prefilter(candidate) {
            Ok(true) => eligible.push(candidate),
            Ok(false) => {}
            Err(MatePromotionRejectReason::UnsupportedParameters) => {}
            Err(reason) => return MateLeafPromotionDecision::ScanOnly { reason },
        }
    }
    if eligible.is_empty() {
        return MateLeafPromotionDecision::ScanOnly {
            reason: MatePromotionRejectReason::NoEligibleBuckets,
        };
    }

    let mut decisions = Vec::new();
    for stride in [16, 32, 64] {
        let group = eligible
            .iter()
            .filter(|candidate| candidate.inputs.sampled_stride == stride)
            .copied()
            .collect::<Vec<_>>();
        if let Ok(Some(decision)) =
            evaluate_group(&group, MatePromotionMode::Sampled { stride }, config)
        {
            decisions.push(decision);
        }
    }
    for width in 1..=4 {
        let group = eligible
            .iter()
            .filter(|candidate| candidate.inputs.packed_width_bytes == width)
            .copied()
            .collect::<Vec<_>>();
        if let Ok(Some(decision)) = evaluate_group(
            &group,
            MatePromotionMode::Packed { width_bytes: width },
            config,
        ) {
            decisions.push(decision);
        }
    }
    decisions
        .into_iter()
        .min_by_key(|decision| match decision {
            MateLeafPromotionDecision::Promote {
                total_promotion_bytes,
                ..
            } => *total_promotion_bytes,
            MateLeafPromotionDecision::ScanOnly { .. } => u64::MAX,
        })
        .unwrap_or(MateLeafPromotionDecision::ScanOnly {
            reason: MatePromotionRejectReason::TotalLimit,
        })
}

fn append_slot(mapping: &mut Vec<u8>, slot: u32, width: u8) -> Result<(), MateBlobBuildError> {
    let max = match width {
        1 => u32::from(u8::MAX),
        2 => u32::from(u16::MAX),
        3 => 0x00FF_FFFF,
        4 => u32::MAX,
        _ => return Err(MateBlobBuildError::UnsupportedMode),
    };
    if slot > max {
        return Err(MateBlobBuildError::SlotDoesNotFit);
    }
    let bytes = slot.to_be_bytes();
    mapping.extend_from_slice(&bytes[4 - usize::from(width)..]);
    Ok(())
}

fn build_mapping(
    mode: MatePromotionMode,
    source_slots: &[u32],
    mate_slots: &[u32],
) -> Result<Vec<u8>, MateBlobBuildError> {
    if source_slots.len() != mate_slots.len() {
        return Err(MateBlobBuildError::RowCountMismatch);
    }
    let mut mapping = Vec::new();
    match mode {
        MatePromotionMode::Sampled { stride } if supported_sampled_stride(stride) => {
            for index in (0..source_slots.len()).step_by(usize::from(stride)) {
                append_slot(&mut mapping, source_slots[index], 4)?;
                append_slot(&mut mapping, mate_slots[index], 4)?;
            }
        }
        MatePromotionMode::Packed { width_bytes } if supported_packed_width(width_bytes) => {
            for (&source, &mate) in source_slots.iter().zip(mate_slots) {
                append_slot(&mut mapping, source, width_bytes)?;
                append_slot(&mut mapping, mate, width_bytes)?;
            }
        }
        _ => return Err(MateBlobBuildError::UnsupportedMode),
    }
    Ok(mapping)
}

/// Builds and validates one complete leaf blob from canonical live pair rows.
pub(crate) fn build_promoted_blob(
    decision: &MateLeafPromotionDecision,
    rows: &[MatePromotionRows],
) -> Result<(MateBlob, Vec<u8>), MateBlobBuildError> {
    let result = build_promoted_blob_unchecked(decision, rows)?;
    let MateLeafPromotionDecision::Promote {
        encoded_blob_bytes,
        total_promotion_bytes,
        ..
    } = decision
    else {
        unreachable!("unchecked builder accepts Promote only");
    };
    let encoded_len =
        u64::try_from(result.1.len()).map_err(|_| MateBlobBuildError::LengthMismatch)?;
    let config = match decision {
        MateLeafPromotionDecision::Promote { config, .. } => config,
        MateLeafPromotionDecision::ScanOnly { .. } => unreachable!(),
    };
    let expected_total = encoded_len
        .checked_add(config.leaf_shared_overhead_bytes)
        .ok_or(MateBlobBuildError::LengthMismatch)?;
    if *encoded_blob_bytes != encoded_len || *total_promotion_bytes != expected_total {
        return Err(MateBlobBuildError::AdmissionSizeMismatch);
    }
    Ok(result)
}

fn build_promoted_blob_unchecked(
    decision: &MateLeafPromotionDecision,
    rows: &[MatePromotionRows],
) -> Result<(MateBlob, Vec<u8>), MateBlobBuildError> {
    let MateLeafPromotionDecision::Promote {
        mode, bucket_ids, ..
    } = decision
    else {
        return Err(MateBlobBuildError::MissingSelectedBucket);
    };
    let mut sorted = rows.to_vec();
    sorted.sort_by_key(|row| (row.inputs.owner_vertex_id, row.inputs.bucket_label_key));
    let mut buckets = Vec::with_capacity(bucket_ids.len());
    for (owner, label) in bucket_ids {
        let mut matches = sorted.iter().filter(|row| {
            row.inputs.owner_vertex_id == *owner && row.inputs.bucket_label_key == *label
        });
        let row = matches
            .next()
            .ok_or(MateBlobBuildError::MissingSelectedBucket)?;
        if matches.next().is_some() {
            return Err(MateBlobBuildError::DuplicateSelectedBucket);
        }
        if row.source_slots.len() as u64 != row.inputs.live_entries {
            return Err(MateBlobBuildError::RowCountMismatch);
        }
        let mapping = build_mapping(*mode, &row.source_slots, &row.mate_slots)?;
        buckets.push(Bucket {
            owner_vertex_id: u32::from(*owner),
            bucket_label_key: label.raw(),
            entries: u32::try_from(row.inputs.live_entries)
                .map_err(|_| MateBlobBuildError::RowCountMismatch)?,
            mode: mode.codec(),
            mapping,
        });
    }
    let blob = MateBlob { buckets };
    let expected = blob.encoded_len().map_err(MateBlobBuildError::Codec)?;
    let bytes = blob.encode().map_err(MateBlobBuildError::Codec)?;
    if bytes.len() != expected {
        return Err(MateBlobBuildError::LengthMismatch);
    }
    let MateLeafPromotionDecision::Promote { config, .. } = decision else {
        unreachable!("decision matched above");
    };
    let encoded_u64 = u64::try_from(bytes.len()).map_err(|_| MateBlobBuildError::LengthMismatch)?;
    let total = encoded_u64
        .checked_add(config.leaf_shared_overhead_bytes)
        .ok_or(MateBlobBuildError::LengthMismatch)?;
    let entries = bucket_ids.iter().try_fold(0u64, |sum, (owner, label)| {
        let row = rows
            .iter()
            .find(|row| {
                row.inputs.owner_vertex_id == *owner && row.inputs.bucket_label_key == *label
            })
            .ok_or(MateBlobBuildError::MissingSelectedBucket)?;
        sum.checked_add(row.inputs.live_entries)
            .ok_or(MateBlobBuildError::LengthMismatch)
    })?;
    let per_entry = config
        .max_bytes_per_entry
        .checked_mul(entries)
        .ok_or(MateBlobBuildError::LengthMismatch)?;
    if encoded_u64 > config.max_encoded_blob_bytes
        || total > config.max_total_promotion_bytes
        || total > per_entry
    {
        return Err(MateBlobBuildError::LengthMismatch);
    }
    Ok((blob, bytes))
}

#[cfg(test)]
pub(crate) fn test_finalize_decision_sizes(
    decision: &mut MateLeafPromotionDecision,
    rows: &[MatePromotionRows],
) {
    let (_, bytes) = build_promoted_blob_unchecked(decision, rows).expect("valid test decision");
    let encoded = u64::try_from(bytes.len()).expect("test blob length");
    if let MateLeafPromotionDecision::Promote {
        config,
        encoded_blob_bytes,
        total_promotion_bytes,
        ..
    } = decision
    {
        *encoded_blob_bytes = encoded;
        *total_promotion_bytes = encoded + config.leaf_shared_overhead_bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(stride: u8, width: u8, entries: u64) -> MatePromotionCandidate {
        MatePromotionCandidate {
            inputs: MatePromotionInputs {
                owner_vertex_id: VertexId::from(1),
                bucket_label_key: BucketLabelKey::directed_from_index(3),
                live_entries: entries,
                source_scan_rows: entries,
                counterpart_scan_rows: entries,
                sampled_stride: stride,
                packed_width_bytes: width,
                min_scan_rows: 1,
            },
            source_slots: (0..entries as u32).collect(),
            mate_slots: (0..entries as u32).map(|slot| slot + 1).collect(),
        }
    }

    fn config() -> MateLeafPromotionConfig {
        MateLeafPromotionConfig {
            leaf_shared_overhead_bytes: 8,
            max_encoded_blob_bytes: 4096,
            max_total_promotion_bytes: 4096,
            max_bytes_per_entry: 100,
        }
    }

    #[test]
    fn exact_prefilter_boundary_is_enforced() {
        let mut one = candidate(16, 1, 1);
        assert!(matches!(
            decide_leaf_promotion(&[one.clone()], config()),
            MateLeafPromotionDecision::ScanOnly { .. }
        ));
        one.inputs.live_entries = 2;
        one.source_slots.push(1);
        one.mate_slots.push(2);
        assert!(matches!(
            decide_leaf_promotion(&[one], config()),
            MateLeafPromotionDecision::Promote { .. }
        ));
    }

    #[test]
    fn malformed_candidate_row_count_is_rejected_before_promotion() {
        let mut candidate = candidate(16, 1, 2);
        candidate.mate_slots.pop();
        assert_eq!(
            prefilter(&candidate),
            Err(MatePromotionRejectReason::CodecRejected)
        );
        assert!(matches!(
            decide_leaf_promotion(&[candidate], config()),
            MateLeafPromotionDecision::ScanOnly {
                reason: MatePromotionRejectReason::CodecRejected
            }
        ));
    }

    #[test]
    fn shared_overhead_is_charged_once_and_codec_is_ssot() {
        let a = candidate(16, 1, 2);
        let mut b = candidate(16, 1, 2);
        b.inputs.owner_vertex_id = VertexId::from(2);
        let decision = decide_leaf_promotion(&[a, b], config());
        let MateLeafPromotionDecision::Promote {
            bucket_ids,
            encoded_blob_bytes,
            total_promotion_bytes,
            ..
        } = decision
        else {
            panic!("expected promotion");
        };
        assert_eq!(bucket_ids.len(), 2);
        assert_eq!(total_promotion_bytes, encoded_blob_bytes + 8);
    }

    #[test]
    fn incompatible_parameters_are_not_coerced() {
        let a = candidate(16, 1, 2);
        let mut b = candidate(32, 2, 2);
        b.inputs.owner_vertex_id = VertexId::from(2);
        let MateLeafPromotionDecision::Promote { bucket_ids, .. } =
            decide_leaf_promotion(&[a, b], config())
        else {
            panic!("one compatible group should promote");
        };
        assert_eq!(bucket_ids.len(), 1);
    }

    #[test]
    fn invalid_config_and_overflow_fail_closed() {
        let mut invalid = config();
        invalid.max_bytes_per_entry = 0;
        assert!(matches!(
            decide_leaf_promotion(&[candidate(16, 1, 2)], invalid),
            MateLeafPromotionDecision::ScanOnly {
                reason: MatePromotionRejectReason::InvalidConfig
            }
        ));
        let mut overflow = candidate(16, 1, 2);
        overflow.inputs.source_scan_rows = u64::MAX;
        overflow.inputs.counterpart_scan_rows = 1;
        assert!(matches!(
            decide_leaf_promotion(&[overflow], config()),
            MateLeafPromotionDecision::ScanOnly {
                reason: MatePromotionRejectReason::ArithmeticOverflow
            }
        ));
    }

    #[test]
    fn unsupported_width_and_stride_fail_closed_without_defaults() {
        let invalid = candidate(0, 0, 2);
        assert_eq!(
            prefilter(&invalid),
            Err(MatePromotionRejectReason::UnsupportedParameters)
        );
        assert_eq!(
            decide_leaf_promotion(&[invalid], config()),
            MateLeafPromotionDecision::ScanOnly {
                reason: MatePromotionRejectReason::NoEligibleBuckets
            }
        );
    }

    #[test]
    fn total_and_bytes_per_entry_exact_boundaries_are_admitted() {
        let candidate = candidate(0, 1, 2);
        let mut generous = config();
        generous.max_encoded_blob_bytes = u64::MAX;
        generous.max_total_promotion_bytes = u64::MAX;
        generous.max_bytes_per_entry = 1 << 20;
        let promoted = decide_leaf_promotion(std::slice::from_ref(&candidate), generous);
        let MateLeafPromotionDecision::Promote {
            encoded_blob_bytes,
            total_promotion_bytes,
            ..
        } = promoted
        else {
            panic!("expected packed promotion");
        };

        let mut exact_total = generous;
        exact_total.max_encoded_blob_bytes = encoded_blob_bytes;
        exact_total.max_total_promotion_bytes = total_promotion_bytes;
        assert!(matches!(
            decide_leaf_promotion(std::slice::from_ref(&candidate), exact_total),
            MateLeafPromotionDecision::Promote { .. }
        ));

        let entries = candidate.inputs.live_entries;
        assert_eq!(total_promotion_bytes % entries, 0);
        let mut exact_per_entry = generous;
        exact_per_entry.max_encoded_blob_bytes = encoded_blob_bytes;
        exact_per_entry.max_total_promotion_bytes = total_promotion_bytes;
        exact_per_entry.max_bytes_per_entry = total_promotion_bytes / entries;
        assert!(matches!(
            decide_leaf_promotion(std::slice::from_ref(&candidate), exact_per_entry),
            MateLeafPromotionDecision::Promote { .. }
        ));

        let mut above_total = exact_total;
        above_total.max_total_promotion_bytes = total_promotion_bytes - 1;
        assert!(matches!(
            decide_leaf_promotion(std::slice::from_ref(&candidate), above_total),
            MateLeafPromotionDecision::ScanOnly { .. }
        ));

        let mut above_per_entry = exact_per_entry;
        above_per_entry.max_bytes_per_entry = (total_promotion_bytes - 1) / entries;
        assert!(matches!(
            decide_leaf_promotion(std::slice::from_ref(&candidate), above_per_entry),
            MateLeafPromotionDecision::ScanOnly { .. }
        ));
    }

    #[test]
    fn equal_sampled_and_packed_aggregate_prefers_sampled() {
        let candidate = candidate(16, 2, 2);
        let decision = decide_leaf_promotion(std::slice::from_ref(&candidate), config());
        assert!(matches!(
            decision,
            MateLeafPromotionDecision::Promote {
                mode: MatePromotionMode::Sampled { stride: 16 },
                ..
            }
        ));
    }

    #[test]
    fn sampled_builder_emits_exact_stride_checkpoints() {
        let candidate = candidate(16, 2, 33);
        let decision = decide_leaf_promotion(std::slice::from_ref(&candidate), config());
        assert!(matches!(
            decision,
            MateLeafPromotionDecision::Promote {
                mode: MatePromotionMode::Sampled { stride: 16 },
                ..
            }
        ));
        let rows = MatePromotionRows {
            inputs: candidate.inputs,
            source_slots: (0..33).map(|slot| 100 + slot).collect(),
            mate_slots: (0..33).map(|slot| 200 + slot).collect(),
        };
        let (blob, bytes) = build_promoted_blob(&decision, &[rows]).expect("sampled build");
        let expected_mapping = [0u32, 16, 32]
            .into_iter()
            .flat_map(|ordinal| {
                (100 + ordinal)
                    .to_be_bytes()
                    .into_iter()
                    .chain((200 + ordinal).to_be_bytes())
            })
            .collect::<Vec<_>>();
        assert_eq!(blob.buckets[0].mapping, expected_mapping);
        assert_eq!(MateBlob::decode(&bytes).expect("decode sampled"), blob);
        assert_eq!(
            MateBlob::encoded_len(&blob).expect("encoded length"),
            bytes.len()
        );
    }

    #[test]
    fn incompatible_bucket_is_omitted_from_built_leaf_blob() {
        let admitted = candidate(16, 1, 2);
        let mut rejected = candidate(32, 2, 2);
        rejected.inputs.owner_vertex_id = VertexId::from(2);
        rejected.inputs.bucket_label_key = BucketLabelKey::directed_from_index(4);
        let decision = decide_leaf_promotion(&[admitted.clone(), rejected.clone()], config());
        let MateLeafPromotionDecision::Promote {
            bucket_ids,
            encoded_blob_bytes,
            total_promotion_bytes,
            ..
        } = decision.clone()
        else {
            panic!("expected one compatible bucket to promote");
        };
        assert_eq!(
            bucket_ids,
            vec![(
                admitted.inputs.owner_vertex_id,
                admitted.inputs.bucket_label_key
            )]
        );

        let rows = vec![
            MatePromotionRows {
                inputs: admitted.inputs,
                source_slots: vec![10, 20],
                mate_slots: vec![11, 21],
            },
            MatePromotionRows {
                inputs: rejected.inputs,
                source_slots: vec![30, 40],
                mate_slots: vec![31, 41],
            },
        ];
        let (blob, bytes) = build_promoted_blob(&decision, &rows).expect("build admitted bucket");
        assert_eq!(blob.buckets.len(), 1);
        assert_eq!(
            (
                blob.buckets[0].owner_vertex_id,
                blob.buckets[0].bucket_label_key
            ),
            (
                u32::from(admitted.inputs.owner_vertex_id),
                admitted.inputs.bucket_label_key.raw()
            )
        );
        assert_eq!(MateBlob::decode(&bytes).expect("decode leaf blob"), blob);
        assert_eq!(encoded_blob_bytes, bytes.len() as u64);
        assert_eq!(
            total_promotion_bytes,
            encoded_blob_bytes + config().leaf_shared_overhead_bytes
        );
    }

    #[test]
    fn packed_builder_preserves_parallel_pair_lanes() {
        let candidate = candidate(16, 1, 3);
        let decision = decide_leaf_promotion(std::slice::from_ref(&candidate), config());
        let rows = MatePromotionRows {
            inputs: candidate.inputs,
            source_slots: vec![10, 20, 30],
            mate_slots: vec![11, 21, 31],
        };
        let (blob, bytes) = build_promoted_blob(&decision, &[rows]).expect("build");
        assert_eq!(blob.buckets[0].mapping, vec![10, 11, 20, 21, 30, 31]);
        assert_eq!(MateBlob::decode(&bytes).expect("decode"), blob);
    }

    #[test]
    fn builder_rejects_forged_admission_sizes() {
        let candidate = candidate(16, 1, 3);
        let mut decision = decide_leaf_promotion(std::slice::from_ref(&candidate), config());
        let rows = MatePromotionRows {
            inputs: candidate.inputs,
            source_slots: vec![10, 20, 30],
            mate_slots: vec![11, 21, 31],
        };
        test_finalize_decision_sizes(&mut decision, std::slice::from_ref(&rows));
        if let MateLeafPromotionDecision::Promote {
            encoded_blob_bytes, ..
        } = &mut decision
        {
            *encoded_blob_bytes += 1;
        } else {
            panic!("expected promotion");
        }
        assert_eq!(
            build_promoted_blob(&decision, std::slice::from_ref(&rows)),
            Err(MateBlobBuildError::AdmissionSizeMismatch)
        );
        if let MateLeafPromotionDecision::Promote {
            encoded_blob_bytes,
            total_promotion_bytes,
            ..
        } = &mut decision
        {
            *encoded_blob_bytes -= 1;
            *total_promotion_bytes += 1;
        }
        assert_eq!(
            build_promoted_blob(&decision, std::slice::from_ref(&rows)),
            Err(MateBlobBuildError::AdmissionSizeMismatch)
        );
    }

    #[test]
    fn duplicate_selected_rows_are_rejected() {
        let candidate = candidate(16, 1, 3);
        let decision = decide_leaf_promotion(std::slice::from_ref(&candidate), config());
        let row = MatePromotionRows {
            inputs: candidate.inputs,
            source_slots: vec![10, 20, 30],
            mate_slots: vec![11, 21, 31],
        };
        assert_eq!(
            build_promoted_blob(&decision, &[row.clone(), row]),
            Err(MateBlobBuildError::DuplicateSelectedBucket)
        );
    }

    #[test]
    fn encoded_limit_boundary_is_fail_closed() {
        let candidate = candidate(16, 1, 2);
        let promoted = decide_leaf_promotion(std::slice::from_ref(&candidate), config());
        let encoded = match promoted {
            MateLeafPromotionDecision::Promote {
                encoded_blob_bytes, ..
            } => encoded_blob_bytes,
            MateLeafPromotionDecision::ScanOnly { .. } => panic!("expected promotion"),
        };
        let mut below = config();
        below.max_encoded_blob_bytes = encoded - 1;
        assert!(matches!(
            decide_leaf_promotion(&[candidate], below),
            MateLeafPromotionDecision::ScanOnly { .. }
        ));
    }
}
