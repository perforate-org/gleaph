# 0103. The console's graph panel is hosted on Rune, over a renderer-agnostic core

Date: 2026-11-07
Status: implemented (2026-11-09: `crates/graph-view-rune` is the adapter and `crates/gleaph-console`
is the host; both are workspace members and pass the gate in the integration record)
Anchor timestamp: 2026-11-07 08:45:00 UTC +0000

Related: [ADR 0076](0076-explorer-web-worker-backend.md) (the web worker backend this amends for
the console), [graph-explorer roadmap](../demo/graph-explorer-roadmap.md) §7, §17, §18,
[large-graph rendering research](../investigations/2026-11-07-large-graph-rendering-research.md).

## Context

The graph explorer roadmap plans one GPUI component serving both the browser landing demo and a
native developer console (§17, §18). The console needs a graph panel that stays interactive at
10,000 nodes / 100,000 edges while a query result table, a GQL editor and an inspector are on
screen, and whose layout keeps moving while the user pans and zooms.

GPUI's paint path bounds that:

| Fact | Source |
| --- | --- |
| The scene has eight primitive kinds (`Quad`, `Path`, `Shadow`, `Underline`, sprites, `Surface`); there is no caller-supplied vertex buffer | `gpui` pinned rev `e0931d5`, `crates/gpui/src/scene.rs` |
| `paint_path` inserts the path into the frame every time it is called; there is no cross-frame geometry cache | same, `crates/gpui/src/window.rs` |
| Quads are axis-aligned only; a rotated line must be a path (CPU-tessellated by lyon) | same, `paint_quad` / `Quad` |
| `paint_surface` is a macOS `CVPixelBuffer` video surface, not a general offscreen cache | same, `paint_surface` |

So a GPUI edge costs CPU tessellation on the frame thread every frame, and the cost grows with
visible edges. The measured native baseline is 9.9 ms of paint-frame build at only 5,000 nodes /
~7,400 edges (ADR 0076, 2026-08-24) — before tessellation.

A retained-instancing host does not pay that: the same graph measured on Rune (spike, 2026-11-07,
M2 Pro, headless 1200×800@1x, thread CPU medians, synthetic data) is 5.68 ms for 10k nodes +
100k retained edges, 2.09 ms with a 2k label budget and collision grid, and a whole console
(editor, graph, inspector, virtualized results table) at p50 2.6 ms / p95 6.3 ms with live layout
adopting one snapshot every third frame. Numbers are thread CPU because the machine was under
heavy external load; wall clock and GPU timing were not trustworthy.

## Problem

Which host draws the console's graph panel, and what does the rest of the component look like if
the host is not the one the roadmap assumed?

The console cannot be built on the current GPUI path without either accepting a graph panel that
loses its budget at the sizes it exists to show, or rewriting the paint path around GPUI's
primitives — which is a rewrite of the renderer inside a host that does not want one.

## Existing architecture assessment

The component was already two things wearing one crate name. Measured at the point of this
decision (`crates/gpui-graph`, 22.5k lines):

- 13.9k lines named no toolkit at all: the graph model, keyed identity, patches, the scene, the
  layout engine trait and its engines, the layout session, the paint frame (culling, LOD, label
  budgets), hit testing, viewport, interaction, styling, and the worker wire.
- 8.4k lines named GPUI: `view.rs` (the element), plus a handful of GPUI references in `scene.rs`
  (`Context`/`Task` for the executor driver) and `style.rs` (`Hsla`/`TextStyle`).

So the concepts the console needs — what a frame contains, what "visible" means, which label wins,
what a stale layout step is — already lived in the toolkit-free half. What was missing was a
boundary that said so, and one dependency inversion: `runtime.rs`'s sync proof was created by
`GraphScene::sync_runtime`, so the "core" depended on the scene, and the scene named GPUI.

The alternative of writing a second renderer *beside* the GPUI one without extracting the core was
rejected: it would put the LOD, budget, culling and staleness rules in two places, which is the
duplication this repository treats as a defect rather than a style choice.

## Alternatives

1. **Stay on GPUI and optimize the paint path.** Density aggregation above ~50k visible edges,
   edges as filled quads instead of stroked Béziers, tessellation moved to a worker using the
   previous viewport. Each helps; none removes the per-frame geometry build, because GPUI has no
   retained geometry and no caller-supplied vertex buffer. Cost: months of work inside a host that
   structurally cannot hold the result.
2. **Write the project's own wgpu renderer now.** Best long-term ceiling (retained instancing,
   uniforms for pan/zoom). Largest cost: a new renderer, text stack integration, input, a11y and
   IME before the console shows a single query.
3. **Adopt Rune as the console host.** Retained instanced drawing already exists (analytic
   capsule segments per edge, one draw call, `live_scale`/`live_offset` for camera), the widgets
   needed for the console exist (editable text input, virtual rows, overlays, scroll), and it
   builds for native and web. Cost: a pre-release dependency, and a second host to keep working.
