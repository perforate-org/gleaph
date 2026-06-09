//! GraphStore `vertex_labels` implementation.

use super::super::VertexLabelStoreError;
use super::super::stable::VERTEX_LABELS;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{Vertex, VertexLabelId};
use gleaph_graph_kernel::plan_exec::ResolvedLabelTable;
use ic_stable_lara::VertexId;

use crate::index::label_pending;

use super::GraphStore;

impl GraphStore {
    pub fn vertex_labels(&self, vertex_id: VertexId, vertex: Vertex) -> Vec<VertexLabelId> {
        VERTEX_LABELS.with_borrow(|labels| labels.labels_for(vertex_id, vertex))
    }

    pub(crate) fn vertex_label_gql_list(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        resolved_labels: Option<&ResolvedLabelTable>,
    ) -> Vec<Value> {
        VERTEX_LABELS.with_borrow(|labels| {
            labels.with_label_ids(vertex_id, vertex, |slice| {
                let mut out = Vec::with_capacity(slice.len());
                for &label in slice {
                    let value = resolved_labels
                        .and_then(|labels| {
                            labels
                                .vertex
                                .iter()
                                .find(|entry| entry.id == label)
                                .map(|entry| Value::Text(entry.name.clone()))
                        })
                        .unwrap_or_else(|| Value::Uint64(u64::from(label.raw())));
                    out.push(value);
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
        let prev = self.vertex_labels(vertex_id, vertex);
        let next: Vec<_> = labels.into_iter().collect();
        label_pending::record_vertex_label_set(vertex_id, &prev, &next);
        VERTEX_LABELS
            .with_borrow_mut(|store| store.set_labels(vertex_id, vertex, next.iter().copied()))
    }

    pub fn add_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Result<Vertex, VertexLabelStoreError> {
        let prev = self.vertex_labels(vertex_id, vertex);
        let mut next = prev.clone();
        next.push(label);
        label_pending::record_vertex_label_set(vertex_id, &prev, &next);
        let out =
            VERTEX_LABELS.with_borrow_mut(|store| store.add_label(vertex_id, vertex, label))?;
        Ok(out)
    }

    pub fn remove_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Vertex {
        let prev = self.vertex_labels(vertex_id, vertex);
        let next: Vec<_> = prev.iter().filter(|l| **l != label).copied().collect();
        label_pending::record_vertex_label_set(vertex_id, &prev, &next);
        let out =
            VERTEX_LABELS.with_borrow_mut(|store| store.remove_label(vertex_id, vertex, label));
        out
    }
}
