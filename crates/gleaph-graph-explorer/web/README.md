# Explorer web entry (ADR 0076 S4a)

The graph explorer's minimal wasm32 web entry: a main-thread GPUI application
(`gpui_web`) plus the **application-owned worker bootstrap** that ADR 0076 S2's
transport contract anticipated. gpui-graph ships no worker bundle —
`gpui_graph::worker::web_transport` documents that an application spawns its own
Worker script and drives a concrete `WorkerBackend` inside
`DedicatedWorkerGlobalScope`. This directory is that contract's first working
demonstration, and the first browser execution of the S1–S3 seam.

## Layout

| Path | Role |
| --- | --- |
| `app/` | Main-thread GPUI app: boots `gpui_web`, generates a deterministic demo graph (`?nodes=N`, default 1500), injects it into the worker, connects `connect_worker_channel`, selects `FrameSource::Worker`, renders delivered frames with a status overlay. |
| `worker/` | wasm module imported by the worker scripts: drives `WorkerBackend` per message (library requests verbatim + application batch codec) and hosts the paint-frame timing core. The timing core and codec are plain Rust, unit-tested natively. |
| `assets/worker.js` | Application-owned Worker script for the real backend (module worker). |
| `assets/bench_worker.js` | Application-owned Worker script for the timing harness. |
| `assets/index.html` | App page. |
| `assets/harness.html` | Timing harness page; runs 500/2000/5000-node builds in dedicated workers and tabulates mean/p50/min/max plus wire size. |
| `build.sh` | Builds both wasm modules (`cargo build --target wasm32-unknown-unknown --release` + `wasm-bindgen --target web`) into `dist/`. |

## Build & run

```sh
cd crates/gleaph-graph-explorer/web
./build.sh
python3 -m http.server 8080 --directory dist   # any static server works
```

- App: <http://127.0.0.1:8080/> (`?nodes=2000` sizes the demo graph,
  `&source=inprocess` runs the same page through the synchronous frame source
  for comparison).
- Harness: <http://127.0.0.1:8080/harness.html>.

**No COOP/COEP/CORP headers are required or used** — this slice is deliberately
the no-threads half of S4; threads/atomics/SharedArrayBuffer are S4b.

## What to look for

Browser console (app page):

```
[explorer-worker] booted in DedicatedWorkerGlobalScope
[explorer-web] scene injected: N nodes / ~2N edges
[explorer-web] frame #1 delivered: … nodes / … edges / … labels (X ms round trip)
```

The status overlay repeats the same facts in-page; the graph animates because
the replica steps ForceAtlas2 once per requested frame and ships each result as
a transferable `PaintFrameWire`.

## Scalar penalty measurement

`harness.html` measures wasm (`?nodes=500,2000,5000&iterations=60` to override
the defaults); the native baseline runs the same `measure_paint_build` code
path on the host (see `worker/src/paint_timing.rs`) with a one-thread rayon
pool as the serial baseline:

```sh
RAYON_NUM_THREADS=1 cargo run --release   # any driver calling
                                          # paint_timing::measure_paint_build
```

Measured 2026-08-24 (release + `simd128`, 60 iterations, 10-core host):
wasm p50 2.2 / 9.2 / 26.1 ms at 500 / 2000 / 5000 nodes versus native serial
p50 2.4 / 8.2 / 23.9 ms — a scalar penalty of ~1.1×, replacing the ADR 0076
"1.2–3×" estimate (native parallel p50 at 5k: 9.0 ms). Recorded in
`crates/gpui-graph/DESIGN.md` §18.2.

## Notes

- This sub-workspace is standalone (like `demo/social/wasm`) so root workspace
  members and lockfile stay untouched by web-only work. The pinned Zed revision
  and wasm-bindgen family versions are restated to unify with the root lockfile.
- Drag position edits are not yet mirrored into the worker (ADR 0076 trade-off,
  later slice); scene injection is one-shot at startup.
