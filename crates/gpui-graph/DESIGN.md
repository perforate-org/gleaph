# gpui-graph Design

Status: Partially Implemented (v0.1 core)
Scope: GPUI-native interactive property graph visualization
Primary targets: Native GPUI and GPUI Web/WASM

> **Implementation status (2026-08-16 UTC):** The v0.1 core described below is
> implemented in `src/` and covered by unit tests: the logical graph model
> (`graph.rs`), keyed external identity (`keyed_graph.rs`), batches/patches and
> deltas (`patch.rs`), the shared scene (`scene.rs`), the layout architecture
> with Fixed and ForceAtlas2 engines (`layout/`), the viewport (`viewport.rs`),
> hit testing (`hit_test.rs`), the paint frame (`paint.rs`), styling
> (`style.rs`), the derived runtime (`runtime.rs`), and the GPUI view layer
> (`view.rs`). The GPUI view layer compiles but has not been exercised in a
> running GPUI application; runtime verification on native and Web/WASM is
> outstanding. Deferred items in §37 remain open.

## 1. Overview

`gpui-graph` is a high-performance, composable property graph visualization component for GPUI.

It is intended to render and interact with graph-shaped data inside ordinary GPUI applications without imposing an application shell, database model, query language, or surrounding user interface.

The same graph component should be usable in contexts such as:

- a graph database browser,
- a dashboard pane,
- an embedded graph preview,
- a website demonstration rendered through GPUI Web/WASM,
- a minimap or secondary view,
- a read-only visualization,
- or another GPUI application unrelated to Gleaph.

The primary design principle is:

> `gpui-graph` is a composable GPUI element, not an application shell.

Rendering, graph topology, visualization state, layout state, view state, and surrounding application UI must remain independently composable.

---

## 2. Motivation

Graph visualization has different requirements from conventional GPUI component rendering.

A graph may contain thousands or tens of thousands of visible primitives. Representing every node and edge as an independent GPUI element would introduce unnecessary element-tree and layout overhead.

At the same time, graph applications require more than static rendering:

- interactive pan and zoom,
- node and edge hit testing,
- selection,
- node dragging and pinning,
- incremental graph expansion,
- continuous force-directed layout,
- visibility culling,
- level-of-detail behavior,
- parallel edges and self-loops,
- and synchronization with asynchronous data sources.

`gpui-graph` therefore combines GPUI's normal composition model with low-level canvas rendering.

Externally, a graph behaves like an ordinary GPUI component.

Internally, graph primitives are rendered as a compact scene through GPUI's low-level painting APIs.

---

## 3. Goals

### 3.1 Composability

A `GraphView` must coexist naturally with ordinary GPUI and `gpui-component` content.

For example:

```rust
div()
    .flex()
    .size_full()
    .child(sidebar)
    .child(
        GraphView::new(graph_view.clone())
            .flex_1()
    )
    .child(inspector)
```

`gpui-graph` must not assume ownership of:

- the window,
- the application layout,
- toolbars,
- panels,
- inspectors,
- query editors,
- navigation,
- or application-level state.

### 3.2 Native and Web reuse

The graph model, layout architecture, interaction model, and renderer should work through GPUI on both:

- native targets,
- GPUI Web/WASM.

Platform-specific scheduling may differ, but the public graph API and visualization behavior should remain shared.

### 3.3 Property graph semantics

The graph model must support:

- directed edges,
- undirected edges,
- parallel edges,
- self-loops,
- independent node identity,
- independent edge identity.

### 3.4 Incremental exploration

The graph must support efficient incremental changes.

A graph browser should be able to load an initial subgraph and subsequently merge additional query results without rebuilding all application state.

Typical flow:

```text
query
  ↓
subgraph result
  ↓
merge
  ↓
GraphDelta
  ↓
layout reactivation
  ↓
incremental rendering
```

### 3.5 High-performance rendering

Rendering must avoid a one-element-per-node architecture for ordinary graph primitives.

The main rendering path should be:

```text
Graph + Scene
      ↓
visibility / LOD
      ↓
PaintFrame
      ↓
GPUI canvas
      ↓
edges → nodes → labels → highlights
```

### 3.6 Layout extensibility

Layout must not be tied to a single algorithm.

v0.1 should support:

- ForceAtlas2,
- fixed/manual positioning.

Future layout algorithms should be addable without changing the graph model or renderer.

---

## 4. Non-goals

The following are explicitly outside the v0.1 core:

- GQL parsing or execution,
- Gleaph-specific query logic,
- database connectivity,
- property inspectors,
- application toolbars,
- application navigation,
- persistence,
- undo/redo,
- collaborative editing,
- node-editor ports,
- user-created edge connections,
- workflow-editor semantics,
- arbitrary GPUI subtrees as graph nodes,
- a general constraint solver,
- graph database storage,
- a general graph algorithm framework.

