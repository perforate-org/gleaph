//! Canonical, read-only mate-leaf enumeration.
//!
//! This module owns the bridge from graph-owned bucket identities/live rows to the existing
//! promotion admission types. It never opens a rebuild token or writes derived storage.

use super::{
    DeferredBidirectionalLabeledError, DeferredBidirectionalLabeledLaraGraph, Orientation,
    deferred::MateLeafRebuildError,
};
use crate::{VertexId, labeled::BucketLabelKey};
use std::fmt;

use super::mate_promotion::{
    MateLeafPromotionConfig, MateLeafPromotionDecision, MateLeafPromotionPlan,
    MatePromotionCandidate, MatePromotionInputs, MatePromotionRows, supported_packed_width,
    supported_sampled_stride, valid_config,
};

/// Enumeration-only policy. Shared byte/overhead limits remain in the promotion config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MateLeafEnumerationPolicy {
    pub config: MateLeafPromotionConfig,
    pub sampled_stride: u8,
    pub packed_width_bytes: u8,
    pub min_scan_rows: u64,
}

/// Stable owner policy used by queued rebuild work.
pub(crate) fn default_mate_leaf_enumeration_policy() -> MateLeafEnumerationPolicy {
    MateLeafEnumerationPolicy {
        config: MateLeafPromotionConfig {
            leaf_shared_overhead_bytes: 24,
            max_encoded_blob_bytes: 64 * 1024,
            max_total_promotion_bytes: 256 * 1024,
            max_bytes_per_entry: 4096,
        },
        sampled_stride: 32,
        packed_width_bytes: 4,
        min_scan_rows: 1,
    }
}

/// Typed failures from canonical enumeration.
#[derive(Debug)]
pub(crate) enum MateLeafEnumerationError {
    InvalidLeaf {
        leaf: u32,
        segment_count: u32,
    },
    InvalidOrientation(BucketLabelKey),
    Storage(DeferredBidirectionalLabeledError),
    MissingCounterpart {
        owner: VertexId,
        label: BucketLabelKey,
        neighbor: VertexId,
        rank: usize,
    },
    CountMismatch {
        owner: VertexId,
        label: BucketLabelKey,
        source: usize,
        counterpart: usize,
    },
    DuplicateBucket {
        owner: VertexId,
        label: BucketLabelKey,
    },
    UnsupportedWidth {
        owner: VertexId,
        label: BucketLabelKey,
        width: u16,
    },
    ArithmeticOverflow,
    PolicyRejected,
}

impl PartialEq for MateLeafEnumerationError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::InvalidLeaf {
                    leaf: a,
                    segment_count: b,
                },
                Self::InvalidLeaf {
                    leaf: c,
                    segment_count: d,
                },
            ) => a == c && b == d,
            (Self::InvalidOrientation(a), Self::InvalidOrientation(b)) => a == b,
            (Self::Storage(a), Self::Storage(b)) => a.to_string() == b.to_string(),
            (
                Self::MissingCounterpart {
                    owner: ao,
                    label: al,
                    neighbor: an,
                    rank: ar,
                },
                Self::MissingCounterpart {
                    owner: bo,
                    label: bl,
                    neighbor: bn,
                    rank: br,
                },
            ) => ao == bo && al == bl && an == bn && ar == br,
            (
                Self::CountMismatch {
                    owner: ao,
                    label: al,
                    source: as_,
                    counterpart: ac,
                },
                Self::CountMismatch {
                    owner: bo,
                    label: bl,
                    source: bs,
                    counterpart: bc,
                },
            ) => ao == bo && al == bl && as_ == bs && ac == bc,
            (
                Self::DuplicateBucket {
                    owner: ao,
                    label: al,
                },
                Self::DuplicateBucket {
                    owner: bo,
                    label: bl,
                },
            ) => ao == bo && al == bl,
            (
                Self::UnsupportedWidth {
                    owner: ao,
                    label: al,
                    width: aw,
                },
                Self::UnsupportedWidth {
                    owner: bo,
                    label: bl,
                    width: bw,
                },
            ) => ao == bo && al == bl && aw == bw,
            (Self::ArithmeticOverflow, Self::ArithmeticOverflow)
            | (Self::PolicyRejected, Self::PolicyRejected) => true,
            _ => false,
        }
    }
}

