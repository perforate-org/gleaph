# Explorer web entry (ADR 0076 S4a)

The graph explorer's minimal wasm32 web entry: a main-thread GPUI application
(`gpui_web`) plus the **application-owned worker bootstrap** that ADR 0076 S2's
transport contract anticipated. gpui-graph ships no worker bundle — the main
thread connects through the library's
`gpui_graph::worker::web_transport::PostMessageChannel` (spawn, readiness
replay, envelope routing, frame delivery), and the worker side is one
`web_transport::serve(backend, codec)` call over this directory's backend
configuration. This was the first browser execution of the S1–S3 seam; its
hand-rolled channel plumbing was deleted when the library extracted the
generic part.

## Layout

| Path | Role |
| --- | --- |
| `common/` | Application-owned data both wasm modules share: the deterministic demo fixture (`?nodes=N`, default 1500) and the merge-batch `PayloadCodec`. Plain Rust, unit-tested natively. |
| `app/` | Main-thread GPUI app: boots `gpui_web`, injects the fixture into the worker through a `PostMessageChannel` handle under the application codec, selects `FrameSource::Worker`, renders delivered frames with a status overlay. |
| `worker/` | Worker-side wasm module: backend configuration (ForceAtlas2 + node labels) handed to `web_transport::serve`, plus the paint-frame timing core (`paint_timing.rs`, unit-tested natively). The whole browser conversation is replayed natively in its test. |
| `assets/worker.js` | Application-owned classic-worker script for the real backend: `importScripts` the `--target no-modules` glue, boots the module, posts `"ready"`. |
| `assets/bench_worker.js` | Same contract for the timing harness. |
| `assets/index.html` | App page. |
| `assets/harness.html` | Timing harness page; runs 500/2000/5000-node builds in dedicated workers and tabulates mean/p50/min/max plus wire size. |
| `build.sh` | Builds both wasm modules into `dist/` (app glue `--target web`; worker glue `--target no-modules`, see below). |

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
[explorer-worker] wasm initialized, starting backend
[explorer-web] scene queued for injection: N nodes / ~2N edges
[explorer-web] frame #1 delivered: … nodes / … edges / … labels (X ms round trip)
```

The status overlay repeats the same facts in-page; the graph animates because
the replica steps ForceAtlas2 once per requested frame and ships each result as
a transferable `PaintFrameWire`.

**Why classic workers:** `PostMessageChannel` spawns with plain
`new Worker(url)` — a classic dedicated worker — where ES module syntax cannot
parse. The worker scripts therefore load wasm-bindgen glue emitted with
`--target no-modules` via `importScripts` and keep the same `"ready"`
handshake and transferable-buffer protocol as before.

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
