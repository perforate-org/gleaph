# Large-graph rendering: what the field does, and what Gleaph should do

Status: **Record** — a dated survey feeding the console/graph-renderer decision. It changes no
contract; when a renderer is chosen, that decision belongs in an ADR.

Anchor timestamp: 2026-11-07 03:32:14 UTC +0000
Repository anchor: `270e8845`
Measured on: Apple M2 Pro (macOS 27.2), Rust 1.101.0-nightly; machine load 8–15 during runs
(other builds), which makes **wall/GPU times unreliable** and thread-CPU times the usable column.

## 1. Why this record exists

The console needs a graph panel that shows the demo graphs (7,244 vertices / 47,230 edges today)
and has headroom for 10k nodes / 100k edges, with pan/zoom, hover picking, labels, and a frame
budget that leaves room for chrome. The current GPUI path (`crates/gpui-graph`) is measured and
structurally capped ([ADR 0076](../adr/0076-explorer-web-worker-backend.md)): it rebuilds a
screen-space paint frame every frame, tessellates edge paths with lyon, and has no custom GPU
pass. A Rune spike (2026-11-06/07) showed an instanced path can do 100k edges well inside budget.
This record asks: **what do the best existing implementations do, and which of their techniques
matter at our size?**

## 2. Method and its limits

- Six research branches were fanned out (academic literature; web graph libraries; native C++
  graph applications; GPU primitive techniques; force-layout acceleration; LOD/aggregation).
- **Three branches failed before reporting** (native applications, layout acceleration,
  LOD/aggregation). Their ground is covered here from (a) the branches that did report, (b) the
  first-hand source inspection done earlier in the same investigation (Graphia and Tulip trees,
  `re_renderer` line/picking modules), and (c) the primary sources cited below. Where a claim
  rests only on a paper abstract or vendor text, it is marked.
- Claim marking: **[M]** measured/reported in the cited work · **[C]** claimed by authors/vendor,
  not independently reproduced · **[O]** our own measurement in this repository.

## 3. Our own measurements

### 3.1 Current GPUI path [O]

[ADR 0076](../adr/0076-explorer-web-worker-backend.md) at 5,000 nodes / ~7,400 edges
(12,300 primitives): paint-frame build **9.9 ms** after parallelising the edge map, cull and
density phases; ForceAtlas2 step (8 iterations) **8.2 ms**. The build is screen-space, per frame;
edges are CPU-tessellated into paths; each label is its own z-order layer. At 110k primitives this
path is not in the same class.

### 3.2 Rune spike (instanced segments, one canvas, 1200×800@1x) [O]

| Scene | Instances | thread CPU p50 |
|---|---:|---:|
| nodes only, 10k | 10,001 | 0.78 ms |
| 10k nodes + 30k edges | 40,001 | 2.35 ms |
| **10k nodes + 100k edges (retained camera)** | **110,001** | **5.68 ms** |
| + 2,000 labels | 118,891 | 5.60 ms |
| + 10,000 labels | ~130k | 24.54 ms |
| 20k nodes + 200k edges | 220,001 | 13.05 ms |
| 10k nodes + 100k edges (rebuild every frame) | 110,001 | 10.39 ms |

Per-instance marginal cost ≈ **52–60 ns**; Rune's instance ABI is 128 B, so 110k instances upload
≈ 14 MB/frame (≈ 840 MB/s at 60 Hz). Hover picking with a 16 pt uniform grid: **3.6 µs/query**
average, 11 µs max. Re-running the view to show a hover ring costs **+0.7 ms**. Caveats: headless
only; GPU/present times unusable under machine load (see §2); no LOD, no layout, no labels beyond
the counts above. Harness and logs: `plans/spike-rune-graph-2026-11-06/` (gitignored).

## 4. What production systems do

| System | Renderer | Instance/buffer strategy | Picking | Labels | Documented scale |
|---|---|---|---|---|---|
| **Sigma.js 4.0.4** | WebGL2, instanced unit quads, one merged layer | **retained RGBA32F data textures**, `texSubImage2D` of dirty rows; per-instance attribs | GPU id pass, async PBO + fence, downsampled | GPU SDF atlas | docs: "tens of thousands"; perf guide targets "hundreds of thousands of edges" [M] |
| **cosmos.gl (Cosmograph)** | WebGL2 + luma.gl; points as `POINTS`, links instanced strips | positions live in float textures, GPU-side; layout is fragment shaders on ping-pong FBOs | index FBO + window readback, async | none in engine | "hundreds of thousands"; CPU layouts wall at ~50k nodes [C] |
| **deck.gl 9.4** | WebGL2 + WebGPU, instanced screen-space quads | retained, rebuilt on layer-prop change; GPU aggregation via transform feedback | picking FBO, `readPixelsToArrayWebGL` | none in core | **1M items @60 FPS; 10M @10–20 FPS; 1 GB alloc cap** [M, docs] |
| **GraphWaGu (academic)** | WebGPU, one instanced draw per primitive class, vertex shader fetches positions from a storage buffer that layout also writes | GPU-resident, no layout→render copy | — | — | **100k nodes / 2M edges ≥10 FPS on RTX 2060**; 200k/4M renderable [M] |
| **Graphia (C++/Qt)** | custom OpenGL, `glDrawElementsInstanced` + `glVertexAttribDivisor`, SDF nodes, glyph-atlas labels | per-instance attributes, shader arrowheads | CPU (no GPU pick pass) | glyph atlas | "millions of data points and relationships" [C] |
| **Tulip (C++/Qt)** | OpenGL, CPU vertex-array batches | per-entity buffers | CPU | — | "up to the million" [C]; LOD via `GlCPULODCalculator` / `GlQuadTreeLODCalculator` / `GlGraph{Low,High}DetailsRenderer` [M: files verified] |
| **Rerun re_renderer** | wgpu; lines drawn as **one non-instanced triangle list expanded by vertex id** (its comments argue instancing is slow at small batch sizes) | per-frame uploads | picking layer (RGBA32Uint object/instance ids + depth) | via UI | renderer, not a graph tool [M: source verified] |
| **jbrowse ADR-019** | WebGL2/WebGPU | — | **removed GPU picking**: `mapAsync` cost **50–100 ms/pick** on Firefox+WebGPU; replaced with Flatbush + CPU geometry (<100k instances) | — | [M: ADR text verified] |

**Three convergent patterns:** (1) retained geometry plus a camera matrix uniform, so pan/zoom
uploads nothing; (2) one instanced draw per primitive class; (3) picking decoupled from the paint
pass — either CPU spatial index, or one id-pass readback per camera change, never per hover.

