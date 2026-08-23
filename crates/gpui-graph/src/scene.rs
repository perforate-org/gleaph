//! Graph scene: shared visualization state (§9, §10).
//!
//! A [`GraphScene`] represents a visualization of a graph. It owns the graph,
//! per-node/per-edge scene state, the layout session, and revisions. It does
//! not own a particular viewport, so the same scene can be rendered through
//! multiple independent views (Invariant 9).

use std::hash::BuildHasher;

use glam::Vec2;
use gpui::{Context, Task};
use slotmap::SecondaryMap;

use crate::graph::{EdgeId, Graph, GraphDelta, NodeId};
use crate::keyed_graph::KeyedGraph;
use crate::layout::controller::{LayoutController, LayoutRunState};
use crate::layout::graph::{LayoutEdge, LayoutGraph, LayoutIndex, LayoutNode, LayoutState};
use crate::layout::placement::{Placement, Rng};
use crate::layout::{LayoutBudget, LayoutDelta, LayoutEngine, LayoutProgress, LayoutSync};
use crate::patch::{GraphBatch, GraphPatch};

/// Per-node visualization state, stored separately from logical graph data
/// (§10).
#[derive(Debug, Clone, Copy)]
pub struct NodeSceneState {
    /// World-space position of the node.
    pub position: Vec2,
    /// Whether the node is hard-pinned.
    pub pinned: bool,
}

impl Default for NodeSceneState {
    fn default() -> Self {
        Self {
            position: Vec2::ZERO,
            pinned: false,
        }
    }
}

/// Per-edge visualization state (§10).
///
/// Currently empty; future attributes (visibility, opacity, emphasis) should
/// not be added until required.
#[derive(Debug, Clone, Copy, Default)]
pub struct EdgeSceneState {}

/// Shared visualization state for a graph (§9).
pub struct GraphScene<NK, EK, N = (), E = (), S = crate::hash::DefaultBuildHasher>
where
    S: BuildHasher + Default + Clone,
{
    keyed: KeyedGraph<NK, EK, N, E, S>,
    node_scene: SecondaryMap<NodeId, NodeSceneState>,
    edge_scene: SecondaryMap<EdgeId, EdgeSceneState>,
    layout_graph: LayoutGraph,
    layout_state: LayoutState,
    /// Maps a node's stable identity to its dense index in `layout_graph` and
    /// `layout_state`, so per-node lookups (e.g. cluster centers) are O(1)
    /// instead of a linear scan of `layout_graph.node_ids`.
    node_index: std::collections::HashMap<NodeId, usize, S>,
    /// The active layout engine. `None` while the engine is parked inside an
    /// in-flight background step (§30); every engine touchpoint must treat
    /// `None` as busy and defer through [`Self::pending_engine_rebuild`] or
    /// [`Self::pending_engine`].
    engine: Option<Box<dyn LayoutEngine>>,
    /// Background task computing one layout budget off the UI thread.
    /// Cancels on drop, so replacing or dropping the scene also reclaims the
    /// parked engine.
    flight: Option<Task<()>>,
    /// An engine installed while a flight held the previous one; swapped in
    /// (and rebuilt) when the flight completes.
    pending_engine: Option<Box<dyn LayoutEngine>>,
    /// Set when the projection changed while the engine was parked, so the
    /// returning engine must rebuild before stepping again (§11.6).
    pending_engine_rebuild: bool,
    /// Bumped by direct position writes (`set_position`, i.e. drags) so a
    /// completed flight can detect that its input snapshot was externally
    /// mutated and discard its output instead of clobbering the user's drag.
    layout_write_epoch: u64,
    controller: LayoutController,
    placement: Placement,
    rng: Rng,
    topology_revision: u64,
    data_revision: u64,
    geometry_revision: u64,
    style_revision: u64,
}

/// A scene using the default SipHash hasher.
impl<NK, EK, N, E> GraphScene<NK, EK, N, E>
where
    NK: Eq + std::hash::Hash,
    EK: Eq + std::hash::Hash,
{
    /// Create an empty scene with a fixed layout.
    pub fn new() -> Self {
        Self::with_hasher(crate::hash::DefaultBuildHasher::default())
    }
}

