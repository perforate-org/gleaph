use super::store::{EdgeHandle, GraphStore, GraphStoreError};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{EdgeMeta, InlineEdgeLabelId, LabelId, PropertyId};
use ic_stable_lara::VertexId;

pub trait GraphMutationExecutor {
    fn insert_vertex_with(
        &self,
        labels: impl IntoIterator<Item = LabelId>,
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<VertexId, GraphStoreError>;

    fn insert_directed_edge_with(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        label: Option<LabelId>,
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError>;

    fn insert_undirected_edge_with(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        label: Option<LabelId>,
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
        labels: impl IntoIterator<Item = LabelId>,
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
        label: Option<LabelId>,
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let handle =
            self.insert_directed_edge(source_vertex_id, target_vertex_id, edge_meta(label)?)?;
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
        label: Option<LabelId>,
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let handle = self.insert_undirected_edge(endpoint_a, endpoint_b, edge_meta(label)?)?;
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

fn edge_meta(label: Option<LabelId>) -> Result<EdgeMeta, GraphStoreError> {
    let inline = match label {
        None => None,
        Some(l) if l.raw() == 0 => None,
        Some(l) => Some(
            InlineEdgeLabelId::from_label_id(l).ok_or(GraphStoreError::InvalidEdgeLabelId(l))?,
        ),
    };
    Ok(EdgeMeta::new(false, false, inline))
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

    #[test]
    fn inserts_vertex_with_labels_and_properties() {
        let store = GraphStore::new();
        let label = LabelId::from_raw(0x4000 + 111);
        let property = PropertyId::from_raw(222);

        let vertex_id = store
            .insert_vertex_with([label], [(property, Value::Text("Alice".into()))])
            .expect("insert vertex");
        let vertex = store.vertex(vertex_id).expect("read vertex");

        assert_eq!(store.vertex_labels(vertex_id, vertex), vec![label]);
        assert_eq!(
            store.vertex_property(vertex_id, property),
            Some(Value::Text("Alice".into()))
        );
    }

    #[test]
    fn inserts_edges_with_labels_and_properties() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("insert source");
        let target = store.insert_vertex().expect("insert target");
        let label = LabelId::from_raw(123);
        let property = PropertyId::from_raw(234);

        let directed = store
            .insert_directed_edge_with(
                source,
                target,
                Some(label),
                [(property, Value::Text("knows".into()))],
            )
            .expect("insert directed edge");

        assert_eq!(directed.owner_vertex_id, source);
        assert_eq!(
            store.edge_property(directed.owner_vertex_id, directed.vertex_edge_id, property),
            Some(Value::Text("knows".into()))
        );
        assert!(store.out_edges(source).unwrap().iter().any(|edge| {
            edge.target == target
                && edge.vertex_edge_id == directed.vertex_edge_id
                && edge.meta.inline_label_bits() == label.raw()
        }));

        let undirected = store
            .insert_undirected_edge_with(
                target,
                source,
                Some(label),
                [(property, Value::Text("related".into()))],
            )
            .expect("insert undirected edge");

        assert_eq!(undirected.owner_vertex_id, target);
        assert_eq!(
            store.edge_property(
                undirected.owner_vertex_id,
                undirected.vertex_edge_id,
                property
            ),
            Some(Value::Text("related".into()))
        );
        assert!(store.out_edges(target).unwrap().iter().any(|edge| {
            edge.target == source
                && edge.vertex_edge_id == undirected.vertex_edge_id
                && edge.meta.inline_label_bits() == label.raw()
                && edge.meta.is_undirected()
        }));
    }

    #[test]
    fn named_vertex_mutation_resolves_catalog_entries() {
        let store = GraphStore::new();

        let vertex_id = store
            .insert_vertex_named(["Person"], [("name", Value::Text("Alice".into()))])
            .expect("insert named vertex");
        let vertex = store.vertex(vertex_id).expect("read vertex");
        let label = store.label_id("Person").expect("person label id");
        let property = store.property_id("name").expect("name property id");

        assert_eq!(store.vertex_labels(vertex_id, vertex), vec![label]);
        assert_eq!(
            store.vertex_property(vertex_id, property),
            Some(Value::Text("Alice".into()))
        );
    }

    #[test]
    fn named_edge_mutation_resolves_catalog_entries() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("insert source");
        let target = store.insert_vertex().expect("insert target");

        let handle = store
            .insert_directed_edge_named(
                source,
                target,
                Some("KNOWS"),
                [("since", Value::Int64(2026))],
            )
            .expect("insert named edge");
        let label = store.label_id("KNOWS").expect("knows label id");
        let property = store.property_id("since").expect("since property id");

        assert_eq!(
            store.edge_property(handle.owner_vertex_id, handle.vertex_edge_id, property),
            Some(Value::Int64(2026))
        );
        assert!(store.out_edges(source).unwrap().iter().any(|edge| {
            edge.target == target
                && edge.vertex_edge_id == handle.vertex_edge_id
                && edge.meta.inline_label_bits() == label.raw()
        }));
    }
}
