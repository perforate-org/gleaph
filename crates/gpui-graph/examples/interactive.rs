//! An interactive, dynamic `gpui-graph` example.
//!
//! Where `basic.rs` shows a fixed, static graph, this example exercises the
//! same four-layer public API (§27) *dynamically*:
//!
//! - a logical graph built from a batch and mutated live with `merge` / `apply`,
//! - a shared scene whose `ForceAtlas2` layout relaxes into place each frame,
//! - a view state that emits `GraphEvent`s (click, double-click, selection),
//! - a composable `GraphView` plus ordinary GPUI chrome layered on top.
//!
//! The graph models a small collaboration team. The *full* membership is a
//! static catalog (the example's stand-in for a database query result); the
//! scene only ever renders a working subset. Clicking a node "expands" it by
//! merging its catalog neighbors into the scene, so you see the graph grow and
//! the force model relax into the new topology — the same pattern a
//! graph-database UI uses to page through a result incrementally.
//!
//! Interactions:
//! - **click a node** to expand its neighbors into the graph,
//! - **double-click a node** to collapse it (and its incident edges) out,
//! - drag to move a node, pan on empty space, scroll to zoom.
//!
//! The overlay panel reports the current selection and graph size so the
//! dynamic topology is visible in text as well as pixels.

use std::collections::HashMap;

use gpui::{
    App, Application, Bounds, Context, Entity, Render, TextStyle, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rems, size, white,
};
use gpui_graph::{
    EdgeDirection, ForceAtlas2, GraphBatch, GraphEvent, GraphPatch, GraphScene, GraphView,
    GraphViewState, LayoutBudget, LayoutProgress, NodePatch,
};

/// Application data for a node (§6.3). The external key is stored with the
/// payload so the example can expand a clicked `NodeId` back into the catalog:
/// the scene exposes no reverse `NodeId -> key` lookup, so the application
/// owns that correspondence — exactly as a real graph-database app stores the
/// key with its payload.
struct Person {
    key: &'static str,
    name: &'static str,
    role: &'static str,
}

/// Each member's neighbors in the catalog. Only edges whose both endpoints are
/// in the scene are ever created, so the scene grows edge-by-edge with nodes.
fn neighbors(key: &'static str) -> &'static [&'static str] {
    match key {
        "Alice" => &["Bob", "Carol", "Dave", "Eve"],
        "Bob" => &["Alice", "Dave", "Frank"],
        "Carol" => &["Alice", "Grace", "Heidi"],
        "Dave" => &["Alice", "Bob", "Ivan"],
        "Eve" => &["Alice", "Judy"],
        "Frank" => &["Bob", "Mallory"],
        "Grace" => &["Carol"],
        "Heidi" => &["Carol", "Oscar"],
        "Ivan" => &["Dave", "Judy"],
        "Judy" => &["Eve", "Ivan"],
        "Mallory" => &["Frank", "Oscar"],
        "Oscar" => &["Heidi", "Mallory"],
        _ => &[],
    }
}

fn person(key: &'static str) -> Person {
    let name = key;
    let role = match key {
        "Alice" => "lead",
        "Bob" => "platform",
        "Carol" => "design",
        "Dave" => "backend",
        "Eve" => "backend",
        "Frank" => "infra",
        "Grace" => "design",
        "Heidi" => "frontend",
        "Ivan" => "backend",
        "Judy" => "data",
        "Mallory" => "security",
        "Oscar" => "frontend",
        _ => "member",
    };
    Person { key, name, role }
}

/// Leak a formatted string into a `&'static str`, the same pattern the other
/// examples use to build edge keys at runtime.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

type Scene = GraphScene<&'static str, &'static str, Person, &'static str>;
type View = GraphViewState<&'static str, &'static str, Person, &'static str>;

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1000.), px(680.)), cx);
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
    view: Entity<View>,
    scene: Entity<Scene>,
    settled: bool,
    /// The name currently selected, shown in the overlay panel.
    selected: Option<(&'static str, &'static str)>,
    /// Keys of nodes that have been collapsed, so clicking them re-expands.
    collapsed: HashMap<&'static str, ()>,
    /// Keeps the view-event subscription alive for the example's lifetime.
    _subscription: gpui::Subscription,
}

