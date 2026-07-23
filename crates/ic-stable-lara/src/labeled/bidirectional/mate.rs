//! Persistence-free physical mate resolution for bidirectional labeled LARA.
//!
//! The adjacency rows remain the source of truth.  This module derives a mate by
//! selecting the same live equal-neighbor occurrence rank in the counterpart
//! bucket; it does not allocate or persist an index.

use super::mate_blob_prototype::{Bucket, Mode};
use super::mate_storage::MateLocatorState;
use super::{DeferredBidirectionalLabeledLaraGraph, Orientation};
use crate::{VertexId, labeled::BucketLabelKey, traits::CsrEdgeTombstone};
use ic_stable_structures::Memory;
use std::fmt;

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static CANONICAL_MATE_LOOKUPS: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_canonical_mate_lookup_count() {
    CANONICAL_MATE_LOOKUPS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn canonical_mate_lookup_count() -> u32 {
    CANONICAL_MATE_LOOKUPS.with(Cell::get)
}

/// A physical edge location together with the orientation that owns its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalEdgeRef {
    /// Forward outgoing or reverse incoming orientation.
    pub orientation: Orientation,
    /// Vertex owning the label bucket row.
    pub owner_vertex_id: VertexId,
    /// Storage label of the bucket containing the row.
    pub label_id: BucketLabelKey,
    /// Live slot index inside the label row.
    pub slot_index: u32,
}

/// Fail-closed errors returned by scan-only mate resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MateLookupError {
    /// The requested physical slot is not live in the declared bucket.
    SourceNotFound(PhysicalEdgeRef),
    /// The counterpart bucket has no matching occurrence at the source rank.
    MateNotFound(PhysicalEdgeRef),
    /// A physical source was observed more than once during selection.
    AmbiguousSource(PhysicalEdgeRef),
    /// The two projections disagree about the number of live equal-neighbor rows.
    InconsistentRelation {
        /// Source physical reference.
        source: PhysicalEdgeRef,
        /// Owner vertex of the counterpart bucket.
        counterpart_owner: VertexId,
        /// Number of matching source rows.
        source_count: u32,
        /// Number of matching counterpart rows.
        counterpart_count: u32,
    },
    /// The reference uses an impossible orientation for its bucket kind.
    InvalidOrientation(PhysicalEdgeRef),
    /// The underlying LARA scan failed.
    ReadFailed(String),
}

impl fmt::Display for MateLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceNotFound(edge) => write!(f, "source edge is not live: {edge:?}"),
            Self::MateNotFound(edge) => write!(f, "mate edge is missing for {edge:?}"),
            Self::AmbiguousSource(edge) => write!(f, "source edge is ambiguous: {edge:?}"),
            Self::InconsistentRelation {
                source,
                counterpart_owner,
                source_count,
                counterpart_count,
            } => write!(
                f,
                "inconsistent relation for {source:?}: counterpart owner={counterpart_owner:?}, source_count={source_count}, counterpart_count={counterpart_count}"
            ),
            Self::InvalidOrientation(edge) => write!(f, "invalid orientation for {edge:?}"),
            Self::ReadFailed(message) => write!(f, "mate scan failed: {message}"),
        }
    }
}

impl std::error::Error for MateLookupError {}

fn scan_rank<E, M>(
    graph: &crate::labeled::LabeledLaraGraph<E, M>,
    edge: PhysicalEdgeRef,
) -> Result<(VertexId, u32, u32, u32), MateLookupError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    let mut rows = Vec::new();
    graph
        .for_each_live_edge_slot_for_label(edge.owner_vertex_id, edge.label_id, |slot, row| {
            rows.push((slot, row));
        })
        .map_err(|err| MateLookupError::ReadFailed(err.to_string()))?;
    let source_neighbor = rows
        .iter()
        .find(|(slot, _)| *slot == edge.slot_index)
        .map(|(_, row)| row.neighbor_vid());
    let Some(source_neighbor) = source_neighbor else {
        return Err(MateLookupError::SourceNotFound(edge));
    };
    if rows
        .iter()
        .filter(|(slot, _)| *slot == edge.slot_index)
        .count()
        != 1
    {
        return Err(MateLookupError::AmbiguousSource(edge));
    }

    let mut rank = 0u32;
    let mut total = 0u32;
    let mut seen = false;
    let row_count = u32::try_from(rows.len())
        .map_err(|_| MateLookupError::ReadFailed("source row count overflow".into()))?;
    for (slot, row) in rows {
        if row.neighbor_vid() != source_neighbor {
            continue;
        }
        if slot == edge.slot_index {
            seen = true;
            rank = total;
        }
        total = total
            .checked_add(1)
            .ok_or_else(|| MateLookupError::ReadFailed("source rank overflow".into()))?;
    }
    if !seen {
        return Err(MateLookupError::SourceNotFound(edge));
    }
    Ok((source_neighbor, rank, total, row_count))
}

