//! Federated label and property resolution catalogs (ADR 0018 graph-scoped vocabulary).

use super::super::stable::{
    ROUTER_EDGE_LABEL_CATALOG, ROUTER_EDGE_PAYLOAD_PROFILES, ROUTER_PROPERTY_CATALOG,
    ROUTER_VERTEX_LABEL_CATALOG,
};
use crate::facade::auth;
use crate::facade::stable::constraint_catalog::{self as constraint_store, ConstraintDefRecord};
use crate::facade::stable::constraint_name_catalog::{
    intern_constraint_name, lookup_constraint_name_id,
};
use crate::facade::stable::edge_payload_profiles::EdgePayloadProfileStoreError;
use crate::facade::stable::graph_catalog::{catalog_error_to_router, resolve_registered_graph_id};
use crate::state::RouterError;
use crate::types::{EdgeLabelId, PropertyId, VertexLabelId};
use candid::Principal;
use gleaph_gql::type_check::collect_graph_type_vocabulary;
use gleaph_gql_planner::{LabelUseIntent, PhysicalPlan, PropertyUseIntent};
use gleaph_graph_kernel::entry::{EdgePayloadProfile, GraphId};
use gleaph_graph_kernel::plan_exec::{
    ResolvedEdgeLabel, ResolvedLabelTable, ResolvedProperty, ResolvedPropertyTable,
    ResolvedVertexLabel,
};

use super::{RouterStore, validate_metadata_name};

fn map_edge_payload_profile_err(err: EdgePayloadProfileStoreError) -> RouterError {
    RouterError::InvalidArgument(err.to_string())
}

impl RouterStore {
    pub(crate) fn commit_intern_graph_type_vocabulary(
        graph_id: GraphId,
        def: &gleaph_gql::ast::GraphTypeDefinition,
    ) -> Result<(), RouterError> {
        let vocabulary = collect_graph_type_vocabulary(def);
        for name in &vocabulary.vertex_labels {
            validate_metadata_name(name)?;
            Self::commit_intern_vertex_label_name(graph_id, name)?;
        }
        for name in &vocabulary.edge_labels {
            validate_metadata_name(name)?;
            Self::commit_intern_edge_label_name(graph_id, name)?;
        }
        for name in &vocabulary.properties {
            validate_metadata_name(name)?;
            Self::commit_intern_property_name(graph_id, name)?;
        }
        Ok(())
    }

    pub(super) fn commit_intern_vertex_label_name(
        graph_id: GraphId,
        name: &str,
    ) -> Result<VertexLabelId, RouterError> {
        ROUTER_VERTEX_LABEL_CATALOG
            .with_borrow_mut(|catalog| catalog.get_or_insert(graph_id, name))
            .map_err(|e| catalog_error_to_router(e, "vertex label"))
    }

    pub(super) fn commit_intern_edge_label_name(
        graph_id: GraphId,
        name: &str,
    ) -> Result<EdgeLabelId, RouterError> {
        let id = ROUTER_EDGE_LABEL_CATALOG
            .with_borrow_mut(|catalog| catalog.get_or_insert(graph_id, name))
            .map_err(|e| catalog_error_to_router(e, "edge label"))?;
        Self::commit_ensure_edge_label_payload_profile_default(graph_id, id)?;
        Ok(id)
    }

    pub(super) fn commit_ensure_edge_label_payload_profile_default(
        graph_id: GraphId,
        id: EdgeLabelId,
    ) -> Result<(), RouterError> {
        ROUTER_EDGE_PAYLOAD_PROFILES
            .with_borrow_mut(|store| {
                store.insert_if_absent(graph_id, id, EdgePayloadProfile::no_payload())
            })
            .map_err(map_edge_payload_profile_err)
    }

    pub(super) fn commit_set_edge_label_payload_profile(
        graph_id: GraphId,
        id: EdgeLabelId,
        profile: EdgePayloadProfile,
    ) -> Result<(), RouterError> {
        ROUTER_EDGE_PAYLOAD_PROFILES
            .with_borrow_mut(|store| store.insert(graph_id, id, profile))
            .map_err(map_edge_payload_profile_err)
    }

    fn lookup_edge_payload_profile(
        &self,
        graph_id: GraphId,
        id: EdgeLabelId,
    ) -> EdgePayloadProfile {
        ROUTER_EDGE_PAYLOAD_PROFILES
            .with_borrow(|store| store.get(graph_id, id))
            .unwrap_or_else(EdgePayloadProfile::no_payload)
    }

