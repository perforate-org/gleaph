//! A minimal `gpui-graph` example.
//!
//! Demonstrates the four-layer public API (§27): a logical graph populated by
//! merging a batch, a shared scene with a ForceAtlas2 layout, a view state, and
//! a composable `GraphView` rendered inside ordinary GPUI layout.

use gpui::{
    App, Bounds, Context, Entity, Render, TextStyle, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rems, size, white,
};
use gpui_graph::{EdgeDirection, FixedLayout, GraphBatch, GraphScene, GraphView, GraphViewState};

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
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
        // 1. Shared scene with a FixedLayout for deterministic positioning.
        let scene = cx.new(|_cx| GraphScene::new().with_layout(Box::new(FixedLayout)));

        // 2. Populate the scene and set manual positions for a triangular layout.
        scene.update(cx, |scene, cx| {
            scene.merge(
                GraphBatch::new()
                    .node("alice", "Alice")
                    .node("bob", "Bob")
                    .node("carol", "Carol")
                    .edge("ab", "alice", "bob", EdgeDirection::Directed, "knows")
                    .edge("ac", "alice", "carol", EdgeDirection::Directed, "knows")
                    // Parallel edges fan out as curves.
                    .edge("ab2", "alice", "bob", EdgeDirection::Directed, "likes")
                    .edge("ab3", "alice", "bob", EdgeDirection::Directed, "mentions")
                    // A self-loop renders as an onigiri (rounded triangle) that
                    // points away from the node's other edges.
                    .edge("aa", "alice", "alice", EdgeDirection::Directed, "self"),
            );

            cx.notify();
        });

        // 3. A view state over the scene.
        let view = cx.new(|cx| GraphViewState::new_with_default_labels(scene, cx));

        // 4. Style the graph for a high-contrast dark theme.
        view.update(cx, |view, cx| {
            let style = view.style_mut();
            style.label_style = TextStyle {
                color: white(),
                font_size: rems(0.8).into(),
                ..TextStyle::default()
            };
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
