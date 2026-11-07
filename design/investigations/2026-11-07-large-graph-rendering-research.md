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
