use std::cell::RefCell;

use super::*;

impl<'a, M: Memory> GraphPmaBatchSession<'a, M> {
    /// Creates one facade-level batch mutation session.
    pub fn new(
        graph: &'a mut GraphRuntime,
        manager: &'a RefCell<RegionManager>,
        memory: &'a M,
    ) -> Self {
        Self {
            inner: GraphBatchMutationSession::new(graph, manager, memory),
        }
    }

    /// Returns the graph runtime currently being mutated.
    pub fn graph(&self) -> &GraphRuntime {
        self.inner.graph()
    }

    /// Returns the graph runtime mutably.
    pub fn graph_mut(&mut self) -> &mut GraphRuntime {
        self.inner.graph_mut()
    }

    /// Prepares local capacity for an upcoming batch without inserting yet.
    pub fn prepare_local_capacity(&mut self, spec: RebalancePrepareSpec<'_>) -> Option<bool> {
        self.inner.prepare_local_capacity(spec)
    }

    /// Inserts one edge using the batch-aware rebalance path without flushing yet.
    pub fn insert_edge_pair(&mut self, spec: RebalanceInsertSpec<'_>) -> Option<GraphInsertResult> {
        self.inner.insert_edge_pair(spec)
    }

    /// Replaces one logical edge without flushing yet.
    pub fn replace_edge_pair(
        &mut self,
        spec: EdgeReplaceSpec,
    ) -> Option<(GraphMutationPath, (EdgeEntry, EdgeEntry))> {
        self.inner.replace_edge_pair(spec)
    }

    /// Tombstones one logical edge without flushing yet.
    pub fn tombstone_edge_pair(&mut self, spec: EdgeTombstoneSpec) -> Option<GraphMutationPath> {
        self.inner.tombstone_edge_pair(spec)
    }

    /// Flushes dirty graph state accumulated so far in this batch.
    pub fn flush(&mut self) -> Result<(Vec<usize>, Vec<usize>), WritebackError> {
        self.inner.flush()
    }
}

impl<'a, S: GraphPmaStore> GraphPmaStoreAdapter<'a, S> {
    /// Creates one adapter over a graph store plus stable memory.
    pub fn new(store: &'a mut S, memory: &'a S::Mem) -> Self {
        Self { store, memory }
    }

    /// Returns immutable access to the wrapped store.
    pub fn store(&self) -> &S {
        self.store
    }

    /// Returns the most recent facade-level write event observed through the bound store.
    pub fn last_write_event(&self) -> Option<&GraphPmaFacadeWriteEvent> {
        self.store.last_write_event()
    }

    /// Returns recent facade-level write events in observation order.
    pub fn write_history(&self) -> &[GraphPmaFacadeWriteEvent] {
        self.store.write_history()
    }

    /// Returns the recent façade write history projected onto the shared event vocabulary.
    pub fn shared_write_history(&self) -> Vec<GraphPmaWriteEventProjection> {
        self.write_history()
            .iter()
            .flat_map(GraphPmaFacadeWriteEvent::shared_projections)
            .collect()
    }

    /// Returns the recent bound-store write history formatted as compact diagnostics lines.
    pub fn formatted_write_history(&self) -> Vec<String> {
        format_write_event_history(&self.shared_write_history())
    }

    pub fn formatted_last_write_event(&self) -> Option<String> {
        format_last_write_event(&self.shared_write_history())
    }

    /// Returns mutable access to the wrapped store.
    pub fn store_mut(&mut self) -> &mut S {
        self.store
    }

    /// Consumes the adapter and returns its wrapped store plus bound memory.
    pub fn into_parts(self) -> (&'a mut S, &'a S::Mem) {
        (self.store, self.memory)
    }

    /// Appends one empty vertex slot pair.
    pub fn append_empty_vertex_pair(&mut self) -> GraphPmaResult<(usize, usize)> {
        self.store.append_empty_vertex_pair()
    }

    /// Appends `count` empty vertex slot pairs.
    pub fn append_empty_vertex_pairs(
        &mut self,
        count: usize,
    ) -> GraphPmaResult<Vec<(usize, usize)>> {
        self.store.append_empty_vertex_pairs(count)
    }

