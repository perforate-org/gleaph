//! The graph panel: a Rune host for `graph-view-core`.
//!
//! This is the adapter the crate split exists for. Everything that decides *what* is on screen
//! comes from the core (`GraphScene`, `build_indexed_paint_frame`, `hit_test`, `Viewport`,
//! `GraphStyle`); this module turns a frame into Rune geometry, maps pointers onto the core's
//! viewport, and places labels using Rune's text measurement.
//!
//! The frame is built in canvas pixels for the current camera, so a camera change rebuilds it: the
//! core's culling, LOD and label budget are camera-dependent by design. Rune retains the resulting
//! shape between rebuilds, which is what keeps a still frame free.

use glam::Vec2;
use graph_view_core::{
    EdgeDirection, ForceAtlas2, GraphBatch, GraphRuntime, GraphScene, GraphStyle, Hover,
    LayoutBudget, LayoutProgress, LayoutSource, OverlayCategory, Selection, Viewport, WorldBounds,
    build_indexed_paint_frame, hit_test,
};
use rune::{Color, El, LineCap, Shape, TextStyle, row, text};

/// A placed label: screen position and its text.
#[derive(Clone)]
pub struct PlacedLabel {
    pub position: [f32; 2],
    pub text: String,
    pub emphasized: bool,
}

/// The console's graph panel state.
pub struct GraphPanel {
    scene: GraphScene<u32, u32, (), ()>,
    runtime: GraphRuntime,
    viewport: Viewport,
    style: GraphStyle,
    selection: Selection,
    hover: Hover,
    /// Node keys in insertion order, so a hover can name a node without a reverse map.
    keys: Vec<u32>,
    /// `NodeId` -> key, for label text and the inspector.
    node_keys: std::rc::Rc<std::collections::HashMap<graph_view_core::NodeId, u32>>,
    edge_keys: std::rc::Rc<std::collections::HashMap<graph_view_core::EdgeId, u32>>,
    /// What colours encode: one categorical dimension at a time.
    color_by: ColorBy,
    /// A legend row hovered right now (transient).
    hover_emphasis: Option<Emphasis>,
    /// A legend row clicked (sticky until clicked again).
    filter_emphasis: Option<Emphasis>,
    edge_categories: crate::theme::Categories,
    edge_label_counts: Vec<usize>,
    /// The frame for `viewport`, and the geometry built from it.
    shape: Shape,
    labels: Vec<PlacedLabel>,
    splats: Vec<graph_view_core::PaintEdgeSplat>,
    /// Ink for the density ramp: the palette's faint ink (the APCA large-text level), so the
    /// densest cell reads as a non-text indicator against the panel rather than a bright wash.
    density_ink: Color,
    /// Ring drawn around a selected node. The core paints only a fill per state, and an ink fill
    /// alone reads as "disabled" on the light theme; the ring says "chosen" in both.
    selection_ring: Color,
    categories: crate::theme::Categories,
    /// The canvas colour, used to cut a gap between ring sectors.
    ring_gap: Color,
    /// Counts of each primary label, and how many nodes carry more than one label.
    label_counts: Vec<usize>,
    /// The application's label vocabulary: the adapter knows no Gleaph label names.
    label_tables: LabelTables,
    multi_label: usize,
    /// The aggregate layer as a coarse image the shader upscales bilinearly.
    density: Option<rune::Pixels>,
    density_build_us: u128,
    nodes_drawn: usize,
    edges_drawn: usize,
    /// Camera, positions, selection or hover changed since the last build.
    dirty: bool,
    /// The style's resting straight-edge threshold, restored when the layout settles.
    resting_straight_threshold: f32,
    /// The keyboard cursor node (`None` until an arrow key is pressed).
    cursor: Option<graph_view_core::NodeId>,
    /// Screen positions of the last painted nodes, for keyboard stepping.
    node_positions: Vec<(graph_view_core::NodeId, [f32; 2])>,
}

/// The label vocabulary a graph draws its colour coding and label rings from.
///
/// The adapter has no opinion about Gleaph's labels: the application supplies the schema's node and
/// edge label names, and the panel indexes into them.
#[derive(Clone, Debug, Default)]
pub struct LabelTables {
    pub node: Vec<String>,
    pub edge: Vec<String>,
}

impl GraphPanel {
    /// Build a panel over a synthetic graph, and fit the camera to it.
    pub fn new(
        nodes: usize,
        edges: usize,
        width: f32,
        height: f32,
        label_tables: LabelTables,
    ) -> Self {
        let force = std::env::var("GRAPH_FA2").is_ok();
        let mut scene: GraphScene<u32, u32, (), ()> = if force {
            GraphScene::new().with_layout(Box::new(ForceAtlas2::default()))
        } else {
            GraphScene::new()
        };
        // Off-thread layout: the scene parks its engine, hands it to a worker thread, and adopts
        // the result on a later step. The frame thread never runs an iteration.
        scene.set_layout_source(LayoutSource::BackgroundThread);
        scene.merge(synthetic(nodes, edges));
        scene.rebuild_layout();
        if !force {
            // A clustered placement, so the snapshot shows a graph rather than a layout artifact.
            let ids: Vec<_> = scene.graph().nodes().map(|(id, _)| id).collect();
            for (index, id) in ids.into_iter().enumerate() {
                scene.set_position(id, clustered_position(index, nodes));
            }
        }

        let style = GraphStyle {
            // With the square-root zoom law the base value is only the 1x size: 5 points at zoom 1,
            // ~2.7 at the fitted 0.29, and the 8-point cap keeps a deep zoom from turning markers into
            // discs. Tuned by comparing fitted and 1.6x renders.
            node_radius: 5.0,
            node_max_screen_radius: 8.0,
            node_stroke_width: 1.0,
            edge_width: 1.4,
            edge_arrow_enabled: false,
            label_budget: Some(220),
            ..GraphStyle::default()
        };

        let resting_straight_threshold = style.edge_straight_threshold;
        let mut panel = Self {
            scene,
            runtime: GraphRuntime::new(),
            viewport: Viewport::new(),
            style,
            selection: Selection::new(),
            hover: Hover::default(),
            keys: (0..nodes as u32).collect(),
            node_keys: std::rc::Rc::new(std::collections::HashMap::new()),
            edge_keys: std::rc::Rc::new(std::collections::HashMap::new()),
            color_by: ColorBy::NodeLabel,
            hover_emphasis: None,
            filter_emphasis: None,
            edge_categories: crate::theme::Palette::dark().edge_categories(),
            edge_label_counts: vec![0; label_tables.edge.len()],
            shape: Shape::new(),
            labels: Vec::new(),
            splats: Vec::new(),
            density_ink: Color {
                r: 0.72,
                g: 0.75,
                b: 0.82,
                a: 1.0,
            },
            selection_ring: Color {
                r: 0.16,
                g: 0.5,
                b: 1.0,
                a: 1.0,
            },
            categories: crate::theme::Palette::dark().categories(),
            ring_gap: Color {
                r: 0.043,
                g: 0.051,
                b: 0.071,
                a: 1.0,
            },
            label_counts: {
                let mut counts = vec![0; label_tables.node.len()];
                for key in 0..nodes as u32 {
                    if let Some(primary) = labels_for(key, label_tables.node.len()).first() {
                        counts[*primary] += 1;
                    }
                }
                counts
            },
            multi_label: (0..nodes as u32)
                .filter(|key| labels_for(*key, label_tables.node.len()).len() > 1)
                .count(),
            density: None,
            density_build_us: 0,
            nodes_drawn: 0,
            edges_drawn: 0,
            dirty: true,
            resting_straight_threshold,
            cursor: None,
            node_positions: Vec::new(),
            label_tables,
        };
        panel.node_keys = std::rc::Rc::new(
            panel
                .scene
                .graph()
                .nodes()
                .enumerate()
                .map(|(index, (id, _))| (id, index as u32))
                .collect(),
        );
        panel.edge_keys = std::rc::Rc::new(
            panel
                .scene
                .graph()
                .edges()
                .enumerate()
                .map(|(index, (id, _))| (id, index as u32))
                .collect(),
        );
        for index in 0..edges as u32 {
            let edge_label = panel.edge_label_of(index);
            if let Some(count) = panel.edge_label_counts.get_mut(edge_label) {
                *count += 1;
            }
        }
        panel.resize(width, height);
        if let Ok(zoom) = std::env::var("GRAPH_ZOOM")
            && let Ok(zoom) = zoom.parse::<f32>()
        {
            panel
                .viewport
                .zoom_at(Vec2::new(width * 0.5, height * 0.5), zoom);
        }
        if let Ok(threshold) = std::env::var("GRAPH_AGGREGATE")
            && let Ok(threshold) = threshold.parse::<usize>()
        {
            panel.style.edge_aggregate_threshold = Some(threshold);
        }
        if let Ok(threshold) = std::env::var("GRAPH_STRAIGHT")
            && let Ok(threshold) = threshold.parse::<f32>()
        {
            panel.style.edge_straight_threshold = threshold;
        }
        panel.dirty = true;
        panel
    }

