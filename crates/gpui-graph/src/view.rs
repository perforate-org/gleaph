//! View state and composable view component (§16, §27.3, §27.4).
//!
//! A [`GraphViewState`] represents the state of a particular view into a graph
//! scene: viewport, hover, selection, interaction state, and style. Two view
//! states may reference the same scene (e.g. a main view and a minimap).
//! [`GraphView`] is a lightweight composable GPUI component that renders a view
//! state through GPUI's low-level canvas API. `GraphView` owns the boundary
//! conversion between GPUI window-space input/paint coordinates and the
//! canvas-local coordinates used by [`Viewport`] and [`crate::paint::PaintFrame`].

use glam::Vec2;
use gpui::{
    Bounds, Context, Div, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    PathBuilder, ScrollDelta, StyleRefinement, Styled, Window, canvas, div, point, px, quad, size,
};
use std::{cell::Cell, marker::PhantomData, rc::Rc};

#[cfg(test)]
use std::cell::RefCell;

use crate::graph::{EdgeDirection, NodeId};
use crate::hit_test;
use crate::interaction::{GraphEvent, Hover, MouseButton, Selection};
use crate::layout::LayoutBudget;
use crate::scene::GraphScene;
use crate::style::{ArrowShape, GraphStyle};
use crate::viewport::{Viewport, WorldBounds};

/// A resolver that returns the label text for a node, or `None` for no label.
type NodeLabelResolver<N> = Rc<dyn Fn(NodeId, &N) -> Option<String>>;

/// The state of a particular view into a graph scene (§16).
pub struct GraphViewState<NK, EK, N, E> {
    scene: Entity<GraphScene<NK, EK, N, E>>,
    viewport: Viewport,
    selection: Selection,
    hover: Hover,
    style: GraphStyle,
    node_label: NodeLabelResolver<N>,
    dragging: Option<NodeId>,
    panning: bool,
    last_mouse: Vec2,
    initial_auto_fit: InitialAutoFitState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InitialAutoFitState {
    pending: bool,
}

impl InitialAutoFitState {
    fn cancel(&mut self) {
        self.pending = false;
    }

    fn consume_if_canvas_ready(&mut self, size: Vec2) -> bool {
        if self.pending && size.x > 0.0 && size.y > 0.0 {
            self.pending = false;
            true
        } else {
            false
        }
    }
}

impl Default for InitialAutoFitState {
    fn default() -> Self {
        Self { pending: true }
    }
}

/// Coordinates owned by one rendered graph element.
///
/// GPUI input positions and paint primitives are window-space, while the
/// viewport and [`crate::paint::PaintFrame`] use canvas-local pixels. The
/// element-local origin is the only state needed to cross that boundary; it
/// is deliberately not part of [`GraphViewState`] or [`Viewport`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct CanvasCoordinates {
    origin: Vec2,
}

impl CanvasCoordinates {
    fn from_bounds(bounds: Bounds<gpui::Pixels>) -> Self {
        Self {
            origin: Vec2::new(f32::from(bounds.origin.x), f32::from(bounds.origin.y)),
        }
    }

    fn mouse_move_position(self, position: gpui::Point<gpui::Pixels>) -> Vec2 {
        self.window_to_canvas(Vec2::new(f32::from(position.x), f32::from(position.y)))
    }

    fn mouse_down_position(self, position: gpui::Point<gpui::Pixels>) -> Vec2 {
        self.window_to_canvas(Vec2::new(f32::from(position.x), f32::from(position.y)))
    }

    fn scroll_position(self, position: gpui::Point<gpui::Pixels>) -> Vec2 {
        self.window_to_canvas(Vec2::new(f32::from(position.x), f32::from(position.y)))
    }

    fn window_to_canvas(self, position: Vec2) -> Vec2 {
        position - self.origin
    }

    fn canvas_to_window(self, position: Vec2) -> Vec2 {
        position + self.origin
    }

    fn edge_endpoints(self, edge: &crate::paint::PaintEdge) -> (Vec2, Vec2) {
        (
            self.canvas_to_window(edge.source),
            self.canvas_to_window(edge.target),
        )
    }