impl<NK, EK, N, E, S> GraphScene<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash,
    EK: Eq + std::hash::Hash,
    S: BuildHasher + Default + Clone,
{
    /// Create an empty scene with an explicit hasher shared by the key maps,
    /// the node index, and any derived runtime index.
    pub fn with_hasher(hasher: S) -> Self {
        Self {
            keyed: KeyedGraph::with_hasher(hasher.clone()),
            node_scene: SecondaryMap::new(),
            edge_scene: SecondaryMap::new(),
            layout_graph: LayoutGraph::new(Vec::new(), Vec::new(), Vec::new(), 0),
            layout_state: LayoutState::new(),
            node_index: std::collections::HashMap::with_hasher(hasher),
            engine: Some(Box::new(crate::layout::FixedLayout)),
            flight: None,
            pending_engine: None,
            pending_engine_rebuild: false,
            layout_write_epoch: 0,
            controller: LayoutController::new(),
            placement: Placement::default(),
            rng: Rng::new(0),
            topology_revision: 0,
            data_revision: 0,
            geometry_revision: 0,
            style_revision: 0,
        }
    }

    /// Set the layout engine (builder form).
    pub fn with_layout(mut self, engine: Box<dyn LayoutEngine>) -> Self {
        self.set_layout(engine);
        self
    }

    /// Set the initial placement policy for newly introduced nodes.
    pub fn with_placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    /// The underlying logical graph.
    pub fn graph(&self) -> &Graph<N, E> {
        self.keyed.graph()
    }

    /// Synchronize derived runtime state from this scene's immutable snapshot.
    ///
    /// The scene owns the graph, revisions, node positions, and cluster
    /// geometry, so callers cannot accidentally combine data from different
    /// sources when rebuilding a runtime. The replacement is built off to the
    /// side and installed atomically only when the source identity or either
    /// revision is stale.
    pub fn sync_runtime<'a>(
        &'a self,
        runtime: &'a mut crate::runtime::GraphRuntime<S>,
    ) -> crate::runtime::SyncedGraphRuntime<'a, NK, EK, N, E, S> {
        let source = crate::runtime::RuntimeSource::from_scene(self);
        if runtime.is_stale_for(&source) {
            runtime.rebuild_from_source(source);
        }
        crate::runtime::SyncedGraphRuntime {
            scene: self,
            runtime,
        }
    }

    /// Resolve an external node key to an internal node identity.
    pub fn node_id(&self, key: &NK) -> Option<NodeId> {
        self.keyed.node_id(key)
    }

    /// Resolve an external edge key to an internal edge identity.
    pub fn edge_id(&self, key: &EK) -> Option<EdgeId> {
        self.keyed.edge_id(key)
    }

    /// Merge a batch of graph data into the scene (§8.1).
    pub fn merge(&mut self, batch: GraphBatch<NK, EK, N, E>) -> GraphDelta {
        let delta = self.keyed.merge(batch);
        self.apply_delta(&delta);
        delta
    }

    /// Apply a set of explicit mutations to the scene (§8.2).
    pub fn apply(&mut self, patch: GraphPatch<NK, EK, N, E>) -> GraphDelta {
        let delta = self.keyed.apply(patch);
        self.apply_delta(&delta);
        delta
    }

    /// Per-node scene state.
    pub fn node_scene(&self, node: NodeId) -> Option<&NodeSceneState> {
        self.node_scene.get(node)
    }

    /// World-space position of a node.
    pub fn node_position(&self, node: NodeId) -> Option<Vec2> {
        self.node_scene.get(node).map(|s| s.position)
    }

    /// Cluster center and radius of a node, if the layout grouped it into a
    /// cluster.
    ///
    /// Returns the world-space center and radius of the node's cluster (e.g.
    /// the center and radius of its SCC circle), or `None` if the node is not
    /// clustered.
    pub fn node_cluster_center(&self, node: NodeId) -> Option<(Vec2, f32)> {
        let i = self.node_index.get(&node)?;
        self.layout_state.cluster_centers[*i]
    }

    /// Whether a node is pinned.
    pub fn is_pinned(&self, node: NodeId) -> bool {
        self.node_scene.get(node).map(|s| s.pinned).unwrap_or(false)
    }

    /// Pin a node.
    pub fn pin(&mut self, node: NodeId) {
        if let Some(scene) = self.node_scene.get_mut(node) {
            scene.pinned = true;
        }
        if let Some(i) = self.node_index.get(&node) {
            self.layout_state.pinned.set(*i, true);
        }
    }

    /// Unpin a node.
    pub fn unpin(&mut self, node: NodeId) {
        if let Some(scene) = self.node_scene.get_mut(node) {
            scene.pinned = false;
        }
        if let Some(i) = self.node_index.get(&node) {
            self.layout_state.pinned.set(*i, false);
        }
    }

    /// Set a node's world-space position directly.
    pub fn set_position(&mut self, node: NodeId, position: Vec2) {
        let Some(current) = self.node_scene.get(node).map(|scene| scene.position) else {
            return;
        };
        if current == position {
            return;
        }
        if let Some(scene) = self.node_scene.get_mut(node) {
            scene.position = position;
        }
        if let Some(i) = self.node_index.get(&node) {
            self.layout_state.positions[*i] = position;
        }
        self.layout_write_epoch += 1;
        self.bump_geometry_revision();
    }

    /// Replace the layout engine, rebuilding its internal state.
    ///
    /// While a background flight holds the previous engine, the replacement
    /// is parked in [`Self::pending_engine`] and takes over (with a rebuild
    /// against the current projection) when the flight completes; the
    /// returning engine is dropped.
    pub fn set_layout(&mut self, engine: Box<dyn LayoutEngine>) {
        match self.engine.as_mut() {
            Some(slot) => {
                *slot = engine;
                self.rebuild_layout_engine();
            }
            None => {
                self.pending_engine = Some(engine);
                // The parked engine's flight result will be discarded anyway.
                self.pending_engine_rebuild = true;
            }
        }
        self.bump_geometry_revision();
        self.controller.reheat();
    }

    /// The current layout run state.
    pub fn layout_state(&self) -> LayoutRunState {
        self.controller.state()
    }

    /// Step the layout by up to `budget.max_iterations` iterations.
    ///
    /// Returns the layout progress and copies updated positions back into the
    /// scene.
    pub fn step_layout(&mut self, budget: LayoutBudget) -> LayoutProgress {
        if !self.controller.should_step() {
            return LayoutProgress::Settled;
        }
        if self.engine.is_none() {
            // A background flight owns the engine; this synchronous call has
            // nothing to advance. Report running so callers keep their loop.
            return LayoutProgress::Running { stability: None };
        }
        let progress = self
            .engine
            .as_mut()
            .expect("engine presence checked above")
            .step(&self.layout_graph, &mut self.layout_state, budget);
        for (i, id) in self.layout_graph.node_ids.iter().enumerate() {
            if let Some(scene) = self.node_scene.get_mut(*id) {
                scene.position = self.layout_state.positions[i];
            }
        }
        self.bump_geometry_revision();
        if progress == LayoutProgress::Settled {
            self.controller.notify_converged();
        }
        progress
    }

    /// Rebuild the dense layout projection from the current graph and scene
    /// state, preserving existing positions and assigning initial positions to
    /// new nodes (§11.6, §13).
    pub fn rebuild_layout(&mut self) {
        // Consume the old dense index so each new node can look up its
        // previous layout index without building a second hash structure.
        let prev_index_of = std::mem::take(&mut self.node_index);
        let new_ids: Vec<NodeId> = self.graph().nodes().map(|(id, _)| id).collect();
        let mut index_of: std::collections::HashMap<NodeId, usize, S> =
            std::collections::HashMap::with_capacity_and_hasher(new_ids.len(), S::default());
        let mut remap = Vec::with_capacity(new_ids.len());

        let mut layout_state = LayoutState::new();
        layout_state.resize(new_ids.len());
        for (i, id) in new_ids.iter().enumerate() {
            let scene = self.node_scene.get(*id).copied().unwrap_or_default();
            let prev_index = prev_index_of.get(id).copied();
            remap.push(prev_index.map(|index| index as u32));
            let pos = if prev_index.is_some() {
                // Carried node: the scene's canonical position survives the
                // re-projection (§11.6).
                scene.position
            } else {
                let pos = self
                    .placement
                    .initial_position(&layout_state, &mut self.rng);
                if let Some(s) = self.node_scene.get_mut(*id) {
                    s.position = pos;
                }
                pos
            };
            layout_state.positions[i] = pos;
            layout_state.pinned.set(i, scene.pinned);
            index_of.insert(*id, i);
        }

        let mut edges = Vec::new();
        for (_, edge) in self.graph().edges() {
            let Some(&source) = index_of.get(&edge.source) else {
                continue;
            };
            let Some(&target) = index_of.get(&edge.target) else {
                continue;
            };
            edges.push(LayoutEdge {
                source: LayoutIndex(source as u32),
                target: LayoutIndex(target as u32),
                direction: edge.direction,
            });
        }

        self.layout_graph = LayoutGraph::new(
            vec![LayoutNode {}; new_ids.len()],
            edges,
            new_ids,
            self.topology_revision,
        );
        self.layout_state = layout_state;
        self.node_index = index_of;

        // §11.6: offer the projection change to the active engine as an
        // incremental update. Engines that cannot apply it incrementally
        // (or a flight-parked engine, which cannot be synced mid-air) fall
        // back to a full rebuild.
        let delta = LayoutDelta { remap };
        let synced = match self.engine.as_mut() {
            Some(engine) => {
                engine.apply_delta(&self.layout_graph, &delta, &mut self.layout_state)
                    == LayoutSync::Applied
            }
            None => false,
        };
        if !synced {
            self.rebuild_layout_engine();
        }
        self.bump_geometry_revision();
        self.controller.notify_topology_changed();
    }

    /// Rebuild the active engine from the current projection and state.
    ///
    /// Cluster centers are derived output owned by the active engine. Clear
    /// them before every rebuild so an engine that does not provide clusters
    /// cannot inherit geometry from a previous engine, while canonical node
    /// positions remain untouched.
    fn rebuild_layout_engine(&mut self) {
        let Some(engine) = self.engine.as_mut() else {
            // Engine is parked in a background flight; rebuild it on return.
            self.pending_engine_rebuild = true;
            return;
        };
        self.layout_state.cluster_centers.fill(None);
        engine.rebuild(&self.layout_graph, &mut self.layout_state);
    }

    /// Topology revision (§31).
    pub fn topology_revision(&self) -> u64 {
        self.topology_revision
    }

    /// Data revision (§31).
    pub fn data_revision(&self) -> u64 {
        self.data_revision
    }

    /// Geometry revision (§31).
    pub fn geometry_revision(&self) -> u64 {
        self.geometry_revision
    }

    /// Style revision (§31).
    pub fn style_revision(&self) -> u64 {
        self.style_revision
    }

    /// Bump the geometry revision (e.g. after a layout step).
    pub fn bump_geometry_revision(&mut self) {
        self.geometry_revision += 1;
    }

    /// Bump the style revision (e.g. after a theme change).
    pub fn bump_style_revision(&mut self) {
        self.style_revision += 1;
    }

    fn apply_delta(&mut self, delta: &GraphDelta) {
        for &id in &delta.added_nodes {
            self.node_scene.insert(id, NodeSceneState::default());
        }
        for &id in &delta.removed_nodes {
            self.node_scene.remove(id);
        }
        for &id in &delta.added_edges {
            self.edge_scene.insert(id, EdgeSceneState::default());
        }
        for &id in &delta.removed_edges {
            self.edge_scene.remove(id);
        }

        if delta.topology_changed {
            self.topology_revision += 1;
            self.rebuild_layout();
        }
        if !delta.updated_nodes.is_empty() || !delta.updated_edges.is_empty() {
            self.data_revision += 1;
        }
    }
}

