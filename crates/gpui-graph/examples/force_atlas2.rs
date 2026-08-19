//! A `ForceAtlas2` layout example.
//!
//! Demonstrates the dynamic layout engine: a shared scene with a `ForceAtlas2`
//! layout, a view state, and a composable `GraphView`. Unlike the deterministic
//! `FixedLayout` and `SccLayoutEngine` examples, `ForceAtlas2` is an iterative
//! force model (repulsion, attraction, gravity), so the layout advances one
//! frame at a time. The example steps the layout on every animation frame and
//! requests the next frame, so the graph visibly relaxes into place.
//!
//! The graph is a small random graph with a few hubs, so the force model has
//! something to work with: hubs repel each other while their neighbors are
//! pulled in, and the whole graph is drawn toward the origin by gravity.

use gpui::{
    App, Application, Bounds, Context, Entity, Render, TextStyle, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rems, size, white,
};
use gpui_graph::{
    EdgeDirection, ForceAtlas2, GraphBatch, GraphScene, GraphView, GraphViewState, LayoutBudget,
    LayoutProgress, Rng,
};

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(800.), px(600.)), cx);
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
    view: Entity<GraphViewState<&'static str, &'static str, &'static str, &'static str>>,
    scene: Entity<GraphScene<&'static str, &'static str, &'static str, &'static str>>,
    settled: bool,
}

impl Example {
    fn new(cx: &mut Context<Self>) -> Self {
        // 1. Shared scene with a ForceAtlas2 layout.
        let scene = cx.new(|_cx| GraphScene::new().with_layout(Box::new(ForceAtlas2::default())));

        // 2. Populate the scene with a small random graph: a few hubs, each
        //    with a handful of neighbors, plus a couple of cross links.
        let mut batch = GraphBatch::new();
        let mut rng = Rng::new(42);
        let n = 24;
        let mut nodes: Vec<&'static str> = Vec::new();
        for i in 0..n {
            let key: &'static str = Box::leak(format!("n{i}").into_boxed_str());
            nodes.push(key);
            batch = batch.node(key, key);
        }
        // A few hubs connect to many others; the rest connect sparsely.
        for i in 0..n {
            let degree = if i % 6 == 0 { 5 } else { 2 };
            for _ in 0..degree {
                let j = (rng.next_f32() * n as f32) as usize % n;
                if i != j {
                    let key: &'static str = Box::leak(format!("e{i}_{j}").into_boxed_str());
                    batch = batch.edge(key, nodes[i], nodes[j], EdgeDirection::Undirected, "");
                }
            }
        }
        scene.update(cx, |scene, cx| {
            scene.merge(batch);
            cx.notify();
        });

        // 3. A view state over the scene. Use `new` (no default labels) and
        //    set only node labels, so edges carry no label and are not cut at
        //    their midpoint by an empty label rect.
        let view = cx.new(|cx| GraphViewState::new(scene.clone(), cx));
        view.update(cx, |view, cx| {
            view.set_node_label(|_id, node| Some(node.to_string()));
            cx.notify();
        });

        // 4. Style the graph for a high-contrast dark theme.
        view.update(cx, |view, cx| {
            let style = view.style_mut();
            style.label_style = TextStyle {
                color: white(),
                font_size: rems(0.7).into(),
                ..TextStyle::default()
            };
            cx.notify();
        });

        Self {
            view,
            scene,
            settled: false,
        }
    }
}

impl Render for Example {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Advance the force model by one frame. Once the layout settles, stop
        // requesting animation frames so the window is not redrawn forever.
        if !self.settled {
            let progress = self.scene.update(cx, |scene, cx| {
                let p = scene.step_layout(LayoutBudget::default());
                cx.notify();
                p
            });
            if progress == LayoutProgress::Settled {
                self.settled = true;
            } else {
                window.request_animation_frame();
            }
        }

        div()
            .size_full()
            .bg(gpui::hsla(0.0, 0.0, 0.1, 1.0)) // Dark charcoal background
            .child(GraphView::new(self.view.clone()).size_full())
    }
}
