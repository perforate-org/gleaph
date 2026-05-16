use super::store::{EdgeHandle, GraphStore, GraphStoreError};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId, VertexLabelId};
use ic_stable_lara::VertexId;

pub trait GraphMutationExecutor {
    fn insert_vertex_with(
        &self,
        labels: impl IntoIterator<Item = VertexLabelId>,
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<VertexId, GraphStoreError>;

    fn insert_directed_edge_with(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        label: Option<EdgeLabelId>,
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError>;

    fn insert_undirected_edge_with(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        label: Option<EdgeLabelId>,
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError>;

    fn insert_vertex_named(
        &self,
        labels: impl IntoIterator<Item = impl AsRef<str>>,
        properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
    ) -> Result<VertexId, GraphStoreError>;

    fn insert_directed_edge_named(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        label: Option<impl AsRef<str>>,
        properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError>;

    fn insert_undirected_edge_named(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        label: Option<impl AsRef<str>>,
        properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError>;
}

impl GraphMutationExecutor for GraphStore {
    fn insert_vertex_with(
        &self,
        labels: impl IntoIterator<Item = VertexLabelId>,
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<VertexId, GraphStoreError> {
        let vertex_id = self.insert_vertex()?;
        let vertex = self
            .vertex(vertex_id)
            .expect("newly inserted vertex must be readable");
        let vertex = self.set_vertex_labels(vertex_id, vertex, labels)?;
        self.set_vertex(vertex_id, vertex)?;

        for (property_id, value) in properties {
            self.set_vertex_property(vertex_id, property_id, value)?;
        }

        Ok(vertex_id)
    }

    fn insert_directed_edge_with(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        label: Option<EdgeLabelId>,
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let handle = self.insert_directed_edge(source_vertex_id, target_vertex_id, label)?;
        for (property_id, value) in properties {
            self.set_edge_property(
                handle.owner_vertex_id,
                handle.vertex_edge_id,
                property_id,
                value,
            )?;
        }
        Ok(handle)
    }

    fn insert_undirected_edge_with(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        label: Option<EdgeLabelId>,
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let handle = self.insert_undirected_edge(endpoint_a, endpoint_b, label)?;
        for (property_id, value) in properties {
            self.set_edge_property(
                handle.owner_vertex_id,
                handle.vertex_edge_id,
                property_id,
                value,
            )?;
        }
        Ok(handle)
    }

    fn insert_vertex_named(
        &self,
        labels: impl IntoIterator<Item = impl AsRef<str>>,
        properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
    ) -> Result<VertexId, GraphStoreError> {
        let labels = labels
            .into_iter()
            .map(|label| self.get_or_insert_vertex_label_id(label.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        let properties = resolve_properties(self, properties)?;
        self.insert_vertex_with(labels, properties)
    }

    fn insert_directed_edge_named(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        label: Option<impl AsRef<str>>,
        properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let label = label
            .map(|label| self.get_or_insert_edge_label_id(label.as_ref()))
            .transpose()?;
        let properties = resolve_properties(self, properties)?;
        self.insert_directed_edge_with(source_vertex_id, target_vertex_id, label, properties)
    }

    fn insert_undirected_edge_named(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        label: Option<impl AsRef<str>>,
        properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let label = label
            .map(|label| self.get_or_insert_edge_label_id(label.as_ref()))
            .transpose()?;
        let properties = resolve_properties(self, properties)?;
        self.insert_undirected_edge_with(endpoint_a, endpoint_b, label, properties)
    }
}

fn resolve_properties(
    store: &GraphStore,
    properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
) -> Result<Vec<(PropertyId, Value)>, GraphStoreError> {
    properties
        .into_iter()
        .map(|(name, value)| {
            store
                .get_or_insert_property_id(name.as_ref())
                .map(|id| (id, value))
                .map_err(GraphStoreError::from)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId, TaggedEdgeLabelId};
    use ic_stable_lara::{BucketLabelKey as LaraLabelId, CsrEdge};

    #[test]
    fn inserts_edges_with_labels_and_properties() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("insert source");
        let target = store.insert_vertex().expect("insert target");
        let directed_label = EdgeLabelId::from_raw(123);
        let undirected_label = EdgeLabelId::from_raw(124);
        let property = PropertyId::from_raw(234);

        let directed = store
            .insert_directed_edge_with(
                source,
                target,
                Some(directed_label),
                [(property, Value::Text("knows".into()))],
            )
            .expect("insert directed edge");

        assert_eq!(directed.owner_vertex_id, source);
        assert_eq!(
            store.edge_property(directed.owner_vertex_id, directed.vertex_edge_id, property),
            Some(Value::Text("knows".into()))
        );
        assert!(store.out_edges(source).unwrap().iter().any(|edge| {
            edge.neighbor_vid() == target
                && edge.vertex_edge_id == directed.vertex_edge_id
                && store.find_forward_edge_bucket_label(source, edge).unwrap()
                    == Some(LaraLabelId::from_raw(
                        directed_label.pack(EdgeDirectedness::Directed).raw(),
                    ))
        }));

        let undirected = store
            .insert_undirected_edge_with(
                target,
                source,
                Some(undirected_label),
                [(property, Value::Text("related".into()))],
            )
            .expect("insert undirected edge");

        assert_eq!(undirected.owner_vertex_id, target);
        assert!(store.out_edges(target).unwrap().iter().any(|edge| {
            edge.neighbor_vid() == source
                && edge.vertex_edge_id == undirected.vertex_edge_id
                && store
                    .find_forward_edge_bucket_label(target, edge)
                    .map(|l| l.map(|id| TaggedEdgeLabelId::from_raw(id.raw())))
                    .ok()
                    .flatten()
                    .is_some_and(|id| id.is_undirected())
        }));
    }

    #[test]
    fn named_vertex_mutation_resolves_catalog_entries() {
        let store = GraphStore::new();

        let vertex_id = store
            .insert_vertex_named(["Person"], [("name", Value::Text("Alice".into()))])
            .expect("insert named vertex");
        let vertex = store.vertex(vertex_id).expect("read vertex");
        let label = store.vertex_label_id("Person").expect("person label id");
        let property = store.property_id("name").expect("name property id");

        assert_eq!(store.vertex_labels(vertex_id, vertex), vec![label]);
        assert_eq!(
            store.vertex_property(vertex_id, property),
            Some(Value::Text("Alice".into()))
        );
    }
}