    /// Resize the canvas and refit the camera the first time it gains a size.
    /// The graph viewport's current size in points.
    pub fn size(&self) -> Vec2 {
        self.viewport.size()
    }

    pub fn resize(&mut self, width: f32, height: f32) {
        let was_unsized = self.viewport.size() == Vec2::ZERO;
        self.viewport.set_size(Vec2::new(width, height));
        if was_unsized {
            self.fit();
        }
        self.dirty = true;
    }

    /// Fit the camera to the graph's world bounds.
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
        self.dirty = true;
    }

    /// The zoom factor the core viewport is using.
    pub fn zoom(&self) -> f32 {
        self.viewport.zoom()
    }

    /// Pan by a screen-space delta (a pointer drag).
    pub fn pan(&mut self, delta: [f32; 2]) {
        self.viewport.pan(Vec2::new(delta[0], delta[1]));
        self.dirty = true;
    }

    /// Zoom about a screen point (a wheel or pinch).
    pub fn zoom_at(&mut self, point: [f32; 2], factor: f32) {
        self.viewport.zoom_at(Vec2::new(point[0], point[1]), factor);
        self.dirty = true;
    }

    /// Hover the element under a screen point, returning `true` when it changed.
    pub fn hover_at(&mut self, point: [f32; 2]) -> bool {
        let synced = self.scene.sync_runtime(&mut self.runtime);
        let result = hit_test(
            &synced,
            &self.viewport,
            &self.style,
            Vec2::new(point[0], point[1]),
            &|_, _: &()| None,
            self.splats.is_empty(),
        );
        let hover = Hover {
            node: result.node,
            edge: result.edge,
        };
        if hover == self.hover {
            return false;
        }
        self.hover = hover;
        self.dirty = true;
        true
    }

    /// Select the element under a screen point (or clear the selection on empty space).
    pub fn click_at(&mut self, point: [f32; 2]) -> bool {
        let synced = self.scene.sync_runtime(&mut self.runtime);
        let result = hit_test(
            &synced,
            &self.viewport,
            &self.style,
            Vec2::new(point[0], point[1]),
            &|_, _: &()| None,
            self.splats.is_empty(),
        );
        let selection = Selection {
            nodes: result.node.into_iter().collect(),
            edges: result.edge.into_iter().collect(),
        };
        if selection == self.selection {
            return false;
        }
        self.selection = selection;
        self.dirty = true;
        true
    }

    /// Clear the selection (a context-menu action).
    pub fn clear_selection(&mut self) -> bool {
        if self.selection.nodes.is_empty() && self.selection.edges.is_empty() {
            return false;
        }
        self.selection = graph_view_core::Selection::new();
        self.dirty = true;
        true
    }

    /// The label of the element under a screen point, for an inspector.
    pub fn pick_label(&self, point: [f32; 2]) -> Option<(String, String)> {
        self.pick(point)
    }

    /// The hit for a screen point, against a freshly synced runtime.
    fn pick(&self, point: [f32; 2]) -> Option<(String, String)> {
        let mut runtime = GraphRuntime::new();
        let synced = self.scene.sync_runtime(&mut runtime);
        let result = hit_test(
            &synced,
            &self.viewport,
            &self.style,
            Vec2::new(point[0], point[1]),
            &|_, _: &()| None,
            self.splats.is_empty(),
        );
        if let Some(node) = result.node {
            let index = self
                .keys
                .iter()
                .position(|key| self.scene.node_id(key) == Some(node))
                .unwrap_or_default();
            return Some(("node".to_owned(), format!("N{index:04}")));
        }
        result.edge.map(|edge| {
            let count = self.scene.graph().edge_count();
            let index = self
                .scene
                .graph()
                .edges()
                .position(|(id, _)| id == edge)
                .unwrap_or(0);
            ("edge".to_owned(), format!("E{index:05} / {count}"))
        })
    }

    /// Step the layout by one budget; `true` when positions moved.
    ///
    /// While the layout is still moving, edges draw straight (the core's existing motion LOD,
    /// normally scoped to interaction): a moving graph does not need curved bows, and skipping
    /// them and the obstacle pass removes about a third of the frame build on every adopted
    /// snapshot. The resting threshold returns as soon as the layout settles.
    pub fn step_layout(&mut self, budget: LayoutBudget) -> bool {
        if std::env::var("GRAPH_PAUSE").is_ok() {
            return false;
        }
        let progress = self.scene.step_layout(budget);
        let moving = progress != LayoutProgress::Settled;
        if moving {
            if self.style.edge_straight_threshold < f32::MAX {
                self.style.edge_straight_threshold = f32::MAX;
                self.dirty = true;
            }
        } else if (self.style.edge_straight_threshold - self.resting_straight_threshold).abs() > 0.0
        {
            self.style.edge_straight_threshold = self.resting_straight_threshold;
            self.dirty = true;
        }
        if moving {
            self.dirty = true;
        }
        moving
    }

    /// Whether the layout has settled.
    pub fn layout_settled(&self) -> bool {
        self.scene.layout_state() == graph_view_core::LayoutRunState::Settled
    }

    /// Rebuild the frame and geometry if anything changed since the last call.
    pub fn sync(&mut self) -> bool {
        if !self.dirty {
            return false;
        }
        self.dirty = false;
        let started = std::time::Instant::now();
        let sync_started = std::time::Instant::now();
        // Read the emphasis before the mutable runtime borrow, then bind the overlay closures as
        // locals so their lifetime matches the frame input.
        let emphasis = self.hover_emphasis.or(self.filter_emphasis);
        let synced = self.scene.sync_runtime(&mut self.runtime);
        let sync_us = sync_started.elapsed().as_micros();
        let mut style = self.style.clone();
        style.edge_aggregate_threshold =
            self.style.aggregate_threshold_for(!self.splats.is_empty());
        let node_keys = std::rc::Rc::clone(&self.node_keys);
        let edge_keys = std::rc::Rc::clone(&self.edge_keys);
        let node_label_count = self.label_tables.node.len();
        let edge_label_count = self.label_tables.edge.len();
        let node_overlay = move |id: graph_view_core::NodeId| {
            let Some(emphasis) = emphasis else {
                return OverlayCategory::None;
            };
            match emphasis {
                Emphasis::NodeLabel(index) => {
                    let primary = node_keys
                        .get(&id)
                        .map(|key| labels_for(*key, node_label_count)[0]);
                    if primary == Some(index) {
                        OverlayCategory::None
                    } else {
                        OverlayCategory::Dimmed
                    }
                }
                // The other dimension stays as context: its members are not the subject.
                Emphasis::EdgeLabel(_) => OverlayCategory::None,
            }
        };
        let edge_overlay = move |id: graph_view_core::EdgeId| {
            let Some(emphasis) = emphasis else {
                return OverlayCategory::None;
            };
            match emphasis {
                Emphasis::NodeLabel(_) => OverlayCategory::Dimmed,
                Emphasis::EdgeLabel(index) => {
                    let label = edge_keys
                        .get(&id)
                        .map(|key| edge_label_for(*key, edge_label_count));
                    if label == Some(index) {
                        OverlayCategory::None
                    } else {
                        OverlayCategory::Dimmed
                    }
                }
            }
        };
        let frame = build_indexed_paint_frame(graph_view_core::IndexedPaintFrameInput {
            synced: &synced,
            node_label: &|id, _: &()| {
                self.node_keys.get(&id).map(|key| {
                    let extras = labels_for(*key, node_label_count).len().saturating_sub(1);
                    if extras > 0 {
                        format!("N{key} +{extras}")
                    } else {
                        format!("N{key}")
                    }
                })
            },
            edge_label: &|_, _: &()| None,
            viewport: &self.viewport,
            style: &style,
            selection: &self.selection,
            hover: &self.hover,
            node_overlay: Some(&node_overlay),
            edge_overlay: Some(&edge_overlay),
        });
        let frame_us = started.elapsed().as_micros();
        self.node_positions = frame
            .nodes
            .iter()
            .map(|node| (node.id, [node.position.x, node.position.y]))
            .collect();
        self.nodes_drawn = frame.nodes.len();
        self.edges_drawn = frame.edges.len();
        self.splats = frame.edge_splats.clone();
        let shape_started = std::time::Instant::now();
        self.shape = self.build_shape(&frame);
        let shape_us = shape_started.elapsed().as_micros();
        let density_started = std::time::Instant::now();
        self.build_density();
        let density_us = density_started.elapsed().as_micros();
        let labels_started = std::time::Instant::now();
        self.labels = self.place_labels(&frame);
        let labels_us = labels_started.elapsed().as_micros();
        {
            let mut r_min = f32::INFINITY;
            let mut r_max = 0.0f32;
            let mut px_min = Vec2::splat(f32::INFINITY);
            let mut px_max = Vec2::splat(f32::NEG_INFINITY);
            for node in &frame.nodes {
                r_min = r_min.min(node.radius);
                r_max = r_max.max(node.radius);
                px_min = px_min.min(node.position);
                px_max = px_max.max(node.position);
            }
            let pieces: usize = frame.edges.iter().map(|e| e.path.len()).sum();
            let curved = frame.edges.iter().filter(|e| e.path.len() > 1).count();
            eprintln!(
                "sync: {frame_us} us frame (runtime {sync_us} + frame + shape {shape_us} + density {density_us} + labels {labels_us}) | nodes {} edges {} | radius {r_min:.2}..{r_max:.2} | pos {px_min:?}..{px_max:?} | pieces {pieces} multi-piece {curved} | labels {} | zoom {:.3}",
                frame.nodes.len(),
                frame.edges.len(),
                frame.labels.len(),
                self.viewport.zoom()
            );
        }
        true
    }

    /// The geometry to draw, in canvas pixels for the current camera.
    pub fn shape(&self) -> Shape {
        self.shape.clone()
    }

    /// Show or hide node labels (the label budget is the core's mechanism).
    pub fn set_labels(&mut self, show: bool) {
        self.style.label_budget = if show { Some(220) } else { Some(0) };
        self.dirty = true;
    }

    /// Draw curved edges or the straight motion-LOD form.
    pub fn set_curved(&mut self, curved: bool) {
        self.resting_straight_threshold = if curved { 24.0 } else { f32::MAX };
        self.style.edge_straight_threshold = self.resting_straight_threshold;
        self.dirty = true;
    }

    /// Aggregate edges into the density layer (the core draws cells instead of individual edges).
    pub fn set_aggregate(&mut self, aggregate: bool) {
        self.style.edge_aggregate_threshold = if aggregate { Some(0) } else { None };
        self.dirty = true;
    }

    /// Pin every selected node (or unpin them), so the layout leaves them where they are.
    pub fn set_selection_pinned(&mut self, pinned: bool) -> bool {
        let selected = self.selection.nodes.clone();
        if selected.is_empty() {
            return false;
        }
        for node in selected {
            if pinned {
                self.scene.pin(node);
            } else {
                self.scene.unpin(node);
            }
        }
        self.dirty = true;
        true
    }

    /// Apply the palette's graph colours to the core style.
    ///
    /// The core authors graph appearance in its own HSL colour vocabulary (`graph_view_core::Color`),
    /// so the adapter converts here - the one place the two colour models meet.
    pub fn apply_theme(&mut self, colors: crate::theme::GraphColors) {
        self.density_ink = colors.density;
        self.selection_ring = colors.selected_ring;
        self.categories = colors.categories;
        self.edge_categories = colors.edge_categories;
        self.ring_gap = colors.canvas;
        self.style.node_fill = core_color(colors.node);
        self.style.node_stroke_color = core_color(colors.node_stroke);
        self.style.edge_color = core_color(colors.edge);
        self.style.label_style.color = core_color(colors.label);
        self.style.node_fill_selected = core_color(colors.selected);
        self.style.node_fill_hovered = core_color(colors.hovered);
        self.style.edge_color_selected = core_color(colors.selected);
        self.style.edge_color_hovered = core_color(colors.hovered);
        let muted = core_color(colors.muted);
        self.style.node_fill_muted = muted;
        self.style.edge_color_muted = muted;
        self.dirty = true;
    }

    /// The labels to draw, already budgeted and collision-free.
    pub fn labels(&self) -> &[PlacedLabel] {
        &self.labels
    }

    /// The aggregate density image (one texel every 8 points) and its build time.
    pub fn density(&self) -> Option<(&rune::Pixels, u128)> {
        self.density
            .as_ref()
            .map(|image| (image, self.density_build_us))
    }

    /// Build the density image from this frame's cells, if it aggregated.
    fn build_density(&mut self) {
        if self.splats.is_empty() {
            if let Some(image) = &self.density {
                image.set(1, 1, vec![0, 0, 0, 0]);
                self.density = None;
            }
            return;
        }
        let started = std::time::Instant::now();
        const TEXEL: f32 = 6.0;
        let size = self.viewport.size();
        let width = ((size.x / TEXEL).ceil() as u32).max(1);
        let height = ((size.y / TEXEL).ceil() as u32).max(1);
        let mut field = vec![0.0f32; (width * height) as usize];
        // Bilinear splat: a cell's intensity lands on the four texels around its centre, so the
        // core's integer cell grid never shows through.
        for splat in &self.splats {
            let centre = (splat.min + splat.max) * 0.5 / TEXEL - Vec2::splat(0.5);
            let x0 = centre.x.floor();
            let y0 = centre.y.floor();
            let (fx, fy) = (centre.x - x0, centre.y - y0);
            let (x0, y0) = (x0 as i32, y0 as i32);
            for (dx, dy, weight) in [
                (0, 0, (1.0 - fx) * (1.0 - fy)),
                (1, 0, fx * (1.0 - fy)),
                (0, 1, (1.0 - fx) * fy),
                (1, 1, fx * fy),
            ] {
                let (x, y) = (x0 + dx, y0 + dy);
                if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                    continue;
                }
                field[(y as u32 * width + x as u32) as usize] += splat.intensity * weight;
            }
        }
        // Low-pass before the ramp: the bilinear upscale interpolates between texels but keeps a
        // step inside a sparse texel, which is what read as a block at the cloud's edge.
        let mut scratch = vec![0.0f32; field.len()];
        blur_field(&mut field, width as usize, height as usize, &mut scratch);
        let peak = field
            .iter()
            .copied()
            .fold(0.0f32, f32::max)
            .max(f32::EPSILON);
        let ink = self.density_ink;
        let encode = |c: f32| {
            let c = if c <= 0.003_130_8 {
                c * 12.92
            } else {
                1.055 * c.powf(1.0 / 2.4) - 0.055
            };
            (c * 255.0).round().clamp(0.0, 255.0) as u8
        };
        let mut rgba = Vec::with_capacity(field.len() * 4);
        for value in &field {
            // Heavy-tailed density: sqrt compresses so sparse structure shows; alpha grows faster
            // so the low end stays transparent and dense cores read solid.
            let t = (value / peak).sqrt().clamp(0.0, 1.0);
            // Drop the faintest tail: an isolated texel below the floor reads as a stray dot
            // rather than density, and it carries no structure a reader can use.
            let t = ((t - 0.03) / 0.97).max(0.0);
            let alpha = t.powf(0.7) * 0.95;
            rgba.push(encode(ink.r));
            rgba.push(encode(ink.g));
            rgba.push(encode(ink.b));
            rgba.push((alpha * 255.0).round() as u8);
        }
        let image = self.density.clone().unwrap_or_default();
        image.set(width, height, rgba);
        self.density = Some(image);
        self.density_build_us = started.elapsed().as_micros();
    }

    /// The aggregated edge cells to draw, if the frame carries them.
    pub fn splats(&self) -> &[graph_view_core::PaintEdgeSplat] {
        &self.splats
    }

    /// Centre the camera on the node with external key `key` at `zoom`: a snapshot hook for
    /// looking at a single marker (rings, selection, label) up close.
    pub fn focus_key(&mut self, key: u32, zoom: f32) {
        let Some((&id, _)) = self.node_keys.iter().find(|(_, k)| **k == key) else {
            return;
        };
        let Some(position) = self.scene.node_position(id) else {
            return;
        };
        self.viewport.focus(position);
        self.viewport.set_size(self.viewport.size());
        let factor = zoom / self.viewport.zoom().max(f32::EPSILON);
        let centre = self.viewport.size() * 0.5;
        self.viewport.zoom_at(centre, factor);
        self.dirty = true;
    }

    /// Emphasise a label as if its legend row were clicked (snapshot hook).
    pub fn emphasise(&mut self, emphasis: Emphasis) {
        self.filter_emphasis = Some(emphasis);
        self.dirty = true;
    }

    /// Select the first node and hover the first edge: a snapshot of the interaction states.
    pub fn select_for_snapshot(&mut self) {
        let Some((node, _)) = self.scene.graph().nodes().next() else {
            return;
        };
        let Some((edge, _)) = self.scene.graph().edges().next() else {
            return;
        };
        self.selection = Selection {
            nodes: vec![node].into_iter().collect(),
            edges: vec![edge].into_iter().collect(),
        };
        self.hover = Hover {
            node: Some(node),
            edge: Some(edge),
        };
        self.dirty = true;
    }

    fn labels_of(&self, key: u32) -> Vec<usize> {
        labels_for(key, self.label_tables.node.len())
    }

    fn edge_label_of(&self, key: u32) -> usize {
        edge_label_for(key, self.label_tables.edge.len())
    }

    fn node_label(&self, index: usize) -> String {
        match self.label_tables.node.get(index) {
            Some(label) => label.clone(),
            None => format!("#{index}"),
        }
    }

    fn edge_label(&self, index: usize) -> String {
        match self.label_tables.edge.get(index) {
            Some(label) => label.clone(),
            None => format!("#{index}"),
        }
    }

    /// The labels of the selected node, in registration order, for the inspector.
    pub fn selected_labels(&self) -> Vec<String> {
        self.selection
            .nodes
            .first()
            .and_then(|id| self.node_keys.get(id))
            .map(|key| {
                self.labels_of(*key)
                    .into_iter()
                    .map(|index| self.node_label(index))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The keyboard cursor node (`None` until an arrow key is pressed).
    pub fn cursor(&self) -> Option<graph_view_core::NodeId> {
        self.cursor
    }

    /// Move the keyboard cursor one step in a screen direction, starting from the node nearest the
    /// viewport centre. Returns true when the cursor moved (so the view repaints).
    pub fn move_cursor(&mut self, dx: f32, dy: f32) -> bool {
        let nodes = self.node_positions.clone();
        if nodes.is_empty() {
            return false;
        }
        let target = match self
            .cursor
            .and_then(|id| nodes.iter().find(|(node, _)| *node == id).map(|(_, p)| *p))
        {
            Some(at) => nodes
                .iter()
                .filter(|(_, p)| {
                    let (vx, vy) = (p[0] - at[0], p[1] - at[1]);
                    let along = vx * dx + vy * dy;
                    along > 0.5
                })
                .min_by(|(_, a), (_, b)| {
                    let score = |p: &[f32; 2]| {
                        let (vx, vy) = (p[0] - at[0], p[1] - at[1]);
                        let along = vx * dx + vy * dy;
                        let across = (vx * dy - vy * dx).abs();
                        along + across * 2.0
                    };
                    score(a).total_cmp(&score(b))
                })
                .map(|(id, _)| *id),
            None => {
                let centre = [self.viewport.size().x * 0.5, self.viewport.size().y * 0.5];
                nodes
                    .iter()
                    .min_by(|(_, a), (_, b)| {
                        let da = (a[0] - centre[0]).hypot(a[1] - centre[1]);
                        let db = (b[0] - centre[0]).hypot(b[1] - centre[1]);
                        da.total_cmp(&db)
                    })
                    .map(|(id, _)| *id)
            }
        };
        let Some(target) = target else {
            return false;
        };
        if self.cursor == Some(target) {
            return false;
        }
        self.cursor = Some(target);
        self.dirty = true;
        true
    }

    /// Select the keyboard cursor's node (Enter).
    pub fn select_cursor(&mut self) -> bool {
        let Some(id) = self.cursor else {
            return false;
        };
        let selection = graph_view_core::Selection {
            nodes: vec![id].into_iter().collect(),
            edges: Default::default(),
        };
        if selection == self.selection {
            return false;
        }
        self.selection = selection;
        self.dirty = true;
        true
    }

    /// Clear the keyboard cursor and the selection (Escape).
    pub fn clear_cursor_and_selection(&mut self) -> bool {
        let had = self.cursor.is_some()
            || !self.selection.nodes.is_empty()
            || !self.selection.edges.is_empty();
        self.cursor = None;
        self.selection = graph_view_core::Selection::new();
        if had {
            self.dirty = true;
        }
        had
    }

    /// The node under the keyboard cursor, for the inspector.
    pub fn cursor_label(&self) -> Option<String> {
        let id = self.cursor?;
        let key = self.node_keys.get(&id)?;
        Some(format!("N{key}"))
    }

    /// Whether this row is the sticky filter (the view draws it as pressed).
    pub fn is_filtered(&self, emphasis: Emphasis) -> bool {
        self.filter_emphasis == Some(emphasis)
    }

    /// Set the transient hover emphasis (a legend row under the pointer).
    pub fn set_hover_emphasis(&mut self, emphasis: Option<Emphasis>) {
        if self.hover_emphasis != emphasis {
            self.hover_emphasis = emphasis;
            self.dirty = true;
        }
    }

    /// Toggle the sticky filter for a row.
    pub fn toggle_filter(&mut self, emphasis: Emphasis) {
        self.filter_emphasis = if self.filter_emphasis == Some(emphasis) {
            None
        } else {
            Some(emphasis)
        };
        self.dirty = true;
    }

    /// What colours encode.
    pub fn color_mode(&self) -> ColorBy {
        self.color_by
    }

    /// Switch the categorical dimension (one at a time) and rebuild.
    pub fn set_color_by(&mut self, mode: ColorBy) {
        if self.color_by != mode {
            self.color_by = mode;
            self.dirty = true;
        }
    }

    /// The legend rows for the current mode: name, swatch colour, count.
    pub fn color_rows(&self) -> Vec<(String, Color, usize)> {
        let rows = match self.color_by {
            ColorBy::NodeLabel => (0..self.label_tables.node.len())
                .map(|index| {
                    (
                        self.node_label(index),
                        self.categories.index(index),
                        self.label_counts[index],
                    )
                })
                .collect(),
            ColorBy::EdgeLabel => (0..self.label_tables.edge.len())
                .map(|index| {
                    (
                        self.edge_label(index),
                        self.edge_categories.index(index),
                        self.edge_label_counts[index],
                    )
                })
                .collect(),
            ColorBy::None => Vec::new(),
        };
        // Busiest label first: the common types are what a reader looks for. `sort_by` is stable,
        // so equal counts keep registration order and the list never shuffles between frames.
        let mut rows = rows;
        rows.sort_by_key(|(_, _, count)| std::cmp::Reverse(*count));
        rows
    }

    /// A categorical colour by index (the legend and the shapes read the same table).
    pub fn category_color(&self, index: usize) -> Color {
        self.categories.index(index)
    }

    /// How many nodes carry this label as their primary label.
    pub fn label_count(&self, index: usize) -> usize {
        self.label_counts.get(index).copied().unwrap_or_default()
    }

    /// The count of edges with relationship type `index`.
    pub fn edge_label_count(&self, index: usize) -> usize {
        self.edge_label_counts.get(index).copied().unwrap_or(0)
    }

    /// How many nodes carry more than one label (the legend note reports it).
    pub fn multi_label_count(&self) -> usize {
        self.multi_label
    }

    /// Visible primitive counts, for the status line.
    pub fn drawn(&self) -> (usize, usize) {
        (self.nodes_drawn, self.edges_drawn)
    }

    fn build_shape(&self, frame: &graph_view_core::PaintFrame) -> Shape {
        let draw = std::env::var("GRAPH_DRAW").unwrap_or_else(|_| "both".to_owned());
        let draw_edges = draw != "nodes";
        let draw_nodes = draw != "edges";
        // Aggregate frames carry their density in the panel's image; no cell geometry is drawn.
        let mut shape = Shape::new();
        if draw_edges {
            // Ordinary edges first, then the hovered and selected ones: Rune draws in tree order,
            // and a highlighted edge has to sit above its neighbours rather than lose the race.
            // Width is the state cue (colour alone disappears on a dense graph); the frame already
            // carries both flags per edge, so this needs no core contract change.
            let normal: Vec<_> = frame
                .edges
                .iter()
                .filter(|edge| !edge.selected && !edge.hovered)
                .collect();
            let mut highlighted: Vec<_> = frame
                .edges
                .iter()
                .filter(|edge| edge.selected || edge.hovered)
                .collect();
            highlighted.sort_by_key(|edge| u8::from(!edge.selected));
            for edge in normal.into_iter().chain(highlighted) {
                let color = if self.color_by == ColorBy::EdgeLabel
                    && !edge.selected
                    && !edge.hovered
                    && edge.overlay == graph_view_core::OverlayCategory::None
                {
                    let label = self
                        .edge_keys
                        .get(&edge.id)
                        .map(|key| self.edge_label_of(*key))
                        .unwrap_or(usize::MAX);
                    self.edge_categories.index(label)
                } else {
                    edge_color(&self.style, edge.selected, edge.hovered, edge.overlay)
                };
                let width = if edge.selected {
                    self.style.edge_width * 2.2
                } else if edge.hovered {
                    self.style.edge_width * 1.7
                } else {
                    self.style.edge_width
                };
                // One analytic capsule per curve piece: Rune's segment primitive is exactly a
                // stroked capsule, so the node-indexed instance stays one primitive per piece.
                for (p0, _c, p1) in &edge.path {
                    shape = shape.line([p0.x, p0.y], [p1.x, p1.y], width, LineCap::Round, color);
                }
            }
        }
        for node in frame.nodes.iter().filter(|_| draw_nodes) {
            let label_ring = self.node_keys.get(&node.id).map(|key| self.labels_of(*key));
            let label_extras = label_ring
                .as_ref()
                .map(|labels| labels.len().saturating_sub(1))
                .unwrap_or(0);
            if node.selected {
                // The selection ring floats off whatever it encircles: its own gap, then its own
                // width, so neither the marker nor the label ring touches it. `base_outer` is the
                // outside edge of the innermost decoration (the label ring when there is one).
                let ring_gap = (node.radius * 0.22).clamp(0.8, 1.6);
                let ring_width = (node.radius * 0.5).clamp(1.0, 3.2);
                let base_outer = if label_extras > 0 {
                    node.radius + ring_gap + ring_width
                } else {
                    node.radius
                };
                let selection_gap = (node.radius * 0.3).clamp(1.2, 2.4);
                let selection_width = 2.0;
                // A filled disc of `radius`, then a canvas-coloured disc punched out of it: the
                // visible part is an annulus with `selection_gap` of bare canvas on both sides.
                let radius = base_outer + selection_gap + selection_width;
                let hole = radius - selection_width;
                let center = [node.position.x, node.position.y];
                shape = shape.line(
                    center,
                    [center[0] + 0.01, center[1]],
                    radius * 2.0,
                    LineCap::Round,
                    self.selection_ring,
                );
                shape = shape.line(
                    center,
                    [center[0] + 0.01, center[1]],
                    hole * 2.0,
                    LineCap::Round,
                    self.ring_gap,
                );
            }
            if self.cursor == Some(node.id) {
                let radius = node.radius
                    + (node.radius * 0.22).clamp(0.8, 1.6)
                    + (node.radius * 0.5).clamp(1.0, 3.2)
                    + 3.5;
                let width = 1.5;
                let center = [node.position.x, node.position.y];
                shape = shape.line(
                    center,
                    [center[0] + 0.01, center[1]],
                    radius * 2.0,
                    LineCap::Round,
                    self.selection_ring,
                );
                shape = shape.line(
                    center,
                    [center[0] + 0.01, center[1]],
                    (radius - width) * 2.0,
                    LineCap::Round,
                    self.ring_gap,
                );
            }
            let category = self
                .node_keys
                .get(&node.id)
                .map(|key| self.labels_of(*key).first().copied().unwrap_or(usize::MAX))
                .unwrap_or(usize::MAX);
            let accent = opaque(self.style.node_fill);
            let base = match self.color_by {
                ColorBy::NodeLabel => self.categories.index(category),
                ColorBy::EdgeLabel | ColorBy::None => accent,
            };
            // Selection is a tint of the element's own colour, not an inversion: inverting reads as
            // "inactive" in the light theme. The ring carries the state; the fill keeps the label.
            let color = if node.overlay != graph_view_core::OverlayCategory::None {
                node_color(&self.style, node.selected, node.hovered, node.overlay)
            } else if node.selected {
                base.mix(accent, 0.45)
            } else if node.hovered {
                base.mix(accent, 0.22)
            } else if node.overlay == graph_view_core::OverlayCategory::None {
                match self.color_by {
                    ColorBy::NodeLabel => self.categories.index(category),
                    ColorBy::EdgeLabel | ColorBy::None => opaque(self.style.node_fill),
                }
            } else {
                node_color(&self.style, node.selected, node.hovered, node.overlay)
            };
            // A node is a zero-length stroke with a round cap: one instanced capsule.
            let center = [node.position.x, node.position.y];
            // Labels beyond the first are drawn as a **ring** around the marker, split into equal
            // sectors in their own colours. The ring is emitted *before* the node body and before
            // later nodes, so the body (and any neighbour drawn afterwards) sits on top of it: the
            // ring can never cover another node.
            if let Some(key) = self.node_keys.get(&node.id) {
                let labels = self.labels_of(*key);
                let extras = labels.get(1..).unwrap_or_default();
                if !extras.is_empty() {
                    // The ring floats off the marker: width and gap both follow its radius, so a
                    // small marker gets a thin ring close in and a large one a band with room.
                    let ring_width = (node.radius * 0.5).clamp(1.0, 3.2);
                    let ring_gap = (node.radius * 0.22).clamp(0.8, 1.6);
                    let radius = node.radius + ring_gap + ring_width * 0.5;
                    // Three sectors at most: a fourth turns the darkest sliver of a small marker
                    // into a colour nobody can name. The rest stay in the inspector.
                    let shown = extras.len().min(3);
                    let overflow = extras.len() > 3;
                    let sectors = shown;
                    let step = std::f32::consts::TAU / sectors as f32;
                    // The separator will paint over `SEPARATOR` points across each boundary, so the
                    // sectors are trimmed by half of that and nothing is drawn under it. The
                    // separators then only close the gap (and hide the edge lines the ring crosses)
                    // instead of covering coloured pixels - no overdraw, and no seam artefacts
                    // where a cut and the cover disagree.
                    const SEPARATOR: f32 = 1.0;
                    let trim = ((SEPARATOR * 0.5) / radius).min(step * 0.25);
                    for (slot, extra) in extras.iter().take(shown).enumerate() {
                        let start_angle = -std::f32::consts::FRAC_PI_2 + slot as f32 * step + trim;
                        let color = if overflow && slot == shown - 1 {
                            self.categories.other
                        } else {
                            self.categories.index(*extra)
                        };
                        // Per-chord capsules with butt caps, overlapping each other by half a width
                        // at the interior joints: the flat radial cut keeps the ends from tapering,
                        // and the overlap fills the V-notch butt caps leave at a joint.
                        let half_width = ring_width * 0.5;
                        let arc_len = step - 2.0 * trim;
                        let segments = ((arc_len * radius / 0.6).ceil() as usize).clamp(4, 48);
                        let mut arc: Vec<[f32; 2]> = Vec::with_capacity(segments + 1);
                        for step_index in 0..=segments {
                            let angle = start_angle + arc_len * step_index as f32 / segments as f32;
                            arc.push([
                                center[0] + radius * angle.cos(),
                                center[1] + radius * angle.sin(),
                            ]);
                        }
                        for chord in 0..segments {
                            let (a, b) = (arc[chord], arc[chord + 1]);
                            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
                            let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
                            let (ux, uy) = (dx / len, dy / len);
                            // The overlap only has to close the wedge the abutting cuts leave, which
                            // is a fraction of a point. A half-width overlap composited two AA
                            // edges over the background and left a brighter hairline along the rim.
                            let overlap = if chord == 0 {
                                0.0
                            } else {
                                0.15f32.min(half_width)
                            };
                            let start = [a[0] - ux * overlap, a[1] - uy * overlap];
                            shape = shape.line(start, b, ring_width, LineCap::Butt, color);
                        }
                    }
                    // Separators are drawn AFTER every sector: drawing each one between two sectors
                    // let the next sector paint over it, which left only the last boundary (the one
                    // at the top) visible. They sit on the boundaries at the end of each sector.
                    let inner = node.radius + ring_gap;
                    let outer = radius + ring_width * 0.5;
                    for slot in 0..shown {
                        let boundary = -std::f32::consts::FRAC_PI_2 + (slot + 1) as f32 * step;
                        shape = shape.line(
                            [
                                center[0] + inner * boundary.cos(),
                                center[1] + inner * boundary.sin(),
                            ],
                            [
                                center[0] + outer * boundary.cos(),
                                center[1] + outer * boundary.sin(),
                            ],
                            SEPARATOR,
                            LineCap::Butt,
                            self.ring_gap,
                        );
                    }
                }
            }
            // A node is a zero-length stroke with a round cap: one instanced capsule.
            let center = [node.position.x, node.position.y];
            shape = shape.line(
                center,
                [center[0] + 0.01, center[1]],
                node.radius * 2.0,
                LineCap::Round,
                color,
            );
        }
        shape
    }

    /// Place labels in the core's priority order, dropping the ones that collide.
    ///
    /// The core decided *which* labels survived its budget and in what priority order; measuring
    /// shaped text and resolving overlap needs this host, so it happens here, in that order.
    /// Place the frame's labels with the core's shared rule (offset, collision, marker
    /// avoidance, viewport-edge clamp). The host only maps the priority onto its emphasis flag.
    fn place_labels(&self, frame: &graph_view_core::PaintFrame) -> Vec<PlacedLabel> {
        let metrics = graph_view_core::label_layout::LabelMetrics::default();
        graph_view_core::label_layout::place_labels(frame, self.viewport.size(), &metrics)
            .into_iter()
            .map(|label| PlacedLabel {
                position: [label.position.x, label.position.y],
                text: label.text,
                emphasized: matches!(
                    label.priority,
                    graph_view_core::paint::LabelPriority::Selected
                        | graph_view_core::paint::LabelPriority::Hovered
                ),
            })
            .collect()
    }
}

/// Separable blur of a scalar field, clamped at the edges.
///
/// The image is upscaled bilinearly, which interpolates between texels but cannot remove the step
/// inside one: at the fringe of a sparse cloud a single texel reads as a small block. A `[1, 4, 6,
/// 4, 1] / 16` pass each way low-passes the field first, so the upscale has no step to show.
fn blur_field(field: &mut [f32], width: usize, height: usize, scratch: &mut [f32]) {
    const KERNEL: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
    let sample = |data: &[f32], x: isize, y: isize, w: usize, h: usize| {
        let x = x.clamp(0, w as isize - 1) as usize;
        let y = y.clamp(0, h as isize - 1) as usize;
        data[y * w + x]
    };
    // Horizontal pass into the scratch buffer.
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0.0;
            for (k, weight) in KERNEL.iter().enumerate() {
                sum += weight
                    * sample(
                        field,
                        x as isize + k as isize - 2,
                        y as isize,
                        width,
                        height,
                    );
            }
            scratch[y * width + x] = sum;
        }
    }
    // Vertical pass back into the field.
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0.0;
            for (k, weight) in KERNEL.iter().enumerate() {
                sum += weight
                    * sample(
                        scratch,
                        x as isize,
                        y as isize + k as isize - 2,
                        width,
                        height,
                    );
            }
            field[y * width + x] = sum;
        }
    }
}