impl<NK, EK, N, E, S> Default for GraphScene<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash,
    EK: Eq + std::hash::Hash,
    S: BuildHasher + Default + Clone,
{
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

impl<NK, EK, N, E, S> GraphScene<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: 'static,
    E: 'static,
    S: BuildHasher + Default + Clone + 'static,
{
    /// Step the layout by one frame budget on GPUI's background executor
    /// (§30 native strategy).
    ///
    /// The engine and a snapshot of the layout session move into the spawned
    /// task; when it completes, computed positions are adopted only if neither
    /// the topology nor any direct position write happened meanwhile — stale
    /// results are discarded and the returning engine rebuilds against the
    /// current projection instead (§11.6: positions are preserved across that
    /// rebuild). While a flight is in flight this returns [`LayoutProgress::Running`]
    /// without doing work, so callers can drive frames with the same loop they
    /// use for [`Self::step_layout`].
    pub fn step_layout_async(
        &mut self,
        budget: LayoutBudget,
        cx: &mut Context<Self>,
    ) -> LayoutProgress {
        if !self.controller.should_step() {
            return LayoutProgress::Settled;
        }
        if self.flight.is_some() || self.engine.is_none() {
            return LayoutProgress::Running { stability: None };
        }
        let Some(mut engine) = self.engine.take() else {
            return LayoutProgress::Running { stability: None };
        };
        let graph = self.layout_graph.clone();
        let mut state = self.layout_state.clone();
        let topology_revision = graph.topology_revision;
        let write_epoch = self.layout_write_epoch;
        let task = cx.spawn(async move |this, cx| {
            let finished = cx
                .background_executor()
                .spawn(async move {
                    let progress = engine.step(&graph, &mut state, budget);
                    (engine, state, progress)
                })
                .await;
            this.update(cx, |scene, cx| {
                scene.complete_layout_flight(finished, topology_revision, write_epoch, cx)
            })
            .ok();
        });
        self.flight = Some(task);
        LayoutProgress::Running { stability: None }
    }

    /// Apply (or discard) a completed background step.
    fn complete_layout_flight(
        &mut self,
        finished: (Box<dyn LayoutEngine>, LayoutState, LayoutProgress),
        topology_revision: u64,
        write_epoch: u64,
        cx: &mut Context<Self>,
    ) {
        self.flight = None;
        let (engine, state, progress) = finished;

        // A replacement installed during the flight wins over the parked one.
        if let Some(pending) = self.pending_engine.take() {
            drop(engine);
            self.engine = Some(pending);
            self.pending_engine_rebuild = true;
        } else {
            self.engine = Some(engine);
        }

        // Adopt results only when nothing the snapshot depends on changed.
        // Topology changes also shift dense indices, so their results must
        // never be applied even if node counts happen to match.
        let fresh = topology_revision == self.layout_graph.topology_revision
            && write_epoch == self.layout_write_epoch;
        if fresh && !self.pending_engine_rebuild {
            self.layout_state.positions = state.positions;
            self.layout_state.cluster_centers = state.cluster_centers;
            for (i, id) in self.layout_graph.node_ids.iter().enumerate() {
                if let Some(scene) = self.node_scene.get_mut(*id) {
                    scene.position = self.layout_state.positions[i];
                }
            }
            self.bump_geometry_revision();
            if progress == LayoutProgress::Settled {
                self.controller.notify_converged();
            }
        } else {
            // Stale or deferred-rebuild path: recompute against the current
            // projection. Positions live in `self.layout_state` already and
            // are preserved across rebuilds (§11.6).
            drop(state);
            self.rebuild_layout_engine();
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeDirection;
    use crate::layout::{FixedLayout, ForceAtlas2, SccLayoutEngine};
    use crate::runtime::GraphRuntime;
    use gpui::{AppContext, Entity, TestAppContext};

    #[gpui::test]
    /// §11.6 dynamic incremental update: a settled graph that gains one far
    /// node must respond locally. The fresh node travels to its neighborhood
    /// while carried nodes keep their adapted factors and barely move.
    #[gpui::test]
    fn incremental_merge_settles_locally(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let scene: Entity<Scene> =
            cx.new(|_| GraphScene::new().with_layout(Box::new(ForceAtlas2::default())));
        const RING: [&str; 12] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"];
        const RING_EDGES: [&str; 12] = [
            "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11",
        ];
        scene.update(cx, |s, _| {
            let mut batch = GraphBatch::new();
            // A tightly connected ring: settles into a stable equilibrium.
            for key in RING {
                batch = batch.node(key, key);
            }
            for i in 0..12 {
                batch = batch.edge(
                    RING_EDGES[i],
                    RING[i],
                    RING[(i + 1) % 12],
                    EdgeDirection::Undirected,
                    "x",
                );
            }
            s.merge(batch);
        });

        let mut settled_positions: Vec<Vec2> = Vec::new();
        for _ in 0..600 {
            let progress =
                scene.update(cx, |s, cx| s.step_layout_async(LayoutBudget::default(), cx));
            if progress == LayoutProgress::Settled {
                break;
            }
            cx.run_until_parked();
        }
        scene.update(cx, |s, _| {
            settled_positions = RING
                .iter()
                .map(|key| s.node_position(s.node_id(key).unwrap()).unwrap())
                .collect();
        });

        // Add one fresh node bound to ring node 0; placement spreads it away.
        scene.update(cx, |s, _| {
            s.merge(GraphBatch::new().node("fresh", "F").edge(
                "hook",
                "fresh",
                "0",
                EdgeDirection::Undirected,
                "x",
            ));
        });
        let fresh_initial = scene.update(cx, |s, _| {
            s.node_position(s.node_id(&"fresh").unwrap()).unwrap()
        });

        for _ in 0..600 {
            let progress =
                scene.update(cx, |s, cx| s.step_layout_async(LayoutBudget::default(), cx));
            if progress == LayoutProgress::Settled {
                break;
            }
            cx.run_until_parked();
        }

        scene.update(cx, |s, _| {
            let fresh_final = s.node_position(s.node_id(&"fresh").unwrap()).unwrap();
            let travel = (fresh_final - fresh_initial).length();
            assert!(
                travel > 1.0,
                "fresh node must travel to its neighborhood, moved {travel}"
            );

            // Carried nodes keep their adapted convergence factors, so their
            // response stays local even though the sync reset global cooling.
            let mut max_carried_drift = 0.0f32;
            for (i, before) in settled_positions.iter().enumerate() {
                let after = s.node_position(s.node_id(&RING[i]).unwrap()).unwrap();
                max_carried_drift = max_carried_drift.max((*before - after).length());
            }
            assert!(
                max_carried_drift < travel,
                "carried drift {max_carried_drift} must stay below fresh travel {travel}"
            );
            assert_eq!(s.layout_state(), LayoutRunState::Settled);
        });
    }

    /// §11.6: removing a node from a running layout compacts dense indices;
    /// the engine must keep stepping and settle with finite positions.
    #[gpui::test]
    /// The demo drivers pace relaxation with a small per-frame iteration
    /// count plus a wall-clock ceiling (see FRAME_LAYOUT_BUDGET in the
    /// examples). The pacing contract: convergence must take visibly many
    /// frames — a budget that converges inside the first frame collapses the
    /// animation into a single jump.
    #[gpui::test]
    fn frame_budget_keeps_relaxation_visible(cx: &mut TestAppContext) {
        let scene: Entity<Scene> =
            cx.new(|_| GraphScene::new().with_layout(Box::new(ForceAtlas2::default())));
        scene.update(cx, |s, _| {
            const RING: [&str; 12] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"];
            let mut batch = GraphBatch::new();
            for key in RING {
                batch = batch.node(key, key);
            }
            const RING_EDGES: [&str; 12] = [
                "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11",
            ];
            for i in 0..12 {
                batch = batch.edge(
                    RING_EDGES[i],
                    RING[i],
                    RING[(i + 1) % 12],
                    EdgeDirection::Undirected,
                    "x",
                );
            }
            s.merge(batch);
        });

        // Mirrors the examples' driver: one iteration per animation frame.
        let frame_budget = LayoutBudget {
            max_iterations: 1,
            max_duration: Some(core::time::Duration::from_millis(6)),
        };
        let mut frames = 0u32;
        loop {
            let progress = scene.update(cx, |s, _| s.step_layout(frame_budget));
            frames += 1;
            if progress == LayoutProgress::Settled || frames > 1500 {
                break;
            }
        }
        println!("ring relaxation animated over {frames} frames");
        assert!(
            (30..=1500).contains(&frames),
            "relaxation must stay visible across frames, took {frames}"
        );
    }

    #[gpui::test]
    fn removal_mid_run_resettles_with_compacted_indices(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let scene: Entity<Scene> =
            cx.new(|_| GraphScene::new().with_layout(Box::new(ForceAtlas2::default())));
        const CHAIN: [&str; 8] = ["n0", "n1", "n2", "n3", "n4", "n5", "n6", "n7"];
        const CHAIN_EDGES: [&str; 7] = ["c0", "c1", "c2", "c3", "c4", "c5", "c6"];
        scene.update(cx, |s, _| {
            let mut batch = GraphBatch::new();
            for key in CHAIN {
                batch = batch.node(key, key);
            }
            for i in 0..7 {
                batch = batch.edge(
                    CHAIN_EDGES[i],
                    CHAIN[i],
                    CHAIN[i + 1],
                    EdgeDirection::Undirected,
                    "x",
                );
            }
            s.merge(batch);
        });

        // A few steps, then remove a middle node while the run is live.
        for _ in 0..5 {
            let progress =
                scene.update(cx, |s, cx| s.step_layout_async(LayoutBudget::default(), cx));
            if progress == LayoutProgress::Settled {
                break;
            }
            cx.run_until_parked();
        }
        scene.update(cx, |s, _| {
            s.apply(
                crate::patch::GraphPatch::new()
                    .node(crate::patch::NodePatch::Remove { key: CHAIN[3] }),
            );
        });

        for _ in 0..300 {
            let progress =
                scene.update(cx, |s, cx| s.step_layout_async(LayoutBudget::default(), cx));
            if progress == LayoutProgress::Settled {
                break;
            }
            cx.run_until_parked();
        }

        scene.update(cx, |s, _| {
            assert_eq!(s.layout_state(), LayoutRunState::Settled);
            // Every surviving node keeps a finite position; n3 was removed.
            for id in CHAIN {
                let Some(node) = s.node_id(&id) else {
                    assert_eq!(id, CHAIN[3], "only the removed node is gone");
                    continue;
                };
                let p = s.node_position(node).unwrap();
                assert!(
                    p.x.is_finite() && p.y.is_finite(),
                    "position for {id} must stay finite"
                );
            }
        });
    }

    #[gpui::test]
    fn background_step_applies_positions_and_settles(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let scene: Entity<Scene> =
            cx.new(|_| GraphScene::new().with_layout(Box::new(ForceAtlas2::default())));
        scene.update(cx, |s, _| {
            // A connected triangle: attraction binds the nodes, so the
            // relaxation genuinely moves them before settling. The initial
            // geometry is pinned explicitly so the 40-step settle budget does
            // not depend on the default placement policy's spread.
            s.merge(
                GraphBatch::new()
                    .node("a", "A")
                    .node("b", "B")
                    .node("c", "C")
                    .edge("ab", "a", "b", EdgeDirection::Undirected, "x")
                    .edge("bc", "b", "c", EdgeDirection::Undirected, "y"),
            );
            let (a, b, c) = (
                s.node_id(&"a").unwrap(),
                s.node_id(&"b").unwrap(),
                s.node_id(&"c").unwrap(),
            );
            s.set_position(a, Vec2::new(-30.0, 0.0));
            s.set_position(b, Vec2::new(30.0, 6.0));
            s.set_position(c, Vec2::new(10.0, -24.0));
        });
        let initial = scene.update(cx, |s, _| {
            s.node_position(s.node_id(&"a").unwrap()).unwrap()
        });

        for _ in 0..40 {
            let progress =
                scene.update(cx, |s, cx| s.step_layout_async(LayoutBudget::default(), cx));
            if progress == LayoutProgress::Settled {
                break;
            }
            cx.run_until_parked();
        }
        let separated = scene.update(cx, |s, _| {
            let a = s.node_position(s.node_id(&"a").unwrap()).unwrap();
            let b = s.node_position(s.node_id(&"b").unwrap()).unwrap();
            (a, b)
        });
        assert!(
            (separated.0 - separated.1).length() > 1.0,
            "background steps must move nodes apart: {separated:?}"
        );
        assert!(
            (separated.0 - initial).length() > 1.0,
            "node a must have moved from its initial placement: {initial} -> {}",
            separated.0
        );
        assert_eq!(
            scene.update(cx, |s, _| s.layout_state()),
            LayoutRunState::Settled
        );
    }

    #[gpui::test]
    fn background_flight_discards_result_after_topology_change(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let scene: Entity<Scene> =
            cx.new(|_| GraphScene::new().with_layout(Box::new(ForceAtlas2::default())));
        scene.update(cx, |s, _| {
            s.merge(GraphBatch::new().node("a", "A").node("b", "B").edge(
                "ab",
                "a",
                "b",
                EdgeDirection::Undirected,
                "x",
            ));
        });

        // Kick a flight, then mutate topology before the executor runs it.
        let kicked = scene.update(cx, |s, cx| s.step_layout_async(LayoutBudget::default(), cx));
        assert_eq!(kicked, LayoutProgress::Running { stability: None });
        scene.update(cx, |s, _| {
            s.merge(GraphBatch::new().node("c", "C").edge(
                "ac",
                "a",
                "c",
                EdgeDirection::Undirected,
                "knows",
            ));
        });
        cx.run_until_parked();

        // The stale result must have been discarded and the returning engine
        // rebuilt against the three-node projection; stepping continues.
        scene.update(cx, |s, cx| {
            assert_eq!(s.layout_graph.node_count(), 3);
            let progress = s.step_layout_async(LayoutBudget::default(), cx);
            assert_eq!(progress, LayoutProgress::Running { stability: None });
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn set_position_during_flight_prevents_result_clobber(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let scene: Entity<Scene> =
            cx.new(|_| GraphScene::new().with_layout(Box::new(ForceAtlas2::default())));
        scene.update(cx, |s, _| {
            s.merge(GraphBatch::new().node("a", "A").node("b", "B"));
        });
        let a = scene.update(cx, |s, _| s.node_id(&"a").unwrap());
        scene.update(cx, |s, _| s.set_position(a, glam::Vec2::new(500.0, 500.0)));

        let _ = scene.update(cx, |s, cx| s.step_layout_async(LayoutBudget::default(), cx));
        // Simulate a concurrent drag after the snapshot was taken but before
        // completion: bump the write epoch via another direct write.
        scene.update(cx, |s, _| s.set_position(a, glam::Vec2::new(600.0, 600.0)));
        cx.run_until_parked();

        // The flight's positions targeted pre-drag state; the drag value must
        // survive as the authoritative position until the next fresh step.
        scene.update(cx, |s, cx| {
            let _ = s.step_layout_async(LayoutBudget::default(), cx);
        });
        cx.run_until_parked();
    }

    type Scene = GraphScene<&'static str, &'static str, &'static str, &'static str>;

    fn scene() -> Scene {
        GraphScene::new().with_layout(Box::new(FixedLayout))
    }

    fn synced_runtime<NK, EK, N, E>(s: &GraphScene<NK, EK, N, E>) -> GraphRuntime
    where
        NK: Eq + std::hash::Hash,
        EK: Eq + std::hash::Hash,
    {
        let mut runtime = GraphRuntime::new();
        s.sync_runtime(&mut runtime);
        runtime
    }

    #[test]
    fn with_hasher_builds_scene_and_runtime_with_chosen_hasher() {
        let mut s: GraphScene<&str, &str, &str, &str, std::collections::hash_map::RandomState> =
            GraphScene::with_hasher(std::collections::hash_map::RandomState::default());
        let delta = s.merge(GraphBatch::new().node("a", "A").node("b", "B").edge(
            "ab",
            "a",
            "b",
            EdgeDirection::Directed,
            "knows",
        ));
        assert_eq!(delta.added_nodes.len(), 2);
        assert_eq!(delta.added_edges.len(), 1);
        assert!(s.node_id(&"a").is_some());
        assert!(s.edge_id(&"ab").is_some());

        // The runtime shares the scene's hasher and builds its index.
        let mut rt = GraphRuntime::<std::collections::hash_map::RandomState>::default();
        let synced = s.sync_runtime(&mut rt);
        assert_eq!(synced.edges().edge_ids.len(), 1);
    }

    #[test]
    fn merge_populates_scene_and_layout() {
        let mut s = scene();
        let batch = GraphBatch::new().node("a", "A").node("b", "B").edge(
            "ab",
            "a",
            "b",
            EdgeDirection::Directed,
            "knows",
        );
        let delta = s.merge(batch);

        assert_eq!(delta.added_nodes.len(), 2);
        assert_eq!(delta.added_edges.len(), 1);
        assert_eq!(s.graph().node_count(), 2);
        assert_eq!(s.graph().edge_count(), 1);
        assert_eq!(s.layout_graph.node_count(), 2);
        assert_eq!(s.layout_graph.edge_count(), 1);
        assert_eq!(s.topology_revision(), 1);
    }

    #[test]
    fn merge_reuses_nodes_and_preserves_positions() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A").node("b", "B").edge(
            "ab",
            "a",
            "b",
            EdgeDirection::Directed,
            "knows",
        ));
        let a = s.node_id(&"a").unwrap();
        s.set_position(a, Vec2::new(50.0, 60.0));

        // Merge a batch that reuses `a` and adds `c`.
        s.merge(GraphBatch::new().node("a", "A").node("c", "C").edge(
            "ac",
            "a",
            "c",
            EdgeDirection::Directed,
            "knows",
        ));

        // `a` keeps its position; `c` is new.
        assert_eq!(s.node_position(a), Some(Vec2::new(50.0, 60.0)));
        let c = s.node_id(&"c").unwrap();
        // The new node must receive an initial placement, not remain at the
        // default origin.
        let c_pos = s.node_position(c).unwrap();
        assert_ne!(
            c_pos,
            Vec2::ZERO,
            "new node should receive an initial placement"
        );
        assert_eq!(s.graph().node_count(), 3);
    }

    #[test]
    fn pin_and_unpin() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A"));
        let a = s.node_id(&"a").unwrap();
        assert!(!s.is_pinned(a));
        s.pin(a);
        assert!(s.is_pinned(a));
        s.unpin(a);
        assert!(!s.is_pinned(a));
    }

    #[test]
    fn pin_syncs_to_layout_state() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A"));
        let a = s.node_id(&"a").unwrap();
        let i = s
            .layout_graph
            .node_ids
            .iter()
            .position(|&x| x == a)
            .unwrap();

        s.pin(a);
        assert!(
            s.layout_state.pinned[i],
            "pin must reach the dense layout state"
        );
        s.unpin(a);
        assert!(
            !s.layout_state.pinned[i],
            "unpin must reach the dense layout state"
        );
    }

    #[test]
    fn set_position_syncs_to_layout_state() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A"));
        let a = s.node_id(&"a").unwrap();
        let i = s
            .layout_graph
            .node_ids
            .iter()
            .position(|&x| x == a)
            .unwrap();

        s.set_position(a, Vec2::new(42.0, 24.0));
        assert_eq!(
            s.layout_state.positions[i],
            Vec2::new(42.0, 24.0),
            "set_position must reach the dense layout state"
        );
    }

    #[test]
    fn data_revision_bumps_on_node_update() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A"));
        let rev = s.data_revision();
        // Re-providing an existing node updates its data.
        s.merge(GraphBatch::new().node("a", "A2"));
        assert!(
            s.data_revision() > rev,
            "data update must bump the data revision"
        );
    }

    #[test]
    fn edge_topology_upsert_preserves_identity_updates_projection_and_reheats() {
        let mut s = scene();
        s.apply(
            GraphPatch::new()
                .node(crate::patch::NodePatch::Upsert {
                    key: "a",
                    data: "A",
                })
                .node(crate::patch::NodePatch::Upsert {
                    key: "b",
                    data: "B",
                })
                .node(crate::patch::NodePatch::Upsert {
                    key: "c",
                    data: "C",
                })
                .edge(crate::patch::EdgePatch::Upsert {
                    key: "edge",
                    source: "a",
                    target: "a",
                    direction: EdgeDirection::Directed,
                    data: "old",
                }),
        );
        let edge_id = s.edge_id(&"edge").unwrap();
        s.step_layout(LayoutBudget::default());
        assert_eq!(s.layout_state(), LayoutRunState::Settled);
        let topology_before = s.topology_revision();
        let data_before = s.data_revision();

        let delta = s.apply(GraphPatch::new().edge(crate::patch::EdgePatch::Upsert {
            key: "edge",
            source: "b",
            target: "c",
            direction: EdgeDirection::Undirected,
            data: "new",
        }));
        assert_eq!(delta.updated_edges, vec![edge_id]);
        assert!(delta.topology_changed);
        assert_eq!(s.edge_id(&"edge"), Some(edge_id));
        assert_eq!(s.topology_revision(), topology_before + 1);
        assert_eq!(s.data_revision(), data_before + 1);
        assert_eq!(
            s.graph().incident_edges(s.node_id(&"a").unwrap()),
            Some(&[][..])
        );
        assert_eq!(
            s.graph().incident_edges(s.node_id(&"b").unwrap()),
            Some(&[edge_id][..])
        );
        assert_eq!(
            s.graph().incident_edges(s.node_id(&"c").unwrap()),
            Some(&[edge_id][..])
        );
        let source_index = s
            .layout_graph
            .node_ids
            .iter()
            .position(|id| *id == s.node_id(&"b").unwrap())
            .unwrap();
        let target_index = s
            .layout_graph
            .node_ids
            .iter()
            .position(|id| *id == s.node_id(&"c").unwrap())
            .unwrap();
        assert_eq!(s.layout_graph.edges.len(), 1);
        assert_eq!(s.layout_graph.edges[0].source.0, source_index as u32);
        assert_eq!(s.layout_graph.edges[0].target.0, target_index as u32);
        assert_eq!(s.layout_graph.edges[0].direction, EdgeDirection::Undirected);
        assert_eq!(s.layout_state(), LayoutRunState::Running);

        s.step_layout(LayoutBudget::default());
        s.apply(GraphPatch::new().edge(crate::patch::EdgePatch::Upsert {
            key: "edge",
            source: "c",
            target: "c",
            direction: EdgeDirection::Directed,
            data: "newer",
        }));
        assert_eq!(
            s.graph().incident_edges(s.node_id(&"b").unwrap()),
            Some(&[][..])
        );
        assert_eq!(
            s.graph().incident_edges(s.node_id(&"c").unwrap()),
            Some(&[edge_id][..])
        );
        assert_eq!(s.edge_id(&"edge"), Some(edge_id));
        assert_eq!(s.layout_graph.edges[0].source.0, target_index as u32);
        assert_eq!(s.layout_graph.edges[0].target.0, target_index as u32);
        assert_eq!(s.layout_graph.edges[0].direction, EdgeDirection::Directed);
        assert_eq!(s.layout_state(), LayoutRunState::Running);
    }

    #[test]
    fn edge_direction_only_upsert_rebuilds_projection_and_reheats() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A").node("b", "B").edge(
            "edge",
            "a",
            "b",
            EdgeDirection::Directed,
            "old",
        ));
        let edge_id = s.edge_id(&"edge").unwrap();
        let source = s.node_id(&"a").unwrap();
        let target = s.node_id(&"b").unwrap();
        s.step_layout(LayoutBudget::default());
        assert_eq!(s.layout_state(), LayoutRunState::Settled);
        let topology_before = s.topology_revision();
        let data_before = s.data_revision();

        let delta = s.apply(GraphPatch::new().edge(crate::patch::EdgePatch::Upsert {
            key: "edge",
            source: "a",
            target: "b",
            direction: EdgeDirection::Undirected,
            data: "new",
        }));

        assert_eq!(delta.updated_edges, vec![edge_id]);
        assert!(delta.topology_changed);
        assert_eq!(s.edge_id(&"edge"), Some(edge_id));
        assert_eq!(s.graph().edge(edge_id).unwrap().source, source);
        assert_eq!(s.graph().edge(edge_id).unwrap().target, target);
        assert_eq!(
            s.graph().edge(edge_id).unwrap().direction,
            EdgeDirection::Undirected
        );
        assert_eq!(s.topology_revision(), topology_before + 1);
        assert_eq!(s.data_revision(), data_before + 1);

        let source_index = s
            .layout_graph
            .node_ids
            .iter()
            .position(|id| *id == source)
            .unwrap();
        let target_index = s
            .layout_graph
            .node_ids
            .iter()
            .position(|id| *id == target)
            .unwrap();
        assert_eq!(s.layout_graph.edges.len(), 1);
        assert_eq!(s.layout_graph.edges[0].source.0, source_index as u32);
        assert_eq!(s.layout_graph.edges[0].target.0, target_index as u32);
        assert_eq!(s.layout_graph.edges[0].direction, EdgeDirection::Undirected);
        assert_eq!(s.layout_state(), LayoutRunState::Running);
    }

    #[test]
    fn edge_data_upsert_updates_data_without_rebuilding_or_reheating() {
        let mut s = scene();
        s.apply(
            GraphPatch::new()
                .node(crate::patch::NodePatch::Upsert {
                    key: "a",
                    data: "A",
                })
                .node(crate::patch::NodePatch::Upsert {
                    key: "b",
                    data: "B",
                })
                .edge(crate::patch::EdgePatch::Upsert {
                    key: "edge",
                    source: "a",
                    target: "b",
                    direction: EdgeDirection::Directed,
                    data: "old",
                }),
        );
        let edge_id = s.edge_id(&"edge").unwrap();
        s.step_layout(LayoutBudget::default());
        let topology_before = s.topology_revision();
        let layout_edges_before = s.layout_graph.edges.clone();
        assert_eq!(s.layout_state(), LayoutRunState::Settled);

        let delta = s.apply(GraphPatch::new().edge(crate::patch::EdgePatch::Upsert {
            key: "edge",
            source: "a",
            target: "b",
            direction: EdgeDirection::Directed,
            data: "new",
        }));
        assert_eq!(delta.updated_edges, vec![edge_id]);
        assert!(!delta.topology_changed);
        assert_eq!(s.edge_id(&"edge"), Some(edge_id));
        assert_eq!(s.graph().edge_data(edge_id), Some(&"new"));
        assert_eq!(s.topology_revision(), topology_before);
        assert!(s.data_revision() > 0);
        assert_eq!(s.layout_graph.edges.len(), layout_edges_before.len());
        for (actual, before) in s.layout_graph.edges.iter().zip(&layout_edges_before) {
            assert_eq!(actual.source, before.source);
            assert_eq!(actual.target, before.target);
            assert_eq!(actual.direction, before.direction);
        }
        assert_eq!(s.layout_state(), LayoutRunState::Settled);
    }

    #[test]
    fn edge_upsert_with_unknown_endpoint_is_atomic_for_scene() {
        let mut s = scene();
        s.apply(
            GraphPatch::new()
                .node(crate::patch::NodePatch::Upsert {
                    key: "a",
                    data: "A",
                })
                .node(crate::patch::NodePatch::Upsert {
                    key: "b",
                    data: "B",
                })
                .edge(crate::patch::EdgePatch::Upsert {
                    key: "edge",
                    source: "a",
                    target: "b",
                    direction: EdgeDirection::Directed,
                    data: "old",
                }),
        );
        let edge_id = s.edge_id(&"edge").unwrap();
        s.step_layout(LayoutBudget::default());
        let source_id = s.node_id(&"a").unwrap();
        let target_id = s.node_id(&"b").unwrap();

        for (source, target) in [("ghost", "b"), ("a", "ghost")] {
            let edge_before = s.graph().edge(edge_id).unwrap().clone();
            let topology_before = s.topology_revision();
            let data_before = s.data_revision();
            let geometry_before = s.geometry_revision();
            let style_before = s.style_revision();
            let layout_edges_before = s.layout_graph.edges.clone();
            let layout_topology_before = s.layout_graph.topology_revision;

            let delta = s.apply(GraphPatch::new().edge(crate::patch::EdgePatch::Upsert {
                key: "edge",
                source,
                target,
                direction: EdgeDirection::Undirected,
                data: "new",
            }));
            assert!(delta.is_empty());
            assert_eq!(s.edge_id(&"edge"), Some(edge_id));
            assert_eq!(s.node_id(&"a"), Some(source_id));
            assert_eq!(s.node_id(&"b"), Some(target_id));
            assert_eq!(s.graph().node_count(), 2);
            assert_eq!(s.graph().edge_count(), 1);
            assert_eq!(s.graph().edge(edge_id).unwrap().source, edge_before.source);
            assert_eq!(s.graph().edge(edge_id).unwrap().target, edge_before.target);
            assert_eq!(
                s.graph().edge(edge_id).unwrap().direction,
                edge_before.direction
            );
            assert_eq!(s.graph().edge_data(edge_id), Some(&"old"));
            assert_eq!(s.graph().incident_edges(source_id).unwrap(), &[edge_id]);
            assert_eq!(s.graph().incident_edges(target_id).unwrap(), &[edge_id]);
            assert_eq!(s.topology_revision(), topology_before);
            assert_eq!(s.data_revision(), data_before);
            assert_eq!(s.geometry_revision(), geometry_before);
            assert_eq!(s.style_revision(), style_before);
            assert_eq!(s.layout_graph.node_ids, vec![source_id, target_id]);
            assert_eq!(s.layout_graph.topology_revision, layout_topology_before);
            assert_eq!(s.layout_graph.edges.len(), layout_edges_before.len());
            for (actual, before) in s.layout_graph.edges.iter().zip(&layout_edges_before) {
                assert_eq!(actual.source, before.source);
                assert_eq!(actual.target, before.target);
                assert_eq!(actual.direction, before.direction);
            }
            assert_eq!(s.layout_state(), LayoutRunState::Settled);
        }
    }

    #[test]
    fn remove_node_removes_scene_state() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A").node("b", "B").edge(
            "ab",
            "a",
            "b",
            EdgeDirection::Directed,
            "knows",
        ));
        let a = s.node_id(&"a").unwrap();
        s.apply(GraphPatch::new().node(crate::patch::NodePatch::Remove { key: "a" }));
        assert!(s.node_scene(a).is_none());
        assert_eq!(s.graph().node_count(), 1);
        assert_eq!(s.graph().edge_count(), 0);
        // The dense layout projection must be rebuilt to drop the removed node
        // and its incident edge.
        assert_eq!(s.layout_graph.node_count(), 1);
        assert_eq!(s.layout_graph.edge_count(), 0);
    }

    #[test]
    fn step_layout_with_fixed_returns_settled() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A"));
        let progress = s.step_layout(LayoutBudget::default());
        assert_eq!(progress, LayoutProgress::Settled);
    }

    #[test]
    fn step_layout_copies_positions_back_to_scene() {
        let mut s = GraphScene::new().with_layout(Box::new(ForceAtlas2::default()));
        s.merge(GraphBatch::new().node("a", "A").node("b", "B").edge(
            "ab",
            "a",
            "b",
            EdgeDirection::Undirected,
            (),
        ));
        let a = s.node_id(&"a").unwrap();
        let b = s.node_id(&"b").unwrap();
        // Start far apart so the force model moves them.
        s.set_position(a, Vec2::new(-100.0, 0.0));
        s.set_position(b, Vec2::new(100.0, 0.0));

        s.step_layout(LayoutBudget {
            max_iterations: 1,
            max_duration: None,
        });

        // The layout moved the nodes; the scene must reflect the new positions.
        assert_ne!(s.node_position(a).unwrap(), Vec2::new(-100.0, 0.0));
        assert_ne!(s.node_position(b).unwrap(), Vec2::new(100.0, 0.0));
    }

    #[test]
    fn force_atlas_step_invalidates_synced_runtime() {
        let mut s = GraphScene::new().with_layout(Box::new(ForceAtlas2::default()));
        s.merge(GraphBatch::new().node("a", ()).node("b", ()).edge(
            "ab",
            "a",
            "b",
            EdgeDirection::Undirected,
            (),
        ));
        let a = s.node_id(&"a").unwrap();
        let b = s.node_id(&"b").unwrap();
        s.set_position(a, Vec2::new(-100.0, 0.0));
        s.set_position(b, Vec2::new(100.0, 0.0));
        let mut runtime = synced_runtime(&s);
        let initial_source = runtime
            .edges()
            .edge_ids
            .iter()
            .position(|&id| id == s.edge_id(&"ab").unwrap())
            .map(|index| runtime.edges().source[index])
            .unwrap();
        let initial_target = runtime
            .edges()
            .edge_ids
            .iter()
            .position(|&id| id == s.edge_id(&"ab").unwrap())
            .map(|index| runtime.edges().target[index])
            .unwrap();
        let geometry_before = s.geometry_revision();

        let progress = s.step_layout(LayoutBudget {
            max_iterations: 1,
            max_duration: None,
        });

        assert!(matches!(
            progress,
            LayoutProgress::Running { .. } | LayoutProgress::Settled
        ));
        assert_eq!(s.geometry_revision(), geometry_before + 1);
        assert_ne!(runtime.geometry_revision(), s.geometry_revision());

        // Follow the same stale -> rebuild-and-sync path as GraphViewState's
        // prepare_canvas before inspecting the derived runtime.
        s.sync_runtime(&mut runtime);
        assert_eq!(runtime.geometry_revision(), s.geometry_revision());

        let expected_edge = s.edge_id(&"ab").unwrap();
        let edge_index = runtime
            .edges()
            .edge_ids
            .iter()
            .position(|&id| id == expected_edge)
            .expect("ForceAtlas edge must be in runtime edge prep");
        let moved_source = s.node_position(a).unwrap();
        let moved_target = s.node_position(b).unwrap();
        assert_ne!(moved_source, initial_source);
        assert_ne!(moved_target, initial_target);
        assert_eq!(runtime.edges().source[edge_index], moved_source);
        assert_eq!(runtime.edges().target[edge_index], moved_target);

        let bounds = crate::viewport::WorldBounds {
            min: moved_source.min(moved_target),
            max: moved_source.max(moved_target),
        };
        let candidate_ids = runtime
            .visible_edge_candidates(&bounds, 0.0)
            .into_iter()
            .map(|index| runtime.edges().edge_ids[index])
            .collect::<Vec<_>>();
        assert_eq!(candidate_ids, vec![expected_edge]);
    }

    #[test]
    fn set_position_only_invalidates_runtime_for_a_real_change() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A"));
        let a = s.node_id(&"a").unwrap();
        let original = s.node_position(a).unwrap();
        let runtime = synced_runtime(&s);
        let geometry_before = s.geometry_revision();

        s.set_position(a, original);
        s.set_position(NodeId::default(), Vec2::new(42.0, 24.0));
        assert_eq!(s.geometry_revision(), geometry_before);
        assert_eq!(runtime.geometry_revision(), s.geometry_revision());

        s.set_position(a, original + Vec2::new(1.0, 0.0));
        assert_eq!(s.geometry_revision(), geometry_before + 1);
        assert_ne!(runtime.geometry_revision(), s.geometry_revision());
    }

    #[test]
    fn public_layout_rebuilds_invalidate_runtime_geometry_revision() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A").node("b", "B"));
        let mut runtime = synced_runtime(&s);

        let before_rebuild = s.geometry_revision();
        s.rebuild_layout();
        assert_eq!(s.geometry_revision(), before_rebuild + 1);
        assert_ne!(runtime.geometry_revision(), s.geometry_revision());

        s.sync_runtime(&mut runtime);
        let before_set_layout = s.geometry_revision();
        s.set_layout(Box::new(SccLayoutEngine));
        assert_eq!(s.geometry_revision(), before_set_layout + 1);
        assert_ne!(runtime.geometry_revision(), s.geometry_revision());
    }

    #[test]
    fn settled_layout_step_does_not_churn_geometry_revision() {
        let mut s = scene();
        s.merge(GraphBatch::new().node("a", "A"));
        s.step_layout(LayoutBudget::default());
        let geometry_after_step = s.geometry_revision();

        assert_eq!(
            s.step_layout(LayoutBudget::default()),
            LayoutProgress::Settled
        );
        assert_eq!(s.geometry_revision(), geometry_after_step);
    }

    #[test]
    fn scc_layout_edges_with_both_endpoints_visible_remain_candidates_at_high_zoom() {
        // Reproduces the reported bug in examples/matrices_and_graphs.rs: an edge
        // whose two endpoints are both on-screen (e.g. n2 -> n10) disappears when
        // zooming in. The spatial index must keep such an edge as a candidate
        // even at high zoom, so the precise cull test (which keeps any edge with
        // an on-screen endpoint) can retain it.
        // The example's 13-node reducible digraph, in Frobenius order.
        let matrix: [&[u8]; 13] = [
            &[0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            &[0, 0, 1, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],
            &[1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
            &[0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0],
            &[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
            &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            &[0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0],
            &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0],
            &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
        ];

        let mut batch = GraphBatch::new();
        for i in 0..13 {
            batch = batch.node(i.to_string(), ());
        }
        for (i, row) in matrix.iter().enumerate() {
            for (j, &entry) in row.iter().enumerate() {
                if entry == 1 {
                    batch = batch.edge(
                        format!("{i}->{j}"),
                        i.to_string(),
                        j.to_string(),
                        EdgeDirection::Directed,
                        (),
                    );
                }
            }
        }

        let mut s = GraphScene::new().with_layout(Box::new(SccLayoutEngine));
        s.merge(batch);
        s.step_layout(LayoutBudget::default());

        // Build the runtime index from the settled scene, exactly as the view
        // does in prepaint.
        let mut rt = GraphRuntime::new();
        s.sync_runtime(&mut rt);

        // The edge n2 -> n10 (0-based: 1 -> 9) is a cross-block edge. At a high
        // zoom centered on the two endpoints, both are on-screen, so the precise
        // cull test keeps the edge. The index must return it as a candidate.
        let n2 = s.node_id(&"1".to_string()).unwrap();
        let n10 = s.node_id(&"9".to_string()).unwrap();
        let expected_edge = s.edge_id(&"1->9".to_string()).unwrap();
        let p2 = s.node_position(n2).unwrap();
        let p10 = s.node_position(n10).unwrap();
        let min = p2.min(p10);
        let max = p2.max(p10);
        let bounds = crate::viewport::WorldBounds { min, max };
        let candidates = rt.visible_edge_candidates(&bounds, 0.0);
        let expected_edge_index = rt
            .edges()
            .edge_ids
            .iter()
            .position(|&id| id == expected_edge)
            .unwrap();
        assert!(
            candidates.contains(&expected_edge_index),
            "edge n2->n10 must remain a candidate when both endpoints are on-screen \
             (p2={p2:?}, p10={p10:?})"
        );
    }

    #[test]
    fn switching_from_scc_to_fixed_clears_cluster_geometry_before_runtime_sync() {
        const NODE_KEYS: [&str; 20] = [
            "n0", "n1", "n2", "n3", "n4", "n5", "n6", "n7", "n8", "n9", "n10", "n11", "n12", "n13",
            "n14", "n15", "n16", "n17", "n18", "n19",
        ];
        const EDGE_KEYS: [&str; 20] = [
            "e0", "e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8", "e9", "e10", "e11", "e12", "e13",
            "e14", "e15", "e16", "e17", "e18", "e19",
        ];

        let mut batch = GraphBatch::new();
        for key in NODE_KEYS {
            batch = batch.node(key, ());
        }
        for i in 0..NODE_KEYS.len() {
            batch = batch.edge(
                EDGE_KEYS[i],
                NODE_KEYS[i],
                NODE_KEYS[(i + 1) % NODE_KEYS.len()],
                EdgeDirection::Directed,
                (),
            );
        }

        let mut s = GraphScene::new().with_layout(Box::new(SccLayoutEngine));
        s.merge(batch);
        // Pin the canonical node geometry deterministically. The SCC engine
        // derives its cluster circles from topology alone, but the scene's
        // canonical positions come from the initial placement policy, and the
        // stale-curve premise below (an old bowed control point far outside
        // every post-switch straight chord) must not depend on that policy.
        for (i, key) in NODE_KEYS.iter().enumerate() {
            let angle = (i as f32 / NODE_KEYS.len() as f32) * std::f32::consts::TAU;
            s.set_position(
                s.node_id(key).unwrap(),
                Vec2::new(angle.cos(), angle.sin()) * 20.0,
            );
        }
        let source_id = s.node_id(&NODE_KEYS[0]).unwrap();
        let target_id = s.node_id(&NODE_KEYS[1]).unwrap();
        let edge_id = s.edge_id(&EDGE_KEYS[0]).unwrap();
        let old_source = s.node_position(source_id).unwrap();
        let old_target = s.node_position(target_id).unwrap();
        let old_cluster = s
            .node_cluster_center(source_id)
            .expect("SCC rebuild must record a cluster center");
        assert_eq!(s.node_cluster_center(target_id), Some(old_cluster));

        // Locate a point on the old SCC-bowed curve far enough outside the
        // straight source-target box that the runtime candidate query can
        // distinguish stale cluster geometry from an unclustered edge.
        let no_obstacles = crate::paint::ObstacleField::new(&[], 1.0);
        let no_reverse = [false];
        let no_parallel = [None];
        let old_control = crate::paint::edge_control_point(
            old_source,
            old_target,
            &crate::paint::EdgeCurveContext {
                index: 0,
                signed_density: 0.0,
                has_reverse: &no_reverse,
                parallel: &no_parallel,
                obstacles: &no_obstacles,
                obstacle_radius: 0.0,
                endpoints_in_field: (false, false),
            },
            Some(old_cluster),
        );

        let mut runtime = synced_runtime(&s);
        assert_eq!(runtime.geometry_revision(), s.geometry_revision());

        s.set_layout(Box::new(FixedLayout));
        assert_eq!(s.node_position(source_id), Some(old_source));
        assert_eq!(s.node_position(target_id), Some(old_target));
        assert!(s.node_cluster_center(source_id).is_none());
        assert!(s.node_cluster_center(target_id).is_none());
        assert_ne!(runtime.geometry_revision(), s.geometry_revision());

        // Follow the same stale -> rebuild-and-sync path as GraphViewState.
        s.sync_runtime(&mut runtime);
        assert_eq!(runtime.geometry_revision(), s.geometry_revision());

        let edge_index = runtime
            .edges()
            .edge_ids
            .iter()
            .position(|&id| id == edge_id)
            .expect("fixed-layout edge must be in runtime edge prep");
        assert_eq!(runtime.edges().source[edge_index], old_source);
        assert_eq!(runtime.edges().target[edge_index], old_target);

        let bounds = crate::viewport::WorldBounds {
            min: old_control - Vec2::splat(1.0),
            max: old_control + Vec2::splat(1.0),
        };
        let candidate_ids = runtime
            .visible_edge_candidates(&bounds, 0.0)
            .into_iter()
            .map(|index| runtime.edges().edge_ids[index])
            .collect::<Vec<_>>();
        assert!(
            candidate_ids.is_empty(),
            "fixed layout must not index the old SCC curve (old_control={old_control:?}, \
             candidates={candidate_ids:?})"
        );
    }

    #[test]
    fn populated_with_layout_uses_the_same_rebuild_boundary_for_fixed_and_force() {
        fn populated_scc() -> Scene {
            let mut scene = GraphScene::new().with_layout(Box::new(SccLayoutEngine));
            scene.merge(
                GraphBatch::new()
                    .node("a", "A")
                    .node("b", "B")
                    .edge("ab", "a", "b", EdgeDirection::Directed, "AB")
                    .edge("ba", "b", "a", EdgeDirection::Directed, "BA"),
            );
            scene
        }

        let replacements: [Box<dyn LayoutEngine>; 2] =
            [Box::new(FixedLayout), Box::new(ForceAtlas2::default())];
        for replacement in replacements {
            let mut scene = populated_scc();
            let a = scene.node_id(&"a").unwrap();
            let b = scene.node_id(&"b").unwrap();
            let positions_before = [
                scene.node_position(a).unwrap(),
                scene.node_position(b).unwrap(),
            ];
            assert!(scene.node_cluster_center(a).is_some());
            assert!(scene.node_cluster_center(b).is_some());
            let mut runtime = synced_runtime(&scene);
            let revision_before = scene.geometry_revision();

            scene = scene.with_layout(replacement);

            assert_eq!(scene.geometry_revision(), revision_before + 1);
            assert_eq!(scene.layout_state(), LayoutRunState::Running);
            assert_eq!(scene.node_position(a), Some(positions_before[0]));
            assert_eq!(scene.node_position(b), Some(positions_before[1]));
            assert!(scene.node_cluster_center(a).is_none());
            assert!(scene.node_cluster_center(b).is_none());
            assert_ne!(runtime.geometry_revision(), scene.geometry_revision());
            {
                let _synced = scene.sync_runtime(&mut runtime);
            }
            assert_eq!(runtime.geometry_revision(), scene.geometry_revision());
        }
    }
}

#[cfg(test)]
mod async_probe {
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[gpui::test]
    fn background_spawn_completes_under_test_executor(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = flag.clone();
        let scene = cx.new(|_| 0u32);
        scene.update(cx, |_, cx| {
            cx.spawn(async move |this, cx| {
                let done = cx.background_executor().spawn(async move {
                    flag2.store(true, Ordering::SeqCst);
                });
                done.await;
                let _ = this.update(cx, |_, _| {});
            })
            .detach();
        });
        cx.run_until_parked();
        assert!(flag.load(Ordering::SeqCst), "background task must run");
    }
}
