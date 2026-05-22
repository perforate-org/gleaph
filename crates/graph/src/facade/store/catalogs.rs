//! GraphStore `catalogs` implementation.

use super::super::PropertyCatalogError;
use super::super::stable::edge_label_catalog::EdgeLabelCatalogError;
use super::super::stable::vertex_label_catalog::VertexLabelCatalogError;
use super::super::stable::{
    EDGE_LABEL_CATALOG, EDGE_VALUE_PROFILES, EDGE_WEIGHT_PROFILES, GRAPH_DEFAULT_EDGE_LABEL,
    PROPERTY_CATALOG, VERTEX_LABEL_CATALOG,
};
use gleaph_graph_kernel::entry::{
    Edge, EdgeLabelId, EdgeValueProfile, EdgeWeightProfile, PropertyId, TaggedEdgeLabelId,
    VertexLabelId,
};
use ic_stable_lara::{DeferredBidirectionalLabeledError, VertexId};

use super::GraphStore;
use super::error::GraphStoreError;

impl GraphStore {
    pub fn vertex_label_id(&self, name: &str) -> Option<VertexLabelId> {
        VERTEX_LABEL_CATALOG.with_borrow(|catalog| catalog.get_id(name))
    }

    pub fn edge_label_id(&self, name: &str) -> Option<EdgeLabelId> {
        EDGE_LABEL_CATALOG.with_borrow(|catalog| catalog.get_id(name))
    }

    pub fn edge_label_tagged_directed(&self, name: &str) -> Option<TaggedEdgeLabelId> {
        EDGE_LABEL_CATALOG.with_borrow(|catalog| catalog.get_tagged_directed(name))
    }

    pub fn edge_label_tagged_undirected(&self, name: &str) -> Option<TaggedEdgeLabelId> {
        EDGE_LABEL_CATALOG.with_borrow(|catalog| catalog.get_tagged_undirected(name))
    }

    pub fn vertex_label_name(&self, id: VertexLabelId) -> Option<String> {
        VERTEX_LABEL_CATALOG.with_borrow(|catalog| catalog.get_name(id))
    }

    pub fn edge_label_name(&self, id: EdgeLabelId) -> Option<String> {
        EDGE_LABEL_CATALOG.with_borrow(|catalog| catalog.get_name(id))
    }

    pub fn get_or_insert_vertex_label_id(
        &self,
        name: &str,
    ) -> Result<VertexLabelId, VertexLabelCatalogError> {
        VERTEX_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.get_or_insert(name))
    }

    pub fn get_or_insert_edge_label_id(
        &self,
        name: &str,
    ) -> Result<EdgeLabelId, EdgeLabelCatalogError> {
        EDGE_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.get_or_insert(name))
    }

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

    pub fn insert_vertex_label_with_id(
        &self,
        name: &str,
        id: VertexLabelId,
    ) -> Result<(), VertexLabelCatalogError> {
        VERTEX_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.insert_with_id(name, id))
    }

    pub fn insert_edge_label_with_id(
        &self,
        name: &str,
        id: EdgeLabelId,
    ) -> Result<(), EdgeLabelCatalogError> {
        EDGE_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.insert_with_id(name, id))
    }

    pub fn set_edge_label_weight_profile(
        &self,
        label: EdgeLabelId,
        profile: EdgeWeightProfile,
    ) -> Result<(), GraphStoreError> {
        let value_profile = EdgeValueProfile::from(profile.clone());
        EDGE_WEIGHT_PROFILES.with_borrow_mut(|store| store.insert(label, profile))?;
        EDGE_VALUE_PROFILES.with_borrow_mut(|store| store.insert(label, value_profile))?;
        Ok(())
    }

    pub fn set_edge_label_value_profile(
        &self,
        label: EdgeLabelId,
        profile: EdgeValueProfile,
    ) -> Result<(), GraphStoreError> {
        EDGE_VALUE_PROFILES.with_borrow_mut(|store| store.insert(label, profile))?;
        Ok(())
    }

    pub fn edge_label_weight_profile(&self, label: EdgeLabelId) -> Option<EdgeWeightProfile> {
        EDGE_WEIGHT_PROFILES.with_borrow(|store| store.get(label))
    }

    pub fn edge_label_value_profile(&self, label: EdgeLabelId) -> Option<EdgeValueProfile> {
        EDGE_VALUE_PROFILES.with_borrow(|store| store.get(label))
    }

    pub fn remove_edge_label_weight_profile(&self, label: EdgeLabelId) {
        EDGE_WEIGHT_PROFILES.with_borrow_mut(|store| store.remove(label));
        EDGE_VALUE_PROFILES.with_borrow_mut(|store| store.remove(label));
    }

    pub fn remove_edge_label_value_profile(&self, label: EdgeLabelId) {
        EDGE_VALUE_PROFILES.with_borrow_mut(|store| store.remove(label));
    }

    pub fn property_id(&self, name: &str) -> Option<PropertyId> {
        PROPERTY_CATALOG.with_borrow(|catalog| catalog.get_id(name))
    }

    pub fn property_name(&self, id: PropertyId) -> Option<String> {
        PROPERTY_CATALOG.with_borrow(|catalog| catalog.get_name(id))
    }

    pub fn get_or_insert_property_id(
        &self,
        name: &str,
    ) -> Result<PropertyId, PropertyCatalogError> {
        PROPERTY_CATALOG.with_borrow_mut(|catalog| catalog.get_or_insert(name))
    }

    pub fn insert_property_with_id(
        &self,
        name: &str,
        id: PropertyId,
    ) -> Result<(), PropertyCatalogError> {
        PROPERTY_CATALOG.with_borrow_mut(|catalog| catalog.insert_with_id(name, id))
    }
}