/// A linear-sRGB Rune colour as the core's HSL colour vocabulary.
fn core_color(color: Color) -> graph_view_core::Color {
    let encode = |c: f32| {
        if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    let (r, g, b) = (encode(color.r), encode(color.g), encode(color.b));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) * 0.5;
    let delta = max - min;
    let saturation = if delta <= f32::EPSILON {
        0.0
    } else {
        delta / (1.0 - (2.0 * lightness - 1.0).abs()).max(f32::EPSILON)
    };
    let hue = if delta <= f32::EPSILON {
        0.0
    } else {
        let sector = if max == r {
            ((g - b) / delta).rem_euclid(6.0)
        } else if max == g {
            (b - r) / delta + 2.0
        } else {
            (r - g) / delta + 4.0
        };
        sector / 6.0
    };
    graph_view_core::Color {
        h: hue,
        s: saturation.clamp(0.0, 1.0),
        l: lightness.clamp(0.0, 1.0),
        a: color.a,
    }
}

/// A core colour as Rune paints it (Rune's colour is linear sRGB, the core sets HSL).
fn opaque(color: graph_view_core::Color) -> Color {
    let [r, g, b, a] = color.to_srgba();
    let linear = |c: f32| {
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    Color {
        r: linear(r),
        g: linear(g),
        b: linear(b),
        a,
    }
}

fn node_color(
    style: &GraphStyle,
    selected: bool,
    hovered: bool,
    overlay: OverlayCategory,
) -> Color {
    let color = if selected {
        style.node_fill_selected
    } else if hovered {
        style.node_fill_hovered
    } else {
        match overlay {
            OverlayCategory::None | OverlayCategory::Accent => style.node_fill,
            OverlayCategory::Dimmed => style.node_fill_muted,
            OverlayCategory::Emphasized => style.node_fill_overlay,
        }
    };
    opaque(color)
}

fn edge_color(
    style: &GraphStyle,
    selected: bool,
    hovered: bool,
    overlay: OverlayCategory,
) -> Color {
    let color = if selected {
        style.edge_color_selected
    } else if hovered {
        style.edge_color_hovered
    } else {
        match overlay {
            OverlayCategory::None | OverlayCategory::Accent => style.edge_color,
            OverlayCategory::Dimmed => style.edge_color_muted,
            OverlayCategory::Emphasized => style.edge_color_overlay,
        }
    };
    opaque(color)
}

/// Labels drawn for the panel: absolute text at the placed screen positions.
///
/// Colours come from the theme: the label ink is the palette's body ink, and an emphasized label
/// (selected or hovered) takes the accent so it reads as the active element.
pub fn label_elements(
    labels: &[PlacedLabel],
    label_color: Color,
    emphasis_color: Color,
    canvas: Color,
) -> Vec<El> {
    // A label on a graph sits over edges, not over a flat field, so contrast against the surface is
    // not what decides legibility: the plate is. A translucent canvas-coloured plate behind the
    // text restores the local background the glyphs were contrast-checked against (the label
    // backdrop every graph renderer offers). Padding is compensated in the offset so the text keeps
    // the position the core placed.
    const PAD_X: f32 = 3.0;
    const PAD_Y: f32 = 1.0;
    let plate = canvas.alpha(0.78);
    labels
        .iter()
        .map(|label| {
            row()
                .absolute(label.position[0] - PAD_X, label.position[1] - PAD_Y)
                .center_y()
                .px(PAD_X)
                .py(PAD_Y)
                .radius(3.0)
                .fill(plate)
                .child(
                    text(label.text.clone())
                        // 500 keeps the strokes from thinning out at 11 points on a dark field;
                        // the plate supplies the contrast, the weight supplies the density.
                        .font(TextStyle::new(
                            11.0,
                            if label.emphasized { 700.0 } else { 500.0 },
                        ))
                        .color(if label.emphasized {
                            emphasis_color
                        } else {
                            label_color
                        }),
                )
        })
        .collect()
}

// A synthetic graph for the prototype: clustered nodes and mostly-local edges.

/// The labels a synthetic node carries, in registration order.
///
/// Deterministic from the node key, so a frame's colours never shift between runs or rebuilds. Every
/// seventh node carries a second label and every twenty-third a third, which is enough to exercise
/// the multi-label treatment without turning the graph into a colour soup.
pub fn labels_for(key: u32, count: usize) -> Vec<usize> {
    if count == 0 {
        return Vec::new();
    }
    // Snapshot hook: give every node extra labels so the ring treatment can be inspected at any
    // zoom without hunting for the one node in seven that carries them.
    static ALL_MULTI: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ALL_MULTI.get_or_init(|| std::env::var("CONSOLE_ALL_MULTI").is_ok()) {
        let primary = key as usize % count;
        let mut labels = vec![primary];
        for offset in [1usize, 2] {
            let next = (primary + offset) % count;
            if !labels.contains(&next) {
                labels.push(next);
            }
        }
        if key % 2 == 0 {
            let next = (primary + 3) % count;
            if !labels.contains(&next) {
                labels.push(next);
            }
        }
        if key % 3 == 0 {
            let next = (primary + 4) % count;
            if !labels.contains(&next) {
                labels.push(next);
            }
        }
        return labels;
    }
    let mut labels = vec![(key as usize) % count];
    if key % 7 == 0 {
        let second = (labels[0] + 1 + (key as usize / 7) % (count - 1)) % count;
        if second != labels[0] {
            labels.push(second);
        }
    }
    if key % 23 == 0 {
        let third = (labels[0] + 2) % count;
        if !labels.contains(&third) {
            labels.push(third);
        }
    }
    labels
}

/// The relationship type of a synthetic edge (deterministic from its key).
pub fn edge_label_for(key: u32, count: usize) -> usize {
    if count == 0 { 0 } else { key as usize % count }
}

/// A label whose members are emphasised while everything else dims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    NodeLabel(usize),
    EdgeLabel(usize),
}

