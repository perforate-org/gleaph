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
    Bounds, Context, Div, Entity, EventEmitter, FillOptions, FillRule, InteractiveElement,
    IntoElement, ParentElement, PathBuilder, PathStyle, ScrollDelta, StyleRefinement, Styled,
    Window, canvas, div, point, px, quad, size,
};
use std::{cell::Cell, marker::PhantomData, rc::Rc};

#[cfg(test)]
use std::cell::RefCell;

use crate::graph::{EdgeDirection, EdgeId, NodeId};
use crate::hit_test;
use crate::interaction::{GraphEvent, Hover, MouseButton, Selection};
use crate::layout::LayoutBudget;
use crate::scene::GraphScene;
use crate::style::{ArrowShape, GraphStyle};
use crate::viewport::{Viewport, WorldBounds};

/// A resolver that returns the label text for a node, or `None` for no label.
type NodeLabelResolver<N> = Rc<dyn Fn(NodeId, &N) -> Option<String>>;

/// A resolver that returns the label text for an edge, or `None` for no label.
type EdgeLabelResolver<E> = Rc<dyn Fn(EdgeId, &E) -> Option<String>>;

/// A resolver that returns a node's query-overlay category.
type NodeOverlayResolver = Rc<crate::paint::NodeOverlay>;

/// A resolver that returns an edge's query-overlay category.
type EdgeOverlayResolver = Rc<crate::paint::EdgeOverlay>;

