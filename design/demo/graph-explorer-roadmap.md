# Graph Explorer Roadmap

Anchor timestamp: 2026-08-20 19:52:59 UTC +0000

## Status

**Planned — Phase 0 (prerequisite validation) and Phase 1 (core vertical slice)** are the active
delivery contract tracked by `plans/0264-graph-explorer-phase0-1.md`. Phases 2–10 are explicitly
planned and out of scope until Phase 0–1 validate the architecture. This document describes intended
behavior; sections beyond Phase 1 reflect planned behavior, not implemented behavior.

The first implementation target is deliberately narrow: a read-only explorer that loads a complete
demo graph, executes a small set of real prepared queries through the Gleaph Rust SDK, and visualizes
the results by highlighting relevant graph elements without replacing the underlying scene.

This document intentionally avoids depending on undocumented or implementation-specific details of
Gleaph, the Gleaph Rust SDK, or `gpui-graph`. Those systems should be integrated through narrow
adapters based on their actual public APIs.

The main architectural goals are:

- reuse the same graph exploration state and presentation logic across landing and console surfaces;
- keep `gpui-graph` independent of Gleaph-specific concepts;
- keep networking and protocol concerns inside the Gleaph Rust SDK;
- treat query results as presentation overlays over a persistent graph scene rather than as replacement graphs;
- preserve a clear path from the initial web demo to a richer web/native developer console;
- avoid prematurely implementing console-only features.

---

# 2. Product Model

The Graph Explorer should be understood as an application feature layered above a general-purpose
graph renderer.

```text
Gleaph Rust SDK                gpui-graph
       │                           │
       │                           │
       └────────────┬──────────────┘
                    │
             Graph Explorer
                    │
            ┌───────┴────────┐
            │                │
      Landing Demo       Developer Console
```

The responsibilities are intentionally separated.

### Gleaph Rust SDK

The SDK owns communication with Gleaph.

It is expected to provide suitable abstractions for operations such as:

- connecting to a Gleaph deployment;
- selecting or identifying a graph;
- retrieving graph data required by the explorer;
- executing prepared queries;
- passing query parameters;
- decoding query responses;
- handling identities, transports, retries, and protocol errors.

The Graph Explorer must not duplicate these responsibilities.

### `gpui-graph`

`gpui-graph` owns general graph visualization and interaction primitives.

Expected capabilities include, where supported by its public API:

- graph data presentation;
- node and edge rendering;
- layout;
- viewport transformations;
- pan and zoom;
- hit testing;
- hover and selection;
- styling;
- rendering invalidation.

Gleaph-specific concepts such as prepared queries, result bindings, logical graph identities, or
query semantics must not be introduced into `gpui-graph`.

### Graph Explorer

The Graph Explorer bridges the two systems.

It owns:

- graph exploration session state;
- conversion from Gleaph graph data into a form consumable by `gpui-graph`;
- identity mapping between Gleaph entities and rendered graph entities;
- prepared-query presets;
- query execution orchestration;
- query-result presentation;
- highlighting and dimming policies;
- explorer-specific controls;
- shared panels used by the landing page and console.

---

# 3. Recommended Package Structure

The initial implementation should avoid excessive crate fragmentation.

A reasonable starting point is:

```text
crates/
├── gpui-graph/
├── gleaph-sdk/
├── gleaph-graph-explorer/
└── gleaph-console/

apps/
└── landing/
```

Only `gleaph-graph-explorer` needs to be introduced specifically for this effort.

Internally it can remain modular:

```text
gleaph-graph-explorer/
└── src/
    ├── lib.rs
    ├── session.rs
    ├── graph.rs
    ├── mapping.rs
    ├── query.rs
    ├── presentation.rs
    ├── explorer.rs
    ├── graph_panel.rs
    ├── query_controls.rs
    ├── result_summary.rs
    └── inspector.rs
```

Splitting this into separate `core` and `ui` crates should only be considered once there is a
concrete dependency or compilation reason to do so.

---

# 4. Core Architectural Principle: Persistent Scene, Transient Overlays

The defining interaction model is:

1. load and display the full demo graph;
2. establish a stable layout;
3. execute prepared queries against the actual demo graph;
4. preserve the graph and its node positions;
5. express query results as temporary visual overlays;
6. replace or animate the overlay when another query is selected.

The graph itself should therefore not be reconstructed for each query.

Conceptually:

```text
Persistent Graph Scene
        │
        ├── topology
        ├── graph metadata
        └── stable layout
              │
              ▼
      Query Presentation Overlay
              │
              ├── dim context
              ├── emphasize matches
              ├── emphasize results
              └── emphasize properties
```