**Three JS-specific patterns that do not transfer:** WebGL2/WebGPU shader duplication; PBO +
`fenceSync` polling loops (a workaround for a synchronous API); per-frame typed-array packing of
instance data (native keeps a mapped ring buffer instead). `Cargo.lock` in this repository has no
`wgpu` dependency today; a native renderer would use wgpu 30 (the version Rune pins), while the
Zed GPUI pin in `Cargo.toml` resolves wgpu through `gpui_wgpu` — any future direct wgpu work must
pin explicitly to avoid two wgpu trees.

## 5. GPU primitives: the numbers that decide

- **Instance record size is the bandwidth budget.** Instanced line quads need only
  `pointA`/`pointB` (16–24 B) plus colour/width; 100k edges × 16 B = **1.6 MB/frame (96 MB/s at
  60 Hz)**, versus our current 128 B record = 12.8 MB/frame (768 MB/s). 128 B is not required by
  any API; it converges from `max_immediate_size` (128 on DX12/Vulkan) and cache-line reasoning
  [C].
- **WebGL2 fallback constrains layout**: `max_vertex_buffer_array_stride = 255 B`,
  `max_storage_buffer_binding_size = 0` (downlevel defaults). A 16–24 B record works everywhere;
  a 128 B record does not.
- **Analytic AA beats MSAA at this shape count.** Mapbox's argument (MSAA "not reliably present …
  generally of poor quality") plus the fill cost: at 1080p, 4× MSAA adds ~33 MB colour + 33 MB
  depth and a resolve, and wgpu cannot MSAA the swapchain. 100k edges × ~60 px × 2 px ≈ 12M
  fragments ≈ **5.8× screen coverage per frame** (estimate from cited constants) — translucent
  edges are fill-bound long before they are vertex-bound.
- **Retained geometry removes 99.99 % of upload**: 12.8 MB/frame → a 64 B matrix. Mapbox and
  Rye Terrell both put the transform in the vertex shader for this reason.
- **Picking**: nodes (10⁴) → CPU grid, sub-µs, needed anyway; edges (10⁵) → either CPU
  distance-to-segment over the visible set (≈ 50–200 µs for 100k, estimate) or one id-pass
  readback per camera change into a CPU-resident buffer. **Never a readback per mouse move**
  (jbrowse measured 50–100 ms).