4. **Keep two hosts long-term with no shared core.** Rejected: two implementations of one rule.

## Decision

**Adopt alternative 3, over an extracted core (alternative 1's precondition, done as its own
slice):**

1. **Split the component** into `graph-view-core` (no rendering toolkit) and `gpui-graph` (the
   GPUI adapter). *Implemented in this slice.* The core owns the scene, layout session, paint
   frame, LOD and label budgets, hit testing, viewport, interaction, styling, and the worker wire;
   the adapter owns the element and the GPUI-executor layout driver. `gpui-graph` re-exports the
   core modules, so `gpui_graph::paint::PaintFrame` names the same item as
   `graph_view_core::paint::PaintFrame`.
2. **The core owns appearance vocabulary**: `Color` (HSL + alpha) and `LabelStyle` (colour, font
   size in rem or px, weight). Each host converts at its boundary
   (`gpui-graph::color`); the core never names a toolkit's colour or text type.
3. **The layout driver belongs to the host.** The core hands out a `LayoutFlight` and accepts a
   finished `CompletedLayoutStep`; where it runs — GPUI's executor, a dedicated thread
   (`LayoutSource::BackgroundThread`), or a console's own loop — is the driver's business, and the
   in-flight handle is held by the driver, not the scene.
4. **The console's graph panel is hosted on Rune**, in a new adapter crate over
   `graph-view-core`. *Decided, not implemented.* Rune is pinned to a `dev` revision beside the
   GPUI pin.
5. **GPUI remains the landing demo's host** for now. Whether it stays long-term is deferred until
   the Rune adapter exists and the pre-release risk is measured; Rune targets the web too, so one
   renderer is a reachable end state, not a hope.

**Explicitly out of this decision:** the dependency-age policy question. Rune requires
dependencies inside the repository's `[registry] global-min-publish-age = "14 days"` window, so
adding it to the workspace needs one of: (A) wait for the window, (B) pin a Rune revision whose
dependencies predate it, (C) exempt the console crate. (A) is preferred, (C) is the fallback that
must be an explicit decision in the policy's own terms — this ADR does not relax the policy.

## Consequences

- The console gets a host whose cost model matches the requirement, without a renderer rewrite.
- The visualization rules exist once. A future renderer (own wgpu, or another toolkit) implements
  an adapter instead of re-deriving culling, LOD, label priority and staleness.
- `gpui-graph` remains fully supported; its tests, benches and examples keep working through the
  re-exports, and the split is invisible to callers that name `gpui_graph::…` paths.
- The core can be built with no GPUI in its dependency graph
  (`cargo tree -p graph-view-core --no-default-features` finds none), which is what makes a
  non-GPUI host possible at all.

## Trade-offs

- **Two hosts to keep working.** Mitigated by keeping all logic in the core and both adapters
  thin, and by treating "no rule in an adapter" as the review criterion.
- **A pre-release dependency.** Rune is pinned by revision; a breaking change is a coordinated
  bump of one adapter crate, not the core.
- **Text measurement stays in the host.** The core ranks and budgets labels and publishes anchors;
  collision resolution needs shaped-text metrics, so it follows host measurement in the priority
  order the core publishes. Two hosts will therefore place labels slightly differently — accepted,
  and it is why the priority contract exists.
- **Accessibility and IME are unverified on Rune.** A blocking risk for the console (not for the
  landing demo), to be checked in the adapter slice rather than assumed.
- **The 10k/100k figures are from a spike** with synthetic data and a drifting position producer,
  not from a real layout engine on a real graph. They establish the cost model, not a guarantee.

## Performance trajectory

The measured cost of this component is not "rendering is slow": it is that **per-frame work is
proportional to the number of elements on screen when it should be proportional to the camera**.
Three structural layers remove that, and only the first is in place:

| Layer | Structure | What becomes O(1) per frame | Status, evidence |
| --- | --- | --- | --- |
| **A. Retained geometry, camera in a uniform** | Geometry is built once and drawn with a per-frame transform; pan/zoom writes one uniform | Pan/zoom CPU cost | **In place on Rune** (`live_scale`/`live_offset`): 10k nodes + 100k edges = 5.68 ms, 2.09 ms with the label budget. Not available on GPUI, which re-tessellates every frame |
| **B. Topology separated from positions** | Edges carry node *indices*, not coordinates; a position buffer is the only thing a layout step rewrites | A layout adoption: `8 B × nodes` instead of rebuilding every primitive | **Unverified.** The spike rebuilds the whole shape per adoption: 1.9 ms at 30k edges, 4.8 ms at 100k. A position-only update would be ~80 KB for 10k nodes |
| **C. Aggregation and GPU compute** | Above ~50k visible edges, aggregate into a density field; run layout iterations in a compute shader | Edge count, and the layout iteration cost (1–21 ms/iter on CPU at 10k) | **Not started.** Research record §5–§6: density LOD builds 1.5M edges in 0.4 s; GPU force-directed layout is reported 40–123× faster at 20k–100k nodes |