    pub(super) fn commit_intern_property_name(
        graph_id: GraphId,
        name: &str,
    ) -> Result<PropertyId, RouterError> {
        ROUTER_PROPERTY_CATALOG
            .with_borrow_mut(|catalog| catalog.get_or_insert(graph_id, name))
            .map_err(|e| catalog_error_to_router(e, "property"))
    }

    pub fn admin_intern_vertex_label(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        name: &str,
    ) -> Result<VertexLabelId, RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(logical_graph_name)?;
        validate_metadata_name(name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        Self::commit_intern_vertex_label_name(graph_id, name)
    }

    pub fn admin_intern_edge_label(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        name: &str,
    ) -> Result<EdgeLabelId, RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(logical_graph_name)?;
        validate_metadata_name(name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        Self::commit_intern_edge_label_name(graph_id, name)
    }

    pub fn admin_intern_property(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        name: &str,
    ) -> Result<PropertyId, RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(logical_graph_name)?;
        validate_metadata_name(name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        Self::commit_intern_property_name(graph_id, name)
    }

    pub fn admin_set_edge_label_payload_profile(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        name: &str,
        profile: EdgePayloadProfile,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(logical_graph_name)?;
        validate_metadata_name(name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        let id = self.lookup_edge_label_id(graph_id, name)?;
        Self::commit_set_edge_label_payload_profile(graph_id, id, profile)
    }

    pub fn lookup_vertex_label_id(
        &self,
        graph_id: GraphId,
        name: &str,
    ) -> Result<VertexLabelId, RouterError> {
        ROUTER_VERTEX_LABEL_CATALOG
            .with_borrow(|catalog| catalog.get_id(graph_id, name))
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn lookup_edge_label_id(
        &self,
        graph_id: GraphId,
        name: &str,
    ) -> Result<EdgeLabelId, RouterError> {
        ROUTER_EDGE_LABEL_CATALOG
            .with_borrow(|catalog| catalog.get_id(graph_id, name))
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn lookup_property_id(
        &self,
        graph_id: GraphId,
        name: &str,
    ) -> Result<PropertyId, RouterError> {
        ROUTER_PROPERTY_CATALOG
            .with_borrow(|catalog| catalog.get_id(graph_id, name))
            .ok_or_else(|| RouterError::NotFound(name.to_owned()))
    }

    pub fn reverse_vertex_label_name(
        &self,
        graph_id: GraphId,
        label_id: VertexLabelId,
    ) -> Result<String, RouterError> {
        ROUTER_VERTEX_LABEL_CATALOG
            .with_borrow(|catalog| catalog.get_name(graph_id, label_id))
            .ok_or_else(|| RouterError::NotFound(format!("vertex label id {}", label_id.raw())))
    }

    pub fn reverse_edge_label_name(
        &self,
        graph_id: GraphId,
        label_id: EdgeLabelId,
    ) -> Result<String, RouterError> {
        ROUTER_EDGE_LABEL_CATALOG
            .with_borrow(|catalog| catalog.get_name(graph_id, label_id))
            .ok_or_else(|| RouterError::NotFound(format!("edge label id {}", label_id.raw())))
    }

    pub fn reverse_property_name(
        &self,
        graph_id: GraphId,
        property_id: PropertyId,
    ) -> Result<String, RouterError> {
        ROUTER_PROPERTY_CATALOG
            .with_borrow(|catalog| catalog.get_name(graph_id, property_id))
            .ok_or_else(|| RouterError::NotFound(format!("property id {}", property_id.raw())))
    }

    /// Declares a vertex single-property uniqueness constraint (ADR 0030, first cut).
    ///
    /// Declare-on-empty contract: the target label must be brand-new — never interned in this
    /// graph's vertex label catalog. Labels intern only on DML `CreateIfMissing` (read plans
    /// never intern), so an absent label structurally guarantees an empty domain. An already
    /// interned label is always rejected, even at live count 0, including admin/graph-type
    /// interned labels and labels left behind by a prior `DROP CONSTRAINT` (re-enablement on a
    /// populated label is deferred to the future validate path).
    ///
    /// Label, property, and constraint registration happen in one no-`await` region; every
    /// validation runs first in the read-only preflight, so a rejected `CREATE` leaves no
    /// partial label/property/constraint state.
    pub(crate) fn create_unique_constraint(
        &self,
        graph_id: GraphId,
        constraint_name: &str,
        if_not_exists: bool,
        label: &str,
        property: &str,
    ) -> Result<(), RouterError> {
        // --- preflight (read-only): all validation completes before any mutation ---
        validate_metadata_name(constraint_name)?;
        validate_metadata_name(label)?;
        validate_metadata_name(property)?;

        if let Some(id) = lookup_constraint_name_id(graph_id, constraint_name)
            && constraint_store::constraint_record_exists(graph_id, id)
        {
            if if_not_exists {
                return Ok(());
            }
            return Err(RouterError::Conflict(format!(
                "constraint already exists: {constraint_name}"
            )));
        }

        if self.lookup_vertex_label_id(graph_id, label).is_ok() {
            return Err(RouterError::Conflict(format!(
                "uniqueness constraint requires a brand-new vertex label; '{label}' already exists (ADR 0030 declare-on-empty)"
            )));
        }

        // --- commit (single no-await region, all-or-nothing) ---
        let property_id = Self::commit_intern_property_name(graph_id, property)?;
        let vertex_label_id = Self::commit_intern_vertex_label_name(graph_id, label)?;
        let constraint_name_id = intern_constraint_name(graph_id, constraint_name)?;
        constraint_store::create_unique_constraint(
            graph_id,
            constraint_name_id,
            ConstraintDefRecord {
                vertex_label_id,
                property_id,
            },
            false,
        )?;
        Ok(())
    }

    pub(crate) fn drop_unique_constraint(
        &self,
        graph_id: GraphId,
        constraint_name: &str,
        if_exists: bool,
    ) -> Result<(), RouterError> {
        validate_metadata_name(constraint_name)?;
        let Some(id) = lookup_constraint_name_id(graph_id, constraint_name) else {
            if if_exists {
                return Ok(());
            }
            return Err(RouterError::NotFound(constraint_name.to_owned()));
        };
        constraint_store::drop_unique_constraint(graph_id, id, if_exists)?;
        Ok(())
    }

    pub fn resolve_plan_labels(
        &self,
        graph_id: GraphId,
        plans: &[PhysicalPlan],
    ) -> Result<ResolvedLabelTable, RouterError> {
        let mut out = ResolvedLabelTable::default();
        for plan in plans {
            let uses = plan.label_uses();
            for (name, intent) in uses.node_labels {
                validate_metadata_name(&name)?;
                let id = match intent {
                    LabelUseIntent::ReadExisting => self.lookup_vertex_label_id(graph_id, &name)?,
                    LabelUseIntent::CreateIfMissing => {
                        Self::commit_intern_vertex_label_name(graph_id, &name)?
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
                    LabelUseIntent::ReadExisting => self.lookup_edge_label_id(graph_id, &name)?,
                    LabelUseIntent::CreateIfMissing => {
                        Self::commit_intern_edge_label_name(graph_id, &name)?
                    }
                };
                if !out.edge.iter().any(|entry| entry.name == name.as_ref()) {
                    out.edge.push(ResolvedEdgeLabel::new(
                        name.to_string(),
                        id,
                        self.lookup_edge_payload_profile(graph_id, id),
                    ));
                }
            }
        }
        self.enrich_edge_labels_for_predicate_fusion(graph_id, &mut out)?;
        Ok(out)
    }

    fn enrich_edge_labels_for_predicate_fusion(
        &self,
        graph_id: GraphId,
        table: &mut ResolvedLabelTable,
    ) -> Result<(), RouterError> {
        let fusion_ids = ROUTER_EDGE_PAYLOAD_PROFILES
            .with_borrow(|store| store.label_ids_with_nonzero_payload(graph_id));
        for id in fusion_ids {
            if table.edge.iter().any(|entry| entry.id == id) {
                continue;
            }
            let name = self.reverse_edge_label_name(graph_id, id)?;
            table.edge.push(ResolvedEdgeLabel::new(
                name,
                id,
                self.lookup_edge_payload_profile(graph_id, id),
            ));
        }
        Ok(())
    }

    pub fn resolve_plan_properties(
        &self,
        graph_id: GraphId,
        plans: &[PhysicalPlan],
    ) -> Result<ResolvedPropertyTable, RouterError> {
        let mut out = ResolvedPropertyTable::default();
        for plan in plans {
            let uses = plan.property_uses();
            for (name, intent) in uses.properties {
                validate_metadata_name(&name)?;
                let id = match intent {
                    PropertyUseIntent::ReadExisting => self.lookup_property_id(graph_id, &name)?,
                    PropertyUseIntent::CreateIfMissing => {
                        Self::commit_intern_property_name(graph_id, &name)?
                    }
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