impl Eq for MateLeafEnumerationError {}

impl fmt::Display for MateLeafEnumerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLeaf {
                leaf,
                segment_count,
            } => {
                write!(f, "leaf {leaf} out of range (count={segment_count})")
            }
            Self::InvalidOrientation(label) => write!(f, "invalid orientation for bucket {label}"),
            Self::Storage(error) => write!(f, "canonical storage read failed: {error}"),
            Self::MissingCounterpart {
                owner,
                label,
                neighbor,
                rank,
            } => write!(
                f,
                "missing counterpart owner={owner} label={label} neighbor={neighbor} rank={rank}"
            ),
            Self::CountMismatch {
                owner,
                label,
                source,
                counterpart,
            } => write!(
                f,
                "counterpart count mismatch owner={owner} label={label} source={source} counterpart={counterpart}"
            ),
            Self::DuplicateBucket { owner, label } => {
                write!(f, "duplicate bucket owner={owner} label={label}")
            }
            Self::UnsupportedWidth {
                owner,
                label,
                width,
            } => write!(
                f,
                "unsupported payload width owner={owner} label={label} width={width}"
            ),
            Self::ArithmeticOverflow => write!(f, "canonical enumeration arithmetic overflow"),
            Self::PolicyRejected => write!(f, "invalid enumeration policy"),
        }
    }
}

impl std::error::Error for MateLeafEnumerationError {}

/// Complete pure result used by the later publication boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnumeratedMateLeaf {
    pub orientation: Orientation,
    pub leaf: u32,
    pub policy: MateLeafEnumerationPolicy,
    pub candidates: Vec<MatePromotionCandidate>,
    pub rows: Vec<MatePromotionRows>,
    pub decision: MateLeafPromotionDecision,
    /// Ordered canonical identity/neighbor observations. Equality, not a hash, is authoritative.
    pub canonical_observations: Vec<CanonicalMateBucketObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CanonicalMateBucketObservation {
    pub owner: VertexId,
    pub label: BucketLabelKey,
    pub source_neighbors: Vec<VertexId>,
    pub counterpart_neighbors: Vec<VertexId>,
    pub source_slots: Vec<u32>,
    pub mate_slots: Vec<u32>,
}

fn validate_bucket_identities(
    identities: &[(VertexId, BucketLabelKey)],
) -> Result<(), MateLeafEnumerationError> {
    let mut seen = std::collections::BTreeSet::new();
    for &(owner, label) in identities {
        if !seen.insert((owner, label)) {
            return Err(MateLeafEnumerationError::DuplicateBucket { owner, label });
        }
    }
    Ok(())
}

fn graph_error(
    orientation: Orientation,
    error: impl Into<DeferredBidirectionalLabeledError>,
) -> MateLeafEnumerationError {
    let _ = orientation;
    MateLeafEnumerationError::Storage(error.into())
}

