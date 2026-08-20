//! GraphStore `vertex_labels` implementation.

use super::super::stable::VERTEX_LABELS;
use super::error::GraphStoreError;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{Vertex, VertexLabelId};
use gleaph_graph_kernel::plan_exec::{MutationId, ResolvedLabelTable};
use ic_stable_lara::VertexId;

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
    ) -> Result<Vertex, GraphStoreError> {
        self.set_vertex_labels_with_mutation_id(vertex_id, vertex, labels, 0)
    }

    pub fn add_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Result<Vertex, GraphStoreError> {
        self.add_vertex_label_with_mutation_id(vertex_id, vertex, label, 0)
    }

    pub fn remove_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Result<Vertex, GraphStoreError> {
        self.remove_vertex_label_with_mutation_id(vertex_id, vertex, label, 0)
    }

    pub(crate) fn set_vertex_labels_with_mutation_id(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        labels: impl IntoIterator<Item = VertexLabelId>,
        mutation_id: MutationId,
    ) -> Result<Vertex, GraphStoreError> {
        self.apply_vertex_label_transition(vertex_id, vertex, labels, mutation_id)
    }

    pub(crate) fn add_vertex_label_with_mutation_id(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
        mutation_id: MutationId,
    ) -> Result<Vertex, GraphStoreError> {
        let mut labels = self.vertex_labels(vertex_id, vertex);
        labels.push(label);
        self.apply_vertex_label_transition(vertex_id, vertex, labels, mutation_id)
    }

    pub(crate) fn remove_vertex_label_with_mutation_id(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
        mutation_id: MutationId,
    ) -> Result<Vertex, GraphStoreError> {
        let labels = self
            .vertex_labels(vertex_id, vertex)
            .into_iter()
            .filter(|current| *current != label);
        self.apply_vertex_label_transition(vertex_id, vertex, labels, mutation_id)
    }
}
