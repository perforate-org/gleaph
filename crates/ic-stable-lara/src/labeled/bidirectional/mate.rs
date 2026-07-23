//! Persistence-free physical mate resolution for bidirectional labeled LARA.
//!
//! The adjacency rows remain the source of truth.  This module derives a mate by
//! selecting the same live equal-neighbor occurrence rank in the counterpart
//! bucket; it does not allocate or persist an index.

use super::{DeferredBidirectionalLabeledLaraGraph, Orientation};
use crate::{VertexId, labeled::BucketLabelKey, traits::CsrEdgeTombstone};
use ic_stable_structures::Memory;
use std::fmt;

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
) -> Result<(VertexId, u32, u32), MateLookupError>
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
    Ok((source_neighbor, rank, total))
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

impl<E, M> DeferredBidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    /// Resolves the exact paired physical entry by equal-neighbor occurrence rank.
    pub fn mate_of(&self, edge: PhysicalEdgeRef) -> Result<PhysicalEdgeRef, MateLookupError> {
        if edge.label_id.is_undirected() && matches!(edge.orientation, Orientation::Reverse) {
            return Err(MateLookupError::InvalidOrientation(edge));
        }
        let (neighbor, rank, source_count) = match edge.orientation {
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
        if edge.label_id.is_directed() {
            return Ok(match edge.orientation {
                Orientation::Forward => edge,
                Orientation::Reverse => mate,
            });
        }
        if edge.owner_vertex_id == mate.owner_vertex_id {
            return Ok(edge);
        }
        if edge.owner_vertex_id > mate.owner_vertex_id {
            Ok(edge)
        } else {
            Ok(mate)
        }
    }
}