fn select_rank<E, M>(
    graph: &crate::labeled::LabeledLaraGraph<E, M>,
    owner: VertexId,
    label: BucketLabelKey,
    neighbor: VertexId,
    rank: u32,
) -> Result<(PhysicalEdgeRef, u32), MateLookupError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    let mut rows = Vec::new();
    graph
        .for_each_live_edge_slot_for_label(owner, label, |slot, row| {
            rows.push((slot, row));
        })
        .map_err(|err| MateLookupError::ReadFailed(err.to_string()))?;
    let mut count = 0u32;
    let mut selected = None;
    for (slot, row) in rows {
        if row.neighbor_vid() == neighbor {
            if count == rank {
                selected = Some(PhysicalEdgeRef {
                    orientation: Orientation::Forward,
                    owner_vertex_id: owner,
                    label_id: label,
                    slot_index: slot,
                });
            }
            count = count
                .checked_add(1)
                .ok_or_else(|| MateLookupError::ReadFailed("counterpart rank overflow".into()))?;
        }
    }
    selected
        .map(|edge| (edge, count))
        .ok_or(MateLookupError::MateNotFound(PhysicalEdgeRef {
            orientation: Orientation::Forward,
            owner_vertex_id: owner,
            label_id: label,
            slot_index: u32::MAX,
        }))
}

fn packed_slot(bytes: &[u8], width: u8, index: usize) -> Option<u32> {
    let start = index.checked_mul(usize::from(width))?;
    let end = start.checked_add(usize::from(width))?;
    let value = bytes.get(start..end)?;
    let mut padded = [0u8; 4];
    padded[4 - usize::from(width)..].copy_from_slice(value);
    Some(u32::from_be_bytes(padded))
}

fn blob_mate_slot(bucket: &Bucket, source_slot: u32, rank: u32) -> Result<u32, MateLookupError> {
    match bucket.mode {
        Mode::Packed { width_bytes } => {
            let entries = usize::try_from(bucket.entries)
                .map_err(|_| MateLookupError::ReadFailed("blob entry count overflow".into()))?;
            let width = usize::from(width_bytes);
            let expected = entries
                .checked_mul(width)
                .and_then(|value| value.checked_mul(2))
                .ok_or_else(|| MateLookupError::ReadFailed("packed length overflow".into()))?;
            if bucket.mapping.len() != expected {
                return Err(MateLookupError::ReadFailed(
                    "packed mapping length mismatch".into(),
                ));
            }
            let mut previous = None;
            for entry in 0..entries {
                let source = packed_slot(&bucket.mapping, width_bytes, entry * 2)
                    .ok_or_else(|| MateLookupError::ReadFailed("packed source truncated".into()))?;
                let mate = packed_slot(&bucket.mapping, width_bytes, entry * 2 + 1)
                    .ok_or_else(|| MateLookupError::ReadFailed("packed mate truncated".into()))?;
                if previous.is_some_and(|prior| source <= prior) {
                    return Err(MateLookupError::ReadFailed(
                        "packed source slots are not strictly increasing".into(),
                    ));
                }
                previous = Some(source);
                if source == source_slot {
                    if entry
                        != usize::try_from(rank).map_err(|_| {
                            MateLookupError::ReadFailed("packed rank overflow".into())
                        })?
                    {
                        return Err(MateLookupError::ReadFailed(
                            "packed source rank mismatch".into(),
                        ));
                    }
                    return Ok(mate);
                }
            }
            Err(MateLookupError::ReadFailed(
                "source slot is absent from packed mapping".into(),
            ))
        }
        Mode::Sampled { stride } => {
            let entries = usize::try_from(bucket.entries)
                .map_err(|_| MateLookupError::ReadFailed("sample entry count overflow".into()))?;
            let checkpoints = entries
                .checked_add(usize::from(stride) - 1)
                .ok_or_else(|| MateLookupError::ReadFailed("sample count overflow".into()))?
                / usize::from(stride);
            for checkpoint in 0..checkpoints {
                let source = packed_slot(&bucket.mapping, 4, checkpoint * 2)
                    .ok_or_else(|| MateLookupError::ReadFailed("sample source truncated".into()))?;
                let mate = packed_slot(&bucket.mapping, 4, checkpoint * 2 + 1)
                    .ok_or_else(|| MateLookupError::ReadFailed("sample mate truncated".into()))?;
                let rank = usize::try_from(rank)
                    .map_err(|_| MateLookupError::ReadFailed("sample rank overflow".into()))?;
                if source == source_slot && checkpoint * usize::from(stride) == rank {
                    return Ok(mate);
                }
            }
            Err(MateLookupError::ReadFailed(
                "source is not represented by a sampled checkpoint".into(),
            ))
        }
    }
}

