//! GraphStore `vertex_labels` implementation.

use super::super::VertexLabelStoreError;
use super::super::stable::VERTEX_LABELS;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{Vertex, VertexLabelId};
use ic_stable_lara::VertexId;

use super::GraphStore;

impl GraphStore {
    pub fn vertex_labels(&self, vertex_id: VertexId, vertex: Vertex) -> Vec<VertexLabelId> {
        VERTEX_LABELS.with_borrow(|labels| labels.labels_for(vertex_id, vertex))
    }

    pub(crate) fn vertex_label_gql_list(&self, vertex_id: VertexId, vertex: Vertex) -> Vec<Value> {
        VERTEX_LABELS.with_borrow(|labels| {
            labels.with_label_ids(vertex_id, vertex, |slice| {
                let mut out = Vec::with_capacity(slice.len());
                for &label in slice {
                    out.push(
                        self.vertex_label_name(label)
                            .map(Value::Text)
                            .unwrap_or_else(|| Value::Uint64(u64::from(label.raw()))),
                    );
                }
                out
            })
        })
    }

    pub(crate) fn vertex_has_any_label(&self, vertex_id: VertexId, vertex: Vertex) -> bool {
        VERTEX_LABELS.with_borrow(|labels| {
            labels.with_label_ids(vertex_id, vertex, |slice| !slice.is_empty())
        })
    }

    pub fn vertex_has_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label_id: VertexLabelId,
    ) -> bool {
        VERTEX_LABELS.with_borrow(|labels| {
            labels.with_label_ids(vertex_id, vertex, |slice| slice.contains(&label_id))
        })
    }

    pub fn set_vertex_labels(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        labels: impl IntoIterator<Item = VertexLabelId>,
    ) -> Result<Vertex, VertexLabelStoreError> {
        let prev: Vec<_> = self.vertex_labels(vertex_id, vertex);
        let next: Vec<_> = labels.into_iter().collect();
        let out = VERTEX_LABELS
            .with_borrow_mut(|store| store.set_labels(vertex_id, vertex, next.iter().copied()))?;
        use crate::facade::migration::incremental::{
            journal_vertex_label_added, journal_vertex_label_removed,
        };
        for label in prev.iter().filter(|l| !next.contains(l)) {
            let _ = journal_vertex_label_removed(self, vertex_id, *label);
        }
        for label in next.iter().filter(|l| !prev.contains(l)) {
            let _ = journal_vertex_label_added(self, vertex_id, *label);
        }
        Ok(out)
    }

    pub fn add_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Result<Vertex, VertexLabelStoreError> {
        let out =
            VERTEX_LABELS.with_borrow_mut(|store| store.add_label(vertex_id, vertex, label))?;
        let _ = crate::facade::migration::incremental::journal_vertex_label_added(
            self, vertex_id, label,
        );
        Ok(out)
    }

    pub fn remove_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Vertex {
        let out =
            VERTEX_LABELS.with_borrow_mut(|store| store.remove_label(vertex_id, vertex, label));
        let _ = crate::facade::migration::incremental::journal_vertex_label_removed(
            self, vertex_id, label,
        );
        out
    }
}
