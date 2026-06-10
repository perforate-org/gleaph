//! Label storage domain: vertex label sets plus federated label index events.

use super::super::VertexLabelStoreError;
use super::super::stable::VERTEX_LABELS;
use crate::index::label_pending;
use gleaph_graph_kernel::entry::{Vertex, VertexLabelId};
use ic_stable_lara::VertexId;

use super::GraphStore;

impl GraphStore {
    pub(super) fn commit_set_vertex_labels(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        labels: impl IntoIterator<Item = VertexLabelId>,
    ) -> Result<Vertex, VertexLabelStoreError> {
        let prev = self.vertex_labels(vertex_id, vertex);
        let next: Vec<_> = labels.into_iter().collect();
        label_pending::record_vertex_label_set(vertex_id, &prev, &next);
        VERTEX_LABELS
            .with_borrow_mut(|store| store.set_labels(vertex_id, vertex, next.iter().copied()))
    }

    pub(super) fn commit_add_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Result<Vertex, VertexLabelStoreError> {
        let prev = self.vertex_labels(vertex_id, vertex);
        let mut next = prev.clone();
        next.push(label);
        label_pending::record_vertex_label_set(vertex_id, &prev, &next);
        VERTEX_LABELS.with_borrow_mut(|store| store.add_label(vertex_id, vertex, label))
    }

    pub(super) fn commit_remove_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Vertex {
        let prev = self.vertex_labels(vertex_id, vertex);
        let next: Vec<_> = prev.iter().filter(|l| **l != label).copied().collect();
        label_pending::record_vertex_label_set(vertex_id, &prev, &next);
        VERTEX_LABELS.with_borrow_mut(|store| store.remove_label(vertex_id, vertex, label))
    }

    /// Clear all labels on delete without touching the CSR vertex row.
    pub(super) fn commit_clear_vertex_labels(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
    ) -> Result<(), super::error::GraphStoreError> {
        let prev = self.vertex_labels(vertex_id, vertex);
        label_pending::record_vertex_label_set(vertex_id, &prev, &[]);
        VERTEX_LABELS
            .with_borrow_mut(|labels| labels.set_labels(vertex_id, vertex, []))
            .map(|_| ())
            .map_err(super::error::GraphStoreError::from)
    }
}
