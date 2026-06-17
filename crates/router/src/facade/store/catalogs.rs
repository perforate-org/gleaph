//! Federated label and property resolution catalogs.

use super::super::stable::{
    ROUTER_EDGE_LABEL_CATALOG, ROUTER_EDGE_PAYLOAD_PROFILES, ROUTER_PROPERTY_CATALOG,
    ROUTER_VERTEX_LABEL_CATALOG,
};
use crate::facade::auth;
use crate::state::RouterError;
use crate::types::{EdgeLabelId, PropertyId, VertexLabelId};
use candid::Principal;
use gleaph_gql_planner::{LabelUseIntent, PhysicalPlan, PropertyUseIntent};
use gleaph_graph_kernel::bidirectional_catalog::CatalogError;
use gleaph_graph_kernel::edge_payload_profile_store::EdgePayloadProfileStoreError;
use gleaph_graph_kernel::entry::EdgePayloadProfile;
use gleaph_graph_kernel::plan_exec::{
    ResolvedEdgeLabel, ResolvedLabelTable, ResolvedProperty, ResolvedPropertyTable,
    ResolvedVertexLabel,
};

use super::{RouterStore, validate_metadata_name};

fn map_catalog_err<Id: std::fmt::Display>(err: CatalogError<Id>) -> RouterError {
    RouterError::InvalidArgument(err.to_string())
}

fn map_edge_payload_profile_err(err: EdgePayloadProfileStoreError) -> RouterError {
    RouterError::InvalidArgument(err.to_string())
}

impl RouterStore {
    pub(super) fn commit_intern_vertex_label_name(
        name: &str,
    ) -> Result<VertexLabelId, RouterError> {
        ROUTER_VERTEX_LABEL_CATALOG
            .with_borrow_mut(|catalog| catalog.get_or_insert(name))
            .map_err(map_catalog_err)
    }

    pub(super) fn commit_intern_edge_label_name(name: &str) -> Result<EdgeLabelId, RouterError> {
        let id = ROUTER_EDGE_LABEL_CATALOG
            .with_borrow_mut(|catalog| catalog.get_or_insert(name))
            .map_err(map_catalog_err)?;
        Self::commit_ensure_edge_label_payload_profile_default(id)?;
        Ok(id)
    }

    pub(super) fn commit_ensure_edge_label_payload_profile_default(
        id: EdgeLabelId,
    ) -> Result<(), RouterError> {
        ROUTER_EDGE_PAYLOAD_PROFILES
            .with_borrow_mut(|store| store.insert_if_absent(id, EdgePayloadProfile::no_payload()))
            .map_err(map_edge_payload_profile_err)
    }

    pub(super) fn commit_set_edge_label_payload_profile(
        id: EdgeLabelId,
        profile: EdgePayloadProfile,
    ) -> Result<(), RouterError> {
        ROUTER_EDGE_PAYLOAD_PROFILES
            .with_borrow_mut(|store| store.insert(id, profile))
            .map_err(map_edge_payload_profile_err)
    }

    fn lookup_edge_payload_profile(&self, id: EdgeLabelId) -> EdgePayloadProfile {
        ROUTER_EDGE_PAYLOAD_PROFILES
            .with_borrow(|store| store.get(id))
            .unwrap_or_else(EdgePayloadProfile::no_payload)
    }

    pub(super) fn commit_intern_property_name(name: &str) -> Result<PropertyId, RouterError> {
        ROUTER_PROPERTY_CATALOG
            .with_borrow_mut(|catalog| catalog.get_or_insert(name))
            .map_err(map_catalog_err)
    }

