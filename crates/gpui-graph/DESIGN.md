# gpui-graph Design

Status: Partially Implemented (v0.1 core)
Scope: GPUI-native interactive property graph visualization
Primary targets: Native GPUI and GPUI Web/WASM

> **Implementation status (2026-08-20 UTC):** The v0.1 core described below is
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
- fixed/manual positioning,
- strongly-connected-component layered placement (§15.3).

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

The hash function behind the crate's `HashMap`/`HashSet`s is parameterized by
a `std::hash::BuildHasher` type parameter `S` (defaulting to SipHash
`std::collections::hash_map::RandomState`). `KeyedGraph`, `GraphScene`, and
`GraphRuntime` each carry `S`, so the same hasher backs the external-key maps,
the node index, and the derived spatial grids consistently across one scene
and its runtime. `new()` stays SipHash-only (mirroring `std`), while
`with_hasher(S)` lets callers choose a faster non-cryptographic hasher such as
`rapidhash::fast::RandomState`. Benchmarking (see `benches/paint_bench.rs`)
measures the hasher's contribution to the per-frame paint cost.

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

## 15.3 Strongly-Connected-Component circular

`SccLayoutEngine` is a deterministic layout for directed graphs whose structure
is best read through strongly connected components — the same block shape the
Frobenius normal form of a matrix produces.

It:

1. computes the strongly connected components (Tarjan),
2. contracts each component to a single node (the condensation DAG) and assigns
   each component a layer by its longest-path depth (Mirsky's theorem), so
   components at the same depth share a column,
3. places each component's nodes on a circle centered at the component's
   position, and arranges the component circles along the layers so the drawing
   reads left-to-right block by block with edges flowing from earlier to later
   columns.

Because the placement derives entirely from topology, `rebuild` produces a full
deterministic placement and `step` settles immediately (no iterations). Pinned
nodes keep their position and are not overwritten. Spacing, circle radius, and
orientation are currently fixed constants; a builder surface may be added later
if a second concrete use demands it.

The engine records each node's cluster center and radius (the center and radius
of its SCC circle) in `LayoutState::cluster_centers`. The paint layer reads this
through `PaintFrameInput::node_cluster_center` and bows every edge whose
endpoints share a cluster center outward from that center, placing the control
point at least the cluster radius from the center so a chord through the center
still bows outward rather than inward. This keeps a cluster readable as a circle
even when node spacing is large enough that the density-based bow would
otherwise leave the edge straight. Edges between clusters (different or no
centers) keep their normal density-based bow. Hit testing uses the same cluster
centers so the selectable geometry matches what is drawn.

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

The public builders keep source ownership explicit. `build_paint_frame` accepts
the standalone `PaintFrameInput` and is the linear path for callers that provide
their own graph and scene resolvers. `build_indexed_paint_frame` accepts an
`IndexedPaintFrameInput` containing the `SyncedGraphRuntime` proof returned by
`GraphScene::sync_runtime`; it resolves the graph, positions, and cluster
geometry from that same borrowed scene snapshot before using the runtime's
queries. `GraphViewState` uses the indexed builder, while tests and other
callers can use the linear builder without constructing runtime state.

## 18.3 Edge curves

Edges render as quadratic Bézier curves that bow toward the side with lower
local edge density, so edges in open space curve visibly while edges in dense
regions stay nearly straight. To keep parallel edges (multiple edges between
the same node pair) and self-loops visually distinct, the paint layer assigns a
quadratic Bézier control point:

- every non-loop edge bows toward the side with fewer neighbor edges; a lone
  edge with no neighbors is straight,
- parallel edges are fanned perpendicular to the edge direction by a spacing
  that scales with the edge's world length: a shorter edge yields a narrower
  spacing, a longer edge a wider spacing. The power is sub-linear, so the
  sagitta grows more slowly than the chord and curvature still drops as the
  node distance grows. The world-space fan is zoom-invariant (it depends only
  on world length, not on the viewport zoom); the non-clustered drawn edge uses
  the coordinate-scale correction so the spatial index, cull test, and drawn
  path agree at every zoom level,
- a self-loop renders as an onigiri (rounded triangle) path.

