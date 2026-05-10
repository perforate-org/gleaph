use crate::stable::edge_ids::{VertexEdgeIdAllocatorError, canonical_undirected_owner};
use crate::stable::label_catalog::LabelCatalogError;
use crate::stable::property_catalog::PropertyCatalogError;
use crate::stable::vertex_labels::VertexLabelStoreError;
use crate::stable::vertex_properties::VertexPropertyStoreError;
use crate::{
    EDGE_PROPERTIES, GRAPH, LABEL_CATALOG, PROPERTY_CATALOG, VERTEX_EDGE_IDS, VERTEX_LABELS,
    VERTEX_PROPERTIES,
};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{Edge, EdgeMeta, LabelId, PropertyId, Vertex, VertexEdgeId};
use ic_stable_lara::{
    BidirectionalMaintenanceReport, DeferredBidirectionalLaraGraph as Graph, DeleteEdgeObserver,
    MaintenanceBudget, VertexCount, VertexId, bidirectional::DeferredBidirectionalLaraError,
};
use std::fmt;

/// Stateless facade over graph storage thread-locals.
///
/// `GraphStore` is the public coordination point for operations that need to
/// touch multiple stable structures in a consistent order. It intentionally
/// carries no fields; all state lives in the canister-local stable structures
/// initialized in `lib.rs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphStore;

#[derive(Clone, Copy, Debug, Default)]
struct EdgePropertyDeleteObserver;

impl DeleteEdgeObserver<Edge> for EdgePropertyDeleteObserver {
    fn on_delete_outgoing_edge(&mut self, source: VertexId, edge: Edge) {
        let owner = GraphStore::edge_sidecar_owner_from_out_row(source, &edge);
        GraphStore::clear_edge_properties_stable(owner, edge.vertex_edge_id);
    }

    fn on_delete_incoming_edge(&mut self, destination: VertexId, edge: Edge) {
        let owner = GraphStore::edge_sidecar_owner_from_in_row(destination, &edge);
        GraphStore::clear_edge_properties_stable(owner, edge.vertex_edge_id);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeHandle {
    pub owner_vertex_id: VertexId,
    pub vertex_edge_id: VertexEdgeId,
}

#[derive(Debug)]
pub enum GraphStoreError {
    Graph(DeferredBidirectionalLaraError),
    VertexEdgeId(VertexEdgeIdAllocatorError),
    LabelCatalog(LabelCatalogError),
    PropertyCatalog(PropertyCatalogError),
    VertexLabel(VertexLabelStoreError),
    PropertyValue(VertexPropertyStoreError),
    /// `DELETE` vertex without `DETACH` while the vertex still has incident edges.
    VertexNotDetached {
        vertex_id: VertexId,
    },
    /// No outgoing edge record matches the handle on the owner's forward row.
    EdgeNotFound {
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
    },
}

impl fmt::Display for GraphStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(err) => write!(f, "{err}"),
            Self::VertexEdgeId(err) => write!(f, "{err}"),
            Self::LabelCatalog(err) => write!(f, "{err}"),
            Self::PropertyCatalog(err) => write!(f, "{err}"),
            Self::VertexLabel(err) => write!(f, "{err}"),
            Self::PropertyValue(err) => write!(f, "{err}"),
            Self::VertexNotDetached { vertex_id } => write!(
                f,
                "cannot delete vertex {vertex_id:?} without DETACH while it still has incident edges"
            ),
            Self::EdgeNotFound {
                owner_vertex_id,
                vertex_edge_id,
            } => write!(
                f,
                "no edge record for owner {owner_vertex_id:?} and local edge id {vertex_edge_id:?}"
            ),
        }
    }
}

impl std::error::Error for GraphStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(err) => Some(err),
            Self::VertexEdgeId(err) => Some(err),
            Self::LabelCatalog(err) => Some(err),
            Self::PropertyCatalog(err) => Some(err),
            Self::VertexLabel(err) => Some(err),
            Self::PropertyValue(err) => Some(err),
            Self::VertexNotDetached { .. } | Self::EdgeNotFound { .. } => None,
        }
    }
}

impl From<DeferredBidirectionalLaraError> for GraphStoreError {
    fn from(value: DeferredBidirectionalLaraError) -> Self {
        Self::Graph(value)
    }
}