/// What colours encode, as one dimension at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorBy {
    /// Vertex labels colour nodes (the default).
    #[default]
    NodeLabel,
    /// Relationship types colour edges; nodes fall back to the single accent.
    EdgeLabel,
    /// No categorical colour: the accent node fill and neutral edges.
    None,
}

/// Communities the clustered layout uses; edges stay inside one so the graph is local.
const CLUSTERS: usize = 14;

fn synthetic(nodes: usize, edges: usize) -> GraphBatch<u32, u32, (), ()> {
    let mut batch = GraphBatch::new();
    for key in 0..nodes as u32 {
        batch = batch.node(key, ());
    }
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for key in 0..edges as u32 {
        // Local edges by default: a real graph's edges join nodes near each other, so the camera
        // cull actually removes edges. `GRAPH_LONG=1` keeps the old span-forty-indices generator
        // for stress runs where almost every edge crosses the viewport.
        let a = (next() % nodes as u64) as u32;
        let b = if std::env::var("GRAPH_LONG").is_ok() {
            let step = 1 + (next() % 40) as u32;
            (a + step) % nodes as u32
        } else {
            // Same community (index % CLUSTERS picks the clustered position) and a few rings away.
            let residue = a % CLUSTERS as u32;
            let ring = a / CLUSTERS as u32;
            let step = 1 + (next() % 6) as u32;
            let candidate = (ring + step) * CLUSTERS as u32 + residue;
            if candidate < nodes as u32 {
                candidate
            } else {
                (next() % nodes as u64) as u32
            }
        };
        batch = batch.edge(key, a, b, EdgeDirection::Undirected, ());
    }
    batch
}

