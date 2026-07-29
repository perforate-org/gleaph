//! Federated label and property resolution catalogs (ADR 0018 graph-scoped vocabulary).

use super::super::stable::{
    ROUTER_CONSTRAINT_NAME_CATALOG, ROUTER_EDGE_INLINE_PROPERTY_PROFILES,
    ROUTER_EDGE_LABEL_CATALOG, ROUTER_PROPERTY_CATALOG, ROUTER_VERTEX_LABEL_CATALOG,
};
use crate::facade::auth;
use crate::facade::stable::constraint_catalog::{
    self as constraint_store, ConstraintDefRecord, ConstraintLifecycle, UniqueEnforcementStrategy,
};
use crate::facade::stable::constraint_name_catalog::{
    intern_constraint_name, lookup_constraint_name_id,
};
use crate::facade::stable::edge_inline_property_profiles::{
    EdgeInlinePropertyProfileStoreError, InlineScalarType, InlineStructFieldSpec,
    InlineStructLayout,
};
use crate::facade::stable::graph_catalog::{
    catalog_error_to_router, list_live_shards_for_graph_id, resolve_registered_graph_id,
};
use crate::facade::stable::indexed_catalog::edge_index_uses_property_label;
use crate::facade::stable::reservation_catalog::ProofShard;
use crate::state::RouterError;
use crate::types::{EdgeLabelId, PropertyId, VertexLabelId};
use candid::Principal;
use gleaph_gql::ast::{EdgeTypeDef, GraphTypeDefinition, GraphTypeElement, ValueType};
use gleaph_gql::type_check::collect_graph_type_vocabulary;
use gleaph_gql_planner::{LabelUseIntent, PhysicalPlan, PropertyUseIntent};
use gleaph_graph_kernel::entry::{EdgeInlinePropertyProfile, GraphId};
use gleaph_graph_kernel::plan_exec::{
    ResolvedEdgeLabel, ResolvedInlineSchema, ResolvedLabelTable, ResolvedProperty,
    ResolvedPropertyTable, ResolvedVertexLabel,
};

use super::{RouterStore, validate_metadata_name};

fn map_edge_inline_property_profile_err(err: EdgeInlinePropertyProfileStoreError) -> RouterError {
    RouterError::InvalidArgument(err.to_string())
}

enum InlineSchema {
    Scalar { scalar_type: InlineScalarType },
    Struct { fields: Vec<InlineStructFieldSpec> },
}

const MAX_INLINE_RECORD_DEPTH: usize = 4;

fn flatten_inline_record_fields(
    prefix: &str,
    fields: &[gleaph_gql::ast::RecordFieldType],
    depth: usize,
    output: &mut Vec<InlineStructFieldSpec>,
) -> Result<(), RouterError> {
    if depth > MAX_INLINE_RECORD_DEPTH {
        return Err(RouterError::InvalidArgument(format!(
            "INLINE RECORD nesting exceeds maximum depth {MAX_INLINE_RECORD_DEPTH}"
        )));
    }
    for field in fields {
        let name = if prefix.is_empty() {
            field.name.clone()
        } else {
            format!("{prefix}.{}", field.name)
        };
        match &field.value_type {
            ValueType::Record { fields, .. } => {
                flatten_inline_record_fields(&name, fields, depth + 1, output)?;
            }
            value_type => output.push(InlineStructFieldSpec {
                name,
                scalar_type: inline_scalar_type(value_type)?,
            }),
        }
    }
    Ok(())
}

fn edge_schema_keys(edge: &EdgeTypeDef) -> Vec<String> {
    let mut keys = edge
        .label_set
        .as_ref()
        .map(|labels| labels.labels.clone())
        .unwrap_or_default();
    if keys.is_empty()
        && let Some(name) = &edge.name
    {
        keys.push(name.clone());
    }
    keys
}