impl From<VertexEdgeIdAllocatorError> for GraphStoreError {
    fn from(value: VertexEdgeIdAllocatorError) -> Self {
        Self::VertexEdgeId(value)
    }
}

impl GraphStore {
    pub const fn new() -> Self {
        Self
    }

    pub fn label_id(&self, name: &str) -> Option<LabelId> {
        LABEL_CATALOG.with(|catalog| catalog.borrow().get_id(name))
    }

    pub fn label_name(&self, id: LabelId) -> Option<String> {
        LABEL_CATALOG.with(|catalog| catalog.borrow().get_name(id))
    }

    pub fn get_or_insert_label_id(&self, name: &str) -> Result<LabelId, LabelCatalogError> {
        LABEL_CATALOG.with(|catalog| catalog.borrow_mut().get_or_insert(name))
    }

    pub fn insert_label_with_id(&self, name: &str, id: LabelId) -> Result<(), LabelCatalogError> {
        LABEL_CATALOG.with(|catalog| catalog.borrow_mut().insert_with_id(name, id))
    }

    pub fn property_id(&self, name: &str) -> Option<PropertyId> {
        PROPERTY_CATALOG.with(|catalog| catalog.borrow().get_id(name))
    }

    pub fn property_name(&self, id: PropertyId) -> Option<String> {
        PROPERTY_CATALOG.with(|catalog| catalog.borrow().get_name(id))
    }

    pub fn get_or_insert_property_id(
        &self,
        name: &str,
    ) -> Result<PropertyId, PropertyCatalogError> {
        PROPERTY_CATALOG.with(|catalog| catalog.borrow_mut().get_or_insert(name))
    }

    pub fn insert_property_with_id(
        &self,
        name: &str,
        id: PropertyId,
    ) -> Result<(), PropertyCatalogError> {
        PROPERTY_CATALOG.with(|catalog| catalog.borrow_mut().insert_with_id(name, id))
    }

    pub fn vertex_count(&self) -> VertexCount {
        GRAPH.with(|graph| graph.borrow().vertex_count())
    }

    pub fn insert_vertex(&self) -> Result<VertexId, DeferredBidirectionalLaraError> {
        self.insert_vertex_row(Vertex::default())
    }

    pub fn insert_vertex_row(
        &self,
        vertex: Vertex,
    ) -> Result<VertexId, DeferredBidirectionalLaraError> {
        self.with_graph_mut(|graph| graph.push_vertex(vertex))
    }

    pub fn vertex(&self, vertex_id: VertexId) -> Option<Vertex> {
        if !self.contains_vertex(vertex_id) {
            return None;
        }
        GRAPH.with(|graph| Some(graph.borrow().forward().vertices().get(vertex_id)))
    }

    pub fn set_vertex(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
    ) -> Result<(), DeferredBidirectionalLaraError> {
        self.ensure_vertex_id(vertex_id)?;
        GRAPH.with(|graph| {
            let graph = graph.borrow();
            graph.forward().vertices().set(vertex_id, &vertex);
            graph.reverse().vertices().set(vertex_id, &vertex);
        });
        Ok(())
    }

    pub fn vertex_labels(&self, vertex_id: VertexId, vertex: Vertex) -> Vec<LabelId> {
        VERTEX_LABELS.with(|labels| labels.borrow().labels_for(vertex_id, vertex))
    }

    pub fn set_vertex_labels(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        labels: impl IntoIterator<Item = LabelId>,
    ) -> Result<Vertex, VertexLabelStoreError> {
        VERTEX_LABELS.with(|store| store.borrow_mut().set_labels(vertex_id, vertex, labels))
    }