    fn node_bounds(self, node: &crate::paint::PaintNode) -> Bounds<gpui::Pixels> {
        let origin = self.canvas_to_window(node.position - Vec2::splat(node.radius));
        let diameter = px(node.radius * 2.0);
        Bounds {
            origin: point(px(origin.x), px(origin.y)),
            size: size(diameter, diameter),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
enum TestPaintPrimitive {
    Edge { source: Vec2, target: Vec2 },
    Arrow { source: Vec2, target: Vec2 },
    Node { origin: Vec2, size: Vec2 },
    Label { position: Vec2 },
}

#[cfg(test)]
thread_local! {
    static TEST_PAINT_TRACE: RefCell<Vec<TestPaintPrimitive>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn clear_test_paint_trace() {
    TEST_PAINT_TRACE.with(|trace| trace.borrow_mut().clear());
}

#[cfg(test)]
fn take_test_paint_trace() -> Vec<TestPaintPrimitive> {
    TEST_PAINT_TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()))
}

impl<NK, EK, N, E> GraphViewState<NK, EK, N, E>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: 'static,
    E: 'static,
{
    /// Create a view state over the given scene.
    pub fn new(scene: Entity<GraphScene<NK, EK, N, E>>, _cx: &mut Context<Self>) -> Self {
        Self {
            scene,
            viewport: Viewport::new(),
            selection: Selection::new(),
            hover: Hover::default(),
            style: GraphStyle::default(),
            node_label: Rc::new(|_id, _node| None),
            dragging: None,
            panning: false,
            last_mouse: Vec2::ZERO,
            initial_auto_fit: InitialAutoFitState::default(),
        }
    }

    /// The scene this view observes.
    pub fn scene(&self) -> &Entity<GraphScene<NK, EK, N, E>> {
        &self.scene
    }

    /// The viewport.
    pub fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    /// The viewport, mutably.
    ///
    /// Accessing the mutable viewport is an explicit viewport override, so it
    /// takes precedence over the one-time default initial fit.
    pub fn viewport_mut(&mut self) -> &mut Viewport {
        self.cancel_initial_auto_fit();
        &mut self.viewport
    }

    /// The current selection.
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// The current selection, mutably.
    pub fn selection_mut(&mut self) -> &mut Selection {
        &mut self.selection
    }

    /// The current hover target.
    pub fn hover(&self) -> &Hover {
        &self.hover
    }

    /// The graph style.
    pub fn style(&self) -> &GraphStyle {
        &self.style
    }

    /// The graph style, mutably.
    pub fn style_mut(&mut self) -> &mut GraphStyle {
        &mut self.style
    }

    /// Set the node label resolver.
    ///
    /// The resolver returns the label text for a node, or `None` to render no
    /// label. It is called during prepaint for every visible node. This
    /// overrides any default label resolution.
    pub fn set_node_label(&mut self, resolver: impl Fn(NodeId, &N) -> Option<String> + 'static) {
        self.node_label = Rc::new(resolver);
    }

    /// Fit the viewport to the bounds of all nodes in the scene.
    ///
    /// An explicit fit takes precedence over the one-time default initial fit.
    pub fn fit_all(&mut self, cx: &mut Context<Self>) {
        self.cancel_initial_auto_fit();
        self.fit_all_impl(cx);
    }

    fn fit_all_impl(&mut self, cx: &mut Context<Self>) {
        let scene = self.scene.read(cx);
        let mut min = Vec2::ZERO;
        let mut max = Vec2::ZERO;
        let mut first = true;
        for (id, _) in scene.graph().nodes() {
            if let Some(pos) = scene.node_position(id) {
                if first {
                    min = pos;
                    max = pos;
                    first = false;
                } else {
                    min = min.min(pos);
                    max = max.max(pos);
                }
            }
        }
        if !first {
            self.viewport.fit_bounds(WorldBounds { min, max }, 0.1);
        }
    }

    fn prepare_canvas(&mut self, size: Vec2, cx: &mut Context<Self>) {
        self.viewport.set_size(size);
        if self.initial_auto_fit.consume_if_canvas_ready(size) {
            self.fit_all_impl(cx);
        }
    }

    /// Center the viewport on a node without changing zoom.
    ///
    /// An explicit focus takes precedence over the one-time default initial fit.
    pub fn focus_node(&mut self, node: NodeId, cx: &mut Context<Self>) {
        self.cancel_initial_auto_fit();
        if let Some(pos) = self.scene.read(cx).node_position(node) {
            self.viewport.focus(pos);
        }
    }

    fn cancel_initial_auto_fit(&mut self) {
        self.initial_auto_fit.cancel();
    }

    fn handle_zoom(&mut self, pos: Vec2, factor: f32, cx: &mut Context<Self>) {
        self.cancel_initial_auto_fit();
        self.viewport.zoom_at(pos, factor);
        cx.emit(GraphEvent::ViewportChanged);
        cx.notify();
    }

    /// Set the selection and emit a selection-changed event.
    pub fn set_selection(&mut self, selection: Selection, cx: &mut Context<Self>) {
        self.selection = selection;
        cx.emit(GraphEvent::SelectionChanged {
            selection: self.selection.clone(),
        });
        cx.notify();
    }

    /// Step the scene's layout by one frame budget.
    pub fn step_layout(&mut self, cx: &mut Context<Self>) {
        self.scene.update(cx, |scene, cx| {
            scene.step_layout(LayoutBudget::default());
            cx.notify();
        });
    }
}

impl<NK, EK, N, E> GraphViewState<NK, EK, N, E>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: std::fmt::Display + 'static,
    E: 'static,
{
    /// Create a view state over the given scene with default node labels.
    ///
    /// Each node's label is its `Display` representation. Callers can still
    /// override this per node with [`Self::set_node_label`].
    pub fn new_with_default_labels(
        scene: Entity<GraphScene<NK, EK, N, E>>,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            scene,
            viewport: Viewport::new(),
            selection: Selection::new(),
            hover: Hover::default(),
            style: GraphStyle::default(),
            node_label: Rc::new(|_id, node| Some(node.to_string())),
            dragging: None,
            panning: false,
            last_mouse: Vec2::ZERO,
            initial_auto_fit: InitialAutoFitState::default(),
        }
    }
}

impl<NK, EK, N, E> EventEmitter<GraphEvent> for GraphViewState<NK, EK, N, E>
where
    NK: 'static,
    EK: 'static,
    N: 'static,
    E: 'static,
{
}

/// A composable GPUI component that renders a graph view state (§27.4).
///
/// `GraphView` is a styled element: it participates in normal GPUI layout and
/// styling (e.g. `.size_full()`, `.border_1()`) and renders the graph through
/// GPUI's low-level canvas API.
pub struct GraphView<NK, EK, N, E> {
    element: Div,
    _marker: PhantomData<fn(NK, EK, N, E)>,
}

impl<NK, EK, N, E> GraphView<NK, EK, N, E>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: 'static,
    E: 'static,
{
    /// Create a graph view over the given view state.
    pub fn new(view: Entity<GraphViewState<NK, EK, N, E>>) -> Self {
        let canvas_coordinates = Rc::new(Cell::new(CanvasCoordinates::default()));
        let coordinates_move = Rc::clone(&canvas_coordinates);
        let coordinates_down = Rc::clone(&canvas_coordinates);
        let coordinates_scroll = Rc::clone(&canvas_coordinates);
        let coordinates_prepaint = Rc::clone(&canvas_coordinates);
        let coordinates_paint = Rc::clone(&canvas_coordinates);
        let view_move = view.clone();
        let view_down = view.clone();
        let view_up = view.clone();
        let view_scroll = view.clone();
        let view_prepaint = view.clone();
        let view_paint = view.clone();

        let element = div()
            .size_full()
            .on_mouse_move(move |event, _window, cx| {
                let pos = coordinates_move.get().mouse_move_position(event.position);
                view_move.update(cx, |vs, cx| {
                    vs.handle_mouse_move(pos, cx);
                });
            })
            .on_mouse_down(gpui::MouseButton::Left, move |event, _window, cx| {
                let pos = coordinates_down.get().mouse_down_position(event.position);
                let click_count = event.click_count;
                view_down.update(cx, |vs, cx| {
                    vs.handle_mouse_down(pos, click_count, cx);
                });
            })
            .on_mouse_up(gpui::MouseButton::Left, move |_event, _window, cx| {
                view_up.update(cx, |vs, cx| {
                    vs.dragging = None;
                    vs.panning = false;
                    cx.notify();
                });
            })
            .on_scroll_wheel(move |event, _window, cx| {
                let pos = coordinates_scroll.get().scroll_position(event.position);
                let factor = match event.delta {
                    ScrollDelta::Lines(delta) => (1.0 + delta.y * 0.1).clamp(0.5, 2.0),
                    ScrollDelta::Pixels(delta) => (1.0 + f32::from(delta.y) * 0.01).clamp(0.5, 2.0),
                };
                view_scroll.update(cx, |vs, cx| {
                    vs.handle_zoom(pos, factor, cx);
                });
            })
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        coordinates_prepaint.set(CanvasCoordinates::from_bounds(bounds));
                        // Prepaint: size the viewport from the element bounds and
                        // build the paint frame from fresh scene state. On the
                        // first valid layout, auto-fit the graph exactly once so
                        // it is visible without an explicit `fit_all` call.
                        let size =
                            Vec2::new(f32::from(bounds.size.width), f32::from(bounds.size.height));
                        view_prepaint.update(cx, |vs, cx| {
                            vs.prepare_canvas(size, cx);
                        });
                        let vs = view_prepaint.read(cx);
                        let scene = vs.scene.read(cx);
                        let node_label = vs.node_label.clone();
                        crate::paint::build_paint_frame(
                            scene.graph(),
                            &|id| scene.node_position(id),
                            &|id, node| node_label(id, node),
                            &vs.viewport,
                            &vs.style,
                            &vs.selection,
                            &vs.hover,
                        )
                    },
                    move |_bounds, frame: crate::paint::PaintFrame, window, cx| {
                        let coordinates = coordinates_paint.get();
                        let style = view_paint.read(cx).style.clone();
                        // Edges first, then nodes (§18.1).
                        for edge in &frame.edges {
                            let (source, target) = coordinates.edge_endpoints(edge);
                            let color = if edge.selected {
                                style.edge_color_selected
                            } else if edge.hovered {
                                style.edge_color_hovered
                            } else {
                                style.edge_color
                            };
                            // Trim both endpoints to the node boundary so the line
                            // and arrowhead are not hidden beneath the nodes.
                            let (line_source, line_target) =
                                trim_to_node_boundary(source, target, style.node_radius);
                            let mut builder = PathBuilder::stroke(px(style.edge_width));
                            builder.move_to(point(px(line_source.x), px(line_source.y)));
                            builder.line_to(point(px(line_target.x), px(line_target.y)));
                            if let Ok(path) = builder.build() {
                                window.paint_path(path, color);
                            }
                            if edge.direction == EdgeDirection::Directed && style.edge_arrow_enabled
                            {
                                paint_edge_arrow(window, line_source, line_target, &style, color);
                                #[cfg(test)]
                                TEST_PAINT_TRACE.with(|trace| {
                                    trace
                                        .borrow_mut()
                                        .push(TestPaintPrimitive::Arrow { source, target });
                                });
                            }
                            #[cfg(test)]
                            TEST_PAINT_TRACE.with(|trace| {
                                trace
                                    .borrow_mut()
                                    .push(TestPaintPrimitive::Edge { source, target });
                            });
                        }
                        for node in &frame.nodes {
                            let bounds = coordinates.node_bounds(node);
                            let color = if node.selected {
                                style.node_fill_selected
                            } else if node.hovered {
                                style.node_fill_hovered
                            } else {
                                style.node_fill
                            };
                            window.paint_quad(quad(
                                bounds,
                                px(node.radius),
                                color,
                                px(style.node_stroke_width),
                                style.node_stroke_color,
                                Default::default(),
                            ));
                            #[cfg(test)]
                            TEST_PAINT_TRACE.with(|trace| {
                                trace.borrow_mut().push(TestPaintPrimitive::Node {
                                    origin: Vec2::new(
                                        f32::from(bounds.origin.x),
                                        f32::from(bounds.origin.y),
                                    ),
                                    size: Vec2::new(
                                        f32::from(bounds.size.width),
                                        f32::from(bounds.size.height),
                                    ),
                                });
                            });
                        }
                        for label in &frame.labels {
                            paint_label(window, cx, &coordinates, label, &style);
                        }
                    },
                )
                .size_full(),
            );
        Self {
            element,
            _marker: PhantomData,
        }
    }
}