fn inline_scalar_type(value_type: &ValueType) -> Result<InlineScalarType, RouterError> {
    let value_type = match value_type {
        ValueType::NotNull(inner) => inner.as_ref(),
        value_type => value_type,
    };
    let scalar = match value_type {
        ValueType::Uint8 { .. } => InlineScalarType::U8,
        ValueType::Uint16 { .. } => InlineScalarType::U16,
        ValueType::Uint32 { .. } => InlineScalarType::U32,
        ValueType::Uint64 { .. } => InlineScalarType::U64,
        ValueType::Uint128 { .. } => InlineScalarType::U128,
        ValueType::Int8 { .. } => InlineScalarType::I8,
        ValueType::Int16 { .. } => InlineScalarType::I16,
        ValueType::Int32 { .. } => InlineScalarType::I32,
        ValueType::Int64 { .. } => InlineScalarType::I64,
        ValueType::Int128 { .. } => InlineScalarType::I128,
        ValueType::Float16 { .. } => InlineScalarType::F16,
        ValueType::Float32 { .. } => InlineScalarType::F32,
        ValueType::Float64 { .. } => InlineScalarType::F64,
        ValueType::ExtensionType { name } if name.parts.len() == 1 => {
            InlineScalarType::from_ddl_name(&name.parts[0]).ok_or_else(|| {
                RouterError::InvalidArgument(format!(
                    "unsupported INLINE scalar type `{}`",
                    name.parts[0]
                ))
            })?
        }
        _ => {
            return Err(RouterError::InvalidArgument(
                "INLINE requires a fixed-width scalar type or a fixed-size RECORD".into(),
            ));
        }
    };
    Ok(scalar)
}