    pub fn add_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: LabelId,
    ) -> Result<Vertex, VertexLabelStoreError> {
        VERTEX_LABELS.with(|store| store.borrow_mut().add_label(vertex_id, vertex, label))
    }

    pub fn remove_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: LabelId,
    ) -> Vertex {
        VERTEX_LABELS.with(|store| store.borrow_mut().remove_label(vertex_id, vertex, label))
    }

    pub fn vertex_property(&self, vertex_id: VertexId, property_id: PropertyId) -> Option<Value> {
        VERTEX_PROPERTIES.with(|properties| properties.borrow().get(vertex_id, property_id))
    }

    pub fn set_vertex_property(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        VERTEX_PROPERTIES
            .with(|properties| properties.borrow_mut().set(vertex_id, property_id, value))
    }

    pub fn remove_vertex_property(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
    ) -> Option<Value> {
        VERTEX_PROPERTIES.with(|properties| properties.borrow_mut().remove(vertex_id, property_id))
    }

    pub fn vertex_properties(&self, vertex_id: VertexId) -> Vec<(PropertyId, Value)> {
        VERTEX_PROPERTIES.with(|properties| properties.borrow().properties_for(vertex_id))
    }

    pub fn edge_property(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
    ) -> Option<Value> {
        EDGE_PROPERTIES.with(|properties| {
            properties
                .borrow()
                .get(owner_vertex_id, vertex_edge_id, property_id)
        })
    }

    pub fn set_edge_property(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        EDGE_PROPERTIES.with(|properties| {
            properties
                .borrow_mut()
                .set(owner_vertex_id, vertex_edge_id, property_id, value)
        })
    }

    pub fn remove_edge_property(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
    ) -> Option<Value> {
        EDGE_PROPERTIES.with(|properties| {
            properties
                .borrow_mut()
                .remove(owner_vertex_id, vertex_edge_id, property_id)
        })
    }

    pub fn edge_properties(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
    ) -> Vec<(PropertyId, Value)> {
        EDGE_PROPERTIES.with(|properties| {
            properties
                .borrow()
                .properties_for_edge(owner_vertex_id, vertex_edge_id)
        })
    }

    pub fn allocate_vertex_edge_id(
        &self,
        owner_vertex_id: VertexId,
    ) -> Result<VertexEdgeId, VertexEdgeIdAllocatorError> {
        VERTEX_EDGE_IDS.with(|ids| ids.borrow_mut().allocate_for_owner(owner_vertex_id))
    }

    pub fn allocate_directed_edge_id(
        &self,
        source_vertex_id: VertexId,
    ) -> Result<(VertexId, VertexEdgeId), VertexEdgeIdAllocatorError> {
        VERTEX_EDGE_IDS.with(|ids| ids.borrow_mut().allocate_directed(source_vertex_id))
    }

    pub fn allocate_undirected_edge_id(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
    ) -> Result<(VertexId, VertexEdgeId), VertexEdgeIdAllocatorError> {
        VERTEX_EDGE_IDS.with(|ids| ids.borrow_mut().allocate_undirected(endpoint_a, endpoint_b))
    }

    pub fn insert_directed_edge(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        meta: EdgeMeta,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        self.ensure_vertex_id(target_vertex_id)?;

        let (owner_vertex_id, vertex_edge_id) = self.allocate_directed_edge_id(source_vertex_id)?;
        let edge = Edge {
            target: target_vertex_id,
            vertex_edge_id,
            meta: meta.with_undirected(false),
        };
        self.with_graph_mut(|graph| {
            graph.insert_directed_deferred(source_vertex_id, target_vertex_id, edge)
        })?;

        Ok(EdgeHandle {
            owner_vertex_id,
            vertex_edge_id,
        })
    }

    pub fn insert_undirected_edge(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        meta: EdgeMeta,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(endpoint_a)?;
        self.ensure_vertex_id(endpoint_b)?;

        let (owner_vertex_id, vertex_edge_id) =
            self.allocate_undirected_edge_id(endpoint_a, endpoint_b)?;
        let edge = Edge {
            target: endpoint_b,
            vertex_edge_id,
            meta: meta.with_undirected(true),
        };
        self.with_graph_mut(|graph| {
            graph.insert_undirected_deferred(endpoint_a, endpoint_b, edge)
        })?;

        Ok(EdgeHandle {
            owner_vertex_id,
            vertex_edge_id,
        })
    }

    pub fn out_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLaraError> {
        GRAPH.with(|graph| graph.borrow().collect_out_edges_slot_order(vertex_id))
    }

    pub fn in_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLaraError> {
        GRAPH.with(|graph| graph.borrow().collect_in_edges_slot_order(vertex_id))
    }

    /// Runs deferred LARA maintenance until the queue is empty or the budget is exhausted.
    ///
    /// Production canisters should use a tight instruction budget and rely on
    /// heartbeat/timer draining; tests and small graphs typically pass
    /// `MaintenanceBudget { max_instructions: 0, .. }` to disable the instruction cap.
    ///
    /// For timer-driven draining with a conservative cap under the ICP per-message limit,
    /// prefer [`Self::run_timer_maintenance_tick`].
    ///
    /// See `docs/ic-timer-maintenance-strategy.md` for the intended canister maintenance model.
    pub fn run_maintenance_best_effort(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<BidirectionalMaintenanceReport, GraphStoreError> {
        GRAPH
            .with(|graph| {
                let graph = graph.borrow();
                let mut observer = EdgePropertyDeleteObserver;
                graph.maintenance_with_delete_observer(budget, &mut observer)
            })
            .map_err(GraphStoreError::from)
    }

    /// Runs one **budgeted** LARA maintenance pass for timer/heartbeat loops.
    ///
    /// Uses [`timer_lara_maintenance_budget`](crate::facade::timer_lara_maintenance_budget),
    /// aligned with the ICP per-message instruction ceiling documented at
    /// <https://docs.internetcomputer.org/references/cycles-costs/#resource-limits>.
    /// Call again on later timer ticks while the returned report's
    /// `remaining_queue_len()` is non-zero, or when a prior budgeted run set
    /// `instruction_budget_exhausted` and work may remain.
    ///
    /// Mutation paths that must finish deferred work in the same message should
    /// keep using the internal full drain (`max_instructions: 0`) instead.
    pub fn run_timer_maintenance_tick(
        &self,
    ) -> Result<BidirectionalMaintenanceReport, GraphStoreError> {
        self.run_maintenance_best_effort(crate::facade::timer_lara_maintenance_budget())
    }

    /// `DELETE` semantics: remove the vertex only when it has no incident edges.
    pub fn delete_vertex(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        if self.vertex_has_incident_edges(vertex_id)? {
            return Err(GraphStoreError::VertexNotDetached { vertex_id });
        }
        self.clear_vertex_stable_payloads_before_graph_delete(vertex_id)?;
        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))?;
        self.drain_deferred_maintenance()?;
        Ok(())
    }

    /// `DETACH DELETE` semantics: remove all incident edges, then delete the vertex.
    ///
    /// Incident edges are cleared via LARA's queued incremental `delete_vertex_deferred`
    /// maintenance; stable edge property sidecars are cleared as each edge is removed.
    pub fn detach_delete_vertex(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        self.clear_vertex_stable_payloads_before_graph_delete(vertex_id)?;
        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))?;
        self.drain_deferred_maintenance()?;
        Ok(())
    }

    /// Removes one logical edge (and its stable properties) identified by `handle`.
    pub fn delete_edge_by_handle(&self, handle: EdgeHandle) -> Result<(), GraphStoreError> {
        self.ensure_vertex_id(handle.owner_vertex_id)
            .map_err(GraphStoreError::from)?;
        let edge = self
            .find_outgoing_edge_record(handle.owner_vertex_id, handle.vertex_edge_id)?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: handle.owner_vertex_id,
                vertex_edge_id: handle.vertex_edge_id,
            })?;
        Self::clear_edge_properties_stable(handle.owner_vertex_id, handle.vertex_edge_id);
        let removed = if edge.meta.is_undirected() {
            self.with_graph_mut(|graph| {
                graph.remove_undirected_deferred(handle.owner_vertex_id, edge.target, edge)
            })?
        } else {
            self.with_graph_mut(|graph| {
                graph.remove_directed_deferred(handle.owner_vertex_id, edge.target, edge)
            })?
        };
        if !removed {
            return Err(GraphStoreError::EdgeNotFound {
                owner_vertex_id: handle.owner_vertex_id,
                vertex_edge_id: handle.vertex_edge_id,
            });
        }
        self.drain_deferred_maintenance()?;
        Ok(())
    }

    fn drain_deferred_maintenance(&self) -> Result<(), GraphStoreError> {
        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        self.run_maintenance_best_effort(budget)?;
        Ok(())
    }

    fn vertex_has_incident_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<bool, DeferredBidirectionalLaraError> {
        GRAPH.with(|graph| graph.borrow().has_incident_edges(vertex_id))
    }

    fn edge_sidecar_owner_from_out_row(endpoint: VertexId, edge: &Edge) -> VertexId {
        if edge.meta.is_undirected() {
            canonical_undirected_owner(endpoint, edge.target)
        } else {
            endpoint
        }
    }

    fn edge_sidecar_owner_from_in_row(dst: VertexId, edge: &Edge) -> VertexId {
        if edge.meta.is_undirected() {
            canonical_undirected_owner(dst, edge.target)
        } else {
            edge.target
        }
    }

    fn clear_edge_properties_stable(owner_vertex_id: VertexId, vertex_edge_id: VertexEdgeId) {
        EDGE_PROPERTIES.with(|store| {
            store
                .borrow_mut()
                .remove_all_for_edge(owner_vertex_id, vertex_edge_id);
        });
    }

    fn clear_vertex_properties_stable_only(&self, vertex_id: VertexId) {
        let props: Vec<PropertyId> = VERTEX_PROPERTIES.with(|store| {
            store
                .borrow()
                .properties_for(vertex_id)
                .into_iter()
                .map(|(pid, _)| pid)
                .collect()
        });
        for pid in props {
            VERTEX_PROPERTIES.with(|store| {
                store.borrow_mut().remove(vertex_id, pid);
            });
        }
    }

    fn clear_vertex_stable_payloads_before_graph_delete(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        self.clear_vertex_properties_stable_only(vertex_id);

        let vertex = self.vertex(vertex_id).ok_or_else(|| {
            GraphStoreError::Graph(DeferredBidirectionalLaraError::VertexOutOfRange {
                vid: vertex_id,
                len: self.vertex_count(),
            })
        })?;
        let vertex = VERTEX_LABELS.with(|labels| {
            labels
                .borrow_mut()
                .set_labels(vertex_id, vertex, [])
                .map_err(GraphStoreError::from)
        })?;
        self.set_vertex(vertex_id, vertex)
            .map_err(GraphStoreError::from)?;
        Ok(())
    }

    fn find_outgoing_edge_record(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
    ) -> Result<Option<Edge>, GraphStoreError> {
        let edges = self
            .out_edges(owner_vertex_id)
            .map_err(GraphStoreError::from)?;
        Ok(edges
            .into_iter()
            .find(|candidate| candidate.vertex_edge_id == vertex_edge_id))
    }

    fn contains_vertex(&self, vertex_id: VertexId) -> bool {
        u64::from(vertex_id) < u64::from(self.vertex_count())
    }

    fn ensure_vertex_id(&self, vertex_id: VertexId) -> Result<(), DeferredBidirectionalLaraError> {
        if self.contains_vertex(vertex_id) {
            Ok(())
        } else {
            Err(DeferredBidirectionalLaraError::VertexOutOfRange {
                vid: vertex_id,
                len: self.vertex_count(),
            })
        }
    }

    pub(crate) fn with_graph_mut<R>(
        &self,
        f: impl FnOnce(&mut Graph<Edge, Vertex, crate::stable::memory::Memory>) -> R,
    ) -> R {
        GRAPH.with(|graph| f(&mut graph.borrow_mut()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_vertices_and_edges_through_facade() {
        let store = GraphStore::new();
        let start: u64 = store.vertex_count().into();
        let source = store.insert_vertex().expect("insert source vertex");
        let target = store.insert_vertex().expect("insert target vertex");

        assert_eq!(source, VertexId::from(start as u32));
        assert_eq!(target, VertexId::from(start as u32 + 1));

        let directed = store
            .insert_directed_edge(source, target, EdgeMeta::default())
            .expect("insert directed edge");

        assert_eq!(directed.owner_vertex_id, source);
        assert_eq!(directed.vertex_edge_id, VertexEdgeId::from_raw(1));

        let out_edges = store.out_edges(source).expect("read out edges");
        assert!(out_edges.iter().any(|edge| {
            edge.target == target
                && edge.vertex_edge_id == directed.vertex_edge_id
                && !edge.meta.is_undirected()
        }));

        let undirected = store
            .insert_undirected_edge(target, source, EdgeMeta::default())
            .expect("insert undirected edge");

        assert_eq!(undirected.owner_vertex_id, target);
        assert_eq!(undirected.vertex_edge_id, VertexEdgeId::from_raw(1));

        let target_out_edges = store.out_edges(target).expect("read target out edges");
        assert!(target_out_edges.iter().any(|edge| {
            edge.target == source
                && edge.vertex_edge_id == undirected.vertex_edge_id
                && edge.meta.is_undirected()
        }));
    }

    #[test]
    fn timer_maintenance_tick_runs_on_empty_graph() {
        let store = GraphStore::new();
        let report = store.run_timer_maintenance_tick().expect("tick");
        assert_eq!(report.remaining_queue_len(), 0);
    }
}
