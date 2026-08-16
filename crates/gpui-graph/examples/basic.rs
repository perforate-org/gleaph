//! A minimal `gpui-graph` example.
//!
//! Demonstrates the four-layer public API (§27): a logical graph populated by
//! merging a batch, a shared scene with a ForceAtlas2 layout, a view state, and
//! a composable `GraphView` rendered inside ordinary GPUI layout.

use gpui::{
    App, Application, Bounds, Context, Entity, Render, TextStyle, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rems, size, white,
};
use gpui_graph::{EdgeDirection, ForceAtlas2, GraphBatch, GraphScene, GraphView, GraphViewState};

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
}

impl Example {
    fn new(cx: &mut Context<Self>) -> Self {
        // 1. Shared scene with a ForceAtlas2 layout.
        let scene = cx.new(|_cx| GraphScene::new().with_layout(Box::new(ForceAtlas2::default())));

        // 2. Populate the scene by merging a batch of graph data.
        scene.update(cx, |scene, cx| {
            scene.merge(
                GraphBatch::new()
                    .node("alice", "Alice")
                    .node("bob", "Bob")
                    .node("carol", "Carol")
                    .edge("ab", "alice", "bob", EdgeDirection::Directed, "knows")
                    .edge("ac", "alice", "carol", EdgeDirection::Directed, "knows"),
            );
            cx.notify();
        });

        // 3. A view state over the scene. The view auto-fits the graph on its
        //    first layout, so no explicit `fit_all` is required here. Because
        //    the node and edge data types (`&'static str`) implement `Display`,
        //    default node and edge labels are shown automatically.
        let view = cx.new(|cx| GraphViewState::new_with_default_labels(scene, cx));

        // 4. Style the label text and its offset below the node.
        view.update(cx, |view, cx| {
            let style = view.style_mut();
            style.label_style = TextStyle {
                color: white(),
                font_size: rems(0.8).into(),
                ..TextStyle::default()
            };
            style.label_offset = 4.0;
            cx.notify();
        });

        Self { view }
    }
}

impl Render for Example {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(GraphView::new(self.view.clone()).size_full())
    }
}
