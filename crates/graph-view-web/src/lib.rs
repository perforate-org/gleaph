//! The browser host for [`graph_view_core`].
//!
//! The core decides what is visible, where nodes and labels land, and what a pointer hits; this
//! crate carries those answers across the wasm boundary so a canvas host draws them. It owns a
//! [`GraphScene`], its layout runtime and a [`Viewport`], and hands out a frame as flat typed
//! planes: positions and radii for nodes, endpoint pairs for edges, anchors for labels. The canvas
//! renderer and the DOM label layer live on the JavaScript side, where the page's own typography,
//! theme and accessibility apply.
//!
//! This is deliberately not a second renderer: the numbers come from `build_indexed_paint_frame`,
//! the same frame the desktop console paints, so culling, LOD and label budgets behave identically
//! on the page.

use glam::Vec2;
use graph_view_core::{
    EdgeDirection, ForceAtlas2, GraphBatch, GraphRuntime, GraphScene, GraphStyle, HitTestResult,
    LayoutBudget, Selection, Viewport, WorldBounds, build_indexed_paint_frame, hit_test_frame,
};

/// A frame flattened into planes for the canvas host.
///
/// Nodes: `[x, y, radius, label_slot, node_index]` per visible node.
/// Edges: `[x1, y1, x2, y2]` per visible edge, trimmed to the node boundaries.
/// Labels: `[x, y, label_slot, node_index]` per placed label, top-left, in label-budget order.
#[derive(Clone, Debug, Default)]
pub struct FrameData {
    pub nodes: Vec<f32>,
    pub edges: Vec<f32>,
    pub labels: Vec<f32>,
}

/// The graph session the page drives.
pub struct Session {
    scene: GraphScene<u32, u32, (), ()>,
    runtime: GraphRuntime,
    viewport: Viewport,
    style: GraphStyle,
    /// The application's categorical slot per node, used for colour and labels.
    label_slots: Vec<u32>,
    /// Node keys in insertion order: `NodeId` -> slot index.
    node_ids: Vec<graph_view_core::NodeId>,
}

impl Session {
    /// Build a session over a graph the page supplies.
    ///
    /// `positions` are world-space `[x, y]` pairs (the preset's starting layout), `edges` are
    /// `[source, target]` index pairs, and `label_slots[i]` is the colour/label class of node `i`.
    pub fn new(
        positions: &[f32],
        edges: &[u32],
        label_slots: &[u32],
        width: f32,
        height: f32,
    ) -> Self {
        let node_count = positions.len() / 2;
        let mut batch: GraphBatch<u32, u32, (), ()> = GraphBatch::new();
        for key in 0..node_count as u32 {
            batch = batch.node(key, ());
        }
        for (index, pair) in edges.as_chunks::<2>().0.iter().enumerate() {
            batch = batch.edge(
                index as u32,
                pair[0],
                pair[1],
                EdgeDirection::Undirected,
                (),
            );
        }
        // A demo starts from a ring and relaxes: the default scene has a fixed layout, which is
        // right for a console that hands positions to it but wrong for a page showing the layout.
        let mut scene = GraphScene::new().with_layout(Box::new(ForceAtlas2::default()));
        scene.merge(batch);
        scene.rebuild_layout();
        let node_ids: Vec<_> = scene.graph().nodes().map(|(id, _)| id).collect();
        for (index, id) in node_ids.iter().enumerate() {
            scene.set_position(
                *id,
                Vec2::new(positions[index * 2], positions[index * 2 + 1]),
            );
        }
        let mut label_slots = label_slots.to_vec();
        label_slots.resize(node_count, 0);
        let mut session = Self {
            scene,
            runtime: GraphRuntime::new(),
            viewport: Viewport::new(),
            style: GraphStyle {
                node_radius: 5.0,
                node_max_screen_radius: 8.0,
                node_stroke_width: 1.0,
                edge_width: 1.4,
                edge_arrow_enabled: false,
                label_budget: Some(220),
                ..GraphStyle::default()
            },
            label_slots,
            node_ids,
        };
        session.viewport.set_size(Vec2::new(width, height));
        session.fit();
        session
    }

    pub fn resize(&mut self, width: f32, height: f32) {
        self.viewport.set_size(Vec2::new(width, height));
    }