fn canonical_from_mate(source: PhysicalEdgeRef, mate: PhysicalEdgeRef) -> PhysicalEdgeRef {
    if source.label_id.is_directed() {
        return match source.orientation {
            Orientation::Forward => source,
            Orientation::Reverse => mate,
        };
    }
    if source.owner_vertex_id >= mate.owner_vertex_id {
        source
    } else {
        mate
    }
}

fn validate_blob_mate<E, M>(
    graph: &crate::labeled::LabeledLaraGraph<E, M>,
    source: PhysicalEdgeRef,
    mate: PhysicalEdgeRef,
    expected_neighbor: VertexId,
    expected_matching: u32,
    expected_rank: u32,
) -> Result<(), MateLookupError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    let mut seen = 0u32;
    let mut matching = 0u32;
    let mut candidate_rank = None;
    graph
        .for_each_live_edge_slot_for_label(mate.owner_vertex_id, mate.label_id, |slot, row| {
            if slot == mate.slot_index {
                seen = seen.saturating_add(1);
                if row.neighbor_vid() != source.owner_vertex_id {
                    seen = u32::MAX;
                }
            }
            if row.neighbor_vid() == source.owner_vertex_id {
                if slot == mate.slot_index {
                    candidate_rank = Some(matching);
                }
                matching = matching.saturating_add(1);
            }
        })
        .map_err(|error| MateLookupError::ReadFailed(error.to_string()))?;
    if seen != 1 || mate.owner_vertex_id != expected_neighbor {
        return Err(MateLookupError::ReadFailed(
            "published counterpart is not a live matching row".into(),
        ));
    }
    if matching != expected_matching {
        return Err(MateLookupError::InconsistentRelation {
            source,
            counterpart_owner: mate.owner_vertex_id,
            source_count: expected_matching,
            counterpart_count: matching,
        });
    }
    if candidate_rank != Some(expected_rank) {
        return Err(MateLookupError::ReadFailed(
            "published counterpart rank disagrees with source".into(),
        ));
    }
    Ok(())
}