These may exist in higher-level applications or crates.

For example:

```text
Gleaph Dashboard
├── GQL editor
├── property inspector
├── Gleaph client
├── graph exploration semantics
└── gpui-graph
```

---

# 5. High-Level Architecture

The core architecture is divided into six domains:

```text
                   Graph
                     │
                     ▼
               GraphScene
                /    |    \
               /     |     \
              ▼      ▼      ▼
        Layout   Runtime   Styling
              \      |      /
               \     |     /
                ▼    ▼    ▼
                 PaintFrame
                     │
                     ▼
                  GraphView
                     │
                     ▼
                gpui::canvas
```

View-specific interaction state is separate:

```text
GraphScene
    │
    ├───────────────┐
    ▼               ▼
GraphViewState   GraphViewState
main viewport    minimap viewport
```

This permits multiple views over the same visualization state.

---

# 6. Graph Model

## 6.1 Stable internal identity

Nodes and edges use generational identifiers.

Conceptually:

```rust
slotmap::new_key_type! {
    pub struct NodeId;
    pub struct EdgeId;
}
```

The graph may internally use structures such as:

```rust
pub struct Graph<N = (), E = ()> {
    nodes: DenseSlotMap<NodeId, Node<N>>,
    edges: DenseSlotMap<EdgeId, Edge<E>>,
}
```

`NodeId` and `EdgeId` are stable identities for the lifetime of an entity.

Deletion followed by slot reuse must not cause stale selections, layout references, or interaction state to silently reference a different entity.

---

## 6.2 Nodes

The logical node representation remains deliberately small.

Conceptually:

```rust
pub struct Node<N> {
    pub data: N,
    incident_edges: Vec<EdgeId>,
}
```

Visual information such as position does not belong to `Node`.

In particular, the following is intentionally avoided:

```rust
pub struct Node<N> {
    pub position: Vec2,
    // ...
}
```

Graph topology and visualization state are separate concerns.

---

## 6.3 Edges

Conceptually:

```rust
pub struct Edge<E> {
    pub source: NodeId,
    pub target: NodeId,
    pub direction: EdgeDirection,
    pub data: E,
}

pub enum EdgeDirection {
    Directed,
    Undirected,
}
```

Edges have independent identity.

The tuple:

```text
(source, target)
```

must never be treated as edge identity.

This is required for parallel edges and property graph semantics.

---

## 6.4 Supported topology

The core graph explicitly supports:

```text
A ─────▶ B       directed

A ────── B       undirected

A ──x──▶ B
A ──y──▶ B       parallel edges

A ─────▶ A       directed self-loop

A ────── A       undirected self-loop
```

Rendering policy for these cases may evolve independently from the logical representation.

---

## 6.5 Adjacency

v0.1 favors a simple adjacency representation.

Each node maintains incident edge identifiers:

```rust
incident_edges: Vec<EdgeId>
```

Incoming and outgoing relationships can be derived from the associated edge records.

Deletion may initially perform an `O(degree)` removal from incident edge lists.

This should remain simple until profiling demonstrates that a more complex adjacency structure is justified.

The graph being visualized is normally a client-side working subgraph rather than the complete database graph.

---

# 7. External Identity

`gpui-graph` internal identity must remain independent from application or database identity.

For example:

```text
Gleaph VertexRef
       │
       ▼
external key map
       │
       ▼
gpui_graph::NodeId
```

A source-facing graph layer may therefore be provided:

```rust
pub struct KeyedGraph<NK, EK, N = (), E = ()> {
    graph: Graph<N, E>,
    node_keys: HashMap<NK, NodeId>,
    edge_keys: HashMap<EK, EdgeId>,
}
```

The exact container types remain implementation details.

`KeyedGraph` owns the graph and both external-key maps as one consistency
boundary. Its graph accessor is read-only; there is intentionally no raw
mutable graph escape. All topology mutations must flow through `merge` or
`apply`, so node and edge removal can clean the corresponding key maps and
edge upserts can preserve stable internal identity while updating graph
adjacency.

This enables applications to use identities such as:

- database primary keys,
- UUIDs,
- Gleaph vertex references,
- domain identifiers,

without imposing them on layout and rendering internals.

---

# 8. Graph Updates

Three concepts are distinguished.

## 8.1 GraphBatch

A `GraphBatch` represents graph data that should be merged into the currently known graph.

Typical source:

```text
database query result
```

A node already present in the graph is reused rather than duplicated.

Example:

```text
existing

Alice ─── Bob

incoming batch

Alice ─── Carol

result

Bob ─── Alice ─── Carol
```

---

## 8.2 GraphPatch

A `GraphPatch` represents explicit mutations such as:

- upsert node,
- remove node,
- upsert edge,
- remove edge.

Conceptually:

```rust
pub enum NodePatch<K, N> {
    Upsert {
        key: K,
        data: N,
    },
    Remove {
        key: K,
    },
}
```

and similarly for edges.

An existing edge key is an insert-or-replace operation. The existing `EdgeId`
is retained while source, target, direction, and data are replaced together.
`Graph` validates both endpoint identities before mutation and owns the
adjacency update, including the rule that a self-loop appears once. An unknown
endpoint rejects the edge mutation without changing the graph, key maps, or
scene revisions.

---

## 8.3 GraphDelta

After a batch or patch is applied, internal systems receive a delta expressed using internal identities.

Conceptually:

```rust
pub struct GraphDelta {
    pub added_nodes: Vec<NodeId>,
    pub updated_nodes: Vec<NodeId>,
    pub removed_nodes: Vec<NodeId>,

    pub added_edges: Vec<EdgeId>,
    pub updated_edges: Vec<EdgeId>,
    pub removed_edges: Vec<EdgeId>,
    pub topology_changed: bool,
}
```

`topology_changed` is set for additions, removals, and existing-edge updates
that change endpoints or direction. A data-only edge update remains in
`updated_edges` but leaves this marker clear, allowing `GraphScene` to advance
the data revision without rebuilding or reheating the layout.

The architecture therefore becomes:

```text
external world

GraphBatch / GraphPatch
          │
          ▼
        Graph
          │
          ▼
      GraphDelta

internal visualization world
```

`GraphDelta` drives:

- layout invalidation,
- geometry invalidation,
- spatial index updates,
- scene changes.

---

# 9. Graph Scene

`GraphScene` represents a visualization of a graph.

It owns visualization state that is shared between views.

Conceptually:

```text
GraphScene
├── graph
├── node scene state
├── edge scene state
├── layout session
├── layout configuration
├── geometry state
└── revisions
```

It does not own a particular viewport.

This makes it possible to render the same scene through multiple views.

---

# 10. Scene State

Node visualization state is stored separately from logical graph data.

Conceptually:

```rust
pub struct NodeSceneState {
    pub position: Vec2,
    pub pinned: bool,
}
```

A structure such as:

```rust
SecondaryMap<NodeId, NodeSceneState>
```

is suitable because scene state is keyed by stable graph identity.

Possible future scene attributes include:

- visibility,
- opacity,
- layout group,
- temporary emphasis,
- animation state.

These should not be added until required.

---

# 11. Layout Architecture

## 11.1 Layout as a session

Layout is not modeled as:

```rust
fn layout(graph: &Graph) -> Positions;
```

Instead, layout is a long-lived session.

This is essential for interactive graph exploration:

```text
initial graph
    ↓
layout
    ↓
settle
    ↓
expand node
    ↓
new nodes
    ↓
reheat
    ↓
settle again
```

Existing positions should normally survive graph expansion.

---

## 11.2 LayoutGraph

The logical graph is projected into a dense representation optimized for numerical layout algorithms.

```text
Graph
  │
  ▼
LayoutGraph
```

Conceptually:

```rust
pub struct LayoutGraph {
    pub nodes: Vec<LayoutNode>,
    pub edges: Vec<LayoutEdge>,

    pub node_ids: Vec<NodeId>,
    pub topology_revision: u64,
}
```

A dense index is used internally:

```rust
#[repr(transparent)]
pub struct LayoutIndex(u32);
```

This allows layout hot loops to use:

```rust
positions[index.0 as usize]
```

instead of hash lookups or generational-key lookups.

The design deliberately separates:

> stable identity for graph state

from:

> dense indexing for numerical computation.

---

## 11.3 Layout topology semantics

`LayoutGraph` preserves enough topology information for different algorithms to make different decisions.

For example:

- ForceAtlas2 may treat directed edges as undirected attraction relationships.
- A hierarchical layout may use edge direction.
- Parallel edges may be retained or aggregated.
- Self-loops may be ignored by a force layout.

These are layout projection policies rather than graph semantics.

---

## 11.4 LayoutState

Shared layout state contains algorithm-independent state.

Conceptually:

```rust
pub struct LayoutState {
    pub positions: Vec<Vec2>,
    pub pinned: BitVec,
}
```

Algorithm-specific state does not belong here.

Examples of algorithm-specific state:

- velocity,
- temperature,
- Barnes-Hut tree,
- attraction statistics,
- convergence state,
- ForceAtlas2 speed state.

These remain inside the layout engine.

---

## 11.5 LayoutEngine

All layouts use a unified stateful interface rather than separate static and dynamic traits.

Conceptually:

```rust
pub trait LayoutEngine {
    fn rebuild(
        &mut self,
        graph: &LayoutGraph,
        state: &mut LayoutState,
    );

    fn apply_delta(
        &mut self,
        graph: &LayoutGraph,
        delta: &LayoutDelta,
        state: &mut LayoutState,
    ) -> LayoutSync;

    fn step(
        &mut self,
        graph: &LayoutGraph,
        state: &mut LayoutState,
        budget: LayoutBudget,
    ) -> LayoutProgress;
}
```

Exact method signatures may evolve during implementation.

The architectural contract is more important than the specific initial Rust API.

---

## 11.6 LayoutSync

Incremental topology updates are optional for an engine.

An engine may report:

```rust
pub enum LayoutSync {
    Applied,
    RebuildRequired,
}
```

`RebuildRequired` is a valid default.

A rebuild must not imply discarding existing positions.

Instead:

```text
old nodes
    → preserve existing positions

new nodes
    → assign initial positions

layout engine
    → rebuild internal algorithm state

simulation
    → continue
```

---

## 11.7 LayoutProgress

Layout execution is incremental.

Conceptually:

```rust
pub enum LayoutProgress {
    Running {
        stability: Option<f32>,
    },
    Settled,
}
```

Algorithm-specific convergence information must not leak into the general API.

ForceAtlas-specific metrics, for example, may be converted into a general stability measure or exposed only through diagnostics.

---

## 11.8 LayoutBudget

Layout execution is controlled through a budget.

A simple v0.1 form may be:

```rust
pub struct LayoutBudget {
    pub max_iterations: u32,
}
```

Iteration-oriented budgeting is preferred over making physical elapsed time part of layout semantics.

The scheduling layer may still impose a wall-clock frame budget.

---

# 12. Layout Controller

The layout algorithm decides how to compute positions.

It does not decide when it should run.

That responsibility belongs to a layout controller.

```text
Graph changed ──────┐
                    │
GPUI frame ─────────┼──▶ LayoutController ──▶ LayoutEngine
                    │
user interaction ───┘
```

A minimal state machine may be:

```rust
pub enum LayoutRunState {
    Running,
    Settled,
    Paused,
}
```

Typical transitions:

```text
Settled
   │
   │ topology changed
   ▼
Running
   │
   │ convergence
   ▼
Settled
```

User interaction may also reheat or pause a simulation.

---

# 13. Initial Placement

Initial placement is distinct from layout.

This matters particularly when graph exploration introduces new nodes.

Randomly redistributing the entire graph whenever a node is expanded creates poor interaction stability.

For example:

```text
before expansion

A ─── B ─── C


after expanding B

      D
      │
A ─── B ─── C
      │
      E
```

`D` and `E` should initially appear near `B`.

Possible placement policies include:

```text
Random
Around(origin)
Barycenter
Fixed(position)
```

The public abstraction should remain small in v0.1.

Applications may provide an origin hint when merging graph data produced by an expansion operation.

---

# 14. Pinning

Hard node pinning is part of the common layout model.

Typical interaction:

```text
drag node
   ↓
set position
   ↓
pin node
   ↓
other nodes continue layout
```

v0.1 does not require a general graph constraint system.

Hard pinning is sufficient.

---

# 15. v0.1 Layout Engines

v0.1 provides two layout modes.

## 15.1 ForceAtlas2

The primary dynamic graph layout.

The public API should wrap rather than expose the API of the underlying ForceAtlas2 implementation.

Preferred:

```rust
ForceAtlas2::default()
```

rather than:

```rust
forceatlas2::Settings
```

as part of the `gpui-graph` public surface.

This keeps the layout implementation replaceable.

---

## 15.2 Fixed / Manual

A fixed layout performs no automatic topology-driven repositioning.

It is useful for:

- externally positioned graphs,
- manually arranged graphs,
- saved layouts,
- tests,
- deterministic demonstrations.

---

# 16. View State

`GraphViewState` represents the state of a particular view into a graph scene.

Conceptually:

```text
GraphViewState
├── viewport
├── hover state
├── interaction state
├── focus state
└── temporary view state
```

It references a `GraphScene`.

Two view states may reference the same scene.

The default viewport carries a private one-time initial-fit state. It remains
pending through zero-sized and one-axis-zero canvas layouts, and is consumed
only by the first layout whose width and height are both positive. Any explicit
viewport or framing operation made before that layout—including mutable
viewport access, pan, zoom, `fit_all`, or `focus_node`—cancels the pending fit;
the explicit operation therefore remains authoritative.

Example:

```text
GraphScene
    │
    ├──────────────▶ Main GraphViewState
    │
    └──────────────▶ Minimap GraphViewState
```

---

# 17. Viewport

The renderer operates in graph world coordinates.

Canvas-local pixels are introduced only through the viewport transformation.

