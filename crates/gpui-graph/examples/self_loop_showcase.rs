//! A deterministic self-loop rendering probe.
//!
//! Fixed node positions remove the initial-placement / auto-fit race so the
//! onigiri geometry can be inspected in screenshots reproducibly.

use gpui::{
    App, Bounds, Context, Entity, Render, TextStyle, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rems, size, white,
};
use gpui_graph::{EdgeDirection, FixedLayout, GraphBatch, GraphScene, GraphView, GraphViewState};

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
    view: Entity<GraphViewState<&'static str, &'static str, &'static str, &'static str>>,
}

impl Example {
    fn new(cx: &mut Context<Self>) -> Self {
        let scene = cx.new(|_cx| GraphScene::new().with_layout(Box::new(FixedLayout)));

        scene.update(cx, |scene, cx| {
            scene.merge(
                GraphBatch::new()
                    .node("lone", "Lone") // self-loop with no other edges
                    .node("hub", "Hub") // self-loop plus three edges
                    .node("n1", "N1")
                    .node("n2", "N2")
                    .node("n3", "N3")
                    // The label-dodge scenario: the single neighbor above
                    // bisects the free half-plane toward the node's own label
                    // band, so the loop axis must dodge out of it.
                    .node("top", "TOP")
                    .node("down", "down")
                    .edge("hh1", "hub", "n1", EdgeDirection::Directed, "e1")
                    .edge("hh2", "hub", "n2", EdgeDirection::Directed, "e2")
                    .edge("hh3", "hub", "n3", EdgeDirection::Directed, "e3")
                    .edge("td", "down", "top", EdgeDirection::Undirected, "link")
                    .edge("loop_lone", "lone", "lone", EdgeDirection::Directed, "self")
                    .edge("loop_hub", "hub", "hub", EdgeDirection::Directed, "self")
                    .edge("loop_down", "down", "down", EdgeDirection::Directed, "self"),
            );

            // Fixed positions: hub centered with three neighbors around it,
            // lone node isolated on the right.
            let a = scene.node_id(&"lone").unwrap();
            let h = scene.node_id(&"hub").unwrap();
            let n1 = scene.node_id(&"n1").unwrap();
            let n2 = scene.node_id(&"n2").unwrap();
            let n3 = scene.node_id(&"n3").unwrap();
            let top = scene.node_id(&"top").unwrap();
            let down = scene.node_id(&"down").unwrap();
            scene.set_position(a, glam::vec2(260.0, -140.0));
            scene.set_position(h, glam::vec2(-40.0, 0.0));
            scene.set_position(n1, glam::vec2(-190.0, -150.0));
            scene.set_position(n2, glam::vec2(-220.0, 60.0));
            scene.set_position(n3, glam::vec2(60.0, 170.0));
            scene.set_position(top, glam::vec2(260.0, -20.0));
            scene.set_position(down, glam::vec2(260.0, 160.0));
            let _ = cx;
            cx.notify();
        });

        let view = cx.new(|cx| GraphViewState::new_with_default_labels(scene, cx));

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
            .bg(gpui::hsla(0.0, 0.0, 0.1, 1.0))
            .child(GraphView::new(self.view.clone()).size_full())
    }
}