impl Example {
    fn new(cx: &mut Context<Self>) -> Self {
        let scene: Entity<Scene> =
            cx.new(|_cx| GraphScene::new().with_layout(Box::new(ForceAtlas2::default())));

        let view: Entity<View> = cx.new(|cx| {
            let mut view = GraphViewState::new(scene.clone(), cx);
            view.set_node_label(|_id, person| Some(person.name.to_string()));
            view.set_edge_label(|_id, rel| Some(rel.to_string()));
            view
        });

        // Style the graph for a high-contrast dark theme.
        view.update(cx, |view, cx| {
            let style = view.style_mut();
            style.node_radius = 7.0;
            style.node_stroke_width = 1.5;
            style.label_style = TextStyle {
                color: white(),
                font_size: rems(0.75).into(),
                ..TextStyle::default()
            };
            style.label_offset = 2.0;
            style.edge_width = 1.5;
            style.edge_arrow_min_length = 24.0;
            cx.notify();
        });

        // Wire the view's interaction events back into this example: clicks
        // expand, double-clicks collapse, and selection updates the panel.
        let sub = cx.subscribe(&view, |this, _view, event, cx| {
            this.handle_event(event, cx);
        });

        let mut this = Self {
            view,
            scene,
            settled: false,
            selected: None,
            collapsed: HashMap::new(),
            // Keep the subscription alive for the lifetime of the example.
            _subscription: sub,
        };
        // Start from a single node; the user grows the graph by clicking.
        this.expand("Alice", cx);
        this
    }

    /// Expand `key` and its catalog neighbors into the scene. Adding new nodes
    /// bumps the topology revision, which reheats the ForceAtlas2 layout so it
    /// relaxes around the freshly grown graph.
    fn expand(&mut self, key: &'static str, cx: &mut Context<Self>) {
        self.collapsed.remove(key);
        let mut batch = GraphBatch::new();
        batch = batch.node(key, person(key));
        for &neighbor in neighbors(key) {
            batch = batch.node(neighbor, person(neighbor));
        }
        for &nk in neighbors(key) {
            // Key the edge by the lexicographically-ordered endpoint pair so the
            // same undirected link has one stable identity regardless of which
            // endpoint expanded first. Otherwise expanding Alice then Dave would
            // create both `Alice--Dave` and `Dave--Alice` as parallel edges.
            let (lo, hi) = if key <= nk { (key, nk) } else { (nk, key) };
            batch = batch.edge(
                leak(format!("{lo}--{hi}")),
                key,
                nk,
                EdgeDirection::Undirected,
                "works with",
            );
        }
        self.scene.update(cx, |scene, cx| {
            scene.merge(batch);
            cx.notify();
        });
        // New nodes reheat the layout, so resume animating it to settle.
        self.settled = false;
        // Keep the action centered on the clicked node.
        let node = self.scene.read(cx).node_id(&key).expect("node expanded");
        self.view.update(cx, |view, cx| view.focus_node(node, cx));
    }

    /// Collapse `key` and its incident edges out of the scene.
    fn collapse(&mut self, key: &'static str, cx: &mut Context<Self>) {
        if self.collapsed.contains_key(&key) {
            return;
        }
        self.collapsed.insert(key, ());
        self.scene.update(cx, |scene, cx| {
            scene.apply(GraphPatch::new().node(NodePatch::Remove { key }));
            cx.notify();
        });
        if self.selected.map(|(name, _)| name == key).unwrap_or(false) {
            self.selected = None;
        }
    }

    fn handle_event(&mut self, event: &GraphEvent, cx: &mut Context<Self>) {
        match event {
            GraphEvent::NodeClicked { node, .. } => {
                let key = self.scene.read(cx).graph().node(*node).map(|n| n.data.key);
                if let Some(key) = key {
                    self.expand(key, cx);
                }
            }
            GraphEvent::NodeDoubleClicked { node } => {
                let key = self.scene.read(cx).graph().node(*node).map(|n| n.data.key);
                if let Some(key) = key {
                    self.collapse(key, cx);
                }
            }
            GraphEvent::SelectionChanged { selection } => {
                self.selected = selection.nodes.first().and_then(|id| {
                    self.scene
                        .read(cx)
                        .graph()
                        .node(*id)
                        .map(|n| (n.data.name, n.data.role))
                });
                cx.notify();
            }
            _ => {}
        }
    }
}

impl Render for Example {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Advance the force model one frame, stopping when the layout settles.
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

        let node_count = self.scene.read(cx).graph().node_count();
        let edge_count = self.scene.read(cx).graph().edge_count();
        let selected = self.selected;

        let panel = div().absolute().top(px(16.)).right(px(16.)).child(
            div()
                .bg(gpui::hsla(0.0, 0.0, 0.16, 1.0))
                .p_4()
                .rounded_md()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_color(white())
                        .text_sm()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child("Interactive team graph"),
                )
                .child(
                    div()
                        .text_color(gpui::hsla(0.0, 0.0, 0.7, 1.0))
                        .text_xs()
                        .child(format!("{node_count} nodes  ·  {edge_count} edges")),
                )
                .child(match selected {
                    Some((name, role)) => div()
                        .text_color(gpui::hsla(0.08, 0.7, 0.55, 1.0))
                        .text_xs()
                        .child(format!("selected: {name} · {role}")),
                    None => div()
                        .text_color(gpui::hsla(0.0, 0.0, 0.7, 1.0))
                        .text_xs()
                        .child("click to expand neighbors · double-click to collapse"),
                }),
        );

        div()
            .size_full()
            .bg(gpui::hsla(0.0, 0.0, 0.1, 1.0))
            .child(panel)
            .child(GraphView::new(self.view.clone()).size_full())
    }
}