The viewport's screen coordinates are canvas-local pixels, not window-space
coordinates. `GraphView` is the sole owner of the GPUI boundary conversion: it
subtracts the current canvas bounds origin from window-space input before
calling viewport or hit-test operations, and applies that origin to paint
geometry only when sending it to GPUI. The origin is per rendered element and
does not belong in `GraphViewState` or `Viewport`.

```text
world coordinates
       │
       ▼
    Viewport
       │
       ▼
screen / canvas-local pixels
```

The viewport is responsible for:

- world-to-screen conversion,
- screen-to-world conversion,
- pan,
- zoom,
- visible world bounds,
- fit graph,
- fit selection,
- focus node.

Layout algorithms must not depend on `gpui::Pixels`.

---

# 18. Rendering Architecture

## 18.1 Canvas-based rendering

Ordinary graph primitives are not represented as individual GPUI elements.

The render pipeline is:

```text
GraphScene
    │
    ▼
Scene projection
    │
    ▼
visibility / LOD
    │
    ▼
PaintFrame
    │
    ▼
gpui::canvas
```

Within the canvas, rendering occurs in logical layers such as:

```text
edges
  ↓
nodes
  ↓
labels
  ↓
selection / hover highlights
```

The exact order may be configurable later.

---

## 18.2 PaintFrame

`PaintFrame` is an intermediate frame representation containing only the information required for the current paint.

Node positions and edge endpoints in a `PaintFrame` are canvas-local pixels.
`paint.rs` never knows about GPUI bounds or window coordinates; `GraphView`
adds the current canvas origin exactly once at the paint boundary.

Conceptually:

```text
PaintFrame
├── visible nodes
├── visible edges
├── visible labels
├── interaction highlights
└── overlay anchors
```

This separates graph and scene state from rendering mechanics.

The frame may contain already transformed geometry or compact records optimized for painting.

## 18.3 Edge curves

Edges render as straight lines by default. To keep parallel edges (multiple
edges between the same node pair) and self-loops visually distinct, the paint
layer assigns a quadratic Bézier control point:

- a single edge between two distinct nodes has no control point (straight line),
- parallel edges are fanned perpendicular to the edge direction,
- a self-loop renders as an onigiri (rounded triangle) path.

Every edge is stored on `PaintEdge.path` as a list of quadratic Bézier segments
already trimmed to the node boundaries, so the endpoints emerge from the node
center rather than the node edge. A non-loop edge is a single segment; a
self-loop is a list of onigiri segments. Hit testing samples each segment so
curved edges and self-loops remain selectable.

A self-loop uses the node as the apex (tip) of a rounded triangle: a wide,
rounded base sits away from the node. The loop points away from the node's
other incident edges, defaulting to up when the node has no other edges.

---

# 19. Rich Overlays

Not all UI should be painted as graph primitives.

Sparse rich UI may remain ordinary GPUI elements.

Examples:

- tooltip,
- context menu,
- selected-node popover,
- floating controls,
- loading indicator.

Conceptually:

```text
Graph canvas
    nodes / edges / labels

GPUI overlay layer
    tooltip / context menu / popup
```

Applications should also be free to place their own overlays around a `GraphView`.

For example:

```rust
div()
    .relative()
    .child(
        GraphView::new(view.clone())
            .size_full()
    )
    .child(
        div()
            .absolute()
            .top_4()
            .right_4()
            .child(toolbar)
    )
```

`gpui-graph` must not require a built-in toolbar system.

---

# 20. Runtime

Rendering caches and spatial acceleration live outside the logical graph.

Conceptually:

```text
GraphRuntime
├── spatial lookup
├── visible-record queries
├── edge geometry cache
├── hit-test acceleration
├── label geometry cache
└── rendering caches
```

The runtime is derived state.

It must be reconstructible from authoritative graph and scene state.

---

# 21. Hit Testing

Hit testing uses two conceptual stages:

```text
coarse candidate lookup
          │
          ▼
precise geometry test
          │
          ▼
actual hit
```

Node hit testing may be simple geometry.

Edge hit testing may require distance-to-segment or curve calculations.

The exact spatial index is deliberately not fixed in the v0.1 design.

During active force layout, all node positions may change continuously, making maintenance of a static spatial index potentially more expensive than a linear scan for small or medium graphs.

The implementation should therefore be selected empirically.

Possible policy:

```text
small or highly dynamic graph
    → direct scan

large settled graph
    → spatial acceleration
```

---

# 22. Visibility and Level of Detail

The renderer should not assume that every loaded graph primitive must be painted at every zoom level.

The runtime should eventually support:

- viewport culling,
- label suppression,
- node simplification,
- edge simplification,
- aggregation,
- zoom-dependent detail.

v0.1 requires viewport culling infrastructure but does not need an elaborate LOD policy.