impl RouterStore {
    /// Resolve the names carried by the ordered public edge wire without interning anything.
    /// Ordered admission must be read-only until the mutation reservation owns the request.
    #[allow(
        dead_code,
        reason = "wired into the ordered public admission endpoint in the next slice"
    )]
    pub(crate) fn resolve_ordered_edge_catalogs(
        &self,
        graph_id: GraphId,
        edge_label_names: impl IntoIterator<Item = Option<String>>,
        property_names: impl IntoIterator<Item = String>,
    ) -> Result<(ResolvedLabelTable, ResolvedPropertyTable), RouterError> {
        let mut labels = ResolvedLabelTable::default();
        for name in edge_label_names.into_iter().flatten() {
            validate_metadata_name(&name)?;
            if labels.edge.iter().any(|entry| entry.name == name) {
                continue;
            }
            let id = self.lookup_edge_label_id(graph_id, &name)?;
            let (profile, inline_schema) =
                self.lookup_edge_inline_property_profile_and_inline_schema(graph_id, id);
            labels.edge.push(ResolvedEdgeLabel::with_inline_schema(
                name,
                id,
                profile,
                inline_schema,
            ));
        }

        let mut properties = ResolvedPropertyTable::default();
        for name in property_names {
            validate_metadata_name(&name)?;
            if properties.properties.iter().any(|entry| entry.name == name) {
                continue;
            }
            let id = self.lookup_property_id(graph_id, &name)?;
            properties.properties.push(ResolvedProperty { name, id });
        }
        Ok((labels, properties))
    }

    /// Resolve the names carried by the ordered public vertex wire without interning anything.
    #[allow(
        dead_code,
        reason = "wired into the ordered public vertex admission endpoint in the next slice"
    )]
    pub(crate) fn resolve_ordered_vertex_catalogs(
        &self,
        graph_id: GraphId,
        vertex_label_names: impl IntoIterator<Item = String>,
        property_names: impl IntoIterator<Item = String>,
    ) -> Result<(ResolvedLabelTable, ResolvedPropertyTable), RouterError> {
        let mut labels = ResolvedLabelTable::default();
        for name in vertex_label_names {
            validate_metadata_name(&name)?;
            if labels.vertex.iter().any(|entry| entry.name == name) {
                continue;
            }
            labels.vertex.push(ResolvedVertexLabel {
                id: self.lookup_vertex_label_id(graph_id, &name)?,
                name,
            });
        }
        let mut properties = ResolvedPropertyTable::default();
        for name in property_names {
            validate_metadata_name(&name)?;
            if properties.properties.iter().any(|entry| entry.name == name) {
                continue;
            }
            properties.properties.push(ResolvedProperty {
                id: self.lookup_property_id(graph_id, &name)?,
                name,
            });
        }
        Ok((labels, properties))
    }

    /// Resolve both vocabularies for a mixed ordered batch without mutating the catalogs.
    pub(crate) fn resolve_ordered_mixed_catalogs(
        &self,
        graph_id: GraphId,
        vertex_label_names: impl IntoIterator<Item = String>,
        edge_label_names: impl IntoIterator<Item = String>,
        property_names: impl IntoIterator<Item = String>,
    ) -> Result<(ResolvedLabelTable, ResolvedPropertyTable), RouterError> {
        let (vertex_labels, _) = self.resolve_ordered_vertex_catalogs(
            graph_id,
            vertex_label_names,
            std::iter::empty::<String>(),
        )?;
        let (edge_labels, _) = self.resolve_ordered_edge_catalogs(
            graph_id,
            edge_label_names.into_iter().map(Some),
            std::iter::empty::<String>(),
        )?;
        let mut labels = vertex_labels;
        labels.edge = edge_labels.edge;
        let mut properties = ResolvedPropertyTable::default();
        for name in property_names {
            validate_metadata_name(&name)?;
            if properties.properties.iter().any(|entry| entry.name == name) {
                continue;
            }
            properties.properties.push(ResolvedProperty {
                id: self.lookup_property_id(graph_id, &name)?,
                name,
            });
        }
        Ok((labels, properties))
    }

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
        Self::commit_graph_type_inline_schemas(graph_id, def)?;
        Ok(())
    }

    fn commit_graph_type_inline_schemas(
        graph_id: GraphId,
        def: &GraphTypeDefinition,
    ) -> Result<(), RouterError> {
        let store = RouterStore::new();
        for element in &def.elements {
            let GraphTypeElement::Edge(edge) = element else {
                continue;
            };
            let inline = edge.properties.iter().filter(|property| property.inline);
            let mut inline = inline.peekable();
            let Some(property) = inline.next() else {
                continue;
            };
            if inline.next().is_some() {
                return Err(RouterError::InvalidArgument(format!(
                    "edge type `{}` declares more than one INLINE property",
                    edge.name.as_deref().unwrap_or("<unnamed>")
                )));
            }

            let labels = edge_schema_keys(edge);
            if labels.is_empty() {
                return Err(RouterError::InvalidArgument(
                    "an INLINE edge property requires an edge name or label".into(),
                ));
            }
            let schema = match &property.value_type {
                ValueType::Record { fields, .. } => {
                    let mut flattened = Vec::new();
                    flatten_inline_record_fields("", fields, 0, &mut flattened)?;
                    InlineSchema::Struct { fields: flattened }
                }
                value_type => InlineSchema::Scalar {
                    scalar_type: inline_scalar_type(value_type)?,
                },
            };

            for label in labels {
                match &schema {
                    InlineSchema::Scalar { scalar_type } => {
                        store.commit_set_edge_label_inline_scalar_schema(
                            graph_id,
                            &label,
                            &property.name,
                            *scalar_type,
                        )?;
                    }
                    InlineSchema::Struct { fields } => {
                        store.commit_set_edge_label_inline_struct_schema(
                            graph_id,
                            &label,
                            &property.name,
                            fields.clone(),
                        )?;
                    }
                }
            }
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

    pub(crate) fn commit_intern_edge_label_name(
        graph_id: GraphId,
        name: &str,
    ) -> Result<EdgeLabelId, RouterError> {
        let id = ROUTER_EDGE_LABEL_CATALOG
            .with_borrow_mut(|catalog| catalog.get_or_insert(graph_id, name))
            .map_err(|e| catalog_error_to_router(e, "edge label"))?;
        Self::commit_ensure_edge_label_inline_property_profile_default(graph_id, id)?;
        Ok(id)
    }

    pub(super) fn commit_ensure_edge_label_inline_property_profile_default(
        graph_id: GraphId,
        id: EdgeLabelId,
    ) -> Result<(), RouterError> {
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow_mut(|store| store.insert_if_absent_no_inline_property(graph_id, id))
            .map_err(map_edge_inline_property_profile_err)
    }

    pub(super) fn commit_set_edge_label_inline_property_profile(
        graph_id: GraphId,
        id: EdgeLabelId,
        profile: EdgeInlinePropertyProfile,
    ) -> Result<(), RouterError> {
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow_mut(|store| store.insert_unnamed_profile_profile(graph_id, id, profile))
            .map_err(map_edge_inline_property_profile_err)
    }

    pub(crate) fn commit_set_edge_label_inline_scalar_schema(
        &self,
        graph_id: GraphId,
        edge_label_name: &str,
        property_name: &str,
        scalar_type: InlineScalarType,
    ) -> Result<(), RouterError> {
        validate_metadata_name(edge_label_name)?;
        validate_metadata_name(property_name)?;

        // --- preflight: every validation and capacity check is read-only ---
        let existing_label_id = self.lookup_edge_label_id(graph_id, edge_label_name).ok();
        let existing_property_id = self.lookup_property_id(graph_id, property_name).ok();

        if let Some(label_id) = existing_label_id
            && let Some(existing) = ROUTER_EDGE_INLINE_PROPERTY_PROFILES
                .with_borrow(|store| store.get_record(graph_id, label_id))
        {
            if let Some(property_id) = existing_property_id
                    && let crate::facade::stable::edge_inline_property_profiles::EdgeInlinePropertySchemaRecord::InlineScalar {
                        property_id: existing_pid,
                        scalar_type: existing_st,
                        ..
                    } = existing
                        && existing_pid == property_id && existing_st == scalar_type {
                            return Ok(());
                        }
            if existing.is_inline_scalar() {
                return Err(RouterError::Conflict(format!(
                    "edge label {edge_label_name} already has an inline scalar schema"
                )));
            }
            if existing.is_named_inline() {
                return Err(RouterError::Conflict(format!(
                    "edge label {edge_label_name} already has an inline struct schema"
                )));
            }
            if existing.profile() != EdgeInlinePropertyProfile::no_inline_property() {
                return Err(RouterError::Conflict(format!(
                    "edge label {edge_label_name} has a legacy unnamed inline property profile; inline schema is incompatible"
                )));
            }
        }

        // Inline/index conflict guard (ADR 0034 Slice 21/24): a sidecar edge Property Index
        // for the same (label, property) would be semantically stale because inline property bytes
        // mutations do not maintain postings. Reject the DDL before any catalog write.
        if let Some(label_id) = existing_label_id
            && let Some(property_id) = existing_property_id
            && edge_index_uses_property_label(graph_id, property_id, label_id.raw())
        {
            return Err(RouterError::Conflict(format!(
                "edge label {edge_label_name} already has a property index on {property_name}; drop the index before creating an inline schema"
            )));
        }

        // Capacity preflight: prove id allocation will succeed before any mutation.
        if existing_label_id.is_none() {
            ROUTER_EDGE_LABEL_CATALOG
                .with_borrow(|catalog| catalog.peek_next_id(graph_id, edge_label_name))
                .map_err(|e| catalog_error_to_router(e, "edge label"))?;
        }
        if existing_property_id.is_none() {
            ROUTER_PROPERTY_CATALOG
                .with_borrow(|catalog| catalog.peek_next_id(graph_id, property_name))
                .map_err(|e| catalog_error_to_router(e, "property"))?;
        }

        // --- commit: idempotent intern + schema record write ---
        let label_id = Self::commit_intern_edge_label_name(graph_id, edge_label_name)?;
        let property_id = Self::commit_intern_property_name(graph_id, property_name)?;
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow_mut(|store| {
                store.set_inline_scalar_schema(graph_id, label_id, property_id, scalar_type)
            })
            .map_err(map_edge_inline_property_profile_err)
    }

    pub(crate) fn commit_set_edge_label_inline_struct_schema(
        &self,
        graph_id: GraphId,
        edge_label_name: &str,
        property_name: &str,
        fields: Vec<InlineStructFieldSpec>,
    ) -> Result<(), RouterError> {
        validate_metadata_name(edge_label_name)?;
        validate_metadata_name(property_name)?;

        for InlineStructFieldSpec { name, .. } in &fields {
            validate_metadata_name(name)?;
        }

        // Validate the canonical layout before any catalog allocation.
        let declared: Vec<(String, InlineScalarType)> = fields
            .into_iter()
            .map(|InlineStructFieldSpec { name, scalar_type }| (name, scalar_type))
            .collect();
        let layout = InlineStructLayout::from_fields_with_record_bound(
            declared,
            crate::facade::stable::edge_inline_property_profiles::MAX_INLINE_STRUCT_RECORD_BYTES,
        )
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

        // --- preflight: every validation and capacity check is read-only ---
        let existing_label_id = self.lookup_edge_label_id(graph_id, edge_label_name).ok();
        let existing_property_id = self.lookup_property_id(graph_id, property_name).ok();

        if let Some(label_id) = existing_label_id
            && let Some(existing) = ROUTER_EDGE_INLINE_PROPERTY_PROFILES
                .with_borrow(|store| store.get_record(graph_id, label_id))
        {
            match existing {
                crate::facade::stable::edge_inline_property_profiles::EdgeInlinePropertySchemaRecord::InlineStruct {
                    property_id: existing_pid,
                    field_specs: ref existing_specs,
                } if let Some(property_id) = existing_property_id
                    && existing_pid == property_id
                    && existing_specs == layout.field_specs() => {
                    return Ok(());
                }
                crate::facade::stable::edge_inline_property_profiles::EdgeInlinePropertySchemaRecord::InlineStruct { .. } => {
                    return Err(RouterError::Conflict(format!(
                        "edge label {edge_label_name} already has an inline struct schema"
                    )));
                }
                crate::facade::stable::edge_inline_property_profiles::EdgeInlinePropertySchemaRecord::InlineScalar { .. } => {
                    return Err(RouterError::Conflict(format!(
                        "edge label {edge_label_name} already has an inline scalar schema"
                    )));
                }
                crate::facade::stable::edge_inline_property_profiles::EdgeInlinePropertySchemaRecord::UnnamedProfile { profile } => {
                    if profile != EdgeInlinePropertyProfile::no_inline_property() {
                        return Err(RouterError::Conflict(format!(
                            "edge label {edge_label_name} has a legacy unnamed inline property profile; inline schema is incompatible"
                        )));
                    }
                }
            }
        }

        // Inline/index conflict guard (ADR 0034 Slice 21): a sidecar edge Property Index
        // for the same (label, property) would be semantically stale because inline property bytes
        // mutations do not maintain postings. Reject the DDL before any catalog write.
        if let Some(label_id) = existing_label_id
            && let Some(property_id) = existing_property_id
            && edge_index_uses_property_label(graph_id, property_id, label_id.raw())
        {
            return Err(RouterError::Conflict(format!(
                "edge label {edge_label_name} already has a property index on {property_name}; drop the index before creating an inline struct schema"
            )));
        }

        // Capacity preflight: prove id allocation will succeed before any mutation.
        if existing_label_id.is_none() {
            ROUTER_EDGE_LABEL_CATALOG
                .with_borrow(|catalog| catalog.peek_next_id(graph_id, edge_label_name))
                .map_err(|e| catalog_error_to_router(e, "edge label"))?;
        }
        if existing_property_id.is_none() {
            ROUTER_PROPERTY_CATALOG
                .with_borrow(|catalog| catalog.peek_next_id(graph_id, property_name))
                .map_err(|e| catalog_error_to_router(e, "property"))?;
        }

        // --- commit: idempotent intern + schema record write ---
        let label_id = Self::commit_intern_edge_label_name(graph_id, edge_label_name)?;
        let property_id = Self::commit_intern_property_name(graph_id, property_name)?;
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow_mut(|store| {
                store.set_inline_struct_schema(graph_id, label_id, property_id, layout)
            })
            .map_err(map_edge_inline_property_profile_err)
    }

    fn lookup_edge_inline_property_profile_and_inline_schema(
        &self,
        graph_id: GraphId,
        id: EdgeLabelId,
    ) -> (EdgeInlinePropertyProfile, Option<ResolvedInlineSchema>) {
        ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow(|store| store.get_profile_and_inline_schema(graph_id, id))
    }

    pub(crate) fn commit_intern_property_name(
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

    pub fn admin_set_edge_label_inline_property_profile(
        &self,
        caller: Principal,
        logical_graph_name: &str,
        name: &str,
        profile: EdgeInlinePropertyProfile,
    ) -> Result<(), RouterError> {
        auth::require_admin(&caller)?;
        validate_metadata_name(logical_graph_name)?;
        validate_metadata_name(name)?;
        let graph_id = resolve_registered_graph_id(logical_graph_name)?;
        let id = self.lookup_edge_label_id(graph_id, name)?;
        Self::commit_set_edge_label_inline_property_profile(graph_id, id, profile)
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
    /// partial label/property/constraint state. Id-exhaustion is part of that preflight: each
    /// catalog the commit will allocate from is capacity-checked via `peek_next_id` before the
    /// first mutation, so the commit region cannot fail half-way and strand a label/property/name.
    // ADR 0030 slice 8: published — reached from `CREATE CONSTRAINT` DDL (`gql::run_gql`) and from the
    // `pocket-ic-e2e` declaration seam. Enforces the declare-on-empty (brand-new-label) contract.
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
            && let Some(record) =
                constraint_store::find_unique_constraint_any_lifecycle(graph_id, id)
        {
            match record.state {
                // ADR 0030 slice 9: the same `ConstraintNameId` is reused after `Removed`, so a
                // re-CREATE must wait until the drop-drain completion gate has purged every
                // reservation and pending effect for the id. Until then, reject transiently — even
                // under IF NOT EXISTS, since the name is being removed, not "already present".
                ConstraintLifecycle::Dropping => {
                    return Err(RouterError::Conflict(format!(
                        "constraint {constraint_name} is being dropped; retry after cleanup completes"
                    )));
                }
                ConstraintLifecycle::Active => {
                    if if_not_exists {
                        return Ok(());
                    }
                    return Err(RouterError::Conflict(format!(
                        "constraint already exists: {constraint_name}"
                    )));
                }
            }
        }

        if self.lookup_vertex_label_id(graph_id, label).is_ok() {
            return Err(RouterError::Conflict(format!(
                "uniqueness constraint requires a brand-new vertex label; '{label}' already exists (ADR 0030 declare-on-empty)"
            )));
        }

        // Capacity preflight: prove every id allocation the commit performs will succeed, so the
        // no-await region cannot fail after interning the property or label. `peek_next_id` is
        // read-only and returns the same error `get_or_insert` would on exhaustion.
        ROUTER_PROPERTY_CATALOG
            .with_borrow(|catalog| catalog.peek_next_id(graph_id, property))
            .map_err(|e| catalog_error_to_router(e, "property"))?;
        ROUTER_VERTEX_LABEL_CATALOG
            .with_borrow(|catalog| catalog.peek_next_id(graph_id, label))
            .map_err(|e| catalog_error_to_router(e, "vertex label"))?;
        ROUTER_CONSTRAINT_NAME_CATALOG
            .with_borrow(|catalog| catalog.peek_next_id(graph_id, constraint_name))
            .map_err(|e| catalog_error_to_router(e, "constraint"))?;

        // ADR 0030 slice 10: freeze the enforcement strategy at CREATE from the same live-shard
        // definition dispatch uses. A graph with exactly one live (index-attached) shard enforces
        // graph-wide uniqueness entirely inside that one shard's local table (the `ShardLocalGlobal`
        // fast path); any other topology uses the federated TCC path. The owning shard's full
        // identity (`shard_id` + `graph_canister`) is pinned via `ProofShard` so shard-id reuse
        // cannot later mis-route enforcement or DROP purge. Read-only, so it stays outside the
        // no-await commit region below.
        let live_shards = list_live_shards_for_graph_id(graph_id)?;
        let (strategy, owning_shard) = match live_shards.as_slice() {
            [only] => (
                UniqueEnforcementStrategy::ShardLocalGlobal,
                Some(ProofShard::new(only.shard_id, only.graph_canister)),
            ),
            _ => (UniqueEnforcementStrategy::FederatedTcc, None),
        };

        // --- commit (single no-await region, all-or-nothing) ---
        let property_id = Self::commit_intern_property_name(graph_id, property)?;
        let vertex_label_id = Self::commit_intern_vertex_label_name(graph_id, label)?;
        let constraint_name_id = intern_constraint_name(graph_id, constraint_name)?;
        constraint_store::create_unique_constraint(
            graph_id,
            constraint_name_id,
            ConstraintDefRecord::new_active(vertex_label_id, property_id, strategy, owning_shard),
            false,
        )?;
        Ok(())
    }

    /// Initiates `DROP CONSTRAINT` (ADR 0030 slice 9): synchronously flips the constraint
    /// `Active → Dropping` and returns immediately. The actual cleanup — draining every reservation
    /// and pending unique effect keyed by the dropped `ConstraintNameId`, then deleting the record
    /// (`Removed`) — runs asynchronously in the drop-drain recovery lane ([`crate::constraint_drop`]).
    ///
    /// While `Dropping`, the constraint is inactive for new acquires (new INSERTs proceed
    /// unconstrained) but still captures `Release` effects, and a same-name re-CREATE is rejected
    /// (`Conflict`) until the completion gate proves the id is safe to reuse. Idempotent on a
    /// constraint already `Dropping`; with `if_exists`, an absent constraint is a no-op.
    pub(crate) fn begin_drop_unique_constraint(
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
        constraint_store::begin_drop(graph_id, id, if_exists, super::ic_time_ns())?;
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
                    let (profile, inline_schema) =
                        self.lookup_edge_inline_property_profile_and_inline_schema(graph_id, id);
                    out.edge.push(ResolvedEdgeLabel::with_inline_schema(
                        name.to_string(),
                        id,
                        profile,
                        inline_schema,
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
        let fusion_ids = ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow(|store| store.label_ids_with_nonzero_inline_property_bytes(graph_id));
        for id in fusion_ids {
            if table.edge.iter().any(|entry| entry.id == id) {
                continue;
            }
            let name = self.reverse_edge_label_name(graph_id, id)?;
            let (profile, inline_schema) =
                self.lookup_edge_inline_property_profile_and_inline_schema(graph_id, id);
            table.edge.push(ResolvedEdgeLabel::with_inline_schema(
                name,
                id,
                profile,
                inline_schema,
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