A query transition should normally be:

```text
Query A overlay
      ↓
query starts
      ↓
existing graph remains visible
      ↓
Query B response arrives
      ↓
Query B overlay replaces/transitions from A
```

It should not normally be:

```text
Query A
  ↓
destroy graph
  ↓
loading spinner
  ↓
rebuild graph for Query B
```

This persistent-scene behavior is a requirement for the initial demo.

---

# 5. Graph Explorer Session

The central reusable abstraction should be an explorer session rather than a monolithic widget.

A conceptual model is:

```rust
pub struct GraphExplorerSession {
    // SDK/client handle or equivalent access path

    // Current graph and mapping state

    // Query preset / parameters / execution state

    // Current result presentation

    // Selection and interaction state
}
```

The exact ownership model should follow idiomatic GPUI patterns and the actual SDK API.

The important requirement is that multiple UI components must be able to observe and act upon the
same session.

For example:

```text
                  GraphExplorerSession
                    /      |       \
                   /       |        \
          QueryControls GraphPanel InspectorPanel
```

This is what allows the landing experience and console experience to share behavior without sharing
the same layout.

---

# 6. Graph Identity Mapping

The explorer must maintain an explicit boundary between Gleaph entity identities and `gpui-graph`
identities.

Conceptually:

```rust
struct GraphIdentityMap {
    // Gleaph vertex identity -> rendered node identity
    // Gleaph edge identity   -> rendered edge identity
}
```

The actual key types must use whatever stable public identities are exposed by Gleaph and
`gpui-graph`.

This mapping is required for:

- converting query results into highlights;
- selecting entities programmatically;
- connecting inspector state to rendered entities;
- avoiding any requirement for `gpui-graph` to understand Gleaph-specific identity semantics.

The mapping must remain valid for the lifetime of the loaded graph scene.

If the graph topology is replaced wholesale, the mapping may be rebuilt.

Overlay-only changes must not require mapping reconstruction.

---

# 7. Query Preset Model

The landing demo should not initially expose an arbitrary query editor.

Instead, it should expose a small curated set of prepared-query presets.

A conceptual model:

```rust
pub struct QueryPreset {
    pub id: QueryPresetId,
    pub title: SharedString,
    pub description: SharedString,
    pub prepared_query: PreparedQueryReference,
    pub parameters: Vec<QueryParameterDefinition>,
    pub presentation: QueryPresentationDefinition,
}
```

The concrete representation of a prepared query should come from, or cleanly adapt to, the Rust SDK.

A preset should describe:

- a user-facing title;
- a concise explanation;
- which prepared query to execute;
- any parameters the user may modify;
- presentation metadata required to interpret the result visually.

The query itself remains a Gleaph concern.

The preset is an explorer concern.

---

# 8. Query Presentation Semantics

The explorer should not make `gpui-graph` understand query semantics.

Instead:

```text
Gleaph Query Response
        ↓
Query Result Adapter
        ↓
Explorer Presentation
        ↓
gpui-graph styling / overlay primitives
```

A minimal semantic model for the initial demo is:

```text
Context
Matched
Result
Property
```

Where:

- **Context** is the graph that remains visible but is unrelated to the active result.
- **Matched** refers to graph entities participating in a relevant matched structure or returned path.
- **Result** refers to entities that represent the principal returned result.
- **Property** refers to a property whose value is relevant to the query result or presentation.

The exact ability to distinguish matched versus returned entities depends on what information the
SDK/query response exposes.

If the response does not contain enough information, the initial implementation may collapse the
model to:

```text
Context
Result
Property
```

The explorer should not fabricate execution provenance.

---

# 9. Do Not Expose Physical Execution Traces as Query Semantics

The visual demo is intended to explain logical graph relationships, not storage-engine behavior.

If future Gleaph APIs provide execution tracing, the Graph Explorer should distinguish between:

- logical match provenance;
- physical execution/profile information.

For visualization over the graph, logical provenance is preferred.

Examples of appropriate logical presentation information include:

- returned vertices;
- returned edges;
- returned paths;
- bindings corresponding to query variables;
- relevant property values.

Storage reads, internal index probes, execution order, or planner-specific intermediate operations
should belong to a future profiling/debugging surface instead.

---

# 10. Overlay Requirements for `gpui-graph`

The initial Graph Explorer depends on some form of style overlay or equivalent capability in
`gpui-graph`.