Thresholds must be benchmark-driven rather than fixed prematurely.

---

# 23. Interaction

v0.1 supports:

- hover,
- node selection,
- edge selection,
- pan,
- zoom,
- node drag,
- pin,
- unpin.

Graph-database-specific actions such as:

```text
Expand neighbors
```

do not belong in `gpui-graph`.

Instead, the component emits general interaction events.

---

# 24. Events

Graph interaction should integrate with GPUI's state and event model rather than being defined exclusively through a large set of builder callbacks.

Conceptually:

```rust
pub enum GraphEvent {
    NodeClicked {
        node: NodeId,
        button: MouseButton,
    },

    NodeDoubleClicked {
        node: NodeId,
    },

    EdgeClicked {
        edge: EdgeId,
        button: MouseButton,
    },

    SelectionChanged {
        nodes: Vec<NodeId>,
        edges: Vec<EdgeId>,
    },

    NodeMoved {
        node: NodeId,
        position: Vec2,
    },

    ViewportChanged,
}
```

A higher-level application can interpret these events.

For example:

```text
gpui-graph

NodeDoubleClicked(NodeId)

          ↓

Gleaph Dashboard

query neighbors
          ↓
GraphBatch
          ↓
merge
```

Another application may assign completely different semantics to the same interaction.

---

# 25. Controlled and Shared State

Selection, viewport, layout, and scene information should not be buried irreversibly inside a private renderer object.

Higher-level applications may need to coordinate graph state with:

- a property inspector,
- a query editor,
- a table,
- a minimap,
- another graph view,
- navigation history.

For example:

```text
                   Selection
                  /    |    \
                 /     |     \
                ▼      ▼      ▼
          GraphView  Table  Inspector
```

v0.1 does not need a complex React-style controlled/uncontrolled API.

It does need clean state boundaries that make such composition possible.

---

# 26. Styling

Styling is divided into two layers.

## 26.1 GPUI element styling

The graph view itself participates in normal GPUI layout and styling.

For example:

```rust
GraphView::new(view.clone())
    .w_full()
    .h(px(600.))
    .border_1()
    .rounded_lg()
```

This includes concerns such as:

- width,
- height,
- flex behavior,
- margin,
- background,
- border,
- clipping.

---

## 26.2 Graph styling

Graph-specific appearance belongs to graph styling.

Examples:

- node radius,
- node fill,
- node stroke,
- edge width,
- edge pattern,
- arrows,
- label appearance,
- selected state,
- hovered state.

Conceptually:

```rust
GraphView::new(view)
    .node_style(...)
    .edge_style(...)
    .label_style(...)
```

The exact style API should remain small in v0.1 and evolve from real use cases.

Directed edges render an arrowhead at the target end. The arrowhead is
configurable through `GraphStyle`:

- `edge_arrow_enabled` — whether directed edges draw an arrowhead,
- `edge_arrow_size` — arrowhead length along the edge in pixels,
- `edge_arrow_shape` — one of `Triangle`, `Line`, or `Circle`.

Undirected edges never draw an arrowhead regardless of these settings.

Graph styling reuses GPUI types. Colors are `gpui::Hsla` and label text uses
`gpui::TextStyle`, so graph appearance shares a single color and font vocabulary
with the rest of the application. The node stroke is split into
`node_stroke_width` and `node_stroke_color`.

Node labels are resolved by a callback set on the view state
(`set_node_label`) and rendered centered below the node. Edge labels are
resolved by `set_edge_label` and rendered centered at the edge midpoint. When
the node and edge data types implement `Display`, `new_with_default_labels`
shows both automatically. Label appearance is configured through `label_style`
(`gpui::TextStyle`) and `label_offset`.

---

# 27. Public API Shape

The public API should expose three distinct conceptual layers.

## 27.1 Graph

Logical topology and data.

```rust
let graph = Graph::new();
```

or a keyed equivalent.

---

## 27.2 GraphScene

Long-lived visualization state.

Conceptually:

```rust
let scene = cx.new(|cx| {
    GraphScene::new(graph)
        .layout(ForceAtlas2::default())
});
```

Typical imperative operations:

```rust
scene.update(cx, |scene, cx| {
    scene.merge(batch);
    cx.notify();
});
```

Possible future operations:

```rust
scene.set_layout(...);
scene.pin(...);
scene.unpin(...);
scene.apply(...);
```

---

## 27.3 GraphViewState

A particular view into a scene.

Conceptually:

```rust
let view = cx.new(|cx| {
    GraphViewState::new(scene.clone(), cx)
});
```

View-specific operations may include:

```rust
view.update(cx, |view, cx| {
    view.fit_all(cx);
    view.focus_node(node, cx);
});
```