    /// Frame every node with an 8% margin, the camera the page opens on.
    pub fn fit(&mut self) {
        let mut bounds = WorldBounds {
            min: Vec2::splat(f32::INFINITY),
            max: Vec2::splat(f32::NEG_INFINITY),
        };
        for (id, _) in self.scene.graph().nodes() {
            if let Some(position) = self.scene.node_position(id) {
                bounds.min = bounds.min.min(position);
                bounds.max = bounds.max.max(position);
            }
        }
        self.viewport.fit_bounds(bounds, 0.08);
    }

    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.viewport.pan(Vec2::new(dx, dy));
    }

    pub fn zoom_at(&mut self, x: f32, y: f32, factor: f32) {
        self.viewport.zoom_at(Vec2::new(x, y), factor);
    }

    /// Advance the layout by up to `iterations` steps; true while it is still moving.
    pub fn step(&mut self, iterations: u32) -> bool {
        let progress = self.scene.step_layout(LayoutBudget {
            max_iterations: iterations.max(1),
            max_duration: None,
        });
        progress != graph_view_core::LayoutProgress::Settled
    }

    /// The core's paint frame, for hosts that place labels or hit-test with it.
    pub fn paint_frame(&mut self) -> graph_view_core::PaintFrame {
        let synced = self.scene.sync_runtime(&mut self.runtime);
        let node_ids = self.node_ids.clone();
        let label_slots = self.label_slots.clone();
        build_indexed_paint_frame(graph_view_core::IndexedPaintFrameInput {
            synced: &synced,
            node_label: &|id, _: &()| {
                node_ids
                    .iter()
                    .position(|candidate| *candidate == id)
                    .map(|index| label_slots[index].to_string())
            },
            edge_label: &|_, _: &()| None,
            viewport: &self.viewport,
            style: &self.style,
            selection: &Selection::new(),
            hover: &graph_view_core::Hover::default(),
            node_overlay: None,
            edge_overlay: None,
        })
    }

    /// Build the frame for the current camera and layout.
    pub fn frame_data(&mut self) -> FrameData {
        let frame = self.paint_frame();
        let mut data = FrameData::default();
        for node in &frame.nodes {
            let slot = self
                .node_ids
                .iter()
                .position(|candidate| *candidate == node.id)
                .map(|index| self.label_slots[index])
                .unwrap_or_default();
            let index = self
                .node_ids
                .iter()
                .position(|candidate| *candidate == node.id)
                .unwrap_or_default();
            data.nodes.extend_from_slice(&[
                node.position.x,
                node.position.y,
                node.radius,
                slot as f32,
                index as f32,
            ]);
        }
        for edge in &frame.edges {
            data.edges.extend_from_slice(&[
                edge.source.x,
                edge.source.y,
                edge.target.x,
                edge.target.y,
            ]);
        }
        let placed = graph_view_core::label_layout::place_labels(
            &frame,
            self.viewport.size(),
            &graph_view_core::label_layout::LabelMetrics::default(),
        );
        for label in &placed {
            let index = label.text.parse::<usize>().unwrap_or_default();
            let slot = self.label_slots.get(index).copied().unwrap_or_default();
            data.labels.extend_from_slice(&[
                label.position.x,
                label.position.y,
                slot as f32,
                index as f32,
            ]);
        }
        data
    }

    /// The node index under a screen point, or -1.
    pub fn pick(&mut self, x: f32, y: f32) -> i32 {
        let frame = {
            let synced = self.scene.sync_runtime(&mut self.runtime);
            build_indexed_paint_frame(graph_view_core::IndexedPaintFrameInput {
                synced: &synced,
                node_label: &|_, _: &()| None,
                edge_label: &|_, _: &()| None,
                viewport: &self.viewport,
                style: &self.style,
                selection: &Selection::new(),
                hover: &graph_view_core::Hover::default(),
                node_overlay: None,
                edge_overlay: None,
            })
        };
        let HitTestResult { node, edge } = hit_test_frame(&frame, Vec2::new(x, y), &self.style);
        if let Some(id) = node {
            return self
                .node_ids
                .iter()
                .position(|candidate| *candidate == id)
                .map(|index| index as i32)
                .unwrap_or(-1);
        }
        let _ = edge;
        -1
    }
}

/// The wasm-facing handle: the same session, with the frame returned as typed planes.
#[cfg(target_arch = "wasm32")]
mod web {
    use super::Session;
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    pub struct GraphSession {
        inner: Session,
    }

    #[wasm_bindgen]
    impl GraphSession {
        #[wasm_bindgen(constructor)]
        pub fn new(
            positions: &[f32],
            edges: &[u32],
            label_slots: &[u32],
            width: f32,
            height: f32,
        ) -> GraphSession {
            GraphSession {
                inner: Session::new(positions, edges, label_slots, width, height),
            }
        }