The exact API should follow existing `gpui-graph` architecture, but the required semantics are:

### Element-specific emphasis

The explorer must be able to alter presentation of:

- individual nodes;
- individual edges;
- where applicable, individual property displays.

### Default dimming

The explorer must be able to dim all unrelated graph elements without enumerating every unrelated
element manually.

Conceptually:

```text
overlay default:
    nodes -> muted
    edges -> muted

overrides:
    matched nodes -> emphasized
    result nodes  -> primary
    matched edges -> emphasized
```

### Separation from selection

Query emphasis must not reuse the same state as user selection.

These must remain independently composable:

```text
Base Graph
    ↓
Query Overlay
    ↓
Selection
    ↓
Hover
```

A result node that the user also selects must be representable as both states simultaneously.

### Style-only invalidation

Changing an overlay should not unnecessarily:

- rebuild graph topology;
- rerun graph layout;
- reconstruct the entire scene.

At minimum, the implementation should make a deliberate distinction between topology/layout changes
and style changes.

---

# 11. Stable Layout Requirement

Node positions must remain stable across query execution.

For the initial demo:

- the complete graph is loaded;
- a layout is produced once;
- query result changes only modify presentation state.

If the user selects another prepared query, the graph should not visually reorganize unless the
topology itself has changed.

This stability is critical because the user must be able to understand:

> "The same graph is being viewed through another query."

rather than:

> "A new graph appeared."

The first version does not require sophisticated incremental layout.

It only requires that overlay updates do not trigger unnecessary relayout.

---

# 12. Interaction Requirements

The MVP should be read-only.

Required interactions:

- pan;
- zoom;
- hover;
- click/select;
- fit entire graph;
- reset or return to the default viewport.

Desirable but not mandatory for the first release:

- focus results;
- focus selected node;
- subtle hover tooltips.

Not required for the MVP:

- graph editing;
- node creation;
- edge creation;
- drag-to-connect;
- destructive operations.

---

# 13. Visual Transition Requirements

The initial release does not require full graph animation.

It should, however, preferably support lightweight transitions when query presentation changes.

Useful transitions include:

- opacity interpolation;
- node emphasis interpolation;
- edge-width interpolation;
- label emphasis transitions.

Topology animation and physics-based transitions are explicitly out of scope for the first demo.

If adding animation complicates the underlying API significantly, the first version may switch
overlays immediately. Correct architecture is more important than animation.

---

# 14. Property Presentation

The explorer should be able to associate properties with graph elements, even if full property
rendering is initially delegated to surrounding UI.

For example:

```text
Alice
────────────
country    Japan
age        28
score      0.91
```

A query may conceptually emphasize `country` or `score`.

For the first demo, property highlighting may be limited to an inspector/summary, selected-node
details, or labels attached to a result node.

A full always-visible property table inside every rendered node is not required.

---

# 15. Loading and Query Execution Behavior

The graph should stay visible while a query executes.

Recommended lifecycle:

```text
Idle
  ↓ user runs query

Executing
  ├ existing graph remains visible
  ├ existing overlay may remain visible
  └ query control indicates pending state

Success
  ├ new presentation overlay becomes active
  └ summary metadata updates

Failure
  ├ existing graph remains visible
  ├ previous valid presentation may remain
  └ concise error state is shown
```

A failed query should not destroy the graph scene.

The landing demo should feel resilient rather than transactional.

---

# 16. Landing Demo Composition

The landing page should use a compact, opinionated composition.

```text
┌───────────────────────────────────────────┐
│ [ Query preset ▼ ]                 Run   │
│ Short explanation                         │
├───────────────────────────────────────────┤
│                                           │
│             Full graph                    │
│                                           │
│      context + highlighted result         │
│                                           │
├───────────────────────────────────────────┤
│ result count · execution latency          │
└───────────────────────────────────────────┘
```

The landing page does not require Dock.

It should use either:

- a complete `GraphExplorer` assembled component, or
- a small fixed composition of shared explorer controls and `GraphPanel`.

The landing host should remain thin.

---

# 17. Console Composition

The console should reuse the same session and graph presentation logic, but compose the interface
differently.

```text
┌──────────────┬─────────────────────┬───────────────┐
│ Query Panel  │                     │ Inspector     │
│              │     Graph Panel     │               │
│              │                     │               │
├──────────────┴─────────────────────┴───────────────┤
│ Results / Raw / Profile / Metrics                  │
└────────────────────────────────────────────────────┘
```

The console therefore should not depend on a single indivisible `GraphExplorer` widget.