These operations are explicit framing choices and take precedence over the
private one-time default initial fit. The mutable viewport accessor has the
same precedence when callers configure pan or zoom directly.

---

## 27.4 GraphView

A lightweight composable GPUI component.

The canvas element records its current bounds origin in element-local render
state. Mouse move/down/scroll positions are translated from window space to
canvas-local space before entering `GraphViewState`; canvas-local
`PaintFrame` geometry is translated back to window space exactly once during
painting. This keeps canvas origin handling out of both `Viewport` and
`PaintFrame`.

Conceptually:

```rust
GraphView::new(view.clone())
    .size_full()
```

The API should make a graph feel like an ordinary GPUI child.

---

# 28. Composition Example

A higher-level graph browser may look like:

```rust
div()
    .flex()
    .size_full()
    .child(
        div()
            .w(px(320.))
            .child(query_editor)
    )
    .child(
        GraphView::new(graph_view.clone())
            .flex_1()
    )
    .child(
        div()
            .w(px(280.))
            .child(property_inspector)
    )
```

`gpui-graph` has no knowledge of the query editor or property inspector.

---

# 29. Website / WASM Example

The same component may be embedded into a website rendered using GPUI Web/WASM:

```text
Website section
     │
     ├── heading
     ├── explanatory copy
     │
     └── GraphView
             │
             ▼
         GPUI Web
             │
             ▼
            WASM
```

The graph renderer does not require a separate web-specific graph model.

---

# 30. Scheduling

Layout scheduling is deliberately separated from layout computation.

Native and Web/WASM may use different execution strategies.

## Native

Possible future strategy:

```text
background task
     ↓
layout progression
     ↓
scene update
     ↓
GPUI repaint
```

## Web/WASM

Possible strategy:

```text
frame
  ↓
small layout budget
  ↓
paint
  ↓
next frame
```

The `LayoutEngine` API must not require background threading.

v0.1 should favor a cooperative stepping model that works everywhere.

---

# 31. Revisions and Invalidation

Different classes of changes must be distinguishable.

At minimum, the implementation should track concepts equivalent to:

```text
topology_revision
data_revision
geometry_revision
style_revision
```

Examples:

### Force layout iteration

```text
topology unchanged
data unchanged
geometry changed
```

### Node property update

```text
topology unchanged
data changed
geometry maybe unchanged
```

### Existing edge upsert

An endpoint or direction replacement changes both topology and data while
retaining the same `EdgeId`; the scene rebuilds its dense layout projection and
reheats the layout. Replacing only edge data changes the data revision and
preserves the existing projection and layout run state.

### Edge insertion

```text
topology changed
geometry changed
```

### Theme change

```text
style changed
```

This allows expensive derived structures to be invalidated selectively.

---

# 32. Performance Strategy

The v0.1 architecture should optimize for good data flow while avoiding premature micro-optimization.

The core performance principles are:

1. Stable generational graph identity.
2. Dense layout projections for numerical algorithms.
3. Batched low-level canvas rendering.
4. Viewport culling.
5. Derived runtime caches.
6. Incremental graph mutation.
7. Revision-based invalidation.
8. Cooperative layout stepping.
9. Avoiding one GPUI element per graph primitive.
10. Benchmark-driven spatial indexing and LOD.

---

# 33. Benchmarking

Performance decisions should be validated against progressively larger visible subgraphs.

Suggested initial strata:

```text
500 nodes
5,000 nodes
20,000 nodes
```

Additional edge-density strata should also be tested.

Benchmarks should distinguish:

- topology insertion,
- batch merge,
- layout iteration,
- paint-frame construction,
- edge geometry generation,
- viewport culling,
- hit testing,
- canvas painting.

Native and Web/WASM performance should both be measured.

A design change should not be justified solely by theoretical asymptotics when the working graph sizes and renderer behavior do not demonstrate a practical benefit.

---

# 34. v0.1 Module Structure

The initial implementation should remain a single crate unless real reuse pressure justifies splitting it.

Suggested structure:

```text
gpui-graph/
└── src/
    ├── lib.rs
    │
    ├── graph.rs
    ├── keyed_graph.rs
    ├── patch.rs
    ├── scene.rs
    ├── viewport.rs
    ├── interaction.rs
    ├── runtime.rs
    ├── hit_test.rs
    ├── paint.rs
    ├── style.rs
    ├── view.rs
    │
    └── layout/
        ├── mod.rs
        ├── graph.rs
        ├── controller.rs
        ├── placement.rs
        ├── fixed.rs
        └── force_atlas2.rs
```

This structure is provisional.

The architecture should not be distorted to preserve this exact module layout.

---

# 35. Dependency Direction

`gpui-graph` depends on GPUI.

It should not depend on `gpui-component`.

