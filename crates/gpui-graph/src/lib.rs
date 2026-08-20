//! `gpui-graph` is a composable property graph visualization component for GPUI.
//!
//! See `DESIGN.md` for the full architecture. The crate is organized around four
//! primary separations:
//!
//! - logical graph vs visual scene,
//! - stable graph identity vs dense layout identity,
//! - shared visualization state vs individual view state,
//! - graph rendering vs application UI.

pub mod graph;
pub mod hash;
pub mod hit_test;
pub mod interaction;
pub mod keyed_graph;
pub mod layout;
pub mod paint;
pub mod patch;
pub mod runtime;
pub mod scene;
pub mod style;
pub mod view;
pub mod viewport;

pub use graph::{EdgeDirection, EdgeId, Graph, GraphDelta, NodeId};
pub use hit_test::{HitTestResult, hit_test};
pub use interaction::{GraphEvent, Hover, MouseButton, Selection};
pub use keyed_graph::KeyedGraph;
pub use layout::{
    FixedLayout, ForceAtlas2, LayoutBudget, LayoutController, LayoutEngine, LayoutGraph,
    LayoutIndex, LayoutProgress, LayoutRunState, LayoutState, LayoutSync, Placement, Rng,
    SccLayoutEngine,
};
pub use paint::{
    IndexedPaintFrameInput, PaintEdge, PaintEdgeLabel, PaintFrame, PaintFrameInput, PaintLabel,
    PaintNode, build_indexed_paint_frame, build_paint_frame,
};
pub use patch::{EdgePatch, GraphBatch, GraphPatch, NodePatch};
pub use runtime::{GraphRuntime, SyncedGraphRuntime};
pub use scene::{EdgeSceneState, GraphScene, NodeSceneState};
pub use style::{ArrowShape, GraphStyle};
pub use view::{GraphView, GraphViewState};
pub use viewport::{Viewport, WorldBounds};
