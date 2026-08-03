//! GraphStore `edge_insert` implementation.

use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{
    Edge, EdgeInlinePropertyBytes, EdgeLabelId, EdgeSlotIndex, PropertyId, VertexRef,
};
use gleaph_graph_kernel::index::IndexBuildSubject;
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_lara::{
    VertexId,
    labeled::{BucketLabelKey as LaraLabelId, EdgePlacementPolicy},
    traits::CsrEdge,
};

use super::GraphStore;
use super::adjacency::{EdgeInsertSpec, journal_edge_insert};
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{
    build_edge_to, canonical_undirected_owner, edge_matches_local_neighbor, edge_storage_label,
    lara_edge_placement, lara_label, validate_edge_inline_property_bytes_for_label,
};
use crate::edge_inline_property_schema::resolved_edge_label_with;
use crate::facade::store::index_build_admission::{
    FencedTransition, PlannedBuildEnvelope, trap_post_fence_commit,
};
use crate::property::inline_index_values;

/// Validated inputs for the LARA edge insert produced by the shared fenced preflight.
struct PreflightEdgeInsert {
    properties: Vec<(PropertyId, Value)>,
    label: LaraLabelId,
    inline_property_width: u16,
    placement: EdgePlacementPolicy,
    planned: Vec<PlannedBuildEnvelope>,
    needs_commit: bool,
    shard_id: u32,
}

impl GraphStore {
    fn edge_inline_property_width_u16(
        inline_property_bytes: &[u8],
    ) -> Result<u16, GraphStoreError> {
        u16::try_from(inline_property_bytes.len()).map_err(|_| {
            GraphStoreError::InvalidEdgeInlinePropertyBytesWidth(inline_property_bytes.len())
        })
    }

    /// Shared pure prefix for a fenced edge insert: validates the endpoints, label, inline bytes,
    /// and initial sidecars, then runs the fence plan. Directed and undirected inserts differ only
    /// after this point (LARA placement and canonical resolution), so they share one preflight.
    fn preflight_edge_insert(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        undirected: bool,
        inline_property_bytes: &[u8],
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
    ) -> Result<PreflightEdgeInsert, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        self.ensure_vertex_id(target_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_inline_property_bytes_for_label(catalog_label, inline_property_bytes)?;
        let properties: Vec<(PropertyId, Value)> = properties.into_iter().collect();
        for (property_id, value) in &properties {
            crate::property::ensure_property_id(*property_id).map_err(|id| {
                GraphStoreError::PropertyValue(
                    super::super::stable::vertex_properties::VertexPropertyStoreError::ReservedPropertyId(
                        id,
                    ),
                )
            })?;
            crate::property::ensure_persistable(value).map_err(|error| {
                GraphStoreError::PropertyValue(
                    super::super::stable::vertex_properties::VertexPropertyStoreError::InvalidValue(
                        error,
                    ),
                )
            })?;
        }
        let label = lara_label(edge_storage_label(catalog_label, undirected));
        let inline_property_width = Self::edge_inline_property_width_u16(inline_property_bytes)?;
        let placement = lara_edge_placement(
            catalog_label
                .and_then(|label| resolved_edge_label_with(None, label).map(|l| l.ordering)),
        );
        // Fence plan BEFORE the LARA insert: decode the INLINE transitions and combine them with
        // the sidecar transitions (all prev=None for a fresh edge). Any Sealing membership
        // rejects before any canonical write; Building scopes are validated without reserving.
        let (planned, needs_commit, shard_id) =
            self.plan_edge_insert_admission(label.raw(), inline_property_bytes, &properties)?;
        Ok(PreflightEdgeInsert {
            properties,
            label,
            inline_property_width,
            placement,
            planned,
            needs_commit,
            shard_id,
        })
    }

    pub(crate) fn validate_catalog_edge_label(
        label: Option<EdgeLabelId>,
    ) -> Result<(), GraphStoreError> {
        if let Some(id) = label
            && id.raw() != 0
            && !id.is_catalog_allocatable()
        {
            return Err(GraphStoreError::InvalidEdgeLabelId(id));
        }
        Ok(())
    }

