//! Federated label and property resolution catalogs.

use super::super::stable::{
    ROUTER_EDGE_LABEL_BY_ID, ROUTER_EDGE_LABEL_BY_NAME, ROUTER_PROPERTY_BY_ID,
    ROUTER_PROPERTY_BY_NAME, ROUTER_VERTEX_LABEL_BY_ID, ROUTER_VERTEX_LABEL_BY_NAME,
};
use crate::state::RouterError;
use crate::types::{EdgeLabelId, PropertyId, VertexLabelId};
use candid::Principal;
use gleaph_gql_planner::{LabelUseIntent, PhysicalPlan};
use gleaph_graph_kernel::plan_exec::{ResolvedEdgeLabel, ResolvedLabelTable, ResolvedVertexLabel};

use super::{
    RouterStore, intern_edge_label_name, intern_vertex_label_name, validate_metadata_name,
};

impl RouterStore {
    pub fn admin_intern_vertex_label(
        &self,
        caller: Principal,
        name: &str,
    ) -> Result<VertexLabelId, RouterError> {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        validate_metadata_name(name)?;
        intern_vertex_label_name(name)
    }

    pub fn admin_intern_edge_label(
        &self,
        caller: Principal,
        name: &str,
    ) -> Result<EdgeLabelId, RouterError> {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        validate_metadata_name(name)?;
        intern_edge_label_name(name)
    }

    pub fn admin_intern_property(
        &self,
        caller: Principal,
        name: &str,
    ) -> Result<PropertyId, RouterError> {
        if !self.is_controller(caller) {
            return Err(RouterError::NotAuthorized);
        }
        validate_metadata_name(name)?;
        if let Some(id) = ROUTER_PROPERTY_BY_NAME.with_borrow(|m| m.get(&name.to_string())) {
            return Ok(PropertyId::from_raw(id));
        }
        let next_id = ROUTER_PROPERTY_BY_ID.with_borrow(|m| m.keys().max().unwrap_or(0)) + 1;
        ROUTER_PROPERTY_BY_NAME.with_borrow_mut(|m| {
            m.insert(name.to_string(), next_id);
        });
        ROUTER_PROPERTY_BY_ID.with_borrow_mut(|m| {
            m.insert(next_id, name.to_string());
        });
        Ok(PropertyId::from_raw(next_id))
    }

    pub fn lookup_vertex_label_id(&self, name: &str) -> Result<VertexLabelId, RouterError> {
        ROUTER_VERTEX_LABEL_BY_NAME
            .with_borrow(|m| m.get(&name.to_string()))
            .map(VertexLabelId::from_raw)
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn lookup_edge_label_id(&self, name: &str) -> Result<EdgeLabelId, RouterError> {
        ROUTER_EDGE_LABEL_BY_NAME
            .with_borrow(|m| m.get(&name.to_string()))
            .map(EdgeLabelId::from_raw)
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn lookup_property_id(&self, name: &str) -> Result<PropertyId, RouterError> {
        ROUTER_PROPERTY_BY_NAME
            .with_borrow(|m| m.get(&name.to_string()))
            .map(PropertyId::from_raw)
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn reverse_vertex_label_name(
        &self,
        label_id: VertexLabelId,
    ) -> Result<String, RouterError> {
        ROUTER_VERTEX_LABEL_BY_ID
            .with_borrow(|m| m.get(&label_id.raw()))
            .ok_or_else(|| RouterError::NotFound(format!("vertex label id {}", label_id.raw())))
    }

    pub fn reverse_edge_label_name(&self, label_id: EdgeLabelId) -> Result<String, RouterError> {
        ROUTER_EDGE_LABEL_BY_ID
            .with_borrow(|m| m.get(&label_id.raw()))
            .ok_or_else(|| RouterError::NotFound(format!("edge label id {}", label_id.raw())))
    }

    pub fn reverse_property_name(&self, property_id: PropertyId) -> Result<String, RouterError> {
        ROUTER_PROPERTY_BY_ID
            .with_borrow(|m| m.get(&property_id.raw()))
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
                    LabelUseIntent::CreateIfMissing => intern_vertex_label_name(&name)?,
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
                    LabelUseIntent::CreateIfMissing => intern_edge_label_name(&name)?,
                };
                if !out.edge.iter().any(|entry| entry.name == name.as_ref()) {
                    out.edge.push(ResolvedEdgeLabel {
                        name: name.to_string(),
                        id,
                    });
                }
            }
        }
        Ok(out)
    }
}