/// A deterministic clustered world position for node `index`: 14 communities plus scattered
/// bridges, the shape a force-directed graph of this size settles into.
fn clustered_position(index: usize, count: usize) -> Vec2 {
    let mut seed = 0x243F_6A88_85A3_08D3u64 ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut unit = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f32 / (1u64 << 53) as f32
    };
    let radius = 900.0;
    let cluster = index % CLUSTERS;
    let angle = cluster as f32 * std::f32::consts::TAU / CLUSTERS as f32;
    let center = Vec2::new(radius * 0.55 * angle.cos(), radius * 0.55 * angle.sin());
    let local = 260.0 * unit().sqrt();
    let local_angle = unit() * std::f32::consts::TAU;
    let point = center + Vec2::new(local * local_angle.cos(), local * local_angle.sin());
    if index % 10 == 0 {
        let scatter = radius * unit().sqrt();
        let scatter_angle = unit() * std::f32::consts::TAU;
        return Vec2::new(scatter * scatter_angle.cos(), scatter * scatter_angle.sin());
    }
    let _ = count;
    point
}

#[cfg(test)]
mod density_tests {
    use super::blur_field;

    fn field(width: usize, height: usize, value: fn(usize, usize) -> f32) -> Vec<f32> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| value(x, y))
            .collect()
    }

    /// An impulse stays centred (symmetry) and the pass conserves its mass: a blur redistributes
    /// density, it never invents or loses it.
    #[test]
    fn blurring_an_impulse_keeps_it_centred_and_conserves_mass() {
        let (w, h) = (21, 15);
        let mut data = field(w, h, |x, y| if (x, y) == (10, 7) { 1.0 } else { 0.0 });
        let before: f32 = data.iter().sum();
        let mut scratch = vec![0.0; data.len()];
        blur_field(&mut data, w, h, &mut scratch);
        let after: f32 = data.iter().sum();
        assert!(
            (after - before).abs() < 0.02 * before,
            "mass must be preserved: {before} -> {after}"
        );
        let at = |x: usize, y: usize| data[y * w + x];
        // Symmetric around the impulse, and strictly lower than it at its own texel.
        assert!((at(9, 7) - at(11, 7)).abs() < 1e-5);
        assert!((at(10, 6) - at(10, 8)).abs() < 1e-5);
        assert!(at(10, 7) < 1.0);
        assert!(at(10, 7) > at(9, 7), "the centre keeps the most mass");
    }

    /// The point of the pass: a one-texel step in the source becomes a ramp, so the bilinear
    /// upscale has nothing to show as a block.
    #[test]
    fn blurring_removes_a_one_texel_step() {
        let (w, h) = (11, 3);
        let mut data = field(w, h, |x, _| if x >= 5 { 1.0 } else { 0.0 });
        let mut scratch = vec![0.0; data.len()];
        blur_field(&mut data, w, h, &mut scratch);
        let row: Vec<f32> = (0..w).map(|x| data[w + x]).collect();
        let step = row
            .windows(2)
            .map(|p| (p[1] - p[0]).abs())
            .fold(0.0, f32::max);
        assert!(
            step < 0.6,
            "the hard step must become a ramp, largest neighbour jump was {step:.2}: {row:?}"
        );
        assert!(row.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
    }
}