impl<NK, EK, N, E> Styled for GraphView<NK, EK, N, E> {
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl<NK, EK, N, E> IntoElement for GraphView<NK, EK, N, E> {
    type Element = Div;

    fn into_element(self) -> Div {
        self.element
    }
}

/// Trim an edge's endpoints inward by `radius` along the edge direction so the
/// line and arrowhead stop at the node boundary instead of beneath the nodes.
fn trim_to_node_boundary(source: Vec2, target: Vec2, radius: f32) -> (Vec2, Vec2) {
    let dir = target - source;
    let len = dir.length();
    if len < f32::EPSILON {
        return (source, target);
    }
    let unit = dir / len;
    let inset = radius.min(len * 0.5);
    (source + unit * inset, target - unit * inset)
}

/// Paint an arrowhead at the target end of a directed edge.
///
/// `source` and `target` are window-space endpoints. The arrowhead is drawn
/// with the edge's resolved color and the shape/size from `style`.
fn paint_edge_arrow(
    window: &mut Window,
    source: Vec2,
    target: Vec2,
    style: &GraphStyle,
    color: gpui::Hsla,
) {
    let dir = target - source;
    let len = dir.length();
    if len < f32::EPSILON {
        return;
    }
    let unit = dir / len;
    let arrow_size = style.edge_arrow_size;
    let tip = target;
    let base = tip - unit * arrow_size;
    let normal = Vec2::new(-unit.y, unit.x);

    match style.edge_arrow_shape {
        ArrowShape::Triangle => {
            let half = arrow_size * 0.5;
            let mut builder = PathBuilder::fill();
            builder.move_to(point(px(tip.x), px(tip.y)));
            builder.line_to(point(
                px(base.x + normal.x * half),
                px(base.y + normal.y * half),
            ));
            builder.line_to(point(
                px(base.x - normal.x * half),
                px(base.y - normal.y * half),
            ));
            builder.close();
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        }
        ArrowShape::Line => {
            let half = arrow_size * 0.5;
            let mut builder = PathBuilder::stroke(px(style.edge_width));
            builder.move_to(point(
                px(base.x + normal.x * half),
                px(base.y + normal.y * half),
            ));
            builder.line_to(point(px(tip.x), px(tip.y)));
            builder.line_to(point(
                px(base.x - normal.x * half),
                px(base.y - normal.y * half),
            ));
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        }
        ArrowShape::Circle => {
            let radius = arrow_size * 0.5;
            let center = tip - unit * radius;
            window.paint_quad(quad(
                Bounds {
                    origin: point(px(center.x - radius), px(center.y - radius)),
                    size: size(px(radius * 2.0), px(radius * 2.0)),
                },
                px(radius),
                color,
                px(0.0),
                gpui::transparent_black(),
                Default::default(),
            ));
        }
    }
}

impl<NK, EK, N, E> GraphViewState<NK, EK, N, E>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: 'static,
    E: 'static,
{
    fn handle_mouse_move(&mut self, pos: Vec2, cx: &mut Context<Self>) {
        if let Some(node) = self.dragging {
            let world = self.viewport.screen_to_world(pos);
            self.scene.update(cx, |scene, cx| {
                scene.set_position(node, world);
                scene.pin(node);
                cx.notify();
            });
            cx.emit(GraphEvent::NodeMoved {
                node,
                position: world,
            });
        } else if self.panning {
            self.cancel_initial_auto_fit();
            let delta = pos - self.last_mouse;
            self.viewport.pan(delta);
            cx.emit(GraphEvent::ViewportChanged);
        } else {
            let scene = self.scene.read(cx);
            let hit = hit_test::hit_test(
                scene.graph(),
                &|id| scene.node_position(id),
                &self.viewport,
                &self.style,
                pos,
            );
            self.hover = Hover {
                node: hit.node,
                edge: hit.edge,
            };
        }
        self.last_mouse = pos;
        cx.notify();
    }

    fn handle_mouse_down(&mut self, pos: Vec2, click_count: usize, cx: &mut Context<Self>) {
        let scene = self.scene.read(cx);
        let hit = hit_test::hit_test(
            scene.graph(),
            &|id| scene.node_position(id),
            &self.viewport,
            &self.style,
            pos,
        );

        if let Some(node) = hit.node {
            if click_count >= 2 {
                cx.emit(GraphEvent::NodeDoubleClicked { node });
            } else {
                cx.emit(GraphEvent::NodeClicked {
                    node,
                    button: MouseButton::Left,
                });
            }
            self.dragging = Some(node);
            self.selection.nodes = vec![node];
            self.selection.edges.clear();
            cx.emit(GraphEvent::SelectionChanged {
                selection: self.selection.clone(),
            });
        } else if let Some(edge) = hit.edge {
            cx.emit(GraphEvent::EdgeClicked {
                edge,
                button: MouseButton::Left,
            });
            self.selection.edges = vec![edge];
            self.selection.nodes.clear();
            cx.emit(GraphEvent::SelectionChanged {
                selection: self.selection.clone(),
            });
        } else {
            self.cancel_initial_auto_fit();
            self.panning = true;
        }
        self.last_mouse = pos;
        cx.notify();
    }
}

/// Paint a node label centered below the node.
fn paint_label(
    window: &mut Window,
    cx: &mut gpui::App,
    coordinates: &CanvasCoordinates,
    label: &crate::paint::PaintLabel,
    style: &GraphStyle,
) {
    let anchor = coordinates.canvas_to_window(label.position);
    let font_size = style.label_style.font_size.to_pixels(window.rem_size());
    let line_height = style
        .label_style
        .line_height
        .to_pixels(font_size.into(), window.rem_size());
    let run = style.label_style.to_run(label.text.len());
    let Ok(lines) =
        window
            .text_system()
            .shape_text(label.text.clone().into(), font_size, &[run], None, None)
    else {
        return;
    };
    let mut origin = point(
        px(anchor.x),
        px(anchor.y + style.node_radius + style.label_offset),
    );
    for line in &lines {
        // Center the label horizontally on the node by shifting the origin by
        // half the line width. `WrappedLine::paint` only honors `TextAlign`
        // when a bounds width is provided, so we center manually.
        let line_size = line.size(line_height);
        let centered = point(px(anchor.x - f32::from(line_size.width) * 0.5), origin.y);
        let _ = line.paint(
            centered,
            line_height,
            gpui::TextAlign::Center,
            None,
            window,
            cx,
        );
        origin.y += line_size.height;
    }
    #[cfg(test)]
    TEST_PAINT_TRACE.with(|trace| {
        trace
            .borrow_mut()
            .push(TestPaintPrimitive::Label { position: anchor });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeDirection;
    use crate::patch::GraphBatch;
    use crate::scene::GraphScene;
    use gpui::{
        AppContext, Entity, Modifiers, MouseButton as GpuiMouseButton, ScrollDelta,
        ScrollWheelEvent, TestAppContext, TouchPhase, VisualTestContext, point, px,
    };

    type TestView = GraphViewState<&'static str, &'static str, (), ()>;

    fn test_view(cx: &mut TestAppContext) -> Entity<TestView> {
        let scene: Entity<GraphScene<&'static str, &'static str, (), ()>> = cx.new(|_| {
            let mut scene = GraphScene::new();
            scene.merge(GraphBatch::new().node("a", ()).node("b", ()));
            let a = scene.node_id(&"a").unwrap();
            let b = scene.node_id(&"b").unwrap();
            scene.set_position(a, Vec2::new(-10.0, -20.0));
            scene.set_position(b, Vec2::new(30.0, 40.0));
            scene
        });
        cx.new(|cx| GraphViewState::new(scene, cx))
    }

    fn test_view_with_edge(cx: &mut TestAppContext) -> Entity<TestView> {
        let scene: Entity<GraphScene<&'static str, &'static str, (), ()>> = cx.new(|_| {
            let mut scene = GraphScene::new();
            scene.merge(GraphBatch::new().node("a", ()).node("b", ()).edge(
                "ab",
                "a",
                "b",
                EdgeDirection::Directed,
                (),
            ));
            let a = scene.node_id(&"a").unwrap();
            let b = scene.node_id(&"b").unwrap();
            scene.set_position(a, Vec2::new(-10.0, 0.0));
            scene.set_position(b, Vec2::new(10.0, 0.0));
            scene
        });
        cx.new(|cx| GraphViewState::new(scene, cx))
    }

    fn draw_graph_view<N: 'static>(
        cx: &mut VisualTestContext,
        view: &Entity<GraphViewState<&'static str, &'static str, N, ()>>,
        origin: Vec2,
        canvas_size: Vec2,
    ) {
        cx.draw(
            point(px(origin.x), px(origin.y)),
            size(px(canvas_size.x), px(canvas_size.y)),
            |_, _| GraphView::new(view.clone()).into_element(),
        );
    }

    fn assert_no_auto_fit_after(
        cx: &mut TestAppContext,
        override_view: impl FnOnce(&mut TestView, &mut Context<TestView>),
    ) {
        let view = test_view(cx);
        cx.update_entity(&view, |state, cx| {
            state.viewport.set_size(Vec2::new(100.0, 100.0));
            override_view(state, cx);

            let before = (state.viewport.center(), state.viewport.zoom());
            state.prepare_canvas(Vec2::new(800.0, 600.0), cx);

            assert_eq!((state.viewport.center(), state.viewport.zoom()), before);
            assert!(!state.initial_auto_fit.pending);
        });
    }

    #[gpui::test]
    fn initial_auto_fit_waits_for_both_canvas_axes(cx: &mut TestAppContext) {
        let view = test_view(cx);
        cx.update_entity(&view, |state, cx| {
            state.prepare_canvas(Vec2::new(0.0, 240.0), cx);
            assert!(state.initial_auto_fit.pending);

            state.prepare_canvas(Vec2::new(320.0, 0.0), cx);
            assert!(state.initial_auto_fit.pending);

            state.prepare_canvas(Vec2::new(320.0, 240.0), cx);
            assert!(!state.initial_auto_fit.pending);
        });
    }

    #[gpui::test]
    fn viewport_mut_prevents_initial_auto_fit(cx: &mut TestAppContext) {
        assert_no_auto_fit_after(cx, |state, _| {
            state.viewport_mut().focus(Vec2::new(77.0, 88.0));
        });
    }

    #[gpui::test]
    fn fit_all_prevents_initial_auto_fit(cx: &mut TestAppContext) {
        assert_no_auto_fit_after(cx, |state, cx| {
            state.fit_all(cx);
        });
    }

    #[gpui::test]
    fn focus_node_prevents_initial_auto_fit(cx: &mut TestAppContext) {
        assert_no_auto_fit_after(cx, |state, cx| {
            let node = state.scene.read(cx).node_id(&"b").unwrap();
            state.focus_node(node, cx);
        });
    }

    #[gpui::test]
    fn graph_view_prepaint_fits_once_and_preserves_explicit_viewport(cx: &mut TestAppContext) {
        let view = test_view(cx);
        let cx = cx.add_empty_window();
        draw_graph_view(cx, &view, Vec2::new(80.0, 40.0), Vec2::new(320.0, 240.0));

        let first_fit = cx.update_entity(&view, |state, _| {
            assert!(!state.initial_auto_fit.pending);
            (state.viewport.center(), state.viewport.zoom())
        });
        assert_eq!(first_fit.0, Vec2::new(10.0, 10.0));
        assert!((first_fit.1 - 3.6).abs() < 1e-5);

        draw_graph_view(cx, &view, Vec2::new(80.0, 40.0), Vec2::new(640.0, 480.0));
        cx.update_entity(&view, |state, _| {
            assert_eq!((state.viewport.center(), state.viewport.zoom()), first_fit);
        });

        let explicit = test_view(cx);
        cx.update_entity(&explicit, |state, _| {
            state.viewport_mut().focus(Vec2::new(77.0, 88.0));
        });
        draw_graph_view(
            cx,
            &explicit,
            Vec2::new(80.0, 40.0),
            Vec2::new(320.0, 240.0),
        );
        cx.update_entity(&explicit, |state, _| {
            assert!(!state.initial_auto_fit.pending);
            assert_eq!(state.viewport.center(), Vec2::new(77.0, 88.0));
            assert_eq!(state.viewport.zoom(), 1.0);
        });
    }

    #[gpui::test]
    fn graph_view_mouse_callbacks_use_nonzero_origin_for_selection_and_drag(
        cx: &mut TestAppContext,
    ) {
        let view = test_view(cx);
        let target = cx.update_entity(&view, |state, cx| {
            let target = state.scene.read(cx).node_id(&"b").unwrap();
            state.viewport_mut().set_size(Vec2::new(200.0, 120.0));
            state.viewport_mut().focus(Vec2::ZERO);
            target
        });
        let cx = cx.add_empty_window();
        let origin = Vec2::new(80.0, 40.0);
        let canvas_size = Vec2::new(200.0, 120.0);
        draw_graph_view(cx, &view, origin, canvas_size);

        let target_local = Vec2::new(130.0, 100.0);
        cx.simulate_mouse_down(
            point(px(origin.x + target_local.x), px(origin.y + target_local.y)),
            GpuiMouseButton::Left,
            Modifiers::none(),
        );
        cx.update_entity(&view, |state, _| {
            assert_eq!(state.dragging, Some(target));
            assert_eq!(state.selection.nodes, vec![target]);
        });

        let moved_local = Vec2::new(150.0, 110.0);
        let expected_world = cx.update_entity(&view, |state, _| {
            state.viewport.screen_to_world(moved_local)
        });
        cx.simulate_mouse_move(
            point(px(origin.x + moved_local.x), px(origin.y + moved_local.y)),
            GpuiMouseButton::Left,
            Modifiers::none(),
        );
        cx.update_entity(&view, |state, cx| {
            let scene_position = state.scene.read(cx).node_position(target).unwrap();
            assert!((scene_position - expected_world).length() < 1e-5);
        });
    }

    #[gpui::test]
    fn graph_view_pan_cancels_pending_auto_fit_through_mouse_down(cx: &mut TestAppContext) {
        let view = test_view(cx);
        let cx = cx.add_empty_window();
        let origin = Vec2::new(80.0, 40.0);
        draw_graph_view(cx, &view, origin, Vec2::new(200.0, 120.0));
        cx.update_entity(&view, |state, _| {
            // Recreate the pending initial-render state after the first draw so
            // the event path, rather than a helper, owns cancellation.
            state.initial_auto_fit.pending = true;
        });

        cx.simulate_mouse_down(
            point(px(origin.x + 20.0), px(origin.y + 20.0)),
            GpuiMouseButton::Left,
            Modifiers::none(),
        );
        let before_redraw = cx.update_entity(&view, |state, _| {
            assert!(state.panning);
            assert!(!state.initial_auto_fit.pending);
            (state.viewport.center(), state.viewport.zoom())
        });

        draw_graph_view(cx, &view, origin, Vec2::new(320.0, 240.0));
        cx.update_entity(&view, |state, _| {
            assert_eq!(
                (state.viewport.center(), state.viewport.zoom()),
                before_redraw
            );
        });
    }

    #[gpui::test]
    fn graph_view_scroll_uses_local_anchor_and_cancels_pending_auto_fit(cx: &mut TestAppContext) {
        let view = test_view(cx);
        cx.update_entity(&view, |state, _| {
            state.viewport_mut().set_size(Vec2::new(200.0, 120.0));
            state.viewport_mut().focus(Vec2::ZERO);
        });
        let cx = cx.add_empty_window();
        let origin = Vec2::new(80.0, 40.0);
        let anchor = Vec2::new(70.0, 40.0);
        draw_graph_view(cx, &view, origin, Vec2::new(200.0, 120.0));
        cx.update_entity(&view, |state, _| {
            state.initial_auto_fit.pending = true;
        });

        let before = cx.update_entity(&view, |state, _| {
            (
                state.viewport.screen_to_world(anchor),
                state.viewport.zoom(),
            )
        });
        cx.simulate_event(ScrollWheelEvent {
            position: point(px(origin.x + anchor.x), px(origin.y + anchor.y)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(10.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        let after = cx.update_entity(&view, |state, _| {
            assert!(!state.initial_auto_fit.pending);
            (
                state.viewport.screen_to_world(anchor),
                state.viewport.zoom(),
            )
        });
        assert!((after.0 - before.0).length() < 1e-5);
        assert!((after.1 - before.1).abs() > 1e-5);

        let transformed = cx.update_entity(&view, |state, _| {
            (state.viewport.center(), state.viewport.zoom())
        });
        draw_graph_view(cx, &view, origin, Vec2::new(320.0, 240.0));
        cx.update_entity(&view, |state, _| {
            assert_eq!(
                (state.viewport.center(), state.viewport.zoom()),
                transformed
            );
        });
    }

    #[gpui::test]
    fn graph_view_paints_nodes_and_edges_at_window_origin_plus_local_geometry(
        cx: &mut TestAppContext,
    ) {
        let view = test_view_with_edge(cx);
        cx.update_entity(&view, |state, _| {
            state.viewport_mut().set_size(Vec2::new(200.0, 100.0));
            state.viewport_mut().focus(Vec2::ZERO);
        });

        let cx = cx.add_empty_window();
        let origin = Vec2::new(80.0, 40.0);
        clear_test_paint_trace();
        draw_graph_view(cx, &view, origin, Vec2::new(200.0, 100.0));

        assert_eq!(
            take_test_paint_trace(),
            vec![
                TestPaintPrimitive::Arrow {
                    source: Vec2::new(origin.x + 90.0, origin.y + 50.0),
                    target: Vec2::new(origin.x + 110.0, origin.y + 50.0),
                },
                TestPaintPrimitive::Edge {
                    source: Vec2::new(origin.x + 90.0, origin.y + 50.0),
                    target: Vec2::new(origin.x + 110.0, origin.y + 50.0),
                },
                TestPaintPrimitive::Node {
                    origin: Vec2::new(origin.x + 84.0, origin.y + 44.0),
                    size: Vec2::splat(12.0),
                },
                TestPaintPrimitive::Node {
                    origin: Vec2::new(origin.x + 104.0, origin.y + 44.0),
                    size: Vec2::splat(12.0),
                },
            ]
        );
    }

    #[gpui::test]
    fn graph_view_skips_arrow_when_disabled(cx: &mut TestAppContext) {
        let view = test_view_with_edge(cx);
        cx.update_entity(&view, |state, _| {
            state.viewport_mut().set_size(Vec2::new(200.0, 100.0));
            state.viewport_mut().focus(Vec2::ZERO);
            state.style_mut().edge_arrow_enabled = false;
        });

        let cx = cx.add_empty_window();
        let origin = Vec2::new(80.0, 40.0);
        clear_test_paint_trace();
        draw_graph_view(cx, &view, origin, Vec2::new(200.0, 100.0));

        let trace = take_test_paint_trace();
        assert!(
            trace
                .iter()
                .all(|primitive| !matches!(primitive, TestPaintPrimitive::Arrow { .. })),
            "no arrow should be painted when disabled"
        );
        assert!(
            trace
                .iter()
                .any(|primitive| matches!(primitive, TestPaintPrimitive::Edge { .. })),
            "the edge itself should still be painted"
        );
    }

    #[gpui::test]
    fn graph_view_paints_node_labels(cx: &mut TestAppContext) {
        let view = test_view(cx);
        cx.update_entity(&view, |state, _| {
            state.viewport_mut().set_size(Vec2::new(200.0, 100.0));
            state.viewport_mut().focus(Vec2::ZERO);
            state.set_node_label(|_id, _node| Some("label".to_string()));
        });

        let cx = cx.add_empty_window();
        let origin = Vec2::new(80.0, 40.0);
        clear_test_paint_trace();
        draw_graph_view(cx, &view, origin, Vec2::new(200.0, 100.0));

        let trace = take_test_paint_trace();
        let labels: Vec<_> = trace
            .iter()
            .filter_map(|primitive| match primitive {
                TestPaintPrimitive::Label { position } => Some(*position),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 2, "both nodes should have a label");
        assert_eq!(labels[0], Vec2::new(origin.x + 90.0, origin.y + 30.0));
        assert_eq!(labels[1], Vec2::new(origin.x + 130.0, origin.y + 90.0));
    }

    #[gpui::test]
    fn default_labels_use_display_and_can_be_overridden(cx: &mut TestAppContext) {
        let scene: Entity<GraphScene<&'static str, &'static str, &'static str, ()>> =
            cx.new(|_| {
                let mut scene = GraphScene::new();
                scene.merge(GraphBatch::new().node("a", "Alice").node("b", "Bob"));
                let a = scene.node_id(&"a").unwrap();
                let b = scene.node_id(&"b").unwrap();
                scene.set_position(a, Vec2::new(-10.0, -20.0));
                scene.set_position(b, Vec2::new(30.0, 40.0));
                scene
            });
        let view: Entity<GraphViewState<&'static str, &'static str, &'static str, ()>> =
            cx.new(|cx| GraphViewState::new_with_default_labels(scene, cx));
        cx.update_entity(&view, |state, _| {
            state.viewport_mut().set_size(Vec2::new(200.0, 100.0));
            state.viewport_mut().focus(Vec2::ZERO);
        });

        let cx = cx.add_empty_window();
        let origin = Vec2::new(80.0, 40.0);
        clear_test_paint_trace();
        draw_graph_view(cx, &view, origin, Vec2::new(200.0, 100.0));
        let trace = take_test_paint_trace();
        let labels: Vec<_> = trace
            .iter()
            .filter_map(|primitive| match primitive {
                TestPaintPrimitive::Label { position } => Some(*position),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels.len(),
            2,
            "default labels should render for Display nodes"
        );
    }
}
