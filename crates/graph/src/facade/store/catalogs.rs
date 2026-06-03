//! GraphStore `catalogs` implementation.

use super::super::PropertyCatalogError;
use super::super::stable::{
    EDGE_PAYLOAD_PROFILES, EDGE_WEIGHT_PROFILES, GRAPH_DEFAULT_EDGE_LABEL, PROPERTY_CATALOG,
};
use gleaph_graph_kernel::entry::{
    Edge, EdgeLabelId, EdgePayloadProfile, EdgeWeightProfile, PropertyId, TaggedEdgeLabelId,
};
use ic_stable_lara::{DeferredBidirectionalLabeledError, VertexId};

use super::GraphStore;
use super::error::GraphStoreError;

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

    /// Installs weight + derived payload profiles for a catalog label at graph init time only.
    ///
    /// Call before any edge insert using this label. Re-installation returns
    /// [`GraphStoreError::EdgeLabelProfileAlreadyInstalled`].
    pub(crate) fn install_edge_label_weight_profile_at_init(
        &self,
        label: EdgeLabelId,
        profile: EdgeWeightProfile,
    ) -> Result<(), GraphStoreError> {
        Self::ensure_edge_label_payload_profile_uninstalled(label)?;
        let payload_profile = EdgePayloadProfile::from(profile.clone());
        EDGE_WEIGHT_PROFILES.with_borrow_mut(|store| store.insert(label, profile))?;
        EDGE_PAYLOAD_PROFILES.with_borrow_mut(|store| store.insert(label, payload_profile))?;
        Ok(())
    }

    /// Installs a payload profile for a catalog label at graph init time only.
    ///
    /// Call before any edge insert using this label. Re-installation returns
    /// [`GraphStoreError::EdgeLabelProfileAlreadyInstalled`].
    pub(crate) fn install_edge_label_payload_profile_at_init(
        &self,
        label: EdgeLabelId,
        profile: EdgePayloadProfile,
    ) -> Result<(), GraphStoreError> {
        Self::ensure_edge_label_payload_profile_uninstalled(label)?;
        EDGE_PAYLOAD_PROFILES.with_borrow_mut(|store| store.insert(label, profile))?;
        Ok(())
    }

    pub fn edge_label_weight_profile(&self, label: EdgeLabelId) -> Option<EdgeWeightProfile> {
        EDGE_WEIGHT_PROFILES.with_borrow(|store| store.get(label))
    }

    pub fn edge_label_payload_profile(&self, label: EdgeLabelId) -> Option<EdgePayloadProfile> {
        EDGE_PAYLOAD_PROFILES.with_borrow(|store| store.get(label))
    }

    pub(crate) fn remove_edge_label_weight_profile(&self, label: EdgeLabelId) {
        EDGE_WEIGHT_PROFILES.with_borrow_mut(|store| store.remove(label));
        EDGE_PAYLOAD_PROFILES.with_borrow_mut(|store| store.remove(label));
    }

    pub(crate) fn remove_edge_label_payload_profile(&self, label: EdgeLabelId) {
        EDGE_PAYLOAD_PROFILES.with_borrow_mut(|store| store.remove(label));
    }

    fn ensure_edge_label_payload_profile_uninstalled(
        label: EdgeLabelId,
    ) -> Result<(), GraphStoreError> {
        if EDGE_PAYLOAD_PROFILES
            .with_borrow(|store| store.get(label))
            .is_some()
        {
            return Err(GraphStoreError::EdgeLabelProfileAlreadyInstalled(label));
        }
        Ok(())
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