    pub fn insert_directed_edge(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_directed_edge_with_inline_property_bytes(
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            &[],
        )
    }

    pub(crate) fn insert_directed_edge_with_inline_property_bytes(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_property_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_directed_edge_with_inline_property_bytes_and_properties(
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            inline_property_bytes,
            std::iter::empty(),
            0,
        )
    }

    /// Fenced edge insert: admits INLINE and initial sidecar transitions before the LARA write.
    pub(crate) fn insert_directed_edge_with_inline_property_bytes_and_properties(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_property_bytes: &[u8],
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
        mutation_id: MutationId,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let PreflightEdgeInsert {
            properties,
            label,
            inline_property_width,
            placement,
            planned,
            needs_commit,
            shard_id,
        } = self.preflight_edge_insert(
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            false,
            inline_property_bytes,
            properties,
        )?;

        let forward = build_edge_to(target_vertex_id)
            .with_stored_inline_property_bytes(inline_property_width, inline_property_bytes);
        let reverse = Edge {
            target: VertexRef::local(source_vertex_id),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            inline_property: EdgeInlinePropertyBytes::EMPTY,
        }
        .with_stored_inline_property_bytes(inline_property_width, inline_property_bytes);
        let locations = self.with_graph_mut(|graph| {
            if inline_property_width != 0 {
                graph.ensure_directed_edge_inline_property_width(
                    source_vertex_id,
                    target_vertex_id,
                    label,
                    inline_property_width,
                )?;
            }
            graph.insert_directed_edge_with_locations(
                source_vertex_id,
                target_vertex_id,
                label,
                forward,
                reverse,
                placement,
                &Self::maintenance_policy_for_label,
            )
        })?;
        let canonical = if let Some(location) = locations.forward {
            EdgeHandle::at_slot(source_vertex_id, label, location.logical_slot)
        } else {
            self.find_first_forward_handle(source_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target_vertex_id, inline_property_bytes)
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?
        };
        // Commit (infallible): bind the exact canonical subject and reserve+append the envelopes
        // before any derived dispatch or sidecar write.
        if needs_commit {
            let subject = IndexBuildSubject::Edge {
                shard_id,
                owner_vertex_id: u32::try_from(u64::from(canonical.owner_vertex_id))
                    .expect("canonical owner vertex id fits u32"),
                label_id: canonical.label_id.raw(),
                slot_index: canonical.slot_index.raw(),
            };
            self.commit_index_build_admission(mutation_id, subject, planned);
        }
        self.commit_directed_edge_insert(EdgeInsertSpec {
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            undirected: false,
            inline_property_bytes,
            canonical,
        })
        .unwrap_or_else(trap_post_fence_commit);
        self.commit_edge_property_writes_at_canonical(canonical, &properties);
        Ok(canonical)
    }

    pub fn insert_undirected_edge(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_undirected_edge_with_inline_property_bytes(
            endpoint_a,
            endpoint_b,
            catalog_label,
            &[],
        )
    }

    pub(crate) fn insert_undirected_edge_with_inline_property_bytes(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_property_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_undirected_edge_with_inline_property_bytes_and_properties(
            endpoint_a,
            endpoint_b,
            catalog_label,
            inline_property_bytes,
            std::iter::empty(),
            0,
        )
    }