    pub fn admin_intern_vertex_label(
        &self,
        caller: Principal,
        name: &str,
    ) -> Result<VertexLabelId, RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(name)?;
        Self::commit_intern_vertex_label_name(name)
    }

    pub fn admin_intern_edge_label(
        &self,
        caller: Principal,
        name: &str,
    ) -> Result<EdgeLabelId, RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(name)?;
        Self::commit_intern_edge_label_name(name)
    }

    pub fn admin_intern_property(
        &self,
        caller: Principal,
        name: &str,
    ) -> Result<PropertyId, RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(name)?;
        Self::commit_intern_property_name(name)
    }

    pub fn admin_set_edge_label_payload_profile(
        &self,
        caller: Principal,
        name: &str,
        profile: EdgePayloadProfile,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(name)?;
        let id = self.lookup_edge_label_id(name)?;
        Self::commit_set_edge_label_payload_profile(id, profile)
    }

    pub fn lookup_vertex_label_id(&self, name: &str) -> Result<VertexLabelId, RouterError> {
        ROUTER_VERTEX_LABEL_CATALOG
            .with_borrow(|catalog| catalog.get_id(name))
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn lookup_edge_label_id(&self, name: &str) -> Result<EdgeLabelId, RouterError> {
        ROUTER_EDGE_LABEL_CATALOG
            .with_borrow(|catalog| catalog.get_id(name))
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn lookup_property_id(&self, name: &str) -> Result<PropertyId, RouterError> {
        ROUTER_PROPERTY_CATALOG
            .with_borrow(|catalog| catalog.get_id(name))
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn reverse_vertex_label_name(
        &self,
        label_id: VertexLabelId,
    ) -> Result<String, RouterError> {
        ROUTER_VERTEX_LABEL_CATALOG
            .with_borrow(|catalog| catalog.get_name(label_id))
            .ok_or_else(|| RouterError::NotFound(format!("vertex label id {}", label_id.raw())))
    }

    pub fn reverse_edge_label_name(&self, label_id: EdgeLabelId) -> Result<String, RouterError> {
        ROUTER_EDGE_LABEL_CATALOG
            .with_borrow(|catalog| catalog.get_name(label_id))
            .ok_or_else(|| RouterError::NotFound(format!("edge label id {}", label_id.raw())))
    }

    pub fn reverse_property_name(&self, property_id: PropertyId) -> Result<String, RouterError> {
        ROUTER_PROPERTY_CATALOG
            .with_borrow(|catalog| catalog.get_name(property_id))
            .ok_or_else(|| RouterError::NotFound(format!("property id {}", property_id.raw())))
    }

    pub fn resolve_plan_labels(
        &self,
        plans: &[PhysicalPlan],
    ) -> Result<ResolvedLabelTable, RouterError> {
        let mut out = ResolvedLabelTable::default();
        for plan in plans {
            let uses = plan.label_uses();
            for (name, intent) in uses.node_labels {
                validate_metadata_name(&name)?;
                let id = match intent {
                    LabelUseIntent::ReadExisting => self.lookup_vertex_label_id(&name)?,
                    LabelUseIntent::CreateIfMissing => {
                        Self::commit_intern_vertex_label_name(&name)?
                    }
                };
                if !out.vertex.iter().any(|entry| entry.name == name.as_ref()) {
                    out.vertex.push(ResolvedVertexLabel {
                        name: name.to_string(),
                        id,
                    });
                }
            }
            for (name, intent) in uses.edge_labels {
                validate_metadata_name(&name)?;
                let id = match intent {
                    LabelUseIntent::ReadExisting => self.lookup_edge_label_id(&name)?,
                    LabelUseIntent::CreateIfMissing => Self::commit_intern_edge_label_name(&name)?,
                };
                if !out.edge.iter().any(|entry| entry.name == name.as_ref()) {
                    out.edge.push(ResolvedEdgeLabel::new(
                        name.to_string(),
                        id,
                        self.lookup_edge_payload_profile(id),
                    ));
                }
            }
        }
        self.enrich_edge_labels_for_predicate_fusion(&mut out)?;
        Ok(out)
    }

    fn enrich_edge_labels_for_predicate_fusion(
        &self,
        table: &mut ResolvedLabelTable,
    ) -> Result<(), RouterError> {
        let fusion_ids = ROUTER_EDGE_PAYLOAD_PROFILES
            .with_borrow(|store| store.label_ids_with_nonzero_payload());
        for id in fusion_ids {
            if table.edge.iter().any(|entry| entry.id == id) {
                continue;
            }
            let name = self.reverse_edge_label_name(id)?;
            table.edge.push(ResolvedEdgeLabel::new(
                name,
                id,
                self.lookup_edge_payload_profile(id),
            ));
        }
        Ok(())
    }

    pub fn resolve_plan_properties(
        &self,
        plans: &[PhysicalPlan],
    ) -> Result<ResolvedPropertyTable, RouterError> {
        let mut out = ResolvedPropertyTable::default();
        for plan in plans {
            let uses = plan.property_uses();
            for (name, intent) in uses.properties {
                validate_metadata_name(&name)?;
                let id = match intent {
                    PropertyUseIntent::ReadExisting => self.lookup_property_id(&name)?,
                    PropertyUseIntent::CreateIfMissing => Self::commit_intern_property_name(&name)?,
                };
                if !out
                    .properties
                    .iter()
                    .any(|entry| entry.name == name.as_ref())
                {
                    out.properties.push(ResolvedProperty {
                        name: name.to_string(),
                        id,
                    });
                }
            }
        }
        Ok(out)
    }
}