/// The state of a particular view into a graph scene (§16).
pub struct GraphViewState<NK, EK, N, E, S = crate::hash::DefaultBuildHasher>
where
    S: std::hash::BuildHasher + Default + Clone,
{
    scene: Entity<GraphScene<NK, EK, N, E, S>>,
    viewport: Viewport,
    selection: Selection,
    hover: Hover,
    style: GraphStyle,
    runtime: crate::runtime::GraphRuntime<S>,
    node_label: NodeLabelResolver<N>,
    edge_label: EdgeLabelResolver<E>,
    node_overlay: NodeOverlayResolver,
    edge_overlay: EdgeOverlayResolver,
    dragging: Option<NodeId>,
    panning: bool,
    last_mouse: Vec2,
    /// When interaction-time LOD is enabled, the time of the last pan or zoom
    /// event. Used to keep the straight-line threshold elevated while the camera
    /// is moving and for a short settle period afterward, so per-edge curve work
    /// is skipped during interaction and detail settles back without popping.
    /// `None` when interaction-time LOD is disabled or currently settled.
    interaction_active_since: Option<web_time::Instant>,
    /// A handle to the scheduled settle task that re-evaluates the straight-line
    /// threshold after the interaction settle period elapses. Dropping it
    /// cancels the pending settle. `None` when no settle is scheduled.
    interaction_settle_task: Option<gpui::Task<()>>,
    initial_auto_fit: InitialAutoFitState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// One-time default initial fit, armed when the view is created.
///
/// Two facts gate the fit:
///
/// - The canvas must have reported a non-empty size (`note_canvas_size`),
///   because fitting against a zero-sized viewport collapses the graph.
/// - Input-driven cancellation (`cancel_on_input`) is honored only after such
///   a frame existed. Earlier events cannot express camera intent — the
///   viewport has no pixel size, so world/screen math is degenerate — and
///   must not consume the fit, or the graph stays unfitted permanently.
struct InitialAutoFitState {
    pending: bool,
    canvas_sized: bool,
}

impl InitialAutoFitState {
    fn cancel(&mut self) {
        self.pending = false;
    }

    fn cancel_on_input(&mut self) {
        if self.canvas_sized {
            self.pending = false;
        }
    }

    fn note_canvas_size(&mut self, size: Vec2) {
        if size.x > 0.0 && size.y > 0.0 {
            self.canvas_sized = true;
        }
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
        Self {
            pending: true,
            canvas_sized: false,
        }
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

    /// Convert an edge's canvas-local path to window-space coordinates.
    fn edge_path_window(self, edge: &crate::paint::PaintEdge) -> Vec<crate::paint::Bezier> {
        edge.path
            .iter()
            .map(|(p0, p1, p2)| {
                (
                    self.canvas_to_window(*p0),
                    self.canvas_to_window(*p1),
                    self.canvas_to_window(*p2),
                )
            })
            .collect()
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
    EdgeLabel { position: Vec2 },
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

impl<NK, EK, N, E, S> GraphViewState<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: 'static,
    E: 'static,
    S: std::hash::BuildHasher + Default + Clone + 'static,
{
    /// Create a view state over the given scene.
    pub fn new(scene: Entity<GraphScene<NK, EK, N, E, S>>, _cx: &mut Context<Self>) -> Self {
        Self {
            scene,
            viewport: Viewport::new(),
            selection: Selection::new(),
            hover: Hover::default(),
            style: GraphStyle::default(),
            runtime: crate::runtime::GraphRuntime::default(),
            node_label: Rc::new(|_id, _node| None),
            edge_label: Rc::new(|_id, _edge| None),
            node_overlay: Rc::new(|_id| crate::paint::OverlayCategory::None),
            edge_overlay: Rc::new(|_id| crate::paint::OverlayCategory::None),
            dragging: None,
            panning: false,
            last_mouse: Vec2::ZERO,
            interaction_active_since: None,
            interaction_settle_task: None,
            initial_auto_fit: InitialAutoFitState::default(),
        }
    }

    /// The scene this view observes.
    pub fn scene(&self) -> &Entity<GraphScene<NK, EK, N, E, S>> {
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

    /// The style to use when painting this frame, applying interaction-time LOD.
    ///
    /// When interaction-time LOD is enabled ([`GraphStyle::edge_straight_threshold_while_interacting`])
    /// and an interaction is active or within its settle window, the returned
    /// style uses the elevated straight-line threshold so the current frame skips
    /// per-edge curve work. Otherwise the idle [`Self::style`] is returned. The
    /// threshold is the only field that differs, so painting, hit testing, and
    /// label logic stay consistent with the style the caller configured.
    pub(crate) fn paint_style(&self) -> GraphStyle {
        let mut style = self.style.clone();
        if let Some(threshold) = self.interaction_straight_threshold() {
            style.edge_straight_threshold = threshold;
        }
        style
    }

    /// The straight-line threshold to use right now, accounting for interaction
    /// LOD: `Some(elevated)` while an interaction is active or within its settle
    /// window, `None` to use the idle [`Self::style`] threshold.
    fn interaction_straight_threshold(&self) -> Option<f32> {
        if self.interaction_active_since.is_none()
            || self.style.edge_straight_threshold_while_interacting <= 0.0
        {
            return None;
        }
        Some(self.style.edge_straight_threshold_while_interacting)
    }

    /// Set the node label resolver.
    ///
    /// The resolver returns the label text for a node, or `None` to render no
    /// label. It is called during prepaint for every visible node. This
    /// overrides any default label resolution.
    pub fn set_node_label(&mut self, resolver: impl Fn(NodeId, &N) -> Option<String> + 'static) {
        self.node_label = Rc::new(resolver);
    }

    /// Set the edge label resolver.
    ///
    /// The resolver returns the label text for an edge, or `None` to render no
    /// label. It is called during prepaint for every visible edge. This
    /// overrides any default edge label resolution.
    pub fn set_edge_label(&mut self, resolver: impl Fn(EdgeId, &E) -> Option<String> + 'static) {
        self.edge_label = Rc::new(resolver);
    }

    /// Set the node overlay resolver.
    ///
    /// The resolver returns the query-overlay category for a node, or
    /// [`OverlayCategory::None`](crate::paint::OverlayCategory::None) to keep the
    /// base style. This is independent of selection and hover; a result node the
    /// user also selects renders both states simultaneously.
    pub fn set_node_overlay(
        &mut self,
        resolver: impl Fn(NodeId) -> crate::paint::OverlayCategory + 'static,
    ) {
        self.node_overlay = Rc::new(resolver);
    }

    /// Set the edge overlay resolver.
    ///
    /// The resolver returns the query-overlay category for an edge, or `None` to
    /// keep the base style. This is independent of selection and hover.
    pub fn set_edge_overlay(
        &mut self,
        resolver: impl Fn(EdgeId) -> crate::paint::OverlayCategory + 'static,
    ) {
        self.edge_overlay = Rc::new(resolver);
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
        self.initial_auto_fit.note_canvas_size(size);
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

    /// Cancel the pending initial fit because user input asked for the camera.
    ///
    /// Input that arrives before any laid-out frame has sized the canvas is
    /// ignored as an override signal: with no pixel size yet, the event cannot
    /// express a meaningful camera operation, so consuming the one-time fit
    /// would leave the graph unfitted forever instead of honoring intent.
    fn cancel_initial_auto_fit_on_input(&mut self) {
        self.initial_auto_fit.cancel_on_input();
    }

    /// Record the start of a pan or zoom interaction and schedule the settle.
    ///
    /// When interaction-time LOD is enabled, this elevates the straight-line
    /// threshold so the current frame renders every eligible edge as a cheap
    /// straight segment, and schedules a settle task to re-evaluate the threshold
    /// after [`GraphStyle::edge_settle_time_ms`] so detail does not pop back the
    /// instant the camera stops. Repeated events cancel and reschedule the settle,
    /// so the low-detail threshold persists while the camera keeps moving.
    fn begin_interaction(&mut self, cx: &mut Context<Self>) {
        let style = &self.style;
        if style.edge_straight_threshold_while_interacting <= 0.0
            && style.edge_settle_time_ms <= 0.0
        {
            return;
        }
        let settle_ms = style.edge_settle_time_ms;
        let settle_duration = std::time::Duration::from_millis(settle_ms.max(0.0) as u64);
        let task = cx.spawn(
            move |view: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                // Clone the async app so the settle future owns it across the
                // await, satisfying the 'static bound on the spawned task.
                let cx = cx.clone();
                async move {
                    cx.background_executor().timer(settle_duration).await;
                    // If the view is still alive, clear the interaction state and repaint
                    // so the straight-line threshold settles back to the idle value. The
                    // weak handle avoids holding the entity hostage across the await.
                    view.update(&mut cx.clone(), |view, cx| {
                        view.interaction_active_since = None;
                        view.interaction_settle_task = None;
                        cx.notify();
                    })
                    .ok();
                }
            },
        );
        self.interaction_settle_task = Some(task);
        self.interaction_active_since = Some(cx.background_executor().now());
    }

    fn handle_zoom(&mut self, pos: Vec2, factor: f32, cx: &mut Context<Self>) {
        self.cancel_initial_auto_fit_on_input();
        self.begin_interaction(cx);
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

impl<NK, EK, N, E, S> GraphViewState<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: std::fmt::Display + 'static,
    E: std::fmt::Display + 'static,
    S: std::hash::BuildHasher + Default + Clone + 'static,
{
    /// Create a view state over the given scene with default node and edge labels.
    ///
    /// Each node's label is its `Display` representation and each edge's label
    /// is its `Display` representation. Callers can still override these per
    /// element with [`Self::set_node_label`] and [`Self::set_edge_label`].
    pub fn new_with_default_labels(
        scene: Entity<GraphScene<NK, EK, N, E, S>>,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            scene,
            viewport: Viewport::new(),
            selection: Selection::new(),
            hover: Hover::default(),
            style: GraphStyle::default(),
            runtime: crate::runtime::GraphRuntime::default(),
            node_label: Rc::new(|_id, node| Some(node.to_string())),
            edge_label: Rc::new(|_id, edge| Some(edge.to_string())),
            node_overlay: Rc::new(|_id| crate::paint::OverlayCategory::None),
            edge_overlay: Rc::new(|_id| crate::paint::OverlayCategory::None),
            dragging: None,
            panning: false,
            last_mouse: Vec2::ZERO,
            interaction_active_since: None,
            interaction_settle_task: None,
            initial_auto_fit: InitialAutoFitState::default(),
        }
    }
}

impl<NK, EK, N, E, S> EventEmitter<GraphEvent> for GraphViewState<NK, EK, N, E, S>
where
    NK: 'static,
    EK: 'static,
    N: 'static,
    E: 'static,
    S: std::hash::BuildHasher + Default + Clone + 'static,
{
}

/// A composable GPUI component that renders a graph view state (§27.4).
///
/// `GraphView` is a styled element: it participates in normal GPUI layout and
/// styling (e.g. `.size_full()`, `.border_1()`) and renders the graph through
/// GPUI's low-level canvas API.
pub struct GraphView<NK, EK, N, E, S = crate::hash::DefaultBuildHasher>
where
    S: std::hash::BuildHasher + Default + Clone,
{
    element: Div,
    #[allow(clippy::type_complexity)]
    _marker: PhantomData<fn(NK, EK, N, E, S)>,
}

impl<NK, EK, N, E, S> GraphView<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: 'static,
    E: 'static,
    S: std::hash::BuildHasher + Default + Clone + 'static,
{
    /// Create a graph view over the given view state.
    pub fn new(view: Entity<GraphViewState<NK, EK, N, E, S>>) -> Self {
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
                            let paint_style = vs.paint_style();
                            let scene_entity = vs.scene.clone();
                            let scene = scene_entity.read(cx);
                            let synced = scene.sync_runtime(&mut vs.runtime);
                            let node_label = vs.node_label.clone();
                            let edge_label = vs.edge_label.clone();
                            let node_overlay = vs.node_overlay.clone();
                            let edge_overlay = vs.edge_overlay.clone();
                            crate::paint::build_indexed_paint_frame(
                                crate::paint::IndexedPaintFrameInput {
                                    synced: &synced,
                                    node_label: &|id, node| node_label(id, node),
                                    edge_label: &|id, edge| edge_label(id, edge),
                                    viewport: &vs.viewport,
                                    style: &paint_style,
                                    selection: &vs.selection,
                                    hover: &vs.hover,
                                    node_overlay: Some(&move |id| node_overlay(id)),
                                    edge_overlay: Some(&move |id| edge_overlay(id)),
                                },
                            )
                        })
                    },
                    move |_bounds, mut frame: crate::paint::PaintFrame, window, cx| {
                        let coordinates = coordinates_paint.get();
                        let style = view_paint.read(cx).style.clone();
                        // The window-space visible rectangle, used to clip edges
                        // to the viewport so the tessellator never sees the huge
                        // coordinates of off-screen nodes (which would make it
                        // subdivide the curve excessively at deep zoom).
                        let viewport = view_paint.read(cx).viewport();
                        // Label anchors ride the same clamped screen radius
                        // the markers are drawn with.
                        let node_radius_screen = style.node_screen_radius(viewport.zoom());
                        let viewport_size = viewport.size();
                        let viewport_rect = Bounds {
                            origin: point(px(coordinates.origin.x), px(coordinates.origin.y)),
                            size: size(px(viewport_size.x), px(viewport_size.y)),
                        };
                        // Slide edge labels along their edges so they do not
                        // overlap each other or the (fixed) node labels, then
                        // compute the window-space bounds of every node and edge
                        // label so edges can be cut where they pass behind a
                        // label.
                        // Shape every label once per frame. Collision
                        // resolution slides labels along their paths but never
                        // changes their sizes, so these measurements serve the
                        // collision rects, the edge cut-outs, and painting
                        // alike instead of re-shaping per call.
                        let node_measures: Vec<Option<MeasuredLabel>> = frame
                            .labels
                            .iter()
                            .map(|label| measure_label(window, &label.text, &style))
                            .collect();
                        let mut edge_measures: Vec<Option<MeasuredLabel>> = frame
                            .edge_labels
                            .iter()
                            .map(|label| measure_label(window, &label.text, &style))
                            .collect();
                        let node_label_rects: Vec<Bounds<gpui::Pixels>> = frame
                            .labels
                            .iter()
                            .zip(&node_measures)
                            .filter_map(|(label, measured)| {
                                let measured = measured.as_ref()?;
                                Some(label_rect(measured, label.position, |anchor, _height| {
                                    anchor.y + node_radius_screen + style.label_offset
                                }))
                            })
                            .collect();
                        resolve_edge_label_collisions(
                            &mut frame,
                            &style,
                            &node_label_rects,
                            &edge_measures,
                        );
                        hide_edge_labels_near_nodes(&mut frame, &style, &mut edge_measures);
                        // Edge-label rects carry their owner edge so the
                        // stroke pass can drop a SELF-LOOP's own rect: the
                        // loop's label is parked beside the shape, and its
                        // mask reaching back over the loop would erase the
                        // loop's ink. Normal edges keep their own rect —
                        // that mask is what cuts a readable gap in the line
                        // under the label text. Owners ride beside the rects
                        // (not index alignment) so the mapping stays exact
                        // even when a label fails to measure and is skipped.
                        let mut edge_label_rects: Vec<(EdgeId, Bounds<gpui::Pixels>)> = Vec::new();
                        for (label, measured) in frame.edge_labels.iter().zip(&edge_measures) {
                            let Some(measured) = measured.as_ref() else {
                                continue;
                            };
                            let anchor = coordinates.canvas_to_window(
                                label.position + label.offset * style.label_offset,
                            );
                            let rect = label_rect(measured, anchor, |anchor, height| {
                                anchor.y - height * 0.5
                            });
                            edge_label_rects.push((label.edge, rect));
                        }
                        let mut label_rects: Vec<Bounds<gpui::Pixels>> =
                            edge_label_rects.iter().map(|(_, rect)| *rect).collect();
                        label_rects.extend(frame.labels.iter().zip(&node_measures).filter_map(
                            |(label, measured)| {
                                let measured = measured.as_ref()?;
                                let anchor = coordinates.canvas_to_window(label.position);
                                Some(label_rect(measured, anchor, |anchor, _height| {
                                    anchor.y + node_radius_screen + style.label_offset
                                }))
                            },
                        ));
                        // Edges first, then nodes (§18.1). Edge strokes
                        // accumulate into one path per resolved color so a
                        // large graph emits a handful of primitives instead of
                        // one per edge. Groups emit bottom-to-top (dimmed,
                        // base, query overlay, selected, hovered) so selection
                        // and hover stay legible over ordinary edges regardless
                        // of graph iteration order.
                        let mut strokes: Vec<EdgeStrokeBatch> = Vec::new();
                        // Triangle and circle arrowheads collect here and
                        // paint after the stroke groups. Triangles merge into
                        // one nonzero-winding fill per color (see
                        // append_triangle_arrow); circles are quads, which
                        // GPUI batches natively.
                        let mut solid_arrows: Vec<(gpui::Hsla, Vec2, Vec2)> = Vec::new();
                        for edge in &frame.edges {
                            // A selected/hovered edge keeps its interaction
                            // color on top of any query overlay so selection
                            // and hover stay legible; otherwise the overlay
                            // category selects the color.
                            let (color, layer) =
                                edge_color_layer(&style, edge.selected, edge.hovered, edge.overlay);
                            let path = coordinates.edge_path_window(edge);
                            let mut own_masks = Vec::new();
                            let masks =
                                stroke_masks(edge, &edge_label_rects, &label_rects, &mut own_masks);
                            append_edge_stroke(
                                &mut strokes,
                                color,
                                layer,
                                &path,
                                &style,
                                masks,
                                &viewport_rect,
                            );
                            if edge.direction == EdgeDirection::Directed
                                && style.edge_arrow_enabled
                                && !edge.omit_arrow
                            {
                                // The arrow sits at the end of the edge's path,
                                // pointing along the curve's tangent there. For
                                // a self-loop that is the onigiri's end; for any
                                // other edge it is the trimmed curve's end, just
                                // outside the target node.
                                let (_, p1, p2) = path.last().expect("edge has segments");
                                let dir = (*p2 - *p1).normalize();
                                let arrow_source = *p2 - dir * style.edge_arrow_size;
                                let arrow_target = *p2;
                                if style.edge_arrow_shape == ArrowShape::Line {
                                    // A line arrowhead is a stroke at the edge's
                                    // own width and color: it joins the batch.
                                    let builder = stroke_batch(&mut strokes, color, layer, &style);
                                    append_line_arrow(builder, arrow_source, arrow_target, &style);
                                } else {
                                    solid_arrows.push((color, arrow_source, arrow_target));
                                }
                                #[cfg(test)]
                                TEST_PAINT_TRACE.with(|trace| {
                                    trace.borrow_mut().push(TestPaintPrimitive::Arrow {
                                        source: arrow_source,
                                        target: arrow_target,
                                    });
                                });
                            }
                            #[cfg(test)]
                            TEST_PAINT_TRACE.with(|trace| {
                                let first = path.first().map(|(p0, _, _)| *p0);
                                let last = path.last().map(|(_, _, p2)| *p2);
                                if let (Some(source), Some(target)) = (first, last) {
                                    trace
                                        .borrow_mut()
                                        .push(TestPaintPrimitive::Edge { source, target });
                                }
                            });
                        }
                        strokes.sort_by_key(|batch| batch.layer);
                        for batch in strokes {
                            if let Ok(path) = batch.builder.build() {
                                window.paint_path(path, batch.color);
                            }
                        }
                        // Skip arrows entirely outside the viewport: each is a
                        // few pixels, so the bounding-box reject avoids handing
                        // the tessellator huge off-screen coordinates.
                        if style.edge_arrow_shape == ArrowShape::Triangle {
                            let mut fills: Vec<(gpui::Hsla, PathBuilder)> = Vec::new();
                            for (color, source, target) in &solid_arrows {
                                let Some(arrow) = TriangleArrow::new(*source, *target, &style)
                                else {
                                    continue;
                                };
                                if arrow_outside_viewport(
                                    arrow.tip,
                                    arrow.base,
                                    arrow.normal,
                                    arrow.half,
                                    &viewport_rect,
                                ) {
                                    continue;
                                }
                                match fills.iter_mut().find(|(c, _)| c == color) {
                                    Some((_, builder)) => {
                                        append_triangle_arrow(
                                            builder,
                                            &arrow,
                                            &style,
                                            &label_rects,
                                        );
                                    }
                                    None => {
                                        let mut builder = triangle_fill_builder();
                                        append_triangle_arrow(
                                            &mut builder,
                                            &arrow,
                                            &style,
                                            &label_rects,
                                        );
                                        fills.push((*color, builder));
                                    }
                                }
                            }
                            for (color, builder) in fills {
                                if let Ok(path) = builder.build() {
                                    window.paint_path(path, color);
                                }
                            }
                        } else {
                            debug_assert_eq!(
                                style.edge_arrow_shape,
                                ArrowShape::Circle,
                                "line arrowheads join the stroke batches"
                            );
                            for (color, source, target) in &solid_arrows {
                                let Some(arrow) = TriangleArrow::new(*source, *target, &style)
                                else {
                                    continue;
                                };
                                if arrow_outside_viewport(
                                    arrow.tip,
                                    arrow.base,
                                    arrow.normal,
                                    arrow.half,
                                    &viewport_rect,
                                ) {
                                    continue;
                                }
                                // The circle's center sits halfway from tip to base.
                                let radius = arrow.half;
                                let center = arrow.tip + (arrow.base - arrow.tip) * 0.5;
                                window.paint_quad(quad(
                                    Bounds {
                                        origin: point(px(center.x - radius), px(center.y - radius)),
                                        size: size(px(radius * 2.0), px(radius * 2.0)),
                                    },
                                    px(radius),
                                    *color,
                                    px(0.0),
                                    gpui::transparent_black(),
                                    Default::default(),
                                ));
                            }
                        }
                        for node in &frame.nodes {
                            let bounds = coordinates.node_bounds(node);
                            let color = if node.selected {
                                style.node_fill_selected
                            } else if node.hovered {
                                style.node_fill_hovered
                            } else {
                                match node.overlay {
                                    crate::paint::OverlayCategory::None => style.node_fill,
                                    crate::paint::OverlayCategory::Dimmed => style.node_fill_muted,
                                    crate::paint::OverlayCategory::Emphasized
                                    | crate::paint::OverlayCategory::Accent => {
                                        style.node_fill_overlay
                                    }
                                }
                            };
                            // Node LOD: a simplified node renders as a filled dot
                            // with no stroke, so the quad does no sub-pixel ring
                            // work.
                            let stroke_width = if node.simplified {
                                0.0
                            } else {
                                style.node_stroke_width
                            };
                            let stroke_color = if node.simplified {
                                color
                            } else {
                                style.node_stroke_color
                            };
                            window.paint_quad(quad(
                                bounds,
                                px(node.radius),
                                color,
                                px(stroke_width),
                                stroke_color,
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
                        for (label, measured) in frame.labels.iter().zip(&node_measures) {
                            paint_label(
                                window,
                                cx,
                                &coordinates,
                                label,
                                measured.as_ref(),
                                &style,
                                &viewport_rect,
                                node_radius_screen,
                            );
                        }
                        for (label, measured) in frame.edge_labels.iter().zip(&edge_measures) {
                            paint_edge_label(
                                window,
                                cx,
                                &coordinates,
                                label,
                                measured.as_ref(),
                                &style,
                                &viewport_rect,
                            );
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

impl<NK, EK, N, E, S> Styled for GraphView<NK, EK, N, E, S>
where
    S: std::hash::BuildHasher + Default + Clone,
{
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl<NK, EK, N, E, S> IntoElement for GraphView<NK, EK, N, E, S>
where
    S: std::hash::BuildHasher + Default + Clone,
{
    type Element = Div;

    fn into_element(self) -> Div {
        self.element
    }
}

/// Paint an edge as a list of quadratic Bézier segments.
///
/// `path` is the edge's trimmed path in window-space: a self-loop is a list of
/// onigiri segments, and any other edge is a single segment trimmed to the node
/// boundaries. Each segment is split at the boundaries of any edge-label
/// rectangles so the label stays readable over any background, and clipped to
/// the viewport so the tessellator never sees the huge coordinates of
/// off-screen nodes (which would make it subdivide the curve excessively at
/// deep zoom). Each drawn piece remains a true quadratic Bézier, so the curve
/// keeps its shape at any zoom level.
/// Paint stacking rank of an edge's resolved color, bottom-to-top. Mirrors
/// the resolution precedence in [`edge_color_layer`] so interaction colors
/// paint above ordinary edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EdgeLayer {
    Dimmed,
    Base,
    Overlay,
    Selected,
    Hovered,
}

/// Resolve an edge's paint color and stacking layer. A selected/hovered edge
/// keeps its interaction color on top of any query overlay so selection and
/// hover stay legible; otherwise the overlay category selects the color.
fn edge_color_layer(
    style: &GraphStyle,
    selected: bool,
    hovered: bool,
    overlay: crate::paint::OverlayCategory,
) -> (gpui::Hsla, EdgeLayer) {
    if selected {
        (style.edge_color_selected, EdgeLayer::Selected)
    } else if hovered {
        (style.edge_color_hovered, EdgeLayer::Hovered)
    } else {
        match overlay {
            crate::paint::OverlayCategory::None => (style.edge_color, EdgeLayer::Base),
            crate::paint::OverlayCategory::Dimmed => (style.edge_color_muted, EdgeLayer::Dimmed),
            crate::paint::OverlayCategory::Emphasized | crate::paint::OverlayCategory::Accent => {
                (style.edge_color_overlay, EdgeLayer::Overlay)
            }
        }
    }
}

/// One accumulated stroke path per resolved edge color.
struct EdgeStrokeBatch {
    color: gpui::Hsla,
    layer: EdgeLayer,
    builder: PathBuilder,
}

/// Find or start the stroke batch for `(color, layer)`. Distinct colors are
/// few (base, muted, overlay, selected, hovered), so a linear scan is cheaper
/// than a map.
fn stroke_batch<'a>(
    batches: &'a mut Vec<EdgeStrokeBatch>,
    color: gpui::Hsla,
    layer: EdgeLayer,
    style: &GraphStyle,
) -> &'a mut PathBuilder {
    if let Some(existing) = batches
        .iter_mut()
        .find(|b| b.color == color && b.layer == layer)
    {
        return &mut existing.builder;
    }
    batches.push(EdgeStrokeBatch {
        color,
        layer,
        builder: PathBuilder::stroke(px(style.edge_width)),
    });
    &mut batches.last_mut().expect("just pushed").builder
}

/// Append one edge's visible curve segments to its color's stroke batch.
/// Select the mask rects for one edge's stroke pass.
///
/// A self-loop's own label rect is dropped: the loop's label parks beside
/// the shape, and its mask reaching back over the loop would erase the
/// loop's ink. Every other edge keeps all rects including its own — that
/// own mask is what cuts a readable gap in the line under the label text.
fn stroke_masks<'a>(
    edge: &crate::paint::PaintEdge,
    edge_label_rects: &[(EdgeId, Bounds<gpui::Pixels>)],
    label_rects: &'a [Bounds<gpui::Pixels>],
    own_buf: &'a mut Vec<Bounds<gpui::Pixels>>,
) -> &'a [Bounds<gpui::Pixels>] {
    if edge.source == edge.target {
        match edge_label_rects.iter().position(|(id, _)| *id == edge.id) {
            Some(idx) => {
                own_buf.clear();
                own_buf.extend(
                    label_rects
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != idx)
                        .map(|(_, r)| *r),
                );
                own_buf
            }
            None => label_rects,
        }
    } else {
        label_rects
    }
}

fn append_edge_stroke(
    batches: &mut Vec<EdgeStrokeBatch>,
    color: gpui::Hsla,
    layer: EdgeLayer,
    path: &[Bezier],
    style: &GraphStyle,
    label_rects: &[Bounds<gpui::Pixels>],
    viewport_rect: &Bounds<gpui::Pixels>,
) {
    let builder = stroke_batch(batches, color, layer, style);
    for (p0, p1, p2) in path {
        for curve in visible_edge_curves(
            *p0,
            *p1,
            *p2,
            label_rects,
            style.edge_width,
            Some(viewport_rect),
        ) {
            builder.move_to(point(px(curve.0.x), px(curve.0.y)));
            builder.curve_to(
                point(px(curve.2.x), px(curve.2.y)),
                point(px(curve.1.x), px(curve.1.y)),
            );
        }
    }
}

/// Append a line-arrowhead polyline to an edge's stroke batch.
fn append_line_arrow(builder: &mut PathBuilder, source: Vec2, target: Vec2, style: &GraphStyle) {
    if let Some([left, tip, right]) = line_arrow_points(source, target, style) {
        builder.move_to(point(px(left.x), px(left.y)));
        builder.line_to(point(px(tip.x), px(tip.y)));
        builder.line_to(point(px(right.x), px(right.y)));
    }
}

/// A quadratic Bézier curve `(p0, p1, p2)`.
type Bezier = crate::paint::Bezier;

/// Split a quadratic Bézier into the sub-curves that are both inside the
/// viewport and not behind any label rectangle.
///
/// The curve is split exactly at the t values where it crosses the viewport's
/// edges and each label rectangle's edges, so the result matches both precisely
/// and is independent of zoom or pan. Clipping to the viewport bounds the
/// coordinates handed to the tessellator to the viewport's extent, so a curve
/// that passes through the viewport but whose endpoints are far off-screen (a
/// long edge at deep zoom) is not tessellated at its full, huge scale. Each
/// returned piece is a true quadratic Bézier, preserving the original curve
/// shape.
fn visible_edge_curves(
    p0: Vec2,
    p1: Vec2,
    p2: Vec2,
    label_rects: &[Bounds<gpui::Pixels>],
    edge_width: f32,
    viewport_rect: Option<&Bounds<gpui::Pixels>>,
) -> Vec<Bezier> {
    // A quadratic Bézier lies entirely inside the convex hull of its control
    // points. When every control point is inside the viewport and there are no
    // label masks, the whole curve is visible and needs no splitting: this is
    // the common zoomed-in case (and the overview, where every straight-LOD edge
    // fits comfortably), so it avoids the per-edge viewport_intersections and
    // label masking entirely.
    if label_rects.is_empty()
        && let Some(rect) = viewport_rect
        && inside_rect(p0, rect)
        && inside_rect(p1, rect)
        && inside_rect(p2, rect)
    {
        return vec![(p0, p1, p2)];
    }
    // The t-intervals inside the viewport (kept) and inside any label (masked).
    let keep = match viewport_rect {
        Some(rect) => viewport_intervals(p0, p1, p2, rect),
        None => vec![(0.0, 1.0)],
    };
    // The edge is stroked at `edge_width`, so its ink extends half the width on
    // either side of the centerline. Inflate the rounded label mask by that
    // half-width so no edge ink is drawn over the label.
    let inflate = edge_width * 0.5;
    let mut masked: Vec<(f32, f32)> = Vec::new();
    for rect in label_rects {
        let rr = RoundedRect::from_bounds(rect).inflated(inflate);
        masked.extend(masked_intervals(p0, p1, p2, &rr));
    }
    // Merge overlapping masked intervals.
    masked.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f32, f32)> = Vec::new();
    for (start, end) in masked {
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
            continue;
        }
        merged.push((start, end));
    }

    // Emit the sub-curves inside the viewport, minus the masked label intervals.
    let mut visible = Vec::new();
    for (k0, k1) in keep {
        let mut t = k0;
        for (start, end) in &merged {
            let s = start.max(k0);
            let e = end.min(k1);
            if s >= e {
                continue;
            }
            if s > t {
                visible.push(sub_bezier(p0, p1, p2, t, s));
            }
            t = t.max(e);
        }
        if t < k1 {
            visible.push(sub_bezier(p0, p1, p2, t, k1));
        }
    }
    visible
}

/// Whether `p` lies strictly inside the axis-aligned `rect` (inclusive).
fn inside_rect(p: Vec2, rect: &Bounds<gpui::Pixels>) -> bool {
    let min = Vec2::new(f32::from(rect.origin.x), f32::from(rect.origin.y));
    let max = min + Vec2::new(f32::from(rect.size.width), f32::from(rect.size.height));
    p.x >= min.x && p.x <= max.x && p.y >= min.y && p.y <= max.y
}

/// The t-intervals of a quadratic Bézier that lie inside the axis-aligned
/// `rect`.
///
/// The t values where the curve crosses the rect's edges are collected, then
/// each interval whose midpoint is inside the rect is kept.
fn viewport_intervals(
    p0: Vec2,
    p1: Vec2,
    p2: Vec2,
    rect: &Bounds<gpui::Pixels>,
) -> Vec<(f32, f32)> {
    let min = Vec2::new(f32::from(rect.origin.x), f32::from(rect.origin.y));
    let max = min + Vec2::new(f32::from(rect.size.width), f32::from(rect.size.height));
    let mut ts = vec![0.0, 1.0];
    for value in [min.x, max.x] {
        ts.extend(bezier_roots(p0, p1, p2, value, 0));
    }
    for value in [min.y, max.y] {
        ts.extend(bezier_roots(p0, p1, p2, value, 1));
    }
    ts.sort_by(|a, b| a.total_cmp(b));
    ts.dedup_by(|a, b| (*a - *b).abs() < 1e-9);

    let mut keep = Vec::new();
    for window in ts.windows(2) {
        let (t0, t1) = (window[0], window[1]);
        let p = bezier_point(p0, p1, p2, (t0 + t1) * 0.5);
        if p.x >= min.x && p.x <= max.x && p.y >= min.y && p.y <= max.y {
            keep.push((t0, t1));
        }
    }
    keep
}

/// Split a quadratic Bézier into the sub-curves that do not pass behind any
/// label rectangle, ignoring the viewport.
///
/// Thin wrapper over [`visible_edge_curves`] with an unbounded viewport, used by
/// tests that exercise label masking in isolation.
#[cfg(test)]
fn visible_bezier_curves(
    p0: Vec2,
    p1: Vec2,
    p2: Vec2,
    label_rects: &[Bounds<gpui::Pixels>],
    edge_width: f32,
) -> Vec<Bezier> {
    visible_edge_curves(p0, p1, p2, label_rects, edge_width, None)
}

/// The t-intervals of a quadratic Bézier that lie strictly inside a rounded
/// label rectangle.
///
/// The t values where the curve crosses the rectangle's edges are collected
/// (the rounded corners are strictly inside the box, so every masked interval
/// is bounded by box-edge crossings), then each interval is masked only if its
/// midpoint lies inside the rounded rectangle. A curve hugging a corner is
/// therefore left visible where it passes the rounded corner.
fn masked_intervals(p0: Vec2, p1: Vec2, p2: Vec2, rect: &RoundedRect) -> Vec<(f32, f32)> {
    // Collect every t where the curve crosses a box edge.
    let mut ts = vec![0.0, 1.0];
    for value in [rect.min.x, rect.max.x] {
        ts.extend(bezier_roots(p0, p1, p2, value, 0));
    }
    for value in [rect.min.y, rect.max.y] {
        ts.extend(bezier_roots(p0, p1, p2, value, 1));
    }
    ts.sort_by(|a, b| a.total_cmp(b));
    ts.dedup_by(|a, b| (*a - *b).abs() < 1e-9);

    // An interval is masked if its midpoint lies inside the rounded rectangle.
    let mut masked = Vec::new();
    for window in ts.windows(2) {
        let (t0, t1) = (window[0], window[1]);
        let p = bezier_point(p0, p1, p2, (t0 + t1) * 0.5);
        if rect.contains(p) {
            masked.push((t0, t1));
        }
    }
    masked
}

/// A rectangle with rounded corners used to mask edges and arrowheads that pass
/// behind a label. The corner radius matches the label's background padding, so
/// the masked region hugs the rounded corners instead of leaving a hard square
/// notch in an edge.
struct RoundedRect {
    /// Bottom-left corner.
    min: Vec2,
    /// Top-right corner.
    max: Vec2,
    /// Corner radius in window pixels.
    radius: f32,
}

impl RoundedRect {
    /// The label background padding also used to compute label bounds.
    const RADIUS: f32 = 4.0;

    fn from_bounds(rect: &Bounds<gpui::Pixels>) -> Self {
        let min = Vec2::new(f32::from(rect.origin.x), f32::from(rect.origin.y));
        let max = min + Vec2::new(f32::from(rect.size.width), f32::from(rect.size.height));
        Self {
            min,
            max,
            radius: Self::RADIUS,
        }
    }

    /// Grow the rectangle outward by `d` on all sides. A rounded rect grown by
    /// a disk of radius `d` has both its bounds and its corner radius grown by
    /// `d`.
    fn inflated(self, d: f32) -> Self {
        Self {
            min: self.min - Vec2::splat(d),
            max: self.max + Vec2::splat(d),
            radius: self.radius + d,
        }
    }

    /// Whether `p` lies strictly inside the rounded rectangle.
    fn contains(&self, p: Vec2) -> bool {
        let r = self.radius;
        if p.x <= self.min.x || p.x >= self.max.x || p.y <= self.min.y || p.y >= self.max.y {
            return false;
        }
        // In each corner square the outline is a quarter arc: a point there is
        // inside only when it is within the corner circle's radius of its
        // center.
        let beyond = |cx: f32, cy: f32| {
            let dx = p.x - cx;
            let dy = p.y - cy;
            dx * dx + dy * dy > r * r
        };
        if p.x > self.max.x - r && p.y > self.max.y - r && beyond(self.max.x - r, self.max.y - r) {
            return false;
        }
        if p.x < self.min.x + r && p.y > self.max.y - r && beyond(self.min.x + r, self.max.y - r) {
            return false;
        }
        if p.x > self.max.x - r && p.y < self.min.y + r && beyond(self.max.x - r, self.min.y + r) {
            return false;
        }
        if p.x < self.min.x + r && p.y < self.min.y + r && beyond(self.min.x + r, self.min.y + r) {
            return false;
        }
        true
    }

    /// The rounded outline as an approximating polygon, used to clip the
    /// arrowhead hole against the label shape. Traverses counterclockwise from
    /// the bottom-left edge.
    fn as_polygon(&self, arc_steps: usize) -> Vec<Vec2> {
        let r = self.radius;
        let x0 = self.min.x;
        let y0 = self.min.y;
        let x1 = self.max.x;
        let y1 = self.max.y;
        let mut pts = Vec::new();
        pts.push(Vec2::new(x0 + r, y0));
        push_arc(
            &mut pts,
            Vec2::new(x1 - r, y0 + r),
            r,
            270.0,
            360.0,
            arc_steps,
        );
        pts.push(Vec2::new(x1, y0 + r));
        push_arc(&mut pts, Vec2::new(x1 - r, y1 - r), r, 0.0, 90.0, arc_steps);
        pts.push(Vec2::new(x1 - r, y1));
        push_arc(
            &mut pts,
            Vec2::new(x0 + r, y1 - r),
            r,
            90.0,
            180.0,
            arc_steps,
        );
        pts.push(Vec2::new(x0, y1 - r));
        push_arc(
            &mut pts,
            Vec2::new(x0 + r, y0 + r),
            r,
            180.0,
            270.0,
            arc_steps,
        );
        pts
    }
}

/// Push the points of a circular arc onto `pts`. `start` and `end` are degrees
/// measured counterclockwise from the +x axis.
fn push_arc(pts: &mut Vec<Vec2>, center: Vec2, radius: f32, start: f32, end: f32, steps: usize) {
    let n = steps.max(1);
    for i in 0..=n {
        let t = i as f32 / n as f32;
        let a = (start + (end - start) * t).to_radians();
        pts.push(center + Vec2::new(a.cos(), a.sin()) * radius);
    }
}

/// The real roots in `[0, 1]` of a quadratic Bézier's coordinate equal to
/// `value`. `axis` is `0` for x and `1` for y.
///
/// Uses a numerically stable quadratic solver to avoid catastrophic
/// cancellation when the coefficients are large (e.g. a long edge with a
/// fanned control point far from the endpoints).
fn bezier_roots(p0: Vec2, p1: Vec2, p2: Vec2, value: f32, axis: usize) -> Vec<f32> {
    let (v0, v1, v2) = if axis == 0 {
        (p0.x, p1.x, p2.x)
    } else {
        (p0.y, p1.y, p2.y)
    };
    // x(t) = a t^2 + b t + c, with x(t) - value = 0.
    let a = v0 - 2.0 * v1 + v2;
    let b = 2.0 * (v1 - v0);
    let c = v0 - value;

    let mut roots = Vec::new();
    // If the quadratic term is negligible relative to the linear term, the
    // curve is effectively linear on this axis; solve linearly to avoid
    // catastrophic cancellation in the quadratic formula.
    if a.abs() <= b.abs() * 1e-6 {
        if b.abs() > f32::EPSILON {
            let t = -c / b;
            if (0.0..=1.0).contains(&t) {
                roots.push(t);
            }
        }
        return roots;
    }

    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return roots;
    }
    let sqrt = disc.sqrt();
    if b.abs() <= f32::EPSILON {
        // Symmetric curve (b == 0): roots are ±sqrt(-c/a).
        let t = (-c / a).sqrt();
        for t in [-t, t] {
            if (0.0..=1.0).contains(&t) {
                roots.push(t);
            }
        }
        return roots;
    }
    // Stable quadratic formula: q = -0.5 * (b + sign(b) * sqrt(disc)).
    let q = -0.5 * (b + b.signum() * sqrt);
    let t1 = q / a;
    let t2 = c / q;
    for t in [t1, t2] {
        if (0.0..=1.0).contains(&t) {
            roots.push(t);
        }
    }
    roots
}

/// A point on a quadratic Bézier at parameter `t`.
fn bezier_point(p0: Vec2, p1: Vec2, p2: Vec2, t: f32) -> Vec2 {
    let inv = 1.0 - t;
    inv * inv * p0 + 2.0 * inv * t * p1 + t * t * p2
}

/// The sub-curve of a quadratic Bézier from parameter `t0` to `t1`.
fn sub_bezier(p0: Vec2, p1: Vec2, p2: Vec2, t0: f32, t1: f32) -> Bezier {
    // Subdivide at t0 to get the [t0, 1] piece, then at the normalized t1.
    let (_, right) = subdivide(p0, p1, p2, t0);
    let s = (t1 - t0) / (1.0 - t0);
    let (left, _) = subdivide(right.0, right.1, right.2, s);
    left
}

/// Split a quadratic Bézier at parameter `t` into `[0, t]` and `[t, 1]`.
fn subdivide(p0: Vec2, p1: Vec2, p2: Vec2, t: f32) -> (Bezier, Bezier) {
    let ab = p0 + (p1 - p0) * t;
    let bc = p1 + (p2 - p1) * t;
    let abc = ab + (bc - ab) * t;
    ((p0, ab, abc), (abc, bc, p2))
}

/// A triangle arrowhead's geometry in window space.
///
/// One owner for the shape shared by every draw path.
#[derive(Debug, Clone, Copy)]
struct TriangleArrow {
    tip: Vec2,
    base: Vec2,
    normal: Vec2,
    half: f32,
}

impl TriangleArrow {
    /// The arrowhead pointing at `target` along `source -> target`, sized by
    /// `style`. `None` when the extent is degenerate.
    fn new(source: Vec2, target: Vec2, style: &GraphStyle) -> Option<Self> {
        let dir = target - source;
        let len = dir.length();
        if len < f32::EPSILON {
            return None;
        }
        let unit = dir / len;
        let tip = target;
        let base = tip - unit * style.edge_arrow_size;
        let normal = Vec2::new(-unit.y, unit.x);
        Some(Self {
            tip,
            base,
            normal,
            half: style.edge_arrow_size * 0.5,
        })
    }
}

/// Twice the signed area of a closed polygon (the shoelace sum); the sign is
/// the polygon's winding.
fn signed_area_times_two(points: &[Vec2]) -> f32 {
    let mut sum = 0.0;
    let n = points.len();
    for i in 0..n {
        let a = points[i];
        let b = points[(i + 1) % n];
        sum += a.x * b.y - b.x * a.y;
    }
    sum
}

/// Reverse `hole` in place when it winds the same way as the triangle of
/// signed area `triangle_area`, so under the nonzero fill rule its winding
/// cancels the triangle inside the hole instead of adding to it.
fn orient_hole_against(mut hole: Vec<Vec2>, triangle_area: f32) -> Vec<Vec2> {
    if signed_area_times_two(&hole) * triangle_area > 0.0 {
        hole.reverse();
    }
    hole
}

/// Append one triangle arrowhead — and the reversed clipped-label contours
/// that punch out any part hidden behind a label — to a nonzero-winding fill
/// builder shared with same-color arrowheads.
///
/// Nonzero (not evenodd) matters when batching: overlapping same-color
/// arrowheads both wind positive and stay filled, where an evenodd merge
/// would XOR them into holes. A label hole cancels only against its own
/// triangle because it winds opposite; where a hole region is covered by
/// another arrowhead, that arrowhead's winding still fills it, exactly as if
/// the later head had painted over it.
fn append_triangle_arrow(
    builder: &mut PathBuilder,
    arrow: &TriangleArrow,
    style: &GraphStyle,
    label_rects: &[Bounds<gpui::Pixels>],
) {
    let triangle = [
        arrow.tip,
        arrow.base + arrow.normal * arrow.half,
        arrow.base - arrow.normal * arrow.half,
    ];
    builder.move_to(point(px(triangle[0].x), px(triangle[0].y)));
    for p in &triangle[1..] {
        builder.line_to(point(px(p.x), px(p.y)));
    }
    builder.close();

    // Punch out the part of the arrow that lies behind a label. The hole is
    // the arrow triangle clipped to the label rect — not the whole rect — so
    // a rect that merely overlaps the arrow's edge does not leave a gray
    // strip beyond the arrow.
    let triangle_area = signed_area_times_two(&triangle);
    let overlapping = label_rects
        .iter()
        .filter(|r| arrow_overlaps_rect(arrow.tip, arrow.base, arrow.half, arrow.normal, r));
    for rect in overlapping {
        let hole = rect_intersection(
            arrow.tip,
            arrow.base,
            arrow.half,
            arrow.normal,
            rect,
            style.edge_width,
        );
        if hole.len() >= 3 {
            add_polygon_subpath(builder, &orient_hole_against(hole, triangle_area));
        }
    }
}

/// Build the nonzero-winding fill builder shared by same-color triangle
/// arrowheads.
fn triangle_fill_builder() -> PathBuilder {
    // FillOptions is #[non_exhaustive], so mutate the default instead of
    // constructing a literal.
    let mut options = FillOptions::default();
    options.fill_rule = FillRule::NonZero;
    PathBuilder::fill().with_style(PathStyle::Fill(options))
}

/// The three points of a line-arrowhead polyline (base-left, tip, base-right),
/// or `None` when the extent is degenerate. Shared by the immediate and
/// batched line-arrow paths so the drawn geometry has one owner.
fn line_arrow_points(source: Vec2, target: Vec2, style: &GraphStyle) -> Option<[Vec2; 3]> {
    let dir = target - source;
    let len = dir.length();
    if len < f32::EPSILON {
        return None;
    }
    let unit = dir / len;
    let tip = target;
    let base = tip - unit * style.edge_arrow_size;
    let normal = Vec2::new(-unit.y, unit.x);
    let half = style.edge_arrow_size * 0.5;
    Some([base + normal * half, tip, base - normal * half])
}

/// Whether the arrowhead's bounding box lies entirely outside the viewport.
///
/// The arrow is a triangle (or line/circle) with tip `tip` and base corners
/// `base ± normal * half`. Its bounding box is a cheap reject test: if it does
/// not intersect the viewport, the arrow is off-screen and need not be handed to
/// the tessellator (which would otherwise see the huge coordinates of an
/// off-screen edge endpoint).
fn arrow_outside_viewport(
    tip: Vec2,
    base: Vec2,
    normal: Vec2,
    half: f32,
    viewport_rect: &Bounds<gpui::Pixels>,
) -> bool {
    let min = tip.min(base + normal * half).min(base - normal * half);
    let max = tip.max(base + normal * half).max(base - normal * half);
    let vmin = Vec2::new(
        f32::from(viewport_rect.origin.x),
        f32::from(viewport_rect.origin.y),
    );
    let vmax = vmin
        + Vec2::new(
            f32::from(viewport_rect.size.width),
            f32::from(viewport_rect.size.height),
        );
    max.x < vmin.x || min.x > vmax.x || max.y < vmin.y || min.y > vmax.y
}

/// Whether `point` lies within `margin` pixels of the viewport.
///
/// Used to skip shaping labels whose anchor is far off-screen. The margin is
/// generous (a few hundred pixels) so labels near the viewport edge remain
/// visible while those well off-screen are rejected.
fn point_near_viewport(point: Vec2, viewport_rect: &Bounds<gpui::Pixels>, margin: f32) -> bool {
    let vmin = Vec2::new(
        f32::from(viewport_rect.origin.x),
        f32::from(viewport_rect.origin.y),
    );
    let vmax = vmin
        + Vec2::new(
            f32::from(viewport_rect.size.width),
            f32::from(viewport_rect.size.height),
        );
    point.x >= vmin.x - margin
        && point.x <= vmax.x + margin
        && point.y >= vmin.y - margin
        && point.y <= vmax.y + margin
}

/// Whether a label rect overlaps the arrowhead's bounding region. The arrow
/// tip is `tip`, and `base ± normal * half` are the base corners.
fn arrow_overlaps_rect(
    tip: Vec2,
    base: Vec2,
    half: f32,
    normal: Vec2,
    rect: &Bounds<gpui::Pixels>,
) -> bool {
    let rmin = Vec2::new(f32::from(rect.origin.x), f32::from(rect.origin.y));
    let rmax = rmin + Vec2::new(f32::from(rect.size.width), f32::from(rect.size.height));
    let p1 = base + normal * half;
    let p2 = base - normal * half;
    let min_x = tip.x.min(p1.x).min(p2.x);
    let max_x = tip.x.max(p1.x).max(p2.x);
    let min_y = tip.y.min(p1.y).min(p2.y);
    let max_y = tip.y.max(p1.y).max(p2.y);
    max_x > rmin.x && min_x < rmax.x && max_y > rmin.y && min_y < rmax.y
}

/// The part of the arrow triangle that lies inside the rounded label rect, as a
/// polygon in window space. The triangle has tip `tip` and base corners
/// `base ± normal * half`. This is the region punched out of the arrow; using
/// the clipped region (not the whole label shape) keeps a label that only
/// overlaps the arrow's edge from leaving a gray strip beyond the arrow, and
/// hugging the rounded corners keeps the hole aligned with the label's outline.
fn rect_intersection(
    tip: Vec2,
    base: Vec2,
    half: f32,
    normal: Vec2,
    rect: &Bounds<gpui::Pixels>,
    edge_width: f32,
) -> Vec<Vec2> {
    // Start from the rounded label outline and clip it against each edge of the
    // triangle (keep the inside). The triangle interior side of each edge is
    // the side that contains the triangle's centroid, so the winding does not
    // matter. The outline is inflated by half the edge width so the arrow's
    // ink (and the stroked edge line reaching it) does not paint over the
    // label.
    let mut poly = RoundedRect::from_bounds(rect)
        .inflated(edge_width * 0.5)
        .as_polygon(8);
    let triangle = [tip, base + normal * half, base - normal * half];
    let centroid = (triangle[0] + triangle[1] + triangle[2]) / 3.0;
    for i in 0..3 {
        let a = triangle[i];
        let b = triangle[(i + 1) % 3];
        let keep_above = inside_half_plane(centroid, a, b);
        poly = clip_half_plane(&poly, a, b, keep_above);
        if poly.len() < 3 {
            return Vec::new();
        }
    }
    poly
}

/// Clip `poly` to the half-plane on the same side of the directed edge `a -> b`
/// as the interior, keeping points where `inside_half_plane` equals
/// `keep_above` (Sutherland–Hodgman).
fn clip_half_plane(poly: &[Vec2], a: Vec2, b: Vec2, keep_above: bool) -> Vec<Vec2> {
    let n = poly.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let cur = poly[i];
        let next = poly[(i + 1) % n];
        let cur_in = inside_half_plane(cur, a, b) == keep_above;
        let next_in = inside_half_plane(next, a, b) == keep_above;
        if cur_in {
            out.push(cur);
        }
        if cur_in != next_in {
            out.push(line_intersection(cur, next, a, b));
        }
    }
    out
}

fn inside_half_plane(p: Vec2, a: Vec2, b: Vec2) -> bool {
    // Left of a->b means the cross product (b - a) x (p - a) is >= 0.
    (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x) >= 0.0
}

fn line_intersection(p1: Vec2, p2: Vec2, a: Vec2, b: Vec2) -> Vec2 {
    let d1 = p2 - p1;
    let d2 = b - a;
    let denom = d1.x * d2.y - d1.y * d2.x;
    if denom.abs() < f32::EPSILON {
        return p2;
    }
    let t = ((a.x - p1.x) * d2.y - (a.y - p1.y) * d2.x) / denom;
    p1 + d1 * t
}

/// Add a closed polygon sub-contour to `builder`, used to punch the clipped
/// arrow region out of a fill path via the evenodd fill rule.
fn add_polygon_subpath(builder: &mut PathBuilder, poly: &[Vec2]) {
    builder.move_to(point(px(poly[0].x), px(poly[0].y)));
    for &p in &poly[1..] {
        builder.line_to(point(px(p.x), px(p.y)));
    }
    builder.close();
}

impl<NK, EK, N, E, S> GraphViewState<NK, EK, N, E, S>
where
    NK: Eq + std::hash::Hash + 'static,
    EK: Eq + std::hash::Hash + 'static,
    N: 'static,
    E: 'static,
    S: std::hash::BuildHasher + Default + Clone + 'static,
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
            self.cancel_initial_auto_fit_on_input();
            self.begin_interaction(cx);
            let delta = pos - self.last_mouse;
            self.viewport.pan(delta);
            cx.emit(GraphEvent::ViewportChanged);
        } else {
            let scene = self.scene.read(cx);
            let synced = scene.sync_runtime(&mut self.runtime);
            let hit = hit_test::hit_test(
                &synced,
                &self.viewport,
                &self.style,
                pos,
                self.node_label.as_ref(),
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
        let synced = scene.sync_runtime(&mut self.runtime);
        let hit = hit_test::hit_test(
            &synced,
            &self.viewport,
            &self.style,
            pos,
            self.node_label.as_ref(),
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
            self.cancel_initial_auto_fit_on_input();
            // Pan starts here: elevate the interaction LOD immediately so the
            // very first frame after mouse_down (before any mouse_move) already
            // renders the graph at the low-detail threshold. Without this the
            // first pan frame would run the idle high-detail threshold and feel
            // sluggish to start moving.
            self.begin_interaction(cx);
            self.panning = true;
        }
        self.last_mouse = pos;
        cx.notify();
    }
}

