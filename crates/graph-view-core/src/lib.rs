//! Renderer-agnostic graph visualization core.
//!
//! This crate owns everything about a graph view that does not depend on how it is drawn: the
//! logical-to-visual graph projection, the layout session, the paint frame (culling, level of
//! detail, label budgets), hit testing, viewport and interaction state, and the wire form used to
//! hand a frame to another thread or process.
//!
//! It exists so a host can render a graph without adopting a particular UI toolkit: `gpui-graph`
//! is the GPUI adapter, a Rune console is another. The split is what keeps the invariants (which
//! primitives exist, what "visible" means, which label wins a collision, what a stale layout step
//! is) in one place instead of one per toolkit.
//!
//! ```text
//! Graph + GraphBatch ──▶ GraphScene (positions, layout session, revisions)
//!                              │ sync_runtime
//!                              ▼
//!                     build_paint_frame  ──▶ PaintFrame ──▶ host draws it
//!                              │
//!                        hit_test / Viewport / interaction
//! ```

pub mod color;
pub mod frame_source;
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
pub mod viewport;
pub mod worker;

pub use color::{Color, FontSize, LabelStyle, hsla, white};
pub use frame_source::{FrameSource, PaintFrameWire, transform_paint_frame};
pub use graph::{EdgeDirection, EdgeId, Graph, GraphDelta, NodeId};
pub use hash::DefaultBuildHasher;
pub use hit_test::{HitTestResult, hit_test, hit_test_frame};
pub use interaction::{GraphEvent, Hover, MouseButton, Selection};
pub use keyed_graph::KeyedGraph;
pub use layout::{
    FixedLayout, ForceAtlas2, LayoutBudget, LayoutController, LayoutEngine, LayoutGraph,
    LayoutIndex, LayoutProgress, LayoutRunState, LayoutState, LayoutSync, Placement, Rng,
    SccLayoutEngine,
};
pub use paint::{
    EdgeOverlay, IndexedPaintFrameInput, NodeOverlay, OverlayCategory, PaintEdge, PaintEdgeLabel,
    PaintFrame, PaintFrameInput, PaintLabel, PaintNode, build_indexed_paint_frame,
    build_paint_frame,
};
pub use patch::{EdgePatch, GraphBatch, GraphPatch, NodePatch};
pub use runtime::{GraphRuntime, SyncedGraphRuntime};
pub use scene::{EdgeSceneState, GraphScene, NodeSceneState};
pub use style::{ArrowShape, GraphStyle};
pub use viewport::{Viewport, WorldBounds};
pub use worker::{FromWorker, SceneMutation, ToWorker, WorkerBackend, WorkerChannel, WorkerInbox};