Self-loops count toward the local density of nearby edges (their midpoint is the
node center), so they push neighboring edges' bows away, but a self-loop's own
onigiri shape is independent of density.

Local edge density is measured in world space so the neighbor set is
zoom-invariant: each edge's midpoint is bucketed into a uniform grid, and the
signed density is the distance-weighted sum of neighbor edges within a fixed
radius, positive on the left of the edge's direction and negative on the right.
Each neighbor contributes `cos(angle) * proximity`, where `cos(angle)` is the
signed perpendicular distance normalized by the neighbor's distance and
`proximity` falls off linearly to zero at the radius. Because `cos(angle)` is
continuous through zero as a neighbor crosses the edge's axis, the bow
transitions smoothly while a node is dragged, so edges do not jitter. The
control point bows perpendicular to the edge by a fraction of the edge length
proportional to the signed density difference (clamped), so the on-screen curve
shape is stable under zoom. When the signed density is zero (no neighbors, or
balanced left/right), the bow is zero and the edge is straight.

The rendering visibility pre-filter is conservative with respect to this
runtime-resolved density: its world-space bbox evaluates both signed-density
directions at the `BOW_MAX` cap and includes the bounded obstacle displacement.
After the exact density and obstacle path are built, an edge whose endpoints are
both outside the viewport is emitted only when the axis-aligned bounds/control
hull of a segment in that actual screen-space path intersects the viewport.
This conservative test prevents false negatives but may retain a harmless
offscreen candidate for later clipping. An exactly zero-length edge between
distinct nodes is not a self-loop; only the graph identity
`edge.source == edge.target` selects the onigiri path.

The shared chord guard rejects only an exact zero or non-finite coordinate-space
length before normalization. It deliberately does not use `f32::EPSILON`: a
finite world chord smaller than that threshold can become a drawable screen
chord after a deep-zoom transform. The world bbox, runtime preprocessing, and
screen path therefore retain the same finite near-degenerate edge while still
avoiding divide-by-zero or non-finite geometry.

The scale conversion is explicit. `parallel_spacing(len, coordinate_scale)`
normalizes a length in the current coordinate space to world length and scales
the resulting spacing back into that space. The runtime/index and cull
pre-filter use world coordinates (`coordinate_scale = 1`); the non-clustered
screen-space path applies the viewport-zoom correction before trimming, and hit
testing reuses the same `edge_path` conversion. This prevents screen-length
fan drift at high zoom while keeping the indexed and linear visibility paths
consistent. Cluster routing additionally applies its cluster-space bow and
reverse-edge offset.

Every edge is stored on `PaintEdge.path` as a list of quadratic Bézier segments
already trimmed to the node boundaries, so the endpoints emerge from the node
center rather than the node edge. A non-loop edge is a single segment; a
self-loop is a list of onigiri segments. Hit testing samples each segment so
curved edges and self-loops remain selectable.

When two nodes overlap, the trimmed curve between them is degenerate (its start
parameter is not before its end) and collapses to a point. Such an edge is
skipped entirely: `edge_path` returns an empty path and `build_paint_frame`
drops it, so no zero-length segment (whose arrow would normalize to NaN) reaches
the paint layer.

A self-loop uses the node as the apex (tip) of a rounded triangle: a wide,
rounded base sits away from the node. The loop points away from the node's
other incident edges, defaulting to up when the node has no other edges.

When `GraphStyle::edge_straight_threshold` is nonzero, a non-self-loop edge
whose on-screen chord length is at or below that value skips the density,
cluster, and obstacle control-point computation entirely and renders as a
straight segment (a degenerate quadratic with its control point at the chord
midpoint), trimmed to the node boundaries (§22).

## 18.4 Label masking

Edges are cut where they pass behind a label so the label stays readable over
any background. The paint layer computes the window-space bounds of every node
and edge label, then splits each edge's Bézier path exactly at the t values
where it crosses a label rectangle (`visible_bezier_curves`). The masked region
matches the label precisely and is independent of zoom or pan. Node labels use
the same rectangle-mask technique as edge labels: a node label's bounds sit
below the node (radius + offset) and cut any edge that passes behind them.