- **Text**: a glyph is ~28 B (glyphon's record), an R8 atlas at 1–3 fixed sizes; 2,000 labels ×
  ~5 glyphs ≈ 10k glyphs ≈ 280 KB/frame. The cost is collision/index work and re-shaping, not
  rasterisation. Mapbox's collision grid is 25 px with a 100 px viewport padding, rebuilt only on
  size change, placement done on zoom settle.
- **wgpu rules that matter**: 1–5 submissions per frame, no per-frame pipelines/bind groups,
  `write_buffer` over mapped temporary buffers, group bind groups by change frequency.

## 6. Academic evidence (scale and cost)

- **GraphWaGu**: 100k nodes / 2M edges ≥10 FPS (RTX 2060), competitor WebGL libraries ≈3 FPS at
  the same size; GPU Barnes–Hut iteration ≈100 ms up to 20k nodes / 400k edges; 100k/2M converges
  in <3 min. **Layout, not raster, is the bottleneck from ~10⁴ nodes.** [M]
- **Zinsmaier et al. (TVCG 2012)**: density/KDE LOD renders **43,517 nodes / 1,538,840 edges in
  0.4 s** and 99,965 / 4,551,564 in 1.2 s on a GTX 590; ~10⁷ nodes and ~10⁶ edges interactive;
  cost linear in edges and saturating in nodes at pixel count. **The single strongest argument for
  image-space aggregation above ~10⁵ edges.** [M]
- **Edge bundling cost** (CUBu's table, US-airlines graph 235 nodes / 2,099 edges): FDEB **65 s**,
  divided EB **27.5 s**, GBEB 5.6 s, KDEEB 0.5 s, MINGLE 0.2 s, **CUBu 0.014 s**; at 900k edges
  CUBu 152 ms vs KDEEB 8,053 ms. FDEB is quadratic and unusable at our size; only GPU gather-based
  mean-shift is interactive. [M]
- **GPU ForceAtlas2** (ICPP 2017, "Exploiting GPUs for Fast Force-Directed Visualization of
  Large-Scale Networks"): 40–123× over CPU implementations, 4M nodes / 120M edges in 14 min
  instead of 9 h. [M, from the paper]
- **Label placement**: point-feature label placement is NP-hard (4-position model); simulated
  annealing maximises labels, greedy maximises speed; production maps use per-tile ordering with
  an ordered anchor list (≤9 anchors) and priority by draw order — not global optimisation. No
  peer-reviewed benchmark exists for label placement at 10³–10⁵ labels on node-link diagrams. [M/C]
- **Evaluation pitfalls**: "interactive" is undefined (10 vs 60 FPS); capped, averaged FPS with no
  p95/p99; synthetic graphs with |E|:|V| = 20 mislead; hardware changes winners (dedicated vs
  integrated GPU flips the WebGL-vs-WebGPU comparison); preprocessing (layout convergence,
  bundling) is often excluded from "interactive" claims.

## 7. Synthesis: the architecture to build

Ranked by payoff for 10k nodes / 100k edges with smooth pan/zoom and hover:

1. **Retain geometry; per-frame upload is one camera uniform.** Nothing per-edge per frame.
2. **Instanced segment quads, 16–24 B/instance, one draw call per primitive class**; expand
   endpoints in the vertex shader. This is the single largest difference from the GPUI path
   (52–60 ns/instance vs 805 ns/primitive, §3).
3. **Analytic SDF antialiasing, sample_count 1.** No MSAA, no path tessellation.
4. **Zero per-edge CPU work in steady state** — no buffer or pipeline creation per frame, one
   submit.
5. **Picking**: CPU uniform grid or BVH for nodes (3.6 µs measured); for edges, CPU
   distance-to-segment over the visible set, or an id pass with **one** readback per camera change.
6. **Labels**: hard budget (2–3k visible), 25–64 px collision grid with ~100 px padding, placed on
   camera settle, one glyph atlas. Labels are ~2.2 µs each in our spike; 10k labels already cost
   +22 ms.
7. **Edge LOD**: keep *visible* edges in the 30–50k band (2.35 ms measured at 40k instances).
   Above that, aggregate: density/splat or edge bundling (CUBu-class only), or switch overview to
   a matrix/density idiom.
8. **Layout off the frame thread**, incremental and frozen during interaction; GPU Barnes–Hut if
   node counts pass ~10⁴ (published 100 ms/iteration at 20k/400k — still a worker job, not a frame
   job).
9. **Do not bundle initially.** It costs 0.014 s to 65 s depending on method and changes what the
   graph means; make it an explicit mode.
10. **Report p95/p99 frame time on named hardware and named graphs**, separating layout,
    preprocessing and raster.

## 8. Consequences for the renderer decision

- The Rune Shape path already implements (1) via `live_scale`/`live_offset`, (2) via the uber-quad
  pipeline, and (3) via analytic segment AA; it is limited by its fixed **128 B** instance record
  and by re-emitting every instance each frame (paint 2.5 ms at 110k).
- A purpose-built wgpu renderer could use a 16–24 B record and a retained instance buffer, cutting
  upload ~8× and paint to ~uniform-only. At 60 ns/instance the current path is already CPU-cheap;
  the win is headroom (200k+) and fill-rate discipline, not raw throughput at 100k.
- Therefore the decision hinges on **chrome and integration cost**, not on the graph panel: Rune
  avoids writing a renderer but has no data grid, code editor, or tabbed dock (46 rune-kit
  modules, none of those); a wgpu renderer needs an ADR and a host decision (egui, GPUI fork,
  Rune compositor, or native shell).
- Either way, **gpui-graph's non-rendering contracts stay**: scene/runtime/layout/hit-test/LOD
  policy (`crates/gpui-graph/src/{scene,runtime,hit_test,paint}.rs`) are toolkit-agnostic; only the
  paint layer is replaced.

## 9. Open questions (unmeasured)

- GPU/present time and p95 at 100k instances on an idle machine (wall times were unusable under
  load).
- Rune's retained-geometry API (`Renderer::with_springs` / `present_at`) versus re-emission: does
  it remove the 2.5 ms paint at 110k?
- Whether a 16–24 B instance record in a purpose-built wgpu renderer measurably beats Rune at
  100k, or only at 200k+.
- LOD/aggregation design: density splat versus edge budget versus bundling for the console's
  actual queries.
- Label placement at 2–3k visible labels: cost and readability.
- Layout: is 10⁴-node FA2 on a worker enough for the console, or is GPU Barnes–Hut needed?

## 10. Sources

Rendering systems: [Sigma.js](https://github.com/jacomyal/sigma.js) (v4 data textures
`packages/sigma/src/rendering/data-texture.ts`; v4 rationale
[Ouestware](https://www.ouestware.com/2026/10/08/sigma-js-4-0-en/); performance guide
[sigmajs.org](https://www.sigmajs.org/how-to/technical/performance/)) ·
[cosmos.gl](https://github.com/cosmosgl/graph) ·
[deck.gl performance](https://github.com/visgl/deck.gl/blob/master/docs/developer-guide/performance.md) ·
[Graphia rendering](https://github.com/graphia-app/graphia/tree/master/source/app/rendering) ·
[Tulip](https://github.com/Tulip-Dev/tulip) (`library/tulip-ogl/src/Gl*LOD*`, `GlGraph*DetailsRenderer`) ·
[Rerun re_renderer lines](https://github.com/rerun-io/rerun/blob/main/crates/ui_tools/re_renderer/src/renderer/lines.rs) ·
[Rerun picking layer](https://github.com/rerun-io/rerun/blob/main/crates/ui_tools/re_renderer/src/draw_phases/picking_layer.rs) ·
[jbrowse ADR-019](https://github.com/GMOD/jbrowse-components/blob/main/agent-docs/architecture-decision-records/adr-019-synteny-cpu-picking.md).

Papers: GraphWaGu ([DOI 10.2312/pgv.20221067](https://doi.org/10.2312/pgv.20221067),
[PDF](https://stevepetruzza.io/pubs/graphwagu-2022.pdf)) · Zinsmaier et al.
([DOI 10.1109/TVCG.2012.238](https://doi.org/10.1109/TVCG.2012.238)) · CUBu
([PDF](https://webspace.science.uu.nl/~telea001/uploads/PAPERS/TVCG15/paper.pdf)) · FDEB
([DOI 10.1111/j.1467-8659.2009.01450.x](https://doi.org/10.1111/j.1467-8659.2009.01450.x)) · KDEEB
([DOI 10.1111/j.1467-8659.2012.03079.x](https://doi.org/10.1111/j.1467-8659.2012.03079.x)) ·
MINGLE ([DOI 10.1109/PACIFICVIS.2011.5742389](https://doi.org/10.1109/PACIFICVIS.2011.5742389)) ·
GPU ForceAtlas2 ([DOI 10.1109/ICPP.2017.47](https://doi.org/10.1109/ICPP.2017.47)) · Walshaw
multilevel ([DOI 10.1007/3-540-44541-2_17](https://doi.org/10.1007/3-540-44541-2_17)) · FM3
([DOI 10.1007/978-3-540-31843-9_29](https://doi.org/10.1007/978-3-540-31843-9_29)) · PFLP
simulated annealing ([DOI 10.1145/212332.212334](https://doi.org/10.1145/212332.212334)) ·
Matrix Zoom ([DOI 10.1109/INFVIS.2004.46](https://doi.org/10.1109/INFVIS.2004.46)) · ZAME
([DOI 10.1109/PACIFICVIS.2008.4475479](https://doi.org/10.1109/PACIFICVIS.2008.4475479)) ·
GraphPU ([write-up](https://latentcat.com/en/blog/building-graphpu), unverified claims).

Primitives: [Instanced Line Rendering](https://wwwtyro.net/2019/11/18/instanced-lines.html) ·
[Mapbox antialiased lines](https://blog.mapbox.com/drawing-antialiased-lines-with-opengl-8766f34192dc) ·
[Mapbox label placement](https://docs.mapbox.com/help/dive-deeper/optimize-map-label-placement/) ·
[wgpu Dos and Don'ts](https://docs.rs/wgpu/latest/wgpu/struct.Device.html) (best-practices docs) ·
[bevy_polyline](https://github.com/james-j-obrien/bevy_polyline) · [glyphon](https://github.com/grovesNL/glyphon).

Our own: [ADR 0076](../adr/0076-explorer-web-worker-backend.md) · Rune spike
(`plans/spike-rune-graph-2026-11-06/`, gitignored: harness, per-run frame logs, snapshots).

---

## 11. Addendum: the three failed branches, re-run

Anchor timestamp: 2026-11-07 03:56:42 UTC +0000 (same day; appended, not rewritten).

The three branches that failed in §2 were relaunched. **Native applications** and **force layout**
reported; the dedicated LOD/label branch did not, so §11.3 is assembled from the native sources
read for §11.1 plus the sources already cited in §5 and §6. All file references below were read
from the repositories named.

### 11.1 Native C++ applications: three different answers

**Graphia** — GPU instancing, CPU picking, no per-element LOD.

- Every per-instance attribute is a divisor-1 attribute (`glVertexAttribDivisor`), in
  `source/app/rendering/graphrenderercore.cpp`: glyphs at 186–207 (`component`, `textureCoord`,
  `textureLayer`, `basePosition`, `glyphOffset`, `glyphSize`, `glyphScale`, `color`), nodes at
  233–238 (`nodePosition`, `component`, `size`, `innerColor`, `outerColor`, `selected`), edges at
  273–282 (`sourcePosition`, `targetPosition`, `sourceSize`, `targetSize`, `edgeType`, `size`,
  colours, `selected`).
- **Three instanced draws per state bucket**: sphere mesh (582), arrow mesh (606), rectangle/quad
  (640). Per-component state (matrices, lighting) is *not* in the instance buffer; it lives in a
  buffer texture read by the `component` instance index (`bindComponentDataTexture`, 557), so
  moving a component rewrites ~34 floats.
- Edges are **one unit arrow mesh** (cylinder + cone) stretched between endpoints in
  `shaders/instancededges.vert` (head split by `ARROW_HEAD_CUTOFF_Y = 0.24`, head length clamped
  to `0.25 × edgeLength`, endpoint sizes used for trimming). Nodes are lit tessellated spheres
  (`nodecolorads.frag`), not SDF discs.
- Labels: one `GlyphData` per glyph, instanced by `glDrawElementsInstanced` (640) from a
  **2048² shelf-packed atlas** (`glyphmap.cpp:43`), layouts cached per unique string
  (`graphrenderer.cpp:204–270`). SDF atlas generated once (`sdf.frag`).
- **Picking is CPU and O(N)**: `graphcommoninteractor.cpp:535–545` casts a camera ray and calls
  `Collision::nearestNodeIntersectingLine` (`layout/collision.cpp:96`), with a 40 px
  `PICK_RADIUS` cone fallback (547–562). The only `glReadPixels` (graphrenderercore.cpp:1240) is
  a debug glyph-map path.
- **No per-element culling or LOD** in the draw path: every node/edge of every visible component is
  instanced (`graphrenderer.cpp:300–460`); only hidden/merged elements and label visibility state
  filter. It leans entirely on one draw call per kind.

**Tulip** — CPU-batched, per-entity LOD by projected size, quadtree culling, GL selection picking.

- LOD metric: **projected AABB size in viewport pixels** for 3D
  (`GlCPULODCalculator.cpp:104–190` → `calculateAABBSize`, `GlTools.cpp:328–406`), which also
  returns `-1` for off-screen (a cull) and `10` when the eye is inside the BB. The 2D path
  (`GlTools.cpp:412`) returns **world-space area**, not pixels — an inconsistency between the two
  camera paths worth not copying.
- `GlQuadTreeLODCalculator` prunes first: per-layer quadtrees, view-rect world AABB query
  (`getElementsWithRatio(cameraBoundingBox, res, ratio)`), only survivors get LOD; the tree is
  rebuilt only when layout/size/selection/camera-direction/visibility change (105–190, 300–460).
- **The LOD ladder (the numbers to copy)**:
  `GlNode.cpp:33` `LOD_MIN_TRESHOLD 10.0` — below 10 px a node glyph becomes a 4 px point
  (`:96`); `GlEdge.cpp:150` below 5 px becomes a 1 px point, and `:169–176` chooses line vs
  quad/tube by width `< 5`; `GlLabel.cpp:605` draws a line instead of text when the viewport height
  of the label is `< 2 px`, thins the outline at `≤ 25 px`, and masks overlapping labels with a
  linear-scan AABB set (`OcclusionTest`), selected labels first
  (`GlGraphHighDetailsRenderer.cpp:489–497`); `labelsDensity == -100` disables labels entirely.
- Two whole renderers: `GlGraphLowDetailsRenderer` batches the entire graph as `GL_LINES` edges and
  `GL_QUADS` nodes with per-vertex colour, no lighting/depth/labels (45–210, 64k-index chunks);
  `GlGraphHighDetailsRenderer` draws per entity with optional z-ordering and occlusion tests
  (236–500). `GlGraphComposite` only constructs the high one (32, 51, 196), so low-detail is
  caller-selected.
- Buffers: **batched, not instanced** — 7 VBOs (points/lines/quads × vertices+colours) refilled
  whole-array with `GL_STREAM_DRAW` + orphaning on layout change
  (`GlVertexArrayManager.cpp:61–82, 355–420`).
- **Picking is GPU selection mode**: `glSelectBuffer` + `GL_SELECT` + `glLoadName(id)`
  (`GlScene.cpp:752–820`, `GlGraphHighDetailsRenderer.cpp:502–524`) — the only one of the three
  that does not pick on the CPU.

**Gephi** — three interchangeable GPU pipelines, tessellation-only LOD, CPU picking.

- Pipeline preference `ArrayDraw` / `InstancedDraw` / `IndirectDraw` = 0 / 50 / 100
  (`modules/VisualizationEngine/.../jogl/availability/`), so indirect (`glMultiDrawArraysIndirect`)
  is preferred, then instanced, then vertex arrays; each honours `OpenGLOptions` disables.
- Instanced nodes: flat per-instance attribute buffer updated with `updateWithOrphaning`, drawn with
  `glDrawArraysInstanced` (`pipeline/instanced/InstancedNodeData.java:29–72`).
- **LOD is tessellation only**: circle mesh 64/32/16/8 vertices at screen sizes 128/16/2 px
  (`AbstractNodeData.java:48–50`); instance count never changes.
- Culling before buffer build: `GraphIndexImpl.getVisibleNodes` → spatial index query; picking is
  CPU via the same index (`GraphIndex.java:43–58`); labels are batched text quads, not instanced.

**Cross-cutting conclusions.**

1. **The drop order is labels → edges → node glyphs**, at roughly **2 px / 5 px / 10 px** of
   projected size. Below those sizes, text, edge shading and node glyphs are unreadable. This is
   the most transferable result of the branch, and it is measured behaviour in Tulip's source, not
   opinion.
2. **Two of three pick on the CPU** (Graphia O(N) ray, Gephi spatial index); only Tulip uses GPU
   selection. This corroborates §5's conclusion from jbrowse.
3. **Instancing is not the differentiator Graphia's scale comes from** — it also carries per-element
   data budgets (unit meshes, buffer textures, one texture-compressed atlas). Tulip reaches the
   million with *no* instancing, using batching + LOD; the two are different bets with the same
   ceiling.
4. **Culling**: only Tulip and Gephi cull per element; Graphia does not cull in the draw path at all.
   At our 100k ceiling, culling is worth it only for the zoomed-in case; LOD/aggregation is the
   zoomed-out answer (§6, Zinsmaier).

### 11.2 Force layout: the numbers that settle the design

**CPU baseline (measured, derived from BatchLayout Table 2 runtime ÷ 500 iterations, 47 threads on
48 cores, [arXiv:2002.08233](https://arxiv.org/abs/2002.08233)).**

| Graph | Gephi FA2-BH |
|---|---:|
| 10,800 nodes / 399,600 edges | **21.4 ms/iteration** |
| 22,044 / 979,380 | 37 ms |
| 37,376 / 298,496 | 40.7 ms |
| 74,752 / 261,120 | 94 ms |
| 114,599 / 239,332 | 145 ms |

Single-thread FA2-BH on the same 10.8k graph: **≈394 ms/iteration** (433.6 s for ~1,100
iterations, minibatch = 1). BatchLayout's exact variant is 15× FA2, its BH variant ≈4× FA2-BH and
≈2× OpenOrd at comparable quality. **FA2 is 20–40× off 60 fps at 10k nodes on one core**, and 2–3×
off even on 47 threads — layout cannot share the frame thread with rendering at 10k.

**GPU and multilevel (reported).**

- GPU ForceAtlas2 (ICPP 2017): **40–123×** over CPU implementations, 4M nodes / 120M edges in
  **14 min** (vs 9 h); 38 real networks. Per-iteration ms not public.
- GraphWaGu (verified from the PDF): Barnes–Hut **≈100 ms/iteration including rendering** until
  **20,000 nodes / 400,000 edges**; 100k/2M converges in **<3 min**. GPU layout at 10k is therefore
  the *same order* as a CPU worker, and wins only from ~50k nodes.
- Multilevel: Walshaw **12 s for 10k vertices**, 5–7 min for the largest (225k); FM3 **<5 min for
  100k**. Multilevel is for one-shot/initial placement, not continuous interaction — FA2's authors
  rejected coarsening precisely because it cannot be continuous.

**Interaction model (what every tool converges on).** Worker/GPU computes, main thread renders a
**double-buffered position snapshot** at 60 fps, forces freeze during drag, local refinement after
edits, stop on stability. graphology ships a worker build; d3-force documents `stop()` + manual
`tick()`; ngraph checks stillness every 100 steps.

**Rust.** `forceatlas2` 0.8.0 is **AGPL-3.0-only** (rejected for this repository unless relicensed);
`fjadra` 0.2.1 (MIT/Apache, d3-force port, quadtree many-body) is the fallback; `fdg` (MIT) has no
SIMD/parallel/GPU yet (open issue) and no benchmarks. No Rust crate publishes ms/iteration, so the
choice is by licence and measured self-benchmark, not by claim.

**Consequence for us:** keep the existing FA2 + Barnes–Hut (already rayon-parallel, 8.2 ms for 8
iterations at 5k nodes per ADR 0076), run it on a worker, time-slice 1–2 iterations per frame, and
publish snapshots. GPU layout is a later option, not a 10k-node requirement.

### 11.3 LOD and label policy to implement first

The dedicated LOD/label branch did not report; this section combines §11.1 (native LOD ladders),
§5 (text/collision primitives) and §6 (aggregation evidence).

1. **Labels first, at a hard budget.** Show 2–3k labels maximum, choose by priority (selection,
   query result, degree), place with a 25–64 px collision grid and ~100 px viewport padding
   (Mapbox's numbers), re-place on camera settle only, draw selected labels first (Tulip's order).
   Below ~2 px of label height, replace text with a line or drop it (Tulip).
2. **Edges second, bounded by count.** Omit edges shorter than ~5 px on screen and drop arrowheads
   below a size threshold (both already exist as `GraphStyle` knobs in `crates/gpui-graph`). Keep
   the *visible edge count* in the 30–50k band — our measurement is 2.35 ms at 40k instances.
3. **Nodes last.** Below ~10 px of diameter, a node becomes a fill-only point (already
   `node_simplify_threshold` in `crates/gpui-graph`).
4. **Above 100k visible edges, stop drawing them individually.** Density/splat aggregation
   (Zinsmaier: 1.5M edges in 0.4 s) or bundling (CUBu: 900k edges in 152 ms); FDEB is not an
   option (65 s on 2k edges).
5. **Culling** with a grid/quadtree view query is worth it when zoomed in; it does nothing at
   zoom-out where everything is visible — our measurement agreed (cull changed 10.39 → 10.51 ms).

### 11.4 What changed in the recommendation

§7 stands; this addendum sharpens it:

- The **LOD ladder now has measured thresholds** (2/5/10 px) instead of a principle.
- **Layout must leave the frame thread** at 10k; the worker + snapshot pattern is the industry
  norm, and GPU layout is not needed below ~50k.
- **CPU picking is the majority design** (Graphia, Gephi, jbrowse) — our 3.6 µs grid is the right
  side of that bet; Tulip's GL selection is the outlier.
- Instance-record size still matters (§5), but the native evidence shows that **budgets and LOD
  matter more than the record**: Tulip reaches a million with no instancing at all.

---

## 12. Addendum: the LOD/label branch reported (concrete defaults)

Anchor timestamp: 2026-11-07 04:45:00 UTC +0000. The dedicated LOD/label branch
that failed in §2 and was marked "did not report" in §11.3 reported later. Its
concrete numbers refine §11.3; the policy there stands, with these defaults:

**Label LOD (measured in the named tools' source).**

| Tool | Knob | Default |
|---|---|---|
| Sigma.js 4.0.4 | `labelRenderedSizeThreshold` (min rendered node size for a label) | **6 px** |
| Sigma.js 4.0.4 | `labelGridCellSize` / `labelDensity` | **100 px** grid / **1** label per cell (×`1/ratio²` when zoomed out) |
| Sigma.js 4.0.4 | `hideLabelsOnMove` / `hideEdgesOnMove` / `enableEdgeEvents` | false / false / **false** |
| MapLibre / Mapbox | collision grid; viewport padding; `text-padding`; anchors | **25 px**; **100 px**; 2 px; up to 8 variable anchors tried in order, previous anchor retried first on zoom |
| Gephi (viz engine) | overlap grid (`avoidNodeLabelOverlap` off by default) | **15** world units; larger label wins |
| Tulip | `labelsDensity` (−100 none … 0 no overlap … 100 all) | **0**; label min/max size 4/30 px |

- Production decluttering is **greedy priority + uniform grid**, never simulated
  annealing. Sigma sorts candidates by rendered node size; Mapbox uses
  `symbol-sort-key`; Tulip draws selected labels first.
- No tool publishes a max-visible-label count; the budgets are emergent: at
  1920×1080 Sigma's default implies ≈220 cells → ≈220 labels, which matches our
  spike's measured 133–373 placed labels on a dense disc.
- **Hide during motion** is the standard lever (`hideLabelsOnMove: true`); Mapbox
  throttles re-placement to at most once per 300 ms because global collision is
  too expensive per frame.

**Edge LOD.** Only Tulip has an automatic screen-space bound: edge LOD < 5 px
renders a 1 px point, ≤ 0 is dropped; node LOD < 10 px renders a 4 px point,
and metanodes only appear at LOD ≥ 10. Sigma, Gephi and Graphia have **no**
zoom-dependent edge LOD; Graphia only culls edges fully enclosed by their
endpoints. Our `edge_min_length` / `edge_arrow_min_length` / `edge_straight_threshold`
knobs implement the same axes with configurable thresholds.

**Aggregate-overview threshold.** The only published number is Ghoniem et al.
(2004): matrix-based views beat node-link for >20 vertices except path-finding,
at link densities 0.2–0.6 (`d = E/V²`). Our demo graph's density is ≈0.001, far
outside that study, so **no published threshold justifies switching our overview
to a matrix**. Node-link plus LOD/labels is defensible; aggregation is a separate
choice (Zinsmaier's density LOD, §6), not a literature-mandated switch.

**Documentation-vs-source inconsistencies found** (the repository rule asks that
these be reported rather than silently followed):

- Tulip's `GlCPULODCalculator` header says edge LOD is off by default and edges
  get LOD 10, but the constructor sets `computeEdgesLOD(true)`.
- Tulip's `GlNode.cpp` comment says "less than four pixel on screen" while the
  constant is `LOD_MIN_TRESHOLD 10.0`.
- Gephi's quickstart documents a "how many labels should appear" density slider,
  but master's `GraphRenderingOptions` exposes only show/hide, overlap avoidance
  and size mode — version drift; check the target release before copying a
  default.
- Sigma's v4 label pass has **no rectangle-vs-rectangle test**, only cell
  occupancy, so a label wider than the 100 px cell can overlap its neighbour.
  Copy the grid, not that gap: our own placement estimates a real rect and tests
  overlap.

---

## 12b. Addendum: what 100k edges cost, and the aggregation design

Anchor timestamp: 2026-11-07 12:25:00 UTC +0000. Measured with the same Rune adapter over
`graph-view-core` (10k nodes, camera fitted, thread CPU):

| Case | Frame build |
| --- | --- |
| 100k edges, curved bows | **42.9 ms** |
| 100k edges, all straight (LOD) | **11.7 ms** |
| 100k edges, live layout (snapshot adopted every 3rd frame) | 25–34 ms |

So the per-edge geometry pipeline itself is fine at 100k (**117 ns per edge** straight), and the
blocker is the **curved-edge bow**: density-shaped control points plus the obstacle pass cost 31 ms
of the 42.9 ms. Two ways to remove it, and only one keeps the paint/hit-test agreement that §21
requires:

- *Straight-LOD everything above N edges.* Cheap, but a bowed edge and a straight edge differ by
  several pixels, so a click near the bow would miss what is drawn. `hit_test` would have to apply
  the same count-based rule, and it does not know the visible count without rebuilding candidates
  per hover. Rejected as a permanent policy (it stays legitimate *during* motion, where the core
  already has `edge_straight_threshold_while_interacting`).
- *Aggregate: draw a density field instead of individual edges.* The research §6 evidence (Zinsmaier
  density LOD; deck.gl/Sigma practices) and the numbers above both point here.

**Implemented (2026-11-07).** The core now has the threshold, the splat primitive, the emission and
the hit-test hand-off; the Rune adapter draws the cells. Measured with the same adapter (10k nodes,
camera fitted, thread CPU):

| Case | Before | Aggregated |
| --- | ---: | ---: |
| 100k edges | 42.9 ms (curved) / 11.7 ms (straight) | **3.6 ms** |
| 400k edges | 46.9 ms (non-aggregated) | **6.6 ms** |

Two decisions came out of implementing it. First, aggregation runs **before** classification and the
precise cull: an aggregate needs only screen-space endpoints, so per-edge classification, culling and
all curve work are skipped, and the frame cost is dominated by the candidate query alone. Second, the
rasterization **samples the candidate list with a stride** above 65,536 edges and scales the per-cell
length by that stride, which bounds the frame cost by the cap rather than the graph size (that is
why 400k costs 6.6 ms and not 40 ms). **Wire form (implemented).** The aggregate layer now rides the ADR 0076 frame wire as
`EdgeSplatPlanes` (rectangle min/max plus intensity per cell), the interaction transform maps the
cells with the camera like every other screen-space primitive, and the random-frame round-trip tests
include splats so encode/decode and the byte transport stay field-for-field exact. A worker or remote
host can therefore receive an aggregated frame; before this the encoder refused one.

**Hysteresis (implemented).** A graph hovering at the threshold would otherwise switch
representation every frame, rebuilding its whole edge layer each time. `GraphStyle::aggregate_threshold_for(currently_aggregated)`
returns the enter threshold, or one eighth below it while aggregated, so leaving needs a visible
set that has fallen a step below the entry point; the host passes it to the frame build and feeds
back the frame's own decision. The GPUI view tracks that decision explicitly (`edges_aggregated`)
for every frame source, because `delivered_frame` is only populated on the worker path.

The remaining design is as decided:

1. **Trigger** - a style field `edge_aggregate_threshold: Option<usize>`: when the post-cull visible
   edge count exceeds it, the frame carries an aggregate edge layer instead of per-edge geometry.
2. **Representation** - `PaintFrame.edge_splats: Vec<PaintEdgeSplat>` with screen-space cell bounds
   and an intensity: one emit per cell whose accumulated edge length passes a floor. A splat is a
   filled rectangle, not a path: Rune draws a quad, GPUI draws a `paint_quad`, and a future wgpu
   adapter draws one instanced quad.
3. **Emission** - one pass over the surviving edges rasterizing each screen segment into a 12–16 px
   cell grid (DDA) and accumulating length, so the cost is O(E + cells) with one integer step per
   cell crossed, and no Bézier, density or obstacle work at all.
4. **Hit testing** - aggregated edges are not individually hittable, and the frame says so by
   carrying splats instead of edges. Nodes stay hittable. This is the one behavioural trade the
   approach makes, and it is why it must be a frame-level switch rather than a per-edge filter:
   §21's agreement becomes "what the frame contains is what hit-testing can name".
5. **Budget** - the aggregate layer's size is bounded by the cell grid (a 1200x800 viewport at 12 px
   is at most ~6.7k cells, and only occupied cells emit), so 100k and 1M visible edges cost the
   same to draw.

What this does not solve: the per-edge **layout** cost and the retained-geometry update (layer B);
those are separate levers, measured in §14.

## 13. Addendum: the console prototype cannot join the workspace yet

Anchor timestamp: 2026-11-07 06:55:00 UTC +0000.

The console prototype (`plans/console-prototype/`, gitignored) was moved toward
`crates/gleaph-console` with Rune pinned as a workspace dependency. Cargo re-resolves the workspace
graph, and the repository's dependency-age policy (`.cargo/config.toml`:
`[registry] global-min-publish-age = "14 days"`) rejects Rune's fresh transitive dependencies:

- `accesskit 0.25.2`: "too new (published 12 days ago, minimum age 14 days)".
- With `CARGO_RESOLVER_INCOMPATIBLE_PUBLISH_AGE=allow`, the next blockers are `hashbrown` (via
  `parley`), `bitflags` (via `zbus`) and `unicode-segmentation` (Rune requires `^1.13`): the
  registry-global age filter is not relaxed by the resolver-level env var.

The workspace membership was reverted; `cargo metadata` passes. The prototype stays parked outside
the workspace with the exact error text and unblock steps in its README. The repo-side half landed
independently: `gleaph_gql::highlight` (UI-neutral lexical kinds, six tests) and the highlighting
boundary note in `design/gql/layers.md`.

Options, in order: wait for `accesskit 0.25.2` to age past the window (two days from 2026-11-07),
pin an older Rune rev and re-run the measurements, or relax the dependency-age policy as an explicit
decision. Do not relax it silently for one crate.

---

## 14. Addendum: the Rune adapter over `graph-view-core`, and what it exposed

Anchor timestamp: 2026-11-07 09:10:00 UTC +0000.

A prototype adapter (`plans/rune-adapter/`, gitignored) draws the console's graph panel from
`graph-view-core` instead of a hand-rolled scene: `GraphScene` + `ForceAtlas2` with
`LayoutSource::BackgroundThread`, `build_indexed_paint_frame`, `hit_test`, `Viewport`, and Rune
geometry built from the frame (one analytic capsule per edge piece, one zero-length round-capped
stroke per node). It depends on the core with `default-features = false`, and
`cargo tree` confirms **no GPUI in the dependency tree** — the crate split works from outside the
workspace, which is how the adapter is being developed before the dependency-age window opens.

Measured headless, 1200×800 equivalent canvas, thread CPU medians, machine under heavy external
load (`--profile perf`):

| Case | Frame build (`build_indexed_paint_frame`) | `sync_runtime` | Rune shape build | Frame CPU p50 |
| --- | --- | --- | --- | --- |
| 5,000 nodes / 7,400 edges, all visible, live FA2 | **16–25 ms** | 9–16 ms | 0.7–0.8 ms | — |
| 10,000 nodes / 30,000 edges, all visible, live FA2 | **550–950 ms** | 66–70 ms | 6–8 ms | 6.4 ms at idle, spikes every third frame |
| 10,000 nodes / 30,000 edges, no layout running | — | — | — | **1.15 ms** steady |

Three findings, in order of importance:

1. **The core's paint-frame build is now the dominant cost, and it is renderer-independent.**
   A controlled scaling run (layout paused, camera fitted, `Frame_us` of `sync`) gives: 5k/7.4k
   **9.9 ms**, 10k/15k **23.6 ms**, 10k/30k **45.6–61.5 ms**, 10k/60k **129.8 ms** — i.e. roughly
   **linear at ~2 µs per visible edge**, plus a node term. *(Correction: an earlier run in this
   record reported 550–950 ms at 10k/30k and "superlinear in local density". That was machine
   load noise, not a scaling law; the controlled runs above replace it.)*
   Of the 45.6 ms at 10k/30k, about **20 ms** is edge culling/classification/geometry that also runs
   with every edge straight, and **~26 ms** is the curved-edge path (density grid lookups + obstacle
   field + Bézier control points). GPUI's ADR 0076 baseline of 9.9 ms at 5k/7.4k agrees with the
   small case. Whatever renderer draws the frame, the frame build has to be fixed first.

   **Where the density phase's time goes** (isolated A/B, single-threaded, clustered 10k/30k
   midpoint sets): a `DensityGrid` query returns **101 candidates per edge at 10k and 299 at 30k**,
   of which only ~36% are inside the radius. Replacing the square root per candidate with a
   squared-distance reject (one root per survivor) took 30k from 74.9 ms to 67.5 ms — a 10% win, not
   the fix. The cost is the **neighbour count**, not the per-pair arithmetic, so the options are
   structural: cache the densities per geometry revision (they depend on world positions only, so
   pan/zoom need not recompute them at all), or bound the neighbour set / use a coarse field for the
   live-layout case where positions change every snapshot. Measured parallel cost in the adapter is
   ~26 ms of the 45.6 ms, so caching alone would take pan/zoom frames to roughly 20–25 ms.

   **Implemented (density cached on `EdgePrep`).** `signed_densities_for` now runs once per geometry
   revision in `build_edge_prep` and the paint layer reads `prep.densities`; the grid query is no
   longer part of a frame build. Measured in the adapter at 10k/30k with the camera static:
   **45.6–54 ms → 24.6–26.1 ms** (three runs), i.e. about 20 ms and 45% of the frame build removed,
   matching the isolated estimate. A direct-query equivalence test (`cached_densities_match_a_direct_density_query`)
   pins the cache to the same values, and a static scene renders a **byte-identical PNG** before and
   after the change, so the optimization is invisible in the output. Live layout still rebuilds the
   prep when positions move (correctly: density is a function of positions), which is why the
   remaining cost for a live-layout console is the per-snapshot rebuild, not pan/zoom.

   **The next bottleneck was the edge spatial index, not the geometry.** Phase timings of a frame
   build (env-gated instrumentation, since removed) at 10k/30k with every edge straight showed
   `visible_edge_candidates` taking **18–25 ms of a ~20 ms build** — i.e. essentially all of it.
   Counting the work explained it: one query visited **22,656,583 grid entries for 30,000 edges**
   (755 cells per edge), because each edge's bounding box is inserted into every 64-unit cell it
   covers and a long edge's box covers hundreds. Capping cells per edge at 64 and sending wider
   boxes to the existing overflow list (`MAX_CELLS_PER_EDGE`) cut the same query to **68,175
   entries and 0.6 ms**, with the candidate set still a superset (an overflow edge is always a
   candidate, pinned by `a_wide_edge_bbox_uses_the_overflow_list_and_stays_a_candidate`). Frame
   build at 10k/30k then measured: straight **20.2 → 7.9 ms**, curved **24.6–26.1 → 11.6 ms**. The
   rendered output is visually unchanged (labels, edges, positions identical in the snapshot; the
   PNG bytes differ only because candidate order changes paint order).

   Running total for one arc of work, same machine and camera: **61.5 ms → 11.6 ms** for a curved
   10k/30k frame build, from three changes that each preserve behaviour (stable label order,
   revision-cached density, cell-capped edge index).

   **Live layout at 30k.** With layout running off-thread and a snapshot adopted every third frame,
   the adapter measures 10.4–12.4 ms of frame build on adoption frames at 10k/30k (the curve LOD is
   forced straight while the layout moves — a moving graph does not need bows — and restored on
   settle). Layer B (position-only updates) is what would make those frames nearly free, but a Rune
   host cannot update an existing shape's instance positions, so the motion LOD is the lever
   available to the console: the graph runs at ~50–60 fps on adoption frames while pan/zoom stays
   inside the budget.
2. **Retained geometry makes idle frames nearly free**: 1.15 ms per frame at 10k/30k with 40k
   instances when nothing changes (paint 0.9 ms, encode 0.4 ms). Layer A of ADR 0103's trajectory
   is confirmed on a real adapter, not just a spike. A layout snapshot is therefore the expensive
   frame, not the drawn frame: a host that rebuilds the frame per snapshot pays the 2 µs/edge build
   cost, which is the gap layer B closes.
3. **Layer B is not available to a Rune host yet**, so a layout snapshot means rebuilding the whole
   shape (6–8 ms at 30k edges) plus a new frame. Publishing every third frame keeps the frames
   between them free, which is what the prototype does; the numbers above are dominated by the
   snapshot frames.

Not yet done, and therefore claimed nowhere: rendered output was produced as PNG snapshots but
**not visually inspected** (the producing model is text-only), labels are placed with an estimated
advance width rather than shaped-text metrics, and the pointer/click paths are wired but not
exercised headlessly.

## 15. Where the console's first-frame stall comes from (2026-11-07)

The console's aggregate run at 100k edges intermittently reported a 39-59 ms `sync`, which was
inconsistent with the 3.6 ms aggregate frame path measured in §14. Separating the two by timing
inside `GraphScene::sync_runtime` and inside `build_edge_prep` (temporary instrumentation, since
removed) gave the answer: the stall is **not** the frame path.

Measured with the Rune adapter, 10 000 nodes, `GRAPH_AGGREGATE` on or off, thread CPU:

| Case | first sync (runtime rebuild) | later syncs (position-only update) |
| --- | ---: | ---: |
| 40k edges | 114 ms (runtime 113.7 ms) | 2.7-4.6 ms |
| 100k edges | 295 ms (runtime 294.5 ms) | 4.5-5.9 ms |

Inside `build_edge_prep` at 100k edges: `signed_densities_for` **135.7 ms**, group table 6.2 ms,
parallel/reverse flags 2.2 ms, density grid 1.1 ms; node cells 0.3 ms and the edge bbox index
9.2-15.8 ms. The density query alone was 1.36 us per edge.

Fix: the per-edge density is now **lazy per rebuild** (`EdgePrep::densities` over a `OnceLock`).
A rebuild or an aggregate frame (which draws no edges) never pays for it; the first frame that
draws edges pays it once and every later frame reuses it. The eager cache field and the test that
compared it against a direct query were replaced by one that asserts the lazy cache equals a direct
query and is computed once.

After the change: 100k rebuild **26.7-27.4 ms** (was 295 ms, ~11x), aggregate steady frames
3.9-4.9 ms (unchanged), non-aggregate steady frames 31-43 ms with the density cached. The remaining
spike is the density query itself, now on the first non-aggregate frame after a rebuild (~135 ms plus
curve building); an approximation for it, or computing densities only for the edges that survive the
cull, is the next lever.

## 16. Density per visible edge, and a hit-test indexing defect (2026-11-07)

§15 left one cost: the lazy density cache computed every edge on its first use, so a zoomed-in frame
paid for the whole graph. Densities are now cached **per edge** (`Vec<OnceLock<f32>>`), and
`EdgePrep::densities_for(&[usize])` computes and stores only the indices it is asked for. A rebuild
still costs nothing (`26.7 ms` at 100k, unchanged from §15); a frame pays for its candidates and
reuses them afterwards. Measured: 4 800 candidates take ~6 ms, versus 136 ms for all 100k. The
synthetic adapter graph is a poor showcase for the win - it is full of long edges, so even at zoom 40
about 46 000 of 100 000 edges remain candidates - but a real graph with local edges culls far more.

The same change fixed a **defect in `hit_test`**: it computed densities aligned with its candidate
list, then read them with `signed_densities[index]` - the *edge* index - instead of
`signed_densities[position]`. Whenever the candidate set was a strict subset (any zoomed-in view), a
candidate whose edge index exceeded the candidate count read past the end (panic in debug builds) or
silently used another edge's density. The pointer path is not exercised headlessly, which is why it
went unnoticed. A regression test now hit-tests the last of three widely separated edges through a
viewport that culls the other two.

## 17. A local-edge test graph, and what culling buys (2026-11-07)

The adapter's synthetic graph connected node `i` to `i + 1..40`, but positions are clustered by
`i % 14`, so nearly every edge spanned two communities: at zoom 40, 46 000 of 100 000 edges were still
camera candidates, and the per-edge density work could not show its win. The generator now keeps an
edge inside its community (same `index % 14`, a few rings away); `GRAPH_LONG=1` restores the old
generator for stress runs. The rendered result also looks like a real graph - communities with local
stars instead of a uniform hairball (`plans/rune-adapter/local-graph-{dark,light}.png`).

With the local graph, 10 000 nodes / 100 000 edges:

| View | Edges drawn (candidates) | first sync | steady sync |
| --- | ---: | ---: | ---: |
| fit (whole graph) | 99 806 | 198 ms | 42 ms |
| zoom 6 | 10 614 | 146 ms | 17-24 ms |

So culling now removes ~90% of the graph at zoom 6. The remaining cost at fit is the price of
drawing every edge individually (density plus curve geometry) - which is exactly the case the
aggregate path exists for: the same graph aggregated draws 0 edges for a ~4 ms frame.

## 18. A cell-aggregate density approximation was measured and rejected (2026-11-07)

§16-17 leave one cost: the exact signed-density query is ~2.4 us per edge, so drawing every edge of a
100 000-edge graph individually pays ~136 ms for density alone. The obvious approximation - one
centroid and count per grid cell, contributing `count * proximity(centroid) * cos(angle to centroid)`
so an edge reads O(cells in range) instead of O(edges in range) - was evaluated before adopting
anything.

Study (`paint::density_approximation_study`, `#[ignore]`d, 4 000 clustered midpoints, exact
reference = the production query):

| Aggregate cell size | time | mean abs error | p95 | max | correlation |
| --- | ---: | ---: | ---: | ---: | ---: |
| exact (reference) | 9.6 ms | - | - | - | 1.00 |
| 1 × radius | 9.0 ms | 3.12 | 8.68 | 27.1 | 0.54 |
| 0.5 × radius | 26.4 ms | 1.12 | 3.04 | 10.5 | 0.86 |
| 0.25 × radius | 56.7 ms | 0.43 | 1.17 | 4.03 | 0.96 |

Mean `|density|` is 1.63, so the only resolution that correlates well is slower than the exact query,
and the only fast one has error larger than the signal. The coarse variants are also slower than the
exact loop because each cell read is a hash lookup while the exact loop walks a contiguous list.

**Decision: keep the exact per-edge query.** A viable approximation would need dense cell arrays
instead of the hash map and would still have to accept p95 errors of several units at the resolutions
that are fast - an edge's bow would visibly change, which is a design decision, not an
optimisation. The remaining lever is reducing *how many edges are drawn individually* (the aggregate
path), not making each edge's density cheaper.
