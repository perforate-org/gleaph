//! GraphStore property helpers and edge orientation.

use super::super::PropertyCatalogError;
use super::super::stable::GRAPH_DEFAULT_EDGE_LABEL;
use gleaph_graph_kernel::entry::{Edge, PropertyId, TaggedEdgeLabelId};
use ic_stable_lara::{DeferredBidirectionalLabeledError, VertexId};

use super::GraphStore;

impl GraphStore {
    pub(crate) fn edge_is_undirected(
        &self,
        owner_vertex_id: VertexId,
        edge: &Edge,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        let bucket = self
            .find_forward_edge_bucket_label(owner_vertex_id, edge)?
            .unwrap_or(GRAPH_DEFAULT_EDGE_LABEL);
        Ok(TaggedEdgeLabelId::from_raw(bucket.raw()).is_undirected())
    }

    /// Standalone / test name lookup (router owns production name→id resolution).
    pub fn property_id(&self, name: &str) -> Option<PropertyId> {
        #[cfg(any(test, feature = "canbench"))]
        {
            Some(crate::test_labels::property_id_for_name(name))
        }
        #[cfg(not(any(test, feature = "canbench")))]
        {
            let _ = name;
            None
        }
    }

    pub fn property_name(&self, _id: PropertyId) -> Option<String> {
        None
    }

    pub fn get_or_insert_property_id(
        &self,
        name: &str,
    ) -> Result<PropertyId, PropertyCatalogError> {
        #[cfg(any(test, feature = "canbench"))]
        {
            Ok(crate::test_labels::property_id_for_name(name))
        }
        #[cfg(not(any(test, feature = "canbench")))]
        {
            let _ = name;
            Err(PropertyCatalogError::IdExhausted)
        }
    }

    pub fn insert_property_with_id(
        &self,
        _name: &str,
        _id: PropertyId,
    ) -> Result<(), PropertyCatalogError> {
        Err(PropertyCatalogError::IdExhausted)
    }
}