The mask is a rounded rectangle (`RoundedRect`) whose corner radius matches the
label background padding (4px). The curve is split at the rectangle's edge
crossings, and each interval is masked only when its midpoint lies inside the
rounded rectangle — so an edge hugging a rounded corner stays visible in the
part that rounds into the corner. This keeps the mask aligned with the label's
rounded outline instead of leaving a hard square notch.

Because the edge is stroked, its ink extends half the edge width on either side
of the centerline. The rounded mask is inflated by that half-width (a rounded
rect grown by a disk), so no edge ink paints over the label. The arrowhead's
mask is inflated the same way.

The per-edge masking work runs only when it is needed: when there are no labels
and every control point of the curve lies inside the viewport, the whole curve
is visible (a quadratic Bézier stays inside the convex hull of its control
points), so `visible_edge_curves` returns the untouched single segment without
splitting. This is the common case for a zoomed-in or overview view where edges
fit inside the window, and it keeps the per-edge cost to a few comparisons
instead of four `bezier_roots` solves plus interval merging.

A directed edge's arrowhead is masked the same way. The arrow is drawn as a
single fill path that also carries the part of each overlapping label (the
label's rounded outline, clipped to the arrowhead) as an extra sub-contour; the
evenodd fill rule punches a hole exactly where the arrow passes behind a label.
The hole is the arrow clipped to the rounded label shape, not the whole rect, so
a label that merely touches the arrow's edge does not leave a gray strip beyond
the arrow. Only label rects that overlap the arrowhead are considered, so
distant labels never alter the arrow.

## 18.5 Edge label collision avoidance

Edge labels that would overlap are slid apart along their edges so they stay
readable. Each `PaintEdgeLabel` carries its edge's trimmed path and a parameter
`t` in `[0, 1]` locating the label on it. Before painting, the view resolves
overlapping label rectangles by displacing both labels apart along their paths
by an amount proportional to the overlap depth — the one with the smaller `t`
toward the start, the other toward the end — over a bounded number of passes.
Because the displacement scales with the overlap, the motion is continuous and
eases as the labels separate. Self-loop labels carry the onigiri path too, so
they slide along the loop to avoid collisions just like non-loop labels.

Node labels are fixed obstacles: edge labels slide along their paths to avoid
overlapping a node label, but node labels never move. The edge label slides in
whichever direction along its path moves it farther from the node label's
center.

When a self-loop label collides with a longer edge's label, the longer edge
slides away from the self-loop while the self-loop stays put — the longer edge
has more room to move, giving a better result. A self-loop is recognized by its
two-segment onigiri path; any other edge has a single segment.

Sliding a label along its edge to avoid another label can push it onto a node,
where it would sit over the node and look broken. Before painting, any edge
label within `edge_label_hide_distance` pixels of a node center is hidden. The
distance is measured from the label's position to the nearest node center in
canvas-local pixels, and is configurable through `GraphStyle`
(`with_edge_label_hide_distance`, default 20px).

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

`GraphRuntime` is an owned replacement cache, not a public query authority. Its
raw node/edge lookup methods and edge-preparation accessor are crate-private.
`GraphScene::sync_runtime` returns the public `SyncedGraphRuntime` proof, whose
borrowed query/preparation methods can be used only while the immutable scene
snapshot and the synchronized runtime remain paired. Callers without that proof
use the linear paint builder; a foreign or stale runtime cannot be supplied to
the indexed builder.

The spatial index is implemented as a uniform grid over node positions and
non-loop edge bounding boxes. Node positions are bucketed into the cell
containing them; each non-loop edge's conservative source-target/control-point
bounding box is inserted into every cell it covers. The bound includes both
directions of the capped density bow, the parallel/cluster geometry, and the
owner's bounded obstacle displacement. A distinct-node edge whose endpoints
exactly coincide is indexed as a point; a finite non-zero chord, including one
smaller than `f32::EPSILON` in world space, retains its full curve bound. Graph
identity, not coordinates, selects self-loop geometry. Self-loops are
deliberately not assigned a fixed
world-space bounding box: the onigiri path is defined in screen coordinates
from viewport zoom and fixed-pixel style values. Every self-loop therefore goes
into `edge_overflow`, and every bounded edge query includes it before the
precise cull evaluates the screen-space path and converts its bounds back to
world space.

The paint layer queries the index for nodes and edge candidates near the visible
region instead of scanning the whole graph every frame. The index is rebuilt
only when the scene's source identity or topology/geometry revision changes
(§31); pan/zoom reuse it. It is a coarse pre-filter: the paint layer still
runs its precise point-in-bounds and conservative curve-bound tests on returned
candidates, then computes final density and obstacle avoidance. When both
endpoints are outside, the axis-aligned bounds/control hull of each quadratic
segment in the final screen-space path is checked against the viewport before
the edge is emitted. This conservative post-candidate predicate prevents false
negatives but may retain a harmless offscreen candidate for later clipping. The
edge query expands the visible region by a fixed world-space slack for
cell-boundary and query-padding tolerance; coverage of bounded curve geometry
is provided by the conservative per-edge bbox itself.

The grid uses 64 world units per cell. A query or one edge bounding box is
enumerated only when its checked rectangle covers at most 4096 cells. A non-loop
edge whose box exceeds that bound, and every self-loop, is kept in
`edge_overflow` and included in every bounded edge query. A non-finite or
over-large query falls back to all indexed nodes or all edge-preparation
indices, respectively, so malformed or very large regions cannot cause
unbounded cell enumeration. Density-grid query ranges clamp each axis before
iteration, so saturated i32 boundary cells are visited once and cannot
double-count a neighbor in signed density.

The world-space parallel fan is zoom-invariant: its spacing depends only on the
edge's world length, not on the viewport zoom. For non-clustered paths, the
screen conversion applies the coordinate-scale correction described in §18.3,
so an edge whose control point is fanned far outside its source-target box is
not dropped by the index at high zoom. Cluster routing additionally applies its
screen-scaled cluster bow and reverse-edge offset.

The runtime also owns the zoom-invariant per-edge preprocessing (`EdgePrep`):
the candidate-edge list, parallel groups (`has_reverse`, `parallel`), edge
midpoints/normals, and the local-density grid. These depend only on the
topology and geometry revisions, so they are rebuilt once per change and reused
across many pan/zoom frames. The paint layer then does only the per-visible-edge
work each frame: it borrows the prep, queries the index for the visible
candidate indices, and runs the precise cull and geometry tests on just those
edges. Without a runtime, the paint layer falls back to building the prep and
scanning every edge in the same frame.

`GraphScene::sync_runtime` is the public and sole coherent synchronization
boundary. It creates the crate-private `RuntimeSource`, which borrows the same
`GraphScene` snapshot used to resolve graph data, node positions, cluster
centers, and topology/geometry revisions. If that source is stale by source
identity or revision, the runtime builds a complete replacement from it and
installs the replacement atomically; no individual cached node or edge can mark
a runtime current. The method then returns `SyncedGraphRuntime`, which borrows
both the immutable scene and the synchronized runtime for one paint operation.
`build_indexed_paint_frame` accepts only that proof, so it cannot combine a
foreign runtime with a scene snapshot. `GraphScene` therefore remains the
single source of truth, while `build_paint_frame` remains the explicit linear
fallback.

`Graph` carries a private source-identity token that is not shared by `Clone`.
`sync_runtime` compares that identity even when topology and geometry revisions
happen to match, rebuilding a runtime for a foreign graph source before it can
produce the indexed proof.

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

The spatial index described in §20 is the rendering visibility pre-filter. It is
not a hit-test acceleration contract. Node hit testing may be simple geometry.

Edge hit testing may require distance-to-segment or curve calculations.

 v0.1 implements hit testing as a spatial-accelerated single path. `hit_test`
 queries the §20 runtime's uniform-grid spatial index
 ([`GraphRuntime::visible_nodes`] / `visible_edge_candidates`) for a small
 candidate set around the pointer, then runs the precise geometry test only on
 those candidates, so it never scans every node and edge. It builds the same edge
 curve context as the paint layer from the scene, so the selectable geometry
 matches the drawn geometry exactly.

 Measured (hit_bench, lattice graphs at pointer-realistic zoom): the node phase
 costs a few hundred nanoseconds regardless of graph size, but the edge phase
 processed every slack-box candidate (`EDGE_INDEX_SLACK` pulls in hundreds of
 edges regardless of graph size) with full curve-context construction, costing
 a near-constant ~0.6–0.7 ms per event. The fix keeps one source of truth for
 each edge's conservative extent: `EdgePrep::curve_bboxes` stores the exact
 `edge_curve_bbox` output that builds the spatial cells, and hit testing
 rejects candidates whose box cannot come near the pointer before any curve
 work. Self-loops and oversized boxes carry an unbounded extent so they always
 reach the precise screen-space test. The screen-space hit threshold widens
 the rejection box so wide-zoom pointers still see near-miss curves. Edge and
 miss queries drop ~8x to ~80 µs per event, size-independent; node queries are
 unchanged.

 The path reuses the runtime's zoom-invariant [`EdgePrep`] — parallel groups,
 midpoints/normals, and the density grid — so the per-event density pass runs
 only for the visible candidates instead of rebuilding every edge's geometry on
 each mouse move. The maintenance cost that motivated the "selected empirically"
 note is unchanged: the spatial index is still derived from the same topology
 and geometry revisions, so its rebuild cost is amortized across pan/zoom rather
 than paid per hit test.

 During active force layout, all node positions may change continuously, making maintenance of a static spatial index potentially more expensive than a linear scan for small or medium graphs. For small or highly dynamic graphs a direct scan may be cheaper than indexing; `GraphViewState` uses the indexed path, which stays correct (it is a superset filter) while trading a small index-build overhead for large-graph hit-test speed.



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

 Viewport culling (§20) is implemented. Edge simplification is implemented as
 straight-line LOD: a non-self-loop edge whose on-screen chord length is at or
 below `GraphStyle::edge_straight_threshold` pixels (default `0.0`, disabled) is
 rendered as a straight segment instead of a density/cluster/obstacle-avoiding
 curve. Because `edge_path` computes the trimmed quadratic with a degenerate
 control point at the chord midpoint, the drawn geometry, hit testing, and label
 masking all keep working unchanged. A straight-LOD edge does not bow, so the
 paint layer also skips its signed local-density computation; only the
 curved-visible subset runs the pairwise density loop, while the density grid is
 still built over every edge's midpoint so straight neighbors continue to count
 toward a curved edge's density (§18.3, §20). Self-loops are never simplified. The
 threshold is configurable rather than fixed so callers can balance curve
 fidelity against paint cost for their graph size and zoom range; benchmark data
 in `benches/paint_bench.rs` (`paint_frame_lod`) guides the choice.

  Two micro-optimizations keep the straight-LOD path cheap, because a straight
  edge is geometrically a line even though it is represented as a degenerate
  quadratic. `straight_line_trim` trims the segment against each node boundary by
  solving the line-circle intersection analytically instead of the binary search
  used for curved edges, so the trim no longer costs 20 Bézier evaluations per
  boundary. And the per-frame obstacle grid is built only when at least one
  visible edge renders curved; in the zoomed-out overview where every edge is
  straight the grid is skipped entirely, so obstacle construction is proportional
  to the curved-visible set rather than every visible node. Together with the
  density skip these keep the paint cost proportional to the straight-LOD
  surface rather than the full per-edge curve work.

  Zooming in raises the on-screen chord length, so a straight-LOD edge crosses the
  threshold and reverts to a curve. To keep pan and zoom smooth on large graphs,
  `GraphStyle::edge_straight_threshold_while_interacting` (default `0.0`,
  disabled) elevates the straight-line threshold while the camera is moving and
  for a short settling period after it stops (`GraphStyle::edge_settle_time_ms`).
  `GraphViewState` records the last pan/zoom event time; while an interaction is
  active, `paint_style` substitutes the elevated threshold so every eligible edge
  renders as a cheap straight segment. A spawned settle task fires once
  `edge_settle_time_ms` elapses after the last event and repaints with the idle
  threshold, so detail settles back smoothly instead of popping the instant the
  camera stops. Repeated events cancel and reschedule the settle, keeping the
  low-detail threshold in effect for the whole gesture. This is the "low detail
  while moving, high detail when the camera stops" interaction LOD pattern
  recommended for visualization renderers: it protects interaction smoothness
  without permanently degrading the final view.

  Arrowheads are a second, independent LOD axis. Each directed edge's arrowhead
  is a separate painted primitive (an extra `paint_path`/`paint_quad` per frame),
  so in a zoomed-out overview the arrowheads can dominate the primitive count
  even when every edge has already been simplified to a straight segment.
  `GraphStyle::edge_arrow_min_length` (default `0.0`, disabled) omits the
  arrowhead of a directed, non-self-loop edge whose on-screen chord is at or
  below the threshold: such an edge carries no readable direction anyway, and its
  arrowhead is typically larger than the edge itself. The decision is recorded on
  `PaintEdge::omit_arrow` during frame construction (where the self-loop
  topology is known) and honored by the paint layer. Self-loops are never
  omitted, because the arrow is their only direction cue and they have no
  short-chord case. The threshold should usually be at least `edge_arrow_size` so
  an omitted arrow is actually smaller than the edges that keep theirs; benchmark
  data in `benches/paint_bench.rs` and the primitive-counting view test guide the
  choice.

  Node simplification and edge omission round out the zoomed-out LOD axis.
  `GraphStyle::node_simplify_threshold` (default `0.0`, disabled) renders a node
  whose on-screen diameter is at or below the threshold as a fill-only dot
  (no stroke), so the quad does no sub-pixel ring work at high density.
  `GraphStyle::edge_min_length` (default `0.0`, disabled) omits a non-self-loop
  edge whose on-screen chord is at or below the threshold entirely — no stroke,
  no arrow, no primitive. Because it is decided inside `edge_path`, the drawn
  geometry, hit testing, and label masking stay in agreement, and the paint layer
  already skips empty paths. Self-loops are never omitted. This is the deepest
  tier of a natural zoom-out cascade: straighten (`edge_straight_threshold`) →
  drop arrows (`edge_arrow_min_length`) → omit the edge (`edge_min_length`) →
  simplify nodes (`node_simplify_threshold`).



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

## 26.3 Query overlays

A query result is expressed as a **per-element overlay** over the persistent graph
scene, not as a replacement graph. This is what lets an application run one query
after another against the same loaded scene and stable layout without rebuilding
topology or relayout.

Each node and edge carries an [`OverlayCategory`]:
`None` (base style), `Dimmed` (subdued context), `Emphasized` (participates in the
active result), or `Accent` (the principal returned result).

Conceptually:

```text
GraphView::new(view)
    .node_overlay(category_resolver)
    .edge_overlay(category_resolver)
```

The overlay is **independent of selection and hover**. A result node the user also
selects renders as both states simultaneously: interaction colors
(`node_fill_selected`, `node_fill_hovered`, `edge_color_selected`,
`edge_color_hovered`) take precedence over the overlay color for the selected or
hovered element, and the overlay colors apply otherwise. The four dedicated
`GraphStyle` fields are `node_fill_overlay`, `node_fill_muted`,
`edge_color_overlay`, and `edge_color_muted`.

An overlay change is a style-only invalidation: it does not bump the topology or
geometry revision, does not relayout, and does not rebuild the scene. The scene
stays the authoritative persistent graph; the overlay lives in the view's paint
pipeline (`set_node_overlay` / `set_edge_overlay`).

---

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
let scene = GraphScene::new().with_layout(Box::new(ForceAtlas2::default()));
```

Typical imperative operations:

```rust
scene.update(cx, |scene, cx| {
    scene.merge(batch);
    cx.notify();
});
```

Imperative operations include:

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

`GraphScene` is the canonical owner of node positions and layout-derived
cluster centers. `bump_geometry_revision()` is the single revision-update
owner for changes to either representation. A real `set_position` change bumps
the revision; a missing node or an assignment of its existing position is a
no-op and does not churn the revision. `step_layout` bumps after an executed
layout-engine step, conservatively even when copied node positions happen to be
unchanged, because layout-derived cluster centers may have changed. It does not
bump when the controller returns early because the layout is already settled.
Public `rebuild_layout` and `set_layout` calls bump conservatively after
rebuilding because either node positions or cluster centers may have changed.
`with_layout` delegates to `set_layout`; when called on a populated scene it
preserves the canonical node positions, clears the previous engine's derived
cluster centers before rebuilding the replacement engine, bumps the geometry
revision, and reheats the layout controller. A topology-changing `merge` or
`apply` path calls `rebuild_layout`, which follows the same geometry invalidation
boundary.
`GraphScene::sync_runtime` compares its borrowed source's topology and geometry
revisions with the runtime before painting, so every public geometry mutation
path makes the derived spatial index stale before it can be reused. A
`step_layout` call that actually executes the layout engine bumps the geometry
revision conservatively, even if copied node positions are unchanged; only a
controller early return for an already-settled run avoids a bump.

Regression coverage is explicit: `scene.rs` covers a real ForceAtlas2 step and
the exact `ab` candidate after positions move,
`set_position_only_invalidates_runtime_for_a_real_change`, public layout
rebuilds, populated `with_layout` replacement, the controller-early-return
no-churn case, SCC-to-Fixed cluster center clearing, and the exact `1->9`
visible-endpoint candidate. `runtime.rs` covers same-revision foreign-source
rejection, graph-clone identity, atomic replacement, the 4096-cell boundary,
non-loop edge overflow, self-loop overflow, and huge-query fallback.
`paint.rs` covers indexed-vs-linear visible sets at overview/deep zoom,
high-zoom parallel fan visibility, the separate linear builder fallback, and
the non-unit-zoom self-loop cull coordinate path, plus the finite
near-degenerate parallel-chord bbox/path boundary at zoom `1e6`.

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

- ~~spatial index implementation~~ (implemented: uniform grid over node
  positions and non-loop edge bounding boxes, with overflow handling, §20),
- spatial index activation threshold,
- geometry-cache policy,
- ~~hit-test acceleration structure and policy~~ (implemented: hit_test
  reuses the runtime index for candidates, then pre-filters them with the
  stored per-edge curve bounding boxes — the same `edge_curve_bbox` extent
  that builds the cells, so one invariant serves both — before any curve
  construction. Measured on lattice graphs: edge/miss events ~0.6–0.7 ms →
  ~80 µs, size-independent; node events unchanged at a few hundred ns.
  Policy note from §21 stands: below roughly a thousand nodes a direct
  nearest-node scan (~1–10 µs) rivals indexed node lookups, but the edge
  phase dominates every query that misses nodes, so no scan fallback is
  warranted),
- paint-record representation.

## Layout

- ForceAtlas2 default settings,
- iteration budget,
- ~~convergence thresholds~~ (implemented: FA2 local speed adaptation —
  per-node swinging/traction balance with a carried convergence factor —
  plus two settling extensions the reference lacks because it never has to
  report `Settled`: a displacement dead-band and a global cooling schedule
  (`COOLING_FACTOR` decay per iteration, reset on rebuild) that guarantees
  termination around stiff equilibria. Pairwise forces saturate at
  `MIN_DISTANCE` in magnitude while keeping true directions, so overlapped
  nodes separate at bounded strength. Measured iterations to settle:
  hub/256 1007 → 24, grid/20x20 never (previously a >4000-iteration
  transient under spring-law gravity and a step cap of 0.1) → 593.
  Root causes of the old non-settling, fixed together: gravity magnitude
  growing with distance saturated every node onto the tiny step cap; the
  cap stretched the collapse into thousands of iterations; and the rest
  dead-band sat inside the equilibrium jitter band),
- ~~Barnes-Hut quadtree~~ (implemented in `ForceAtlas2`: flat-arena quadtree,
  FA2 degree mass, exact chains at the depth floor for coincident nodes,
  parity-tested against brute force; opt-in via
  `with_barnes_hut_threshold`, disabled by default. Measured: ~45% faster on
  a 4096-node dense ring, up to ~6x slower on sparse uniform grids, because
  the quadtree computes all-pairs long-range repulsion where the cutoff grid
  only touches local neighborhoods),
- Barnes-Hut activation policy (topology-aware threshold),
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
