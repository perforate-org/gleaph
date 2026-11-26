//! Vertex labels: each vertex's label set and the label -> vertex postings of ADR 0123, stored in
//! the topology store with the vertices, which updates both in one call (ADR 0136, Decision 8).
//! [`VertexLabelStore`] is the only writer and the one place that normalizes a label set.

use core::ops::ControlFlow;
use std::fmt;

use gleaph_graph_kernel::entry::{VertexId, VertexLabelId};

use crate::facade::topology::{self, TopologyError};

/// Vertex label membership in both directions; the sets and their inverse postings cannot
/// disagree, as one store call writes both.
pub struct VertexLabelStore;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexLabelStoreError {
    ReservedLabelId(VertexLabelId),
    /// The store refused the write (for example, the vertex is deleted).
    Topology(TopologyError),
}

impl fmt::Display for VertexLabelStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedLabelId(id) => write!(f, "label id {} is reserved", id.raw()),
            Self::Topology(error) => write!(f, "vertex label write failed: {error}"),
        }
    }
}

impl std::error::Error for VertexLabelStoreError {}

impl VertexLabelStore {
    /// The vertex's labels, ascending; none for an unlabeled or unknown vertex.
    pub fn labels_for(vertex_id: VertexId) -> Vec<VertexLabelId> {
        topology::labels::labels_of(vertex_id).unwrap_or_default()
    }

    /// Whether the vertex has `label`; `false` for an unknown vertex.
    pub fn has_label(vertex_id: VertexId, label: VertexLabelId) -> bool {
        topology::labels::has(vertex_id, label).unwrap_or(false)
    }

    /// Appends to `out`, in ascending id order, up to `limit` vertices that carry `label` and whose
    /// id is at least `from`. Returns the id to resume from, or `None` once the label is exhausted.
    pub fn vertices_with_label(
        label: VertexLabelId,
        from: u32,
        limit: usize,
        out: &mut Vec<VertexId>,
    ) -> Option<u32> {
        let start = out.len();
        if limit > 0 {
            topology::labels::visit_with_label(
                label,
                from.checked_sub(1).map(VertexId::from),
                |vertex_id| {
                    out.push(vertex_id);
                    if out.len() - start == limit {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                },
            );
        }
        if out.len() - start < limit {
            return None;
        }
        out.last().and_then(|last| u32::from(*last).checked_add(1))
    }

    /// Replaces the vertex's label set (sorted, deduplicated, label 0 rejected) and its postings.
    pub fn set_labels(
        vertex_id: VertexId,
        labels: impl IntoIterator<Item = VertexLabelId>,
    ) -> Result<(), VertexLabelStoreError> {
        let next = normalize_labels(labels)?;
        topology::labels::set(vertex_id, &next).map_err(VertexLabelStoreError::Topology)
    }
}

pub(crate) fn normalize_labels(
    labels: impl IntoIterator<Item = VertexLabelId>,
) -> Result<Vec<VertexLabelId>, VertexLabelStoreError> {
    let mut labels: Vec<_> = labels.into_iter().collect();
    if let Some(id) = labels.iter().copied().find(|id| id.is_reserved()) {
        return Err(VertexLabelStoreError::ReservedLabelId(id));
    }
    labels.sort_unstable();
    labels.dedup();
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(raw: u16) -> VertexLabelId {
        VertexLabelId::from_raw(raw)
    }

    fn vertices(count: u32) -> Vec<VertexId> {
        topology::vertex::add_many(count).expect("vertices")
    }

    fn postings(label: VertexLabelId) -> Vec<u32> {
        let mut out = Vec::new();
        assert_eq!(
            VertexLabelStore::vertices_with_label(label, 0, 64, &mut out),
            None
        );
        out.into_iter().map(u32::from).collect()
    }

    #[test]
    fn insert_publishes_one_posting_per_label_and_sorts_the_set() {
        let v = vertices(8);
        VertexLabelStore::set_labels(v[7], [l(3), l(1), l(3)]).unwrap();
        assert_eq!(VertexLabelStore::labels_for(v[7]), vec![l(1), l(3)]);
        assert_eq!(postings(l(1)), vec![7]);
        assert_eq!(postings(l(3)), vec![7]);
        assert!(VertexLabelStore::has_label(v[7], l(3)));
        assert!(!VertexLabelStore::has_label(v[7], l(2)));
    }

    #[test]
    fn label_change_moves_only_the_changed_postings() {
        let v = vertices(3);
        VertexLabelStore::set_labels(v[1], [l(1), l(2)]).unwrap();
        VertexLabelStore::set_labels(v[2], [l(2)]).unwrap();
        VertexLabelStore::set_labels(v[1], [l(2), l(3)]).unwrap();
        assert_eq!(postings(l(1)), Vec::<u32>::new());
        assert_eq!(postings(l(2)), vec![1, 2]);
        assert_eq!(postings(l(3)), vec![1]);
    }

    #[test]
    fn rejected_label_set_writes_nothing() {
        let v = vertices(8);
        VertexLabelStore::set_labels(v[7], [l(2)]).unwrap();
        assert!(matches!(
            VertexLabelStore::set_labels(v[7], [l(3), VertexLabelId::default()]),
            Err(VertexLabelStoreError::ReservedLabelId(id)) if id.raw() == 0
        ));
        assert_eq!(VertexLabelStore::labels_for(v[7]), vec![l(2)]);
        assert_eq!(postings(l(3)), Vec::<u32>::new());
    }

    #[test]
    fn paged_reads_resume_and_stop_at_the_label_boundary() {
        let v = vertices(5);
        for &vertex in &v {
            VertexLabelStore::set_labels(vertex, [l(4)]).unwrap();
        }
        VertexLabelStore::set_labels(v[2], [l(4), l(5)]).unwrap();

        let mut out = Vec::new();
        assert_eq!(
            VertexLabelStore::vertices_with_label(l(4), 0, 2, &mut out),
            Some(2)
        );
        assert_eq!(
            VertexLabelStore::vertices_with_label(l(4), 2, 2, &mut out),
            Some(4)
        );
        assert_eq!(
            VertexLabelStore::vertices_with_label(l(4), 4, 2, &mut out),
            None
        );
        assert_eq!(
            out.into_iter().map(u32::from).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );

        let mut out = Vec::new();
        assert_eq!(
            VertexLabelStore::vertices_with_label(l(5), 0, 8, &mut out),
            None,
            "a short page ends the label"
        );
        assert_eq!(out, vec![v[2]]);
    }
}