impl<E, M> DeferredBidirectionalLabeledLaraGraph<E, M>
where
    E: crate::traits::CsrEdge + crate::traits::CsrEdgeTombstone,
    M: ic_stable_structures::Memory,
{
    /// Enumerates one canonical orientation/leaf without mutating canonical or derived storage.
    pub(crate) fn enumerate_mate_leaf(
        &self,
        orientation: Orientation,
        leaf: u32,
        policy: MateLeafEnumerationPolicy,
    ) -> Result<EnumeratedMateLeaf, MateLeafEnumerationError> {
        if policy.min_scan_rows == 0
            || !supported_sampled_stride(policy.sampled_stride)
            || !supported_packed_width(policy.packed_width_bytes)
            || !valid_config(policy.config)
        {
            return Err(MateLeafEnumerationError::PolicyRejected);
        }
        let segment_count = self.forward().segment_count();
        if leaf >= segment_count {
            return Err(MateLeafEnumerationError::InvalidLeaf {
                leaf,
                segment_count,
            });
        }
        let source_graph = match orientation {
            Orientation::Forward => self.forward(),
            Orientation::Reverse => self.reverse(),
        };
        if orientation == Orientation::Reverse {
            let forward_identities =
                self.forward()
                    .read_leaf_bucket_identities(leaf)
                    .map_err(|error| {
                        graph_error(
                            orientation,
                            DeferredBidirectionalLabeledError::Forward(error),
                        )
                    })?;
            if let Some((_, label)) = forward_identities
                .into_iter()
                .find(|(_, label)| label.is_undirected())
            {
                return Err(MateLeafEnumerationError::InvalidOrientation(label));
            }
        }
        let identities = source_graph
            .read_leaf_bucket_identities(leaf)
            .map_err(|error| match orientation {
                Orientation::Forward => graph_error(
                    orientation,
                    DeferredBidirectionalLabeledError::Forward(error),
                ),
                Orientation::Reverse => graph_error(
                    orientation,
                    DeferredBidirectionalLabeledError::Reverse(error),
                ),
            })?;
        validate_bucket_identities(&identities)?;
        let mut candidates = Vec::new();
        let mut rows = Vec::new();
        let mut observations = Vec::new();
        for (owner, label) in identities {
            if orientation == Orientation::Reverse && label.is_undirected() {
                return Err(MateLeafEnumerationError::InvalidOrientation(label));
            }
            let mut source = Vec::new();
            source_graph
                .for_each_live_edge_slot_for_label(owner, label, |slot, edge| {
                    source.push((slot, edge.neighbor_vid()));
                })
                .map_err(|error| match orientation {
                    Orientation::Forward => graph_error(
                        orientation,
                        DeferredBidirectionalLabeledError::Forward(error),
                    ),
                    Orientation::Reverse => graph_error(
                        orientation,
                        DeferredBidirectionalLabeledError::Reverse(error),
                    ),
                })?;
            if source.is_empty() {
                continue;
            }
            let counterpart_orientation = if label.is_undirected() {
                Orientation::Forward
            } else {
                match orientation {
                    Orientation::Forward => Orientation::Reverse,
                    Orientation::Reverse => Orientation::Forward,
                }
            };
            let counterpart_graph = match counterpart_orientation {
                Orientation::Forward => self.forward(),
                Orientation::Reverse => self.reverse(),
            };
            let mut counterpart_by_neighbor = std::collections::HashMap::new();
            let mut neighbor_order = Vec::new();
            for (_, neighbor) in &source {
                if counterpart_by_neighbor.contains_key(neighbor) {
                    continue;
                }
                neighbor_order.push(*neighbor);
                let mut counterpart_rows = Vec::new();
                counterpart_graph
                    .for_each_live_edge_slot_for_label(*neighbor, label, |mate_slot, edge| {
                        counterpart_rows.push((mate_slot, edge.neighbor_vid()));
                    })
                    .map_err(|error| match counterpart_orientation {
                        Orientation::Forward => graph_error(
                            orientation,
                            DeferredBidirectionalLabeledError::Forward(error),
                        ),
                        Orientation::Reverse => graph_error(
                            orientation,
                            DeferredBidirectionalLabeledError::Reverse(error),
                        ),
                    })?;
                counterpart_by_neighbor.insert(*neighbor, counterpart_rows);
            }
            let mut source_slots = Vec::with_capacity(source.len());
            let mut mate_slots = Vec::with_capacity(source.len());
            let counterpart_neighbors = neighbor_order
                .iter()
                .filter_map(|neighbor| counterpart_by_neighbor.get(neighbor))
                .flat_map(|rows| rows.iter().map(|(_, neighbor)| *neighbor))
                .collect::<Vec<_>>();
            for (slot, neighbor) in &source {
                source_slots.push(*slot);
                if label.is_undirected() && *neighbor == owner {
                    mate_slots.push(*slot);
                    continue;
                }
                let matches = counterpart_by_neighbor
                    .get(neighbor)
                    .into_iter()
                    .flat_map(|rows| rows.iter())
                    .filter_map(|(mate_slot, mate_neighbor)| {
                        (*mate_neighbor == owner).then_some(*mate_slot)
                    })
                    .collect::<Vec<_>>();
                let rank = source[..source
                    .iter()
                    .position(|(candidate, _)| candidate == slot)
                    .unwrap_or(0)]
                    .iter()
                    .filter(|(_, prior)| *prior == *neighbor)
                    .count();
                let mate = matches.get(rank).copied().ok_or(
                    MateLeafEnumerationError::MissingCounterpart {
                        owner,
                        label,
                        neighbor: *neighbor,
                        rank,
                    },
                )?;
                mate_slots.push(mate);
            }
            for (neighbor, counterpart_rows) in &counterpart_by_neighbor {
                let source_count = source
                    .iter()
                    .filter(|(_, source_neighbor)| source_neighbor == neighbor)
                    .count();
                let counterpart_count = counterpart_rows
                    .iter()
                    .filter(|(_, counterpart_neighbor)| *counterpart_neighbor == owner)
                    .count();
                if source_count != counterpart_count {
                    return Err(MateLeafEnumerationError::CountMismatch {
                        owner,
                        label,
                        source: source_count,
                        counterpart: counterpart_count,
                    });
                }
            }
            let info = source_graph
                .read_label_bucket_placement_info(owner, label)
                .map_err(|error| match orientation {
                    Orientation::Forward => graph_error(
                        orientation,
                        DeferredBidirectionalLabeledError::Forward(error),
                    ),
                    Orientation::Reverse => graph_error(
                        orientation,
                        DeferredBidirectionalLabeledError::Reverse(error),
                    ),
                })?
                .ok_or(MateLeafEnumerationError::CountMismatch {
                    owner,
                    label,
                    source: source.len(),
                    counterpart: 0,
                })?;
            let counterpart_count = counterpart_neighbors.len();
            let live_entries = u64::try_from(source.len())
                .map_err(|_| MateLeafEnumerationError::ArithmeticOverflow)?;
            let source_scan_rows = live_entries;
            let counterpart_scan_rows = u64::try_from(counterpart_count)
                .map_err(|_| MateLeafEnumerationError::ArithmeticOverflow)?;
            let packed_width_bytes = if info.inline_value_byte_width == 0 {
                policy.packed_width_bytes
            } else if info.inline_value_byte_width <= 4 {
                info.inline_value_byte_width as u8
            } else {
                return Err(MateLeafEnumerationError::UnsupportedWidth {
                    owner,
                    label,
                    width: info.inline_value_byte_width,
                });
            };
            let inputs = MatePromotionInputs {
                owner_vertex_id: owner,
                bucket_label_key: label,
                live_entries,
                source_scan_rows,
                counterpart_scan_rows,
                sampled_stride: policy.sampled_stride,
                packed_width_bytes,
                min_scan_rows: policy.min_scan_rows,
            };
            let candidate = MatePromotionCandidate {
                inputs,
                source_slots: source_slots.clone(),
                mate_slots: mate_slots.clone(),
            };
            candidates.push(candidate);
            rows.push(MatePromotionRows {
                inputs,
                source_slots: source_slots.clone(),
                mate_slots: mate_slots.clone(),
            });
            observations.push(CanonicalMateBucketObservation {
                owner,
                label,
                source_neighbors: source.iter().map(|(_, neighbor)| *neighbor).collect(),
                counterpart_neighbors,
                source_slots,
                mate_slots,
            });
        }
        candidates.sort_by_key(|candidate| {
            (
                candidate.inputs.owner_vertex_id,
                candidate.inputs.bucket_label_key,
            )
        });
        rows.sort_by_key(|row| (row.inputs.owner_vertex_id, row.inputs.bucket_label_key));
        observations.sort_by_key(|observation| (observation.owner, observation.label));
        let decision = MateLeafPromotionPlan::new(candidates.clone(), policy.config).decide();
        Ok(EnumeratedMateLeaf {
            orientation,
            leaf,
            policy,
            candidates,
            rows,
            decision,
            canonical_observations: observations,
        })
    }

    /// Re-enumerates and publishes only when the complete canonical aggregate is unchanged.
    ///
    /// The comparison is structural; the observation vectors are intentionally retained in the
    /// aggregate so a same-slot target replacement cannot pass a hash-only check.
    pub(crate) fn rebuild_mate_leaf_from_canonical(
        &self,
        expected: &EnumeratedMateLeaf,
    ) -> Result<(), MateLeafRebuildError> {
        let current = self
            .enumerate_mate_leaf(expected.orientation, expected.leaf, expected.policy)
            .map_err(MateLeafRebuildError::Enumeration)?;
        if current != *expected {
            return Err(MateLeafRebuildError::StaleEnumeration);
        }
        self.rebuild_mate_leaf(
            expected.orientation,
            expected.leaf,
            &expected.decision,
            &expected.rows,
        )
    }

    /// Revalidates and consumes a caller-owned rebuild token. Any pre-publication failure aborts
    /// the token so this boundary cannot leave a locator in `Rebuilding`.
    pub(crate) fn rebuild_mate_leaf_from_canonical_with_token(
        &self,
        expected: &EnumeratedMateLeaf,
        token: &mut Option<super::mate_storage::MateRebuildToken>,
    ) -> Result<(), MateLeafRebuildError> {
        let segment_count = u64::from(self.forward().segment_count());
        let expected_row = match expected.orientation {
            Orientation::Forward => u64::from(expected.leaf),
            Orientation::Reverse => segment_count.checked_add(u64::from(expected.leaf)).ok_or(
                MateLeafRebuildError::Storage(
                    super::mate_storage::MateStorageInitError::RowCountOverflow,
                ),
            )?,
        };
        let Some(token_ref) = token.as_ref() else {
            return Err(MateLeafRebuildError::Storage(
                super::mate_storage::MateStorageInitError::RebuildStateMismatch,
            ));
        };
        if !self.mate_rebuild_token_matches_row(token_ref, expected_row) {
            return Err(MateLeafRebuildError::Storage(
                super::mate_storage::MateStorageInitError::RebuildStateMismatch,
            ));
        }
        let token = token.take().expect("validated rebuild token");
        let current =
            match self.enumerate_mate_leaf(expected.orientation, expected.leaf, expected.policy) {
                Ok(current) => current,
                Err(error) => {
                    return match self.abort_empty_mate_leaf_rebuild(token) {
                        Ok(()) => Err(MateLeafRebuildError::Enumeration(error)),
                        Err(abort) => Err(abort),
                    };
                }
            };
        if current != *expected {
            self.abort_empty_mate_leaf_rebuild(token)?;
            return Err(MateLeafRebuildError::StaleEnumeration);
        }
        match &expected.decision {
            MateLeafPromotionDecision::ScanOnly { .. } => {
                if !expected.rows.is_empty() {
                    self.abort_empty_mate_leaf_rebuild(token)?;
                    return Err(MateLeafRebuildError::RowsForScanOnly);
                }
                self.abort_empty_mate_leaf_rebuild(token)
            }
            MateLeafPromotionDecision::Promote { .. } => {
                let (encoded, _) = match self.build_mate_leaf_blob(
                    expected.orientation,
                    expected.leaf,
                    &expected.decision,
                    &expected.rows,
                ) {
                    Ok(blob) => blob,
                    Err(error) => {
                        self.abort_empty_mate_leaf_rebuild(token)?;
                        return Err(MateLeafRebuildError::Build(error));
                    }
                };
                self.publish_mate_leaf_rebuild(token, &encoded)
                    .map_err(MateLeafRebuildError::Storage)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_identity_fixture_is_rejected() {
        let label = BucketLabelKey::directed_from_index(7);
        assert!(matches!(
            validate_bucket_identities(&[(VertexId::from(1), label), (VertexId::from(1), label)]),
            Err(MateLeafEnumerationError::DuplicateBucket { owner, label: actual })
                if owner == VertexId::from(1) && actual == label
        ));
    }
}