```text
gpui-component ─────┐
                    │
                    ▼
              application UI
                    │
                    ▼
               gpui-graph
                    │
                    ▼
                   GPUI
```

More precisely, both `gpui-component` and `gpui-graph` should compose naturally because they share GPUI as their underlying UI framework.

This allows applications to place `GraphView` inside:

- panels,
- docks,
- resizable layouts,
- tabs,
- cards,
- application-specific containers.

---

# 36. Design Influence

The implementation should study existing GPUI canvas and graph projects for architectural lessons without inheriting their entire abstractions.

In particular:

### open-gpui-canvas

Useful ideas include:

- separating document/model state from runtime state,
- visible-record extraction,
- paint-frame construction,
- coarse and precise hit testing,
- batching rendering instead of one element per object,
- separating painted scene primitives from sparse widget overlays.

It should be treated as an architectural reference rather than a direct dependency because it targets the separate Open GPUI fork rather than official GPUI.

### FerrumFlow

Useful ideas include:

- viewport isolation,
- world/screen coordinate transforms,
- layout strategy separation,
- explicit interaction state.

Its workflow-editor graph model and port-based semantics are not appropriate as the core model for `gpui-graph`.

---

# 37. Deferred Work

The following should remain intentionally open until implementation and profiling provide evidence.

## Rendering

- Bézier routing,
- label collision avoidance,
- text LOD.

## Runtime

- spatial index implementation,
- spatial index activation threshold,
- geometry-cache policy,
- paint-record representation.

## Layout

- ForceAtlas2 default settings,
- convergence thresholds,
- iteration budget,
- Barnes-Hut thresholds,
- dynamic incremental engine updates,
- hierarchical layout,
- Sugiyama layout,
- radial layout,
- community-aware layout.

## Scheduling

- native background layout,
- worker-based Web layout,
- synchronization frequency,
- frame-time budgeting.

## Advanced graph presentation

- community aggregation,
- collapsed groups,
- edge bundling,
- graph clustering,
- semantic zoom,
- millions-of-elements visualization.

These should not block v0.1.

---

# 38. v0.1 Required Features

A v0.1 implementation is considered architecturally complete when it supports:

- stable `NodeId`,
- stable `EdgeId`,
- directed edges,
- undirected edges,
- parallel edges,
- self-loops,
- keyed external identity mapping,
- graph batch merging,
- graph deltas,
- shared `GraphScene`,
- independent `GraphViewState`,
- world-space coordinates,
- pan,
- zoom,
- fit graph,
- node and edge rendering,
- node and edge hit testing,
- hover,
- selection,
- node dragging,
- pinning,
- fixed/manual layout,
- ForceAtlas2 layout,
- incremental layout stepping,
- node expansion-friendly initial placement,
- viewport culling,
- canvas-based batched rendering,
- GPUI-native composition,
- Native GPUI operation,
- GPUI Web/WASM operation.

---

# 39. Architectural Invariants

The following invariants should be preserved during implementation.

### Invariant 1

Logical graph topology must not depend on visual position.

### Invariant 2

External application identity must not become the renderer's internal identity model.

### Invariant 3

Graph entity identity must remain stable across unrelated insertions and deletions.

### Invariant 4

Layout hot loops should operate over dense data.

### Invariant 5

A layout algorithm must not depend on GPUI coordinates or GPUI rendering types.

### Invariant 6

A graph primitive does not require a corresponding GPUI element.

### Invariant 7

A `GraphView` must not own surrounding application UI.

### Invariant 8

Graph exploration semantics belong to the application, not the renderer.

### Invariant 9

A graph scene may be observed through multiple independent views.

### Invariant 10

Native and Web/WASM targets must share the same core visualization architecture.

---

# 40. Summary

`gpui-graph` is designed around four primary separations:

```text
logical graph
    ≠
visual scene

stable graph identity
    ≠
dense layout identity

shared visualization state
    ≠
individual view state

graph rendering
    ≠
application UI
```

The resulting architecture is:

```text
External graph source
        │
        ▼
GraphBatch / GraphPatch
        │
        ▼
Graph / KeyedGraph
        │
        ▼
GraphDelta
        │
        ▼
GraphScene
 ┌──────┼──────────┐
 ▼      ▼          ▼
Layout Runtime   Styling
 │       │          │
 └───────┼──────────┘
         ▼
     PaintFrame
         │
         ▼
   GraphViewState
         │
         ▼
      GraphView
         │
         ▼
    GPUI Canvas
```

This structure keeps the core small enough for v0.1 while leaving clear extension points for larger graphs, additional layouts, richer styling, advanced LOD, native and browser scheduling, and graph-browser-specific behavior.

The central architectural principle remains:

> `gpui-graph` should provide the graph visualization primitive; applications should decide what the graph means and how it participates in the surrounding product.