Instead, it should be able to instantiate multiple panels backed by one session:

```text
GraphExplorerSession
├ QueryPanel
├ GraphPanel
├ InspectorPanel
└ ResultPanel
```

This is a key reuse requirement.

---

# 18. Web and Native Targets

The shared explorer should avoid browser-specific assumptions.

Expected targets:

```text
Landing
    Astro host
        ↓
    GPUI WASM
        ↓
    Graph Explorer

Web Console
    browser host
        ↓
    GPUI WASM
        ↓
    Graph Explorer

Native Console
    GPUI native
        ↓
    Graph Explorer
```

Networking remains the responsibility of the Rust SDK.

The first delivery target is Web/WASM. Native packaging is a later milestone.

---

# 19. Phase 0 — Validate Prerequisites

Before building substantial explorer code, validate that the existing libraries expose sufficient
primitives.

## `gpui-graph`

Confirm or implement the minimum equivalent of:

- persistent graph model/view;
- stable node and edge identities;
- stable layout storage;
- style customization;
- read-only interaction;
- pan/zoom;
- hover;
- selection;
- fit graph;
- efficient repaint after visual-style changes.

Most importantly, determine whether an overlay-like mechanism already exists. Do not introduce a
competing styling architecture if the existing library already has an appropriate abstraction.

## Gleaph Rust SDK

Confirm that the SDK can support:

- loading the graph data required by the demo;
- executing prepared queries;
- passing parameters;
- obtaining stable graph identities in responses;
- compiling for the intended Web/WASM target.

> **Measured 2026-08-21** against the live `social` graph (7,244 vertices / 47,230 edges,
> Router 4caro-hl777-77775-aaaba-cai) via SDK `gql_query`, to size the whole-graph load path and
> to decide whether a router-side compressed-download API is required. The decision is **no**: the
> full topology loads through multiple ordinary SDK queries, so no router change is needed for
> Phase 0-1.
>
> - `MATCH (n) RETURN element_id(n)`: 7,244 rows / ~101 KB — fits in one call.
> - `MATCH ()-[e]->() RETURN element_id(e)`: 47,230 rows / ~850 KB — fits in one call.
> - `MATCH (a)-[e]->(b) RETURN element_id(e), element_id(a), element_id(b)` over all edges at once:
>   rejected — exceeds the 2 MiB safe payload limit (2097152 B).
> - The same query scoped to the single largest edge label (`IN_HOME_FEED`, 26,600): ~1.22 MB — fits.
> - `label(n)`/`labels(n)` and `type(e)` are **supported** by the runtime execution layer.
>   Measured nuance (2026-08-21): label **names** resolve only when the query carries a label
>   constraint — `MATCH (n:User) RETURN labels(n)` → `["User"]`, `label(n)` → `"User"`;
>   an unconstrained `MATCH (n) RETURN labels(n)` falls back to the raw label id (`Uint64`).
>   For whole-graph loading this is non-blocking because labels are scoped per `MATCH (n:Label)`
>   anyway. See the measured load plan below.
> - Whole-graph load plan: one query returning all vertex `element_id`s, one query per vertex label
>   (`MATCH (n:Label)`) to recover label names, and one query per edge label returning edge `id`,
>   `src`, `dst`. Each stays under the 2 MiB limit.

### Exit criteria

Phase 0 is complete when a minimal application can:

1. obtain graph data using the SDK;
2. render it through `gpui-graph`;
3. execute one prepared query;
4. correlate at least one returned graph entity with one rendered graph entity.

---

# 20. Phase 1 — Graph Explorer Core

Implement the non-polished vertical slice.

Deliver:

- `GraphExplorerSession` — graph loading state, mapping, active preset, parameters, execution status,
  current presentation, selection.
- Graph conversion — adapter from SDK graph data to the `gpui-graph` representation, with no
  Gleaph-specific types leaking into `gpui-graph` and no UI styling during data conversion.
- Query execution — choose a preset, provide parameters, execute through the Rust SDK, cancellation
  or stale-response handling where needed, error propagation.
- Query presentation — convert a successful result into a visual presentation model. Initial
  semantics: `context`, `result`. Add `matched` and property semantics when sufficiently supported.

### Exit criteria

A hardcoded demo application can load the full graph, execute at least one real prepared query, and
highlight the returned entities without rebuilding the graph.

---

# 21. Phase 2 — Query Overlay and Stable Scene

Formalize query-result visualization.

Requirements:

- all graph context can be dimmed;
- result nodes can be emphasized;
- result edges can be emphasized;
- overlay is independent from selection;
- overlay changes do not trigger topology rebuilds;
- overlay changes do not trigger unnecessary relayout.

### Exit criteria

Switching between multiple query results preserves all node positions and visibly changes only the
presentation emphasis.

---

# 22. Phase 3 — Demo Query Presets

Create the initial curated demo experience (3–5 prepared queries) demonstrating meaningfully
different capabilities. Each preset contains a title, one-sentence explanation, prepared-query
reference, optional parameters, and a result-presentation definition.

### Exit criteria

A non-developer can switch among presets and understand that different queries illuminate different
portions of the same graph.

---

# 23. Phase 4 — Landing UI

Build the polished compact explorer with required controls, required graph interaction, and required
query behavior. Explicitly do not add arbitrary GQL editor, Dock, full inspector, raw response, query
history, authentication UI, graph mutation, DuckDB, or telemetry.

---

# 24. Phase 5 — Landing Integration

Embed the MVP into the landing site behind a thin host. Avoid linking console-specific
functionality into the landing bundle.

---

# 25. Phase 6 — Explorer Decomposition for Console Reuse

Formalize the explorer UI into composable pieces shared across surfaces.

---

# 26. Phase 7 — Developer Console Integration

Introduce the explorer into the developer console using GPUI Component Dock or equivalent.

---

# 27. Phase 8 — Arbitrary Query Workflow

After prepared-query exploration is robust, introduce an arbitrary query editor sharing result
presentation infrastructure.

---

# 28. Phase 9 — Observability and Local Analytics

Add console-specific operational features separately from the graph-exploration core. This phase
must not create a dependency from `gleaph-graph-explorer` to DuckDB.

---

# 29. Phase 10 — Native Console

Package the console as a native GPUI application sharing the Rust SDK, Graph Explorer, console
model, most GPUI UI, and `gpui-graph`.

---
# 30. Non-Goals for the Initial Demo

The initial work should explicitly resist scope expansion.

The following are not MVP requirements:

- graph mutation;
- schema editing;
- query authoring IDE;
- full query profiler;
- physical execution tracing;
- user accounts;
- access-control management;
- metrics history;
- DuckDB;
- migration management;
- multi-window desktop support;
- user-configurable Dock layouts;
- graph virtualization for extremely large datasets;
- minimap;
- GPU compute layout;
- multiple layout engines;
- collaboration;
- offline graph editing.

---
# 31. Implementation Order

Recommended execution order:

```text
M0  Validate gpui-graph and SDK prerequisites
M1  Load real graph and render it
M2  Execute one real prepared query
M3  Map response identities back to rendered entities
M4  Implement query overlay/dimming
M5  Preserve layout across query changes
M6  Add curated prepared-query presets
M7  Add compact GPUI demo controls
M8  Produce Web/WASM build
M9  Embed in landing
```

Only then proceed with M10–M15 (console decomposition, Dock integration, rich inspector surfaces,
arbitrary GQL, observability/DuckDB, native packaging).

---
# 32. Suggested MVP Acceptance Test

1. Open the demo page.
2. The Graph Explorer initializes.
3. The complete demo graph appears.
4. Choose preset A.
5. A prepared query executes through the Rust SDK.
6. The existing graph stays visible during execution.
7. Result nodes and edges become emphasized.
8. Unrelated graph context becomes subdued.
9. Choose preset B.
10. Node positions remain unchanged.
11. The visual emphasis moves to a different region.
12. Select a highlighted node.
13. Selection remains visually distinguishable from query emphasis.
14. Reset the viewport.
15. Trigger a query error.
16. The graph remains visible and interactive.

---
# 33. Key Design Invariants

- The Rust SDK is the sole Gleaph communication layer.
- `gpui-graph` remains Gleaph-agnostic.
- The complete graph is the persistent scene.
- Query emphasis and selection are separate states.
- Layout is stable across overlay changes.
- Landing and console share behavior, not composition.
- The landing binary remains small.
- The MVP uses real Gleaph execution; mocks are for tests only.

---
# 34. Definition of "Demo Scope"

Work up to and including the first integration consists only of:

```text
Rust SDK integration
+ full demo graph loading
+ gpui-graph rendering
+ stable layout
+ prepared-query presets
+ real prepared-query execution
+ result-to-graph identity mapping
+ query-result highlighting
+ context dimming
+ basic read-only interaction
+ small result summary
+ Web/WASM packaging
+ demo embedding
```

Everything else should be treated as post-demo work.
