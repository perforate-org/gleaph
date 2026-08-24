# 0076. Graph explorer web backend: dedicated worker with wasm threads

Date: 2026-08-24
Status: proposed
Last revised: 2026-08-24

## Context

The graph explorer compiles to wasm32 via `gpui_web` (workspace pin, root `Cargo.toml`) and is
the primary demo surface ([knowledge-graph-demo](../demo/knowledge-graph-demo.md), planned). On
web, everything the explorer does — scene ownership, ForceAtlas2 stepping, paint-frame
construction, and GPUI rendering — runs on the browser main thread: `gpui_web` is a
main-thread platform (rAF dispatcher, canvas output; no worker usage at the pinned revision).

Measured native baseline at 5,000 nodes / ~7,400 edges (10-core, 2026-08-24):

- Paint frame build: 9.9 ms after parallelizing the edge-map, cull, and density phases
  (commits `c68b8e4a4`, `a331bda71`); the serial remainder is ~0.1 ms.
- FA2 step (8 iterations): 8.2 ms; per-iterate breakdown: repulsion 0.90 ms (82%),
  attraction 0.06 ms, movement 0.09 ms, gravity ~0.01 ms. Repulsion is a branchy
  Barnes-Hut descent / cutoff-grid pair kernel — pointer-chasing, SIMD-hostile.
- Both engines already run their hot phases on rayon workers behind
  `RAYON_AVAILABLE` / `PAR_MIN_NODES` (4096) / `PAR_MIN_EDGES` (1024) gates. On
  wasm-family targets rayon is not linked and every phase takes the serial driver
  (`crates/gpui-graph/src/layout/force_atlas2.rs`, `crates/gpui-graph/src/paint.rs`).
- Frame wire size: ~12,300 primitives at 2.5k nodes (~0.5–1 MB linear), i.e. sub-millisecond
  as a transferable buffer copy.

On wasm the same work is serial and carries an unvalidated scalar penalty (commonly 1.2–3×
for numeric code), putting a 5k-node frame at roughly 60–120 ms of main-thread time.

## Problem

At around 1.5–2k nodes and above on web:

1. Frame work exceeds the 16.7 ms (60 Hz) budget serially; at 5k the animation drops to
   roughly 8–15 fps even before input costs.
2. Main-thread saturation freezes input handling and compositing — panning and zooming feel
   broken even when the user would accept a lower graph frame rate.
3. No core parallelism is available at all, so the rayon work already written for native is
   dead weight on web.

Demo-scale graphs (tens of nodes) are unaffected; this ADR is scoped to the scaling regime.

## Existing architecture assessment

- **`PaintFrame` is already the render-side contract.** The geometry pipeline produces it
  from (scene, viewport, style, selection, hover); the GPUI element consumes it. The worker
  boundary needs no new representation — the same frame, linearized, is the wire format.
- **The parallel execution model already exists.** FA2 phases and all three paint phases are
  rayon-ready behind activation gates; wasm merely compiles them out. Enabling threads on web
  is a build/infrastructure change, not a rewrite.
- **What the current architecture cannot express:** off-main-thread execution. The element
  prepaint builds the frame synchronously from an in-process scene read
  (`GraphViewState::prepare_canvas` → `build_indexed_paint_frame`), the test harness draws
  through that synchronous path, and `hit_test` runs per mouse-move against the synced scene.
  No existing concept owns "backend elsewhere", so one seam is required; everything else
  extends existing concepts (`PaintFrame` as wire, existing rayon phases, existing
  `Viewport`/style inputs).

## Alternatives

1. **Minimum change — status quo plus LOD tuning.** Straight-LOD and interaction thresholds
   already exist. Insufficient: serial main-thread work still grows with graph size and input
   still freezes; measured serial costs put 5k far past budget.
2. **wasm-threads only (no worker).** Unlocks the existing rayon phases on the main thread.
   Still saturates the main thread (input freezes remain), and the SAB/COOP-COEP
   infrastructure cost is paid anyway. A cheap half-step, not an endpoint.
3. **Worker only (no threads).** Fixes input responsiveness and compositor smoothness; compute
   stays single-core serial, so the graph itself updates at low fps at 5k. Correct isolation,
   incomplete speed.
4. **GPU compute (wgpu FA2 + renderer).** Largest rewrite, new execution domain end to end,
   and GPUI's renderer is not wgpu-based on web at the pinned revision. Deferred.

## Decision

Adopt alternatives 3 + 2 together, sequenced:

1. **`FrameSource` seam.** `GraphViewState` gains a frame source with two modes:
   `InProcess` (today's synchronous build; default; all existing tests keep passing) and
   `Worker` (postMessage protocol). The paint element consumes `PaintFrame` from whichever
   source is active.
2. **The worker owns the graph backend:** scene entity, layout stepping, runtime sync, and
   paint geometry build (viewport-dependent). The main thread owns GPUI rendering, label
   measurement + collision/masking (window text system dependent), input handling, and hit
   testing against a position snapshot shipped with the last frame (same math, stale by at
   most one frame).
3. **Wire format:** `PaintFrame` linearized to SoA typed arrays in transferable
   `ArrayBuffer`s (sub-millisecond at measured sizes; structured cloning of primitive objects
   is explicitly out). Backpressure: the worker keeps only the latest request, dropping
   intermediate viewports.
4. **Interaction:** pan/zoom applies an affine transform to the last delivered frame on the
   main thread immediately; the worker rebuilds asynchronously (map-style
   transform-while-recompute). Graph-scale interaction latency becomes independent of frame
   build cost.
5. **wasm-threads inside the worker** (`wasm-bindgen-rayon`, atomics build) to turn on the
   existing rayon phases. SharedArrayBuffer requires cross-origin isolation: the asset
   canister serves `Cross-Origin-Opener-Policy: same-origin` and
   `Cross-Origin-Embedder-Policy: require-corp` (or `credentialless`) via
   `.ic-assets.json5` custom headers. A CORP audit of every cross-origin subresource
   (Internet Identity iframes, fonts, any CDN assets) is a hard prerequisite — COEP blocks
   anything that does not opt in.
6. **Determinism is preserved by construction:** every parallel phase already merges in
   candidate order, so a frame is byte-identical regardless of worker pool size; the worker
   protocol adds no reordering (one request in flight, latest-wins).

## Consequences

- Web input and compositing stay at 60 Hz independent of graph size; graph frame rate becomes
  bounded by worker compute instead of main-thread contention.
- The existing rayon phases (layout repulsion/attraction/movement, paint edge-map/cull/
  density) run multi-core on web with no algorithm rewrite.
- Native behavior is untouched: `InProcess` remains the default and the only mode in tests.
- The wire contract makes the frame build cost visible and benchmarkable in isolation.

## Trade-offs

- One new seam (`FrameSource`) and one new execution domain (the worker). Justified by the
  demonstrated main-thread saturation; both extend existing representations rather than
  introducing new ones.
- Hit testing against a snapshot can be one frame stale during fast animation; acceptable for
  hover/drag targeting, and identical in kind to the interaction transform's staleness.
- Drag editing (position mutation) gains a round trip to the worker; drags apply locally to
  the snapshot for immediate feedback and reconcile asynchronously.
- Cross-origin isolation is all-or-nothing: enabling COEP constrains every future
  cross-origin embed on the demo host. `credentialless` eases third-party resources but must
  be validated against the browsers in use.
- gpui_web is young; worker-mode integration bugs are expected in slice S2.

## Migration

Pre-production, no deployed compatibility path:

- S1: `FrameSource` abstraction with `InProcess` only; wire (de)serialization round-trip
  property-tested; zero behavior change.
- S2: worker host + message protocol + latest-wins backpressure behind an opt-in flag;
  `InProcess` stays default.
- S3: interaction transform + async refit; hit-test snapshot.
- S4: wasm-threads build (atomics target features) + `.ic-assets.json5` headers + CORP audit;
  measure the wasm scalar penalty and thread scaling at this point, replacing the 1.2–3×
  estimate.

Each slice lands with `paint_bench` / `layout_bench` green and, from S2 on, a wasm timing
harness (wasm-pack build + browser trace) so web numbers stop being extrapolations.

## Design Documentation Impact

- `crates/gpui-graph/DESIGN.md` §18.2 / §27: document the `FrameSource` modes and the wire
  format when S1/S2 land (not before — planned behavior stays in this ADR).
- [knowledge-graph-demo](../demo/knowledge-graph-demo.md): reference this ADR for the demo
  host's serving requirements (headers) once accepted.
- `crates/gpui-graph/DESIGN.md` execution-policy notes: wasm rayon activation flips from
  "never" to "worker-gated" in S4.

## Measurements

Evidence base for this ADR (all 2026-08-24, 10-core native unless noted):

| Phase | Cost @5k | Parallel today (native) |
| --- | --- | --- |
| Paint edge map | 18.5 ms → ~2 ms | yes (`PAR_MIN_EDGES`) |
| Paint cull (zoomed) | 0.8 ms serial | yes |
| Paint density (zoomed) | 9.1 ms → ~1 ms | yes |
| FA2 repulsion | 0.90 ms/iterate (82%) | yes (`PAR_MIN_NODES`) |
| FA2 attraction/movement/gravity | ~0.16 ms/iterate | yes |

Rejected during audit: lowering `PAR_MIN_NODES` (documented measurement: ≤2500 nodes lose
10× to wake/join), SoA/SIMD on repulsion (branchy kernel, 1.3–1.5× ceiling, float-order
determinism conflict with the "scheduling differs only — physics does not" contract).