Layer A alone already meets the console's budget at 30k visible edges (p95 6.3 ms measured with
live layout adopting every third frame). Layers B and C are what make 100k+ edges *and* live layout
comfortable, and they are the reason a custom renderer stays on the table.

**Measured through a first adapter (2026-11-07, research record §14).** A prototype Rune adapter
over `graph-view-core` (outside the workspace; `default-features = false`, so no GPUI in its tree)
confirms layer A: with nothing changing, a 10k-node / 30k-edge frame costs 1.15 ms with 40k
instances. It also finds the current bottleneck, which is **not** the renderer: the core's
`build_indexed_paint_frame` costs about **2 µs per visible edge** (9.9 ms at 5k/7.4k, 45–61 ms at
10k/30k, 130 ms at 10k/60k; layout paused, so these are frame build alone). That cost is paid by
every host and grows with the visible set, so it is the first thing to fix — ahead of renderer work
and ahead of layer C. Roughly 60% of it at 30k edges is the curved-edge path (density grid lookups,
obstacle field, Bézier control points); caching the camera-independent density on the runtime's
edge prep took a static-camera frame build at 10k/30k from 45.6–54 ms to 24.6–26.1 ms without
changing a pixel, and capping the edge spatial index's cells per edge (a 30k-edge query visited
22.6M cells-entries; the cap makes it 68k) took the curved 10k/30k build from 45.6–54 ms to
**11.6 ms** and the straight case to 7.9 ms. What remains is per-snapshot work for a moving
layout: the frame build is no longer the structural blocker for pan/zoom, only for live layout.
At 100k visible edges the per-edge pipeline is still fine (11.7 ms all-straight), but the curved bow
costs 31 ms on top; the answer there is aggregation rather than geometry work (research record
§12b), which the frame contract expresses as a splat layer that hosts draw as quads. That layer is
implemented: above `GraphStyle::edge_aggregate_threshold` the frame carries screen-space cells
instead of edges (100k edges: 42.9 ms → **3.6 ms**; 400k: 6.6 ms, bounded by a sampling cap), and
hit testing refuses to name an edge exactly when the frame draws none, and the threshold carries a
hysteresis band (enter at the configured count, leave an eighth below it) so a graph sitting at
the boundary does not rebuild its edge layer every frame.

**Trigger for building one (`crates/graph-view-wgpu`, a third adapter over `graph-view-core`).**
Start it when any of these is true, with the measurement attached:

1. With LOD and adoption interval tuned as far as the host allows, console p95 frame CPU stays
   above 16.7 ms at the target graph size.
2. The console needs live updates with more than ~50k visible edges.
3. The host cannot update positions without rebuilding its retained geometry (layer B is blocked
   by the host, not by the design), and adoption cost dominates steady frames.

Do not start it because "GPUI is slow": GPUI is already out of the console's path, and the Rune
adapter is the current answer. The triggers are numeric so the decision is not re-argued from
impressions each time.

## Migration

1. **Core extraction.** *Done*: `graph-view-core` + `gpui-graph`, colour/label bridge, layout
   driver seam, `DESIGN.md` §38.
2. **Explorer split.** Move the UI-independent session, `ResultTable` and parameter definitions
   off GPUI types so the console can use them. *Partly done*: those modules are toolkit-free; the
   crate still depends on GPUI for its example and view.
3. **Rune adapter.** A new crate converting a `PaintFrame` into Rune shapes, with host-side text
   measurement, pointer mapping onto `Viewport`/`interaction`, and hit testing against the core.
4. **Console.** `crates/gleaph-console`: query panel, graph panel, inspector, virtualized results,
   live run on a background runtime. Gated on the dependency-age decision above.
5. **Revisit the GPUI host** once the adapter is proven; delete it only if the landing demo also
   moves.

## Design Documentation Impact

- `crates/gpui-graph/DESIGN.md`: the crate split and its rules (§38); *done*.
- [ADR 0076](0076-explorer-web-worker-backend.md): its premise "the explorer is `gpui_web` and
  everything runs on the browser main thread" becomes a statement about the *landing demo*; the
  console's frame work moves to a non-GPUI host. Amendment added to that ADR.
- [graph-explorer roadmap](../demo/graph-explorer-roadmap.md) §17/§18: the console's host and the
  core/adapter split; the parameter and result-table status notes already added in this arc.

## Status

**Implemented:** the crate split, the core colour/label vocabulary and its single GPUI conversion
point, the layout driver seam (`LayoutSession`, `LayoutSource::BackgroundThread`,
`GraphScene::land_layout_flight`), the label priority budget in the core, and the Rune-side spike
that measured the numbers above.

**Decided, not implemented:** the Rune adapter, the console crate, the explorer's remaining GPUI
dependence, and GPUI's long-term fate.

**Open:** the dependency-age option (A/B/C), Rune's accessibility and IME behaviour, and whether
the landing demo also moves to Rune.