    /// Bootstraps multiple vertex refs and initial edges using the bound memory handle.
    pub fn bootstrap_vertex_refs_and_edges(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> GraphPmaResult<GraphPmaBootstrapGraphWriteSummary> {
        self.store.bootstrap_vertex_refs_and_edges_and_write(
            vertex_refs,
            initial_edges,
            self.memory,
        )
    }

    /// Inserts one logical edge using the bound memory handle.
    pub fn insert_edge_pair_with_local_rebalance(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        self.store
            .insert_edge_pair_with_local_rebalance_and_write(spec, self.memory)
    }

    /// Replaces one logical edge using the bound memory handle.
    pub fn replace_edge_pair(
        &mut self,
        spec: EdgeReplaceSpec,
    ) -> Result<GraphPmaReplaceEdgeSummary, WritebackError> {
        self.store.replace_edge_pair_and_write(spec, self.memory)
    }

    /// Tombstones one logical edge using the bound memory handle.
    pub fn tombstone_edge_pair(
        &mut self,
        spec: EdgeTombstoneSpec,
    ) -> Result<GraphPmaMutationWriteSummary<GraphMutationPath>, WritebackError> {
        self.store.tombstone_edge_pair_and_write(spec, self.memory)
    }

    /// Flushes dirty state using the bound memory handle.
    pub fn flush_dirty(&mut self) -> GraphPmaResult<GraphPmaRefreshedVertices> {
        let (forward, reverse) = self
            .store
            .try_refresh_and_write_dirty_to_stable_memory(self.memory)?;
        Ok(GraphPmaRefreshedVertices::new(forward, reverse))
    }

    /// Resolves one forward-surface logical locator against the current runtime.
    pub fn resolve_forward_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        self.store
            .graph()
            .forward
            .resolve_logical_edge_slot(vertex_ref, ordinal, locator)
    }

    /// Resolves one reverse-surface logical locator against the current runtime.
    pub fn resolve_reverse_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        self.store
            .graph()
            .reverse
            .resolve_logical_edge_slot(vertex_ref, ordinal, locator)
    }
}

impl<'a, M: Memory> GraphPmaStoreAdapter<'a, GraphPma<M>> {
    /// Starts one facade-level batch mutation session through the bound adapter.
    pub fn begin_batch_mutation(&'a mut self) -> GraphPmaBatchSession<'a, M> {
        self.store.begin_batch_mutation(self.memory)
    }
}

impl<'a, S: GraphPmaStore> GraphPmaService for GraphPmaStoreAdapter<'a, S> {
    fn last_write_event(&self) -> Option<&GraphPmaFacadeWriteEvent> {
        Self::last_write_event(self)
    }

    fn write_history(&self) -> &[GraphPmaFacadeWriteEvent] {
        Self::write_history(self)
    }

    fn bootstrap_vertex_refs_and_edges(
        &mut self,
        vertex_refs: &[VertexRef],
        initial_edges: &[(EdgeId, usize, usize, LabelId)],
    ) -> GraphPmaResult<GraphPmaBootstrapGraphWriteSummary> {
        Self::bootstrap_vertex_refs_and_edges(self, vertex_refs, initial_edges)
    }

    fn insert_edge_pair_with_local_rebalance(
        &mut self,
        spec: RebalanceInsertSpec<'_>,
    ) -> Result<GraphInsertWriteSummary, WritebackError> {
        Self::insert_edge_pair_with_local_rebalance(self, spec)
    }

    fn replace_edge_pair(
        &mut self,
        spec: EdgeReplaceSpec,
    ) -> Result<GraphPmaReplaceEdgeSummary, WritebackError> {
        Self::replace_edge_pair(self, spec)
    }

    fn tombstone_edge_pair(
        &mut self,
        spec: EdgeTombstoneSpec,
    ) -> Result<GraphPmaMutationWriteSummary<GraphMutationPath>, WritebackError> {
        Self::tombstone_edge_pair(self, spec)
    }

    fn flush_dirty(&mut self) -> GraphPmaResult<GraphPmaRefreshedVertices> {
        Self::flush_dirty(self)
    }
}
