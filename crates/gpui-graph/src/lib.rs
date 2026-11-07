//! The GPUI adapter for the renderer-agnostic graph view core.
//!
//! Everything that decides *what* a graph view shows lives in [`graph_view_core`]: the scene, the
//! layout session, the paint frame (culling, level of detail, label budgets), hit testing, the
//! viewport and interaction state. This crate adds the GPUI-specific half: the [`GraphView`]
//! element that turns a [`PaintFrame`] into GPUI primitives (quads for nodes, tessellated paths for
//! edges, shaped text for labels), and the GPUI-executor layout driver.
//!
//! The core modules are re-exported, so `gpui_graph::paint` and `gpui_graph::paint::PaintFrame`
//! name the same items as `graph_view_core::paint` and `graph_view_core::paint::PaintFrame`. A host
//! that does not draw with GPUI (a Rune console, say) depends on `graph-view-core` directly and
//! takes none of this crate.

pub use graph_view_core::{
    frame_source, graph, hash, hit_test, interaction, keyed_graph, layout, paint, patch, runtime,
    scene, style, viewport, worker,
};

pub use graph_view_core::{
    ArrowShape, Color, DefaultBuildHasher, EdgeDirection, EdgeId, EdgeOverlay, EdgePatch,
    EdgeSceneState, FixedLayout, FontSize, ForceAtlas2, FrameSource, FromWorker, Graph, GraphBatch,
    GraphDelta, GraphEvent, GraphPatch, GraphRuntime, GraphScene, GraphStyle, HitTestResult, Hover,
    IndexedPaintFrameInput, KeyedGraph, LabelStyle, LayoutBudget, LayoutController, LayoutEngine,
    LayoutGraph, LayoutIndex, LayoutProgress, LayoutRunState, LayoutState, LayoutSync, MouseButton,
    NodeId, NodeOverlay, NodePatch, NodeSceneState, OverlayCategory, PaintEdge, PaintEdgeLabel,
    PaintFrame, PaintFrameInput, PaintFrameWire, PaintLabel, PaintNode, Placement, Rng,
    SccLayoutEngine, SceneMutation, Selection, SyncedGraphRuntime, ToWorker, Viewport,
    WorkerBackend, WorkerChannel, WorkerInbox, WorldBounds, build_indexed_paint_frame,
    build_paint_frame, hsla, transform_paint_frame, white,
};

pub mod color;
pub mod view;

pub use view::{GraphView, GraphViewState};
