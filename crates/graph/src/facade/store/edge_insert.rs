//! GraphStore `edge_insert` implementation.

use crate::facade::topology::insert::{EdgeEnds, Placement};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId, PropertyId};
use gleaph_graph_kernel::entry::{TaggedEdgeLabelId, VertexId};
use gleaph_graph_kernel::index::IndexBuildSubject;
use gleaph_graph_kernel::plan_exec::MutationId;

use super::GraphStore;
use super::adjacency::{EdgeInsertSpec, journal_edge_insert};
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{
    edge_storage_label, lara_edge_placement, lara_label,
    validate_edge_inline_property_bytes_for_label,
};
use crate::edge_inline_property_schema::resolved_edge_label_with;
use crate::facade::store::index_build_admission::{
    FencedTransition, PlannedBuildEnvelope, trap_post_fence_commit,
};
use crate::property::inline_index_values;

/// Validated inputs for the LARA edge insert produced by the shared fenced preflight.
struct PreflightEdgeInsert {
    properties: Vec<(PropertyId, Value)>,
    label: TaggedEdgeLabelId,
    placement: Placement,
    planned: Vec<PlannedBuildEnvelope>,
    needs_commit: bool,
    shard_id: u32,
}

impl GraphStore {
    /// Shared pure prefix for a fenced edge insert: validates the endpoints, label, inline bytes,
    /// and initial sidecars, then runs the fence plan. Directed and undirected inserts differ only
    /// after this point (LARA placement and canonical resolution), so they share one preflight.
    fn preflight_edge_insert(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        directedness: EdgeDirectedness,
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
        let label = lara_label(edge_storage_label(catalog_label, directedness));
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
            0.into(),
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
            placement,
            planned,
            needs_commit,
            shard_id,
        } = {
            #[cfg(feature = "canbench")]
            let _scope = canbench_rs::bench_scope("scalar_insert_preflight");
            self.preflight_edge_insert(
                source_vertex_id,
                target_vertex_id,
                catalog_label,
                gleaph_graph_kernel::entry::EdgeDirectedness::Directed,
                inline_property_bytes,
                properties,
            )?
        };

        let canonical = {
            #[cfg(feature = "canbench")]
            let _scope = canbench_rs::bench_scope("scalar_insert_lara");
            crate::facade::topology::insert::insert_edge(
                EdgeEnds::Directed {
                    source: source_vertex_id,
                    target: target_vertex_id,
                },
                label,
                inline_property_bytes,
                placement,
            )?
        };
        let canonical = EdgeHandle::at_slot(
            canonical.owner_vertex_id,
            canonical.label_id,
            canonical.slot_index,
        );
        // Commit (infallible): bind the exact canonical subject and reserve+append the envelopes
        // before any derived dispatch or sidecar write.
        if needs_commit {
            #[cfg(feature = "canbench")]
            let _scope = canbench_rs::bench_scope("scalar_insert_index_admission");
            let subject = IndexBuildSubject::Edge {
                shard_id: shard_id.into(),
                owner_vertex_id: u32::try_from(u64::from(canonical.owner_vertex_id))
                    .expect("canonical owner vertex id fits u32"),
                label_id: canonical.label_id.raw(),
                slot_index: canonical.slot_index.raw(),
            };
            self.commit_index_build_admission(mutation_id, subject, planned);
        }
        #[cfg(feature = "canbench")]
        let _scope = canbench_rs::bench_scope("scalar_insert_sidecar");
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
            0.into(),
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
            placement,
            planned,
            needs_commit,
            shard_id,
        } = self.preflight_edge_insert(
            endpoint_a,
            endpoint_b,
            catalog_label,
            gleaph_graph_kernel::entry::EdgeDirectedness::Undirected,
            inline_property_bytes,
            properties,
        )?;
        let canonical = crate::facade::topology::insert::insert_edge(
            EdgeEnds::Undirected {
                a: endpoint_a,
                b: endpoint_b,
            },
            label,
            inline_property_bytes,
            placement,
        )?;
        let canonical = EdgeHandle::at_slot(
            canonical.owner_vertex_id,
            canonical.label_id,
            canonical.slot_index,
        );
        let owner_vertex_id = canonical.owner_vertex_id;
        let target = if owner_vertex_id == endpoint_a {
            endpoint_b
        } else {
            endpoint_a
        };
        if needs_commit {
            let subject = IndexBuildSubject::Edge {
                shard_id: shard_id.into(),
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