    /// Fenced edge insert: admits INLINE and initial sidecar transitions before the LARA write.
    pub(crate) fn insert_undirected_edge_with_inline_property_bytes_and_properties(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_property_bytes: &[u8],
        properties: impl IntoIterator<Item = (PropertyId, Value)>,
        mutation_id: MutationId,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let PreflightEdgeInsert {
            properties,
            label,
            inline_property_width,
            placement,
            planned,
            needs_commit,
            shard_id,
        } = self.preflight_edge_insert(
            endpoint_a,
            endpoint_b,
            catalog_label,
            true,
            inline_property_bytes,
            properties,
        )?;
        let edge_ab = build_edge_to(endpoint_b)
            .with_stored_inline_property_bytes(inline_property_width, inline_property_bytes);
        let edge_ba = build_edge_to(endpoint_a)
            .with_stored_inline_property_bytes(inline_property_width, inline_property_bytes);
        let locations = self.with_graph_mut(|graph| {
            if inline_property_width != 0 {
                graph.ensure_undirected_edge_inline_property_width(
                    endpoint_a,
                    endpoint_b,
                    label,
                    inline_property_width,
                )?;
            }
            graph.insert_undirected_deferred_with_locations(
                endpoint_a,
                endpoint_b,
                label,
                edge_ab,
                edge_ba,
                placement,
                &Self::maintenance_policy_for_label,
            )
        })?;
        let owner_vertex_id = canonical_undirected_owner(endpoint_a, endpoint_b);
        let target = if owner_vertex_id == endpoint_a {
            endpoint_b
        } else {
            endpoint_a
        };
        let canonical_location = if owner_vertex_id == endpoint_a {
            locations.forward
        } else {
            locations.reverse
        };
        let canonical = if let Some(location) = canonical_location {
            EdgeHandle::at_slot(owner_vertex_id, label, location.logical_slot)
        } else {
            self.find_first_forward_handle(owner_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target, inline_property_bytes)
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?
        };
        if needs_commit {
            let subject = IndexBuildSubject::Edge {
                shard_id,
                owner_vertex_id: u32::try_from(u64::from(canonical.owner_vertex_id))
                    .expect("canonical owner vertex id fits u32"),
                label_id: canonical.label_id.raw(),
                slot_index: canonical.slot_index.raw(),
            };
            self.commit_index_build_admission(mutation_id, subject, planned);
        }
        self.commit_undirected_edge_insert(EdgeInsertSpec {
            source_vertex_id: owner_vertex_id,
            target_vertex_id: target,
            catalog_label,
            undirected: true,
            inline_property_bytes,
            canonical,
        })
        .unwrap_or_else(trap_post_fence_commit);
        self.commit_edge_property_writes_at_canonical(canonical, &properties);
        Ok(canonical)
    }

    /// Pure admission planning for one fresh edge insert (INLINE values plus initial sidecars).
    ///
    /// Runs before the LARA insert so a Sealing membership rejects with nothing written. The
    /// canonical slot is not known until placement, so the plan returns subject-less envelopes
    /// and the shard identity needed to bind the subject after the insert.
    fn plan_edge_insert_admission(
        &self,
        wire_label_id: u16,
        inline_property_bytes: &[u8],
        properties: &[(PropertyId, Value)],
    ) -> Result<(Vec<PlannedBuildEnvelope>, bool, u32), GraphStoreError> {
        // Fast path: with no Building/Sealing membership every transition is Active, so the fence
        // has no envelope to commit and nothing to reject. Skip the INLINE decode and membership
        // resolution entirely (the common case with no in-flight index build).
        if !crate::index::catalog_context::has_non_active_membership() {
            return Ok((Vec::new(), false, 0));
        }
        let mut transitions = Vec::new();
        let inline_values = inline_index_values(wire_label_id, inline_property_bytes)
            .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })?;
        for (membership, property_id, value) in &inline_values {
            transitions.push(FencedTransition {
                property_id: *property_id,
                prev: None,
                new: Some(value),
                membership: *membership,
            });
        }
        for (property_id, value) in properties {
            for membership in
                crate::index::catalog_context::edge_index_memberships(wire_label_id, *property_id)
            {
                transitions.push(FencedTransition {
                    property_id: *property_id,
                    prev: None,
                    new: Some(value),
                    membership,
                });
            }
        }
        let planned = self.plan_index_build_admission(transitions)?;
        let needs_commit = !planned.is_empty();
        let shard_id = if needs_commit {
            let Some(routing) = self.federation_routing() else {
                return Err(GraphStoreError::IndexBuildAdmission(
                    gleaph_graph_kernel::canonical_export::CanonicalExportError::InvalidRequest,
                ));
            };
            routing.shard_id.raw()
        } else {
            0
        };
        Ok((planned, needs_commit, shard_id))
    }

    pub(crate) fn insert_directed_edge_with_inline_property_bytes_journal(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_property_bytes: &[u8],
        canonical: EdgeHandle,
    ) -> Result<(), GraphStoreError> {
        journal_edge_insert(
            self,
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            false,
            inline_property_bytes,
            canonical,
        )
    }
}