/// Paint a node label centered below the node.
#[allow(clippy::too_many_arguments)]
fn paint_label(
    window: &mut Window,
    cx: &mut gpui::App,
    coordinates: &CanvasCoordinates,
    label: &crate::paint::PaintLabel,
    measured: Option<&MeasuredLabel>,
    style: &GraphStyle,
    viewport_rect: &Bounds<gpui::Pixels>,
    node_radius_screen: f32,
) {
    let anchor = coordinates.canvas_to_window(label.position);
    // Skip labels whose anchor is far outside the viewport. The label is small
    // (a few dozen pixels), so a generous margin keeps labels near the edge
    // visible while rejecting those well off-screen. The text was already
    // shaped once for this frame; this guard only skips the drawing.
    if !point_near_viewport(anchor, viewport_rect, 200.0) {
        return;
    }
    let Some(measured) = measured else {
        return;
    };
    let mut origin = point(
        px(anchor.x),
        px(anchor.y + node_radius_screen + style.label_offset),
    );
    for line in &measured.lines {
        // Center the label horizontally on the node by shifting the origin by
        // half the line width. `WrappedLine::paint` only honors `TextAlign`
        // when a bounds width is provided, so we center manually.
        let line_size = line.size(measured.line_height);
        let centered = point(px(anchor.x - f32::from(line_size.width) * 0.5), origin.y);
        let _ = line.paint(
            centered,
            measured.line_height,
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

/// A label's shaped text and total size, measured once per frame.
///
/// Collision resolution slides labels along their paths but never changes a
/// label's size, so one measurement serves the collision rects, the edge
/// cut-outs, and painting — previously each of those re-shaped the same text
/// per call (up to sixteen passes in the collision loop).
struct MeasuredLabel {
    lines: Vec<gpui::WrappedLine>,
    line_height: gpui::Pixels,
    /// Widest shaped line.
    width: f32,
    /// Sum of line heights.
    height: f32,
}

/// Shape `text` and compute its total laid-out size under `style`'s label
/// typography. Returns `None` when the text cannot be shaped.
fn measure_label(window: &Window, text: &str, style: &GraphStyle) -> Option<MeasuredLabel> {
    let font_size = style.label_style.font_size.to_pixels(window.rem_size());
    let line_height = style
        .label_style
        .line_height
        .to_pixels(font_size.into(), window.rem_size());
    let run = style.label_style.to_run(text.len());
    let lines: Vec<gpui::WrappedLine> = window
        .text_system()
        .shape_text(text.to_owned().into(), font_size, &[run], None, None)
        .ok()?
        .into_iter()
        .collect();
    let mut width = 0.0f32;
    let mut height = 0.0f32;
    for line in &lines {
        let line_size = line.size(line_height);
        width = width.max(f32::from(line_size.width));
        height += f32::from(line_size.height);
    }
    Some(MeasuredLabel {
        lines,
        line_height,
        width,
        height,
    })
}

/// The bounds of a measured label centered horizontally on `anchor`, with a
/// horizontal margin so edges are cut slightly beyond the text, keeping the
/// label clear of the line. `top_y` maps the anchor and total height to the
/// rect's top edge (node labels hang below the node; edge labels center on
/// their anchor).
fn label_rect(
    measured: &MeasuredLabel,
    anchor: Vec2,
    top_y: impl Fn(Vec2, f32) -> f32,
) -> Bounds<gpui::Pixels> {
    let margin = 4.0f32;
    Bounds {
        origin: point(
            px(anchor.x - measured.width * 0.5 - margin),
            px(top_y(anchor, measured.height)),
        ),
        size: size(px(measured.width + margin * 2.0), px(measured.height)),
    }
}

/// Slide edge labels along their edges so overlapping labels move apart
/// smoothly. Each label keeps its offset off the edge line but shifts its
/// anchor along the edge's trimmed path. The displacement is proportional to
/// the overlap depth, so labels move continuously: the deeper the overlap, the
/// farther they slide, and the motion eases as they separate. Node labels are
/// fixed obstacles: edge labels slide to avoid them, but node labels never move.
fn resolve_edge_label_collisions(
    frame: &mut crate::paint::PaintFrame,
    style: &GraphStyle,
    node_label_rects: &[Bounds<gpui::Pixels>],
    measures: &[Option<MeasuredLabel>],
) {
    // A label's rect is its (constant) measured size placed at its current
    // anchor, so sliding a label only moves the rect — no re-shaping.
    let label_rect_at = |label: &crate::paint::PaintEdgeLabel, measured: Option<&MeasuredLabel>| {
        let measured = measured?;
        let anchor = label.position + label.offset * style.label_offset;
        Some(label_rect(measured, anchor, |anchor, height| {
            anchor.y - height * 0.5
        }))
    };
    // Every edge label carries its edge's path (self-loops included), so all
    // of them can slide along it to avoid collisions.
    let movable: Vec<usize> = frame
        .edge_labels
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.path.is_empty())
        .map(|(i, _)| i)
        .collect();
    // Precompute each label's path length to convert a pixel displacement into
    // a parameter change along the curve.
    let path_lengths: Vec<f32> = movable
        .iter()
        .map(|&i| path_length(&frame.edge_labels[i].path))
        .collect();
    // Resolve pairwise overlaps by displacing both labels along their paths by
    // an amount proportional to the overlap depth. Iterate until the overlaps
    // shrink below the intersection threshold or a bounded number of passes.
    for _ in 0..16 {
        let mut moved = false;
        // Edge labels avoid the fixed node labels.
        for &i in &movable {
            let ra = match label_rect_at(
                &frame.edge_labels[i],
                measures.get(i).and_then(|m| m.as_ref()),
            ) {
                Some(ra) => ra,
                None => continue,
            };
            for rect in node_label_rects {
                let Some(overlap) = bounds_intersection(&ra, rect) else {
                    continue;
                };
                let dir = perpendicular(frame.edge_labels[i].offset);
                let depth = project_rect(overlap, dir);
                let dt = depth / path_lengths[position_of(&movable, i)].max(1.0);
                // Slide the edge label away from the node label along its path.
                let t = frame.edge_labels[i].t;
                let toward_start = t - dt;
                let toward_end = t + dt;
                // Choose the direction that moves the label away from the node
                // label's center.
                let center = Vec2::new(
                    f32::from(rect.origin.x) + f32::from(rect.size.width) * 0.5,
                    f32::from(rect.origin.y) + f32::from(rect.size.height) * 0.5,
                );
                let start_pos = path_point(&frame.edge_labels[i].path, toward_start);
                let end_pos = path_point(&frame.edge_labels[i].path, toward_end);
                let d_start = (start_pos - center).length();
                let d_end = (end_pos - center).length();
                frame.edge_labels[i].t = if d_start > d_end {
                    toward_start.max(0.0)
                } else {
                    toward_end.min(1.0)
                };
                frame.edge_labels[i].position =
                    path_point(&frame.edge_labels[i].path, frame.edge_labels[i].t);
                moved = true;
            }
        }
        // Edge labels avoid each other.
        for a in 0..movable.len() {
            for b in (a + 1)..movable.len() {
                let ia = movable[a];
                let ib = movable[b];
                let measure_at = |i: usize| {
                    measures.get(i).and_then(|m| m.as_ref()).map(|m| {
                        let anchor = frame.edge_labels[i].position
                            + frame.edge_labels[i].offset * style.label_offset;
                        label_rect(m, anchor, |anchor, height| anchor.y - height * 0.5)
                    })
                };
                let (ra, rb) = match (measure_at(ia), measure_at(ib)) {
                    (Some(ra), Some(rb)) => (ra, rb),
                    _ => continue,
                };
                let Some(overlap) = bounds_intersection(&ra, &rb) else {
                    continue;
                };
                // A self-loop's path has two onigiri segments; any other edge
                // has one. When a self-loop label collides with a longer edge's
                // label, the longer edge slides away from the self-loop while
                // the self-loop stays put — the longer edge has more room to
                // move, giving a better result.
                let a_is_self_loop = frame.edge_labels[ia].path.len() > 1;
                let b_is_self_loop = frame.edge_labels[ib].path.len() > 1;
                if a_is_self_loop != b_is_self_loop {
                    let other_idx = if a_is_self_loop { ib } else { ia };
                    let self_rect = if a_is_self_loop { ra } else { rb };
                    let self_center = Vec2::new(
                        f32::from(self_rect.origin.x) + f32::from(self_rect.size.width) * 0.5,
                        f32::from(self_rect.origin.y) + f32::from(self_rect.size.height) * 0.5,
                    );
                    let dir = perpendicular(frame.edge_labels[other_idx].offset);
                    let depth = project_rect(overlap, dir);
                    let dt = depth / path_lengths[position_of(&movable, other_idx)].max(1.0);
                    // Slide the longer edge away from the self-loop's center.
                    let t = frame.edge_labels[other_idx].t;
                    let toward_start = t - dt;
                    let toward_end = t + dt;
                    let start_pos = path_point(&frame.edge_labels[other_idx].path, toward_start);
                    let end_pos = path_point(&frame.edge_labels[other_idx].path, toward_end);
                    let d_start = (start_pos - self_center).length();
                    let d_end = (end_pos - self_center).length();
                    frame.edge_labels[other_idx].t = if d_start > d_end {
                        toward_start.max(0.0)
                    } else {
                        toward_end.min(1.0)
                    };
                    frame.edge_labels[other_idx].position = path_point(
                        &frame.edge_labels[other_idx].path,
                        frame.edge_labels[other_idx].t,
                    );
                    moved = true;
                    continue;
                }
                // The path direction is perpendicular to the label's offset
                // (which points off the edge line). Project the overlap onto
                // each label's path direction to get the pixel depth to slide.
                let dir_a = perpendicular(frame.edge_labels[ia].offset);
                let dir_b = perpendicular(frame.edge_labels[ib].offset);
                let depth_a = project_rect(overlap, dir_a);
                let depth_b = project_rect(overlap, dir_b);
                // Convert the pixel depth to a parameter change along the path.
                let dt_a = depth_a / path_lengths[a].max(1.0);
                let dt_b = depth_b / path_lengths[b].max(1.0);
                // Displace both labels apart: the one with the smaller t toward
                // the start, the other toward the end.
                let ta = frame.edge_labels[ia].t;
                let tb = frame.edge_labels[ib].t;
                if ta <= tb {
                    frame.edge_labels[ia].t = (ta - dt_a).max(0.0);
                    frame.edge_labels[ib].t = (tb + dt_b).min(1.0);
                } else {
                    frame.edge_labels[ia].t = (ta + dt_a).min(1.0);
                    frame.edge_labels[ib].t = (tb - dt_b).max(0.0);
                }
                // Recompute the anchor position from the new t.
                frame.edge_labels[ia].position =
                    path_point(&frame.edge_labels[ia].path, frame.edge_labels[ia].t);
                frame.edge_labels[ib].position =
                    path_point(&frame.edge_labels[ib].path, frame.edge_labels[ib].t);
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
}

/// The index of `i` within `movable`.
fn position_of(movable: &[usize], i: usize) -> usize {
    movable.iter().position(|&m| m == i).unwrap_or(0)
}

/// Remove edge labels that have drifted within `edge_label_hide_distance`
/// pixels of a node center. Collision resolution slides labels along their
/// edges to avoid other labels, which can push a label onto a node; such a
/// label would sit over the node and look broken, so it is hidden instead.
/// Distance is measured from the label's position to the nearest node center in
/// canvas-local pixels (the same space as the label's `position`).
fn hide_edge_labels_near_nodes(
    frame: &mut crate::paint::PaintFrame,
    style: &GraphStyle,
    measures: &mut Vec<Option<MeasuredLabel>>,
) {
    let hide = style.edge_label_hide_distance;
    // Prune labels and their parallel measurements together so indices stay
    // aligned for painting after the hide pass.
    let labels = std::mem::take(&mut frame.edge_labels);
    let dropped_measures = std::mem::take(measures);
    let mut kept_labels = Vec::with_capacity(labels.len());
    let mut kept_measures = Vec::with_capacity(dropped_measures.len());
    for (label, measured) in labels.into_iter().zip(dropped_measures) {
        if frame
            .nodes
            .iter()
            .all(|node| (label.position - node.position).length() >= hide)
        {
            kept_labels.push(label);
            kept_measures.push(measured);
        }
    }
    frame.edge_labels = kept_labels;
    *measures = kept_measures;
}

/// The overlapping region of two axis-aligned bounds as `(x, y, width, height)`
/// in pixels, or `None` if they do not overlap.
fn bounds_intersection(
    a: &Bounds<gpui::Pixels>,
    b: &Bounds<gpui::Pixels>,
) -> Option<(f32, f32, f32, f32)> {
    let ax = f32::from(a.origin.x);
    let ay = f32::from(a.origin.y);
    let aw = f32::from(a.size.width);
    let ah = f32::from(a.size.height);
    let bx = f32::from(b.origin.x);
    let by = f32::from(b.origin.y);
    let bw = f32::from(b.size.width);
    let bh = f32::from(b.size.height);
    let x0 = ax.max(bx);
    let y0 = ay.max(by);
    let x1 = (ax + aw).min(bx + bw);
    let y1 = (ay + ah).min(by + bh);
    if x1 > x0 && y1 > y0 {
        Some((x0, y0, x1 - x0, y1 - y0))
    } else {
        None
    }
}

/// The length of a rectangle `(x, y, width, height)` projected onto a unit
/// direction `dir`.
fn project_rect(rect: (f32, f32, f32, f32), dir: Vec2) -> f32 {
    let (_, _, w, h) = rect;
    w * dir.x.abs() + h * dir.y.abs()
}

/// A unit vector perpendicular to `v`.
fn perpendicular(v: Vec2) -> Vec2 {
    let n = Vec2::new(-v.y, v.x);
    let len = n.length();
    if len > f32::EPSILON {
        n / len
    } else {
        Vec2::new(1.0, 0.0)
    }
}

/// The total length of a multi-segment Bézier path, approximated by the sum of
/// the chord lengths of its segments.
fn path_length(path: &[crate::paint::Bezier]) -> f32 {
    path.iter().map(|(p0, _, p2)| (*p2 - *p0).length()).sum()
}

/// A point on a multi-segment Bézier path at parameter `t` in `[0, 1]`.
fn path_point(path: &[crate::paint::Bezier], t: f32) -> Vec2 {
    let t = t.clamp(0.0, 1.0);
    let n = path.len().max(1);
    let seg = (t * n as f32).floor().min((n - 1) as f32) as usize;
    let local = t * n as f32 - seg as f32;
    let (p0, p1, p2) = path[seg];
    bezier_point(p0, p1, p2, local)
}

/// Paint an edge label centered at the edge midpoint, offset off the edge line.
fn paint_edge_label(
    window: &mut Window,
    cx: &mut gpui::App,
    coordinates: &CanvasCoordinates,
    label: &crate::paint::PaintEdgeLabel,
    measured: Option<&MeasuredLabel>,
    style: &GraphStyle,
    viewport_rect: &Bounds<gpui::Pixels>,
) {
    // label.position is already in canvas-local pixels.
    // Apply the user-defined label_offset along the label's fixed offset direction.
    let anchor = coordinates.canvas_to_window(label.position + label.offset * style.label_offset);
    // Skip labels whose anchor is far outside the viewport; this guard only
    // skips the drawing — the text was shaped once for this frame already.
    if !point_near_viewport(anchor, viewport_rect, 200.0) {
        return;
    }
    let Some(measured) = measured else {
        return;
    };
    // Center the label vertically on the anchor by shifting the origin up by
    // half the total text height.
    let mut origin = point(px(anchor.x), px(anchor.y - measured.height * 0.5));
    for line in &measured.lines {
        let line_size = line.size(measured.line_height);
        let centered = point(px(anchor.x - f32::from(line_size.width) * 0.5), origin.y);
        let _ = line.paint(
            centered,
            measured.line_height,
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
            .push(TestPaintPrimitive::EdgeLabel { position: anchor });
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

    #[gpui::test]
    fn view_accepts_scene_with_custom_hasher(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let scene: Entity<
                GraphScene<&str, &str, (), (), std::collections::hash_map::RandomState>,
            > = cx.new(|_| {
                let mut scene =
                    GraphScene::with_hasher(std::collections::hash_map::RandomState::default());
                scene.merge(GraphBatch::new().node("a", ()).node("b", ()));
                let a = scene.node_id(&"a").unwrap();
                let b = scene.node_id(&"b").unwrap();
                scene.set_position(a, Vec2::new(-10.0, -20.0));
                scene.set_position(b, Vec2::new(30.0, 40.0));
                scene
            });
            let view = cx.new(|cx| GraphViewState::new(scene, cx));
            assert_eq!(view.read(cx).scene().read(cx).graph().node_count(), 2);
        });
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

    fn draw_graph_view<N: 'static, E: 'static>(
        cx: &mut VisualTestContext,
        view: &Entity<GraphViewState<&'static str, &'static str, N, E>>,
        origin: Vec2,
        canvas_size: Vec2,
    ) {
        cx.draw(
            point(px(origin.x), px(origin.y)),
            size(px(canvas_size.x), px(canvas_size.y)),
            |_, _| GraphView::new(view.clone()).into_element(),
        );
    }

    /// Draw `view` into a fresh window at `canvas_size` and then fit it to all
    /// nodes, returning the visual context for follow-up input.
    ///
    /// Fitting must run *after* the canvas has a real size. Calling
    /// [`GraphViewState::fit_all`] while the viewport size is still `(0,0)`
    /// computes `zoom = 0`, which `Viewport::fit_bounds` clamps to `0.0001` and
    /// collapses the whole graph to a single pixel (every edge degenerates to an
    /// empty path and disappears). The real view relies on the one-time initial
    /// auto-fit that runs during prepaint with a nonzero canvas size; this helper
    /// reproduces that ordering for tests that need an explicit fit.
    fn draw_and_fit_view<'a, N: 'static, E: 'static>(
        cx: &'a mut TestAppContext,
        view: &Entity<GraphViewState<&'static str, &'static str, N, E>>,
        canvas_size: Vec2,
    ) -> &'a mut VisualTestContext {
        let cx = cx.add_empty_window();
        draw_graph_view(cx, view, Vec2::ZERO, canvas_size);
        cx.update_entity(view, |state, cx| state.fit_all(cx));
        cx
    }

    #[gpui::test]
    fn profile_large_grid(cx: &mut TestAppContext) {
        let scene: Entity<GraphScene<&'static str, &'static str, (), ()>> = cx.new(|_| {
            let side = 50;
            let mut batch = GraphBatch::new();
            let mut ids = Vec::new();
            for y in 0..side {
                for x in 0..side {
                    let id = format!("{}x{}", x, y).leak() as &'static str;
                    batch = batch.node(id, ());
                    ids.push(id);
                }
            }
            let at = |x: usize, y: usize| ids[y * side + x];
            let mut ek = 0usize;
            for y in 0..side {
                for x in 0..side {
                    let id = at(x, y);
                    if x + 1 < side {
                        let key = format!("e{}", ek).leak() as &'static str;
                        batch = batch.edge(key, id, at(x + 1, y), EdgeDirection::Directed, ());
                        ek += 1;
                    }
                    if y + 1 < side {
                        let key = format!("e{}", ek).leak() as &'static str;
                        batch = batch.edge(key, id, at(x, y + 1), EdgeDirection::Directed, ());
                        ek += 1;
                    }
                }
            }
            let mut scene = GraphScene::new();
            scene.merge(batch);
            for y in 0..side {
                for x in 0..side {
                    let node = scene.node_id(&ids[y * side + x]).unwrap();
                    scene.set_position(node, Vec2::new(x as f32 * 60.0, y as f32 * 60.0));
                }
            }
            scene
        });
        let view = cx.new(|cx| GraphViewState::new(scene, cx));
        cx.update_entity(&view, |state, _| {
            let style = state.style_mut();
            style.edge_straight_threshold = 24.0;
            style.edge_straight_threshold_while_interacting = 10_000.0;
            style.edge_settle_time_ms = 250.0;
        });
        // Fit must happen after the canvas has a real size, or `fit_bounds`
        // collapses the whole graph to one pixel (zoom 0 -> 0.0001 clamp) and
        // every edge degenerates to an empty path. `draw_and_fit_view` enforces
        // that ordering for every caller.
        clear_test_paint_trace();
        draw_and_fit_view(cx, &view, Vec2::new(900.0, 700.0));
        let trace = take_test_paint_trace();
        let mut nodes = 0;
        let mut edges = 0;
        let mut arrows = 0;
        for p in &trace {
            match p {
                TestPaintPrimitive::Node { .. } => nodes += 1,
                TestPaintPrimitive::Edge { .. } => edges += 1,
                TestPaintPrimitive::Arrow { .. } => arrows += 1,
                _ => {}
            }
        }
        eprintln!(
            "PROF primitives: nodes={nodes} edges={edges} arrows={arrows} total={}",
            trace.len()
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
    fn input_before_any_sized_canvas_keeps_initial_auto_fit(cx: &mut TestAppContext) {
        let view = test_view(cx);
        // A zoom event lands before any laid-out frame sized the canvas. The
        // viewport has no pixel size yet, so the event cannot express camera
        // intent and must not consume the one-time fit.
        cx.update_entity(&view, |state, cx| {
            state.handle_zoom(Vec2::new(100.0, 60.0), 1.1, cx);
            assert!(
                state.initial_auto_fit.pending,
                "pre-layout input must not cancel the pending initial fit"
            );
        });
        // The next laid-out frame still fits the graph exactly once.
        let cx = cx.add_empty_window();
        draw_graph_view(cx, &view, Vec2::new(80.0, 40.0), Vec2::new(320.0, 240.0));
        cx.update_entity(&view, |state, _| {
            assert!(!state.initial_auto_fit.pending);
            assert!(
                state.viewport.zoom() > 1.0,
                "the deferred initial fit still ran, got zoom {}",
                state.viewport.zoom()
            );
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
    fn interaction_lod_elevates_threshold_while_active(cx: &mut TestAppContext) {
        let view = test_view(cx);
        cx.update_entity(&view, |state, _| {
            state.style_mut().edge_straight_threshold = 24.0;
            state.style_mut().edge_straight_threshold_while_interacting = 10_000.0;
            state.style_mut().edge_settle_time_ms = 300.0;
        });
        // Idle: the paint style matches the configured threshold.
        let idle = cx.update_entity(&view, |state, _| {
            (
                state.interaction_active_since.is_none(),
                state.paint_style(),
            )
        });
        assert!(idle.0, "no interaction active at rest");
        assert_eq!(idle.1.edge_straight_threshold, 24.0);
        assert_eq!(idle.1.edge_straight_threshold_while_interacting, 10_000.0);

        // After a zoom, the paint style uses the elevated interaction threshold.
        let (active, elevated) = cx.update_entity(&view, |state, cx| {
            state.handle_zoom(Vec2::new(100.0, 60.0), 1.1, cx);
            (
                state.interaction_active_since.is_some(),
                state.paint_style(),
            )
        });
        assert!(active, "zoom marks the view as interacting");
        assert_eq!(
            elevated.edge_straight_threshold, 10_000.0,
            "interaction threshold is used while active"
        );
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

        let trace = take_test_paint_trace();
        assert_eq!(trace.len(), 4);
        // A lone edge with no neighbors is straight. The arrow sits at the
        // trimmed edge's end, just outside the target node (radius 3 + gap 2 =
        // 5 from the node center at x=110), pointing left. The edge is trimmed
        // to the same boundary. Compare with a small tolerance because the
        // boundary is found by binary search.
        let approx = |a: Vec2, b: Vec2| (a - b).length() < 1e-2;
        match &trace[0] {
            TestPaintPrimitive::Arrow { source, target } => {
                assert!(approx(*source, Vec2::new(origin.x + 97.0, origin.y + 50.0)));
                assert!(approx(
                    *target,
                    Vec2::new(origin.x + 105.0, origin.y + 50.0)
                ));
            }
            other => panic!("expected arrow, got {other:?}"),
        }
        match &trace[1] {
            TestPaintPrimitive::Edge { source, target } => {
                assert!(approx(*source, Vec2::new(origin.x + 95.0, origin.y + 50.0)));
                assert!(approx(
                    *target,
                    Vec2::new(origin.x + 105.0, origin.y + 50.0)
                ));
            }
            other => panic!("expected edge, got {other:?}"),
        }
        assert_eq!(
            trace[2],
            TestPaintPrimitive::Node {
                origin: Vec2::new(origin.x + 87.0, origin.y + 47.0),
                size: Vec2::splat(6.0),
            }
        );
        assert_eq!(
            trace[3],
            TestPaintPrimitive::Node {
                origin: Vec2::new(origin.x + 107.0, origin.y + 47.0),
                size: Vec2::splat(6.0),
            }
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
    fn graph_view_omits_arrow_on_short_edge_under_arrow_lod(cx: &mut TestAppContext) {
        // test_view_with_edge places the nodes 20px apart (x=-10 and x=10), well
        // below a 50px arrow LOD threshold, so the directed edge's arrowhead is
        // omitted while the edge line and both nodes still paint.
        let view = test_view_with_edge(cx);
        cx.update_entity(&view, |state, _| {
            state.viewport_mut().set_size(Vec2::new(200.0, 100.0));
            state.viewport_mut().focus(Vec2::ZERO);
            state.style_mut().edge_arrow_min_length = 50.0;
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
            "a short directed edge below the arrow LOD threshold must omit its arrow"
        );
        assert!(
            trace
                .iter()
                .any(|primitive| matches!(primitive, TestPaintPrimitive::Edge { .. })),
            "the edge line itself should still be painted"
        );
        let node_count = trace
            .iter()
            .filter(|primitive| matches!(primitive, TestPaintPrimitive::Node { .. }))
            .count();
        assert_eq!(node_count, 2, "both nodes should still be painted");
    }

    #[test]
    fn visible_bezier_curves_skips_curves_inside_label_rect() {
        // A straight horizontal edge from (-20, 0) to (20, 0), with a label
        // rect covering x in [0, 10]. The edge is split exactly at the rect
        // edges, so two visible pieces remain, one on each side.
        let rect = Bounds {
            origin: point(px(0.0), px(-5.0)),
            size: size(px(10.0), px(10.0)),
        };
        let curves = visible_bezier_curves(
            Vec2::new(-20.0, 0.0),
            Vec2::new(0.0, 0.0),
            Vec2::new(20.0, 0.0),
            &[rect],
            0.0,
        );
        assert_eq!(curves.len(), 2);
        // The first piece ends exactly at the rect's left edge (x = 0); the
        // second starts exactly at the rect's right edge (x = 10).
        assert!((curves[0].2.x - 0.0).abs() < 1e-3);
        assert!((curves[1].0.x - 10.0).abs() < 1e-3);
    }

    #[test]
    fn visible_bezier_curves_stable_for_long_near_straight_edge() {
        // A long edge whose control point is far from the endpoints (a fanned
        // parallel edge). The curve is nearly straight and passes through the
        // label rect. The mask must be a small interval around the rect, not
        // the whole edge.
        let rect = Bounds {
            origin: point(px(0.0), px(-5.0)),
            size: size(px(10.0), px(10.0)),
        };
        // Alice at origin, Bob far to the right; the fanned control point is
        // far above, making a long, gently curved edge through the rect.
        let curves = visible_bezier_curves(
            Vec2::new(0.0, 0.0),
            Vec2::new(500.0, -2000.0),
            Vec2::new(1000.0, 0.0),
            &[rect],
            0.0,
        );
        assert!(!curves.is_empty(), "edge must not disappear");
        // The masked region must be small: the total visible length should be
        // close to the full edge length (only a small gap at the label).
        let total_visible = curves.iter().map(|(a, _, c)| (c - a).length()).sum::<f32>();
        let full = (Vec2::new(1000.0, 0.0) - Vec2::new(0.0, 0.0)).length();
        assert!(
            total_visible > full * 0.9,
            "mask must be small, got visible {total_visible} of {full}"
        );
    }

    #[test]
    fn visible_bezier_curves_returns_single_curve_when_no_label() {
        let curves = visible_bezier_curves(
            Vec2::new(-20.0, 0.0),
            Vec2::new(0.0, 0.0),
            Vec2::new(20.0, 0.0),
            &[],
            0.0,
        );
        assert_eq!(curves.len(), 1);
        assert_eq!(
            curves[0],
            (
                Vec2::new(-20.0, 0.0),
                Vec2::new(0.0, 0.0),
                Vec2::new(20.0, 0.0)
            )
        );
    }

    #[test]
    fn visible_bezier_curves_keeps_visible_pieces_at_high_zoom() {
        // Simulate a high-zoom scenario: the edge spans a large window-space
        // distance and a label rect sits near the center. The edge must not
        // disappear entirely; visible pieces must remain on both sides.
        let rect = Bounds {
            origin: point(px(0.0), px(-5.0)),
            size: size(px(10.0), px(10.0)),
        };
        let curves = visible_bezier_curves(
            Vec2::new(-1000.0, 0.0),
            Vec2::new(0.0, 0.0),
            Vec2::new(1000.0, 0.0),
            &[rect],
            0.0,
        );
        assert_eq!(curves.len(), 2, "edge must not disappear at high zoom");
        assert!(curves[0].2.x <= 0.0);
        assert!(curves[1].0.x >= 10.0);
    }

    #[test]
    fn visible_bezier_curves_keeps_curved_edge_at_high_zoom() {
        // A curved edge at high zoom: the control point is far from the label
        // rect, so most of the curve is clear of it. The edge must not
        // disappear entirely.
        let rect = Bounds {
            origin: point(px(0.0), px(-5.0)),
            size: size(px(10.0), px(10.0)),
        };
        let curves = visible_bezier_curves(
            Vec2::new(-1000.0, 0.0),
            Vec2::new(0.0, 500.0),
            Vec2::new(1000.0, 0.0),
            &[rect],
            0.0,
        );
        assert!(
            !curves.is_empty(),
            "curved edge must not disappear at high zoom"
        );
    }

    #[test]
    fn visible_bezier_curves_keeps_edge_when_label_centered_at_high_zoom() {
        // Reproduces the reported bug: at high zoom a label near the center of
        // a long edge caused the whole edge to disappear. The edge must remain
        // visible on both sides of the label.
        let rect = Bounds {
            origin: point(px(0.0), px(-5.0)),
            size: size(px(10.0), px(10.0)),
        };
        let curves = visible_bezier_curves(
            Vec2::new(-1.0e6, 0.0),
            Vec2::new(0.0, 0.0),
            Vec2::new(1.0e6, 0.0),
            &[rect],
            0.0,
        );
        assert_eq!(curves.len(), 2, "edge must not disappear at high zoom");
        assert!(curves[0].2.x <= 0.0);
        assert!(curves[1].0.x >= 10.0);
    }

    #[test]
    fn visible_bezier_curves_preserves_curve_shape() {
        // A curved edge whose control point is far from the label rect: the
        // curve does not pass behind the rect, so it is returned whole.
        let rect = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(10.0), px(10.0)),
        };
        let curves = visible_bezier_curves(
            Vec2::new(-20.0, 0.0),
            Vec2::new(0.0, 50.0),
            Vec2::new(20.0, 0.0),
            &[rect],
            0.0,
        );
        // The curve's y stays above the rect (y in [0, 10]), so it never enters
        // the rect and is returned whole.
        assert_eq!(curves.len(), 1);
        assert_eq!(
            curves[0],
            (
                Vec2::new(-20.0, 0.0),
                Vec2::new(0.0, 50.0),
                Vec2::new(20.0, 0.0)
            )
        );
    }

    #[test]
    fn rounded_label_mask_excludes_corner_arc() {
        // A label rect [0,10]x[0,10] with rounded corners of radius 4. Points
        // inside the flat edges are inside the mask; points in the corner arc
        // (outside the rounded rectangle) are not.
        let rr = RoundedRect::from_bounds(&Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(10.0), px(10.0)),
        });
        // Center of the flat bottom edge: inside.
        assert!(rr.contains(Vec2::new(5.0, 1.0)));
        // Along the corner, a point on the arc's concave side is outside.
        // Bottom-left arc center (4,4) with radius 4; (0.5, 1.5) is 4.3 from it.
        assert!(!rr.contains(Vec2::new(0.5, 1.5)));
        // A point still inside the rounded corner (within the arc radius).
        // (2.5, 1.5) is 2.9 from the arc center, so it is inside.
        assert!(rr.contains(Vec2::new(2.5, 1.5)));
    }

    #[test]
    fn rounded_mask_inflates_by_half_edge_width() {
        // The mask is inflated by half the edge width so the stroked edge's ink
        // (which extends beyond the centerline) does not paint over the label.
        // A 1px-wide edge has a 0.5px half-width, so a point just outside the
        // label rect must be inside the inflated mask but outside the
        // uninflated one.
        let rect = Bounds {
            origin: point(px(0.0), px(-5.0)),
            size: size(px(10.0), px(10.0)),
        };
        let plain = RoundedRect::from_bounds(&rect);
        let inflated = RoundedRect::from_bounds(&rect).inflated(0.5);
        // y = -5.3 is 0.3 below the label's top edge (y = -5): outside the
        // strict rect but within half an edge width of it.
        assert!(!plain.contains(Vec2::new(5.0, -5.3)));
        assert!(inflated.contains(Vec2::new(5.0, -5.3)));
        // The edge-width split happens through visible_bezier_curves too: a
        // horizontal edge just outside the rect is still masked when a stroke
        // half-width is given.
        let curves = visible_bezier_curves(
            Vec2::new(-20.0, -5.2),
            Vec2::new(0.0, -5.2),
            Vec2::new(20.0, -5.2),
            &[rect],
            1.0,
        );
        assert_eq!(curves.len(), 2, "edge near the label must still be masked");
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

    #[test]
    fn node_label_bounds_rect_cuts_edges_passing_behind_it() {
        // A node label sits below a node at the origin. An edge passing through
        // the label's rect must be cut into visible pieces on either side, just
        // like an edge label.
        let rect = Bounds {
            origin: point(px(0.0), px(10.0)),
            size: size(px(20.0), px(10.0)),
        };
        let curves = visible_bezier_curves(
            Vec2::new(-30.0, 15.0),
            Vec2::new(0.0, 15.0),
            Vec2::new(30.0, 15.0),
            &[rect],
            0.0,
        );
        assert_eq!(curves.len(), 2, "edge must be cut around the node label");
        assert!(curves[0].2.x <= 0.0);
        assert!(curves[1].0.x >= 20.0);
    }

    #[test]
    fn arrow_overlaps_rect_masks_arrow_triangle() {
        // A triangle arrowhead pointing left toward (80,50), tip at (100,50).
        let tip = Vec2::new(100.0, 50.0);
        let base = Vec2::new(80.0, 50.0);
        let normal = Vec2::new(0.0, -1.0);
        let half = 10.0;
        // A label rect covering the tip: the arrow overlaps it and must be
        // punched out there.
        let tip_rect = Bounds {
            origin: point(px(95.0), px(40.0)),
            size: size(px(10.0), px(20.0)),
        };
        assert!(
            arrow_overlaps_rect(tip, base, half, normal, &tip_rect),
            "a rect over the tip must overlap the arrow"
        );
        // A distant rect that does not touch the arrow: no overlap.
        let far_rect = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(10.0), px(10.0)),
        };
        assert!(
            !arrow_overlaps_rect(tip, base, half, normal, &far_rect),
            "a distant rect must not overlap the arrow"
        );
    }

    #[test]
    fn rect_intersection_clips_label_to_arrow_triangle() {
        // Triangle pointing left, tip at (100,50), base corners (80,60)/(80,40).
        let tip = Vec2::new(100.0, 50.0);
        let base = Vec2::new(80.0, 50.0);
        let normal = Vec2::new(0.0, -1.0);
        let half = 10.0;
        // A label rect whose left part pokes out beyond the arrow's base. The
        // hole must stop at the triangle's base edge (x=80), not cover the whole
        // rect — otherwise a gray strip would appear beyond the arrow.
        let rect = Bounds {
            origin: point(px(70.0), px(40.0)),
            size: size(px(40.0), px(20.0)),
        };
        let hole = rect_intersection(tip, base, half, normal, &rect, 0.0);
        assert!(hole.len() >= 3, "the clipped hole must be a polygon");
        for p in &hole {
            assert!(
                p.x >= 80.0 - 1e-3,
                "hole must not extend past the arrow base"
            );
        }
    }

    #[gpui::test]
    fn default_labels_use_display_and_can_be_overridden(cx: &mut TestAppContext) {
        let scene: Entity<GraphScene<&'static str, &'static str, &'static str, &'static str>> = cx
            .new(|_| {
                let mut scene = GraphScene::new();
                scene.merge(GraphBatch::new().node("a", "Alice").node("b", "Bob").edge(
                    "ab",
                    "a",
                    "b",
                    EdgeDirection::Directed,
                    "knows",
                ));
                let a = scene.node_id(&"a").unwrap();
                let b = scene.node_id(&"b").unwrap();
                scene.set_position(a, Vec2::new(-10.0, -20.0));
                scene.set_position(b, Vec2::new(30.0, 40.0));
                scene
            });
        let view: Entity<GraphViewState<&'static str, &'static str, &'static str, &'static str>> =
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
        let edge_labels: Vec<_> = trace
            .iter()
            .filter_map(|primitive| match primitive {
                TestPaintPrimitive::EdgeLabel { position } => Some(*position),
                _ => None,
            })
            .collect();
        assert_eq!(
            edge_labels.len(),
            1,
            "default edge labels should render for Display edges"
        );
    }

    #[test]
    fn label_hole_winds_opposite_its_triangle() {
        // A triangle arrowhead pointing right at (100, 50). A label rect
        // covering the tip clips the hole; under the nonzero fill rule the
        // hole must wind opposite its triangle, or the punch fails and the
        // label is covered.
        let style = GraphStyle::default();
        let source = Vec2::new(80.0, 50.0);
        let target = Vec2::new(100.0, 50.0);
        let arrow = TriangleArrow::new(source, target, &style).expect("non-degenerate");
        let triangle = [
            arrow.tip,
            arrow.base + arrow.normal * arrow.half,
            arrow.base - arrow.normal * arrow.half,
        ];
        let triangle_area = signed_area_times_two(&triangle);
        assert!(triangle_area != 0.0);

        let rect = Bounds {
            origin: point(px(95.0), px(40.0)),
            size: size(px(10.0), px(20.0)),
        };
        let hole = rect_intersection(arrow.tip, arrow.base, arrow.half, arrow.normal, &rect, 0.0);
        assert!(hole.len() >= 3, "the clipped hole must be a polygon");

        // The raw clipped polygon may wind either way depending on geometry;
        // after orientation it always opposes the triangle.
        let oriented = orient_hole_against(hole.clone(), triangle_area);
        assert!(
            signed_area_times_two(&oriented) * triangle_area < 0.0,
            "hole must wind opposite its triangle"
        );
        // Orientation only reverses order — same vertices.
        let mut sorted_hole = hole.clone();
        sorted_hole.sort_by(|a, b| (a.x, a.y).partial_cmp(&(b.x, b.y)).unwrap());
        let mut sorted_oriented = oriented.clone();
        sorted_oriented.sort_by(|a, b| (a.x, a.y).partial_cmp(&(b.x, b.y)).unwrap());
        for (a, b) in sorted_hole.iter().zip(sorted_oriented.iter()) {
            assert_eq!(a, b);
        }
        // Idempotent: re-orienting an already-opposed hole keeps it opposed.
        let again = orient_hole_against(oriented.clone(), triangle_area);
        assert!(
            signed_area_times_two(&again) * triangle_area < 0.0,
            "orientation must be stable"
        );
    }

    #[test]
    fn overlapping_same_color_triangles_fill_under_nonzero() {
        // Two triangles sharing a region: under the nonzero rule the shared
        // area's winding is +2, so a merged batch fills it instead of XOR-ing
        // a hole. This pins down why the fill builder must use NonZero.
        let style = GraphStyle::default();
        let a = TriangleArrow::new(Vec2::new(80.0, 50.0), Vec2::new(100.0, 50.0), &style)
            .expect("non-degenerate");
        let b = TriangleArrow::new(Vec2::new(85.0, 55.0), Vec2::new(105.0, 55.0), &style)
            .expect("non-degenerate");
        let tri_of = |arrow: &TriangleArrow| {
            vec![
                arrow.tip,
                arrow.base + arrow.normal * arrow.half,
                arrow.base - arrow.normal * arrow.half,
            ]
        };
        // Both wind the same way regardless of direction of travel here;
        // that is exactly what makes nonzero merge them into one filled
        // union.
        assert!(
            signed_area_times_two(&tri_of(&a)) * signed_area_times_two(&tri_of(&b)) > 0.0,
            "same-shape arrows must wind alike so nonzero unions them"
        );
    }

    #[gpui::test]
    fn batched_edge_strokes_render_every_visible_edge(cx: &mut TestAppContext) {
        // Three undirected chain edges with the middle one selected, so base
        // and selected stroke groups both accumulate. The per-color batching
        // must still paint every visible edge exactly once.
        let scene: Entity<GraphScene<&'static str, &'static str, (), ()>> = cx.new(|_| {
            let mut scene = GraphScene::new();
            scene.merge(
                GraphBatch::new()
                    .node("a", ())
                    .node("b", ())
                    .node("c", ())
                    .node("d", ())
                    .edge("ab", "a", "b", EdgeDirection::Undirected, ())
                    .edge("bc", "b", "c", EdgeDirection::Undirected, ())
                    .edge("cd", "c", "d", EdgeDirection::Undirected, ()),
            );
            for (key, x) in [("a", 0.0), ("b", 30.0), ("c", 60.0), ("d", 90.0)] {
                let id = scene.node_id(&key).expect("chain node exists");
                scene.set_position(id, Vec2::new(x, 5.0));
            }
            scene
        });
        let view: Entity<GraphViewState<&'static str, &'static str, (), ()>> =
            cx.new(|cx| GraphViewState::new(scene, cx));
        cx.update_entity(&view, |state, cx| {
            if let Some(edge) = state.scene.read(cx).edge_id(&"bc") {
                state.selection_mut().edges.push(edge);
            }
        });

        let visual = draw_and_fit_view(cx, &view, Vec2::new(200.0, 100.0));
        clear_test_paint_trace();
        draw_graph_view(visual, &view, Vec2::ZERO, Vec2::new(200.0, 100.0));
        let trace = take_test_paint_trace();

        let edges: Vec<_> = trace
            .iter()
            .filter(|p| matches!(p, TestPaintPrimitive::Edge { .. }))
            .collect();
        assert_eq!(edges.len(), 3, "every visible edge must be painted once");
        assert!(
            trace
                .iter()
                .any(|p| matches!(p, TestPaintPrimitive::Node { .. })),
            "nodes still paint after the edge batches"
        );
        assert!(
            !trace
                .iter()
                .any(|p| matches!(p, TestPaintPrimitive::Arrow { .. })),
            "undirected edges paint no arrowheads"
        );
    }

    #[gpui::test]
    fn batched_triangle_arrows_render_directed_edges(cx: &mut TestAppContext) {
        // Directed chain edges with the default triangle shape exercise the
        // nonzero-winding batched fill end to end: every edge paints once and
        // every directed edge emits exactly one arrowhead.
        let scene: Entity<GraphScene<&'static str, &'static str, (), ()>> = cx.new(|_| {
            let mut scene = GraphScene::new();
            scene.merge(
                GraphBatch::new()
                    .node("a", ())
                    .node("b", ())
                    .node("c", ())
                    .edge("ab", "a", "b", EdgeDirection::Directed, ())
                    .edge("bc", "b", "c", EdgeDirection::Directed, ()),
            );
            for (key, x) in [("a", 0.0), ("b", 40.0), ("c", 80.0)] {
                let id = scene.node_id(&key).expect("chain node exists");
                scene.set_position(id, Vec2::new(x, 5.0));
            }
            scene
        });
        let view: Entity<GraphViewState<&'static str, &'static str, (), ()>> =
            cx.new(|cx| GraphViewState::new(scene, cx));

        let visual = draw_and_fit_view(cx, &view, Vec2::new(200.0, 100.0));
        clear_test_paint_trace();
        draw_graph_view(visual, &view, Vec2::ZERO, Vec2::new(200.0, 100.0));
        let trace = take_test_paint_trace();

        let edges = trace
            .iter()
            .filter(|p| matches!(p, TestPaintPrimitive::Edge { .. }))
            .count();
        assert_eq!(edges, 2, "every visible edge must be painted once");
        let arrows = trace
            .iter()
            .filter(|p| matches!(p, TestPaintPrimitive::Arrow { .. }))
            .count();
        assert_eq!(
            arrows, 2,
            "each directed edge paints one triangle arrowhead"
        );
    }

    #[test]
    fn path_point_samples_along_segments() {
        // A straight path from (0,0) to (10,0). t=0 is the start, t=1 the end,
        // t=0.5 the midpoint.
        let path = vec![(
            Vec2::new(0.0, 0.0),
            Vec2::new(5.0, 0.0),
            Vec2::new(10.0, 0.0),
        )];
        assert_eq!(path_point(&path, 0.0), Vec2::new(0.0, 0.0));
        assert_eq!(path_point(&path, 0.5), Vec2::new(5.0, 0.0));
        assert_eq!(path_point(&path, 1.0), Vec2::new(10.0, 0.0));
        // t is clamped to [0, 1].
        assert_eq!(path_point(&path, 1.5), Vec2::new(10.0, 0.0));
    }

    #[gpui::test]
    fn edge_label_collisions_move_labels_apart(cx: &mut TestAppContext) {
        // Two edge labels on the same straight path, both starting at the
        // midpoint so they overlap. Collision resolution must slide them apart
        // along the path.
        let path = vec![(
            Vec2::new(0.0, 0.0),
            Vec2::new(50.0, 0.0),
            Vec2::new(100.0, 0.0),
        )];
        let mut frame = crate::paint::PaintFrame::new();
        frame.edge_labels.push(crate::paint::PaintEdgeLabel {
            edge: crate::graph::EdgeId::default(),
            position: Vec2::new(50.0, 0.0),
            offset: Vec2::new(0.0, -1.0),
            text: "alpha".to_string(),
            path: path.clone(),
            t: 0.5,
        });
        frame.edge_labels.push(crate::paint::PaintEdgeLabel {
            edge: crate::graph::EdgeId::default(),
            position: Vec2::new(50.0, 0.0),
            offset: Vec2::new(0.0, -1.0),
            text: "beta".to_string(),
            path: path.clone(),
            t: 0.5,
        });

        let cx = cx.add_empty_window();
        let style = GraphStyle::default();
        cx.update(|window, _| {
            let measures: Vec<Option<MeasuredLabel>> = frame
                .edge_labels
                .iter()
                .map(|label| measure_label(window, &label.text, &style))
                .collect();
            resolve_edge_label_collisions(&mut frame, &style, &[], &measures);
        });

        // The two labels must have moved to distinct positions along the path.
        let a = frame.edge_labels[0].position;
        let b = frame.edge_labels[1].position;
        assert!(
            (a - b).length() > 1.0,
            "labels must separate, got {a:?} and {b:?}"
        );
        // They stay on the path (y = 0).
        assert_eq!(a.y, 0.0);
        assert_eq!(b.y, 0.0);
    }

    #[gpui::test]
    fn edge_label_avoids_fixed_node_label(cx: &mut TestAppContext) {
        // An edge label sits at the midpoint of a path that passes through a
        // fixed node label's rect. The edge label must slide away along its
        // path, while the node label stays put.
        let path = vec![(
            Vec2::new(0.0, 0.0),
            Vec2::new(50.0, 0.0),
            Vec2::new(100.0, 0.0),
        )];
        let mut frame = crate::paint::PaintFrame::new();
        frame.edge_labels.push(crate::paint::PaintEdgeLabel {
            edge: crate::graph::EdgeId::default(),
            position: Vec2::new(50.0, 0.0),
            offset: Vec2::new(0.0, -1.0),
            text: "edge".to_string(),
            path: path.clone(),
            t: 0.5,
        });
        // A fixed node label whose rect covers the edge label's midpoint.
        let node_rect = Bounds {
            origin: point(px(40.0), px(-10.0)),
            size: size(px(20.0), px(20.0)),
        };

        let cx = cx.add_empty_window();
        let style = GraphStyle::default();
        cx.update(|window, _| {
            let measures: Vec<Option<MeasuredLabel>> = frame
                .edge_labels
                .iter()
                .map(|label| measure_label(window, &label.text, &style))
                .collect();
            resolve_edge_label_collisions(&mut frame, &style, &[node_rect], &measures);
        });

        // The edge label must have moved off the node label's rect.
        let pos = frame.edge_labels[0].position;
        assert!(
            pos.x < 40.0 || pos.x > 60.0,
            "edge label must move off the node label, got x = {}",
            pos.x
        );
        // It stays on the path (y = 0).
        assert_eq!(pos.y, 0.0);
    }

    #[gpui::test]
    fn longer_edge_label_moves_away_from_self_loop(cx: &mut TestAppContext) {
        // A self-loop label (two-segment onigiri path) collides with a longer
        // edge's label (single-segment path). The longer edge must slide away
        // from the self-loop while the self-loop stays put.
        let self_loop_path = vec![
            (
                Vec2::new(0.0, 0.0),
                Vec2::new(0.0, -20.0),
                Vec2::new(0.0, -40.0),
            ),
            (
                Vec2::new(0.0, -40.0),
                Vec2::new(0.0, -20.0),
                Vec2::new(0.0, 0.0),
            ),
        ];
        let long_path = vec![(
            Vec2::new(0.0, 0.0),
            Vec2::new(50.0, 0.0),
            Vec2::new(100.0, 0.0),
        )];
        let mut frame = crate::paint::PaintFrame::new();
        frame.edge_labels.push(crate::paint::PaintEdgeLabel {
            edge: crate::graph::EdgeId::default(),
            position: Vec2::new(0.0, -40.0),
            offset: Vec2::new(0.0, -1.0),
            text: "loop".to_string(),
            path: self_loop_path,
            t: 0.5,
        });
        frame.edge_labels.push(crate::paint::PaintEdgeLabel {
            edge: crate::graph::EdgeId::default(),
            position: Vec2::new(50.0, 0.0),
            offset: Vec2::new(0.0, -1.0),
            text: "long".to_string(),
            path: long_path,
            t: 0.5,
        });

        let cx = cx.add_empty_window();
        let style = GraphStyle::default();
        cx.update(|window, _| {
            let measures: Vec<Option<MeasuredLabel>> = frame
                .edge_labels
                .iter()
                .map(|label| measure_label(window, &label.text, &style))
                .collect();
            resolve_edge_label_collisions(&mut frame, &style, &[], &measures);
        });

        // The self-loop label stays at its base center.
        assert_eq!(frame.edge_labels[0].position, Vec2::new(0.0, -40.0));
        // The longer edge label must have moved off the self-loop label.
        let long_pos = frame.edge_labels[1].position;
        assert!(
            (long_pos - Vec2::new(0.0, -40.0)).length() > 1.0,
            "longer edge label must move away from the self-loop, got {long_pos:?}"
        );
    }

    #[test]
    fn edge_label_hidden_near_node_center() {
        // An edge label sits 10px from a node center. With the default hide
        // distance of 20px it must be hidden; raising the threshold keeps it.
        let mut frame = crate::paint::PaintFrame::new();
        frame.nodes.push(crate::paint::PaintNode {
            id: NodeId::default(),
            position: Vec2::new(0.0, 0.0),
            radius: 6.0,
            selected: false,
            hovered: false,
            overlay: crate::paint::OverlayCategory::None,
            simplified: false,
        });
        frame.edge_labels.push(crate::paint::PaintEdgeLabel {
            edge: crate::graph::EdgeId::default(),
            position: Vec2::new(10.0, 0.0),
            offset: Vec2::new(0.0, -1.0),
            text: "edge".to_string(),
            path: vec![(
                Vec2::new(0.0, 0.0),
                Vec2::new(50.0, 0.0),
                Vec2::new(100.0, 0.0),
            )],
            t: 0.5,
        });

        let default_style = GraphStyle::default();
        let mut measures: Vec<Option<MeasuredLabel>> =
            (0..frame.edge_labels.len()).map(|_| None).collect();
        hide_edge_labels_near_nodes(&mut frame, &default_style, &mut measures);
        assert_eq!(measures.len(), frame.edge_labels.len());
        assert!(
            frame.edge_labels.is_empty(),
            "a label 10px from a node center must be hidden at the default threshold"
        );

        // A far label is kept.
        frame.edge_labels.push(crate::paint::PaintEdgeLabel {
            edge: crate::graph::EdgeId::default(),
            position: Vec2::new(50.0, 50.0),
            offset: Vec2::new(0.0, -1.0),
            text: "far".to_string(),
            path: vec![(
                Vec2::new(0.0, 0.0),
                Vec2::new(50.0, 0.0),
                Vec2::new(100.0, 0.0),
            )],
            t: 0.5,
        });
        let wide_style = GraphStyle::default().with_edge_label_hide_distance(5.0);
        let mut measures: Vec<Option<MeasuredLabel>> =
            (0..frame.edge_labels.len()).map(|_| None).collect();
        hide_edge_labels_near_nodes(&mut frame, &wide_style, &mut measures);
        assert_eq!(
            frame.edge_labels.len(),
            1,
            "a label 50px away must survive a 5px hide distance"
        );
        assert_eq!(measures.len(), 1, "measurements stay aligned with labels");
        assert_eq!(frame.edge_labels[0].text, "far");
    }

    #[test]
    fn normal_edge_keeps_its_own_label_mask() {
        // The mask under an edge's own label is what cuts a readable gap in
        // the line beneath the text, so a normal edge must keep every rect.
        let edge = test_paint_edge(EdgeId::default(), false);
        let rects = vec![
            Bounds {
                origin: point(px(0.0), px(0.0)),
                size: size(px(10.0), px(10.0)),
            },
            Bounds {
                origin: point(px(20.0), px(20.0)),
                size: size(px(10.0), px(10.0)),
            },
        ];
        let owned = [(EdgeId::default(), rects[0])];
        let mut buf = Vec::new();
        let masks = stroke_masks(&edge, &owned, &rects, &mut buf);
        assert_eq!(masks.len(), 2, "normal edge keeps all mask rects");
    }

    #[test]
    fn self_loop_drops_only_its_own_label_mask() {
        // A self-loop's label parks beside the shape; its own rect must be
        // excluded while every other rect (other labels, node labels) stays.
        let loop_edge = test_paint_edge(EdgeId::default(), true);
        let own = (
            loop_edge.id,
            Bounds {
                origin: point(px(0.0), px(0.0)),
                size: size(px(10.0), px(10.0)),
            },
        );
        let other = (
            EdgeId::default(),
            Bounds {
                origin: point(px(50.0), px(50.0)),
                size: size(px(10.0), px(10.0)),
            },
        );
        let node = Bounds {
            origin: point(px(100.0), px(100.0)),
            size: size(px(10.0), px(10.0)),
        };
        let mut rects = vec![own.1, other.1, node];
        let owned = vec![own, other];
        let mut buf = Vec::new();
        let masks = stroke_masks(&loop_edge, &owned, &rects, &mut buf);
        assert_eq!(masks.len(), 2, "own rect dropped, the rest stay");
        assert!(
            !masks.contains(&own.1),
            "the self-loop's own rect must not mask its strokes"
        );
        rects.clear(); // keep `rects` alive for the borrow above
    }

    #[test]
    fn unlabeled_self_loop_keeps_all_masks() {
        // No own rect exists; nothing to drop.
        let loop_edge = test_paint_edge(EdgeId::default(), true);
        let rects = vec![Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(10.0), px(10.0)),
        }];
        let mut buf = Vec::new();
        let masks = stroke_masks(&loop_edge, &[], &rects, &mut buf);
        assert_eq!(masks.len(), 1);
    }

    /// A minimal [`crate::paint::PaintEdge`] for mask-selection tests.
    /// `loop_edge` puts source and target at the same point, which is how the
    /// render loop classifies self-loops.
    fn test_paint_edge(id: EdgeId, loop_edge: bool) -> crate::paint::PaintEdge {
        crate::paint::PaintEdge {
            id,
            source: Vec2::new(10.0, 10.0),
            target: Vec2::new(if loop_edge { 10.0 } else { 90.0 }, 10.0),
            path: Vec::new(),
            direction: EdgeDirection::Directed,
            selected: false,
            hovered: false,
            overlay: crate::paint::OverlayCategory::None,
            omit_arrow: false,
        }
    }

    #[gpui::test]
    fn self_loop_ink_survives_a_crowded_label_field(cx: &mut TestAppContext) {
        // Reproduces the basic-example neighborhood: a labelled self-loop
        // node with two labelled edges and an edge label per edge. After the
        // render pipeline masks strokes behind every label rectangle, most of
        // the onigiri's ink must survive - a loop reduced to fragments reads
        // as broken and tiny regardless of its geometric size.
        use crate::graph::Graph;
        use crate::paint::{PaintFrameInput, build_paint_frame};

        let mut g: Graph<&'static str, &'static str> = Graph::new();
        let a = g.add_node("alice");
        let b = g.add_node("bob");
        let c = g.add_node("carol");
        g.add_edge(a, b, EdgeDirection::Directed, "ab");
        g.add_edge(a, c, EdgeDirection::Directed, "ac");
        g.add_edge(a, a, EdgeDirection::Directed, "aa");

        let positions = move |id: NodeId| match id {
            id if id == a => Some(Vec2::new(400.0, 300.0)),
            id if id == b => Some(Vec2::new(560.0, 210.0)),
            id if id == c => Some(Vec2::new(470.0, 430.0)),
            _ => None,
        };
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(800.0, 600.0));
        vp.focus(Vec2::new(470.0, 315.0));
        vp.zoom_at(vp.size() * 0.5, 2.0);
        let style = GraphStyle::default();

        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &|_| None,
            node_label: &|_, name: &&'static str| Some((*name).to_string()),
            edge_label: &|_, name: &&'static str| Some((*name).to_string()),
            viewport: &vp,
            style: &style,
            selection: &Selection::new(),
            hover: &Hover::default(),
            node_overlay: None,
            edge_overlay: None,
        });

        let loop_edge = frame
            .edges
            .iter()
            .find(|e| e.source == e.target && e.path.len() > 1)
            .expect("self-loop present");
        let loop_path = loop_edge.path.clone();

        let cxw = cx.add_empty_window();
        cxw.update(|window, _| {
            // Mirror the render block: measure every label, resolve edge
            // label collisions against node labels, hide labels near nodes,
            // then build the mask rects exactly as painting does.
            let radius = style.node_screen_radius(vp.zoom());
            let node_measures: Vec<_> = frame
                .labels
                .iter()
                .map(|l| measure_label(window, &l.text, &style))
                .collect();
            let mut edge_measures: Vec<_> = frame
                .edge_labels
                .iter()
                .map(|l| measure_label(window, &l.text, &style))
                .collect();
            let node_rects: Vec<Bounds<gpui::Pixels>> = frame
                .labels
                .iter()
                .zip(&node_measures)
                .filter_map(|(l, m)| {
                    let m = m.as_ref()?;
                    Some(label_rect(m, l.position, |anchor, _| {
                        anchor.y + radius + style.label_offset
                    }))
                })
                .collect();
            let mut working = frame.clone();
            resolve_edge_label_collisions(&mut working, &style, &node_rects, &edge_measures);
            hide_edge_labels_near_nodes(&mut working, &style, &mut edge_measures);

            // The loop's own label rect never masks its own strokes.
            let own_idx = working
                .edge_labels
                .iter()
                .position(|l| l.edge == loop_edge.id)
                .expect("self-loop label present");
            let all_rects: Vec<Bounds<gpui::Pixels>> = working
                .edge_labels
                .iter()
                .zip(&edge_measures)
                .filter_map(|(l, m)| {
                    let m = m.as_ref()?;
                    let anchor = l.position + l.offset * style.label_offset;
                    Some(label_rect(m, anchor, |anchor, h| anchor.y - h * 0.5))
                })
                .collect();
            let rects: Vec<Bounds<gpui::Pixels>> = all_rects
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != own_idx)
                .map(|(_, r)| *r)
                .collect();

            let sample_len = |p0: Vec2, p1: Vec2, p2: Vec2| -> f32 {
                let mut len = 0.0;
                let mut prev = p0;
                for k in 1..=16 {
                    let t = k as f32 / 16.0;
                    let inv = 1.0 - t;
                    let pt = inv * inv * p0 + 2.0 * inv * t * p1 + t * t * p2;
                    len += (pt - prev).length();
                    prev = pt;
                }
                len
            };
            let mut kept = 0.0;
            let mut total = 0.0;
            for &(p0, p1, p2) in &loop_path {
                total += sample_len(p0, p1, p2);
                for (c0, c1, c2) in visible_edge_curves(p0, p1, p2, &rects, style.edge_width, None)
                {
                    kept += sample_len(c0, c1, c2);
                }
            }
            println!("loop ink kept {kept:.1} / {total:.1}");
            assert!(
                kept > total * 0.9,
                "label masks cut away too much of the self-loop: {kept:.1} of {total:.1}"
            );
        });
    }

    #[gpui::test]
    fn downward_loop_survives_low_zoom_with_node_label(cx: &mut TestAppContext) {
        // A single neighbor above bisects the loop axis to straight down -
        // exactly the band where the node label renders. At low zoom the loop
        // is a few pixels tall while the label box keeps its text size, so an
        // uncorrected loop is masked away almost entirely. The axis treats
        // the label direction as an obstacle, and the mirrored render
        // pipeline (node label rects included, exactly as painting masks)
        // must then keep most of the ink.
        use crate::graph::Graph;
        use crate::paint::{PaintFrameInput, build_paint_frame};

        let mut g: Graph<&'static str, &'static str> = Graph::new();
        let a = g.add_node("alice");
        let b = g.add_node("b");
        g.add_edge(a, b, EdgeDirection::Directed, "ab");
        g.add_edge(a, a, EdgeDirection::Directed, "aa");

        let positions = move |id: NodeId| match id {
            id if id == a => Some(Vec2::new(0.0, 0.0)),
            id if id == b => Some(Vec2::new(0.0, -700.0)),
            _ => None,
        };
        let mut vp = Viewport::new();
        vp.set_size(Vec2::new(800.0, 300.0));
        vp.fit_bounds(
            crate::viewport::WorldBounds {
                min: Vec2::new(-10.0, -710.0),
                max: Vec2::new(10.0, 10.0),
            },
            0.05,
        );
        // A realistic label gap; the zero default seats the label box against
        // the marker rim, where any loop leg grazes the band by construction.
        let style = GraphStyle {
            label_offset: 2.0,
            ..GraphStyle::default()
        };

        let frame = build_paint_frame(PaintFrameInput {
            graph: &g,
            node_position: &positions,
            node_cluster_center: &|_| None,
            node_label: &|_, name: &&'static str| Some((*name).to_string()),
            edge_label: &|_, name: &&'static str| Some((*name).to_string()),
            viewport: &vp,
            style: &style,
            selection: &Selection::new(),
            hover: &Hover::default(),
            node_overlay: None,
            edge_overlay: None,
        });

        let loop_edge = frame
            .edges
            .iter()
            .find(|e| e.source == e.target)
            .expect("self-loop present");
        let loop_path = loop_edge.path.clone();
        assert!(
            !loop_path.is_empty(),
            "the loop must be drawable at this zoom"
        );
        // Axis contract: not downward (right side per the tie rule).
        let base = loop_path[0].2;
        assert!(
            (base.y - loop_edge.source.y).abs() < 1e-3 && base.x > loop_edge.source.x,
            "loop should dodge out of the label band; base {base:?}"
        );

        let cxw = cx.add_empty_window();
        cxw.update(|window, _| {
            // Mirror the render block: measure labels, resolve collisions,
            // hide near-node labels, then mask strokes behind every rect -
            // node label rects included, with stroke_masks selecting them.
            let radius = style.node_screen_radius(vp.zoom());
            let node_measures: Vec<_> = frame
                .labels
                .iter()
                .map(|l| measure_label(window, &l.text, &style))
                .collect();
            let mut edge_measures: Vec<_> = frame
                .edge_labels
                .iter()
                .map(|l| measure_label(window, &l.text, &style))
                .collect();
            let node_rects: Vec<Bounds<gpui::Pixels>> = frame
                .labels
                .iter()
                .zip(&node_measures)
                .filter_map(|(l, m)| {
                    let m = m.as_ref()?;
                    Some(label_rect(m, l.position, |anchor, _| {
                        anchor.y + radius + style.label_offset
                    }))
                })
                .collect();
            let mut working = frame.clone();
            resolve_edge_label_collisions(&mut working, &style, &node_rects, &edge_measures);
            hide_edge_labels_near_nodes(&mut working, &style, &mut edge_measures);

            let edge_rects: Vec<(EdgeId, Bounds<gpui::Pixels>)> = working
                .edge_labels
                .iter()
                .zip(&edge_measures)
                .filter_map(|(l, m)| {
                    let m = m.as_ref()?;
                    let anchor = l.position + l.offset * style.label_offset;
                    Some((l.edge, label_rect(m, anchor, |a, h| a.y - h * 0.5)))
                })
                .collect();
            let mut rects: Vec<Bounds<gpui::Pixels>> = edge_rects.iter().map(|(_, r)| *r).collect();
            rects.extend(node_rects);

            let mut own_buf = Vec::new();
            let masks = stroke_masks(loop_edge, &edge_rects, &rects, &mut own_buf);

            let sample_len = |p0: Vec2, p1: Vec2, p2: Vec2| -> f32 {
                let mut len = 0.0;
                let mut prev = p0;
                for k in 1..=16 {
                    let t = k as f32 / 16.0;
                    let inv = 1.0 - t;
                    let pt = inv * inv * p0 + 2.0 * inv * t * p1 + t * t * p2;
                    len += (pt - prev).length();
                    prev = pt;
                }
                len
            };
            let mut kept = 0.0;
            let mut total = 0.0;
            for &(p0, p1, p2) in &loop_path {
                total += sample_len(p0, p1, p2);
                for (c0, c1, c2) in visible_edge_curves(p0, p1, p2, masks, style.edge_width, None) {
                    kept += sample_len(c0, c1, c2);
                }
            }
            for (i, r) in rects.iter().enumerate() {
                println!(
                    "rect[{i}] x[{:.1},{:.1}] y[{:.1},{:.1}]",
                    f32::from(r.origin.x),
                    f32::from(r.origin.x) + f32::from(r.size.width),
                    f32::from(r.origin.y),
                    f32::from(r.origin.y) + f32::from(r.size.height)
                );
            }
            println!("loop source {:?} base {base:?}", loop_edge.source);
            println!("downward-loop ink kept {kept:.1} / {total:.1}");
            assert!(
                kept > total * 0.9,
                "label masks cut away too much of the dodged self-loop: {kept:.1} of {total:.1}"
            );
        });
    }

    #[test]
    fn visible_edge_curves_returns_whole_curve_when_inside_viewport() {
        // When every control point lies inside the viewport and there are no
        // labels, the convex-hull fast path must return the untouched curve
        // (no splitting), so the common in-viewport case skips per-edge interval
        // and masking work.
        let viewport = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(200.0), px(100.0)),
        };
        let (p0, p1, p2) = (
            Vec2::new(20.0, 20.0),
            Vec2::new(100.0, 80.0),
            Vec2::new(180.0, 20.0),
        );
        let curves = visible_edge_curves(p0, p1, p2, &[], 2.0, Some(&viewport));
        assert_eq!(curves.len(), 1, "fully-inside curve is a single segment");
        let (q0, q1, q2) = curves[0];
        assert_eq!((q0, q1, q2), (p0, p1, p2), "curve is returned untouched");
    }

    #[test]
    fn visible_edge_curves_keeps_only_viewport_piece() {
        // A long horizontal curve whose endpoints are far outside the viewport;
        // only the sub-curve crossing it must be kept, and its coordinates must
        // lie within the viewport.
        let viewport = Bounds {
            origin: point(px(100.0), px(50.0)),
            size: size(px(200.0), px(100.0)),
        };
        let curves = visible_edge_curves(
            Vec2::new(-10000.0, 100.0),
            Vec2::new(0.0, 100.0),
            Vec2::new(10000.0, 100.0),
            &[],
            0.0,
            Some(&viewport),
        );
        assert!(!curves.is_empty(), "a crossing curve must be kept");
        for (p0, p1, p2) in &curves {
            for p in [p0, p1, p2] {
                assert!(
                    p.x >= 100.0 && p.x <= 300.0,
                    "clipped control point {p:?} must lie within the viewport's x range"
                );
            }
        }
    }

    #[test]
    fn visible_edge_curves_returns_empty_when_fully_outside() {
        let viewport = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(100.0), px(100.0)),
        };
        // A curve entirely to the right of the viewport.
        let curves = visible_edge_curves(
            Vec2::new(500.0, 50.0),
            Vec2::new(600.0, 50.0),
            Vec2::new(700.0, 50.0),
            &[],
            0.0,
            Some(&viewport),
        );
        assert!(curves.is_empty(), "a fully-outside curve must be dropped");
    }

    #[test]
    fn visible_edge_curves_masks_label_within_viewport() {
        // A curve inside the viewport with a label rect in the middle: the
        // visible pieces must be on both sides of the label, within the viewport.
        let viewport = Bounds {
            origin: point(px(-100.0), px(-50.0)),
            size: size(px(200.0), px(100.0)),
        };
        let label = Bounds {
            origin: point(px(-10.0), px(-5.0)),
            size: size(px(20.0), px(10.0)),
        };
        let curves = visible_edge_curves(
            Vec2::new(-100.0, 0.0),
            Vec2::new(0.0, 0.0),
            Vec2::new(100.0, 0.0),
            &[label],
            0.0,
            Some(&viewport),
        );
        assert_eq!(curves.len(), 2, "edge must be split around the label");
        assert!(curves[0].2.x <= -10.0);
        assert!(curves[1].0.x >= 10.0);
    }

    #[test]
    fn arrow_outside_viewport_rejects_off_screen_arrows() {
        let viewport = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(800.0), px(600.0)),
        };
        // An arrow far to the right of the viewport.
        let tip = Vec2::new(5000.0, 300.0);
        let base = Vec2::new(4990.0, 300.0);
        let normal = Vec2::new(0.0, -1.0);
        assert!(
            arrow_outside_viewport(tip, base, normal, 5.0, &viewport),
            "an arrow far off-screen must be rejected"
        );
        // An arrow inside the viewport.
        let tip = Vec2::new(400.0, 300.0);
        let base = Vec2::new(390.0, 300.0);
        assert!(
            !arrow_outside_viewport(tip, base, normal, 5.0, &viewport),
            "an arrow inside the viewport must be kept"
        );
    }

    #[test]
    fn point_near_viewport_accepts_inside_and_rejects_far() {
        let viewport = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(800.0), px(600.0)),
        };
        // Inside the viewport.
        assert!(point_near_viewport(
            Vec2::new(400.0, 300.0),
            &viewport,
            200.0
        ));
        // Just outside the viewport but within the margin.
        assert!(point_near_viewport(
            Vec2::new(900.0, 300.0),
            &viewport,
            200.0
        ));
        // Far outside the viewport, beyond the margin.
        assert!(!point_near_viewport(
            Vec2::new(5000.0, 300.0),
            &viewport,
            200.0
        ));
    }
}