impl<E, M> DeferredBidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    /// Resolves a mate through a Published Sampled/Packed blob when possible. The blob is only an
    /// accelerator: every candidate is checked against the live counterpart row and relation
    /// counts, while malformed, stale, or non-applicable data falls back to canonical rank/select
    /// exactly once. This remains an internal dormant bridge; ordinary callers still use
    /// EDGE_ALIASES.
    #[doc(hidden)]
    pub fn published_mate_of(
        &self,
        edge: PhysicalEdgeRef,
    ) -> Result<PhysicalEdgeRef, MateLookupError> {
        let canonical_fallback = || self.mate_of(edge);
        let leaf = u32::from(edge.owner_vertex_id) / self.forward().segment_size().max(1);
        let row = match self.mate_leaf_row(edge.orientation, leaf) {
            Ok(row) => row,
            Err(_) => return canonical_fallback(),
        };
        let state = match self.mate.locator_state(row) {
            Ok(state) => state,
            Err(_) => return canonical_fallback(),
        };
        if !matches!(state, MateLocatorState::Published { .. }) {
            return canonical_fallback();
        }

        let blob_candidate = (|| {
            let (neighbor, rank, source_count, row_count) = match edge.orientation {
                Orientation::Forward => scan_rank(self.forward(), edge)?,
                Orientation::Reverse => scan_rank(self.reverse(), edge)?,
            };
            let bucket = self
                .mate
                .published_bucket(row, u32::from(edge.owner_vertex_id), edge.label_id.raw())
                .map_err(|error| MateLookupError::ReadFailed(error.to_string()))?
                .ok_or_else(|| MateLookupError::ReadFailed("published bucket is missing".into()))?;
            if bucket.entries != row_count {
                return Err(MateLookupError::ReadFailed(
                    "published entry count disagrees with canonical rows".into(),
                ));
            }
            if edge.label_id.is_undirected() && neighbor == edge.owner_vertex_id {
                return Ok(edge);
            }
            let mate_slot = blob_mate_slot(&bucket, edge.slot_index, rank)?;
            let (orientation, owner) = if edge.label_id.is_directed() {
                match edge.orientation {
                    Orientation::Forward => (Orientation::Reverse, neighbor),
                    Orientation::Reverse => (Orientation::Forward, neighbor),
                }
            } else {
                (Orientation::Forward, neighbor)
            };
            let candidate = PhysicalEdgeRef {
                orientation,
                owner_vertex_id: owner,
                label_id: edge.label_id,
                slot_index: mate_slot,
            };
            let counterpart_graph = match candidate.orientation {
                Orientation::Forward => self.forward(),
                Orientation::Reverse => self.reverse(),
            };
            validate_blob_mate(
                counterpart_graph,
                edge,
                candidate,
                neighbor,
                source_count,
                rank,
            )?;
            Ok(candidate)
        })();
        blob_candidate.or_else(|_| self.mate_of(edge))
    }

    /// Resolves the exact paired physical entry by equal-neighbor occurrence rank.
    pub fn mate_of(&self, edge: PhysicalEdgeRef) -> Result<PhysicalEdgeRef, MateLookupError> {
        #[cfg(test)]
        CANONICAL_MATE_LOOKUPS.with(|count| count.set(count.get().saturating_add(1)));
        if edge.label_id.is_undirected() && matches!(edge.orientation, Orientation::Reverse) {
            return Err(MateLookupError::InvalidOrientation(edge));
        }
        let (neighbor, rank, source_count, _row_count) = match edge.orientation {
            Orientation::Forward => scan_rank(self.forward(), edge)?,
            Orientation::Reverse => scan_rank(self.reverse(), edge)?,
        };
        if edge.label_id.is_undirected() && neighbor == edge.owner_vertex_id {
            return Ok(edge);
        }
        let (counterpart_orientation, counterpart_owner) = if edge.label_id.is_directed() {
            match edge.orientation {
                Orientation::Forward => (Orientation::Reverse, neighbor),
                Orientation::Reverse => (Orientation::Forward, neighbor),
            }
        } else {
            (Orientation::Forward, neighbor)
        };
        let (mate, counterpart_count) = match counterpart_orientation {
            Orientation::Forward => select_rank(
                self.forward(),
                counterpart_owner,
                edge.label_id,
                edge.owner_vertex_id,
                rank,
            )?,
            Orientation::Reverse => select_rank(
                self.reverse(),
                counterpart_owner,
                edge.label_id,
                edge.owner_vertex_id,
                rank,
            )?,
        };
        if source_count != counterpart_count {
            return Err(MateLookupError::InconsistentRelation {
                source: edge,
                counterpart_owner,
                source_count,
                counterpart_count,
            });
        }
        Ok(PhysicalEdgeRef {
            orientation: counterpart_orientation,
            ..mate
        })
    }

    /// Resolves the canonical physical entry for an edge without persistent metadata.
    pub fn canonical_handle(
        &self,
        edge: PhysicalEdgeRef,
    ) -> Result<PhysicalEdgeRef, MateLookupError> {
        let mate = self.mate_of(edge)?;
        Ok(canonical_from_mate(edge, mate))
    }

    /// Canonicalizes a Published mate result without re-running canonical rank/select.
    #[doc(hidden)]
    pub fn published_canonical_handle(
        &self,
        edge: PhysicalEdgeRef,
    ) -> Result<PhysicalEdgeRef, MateLookupError> {
        let mate = self.published_mate_of(edge)?;
        Ok(canonical_from_mate(edge, mate))
    }
}