        pub fn resize(&mut self, width: f32, height: f32) {
            self.inner.resize(width, height);
        }

        pub fn fit(&mut self) {
            self.inner.fit();
        }

        pub fn pan(&mut self, dx: f32, dy: f32) {
            self.inner.pan(dx, dy);
        }

        pub fn zoom_at(&mut self, x: f32, y: f32, factor: f32) {
            self.inner.zoom_at(x, y, factor);
        }

        pub fn step(&mut self, iterations: u32) -> bool {
            self.inner.step(iterations)
        }

        pub fn pick(&mut self, x: f32, y: f32) -> i32 {
            self.inner.pick(x, y)
        }

        /// `[nodePlane, edgePlane, labelPlane]` as `Float32Array`s: no object per node, no strings
        /// per frame, and nothing to parse on the hot path.
        pub fn frame(&mut self) -> js_sys::Array {
            let data = self.inner.frame_data();
            let out = js_sys::Array::new();
            out.push(&js_sys::Float32Array::from(&data.nodes[..]));
            out.push(&js_sys::Float32Array::from(&data.edges[..]));
            out.push(&js_sys::Float32Array::from(&data.labels[..]));
            out
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::GraphSession;

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        // A five-node line with a single label class, at distinct world positions.
        let positions = [0.0, 0.0, 10.0, 0.0, 20.0, 0.0, 30.0, 0.0, 40.0, 0.0];
        let edges = [0, 1, 1, 2, 2, 3, 3, 4];
        Session::new(&positions, &edges, &[0; 5], 400.0, 300.0)
    }

    #[test]
    fn a_frame_carries_nodes_edges_and_labels_in_screen_space() {
        let mut session = session();
        let frame = session.frame_data();
        assert_eq!(frame.nodes.len() % 5, 0);
        assert_eq!(
            frame.nodes.len() / 5,
            5,
            "every node is visible when fitted"
        );
        assert_eq!(frame.edges.len() % 4, 0);
        assert_eq!(frame.edges.len() / 4, 4);
        for node in frame.nodes.as_chunks::<5>().0 {
            assert!(node[0] >= 0.0 && node[0] <= 400.0, "x within the viewport");
            assert!(node[1] >= 0.0 && node[1] <= 300.0, "y within the viewport");
            assert!(node[2] > 0.0, "a visible node has a radius");
        }
        assert!(!frame.labels.is_empty(), "the label budget admits labels");
    }

    #[test]
    fn panning_moves_the_frame_and_picking_finds_the_node_under_the_pointer() {
        let mut session = session();
        let before = session.frame_data();
        let (x, y, _radius) = (before.nodes[0], before.nodes[1], before.nodes[2]);
        let picked = session.pick(x, y);
        assert!(picked >= 0, "the first node is under its own centre");
        session.pan(25.0, 0.0);
        let after = session.frame_data();
        assert!(
            (after.nodes[0] - (before.nodes[0] + 25.0)).abs() < 0.5,
            "a pan shifts the frame by the pointer delta"
        );
    }

    #[test]
    fn zooming_in_makes_the_marker_radius_grow() {
        let mut session = session();
        let before = session.frame_data();
        let radius = before.nodes[2];
        session.zoom_at(200.0, 150.0, 2.0);
        let after = session.frame_data();
        assert!(after.nodes[2] > radius, "zoom scales the on-screen radius");
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;
    use graph_view_core::label_layout::{LabelMetrics, place_labels};

    #[test]
    fn labels_are_placed_off_their_markers_and_inside_the_viewport() {
        let positions = [0.0, 0.0, 10.0, 0.0, 20.0, 0.0];
        let edges = [0u32, 1, 1, 2];
        let mut session = Session::new(&positions, &edges, &[0, 1, 2], 400.0, 300.0);
        let frame = session.paint_frame();
        let placed = place_labels(
            &frame,
            glam::Vec2::new(400.0, 300.0),
            &LabelMetrics::default(),
        );
        assert!(!placed.is_empty(), "the frame has labels to place");
        for label in &placed {
            assert!(
                label.position.x >= 0.0 && label.position.x <= 400.0,
                "{label:?} is inside the canvas"
            );
            assert!(label.position.y >= 0.0 && label.position.y <= 300.0);
        }
        // The stand-off rule itself is tested where it lives: `graph-view-core`'s
        // `label_layout` tests. Here it is enough that a browser host can see the placed labels
        // and that they stay inside the canvas.
    }
}
