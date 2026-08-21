//! A `rapidhash`-backed example for a large graph.
//!
//! Demonstrates rendering many nodes and edges through the same public API as
//! the other examples, but with a faster non-cryptographic hasher
//! ([`rapidhash::fast::RandomState`]) chosen on the scene, runtime, and view.
//! The hasher sits behind the spatial grids that accelerate per-frame culling
//! and edge avoidance.
//!
//! The graph is a large grid with nodes laid out at uniform spacing and no
//! labels, matching the `paint_bench` overview scenario. Keeping nodes sparse
//! and label-free keeps the per-frame paint cost dominated by the grid hasher
//! rather than by text shaping or dense neighbor sets.
//!
//! This example uses `rapidhash` from the crate's dev-dependencies; examples
//! resolve dev-dependencies, so no extra dependency is required.

use glam::Vec2;
use gpui::{
    App, Bounds, Context, Entity, Render, TextStyle, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rems, size, white,
};
use gpui_graph::{EdgeDirection, FixedLayout, GraphBatch, GraphScene, GraphView, GraphViewState};

/// The hasher chosen for this example. `rapidhash::fast::RandomState` is a
/// fast non-cryptographic hasher; the default SipHash `RandomState` is the
/// alternative.
type Hasher = rapidhash::fast::RandomState;
/// Node identity is a dense `usize` index; edges are keyed by a monotonically
/// increasing `usize`. Nodes carry no per-node data and edges carry no data.
type ViewState = GraphViewState<usize, usize, (), (), Hasher>;

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(900.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(Example::new),
        )
        .unwrap();
        cx.activate(true);
    });
}

struct Example {
    view: Entity<ViewState>,
}

impl Example {
    fn new(cx: &mut Context<Self>) -> Self {
        // 1. Shared scene with the rapidhash hasher and a fixed layout, so
        //    node positions are set manually below.
        let scene = cx.new(|_cx| {
            GraphScene::<usize, usize, (), (), Hasher>::with_hasher(Hasher::default())
                .with_layout(Box::new(FixedLayout))
        });

        // 2. Populate the scene with a large grid graph: `side * side` nodes,
        //    each with edges to its right and down neighbors. Uniform spacing
        //    keeps the local density bounded, as in the paint benchmark.
        let side = 50;
        let spacing = 60.0;
        let mut batch = GraphBatch::new();
        let mut ids = Vec::new();
        for y in 0..side {
            for x in 0..side {
                let id = y * side + x;
                batch = batch.node(id, ());
                ids.push(id);
            }
        }
        let at = |x: usize, y: usize| ids[y * side + x];
        let mut edge_key = 0usize;
        for y in 0..side {
            for x in 0..side {
                let id = at(x, y);
                if x + 1 < side {
                    batch = batch.edge(edge_key, id, at(x + 1, y), EdgeDirection::Directed, ());
                    edge_key += 1;
                }
                if y + 1 < side {
                    batch = batch.edge(edge_key, id, at(x, y + 1), EdgeDirection::Directed, ());
                    edge_key += 1;
                }
            }
        }
        scene.update(cx, |scene, cx| {
            scene.merge(batch);
            // Set uniform positions so the spatial grids stay sparse.
            for y in 0..side {
                for x in 0..side {
                    let node = scene.node_id(&ids[y * side + x]).expect("grid node exists");
                    scene.set_position(node, Vec2::new(x as f32 * spacing, y as f32 * spacing));
                }
            }
            cx.notify();
        });

        // 3. A view state over the scene, inheriting the rapidhash hasher from
        //    the scene argument. No labels, matching the benchmark overview.
        let view = cx.new(|cx| GraphViewState::new(scene, cx));

        // 4. Style the graph for a high-contrast dark theme. Edges shorter than
        //    24px on screen are simplified to straight lines (level-of-detail),
        //    which is what a fully-zoomed-out overview of this dense grid hits.
        //    While the user pans or zooms, the interaction-time threshold collapses
        //    every edge to a straight segment so the camera stays smooth, settling
        //    back to the idle 24px threshold shortly after the gesture stops.
        view.update(cx, |view, cx| {
            let style = view.style_mut();
            style.label_style = TextStyle {
                color: white(),
                font_size: rems(0.6).into(),
                ..TextStyle::default()
            };
            style.edge_straight_threshold = 24.0;
            style.edge_straight_threshold_while_interacting = 10_000.0;
            // In the zoomed-out overview every edge is far shorter than its
            // arrowhead, so omitting arrowheads for edges below 24px removes
            // ~4900 arrow primitives with no readable direction lost.
            style.edge_arrow_min_length = 24.0;
            // The 2500 nodes render at a fixed 12px diameter; drawing them as
            // fill-only dots (no sub-pixel stroke ring) is visually identical at
            // this density and cheaper per quad.
            style.node_simplify_threshold = style.node_radius * 2.0;
            // Only trim truly sub-pixel edges; the overview's ~16px edges stay.
            style.edge_min_length = 2.0;
            cx.notify();
        });

        Self { view }
    }
}

impl Render for Example {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(gpui::hsla(0.0, 0.0, 0.1, 1.0)) // Dark charcoal background
            .child(GraphView::new(self.view.clone()).size_full())
    }
}
